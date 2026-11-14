//! `compare` against real servers: two seeded instances, the same one on both sides,
//! and a snapshot file for one side.
#![cfg(feature = "integration-tests")]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use redis_lens::cli::CompareArgs;
use redis_lens::cli::compare::{compare_profiles, render_compare};
use redis_lens::config::Config;
use redis_lens::diff::{Presence, TtlMismatch};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::secrets::MemoryStore;
use redis_lens::snapshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Seeded into values; must never show up in the output.
const MARKER: &str = "VALUE-MARKER-456";

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("redis-lens-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn profile(name: &str, env: Env, port: u16) -> Profile {
    Profile {
        name: name.to_string(),
        env,
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
        tags: Vec::new(),
    }
}

/// `staging` (a stage profile) and `prod` (a prod profile, so read-only with the default
/// rate limit), one per server.
fn temp_config(staging_port: u16, prod_port: u16) -> PathBuf {
    let path = temp_dir("compare-it-config").join("config.toml");
    Config {
        profiles: vec![
            profile("staging", Env::Stage, staging_port),
            profile("prod", Env::Prod, prod_port),
        ],
        ..Default::default()
    }
    .save(&path)
    .unwrap();
    path
}

fn args(left: &str, right: &str) -> CompareArgs {
    CompareArgs {
        left: left.to_string(),
        right: right.to_string(),
        left_file: None,
        right_file: None,
        sample: 10_000,
        max_rate: None,
        json: false,
    }
}

async fn set(
    conn: &mut redis::aio::MultiplexedConnection,
    key: String,
    value: String,
    ttl: Option<u64>,
) {
    let mut cmd = redis::cmd("SET");
    cmd.arg(key).arg(value);
    if let Some(s) = ttl {
        cmd.arg("EX").arg(s);
    }
    let _: () = cmd.query_async(conn).await.unwrap();
}

/// Writes the server has seen since its last save: any write `compare` sent would show.
async fn changes(conn: &mut redis::aio::MultiplexedConnection) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("persistence")
        .query_async(conn)
        .await
        .unwrap();
    info.lines()
        .find_map(|l| l.strip_prefix("rdb_changes_since_last_save:"))
        .and_then(|v| v.trim().parse().ok())
        .expect("rdb_changes_since_last_save in INFO persistence")
}

/// Staging: carts that expire, sessions, and debug keys only it has. Prod: four times the
/// carts, none expiring (the bug), sessions twice the size, and audit keys only it has.
async fn seed(
    staging: &mut redis::aio::MultiplexedConnection,
    prod: &mut redis::aio::MultiplexedConnection,
) {
    for i in 0..50 {
        set(
            staging,
            format!("cart:{}", 1000 + i),
            format!("{MARKER}{}", "c".repeat(200)),
            Some(3600),
        )
        .await;
    }
    for i in 0..200 {
        set(
            prod,
            format!("cart:{}", 1000 + i),
            format!("{MARKER}{}", "c".repeat(200)),
            None,
        )
        .await;
    }
    for i in 0..40 {
        set(
            staging,
            format!("session:{}", 5000 + i),
            "s".repeat(1000),
            Some(600),
        )
        .await;
        set(
            prod,
            format!("session:{}", 5000 + i),
            "s".repeat(2000),
            Some(600),
        )
        .await;
    }
    for i in 0..10 {
        set(staging, format!("debug:{}", 1 + i), "d".into(), None).await;
        set(prod, format!("audit:{}", 1 + i), "a".repeat(50), None).await;
    }
}

