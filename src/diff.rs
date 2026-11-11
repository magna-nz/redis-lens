//! Comparing two scan reports: of the same server over time ([`diff`]), or of two
//! instances such as staging and prod ([`compare`]).

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

// ---------------------------------------------------------------------------
// Comparing two instances (`redis-lens compare`)
// ---------------------------------------------------------------------------

/// What the shares in a [`Comparison`] are shares of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareBasis {
    /// Estimated memory: both sides had sizes.
    Bytes,
    /// Estimated keys: at least one side could not run `MEMORY USAGE`.
    Keys,
}

/// Which sides a pattern was sampled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Presence {
    Both,
    LeftOnly,
    RightOnly,
}

/// Most of a pattern's keys have no TTL on one side, while most have one on the other:
/// the likely bug class (an expiry set in one environment and forgotten in the other).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TtlMismatch {
    NoTtlOnLeft,
    NoTtlOnRight,
}

/// One side of one pattern.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CompareSide {
    pub est_keys: u64,
    pub est_bytes: Option<u64>,
    pub sampled_keys: u64,
    /// This pattern's share of its side's total, by [`Comparison::share_basis`], 0.0 to 1.0.
    pub share: f64,
    /// Estimated bytes per key. `None` without sizes or keys.
    pub avg_bytes: Option<f64>,
    /// Share of sampled keys with no TTL, 0.0 to 1.0.
    pub no_ttl: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PatternCompare {
    pub pattern: String,
    pub presence: Presence,
    pub left: Option<CompareSide>,
    pub right: Option<CompareSide>,
    /// Right estimated keys / left estimated keys. `None` unless both sides have keys.
    pub key_ratio: Option<f64>,
    /// Right average size / left average size. `None` unless both sides have a non-zero
    /// average size.
    pub avg_size_ratio: Option<f64>,
    pub ttl_mismatch: Option<TtlMismatch>,
}

/// What one side of a comparison looked at.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CompareSummary {
    pub profile: String,
    pub taken_at_unix: i64,
    pub used_memory: u64,
    pub total_keys: u64,
    pub sampled_keys: u64,
    /// `sampled_keys / total_keys`, at most 1. `None` for an empty database.
    pub sample_fraction: Option<f64>,
    pub sizes_available: bool,
    pub complete: bool,
    pub match_glob: Option<String>,
}

impl CompareSummary {
    fn of(r: &ScanReport) -> Self {
        Self {
            profile: r.profile.clone(),
            taken_at_unix: r.taken_at_unix,
            used_memory: r.server.used_memory,
            total_keys: r.server.total_keys,
            sampled_keys: r.sampled_keys,
            sample_fraction: (r.server.total_keys > 0)
                .then(|| (r.sampled_keys as f64 / r.server.total_keys as f64).min(1.0)),
            sizes_available: r.sizes_available,
            complete: r.complete,
            match_glob: r.match_glob.clone(),
        }
    }
}

/// Two instances' patterns side by side.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Comparison {
    pub left: CompareSummary,
    pub right: CompareSummary,
    pub share_basis: ShareBasis,
    /// Both sides used the same `MATCH` glob (or none), so a pattern on one side only was
    /// at least within reach of the other side's scan.
    pub same_keyspace: bool,
    /// Patterns with a [`TtlMismatch`] first, then by the larger of the two shares,
    /// largest first, then by name.
    pub patterns: Vec<PatternCompare>,
}

impl Comparison {
    pub fn pattern(&self, pattern: &str) -> Option<&PatternCompare> {
        self.patterns.iter().find(|p| p.pattern == pattern)
    }
}

/// `a / b`, or `None` when it isn't a finite, meaningful number.
fn ratio(a: f64, b: f64) -> Option<f64> {
    (b > 0.0 && a.is_finite())
        .then(|| a / b)
        .filter(|r| r.is_finite())
}

fn compare_side(p: &PatternStats, basis: ShareBasis, total: u64) -> CompareSide {
    let amount = match basis {
        ShareBasis::Bytes => p.est_bytes.unwrap_or(0),
        ShareBasis::Keys => p.est_keys,
    };
    CompareSide {
        est_keys: p.est_keys,
        est_bytes: p.est_bytes,
        sampled_keys: p.sampled_keys,
        share: ratio(amount as f64, total as f64).unwrap_or(0.0),
        avg_bytes: p.est_bytes.and_then(|b| ratio(b as f64, p.est_keys as f64)),
        no_ttl: p.no_ttl_ratio(),
    }
}

