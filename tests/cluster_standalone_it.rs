//! "Force standalone" against real clusters (Redis and Valkey, 3 primaries in one
//! container): the seed node is read alone, with no `CLUSTER SHARDS` or `CLUSTER SLOTS`,
//! and a key on another node gets the fixed line, never a followed redirect.
#![cfg(feature = "integration-tests")]

mod common;

use std::time::Duration;

use common::cluster::{TestCluster, cluster};
use redis_lens::budget::Budget;
use redis_lens::conn::cluster::{ScanTargets, connect_targets};
use redis_lens::live::cmdline::{Args, Reply as CmdReply, Request as CmdRequest};
use redis_lens::live::keys::{FactKey, Reply as KeysReply, Request as KeysRequest};
use redis_lens::live::{
    self, Dest, LiveError, LiveHandle, LiveReply, LiveRequest, ReqId, STANDALONE_REDIRECT,
};
use redis_lens::model::{Auth, Endpoint, Env, KeyType, NOTE_ONE_NODE, Profile, ServerMode};
use redis_lens::scan::ScanOptions;
use redis_lens::scan::cluster::scan_targets;
use redis_lens::secrets::MemoryStore;
use tokio::sync::mpsc;

const KEYS: u64 = 900;
const WAIT: Duration = Duration::from_secs(15);

