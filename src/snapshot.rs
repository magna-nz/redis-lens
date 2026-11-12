//! Saving scan reports to disk, loading them back for the Diff view and `trend`, and
//! deleting old ones (retention).
//!
//! Files are `<dir>/<profile dir>/<YYYYMMDDTHHMMSSZ>.json`, holding a serialized
//! [`ScanReport`]: patterns and example key names, never values or credentials. The
//! profile directory is `<sanitized name>-<12 hex digits of FNV-1a 64 of the name>`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::ScanReport;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("could not access {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a valid snapshot: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("could not find a data directory for this user")]
    NoDataDir,
    #[error("will not delete snapshots in {path}: {reason}")]
    Unsafe { path: PathBuf, reason: &'static str },
}

/// One saved snapshot, newest first when listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub path: PathBuf,
    pub profile: String,
    pub taken_at_unix: i64,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> SnapshotError + '_ {
    move |source| SnapshotError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// `<data dir>/redis-lens/snapshots`, e.g. `~/Library/Application Support/redis-lens/snapshots`.
pub fn default_dir() -> Result<PathBuf, SnapshotError> {
    let dirs =
        directories::ProjectDirs::from("", "", "redis-lens").ok_or(SnapshotError::NoDataDir)?;
    Ok(dirs.data_dir().join("snapshots"))
}

/// FNV-1a, 64 bit. Written out here so directory names never depend on the platform,
/// the Rust version or a crate's hashing choices.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Names Windows reserves for devices, whatever the extension.
const WINDOWS_RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A readable, filesystem-safe, lower-case form of a profile name. Anything other than
/// `[a-z0-9._-]` becomes `_` (spaces too), trailing dots are dropped, and Windows device
/// names get a `_`. Different names can share a result; [`profile_dir_name`] adds a hash.
fn sanitize(profile: &str) -> String {
    let mapped: String = profile
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let mut s = mapped.trim_end_matches('.').to_string();
    if s.is_empty() {
        return "_".to_string();
    }
    let stem_len = s.find('.').unwrap_or(s.len());
    if WINDOWS_RESERVED.contains(&&s[..stem_len]) {
        s.insert(stem_len, '_');
    }
    s
}

/// `<sanitized>-<first 12 hex digits of FNV-1a 64 of the exact name>`, so profiles whose
/// sanitized names agree (case, punctuation, non-ASCII) still get their own directory.
fn profile_dir_name(profile: &str) -> String {
    let hash = format!("{:016x}", fnv1a64(profile.as_bytes()));
    format!("{}-{}", sanitize(profile), &hash[..12])
}

/// Days since 1970-01-01 to (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) to days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYYMMDDTHHMMSSZ` in UTC.
pub fn timestamp_name(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// `2024-02-29 12:34:56 UTC`, for people.
pub fn format_utc(unix: i64) -> String {
    let n = timestamp_name(unix);
    format!(
        "{}-{}-{} {}:{}:{} UTC",
        &n[0..4],
        &n[4..6],
        &n[6..8],
        &n[9..11],
        &n[11..13],
        &n[13..15]
    )
}

/// Inverse of [`timestamp_name`]. `None` when `s` is not in that format.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    let (y, m, d) = (digits(0..4)?, digits(4..6)?, digits(6..8)?);
    let (hh, mm, ss) = (digits(9..11)?, digits(11..13)?, digits(13..15)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    Some(days_from_civil(y, m as u32, d as u32) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Makes temp file names unique within this process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The directory holding `profile`'s snapshots.
fn profile_dir(dir: &Path, profile: &str) -> PathBuf {
    dir.join(profile_dir_name(profile))
}

/// Write `report` under `dir/<profile dir>/`, named by its `taken_at_unix` in UTC. Writes
/// atomically: a uniquely named temp file, flushed to disk, then renamed into place.
pub fn save(dir: &Path, report: &ScanReport) -> Result<PathBuf, SnapshotError> {
    let pdir = profile_dir(dir, &report.profile);
    std::fs::create_dir_all(&pdir).map_err(io(&pdir))?;
    let name = timestamp_name(report.taken_at_unix);
    let path = pdir.join(format!("{name}.json"));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let tmp = pdir.join(format!(
        ".{name}.json.tmp{}-{}-{nanos}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let text = serde_json::to_string_pretty(report).map_err(|e| SnapshotError::Parse {
        path: path.clone(),
        message: e.to_string(),
    })?;
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(&tmp)(e));
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(&path)(e));
    }
    Ok(path)
}

/// The fields of a snapshot needed to tell whose it is, without the rest.
#[derive(serde::Deserialize)]
struct Header {
    profile: String,
}

/// Snapshot files in `profile`'s directory with a valid timestamp name, newest first.
/// Does not open them.
fn candidates(dir: &Path, profile: &str) -> Result<Vec<SnapshotInfo>, SnapshotError> {
    let pdir = profile_dir(dir, profile);
    let entries = match std::fs::read_dir(&pdir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(&pdir)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io(&pdir))?;
        let path = entry.path();
        let Some(stem) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        let Some(taken_at_unix) = parse_timestamp(stem) else {
            continue;
        };
        out.push(SnapshotInfo {
            path,
            profile: profile.to_string(),
            taken_at_unix,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.taken_at_unix));
    Ok(out)
}

