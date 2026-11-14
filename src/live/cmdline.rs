//! Live requests for the command line panel (spec "Command line panel"). Owner: part 4.
//!
//! - `COMMAND INFO <name>`: a command's flags and ACL categories, read before the
//!   command is sent, and its subcommands (Redis 7 and Valkey list them there).
//! - `COMMAND DOCS <name>`: the helper (Redis 7.0 and later, and Valkey). An older
//!   server answers with an unknown subcommand: [`LiveError::Unsupported`].
//! - `COMMAND LIST`: the command names, for `tab`. Asked once a session.
//! - The typed command itself, only when [`gate`] lets it through; for a repeat with
//!   pipeline mode on, up to [`MAX_REPEAT`] copies of it in one pipeline
//!   ([`Request::Batch`]), each copy checked by [`gate`] before any is sent, all of
//!   them taken from the budget at once. The panel reads the repeat count with
//!   [`parse_repeat`] and sizes pipelines with [`pipeline_batch`].
//!
//! **Views.** Each reply is drawn four ways when it comes back ([`Output::of`]): text
//! like `redis-cli` ([`format_reply`]), a table ([`format_table`]), JSON
//! ([`format_json`]) and RESP-like with types ([`format_raw`]), every one escaped and
//! cut at [`MAX_REPLY_BYTES`] the same way.
//!
//! **No writes.** Phase 2 sends only commands with the `readonly` flag, on any profile;
//! `@dangerous` ones (`KEYS`) only once the user confirmed. Commands that block
//! (`BLPOP`, `XREAD BLOCK`) and the ones [`command::refused`] names are never sent.
//! The panel decides with [`gate`] before it sends, and [`run`] checks again.
//!
//! **Values stay on screen.** The typed arguments are held as [`Args`] and a reply as
//! [`Output`], whose `Debug` print only how many there are. A reply is drawn like
//! `redis-cli` by [`format_reply`], every string escaped ([`value::escape`]), and cut at
//! [`MAX_REPLY_BYTES`]. Every command takes one from the budget.

use std::fmt;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use redis::{ErrorKind, RedisError, RedisResult, ServerErrorKind, Value};
use unicode_width::UnicodeWidthStr;

use super::command;
use super::server::{self, KeySpec};
use super::streams::{field, pairs, text};
use super::{LiveConn, LiveError, error_for, is_unknown_command, query};
use crate::budget::Budget;
use crate::value;

/// The most bytes of a reply drawn. Past it the reply is cut and says so.
pub const MAX_REPLY_BYTES: usize = 1024 * 1024;
/// The most characters of the server's error text kept.
pub const ERROR_CHARS: usize = 200;
/// What [`run`] says when a command reaches it that [`gate`] does not let through.
pub const NOT_SENT: &str = "not sent: the command line only sends readonly commands";

/// What the command line was given, word by word (`args[0]` is the command). Values:
/// `Debug` shows only how many words there are.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Args(Vec<Vec<u8>>);

impl Args {
    pub fn new(args: Vec<Vec<u8>>) -> Self {
        Self(args)
    }

    pub fn as_slice(&self) -> &[Vec<u8>] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Args(<{} words>)", self.0.len())
    }
}

#[derive(Clone, PartialEq)]
pub enum Request {
    /// `COMMAND INFO <name>`. `name` is the first word as typed, lower-case.
    Info { name: String },
    /// `COMMAND DOCS <name>`.
    Docs { name: String },
    /// `COMMAND LIST`.
    Names,
    /// The command, when [`gate`] says [`Gate::Send`] for `args` and `info`, or
    /// [`Gate::Ask`] and the user `confirmed`.
    Run {
        args: Args,
        info: CommandInfo,
        confirmed: bool,
    },
    /// `times` copies of the command in one pipeline, for a repeat with pipeline mode
    /// on: each one checked by [`gate`] as [`Request::Run`] is, before any is sent, and
    /// all of them taken from the budget at once. 1 to [`MAX_REPEAT`].
    Batch {
        args: Args,
        info: CommandInfo,
        confirmed: bool,
        times: u32,
    },
}

/// The typed name is the user's text: `Debug` leaves it out.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Request::Info { .. } => f.write_str("Info { .. }"),
            Request::Docs { .. } => f.write_str("Docs { .. }"),
            Request::Names => f.write_str("Names"),
            Request::Run {
                args, confirmed, ..
            } => f
                .debug_struct("Run")
                .field("args", args)
                .field("confirmed", confirmed)
                .finish_non_exhaustive(),
            Request::Batch {
                args,
                confirmed,
                times,
                ..
            } => f
                .debug_struct("Batch")
                .field("args", args)
                .field("confirmed", confirmed)
                .field("times", times)
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// `None`: the server has no such command.
    Info(Option<CommandInfo>),
    /// `None`: the server has no docs for the name.
    Docs(Option<Docs>),
    /// Lower-case, sorted.
    Names(Vec<String>),
    Ran(Output),
    /// A [`Request::Batch`]'s replies, in order: one per command.
    Batch(Vec<Output>),
}

/// What `COMMAND INFO` says about a command: the server's own names, lower-case.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandInfo {
    /// `get`, or `config|get` for a subcommand.
    pub name: String,
    /// `readonly`, `blocking`, `admin` …
    pub flags: Vec<String>,
    /// `@read`, `@dangerous` …
    pub categories: Vec<String>,
    /// Redis 7 and Valkey: the subcommands of a container command such as `CONFIG`.
    pub subcommands: Vec<CommandInfo>,
    /// Where its keys are, for routing on a cluster: the key specs (Redis 7.0 and
    /// later, and Valkey), or one spec from the first/last/step key positions before.
    /// Empty for a keyless command, and for a container (its subcommands hold theirs).
    pub keys: Vec<KeySpec>,
    /// Some keys may not be where `keys` says: the `movablekeys` flag, or a key spec
    /// redis-lens can't read. Finding them would need `COMMAND GETKEYS`, never sent.
    pub keys_partial: bool,
}

impl CommandInfo {
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    pub fn in_category(&self, category: &str) -> bool {
        self.categories.iter().any(|c| c == category)
    }

    /// The name as a user writes it: `CONFIG GET`.
    pub fn display(&self) -> String {
        self.name.replace('|', " ").to_ascii_uppercase()
    }

    /// The subcommand `sub` (as typed), if the server lists it.
    pub fn subcommand(&self, sub: &[u8]) -> Option<&CommandInfo> {
        let sub = String::from_utf8_lossy(sub).to_ascii_lowercase();
        let want = format!("{}|{sub}", self.name);
        self.subcommands.iter().find(|s| s.name == want)
    }
}

/// The keys of a typed command, as its key specs find them (spec "Cluster live views
/// (P4)", Command line).
#[derive(Clone, PartialEq, Eq)]
pub enum Keys {
    /// The command takes no keys.
    None,
    /// It takes keys, but none was found where its key specs say
    /// ([`CommandInfo::keys_partial`]).
    Unfound,
    /// The keys, in the order of the arguments.
    Found(Vec<Vec<u8>>),
}

/// Key names are the user's: `Debug` prints how many there are.
impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Keys::None => f.write_str("None"),
            Keys::Unfound => f.write_str("Unfound"),
            Keys::Found(k) => write!(f, "Found({} keys)", k.len()),
        }
    }
}

/// The positions of keys that a command's key specs can miss, for the commands that
/// write to a destination key: the keys after the `numkeys` of `ZUNIONSTORE`,
/// `ZINTERSTORE` and `ZDIFFSTORE`, and the `STORE` / `STOREDIST` destination of `SORT`,
/// `GEORADIUS` and `GEORADIUSBYMEMBER`. Other commands: none.
fn extra_key_positions(name: &str, args: &[Vec<u8>]) -> Vec<usize> {
    let word = |i: usize| {
        args.get(i)
            .map(|a| String::from_utf8_lossy(a).to_ascii_lowercase())
    };
    let sort = name == "sort";
    let mut out = Vec::new();
    match name {
        "zunionstore" | "zinterstore" | "zdiffstore" => {
            let n: usize = word(2).and_then(|n| n.parse().ok()).unwrap_or(0);
            out.extend((3..3usize.saturating_add(n)).take(args.len()));
        }
        "sort" | "georadius" | "georadiusbymember" => {
            // Options start after the fixed arguments; skip each one's operands.
            let mut i = match name {
                "sort" => 2,
                "georadius" => 6,
                _ => 5,
            };
            while let Some(w) = word(i) {
                match w.as_str() {
                    "store" => {
                        out.push(i + 1);
                        i += 2;
                    }
                    "storedist" if !sort => {
                        out.push(i + 1);
                        i += 2;
                    }
                    "limit" if sort => i += 3,
                    "by" | "get" if sort => i += 2,
                    "count" if !sort => i += 2,
                    _ => i += 1,
                }
            }
        }
        _ => {}
    }
    out.retain(|i| *i < args.len());
    out
}

/// The keys of `args` (`args[0]` is the command), from the key specs of `info`, or of
/// the subcommand typed when `info` is a container.
pub fn command_keys(args: &[Vec<u8>], info: &CommandInfo) -> Keys {
    let info = args
        .get(1)
        .filter(|_| !info.subcommands.is_empty())
        .and_then(|s| info.subcommand(s))
        .unwrap_or(info);
    let mut at = super::server::key_positions(&info.keys, args);
    if info.keys_partial {
        // The specs may find only some keys (Redis 6.2's first/last/step, and SORT
        // on every version): look for the rest where the command puts them.
        at.extend(extra_key_positions(&info.name, args));
        at.sort_unstable();
        at.dedup();
    }
    let found: Vec<Vec<u8>> = at
        .into_iter()
        .filter_map(|i| args.get(i).cloned())
        .collect();
    if !found.is_empty() {
        Keys::Found(found)
    } else if info.keys.is_empty() && !info.keys_partial {
        Keys::None
    } else {
        Keys::Unfound
    }
}

/// What the command line does with a typed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// A readonly command: sent as it is.
    Send,
    /// A readonly `@dangerous` command (`KEYS`): sent only once the user confirms.
    Ask,
    /// A command that writes: sent only through the write gate (spec "Write
    /// permissions"), as [`crate::write::WriteOp::Command`].
    Write,
    /// Not sent. Holds why, in redis-lens's own words, safe to draw.
    Refuse(String),
}

/// The first word as typed, upper-case and quoted when it needs to be: safe to draw.
pub fn typed_name(args: &[Vec<u8>]) -> String {
    let mut out = String::new();
    if let Some(a) = args.first() {
        command::push_word(&a.to_ascii_uppercase(), &mut out);
    }
    out
}

/// The command line's one decision (spec "Command line panel", "Write permissions"):
/// whether `args` is sent, given what `COMMAND INFO` said about its name (`None`: the
/// server has no such command). In order:
///
/// 1. the commands [`command::refused`] names, with its pointer;
/// 2. an unknown command or subcommand;
/// 3. a command that blocks (the `blocking` flag, or `@blocking` before Redis 7):
///    `XREAD` only with `BLOCK`;
/// 4. scripts and functions, module internals, module commands not known to only read,
///    and containers whose subcommands' flags the server doesn't list, on every
///    profile, writes on or off;
/// 5. a command without the `readonly` flag is a write: [`Gate::Write`], which only
///    the write gate sends;
/// 6. a readonly `@dangerous` command asks first;
/// 7. everything else is sent.
pub fn gate(args: &[Vec<u8>], info: Option<&CommandInfo>) -> Gate {
    let Some(first) = args.first() else {
        return Gate::Refuse("Nothing to send.".into());
    };
    if let Some(why) = command::refused(args) {
        return Gate::Refuse(why.into());
    }
    let Some(info) = info else {
        return Gate::Refuse(format!(
            "Not sent: the server has no command {}.",
            typed_name(args)
        ));
    };
    if !info
        .name
        .as_bytes()
        .eq_ignore_ascii_case(&first.to_ascii_lowercase())
    {
        return Gate::Refuse(NOT_SENT.into());
    }
    // A script or function runs commands of its own, which this gate never sees:
    // `EVAL_RO` is flagged readonly but could reset the slow log, run KEYS without the
    // question, or loop until someone sends SCRIPT KILL.
    // Judged by the command's own name: `config` for `config|get`.
    let base = info.name.split('|').next().unwrap_or("");
    if info.in_category("@scripting") || SCRIPTING.contains(&base) {
        return Gate::Refuse(format!(
            "Not sent: {} runs a script, whose commands the command line can't check.",
            info.display()
        ));
    }
    // Module internals (`_FT.DEBUG`) are flagged readonly but change state or hang.
    if base.starts_with('_') {
        return Gate::Refuse(format!(
            "Not sent: {} is a module's internal command.",
            info.display()
        ));
    }
    // Module commands (`FT.CONFIG`, `FT.ALIASADD`, `GRAPH.CONFIG` …) are often flagged
    // readonly though they change state, and list no subcommands: only the ones known
    // to read are sent.
    if base.contains('.') && !MODULE_READS.contains(&base) {
        return Gate::Refuse(format!(
            "Not sent: {} is a module command the command line doesn't know to only read.",
            info.display()
        ));
    }
    let cmd = if info.subcommands.is_empty() {
        // Before Redis 7.0 a container command such as MEMORY lists no subcommands and
        // its flags cover them all (6.2 flags MEMORY readonly, MEMORY PURGE included):
        // only the subcommands known to read are sent.
        if let Some(subs) = OLD_READONLY_SUBCOMMANDS
            .iter()
            .find(|(c, _)| *c == info.name)
            .map(|(_, subs)| *subs)
        {
            let sub = args
                .get(1)
                .map(|s| String::from_utf8_lossy(s).to_ascii_lowercase());
            if !sub.as_deref().is_some_and(|s| subs.contains(&s)) {
                let mut name = info.display();
                if let Some(s) = args.get(1) {
                    name.push(' ');
                    command::push_word(&s.to_ascii_uppercase(), &mut name);
                }
                return Gate::Refuse(format!(
                    "Not sent: this server doesn't list {name}'s flags, and it is not one \
                     of the subcommands known to only read ({}).",
                    subs.join(", ").to_ascii_uppercase()
                ));
            }
        } else if CONTAINERS.contains(&info.name.as_str()) {
            return Gate::Refuse(format!(
                "Not sent: this server doesn't list the flags of {}'s subcommands.",
                info.display()
            ));
        }
        info
    } else {
        let Some(sub) = args.get(1) else {
            return Gate::Refuse(format!(
                "Not sent: {} needs a subcommand ({} HELP lists them).",
                info.display(),
                info.display()
            ));
        };
        match info.subcommand(sub) {
            Some(s) => s,
            None => {
                let mut name = info.display();
                name.push(' ');
                command::push_word(&sub.to_ascii_uppercase(), &mut name);
                return Gate::Refuse(format!("Not sent: the server has no command {name}."));
            }
        }
    };
    if blocks(args, cmd) {
        return Gate::Refuse(format!(
            "Not sent: {} can block, and would hold the connection every view shares.",
            cmd.display()
        ));
    }
    if !cmd.has_flag("readonly") {
        return Gate::Write;
    }
    // Servers before 6.0 have no ACL categories: KEYS asks there too.
    if cmd.in_category("@dangerous")
        || matches!(cmd.name.as_str(), "keys" | "sort_ro" | "ft.tagvals")
    {
        return Gate::Ask;
    }
    Gate::Send
}

