//! Live requests for the key list and Big keys (spec "1 Patterns, additions", "Key list";
//! "2 Analysis"). Owner: wave B2. Phase 1: no writes, no values.
//!
//! Only metadata commands are sent: `SCAN`, `TYPE`, `PTTL`, `MEMORY USAGE`, `OBJECT
//! ENCODING`, `OBJECT FREQ` and the length commands (`STRLEN`, `HLEN`, `LLEN`, `SCARD`,
//! `ZCARD`, `XLEN`). Never `KEYS` and never a command that reads a value. Every command
//! goes through the shared budget ([`query`], [`query_pipe`]); per-key commands are
//! pipelined in batches of [`BATCH`] keys, like a scan.

use std::collections::HashSet;

use redis::{FromRedisValue, Value};

use super::{LiveConn, LiveError, error_for, query, query_pipe};
use crate::analysis::{KeyFacts, key_in_pattern, match_glob};
use crate::budget::Budget;
use crate::model::{KeyType, Ttl};

/// Keys a page of the key list loads: `n` loads this many more.
pub const PAGE_KEYS: usize = 1_000;
/// The `COUNT` hint of each `SCAN`.
pub const SCAN_COUNT: u64 = 1_000;
/// At most this many `SCAN` calls per page, so a pattern with few keys in a big
/// keyspace never holds the connection for long. The page then has fewer keys and the
/// walk goes on with the next `n`.
pub const MAX_SCAN_CALLS: usize = 500;
/// Keys per pipeline.
pub const BATCH: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// The next page of `pattern`'s key list: `SCAN cursor MATCH <glob> COUNT n` until
    /// [`PAGE_KEYS`] keys that infer to `pattern` are found (the glob also matches other
    /// patterns' keys, which are left out), the cursor returns to 0, or after
    /// [`MAX_SCAN_CALLS`] calls. Then the metadata of each key, pipelined.
    Page {
        pattern: String,
        /// Where the walk continues; 0 to start.
        cursor: u64,
        /// Keys found by the previous page but not loaded yet, loaded first.
        carry: Vec<String>,
        /// Send `MEMORY USAGE` (the scan was allowed it).
        sizes: bool,
        /// Send `OBJECT FREQ` (LFU policy, and the scan was allowed it).
        freq: bool,
    },
    /// The encoding of known keys, and the length of those with `length` set: for Big
    /// keys and Recommendations.
    Facts { keys: Vec<FactKey> },
}

/// A key for [`Request::Facts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactKey {
    pub key: String,
    /// Its type from the scan, which picks the length command.
    pub key_type: KeyType,
    /// Read its length too, not only its encoding.
    pub length: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Page(Page),
    Facts {
        /// One per key that still existed, in request order.
        facts: Vec<(String, KeyFacts)>,
        /// Commands the server refused or does not have.
        refused: Vec<LiveError>,
    },
}

/// One page of the key list.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub rows: Vec<KeyRow>,
    /// Where the walk goes on; 0 when it is done.
    pub cursor: u64,
    /// Keys found but not loaded yet: the next page loads them first.
    pub carry: Vec<String>,
    /// Keys the glob matched that infer to another pattern, left out.
    pub other_pattern: u64,
    /// Keys that matched but could not be read: `TYPE` or `PTTL` refused (a key ACL),
    /// or a name that is not UTF-8. Keys that vanished meanwhile are not counted.
    pub unreadable: u64,
    /// Commands the server refused or does not have, once each, in redis-lens's own
    /// words ([`LiveError::Denied`], [`LiveError::Unsupported`]). Their column is off.
    pub refused: Vec<LiveError>,
}

/// One key of the key list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub key: String,
    pub key_type: KeyType,
    pub ttl: Ttl,
    /// `MEMORY USAGE`; `None` when not sent or refused.
    pub bytes: Option<u64>,
    /// `OBJECT ENCODING`.
    pub encoding: Option<String>,
    /// `STRLEN`, `HLEN`, `LLEN`, `SCARD`, `ZCARD` or `XLEN`; `None` for other types.
    pub length: Option<u64>,
    /// `OBJECT FREQ`; `None` when not sent or refused.
    pub freq: Option<u8>,
}

/// The command that reads a key's length, by type. `None` for types without one.
pub fn length_command(t: KeyType) -> Option<&'static str> {
    match t {
        KeyType::String => Some("STRLEN"),
        KeyType::Hash => Some("HLEN"),
        KeyType::List => Some("LLEN"),
        KeyType::Set => Some("SCARD"),
        KeyType::ZSet => Some("ZCARD"),
        KeyType::Stream => Some("XLEN"),
        KeyType::Json | KeyType::Other => None,
    }
}

/// Runs one request. Every command goes through `budget`.
pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    match req {
        Request::Page {
            pattern,
            cursor,
            carry,
            sizes,
            freq,
        } => page(conn, budget, &pattern, cursor, carry, sizes, freq)
            .await
            .map(Reply::Page),
        Request::Facts { keys } => facts(conn, budget, &keys).await,
    }
}

