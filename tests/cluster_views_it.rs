//! The views that work node by node on a cluster (spec "Cluster live views (P4)")
//! against real clusters, Redis and Valkey, through the live task as the TUI starts it:
//! the key list over every primary, the key detail and an edit on the slot owner,
//! Streams, Pub/Sub counts and shard channels, bulk actions per primary with their key
//! file, `redis-lens expire`, the command line's routing, and no write on any node from
//! a read-only profile. Shard channel tests need Redis 7.0 or later (every Valkey).
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::cluster::{TestCluster, cluster};
use redis_lens::analysis::match_glob;
use redis_lens::budget::Budget;
use redis_lens::config::Config;
use redis_lens::conn::cluster::{ClusterTargets, ScanTargets, connect_targets};
use redis_lens::live::bulk::{self, FileAt, Reply as BulkReply, Request as BulkRequest, Walk};
use redis_lens::live::cmdline::{self, Args, Keys, Reply as CmdReply, Request as CmdRequest};
use redis_lens::live::keys::{Reply as KeysReply, Request as KeysRequest};
use redis_lens::live::nodes::{Node, NodeInfo, Table};
use redis_lens::live::pubsub::{
    self, Push, Reply as PubSubReply, Request as PubSubRequest, Subscription,
};
use redis_lens::live::streams::{Reply as StreamsReply, Request as StreamsRequest};
use redis_lens::live::values::{Kind, Reply as ValueReply, Request as ValueRequest};
use redis_lens::live::{self, Dest, Follow, LiveError, LiveHandle, LiveReply, LiveRequest, ReqId};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::secrets::MemoryStore;
use redis_lens::value::Raw;
use redis_lens::write::{Answer, BulkAction, Permit, Refusals, Settings, WriteOp};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(15);
static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-cluster-views-it-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn profile(port: u16, env: Env, read_only: Option<bool>) -> Profile {
    Profile {
        name: "it".into(),
        env,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        tags: Vec::new(),
    }
}

/// A permit from the gate on a local profile with writes on (the answer `y`).
fn permit(op: WriteOp, port: u16) -> Permit {
    Permit::grant(
        op,
        &profile(port, Env::Local, None),
        &Settings::default(),
        &Refusals::default(),
        &Answer::Yes,
    )
    .expect("a permit")
}

async fn targets(c: &TestCluster) -> ClusterTargets {
    let p = profile(c.seed_port(), Env::Prod, None);
    match connect_targets(&p, &MemoryStore::default()).await {
        Ok(ScanTargets::Cluster(t)) => t,
        Ok(ScanTargets::Single { .. }) => panic!("a cluster node was taken for a standalone"),
        Err(e) => panic!("connect failed: {e}"),
    }
}

type Answer2 = Result<LiveReply, LiveError>;
type Rx = mpsc::UnboundedReceiver<(ReqId, Answer2)>;

/// The live task over every shard, each node with its own budget.
fn start(t: &ClusterTargets) -> (LiveHandle, Rx) {
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
            budget: n.budget.clone().unwrap_or_else(|| Budget::new(None)),
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

struct Live {
    h: LiveHandle,
    rx: Rx,
    next: u64,
}

impl Live {
    async fn ask(&mut self, dest: Dest, req: LiveRequest) -> Answer2 {
        self.next += 1;
        let id = ReqId(self.next);
        assert!(self.h.send_to(id, dest, req));
        let (got, answer) = tokio::time::timeout(WAIT, self.rx.recv())
            .await
            .expect("an answer in time")
            .expect("the task is alive");
        assert_eq!(got, id);
        answer
    }
}

fn never(i: usize) -> Dest {
    Dest::node(i).follow(Follow::Never)
}

/// The index of the shard that owns `key`'s slot.
fn owner(t: &ClusterTargets, key: &str) -> usize {
    let slot = redis_lens::conn::slot::key_slot(key.as_bytes());
    t.nodes
        .iter()
        .position(|n| n.slots.iter().any(|&(a, b)| a <= slot && slot <= b))
        .expect("an owner")
}

/// Calls of `name` on every node, by port.
async fn calls(c: &TestCluster, ports: &[u16], name: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for p in ports {
        let stats: HashMap<String, (u64, u64)> = c.commandstats(*p).await;
        out.push(stats.get(name).map_or(0, |s| s.0));
    }
    out
}

/// `n` of `user:<i>:cart`, spread over every shard.
fn carts(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| (format!("user:{i}:cart"), "x".into()))
        .collect()
}

