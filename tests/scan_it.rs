//! Scanner against real Redis and Valkey servers.
#![cfg(feature = "integration-tests")]

mod common;

use redis::AsyncCommands;
use redis_lens::model::{
    KeyType, PERM_FREQ, PERM_INFO, PERM_MEMORY, PERM_SCAN, PERM_TYPE_TTL, PermCheck, PermStatus,
    PermissionReport, ScanReport,
};
use redis_lens::scan::{ScanOptions, sample_keys, scan};

fn perms(denied: &[&str]) -> PermissionReport {
    PermissionReport {
        checks: [PERM_SCAN, PERM_TYPE_TTL, PERM_MEMORY, PERM_FREQ, PERM_INFO]
            .into_iter()
            .map(|label| PermCheck {
                label,
                status: if denied.contains(&label) {
                    PermStatus::Denied("NOPERM".into())
                } else {
                    PermStatus::Allowed
                },
            })
            .collect(),
    }
}

async fn seed(conn: &mut redis::aio::MultiplexedConnection) {
    let mut pipe = redis::pipe();
    for n in 0..300u32 {
        let key = format!("user:{}:cart", 1000 + n);
        pipe.hset(&key, "items", n).ignore();
        if n % 3 != 0 {
            pipe.expire(&key, 3600).ignore();
        }
    }
    for n in 0..200u64 {
        let key = format!("session:{n:08x}-4b3d-4e0a-9c61-{n:012x}");
        pipe.set_ex(key, "payload", 600).ignore();
    }
    let _: () = pipe.query_async(conn).await.unwrap();
    let chunk = "x".repeat(1024);
    for _ in 0..5 {
        let items: Vec<&str> = vec![chunk.as_str(); 1000];
        let _: () = conn.rpush("queue:big", items).await.unwrap();
    }
}

fn by_pattern<'a>(r: &'a ScanReport, p: &str) -> &'a redis_lens::model::PatternStats {
    r.patterns
        .iter()
        .find(|s| s.pattern == p)
        .unwrap_or_else(|| panic!("no pattern {p}"))
}

async fn full_scan(server: common::TestServer) {
    let mut conn = server.admin().await;
    seed(&mut conn).await;
    let report = scan("t", &mut conn, &ScanOptions::default(), &perms(&[]), None)
        .await
        .unwrap();
    assert_eq!(report.sampled_keys, 501);
    assert!(report.sizes_available);
    assert_eq!(report.patterns.len(), 3);
    let cart = by_pattern(&report, "user:{id}:cart");
    assert_eq!(cart.sampled_keys, 300);
    assert_eq!(cart.est_keys, 300);
    assert_eq!(cart.no_ttl, 100);
    assert_eq!(cart.key_type, Some(KeyType::Hash));
    assert!(cart.sampled_bytes.is_some());
    let sess = by_pattern(&report, "session:{uuid}");
    assert_eq!(sess.sampled_keys, 200);
    assert_eq!(sess.no_ttl, 0);
    assert_eq!(sess.key_type, Some(KeyType::String));
    let big = by_pattern(&report, "queue:big");
    assert_eq!(big.sampled_keys, 1);
    assert_eq!(big.key_type, Some(KeyType::List));
    assert!(big.size_max.unwrap() > 5_000_000);
    assert_eq!(report.patterns[0].pattern, "queue:big");
    assert_eq!(report.server.total_keys, 501);
}

async fn no_memory_perm(server: common::TestServer) {
    let mut conn = server.admin().await;
    seed(&mut conn).await;
    let report = scan(
        "t",
        &mut conn,
        &ScanOptions::default(),
        &perms(&[PERM_MEMORY]),
        None,
    )
    .await
    .unwrap();
    assert!(!report.sizes_available);
    assert_eq!(report.sampled_keys, 501);
    for p in &report.patterns {
        assert_eq!(p.sampled_bytes, None, "{}", p.pattern);
        assert_eq!(p.est_bytes, None);
        assert!(p.examples.iter().all(|e| e.bytes.is_none()));
    }
}