/// Module commands known to only read (the rest are refused, see [`gate`]): RedisJSON,
/// RediSearch, TimeSeries, the probabilistic types and RedisGraph's read-only query.
const MODULE_READS: &[&str] = &[
    "json.get",
    "json.mget",
    "json.type",
    "json.strlen",
    "json.arrlen",
    "json.arrindex",
    "json.objkeys",
    "json.objlen",
    "json.resp",
    "ft.search",
    "ft.aggregate",
    "ft.info",
    "ft.explain",
    "ft.explaincli",
    "ft.profile",
    "ft._list",
    "ft.tagvals",
    "ft.spellcheck",
    "ft.syndump",
    "ft.dictdump",
    "ft.sugget",
    "ft.suglen",
    "ts.get",
    "ts.mget",
    "ts.range",
    "ts.revrange",
    "ts.mrange",
    "ts.mrevrange",
    "ts.info",
    "ts.queryindex",
    "bf.exists",
    "bf.mexists",
    "bf.info",
    "bf.card",
    "cf.exists",
    "cf.mexists",
    "cf.count",
    "cf.info",
    "cms.query",
    "cms.info",
    "topk.query",
    "topk.list",
    "topk.info",
    "tdigest.min",
    "tdigest.max",
    "tdigest.quantile",
    "tdigest.cdf",
    "tdigest.rank",
    "tdigest.revrank",
    "tdigest.byrank",
    "tdigest.byrevrank",
    "tdigest.trimmed_mean",
    "tdigest.info",
    "graph.ro_query",
    "graph.explain",
    "graph.list",
];

/// Scripts and functions, refused whatever their flags (see [`gate`]).
const SCRIPTING: &[&str] = &[
    "eval",
    "eval_ro",
    "evalsha",
    "evalsha_ro",
    "fcall",
    "fcall_ro",
    "script",
    "function",
];

/// Container commands whose subcommands a server before 7.0 doesn't list, with the
/// subcommands known to only read. Other containers there are refused.
const OLD_READONLY_SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "memory",
        &["usage", "stats", "doctor", "malloc-stats", "help"],
    ),
    (
        "object",
        &["encoding", "freq", "idletime", "refcount", "help"],
    ),
    ("xinfo", &["stream", "groups", "consumers", "help"]),
];

/// Container commands (they have subcommands from Redis 7.0).
const CONTAINERS: &[&str] = &[
    "acl", "client", "cluster", "command", "config", "debug", "function", "latency", "module",
    "pubsub", "script", "slowlog", "xgroup", "sentinel",
];

/// Whether `cmd` would block. `XREAD` has the flag but blocks only with `BLOCK`.
fn blocks(args: &[Vec<u8>], cmd: &CommandInfo) -> bool {
    if !cmd.has_flag("blocking") && !cmd.in_category("@blocking") {
        return false;
    }
    if cmd.name == "xread" {
        return args
            .iter()
            .skip(1)
            .any(|a| a.eq_ignore_ascii_case(b"block"));
    }
    true
}

/// What `COMMAND DOCS` says about a command, every string escaped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Docs {
    pub summary: String,
    pub since: String,
    pub complexity: String,
    /// The arguments as one line: `key [NX | XX] [GET]`.
    pub arguments: String,
    /// By the server's name, `config|get`.
    pub subcommands: Vec<(String, Docs)>,
}

impl Docs {
    /// The docs of the subcommand `sub` of `name`, both as typed.
    pub fn subcommand(&self, name: &str, sub: &[u8]) -> Option<&Docs> {
        let want = format!(
            "{}|{}",
            name.to_ascii_lowercase(),
            String::from_utf8_lossy(sub).to_ascii_lowercase()
        );
        self.subcommands
            .iter()
            .find(|(n, _)| *n == want)
            .map(|(_, d)| d)
    }
}

/// How the panel draws replies (spec "Command line panel", result views). Every view
/// escapes every string and stops at [`MAX_REPLY_BYTES`], like the text view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Like `redis-cli`: [`format_reply`].
    #[default]
    Text,
    /// Pairs as two columns, arrays of arrays as rows: [`format_table`].
    Table,
    /// The reply as JSON: [`format_json`].
    Json,
    /// RESP-like, each value with its type: [`format_raw`].
    Raw,
}

impl View {
    /// The view after this one: text, table, JSON, raw, then text again.
    pub fn next(self) -> Self {
        match self {
            View::Text => View::Table,
            View::Table => View::Json,
            View::Json => View::Raw,
            View::Raw => View::Text,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Text => "text",
            View::Table => "table",
            View::Json => "JSON",
            View::Raw => "raw",
        }
    }
}

/// A reply drawn in every [`View`]: lines safe to draw. They are values, so `Debug`
/// shows only how many there are.
#[derive(Clone, PartialEq, Default)]
pub struct Output {
    /// [`View::Text`].
    lines: Vec<String>,
    table: Vec<String>,
    /// [`View::Table`] for a command whose flat array holds pairs (`HGETALL`,
    /// `CONFIG GET`): only when the reply is a flat array of an even length.
    table_pairs: Option<Vec<String>>,
    json: Vec<String>,
    raw: Vec<String>,
    /// The reply was over [`MAX_REPLY_BYTES`]: the last line says so.
    pub cut: bool,
    /// The reply is the server's error.
    pub error: bool,
    /// The command's round trip, the wait for the budget not included.
    pub took: Duration,
}

impl Output {
    /// The text view's lines.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The lines of `view`. `pairs`: the command's flat array holds pairs
    /// ([`pairs_hint`]), so the table shows it as two columns.
    pub fn lines_in(&self, view: View, pairs: bool) -> &[String] {
        match view {
            View::Text => &self.lines,
            View::Table => match &self.table_pairs {
                Some(p) if pairs => p,
                _ => &self.table,
            },
            View::Json => &self.json,
            View::Raw => &self.raw,
        }
    }

    /// The bytes held, every view counted.
    pub fn bytes(&self) -> usize {
        let sum = |l: &[String]| l.iter().map(String::len).sum::<usize>();
        sum(&self.lines)
            + sum(&self.table)
            + self.table_pairs.as_deref().map_or(0, sum)
            + sum(&self.json)
            + sum(&self.raw)
    }

    /// One error line (from [`run_error`]), as the reply: the same in every view.
    pub fn error_line(line: String, took: Duration) -> Self {
        Self {
            lines: vec![line.clone()],
            table: vec![line.clone()],
            table_pairs: None,
            json: vec![line.clone()],
            raw: vec![line],
            cut: false,
            error: true,
            took,
        }
    }

    /// `v` formatted for every view.
    pub fn of(v: &Value, took: Duration) -> Self {
        let (lines, cut) = format_reply(v);
        let flat_even = items(v).is_some_and(|i| {
            !i.is_empty() && i.len() % 2 == 0 && i.iter().all(|x| items(x).is_none())
        });
        Self {
            lines,
            table: format_table(v, false).0,
            table_pairs: flat_even.then(|| format_table(v, true).0),
            json: format_json(v).0,
            raw: format_raw(v).0,
            cut,
            error: matches!(v, Value::ServerError(_)),
            took,
        }
    }
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Output(<{} lines>, cut: {}, error: {}, took: {:?})",
            self.lines.len(),
            self.cut,
            self.error,
            self.took
        )
    }
}

/// Commands whose flat array reply holds pairs, field then value: the table view shows
/// them as two columns. Those always, and the ones in [`PAIRS_WITH`] given that
/// option.
const PAIRS: &[&str] = &[
    "hgetall",
    "config|get",
    "memory|stats",
    "xinfo|stream",
    "ft.info",
    "ts.info",
    "bf.info",
    "cf.info",
    "cms.info",
    "topk.info",
    "tdigest.info",
];

/// Commands whose flat array holds pairs with the option given.
const PAIRS_WITH: &[(&str, &str)] = &[
    ("hrandfield", "withvalues"),
    ("zrandmember", "withscores"),
    ("zrange", "withscores"),
    ("zrangebyscore", "withscores"),
    ("zrevrange", "withscores"),
    ("zrevrangebyscore", "withscores"),
    ("zunion", "withscores"),
    ("zinter", "withscores"),
    ("zdiff", "withscores"),
];

/// Whether `args`' reply, when it is a flat array, holds pairs (`HGETALL`, `CONFIG
/// GET`, `ZRANGE … WITHSCORES`): the table view shows it as two columns.
pub fn pairs_hint(args: &[Vec<u8>]) -> bool {
    let word = |i: usize| {
        args.get(i)
            .map(|a| String::from_utf8_lossy(a).to_ascii_lowercase())
    };
    let Some(first) = word(0) else {
        return false;
    };
    if PAIRS.contains(&first.as_str()) {
        return true;
    }
    if let Some(sub) = word(1)
        && PAIRS.contains(&format!("{first}|{sub}").as_str())
    {
        return true;
    }
    PAIRS_WITH.iter().any(|(c, opt)| {
        *c == first
            && args
                .iter()
                .skip(1)
                .any(|a| a.eq_ignore_ascii_case(opt.as_bytes()))
    })
}

/// The most commands one `N cmd` line repeats (spec "Command line panel", repeat).
pub const MAX_REPEAT: u32 = 100;

/// Whether `w` reads as a repeat count: digits, perhaps signed. Such a first word is
/// never a command, so it is taken for a count, and a bad one is refused.
pub fn is_count_word(w: &[u8]) -> bool {
    let digits = w
        .strip_prefix(b"-")
        .or_else(|| w.strip_prefix(b"+"))
        .unwrap_or(w);
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

/// Words a line's leading count takes: 1 when its first word reads as a count and a
/// command follows, else 0. The command is the rest.
pub fn count_words(args: &[Vec<u8>]) -> usize {
    usize::from(args.len() > 1 && is_count_word(&args[0]))
}

/// The repeat count of a typed line, as `redis-cli` reads it: a leading integer before
/// the command (`5 INCR counter`). `Ok(None)` when there is none; `Ok(Some(n))` for 1
/// to [`MAX_REPEAT`]; otherwise why it is not sent, safe to draw. The command is
/// `&args[count_words(args)..]`.
pub fn parse_repeat(args: &[Vec<u8>]) -> Result<Option<u32>, String> {
    if count_words(args) == 0 {
        return Ok(None);
    }
    let w = &args[0];
    let bad = || {
        format!(
            "Not sent: a repeat count is a whole number from 1 to {MAX_REPEAT}, before the command."
        )
    };
    if !w.iter().all(u8::is_ascii_digit) {
        return Err(bad());
    }
    let digits = std::str::from_utf8(w).map_err(|_| bad())?;
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Err("Not sent: a repeat count of 0 sends nothing.".into());
    }
    match trimmed.parse::<u32>() {
        Ok(n) if n <= MAX_REPEAT => Ok(Some(n)),
        _ => Err(format!(
            "Not sent: at most {MAX_REPEAT} repeats a line; {} is more.",
            cut_chars(trimmed, 24)
        )),
    }
}

/// The line as the history and the output keep it: [`command::masked_line`] of the
/// command, after its repeat count if it has one.
pub fn masked_with_count(args: &[Vec<u8>]) -> String {
    let skip = count_words(args);
    let rest = command::masked_line(&args[skip..]);
    if skip == 0 {
        return rest;
    }
    let mut out = String::new();
    command::push_word(&args[0], &mut out);
    out.push(' ');
    out.push_str(&rest);
    out
}

/// How many of a repeat's `left` commands one pipeline holds: at most a tenth of a
/// second of the budget (`per_sec / 10`, at least 1), so `esc` stops a run between
/// batches and no batch is a burst bigger than that; without a limit, all of them.
pub fn pipeline_batch(per_sec: Option<u32>, left: u32) -> u32 {
    let cap = per_sec
        .filter(|r| *r > 0)
        .map_or(MAX_REPEAT, |r| (r / 10).max(1));
    left.min(cap).min(MAX_REPEAT)
}

