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
//!
//! Every note is a fixed text written here: never a server's error text, which can
//! name the user.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use redis::Value;
use redis::aio::ConnectionLike;

use crate::model::{
    Endpoint, NodeRole, PERM_LABELS, PermCheck, PermStatus, PermissionReport, Profile,
};
use crate::secrets::SecretStore;

use super::{
    ConnError, Connection, Pinned, REPLICA_TIMEOUT, ResolvedAuth, Route, ScanTarget, Unusable,
    info_replication, master_replid, may_send_credentials, open, replica_step, verified_replica,
};

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

/// The note on the only target when neither `CLUSTER SHARDS` nor `CLUSTER SLOTS` could
/// be read: the seed is scanned alone.
pub const SHARDS_UNREADABLE: &str = "the cluster's shards could not be read (CLUSTER SHARDS and \
     CLUSTER SLOTS were refused or unreadable), so only the seed node is scanned";

/// Every note that says why a shard's primary is scanned although a replica was wanted
/// starts with this.
pub const REPLICA_FALLBACK_PREFIX: &str = "replica not used: ";

/// How long reaching one announced node, connecting and checking its ID may take.
const NODE_TIMEOUT: Duration = Duration::from_secs(5);

/// The highest slot of a cluster.
const MAX_SLOT: u16 = 16_383;

impl ScanTargets {
    /// Which server(s) the scan reads from: the standalone target, or
    /// [`ScanTarget::Cluster`] with [`ClusterTargets::summary`].
    pub fn target(&self) -> ScanTarget {
        match self {
            Self::Single { target, .. } => target.clone(),
            Self::Cluster(c) => ScanTarget::Cluster(c.summary()),
        }
    }

    /// [`super::check_permissions`] on the standalone connection, or
    /// [`ClusterTargets::check_permissions`] (merged over every connected node, each
    /// node's report kept in [`NodeTarget::perms`]).
    pub async fn check_permissions(&mut self) -> PermissionReport {
        match self {
            Self::Single { conn, .. } => super::check_permissions(conn).await,
            Self::Cluster(c) => c.check_permissions().await,
        }
    }
}

impl ClusterTargets {
    /// One line for the `Target:` line and the TUI header, for example
    /// `cluster · 3 shards · replicas 2/3` or `cluster · 3 shards · 1 unreachable`.
    ///
    /// `replicas r/n` appears when a replica was wanted: some shard is scanned on a
    /// replica, or its note says why it is not ([`REPLICA_FALLBACK_PREFIX`]).
    pub fn summary(&self) -> String {
        let shards: Vec<ShardState<'_>> = self
            .nodes
            .iter()
            .map(|t| ShardState {
                role: t.role,
                connected: t.conn.is_some(),
                note: t.note.as_deref(),
                slots_known: !t.slots.is_empty(),
            })
            .collect();
        summary_text(&shards)
    }

    /// Run [`super::check_permissions`] on every connected node, keep each node's
    /// report in [`NodeTarget::perms`], and return the merged report: a command counts
    /// as allowed only when every connected node allows it.
    pub async fn check_permissions(&mut self) -> PermissionReport {
        // Collected first, boxed as `Send`: a stream that holds these iterator closures
        // trips rustc's higher-ranked `Send` check when the caller is spawned (see
        // `scan::cluster::tests::futures_are_send`).
        let checks: Vec<BoxFuture<'_, (usize, PermissionReport)>> = self
            .nodes
            .iter_mut()
            .enumerate()
            .filter(|(_, t)| t.conn.is_some())
            .map(|(i, t)| {
                async move {
                    let Some(c) = t.conn.as_mut() else {
                        return (i, PermissionReport::default());
                    };
                    let r = super::check_permissions(c).await;
                    t.perms = Some(r.clone());
                    (i, r)
                }
                .boxed()
            })
            .collect();
        let mut reports: Vec<(usize, PermissionReport)> = stream::iter(checks)
            .buffer_unordered(crate::scan::cluster::MAX_CONCURRENT_NODES)
            .collect()
            .await;
        reports.sort_by_key(|(i, _)| *i);
        let reports: Vec<PermissionReport> = reports.into_iter().map(|(_, r)| r).collect();
        merge_permissions(&reports)
    }
}

/// What [`ClusterTargets::summary`] looks at in each target.
struct ShardState<'a> {
    role: NodeRole,
    connected: bool,
    note: Option<&'a str>,
    slots_known: bool,
}

fn summary_text(shards: &[ShardState<'_>]) -> String {
    let n = shards.len();
    if n == 1 && !shards[0].slots_known {
        return "cluster · seed node only (shards unknown)".to_string();
    }
    let mut s = format!("cluster · {n} shard{}", if n == 1 { "" } else { "s" });
    let replicas = shards
        .iter()
        .filter(|t| t.role == NodeRole::Replica)
        .count();
    let wanted = replicas > 0
        || shards.iter().any(|t| {
            t.note
                .is_some_and(|n| n.starts_with(REPLICA_FALLBACK_PREFIX))
        });
    if wanted {
        s.push_str(&format!(" · replicas {replicas}/{n}"));
    }
    let down = shards.iter().filter(|t| !t.connected).count();
    if down > 0 {
        s.push_str(&format!(" · {down} unreachable"));
    }
    s
}

/// The merged permission report of a cluster, from each connected node's report in
/// node order: per label, `Allowed` only when every node allows it, otherwise the first
/// node's status that is not `Allowed` (`Denied` or `Skipped`). With no report at all,
/// every label is `Skipped`.
pub(crate) fn merge_permissions(reports: &[PermissionReport]) -> PermissionReport {
    let checks = PERM_LABELS
        .iter()
        .filter_map(|&label| {
            if reports.is_empty() {
                return Some(PermCheck {
                    label,
                    status: PermStatus::Skipped("no node of the cluster is connected".into()),
                });
            }
            let mut statuses = reports.iter().map(|r| {
                r.checks
                    .iter()
                    .find(|c| c.label == label)
                    .map(|c| &c.status)
            });
            if statuses.clone().all(|s| s.is_none()) {
                return None;
            }
            let status = statuses
                .find_map(|s| match s {
                    Some(PermStatus::Allowed) => None,
                    Some(other) => Some(other.clone()),
                    None => Some(PermStatus::Skipped("not checked on every node".into())),
                })
                .unwrap_or(PermStatus::Allowed);
            Some(PermCheck { label, status })
        })
        .collect();
    PermissionReport { checks }
}

/// Connect for a scan. Connects to the profile's endpoint as
/// [`super::connect_for_scan`] does; when that server is a cluster node (`INFO server`
/// says so, or `INFO cluster` when it doesn't), discovers the shards and connects to the
/// node to scan for each, returning [`ScanTargets::Cluster`]. Otherwise returns what
/// `connect_for_scan` returns, as [`ScanTargets::Single`].
///
/// The profile's server is connected to once: a standalone server's replica step runs
/// on that connection, and a cluster reuses it for the seed's shard. The `INFO server`
/// read to tell a cluster apart answers the scan's own first `INFO server` on that
/// connection, so detection costs the server no extra command.
///
/// A cluster only has database 0: a profile with another `db` is refused with an error
/// before any other node is connected to.
///
/// A Sentinel profile is never a cluster: its primary is asked from the Sentinels and
/// checked (a cluster node is refused), and its `endpoint` is never dialled.
pub async fn connect_targets(
    profile: &Profile,
    secrets: &dyn SecretStore,
) -> Result<ScanTargets, ConnError> {
    let route = Route::for_profile(profile, secrets).await?;
    let auth = ResolvedAuth::new(profile, secrets).await?;
    if profile.sentinel.is_some() {
        let (conn, target) = super::connect_for_scan_with(profile, secrets, &route, &auth).await?;
        return Ok(ScanTargets::Single { conn, target });
    }

    // The seed, without the replica step yet: on a cluster, replicas are chosen per
    // shard.
    let mut seed = match auth.open(&route, &profile.endpoint).await {
        Ok(c) => c,
        Err(e) => {
            // A cluster node refuses `SELECT` for any database but 0 (a server error,
            // not an I/O or authentication one): say why.
            let refused = matches!(
                &e,
                ConnError::Redis(r) if r.kind() == redis::ErrorKind::Server(
                    redis::ServerErrorKind::ResponseError
                )
            );
            if profile.db != 0
                && refused
                && seed_is_cluster_on_db0(&route, &auth, &profile.endpoint).await
            {
                return Err(db_refused(profile.db));
            }
            return Err(e);
        }
    };
    if !is_cluster(&mut seed).await {
        if profile.prefer_replica {
            // The replica step of `connect_for_scan`, on the seed's connection.
            let (conn, target) = replica_step(profile, &route, &auth, seed).await;
            return Ok(ScanTargets::Single { conn, target });
        }
        return Ok(ScanTargets::Single {
            conn: seed,
            target: ScanTarget::Primary,
        });
    }
    if profile.db != 0 {
        return Err(db_refused(profile.db));
    }
    Ok(ScanTargets::Cluster(
        discover(profile, &route, &auth, seed).await,
    ))
}

/// The error for a cluster profile whose database is not 0.
fn db_refused(db: u32) -> ConnError {
    ConnError::Redis(redis::RedisError::from(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "this profile selects database {db}, but a cluster only has database 0; \
             set the profile's database to 0 to scan it"
        ),
    )))
}

