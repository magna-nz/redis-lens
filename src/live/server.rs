//! Live requests for the server view (spec "3 Server"). Owner: wave B (`Ping` and
//! `Pong`: wave A1). Phase 1: no writes, no values.
//!
//! - [`Request::Info`]: `INFO`, parsed by [`parse_info`].
//! - [`Request::Clients`]: `CLIENT LIST`, parsed by [`parse_client_list`]. The `user=`
//!   field is dropped while parsing: it names the ACL user (CLAUDE.md rule 1).
//! - [`Request::SlowLog`]: `SLOWLOG GET 128`, then `CONFIG GET slowlog-*` where it is
//!   allowed, then `COMMAND INFO` for the command names not seen before. The key names of
//!   an entry come from the command's key specs ([`CommandKeys`]) applied to the logged
//!   arguments. The arguments themselves are dropped inside [`run`]: nothing that leaves
//!   this module holds one (they are P2, on screen only).
//!
//! Why `COMMAND INFO` and not `COMMAND GETKEYS`: it costs one command per new command name
//! (the view keeps the answers, so a steady log costs none), it is in the minimal ACL
//! user of spec "Permission check, additions", and it never sends the logged arguments
//! back to the server. `GETKEYS` would cost one command per entry, and the slow log cuts
//! long argument lists and long arguments, which `GETKEYS` refuses or answers wrongly.

use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

use redis::Value;

use super::{Conn, LiveError, query};
use crate::budget::Budget;

/// How many slow log entries one poll asks for.
pub const SLOWLOG_ENTRIES: u32 = 128;
/// Key names kept per slow log entry; the rest are counted in [`SlowEntry::more_keys`].
pub const MAX_KEYS_PER_ENTRY: usize = 8;
/// The longest text kept from the server for a name (client name, key, library).
const MAX_TEXT: usize = 128;
/// Command names asked about in one `COMMAND INFO`.
const MAX_NAMES_PER_INFO: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// `PING`: checks the live connection end to end.
    Ping,
    /// `INFO`: the Overview poll.
    Info,
    /// `CLIENT LIST`: the Clients poll.
    Clients,
    /// `SLOWLOG GET 128`: the Slow log poll.
    SlowLog {
        /// What the view has learned so far about commands' key specs, by lowercase
        /// name (`get`, `xinfo|stream`).
        specs: HashMap<String, CommandKeys>,
        /// Also read `CONFIG GET slowlog-*`. False once the server refused it.
        config: bool,
        /// Also look up key names. False once `COMMAND INFO` was refused.
        keys: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// The server answered `PING`.
    Pong,
    /// One `INFO`, taken at `at` (when the answer arrived).
    Info {
        at: Instant,
        sample: Box<InfoSample>,
    },
    /// The connections `CLIENT LIST` showed, without their users.
    Clients(Vec<ClientConn>),
    SlowLog(Box<SlowLogReply>),
}

/// Runs one request. Every command goes through `budget`.
pub async fn run(conn: &mut Conn, budget: &Budget, req: Request) -> Result<Reply, LiveError> {
    match req {
        Request::Ping => {
            let _: String = query(conn, budget, "PING", &redis::cmd("PING")).await?;
            Ok(Reply::Pong)
        }
        Request::Info => {
            let v: Value = query(conn, budget, "INFO", &redis::cmd("INFO")).await?;
            let at = Instant::now();
            let sample = Box::new(parse_info(&text_of(&v).unwrap_or_default()));
            Ok(Reply::Info { at, sample })
        }
        Request::Clients => {
            let mut c = redis::cmd("CLIENT");
            c.arg("LIST");
            let v: Value = query(conn, budget, "CLIENT LIST", &c).await?;
            Ok(Reply::Clients(parse_client_list(
                &text_of(&v).unwrap_or_default(),
            )))
        }
        Request::SlowLog {
            specs,
            config,
            keys,
        } => slowlog(conn, budget, specs, config, keys)
            .await
            .map(|r| Reply::SlowLog(Box::new(r))),
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// A text reply (`INFO`, `CLIENT LIST`) as a string; bytes that are not UTF-8 become `�`.
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

/// `s` for the screen: control characters become `·` and the length is capped, so a
/// name from the server can neither move the cursor nor widen a table.
pub fn clean(s: &str) -> String {
    let mut out: String = s
        .chars()
        .take(MAX_TEXT)
        .map(|c| if c.is_control() { '·' } else { c })
        .collect();
    if s.chars().count() > MAX_TEXT {
        out.push('…');
    }
    out
}

fn clean_bytes(b: &[u8]) -> String {
    clean(&String::from_utf8_lossy(b))
}

// ---------------------------------------------------------------------------
// INFO
// ---------------------------------------------------------------------------

/// A server's role as `INFO replication` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Primary,
    Replica,
}

/// The `INFO` fields of the Overview. A field the server does not report (managed
/// services hide some) is `None`, and only that field is off.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InfoSample {
    pub uptime_secs: Option<u64>,
    pub role: Option<Role>,
    /// `connected_slaves` (`connected_replicas` on newer servers).
    pub replicas: Option<u64>,
    /// On a replica: whether `master_link_status` is `up`.
    pub link_up: Option<bool>,
    pub clients: Option<u64>,
    pub blocked: Option<u64>,
    pub used_memory: Option<u64>,
    /// `0` means no limit.
    pub maxmemory: Option<u64>,
    pub fragmentation: Option<f64>,
    pub policy: Option<String>,
    // Counters since the server started.
    pub commands: Option<u64>,
    pub hits: Option<u64>,
    pub misses: Option<u64>,
    pub net_in: Option<u64>,
    pub net_out: Option<u64>,
    pub evicted: Option<u64>,
    pub expired: Option<u64>,
    /// Seconds of CPU since the server started.
    pub cpu_user: Option<f64>,
    pub cpu_sys: Option<f64>,
    /// `rdb_last_bgsave_status` is `ok`.
    pub rdb_ok: Option<bool>,
    pub aof_on: Option<bool>,
    /// `aof_last_write_status` is `ok`.
    pub aof_ok: Option<bool>,
}

