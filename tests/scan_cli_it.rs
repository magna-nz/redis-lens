//! `redis-lens scan` against real servers.
#![cfg(feature = "integration-tests")]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use redis_lens::cli::ScanArgs;
use redis_lens::cli::scan::{render_table, report};
use redis_lens::config::Config;
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile, ScanReport};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_config(profile: &Profile) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("redis-lens-scan-it-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    Config {
        profiles: vec![profile.clone()],
    }
    .save(&path)
    .unwrap();
    path
}

fn profile(port: u16, auth: Auth) -> Profile {
    Profile {
        name: "it".to_string(),
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
    }
}

fn args() -> ScanArgs {
    ScanArgs {
        profile: "it".to_string(),
        sample: 10_000,
        match_glob: None,
        json: false,
        max_rate: None,
        save: false,
        keep: None,
        keep_days: None,
        checks: Vec::new(),
        rules: None,
        strict: false,
        report: None,
        output: None,
        examples: false,
    }
}

async fn seed(server: &common::TestServer) {
    let mut admin = server.admin().await;
    for i in 0..100 {
        let _: () = redis::cmd("SET")
            .arg(format!("user:{}:profile", 1000 + i))
            .arg("x".repeat(50))
            .query_async(&mut admin)
            .await
            .unwrap();
        let _: () = redis::cmd("SET")
            .arg(format!("session:{}", 5000 + i))
            .arg("y")
            .arg("EX")
            .arg(3600)
            .query_async(&mut admin)
            .await
            .unwrap();
        let _: () = redis::cmd("HSET")
            .arg(format!("order:{}", 9000 + i))
            .arg("f")
            .arg("v")
            .query_async(&mut admin)
            .await
            .unwrap();
    }
}

fn check_report(r: &ScanReport) {
    assert_eq!(r.sampled_keys, 300);
    assert_eq!(r.patterns.len(), 3, "{:?}", r.patterns);
    for name in ["user:{id}:profile", "session:{id}", "order:{id}"] {
        let p = r
            .patterns
            .iter()
            .find(|p| p.pattern == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(p.est_keys, 100, "{name}");
    }
}

#[tokio::test]
async fn scan_redis_and_valkey() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let p = profile(server.port, Auth::None);
        let path = temp_config(&p);
        let (r, perms) = report(&path, &MemoryStore::default(), &args(), None)
            .await
            .unwrap();
        check_report(&r);
        assert!(r.sizes_available);
        assert!(r.complete, "a 300-key database fits in one walk");
        assert_eq!(r.match_glob, None);
        let table = render_table(&r, &perms, 100);
        println!("{table}");
        for name in ["user:{id}:profile", "session:{id}", "order:{id}"] {
            assert!(table.contains(name), "{table}");
        }
        assert!(!table.contains("sizes unavailable"));
        let json = serde_json::to_string_pretty(&r).unwrap();
        let back: ScanReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}

#[tokio::test]
async fn match_glob_limits_the_scan() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let path = temp_config(&profile(server.port, Auth::None));
        let mut a = args();
        a.match_glob = Some("session:*".to_string());
        let (r, _) = report(&path, &MemoryStore::default(), &a, None)
            .await
            .unwrap();
        assert_eq!(r.patterns.len(), 1);
        assert_eq!(r.patterns[0].pattern, "session:{id}");
        // A MATCH scan never saw the whole database, and says which glob it used.
        assert!(!r.complete);
        assert_eq!(r.match_glob.as_deref(), Some("session:*"));
    }
}

#[tokio::test]
async fn sample_limit_is_not_complete() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let path = temp_config(&profile(server.port, Auth::None));
        let mut a = args();
        a.sample = 50;
        let (r, _) = report(&path, &MemoryStore::default(), &a, None)
            .await
            .unwrap();
        assert!(!r.complete);
        assert_eq!(r.match_glob, None);
    }
}

#[tokio::test]
async fn missing_profile_is_named() {
    let server = common::redis().await;
    let path = temp_config(&profile(server.port, Auth::None));
    let mut a = args();
    a.profile = "nope".to_string();
    let e = report(&path, &MemoryStore::default(), &a, None)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("nope"), "{e}");
}

#[tokio::test]
async fn limited_acl_user_degrades() {
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
        "+select",
        "+info",
        "+dbsize",
        "+scan",
        "+type",
        "+ttl",
        "+pttl",
    ])
    .await;
    seed(&server).await;
    let p = profile(server.port, Auth::Keychain);
    let secrets = MemoryStore::default();
    secrets
        .set(
            &p.secret_ref(),
            &Credentials {
                username: Some("lim".to_string()),
                password: SecretString::from("pw".to_string()),
            },
        )
        .unwrap();
    let path = temp_config(&p);
    let (r, perms) = report(&path, &secrets, &args(), None).await.unwrap();
    assert!(!r.sizes_available);
    check_report(&r);
    assert!(r.patterns.iter().all(|p| p.est_bytes.is_none()));
    let table = render_table(&r, &perms, 100);
    println!("{table}");
    assert!(table.contains("sizes unavailable: MEMORY USAGE denied"));
    assert!(!table.contains("pw"));
}
