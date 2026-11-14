//! Snapshots, `diff` and hot keys against real servers.
#![cfg(feature = "integration-tests")]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use redis_lens::cli::diff::{diff_profile, render_diff};
use redis_lens::cli::scan::{render_table, report, save_if_requested};
use redis_lens::cli::{DiffArgs, ScanArgs};
use redis_lens::config::Config;
use redis_lens::diff::Cause;
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::secrets::MemoryStore;
use redis_lens::snapshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Seeded into values; must never show up in a snapshot file.
const MARKER: &str = "VALUE-MARKER-123";

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("redis-lens-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn temp_config(port: u16) -> PathBuf {
    let profile = Profile {
        name: "it".to_string(),
        env: Env::Local,
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
    };
    let path = temp_dir("diff-it-config").join("config.toml");
    Config {
        profiles: vec![profile],
        ..Default::default()
    }
    .save(&path)
    .unwrap();
    path
}

fn scan_args(save: bool) -> ScanArgs {
    ScanArgs {
        profile: "it".to_string(),
        sample: 10_000,
        match_glob: None,
        json: false,
        max_rate: Some(0),
        save,
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

fn diff_args() -> DiffArgs {
    DiffArgs {
        profile: "it".to_string(),
        against: None,
        max_rate: Some(0),
        json: false,
    }
}

async fn set(conn: &mut redis::aio::MultiplexedConnection, key: String, value: String) {
    let _: () = redis::cmd("SET")
        .arg(key)
        .arg(value)
        .query_async(conn)
        .await
        .unwrap();
}

async fn hset(conn: &mut redis::aio::MultiplexedConnection, key: String, value: String) {
    let _: () = redis::cmd("HSET")
        .arg(key)
        .arg("f")
        .arg(value)
        .query_async(conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn diff_finds_causes_on_redis_and_valkey() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        for i in 0..100 {
            set(
                &mut admin,
                format!("user:{}:profile", 1000 + i),
                format!("{MARKER}{}", "x".repeat(2000)),
            )
            .await;
            hset(
                &mut admin,
                format!("order:{}", 9000 + i),
                format!("{MARKER}{}", "v".repeat(100)),
            )
            .await;
            set(&mut admin, format!("flag:{}", 100 + i), "1".to_string()).await;
        }
        let config = temp_config(server.port);
        let dir = temp_dir("diff-it-snaps");
        let secrets = MemoryStore::default();

        // No snapshot yet: a clear error, and nothing saved.
        let err = diff_profile(&config, &secrets, &diff_args(), &dir, None)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "no snapshot for it; run `redis-lens scan it --save` first"
        );
        assert!(snapshot::list(&dir, "it").unwrap().is_empty());

        let args = scan_args(true);
        let (first, _) = report(&config, &secrets, &args, None).await.unwrap();
        let saved = save_if_requested(&dir, &args, &first).unwrap().unwrap();
        assert!(saved.starts_with(&dir));
        let text = std::fs::read_to_string(&saved).unwrap();
        assert!(!text.contains("password"));
        assert!(
            !text.contains(MARKER),
            "a value leaked into {}",
            saved.display()
        );
        // The data was really there to leak.
        let v: String = redis::cmd("GET")
            .arg("user:1000:profile")
            .query_async(&mut admin)
            .await
            .unwrap();
        assert!(v.starts_with(MARKER));

        // Snapshots are named by the second, and the baseline must be strictly older.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        for i in 100..300 {
            set(
                &mut admin,
                format!("user:{}:profile", 1000 + i),
                "x".repeat(2000),
            )
            .await;
        }
        for i in 0..100 {
            hset(&mut admin, format!("order:{}", 9000 + i), "v".repeat(4000)).await;
        }
        for i in 0..50 {
            set(&mut admin, format!("audit:{}", 1 + i), "a".repeat(2000)).await;
        }

        let (d, second) = diff_profile(&config, &secrets, &diff_args(), &dir, None)
            .await
            .unwrap();
        let text = render_diff(&d, 60);
        println!("{text}");
        assert_eq!(
            d.pattern("user:{id}:profile").unwrap().cause,
            Cause::MoreKeys
        );
        assert_eq!(d.pattern("order:{id}").unwrap().cause, Cause::BiggerValues);
        assert_eq!(d.pattern("audit:{id}").unwrap().cause, Cause::New);
        assert_eq!(d.pattern("flag:{id}").unwrap().cause, Cause::Unchanged);
        assert_eq!(d.before_taken_at_unix, first.taken_at_unix);
        assert_eq!(d.after_taken_at_unix, second.taken_at_unix);
        for l in text.lines() {
            assert!(l.chars().count() <= 60, "{l:?}");
        }
        // The diff saved the new scan, so it is now the newest snapshot.
        let saved_all = snapshot::list(&dir, "it").unwrap();
        assert_eq!(saved_all.len(), 2);
        for info in &saved_all {
            let text = std::fs::read_to_string(&info.path).unwrap();
            assert!(!text.contains(MARKER), "{}", info.path.display());
        }

        // --against compares with a chosen file.
        let mut against = diff_args();
        against.against = Some(saved);
        let (d2, _) = diff_profile(&config, &secrets, &against, &dir, None)
            .await
            .unwrap();
        assert_eq!(d2.before_taken_at_unix, first.taken_at_unix);
        assert_eq!(d2.pattern("audit:{id}").unwrap().cause, Cause::New);
    }
}

