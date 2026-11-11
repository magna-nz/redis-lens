//! Saving scan reports to disk and loading them back for the Diff view.
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
/// How many of a profile's snapshots to keep (`scan --save --keep N --keep-days D`,
/// `snapshots prune`). `None` in both means keep everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Retention {
    pub keep: Option<usize>,
    pub keep_days: Option<u64>,
}

/// Delete `profile`'s snapshots in `dir` that `retention` doesn't keep, judged at
/// `now_unix`, and return the deleted paths. Only files `save` wrote for that profile
/// are touched. Contract stub for the quick-wins wave: deletes nothing yet.
pub fn apply_retention(
    _dir: &Path,
    _profile: &str,
    _retention: Retention,
    _now_unix: i64,
) -> Result<Vec<PathBuf>, SnapshotError> {
    Ok(Vec::new())
}

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
            complete: true,
            match_glob: None,
            server_context: None,
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
}
