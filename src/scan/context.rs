//! Exact server-side figures (`INFO keysizes`, `INFO keyspace`, `MEMORY STATS`) set
//! beside the sampled estimates.
//!
//! Owner: scanner. Reading them costs three commands per scan, paced by the scan's
//! command budget. Each part is optional: a section the server doesn't have, a denied or
//! unknown command, or a reply that can't be read leaves that part `None` with a
//! [`ContextGap`] saying why, and never fails the scan.

use redis::aio::ConnectionLike;
use redis::{ServerErrorKind, Value};

use crate::format;
use crate::model::{
    ContextGap, KeyType, KeysizeHistogram, KeyspaceInfo, MemoryStats, PERM_MEMORY_STATS,
    PermStatus, PermissionReport, ScanReport, ServerContext,
};

use crate::budget::Budget;

/// Strings this long or longer count as big in the `INFO keysizes` summary.
pub const BIG_STRING_BYTES: u64 = 1 << 20;
/// Collections with this many items or more count as big.
pub const BIG_COLLECTION_ITEMS: u64 = 1 << 16;

/// The type-mix check needs at least this many keys on both sides.
const MIX_MIN_KEYS: u64 = 100;
/// ... and warns when a type's share differs by this much (20 percentage points).
const MIX_MAX_GAP: f64 = 0.20;
/// ... unless patterns of mixed type hold more than this share of the sample.
const MIX_MAX_UNTYPED: f64 = 0.10;

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Read the server context for database `db`. Sends `INFO keyspace`, `INFO keysizes`
/// and, unless the permission check denied or skipped it, `MEMORY STATS`; every one is
/// read-only. Each command is taken from `budget` before it is sent.
pub(super) async fn read<C: ConnectionLike + Send>(
    conn: &mut C,
    db: u32,
    perms: &PermissionReport,
    budget: &Budget,
) -> ServerContext {
    let mut ctx = ServerContext::default();

    budget.take(1).await;
    let keyspace: Result<String, _> = redis::cmd("INFO").arg("keyspace").query_async(conn).await;
    match keyspace {
        Ok(text) => ctx.keyspace = Some(parse_keyspace(&text, db)),
        Err(_) => ctx.missing.push(ContextGap::KeyspaceUnavailable),
    }

    // Servers without the section answer with an empty string. Detected by presence,
    // never by version.
    budget.take(1).await;
    let keysizes: Result<String, _> = redis::cmd("INFO").arg("keysizes").query_async(conn).await;
    ctx.keysizes = keysizes.ok().and_then(|t| parse_keysizes(&t, db));
    if ctx.keysizes.is_none() {
        ctx.missing.push(ContextGap::NoKeysizes);
    }

    let probe = perms
        .checks
        .iter()
        .find(|c| c.label == PERM_MEMORY_STATS)
        .map(|c| &c.status);
    match probe {
        Some(PermStatus::Denied(m)) if m.starts_with("NOPERM") => {
            ctx.missing.push(ContextGap::MemoryStatsDenied);
        }
        Some(PermStatus::Denied(_) | PermStatus::Skipped(_)) => {
            ctx.missing.push(ContextGap::MemoryStatsUnavailable);
        }
        // Allowed, or not checked by this caller: try it, and take a refusal as a gap.
        Some(PermStatus::Allowed) | None => {
            budget.take(1).await;
            let reply: Result<Value, redis::RedisError> =
                redis::cmd("MEMORY").arg("STATS").query_async(conn).await;
            match reply {
                Ok(v) => match parse_memory_stats(&v) {
                    Some(m) => ctx.memory = Some(m),
                    None => ctx.missing.push(ContextGap::MemoryStatsUnavailable),
                },
                Err(e) if e.kind() == redis::ErrorKind::Server(ServerErrorKind::NoPerm) => {
                    ctx.missing.push(ContextGap::MemoryStatsDenied)
                }
                Err(_) => ctx.missing.push(ContextGap::MemoryStatsUnavailable),
            }
        }
    }
    ctx
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// The `dbN` line of `INFO keyspace`, e.g. `db0:keys=4,expires=1,avg_ttl=99954,subexpiry=1`.
/// A database without keys has no line and reads as zeros. Unknown fields are ignored
/// and a field that isn't a number reads as 0.
pub fn parse_keyspace(text: &str, db: u32) -> KeyspaceInfo {
    let mut info = KeyspaceInfo {
        db,
        ..KeyspaceInfo::default()
    };
    let prefix = format!("db{db}:");
    let Some(rest) = text.lines().find_map(|l| l.trim().strip_prefix(&prefix)) else {
        return info;
    };
    for kv in rest.split(',') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        let n = v.trim().parse::<u64>().unwrap_or(0);
        match k.trim() {
            "keys" => info.keys = n,
            "expires" => info.expires = n,
            "avg_ttl" => info.avg_ttl_ms = n,
            "subexpiry" => info.subexpiry = Some(n),
            _ => {}
        }
    }
    info
}

/// A bin label of `INFO keysizes`: `0`, `512`, `1K`, `2M`, … (powers of 1024).
fn bin_bound(label: &str) -> Option<u64> {
    let label = label.trim();
    let (digits, shift) = match label.chars().last()? {
        'K' | 'k' => (&label[..label.len() - 1], 10),
        'M' | 'm' => (&label[..label.len() - 1], 20),
        'G' | 'g' => (&label[..label.len() - 1], 30),
        'T' | 't' => (&label[..label.len() - 1], 40),
        'P' | 'p' => (&label[..label.len() - 1], 50),
        'E' | 'e' => (&label[..label.len() - 1], 60),
        _ => (label, 0),
    };
    let n: u64 = digits.parse().ok()?;
    n.checked_mul(1u64 << shift)
}