/// The key list of `user:{id}:cart` on node `i`, every page until its cursor is 0.
async fn walk_node(live: &mut Live, i: usize) -> Vec<String> {
    let (mut cursor, mut carry, mut keys) = (0, Vec::new(), Vec::new());
    loop {
        let r = live
            .ask(
                never(i),
                LiveRequest::Keys(KeysRequest::Page {
                    pattern: "user:{id}:cart".into(),
                    cursor,
                    carry,
                    sizes: false,
                    freq: false,
                }),
            )
            .await;
        let Ok(LiveReply::Keys(KeysReply::Page(p))) = r else {
            panic!("node {i}: {r:?}");
        };
        keys.extend(p.rows.into_iter().map(|r| r.key));
        (cursor, carry) = (p.cursor, p.carry);
        if cursor == 0 && carry.is_empty() {
            return keys;
        }
    }
}

#[tokio::test]
async fn the_key_list_walks_every_primary_and_reads_and_edits_on_the_owner() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        c.seed(&carts(300)).await;
        let t = targets(&c).await;
        let ports: Vec<u16> = t.nodes.iter().map(|n| n.endpoint.port).collect();
        let (h, rx) = start(&t);
        let mut live = Live { h, rx, next: 0 };
        // Each primary's own SCAN: its keys only, all of them over the three nodes.
        let mut all = BTreeSet::new();
        for i in 0..3 {
            let keys = walk_node(&mut live, i).await;
            assert!(!keys.is_empty(), "valkey={valkey}: node {i}");
            for k in keys {
                assert_eq!(owner(&t, &k), i, "valkey={valkey}: {k}");
                assert!(all.insert(k));
            }
        }
        assert_eq!(all.len(), 300, "valkey={valkey}");

        // The key detail reads the owner, nothing elsewhere.
        c.reset_stats().await;
        let o = owner(&t, "user:7:cart");
        let r = live
            .ask(
                Dest::key("user:7:cart"),
                LiveRequest::Value(ValueRequest {
                    key: "user:7:cart".into(),
                    kind: Kind::String,
                    at: None,
                    field_ttls: false,
                    size: None,
                }),
            )
            .await;
        assert!(
            matches!(r, Ok(LiveReply::Value(ValueReply::Page(_)))),
            "valkey={valkey}: {r:?}"
        );
        let reads = calls(&c, &ports, "getrange").await;
        assert_eq!(reads[o], 1, "valkey={valkey}: {reads:?}");
        assert_eq!(reads.iter().sum::<u64>(), 1, "valkey={valkey}: {reads:?}");

        // An edit lands on the owner, through a permit.
        let op = WriteOp::HSet {
            key: "{edit}h".into(),
            field: Raw::from("f"),
            value: Raw::from("v"),
        };
        let r = live
            .ask(
                Dest::key("{edit}h"),
                LiveRequest::Write(permit(op, c.seed_port())),
            )
            .await;
        assert!(r.is_ok(), "valkey={valkey}: {r:?}");
        let o = owner(&t, "{edit}h");
        let hsets = calls(&c, &ports, "hset").await;
        assert_eq!(hsets[o], 1, "valkey={valkey}: {hsets:?}");
        assert_eq!(hsets.iter().sum::<u64>(), 1, "valkey={valkey}");
    }
}

