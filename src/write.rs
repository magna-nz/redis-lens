//! The one write gate (spec "Write permissions"). Every write redis-lens sends is a
//! [`WriteOp`], and an op reaches the server only inside a [`Permit`], which only
//! [`Permit::grant`] makes. `grant` applies checks 2 and 3 of the spec itself, every
//! time:
//!
//! 2. **the profile**: never on a read-only profile ([`Profile::is_read_only`]);
//! 3. **the action**: the answer given must be the one [`decide`] asks for: none,
//!    `y`, or the exact name typed.
//!
//! Check 1, the server's ACL, is the server's: a `NOPERM` turns the action off for the
//! session ([`Refusals`]), and `ACL DRYRUN`, where allowed, tells ahead of time
//! (`conn::check_view_permissions`).
//!
//! No IO here: the live connection sends a permit's commands
//! ([`crate::live::write::run`]), bulk actions run them in batches
//! ([`crate::live::bulk`]).
//!
//! **Values stay on screen.** An op holds values as [`Raw`] and typed command lines as
//! [`Args`], whose `Debug` print only their length. [`WriteOp::shown`] writes the exact
//! command as one line for the prompt, credentials masked and every character safe to
//! draw; it is only ever drawn.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::live::cmdline::{Args, CommandInfo};
use crate::live::command;
use crate::model::{Env, Profile};
use crate::value::Raw;

/// `[settings]` in the config file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Turn off the `y`/`n` prompts of check 3 on profiles that aren't `prod`. It never
    /// applies to `prod`, to bulk actions, or to a prompt where a name is typed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_non_prod_confirm: bool,
}

impl Settings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The rows of the spec's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Edit, add, set a TTL, rename (`RENAMENX`), publish, ack, claim, create an index.
    Edit,
    /// Delete a key, field or element, trim a stream, drop an index: data is deleted or
    /// overwritten.
    Destructive,
    /// Bulk expire or delete by pattern, after the dry run.
    Bulk,
    /// A command with the `@dangerous` ACL category (`FLUSHDB`, `CONFIG SET`,
    /// `CLIENT KILL`, `SLOWLOG RESET` …).
    Dangerous,
}

/// What check 3 asks before an op is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Not sent. Why, in redis-lens's own words, safe to draw.
    Refuse(String),
    /// Sent without a question.
    Run,
    /// `y` sends, `n` or `esc` cancels.
    Ask,
    /// Sent once exactly this text is typed and `enter` pressed. A key, index or
    /// pattern name, or the profile's name: safe to draw once escaped.
    Type(String),
}

/// The answer given to a [`Decision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// No question was asked.
    None,
    /// `y`.
    Yes,
    /// Text typed and `enter` pressed.
    Typed(String),
}

/// Write commands the server refused for this user (`NOPERM`, or `ACL DRYRUN`
/// saying the command is not allowed), by the name [`WriteOp::command`] gives. Kept
/// for the session; a refused action is off with a fixed reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refusals(BTreeSet<String>);

impl Refusals {
    pub fn add(&mut self, command: &str) {
        self.0.insert(command.to_ascii_uppercase());
    }

    pub fn refused(&self, command: &str) -> bool {
        self.0.contains(&command.to_ascii_uppercase())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

/// What reads the profile is read-only, said the same way everywhere.
pub fn read_only_reason(profile: &Profile) -> String {
    format!(
        "Not sent: {} is read-only. Writes are turned on with w on 0 Connections.",
        crate::value::escape(&profile.name)
    )
}

/// Why an action the server refused is off.
pub fn refused_reason(command: &str) -> String {
    format!("Not sent: the server refused {command} for this user.")
}

/// Check 2 and 3 for an op of `kind` on `profile` (spec "Write permissions" table).
/// `name` is what a destructive or bulk op asks to have typed on `prod` (the key, the
/// index or the pattern); a dangerous op asks for the profile's name instead.
pub fn decide(profile: &Profile, settings: &Settings, kind: Kind, name: &str) -> Decision {
    if profile.is_read_only() {
        return Decision::Refuse(read_only_reason(profile));
    }
    let prod = profile.env == Env::Prod;
    let skip = settings.skip_non_prod_confirm && !prod;
    match kind {
        Kind::Edit if prod => Decision::Ask,
        Kind::Edit => Decision::Run,
        Kind::Destructive | Kind::Bulk if prod => Decision::Type(if name.is_empty() {
            profile.name.clone()
        } else {
            name.to_string()
        }),
        Kind::Destructive if skip => Decision::Run,
        Kind::Destructive | Kind::Bulk => Decision::Ask,
        Kind::Dangerous if prod => Decision::Type(profile.name.clone()),
        Kind::Dangerous if skip => Decision::Run,
        Kind::Dangerous => Decision::Ask,
    }
}

/// The decision for `op` on `profile`, refusals included: what [`Permit::grant`] holds
/// the answer to.
pub fn decide_op(
    op: &WriteOp,
    profile: &Profile,
    settings: &Settings,
    refusals: &Refusals,
) -> Decision {
    if let Some(why) = op.invalid() {
        return Decision::Refuse(why);
    }
    let command = op.command();
    if refusals.refused(&command) {
        return Decision::Refuse(refused_reason(&command));
    }
    decide(profile, settings, op.kind(), &op.name_to_type())
}

/// Leave to send one op. Only [`Permit::grant`] makes one, so holding one means the
/// gate let the op through for the profile it was granted on. `Debug` shows the
/// command's name only.
#[derive(Clone, PartialEq)]
pub struct Permit {
    op: WriteOp,
}

impl Permit {
    /// The gate: a permit for `op` on `profile` when [`decide_op`] says
    /// [`Decision::Run`] and no question was asked, [`Decision::Ask`] and the answer is
    /// `y`, or [`Decision::Type`] and exactly that text was typed. Otherwise why not,
    /// in redis-lens's own words, safe to draw.
    pub fn grant(
        op: WriteOp,
        profile: &Profile,
        settings: &Settings,
        refusals: &Refusals,
        answer: &Answer,
    ) -> Result<Self, String> {
        match (decide_op(&op, profile, settings, refusals), answer) {
            (Decision::Refuse(why), _) => Err(why),
            (Decision::Run, _) | (Decision::Ask, Answer::Yes) => Ok(Self { op }),
            (Decision::Type(want), Answer::Typed(got)) if *got == want => Ok(Self { op }),
            (Decision::Type(_), Answer::Typed(_)) => {
                Err("Not sent: the name typed did not match.".to_string())
            }
            (Decision::Ask | Decision::Type(_), _) => {
                Err("Not sent: it was not confirmed.".to_string())
            }
        }
    }

    pub fn op(&self) -> &WriteOp {
        &self.op
    }

    /// A permit made without the gate, for tests of what runs a permit.
    #[cfg(test)]
    pub(crate) fn for_test(op: WriteOp) -> Self {
        Self { op }
    }
}

impl fmt::Debug for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Permit({})", self.op.command())
    }
}

