//! Comparing two scan reports of the same server.

use crate::model::{PatternStats, ScanReport};

/// Why a pattern's memory changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// Not in the older report.
    New,
    /// Not in the newer report.
    Gone,
    /// The key count changed more than the average size did, compared as log ratios.
    /// The direction follows `delta_bytes` (or `delta_keys` without sizes).
    MoreKeys,
    FewerKeys,
    /// The average size changed more than the key count did, compared as log ratios.
    BiggerValues,
    SmallerValues,
    /// Changed by less than 1% or less than 64 KiB (or 1% of keys when sizes are unavailable).
    Unchanged,
    /// In only one report, but the two reports didn't look at the same keys (a different
    /// `MATCH` glob), so it can't be called new or gone.
    Unknown,
}

impl Cause {
    /// Lower-case words for tables, e.g. `more keys`.
    pub fn words(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Gone => "gone",
            Self::MoreKeys => "more keys",
            Self::FewerKeys => "fewer keys",
            Self::BiggerValues => "bigger values",
            Self::SmallerValues => "smaller values",
            Self::Unchanged => "unchanged",
            Self::Unknown => "not in the other scan",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PatternDiff {
    pub pattern: String,
    pub keys_before: Option<u64>,
    pub keys_after: Option<u64>,
    /// `None` when that report had no sizes.
    pub bytes_before: Option<u64>,
    pub bytes_after: Option<u64>,
    /// `bytes_after - bytes_before`, treating a pattern missing from one report as 0.
    /// `None` when either report lacks sizes, or when the pattern is in both reports but
    /// one of them has no size for it. Saturates at the `i64` range.
    pub delta_bytes: Option<i64>,
    pub delta_keys: i64,
    pub cause: Cause,
    /// At least one of the two scans did not see the whole keyspace, so a `New` pattern
    /// may only be one the older scan missed.
    pub partial: bool,
    /// `sampled_keys` of the newer report: the scale for how many keys a partial `New`
    /// pattern without sizes needs before it counts as growing.
    #[serde(skip)]
    pub sampled_keys_after: u64,
    /// This pattern's `sampled_keys` in the newer report (0 when it is not there), in
    /// the same units as [`Self::sampled_keys_after`].
    #[serde(skip)]
    pub pattern_sampled_keys_after: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReportDiff {
    pub before_taken_at_unix: i64,
    pub after_taken_at_unix: i64,
    pub used_memory_before: u64,
    pub used_memory_after: u64,
    /// Sorted by |delta_bytes| (or |delta_keys| without sizes), largest first, then pattern.
    pub patterns: Vec<PatternDiff>,
}

impl ReportDiff {
    pub fn pattern(&self, pattern: &str) -> Option<&PatternDiff> {
        self.patterns.iter().find(|p| p.pattern == pattern)
    }
}

/// `a - b` without overflow, clamped to the `i64` range.
pub fn sub(a: u64, b: u64) -> i64 {
    let d = i128::from(a) - i128::from(b);
    d.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// Compare an older report with a newer one, matching patterns by name.
///
/// A pattern in only one report is `New` or `Gone` when both reports saw the whole
/// keyspace or used the same `MATCH` glob; otherwise it is `Unknown`.
pub fn diff(before: &ScanReport, after: &ScanReport) -> ReportDiff {
    let sizes = before.sizes_available && after.sizes_available;
    let partial = !(before.complete && after.complete);
    let comparable = (before.complete && after.complete) || before.match_glob == after.match_glob;
    let mut names: Vec<&str> = before
        .patterns
        .iter()
        .chain(&after.patterns)
        .map(|p| p.pattern.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut patterns: Vec<PatternDiff> = names
        .into_iter()
        .map(|name| {
            let b = before.patterns.iter().find(|p| p.pattern == name);
            let a = after.patterns.iter().find(|p| p.pattern == name);
            pattern_diff(name, b, a, sizes, comparable, partial, after.sampled_keys)
        })
        .collect();
    let magnitude = |p: &PatternDiff| match p.delta_bytes {
        Some(d) if sizes => d.unsigned_abs(),
        _ => p.delta_keys.unsigned_abs(),
    };
    patterns.sort_by(|x, y| {
        magnitude(y)
            .cmp(&magnitude(x))
            .then_with(|| x.pattern.cmp(&y.pattern))
    });
    ReportDiff {
        before_taken_at_unix: before.taken_at_unix,
        after_taken_at_unix: after.taken_at_unix,
        used_memory_before: before.server.used_memory,
        used_memory_after: after.server.used_memory,
        patterns,
    }
}

fn pattern_diff(
    name: &str,
    b: Option<&PatternStats>,
    a: Option<&PatternStats>,
    sizes: bool,
    comparable: bool,
    partial: bool,
    sampled_keys_after: u64,
) -> PatternDiff {
    let keys_before = b.map(|p| p.est_keys);
    let keys_after = a.map(|p| p.est_keys);
    let bytes_before = if sizes {
        b.and_then(|p| p.est_bytes)
    } else {
        None
    };
    let bytes_after = if sizes {
        a.and_then(|p| p.est_bytes)
    } else {
        None
    };
    // A side is known when the pattern is absent (0) or present with a size.
    let known = |p: Option<&PatternStats>, bytes: Option<u64>| p.is_none() || bytes.is_some();
    let delta_bytes = (sizes && known(b, bytes_before) && known(a, bytes_after))
        .then(|| sub(bytes_after.unwrap_or(0), bytes_before.unwrap_or(0)));
    let delta_keys = sub(keys_after.unwrap_or(0), keys_before.unwrap_or(0));
    let cause = match (b, a) {
        (None, _) | (_, None) if !comparable => Cause::Unknown,
        (None, _) => Cause::New,
        (_, None) => Cause::Gone,
        (Some(_), Some(_)) => classify(
            keys_before.unwrap_or(0),
            keys_after.unwrap_or(0),
            bytes_before.zip(bytes_after),
        ),
    };
    PatternDiff {
        pattern: name.to_string(),
        keys_before,
        keys_after,
        bytes_before,
        bytes_after,
        delta_bytes,
        delta_keys,
        cause,
        partial,
        sampled_keys_after,
        pattern_sampled_keys_after: a.map_or(0, |p| p.sampled_keys),
    }
}

/// The cause for a pattern present in both reports.
///
/// Memory is keys × average size, so `ln(bytes ratio) = ln(keys ratio) + ln(avg ratio)`.
/// Whichever term is larger in magnitude explains the change, and the direction always
/// follows the sign of the byte change.
fn classify(kb: u64, ka: u64, bytes: Option<(u64, u64)>) -> Cause {
    const FLOOR_BYTES: f64 = 64.0 * 1024.0;
    let dk = sub(ka, kb);
    let Some((bb, ba)) = bytes else {
        if (dk.unsigned_abs() as f64) < (kb as f64 * 0.01).max(1.0) {
            return Cause::Unchanged;
        }
        return if dk > 0 {
            Cause::MoreKeys
        } else {
            Cause::FewerKeys
        };
    };
    let db = sub(ba, bb);
    if (db.unsigned_abs() as f64) < (bb as f64 * 0.01).max(FLOOR_BYTES) {
        return Cause::Unchanged;
    }
    let grew = db > 0;
    let keys_cause = if grew {
        Cause::MoreKeys
    } else {
        Cause::FewerKeys
    };
    let size_cause = if grew {
        Cause::BiggerValues
    } else {
        Cause::SmallerValues
    };
    if dk == 0 {
        return size_cause;
    }
    if kb == 0 || ka == 0 {
        // No average size on one side: the key count is what changed.
        return keys_cause;
    }
    let ln_keys = (ka as f64 / kb as f64).ln();
    let ln_bytes = (ba as f64 / bb as f64).ln();
    let ln_avg = ln_bytes - ln_keys;
    // A side with 0 bytes makes `ln_avg` infinite: then the size is what changed.
    if ln_keys.abs() >= ln_avg.abs() {
        keys_cause
    } else {
        size_cause
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ServerFlavor, ServerInfo, ServerMode};

    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;

    fn stats(name: &str, keys: u64, bytes: Option<u64>) -> PatternStats {
        PatternStats {
            pattern: name.into(),
            key_type: None,
            sampled_keys: keys,
            est_keys: keys,
            sampled_bytes: bytes,
            est_bytes: bytes,
            no_ttl: 0,
            ttl_hist: [0; 7],
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: vec![],
        }
    }

    fn report(patterns: Vec<PatternStats>, sizes: bool) -> ScanReport {
        ScanReport {
            profile: "p".into(),
            taken_at_unix: 0,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7".into(),
                mode: ServerMode::Standalone,
                used_memory: 10,
                maxmemory: 0,
                maxmemory_policy: "noeviction".into(),
                total_keys: 0,
            },
            sampled_keys: 0,
            patterns,
            sizes_available: sizes,
            hot_keys: vec![],
            complete: false,
            match_glob: None,
        }
    }

    fn one(before: PatternStats, after: PatternStats, sizes: bool) -> PatternDiff {
        let d = diff(&report(vec![before], sizes), &report(vec![after], sizes));
        d.patterns[0].clone()
    }

    #[test]
    fn carries_the_newer_reports_sample_size() {
        let mut before = report(vec![stats("a", 1, None)], false);
        before.sampled_keys = 10;
        let mut after = report(vec![stats("a", 1, None), stats("b", 60, None)], false);
        after.sampled_keys = 4000;
        let d = diff(&before, &after);
        assert!(d.patterns.iter().all(|p| p.sampled_keys_after == 4000));
        assert_eq!(d.pattern("b").unwrap().pattern_sampled_keys_after, 60);
        // Scaled: 10 sampled keys standing for 1,000.
        let mut scaled = stats("c", 1000, None);
        scaled.sampled_keys = 10;
        after.patterns.push(scaled);
        let c = diff(&before, &after).pattern("c").unwrap().clone();
        assert_eq!((c.delta_keys, c.pattern_sampled_keys_after), (1000, 10));
        // Not part of the JSON output.
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("sampled_keys_after"), "{json}");
    }

    #[test]
    fn partial_flag_follows_completeness() {
        let mk = |complete: bool| {
            let mut r = report(vec![stats("a", 1, Some(1))], true);
            r.complete = complete;
            r
        };
        assert!(diff(&mk(true), &mk(false)).patterns[0].partial);
        assert!(diff(&mk(false), &mk(true)).patterns[0].partial);
        assert!(!diff(&mk(true), &mk(true)).patterns[0].partial);
    }

    #[test]
    fn new_and_gone() {
        let d = diff(
            &report(vec![stats("old", 10, Some(MB))], true),
            &report(vec![stats("fresh", 5, Some(2 * MB))], true),
        );
        let new = d.pattern("fresh").unwrap();
        assert_eq!(new.cause, Cause::New);
        assert_eq!(new.keys_before, None);
        assert_eq!(new.delta_bytes, Some(2 * MB as i64));
        assert_eq!(new.delta_keys, 5);
        let gone = d.pattern("old").unwrap();
        assert_eq!(gone.cause, Cause::Gone);
        assert_eq!(gone.keys_after, None);
        assert_eq!(gone.delta_bytes, Some(-(MB as i64)));
        assert_eq!(gone.delta_keys, -10);
        // Sorted by |delta|, largest first.
        assert_eq!(d.patterns[0].pattern, "fresh");
    }

    #[test]
    fn unchanged_thresholds() {
        // Below the 64 KiB floor even though it is more than 1%.
        let d = one(
            stats("a", 100, Some(100 * KB)),
            stats("a", 100, Some(150 * KB)),
            true,
        );
        assert_eq!(d.cause, Cause::Unchanged);
        // Below 1% of a large pattern but above the floor.
        let d = one(
            stats("a", 1000, Some(100 * MB)),
            stats("a", 1000, Some(100 * MB + 500 * KB)),
            true,
        );
        assert_eq!(d.cause, Cause::Unchanged);
        // Above both.
        let d = one(
            stats("a", 1000, Some(100 * MB)),
            stats("a", 1000, Some(102 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::BiggerValues);
    }

    #[test]
    fn more_and_fewer_keys() {
        let d = one(
            stats("a", 1000, Some(10 * MB)),
            stats("a", 2000, Some(20 * MB + 100 * KB)),
            true,
        );
        assert_eq!(d.cause, Cause::MoreKeys);
        assert_eq!(d.delta_keys, 1000);
        let d = one(
            stats("a", 2000, Some(20 * MB)),
            stats("a", 1000, Some(10 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::FewerKeys);
    }

    #[test]
    fn bigger_and_smaller_values() {
        let d = one(
            stats("a", 1000, Some(10 * MB)),
            stats("a", 1000, Some(30 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::BiggerValues);
        let d = one(
            stats("a", 1000, Some(30 * MB)),
            stats("a", 1000, Some(10 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::SmallerValues);
        // Keys up 10% but average size up 100%.
        let d = one(
            stats("a", 1000, Some(10 * MB)),
            stats("a", 1100, Some(22 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::BiggerValues);
        // Bytes up, keys up a lot, average size down: keys win.
        let d = one(
            stats("a", 1000, Some(10 * MB)),
            stats("a", 3000, Some(20 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::MoreKeys);
        // Keys down 50%, average up 50%: bytes down 25%, keys win.
        let d = one(
            stats("a", 1000, Some(10 * MB)),
            stats("a", 500, Some(7 * MB + 512 * KB)),
            true,
        );
        assert_eq!(d.cause, Cause::FewerKeys);
        // Average size shrank by more than keys grew.
        let d = one(
            stats("a", 1000, Some(10 * MB)),
            stats("a", 1100, Some(5 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::SmallerValues);
    }

    #[test]
    fn without_sizes() {
        let sz = |n| stats("a", n, None);
        assert_eq!(one(sz(1000), sz(1005), false).cause, Cause::Unchanged);
        assert_eq!(one(sz(1000), sz(1010), false).cause, Cause::MoreKeys);
        assert_eq!(one(sz(1000), sz(900), false).cause, Cause::FewerKeys);
        assert_eq!(one(sz(10), sz(10), false).cause, Cause::Unchanged);
        assert_eq!(one(sz(10), sz(11), false).cause, Cause::MoreKeys);
        let d = one(sz(10), sz(11), false);
        assert_eq!(d.delta_bytes, None);
        assert_eq!(d.bytes_before, None);
    }

    #[test]
    fn mixed_size_availability_drops_bytes() {
        let d = diff(
            &report(vec![stats("a", 10, None)], false),
            &report(vec![stats("a", 20, Some(MB))], true),
        );
        let p = &d.patterns[0];
        assert_eq!(p.delta_bytes, None);
        assert_eq!(p.bytes_after, None);
        assert_eq!(p.cause, Cause::MoreKeys);
    }

    #[test]
    fn sorted_by_magnitude_then_name() {
        let d = diff(
            &report(
                vec![
                    stats("b", 10, Some(MB)),
                    stats("a", 10, Some(MB)),
                    stats("c", 10, Some(10 * MB)),
                ],
                true,
            ),
            &report(
                vec![
                    stats("b", 10, Some(2 * MB)),
                    stats("a", 10, Some(2 * MB)),
                    stats("c", 10, Some(4 * MB)),
                ],
                true,
            ),
        );
        let order: Vec<_> = d.patterns.iter().map(|p| p.pattern.as_str()).collect();
        assert_eq!(order, ["c", "a", "b"]);
        assert_eq!(d.used_memory_before, 10);
    }

    #[test]
    fn cause_direction_follows_memory() {
        // Keys up 60%, memory down 20%: the average size halved, which explains it.
        let d = one(
            stats("a", 1000, Some(10 * MB)),
            stats("a", 1600, Some(8 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::SmallerValues);
        assert!(d.delta_bytes.unwrap() < 0);
        // The mirror image: keys down, memory up.
        let d = one(
            stats("a", 1600, Some(8 * MB)),
            stats("a", 1000, Some(10 * MB)),
            true,
        );
        assert_eq!(d.cause, Cause::BiggerValues);
        // Every combination agrees with the sign of the byte change.
        for kb in [1u64, 10, 999, 1000, 5000] {
            for ka in [1u64, 10, 999, 1600, 9000] {
                for (bb, ba) in [
                    (10 * MB, 8 * MB),
                    (8 * MB, 10 * MB),
                    (MB, 40 * MB),
                    (40 * MB, MB),
                ] {
                    let c = one(stats("a", kb, Some(bb)), stats("a", ka, Some(ba)), true).cause;
                    if ba > bb {
                        assert!(
                            matches!(c, Cause::MoreKeys | Cause::BiggerValues),
                            "{kb}->{ka} {bb}->{ba}: {c:?}"
                        );
                    } else {
                        assert!(
                            matches!(c, Cause::FewerKeys | Cause::SmallerValues),
                            "{kb}->{ka} {bb}->{ba}: {c:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn huge_values_do_not_overflow() {
        let big = 1u64 << 63;
        for (b, a) in [(0, big), (big, 0), (0, u64::MAX), (u64::MAX, 0), (big, big)] {
            let d = one(stats("a", b, Some(b)), stats("a", a, Some(a)), true);
            let expect = (i128::from(a) - i128::from(b))
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
            assert_eq!(d.delta_bytes, Some(expect));
            assert_eq!(d.delta_keys, expect);
            let d = one(stats("a", b, None), stats("a", a, None), false);
            assert_eq!(d.delta_keys, expect);
        }
        let d = diff(
            &report(vec![], true),
            &report(vec![stats("a", big, Some(u64::MAX))], true),
        );
        assert_eq!(d.patterns[0].delta_bytes, Some(i64::MAX));
        assert_eq!(d.patterns[0].delta_keys, i64::MAX);
    }

    #[test]
    fn missing_size_on_one_side_has_no_delta() {
        let d = one(stats("a", 10, None), stats("a", 20, Some(MB)), true);
        assert_eq!(d.delta_bytes, None);
        assert_eq!(d.bytes_after, Some(MB));
        assert_eq!(d.cause, Cause::MoreKeys);
        let d = one(stats("a", 10, Some(MB)), stats("a", 20, None), true);
        assert_eq!(d.delta_bytes, None);
        // A pattern absent from one report still counts that side as 0.
        let d = diff(
            &report(vec![], true),
            &report(vec![stats("a", 5, Some(MB))], true),
        );
        assert_eq!(d.patterns[0].delta_bytes, Some(MB as i64));
    }

    #[test]
    fn new_and_gone_need_comparable_scans() {
        let with = |glob: Option<&str>, complete: bool, p: Vec<PatternStats>| {
            let mut r = report(p, true);
            r.match_glob = glob.map(str::to_string);
            r.complete = complete;
            r
        };
        let old = || vec![stats("user:{id}", 10, Some(MB)), stats("old", 10, Some(MB))];
        let new = || {
            vec![
                stats("user:{id}", 10, Some(MB)),
                stats("fresh", 5, Some(MB)),
            ]
        };
        // A MATCH scan compared with a full one: unsure.
        let d = diff(
            &with(Some("user:*"), false, old()),
            &with(None, true, new()),
        );
        assert_eq!(d.pattern("fresh").unwrap().cause, Cause::Unknown);
        assert_eq!(d.pattern("old").unwrap().cause, Cause::Unknown);
        assert_eq!(d.pattern("user:{id}").unwrap().cause, Cause::Unchanged);
        let d = diff(
            &with(None, true, old()),
            &with(Some("order:*"), false, new()),
        );
        assert_eq!(d.pattern("fresh").unwrap().cause, Cause::Unknown);
        // The same glob, or two complete scans, can tell.
        let d = diff(
            &with(Some("u*"), false, old()),
            &with(Some("u*"), false, new()),
        );
        assert_eq!(d.pattern("fresh").unwrap().cause, Cause::New);
        assert_eq!(d.pattern("old").unwrap().cause, Cause::Gone);
        let d = diff(&with(None, true, old()), &with(None, true, new()));
        assert_eq!(d.pattern("fresh").unwrap().cause, Cause::New);
        assert_eq!(Cause::Unknown.words(), "not in the other scan");
        assert_eq!(
            serde_json::to_string(&Cause::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    #[test]
    fn sorted_by_keys_without_sizes() {
        let d = diff(
            &report(vec![stats("a", 100, None), stats("b", 100, None)], false),
            &report(vec![stats("a", 150, None), stats("b", 20, None)], false),
        );
        assert_eq!(d.patterns[0].pattern, "b");
    }
}
