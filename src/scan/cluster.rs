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
//!
//! Patterns are inferred once over the union of every node's keys (so the `{var}`
//! collapse sees all of them), and both the merged rows and each node's rows come from
//! that one assignment: a node's rows always add up to the merged rows (see
//! [`crate::pattern::aggregate_nodes`]).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use redis::aio::ConnectionLike;
use tokio::sync::watch;

use crate::conn::cluster::{ClusterTargets, ScanTargets};
use crate::model::{
    Endpoint, KeySample, KeysizeHistogram, KeyspaceInfo, MemoryStats, NodeReport, NodeRole,
    NodeStatus, PERM_MEMORY, PermissionReport, ScanReport, ServerContext, ServerContextParts,
    ServerInfo, ServerMode,
};

use super::{
    PartialHook, PartialPatterns, PartialSender, ScanError, ScanOptions, ScanProgress,
    ServerSample, Throttle, estimate_scale, hot_keys, sample_server, server_info,
};

/// How many nodes are scanned at the same time.
pub const MAX_CONCURRENT_NODES: usize = 4;

/// The number of slots in a cluster.
const SLOTS: u32 = 16_384;

/// Note for a node whose `INFO` or `DBSIZE` could not be read.
const NOTE_INFO_FAILED: &str =
    "INFO or DBSIZE could not be read on this node, so it was not scanned";
/// Note for a node whose walk stopped early.
const NOTE_STOPPED: &str =
    "the scan of this node stopped early (a connection or server error); its results are partial";
/// Note for a node that was not connected and carries no note of its own.
const NOTE_NOT_CONNECTED: &str = "this node could not be connected to";

/// Scan whatever [`crate::conn::cluster::connect_targets`] returned: a standalone server
/// with [`super::scan_with_partial`] and `perms`, or a cluster with [`scan_cluster`]
/// (which uses each node's own report and ignores `perms`).
///
/// Run [`ScanTargets::check_permissions`] first and pass its result as `perms`.
pub async fn scan_targets(
    profile_name: &str,
    targets: &mut ScanTargets,
    opts: &ScanOptions,
    perms: &PermissionReport,
    progress: Option<&watch::Sender<ScanProgress>>,
    partial: Option<&PartialSender>,
) -> Result<ScanReport, ScanError> {
    match targets {
        ScanTargets::Single { conn, .. } => {
            super::scan_with_partial(profile_name, conn, opts, perms, progress, partial).await
        }
        ScanTargets::Cluster(c) => scan_cluster(profile_name, c, opts, progress, partial).await,
    }
}

/// Scan every shard of `targets` and merge the results into one [`ScanReport`] with
/// [`ScanReport::nodes`] set (see its docs for what the merged fields hold). Hot keys
/// are the top 50 across the cluster, each with [`crate::model::KeySample::node`].
///
/// `targets` must have had [`ClusterTargets::check_permissions`] run: each node uses
/// its own permission report. `progress` sums the nodes' progress; `partial` publishes
/// the merged partial patterns, throttled like [`super::scan_with_partial`].
pub async fn scan_cluster(
    profile_name: &str,
    targets: &mut ClusterTargets,
    opts: &ScanOptions,
    progress: Option<&watch::Sender<ScanProgress>>,
    partial: Option<&PartialSender>,
) -> Result<ScanReport, ScanError> {
    let nodes = targets
        .nodes
        .iter_mut()
        .map(|t| NodeIn {
            meta: NodeMeta {
                id: t.id.clone(),
                endpoint: t.endpoint.clone(),
                role: t.role,
                primary_id: t.primary_id.clone(),
                slots: t.slots.clone(),
                note: t.note.clone(),
            },
            // Without a report, nothing optional is asked for (and MEMORY STATS is
            // tried, as for a standalone scan without one).
            perms: t.perms.clone().unwrap_or_default(),
            conn: t.conn.as_mut(),
        })
        .collect();
    scan_nodes(profile_name, nodes, opts, progress, partial).await
}

/// What the report says about a node, as discovery found it.
#[derive(Debug, Clone)]
pub(crate) struct NodeMeta {
    pub id: String,
    pub endpoint: Endpoint,
    pub role: NodeRole,
    pub primary_id: String,
    pub slots: Vec<(u16, u16)>,
    pub note: Option<String>,
}

/// One node to scan: what discovery found, its permission report and its connection
/// (`None` when it could not be connected).
pub(crate) struct NodeIn<'c, C> {
    pub meta: NodeMeta,
    pub perms: PermissionReport,
    pub conn: Option<&'c mut C>,
}

/// Split `limit` samples across nodes in proportion to their `sizes` (`DBSIZE`):
/// largest remainder, so the total is exactly `limit` when any node has keys (0 when
/// none has). Every node with keys gets at least one sample when `limit` allows it
/// (taken from the node with the largest share); when `limit` is smaller than the
/// number of such nodes, the largest nodes get one each. A node without keys gets 0.
pub(crate) fn split_sample_limit(limit: u64, sizes: &[u64]) -> Vec<u64> {
    let total: u128 = sizes.iter().map(|s| u128::from(*s)).sum();
    let mut out = vec![0u64; sizes.len()];
    if total == 0 || limit == 0 {
        return out;
    }
    // Largest first, ties by position.
    let mut order: Vec<usize> = (0..sizes.len()).filter(|i| sizes[*i] > 0).collect();
    order.sort_by(|a, b| sizes[*b].cmp(&sizes[*a]).then(a.cmp(b)));
    if limit <= order.len() as u64 {
        for i in order.into_iter().take(limit as usize) {
            out[i] = 1;
        }
        return out;
    }

    let mut rems: Vec<(u128, usize)> = Vec::with_capacity(sizes.len());
    let mut given: u64 = 0;
    for (i, s) in sizes.iter().enumerate() {
        let exact = u128::from(limit) * u128::from(*s);
        // At most `limit`, so it fits.
        out[i] = (exact / total) as u64;
        given += out[i];
        rems.push((exact % total, i));
    }
    rems.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(sizes[b.1].cmp(&sizes[a.1]))
            .then(a.1.cmp(&b.1))
    });
    for (_, i) in rems.into_iter().take((limit - given) as usize) {
        out[i] += 1;
    }
    // At least one each: there is always a node with two or more to take from, since
    // `limit` is larger than the number of nodes with keys.
    for &i in order.iter().rev() {
        if out[i] == 0 {
            let donor = (0..out.len())
                .max_by(|a, b| out[*a].cmp(&out[*b]).then(b.cmp(a)))
                .expect("a node with keys");
            out[donor] -= 1;
            out[i] = 1;
        }
    }
    out
}