/// How `t` changes a TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// `EXPIRE key seconds` (or `HEXPIRE`).
    Seconds(u64),
    /// `PERSIST key` (or `HPERSIST`).
    Persist,
}

/// What `N` creates: a new key of one type with its first item.
#[derive(Debug, Clone, PartialEq)]
pub enum NewKey {
    String(Raw),
    Hash {
        field: Raw,
        value: Raw,
    },
    List(Raw),
    Set(Raw),
    ZSet {
        member: Raw,
        score: f64,
    },
    /// `XADD key * field value`.
    Stream {
        field: Raw,
        value: Raw,
    },
}

/// `X`: how a stream is trimmed. Always approximate (`~`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trim {
    /// `XTRIM key MAXLEN ~ n`.
    MaxLen(u64),
    /// `XTRIM key MINID ~ id`.
    MinId(String),
}

/// The slow log settings `c` may change (`CONFIG SET`), and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowlogSetting {
    /// `slowlog-log-slower-than`, in microseconds (`-1` turns the log off, `0` logs
    /// every command).
    Threshold(i64),
    /// `slowlog-max-len`.
    MaxLen(u64),
}

/// `C`: what an index covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOn {
    Hash,
    Json,
}

/// One field of the `FT.CREATE` form: its name (a hash field, or a JSON path such as
/// `$.name` with an alias) and its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexField {
    pub name: String,
    /// For a JSON path: `AS <alias>`.
    pub alias: Option<String>,
    /// `TEXT`, `TAG`, `NUMERIC` or `GEO`.
    pub kind: String,
}

/// `x` or `D` on a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkAction {
    /// `EXPIRE key seconds NX`: keys without a TTL only.
    Expire { seconds: u64 },
    /// `UNLINK key`.
    Delete,
}

/// Every write redis-lens can send, with the exact commands. Keys and names are as the
/// server returned them; values are [`Raw`].
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOp {
    // Key detail (spec "Edits on the Key detail").
    /// `e` on a string: sent with `WATCH`, then `MULTI`, `SET key value KEEPTTL`,
    /// `EXEC`, only when the value is still `old` (the whole value as loaded).
    SetString { key: String, old: Raw, value: Raw },
    /// `e` on a hash field: `HSET key field value`.
    HSet { key: String, field: Raw, value: Raw },
    /// `+` on a hash: `HSETNX key field value`, which never overwrites a field.
    HSetNx { key: String, field: Raw, value: Raw },
    /// `e` on a list element: `WATCH`, then `LSET key index value` in `MULTI`/`EXEC`
    /// when the element at `index` is still `old`.
    LSet {
        key: String,
        index: u64,
        old: Raw,
        value: Raw,
    },
    /// `+` on a list: `RPUSH key value`.
    RPush { key: String, value: Raw },
    /// `e` on a set member: `SREM key old` and `SADD key value` in `MULTI`/`EXEC`.
    SReplace { key: String, old: Raw, value: Raw },
    /// `+` on a set: `SADD key member`.
    SAdd { key: String, member: Raw },
    /// `e` on a sorted set member: `ZADD key XX score member` (the score changes).
    ZScore {
        key: String,
        member: Raw,
        score: f64,
    },
    /// `+` on a sorted set: `ZADD key NX score member`.
    ZAdd {
        key: String,
        member: Raw,
        score: f64,
    },
    /// `+` on a stream: `XADD key * field value …`.
    XAdd {
        key: String,
        fields: Vec<(Raw, Raw)>,
    },
    /// `e` on a JSON document: `JSON.SET key $ value`.
    JsonSet { key: String, value: Raw },
    /// `t`: `EXPIRE key seconds` or `PERSIST key`.
    KeyTtl { key: String, ttl: Ttl },
    /// `t` on a hash field: `HEXPIRE key seconds FIELDS 1 field` or
    /// `HPERSIST key FIELDS 1 field`.
    FieldTtl { key: String, field: Raw, ttl: Ttl },
    /// `R`: `RENAMENX key new`. It never overwrites a key.
    Rename { key: String, new: String },
    /// `d` on a hash field: `HDEL key field`.
    HDel { key: String, field: Raw },
    /// `d` on a set member: `SREM key member`.
    SRem { key: String, member: Raw },
    /// `d` on a sorted set member: `ZREM key member`.
    ZRem { key: String, member: Raw },
    /// `d` on a stream entry: `XDEL key id`.
    XDel { key: String, id: String },
    /// `d` on a list element: `WATCH`, then `LSET key index <marker>` and
    /// `LREM key 1 <marker>` in `MULTI`/`EXEC` when the element at `index` is still
    /// `old`. The marker is unique to this op.
    LDel {
        key: String,
        index: u64,
        old: Raw,
        marker: String,
    },
    /// `D`: `UNLINK key`.
    Unlink { key: String },
    /// `N` on the key list: the key with its first item, only when it does not exist
    /// (`SET key value NX`; for the other types `WATCH`, `EXISTS`, then the add in
    /// `MULTI`/`EXEC`).
    NewKey { key: String, value: NewKey },

    // Server.
    /// `K` on Clients: `CLIENT KILL ID id`.
    ClientKill { id: u64 },
    /// `c` on the Slow log: `CONFIG SET slowlog-log-slower-than n` or
    /// `CONFIG SET slowlog-max-len n`.
    SlowlogConfig(SlowlogSetting),
    /// `R` on the Slow log: `SLOWLOG RESET`.
    SlowlogReset,

    // Streams.
    /// `c`: `XAUTOCLAIM key group consumer min-idle-ms 0-0 COUNT count`, claiming
    /// pending entries idle at least `min_idle_ms` for `consumer`.
    XAutoClaim {
        key: String,
        group: String,
        consumer: String,
        min_idle_ms: u64,
        count: u64,
    },
    /// `a`: `XACK key group id`.
    XAck {
        key: String,
        group: String,
        id: String,
    },
    /// `X`: `XTRIM key MAXLEN ~ n` or `XTRIM key MINID ~ id`.
    XTrim { key: String, trim: Trim },

    // Pub/Sub.
    /// `p`: `PUBLISH channel message`.
    Publish { channel: String, message: Raw },

    // Search.
    /// `C`: `FT.CREATE index ON HASH|JSON PREFIX n p… SCHEMA field [AS alias] type …`.
    FtCreate {
        index: String,
        on: IndexOn,
        prefixes: Vec<String>,
        fields: Vec<IndexField>,
    },
    /// `D`: `FT.DROPINDEX index`, never with `DD`: the documents stay.
    FtDropIndex { index: String },

    // Patterns.
    /// `x` or `D`: every key of `pattern` (found with `SCAN MATCH glob`, kept when it
    /// infers to `pattern`), in batches. Run by [`crate::live::bulk`].
    Bulk {
        pattern: String,
        glob: String,
        action: BulkAction,
    },

    // Command line.
    /// A write typed on the command line, as typed, with what `COMMAND INFO` said
    /// about it (spec "Command line panel": "Writes (P3)").
    Command { args: Args, info: CommandInfo },
}

