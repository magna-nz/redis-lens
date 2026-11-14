//! The names pager (`live::names`) and the pattern tree from names
//! (`pattern::names`), against real Redis and Valkey servers and real clusters: every
//! name is read across pages with `SCAN` only, and nothing else about a key.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use common::cluster::{cluster, parse_commandstats};
use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn::{self, cluster::ScanTargets};
use redis_lens::live::names::{self, Page, Reply, Request, Walk};
use redis_lens::live::nodes::{Node, NodeInfo, Table};
use redis_lens::live::{self, LiveError, LiveReply, LiveRequest, ReqId};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::pattern::names::NamePatterns;
use redis_lens::secrets::MemoryStore;
use tokio::sync::mpsc;

/// Commands the walk may cause besides `SCAN`: the test's own `CONFIG RESETSTAT`,
/// `INFO commandstats` and the handshake of the admin connections it opens for them
/// (`TestCluster::commandstats` opens one per call).
const BOOKKEEPING: [&str; 6] = [
    "info",
    "config|resetstat",
    "config",
    "client|setinfo",
    "client",
    "hello",
];

fn profile(port: u16) -> Profile {
    Profile {
        name: "names".to_string(),
        env: Env::Prod,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
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

/// `n` names in three patterns.
fn seeded(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| match i % 3 {
            0 => format!("user:{i}"),
            1 => format!("session:{i}:data"),
            _ => format!("cart:{i}"),
        })
        .collect()
}

async fn seed(admin: &mut MultiplexedConnection, names: &[String]) {
    let mut pipe = redis::pipe();
    for k in names {
        pipe.cmd("SET").arg(k).arg("v").ignore();
    }
    let () = pipe.query_async(admin).await.unwrap();
}

async fn reset_stats(c: &mut MultiplexedConnection) {
    redis::cmd("CONFIG")
        .arg("RESETSTAT")
        .query_async::<()>(c)
        .await
        .unwrap();
}

async fn commandstats(c: &mut MultiplexedConnection) -> HashMap<String, (u64, u64)> {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(c)
        .await
        .unwrap();
    parse_commandstats(&info)
}

/// Only `SCAN` (and the test's own bookkeeping) was sent, `scans` times.
fn assert_scan_only(stats: &HashMap<String, (u64, u64)>, scans: u64, what: &str) {
    for name in stats.keys() {
        assert!(
            name == "scan" || BOOKKEEPING.contains(&name.as_str()),
            "{what}: {name} was sent: {stats:?}"
        );
    }
    assert_eq!(stats.get("scan").map(|s| s.0), Some(scans), "{what}");
}

async fn one_page(
    c: &mut MultiplexedConnection,
    b: &Budget,
    cursor: u64,
    glob: Option<&str>,
) -> Page {
    let req = Request::Page {
        cursor,
        glob: glob.map(String::from),
    };
    let Reply::Page(p) = names::run(c, b, req).await.expect("a page");
    p
}

#[tokio::test]
async fn the_pager_reads_every_name_with_scan_only() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let all = seeded(3_000);
        seed(&mut admin, &all).await;
        let mut c = conn::connect(&profile(server.port), &MemoryStore::default())
            .await
            .expect("connect")
            .into_inner();
        reset_stats(&mut admin).await;

        let budget = Budget::new(Some(5_000));
        let mut tree = NamePatterns::new();
        let mut walk = Walk::standalone(None);
        let mut pages = 0u64;
        let mut first_estimate = None;
        while let Some(ask) = walk.next_ask() {
            assert_eq!(ask.node, 0);
            let LiveRequest::Names(req) = ask.req else {
                panic!("{what}: not a names request")
            };
            let Reply::Page(page) = names::run(&mut c, &budget, req).await.expect("a page");
            pages += 1;
            tree.add(page.names.iter().cloned());
            tree.add_unreadable(page.unreadable);
            walk.take(0, &page);
            if first_estimate.is_none() && !walk.done() {
                first_estimate = Some(tree.patterns(all.len() as u64, false));
            }
            assert!(pages < 100, "{what}: the walk never ends");
        }
        assert!(walk.complete(), "{what}");
        assert!(pages > 1, "{what}: one page held every name");
        assert_eq!(
            budget.taken(),
            pages,
            "{what}: one command a page, through the budget"
        );
        let got: HashSet<&str> = all
            .iter()
            .map(String::as_str)
            .filter(|n| tree.contains(n))
            .collect();
        assert_eq!(got.len(), all.len(), "{what}: every name was read");
        assert_eq!(tree.names_read(), all.len() as u64, "{what}");
        assert_scan_only(&commandstats(&mut admin).await, pages, what);

        // Walked to the end: the tree counts names as seen.
        let rows = tree.patterns(all.len() as u64, true);
        let mut by_pattern: Vec<(&str, u64)> = rows
            .iter()
            .map(|r| (r.pattern.as_str(), r.est_keys))
            .collect();
        by_pattern.sort();
        assert_eq!(
            by_pattern,
            [
                ("cart:{id}", 1_000),
                ("session:{id}:data", 1_000),
                ("user:{id}", 1_000)
            ],
            "{what}"
        );
        // After the first page: estimated from DBSIZE, within rounding.
        let first = first_estimate.expect("more than one page");
        let total: u64 = first.iter().map(|r| r.est_keys).sum();
        assert!(total.abs_diff(3_000) <= 2, "{what}: first estimate {total}");
    }
}

