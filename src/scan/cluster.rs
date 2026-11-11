//! Scanning a cluster: each shard's node with `SCAN`, then merging the results.
//!
//! Owner: scanner (cluster agent in v0.3).
//!
//! Up to [`MAX_CONCURRENT_NODES`] nodes are scanned at once, each with the full command
//! budget of [`ScanOptions::max_commands_per_sec`] (the budget is per node: each node is
//! its own server). [`ScanOptions::sample_limit`] is split across the shards in
//! proportion to each node's `DBSIZE`. Each node's estimates scale by that node's own
//! `DBSIZE / keys present`, and the merged estimates are their sums. A node that fails
//! (unreachable, an error before or during sampling) is reported in its
//! [`crate::model::NodeReport`] and never fails the scan, unless no node could be
//! scanned at all.

use tokio::sync::watch;

use crate::conn::cluster::ClusterTargets;
use crate::model::ScanReport;

use super::{PartialSender, ScanError, ScanOptions, ScanProgress};

/// How many nodes are scanned at the same time.
pub const MAX_CONCURRENT_NODES: usize = 4;

/// Scan every shard of `targets` and merge the results into one [`ScanReport`] with
/// [`ScanReport::nodes`] set (see its docs for what the merged fields hold). Hot keys
/// are the top 50 across the cluster, each with [`crate::model::KeySample::node`].
///
/// `targets` must have had [`ClusterTargets::check_permissions`] run: each node uses
/// its own permission report. `progress` sums the nodes' progress; `partial` publishes
/// the merged partial patterns, throttled like [`super::scan_with_partial`].
pub async fn scan_cluster(
    _profile_name: &str,
    _targets: &mut ClusterTargets,
    _opts: &ScanOptions,
    _progress: Option<&watch::Sender<ScanProgress>>,
    _partial: Option<&PartialSender>,
) -> Result<ScanReport, ScanError> {
    Err(ScanError::Protocol(
        "cluster scans are not supported yet".into(),
    ))
}
