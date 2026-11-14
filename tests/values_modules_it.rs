//! The key detail's reads of module types and arrays (`live::values`), against
//! `redis:8.10.1` (RedisTimeSeries and RedisBloom built in, arrays since 8.8) and the
//! Valkey bundle (valkey-bloom only): the `TYPE` replies they really send, what each
//! type reads through the budget, both protocols, refusals in fixed words, and the
//! graceful "not available" line where a module is missing.
//!
//! The images are pinned to the tags these tests were checked with, and started here
//! the way `tests/search_it.rs` starts them.
#![cfg(feature = "integration-tests")]

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn;
use redis_lens::live::values::{
    At, Items, Kind, Module, PAGE_ITEMS, Page, Reply, Request, TS_SAMPLES,
};
use redis_lens::live::{self, LiveError, LiveReply, LiveRequest};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile};
use redis_lens::secrets::{MemoryStore, SecretStore};
use redis_lens::value::Raw;
use secrecy::SecretString;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// Redis 8 has TimeSeries, Bloom (with Cuckoo, CMS, Top-K, t-digest) and arrays.
const REDIS8: (&str, &str, &str) = ("redis", "8.10.1", "redis-server");
/// The Valkey bundle loads valkey-bloom, valkey-json, valkey-search and valkey-ldap.
const VALKEY_BUNDLE: (&str, &str, &str) = ("valkey/valkey-bundle", "9.1.3", "valkey-server");

struct Server {
    port: u16,
    _container: ContainerAsync<GenericImage>,
}

impl Server {
    async fn start(image: (&str, &str, &str), args: &[&str]) -> Self {
        let mut cmd = vec![image.2.to_string()];
        cmd.extend(args.iter().map(|a| (*a).to_string()));
        let container = GenericImage::new(image.0, image.1)
            .with_exposed_port(6379.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
            .with_cmd(cmd)
            .start()
            .await
            .expect("start container (is Docker running?)");
        let port = container
            .get_host_port_ipv4(6379.tcp())
            .await
            .expect("mapped port");
        Self {
            port,
            _container: container,
        }
    }

    /// A RESP2 connection, as the default user: for seeding, and the RESP2 shapes.
    async fn admin(&self) -> MultiplexedConnection {
        redis::Client::open(format!("redis://127.0.0.1:{}", self.port))
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .expect("connect")
    }

    fn profile(&self, auth: Auth) -> Profile {
        Profile {
            name: "modules".to_string(),
            env: Env::Local,
            endpoint: Endpoint {
                host: "127.0.0.1".to_string(),
                port: self.port,
                tls: false,
            },
            db: 0,
            auth,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            tags: Vec::new(),
        }
    }

    /// The connection redis-lens itself makes (RESP3).
    async fn lens(&self) -> MultiplexedConnection {
        conn::connect(&self.profile(Auth::None), &MemoryStore::default())
            .await
            .expect("connect")
            .into_inner()
    }

    /// redis-lens's connection as the ACL user `user`.
    async fn lens_as(&self, user: &str, pw: &str) -> MultiplexedConnection {
        let secrets = MemoryStore::default();
        let p = self.profile(Auth::Keychain);
        secrets
            .set(
                &p.secret_ref(),
                &Credentials {
                    username: Some(user.into()),
                    password: SecretString::from(pw.to_string()),
                },
            )
            .unwrap();
        conn::connect(&p, &secrets)
            .await
            .expect("connect as an ACL user")
            .into_inner()
    }
}

async fn cmd(conn: &mut MultiplexedConnection, args: &[&str]) {
    let mut c = redis::cmd(args[0]);
    for a in &args[1..] {
        c.arg(*a);
    }
    let _: redis::Value = c
        .query_async(conn)
        .await
        .unwrap_or_else(|e| panic!("{args:?}: {e}"));
}

async fn type_of(conn: &mut MultiplexedConnection, key: &str) -> String {
    redis::cmd("TYPE").arg(key).query_async(conn).await.unwrap()
}

/// A first page as the key list asks for it: the key list sees these types as
/// "other", so `TYPE` decides what is read.
fn first(key: &str) -> Request {
    Request {
        key: key.to_string(),
        kind: Kind::Other("other".into()),
        at: None,
        field_ttls: false,
        size: Some(1_000),
    }
}

async fn run(
    c: &mut MultiplexedConnection,
    budget: &Budget,
    r: Request,
) -> Result<Reply, LiveError> {
    match live::run(c, budget, LiveRequest::Value(r)).await? {
        LiveReply::Value(v) => Ok(v),
        other => panic!("not a value reply: {other:?}"),
    }
}

fn as_page(r: Reply) -> Page {
    match r {
        Reply::Page(p) => p,
        Reply::Gone => panic!("the key is gone"),
    }
}

fn module(p: &Page) -> &Module {
    match &p.items {
        Items::Module(m) => m,
        other => panic!("not a module summary: {other:?}"),
    }
}

fn info<'a>(m: &'a Module, name: &str) -> Option<&'a Raw> {
    m.info
        .iter()
        .find(|(n, _)| n.as_bytes() == name.as_bytes())
        .map(|(_, v)| v)
}

