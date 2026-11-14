//! `scan`, `--check`, reports, snapshots, `diff` and `compare` against real clusters
//! (Redis and Valkey): three primaries in one container, announcing 127.0.0.1.
#![cfg(feature = "integration-tests")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::cluster::{TestCluster, cluster};
use redis_lens::cli::compare::{compare_profiles, render_compare};
use redis_lens::cli::diff::{diff_profile_with_baseline, render_diff_noted};
use redis_lens::cli::scan::execute;
use redis_lens::cli::{CompareArgs, DiffArgs, ReportFormat, ScanArgs, exit_code};
use redis_lens::config::Config;
use redis_lens::diff::{NODES_DIFFER_NOTE, Presence, node_set_note};
use redis_lens::model::{Auth, Endpoint, Env, NodeStatus, Profile, ScanReport};
use redis_lens::secrets::MemoryStore;
use redis_lens::snapshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

const USERS: u64 = 600;
const SESSIONS: u64 = 300;
const CARTS: u64 = 150;

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-cluster-cli-{tag}-{}-{n}",
        std::process::id()
    ));
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
        sentinel: None,
    }
}

fn config(profiles: Vec<Profile>) -> PathBuf {
    let path = temp_dir("config").join("config.toml");
    Config {
        profiles,
        ..Default::default()
    }
    .save(&path)
    .unwrap();
    path
}

fn keys(users: u64, sessions: u64, carts: u64) -> Vec<(String, String)> {
    let mut out = Vec::new();
    out.extend((0..users).map(|i| (format!("user:{i}"), "x".repeat(20))));
    out.extend((0..sessions).map(|i| (format!("session:{i}:data"), "y".repeat(50))));
    out.extend((0..carts).map(|i| (format!("cart:{i}"), "z".repeat(5))));
    out
}