/// Whether the profile's endpoint is a cluster node, on a connection that selects
/// database 0. Only asked when connecting with the profile's database failed.
async fn seed_is_cluster_on_db0(route: &Arc<Route>, auth: &ResolvedAuth, ep: &Endpoint) -> bool {
    // The seed's forward (behind SSH) is already open: this reach is immediate.
    let Ok(reached) = route.reach(ep).await else {
        return false;
    };
    let probe = async {
        let config = auth.config.clone().set_dns_resolver(Pinned(reached.addrs));
        let c = open(ep, auth.settings.clone().set_db(0), &config)
            .await
            .ok()?;
        let mut c = Connection::new(c, route);
        Some(is_cluster(&mut c).await)
    };
    tokio::time::timeout(NODE_TIMEOUT, probe)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// What `INFO server` (or `INFO cluster`) says: `Some(true)` for a cluster node,
/// `Some(false)` for anything else it names, `None` when the text says neither.
pub(crate) fn cluster_mode(info: &str) -> Option<bool> {
    let field = |name: &str| {
        info.lines().find_map(|l| {
            let (k, v) = l.trim().split_once(':')?;
            (k == name).then(|| v.trim())
        })
    };
    // Valkey 8 says `server_mode`; Redis (and Valkey 7) `redis_mode`.
    if let Some(mode) = field("redis_mode").or_else(|| field("server_mode")) {
        return Some(mode == "cluster");
    }
    field("cluster_enabled").map(|v| v == "1")
}

/// `INFO server` says `redis_mode:cluster` (Valkey 8: `server_mode:cluster`), or (when
/// it has neither or can't be read) `INFO cluster` says `cluster_enabled:1`. A readable
/// `INFO server` reply is kept on `c` to answer the scan's own `INFO server` (see
/// `Connection::keep_info_server`).
pub(super) async fn is_cluster(c: &mut Connection) -> bool {
    let reply: Result<Value, _> = redis::cmd("INFO").arg("server").query_async(c).await;
    let server = reply.ok().and_then(|v| {
        let text = redis::from_redis_value_ref::<String>(&v).ok()?;
        Some((v, text))
    });
    if let Some((reply, text)) = server {
        c.keep_info_server(reply);
        if let Some(mode) = cluster_mode(&text) {
            return mode;
        }
    }
    let cluster: Result<String, _> = redis::cmd("INFO").arg("cluster").query_async(c).await;
    cluster
        .ok()
        .as_deref()
        .and_then(cluster_mode)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Parsing CLUSTER SHARDS and CLUSTER SLOTS
// ---------------------------------------------------------------------------

/// One node as `CLUSTER SHARDS` or `CLUSTER SLOTS` announces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnnouncedNode {
    pub id: String,
    /// `endpoint` in SHARDS; the first field in SLOTS (the preferred endpoint).
    pub endpoint: Option<String>,
    pub ip: Option<String>,
    /// Kept for completeness; only used when it is also the endpoint.
    pub hostname: Option<String>,
    /// `port` in SHARDS; the port in SLOTS (already the TLS port on a TLS cluster).
    pub port: Option<u16>,
    /// `tls-port` in SHARDS.
    pub tls_port: Option<u16>,
    /// `health: online` in SHARDS; every node listed in SLOTS.
    pub online: bool,
}

/// One shard: its slots, its primary and its replicas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shard {
    /// Inclusive ranges, ascending, adjacent ranges joined.
    pub slots: Vec<(u16, u16)>,
    pub primary: Option<AnnouncedNode>,
    pub replicas: Vec<AnnouncedNode>,
}

pub(super) fn text(v: &Value) -> Option<String> {
    match v {
        Value::BulkString(b) => String::from_utf8(b.clone()).ok(),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

pub(super) fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        other => text(other)?.trim().parse().ok(),
    }
}

pub(super) fn items(v: &Value) -> Option<&[Value]> {
    match v {
        Value::Array(a) | Value::Set(a) => Some(a),
        _ => None,
    }
}

/// Key/value pairs: a RESP3 map, or a RESP2 flat array of keys and values.
pub(super) fn pairs(v: &Value) -> Option<Vec<(String, &Value)>> {
    match v {
        Value::Map(m) => Some(m.iter().filter_map(|(k, v)| Some((text(k)?, v))).collect()),
        Value::Array(a) if a.len() % 2 == 0 => {
            let mut out = Vec::with_capacity(a.len() / 2);
            for [k, v] in a.as_chunks::<2>().0 {
                out.push((text(k)?, v));
            }
            Some(out)
        }
        _ => None,
    }
}

fn port(v: &Value) -> Option<u16> {
    int(v).and_then(|n| u16::try_from(n).ok())
}

fn slot(v: &Value) -> Option<u16> {
    int(v)
        .and_then(|n| u16::try_from(n).ok())
        .filter(|s| *s <= MAX_SLOT)
}

/// Sort ranges and join the adjacent or overlapping ones.
fn normalize_slots(mut ranges: Vec<(u16, u16)>) -> Vec<(u16, u16)> {
    ranges.sort_unstable();
    let mut out: Vec<(u16, u16)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match out.last_mut() {
            Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Parse a `CLUSTER SHARDS` reply (RESP2 arrays or RESP3 maps). `None` when the reply
/// is not shaped like one at all; shards, ranges and nodes that can't be read are
/// skipped.
pub(crate) fn parse_shards(v: &Value) -> Option<Vec<Shard>> {
    let mut out = Vec::new();
    for entry in items(v)? {
        let Some(fields) = pairs(entry) else {
            continue;
        };
        let mut slots = Vec::new();
        let mut primary = None;
        let mut replicas = Vec::new();
        for (k, v) in fields {
            match k.as_str() {
                "slots" => {
                    let Some(bounds) = items(v) else { continue };
                    for [a, b] in bounds.as_chunks::<2>().0 {
                        if let (Some(a), Some(b)) = (slot(a), slot(b))
                            && a <= b
                        {
                            slots.push((a, b));
                        }
                    }
                }
                "nodes" => {
                    let Some(nodes) = items(v) else { continue };
                    for n in nodes {
                        let Some((node, is_primary)) = shards_node(n) else {
                            continue;
                        };
                        if is_primary && primary.is_none() {
                            primary = Some(node);
                        } else if !is_primary {
                            replicas.push(node);
                        }
                    }
                }
                _ => {}
            }
        }
        out.push(Shard {
            slots: normalize_slots(slots),
            primary,
            replicas,
        });
    }
    Some(out)
}

/// One node of a SHARDS entry, and whether its role is `master`.
fn shards_node(v: &Value) -> Option<(AnnouncedNode, bool)> {
    let mut node = AnnouncedNode {
        id: String::new(),
        endpoint: None,
        ip: None,
        hostname: None,
        port: None,
        tls_port: None,
        online: false,
    };
    let mut is_primary = false;
    for (k, v) in pairs(v)? {
        match k.as_str() {
            "id" => node.id = text(v).unwrap_or_default(),
            "endpoint" => node.endpoint = text(v),
            "ip" => node.ip = text(v),
            "hostname" => node.hostname = text(v),
            "port" => node.port = port(v),
            "tls-port" => node.tls_port = port(v),
            "role" => is_primary = text(v).as_deref() == Some("master"),
            "health" => node.online = text(v).as_deref() == Some("online"),
            _ => {}
        }
    }
    (!node.id.is_empty()).then_some((node, is_primary))
}

/// Parse a `CLUSTER SLOTS` reply (Redis 6.2 `[ip, port, id]` nodes, or 7+ with a
/// metadata map) into shards, grouped by primary ID in the order first seen. `None`
/// when the reply is not shaped like one at all.
pub(crate) fn parse_slots(v: &Value) -> Option<Vec<Shard>> {
    let mut out: Vec<Shard> = Vec::new();
    for entry in items(v)? {
        let Some(e) = items(entry) else { continue };
        let (Some(a), Some(b)) = (e.first().and_then(slot), e.get(1).and_then(slot)) else {
            continue;
        };
        if a > b {
            continue;
        }
        let mut nodes = e.iter().skip(2).filter_map(slots_node);
        let Some(primary) = nodes.next() else {
            continue;
        };
        let replicas: Vec<AnnouncedNode> = nodes.collect();
        let pos = out
            .iter()
            .position(|s| s.primary.as_ref().is_some_and(|p| p.id == primary.id));
        let shard = match pos {
            Some(i) => &mut out[i],
            None => {
                out.push(Shard {
                    slots: Vec::new(),
                    primary: Some(primary),
                    replicas: Vec::new(),
                });
                out.last_mut().expect("just pushed")
            }
        };
        shard.slots.push((a, b));
        for r in replicas {
            if !shard.replicas.iter().any(|x| x.id == r.id) {
                shard.replicas.push(r);
            }
        }
    }
    for s in &mut out {
        s.slots = normalize_slots(std::mem::take(&mut s.slots));
    }
    Some(out)
}

/// `[endpoint, port, id, metadata?]` of a SLOTS entry.
fn slots_node(v: &Value) -> Option<AnnouncedNode> {
    let f = items(v)?;
    let id = f.get(2).and_then(text).filter(|id| !id.is_empty())?;
    let mut node = AnnouncedNode {
        id,
        endpoint: f.first().and_then(text),
        ip: None,
        hostname: None,
        port: f.get(1).and_then(port),
        tls_port: None,
        online: true,
    };
    if let Some(meta) = f.get(3).and_then(pairs) {
        for (k, v) in meta {
            match k.as_str() {
                "ip" => node.ip = text(v),
                "hostname" => node.hostname = text(v),
                _ => {}
            }
        }
    }
    Some(node)
}

/// Why an announced node can't be connected to, from its announcement alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddrRefusal {
    /// No endpoint or IP that can be connected to: `?`, empty, `0.0.0.0` or `::`.
    NoHost,
    /// No port, or port 0.
    NoPort,
}

/// The address to connect to for `node`: the announced `endpoint`, else its `ip` (a
/// `hostname` only where it is the endpoint), with `tls-port` when the profile uses
/// TLS (else `port`, for replies that list one port only) and `port` otherwise.
/// Hosts are checked as replica addresses are: IP addresses other than the unspecified
/// ones, in their normal form, or names made of letters, digits, `-`, `_` and dots.
pub(crate) fn node_endpoint(node: &AnnouncedNode, tls: bool) -> Result<Endpoint, AddrRefusal> {
    let host = [node.endpoint.as_deref(), node.ip.as_deref()]
        .into_iter()
        .flatten()
        .find_map(super::replica_host)
        .ok_or(AddrRefusal::NoHost)?;
    let nonzero = |p: Option<u16>| p.filter(|p| *p != 0);
    let port = if tls {
        nonzero(node.tls_port).or(nonzero(node.port))
    } else {
        nonzero(node.port)
    }
    .ok_or(AddrRefusal::NoPort)?;
    Ok(Endpoint { host, port, tls })
}

/// The address shown for a node that can't be connected to: what it announced.
fn announced_endpoint(node: &AnnouncedNode, tls: bool) -> Endpoint {
    let host = node
        .endpoint
        .clone()
        .filter(|h| !h.is_empty())
        .or_else(|| node.ip.clone())
        .unwrap_or_else(|| "?".into());
    let port = if tls {
        node.tls_port.or(node.port)
    } else {
        node.port
    };
    Endpoint {
        host,
        port: port.unwrap_or(0),
        tls,
    }
}

// ---------------------------------------------------------------------------
// Connecting to the shards
// ---------------------------------------------------------------------------

/// Why an announced node was not connected to. Turned into a fixed note by
/// [`NodeFailure::note`]; never holds a server's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeFailure {
    Address(AddrRefusal),
    /// Its host name did not resolve (or no forward could be opened to it).
    Resolve,
    /// Credentials would be needed and may not be sent there.
    WontSendCredentials,
    Auth,
    Unreachable,
    Timeout,
    /// `CLUSTER MYID` named another node.
    IdMismatch,
    /// With `prefer_replica`: it answered, but is not a replica of the shard's primary.
    NotAReplica,
    /// A replica that refused `READONLY`, without which it redirects every key command
    /// to its primary.
    ReadOnlyRefused,
}

