//! The Slots panel's requests (spec "2 Analysis · Slots (P4)"): what each slot range of
//! a node holds, from `CLUSTER SLOT-STATS SLOTSRANGE <start> <end>` (Redis 8.2 and
//! later, Valkey 8.0 and later). One request goes to one node and asks for each of the
//! slot ranges it owns, one command per range, every command from that node's budget.
//!
//! The node's version is read first, with `INFO server` (one command), and a node that
//! is too old is not sent `CLUSTER SLOT-STATS`: the reply says so instead. The refusal
//! of a node that has the command (`NOPERM`) comes back as [`LiveError::Denied`].

use redis::Value;

use crate::budget::Budget;
use crate::model::ServerFlavor;

use super::{LiveConn, LiveError, query};

/// The name a failure of the command is called by.
const COMMAND: &str = "CLUSTER SLOT-STATS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Read these slot ranges (inclusive) from the node.
    Stats { ranges: Vec<(u16, u16)> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Stats(NodeStats),
}

/// What one node answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStats {
    pub flavor: ServerFlavor,
    /// From `INFO server`, for example `8.2.0`.
    pub version: String,
    /// The version has `CLUSTER SLOT-STATS`. When false `ranges` is empty and nothing
    /// was sent but `INFO server`.
    pub supported: bool,
    /// One entry per range asked for, in the order asked.
    pub ranges: Vec<RangeStats>,
}

/// The slots of one range, in slot order as the server listed them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeStats {
    pub start: u16,
    pub end: u16,
    pub slots: Vec<SlotStat>,
}

/// One slot's figures. A metric the reply does not carry is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotStat {
    pub slot: u16,
    pub keys: Option<u64>,
    pub cpu_usec: Option<u64>,
    pub net_in: Option<u64>,
    pub net_out: Option<u64>,
    /// `memory-bytes`, Redis only.
    pub memory: Option<u64>,
}

