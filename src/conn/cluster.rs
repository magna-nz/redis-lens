//! Cluster discovery: finding a cluster's shards and connecting to the node to scan for
//! each, over plain per-node connections (not redis-rs's cluster client).
//!
//! Owner: connections (cluster agent in v0.3).
//!
//! Discovery reads `CLUSTER SHARDS` (Redis 7+, Valkey) and falls back to `CLUSTER SLOTS`
//! (Redis 6.2, or `CLUSTER SHARDS` denied). Every node is reached over the profile's
//! [`Route`] with the profile's credentials, resolved once. Credentials go to an
//! announced node only under the same rule as replica candidates (not loopback, or the
//! seed is loopback too; behind SSH judged from the bastion). With `prefer_replica`,
//! each shard's online replicas are tried and verified as in
//! [`super::connect_for_scan`]; a shard whose replica can't be used is scanned on its
//! primary, with a note. A node that can't be reached leaves its shard without a
//! connection and with a note; it never fails the whole connect.

use crate::model::{Endpoint, NodeRole, PermissionReport, Profile};
use crate::secrets::SecretStore;

use super::{ConnError, Connection, ScanTarget};

/// What a scan runs against.
pub enum ScanTargets {
    /// A standalone server (or a replica of one), as [`super::connect_for_scan`] gives.
    Single {
        conn: Connection,
        target: ScanTarget,
    },
    /// A cluster: one entry per shard.
    Cluster(ClusterTargets),
}

/// The node chosen to scan for each shard of a cluster, in slot order.
pub struct ClusterTargets {
    pub nodes: Vec<NodeTarget>,
}

/// One shard of a cluster and the node chosen to scan for it.
pub struct NodeTarget {
    /// The node ID of the node to scan.
    pub id: String,
    /// Its announced address (never the local end of an SSH forward).
    pub endpoint: Endpoint,
    pub role: NodeRole,
    /// The node ID of the shard's primary; equal to `id` for a primary.
    pub primary_id: String,
    /// The shard's slot ranges, inclusive, ascending.
    pub slots: Vec<(u16, u16)>,
    /// `None` when no node of the shard could be connected; `note` says why.
    pub conn: Option<Connection>,
    /// Fixed text written by redis-lens: why `conn` is `None`, or why the primary is
    /// scanned although a replica was wanted. Never a server's error text.
    pub note: Option<String>,
    /// Filled by [`ClusterTargets::check_permissions`].
    pub perms: Option<PermissionReport>,
}

impl ClusterTargets {
    /// One line for the `Target:` line and the TUI header, for example
    /// `cluster · 3 shards · replicas 2/3` or `cluster · 3 shards · 1 unreachable`.
    pub fn summary(&self) -> String {
        String::new()
    }

    /// Run [`super::check_permissions`] on every connected node, keep each node's
    /// report in [`NodeTarget::perms`], and return the merged report: a command counts
    /// as allowed only when every connected node allows it.
    pub async fn check_permissions(&mut self) -> PermissionReport {
        PermissionReport::default()
    }
}

/// Connect for a scan. Connects to the profile's endpoint as
/// [`super::connect_for_scan`] does; when that server is a cluster node
/// (`cluster_enabled:1` in `INFO cluster`), discovers the shards and connects to the
/// node to scan for each, returning [`ScanTargets::Cluster`]. Otherwise returns what
/// `connect_for_scan` returns, as [`ScanTargets::Single`].
pub async fn connect_targets(
    profile: &Profile,
    secrets: &dyn SecretStore,
) -> Result<ScanTargets, ConnError> {
    let (conn, target) = super::connect_for_scan(profile, secrets).await?;
    Ok(ScanTargets::Single { conn, target })
}