/// Parses the text of `INFO`. Never fails: what is missing or unreadable is `None`.
pub fn parse_info(text: &str) -> InfoSample {
    let map: HashMap<&str, &str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once(':'))
        .collect();
    let uint = |k: &str| map.get(k).and_then(|v| v.trim().parse::<u64>().ok());
    let float = |k: &str| {
        map.get(k)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|f| f.is_finite() && *f >= 0.0)
    };
    let word = |k: &str| map.get(k).map(|v| v.trim());
    InfoSample {
        uptime_secs: uint("uptime_in_seconds"),
        role: word("role").and_then(|r| match r {
            "master" | "primary" => Some(Role::Primary),
            "slave" | "replica" => Some(Role::Replica),
            _ => None,
        }),
        replicas: uint("connected_slaves").or_else(|| uint("connected_replicas")),
        link_up: word("master_link_status").map(|s| s == "up"),
        clients: uint("connected_clients"),
        blocked: uint("blocked_clients"),
        used_memory: uint("used_memory"),
        maxmemory: uint("maxmemory"),
        fragmentation: float("mem_fragmentation_ratio"),
        policy: word("maxmemory_policy")
            .filter(|p| !p.is_empty())
            .map(clean),
        commands: uint("total_commands_processed"),
        hits: uint("keyspace_hits"),
        misses: uint("keyspace_misses"),
        net_in: uint("total_net_input_bytes"),
        net_out: uint("total_net_output_bytes"),
        evicted: uint("evicted_keys"),
        expired: uint("expired_keys"),
        cpu_user: float("used_cpu_user"),
        cpu_sys: float("used_cpu_sys"),
        rdb_ok: word("rdb_last_bgsave_status").map(|s| s == "ok"),
        aof_on: uint("aof_enabled").map(|n| n != 0),
        aof_ok: word("aof_last_write_status").map(|s| s == "ok"),
    }
}

// ---------------------------------------------------------------------------
// CLIENT LIST
// ---------------------------------------------------------------------------

/// One line of `CLIENT LIST`. It has no user: `user=` is dropped on parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientConn {
    pub id: u64,
    /// `ip:port`, `[ip]:port`, or a socket path.
    pub addr: String,
    /// `CLIENT SETNAME`; empty for none.
    pub name: String,
    /// `lib-name`, Redis 7.2 and later; empty for none.
    pub lib_name: String,
    pub lib_ver: String,
    /// Seconds since it connected.
    pub age: u64,
    /// Seconds since its last command.
    pub idle: u64,
    /// Its last command, such as `get` or `client|list`; `NULL` before the first.
    pub cmd: String,
    /// Bytes: `tot-mem` where the server has it, else the query and output buffers.
    pub mem: u64,
    pub db: Option<u64>,
}

/// Parses the text of `CLIENT LIST`. Lines without an `id=` are skipped.
pub fn parse_client_list(text: &str) -> Vec<ClientConn> {
    text.lines().filter_map(parse_client_line).collect()
}

fn parse_client_line(line: &str) -> Option<ClientConn> {
    let mut c = ClientConn::default();
    let (mut has_id, mut tot_mem, mut qbuf, mut omem) = (false, None, 0u64, 0u64);
    for field in line.split_whitespace() {
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        let num = || v.parse::<u64>().ok();
        match k {
            "id" => {
                if let Some(n) = num() {
                    c.id = n;
                    has_id = true;
                }
            }
            "addr" => c.addr = clean(v),
            "name" => c.name = clean(v),
            "lib-name" => c.lib_name = clean(v),
            "lib-ver" => c.lib_ver = clean(v),
            "age" => c.age = num().unwrap_or(0),
            "idle" => c.idle = num().unwrap_or(0),
            "cmd" => c.cmd = clean(v),
            "db" => c.db = num(),
            "tot-mem" => tot_mem = num(),
            "qbuf" => qbuf = num().unwrap_or(0),
            "omem" => omem = num().unwrap_or(0),
            // `user` is deliberately not read, nor is anything else.
            _ => {}
        }
    }
    c.mem = tot_mem.unwrap_or(qbuf.saturating_add(omem));
    has_id.then_some(c)
}

// ---------------------------------------------------------------------------
// Key specs
// ---------------------------------------------------------------------------

/// What `COMMAND INFO` says about where a command's keys are.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandKeys {
    /// Zero or more key specs; none means the command takes no keys.
    Specs(Vec<KeySpec>),
    /// A command with subcommands (`XINFO`, `CONFIG`): look up `name|sub` instead.
    Container,
    /// The server does not know the command (a module that is gone, say).
    Unknown,
}

