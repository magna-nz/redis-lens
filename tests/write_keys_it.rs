//! The key detail's and key list's writes (spec "Edits on the Key detail (P3)"), sent
//! through `live::write::run` with a permit from the gate, against real Redis and
//! Valkey servers. Writes go to the test containers only.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashSet;

use redis::AsyncCommands;
use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn;
use redis_lens::live::LiveError;
use redis_lens::live::LiveRequest;
use redis_lens::live::keys::{self, KeyRow};
use redis_lens::live::values::field_ttls_supported;
use redis_lens::live::write::{self, Reply, list_marker};
use redis_lens::model::{Auth, Endpoint, Env, KeyType, Profile, ServerFlavor, Ttl as KeyTtl};
use redis_lens::secrets::MemoryStore;
use redis_lens::tui::app::views::value::meta_request;
use redis_lens::value::Raw;
use redis_lens::write::{
    Answer, Decision, NewKey, Permit, Refusals, Settings, Ttl, WriteOp, decide_op,
};

/// A local profile with writes on.
fn profile(port: u16) -> Profile {
    Profile {
        name: "write-keys".to_string(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only: Some(false),
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        tags: Vec::new(),
    }
}

struct Lens {
    conn: MultiplexedConnection,
    profile: Profile,
    budget: std::sync::Arc<Budget>,
}

impl Lens {
    async fn new(server: &common::TestServer) -> Self {
        let profile = profile(server.port);
        let conn = conn::connect(&profile, &MemoryStore::default())
            .await
            .expect("connect")
            .into_inner();
        Self {
            conn,
            profile,
            budget: Budget::new(None),
        }
    }

    /// Sends `op` through the gate, with the answer it asks for.
    async fn run(&mut self, op: WriteOp) -> Result<Reply, LiveError> {
        let settings = Settings::default();
        let none = Refusals::default();
        let answer = match decide_op(&op, &self.profile, &settings, &none) {
            Decision::Run => Answer::None,
            Decision::Ask => Answer::Yes,
            Decision::Type(name) => Answer::Typed(name),
            Decision::Refuse(why) => panic!("refused: {why}"),
        };
        let permit = Permit::grant(op, &self.profile, &settings, &none, &answer).expect("a permit");
        write::run(&mut self.conn, &self.budget, permit).await
    }

    /// What the key detail reads after a write: the key list's row of `key` alone.
    async fn row(&mut self, key: &str) -> Option<KeyRow> {
        let was = KeyRow {
            key: key.to_string(),
            key_type: KeyType::Hash,
            ttl: KeyTtl::Persistent,
            bytes: Some(1),
            encoding: None,
            length: None,
            freq: None,
        };
        let LiveRequest::Keys(req) = meta_request(&was, "h") else {
            panic!("not a key list request");
        };
        let keys::Reply::Page(page) = keys::run(&mut self.conn, &self.budget, req)
            .await
            .expect("the row")
        else {
            panic!("not a page");
        };
        assert_eq!(page.cursor, 0);
        assert!(page.rows.len() <= 1);
        page.rows.into_iter().next()
    }

    async fn ok(&mut self, op: WriteOp) -> Reply {
        let what = op.command();
        match self.run(op).await {
            Ok(r) => r,
            Err(e) => panic!("{what}: {e}"),
        }
    }
}

fn raw(s: &str) -> Raw {
    Raw::from(s)
}

/// Whether the server has `HTTL` and `HEXPIRE`, the way the key detail decides it.
async fn has_field_ttls(c: &mut MultiplexedConnection) -> bool {
    let info: String = redis::cmd("INFO")
        .arg("server")
        .query_async(c)
        .await
        .unwrap();
    let get = |k: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}:")))
            .map(|v| v.trim().to_string())
    };
    match get("valkey_version") {
        Some(v) => field_ttls_supported(ServerFlavor::Valkey, &v),
        None => field_ttls_supported(ServerFlavor::Redis, &get("redis_version").unwrap()),
    }
}

