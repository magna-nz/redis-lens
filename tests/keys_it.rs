//! The key list and the Big keys lengths (`live::keys`), against real Redis and Valkey
//! servers.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashSet;
use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn;
use redis_lens::live::LiveError;
use redis_lens::live::keys::{self, FactKey, KeyRow, PAGE_KEYS, Page, Reply, Request};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, KeyType, Profile, Ttl};
use redis_lens::pattern::pattern_for_key;
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;

fn profile(port: u16, auth: Auth) -> Profile {
    Profile {
        name: "keys".to_string(),
        env: Env::Local,
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

/// `lens` may do everything but `rules` (say `-memory`), on every key.
fn lens_without(rules: &[&'static str]) -> Vec<&'static str> {
    let mut args = vec!["--user", "lens", "on", ">lens-pw", "~*", "+@all"];
    args.extend_from_slice(rules);
    args
}

async fn page(
    c: &mut MultiplexedConnection,
    pattern: &str,
    cursor: u64,
    carry: Vec<String>,
    sizes: bool,
    freq: bool,
) -> Result<Page, LiveError> {
    let req = Request::Page {
        pattern: pattern.to_string(),
        cursor,
        carry,
        sizes,
        freq,
    };
    match keys::run(c, &Budget::new(None), req).await? {
        Reply::Page(p) => Ok(p),
        other => panic!("not a page: {other:?}"),
    }
}

/// Every page of `pattern`, as `n` loads them.
async fn all_pages(c: &mut MultiplexedConnection, pattern: &str) -> Vec<Page> {
    let mut out = Vec::new();
    let (mut cursor, mut carry) = (0, Vec::new());
    loop {
        let p = page(c, pattern, cursor, carry, true, false)
            .await
            .expect("page");
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

fn row<'a>(p: &'a Page, key: &str) -> &'a KeyRow {
    p.rows
        .iter()
        .find(|r| r.key == key)
        .unwrap_or_else(|| panic!("{key} not listed: {p:?}"))
}

#[tokio::test]
async fn the_glob_also_matches_another_patterns_keys_which_are_left_out() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let mut pipe = redis::pipe();
        for id in 1..=30 {
            pipe.hset(format!("user:{id}:cart"), "sku", 1).ignore();
        }
        pipe.hset("user:abc:cart", "sku", 1).ignore();
        pipe.set("user:7:cart:old", "x").ignore();
        let () = pipe.query_async(&mut admin).await.unwrap();

        let ids = pattern_for_key("user:1:cart");
        assert_ne!(ids, pattern_for_key("user:abc:cart"), "valkey {valkey}");
        let mut c = connect(&server).await;
        let p = page(&mut c, &ids, 0, Vec::new(), true, false)
            .await
            .unwrap();
        let listed: HashSet<&str> = p.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(listed.len(), 30, "valkey {valkey}: {listed:?}");
        assert!(!listed.contains("user:abc:cart"), "valkey {valkey}");
        assert!(
            !listed.contains("user:7:cart:old"),
            "the glob does not match it"
        );
        assert_eq!(p.other_pattern, 1, "valkey {valkey}");
        assert_eq!((p.cursor, p.carry.len()), (0, 0), "valkey {valkey}");

        // The literal pattern lists its one key.
        let p = page(&mut c, "user:abc:cart", 0, Vec::new(), true, false)
            .await
            .unwrap();
        let listed: Vec<&str> = p.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(listed, ["user:abc:cart"], "valkey {valkey}");
    }
}

#[tokio::test]
async fn n_loads_a_thousand_keys_at_a_time_until_every_key_is_listed() {
    const KEYS: usize = 2_500;
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let mut pipe = redis::pipe();
        for id in 0..KEYS {
            pipe.set(format!("item:{id}"), "v").ignore();
        }
        // Other patterns the walk passes over.
        for id in 0..300 {
            pipe.set(format!("other:{id}"), "v").ignore();
        }
        let () = pipe.query_async(&mut admin).await.unwrap();

        let mut c = connect(&server).await;
        let pages = all_pages(&mut c, &pattern_for_key("item:1")).await;
        assert!(pages.len() >= 3, "valkey {valkey}: {} pages", pages.len());
        assert_eq!(pages[0].rows.len(), PAGE_KEYS, "valkey {valkey}");
        let mut listed = HashSet::new();
        for p in &pages {
            assert!(p.rows.len() <= PAGE_KEYS, "valkey {valkey}");
            for r in &p.rows {
                assert!(r.key.starts_with("item:"), "{}", r.key);
                listed.insert(r.key.clone());
            }
        }
        assert_eq!(listed.len(), KEYS, "valkey {valkey}");
    }
}

#[tokio::test]
async fn each_row_has_its_metadata_from_one_pipeline() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &["--maxmemory-policy", "allkeys-lfu"]).await;
        let mut admin = server.admin().await;
        let () = redis::pipe()
            .set_ex("s:1", "hello", 3600)
            .ignore()
            .hset_multiple("h:1", &[("a", 1), ("b", 2), ("c", 3)])
            .ignore()
            .rpush("l:1", &[1, 2, 3, 4])
            .ignore()
            .sadd("t:1", &[1, 2])
            .ignore()
            .zadd_multiple("z:1", &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")])
            .ignore()
            .cmd("XADD")
            .arg("x:1")
            .arg("*")
            .arg("f")
            .arg("v")
            .ignore()
            .cmd("XADD")
            .arg("x:1")
            .arg("*")
            .arg("f")
            .arg("v")
            .ignore()
            .query_async(&mut admin)
            .await
            .unwrap();

        let mut c = connect(&server).await;
        for (key, key_type, length) in [
            ("s:1", KeyType::String, 5),
            ("h:1", KeyType::Hash, 3),
            ("l:1", KeyType::List, 4),
            ("t:1", KeyType::Set, 2),
            ("z:1", KeyType::ZSet, 5),
            ("x:1", KeyType::Stream, 2),
        ] {
            let p = page(&mut c, &pattern_for_key(key), 0, Vec::new(), true, true)
                .await
                .unwrap();
            assert!(p.refused.is_empty(), "valkey {valkey}: {:?}", p.refused);
            let r = row(&p, key);
            assert_eq!(r.key_type, key_type, "{key}");
            assert_eq!(r.length, Some(length), "valkey {valkey}: {key}");
            assert!(r.bytes.is_some_and(|b| b > 0), "{key}: {r:?}");
            assert!(r.encoding.is_some(), "{key}: {r:?}");
            assert!(r.freq.is_some(), "LFU policy: {key}: {r:?}");
        }
        let p = page(&mut c, &pattern_for_key("s:1"), 0, Vec::new(), true, true)
            .await
            .unwrap();
        match row(&p, "s:1").ttl {
            Ttl::Expires(d) => {
                assert!(d > Duration::from_secs(3500) && d <= Duration::from_secs(3600))
            }
            other => panic!("s:1 expires: {other:?}"),
        }
        assert_eq!(row(&p, "s:1").encoding.as_deref(), Some("embstr"));
        let p = page(&mut c, &pattern_for_key("h:1"), 0, Vec::new(), false, false)
            .await
            .unwrap();
        let r = row(&p, "h:1");
        assert_eq!(r.ttl, Ttl::Persistent);
        assert_eq!((r.bytes, r.freq), (None, None), "not asked for");
    }
}