/// Collects what a pipeline's per-command errors say: the commands refused or unknown,
/// once each. Other errors (`WRONGTYPE` after a key changed type, say) only leave that
/// value out.
#[derive(Debug, Default)]
struct Refused(Vec<LiveError>);

impl Refused {
    /// The value of a pipeline reply, or `None` after noting why it failed.
    fn read<T: FromRedisValue>(&mut self, name: &str, v: &Value) -> Option<T> {
        if let Value::ServerError(e) = v {
            let e = error_for(name, &redis::RedisError::from(e.clone()));
            if matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_)) && !self.0.contains(&e)
            {
                self.0.push(e);
            }
            return None;
        }
        T::from_redis_value(v.clone()).ok()
    }
}

#[allow(clippy::too_many_arguments)]
async fn page(
    conn: &mut impl LiveConn,
    budget: &Budget,
    pattern: &str,
    mut cursor: u64,
    carry: Vec<String>,
    sizes: bool,
    freq: bool,
) -> Result<Page, LiveError> {
    let glob = match_glob(pattern);
    let mut seen: HashSet<String> = carry.iter().cloned().collect();
    let mut found: Vec<String> = carry;
    let (mut other_pattern, mut unreadable) = (0u64, 0u64);
    // A page that starts from a carry with enough keys sends no SCAN; one that starts
    // at a finished walk (cursor 0 with a carry) does not walk again.
    let mut calls = 0;
    let walk = !(cursor == 0 && !found.is_empty());
    while walk && found.len() < PAGE_KEYS && calls < MAX_SCAN_CALLS {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor)
            .arg("MATCH")
            .arg(&glob)
            .arg("COUNT")
            .arg(SCAN_COUNT);
        let (next, keys): (u64, Vec<Vec<u8>>) = query(conn, budget, "SCAN", &cmd).await?;
        calls += 1;
        cursor = next;
        for raw in keys {
            match String::from_utf8(raw) {
                Ok(k) if !key_in_pattern(&k, pattern) => other_pattern += 1,
                Ok(k) => {
                    if seen.insert(k.clone()) {
                        found.push(k);
                    }
                }
                Err(_) => unreadable += 1,
            }
        }
        if cursor == 0 {
            break;
        }
    }
    let rest = found.split_off(found.len().min(PAGE_KEYS));
    let mut refused = Refused::default();
    let mut rows = Vec::with_capacity(found.len());
    for batch in found.chunks(BATCH) {
        unreadable += meta_batch(conn, budget, batch, sizes, freq, &mut refused, &mut rows).await?;
    }
    Ok(Page {
        rows,
        cursor,
        carry: rest,
        other_pattern,
        unreadable,
        refused: refused.0,
    })
}

/// `TYPE`, `PTTL`, `MEMORY USAGE`, `OBJECT ENCODING` and `OBJECT FREQ` for `keys` in one
/// pipeline, then the length of each in a second one. Pushes a row per key that still
/// exists and returns how many could not be read.
async fn meta_batch(
    conn: &mut impl LiveConn,
    budget: &Budget,
    keys: &[String],
    sizes: bool,
    freq: bool,
    refused: &mut Refused,
    rows: &mut Vec<KeyRow>,
) -> Result<u64, LiveError> {
    let stride = 3 + usize::from(sizes) + usize::from(freq);
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    for key in keys {
        pipe.cmd("TYPE").arg(key);
        pipe.cmd("PTTL").arg(key);
        if sizes {
            pipe.cmd("MEMORY")
                .arg("USAGE")
                .arg(key)
                .arg("SAMPLES")
                .arg(5);
        }
        pipe.cmd("OBJECT").arg("ENCODING").arg(key);
        if freq {
            pipe.cmd("OBJECT").arg("FREQ").arg(key);
        }
    }
    let replies: Vec<Value> = query_pipe(conn, budget, "the key metadata", &pipe).await?;
    if replies.len() != keys.len() * stride {
        return Err(LiveError::Failed(
            "the key metadata came back incomplete".to_string(),
        ));
    }
    let mut unreadable = 0;
    let mut batch: Vec<KeyRow> = Vec::with_capacity(keys.len());
    for (key, r) in keys.iter().zip(replies.chunks(stride)) {
        let Some(type_reply) = refused.read::<String>("TYPE", &r[0]) else {
            unreadable += 1;
            continue;
        };
        if type_reply == "none" {
            continue;
        }
        let Some(pttl) = refused.read::<i64>("PTTL", &r[1]) else {
            unreadable += 1;
            continue;
        };
        // -2: gone since TYPE.
        let Some(ttl) = Ttl::from_pttl(pttl) else {
            continue;
        };
        let mut at = 2;
        let bytes = if sizes {
            at += 1;
            refused.read::<u64>("MEMORY USAGE", &r[at - 1])
        } else {
            None
        };
        let encoding = refused.read::<String>("OBJECT ENCODING", &r[at]);
        at += 1;
        let freq = if freq {
            refused.read::<u8>("OBJECT FREQ", &r[at])
        } else {
            None
        };
        batch.push(KeyRow {
            key: key.clone(),
            key_type: KeyType::from_type_reply(&type_reply),
            ttl,
            bytes,
            encoding,
            length: None,
            freq,
        });
    }
    let typed: Vec<(String, KeyType)> = batch.iter().map(|r| (r.key.clone(), r.key_type)).collect();
    let lengths = lengths(conn, budget, &typed, refused).await?;
    for (row, length) in batch.iter_mut().zip(lengths) {
        row.length = length;
    }
    rows.extend(batch);
    Ok(unreadable)
}