#[tokio::test]
async fn match_narrows_the_names() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let all = seeded(600);
        seed(&mut admin, &all).await;
        let mut c = conn::connect(&profile(server.port), &MemoryStore::default())
            .await
            .expect("connect")
            .into_inner();
        let b = Budget::new(None);
        let mut cursor = 0;
        let mut found = HashSet::new();
        loop {
            let p = one_page(&mut c, &b, cursor, Some("user:*")).await;
            found.extend(p.names);
            cursor = p.cursor;
            if cursor == 0 {
                break;
            }
        }
        assert_eq!(found.len(), 200, "{what}");
        assert!(found.iter().all(|n| n.starts_with("user:")), "{what}");
    }
}

type Answer = Result<LiveReply, LiveError>;

/// On a cluster the walk reads every shard's node in turn, one cursor per node, sends
/// only `SCAN` to each, and says when every node is done.
#[tokio::test]
async fn a_cluster_walk_reads_every_node() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let c = cluster(valkey, false).await;
        let all = seeded(2_400);
        let pairs: Vec<(String, String)> = all.iter().map(|k| (k.clone(), "v".into())).collect();
        c.seed(&pairs).await;
        let mut t =
            match conn::cluster::connect_targets(&profile(c.seed_port()), &MemoryStore::default())
                .await
            {
                Ok(ScanTargets::Cluster(t)) => t,
                Ok(ScanTargets::Single { .. }) => {
                    panic!("{what}: a cluster taken for a standalone")
                }
                Err(e) => panic!("{what}: connect failed: {e}"),
            };
        for n in &mut t.nodes {
            n.budget = Some(Budget::new(None));
        }
        let infos: Vec<NodeInfo> = t
            .nodes
            .iter()
            .map(|n| NodeInfo {
                id: n.id.clone(),
                endpoint: n.endpoint.clone(),
                role: n.role,
                slots: n.slots.clone(),
                connected: n.conn.is_some(),
            })
            .collect();
        let table = t
            .nodes
            .iter()
            .zip(&infos)
            .map(|(n, info)| Node {
                info: info.clone(),
                conn: n.conn.clone().map(|c| c.into_inner()),
                budget: n.budget.clone().unwrap(),
            })
            .collect();
        let (tx, mut rx) = mpsc::unbounded_channel::<(ReqId, Answer)>();
        let handle = live::spawn_nodes(
            Table::cluster(table),
            move |id, r| {
                let _ = tx.send((id, r));
            },
            |_| {},
        );
        c.reset_stats().await;

        let mut walk = Walk::cluster(&infos, None);
        assert!(walk.missing().is_empty(), "{what}");
        let mut tree = NamePatterns::new();
        let mut pages_per_node: HashMap<usize, u64> = HashMap::new();
        let mut order = Vec::new();
        let mut id = 0;
        while let Some(ask) = walk.next_ask() {
            id += 1;
            assert!(handle.send_to(ReqId(id), ask.dest.clone(), ask.req.clone()));
            let (got, answer) = tokio::time::timeout(Duration::from_secs(15), rx.recv())
                .await
                .expect("an answer in time")
                .expect("the task is alive");
            assert_eq!(got, ReqId(id));
            let page = match answer {
                Ok(LiveReply::Names(Reply::Page(p))) => p,
                other => panic!("{what}: {other:?}"),
            };
            *pages_per_node.entry(ask.node).or_default() += 1;
            order.push(ask.node);
            tree.add(page.names.iter().cloned());
            walk.take(ask.node, &page);
            assert!(id < 300, "{what}: the walk never ends");
        }
        assert!(walk.done() && walk.complete(), "{what}");
        assert_eq!(walk.nodes_done(), (3, 3), "{what}");
        // The nodes took turns.
        assert_eq!(&order[..3], [0, 1, 2], "{what}");
        assert_eq!(tree.names_read(), all.len() as u64, "{what}");
        assert!(all.iter().all(|n| tree.contains(n)), "{what}");
        for (i, n) in t.nodes.iter().enumerate() {
            let stats = c.commandstats(n.endpoint.port).await;
            assert_scan_only(&stats, pages_per_node[&i], &format!("{what} node {i}"));
        }
        let est: u64 = tree
            .patterns(all.len() as u64, walk.complete())
            .iter()
            .map(|r| r.est_keys)
            .sum();
        assert_eq!(est, all.len() as u64, "{what}");
        drop(handle);
    }
}
