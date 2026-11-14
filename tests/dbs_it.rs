//! The database list and the database switch (spec "Database switch (P4)") against real
//! Redis and Valkey: the list read through the live request path, a connect and a scan
//! on another database, the Pub/Sub tail after a switch, and the feature being off on a
//! cluster with nothing sent.
#![cfg(feature = "integration-tests")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn::cluster::{ScanTargets, connect_targets};
use redis_lens::conn::{self, ScanTarget};
use redis_lens::live::dbs::{Reply as DbsReply, Request as DbsRequest};
use redis_lens::live::keys::{FactKey, Reply as KeysReply, Request as KeysRequest};
use redis_lens::live::nodes::NodeInfo;
use redis_lens::live::pubsub::{self, Push, Subscription};
use redis_lens::live::{self, LiveError, LiveReply, LiveRequest, ReqId};
use redis_lens::model::{Auth, Endpoint, Env, KeyType, PermissionReport, Profile};
use redis_lens::scan::{self, ScanOptions};
use redis_lens::secrets::MemoryStore;
use redis_lens::tui::app::{App, Effect, Msg};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(15);

fn profile(port: u16, db: u32) -> Profile {
    Profile {
        name: "dbs".into(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port,
            tls: false,
        },
        db,
        auth: Auth::None,
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        tags: Vec::new(),
    }
}

/// A connection to database `db` of the server, for seeding.
async fn on_db(server: &common::TestServer, db: u32) -> MultiplexedConnection {
    redis::Client::open(format!("{}/{db}", server.url()))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .expect("connect")
}

async fn seed(server: &common::TestServer) {
    let mut d0 = on_db(server, 0).await;
    let mut d3 = on_db(server, 3).await;
    let mut pipe = redis::pipe();
    for i in 0..30 {
        pipe.cmd("SET").arg(format!("alpha:{i}")).arg("v").ignore();
    }
    pipe.cmd("SET")
        .arg("alpha:ttl")
        .arg("v")
        .arg("EX")
        .arg(1000)
        .ignore();
    let _: () = pipe.query_async(&mut d0).await.unwrap();
    let mut pipe = redis::pipe();
    for i in 0..20 {
        pipe.cmd("SET").arg(format!("beta:{i}")).arg("v").ignore();
    }
    let _: () = pipe.query_async(&mut d3).await.unwrap();
}

type Answer = (ReqId, Result<LiveReply, LiveError>);

fn start(
    c: MultiplexedConnection,
    budget: Arc<Budget>,
) -> (live::LiveHandle, mpsc::UnboundedReceiver<Answer>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = live::spawn(c, budget, move |id, r| {
        let _ = tx.send((id, r));
    });
    (handle, rx)
}

async fn ask(
    h: &live::LiveHandle,
    rx: &mut mpsc::UnboundedReceiver<Answer>,
    id: u64,
    req: LiveRequest,
) -> Result<LiveReply, LiveError> {
    assert!(h.send(ReqId(id), req));
    let (got, answer) = timeout(WAIT, rx.recv())
        .await
        .expect("an answer in time")
        .expect("the live task is alive");
    assert_eq!(got, ReqId(id));
    answer
}

fn keyspace() -> LiveRequest {
    LiveRequest::Dbs(DbsRequest::Keyspace)
}

fn fact_of(key: &str) -> LiveRequest {
    LiveRequest::Keys(KeysRequest::Facts {
        keys: vec![FactKey {
            key: key.into(),
            key_type: KeyType::String,
            length: false,
        }],
    })
}

async fn has_key(
    h: &live::LiveHandle,
    rx: &mut mpsc::UnboundedReceiver<Answer>,
    id: u64,
    key: &str,
) -> bool {
    match ask(h, rx, id, fact_of(key)).await.expect("facts") {
        LiveReply::Keys(KeysReply::Facts { facts, .. }) => !facts.is_empty(),
        other => panic!("unexpected reply {other:?}"),
    }
}

#[tokio::test]
async fn the_list_shows_every_database_with_its_keys_in_one_command() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        // The list is the same whichever database the live connection is on.
        for db in [0, 3] {
            let c = conn::connect(&profile(server.port, db), &MemoryStore::default())
                .await
                .expect("connect")
                .into_inner();
            let budget = Budget::new(None);
            let (h, mut rx) = start(c, Arc::clone(&budget));
            let Ok(LiveReply::Dbs(DbsReply::Keyspace(dbs))) = ask(&h, &mut rx, 1, keyspace()).await
            else {
                panic!("expected the keyspace");
            };
            let shown: Vec<_> = dbs.iter().map(|k| (k.db, k.keys, k.expires)).collect();
            assert_eq!(shown, [(0, 31, 1), (3, 20, 0)], "live on db {db}");
            // One INFO, from the budget.
            assert_eq!(budget.taken(), 1);
        }
    }
}

#[tokio::test]
async fn an_empty_server_lists_no_database() {
    for server in [common::redis().await, common::valkey().await] {
        let c = conn::connect(&profile(server.port, 0), &MemoryStore::default())
            .await
            .expect("connect")
            .into_inner();
        let (h, mut rx) = start(c, Budget::new(None));
        let Ok(LiveReply::Dbs(DbsReply::Keyspace(dbs))) = ask(&h, &mut rx, 1, keyspace()).await
        else {
            panic!("expected the keyspace");
        };
        assert!(dbs.is_empty());
    }
}

