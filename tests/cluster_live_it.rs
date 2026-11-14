//! The live views on a cluster (spec "Cluster live views (P4)") against real clusters,
//! Redis and Valkey, each in one container with 3 primaries announcing 127.0.0.1: one
//! connection and one budget per node, requests routed by slot or to a picked node,
//! `MOVED` and `ASK` followed once, and no write on any node from a connect.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::cluster::{TestCluster, cluster};
use redis_lens::budget::Budget;
use redis_lens::conn::cluster::{ClusterTargets, ScanTargets, connect_targets};
use redis_lens::live::keys::{FactKey, Reply as KeysReply, Request as KeysRequest};
use redis_lens::live::nodes::{Node, NodeInfo, Table};
use redis_lens::live::server::{Reply as ServerReply, Request as ServerRequest};
use redis_lens::live::{self, Dest, LiveError, LiveHandle, LiveReply, LiveRequest, ReqId};
use redis_lens::model::{Auth, Endpoint, Env, KeyType, Profile};
use redis_lens::secrets::MemoryStore;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(15);

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
        // A prod profile: read-only by default.
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
    match connect_targets(&profile(c.seed_port()), &MemoryStore::default()).await {
        Ok(ScanTargets::Cluster(t)) => t,
        Ok(ScanTargets::Single { .. }) => panic!("a cluster node was taken for a standalone"),
        Err(e) => panic!("connect failed: {e}"),
    }
}

/// The live task over every shard, as the TUI starts it: each node's connection and
/// budget.
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

type Answer = Result<LiveReply, LiveError>;

async fn ask(
    h: &LiveHandle,
    rx: &mut mpsc::UnboundedReceiver<(ReqId, Answer)>,
    id: u64,
    dest: Dest,
    req: LiveRequest,
) -> Answer {
    assert!(h.send_to(ReqId(id), dest, req));
    let (got, answer) = tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("an answer in time")
        .expect("the task is alive");
    assert_eq!(got, ReqId(id));
    answer
}

fn facts(key: &str) -> LiveRequest {
    LiveRequest::Keys(KeysRequest::Facts {
        keys: vec![FactKey {
            key: key.into(),
            key_type: KeyType::String,
            length: true,
        }],
    })
}

/// Calls of `name` (`strlen`, `info`, ...) on every node, by port.
async fn calls(c: &TestCluster, ports: &[u16], name: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for p in ports {
        let stats: HashMap<String, (u64, u64)> = c.commandstats(*p).await;
        out.push(stats.get(name).map_or(0, |s| s.0));
    }
    out
}

async fn set(c: &TestCluster, port: u16, key: &str, value: &str) {
    let mut a = c.admin(port).await;
    redis::cmd("SET")
        .arg(key)
        .arg(value)
        .query_async::<()>(&mut a)
        .await
        .unwrap();
}

