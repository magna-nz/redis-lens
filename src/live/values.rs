//! Live requests for the key detail screen (spec "Key detail"): a key's value, read in
//! pages through the shared budget.
//!
//! Only these read commands are sent, never a whole-key read (`HGETALL`, `SMEMBERS`,
//! `LRANGE 0 -1`, …) and never a write:
//!
//! - the first page of every key: `TYPE` first, in one pipeline with the page's
//!   commands for the type the key list saw. When `TYPE` says another type, the page
//!   is read again for that type; `none` means the key is gone;
//! - string: `STRLEN` and `GETRANGE key 0 65535` (the first [`STRING_BYTES`]);
//! - hash: `HSCAN key cursor COUNT 100`, then `HTTL key FIELDS n …` for that page's
//!   fields when field TTLs are on ([`field_ttls_supported`]);
//! - list: `LRANGE key i i+99`; set: `SSCAN key cursor COUNT 100`; sorted set:
//!   `ZRANGE key i i+99 WITHSCORES`;
//! - stream: `XREVRANGE key + - COUNT 100`, then `XREVRANGE key (<last id> - COUNT 100`;
//! - JSON: `JSON.GET key`, only when the key's `MEMORY USAGE` is known and at most
//!   [`JSON_MAX_SIZE`]; the document kept is cut at [`JSON_SHOWN`];
//! - vector set: `VRANDMEMBER key 100`, once;
//! - any other type: nothing.
//!
//! Values, field names, members and elements are held as [`Raw`], whose `Debug` shows
//! only a length, so nothing here can put a value into `Debug` output, an error or a
//! log line. Errors are redis-lens's own words ([`error_for`]).

use redis::Value;

use super::{LiveConn, LiveError, error_for, query, query_pipe};
use crate::budget::Budget;
use crate::model::{KeyType, ServerFlavor, Ttl};
use crate::value::Raw;

/// Items per page: the `COUNT` of `HSCAN` and `SSCAN`, the span of `LRANGE` and
/// `ZRANGE`, the `COUNT` of `XREVRANGE`.
pub const PAGE_ITEMS: usize = 100;
/// The most of a string `GETRANGE` reads: the first 64 KiB.
pub const STRING_BYTES: usize = 64 * 1024;
/// `JSON.GET` is sent only for keys whose `MEMORY USAGE` is known and at most this.
pub const JSON_MAX_SIZE: u64 = 256 * 1024;
/// The most of a JSON document that is kept and shown.
pub const JSON_SHOWN: usize = 64 * 1024;
/// The most items the screen holds. Past it, `n` loads no more.
pub const MAX_ITEMS: usize = 10_000;
/// How many random elements `VRANDMEMBER` asks for.
pub const VECTOR_SAMPLE: usize = 100;

/// The type of a key as its value is read. From `TYPE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    /// `ReJSON-RL`.
    Json,
    /// `vectorset`.
    Vector,
    /// Any other type, with its `TYPE` reply: its value is not read.
    Other(String),
}

impl Kind {
    /// From a `TYPE` reply.
    pub fn from_type(reply: &str) -> Self {
        match reply {
            "string" => Self::String,
            "hash" => Self::Hash,
            "list" => Self::List,
            "set" => Self::Set,
            "zset" => Self::ZSet,
            "stream" => Self::Stream,
            "ReJSON-RL" => Self::Json,
            "vectorset" => Self::Vector,
            other => Self::Other(other.to_string()),
        }
    }

    /// The command that reads the value, for the panel title. `None` for other types.
    pub fn command(&self) -> Option<&'static str> {
        Some(match self {
            Self::String => "GETRANGE",
            Self::Hash => "HSCAN",
            Self::List => "LRANGE",
            Self::Set => "SSCAN",
            Self::ZSet => "ZRANGE",
            Self::Stream => "XREVRANGE",
            Self::Json => "JSON.GET",
            Self::Vector => "VRANDMEMBER",
            Self::Other(_) => return None,
        })
    }
}

impl From<KeyType> for Kind {
    fn from(t: KeyType) -> Self {
        match t {
            KeyType::String => Self::String,
            KeyType::Hash => Self::Hash,
            KeyType::List => Self::List,
            KeyType::Set => Self::Set,
            KeyType::ZSet => Self::ZSet,
            KeyType::Stream => Self::Stream,
            KeyType::Json => Self::Json,
            KeyType::Other => Self::Other("other".to_string()),
        }
    }
}