#[tokio::test]
async fn streams_are_read_on_their_owners_one_request_per_node() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        let t = targets(&c).await;
        let ports: Vec<u16> = t.nodes.iter().map(|n| n.endpoint.port).collect();
        // "bar" is node 0's, "foo" node 2's.
        for (key, port) in [("bar", 0), ("foo", 2)] {
            assert_eq!(owner(&t, key), port);
            let mut a = c.admin(ports[port]).await;
            redis::cmd("XADD")
                .arg(key)
                .arg("*")
                .arg("f")
                .arg("v")
                .query_async::<String>(&mut a)
                .await
                .unwrap();
        }
        let (h, rx) = start(&t);
        let mut live = Live { h, rx, next: 0 };
        c.reset_stats().await;
        for (key, node) in [("bar", 0), ("foo", 2)] {
            let r = live
                .ask(
                    never(node),
                    LiveRequest::Streams(StreamsRequest::Info {
                        keys: vec![key.into()],
                        refused: Default::default(),
                    }),
                )
                .await;
            let Ok(LiveReply::Streams(StreamsReply::Info { streams, .. })) = r else {
                panic!("valkey={valkey}: {r:?}");
            };
            assert_eq!(
                streams[0].info.as_ref().map(|i| i.length),
                Some(1),
                "valkey={valkey}: {key}"
            );
        }
        let xinfo = calls(&c, &ports, "xinfo|stream").await;
        let xinfo = if xinfo.iter().sum::<u64>() == 0 {
            calls(&c, &ports, "xinfo").await
        } else {
            xinfo
        };
        // Redis 6.2 counts every XINFO subcommand as `xinfo`: none on node 1.
        assert!(
            xinfo[0] > 0 && xinfo[1] == 0 && xinfo[2] > 0,
            "valkey={valkey}: {xinfo:?}"
        );
    }
}

#[tokio::test]
async fn pubsub_counts_shard_channels_per_node_and_a_shard_tail_gets_an_spublish() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        let t = targets(&c).await;
        let (h, rx) = start(&t);
        let mut live = Live { h, rx, next: 0 };
        // Classic counts on the picked node.
        let r = live
            .ask(Dest::node(1), LiveRequest::PubSub(PubSubRequest::Counts))
            .await;
        assert!(
            matches!(r, Ok(LiveReply::PubSub(PubSubReply::Counts(_)))),
            "valkey={valkey}: {r:?}"
        );
        if c.is_redis6().await {
            // Redis 6.2 has no shard channels: the view leaves them out with a reason.
            continue;
        }
        // An SSUBSCRIBE tail on the channel's owner.
        let o = owner(&t, "foo");
        let node = redis_lens::conn::NodeAddr {
            endpoint: t.nodes[o].endpoint.clone(),
            credentials: t.nodes[o].credentials,
        };
        let p = profile(c.seed_port(), Env::Prod, None);
        let (mut sub, mut pushes) =
            redis_lens::conn::open_subscriber(&p, &MemoryStore::default(), Some(&node))
                .await
                .expect("open the subscriber");
        let budget = Budget::new(None);
        pubsub::subscribe(&mut sub, &budget, &Subscription::Shard("foo".into()))
            .await
            .expect("ssubscribe");
        // Shard counts from every primary: only the owner lists it.
        let end = tokio::time::Instant::now() + WAIT;
        loop {
            let mut seen = Vec::new();
            for i in 0..3 {
                let r = live
                    .ask(never(i), LiveRequest::PubSub(PubSubRequest::ShardCounts))
                    .await;
                let Ok(LiveReply::PubSub(PubSubReply::ShardCounts(s))) = r else {
                    panic!("valkey={valkey}: {r:?}");
                };
                seen.push(
                    s.channels
                        .iter()
                        .find(|ch| ch.name == "foo")
                        .and_then(|ch| ch.subscribers),
                );
            }
            let want: Vec<Option<u64>> = (0..3).map(|i| (i == o).then_some(1)).collect();
            if seen == want {
                break;
            }
            assert!(
                tokio::time::Instant::now() < end,
                "valkey={valkey}: {seen:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // SPUBLISH through the gate, on the owner: the tail gets it.
        let op = WriteOp::SPublish {
            channel: "foo".into(),
            message: Raw::from("hello"),
        };
        let r = live
            .ask(
                Dest::key("foo"),
                LiveRequest::Write(permit(op, c.seed_port())),
            )
            .await;
        assert!(r.is_ok(), "valkey={valkey}: {r:?}");
        let m = loop {
            let push = tokio::time::timeout(WAIT, pushes.recv())
                .await
                .expect("a push in time")
                .expect("open");
            match pubsub::parse_push(push, 1) {
                Push::Message(m) => break m,
                Push::Lost => panic!("lost"),
                Push::Other => {}
            }
        };
        assert_eq!(m.channel, "foo", "valkey={valkey}");
        assert_eq!(m.payload.as_bytes(), b"hello", "valkey={valkey}");
    }
}