/// Snapshots for a profile, newest first. A missing directory is an empty list. Files
/// that can't be read, aren't valid snapshots, or belong to another profile (a hash
/// collision or a copied file) are left out.
pub fn list(dir: &Path, profile: &str) -> Result<Vec<SnapshotInfo>, SnapshotError> {
    Ok(candidates(dir, profile)?
        .into_iter()
        .filter(|info| {
            std::fs::read_to_string(&info.path)
                .ok()
                .and_then(|text| serde_json::from_str::<Header>(&text).ok())
                .is_some_and(|h| h.profile == profile)
        })
        .collect())
}

pub fn load(path: &Path) -> Result<ScanReport, SnapshotError> {
    let text = std::fs::read_to_string(path).map_err(io(path))?;
    serde_json::from_str(&text).map_err(|e| SnapshotError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

/// The newest snapshot for `profile` taken strictly before `before_unix` with the same
/// `SCAN MATCH` glob, if any. Used to compare a fresh scan with the previous one.
/// Corrupt files and snapshots of another profile are skipped in favour of older ones.
pub fn latest_before(
    dir: &Path,
    profile: &str,
    before_unix: i64,
    match_glob: Option<&str>,
) -> Result<Option<ScanReport>, SnapshotError> {
    Ok(candidates(dir, profile)?
        .into_iter()
        .filter(|s| s.taken_at_unix < before_unix)
        .filter_map(|s| load(&s.path).ok())
        .find(|r| r.profile == profile && r.match_glob.as_deref() == match_glob))
}

/// How many of a profile's snapshots to keep (`scan --save --keep N --keep-days D`,
/// `snapshots prune`). `None` in both means keep everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Retention {
    pub keep: Option<usize>,
    pub keep_days: Option<u64>,
}

impl Retention {
    /// Whether the snapshot at `index` in a newest-first list, taken at `taken_at_unix`,
    /// is kept at `now_unix`: it is among the newest `keep` (when set) and no older than
    /// `keep_days` days (when set). The newest (`index` 0) is always kept.
    pub fn keeps(&self, index: usize, taken_at_unix: i64, now_unix: i64) -> bool {
        if index == 0 {
            return true;
        }
        let by_count = self.keep.is_none_or(|n| index < n);
        let by_age = self.keep_days.is_none_or(|d| {
            let max_age = i64::try_from(d.saturating_mul(86_400)).unwrap_or(i64::MAX);
            now_unix.saturating_sub(taken_at_unix) <= max_age
        });
        by_count && by_age
    }
}

/// `profile`'s snapshots that `retention` would delete at `now_unix`, newest first.
///
/// Only files `save` wrote for that profile count, and only those can be returned: a
/// regular file (not a symbolic link) directly in the profile's directory, named exactly
/// `YYYYMMDDTHHMMSSZ.json` as [`timestamp_name`] writes it, whose stored `profile` is
/// `profile`. Temp files, other names, corrupt files and other profiles' snapshots are
/// neither counted nor touched. A profile directory that is a symbolic link is refused,
/// so nothing outside `dir` is ever deleted through it.
pub fn retention_plan(
    dir: &Path,
    profile: &str,
    retention: Retention,
    now_unix: i64,
) -> Result<Vec<SnapshotInfo>, SnapshotError> {
    let pdir = profile_dir(dir, profile);
    match std::fs::symlink_metadata(&pdir) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            return Err(SnapshotError::Unsafe {
                path: pdir,
                reason: "it is not a plain directory",
            });
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(&pdir)(e)),
    }
    let owned: Vec<SnapshotInfo> = list(dir, profile)?
        .into_iter()
        .filter(|info| written_by_save(&pdir, info))
        .collect();
    // Snapshots dated after `now_unix` (a clock that was ahead, or a file copied from
    // another machine) are never deleted and take no keep slot: otherwise they would
    // rank as the newest and `scan --save --keep N` would delete the snapshot it just
    // wrote. Only snapshots up to now are ranked.
    Ok(owned
        .into_iter()
        .filter(|info| info.taken_at_unix <= now_unix)
        .enumerate()
        .filter(|(i, info)| !retention.keeps(*i, info.taken_at_unix, now_unix))
        .map(|(_, info)| info)
        .collect())
}

/// `info` is a regular file directly in `pdir` whose name is exactly what [`save`] writes
/// for its time.
fn written_by_save(pdir: &Path, info: &SnapshotInfo) -> bool {
    let expected = format!("{}.json", timestamp_name(info.taken_at_unix));
    info.path.parent() == Some(pdir)
        && info.path.file_name().and_then(|n| n.to_str()) == Some(expected.as_str())
        && std::fs::symlink_metadata(&info.path).is_ok_and(|m| m.file_type().is_file())
}

/// Delete `profile`'s snapshots in `dir` that `retention` doesn't keep, judged at
/// `now_unix`, and return the deleted paths, newest first. Only files `save` wrote for
/// that profile are touched (see [`retention_plan`]); the newest is never deleted.
pub fn apply_retention(
    dir: &Path,
    profile: &str,
    retention: Retention,
    now_unix: i64,
) -> Result<Vec<PathBuf>, SnapshotError> {
    if retention == Retention::default() {
        return Ok(Vec::new());
    }
    let mut deleted = Vec::new();
    for info in retention_plan(dir, profile, retention, now_unix)? {
        std::fs::remove_file(&info.path).map_err(io(&info.path))?;
        deleted.push(info.path);
    }
    Ok(deleted)
}