/// Which side, if any, keeps most of the pattern's keys without a TTL while the other
/// gives most of them one. "Most" is more than half of the sampled keys.
fn ttl_mismatch(l: &CompareSide, r: &CompareSide) -> Option<TtlMismatch> {
    if l.sampled_keys == 0 || r.sampled_keys == 0 {
        return None;
    }
    if l.no_ttl > 0.5 && r.no_ttl < 0.5 {
        Some(TtlMismatch::NoTtlOnLeft)
    } else if r.no_ttl > 0.5 && l.no_ttl < 0.5 {
        Some(TtlMismatch::NoTtlOnRight)
    } else {
        None
    }
}

/// Compare the patterns of two instances, e.g. staging (`left`) and prod (`right`).
///
/// Datasets rarely match in size, so this works in shares of each side's total and in
/// ratios rather than absolute differences. Shares are of estimated memory when both
/// sides have sizes, of estimated keys otherwise.
pub fn compare(left: &ScanReport, right: &ScanReport) -> Comparison {
    let basis = if left.sizes_available && right.sizes_available {
        ShareBasis::Bytes
    } else {
        ShareBasis::Keys
    };
    let total = |r: &ScanReport| -> u64 {
        r.patterns
            .iter()
            .map(|p| match basis {
                ShareBasis::Bytes => p.est_bytes.unwrap_or(0),
                ShareBasis::Keys => p.est_keys,
            })
            .fold(0u64, u64::saturating_add)
    };
    let (lt, rt) = (total(left), total(right));
    let mut names: Vec<&str> = left
        .patterns
        .iter()
        .chain(&right.patterns)
        .map(|p| p.pattern.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut patterns: Vec<PatternCompare> = names
        .into_iter()
        .map(|name| {
            let side = |r: &ScanReport, t: u64| {
                r.patterns
                    .iter()
                    .find(|p| p.pattern == name)
                    .map(|p| compare_side(p, basis, t))
            };
            let (l, r) = (side(left, lt), side(right, rt));
            let presence = match (&l, &r) {
                (Some(_), Some(_)) => Presence::Both,
                (Some(_), None) => Presence::LeftOnly,
                _ => Presence::RightOnly,
            };
            let (key_ratio, avg_size_ratio, ttl) = match (&l, &r) {
                (Some(l), Some(r)) => (
                    ratio(r.est_keys as f64, l.est_keys as f64),
                    l.avg_bytes
                        .zip(r.avg_bytes)
                        .and_then(|(la, ra)| ratio(ra, la)),
                    ttl_mismatch(l, r),
                ),
                _ => (None, None, None),
            };
            PatternCompare {
                pattern: name.to_string(),
                presence,
                left: l,
                right: r,
                key_ratio,
                avg_size_ratio,
                ttl_mismatch: ttl,
            }
        })
        .collect();
    let biggest = |p: &PatternCompare| {
        let s = |side: &Option<CompareSide>| side.as_ref().map_or(0.0, |s| s.share);
        s(&p.left).max(s(&p.right))
    };
    patterns.sort_by(|a, b| {
        b.ttl_mismatch
            .is_some()
            .cmp(&a.ttl_mismatch.is_some())
            .then_with(|| biggest(b).total_cmp(&biggest(a)))
            .then_with(|| a.pattern.cmp(&b.pattern))
    });
    Comparison {
        left: CompareSummary::of(left),
        right: CompareSummary::of(right),
        share_basis: basis,
        same_keyspace: left.match_glob == right.match_glob,
        patterns,
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
            server_context: None,
            nodes: None,
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
    fn with_ttl(name: &str, keys: u64, bytes: Option<u64>, no_ttl: u64) -> PatternStats {
        let mut p = stats(name, keys, bytes);
        p.no_ttl = no_ttl;
        p
    }

    fn side_report(
        patterns: Vec<PatternStats>,
        sizes: bool,
        total: u64,
        sampled: u64,
    ) -> ScanReport {
        let mut r = report(patterns, sizes);
        r.server.total_keys = total;
        r.sampled_keys = sampled;
        r
    }

    #[test]
    fn compare_shares_ratios_and_presence() {
        // Staging is a tenth of prod: equal shares and a key ratio of 10.
        let left = side_report(
            vec![
                with_ttl("session:{id}", 100, Some(100 * KB), 0),
                with_ttl("cart:{id}", 300, Some(300 * KB), 0),
                with_ttl("debug:{id}", 10, Some(10 * KB), 10),
            ],
            true,
            410,
            410,
        );
        let right = side_report(
            vec![
                with_ttl("session:{id}", 1000, Some(2000 * KB), 0),
                with_ttl("cart:{id}", 3000, Some(3000 * KB), 0),
                with_ttl("audit:{id}", 500, Some(5000 * KB), 500),
            ],
            true,
            100_000,
            4_500,
        );
        let c = compare(&left, &right);
        assert_eq!(c.share_basis, ShareBasis::Bytes);
        assert!(c.same_keyspace);
        let s = c.pattern("session:{id}").unwrap();
        assert_eq!(s.presence, Presence::Both);
        let (l, r) = (s.left.as_ref().unwrap(), s.right.as_ref().unwrap());
        assert!((l.share - 100.0 / 410.0).abs() < 1e-12);
        assert!((r.share - 0.2).abs() < 1e-12);
        assert_eq!(s.key_ratio, Some(10.0));
        // 2 KiB a key against 1 KiB.
        assert_eq!(s.avg_size_ratio, Some(2.0));
        assert_eq!(
            c.pattern("debug:{id}").unwrap().presence,
            Presence::LeftOnly
        );
        let audit = c.pattern("audit:{id}").unwrap();
        assert_eq!(audit.presence, Presence::RightOnly);
        assert!(audit.left.is_none() && audit.key_ratio.is_none());
        assert!(audit.avg_size_ratio.is_none() && audit.ttl_mismatch.is_none());
        // Sample fractions.
        assert_eq!(c.left.sample_fraction, Some(1.0));
        assert_eq!(c.right.sample_fraction, Some(0.045));
        // Sorted by the larger share: cart (73% of staging), audit (50% of prod),
        // session, debug.
        let order: Vec<_> = c.patterns.iter().map(|p| p.pattern.as_str()).collect();
        assert_eq!(
            order,
            ["cart:{id}", "audit:{id}", "session:{id}", "debug:{id}"]
        );
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["share_basis"], "bytes");
        assert_eq!(json["patterns"][1]["presence"], "right-only");
    }

    #[test]
    fn compare_flags_ttl_missing_on_one_side() {
        let left = side_report(
            vec![
                // Expires on staging, not on prod: the bug class.
                with_ttl("cart:{id}", 100, Some(MB), 0),
                // The mirror image.
                with_ttl("lock:{id}", 10, Some(KB), 9),
                // Persistent on both: intentional.
                with_ttl("config:{var}", 5, Some(KB), 5),
                // Exactly half is not "most".
                with_ttl("mixed:{id}", 10, Some(KB), 5),
                with_ttl("big:{id}", 10, Some(100 * MB), 0),
            ],
            true,
            0,
            0,
        );
        let right = side_report(
            vec![
                with_ttl("cart:{id}", 100, Some(MB), 80),
                with_ttl("lock:{id}", 10, Some(KB), 0),
                with_ttl("config:{var}", 5, Some(KB), 5),
                with_ttl("mixed:{id}", 10, Some(KB), 10),
                with_ttl("big:{id}", 10, Some(100 * MB), 0),
            ],
            true,
            0,
            0,
        );
        let c = compare(&left, &right);
        let flag = |n: &str| c.pattern(n).unwrap().ttl_mismatch;
        assert_eq!(flag("cart:{id}"), Some(TtlMismatch::NoTtlOnRight));
        assert_eq!(flag("lock:{id}"), Some(TtlMismatch::NoTtlOnLeft));
        assert_eq!(flag("config:{var}"), None);
        assert_eq!(flag("mixed:{id}"), None);
        assert_eq!(flag("big:{id}"), None);
        // Flagged patterns lead, ahead of the much bigger big:{id}.
        let order: Vec<_> = c.patterns.iter().map(|p| p.pattern.as_str()).collect();
        assert_eq!(order[..3], ["cart:{id}", "lock:{id}", "big:{id}"]);
        // An empty database has no sample fraction.
        assert_eq!(c.left.sample_fraction, None);
        assert_eq!(
            serde_json::to_value(TtlMismatch::NoTtlOnRight).unwrap(),
            "no-ttl-on-right"
        );
    }

    #[test]
    fn compare_without_sizes_uses_key_shares() {
        let left = side_report(
            vec![stats("a", 30, Some(MB)), stats("b", 10, Some(MB))],
            true,
            40,
            40,
        );
        let right = side_report(
            vec![stats("a", 50, None), stats("b", 50, None)],
            false,
            100,
            100,
        );
        let c = compare(&left, &right);
        assert_eq!(c.share_basis, ShareBasis::Keys);
        let a = c.pattern("a").unwrap();
        assert_eq!(a.left.as_ref().unwrap().share, 0.75);
        assert_eq!(a.right.as_ref().unwrap().share, 0.5);
        assert_eq!(a.avg_size_ratio, None);
        assert!((a.key_ratio.unwrap() - 50.0 / 30.0).abs() < 1e-12);
    }

    #[test]
    fn compare_ratios_with_zeros() {
        let left = side_report(
            vec![
                stats("zero-keys", 0, Some(0)),
                stats("zero-bytes", 10, Some(0)),
                stats("empty-right", 10, Some(MB)),
            ],
            true,
            0,
            0,
        );
        let right = side_report(
            vec![
                stats("zero-keys", 5, Some(MB)),
                stats("zero-bytes", 10, Some(MB)),
                stats("empty-right", 0, Some(0)),
            ],
            true,
            0,
            0,
        );
        let c = compare(&left, &right);
        let p = |n: &str| c.pattern(n).unwrap().clone();
        // No keys on the left: no key ratio, no average size.
        assert_eq!(p("zero-keys").key_ratio, None);
        assert_eq!(p("zero-keys").left.unwrap().avg_bytes, None);
        assert_eq!(p("zero-keys").avg_size_ratio, None);
        // A zero average on the left can't be divided by.
        assert_eq!(p("zero-bytes").key_ratio, Some(1.0));
        assert_eq!(p("zero-bytes").avg_size_ratio, None);
        // Zero on the right is a ratio of 0, not missing.
        assert_eq!(p("empty-right").key_ratio, Some(0.0));
        assert_eq!(p("empty-right").avg_size_ratio, None);
        // Nothing is NaN or infinite, so the JSON has no surprises.
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("NaN") && !json.contains("inf"), "{json}");
        // An empty side: every share is 0.
        let c = compare(&report(vec![], true), &right);
        assert!(c.patterns.iter().all(|p| p.presence == Presence::RightOnly));
        let c = compare(&left, &report(vec![stats("x", 0, Some(0))], true));
        assert_eq!(c.pattern("x").unwrap().right.as_ref().unwrap().share, 0.0);
    }

    #[test]
    fn compare_with_itself_is_unchanged() {
        let r = side_report(
            vec![
                with_ttl("a", 100, Some(MB), 100),
                with_ttl("b", 10, Some(KB), 0),
            ],
            true,
            110,
            110,
        );
        let c = compare(&r, &r);
        for p in &c.patterns {
            assert_eq!(p.presence, Presence::Both, "{}", p.pattern);
            assert_eq!(p.key_ratio, Some(1.0));
            assert_eq!(p.avg_size_ratio, Some(1.0));
            assert_eq!(p.ttl_mismatch, None);
            assert_eq!(p.left, p.right);
        }
    }

    #[test]
    fn compare_notes_different_globs() {
        let mut left = report(vec![stats("user:{id}", 1, Some(1))], true);
        left.match_glob = Some("user:*".into());
        let right = report(vec![stats("order:{id}", 1, Some(1))], true);
        assert!(!compare(&left, &right).same_keyspace);
    }
}
