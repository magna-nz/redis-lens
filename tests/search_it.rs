//! The Search view's live requests against Redis 8 (Search built in), valkey-search from
//! the Valkey bundle, and servers without the module.
//!
//! The images are pinned to the tags these tests were checked with. Neither is in
//! `tests/common`, so they are started here, the same way.
#![cfg(feature = "integration-tests")]

mod common;

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::live::LiveError;
use redis_lens::live::search::{self, IndexInfo, IndexList, Reply, Request};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::time::{Instant, sleep};

/// Redis 8 has Redis Search built in.
const REDIS8: (&str, &str, &str) = ("redis", "8.10.1", "redis-server");
/// The Valkey bundle loads valkey-search, valkey-json, valkey-bloom and valkey-ldap.
const VALKEY_BUNDLE: (&str, &str, &str) = ("valkey/valkey-bundle", "9.1.3", "valkey-server");

struct Server {
    port: u16,
    valkey: bool,
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
            valkey: image.2 == "valkey-server",
            _container: container,
        }
    }

    async fn conn(&self) -> MultiplexedConnection {
        redis::Client::open(format!("redis://127.0.0.1:{}", self.port))
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .expect("connect")
    }

    async fn conn_as(&self, user: &str, password: &str) -> MultiplexedConnection {
        redis::Client::open(format!("redis://{user}:{password}@127.0.0.1:{}", self.port))
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .expect("connect")
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

async fn indexes(conn: &mut MultiplexedConnection) -> Result<IndexList, LiveError> {
    match search::run(conn, &Budget::new(None), Request::Indexes).await? {
        Reply::Indexes(l) => Ok(l),
    }
}

fn index<'a>(list: &'a IndexList, name: &str) -> &'a IndexInfo {
    list.indexes
        .iter()
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("no index {name} in {:?}", list.indexes))
}

/// Three products and two sessions, with an index on each.
async fn seed(conn: &mut MultiplexedConnection) {
    for i in 1..=3 {
        cmd(
            conn,
            &[
                "HSET",
                &format!("product:{i}"),
                "title",
                "red shoe",
                "price",
                &(i * 10).to_string(),
                "tags",
                "a,b",
            ],
        )
        .await;
    }
    for i in 1..=2 {
        cmd(
            conn,
            &[
                "JSON.SET",
                &format!("session:{i}"),
                "$",
                &format!("{{\"user\":{{\"id\":\"u{i}\"}}}}"),
            ],
        )
        .await;
    }
    cmd(
        conn,
        &[
            "FT.CREATE",
            "idx:products",
            "ON",
            "HASH",
            "PREFIX",
            "1",
            "product:",
            "SCHEMA",
            "title",
            "TEXT",
            "price",
            "NUMERIC",
            "tags",
            "TAG",
        ],
    )
    .await;
    cmd(
        conn,
        &[
            "FT.CREATE",
            "idx:sessions",
            "ON",
            "JSON",
            "PREFIX",
            "1",
            "session:",
            "SCHEMA",
            "$.user.id",
            "AS",
            "user",
            "TAG",
        ],
    )
    .await;
}

