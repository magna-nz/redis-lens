//! `redis-lens trend <profile>`: each pattern's memory over the profile's recent
//! snapshots, as a sparkline with a least-squares growth rate.
//!
//! The logic lives in [`load_trend`] and [`render_trend`] so tests can run it against a
//! temp snapshot directory; [`run`] wires it to the data directory and the terminal. It
//! reads local snapshot files only, never a server.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::TrendArgs;
use super::scan::truncate;
use crate::format;
use crate::snapshot::{self, PatternTrend, Trend, TrendMetric};

const MIN_WIDTH: usize = 60;
const MAX_ROWS: usize = 40;
const DEFAULT_WIDTH: usize = 100;

/// The trend over `profile`'s newest `last` snapshots in `dir` that share the newest
/// one's `MATCH` glob, with only the patterns containing `pattern` when given. Also
/// returns how many newer-or-interleaved snapshots were left out for another glob.
pub fn load_trend(
    dir: &Path,
    profile: &str,
    last: usize,
    pattern: Option<&str>,
) -> anyhow::Result<(Trend, usize)> {
    if last < 2 {
        anyhow::bail!("--last must be at least 2: a trend needs two snapshots");
    }
    let newest = snapshot::list(dir, profile)?
        .into_iter()
        .find_map(|info| snapshot::load(&info.path).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no snapshot for {profile}; run `redis-lens scan {profile} --save` first"
            )
        })?;
    let glob = newest.match_glob.clone();
    let mut reports = snapshot::recent_matching(dir, profile, i64::MAX, glob.as_deref(), last)?;
    let oldest = reports.last().map_or(i64::MAX, |r| r.taken_at_unix);
    // Snapshots inside the window that were left out for another glob.
    let skipped = snapshot::list(dir, profile)?
        .into_iter()
        .filter(|i| i.taken_at_unix >= oldest)
        .count()
        .saturating_sub(reports.len());
    reports.reverse();
    let mut t = snapshot::trend(&reports);
    if let Some(p) = pattern {
        t.patterns.retain(|pt| pt.pattern.contains(p));
    }
    Ok((t, skipped))
}

fn value_fmt(metric: TrendMetric) -> fn(u64) -> String {
    match metric {
        TrendMetric::Bytes => format::bytes,
        TrendMetric::Keys => format::count,
    }
}

/// `+2.1%/d` and `+94M/d`, or dashes without a rate.
fn growth_cells(p: &PatternTrend, metric: TrendMetric) -> (String, String) {
    let dash = || "—".to_string();
    match p.growth {
        Some(g) => (
            g.ratio_per_day
                .map_or_else(dash, |r| format!("{}/d", format::signed_percent1(r))),
            format!("{}/d", format::signed_with(g.per_day, value_fmt(metric))),
        ),
        None => (dash(), dash()),
    }
}