/// One key spec: where the search starts, and how the keys follow from there.
#[derive(Debug, Clone, PartialEq)]
pub struct KeySpec {
    pub begin: Begin,
    pub find: Find,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Begin {
    /// At this argument (0 is the command name).
    Index(i64),
    /// After the first argument equal to `word`, searched from index `from` (a negative
    /// `from` searches backwards from the end).
    Keyword { word: String, from: i64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Find {
    /// From the start to `last` (relative to the start; `-1` is the last argument, and
    /// with `limit` the last of the first `1/limit` of the arguments), every `step`.
    Range { last: i64, step: i64, limit: i64 },
    /// The argument `idx` after the start is the key count; the keys start `first`
    /// after the start.
    KeyNum { idx: i64, first: i64, step: i64 },
}

/// The positions in `args` (0 is the command name) of the keys the specs find.
pub fn key_positions(specs: &[KeySpec], args: &[Vec<u8>]) -> Vec<usize> {
    let argc = args.len() as i64;
    let mut out = Vec::new();
    for spec in specs {
        let start = match &spec.begin {
            Begin::Index(i) => *i,
            Begin::Keyword { word, from } => {
                let (mut i, dir) = if *from > 0 {
                    (*from, 1)
                } else {
                    (argc.saturating_add(*from), -1)
                };
                let mut found = 0;
                while i >= 1 && i < argc {
                    if args[i as usize].eq_ignore_ascii_case(word.as_bytes()) {
                        found = i + 1;
                        break;
                    }
                    i += dir;
                }
                found
            }
        };
        if start < 1 || start >= argc {
            continue;
        }
        let (first, last, step) = match &spec.find {
            Find::Range { last, step, limit } => {
                let last_pos = if *last >= 0 {
                    start.saturating_add(*last)
                } else if *limit <= 0 {
                    argc.saturating_add(*last)
                } else {
                    start.saturating_add((argc - start) / *limit + *last)
                };
                (start, last_pos, (*step).max(1))
            }
            Find::KeyNum { idx, first, step } => {
                let at = start.saturating_add(*idx);
                if at < 1 || at >= argc {
                    continue;
                }
                let Some(n) = std::str::from_utf8(&args[at as usize])
                    .ok()
                    .and_then(|s| s.parse::<i64>().ok())
                    .filter(|n| *n >= 0)
                else {
                    continue;
                };
                let step = (*step).max(1);
                let first = start.saturating_add(*first);
                let last = first
                    .saturating_add(n.saturating_mul(step))
                    .saturating_sub(1);
                (first, last, step)
            }
        };
        let last = last.min(argc - 1);
        let mut i = first.max(1);
        while i <= last {
            let i_us = i as usize;
            if !out.contains(&i_us) {
                out.push(i_us);
            }
            i += step;
        }
    }
    out.sort_unstable();
    out
}

/// `(key, value)` pairs of a reply that is a map (RESP3) or a flat array (RESP2).
fn pairs(v: &Value) -> Vec<(String, &Value)> {
    let name = |k: &Value| text_of(k).map(|s| s.to_lowercase());
    match v {
        Value::Map(m) => m
            .iter()
            .filter_map(|(k, v)| name(k).map(|k| (k, v)))
            .collect(),
        Value::Array(a) => a
            .chunks_exact(2)
            .filter_map(|c| name(&c[0]).map(|k| (k, &c[1])))
            .collect(),
        _ => Vec::new(),
    }
}

fn find<'a>(p: &[(String, &'a Value)], key: &str) -> Option<&'a Value> {
    p.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}

fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        other => text_of(other).and_then(|s| s.trim().parse().ok()),
    }
}

fn int_at(p: &[(String, &Value)], key: &str) -> Option<i64> {
    find(p, key).and_then(int_of)
}

fn parse_spec(v: &Value) -> Option<KeySpec> {
    let top = pairs(v);
    let bs = pairs(find(&top, "begin_search")?);
    let bs_spec = pairs(find(&bs, "spec")?);
    let begin = match text_of(find(&bs, "type")?)?.as_str() {
        "index" => Begin::Index(int_at(&bs_spec, "index")?),
        "keyword" => Begin::Keyword {
            word: text_of(find(&bs_spec, "keyword")?)?,
            from: int_at(&bs_spec, "startfrom")?,
        },
        _ => return None,
    };
    let fk = pairs(find(&top, "find_keys")?);
    let fk_spec = pairs(find(&fk, "spec")?);
    let find = match text_of(find(&fk, "type")?)?.as_str() {
        "range" => Find::Range {
            last: int_at(&fk_spec, "lastkey")?,
            step: int_at(&fk_spec, "keystep")?,
            limit: int_at(&fk_spec, "limit").unwrap_or(0),
        },
        "keynum" => Find::KeyNum {
            idx: int_at(&fk_spec, "keynumidx")?,
            first: int_at(&fk_spec, "firstkey")?,
            step: int_at(&fk_spec, "keystep")?,
        },
        _ => return None,
    };
    Some(KeySpec { begin, find })
}

/// One element of a `COMMAND INFO` reply: `[name, arity, flags, first, last, step,
/// categories, tips, key specs, subcommands]`. Before Redis 7.0 only the first six
/// exist, and the legacy `first`/`last`/`step` triple stands in for the key specs.
pub fn parse_command_info(v: &Value) -> CommandKeys {
    let Value::Array(a) = v else {
        return CommandKeys::Unknown;
    };
    if let Some(Value::Array(subs)) = a.get(9)
        && !subs.is_empty()
    {
        return CommandKeys::Container;
    }
    if let Some(Value::Array(specs)) = a.get(8) {
        let parsed: Vec<KeySpec> = specs.iter().filter_map(parse_spec).collect();
        if !parsed.is_empty() {
            return CommandKeys::Specs(parsed);
        }
    }
    CommandKeys::Specs(legacy(a).into_iter().collect())
}

/// The legacy `first`/`last`/`step` triple as one key spec; `None` for no keys.
fn legacy(a: &[Value]) -> Option<KeySpec> {
    let first = int_of(a.get(3)?)?;
    let last = int_of(a.get(4)?)?;
    let step = int_of(a.get(5)?)?;
    (first > 0 && step > 0).then(|| KeySpec {
        begin: Begin::Index(first),
        find: Find::Range {
            last: if last >= 0 { last - first } else { last },
            step,
            limit: 0,
        },
    })
}

// ---------------------------------------------------------------------------
// SLOWLOG
// ---------------------------------------------------------------------------

/// One slow log entry, as the screen shows it: no arguments, only key names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowEntry {
    pub id: u64,
    /// Unix seconds when it ran.
    pub unix: i64,
    pub micros: u64,
    /// Upper case, with the subcommand of a container command (`XINFO STREAM`).
    pub command: String,
    /// Up to [`MAX_KEYS_PER_ENTRY`] key names.
    pub keys: Vec<String>,
    pub more_keys: usize,
    /// `ip:port` of the client; empty before Redis 4.0.
    pub client: String,
    pub client_name: String,
    /// A read of a whole key: `HGETALL`, `LRANGE key 0 -1`, `GET`, and so on.
    pub whole_key_read: bool,
}