impl NodeFailure {
    pub(crate) fn note(self, ep: &Endpoint) -> String {
        match self {
            Self::Address(AddrRefusal::NoHost) => {
                "the cluster announces no usable address for this node".to_string()
            }
            Self::Address(AddrRefusal::NoPort) => {
                "the cluster announces no usable port for this node".to_string()
            }
            Self::Resolve => format!("{ep}: the host name could not be resolved"),
            Self::WontSendCredentials => format!(
                "{ep} is a loopback address and the seed is not on the same machine (behind SSH: \
                 the bastion), so credentials are not sent to it"
            ),
            Self::Auth => format!("{ep}: authentication failed"),
            Self::Unreachable => format!("{ep}: could not connect"),
            Self::Timeout => format!("{ep}: timed out"),
            Self::IdMismatch => format!(
                "{ep} answers with a different node ID than the cluster announced, so it \
                 is not scanned"
            ),
            Self::NotAReplica => format!("{ep} is not a replica of this shard's primary"),
            Self::ReadOnlyRefused => format!(
                "{ep} refused READONLY, without which a cluster replica redirects every key \
                 to its primary"
            ),
        }
    }

    fn from_conn_error(e: &ConnError) -> Self {
        match e {
            ConnError::Redis(r)
                if r.kind() == redis::ErrorKind::AuthenticationFailed
                    || matches!(r.code(), Some("WRONGPASS" | "NOAUTH")) =>
            {
                Self::Auth
            }
            _ => Self::Unreachable,
        }
    }

    fn from_unusable(u: &Unusable) -> Self {
        match u {
            Unusable::NotAReplica => Self::NotAReplica,
            Unusable::WontSendCredentials => Self::WontSendCredentials,
            Unusable::Resolve(_) => Self::Resolve,
            Unusable::Conn(e) => Self::from_conn_error(e),
        }
    }
}

/// Whether connecting to an announced node may send the profile's credentials: always
/// when there are none to send, otherwise under [`may_send_credentials`] with the
/// seed's and the node's locality.
pub(crate) fn credentials_allowed(
    sends_credentials: bool,
    seed_local: bool,
    node_local: bool,
) -> bool {
    !sends_credentials || may_send_credentials(seed_local, node_local)
}

/// What every shard's connection shares.
struct Ctx<'a> {
    route: &'a Arc<Route>,
    auth: &'a ResolvedAuth,
    profile: &'a Profile,
    /// The seed's connection, reused for its own shard.
    seed: &'a Connection,
    seed_id: Option<&'a str>,
    /// The seed counts as on this machine (from the route's side).
    seed_local: bool,
}

impl Ctx<'_> {
    fn is_seed(&self, id: &str, ep: &Endpoint) -> bool {
        match self.seed_id {
            Some(seed) => seed == id,
            None => {
                let seed = &self.profile.endpoint;
                seed.port == ep.port && seed.host.eq_ignore_ascii_case(&ep.host)
            }
        }
    }
}