/// Runs one request. Every command takes one from `budget`.
pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    match req {
        Request::Info { name } => {
            let mut c = redis::cmd("COMMAND");
            c.arg("INFO").arg(&name);
            let v: Value = query(conn, budget, "COMMAND INFO", &c).await?;
            Ok(Reply::Info(parse_info(&v)))
        }
        Request::Docs { name } => {
            let mut c = redis::cmd("COMMAND");
            c.arg("DOCS").arg(&name);
            let v: Value = query(conn, budget, "COMMAND DOCS", &c).await?;
            Ok(Reply::Docs(parse_docs(&v)))
        }
        Request::Names => {
            let mut c = redis::cmd("COMMAND");
            c.arg("LIST");
            let v: Value = query(conn, budget, "COMMAND LIST", &c).await?;
            Ok(Reply::Names(parse_names(&v)))
        }
        Request::Run {
            args,
            info,
            confirmed,
        } => {
            // The panel decided already; this is the second check, so a mistake there
            // still sends nothing that is not readonly.
            match gate(args.as_slice(), Some(&info)) {
                Gate::Send => {}
                Gate::Ask if confirmed => {}
                Gate::Ask | Gate::Write | Gate::Refuse(_) => {
                    return Err(LiveError::Failed(NOT_SENT.into()));
                }
            }
            let name = sent_name(args.as_slice(), &info);
            let mut c = redis::Cmd::new();
            for a in args.as_slice() {
                c.arg(a.as_slice());
            }
            budget.take(1).await;
            let start = Instant::now();
            let r: RedisResult<Value> = c.query_async(conn).await;
            let took = start.elapsed();
            match r {
                Ok(v) => Ok(Reply::Ran(Output::of(&v, took))),
                Err(e) => {
                    let line = run_error(&name, &e)?;
                    Ok(Reply::Ran(Output::error_line(line, took)))
                }
            }
        }
        Request::Batch {
            args,
            info,
            confirmed,
            times,
        } => {
            if times == 0 || times > MAX_REPEAT {
                return Err(LiveError::Failed(format!(
                    "not sent: a pipeline holds 1 to {MAX_REPEAT} commands"
                )));
            }
            // Each command is checked before the batch is sent, the same check as a
            // single one: one that is not let through sends none of them.
            for _ in 0..times {
                match gate(args.as_slice(), Some(&info)) {
                    Gate::Send => {}
                    Gate::Ask if confirmed => {}
                    Gate::Ask | Gate::Write | Gate::Refuse(_) => {
                        return Err(LiveError::Failed(NOT_SENT.into()));
                    }
                }
            }
            let name = sent_name(args.as_slice(), &info);
            let mut pipe = redis::pipe();
            pipe.ignore_errors();
            for _ in 0..times {
                let mut c = redis::Cmd::new();
                for a in args.as_slice() {
                    c.arg(a.as_slice());
                }
                pipe.add_command(c);
            }
            budget.take(times).await;
            let start = Instant::now();
            let r: RedisResult<Vec<Value>> = pipe.query_async(conn).await;
            let took = start.elapsed();
            let replies = r.map_err(|e| error_for(&name, &e))?;
            if replies.len() != times as usize {
                return Err(LiveError::Failed(
                    "the pipeline's replies came back incomplete".into(),
                ));
            }
            let mut out = Vec::with_capacity(replies.len());
            for v in &replies {
                out.push(match v {
                    // An error of one command: as a single command's would be. A
                    // NOPERM fails the batch, in redis-lens's words.
                    Value::ServerError(e) => {
                        let e = RedisError::from(e.clone());
                        Output::error_line(run_error(&name, &e)?, took)
                    }
                    v => Output::of(v, took),
                });
            }
            Ok(Reply::Batch(out))
        }
    }
}

/// The name a failure of the sent command is called: the server's, `CONFIG GET` for a
/// subcommand.
fn sent_name(args: &[Vec<u8>], info: &CommandInfo) -> String {
    args.get(1)
        .and_then(|s| info.subcommand(s))
        .unwrap_or(info)
        .display()
}

/// A failure of the typed command `name`. `NOPERM`, an unknown command, a timeout and a
/// dropped connection are redis-lens's own words ([`error_for`]); `NOPERM` never shows
/// the server's text, which names the user. Any other error from the server is about
/// the command the user typed, as in `redis-cli`: `(error) ERR syntax error`, escaped
/// and cut at [`ERROR_CHARS`] characters.
pub fn run_error(name: &str, e: &RedisError) -> Result<String, LiveError> {
    let plain = match e.kind() {
        ErrorKind::Server(ServerErrorKind::NoPerm) => false,
        ErrorKind::Server(_) | ErrorKind::Extension => true,
        _ => false,
    };
    if !plain || is_unknown_command(e) {
        return Err(error_for(name, e));
    }
    Ok(error_line(e.code().unwrap_or(""), e.detail().unwrap_or("")))
}

/// `(error) CODE detail`, escaped and cut.
fn error_line(code: &str, detail: &str) -> String {
    // A NOPERM inside a reply (a transaction's, a script's) names the user: fixed text.
    if code.eq_ignore_ascii_case("NOPERM") {
        return "(error) NOPERM the server refused this for this user".to_string();
    }
    let msg = value::escape(format!("{code} {detail}").trim());
    let msg = if msg.is_empty() {
        "the command failed".to_string()
    } else {
        cut_chars(&msg, ERROR_CHARS)
    };
    format!("(error) {msg}")
}

/// `s` cut to `max` characters, the last one `…` when it was cut.
fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The items of an array or a set.
fn items(v: &Value) -> Option<&[Value]> {
    match v {
        Value::Array(i) | Value::Set(i) => Some(i),
        _ => None,
    }
}

fn words(v: Option<&Value>) -> Vec<String> {
    v.and_then(items)
        .unwrap_or(&[])
        .iter()
        .filter_map(text)
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// One entry of `COMMAND INFO`: `[name, arity, flags, first, last, step, categories,
/// tips, key specs, subcommands]`. Categories came in Redis 6.0, subcommands in 7.0.
fn parse_entry(v: &Value) -> Option<CommandInfo> {
    let parts = items(v)?;
    let name = text(parts.first()?)?.to_ascii_lowercase();
    let flags = words(parts.get(2));
    // The key specs, or before Redis 7.0 the first/last/step triple.
    let keys = match server::parse_command_info(&Value::Array(parts.to_vec())) {
        server::CommandKeys::Specs(s) => s,
        server::CommandKeys::Container | server::CommandKeys::Unknown => Vec::new(),
    };
    let unread = parts
        .get(8)
        .and_then(items)
        .is_some_and(|specs| specs.iter().any(|s| server::parse_spec(s).is_none()));
    Some(CommandInfo {
        name,
        keys_partial: unread || flags.iter().any(|f| f == "movablekeys"),
        keys,
        flags,
        categories: words(parts.get(6)),
        subcommands: parts
            .get(9)
            .and_then(items)
            .unwrap_or(&[])
            .iter()
            .filter_map(parse_entry)
            .collect(),
    })
}

/// Reads the reply of `COMMAND INFO <name>`: one entry, or nil for an unknown command.
pub fn parse_info(v: &Value) -> Option<CommandInfo> {
    parse_entry(items(v)?.first()?)
}

/// Reads the reply of `COMMAND LIST`.
pub fn parse_names(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = items(v)
        .unwrap_or(&[])
        .iter()
        .filter_map(text)
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_graphic() && c != '"' && c != '\'' && c != '\\')
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Reads the reply of `COMMAND DOCS <name>`: `[name, docs]` (a map over RESP3). An
/// empty reply: no docs.
pub fn parse_docs(v: &Value) -> Option<Docs> {
    let (_, d) = pairs(v).into_iter().next()?;
    Some(docs_of(d, 0))
}

fn docs_of(v: &Value, depth: usize) -> Docs {
    let get = |k: &str| field(v, k).and_then(text).map(|t| value::escape(&t));
    Docs {
        summary: get("summary").unwrap_or_default(),
        since: get("since").unwrap_or_default(),
        complexity: get("complexity").unwrap_or_default(),
        arguments: field(v, "arguments")
            .and_then(items)
            .map(|a| {
                a.iter()
                    .map(|x| argument(x, 0))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        subcommands: if depth > 0 {
            Vec::new()
        } else {
            field(v, "subcommands")
                .map(pairs)
                .unwrap_or_default()
                .into_iter()
                .map(|(n, d)| (n.to_ascii_lowercase(), docs_of(d, depth + 1)))
                .collect()
        },
    }
}

/// One argument of `COMMAND DOCS` as the docs write it: `[EX seconds | PX ms]`.
fn argument(v: &Value, depth: usize) -> String {
    if depth > 8 {
        return "…".into();
    }
    let get = |k: &str| field(v, k).and_then(text).map(|t| value::escape(&t));
    let flags = words(field(v, "flags"));
    let ty = get("type").unwrap_or_default();
    let token = get("token");
    let children = || -> Vec<String> {
        field(v, "arguments")
            .and_then(items)
            .unwrap_or(&[])
            .iter()
            .map(|a| argument(a, depth + 1))
            .collect()
    };
    let mut body = match ty.as_str() {
        "pure-token" => token.clone().unwrap_or_default(),
        "oneof" => {
            let all = children().join(" | ");
            if flags.iter().any(|f| f == "optional") {
                all
            } else {
                format!("<{all}>")
            }
        }
        "block" => children().join(" "),
        _ => get("display_text")
            .or_else(|| get("name"))
            .unwrap_or_default(),
    };
    if ty != "pure-token"
        && let Some(t) = &token
    {
        body = format!("{t} {body}");
    }
    if flags
        .iter()
        .any(|f| f == "multiple" || f == "multiple_token")
    {
        body = format!("{body} [{body} ...]");
    }
    if flags.iter().any(|f| f == "optional") {
        body = format!("[{body}]");
    }
    body
}

/// Text that stops growing past [`MAX_REPLY_BYTES`].
struct Sink {
    out: String,
    cut: bool,
}

impl Sink {
    fn push(&mut self, s: &str) {
        if self.cut {
            return;
        }
        let room = MAX_REPLY_BYTES.saturating_sub(self.out.len());
        if s.len() <= room {
            self.out.push_str(s);
            return;
        }
        let mut end = room;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.out.push_str(&s[..end]);
        self.cut = true;
    }

    fn new() -> Self {
        Self {
            out: String::new(),
            cut: false,
        }
    }

    /// The lines, the last one [`CUT_NOTE`] when it was cut, and whether it was.
    fn finish(self) -> (Vec<String>, bool) {
        let mut lines: Vec<String> = self.out.split('\n').map(str::to_string).collect();
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if self.cut {
            lines.push(CUT_NOTE.to_string());
        }
        (lines, self.cut)
    }

    /// Removes the last newline, before ` => ` and a map's value.
    fn unline(&mut self) {
        if !self.cut && self.out.ends_with('\n') {
            self.out.pop();
        }
    }
}

/// What a cut reply ends with.
pub const CUT_NOTE: &str = "(cut: the reply is over 1 MiB, so the rest is not shown)";

/// Draws `v` the way `redis-cli` does on a terminal:
///
/// - `(integer) 5`, `(nil)`, `(double) 1.5`, `(true)`, `(big number) 1…`;
/// - strings quoted, `"a\nb"`; status replies bare, `OK`;
/// - arrays numbered `1) …`, sets `1~ …`, maps `1# "key" => value`, nested ones
///   indented under their number, empty ones `(empty array)`, `(empty set)`,
///   `(empty hash)`;
/// - errors `(error) ERR …`, escaped and cut at [`ERROR_CHARS`].
///
/// Every string goes through [`value::escape`] (and invalid UTF-8 shows as `\xNN`), so
/// each line is safe to draw. Past [`MAX_REPLY_BYTES`] the rest is left out and the last
/// line is [`CUT_NOTE`]; the second value says it was cut.
pub fn format_reply(v: &Value) -> (Vec<String>, bool) {
    let mut sink = Sink::new();
    tty(v, "", &mut sink, 0);
    sink.finish()
}

/// Draws `v` as a table (spec "Command line panel", result views), every cell escaped:
///
/// - a map, an array of two-item arrays, or with `pairs` ([`pairs_hint`]) a flat array
///   of an even length: `#`, `field` and `value` columns, one row a pair;
/// - an array of arrays: one row each, a column per item;
/// - another array or set: `#` and `value`;
/// - anything else as [`format_reply`] draws it.
///
/// A cell of an aggregate says what it holds (`(array of 3)`). Every column but the
/// last is cut at [`CELL_CHARS`] characters. Past [`MAX_REPLY_BYTES`] it stops like
/// [`format_reply`].
pub fn format_table(v: &Value, pairs: bool) -> (Vec<String>, bool) {
    let v = match v {
        Value::Attribute { data, .. } => data,
        v => v,
    };
    let mut sink = Sink::new();
    let rows: Option<(Vec<String>, Vec<Vec<String>>)> = match v {
        Value::Map(m) if !m.is_empty() => Some((
            vec!["#".into(), "field".into(), "value".into()],
            rows_of(m.iter().map(|(k, v)| vec![cell(k), cell(v)])),
        )),
        Value::Array(i) | Value::Set(i) | Value::Push { data: i, .. } if !i.is_empty() => {
            let inner: Option<Vec<&[Value]>> = i.iter().map(items).collect();
            match inner {
                Some(inner) if inner.iter().all(|r| r.len() == 2) => Some((
                    vec!["#".into(), "field".into(), "value".into()],
                    rows_of(inner.iter().map(|r| vec![cell(&r[0]), cell(&r[1])])),
                )),
                Some(inner) => {
                    let cols = inner.iter().map(|r| r.len()).max().unwrap_or(0);
                    let mut head = vec!["#".to_string()];
                    head.extend((1..=cols).map(|c| c.to_string()));
                    Some((
                        head,
                        rows_of(inner.iter().map(|r| r.iter().map(cell).collect())),
                    ))
                }
                None if pairs && i.len() % 2 == 0 => {
                    let (two, _) = i.as_chunks::<2>();
                    Some((
                        vec!["#".into(), "field".into(), "value".into()],
                        rows_of(two.iter().map(|[k, v]| vec![cell(k), cell(v)])),
                    ))
                }
                None => Some((
                    vec!["#".into(), "value".into()],
                    rows_of(i.iter().map(|x| vec![cell(x)])),
                )),
            }
        }
        _ => None,
    };
    let Some((head, rows)) = rows else {
        tty(v, "", &mut sink, 0);
        return sink.finish();
    };
    let cols = head.len();
    let mut widths: Vec<usize> = head.iter().map(|h| h.width()).collect();
    for r in &rows {
        for (c, text) in r.iter().enumerate() {
            if c + 1 < cols {
                widths[c] = widths[c].max(text.width());
            }
        }
    }
    let line = |cells: &[String], sink: &mut Sink| {
        let mut out = String::new();
        for (c, text) in cells.iter().enumerate() {
            if c > 0 {
                out.push_str("  ");
            }
            out.push_str(text);
            if c + 1 < cells.len() {
                out.push_str(&" ".repeat(widths[c].saturating_sub(text.width())));
            }
        }
        sink.push(out.trim_end());
        sink.push("\n");
    };
    line(&head, &mut sink);
    let mut cut = false;
    for r in &rows {
        if sink.cut {
            break;
        }
        if r.first().is_some_and(|c| c == ROWS_CUT) {
            cut = true;
            break;
        }
        line(r, &mut sink);
    }
    if cut {
        sink.cut = true;
    }
    sink.finish()
}

/// Characters a table cell keeps, but in the last column.
pub const CELL_CHARS: usize = 60;

/// Marks where [`rows_of`] stopped: the rows held the most a reply shows.
const ROWS_CUT: &str = "\u{0}cut";

/// The rows of a table, the row number first, each cell but the last cut at
/// [`CELL_CHARS`]. Stops past [`MAX_REPLY_BYTES`] of cells, with a [`ROWS_CUT`] row.
fn rows_of(rows: impl Iterator<Item = Vec<String>>) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for (i, r) in rows.enumerate() {
        if bytes > MAX_REPLY_BYTES {
            out.push(vec![ROWS_CUT.to_string()]);
            break;
        }
        let last = r.len().saturating_sub(1);
        let mut row = vec![(i + 1).to_string()];
        for (c, text) in r.into_iter().enumerate() {
            row.push(if c < last {
                cut_chars(&text, CELL_CHARS)
            } else {
                text
            });
        }
        bytes += row.iter().map(String::len).sum::<usize>() + 2 * row.len();
        out.push(row);
    }
    out
}

/// One value as a table cell, on one line and safe to draw: strings escaped (`\n` as
/// two characters) and not quoted, but the empty one as `""`; an aggregate as what it
/// holds.
fn cell(v: &Value) -> String {
    let count = |what: &str, n: usize| match n {
        0 => format!("(empty {what})"),
        n => format!("({what} of {n})"),
    };
    match v {
        Value::Nil => "(nil)".into(),
        Value::Int(i) => i.to_string(),
        Value::Double(d) => double(*d),
        Value::Boolean(b) => b.to_string(),
        Value::BigNumber(n) => escaped_bytes(n),
        Value::BulkString(b) if b.is_empty() => "\"\"".into(),
        Value::BulkString(b) => {
            let b = &b[..b.len().min(MAX_REPLY_BYTES)];
            escaped_bytes(b)
        }
        Value::SimpleString(s) => value::escape(s),
        Value::Okay => "OK".into(),
        Value::VerbatimString { text, .. } => value::escape(text),
        Value::ServerError(e) => error_line(e.code(), e.details().unwrap_or("")),
        Value::Array(i) => count("array", i.len()),
        Value::Set(i) => count("set", i.len()),
        Value::Push { data, .. } => count("push", data.len()),
        Value::Map(m) => count("map", m.len()),
        Value::Attribute { data, .. } => cell(data),
        _ => "(a reply redis-lens cannot show)".into(),
    }
}

/// Draws `v` as JSON, two spaces an indent (spec "Command line panel", result views):
/// arrays, sets and pushes as arrays; a map as an object when every key is a string,
/// otherwise as an array of `[key, value]` pairs; nil as `null`; integers and doubles
/// as numbers, but `nan` and `±inf` as strings; a big number as a string, so no
/// digit is lost; booleans as they are; a server error as `{"error": "…"}`. Strings
/// are JSON strings with every control and bidi character as `\uXXXX`, and invalid
/// UTF-8 as the text `\xNN`. Past [`MAX_REPLY_BYTES`] it stops like [`format_reply`].
pub fn format_json(v: &Value) -> (Vec<String>, bool) {
    let mut sink = Sink::new();
    json(v, "", &mut sink, 0);
    sink.push("\n");
    sink.finish()
}

fn json(v: &Value, indent: &str, out: &mut Sink, depth: usize) {
    if out.cut {
        return;
    }
    if depth > MAX_DEPTH {
        out.push("\"…\"");
        return;
    }
    let inner = format!("{indent}  ");
    match v {
        Value::Nil => out.push("null"),
        Value::Int(i) => out.push(&i.to_string()),
        Value::Double(d) if d.is_finite() => out.push(&d.to_string()),
        Value::Double(d) => out.push(&json_str(double(*d).as_bytes())),
        Value::Boolean(b) => out.push(if *b { "true" } else { "false" }),
        Value::BigNumber(n) => out.push(&json_str(n)),
        Value::BulkString(b) => {
            let room = MAX_REPLY_BYTES.saturating_sub(out.out.len());
            out.push(&json_str(&b[..b.len().min(room.saturating_add(8))]));
        }
        Value::SimpleString(s) => out.push(&json_str(s.as_bytes())),
        Value::Okay => out.push("\"OK\""),
        Value::VerbatimString { text, .. } => out.push(&json_str(text.as_bytes())),
        Value::ServerError(e) => {
            let line = error_line(e.code(), e.details().unwrap_or(""));
            let msg = line.strip_prefix("(error) ").unwrap_or(&line);
            out.push(&format!("{{\"error\": {}}}", json_str(msg.as_bytes())));
        }
        Value::Array(i) | Value::Set(i) | Value::Push { data: i, .. } => {
            json_list(i.iter(), i.len(), indent, &inner, out, |x, out| {
                json(x, &inner, out, depth + 1);
            });
        }
        Value::Map(m) => {
            let keyed = m.iter().all(|(k, _)| {
                matches!(
                    k,
                    Value::BulkString(_) | Value::SimpleString(_) | Value::VerbatimString { .. }
                )
            });
            if m.is_empty() {
                out.push("{}");
            } else if keyed {
                out.push("{\n");
                for (n, (k, val)) in m.iter().enumerate() {
                    if out.cut {
                        return;
                    }
                    out.push(&inner);
                    let key = match k {
                        Value::BulkString(b) => json_str(b),
                        Value::SimpleString(s) => json_str(s.as_bytes()),
                        Value::VerbatimString { text, .. } => json_str(text.as_bytes()),
                        _ => String::new(),
                    };
                    out.push(&key);
                    out.push(": ");
                    json(val, &inner, out, depth + 1);
                    out.push(if n + 1 < m.len() { ",\n" } else { "\n" });
                }
                out.push(indent);
                out.push("}");
            } else {
                let pair_indent = format!("{inner}  ");
                json_list(m.iter(), m.len(), indent, &inner, out, |(k, val), out| {
                    out.push("[\n");
                    out.push(&pair_indent);
                    json(k, &pair_indent, out, depth + 2);
                    out.push(",\n");
                    out.push(&pair_indent);
                    json(val, &pair_indent, out, depth + 2);
                    out.push("\n");
                    out.push(&inner);
                    out.push("]");
                });
            }
        }
        Value::Attribute { data, .. } => json(data, indent, out, depth),
        _ => out.push("\"(a reply redis-lens cannot show)\""),
    }
}

/// `[` the items, one a line, `]`; `[]` when there are none.
fn json_list<T>(
    items: impl Iterator<Item = T>,
    len: usize,
    indent: &str,
    inner: &str,
    out: &mut Sink,
    mut each: impl FnMut(T, &mut Sink),
) {
    if len == 0 {
        out.push("[]");
        return;
    }
    out.push("[\n");
    for (n, x) in items.enumerate() {
        if out.cut {
            return;
        }
        out.push(inner);
        each(x, out);
        out.push(if n + 1 < len { ",\n" } else { "\n" });
    }
    out.push(indent);
    out.push("]");
}

/// `b` as a JSON string, safe to draw: `\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\f`, every
/// other control and bidi character as `\uXXXX`, and invalid UTF-8 as the text `\xNN`.
fn json_str(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() + 2);
    out.push('"');
    for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                c if c.is_control() || value::is_bidi_control(c) => {
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                c => out.push(c),
            }
        }
        for x in chunk.invalid() {
            let _ = write!(out, "\\\\x{x:02x}");
        }
    }
    out.push('"');
    out
}