/// Every dry-run step of node `i`, summed.
async fn count_node(live: &mut Live, i: usize, pattern: &str) -> bulk::Count {
    let mut total = bulk::Count::default();
    let mut walk = Walk::Start;
    loop {
        let r = live
            .ask(
                never(i),
                LiveRequest::Bulk(BulkRequest::Count {
                    pattern: pattern.into(),
                    glob: match_glob(pattern),
                    walk,
                }),
            )
            .await;
        let Ok(LiveReply::Bulk(BulkReply::Count(c))) = r else {
            panic!("node {i}: {r:?}");
        };
        total.add(&c);
        match c.walk {
            Some(Walk::Done) | None => return total,
            Some(w) => walk = w,
        }
    }
}

/// Every batch of `permit` on node `i`, its lines in `file`. Returns the keys changed.
async fn run_node(live: &mut Live, i: usize, permit: &Permit, file: &Path, addr: &str) -> u64 {
    let (mut walk, mut carry, mut changed) = (Walk::Start, Vec::new(), 0);
    loop {
        let r = live
            .ask(
                never(i),
                LiveRequest::Bulk(BulkRequest::Batch {
                    permit: permit.clone(),
                    walk,
                    carry,
                    file: FileAt::Path(file.to_path_buf()),
                    node: Some(addr.into()),
                }),
            )
            .await;
        let Ok(LiveReply::Bulk(BulkReply::Batch(b))) = r else {
            panic!("node {i}: {r:?}");
        };
        assert_eq!(b.failed, 0);
        changed += b.changed;
        if b.finished() {
            return changed;
        }
        (walk, carry) = (b.walk, b.carry);
    }
}

