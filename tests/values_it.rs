//! The key detail's value reads (`live::values`), against real Redis and Valkey servers.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashSet;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn;
use redis_lens::live::values::{
    self, At, Items, Kind, MAX_ITEMS, PAGE_ITEMS, Page, Reply, Request, STRING_BYTES,
    field_ttls_supported,
};
use redis_lens::live::{self, LiveError, LiveReply, LiveRequest};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile, ServerFlavor, Ttl};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;

fn profile(port: u16, auth: Auth) -> Profile {
    Profile {
        name: "values".to_string(),
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
        sentinel: None,
        tags: Vec::new(),
    }
}

async fn connect(server: &common::TestServer) -> MultiplexedConnection {
    conn::connect(&profile(server.port, Auth::None), &MemoryStore::default())
        .await
        .expect("connect")
        .into_inner()
}

/// Connects as the ACL user `user` with password `pw`.
async fn connect_as(server: &common::TestServer, user: &str, pw: &str) -> MultiplexedConnection {
    let secrets = MemoryStore::default();
    let p = profile(server.port, Auth::Keychain);
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

/// Whether the server has `HTTL`, the way the key detail decides it: from the version.
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

fn req(key: &str, kind: Kind, at: Option<At>, field_ttls: bool) -> Request {
    Request {
        key: key.to_string(),
        kind,
        at,
        field_ttls,
        size: Some(1_000),
    }
}

/// One page, through `live::run` as the live task runs it.
async fn page(
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

/// Every page of `key` read as `kind`, as `n` loads them: the first with `TYPE`.
async fn all_pages(
    c: &mut MultiplexedConnection,
    budget: &Budget,
    key: &str,
    kind: Kind,
    field_ttls: bool,
) -> Vec<Page> {
    let mut out = Vec::new();
    let mut at = None;
    loop {
        let first = out.is_empty();
        let p = as_page(
            page(c, budget, req(key, kind.clone(), at.clone(), field_ttls))
                .await
                .expect("page"),
        );
        if first {
            assert!(p.type_name.is_some());
        }
        at = p.next.clone();
        out.push(p);
        if at.is_none() {
            return out;
        }
        assert!(out.len() < 200, "the walk never ends");
    }
}

#[tokio::test]
async fn a_big_hash_reads_in_hscan_pages_and_every_field_once() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let mut pipe = redis::pipe();
        // More fields than a listpack holds (512 on Redis 7.4), so HSCAN pages.
        for i in 0..1_000 {
            pipe.hset("h", format!("f{i}"), format!("v{i}")).ignore();
        }
        let () = pipe.query_async(&mut admin).await.unwrap();
        let mut c = connect(&server).await;
        let budget = Budget::new(None);
        let pages = all_pages(&mut c, &budget, "h", Kind::Hash, false).await;
        assert!(pages.len() > 1, "valkey {valkey}: one page");
        let mut seen = HashSet::new();
        for p in &pages {
            let Items::Hash(fields) = &p.items else {
                panic!("valkey {valkey}: {p:?}")
            };
            for f in fields {
                seen.insert(f.name.as_bytes().to_vec());
                let n = std::str::from_utf8(&f.name.as_bytes()[1..]).unwrap();
                assert_eq!(f.value.as_bytes(), format!("v{n}").as_bytes());
                assert_eq!(f.ttl, None);
            }
        }
        assert_eq!(seen.len(), 1_000, "valkey {valkey}");
        // TYPE and HSCAN on the first page, one HSCAN on each next one, no HTTL.
        assert_eq!(budget.taken(), pages.len() as u64 + 1, "valkey {valkey}");
    }
}

#[tokio::test]
async fn a_100_kib_string_reads_its_first_64_kib_and_its_length() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let big = vec![b'x'; 100 * 1024];
        let () = redis::cmd("SET")
            .arg("s")
            .arg(&big)
            .query_async(&mut admin)
            .await
            .unwrap();
        let mut c = connect(&server).await;
        let budget = Budget::new(None);
        let p = as_page(
            page(&mut c, &budget, req("s", Kind::String, None, false))
                .await
                .unwrap(),
        );
        assert_eq!(p.type_name.as_deref(), Some("string"));
        let Items::String { value, len } = p.items else {
            panic!("{:?}", p.items)
        };
        assert_eq!(
            (value.len(), len),
            (STRING_BYTES, Some(100 * 1024)),
            "valkey {valkey}"
        );
        assert_eq!(p.next, None);
        assert_eq!(budget.taken(), 3, "TYPE, STRLEN, GETRANGE");
    }
}