async fn limited(server: common::TestServer) {
    let mut conn = server.admin().await;
    seed(&mut conn).await;
    let opts = ScanOptions {
        sample_limit: 100,
        ..ScanOptions::default()
    };
    let (tx, rx) = tokio::sync::watch::channel(Default::default());
    let report = scan("t", &mut conn, &opts, &perms(&[]), Some(&tx))
        .await
        .unwrap();
    assert_eq!(report.sampled_keys, 100);
    let total: u64 = report.patterns.iter().map(|p| p.sampled_keys).sum();
    assert_eq!(total, 100);
    let scale = 501.0 / 100.0;
    for p in &report.patterns {
        assert_eq!(p.est_keys, (p.sampled_keys as f64 * scale).round() as u64);
    }
    let est: u64 = report.patterns.iter().map(|p| p.est_keys).sum();
    assert!((495..=507).contains(&est), "est {est}");
    let last = *rx.borrow();
    assert!(last.done);
    assert_eq!(last.sampled, 100);
    assert_eq!(last.target, 100);
}

async fn small_batches(server: common::TestServer) {
    let mut conn = server.admin().await;
    seed(&mut conn).await;
    let opts = ScanOptions {
        scan_count: 50,
        batch_size: 64,
        max_commands_per_sec: Some(100_000),
        match_glob: Some("session:*".into()),
        ..ScanOptions::default()
    };
    let samples = sample_keys(&mut conn, &opts, true, false, None)
        .await
        .unwrap();
    assert_eq!(samples.len(), 200);
    let mut keys: Vec<_> = samples.iter().map(|s| s.key.clone()).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 200);
    assert!(samples.iter().all(|s| s.bytes.is_some()));
}

#[tokio::test]
async fn full_scan_redis() {
    full_scan(common::redis().await).await;
}

#[tokio::test]
async fn full_scan_valkey() {
    full_scan(common::valkey().await).await;
}

#[tokio::test]
async fn no_memory_perm_redis() {
    no_memory_perm(common::redis().await).await;
}

#[tokio::test]
async fn no_memory_perm_valkey() {
    no_memory_perm(common::valkey().await).await;
}

#[tokio::test]
async fn sample_limit_redis() {
    limited(common::redis().await).await;
}

#[tokio::test]
async fn sample_limit_valkey() {
    limited(common::valkey().await).await;
}

#[tokio::test]
async fn small_batches_redis() {
    small_batches(common::redis().await).await;
}

#[tokio::test]
async fn small_batches_valkey() {
    small_batches(common::valkey().await).await;
}

#[tokio::test]
async fn lfu_freq_is_read() {
    let server = common::redis_with(&["--maxmemory-policy", "allkeys-lfu"]).await;
    let mut conn = server.admin().await;
    seed(&mut conn).await;
    let report = scan("t", &mut conn, &ScanOptions::default(), &perms(&[]), None)
        .await
        .unwrap();
    assert!(report.server.lfu_enabled());
    assert!(
        report
            .patterns
            .iter()
            .flat_map(|p| &p.examples)
            .all(|e| e.lfu_freq.is_some())
    );
    // Without the FREQ permission the field stays None.
    let report = scan(
        "t",
        &mut conn,
        &ScanOptions::default(),
        &perms(&[PERM_FREQ]),
        None,
    )
    .await
    .unwrap();
    assert!(
        report
            .patterns
            .iter()
            .flat_map(|p| &p.examples)
            .all(|e| e.lfu_freq.is_none())
    );
}

#[tokio::test]
async fn freq_is_none_without_lfu_policy() {
    let server = common::redis().await;
    let mut conn = server.admin().await;
    seed(&mut conn).await;
    let report = scan("t", &mut conn, &ScanOptions::default(), &perms(&[]), None)
        .await
        .unwrap();
    assert!(
        report
            .patterns
            .iter()
            .flat_map(|p| &p.examples)
            .all(|e| e.lfu_freq.is_none())
    );
}

