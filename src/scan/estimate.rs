//! Analysis presets and a rough cost estimate, for the "Analyze the keys?" dialog
//! ("Quick 10,000 keys, about 35K commands, about 7 s"). Pure: sends nothing.
//!
//! Owner: scanner. An analysis is the scan ([`super::scan_with_budget`],
//! [`super::cluster::scan_targets`]) with [`super::ScanOptions::sample_limit`] set to
//! [`QUICK_SAMPLE`] or [`DEEP_SAMPLE`]. The `scan` command keeps its own default of
//! 100,000.

use std::time::Duration;

use super::cluster::{MAX_CONCURRENT_NODES, split_sample_limit};
use super::figures::FIGURE_COMMANDS;

/// Keys a Quick analysis samples (the default in the dialog).
pub const QUICK_SAMPLE: u64 = 10_000;
/// Keys a Deep analysis samples.
pub const DEEP_SAMPLE: u64 = 100_000;

/// The `COUNT` hint the scan passes to `SCAN` by default; each call returns about that
/// many keys.
const SCAN_COUNT: u64 = 1_000;

/// What an analysis is expected to cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalysisEstimate {
    /// Keys it expects to look up: the sample, or every key when the database (or a
    /// shard) holds fewer.
    pub keys: u64,
    /// Commands it sends: the figures it reads first ([`FIGURE_COMMANDS`] per node),
    /// one `SCAN` per [`SCAN_COUNT`] keys, and `TYPE` and `PTTL` per key, plus `MEMORY
    /// USAGE` with sizes and `OBJECT FREQ` with LFU counters.
    pub commands: u64,
    /// How long the command budget makes it take at least: commands over
    /// `max_commands_per_sec` (on a cluster, per node, up to
    /// [`MAX_CONCURRENT_NODES`] nodes at a time). Round trips come on top, so it is a
    /// lower bound, close on a server nearby. `None` without a limit: the analysis
    /// then runs as fast as the server answers, usually a few seconds or less.
    pub duration: Option<Duration>,
}

/// Commands per key looked up.
fn per_key(sizes: bool, freq: bool) -> u64 {
    2 + u64::from(sizes) + u64::from(freq)
}

/// Commands one server's analysis sends with a sample limit of `limit` over `dbsize`
/// keys, and the keys it looks up.
fn server_cost(limit: u64, dbsize: u64, sizes: bool, freq: bool) -> (u64, u64) {
    let keys = limit.min(dbsize);
    // A limit of 0 sends no SCAN; an empty database still answers one.
    let scans = if limit == 0 {
        0
    } else {
        keys.div_ceil(SCAN_COUNT).max(1)
    };
    let commands = FIGURE_COMMANDS
        .saturating_add(scans)
        .saturating_add(keys.saturating_mul(per_key(sizes, freq)));
    (keys, commands)
}

fn at_rate(commands: u64, max_commands_per_sec: Option<u32>) -> Option<Duration> {
    let rate = max_commands_per_sec.filter(|r| *r > 0)?;
    Some(Duration::from_secs_f64(commands as f64 / f64::from(rate)))
}

/// The cost of analysing `sample` keys of a standalone server with `dbsize` keys.
/// `sizes`: `MEMORY USAGE` is allowed; `freq`: `OBJECT FREQ` is allowed and the policy
/// is LFU (the same choices the scan makes from the permission check and `INFO`).
/// `max_commands_per_sec` is the profile's budget ([`super::ScanOptions::for_profile`]),
/// `None` for no limit.
pub fn estimate_analysis(
    sample: u64,
    dbsize: u64,
    sizes: bool,
    freq: bool,
    max_commands_per_sec: Option<u32>,
) -> AnalysisEstimate {
    let (keys, commands) = server_cost(sample, dbsize, sizes, freq);
    AnalysisEstimate {
        keys,
        commands,
        duration: at_rate(commands, max_commands_per_sec),
    }
}

