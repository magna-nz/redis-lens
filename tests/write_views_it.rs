//! The writes of the Server, Streams, Pub/Sub and Search views against real servers,
//! each sent by `live::write::run` with a permit from the gate (`Permit::grant`) on a
//! local profile with writes on: `CLIENT KILL ID`, `CONFIG SET slowlog-*` and
//! `SLOWLOG RESET`, `XAUTOCLAIM`, `XACK` and `XTRIM` (with the `XPENDING` read the
//! Streams view lists pending entries with), `PUBLISH`, and `FT.CREATE` and
//! `FT.DROPINDEX` where the server has Search. Every test runs on Redis and Valkey, and
//! passes with `REDIS_LENS_IT_REDIS_IMAGE=redis:6.2-alpine`.
#![cfg(feature = "integration-tests")]

mod common;

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::live::LiveError;
use redis_lens::live::server::{self, ClientConn};
use redis_lens::live::streams::{self, Refused};
use redis_lens::live::write::{Reply, run};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::tui::app::views::server::is_own;
use redis_lens::value::Raw;
use redis_lens::write::{
    Answer, IndexField, IndexOn, Permit, Refusals, Settings, SlowlogSetting, Trim, WriteOp,
};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

fn profile() -> Profile {
    Profile {
        name: "it".to_string(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port: 6379,
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
    }
}

/// Sends `op` as the TUI would after `y`: through the gate, then the runner.
async fn write(conn: &mut MultiplexedConnection, op: WriteOp) -> Result<Reply, LiveError> {
    let permit = Permit::grant(
        op,
        &profile(),
        &Settings::default(),
        &Refusals::default(),
        &Answer::Yes,
    )
    .expect("a permit on a local profile with writes on");
    run(conn, &Budget::new(None), permit).await
}

async fn cmd<T: redis::FromRedisValue>(conn: &mut MultiplexedConnection, args: &[&str]) -> T {
    let mut c = redis::cmd(args[0]);
    for a in &args[1..] {
        c.arg(*a);
    }
    c.query_async(conn)
        .await
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

async fn clients(conn: &mut MultiplexedConnection) -> Vec<ClientConn> {
    match server::run(conn, &Budget::new(None), server::Request::Clients).await {
        Ok(server::Reply::Clients(list)) => list,
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Server: CLIENT KILL, CONFIG SET, SLOWLOG RESET
// ---------------------------------------------------------------------------

async fn client_kill(s: common::TestServer) {
    let mut lens = s.admin().await;
    let mut victim = s.admin().await;
    let victim_id: u64 = cmd(&mut victim, &["CLIENT", "ID"]).await;
    let own_id: u64 = cmd(&mut lens, &["CLIENT", "ID"]).await;
    // The victim's last command is not a CLIENT one (before Redis 7.0 the list names
    // every CLIENT subcommand `client`, so its CLIENT ID would look like the list).
    let _: String = cmd(&mut victim, &["PING"]).await;

    // The view finds its own connection in the list it reads, and only that one.
    let list = clients(&mut lens).await;
    let own: Vec<u64> = list.iter().filter(|c| is_own(c)).map(|c| c.id).collect();
    assert_eq!(own, vec![own_id], "{list:?}");

    assert_eq!(
        write(&mut lens, WriteOp::ClientKill { id: victim_id }).await,
        Ok(Reply::Int(1))
    );
    let list = clients(&mut lens).await;
    assert!(list.iter().all(|c| c.id != victim_id), "{list:?}");
    // Gone already: nothing killed, and not an error.
    assert_eq!(
        write(&mut lens, WriteOp::ClientKill { id: victim_id }).await,
        Ok(Reply::Int(0))
    );
}

#[tokio::test]
async fn client_kill_redis() {
    client_kill(common::redis().await).await;
}

#[tokio::test]
async fn client_kill_valkey() {
    client_kill(common::valkey().await).await;
}

async fn slow_log(s: common::TestServer) {
    let mut c = s.admin().await;
    let ok = |r: Result<Reply, LiveError>| assert_eq!(r, Ok(Reply::Ok));
    ok(write(&mut c, WriteOp::SlowlogConfig(SlowlogSetting::Threshold(0))).await);
    ok(write(&mut c, WriteOp::SlowlogConfig(SlowlogSetting::MaxLen(200))).await);
    let got: Vec<String> = cmd(&mut c, &["CONFIG", "GET", "slowlog-*"]).await;
    let get = |name: &str| {
        got.chunks(2)
            .find(|p| p[0] == name)
            .map(|p| p[1].clone())
            .unwrap()
    };
    assert_eq!(get("slowlog-log-slower-than"), "0");
    assert_eq!(get("slowlog-max-len"), "200");
    // Every command is logged now.
    let _: () = cmd(&mut c, &["SET", "k", "v"]).await;
    let len: u64 = cmd(&mut c, &["SLOWLOG", "LEN"]).await;
    assert!(len > 0);
    ok(write(&mut c, WriteOp::SlowlogReset).await);
    ok(write(
        &mut c,
        WriteOp::SlowlogConfig(SlowlogSetting::Threshold(-1)),
    )
    .await);
    let len: u64 = cmd(&mut c, &["SLOWLOG", "LEN"]).await;
    assert!(len <= 1, "only the CONFIG SET after the reset: {len}");
}

#[tokio::test]
async fn slow_log_redis() {
    slow_log(common::redis().await).await;
}

#[tokio::test]
async fn slow_log_valkey() {
    slow_log(common::valkey().await).await;
}

// ---------------------------------------------------------------------------
// Streams: XPENDING (read), XAUTOCLAIM, XACK, XTRIM
// ---------------------------------------------------------------------------

async fn pending(conn: &mut MultiplexedConnection, consumer: Option<&str>) -> streams::Pending {
    let req = streams::Request::Pending {
        key: "orders".into(),
        group: "billing".into(),
        consumer: consumer.map(str::to_string),
        refused: Refused::default(),
    };
    let reply = streams::run(conn, &Budget::new(None), req)
        .await
        .expect("XPENDING");
    let streams::Reply::Info { mut streams, .. } = reply;
    let f = streams.pop().expect("one stream");
    assert_eq!(f.key, "orders");
    f.info.expect("a stream").pending.expect("its pending list")
}

async fn stream_writes(s: common::TestServer) {
    let mut c = s.admin().await;
    for i in 0..250 {
        let id = format!("1700000000{i:03}-0");
        let _: String = cmd(&mut c, &["XADD", "orders", &id, "n", &i.to_string()]).await;
    }
    let _: () = cmd(&mut c, &["XGROUP", "CREATE", "orders", "billing", "0"]).await;
    let _: redis::Value = cmd(
        &mut c,
        &[
            "XREADGROUP",
            "GROUP",
            "billing",
            "b-1",
            "COUNT",
            "3",
            "STREAMS",
            "orders",
            ">",
        ],
    )
    .await;

    let p = pending(&mut c, Some("b-1")).await;
    assert_eq!(p.error, None);
    assert_eq!(p.consumer.as_deref(), Some("b-1"));
    let ids: Vec<&str> = p.entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        ["1700000000000-0", "1700000000001-0", "1700000000002-0"]
    );
    assert!(
        p.entries
            .iter()
            .all(|e| e.consumer == "b-1" && e.deliveries == 1)
    );

    // Claimed for b-2.
    let r = write(
        &mut c,
        WriteOp::XAutoClaim {
            key: "orders".into(),
            group: "billing".into(),
            consumer: "b-2".into(),
            min_idle_ms: 0,
            count: 100,
        },
    )
    .await;
    let Ok(Reply::Claimed { ids, deleted }) = r else {
        panic!("{r:?}");
    };
    assert_eq!(ids.len(), 3);
    assert_eq!(deleted, 0);
    assert!(pending(&mut c, Some("b-1")).await.entries.is_empty());
    assert_eq!(pending(&mut c, None).await.entries.len(), 3);

    // Acked once; a second time there is nothing pending.
    let ack = WriteOp::XAck {
        key: "orders".into(),
        group: "billing".into(),
        id: "1700000000000-0".into(),
    };
    assert_eq!(write(&mut c, ack.clone()).await, Ok(Reply::Int(1)));
    assert_eq!(write(&mut c, ack).await, Ok(Reply::Int(0)));
    assert_eq!(pending(&mut c, Some("b-2")).await.entries.len(), 2);

    // An unknown group is said, not failed.
    let req = streams::Request::Pending {
        key: "orders".into(),
        group: "nobody".into(),
        consumer: None,
        refused: Refused::default(),
    };
    let streams::Reply::Info { streams, .. } =
        streams::run(&mut c, &Budget::new(None), req).await.unwrap();
    let p = streams[0].info.as_ref().unwrap().pending.as_ref().unwrap();
    assert!(p.entries.is_empty() && p.error.is_some(), "{p:?}");

    // Trimmed approximately: whole nodes of 100 entries go.
    let r = write(
        &mut c,
        WriteOp::XTrim {
            key: "orders".into(),
            trim: Trim::MaxLen(10),
        },
    )
    .await;
    let Ok(Reply::Int(n)) = r else {
        panic!("{r:?}")
    };
    let len: i64 = cmd(&mut c, &["XLEN", "orders"]).await;
    assert!(n >= 100 && len >= 10 && n + len == 250, "{n} {len}");
    let r = write(
        &mut c,
        WriteOp::XTrim {
            key: "orders".into(),
            trim: Trim::MinId("1700000000249-0".into()),
        },
    )
    .await;
    assert!(matches!(r, Ok(Reply::Int(_))), "{r:?}");
}

#[tokio::test]
async fn stream_writes_redis() {
    stream_writes(common::redis().await).await;
}

#[tokio::test]
async fn stream_writes_valkey() {
    stream_writes(common::valkey().await).await;
}

// ---------------------------------------------------------------------------
// Pub/Sub: PUBLISH
// ---------------------------------------------------------------------------

async fn publish(s: common::TestServer) {
    let mut c = s.admin().await;
    let op = || WriteOp::Publish {
        channel: "orders:eu".into(),
        message: Raw::from("made-up message"),
    };
    assert_eq!(write(&mut c, op()).await, Ok(Reply::Int(0)));
    let client = redis::Client::open(s.url()).unwrap();
    let mut sub = client.get_async_pubsub().await.unwrap();
    sub.subscribe("orders:eu").await.unwrap();
    let mut psub = client.get_async_pubsub().await.unwrap();
    psub.psubscribe("orders:*").await.unwrap();
    // A subscription and a pattern: two receivers.
    let end = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let r = write(&mut c, op()).await;
        if r == Ok(Reply::Int(2)) {
            break;
        }
        assert!(tokio::time::Instant::now() < end, "{r:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop((sub, psub));
}

#[tokio::test]
async fn publish_redis() {
    publish(common::redis().await).await;
}

#[tokio::test]
async fn publish_valkey() {
    publish(common::valkey().await).await;
}

// ---------------------------------------------------------------------------
// Search: FT.CREATE, FT.DROPINDEX
// ---------------------------------------------------------------------------

/// Redis 8 has Redis Search built in; the Valkey bundle loads valkey-search. The
/// tags are the ones `search_it.rs` pins.
const REDIS8: (&str, &str, &str) = ("redis", "8.10.1", "redis-server");
const VALKEY_BUNDLE: (&str, &str, &str) = ("valkey/valkey-bundle", "9.1.3", "valkey-server");

async fn start(image: (&str, &str, &str)) -> (ContainerAsync<GenericImage>, MultiplexedConnection) {
    let container = GenericImage::new(image.0, image.1)
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_cmd(vec![image.2.to_string()])
        .start()
        .await
        .expect("start container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(6379.tcp())
        .await
        .expect("mapped port");
    let conn = redis::Client::open(format!("redis://127.0.0.1:{port}"))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .expect("connect");
    (container, conn)
}

/// The server has Search (`FT._LIST` works).
async fn has_search(c: &mut MultiplexedConnection) -> bool {
    redis::cmd("FT._LIST")
        .query_async::<redis::Value>(c)
        .await
        .is_ok()
}

async fn index_names(c: &mut MultiplexedConnection) -> Vec<String> {
    cmd(c, &["FT._LIST"]).await
}

async fn search_writes(c: &mut MultiplexedConnection, label: &str) {
    if !has_search(c).await {
        println!("{label}: skipped FT.CREATE and FT.DROPINDEX: the server has no Search module");
        return;
    }
    let _: () = cmd(c, &["HSET", "product:1", "title", "lamp", "price", "12"]).await;
    let create = WriteOp::FtCreate {
        index: "idx:it".into(),
        on: IndexOn::Hash,
        prefixes: vec!["product:".into()],
        fields: vec![
            IndexField {
                name: "price".into(),
                alias: None,
                kind: "NUMERIC".into(),
            },
            IndexField {
                name: "title".into(),
                alias: None,
                kind: "TAG".into(),
            },
        ],
    };
    assert_eq!(write(c, create.clone()).await, Ok(Reply::Ok), "{label}");
    assert!(index_names(c).await.contains(&"idx:it".to_string()));
    let again = write(c, create).await;
    assert!(
        matches!(&again, Err(LiveError::Failed(why)) if why.starts_with("FT.CREATE failed")),
        "{label}: {again:?}"
    );
    let drop = WriteOp::FtDropIndex {
        index: "idx:it".into(),
    };
    assert_eq!(write(c, drop.clone()).await, Ok(Reply::Ok), "{label}");
    assert!(!index_names(c).await.contains(&"idx:it".to_string()));
    // Never DD: the documents stay.
    let exists: i64 = cmd(c, &["EXISTS", "product:1"]).await;
    assert_eq!(exists, 1, "{label}");
    let gone = write(c, drop).await;
    assert!(
        matches!(gone, Err(LiveError::Failed(_))),
        "{label}: {gone:?}"
    );
}

#[tokio::test]
async fn search_writes_without_the_module_are_skipped() {
    for (label, s) in [
        ("redis", common::redis().await),
        ("valkey", common::valkey().await),
    ] {
        let mut c = s.admin().await;
        search_writes(&mut c, label).await;
    }
}

#[tokio::test]
async fn search_writes_redis_8() {
    let (_container, mut c) = start(REDIS8).await;
    search_writes(&mut c, "redis 8").await;
}

#[tokio::test]
async fn search_writes_valkey_bundle() {
    let (_container, mut c) = start(VALKEY_BUNDLE).await;
    search_writes(&mut c, "valkey-bundle").await;
}
