//! Live requests for the streams view. Owner: wave B. Phase 1: no writes, no values.
//!
//! One request reads `XINFO STREAM`, `XINFO GROUPS` and `XINFO CONSUMERS` for a list of
//! stream keys. It reads entry IDs only: `XINFO STREAM` (without `FULL`) answers with the
//! first and last entry, field values included, and [`parse_stream`] keeps only the ID of
//! the first one. No entry value is stored in a type of this module, so none can reach the
//! screen, a log, a `Debug` print or a snapshot.
//!
//! Every command goes through [`query`] / [`query_pipe`], so it takes from the budget, and
//! every failure is turned into a [`LiveError`] in redis-lens's own words. A refused or
//! unsupported `XINFO STREAM` fails the whole request (the view turns itself off); a
//! refused `XINFO GROUPS`, `XINFO CONSUMERS` or `MEMORY USAGE` only leaves that part of
//! one stream out, with a fixed note.

use std::time::{SystemTime, UNIX_EPOCH};

use redis::{RedisError, RedisResult, ServerErrorKind, Value};

use super::{Conn, LiveError, error_for, query, query_pipe};
use crate::budget::Budget;

/// The most streams one request reads.
pub const MAX_STREAMS: usize = 50;
/// The most groups of one stream whose consumers are read.
pub const MAX_GROUPS: usize = 25;

/// `stuck`: a consumer holding pending entries has been idle this long (30 min).
pub const STUCK_IDLE_MS: u64 = 30 * 60 * 1000;
/// `lagging`: a group's lag is at least this many entries.
pub const LAGGING_LAG: u64 = 1_000;
/// `unbounded`: the oldest entry is older than this (30 days)...
pub const UNBOUNDED_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// ... and the stream holds at least this many entries.
pub const UNBOUNDED_LEN: u64 = 100_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Reads the streams `keys` (the first [`MAX_STREAMS`]). Commands in `refused` are
    /// not sent: the server refused them for this user before.
    Info { keys: Vec<String>, refused: Refused },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// One entry per requested key, in order, and every command refused so far (the
    /// request's plus this poll's), for the view to send back with its next poll.
    Info {
        streams: Vec<Fetched>,
        refused: Refused,
    },
}

/// The optional commands of a poll that the server refused for this user (`NOPERM`).
/// They are not asked again until the view is refreshed with `r`, so a refusal does
/// not land in the ACL log every 5 s.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refused {
    /// `TIME`: ages use this computer's clock instead.
    pub time: bool,
    pub groups: bool,
    pub consumers: bool,
    pub memory: bool,
}

/// What reading one key gave.
#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    pub key: String,
    /// `None` when the key is gone or is no longer a stream (the scan was a while ago).
    pub info: Option<StreamInfo>,
}

/// What `XINFO STREAM`, `XINFO GROUPS` and `XINFO CONSUMERS` say about one stream. Entry
/// IDs only, never field values.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub key: String,
    pub length: u64,
    /// `MEMORY USAGE`; `None` when the user may not run it.
    pub bytes: Option<u64>,
    /// The ID of the oldest entry. `None` for an empty stream.
    pub first_id: Option<String>,
    /// `last-generated-id`.
    pub last_id: String,
    /// How old the oldest entry is, from its ID and the server's clock. `None` for an
    /// empty stream.
    pub oldest_age_ms: Option<u64>,
    /// `entries-added` (Redis 7.0 and later).
    pub entries_added: Option<u64>,
    /// `None` when `XINFO GROUPS` was refused.
    pub groups: Option<Vec<GroupInfo>>,
    /// Why parts are missing, in redis-lens's own words ([`LiveError`]'s texts).
    pub notes: Vec<String>,
}