/// Draws `v` RESP-like, each value on a line of its own with its type first (spec
/// "Command line panel", result views): `[bulk string] $5 "hello"`, `[integer] :5`,
/// `[null] _`, `[simple string] +OK`, `[error] -ERR …`, `[double] ,1.5`, `[boolean]
/// #t`, `[big number] (1…`, `[verbatim string] =9 txt "…"`, and aggregates as `[array]
/// *2`, `[set] ~2`, `[map] %1` (its keys and values in turn), `[push] >2`, with their
/// items indented under them. Strings are quoted and escaped as in [`format_reply`];
/// the lengths are the server's, in bytes. Past [`MAX_REPLY_BYTES`] it stops like
/// [`format_reply`].
pub fn format_raw(v: &Value) -> (Vec<String>, bool) {
    let mut sink = Sink::new();
    raw(v, "", &mut sink, 0);
    sink.finish()
}

fn raw(v: &Value, indent: &str, out: &mut Sink, depth: usize) {
    if out.cut {
        return;
    }
    out.push(indent);
    if depth > MAX_DEPTH {
        out.push("…\n");
        return;
    }
    let inner = format!("{indent}  ");
    let list = |label: &str, mark: char, items: &[Value], out: &mut Sink| {
        out.push(&format!("[{label}] {mark}{}\n", items.len()));
        for x in items {
            if out.cut {
                return;
            }
            raw(x, &inner, out, depth + 1);
        }
    };
    match v {
        Value::Nil => out.push("[null] _\n"),
        Value::Int(i) => out.push(&format!("[integer] :{i}\n")),
        Value::Double(d) => out.push(&format!("[double] ,{}\n", double(*d))),
        Value::Boolean(b) => out.push(if *b {
            "[boolean] #t\n"
        } else {
            "[boolean] #f\n"
        }),
        Value::BigNumber(n) => out.push(&format!("[big number] ({}\n", escaped_bytes(n))),
        Value::BulkString(b) => {
            out.push(&format!("[bulk string] ${} ", b.len()));
            let room = MAX_REPLY_BYTES.saturating_sub(out.out.len());
            out.push(&quoted(b, room.saturating_add(8)));
            out.push("\n");
        }
        Value::SimpleString(s) => out.push(&format!("[simple string] +{}\n", value::escape(s))),
        Value::Okay => out.push("[simple string] +OK\n"),
        Value::VerbatimString { format, text } => {
            let f = match format {
                redis::VerbatimFormat::Text => "txt".to_string(),
                redis::VerbatimFormat::Markdown => "mkd".to_string(),
                redis::VerbatimFormat::Unknown(u) => value::escape(u),
                _ => "?".to_string(),
            };
            out.push(&format!("[verbatim string] ={} {f} ", text.len() + 4));
            let room = MAX_REPLY_BYTES.saturating_sub(out.out.len());
            out.push(&quoted(text.as_bytes(), room.saturating_add(8)));
            out.push("\n");
        }
        Value::ServerError(e) => {
            let line = error_line(e.code(), e.details().unwrap_or(""));
            let msg = line.strip_prefix("(error) ").unwrap_or(&line);
            out.push(&format!("[error] -{msg}\n"));
        }
        Value::Array(i) => list("array", '*', i, out),
        Value::Set(i) => list("set", '~', i, out),
        Value::Push { data, .. } => list("push", '>', data, out),
        Value::Map(m) => {
            out.push(&format!("[map] %{}\n", m.len()));
            for (k, val) in m {
                if out.cut {
                    return;
                }
                raw(k, &inner, out, depth + 1);
                raw(val, &inner, out, depth + 1);
            }
        }
        Value::Attribute { data, attributes } => {
            out.push(&format!("[attribute] |{}\n", attributes.len()));
            for (k, val) in attributes {
                if out.cut {
                    return;
                }
                raw(k, &inner, out, depth + 1);
                raw(val, &inner, out, depth + 1);
            }
            raw(data, indent, out, depth);
        }
        _ => out.push("[unknown] (a reply redis-lens cannot show)\n"),
    }
}

/// How deep a reply is drawn; deeper parts show as `…`.
const MAX_DEPTH: usize = 64;