/// The histograms of `INFO keysizes` for database `db`, in the order the server lists
/// them. `None` when the text has no `# Keysizes` section and no `dbN_distrib_` line:
/// the server doesn't have the section. Bins that can't be read are skipped.
pub fn parse_keysizes(text: &str, db: u32) -> Option<Vec<KeysizeHistogram>> {
    let mut present = false;
    let mut out = Vec::new();
    let prefix = format!("db{db}_distrib_");
    for line in text.lines() {
        let line = line.trim();
        if line.eq_ignore_ascii_case("# keysizes") {
            present = true;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.starts_with("db") && key.contains("_distrib_") {
            present = true;
        }
        let Some(name) = key.strip_prefix(&prefix) else {
            continue;
        };
        let Some((key_type, unit)) = name.rsplit_once('_') else {
            continue;
        };
        if key_type.is_empty() || unit.is_empty() {
            continue;
        }
        let mut bins: Vec<(u64, u64)> = value
            .split(',')
            .filter_map(|b| {
                let (label, count) = b.split_once('=')?;
                Some((bin_bound(label)?, count.trim().parse::<u64>().ok()?))
            })
            .collect();
        bins.sort_unstable();
        out.push(KeysizeHistogram {
            key_type: key_type.to_string(),
            unit: unit.to_string(),
            bins,
        });
    }
    present.then_some(out)
}

fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::BulkString(b) => String::from_utf8(b.clone()).ok(),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

fn value_f64(v: &Value) -> Option<f64> {
    let f = match v {
        Value::Int(n) => *n as f64,
        Value::Double(d) => *d,
        other => value_text(other)?.trim().parse::<f64>().ok()?,
    };
    f.is_finite().then_some(f)
}

fn value_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Int(n) => u64::try_from(*n).ok(),
        other => {
            let f = value_f64(other)?;
            (f >= 0.0 && f < u64::MAX as f64).then(|| f.round() as u64)
        }
    }
}

/// The fields redis-lens uses from a `MEMORY STATS` reply: a flat array of names and
/// values (RESP2) or a map (RESP3). Numbers may come as integers, doubles or text;
/// nested values (`db.0`) and unknown names are ignored. `None` when the reply is not
/// an array or map, or holds none of the fields.
pub fn parse_memory_stats(v: &Value) -> Option<MemoryStats> {
    let pairs: Vec<(&Value, &Value)> = match v {
        Value::Array(items) => items
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[k, v]| (k, v))
            .collect(),
        Value::Map(items) => items.iter().map(|(k, v)| (k, v)).collect(),
        _ => return None,
    };
    let mut m = MemoryStats::default();
    let mut any = false;
    for (k, v) in pairs {
        let Some(name) = value_text(k) else {
            continue;
        };
        let slot = match name.as_str() {
            "dataset.bytes" => &mut m.dataset_bytes,
            "overhead.total" => &mut m.overhead_total,
            "keys.count" => &mut m.keys_count,
            "keys.bytes-per-key" => &mut m.bytes_per_key,
            "fragmentation" => {
                if let Some(f) = value_f64(v).filter(|f| *f >= 0.0) {
                    m.fragmentation = Some(f);
                    any = true;
                }
                continue;
            }
            _ => continue,
        };
        if let Some(n) = value_u64(v) {
            *slot = Some(n);
            any = true;
        }
    }
    any.then_some(m)
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

/// The sample's estimated bytes as a share of `dataset.bytes`, and `dataset.bytes`.
/// `None` without sizes or without `MEMORY STATS`.
pub fn coverage(report: &ScanReport) -> Option<(f64, u64)> {
    let dataset = report.server_context.as_ref()?.memory?.dataset_bytes?;
    if !report.sizes_available || dataset == 0 {
        return None;
    }
    let est: u64 = report
        .patterns
        .iter()
        .filter_map(|p| p.est_bytes)
        .fold(0, u64::saturating_add);
    Some((est as f64 / dataset as f64, dataset))
}

/// A power of two as `INFO keysizes` labels it: `512`, `1K`, `2M`.
fn bound_text(n: u64) -> String {
    const UNITS: [&str; 7] = ["", "K", "M", "G", "T", "P", "E"];
    let mut v = n;
    let mut unit = 0;
    while v >= 1024 && v.is_multiple_of(1024) && unit < UNITS.len() - 1 {
        v /= 1024;
        unit += 1;
    }
    format!("{v}{}", UNITS[unit])
}

/// `strings 2K–4K bytes`: the histogram's largest non-empty bin.
fn largest_bin(h: &KeysizeHistogram) -> Option<String> {
    let (lo, _) = h.bins.iter().rev().find(|(_, n)| *n > 0)?;
    let range = if *lo == 0 {
        "0".to_string()
    } else {
        format!("{}–{}", bound_text(*lo), bound_text(lo.saturating_mul(2)))
    };
    Some(format!("{} {range} {}", h.key_type, unit_word(h)))
}

fn unit_word(h: &KeysizeHistogram) -> &'static str {
    if h.unit == "sizes" { "bytes" } else { "items" }
}

fn is_strings(h: &KeysizeHistogram) -> bool {
    h.key_type == "strings"
}

/// Keys in bins at or above the big-key threshold for the histogram's type.
fn big_keys(h: &KeysizeHistogram) -> u64 {
    let limit = if is_strings(h) {
        BIG_STRING_BYTES
    } else {
        BIG_COLLECTION_ITEMS
    };
    h.bins
        .iter()
        .filter(|(lo, _)| *lo >= limit)
        .map(|(_, n)| *n)
        .fold(0, u64::saturating_add)
}