/// Where a next page starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum At {
    /// The `HSCAN` or `SSCAN` cursor.
    Cursor(u64),
    /// The first index of `LRANGE` or `ZRANGE`.
    Index(u64),
    /// The last stream entry ID read: the next `XREVRANGE` continues before it.
    Before(String),
}

/// One page of a key's value.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub key: String,
    /// The first page (`at` is `None`): the type the key list saw, whose commands are
    /// pipelined with `TYPE`. A next page: the type the first page's `TYPE` said.
    pub kind: Kind,
    /// `None` for the first page.
    pub at: Option<At>,
    /// Send `HTTL` for a hash page's fields (Redis 7.4 and later, Valkey 9.0 and
    /// later; see [`field_ttls_supported`]).
    pub field_ttls: bool,
    /// The key's `MEMORY USAGE` from the key list: `JSON.GET` needs it known and at
    /// most [`JSON_MAX_SIZE`].
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// `TYPE` said `none`: the key no longer exists.
    Gone,
    Page(Page),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// The `TYPE` reply, on the first page only. Its type is the one read.
    pub type_name: Option<String>,
    pub items: Items,
    /// Where the next page starts; `None` when this was the last.
    pub next: Option<At>,
    /// Commands the server refused or lacks that only leave a part out (`STRLEN`,
    /// `HTTL`), once each, in redis-lens's own words.
    pub refused: Vec<LiveError>,
}

/// What a page read, by type.
#[derive(Debug, Clone, PartialEq)]
pub enum Items {
    /// The first [`STRING_BYTES`] of a string, and its whole length (`STRLEN`; `None`
    /// when refused).
    String {
        value: Raw,
        len: Option<u64>,
    },
    Hash(Vec<Field>),
    /// Elements in index order, from the page's first index.
    List(Vec<Raw>),
    Set(Vec<Raw>),
    ZSet(Vec<Member>),
    /// Newest first.
    Stream(Vec<Entry>),
    /// The document, cut at [`JSON_SHOWN`], and its whole length in bytes.
    Json {
        doc: Raw,
        len: usize,
    },
    /// `JSON.GET` was not sent: the size is unknown or over [`JSON_MAX_SIZE`].
    JsonNotRead {
        size: Option<u64>,
    },
    /// [`VECTOR_SAMPLE`] random elements of a vector set.
    Vector(Vec<Raw>),
    /// A type whose value redis-lens does not read.
    NotRead,
}

impl Items {
    /// How many items it holds: 1 for a string or a JSON document, 0 when not read.
    pub fn len(&self) -> usize {
        match self {
            Self::String { .. } | Self::Json { .. } => 1,
            Self::Hash(v) => v.len(),
            Self::List(v) | Self::Set(v) | Self::Vector(v) => v.len(),
            Self::ZSet(v) => v.len(),
            Self::Stream(v) => v.len(),
            Self::JsonNotRead { .. } | Self::NotRead => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A hash field.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: Raw,
    pub value: Raw,
    /// From `HTTL`; `None` when not asked, refused, or the field has gone.
    pub ttl: Option<Ttl>,
}

/// A sorted set member and its score as the server wrote it.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub member: Raw,
    pub score: Raw,
}

/// A stream entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub fields: Vec<(Raw, Raw)>,
}

/// Whether `HTTL` exists: Redis 7.4 and later, Valkey 9.0 and later (spec "Server
/// versions"). The version is read leniently: the first two numbers, whatever follows.
pub fn field_ttls_supported(flavor: ServerFlavor, version: &str) -> bool {
    let Some((major, minor)) = major_minor(version) else {
        return false;
    };
    match flavor {
        ServerFlavor::Redis => (major, minor) >= (7, 4),
        ServerFlavor::Valkey => major >= 9,
        ServerFlavor::Unknown => false,
    }
}

/// `7.4.2` → (7, 4); `8` → (8, 0); `v9.0-rc1` → (9, 0).
fn major_minor(version: &str) -> Option<(u64, u64)> {
    let mut nums = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().ok());
    let major = nums.next()??;
    let minor = nums.next().flatten().unwrap_or(0);
    Some((major, minor))
}