/// The server's `slowlog-log-slower-than` and `slowlog-max-len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlowConfig {
    /// Microseconds; negative disables the log, 0 logs every command.
    pub threshold_micros: i64,
    pub max_len: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlowLogReply {
    /// Newest first, as the server returns them.
    pub entries: Vec<SlowEntry>,
    /// `None` when not asked.
    pub config: Option<Result<SlowConfig, LiveError>>,
    /// Why key names are missing, when they are.
    pub keys_off: Option<LiveError>,
    /// Key specs learned in this poll, to add to the view's cache.
    pub learned: HashMap<String, CommandKeys>,
}

/// An entry as `SLOWLOG GET` sends it, before the arguments are dropped.
struct RawSlow {
    id: u64,
    unix: i64,
    micros: u64,
    args: Vec<Vec<u8>>,
    addr: String,
    name: String,
}

/// Arguments are values (P2): never print them.
impl fmt::Debug for RawSlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawSlow")
            .field("id", &self.id)
            .field("args", &self.args.len())
            .finish_non_exhaustive()
    }
}

fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::BulkString(b) => Some(b.clone()),
        Value::SimpleString(s) => Some(s.clone().into_bytes()),
        Value::VerbatimString { text, .. } => Some(text.clone().into_bytes()),
        Value::Int(n) => Some(n.to_string().into_bytes()),
        _ => None,
    }
}

fn parse_slowlog(v: &Value) -> Vec<RawSlow> {
    let Value::Array(entries) = v else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let Value::Array(f) = e else { return None };
            let Value::Array(args) = f.get(3)? else {
                return None;
            };
            Some(RawSlow {
                id: u64::try_from(int_of(f.first()?)?).ok()?,
                unix: int_of(f.get(1)?)?,
                micros: u64::try_from(int_of(f.get(2)?)?).ok()?,
                args: args.iter().filter_map(bytes_of).collect(),
                addr: f
                    .get(4)
                    .and_then(bytes_of)
                    .map_or_else(String::new, |b| clean_bytes(&b)),
                name: f
                    .get(5)
                    .and_then(bytes_of)
                    .map_or_else(String::new, |b| clean_bytes(&b)),
            })
        })
        .collect()
}

/// The slow log cuts a long command to 32 arguments, the last being
/// `... (N more arguments)`. That one is not a key.
fn drop_cut_marker(args: &mut Vec<Vec<u8>>) {
    if args.len() >= 32
        && args
            .last()
            .is_some_and(|a| a.starts_with(b"... (") && a.ends_with(b" more arguments)"))
    {
        args.pop();
    }
}

fn lower(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_lowercase()
}

/// The cache name of a command: `get`, or `xinfo|stream` for a container's subcommand.
fn spec_name(known: &HashMap<String, CommandKeys>, args: &[Vec<u8>]) -> Option<String> {
    let name = lower(args.first()?);
    match (known.get(&name), args.get(1)) {
        (Some(CommandKeys::Container), Some(sub)) => Some(format!("{name}|{}", lower(sub))),
        _ => Some(name),
    }
}

/// Whether the entry read a whole key: it is slow, so the key is probably big.
fn is_whole_key_read(args: &[Vec<u8>]) -> bool {
    let Some(name) = args.first().map(|a| lower(a)) else {
        return false;
    };
    let arg = |i: usize| args.get(i).map(Vec::as_slice);
    let has = |w: &[u8]| args.iter().skip(2).any(|a| a.eq_ignore_ascii_case(w));
    match name.as_str() {
        "hgetall" | "smembers" | "hkeys" | "hvals" | "get" => true,
        "lrange" => arg(2) == Some(b"0") && arg(3) == Some(b"-1"),
        "zrange" | "zrevrange" => {
            arg(2) == Some(b"0") && arg(3) == Some(b"-1") && !has(b"BYSCORE") && !has(b"BYLEX")
        }
        "xrange" => arg(2) == Some(b"-") && arg(3) == Some(b"+") && !has(b"COUNT"),
        _ => false,
    }
}

fn entry_from(raw: RawSlow, known: &HashMap<String, CommandKeys>) -> SlowEntry {
    let mut args = raw.args;
    drop_cut_marker(&mut args);
    let container = args
        .first()
        .is_some_and(|a| known.get(&lower(a)) == Some(&CommandKeys::Container));
    let mut command = args
        .first()
        .map_or_else(|| "(none)".to_string(), |a| clean_bytes(a).to_uppercase());
    if container && let Some(sub) = args.get(1) {
        command = format!("{command} {}", clean_bytes(sub).to_uppercase());
    }
    let mut keys = Vec::new();
    let mut more = 0;
    if let Some(CommandKeys::Specs(specs)) = spec_name(known, &args).and_then(|n| known.get(&n)) {
        for pos in key_positions(specs, &args) {
            if keys.len() < MAX_KEYS_PER_ENTRY {
                keys.push(clean_bytes(&args[pos]));
            } else {
                more += 1;
            }
        }
    }
    SlowEntry {
        id: raw.id,
        unix: raw.unix,
        micros: raw.micros,
        command,
        keys,
        more_keys: more,
        client: raw.addr,
        client_name: raw.name,
        whole_key_read: is_whole_key_read(&args),
    }
    // `args` is dropped here, with the values in it.
}