/// The lag of a consumer group (Redis 7.0 and later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lag {
    /// The server does not report a lag (before Redis 7.0).
    Unsupported,
    /// The server reports `nil`: it cannot work the lag out (entries were deleted).
    Unknown,
    Known(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupInfo {
    pub name: String,
    /// The number of consumers.
    pub consumers: u64,
    /// Entries delivered and not yet acknowledged.
    pub pending: u64,
    pub last_delivered_id: String,
    pub lag: Lag,
    /// `None` when `XINFO CONSUMERS` was refused or the group is past [`MAX_GROUPS`].
    pub readers: Option<Vec<ConsumerInfo>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsumerInfo {
    pub name: String,
    pub pending: u64,
    pub idle_ms: u64,
    /// Time since the last successful interaction (Redis 7.2 and later).
    pub inactive_ms: Option<u64>,
}

/// What a stream's state says, as the spec's verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A consumer that holds pending entries has been idle 30 min or more.
    Stuck,
    /// A group's lag is 1,000 or more.
    Lagging,
    /// No consumer groups.
    NoReaders,
    /// The oldest entry is over 30 days old and the stream holds 100,000 entries or more.
    Unbounded,
    Ok,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stuck => "stuck",
            Self::Lagging => "lagging",
            Self::NoReaders => "no readers",
            Self::Unbounded => "unbounded",
            Self::Ok => "ok",
        }
    }
}

/// Every verdict that applies, in the spec's order (`stuck`, `lagging`, `no readers`,
/// `unbounded`); `[Ok]` when none does. The first is the one the list shows.
pub fn verdicts(s: &StreamInfo) -> Vec<Verdict> {
    let mut out = Vec::new();
    let groups = s.groups.as_deref().unwrap_or(&[]);
    let stuck = groups.iter().any(|g| {
        g.readers
            .iter()
            .flatten()
            .any(|c| c.pending > 0 && c.idle_ms >= STUCK_IDLE_MS)
    });
    if stuck {
        out.push(Verdict::Stuck);
    }
    if groups
        .iter()
        .any(|g| matches!(g.lag, Lag::Known(n) if n >= LAGGING_LAG))
    {
        out.push(Verdict::Lagging);
    }
    // Without the groups (refused) nothing is known about readers.
    if s.groups.as_ref().is_some_and(Vec::is_empty) {
        out.push(Verdict::NoReaders);
    }
    if s.length >= UNBOUNDED_LEN && s.oldest_age_ms.is_some_and(|a| a > UNBOUNDED_AGE_MS) {
        out.push(Verdict::Unbounded);
    }
    if out.is_empty() {
        out.push(Verdict::Ok);
    }
    out
}

/// Runs one request. Every command goes through `budget`.
pub async fn run(conn: &mut Conn, budget: &Budget, req: Request) -> Result<Reply, LiveError> {
    match req {
        Request::Info { keys, mut refused } => {
            let now_ms = server_time_ms(conn, budget, &mut refused).await?;
            let mut out = Vec::with_capacity(keys.len().min(MAX_STREAMS));
            for key in keys.into_iter().take(MAX_STREAMS) {
                let info = fetch(conn, budget, &key, now_ms, &mut refused).await?;
                out.push(Fetched { key, info });
            }
            Ok(Reply::Info {
                streams: out,
                refused,
            })
        }
    }
}

/// The server's clock in Unix milliseconds (`TIME`), so ages compare IDs, which the server
/// made, with the server's own clock. Falls back to ours when `TIME` fails, and stops
/// asking once it is refused.
async fn server_time_ms(
    conn: &mut Conn,
    budget: &Budget,
    refused: &mut Refused,
) -> Result<u64, LiveError> {
    if refused.time {
        return Ok(local_time_ms());
    }
    match query::<(u64, u64)>(conn, budget, "TIME", &redis::cmd("TIME")).await {
        Ok((secs, micros)) => Ok(secs.saturating_mul(1000).saturating_add(micros / 1000)),
        Err(LiveError::Closed) => Err(LiveError::Closed),
        Err(e) => {
            refused.time |= matches!(e, LiveError::Denied(_));
            Ok(local_time_ms())
        }
    }
}