/// Commands that delete or overwrite data (spec "What counts as destructive or
/// dangerous"), lower-case as `COMMAND INFO` names them. `RENAME` may overwrite a key,
/// so it counts; `RENAMENX` never does.
pub const DESTRUCTIVE: [&str; 44] = [
    "del",
    "unlink",
    "hdel",
    "hgetdel",
    "srem",
    "zrem",
    "lrem",
    "xdel",
    "xtrim",
    "flushdb",
    "flushall",
    "rename",
    "ft.dropindex",
    "ft._dropindexifx",
    "getdel",
    "ltrim",
    "lpop",
    "rpop",
    "lmpop",
    "spop",
    "zpopmin",
    "zpopmax",
    "zmpop",
    "zremrangebyrank",
    "zremrangebyscore",
    "zremrangebylex",
    "rpoplpush",
    "lmove",
    "smove",
    "move",
    "copy",
    "restore",
    "bitop",
    "sinterstore",
    "sunionstore",
    "sdiffstore",
    "zinterstore",
    "zunionstore",
    "zdiffstore",
    "xdelex",
    "xackdel",
    "vrem",
    "delifeq",
    "delex",
];

/// Commands that write a destination they overwrite or delete keys and elements
/// besides those on [`DESTRUCTIVE`]: `ZRANGESTORE`, `GEO*` with `STORE`, `SORT … STORE`,
/// `XGROUP DESTROY|DELCONSUMER`, and a TTL that may not be in the future (`EXPIRE k 0`
/// deletes `k`; an absolute time may be past).
fn destructive_by_arguments(args: &[Vec<u8>], name: &str) -> bool {
    let has = |w: &str| {
        args.iter()
            .skip(1)
            .any(|a| a.eq_ignore_ascii_case(w.as_bytes()))
    };
    let not_positive = |a: Option<&Vec<u8>>| {
        a.and_then(|a| std::str::from_utf8(a).ok())
            .and_then(|a| a.trim().parse::<i64>().ok())
            .is_none_or(|n| n <= 0)
    };
    // `EX n` / `PX n` with n not positive, or any `EXAT` / `PXAT`.
    let ttl_option = || {
        args.iter().enumerate().skip(2).any(|(i, a)| {
            let is = |w: &str| a.eq_ignore_ascii_case(w.as_bytes());
            is("exat") || is("pxat") || ((is("ex") || is("px")) && not_positive(args.get(i + 1)))
        })
    };
    let sub = || args.get(1).map(|a| a.to_ascii_lowercase());
    match name {
        "getex" | "set" => ttl_option(),
        "hexpire" | "hpexpire" => not_positive(args.get(2)),
        "hexpireat" | "hpexpireat" => true,
        "xgroup" => matches!(sub().as_deref(), Some(b"destroy" | b"delconsumer")),
        "zrangestore" => true,
        "sort" | "georadius" | "georadiusbymember" => has("store") || has("storedist"),
        "geosearchstore" => true,
        "expire" | "pexpire" => not_positive(args.get(2)),
        "expireat" | "pexpireat" => true,
        _ => false,
    }
}

/// The kind of a command typed on the command line: `@dangerous` from `COMMAND INFO`
/// (the subcommand's entry for a container command), then the destructive list, then
/// an edit.
pub fn command_kind(args: &[Vec<u8>], info: &CommandInfo) -> Kind {
    let cmd = args
        .get(1)
        .filter(|_| !info.subcommands.is_empty())
        .and_then(|s| info.subcommand(s))
        .unwrap_or(info);
    if cmd.in_category("@dangerous") || info.in_category("@dangerous") {
        Kind::Dangerous
    } else if DESTRUCTIVE.contains(&info.name.as_str())
        || destructive_by_arguments(args, &info.name)
    {
        Kind::Destructive
    } else {
        Kind::Edit
    }
}

fn word(out: &mut String, w: &[u8]) {
    if !out.is_empty() {
        out.push(' ');
    }
    command::push_word(w, out);
}

fn num(n: impl fmt::Display) -> Vec<u8> {
    n.to_string().into_bytes()
}

/// A score as `ZADD` takes it.
pub fn score_arg(score: f64) -> Vec<u8> {
    if score == f64::INFINITY {
        b"+inf".to_vec()
    } else if score == f64::NEG_INFINITY {
        b"-inf".to_vec()
    } else {
        num(score)
    }
}