/// `scan --save --keep 2` three times: the oldest snapshot goes, the two newest stay,
/// and another profile's snapshot in the same directory is untouched.
#[tokio::test]
async fn save_with_keep_prunes_old_snapshots() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        for i in 0..20 {
            set(&mut admin, format!("keep:{i}"), "v".to_string()).await;
        }
        let config = temp_config(server.port);
        let dir = temp_dir("diff-it-keep");
        let secrets = MemoryStore::default();
        let mut other = report(&config, &secrets, &scan_args(false), None)
            .await
            .unwrap()
            .0;
        other.profile = "someone-else".to_string();
        other.taken_at_unix -= 86_400;
        snapshot::save(&dir, &other).unwrap();

        let mut args = scan_args(true);
        args.keep = Some(2);
        let mut saved = Vec::new();
        for n in 0..3 {
            if n > 0 {
                // Snapshots are named by the second.
                tokio::time::sleep(Duration::from_millis(1100)).await;
            }
            let (r, _) = report(&config, &secrets, &args, None).await.unwrap();
            saved.push(save_if_requested(&dir, &args, &r).unwrap().unwrap());
        }
        let left: Vec<PathBuf> = snapshot::list(&dir, "it")
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert_eq!(left, [saved[2].clone(), saved[1].clone()]);
        assert!(!saved[0].exists(), "{} was kept", saved[0].display());
        assert_eq!(snapshot::list(&dir, "someone-else").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn hot_keys_with_lfu_redis() {
    hot_keys_with_lfu(common::redis_with(&["--maxmemory-policy", "allkeys-lfu"]).await).await;
}

#[tokio::test]
async fn hot_keys_with_lfu_valkey() {
    hot_keys_with_lfu(common::valkey_with(&["--maxmemory-policy", "allkeys-lfu"]).await).await;
}

async fn hot_keys_with_lfu(server: common::TestServer) {
    let mut admin = server.admin().await;
    for i in 0..50 {
        set(&mut admin, format!("item:{i}"), "x".to_string()).await;
    }
    for _ in 0..300 {
        for i in 0..3 {
            let _: Option<String> = redis::cmd("GET")
                .arg(format!("item:{i}"))
                .query_async(&mut admin)
                .await
                .unwrap();
        }
    }
    let config = temp_config(server.port);
    let (r, perms) = report(&config, &MemoryStore::default(), &scan_args(false), None)
        .await
        .unwrap();
    assert!(!r.hot_keys.is_empty());
    assert!(r.hot_keys.len() <= 50);
    assert!(r.hot_keys.iter().all(|k| k.lfu_freq.is_some_and(|f| f > 0)));
    for w in r.hot_keys.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        assert!(
            a.lfu_freq > b.lfu_freq || (a.lfu_freq == b.lfu_freq && a.key < b.key),
            "{a:?} before {b:?}"
        );
    }
    assert!(["item:0", "item:1", "item:2"].contains(&r.hot_keys[0].key.as_str()));
    let table = render_table(&r, &perms, 100);
    println!("{table}");
    assert!(table.contains("Hot:     item:"), "{table}");
}