fn local_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What a failed command of a pipeline means for the stream it was about.
enum Failure {
    /// The key is gone, or holds something else now.
    Gone,
    Error(LiveError),
}

fn failure(name: &str, e: &RedisError) -> Failure {
    let text = format!("{} {}", e.code().unwrap_or(""), e.detail().unwrap_or("")).to_lowercase();
    let no_such_key = e.kind() == redis::ErrorKind::Server(ServerErrorKind::ResponseError)
        && text.contains("no such key");
    if no_such_key || e.code() == Some("WRONGTYPE") {
        Failure::Gone
    } else {
        Failure::Error(error_for(name, e))
    }
}

async fn fetch(
    conn: &mut Conn,
    budget: &Budget,
    key: &str,
    now_ms: u64,
    refused: &mut Refused,
) -> Result<Option<StreamInfo>, LiveError> {
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    pipe.cmd("XINFO").arg("STREAM").arg(key);
    if !refused.groups {
        pipe.cmd("XINFO").arg("GROUPS").arg(key);
    }
    if !refused.memory {
        pipe.cmd("MEMORY").arg("USAGE").arg(key);
    }
    let want = 1 + usize::from(!refused.groups) + usize::from(!refused.memory);
    let replies: Vec<RedisResult<Value>> = query_pipe(conn, budget, "XINFO STREAM", &pipe).await?;
    if replies.len() != want {
        return Err(LiveError::Failed("XINFO STREAM failed".into()));
    }
    let mut replies = replies.into_iter();
    let stream = replies.next().expect("XINFO STREAM's reply");
    let groups = if refused.groups {
        Err(None)
    } else {
        replies.next().expect("XINFO GROUPS' reply").map_err(Some)
    };
    let memory = if refused.memory {
        Err(None)
    } else {
        replies.next().expect("MEMORY USAGE's reply").map_err(Some)
    };

    let stream = match stream {
        Ok(v) => v,
        Err(e) => {
            return match failure("XINFO STREAM", &e) {
                Failure::Gone => Ok(None),
                Failure::Error(err) => Err(err),
            };
        }
    };
    let mut notes = Vec::new();
    let groups = match groups {
        Ok(v) => Some(v),
        // Refused before: not asked, still said.
        Err(None) => {
            notes.push(LiveError::Denied("XINFO GROUPS".into()).to_string());
            None
        }
        Err(Some(e)) => match failure("XINFO GROUPS", &e) {
            Failure::Gone => return Ok(None),
            Failure::Error(LiveError::Closed) => return Err(LiveError::Closed),
            Failure::Error(err) => {
                refused.groups |= matches!(err, LiveError::Denied(_));
                notes.push(err.to_string());
                None
            }
        },
    };
    let bytes = match memory {
        Ok(v) => uint(&v),
        Err(None) => {
            notes.push(LiveError::Denied("MEMORY USAGE".into()).to_string());
            None
        }
        Err(Some(e)) => {
            // Only worth saying when the user may not size keys; a vanished key is not.
            match error_for("MEMORY USAGE", &e) {
                LiveError::Closed => return Err(LiveError::Closed),
                denied @ LiveError::Denied(_) => {
                    refused.memory = true;
                    notes.push(denied.to_string());
                }
                _ => {}
            }
            None
        }
    };

    let mut info = parse_stream(key, &stream, groups.as_ref(), bytes, now_ms);
    info.notes = notes;
    drop(stream);

    // Consumers: one `XINFO CONSUMERS` per group, pipelined.
    if refused.consumers && info.groups.as_ref().is_some_and(|g| !g.is_empty()) {
        info.notes
            .push(LiveError::Denied("XINFO CONSUMERS".into()).to_string());
    } else if let Some(gs) = info.groups.as_mut().filter(|g| !g.is_empty()) {
        let names = group_names(groups.as_ref());
        let mut pipe = redis::pipe();
        pipe.ignore_errors();
        for name in names.iter().take(MAX_GROUPS) {
            pipe.cmd("XINFO")
                .arg("CONSUMERS")
                .arg(key)
                .arg(name.as_slice());
        }
        let replies: Vec<RedisResult<Value>> =
            query_pipe(conn, budget, "XINFO CONSUMERS", &pipe).await?;
        let mut refused_note = None;
        for (g, reply) in gs.iter_mut().zip(replies) {
            match reply {
                Ok(v) => g.readers = Some(parse_consumers(&v)),
                Err(e) => match failure("XINFO CONSUMERS", &e) {
                    Failure::Gone => return Ok(None),
                    Failure::Error(LiveError::Closed) => return Err(LiveError::Closed),
                    Failure::Error(err) => {
                        refused.consumers |= matches!(err, LiveError::Denied(_));
                        refused_note = Some(err.to_string());
                    }
                },
            }
        }
        info.notes.extend(refused_note);
        if gs.len() > MAX_GROUPS {
            info.notes.push(format!(
                "consumers are read for the first {MAX_GROUPS} groups"
            ));
        }
    }
    Ok(Some(info))
}