fn text(r: &Raw) -> String {
    String::from_utf8_lossy(r.as_bytes()).into_owned()
}

/// One key of each module type, and an array of 250 elements at indexes 0, 7, 14, ….
async fn seed_redis8(c: &mut MultiplexedConnection) {
    cmd(
        c,
        &[
            "TS.CREATE",
            "ts:temp",
            "RETENTION",
            "0",
            "LABELS",
            "sensor",
            "temp",
            "room",
            "kitchen",
        ],
    )
    .await;
    cmd(c, &["TS.CREATE", "ts:avg"]).await;
    cmd(
        c,
        &[
            "TS.CREATERULE",
            "ts:temp",
            "ts:avg",
            "AGGREGATION",
            "avg",
            "60000",
        ],
    )
    .await;
    let mut pipe = redis::pipe();
    for i in 0..600u64 {
        pipe.cmd("TS.ADD")
            .arg("ts:temp")
            .arg(1_700_000_000_000 + i * 1000)
            .arg(20.0 + (i % 10) as f64 / 2.0)
            .ignore();
    }
    let () = pipe.query_async(c).await.unwrap();
    cmd(c, &["BF.ADD", "bf:seen", "a"]).await;
    cmd(c, &["CF.ADD", "cf:seen", "a"]).await;
    cmd(c, &["CMS.INITBYDIM", "cms:hits", "100", "5"]).await;
    cmd(c, &["CMS.INCRBY", "cms:hits", "a", "3"]).await;
    cmd(c, &["TOPK.RESERVE", "topk:pages", "3"]).await;
    cmd(
        c,
        &[
            "TOPK.ADD",
            "topk:pages",
            "home",
            "cart",
            "home",
            "pay",
            "home",
            "cart",
        ],
    )
    .await;
    cmd(c, &["TDIGEST.CREATE", "td:latency"]).await;
    cmd(
        c,
        &["TDIGEST.ADD", "td:latency", "1", "2", "3", "4", "5", "100"],
    )
    .await;
    let mut pipe = redis::pipe();
    for i in 0..250u64 {
        pipe.cmd("ARSET")
            .arg("arr:sparse")
            .arg(i * 7)
            .arg(format!("e{i}"))
            .ignore();
    }
    let () = pipe.query_async(c).await.unwrap();
}

#[tokio::test]
async fn type_replies_are_the_ones_kind_knows() {
    let redis = Server::start(REDIS8, &[]).await;
    let mut c = redis.admin().await;
    seed_redis8(&mut c).await;
    for (key, want, kind) in [
        ("ts:temp", "TSDB-TYPE", Kind::TimeSeries),
        ("bf:seen", "MBbloom--", Kind::Bloom),
        ("cf:seen", "MBbloomCF", Kind::Cuckoo),
        ("cms:hits", "CMSk-TYPE", Kind::Cms),
        ("topk:pages", "TopK-TYPE", Kind::TopK),
        ("td:latency", "TDIS-TYPE", Kind::TDigest),
        ("arr:sparse", "array", Kind::Array),
    ] {
        let t = type_of(&mut c, key).await;
        assert_eq!(t, want, "{key}");
        assert_eq!(Kind::from_type(&t), kind);
    }
    let valkey = Server::start(VALKEY_BUNDLE, &[]).await;
    let mut v = valkey.admin().await;
    cmd(&mut v, &["BF.ADD", "bf:seen", "a"]).await;
    let t = type_of(&mut v, "bf:seen").await;
    assert_eq!(t, "bloomfltr");
    assert_eq!(Kind::from_type(&t), Kind::Bloom);
}