/// The directory holding `profile`'s snapshots under `dir`, whether or not it exists.
pub fn profile_dir_path(dir: &Path, profile: &str) -> PathBuf {
    profile_dir(dir, profile)
}

/// Up to `n` of `profile`'s snapshots taken strictly before `before_unix` with the
/// `SCAN MATCH` glob `match_glob`, newest first. Corrupt files and other profiles'
/// snapshots are skipped.
pub fn recent_matching(
    dir: &Path,
    profile: &str,
    before_unix: i64,
    match_glob: Option<&str>,
    n: usize,
) -> Result<Vec<ScanReport>, SnapshotError> {
    Ok(candidates(dir, profile)?
        .into_iter()
        .filter(|s| s.taken_at_unix < before_unix)
        .filter_map(|s| load(&s.path).ok())
        .filter(|r| r.profile == profile && r.match_glob.as_deref() == match_glob)
        .take(n)
        .collect())
}

/// What a trend measures: estimated bytes when every snapshot in it had sizes, estimated
/// keys otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrendMetric {
    Bytes,
    Keys,
}

/// One snapshot in a trend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TrendPoint {
    pub taken_at_unix: i64,
    /// The scan saw the whole keyspace. A partial scan's figures are estimates scaled up
    /// from a sample, so its point is less certain.
    pub complete: bool,
    pub sampled_keys: u64,
}

/// A least-squares growth rate.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Growth {
    /// The slope: bytes (or keys) per day.
    pub per_day: f64,
    /// The slope as a fraction of the mean of the points it was fitted to, e.g. 0.02 for
    /// 2% a day. `None` when the mean is 0.
    pub ratio_per_day: Option<f64>,
}

/// One pattern's values over a trend's points.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PatternTrend {
    pub pattern: String,
    /// One per [`Trend::points`], oldest first. `None` where the pattern was not in that
    /// snapshot (a gap, not a zero).
    pub values: Vec<Option<u64>>,
    /// `None` with fewer than two values at different times.
    pub growth: Option<Growth>,
}

/// Each pattern's memory (or key count) over a run of snapshots of one profile taken
/// with the same `MATCH` glob.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Trend {
    pub profile: String,
    pub match_glob: Option<String>,
    pub metric: TrendMetric,
    /// Oldest first.
    pub points: Vec<TrendPoint>,
    /// Sorted by growth per day, largest first; patterns without a growth rate last; then
    /// by name.
    pub patterns: Vec<PatternTrend>,
}

impl Trend {
    pub fn pattern(&self, pattern: &str) -> Option<&PatternTrend> {
        self.patterns.iter().find(|p| p.pattern == pattern)
    }

    /// How many of the points were partial scans.
    pub fn partial_points(&self) -> usize {
        self.points.iter().filter(|p| !p.complete).count()
    }
}

/// Least-squares slope of `y` over `x`, and the mean of `y`. `None` with fewer than two
/// points or when every `x` is the same.
pub fn least_squares(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if sxx == 0.0 || !sxx.is_finite() {
        return None;
    }
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let slope = sxy / sxx;
    slope.is_finite().then_some((slope, my))
}

/// The growth of `values` taken at `times` (Unix seconds), per day. Gaps are left out of
/// the fit, not counted as zero.
pub fn growth(times: &[i64], values: &[Option<u64>]) -> Option<Growth> {
    let t0 = *times.first()?;
    let points: Vec<(f64, f64)> = times
        .iter()
        .zip(values)
        .filter_map(|(t, v)| v.map(|v| ((t - t0) as f64 / 86_400.0, v as f64)))
        .collect();
    let (slope, mean) = least_squares(&points)?;
    Some(Growth {
        per_day: slope,
        ratio_per_day: (mean > 0.0).then(|| slope / mean),
    })
}