/// Whether the slot ranges cover every slot of a cluster.
fn covers_all_slots<'a>(ranges: impl Iterator<Item = &'a (u16, u16)>) -> bool {
    let mut ranges: Vec<(u16, u16)> = ranges.copied().collect();
    ranges.sort_unstable();
    let mut next: u32 = 0;
    for (a, b) in ranges {
        if u32::from(a) > next {
            return false;
        }
        next = next.max(u32::from(b) + 1);
    }
    next >= SLOTS
}

/// The merged partial patterns while the nodes are sampled: each node's samples so far
/// (copied as they arrive) and keys present, aggregated as the final report is.
struct ClusterPartial<'a> {
    tx: &'a PartialSender,
    samples: Vec<Vec<KeySample>>,
    present: Vec<u64>,
    totals: Vec<u64>,
    has_match: bool,
    sizes_available: bool,
    target: u64,
    throttle: Throttle,
}

impl ClusterPartial<'_> {
    fn update(&mut self, node: usize, samples: &[KeySample], keys_present: u64, now: Instant) {
        let have = self.samples[node].len();
        if samples.len() > have {
            self.samples[node].extend(samples[have..].iter().cloned().map(|mut s| {
                s.node = Some(node_index(node));
                s
            }));
        }
        self.present[node] = keys_present;
        let sampled: usize = self.samples.iter().map(Vec::len).sum();
        if sampled == 0 || !self.throttle.ready(now) {
            return;
        }
        // No node has finished its walk as far as these figures know.
        let scales: Vec<f64> = (0..self.samples.len())
            .map(|i| estimate_scale(self.totals[i], self.present[i], false, self.has_match))
            .collect();
        let views: Vec<&[KeySample]> = self.samples.iter().map(Vec::as_slice).collect();
        let (patterns, _) = crate::pattern::aggregate_nodes(&views, &scales);
        self.publish(patterns, sampled as u64);
        self.throttle.record(now, Instant::now());
    }

    fn publish(&self, patterns: Vec<crate::model::PatternStats>, sampled: u64) {
        self.tx
            .send_replace(Some(std::sync::Arc::new(PartialPatterns {
                patterns,
                sampled,
                target: self.target,
                sizes_available: self.sizes_available,
            })));
    }
}

/// One node's view of the shared [`ClusterPartial`].
struct NodeHook<'s, 'a> {
    node: usize,
    shared: &'s Mutex<ClusterPartial<'a>>,
}

impl PartialHook for NodeHook<'_, '_> {
    fn after_batch(&mut self, samples: &[KeySample], keys_present: u64, now: Instant) {
        if let Ok(mut p) = self.shared.lock() {
            p.update(self.node, samples, keys_present, now);
        }
    }
}

/// [`KeySample::node`] for the node at `i`.
fn node_index(i: usize) -> u16 {
    u16::try_from(i).unwrap_or(u16::MAX)
}

/// The top hot keys across nodes, each tagged with its node's index.
pub(crate) fn cluster_hot_keys(per_node: &[Vec<KeySample>]) -> Vec<KeySample> {
    let all: Vec<KeySample> = per_node
        .iter()
        .enumerate()
        .flat_map(|(i, samples)| {
            samples.iter().cloned().map(move |mut s| {
                s.node = Some(node_index(i));
                s
            })
        })
        .collect();
    hot_keys(&all)
}

/// How a node's scan went.
struct NodeOut {
    /// The node had a connection.
    connected: bool,
    info: Option<ServerInfo>,
    sample: Option<ServerSample>,
}

/// [`scan_cluster`] over any connection type, so tests can use fake servers.
pub(crate) async fn scan_nodes<C: ConnectionLike + Send>(
    profile_name: &str,
    mut nodes: Vec<NodeIn<'_, C>>,
    opts: &ScanOptions,
    progress: Option<&watch::Sender<ScanProgress>>,
    partial: Option<&PartialSender>,
) -> Result<ScanReport, ScanError> {
    let has_match = opts.match_glob.is_some();

    // Phase 1: INFO and DBSIZE of every connected node.
    // The per-node futures are boxed as `Send` and collected before they are streamed:
    // a stream that holds iterator closures trips rustc's higher-ranked `Send` check when
    // the caller is spawned (see `tests::futures_are_send`).
    let reads: Vec<BoxFuture<'_, Option<ServerInfo>>> = nodes
        .iter_mut()
        .map(|n| {
            async move {
                match n.conn.as_deref_mut() {
                    Some(c) => server_info(c).await.ok(),
                    None => None,
                }
            }
            .boxed()
        })
        .collect();
    let infos: Vec<Option<ServerInfo>> = stream::iter(reads)
        .buffered(MAX_CONCURRENT_NODES)
        .collect()
        .await;
    if infos.iter().all(Option::is_none) {
        return Err(ScanError::NoNodes);
    }

    // Phase 2: sample, the limit split by DBSIZE.
    let sizes: Vec<u64> = infos
        .iter()
        .map(|i| i.as_ref().map_or(0, |i| i.total_keys))
        .collect();
    let quotas = split_sample_limit(opts.sample_limit, &sizes);
    let target: u64 = quotas.iter().zip(&sizes).map(|(q, s)| (*q).min(*s)).sum();
    if let Some(tx) = progress {
        tx.send_replace(ScanProgress {
            sampled: 0,
            target,
            done: false,
        });
    }
    let counts: Vec<AtomicU64> = nodes.iter().map(|_| AtomicU64::new(0)).collect();
    let sizes_available = nodes
        .iter()
        .zip(&infos)
        .filter(|(_, i)| i.is_some())
        .all(|(n, _)| n.perms.allowed(PERM_MEMORY));
    let shared = partial.map(|tx| {
        Mutex::new(ClusterPartial {
            tx,
            samples: vec![Vec::new(); nodes.len()],
            present: vec![0; nodes.len()],
            totals: sizes.clone(),
            has_match,
            sizes_available,
            target,
            throttle: Throttle::default(),
        })
    });

    let jobs = nodes
        .iter_mut()
        .zip(&infos)
        .zip(&quotas)
        .enumerate()
        .filter_map(|(i, ((n, info), quota))| {
            let info = info.as_ref()?;
            let conn = n.conn.as_deref_mut()?;
            Some((i, conn, info, *quota, &n.perms))
        })
        .map(|(i, conn, info, quota, perms)| {
            let counts = &counts;
            let shared = shared.as_ref();
            async move {
                let node_opts = ScanOptions {
                    sample_limit: quota,
                    ..opts.clone()
                };
                let report = move |sampled: u64, _done: bool| {
                    counts[i].store(sampled, Ordering::Relaxed);
                    if let Some(tx) = progress {
                        let sum = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
                        tx.send_modify(|p| p.sampled = sum);
                    }
                };
                let mut hook = shared.map(|shared| NodeHook { node: i, shared });
                let sample = sample_server(
                    conn,
                    info,
                    &node_opts,
                    perms,
                    &|| {},
                    Some(&report),
                    hook.as_mut().map(|h| h as &mut dyn PartialHook),
                )
                .await;
                (i, sample)
            }
            .boxed()
        })
        .collect::<Vec<BoxFuture<'_, (usize, ServerSample)>>>();
    let mut samples: Vec<Option<ServerSample>> = (0..infos.len()).map(|_| None).collect();
    let done: Vec<(usize, ServerSample)> = stream::iter(jobs)
        .buffer_unordered(MAX_CONCURRENT_NODES)
        .collect()
        .await;
    for (i, s) in done {
        samples[i] = Some(s);
    }
    let outs: Vec<NodeOut> = infos
        .into_iter()
        .zip(samples)
        .zip(nodes.iter().map(|n| n.conn.is_some()))
        .map(|((info, sample), connected)| NodeOut {
            connected,
            info,
            sample,
        })
        .collect();
    let metas: Vec<NodeMeta> = nodes.into_iter().map(|n| n.meta).collect();

    let report = merge(profile_name, metas, outs, &quotas, opts);
    if let Some(tx) = progress {
        tx.send_modify(|p| {
            p.sampled = report.sampled_keys;
            p.done = true;
        });
    }
    if let Some(shared) = &shared
        && let Ok(p) = shared.lock()
    {
        p.publish(report.patterns.clone(), report.sampled_keys);
    }
    Ok(report)
}