fn profile(port: u16) -> Profile {
    Profile {
        name: "node".into(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
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
        force_standalone: true,
        tags: Vec::new(),
    }
}

async fn dbsize(c: &TestCluster, port: u16) -> u64 {
    let mut a = c.admin(port).await;
    redis::cmd("DBSIZE").query_async(&mut a).await.unwrap()
}

/// A key the seed does not own (a plain `GET` to it answers `MOVED`), and one it does.
async fn foreign_and_own_key(c: &TestCluster) -> (String, String) {
    let mut a = c.admin(c.seed_port()).await;
    let (mut foreign, mut own) = (None, None);
    for i in 0..200 {
        let key = format!("user:{i}");
        let r: redis::RedisResult<Option<String>> =
            redis::cmd("GET").arg(&key).query_async(&mut a).await;
        match r {
            Err(e) if e.kind() == redis::ErrorKind::Server(redis::ServerErrorKind::Moved) => {
                foreign.get_or_insert(key);
            }
            Ok(_) => {
                own.get_or_insert(key);
            }
            Err(e) => panic!("GET {key}: {e}"),
        }
        if foreign.is_some() && own.is_some() {
            break;
        }
    }
    (
        foreign.expect("a key on another node"),
        own.expect("a key on the seed"),
    )
}

type Answer = Result<LiveReply, LiveError>;

async fn ask(
    h: &LiveHandle,
    rx: &mut mpsc::UnboundedReceiver<(ReqId, Answer)>,
    id: u64,
    req: LiveRequest,
) -> Answer {
    assert!(h.send_to(ReqId(id), Dest::default(), req));
    let (got, answer) = tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("an answer in time")
        .expect("the task is alive");
    assert_eq!(got, ReqId(id));
    answer
}

fn facts(key: &str) -> LiveRequest {
    LiveRequest::Keys(KeysRequest::Facts {
        keys: vec![FactKey {
            key: key.into(),
            key_type: KeyType::String,
            length: true,
        }],
    })
}

/// The scan reads the seed's keys only and sends no discovery command.
async fn scan_reads_one_node(valkey: bool) {
    let c = cluster(valkey, false).await;
    let keys: Vec<(String, String)> = (0..KEYS)
        .map(|i| (format!("user:{i}"), "x".repeat(20)))
        .collect();
    c.seed(&keys).await;
    let seed_keys = dbsize(&c, c.seed_port()).await;
    assert!(
        0 < seed_keys && seed_keys < KEYS,
        "the seed holds part of the keys: {seed_keys} of {KEYS}"
    );

    let p = profile(c.seed_port());
    c.reset_stats().await;
    let mut targets = connect_targets(&p, &MemoryStore::default())
        .await
        .map_err(|e| e.to_string())
        .expect("connect");
    assert!(matches!(targets, ScanTargets::Single { .. }));
    let perms = targets.check_permissions().await;
    let opts = ScanOptions::for_profile(&p);
    let mut report = scan_targets(&p.name, &mut targets, &opts, &perms, None, None)
        .await
        .expect("scan");
    report.note_standalone_read(&p);

    assert_eq!(report.server.mode, ServerMode::Cluster);
    assert!(report.nodes.is_none());
    assert!(report.complete);
    assert_eq!(report.sampled_keys, seed_keys, "valkey={valkey}");
    assert_eq!(report.server.total_keys, seed_keys, "valkey={valkey}");
    assert!(report.sampled_keys < KEYS);
    assert_eq!(report.note.as_deref(), Some(NOTE_ONE_NODE));

    // The seed was never asked for the cluster's shape, and the other nodes were not
    // spoken to.
    let seed = c.commandstats(c.seed_port()).await;
    let cluster_cmds: Vec<_> = seed.keys().filter(|k| k.starts_with("cluster")).collect();
    assert!(cluster_cmds.is_empty(), "valkey={valkey}: {cluster_cmds:?}");
    for port in c.ports.iter().filter(|p| **p != c.seed_port()) {
        let stats = c.commandstats(*port).await;
        assert!(
            !stats.contains_key("scan") && !stats.contains_key("type"),
            "valkey={valkey}: node {port} was read: {stats:?}"
        );
    }
}

/// A key on another node: the fixed line, and nothing is sent to that node.
async fn foreign_key_gets_the_fixed_line(valkey: bool) {
    let c = cluster(valkey, false).await;
    let keys: Vec<(String, String)> = (0..KEYS)
        .map(|i| (format!("user:{i}"), "x".repeat(20)))
        .collect();
    c.seed(&keys).await;
    let (foreign, own) = foreign_and_own_key(&c).await;

    let p = profile(c.seed_port());
    let targets = connect_targets(&p, &MemoryStore::default())
        .await
        .map_err(|e| e.to_string())
        .expect("connect");
    let ScanTargets::Single { conn, .. } = targets else {
        panic!("force standalone must not discover");
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let h = live::spawn_forced_standalone(
        conn.into_inner(),
        Budget::new(None),
        move |id, r| {
            let _ = tx.send((id, r));
        },
        |_| {},
    );
    c.reset_stats().await;

    // A key read.
    let r = ask(&h, &mut rx, 1, facts(&foreign)).await;
    assert_eq!(
        r,
        Err(LiveError::Failed(STANDALONE_REDIRECT.into())),
        "valkey={valkey}"
    );
    // A key the seed owns reads as usual.
    let r = ask(&h, &mut rx, 2, facts(&own)).await;
    let Ok(LiveReply::Keys(KeysReply::Facts { facts: got, .. })) = r else {
        panic!("valkey={valkey}: {r:?}");
    };
    assert_eq!(got[0].1.length, Some(20), "valkey={valkey}");

    // The command line: `GET` of the foreign key.
    let r = ask(
        &h,
        &mut rx,
        3,
        LiveRequest::Command(CmdRequest::Info { name: "get".into() }),
    )
    .await;
    let Ok(LiveReply::Command(CmdReply::Info(Some(info)))) = r else {
        panic!("valkey={valkey}: {r:?}");
    };
    let run = |key: &str| {
        LiveRequest::Command(CmdRequest::Run {
            args: Args::new(vec![b"get".to_vec(), key.as_bytes().to_vec()]),
            info: info.clone(),
            confirmed: false,
        })
    };
    let r = ask(&h, &mut rx, 4, run(&foreign)).await;
    assert_eq!(
        r,
        Err(LiveError::Failed(STANDALONE_REDIRECT.into())),
        "valkey={valkey}"
    );
    let r = ask(&h, &mut rx, 5, run(&own)).await;
    assert!(
        matches!(r, Ok(LiveReply::Command(CmdReply::Ran(ref o))) if !o.error),
        "valkey={valkey}: {r:?}"
    );

    // Nothing reached another node: the redirect was not followed.
    for port in c.ports.iter().filter(|p| **p != c.seed_port()) {
        let stats = c.commandstats(*port).await;
        assert!(
            !stats.contains_key("get") && !stats.contains_key("strlen"),
            "valkey={valkey}: node {port} was sent a command: {stats:?}"
        );
    }
}

#[tokio::test]
async fn redis_scan_reads_one_node() {
    scan_reads_one_node(false).await;
}

#[tokio::test]
async fn valkey_scan_reads_one_node() {
    scan_reads_one_node(true).await;
}

#[tokio::test]
async fn redis_foreign_key_gets_the_fixed_line() {
    foreign_key_gets_the_fixed_line(false).await;
}

#[tokio::test]
async fn valkey_foreign_key_gets_the_fixed_line() {
    foreign_key_gets_the_fixed_line(true).await;
}