#[tokio::test]
async fn after_a_switch_the_scan_and_the_live_connection_are_on_the_new_database() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let store = MemoryStore::default();
        // The switched profile is the session's copy with another `db`.
        let switched = profile(server.port, 3);
        let ScanTargets::Single { mut conn, .. } =
            connect_targets(&switched, &store).await.expect("connect")
        else {
            panic!("a standalone server");
        };
        let perms = conn::check_permissions(&mut conn).await;
        let opts = ScanOptions::for_profile(&switched);
        assert_eq!(opts.db, 3);
        let budget = Budget::new(None);
        let report = scan::scan_with_budget("dbs", &mut conn, &opts, &perms, None, None, &budget)
            .await
            .expect("scan");
        let names: Vec<_> = report.patterns.iter().map(|p| p.pattern.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with("beta")), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("alpha")), "{names:?}");
        assert_eq!(
            report.patterns.iter().map(|p| p.sampled_keys).sum::<u64>(),
            20
        );

        // The live connection reads a db 3 key, and not a db 0 key.
        let (h, mut rx) = start(conn.clone().into_inner(), budget);
        assert!(has_key(&h, &mut rx, 1, "beta:1").await);
        assert!(!has_key(&h, &mut rx, 2, "alpha:1").await);

        // The saved profile (db 0) sees the other way round.
        let c0 = conn::connect(&profile(server.port, 0), &store)
            .await
            .expect("connect")
            .into_inner();
        let (h0, mut rx0) = start(c0, Budget::new(None));
        assert!(has_key(&h0, &mut rx0, 1, "alpha:1").await);
        assert!(!has_key(&h0, &mut rx0, 2, "beta:1").await);
    }
}

#[tokio::test]
async fn a_tail_subscribed_again_after_the_switch_receives_what_is_published_after_it() {
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let store = MemoryStore::default();
        for to in [
            Subscription::Channel("news".into()),
            Subscription::Pattern("orders:*".into()),
        ] {
            let channel = match &to {
                Subscription::Channel(c) => c.clone(),
                Subscription::Pattern(_) => "orders:eu".to_string(),
            };
            // The tail before the switch, on the profile's db 0.
            let (mut before, _rx0) = conn::open_subscriber(&profile(server.port, 0), &store, None)
                .await
                .expect("open the subscriber");
            pubsub::subscribe(&mut before, &Budget::new(None), &to)
                .await
                .expect("subscribe");
            // The switch ends it; the new run subscribes again to the same channel or
            // pattern, on a connection to the new database.
            drop(before);
            let (mut after, mut rx) = conn::open_subscriber(&profile(server.port, 3), &store, None)
                .await
                .expect("open the subscriber on db 3");
            let budget = Budget::new(None);
            pubsub::subscribe(&mut after, &budget, &to)
                .await
                .expect("subscribe again");
            assert_eq!(budget.taken(), 1);

            // Pub/Sub is not per database: a message published from db 0 arrives.
            let mut admin = on_db(&server, 0).await;
            let n: i64 = redis::cmd("PUBLISH")
                .arg(&channel)
                .arg("after the switch")
                .query_async(&mut admin)
                .await
                .unwrap();
            assert_eq!(n, 1, "{to:?}");
            let push = loop {
                let push = timeout(WAIT, rx.recv())
                    .await
                    .expect("a push in time")
                    .expect("the subscriber is alive");
                match pubsub::parse_push(push, 1) {
                    Push::Message(m) => break m,
                    Push::Lost => panic!("connection lost"),
                    Push::Other => {}
                }
            };
            assert_eq!(push.channel, channel);
            assert_eq!(push.payload.as_bytes(), b"after the switch");
        }
    }
}

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// On a cluster `b` is off with its fixed reason, the app asks for nothing, and no node
/// receives a command for it.
#[tokio::test]
async fn on_a_cluster_the_database_list_is_off_and_nothing_is_sent() {
    for valkey in [false, true] {
        let cluster = common::cluster::cluster(valkey, false).await;
        let seed = profile(cluster.seed_port(), 0);
        let ScanTargets::Cluster(t) = connect_targets(&seed, &MemoryStore::default())
            .await
            .expect("connect")
        else {
            panic!("a cluster");
        };
        let nodes: Vec<NodeInfo> = t
            .nodes
            .iter()
            .map(|n| NodeInfo {
                id: n.id.clone(),
                endpoint: n.endpoint.clone(),
                role: n.role,
                slots: n.slots.clone(),
                connected: n.conn.is_some(),
            })
            .collect();
        let ports: Vec<u16> = nodes.iter().map(|n| n.endpoint.port).collect();
        let mut info_before = Vec::new();
        for p in &ports {
            info_before.push(cluster.commandstats(*p).await.get("info").map(|s| s.0));
        }

        let mut app = App::new(vec![seed]);
        let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let [Effect::Connect { id, scan, .. }] = effects[..] else {
            panic!("expected a connect, got {effects:?}");
        };
        app.apply(Msg::Connected {
            id,
            scan,
            server: None,
            perms: PermissionReport::default(),
            target: ScanTarget::Primary,
        });
        app.apply(Msg::Nodes { id, scan, nodes });
        app.apply(Msg::LiveReady { id, scan });

        let effects = app.handle_key(ch('b'));
        assert!(effects.is_empty(), "valkey={valkey}: {effects:?}");
        assert!(app.dbs.is_none());
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("A cluster only has database 0.")
        );

        let mut info_after = Vec::new();
        for p in &ports {
            info_after.push(cluster.commandstats(*p).await.get("info").map(|s| s.0));
        }
        // Only the test's own commandstats reads (INFO commandstats) can move the count,
        // and they do so the same way before and after: nothing came from the app.
        let moved: Vec<_> = info_before
            .iter()
            .zip(&info_after)
            .map(|(b, a)| a.unwrap_or(0) - b.unwrap_or(0))
            .collect();
        assert!(moved.iter().all(|m| *m <= 1), "{moved:?}");
        drop(t);
    }
}