fn join_notes(a: Option<String>, b: &str) -> Option<String> {
    Some(match a {
        Some(a) => format!("{a}; {b}"),
        None => b.to_string(),
    })
}

/// Build the report from each node's outcome. At least one node has `info`.
fn merge(
    profile_name: &str,
    metas: Vec<NodeMeta>,
    outs: Vec<NodeOut>,
    quotas: &[u64],
    opts: &ScanOptions,
) -> ScanReport {
    let has_match = opts.match_glob.is_some();
    let mut node_samples: Vec<Vec<KeySample>> = Vec::with_capacity(outs.len());
    let mut scales: Vec<f64> = Vec::with_capacity(outs.len());
    let mut reports: Vec<NodeReport> = Vec::with_capacity(outs.len());
    let mut contexts: Vec<ServerContext> = Vec::new();
    let mut scanned_infos: Vec<ServerInfo> = Vec::new();
    let mut all_complete = true;
    let mut sizes_available = true;

    for (i, (meta, out)) in metas.into_iter().zip(outs).enumerate() {
        let (status, note, sample) = match (out.connected, &out.info, out.sample) {
            (false, _, _) => (
                NodeStatus::Failed,
                meta.note
                    .clone()
                    .or_else(|| Some(NOTE_NOT_CONNECTED.into())),
                None,
            ),
            (true, None, _) | (true, Some(_), None) => (
                NodeStatus::Failed,
                join_notes(meta.note.clone(), NOTE_INFO_FAILED),
                None,
            ),
            (true, Some(_), Some(s)) if s.error.is_some() => (
                NodeStatus::Partial,
                join_notes(meta.note.clone(), NOTE_STOPPED),
                Some(s),
            ),
            (true, Some(_), Some(s)) => (NodeStatus::Scanned, meta.note.clone(), Some(s)),
        };
        let mut samples: Vec<KeySample> = Vec::new();
        let mut scale = 1.0;
        let mut complete = false;
        let mut with_sizes = false;
        if let (Some(info), Some(s)) = (&out.info, sample) {
            scale = s.scale(info.total_keys, has_match);
            // A node whose DBSIZE was 0 gets no samples and no SCAN: DBSIZE said
            // there was nothing to see.
            let empty = info.total_keys == 0 && quotas.get(i) == Some(&0);
            complete = status == NodeStatus::Scanned && !has_match && (s.outcome.complete || empty);
            with_sizes = s.with_sizes;
            sizes_available &= with_sizes;
            samples = s.outcome.samples;
            for k in &mut samples {
                k.node = Some(node_index(i));
            }
            contexts.push(s.context);
            scanned_infos.push(info.clone());
        }
        // A node not looked at completely, or never looked at: the cluster wasn't.
        all_complete &= complete;
        reports.push(NodeReport {
            id: meta.id,
            endpoint: meta.endpoint,
            role: meta.role,
            primary_id: meta.primary_id,
            slots: meta.slots,
            server: out.info,
            status,
            note,
            sampled_keys: samples.len() as u64,
            complete,
            sizes_available: with_sizes,
            patterns: Vec::new(),
        });
        node_samples.push(samples);
        scales.push(scale);
    }

    let views: Vec<&[KeySample]> = node_samples.iter().map(Vec::as_slice).collect();
    let (patterns, per_node) = crate::pattern::aggregate_nodes(&views, &scales);
    for (r, rows) in reports.iter_mut().zip(per_node) {
        r.patterns = rows;
    }
    let complete = all_complete && covers_all_slots(reports.iter().flat_map(|r| &r.slots));
    let server = merged_server(&scanned_infos);
    let refs: Vec<&ServerContext> = contexts.iter().collect();
    let taken_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    ScanReport {
        profile: profile_name.to_string(),
        taken_at_unix,
        server,
        sampled_keys: node_samples.iter().map(|s| s.len() as u64).sum(),
        patterns,
        sizes_available,
        hot_keys: cluster_hot_keys(&node_samples),
        complete,
        match_glob: opts.match_glob.clone(),
        server_context: Some(merge_contexts(&refs)),
        nodes: Some(reports),
    }
}

/// The cluster's [`ServerInfo`]: the flavor, version and policy of the first node
/// scanned (in slot order), `mode: Cluster`, and `used_memory`, `maxmemory` and
/// `total_keys` summed over the nodes scanned.
fn merged_server(infos: &[ServerInfo]) -> ServerInfo {
    let first = infos.first();
    ServerInfo {
        flavor: first.map_or(crate::model::ServerFlavor::Unknown, |i| i.flavor),
        version: first.map(|i| i.version.clone()).unwrap_or_default(),
        mode: ServerMode::Cluster,
        used_memory: infos
            .iter()
            .map(|i| i.used_memory)
            .fold(0, u64::saturating_add),
        maxmemory: infos
            .iter()
            .map(|i| i.maxmemory)
            .fold(0, u64::saturating_add),
        maxmemory_policy: first
            .map(|i| i.maxmemory_policy.clone())
            .unwrap_or_default(),
        total_keys: infos
            .iter()
            .map(|i| i.total_keys)
            .fold(0, u64::saturating_add),
    }
}

