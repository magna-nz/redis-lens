//! Scanning a replica instead of the primary (`prefer_replica`).
#![cfg(feature = "integration-tests")]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use redis_lens::conn::{self, Replication, ScanTarget, check_permissions};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile, SecretRef};
use redis_lens::scan::{self, ScanOptions};
use redis_lens::secrets::{MemoryStore, SecretError, SecretStore};
use secrecy::SecretString;

const KEYS: u64 = 25;
const USER: &str = "lens";
const PASSWORD: &str = "lens-pw";
/// An ACL user `lens`; the default user stays on with no password.
const LENS_USER: [&str; 7] = ["--user", "lens", "on", ">lens-pw", "~*", "&*", "+@all"];
/// An ACL user `lens` and the default user off, so every command needs `AUTH`.
const LENS_ONLY: [&str; 10] = [
    "--user", "lens", "on", ">lens-pw", "~*", "&*", "+@all", "--user", "default", "off",
];
/// What a replica of a [`LENS_ONLY`] primary needs: the same users, and a login for
/// the replication link.
const LENS_ONLY_REPLICA: [&str; 14] = [
    "--user",
    "lens",
    "on",
    ">lens-pw",
    "~*",
    "&*",
    "+@all",
    "--user",
    "default",
    "off",
    "--masteruser",
    "lens",
    "--masterauth",
    "lens-pw",
];

/// A [`MemoryStore`] that counts `get` calls.
#[derive(Default)]
struct CountingStore {
    inner: MemoryStore,
    gets: AtomicUsize,
}

impl CountingStore {
    fn with_lens(profile: &Profile) -> Self {
        let s = Self::default();
        let creds = Credentials {
            username: Some(USER.to_string()),
            password: SecretString::from(PASSWORD.to_string()),
        };
        s.inner.set(&profile.secret_ref(), &creds).unwrap();
        s
    }

    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
}

impl SecretStore for CountingStore {
    fn get(&self, r: &SecretRef) -> Result<Option<Credentials>, SecretError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(r)
    }
    fn set(&self, r: &SecretRef, creds: &Credentials) -> Result<(), SecretError> {
        self.inner.set(r, creds)
    }
    fn delete(&self, r: &SecretRef) -> Result<(), SecretError> {
        self.inner.delete(r)
    }
}

fn keychain_profile(port: u16) -> Profile {
    Profile {
        auth: Auth::Keychain,
        ..profile(port, true)
    }
}

fn local(port: u16) -> Endpoint {
    Endpoint {
        host: "127.0.0.1".into(),
        port,
        tls: false,
    }
}

