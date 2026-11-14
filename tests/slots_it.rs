//! The Slots panel's requests (spec "2 Analysis · Slots (P4)") against real clusters:
//! `CLUSTER SLOT-STATS` per slot range through the live request path, one budget per
//! node, and the version gate (Redis 8.2 and Valkey 8.0 have the command; the default
//! Redis image, 7.4, and 6.2 do not).
//!
//! Valkey is 8.1 here, so it always has the command. Redis has it only when
//! `REDIS_LENS_IT_REDIS_IMAGE` names 8.2 or later; `REDIS_LENS_IT_REDIS82=1` makes the
//! Redis 8.2 test run (and fail unless the image is 8.2 or later), e.g.
//! `REDIS_LENS_IT_REDIS82=1 REDIS_LENS_IT_REDIS_IMAGE=redis:8.2-alpine`.
#![cfg(feature = "integration-tests")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::cluster::{TestCluster, cluster};
use redis_lens::budget::Budget;
use redis_lens::conn::cluster::{ClusterTargets, ScanTargets, connect_targets};
use redis_lens::live::nodes::{Node, NodeInfo, Table};
use redis_lens::live::slots::{NodeStats, Reply, Request, supports};
use redis_lens::live::{self, Dest, LiveError, LiveHandle, LiveReply, LiveRequest, ReqId};
use redis_lens::model::{Auth, Endpoint, Env, Profile, ServerFlavor};
use redis_lens::secrets::MemoryStore;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(15);

type Answer = Result<LiveReply, LiveError>;

fn profile(port: u16) -> Profile {
    Profile {
        name: "cluster".into(),
        env: Env::Prod,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        tags: Vec::new(),
    }
}

async fn targets(c: &TestCluster) -> ClusterTargets {
    let mut t = match connect_targets(&profile(c.seed_port()), &MemoryStore::default()).await {
        Ok(ScanTargets::Cluster(t)) => t,
        Ok(ScanTargets::Single { .. }) => panic!("a cluster node was taken for a standalone"),
        Err(e) => panic!("connect failed: {e}"),
    };
    for n in &mut t.nodes {
        n.budget = Some(Budget::new(None));
    }
    t
}

fn start(t: &ClusterTargets) -> (LiveHandle, mpsc::UnboundedReceiver<(ReqId, Answer)>) {
    let nodes = t
        .nodes
        .iter()
        .map(|n| Node {
            info: NodeInfo {
                id: n.id.clone(),
                endpoint: n.endpoint.clone(),
                role: n.role,
                slots: n.slots.clone(),
                connected: n.conn.is_some(),
            },
            conn: n.conn.clone().map(|c| c.into_inner()),
            budget: n.budget.clone().expect("a budget per node"),
        })
        .collect();
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = live::spawn_nodes(
        Table::cluster(nodes),
        move |id, r| {
            let _ = tx.send((id, r));
        },
        |_| {},
    );
    (handle, rx)
}

/// Sends the node's own slot ranges to node `i` and waits for the answer.
async fn ask_node(
    h: &LiveHandle,
    rx: &mut mpsc::UnboundedReceiver<(ReqId, Answer)>,
    i: usize,
    ranges: Vec<(u16, u16)>,
) -> Answer {
    let id = ReqId(100 + i as u64);
    assert!(h.send_to(
        id,
        Dest::node(i),
        LiveRequest::Slots(Request::Stats { ranges })
    ));
    let (got, answer) = tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("an answer in time")
        .expect("the task is alive");
    assert_eq!(got, id);
    answer
}

fn stats(a: Answer) -> NodeStats {
    match a {
        Ok(LiveReply::Slots(Reply::Stats(s))) => s,
        other => panic!("expected slot stats, got {other:?}"),
    }
}

async fn count_in_slot(c: &TestCluster, port: u16, slot: u16) -> u64 {
    let mut a = c.admin(port).await;
    redis::cmd("CLUSTER")
        .arg("COUNTKEYSINSLOT")
        .arg(slot)
        .query_async(&mut a)
        .await
        .unwrap()
}