#[tokio::test]
async fn big_keys_reads_lengths_and_encodings_and_skips_vanished_keys() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let items: Vec<u32> = (0..200).collect();
        let fields: Vec<(String, u32)> = (0..1_000).map(|i| (format!("f{i}"), i)).collect();
        let () = redis::pipe()
            .rpush("queue:emails", &items)
            .ignore()
            .hset_multiple("user:1:cart", &fields)
            .ignore()
            .hset("user:2:cart", "a", 1)
            .ignore()
            .query_async(&mut admin)
            .await
            .unwrap();

        let mut c = connect(&server).await;
        let fact = |key: &str, key_type, length| FactKey {
            key: key.to_string(),
            key_type,
            length,
        };
        let req = Request::Facts {
            keys: vec![
                fact("queue:emails", KeyType::List, true),
                fact("gone:1", KeyType::String, true),
                fact("user:1:cart", KeyType::Hash, true),
                fact("user:2:cart", KeyType::Hash, false),
            ],
        };
        let Reply::Facts { facts, refused } =
            keys::run(&mut c, &Budget::new(None), req).await.unwrap()
        else {
            panic!("not facts");
        };
        assert!(refused.is_empty(), "{refused:?}");
        let names: Vec<&str> = facts.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["queue:emails", "user:1:cart", "user:2:cart"]);
        assert_eq!(facts[0].1.length, Some(200), "valkey {valkey}");
        assert_eq!(facts[1].1.length, Some(1_000));
        // Past hash-max-listpack-entries (128 or 512 by version) a hash leaves listpack.
        assert_eq!(facts[1].1.encoding.as_deref(), Some("hashtable"));
        assert_eq!(facts[2].1.length, None, "not asked for");
        // A small hash is a listpack from Redis 7.0 (and on Valkey); a ziplist before.
        assert!(
            matches!(facts[2].1.encoding.as_deref(), Some("listpack" | "ziplist")),
            "{:?}",
            facts[2].1.encoding
        );
    }
}

