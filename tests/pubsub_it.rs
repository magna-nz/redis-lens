//! The Pub/Sub view's live requests against real Redis and Valkey: counts with a real
//! subscriber.
#![cfg(feature = "integration-tests")]

mod common;

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::live::LiveError;
use redis_lens::live::pubsub::{self, Counts, MAX_CHANNELS, Reply, Request};
use tokio::time::{Instant, sleep};

async fn counts(conn: &mut MultiplexedConnection, budget: &Budget) -> Result<Counts, LiveError> {
    match pubsub::run(conn, budget, Request::Counts).await? {
        Reply::Counts(c) => Ok(c),
    }
}

/// Asks until `done` holds: a subscription takes a moment to show.
async fn counts_until(conn: &mut MultiplexedConnection, done: impl Fn(&Counts) -> bool) -> Counts {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let c = counts(conn, &Budget::new(None)).await.expect("counts");
        if done(&c) || Instant::now() > end {
            return c;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn subs(c: &Counts, name: &str) -> Option<u64> {
    c.channels
        .iter()
        .find(|ch| ch.name == name)
        .and_then(|ch| ch.subscribers)
}

#[tokio::test]
async fn counts_with_real_subscribers() {
    for server in [common::redis().await, common::valkey().await] {
        let client = redis::Client::open(server.url()).unwrap();
        let mut admin = server.admin().await;

        // Nothing subscribed yet.
        let none = counts(&mut admin, &Budget::new(None)).await.unwrap();
        assert!(none.channels.is_empty());
        assert_eq!((none.total_channels, none.patterns), (0, Some(0)));

        let mut a = client.get_async_pubsub().await.unwrap();
        a.subscribe("news").await.unwrap();
        a.subscribe("alerts").await.unwrap();
        a.psubscribe("n*").await.unwrap();
        let mut b = client.get_async_pubsub().await.unwrap();
        b.subscribe("news").await.unwrap();

        let c = counts_until(&mut admin, |c| subs(c, "news") == Some(2)).await;
        assert_eq!(c.total_channels, 2);
        assert_eq!(subs(&c, "news"), Some(2));
        assert_eq!(subs(&c, "alerts"), Some(1));
        // Most subscribers first.
        let names: Vec<_> = c.channels.iter().map(|ch| ch.name.as_str()).collect();
        assert_eq!(names, ["news", "alerts"]);
        // The pattern subscription is not a channel and not in the channel counts.
        assert_eq!(c.patterns, Some(1));
        assert!(c.notes.is_empty(), "{:?}", c.notes);

        // PUBLISH is never sent: the subscribers received nothing.
        drop((a, b));
    }
}

#[tokio::test]
async fn only_the_first_1000_channels_are_listed() {
    let server = common::redis().await;
    let client = redis::Client::open(server.url()).unwrap();
    let mut admin = server.admin().await;
    let mut ps = client.get_async_pubsub().await.unwrap();
    let names: Vec<String> = (0..MAX_CHANNELS + 100)
        .map(|i| format!("ch{i:04}"))
        .collect();
    ps.subscribe(&names).await.unwrap();
    let budget = Budget::new(None);
    let end = Instant::now() + Duration::from_secs(10);
    let c = loop {
        let c = counts(&mut admin, &budget).await.unwrap();
        if c.total_channels == names.len() || Instant::now() > end {
            break c;
        }
        sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(c.total_channels, MAX_CHANNELS + 100);
    assert_eq!(c.channels.len(), MAX_CHANNELS);
    assert!(c.channels.iter().all(|ch| ch.subscribers == Some(1)));
    drop(ps);
}

#[tokio::test]
async fn every_command_takes_from_the_budget() {
    let server = common::redis().await;
    let client = redis::Client::open(server.url()).unwrap();
    let mut admin = server.admin().await;
    let mut ps = client.get_async_pubsub().await.unwrap();
    ps.subscribe("a").await.unwrap();
    counts_until(&mut admin, |c| c.total_channels == 1).await;
    let budget = Budget::new(None);
    counts(&mut admin, &budget).await.unwrap();
    // CHANNELS and NUMPAT, then one NUMSUB for the one batch of channels.
    assert_eq!(budget.taken(), 3);
}

#[tokio::test]
async fn counts_the_user_may_not_read_are_left_out_with_fixed_notes() {
    let args = [
        "--user",
        "zed-ro",
        "on",
        ">zed-pw",
        "~*",
        "+pubsub|channels",
    ];
    for server in [
        common::redis_with(&args).await,
        common::valkey_with(&args).await,
    ] {
        let client = redis::Client::open(server.url()).unwrap();
        let mut ps = client.get_async_pubsub().await.unwrap();
        ps.subscribe("news").await.unwrap();
        let mut zed = server.admin_as("zed-ro", "zed-pw").await;
        let c = counts_until(&mut zed, |c| c.total_channels == 1).await;
        assert_eq!(c.channels.len(), 1);
        assert_eq!(c.channels[0].subscribers, None);
        assert_eq!(c.patterns, None);
        assert_eq!(
            c.notes,
            [
                "the server refused PUBSUB NUMPAT for this user",
                "the server refused PUBSUB NUMSUB for this user"
            ]
        );
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("zed-ro") && !shown.contains("NOPERM"),
            "{shown}"
        );
    }
}

#[tokio::test]
async fn a_user_who_may_not_list_channels_turns_the_view_off() {
    let args = ["--user", "zed-ro", "on", ">zed-pw", "~*", "+ping"];
    let server = common::redis_with(&args).await;
    let mut zed = server.admin_as("zed-ro", "zed-pw").await;
    let err = counts(&mut zed, &Budget::new(None))
        .await
        .expect_err("denied");
    assert_eq!(err, LiveError::Denied("PUBSUB CHANNELS".into()));
    assert!(!err.to_string().contains("zed-ro"));
}
