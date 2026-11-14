//! Bulk expire and delete by pattern (spec "Bulk actions"): the dry run, and the runner
//! that sends a [`Permit`]'s bulk op in batches. Used by `x` and `D` on Patterns (one
//! step per [`Request`], driven by the app, so `esc` stops between batches) and by
//! `redis-lens expire` (which calls [`count`] and [`batch`] in a loop).
//!
//! - **The dry run** ([`count`]) is a read: `SCAN MATCH <glob>` keeping the keys that
//!   infer to the pattern (the glob also matches other patterns' keys), and `PTTL` on
//!   them, pipelined, to count those that already have a TTL.
//! - **The run** ([`batch`]) takes nothing but a permit for a [`WriteOp::Bulk`]. It walks
//!   the keyspace again with the same `SCAN MATCH`, so keys created or deleted since
//!   the dry run are seen as they are now. Each batch holds at most [`BATCH`] keys of
//!   the pattern. Before a batch is sent, its key names are appended to the key file
//!   ([`KeyFile`]) and flushed to disk. An expire sends `PTTL` first and writes and
//!   sends `EXPIRE key seconds NX` only for the keys with no TTL; a delete writes and
//!   sends `UNLINK` for every key.
//!
//! Every command takes from the session's [`Budget`]; per-key commands are pipelined
//! [`BATCH`] at a time. `KEYS` is never sent.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use redis::Value;
use redis::aio::ConnectionLike;

use super::{LiveConn, LiveError, error_for};
use crate::analysis::{key_in_pattern, match_glob};
use crate::budget::Budget;
use crate::model::{ServerFlavor, ServerInfo, ServerMode};
use crate::write::{BulkAction, Permit, WriteOp};

/// Keys per batch, and per pipeline.
pub const BATCH: usize = 500;
/// The `COUNT` hint of each `SCAN`.
pub const SCAN_COUNT: u64 = 1_000;
/// At most this many `SCAN` calls per step, so a pattern with few keys in a big
/// keyspace never holds a step for long: `esc` is answered between steps.
pub const MAX_SCAN_CALLS: usize = 100;
/// The TTL `x` offers first: 24 h.
pub const DEFAULT_TTL_SECS: u64 = 86_400;
/// The longest TTL accepted (about 136 years).
pub const MAX_TTL_SECS: u64 = u32::MAX as u64;

/// Why bulk actions are off on a cluster.
pub const CLUSTER_OFF: &str = "Not available on a cluster: bulk actions walk one server's \
     keys, and a cluster spreads its keys over its nodes.";
/// Why bulk actions are off when the session reads a replica.
pub const REPLICA_OFF: &str = "Not available on a replica: it takes no writes. Connect to \
     the primary (turn off prefer_replica) to change keys.";

/// Where a `SCAN` walk is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    /// Not started: the first `SCAN` sends cursor 0.
    Start,
    /// Goes on from this cursor.
    At(u64),
    /// The cursor came back to 0.
    Done,
}

/// One step of a bulk action, for the live connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// One step of the dry run ([`count`]).
    Count {
        pattern: String,
        glob: String,
        walk: Walk,
    },
    /// One batch of the run ([`batch`]).
    Batch {
        permit: Permit,
        walk: Walk,
        /// Keys of the pattern found by the previous batch but not sent yet.
        carry: Vec<String>,
        file: FileAt,
    },
}

/// The key file a batch appends to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAt {
    /// Not created yet: the first batch with keys creates it under [`default_dir`],
    /// in `profile`'s directory, named by the time it is created.
    New { profile: String },
    /// Created by an earlier batch of the same run.
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Count(Count),
    Batch(Batch),
}

/// What one dry-run step found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    pub walk: Option<Walk>,
    /// Keys that infer to the pattern and exist.
    pub matched: u64,
    /// Of those, keys that have a TTL already.
    pub with_ttl: u64,
    /// Keys the glob matched that infer to another pattern, left out.
    pub other_pattern: u64,
    /// Keys whose name is not UTF-8 (they can't be inferred) or whose `PTTL` was
    /// refused (a key ACL), left out.
    pub unreadable: u64,
}

