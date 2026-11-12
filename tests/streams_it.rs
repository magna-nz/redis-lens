//! The Streams view's live requests against real Redis and Valkey: real streams with
//! groups and consumers, and the commands refused for the user.
#![cfg(feature = "integration-tests")]

mod common;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::live::LiveError;
use redis_lens::live::streams::{
    self, Fetched, GroupInfo, LAGGING_LAG, Lag, MAX_STREAMS, Refused, Reply, Request, Verdict,
    verdicts,
};

async fn run(
    conn: &mut MultiplexedConnection,
    budget: &Budget,
    keys: &[&str],
) -> Result<Vec<Fetched>, LiveError> {
    Ok(run_with(conn, budget, keys, Refused::default()).await?.0)
}

/// [`run`], sending `refused` and returning what the server refused so far.
async fn run_with(
    conn: &mut MultiplexedConnection,
    budget: &Budget,
    keys: &[&str],
    refused: Refused,
) -> Result<(Vec<Fetched>, Refused), LiveError> {
    let keys = keys.iter().map(|k| (*k).to_string()).collect();
    match streams::run(conn, budget, Request::Info { keys, refused }).await? {
        Reply::Info { streams, refused } => Ok((streams, refused)),
    }
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

/// The major version Redis reports (Valkey reports 7.2 for compatibility).
async fn redis_major(conn: &mut MultiplexedConnection) -> u32 {
    let info: String = cmd(conn, &["INFO", "server"]).await;
    info.lines()
        .find_map(|l| l.strip_prefix("redis_version:"))
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse().ok())
        .expect("redis_version")
}

/// `events` has six entries and two groups, `workers` (two consumers holding entries) and
/// `audit` (caught up); `bare` has entries and no group; `plain` is a string.
async fn seed(conn: &mut MultiplexedConnection) {
    for i in 0..6 {
        let id = format!("1700000000{i:03}-0");
        let _: String = cmd(
            conn,
            &[
                "XADD",
                "events",
                &id,
                "user",
                "alice",
                "token",
                "hunter2-entry-value",
            ],
        )
        .await;
    }
    let _: String = cmd(conn, &["XGROUP", "CREATE", "events", "workers", "0"]).await;
    let _: Value = cmd(
        conn,
        &[
            "XREADGROUP",
            "GROUP",
            "workers",
            "c1",
            "COUNT",
            "3",
            "STREAMS",
            "events",
            ">",
        ],
    )
    .await;
    let _: Value = cmd(
        conn,
        &[
            "XREADGROUP",
            "GROUP",
            "workers",
            "c2",
            "COUNT",
            "1",
            "STREAMS",
            "events",
            ">",
        ],
    )
    .await;
    let _: String = cmd(conn, &["XGROUP", "CREATE", "events", "audit", "$"]).await;
    let _: String = cmd(conn, &["XADD", "bare", "*", "k", "v"]).await;
    let _: String = cmd(conn, &["SET", "plain", "x"]).await;
}

type Value = redis::Value;

fn group<'a>(list: &'a [Fetched], key: &str, name: &str) -> &'a GroupInfo {
    list.iter()
        .find(|f| f.key == key)
        .and_then(|f| f.info.as_ref())
        .and_then(|i| i.groups.as_ref())
        .and_then(|g| g.iter().find(|g| g.name == name))
        .unwrap_or_else(|| panic!("no group {name} on {key}"))
}

#[tokio::test]
async fn streams_with_groups_and_consumers() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        seed(&mut admin).await;
        let major = redis_major(&mut admin).await;
        let budget = Budget::new(None);
        let list = run(&mut admin, &budget, &["events", "bare", "plain", "missing"])
            .await
            .expect("run");
        assert_eq!(list.len(), 4);

        let events = list[0].info.as_ref().expect("events is a stream");
        assert_eq!(events.length, 6);
        assert_eq!(events.first_id.as_deref(), Some("1700000000000-0"));
        assert_eq!(events.last_id, "1700000000005-0");
        assert!(events.bytes.is_some_and(|b| b > 0), "{:?}", events.bytes);
        // The age is from the server's clock: far more than a year, far less than 100.
        let age_days = events.oldest_age_ms.expect("age") / 86_400_000;
        assert!((365..36_500).contains(&age_days), "{age_days}");
        assert!(events.notes.is_empty(), "{:?}", events.notes);

        let groups = events.groups.as_ref().expect("groups");
        assert_eq!(groups.len(), 2);
        let workers = group(&list, "events", "workers");
        assert_eq!((workers.consumers, workers.pending), (2, 4));
        assert_eq!(workers.last_delivered_id, "1700000000003-0");
        let audit = group(&list, "events", "audit");
        assert_eq!((audit.consumers, audit.pending), (0, 0));
        if major >= 7 {
            assert_eq!(workers.lag, Lag::Known(2), "{workers:?}");
            assert_eq!(audit.lag, Lag::Known(0), "{audit:?}");
        } else {
            assert_eq!(workers.lag, Lag::Unsupported);
        }
        let readers = workers.readers.as_ref().expect("consumers");
        let c1 = readers.iter().find(|c| c.name == "c1").expect("c1");
        let c2 = readers.iter().find(|c| c.name == "c2").expect("c2");
        assert_eq!((c1.pending, c2.pending), (3, 1));
        if major >= 7 {
            // `inactive` is Redis 7.2 and later; Valkey has it too.
            let is_valkey = server_is_valkey(&mut admin).await;
            if is_valkey || redis_minor(&mut admin).await >= 2 {
                assert!(c1.inactive_ms.is_some(), "{c1:?}");
            }
        } else {
            assert_eq!(c1.inactive_ms, None);
        }
        assert_eq!(audit.readers.as_deref(), Some(&[][..]));
        assert_eq!(verdicts(events), [Verdict::Ok]);

        // No group: no readers.
        let bare = list[1].info.as_ref().expect("bare");
        assert_eq!(bare.length, 1);
        assert_eq!(bare.groups.as_deref(), Some(&[][..]));
        assert_eq!(verdicts(bare), [Verdict::NoReaders]);

        // A string, and a key that does not exist: gone, not an error.
        assert_eq!(list[2].info, None);
        assert_eq!(list[3].info, None);

        // No entry value anywhere in what was returned.
        let shown = format!("{list:?}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("alice"),
            "{shown}"
        );
    }
}