/// `CLUSTER MYID`, or `None` when it is refused or unreadable.
async fn cluster_myid<C: ConnectionLike + Send>(c: &mut C) -> Option<String> {
    let id: Result<String, _> = redis::cmd("CLUSTER").arg("MYID").query_async(c).await;
    id.ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Where `CLUSTER MYID` is allowed, the node must report the announced `id`.
async fn check_id<C: ConnectionLike + Send>(c: &mut C, id: &str) -> Result<(), NodeFailure> {
    match cluster_myid(c).await {
        Some(got) if got != id => Err(NodeFailure::IdMismatch),
        _ => Ok(()),
    }
}

/// `READONLY`: a cluster replica serves reads of its own slots on this connection
/// instead of redirecting them (`MOVED`) to its primary. Changes nothing on the server.
async fn read_only<C: ConnectionLike + Send>(c: &mut C) -> Result<(), NodeFailure> {
    redis::cmd("READONLY")
        .query_async::<()>(c)
        .await
        .map_err(|_| NodeFailure::ReadOnlyRefused)
}

/// Reach `ep` over the route, pinned to the addresses found once, and connect with the
/// profile's credentials, unless the credential rule refuses it. Then check its ID.
async fn open_announced(
    route: &Arc<Route>,
    auth: &ResolvedAuth,
    seed_local: bool,
    ep: &Endpoint,
    id: &str,
) -> Result<Connection, NodeFailure> {
    let reached = route
        .reach(ep)
        .await
        .map_err(|u| NodeFailure::from_unusable(&u))?;
    if !credentials_allowed(auth.sends_credentials, seed_local, reached.local) {
        return Err(NodeFailure::WontSendCredentials);
    }
    let config = auth.config.clone().set_dns_resolver(Pinned(reached.addrs));
    let c = open(ep, auth.settings.clone(), &config)
        .await
        .map_err(|e| NodeFailure::from_conn_error(&e))?;
    let mut c = Connection::new(c, route);
    check_id(&mut c, id).await?;
    Ok(c)
}

/// [`open_announced`] within [`NODE_TIMEOUT`].
async fn open_announced_timed(
    ctx: &Ctx<'_>,
    ep: &Endpoint,
    id: &str,
) -> Result<Connection, NodeFailure> {
    // Behind SSH, reaching a node starts its forward, which can take longer than the
    // node timeout (ssh has its own readiness limit): start it first, outside the
    // timeout. The forward is kept, so the reach inside is immediate.
    ctx.route
        .reach(ep)
        .await
        .map_err(|u| NodeFailure::from_unusable(&u))?;
    tokio::time::timeout(
        NODE_TIMEOUT,
        open_announced(ctx.route, ctx.auth, ctx.seed_local, ep, id),
    )
    .await
    .unwrap_or(Err(NodeFailure::Timeout))
}

/// Discover the shards through `seed` and connect to the node to scan for each.
async fn discover(
    profile: &Profile,
    route: &Arc<Route>,
    auth: &ResolvedAuth,
    mut seed: Connection,
) -> ClusterTargets {
    // A seed whose name no longer resolves counts as remote, which only restricts.
    let seed_local = match route.reach(&profile.endpoint).await {
        Ok(r) => r.local,
        Err(_) => false,
    };
    let seed_id = cluster_myid(&mut seed).await;
    let shards = read_shards(&mut seed).await;
    let mut shards: Vec<Shard> = match shards {
        Some(s) => s
            .into_iter()
            .filter(|s| !s.slots.is_empty() && s.primary.is_some())
            .collect(),
        None => Vec::new(),
    };
    if shards.is_empty() {
        return seed_alone(profile, seed, seed_id);
    }
    shards.sort_by_key(|s| s.slots[0]);

    let ctx = Ctx {
        route,
        auth,
        profile,
        seed: &seed,
        seed_id: seed_id.as_deref(),
        seed_local,
    };
    let connects: Vec<BoxFuture<'_, NodeTarget>> = shards
        .into_iter()
        .map(|s| connect_shard(&ctx, s).boxed())
        .collect();
    let nodes = stream::iter(connects)
        .buffered(crate::scan::cluster::MAX_CONCURRENT_NODES)
        .collect()
        .await;
    ClusterTargets { nodes }
}

/// `CLUSTER SHARDS`, else `CLUSTER SLOTS` (Redis 6.2, an unknown command, `NOPERM`, or a
/// reply that can't be read). `None` when neither gives anything.
async fn read_shards(seed: &mut Connection) -> Option<Vec<Shard>> {
    let shards: Result<Value, _> = redis::cmd("CLUSTER").arg("SHARDS").query_async(seed).await;
    if let Some(s) = shards.ok().as_ref().and_then(parse_shards)
        && !s.is_empty()
    {
        return Some(s);
    }
    let slots: Result<Value, _> = redis::cmd("CLUSTER").arg("SLOTS").query_async(seed).await;
    slots.ok().as_ref().and_then(parse_slots)
}

/// The seed scanned alone, when the shards can't be read.
fn seed_alone(profile: &Profile, seed: Connection, seed_id: Option<String>) -> ClusterTargets {
    let id = seed_id.unwrap_or_default();
    ClusterTargets {
        nodes: vec![NodeTarget {
            primary_id: id.clone(),
            id,
            endpoint: profile.endpoint.clone(),
            role: NodeRole::Primary,
            slots: Vec::new(),
            conn: Some(seed),
            note: Some(SHARDS_UNREADABLE.to_string()),
            perms: None,
        }],
    }
}

/// Connect to the node to scan for one shard (whose primary is known).
async fn connect_shard(ctx: &Ctx<'_>, shard: Shard) -> NodeTarget {
    let tls = ctx.profile.endpoint.tls;
    let primary = shard.primary.expect("shards without a primary are skipped");
    let chosen = node_endpoint(&primary, tls);
    let mut target = NodeTarget {
        id: primary.id.clone(),
        endpoint: chosen
            .clone()
            .unwrap_or_else(|_| announced_endpoint(&primary, tls)),
        role: NodeRole::Primary,
        primary_id: primary.id.clone(),
        slots: shard.slots,
        conn: None,
        note: None,
        perms: None,
    };
    let primary_conn = match &chosen {
        Err(refusal) => Err(NodeFailure::Address(*refusal)),
        Ok(ep) if ctx.is_seed(&primary.id, ep) => Ok(ctx.seed.clone()),
        Ok(ep) => open_announced_timed(ctx, ep, &primary.id).await,
    };
    let mut primary_conn = match primary_conn {
        Ok(c) => c,
        Err(f) => {
            target.note = Some(f.note(&target.endpoint));
            return target;
        }
    };
    if !ctx.profile.prefer_replica {
        target.conn = Some(primary_conn);
        return target;
    }

    match replica_for(ctx, &target.endpoint, &mut primary_conn, &shard.replicas).await {
        Ok((node, ep, conn)) => {
            target.id = node.id.clone();
            target.endpoint = ep;
            target.role = NodeRole::Replica;
            target.conn = Some(conn);
        }
        Err(why) => {
            target.conn = Some(primary_conn);
            target.note = Some(format!("{REPLICA_FALLBACK_PREFIX}{why}"));
        }
    }
    target
}

/// The first of `replicas` that is online, has a usable address and is verified as a
/// replica of the shard's primary (see [`verified_replica`]), each within the replica
/// timeout, with its ID checked and `READONLY` accepted (a cluster replica redirects
/// key commands to its primary without it). Otherwise why none could be used.
async fn replica_for<'r>(
    ctx: &Ctx<'_>,
    primary_ep: &Endpoint,
    primary: &mut Connection,
    replicas: &'r [AnnouncedNode],
) -> Result<(&'r AnnouncedNode, Endpoint, Connection), String> {
    let tls = ctx.profile.endpoint.tls;
    let candidates: Vec<(&AnnouncedNode, Endpoint)> = replicas
        .iter()
        .filter(|r| r.online)
        .filter_map(|r| Some((r, node_endpoint(r, tls).ok()?)))
        .collect();
    if candidates.is_empty() {
        return Err("no online replica with a usable address".to_string());
    }
    let Ok(info) = info_replication(primary).await else {
        return Err(format!(
            "INFO replication could not be read on the primary {primary_ep}"
        ));
    };
    let Some(replid) = master_replid(&info) else {
        return Err(
            "the primary does not report master_replid, so no replica can be verified".to_string(),
        );
    };
    let mut why = String::new();
    for (node, ep) in candidates {
        // Start the candidate's forward (behind SSH) before the short replica timeout.
        if let Err(u) = ctx.route.reach(&ep).await {
            why = NodeFailure::from_unusable(&u).note(&ep);
            continue;
        }
        // The credential rule is judged against the seed, as for every announced node.
        let attempt = async {
            let mut c =
                verified_replica(ctx.auth, ctx.route, 0, &ctx.profile.endpoint, &replid, &ep)
                    .await
                    .map_err(|u| NodeFailure::from_unusable(&u))?;
            check_id(&mut c, &node.id).await?;
            read_only(&mut c).await?;
            Ok::<_, NodeFailure>(c)
        };
        match tokio::time::timeout(REPLICA_TIMEOUT, attempt).await {
            Ok(Ok(c)) => return Ok((node, ep, c)),
            Ok(Err(f)) => why = f.note(&ep),
            Err(_) => why = NodeFailure::Timeout.note(&ep),
        }
    }
    Err(why)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }
    fn int(n: i64) -> Value {
        Value::Int(n)
    }
    fn arr(v: Vec<Value>) -> Value {
        Value::Array(v)
    }
    fn map(v: Vec<(Value, Value)>) -> Value {
        Value::Map(v)
    }

    // -- Fixed replies, captured from real servers: a 3-primary, 3-replica cluster
    // -- in one container, announcing 127.0.0.1.

    /// Redis 7.4.12, `CLUSTER SHARDS`, RESP2.
    fn redis74_shards_resp2() -> Value {
        arr(vec![
            arr(vec![
                b("slots"),
                arr(vec![int(10923), int(16383)]),
                b("nodes"),
                arr(vec![
                    arr(vec![
                        b("id"),
                        b("2f211b4f20d8b99f05168bd831b1e6e388bafab4"),
                        b("port"),
                        int(30003),
                        b("ip"),
                        b("127.0.0.1"),
                        b("endpoint"),
                        b("127.0.0.1"),
                        b("role"),
                        b("master"),
                        b("replication-offset"),
                        int(182),
                        b("health"),
                        b("online"),
                    ]),
                    arr(vec![
                        b("id"),
                        b("438601783987a2c9b26d6eb05cf7f3678cf0096b"),
                        b("port"),
                        int(30006),
                        b("ip"),
                        b("127.0.0.1"),
                        b("endpoint"),
                        b("127.0.0.1"),
                        b("role"),
                        b("replica"),
                        b("replication-offset"),
                        int(182),
                        b("health"),
                        b("online"),
                    ]),
                ]),
            ]),
            arr(vec![
                b("slots"),
                arr(vec![int(0), int(5460)]),
                b("nodes"),
                arr(vec![
                    arr(vec![
                        b("id"),
                        b("5c6ce97f62b8a1a6f88d5e41fd84b96dff736df3"),
                        b("port"),
                        int(30001),
                        b("ip"),
                        b("127.0.0.1"),
                        b("endpoint"),
                        b("127.0.0.1"),
                        b("role"),
                        b("master"),
                        b("replication-offset"),
                        int(224),
                        b("health"),
                        b("online"),
                    ]),
                    arr(vec![
                        b("id"),
                        b("b383d89861bf156c7139e945d91a74d7699eb065"),
                        b("port"),
                        int(30004),
                        b("ip"),
                        b("127.0.0.1"),
                        b("endpoint"),
                        b("127.0.0.1"),
                        b("role"),
                        b("replica"),
                        b("replication-offset"),
                        int(168),
                        b("health"),
                        b("online"),
                    ]),
                ]),
            ]),
            arr(vec![
                b("slots"),
                arr(vec![int(5461), int(10922)]),
                b("nodes"),
                arr(vec![
                    arr(vec![
                        b("id"),
                        b("99a1696c4204226c70ba1e881414c0bd7cc540e5"),
                        b("port"),
                        int(30002),
                        b("ip"),
                        b("127.0.0.1"),
                        b("endpoint"),
                        b("127.0.0.1"),
                        b("role"),
                        b("master"),
                        b("replication-offset"),
                        int(224),
                        b("health"),
                        b("online"),
                    ]),
                    arr(vec![
                        b("id"),
                        b("2f989a37d2b12d114d82b9f2cd287213d2828305"),
                        b("port"),
                        int(30005),
                        b("ip"),
                        b("127.0.0.1"),
                        b("endpoint"),
                        b("127.0.0.1"),
                        b("role"),
                        b("replica"),
                        b("replication-offset"),
                        int(224),
                        b("health"),
                        b("online"),
                    ]),
                ]),
            ]),
        ])
    }

    /// The same cluster, `CLUSTER SHARDS` over RESP3 (`HELLO 3`): maps.
    fn redis74_shards_resp3() -> Value {
        let node = |id: &str, port: i64, role: &str, offset: i64| {
            map(vec![
                (b("id"), b(id)),
                (b("port"), int(port)),
                (b("ip"), b("127.0.0.1")),
                (b("endpoint"), b("127.0.0.1")),
                (b("role"), b(role)),
                (b("replication-offset"), int(offset)),
                (b("health"), b("online")),
            ])
        };
        let shard = |a: i64, z: i64, nodes: Vec<Value>| {
            map(vec![
                (b("slots"), arr(vec![int(a), int(z)])),
                (b("nodes"), arr(nodes)),
            ])
        };
        arr(vec![
            shard(
                10923,
                16383,
                vec![
                    node(
                        "2f211b4f20d8b99f05168bd831b1e6e388bafab4",
                        30003,
                        "master",
                        182,
                    ),
                    node(
                        "438601783987a2c9b26d6eb05cf7f3678cf0096b",
                        30006,
                        "replica",
                        182,
                    ),
                ],
            ),
            shard(
                0,
                5460,
                vec![
                    node(
                        "5c6ce97f62b8a1a6f88d5e41fd84b96dff736df3",
                        30001,
                        "master",
                        224,
                    ),
                    node(
                        "b383d89861bf156c7139e945d91a74d7699eb065",
                        30004,
                        "replica",
                        168,
                    ),
                ],
            ),
            shard(
                5461,
                10922,
                vec![
                    node(
                        "99a1696c4204226c70ba1e881414c0bd7cc540e5",
                        30002,
                        "master",
                        224,
                    ),
                    node(
                        "2f989a37d2b12d114d82b9f2cd287213d2828305",
                        30005,
                        "replica",
                        224,
                    ),
                ],
            ),
        ])
    }

    /// Redis 7.4.12, `CLUSTER SLOTS` over RESP3: each node ends with an empty
    /// metadata map.
    fn redis74_slots_resp3() -> Value {
        let node = |port: i64, id: &str| arr(vec![b("127.0.0.1"), int(port), b(id), map(vec![])]);
        arr(vec![
            arr(vec![
                int(0),
                int(5460),
                node(30001, "5c6ce97f62b8a1a6f88d5e41fd84b96dff736df3"),
                node(30004, "b383d89861bf156c7139e945d91a74d7699eb065"),
            ]),
            arr(vec![
                int(5461),
                int(10922),
                node(30002, "99a1696c4204226c70ba1e881414c0bd7cc540e5"),
                node(30005, "2f989a37d2b12d114d82b9f2cd287213d2828305"),
            ]),
            arr(vec![
                int(10923),
                int(16383),
                node(30003, "2f211b4f20d8b99f05168bd831b1e6e388bafab4"),
                node(30006, "438601783987a2c9b26d6eb05cf7f3678cf0096b"),
            ]),
        ])
    }

    /// Valkey 8.1, `CLUSTER SHARDS`, RESP2 (the same fields as Redis 7.4).
    fn valkey81_shards_resp2() -> Value {
        let node = |id: &str, port: i64, role: &str, offset: i64| {
            arr(vec![
                b("id"),
                b(id),
                b("port"),
                int(port),
                b("ip"),
                b("127.0.0.1"),
                b("endpoint"),
                b("127.0.0.1"),
                b("role"),
                b(role),
                b("replication-offset"),
                int(offset),
                b("health"),
                b("online"),
            ])
        };
        let shard = |a: i64, z: i64, nodes: Vec<Value>| {
            arr(vec![
                b("slots"),
                arr(vec![int(a), int(z)]),
                b("nodes"),
                arr(nodes),
            ])
        };
        arr(vec![
            shard(
                5461,
                10922,
                vec![
                    node(
                        "ea625a80d7d95c4299e5bddcfe597453955f4c11",
                        31002,
                        "master",
                        84,
                    ),
                    node(
                        "baa748d712b74b478def21ab5714def7841e52c6",
                        31004,
                        "replica",
                        98,
                    ),
                ],
            ),
            shard(
                0,
                5460,
                vec![
                    node(
                        "6bf34c68f5ea7ba5f1fcfe6d2df5fd4de88b9966",
                        31001,
                        "master",
                        112,
                    ),
                    node(
                        "c875a883d1f92c949cf7437bd2b637c5ee34880b",
                        31006,
                        "replica",
                        112,
                    ),
                ],
            ),
            shard(
                10923,
                16383,
                vec![
                    node(
                        "ff0655b288fbf0406fb1e728d823cc9f44249928",
                        31003,
                        "master",
                        98,
                    ),
                    node(
                        "05ba13ebbb8f918eb3fa79fd2698dd82e15b9922",
                        31005,
                        "replica",
                        98,
                    ),
                ],
            ),
        ])
    }

    /// Valkey 8.1, `CLUSTER SLOTS`, RESP2: an empty metadata array per node.
    fn valkey81_slots_resp2() -> Value {
        let node = |port: i64, id: &str| arr(vec![b("127.0.0.1"), int(port), b(id), arr(vec![])]);
        arr(vec![
            arr(vec![
                int(0),
                int(5460),
                node(31001, "6bf34c68f5ea7ba5f1fcfe6d2df5fd4de88b9966"),
                node(31006, "c875a883d1f92c949cf7437bd2b637c5ee34880b"),
            ]),
            arr(vec![
                int(5461),
                int(10922),
                node(31002, "ea625a80d7d95c4299e5bddcfe597453955f4c11"),
                node(31004, "baa748d712b74b478def21ab5714def7841e52c6"),
            ]),
            arr(vec![
                int(10923),
                int(16383),
                node(31003, "ff0655b288fbf0406fb1e728d823cc9f44249928"),
                node(31005, "05ba13ebbb8f918eb3fa79fd2698dd82e15b9922"),
            ]),
        ])
    }

    /// Redis 6.2, `CLUSTER SLOTS`, RESP2: `[ip, port, id]` with no metadata.
    /// (`CLUSTER SHARDS` there answers `-ERR Unknown subcommand`.)
    fn redis62_slots_resp2() -> Value {
        let node = |port: i64, id: &str| arr(vec![b("127.0.0.1"), int(port), b(id)]);
        arr(vec![
            arr(vec![
                int(0),
                int(5460),
                node(32001, "0ff4ba068965d8d7d905088b4343da388cbbdcf9"),
                node(32004, "a2d6e6f387eb9551cf967f7c75acaac80e2108df"),
            ]),
            arr(vec![
                int(5461),
                int(10922),
                node(32002, "d66800842b46bed59f09f8e6e6148d8c0aaca147"),
                node(32005, "532f15a83a7dc8b183db8f10bdcabd038ef69249"),
            ]),
            arr(vec![
                int(10923),
                int(16383),
                node(32003, "9f54b1b649e0b9f8a05dd5bd450689a331efd9d4"),
                node(32006, "caae3d810be4bb4f76f6f28a36498e0b9545c763"),
            ]),
        ])
    }

    /// `(first slot, primary id, primary port, replica ids and ports)`, in slot order.
    type Summary = Vec<((u16, u16), String, u16, Vec<(String, u16)>)>;

    fn summarize(mut shards: Vec<Shard>) -> Summary {
        shards.sort_by_key(|s| s.slots[0]);
        shards
            .into_iter()
            .map(|s| {
                let p = s.primary.unwrap();
                (
                    s.slots[0],
                    p.id,
                    p.port.unwrap(),
                    s.replicas
                        .into_iter()
                        .map(|r| (r.id, r.port.unwrap()))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn redis74_shards_in_both_protocols_and_slots_agree() {
        let two = parse_shards(&redis74_shards_resp2()).unwrap();
        let three = parse_shards(&redis74_shards_resp3()).unwrap();
        assert_eq!(two, three);
        assert_eq!(two.len(), 3);
        assert!(two.iter().all(|s| s.primary.as_ref().unwrap().online));
        assert!(
            two.iter()
                .all(|s| s.replicas.len() == 1 && s.replicas[0].online)
        );
        let first = &two[1];
        assert_eq!(first.slots, vec![(0, 5460)]);
        let p = first.primary.as_ref().unwrap();
        assert_eq!(p.id, "5c6ce97f62b8a1a6f88d5e41fd84b96dff736df3");
        assert_eq!(p.endpoint.as_deref(), Some("127.0.0.1"));
        assert_eq!(p.ip.as_deref(), Some("127.0.0.1"));
        assert_eq!((p.port, p.tls_port), (Some(30001), None));

        let slots = parse_slots(&redis74_slots_resp3()).unwrap();
        assert_eq!(summarize(two.clone()), summarize(slots));
        let s = summarize(two);
        assert_eq!(
            s[2],
            (
                (10923, 16383),
                "2f211b4f20d8b99f05168bd831b1e6e388bafab4".to_string(),
                30003,
                vec![(
                    "438601783987a2c9b26d6eb05cf7f3678cf0096b".to_string(),
                    30006
                )]
            )
        );
    }

    #[test]
    fn valkey81_shards_and_slots_agree() {
        let shards = parse_shards(&valkey81_shards_resp2()).unwrap();
        let slots = parse_slots(&valkey81_slots_resp2()).unwrap();
        assert_eq!(shards.len(), 3);
        assert_eq!(summarize(shards), summarize(slots));
    }

    #[test]
    fn redis62_slots() {
        let slots = parse_slots(&redis62_slots_resp2()).unwrap();
        let s = summarize(slots.clone());
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].0, (0, 5460));
        assert_eq!(s[0].1, "0ff4ba068965d8d7d905088b4343da388cbbdcf9");
        assert_eq!(
            s[0].3,
            vec![("a2d6e6f387eb9551cf967f7c75acaac80e2108df".into(), 32004)]
        );
        let ep = node_endpoint(slots[0].primary.as_ref().unwrap(), false).unwrap();
        assert_eq!(ep.to_string(), "127.0.0.1:32001");
        // Every node SLOTS lists counts as online.
        assert!(slots.iter().all(|s| s.replicas.iter().all(|r| r.online)));
    }

    #[test]
    fn slots_group_split_ranges_by_primary() {
        let node = |port: i64, id: &str| arr(vec![b("10.0.0.1"), int(port), b(id)]);
        let v = arr(vec![
            arr(vec![int(0), int(99), node(7000, "a"), node(7003, "r1")]),
            arr(vec![int(200), int(299), node(7001, "b")]),
            arr(vec![int(100), int(199), node(7000, "a"), node(7004, "r2")]),
            arr(vec![
                int(300),
                int(16383),
                node(7000, "a"),
                node(7003, "r1"),
            ]),
            // Unreadable entries are skipped.
            arr(vec![int(5), int(1)]),
            arr(vec![int(0), int(20_000), node(7009, "x")]),
            b("junk"),
        ]);
        let shards = parse_slots(&v).unwrap();
        assert_eq!(shards.len(), 2);
        assert_eq!(shards[0].slots, vec![(0, 199), (300, 16383)]);
        let ids: Vec<&str> = shards[0].replicas.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["r1", "r2"]);
        assert_eq!(shards[1].slots, vec![(200, 299)]);
    }

    #[test]
    fn shards_without_slots_or_with_failed_replicas() {
        let node = |id: &str, role: &str, health: &str| {
            arr(vec![
                b("id"),
                b(id),
                b("port"),
                int(7000),
                b("ip"),
                b("10.0.0.1"),
                b("endpoint"),
                b("10.0.0.1"),
                b("role"),
                b(role),
                b("health"),
                b(health),
            ])
        };
        let v = arr(vec![
            arr(vec![
                b("slots"),
                arr(vec![]),
                b("nodes"),
                arr(vec![node("empty", "master", "online")]),
            ]),
            arr(vec![
                b("slots"),
                arr(vec![int(0), int(10), int(11), int(20), int(30), int(40)]),
                b("nodes"),
                arr(vec![
                    node("r-fail", "replica", "fail"),
                    node("p", "master", "online"),
                    node("r-load", "replica", "loading"),
                    node("r-ok", "replica", "online"),
                ]),
            ]),
        ]);
        let shards = parse_shards(&v).unwrap();
        assert_eq!(shards.len(), 2);
        assert!(shards[0].slots.is_empty());
        assert_eq!(shards[1].slots, vec![(0, 20), (30, 40)]);
        assert_eq!(shards[1].primary.as_ref().unwrap().id, "p");
        let online: Vec<&str> = shards[1]
            .replicas
            .iter()
            .filter(|r| r.online)
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(online, ["r-ok"]);
    }

    #[test]
    fn replies_that_are_not_shard_lists() {
        for v in [Value::Nil, int(3), b("ERR"), Value::Okay] {
            assert_eq!(parse_shards(&v), None);
            assert_eq!(parse_slots(&v), None);
        }
        assert_eq!(parse_shards(&arr(vec![])), Some(vec![]));
        assert_eq!(parse_slots(&arr(vec![])), Some(vec![]));
    }

    fn announced(endpoint: Option<&str>, ip: Option<&str>, port: Option<u16>) -> AnnouncedNode {
        AnnouncedNode {
            id: "n".into(),
            endpoint: endpoint.map(String::from),
            ip: ip.map(String::from),
            hostname: None,
            port,
            tls_port: None,
            online: true,
        }
    }

    #[test]
    fn endpoint_choice_and_refusals() {
        let ep = |n: &AnnouncedNode, tls| node_endpoint(n, tls).map(|e| e.to_string());
        // The endpoint wins over the IP.
        let n = announced(Some("redis-1.internal"), Some("10.0.0.1"), Some(6379));
        assert_eq!(ep(&n, false), Ok("redis-1.internal:6379".into()));
        // `?`, empty and unspecified endpoints fall back to the IP.
        for bad in [
            "?",
            "",
            "0.0.0.0",
            "::",
            "[::]",
            "bad host",
            "127.1",
            "0x7f000001",
        ] {
            let n = announced(Some(bad), Some("10.0.0.1"), Some(6379));
            assert_eq!(ep(&n, false), Ok("10.0.0.1:6379".into()), "{bad:?}");
            let n = announced(Some(bad), None, Some(6379));
            assert_eq!(ep(&n, false), Err(AddrRefusal::NoHost), "{bad:?}");
            let n = announced(Some(bad), Some(bad), Some(6379));
            assert_eq!(ep(&n, false), Err(AddrRefusal::NoHost), "{bad:?}");
        }
        // A hostname that is not the endpoint is never used.
        let mut n = announced(Some("?"), None, Some(6379));
        n.hostname = Some("redis-1.internal".into());
        assert_eq!(ep(&n, false), Err(AddrRefusal::NoHost));
        // Port 0 or none is refused.
        assert_eq!(
            ep(&announced(Some("10.0.0.1"), None, Some(0)), false),
            Err(AddrRefusal::NoPort)
        );
        assert_eq!(
            ep(&announced(Some("10.0.0.1"), None, None), false),
            Err(AddrRefusal::NoPort)
        );
        // IPv6 in its normal form.
        let n = announced(Some("2001:DB8::5"), None, Some(7000));
        assert_eq!(ep(&n, false), Ok("[2001:db8::5]:7000".into()));
    }

    #[test]
    fn tls_profiles_use_the_tls_port() {
        let mut n = announced(Some("10.0.0.1"), None, Some(6379));
        n.tls_port = Some(6380);
        assert_eq!(node_endpoint(&n, true).unwrap().port, 6380);
        assert!(node_endpoint(&n, true).unwrap().tls);
        assert_eq!(node_endpoint(&n, false).unwrap().port, 6379);
        // TLS-only node: no plain port.
        n.port = Some(0);
        assert_eq!(node_endpoint(&n, true).unwrap().port, 6380);
        assert_eq!(node_endpoint(&n, false), Err(AddrRefusal::NoPort));
        // One port only (SLOTS): it is the port to use.
        n.tls_port = None;
        n.port = Some(6390);
        assert_eq!(node_endpoint(&n, true).unwrap().port, 6390);
    }

    #[test]
    fn the_hostname_endpoint_of_shards_is_used() {
        // `cluster-preferred-endpoint-type hostname`: the endpoint is the host name.
        let v = arr(vec![arr(vec![
            b("slots"),
            arr(vec![int(0), int(16383)]),
            b("nodes"),
            arr(vec![arr(vec![
                b("id"),
                b("p"),
                b("port"),
                int(6379),
                b("tls-port"),
                int(6380),
                b("ip"),
                b("10.0.0.1"),
                b("endpoint"),
                b("redis-0.cache.internal"),
                b("hostname"),
                b("redis-0.cache.internal"),
                b("role"),
                b("master"),
                b("health"),
                b("online"),
            ])]),
        ])]);
        let shards = parse_shards(&v).unwrap();
        let p = shards[0].primary.as_ref().unwrap();
        let ep = node_endpoint(p, true).unwrap();
        assert_eq!(ep.to_string(), "redis-0.cache.internal:6380");
    }

    #[test]
    fn credentials_go_to_a_loopback_node_only_from_a_loopback_seed() {
        // Remote seed, loopback announced node, with credentials: refused.
        assert!(!credentials_allowed(true, false, true));
        assert!(credentials_allowed(true, false, false));
        assert!(credentials_allowed(true, true, true));
        assert!(credentials_allowed(true, true, false));
        // Without credentials there is nothing to protect.
        assert!(credentials_allowed(false, false, true));
    }

    /// A remote seed, a node announced on 127.0.0.1 and a profile with a password:
    /// the node is refused before anything connects to it.
    #[tokio::test]
    async fn an_announced_loopback_node_gets_no_connection_from_a_remote_seed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let auth = ResolvedAuth {
            settings: redis::RedisConnectionInfo::default()
                .set_username("lens")
                .set_password("secret-pw"),
            config: super::super::base_config(),
            sends_credentials: true,
        };
        let route = Arc::new(Route::Direct);
        let ep = Endpoint {
            host: "127.0.0.1".into(),
            port,
            tls: false,
        };
        let r = open_announced(&route, &auth, false, &ep, "id").await;
        assert_eq!(r.err(), Some(NodeFailure::WontSendCredentials));
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "something connected to the refused node");
        let note = NodeFailure::WontSendCredentials.note(&ep);
        assert!(
            !note.contains("secret-pw") && !note.contains("lens"),
            "{note}"
        );
        assert!(note.contains("not sent"), "{note}");
    }

    #[test]
    fn notes_are_fixed_texts() {
        let ep = Endpoint {
            host: "10.0.0.7".into(),
            port: 7000,
            tls: false,
        };
        assert_eq!(
            NodeFailure::Unreachable.note(&ep),
            "10.0.0.7:7000: could not connect"
        );
        assert_eq!(NodeFailure::Timeout.note(&ep), "10.0.0.7:7000: timed out");
        assert!(
            NodeFailure::IdMismatch
                .note(&ep)
                .contains("different node ID")
        );
        // Server errors never reach a note: only their kind is used.
        let wrongpass = ConnError::Redis(redis::RedisError::from((
            redis::ErrorKind::AuthenticationFailed,
            "WRONGPASS",
            "invalid username-password pair or user lens is disabled".to_string(),
        )));
        let f = NodeFailure::from_conn_error(&wrongpass);
        assert_eq!(f, NodeFailure::Auth);
        assert!(!f.note(&ep).contains("lens"));
        let io = ConnError::Redis(redis::RedisError::from(std::io::Error::other("reset")));
        assert_eq!(NodeFailure::from_conn_error(&io), NodeFailure::Unreachable);
    }

    /// Answers every command with one fixed reply.
    struct Answer(Value);

    impl ConnectionLike for Answer {
        fn req_packed_command<'a>(
            &'a mut self,
            _: &'a redis::Cmd,
        ) -> redis::RedisFuture<'a, Value> {
            let v = self.0.clone();
            Box::pin(async move { Ok(v) })
        }
        fn req_packed_commands<'a>(
            &'a mut self,
            _: &'a redis::Pipeline,
            _: usize,
            _: usize,
        ) -> redis::RedisFuture<'a, Vec<Value>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn get_db(&self) -> i64 {
            0
        }
    }

    fn error(text: &[u8]) -> Value {
        redis::parse_redis_value(text).unwrap()
    }

    #[tokio::test]
    async fn a_node_reporting_another_id_is_refused() {
        let id = "5c6ce97f62b8a1a6f88d5e41fd84b96dff736df3";
        assert_eq!(check_id(&mut Answer(b(id)), id).await, Ok(()));
        assert_eq!(
            check_id(
                &mut Answer(b("0000000000000000000000000000000000000000")),
                id
            )
            .await,
            Err(NodeFailure::IdMismatch)
        );
        // Where CLUSTER MYID is refused, the announced ID is taken as it is.
        let noperm =
            error(b"-NOPERM User lens has no permissions to run the 'cluster|myid' command\r\n");
        assert_eq!(check_id(&mut Answer(noperm), id).await, Ok(()));
        // READONLY refused: the replica can't be used.
        assert_eq!(read_only(&mut Answer(Value::Okay)).await, Ok(()));
        let denied =
            error(b"-NOPERM User lens has no permissions to run the 'readonly' command\r\n");
        assert_eq!(
            read_only(&mut Answer(denied)).await,
            Err(NodeFailure::ReadOnlyRefused)
        );
    }

    #[test]
    fn db_refusal_says_why() {
        let e = db_refused(1).to_string();
        assert!(e.contains("database 1"), "{e}");
        assert!(e.contains("only has database 0"), "{e}");
    }

    #[test]
    fn cluster_detection() {
        let server = "# Server\r\nredis_version:7.4.2\r\nredis_mode:cluster\r\n";
        assert_eq!(cluster_mode(server), Some(true));
        assert_eq!(
            cluster_mode("# Server\r\nredis_mode:standalone\r\n"),
            Some(false)
        );
        assert_eq!(
            cluster_mode("# Cluster\r\ncluster_enabled:1\r\n"),
            Some(true)
        );
        assert_eq!(
            cluster_mode("# Cluster\r\ncluster_enabled:0\r\n"),
            Some(false)
        );
        assert_eq!(cluster_mode("# Server\r\nredis_version:7.4.2\r\n"), None);
    }

    /// Valkey 8 names the field `server_mode`, so no `INFO cluster` is needed there.
    #[test]
    fn valkey_server_mode_is_read() {
        let valkey = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.1.4\r\nserver_mode:cluster\r\n";
        assert_eq!(cluster_mode(valkey), Some(true));
        assert_eq!(
            cluster_mode("# Server\r\nserver_mode:standalone\r\n"),
            Some(false)
        );
    }

    fn shard(role: NodeRole, connected: bool, note: Option<&str>) -> ShardState<'_> {
        ShardState {
            role,
            connected,
            note,
            slots_known: true,
        }
    }

    #[test]
    fn summary_text_counts_shards_replicas_and_unreachable_nodes() {
        use NodeRole::{Primary, Replica};
        let fallback = format!("{REPLICA_FALLBACK_PREFIX}no online replica");
        let unreachable = "10.0.0.1:7000: could not connect";
        let t = |shards: &[ShardState<'_>]| summary_text(shards);
        assert_eq!(
            t(&[
                shard(Primary, true, None),
                shard(Primary, true, None),
                shard(Primary, true, None),
            ]),
            "cluster · 3 shards"
        );
        assert_eq!(t(&[shard(Primary, true, None)]), "cluster · 1 shard");
        assert_eq!(
            t(&[
                shard(Replica, true, None),
                shard(Primary, true, Some(&fallback)),
                shard(Replica, true, None),
            ]),
            "cluster · 3 shards · replicas 2/3"
        );
        assert_eq!(
            t(&[
                shard(Primary, true, None),
                shard(Primary, false, Some(unreachable)),
                shard(Primary, true, None),
            ]),
            "cluster · 3 shards · 1 unreachable"
        );
        // Every replica fell back: the count still shows a replica was wanted.
        assert_eq!(
            t(&[
                shard(Primary, true, Some(&fallback)),
                shard(Primary, false, Some(unreachable)),
            ]),
            "cluster · 2 shards · replicas 0/2 · 1 unreachable"
        );
        // The seed alone.
        let seed = ShardState {
            role: Primary,
            connected: true,
            note: Some(SHARDS_UNREADABLE),
            slots_known: false,
        };
        assert_eq!(t(&[seed]), "cluster · seed node only (shards unknown)");
        // Through ClusterTargets: targets without a connection are unreachable.
        let targets = ClusterTargets {
            nodes: vec![NodeTarget {
                id: "a".into(),
                endpoint: Endpoint {
                    host: "10.0.0.1".into(),
                    port: 7000,
                    tls: false,
                },
                role: Primary,
                primary_id: "a".into(),
                slots: vec![(0, MAX_SLOT)],
                conn: None,
                note: Some(unreachable.into()),
                perms: None,
            }],
        };
        assert_eq!(targets.summary(), "cluster · 1 shard · 1 unreachable");
    }

    fn report(statuses: &[(&'static str, PermStatus)]) -> PermissionReport {
        PermissionReport {
            checks: statuses
                .iter()
                .map(|(label, status)| PermCheck {
                    label,
                    status: status.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn merged_permissions_need_every_node() {
        use crate::model::{PERM_FREQ, PERM_MEMORY, PERM_SCAN};
        let denied = PermStatus::Denied("NOPERM".into());
        let skipped = PermStatus::Skipped("eviction policy is noeviction, not LFU".into());
        let a = report(&[
            (PERM_SCAN, PermStatus::Allowed),
            (PERM_MEMORY, PermStatus::Allowed),
            (PERM_FREQ, skipped.clone()),
        ]);
        let b = report(&[
            (PERM_SCAN, PermStatus::Allowed),
            (PERM_MEMORY, denied.clone()),
            (PERM_FREQ, PermStatus::Allowed),
        ]);
        let merged = merge_permissions(&[a.clone(), b]);
        assert!(merged.allowed(PERM_SCAN));
        let status = |l| {
            merged
                .checks
                .iter()
                .find(|c| c.label == l)
                .unwrap()
                .status
                .clone()
        };
        assert_eq!(status(PERM_MEMORY), denied);
        assert_eq!(status(PERM_FREQ), skipped);
        // In PERM_LABELS order, only the labels some node reported.
        let labels: Vec<_> = merged.checks.iter().map(|c| c.label).collect();
        assert_eq!(labels, [PERM_SCAN, PERM_MEMORY, PERM_FREQ]);
        // One node: its own report.
        assert_eq!(merge_permissions(std::slice::from_ref(&a)), a);
        // No connected node: nothing is allowed.
        let none = merge_permissions(&[]);
        assert_eq!(none.checks.len(), PERM_LABELS.len());
        assert!(PERM_LABELS.iter().all(|l| !none.allowed(l)));
    }

    // -- Property tests: any shard list written out as SHARDS (RESP2 or RESP3) or
    // -- SLOTS parses back to itself.

    #[derive(Debug, Clone)]
    struct GenShard {
        ranges: Vec<(u16, u16)>,
        primary: (String, u16),
        replicas: Vec<(String, u16, bool)>,
    }

    fn gen_shards() -> impl Strategy<Value = Vec<GenShard>> {
        // Cut points split the slot space into disjoint ranges, dealt to shards.
        (
            proptest::collection::btree_set(1u16..MAX_SLOT, 1..12),
            1usize..5,
            proptest::collection::vec((any::<u8>(), 0usize..3, any::<bool>()), 5),
        )
            .prop_map(|(cuts, n, extra)| {
                let mut bounds: Vec<u16> = vec![0];
                bounds.extend(cuts);
                bounds.push(MAX_SLOT + 1);
                let ranges: Vec<(u16, u16)> = bounds.windows(2).map(|w| (w[0], w[1] - 1)).collect();
                let n = n.min(ranges.len());
                let mut shards: Vec<GenShard> = (0..n)
                    .map(|i| GenShard {
                        ranges: Vec::new(),
                        primary: (format!("p{i:039}"), 7000 + i as u16),
                        replicas: (0..extra[i].1)
                            .map(|j| {
                                (
                                    format!("r{i}{j:038}"),
                                    8000 + (i * 10 + j) as u16,
                                    extra[i].2 ^ (j % 2 == 1),
                                )
                            })
                            .collect(),
                    })
                    .collect();
                for (k, r) in ranges.into_iter().enumerate() {
                    shards[(k + extra[k % 5].0 as usize) % n].ranges.push(r);
                }
                shards.retain(|s| !s.ranges.is_empty());
                shards
            })
    }

    fn shards_value(shards: &[GenShard], resp3: bool) -> Value {
        let kv = |pairs: Vec<(&str, Value)>| {
            if resp3 {
                map(pairs.into_iter().map(|(k, v)| (b(k), v)).collect())
            } else {
                arr(pairs.into_iter().flat_map(|(k, v)| [b(k), v]).collect())
            }
        };
        let node = |id: &str, port: u16, role: &str, online: bool| {
            kv(vec![
                ("id", b(id)),
                ("port", int(i64::from(port))),
                ("ip", b("10.1.2.3")),
                ("endpoint", b("10.1.2.3")),
                ("role", b(role)),
                ("replication-offset", int(42)),
                ("health", b(if online { "online" } else { "loading" })),
            ])
        };
        arr(shards
            .iter()
            .map(|s| {
                let mut nodes = vec![node(&s.primary.0, s.primary.1, "master", true)];
                for (id, port, online) in &s.replicas {
                    nodes.push(node(id, *port, "replica", *online));
                }
                kv(vec![
                    (
                        "slots",
                        arr(s
                            .ranges
                            .iter()
                            .flat_map(|(a, z)| [int(i64::from(*a)), int(i64::from(*z))])
                            .collect()),
                    ),
                    ("nodes", arr(nodes)),
                ])
            })
            .collect())
    }

    fn slots_value(shards: &[GenShard]) -> Value {
        let node = |id: &str, port: u16| arr(vec![b("10.1.2.3"), int(i64::from(port)), b(id)]);
        let mut entries: Vec<(u16, Value)> = Vec::new();
        for s in shards {
            for (a, z) in &s.ranges {
                let mut e = vec![
                    int(i64::from(*a)),
                    int(i64::from(*z)),
                    node(&s.primary.0, s.primary.1),
                ];
                e.extend(s.replicas.iter().map(|(id, port, _)| node(id, *port)));
                entries.push((*a, arr(e)));
            }
        }
        entries.sort_by_key(|(a, _)| *a);
        arr(entries.into_iter().map(|(_, v)| v).collect())
    }

    /// `(slots, primary id, replica ids with their online flag)`.
    type ParsedShard = (Vec<(u16, u16)>, String, Vec<(String, bool)>);

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn shards_and_slots_round_trip(shards in gen_shards()) {
            let expect: Vec<ParsedShard> = shards
                .iter()
                .map(|s| (
                    normalize_slots(s.ranges.clone()),
                    s.primary.0.clone(),
                    s.replicas.iter().map(|(id, _, on)| (id.clone(), *on)).collect(),
                ))
                .collect();
            for resp3 in [false, true] {
                let got: Vec<_> = parse_shards(&shards_value(&shards, resp3))
                    .unwrap()
                    .into_iter()
                    .map(|s| (
                        s.slots,
                        s.primary.unwrap().id,
                        s.replicas.into_iter().map(|r| (r.id, r.online)).collect::<Vec<_>>(),
                    ))
                    .collect();
                prop_assert_eq!(&got, &expect);
            }
            // SLOTS lists the shards by range and every node as online.
            let mut by_slot = expect.clone();
            by_slot.sort_by_key(|(slots, _, _)| slots[0]);
            let mut got: Vec<_> = parse_slots(&slots_value(&shards))
                .unwrap()
                .into_iter()
                .map(|s| (
                    s.slots,
                    s.primary.unwrap().id,
                    s.replicas.into_iter().map(|r| (r.id, r.online)).collect::<Vec<_>>(),
                ))
                .collect();
            got.sort_by_key(|(slots, _, _)| slots[0]);
            for (shard, want) in got.iter().zip(&by_slot) {
                prop_assert_eq!(&shard.0, &want.0);
                prop_assert_eq!(&shard.1, &want.1);
                let ids: Vec<&String> = shard.2.iter().map(|(id, _)| id).collect();
                let want_ids: Vec<&String> = want.2.iter().map(|(id, _)| id).collect();
                prop_assert_eq!(ids, want_ids);
                prop_assert!(shard.2.iter().all(|(_, on)| *on));
            }
            prop_assert_eq!(got.len(), by_slot.len());
        }

        #[test]
        fn parsers_never_panic(v in arb_value()) {
            if let Some(shards) = parse_shards(&v) {
                for s in shards {
                    prop_assert!(s.slots.iter().all(|(a, z)| a <= z && *z <= MAX_SLOT));
                }
            }
            if let Some(shards) = parse_slots(&v) {
                for s in shards {
                    prop_assert!(!s.slots.is_empty());
                    prop_assert!(s.primary.is_some());
                }
            }
        }

        #[test]
        fn chosen_endpoints_are_always_connectable(
            endpoint in proptest::option::of("[ -~]{0,20}"),
            ip in proptest::option::of(prop_oneof![
                Just("0.0.0.0".to_string()),
                Just("::".to_string()),
                Just("?".to_string()),
                Just(String::new()),
                "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
            ]),
            port in proptest::option::of(any::<u16>()),
            tls_port in proptest::option::of(any::<u16>()),
            tls in any::<bool>(),
        ) {
            let n = AnnouncedNode {
                id: "n".into(),
                endpoint,
                ip,
                hostname: None,
                port,
                tls_port,
                online: true,
            };
            if let Ok(ep) = node_endpoint(&n, tls) {
                prop_assert!(ep.port != 0);
                prop_assert!(!ep.host.is_empty() && ep.host != "?");
                if let Ok(ip) = ep.host.parse::<std::net::IpAddr>() {
                    prop_assert!(!ip.to_canonical().is_unspecified());
                }
                prop_assert_eq!(ep.tls, tls);
            }
        }
    }

    fn arb_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Nil),
            any::<i64>().prop_map(Value::Int),
            (-5i64..20_000).prop_map(Value::Int),
            "[a-z-]{0,12}".prop_map(|s| Value::BulkString(s.into_bytes())),
            prop_oneof![
                Just("slots"),
                Just("nodes"),
                Just("id"),
                Just("port"),
                Just("endpoint"),
                Just("role"),
                Just("master"),
            ]
            .prop_map(|s| Value::BulkString(s.as_bytes().to_vec())),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                proptest::collection::vec((inner.clone(), inner), 0..4).prop_map(Value::Map),
            ]
        })
    }
}
