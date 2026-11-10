//! `redis-lens scan <profile>`.
//!
//! The logic lives in [`report`] and [`render_table`] so tests can run it against a temp
//! config and a `MemoryStore`; [`run`] wires it to the real keychain and the terminal.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::sync::watch;

use super::ScanArgs;
use crate::config::Config;
use crate::conn;
use crate::format;
use crate::model::{PERM_FREQ, PatternStats, PermStatus, PermissionReport, ScanReport};
use crate::scan::{self, ScanOptions, ScanProgress};
use crate::secrets::{KeyringStore, SecretStore};
use crate::snapshot;

const MIN_WIDTH: usize = 60;
const MAX_ROWS: usize = 40;
const DEFAULT_WIDTH: usize = 100;

/// Connect to the profile, check permissions and scan.
pub async fn report(
    config_path: &Path,
    secrets: &dyn SecretStore,
    args: &ScanArgs,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> anyhow::Result<(ScanReport, PermissionReport)> {
    scan_profile(
        config_path,
        secrets,
        &args.profile,
        args.sample,
        args.match_glob.clone(),
        args.max_rate,
        progress,
    )
    .await
}

/// The scan behind both `scan` and `diff`. `max_rate` overrides the profile's default
/// command budget; `Some(0)` means no limit.
pub async fn scan_profile(
    config_path: &Path,
    secrets: &dyn SecretStore,
    profile_name: &str,
    sample: u64,
    match_glob: Option<String>,
    max_rate: Option<u32>,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> anyhow::Result<(ScanReport, PermissionReport)> {
    let config = Config::load(config_path)?;
    let profile = config.get(profile_name).ok_or_else(|| {
        anyhow::anyhow!(
            "no saved connection named '{}'; see `redis-lens conn list`",
            profile_name
        )
    })?;
    let mut connection = conn::connect(profile, secrets).await?;
    let perms = conn::check_permissions(&mut connection).await;
    let mut opts = ScanOptions::for_profile(profile);
    opts.sample_limit = sample;
    opts.match_glob = match_glob;
    if let Some(rate) = max_rate {
        opts.max_commands_per_sec = (rate > 0).then_some(rate);
    }
    let report = scan::scan(&profile.name, &mut connection, &opts, &perms, progress).await?;
    Ok((report, perms))
}

/// With `--save`, write the report into `dir` and tell the user where on stderr.
pub fn save_if_requested(
    dir: &Path,
    args: &ScanArgs,
    report: &ScanReport,
) -> anyhow::Result<Option<PathBuf>> {
    save_into(|| Ok(dir.to_path_buf()), args, report)
}

/// [`save_if_requested`], finding the directory only when `--save` was given, so a
/// plain scan never fails for want of a data directory.
fn save_into(
    dir: impl FnOnce() -> Result<PathBuf, snapshot::SnapshotError>,
    args: &ScanArgs,
    report: &ScanReport,
) -> anyhow::Result<Option<PathBuf>> {
    if !args.save {
        return Ok(None);
    }
    let path = snapshot::save(&dir()?, report)?;
    eprintln!("Saved snapshot {}", path.display());
    Ok(Some(path))
}

/// Run `f` while printing "Sampled n / m" on stderr when it is a terminal.
pub(super) async fn with_progress<T, F, Fut>(f: F) -> T
where
    F: FnOnce(Option<watch::Sender<ScanProgress>>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let show_progress = std::io::stderr().is_terminal();
    let (tx, mut rx) = watch::channel(ScanProgress::default());
    let printer = show_progress.then(|| {
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let p = *rx.borrow_and_update();
                eprint!(
                    "\rSampled {} / {}",
                    format::grouped(p.sampled),
                    format::grouped(p.target)
                );
                let _ = std::io::stderr().flush();
            }
        })
    });
    let result = f(Some(tx)).await;
    if let Some(h) = printer {
        let _ = h.await;
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }
    result
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    if max > 0 {
        out.push('…');
    }
    out
}

fn mode_str(r: &ScanReport) -> String {
    format!("{:?}", r.server.mode).to_lowercase()
}

fn flavor_str(r: &ScanReport) -> String {
    format!("{:?}", r.server.flavor)
}

