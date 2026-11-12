//! `scan --report md|html|csv`: shareable files rendered from a finished [`ScanReport`]
//! and, when there is a baseline snapshot, its [`ReportDiff`].
//!
//! Pure rendering: no server commands, no I/O. Pattern and key names come from whoever
//! writes keys, so every format escapes them for its own syntax (see each module).
//! Example key names appear only when `examples` is true. The report holds no credentials
//! and no connection string.

use crate::cli::ReportFormat;
use crate::diff::{Cause, PatternDiff, ReportDiff};
use crate::format;
use crate::model::{NodeReport, NodeRole, NodeStatus, PatternStats, ScanReport};
use crate::verdict::{TtlVerdict, ttl_verdict};

pub mod csv;
pub mod html;
pub mod md;

/// Rows shown in the Markdown and HTML tables; CSV has every pattern.
pub(crate) const TOP_PATTERNS: usize = 25;
/// Changed patterns shown in the diff section.
const TOP_CHANGES: usize = 15;

/// Render `report` as `format`. `diff` is the comparison with the baseline snapshot, if any.
pub fn render(
    format: ReportFormat,
    report: &ScanReport,
    diff: Option<&ReportDiff>,
    examples: bool,
) -> String {
    match format {
        ReportFormat::Md => md::render(report, diff, examples),
        ReportFormat::Html => html::render(report, diff, examples),
        ReportFormat::Csv => csv::render(report, diff, examples),
    }
}

/// One pattern with the figures every format shows.
pub(crate) struct Row<'a> {
    pub stats: &'a PatternStats,
    /// Share of the scan's bytes (keys without sizes), 0.0 to 1.0.
    pub share: f64,
    pub avg_bytes: Option<u64>,
    pub verdict: TtlVerdict,
    pub change: Option<&'a PatternDiff>,
}

pub(crate) fn rows<'a>(report: &'a ScanReport, diff: Option<&'a ReportDiff>) -> Vec<Row<'a>> {
    let total_bytes: u64 = report.patterns.iter().filter_map(|p| p.est_bytes).sum();
    let total_keys: u64 = report.patterns.iter().map(|p| p.est_keys).sum();
    report
        .patterns
        .iter()
        .map(|p| {
            let share = if report.sizes_available {
                match (p.est_bytes, total_bytes) {
                    (Some(b), t) if t > 0 => b as f64 / t as f64,
                    _ => 0.0,
                }
            } else if total_keys > 0 {
                p.est_keys as f64 / total_keys as f64
            } else {
                0.0
            };
            let change = diff.and_then(|d| d.pattern(&p.pattern));
            Row {
                stats: p,
                share,
                avg_bytes: match p.est_bytes {
                    Some(b) if p.est_keys > 0 => Some(b / p.est_keys),
                    _ => None,
                },
                verdict: ttl_verdict(p, change),
                change,
            }
        })
        .collect()
}

/// The top-of-report facts as (label, value) pairs.
pub(crate) fn facts(report: &ScanReport) -> Vec<(&'static str, String)> {
    let s = &report.server;
    let limit = if s.maxmemory == 0 {
        "no limit".to_string()
    } else {
        format::bytes(s.maxmemory)
    };
    let share = if s.total_keys == 0 {
        0.0
    } else {
        (report.sampled_keys as f64 / s.total_keys as f64).min(1.0)
    };
    let coverage = if report.complete {
        "whole keyspace".to_string()
    } else {
        match &report.match_glob {
            Some(g) => format!("partial sample, MATCH {g}"),
            None => "partial sample".to_string(),
        }
    };
    vec![
        ("Profile", report.profile.clone()),
        (
            "Server",
            format!(
                "{:?} {} ({})",
                s.flavor,
                s.version,
                format!("{:?}", s.mode).to_lowercase()
            ),
        ),
        ("Scanned", crate::snapshot::format_utc(report.taken_at_unix)),
        (
            "Memory",
            format!(
                "{} used of {}, eviction {}",
                format::bytes(s.used_memory),
                limit,
                s.maxmemory_policy
            ),
        ),
        (
            "Keys",
            format!(
                "{} sampled of {} total ({})",
                format::grouped(report.sampled_keys),
                format::grouped(s.total_keys),
                format::percent(share)
            ),
        ),
        ("Coverage", coverage),
        (
            "Sizes",
            if report.sizes_available {
                "available (MEMORY USAGE)".to_string()
            } else {
                "unavailable: MEMORY USAGE denied, so sizes are left out".to_string()
            },
        ),
    ]
}

/// Patterns the TTL view flags (warning verdicts), with the plain-language explanation.
pub(crate) fn risks<'a>(rows: &'a [Row<'a>]) -> Vec<(&'a Row<'a>, String)> {
    rows.iter()
        .filter(|r| r.verdict.is_warning())
        .map(|r| (r, crate::verdict::explain(r.stats, r.change).join(" ")))
        .collect()
}