/// Every node's budget is its own, a request goes to the node it names or to the owner
/// of its key's slot, and a keyed read sent to another node follows `MOVED` once.
#[tokio::test]
async fn requests_reach_the_right_node_and_moved_is_followed_once() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        let mut t = targets(&c).await;
        assert_eq!(t.nodes.len(), 3, "valkey={valkey}");
        for n in &mut t.nodes {
            n.budget = Some(Budget::new(None));
        }
        let ports: Vec<u16> = t.nodes.iter().map(|n| n.endpoint.port).collect();
        // "foo" is slot 12182: the third shard's (10923-16383).
        let owner = 2;
        assert!(
            t.nodes[owner]
                .slots
                .iter()
                .any(|&(a, b)| a <= 12182 && 12182 <= b),
            "valkey={valkey}: {:?}",
            t.nodes[owner].slots
        );
        set(&c, ports[owner], "foo", "abc").await;
        let budgets: Vec<Arc<Budget>> = t.nodes.iter().map(|n| n.budget.clone().unwrap()).collect();
        let (h, mut rx) = start(&t);
        c.reset_stats().await;

        // By key: straight to its owner, nothing anywhere else.
        let r = ask(&h, &mut rx, 1, Dest::key("foo"), facts("foo")).await;
        let Ok(LiveReply::Keys(KeysReply::Facts { facts: got, .. })) = r else {
            panic!("valkey={valkey}: {r:?}");
        };
        assert_eq!(got.len(), 1, "valkey={valkey}");
        assert_eq!(got[0].1.length, Some(3), "valkey={valkey}");
        assert_eq!(
            calls(&c, &ports, "strlen").await,
            [0, 0, 1],
            "valkey={valkey}"
        );
        // One pipeline of two commands (OBJECT ENCODING, STRLEN), from its node's budget.
        assert_eq!(budgets[owner].taken(), 2, "valkey={valkey}");
        assert_eq!(budgets[0].taken(), 0, "valkey={valkey}");

        // Sent to node 0 on purpose: it answers MOVED, and the read is sent once more, to
        // the owner, from the owner's budget.
        let r = ask(&h, &mut rx, 2, Dest::node(0), facts("foo")).await;
        let Ok(LiveReply::Keys(KeysReply::Facts { facts: got, .. })) = r else {
            panic!("valkey={valkey}: {r:?}");
        };
        assert_eq!(got.len(), 1, "valkey={valkey}: {got:?}");
        assert_eq!(got[0].1.length, Some(3), "valkey={valkey}");
        assert_eq!(
            calls(&c, &ports, "strlen").await,
            [0, 0, 2],
            "valkey={valkey}: never run on node 0"
        );
        assert!(
            budgets[0].taken() > 0,
            "valkey={valkey}: node 0 was asked first"
        );

        // The server panels: INFO on the picked node only.
        c.reset_stats().await;
        let r = ask(
            &h,
            &mut rx,
            3,
            Dest::node(1),
            LiveRequest::Server(ServerRequest::Info),
        )
        .await;
        assert!(
            matches!(r, Ok(LiveReply::Server(ServerReply::Info { .. }))),
            "valkey={valkey}: {r:?}"
        );
        assert_eq!(
            calls(&c, &ports, "info").await,
            [0, 1, 0],
            "valkey={valkey}"
        );
    }
}

/// A key of a slot being migrated: the old owner answers `ASK`, and the read is sent
/// once to the importing node with `ASKING` in the same pipeline.
#[tokio::test]
async fn ask_is_followed_once_with_asking() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        let mut t = targets(&c).await;
        for n in &mut t.nodes {
            n.budget = Some(Budget::new(None));
        }
        let ports: Vec<u16> = t.nodes.iter().map(|n| n.endpoint.port).collect();
        let (from, to) = (2, 1);
        let (from_id, to_id) = (t.nodes[from].id.clone(), t.nodes[to].id.clone());
        let slot = 12182; // "foo", and "{foo}gone"
        // Test setup through the admin connections: slot 12182 moves from node 2 to 1.
        let mut a = c.admin(ports[to]).await;
        redis::cmd("CLUSTER")
            .arg("SETSLOT")
            .arg(slot)
            .arg("IMPORTING")
            .arg(&from_id)
            .query_async::<()>(&mut a)
            .await
            .unwrap();
        let mut b = c.admin(ports[from]).await;
        redis::cmd("CLUSTER")
            .arg("SETSLOT")
            .arg(slot)
            .arg("MIGRATING")
            .arg(&to_id)
            .query_async::<()>(&mut b)
            .await
            .unwrap();
        let (h, mut rx) = start(&t);
        c.reset_stats().await;
        // Missing on the old owner: ASK. On the importing node, after ASKING: gone.
        let r = ask(&h, &mut rx, 1, Dest::key("{foo}gone"), facts("{foo}gone")).await;
        let Ok(LiveReply::Keys(KeysReply::Facts {
            facts: got,
            refused,
        })) = r
        else {
            panic!("valkey={valkey}: {r:?}");
        };
        assert!(got.is_empty(), "valkey={valkey}: {got:?}");
        assert!(refused.is_empty(), "valkey={valkey}: {refused:?}");
        assert!(
            calls(&c, &ports, "asking").await[to] >= 1,
            "valkey={valkey}: ASKING went to the importing node"
        );
        assert_eq!(
            calls(&c, &ports, "asking").await[from],
            0,
            "valkey={valkey}"
        );
    }
}