/// Runs one request. Every command goes through `budget`.
pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    let mut refused = Vec::new();
    let (type_name, kind, items, next) = match &req.at {
        None => {
            let mut pipe = redis::pipe();
            pipe.ignore_errors();
            pipe.cmd("TYPE").arg(&req.key);
            let hinted = page_cmds(&req.key, &req.kind, None, req.size);
            for (_, c) in &hinted {
                pipe.add_command(c.clone());
            }
            let replies: Vec<Value> = query_pipe(conn, budget, "TYPE", &pipe).await?;
            let Some((first, rest)) = replies.split_first() else {
                return Err(incomplete());
            };
            let t =
                text(ok("TYPE", first)?).ok_or_else(|| LiveError::Failed("TYPE failed".into()))?;
            if t == "none" {
                return Ok(Reply::Gone);
            }
            let kind = Kind::from_type(&t);
            let (items, next) = if kind == req.kind {
                parse(&kind, &hinted, rest, None, &mut refused)?
            } else {
                read_page(conn, budget, &req.key, &kind, None, req.size, &mut refused).await?
            };
            (Some(t), kind, items, next)
        }
        Some(at) => {
            let (items, next) = read_page(
                conn,
                budget,
                &req.key,
                &req.kind,
                Some(at),
                req.size,
                &mut refused,
            )
            .await?;
            (None, req.kind.clone(), items, next)
        }
    };
    let items = match items {
        Items::Hash(fields) if kind == Kind::Hash && req.field_ttls && !fields.is_empty() => {
            Items::Hash(field_ttls(conn, budget, &req.key, fields, &mut refused).await?)
        }
        other => other,
    };
    Ok(Reply::Page(Page {
        type_name,
        items,
        next,
        refused,
    }))
}

/// Sends a page's commands for `kind` on their own and reads them.
async fn read_page(
    conn: &mut impl LiveConn,
    budget: &Budget,
    key: &str,
    kind: &Kind,
    at: Option<&At>,
    size: Option<u64>,
    refused: &mut Vec<LiveError>,
) -> Result<(Items, Option<At>), LiveError> {
    let cmds = page_cmds(key, kind, at, size);
    if cmds.is_empty() {
        return parse(kind, &cmds, &[], at, refused);
    }
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    for (_, c) in &cmds {
        pipe.add_command(c.clone());
    }
    let name = kind.command().unwrap_or("the value");
    let replies: Vec<Value> = query_pipe(conn, budget, name, &pipe).await?;
    parse(kind, &cmds, &replies, at, refused)
}

/// The commands of one page, with the name a failure of each is called.
fn page_cmds(
    key: &str,
    kind: &Kind,
    at: Option<&At>,
    size: Option<u64>,
) -> Vec<(&'static str, redis::Cmd)> {
    let cursor = match at {
        Some(At::Cursor(c)) => *c,
        _ => 0,
    };
    let index = match at {
        Some(At::Index(i)) => *i,
        _ => 0,
    };
    let last = index + PAGE_ITEMS as u64 - 1;
    let mut out = Vec::new();
    match kind {
        Kind::String => {
            let mut len = redis::cmd("STRLEN");
            len.arg(key);
            let mut range = redis::cmd("GETRANGE");
            range.arg(key).arg(0).arg(STRING_BYTES - 1);
            out.push(("STRLEN", len));
            out.push(("GETRANGE", range));
        }
        Kind::Hash => {
            let mut c = redis::cmd("HSCAN");
            c.arg(key).arg(cursor).arg("COUNT").arg(PAGE_ITEMS);
            out.push(("HSCAN", c));
        }
        Kind::Set => {
            let mut c = redis::cmd("SSCAN");
            c.arg(key).arg(cursor).arg("COUNT").arg(PAGE_ITEMS);
            out.push(("SSCAN", c));
        }
        Kind::List => {
            let mut c = redis::cmd("LRANGE");
            c.arg(key).arg(index).arg(last);
            out.push(("LRANGE", c));
        }
        Kind::ZSet => {
            let mut c = redis::cmd("ZRANGE");
            c.arg(key).arg(index).arg(last).arg("WITHSCORES");
            out.push(("ZRANGE", c));
        }
        Kind::Stream => {
            let mut c = redis::cmd("XREVRANGE");
            c.arg(key);
            match at {
                Some(At::Before(id)) => c.arg(format!("({id}")),
                _ => c.arg("+"),
            };
            c.arg("-").arg("COUNT").arg(PAGE_ITEMS);
            out.push(("XREVRANGE", c));
        }
        Kind::Json => {
            if json_size_ok(size) {
                let mut c = redis::cmd("JSON.GET");
                c.arg(key);
                out.push(("JSON.GET", c));
            }
        }
        Kind::Vector => {
            let mut c = redis::cmd("VRANDMEMBER");
            c.arg(key).arg(VECTOR_SAMPLE);
            out.push(("VRANDMEMBER", c));
        }
        Kind::Other(_) => {}
    }
    out
}