async fn slowlog(
    conn: &mut Conn,
    budget: &Budget,
    mut known: HashMap<String, CommandKeys>,
    want_config: bool,
    want_keys: bool,
) -> Result<SlowLogReply, LiveError> {
    let mut get = redis::cmd("SLOWLOG");
    get.arg("GET").arg(SLOWLOG_ENTRIES);
    let raw: Value = query(conn, budget, "SLOWLOG GET", &get).await?;
    let raws = parse_slowlog(&raw);

    let config = if want_config {
        Some(slow_config(conn, budget).await)
    } else {
        None
    };
    if let Some(Err(LiveError::Closed)) = config {
        return Err(LiveError::Closed);
    }

    let mut learned = HashMap::new();
    let mut keys_off = None;
    if want_keys {
        // Round one: the command names; round two: `name|sub` of the containers.
        for round in 0..2 {
            let mut wanted: Vec<String> = Vec::new();
            for r in &raws {
                let Some(name) = (if round == 0 {
                    r.args.first().map(|a| lower(a))
                } else {
                    spec_name(&known, &r.args).filter(|n| n.contains('|'))
                }) else {
                    continue;
                };
                if !known.contains_key(&name)
                    && !wanted.contains(&name)
                    && wanted.len() < MAX_NAMES_PER_INFO
                    && name.chars().all(|c| c.is_ascii_graphic())
                {
                    wanted.push(name);
                }
            }
            if wanted.is_empty() {
                continue;
            }
            match command_info(conn, budget, &wanted).await {
                Ok(found) => {
                    for (n, k) in found {
                        known.insert(n.clone(), k.clone());
                        learned.insert(n, k);
                    }
                }
                Err(LiveError::Closed) => return Err(LiveError::Closed),
                Err(e) => {
                    keys_off = Some(e);
                    break;
                }
            }
        }
    }
    let entries = raws.into_iter().map(|r| entry_from(r, &known)).collect();
    Ok(SlowLogReply {
        entries,
        config,
        keys_off,
        learned,
    })
}

async fn command_info(
    conn: &mut Conn,
    budget: &Budget,
    names: &[String],
) -> Result<Vec<(String, CommandKeys)>, LiveError> {
    let mut c = redis::cmd("COMMAND");
    c.arg("INFO");
    for n in names {
        c.arg(n.as_str());
    }
    let v: Value = query(conn, budget, "COMMAND INFO", &c).await?;
    let Value::Array(items) = v else {
        return Err(LiveError::Failed("COMMAND INFO failed".to_string()));
    };
    Ok(names
        .iter()
        .zip(items.iter().chain(std::iter::repeat(&Value::Nil)))
        .map(|(n, v)| (n.clone(), parse_command_info(v)))
        .collect())
}

async fn slow_config(conn: &mut Conn, budget: &Budget) -> Result<SlowConfig, LiveError> {
    let mut c = redis::cmd("CONFIG");
    c.arg("GET")
        .arg("slowlog-log-slower-than")
        .arg("slowlog-max-len");
    let v: Value = query(conn, budget, "CONFIG GET", &c).await?;
    parse_slow_config(&v).ok_or_else(|| LiveError::Failed("CONFIG GET failed".to_string()))
}