/// The length of each key by its type, pipelined; `None` for types without a length
/// command and for failures.
async fn lengths(
    conn: &mut impl LiveConn,
    budget: &Budget,
    keys: &[(String, KeyType)],
    refused: &mut Refused,
) -> Result<Vec<Option<u64>>, LiveError> {
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    let mut sent: Vec<Option<&'static str>> = Vec::with_capacity(keys.len());
    for (key, t) in keys {
        let cmd = length_command(*t);
        if let Some(c) = cmd {
            pipe.cmd(c).arg(key);
        }
        sent.push(cmd);
    }
    if pipe.is_empty() {
        return Ok(vec![None; keys.len()]);
    }
    let replies: Vec<Value> = query_pipe(conn, budget, "the key lengths", &pipe).await?;
    let mut replies = replies.iter();
    Ok(sent
        .into_iter()
        .map(|cmd| {
            let name = cmd?;
            refused.read::<u64>(name, replies.next()?)
        })
        .collect())
}

/// [`Request::Facts`]: `OBJECT ENCODING` for every key, and the length of those that ask
/// for it, in batches.
async fn facts(
    conn: &mut impl LiveConn,
    budget: &Budget,
    keys: &[FactKey],
) -> Result<Reply, LiveError> {
    let mut refused = Refused::default();
    let mut out = Vec::with_capacity(keys.len());
    for batch in keys.chunks(BATCH) {
        let mut pipe = redis::pipe();
        pipe.ignore_errors();
        let mut sent: Vec<Option<&'static str>> = Vec::with_capacity(batch.len());
        for k in batch {
            pipe.cmd("OBJECT").arg("ENCODING").arg(&k.key);
            let len = length_command(k.key_type).filter(|_| k.length);
            if let Some(c) = len {
                pipe.cmd(c).arg(&k.key);
            }
            sent.push(len);
        }
        let replies: Vec<Value> = query_pipe(conn, budget, "the key facts", &pipe).await?;
        let mut replies = replies.iter();
        for (k, len) in batch.iter().zip(sent) {
            let Some(enc) = replies.next() else { break };
            // A key that has gone answers OBJECT ENCODING with nil: leave it out.
            let gone = matches!(enc, Value::Nil);
            let encoding = refused.read::<String>("OBJECT ENCODING", enc);
            let length = match len {
                Some(c) => replies.next().and_then(|v| refused.read::<u64>(c, v)),
                None => None,
            };
            if !gone {
                out.push((k.key.clone(), KeyFacts { length, encoding }));
            }
        }
    }
    Ok(Reply::Facts {
        facts: out,
        refused: refused.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_commands_by_type() {
        let got: Vec<_> = [
            KeyType::String,
            KeyType::Hash,
            KeyType::List,
            KeyType::Set,
            KeyType::ZSet,
            KeyType::Stream,
            KeyType::Json,
            KeyType::Other,
        ]
        .map(length_command)
        .to_vec();
        assert_eq!(
            got,
            [
                Some("STRLEN"),
                Some("HLEN"),
                Some("LLEN"),
                Some("SCARD"),
                Some("ZCARD"),
                Some("XLEN"),
                None,
                None
            ]
        );
    }

    /// An error reply as the server sends it: `-CODE detail`.
    fn server_error(line: &str) -> Value {
        let v = redis::parse_redis_value(format!("-{line}\r\n").as_bytes()).unwrap();
        assert!(matches!(v, Value::ServerError(_)), "{v:?}");
        v
    }

    #[test]
    fn refused_commands_are_noted_once_in_fixed_words() {
        let mut r = Refused::default();
        let noperm = server_error(
            "NOPERM User lens-limited has no permissions to run the 'memory|usage' command",
        );
        assert_eq!(r.read::<u64>("MEMORY USAGE", &noperm), None);
        assert_eq!(r.read::<u64>("MEMORY USAGE", &noperm), None);
        let wrongtype =
            server_error("WRONGTYPE Operation against a key holding the wrong kind of value");
        assert_eq!(r.read::<u64>("HLEN", &wrongtype), None);
        assert_eq!(r.read::<u64>("HLEN", &Value::Int(12)), Some(12));
        let unknown =
            server_error("ERR unknown command 'OBJECT', with args beginning with: 'FREQ'");
        assert_eq!(r.read::<u8>("OBJECT FREQ", &unknown), None);
        assert_eq!(
            r.0,
            [
                LiveError::Denied("MEMORY USAGE".into()),
                LiveError::Unsupported("OBJECT FREQ".into())
            ]
        );
        let text = r.0[0].to_string();
        assert!(!text.contains("lens-limited"), "{text}");
    }
}