/// The group names as sent by the server, as bytes, in the order of `XINFO GROUPS`.
fn group_names(groups: Option<&Value>) -> Vec<Vec<u8>> {
    let Some(Value::Array(items)) = groups else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|g| match field(g, "name")? {
            Value::BulkString(b) => Some(b.clone()),
            Value::SimpleString(s) => Some(s.clone().into_bytes()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Parsing. Pure, and tolerant: a reply that does not look as expected gives defaults,
// never a panic.
// ---------------------------------------------------------------------------------------

/// The `(name, value)` pairs of a RESP2 flat array (`name, value, name, value …`) or a
/// RESP3 map. Entries whose name is not text are skipped.
pub(crate) fn pairs(v: &Value) -> Vec<(String, &Value)> {
    match v {
        Value::Map(m) => m.iter().filter_map(|(k, v)| Some((text(k)?, v))).collect(),
        Value::Array(items) => items
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|[k, v]| Some((text(k)?, v)))
            .collect(),
        _ => Vec::new(),
    }
}

/// The value of the field `name` of a flat array or map.
pub(crate) fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    pairs(v)
        .into_iter()
        .find_map(|(k, v)| k.eq_ignore_ascii_case(name).then_some(v))
}

/// Text from a string-like value. `Nil` and everything else gives `None`.
pub(crate) fn text(v: &Value) -> Option<String> {
    match v {
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        Value::Int(i) => Some(i.to_string()),
        Value::Double(d) => Some(d.to_string()),
        _ => None,
    }
}

/// An unsigned number from an integer or a number in text.
pub(crate) fn uint(v: &Value) -> Option<u64> {
    match v {
        Value::Int(i) => u64::try_from(*i).ok(),
        Value::Double(d) if d.is_finite() && *d >= 0.0 => Some(*d as u64),
        Value::BulkString(_) | Value::SimpleString(_) | Value::VerbatimString { .. } => {
            let t = text(v)?;
            let t = t.trim();
            t.parse::<u64>().ok().or_else(|| {
                t.parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite() && *f >= 0.0)
                    .map(|f| f as u64)
            })
        }
        _ => None,
    }
}

/// The millisecond part of a stream ID, `1700000000123-5` → `1700000000123`.
pub fn id_millis(id: &str) -> Option<u64> {
    id.split('-').next()?.parse().ok()
}

