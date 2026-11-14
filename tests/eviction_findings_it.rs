//! The eviction-policy findings of an analysis, against real Redis and Valkey servers
//! started with a `maxmemory-policy`. They send no command of their own: the policy and
//! memory come from the `INFO` a scan reads first.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;

use redis::aio::MultiplexedConnection;
use redis_lens::analysis::{RecKind, Recommendation, recommendations};
use redis_lens::budget::Budget;
use redis_lens::conn;
use redis_lens::scan::{ScanOptions, scan_with_budget};

async fn seed(c: &mut MultiplexedConnection, keys: u64, with_ttl: u64) {
    let mut pipe = redis::pipe();
    for i in 0..keys {
        pipe.cmd("SET").arg(format!("item:{i}")).arg("v").ignore();
        if i < with_ttl {
            pipe.cmd("EXPIRE")
                .arg(format!("item:{i}"))
                .arg(3_600)
                .ignore();
        }
    }
    let _: () = pipe.query_async(c).await.expect("seed");
}

async fn analyze(server: &common::TestServer) -> Vec<Recommendation> {
    let mut c = server.admin().await;
    let perms = conn::check_permissions(&mut c).await;
    let opts = ScanOptions::default();
    let budget = Budget::new(opts.max_commands_per_sec);
    let report = scan_with_budget("evict", &mut c, &opts, &perms, None, None, &budget)
        .await
        .expect("a report");
    recommendations(&report, None, &HashMap::new())
}

fn kinds(recs: &[Recommendation]) -> Vec<RecKind> {
    recs.iter()
        .map(|r| r.kind)
        .filter(|k| {
            matches!(
                k,
                RecKind::VolatileNoTtl | RecKind::NoEvictionFull | RecKind::LfuOff
            )
        })
        .collect()
}

async fn server(valkey: bool, policy: &str) -> common::TestServer {
    common::server_with(valkey, &["--maxmemory-policy", policy]).await
}

#[tokio::test]
async fn a_volatile_policy_with_no_ttl_keys_is_flagged() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let s = server(valkey, "volatile-lru").await;
        seed(&mut s.admin().await, 200, 0).await;
        let recs = analyze(&s).await;
        let rec = recs
            .iter()
            .find(|r| r.kind == RecKind::VolatileNoTtl)
            .unwrap_or_else(|| panic!("{what}: {recs:?}"));
        assert!(rec.text.contains("volatile-lru"), "{what}: {}", rec.text);
        // volatile-lru is not LFU either.
        assert!(kinds(&recs).contains(&RecKind::LfuOff), "{what}");
    }
}

#[tokio::test]
async fn a_volatile_policy_with_ttl_keys_is_quiet() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let s = server(valkey, "volatile-lfu").await;
        seed(&mut s.admin().await, 200, 150).await;
        assert!(kinds(&analyze(&s).await).is_empty(), "{what}");
    }
}

#[tokio::test]
async fn noeviction_near_maxmemory_is_flagged() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let s = server(valkey, "noeviction").await;
        let mut c = s.admin().await;
        seed(&mut c, 500, 0).await;
        let info: String = redis::cmd("INFO")
            .arg("memory")
            .query_async(&mut c)
            .await
            .unwrap();
        let used: u64 = info
            .lines()
            .find_map(|l| l.strip_prefix("used_memory:"))
            .and_then(|v| v.trim().parse().ok())
            .expect("used_memory");
        // 5% above what is used: past the 90% line, with room to read.
        let _: () = redis::cmd("CONFIG")
            .arg("SET")
            .arg("maxmemory")
            .arg(used + used / 20)
            .query_async(&mut c)
            .await
            .unwrap();
        let recs = analyze(&s).await;
        assert!(
            kinds(&recs).contains(&RecKind::NoEvictionFull),
            "{what}: {recs:?}"
        );
        // With no limit it is quiet.
        let _: () = redis::cmd("CONFIG")
            .arg("SET")
            .arg("maxmemory")
            .arg(0)
            .query_async(&mut c)
            .await
            .unwrap();
        assert!(
            !kinds(&analyze(&s).await).contains(&RecKind::NoEvictionFull),
            "{what}"
        );
    }
}

#[tokio::test]
async fn lfu_off_is_flagged_with_how_to_turn_it_on_and_lfu_is_quiet() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let s = server(valkey, "allkeys-lru").await;
        seed(&mut s.admin().await, 50, 10).await;
        let recs = analyze(&s).await;
        let rec = recs
            .iter()
            .find(|r| r.kind == RecKind::LfuOff)
            .unwrap_or_else(|| panic!("{what}: {recs:?}"));
        assert!(rec.hint.contains("allkeys-lfu"), "{what}");
        let s = server(valkey, "allkeys-lfu").await;
        seed(&mut s.admin().await, 50, 10).await;
        assert!(kinds(&analyze(&s).await).is_empty(), "{what}");
    }
}