#[tokio::test]
async fn redis8_module_types_read_once_through_the_budget() {
    let server = Server::start(REDIS8, &[]).await;
    let mut admin = server.admin().await;
    seed_redis8(&mut admin).await;
    // RESP3 (redis-lens's own connection) and RESP2.
    for resp3 in [true, false] {
        let mut c = if resp3 {
            server.lens().await
        } else {
            server.admin().await
        };
        let budget = Budget::new(None);

        // Time series: TS.INFO and the last 500 samples, newest first.
        let p = as_page(run(&mut c, &budget, first("ts:temp")).await.unwrap());
        assert_eq!(p.type_name.as_deref(), Some("TSDB-TYPE"));
        assert_eq!(p.next, None);
        let m = module(&p);
        assert!(m.unavailable.is_none() && m.missing.is_empty(), "{m:?}");
        assert_eq!(info(m, "totalSamples").map(text).as_deref(), Some("600"));
        assert_eq!(
            info(m, "firstTimestamp").map(text).as_deref(),
            Some("1700000000000")
        );
        assert_eq!(
            info(m, "lastTimestamp").map(text).as_deref(),
            Some("1700000599000")
        );
        assert_eq!(info(m, "retentionTime").map(text).as_deref(), Some("0"));
        let labels = text(info(m, "labels").unwrap());
        assert_eq!(labels, "sensor=temp, room=kitchen", "resp3 {resp3}");
        let rules = text(info(m, "rules").unwrap());
        assert!(
            rules.starts_with("ts:avg") && rules.contains("60000") && rules.contains("AVG"),
            "{rules}"
        );
        assert_eq!(m.samples.len(), TS_SAMPLES);
        assert_eq!(m.samples[0].ts, 1_700_000_599_000);
        assert!(m.samples.windows(2).all(|w| w[0].ts > w[1].ts));
        // TYPE, then TS.INFO and TS.REVRANGE in one pipeline.
        assert_eq!(budget.taken(), 3);

        let p = as_page(run(&mut c, &budget, first("bf:seen")).await.unwrap());
        let m = module(&p);
        assert_eq!(info(m, "Capacity").map(text).as_deref(), Some("100"));
        assert_eq!(
            info(m, "Number of items inserted").map(text).as_deref(),
            Some("1")
        );
        let p = as_page(run(&mut c, &budget, first("cf:seen")).await.unwrap());
        assert!(info(module(&p), "Number of buckets").is_some());
        let p = as_page(run(&mut c, &budget, first("cms:hits")).await.unwrap());
        let m = module(&p);
        assert_eq!(info(m, "width").map(text).as_deref(), Some("100"));
        assert_eq!(info(m, "count").map(text).as_deref(), Some("3"));
        assert_eq!(budget.taken(), 3 + 2 * 3);

        let p = as_page(run(&mut c, &budget, first("topk:pages")).await.unwrap());
        let m = module(&p);
        assert_eq!(info(m, "k").map(text).as_deref(), Some("3"));
        assert_eq!(
            m.top.first().map(|(i, n)| (text(i), text(n))),
            Some(("home".into(), "3".into()))
        );
        assert_eq!(m.top.len(), 3);

        let p = as_page(run(&mut c, &budget, first("td:latency")).await.unwrap());
        let m = module(&p);
        assert_eq!(info(m, "Observations").map(text).as_deref(), Some("6"));
        let stats: Vec<(&str, String)> = m.stats.iter().map(|(l, v)| (*l, text(v))).collect();
        assert_eq!(stats.len(), 5, "{stats:?}");
        assert_eq!(stats[0].0, "p50");
        assert_eq!(stats[3], ("min", "1".to_string()));
        assert_eq!(stats[4], ("max", "100".to_string()));
        assert!(m.missing.is_empty(), "{:?}", m.missing);
        // TOPK: TYPE + 2; t-digest: TYPE + 4.
        assert_eq!(budget.taken(), 3 + 2 * 3 + 3 + 5);
    }
}

#[tokio::test]
async fn an_array_reads_in_arscan_pages_and_every_element_once() {
    let server = Server::start(REDIS8, &[]).await;
    let mut admin = server.admin().await;
    seed_redis8(&mut admin).await;
    let mut c = server.lens().await;
    let budget = Budget::new(None);
    let mut seen = Vec::new();
    let mut req = first("arr:sparse");
    let mut pages = 0;
    loop {
        let p = as_page(run(&mut c, &budget, req.clone()).await.unwrap());
        pages += 1;
        if pages == 1 {
            assert_eq!(p.type_name.as_deref(), Some("array"));
        }
        let Items::Array(v) = &p.items else {
            panic!("not an array: {:?}", p.items);
        };
        assert!(v.len() <= PAGE_ITEMS);
        seen.extend(v.iter().map(|e| (e.index, text(&e.value))));
        match p.next {
            Some(at @ At::Index(_)) => {
                req = Request {
                    kind: Kind::Array,
                    at: Some(at),
                    ..first("arr:sparse")
                };
            }
            None => break,
            other => panic!("{other:?}"),
        }
        assert!(pages < 10, "the walk never ends");
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 250);
    for (k, (i, v)) in seen.iter().enumerate() {
        assert_eq!((*i, v.as_str()), (k as u64 * 7, format!("e{k}").as_str()));
    }
}

