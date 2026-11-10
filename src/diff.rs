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
    /// The key count changed more than the average size did.
    MoreKeys,
    FewerKeys,
    /// The average size changed more than the key count did.
    BiggerValues,
    SmallerValues,
    /// Changed by less than 1% and less than 64 KiB (or 1% of keys when sizes are unavailable).
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PatternDiff {
    pub pattern: String,
    pub keys_before: Option<u64>,
    pub keys_after: Option<u64>,
    /// `None` when that report had no sizes.
    pub bytes_before: Option<u64>,
    pub bytes_after: Option<u64>,
    /// `bytes_after - bytes_before`, treating a missing side as 0. `None` when either
    /// report lacks sizes.
    pub delta_bytes: Option<i64>,
    pub delta_keys: i64,
    pub cause: Cause,
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

/// Compare an older report with a newer one, matching patterns by name.
pub fn diff(before: &ScanReport, after: &ScanReport) -> ReportDiff {
    let sizes = before.sizes_available && after.sizes_available;
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
            pattern_diff(name, b, a, sizes)
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
    let delta_bytes =
        sizes.then(|| bytes_after.unwrap_or(0) as i64 - bytes_before.unwrap_or(0) as i64);
    let delta_keys = keys_after.unwrap_or(0) as i64 - keys_before.unwrap_or(0) as i64;
    let cause = match (b, a) {
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
    }
}

/// The cause for a pattern present in both reports.
fn classify(kb: u64, ka: u64, bytes: Option<(u64, u64)>) -> Cause {
    const FLOOR_BYTES: f64 = 64.0 * 1024.0;
    let dk = ka as i64 - kb as i64;
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
    let db = ba as i64 - bb as i64;
    if (db.unsigned_abs() as f64) < (bb as f64 * 0.01).max(FLOOR_BYTES) {
        return Cause::Unchanged;
    }
    let keys_cause = if dk > 0 {
        Cause::MoreKeys
    } else {
        Cause::FewerKeys
    };
    let size_cause = if db > 0 {
        Cause::BiggerValues
    } else {
        Cause::SmallerValues
    };
    if kb == 0 || ka == 0 {
        // No average size on one side: the key count is what changed.
        return if dk != 0 { keys_cause } else { size_cause };
    }
    let rel_keys = ka as f64 / kb as f64 - 1.0;
    let rel_avg = (ba as f64 / ka as f64) / (bb as f64 / kb as f64).max(f64::MIN_POSITIVE) - 1.0;
    if rel_keys.abs() >= rel_avg.abs() {
        if dk == 0 { size_cause } else { keys_cause }
    } else {
        size_cause_by(rel_avg, size_cause)
    }
}

fn size_cause_by(rel_avg: f64, fallback: Cause) -> Cause {
    if rel_avg > 0.0 {
        Cause::BiggerValues
    } else if rel_avg < 0.0 {
        Cause::SmallerValues
    } else {
        fallback
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
        }
    }

    fn one(before: PatternStats, after: PatternStats, sizes: bool) -> PatternDiff {
        let d = diff(&report(vec![before], sizes), &report(vec![after], sizes));
        d.patterns[0].clone()
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
    fn sorted_by_keys_without_sizes() {
        let d = diff(
            &report(vec![stats("a", 100, None), stats("b", 100, None)], false),
            &report(vec![stats("a", 150, None), stats("b", 20, None)], false),
        );
        assert_eq!(d.patterns[0].pattern, "b");
    }
}
