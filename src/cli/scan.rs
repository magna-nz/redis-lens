//! `redis-lens scan <profile>`.
//!
//! The logic lives in [`report`] and [`render_table`] so tests can run it against a temp
//! config and a `MemoryStore`; [`run`] wires it to the real keychain and the terminal.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::sync::watch;

use super::{ChecksFailed, GateError, ScanArgs};
use crate::check;
use crate::config::Config;
use crate::conn::{self, ScanTarget, provider};
use crate::diff::{self, ReportDiff};
use crate::format;
use crate::model::{
    Env, PERM_FREQ, PatternStats, PermStatus, PermissionReport, Profile, ScanReport,
};
use crate::scan::context::context_summary;
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
    let (report, perms, _) = report_with_target(config_path, secrets, args, progress).await?;
    Ok((report, perms))
}

/// [`report`], also saying which server was scanned.
pub async fn report_with_target(
    config_path: &Path,
    secrets: &dyn SecretStore,
    args: &ScanArgs,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> anyhow::Result<(ScanReport, PermissionReport, ScanTarget)> {
    scan_profile_with_target(
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
    let (report, perms, _) = scan_profile_with_target(
        config_path,
        secrets,
        profile_name,
        sample,
        match_glob,
        max_rate,
        progress,
    )
    .await?;
    Ok((report, perms))
}

/// [`scan_profile`], also saying which server was scanned (see [`conn::connect_for_scan`]).
pub async fn scan_profile_with_target(
    config_path: &Path,
    secrets: &dyn SecretStore,
    profile_name: &str,
    sample: u64,
    match_glob: Option<String>,
    max_rate: Option<u32>,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> anyhow::Result<(ScanReport, PermissionReport, ScanTarget)> {
    let config = Config::load(config_path)?;
    let profile = config.get(profile_name).ok_or_else(|| {
        anyhow::anyhow!(
            "no saved connection named '{}'; see `redis-lens conn list`",
            profile_name
        )
    })?;
    let (mut connection, target) = conn::connect_for_scan(profile, secrets).await?;
    let perms = conn::check_permissions(&mut connection).await;
    let opts = scan_options(profile, sample, match_glob, max_rate);
    let report = scan::scan(&profile.name, &mut connection, &opts, &perms, progress).await?;
    Ok((report, perms, target))
}

/// The options for one scan of `profile`. `max_rate` overrides the profile's default
/// command budget; `Some(0)` means no limit.
pub(crate) fn scan_options(
    profile: &Profile,
    sample: u64,
    match_glob: Option<String>,
    max_rate: Option<u32>,
) -> ScanOptions {
    let mut opts = ScanOptions::for_profile(profile);
    opts.sample_limit = sample;
    opts.match_glob = match_glob;
    if let Some(rate) = max_rate {
        opts.max_commands_per_sec = (rate > 0).then_some(rate);
    }
    opts
}

#[cfg(test)]
pub(crate) fn profile_in(env: crate::model::Env) -> Profile {
    use crate::model::{Auth, Endpoint};
    Profile {
        name: "p".into(),
        env,
        endpoint: Endpoint {
            host: "h".into(),
            port: 1,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
    }
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
    let dir = dir()?;
    let path = snapshot::save(&dir, report)?;
    eprintln!("Saved snapshot {}", path.display());
    let retention = snapshot::Retention {
        keep: args.keep,
        keep_days: args.keep_days,
    };
    if retention != snapshot::Retention::default() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let removed = snapshot::apply_retention(&dir, &report.profile, retention, now)?;
        if !removed.is_empty() {
            eprintln!("Removed {} older snapshot(s)", removed.len());
        }
    }
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
    render_table_for(report, perms, &ScanTarget::Primary, width)
}

/// The `Target:` summary line, or `None` for a plain scan of the primary.
pub fn target_line(target: &ScanTarget) -> Option<String> {
    match target {
        ScanTarget::Primary => None,
        t => Some(format!("Target:  {t}")),
    }
}

/// [`render_table`] with a `Target:` line when the scan did not read the primary as asked.
pub fn render_table_for(
    report: &ScanReport,
    perms: &PermissionReport,
    target: &ScanTarget,
    width: usize,
) -> String {
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
    lines.extend(target_line(target));
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
        Some(p) => Ok(p),
        None => Config::default_path().map_err(anyhow::Error::from),
    };
    let path = match path {
        Ok(p) => p,
        Err(e) if is_gate(&args) => return Err(GateError(e).into()),
        Err(e) => return Err(e),
    };
    execute(
        &path,
        &KeyringStore,
        &args,
        snapshot::default_dir,
        &mut std::io::stdout(),
    )
    .await
}

/// True for `scan --check` / `--rules`: errors then exit with code 2 and rule failures with 1.
fn is_gate(args: &ScanArgs) -> bool {
    !args.checks.is_empty() || args.rules.is_some()
}

/// Everything `scan` does after parsing arguments, against a given config, secret store,
/// snapshot directory and standard output, so tests can run it end to end.
///
/// Without `--check` it behaves like a plain scan. With it, rule failures come back as
/// [`ChecksFailed`] and every other error as [`GateError`].
pub async fn execute(
    config_path: &Path,
    secrets: &dyn SecretStore,
    args: &ScanArgs,
    snapshot_dir: impl Fn() -> Result<PathBuf, snapshot::SnapshotError>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if !is_gate(args) {
        scan_and_print(config_path, secrets, args, &snapshot_dir, out, &[]).await?;
        return Ok(());
    }
    let rules = check::load_rules(&args.checks, args.rules.as_deref()).map_err(GateError)?;
    if let Some(profile) = profile_named(config_path, &args.profile)
        && profile.is_read_only()
        && profile.env == Env::Prod
    {
        eprintln!(
            "Warning: '{}' is a production profile. --check runs a real scan against it, \
             with the normal command budget.",
            profile.name
        );
    }
    let evals = scan_and_print(config_path, secrets, args, &snapshot_dir, out, &rules)
        .await
        .map_err(GateError)?;
    let failed = check::failure_count(&evals, args.strict);
    if failed > 0 {
        return Err(ChecksFailed(failed).into());
    }
    Ok(())
}

fn profile_named(config_path: &Path, name: &str) -> Option<Profile> {
    Config::load(config_path).ok()?.get(name).cloned()
}

/// Scan, print what was asked for, judge `rules` (may be empty) and return the verdicts.
async fn scan_and_print(
    config_path: &Path,
    secrets: &dyn SecretStore,
    args: &ScanArgs,
    snapshot_dir: &dyn Fn() -> Result<PathBuf, snapshot::SnapshotError>,
    out: &mut dyn Write,
    rules: &[check::Rule],
) -> anyhow::Result<Vec<check::Evaluation>> {
    if let Some(profile) = profile_named(config_path, &args.profile)
        && let Some(notice) = provider::notice(profile.provider)
    {
        eprintln!("{notice}");
    }
    let result = with_progress(|tx| async move {
        report_with_target(config_path, secrets, args, tx.as_ref()).await
    })
    .await;
    let (report, perms, target) = result?;

    // The baseline is the newest earlier snapshot with the same MATCH glob. Look it up
    // before saving, and only when something will use it.
    let wants_baseline = args.report.is_some() || rules.iter().any(check::Rule::needs_baseline);
    let baseline = if wants_baseline {
        baseline_diff(snapshot_dir, &report)
    } else {
        None
    };
    save_into(snapshot_dir, args, &report)?;

    // Rule results go to standard error when standard output carries the JSON or the report.
    let stdout_taken = args.json || (args.report.is_some() && args.output.is_none());
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&report).context("could not serialize the report")?
        )?;
    } else if let Some(format) = args.report {
        let text = crate::report::render(format, &report, baseline.as_ref(), args.examples);
        match &args.output {
            Some(path) => {
                std::fs::write(path, text)
                    .with_context(|| format!("could not write {}", path.display()))?;
                eprintln!("Wrote report {}", path.display());
            }
            None => write!(out, "{text}")?,
        }
    } else {
        let width = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(DEFAULT_WIDTH);
        writeln!(out, "{}", render_table_for(&report, &perms, &target, width))?;
        for line in context_summary(&report) {
            writeln!(out, "{line}")?;
        }
    }

    let evals = check::evaluate_all(rules, &report, baseline.as_ref());
    if !rules.is_empty() {
        let lines = check::render_lines(&evals, args.strict);
        if stdout_taken {
            for l in lines {
                eprintln!("{l}");
            }
        } else {
            writeln!(out)?;
            for l in lines {
                writeln!(out, "{l}")?;
            }
        }
    }
    if args.strict && rules.is_empty() {
        eprintln!("Note: --strict has no effect without --check or --rules.");
    }
    Ok(evals)
}