fn args() -> ScanArgs {
    ScanArgs {
        profile: "it".to_string(),
        sample: 100_000,
        match_glob: None,
        json: false,
        max_rate: Some(0),
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

fn checks(rules: &[&str]) -> ScanArgs {
    let mut a = args();
    a.checks = rules.iter().map(|r| r.to_string()).collect();
    a
}

/// Run `scan` end to end; returns what went to standard output and the result.
async fn run_scan(
    config: &Path,
    snapshots: &Path,
    args: &ScanArgs,
) -> (String, anyhow::Result<()>) {
    let mut out: Vec<u8> = Vec::new();
    let snapshots = snapshots.to_path_buf();
    let result = execute(
        config,
        &MemoryStore::default(),
        args,
        || Ok(snapshots.clone()),
        &mut out,
    )
    .await;
    (String::from_utf8(out).unwrap(), result)
}

fn est_keys(r: &ScanReport, pattern: &str) -> u64 {
    r.patterns
        .iter()
        .find(|p| p.pattern == pattern)
        .unwrap_or_else(|| panic!("missing {pattern}: {:?}", r.patterns))
        .est_keys
}

async fn seeded(valkey: bool) -> (TestCluster, PathBuf) {
    let c = cluster(valkey, false).await;
    c.seed(&keys(USERS, SESSIONS, CARTS)).await;
    let path = config(vec![profile("it", Env::Local, c.seed_port())]);
    (c, path)
}

/// `scan` on a cluster: the Target line, a Nodes table with every shard, the patterns of
/// all shards merged, JSON and snapshot with the nodes, and the Nodes section of the
/// Markdown and HTML reports.
#[tokio::test]
async fn scan_prints_target_nodes_and_merged_patterns() {
    for valkey in [false, true] {
        let (c, config) = seeded(valkey).await;
        let snaps = temp_dir("scan-snaps");

        let (out, result) = run_scan(&config, &snaps, &args()).await;
        result.unwrap();
        println!("{out}");
        assert!(out.contains("Target:  cluster · 3 shards"), "{out}");
        assert!(out.contains("SLOTS") && out.contains("ENDPOINT"), "{out}");
        for port in &c.ports {
            assert!(out.contains(&format!("127.0.0.1:{port}")), "{port}\n{out}");
        }
        assert_eq!(out.matches("scanned").count(), 3, "{out}");
        for name in ["user:{id}", "session:{id}:data", "cart:{id}"] {
            assert!(out.contains(name), "{name}\n{out}");
        }

        // JSON carries the nodes, and the merged figures add up over the shards.
        let mut j = args();
        j.json = true;
        let (out, result) = run_scan(&config, &snaps, &j).await;
        result.unwrap();
        let r: ScanReport = serde_json::from_str(&out).unwrap();
        assert_eq!(r.sampled_keys, USERS + SESSIONS + CARTS);
        assert_eq!(est_keys(&r, "user:{id}"), USERS);
        assert_eq!(est_keys(&r, "session:{id}:data"), SESSIONS);
        assert_eq!(est_keys(&r, "cart:{id}"), CARTS);
        let nodes = r.nodes.as_ref().expect("a cluster scan has nodes");
        assert_eq!(nodes.len(), 3);
        assert!(nodes.iter().all(|n| n.status == NodeStatus::Scanned));
        assert_eq!(
            nodes.iter().map(|n| n.sampled_keys).sum::<u64>(),
            r.sampled_keys
        );
        let slots: u32 = nodes
            .iter()
            .flat_map(|n| &n.slots)
            .map(|(a, b)| u32::from(b - a) + 1)
            .sum();
        assert_eq!(slots, 16_384);

        // A saved snapshot keeps the nodes, and holds no connection string.
        let mut save = args();
        save.save = true;
        let (_, result) = run_scan(&config, &snaps, &save).await;
        result.unwrap();
        let saved = snapshot::list(&snaps, "it").unwrap();
        assert_eq!(saved.len(), 1);
        let loaded = snapshot::load(&saved[0].path).unwrap();
        assert_eq!(loaded.nodes.as_ref().map(Vec::len), Some(3));
        assert_eq!(est_keys(&loaded, "user:{id}"), USERS);
        let text = std::fs::read_to_string(&saved[0].path).unwrap();
        assert!(
            !text.contains("redis://") && !text.contains("password"),
            "{text}"
        );

        // Reports.
        let mut md = args();
        md.report = Some(ReportFormat::Md);
        let (out, result) = run_scan(&config, &snaps, &md).await;
        result.unwrap();
        assert!(out.contains("## Nodes"), "{out}");
        assert!(
            out.contains(&format!("127.0.0.1:{}", c.seed_port())),
            "{out}"
        );
        let mut html = args();
        html.report = Some(ReportFormat::Html);
        let (out, result) = run_scan(&config, &snaps, &html).await;
        result.unwrap();
        assert!(out.contains("<h2>Nodes</h2>"), "{out}");
    }
}

/// `--check` on a cluster judges the merged patterns, and when a node is down says which
/// one was not scanned, without changing the exit code.
#[tokio::test]
async fn check_on_a_cluster_names_the_nodes_that_were_not_scanned() {
    for valkey in [false, true] {
        let (c, config) = seeded(valkey).await;
        let snaps = temp_dir("check-snaps");

        let (out, result) = run_scan(
            &config,
            &snaps,
            &checks(&["no-ttl-pct<=100", "total-bytes<100MiB"]),
        )
        .await;
        result.unwrap();
        println!("{out}");
        assert!(out.contains("PASS no-ttl-pct<=100"), "{out}");
        assert!(out.contains("PASS total-bytes<100MiB"), "{out}");
        assert!(!out.contains("not fully scanned"), "{out}");
        // Every key is without a TTL, so this rule fails on the merged patterns.
        let (out, result) = run_scan(&config, &snaps, &checks(&["no-ttl-pct<50"])).await;
        assert_eq!(exit_code(&result.unwrap_err()), 1, "{out}");
        assert!(out.contains("FAIL no-ttl-pct<50"), "{out}");

        let victim = *c.ports.iter().find(|p| **p != c.seed_port()).unwrap();
        c.stop_node(victim).await;
        let (out, result) = run_scan(&config, &snaps, &checks(&["no-ttl-pct<=100"])).await;
        result.unwrap();
        println!("{out}");
        assert!(out.contains("PASS no-ttl-pct<=100"), "{out}");
        assert!(
            out.contains(&format!(
                "NOTE not fully scanned: 127.0.0.1:{victim} (failed)"
            )),
            "{out}"
        );
        assert!(out.contains("cluster · 3 shards · 1 unreachable"), "{out}");
        assert!(out.contains("failed"), "{out}");
        // The other shards' keys are still there to judge.
        assert!(!out.contains("(0 patterns checked"), "{out}");
    }
}

/// A saved cluster snapshot diffed against a later scan of the same cluster: the nodes
/// match, so there is no note, and the pattern diff works. Against a standalone server's
/// scan, the one-line note appears and patterns are still diffed on the merged rows.
#[tokio::test]
async fn diff_a_saved_cluster_snapshot() {
    for valkey in [false, true] {
        let (c, cluster_config) = seeded(valkey).await;
        let snaps = temp_dir("diff-snaps");
        let mut save = args();
        save.save = true;
        let (_, result) = run_scan(&cluster_config, &snaps, &save).await;
        result.unwrap();
        let baseline = snapshot::list(&snaps, "it").unwrap().remove(0);
        // Snapshots are named by the second; the diff needs a strictly later scan.
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        c.seed(&keys(0, 0, 3 * CARTS)).await;

        let diff_args = DiffArgs {
            profile: "it".to_string(),
            against: None,
            max_rate: Some(0),
            json: false,
        };
        let secrets = MemoryStore::default();
        let (d, before, after) =
            diff_profile_with_baseline(&cluster_config, &secrets, &diff_args, &snaps, None)
                .await
                .unwrap();
        assert_eq!(d.before_taken_at_unix, baseline.taken_at_unix);
        assert_eq!(before.nodes.as_ref().map(Vec::len), Some(3));
        assert_eq!(after.nodes.as_ref().map(Vec::len), Some(3));
        let cart = d.pattern("cart:{id}").unwrap();
        assert_eq!(cart.keys_before, Some(CARTS));
        assert_eq!(cart.keys_after, Some(CARTS * 3));
        assert_eq!(node_set_note(&before, &after), None);
        let text = render_diff_noted(&d, node_set_note(&before, &after), 100);
        println!("{text}");
        assert!(!text.contains("different nodes"), "{text}");

        // The same snapshot against a standalone server.
        let server = if valkey {
            common::valkey().await
        } else {
            common::redis().await
        };
        let mut admin = server.admin().await;
        for i in 0..100 {
            let _: () = redis::cmd("SET")
                .arg(format!("user:{i}"))
                .arg("x")
                .query_async(&mut admin)
                .await
                .unwrap();
        }
        let standalone_config = config(vec![profile("it", Env::Local, server.port)]);
        let mut against = diff_args;
        against.against = Some(baseline.path.clone());
        let (d, before, after) = diff_profile_with_baseline(
            &standalone_config,
            &secrets,
            &against,
            &temp_dir("diff-snaps-2"),
            None,
        )
        .await
        .unwrap();
        assert!(before.nodes.is_some() && after.nodes.is_none());
        let note = node_set_note(&before, &after);
        assert_eq!(note, Some(NODES_DIFFER_NOTE));
        let text = render_diff_noted(&d, note, 100);
        println!("{text}");
        assert!(text.contains("different nodes"), "{text}");
        let note_at = text.find("Note:").unwrap();
        assert!(note_at < text.find("PATTERN").unwrap(), "{text}");
        let user = d.pattern("user:{id}").unwrap();
        assert_eq!(user.keys_before, Some(USERS));
        assert_eq!(user.keys_after, Some(100));
        assert_eq!(d.pattern("cart:{id}").unwrap().keys_after, None);
    }
}

/// `compare` of a standalone server with a cluster, and of a cluster with itself: the
/// merged patterns are compared and each side's header says it is a cluster.
#[tokio::test]
async fn compare_cluster_with_standalone_and_with_cluster() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        c.seed(&keys(USERS, SESSIONS, CARTS)).await;
        let server = if valkey {
            common::valkey().await
        } else {
            common::redis().await
        };
        let mut admin = server.admin().await;
        for i in 0..100 {
            let _: () = redis::cmd("SET")
                .arg(format!("user:{i}"))
                .arg("x")
                .query_async(&mut admin)
                .await
                .unwrap();
        }
        let path = config(vec![
            profile("staging", Env::Stage, server.port),
            profile("prod", Env::Local, c.seed_port()),
            profile("prod2", Env::Local, c.seed_port()),
        ]);
        let compare_args = |left: &str, right: &str| CompareArgs {
            left: left.to_string(),
            right: right.to_string(),
            left_file: None,
            right_file: None,
            sample: 100_000,
            max_rate: Some(0),
            json: false,
        };
        let secrets = MemoryStore::default();

        let (cmp, left, right) =
            compare_profiles(&path, &secrets, &compare_args("staging", "prod"), None)
                .await
                .unwrap();
        assert!(left.nodes.is_none());
        assert_eq!(right.nodes.as_ref().map(Vec::len), Some(3));
        assert_eq!(cmp.left.shards, None);
        assert_eq!(cmp.right.shards, Some(3));
        let user = cmp.pattern("user:{id}").unwrap();
        assert_eq!(user.presence, Presence::Both);
        assert_eq!(user.left.as_ref().unwrap().est_keys, 100);
        assert_eq!(user.right.as_ref().unwrap().est_keys, USERS);
        assert_eq!(
            cmp.pattern("cart:{id}").unwrap().presence,
            Presence::RightOnly
        );
        let text = render_compare(&cmp, "staging", "prod", 140);
        println!("{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("L  staging: sampled"), "{text}");
        assert!(
            lines[1].starts_with("R  prod (cluster · 3 shards): sampled"),
            "{text}"
        );

        let (cmp, _, _) = compare_profiles(&path, &secrets, &compare_args("prod", "prod2"), None)
            .await
            .unwrap();
        assert_eq!((cmp.left.shards, cmp.right.shards), (Some(3), Some(3)));
        assert!(cmp.patterns.iter().all(|p| p.presence == Presence::Both));
        let text = render_compare(&cmp, "prod", "prod2", 140);
        assert!(text.contains("prod (cluster · 3 shards)"), "{text}");
        assert!(text.contains("prod2 (cluster · 3 shards)"), "{text}");
    }
}