/// Asks until every index has finished building and holds `docs` documents.
async fn settled(conn: &mut MultiplexedConnection, docs: &[(&str, u64)]) -> IndexList {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let l = indexes(conn).await.expect("indexes");
        let done = docs.iter().all(|(n, d)| {
            l.indexes
                .iter()
                .find(|i| i.name == *n)
                .is_some_and(|i| i.docs == Some(*d) && i.indexing == Some(false))
        });
        if done || Instant::now() > end {
            return l;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn indexes_on(server: Server) {
    let mut conn = server.conn().await;
    seed(&mut conn).await;
    let list = settled(&mut conn, &[("idx:products", 3), ("idx:sessions", 2)]).await;
    assert_eq!(list.total, 2);
    assert!(list.notes.is_empty(), "{:?}", list.notes);
    // By name.
    let names: Vec<_> = list.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["idx:products", "idx:sessions"]);

    let products = index(&list, "idx:products");
    assert_eq!(products.on.as_deref(), Some("HASH"));
    assert_eq!(products.prefixes, ["product:"]);
    assert_eq!(products.docs, Some(3));
    assert_eq!(products.indexing, Some(false));
    assert_eq!(products.progress, Some(1.0));
    assert_eq!(products.failures, Some(0));
    // Redis Search lists the fields in the order of `FT.CREATE`, valkey-search by name.
    let mut schema: Vec<_> = products
        .schema
        .iter()
        .map(|f| (f.name.as_str(), f.identifier.as_str(), f.ty.as_str()))
        .collect();
    schema.sort_unstable();
    assert_eq!(
        schema,
        [
            ("price", "price", "NUMERIC"),
            ("tags", "tags", "TAG"),
            ("title", "title", "TEXT")
        ]
    );

    let sessions = index(&list, "idx:sessions");
    assert_eq!(sessions.on.as_deref(), Some("JSON"));
    assert_eq!(sessions.prefixes, ["session:"]);
    assert_eq!(sessions.docs, Some(2));
    let schema: Vec<_> = sessions
        .schema
        .iter()
        .map(|f| (f.name.as_str(), f.identifier.as_str(), f.ty.as_str()))
        .collect();
    assert_eq!(schema, [("user", "$.user.id", "TAG")]);

    // Redis Search reports the memory of an index; valkey-search does not, and the view
    // leaves it out with a reason instead of showing an error.
    if server.valkey {
        assert_eq!(products.bytes, None);
    } else {
        assert!(
            products.bytes.is_some_and(|b| b > 0),
            "{:?}",
            products.bytes
        );
    }

    // A document the module cannot index counts as a failure.
    cmd(&mut conn, &["HSET", "product:bad", "price", "not-a-number"]).await;
    let end = Instant::now() + Duration::from_secs(10);
    let failures = loop {
        let l = indexes(&mut conn).await.unwrap();
        let f = index(&l, "idx:products").failures;
        if f != Some(0) || Instant::now() > end {
            break f;
        }
        sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(failures, Some(1));
}

#[tokio::test]
async fn redis_8_indexes() {
    indexes_on(Server::start(REDIS8, &[]).await).await;
}

#[tokio::test]
async fn valkey_search_indexes() {
    indexes_on(Server::start(VALKEY_BUNDLE, &[]).await).await;
}

#[tokio::test]
async fn a_module_with_no_index_gives_an_empty_list() {
    for image in [REDIS8, VALKEY_BUNDLE] {
        let server = Server::start(image, &[]).await;
        let mut conn = server.conn().await;
        let l = indexes(&mut conn).await.expect("indexes");
        assert_eq!(l, IndexList::default());
    }
}

#[tokio::test]
async fn a_server_without_the_module_says_unsupported() {
    for server in [common::redis().await, common::valkey().await] {
        let mut conn = server.admin().await;
        let err = indexes(&mut conn).await.expect_err("no module");
        assert_eq!(err, LiveError::Unsupported("FT._LIST".into()));
        assert_eq!(err.to_string(), "the server does not support FT._LIST");
    }
}

#[tokio::test]
async fn every_command_takes_from_the_budget() {
    let server = Server::start(REDIS8, &[]).await;
    let mut conn = server.conn().await;
    seed(&mut conn).await;
    let budget = Budget::new(None);
    search::run(&mut conn, &budget, Request::Indexes)
        .await
        .expect("run");
    // FT._LIST, then FT.INFO for each of the two indexes.
    assert_eq!(budget.taken(), 3);
}

/// A user who may list indexes and not read them still sees the names, with a fixed note.
#[tokio::test]
async fn indexes_the_user_may_not_read_keep_their_names() {
    let args = ["--user", "zed-ro", "on", ">zed-pw", "~*", "+ft._list"];
    for image in [REDIS8, VALKEY_BUNDLE] {
        let server = Server::start(image, &args).await;
        let mut admin = server.conn().await;
        seed(&mut admin).await;
        let mut zed = server.conn_as("zed-ro", "zed-pw").await;
        let l = indexes(&mut zed).await.expect("indexes");
        let names: Vec<_> = l.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["idx:products", "idx:sessions"]);
        assert!(
            l.indexes
                .iter()
                .all(|i| i.docs.is_none() && i.schema.is_empty())
        );
        assert_eq!(l.notes, ["the server refused FT.INFO for this user"]);
        let shown = format!("{l:?}");
        assert!(
            !shown.contains("zed-ro") && !shown.contains("NOPERM"),
            "{shown}"
        );
    }
}

#[tokio::test]
async fn a_user_who_may_not_list_indexes_turns_the_view_off() {
    let args = ["--user", "zed-ro", "on", ">zed-pw", "~*", "+ping"];
    let server = Server::start(REDIS8, &args).await;
    let mut zed = server.conn_as("zed-ro", "zed-pw").await;
    let err = indexes(&mut zed).await.expect_err("denied");
    assert_eq!(err, LiveError::Denied("FT._LIST".into()));
    assert!(!err.to_string().contains("zed-ro"));
}