impl Count {
    /// Adds a step's counts to these.
    pub fn add(&mut self, other: &Count) {
        self.walk = other.walk;
        self.matched += other.matched;
        self.with_ttl += other.with_ttl;
        self.other_pattern += other.other_pattern;
        self.unreadable += other.unreadable;
    }

    /// The walk has reached the end.
    pub fn finished(&self) -> bool {
        self.walk == Some(Walk::Done)
    }

    /// How many keys `action` would change: every key for a delete, the keys without
    /// a TTL for an expire.
    pub fn to_change(&self, action: &BulkAction) -> u64 {
        match action {
            BulkAction::Delete => self.matched,
            BulkAction::Expire { .. } => self.matched.saturating_sub(self.with_ttl),
        }
    }
}

/// What one batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub walk: Walk,
    /// Keys of the pattern found but not sent yet: the next batch sends them first.
    pub carry: Vec<String>,
    /// Keys of the pattern this batch handled, whatever became of them.
    pub seen: u64,
    /// Keys whose names went to the key file and to the server.
    pub sent: u64,
    /// Keys the server changed: `EXPIRE` or `UNLINK` answered 1.
    pub changed: u64,
    /// Keys left alone: a TTL already (expire), or gone meanwhile.
    pub skipped: u64,
    /// Keys the server answered with an error.
    pub failed: u64,
    /// Some of those errors were `NOPERM`: the server refused the command for this
    /// user (or for those keys).
    pub denied: bool,
    /// Keys that could not be read (a name that is not UTF-8, or `PTTL` refused).
    pub unreadable: u64,
    /// The key file, once a batch has created it.
    pub file: Option<PathBuf>,
}

impl Batch {
    /// No more batches: the walk is over and nothing is carried.
    pub fn finished(&self) -> bool {
        self.walk == Walk::Done && self.carry.is_empty()
    }
}

/// Runs one step. Every command goes through `budget`.
pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    match req {
        Request::Count {
            pattern,
            glob,
            walk,
        } => count(conn, budget, &pattern, &glob, walk)
            .await
            .map(Reply::Count),
        Request::Batch {
            permit,
            walk,
            carry,
            file,
        } => {
            let mut file = match file {
                FileAt::Path(p) => KeyFile::at(p),
                FileAt::New { profile } => {
                    let WriteOp::Bulk { action, .. } = permit.op() else {
                        return Err(not_bulk());
                    };
                    let dir = default_dir().ok_or_else(|| {
                        LiveError::Failed(
                            "Not sent: no data directory for the key file was found".into(),
                        )
                    })?;
                    KeyFile::new(&dir, &profile, action, now_unix())
                }
            };
            batch(conn, budget, &permit, walk, carry, &mut file)
                .await
                .map(Reply::Batch)
        }
    }
}

fn not_bulk() -> LiveError {
    LiveError::Failed("Not sent: only a bulk action runs in batches".to_string())
}

/// Seconds since the epoch, now.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `<data dir>/redis-lens/bulk`, next to the snapshots: for example
/// `~/Library/Application Support/redis-lens/bulk` on macOS.
pub fn default_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "redis-lens").map(|d| d.data_dir().join("bulk"))
}

/// Keys of `pattern` among `keys` (a `SCAN` reply), new to `seen`, appended to `found`.
/// Returns how many inferred to another pattern, and how many names were not UTF-8.
fn sort_keys(
    keys: Vec<Vec<u8>>,
    pattern: &str,
    seen: &mut HashSet<String>,
    found: &mut Vec<String>,
) -> (u64, u64) {
    let (mut other, mut unreadable) = (0, 0);
    for raw in keys {
        match String::from_utf8(raw) {
            Ok(k) if !key_in_pattern(&k, pattern) => other += 1,
            Ok(k) => {
                if seen.insert(k.clone()) {
                    found.push(k);
                }
            }
            Err(_) => unreadable += 1,
        }
    }
    (other, unreadable)
}