/// [`estimate_analysis`] for a cluster whose nodes hold `dbsizes` keys (in the scan's
/// node order). The sample is split across the nodes in proportion to their `DBSIZE`
/// as the cluster scan splits it, each node has the full budget, and up to
/// [`MAX_CONCURRENT_NODES`] are sampled at once: the duration is when the last node
/// would finish, nodes taken in order as a place frees up.
pub fn estimate_cluster_analysis(
    sample: u64,
    dbsizes: &[u64],
    sizes: bool,
    freq: bool,
    max_commands_per_sec: Option<u32>,
) -> AnalysisEstimate {
    let quotas = split_sample_limit(sample, dbsizes);
    let mut keys = 0u64;
    let mut commands = 0u64;
    // When each of the places running a node frees up, in commands.
    let mut lanes = [0u64; MAX_CONCURRENT_NODES];
    for (quota, dbsize) in quotas.iter().zip(dbsizes) {
        let (k, c) = server_cost(*quota, *dbsize, sizes, freq);
        keys = keys.saturating_add(k);
        commands = commands.saturating_add(c);
        if let Some(lane) = lanes.iter_mut().min() {
            *lane = lane.saturating_add(c);
        }
    }
    let longest = lanes.iter().copied().max().unwrap_or(0);
    AnalysisEstimate {
        keys,
        commands,
        duration: at_rate(longest, max_commands_per_sec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets() {
        assert_eq!(QUICK_SAMPLE, 10_000);
        assert_eq!(DEEP_SAMPLE, 100_000);
        // The CLI's default stays the deep one.
        assert_eq!(
            super::super::ScanOptions::default().sample_limit,
            DEEP_SAMPLE
        );
    }

    #[test]
    fn quick_and_deep_on_a_big_server() {
        // 10,000 keys of 7.92M, with sizes, no LFU: 6 + 10 SCAN + 30,000.
        let q = estimate_analysis(QUICK_SAMPLE, 7_920_000, true, false, Some(5_000));
        assert_eq!(q.keys, 10_000);
        assert_eq!(q.commands, 30_016);
        assert_eq!(
            q.duration,
            Some(Duration::from_secs_f64(30_016.0 / 5_000.0))
        );
        // With LFU counters too: 4 per key.
        let q = estimate_analysis(QUICK_SAMPLE, 7_920_000, true, true, Some(5_000));
        assert_eq!(q.commands, 40_016);
        // Deep: ten times the keys.
        let d = estimate_analysis(DEEP_SAMPLE, 7_920_000, true, true, Some(5_000));
        assert_eq!(d.keys, 100_000);
        assert_eq!(d.commands, 6 + 100 + 400_000);
        assert!(d.duration.unwrap() > Duration::from_secs(80));
        // Without sizes: TYPE and PTTL only.
        let n = estimate_analysis(QUICK_SAMPLE, 7_920_000, false, false, Some(1_000));
        assert_eq!(n.commands, 6 + 10 + 20_000);
        assert_eq!(n.duration, Some(Duration::from_secs_f64(20.016)));
    }

    #[test]
    fn small_and_empty_databases() {
        // Fewer keys than the sample: every key, and one SCAN per thousand.
        let e = estimate_analysis(QUICK_SAMPLE, 2_500, true, false, Some(5_000));
        assert_eq!(e.keys, 2_500);
        assert_eq!(e.commands, 6 + 3 + 7_500);
        // An empty database: the figures and one SCAN.
        let e = estimate_analysis(QUICK_SAMPLE, 0, true, true, Some(5_000));
        assert_eq!((e.keys, e.commands), (0, 7));
        // A sample of 0 sends no SCAN.
        let e = estimate_analysis(0, 1_000, true, true, None);
        assert_eq!((e.keys, e.commands), (0, 6));
    }

    #[test]
    fn no_limit_has_no_duration() {
        for rate in [None, Some(0)] {
            let e = estimate_analysis(DEEP_SAMPLE, 1_000_000, true, true, rate);
            assert_eq!(e.duration, None, "{rate:?}");
            assert!(e.commands > 400_000);
        }
    }

    #[test]
    fn a_cluster_splits_the_sample_and_runs_nodes_side_by_side() {
        // Three equal shards: 3,334 + 3,333 + 3,333 keys, all at once.
        let c = estimate_cluster_analysis(QUICK_SAMPLE, &[1_000_000; 3], true, false, Some(5_000));
        assert_eq!(c.keys, 10_000);
        assert_eq!(c.commands, 3 * 6 + 3 * 4 + 30_000);
        // The longest node: 6 + 4 SCAN + 3,334 * 3.
        let longest = 6 + 4 + 3_334 * 3;
        assert_eq!(
            c.duration,
            Some(Duration::from_secs_f64(longest as f64 / 5_000.0))
        );
        let single = estimate_analysis(QUICK_SAMPLE, 3_000_000, true, false, Some(5_000));
        assert!(c.duration < single.duration);

        // Six shards: four at a time, the last two after the first two finish.
        let c = estimate_cluster_analysis(6_000, &[100_000; 6], false, false, Some(1_000));
        let per_node = 6 + 1 + 1_000 * 2;
        assert_eq!(c.commands, 6 * per_node);
        assert_eq!(
            c.duration,
            Some(Duration::from_secs_f64(2.0 * per_node as f64 / 1_000.0))
        );
        // An empty shard reads its figures only.
        let c = estimate_cluster_analysis(100, &[0, 50], true, true, None);
        assert_eq!(c.keys, 50);
        assert_eq!(c.commands, 6 + (6 + 1 + 50 * 4));
        assert_eq!(c.duration, None);
        // One shard is a standalone server.
        assert_eq!(
            estimate_cluster_analysis(QUICK_SAMPLE, &[42_000], true, true, Some(5_000)),
            estimate_analysis(QUICK_SAMPLE, 42_000, true, true, Some(5_000))
        );
    }

    proptest::proptest! {
        /// More keys never cost fewer commands or less time, and the keys looked up
        /// never exceed the sample or the database.
        #[test]
        fn estimates_grow_with_the_sample(
            a in 0u64..200_000,
            b in 0u64..200_000,
            dbsize in 0u64..10_000_000,
            sizes in proptest::bool::ANY,
            freq in proptest::bool::ANY,
            rate in 1u32..20_000,
        ) {
            let (lo, hi) = (a.min(b), a.max(b));
            let x = estimate_analysis(lo, dbsize, sizes, freq, Some(rate));
            let y = estimate_analysis(hi, dbsize, sizes, freq, Some(rate));
            proptest::prop_assert!(x.commands <= y.commands);
            proptest::prop_assert!(x.duration <= y.duration);
            proptest::prop_assert!(y.keys <= hi.min(dbsize));
        }
    }
}