/// Reads the reply of `XINFO STREAM` and, when given, of `XINFO GROUPS`. Of `first-entry`
/// only the ID is kept: its field values are dropped here.
pub fn parse_stream(
    key: &str,
    stream: &Value,
    groups: Option<&Value>,
    bytes: Option<u64>,
    now_ms: u64,
) -> StreamInfo {
    let pairs = pairs(stream);
    let get = |name: &str| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| *v);
    let first_id = get("first-entry").and_then(|e| match e {
        Value::Array(parts) => parts.first().and_then(text),
        _ => None,
    });
    let oldest_age_ms = first_id
        .as_deref()
        .and_then(id_millis)
        .map(|ms| now_ms.saturating_sub(ms));
    StreamInfo {
        key: key.to_string(),
        length: get("length").and_then(uint).unwrap_or(0),
        bytes,
        first_id,
        last_id: get("last-generated-id").and_then(text).unwrap_or_default(),
        oldest_age_ms,
        entries_added: get("entries-added").and_then(uint),
        groups: groups.map(parse_groups),
        notes: Vec::new(),
    }
}

/// Reads the reply of `XINFO GROUPS`.
pub fn parse_groups(v: &Value) -> Vec<GroupInfo> {
    let Value::Array(items) = v else {
        return Vec::new();
    };
    items
        .iter()
        .map(|g| {
            let get = |name: &str| field(g, name);
            GroupInfo {
                name: get("name").and_then(text).unwrap_or_default(),
                consumers: get("consumers").and_then(uint).unwrap_or(0),
                pending: get("pending").and_then(uint).unwrap_or(0),
                last_delivered_id: get("last-delivered-id").and_then(text).unwrap_or_default(),
                lag: match get("lag") {
                    None => Lag::Unsupported,
                    Some(v) => uint(v).map_or(Lag::Unknown, Lag::Known),
                },
                readers: None,
            }
        })
        .collect()
}