/// `Some(f(values))` when every item has a value, `None` otherwise (or with no item).
fn all_some<T: Copy, R>(items: &[Option<T>], f: impl FnOnce(&[T]) -> R) -> Option<R> {
    let values: Option<Vec<T>> = items.iter().copied().collect();
    values.filter(|v| !v.is_empty()).map(|v| f(&v))
}

fn sum(values: &[u64]) -> u64 {
    values.iter().copied().fold(0, u64::saturating_add)
}

/// The server context of a cluster, from the nodes scanned. With one node it is that
/// node's own. Otherwise each part is merged only when every node has it (a sum over
/// some of the shards would read as the whole cluster):
/// - `INFO keyspace`: keys, expires and subexpiry summed; `avg_ttl` averaged weighted
///   by each node's `expires`.
/// - `MEMORY STATS`: `dataset.bytes`, `overhead.total` and `keys.count` summed;
///   `keys.bytes-per-key` averaged weighted by `keys.count`; `fragmentation` left out
///   (a ratio per process, which no sum or average over nodes describes).
/// - `INFO keysizes`: histograms of the same type and unit summed bin by bin, in the
///   order first seen.
/// - The reasons for missing parts: every node's, each once, in the order first seen.
pub(crate) fn merge_contexts(contexts: &[&ServerContext]) -> ServerContext {
    if let [one] = contexts {
        return (*one).clone();
    }
    let mut parts = ServerContextParts::default();

    let keyspaces: Vec<Option<KeyspaceInfo>> = contexts.iter().map(|c| c.keyspace).collect();
    parts.keyspace = all_some(&keyspaces, |ks| {
        let expires = sum(&ks.iter().map(|k| k.expires).collect::<Vec<_>>());
        let weighted: u128 = ks
            .iter()
            .map(|k| u128::from(k.avg_ttl_ms) * u128::from(k.expires))
            .sum();
        let subs: Vec<Option<u64>> = ks.iter().map(|k| k.subexpiry).collect();
        KeyspaceInfo {
            db: ks[0].db,
            keys: sum(&ks.iter().map(|k| k.keys).collect::<Vec<_>>()),
            expires,
            avg_ttl_ms: if expires == 0 {
                0
            } else {
                u64::try_from(weighted / u128::from(expires)).unwrap_or(u64::MAX)
            },
            subexpiry: all_some(&subs, sum),
        }
    });

    let memories: Vec<Option<MemoryStats>> = contexts.iter().map(|c| c.memory).collect();
    parts.memory = all_some(&memories, |ms| {
        let field = |f: fn(&MemoryStats) -> Option<u64>| {
            all_some(&ms.iter().map(f).collect::<Vec<_>>(), sum)
        };
        let keys_count = field(|m| m.keys_count);
        let pairs: Vec<Option<(u64, u64)>> = ms
            .iter()
            .map(|m| Some((m.bytes_per_key?, m.keys_count?)))
            .collect();
        let bytes_per_key = all_some(&pairs, |p| {
            let keys: u128 = p.iter().map(|(_, k)| u128::from(*k)).sum();
            let bytes: u128 = p.iter().map(|(b, k)| u128::from(*b) * u128::from(*k)).sum();
            (keys > 0).then(|| u64::try_from(bytes / keys).unwrap_or(u64::MAX))
        })
        .flatten();
        MemoryStats {
            dataset_bytes: field(|m| m.dataset_bytes),
            overhead_total: field(|m| m.overhead_total),
            keys_count,
            bytes_per_key,
            fragmentation: None,
        }
    });

    if contexts.iter().all(|c| c.keysizes.is_some()) && !contexts.is_empty() {
        let mut merged: Vec<KeysizeHistogram> = Vec::new();
        for h in contexts.iter().flat_map(|c| c.keysizes.iter().flatten()) {
            let pos = merged
                .iter()
                .position(|m| m.key_type == h.key_type && m.unit == h.unit);
            let target = match pos {
                Some(p) => &mut merged[p],
                None => {
                    merged.push(KeysizeHistogram {
                        key_type: h.key_type.clone(),
                        unit: h.unit.clone(),
                        bins: Vec::new(),
                    });
                    merged.last_mut().expect("just pushed")
                }
            };
            for (bound, n) in &h.bins {
                match target.bins.iter_mut().find(|(b, _)| b == bound) {
                    Some(bin) => bin.1 = bin.1.saturating_add(*n),
                    None => target.bins.push((*bound, *n)),
                }
            }
            target.bins.sort_unstable();
        }
        parts.keysizes = Some(merged);
    }

    for c in contexts {
        for gap in &c.missing {
            if !parts.missing.contains(gap) {
                parts.missing.push(*gap);
            }
        }
    }
    parts.into()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::tests::fake::{self, Fate};
    use super::*;
    use crate::model::{
        ContextGap, KeyType, PERM_FREQ, PERM_INFO, PERM_SCAN, PERM_TYPE_TTL, PermCheck, PermStatus,
        Ttl,
    };
    use proptest::prelude::*;

    /// Compile-time check, never run: connecting to, checking and scanning whatever
    /// `connect_targets` returns can go in `tokio::spawn`, the way the TUI runs it. Fails
    /// to build (rustc's "implementation of `Send` is not general enough") when a
    /// per-node future in a `buffered`/`buffer_unordered` stream is left unboxed.
    #[allow(dead_code)]
    fn futures_are_send(
        profile: crate::model::Profile,
        store: Box<dyn crate::secrets::SecretStore>,
        opts: ScanOptions,
        progress: watch::Sender<ScanProgress>,
        partial: PartialSender,
    ) {
        drop(tokio::spawn(async move {
            let Ok(mut targets) =
                crate::conn::cluster::connect_targets(&profile, store.as_ref()).await
            else {
                return;
            };
            let perms = targets.check_permissions().await;
            let _ = scan_targets(
                &profile.name,
                &mut targets,
                &opts,
                &perms,
                Some(&progress),
                Some(&partial),
            )
            .await;
        }));
    }

    fn perms_all() -> PermissionReport {
        PermissionReport {
            checks: [PERM_SCAN, PERM_TYPE_TTL, PERM_MEMORY, PERM_FREQ, PERM_INFO]
                .into_iter()
                .map(|label| PermCheck {
                    label,
                    status: PermStatus::Allowed,
                })
                .collect(),
        }
    }

    fn opts(sample_limit: u64, batch_size: usize) -> ScanOptions {
        ScanOptions {
            sample_limit,
            batch_size,
            max_commands_per_sec: None,
            ..ScanOptions::default()
        }
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// Keys `prefix:i` for `i` in `range`, every one present.
    fn keys(prefixes: &[&str], range: std::ops::Range<usize>) -> Vec<(Vec<u8>, Fate)> {
        range
            .map(|i| {
                let p = prefixes[i % prefixes.len()];
                (format!("{p}:{i}").into_bytes(), Fate::Present)
            })
            .collect()
    }

    fn meta(i: usize, slots: Vec<(u16, u16)>) -> NodeMeta {
        NodeMeta {
            id: format!("node{i}"),
            endpoint: Endpoint {
                host: "10.0.0.1".into(),
                port: 7000 + i as u16,
                tls: false,
            },
            role: NodeRole::Primary,
            primary_id: format!("node{i}"),
            slots,
            note: None,
        }
    }

    /// Three shards over the whole slot space.
    fn thirds() -> [Vec<(u16, u16)>; 3] {
        [vec![(0, 5460)], vec![(5461, 10922)], vec![(10923, 16383)]]
    }

    fn inputs(servers: &mut [fake::Server]) -> Vec<NodeIn<'_, fake::Server>> {
        let slots = thirds();
        servers
            .iter_mut()
            .enumerate()
            .map(|(i, s)| NodeIn {
                meta: meta(i, slots[i % 3].clone()),
                perms: perms_all(),
                conn: Some(s),
            })
            .collect()
    }

    fn est_total(rows: &[crate::model::PatternStats]) -> u64 {
        rows.iter().map(|p| p.est_keys).sum()
    }

    #[test]
    fn complete_scan_of_every_shard() {
        let mut servers = vec![
            fake::Server::new(keys(&["user", "cart"], 0..300), 50),
            fake::Server::new(keys(&["user", "cart"], 300..400), 50),
            fake::Server::new(keys(&["user", "sess"], 400..1000), 50),
        ];
        let r = run(scan_nodes(
            "c",
            inputs(&mut servers),
            &opts(100_000, 64),
            None,
            None,
        ))
        .unwrap();
        assert!(r.complete);
        assert_eq!(r.server.mode, ServerMode::Cluster);
        assert_eq!(r.server.total_keys, 1_000);
        assert_eq!(r.sampled_keys, 1_000);
        assert_eq!(est_total(&r.patterns), 1_000);
        let nodes = r.nodes.as_ref().unwrap();
        for (n, want) in nodes.iter().zip([300, 100, 600]) {
            assert_eq!(n.status, NodeStatus::Scanned);
            assert!(n.complete);
            assert_eq!(n.sampled_keys, want);
            assert_eq!(n.server.as_ref().unwrap().total_keys, want);
            assert_eq!(est_total(&n.patterns), want);
            assert!(n.patterns.iter().all(|p| p.examples.is_empty()));
        }
        // INFO memory is summed too.
        let one = nodes[0].server.as_ref().unwrap().used_memory;
        assert_eq!(r.server.used_memory, 3 * one);
        // The merged rows keep examples, each from its own node.
        let user = r
            .patterns
            .iter()
            .find(|p| p.pattern == "user:{id}")
            .unwrap();
        assert_eq!(user.examples.len(), 5);
        assert!(user.examples.iter().all(|e| e.node.is_some()));
        // Only read commands, and never KEYS.
        for s in &servers {
            assert!(s.log.iter().all(|c| c != "KEYS"));
        }
    }

    /// Each node's rows sum to the merged rows, for every pattern and figure that
    /// adds up.
    fn assert_rows_add_up(r: &ScanReport) {
        let nodes = r.nodes.as_ref().unwrap();
        for m in &r.patterns {
            let rows: Vec<_> = nodes
                .iter()
                .filter_map(|n| n.patterns.iter().find(|p| p.pattern == m.pattern))
                .collect();
            assert!(!rows.is_empty(), "{}", m.pattern);
            let s =
                |f: fn(&crate::model::PatternStats) -> u64| rows.iter().map(|p| f(p)).sum::<u64>();
            assert_eq!(s(|p| p.sampled_keys), m.sampled_keys, "{}", m.pattern);
            assert_eq!(s(|p| p.est_keys), m.est_keys, "{}", m.pattern);
            assert_eq!(s(|p| p.no_ttl), m.no_ttl, "{}", m.pattern);
            assert_eq!(
                s(|p| p.est_bytes.unwrap_or(0)),
                m.est_bytes.unwrap_or(0),
                "{}",
                m.pattern
            );
            for b in 0..7 {
                let hist: u64 = rows.iter().map(|p| p.ttl_hist[b]).sum();
                assert_eq!(hist, m.ttl_hist[b]);
            }
        }
        // And no node has a pattern the merged rows lack.
        for n in nodes {
            for p in &n.patterns {
                assert!(r.patterns.iter().any(|m| m.pattern == p.pattern));
            }
        }
    }

    #[test]
    fn sampled_shards_scale_by_their_own_dbsize() {
        // 3,000, 1,000 and 6,000 keys, 1,000 samples split 300/100/600: each node is
        // scaled to its own DBSIZE, and the merged estimate is the cluster's.
        let mut servers = vec![
            fake::Server::new(keys(&["user", "cart", "sess"], 0..3_000), 100),
            fake::Server::new(keys(&["user", "cart", "sess"], 3_000..4_000), 100),
            fake::Server::new(keys(&["user", "cart", "sess"], 4_000..10_000), 100),
        ];
        let r = run(scan_nodes(
            "c",
            inputs(&mut servers),
            &opts(1_000, 50),
            None,
            None,
        ))
        .unwrap();
        assert!(!r.complete);
        let nodes = r.nodes.as_ref().unwrap();
        let sampled: Vec<u64> = nodes.iter().map(|n| n.sampled_keys).collect();
        assert_eq!(sampled, [300, 100, 600]);
        for (n, want) in nodes.iter().zip([3_000u64, 1_000, 6_000]) {
            assert!(est_total(&n.patterns).abs_diff(want) <= 3);
            assert!(!n.complete);
        }
        assert!(est_total(&r.patterns).abs_diff(10_000) <= 9);
        assert_rows_add_up(&r);
    }

    #[test]
    fn var_collapse_sees_every_node() {
        // 30 tenants, 10 per node: no node alone reaches the collapse threshold of 20,
        // the union does, and every node's rows use the merged pattern.
        let tenant_keys = |range: std::ops::Range<usize>| {
            range
                .map(|i| {
                    (
                        format!("cfg:tenant-{i:04}x:flags").into_bytes(),
                        Fate::Present,
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut servers = vec![
            fake::Server::new(tenant_keys(0..10), 50),
            fake::Server::new(tenant_keys(10..20), 50),
            fake::Server::new(tenant_keys(20..30), 50),
        ];
        let r = run(scan_nodes(
            "c",
            inputs(&mut servers),
            &opts(1_000, 50),
            None,
            None,
        ))
        .unwrap();
        assert_eq!(r.patterns.len(), 1);
        assert_eq!(r.patterns[0].pattern, "cfg:{var}:flags");
        for n in r.nodes.as_ref().unwrap() {
            assert_eq!(n.patterns.len(), 1);
            assert_eq!(n.patterns[0].pattern, "cfg:{var}:flags");
            assert_eq!(n.patterns[0].sampled_keys, 10);
        }
        assert_rows_add_up(&r);
    }

    #[test]
    fn failures_before_and_during_sampling() {
        let mut a = fake::Server::new(keys(&["user"], 0..500), 100);
        let mut b = fake::Server::new(keys(&["user"], 500..1_000), 100);
        b.fail_scan = Some(2);
        let mut c = fake::Server::new(keys(&["user"], 1_000..1_500), 100);
        c.fail_info = true;
        let slots = thirds();
        let mut nodes = vec![
            NodeIn {
                meta: meta(0, slots[0].clone()),
                perms: perms_all(),
                conn: Some(&mut a),
            },
            NodeIn {
                meta: meta(1, slots[1].clone()),
                perms: perms_all(),
                conn: Some(&mut b),
            },
            NodeIn {
                meta: meta(2, slots[2].clone()),
                perms: perms_all(),
                conn: Some(&mut c),
            },
            NodeIn {
                meta: NodeMeta {
                    note: Some("10.0.0.1:7003: could not connect".into()),
                    ..meta(3, vec![])
                },
                perms: perms_all(),
                conn: None,
            },
        ];
        nodes.push(NodeIn {
            meta: meta(4, vec![]),
            perms: perms_all(),
            conn: None,
        });
        let r = run(scan_nodes("c", nodes, &opts(10_000, 50), None, None)).unwrap();
        let n = r.nodes.as_ref().unwrap();
        assert_eq!(n[0].status, NodeStatus::Scanned);
        assert_eq!(n[0].sampled_keys, 500);
        // Two SCAN pages (200 keys) were sampled before the third failed: kept.
        assert_eq!(n[1].status, NodeStatus::Partial);
        assert_eq!(n[1].sampled_keys, 200);
        assert!(!n[1].complete);
        assert_eq!(n[1].note.as_deref(), Some(NOTE_STOPPED));
        assert_eq!(n[2].status, NodeStatus::Failed);
        assert_eq!(n[2].server, None);
        assert_eq!(n[2].note.as_deref(), Some(NOTE_INFO_FAILED));
        assert!(n[2].patterns.is_empty());
        assert_eq!(n[3].status, NodeStatus::Failed);
        assert_eq!(
            n[3].note.as_deref(),
            Some("10.0.0.1:7003: could not connect")
        );
        assert_eq!(n[4].note.as_deref(), Some(NOTE_NOT_CONNECTED));
        // The scanned and partial nodes are merged; the cluster is not complete.
        assert!(!r.complete);
        assert_eq!(r.sampled_keys, 700);
        assert_eq!(r.server.total_keys, 1_000);
        assert_rows_add_up(&r);
    }

    #[test]
    fn no_scannable_node_is_an_error() {
        let mut a = fake::Server::new(keys(&["user"], 0..10), 100);
        a.fail_info = true;
        let nodes = vec![
            NodeIn {
                meta: meta(0, vec![(0, 16383)]),
                perms: perms_all(),
                conn: Some(&mut a),
            },
            NodeIn {
                meta: meta(1, vec![]),
                perms: perms_all(),
                conn: None,
            },
        ];
        let e = run(scan_nodes("c", nodes, &opts(100, 50), None, None)).unwrap_err();
        assert!(matches!(e, ScanError::NoNodes), "{e}");
    }

    #[test]
    fn complete_needs_every_slot_covered() {
        // The only node seen completely, but it serves part of the slots (or the
        // shards could not be read at all): not the whole cluster.
        for slots in [vec![(0, 5460)], vec![]] {
            let mut a = fake::Server::new(keys(&["user"], 0..10), 100);
            let nodes = vec![NodeIn {
                meta: meta(0, slots),
                perms: perms_all(),
                conn: Some(&mut a),
            }];
            let r = run(scan_nodes("c", nodes, &opts(100, 50), None, None)).unwrap();
            assert!(r.nodes.as_ref().unwrap()[0].complete);
            assert!(!r.complete);
        }
        assert!(covers_all_slots([(0, 100), (101, 16383)].iter()));
        assert!(covers_all_slots([(101, 16383), (0, 100)].iter()));
        assert!(!covers_all_slots([(0, 100), (102, 16383)].iter()));
        assert!(!covers_all_slots([(0, 16382)].iter()));
        assert!(!covers_all_slots(std::iter::empty()));
    }

    #[test]
    fn an_empty_shard_is_complete_without_a_scan() {
        let mut servers = vec![
            fake::Server::new(keys(&["user"], 0..100), 100),
            fake::Server::new(Vec::new(), 100),
            fake::Server::new(keys(&["user"], 100..200), 100),
        ];
        let r = run(scan_nodes(
            "c",
            inputs(&mut servers),
            &opts(1_000, 50),
            None,
            None,
        ))
        .unwrap();
        assert!(r.complete);
        assert!(
            !servers[1].log.iter().any(|c| c == "SCAN"),
            "{:?}",
            servers[1].log
        );
        assert_eq!(r.sampled_keys, 200);
    }

    /// Sizes need every scanned node: one node denied `MEMORY USAGE` turns them off for
    /// the cluster, while its own row says which node it was.
    #[test]
    fn one_node_without_memory_usage_turns_sizes_off() {
        let mut servers = vec![
            fake::Server::new(keys(&["user"], 0..100), 100),
            fake::Server::new(keys(&["user"], 100..200), 100),
            fake::Server::new(keys(&["user"], 200..300), 100),
        ];
        let all = run(scan_nodes(
            "c",
            inputs(&mut servers),
            &opts(1_000, 50),
            None,
            None,
        ))
        .unwrap();
        assert!(all.sizes_available);

        let mut nodes = inputs(&mut servers);
        nodes[1].perms.checks.retain(|c| c.label != PERM_MEMORY);
        nodes[1].perms.checks.push(PermCheck {
            label: PERM_MEMORY,
            status: PermStatus::Denied("NOPERM".into()),
        });
        let r = run(scan_nodes("c", nodes, &opts(1_000, 50), None, None)).unwrap();
        assert!(!r.sizes_available);
        let rows = r.nodes.unwrap();
        let sizes: Vec<bool> = rows.iter().map(|n| n.sizes_available).collect();
        assert_eq!(sizes, [true, false, true]);
    }

    #[test]
    fn match_never_makes_a_cluster_complete() {
        let mut servers = vec![fake::Server::new(keys(&["user"], 0..10), 100)];
        let mut nodes = inputs(&mut servers);
        nodes[0].meta.slots = vec![(0, 16383)];
        let o = ScanOptions {
            match_glob: Some("user:*".into()),
            ..opts(100, 50)
        };
        let r = run(scan_nodes("c", nodes, &o, None, None)).unwrap();
        assert!(!r.complete);
        assert_eq!(r.match_glob.as_deref(), Some("user:*"));
        assert!(servers[0].log.iter().any(|c| c == "SCAN"));
    }

    /// The command budget is per node: two nodes at 200 commands a second each finish
    /// in about the time one takes, not two.
    #[test]
    fn each_node_gets_its_own_limiter() {
        let rate = 200u32;
        let mut servers = vec![
            fake::Server::new(keys(&["user"], 0..120), 40),
            fake::Server::new(keys(&["user"], 120..240), 40),
        ];
        let mut nodes = inputs(&mut servers);
        nodes[0].meta.slots = vec![(0, 8000)];
        nodes[1].meta.slots = vec![(8001, 16383)];
        let o = ScanOptions {
            max_commands_per_sec: Some(rate),
            ..opts(1_000, 20)
        };
        let t0 = Instant::now();
        let r = run(scan_nodes("c", nodes, &o, None, None)).unwrap();
        let took = t0.elapsed();
        assert!(r.complete);
        // Commands paced by a limiter: the context reads and the walk (INFO server,
        // INFO memory and DBSIZE come before it).
        let paced = |s: &fake::Server| s.log.len() as u64 - 3;
        let per_node: Vec<u64> = servers.iter().map(paced).collect();
        assert!(per_node.iter().all(|n| *n > 20), "{per_node:?}");
        let one = Duration::from_secs_f64(per_node[0].max(per_node[1]) as f64 / f64::from(rate));
        let shared = Duration::from_secs_f64(per_node.iter().sum::<u64>() as f64 / f64::from(rate));
        // Each node keeps to its own budget...
        assert!(took >= one, "{took:?} < {one:?}");
        // ...and does not wait for the other's commands.
        assert!(
            took < shared.mul_f64(0.8),
            "{took:?}, one budget {shared:?}"
        );
    }

    #[test]
    fn progress_sums_the_nodes_and_partials_hold_merged_patterns() {
        let mut servers = vec![
            fake::Server::new(keys(&["user", "cart"], 0..400), 100),
            fake::Server::new(keys(&["user", "cart"], 400..1_000), 100),
        ];
        let mut nodes = inputs(&mut servers);
        nodes[0].meta.slots = vec![(0, 8000)];
        nodes[1].meta.slots = vec![(8001, 16383)];
        let (ptx, prx) = watch::channel(ScanProgress::default());
        let (tx, rx) = watch::channel(None);
        let r = run(scan_nodes(
            "c",
            nodes,
            &opts(500, 100),
            Some(&ptx),
            Some(&tx),
        ))
        .unwrap();
        let p = *prx.borrow();
        assert_eq!(p.target, 500);
        assert_eq!(p.sampled, 500);
        assert!(p.done);
        let last = rx.borrow().clone().unwrap();
        assert_eq!(last.patterns, r.patterns);
        assert_eq!(last.sampled, 500);
        assert_eq!(last.target, 500);
        assert!(last.sizes_available);
        // Partials send nothing to the servers.
        assert!(servers.iter().all(|s| !s.log.iter().any(|c| c == "KEYS")));
    }

    #[test]
    fn partials_are_published_while_nodes_are_sampled() {
        let (tx, rx) = watch::channel(None);
        let mut p = ClusterPartial {
            tx: &tx,
            samples: vec![Vec::new(), Vec::new()],
            present: vec![0, 0],
            totals: vec![1_000, 3_000],
            has_match: false,
            sizes_available: false,
            target: 400,
            throttle: Throttle::default(),
        };
        let s = |k: &str| KeySample {
            key: k.into(),
            key_type: KeyType::String,
            bytes: None,
            ttl: Ttl::Persistent,
            lfu_freq: None,
            node: None,
        };
        let t0 = Instant::now();
        let a: Vec<KeySample> = (0..10).map(|i| s(&format!("a:{i}"))).collect();
        p.update(0, &a, 10, t0);
        let first = rx.borrow().clone().unwrap();
        // 10 of 1,000 keys on node 0: scale 100.
        assert_eq!(first.patterns[0].est_keys, 1_000);
        assert_eq!(first.sampled, 10);
        // Too soon: node 1's samples are kept but not published yet.
        let b: Vec<KeySample> = (0..30).map(|i| s(&format!("b:{i}"))).collect();
        p.update(1, &b, 30, t0 + Duration::from_millis(10));
        assert_eq!(rx.borrow().as_ref().unwrap().sampled, 10);
        p.update(1, &b, 30, t0 + Duration::from_secs(5));
        let later = rx.borrow().clone().unwrap();
        assert_eq!(later.sampled, 40);
        let total: u64 = later.patterns.iter().map(|p| p.est_keys).sum();
        assert_eq!(total, 4_000);
        assert_eq!(p.samples[1].len(), 30, "samples are copied once");
        assert!(p.samples[1].iter().all(|k| k.node == Some(1)));
    }

    #[test]
    fn hot_keys_across_nodes_carry_their_node() {
        let s = |k: &str, f: u8| KeySample {
            key: k.into(),
            key_type: KeyType::String,
            bytes: None,
            ttl: Ttl::Persistent,
            lfu_freq: Some(f),
            node: None,
        };
        let a: Vec<KeySample> = (0..40).map(|i| s(&format!("a:{i:02}"), i as u8)).collect();
        let b: Vec<KeySample> = (0..40)
            .map(|i| s(&format!("b:{i:02}"), 100 + i as u8))
            .collect();
        let c = vec![s("c:0", 0), s("c:hot", 255)];
        let hot = cluster_hot_keys(&[a, b, c]);
        assert_eq!(hot.len(), 50);
        assert_eq!((hot[0].key.as_str(), hot[0].node), ("c:hot", Some(2)));
        assert_eq!((hot[1].key.as_str(), hot[1].node), ("b:39", Some(1)));
        // 1 + 40 from node 1, then the 9 hottest of node 0.
        assert_eq!((hot[49].key.as_str(), hot[49].node), ("a:31", Some(0)));
        assert!(hot.iter().all(|k| k.key != "c:0"));
    }

    fn ctx(parts: ServerContextParts) -> ServerContext {
        parts.into()
    }

    #[test]
    fn contexts_sum_what_adds_up() {
        let a = ctx(ServerContextParts {
            keyspace: Some(KeyspaceInfo {
                db: 0,
                keys: 100,
                expires: 10,
                avg_ttl_ms: 1_000,
                subexpiry: Some(1),
            }),
            memory: Some(MemoryStats {
                dataset_bytes: Some(1_000),
                overhead_total: Some(50),
                keys_count: Some(100),
                bytes_per_key: Some(10),
                fragmentation: Some(1.2),
            }),
            keysizes: Some(vec![KeysizeHistogram {
                key_type: "strings".into(),
                unit: "sizes".into(),
                bins: vec![(0, 1), (8, 5)],
            }]),
            missing: vec![],
        });
        let b = ctx(ServerContextParts {
            keyspace: Some(KeyspaceInfo {
                db: 0,
                keys: 300,
                expires: 30,
                avg_ttl_ms: 5_000,
                subexpiry: Some(2),
            }),
            memory: Some(MemoryStats {
                dataset_bytes: Some(3_000),
                overhead_total: Some(70),
                keys_count: Some(300),
                bytes_per_key: Some(20),
                fragmentation: Some(3.0),
            }),
            keysizes: Some(vec![
                KeysizeHistogram {
                    key_type: "strings".into(),
                    unit: "sizes".into(),
                    bins: vec![(8, 2), (16, 1)],
                },
                KeysizeHistogram {
                    key_type: "hashes".into(),
                    unit: "items".into(),
                    bins: vec![(1, 4)],
                },
            ]),
            missing: vec![],
        });
        let m = merge_contexts(&[&a, &b]);
        let ks = m.keyspace.unwrap();
        assert_eq!((ks.keys, ks.expires, ks.subexpiry), (400, 40, Some(3)));
        // Weighted by expires: (1,000 * 10 + 5,000 * 30) / 40.
        assert_eq!(ks.avg_ttl_ms, 4_000);
        let mem = m.memory.unwrap();
        assert_eq!(mem.dataset_bytes, Some(4_000));
        assert_eq!(mem.overhead_total, Some(120));
        assert_eq!(mem.keys_count, Some(400));
        // Weighted by keys: (10 * 100 + 20 * 300) / 400 = 17.5, rounded down.
        assert_eq!(mem.bytes_per_key, Some(17));
        assert_eq!(mem.fragmentation, None);
        let hists = m.keysizes.as_ref().unwrap();
        assert_eq!(hists.len(), 2);
        assert_eq!(hists[0].bins, vec![(0, 1), (8, 7), (16, 1)]);
        assert_eq!(hists[1].key_type, "hashes");
        assert!(m.missing.is_empty());
        // One node: its own context, untouched.
        assert_eq!(merge_contexts(&[&a]), a);
    }

    #[test]
    fn contexts_leave_out_what_some_node_lacks() {
        let full = ctx(ServerContextParts {
            keyspace: Some(KeyspaceInfo {
                db: 0,
                keys: 1,
                expires: 0,
                avg_ttl_ms: 0,
                subexpiry: Some(0),
            }),
            memory: Some(MemoryStats {
                dataset_bytes: Some(1),
                keys_count: Some(1),
                ..MemoryStats::default()
            }),
            keysizes: Some(vec![]),
            missing: vec![],
        });
        let lacking = ctx(ServerContextParts {
            keyspace: Some(KeyspaceInfo {
                db: 0,
                keys: 2,
                expires: 0,
                avg_ttl_ms: 0,
                subexpiry: None,
            }),
            memory: None,
            keysizes: None,
            missing: vec![ContextGap::NoKeysizes, ContextGap::MemoryStatsDenied],
        });
        let m = merge_contexts(&[&full, &lacking]);
        let ks = m.keyspace.unwrap();
        assert_eq!(ks.keys, 3);
        assert_eq!(ks.subexpiry, None);
        assert_eq!(m.memory, None);
        assert_eq!(m.keysizes, None);
        assert_eq!(
            m.missing,
            vec![ContextGap::NoKeysizes, ContextGap::MemoryStatsDenied]
        );
    }

    #[test]
    fn split_examples() {
        assert_eq!(
            split_sample_limit(1_000, &[3_000, 1_000, 6_000]),
            [300, 100, 600]
        );
        assert_eq!(
            split_sample_limit(100_000, &[300, 100, 600]),
            [30_000, 10_000, 60_000]
        );
        // Largest remainder: 10 over three equal shards.
        assert_eq!(split_sample_limit(10, &[5, 5, 5]).iter().sum::<u64>(), 10);
        // A small shard still gets one.
        assert_eq!(split_sample_limit(10, &[1_000_000, 1]), [9, 1]);
        // Fewer samples than shards: the largest get one.
        assert_eq!(split_sample_limit(2, &[5, 50, 0, 7]), [0, 1, 0, 1]);
        assert_eq!(split_sample_limit(0, &[5, 5]), [0, 0]);
        assert_eq!(split_sample_limit(100, &[0, 0]), [0, 0]);
        assert_eq!(split_sample_limit(100, &[]), Vec::<u64>::new());
    }

    proptest! {
        #[test]
        fn split_is_proportional_and_within_the_limit(
            limit in 0u64..2_000_000,
            sizes in proptest::collection::vec(prop_oneof![Just(0u64), 0u64..5, 0u64..10_000_000], 0..12),
        ) {
            let q = split_sample_limit(limit, &sizes);
            prop_assert_eq!(q.len(), sizes.len());
            let total: u64 = sizes.iter().sum();
            let given: u64 = q.iter().sum();
            prop_assert!(given <= limit);
            if total > 0 {
                prop_assert_eq!(given, limit);
            }
            let nonempty = sizes.iter().filter(|s| **s > 0).count() as u64;
            for (s, n) in sizes.iter().zip(&q) {
                if *s == 0 {
                    prop_assert_eq!(*n, 0);
                } else if limit >= nonempty {
                    prop_assert!(*n >= 1);
                }
                if total > 0 && limit >= nonempty {
                    // Within one of the exact share, plus what was moved to give every
                    // shard at least one.
                    let exact = limit as f64 * *s as f64 / total as f64;
                    prop_assert!((*n as f64 - exact).abs() < 1.0 + nonempty as f64, "{} vs {}", n, exact);
                }
                // A limit as large as the cluster lets every shard be seen whole.
                if limit >= total {
                    prop_assert!(*n >= *s);
                }
            }
        }
    }
}