/// A connect on a read-only profile, its probes on every node (each from that node's
/// budget) and the live reads send no write command to any node.
#[tokio::test]
async fn a_read_only_cluster_connect_writes_nothing_on_any_node() {
    // Commands that change data or the server, as `INFO commandstats` names them.
    const WRITES: [&str; 16] = [
        "set", "del", "unlink", "hset", "hdel", "expire", "persist", "rename", "renamenx", "xadd",
        "sadd", "zadd", "lpush", "rpush", "flushall", "flushdb",
    ];
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        c.reset_stats().await;
        let p = profile(c.seed_port());
        assert!(p.is_read_only());
        let mut t = targets(&c).await;
        for n in &mut t.nodes {
            n.budget = Some(Budget::new(None));
        }
        let perms = t.check_all_permissions().await;
        assert!(!perms.checks.is_empty(), "valkey={valkey}");
        for n in &t.nodes {
            assert!(
                n.budget.as_ref().unwrap().taken() > 0,
                "valkey={valkey}: the probes on {} took from its budget",
                n.endpoint
            );
            // The view and write probes are kept per node too.
            let labels: Vec<&str> = n
                .perms
                .as_ref()
                .unwrap()
                .checks
                .iter()
                .map(|c| c.label)
                .collect();
            assert!(labels.len() > 5, "valkey={valkey}: {labels:?}");
        }
        let (h, mut rx) = start(&t);
        for (i, dest) in [
            Dest::node(0),
            Dest::node(1),
            Dest::node(2),
            Dest::key("foo"),
        ]
        .into_iter()
        .enumerate()
        {
            let r = ask(
                &h,
                &mut rx,
                i as u64,
                dest,
                LiveRequest::Server(ServerRequest::Info),
            )
            .await;
            assert!(r.is_ok(), "valkey={valkey}: {r:?}");
        }
        for port in &c.ports {
            let stats = c.commandstats(*port).await;
            for w in WRITES {
                assert_eq!(
                    stats.get(w).map_or(0, |s| s.0),
                    0,
                    "valkey={valkey}: {w} ran on {port}"
                );
            }
        }
    }
}

/// Monitor on a cluster runs on the node it is given, at that node's address: lines
/// from that node arrive, and none from another node.
#[tokio::test]
async fn monitor_runs_on_the_node_it_is_given() {
    use futures::StreamExt;
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        let t = targets(&c).await;
        let node = redis_lens::conn::NodeAddr {
            endpoint: t.nodes[1].endpoint.clone(),
            credentials: t.nodes[1].credentials,
        };
        let budget = Budget::new(None);
        let p = profile(c.seed_port());
        let mut m =
            redis_lens::conn::open_monitor(&p, &MemoryStore::default(), &budget, Some(&node))
                .await
                .expect("connect")
                .expect("MONITOR allowed");
        assert!(budget.taken() > 0, "valkey={valkey}");
        for (i, word) in [(0, "on-node-0"), (1, "on-node-1")] {
            let mut a = c.admin(t.nodes[i].endpoint.port).await;
            redis::cmd("PING")
                .arg(word)
                .query_async::<String>(&mut a)
                .await
                .unwrap();
        }
        let mut seen = Vec::new();
        let found = tokio::time::timeout(WAIT, async {
            while let Some(v) = m.lines.next().await {
                let line = format!("{v:?}");
                let hit = line.contains("on-node-1");
                seen.push(line);
                if hit {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(found, "valkey={valkey}: {seen:?}");
        assert!(
            !seen.iter().any(|l| l.contains("on-node-0")),
            "valkey={valkey}: {seen:?}"
        );
    }
}
