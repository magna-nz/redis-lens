//! Find key (`ctrl-f`, `live::keys::Request::Find`) against real Redis and Valkey
//! servers, standalone and as a cluster: plain text finds the names that contain it, a
//! glob is used as typed, every key `SCAN` returns is kept, pages of 1,000, the key
//! metadata pipelined and never a value, and a refused `SCAN` said in fixed words.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use common::cluster::{TestCluster, cluster, parse_commandstats};
use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn;
use redis_lens::conn::cluster::{ClusterTargets, ScanTargets, connect_targets};
use redis_lens::live::keys::{self, PAGE_KEYS, Page, Reply, Request, find_glob};
use redis_lens::live::nodes::{Node, NodeInfo, Table};
use redis_lens::live::{self, Dest, Follow, LiveError, LiveHandle, LiveReply, LiveRequest, ReqId};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, KeyType, Profile, Ttl};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(15);

fn profile(port: u16, auth: Auth) -> Profile {
    Profile {
        name: "find".to_string(),
        // A prod profile: read-only by default. A find sends only reads.
        env: Env::Prod,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
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
        force_standalone: false,
        tags: Vec::new(),
    }
}

async fn connect(server: &common::TestServer) -> MultiplexedConnection {
    conn::connect(&profile(server.port, Auth::None), &MemoryStore::default())
        .await
        .expect("connect")
        .into_inner()
}

/// Connects as the ACL user `lens` with password `lens-pw`.
async fn connect_as_lens(server: &common::TestServer) -> MultiplexedConnection {
    let secrets = MemoryStore::default();
    let p = profile(server.port, Auth::Keychain);
    secrets
        .set(
            &p.secret_ref(),
            &Credentials {
                username: Some("lens".into()),
                password: SecretString::from("lens-pw"),
            },
        )
        .unwrap();
    conn::connect(&p, &secrets)
        .await
        .expect("connect as lens")
        .into_inner()
}

async fn server(valkey: bool) -> common::TestServer {
    if valkey {
        common::valkey().await
    } else {
        common::redis().await
    }
}

/// One page of a find for `text`, as `enter` and `m` ask for it.
async fn find_page(
    c: &mut MultiplexedConnection,
    text: &str,
    cursor: u64,
    carry: Vec<String>,
) -> Result<Page, LiveError> {
    let req = Request::Find {
        glob: find_glob(text),
        cursor,
        carry,
        sizes: true,
        freq: false,
    };
    match keys::run(c, &Budget::new(None), req).await? {
        Reply::Page(p) => Ok(p),
        other => panic!("not a page: {other:?}"),
    }
}

/// Every page of a find, as `m` loads them.
async fn find_all(c: &mut MultiplexedConnection, text: &str) -> Vec<Page> {
    let mut out = Vec::new();
    let (mut cursor, mut carry) = (0, Vec::new());
    loop {
        let p = find_page(c, text, cursor, carry).await.expect("page");
        cursor = p.cursor;
        carry = p.carry.clone();
        let done = cursor == 0 && carry.is_empty();
        out.push(p);
        if done {
            return out;
        }
        assert!(out.len() < 100, "the walk never ends");
    }
}

/// The names every page of a find for `text` listed.
async fn found(c: &mut MultiplexedConnection, text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for p in find_all(c, text).await {
        assert_eq!(p.other_pattern, 0, "a find leaves no key out: {p:?}");
        for r in p.rows {
            assert!(out.insert(r.key.clone()), "{} listed twice", r.key);
        }
    }
    out
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_string()).collect()
}

async fn seed(c: &mut MultiplexedConnection, keys: &[&str]) {
    let mut pipe = redis::pipe();
    for k in keys {
        pipe.set(*k, "v").ignore();
    }
    let () = pipe.query_async(c).await.unwrap();
}

const NAMES: [&str; 9] = [
    "user:1:cart",
    "user:2:cart",
    "user:abc:cart",
    "user:1:cart:old",
    "cart:archive",
    "promo:cart-banner",
    "user:1:profile",
    "cat",
    "CART:upper",
];

