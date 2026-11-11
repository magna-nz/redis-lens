//! `redis-lens compare <left> <right>`: two instances' patterns side by side, e.g.
//! staging and prod.
//!
//! The logic lives in [`compare_profiles`] and [`render_compare`] so tests can run it
//! against a temp config and a `MemoryStore`; [`run`] wires it to the real keychain and
//! the terminal. Each side is scanned through the same path as `scan` and `diff`, with
//! its own profile's rate limit and read-only rule, one after the other; or read from a
//! snapshot file. Nothing is written to either server, and nothing is saved.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::sync::watch;

use super::CompareArgs;
use super::scan::{scan_profile_with_target, target_line, truncate, with_progress};
use crate::config::Config;
use crate::diff::{
    self, CompareSide, Comparison, PatternCompare, Presence, ShareBasis, TtlMismatch,
};
use crate::format;
use crate::model::ScanReport;
use crate::scan::ScanProgress;
use crate::secrets::{KeyringStore, SecretStore};
use crate::snapshot;

const MIN_WIDTH: usize = 60;
const MAX_ROWS: usize = 40;
const DEFAULT_WIDTH: usize = 100;

/// One side: the loaded snapshot file when there is one, else a scan of `profile`.
async fn side(
    config_path: &Path,
    secrets: &dyn SecretStore,
    profile: &str,
    file: Option<ScanReport>,
    args: &CompareArgs,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> anyhow::Result<ScanReport> {
    if let Some(report) = file {
        return Ok(report);
    }
    let (report, _perms, target) = scan_profile_with_target(
        config_path,
        secrets,
        profile,
        args.sample,
        None,
        args.max_rate,
        progress,
    )
    .await
    .with_context(|| format!("scanning {profile}"))?;
    if let Some(line) = target_line(&target) {
        eprintln!("{profile}: {line}");
    }
    Ok(report)
}

/// Load or scan the left side, then the right, and compare them. Scans run one after
/// the other, so the two never add up on a shared server.
pub async fn compare_profiles(
    config_path: &Path,
    secrets: &dyn SecretStore,
    args: &CompareArgs,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> anyhow::Result<(Comparison, ScanReport, ScanReport)> {
    // Read both files first, so a bad path fails before any server is touched.
    let load = |p: &Option<PathBuf>| p.as_deref().map(snapshot::load).transpose();
    let (left_file, right_file) = (load(&args.left_file)?, load(&args.right_file)?);
    let left = side(config_path, secrets, &args.left, left_file, args, progress).await?;
    let right = side(
        config_path,
        secrets,
        &args.right,
        right_file,
        args,
        progress,
    )
    .await?;
    Ok((diff::compare(&left, &right), left, right))
}

fn dash() -> String {
    "—".to_string()
}

fn side_line(tag: &str, label: &str, s: &diff::CompareSummary) -> String {
    let fraction = s.sample_fraction.map_or_else(dash, format::percent);
    let mut out = format!(
        "{tag}  {label}: sampled {} of {} keys ({fraction}) · {}",
        format::grouped(s.sampled_keys),
        format::grouped(s.total_keys),
        snapshot::format_utc(s.taken_at_unix)
    );
    if let Some(g) = &s.match_glob {
        out.push_str(&format!(" · MATCH {g}"));
    }
    if !s.sizes_available {
        out.push_str(" · no sizes");
    }
    out
}

/// The note for a pattern: the TTL mismatch first, then a side it is missing from.
fn note(p: &PatternCompare, left: &str, right: &str, same_keyspace: bool) -> String {
    let maybe = if same_keyspace { "" } else { "?" };
    match (p.ttl_mismatch, p.presence) {
        (Some(TtlMismatch::NoTtlOnLeft), _) => format!("no TTL on {left}, set on {right}"),
        (Some(TtlMismatch::NoTtlOnRight), _) => format!("no TTL on {right}, set on {left}"),
        (None, Presence::LeftOnly) => format!("only on {left}{maybe}"),
        (None, Presence::RightOnly) => format!("only on {right}{maybe}"),
        (None, Presence::Both) => String::new(),
    }
}

fn share(s: &Option<CompareSide>) -> String {
    s.as_ref().map_or_else(dash, |s| format::percent(s.share))
}

fn no_ttl(s: &Option<CompareSide>) -> String {
    s.as_ref().map_or_else(dash, |s| format::percent(s.no_ttl))
}

fn ratio(r: Option<f64>) -> String {
    r.map_or_else(dash, format::ratio)
}

/// Lays out `rows` under `heads` after a pattern column, cutting the pattern to fit.
fn table(
    names: &[&str],
    heads: &[&str],
    rows: &[Vec<String>],
    left_aligned: &[bool],
    width: usize,
) -> Vec<String> {
    let mut w: Vec<usize> = heads.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let fixed: usize = w.iter().sum::<usize>() + 2 * w.len();
    let pat_w = names
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0)
        .max("PATTERN".len())
        .min(width.saturating_sub(fixed).max(8));
    let line = |pat: &str, cells: &[&str]| -> String {
        let mut out = format!("{:<pat_w$}", truncate(pat, pat_w));
        for (i, c) in cells.iter().enumerate() {
            if left_aligned[i] {
                out.push_str(&format!("  {c:<w$}", w = w[i]));
            } else {
                out.push_str(&format!("  {c:>w$}", w = w[i]));
            }
        }
        out
    };
    let mut out = vec![line("PATTERN", heads)];
    for (n, r) in names.iter().zip(rows) {
        let cells: Vec<&str> = r.iter().map(String::as_str).collect();
        out.push(line(n, &cells));
    }
    out
}

/// The comparison as plain text that fits `width` columns (at least 60): shares, ratios
/// and TTL first, absolute numbers after. `left` and `right` label the two sides.
pub fn render_compare(c: &Comparison, left: &str, right: &str, width: usize) -> String {
    let width = width.max(MIN_WIDTH);
    let basis = match c.share_basis {
        ShareBasis::Bytes => "estimated memory",
        ShareBasis::Keys => "estimated keys (sizes unavailable on a side)",
    };
    let mut lines = vec![
        side_line("L", left, &c.left),
        side_line("R", right, &c.right),
        format!("Shares are of each side's {basis}; ratios are R / L."),
    ];
    if !c.same_keyspace {
        lines.push(
            "Note: the MATCH globs differ; a pattern on one side only (?) may be outside the other's glob."
                .to_string(),
        );
    }
    if !(c.left.complete && c.right.complete) {
        lines.push(
            "Note: a side was sampled, so a rare pattern may exist on both but be sampled on one only."
                .to_string(),
        );
    }
    let flagged = c
        .patterns
        .iter()
        .filter(|p| p.ttl_mismatch.is_some())
        .count();
    let one_side = c
        .patterns
        .iter()
        .filter(|p| p.presence != Presence::Both)
        .count();
    lines.push(if flagged > 0 {
        format!("Likely bugs: {flagged} pattern(s) have no TTL on one side but TTLs on the other.")
    } else {
        "No pattern has a TTL on one side only.".to_string()
    });
    if one_side > 0 {
        lines.push(format!(
            "{one_side} pattern(s) were found on one side only."
        ));
    }
    lines.push(String::new());
    if c.patterns.is_empty() {
        lines.push("Neither side sampled any keys.".to_string());
        return finish(lines, width);
    }

    let shown = &c.patterns[..c.patterns.len().min(MAX_ROWS)];
    let names: Vec<&str> = shown.iter().map(|p| p.pattern.as_str()).collect();
    let rows: Vec<Vec<String>> = shown
        .iter()
        .map(|p| {
            vec![
                share(&p.left),
                share(&p.right),
                ratio(p.key_ratio),
                ratio(p.avg_size_ratio),
                no_ttl(&p.left),
                no_ttl(&p.right),
                note(p, left, right, c.same_keyspace),
            ]
        })
        .collect();
    lines.extend(table(
        &names,
        &[
            "SHARE L", "SHARE R", "KEYS R/L", "AVG R/L", "NO TTL L", "NO TTL R", "NOTE",
        ],
        &rows,
        &[false, false, false, false, false, false, true],
        width,
    ));
    lines.push(String::new());
    lines.push("Absolute numbers (estimated)".to_string());
    let bytes = |s: &Option<CompareSide>| {
        s.as_ref()
            .and_then(|s| s.est_bytes)
            .map_or_else(dash, format::bytes)
    };
    let keys =
        |s: &Option<CompareSide>| s.as_ref().map_or_else(dash, |s| format::count(s.est_keys));
    let avg = |s: &Option<CompareSide>| {
        s.as_ref()
            .and_then(|s| s.avg_bytes)
            .map_or_else(dash, |a| format::bytes(a.round() as u64))
    };
    let abs_rows: Vec<Vec<String>> = shown
        .iter()
        .map(|p| {
            vec![
                keys(&p.left),
                keys(&p.right),
                bytes(&p.left),
                bytes(&p.right),
                avg(&p.left),
                avg(&p.right),
            ]
        })
        .collect();
    lines.extend(table(
        &names,
        &["KEYS L", "KEYS R", "MEM L", "MEM R", "AVG L", "AVG R"],
        &abs_rows,
        &[false; 6],
        width,
    ));
    if c.patterns.len() > MAX_ROWS {
        lines.push(format!(
            "… and {} more patterns; use --json for all",
            c.patterns.len() - MAX_ROWS
        ));
    }
    finish(lines, width)
}

fn finish(lines: Vec<String>, width: usize) -> String {
    lines
        .into_iter()
        .map(|l| truncate(l.trim_end(), width))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn run(config: Option<PathBuf>, args: CompareArgs) -> anyhow::Result<()> {
    let path = match config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    let secrets = KeyringStore;
    let result = with_progress(|tx| {
        let (path, secrets, args) = (&path, &secrets, &args);
        async move { compare_profiles(path, secrets, args, tx.as_ref()).await }
    })
    .await;
    let (c, _, _) = result?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&c).context("could not serialize the comparison")?
        );
    } else {
        let width = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(DEFAULT_WIDTH);
        println!("{}", render_compare(&c, &args.left, &args.right, width));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PatternStats, ServerFlavor, ServerInfo, ServerMode};

    const MB: u64 = 1024 * 1024;

    fn stats(name: &str, keys: u64, bytes: Option<u64>, no_ttl: u64) -> PatternStats {
        PatternStats {
            pattern: name.into(),
            key_type: None,
            sampled_keys: keys,
            est_keys: keys,
            sampled_bytes: bytes,
            est_bytes: bytes,
            no_ttl,
            ttl_hist: [0; 7],
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: vec![],
        }
    }

    fn report(profile: &str, patterns: Vec<PatternStats>, total: u64, sampled: u64) -> ScanReport {
        ScanReport {
            profile: profile.into(),
            taken_at_unix: 1_794_319_331,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7".into(),
                mode: ServerMode::Standalone,
                used_memory: 1,
                maxmemory: 0,
                maxmemory_policy: "noeviction".into(),
                total_keys: total,
            },
            sampled_keys: sampled,
            patterns,
            sizes_available: true,
            hot_keys: vec![],
            complete: sampled >= total,
            match_glob: None,
            server_context: None,
            nodes: None,
        }
    }

    fn staging_and_prod() -> (ScanReport, ScanReport) {
        let staging = report(
            "staging",
            vec![
                stats("cart:{id}", 1_000, Some(10 * MB), 0),
                stats("session:{id}", 500, Some(5 * MB), 0),
                stats("debug:{id}", 10, Some(MB), 10),
            ],
            1_510,
            1_510,
        );
        let prod = report(
            "prod",
            vec![
                stats("cart:{id}", 100_000, Some(1_000 * MB), 90_000),
                stats("session:{id}", 50_000, Some(1_000 * MB), 0),
            ],
            1_500_000,
            150_000,
        );
        (staging, prod)
    }

    #[test]
    fn leads_with_shares_ratios_and_ttl() {
        let (staging, prod) = staging_and_prod();
        let c = diff::compare(&staging, &prod);
        let t = render_compare(&c, "staging", "prod", 120);
        println!("{t}");
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(
            lines[0],
            "L  staging: sampled 1,510 of 1,510 keys (100%) · 2026-11-10 14:02:11 UTC"
        );
        assert_eq!(
            lines[1],
            "R  prod: sampled 150,000 of 1,500,000 keys (10%) · 2026-11-10 14:02:11 UTC"
        );
        assert_eq!(
            lines[2],
            "Shares are of each side's estimated memory; ratios are R / L."
        );
        assert_eq!(
            lines[3],
            "Note: a side was sampled, so a rare pattern may exist on both but be sampled on one only."
        );
        assert_eq!(
            lines[4],
            "Likely bugs: 1 pattern(s) have no TTL on one side but TTLs on the other."
        );
        assert_eq!(lines[5], "1 pattern(s) were found on one side only.");
        // The flagged pattern leads, though session has the larger prod share.
        assert_eq!(
            lines[7..11],
            [
                "PATTERN       SHARE L  SHARE R  KEYS R/L  AVG R/L  NO TTL L  NO TTL R  NOTE",
                "cart:{id}         63%      50%      ×100    ×1.00        0%       90%  no TTL on prod, set on staging",
                "session:{id}      31%      50%      ×100    ×2.00        0%        0%",
                "debug:{id}         6%        —         —        —      100%         —  only on staging",
            ]
        );
        // Absolute numbers come second.
        let abs = lines
            .iter()
            .position(|l| *l == "Absolute numbers (estimated)")
            .unwrap();
        assert!(abs > 10);
        assert_eq!(
            lines[abs + 1..],
            [
                "PATTERN       KEYS L  KEYS R  MEM L  MEM R  AVG L  AVG R",
                "cart:{id}       1.0K    100K    10M  1000M    10K    10K",
                "session:{id}     500     50K   5.0M  1000M    10K    20K",
                "debug:{id}        10       —   1.0M      —   102K      —",
            ]
        );
        assert!(!t.contains('\x1b'));
    }

    #[test]
    fn fits_narrow_terminals() {
        let (staging, prod) = staging_and_prod();
        let c = diff::compare(&staging, &prod);
        for w in [10, 60, 80] {
            for l in render_compare(&c, "staging", "prod", w).lines() {
                assert!(l.chars().count() <= w.max(MIN_WIDTH), "{l:?}");
            }
        }
    }

    #[test]
    fn same_report_on_both_sides_is_unchanged() {
        let (staging, _) = staging_and_prod();
        let c = diff::compare(&staging, &staging);
        let t = render_compare(&c, "staging", "staging", 120);
        assert!(t.contains("No pattern has a TTL on one side only."), "{t}");
        assert!(!t.contains("found on one side only"), "{t}");
        for p in &c.patterns {
            assert_eq!(p.key_ratio, Some(1.0));
        }
        let row = t.lines().find(|l| l.starts_with("cart:")).unwrap();
        assert!(row.contains("×1.00"), "{row}");
    }

    #[test]
    fn without_sizes_and_with_different_globs() {
        let (staging, mut prod) = staging_and_prod();
        prod.sizes_available = false;
        prod.match_glob = Some("cart:*".into());
        for p in &mut prod.patterns {
            p.est_bytes = None;
        }
        let c = diff::compare(&staging, &prod);
        let t = render_compare(&c, "staging", "prod", 200);
        assert!(
            t.contains("estimated keys (sizes unavailable on a side)"),
            "{t}"
        );
        assert!(t.contains("the MATCH globs differ"), "{t}");
        assert!(t.contains("only on staging?"), "{t}");
        assert!(t.contains(" · MATCH cart:* · no sizes"), "{t}");
        let empty = diff::compare(&report("a", vec![], 0, 0), &report("b", vec![], 0, 0));
        let t = render_compare(&empty, "a", "b", 80);
        assert!(t.ends_with("Neither side sampled any keys."), "{t}");
        assert!(t.contains("sampled 0 of 0 keys (—)"), "{t}");
    }

    #[tokio::test]
    async fn files_on_both_sides_need_no_server() {
        let dir = std::env::temp_dir().join(format!("redis-lens-compare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (staging, prod) = staging_and_prod();
        let lf = snapshot::save(&dir, &staging).unwrap();
        let rf = snapshot::save(&dir, &prod).unwrap();
        let args = CompareArgs {
            left: "staging".into(),
            right: "prod".into(),
            left_file: Some(lf),
            right_file: Some(rf),
            sample: 10,
            max_rate: None,
            json: true,
        };
        // No config file exists: a scan would fail on it.
        let (c, l, r) = compare_profiles(
            &dir.join("no-such-config.toml"),
            &crate::secrets::MemoryStore::default(),
            &args,
            None,
        )
        .await
        .unwrap();
        assert_eq!((l, r), (staging.clone(), prod.clone()));
        assert_eq!(c, diff::compare(&staging, &prod));
        // A bad right file fails before the left side is scanned.
        let bad = CompareArgs {
            left_file: None,
            right_file: Some(dir.join("absent.json")),
            ..args
        };
        let err = compare_profiles(
            &dir.join("no-such-config.toml"),
            &crate::secrets::MemoryStore::default(),
            &bad,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().starts_with("could not access"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compare_flags_parse_and_reach_the_scan_options() {
        use crate::cli::scan::{profile_in, scan_options};
        use crate::model::Env;
        use clap::Parser;
        let parse = |extra: &[&str]| {
            let mut argv = vec!["redis-lens", "compare", "staging", "prod"];
            argv.extend_from_slice(extra);
            match crate::cli::Cli::try_parse_from(argv).unwrap().command {
                Some(crate::cli::Command::Compare(a)) => a,
                other => panic!("not compare: {other:?}"),
            }
        };
        let a = parse(&["--sample", "500", "--max-rate", "250", "--json"]);
        assert_eq!((a.sample, a.max_rate, a.json), (500, Some(250), true));
        // The same call `side` makes for each scan.
        let rate = |env, a: &CompareArgs| {
            scan_options(&profile_in(env), a.sample, None, a.max_rate).max_commands_per_sec
        };
        assert_eq!(rate(Env::Prod, &a), Some(250));
        assert_eq!(rate(Env::Prod, &parse(&[])), Some(5_000));
        assert_eq!(rate(Env::Local, &parse(&[])), None);
        let a = parse(&["--left-file", "a.json", "--right-file", "b.json"]);
        assert_eq!(a.left_file.as_deref(), Some(Path::new("a.json")));
        assert_eq!(a.right_file.as_deref(), Some(Path::new("b.json")));
    }
}
