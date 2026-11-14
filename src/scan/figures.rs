//! The server's own figures, read on connect without reading a single key: `INFO
//! server`, `INFO memory` and `DBSIZE`, then the server context (`INFO keyspace`, `INFO
//! keysizes`, `MEMORY STATS`, see [`super::context`]). Six read-only commands, each
//! taken from the command budget before it is sent. Never `SCAN`, `TYPE`, `PTTL`,
//! `MEMORY USAGE` or `OBJECT FREQ`.
//!
//! Owner: scanner. A scan reads its figures through [`server_figures_with_budget`] too
//! (one code path), and so does every node of a cluster scan ([`read_nodes`]).
//!
//! **Degrading.** `INFO server`, `INFO memory` and `DBSIZE` are the ground everything
//! else stands on: when one of them fails the read fails with a [`ScanError`]
//! ([`failure_text`] gives a fixed sentence for it). Each part of the server context is
//! optional: a section the server doesn't have, a refused or unknown command, or a
//! reply that can't be read leaves that part `None` with a
//! [`crate::model::ContextGap`] saying why ([`ServerFigures::gaps`]).

use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use redis::aio::ConnectionLike;

use crate::budget::Budget;
use crate::conn::cluster::ClusterTargets;
use crate::model::{PermissionReport, ServerContext, ServerInfo};

use super::cluster::{MAX_CONCURRENT_NODES, merge_contexts, merged_server};
use super::{ScanError, context, server_info_with_budget};

/// The commands [`server_figures_with_budget`] sends at most: `INFO server`, `INFO
/// memory`, `DBSIZE`, `INFO keyspace`, `INFO keysizes` and `MEMORY STATS`.
pub const FIGURE_COMMANDS: u64 = 6;

/// Note for a cluster node that has no connection and no note of its own.
pub const NOTE_NOT_CONNECTED: &str = "this node could not be connected to";
/// Note for a cluster node whose `INFO` or `DBSIZE` could not be read.
pub const NOTE_FIGURES_FAILED: &str = "INFO or DBSIZE could not be read on this node";

/// What a server says about itself, without reading any key.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerFigures {
    /// `INFO server`, `INFO memory` and `DBSIZE`.
    pub server: ServerInfo,
    /// `INFO keyspace`, `INFO keysizes` and `MEMORY STATS`, each part optional, with
    /// the reasons for the parts that are missing in `missing`.
    pub context: ServerContext,
}

/// The Overview's "Keys" headline (`7.92M keys · 31% have a TTL · 13G of data, 2.1G
/// overhead`) and the big-key count, from [`ServerFigures`] alone. Each optional part
/// is `None` when the figure it comes from is missing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Headline {
    /// `DBSIZE` (summed over the nodes on a cluster).
    pub keys: u64,
    /// Keys with a TTL: `expires` of `INFO keyspace`.
    pub with_ttl: Option<u64>,
    /// `expires / keys` of `INFO keyspace`, 0.0 to 1.0. `None` without `INFO keyspace`
    /// or when it counts no key.
    pub ttl_share: Option<f64>,
    /// `dataset.bytes` of `MEMORY STATS`.
    pub dataset_bytes: Option<u64>,
    /// `overhead.total` of `MEMORY STATS`.
    pub overhead_bytes: Option<u64>,
    /// Keys `INFO keysizes` counts as big ([`context::big_keys_total`]: strings of 1 MiB
    /// or more, collections of 64K items or more). `None` on servers without the
    /// section (Redis before 8, Valkey).
    pub big_keys: Option<u64>,
}

impl ServerFigures {
    /// Why parts are missing, as fixed sentences ([`context::gap_text`]), in the order
    /// the parts are read. Never the server's own text.
    pub fn gaps(&self) -> Vec<&'static str> {
        self.context
            .missing
            .iter()
            .filter_map(|g| context::gap_text(*g))
            .collect()
    }

    /// The headline figures.
    pub fn headline(&self) -> Headline {
        let ks = self.context.keyspace;
        let memory = self.context.memory;
        Headline {
            keys: self.server.total_keys,
            with_ttl: ks.map(|k| k.expires),
            ttl_share: ks
                .filter(|k| k.keys > 0)
                .map(|k| (k.expires as f64 / k.keys as f64).min(1.0)),
            dataset_bytes: memory.and_then(|m| m.dataset_bytes),
            overhead_bytes: memory.and_then(|m| m.overhead_total),
            big_keys: self
                .context
                .keysizes
                .as_deref()
                .map(context::big_keys_total),
        }
    }
}