/// Reads the reply of `XINFO CONSUMERS`.
pub fn parse_consumers(v: &Value) -> Vec<ConsumerInfo> {
    let Value::Array(items) = v else {
        return Vec::new();
    };
    items
        .iter()
        .map(|c| {
            let get = |name: &str| field(c, name);
            ConsumerInfo {
                name: get("name").and_then(text).unwrap_or_default(),
                pending: get("pending").and_then(uint).unwrap_or(0),
                idle_ms: get("idle").and_then(uint).unwrap_or(0),
                inactive_ms: get("inactive").and_then(uint),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn flat(items: Vec<(&str, Value)>) -> Value {
        Value::Array(
            items
                .into_iter()
                .flat_map(|(k, v)| [b(k), v])
                .collect::<Vec<_>>(),
        )
    }

    fn entry(id: &str) -> Value {
        Value::Array(vec![b(id), flat(vec![("password", b("hunter2"))])])
    }

    /// The reply of `XINFO STREAM` on Redis 7.2 / Valkey.
    fn stream_reply(len: i64, first: Option<&str>) -> Value {
        flat(vec![
            ("length", Value::Int(len)),
            ("radix-tree-keys", Value::Int(1)),
            ("radix-tree-nodes", Value::Int(2)),
            ("last-generated-id", b("1700000009999-3")),
            ("max-deleted-entry-id", b("0-0")),
            ("entries-added", Value::Int(len + 5)),
            ("recorded-first-entry-id", b("1700000000000-0")),
            ("groups", Value::Int(1)),
            ("first-entry", first.map_or(Value::Nil, entry)),
            ("last-entry", entry("1700000009999-3")),
        ])
    }

    fn group(name: &str, pending: i64, lag: Option<Value>) -> Value {
        let mut f = vec![
            ("name", b(name)),
            ("consumers", Value::Int(2)),
            ("pending", Value::Int(pending)),
            ("last-delivered-id", b("1700000005000-0")),
        ];
        if let Some(l) = lag {
            f.push(("entries-read", Value::Int(10)));
            f.push(("lag", l));
        }
        flat(f)
    }

    fn consumer(name: &str, pending: i64, idle: i64, inactive: Option<i64>) -> Value {
        let mut f = vec![
            ("name", b(name)),
            ("pending", Value::Int(pending)),
            ("idle", Value::Int(idle)),
        ];
        if let Some(i) = inactive {
            f.push(("inactive", Value::Int(i)));
        }
        flat(f)
    }

    #[test]
    fn stream_reply_keeps_ids_and_no_values() {
        let reply = stream_reply(42, Some("1700000000000-0"));
        let info = parse_stream("events", &reply, None, Some(2048), 1_700_000_100_000);
        assert_eq!(info.key, "events");
        assert_eq!(info.length, 42);
        assert_eq!(info.bytes, Some(2048));
        assert_eq!(info.first_id.as_deref(), Some("1700000000000-0"));
        assert_eq!(info.last_id, "1700000009999-3");
        assert_eq!(info.oldest_age_ms, Some(100_000));
        assert_eq!(info.entries_added, Some(47));
        assert_eq!(info.groups, None);
        // No entry value anywhere in the parsed type, nor in its Debug output.
        let shown = format!("{info:?}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("password"),
            "{shown}"
        );
    }

    #[test]
    fn empty_stream_has_no_oldest_entry() {
        let info = parse_stream("s", &stream_reply(0, None), None, None, 5);
        assert_eq!(info.first_id, None);
        assert_eq!(info.oldest_age_ms, None);
    }

    #[test]
    fn redis_6_2_stream_has_no_entries_added() {
        let reply = flat(vec![
            ("length", Value::Int(3)),
            ("radix-tree-keys", Value::Int(1)),
            ("radix-tree-nodes", Value::Int(2)),
            ("last-generated-id", b("5-0")),
            ("groups", Value::Int(0)),
            ("first-entry", entry("1-0")),
            ("last-entry", entry("5-0")),
        ]);
        let info = parse_stream("s", &reply, None, None, 10);
        assert_eq!(info.entries_added, None);
        assert_eq!(info.oldest_age_ms, Some(9));
    }

    #[test]
    fn resp3_maps_parse_like_flat_arrays() {
        let map = Value::Map(vec![
            (b("length"), Value::Int(7)),
            (b("last-generated-id"), b("9-9")),
            (b("first-entry"), entry("2-0")),
        ]);
        let info = parse_stream("s", &map, None, None, 12);
        assert_eq!((info.length, info.last_id.as_str()), (7, "9-9"));
        assert_eq!(info.oldest_age_ms, Some(10));
    }

    #[test]
    fn group_lag_is_version_dependent() {
        let v = Value::Array(vec![
            group("known", 1, Some(Value::Int(12))),
            group("unknown", 0, Some(Value::Nil)),
            group("old", 0, None),
        ]);
        let gs = parse_groups(&v);
        assert_eq!(gs[0].lag, Lag::Known(12));
        assert_eq!(gs[1].lag, Lag::Unknown);
        assert_eq!(gs[2].lag, Lag::Unsupported);
        assert_eq!(gs[0].name, "known");
        assert_eq!(gs[0].pending, 1);
        assert_eq!(gs[0].last_delivered_id, "1700000005000-0");
        assert_eq!(gs[0].consumers, 2);
    }

    #[test]
    fn consumer_inactive_is_version_dependent() {
        let v = Value::Array(vec![
            consumer("new", 3, 1000, Some(2000)),
            consumer("old", 0, 5, None),
        ]);
        let cs = parse_consumers(&v);
        assert_eq!(cs[0].inactive_ms, Some(2000));
        assert_eq!(cs[1].inactive_ms, None);
        assert_eq!(
            (cs[0].name.as_str(), cs[0].pending, cs[0].idle_ms),
            ("new", 3, 1000)
        );
    }

    #[test]
    fn group_names_keep_their_bytes() {
        let v = Value::Array(vec![flat(vec![(
            "name",
            Value::BulkString(vec![0xff, b'a']),
        )])]);
        assert_eq!(group_names(Some(&v)), vec![vec![0xff, b'a']]);
    }

    #[test]
    fn ids() {
        assert_eq!(id_millis("1700000000123-5"), Some(1_700_000_000_123));
        assert_eq!(id_millis("17"), Some(17));
        assert_eq!(id_millis("x-1"), None);
        assert_eq!(id_millis(""), None);
        assert_eq!(id_millis("-1"), None);
    }

    fn info_with(length: u64, age_ms: Option<u64>, groups: Option<Vec<GroupInfo>>) -> StreamInfo {
        StreamInfo {
            key: "s".into(),
            length,
            bytes: None,
            first_id: None,
            last_id: String::new(),
            oldest_age_ms: age_ms,
            entries_added: None,
            groups,
            notes: Vec::new(),
        }
    }

    fn grp(lag: Lag, readers: Option<Vec<ConsumerInfo>>) -> GroupInfo {
        GroupInfo {
            name: "g".into(),
            consumers: readers.as_ref().map_or(0, |r| r.len() as u64),
            pending: 0,
            last_delivered_id: "0-0".into(),
            lag,
            readers,
        }
    }

    fn cons(pending: u64, idle_ms: u64) -> ConsumerInfo {
        ConsumerInfo {
            name: "c".into(),
            pending,
            idle_ms,
            inactive_ms: None,
        }
    }

    #[test]
    fn stuck_needs_pending_entries_and_30_minutes_idle() {
        let v = |pending, idle| {
            verdicts(&info_with(
                1,
                None,
                Some(vec![grp(Lag::Known(0), Some(vec![cons(pending, idle)]))]),
            ))
        };
        assert_eq!(v(1, STUCK_IDLE_MS), [Verdict::Stuck]);
        assert_eq!(v(1, STUCK_IDLE_MS - 1), [Verdict::Ok]);
        assert_eq!(v(0, STUCK_IDLE_MS * 10), [Verdict::Ok]);
    }

    #[test]
    fn lagging_is_a_lag_of_1000() {
        let v = |lag| verdicts(&info_with(1, None, Some(vec![grp(lag, Some(vec![]))])));
        assert_eq!(v(Lag::Known(LAGGING_LAG)), [Verdict::Lagging]);
        assert_eq!(v(Lag::Known(LAGGING_LAG - 1)), [Verdict::Ok]);
        // A lag the server cannot work out, or does not report, says nothing.
        assert_eq!(v(Lag::Unknown), [Verdict::Ok]);
        assert_eq!(v(Lag::Unsupported), [Verdict::Ok]);
    }

    #[test]
    fn no_readers_is_no_groups() {
        assert_eq!(
            verdicts(&info_with(1, None, Some(vec![]))),
            [Verdict::NoReaders]
        );
        // A group without consumers is still a group.
        assert_eq!(
            verdicts(&info_with(
                1,
                None,
                Some(vec![grp(Lag::Known(0), Some(vec![]))])
            )),
            [Verdict::Ok]
        );
        // XINFO GROUPS refused: unknown, so no verdict.
        assert_eq!(verdicts(&info_with(1, None, None)), [Verdict::Ok]);
    }

    #[test]
    fn unbounded_needs_both_age_and_length() {
        let v = |len, age| verdicts(&info_with(len, age, Some(vec![grp(Lag::Known(0), None)])));
        assert_eq!(
            v(UNBOUNDED_LEN, Some(UNBOUNDED_AGE_MS + 1)),
            [Verdict::Unbounded]
        );
        assert_eq!(
            v(UNBOUNDED_LEN - 1, Some(UNBOUNDED_AGE_MS + 1)),
            [Verdict::Ok]
        );
        assert_eq!(v(UNBOUNDED_LEN, Some(UNBOUNDED_AGE_MS)), [Verdict::Ok]);
        assert_eq!(v(UNBOUNDED_LEN, None), [Verdict::Ok]);
    }

    #[test]
    fn verdicts_come_in_the_specs_order() {
        let s = info_with(
            UNBOUNDED_LEN,
            Some(UNBOUNDED_AGE_MS + 1),
            Some(vec![grp(
                Lag::Known(5000),
                Some(vec![cons(2, STUCK_IDLE_MS)]),
            )]),
        );
        assert_eq!(
            verdicts(&s),
            [Verdict::Stuck, Verdict::Lagging, Verdict::Unbounded]
        );
        let s = info_with(UNBOUNDED_LEN, Some(UNBOUNDED_AGE_MS + 1), Some(vec![]));
        assert_eq!(verdicts(&s), [Verdict::NoReaders, Verdict::Unbounded]);
    }

    fn value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Nil),
            any::<i64>().prop_map(Value::Int),
            proptest::collection::vec(any::<u8>(), 0..12).prop_map(Value::BulkString),
            "[a-z-]{0,12}".prop_map(Value::SimpleString),
            any::<f64>().prop_map(Value::Double),
        ];
        leaf.prop_recursive(4, 48, 6, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                proptest::collection::vec((inner.clone(), inner), 0..4).prop_map(Value::Map),
            ]
        })
    }

    proptest! {
        #[test]
        fn parsers_never_panic(v in value(), g in value(), now in any::<u64>()) {
            let info = parse_stream("k", &v, Some(&g), None, now);
            let _ = verdicts(&info);
            let _ = parse_groups(&g);
            let _ = parse_consumers(&v);
            let _ = group_names(Some(&g));
        }

        #[test]
        fn an_id_roundtrips(ms in any::<u64>(), seq in any::<u64>()) {
            prop_assert_eq!(id_millis(&format!("{ms}-{seq}")), Some(ms));
        }

        #[test]
        fn age_never_underflows(ms in any::<u64>(), now in any::<u64>()) {
            let reply = flat(vec![
                ("length", Value::Int(1)),
                ("first-entry", entry(&format!("{ms}-0"))),
            ]);
            let info = parse_stream("k", &reply, None, None, now);
            prop_assert_eq!(info.oldest_age_ms, Some(now.saturating_sub(ms)));
        }

        #[test]
        fn text_is_never_kept_from_an_entry(secret in "[a-z]{8,16}") {
            let reply = flat(vec![
                ("length", Value::Int(1)),
                ("first-entry", Value::Array(vec![b("1-0"), flat(vec![("f", b(&secret))])])),
                ("last-entry", Value::Array(vec![b("2-0"), flat(vec![("f", b(&secret))])])),
            ]);
            let info = parse_stream("k", &reply, None, None, 10);
            let shown = format!("{info:?}");
            prop_assert!(!shown.contains(&secret));
        }
    }

    #[test]
    fn failures_are_told_apart() {
        let err = |kind, code: &'static str, detail: &str| {
            RedisError::from((redis::ErrorKind::Server(kind), code, detail.to_string()))
        };
        assert!(matches!(
            failure(
                "XINFO STREAM",
                &err(ServerErrorKind::ResponseError, "ERR", "no such key")
            ),
            Failure::Gone
        ));
        assert!(matches!(
            failure(
                "XINFO STREAM",
                &redis::make_extension_error(
                    "WRONGTYPE".into(),
                    Some("Operation against a key holding the wrong kind of value".into())
                )
            ),
            Failure::Gone
        ));
        assert!(matches!(
            failure(
                "XINFO STREAM",
                &err(
                    ServerErrorKind::NoPerm,
                    "NOPERM",
                    "User lens-ro has no permissions"
                )
            ),
            Failure::Error(LiveError::Denied(_))
        ));
        let unknown = err(
            ServerErrorKind::ResponseError,
            "ERR",
            "unknown subcommand 'GROUPS'",
        );
        assert!(super::super::is_unknown_command(&unknown));
        assert!(matches!(
            failure("XINFO GROUPS", &unknown),
            Failure::Error(LiveError::Unsupported(_))
        ));
        // The note for a soft failure is the fixed text, never the server's.
        let note = error_for(
            "XINFO CONSUMERS",
            &err(
                ServerErrorKind::NoPerm,
                "NOPERM",
                "User lens-ro has no permissions",
            ),
        )
        .to_string();
        assert_eq!(note, "the server refused XINFO CONSUMERS for this user");
    }
}