/// Keys at or above the big-key thresholds ([`BIG_STRING_BYTES`] for strings,
/// [`BIG_COLLECTION_ITEMS`] for the rest) over every histogram.
pub fn big_keys_total(hists: &[KeysizeHistogram]) -> u64 {
    hists.iter().map(big_keys).fold(0, u64::saturating_add)
}

fn keysizes_type(name: &str) -> Option<KeyType> {
    Some(match name {
        "strings" => KeyType::String,
        "lists" => KeyType::List,
        "sets" => KeyType::Set,
        "zsets" => KeyType::ZSet,
        "hashes" => KeyType::Hash,
        _ => return None,
    })
}

/// Warnings where the sample disagrees sharply with `INFO keysizes`, which suggests the
/// sample is biased. Two checks, both skipped with `MATCH` (the sample then covers only
/// matching keys):
///
/// - **Big strings.** `MEMORY USAGE` is never smaller than a string's length, so when
///   the server counts strings of 1 MiB or more and no sampled pattern that may hold
///   strings has a key that large, the sample missed them.
/// - **Type mix.** The share of strings, lists, sets, sorted sets and hashes among the
///   keys the histogram counts, against the same share among sampled keys, when both
///   sides have at least 100 keys and patterns of mixed type hold at most 10% of the
///   sample. Warns about the type that differs most, when by 20 points or more.
pub fn bias_warnings(report: &ScanReport) -> Vec<String> {
    let mut out = Vec::new();
    let Some(hists) = report
        .server_context
        .as_ref()
        .and_then(|c| c.keysizes.as_ref())
    else {
        return out;
    };
    if report.match_glob.is_some() {
        return out;
    }

    let big_strings: u64 = hists.iter().filter(|h| is_strings(h)).map(big_keys).sum();
    if big_strings > 0 && report.sizes_available {
        let sampled_big = report.patterns.iter().any(|p| {
            matches!(p.key_type, None | Some(KeyType::String))
                && p.size_max.is_some_and(|m| m >= BIG_STRING_BYTES)
        });
        if !sampled_big {
            out.push(format!(
                "Possible sampling bias: INFO keysizes counts {} {} of 1M bytes or more, \
                 but no sampled string is that large, so memory may be underestimated",
                format::grouped(big_strings),
                if big_strings == 1 {
                    "string"
                } else {
                    "strings"
                }
            ));
        }
    }

    // Type mix, over the five types the histogram covers.
    let mut exact: Vec<(KeyType, &str, u64)> = Vec::new();
    for h in hists {
        if let Some(t) = keysizes_type(&h.key_type) {
            let n = h.bins.iter().map(|(_, n)| *n).fold(0, u64::saturating_add);
            match exact.iter_mut().find(|(k, _, _)| *k == t) {
                Some(e) => e.2 = e.2.saturating_add(n),
                None => exact.push((t, h.key_type.as_str(), n)),
            }
        }
    }
    let exact_total: u64 = exact.iter().map(|e| e.2).sum();
    let sampled_of = |t: KeyType| -> u64 {
        report
            .patterns
            .iter()
            .filter(|p| p.key_type == Some(t))
            .map(|p| p.sampled_keys)
            .sum()
    };
    let covered = [
        KeyType::String,
        KeyType::List,
        KeyType::Set,
        KeyType::ZSet,
        KeyType::Hash,
    ];
    let sampled_total: u64 = covered.iter().map(|t| sampled_of(*t)).sum();
    let untyped: u64 = report
        .patterns
        .iter()
        .filter(|p| p.key_type.is_none())
        .map(|p| p.sampled_keys)
        .sum();
    let untyped_share = untyped as f64 / (sampled_total + untyped).max(1) as f64;
    if exact_total >= MIX_MIN_KEYS
        && sampled_total >= MIX_MIN_KEYS
        && untyped_share <= MIX_MAX_UNTYPED
    {
        // The type that differs most; on a tie (two types always differ by the same
        // amount when only two are present), the first in `covered`.
        let mut worst: Option<(KeyType, f64, f64)> = None;
        for t in covered {
            let e = exact.iter().find(|x| x.0 == t);
            let exact_share = e.map_or(0, |x| x.2) as f64 / exact_total as f64;
            let sampled_share = sampled_of(t) as f64 / sampled_total as f64;
            let gap = (exact_share - sampled_share).abs();
            if worst.is_none_or(|(_, e, s)| gap > (e - s).abs() + 1e-9) {
                worst = Some((t, exact_share, sampled_share));
            }
        }
        if let Some((t, e, s)) = worst
            && (e - s).abs() >= MIX_MAX_GAP
        {
            let name = exact
                .iter()
                .find(|x| x.0 == t)
                .map_or_else(|| plural_type(t), |x| x.1.to_string());
            out.push(format!(
                "Possible sampling bias: {name} are {} of keys (INFO keysizes) but {} of the sample",
                format::percent(e),
                format::percent(s)
            ));
        }
    }
    out
}

fn plural_type(t: KeyType) -> String {
    match t {
        KeyType::String => "strings",
        KeyType::List => "lists",
        KeyType::Set => "sets",
        KeyType::ZSet => "zsets",
        KeyType::Hash => "hashes",
        _ => "other keys",
    }
    .to_string()
}