/// Read the figures of database `db` on one server, taking every command from `budget`:
/// `INFO server`, `INFO memory`, `DBSIZE`, then `INFO keyspace`, `INFO keysizes` and
/// `MEMORY STATS`. No key is read.
///
/// `perms` is the permission check's report: `MEMORY STATS` is not sent when it was
/// denied or skipped there (the gap says why). Pass `&PermissionReport::default()` when
/// there is none: `MEMORY STATS` is then tried, and a refusal becomes a gap.
///
/// Fails only when `INFO server`, `INFO memory` or `DBSIZE` fails or can't be read
/// (show [`failure_text`], never the error's own text: it can name the ACL user).
pub async fn server_figures_with_budget<C: ConnectionLike + Send>(
    conn: &mut C,
    db: u32,
    perms: &PermissionReport,
    budget: &Budget,
) -> Result<ServerFigures, ScanError> {
    let server = server_info_with_budget(conn, budget).await?;
    // A few read-only commands; any part that fails is left out, never the read.
    let context = context::read(conn, db, perms, budget).await;
    Ok(ServerFigures { server, context })
}

/// A fixed sentence for a failed [`server_figures_with_budget`], for the screen: never
/// the server's text, which can name the ACL user.
pub fn failure_text(e: &ScanError) -> &'static str {
    match e {
        ScanError::Redis(r)
            if r.kind() == redis::ErrorKind::Server(redis::ServerErrorKind::NoPerm) =>
        {
            "INFO or DBSIZE is denied for this user (ACL +info +dbsize), so the server's figures are missing"
        }
        ScanError::Redis(r) if r.is_timeout() => {
            "the server did not answer INFO or DBSIZE in time, so its figures are missing"
        }
        ScanError::Redis(r) if r.is_io_error() || r.is_connection_dropped() => {
            "the connection was lost while reading INFO and DBSIZE"
        }
        ScanError::Redis(_) | ScanError::Protocol(_) => {
            "INFO or DBSIZE could not be read, so the server's figures are missing"
        }
        ScanError::NoNodes => "no node of the cluster answered INFO and DBSIZE",
    }
}

/// One cluster node's figures, by its place in [`ClusterTargets::nodes`] (the live
/// node table's order).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeFigures {
    /// `None` when the node has no connection or its `INFO`/`DBSIZE` failed.
    pub figures: Option<ServerFigures>,
    /// Why `figures` is `None`, or the node's own note from discovery. Fixed text,
    /// never a server's error text.
    pub note: Option<String>,
}

/// A cluster's figures, read on every connected node without sampling.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterFigures {
    /// The nodes that answered, summed the way a cluster scan sums them (spec "Cluster",
    /// Merged figures): the first such node's flavor, version and policy, with
    /// `used_memory`, `maxmemory` and `DBSIZE` summed and `mode` cluster; the server
    /// context summed where it adds up, a part kept only when every such node has it.
    pub merged: ServerFigures,
    /// One per node of the targets, in their order.
    pub nodes: Vec<NodeFigures>,
}

impl ClusterFigures {
    /// How many nodes answered.
    pub fn answered(&self) -> usize {
        self.nodes.iter().filter(|n| n.figures.is_some()).count()
    }
}