fn incomplete() -> LiveError {
    LiveError::Failed("the value came back incomplete".to_string())
}

fn shape(name: &str) -> LiveError {
    LiveError::Failed(format!("{name} came back in an unexpected shape"))
}

/// `v`, or the error it holds in redis-lens's words.
fn ok<'a>(name: &str, v: &'a Value) -> Result<&'a Value, LiveError> {
    match v {
        Value::ServerError(e) => Err(error_for(name, &redis::RedisError::from(e.clone()))),
        v => Ok(v),
    }
}

/// The bytes of a string-like reply.
fn bytes(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::BulkString(b) => Some(b.clone()),
        Value::SimpleString(s) => Some(s.as_bytes().to_vec()),
        Value::VerbatimString { text, .. } => Some(text.as_bytes().to_vec()),
        Value::Int(n) => Some(n.to_string().into_bytes()),
        Value::Double(d) => Some(d.to_string().into_bytes()),
        Value::Okay => Some(b"OK".to_vec()),
        _ => None,
    }
}

fn text(v: &Value) -> Option<String> {
    String::from_utf8(bytes(v)?).ok()
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        v => text(v)?.parse().ok(),
    }
}

fn array(v: &Value) -> Option<&[Value]> {
    match v {
        Value::Array(a) | Value::Set(a) => Some(a),
        _ => None,
    }
}

/// Key and value pairs: a flat array, an array of pairs, or a map.
fn pairs(name: &str, v: &Value) -> Result<Vec<(Raw, Raw)>, LiveError> {
    let pair = |k: &Value, v: &Value| -> Result<(Raw, Raw), LiveError> {
        Ok((
            Raw::new(bytes(k).ok_or_else(|| shape(name))?),
            Raw::new(bytes(v).ok_or_else(|| shape(name))?),
        ))
    };
    match v {
        Value::Map(m) => m.iter().map(|(k, v)| pair(k, v)).collect(),
        Value::Nil => Ok(Vec::new()),
        v => {
            let a = array(v).ok_or_else(|| shape(name))?;
            if a.iter().all(|x| array(x).is_some()) && !a.is_empty() {
                a.iter()
                    .map(|x| match array(x) {
                        Some([k, v]) => pair(k, v),
                        _ => Err(shape(name)),
                    })
                    .collect()
            } else {
                let (chunks, rest) = a.as_chunks::<2>();
                if !rest.is_empty() {
                    return Err(shape(name));
                }
                chunks.iter().map(|[k, v]| pair(k, v)).collect()
            }
        }
    }
}

/// A `SCAN`-style reply: the next cursor and the items.
fn scan_reply<'a>(name: &str, v: &'a Value) -> Result<(u64, &'a Value), LiveError> {
    match array(v) {
        Some([cursor, items]) => {
            let c = text(cursor)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| shape(name))?;
            Ok((c, items))
        }
        _ => Err(shape(name)),
    }
}

fn raws(name: &str, v: &Value) -> Result<Vec<Raw>, LiveError> {
    if matches!(v, Value::Nil) {
        return Ok(Vec::new());
    }
    array(v)
        .ok_or_else(|| shape(name))?
        .iter()
        .map(|x| bytes(x).map(Raw::new).ok_or_else(|| shape(name)))
        .collect()
}