impl WriteOp {
    /// The row of the spec's table this op is in.
    pub fn kind(&self) -> Kind {
        use WriteOp::*;
        match self {
            SetString { .. } | HSet { .. } | LSet { .. } | SReplace { .. } | JsonSet { .. } => {
                // An edit overwrites the old value, but the spec lists edits as edits:
                // the value shown is the one replaced.
                Kind::Edit
            }
            HSetNx { .. }
            | RPush { .. }
            | SAdd { .. }
            | ZScore { .. }
            | ZAdd { .. }
            | XAdd { .. }
            | KeyTtl { .. }
            | FieldTtl { .. }
            | Rename { .. }
            | NewKey { .. }
            | XAutoClaim { .. }
            | XAck { .. }
            | Publish { .. }
            | FtCreate { .. } => Kind::Edit,
            HDel { .. }
            | SRem { .. }
            | ZRem { .. }
            | XDel { .. }
            | LDel { .. }
            | Unlink { .. }
            | XTrim { .. }
            | FtDropIndex { .. } => Kind::Destructive,
            ClientKill { .. } | SlowlogConfig(_) | SlowlogReset => Kind::Dangerous,
            Bulk { .. } => Kind::Bulk,
            Command { args, info } => command_kind(args.as_slice(), info),
        }
    }

    /// The command's name as a user writes it (`HSET`, `CLIENT KILL`): what a `NOPERM`
    /// is about, and what [`Refusals`] holds.
    pub fn command(&self) -> String {
        use WriteOp::*;
        let name = match self {
            SetString { .. } => "SET",
            HSet { .. } => "HSET",
            HSetNx { .. } => "HSETNX",
            LSet { .. } => "LSET",
            RPush { .. } => "RPUSH",
            SReplace { .. } | SAdd { .. } => "SADD",
            ZScore { .. } | ZAdd { .. } => "ZADD",
            XAdd { .. } => "XADD",
            JsonSet { .. } => "JSON.SET",
            KeyTtl {
                ttl: Ttl::Seconds(_),
                ..
            } => "EXPIRE",
            KeyTtl {
                ttl: Ttl::Persist, ..
            } => "PERSIST",
            FieldTtl {
                ttl: Ttl::Seconds(_),
                ..
            } => "HEXPIRE",
            FieldTtl {
                ttl: Ttl::Persist, ..
            } => "HPERSIST",
            Rename { .. } => "RENAMENX",
            HDel { .. } => "HDEL",
            SRem { .. } => "SREM",
            ZRem { .. } => "ZREM",
            XDel { .. } => "XDEL",
            LDel { .. } => "LREM",
            Unlink { .. } => "UNLINK",
            NewKey { value, .. } => match value {
                self::NewKey::String(_) => "SET",
                self::NewKey::Hash { .. } => "HSET",
                self::NewKey::List(_) => "RPUSH",
                self::NewKey::Set(_) => "SADD",
                self::NewKey::ZSet { .. } => "ZADD",
                self::NewKey::Stream { .. } => "XADD",
            },
            ClientKill { .. } => "CLIENT KILL",
            SlowlogConfig(_) => "CONFIG SET",
            SlowlogReset => "SLOWLOG RESET",
            XAutoClaim { .. } => "XAUTOCLAIM",
            XAck { .. } => "XACK",
            XTrim { .. } => "XTRIM",
            Publish { .. } => "PUBLISH",
            FtCreate { .. } => "FT.CREATE",
            FtDropIndex { .. } => "FT.DROPINDEX",
            Bulk {
                action: BulkAction::Expire { .. },
                ..
            } => "EXPIRE",
            Bulk {
                action: BulkAction::Delete,
                ..
            } => "UNLINK",
            Command { args, info } => {
                return args
                    .as_slice()
                    .get(1)
                    .filter(|_| !info.subcommands.is_empty())
                    .and_then(|s| info.subcommand(s))
                    .unwrap_or(info)
                    .display();
            }
        };
        name.to_string()
    }

    /// What a destructive or bulk op asks to have typed on `prod`: the key, the index,
    /// the pattern, or for a command typed on the command line its first argument.
    pub fn name_to_type(&self) -> String {
        use WriteOp::*;
        match self {
            SetString { key, .. }
            | HSet { key, .. }
            | HSetNx { key, .. }
            | LSet { key, .. }
            | RPush { key, .. }
            | SReplace { key, .. }
            | SAdd { key, .. }
            | ZScore { key, .. }
            | ZAdd { key, .. }
            | XAdd { key, .. }
            | JsonSet { key, .. }
            | KeyTtl { key, .. }
            | FieldTtl { key, .. }
            | Rename { key, .. }
            | HDel { key, .. }
            | SRem { key, .. }
            | ZRem { key, .. }
            | XDel { key, .. }
            | LDel { key, .. }
            | Unlink { key }
            | NewKey { key, .. }
            | XAutoClaim { key, .. }
            | XAck { key, .. }
            | XTrim { key, .. } => key.clone(),
            Publish { channel, .. } => channel.clone(),
            FtCreate { index, .. } | FtDropIndex { index } => index.clone(),
            Bulk { pattern, .. } => pattern.clone(),
            ClientKill { id } => id.to_string(),
            SlowlogConfig(_) | SlowlogReset => String::new(),
            // The key, for a command with only one; otherwise nothing, and the
            // profile's name is typed instead (`DEL a b c`, `DEL ""`).
            Command { args, .. } => match args.as_slice() {
                [_, key] if !key.is_empty() => String::from_utf8_lossy(key).into_owned(),
                _ => String::new(),
            },
        }
    }