#[tokio::test]
async fn plain_text_finds_every_name_that_contains_it_and_nothing_else() {
    for valkey in [false, true] {
        let s = server(valkey).await;
        let mut admin = s.admin().await;
        seed(&mut admin, &NAMES).await;
        let mut c = connect(&s).await;
        assert_eq!(
            found(&mut c, "cart").await,
            set(&[
                "user:1:cart",
                "user:2:cart",
                "user:abc:cart",
                "user:1:cart:old",
                "cart:archive",
                "promo:cart-banner",
            ]),
            "valkey {valkey}: case matters, and keys of every pattern are kept"
        );
        assert_eq!(
            found(&mut c, "user:1").await,
            set(&["user:1:cart", "user:1:cart:old", "user:1:profile"]),
            "valkey {valkey}"
        );
        // `]` and `^` are only special inside brackets.
        seed(&mut admin, &["odd]^:1"]).await;
        assert_eq!(
            found(&mut c, "]^").await,
            set(&["odd]^:1"]),
            "valkey {valkey}"
        );
    }
}

#[tokio::test]
async fn a_glob_is_used_as_typed() {
    for valkey in [false, true] {
        let s = server(valkey).await;
        let mut admin = s.admin().await;
        seed(&mut admin, &NAMES).await;
        let mut c = connect(&s).await;
        assert_eq!(
            found(&mut c, "user:*:cart").await,
            set(&["user:1:cart", "user:2:cart", "user:abc:cart"]),
            "valkey {valkey}: a glob matches from the start, and other patterns' keys stay"
        );
        assert_eq!(
            found(&mut c, "user:?:cart").await,
            set(&["user:1:cart", "user:2:cart"]),
            "valkey {valkey}"
        );
        assert_eq!(
            found(&mut c, "[cC][aA]*").await,
            set(&["cart:archive", "cat", "CART:upper"]),
            "valkey {valkey}"
        );
    }
}

#[tokio::test]
async fn no_match_is_an_empty_page_with_the_walk_done() {
    for valkey in [false, true] {
        let s = server(valkey).await;
        let mut admin = s.admin().await;
        seed(&mut admin, &NAMES).await;
        let mut c = connect(&s).await;
        let p = find_page(&mut c, "nope", 0, Vec::new()).await.unwrap();
        assert!(p.rows.is_empty(), "valkey {valkey}: {p:?}");
        assert_eq!((p.cursor, p.carry.len()), (0, 0), "valkey {valkey}");
        assert_eq!((p.other_pattern, p.unreadable), (0, 0), "valkey {valkey}");
    }
}

#[tokio::test]
async fn a_backslash_in_the_text_is_matched_as_itself() {
    for valkey in [false, true] {
        let s = server(valkey).await;
        let mut admin = s.admin().await;
        seed(
            &mut admin,
            &["dir\\file:1", "dirfile:1", "dir\\\\file", "x*y", "xay"],
        )
        .await;
        let mut c = connect(&s).await;
        // Plain text: the backslash is doubled in the glob, so it matches only itself.
        assert_eq!(
            found(&mut c, "dir\\file").await,
            set(&["dir\\file:1"]),
            "valkey {valkey}"
        );
        assert_eq!(
            found(&mut c, "\\\\").await,
            set(&["dir\\\\file"]),
            "valkey {valkey}"
        );
        // A glob the user wrote: `\*` is a literal star.
        assert_eq!(
            found(&mut c, "x\\*y").await,
            set(&["x*y"]),
            "valkey {valkey}"
        );
        assert_eq!(
            found(&mut c, "x*y").await,
            set(&["x*y", "xay"]),
            "valkey {valkey}"
        );
    }
}

#[tokio::test]
async fn m_loads_a_thousand_matching_keys_at_a_time_until_every_one_is_listed() {
    const KEYS: usize = 2_500;
    for valkey in [false, true] {
        let s = server(valkey).await;
        let mut admin = s.admin().await;
        let mut pipe = redis::pipe();
        for id in 0..KEYS {
            pipe.set(format!("order:{id}:line"), "v").ignore();
        }
        for id in 0..300 {
            pipe.set(format!("other:{id}"), "v").ignore();
        }
        let () = pipe.query_async(&mut admin).await.unwrap();
        let mut c = connect(&s).await;
        let pages = find_all(&mut c, ":line").await;
        assert!(pages.len() >= 3, "valkey {valkey}: {} pages", pages.len());
        assert_eq!(pages[0].rows.len(), PAGE_KEYS, "valkey {valkey}");
        let mut listed = BTreeSet::new();
        for p in &pages {
            assert!(p.rows.len() <= PAGE_KEYS, "valkey {valkey}");
            for r in &p.rows {
                assert!(r.key.ends_with(":line"), "{}", r.key);
                listed.insert(r.key.clone());
            }
        }
        assert_eq!(listed.len(), KEYS, "valkey {valkey}");
    }
}

