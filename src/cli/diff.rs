//! `redis-lens diff <profile>`: scan, then compare with the previous snapshot.
//!
//! The logic lives in [`diff_profile`] and [`render_diff`] so tests can run it against a
//! temp config, a `MemoryStore` and a temp snapshot directory; [`run`] wires it to the real
//! keychain, the data directory and the terminal.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::DiffArgs;
use super::scan::{scan_profile_with_target, target_line, truncate, with_progress};
use crate::config::Config;
use crate::diff::{self, Cause, PatternDiff, ReportDiff};
use crate::format;
use crate::model::ScanReport;
use crate::scan::ScanProgress;
use crate::secrets::{KeyringStore, SecretStore};
use crate::snapshot;
use tokio::sync::watch;

const MIN_WIDTH: usize = 60;
const MAX_ROWS: usize = 40;
const DEFAULT_WIDTH: usize = 100;

fn no_snapshot(profile: &str) -> anyhow::Error {
    anyhow::anyhow!("no snapshot for {profile}; run `redis-lens scan {profile} --save` first")
}

/// Scan the profile, compare with `--against` or the newest earlier snapshot in `dir`, and
/// save the new scan into `dir` so the next diff compares against it. Returns the diff and
/// the new report.
pub async fn diff_profile(
    config_path: &Path,
    secrets: &dyn SecretStore,
    args: &DiffArgs,
    dir: &Path,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> anyhow::Result<(ReportDiff, ScanReport)> {
    // Fail before touching the server when there is nothing to compare with.
    let against = match &args.against {
        Some(path) => Some(snapshot::load(path)?),
        None => {
            // The scan uses no MATCH glob, so only a baseline with none counts.
            if snapshot::latest_before(dir, &args.profile, i64::MAX, None)?.is_none() {
                return Err(no_snapshot(&args.profile));
            }
            None
        }
    };
    let (new, _perms, target) = scan_profile_with_target(
        config_path,
        secrets,
        &args.profile,
        100_000,
        None,
        args.max_rate,
        progress,
    )
    .await?;
    let before = match against {
        Some(r) => r,
        None => snapshot::latest_before(
            dir,
            &new.profile,
            new.taken_at_unix,
            new.match_glob.as_deref(),
        )?
        .ok_or_else(|| no_snapshot(&args.profile))?,
    };
    let result = diff::diff(&before, &new);
    if let Some(line) = target_line(&target) {
        eprintln!("{line}");
    }
    let path = snapshot::save(dir, &new)?;
    eprintln!("Saved snapshot {}", path.display());
    Ok((result, new))
}

fn cause_words(c: Cause) -> &'static str {
    c.words()
}

fn signed(n: i64, fmt: impl Fn(u64) -> String) -> String {
    match n {
        0 => fmt(0),
        n if n > 0 => format!("+{}", fmt(n as u64)),
        n => format!("-{}", fmt(n.unsigned_abs())),
    }
}

