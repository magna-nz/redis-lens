//! Connect-time server figures (no key read) and stopping an analysis early, against
//! real Redis and Valkey servers, and the figures of a real cluster.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::cluster::{cluster, parse_commandstats};
use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn::{self, cluster::ScanTargets};
use redis_lens::model::{
    Auth, ContextGap, Credentials, Endpoint, Env, PermissionReport, Profile, ServerMode,
};
use redis_lens::scan::figures::{cluster_figures, server_figures_with_budget};
use redis_lens::scan::{ScanOptions, ScanProgress, StopFlag, scan_with_budget};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;

const KEYS: u64 = 2_500;
const WITH_TTL: u64 = 500;

/// What a connect may send: the figures' commands, plus the test's own `CONFIG
/// RESETSTAT`, `INFO commandstats` and the handshake of the admin connections it opens
/// for them (`TestCluster::commandstats` opens one per call). Names as Redis 7+ and
/// Valkey list them (`a|b`), and `memory` as Redis 6.2 lists `MEMORY STATS`.
const SERVER_ONLY: [&str; 9] = [
    "info",
    "dbsize",
    "memory|stats",
    "memory",
    "config|resetstat",
    "config",
    "client|setinfo",
    "client",
    "hello",
];

/// Commands that read a key: none may be sent on connect.
const KEY_COMMANDS: [&str; 9] = [
    "scan",
    "keys",
    "type",
    "pttl",
    "ttl",
    "memory|usage",
    "object|freq",
    "object",
    "get",
];

fn profile(port: u16, auth: Auth) -> Profile {
    Profile {
        name: "figures".to_string(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        },
        db: 0,
        auth,
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        tags: Vec::new(),
    }
}

async fn connect(server: &common::TestServer) -> MultiplexedConnection {
    conn::connect(&profile(server.port, Auth::None), &MemoryStore::default())
        .await
        .expect("connect")
        .into_inner()
}

async fn connect_as_lens(server: &common::TestServer) -> MultiplexedConnection {
    let secrets = MemoryStore::default();
    let p = profile(server.port, Auth::Keychain);
    secrets
        .set(
            &p.secret_ref(),
            &Credentials {
                username: Some("lens".into()),
                password: SecretString::from("lens-pw"),
            },
        )
        .unwrap();
    conn::connect(&p, &secrets)
        .await
        .expect("connect as lens")
        .into_inner()
}

/// `KEYS` strings in three patterns, `WITH_TTL` of them with a TTL.
async fn seed(admin: &mut MultiplexedConnection, keys: u64) {
    let mut pipe = redis::pipe();
    for i in 0..keys {
        let key = match i % 3 {
            0 => format!("user:{i}"),
            1 => format!("session:{i}:data"),
            _ => format!("cart:{i}"),
        };
        if i < WITH_TTL {
            pipe.cmd("SET")
                .arg(&key)
                .arg("v")
                .arg("EX")
                .arg(3_600)
                .ignore();
        } else {
            pipe.cmd("SET").arg(&key).arg("v").ignore();
        }
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

/// Every command sent is a server-level one, and no key was read.
fn assert_no_key_read(stats: &HashMap<String, (u64, u64)>, what: &str) {
    for k in KEY_COMMANDS {
        assert!(!stats.contains_key(k), "{what}: {k} was sent: {stats:?}");
    }
    for name in stats.keys() {
        assert!(
            SERVER_ONLY.contains(&name.as_str()),
            "{what}: {name} was sent: {stats:?}"
        );
    }
    // The old format folds MEMORY USAGE into `memory`: only MEMORY STATS, once.
    if let Some((calls, _)) = stats.get("memory") {
        assert!(*calls <= 1, "{what}: {stats:?}");
    }
}

#[tokio::test]
async fn connect_figures_read_no_key() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        seed(&mut admin, KEYS).await;
        let mut c = connect(&server).await;
        // The permission check probes key commands: it runs before the figures.
        let perms = conn::check_permissions(&mut c).await;
        reset_stats(&mut admin).await;

        let budget = Budget::new(Some(5_000));
        let f = server_figures_with_budget(&mut c, 0, &perms, &budget)
            .await
            .expect("figures");
        let stats = commandstats(&mut admin).await;
        assert_no_key_read(&stats, what);
        assert_eq!(stats.get("dbsize").map(|s| s.0), Some(1), "{what}");
        assert_eq!(
            budget.taken(),
            6,
            "{what}: every command through the budget"
        );

        assert_eq!(f.server.total_keys, KEYS, "{what}");
        let ks = f.context.keyspace.expect("INFO keyspace");
        assert_eq!((ks.keys, ks.expires), (KEYS, WITH_TTL), "{what}");
        let m = f.context.memory.expect("MEMORY STATS");
        assert!(m.dataset_bytes.is_some_and(|b| b > 0), "{what}: {m:?}");
        // Redis 8 has INFO keysizes; Redis 7.4 and Valkey say there is none.
        assert!(
            f.context.keysizes.is_some() || f.context.missing.contains(&ContextGap::NoKeysizes),
            "{what}: {:?}",
            f.context.missing
        );
        let h = f.headline();
        assert_eq!(h.keys, KEYS);
        assert_eq!(h.with_ttl, Some(WITH_TTL));
        assert_eq!(h.ttl_share, Some(WITH_TTL as f64 / KEYS as f64));
        assert!(h.dataset_bytes.is_some() && h.overhead_bytes.is_some());
    }
}