fn tty(v: &Value, prefix: &str, out: &mut Sink, depth: usize) {
    if out.cut {
        return;
    }
    if depth > MAX_DEPTH {
        out.push("…\n");
        return;
    }
    match v {
        Value::Nil => out.push("(nil)\n"),
        Value::Int(i) => out.push(&format!("(integer) {i}\n")),
        Value::Double(d) => out.push(&format!("(double) {}\n", double(*d))),
        Value::Boolean(b) => out.push(if *b { "(true)\n" } else { "(false)\n" }),
        Value::BigNumber(n) => {
            out.push("(big number) ");
            out.push(&escaped_bytes(n));
            out.push("\n");
        }
        Value::BulkString(b) => {
            let room = MAX_REPLY_BYTES.saturating_sub(out.out.len());
            out.push(&quoted(b, room.saturating_add(8)));
            out.push("\n");
        }
        Value::SimpleString(s) => {
            out.push(&value::escape(s));
            out.push("\n");
        }
        Value::Okay => out.push("OK\n"),
        Value::VerbatimString { text, .. } => {
            let mut lines = text.split('\n').peekable();
            let mut first = true;
            while let Some(l) = lines.next() {
                if lines.peek().is_none() && l.is_empty() && !first {
                    break;
                }
                if !first {
                    out.push(prefix);
                }
                out.push(&value::escape(l.trim_end_matches('\r')));
                out.push("\n");
                first = false;
            }
        }
        Value::ServerError(e) => {
            out.push(&error_line(e.code(), e.details().unwrap_or("")));
            out.push("\n");
        }
        Value::Array(items) => aggregate(items, ')', "(empty array)", prefix, out, depth),
        Value::Set(items) => aggregate(items, '~', "(empty set)", prefix, out, depth),
        Value::Push { data, .. } => aggregate(data, ')', "(empty push)", prefix, out, depth),
        Value::Map(m) => {
            if m.is_empty() {
                out.push("(empty hash)\n");
                return;
            }
            let w = digits(m.len());
            let inner = format!("{prefix}{}", " ".repeat(w + 2));
            for (i, (k, val)) in m.iter().enumerate() {
                if out.cut {
                    return;
                }
                out.push(&format!(
                    "{}{:>w$}# ",
                    if i == 0 { "" } else { prefix },
                    i + 1
                ));
                tty(k, &inner, out, depth + 1);
                out.unline();
                out.push(" => ");
                tty(val, &inner, out, depth + 1);
            }
        }
        Value::Attribute { data, .. } => tty(data, prefix, out, depth),
        _ => out.push("(a reply redis-lens cannot show)\n"),
    }
}

fn aggregate(items: &[Value], sep: char, empty: &str, prefix: &str, out: &mut Sink, depth: usize) {
    if items.is_empty() {
        out.push(empty);
        out.push("\n");
        return;
    }
    let w = digits(items.len());
    let inner = format!("{prefix}{}", " ".repeat(w + 2));
    for (i, item) in items.iter().enumerate() {
        if out.cut {
            return;
        }
        out.push(&format!(
            "{}{:>w$}{sep} ",
            if i == 0 { "" } else { prefix },
            i + 1
        ));
        tty(item, &inner, out, depth + 1);
    }
}

fn digits(mut n: usize) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// A double as the server writes it: `inf`, `-inf`, `nan`, otherwise the shortest
/// form that reads back.
fn double(d: f64) -> String {
    if d.is_nan() {
        "nan".into()
    } else if d.is_infinite() {
        if d > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        d.to_string()
    }
}

/// Bytes as text that is safe to draw: invalid UTF-8 as `\xNN`.
fn escaped_bytes(b: &[u8]) -> String {
    let mut out = String::new();
    for chunk in b.utf8_chunks() {
        out.push_str(&value::escape(chunk.valid()));
        for x in chunk.invalid() {
            let _ = write!(out, "\\x{x:02x}");
        }
    }
    out
}