/// Why a part is missing, as a sentence. `None` for reasons from a newer version.
pub fn gap_text(gap: ContextGap) -> Option<&'static str> {
    Some(match gap {
        ContextGap::KeyspaceUnavailable => {
            "INFO keyspace could not be read, so the exact key counts are missing"
        }
        ContextGap::MemoryStatsDenied => {
            "MEMORY STATS is denied for this user (ACL +memory|stats), so dataset.bytes is missing"
        }
        ContextGap::MemoryStatsUnavailable => {
            "MEMORY STATS is not available on this server, so dataset.bytes is missing"
        }
        ContextGap::NoKeysizes => {
            "This server has no INFO keysizes (Redis 8 and later have it), so there is no exact size histogram"
        }
        ContextGap::Other => return None,
    })
}

/// The `INFO keyspace` line as text: `Keys in db0 (INFO keyspace, exact): 120,345 · …`.
pub fn keyspace_line(k: &KeyspaceInfo) -> String {
    let mut s = format!(
        "Keys in db{} (INFO keyspace, exact): {}",
        k.db,
        format::grouped(k.keys)
    );
    if k.keys > 0 {
        s.push_str(&format!(
            " · {} with a TTL · {} without",
            format::grouped(k.expires),
            format::grouped(k.keys.saturating_sub(k.expires))
        ));
    }
    if k.expires > 0 && k.avg_ttl_ms > 0 {
        s.push_str(&format!(
            " · avg TTL {}",
            format::duration(std::time::Duration::from_millis(k.avg_ttl_ms))
        ));
    }
    if let Some(n) = k.subexpiry.filter(|n| *n > 0) {
        s.push_str(&format!(
            " · {} with field TTLs (subexpiry)",
            format::grouped(n)
        ));
    }
    s
}

/// The big-key line from `INFO keysizes`.
pub fn keysizes_line(db: u32, hists: &[KeysizeHistogram]) -> String {
    let largest: Vec<String> = hists.iter().filter_map(largest_bin).collect();
    if largest.is_empty() {
        return format!("Big keys (INFO keysizes, exact): no keys in db{db}");
    }
    let big: Vec<String> = hists
        .iter()
        .filter_map(|h| {
            let n = big_keys(h);
            (n > 0).then(|| {
                let limit = if is_strings(h) {
                    "1M bytes"
                } else {
                    "64K items"
                };
                format!("{} {} of {limit} or more", format::grouped(n), h.key_type)
            })
        })
        .collect();
    let head = if big.is_empty() {
        "none of 1M bytes or 64K items or more".to_string()
    } else {
        big.join(" · ")
    };
    format!(
        "Big keys (INFO keysizes, exact): {head}; largest: {}",
        largest.join(", ")
    )
}

/// Plain-text lines summarising `report.server_context` for the `scan` table and the
/// reports, e.g. "The sample explains 82% of the dataset (dataset.bytes 1.2G)". Empty when
/// there is none.
///
/// In order: how much of `dataset.bytes` the sample explains, the other `MEMORY STATS`
/// figures, the exact key counts, the big-key summary, why parts are missing, and any
/// sampling-bias warnings (starting with `▲`).
pub fn context_summary(report: &ScanReport) -> Vec<String> {
    let Some(ctx) = &report.server_context else {
        return Vec::new();
    };
    let mut lines = Vec::new();

    if let Some(m) = &ctx.memory {
        match (coverage(report), m.dataset_bytes) {
            (Some((ratio, dataset)), _) => {
                let who = match &report.match_glob {
                    Some(g) => format!("Keys matching {g} explain"),
                    None => "The sample explains".to_string(),
                };
                lines.push(format!(
                    "{who} {} of the dataset (dataset.bytes {})",
                    format::percent(ratio),
                    format::bytes(dataset)
                ));
            }
            (None, Some(dataset)) => lines.push(format!(
                "Dataset (MEMORY STATS): dataset.bytes {}; without MEMORY USAGE the sample \
                 has no sizes to compare with it",
                format::bytes(dataset)
            )),
            (None, None) => {}
        }
        let mut parts = Vec::new();
        if let Some(n) = m.overhead_total {
            parts.push(format!("overhead.total {}", format::bytes(n)));
        }
        if let Some(n) = m.bytes_per_key {
            parts.push(format!("{} per key", format::bytes(n)));
        }
        if let Some(f) = m.fragmentation {
            parts.push(format!("fragmentation {f:.2}"));
        }
        if !parts.is_empty() {
            lines.push(format!("MEMORY STATS: {}", parts.join(" · ")));
        }
    }
    if let Some(k) = &ctx.keyspace {
        lines.push(keyspace_line(k));
    }
    if let Some(h) = &ctx.keysizes {
        let db = ctx.keyspace.map_or(0, |k| k.db);
        lines.push(keysizes_line(db, h));
    }
    lines.extend(
        ctx.missing
            .iter()
            .filter_map(|g| gap_text(*g))
            .map(String::from),
    );
    lines.extend(bias_warnings(report).into_iter().map(|w| format!("▲ {w}")));
    lines
}

#[cfg(test)]
mod tests {
    /// A snapshot is saved as JSON and compared with the scan it came from, so float
    /// fields must read back bit for bit (serde_json's `float_roundtrip` feature: without
    /// it, `fragmentation` can come back one unit off in the last place).
    #[test]
    fn memory_stats_floats_survive_a_json_round_trip() {
        for f in [
            1.530_957_669_598_477_5,
            1.831_791_926_153_223_4,
            9.567_604_255_623_749,
            0.1 + 0.2,
            0.0,
        ] {
            let m = crate::model::MemoryStats {
                fragmentation: Some(f),
                ..Default::default()
            };
            let back: crate::model::MemoryStats =
                serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
            assert_eq!(
                back.fragmentation.map(f64::to_bits),
                Some(f.to_bits()),
                "{f}"
            );
        }
    }