fn profile(port: u16, prefer_replica: bool) -> Profile {
    Profile {
        name: "p".to_string(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only: None,
        prefer_replica,
    }
}

async fn info(conn: &mut MultiplexedConnection) -> String {
    redis::cmd("INFO")
        .arg("replication")
        .query_async(conn)
        .await
        .unwrap()
}

async fn seed(server: &common::TestServer) {
    seed_conn(&mut server.admin().await).await;
}

async fn seed_conn(c: &mut MultiplexedConnection) {
    for i in 0..KEYS {
        let _: () = redis::cmd("SET")
            .arg(format!("user:{i}:profile"))
            .arg("x")
            .query_async(c)
            .await
            .unwrap();
    }
}

/// Wait until the replica's link is up and the primary lists an online replica.
async fn wait_for_replication(primary: &common::TestServer, replica: &common::TestServer) {
    wait_until_replicating(&mut primary.admin().await, &mut replica.admin().await).await;
}

/// [`wait_for_replication`] over connections the caller opened.
async fn wait_until_replicating(p: &mut MultiplexedConnection, r: &mut MultiplexedConnection) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let up = info(r).await.contains("master_link_status:up");
        let listed = matches!(
            conn::parse_replication(&info(p).await),
            Replication::Master { online } if !online.is_empty()
        );
        if up && listed {
            return;
        }
        assert!(Instant::now() < deadline, "replication did not come up");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Primary and replica for one flavour, with the keys already replicated.
async fn pair(
    valkey: bool,
    announce_port: Option<u16>,
) -> (common::TestServer, common::TestServer) {
    let primary = if valkey {
        common::valkey().await
    } else {
        common::redis().await
    };
    seed(&primary).await;
    let replica = common::replica_of(&primary, valkey, announce_port).await;
    wait_for_replication(&primary, &replica).await;
    (primary, replica)
}

#[tokio::test]
async fn scans_a_replica_when_asked() {
    for valkey in [false, true] {
        let (primary, replica) = pair(valkey, None).await;
        let secrets = MemoryStore::default();
        let (mut c, target) = conn::connect_for_scan(&profile(primary.port, true), &secrets)
            .await
            .unwrap();
        assert_eq!(
            target,
            ScanTarget::Replica(Endpoint {
                host: "127.0.0.1".into(),
                port: replica.port,
                tls: false
            }),
            "valkey={valkey}"
        );
        let replica_info = info(&mut c).await;
        assert!(replica_info.contains("role:slave"));
        // The scanned server carries the primary's replication ID.
        let primary_info = info(&mut primary.admin().await).await;
        assert!(conn::master_replid(&primary_info).is_some());
        assert_eq!(
            conn::master_replid(&replica_info),
            conn::master_replid(&primary_info)
        );

        let perms = check_permissions(&mut c).await;
        let report = scan::scan(
            "p",
            &mut c,
            &ScanOptions::for_profile(&profile(primary.port, true)),
            &perms,
            None,
        )
        .await
        .unwrap();
        assert_eq!(report.sampled_keys, KEYS, "valkey={valkey}");
        assert_eq!(report.patterns[0].pattern, "user:{id}:profile");
    }
}

#[tokio::test]
async fn stays_on_the_primary_unless_asked() {
    let (primary, _replica) = pair(false, None).await;
    let (mut c, target) =
        conn::connect_for_scan(&profile(primary.port, false), &MemoryStore::default())
            .await
            .unwrap();
    assert_eq!(target, ScanTarget::Primary);
    assert!(info(&mut c).await.contains("role:master"));
}

#[tokio::test]
async fn an_already_replica_profile_is_scanned_as_is() {
    let (_primary, replica) = pair(false, None).await;
    let (_c, target) =
        conn::connect_for_scan(&profile(replica.port, true), &MemoryStore::default())
            .await
            .unwrap();
    assert!(matches!(target, ScanTarget::Replica(ep) if ep.port == replica.port));
}

#[tokio::test]
async fn falls_back_to_the_primary_when_the_replica_is_unreachable() {
    for valkey in [false, true] {
        // The replica tells the primary to list it on a port nobody listens on.
        let dead = common::free_port();
        let (primary, _replica) = pair(valkey, Some(dead)).await;
        let (mut c, target) =
            conn::connect_for_scan(&profile(primary.port, true), &MemoryStore::default())
                .await
                .unwrap();
        let ScanTarget::PrimaryFallback(why) = target else {
            panic!("expected a fallback, got {target:?} (valkey={valkey})");
        };
        assert!(why.contains(&format!("127.0.0.1:{dead}")), "{why}");
        assert!(info(&mut c).await.contains("role:master"));
    }
}

#[tokio::test]
async fn a_primary_without_replicas_falls_back() {
    let primary = common::redis().await;
    let (_c, target) =
        conn::connect_for_scan(&profile(primary.port, true), &MemoryStore::default())
            .await
            .unwrap();
    assert_eq!(
        target,
        ScanTarget::PrimaryFallback("no online replica".into())
    );
}

async fn command_stats(c: &mut MultiplexedConnection) -> String {
    redis::cmd("INFO")
        .arg("commandstats")
        .query_async(c)
        .await
        .unwrap()
}

/// Scan through `c` and check it saw the primary's keys only.
async fn scan_sees_only_the_primary(c: &mut MultiplexedConnection, port: u16) {
    let p = profile(port, true);
    let perms = check_permissions(c).await;
    let report = scan::scan("p", c, &ScanOptions::for_profile(&p), &perms, None)
        .await
        .unwrap();
    assert_eq!(report.sampled_keys, KEYS);
    assert!(
        report
            .patterns
            .iter()
            .all(|p| !p.pattern.starts_with("other"))
    );
}

/// The replication-ID check: a server that is not a replica of this primary, but sits
/// on the port the primary announces, is never scanned. Here it answers `INFO` without
/// credentials, so it is rejected before any `AUTH` is sent.
#[tokio::test]
async fn an_unrelated_server_on_the_announced_port_is_not_scanned() {
    for valkey in [false, true] {
        let primary = common::server_with(valkey, &LENS_USER).await;
        seed(&primary).await;
        let unrelated = common::server_with(valkey, &LENS_USER).await;
        let mut u = unrelated.admin().await;
        let _: () = redis::cmd("SET")
            .arg("other:1")
            .arg("x")
            .query_async(&mut u)
            .await
            .unwrap();
        let replica = common::replica_of(&primary, valkey, Some(unrelated.port)).await;
        wait_for_replication(&primary, &replica).await;

        let p = keychain_profile(primary.port);
        let store = CountingStore::with_lens(&p);
        let (mut c, target) = conn::connect_for_scan(&p, &store).await.unwrap();
        assert_eq!(
            target,
            ScanTarget::PrimaryFallback(format!(
                "replica at 127.0.0.1:{} is not a replica of this primary",
                unrelated.port
            )),
            "valkey={valkey}"
        );
        assert!(info(&mut c).await.contains("role:master"));
        scan_sees_only_the_primary(&mut c, primary.port).await;

        let stats = command_stats(&mut u).await;
        for cmd in ["scan", "type", "pttl", "memory", "auth", "hello"] {
            assert!(
                !stats.contains(&format!("cmdstat_{cmd}:")),
                "{cmd} reached the unrelated server (valkey={valkey}): {stats}"
            );
        }
    }
}

/// The same with every server requiring `AUTH`: the candidate on the primary's host gets
/// the credentials, fails the replication-ID check and is still not scanned.
#[tokio::test]
async fn an_unrelated_server_that_needs_auth_is_not_scanned() {
    for valkey in [false, true] {
        let primary = common::server_with(valkey, &LENS_ONLY).await;
        let mut pc = primary.admin_as(USER, PASSWORD).await;
        seed_conn(&mut pc).await;
        let unrelated = common::server_with(valkey, &LENS_ONLY).await;
        let mut u = unrelated.admin_as(USER, PASSWORD).await;
        let replica =
            common::replica_of_with(&primary, valkey, Some(unrelated.port), &LENS_ONLY_REPLICA)
                .await;
        let mut rc = replica.admin_as(USER, PASSWORD).await;
        wait_until_replicating(&mut pc, &mut rc).await;

        let p = keychain_profile(primary.port);
        let store = CountingStore::with_lens(&p);
        let (mut c, target) = conn::connect_for_scan(&p, &store).await.unwrap();
        assert_eq!(
            target,
            ScanTarget::PrimaryFallback(format!(
                "replica at 127.0.0.1:{} is not a replica of this primary",
                unrelated.port
            )),
            "valkey={valkey}"
        );
        assert_eq!(store.gets(), 1);
        assert!(info(&mut c).await.contains("role:master"));
        scan_sees_only_the_primary(&mut c, primary.port).await;
        let stats = command_stats(&mut u).await;
        assert!(!stats.contains("cmdstat_scan:"), "valkey={valkey}: {stats}");
    }
}

/// An ACL user on a primary and replica that both refuse unauthenticated commands:
/// the keychain is read once, and the replica is used.
#[tokio::test]
async fn scans_an_acl_replica_with_keychain_credentials_read_once() {
    for valkey in [false, true] {
        let primary = common::server_with(valkey, &LENS_ONLY).await;
        let mut pc = primary.admin_as(USER, PASSWORD).await;
        seed_conn(&mut pc).await;
        let replica = common::replica_of_with(&primary, valkey, None, &LENS_ONLY_REPLICA).await;
        let mut rc = replica.admin_as(USER, PASSWORD).await;
        wait_until_replicating(&mut pc, &mut rc).await;

        let p = keychain_profile(primary.port);
        let store = CountingStore::with_lens(&p);
        let (mut c, target) = conn::connect_for_scan(&p, &store).await.unwrap();
        assert_eq!(
            target,
            ScanTarget::Replica(local(replica.port)),
            "valkey={valkey}"
        );
        assert_eq!(store.gets(), 1, "valkey={valkey}");
        assert!(info(&mut c).await.contains("role:slave"));
        scan_sees_only_the_primary(&mut c, primary.port).await;
    }
}

/// A candidate that accepts TCP connections and never answers: the 2 s replica timeout
/// fires and the scan stays on the primary.
#[tokio::test]
async fn a_candidate_that_never_answers_times_out() {
    for valkey in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let silent = listener.local_addr().unwrap().port();
        // Accept and hold every connection without reading or writing.
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in listener.incoming() {
                match s {
                    Ok(s) => held.push(s),
                    Err(_) => break,
                }
            }
        });
        let (primary, _replica) = pair(valkey, Some(silent)).await;

        let started = Instant::now();
        let (mut c, target) =
            conn::connect_for_scan(&profile(primary.port, true), &MemoryStore::default())
                .await
                .unwrap();
        let took = started.elapsed();
        assert_eq!(
            target,
            ScanTarget::PrimaryFallback(format!("127.0.0.1:{silent}: timed out")),
            "valkey={valkey}"
        );
        assert!(
            took >= Duration::from_secs(2) && took < Duration::from_secs(4),
            "took {took:?}"
        );
        assert!(info(&mut c).await.contains("role:master"));
    }
}
