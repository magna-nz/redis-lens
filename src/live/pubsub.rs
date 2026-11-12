//! Live requests for the Pub/Sub view. Owner: wave B. Phase 1: no writes, no payloads.
//!
//! Only counts: `PUBSUB CHANNELS`, `PUBSUB NUMSUB` and `PUBSUB NUMPAT`. Nothing here
//! subscribes or publishes, so no message payload is ever read.

use redis::{RedisResult, Value};

use super::streams::{text, uint};
use super::{Conn, LiveError, error_for, query_pipe};
use crate::budget::Budget;

/// The most channels listed (spec "5 Pub/Sub": the first 1,000).
pub const MAX_CHANNELS: usize = 1_000;
/// Channels per `PUBSUB NUMSUB` command.
const NUMSUB_BATCH: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// The active channels with their subscriber counts, and the pattern count.
    Counts,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Counts(Counts),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Counts {
    /// The first [`MAX_CHANNELS`] channels, most subscribers first, then by name.
    pub channels: Vec<Channel>,
    /// How many channels are active. More than `channels.len()` when the list was cut.
    pub total_channels: usize,
    /// `PUBSUB NUMPAT`: patterns subscribed with `PSUBSCRIBE`. `None` when refused.
    pub patterns: Option<u64>,
    /// Why a count is missing, in redis-lens's own words.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub name: String,
    /// Subscribers to this channel by name (`SUBSCRIBE`; pattern subscribers are not
    /// counted). `None` when `PUBSUB NUMSUB` was refused.
    pub subscribers: Option<u64>,
}

/// Runs one request. Every command goes through `budget`.
pub async fn run(conn: &mut Conn, budget: &Budget, req: Request) -> Result<Reply, LiveError> {
    match req {
        Request::Counts => counts(conn, budget).await.map(Reply::Counts),
    }
}

async fn counts(conn: &mut Conn, budget: &Budget) -> Result<Counts, LiveError> {
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    pipe.cmd("PUBSUB").arg("CHANNELS");
    pipe.cmd("PUBSUB").arg("NUMPAT");
    let mut replies: Vec<RedisResult<Value>> =
        query_pipe(conn, budget, "PUBSUB CHANNELS", &pipe).await?;
    if replies.len() != 2 {
        return Err(LiveError::Failed("PUBSUB CHANNELS failed".into()));
    }
    let numpat = replies.pop().expect("two replies");
    let channels = replies.pop().expect("two replies");

    let channels = channels.map_err(|e| error_for("PUBSUB CHANNELS", &e))?;
    let mut out = Counts::default();
    let mut names = parse_channels(&channels);
    out.total_channels = names.len();
    names.truncate(MAX_CHANNELS);

    match numpat {
        Ok(v) => out.patterns = uint(&v),
        Err(e) => match error_for("PUBSUB NUMPAT", &e) {
            LiveError::Closed => return Err(LiveError::Closed),
            other => out.notes.push(other.to_string()),
        },
    }

    let mut subs: Option<Vec<(String, u64)>> = Some(Vec::new());
    if !names.is_empty() {
        let mut pipe = redis::pipe();
        pipe.ignore_errors();
        for batch in names.chunks(NUMSUB_BATCH) {
            let mut c = redis::cmd("PUBSUB");
            c.arg("NUMSUB");
            for n in batch {
                c.arg(n.as_slice());
            }
            pipe.add_command(c);
        }
        let replies: Vec<RedisResult<Value>> =
            query_pipe(conn, budget, "PUBSUB NUMSUB", &pipe).await?;
        for r in replies {
            match r {
                Ok(v) => {
                    if let Some(s) = subs.as_mut() {
                        s.extend(parse_numsub(&v));
                    }
                }
                Err(e) => match error_for("PUBSUB NUMSUB", &e) {
                    LiveError::Closed => return Err(LiveError::Closed),
                    other => {
                        if subs.is_some() {
                            out.notes.push(other.to_string());
                        }
                        subs = None;
                    }
                },
            }
        }
    }
    out.channels = merge(&names, subs.as_deref());
    Ok(out)
}

/// The channel names of a `PUBSUB CHANNELS` reply, as the server sent them.
pub fn parse_channels(v: &Value) -> Vec<Vec<u8>> {
    let items = match v {
        Value::Array(items) | Value::Set(items) => items,
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|c| match c {
            Value::BulkString(b) => Some(b.clone()),
            Value::SimpleString(s) => Some(s.clone().into_bytes()),
            _ => None,
        })
        .collect()
}