    use super::*;
    use crate::model::{PatternStats, ServerContextParts, ServerFlavor, ServerInfo, ServerMode};
    use proptest::prelude::*;

    // Captured from real servers (redis-cli, CRLF line ends kept): Redis 8.0 with
    // `SET s1 hello`, a 3,000-byte `s2`, a 3-item list, a 2-field hash with one field
    // TTL (HEXPIRE) and one key in db2.
    const REDIS8_KEYSIZES: &str = "# Keysizes\r\ndb0_distrib_strings_sizes:4=1,2K=1\r\ndb0_distrib_lists_items:2=1\r\ndb0_distrib_hashes_items:2=1\r\ndb2_distrib_strings_sizes:1=1\r\n";
    const REDIS8_KEYSPACE: &str = "# Keyspace\r\ndb0:keys=4,expires=1,avg_ttl=99954,subexpiry=1\r\ndb2:keys=1,expires=0,avg_ttl=0,subexpiry=0\r\n";
    const VALKEY81_KEYSPACE: &str =
        "# Keyspace\r\ndb0:keys=4,expires=1,avg_ttl=99924\r\ndb2:keys=1,expires=0,avg_ttl=0\r\n";
    // The example from the INFO page on redis.io.
    const DOCS_KEYSIZES: &str = "# Keysizes\r\ndb0_distrib_strings_sizes:2=1,4=1,8=1,16=1,32=1,64=1,128=1,256=1,512=1,1K=1,2K=1,4K=1,8K=1,16K=1,32K=1,64K=1,128K=1,256K=1,512K=1,1M=1\r\ndb0_distrib_lists_items:1=1,2=1,4=1,8=1,16=1,32=1,64=1,128=1,256=1,512=1,1K=1,2K=1,4K=1,8K=1,16K=1,32K=1,64K=1,128K=1,256K=1,512K=1,1M=1\r\ndb0_distrib_sets_items:1=1,2=1,4=1,8=1,16=1,32=1,64=1,128=1,256=1,512=1,1K=1,2K=1,4K=1,8K=1,16K=1,32K=1,64K=1,128K=1,256K=1,512K=1,1M=1\r\ndb0_distrib_hashes_items:2=1,4=1,8=1,16=1,32=1,64=1,128=1,256=1,512=1,1K=1,2K=1,4K=1,8K=1,16K=1,32K=1,64K=1,128K=1,256K=1,512K=1,1M=1\r\n";

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    /// A Redis 7.4 `MEMORY STATS` reply over RESP2, as captured (doubles are text).
    fn redis74_memory_stats() -> Value {
        let mut v = Vec::new();
        let mut kv = |k: &str, val: Value| {
            v.push(bulk(k));
            v.push(val);
        };
        kv("peak.allocated", Value::Int(1_171_312));
        kv("total.allocated", Value::Int(1_171_320));
        kv("startup.allocated", Value::Int(981_192));
        kv("replication.backlog", Value::Int(0));
        kv("clients.slaves", Value::Int(0));
        kv("clients.normal", Value::Int(0));
        kv("functions.caches", Value::Int(192));
        kv(
            "db.0",
            Value::Array(vec![
                bulk("overhead.hashtable.main"),
                Value::Int(472),
                bulk("overhead.hashtable.expires"),
                Value::Int(336),
            ]),
        );
        kv("overhead.total", Value::Int(982_192));
        kv("db.dict.rehashing.count", Value::Int(0));
        kv("keys.count", Value::Int(4));
        kv("keys.bytes-per-key", Value::Int(47_282));
        kv("dataset.bytes", Value::Int(189_128));
        kv("dataset.percentage", bulk("99.47403717041016"));
        kv("peak.percentage", bulk("100.00068664550781"));
        kv("allocator.allocated", Value::Int(4_915_488));
        kv("rss-overhead.bytes", Value::Int(-1_945_600));
        kv("fragmentation", bulk("13.032083511352539"));
        kv("fragmentation.bytes", Value::Int(13_633_024));
        Value::Array(v)
    }

    #[test]
    fn keyspace_lines() {
        assert_eq!(
            parse_keyspace(REDIS8_KEYSPACE, 0),
            KeyspaceInfo {
                db: 0,
                keys: 4,
                expires: 1,
                avg_ttl_ms: 99_954,
                subexpiry: Some(1),
            }
        );
        assert_eq!(
            parse_keyspace(REDIS8_KEYSPACE, 2),
            KeyspaceInfo {
                db: 2,
                keys: 1,
                expires: 0,
                avg_ttl_ms: 0,
                subexpiry: Some(0),
            }
        );
        // Valkey and Redis before 7.4 have no subexpiry.
        assert_eq!(parse_keyspace(VALKEY81_KEYSPACE, 0).subexpiry, None);
        assert_eq!(parse_keyspace(VALKEY81_KEYSPACE, 0).keys, 4);
        // db1 has no line: it is empty. db20 must not match db2's line.
        assert_eq!(parse_keyspace(REDIS8_KEYSPACE, 1).keys, 0);
        assert_eq!(parse_keyspace(REDIS8_KEYSPACE, 20).keys, 0);
        assert_eq!(parse_keyspace("", 0), KeyspaceInfo::default());
        // Tolerant of junk.
        let k = parse_keyspace("db0:keys=x,expires,avg_ttl=5,new_field=7\n", 0);
        assert_eq!((k.keys, k.expires, k.avg_ttl_ms), (0, 0, 5));
    }