/// Whether a server of this flavor and version has `CLUSTER SLOT-STATS`: Redis 8.2 and
/// later, Valkey 8.0 and later. An unparsable version does not.
pub fn supports(flavor: ServerFlavor, version: &str) -> bool {
    let mut parts = version.split('.');
    let num = |p: Option<&str>| {
        p.map(|s| {
            s.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|s| s.parse::<u32>().ok())
    };
    let (Some(major), minor) = (num(parts.next()), num(parts.next()).unwrap_or(0)) else {
        return false;
    };
    match flavor {
        ServerFlavor::Redis => (major, minor) >= (8, 2),
        ServerFlavor::Valkey => major >= 8,
        ServerFlavor::Unknown => false,
    }
}

/// Flavor and version from the text of `INFO server`. `valkey_version` wins over the
/// compatibility `redis_version` Valkey also reports.
pub fn parse_version(info: &str) -> Option<(ServerFlavor, String)> {
    let field = |name: &str| {
        info.lines().find_map(|l| {
            let (k, v) = l.trim().split_once(':')?;
            (k == name).then(|| v.trim().to_string())
        })
    };
    if let Some(v) = field("valkey_version") {
        return Some((ServerFlavor::Valkey, v));
    }
    let v = field("redis_version")?;
    let flavor = if field("server_name").as_deref() == Some("valkey") {
        ServerFlavor::Valkey
    } else {
        ServerFlavor::Redis
    };
    Some((flavor, v))
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::BulkString(b) => String::from_utf8(b.clone()).ok(),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

fn number(v: &Value) -> Option<u64> {
    match v {
        Value::Int(n) => u64::try_from(*n).ok(),
        // A double that is a whole, non-negative number.
        Value::Double(d) if *d >= 0.0 && d.fract() == 0.0 => Some(*d as u64),
        other => text(other)?.trim().parse().ok(),
    }
}

/// One slot's metrics: a flat `[name, value, ...]` array (RESP2) or a map (RESP3).
/// Names are matched ignoring case; a name this version does not know is skipped.
fn metrics(slot: u16, v: &Value) -> Option<SlotStat> {
    let pairs: Vec<(&Value, &Value)> = match v {
        Value::Map(m) => m.iter().map(|(k, v)| (k, v)).collect(),
        Value::Array(a) => a.chunks_exact(2).map(|c| (&c[0], &c[1])).collect(),
        _ => return None,
    };
    let mut out = SlotStat {
        slot,
        ..SlotStat::default()
    };
    for (k, v) in pairs {
        let Some(name) = text(k) else { continue };
        let n = number(v);
        match name.to_ascii_lowercase().as_str() {
            "key-count" => out.keys = n,
            "cpu-usec" => out.cpu_usec = n,
            "network-bytes-in" => out.net_in = n,
            "network-bytes-out" => out.net_out = n,
            "memory-bytes" => out.memory = n,
            _ => {}
        }
    }
    Some(out)
}

/// Parses the reply of `CLUSTER SLOT-STATS SLOTSRANGE`: `[[slot, metrics], ...]`.
/// `None` when it is not that shape.
pub fn parse_stats(v: &Value) -> Option<Vec<SlotStat>> {
    let Value::Array(entries) = v else {
        return None;
    };
    entries
        .iter()
        .map(|e| match e {
            Value::Array(pair) if pair.len() == 2 => {
                let slot = u16::try_from(number(&pair[0])?).ok()?;
                metrics(slot, &pair[1])
            }
            _ => None,
        })
        .collect()
}

pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    let Request::Stats { ranges } = req;
    let info: String = query(conn, budget, "INFO", redis::cmd("INFO").arg("server")).await?;
    let Some((flavor, version)) = parse_version(&info) else {
        return Err(LiveError::Failed(
            "the server's INFO did not say its version".to_string(),
        ));
    };
    if !supports(flavor, &version) {
        return Ok(Reply::Stats(NodeStats {
            flavor,
            version,
            supported: false,
            ranges: Vec::new(),
        }));
    }
    let mut out = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        let v: Value = query(
            conn,
            budget,
            COMMAND,
            redis::cmd("CLUSTER")
                .arg("SLOT-STATS")
                .arg("SLOTSRANGE")
                .arg(start)
                .arg(end),
        )
        .await?;
        let Some(slots) = parse_stats(&v) else {
            return Err(LiveError::Failed(format!(
                "{COMMAND} gave a reply redis-lens does not understand"
            )));
        };
        out.push(RangeStats { start, end, slots });
    }
    Ok(Reply::Stats(NodeStats {
        flavor,
        version,
        supported: true,
        ranges: out,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn flat(pairs: &[(&str, i64)]) -> Value {
        Value::Array(
            pairs
                .iter()
                .flat_map(|(k, n)| [b(k), Value::Int(*n)])
                .collect(),
        )
    }

    #[test]
    fn resp2_flat_arrays() {
        let v = Value::Array(vec![
            Value::Array(vec![
                Value::Int(0),
                flat(&[
                    ("key-count", 4),
                    ("cpu-usec", 90),
                    ("network-bytes-in", 10),
                    ("network-bytes-out", 20),
                ]),
            ]),
            Value::Array(vec![Value::Int(1), flat(&[("key-count", 0)])]),
        ]);
        let got = parse_stats(&v).unwrap();
        assert_eq!(
            got[0],
            SlotStat {
                slot: 0,
                keys: Some(4),
                cpu_usec: Some(90),
                net_in: Some(10),
                net_out: Some(20),
                memory: None
            }
        );
        // A missing metric stays missing, not 0.
        assert_eq!(got[1].keys, Some(0));
        assert_eq!(got[1].cpu_usec, None);
    }

    #[test]
    fn resp3_maps_and_upper_case_names_and_memory() {
        let m = Value::Map(vec![
            (b("KEY-COUNT"), Value::Int(7)),
            (Value::SimpleString("Memory-Bytes".into()), Value::Int(512)),
            (b("cpu-usec"), b("12")),
            (b("some-future-metric"), Value::Int(1)),
        ]);
        let v = Value::Array(vec![Value::Array(vec![Value::Int(5), m])]);
        let got = parse_stats(&v).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].slot, 5);
        assert_eq!(got[0].keys, Some(7));
        assert_eq!(got[0].memory, Some(512));
        assert_eq!(got[0].cpu_usec, Some(12));
        assert_eq!(got[0].net_in, None);
    }

    #[test]
    fn a_reply_of_the_wrong_shape_is_refused() {
        assert_eq!(parse_stats(&Value::Nil), None);
        assert_eq!(
            parse_stats(&Value::Array(vec![Value::Array(vec![Value::Int(1)])])),
            None
        );
        assert_eq!(parse_stats(&Value::Array(vec![])), Some(vec![]));
    }

    #[test]
    fn versions_with_the_command() {
        assert!(supports(ServerFlavor::Redis, "8.2.0"));
        assert!(supports(ServerFlavor::Redis, "8.10.1"));
        assert!(supports(ServerFlavor::Redis, "9.0.0"));
        assert!(!supports(ServerFlavor::Redis, "8.0.3"));
        assert!(!supports(ServerFlavor::Redis, "7.4.2"));
        assert!(!supports(ServerFlavor::Redis, "6.2.14"));
        assert!(supports(ServerFlavor::Valkey, "8.0.0"));
        assert!(supports(ServerFlavor::Valkey, "8.1.3"));
        assert!(!supports(ServerFlavor::Valkey, "7.2.4"));
        assert!(!supports(ServerFlavor::Redis, "unknown"));
        assert!(!supports(ServerFlavor::Unknown, "9.0.0"));
        assert!(supports(ServerFlavor::Redis, "8.2.0-rc1"));
    }

    #[test]
    fn info_gives_flavor_and_version() {
        let valkey = "redis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.1.0\r\n";
        assert_eq!(
            parse_version(valkey),
            Some((ServerFlavor::Valkey, "8.1.0".into()))
        );
        assert_eq!(
            parse_version("redis_version:8.2.1\r\n"),
            Some((ServerFlavor::Redis, "8.2.1".into()))
        );
        assert_eq!(parse_version("nothing"), None);
    }
}