/// The patterns whose memory changed, largest change first (the diff's own order).
pub(crate) fn changes(diff: &ReportDiff) -> Vec<&PatternDiff> {
    diff.patterns
        .iter()
        .filter(|p| p.cause != Cause::Unchanged)
        .take(TOP_CHANGES)
        .collect()
}

/// `+1.2M`, `-300B`, `0B`.
pub(crate) fn signed_bytes(n: i64) -> String {
    match n {
        0 => format::bytes(0),
        n if n > 0 => format!("+{}", format::bytes(n as u64)),
        n => format!("-{}", format::bytes(n.unsigned_abs())),
    }
}

pub(crate) fn signed_count(n: i64) -> String {
    match n {
        0 => "0".to_string(),
        n if n > 0 => format!("+{}", format::grouped(n as u64)),
        n => format!("-{}", format::grouped(n.unsigned_abs())),
    }
}

/// The change of one pattern as text: bytes when both scans had sizes, else keys.
pub(crate) fn change_text(c: &PatternDiff) -> String {
    match c.delta_bytes {
        Some(d) => signed_bytes(d),
        None => format!("{} keys", signed_count(c.delta_keys)),
    }
}

/// Most slot ranges written out in the Nodes table; the rest are counted.
const MAX_SLOT_RANGES: usize = 3;

/// Column heads of the Nodes table, shared by the text, Markdown and HTML reports.
pub(crate) const NODE_HEADS: [&str; 8] = [
    "SLOTS", "ENDPOINT", "ROLE", "STATUS", "KEYS", "SAMPLED", "MEMORY", "NOTE",
];

/// Slot ranges as `0-5460`, joined with commas. A single slot is `5`; after
/// [`MAX_SLOT_RANGES`] ranges the rest are counted (`0-99,200-299,400-499,+2 more`).
pub(crate) fn slots_text(slots: &[(u16, u16)]) -> String {
    if slots.is_empty() {
        return "-".to_string();
    }
    let range = |&(a, b): &(u16, u16)| {
        if a == b {
            a.to_string()
        } else {
            format!("{a}-{b}")
        }
    };
    let mut parts: Vec<String> = slots.iter().take(MAX_SLOT_RANGES).map(range).collect();
    if slots.len() > MAX_SLOT_RANGES {
        parts.push(format!("+{} more", slots.len() - MAX_SLOT_RANGES));
    }
    parts.join(",")
}

pub(crate) fn role_word(r: NodeRole) -> &'static str {
    match r {
        NodeRole::Primary => "primary",
        NodeRole::Replica => "replica",
    }
}

pub(crate) fn status_word(s: NodeStatus) -> &'static str {
    match s {
        NodeStatus::Scanned => "scanned",
        NodeStatus::Partial => "partial",
        NodeStatus::Failed => "failed",
    }
}

/// The cells of one Nodes table row, in [`NODE_HEADS`] order. Unknown figures are `-`.
pub(crate) fn node_cells(n: &NodeReport) -> [String; 8] {
    [
        slots_text(&n.slots),
        n.endpoint.to_string(),
        role_word(n.role).to_string(),
        status_word(n.status).to_string(),
        n.server
            .as_ref()
            .map_or("-".to_string(), |s| format::count(s.total_keys)),
        format::count(n.sampled_keys),
        n.server
            .as_ref()
            .map_or("-".to_string(), |s| format::bytes(s.used_memory)),
        n.note.clone().unwrap_or_default(),
    ]
}