    /// Why the op can't be sent at all, before any question: a typed command with no
    /// name, a `FT.CREATE` without fields. `None` when it is well formed.
    pub fn invalid(&self) -> Option<String> {
        match self {
            // Only what the command line's own gate calls a write: scripts, unlisted
            // module commands, blocking and connection commands stay refused here too.
            WriteOp::Command { args, info } => {
                match crate::live::cmdline::gate(args.as_slice(), Some(info)) {
                    crate::live::cmdline::Gate::Write => None,
                    crate::live::cmdline::Gate::Refuse(why) => Some(why),
                    crate::live::cmdline::Gate::Send | crate::live::cmdline::Gate::Ask => {
                        Some("Not sent: a readonly command is not sent as a write.".into())
                    }
                }
            }
            WriteOp::FtCreate { fields, .. } if fields.is_empty() => {
                Some("Not sent: an index needs at least one field.".into())
            }
            WriteOp::XAdd { fields, .. } if fields.is_empty() => {
                Some("Not sent: an entry needs at least one field.".into())
            }
            WriteOp::Rename { key, new } if key == new => {
                Some("Not sent: the new name is the same as the old one.".into())
            }
            WriteOp::Bulk { pattern, glob, .. } if pattern.is_empty() || glob.is_empty() => {
                Some("Not sent: no pattern.".into())
            }
            _ => None,
        }
    }

    /// The commands that write, word by word, in the order they are sent. Checks sent
    /// before them (`WATCH`, the comparison read, `EXISTS`) and `MULTI`/`EXEC` are not
    /// listed; [`WriteOp::shown`] says when they are used. A bulk op lists its command
    /// for one key, `<key>`.
    pub fn commands(&self) -> Vec<Vec<Vec<u8>>> {
        use WriteOp::*;
        let b = |s: &str| s.as_bytes().to_vec();
        let r = |v: &Raw| v.as_bytes().to_vec();
        match self {
            SetString { key, value, .. } => {
                vec![vec![b("SET"), b(key), r(value), b("KEEPTTL")]]
            }
            HSet { key, field, value } => vec![vec![b("HSET"), b(key), r(field), r(value)]],
            HSetNx { key, field, value } => {
                vec![vec![b("HSETNX"), b(key), r(field), r(value)]]
            }
            LSet {
                key, index, value, ..
            } => vec![vec![b("LSET"), b(key), num(index), r(value)]],
            RPush { key, value } => vec![vec![b("RPUSH"), b(key), r(value)]],
            SReplace { key, old, value } => vec![
                vec![b("SREM"), b(key), r(old)],
                vec![b("SADD"), b(key), r(value)],
            ],
            SAdd { key, member } => vec![vec![b("SADD"), b(key), r(member)]],
            ZScore { key, member, score } => vec![vec![
                b("ZADD"),
                b(key),
                b("XX"),
                score_arg(*score),
                r(member),
            ]],
            ZAdd { key, member, score } => vec![vec![
                b("ZADD"),
                b(key),
                b("NX"),
                score_arg(*score),
                r(member),
            ]],
            XAdd { key, fields } => {
                let mut c = vec![b("XADD"), b(key), b("*")];
                for (f, v) in fields {
                    c.push(r(f));
                    c.push(r(v));
                }
                vec![c]
            }
            JsonSet { key, value } => vec![vec![b("JSON.SET"), b(key), b("$"), r(value)]],
            KeyTtl {
                key,
                ttl: Ttl::Seconds(s),
            } => vec![vec![b("EXPIRE"), b(key), num(s)]],
            KeyTtl {
                key,
                ttl: Ttl::Persist,
            } => vec![vec![b("PERSIST"), b(key)]],
            FieldTtl {
                key,
                field,
                ttl: Ttl::Seconds(s),
            } => vec![vec![
                b("HEXPIRE"),
                b(key),
                num(s),
                b("FIELDS"),
                b("1"),
                r(field),
            ]],
            FieldTtl {
                key,
                field,
                ttl: Ttl::Persist,
            } => vec![vec![b("HPERSIST"), b(key), b("FIELDS"), b("1"), r(field)]],
            Rename { key, new } => vec![vec![b("RENAMENX"), b(key), b(new)]],
            HDel { key, field } => vec![vec![b("HDEL"), b(key), r(field)]],
            SRem { key, member } => vec![vec![b("SREM"), b(key), r(member)]],
            ZRem { key, member } => vec![vec![b("ZREM"), b(key), r(member)]],
            XDel { key, id } => vec![vec![b("XDEL"), b(key), b(id)]],
            LDel {
                key, index, marker, ..
            } => vec![
                vec![b("LSET"), b(key), num(index), b(marker)],
                vec![b("LREM"), b(key), b("1"), b(marker)],
            ],
            Unlink { key } => vec![vec![b("UNLINK"), b(key)]],
            NewKey { key, value } => vec![match value {
                self::NewKey::String(v) => vec![b("SET"), b(key), r(v), b("NX")],
                self::NewKey::Hash { field, value } => vec![b("HSET"), b(key), r(field), r(value)],
                self::NewKey::List(v) => vec![b("RPUSH"), b(key), r(v)],
                self::NewKey::Set(v) => vec![b("SADD"), b(key), r(v)],
                self::NewKey::ZSet { member, score } => {
                    vec![b("ZADD"), b(key), score_arg(*score), r(member)]
                }
                self::NewKey::Stream { field, value } => {
                    vec![b("XADD"), b(key), b("*"), r(field), r(value)]
                }
            }],
            ClientKill { id } => vec![vec![b("CLIENT"), b("KILL"), b("ID"), num(id)]],
            SlowlogConfig(SlowlogSetting::Threshold(n)) => vec![vec![
                b("CONFIG"),
                b("SET"),
                b("slowlog-log-slower-than"),
                num(n),
            ]],
            SlowlogConfig(SlowlogSetting::MaxLen(n)) => {
                vec![vec![b("CONFIG"), b("SET"), b("slowlog-max-len"), num(n)]]
            }
            SlowlogReset => vec![vec![b("SLOWLOG"), b("RESET")]],
            XAutoClaim {
                key,
                group,
                consumer,
                min_idle_ms,
                count,
            } => vec![vec![
                b("XAUTOCLAIM"),
                b(key),
                b(group),
                b(consumer),
                num(min_idle_ms),
                b("0-0"),
                b("COUNT"),
                num(count),
            ]],
            XAck { key, group, id } => vec![vec![b("XACK"), b(key), b(group), b(id)]],
            XTrim {
                key,
                trim: Trim::MaxLen(n),
            } => vec![vec![b("XTRIM"), b(key), b("MAXLEN"), b("~"), num(n)]],
            XTrim {
                key,
                trim: Trim::MinId(id),
            } => vec![vec![b("XTRIM"), b(key), b("MINID"), b("~"), b(id)]],
            Publish { channel, message } => vec![vec![b("PUBLISH"), b(channel), r(message)]],
            FtCreate {
                index,
                on,
                prefixes,
                fields,
            } => {
                let mut c = vec![
                    b("FT.CREATE"),
                    b(index),
                    b("ON"),
                    b(match on {
                        IndexOn::Hash => "HASH",
                        IndexOn::Json => "JSON",
                    }),
                ];
                if !prefixes.is_empty() {
                    c.push(b("PREFIX"));
                    c.push(num(prefixes.len()));
                    c.extend(prefixes.iter().map(|p| b(p)));
                }
                c.push(b("SCHEMA"));
                for f in fields {
                    c.push(b(&f.name));
                    if let Some(a) = &f.alias {
                        c.push(b("AS"));
                        c.push(b(a));
                    }
                    c.push(b(&f.kind));
                }
                vec![c]
            }
            FtDropIndex { index } => vec![vec![b("FT.DROPINDEX"), b(index)]],
            Bulk {
                action: BulkAction::Expire { seconds },
                ..
            } => vec![vec![b("EXPIRE"), b("<key>"), num(seconds), b("NX")]],
            Bulk {
                action: BulkAction::Delete,
                ..
            } => vec![vec![b("UNLINK"), b("<key>")]],
            Command { args, .. } => vec![args.as_slice().to_vec()],
        }
    }