/// The report as plain text that fits `width` columns (at least 60).
pub fn render_table(report: &ScanReport, perms: &PermissionReport, width: usize) -> String {
    let width = width.max(MIN_WIDTH);
    let s = &report.server;
    let mut lines: Vec<String> = Vec::new();

    lines.push(format!(
        "Profile: {}   {} {}   {}",
        report.profile,
        flavor_str(report),
        s.version,
        mode_str(report)
    ));
    let limit = if s.maxmemory == 0 {
        "no limit".to_string()
    } else {
        format::bytes(s.maxmemory)
    };
    lines.push(format!(
        "Memory:  {} used of {}   eviction: {}",
        format::bytes(s.used_memory),
        limit,
        s.maxmemory_policy
    ));
    let share = if s.total_keys == 0 {
        0.0
    } else {
        report.sampled_keys as f64 / s.total_keys as f64
    };
    lines.push(format!(
        "Keys:    {} total, {} sampled ({})",
        format::grouped(s.total_keys),
        format::grouped(report.sampled_keys),
        format::percent(share.min(1.0))
    ));

    let mut degraded = Vec::new();
    if !report.sizes_available {
        degraded.push("sizes unavailable: MEMORY USAGE denied".to_string());
    }
    if !s.lfu_enabled() {
        degraded.push(format!(
            "hot keys unavailable: eviction policy is {}",
            s.maxmemory_policy
        ));
    } else if perms
        .checks
        .iter()
        .any(|c| c.label == PERM_FREQ && matches!(c.status, PermStatus::Denied(_)))
    {
        degraded.push("hot keys unavailable: OBJECT FREQ denied".to_string());
    }
    if !report.hot_keys.is_empty() {
        let top: Vec<String> = report
            .hot_keys
            .iter()
            .take(3)
            .map(|k| {
                format!(
                    "{} (freq {})",
                    truncate(&k.key, 40),
                    k.lfu_freq.unwrap_or(0)
                )
            })
            .collect();
        lines.push(format!("Hot:     {}", top.join(", ")));
    }
    for d in &degraded {
        lines.push(format!("Note:    {d}"));
    }
    lines.push(String::new());

    let sizes = report.sizes_available;
    let total_bytes: u64 = report.patterns.iter().filter_map(|p| p.est_bytes).sum();
    let total_keys: u64 = report.patterns.iter().map(|p| p.est_keys).sum();
    let shown: &[PatternStats] = &report.patterns[..report.patterns.len().min(MAX_ROWS)];

    let rows: Vec<[String; 6]> = shown
        .iter()
        .map(|p| {
            let ratio = if sizes {
                match (p.est_bytes, total_bytes) {
                    (Some(b), t) if t > 0 => b as f64 / t as f64,
                    _ => 0.0,
                }
            } else if total_keys > 0 {
                p.est_keys as f64 / total_keys as f64
            } else {
                0.0
            };
            let avg = match p.est_bytes {
                Some(b) if p.est_keys > 0 => format::bytes(b / p.est_keys),
                _ => "—".to_string(),
            };
            [
                p.key_type.map_or("mixed", |t| t.as_str()).to_string(),
                format::count(p.est_keys),
                p.est_bytes.map_or("—".to_string(), format::bytes),
                format::percent(ratio),
                avg,
                format::percent(p.no_ttl_ratio()),
            ]
        })
        .collect();

    let heads = ["TYPE", "KEYS", "MEMORY", "SHARE", "AVG", "NO TTL"];
    let mut w = [0usize; 6];
    for (i, h) in heads.iter().enumerate() {
        w[i] = h.chars().count();
        for r in &rows {
            w[i] = w[i].max(r[i].chars().count());
        }
    }
    let fixed: usize = w.iter().sum::<usize>() + 2 * 6;
    let pat_budget = width.saturating_sub(fixed).max(8);
    let pat_w = shown
        .iter()
        .map(|p| p.pattern.chars().count())
        .max()
        .unwrap_or(0)
        .max("PATTERN".len())
        .min(pat_budget);

    let line = |pat: &str, cells: [&str; 6]| -> String {
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
        lines.push(line(&p.pattern, [&r[0], &r[1], &r[2], &r[3], &r[4], &r[5]]));
    }
    if report.patterns.len() > MAX_ROWS {
        lines.push(format!(
            "… and {} more patterns; use --json for all",
            report.patterns.len() - MAX_ROWS
        ));
    }

    lines
        .into_iter()
        .map(|l| truncate(l.trim_end(), width))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn run(config: Option<PathBuf>, args: ScanArgs) -> anyhow::Result<()> {
    let path = match config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    let secrets = KeyringStore;
    let result = with_progress(|tx| {
        let (path, secrets, args) = (&path, &secrets, &args);
        async move { report(path, secrets, args, tx.as_ref()).await }
    })
    .await;
    let (report, perms) = result?;
    save_into(snapshot::default_dir, &args, &report)?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("could not serialize the report")?
        );
    } else {
        let width = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(DEFAULT_WIDTH);
        println!("{}", render_table(&report, &perms, width));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{KeyType, PERM_LABELS, PermCheck, ServerFlavor, ServerInfo, ServerMode};

    fn pattern(name: &str, keys: u64, bytes: Option<u64>) -> PatternStats {
        PatternStats {
            pattern: name.to_string(),
            key_type: Some(KeyType::Hash),
            sampled_keys: keys,
            est_keys: keys,
            sampled_bytes: bytes,
            est_bytes: bytes,
            no_ttl: keys / 2,
            ttl_hist: [0; 7],
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: vec![],
        }
    }

    fn report(patterns: Vec<PatternStats>, sizes: bool, policy: &str) -> ScanReport {
        ScanReport {
            profile: "local".into(),
            taken_at_unix: 0,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7.4.2".into(),
                mode: ServerMode::Standalone,
                used_memory: 5_000_000,
                maxmemory: 0,
                maxmemory_policy: policy.into(),
                total_keys: 85_964,
            },
            sampled_keys: 12_400,
            patterns,
            sizes_available: sizes,
            hot_keys: Vec::new(),
            complete: false,
            match_glob: None,
        }
    }

    fn perms() -> PermissionReport {
        PermissionReport {
            checks: PERM_LABELS
                .iter()
                .map(|l| PermCheck {
                    label: l,
                    status: PermStatus::Allowed,
                })
                .collect(),
        }
    }

    fn args(save: bool) -> ScanArgs {
        ScanArgs {
            profile: "local".into(),
            sample: 10,
            match_glob: None,
            json: false,
            max_rate: None,
            save,
        }
    }

    #[test]
    fn data_dir_is_only_needed_with_save() {
        let r = report(vec![], true, "noeviction");
        let none = save_into(
            || panic!("looked up the data directory without --save"),
            &args(false),
            &r,
        );
        assert!(none.unwrap().is_none());
        let err = save_into(|| Err(snapshot::SnapshotError::NoDataDir), &args(true), &r);
        assert!(err.is_err());
    }

    #[test]
    fn table_with_sizes() {
        let r = report(
            vec![
                pattern("user:{id}:cart", 3000, Some(3_000_000)),
                pattern("session:{id}", 1000, Some(1_000_000)),
            ],
            true,
            "allkeys-lfu",
        );
        let t = render_table(&r, &perms(), 100);
        assert!(t.contains("local"));
        assert!(t.contains("Redis 7.4.2"));
        assert!(t.contains("no limit"));
        assert!(t.contains("85,964"));
        assert!(t.contains("12,400"));
        assert!(t.contains("(14%)"));
        assert!(t.contains("PATTERN"));
        assert!(t.contains("user:{id}:cart"));
        assert!(t.contains("75%"));
        assert!(!t.contains("unavailable"));
        assert!(!t.contains('\x1b'));
    }

    #[test]
    fn table_without_sizes() {
        let r = report(vec![pattern("a:{id}", 10, None)], false, "noeviction");
        let t = render_table(&r, &perms(), 100);
        assert!(t.contains("sizes unavailable: MEMORY USAGE denied"));
        assert!(t.contains("hot keys unavailable: eviction policy is noeviction"));
        let row = t.lines().find(|l| l.starts_with("a:{id}")).unwrap();
        assert!(row.contains('—'));
        assert!(row.contains("100%"));
    }

    #[test]
    fn hot_line_lists_top_three() {
        let mut r = report(vec![pattern("a", 1, Some(1))], true, "allkeys-lfu");
        assert!(!render_table(&r, &perms(), 100).contains("Hot:"));
        r.hot_keys = (0..5)
            .map(|i| crate::model::KeySample {
                key: format!("k{i}"),
                key_type: KeyType::String,
                bytes: None,
                ttl: crate::model::Ttl::Persistent,
                lfu_freq: Some(200 - i),
            })
            .collect();
        let t = render_table(&r, &perms(), 100);
        let line = t.lines().find(|l| l.starts_with("Hot:")).unwrap();
        assert_eq!(line, "Hot:     k0 (freq 200), k1 (freq 199), k2 (freq 198)");
    }

    #[test]
    fn freq_denied_line() {
        let mut p = perms();
        for c in &mut p.checks {
            if c.label == PERM_FREQ {
                c.status = PermStatus::Denied("NOPERM".into());
            }
        }
        let r = report(vec![pattern("a", 1, Some(1))], true, "allkeys-lfu");
        let t = render_table(&r, &p, 100);
        assert!(t.contains("hot keys unavailable: OBJECT FREQ denied"));
    }

    #[test]
    fn more_than_forty_patterns() {
        let pats = (0..45)
            .map(|i| pattern(&format!("p{i}:{{id}}"), 10, Some(1000 - i)))
            .collect();
        let r = report(pats, true, "noeviction");
        let t = render_table(&r, &perms(), 100);
        assert!(t.contains("… and 5 more patterns; use --json for all"));
        assert!(t.contains("p39:{id}"));
        assert!(!t.contains("p40:{id}"));
    }

    #[test]
    fn narrow_width_fits() {
        let r = report(
            vec![pattern(
                "very:long:{id}:pattern:with:{uuid}:many:segments:that:go:on:forever",
                5000,
                Some(123_456_789),
            )],
            true,
            "noeviction",
        );
        let t = render_table(&r, &perms(), 60);
        for l in t.lines() {
            assert!(l.chars().count() <= 60, "{l:?} is too wide");
        }
        assert!(t.contains('…'));
        // Widths below the minimum are raised to it.
        for l in render_table(&r, &perms(), 10).lines() {
            assert!(l.chars().count() <= 60);
        }
    }
}