/// Reads the replies of [`page_cmds`]. The main read's error fails the request; a
/// refused `STRLEN` only leaves the length out.
fn parse(
    kind: &Kind,
    cmds: &[(&'static str, redis::Cmd)],
    replies: &[Value],
    at: Option<&At>,
    refused: &mut Vec<LiveError>,
) -> Result<(Items, Option<At>), LiveError> {
    if replies.len() != cmds.len() {
        return Err(incomplete());
    }
    let index = match at {
        Some(At::Index(i)) => *i,
        _ => 0,
    };
    let full = |n: usize| n >= PAGE_ITEMS;
    Ok(match kind {
        Kind::String => {
            let len = match ok("STRLEN", &replies[0]) {
                Ok(v) => int(v).and_then(|n| u64::try_from(n).ok()),
                Err(e) => {
                    note(refused, e);
                    None
                }
            };
            let v = ok("GETRANGE", &replies[1])?;
            let value = match v {
                Value::Nil => Vec::new(),
                v => bytes(v).ok_or_else(|| shape("GETRANGE"))?,
            };
            (
                Items::String {
                    value: Raw::new(value),
                    len,
                },
                None,
            )
        }
        Kind::Hash => {
            let (cursor, items) = scan_reply("HSCAN", ok("HSCAN", &replies[0])?)?;
            let fields = pairs("HSCAN", items)?
                .into_iter()
                .map(|(name, value)| Field {
                    name,
                    value,
                    ttl: None,
                })
                .collect();
            (
                Items::Hash(fields),
                (cursor != 0).then_some(At::Cursor(cursor)),
            )
        }
        Kind::Set => {
            let (cursor, items) = scan_reply("SSCAN", ok("SSCAN", &replies[0])?)?;
            (
                Items::Set(raws("SSCAN", items)?),
                (cursor != 0).then_some(At::Cursor(cursor)),
            )
        }
        Kind::List => {
            let items = raws("LRANGE", ok("LRANGE", &replies[0])?)?;
            let next = full(items.len()).then_some(At::Index(index + items.len() as u64));
            (Items::List(items), next)
        }
        Kind::ZSet => {
            let members: Vec<Member> = pairs("ZRANGE", ok("ZRANGE", &replies[0])?)?
                .into_iter()
                .map(|(member, score)| Member { member, score })
                .collect();
            let next = full(members.len()).then_some(At::Index(index + members.len() as u64));
            (Items::ZSet(members), next)
        }
        Kind::Stream => {
            let v = ok("XREVRANGE", &replies[0])?;
            let mut entries = Vec::new();
            for e in array(v).ok_or_else(|| shape("XREVRANGE"))? {
                let [id, fields] = array(e).ok_or_else(|| shape("XREVRANGE"))? else {
                    return Err(shape("XREVRANGE"));
                };
                entries.push(Entry {
                    id: text(id).ok_or_else(|| shape("XREVRANGE"))?,
                    fields: pairs("XREVRANGE", fields)?,
                });
            }
            let next = entries
                .last()
                .filter(|_| full(entries.len()))
                .map(|e| At::Before(e.id.clone()));
            (Items::Stream(entries), next)
        }
        Kind::Json => match replies.first() {
            None => (Items::JsonNotRead { size: None }, None),
            Some(v) => {
                let mut doc = match ok("JSON.GET", v)? {
                    Value::Nil => Vec::new(),
                    v => bytes(v).ok_or_else(|| shape("JSON.GET"))?,
                };
                let len = doc.len();
                doc.truncate(JSON_SHOWN);
                (
                    Items::Json {
                        doc: Raw::new(doc),
                        len,
                    },
                    None,
                )
            }
        },
        Kind::Vector => (
            Items::Vector(raws("VRANDMEMBER", ok("VRANDMEMBER", &replies[0])?)?),
            None,
        ),
        Kind::Other(_) => (Items::NotRead, None),
    })
}

/// Notes a refused or unknown command once; other failures leave only that part out.
fn note(refused: &mut Vec<LiveError>, e: LiveError) {
    if matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_)) && !refused.contains(&e) {
        refused.push(e);
    }
}

/// True when `JSON.GET` is sent for a key of this `MEMORY USAGE`: known and at most
/// [`JSON_MAX_SIZE`].
pub fn json_size_ok(size: Option<u64>) -> bool {
    size.is_some_and(|s| s <= JSON_MAX_SIZE)
}