/// [`server_figures_with_budget`] on every connected node of a cluster (database 0, the
/// only one a cluster has), up to [`MAX_CONCURRENT_NODES`] at a time, each through its
/// own budget. A node without a budget gets one at `max_commands_per_sec`, kept in the
/// targets so a later analysis and the live views on that node share it. Each node
/// uses its own permission report ([`ClusterTargets::check_permissions`]); without one,
/// `MEMORY STATS` is tried.
///
/// A node that fails is left out with a note ([`NodeFigures::note`]); the read fails
/// with [`ScanError::NoNodes`] only when no node answered.
pub async fn cluster_figures(
    targets: &mut ClusterTargets,
    max_commands_per_sec: Option<u32>,
) -> Result<ClusterFigures, ScanError> {
    let empty = PermissionReport::default();
    for t in &mut targets.nodes {
        if t.budget.is_none() {
            t.budget = Some(Budget::new(max_commands_per_sec));
        }
    }
    let reads: Vec<NodeRead<'_, crate::conn::Connection>> = targets
        .nodes
        .iter_mut()
        .map(|t| NodeRead {
            conn: t.conn.as_mut(),
            perms: t.perms.as_ref().unwrap_or(&empty),
            budget: t
                .budget
                .clone()
                .unwrap_or_else(|| Budget::new(max_commands_per_sec)),
        })
        .collect();
    let found = read_nodes(reads, 0).await;
    let nodes: Vec<NodeFigures> = targets
        .nodes
        .iter()
        .zip(found)
        .map(|(t, figures)| {
            let note = match (&figures, t.conn.is_some()) {
                (Some(_), _) => t.note.clone(),
                (None, false) => t.note.clone().or_else(|| Some(NOTE_NOT_CONNECTED.into())),
                (None, true) => Some(match &t.note {
                    Some(n) => format!("{n}; {NOTE_FIGURES_FAILED}"),
                    None => NOTE_FIGURES_FAILED.to_string(),
                }),
            };
            NodeFigures { figures, note }
        })
        .collect();
    let answered: Vec<&ServerFigures> = nodes.iter().filter_map(|n| n.figures.as_ref()).collect();
    let merged = merge_figures(&answered).ok_or(ScanError::NoNodes)?;
    Ok(ClusterFigures { merged, nodes })
}

/// One node for [`read_nodes`].
pub(crate) struct NodeRead<'a, C> {
    /// `None` when the node could not be connected to.
    pub conn: Option<&'a mut C>,
    pub perms: &'a PermissionReport,
    pub budget: Arc<Budget>,
}

/// [`server_figures_with_budget`] on each node, up to [`MAX_CONCURRENT_NODES`] at a
/// time, in the nodes' order; `None` for a node without a connection or whose read
/// failed. The cluster scan's first phase and [`cluster_figures`] both read through it.
pub(crate) async fn read_nodes<C: ConnectionLike + Send>(
    nodes: Vec<NodeRead<'_, C>>,
    db: u32,
) -> Vec<Option<ServerFigures>> {
    // Boxed as `Send` and collected before they are streamed: see
    // `cluster::tests::futures_are_send`.
    let reads: Vec<BoxFuture<'_, Option<ServerFigures>>> = nodes
        .into_iter()
        .map(|n| {
            async move {
                let conn = n.conn?;
                server_figures_with_budget(conn, db, n.perms, &n.budget)
                    .await
                    .ok()
            }
            .boxed()
        })
        .collect();
    stream::iter(reads)
        .buffered(MAX_CONCURRENT_NODES)
        .collect()
        .await
}

