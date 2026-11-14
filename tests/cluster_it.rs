//! Cluster discovery and scanning against real clusters (Redis and Valkey), each in
//! one container with 3 primaries (plus 3 replicas where needed) announcing 127.0.0.1.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;

use common::cluster::{TestCluster, cluster, cluster_with};
use redis_lens::conn::cluster::{
    ClusterTargets, REPLICA_FALLBACK_PREFIX, SHARDS_UNREADABLE, ScanTargets, connect_targets,
};
use redis_lens::model::{
    Auth, Credentials, Endpoint, Env, NodeRole, NodeStatus, PERM_MEMORY, PERM_SCAN, PERM_TYPE_TTL,
    Profile, ScanReport,
};
use redis_lens::scan::ScanOptions;
use redis_lens::scan::cluster::scan_cluster;
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;

const USERS: u64 = 1_500;
const SESSIONS: u64 = 900;
const CARTS: u64 = 600;
const TOTAL: u64 = USERS + SESSIONS + CARTS;

fn profile(port: u16) -> Profile {
    Profile {
        name: "cluster".into(),
        env: Env::Local,
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

fn keychain_profile(port: u16, user: &str, password: &str) -> (Profile, MemoryStore) {
    let mut p = profile(port);
    p.auth = Auth::Keychain;
    let store = MemoryStore::default();
    store
        .set(
            &p.secret_ref(),
            &Credentials {
                username: Some(user.into()),
                password: SecretString::from(password.to_string()),
            },
        )
        .unwrap();
    (p, store)
}

async fn targets(p: &Profile, store: &dyn SecretStore) -> ClusterTargets {
    match connect_targets(p, store).await {
        Ok(ScanTargets::Cluster(t)) => t,
        Ok(ScanTargets::Single { .. }) => {
            panic!("a cluster node was taken for a standalone server")
        }
        Err(e) => panic!("connect failed: {e}"),
    }
}

fn keys() -> Vec<(String, String)> {
    let mut out = Vec::new();
    out.extend((0..USERS).map(|i| (format!("user:{i}"), "x".repeat(20))));
    out.extend((0..SESSIONS).map(|i| (format!("session:{i}:data"), "y".repeat(50))));
    out.extend((0..CARTS).map(|i| (format!("cart:{i}"), "z".repeat(5))));
    out
}

async fn scan(t: &mut ClusterTargets) -> ScanReport {
    let perms = t.check_permissions().await;
    assert!(perms.allowed(PERM_SCAN), "{perms:?}");
    let opts = ScanOptions::for_profile(&profile(0));
    scan_cluster("cluster", t, &opts, None, None)
        .await
        .expect("cluster scan")
}

async fn dbsize(c: &TestCluster, port: u16) -> u64 {
    let mut a = c.admin(port).await;
    redis::cmd("DBSIZE").query_async(&mut a).await.unwrap()
}

/// What redis-lens may send: read-only commands, the connection handshake, and the
/// test's own `INFO commandstats`. Names as Redis 7+ (`a|b`) and 6.2 (`a`) list them.
const READ_ONLY: [&str; 20] = [
    "info",
    "dbsize",
    "scan",
    "type",
    "pttl",
    "memory|usage",
    "memory|stats",
    "memory",
    "object|freq",
    "object",
    "cluster|shards",
    "cluster|slots",
    "cluster|myid",
    "cluster",
    "client|setinfo",
    "client",
    "hello",
    "ping",
    "auth",
    "readonly",
];

/// Discovery, a seeded scan whose estimates are exact, patterns merged over the slots,
/// no `KEYS` and no write, and `db = 1` refused.
async fn discovery_and_seeded_scan(valkey: bool) {
    let c = cluster(valkey, false).await;
    c.seed(&keys()).await;

    // db 1 is refused, before any other node is connected to.
    let mut db1 = profile(c.seed_port());
    db1.db = 1;
    let err = connect_targets(&db1, &MemoryStore::default())
        .await
        .err()
        .expect("db 1 on a cluster is refused");
    assert!(err.to_string().contains("only has database 0"), "{err}");

    c.reset_stats().await;
    let before: Vec<HashMap<String, (u64, u64)>> = {
        let mut v = Vec::new();
        for p in &c.ports {
            v.push(c.commandstats(*p).await);
        }
        v
    };

    let mut t = targets(&profile(c.seed_port()), &MemoryStore::default()).await;
    assert_eq!(t.nodes.len(), 3);
    assert_eq!(t.summary(), "cluster · 3 shards");
    let mut slots: Vec<(u16, u16)> = t.nodes.iter().flat_map(|n| n.slots.clone()).collect();
    slots.sort_unstable();
    assert_eq!(slots.first().map(|s| s.0), Some(0));
    assert_eq!(slots.last().map(|s| s.1), Some(16_383));
    // In slot order, every primary connected, the seed among them.
    assert!(t.nodes.windows(2).all(|w| w[0].slots[0] < w[1].slots[0]));
    for n in &t.nodes {
        assert_eq!(n.role, NodeRole::Primary);
        assert_eq!(n.id, n.primary_id);
        assert_eq!(n.id.len(), 40);
        assert!(n.conn.is_some(), "{:?}", n.note);
        assert_eq!(n.note, None);
        assert!(c.ports.contains(&n.endpoint.port));
    }

    let r = scan(&mut t).await;
    assert!(r.complete);
    assert_eq!(r.server.total_keys, TOTAL);
    assert_eq!(r.sampled_keys, TOTAL);
    let nodes = r.nodes.as_ref().expect("per-node results");
    assert_eq!(nodes.len(), 3);
    let mut sum = 0;
    for n in nodes {
        let size = dbsize(&c, n.endpoint.port).await;
        assert_eq!(n.status, NodeStatus::Scanned);
        assert!(n.complete);
        assert_eq!(n.sampled_keys, size);
        assert_eq!(n.server.as_ref().unwrap().total_keys, size);
        assert!(size > 0);
        sum += size;
    }
    assert_eq!(sum, TOTAL);

    // Patterns spread over every slot merge, and the node rows add up to them.
    for (name, want) in [
        ("user:{id}", USERS),
        ("session:{id}:data", SESSIONS),
        ("cart:{id}", CARTS),
    ] {
        let merged = r
            .patterns
            .iter()
            .find(|p| p.pattern == name)
            .unwrap_or_else(|| panic!("{name} in {:?}", r.patterns));
        assert_eq!(merged.est_keys, want, "{name}");
        assert_eq!(merged.sampled_keys, want, "{name}");
        let rows: Vec<_> = nodes
            .iter()
            .map(|n| {
                n.patterns
                    .iter()
                    .find(|p| p.pattern == name)
                    .expect("on every node")
            })
            .collect();
        assert_eq!(rows.iter().map(|p| p.est_keys).sum::<u64>(), want);
        assert_eq!(
            rows.iter().map(|p| p.est_bytes.unwrap_or(0)).sum::<u64>(),
            merged.est_bytes.unwrap_or(0)
        );
    }
    assert_eq!(r.patterns.len(), 3, "{:?}", r.patterns);

    // Only read-only commands, never KEYS, on every node.
    for (p, before) in c.ports.iter().zip(&before) {
        let after = c.commandstats(*p).await;
        for (name, (calls, _)) in &after {
            let new = calls - before.get(name).map_or(0, |b| b.0);
            if new > 0 {
                assert!(
                    READ_ONLY.contains(&name.as_str()),
                    "node {p} got {new} x {name}"
                );
            }
        }
        assert!(!after.contains_key("keys"), "KEYS sent to {p}");
    }

    // prefer_replica on a cluster without replicas: every shard falls back to its
    // primary, with a note saying why.
    let mut wants_replica = profile(c.seed_port());
    wants_replica.prefer_replica = true;
    let t = targets(&wants_replica, &MemoryStore::default()).await;
    assert_eq!(t.summary(), "cluster · 3 shards · replicas 0/3");
    for n in &t.nodes {
        assert_eq!(n.role, NodeRole::Primary);
        assert!(n.conn.is_some());
        let note = n.note.as_deref().unwrap_or_default();
        assert!(note.starts_with(REPLICA_FALLBACK_PREFIX), "{note}");
        assert!(note.contains("no online replica"), "{note}");
    }
}

/// `prefer_replica`: every shard is scanned on a verified replica.
async fn prefer_replica(valkey: bool) {
    let c = cluster(valkey, true).await;
    c.seed(&keys()).await;
    let mut p = profile(c.seed_port());
    p.prefer_replica = true;
    let mut t = targets(&p, &MemoryStore::default()).await;
    assert_eq!(t.nodes.len(), 3);
    let notes: Vec<_> = t.nodes.iter().map(|n| n.note.clone()).collect();
    assert_eq!(
        t.summary(),
        "cluster · 3 shards · replicas 3/3",
        "{notes:?}"
    );
    for n in &mut t.nodes {
        assert_eq!(n.role, NodeRole::Replica, "{:?}", n.note);
        assert_ne!(n.id, n.primary_id);
        let conn = n.conn.as_mut().unwrap();
        let info: String = redis::cmd("INFO")
            .arg("replication")
            .query_async(conn)
            .await
            .unwrap();
        assert!(info.contains("role:slave"), "{info}");
        let id: String = redis::cmd("CLUSTER")
            .arg("MYID")
            .query_async(conn)
            .await
            .unwrap();
        assert_eq!(id, n.id);
    }
    let r = scan(&mut t).await;
    assert!(r.complete);
    assert_eq!(r.sampled_keys, TOTAL);
    let nodes = r.nodes.unwrap();
    assert!(nodes.iter().all(|n| n.role == NodeRole::Replica));
    assert!(nodes.iter().all(|n| n.status == NodeStatus::Scanned));

    // Without the flag, the same cluster is scanned on its primaries.
    let mut t = targets(&profile(c.seed_port()), &MemoryStore::default()).await;
    assert_eq!(t.summary(), "cluster · 3 shards");
    assert!(t.nodes.iter().all(|n| n.role == NodeRole::Primary));
    assert_eq!(scan(&mut t).await.sampled_keys, TOTAL);
}

/// A stopped node gives `Failed`, and the rest is still scanned.
async fn stopped_node(valkey: bool) {
    let c = cluster(valkey, false).await;
    c.seed(&keys()).await;
    let t = targets(&profile(c.seed_port()), &MemoryStore::default()).await;
    let victim = t
        .nodes
        .iter()
        .map(|n| n.endpoint.port)
        .find(|p| *p != c.seed_port())
        .unwrap();
    let lost = dbsize(&c, victim).await;
    drop(t);
    c.stop_node(victim).await;

    let mut t = targets(&profile(c.seed_port()), &MemoryStore::default()).await;
    assert_eq!(t.nodes.len(), 3);
    assert_eq!(t.summary(), "cluster · 3 shards · 1 unreachable");
    let down = t.nodes.iter().find(|n| n.endpoint.port == victim).unwrap();
    assert!(down.conn.is_none());
    // A node discovery could not use is never one the credentials may go to: Monitor
    // and the Pub/Sub tail open nothing to it.
    assert!(!down.credentials);
    assert!(
        t.nodes
            .iter()
            .filter(|n| n.conn.is_some())
            .all(|n| n.credentials)
    );
    let note = down.note.clone().unwrap();
    assert!(
        note.contains("could not connect") || note.contains("timed out"),
        "{note}"
    );

    let r = scan(&mut t).await;
    assert!(!r.complete);
    assert_eq!(r.sampled_keys, TOTAL - lost);
    let nodes = r.nodes.unwrap();
    for n in &nodes {
        if n.endpoint.port == victim {
            assert_eq!(n.status, NodeStatus::Failed);
            assert_eq!(n.note.as_deref(), Some(note.as_str()));
            assert_eq!(n.sampled_keys, 0);
            assert_eq!(n.server, None);
        } else {
            assert_eq!(n.status, NodeStatus::Scanned);
            assert!(n.complete);
        }
    }
}

/// Keys under one hash tag leave two shards empty. The one that doesn't own the probe
/// key's slot answers `MOVED` to the permission probe, which must not turn sizes off.
async fn empty_shards(valkey: bool) {
    let c = cluster(valkey, false).await;
    let tagged: Vec<(String, String)> = (0..300)
        .map(|i| (format!("{{tag}}:item:{i}"), "x".repeat(20)))
        .collect();
    c.seed(&tagged).await;
    let mut t = targets(&profile(c.seed_port()), &MemoryStore::default()).await;
    let mut empty = 0;
    for n in &t.nodes {
        if dbsize(&c, n.endpoint.port).await == 0 {
            empty += 1;
        }
    }
    assert_eq!(empty, 2, "one hash tag fills one shard of three");
    let perms = t.check_permissions().await;
    assert!(perms.allowed(PERM_MEMORY), "{perms:?}");
    assert!(perms.allowed(PERM_TYPE_TTL), "{perms:?}");
    let r = scan(&mut t).await;
    assert!(r.sizes_available, "an empty shard turned sizes off");
    assert!(r.complete);
    assert_eq!(r.sampled_keys, 300);
}

/// An ACL user denied `CLUSTER SHARDS` falls back to `CLUSTER SLOTS`; one denied both
/// gets the seed alone, with a note.
async fn acl_fallbacks(valkey: bool) {
    // Redis 6.2 has no CLUSTER SHARDS and no `-command|subcommand` rules: the fallback
    // happens anyway, and `-cluster` denies every CLUSTER subcommand.
    let old = !valkey && common::redis_image().1.starts_with("6.");
    let mut args: Vec<&str> = vec!["--user", "lens", "on", ">lens-pw", "~*", "&*", "+@all"];
    if !old {
        args.push("-cluster|shards");
    }
    args.extend(["--user", "blind", "on", ">blind-pw", "~*", "&*", "+@all"]);
    if old {
        args.push("-cluster");
    } else {
        args.extend(["-cluster|shards", "-cluster|slots"]);
    }
    let c = cluster_with(valkey, false, &args).await;
    assert_eq!(c.is_redis6().await, old);
    c.seed(&keys()).await;

    c.reset_stats().await;
    let (p, store) = keychain_profile(c.seed_port(), "lens", "lens-pw");
    let mut t = targets(&p, &store).await;
    assert_eq!(t.nodes.len(), 3);
    assert!(t.nodes.iter().all(|n| n.conn.is_some()));
    let stats = c.commandstats(c.seed_port()).await;
    if old {
        assert!(stats.get("cluster").is_some_and(|s| s.0 >= 2), "{stats:?}");
    } else {
        assert!(
            stats.get("cluster|slots").is_some_and(|s| s.0 >= 1),
            "{stats:?}"
        );
        assert!(
            stats.get("cluster|shards").is_some_and(|s| s.1 >= 1),
            "CLUSTER SHARDS was not tried: {stats:?}"
        );
    }
    let r = scan(&mut t).await;
    assert!(r.complete);
    assert_eq!(r.sampled_keys, TOTAL);

    let (p, store) = keychain_profile(c.seed_port(), "blind", "blind-pw");
    let mut t = targets(&p, &store).await;
    assert_eq!(t.nodes.len(), 1);
    assert_eq!(t.nodes[0].note.as_deref(), Some(SHARDS_UNREADABLE));
    assert_eq!(t.nodes[0].endpoint.port, c.seed_port());
    assert!(t.nodes[0].conn.is_some());
    assert_eq!(t.summary(), "cluster · seed node only (shards unknown)");
    let r = scan(&mut t).await;
    assert!(!r.complete, "one node of three is not the whole cluster");
    assert_eq!(r.sampled_keys, dbsize(&c, c.seed_port()).await);
    let nodes = r.nodes.unwrap();
    assert_eq!(nodes[0].note.as_deref(), Some(SHARDS_UNREADABLE));
    assert!(
        !nodes[0]
            .note
            .as_deref()
            .unwrap()
            .starts_with(REPLICA_FALLBACK_PREFIX)
    );
}

#[tokio::test]
async fn redis_discovery_and_seeded_scan() {
    discovery_and_seeded_scan(false).await;
}

#[tokio::test]
async fn valkey_discovery_and_seeded_scan() {
    discovery_and_seeded_scan(true).await;
}

#[tokio::test]
async fn redis_prefer_replica() {
    prefer_replica(false).await;
}

#[tokio::test]
async fn valkey_prefer_replica() {
    prefer_replica(true).await;
}

#[tokio::test]
async fn redis_stopped_node() {
    stopped_node(false).await;
}

#[tokio::test]
async fn valkey_stopped_node() {
    stopped_node(true).await;
}

#[tokio::test]
async fn redis_acl_fallbacks() {
    acl_fallbacks(false).await;
}

#[tokio::test]
async fn valkey_acl_fallbacks() {
    acl_fallbacks(true).await;
}

#[tokio::test]
async fn redis_empty_shards() {
    empty_shards(false).await;
}

#[tokio::test]
async fn valkey_empty_shards() {
    empty_shards(true).await;
}