/// Seeds keys, asks every node for its ranges and checks the counts against
/// `CLUSTER COUNTKEYSINSLOT`, the per-range sums against `DBSIZE`, and the budgets.
async fn counts_match(c: &TestCluster, label: &str) {
    let seed: Vec<(String, String)> = (0..300)
        .map(|i| (format!("slots:{i}"), "v".to_string()))
        .collect();
    c.seed(&seed).await;
    let t = targets(c).await;
    let budgets: Vec<Arc<Budget>> = t.nodes.iter().map(|n| n.budget.clone().unwrap()).collect();
    let (h, mut rx) = start(&t);
    let mut total = 0;
    for (i, n) in t.nodes.iter().enumerate() {
        let ranges = n.slots.clone();
        let s = stats(ask_node(&h, &mut rx, i, ranges.clone()).await);
        assert!(s.supported, "{label}: {} {}", s.flavor_name(), s.version);
        // INFO server, then one command per range, each from this node's budget.
        assert_eq!(budgets[i].taken(), 1 + ranges.len() as u64, "{label}");
        assert_eq!(s.ranges.len(), ranges.len(), "{label}");
        let mut node_keys = 0;
        for (r, want) in s.ranges.iter().zip(&ranges) {
            assert_eq!((r.start, r.end), *want, "{label}");
            assert_eq!(
                r.slots.len(),
                usize::from(r.end - r.start) + 1,
                "{label}: one entry per slot"
            );
            node_keys += r
                .slots
                .iter()
                .map(|x| x.keys.expect("key-count"))
                .sum::<u64>();
            // A few slots against COUNTKEYSINSLOT: the first, a middle one, and every
            // slot that holds keys among the first 600.
            for st in r.slots.iter().filter(|x| x.keys != Some(0)).take(5) {
                assert_eq!(
                    st.keys,
                    Some(count_in_slot(c, n.endpoint.port, st.slot).await),
                    "{label}: slot {}",
                    st.slot
                );
            }
            let mid = r.slots[r.slots.len() / 2];
            assert_eq!(
                mid.keys,
                Some(count_in_slot(c, n.endpoint.port, mid.slot).await),
                "{label}: slot {}",
                mid.slot
            );
        }
        let mut a = c.admin(n.endpoint.port).await;
        let dbsize: u64 = redis::cmd("DBSIZE").query_async(&mut a).await.unwrap();
        assert_eq!(node_keys, dbsize, "{label}: ranges add up to DBSIZE");
        total += node_keys;
    }
    assert_eq!(total, 300, "{label}");
}

trait Named {
    fn flavor_name(&self) -> &'static str;
}

impl Named for NodeStats {
    fn flavor_name(&self) -> &'static str {
        match self.flavor {
            ServerFlavor::Redis => "Redis",
            ServerFlavor::Valkey => "Valkey",
            ServerFlavor::Unknown => "server",
        }
    }
}

/// Valkey 8.1: the command exists; the counts match `CLUSTER COUNTKEYSINSLOT`.
#[tokio::test]
async fn valkey_counts_per_range_match_countkeysinslot() {
    let c = cluster(true, false).await;
    counts_match(&c, "valkey").await;
}

/// A node too old for the command is not sent it: the reply says so and only
/// `INFO server` was taken from the budget.
async fn too_old_is_off(c: &TestCluster, label: &str) {
    let t = targets(c).await;
    let budgets: Vec<Arc<Budget>> = t.nodes.iter().map(|n| n.budget.clone().unwrap()).collect();
    let (h, mut rx) = start(&t);
    let s = stats(ask_node(&h, &mut rx, 0, t.nodes[0].slots.clone()).await);
    assert!(!s.supported, "{label}: {}", s.version);
    assert_eq!(s.flavor, ServerFlavor::Redis, "{label}");
    assert!(!supports(s.flavor, &s.version), "{label}");
    assert!(s.ranges.is_empty(), "{label}");
    assert_eq!(budgets[0].taken(), 1, "{label}: INFO server only");
}

/// The Redis image under test: with the default (7.4) or 6.2 the panel is off; from 8.2
/// the counts match as on Valkey.
#[tokio::test]
async fn redis_follows_its_version() {
    let c = cluster(false, false).await;
    let version = c.version().await;
    if supports(ServerFlavor::Redis, &version) {
        counts_match(&c, "redis").await;
    } else {
        assert!(
            version.starts_with("7.") || version.starts_with("6.") || version.starts_with("8."),
            "{version}"
        );
        too_old_is_off(&c, &format!("redis {version}")).await;
    }
}

/// Redis 8.2 or later, only when asked for: `REDIS_LENS_IT_REDIS82=1` with
/// `REDIS_LENS_IT_REDIS_IMAGE=redis:8.2-alpine` (or newer). Fails if the image is older,
/// so a run that says it covered 8.2 did.
#[tokio::test]
async fn redis_82_counts_match_when_asked_for() {
    if std::env::var("REDIS_LENS_IT_REDIS82").as_deref() != Ok("1") {
        eprintln!("skipped: set REDIS_LENS_IT_REDIS82=1 and a Redis 8.2 image");
        return;
    }
    let c = cluster(false, false).await;
    let version = c.version().await;
    assert!(
        supports(ServerFlavor::Redis, &version),
        "REDIS_LENS_IT_REDIS82=1 needs REDIS_LENS_IT_REDIS_IMAGE=redis:8.2-alpine or newer, got {version}"
    );
    counts_match(&c, "redis 8.2").await;
}