#[tokio::test]
async fn missing_memory_usage_acl_degrades() {
    let server = common::redis_with(&[
        "--user",
        "lim",
        "on",
        ">pw",
        "~*",
        "-@all",
        "+ping",
        "+hello",
        "+client|setinfo",
        "+client|setname",
        "+info",
        "+dbsize",
        "+scan",
        "+type",
        "+pttl",
        "+select",
    ])
    .await;
    let mut admin = server.admin().await;
    seed(&mut admin).await;

    let mut conn = redis::Client::open(format!("redis://lim:pw@127.0.0.1:{}", server.port))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let samples = sample_keys(&mut conn, &ScanOptions::default(), true, false, None)
        .await
        .expect("a denied MEMORY USAGE must not fail the scan");
    assert_eq!(samples.len(), 501);
    assert!(samples.iter().all(|s| s.bytes.is_none()));
    // And the rest still works: TTLs came through.
    assert_eq!(
        samples
            .iter()
            .filter(|s| s.ttl == redis_lens::model::Ttl::Persistent)
            .count(),
        101 // 100 carts + queue:big
    );
}

/// 10,000 keys `k:0` to `k:9999`, read by a user whose ACL only allows `k:1*` (1,111
/// keys). SCAN still returns every key, so the 1,000 keys seen stand for a tenth of
/// the keyspace even though only about 111 of them can be sampled.
async fn key_acl_scale(server: &common::TestServer, user: &str) {
    let mut admin = server.admin().await;
    let mut pipe = redis::pipe();
    for n in 0..10_000u32 {
        pipe.set(format!("k:{n}"), n).ignore();
    }
    let _: () = pipe.query_async(&mut admin).await.unwrap();

    let mut conn = redis::Client::open(format!("redis://{user}:pw@127.0.0.1:{}", server.port))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let opts = ScanOptions {
        sample_limit: 1_000,
        max_commands_per_sec: None,
        ..ScanOptions::default()
    };
    let report = scan("t", &mut conn, &opts, &perms(&[]), None)
        .await
        .unwrap();
    assert_eq!(report.server.total_keys, 10_000);
    assert!(report.sampled_keys < 300, "sampled {}", report.sampled_keys);
    let est: u64 = report.patterns.iter().map(|p| p.est_keys).sum();
    assert!((800..=1_500).contains(&est), "est {est}, truth 1,111");
}

#[tokio::test]
async fn key_acl_does_not_collapse_scale_redis() {
    let server = common::redis_with(&["--user", "lim", "on", ">pw", "~k:1*", "+@all"]).await;
    key_acl_scale(&server, "lim").await;
}

#[tokio::test]
async fn key_acl_does_not_collapse_scale_valkey() {
    let server = common::valkey().await;
    let mut admin = server.admin().await;
    let _: () = redis::cmd("ACL")
        .arg(&["SETUSER", "lim", "on", ">pw", "~k:1*", "+@all"])
        .query_async(&mut admin)
        .await
        .unwrap();
    key_acl_scale(&server, "lim").await;
}

/// An empty database has a progress target of 0, and it stays 0.
async fn empty_progress(server: common::TestServer) {
    let mut conn = server.admin().await;
    let (tx, rx) = tokio::sync::watch::channel(Default::default());
    let report = scan(
        "t",
        &mut conn,
        &ScanOptions::default(),
        &perms(&[]),
        Some(&tx),
    )
    .await
    .unwrap();
    assert_eq!(report.sampled_keys, 0);
    assert!(report.patterns.is_empty());
    let last = *rx.borrow();
    assert!(last.done);
    assert_eq!(last.sampled, 0);
    assert_eq!(last.target, 0);
}

#[tokio::test]
async fn empty_db_progress_target_redis() {
    empty_progress(common::redis().await).await;
}

#[tokio::test]
async fn empty_db_progress_target_valkey() {
    empty_progress(common::valkey().await).await;
}