async fn server_is_valkey(conn: &mut MultiplexedConnection) -> bool {
    let info: String = cmd(conn, &["INFO", "server"]).await;
    info.contains("valkey_version:")
}

async fn redis_minor(conn: &mut MultiplexedConnection) -> u32 {
    let info: String = cmd(conn, &["INFO", "server"]).await;
    info.lines()
        .find_map(|l| l.strip_prefix("redis_version:"))
        .and_then(|v| v.split('.').nth(1))
        .and_then(|v| v.parse().ok())
        .expect("redis_version")
}

#[tokio::test]
async fn every_command_takes_from_the_budget() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        seed(&mut admin).await;
        let budget = Budget::new(None);
        run(&mut admin, &budget, &["events"]).await.expect("run");
        // TIME, then XINFO STREAM + XINFO GROUPS + MEMORY USAGE, then XINFO CONSUMERS for
        // each of the two groups.
        assert_eq!(budget.taken(), 1 + 3 + 2);
    }
}

#[tokio::test]
async fn a_lagging_stream_is_flagged() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        if redis_major(&mut admin).await < 7 {
            continue;
        }
        let mut pipe = redis::pipe();
        for _ in 0..=LAGGING_LAG {
            pipe.cmd("XADD")
                .arg("busy")
                .arg("*")
                .arg("k")
                .arg("v")
                .ignore();
        }
        let _: () = pipe.query_async(&mut admin).await.unwrap();
        let _: String = cmd(&mut admin, &["XGROUP", "CREATE", "busy", "slow", "0"]).await;
        let list = run(&mut admin, &Budget::new(None), &["busy"])
            .await
            .expect("run");
        let info = list[0].info.as_ref().expect("busy");
        assert_eq!(info.length, LAGGING_LAG + 1);
        assert_eq!(
            group(&list, "busy", "slow").lag,
            Lag::Known(LAGGING_LAG + 1)
        );
        assert_eq!(verdicts(info), [Verdict::Lagging]);
    }
}

#[tokio::test]
async fn at_most_the_first_streams_are_read() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        let mut pipe = redis::pipe();
        let names: Vec<String> = (0..MAX_STREAMS + 5).map(|i| format!("s{i:03}")).collect();
        for n in &names {
            pipe.cmd("XADD").arg(n).arg("*").arg("k").arg("v").ignore();
        }
        let _: () = pipe.query_async(&mut admin).await.unwrap();
        let keys: Vec<&str> = names.iter().map(String::as_str).collect();
        let list = run(&mut admin, &Budget::new(None), &keys)
            .await
            .expect("run");
        assert_eq!(list.len(), MAX_STREAMS);
        assert!(list.iter().all(|f| f.info.is_some()));
    }
}

