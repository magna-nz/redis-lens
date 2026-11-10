//! Saving scan reports to disk and loading them back for the Diff view.
//!
//! Files are `<dir>/<profile>/<YYYYMMDDTHHMMSSZ>.json`, holding a serialized
//! [`ScanReport`]: patterns and example key names, never values or credentials.

use std::path::{Path, PathBuf};

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

/// Anything other than `[A-Za-z0-9._-]` becomes `_`. Dot-only names are made safe too.
fn sanitize(profile: &str) -> String {
    let s: String = profile
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        s.replace('.', "_") + "_"
    } else {
        s
    }
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
fn parse_timestamp(s: &str) -> Option<i64> {
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

/// Write `report` under `dir/<profile>/`, named by its `taken_at_unix` in UTC. Writes
/// atomically (temp file, then rename). Profile names are made filesystem-safe.
pub fn save(dir: &Path, report: &ScanReport) -> Result<PathBuf, SnapshotError> {
    let pdir = dir.join(sanitize(&report.profile));
    std::fs::create_dir_all(&pdir).map_err(io(&pdir))?;
    let name = timestamp_name(report.taken_at_unix);
    let path = pdir.join(format!("{name}.json"));
    let tmp = pdir.join(format!(".{name}.json.tmp{}", std::process::id()));
    let text = serde_json::to_string_pretty(report).map_err(|e| SnapshotError::Parse {
        path: path.clone(),
        message: e.to_string(),
    })?;
    std::fs::write(&tmp, text).map_err(io(&tmp))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(&path)(e));
    }
    Ok(path)
}

/// Snapshots for a profile, newest first. A missing directory is an empty list.
pub fn list(dir: &Path, profile: &str) -> Result<Vec<SnapshotInfo>, SnapshotError> {
    let pdir = dir.join(sanitize(profile));
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

pub fn load(path: &Path) -> Result<ScanReport, SnapshotError> {
    let text = std::fs::read_to_string(path).map_err(io(path))?;
    serde_json::from_str(&text).map_err(|e| SnapshotError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

/// The newest snapshot for `profile` taken strictly before `before_unix`, if any.
/// Used to compare a fresh scan with the previous one.
pub fn latest_before(
    dir: &Path,
    profile: &str,
    before_unix: i64,
) -> Result<Option<ScanReport>, SnapshotError> {
    match list(dir, profile)?
        .into_iter()
        .find(|s| s.taken_at_unix < before_unix)
    {
        Some(info) => load(&info.path).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ServerFlavor, ServerInfo, ServerMode};

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
        assert_eq!(sanitize("a.b-c_D9"), "a.b-c_D9");
        assert_eq!(sanitize(".."), "___");
        assert_eq!(sanitize(""), "_");
        assert_eq!(sanitize("é"), "_");
    }

    #[test]
    fn round_trip_and_atomic() {
        let dir = temp_dir("rt");
        let r = report("my/profile", 1_709_210_096);
        let path = save(&dir, &r).unwrap();
        assert_eq!(path, dir.join("my_profile").join("20240229T123456Z.json"));
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
        let pdir = dir.join("p");
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
            latest_before(&dir, "p", t)
                .unwrap()
                .map(|r| r.taken_at_unix)
        };
        assert_eq!(at(301), Some(300));
        assert_eq!(at(300), Some(200));
        assert_eq!(at(101), Some(100));
        assert_eq!(at(100), None);
        assert_eq!(latest_before(&dir, "missing", 999).unwrap(), None);
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
}