    #[test]
    fn keysizes_sections() {
        let h = parse_keysizes(REDIS8_KEYSIZES, 0).unwrap();
        assert_eq!(
            h,
            vec![
                KeysizeHistogram {
                    key_type: "strings".into(),
                    unit: "sizes".into(),
                    bins: vec![(4, 1), (2048, 1)],
                },
                KeysizeHistogram {
                    key_type: "lists".into(),
                    unit: "items".into(),
                    bins: vec![(2, 1)],
                },
                KeysizeHistogram {
                    key_type: "hashes".into(),
                    unit: "items".into(),
                    bins: vec![(2, 1)],
                },
            ]
        );
        let db2 = parse_keysizes(REDIS8_KEYSIZES, 2).unwrap();
        assert_eq!(db2.len(), 1);
        assert_eq!(db2[0].bins, vec![(1, 1)]);
        // The section is there but this database has no keys.
        assert_eq!(parse_keysizes(REDIS8_KEYSIZES, 5), Some(vec![]));
        assert_eq!(parse_keysizes("# Keysizes\r\n", 0), Some(vec![]));
        // Redis 7.4 and Valkey 8.1 answer INFO keysizes with nothing.
        assert_eq!(parse_keysizes("", 0), None);
        assert_eq!(parse_keysizes("\r\n", 0), None);

        let docs = parse_keysizes(DOCS_KEYSIZES, 0).unwrap();
        assert_eq!(docs.len(), 4);
        let strings = &docs[0];
        assert_eq!(strings.bins.first(), Some(&(2, 1)));
        assert_eq!(strings.bins.last(), Some(&(1 << 20, 1)));
        assert_eq!(strings.bins.len(), 20);
        assert_eq!(docs[3].key_type, "hashes");
    }

    #[test]
    fn keysize_bins() {
        assert_eq!(bin_bound("0"), Some(0));
        assert_eq!(bin_bound("512"), Some(512));
        assert_eq!(bin_bound("1K"), Some(1024));
        assert_eq!(bin_bound("64K"), Some(65_536));
        assert_eq!(bin_bound("2M"), Some(2 << 20));
        assert_eq!(bin_bound("1G"), Some(1 << 30));
        assert_eq!(bin_bound("8E"), Some(1 << 63));
        assert_eq!(bin_bound("16E"), None, "overflows");
        assert_eq!(bin_bound("K"), None);
        assert_eq!(bin_bound(""), None);
        assert_eq!(bin_bound("x"), None);
        // An unreadable bin is skipped, the rest stay.
        let h = parse_keysizes("db0_distrib_strings_sizes:4=1,bad,8=x,16=2\r\n", 0).unwrap();
        assert_eq!(h[0].bins, vec![(4, 1), (16, 2)]);
    }

    #[test]
    fn memory_stats_resp2() {
        let m = parse_memory_stats(&redis74_memory_stats()).unwrap();
        assert_eq!(m.dataset_bytes, Some(189_128));
        assert_eq!(m.overhead_total, Some(982_192));
        assert_eq!(m.keys_count, Some(4));
        assert_eq!(m.bytes_per_key, Some(47_282));
        assert!((m.fragmentation.unwrap() - 13.032).abs() < 0.001);
    }

    #[test]
    fn memory_stats_resp3_and_odd_values() {
        let v = Value::Map(vec![
            (
                Value::SimpleString("dataset.bytes".into()),
                Value::Double(1.5e9),
            ),
            (bulk("overhead.total"), bulk("1000")),
            (bulk("fragmentation"), Value::Double(1.25)),
            (bulk("keys.count"), Value::Int(-1)),
            (Value::Int(5), Value::Int(5)),
        ]);
        let m = parse_memory_stats(&v).unwrap();
        assert_eq!(m.dataset_bytes, Some(1_500_000_000));
        assert_eq!(m.overhead_total, Some(1000));
        assert_eq!(m.fragmentation, Some(1.25));
        assert_eq!(m.keys_count, None, "negative");
        // Not an array or map, or no field it knows.
        assert_eq!(parse_memory_stats(&Value::Nil), None);
        assert_eq!(parse_memory_stats(&bulk("dataset.bytes")), None);
        assert_eq!(
            parse_memory_stats(&Value::Array(vec![bulk("x"), Value::Int(1)])),
            None
        );
        // An odd-length array drops the last name.
        assert_eq!(
            parse_memory_stats(&Value::Array(vec![bulk("dataset.bytes")])),
            None
        );
        // NaN and infinity are not figures.
        let v = Value::Array(vec![bulk("fragmentation"), bulk("nan")]);
        assert_eq!(parse_memory_stats(&v), None);
    }

    fn pattern(
        name: &str,
        t: Option<KeyType>,
        sampled: u64,
        est_bytes: u64,
        max: u64,
    ) -> PatternStats {
        PatternStats {
            pattern: name.into(),
            key_type: t,
            sampled_keys: sampled,
            est_keys: sampled * 10,
            sampled_bytes: Some(est_bytes / 10),
            est_bytes: Some(est_bytes),
            no_ttl: 0,
            ttl_hist: [0; 7],
            size_p50: Some(max / 2),
            size_p99: Some(max),
            size_max: Some(max),
            examples: Vec::new(),
        }
    }