#[tokio::test]
async fn lists_sets_sorted_sets_and_streams_read_in_pages() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let mut pipe = redis::pipe();
        for i in 0..250 {
            pipe.rpush("l", format!("e{i}")).ignore();
            pipe.sadd("set", format!("m{i}")).ignore();
        }
        for i in 0..150 {
            pipe.zadd("z", format!("m{i}"), i as f64 + 0.5).ignore();
            pipe.cmd("XADD")
                .arg("x")
                .arg(format!("{}-0", i + 1))
                .arg("n")
                .arg(i)
                .ignore();
        }
        let () = pipe.query_async(&mut admin).await.unwrap();
        let mut c = connect(&server).await;
        let budget = Budget::new(None);

        // A list, in index order, 100 at a time.
        let pages = all_pages(&mut c, &budget, "l", Kind::List, false).await;
        let lens: Vec<usize> = pages.iter().map(|p| p.items.len()).collect();
        assert!(
            lens.iter().all(|n| *n <= PAGE_ITEMS),
            "valkey {valkey}: {lens:?}"
        );
        let mut all = Vec::new();
        for p in &pages {
            let Items::List(v) = &p.items else { panic!() };
            all.extend(v.iter().map(|e| e.as_bytes().to_vec()));
        }
        let want: Vec<Vec<u8>> = (0..250).map(|i| format!("e{i}").into_bytes()).collect();
        assert_eq!(all, want, "valkey {valkey}");

        // A set, every member once.
        let pages = all_pages(&mut c, &budget, "set", Kind::Set, false).await;
        let mut seen = HashSet::new();
        for p in &pages {
            let Items::Set(v) = &p.items else { panic!() };
            seen.extend(v.iter().map(|m| m.as_bytes().to_vec()));
        }
        assert_eq!(seen.len(), 250, "valkey {valkey}");

        // A sorted set, by score, with scores.
        let pages = all_pages(&mut c, &budget, "z", Kind::ZSet, false).await;
        assert_eq!(pages.len(), 2, "valkey {valkey}");
        let Items::ZSet(first) = &pages[0].items else {
            panic!()
        };
        assert_eq!(first.len(), PAGE_ITEMS);
        assert_eq!(first[0].member.as_bytes(), b"m0");
        assert_eq!(first[0].score.as_bytes(), b"0.5");
        assert_eq!(pages[1].items.len(), 50);

        // A stream, newest first; the second page goes on before the last ID.
        let pages = all_pages(&mut c, &budget, "x", Kind::Stream, false).await;
        assert_eq!(pages.len(), 2, "valkey {valkey}");
        let ids: Vec<String> = pages
            .iter()
            .flat_map(|p| match &p.items {
                Items::Stream(e) => e.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
                _ => panic!(),
            })
            .collect();
        let want: Vec<String> = (1..=150).rev().map(|i| format!("{i}-0")).collect();
        assert_eq!(ids, want, "valkey {valkey}");
        let Items::Stream(e) = &pages[0].items else {
            panic!()
        };
        assert_eq!(e[0].fields[0].0.as_bytes(), b"n");
        assert_eq!(e[0].fields[0].1.as_bytes(), b"149");
    }
}

#[tokio::test]
async fn a_gone_key_and_a_key_whose_type_changed() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let () = redis::cmd("RPUSH")
            .arg("was-a-string")
            .arg("a")
            .arg("b")
            .query_async(&mut admin)
            .await
            .unwrap();
        let mut c = connect(&server).await;
        let budget = Budget::new(None);
        let r = page(&mut c, &budget, req("nope", Kind::Hash, None, false))
            .await
            .unwrap();
        assert_eq!(r, Reply::Gone, "valkey {valkey}");
        // The key list said string; TYPE says list, and the list is what is read.
        let p = as_page(
            page(
                &mut c,
                &budget,
                req("was-a-string", Kind::String, None, false),
            )
            .await
            .unwrap(),
        );
        assert_eq!(p.type_name.as_deref(), Some("list"));
        assert_eq!(p.items.len(), 2, "valkey {valkey}");
    }
}