#[tokio::test]
async fn refusals_are_fixed_words_and_a_minimal_reader_reads_them_all() {
    let args = [
        // Everything but TS.INFO; then TS.REVRANGE and TDIGEST.MAX refused.
        "--user",
        "noinfo",
        "on",
        ">pw",
        "~*",
        "+@all",
        "-ts.info",
        "--user",
        "norange",
        "on",
        ">pw",
        "~*",
        "+@all",
        "-ts.revrange",
        "-tdigest.max",
        // The minimal read-only user of spec "Permission check", with +@read -keys.
        "--user",
        "ro",
        "on",
        ">pw",
        "~*",
        "resetchannels",
        "-@all",
        "+@read",
        "-keys",
        "+ping",
        "+hello",
        "+auth",
        "+client|setinfo",
        "+client|setname",
        "+select",
        "+info",
    ];
    let server = Server::start(REDIS8, &args).await;
    let mut admin = server.admin().await;
    seed_redis8(&mut admin).await;
    let budget = Budget::new(None);

    let mut c = server.lens_as("noinfo", "pw").await;
    let p = as_page(run(&mut c, &budget, first("ts:temp")).await.unwrap());
    let m = module(&p);
    assert_eq!(
        m.unavailable.as_deref(),
        Some("Time series not read: the server refused TS.INFO for this user.")
    );
    assert!(m.info.is_empty() && m.samples.is_empty());
    assert!(!format!("{p:?}").contains("noinfo"));

    let mut c = server.lens_as("norange", "pw").await;
    let p = as_page(run(&mut c, &budget, first("ts:temp")).await.unwrap());
    let m = module(&p);
    assert!(m.unavailable.is_none());
    assert!(info(m, "totalSamples").is_some());
    assert_eq!(
        m.missing,
        ["Samples not read: the server refused TS.REVRANGE for this user."]
    );
    let p = as_page(run(&mut c, &budget, first("td:latency")).await.unwrap());
    assert_eq!(
        module(&p).missing,
        ["Maximum not read: the server refused TDIGEST.MAX for this user."]
    );

    // Every command here is in @read: the minimal user reads every type.
    let mut c = server.lens_as("ro", "pw").await;
    for key in [
        "ts:temp",
        "bf:seen",
        "cf:seen",
        "cms:hits",
        "topk:pages",
        "td:latency",
    ] {
        let p = as_page(run(&mut c, &budget, first(key)).await.unwrap());
        let m = module(&p);
        assert!(
            m.unavailable.is_none() && m.missing.is_empty(),
            "{key}: {m:?}"
        );
    }
    let p = as_page(run(&mut c, &budget, first("arr:sparse")).await.unwrap());
    assert_eq!(p.items.len(), PAGE_ITEMS);
}

#[tokio::test]
async fn valkey_reads_bloom_and_says_the_other_modules_are_not_available() {
    let server = Server::start(VALKEY_BUNDLE, &[]).await;
    let mut admin = server.admin().await;
    cmd(&mut admin, &["BF.ADD", "bf:seen", "a"]).await;
    cmd(&mut admin, &["SET", "plain", "v"]).await;
    let mut c = server.lens().await;
    let budget = Budget::new(None);
    let p = as_page(run(&mut c, &budget, first("bf:seen")).await.unwrap());
    assert_eq!(p.type_name.as_deref(), Some("bloomfltr"));
    let m = module(&p);
    assert!(m.unavailable.is_none(), "{m:?}");
    assert_eq!(info(m, "Capacity").map(text).as_deref(), Some("100"));
    assert_eq!(
        info(m, "Number of items inserted").map(text).as_deref(),
        Some("1")
    );
    assert_eq!(budget.taken(), 2);

    // The other module commands are unknown here. A key of those types can't exist on
    // this server, so each read is sent straight, as a next page is (no TYPE first):
    // the panel says why in one line, and nothing fails.
    for (kind, want) in [
        (
            Kind::TimeSeries,
            "Time series not read: the server does not support TS.INFO.",
        ),
        (
            Kind::Cuckoo,
            "Cuckoo filter not read: the server does not support CF.INFO.",
        ),
        (
            Kind::Cms,
            "Count-Min Sketch not read: the server does not support CMS.INFO.",
        ),
        (
            Kind::TopK,
            "Top-K not read: the server does not support TOPK.INFO.",
        ),
        (
            Kind::TDigest,
            "t-digest not read: the server does not support TDIGEST.INFO.",
        ),
    ] {
        let r = Request {
            kind: kind.clone(),
            at: Some(At::Index(0)),
            ..first("plain")
        };
        let p = as_page(run(&mut c, &budget, r).await.unwrap());
        assert_eq!(module(&p).unavailable.as_deref(), Some(want), "{kind:?}");
    }
    // An array read where there are no arrays: the read is refused like any other
    // collection's, in fixed words.
    let r = Request {
        kind: Kind::Array,
        at: Some(At::Index(0)),
        ..first("plain")
    };
    assert_eq!(
        run(&mut c, &budget, r).await.unwrap_err(),
        LiveError::Unsupported("ARSCAN".into())
    );
}