/// A string reply quoted like `redis-cli`: `"…"`, with `\"`, `\\`, `\n`, `\r`, `\t`,
/// `\a` and `\b`; other control and bidi characters as [`value::escape`] draws them,
/// and invalid UTF-8 as `\xNN`. Stops after about `max` bytes.
fn quoted(b: &[u8], max: usize) -> String {
    let mut out = String::with_capacity(b.len().min(max) + 2);
    out.push('"');
    let mut buf = [0u8; 4];
    'all: for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            if out.len() > max {
                break 'all;
            }
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{7}' => out.push_str("\\a"),
                '\u{8}' => out.push_str("\\b"),
                c if c.is_control() || value::is_bidi_control(c) => {
                    out.push_str(&value::escape(c.encode_utf8(&mut buf)));
                }
                c => out.push(c),
            }
        }
        for x in chunk.invalid() {
            let _ = write!(out, "\\x{x:02x}");
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn st(s: &str) -> Value {
        Value::SimpleString(s.into())
    }

    fn args(s: &str) -> Vec<Vec<u8>> {
        command::split(s).unwrap()
    }

    fn lines(v: &Value) -> Vec<String> {
        format_reply(v).0
    }

    /// A script or function runs commands the gate never sees: refused whatever its
    /// flags, with or without ACL categories (found by the phase 2 verifier: EVAL_RO
    /// reset the slow log and ran KEYS without the question).
    #[test]
    fn scripts_and_module_internals_are_never_sent() {
        let ro = &["readonly", "noscript", "stale"];
        for (line, name, cats) in [
            (
                "EVAL_RO \"return redis.call('slowlog','reset')\" 0",
                "eval_ro",
                &["@slow", "@scripting"][..],
            ),
            ("evalsha_ro abc 0", "evalsha_ro", &[][..]),
            ("FCALL_RO f 0", "fcall_ro", &["@scripting"][..]),
            ("_FT.DEBUG x", "_ft.debug", &[][..]),
            ("search.CLUSTERINFO", "search.clusterinfo", &[][..]),
        ] {
            let g = gate(&args(line), Some(&info(name, ro, cats)));
            assert!(matches!(g, Gate::Refuse(_)), "{line}: {g:?}");
        }
    }

    /// Module commands flagged readonly that change state (found on Redis Stack 6.2 and
    /// 7.2: FT.CONFIG SET, FT.ALIASADD, FT.DICTADD, GRAPH.CONFIG SET, FT.DEBUG) are not
    /// sent; known module reads are.
    #[test]
    fn only_known_module_reads_are_sent() {
        let ro = &["readonly"];
        for (line, name) in [
            ("FT.CONFIG SET MAXEXPANSIONS 7", "ft.config"),
            ("FT.ALIASADD a idx", "ft.aliasadd"),
            ("FT._ALIASADDIFNX a idx", "ft._aliasaddifnx"),
            ("FT.DICTADD d w", "ft.dictadd"),
            ("FT.DEBUG GC_FORCEINVOKE idx", "ft.debug"),
            ("FT.CURSOR DEL idx 1", "ft.cursor"),
            ("GRAPH.CONFIG SET RESULTSET_SIZE 5", "graph.config"),
            ("CF.COMPACT k", "cf.compact"),
            ("search.CLUSTERINFO", "search.clusterinfo"),
            ("timeseries.CLUSTERSET x", "timeseries.clusterset"),
        ] {
            let g = gate(&args(line), Some(&info(name, ro, &[])));
            assert!(matches!(g, Gate::Refuse(_)), "{line}: {g:?}");
        }
        for (line, name) in [("FT.SEARCH idx *", "ft.search"), ("JSON.GET k", "json.get")] {
            assert_eq!(
                gate(&args(line), Some(&info(name, ro, &["@read"]))),
                Gate::Send,
                "{line}"
            );
        }
    }

    /// SORT_RO and FT.TAGVALS ask on a server without ACL categories, like KEYS (Redis
    /// Stack 6.2 and 7.2 give module commands none).
    #[test]
    fn sort_ro_asks_without_categories() {
        let s = info("sort_ro", &["readonly", "movablekeys"], &[]);
        assert_eq!(gate(&args("SORT_RO k"), Some(&s)), Gate::Ask);
        let t = info("ft.tagvals", &["readonly"], &[]);
        assert_eq!(gate(&args("FT.TAGVALS idx tag"), Some(&t)), Gate::Ask);
    }

    /// Before Redis 7.0 a container lists no subcommands and its own flags cover them
    /// all: 6.2 flags MEMORY readonly, MEMORY PURGE included.
    #[test]
    fn an_old_server_sends_only_known_reading_subcommands() {
        let memory = info("memory", &["readonly", "random"], &["@read", "@slow"]);
        assert_eq!(gate(&args("MEMORY USAGE k"), Some(&memory)), Gate::Send);
        for line in ["MEMORY PURGE", "MEMORY", "memory Purge"] {
            assert!(
                matches!(gate(&args(line), Some(&memory)), Gate::Refuse(_)),
                "{line}"
            );
        }
        let object = info("object", &["readonly"], &[]);
        assert_eq!(gate(&args("OBJECT ENCODING k"), Some(&object)), Gate::Send);
        let client = info("client", &["readonly"], &[]);
        assert!(matches!(
            gate(&args("CLIENT LIST"), Some(&client)),
            Gate::Refuse(_)
        ));
    }

    /// KEYS asks on a server with no ACL categories (before 6.0) too.
    #[test]
    fn keys_asks_without_categories() {
        let keys = info("keys", &["readonly", "sort_for_script"], &[]);
        assert_eq!(gate(&args("KEYS *"), Some(&keys)), Gate::Ask);
    }

    /// A NOPERM inside a reply names the user: fixed text.
    #[test]
    fn a_nested_noperm_is_fixed_text() {
        let line = error_line(
            "NOPERM",
            "User bob has no permissions to run the 'get' command",
        );
        assert!(!line.contains("bob"), "{line}");
        assert!(line.contains("refused"), "{line}");
    }

    fn info(name: &str, flags: &[&str], cats: &[&str]) -> CommandInfo {
        CommandInfo {
            name: name.into(),
            flags: flags.iter().map(|s| (*s).into()).collect(),
            categories: cats.iter().map(|s| (*s).into()).collect(),
            subcommands: Vec::new(),
            keys: Vec::new(),
            keys_partial: false,
        }
    }

    /// A connection that records every command it is sent.
    struct Recorder {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl redis::aio::ConnectionLike for Recorder {
        fn req_packed_command<'a>(
            &'a mut self,
            cmd: &'a redis::Cmd,
        ) -> redis::RedisFuture<'a, Value> {
            let words: Vec<String> = cmd
                .args_iter()
                .map(|a| match a {
                    redis::Arg::Simple(b) => String::from_utf8_lossy(b).into_owned(),
                    _ => String::new(),
                })
                .collect();
            self.log.lock().unwrap().push(words.join(" "));
            Box::pin(async { Ok(Value::Okay) })
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            _pipe: &'a redis::Pipeline,
            _offset: usize,
            _count: usize,
        ) -> redis::RedisFuture<'a, Vec<Value>> {
            self.log.lock().unwrap().push("PIPELINE".into());
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_db(&self) -> i64 {
            0
        }
    }

    #[tokio::test]
    async fn run_sends_nothing_that_is_a_write_or_refused_or_unconfirmed() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut conn = Recorder {
            log: std::sync::Arc::clone(&log),
        };
        let budget = Budget::new(None);
        let get = info("get", &["readonly", "fast"], &["@read", "@string"]);
        let set = info("set", &["write", "denyoom"], &["@write", "@string"]);
        let keys = info("keys", &["readonly"], &["@keyspace", "@read", "@dangerous"]);
        let plain = |name: &str| info(name, &["fast"], &["@fast", "@connection"]);
        let run = |line: &str, info: CommandInfo, confirmed: bool| Request::Run {
            args: Args::new(args(line)),
            info,
            confirmed,
        };
        // A write has no Permit here: never sent, confirmed or not.
        for confirmed in [false, true] {
            let r = super::run(&mut conn, &budget, run("SET k v", set.clone(), confirmed)).await;
            assert_eq!(r.unwrap_err(), LiveError::Failed(NOT_SENT.into()));
        }
        // The refusal list is checked again here: handed straight to `run`, these
        // are not sent (the info says readonly, which must not matter).
        for line in [
            "SUBSCRIBE c",
            "MONITOR",
            "SELECT 1",
            "MULTI",
            "WATCH k",
            "READONLY",
            "ASKING",
            "CLIENT SETNAME x",
        ] {
            let name = line.split(' ').next().unwrap().to_ascii_lowercase();
            let r = super::run(
                &mut conn,
                &budget,
                run(line, info(&name, &["readonly"], &["@read"]), true),
            )
            .await;
            assert!(r.is_err(), "{line}");
            let r = super::run(&mut conn, &budget, run(line, plain(&name), true)).await;
            assert!(r.is_err(), "{line}");
        }
        // A dangerous readonly command is sent only once confirmed.
        let r = super::run(&mut conn, &budget, run("KEYS *", keys.clone(), false)).await;
        assert!(r.is_err());
        assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
        // A readonly one is sent, and takes from the budget.
        let r = super::run(&mut conn, &budget, run("GET k", get, false)).await;
        assert!(r.is_ok(), "{r:?}");
        let r = super::run(&mut conn, &budget, run("KEYS *", keys, true)).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(*log.lock().unwrap(), ["GET k", "KEYS *"]);
        assert_eq!(budget.taken(), 2);
    }

    #[test]
    fn keys_a_spec_misses_are_found_where_the_command_puts_them() {
        let partial = |name: &str| CommandInfo {
            keys: vec![KeySpec {
                begin: server::Begin::Index(1),
                find: server::Find::Range {
                    last: 0,
                    step: 1,
                    limit: 0,
                },
            }],
            keys_partial: true,
            ..info(name, &["write", "movablekeys"], &["@write"])
        };
        let found = |line: &str, name: &str| command_keys(&args(line), &partial(name));
        // Redis 6.2 finds only the destination of these.
        assert_eq!(
            found("ZUNIONSTORE d 2 a b", "zunionstore"),
            Keys::Found(args("d a b"))
        );
        assert_eq!(
            found("ZINTERSTORE d 3 a b c WEIGHTS 1 2 3", "zinterstore"),
            Keys::Found(args("d a b c"))
        );
        // SORT on every version finds only the key.
        assert_eq!(
            found("SORT k BY w_* GET # STORE d", "sort"),
            Keys::Found(args("k d"))
        );
        // A pattern named STORE is not a destination; LIMIT's operands are skipped.
        assert_eq!(
            found("SORT k BY store LIMIT 0 5", "sort"),
            Keys::Found(args("k"))
        );
        assert_eq!(
            found("GEORADIUS k 1 2 5 km COUNT 3 STORE d", "georadius"),
            Keys::Found(args("k d"))
        );
        assert_eq!(
            found(
                "GEORADIUSBYMEMBER k m 5 km STOREDIST d",
                "georadiusbymember"
            ),
            Keys::Found(args("k d"))
        );
        // Where the specs found them all, nothing is added twice.
        let mut full = partial("zunionstore");
        full.keys.push(KeySpec {
            begin: server::Begin::Index(2),
            find: server::Find::KeyNum {
                idx: 0,
                first: 1,
                step: 1,
            },
        });
        assert_eq!(
            command_keys(&args("ZUNIONSTORE d 2 a b"), &full),
            Keys::Found(args("d a b"))
        );
        // Not partial: no guessing.
        let mut exact = partial("sort");
        exact.keys_partial = false;
        assert_eq!(
            command_keys(&args("SORT k STORE d"), &exact),
            Keys::Found(args("k"))
        );
    }

    #[test]
    fn scalars_are_drawn_like_redis_cli() {
        assert_eq!(lines(&Value::Int(5)), ["(integer) 5"]);
        assert_eq!(lines(&Value::Int(-3)), ["(integer) -3"]);
        assert_eq!(lines(&Value::Nil), ["(nil)"]);
        assert_eq!(lines(&Value::Okay), ["OK"]);
        assert_eq!(lines(&st("PONG")), ["PONG"]);
        assert_eq!(lines(&b("hello")), ["\"hello\""]);
        assert_eq!(lines(&b("")), ["\"\""]);
        assert_eq!(
            lines(&b("a\"b\\c\nd\r\te\u{7}\u{8}")),
            [r#""a\"b\\c\nd\r\te\a\b""#]
        );
        assert_eq!(
            lines(&Value::BulkString(vec![0xff, b'a', 0x00])),
            [r#""\xffa\x00""#]
        );
        assert_eq!(lines(&Value::Double(1.5)), ["(double) 1.5"]);
        assert_eq!(lines(&Value::Double(f64::INFINITY)), ["(double) inf"]);
        assert_eq!(lines(&Value::Double(f64::NEG_INFINITY)), ["(double) -inf"]);
        assert_eq!(lines(&Value::Boolean(true)), ["(true)"]);
        assert_eq!(lines(&Value::Boolean(false)), ["(false)"]);
        assert_eq!(
            lines(&Value::BigNumber(b"1234567890123456789012345".to_vec())),
            ["(big number) 1234567890123456789012345"]
        );
        assert_eq!(
            lines(&Value::VerbatimString {
                format: redis::VerbatimFormat::Text,
                text: "# Server\r\nredis_version:7.4.0\r\n".into(),
            }),
            ["# Server", "redis_version:7.4.0"]
        );
    }

    #[test]
    fn aggregates_are_numbered_and_nested_like_redis_cli() {
        assert_eq!(lines(&Value::Array(vec![])), ["(empty array)"]);
        assert_eq!(lines(&Value::Set(vec![])), ["(empty set)"]);
        assert_eq!(lines(&Value::Map(vec![])), ["(empty hash)"]);
        assert_eq!(
            lines(&Value::Array(vec![b("a"), Value::Int(2), Value::Nil])),
            ["1) \"a\"", "2) (integer) 2", "3) (nil)"]
        );
        let nested = Value::Array(vec![
            Value::Array(vec![b("a"), b("b")]),
            b("c"),
            Value::Array(vec![Value::Array(vec![b("d")]), Value::Array(vec![])]),
        ]);
        assert_eq!(
            lines(&nested),
            [
                "1) 1) \"a\"",
                "   2) \"b\"",
                "2) \"c\"",
                "3) 1) 1) \"d\"",
                "   2) (empty array)",
            ]
        );
        // Ten or more items: the numbers are right-aligned and nesting goes past them.
        let many = Value::Array(
            (0..11)
                .map(|i| {
                    if i == 9 {
                        Value::Array(vec![b("x"), b("y")])
                    } else {
                        Value::Int(i)
                    }
                })
                .collect(),
        );
        let l = lines(&many);
        assert_eq!(l[0], " 1) (integer) 0");
        assert_eq!(l[9], "10) 1) \"x\"");
        assert_eq!(l[10], "    2) \"y\"");
        assert_eq!(l[11], "11) (integer) 10");
    }

    #[test]
    fn resp3_maps_sets_and_errors() {
        let map = Value::Map(vec![
            (b("name"), b("lens")),
            (b("tags"), Value::Set(vec![b("a"), b("b")])),
            (st("ok"), Value::Boolean(true)),
        ]);
        assert_eq!(
            lines(&map),
            [
                "1# \"name\" => \"lens\"",
                "2# \"tags\" => 1~ \"a\"",
                "   2~ \"b\"",
                "3# ok => (true)",
            ]
        );
        let err = redis::RedisError::from((
            redis::ErrorKind::Server(ServerErrorKind::ResponseError),
            "ERR",
            "x".to_string(),
        ));
        let _ = err;
        let attr = Value::Attribute {
            data: Box::new(Value::Int(1)),
            attributes: vec![(b("ttl"), Value::Int(3))],
        };
        assert_eq!(lines(&attr), ["(integer) 1"]);
        let push = Value::Push {
            kind: redis::PushKind::Message,
            data: vec![b("message"), b("ch")],
        };
        assert_eq!(lines(&push), ["1) \"message\"", "2) \"ch\""]);
    }

    #[test]
    fn every_string_is_escaped() {
        let evil = "\u{1b}[31mred\u{202e}x\u{7f}\u{85}";
        let v = Value::Array(vec![
            b(evil),
            st(evil),
            Value::Map(vec![(b(evil), b(evil))]),
            Value::VerbatimString {
                format: redis::VerbatimFormat::Text,
                text: evil.into(),
            },
            Value::BigNumber(evil.as_bytes().to_vec()),
        ]);
        for l in lines(&v) {
            assert!(
                !l.chars()
                    .any(|c| c.is_control() || value::is_bidi_control(c)),
                "{l:?}"
            );
            assert!(l.contains("\\x1b"), "{l:?}");
        }
    }

    #[test]
    fn a_reply_over_one_mib_is_cut_and_says_so() {
        let big = Value::BulkString(vec![b'a'; MAX_REPLY_BYTES * 2]);
        let (l, cut) = format_reply(&big);
        assert!(cut);
        assert_eq!(l.last().unwrap(), CUT_NOTE);
        let total: usize = l.iter().map(String::len).sum();
        assert!(total <= MAX_REPLY_BYTES + CUT_NOTE.len() + 16, "{total}");
        // Many small items too, and a nested one.
        let many = Value::Array(vec![
            Value::Array((0..200_000).map(|_| b("0123456789")).collect()),
            b("after"),
        ]);
        let (l, cut) = format_reply(&many);
        assert!(cut);
        assert_eq!(l.last().unwrap(), CUT_NOTE);
        assert!(!l.iter().any(|x| x.contains("after")));
        let total: usize = l.iter().map(|x| x.len() + 1).sum();
        assert!(total <= MAX_REPLY_BYTES + CUT_NOTE.len() + 2, "{total}");
        // A reply under the cap is whole.
        let (_, cut) = format_reply(&Value::BulkString(vec![b'a'; 1000]));
        assert!(!cut);
    }

    #[test]
    fn output_debug_shows_no_value() {
        let mark = "s3cr3t-value-77";
        let out = Output::of(
            &Value::Array(vec![b(mark), Value::Map(vec![(b(mark), b(mark))])]),
            Duration::from_millis(1),
        );
        assert!(out.lines().iter().any(|l| l.contains(mark)));
        let dbg = format!(
            "{:?} {:?}",
            Reply::Ran(out),
            Request::Run {
                args: Args::new(args(&format!("GET {mark}"))),
                info: info("get", &["readonly"], &[]),
                confirmed: false,
            }
        );
        assert!(!value::leaks(&dbg, mark), "{dbg}");
        let dbg = format!(
            "{:?} {:?}",
            Request::Info { name: mark.into() },
            Request::Docs { name: mark.into() }
        );
        assert!(!value::leaks(&dbg, mark), "{dbg}");
    }

    /// The decision table: readonly, blocking, refused, dangerous and unknown.
    #[test]
    fn the_gate_sends_readonly_commands_only() {
        let get = info("get", &["readonly", "fast"], &["@read", "@string", "@fast"]);
        let set = info(
            "set",
            &["write", "denyoom"],
            &["@write", "@string", "@slow"],
        );
        let keys = info(
            "keys",
            &["readonly"],
            &["@keyspace", "@read", "@slow", "@dangerous"],
        );
        let blpop = info("blpop", &["write", "blocking"], &["@blocking"]);
        let xread = info(
            "xread",
            &["readonly", "blocking", "movablekeys"],
            &["@read"],
        );
        let xread62 = info(
            "xread",
            &["readonly", "movablekeys"],
            &["@read", "@blocking"],
        );
        let ping = info("ping", &["fast"], &["@fast", "@connection"]);
        let info_cmd = info("info", &["loading", "stale"], &["@slow", "@dangerous"]);
        let mut config = info("config", &[], &["@slow"]);
        config.subcommands = vec![
            info(
                "config|get",
                &["admin", "noscript"],
                &["@admin", "@dangerous"],
            ),
            info("config|set", &["admin"], &["@admin", "@dangerous"]),
        ];
        let mut object = info("object", &[], &["@slow"]);
        object.subcommands = vec![info(
            "object|encoding",
            &["readonly"],
            &["@keyspace", "@read"],
        )];

        let table: Vec<(&str, Option<&CommandInfo>, Result<Gate, &str>)> = vec![
            ("GET k", Some(&get), Ok(Gate::Send)),
            ("get k", Some(&get), Ok(Gate::Send)),
            // Writes go to the write gate (phase 3).
            ("SET k v", Some(&set), Ok(Gate::Write)),
            ("KEYS *", Some(&keys), Ok(Gate::Ask)),
            ("BLPOP q 0", Some(&blpop), Err("can block")),
            ("XREAD BLOCK 0 STREAMS s $", Some(&xread), Err("can block")),
            ("XREAD STREAMS s 0", Some(&xread), Ok(Gate::Send)),
            (
                "XREAD block 10 STREAMS s 0",
                Some(&xread62),
                Err("can block"),
            ),
            // No readonly flag: a write as far as the gate knows (the owner's open
            // question about an allow-list for these is in status.md).
            ("PING", Some(&ping), Ok(Gate::Write)),
            ("INFO", Some(&info_cmd), Ok(Gate::Write)),
            ("CONFIG GET maxmemory", Some(&config), Ok(Gate::Write)),
            ("CONFIG", Some(&config), Err("needs a subcommand")),
            ("CONFIG NOPE", Some(&config), Err("no command CONFIG NOPE")),
            ("OBJECT ENCODING k", Some(&object), Ok(Gate::Send)),
            ("NOSUCH x", None, Err("no command NOSUCH")),
            ("SUBSCRIBE ch", None, Err("Pub/Sub")),
            ("MONITOR", Some(&get), Err("Monitor")),
            ("SELECT 1", None, Err("SELECT")),
            ("AUTH pw", None, Err("connection every view shares")),
            ("HELLO 3", None, Err("connection every view shares")),
            ("MULTI", None, Err("connection every view shares")),
            ("WATCH k", None, Err("connection every view shares")),
            (
                "CLIENT SETNAME x",
                None,
                Err("connection every view shares"),
            ),
            ("RESET", None, Err("connection every view shares")),
            ("QUIT", None, Err("connection every view shares")),
            (
                "CLIENT REPLY OFF",
                None,
                Err("connection every view shares"),
            ),
            // Flags for another command are never trusted.
            ("SET k v", Some(&get), Err(NOT_SENT)),
        ];
        for (typed, info, want) in table {
            let got = gate(&args(typed), info);
            match want {
                Ok(g) => assert_eq!(got, g, "{typed}"),
                Err(text) => match &got {
                    Gate::Refuse(why) => assert!(why.contains(text), "{typed}: {why}"),
                    other => panic!("{typed}: {other:?}"),
                },
            }
        }
        assert_eq!(gate(&[], None), Gate::Refuse("Nothing to send.".into()));
    }

    #[test]
    fn a_typed_name_is_safe_to_draw() {
        let why = gate(&[b"x\x1b[2J\xe2\x80\xae".to_vec()], None);
        let Gate::Refuse(why) = why else {
            panic!("{why:?}")
        };
        assert!(
            !why.chars()
                .any(|c| c.is_control() || value::is_bidi_control(c)),
            "{why:?}"
        );
    }

    fn arr(v: Vec<Value>) -> Value {
        Value::Array(v)
    }

    fn entry(name: &str, flags: &[&str], cats: &[&str], subs: Vec<Value>) -> Value {
        arr(vec![
            b(name),
            Value::Int(-2),
            arr(flags.iter().map(|f| st(f)).collect()),
            Value::Int(0),
            Value::Int(0),
            Value::Int(0),
            arr(cats.iter().map(|f| st(f)).collect()),
            arr(vec![]),
            arr(vec![]),
            arr(subs),
        ])
    }

    #[test]
    fn reads_command_info_in_both_protocols() {
        let v = arr(vec![entry(
            "config",
            &[],
            &["@slow"],
            vec![entry(
                "config|get",
                &["admin"],
                &["@admin", "@dangerous"],
                vec![],
            )],
        )]);
        let i = parse_info(&v).unwrap();
        assert_eq!(i.name, "config");
        assert_eq!(i.subcommands[0].name, "config|get");
        assert!(i.subcommands[0].in_category("@dangerous"));
        assert_eq!(i.subcommand(b"GET").unwrap().display(), "CONFIG GET");
        // Redis 6.2: seven fields, no subcommands. RESP3: sets.
        let v = arr(vec![arr(vec![
            b("keys"),
            Value::Int(2),
            Value::Set(vec![st("readonly")]),
            Value::Int(0),
            Value::Int(0),
            Value::Int(0),
            Value::Set(vec![st("@read"), st("@dangerous")]),
        ])]);
        let i = parse_info(&v).unwrap();
        assert!(i.has_flag("readonly") && i.in_category("@dangerous"));
        assert!(i.subcommands.is_empty());
        assert_eq!(parse_info(&arr(vec![Value::Nil])), None);
        assert_eq!(parse_info(&arr(vec![])), None);
    }

    fn words(line: &str) -> Vec<Vec<u8>> {
        line.split(' ').map(|w| w.as_bytes().to_vec()).collect()
    }

    /// A Redis 7 key spec: `begin_search` index `at`, `find_keys` range to `last`.
    fn range_spec(at: i64, last: i64, step: i64) -> Value {
        let map = |pairs: Vec<(&str, Value)>| {
            Value::Map(pairs.into_iter().map(|(k, v)| (b(k), v)).collect())
        };
        map(vec![
            (
                "begin_search",
                map(vec![
                    ("type", b("index")),
                    ("spec", map(vec![("index", Value::Int(at))])),
                ]),
            ),
            (
                "find_keys",
                map(vec![
                    ("type", b("range")),
                    (
                        "spec",
                        map(vec![
                            ("lastkey", Value::Int(last)),
                            ("keystep", Value::Int(step)),
                            ("limit", Value::Int(0)),
                        ]),
                    ),
                ]),
            ),
        ])
    }

    /// A `COMMAND INFO` entry with first/last/step and the key specs `specs`.
    fn keyed(name: &str, flags: &[&str], fls: (i64, i64, i64), specs: Vec<Value>) -> Value {
        arr(vec![
            b(name),
            Value::Int(-2),
            arr(flags.iter().map(|f| st(f)).collect()),
            Value::Int(fls.0),
            Value::Int(fls.1),
            Value::Int(fls.2),
            arr(vec![st("@read")]),
            arr(vec![]),
            arr(specs),
            arr(vec![]),
        ])
    }

    #[test]
    fn a_commands_keys_come_from_its_key_specs_or_first_last_step() {
        // Redis 7: the key specs.
        let mget = parse_info(&arr(vec![keyed(
            "mget",
            &["readonly"],
            (1, -1, 1),
            vec![range_spec(1, -1, 1)],
        )]))
        .unwrap();
        assert!(!mget.keys_partial);
        assert_eq!(
            command_keys(&words("MGET a b c"), &mget),
            Keys::Found(words("a b c"))
        );
        // Redis 6.2: no key specs, the first/last/step triple.
        let v62 = arr(vec![arr(vec![
            b("mset"),
            Value::Int(-3),
            arr(vec![st("write")]),
            Value::Int(1),
            Value::Int(-1),
            Value::Int(2),
            arr(vec![st("@write")]),
        ])]);
        let mset = parse_info(&v62).unwrap();
        assert_eq!(
            command_keys(&words("MSET k1 v1 k2 v2"), &mset),
            Keys::Found(words("k1 k2"))
        );
        // Keyless.
        let ping = parse_info(&arr(vec![keyed("ping", &["fast"], (0, 0, 0), vec![])])).unwrap();
        assert_eq!(command_keys(&words("PING"), &ping), Keys::None);
        // Moving keys the specs can't place: unfound, never keyless.
        let zunion = parse_info(&arr(vec![keyed(
            "zunion",
            &["readonly", "movablekeys"],
            (0, 0, 0),
            vec![],
        )]))
        .unwrap();
        assert!(zunion.keys_partial);
        assert_eq!(command_keys(&words("ZUNION 2 a b"), &zunion), Keys::Unfound);
        // A spec redis-lens can't read makes the keys partial too.
        let odd = Value::Map(vec![(
            b("begin_search"),
            Value::Map(vec![(b("type"), b("unknown"))]),
        )]);
        let migrate = parse_info(&arr(vec![keyed(
            "migrate",
            &["write"],
            (3, 3, 1),
            vec![odd],
        )]))
        .unwrap();
        assert!(migrate.keys_partial);
        // A container: the subcommand's own key specs.
        let mut xinfo = parse_info(&arr(vec![entry("xinfo", &[], &["@slow"], vec![])])).unwrap();
        let stream = parse_info(&arr(vec![keyed(
            "xinfo|stream",
            &["readonly"],
            (2, 2, 1),
            vec![range_spec(2, 0, 1)],
        )]))
        .unwrap();
        xinfo.subcommands = vec![stream];
        assert_eq!(
            command_keys(&words("XINFO STREAM orders"), &xinfo),
            Keys::Found(words("orders"))
        );
        // Debug prints how many keys, never their names.
        let d = format!("{:?}", command_keys(&words("GET s3cr3t"), &mget));
        assert_eq!(d, "Found(1 keys)");
    }

    fn flat(pairs: Vec<(&str, Value)>) -> Value {
        arr(pairs.into_iter().flat_map(|(k, v)| [b(k), v]).collect())
    }

    fn arg(pairs: Vec<(&str, Value)>) -> Value {
        flat(pairs)
    }

    #[test]
    fn reads_command_docs_and_writes_the_arguments() {
        let set_args = arr(vec![
            arg(vec![("name", b("key")), ("type", b("key"))]),
            arg(vec![("name", b("value")), ("type", b("string"))]),
            arg(vec![
                ("name", b("condition")),
                ("type", b("oneof")),
                ("flags", arr(vec![st("optional")])),
                (
                    "arguments",
                    arr(vec![
                        arg(vec![("type", b("pure-token")), ("token", b("NX"))]),
                        arg(vec![("type", b("pure-token")), ("token", b("XX"))]),
                    ]),
                ),
            ]),
            arg(vec![
                ("name", b("seconds")),
                ("type", b("integer")),
                ("token", b("EX")),
                ("flags", arr(vec![st("optional")])),
            ]),
            arg(vec![
                ("name", b("field")),
                ("type", b("string")),
                ("flags", arr(vec![st("multiple")])),
            ]),
        ]);
        let v = arr(vec![
            b("set"),
            flat(vec![
                ("summary", b("Sets the string value of a key.")),
                ("since", b("1.0.0")),
                ("group", b("string")),
                ("complexity", b("O(1)")),
                ("arguments", set_args),
            ]),
        ]);
        let d = parse_docs(&v).unwrap();
        assert_eq!(d.summary, "Sets the string value of a key.");
        assert_eq!(d.complexity, "O(1)");
        assert_eq!(
            d.arguments,
            "key value [NX | XX] [EX seconds] field [field ...]"
        );
        // RESP3: a map, and subcommands.
        let v = Value::Map(vec![(
            b("config"),
            Value::Map(vec![
                (b("summary"), b("A container.")),
                (
                    b("subcommands"),
                    Value::Map(vec![(
                        b("config|get"),
                        Value::Map(vec![(b("summary"), b("Returns the\u{1b} values."))]),
                    )]),
                ),
            ]),
        )]);
        let d = parse_docs(&v).unwrap();
        let sub = d.subcommand("CONFIG", b"get").unwrap();
        assert_eq!(sub.summary, "Returns the\\x1b values.");
        assert_eq!(parse_docs(&arr(vec![])), None);
    }

    #[test]
    fn reads_command_names() {
        let v = arr(vec![b("get"), b("SET"), b("bad\nname"), b("get")]);
        assert_eq!(parse_names(&v), ["get", "set"]);
    }

    #[test]
    fn errors_of_the_typed_command() {
        let e = |kind, code: &'static str, detail: &str| {
            redis::RedisError::from((ErrorKind::Server(kind), code, detail.to_string()))
        };
        let noperm = e(
            ServerErrorKind::NoPerm,
            "NOPERM",
            "User lens-ro has no permissions to run the 'get' command",
        );
        let err = run_error("GET", &noperm).unwrap_err();
        assert_eq!(err.to_string(), "the server refused GET for this user");
        let syntax = e(
            ServerErrorKind::ResponseError,
            "ERR",
            "syntax error\u{1b}[2J",
        );
        assert_eq!(
            run_error("GET", &syntax).unwrap(),
            "(error) ERR syntax error\\x1b[2J"
        );
        let wrongtype = redis::RedisError::from((
            ErrorKind::Extension,
            "WRONGTYPE",
            "Operation against a key holding the wrong kind of value".to_string(),
        ));
        assert!(
            run_error("GET", &wrongtype)
                .unwrap()
                .starts_with("(error) Operation against a key")
        );
        let long = e(ServerErrorKind::ResponseError, "ERR", &"x".repeat(500));
        let line = run_error("GET", &long).unwrap();
        assert!(line.chars().count() <= ERROR_CHARS + 8 && line.ends_with('…'));
        let unknown = e(
            ServerErrorKind::ResponseError,
            "ERR",
            "unknown command 'FOO', with args beginning with: ",
        );
        assert_eq!(
            run_error("FOO", &unknown).unwrap_err(),
            LiveError::Unsupported("FOO".into())
        );
    }
}