#[tokio::test]
async fn a_user_without_memory_usage_gets_every_other_column() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens_without(&["-memory"])).await;
        let mut admin = server.admin().await;
        let () = redis::pipe()
            .hset("user:1:cart", "a", 1)
            .ignore()
            .hset("user:2:cart", "a", 1)
            .ignore()
            .query_async(&mut admin)
            .await
            .unwrap();

        let mut c = connect_as_lens(&server).await;
        let p = page(
            &mut c,
            &pattern_for_key("user:1:cart"),
            0,
            Vec::new(),
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            p.refused,
            [LiveError::Denied("MEMORY USAGE".into())],
            "valkey {valkey}: once, in fixed words"
        );
        assert_eq!(p.rows.len(), 2);
        for r in &p.rows {
            assert_eq!(r.bytes, None);
            assert_eq!(r.key_type, KeyType::Hash);
            assert_eq!(r.length, Some(1));
            assert!(r.encoding.is_some());
        }
        let text = format!("{p:?}");
        for leak in ["NOPERM", "lens", "lens-pw"] {
            assert!(!text.contains(leak), "valkey {valkey}: {leak} in {text}");
        }
    }
}

#[tokio::test]
async fn a_user_without_scan_or_object_is_told_in_fixed_words() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens_without(&["-scan", "-object"])).await;
        let mut admin = server.admin().await;
        let () = redis::cmd("RPUSH")
            .arg("queue:emails")
            .arg(1)
            .query_async(&mut admin)
            .await
            .unwrap();

        let mut c = connect_as_lens(&server).await;
        let e = page(&mut c, "queue:emails", 0, Vec::new(), true, false)
            .await
            .unwrap_err();
        assert_eq!(e, LiveError::Denied("SCAN".into()), "valkey {valkey}");

        let req = Request::Facts {
            keys: vec![FactKey {
                key: "queue:emails".into(),
                key_type: KeyType::List,
                length: true,
            }],
        };
        let Reply::Facts { facts, refused } =
            keys::run(&mut c, &Budget::new(None), req).await.unwrap()
        else {
            panic!("not facts");
        };
        assert_eq!(refused, [LiveError::Denied("OBJECT ENCODING".into())]);
        // The length is still read.
        assert_eq!(facts.len(), 1);
        assert_eq!(
            (facts[0].1.length, facts[0].1.encoding.as_deref()),
            (Some(1), None)
        );
        let text = format!("{facts:?} {refused:?}");
        assert!(!text.contains("lens") && !text.contains("NOPERM"), "{text}");
    }
}