#[tokio::test]
async fn bulk_expire_and_delete_run_per_primary_with_the_node_in_the_key_file() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        c.seed(&carts(240)).await;
        let t = targets(&c).await;
        let (h, rx) = start(&t);
        let mut live = Live { h, rx, next: 0 };
        let addrs: Vec<String> = t.nodes.iter().map(|n| n.endpoint.to_string()).collect();
        // The dry run counts each primary's own keys.
        let mut per_node = Vec::new();
        for i in 0..3 {
            per_node.push(count_node(&mut live, i, "user:{id}:cart").await.matched);
        }
        assert_eq!(per_node.iter().sum::<u64>(), 240, "valkey={valkey}");
        assert!(
            per_node.iter().all(|n| *n > 0),
            "valkey={valkey}: {per_node:?}"
        );
        let pattern = "user:{id}:cart".to_string();
        let glob = match_glob(&pattern);
        let dir = temp_dir("bulk");
        let expire_nx = !c.is_redis6().await;
        if expire_nx {
            let file = dir.join("expire.txt");
            std::fs::write(&file, "").unwrap();
            let op = WriteOp::Bulk {
                pattern: pattern.clone(),
                glob: glob.clone(),
                action: BulkAction::Expire { seconds: 600 },
            };
            let p = permit(op, c.seed_port());
            let mut changed = 0;
            for (i, addr) in addrs.iter().enumerate() {
                changed += run_node(&mut live, i, &p, &file, addr).await;
            }
            assert_eq!(changed, 240, "valkey={valkey}");
            let text = std::fs::read_to_string(&file).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len(), 240, "valkey={valkey}");
            for l in lines {
                let (node, key) = bulk::split_key_line(l).expect("node, tab, key");
                assert_eq!(addrs[owner(&t, &key)], node, "valkey={valkey}: {l}");
            }
            let o = owner(&t, "user:3:cart");
            let mut a = c.admin(t.nodes[o].endpoint.port).await;
            let ttl: i64 = redis::cmd("TTL")
                .arg("user:3:cart")
                .query_async(&mut a)
                .await
                .unwrap();
            assert!((1..=600).contains(&ttl), "valkey={valkey}: {ttl}");
        }
        let file = dir.join("delete.txt");
        std::fs::write(&file, "").unwrap();
        let op = WriteOp::Bulk {
            pattern,
            glob,
            action: BulkAction::Delete,
        };
        let p = permit(op, c.seed_port());
        let mut deleted = 0;
        for (i, addr) in addrs.iter().enumerate() {
            deleted += run_node(&mut live, i, &p, &file, addr).await;
        }
        assert_eq!(deleted, 240, "valkey={valkey}");
        for port in &c.ports {
            let mut a = c.admin(*port).await;
            let n: u64 = redis::cmd("DBSIZE").query_async(&mut a).await.unwrap();
            assert_eq!(n, 0, "valkey={valkey}: {port}");
        }
    }
}

#[tokio::test]
async fn redis_lens_expire_runs_on_every_primary_of_a_cluster() {
    use redis_lens::cli::expire::{ExpireArgs, execute};
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        c.seed(&carts(90)).await;
        let dir = temp_dir("cli");
        let config = dir.join("config.toml");
        Config {
            profiles: vec![profile(c.seed_port(), Env::Local, None)],
            ..Default::default()
        }
        .save(&config)
        .unwrap();
        let mut out = Vec::new();
        let ask = |_: &str, hint: &str| -> anyhow::Result<String> {
            anyhow::bail!("no terminal in tests; {hint}")
        };
        let args = ExpireArgs {
            profile: "it".into(),
            pattern: "user:{id}:cart".into(),
            ttl: 90,
            apply: true,
            yes: true,
            confirm: None,
            max_rate: Some(10_000),
        };
        let r = execute(
            &config,
            &MemoryStore::default(),
            &args,
            &dir,
            &ask,
            &mut out,
            &mut |_| {},
        )
        .await;
        let out = String::from_utf8(out).unwrap();
        if c.is_redis6().await {
            assert!(r.unwrap_err().to_string().contains("Redis 6.2"));
            continue;
        }
        let r = r.unwrap_or_else(|e| panic!("valkey={valkey}: {e}\n{out}"));
        assert_eq!(
            r.run.as_ref().unwrap().changed,
            90,
            "valkey={valkey}: {out}"
        );
        assert!(
            out.contains("Nodes:    3 primaries"),
            "valkey={valkey}: {out}"
        );
        assert!(
            out.contains("the node, a tab, the key"),
            "valkey={valkey}: {out}"
        );
        let calls_expire: u64 = {
            let mut n = 0;
            for p in &c.ports {
                n += c.commandstats(*p).await.get("expire").map_or(0, |s| s.0);
            }
            n
        };
        assert_eq!(calls_expire, 90, "valkey={valkey}");
    }
}