async fn two_seeded_servers(valkey: bool) {
    let (s, p) = (
        common::server_with(valkey, &[]).await,
        common::server_with(valkey, &[]).await,
    );
    let (mut sa, mut pa) = (s.admin().await, p.admin().await);
    seed(&mut sa, &mut pa).await;
    let config = temp_config(s.port, p.port);
    let secrets = MemoryStore::default();
    let (before_s, before_p) = (changes(&mut sa).await, changes(&mut pa).await);

    let (c, left, right) = compare_profiles(&config, &secrets, &args("staging", "prod"), None)
        .await
        .unwrap();
    let text = render_compare(&c, "staging", "prod", 120);
    println!("{text}");

    // Nothing was written to either server.
    assert_eq!(changes(&mut sa).await, before_s, "compare wrote to staging");
    assert_eq!(changes(&mut pa).await, before_p, "compare wrote to prod");
    assert_eq!(
        (left.profile.as_str(), right.profile.as_str()),
        ("staging", "prod")
    );
    assert!(left.complete && right.complete);
    assert_eq!(c.left.sample_fraction, Some(1.0));
    assert_eq!(c.right.sample_fraction, Some(1.0));
    assert!(c.same_keyspace);

    let cart = c.pattern("cart:{id}").unwrap();
    assert_eq!(cart.presence, Presence::Both);
    assert_eq!(cart.ttl_mismatch, Some(TtlMismatch::NoTtlOnRight));
    assert_eq!(cart.key_ratio, Some(4.0));
    let avg = cart.avg_size_ratio.unwrap();
    assert!((0.9..1.1).contains(&avg), "cart average size ratio {avg}");
    let session = c.pattern("session:{id}").unwrap();
    assert_eq!(session.ttl_mismatch, None);
    assert_eq!(session.key_ratio, Some(1.0));
    let avg = session.avg_size_ratio.unwrap();
    assert!(
        (1.5..2.5).contains(&avg),
        "session average size ratio {avg}"
    );
    assert_eq!(
        c.pattern("debug:{id}").unwrap().presence,
        Presence::LeftOnly
    );
    assert_eq!(
        c.pattern("audit:{id}").unwrap().presence,
        Presence::RightOnly
    );
    // The likely bug leads.
    assert_eq!(c.patterns[0].pattern, "cart:{id}");
    assert!(text.contains("Likely bugs: 1 pattern(s)"), "{text}");
    assert!(text.contains("no TTL on prod, set on staging"), "{text}");
    assert!(text.contains("only on staging") && text.contains("only on prod"));
    assert!(!text.contains(MARKER), "a value leaked into the output");
    let json = serde_json::to_string(&c).unwrap();
    assert!(!json.contains(MARKER), "a value leaked into the JSON");

    // The same profile on both sides: everything unchanged.
    let (same, _, _) = compare_profiles(&config, &secrets, &args("prod", "prod"), None)
        .await
        .unwrap();
    assert!(!same.patterns.is_empty());
    for p in &same.patterns {
        assert_eq!(p.presence, Presence::Both, "{}", p.pattern);
        assert_eq!(p.key_ratio, Some(1.0), "{}", p.pattern);
        assert_eq!(p.avg_size_ratio, Some(1.0), "{}", p.pattern);
        assert_eq!(p.ttl_mismatch, None, "{}", p.pattern);
    }
    let text = render_compare(&same, "prod", "prod", 120);
    assert!(
        text.contains("No pattern has a TTL on one side only."),
        "{text}"
    );
    assert_eq!(changes(&mut pa).await, before_p, "compare wrote to prod");

    // A snapshot file for one side gives the same answer as scanning it.
    let dir = temp_dir("compare-it-snaps");
    let file = snapshot::save(&dir, &left).unwrap();
    let mut from_file = args("staging", "prod");
    from_file.left_file = Some(file);
    let (f, l2, _) = compare_profiles(&config, &secrets, &from_file, None)
        .await
        .unwrap();
    assert_eq!(l2, left);
    let presence = |c: &redis_lens::diff::Comparison| {
        c.patterns
            .iter()
            .map(|p| (p.pattern.clone(), p.presence, p.ttl_mismatch, p.key_ratio))
            .collect::<Vec<_>>()
    };
    assert_eq!(presence(&f), presence(&c));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn compare_two_seeded_redis_servers() {
    two_seeded_servers(false).await;
}

#[tokio::test]
async fn compare_two_seeded_valkey_servers() {
    two_seeded_servers(true).await;
}

/// A Redis on one side and a Valkey on the other: two different servers, two flavours.
#[tokio::test]
async fn compare_redis_with_valkey() {
    let (s, p) = (common::redis().await, common::valkey().await);
    let (mut sa, mut pa) = (s.admin().await, p.admin().await);
    seed(&mut sa, &mut pa).await;
    let config = temp_config(s.port, p.port);
    let (c, left, right) = compare_profiles(
        &config,
        &MemoryStore::default(),
        &args("staging", "prod"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        (left.server.flavor, right.server.flavor),
        (
            redis_lens::model::ServerFlavor::Redis,
            redis_lens::model::ServerFlavor::Valkey
        )
    );
    assert_eq!(
        c.pattern("cart:{id}").unwrap().ttl_mismatch,
        Some(TtlMismatch::NoTtlOnRight)
    );
}