/// The `(channel, subscribers)` pairs of a `PUBSUB NUMSUB` reply (a flat array in RESP2,
/// a map in RESP3).
pub fn parse_numsub(v: &Value) -> Vec<(String, u64)> {
    match v {
        Value::Map(m) => m
            .iter()
            .filter_map(|(k, n)| Some((text(k)?, uint(n)?)))
            .collect(),
        Value::Array(items) => items
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|[k, n]| Some((text(k)?, uint(n)?)))
            .collect(),
        _ => Vec::new(),
    }
}

/// `names` with their counts from `subs`, most subscribers first, then by name.
/// `subs == None` leaves the counts out.
pub fn merge(names: &[Vec<u8>], subs: Option<&[(String, u64)]>) -> Vec<Channel> {
    let mut out: Vec<Channel> = names
        .iter()
        .map(|n| {
            let name = String::from_utf8_lossy(n).into_owned();
            let subscribers = subs.map(|s| {
                s.iter()
                    .find_map(|(c, k)| (*c == name).then_some(*k))
                    .unwrap_or(0)
            });
            Channel { name, subscribers }
        })
        .collect();
    out.sort_by(|a, b| {
        b.subscribers
            .cmp(&a.subscribers)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn channels_keep_their_bytes() {
        let v = Value::Array(vec![
            b("news"),
            Value::BulkString(vec![0xff, b'x']),
            Value::Nil,
        ]);
        assert_eq!(parse_channels(&v), vec![b"news".to_vec(), vec![0xff, b'x']]);
        assert!(parse_channels(&Value::Nil).is_empty());
    }

    #[test]
    fn numsub_reads_flat_arrays_and_maps() {
        let flat = Value::Array(vec![b("a"), Value::Int(2), b("b"), Value::Int(0)]);
        assert_eq!(parse_numsub(&flat), vec![("a".into(), 2), ("b".into(), 0)]);
        let map = Value::Map(vec![(b("a"), Value::Int(2))]);
        assert_eq!(parse_numsub(&map), vec![("a".into(), 2)]);
        // A trailing name without a count is ignored.
        let odd = Value::Array(vec![b("a"), Value::Int(2), b("b")]);
        assert_eq!(parse_numsub(&odd), vec![("a".into(), 2)]);
    }

    #[test]
    fn merge_ranks_by_subscribers_then_name() {
        let names = vec![b"b".to_vec(), b"a".to_vec(), b"c".to_vec()];
        let subs = vec![("a".into(), 1), ("b".into(), 3), ("c".into(), 1)];
        let m = merge(&names, Some(&subs));
        let order: Vec<_> = m.iter().map(|c| (c.name.as_str(), c.subscribers)).collect();
        assert_eq!(order, [("b", Some(3)), ("a", Some(1)), ("c", Some(1))]);
        // Counts refused: names only, in name order.
        let m = merge(&names, None);
        let order: Vec<_> = m.iter().map(|c| (c.name.as_str(), c.subscribers)).collect();
        assert_eq!(order, [("a", None), ("b", None), ("c", None)]);
        // A channel missing from the counts has none.
        assert_eq!(merge(&names[..1], Some(&[]))[0].subscribers, Some(0));
    }

    proptest! {
        #[test]
        fn parsers_never_panic(items in proptest::collection::vec(
            prop_oneof![
                Just(Value::Nil),
                any::<i64>().prop_map(Value::Int),
                proptest::collection::vec(any::<u8>(), 0..8).prop_map(Value::BulkString),
            ],
            0..12,
        )) {
            let v = Value::Array(items);
            let names = parse_channels(&v);
            let subs = parse_numsub(&v);
            let merged = merge(&names, Some(&subs));
            prop_assert_eq!(merged.len(), names.len());
        }

        #[test]
        fn merge_is_sorted(counts in proptest::collection::vec(0u64..5, 0..20)) {
            let names: Vec<Vec<u8>> =
                (0..counts.len()).map(|i| format!("c{i:02}").into_bytes()).collect();
            let subs: Vec<(String, u64)> =
                names.iter().zip(&counts).map(|(n, k)| (String::from_utf8(n.clone()).unwrap(), *k)).collect();
            let m = merge(&names, Some(&subs));
            for w in m.windows(2) {
                prop_assert!(w[0].subscribers >= w[1].subscribers);
            }
        }
    }
}