/// The trend over `reports`, which should be one profile's snapshots with the same
/// `MATCH` glob, in any order (they are sorted oldest first).
pub fn trend(reports: &[ScanReport]) -> Trend {
    let mut reports: Vec<&ScanReport> = reports.iter().collect();
    reports.sort_by_key(|r| r.taken_at_unix);
    let metric = if !reports.is_empty() && reports.iter().all(|r| r.sizes_available) {
        TrendMetric::Bytes
    } else {
        TrendMetric::Keys
    };
    let points: Vec<TrendPoint> = reports
        .iter()
        .map(|r| TrendPoint {
            taken_at_unix: r.taken_at_unix,
            complete: r.complete,
            sampled_keys: r.sampled_keys,
        })
        .collect();
    let times: Vec<i64> = points.iter().map(|p| p.taken_at_unix).collect();
    let mut names: Vec<&str> = reports
        .iter()
        .flat_map(|r| r.patterns.iter().map(|p| p.pattern.as_str()))
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut patterns: Vec<PatternTrend> = names
        .into_iter()
        .map(|name| {
            let values: Vec<Option<u64>> = reports
                .iter()
                .map(|r| {
                    let p = r.patterns.iter().find(|p| p.pattern == name)?;
                    match metric {
                        TrendMetric::Bytes => p.est_bytes,
                        TrendMetric::Keys => Some(p.est_keys),
                    }
                })
                .collect();
            PatternTrend {
                pattern: name.to_string(),
                growth: growth(&times, &values),
                values,
            }
        })
        .collect();
    patterns.sort_by(|a, b| {
        let rate = |p: &PatternTrend| p.growth.map(|g| g.per_day);
        match (rate(a), rate(b)) {
            (Some(x), Some(y)) => y.total_cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.pattern.cmp(&b.pattern))
    });
    Trend {
        profile: reports
            .last()
            .map_or_else(String::new, |r| r.profile.clone()),
        match_glob: reports.last().and_then(|r| r.match_glob.clone()),
        metric,
        points,
        patterns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PatternStats, ServerFlavor, ServerInfo, ServerMode};

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redis-lens-snap-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn report(profile: &str, at: i64) -> ScanReport {
        ScanReport {
            profile: profile.into(),
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
            sampled_keys: 0,
            patterns: vec![],
            sizes_available: true,
            hot_keys: vec![],
            complete: true,
            match_glob: None,
            server_context: None,
            key_sizes: None,
            nodes: None,
        }
    }

    #[test]
    fn timestamp_names() {
        assert_eq!(timestamp_name(0), "19700101T000000Z");
        assert_eq!(timestamp_name(1_709_210_096), "20240229T123456Z");
        assert_eq!(timestamp_name(-1), "19691231T235959Z");
        assert_eq!(format_utc(1_709_210_096), "2024-02-29 12:34:56 UTC");
        for t in [0, 1, 951_782_400, 1_709_210_096, 4_102_444_799, -86_400] {
            assert_eq!(parse_timestamp(&timestamp_name(t)), Some(t), "{t}");
        }
        assert_eq!(parse_timestamp("nonsense"), None);
        assert_eq!(parse_timestamp("20241301T000000Z"), None);
        assert_eq!(parse_timestamp("2024010AT000000Z"), None);
    }

    #[test]
    fn sanitises_names() {
        assert_eq!(sanitize("prod/eu west:1"), "prod_eu_west_1");
        assert_eq!(sanitize("a.b-c_D9"), "a.b-c_d9");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize(""), "_");
        assert_eq!(sanitize("é"), "_");
        // Trailing dots and spaces are not allowed on Windows.
        assert_eq!(sanitize("cache."), "cache");
        assert_eq!(sanitize("cache. "), "cache._");
        assert_eq!(sanitize("a..."), "a");
        // Windows device names, with or without an extension.
        assert_eq!(sanitize("CON"), "con_");
        assert_eq!(sanitize("nul"), "nul_");
        assert_eq!(sanitize("Com1"), "com1_");
        assert_eq!(sanitize("lpt9.local"), "lpt9_.local");
        assert_eq!(sanitize("console"), "console");
        assert_eq!(sanitize("com10"), "com10");
    }

    #[test]
    fn fnv1a64_reference_values() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(
            profile_dir_name("p"),
            format!("p-{:012x}", fnv1a64(b"p") >> 16)
        );
    }

    #[test]
    fn profile_dirs_do_not_collide() {
        let dir = temp_dir("collide");
        let groups: [&[&str]; 4] = [
            &["prod eu", "prod_eu", "prod:eu"],
            &["Prod", "prod", "PROD"],
            &["é", "ü", "日"],
            &["cache", "docker:cache"],
        ];
        for group in groups {
            let names: std::collections::HashSet<_> =
                group.iter().map(|p| profile_dir_name(p)).collect();
            assert_eq!(names.len(), group.len(), "{group:?}");
            for (i, p) in group.iter().enumerate() {
                save(&dir, &report(p, 100 + i as i64)).unwrap();
            }
            for (i, p) in group.iter().enumerate() {
                let found = list(&dir, p).unwrap();
                assert_eq!(found.len(), 1, "{p}");
                assert_eq!(found[0].taken_at_unix, 100 + i as i64, "{p}");
                let r = latest_before(&dir, p, 999, None).unwrap().unwrap();
                assert_eq!(r.profile, *p);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cluster report with per-node pattern rows and a hot key tagged with its node.
    fn cluster_report_with_patterns() -> ScanReport {
        use crate::model::{KeySample, KeyType, Ttl};
        let mut r = crate::report::fixtures::cluster_report();
        r.profile = "cl".into();
        let rows = r.patterns.clone();
        for n in r.nodes.as_mut().unwrap() {
            n.patterns = rows
                .iter()
                .map(|p| PatternStats {
                    examples: Vec::new(),
                    ..p.clone()
                })
                .collect();
        }
        r.hot_keys = vec![KeySample {
            key: "session:abc".into(),
            key_type: KeyType::Hash,
            bytes: Some(10),
            ttl: Ttl::Persistent,
            lfu_freq: Some(7),
            node: Some(1),
        }];
        r
    }

    #[test]
    fn cluster_report_round_trips_through_save_and_load() {
        let dir = temp_dir("cluster-rt");
        let r = cluster_report_with_patterns();
        let path = save(&dir, &r).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.nodes.as_ref().unwrap().len(), 3);
        assert_eq!(back.hot_keys[0].node, Some(1));
        assert!(!back.nodes.as_ref().unwrap()[0].patterns.is_empty());
        // It also comes back through the lookups the diff and --check use.
        let found = latest_before(&dir, "cl", r.taken_at_unix + 1, None).unwrap();
        assert_eq!(found.unwrap(), r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshots_from_before_v03_have_no_nodes() {
        let dir = temp_dir("pre-v03");
        let r = report("old", 100);
        let mut value = serde_json::to_value(&r).unwrap();
        // The file as v0.2 wrote it: no `nodes` key at all.
        assert!(value.as_object_mut().unwrap().remove("nodes").is_none());
        let path = profile_dir(&dir, "old").join(format!("{}.json", timestamp_name(100)));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.nodes, None);
        assert_eq!(back, r);
        assert_eq!(list(&dir, "old").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn standalone_snapshot_writes_no_nodes_key() {
        let text = serde_json::to_string(&report("p", 1)).unwrap();
        assert!(!text.contains("nodes"), "{text}");
    }

    #[test]
    fn snapshot_of_another_profile_is_ignored() {
        let dir = temp_dir("foreign");
        save(&dir, &report("p", 100)).unwrap();
        // A newer file in p's directory that says it belongs to someone else, as after a
        // hash collision or a copied file.
        let foreign = serde_json::to_string(&report("q", 200)).unwrap();
        let pdir = profile_dir(&dir, "p");
        std::fs::write(pdir.join(format!("{}.json", timestamp_name(200))), foreign).unwrap();
        let times: Vec<_> = list(&dir, "p")
            .unwrap()
            .iter()
            .map(|s| s.taken_at_unix)
            .collect();
        assert_eq!(times, [100]);
        let r = latest_before(&dir, "p", 999, None).unwrap().unwrap();
        assert_eq!((r.profile.as_str(), r.taken_at_unix), ("p", 100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_before_skips_corrupt_files() {
        let dir = temp_dir("corrupt");
        save(&dir, &report("p", 100)).unwrap();
        let pdir = profile_dir(&dir, "p");
        std::fs::write(
            pdir.join(format!("{}.json", timestamp_name(200))),
            "not json",
        )
        .unwrap();
        std::fs::write(pdir.join(format!("{}.json", timestamp_name(300))), "{").unwrap();
        let r = latest_before(&dir, "p", 999, None).unwrap().unwrap();
        assert_eq!(r.taken_at_unix, 100);
        assert_eq!(list(&dir, "p").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_before_matches_the_glob() {
        let dir = temp_dir("glob");
        save(&dir, &report("p", 100)).unwrap();
        let mut partial = report("p", 200);
        partial.match_glob = Some("user:*".into());
        partial.complete = false;
        save(&dir, &partial).unwrap();
        let at = |glob| {
            latest_before(&dir, "p", 999, glob)
                .unwrap()
                .map(|r| r.taken_at_unix)
        };
        assert_eq!(at(None), Some(100));
        assert_eq!(at(Some("user:*")), Some(200));
        assert_eq!(at(Some("order:*")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_snapshots_are_incomplete() {
        let dir = temp_dir("old");
        let mut v = serde_json::to_value(report("p", 100)).unwrap();
        let obj = v.as_object_mut().unwrap();
        obj.remove("complete");
        obj.remove("match_glob");
        obj.remove("hot_keys");
        let path = dir.join("old.json");
        std::fs::write(&path, v.to_string()).unwrap();
        let r = load(&path).unwrap();
        assert!(!r.complete);
        assert_eq!(r.match_glob, None);
        assert!(r.hot_keys.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_do_not_collide() {
        let dir = temp_dir("race");
        let r = report("p", 100);
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..16).map(|_| s.spawn(|| save(&dir, &r))).collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }
        });
        let names: Vec<_> = std::fs::read_dir(profile_dir(&dir, "p"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["19700101T000140Z.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_and_atomic() {
        let dir = temp_dir("rt");
        let r = report("my/profile", 1_709_210_096);
        let path = save(&dir, &r).unwrap();
        assert_eq!(
            path,
            dir.join(format!(
                "my_profile-{}",
                &format!("{:016x}", fnv1a64(b"my/profile"))[..12]
            ))
            .join("20240229T123456Z.json")
        );
        assert_eq!(load(&path).unwrap(), r);
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["20240229T123456Z.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_order_and_ignores_junk() {
        let dir = temp_dir("list");
        assert!(list(&dir, "none").unwrap().is_empty());
        for t in [300, 100, 200] {
            save(&dir, &report("p", t)).unwrap();
        }
        save(&dir, &report("other", 999)).unwrap();
        let pdir = profile_dir(&dir, "p");
        std::fs::write(pdir.join("notes.json"), "{}").unwrap();
        std::fs::write(pdir.join("readme.txt"), "x").unwrap();
        let times: Vec<_> = list(&dir, "p")
            .unwrap()
            .iter()
            .map(|s| s.taken_at_unix)
            .collect();
        assert_eq!(times, [300, 200, 100]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_before_is_strict() {
        let dir = temp_dir("lb");
        for t in [100, 200, 300] {
            save(&dir, &report("p", t)).unwrap();
        }
        let at = |t| {
            latest_before(&dir, "p", t, None)
                .unwrap()
                .map(|r| r.taken_at_unix)
        };
        assert_eq!(at(301), Some(300));
        assert_eq!(at(300), Some(200));
        assert_eq!(at(101), Some(100));
        assert_eq!(at(100), None);
        assert_eq!(latest_before(&dir, "missing", 999, None).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_snapshot_is_a_parse_error() {
        let dir = temp_dir("bad");
        let p = dir.join("x.json");
        std::fs::write(&p, "not json").unwrap();
        assert!(matches!(load(&p), Err(SnapshotError::Parse { .. })));
        assert!(matches!(
            load(&dir.join("absent.json")),
            Err(SnapshotError::Io { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_dir_ends_in_snapshots() {
        if let Ok(d) = default_dir() {
            assert!(d.ends_with("snapshots"));
        }
    }
    const DAY: i64 = 86_400;

    fn times(paths: &[PathBuf]) -> Vec<i64> {
        paths
            .iter()
            .map(|p| {
                let stem = p.file_stem().unwrap().to_str().unwrap();
                parse_timestamp(stem).unwrap()
            })
            .collect()
    }

    fn listed(dir: &Path, profile: &str) -> Vec<i64> {
        list(dir, profile)
            .unwrap()
            .iter()
            .map(|s| s.taken_at_unix)
            .collect()
    }

    /// Five snapshots of `p`, one a day, the newest at `now`.
    fn five_days(tag: &str, now: i64) -> PathBuf {
        let dir = temp_dir(tag);
        for d in 0..5 {
            save(&dir, &report("p", now - d * DAY)).unwrap();
        }
        dir
    }

    #[test]
    fn retention_keep_only() {
        let now = 1_800_000_000;
        let dir = five_days("ret-keep", now);
        let r = Retention {
            keep: Some(2),
            keep_days: None,
        };
        let plan: Vec<i64> = retention_plan(&dir, "p", r, now)
            .unwrap()
            .iter()
            .map(|s| s.taken_at_unix)
            .collect();
        assert_eq!(plan, [now - 2 * DAY, now - 3 * DAY, now - 4 * DAY]);
        // Planning deletes nothing.
        assert_eq!(listed(&dir, "p").len(), 5);
        let deleted = apply_retention(&dir, "p", r, now).unwrap();
        assert_eq!(times(&deleted), plan);
        assert_eq!(listed(&dir, "p"), [now, now - DAY]);
        // Applying it again finds nothing more.
        assert!(apply_retention(&dir, "p", r, now).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Clock skew: snapshots dated after now must not push out the one just saved.
    #[test]
    fn future_dated_snapshots_never_cost_the_newest_real_one() {
        let now = 1_800_000_000;
        for keep in [1, 2] {
            let dir = temp_dir(&format!("ret-future-{keep}"));
            for ahead in [300 * DAY, 200 * DAY] {
                save(&dir, &report("p", now + ahead)).unwrap();
            }
            for back in [0, DAY, 2 * DAY] {
                save(&dir, &report("p", now - back)).unwrap();
            }
            let r = Retention {
                keep: Some(keep),
                keep_days: None,
            };
            let deleted = apply_retention(&dir, "p", r, now).unwrap();
            // The future files and the newest `keep` real ones stay; nothing at or
            // after `now` is deleted.
            assert!(times(&deleted).iter().all(|t| *t < now), "keep {keep}");
            let left = listed(&dir, "p");
            assert!(
                left.contains(&now),
                "keep {keep}: the snapshot just saved is gone"
            );
            assert!(left.contains(&(now + 300 * DAY)) && left.contains(&(now + 200 * DAY)));
            assert_eq!(left.len(), 2 + keep, "keep {keep}");
            // `snapshots prune` uses the same plan.
            assert!(retention_plan(&dir, "p", r, now).unwrap().is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn retention_days_only() {
        let now = 1_800_000_000;
        let dir = five_days("ret-days", now);
        let r = Retention {
            keep: None,
            keep_days: Some(2),
        };
        // Exactly two days old is still kept; older is not.
        let deleted = apply_retention(&dir, "p", r, now).unwrap();
        assert_eq!(times(&deleted), [now - 3 * DAY, now - 4 * DAY]);
        assert_eq!(listed(&dir, "p"), [now, now - DAY, now - 2 * DAY]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_both_limits() {
        let now = 1_800_000_000;
        // Count is the tighter limit.
        let dir = five_days("ret-both-a", now);
        let r = Retention {
            keep: Some(2),
            keep_days: Some(3),
        };
        apply_retention(&dir, "p", r, now).unwrap();
        assert_eq!(listed(&dir, "p"), [now, now - DAY]);
        let _ = std::fs::remove_dir_all(&dir);
        // Age is the tighter limit.
        let dir = five_days("ret-both-b", now);
        let r = Retention {
            keep: Some(4),
            keep_days: Some(1),
        };
        apply_retention(&dir, "p", r, now).unwrap();
        assert_eq!(listed(&dir, "p"), [now, now - DAY]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_never_deletes_the_newest() {
        let now = 1_800_000_000;
        let dir = five_days("ret-newest", now);
        // Every snapshot is older than the limit, and --keep 0.
        let r = Retention {
            keep: Some(0),
            keep_days: Some(1),
        };
        apply_retention(&dir, "p", r, now + 30 * DAY).unwrap();
        assert_eq!(listed(&dir, "p"), [now]);
        let _ = std::fs::remove_dir_all(&dir);
        // No limit at all deletes nothing.
        let dir = five_days("ret-none", now);
        assert!(
            apply_retention(&dir, "p", Retention::default(), now)
                .unwrap()
                .is_empty()
        );
        assert_eq!(listed(&dir, "p").len(), 5);
        // A missing directory is nothing to delete.
        assert!(
            apply_retention(
                &dir,
                "absent",
                Retention {
                    keep: Some(1),
                    keep_days: None
                },
                now
            )
            .unwrap()
            .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
        // Huge day counts don't overflow, and future-dated files are kept.
        assert!(
            Retention {
                keep: None,
                keep_days: Some(u64::MAX)
            }
            .keeps(3, i64::MIN, i64::MAX)
        );
        assert!(
            Retention {
                keep: None,
                keep_days: Some(0)
            }
            .keeps(3, now + DAY, now)
        );
    }

    #[test]
    fn retention_leaves_other_files_alone() {
        let now = 1_800_000_000;
        let dir = five_days("ret-foreign", now);
        // Other profiles' snapshots, in their own directories, are untouched.
        for d in 0..3 {
            save(&dir, &report("q", now - d * DAY)).unwrap();
            save(&dir, &report("P", now - d * DAY)).unwrap();
        }
        let pdir = profile_dir(&dir, "p");
        // Files in p's directory that save did not write for p.
        let old = timestamp_name(now - 10 * DAY);
        let foreign = pdir.join(format!("{}.json", timestamp_name(now - 9 * DAY)));
        std::fs::write(&foreign, serde_json::to_string(&report("q", 1)).unwrap()).unwrap();
        let corrupt = pdir.join(format!("{}.json", timestamp_name(now - 8 * DAY)));
        std::fs::write(&corrupt, "not json").unwrap();
        let junk = [
            pdir.join("notes.json"),
            pdir.join("readme.txt"),
            pdir.join(format!("{old}.json.bak")),
            pdir.join(format!(".{old}.json.tmp1-2-3")),
            pdir.join(format!("{}.JSON", timestamp_name(now - 12 * DAY))),
            // Parses as a time but save never writes it (31 February).
            pdir.join("20240231T000000Z.json"),
        ];
        let body = serde_json::to_string(&report("p", now - 10 * DAY)).unwrap();
        for f in &junk {
            std::fs::write(f, &body).unwrap();
        }
        // A file outside the snapshot directory, linked in under a snapshot name.
        let outside = temp_dir("ret-outside").join("keep-me.json");
        std::fs::write(&outside, &body).unwrap();
        #[cfg(unix)]
        let link = {
            let link = pdir.join(format!("{old}.json"));
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            link
        };
        // A sub-directory with a snapshot name.
        let sub = pdir.join(format!("{}.json", timestamp_name(now - 11 * DAY)));
        std::fs::create_dir(&sub).unwrap();

        let r = Retention {
            keep: Some(1),
            keep_days: None,
        };
        let deleted = apply_retention(&dir, "p", r, now).unwrap();
        assert_eq!(
            times(&deleted),
            [now - DAY, now - 2 * DAY, now - 3 * DAY, now - 4 * DAY]
        );
        for f in junk.iter().chain([&foreign, &corrupt, &outside, &sub]) {
            assert!(f.exists(), "{} was deleted", f.display());
        }
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "the link was deleted"
        );
        assert_eq!(listed(&dir, "q").len(), 3);
        assert_eq!(listed(&dir, "P").len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(outside.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn retention_refuses_a_linked_profile_directory() {
        let now = 1_800_000_000;
        let elsewhere = five_days("ret-elsewhere", now);
        let dir = temp_dir("ret-linked");
        std::os::unix::fs::symlink(profile_dir(&elsewhere, "p"), profile_dir(&dir, "p")).unwrap();
        // Reading through it is fine; deleting through it is refused.
        assert_eq!(listed(&dir, "p").len(), 5);
        let r = Retention {
            keep: Some(1),
            keep_days: None,
        };
        let err = apply_retention(&dir, "p", r, now).unwrap_err();
        assert!(matches!(err, SnapshotError::Unsafe { .. }), "{err}");
        assert!(err.to_string().starts_with("will not delete snapshots in"));
        assert_eq!(listed(&elsewhere, "p").len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn recent_matching_filters_and_limits() {
        let dir = temp_dir("recent");
        for t in [100, 200, 300, 400] {
            save(&dir, &report("p", t)).unwrap();
        }
        let mut globbed = report("p", 350);
        globbed.match_glob = Some("u*".into());
        save(&dir, &globbed).unwrap();
        let pdir = profile_dir(&dir, "p");
        std::fs::write(pdir.join(format!("{}.json", timestamp_name(250))), "{").unwrap();
        let at = |before, glob, n| -> Vec<i64> {
            recent_matching(&dir, "p", before, glob, n)
                .unwrap()
                .iter()
                .map(|r| r.taken_at_unix)
                .collect()
        };
        assert_eq!(at(i64::MAX, None, 10), [400, 300, 200, 100]);
        assert_eq!(at(400, None, 2), [300, 200]);
        assert_eq!(at(i64::MAX, Some("u*"), 10), [350]);
        assert!(at(i64::MAX, Some("x*"), 10).is_empty());
        assert!(at(i64::MAX, None, 0).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn with_patterns(at: i64, sizes: bool, pats: &[(&str, u64)]) -> ScanReport {
        let mut r = report("p", at);
        r.sizes_available = sizes;
        r.patterns = pats
            .iter()
            .map(|(name, v)| crate::model::PatternStats {
                pattern: (*name).into(),
                key_type: None,
                sampled_keys: 1,
                est_keys: *v / 10,
                sampled_bytes: sizes.then_some(*v),
                est_bytes: sizes.then_some(*v),
                no_ttl: 0,
                ttl_hist: [0; 7],
                size_p50: None,
                size_p99: None,
                size_max: None,
                examples: vec![],
            })
            .collect();
        r
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9 * b.abs().max(1.0)
    }

    #[test]
    fn least_squares_cases() {
        assert_eq!(least_squares(&[]), None);
        assert_eq!(least_squares(&[(1.0, 5.0)]), None);
        // The same time twice has no slope.
        assert_eq!(least_squares(&[(1.0, 5.0), (1.0, 9.0)]), None);
        assert_eq!(
            least_squares(&[(0.0, 3.0), (1.0, 3.0), (2.0, 3.0)]),
            Some((0.0, 3.0))
        );
        let (slope, mean) = least_squares(&[(0.0, 1.0), (1.0, 3.0), (2.0, 5.0)]).unwrap();
        assert!(close(slope, 2.0) && close(mean, 3.0), "{slope} {mean}");
        // Noisy: y = 1, 2, 2, 4 at x = 0..4 fits 0.9 a step.
        let (slope, _) = least_squares(&[(0.0, 1.0), (1.0, 2.0), (2.0, 2.0), (3.0, 4.0)]).unwrap();
        assert!(close(slope, 0.9), "{slope}");
    }

    #[test]
    fn growth_cases() {
        let days = |n: i64| (0..n).map(|d| 1_000 + d * DAY).collect::<Vec<_>>();
        // Empty, one point, all gaps.
        assert_eq!(growth(&[], &[]), None);
        assert_eq!(growth(&days(1), &[Some(10)]), None);
        assert_eq!(growth(&days(3), &[None, Some(5), None]), None);
        // Flat.
        let g = growth(&days(3), &[Some(7), Some(7), Some(7)]).unwrap();
        assert_eq!((g.per_day, g.ratio_per_day), (0.0, Some(0.0)));
        // Rising 2 a day around a mean of 100.
        let g = growth(&days(3), &[Some(98), Some(100), Some(102)]).unwrap();
        assert!(
            close(g.per_day, 2.0) && close(g.ratio_per_day.unwrap(), 0.02),
            "{g:?}"
        );
        // Gaps are left out, not counted as 0: 10 → (gap) → 30 is 10 a day.
        let g = growth(&days(3), &[Some(10), None, Some(30)]).unwrap();
        assert!(close(g.per_day, 10.0), "{g:?}");
        // Half-day spacing is measured in days.
        let g = growth(&[0, DAY / 2], &[Some(0), Some(50)]).unwrap();
        assert!(close(g.per_day, 100.0), "{g:?}");
        // All zero: a slope but no relative rate.
        let g = growth(&days(2), &[Some(0), Some(0)]).unwrap();
        assert_eq!(g.ratio_per_day, None);
    }

    #[test]
    fn trend_over_snapshots() {
        let reports = [
            // Out of order on purpose.
            with_patterns(2 * DAY, true, &[("a", 120), ("b", 50)]),
            with_patterns(0, true, &[("a", 100), ("b", 50), ("c", 9)]),
            with_patterns(DAY, true, &[("a", 110), ("b", 50)]),
        ];
        let t = trend(&reports);
        assert_eq!(t.metric, TrendMetric::Bytes);
        assert_eq!(t.profile, "p");
        let at: Vec<i64> = t.points.iter().map(|p| p.taken_at_unix).collect();
        assert_eq!(at, [0, DAY, 2 * DAY]);
        let names: Vec<&str> = t.patterns.iter().map(|p| p.pattern.as_str()).collect();
        // a grows, b is flat, c has one point and so no rate.
        assert_eq!(names, ["a", "b", "c"]);
        let a = t.pattern("a").unwrap();
        assert_eq!(a.values, [Some(100), Some(110), Some(120)]);
        assert!(close(a.growth.unwrap().per_day, 10.0));
        assert!(close(
            a.growth.unwrap().ratio_per_day.unwrap(),
            10.0 / 110.0
        ));
        assert_eq!(t.pattern("c").unwrap().values, [Some(9), None, None]);
        assert_eq!(t.pattern("c").unwrap().growth, None);
        assert_eq!(t.partial_points(), 0);
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["metric"], "bytes");
        assert_eq!(json["patterns"][2]["values"][1], serde_json::Value::Null);
    }

    #[test]
    fn trend_uses_keys_without_sizes_and_marks_partial_scans() {
        let mut partial = with_patterns(DAY, false, &[("a", 200)]);
        partial.complete = false;
        let t = trend(&[with_patterns(0, true, &[("a", 100)]), partial]);
        assert_eq!(t.metric, TrendMetric::Keys);
        assert_eq!(t.pattern("a").unwrap().values, [Some(10), Some(20)]);
        assert_eq!(t.partial_points(), 1);
        assert!(!t.points[1].complete);
        // No reports at all.
        let t = trend(&[]);
        assert!(t.points.is_empty() && t.patterns.is_empty());
        assert_eq!(t.metric, TrendMetric::Keys);
    }
}