#[tokio::test]
async fn match_snapshot_is_not_a_baseline() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        for i in 0..20 {
            set(&mut admin, format!("user:{i}"), "u".to_string()).await;
            set(&mut admin, format!("order:{i}"), "o".to_string()).await;
        }
        let config = temp_config(server.port);
        let dir = temp_dir("diff-it-match");
        let secrets = MemoryStore::default();
        let mut args = scan_args(true);
        args.match_glob = Some("user:*".to_string());
        let (partial, _) = report(&config, &secrets, &args, None).await.unwrap();
        assert!(!partial.complete);
        save_if_requested(&dir, &args, &partial).unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;

        // A full diff must not compare against the user:* snapshot, which would call
        // every order key new.
        let err = diff_profile(&config, &secrets, &diff_args(), &dir, None)
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("no snapshot for it"), "{err}");

        // Asked for explicitly, it can be compared, but nothing is called new.
        let mut against = diff_args();
        against.against = Some(snapshot::list(&dir, "it").unwrap()[0].path.clone());
        let (d, _) = diff_profile(&config, &secrets, &against, &dir, None)
            .await
            .unwrap();
        assert_eq!(d.pattern("order:{id}").unwrap().cause, Cause::Unknown);
        assert!(d.patterns.iter().all(|p| p.cause != Cause::New));
    }
}

/// Commands the server has processed so far (`INFO stats`).
async fn commands_processed(admin: &mut redis::aio::MultiplexedConnection) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("stats")
        .query_async(admin)
        .await
        .unwrap();
    info.lines()
        .find_map(|l| l.strip_prefix("total_commands_processed:"))
        .and_then(|v| v.trim().parse().ok())
        .expect("total_commands_processed in INFO stats")
}

/// `--max-rate N` reaches the scans of both `scan` and `diff`, through the same entry
/// points the CLI uses: the profile is `local` (no limit by default), so only the flag
/// can slow them down to at least commands / N seconds. Rule 2.
#[tokio::test]
async fn max_rate_paces_scan_and_diff() {
    const RATE: u32 = 200;
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        for i in 0..60 {
            set(&mut admin, format!("rate:{i}"), "v".to_string()).await;
        }
        let config = temp_config(server.port);
        let dir = temp_dir("diff-it-rate");
        let secrets = MemoryStore::default();
        // A baseline for `diff`, scanned without a limit.
        let base = scan_args(true);
        let (first, _) = report(&config, &secrets, &base, None).await.unwrap();
        save_if_requested(&dir, &base, &first).unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;

        let mut paced = scan_args(false);
        paced.max_rate = Some(RATE);
        let mut paced_diff = diff_args();
        paced_diff.max_rate = Some(RATE);
        for which in ["scan", "diff"] {
            let before = commands_processed(&mut admin).await;
            let started = std::time::Instant::now();
            if which == "scan" {
                report(&config, &secrets, &paced, None).await.unwrap();
            } else {
                diff_profile(&config, &secrets, &paced_diff, &dir, None)
                    .await
                    .unwrap();
            }
            let took = started.elapsed();
            // Less the INFO call that reads the count after.
            let sent = commands_processed(&mut admin).await - before - 1;
            assert!(sent >= 120, "{which}: only {sent} commands");
            // The limiter paces what it sends after the permission probes and INFO;
            // leave 20 commands of slack for those.
            let min = Duration::from_secs_f64((sent - 20) as f64 / f64::from(RATE));
            assert!(
                took >= min,
                "{which}: {sent} commands in {took:?}, budget {RATE}/s needs {min:?}"
            );
        }
    }
}