#[tokio::test]
async fn the_command_line_finds_keys_from_real_command_info_and_routes_get_to_the_owner() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        let t = targets(&c).await;
        let ports: Vec<u16> = t.nodes.iter().map(|n| n.endpoint.port).collect();
        let o = owner(&t, "foo");
        let mut a = c.admin(ports[o]).await;
        redis::cmd("SET")
            .arg("foo")
            .arg("bar")
            .query_async::<()>(&mut a)
            .await
            .unwrap();
        let (h, rx) = start(&t);
        let mut live = Live { h, rx, next: 0 };
        let words = |s: &str| -> Vec<Vec<u8>> { s.split(' ').map(|w| w.into()).collect() };
        // MGET across slots: both keys found (key specs, or first/last/step on 6.2), in
        // different slots, so the panel refuses it before sending.
        let mget = info(&mut live, "mget").await;
        let Keys::Found(keys) = cmdline::command_keys(&words("MGET foo bar"), &mget) else {
            panic!("valkey={valkey}: keys of MGET");
        };
        let slots: Vec<u16> = keys
            .iter()
            .map(|k| redis_lens::conn::slot::key_slot(k))
            .collect();
        assert_ne!(slots[0], slots[1]);
        let ping = info(&mut live, "ping").await;
        assert_eq!(cmdline::command_keys(&words("PING"), &ping), Keys::None);
        // GET to its owner: run there, nowhere else.
        let get = info(&mut live, "get").await;
        assert_eq!(
            cmdline::command_keys(&words("GET foo"), &get),
            Keys::Found(vec![b"foo".to_vec()])
        );
        c.reset_stats().await;
        let r = live
            .ask(
                Dest::key("foo"),
                LiveRequest::Command(CmdRequest::Run {
                    args: Args::new(words("GET foo")),
                    info: get,
                    confirmed: false,
                }),
            )
            .await;
        let Ok(LiveReply::Command(CmdReply::Ran(out))) = r else {
            panic!("valkey={valkey}: {r:?}");
        };
        assert_eq!(out.lines(), ["\"bar\""], "valkey={valkey}");
        let gets = calls(&c, &ports, "get").await;
        assert_eq!(gets[o], 1, "valkey={valkey}: {gets:?}");
        assert_eq!(gets.iter().sum::<u64>(), 1, "valkey={valkey}: {gets:?}");
    }
}

/// `COMMAND INFO <name>` on node 0.
async fn info(live: &mut Live, name: &str) -> cmdline::CommandInfo {
    let req = LiveRequest::Command(CmdRequest::Info { name: name.into() });
    match live.ask(Dest::node(0), req).await {
        Ok(LiveReply::Command(CmdReply::Info(Some(i)))) => i,
        other => panic!("{name}: {other:?}"),
    }
}

/// A read-only profile: the gate grants no write, and the reads of every view on every
/// node send no write command anywhere.
#[tokio::test]
async fn a_read_only_profile_sends_no_write_to_any_node_from_the_views() {
    const WRITES: [&str; 14] = [
        "set", "del", "unlink", "hset", "expire", "persist", "renamenx", "xadd", "xack", "xtrim",
        "spublish", "publish", "flushall", "flushdb",
    ];
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        c.seed(&carts(30)).await;
        let t = targets(&c).await;
        let (h, rx) = start(&t);
        let mut live = Live { h, rx, next: 0 };
        let ro = profile(c.seed_port(), Env::Prod, None);
        assert!(ro.is_read_only());
        for op in [
            WriteOp::SPublish {
                channel: "foo".into(),
                message: Raw::from("x"),
            },
            WriteOp::Unlink { key: "foo".into() },
            WriteOp::Bulk {
                pattern: "user:{id}:cart".into(),
                glob: "user:*:cart".into(),
                action: BulkAction::Delete,
            },
        ] {
            assert!(
                Permit::grant(
                    op,
                    &ro,
                    &Settings::default(),
                    &Refusals::default(),
                    &Answer::Yes
                )
                .is_err(),
                "valkey={valkey}"
            );
        }
        c.reset_stats().await;
        for i in 0..3 {
            walk_node(&mut live, i).await;
            count_node(&mut live, i, "user:{id}:cart").await;
            let _ = live
                .ask(never(i), LiveRequest::PubSub(PubSubRequest::Counts))
                .await;
            let _ = live
                .ask(never(i), LiveRequest::PubSub(PubSubRequest::ShardCounts))
                .await;
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