/// An ACL user that may read the stream and its groups but not its consumers, the clock
/// or key sizes: those parts are left out with fixed notes, nothing else changes.
#[tokio::test]
async fn parts_the_user_may_not_read_are_left_out_with_fixed_notes() {
    let args = [
        "--user",
        "zed-ro",
        "on",
        ">zed-pw",
        "~*",
        "+xinfo|stream",
        "+xinfo|groups",
    ];
    for server in [
        common::redis_with(&args).await,
        common::valkey_with(&args).await,
    ] {
        let mut admin = server.admin().await;
        seed(&mut admin).await;
        let mut zed = server.admin_as("zed-ro", "zed-pw").await;
        let (list, refused) = run_with(
            &mut zed,
            &Budget::new(None),
            &["events"],
            Refused::default(),
        )
        .await
        .expect("run");
        assert_eq!(
            refused,
            Refused {
                time: true,
                groups: false,
                consumers: true,
                memory: true,
            }
        );
        // The next poll sends the refusals back: only XINFO STREAM and XINFO GROUPS go
        // out, and the notes stay.
        let budget = Budget::new(None);
        let (again, _) = run_with(&mut zed, &budget, &["events"], refused)
            .await
            .expect("run again");
        assert_eq!(budget.taken(), 2);
        assert_eq!(
            again[0].info.as_ref().unwrap().notes,
            list[0].info.as_ref().unwrap().notes
        );
        let info = list[0].info.as_ref().expect("events");
        assert_eq!(info.length, 6);
        assert_eq!(info.bytes, None);
        let workers = group(&list, "events", "workers");
        assert_eq!(workers.pending, 4);
        assert_eq!(workers.readers, None);
        assert!(
            info.notes
                .contains(&"the server refused MEMORY USAGE for this user".to_string()),
            "{:?}",
            info.notes
        );
        assert!(
            info.notes
                .contains(&"the server refused XINFO CONSUMERS for this user".to_string()),
            "{:?}",
            info.notes
        );
        let shown = format!("{list:?}");
        assert!(
            !shown.contains("zed-ro") && !shown.contains("zed-pw") && !shown.contains("NOPERM"),
            "{shown}"
        );
    }
}

#[tokio::test]
async fn a_user_who_may_not_run_xinfo_turns_the_view_off() {
    let args = ["--user", "zed-ro", "on", ">zed-pw", "~*", "+ping"];
    for server in [
        common::redis_with(&args).await,
        common::valkey_with(&args).await,
    ] {
        let mut admin = server.admin().await;
        seed(&mut admin).await;
        let mut zed = server.admin_as("zed-ro", "zed-pw").await;
        let err = run(&mut zed, &Budget::new(None), &["events"])
            .await
            .expect_err("denied");
        assert_eq!(err, LiveError::Denied("XINFO STREAM".into()));
        let text = err.to_string();
        assert!(
            !text.contains("zed-ro") && !text.contains("NOPERM"),
            "{text}"
        );
    }
}

// -----------------------------------------------------------------------------------
// Entries (phase 2): `enter` reads them as the key detail does, with `XREVRANGE`.
// -----------------------------------------------------------------------------------

use redis_lens::live::values::{self, At, Items, Kind, Page};

async fn entries_page(
    conn: &mut MultiplexedConnection,
    budget: &Budget,
    at: Option<At>,
) -> Result<Page, LiveError> {
    let req = values::Request {
        key: "big".into(),
        kind: Kind::Stream,
        at,
        field_ttls: false,
        size: None,
    };
    match values::run(conn, budget, req).await? {
        values::Reply::Page(p) => Ok(p),
        values::Reply::Gone => panic!("the stream is gone"),
    }
}

fn ids(p: &Page) -> Vec<String> {
    match &p.items {
        Items::Stream(e) => e.iter().map(|e| e.id.clone()).collect(),
        _ => panic!("not stream entries"),
    }
}

#[tokio::test]
async fn entries_read_newest_first_in_pages_of_100() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        let mut pipe = redis::pipe();
        for i in 1..=150u32 {
            pipe.cmd("XADD")
                .arg("big")
                .arg(format!("{i}-0"))
                .arg("n")
                .arg(i)
                .arg("payload")
                .arg("hunter2-entry-value")
                .ignore();
        }
        let () = pipe.query_async(&mut admin).await.unwrap();
        let budget = Budget::new(None);
        let first = entries_page(&mut admin, &budget, None)
            .await
            .expect("first");
        // TYPE and XREVRANGE, pipelined.
        assert_eq!(budget.taken(), 2);
        assert_eq!(first.type_name.as_deref(), Some("stream"));
        let want: Vec<String> = (51..=150).rev().map(|i| format!("{i}-0")).collect();
        assert_eq!(ids(&first), want);
        assert_eq!(first.next, Some(At::Before("51-0".into())));
        let next = entries_page(&mut admin, &budget, first.next.clone())
            .await
            .expect("next");
        assert_eq!(budget.taken(), 3);
        let want: Vec<String> = (1..=50).rev().map(|i| format!("{i}-0")).collect();
        assert_eq!(ids(&next), want);
        assert_eq!(next.next, None);
        // The values are held as Raw: Debug shows lengths only.
        let shown = format!("{first:?} {next:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }
}

#[tokio::test]
async fn a_user_without_xrevrange_is_refused_in_fixed_words() {
    let args = ["--user", "zed-ro", "on", ">zed-pw", "~*", "+type", "+xinfo"];
    for server in [
        common::redis_with(&args).await,
        common::valkey_with(&args).await,
    ] {
        let mut admin = server.admin().await;
        let _: String = cmd(&mut admin, &["XADD", "big", "1-0", "a", "b"]).await;
        let mut zed = server.admin_as("zed-ro", "zed-pw").await;
        let err = entries_page(&mut zed, &Budget::new(None), None)
            .await
            .expect_err("denied");
        assert_eq!(err, LiveError::Denied("XREVRANGE".into()));
        let text = err.to_string();
        assert!(
            !text.contains("zed-ro") && !text.contains("NOPERM"),
            "{text}"
        );
    }
}