/// `SCAN cursor MATCH glob COUNT n` from `walk` until `found` holds [`BATCH`] keys of
/// `pattern`, the walk ends, or after [`MAX_SCAN_CALLS`] calls.
async fn walk_keys<C: ConnectionLike + Send>(
    conn: &mut C,
    budget: &Budget,
    pattern: &str,
    glob: &str,
    mut walk: Walk,
    found: &mut Vec<String>,
) -> Result<(Walk, u64, u64), LiveError> {
    let mut seen: HashSet<String> = found.iter().cloned().collect();
    let (mut other, mut unreadable) = (0, 0);
    let mut calls = 0;
    while found.len() < BATCH && calls < MAX_SCAN_CALLS {
        let cursor = match walk {
            Walk::Start => 0,
            Walk::At(c) => c,
            Walk::Done => break,
        };
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor)
            .arg("MATCH")
            .arg(glob)
            .arg("COUNT")
            .arg(SCAN_COUNT);
        budget.take(1).await;
        let (next, keys): (u64, Vec<Vec<u8>>) = cmd
            .query_async(conn)
            .await
            .map_err(|e| error_for("SCAN", &e))?;
        calls += 1;
        walk = if next == 0 {
            Walk::Done
        } else {
            Walk::At(next)
        };
        let (o, u) = sort_keys(keys, pattern, &mut seen, found);
        other += o;
        unreadable += u;
    }
    Ok((walk, other, unreadable))
}

/// One reply of a pipeline sent with `ignore_errors`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Int(i64),
    Denied,
    Error,
}

fn answer(v: &Value) -> Answer {
    match v {
        Value::Int(n) => Answer::Int(*n),
        Value::ServerError(e) => {
            if redis::RedisError::from(e.clone()).kind()
                == redis::ErrorKind::Server(redis::ServerErrorKind::NoPerm)
            {
                Answer::Denied
            } else {
                Answer::Error
            }
        }
        _ => Answer::Error,
    }
}

/// Sends `command key [extra…]` for each key, in one pipeline of at most [`BATCH`],
/// taking its length from `budget`. One answer per key.
async fn per_key<C: ConnectionLike + Send>(
    conn: &mut C,
    budget: &Budget,
    command: &str,
    keys: &[String],
    extra: &[String],
) -> Result<Vec<Answer>, LiveError> {
    let mut out = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(BATCH) {
        let mut pipe = redis::pipe();
        pipe.ignore_errors();
        for k in chunk {
            pipe.cmd(command).arg(k).arg(extra);
        }
        budget
            .take(u32::try_from(chunk.len()).unwrap_or(u32::MAX))
            .await;
        let replies: Vec<Value> = pipe
            .query_async(conn)
            .await
            .map_err(|e| error_for(command, &e))?;
        if replies.len() != chunk.len() {
            return Err(LiveError::Failed(format!(
                "the server's answer to {command} could not be read"
            )));
        }
        out.extend(replies.iter().map(answer));
    }
    Ok(out)
}

/// The keys found, with their `PTTL`: `-2` gone, `-1` no TTL.
async fn pttls<C: ConnectionLike + Send>(
    conn: &mut C,
    budget: &Budget,
    keys: &[String],
) -> Result<Vec<Answer>, LiveError> {
    per_key(conn, budget, "PTTL", keys, &[]).await
}

/// One step of the dry run: the keys of `pattern` found by `SCAN MATCH glob` from
/// `walk` (see [`MAX_SCAN_CALLS`]), and how many have a TTL. Reads only.
pub async fn count<C: ConnectionLike + Send>(
    conn: &mut C,
    budget: &Budget,
    pattern: &str,
    glob: &str,
    walk: Walk,
) -> Result<Count, LiveError> {
    let mut found = Vec::new();
    let (walk, other, mut unreadable) =
        walk_keys(conn, budget, pattern, glob, walk, &mut found).await?;
    let (mut matched, mut with_ttl) = (0, 0);
    for a in pttls(conn, budget, &found).await? {
        match a {
            Answer::Int(-2) => {}
            Answer::Int(-1) => matched += 1,
            Answer::Int(_) => {
                matched += 1;
                with_ttl += 1;
            }
            Answer::Denied | Answer::Error => unreadable += 1,
        }
    }
    Ok(Count {
        walk: Some(walk),
        matched,
        with_ttl,
        other_pattern: other,
        unreadable,
    })
}

