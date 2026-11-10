//! Scanning a replica instead of the primary (`prefer_replica`).
#![cfg(feature = "integration-tests")]

mod common;

use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use redis_lens::conn::{self, Replication, ScanTarget, check_permissions};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::scan::{self, ScanOptions};
use redis_lens::secrets::MemoryStore;

const KEYS: u64 = 25;

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
    let mut c = server.admin().await;
    for i in 0..KEYS {
        let _: () = redis::cmd("SET")
            .arg(format!("user:{i}:profile"))
            .arg("x")
            .query_async(&mut c)
            .await
            .unwrap();
    }
}

/// Wait until the replica's link is up and the primary lists an online replica.
async fn wait_for_replication(primary: &common::TestServer, replica: &common::TestServer) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut r = replica.admin().await;
    let mut p = primary.admin().await;
    loop {
        let up = info(&mut r).await.contains("master_link_status:up");
        let listed = matches!(
            conn::parse_replication(&info(&mut p).await),
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
        assert!(info(&mut c).await.contains("role:slave"));

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
