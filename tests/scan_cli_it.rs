//! `redis-lens scan` against real servers.
#![cfg(feature = "integration-tests")]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use redis_lens::cli::scan::{execute, render_table, report};
use redis_lens::cli::{ReportFormat, ScanArgs, exit_code};
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
        ..Default::default()
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
        ssh: None,
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

// ---------------------------------------------------------------------------
// scan --check and --report
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-scan-it-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `scan` end to end; returns what went to standard output and the result.
async fn run_scan(
    config: &std::path::Path,
    snapshots: &std::path::Path,
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

fn checks(rules: &[&str]) -> ScanArgs {
    let mut a = args();
    a.checks = rules.iter().map(|r| r.to_string()).collect();
    a
}

#[tokio::test]
async fn passing_check_exits_zero() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let config = temp_config(&profile(server.port, Auth::None));
        let snaps = temp_dir("pass");
        let a = checks(&[
            "verdict!=mixed",
            "total-bytes<100MiB",
            "pattern-bytes<10MiB",
        ]);
        let (out, result) = run_scan(&config, &snaps, &a).await;
        result.unwrap();
        assert!(out.contains("PASS verdict!=mixed"), "{out}");
        assert!(out.contains("3 passed, 0 failed, 0 skipped"), "{out}");
        // The table is still printed.
        assert!(out.contains("PATTERN"), "{out}");
    }
}

#[tokio::test]
async fn failing_check_exits_one() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let config = temp_config(&profile(server.port, Auth::None));
        let snaps = temp_dir("fail");
        let a = checks(&["no-ttl-pct<20", "total-bytes<100MiB"]);
        let (out, result) = run_scan(&config, &snaps, &a).await;
        let err = result.unwrap_err();
        assert_eq!(exit_code(&err), 1, "{err:?}");
        assert!(
            out.contains("FAIL no-ttl-pct<20  ")
                && out.contains("user:{id}:profile 100% (100 sampled)"),
            "{out}"
        );
        assert!(out.contains("order:{id}"), "{out}");
        assert!(!out.contains("session:{id} 0%"), "session has TTLs: {out}");
        assert!(out.contains("PASS total-bytes<100MiB"), "{out}");
        assert!(out.contains("1 passed, 1 failed, 0 skipped"), "{out}");
    }
}

#[tokio::test]
async fn broken_scan_and_bad_rules_exit_two() {
    let server = common::redis().await;
    seed(&server).await;
    let config = temp_config(&profile(server.port, Auth::None));
    let snaps = temp_dir("broken");

    let mut a = checks(&["no-ttl-pct<20"]);
    a.profile = "nope".to_string();
    let (_, result) = run_scan(&config, &snaps, &a).await;
    let err = result.unwrap_err();
    assert_eq!(exit_code(&err), 2, "{err:?}");
    assert!(format!("{err:#}").contains("nope"));

    // A rule that doesn't parse is a usage error, found before the server is touched.
    let (out, result) = run_scan(&config, &snaps, &checks(&["no-ttl-pct<<"])).await;
    let err = result.unwrap_err();
    assert_eq!(exit_code(&err), 2, "{err:?}");
    assert!(err.to_string().contains("no-ttl-pct<<"), "{err}");
    assert!(out.is_empty());

    // A server that isn't there.
    let dead = temp_config(&profile(1, Auth::None));
    let (_, result) = run_scan(&dead, &snaps, &checks(&["no-ttl-pct<20"])).await;
    assert_eq!(exit_code(&result.unwrap_err()), 2);
}

#[tokio::test]
async fn rules_file_and_strict_without_sizes() {
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
    let config = temp_config(&p);
    let snaps = temp_dir("strict");
    let rules = temp_dir("rules").join("rules.toml");
    std::fs::write(
        &rules,
        "[check]\nrules = [\"pattern-bytes<1MiB\", \"no-ttl-pct<=100\"]\nmin_keys = 10\n",
    )
    .unwrap();

    let run = |strict: bool| {
        let (config, snaps, secrets, rules) = (&config, &snaps, &secrets, &rules);
        async move {
            let mut a = args();
            a.rules = Some(rules.clone());
            a.strict = strict;
            let mut out: Vec<u8> = Vec::new();
            let result = execute(config, secrets, &a, || Ok(snaps.clone()), &mut out).await;
            (String::from_utf8(out).unwrap(), result)
        }
    };
    let (out, result) = run(false).await;
    result.unwrap();
    assert!(
        out.contains("SKIP pattern-bytes<1MiB  (no MEMORY USAGE)"),
        "{out}"
    );
    assert!(out.contains("PASS no-ttl-pct<=100"), "{out}");
    let (out, result) = run(true).await;
    assert_eq!(exit_code(&result.unwrap_err()), 1);
    assert!(
        out.contains("FAIL pattern-bytes<1MiB  (not judged: no MEMORY USAGE"),
        "{out}"
    );
}