/// The whole dry run: [`count`] until the walk ends, calling `progress` with the
/// counts so far after each step.
pub async fn dry_run<C: ConnectionLike + Send>(
    conn: &mut C,
    budget: &Budget,
    pattern: &str,
    glob: &str,
    mut progress: impl FnMut(&Count),
) -> Result<Count, LiveError> {
    let mut total = Count::default();
    let mut walk = Walk::Start;
    loop {
        let step = count(conn, budget, pattern, glob, walk).await?;
        total.add(&step);
        progress(&total);
        match step.walk {
            Some(Walk::Done) | None => return Ok(total),
            Some(w) => walk = w,
        }
    }
}

/// One batch of `permit`'s bulk op: at most [`BATCH`] keys of the pattern (`carry`
/// first, then `SCAN MATCH` from `walk`), their names appended to `file` and flushed,
/// then the writes. Refuses any op that is not [`WriteOp::Bulk`], and a glob that is not
/// the pattern's.
pub async fn batch<C: ConnectionLike + Send>(
    conn: &mut C,
    budget: &Budget,
    permit: &Permit,
    walk: Walk,
    carry: Vec<String>,
    file: &mut KeyFile,
) -> Result<Batch, LiveError> {
    let WriteOp::Bulk {
        pattern,
        glob,
        action,
    } = permit.op()
    else {
        return Err(not_bulk());
    };
    if *glob != match_glob(pattern) {
        return Err(LiveError::Failed(
            "Not sent: the glob is not the pattern's".to_string(),
        ));
    }
    let mut found = carry;
    let (walk, _, mut unreadable) =
        walk_keys(conn, budget, pattern, glob, walk, &mut found).await?;
    let carry = found.split_off(found.len().min(BATCH));
    let mut out = Batch {
        walk,
        carry,
        seen: found.len() as u64,
        sent: 0,
        changed: 0,
        skipped: 0,
        failed: 0,
        denied: false,
        unreadable: 0,
        file: None,
    };
    let (send, command, extra) = match action {
        BulkAction::Delete => (found, "UNLINK", Vec::new()),
        BulkAction::Expire { seconds } => {
            let ttls = pttls(conn, budget, &found).await?;
            let mut send = Vec::new();
            for (k, a) in found.into_iter().zip(ttls) {
                match a {
                    Answer::Int(-1) => send.push(k),
                    Answer::Int(_) => out.skipped += 1,
                    Answer::Denied | Answer::Error => unreadable += 1,
                }
            }
            (send, "EXPIRE", vec![seconds.to_string(), "NX".to_string()])
        }
    };
    out.unreadable = unreadable;
    if send.is_empty() {
        return Ok(out);
    }
    file.append(&send).map_err(|e| {
        LiveError::Failed(format!(
            "Not sent: the key file {} could not be written ({})",
            file.path().display(),
            e.kind()
        ))
    })?;
    out.file = Some(file.path().to_path_buf());
    out.sent = send.len() as u64;
    for a in per_key(conn, budget, command, &send, &extra).await? {
        match a {
            Answer::Int(1) => out.changed += 1,
            Answer::Int(_) => out.skipped += 1,
            Answer::Denied => {
                out.failed += 1;
                out.denied = true;
            }
            Answer::Error => out.failed += 1,
        }
    }
    Ok(out)
}

/// The key file of a bulk run (spec "Bulk actions"):
/// `<dir>/<profile dir>/<YYYYMMDDTHHMMSSZ>-<expire|delete>.txt`, one escaped key name per
/// line ([`escape_name`]) and nothing else. It is created by the first batch with keys
/// (directories mode 0700, the file 0600, never over an existing file) and each batch's
/// names are appended and flushed to disk before the batch is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFile {
    path: PathBuf,
    created: bool,
}