/// The diff of the newest earlier snapshot (same profile, same MATCH glob) against `report`.
/// A snapshot directory that can't be read means no baseline, with a note on stderr.
fn baseline_diff(
    snapshot_dir: &dyn Fn() -> Result<PathBuf, snapshot::SnapshotError>,
    report: &ScanReport,
) -> Option<ReportDiff> {
    let found = snapshot_dir().and_then(|dir| {
        snapshot::latest_before(
            &dir,
            &report.profile,
            report.taken_at_unix,
            report.match_glob.as_deref(),
        )
    });
    match found {
        Ok(before) => before.map(|b| diff::diff(&b, report)),
        Err(e) => {
            eprintln!("Note: no baseline snapshot: {e}");
            None
        }
    }
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
            server_context: None,
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
            keep: None,
            keep_days: None,
            checks: Vec::new(),
            rules: None,
            strict: false,
            report: None,
            output: None,
            examples: false,
        }
    }

    fn parsed_scan(extra: &[&str]) -> ScanArgs {
        use clap::Parser;
        let mut argv = vec!["redis-lens", "scan", "p"];
        argv.extend_from_slice(extra);
        match crate::cli::Cli::try_parse_from(argv).unwrap().command {
            Some(crate::cli::Command::Scan(a)) => a,
            other => panic!("not scan: {other:?}"),
        }
    }

    #[test]
    fn max_rate_flag_reaches_the_scan_options() {
        use crate::model::Env;
        let rate = |env, extra: &[&str]| {
            let a = parsed_scan(extra);
            scan_options(&profile_in(env), a.sample, a.match_glob, a.max_rate).max_commands_per_sec
        };
        assert_eq!(rate(Env::Prod, &["--max-rate", "250"]), Some(250));
        assert_eq!(rate(Env::Local, &["--max-rate", "250"]), Some(250));
        // 0 means no limit, even where the profile has a default.
        assert_eq!(rate(Env::Prod, &["--max-rate", "0"]), None);
        // Absent keeps the profile default.
        assert_eq!(rate(Env::Prod, &[]), Some(5_000));
        assert_eq!(rate(Env::Local, &[]), None);
    }

    #[test]
    fn scan_options_carry_sample_and_match() {
        let o = scan_options(
            &profile_in(crate::model::Env::Prod),
            42,
            Some("user:*".into()),
            None,
        );
        assert_eq!(o.sample_limit, 42);
        assert_eq!(o.match_glob.as_deref(), Some("user:*"));
    }

    #[test]
    fn target_line_shows_replicas_and_fallbacks_only() {
        use crate::model::Endpoint;
        assert_eq!(target_line(&ScanTarget::Primary), None);
        let ep = Endpoint {
            host: "10.0.1.21".into(),
            port: 6379,
            tls: false,
        };
        assert_eq!(
            target_line(&ScanTarget::Replica(ep)).as_deref(),
            Some("Target:  replica 10.0.1.21:6379")
        );
        let r = report(vec![], true, "noeviction");
        let t = render_table_for(
            &r,
            &perms(),
            &ScanTarget::PrimaryFallback("connection refused".into()),
            100,
        );
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with("Profile:"));
        assert_eq!(
            lines[1],
            "Target:  primary (replica unreachable: connection refused)"
        );
        assert!(!render_table(&r, &perms(), 100).contains("Target:"));
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