#[tokio::test]
async fn field_ttls_where_the_server_has_httl() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let () = redis::cmd("HSET")
            .arg("h")
            .arg("short")
            .arg("1")
            .arg("forever")
            .arg("2")
            .query_async(&mut admin)
            .await
            .unwrap();
        let mut c = connect(&server).await;
        let on = has_field_ttls(&mut c).await;
        let budget = Budget::new(None);
        if on {
            let _: redis::Value = redis::cmd("HEXPIRE")
                .arg("h")
                .arg(3600)
                .arg("FIELDS")
                .arg(1)
                .arg("short")
                .query_async(&mut admin)
                .await
                .unwrap();
            let p = as_page(
                page(&mut c, &budget, req("h", Kind::Hash, None, true))
                    .await
                    .unwrap(),
            );
            let Items::Hash(fields) = &p.items else {
                panic!()
            };
            let ttl = |name: &[u8]| {
                fields
                    .iter()
                    .find(|f| f.name.as_bytes() == name)
                    .unwrap()
                    .ttl
            };
            assert!(
                matches!(ttl(b"short"), Some(Ttl::Expires(d)) if d.as_secs() > 3500),
                "{:?}",
                ttl(b"short")
            );
            assert_eq!(ttl(b"forever"), Some(Ttl::Persistent));
            assert!(p.refused.is_empty());
            assert_eq!(budget.taken(), 3, "TYPE, HSCAN, HTTL");
        } else {
            // Valkey 8.1 and Redis before 7.4: the view sends field_ttls false, and no
            // HTTL goes out.
            let p = as_page(
                page(&mut c, &budget, req("h", Kind::Hash, None, false))
                    .await
                    .unwrap(),
            );
            assert_eq!(budget.taken(), 2, "valkey {valkey}: TYPE, HSCAN only");
            let Items::Hash(fields) = &p.items else {
                panic!()
            };
            assert!(fields.iter().all(|f| f.ttl.is_none()));
            // Asked anyway, the missing command only turns field TTLs off.
            let p = as_page(
                page(&mut c, &budget, req("h", Kind::Hash, None, true))
                    .await
                    .unwrap(),
            );
            assert_eq!(p.refused, [LiveError::Unsupported("HTTL".into())]);
            assert_eq!(p.items.len(), 2);
        }
    }
}

#[tokio::test]
async fn a_user_without_hscan_is_refused_in_fixed_words() {
    for valkey in [false, true] {
        let args = ["--user", "lens", "on", ">lens-pw", "~*", "+@all", "-hscan"];
        let server = common::server_with(valkey, &args).await;
        let mut admin = server.admin().await;
        let () = redis::cmd("HSET")
            .arg("h")
            .arg("f")
            .arg("v")
            .query_async(&mut admin)
            .await
            .unwrap();
        let mut c = connect_as(&server, "lens", "lens-pw").await;
        let e = page(
            &mut c,
            &Budget::new(None),
            req("h", Kind::Hash, None, false),
        )
        .await
        .unwrap_err();
        assert_eq!(e, LiveError::Denied("HSCAN".into()), "valkey {valkey}");
        let text = e.to_string();
        assert_eq!(text, "the server refused HSCAN for this user");
        assert!(!text.contains("lens"), "{text}");
    }
}

/// `-@all +@read -keys` and what a connection needs: every type reads, so nothing
/// the key detail sends is a write.
#[tokio::test]
async fn a_read_only_user_reads_every_type() {
    for valkey in [false, true] {
        let args = [
            "--user",
            "ro",
            "on",
            ">ro-pw",
            "~*",
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
        let server = common::server_with(valkey, &args).await;
        let mut admin = server.admin().await;
        let mut pipe = redis::pipe();
        pipe.set("s", "v").ignore();
        pipe.hset("h", "f", "v").ignore();
        pipe.rpush("l", "e").ignore();
        pipe.sadd("set", "m").ignore();
        pipe.zadd("z", "m", 1).ignore();
        pipe.cmd("XADD")
            .arg("x")
            .arg("*")
            .arg("f")
            .arg("v")
            .ignore();
        let () = pipe.query_async(&mut admin).await.unwrap();
        let mut c = connect_as(&server, "ro", "ro-pw").await;
        let ttls = has_field_ttls(&mut c).await;
        let budget = Budget::new(None);
        for (key, kind) in [
            ("s", Kind::String),
            ("h", Kind::Hash),
            ("l", Kind::List),
            ("set", Kind::Set),
            ("z", Kind::ZSet),
            ("x", Kind::Stream),
        ] {
            let p = as_page(
                page(&mut c, &budget, req(key, kind.clone(), None, ttls))
                    .await
                    .unwrap_or_else(|e| panic!("valkey {valkey}: {key}: {e}")),
            );
            assert_eq!(p.items.len(), 1, "valkey {valkey}: {key}");
            assert!(
                p.refused.is_empty(),
                "valkey {valkey}: {key}: {:?}",
                p.refused
            );
        }
    }
}

#[test]
fn the_caps_are_what_the_spec_says() {
    assert_eq!(PAGE_ITEMS, 100);
    assert_eq!(STRING_BYTES, 64 * 1024);
    assert_eq!(values::JSON_MAX_SIZE, 256 * 1024);
    assert_eq!(values::JSON_SHOWN, 64 * 1024);
    assert_eq!(MAX_ITEMS, 10_000);
}
