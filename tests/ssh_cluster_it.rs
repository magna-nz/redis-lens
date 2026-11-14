//! A cluster behind an SSH bastion, on Redis and Valkey.
//!
//! The cluster runs in one container on the bastion's Docker network with nothing
//! published, its nodes announcing their address on that network, so the only way to
//! any node is a forward through the bastion (see `common::ssh` and
//! `common::cluster::hidden_cluster`). For the credential rule, a second layout runs the
//! cluster in the bastion's own network namespace, announcing 127.0.0.1.
#![cfg(feature = "integration-tests")]

mod common;

use common::cluster::{ClusterPlace, HiddenCluster, hidden_cluster};
use common::ssh::{self as bastion, Key, SSH_USER, SshBastion};
use redis_lens::conn::cluster::{ClusterTargets, ScanTargets, connect_targets};
use redis_lens::model::{
    Auth, Credentials, Endpoint, Env, NodeRole, NodeStatus, PERM_SCAN, Profile, ScanReport,
};
use redis_lens::scan::ScanOptions;
use redis_lens::scan::cluster::scan_targets;
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;

const USERS: u64 = 600;
const SESSIONS: u64 = 300;
const TOTAL: u64 = USERS + SESSIONS;
/// An ACL user `lens`; the default user stays on for the cluster's own tooling.
const LENS: [&str; 7] = ["--user", "lens", "on", ">lens-pw", "~*", "&*", "+@all"];

struct World {
    dir: std::path::PathBuf,
    key: Key,
    known_hosts: std::path::PathBuf,
    bastion: SshBastion,
}

impl World {
    async fn new(tag: &str) -> Self {
        let dir = bastion::temp_dir(tag);
        let key = bastion::keygen(&dir, "id_ed25519");
        let bastion = SshBastion::start(&bastion::unique(tag), &key).await;
        let known_hosts = bastion.known_hosts(&dir);
        Self {
            dir,
            key,
            known_hosts,
            bastion,
        }
    }

    /// A cluster on the bastion's network, announcing its network address.
    async fn cluster(&self, valkey: bool, replicas: bool) -> HiddenCluster {
        let name = bastion::unique("cluster");
        let place = ClusterPlace::Network {
            network: &self.bastion.network,
            name: &name,
        };
        hidden_cluster(place, valkey, replicas, &[]).await
    }