impl KeyFile {
    /// The key file of a run of `action` for `profile` started at `unix`, under `dir`
    /// (the bulk directory). Nothing is created yet.
    pub fn new(dir: &Path, profile: &str, action: &BulkAction, unix: i64) -> Self {
        let what = match action {
            BulkAction::Expire { .. } => "expire",
            BulkAction::Delete => "delete",
        };
        let name = format!("{}-{what}.txt", crate::snapshot::timestamp_name(unix));
        Self {
            path: crate::snapshot::profile_dir_path(dir, profile).join(name),
            created: false,
        }
    }

    /// The key file an earlier batch created at `path`.
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            created: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a batch has created it.
    pub fn created(&self) -> bool {
        self.created
    }

    /// Appends `keys`, one escaped name per line, and flushes the file to disk. The
    /// first call creates it; a file of that name made meanwhile is never written to:
    /// the name gets `-2`, `-3` … instead.
    pub fn append(&mut self, keys: &[String]) -> std::io::Result<()> {
        let mut f = if self.created {
            std::fs::OpenOptions::new().append(true).open(&self.path)?
        } else {
            let f = self.create()?;
            self.created = true;
            f
        };
        let mut text = String::new();
        for k in keys {
            text.push_str(&escape_name(k));
            text.push('\n');
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()
    }

    fn create(&mut self) -> std::io::Result<std::fs::File> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("the key file has no directory"))?
            .to_path_buf();
        create_dirs(&dir)?;
        let stem = self
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for n in 1..=100 {
            let path = if n == 1 {
                self.path.clone()
            } else {
                dir.join(format!("{stem}-{n}.txt"))
            };
            match create_new(&path) {
                Ok(f) => {
                    self.path = path;
                    return Ok(f);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("too many key files with this name"))
    }
}

#[cfg(unix)]
fn create_dirs(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dirs(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn create_new(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_new(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// A key name as one line of the key file: `\` becomes `\\`, newline `\n`, carriage
/// return `\r`, tab `\t`, and every other control character, and the Unicode line and
/// paragraph separators, `\u{XXXX}` (hex). A name can't make a line break, so it can't
/// forge another line. [`unescape_name`] reads it back.
pub fn escape_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{{{:04X}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// The name an [`escape_name`] line stands for. `None` when the line is not one.
pub fn unescape_name(line: &str) -> Option<String> {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let hex: String = chars.by_ref().take_while(|c| *c != '}').collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// `90s`, `10m`, `24h`, `7d`, or several such as `1h30m`; a bare number is seconds.
/// At least 1 s and at most [`MAX_TTL_SECS`].
pub fn parse_ttl(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let bad = || {
        format!(
            "not a duration: use 90s, 10m, 24h or 7d (got \"{}\")",
            crate::value::escape(t)
        )
    };
    if t.is_empty() {
        return Err("no TTL given: use 90s, 10m, 24h or 7d".to_string());
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        let n: u64 = t.parse().map_err(|_| bad())?;
        return check_ttl(n);
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    for c in t.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c.to_ascii_lowercase() {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            _ => return Err(bad()),
        };
        if num.is_empty() {
            return Err(bad());
        }
        let n: u64 = num.parse().map_err(|_| bad())?;
        num.clear();
        total = n
            .checked_mul(unit)
            .and_then(|s| total.checked_add(s))
            .ok_or_else(|| "the TTL is too long".to_string())?;
    }
    if !num.is_empty() {
        return Err(bad());
    }
    check_ttl(total)
}

fn check_ttl(secs: u64) -> Result<u64, String> {
    match secs {
        0 => Err("the TTL must be at least 1s".to_string()),
        s if s > MAX_TTL_SECS => Err("the TTL is too long".to_string()),
        s => Ok(s),
    }
}

/// A TTL in the largest whole unit: `90s`, `10m`, `24h`, `7d`.
pub fn ttl_text(secs: u64) -> String {
    if secs >= 2 * 86_400 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 && secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// `7.4.2` → (7, 4); `8` → (8, 0).
fn major_minor(version: &str) -> Option<(u64, u64)> {
    let mut nums = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().ok());
    let major = nums.next()??;
    let minor = nums.next().flatten().unwrap_or(0);
    Some((major, minor))
}

/// Why `x` (bulk expire) is off on `server`: `EXPIRE … NX` needs Redis 7.0 or later
/// (every Valkey has it). `None` when it is on.
pub fn expire_off(server: &ServerInfo) -> Option<String> {
    let new_enough = match server.flavor {
        ServerFlavor::Valkey => true,
        ServerFlavor::Redis | ServerFlavor::Unknown => {
            major_minor(&server.version).is_some_and(|v| v >= (7, 0))
        }
    };
    (!new_enough).then(|| {
        format!(
            "Not available on Redis {}: a bulk expire sends EXPIRE … NX, which needs Redis 7.0 \
             or later.",
            crate::value::escape(&server.version)
        )
    })
}

/// Why bulk actions are off on `server` whatever the action: a cluster.
pub fn server_off(server: &ServerInfo) -> Option<&'static str> {
    (server.mode == ServerMode::Cluster).then_some(CLUSTER_OFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn temp_dir(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d =
            std::env::temp_dir().join(format!("redis-lens-bulk-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn only_keys_of_the_pattern_are_kept() {
        // `user:*:cart` also matches keys that infer to other patterns.
        let keys: Vec<Vec<u8>> = vec![
            b"user:1001:cart".to_vec(),
            b"user:1002:cart".to_vec(),
            b"user:1001:cart".to_vec(),
            b"user:admin:cart".to_vec(),
            b"user:1003:cart:old".to_vec(),
            vec![b'u', 0xff, b':', b'1'],
        ];
        let mut seen = HashSet::new();
        let mut found = Vec::new();
        let (other, unreadable) = sort_keys(keys, "user:{id}:cart", &mut seen, &mut found);
        assert_eq!(found, vec!["user:1001:cart", "user:1002:cart"]);
        assert_eq!((other, unreadable), (2, 1));
    }

    #[test]
    fn counts_add_up_and_say_what_changes() {
        let mut total = Count::default();
        total.add(&Count {
            walk: Some(Walk::At(5)),
            matched: 10,
            with_ttl: 4,
            other_pattern: 1,
            unreadable: 0,
        });
        assert!(!total.finished());
        total.add(&Count {
            walk: Some(Walk::Done),
            matched: 5,
            with_ttl: 1,
            other_pattern: 0,
            unreadable: 2,
        });
        assert!(total.finished());
        assert_eq!(
            (total.matched, total.with_ttl, total.unreadable),
            (15, 5, 2)
        );
        assert_eq!(total.to_change(&BulkAction::Expire { seconds: 1 }), 10);
        assert_eq!(total.to_change(&BulkAction::Delete), 15);
    }

    #[test]
    fn pipeline_answers() {
        // An error reply as the server sends it: `-CODE detail`.
        let error =
            |line: &str| redis::parse_redis_value(format!("-{line}\r\n").as_bytes()).unwrap();
        assert_eq!(answer(&Value::Int(1)), Answer::Int(1));
        assert_eq!(
            answer(&error(
                "NOPERM User lens has no permissions to run the 'expire' command"
            )),
            Answer::Denied
        );
        assert_eq!(
            answer(&error("ERR wrong number of arguments for 'expire' command")),
            Answer::Error
        );
        assert_eq!(answer(&Value::Nil), Answer::Error);
    }

    #[test]
    fn the_key_file_is_named_by_profile_time_and_action() {
        let dir = Path::new("/d");
        let f = KeyFile::new(dir, "Prod Cache", &BulkAction::Delete, 0);
        // Compared by component, so the test holds with Windows' separator too.
        let parts = |p: &Path| {
            p.iter()
                .map(|c| c.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let p = parts(f.path());
        assert_eq!(p.len(), 4, "{p:?}");
        assert!(p[2].starts_with("prod_cache-"), "{p:?}");
        assert_eq!(p[3], "19700101T000000Z-delete.txt");
        let f = KeyFile::new(dir, "p", &BulkAction::Expire { seconds: 9 }, 86_400);
        assert_eq!(
            parts(f.path()).last().unwrap(),
            "19700102T000000Z-expire.txt"
        );
        assert!(!f.created());
    }

    #[test]
    fn the_key_file_holds_escaped_names_appended_per_batch() {
        let dir = temp_dir("file");
        let mut f = KeyFile::new(&dir, "p", &BulkAction::Delete, 1_700_000_000);
        f.append(&["a".into(), "b\nforged".into()]).unwrap();
        // Flushed: another reader sees the first batch before the second is written.
        let first = std::fs::read_to_string(f.path()).unwrap();
        assert_eq!(first, "a\nb\\nforged\n");
        f.append(&["c\\d".into()]).unwrap();
        let text = std::fs::read_to_string(f.path()).unwrap();
        assert_eq!(text, "a\nb\\nforged\nc\\\\d\n");
        let back: Vec<String> = text.lines().map(|l| unescape_name(l).unwrap()).collect();
        assert_eq!(back, vec!["a", "b\nforged", "c\\d"]);
        // A batch appends to the file an earlier one created.
        let mut again = KeyFile::at(f.path().to_path_buf());
        again.append(&["e".into()]).unwrap();
        assert!(
            std::fs::read_to_string(f.path())
                .unwrap()
                .ends_with("c\\\\d\ne\n")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(f.path()), 0o600);
            assert_eq!(mode(f.path().parent().unwrap()), 0o700);
            assert_eq!(mode(&dir), 0o700);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_key_file_is_never_written_to() {
        let dir = temp_dir("exists");
        let mut a = KeyFile::new(&dir, "p", &BulkAction::Delete, 0);
        let mut b = KeyFile::new(&dir, "p", &BulkAction::Delete, 0);
        a.append(&["a".into()]).unwrap();
        b.append(&["b".into()]).unwrap();
        assert_ne!(a.path(), b.path());
        assert!(
            b.path()
                .to_string_lossy()
                .ends_with("19700101T000000Z-delete-2.txt")
        );
        assert_eq!(std::fs::read_to_string(a.path()).unwrap(), "a\n");
        assert_eq!(std::fs::read_to_string(b.path()).unwrap(), "b\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escaping() {
        assert_eq!(escape_name("plain:1"), "plain:1");
        assert_eq!(escape_name("a\r\n\t\\"), "a\\r\\n\\t\\\\");
        assert_eq!(
            escape_name("x\u{1b}[31m\u{2028}\u{85}"),
            "x\\u{001B}[31m\\u{2028}\\u{0085}"
        );
        assert_eq!(unescape_name("\\q"), None);
        assert_eq!(unescape_name("\\"), None);
        assert_eq!(unescape_name("\\u{zz}"), None);
    }

    proptest! {
        #[test]
        fn an_escaped_name_is_one_line_and_reads_back(name in any::<String>()) {
            let line = escape_name(&name);
            prop_assert!(!line.contains('\n') && !line.contains('\r'));
            prop_assert_eq!(line.lines().count().max(1), 1);
            prop_assert_eq!(unescape_name(&line), Some(name));
        }
    }

    #[test]
    fn expire_nx_needs_redis_7() {
        let server = |flavor, version: &str, mode| ServerInfo {
            flavor,
            version: version.into(),
            mode,
            used_memory: 0,
            maxmemory: 0,
            maxmemory_policy: String::new(),
            total_keys: 0,
        };
        let s62 = server(ServerFlavor::Redis, "6.2.14", ServerMode::Standalone);
        assert!(expire_off(&s62).unwrap().contains("Redis 6.2.14"));
        for (f, v) in [
            (ServerFlavor::Redis, "7.0.0"),
            (ServerFlavor::Redis, "7.4.2"),
            (ServerFlavor::Valkey, "7.2.5"),
            (ServerFlavor::Valkey, "8.1.1"),
        ] {
            assert_eq!(
                expire_off(&server(f, v, ServerMode::Standalone)),
                None,
                "{v}"
            );
        }
        assert_eq!(
            server_off(&server(ServerFlavor::Redis, "7.4.2", ServerMode::Cluster)),
            Some(CLUSTER_OFF)
        );
        assert_eq!(server_off(&s62), None);
    }
}