fn parse_slow_config(v: &Value) -> Option<SlowConfig> {
    let p = pairs(v);
    Some(SlowConfig {
        threshold_micros: int_at(&p, "slowlog-log-slower-than")?,
        max_len: u64::try_from(int_at(&p, "slowlog-max-len")?).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const INFO: &str = "# Server\r\nredis_version:7.4.2\r\nuptime_in_seconds:93784\r\n\r\n# Clients\r\nconnected_clients:12\r\nblocked_clients:1\r\n\r\n# Memory\r\nused_memory:1048576\r\nmaxmemory:4294967296\r\nmaxmemory_policy:allkeys-lfu\r\nmem_fragmentation_ratio:1.31\r\n\r\n# Persistence\r\nrdb_last_bgsave_status:ok\r\naof_enabled:0\r\n\r\n# Stats\r\ntotal_commands_processed:5000\r\ntotal_net_input_bytes:2000\r\ntotal_net_output_bytes:9000\r\nexpired_keys:7\r\nevicted_keys:3\r\nkeyspace_hits:90\r\nkeyspace_misses:10\r\n\r\n# Replication\r\nrole:master\r\nconnected_slaves:2\r\n\r\n# CPU\r\nused_cpu_sys:12.5\r\nused_cpu_user:30.25\r\n\r\n# Keyspace\r\ndb0:keys=3,expires=1,avg_ttl=0\r\n";

    #[test]
    fn info_fields_are_read() {
        let s = parse_info(INFO);
        assert_eq!(s.uptime_secs, Some(93_784));
        assert_eq!(s.role, Some(Role::Primary));
        assert_eq!(s.replicas, Some(2));
        assert_eq!(s.clients, Some(12));
        assert_eq!(s.blocked, Some(1));
        assert_eq!(s.used_memory, Some(1_048_576));
        assert_eq!(s.maxmemory, Some(4_294_967_296));
        assert_eq!(s.fragmentation, Some(1.31));
        assert_eq!(s.policy.as_deref(), Some("allkeys-lfu"));
        assert_eq!(s.commands, Some(5000));
        assert_eq!((s.hits, s.misses), (Some(90), Some(10)));
        assert_eq!((s.net_in, s.net_out), (Some(2000), Some(9000)));
        assert_eq!((s.evicted, s.expired), (Some(3), Some(7)));
        assert_eq!((s.cpu_user, s.cpu_sys), (Some(30.25), Some(12.5)));
        assert_eq!(
            (s.rdb_ok, s.aof_on, s.aof_ok),
            (Some(true), Some(false), None)
        );
        assert_eq!(s.link_up, None);
    }

    #[test]
    fn info_leaves_out_what_the_server_hides() {
        let s = parse_info(
            "# Server\r\nuptime_in_seconds:5\r\nrole:slave\r\nmaster_link_status:down\r\nmem_fragmentation_ratio:nan\r\nused_memory:abc\r\n",
        );
        assert_eq!(s.uptime_secs, Some(5));
        assert_eq!(s.role, Some(Role::Replica));
        assert_eq!(s.link_up, Some(false));
        assert_eq!(s.fragmentation, None);
        assert_eq!(s.used_memory, None);
        assert_eq!(s.commands, None);
        assert_eq!(parse_info(""), InfoSample::default());
        let v = parse_info("connected_replicas:4\r\nrole:replica\r\n");
        assert_eq!((v.replicas, v.role), (Some(4), Some(Role::Replica)));
    }

    proptest! {
        #[test]
        fn info_parser_never_panics(text in "\\PC{0,400}") {
            let _ = parse_info(&text);
        }

        #[test]
        fn info_numbers_round_trip(
            up in any::<u64>(), mem in any::<u64>(), max in any::<u64>(), cmds in any::<u64>(),
            cpu in 0.0f64..1.0e9,
        ) {
            let text = format!(
                "uptime_in_seconds:{up}\r\nused_memory:{mem}\r\nmaxmemory:{max}\r\ntotal_commands_processed:{cmds}\r\nused_cpu_user:{cpu}\r\n"
            );
            let s = parse_info(&text);
            prop_assert_eq!(s.uptime_secs, Some(up));
            prop_assert_eq!(s.used_memory, Some(mem));
            prop_assert_eq!(s.maxmemory, Some(max));
            prop_assert_eq!(s.commands, Some(cmds));
            prop_assert_eq!(s.cpu_user, Some(cpu));
        }
    }

    const CLIENTS: &str = "id=3 addr=10.0.1.5:50386 laddr=10.0.0.2:6379 fd=8 name=worker age=3700 idle=3650 flags=N db=0 sub=0 psub=0 ssub=0 multi=-1 watch=0 qbuf=26 qbuf-free=20448 argv-mem=10 multi-mem=0 rbs=1024 rbp=0 obl=0 oll=0 omem=0 tot-mem=22298 events=r cmd=get user=lens-admin redir=-1 resp=2 lib-name=redis-py lib-ver=5.0.1\n\
id=4 addr=127.0.0.1:50390 laddr=127.0.0.1:6379 fd=9 name= age=2 idle=0 flags=N db=1 sub=0 psub=0 multi=-1 qbuf=26 qbuf-free=20448 obl=0 oll=0 omem=64 events=r cmd=client|list user=default\n";

    #[test]
    fn client_list_is_read_without_users() {
        let list = parse_client_list(CLIENTS);
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0],
            ClientConn {
                id: 3,
                addr: "10.0.1.5:50386".into(),
                name: "worker".into(),
                lib_name: "redis-py".into(),
                lib_ver: "5.0.1".into(),
                age: 3700,
                idle: 3650,
                cmd: "get".into(),
                mem: 22_298,
                db: Some(0),
            }
        );
        // Older servers have no tot-mem: the buffers stand in.
        assert_eq!(list[1].mem, 26 + 64);
        assert_eq!(list[1].name, "");
        assert_eq!(list[1].db, Some(1));
        let shown = format!("{list:?}");
        assert!(
            !shown.contains("lens-admin") && !shown.contains("user"),
            "{shown}"
        );
    }

    #[test]
    fn client_list_skips_lines_without_an_id() {
        assert!(parse_client_list("").is_empty());
        assert!(parse_client_list("addr=1.2.3.4:5 name=x\n\nfoo\n").is_empty());
        let c = parse_client_list("id=1 name=a\u{1b}[2Jb addr=/tmp/redis.sock:0\n");
        assert_eq!(c[0].name, "a·[2Jb");
        assert_eq!(c[0].addr, "/tmp/redis.sock:0");
    }

    proptest! {
        #[test]
        fn client_list_parser_never_panics(text in "\\PC{0,400}") {
            let _ = parse_client_list(&text);
        }

        #[test]
        fn client_list_never_keeps_the_user(
            id in any::<u64>(), user in "[a-z]{8,12}", idle in any::<u32>(), age in any::<u32>(),
        ) {
            let line = format!("id={id} addr=1.2.3.4:5 name= age={age} idle={idle} cmd=get user={user}");
            let list = parse_client_list(&line);
            prop_assert_eq!(list.len(), 1);
            prop_assert_eq!(list[0].id, id);
            prop_assert_eq!(list[0].idle, u64::from(idle));
            let shown = format!("{list:?}");
            prop_assert!(!shown.contains(&user));
        }
    }

    fn s(v: &str) -> Value {
        Value::BulkString(v.as_bytes().to_vec())
    }

    fn arr(items: Vec<Value>) -> Value {
        Value::Array(items)
    }

    fn spec_value(bs: (&str, Vec<Value>), fk: (&str, Vec<Value>)) -> Value {
        arr(vec![
            s("flags"),
            arr(vec![Value::SimpleString("RW".into())]),
            s("begin_search"),
            arr(vec![s("type"), s(bs.0), s("spec"), arr(bs.1)]),
            s("find_keys"),
            arr(vec![s("type"), s(fk.0), s("spec"), arr(fk.1)]),
        ])
    }

    fn command_info(
        first: i64,
        last: i64,
        step: i64,
        specs: Vec<Value>,
        subs: Vec<Value>,
    ) -> Value {
        arr(vec![
            s("cmd"),
            Value::Int(-2),
            arr(vec![]),
            Value::Int(first),
            Value::Int(last),
            Value::Int(step),
            arr(vec![]),
            arr(vec![]),
            arr(specs),
            arr(subs),
        ])
    }

    fn range_spec(index: i64, last: i64, step: i64, limit: i64) -> Value {
        spec_value(
            ("index", vec![s("index"), Value::Int(index)]),
            (
                "range",
                vec![
                    s("lastkey"),
                    Value::Int(last),
                    s("keystep"),
                    Value::Int(step),
                    s("limit"),
                    Value::Int(limit),
                ],
            ),
        )
    }

    fn args(a: &[&str]) -> Vec<Vec<u8>> {
        a.iter().map(|x| x.as_bytes().to_vec()).collect()
    }

    fn keys_of(info: &Value, a: &[&str]) -> Vec<String> {
        let CommandKeys::Specs(specs) = parse_command_info(info) else {
            panic!("not specs: {info:?}");
        };
        let a = args(a);
        key_positions(&specs, &a)
            .into_iter()
            .map(|i| String::from_utf8(a[i].clone()).unwrap())
            .collect()
    }

    #[test]
    fn key_specs_find_the_keys() {
        // GET key
        let get = command_info(1, 1, 1, vec![range_spec(1, 0, 1, 0)], vec![]);
        assert_eq!(keys_of(&get, &["GET", "user:1"]), ["user:1"]);
        // MSET k v k v
        let mset = command_info(1, -1, 2, vec![range_spec(1, -1, 2, 0)], vec![]);
        assert_eq!(keys_of(&mset, &["MSET", "a", "1", "b", "2"]), ["a", "b"]);
        // EVAL script numkeys key... arg...
        let eval = command_info(
            0,
            0,
            0,
            vec![spec_value(
                ("index", vec![s("index"), Value::Int(2)]),
                (
                    "keynum",
                    vec![
                        s("keynumidx"),
                        Value::Int(0),
                        s("firstkey"),
                        Value::Int(1),
                        s("keystep"),
                        Value::Int(1),
                    ],
                ),
            )],
            vec![],
        );
        assert_eq!(
            keys_of(&eval, &["EVAL", "return 1", "2", "k1", "k2", "arg"]),
            ["k1", "k2"]
        );
        // XREAD [COUNT n] STREAMS key... id...: half of what follows STREAMS.
        let xread = command_info(
            0,
            0,
            0,
            vec![spec_value(
                (
                    "keyword",
                    vec![s("keyword"), s("STREAMS"), s("startfrom"), Value::Int(1)],
                ),
                (
                    "range",
                    vec![
                        s("lastkey"),
                        Value::Int(-1),
                        s("keystep"),
                        Value::Int(1),
                        s("limit"),
                        Value::Int(2),
                    ],
                ),
            )],
            vec![],
        );
        assert_eq!(
            keys_of(
                &xread,
                &["XREAD", "COUNT", "1", "streams", "s1", "s2", "0", "0"]
            ),
            ["s1", "s2"]
        );
        // ZUNIONSTORE dest numkeys key...: two specs.
        let zunion = command_info(
            1,
            1,
            1,
            vec![
                range_spec(1, 0, 1, 0),
                spec_value(
                    ("index", vec![s("index"), Value::Int(2)]),
                    (
                        "keynum",
                        vec![
                            s("keynumidx"),
                            Value::Int(0),
                            s("firstkey"),
                            Value::Int(1),
                            s("keystep"),
                            Value::Int(1),
                        ],
                    ),
                ),
            ],
            vec![],
        );
        assert_eq!(
            keys_of(
                &zunion,
                &["ZUNIONSTORE", "d", "2", "a", "b", "WEIGHTS", "1", "2"]
            ),
            ["d", "a", "b"]
        );
    }

    #[test]
    fn key_specs_survive_odd_arguments() {
        let get = command_info(1, 1, 1, vec![range_spec(1, 0, 1, 0)], vec![]);
        assert!(keys_of(&get, &["GET"]).is_empty());
        let eval = CommandKeys::Specs(vec![KeySpec {
            begin: Begin::Index(2),
            find: Find::KeyNum {
                idx: 0,
                first: 1,
                step: 1,
            },
        }]);
        let CommandKeys::Specs(specs) = eval else {
            unreachable!()
        };
        // A key count that is not a number, or larger than what is left.
        assert!(key_positions(&specs, &args(&["EVAL", "s", "x", "k"])).is_empty());
        assert_eq!(key_positions(&specs, &args(&["EVAL", "s", "9", "k"])), [3]);
        assert!(key_positions(&specs, &args(&["EVAL", "s", "-1", "k"])).is_empty());
    }

    #[test]
    fn command_info_without_specs_uses_the_legacy_triple() {
        // Redis 6.2 answers six elements.
        let del = arr(vec![
            s("del"),
            Value::Int(-2),
            arr(vec![]),
            Value::Int(1),
            Value::Int(-1),
            Value::Int(1),
        ]);
        assert_eq!(keys_of(&del, &["DEL", "a", "b", "c"]), ["a", "b", "c"]);
        let mset = arr(vec![
            s("mset"),
            Value::Int(-3),
            arr(vec![]),
            Value::Int(1),
            Value::Int(-1),
            Value::Int(2),
        ]);
        assert_eq!(keys_of(&mset, &["MSET", "a", "1", "b", "2"]), ["a", "b"]);
        // No keys at all.
        let ping = arr(vec![
            s("ping"),
            Value::Int(-1),
            arr(vec![]),
            Value::Int(0),
            Value::Int(0),
            Value::Int(0),
        ]);
        assert_eq!(parse_command_info(&ping), CommandKeys::Specs(vec![]));
        // A 7.x command without keys has empty specs and a zero triple.
        let keys = command_info(0, 0, 0, vec![], vec![]);
        assert_eq!(parse_command_info(&keys), CommandKeys::Specs(vec![]));
        assert_eq!(parse_command_info(&Value::Nil), CommandKeys::Unknown);
    }

    #[test]
    fn containers_are_looked_up_by_subcommand() {
        let xinfo = command_info(0, 0, 0, vec![], vec![arr(vec![s("xinfo|stream")])]);
        assert_eq!(parse_command_info(&xinfo), CommandKeys::Container);
        let mut known = HashMap::new();
        known.insert("xinfo".to_string(), CommandKeys::Container);
        known.insert(
            "xinfo|stream".to_string(),
            parse_command_info(&command_info(2, 2, 1, vec![range_spec(2, 0, 1, 0)], vec![])),
        );
        let raw = RawSlow {
            id: 1,
            unix: 0,
            micros: 5,
            args: args(&["XINFO", "STREAM", "orders"]),
            addr: String::new(),
            name: String::new(),
        };
        let e = entry_from(raw, &known);
        assert_eq!(e.command, "XINFO STREAM");
        assert_eq!(e.keys, ["orders"]);
    }

    #[test]
    fn resp3_maps_are_read_too() {
        let spec = Value::Map(vec![
            (
                s("begin_search"),
                Value::Map(vec![
                    (s("type"), s("index")),
                    (s("spec"), Value::Map(vec![(s("index"), Value::Int(1))])),
                ]),
            ),
            (
                s("find_keys"),
                Value::Map(vec![
                    (s("type"), s("range")),
                    (
                        s("spec"),
                        Value::Map(vec![
                            (s("lastkey"), Value::Int(0)),
                            (s("keystep"), Value::Int(1)),
                            (s("limit"), Value::Int(0)),
                        ]),
                    ),
                ]),
            ),
        ]);
        let info = command_info(1, 1, 1, vec![spec], vec![]);
        assert_eq!(keys_of(&info, &["GET", "k"]), ["k"]);
        let cfg = Value::Map(vec![
            (s("slowlog-log-slower-than"), s("10000")),
            (s("slowlog-max-len"), s("128")),
        ]);
        assert_eq!(
            parse_slow_config(&cfg),
            Some(SlowConfig {
                threshold_micros: 10_000,
                max_len: 128
            })
        );
    }

    fn raw_entry(id: i64, a: &[&str]) -> Value {
        arr(vec![
            Value::Int(id),
            Value::Int(1_700_000_000i64.wrapping_add(id)),
            Value::Int(25_000),
            arr(a.iter().map(|x| s(x)).collect()),
            s("10.0.1.5:50386"),
            s("worker"),
        ])
    }

    #[test]
    fn slow_log_entries_keep_key_names_and_no_arguments() {
        let reply = arr(vec![
            raw_entry(2, &["SET", "session:9", "hunter2-secret-value"]),
            raw_entry(1, &["KEYS", "*"]),
            arr(vec![Value::Int(0)]),
        ]);
        let raws = parse_slowlog(&reply);
        assert_eq!(raws.len(), 2);
        let shown = format!("{raws:?}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("session"),
            "{shown}"
        );
        let mut known = HashMap::new();
        known.insert(
            "set".to_string(),
            parse_command_info(&command_info(1, 1, 1, vec![range_spec(1, 0, 1, 0)], vec![])),
        );
        known.insert("keys".to_string(), CommandKeys::Specs(vec![]));
        let entries: Vec<SlowEntry> = raws.into_iter().map(|r| entry_from(r, &known)).collect();
        assert_eq!(entries[0].command, "SET");
        assert_eq!(entries[0].keys, ["session:9"]);
        assert_eq!(entries[0].client, "10.0.1.5:50386");
        assert_eq!(entries[0].client_name, "worker");
        assert_eq!(entries[0].micros, 25_000);
        assert_eq!(entries[1].command, "KEYS");
        assert!(entries[1].keys.is_empty());
        let shown = format!("{entries:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn a_cut_argument_list_and_many_keys_are_handled() {
        let mut a: Vec<String> = vec!["DEL".into()];
        a.extend((1..=31).map(|i| format!("k{i}")));
        a.push("... (40 more arguments)".into());
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        let raws = parse_slowlog(&arr(vec![raw_entry(1, &refs)]));
        let mut known = HashMap::new();
        known.insert(
            "del".to_string(),
            parse_command_info(&command_info(
                1,
                -1,
                1,
                vec![range_spec(1, -1, 1, 0)],
                vec![],
            )),
        );
        let e = entry_from(raws.into_iter().next().unwrap(), &known);
        assert_eq!(e.keys.len(), MAX_KEYS_PER_ENTRY);
        assert_eq!(e.keys[0], "k1");
        assert_eq!(e.more_keys, 31 - MAX_KEYS_PER_ENTRY);
        assert!(!e.keys.iter().any(|k| k.contains("more arguments")));
    }

    #[test]
    fn whole_key_reads_are_spotted() {
        let t = |a: &[&str]| is_whole_key_read(&args(a));
        assert!(t(&["HGETALL", "h"]));
        assert!(t(&["smembers", "s"]));
        assert!(t(&["LRANGE", "l", "0", "-1"]));
        assert!(!t(&["LRANGE", "l", "0", "99"]));
        assert!(t(&["ZRANGE", "z", "0", "-1", "WITHSCORES"]));
        assert!(!t(&["ZRANGE", "z", "0", "-1", "BYSCORE"]));
        assert!(t(&["XRANGE", "x", "-", "+"]));
        assert!(!t(&["XRANGE", "x", "-", "+", "COUNT", "10"]));
        assert!(!t(&["SET", "k", "v"]));
        assert!(!t(&[]));
    }

    #[test]
    fn slow_log_config_is_read() {
        let v = arr(vec![
            s("slowlog-log-slower-than"),
            s("-1"),
            s("slowlog-max-len"),
            s("128"),
        ]);
        assert_eq!(
            parse_slow_config(&v),
            Some(SlowConfig {
                threshold_micros: -1,
                max_len: 128
            })
        );
        assert_eq!(parse_slow_config(&arr(vec![])), None);
    }

    proptest! {
        #[test]
        fn slow_log_parsers_never_panic(
            ids in proptest::collection::vec(any::<i64>(), 0..6),
            word in "[a-zA-Z|]{0,10}",
            arg in "\\PC{0,20}",
        ) {
            let v = arr(ids.iter().map(|i| raw_entry(*i, &[&word, &arg, &arg])).collect());
            let known = HashMap::new();
            for r in parse_slowlog(&v) {
                let _ = entry_from(r, &known);
            }
            let _ = parse_command_info(&arr(vec![s(&word), s(&arg), Value::Int(*ids.first().unwrap_or(&0))]));
        }

        #[test]
        fn key_positions_stay_inside_the_arguments(
            index in -3i64..6, last in -3i64..6, step in -1i64..4, limit in -1i64..4,
            argc in 0usize..10,
        ) {
            let specs = [KeySpec {
                begin: Begin::Index(index),
                find: Find::Range { last, step, limit },
            }];
            let a: Vec<Vec<u8>> = (0..argc).map(|i| vec![b'a' + i as u8]).collect();
            for p in key_positions(&specs, &a) {
                prop_assert!(p >= 1 && p < argc.max(1));
            }
        }
    }
}