    /// The exact commands as one line for the prompt ("Sends: …"), joined with `; `,
    /// credentials masked ([`command::masked_line`]) and every character safe to draw.
    /// Says when the commands go in `MULTI`/`EXEC` after a check. Values: on screen
    /// only.
    pub fn shown(&self) -> String {
        let lines: Vec<String> = self
            .commands()
            .iter()
            .map(|c| command::masked_line(c))
            .collect();
        let mut out = lines.join("; ");
        match self {
            WriteOp::SetString { .. } | WriteOp::LSet { .. } | WriteOp::LDel { .. } => {
                out.push_str("  (in MULTI/EXEC after WATCH, only if unchanged)");
            }
            WriteOp::SReplace { .. } => out.push_str("  (in MULTI/EXEC)"),
            WriteOp::NewKey {
                value: NewKey::String(_),
                ..
            } => {}
            WriteOp::NewKey { .. } => {
                out.push_str("  (in MULTI/EXEC after WATCH, only if the key is new)");
            }
            WriteOp::Bulk { glob, .. } => {
                let mut g = String::new();
                word(&mut g, glob.as_bytes());
                out = format!("{out}  for each key of SCAN MATCH {g}, 500 at a time");
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Auth, Endpoint};

    fn profile(env: Env, read_only: Option<bool>) -> Profile {
        Profile {
            name: "p1".into(),
            env,
            endpoint: Endpoint {
                host: "localhost".into(),
                port: 6379,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
        }
    }

    const ON: Settings = Settings {
        skip_non_prod_confirm: false,
    };
    const SKIP: Settings = Settings {
        skip_non_prod_confirm: true,
    };

    #[test]
    fn the_table() {
        use Decision::*;
        let local = profile(Env::Local, None);
        let prod_on = profile(Env::Prod, Some(false));
        for kind in [Kind::Edit, Kind::Destructive, Kind::Bulk, Kind::Dangerous] {
            for p in [
                profile(Env::Prod, None),
                profile(Env::Local, Some(true)),
                profile(Env::Prod, Some(true)),
            ] {
                assert!(
                    matches!(decide(&p, &SKIP, kind, "k"), Refuse(_)),
                    "{kind:?} {p:?}"
                );
            }
        }
        assert_eq!(decide(&local, &ON, Kind::Edit, "k"), Run);
        assert_eq!(decide(&local, &ON, Kind::Destructive, "k"), Ask);
        assert_eq!(decide(&local, &ON, Kind::Bulk, "k"), Ask);
        assert_eq!(decide(&local, &ON, Kind::Dangerous, "k"), Ask);
        assert_eq!(decide(&prod_on, &ON, Kind::Edit, "k"), Ask);
        assert_eq!(
            decide(&prod_on, &ON, Kind::Destructive, "k"),
            Type("k".into())
        );
        assert_eq!(
            decide(&prod_on, &ON, Kind::Bulk, "pat:*"),
            Type("pat:*".into())
        );
        assert_eq!(
            decide(&prod_on, &ON, Kind::Dangerous, "k"),
            Type("p1".into())
        );
        // skip_non_prod_confirm: never on prod, never on bulk actions.
        assert_eq!(decide(&local, &SKIP, Kind::Destructive, "k"), Run);
        assert_eq!(decide(&local, &SKIP, Kind::Dangerous, "k"), Run);
        assert_eq!(decide(&local, &SKIP, Kind::Bulk, "k"), Ask);
        assert_eq!(decide(&prod_on, &SKIP, Kind::Edit, "k"), Ask);
        assert_eq!(
            decide(&prod_on, &SKIP, Kind::Destructive, "k"),
            Type("k".into())
        );
        assert_eq!(
            decide(&prod_on, &SKIP, Kind::Dangerous, "k"),
            Type("p1".into())
        );
    }

    fn every_op() -> Vec<WriteOp> {
        let k = || "user:1".to_string();
        let v = || Raw::from("v");
        vec![
            WriteOp::SetString {
                key: k(),
                old: v(),
                value: v(),
            },
            WriteOp::HSet {
                key: k(),
                field: v(),
                value: v(),
            },
            WriteOp::HSetNx {
                key: k(),
                field: v(),
                value: v(),
            },
            WriteOp::LSet {
                key: k(),
                index: 1,
                old: v(),
                value: v(),
            },
            WriteOp::RPush {
                key: k(),
                value: v(),
            },
            WriteOp::SReplace {
                key: k(),
                old: v(),
                value: Raw::from("w"),
            },
            WriteOp::SAdd {
                key: k(),
                member: v(),
            },
            WriteOp::ZScore {
                key: k(),
                member: v(),
                score: 1.5,
            },
            WriteOp::ZAdd {
                key: k(),
                member: v(),
                score: 1.0,
            },
            WriteOp::XAdd {
                key: k(),
                fields: vec![(v(), v())],
            },
            WriteOp::JsonSet {
                key: k(),
                value: v(),
            },
            WriteOp::KeyTtl {
                key: k(),
                ttl: Ttl::Seconds(60),
            },
            WriteOp::KeyTtl {
                key: k(),
                ttl: Ttl::Persist,
            },
            WriteOp::FieldTtl {
                key: k(),
                field: v(),
                ttl: Ttl::Seconds(60),
            },
            WriteOp::FieldTtl {
                key: k(),
                field: v(),
                ttl: Ttl::Persist,
            },
            WriteOp::Rename {
                key: k(),
                new: "user:2".into(),
            },
            WriteOp::HDel {
                key: k(),
                field: v(),
            },
            WriteOp::SRem {
                key: k(),
                member: v(),
            },
            WriteOp::ZRem {
                key: k(),
                member: v(),
            },
            WriteOp::XDel {
                key: k(),
                id: "1-0".into(),
            },
            WriteOp::LDel {
                key: k(),
                index: 0,
                old: v(),
                marker: "m".into(),
            },
            WriteOp::Unlink { key: k() },
            WriteOp::NewKey {
                key: k(),
                value: NewKey::String(v()),
            },
            WriteOp::NewKey {
                key: k(),
                value: NewKey::ZSet {
                    member: v(),
                    score: 2.0,
                },
            },
            WriteOp::ClientKill { id: 7 },
            WriteOp::SlowlogConfig(SlowlogSetting::Threshold(10_000)),
            WriteOp::SlowlogConfig(SlowlogSetting::MaxLen(128)),
            WriteOp::SlowlogReset,
            WriteOp::XAutoClaim {
                key: k(),
                group: "g".into(),
                consumer: "c".into(),
                min_idle_ms: 60_000,
                count: 100,
            },
            WriteOp::XAck {
                key: k(),
                group: "g".into(),
                id: "1-0".into(),
            },
            WriteOp::XTrim {
                key: k(),
                trim: Trim::MaxLen(1000),
            },
            WriteOp::XTrim {
                key: k(),
                trim: Trim::MinId("1-0".into()),
            },
            WriteOp::Publish {
                channel: "ch".into(),
                message: v(),
            },
            WriteOp::FtCreate {
                index: "idx".into(),
                on: IndexOn::Hash,
                prefixes: vec!["user:".into()],
                fields: vec![IndexField {
                    name: "name".into(),
                    alias: None,
                    kind: "TEXT".into(),
                }],
            },
            WriteOp::FtDropIndex {
                index: "idx".into(),
            },
            WriteOp::Bulk {
                pattern: "user:{id}".into(),
                glob: "user:*".into(),
                action: BulkAction::Delete,
            },
            WriteOp::Bulk {
                pattern: "user:{id}".into(),
                glob: "user:*".into(),
                action: BulkAction::Expire { seconds: 86_400 },
            },
            WriteOp::Command {
                args: Args::new(vec![b"SET".to_vec(), b"a".to_vec(), b"1".to_vec()]),
                info: CommandInfo {
                    name: "set".into(),
                    flags: vec!["write".into()],
                    categories: vec!["@write".into()],
                    subcommands: vec![],
                },
            },
        ]
    }

    #[test]
    fn no_permit_on_a_read_only_profile_whatever_the_answer() {
        let answers = [
            Answer::None,
            Answer::Yes,
            Answer::Typed("user:1".into()),
            Answer::Typed("p1".into()),
            Answer::Typed("user:{id}".into()),
        ];
        for p in [
            profile(Env::Prod, None),
            profile(Env::Prod, Some(true)),
            profile(Env::Local, Some(true)),
            profile(Env::Stage, Some(true)),
        ] {
            for op in every_op() {
                for a in &answers {
                    let r = Permit::grant(op.clone(), &p, &SKIP, &Refusals::default(), a);
                    assert!(r.is_err(), "{op:?} on {p:?} with {a:?}");
                }
            }
        }
    }

    #[test]
    fn a_permit_needs_the_answer_the_decision_asks_for() {
        let prod = profile(Env::Prod, Some(false));
        let local = profile(Env::Local, None);
        let none = Refusals::default();
        for op in every_op() {
            for p in [&prod, &local] {
                let d = decide_op(&op, p, &ON, &none);
                let grant = |a: &Answer| Permit::grant(op.clone(), p, &ON, &none, a).is_ok();
                match d {
                    Decision::Run => assert!(grant(&Answer::None), "{op:?}"),
                    Decision::Ask => {
                        assert!(!grant(&Answer::None), "{op:?}");
                        assert!(grant(&Answer::Yes), "{op:?}");
                    }
                    Decision::Type(want) => {
                        assert!(!grant(&Answer::None), "{op:?}");
                        assert!(!grant(&Answer::Yes), "{op:?}");
                        assert!(!grant(&Answer::Typed(format!("{want}x"))), "{op:?}");
                        assert!(grant(&Answer::Typed(want)), "{op:?}");
                    }
                    Decision::Refuse(why) => panic!("{op:?} refused: {why}"),
                }
            }
        }
    }

    #[test]
    fn a_refused_command_is_off() {
        let local = profile(Env::Local, None);
        let mut refusals = Refusals::default();
        refusals.add("hset");
        let op = WriteOp::HSet {
            key: "k".into(),
            field: Raw::from("f"),
            value: Raw::from("v"),
        };
        assert_eq!(
            Permit::grant(op, &local, &ON, &refusals, &Answer::None).unwrap_err(),
            "Not sent: the server refused HSET for this user."
        );
    }

    #[test]
    fn kinds_and_typed_names() {
        let prod = profile(Env::Prod, Some(false));
        let ops = every_op();
        let unlink = ops
            .iter()
            .find(|o| matches!(o, WriteOp::Unlink { .. }))
            .unwrap();
        assert_eq!(
            decide_op(unlink, &prod, &ON, &Refusals::default()),
            Decision::Type("user:1".into())
        );
        let kill = WriteOp::ClientKill { id: 7 };
        assert_eq!(kill.kind(), Kind::Dangerous);
        assert_eq!(
            decide_op(&kill, &prod, &ON, &Refusals::default()),
            Decision::Type("p1".into())
        );
        let drop = WriteOp::FtDropIndex {
            index: "idx".into(),
        };
        assert_eq!(drop.kind(), Kind::Destructive);
        assert_eq!(
            drop.commands(),
            vec![vec![b"FT.DROPINDEX".to_vec(), b"idx".to_vec()]]
        );
        let rename = WriteOp::Rename {
            key: "a".into(),
            new: "b".into(),
        };
        assert_eq!(rename.kind(), Kind::Edit);
        assert_eq!(rename.command(), "RENAMENX");
    }

    #[test]
    fn typed_commands_are_judged_by_their_categories_and_the_list() {
        let info = |name: &str, cats: &[&str]| CommandInfo {
            name: name.into(),
            flags: vec!["write".into()],
            categories: cats.iter().map(|c| c.to_string()).collect(),
            subcommands: vec![],
        };
        let args = |s: &str| {
            s.split(' ')
                .map(|w| w.as_bytes().to_vec())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            command_kind(&args("SET a 1"), &info("set", &["@write"])),
            Kind::Edit
        );
        assert_eq!(
            command_kind(&args("DEL a"), &info("del", &["@write", "@keyspace"])),
            Kind::Destructive
        );
        assert_eq!(
            command_kind(&args("RENAME a b"), &info("rename", &["@write"])),
            Kind::Destructive
        );
        assert_eq!(
            command_kind(
                &args("FLUSHDB"),
                &info("flushdb", &["@write", "@dangerous"])
            ),
            Kind::Dangerous
        );
        // Commands that delete keys or elements, or overwrite a destination.
        for line in [
            "LTRIM l 0 1",
            "LPOP l",
            "SPOP s",
            "ZPOPMIN z",
            "ZREMRANGEBYSCORE z 0 1",
            "GETDEL k",
            "SINTERSTORE d a b",
            "ZRANGESTORE d z 0 -1",
            "SORT l STORE d",
            "EXPIRE k 0",
            "EXPIRE k -5",
            "PEXPIRE k soon",
            "EXPIREAT k 1",
            "GETEX k PXAT 1",
            "GETEX k EX 0",
            "SET k v EXAT 1",
            "HEXPIRE h 0 FIELDS 1 f",
            "HPEXPIREAT h 1 FIELDS 1 f",
            "XGROUP DESTROY s g",
            "xgroup delconsumer s g c",
            "VREM v e",
        ] {
            let name = line.split(' ').next().unwrap().to_ascii_lowercase();
            assert_eq!(
                command_kind(&args(line), &info(&name, &["@write"])),
                Kind::Destructive,
                "{line}"
            );
        }
        for line in [
            "EXPIRE k 60",
            "SORT l",
            "LPUSH l a",
            "GETEX k EX 60",
            "SET k v EX 60",
            "HEXPIRE h 60 FIELDS 1 f",
            "XGROUP CREATE s g $",
        ] {
            let name = line.split(' ').next().unwrap().to_ascii_lowercase();
            assert_eq!(
                command_kind(&args(line), &info(&name, &["@write"])),
                Kind::Edit,
                "{line}"
            );
        }
        // On prod the key is typed for one key; the profile's name for several or none.
        let prod = profile(Env::Prod, Some(false));
        let del = |line: &str| WriteOp::Command {
            args: Args::new(args(line)),
            info: info("del", &["@write", "@keyspace"]),
        };
        let none = Refusals::default();
        assert_eq!(
            decide_op(&del("DEL a"), &prod, &ON, &none),
            Decision::Type("a".into())
        );
        assert_eq!(
            decide_op(&del("DEL a b c"), &prod, &ON, &none),
            Decision::Type("p1".into())
        );
        let empty = WriteOp::Command {
            args: Args::new(vec![b"DEL".to_vec(), Vec::new()]),
            info: info("del", &["@write", "@keyspace"]),
        };
        assert_eq!(
            decide_op(&empty, &prod, &ON, &none),
            Decision::Type("p1".into())
        );
        let config = CommandInfo {
            subcommands: vec![info("config|set", &["@admin", "@dangerous"])],
            ..info("config", &["@slow"])
        };
        assert_eq!(
            command_kind(&args("CONFIG SET maxmemory 1"), &config),
            Kind::Dangerous
        );
        let op = WriteOp::Command {
            args: Args::new(args("CONFIG SET maxmemory 1")),
            info: config,
        };
        assert_eq!(op.command(), "CONFIG SET");
    }

    #[test]
    fn the_prompt_line_masks_credentials_and_escapes() {
        let config = CommandInfo {
            name: "config".into(),
            flags: vec![],
            categories: vec![],
            subcommands: vec![CommandInfo {
                name: "config|set".into(),
                flags: vec![],
                categories: vec!["@dangerous".into()],
                subcommands: vec![],
            }],
        };
        let op = WriteOp::Command {
            args: Args::new(vec![
                b"CONFIG".to_vec(),
                b"SET".to_vec(),
                b"requirepass".to_vec(),
                b"hunter2".to_vec(),
            ]),
            info: config,
        };
        let shown = op.shown();
        assert!(!shown.contains("hunter2"), "{shown}");
        let op = WriteOp::HSet {
            key: "k\x1b[2J".into(),
            field: Raw::from("f"),
            value: Raw::from("a\nb"),
        };
        let shown = op.shown();
        assert!(!shown.contains('\x1b') && !shown.contains('\n'), "{shown}");
    }

    #[test]
    fn debug_shows_no_value() {
        let op = WriteOp::HSet {
            key: "k".into(),
            field: Raw::from("f"),
            value: Raw::from("secret-value"),
        };
        let p = Permit::for_test(op.clone());
        assert!(!format!("{op:?}").contains("secret-value"));
        assert_eq!(format!("{p:?}"), "Permit(HSET)");
    }
}