    fn report(patterns: Vec<PatternStats>, ctx: Option<ServerContext>) -> ScanReport {
        ScanReport {
            profile: "p".into(),
            taken_at_unix: 0,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "8.0.2".into(),
                mode: ServerMode::Standalone,
                used_memory: 2_000_000_000,
                maxmemory: 0,
                maxmemory_policy: "noeviction".into(),
                total_keys: 120_345,
            },
            sampled_keys: patterns.iter().map(|p| p.sampled_keys).sum(),
            patterns,
            sizes_available: true,
            hot_keys: Vec::new(),
            complete: false,
            match_glob: None,
            server_context: ctx,
            key_sizes: None,
            nodes: None,
        }
    }

    fn hist(t: &str, unit: &str, bins: &[(u64, u64)]) -> KeysizeHistogram {
        KeysizeHistogram {
            key_type: t.into(),
            unit: unit.into(),
            bins: bins.to_vec(),
        }
    }

    fn full_ctx() -> ServerContext {
        ServerContext::from(ServerContextParts {
            keyspace: Some(KeyspaceInfo {
                db: 0,
                keys: 120_345,
                expires: 80_000,
                avg_ttl_ms: 11_100_000,
                subexpiry: Some(12),
            }),
            memory: Some(MemoryStats {
                dataset_bytes: Some(1_288_490_189),
                overhead_total: Some(52_428_800),
                keys_count: Some(120_345),
                bytes_per_key: Some(10_700),
                fragmentation: Some(1.31),
            }),
            keysizes: Some(vec![
                hist(
                    "strings",
                    "sizes",
                    &[(64, 60_000), (2048, 20_000), (1 << 21, 3)],
                ),
                hist("hashes", "items", &[(4, 40_000), (1 << 16, 2)]),
            ]),
            missing: Vec::new(),
        })
    }

    fn sampled_patterns() -> Vec<PatternStats> {
        vec![
            pattern(
                "user:{id}",
                Some(KeyType::String),
                600,
                700_000_000,
                3 << 20,
            ),
            pattern("cart:{id}", Some(KeyType::Hash), 400, 356_000_000, 900_000),
        ]
    }

    #[test]
    fn summary_lines() {
        let r = report(sampled_patterns(), Some(full_ctx()));
        assert_eq!(
            context_summary(&r),
            vec![
                "The sample explains 82% of the dataset (dataset.bytes 1.2G)",
                "MEMORY STATS: overhead.total 50M · 10K per key · fragmentation 1.31",
                "Keys in db0 (INFO keyspace, exact): 120,345 · 80,000 with a TTL · 40,345 without · avg TTL 3h 05m · 12 with field TTLs (subexpiry)",
                "Big keys (INFO keysizes, exact): 3 strings of 1M bytes or more · 2 hashes of 64K items or more; largest: strings 2M–4M bytes, hashes 64K–128K items",
            ]
        );
        // No server context, no lines.
        assert!(context_summary(&report(sampled_patterns(), None)).is_empty());
    }

    #[test]
    fn summary_with_gaps_and_without_sizes() {
        let ctx = ServerContext::from(ServerContextParts {
            keyspace: Some(KeyspaceInfo {
                db: 3,
                keys: 0,
                ..KeyspaceInfo::default()
            }),
            memory: None,
            keysizes: None,
            missing: vec![
                ContextGap::NoKeysizes,
                ContextGap::MemoryStatsDenied,
                ContextGap::Other,
            ],
        });
        let r = report(sampled_patterns(), Some(ctx));
        assert_eq!(
            context_summary(&r),
            vec![
                "Keys in db3 (INFO keyspace, exact): 0",
                "This server has no INFO keysizes (Redis 8 and later have it), so there is no exact size histogram",
                "MEMORY STATS is denied for this user (ACL +memory|stats), so dataset.bytes is missing",
            ]
        );

        let mut r = report(sampled_patterns(), Some(full_ctx()));
        r.sizes_available = false;
        let lines = context_summary(&r);
        assert_eq!(
            lines[0],
            "Dataset (MEMORY STATS): dataset.bytes 1.2G; without MEMORY USAGE the sample has no sizes to compare with it"
        );
        assert_eq!(coverage(&r), None);

        let mut r = report(sampled_patterns(), Some(full_ctx()));
        r.match_glob = Some("user:*".into());
        assert_eq!(
            context_summary(&r)[0],
            "Keys matching user:* explain 82% of the dataset (dataset.bytes 1.2G)"
        );
    }

    #[test]
    fn keysizes_without_big_keys() {
        let h = parse_keysizes(REDIS8_KEYSIZES, 0).unwrap();
        assert_eq!(
            keysizes_line(0, &h),
            "Big keys (INFO keysizes, exact): none of 1M bytes or 64K items or more; largest: strings 2K–4K bytes, lists 2–4 items, hashes 2–4 items"
        );
        assert_eq!(
            keysizes_line(4, &[]),
            "Big keys (INFO keysizes, exact): no keys in db4"
        );
        assert_eq!(
            keysizes_line(0, &[hist("strings", "sizes", &[(0, 5), (4, 0)])]),
            "Big keys (INFO keysizes, exact): none of 1M bytes or 64K items or more; largest: strings 0 bytes"
        );
    }

    #[test]
    fn warns_when_the_sample_missed_big_strings() {
        let mut pats = sampled_patterns();
        pats[0].size_max = Some(4096);
        let r = report(pats.clone(), Some(full_ctx()));
        let w = bias_warnings(&r);
        assert_eq!(
            w,
            vec![
                "Possible sampling bias: INFO keysizes counts 3 strings of 1M bytes or more, but no sampled string is that large, so memory may be underestimated"
            ]
        );
        assert_eq!(context_summary(&r).last().unwrap(), &format!("▲ {}", w[0]));
        // A big mixed-type pattern may hold the strings: no warning.
        pats.push(pattern("misc:{id}", None, 10, 1, 2 << 20));
        assert!(bias_warnings(&report(pats.clone(), Some(full_ctx()))).is_empty());
        // Nor with MATCH, or without sizes.
        pats.pop();
        let mut r = report(pats.clone(), Some(full_ctx()));
        r.match_glob = Some("user:*".into());
        assert!(bias_warnings(&r).is_empty());
        let mut r = report(pats, Some(full_ctx()));
        r.sizes_available = false;
        assert!(bias_warnings(&r).is_empty());
    }

    #[test]
    fn warns_when_the_type_mix_disagrees() {
        // keysizes: 80,003 strings and 40,002 hashes (67% / 33%); sample 95% strings.
        let pats = vec![
            pattern(
                "user:{id}",
                Some(KeyType::String),
                950,
                700_000_000,
                3 << 20,
            ),
            pattern("cart:{id}", Some(KeyType::Hash), 50, 356_000_000, 900_000),
        ];
        let r = report(pats, Some(full_ctx()));
        assert_eq!(
            bias_warnings(&r),
            vec![
                "Possible sampling bias: strings are 67% of keys (INFO keysizes) but 95% of the sample"
            ]
        );
        // 60% / 40% against 67% / 33% is close enough.
        assert!(bias_warnings(&report(sampled_patterns(), Some(full_ctx()))).is_empty());
        // Too few sampled keys to tell.
        let few = vec![
            pattern("user:{id}", Some(KeyType::String), 95, 1, 3 << 20),
            pattern("cart:{id}", Some(KeyType::Hash), 4, 1, 1),
        ];
        assert!(bias_warnings(&report(few, Some(full_ctx()))).is_empty());
        // Mixed-type patterns hold too much of the sample.
        let mixed = vec![
            pattern("user:{id}", Some(KeyType::String), 950, 1, 3 << 20),
            pattern("cart:{id}", Some(KeyType::Hash), 50, 1, 1),
            pattern("x:{id}", None, 200, 1, 1),
        ];
        assert!(bias_warnings(&report(mixed, Some(full_ctx()))).is_empty());
        // Without keysizes there is nothing to compare with.
        let mut ctx = full_ctx();
        ctx.keysizes = None;
        assert!(bias_warnings(&report(sampled_patterns(), Some(ctx))).is_empty());
    }

    #[test]
    fn gap_texts_are_fixed() {
        for g in [
            ContextGap::KeyspaceUnavailable,
            ContextGap::MemoryStatsDenied,
            ContextGap::MemoryStatsUnavailable,
            ContextGap::NoKeysizes,
        ] {
            assert!(gap_text(g).is_some());
        }
        assert_eq!(gap_text(ContextGap::Other), None);
    }

    #[test]
    fn bound_texts() {
        assert_eq!(bound_text(0), "0");
        assert_eq!(bound_text(512), "512");
        assert_eq!(bound_text(1024), "1K");
        assert_eq!(bound_text(3 << 20), "3M");
        assert_eq!(bound_text(1 << 40), "1T");
        assert_eq!(bound_text(1500), "1500");
    }

    #[test]
    fn server_context_serde_round_trip_and_old_snapshots() {
        let ctx = full_ctx();
        let json = serde_json::to_string(&ctx).unwrap();
        let back: ServerContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ctx);
        // An empty object (every part missing) and an unknown gap from a newer version.
        let empty: ServerContext = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ServerContext::default());
        let newer: ServerContext =
            serde_json::from_str(r#"{"missing":["no-keysizes","from-the-future"],"extra":1}"#)
                .unwrap();
        assert_eq!(
            newer.missing,
            vec![ContextGap::NoKeysizes, ContextGap::Other]
        );
        // A report without the field still loads, and one without a context writes none.
        let r = report(sampled_patterns(), None);
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("server_context"), "{json}");
        let back: ScanReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.server_context, None);
    }

    fn value_strategy() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            any::<i64>().prop_map(Value::Int),
            any::<f64>().prop_map(Value::Double),
            ".{0,12}".prop_map(|s| Value::BulkString(s.into_bytes())),
            prop_oneof![
                Just("dataset.bytes"),
                Just("overhead.total"),
                Just("keys.count"),
                Just("keys.bytes-per-key"),
                Just("fragmentation"),
            ]
            .prop_map(|s| Value::BulkString(s.as_bytes().to_vec())),
            Just(Value::Nil),
        ];
        leaf.prop_recursive(3, 32, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                prop::collection::vec((inner.clone(), inner), 0..4).prop_map(Value::Map),
            ]
        })
    }

    proptest! {
        #[test]
        fn parsers_never_panic(text in "(?s).{0,300}", db in 0u32..20) {
            let _ = parse_keyspace(&text, db);
            let _ = parse_keysizes(&text, db);
        }

        #[test]
        fn keysizes_lines_never_panic(
            lines in prop::collection::vec(
                ("db[0-9]{1,2}_distrib_[a-z_]{0,8}", "([0-9]{0,4}[KMGTPEkx]?=[0-9-]{0,5},?){0,6}"),
                0..6,
            )
        ) {
            let text: String = lines.iter().map(|(k, v)| format!("{k}:{v}\r\n")).collect();
            if let Some(h) = parse_keysizes(&text, 0) {
                for x in &h {
                    prop_assert!(x.bins.windows(2).all(|w| w[0] <= w[1]));
                    let _ = largest_bin(x);
                    let _ = big_keys(x);
                }
                let _ = keysizes_line(0, &h);
            }
        }

        #[test]
        fn memory_stats_never_panics(v in value_strategy()) {
            if let Some(m) = parse_memory_stats(&v) {
                prop_assert!(m.fragmentation.is_none_or(|f| f.is_finite() && f >= 0.0));
            }
        }
    }
}
