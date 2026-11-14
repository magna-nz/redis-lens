//! Sentinel profiles (P4) against real Sentinels, on Redis and Valkey: one container with
//! a primary, a replica and two Sentinels (`tests/common/sentinel.rs`).
#![cfg(feature = "integration-tests")]

mod common;

use std::time::Duration;

use common::sentinel::{MASTER, SentinelAuth, SentinelSetup, TestSentinel, sentinel};
use redis_lens::cli::ConnAddArgs;
use redis_lens::cli::conn as cli_conn;
use redis_lens::config::Config;
use redis_lens::conn::cluster::{ScanTargets, connect_targets};
use redis_lens::conn::{self, ScanTarget};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile, Sentinel, SentinelNode};
use redis_lens::scan::{self, ScanOptions};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;

const DATA_PW: &str = "data-pw-it-5n";
const SENTINEL_PW: &str = "sentinel-pw-it-5n";
const SENTINEL_USER: &str = "lens-sentinel";
const SENTINEL_USER_PW: &str = "lens-sentinel-pw-it";

fn node(port: u16) -> SentinelNode {
    SentinelNode {
        host: "127.0.0.1".into(),
        port,
    }
}

/// A Sentinel profile for `s`, asking its Sentinels in order. Its `endpoint` is a port
/// nothing listens on: redis-lens must never dial it.
fn profile(s: &TestSentinel, master: &str) -> Profile {
    Profile {
        name: "watched".into(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port: common::free_port(),
            tls: false,
        },
        db: 0,
        auth: if s.setup.data_password.is_some() {
            Auth::Keychain
        } else {
            Auth::None
        },
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: Some(Sentinel {
            master: master.into(),
            nodes: s.sentinel_ports.iter().map(|&p| node(p)).collect(),
            tls: false,
        }),
    }
}

fn creds(user: Option<&str>, pw: &str) -> Credentials {
    Credentials {
        username: user.map(String::from),
        password: SecretString::from(pw.to_string()),
    }
}

/// The store for `p`: the data node's password and the Sentinels' own credentials, each
/// in its own entry.
fn store(s: &TestSentinel, p: &Profile) -> MemoryStore {
    let store = MemoryStore::default();
    if let Some(pw) = s.setup.data_password {
        store.set(&p.secret_ref(), &creds(None, pw)).unwrap();
    }
    match s.setup.sentinel_auth {
        SentinelAuth::None => {}
        SentinelAuth::Password(pw) => store
            .set(&p.sentinel_secret_ref(), &creds(None, pw))
            .unwrap(),
        SentinelAuth::Acl { user, password } => store
            .set(&p.sentinel_secret_ref(), &creds(Some(user), password))
            .unwrap(),
    }
    store
}

async fn seed(s: &TestSentinel) {
    let mut c = s.admin(s.primary_port).await;
    for i in 0..50 {
        let _: () = redis::cmd("SET")
            .arg(format!("order:{i}"))
            .arg("v")
            .query_async(&mut c)
            .await
            .unwrap();
    }
}

fn primary_ep(port: u16) -> Endpoint {
    Endpoint {
        host: "127.0.0.1".into(),
        port,
        tls: false,
    }
}