#[tokio::test]
async fn growth_rule_against_a_saved_baseline() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let config = temp_config(&profile(server.port, Auth::None));
        let snaps = temp_dir("growth");

        // No snapshot yet: the growth rule is skipped, and fails under --strict.
        let (out, result) = run_scan(&config, &snaps, &checks(&["growth<25%"])).await;
        result.unwrap();
        assert!(
            out.contains("SKIP growth<25%  (no baseline snapshot)"),
            "{out}"
        );
        let mut strict = checks(&["growth<25%"]);
        strict.strict = true;
        let (_, result) = run_scan(&config, &snaps, &strict).await;
        assert_eq!(exit_code(&result.unwrap_err()), 1);

        // Save a baseline, then triple the user profiles.
        let mut save = args();
        save.save = true;
        let (_, result) = run_scan(&config, &snaps, &save).await;
        result.unwrap();
        let mut admin = server.admin().await;
        for i in 0..200 {
            let _: () = redis::cmd("SET")
                .arg(format!("user:{}:profile", 2000 + i))
                .arg("x".repeat(50))
                .query_async(&mut admin)
                .await
                .unwrap();
        }
        // Snapshots have one-second resolution and the baseline must be strictly older.
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;

        let (out, result) = run_scan(&config, &snaps, &checks(&["growth<25%"])).await;
        let err = result.unwrap_err();
        assert_eq!(exit_code(&err), 1, "{out}");
        assert!(
            out.contains("FAIL growth<25%  user:{id}:profile +"),
            "{out}"
        );
        assert!(!out.contains("session:{id} +"), "{out}");
        let (out, result) = run_scan(&config, &snaps, &checks(&["growth<500%"])).await;
        result.unwrap();
        assert!(out.contains("PASS growth<500%"), "{out}");

        // Reports pick up the same baseline.
        let mut rep = args();
        rep.report = Some(ReportFormat::Md);
        let (out, result) = run_scan(&config, &snaps, &rep).await;
        result.unwrap();
        assert!(out.contains("## Changes since"), "{out}");
    }
}

#[tokio::test]
async fn report_files_are_written() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let config = temp_config(&profile(server.port, Auth::None));
        let snaps = temp_dir("report");
        let dir = temp_dir("reportout");
        for (format, name, needle) in [
            (ReportFormat::Md, "report.md", "## Top patterns"),
            (ReportFormat::Html, "report.html", "<!DOCTYPE html>"),
            (ReportFormat::Csv, "report.csv", "pattern,type,sampled_keys"),
        ] {
            let path = dir.join(name);
            let mut a = args();
            a.report = Some(format);
            a.output = Some(path.clone());
            let (out, result) = run_scan(&config, &snaps, &a).await;
            result.unwrap();
            assert!(
                out.is_empty(),
                "the report goes to the file, not stdout: {out}"
            );
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains(needle), "{text}");
            for name in ["user:{id}:profile", "session:{id}", "order:{id}"] {
                assert!(text.contains(name), "{text}");
            }
            // Example key names stay out unless asked for.
            assert!(!text.contains("user:10"), "{text}");
            assert!(!text.contains("127.0.0.1"), "{text}");
        }

        let path = dir.join("with-examples.md");
        let mut a = args();
        a.report = Some(ReportFormat::Md);
        a.output = Some(path.clone());
        a.examples = true;
        run_scan(&config, &snaps, &a).await.1.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("## Example keys") && text.contains("user:10"),
            "{text}"
        );

        // Without -o the report goes to standard output; --check lines go to stderr then.
        let mut a = checks(&["no-ttl-pct<20"]);
        a.report = Some(ReportFormat::Csv);
        let (out, result) = run_scan(&config, &snaps, &a).await;
        assert_eq!(exit_code(&result.unwrap_err()), 1);
        assert!(out.starts_with("pattern,type"), "{out}");
        assert!(!out.contains("FAIL"), "{out}");
    }
}