/// The diff as plain text that fits `width` columns (at least 60).
pub fn render_diff(d: &ReportDiff, width: usize) -> String {
    let width = width.max(MIN_WIDTH);
    let sizes = d.patterns.iter().all(|p| p.delta_bytes.is_some());
    let mut lines = vec![
        format!(
            "Since:   {} → {}",
            snapshot::format_utc(d.before_taken_at_unix),
            snapshot::format_utc(d.after_taken_at_unix)
        ),
        format!(
            "Memory:  {} → {} used ({})",
            format::bytes(d.used_memory_before),
            format::bytes(d.used_memory_after),
            signed(
                diff::sub(d.used_memory_after, d.used_memory_before),
                format::bytes
            )
        ),
    ];
    if !sizes {
        lines.push("Note:    sizes unavailable: comparing key counts".to_string());
    }
    lines.push(String::new());

    let shown: &[PatternDiff] = &d.patterns[..d.patterns.len().min(MAX_ROWS)];
    let rows: Vec<[String; 5]> = shown
        .iter()
        .map(|p| {
            let (before, after, delta, base) = if sizes {
                (
                    p.bytes_before.map(format::bytes),
                    p.bytes_after.map(format::bytes),
                    signed(p.delta_bytes.unwrap_or(0), format::bytes),
                    p.bytes_before,
                )
            } else {
                (
                    p.keys_before.map(format::count),
                    p.keys_after.map(format::count),
                    signed(p.delta_keys, format::count),
                    p.keys_before,
                )
            };
            let change = if sizes {
                p.delta_bytes.unwrap_or(0)
            } else {
                p.delta_keys
            };
            let pct = match base {
                Some(b) if b > 0 => {
                    let r = change as f64 / b as f64;
                    let sign = if change < 0 { "-" } else { "+" };
                    if change == 0 {
                        "0%".to_string()
                    } else {
                        format!("{sign}{}", format::percent(r.abs()))
                    }
                }
                _ => "—".to_string(),
            };
            [
                before.unwrap_or_else(|| "—".to_string()),
                after.unwrap_or_else(|| "—".to_string()),
                delta,
                pct,
                cause_words(p.cause).to_string(),
            ]
        })
        .collect();

    let heads = ["BEFORE", "NOW", "Δ", "Δ%", "WHY"];
    let mut w = [0usize; 5];
    for (i, h) in heads.iter().enumerate() {
        w[i] = h.chars().count();
        for r in &rows {
            w[i] = w[i].max(r[i].chars().count());
        }
    }
    let fixed: usize = w.iter().sum::<usize>() + 2 * 5;
    let pat_budget = width.saturating_sub(fixed).max(8);
    let pat_w = shown
        .iter()
        .map(|p| p.pattern.chars().count())
        .max()
        .unwrap_or(0)
        .max("PATTERN".len())
        .min(pat_budget);
    let line = |pat: &str, cells: [&str; 5]| -> String {
        let mut out = format!("{:<pat_w$}", truncate(pat, pat_w));
        for (i, c) in cells.iter().enumerate() {
            if i == 4 {
                out.push_str(&format!("  {c:<w$}", w = w[i]));
            } else {
                out.push_str(&format!("  {c:>w$}", w = w[i]));
            }
        }
        out
    };
    lines.push(line("PATTERN", heads));
    for (p, r) in shown.iter().zip(&rows) {
        lines.push(line(&p.pattern, [&r[0], &r[1], &r[2], &r[3], &r[4]]));
    }
    if d.patterns.len() > MAX_ROWS {
        lines.push(format!(
            "… and {} more patterns; use --json for all",
            d.patterns.len() - MAX_ROWS
        ));
    }
    lines
        .into_iter()
        .map(|l| truncate(l.trim_end(), width))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn run(config: Option<PathBuf>, args: DiffArgs) -> anyhow::Result<()> {
    let path = match config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    let dir = snapshot::default_dir()?;
    let secrets = KeyringStore;
    let result = with_progress(|tx| {
        let (path, secrets, args, dir) = (&path, &secrets, &args, &dir);
        async move { diff_profile(path, secrets, args, dir, tx.as_ref()).await }
    })
    .await;
    let (d, _) = result?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&d).context("could not serialize the diff")?
        );
    } else {
        let width = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(DEFAULT_WIDTH);
        println!("{}", render_diff(&d, width));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn pd(
        name: &str,
        kb: Option<u64>,
        ka: Option<u64>,
        bb: Option<u64>,
        ba: Option<u64>,
        cause: Cause,
    ) -> PatternDiff {
        PatternDiff {
            pattern: name.into(),
            keys_before: kb,
            keys_after: ka,
            bytes_before: bb,
            bytes_after: ba,
            delta_bytes: Some(ba.unwrap_or(0) as i64 - bb.unwrap_or(0) as i64),
            delta_keys: ka.unwrap_or(0) as i64 - kb.unwrap_or(0) as i64,
            cause,
            partial: false,
            sampled_keys_after: 0,
            pattern_sampled_keys_after: 0,
        }
    }

    fn sample() -> ReportDiff {
        ReportDiff {
            before_taken_at_unix: 1_709_210_096,
            after_taken_at_unix: 1_709_213_696,
            used_memory_before: 40 * MB,
            used_memory_after: 52 * MB,
            patterns: vec![
                pd(
                    "cart:{id}",
                    Some(1000),
                    Some(2000),
                    Some(10 * MB),
                    Some(20 * MB),
                    Cause::MoreKeys,
                ),
                pd(
                    "search:{hash}:results:with:a:very:long:name",
                    Some(500),
                    Some(500),
                    Some(8 * MB),
                    Some(10 * MB),
                    Cause::BiggerValues,
                ),
                pd("job:{uuid}", None, Some(30), None, Some(MB), Cause::New),
                pd("old:{id}", Some(10), None, Some(MB), None, Cause::Gone),
            ],
        }
    }

    #[test]
    fn layout_at_60() {
        let t = render_diff(&sample(), 60);
        println!("{t}");
        for l in t.lines() {
            assert!(l.chars().count() <= 60, "{l:?} is too wide");
        }
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(
            lines[0],
            "Since:   2024-02-29 12:34:56 UTC → 2024-02-29 13:34:56 UTC"
        );
        assert_eq!(lines[1], "Memory:  40M → 52M used (+12M)");
        assert_eq!(lines[2], "");
        let head = lines[3];
        for h in ["PATTERN", "BEFORE", "NOW", "Δ", "Δ%", "WHY"] {
            assert!(head.contains(h), "{head}");
        }
        let cart = lines[4];
        assert!(cart.starts_with("cart:{id}"), "{cart}");
        assert!(cart.contains("10M") && cart.contains("20M"), "{cart}");
        assert!(cart.contains("+10M") && cart.contains("+100%"), "{cart}");
        assert!(cart.ends_with("more keys"), "{cart}");
        assert!(t.contains('…'), "long pattern is truncated");
        let new = t.lines().find(|l| l.starts_with("job:")).unwrap();
        assert!(
            new.contains("—") && new.contains("+1.0M") && new.ends_with("new"),
            "{new}"
        );
        let gone = t.lines().find(|l| l.starts_with("old:")).unwrap();
        assert!(gone.contains("-1.0M") && gone.ends_with("gone"), "{gone}");
        assert!(t.contains("bigger values"));
        assert!(!t.contains('\x1b'));
    }

    #[test]
    fn narrower_than_minimum_is_raised() {
        for l in render_diff(&sample(), 10).lines() {
            assert!(l.chars().count() <= 60);
        }
    }

    #[test]
    fn without_sizes_shows_key_counts() {
        let mut d = sample();
        for p in &mut d.patterns {
            p.bytes_before = None;
            p.bytes_after = None;
            p.delta_bytes = None;
        }
        let t = render_diff(&d, 80);
        assert!(t.contains("sizes unavailable"));
        let cart = t.lines().find(|l| l.starts_with("cart:")).unwrap();
        assert!(
            cart.contains("1.0K") && cart.contains("2.0K") && cart.contains("+100%"),
            "{cart}"
        );
    }

    #[test]
    fn caps_rows() {
        let mut d = sample();
        d.patterns = (0..45)
            .map(|i| {
                pd(
                    &format!("p{i}"),
                    Some(1),
                    Some(1),
                    Some(MB),
                    Some(MB),
                    Cause::Unchanged,
                )
            })
            .collect();
        let t = render_diff(&d, 80);
        assert!(t.contains("… and 5 more patterns; use --json for all"));
        assert!(t.contains("unchanged"));
        assert!(t.contains("0%"));
    }

    #[test]
    fn huge_memory_does_not_overflow() {
        let mut d = sample();
        d.used_memory_before = 0;
        d.used_memory_after = u64::MAX;
        assert!(render_diff(&d, 80).contains("used (+"));
        d.used_memory_before = 1 << 63;
        d.used_memory_after = 0;
        assert!(render_diff(&d, 80).contains("used (-"));
    }

    #[test]
    fn unknown_cause_is_spelled_out() {
        let mut d = sample();
        d.patterns[2].cause = Cause::Unknown;
        let t = render_diff(&d, 100);
        let row = t.lines().find(|l| l.starts_with("job:")).unwrap();
        assert!(row.ends_with("not in the other scan"), "{row}");
    }

    #[tokio::test]
    async fn precheck_ignores_snapshots_taken_with_a_glob() {
        use crate::model::{ServerFlavor, ServerInfo, ServerMode};
        let dir = std::env::temp_dir().join(format!("redis-lens-diff-pre-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let report = ScanReport {
            profile: "p".into(),
            taken_at_unix: 100,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7".into(),
                mode: ServerMode::Standalone,
                used_memory: 1,
                maxmemory: 0,
                maxmemory_policy: "noeviction".into(),
                total_keys: 0,
            },
            sampled_keys: 0,
            patterns: vec![],
            sizes_available: true,
            hot_keys: vec![],
            complete: false,
            match_glob: Some("user:*".into()),
            server_context: None,
            key_sizes: None,
        };
        snapshot::save(&dir, &report).unwrap();
        let args = DiffArgs {
            profile: "p".into(),
            against: None,
            max_rate: None,
            json: false,
        };
        // The config doesn't exist: if the pre-check let this through, the error would
        // be about the config rather than the missing snapshot.
        let err = diff_profile(
            &dir.join("no-such-config.toml"),
            &crate::secrets::MemoryStore::default(),
            &args,
            &dir,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), no_snapshot("p").to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_max_rate_flag_reaches_the_scan_options() {
        use crate::cli::scan::{profile_in, scan_options};
        use crate::model::Env;
        use clap::Parser;
        let rate = |env, extra: &[&str]| {
            let mut argv = vec!["redis-lens", "diff", "p"];
            argv.extend_from_slice(extra);
            let args = match crate::cli::Cli::try_parse_from(argv).unwrap().command {
                Some(crate::cli::Command::Diff(a)) => a,
                other => panic!("not diff: {other:?}"),
            };
            // The same call `diff_profile` makes for its scan.
            scan_options(&profile_in(env), 100_000, None, args.max_rate).max_commands_per_sec
        };
        assert_eq!(rate(Env::Prod, &["--max-rate", "250"]), Some(250));
        assert_eq!(rate(Env::Prod, &["--max-rate", "0"]), None);
        assert_eq!(rate(Env::Prod, &[]), Some(5_000));
        assert_eq!(rate(Env::Local, &[]), None);
    }

    #[test]
    fn missing_snapshot_message() {
        assert_eq!(
            no_snapshot("prod").to_string(),
            "no snapshot for prod; run `redis-lens scan prod --save` first"
        );
    }
}