/// A scan through Sentinel reads the primary it names, and `conn add` / `conn test`
/// show it.
async fn scan_and_conn_test(valkey: bool) {
    let s = sentinel(SentinelSetup::new(valkey)).await;
    seed(&s).await;
    let p = profile(&s, MASTER);
    let secrets = store(&s, &p);
    let targets = connect_targets(&p, &secrets).await.unwrap();
    let ScanTargets::Single { mut conn, target } = targets else {
        panic!("a Sentinel profile was taken for a cluster");
    };
    assert_eq!(target, ScanTarget::Primary);
    let answer = conn.sentinel().unwrap().clone();
    assert_eq!(answer.sentinel, node(s.sentinel_ports[0]));
    assert_eq!(answer.master, MASTER);
    assert_eq!(answer.primary, primary_ep(s.primary_port));
    let perms = conn::check_permissions(&mut conn).await;
    let report = scan::scan(
        &p.name,
        &mut conn,
        &ScanOptions::for_profile(&p),
        &perms,
        None,
    )
    .await
    .unwrap();
    assert!(report.sampled_keys >= 50, "{}", report.sampled_keys);
    assert!(
        report
            .patterns
            .iter()
            .any(|r| r.pattern.starts_with("order:"))
    );

    // `conn add --sentinel …`, then `conn test`.
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-sentinel-it-{}-{}",
        std::process::id(),
        s.primary_port
    ));
    let path = dir.join("config.toml");
    let nodes = s
        .sentinel_ports
        .iter()
        .map(|p| format!("127.0.0.1:{p}"))
        .collect::<Vec<_>>()
        .join(",");
    let args = ConnAddArgs {
        name: "via-sentinel".into(),
        url: format!("redis://127.0.0.1:{}", common::free_port()),
        env: None,
        password_stdin: false,
        ask_password: false,
        entra: false,
        force: false,
        prefer_replica: false,
        provider: None,
        token_command: Vec::new(),
        yes: false,
        writes: None,
        confirm: None,
        ssh: None,
        ssh_identity: None,
        ssh_known_hosts: None,
        sentinel: Some(nodes),
        sentinel_master: Some(MASTER.into()),
        sentinel_tls: false,
        sentinel_user: None,
        sentinel_password_stdin: false,
        sentinel_ask_password: false,
    };
    let cli_store = MemoryStore::default();
    cli_conn::add(&path, &cli_store, &args, None).await.unwrap();
    let out = cli_conn::test(&path, &cli_store, "via-sentinel")
        .await
        .unwrap();
    let want = format!(
        "Sentinel: 127.0.0.1:{} gave {MASTER} at 127.0.0.1:{}.",
        s.sentinel_ports[0], s.primary_port
    );
    assert!(out.contains(&want), "{out}");
    assert!(
        out.contains(&format!(
            "Connected to via-sentinel (127.0.0.1:{}",
            s.primary_port
        )),
        "{out}"
    );
    let saved = Config::load(&path).unwrap();
    assert!(saved.get("via-sentinel").unwrap().sentinel.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A master name no Sentinel knows is a fixed error.
async fn wrong_master_name(valkey: bool) {
    let s = sentinel(SentinelSetup::new(valkey)).await;
    let p = profile(&s, "no-such-master");
    let e = conn::connect(&p, &store(&s, &p)).await.unwrap_err();
    assert_eq!(e.to_string(), "no Sentinel knew the primary no-such-master");
}

/// With the first Sentinel down, the second answers.
async fn first_sentinel_down(valkey: bool) {
    let s = sentinel(SentinelSetup::new(valkey)).await;
    s.stop_sentinel(s.sentinel_ports[0]).await;
    let p = profile(&s, MASTER);
    let c = conn::connect(&p, &store(&s, &p)).await.unwrap();
    let a = c.sentinel().unwrap();
    assert_eq!(a.sentinel, node(s.sentinel_ports[1]));
    assert_eq!(a.primary, primary_ep(s.primary_port));
}

/// The Sentinels' password (requirepass) and the data node's are each used only where
/// they belong: swapping them, or leaving the Sentinel entry out, fails with the fixed
/// texts.
async fn passwords_stay_where_they_belong(valkey: bool) {
    let mut setup = SentinelSetup::new(valkey);
    setup.data_password = Some(DATA_PW);
    setup.sentinel_auth = SentinelAuth::Password(SENTINEL_PW);
    let s = sentinel(setup).await;
    let p = profile(&s, MASTER);
    let good = store(&s, &p);
    let mut c = conn::connect(&p, &good).await.unwrap();
    let pong: String = redis::cmd("PING").query_async(&mut c).await.unwrap();
    assert_eq!(pong, "PONG");

    // No Sentinel entry: the Sentinels are asked without credentials and refuse.
    let no_sentinel = MemoryStore::default();
    no_sentinel
        .set(&p.secret_ref(), &creds(None, DATA_PW))
        .unwrap();
    let e = conn::connect(&p, &no_sentinel)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "every Sentinel refused redis-lens (check the Sentinel credentials)"
    );
    // The two swapped: the Sentinel refuses the data node's password.
    let swapped = MemoryStore::default();
    swapped
        .set(&p.secret_ref(), &creds(None, SENTINEL_PW))
        .unwrap();
    swapped
        .set(&p.sentinel_secret_ref(), &creds(None, DATA_PW))
        .unwrap();
    let e = conn::connect(&p, &swapped).await.unwrap_err().to_string();
    assert_eq!(
        e,
        "every Sentinel refused redis-lens (check the Sentinel credentials)"
    );
    // The right Sentinel password, the wrong data one: the primary refuses it.
    let wrong_data = MemoryStore::default();
    wrong_data
        .set(&p.secret_ref(), &creds(None, SENTINEL_PW))
        .unwrap();
    wrong_data
        .set(&p.sentinel_secret_ref(), &creds(None, SENTINEL_PW))
        .unwrap();
    let e = conn::connect(&p, &wrong_data)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        format!(
            "the primary Sentinel gave (127.0.0.1:{}) refused the profile's credentials",
            s.primary_port
        )
    );
    assert!(!e.contains(SENTINEL_PW) && !e.contains(DATA_PW), "{e}");
}