/// Nodes that were not scanned to the end, as `endpoint (status)`.
pub(crate) fn unfinished_nodes(report: &ScanReport) -> Vec<String> {
    report
        .nodes
        .iter()
        .flatten()
        .filter(|n| n.status != NodeStatus::Scanned)
        .map(|n| format!("{} ({})", n.endpoint, status_word(n.status)))
        .collect()
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::model::{KeySample, KeyType, ServerFlavor, ServerInfo, ServerMode, Ttl};

    pub fn stats(
        name: &str,
        sampled: u64,
        no_ttl: u64,
        bytes: Option<u64>,
        examples: &[&str],
    ) -> PatternStats {
        let mut hist = [0; 7];
        hist[0] = no_ttl;
        hist[3] = sampled - no_ttl;
        PatternStats {
            pattern: name.to_string(),
            key_type: Some(KeyType::Hash),
            sampled_keys: sampled,
            est_keys: sampled * 10,
            sampled_bytes: bytes,
            est_bytes: bytes.map(|b| b * 10),
            no_ttl,
            ttl_hist: hist,
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: examples
                .iter()
                .map(|k| KeySample {
                    key: k.to_string(),
                    key_type: KeyType::Hash,
                    bytes,
                    ttl: Ttl::Persistent,
                    lfu_freq: None,
                    node: None,
                })
                .collect(),
        }
    }

    pub fn report(patterns: Vec<PatternStats>, sizes: bool, at: i64) -> ScanReport {
        ScanReport {
            profile: "staging".into(),
            taken_at_unix: at,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7.4.2".into(),
                mode: ServerMode::Standalone,
                used_memory: 52_428_800,
                maxmemory: 0,
                maxmemory_policy: "allkeys-lfu".into(),
                total_keys: 10_000,
            },
            sampled_keys: patterns.iter().map(|p| p.sampled_keys).sum(),
            patterns,
            sizes_available: sizes,
            hot_keys: vec![],
            complete: true,
            match_glob: None,
            server_context: None,
            key_sizes: None,
            nodes: None,
        }
    }

    /// Three patterns: a leak (no TTL, growing), a mixed one and a healthy one.
    pub fn before_after() -> (ScanReport, ScanReport) {
        let before = report(
            vec![
                stats("user:{id}:cart", 200, 200, Some(1_000_000), &[]),
                stats("session:{id}", 300, 0, Some(500_000), &[]),
                stats("search:{id}", 100, 40, Some(100_000), &[]),
            ],
            true,
            1_709_210_096,
        );
        let after = report(
            vec![
                stats(
                    "user:{id}:cart",
                    200,
                    200,
                    Some(3_000_000),
                    &["user:1001:cart", "user:1002:cart"],
                ),
                stats("session:{id}", 300, 0, Some(500_000), &["session:abc"]),
                stats("search:{id}", 100, 40, Some(100_000), &[]),
                stats("job:{id}", 60, 60, Some(20_000), &[]),
            ],
            true,
            1_709_296_496,
        );
        (before, after)
    }

    /// [`before_after`]'s later report as a three-shard cluster scan: two nodes scanned, one
    /// failed. Made-up addresses.
    pub fn cluster_report() -> ScanReport {
        use crate::model::{Endpoint, NodeReport, NodeRole, NodeStatus};
        let (_, mut r) = before_after();
        r.server.mode = ServerMode::Cluster;
        r.complete = false;
        let base = r.server.clone();
        let node = |i: u16, slots: Vec<(u16, u16)>, status, note: Option<&str>, keys| {
            let up = status != NodeStatus::Failed;
            NodeReport {
                id: format!("{i:040}"),
                endpoint: Endpoint {
                    host: format!("10.0.1.{i}"),
                    port: 7000,
                    tls: false,
                },
                role: NodeRole::Primary,
                primary_id: format!("{i:040}"),
                slots,
                server: up.then(|| ServerInfo {
                    total_keys: keys,
                    used_memory: 10_485_760,
                    ..base.clone()
                }),
                status,
                note: note.map(str::to_string),
                sampled_keys: if up { 300 } else { 0 },
                complete: status == NodeStatus::Scanned,
                sizes_available: up,
                patterns: Vec::new(),
            }
        };
        r.nodes = Some(vec![
            node(1, vec![(0, 5460)], NodeStatus::Scanned, None, 3_400),
            node(
                2,
                vec![(5461, 9000), (9500, 10922)],
                NodeStatus::Partial,
                Some("the scan of this node stopped early"),
                3_300,
            ),
            node(
                3,
                vec![(10923, 16383)],
                NodeStatus::Failed,
                Some("this node could not be connected to"),
                0,
            ),
        ]);
        r
    }

    /// A pattern name and example keys written by an attacker.
    pub fn hostile() -> ScanReport {
        let evil = [
            "<script>alert(1)</script>",
            "a|b`c[d](e)<f>",
            "=cmd()|' /c calc'!A1",
            "line1\nline2",
            "+SUM(1,1)",
            "@x",
            "-2+3",
            "q\"uote's & amp",
        ];
        let mut patterns = Vec::new();
        for (i, name) in evil.iter().enumerate() {
            patterns.push(stats(
                name,
                100,
                100,
                Some(1_000 + i as u64),
                &[name, "\tTAB"],
            ));
        }
        report(patterns, true, 1_709_210_096)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::cluster_report;
    use super::*;

    #[test]
    fn slots_compact_and_truncated() {
        assert_eq!(slots_text(&[(0, 5460)]), "0-5460");
        assert_eq!(slots_text(&[(5, 5), (7, 9)]), "5,7-9");
        assert_eq!(slots_text(&[]), "-");
        let many: Vec<(u16, u16)> = (0..5).map(|i| (i * 100, i * 100 + 99)).collect();
        assert_eq!(slots_text(&many), "0-99,100-199,200-299,+2 more");
    }

    #[test]
    fn node_cells_use_dash_for_unknowns() {
        let r = cluster_report();
        let nodes = r.nodes.as_ref().unwrap();
        let ok = node_cells(&nodes[0]);
        assert_eq!(ok[..4], ["0-5460", "10.0.1.1:7000", "primary", "scanned"]);
        assert_eq!(ok[4], format::count(3_400));
        let failed = node_cells(&nodes[2]);
        assert_eq!((failed[3].as_str(), failed[4].as_str()), ("failed", "-"));
        assert_eq!(failed[6], "-");
    }

    #[test]
    fn unfinished_lists_partial_and_failed() {
        let r = cluster_report();
        assert_eq!(
            unfinished_nodes(&r),
            ["10.0.1.2:7000 (partial)", "10.0.1.3:7000 (failed)"]
        );
        assert!(unfinished_nodes(&fixtures::report(vec![], true, 0)).is_empty());
    }
}