async fn reset_stats(c: &mut MultiplexedConnection) {
    redis::cmd("CONFIG")
        .arg("RESETSTAT")
        .query_async::<()>(c)
        .await
        .unwrap();
}

async fn commandstats(c: &mut MultiplexedConnection) -> HashMap<String, (u64, u64)> {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(c)
        .await
        .unwrap();
    parse_commandstats(&info)
}

/// What a find may send: `SCAN`, then the metadata pipeline. `config|resetstat` and
/// `info` are this test's own.
const METADATA: [&str; 13] = [
    "scan",
    "type",
    "pttl",
    "memory|usage",
    "object|encoding",
    "object|freq",
    "strlen",
    "hlen",
    "llen",
    "scard",
    "zcard",
    "xlen",
    "config|resetstat",
];

#[tokio::test]
async fn each_row_has_its_metadata_and_no_value_is_read() {
    for valkey in [false, true] {
        let s = common::server_with(valkey, &["--maxmemory-policy", "allkeys-lfu"]).await;
        let mut admin = s.admin().await;
        let () = redis::pipe()
            .set_ex("f:s", "hello", 3600)
            .ignore()
            .hset_multiple("f:h", &[("a", 1), ("b", 2), ("c", 3)])
            .ignore()
            .rpush("f:l", &[1, 2, 3, 4])
            .ignore()
            .sadd("f:t", &[1, 2])
            .ignore()
            .zadd_multiple("f:z", &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")])
            .ignore()
            .cmd("XADD")
            .arg("f:x")
            .arg("*")
            .arg("f")
            .arg("v")
            .ignore()
            .query_async(&mut admin)
            .await
            .unwrap();
        let mut c = connect(&s).await;
        reset_stats(&mut admin).await;
        let req = Request::Find {
            glob: find_glob("f:"),
            cursor: 0,
            carry: Vec::new(),
            sizes: true,
            freq: true,
        };
        let Reply::Page(p) = keys::run(&mut c, &Budget::new(None), req).await.unwrap() else {
            panic!("not a page");
        };
        let stats = commandstats(&mut admin).await;
        assert!(p.refused.is_empty(), "valkey {valkey}: {:?}", p.refused);
        assert_eq!(p.rows.len(), 6, "valkey {valkey}: {p:?}");
        for (key, key_type, length) in [
            ("f:s", KeyType::String, 5),
            ("f:h", KeyType::Hash, 3),
            ("f:l", KeyType::List, 4),
            ("f:t", KeyType::Set, 2),
            ("f:z", KeyType::ZSet, 5),
            ("f:x", KeyType::Stream, 1),
        ] {
            let r = p
                .rows
                .iter()
                .find(|r| r.key == key)
                .unwrap_or_else(|| panic!("{key} not listed"));
            assert_eq!(r.key_type, key_type, "valkey {valkey}: {key}");
            assert_eq!(r.length, Some(length), "valkey {valkey}: {key}");
            assert!(r.bytes.is_some_and(|b| b > 0), "{key}: {r:?}");
            assert!(r.encoding.is_some(), "{key}: {r:?}");
            assert!(r.freq.is_some(), "LFU policy: {key}: {r:?}");
            match (key, r.ttl) {
                ("f:s", Ttl::Expires(d)) => assert!(d > Duration::from_secs(3500)),
                ("f:s", other) => panic!("f:s expires: {other:?}"),
                (_, t) => assert_eq!(t, Ttl::Persistent, "{key}"),
            }
        }
        // Only SCAN and the metadata: never KEYS, and no command that reads a value.
        assert!(
            stats.get("scan").is_some_and(|s| s.0 >= 1),
            "valkey {valkey}: {stats:?}"
        );
        // Before Redis 7.0, commandstats counts a container command under its own name
        // (`object`, `memory`, `config`), not per subcommand: such a name passes when one
        // of its allowed subcommands is listed.
        for name in stats.keys() {
            let allowed = METADATA.contains(&name.as_str())
                || name == "info"
                || METADATA.iter().any(|m| {
                    m.strip_prefix(name.as_str())
                        .is_some_and(|r| r.starts_with('|'))
                });
            assert!(allowed, "valkey {valkey}: a find sent {name}: {stats:?}");
        }
    }
}

/// `lens` may do everything but `rules`, on every key.
fn lens_without(rules: &[&'static str]) -> Vec<&'static str> {
    let mut args = vec!["--user", "lens", "on", ">lens-pw", "~*", "+@all"];
    args.extend_from_slice(rules);
    args
}

#[tokio::test]
async fn a_refused_scan_is_said_in_fixed_words() {
    for valkey in [false, true] {
        let s = common::server_with(valkey, &lens_without(&["-scan"])).await;
        let mut admin = s.admin().await;
        seed(&mut admin, &["cart:1"]).await;
        let mut c = connect_as_lens(&s).await;
        let e = find_page(&mut c, "cart", 0, Vec::new()).await.unwrap_err();
        assert_eq!(e, LiveError::Denied("SCAN".into()), "valkey {valkey}");
        let text = format!("{e} {e:?}");
        for leak in ["NOPERM", "lens", "lens-pw"] {
            assert!(!text.contains(leak), "valkey {valkey}: {leak} in {text}");
        }
    }
}

// -- On a cluster ----------------------------------------------------------------

async fn targets(c: &TestCluster) -> ClusterTargets {
    match connect_targets(&profile(c.seed_port(), Auth::None), &MemoryStore::default()).await {
        Ok(ScanTargets::Cluster(t)) => t,
        Ok(ScanTargets::Single { .. }) => panic!("a cluster node was taken for a standalone"),
        Err(e) => panic!("connect failed: {e}"),
    }
}

type Answer = Result<LiveReply, LiveError>;

/// The live task over every shard, each node with its own budget, as the TUI starts it.
fn start(t: &ClusterTargets) -> (LiveHandle, mpsc::UnboundedReceiver<(ReqId, Answer)>) {
    let nodes = t
        .nodes
        .iter()
        .map(|n| Node {
            info: NodeInfo {
                id: n.id.clone(),
                endpoint: n.endpoint.clone(),
                role: n.role,
                slots: n.slots.clone(),
                connected: n.conn.is_some(),
            },
            conn: n.conn.clone().map(|c| c.into_inner()),
            budget: n.budget.clone().unwrap_or_else(|| Budget::new(None)),
        })
        .collect();
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = live::spawn_nodes(
        Table::cluster(nodes),
        move |id, r| {
            let _ = tx.send((id, r));
        },
        |_| {},
    );
    (handle, rx)
}

/// The index of the shard that owns `key`'s slot.
fn owner(t: &ClusterTargets, key: &str) -> usize {
    let slot = redis_lens::conn::slot::key_slot(key.as_bytes());
    t.nodes
        .iter()
        .position(|n| n.slots.iter().any(|&(a, b)| a <= slot && slot <= b))
        .expect("an owner")
}

/// A find walks each primary with its own `SCAN`, redirects never followed: each node
/// lists its own matching keys, and together they are every matching key.
#[tokio::test]
async fn on_a_cluster_each_primary_lists_its_own_matching_keys() {
    for valkey in [false, true] {
        let c = cluster(valkey, false).await;
        let mut keys: Vec<(String, String)> = (0..200)
            .map(|i| (format!("user:{i}:cart"), "x".to_string()))
            .collect();
        keys.extend((0..100).map(|i| (format!("other:{i}"), "x".to_string())));
        c.seed(&keys).await;
        let t = targets(&c).await;
        let (h, mut rx) = start(&t);
        let mut next = 0;
        let mut all = BTreeSet::new();
        for i in 0..t.nodes.len() {
            let (mut cursor, mut carry) = (0, Vec::new());
            loop {
                next += 1;
                let req = LiveRequest::Keys(Request::Find {
                    glob: find_glob(":cart"),
                    cursor,
                    carry,
                    sizes: false,
                    freq: false,
                });
                assert!(h.send_to(ReqId(next), Dest::node(i).follow(Follow::Never), req));
                let (got, answer) = tokio::time::timeout(WAIT, rx.recv())
                    .await
                    .expect("an answer in time")
                    .expect("the task is alive");
                assert_eq!(got, ReqId(next));
                let Ok(LiveReply::Keys(Reply::Page(p))) = answer else {
                    panic!("valkey={valkey}: node {i}: {answer:?}");
                };
                for r in p.rows {
                    assert!(r.key.ends_with(":cart"), "{}", r.key);
                    assert_eq!(owner(&t, &r.key), i, "valkey={valkey}: {}", r.key);
                    assert!(all.insert(r.key));
                }
                (cursor, carry) = (p.cursor, p.carry);
                if cursor == 0 && carry.is_empty() {
                    break;
                }
            }
        }
        assert_eq!(all.len(), 200, "valkey={valkey}");
    }
}