/// A refused `MEMORY STATS` turns that figure off with a reason; the read goes on.
#[tokio::test]
async fn a_refused_memory_stats_is_a_gap_not_an_error() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(
            valkey,
            &[
                "--user",
                "lens",
                "on",
                ">lens-pw",
                "~*",
                "+@all",
                "-memory|stats",
            ],
        )
        .await;
        let mut admin = server.admin().await;
        seed(&mut admin, 30).await;
        let mut c = connect_as_lens(&server).await;
        // Without a permission report, MEMORY STATS is tried and refused.
        let f =
            server_figures_with_budget(&mut c, 0, &PermissionReport::default(), &Budget::new(None))
                .await
                .expect("figures without MEMORY STATS");
        assert_eq!(f.context.memory, None, "{what}");
        assert!(
            f.context.missing.contains(&ContextGap::MemoryStatsDenied),
            "{what}: {:?}",
            f.context.missing
        );
        assert_eq!(f.server.total_keys, 30, "{what}");
        assert!(f.context.keyspace.is_some(), "{what}");
        // The reason is redis-lens's own, never the server's text naming the user.
        for g in f.gaps() {
            assert!(!g.contains("lens") || g.contains("redis-lens"), "{g}");
        }
        assert_eq!(f.headline().dataset_bytes, None);
    }
}

/// `esc` while an analysis runs: the scan returns what it has, not complete.
#[tokio::test]
async fn a_stopped_analysis_keeps_what_it_has() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        seed(&mut admin, 5_000).await;
        let mut c = connect(&server).await;
        let perms = conn::check_permissions(&mut c).await;
        let stop = StopFlag::new();
        let opts = ScanOptions {
            sample_limit: 4_000,
            scan_count: 100,
            batch_size: 50,
            max_commands_per_sec: Some(4_000),
            stop: stop.clone(),
            ..ScanOptions::default()
        };
        // At 4,000 commands a second, 4,000 keys take about three seconds.
        let budget = Budget::new(opts.max_commands_per_sec);
        let (ptx, mut prx) = tokio::sync::watch::channel(ScanProgress::default());
        let task = tokio::spawn(async move {
            scan_with_budget("t", &mut c, &opts, &perms, Some(&ptx), None, &budget).await
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            while prx.borrow_and_update().sampled < 300 {
                prx.changed().await.expect("the scan is running");
            }
        })
        .await
        .expect("300 keys sampled in time");
        stop.stop();
        let report = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the scan stops in time")
            .unwrap()
            .expect("a report, not an error");
        assert!(!report.complete, "{what}");
        assert!(
            report.sampled_keys >= 300,
            "{what}: {}",
            report.sampled_keys
        );
        assert!(
            report.sampled_keys < 4_000,
            "{what}: {}",
            report.sampled_keys
        );
        // Scaled from the samples so far: about the whole keyspace.
        let est: u64 = report.patterns.iter().map(|p| p.est_keys).sum();
        assert!(est.abs_diff(5_000) <= 3, "{what}: est {est}");
        assert!(report.server_context.is_some(), "{what}");
        assert!(prx.borrow().done, "{what}");
    }
}

/// A cluster's figures over every shard, without sampling: no node reads a key, and
/// the sums are the cluster's.
#[tokio::test]
async fn cluster_figures_sum_every_shard_and_read_no_key() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let c = cluster(valkey, false).await;
        let keys: Vec<(String, String)> = (0..900)
            .map(|i| (format!("user:{i}"), "x".to_string()))
            .collect();
        c.seed(&keys).await;
        let mut targets = match conn::cluster::connect_targets(
            &profile(c.seed_port(), Auth::None),
            &MemoryStore::default(),
        )
        .await
        {
            Ok(ScanTargets::Cluster(t)) => t,
            Ok(ScanTargets::Single { .. }) => panic!("{what}: a cluster taken for a standalone"),
            Err(e) => panic!("{what}: connect failed: {e}"),
        };
        targets.check_permissions().await;
        c.reset_stats().await;

        let f = cluster_figures(&mut targets, Some(5_000))
            .await
            .expect("cluster figures");
        assert_eq!(f.nodes.len(), 3, "{what}");
        assert_eq!(f.answered(), 3, "{what}");
        assert_eq!(f.merged.server.mode, ServerMode::Cluster, "{what}");
        assert_eq!(f.merged.server.total_keys, 900, "{what}");
        assert_eq!(
            f.merged.context.keyspace.map(|k| k.keys),
            Some(900),
            "{what}"
        );
        let per_node: u64 = f
            .nodes
            .iter()
            .map(|n| n.figures.as_ref().unwrap().server.total_keys)
            .sum();
        assert_eq!(per_node, 900, "{what}");
        assert!(targets.nodes.iter().all(|n| n.budget.is_some()), "{what}");
        for p in &c.ports {
            let stats = c.commandstats(*p).await;
            assert_no_key_read(&stats, what);
            assert_eq!(stats.get("dbsize").map(|s| s.0), Some(1), "{what} {p}");
        }
        // The headline sums too.
        assert_eq!(f.merged.headline().keys, 900);
    }
}