/// The trend as plain text that fits `width` columns (at least 60).
pub fn render_trend(t: &Trend, skipped: usize, pattern: Option<&str>, width: usize) -> String {
    let width = width.max(MIN_WIDTH);
    let fmt = value_fmt(t.metric);
    let (first, last) = (
        t.points.first().map_or(0, |p| p.taken_at_unix),
        t.points.last().map_or(0, |p| p.taken_at_unix),
    );
    let what = match t.metric {
        TrendMetric::Bytes => "estimated memory",
        TrendMetric::Keys => "estimated keys (no sizes)",
    };
    let mut lines = vec![
        format!(
            "Trend:   {} · {} snapshot(s) · {what}",
            t.profile,
            t.points.len()
        ),
        format!(
            "From:    {} → {}",
            snapshot::format_utc(first),
            snapshot::format_utc(last)
        ),
    ];
    if let Some(g) = &t.match_glob {
        lines.push(format!("Match:   {g} (only snapshots with this glob)"));
    }
    let partial = t.partial_points();
    if partial > 0 {
        // Mark the partial scans under the sparkline positions, oldest first.
        let marks: String = t
            .points
            .iter()
            .map(|p| if p.complete { '·' } else { 'p' })
            .collect();
        lines.push(format!(
            "Partial: {partial} of {} scans sampled part of the keyspace: {marks} (p = partial)",
            t.points.len()
        ));
    }
    if skipped > 0 {
        lines.push(format!(
            "Note:    {skipped} snapshot(s) taken with another MATCH glob were left out"
        ));
    }
    if t.points.len() < 2 {
        lines.push("Note:    only one snapshot: no growth rate yet".to_string());
    }
    lines.push(String::new());
    if t.patterns.is_empty() {
        lines.push(match pattern {
            Some(p) => format!("No pattern contains \"{p}\"."),
            None => "No patterns in these snapshots.".to_string(),
        });
        return finish(lines, width);
    }

    let shown = &t.patterns[..t.patterns.len().min(MAX_ROWS)];
    let rows: Vec<[String; 5]> = shown
        .iter()
        .map(|p| {
            let (pct, abs) = growth_cells(p, t.metric);
            let first = p.values.iter().flatten().next().copied();
            let last = p.values.iter().rev().flatten().next().copied();
            [
                format::sparkline(&p.values),
                first.map_or_else(|| "—".to_string(), fmt),
                last.map_or_else(|| "—".to_string(), fmt),
                pct,
                abs,
            ]
        })
        .collect();
    let heads = ["TREND", "FIRST", "LAST", "GROWTH", "PER DAY"];
    let mut w = [0usize; 5];
    for (i, h) in heads.iter().enumerate() {
        w[i] = h.chars().count();
        for r in &rows {
            w[i] = w[i].max(r[i].chars().count());
        }
    }
    let fixed: usize = w.iter().sum::<usize>() + 2 * 5;
    let pat_w = shown
        .iter()
        .map(|p| p.pattern.chars().count())
        .max()
        .unwrap_or(0)
        .max("PATTERN".len())
        .min(width.saturating_sub(fixed).max(8));
    let line = |pat: &str, cells: [&str; 5]| -> String {
        let mut out = format!("{:<pat_w$}", truncate(pat, pat_w));
        for (i, c) in cells.iter().enumerate() {
            if i == 0 {
                out.push_str(&format!("  {c:<w0$}", w0 = w[0]));
            } else {
                out.push_str(&format!("  {c:>wi$}", wi = w[i]));
            }
        }
        out
    };
    lines.push(line("PATTERN", heads));
    for (p, r) in shown.iter().zip(&rows) {
        lines.push(line(&p.pattern, [&r[0], &r[1], &r[2], &r[3], &r[4]]));
    }
    if t.patterns.len() > MAX_ROWS {
        lines.push(format!(
            "… and {} more patterns; use --pattern or --json",
            t.patterns.len() - MAX_ROWS
        ));
    }
    lines.push(String::new());
    lines.push("Growth:  least-squares fit per day, as a share of the pattern's mean".to_string());
    lines.push("         · in TREND is a snapshot without the pattern (a gap, not 0)".to_string());
    finish(lines, width)
}