/// The result views, the repeat count and the pipeline (spec "Command line panel").
#[cfg(test)]
mod views_and_repeat_tests {
    use std::sync::{Arc, Mutex};

    use proptest::prelude::*;

    use super::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn err(line: &str) -> Value {
        redis::parse_redis_value(format!("-{line}\r\n").as_bytes()).unwrap()
    }

    fn words(line: &str) -> Vec<Vec<u8>> {
        command::split(line).unwrap()
    }

    /// The replies each view is checked on: a name, the reply, and whether its
    /// command's flat array holds pairs.
    fn samples() -> Vec<(&'static str, Value, bool)> {
        vec![
            (
                "HGETALL user:1 (RESP2, flat pairs)",
                Value::Array(vec![b("name"), b("Ada"), b("visits"), b("12")]),
                true,
            ),
            (
                "LRANGE l 0 -1 (flat, even, not pairs)",
                Value::Array(vec![b("a"), b("b"), b("c"), b("d")]),
                false,
            ),
            (
                "HGETALL over RESP3 (a map)",
                Value::Map(vec![(b("name"), b("Ada")), (b("visits"), Value::Int(12))]),
                false,
            ),
            (
                "ZRANGE z 0 -1 WITHSCORES over RESP3 (pairs)",
                Value::Array(vec![
                    Value::Array(vec![b("ada"), Value::Double(1.5)]),
                    Value::Array(vec![b("cyd"), Value::Double(2.0)]),
                ]),
                true,
            ),
            (
                "XRANGE s - + (arrays of arrays)",
                Value::Array(vec![
                    Value::Array(vec![
                        b("1-0"),
                        Value::Array(vec![b("f"), b("v")]),
                        b("extra"),
                    ]),
                    Value::Array(vec![b("2-0"), Value::Array(vec![])]),
                ]),
                false,
            ),
            (
                "scalars",
                Value::Array(vec![
                    Value::Nil,
                    Value::Int(-7),
                    Value::Double(f64::INFINITY),
                    Value::Boolean(true),
                    Value::BigNumber(b"123456789012345678901234567890".to_vec()),
                    Value::Okay,
                    Value::SimpleString("PONG".into()),
                    Value::VerbatimString {
                        format: redis::VerbatimFormat::Text,
                        text: "line 1\nline 2".into(),
                    },
                    b(""),
                ]),
                false,
            ),
            (
                "a set, an error and a NOPERM inside a reply",
                Value::Array(vec![
                    Value::Set(vec![b("x"), b("y")]),
                    err("ERR syntax error"),
                    err("NOPERM User bob has no permissions to run the 'get' command"),
                ]),
                false,
            ),
            (
                "a map with a key that is not a string",
                Value::Map(vec![(Value::Int(1), b("one"))]),
                false,
            ),
            (
                "control and bidi characters",
                Value::Array(vec![
                    b("tab\there\u{1b}[31m red \u{202e}evil"),
                    Value::BulkString(vec![0xff, b'a', 0x07]),
                ]),
                true,
            ),
            ("one integer", Value::Int(42), false),
            ("an empty array", Value::Array(vec![]), false),
        ]
    }