/// `HTTL key FIELDS n …` for a page's fields.
async fn field_ttls(
    conn: &mut impl LiveConn,
    budget: &Budget,
    key: &str,
    mut fields: Vec<Field>,
    refused: &mut Vec<LiveError>,
) -> Result<Vec<Field>, LiveError> {
    let mut cmd = redis::cmd("HTTL");
    cmd.arg(key).arg("FIELDS").arg(fields.len());
    for f in &fields {
        cmd.arg(f.name.as_bytes());
    }
    match query::<Value>(conn, budget, "HTTL", &cmd).await {
        Ok(v) => {
            let ttls = array(&v).unwrap_or(&[]);
            for (f, t) in fields.iter_mut().zip(ttls) {
                f.ttl = int(t).and_then(ttl_of);
            }
        }
        Err(LiveError::Closed) => return Err(LiveError::Closed),
        Err(e) => note(refused, e),
    }
    Ok(fields)
}

/// An `HTTL` reply for one field: -2 no such field, -1 no TTL, else seconds.
fn ttl_of(n: i64) -> Option<Ttl> {
    match n {
        -1 => Some(Ttl::Persistent),
        n if n >= 0 => Some(Ttl::Expires(std::time::Duration::from_secs(n as u64))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn server_error(line: &str) -> Value {
        let v = redis::parse_redis_value(format!("-{line}\r\n").as_bytes()).unwrap();
        assert!(matches!(v, Value::ServerError(_)), "{v:?}");
        v
    }

    #[test]
    fn field_ttls_need_redis_7_4_or_valkey_9() {
        use ServerFlavor::*;
        for (flavor, version, want) in [
            (Redis, "7.4.2", true),
            (Redis, "7.4", true),
            (Redis, "8.0.0", true),
            (Redis, "7.2.5", false),
            (Redis, "6.2.14", false),
            (Redis, "7", false),
            (Valkey, "8.1.3", false),
            (Valkey, "9.0.0-rc1", true),
            (Valkey, "9", true),
            (Unknown, "9.0.0", false),
            (Redis, "", false),
            (Redis, "unknown", false),
            (Redis, "v7.4.0", true),
        ] {
            assert_eq!(
                field_ttls_supported(flavor, version),
                want,
                "{flavor:?} {version}"
            );
        }
    }

    #[test]
    fn page_commands_by_type() {
        let words = |kind: Kind, at: Option<At>, size: Option<u64>| -> Vec<String> {
            page_cmds("k", &kind, at.as_ref(), size)
                .iter()
                .map(|(_, c)| {
                    c.args_iter()
                        .map(|a| match a {
                            redis::Arg::Simple(b) => String::from_utf8_lossy(b).into_owned(),
                            _ => "<cursor>".into(),
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect()
        };
        assert_eq!(
            words(Kind::String, None, None),
            ["STRLEN k", "GETRANGE k 0 65535"]
        );
        assert_eq!(
            words(Kind::Hash, Some(At::Cursor(17)), None),
            ["HSCAN k 17 COUNT 100"]
        );
        assert_eq!(words(Kind::Set, None, None), ["SSCAN k 0 COUNT 100"]);
        assert_eq!(
            words(Kind::List, Some(At::Index(100)), None),
            ["LRANGE k 100 199"]
        );
        assert_eq!(words(Kind::ZSet, None, None), ["ZRANGE k 0 99 WITHSCORES"]);
        assert_eq!(
            words(Kind::Stream, None, None),
            ["XREVRANGE k + - COUNT 100"]
        );
        assert_eq!(
            words(Kind::Stream, Some(At::Before("5-1".into())), None),
            ["XREVRANGE k (5-1 - COUNT 100"]
        );
        assert_eq!(words(Kind::Json, None, Some(1_000)), ["JSON.GET k"]);
        assert!(words(Kind::Json, None, Some(JSON_MAX_SIZE + 1)).is_empty());
        assert!(words(Kind::Json, None, None).is_empty());
        assert_eq!(words(Kind::Vector, None, None), ["VRANDMEMBER k 100"]);
        assert!(words(Kind::Other("x".into()), None, None).is_empty());
    }

    #[test]
    fn replies_parse_in_both_protocols() {
        let mut refused = Vec::new();
        let hash = Value::Array(vec![
            bulk("12"),
            Value::Array(vec![bulk("f1"), bulk("v1"), bulk("f2"), bulk("v2")]),
        ]);
        let cmds = page_cmds("k", &Kind::Hash, None, None);
        let (items, next) = parse(&Kind::Hash, &cmds, &[hash], None, &mut refused).unwrap();
        assert_eq!(next, Some(At::Cursor(12)));
        let Items::Hash(f) = items else { panic!() };
        assert_eq!(f[1].name, Raw::from("f2"));
        assert_eq!(f[1].value, Raw::from("v2"));

        // ZRANGE WITHSCORES: RESP2 flat, RESP3 pairs with doubles.
        let cmds = page_cmds("k", &Kind::ZSet, None, None);
        let flat = Value::Array(vec![bulk("a"), bulk("1.5")]);
        let nested = Value::Array(vec![Value::Array(vec![bulk("a"), Value::Double(1.5)])]);
        for v in [flat, nested] {
            let (items, next) = parse(&Kind::ZSet, &cmds, &[v], None, &mut refused).unwrap();
            assert_eq!(next, None);
            assert_eq!(
                items,
                Items::ZSet(vec![Member {
                    member: Raw::from("a"),
                    score: Raw::from("1.5")
                }])
            );
        }

        // A full stream page continues before its last ID.
        let entries: Vec<Value> = (0..PAGE_ITEMS)
            .map(|i| {
                Value::Array(vec![
                    bulk(&format!("{}-0", 1000 - i)),
                    Value::Array(vec![bulk("f"), bulk("v")]),
                ])
            })
            .collect();
        let cmds = page_cmds("k", &Kind::Stream, None, None);
        let (_, next) = parse(
            &Kind::Stream,
            &cmds,
            &[Value::Array(entries)],
            None,
            &mut refused,
        )
        .unwrap();
        assert_eq!(next, Some(At::Before("901-0".into())));
        assert!(refused.is_empty());
    }

    #[test]
    fn a_refused_strlen_only_leaves_the_length_out_and_a_refused_read_fails() {
        let cmds = page_cmds("k", &Kind::String, None, None);
        let noperm =
            server_error("NOPERM User lens-ro has no permissions to run the 'strlen' command");
        let mut refused = Vec::new();
        let (items, _) = parse(
            &Kind::String,
            &cmds,
            &[noperm.clone(), bulk("s3cr3t-v4lue")],
            None,
            &mut refused,
        )
        .unwrap();
        assert_eq!(
            items,
            Items::String {
                value: Raw::from("s3cr3t-v4lue"),
                len: None
            }
        );
        assert_eq!(refused, [LiveError::Denied("STRLEN".into())]);
        let cmds = page_cmds("k", &Kind::Hash, None, None);
        let e = parse(&Kind::Hash, &cmds, &[noperm], None, &mut refused).unwrap_err();
        assert_eq!(e, LiveError::Denied("HSCAN".into()));
        assert!(!e.to_string().contains("lens-ro"));
    }

    #[test]
    fn json_is_cut_at_64_kib() {
        let cmds = page_cmds("k", &Kind::Json, None, Some(100));
        let big = Value::BulkString(vec![b'1'; JSON_SHOWN + 10]);
        let (items, _) = parse(&Kind::Json, &cmds, &[big], None, &mut Vec::new()).unwrap();
        let Items::Json { doc, len } = items else {
            panic!()
        };
        assert_eq!((doc.len(), len), (JSON_SHOWN, JSON_SHOWN + 10));
        let (items, _) = parse(&Kind::Json, &[], &[], None, &mut Vec::new()).unwrap();
        assert_eq!(items, Items::JsonNotRead { size: None });
    }

    #[test]
    fn field_ttl_replies() {
        assert_eq!(ttl_of(-2), None);
        assert_eq!(ttl_of(-1), Some(Ttl::Persistent));
        assert_eq!(
            ttl_of(60),
            Some(Ttl::Expires(std::time::Duration::from_secs(60)))
        );
    }

    #[test]
    fn debug_never_shows_a_value() {
        let page = Reply::Page(Page {
            type_name: Some("hash".into()),
            items: Items::Hash(vec![Field {
                name: Raw::from("s3cr3t-f1eld"),
                value: Raw::from("s3cr3t-v4lue"),
                ttl: None,
            }]),
            next: None,
            refused: Vec::new(),
        });
        let text = format!("{page:?}");
        assert!(!text.contains("s3cr3t"), "{text}");
    }
}