/// The figures of several nodes as one: [`merged_server`] and [`merge_contexts`] (with
/// one node, that node's own). `None` with no node.
pub(crate) fn merge_figures(found: &[&ServerFigures]) -> Option<ServerFigures> {
    if found.is_empty() {
        return None;
    }
    let infos: Vec<ServerInfo> = found.iter().map(|f| f.server.clone()).collect();
    let contexts: Vec<&ServerContext> = found.iter().map(|f| &f.context).collect();
    Some(ServerFigures {
        server: merged_server(&infos),
        context: merge_contexts(&contexts),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::fake::{self, Fate};
    use super::*;
    use crate::model::{
        ContextGap, KeysizeHistogram, KeyspaceInfo, MemoryStats, PERM_MEMORY_STATS, PermCheck,
        PermStatus, ServerContextParts, ServerMode,
    };

    /// Compile-time check, never run: the connect-time reads can go in `tokio::spawn`,
    /// the way the TUI runs them (see `cluster::tests::futures_are_send`).
    #[allow(dead_code)]
    fn futures_are_send(
        profile: crate::model::Profile,
        store: Box<dyn crate::secrets::SecretStore>,
    ) {
        drop(tokio::spawn(async move {
            let Ok(mut targets) =
                crate::conn::cluster::connect_targets(&profile, store.as_ref()).await
            else {
                return;
            };
            match &mut targets {
                crate::conn::cluster::ScanTargets::Single { conn, .. } => {
                    let b = Budget::new(Some(5_000));
                    let _ =
                        server_figures_with_budget(conn, 0, &PermissionReport::default(), &b).await;
                }
                crate::conn::cluster::ScanTargets::Cluster(c) => {
                    let _ = cluster_figures(c, Some(5_000)).await;
                }
            }
        }));
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn keyspace(n: usize) -> Vec<(Vec<u8>, Fate)> {
        (0..n)
            .map(|i| (format!("user:{i}").into_bytes(), Fate::Present))
            .collect()
    }

    /// Only server-level commands, each through the budget, and no key read.
    #[test]
    fn figures_send_only_server_commands_through_the_budget() {
        let mut server = fake::Server::new(keyspace(50), 10);
        let b = Budget::new(Some(1_000_000));
        let f = run(server_figures_with_budget(
            &mut server,
            0,
            &PermissionReport::default(),
            &b,
        ))
        .unwrap();
        assert_eq!(
            server.log,
            ["INFO", "INFO", "DBSIZE", "INFO", "INFO", "MEMORY"]
        );
        assert_eq!(b.taken(), FIGURE_COMMANDS);
        assert_eq!(f.server.total_keys, 50);
        assert_eq!(f.context.memory.and_then(|m| m.dataset_bytes), Some(5_000));
        // The fake has no keysizes section.
        assert_eq!(f.context.missing, vec![ContextGap::NoKeysizes]);
        assert_eq!(f.gaps().len(), 1);
        assert!(f.gaps()[0].contains("INFO keysizes"), "{:?}", f.gaps());
    }

    /// A MEMORY STATS the permission check denied is not sent and is a gap, not an error.
    #[test]
    fn a_denied_part_is_a_gap() {
        let mut server = fake::Server::new(keyspace(5), 10);
        let perms = PermissionReport {
            checks: vec![PermCheck {
                label: PERM_MEMORY_STATS,
                status: PermStatus::Denied(
                    "NOPERM MEMORY STATS is not allowed for this user".into(),
                ),
            }],
        };
        let b = Budget::new(None);
        let f = run(server_figures_with_budget(&mut server, 0, &perms, &b)).unwrap();
        assert!(
            !server.log.iter().any(|c| c == "MEMORY"),
            "{:?}",
            server.log
        );
        assert_eq!(f.context.memory, None);
        assert_eq!(
            f.context.missing,
            vec![ContextGap::NoKeysizes, ContextGap::MemoryStatsDenied]
        );
        assert_eq!(f.headline().dataset_bytes, None);
    }

    #[test]
    fn a_failed_info_fails_with_a_fixed_text() {
        let mut server = fake::Server::new(keyspace(5), 10);
        server.fail_info = true;
        let e = run(server_figures_with_budget(
            &mut server,
            0,
            &PermissionReport::default(),
            &Budget::new(None),
        ))
        .unwrap_err();
        let text = failure_text(&e);
        assert!(text.contains("INFO"), "{text}");
        // Nothing after the failed INFO server.
        assert_eq!(server.log, ["INFO"]);
        let noperm = ScanError::Redis(redis::RedisError::from((
            redis::ErrorKind::Server(redis::ServerErrorKind::NoPerm),
            "NOPERM",
            "User lens-ro has no permissions to run the 'info' command".to_string(),
        )));
        let text = failure_text(&noperm);
        assert!(
            text.contains("denied") && !text.contains("lens-ro"),
            "{text}"
        );
    }

    fn figures(keys: u64, ks: Option<KeyspaceInfo>, memory: Option<MemoryStats>) -> ServerFigures {
        ServerFigures {
            server: ServerInfo {
                flavor: crate::model::ServerFlavor::Redis,
                version: "8.2.1".into(),
                mode: ServerMode::Standalone,
                used_memory: 1_000,
                maxmemory: 4_000,
                maxmemory_policy: "allkeys-lfu".into(),
                total_keys: keys,
            },
            context: ServerContextParts {
                keyspace: ks,
                memory,
                keysizes: None,
                missing: Vec::new(),
            }
            .into(),
        }
    }

    #[test]
    fn headline_from_keyspace_and_memory_stats() {
        let mut f = figures(
            1_000,
            Some(KeyspaceInfo {
                db: 0,
                keys: 1_000,
                expires: 310,
                avg_ttl_ms: 0,
                subexpiry: None,
            }),
            Some(MemoryStats {
                dataset_bytes: Some(13_000),
                overhead_total: Some(2_100),
                ..MemoryStats::default()
            }),
        );
        f.context.keysizes = Some(vec![KeysizeHistogram {
            key_type: "strings".into(),
            unit: "sizes".into(),
            bins: vec![(512, 10), (1 << 20, 3)],
        }]);
        let h = f.headline();
        assert_eq!(h.keys, 1_000);
        assert_eq!(h.with_ttl, Some(310));
        assert_eq!(h.ttl_share, Some(0.31));
        assert_eq!(h.dataset_bytes, Some(13_000));
        assert_eq!(h.overhead_bytes, Some(2_100));
        assert_eq!(h.big_keys, Some(3));

        // Nothing but DBSIZE: every other part is off.
        let h = figures(7, None, None).headline();
        assert_eq!(
            h,
            Headline {
                keys: 7,
                with_ttl: None,
                ttl_share: None,
                dataset_bytes: None,
                overhead_bytes: None,
                big_keys: None,
            }
        );
        // An empty database has no share.
        let empty = KeyspaceInfo::default();
        assert_eq!(figures(0, Some(empty), None).headline().ttl_share, None);
    }

    /// The per-node read and the summing the cluster scan uses, without sampling: no
    /// node sends a key command, and a node without a connection is `None`.
    #[test]
    fn nodes_are_read_and_summed_without_sampling() {
        let mut a = fake::Server::new(keyspace(30), 10);
        let mut b = fake::Server::new(keyspace(70), 10);
        let mut c = fake::Server::new(keyspace(5), 10);
        c.fail_info = true;
        let perms = PermissionReport::default();
        let budgets: Vec<Arc<Budget>> = (0..4).map(|_| Budget::new(None)).collect();
        let reads = vec![
            NodeRead {
                conn: Some(&mut a),
                perms: &perms,
                budget: Arc::clone(&budgets[0]),
            },
            NodeRead {
                conn: Some(&mut b),
                perms: &perms,
                budget: Arc::clone(&budgets[1]),
            },
            NodeRead {
                conn: Some(&mut c),
                perms: &perms,
                budget: Arc::clone(&budgets[2]),
            },
            NodeRead {
                conn: None,
                perms: &perms,
                budget: Arc::clone(&budgets[3]),
            },
        ];
        let found = run(read_nodes(reads, 0));
        assert_eq!(
            found.iter().map(Option::is_some).collect::<Vec<_>>(),
            [true, true, false, false]
        );
        for s in [&a, &b, &c] {
            assert!(
                s.log
                    .iter()
                    .all(|c| ["INFO", "DBSIZE", "MEMORY"].contains(&c.as_str())),
                "{:?}",
                s.log
            );
        }
        assert_eq!(budgets[0].taken(), FIGURE_COMMANDS);
        assert_eq!(budgets[3].taken(), 0);
        let answered: Vec<&ServerFigures> = found.iter().flatten().collect();
        let merged = merge_figures(&answered).unwrap();
        assert_eq!(merged.server.mode, ServerMode::Cluster);
        assert_eq!(merged.server.total_keys, 100);
        assert_eq!(
            merged.server.used_memory,
            answered.iter().map(|f| f.server.used_memory).sum::<u64>()
        );
        // dataset.bytes adds up; fragmentation, a per-process ratio, is left out.
        let m = merged.context.memory.unwrap();
        assert_eq!(m.dataset_bytes, Some(10_000));
        assert_eq!(m.fragmentation, None);
        assert!(merge_figures(&[]).is_none());
        // One node keeps that node's own context (fragmentation too).
        let one = merge_figures(&answered[..1]).unwrap();
        assert_eq!(one.context, answered[0].context);
    }
}