fn finish(lines: Vec<String>, width: usize) -> String {
    lines
        .into_iter()
        .map(|l| truncate(l.trim_end(), width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The JSON form: the [`Trend`] with each pattern's sparkline added.
#[derive(serde::Serialize)]
struct TrendJson<'a> {
    #[serde(flatten)]
    trend: &'a Trend,
    sparklines: Vec<(&'a str, String)>,
    skipped_other_glob: usize,
}

pub fn to_json(t: &Trend, skipped: usize) -> anyhow::Result<String> {
    let json = TrendJson {
        trend: t,
        sparklines: t
            .patterns
            .iter()
            .map(|p| (p.pattern.as_str(), format::sparkline(&p.values)))
            .collect(),
        skipped_other_glob: skipped,
    };
    serde_json::to_string_pretty(&json).context("could not serialize the trend")
}

pub async fn run(_config: Option<PathBuf>, args: TrendArgs) -> anyhow::Result<()> {
    let dir = snapshot::default_dir()?;
    let (t, skipped) = load_trend(&dir, &args.profile, args.last, args.pattern.as_deref())?;
    if args.json {
        println!("{}", to_json(&t, skipped)?);
    } else {
        let width = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(DEFAULT_WIDTH);
        println!(
            "{}",
            render_trend(&t, skipped, args.pattern.as_deref(), width)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PatternStats, ScanReport, ServerFlavor, ServerInfo, ServerMode};

    const DAY: i64 = 86_400;
    const T0: i64 = 1_794_319_331 - 6 * DAY;
    const MB: u64 = 1024 * 1024;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redis-lens-trend-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn stats(name: &str, keys: u64, bytes: u64) -> PatternStats {
        PatternStats {
            pattern: name.into(),
            key_type: None,
            sampled_keys: keys,
            est_keys: keys,
            sampled_bytes: Some(bytes),
            est_bytes: Some(bytes),
            no_ttl: 0,
            ttl_hist: [0; 7],
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: vec![],
        }
    }

    fn report(at: i64, patterns: Vec<PatternStats>) -> ScanReport {
        ScanReport {
            profile: "p".into(),
            taken_at_unix: at,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7".into(),
                mode: ServerMode::Standalone,
                used_memory: 1,
                maxmemory: 0,
                maxmemory_policy: "noeviction".into(),
                total_keys: 0,
            },
            sampled_keys: 100,
            patterns,
            sizes_available: true,
            hot_keys: vec![],
            complete: true,
            match_glob: None,
            server_context: None,
            key_sizes: None,
            nodes: None,
        }
    }

    /// Seven daily snapshots: `cart` leaks 2 MiB a day from 100 MiB, `session` is flat,
    /// `job` shows up in some only, `tmp` shrinks. Day 3 was a partial scan, and a
    /// snapshot with a glob sits between days 4 and 5.
    fn seven(tag: &str) -> PathBuf {
        let dir = temp_dir(tag);
        for d in 0..7i64 {
            let mut pats = vec![
                stats("cart:{id}", 1000, 100 * MB + d as u64 * 2 * MB),
                stats("session:{id}", 500, 50 * MB),
                stats("tmp:{id}", 10, (70 - 10 * d) as u64 * MB),
            ];
            if d % 3 == 0 {
                pats.push(stats("job:{id}", 5, MB));
            }
            let mut r = report(T0 + d * DAY, pats);
            r.complete = d != 3;
            snapshot::save(&dir, &r).unwrap();
        }
        let mut g = report(T0 + 4 * DAY + 3600, vec![stats("user:{id}", 1, 1)]);
        g.match_glob = Some("user:*".into());
        snapshot::save(&dir, &g).unwrap();
        dir
    }

    #[test]
    fn trend_table() {
        let dir = seven("table");
        let (t, skipped) = load_trend(&dir, "p", 7, None).unwrap();
        assert_eq!(skipped, 1);
        assert_eq!(t.points.len(), 7);
        let text = render_trend(&t, skipped, None, 100);
        println!("{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Trend:   p · 7 snapshot(s) · estimated memory");
        assert_eq!(
            lines[1],
            "From:    2026-11-04 14:02:11 UTC → 2026-11-10 14:02:11 UTC"
        );
        assert_eq!(
            lines[2],
            "Partial: 1 of 7 scans sampled part of the keyspace: ···p··· (p = partial)"
        );
        assert_eq!(
            lines[3],
            "Note:    1 snapshot(s) taken with another MATCH glob were left out"
        );
        assert_eq!(
            &lines[5..],
            [
                "PATTERN       TREND    FIRST  LAST   GROWTH  PER DAY",
                // Sorted by growth: the leak first, the shrinking pattern last.
                "cart:{id}     ▁▂▃▅▆▇█   100M  112M  +1.9%/d  +2.0M/d",
                "job:{id}      ▄··▄··▄   1.0M  1.0M   0.0%/d     0B/d",
                "session:{id}  ▄▄▄▄▄▄▄    50M   50M   0.0%/d     0B/d",
                "tmp:{id}      █▇▆▅▃▂▁    70M   10M   -25%/d   -10M/d",
                "",
                "Growth:  least-squares fit per day, as a share of the pattern's mean",
                "         · in TREND is a snapshot without the pattern (a gap, not 0)",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_and_pattern_narrow_it() {
        let dir = seven("narrow");
        let (t, _) = load_trend(&dir, "p", 3, Some("cart")).unwrap();
        assert_eq!(t.points.len(), 3);
        assert_eq!(t.points[0].taken_at_unix, T0 + 4 * DAY);
        assert_eq!(t.patterns.len(), 1);
        assert_eq!(
            t.patterns[0].values,
            [Some(108 * MB), Some(110 * MB), Some(112 * MB)]
        );
        let (t, _) = load_trend(&dir, "p", 7, Some("nope")).unwrap();
        let text = render_trend(&t, 0, Some("nope"), 80);
        assert!(text.ends_with("No pattern contains \"nope\"."), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn follows_the_newest_snapshots_glob() {
        let dir = seven("glob");
        let mut g = report(T0 + 10 * DAY, vec![stats("user:{id}", 2, 2)]);
        g.match_glob = Some("user:*".into());
        snapshot::save(&dir, &g).unwrap();
        let (t, skipped) = load_trend(&dir, "p", 7, None).unwrap();
        assert_eq!(t.match_glob.as_deref(), Some("user:*"));
        assert_eq!(t.points.len(), 2);
        // Every snapshot between the two user:* ones had no glob.
        assert_eq!(skipped, 2);
        let text = render_trend(&t, skipped, None, 100);
        assert!(text.contains("Match:   user:* (only snapshots with this glob)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_and_single_snapshots() {
        let dir = temp_dir("errors");
        let err = load_trend(&dir, "p", 7, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no snapshot for p; run `redis-lens scan p --save` first"
        );
        assert!(
            load_trend(&dir, "p", 1, None)
                .unwrap_err()
                .to_string()
                .starts_with("--last must be at least 2")
        );
        snapshot::save(&dir, &report(T0, vec![stats("a", 1, MB)])).unwrap();
        let (t, _) = load_trend(&dir, "p", 7, None).unwrap();
        let text = render_trend(&t, 0, None, 80);
        assert!(
            text.contains("only one snapshot: no growth rate yet"),
            "{text}"
        );
        let row = text.lines().find(|l| l.starts_with("a ")).unwrap();
        assert!(row.contains('▄') && row.ends_with("—"), "{row}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_without_sizes_and_narrow_widths() {
        let dir = temp_dir("keys");
        for d in 0..3i64 {
            let mut r = report(
                T0 + d * DAY,
                vec![stats(
                    "very:long:{id}:pattern:with:{uuid}:many:segments:that:go:on",
                    1000 + d as u64 * 100,
                    1,
                )],
            );
            r.sizes_available = false;
            r.patterns[0].est_bytes = None;
            snapshot::save(&dir, &r).unwrap();
        }
        let (t, _) = load_trend(&dir, "p", 7, None).unwrap();
        assert_eq!(t.metric, TrendMetric::Keys);
        let text = render_trend(&t, 0, None, 60);
        println!("{text}");
        assert!(text.contains("estimated keys (no sizes)"), "{text}");
        for l in text.lines() {
            assert!(l.chars().count() <= 60, "{l:?}");
        }
        let row = text.lines().find(|l| l.starts_with("very")).unwrap();
        assert!(row.contains("+9.1%/d") && row.contains("+100/d"), "{row}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_has_values_gaps_and_sparklines() {
        let dir = seven("json");
        let (t, skipped) = load_trend(&dir, "p", 7, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&to_json(&t, skipped).unwrap()).unwrap();
        assert_eq!(v["metric"], "bytes");
        assert_eq!(v["profile"], "p");
        assert_eq!(v["points"][3]["complete"], false);
        assert_eq!(v["skipped_other_glob"], 1);
        let job = v["patterns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pattern"] == "job:{id}")
            .unwrap();
        assert_eq!(job["values"][1], serde_json::Value::Null);
        assert_eq!(v["sparklines"][0][0], "cart:{id}");
        assert_eq!(v["sparklines"][0][1], "▁▂▃▅▆▇█");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