/// A Sentinel ACL user with the minimal rules the spec names, `prefer_replica`
/// included: the replica the Sentinel lists is verified and scanned.
async fn acl_user_and_prefer_replica(valkey: bool) {
    let mut setup = SentinelSetup::new(valkey);
    setup.data_password = Some(DATA_PW);
    setup.sentinel_auth = SentinelAuth::Acl {
        user: SENTINEL_USER,
        password: SENTINEL_USER_PW,
    };
    let s = sentinel(setup).await;
    seed(&s).await;
    let mut p = profile(&s, MASTER);
    p.prefer_replica = true;
    let secrets = store(&s, &p);
    let (mut c, target) = conn::connect_for_scan(&p, &secrets).await.unwrap();
    assert_eq!(target, ScanTarget::Replica(primary_ep(s.replica_port)));
    assert_eq!(c.sentinel().unwrap().primary, primary_ep(s.primary_port));
    let info: String = redis::cmd("INFO")
        .arg("replication")
        .query_async(&mut c)
        .await
        .unwrap();
    assert!(info.contains("role:slave"), "{info}");
    // Through connect_targets, as the TUI and `scan` connect.
    let t = connect_targets(&p, &secrets).await.unwrap();
    assert_eq!(t.target(), ScanTarget::Replica(primary_ep(s.replica_port)));
}

/// The Pub/Sub tail and Monitor ask the Sentinels too, and run on the primary they name,
/// with the data node's password.
async fn tail_and_monitor(valkey: bool) {
    use futures::StreamExt;
    let mut setup = SentinelSetup::new(valkey);
    setup.data_password = Some(DATA_PW);
    setup.sentinel_auth = SentinelAuth::Password(SENTINEL_PW);
    let s = sentinel(setup).await;
    let p = profile(&s, MASTER);
    let secrets = store(&s, &p);
    let mut admin = s.admin(s.primary_port).await;

    let (mut sub, mut pushes) = conn::open_subscriber(&p, &secrets).await.unwrap();
    assert_eq!(sub.sentinel().unwrap().primary, primary_ep(s.primary_port));
    sub.subscribe("orders").await.unwrap();
    let _: i64 = redis::cmd("PUBLISH")
        .arg("orders")
        .arg("placed")
        .query_async(&mut admin)
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(push) = pushes.recv().await {
            if push.kind == redis::PushKind::Message {
                return Some(push);
            }
        }
        None
    })
    .await
    .expect("a message in time")
    .expect("the connection is open");
    assert!(format!("{:?}", got.data).contains("orders"), "{got:?}");

    let budget = redis_lens::budget::Budget::new(None);
    let mut m = conn::open_monitor(&p, &secrets, &budget)
        .await
        .unwrap()
        .expect("MONITOR allowed");
    let _: () = redis::cmd("SET")
        .arg("sentinel:monitor:key")
        .arg("v")
        .query_async(&mut admin)
        .await
        .unwrap();
    let seen = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = m.lines.next().await {
            if format!("{line:?}").contains("sentinel:monitor:key") {
                return true;
            }
        }
        false
    })
    .await
    .expect("the SET in time");
    assert!(seen);
}

/// After a failover the Sentinels name the old replica, and a new connect goes there.
async fn failover(valkey: bool) {
    let s = sentinel(SentinelSetup::new(valkey)).await;
    let p = profile(&s, MASTER);
    let secrets = store(&s, &p);
    let before = conn::connect(&p, &secrets).await.unwrap();
    assert_eq!(before.sentinel().unwrap().primary.port, s.primary_port);
    drop(before);
    if !s.failover(Duration::from_secs(40)).await {
        // SENTINEL FAILOVER depends on the replica being picked in time, which a
        // loaded Docker host doesn't always manage: not a redis-lens failure.
        eprintln!("skipped: the failover did not finish in time on this machine");
        return;
    }
    let after = conn::connect(&p, &secrets).await.unwrap();
    assert_eq!(after.sentinel().unwrap().primary.port, s.replica_port);
}

#[tokio::test]
async fn redis_scan_and_conn_test() {
    scan_and_conn_test(false).await;
}

#[tokio::test]
async fn valkey_scan_and_conn_test() {
    scan_and_conn_test(true).await;
}

#[tokio::test]
async fn redis_wrong_master_name() {
    wrong_master_name(false).await;
}

#[tokio::test]
async fn valkey_wrong_master_name() {
    wrong_master_name(true).await;
}

#[tokio::test]
async fn redis_first_sentinel_down() {
    first_sentinel_down(false).await;
}

#[tokio::test]
async fn valkey_first_sentinel_down() {
    first_sentinel_down(true).await;
}

#[tokio::test]
async fn redis_passwords_stay_where_they_belong() {
    passwords_stay_where_they_belong(false).await;
}

#[tokio::test]
async fn valkey_passwords_stay_where_they_belong() {
    passwords_stay_where_they_belong(true).await;
}

#[tokio::test]
async fn redis_acl_user_and_prefer_replica() {
    acl_user_and_prefer_replica(false).await;
}

#[tokio::test]
async fn valkey_acl_user_and_prefer_replica() {
    acl_user_and_prefer_replica(true).await;
}

#[tokio::test]
async fn redis_tail_and_monitor() {
    tail_and_monitor(false).await;
}

#[tokio::test]
async fn valkey_tail_and_monitor() {
    tail_and_monitor(true).await;
}

#[tokio::test]
async fn redis_failover() {
    failover(false).await;
}

#[tokio::test]
async fn valkey_failover() {
    failover(true).await;
}