    fn profile(&self, host: &str, prefer_replica: bool) -> Profile {
        Profile {
            name: "behind".into(),
            env: Env::Prod,
            endpoint: Endpoint {
                host: host.into(),
                port: 7000,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
            prefer_replica,
            provider: None,
            token_command: None,
            ssh: Some(self.bastion.tunnel(&self.key, &self.known_hosts)),
            sentinel: None,
        }
    }

    /// A store with the SSH user, and `lens` for a keychain profile.
    fn store(&self, p: &Profile) -> MemoryStore {
        let s = MemoryStore::default();
        s.set(
            &p.ssh_secret_ref(),
            &Credentials {
                username: Some(SSH_USER.into()),
                password: SecretString::from(String::new()),
            },
        )
        .unwrap();
        if p.auth == Auth::Keychain {
            s.set(
                &p.secret_ref(),
                &Credentials {
                    username: Some("lens".into()),
                    password: SecretString::from("lens-pw".to_string()),
                },
            )
            .unwrap();
        }
        s
    }

    fn forwards(&self) -> usize {
        bastion::ssh_processes(self.bastion.port)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn seed(c: &HiddenCluster) {
    c.seed("user:", USERS).await;
    c.seed("session:", SESSIONS).await;
    c.wait_for_keys(TOTAL).await;
}

async fn connect(p: &Profile, store: &dyn SecretStore) -> ScanTargets {
    match connect_targets(p, store).await {
        Ok(t @ ScanTargets::Cluster(_)) => t,
        Ok(ScanTargets::Single { .. }) => panic!("a cluster node was taken for a standalone one"),
        Err(e) => panic!("connect failed: {e}"),
    }
}

fn nodes(t: &ScanTargets) -> &ClusterTargets {
    match t {
        ScanTargets::Cluster(c) => c,
        ScanTargets::Single { .. } => unreachable!(),
    }
}

async fn scan(p: &Profile, t: &mut ScanTargets) -> ScanReport {
    let perms = t.check_permissions().await;
    assert!(perms.allowed(PERM_SCAN), "{perms:?}");
    scan_targets(&p.name, t, &ScanOptions::for_profile(p), &perms, None, None)
        .await
        .expect("cluster scan")
}

/// The merged patterns hold every key of every shard.
fn assert_merged(r: &ScanReport) {
    for (name, want) in [("user:{id}", USERS), ("session:{id}", SESSIONS)] {
        let row = r
            .patterns
            .iter()
            .find(|p| p.pattern == name)
            .unwrap_or_else(|| panic!("{name} in {:?}", r.patterns));
        assert_eq!(row.sampled_keys, want, "{name}");
        assert_eq!(row.est_keys, want, "{name}");
    }
    assert_eq!(r.sampled_keys, TOTAL);
}

// --- Primaries ------------------------------------------------------------------------

/// Every shard is reached through its own forward, at the address the cluster
/// announced, and scanned; the forwards close when the targets are dropped.
async fn scans_a_cluster_through_the_bastion(valkey: bool) {
    let w = World::new("cluster").await;
    let c = w.cluster(valkey, false).await;
    seed(&c).await;
    c.reset_stats().await;
    // The seed by container name, which only the bastion can resolve.
    let p = w.profile(&c.host, false);
    let store = w.store(&p);
    let mut t = connect(&p, &store).await;
    let targets = nodes(&t);
    assert_eq!(targets.summary(), "cluster · 3 shards");
    for n in &targets.nodes {
        assert!(n.conn.is_some(), "{:?}", n.note);
        assert_eq!(n.role, NodeRole::Primary);
        assert_eq!(
            n.endpoint.host, c.announced,
            "the announced address, not the forward"
        );
        assert!(c.ports.contains(&n.endpoint.port), "{}", n.endpoint);
    }
    // One forward to the seed (by name), one to each other primary (by address).
    assert_eq!(w.forwards(), 3);

    let r = scan(&p, &mut t).await;
    assert!(r.complete);
    assert_merged(&r);
    let reports = r.nodes.as_ref().expect("per-node results");
    assert_eq!(reports.len(), 3);
    let mut ports: Vec<u16> = reports.iter().map(|n| n.endpoint.port).collect();
    ports.sort_unstable();
    assert_eq!(ports, [7000, 7001, 7002]);
    for n in reports {
        assert_eq!(n.status, NodeStatus::Scanned);
        assert_eq!(n.endpoint.host, c.announced);
        assert_eq!(n.sampled_keys, c.dbsize(n.endpoint.port).await);
    }
    // The seed got no INFO that the other primaries didn't: cluster detection's
    // `INFO server` was the scan's own.
    let info: Vec<u64> = {
        let mut v = Vec::new();
        for port in &c.ports {
            v.push(c.commandstats(*port).await.get("info").map_or(0, |s| s.0));
        }
        v
    };
    assert!(info[0] > 0);
    assert!(
        info.iter().all(|n| *n == info[0]),
        "INFO calls per primary: {info:?}"
    );

    drop(t);
    assert_eq!(bastion::wait_for_ssh_processes(w.bastion.port, 0).await, 0);
}

#[tokio::test]
async fn scans_a_cluster_through_the_bastion_redis() {
    scans_a_cluster_through_the_bastion(false).await;
}

#[tokio::test]
async fn scans_a_cluster_through_the_bastion_valkey() {
    scans_a_cluster_through_the_bastion(true).await;
}

// --- Replicas -------------------------------------------------------------------------

/// With `prefer_replica`, each shard's replica gets its own forward and is scanned in
/// READONLY mode.
async fn scans_cluster_replicas_through_the_bastion(valkey: bool) {
    let w = World::new("creplica").await;
    let c = w.cluster(valkey, true).await;
    seed(&c).await;
    let p = w.profile(&c.host, true);
    let store = w.store(&p);
    let mut t = connect(&p, &store).await;
    let targets = nodes(&t);
    let notes: Vec<_> = targets.nodes.iter().map(|n| n.note.clone()).collect();
    assert_eq!(
        targets.summary(),
        "cluster · 3 shards · replicas 3/3",
        "{notes:?}"
    );
    for n in &targets.nodes {
        assert_eq!(n.role, NodeRole::Replica);
        assert_ne!(n.id, n.primary_id);
        assert_eq!(n.endpoint.host, c.announced);
        assert!(c.ports[3..].contains(&n.endpoint.port), "{}", n.endpoint);
    }
    // Three primaries (checked for their replication ID) and three replicas.
    assert_eq!(w.forwards(), 6);

    let r = scan(&p, &mut t).await;
    assert_merged(&r);
    for n in r.nodes.as_ref().unwrap() {
        assert_eq!(n.status, NodeStatus::Scanned);
        assert_eq!(n.role, NodeRole::Replica);
        assert_eq!(n.endpoint.host, c.announced);
    }
    drop(t);
    assert_eq!(bastion::wait_for_ssh_processes(w.bastion.port, 0).await, 0);
}

#[tokio::test]
async fn scans_cluster_replicas_through_the_bastion_redis() {
    scans_cluster_replicas_through_the_bastion(false).await;
}

#[tokio::test]
async fn scans_cluster_replicas_through_the_bastion_valkey() {
    scans_cluster_replicas_through_the_bastion(true).await;
}

// --- A node the bastion can't reach ---------------------------------------------------

/// A primary that is down: the bastion opens its forward but can't connect through it.
/// That shard is a failed node with a fixed note; the others are scanned.
async fn a_node_the_bastion_cant_reach_fails_alone(valkey: bool) {
    let w = World::new("cdown").await;
    let c = w.cluster(valkey, false).await;
    seed(&c).await;
    let down = c.ports[2];
    let down_keys = c.dbsize(down).await;
    c.stop_node(down).await;

    let p = w.profile(&c.host, false);
    let mut t = connect(&p, &w.store(&p)).await;
    let targets = nodes(&t);
    assert_eq!(targets.summary(), "cluster · 3 shards · 1 unreachable");
    let failed = targets
        .nodes
        .iter()
        .find(|n| n.endpoint.port == down)
        .expect("the stopped node is still announced");
    assert!(failed.conn.is_none());
    let note = failed.note.clone().unwrap_or_default();
    assert!(
        note == format!("{}:{down}: could not connect", c.announced)
            || note == format!("{}:{down}: timed out", c.announced),
        "{note}"
    );
    assert!(!note.contains(SSH_USER));

    let r = scan(&p, &mut t).await;
    assert!(!r.complete);
    assert_eq!(r.sampled_keys, TOTAL - down_keys);
    for n in r.nodes.as_ref().unwrap() {
        if n.endpoint.port == down {
            assert_eq!(n.status, NodeStatus::Failed);
            assert_eq!(n.note.as_deref(), Some(note.as_str()));
        } else {
            assert_eq!(n.status, NodeStatus::Scanned);
        }
    }
    drop(t);
    assert_eq!(bastion::wait_for_ssh_processes(w.bastion.port, 0).await, 0);
}

#[tokio::test]
async fn a_node_the_bastion_cant_reach_fails_alone_redis() {
    a_node_the_bastion_cant_reach_fails_alone(false).await;
}

#[tokio::test]
async fn a_node_the_bastion_cant_reach_fails_alone_valkey() {
    a_node_the_bastion_cant_reach_fails_alone(true).await;
}

// --- The credential rule, judged from the bastion -------------------------------------

/// A cluster on the bastion's own loopback, announcing 127.0.0.1, for a profile with
/// credentials. Reached as 127.0.0.1 (loopback on the bastion), every node gets them.
/// Reached at the bastion's network address, the seed is not loopback there, so the
/// nodes announced as 127.0.0.1 get none: their shards are left out with a note, and
/// only the seed's own shard (its connection reused) is scanned.
async fn loopback_nodes_get_credentials_only_from_a_loopback_seed(valkey: bool) {
    let w = World::new("ctrust").await;
    let c = hidden_cluster(
        ClusterPlace::Namespace(w.bastion.id()),
        valkey,
        false,
        &LENS,
    )
    .await;
    seed(&c).await;

    let mut on_bastion = w.profile("127.0.0.1", false);
    on_bastion.auth = Auth::Keychain;
    let mut t = connect(&on_bastion, &w.store(&on_bastion)).await;
    let targets = nodes(&t);
    assert_eq!(targets.summary(), "cluster · 3 shards");
    for n in &targets.nodes {
        assert!(n.conn.is_some(), "{:?}", n.note);
        assert_eq!(n.endpoint.host, "127.0.0.1");
        assert!(
            c.ports.contains(&n.endpoint.port),
            "the announced port, not the forward's"
        );
    }
    let r = scan(&on_bastion, &mut t).await;
    assert_merged(&r);
    drop(t);

    let mut remote = w.profile(&w.bastion.ip().await, false);
    remote.auth = Auth::Keychain;
    let t = connect(&remote, &w.store(&remote)).await;
    let targets = nodes(&t);
    assert_eq!(targets.summary(), "cluster · 3 shards · 2 unreachable");
    for n in &targets.nodes {
        if n.endpoint.port == 7000 {
            assert!(n.conn.is_some(), "the seed's own shard: {:?}", n.note);
        } else {
            assert!(n.conn.is_none());
            let note = n.note.as_deref().unwrap_or_default();
            assert!(
                note.contains("is a loopback address") && note.contains("credentials are not sent"),
                "{note}"
            );
        }
    }
    drop(t);
    assert_eq!(bastion::wait_for_ssh_processes(w.bastion.port, 0).await, 0);
}

#[tokio::test]
async fn loopback_nodes_get_credentials_only_from_a_loopback_seed_redis() {
    loopback_nodes_get_credentials_only_from_a_loopback_seed(false).await;
}

#[tokio::test]
async fn loopback_nodes_get_credentials_only_from_a_loopback_seed_valkey() {
    loopback_nodes_get_credentials_only_from_a_loopback_seed(true).await;
}

// --- A standalone server through `connect_targets` ------------------------------------

async fn stat(c: &mut redis::aio::MultiplexedConnection, section: &str, field: &str) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg(section)
        .query_async(c)
        .await
        .unwrap();
    info.lines()
        .find_map(|l| l.trim().strip_prefix(field)?.strip_prefix(':'))
        .map_or(0, |v| v.trim().parse().unwrap())
}

/// `cmdstat_info` calls, from `INFO commandstats`.
async fn info_calls(c: &mut redis::aio::MultiplexedConnection) -> u64 {
    let text: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(c)
        .await
        .unwrap();
    common::cluster::parse_commandstats(&text)
        .get("info")
        .map_or(0, |s| s.0)
}

fn direct(port: u16, prefer_replica: bool) -> Profile {
    Profile {
        name: "standalone".into(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only: None,
        prefer_replica,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
    }
}

/// A standalone `prefer_replica` scan through `connect_targets` connects to the primary
/// once (the replica step reuses the seed's connection), and a scan through it sends
/// the server no more `INFO` than one through `connect_for_scan`.
async fn a_standalone_scan_opens_the_primary_once_and_adds_no_info(valkey: bool) {
    let primary = common::server_with(valkey, &[]).await;
    let replica = common::replica_of(&primary, valkey, None).await;
    let mut admin = primary.admin().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !matches!(
        redis_lens::conn::parse_replication(&{
            let i: String = redis::cmd("INFO").arg("replication").query_async(&mut admin).await.unwrap();
            i
        }),
        redis_lens::conn::Replication::Master { online } if !online.is_empty()
    ) {
        assert!(std::time::Instant::now() < deadline, "no replica");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let store = MemoryStore::default();

    let before = stat(&mut admin, "stats", "total_connections_received").await;
    let t = connect_targets(&direct(primary.port, true), &store)
        .await
        .unwrap();
    let ScanTargets::Single { target, .. } = &t else {
        panic!("a standalone server was taken for a cluster");
    };
    assert!(
        matches!(target, redis_lens::conn::ScanTarget::Replica(ep) if ep.port == replica.port),
        "{target}"
    );
    let after = stat(&mut admin, "stats", "total_connections_received").await;
    assert_eq!(after - before, 1, "connections to the primary");
    drop(t);

    // The same scan of the primary through `connect_for_scan` and `connect_targets`:
    // the same number of INFO calls.
    let p = direct(primary.port, false);
    let opts = ScanOptions::for_profile(&p);
    let start = info_calls(&mut admin).await;
    let (mut c, _) = redis_lens::conn::connect_for_scan(&p, &store)
        .await
        .unwrap();
    let perms = redis_lens::conn::check_permissions(&mut c).await;
    redis_lens::scan::scan(&p.name, &mut c, &opts, &perms, None)
        .await
        .unwrap();
    drop(c);
    let middle = info_calls(&mut admin).await;
    let mut t = connect_targets(&p, &store).await.unwrap();
    let perms = t.check_permissions().await;
    scan_targets(&p.name, &mut t, &opts, &perms, None, None)
        .await
        .unwrap();
    drop(t);
    let end = info_calls(&mut admin).await;
    // Each reading counts the one before it.
    assert_eq!(end - middle, middle - start, "INFO calls per scan");
}

#[tokio::test]
async fn a_standalone_scan_opens_the_primary_once_and_adds_no_info_redis() {
    a_standalone_scan_opens_the_primary_once_and_adds_no_info(false).await;
}

#[tokio::test]
async fn a_standalone_scan_opens_the_primary_once_and_adds_no_info_valkey() {
    a_standalone_scan_opens_the_primary_once_and_adds_no_info(true).await;
}