async fn edits(server: common::TestServer, name: &str) {
    let mut a = server.admin().await;
    let mut lens = Lens::new(&server).await;

    // A string: set only while it is still the value loaded; its TTL stays.
    let _: () = a.set_ex("s", "hello", 1_000).await.unwrap();
    let r = lens
        .ok(WriteOp::SetString {
            key: "s".into(),
            old: raw("hello"),
            value: raw("hi there"),
        })
        .await;
    assert_eq!(r, Reply::Ok, "{name}");
    let v: String = a.get("s").await.unwrap();
    assert_eq!(v, "hi there", "{name}");
    let ttl: i64 = a.ttl("s").await.unwrap();
    assert!(ttl > 0, "{name}: KEEPTTL kept the TTL, got {ttl}");
    // Changed since it was loaded, longer or same length: nothing is written.
    for stale in ["hello", "hi thera"] {
        let r = lens
            .ok(WriteOp::SetString {
                key: "s".into(),
                old: raw(stale),
                value: raw("overwritten"),
            })
            .await;
        assert_eq!(r, Reply::Changed, "{name}: {stale}");
    }
    let v: String = a.get("s").await.unwrap();
    assert_eq!(v, "hi there", "{name}");
    // The connection is not left watching: a plain edit goes through after.
    let r = lens
        .ok(WriteOp::SetString {
            key: "s".into(),
            old: raw("hi there"),
            value: raw(""),
        })
        .await;
    assert_eq!(r, Reply::Ok, "{name}");
    let v: String = a.get("s").await.unwrap();
    assert_eq!(v, "", "{name}");

    // A hash: HSET edits, HSETNX never overwrites, HDEL deletes.
    let _: () = a.hset("h", "f", "1").await.unwrap();
    lens.ok(WriteOp::HSet {
        key: "h".into(),
        field: raw("f"),
        value: raw("2"),
    })
    .await;
    let r = lens
        .ok(WriteOp::HSetNx {
            key: "h".into(),
            field: raw("f"),
            value: raw("3"),
        })
        .await;
    assert_eq!(r, Reply::Int(0), "{name}");
    let v: String = a.hget("h", "f").await.unwrap();
    assert_eq!(v, "2", "{name}");
    let r = lens
        .ok(WriteOp::HSetNx {
            key: "h".into(),
            field: raw("g"),
            value: raw("3"),
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let r = lens
        .ok(WriteOp::HDel {
            key: "h".into(),
            field: raw("f"),
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let n: i64 = a.hlen("h").await.unwrap();
    assert_eq!(n, 1, "{name}");

    // A list: LSET checks the element first; LDEL removes only the index given, even
    // with equal elements elsewhere.
    let _: () = a.rpush("l", &["a", "b", "a", "c", "a"]).await.unwrap();
    lens.ok(WriteOp::LSet {
        key: "l".into(),
        index: 1,
        old: raw("b"),
        value: raw("B"),
    })
    .await;
    let r = lens
        .ok(WriteOp::LSet {
            key: "l".into(),
            index: 1,
            old: raw("b"),
            value: raw("X"),
        })
        .await;
    assert_eq!(r, Reply::Changed, "{name}");
    let r = lens
        .ok(WriteOp::LDel {
            key: "l".into(),
            index: 2,
            old: raw("a"),
            marker: list_marker(1),
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let l: Vec<String> = a.lrange("l", 0, -1).await.unwrap();
    assert_eq!(l, ["a", "B", "c", "a"], "{name}");
    // A stale index changes nothing.
    let r = lens
        .ok(WriteOp::LDel {
            key: "l".into(),
            index: 1,
            old: raw("b"),
            marker: list_marker(2),
        })
        .await;
    assert_eq!(r, Reply::Changed, "{name}");
    let r = lens
        .ok(WriteOp::RPush {
            key: "l".into(),
            value: raw("z"),
        })
        .await;
    assert_eq!(r, Reply::Int(5), "{name}");

    // A set: a member is replaced, added once, removed.
    let _: () = a.sadd("st", &["red", "blue"]).await.unwrap();
    lens.ok(WriteOp::SReplace {
        key: "st".into(),
        old: raw("red"),
        value: raw("green"),
    })
    .await;
    let m: HashSet<String> = a.smembers("st").await.unwrap();
    assert_eq!(
        m,
        HashSet::from(["green".to_string(), "blue".to_string()]),
        "{name}"
    );
    let r = lens
        .ok(WriteOp::SAdd {
            key: "st".into(),
            member: raw("blue"),
        })
        .await;
    assert_eq!(r, Reply::Int(0), "{name}");
    let r = lens
        .ok(WriteOp::SRem {
            key: "st".into(),
            member: raw("blue"),
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");

    // A sorted set: the score changes; ZADD NX leaves an existing member alone.
    let _: () = a.zadd("z", "a", 1).await.unwrap();
    lens.ok(WriteOp::ZScore {
        key: "z".into(),
        member: raw("a"),
        score: 5.5,
    })
    .await;
    let r = lens
        .ok(WriteOp::ZAdd {
            key: "z".into(),
            member: raw("a"),
            score: 9.0,
        })
        .await;
    assert_eq!(r, Reply::Int(0), "{name}");
    let s: f64 = a.zscore("z", "a").await.unwrap();
    assert_eq!(s, 5.5, "{name}");
    let r = lens
        .ok(WriteOp::ZAdd {
            key: "z".into(),
            member: raw("b"),
            score: f64::NEG_INFINITY,
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let r = lens
        .ok(WriteOp::ZRem {
            key: "z".into(),
            member: raw("b"),
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");

    // A stream: an entry added, then deleted.
    let r = lens
        .ok(WriteOp::XAdd {
            key: "x".into(),
            fields: vec![(raw("f"), raw("v"))],
        })
        .await;
    let Reply::Text(id) = r else {
        panic!("{name}: {r:?}");
    };
    let r = lens
        .ok(WriteOp::XDel {
            key: "x".into(),
            id,
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let n: i64 = redis::cmd("XLEN")
        .arg("x")
        .query_async(&mut a)
        .await
        .unwrap();
    assert_eq!(n, 0, "{name}");

    // RENAMENX never overwrites.
    let _: () = a.set("r1", "one").await.unwrap();
    let _: () = a.set("r2", "two").await.unwrap();
    let r = lens
        .ok(WriteOp::Rename {
            key: "r1".into(),
            new: "r2".into(),
        })
        .await;
    assert_eq!(r, Reply::Int(0), "{name}");
    let v: String = a.get("r2").await.unwrap();
    assert_eq!(v, "two", "{name}");
    let r = lens
        .ok(WriteOp::Rename {
            key: "r1".into(),
            new: "r3".into(),
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let v: String = a.get("r3").await.unwrap();
    assert_eq!(v, "one", "{name}");

    // N: an existing key is never overwritten, whatever the type asked for.
    for value in [
        NewKey::String(raw("new")),
        NewKey::Hash {
            field: raw("f"),
            value: raw("v"),
        },
        NewKey::ZSet {
            member: raw("m"),
            score: 1.0,
        },
    ] {
        let r = lens
            .ok(WriteOp::NewKey {
                key: "r2".into(),
                value,
            })
            .await;
        assert_eq!(r, Reply::Exists, "{name}");
    }
    let v: String = a.get("r2").await.unwrap();
    assert_eq!(v, "two", "{name}");
    let r = lens
        .ok(WriteOp::NewKey {
            key: "n1".into(),
            value: NewKey::String(raw("v")),
        })
        .await;
    assert_eq!(r, Reply::Ok, "{name}");
    let r = lens
        .ok(WriteOp::NewKey {
            key: "n2".into(),
            value: NewKey::Stream {
                field: raw("f"),
                value: raw("v"),
            },
        })
        .await;
    assert!(matches!(r, Reply::Text(_)), "{name}: {r:?}");
    let r = lens
        .ok(WriteOp::NewKey {
            key: "n3".into(),
            value: NewKey::List(raw("e")),
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let t: String = redis::cmd("TYPE")
        .arg("n2")
        .query_async(&mut a)
        .await
        .unwrap();
    assert_eq!(t, "stream", "{name}");

    // A key's TTL: set, then removed; the metadata read after a write sees both.
    let r = lens
        .ok(WriteOp::KeyTtl {
            key: "h".into(),
            ttl: Ttl::Seconds(100),
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let row = lens.row("h").await.expect("the row");
    let KeyTtl::Expires(d) = row.ttl else {
        panic!("{name}: {row:?}");
    };
    assert!(d.as_secs() > 90 && d.as_secs() <= 100, "{name}: {d:?}");
    assert_eq!(row.length, Some(1), "{name}");
    assert!(row.bytes.is_some(), "{name}");
    let r = lens
        .ok(WriteOp::KeyTtl {
            key: "h".into(),
            ttl: Ttl::Persist,
        })
        .await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let row = lens.row("h").await.expect("the row");
    assert_eq!(row.ttl, KeyTtl::Persistent, "{name}");

    // A field's TTL, where the server has HEXPIRE.
    if has_field_ttls(&mut a).await {
        let r = lens
            .ok(WriteOp::FieldTtl {
                key: "h".into(),
                field: raw("g"),
                ttl: Ttl::Seconds(100),
            })
            .await;
        assert_eq!(r, Reply::Int(1), "{name}");
        let t: Vec<i64> = redis::cmd("HTTL")
            .arg("h")
            .arg("FIELDS")
            .arg(1)
            .arg("g")
            .query_async(&mut a)
            .await
            .unwrap();
        assert!(t[0] > 90, "{name}: {t:?}");
        let r = lens
            .ok(WriteOp::FieldTtl {
                key: "h".into(),
                field: raw("g"),
                ttl: Ttl::Persist,
            })
            .await;
        assert_eq!(r, Reply::Int(1), "{name}");
        let r = lens
            .ok(WriteOp::FieldTtl {
                key: "h".into(),
                field: raw("nope"),
                ttl: Ttl::Seconds(100),
            })
            .await;
        assert_eq!(r, Reply::Int(-2), "{name}");
    } else {
        println!("{name}: no HEXPIRE (needs Redis 7.4 or Valkey 9.0), field TTLs skipped");
    }

    // D: UNLINK.
    let r = lens.ok(WriteOp::Unlink { key: "h".into() }).await;
    assert_eq!(r, Reply::Int(1), "{name}");
    let n: i64 = a.exists("h").await.unwrap();
    assert_eq!(n, 0, "{name}");
    assert_eq!(lens.row("h").await, None, "{name}");

    // A failure is in redis-lens's words: the key holds another type now.
    let e = lens
        .run(WriteOp::HSet {
            key: "s".into(),
            field: raw("f"),
            value: raw("v"),
        })
        .await
        .unwrap_err();
    assert_eq!(
        e,
        LiveError::Failed("HSET failed: the key holds another type now".into()),
        "{name}"
    );
    // A check that fails after WATCH leaves the connection not watching: a change to
    // that key afterwards does not abort the next edit of another key.
    assert!(
        lens.run(WriteOp::LSet {
            key: "s".into(),
            index: 0,
            old: raw("a"),
            value: raw("b"),
        })
        .await
        .is_err(),
        "{name}"
    );
    let _: () = a.set("s", "changed").await.unwrap();
    let _: () = a.set("s2", "x").await.unwrap();
    let r = lens
        .ok(WriteOp::SetString {
            key: "s2".into(),
            old: raw("x"),
            value: raw("y"),
        })
        .await;
    assert_eq!(r, Reply::Ok, "{name}");
}

#[tokio::test]
async fn key_edits_on_redis() {
    edits(common::redis().await, "redis").await;
}

#[tokio::test]
async fn key_edits_on_valkey() {
    edits(common::valkey().await, "valkey").await;
}