    fn sampled(view: View) -> String {
        let mut out = String::new();
        for (name, v, pairs) in samples() {
            let o = Output::of(&v, Duration::ZERO);
            out.push_str(&format!("## {name}\n"));
            for l in o.lines_in(view, pairs) {
                out.push_str(l);
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn table_view() {
        insta::assert_snapshot!("table_view", sampled(View::Table));
    }

    #[test]
    fn json_view() {
        insta::assert_snapshot!("json_view", sampled(View::Json));
    }

    #[test]
    fn raw_view() {
        insta::assert_snapshot!("raw_view", sampled(View::Raw));
    }

    #[test]
    fn views_cycle_and_the_text_view_is_today_s() {
        let mut v = View::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(v.label());
            v = v.next();
        }
        assert_eq!(seen, ["text", "table", "JSON", "raw"]);
        assert_eq!(v, View::Text);
        for (_, v, pairs) in samples() {
            let o = Output::of(&v, Duration::ZERO);
            assert_eq!(o.lines_in(View::Text, pairs), format_reply(&v).0);
            assert_eq!(o.lines(), o.lines_in(View::Text, !pairs));
        }
        // An error line is the same in every view.
        let e = Output::error_line("(error) ERR x".into(), Duration::ZERO);
        for view in [View::Text, View::Table, View::Json, View::Raw] {
            assert_eq!(e.lines_in(view, true), ["(error) ERR x"]);
        }
    }

    /// No view lets a terminal control byte, a C1 control or a bidi control through,
    /// and the NOPERM text (which names the user) never shows.
    #[test]
    fn every_view_escapes_and_hides_the_noperm_text() {
        for view in [View::Text, View::Table, View::Json, View::Raw] {
            let all = sampled(view);
            for c in all.chars() {
                assert!(
                    c == '\n' || !(c.is_control() || value::is_bidi_control(c)),
                    "{view:?}: {c:?} in {all}"
                );
            }
            assert!(!all.contains("bob"), "{view:?}: {all}");
        }
    }

    #[test]
    fn the_json_view_is_json() {
        for (name, v, _) in samples() {
            let text = format_json(&v).0.join("\n");
            let parsed: serde_json::Value =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}\n{text}"));
            if name == "HGETALL over RESP3 (a map)" {
                assert_eq!(parsed, serde_json::json!({"name": "Ada", "visits": 12}));
            }
        }
        let j = |v: Value| serde_json::from_str::<serde_json::Value>(&format_json(&v).0.join(""));
        assert_eq!(j(Value::Nil).unwrap(), serde_json::Value::Null);
        assert_eq!(j(Value::Double(f64::NAN)).unwrap(), "nan");
        assert_eq!(j(Value::Double(-0.25)).unwrap(), -0.25);
        assert_eq!(
            j(Value::BigNumber(b"99999999999999999999999".to_vec())).unwrap(),
            "99999999999999999999999"
        );
        assert_eq!(
            j(Value::Set(vec![b("a")])).unwrap(),
            serde_json::json!(["a"])
        );
        assert_eq!(
            j(err("ERR nope")).unwrap(),
            serde_json::json!({"error": "ERR nope"})
        );
    }

    #[test]
    fn every_view_is_cut_past_one_mib_and_says_so() {
        let big = Value::Array(vec![Value::BulkString(vec![b'x'; 2 * MAX_REPLY_BYTES])]);
        let many = Value::Array((0..40_000).map(|i| b(&format!("{i:>40}"))).collect());
        for v in [&big, &many] {
            for (name, (lines, cut)) in [
                ("table", format_table(v, false)),
                ("table pairs", format_table(v, true)),
                ("json", format_json(v)),
                ("raw", format_raw(v)),
            ] {
                assert!(cut, "{name}");
                assert_eq!(lines.last().map(String::as_str), Some(CUT_NOTE), "{name}");
                let bytes: usize = lines.iter().map(String::len).sum();
                assert!(bytes <= MAX_REPLY_BYTES + 1024, "{name}: {bytes}");
            }
        }
    }

    #[test]
    fn pairs_come_from_the_command() {
        for line in [
            "HGETALL h",
            "hgetall h",
            "CONFIG GET maxmemory*",
            "MEMORY STATS",
            "ZRANGE z 0 -1 WITHSCORES",
            "HRANDFIELD h 3 withvalues",
            "FT.INFO idx",
        ] {
            assert!(pairs_hint(&words(line)), "{line}");
        }
        for line in [
            "LRANGE l 0 -1",
            "ZRANGE z 0 -1",
            "CONFIG SET a b",
            "SMEMBERS s",
            "",
        ] {
            assert!(!pairs_hint(&words(line)), "{line}");
        }
    }

    #[test]
    fn repeat_counts() {
        let p = |line: &str| parse_repeat(&words(line));
        assert_eq!(p("5 INCR counter"), Ok(Some(5)));
        assert_eq!(p("007 GET k"), Ok(Some(7)));
        assert_eq!(p("100 PING"), Ok(Some(100)));
        assert_eq!(p("1 PING"), Ok(Some(1)));
        assert_eq!(p("GET 5"), Ok(None));
        assert_eq!(p("5"), Ok(None));
        assert_eq!(p(""), Ok(None));
        assert!(p("0 PING").unwrap_err().contains("of 0 sends nothing"));
        for line in ["101 PING", "99999999999999999999999999 PING"] {
            let e = p(line).unwrap_err();
            assert!(e.contains("at most 100"), "{line}: {e}");
            assert!(!e.contains('\n'), "one line: {e}");
        }
        for line in ["-1 PING", "+5 PING", "-0 PING"] {
            assert!(p(line).unwrap_err().contains("1 to 100"), "{line}");
        }
        assert_eq!(count_words(&words("5 GET k")), 1);
        assert_eq!(count_words(&words("GET k")), 0);
        assert_eq!(count_words(&words("5")), 0);
    }

    #[test]
    fn a_counted_line_is_masked_after_its_count() {
        assert_eq!(
            masked_with_count(&words("3 CONFIG SET requirepass hunter2")),
            "3 CONFIG SET requirepass ***"
        );
        assert_eq!(masked_with_count(&words("2 AUTH hunter2")), "2 AUTH ***");
        assert_eq!(masked_with_count(&words("AUTH hunter2")), "AUTH ***");
        assert_eq!(masked_with_count(&words("-3 AUTH hunter2")), "-3 AUTH ***");
    }

    #[test]
    fn pipelines_hold_a_tenth_of_a_second_of_the_budget() {
        assert_eq!(pipeline_batch(None, 100), 100);
        assert_eq!(pipeline_batch(Some(0), 100), 100);
        assert_eq!(pipeline_batch(Some(5_000), 100), 100);
        assert_eq!(pipeline_batch(Some(100), 100), 10);
        assert_eq!(pipeline_batch(Some(20), 7), 2);
        assert_eq!(pipeline_batch(Some(5), 7), 1);
        assert_eq!(pipeline_batch(Some(1_000), 3), 3);
        assert_eq!(pipeline_batch(None, 500), MAX_REPEAT);
    }

    proptest! {
        /// A leading number is always read as the count: 1 to 100 repeat, anything
        /// else is refused with one line, and the command is what follows.
        #[test]
        fn a_leading_number_is_the_count(
            n in 0u64..10_000_000,
            zeros in 0usize..3,
            cmd in "[A-Za-z]{1,10}",
            rest in proptest::collection::vec("[a-z0-9]{0,6}", 0..4),
        ) {
            let mut args = vec![
                format!("{}{n}", "0".repeat(zeros)).into_bytes(),
                cmd.clone().into_bytes(),
            ];
            args.extend(rest.iter().map(|r| r.clone().into_bytes()));
            prop_assert_eq!(count_words(&args), 1);
            match parse_repeat(&args) {
                Ok(Some(t)) => prop_assert!((1..=100).contains(&t) && u64::from(t) == n),
                Ok(None) => prop_assert!(false, "a number was not read as a count"),
                Err(why) => {
                    prop_assert!(n == 0 || n > 100);
                    prop_assert!(!why.contains('\n') && why.starts_with("Not sent:"));
                }
            }
            prop_assert_eq!(&args[count_words(&args)..][0], &cmd.into_bytes());
        }

        /// A first word that is not a number is the command: no count.
        #[test]
        fn a_word_is_never_a_count(
            first in "[A-Za-z_.][A-Za-z0-9_.|-]{0,10}",
            rest in proptest::collection::vec(any::<Vec<u8>>(), 0..4),
        ) {
            let mut args = vec![first.into_bytes()];
            args.extend(rest);
            prop_assert_eq!(parse_repeat(&args), Ok(None));
            prop_assert_eq!(count_words(&args), 0);
        }

        /// Any words at all: no panic, and a count is only ever 1 to 100.
        #[test]
        fn parsing_any_line_never_panics(
            args in proptest::collection::vec(any::<Vec<u8>>(), 0..5),
        ) {
            if let Ok(Some(n)) = parse_repeat(&args) {
                prop_assert!((1..=MAX_REPEAT).contains(&n));
            }
        }

        /// Whatever a reply holds, no view draws a control or bidi character, and the
        /// JSON view is JSON.
        #[test]
        fn no_view_draws_a_control_character(s in any::<Vec<u8>>(), t in "\\PC*") {
            let v = Value::Array(vec![
                Value::BulkString(s.clone()),
                Value::Map(vec![(Value::BulkString(s), Value::SimpleString(t.clone()))]),
                Value::Array(vec![Value::BulkString(t.into_bytes()), Value::Int(1)]),
            ]);
            let o = Output::of(&v, Duration::ZERO);
            for view in [View::Text, View::Table, View::Json, View::Raw] {
                for pairs in [false, true] {
                    for l in o.lines_in(view, pairs) {
                        prop_assert!(
                            !l.chars().any(|c| c.is_control() || value::is_bidi_control(c)),
                            "{:?}: {:?}",
                            view,
                            l
                        );
                    }
                }
            }
            let json = o.lines_in(View::Json, false).join("\n");
            prop_assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok(), "{}", json);
        }
    }

    /// A connection that answers a pipeline of `n` with `n` replies (the second an
    /// error when `fail_second`) and records what it was sent.
    struct Piped {
        log: Arc<Mutex<Vec<String>>>,
        fail_second: bool,
    }

    impl redis::aio::ConnectionLike for Piped {
        fn req_packed_command<'a>(
            &'a mut self,
            _cmd: &'a redis::Cmd,
        ) -> redis::RedisFuture<'a, Value> {
            self.log.lock().unwrap().push("ONE".into());
            Box::pin(async { Ok(Value::Int(1)) })
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            pipe: &'a redis::Pipeline,
            _offset: usize,
            count: usize,
        ) -> redis::RedisFuture<'a, Vec<Value>> {
            self.log
                .lock()
                .unwrap()
                .push(format!("PIPELINE {}", pipe.len()));
            let fail = self.fail_second;
            Box::pin(async move {
                Ok((0..count)
                    .map(|i| {
                        if fail && i == 1 {
                            err("WRONGTYPE Operation against a key holding the wrong kind of value")
                        } else {
                            Value::Int(i64::try_from(i).unwrap() + 1)
                        }
                    })
                    .collect())
            })
        }

        fn get_db(&self) -> i64 {
            0
        }
    }

    fn info(name: &str, flags: &[&str], cats: &[&str]) -> CommandInfo {
        CommandInfo {
            name: name.into(),
            flags: flags.iter().map(|s| (*s).into()).collect(),
            categories: cats.iter().map(|s| (*s).into()).collect(),
            ..CommandInfo::default()
        }
    }

    fn batch(line: &str, info: CommandInfo, confirmed: bool, times: u32) -> Request {
        Request::Batch {
            args: Args::new(words(line)),
            info,
            confirmed,
            times,
        }
    }

    #[tokio::test]
    async fn a_batch_is_one_pipeline_checked_command_by_command() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut conn = Piped {
            log: Arc::clone(&log),
            fail_second: true,
        };
        let budget = Budget::new(None);
        let get = info("get", &["readonly", "fast"], &["@read"]);
        let r = super::run(&mut conn, &budget, batch("GET k", get.clone(), false, 5)).await;
        let Ok(Reply::Batch(outs)) = r else {
            panic!("{r:?}")
        };
        assert_eq!(outs.len(), 5);
        assert_eq!(outs[0].lines(), ["(integer) 1"]);
        assert!(outs[1].error && outs[1].lines()[0].starts_with("(error) WRONGTYPE"));
        assert_eq!(outs[4].lines(), ["(integer) 5"]);
        assert_eq!(*log.lock().unwrap(), ["PIPELINE 5"]);
        assert_eq!(budget.taken(), 5);

        // Not let through: nothing is sent, and nothing taken from the budget.
        log.lock().unwrap().clear();
        let set = info("set", &["write", "denyoom"], &["@write"]);
        let keys = info("keys", &["readonly"], &["@read", "@dangerous"]);
        let script = info("eval_ro", &["readonly"], &["@scripting"]);
        for req in [
            batch("SET k v", set, true, 3),
            batch("KEYS *", keys.clone(), false, 3),
            batch("EVAL_RO \"return 1\" 0", script, true, 3),
            batch(
                "SUBSCRIBE c",
                info("subscribe", &["readonly"], &[]),
                true,
                3,
            ),
            batch("GET k", get.clone(), false, 0),
            batch("GET k", get.clone(), false, MAX_REPEAT + 1),
        ] {
            let r = super::run(&mut conn, &budget, req).await;
            assert!(matches!(r, Err(LiveError::Failed(_))), "{r:?}");
        }
        assert!(log.lock().unwrap().is_empty());
        assert_eq!(budget.taken(), 5);
        // A confirmed @dangerous readonly one is sent.
        let r = super::run(&mut conn, &budget, batch("KEYS *", keys, true, 2)).await;
        assert!(
            matches!(r, Ok(Reply::Batch(ref o)) if o.len() == 2),
            "{r:?}"
        );
    }

    /// The budget holds for a repeat as for single commands: 10 commands at 10 a
    /// second take a second, in pipelines or one by one.
    #[tokio::test(start_paused = true)]
    async fn the_budget_limits_a_repeat_in_pipelines_and_one_by_one() {
        let get = info("get", &["readonly"], &["@read"]);
        for (rate, pipelined, trips) in [(10, false, 10), (10, true, 10), (40, true, 3)] {
            let log = Arc::new(Mutex::new(Vec::new()));
            let mut conn = Piped {
                log: Arc::clone(&log),
                fail_second: false,
            };
            let budget = Budget::new(Some(rate));
            let t0 = tokio::time::Instant::now();
            let mut left = 10;
            while left > 0 {
                let n = if pipelined {
                    pipeline_batch(budget.per_sec(), left)
                } else {
                    1
                };
                let req = if n > 1 {
                    batch("GET k", get.clone(), false, n)
                } else {
                    Request::Run {
                        args: Args::new(words("GET k")),
                        info: get.clone(),
                        confirmed: false,
                    }
                };
                super::run(&mut conn, &budget, req).await.unwrap();
                left -= n;
            }
            assert_eq!(budget.taken(), 10);
            let want = Duration::from_secs_f64(10.0 / f64::from(rate));
            assert!(
                t0.elapsed() >= want,
                "{rate} {pipelined}: {:?}",
                t0.elapsed()
            );
            // At 10 a second a pipeline holds one command (a tenth of a second); at 40
            // it holds 4.
            assert_eq!(log.lock().unwrap().len(), trips, "{rate} {pipelined}");
        }
    }

    #[test]
    fn batch_debug_shows_no_value() {
        let r = batch(
            "GET hunter2-mark",
            info("get", &["readonly"], &[]),
            false,
            3,
        );
        let d = format!("{r:?}");
        assert!(!d.contains("hunter2"), "{d}");
        assert!(d.contains("times: 3"), "{d}");
        let o = Output::of(&b("hunter2-mark"), Duration::ZERO);
        assert!(!format!("{:?}", Reply::Batch(vec![o])).contains("hunter2"));
    }
}
