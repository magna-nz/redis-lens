//! The Pub/Sub view's live requests against real Redis and Valkey: counts with a real
//! subscriber, and the live tail on a connection of its own.
#![cfg(feature = "integration-tests")]

mod common;

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn;
use redis_lens::live::pubsub::{
    self, Counts, MAX_CHANNELS, Push, Reply, Request, Subscription, TailMessage,
};
use redis_lens::live::{CONNECTION_LOST, LiveError};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Instant, sleep, timeout};

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
    for server in [common::redis().await, common::valkey().await] {
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
}

#[tokio::test]
async fn every_command_takes_from_the_budget() {
    for server in [common::redis().await, common::valkey().await] {
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
    for server in [
        common::redis_with(&args).await,
        common::valkey_with(&args).await,
    ] {
        let mut zed = server.admin_as("zed-ro", "zed-pw").await;
        let err = counts(&mut zed, &Budget::new(None))
            .await
            .expect_err("denied");
        assert_eq!(err, LiveError::Denied("PUBSUB CHANNELS".into()));
        assert!(!err.to_string().contains("zed-ro"));
    }
}

// -- The live tail ----------------------------------------------------------------

fn profile(port: u16, auth: Auth) -> Profile {
    Profile {
        name: "tail".to_string(),
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
    }
}

/// The tail's own connection, as the default user or as `user`.
async fn subscriber(
    server: &common::TestServer,
    user: Option<(&str, &str)>,
) -> (conn::Connection, UnboundedReceiver<redis::PushInfo>) {
    let secrets = MemoryStore::default();
    let p = match user {
        None => profile(server.port, Auth::None),
        Some((name, pw)) => {
            let p = profile(server.port, Auth::Keychain);
            secrets
                .set(
                    &p.secret_ref(),
                    &Credentials {
                        username: Some(name.into()),
                        password: SecretString::from(pw),
                    },
                )
                .unwrap();
            p
        }
    };
    conn::open_subscriber(&p, &secrets, None)
        .await
        .expect("open the subscriber")
}

/// The next message pushed, skipping confirmations.
async fn next_message(rx: &mut UnboundedReceiver<redis::PushInfo>) -> TailMessage {
    loop {
        let push = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a push in time")
            .expect("the connection is open");
        match pubsub::parse_push(push, 1) {
            Push::Message(m) => return m,
            Push::Lost => panic!("connection lost"),
            Push::Other => {}
        }
    }
}

async fn publish(admin: &mut MultiplexedConnection, channel: &str, payload: &[u8]) -> i64 {
    redis::cmd("PUBLISH")
        .arg(channel)
        .arg(payload)
        .query_async(admin)
        .await
        .expect("PUBLISH")
}

#[tokio::test]
async fn the_tail_receives_what_is_published_byte_for_byte() {
    for server in [common::redis().await, common::valkey().await] {
        let (mut c, mut rx) = subscriber(&server, None).await;
        let budget = Budget::new(None);
        pubsub::subscribe(&mut c, &budget, &Subscription::Channel("news".into()))
            .await
            .expect("subscribe");
        // The SUBSCRIBE took one command from the budget.
        assert_eq!(budget.taken(), 1);
        let mut admin = server.admin().await;
        let binary = [0u8, 0xff, 0x1b, b'[', b'2', b'J', 0x80, b'\n'];
        assert_eq!(publish(&mut admin, "news", &binary).await, 1);
        assert_eq!(publish(&mut admin, "other", b"not ours").await, 0);
        assert_eq!(publish(&mut admin, "news", b"second").await, 1);
        let m = next_message(&mut rx).await;
        assert_eq!((m.channel.as_str(), m.pattern.as_deref()), ("news", None));
        assert_eq!(m.payload.as_bytes(), binary);
        assert_eq!(next_message(&mut rx).await.payload.as_bytes(), b"second");
    }
}

#[tokio::test]
async fn a_pattern_tail_gets_the_pattern_and_the_channel() {
    for server in [common::redis().await, common::valkey().await] {
        let (mut c, mut rx) = subscriber(&server, None).await;
        let budget = Budget::new(None);
        pubsub::subscribe(&mut c, &budget, &Subscription::Pattern("orders:*".into()))
            .await
            .expect("psubscribe");
        assert_eq!(budget.taken(), 1);
        let mut admin = server.admin().await;
        publish(&mut admin, "news", b"not matched").await;
        assert_eq!(publish(&mut admin, "orders:eu", b"{\"id\":1}").await, 1);
        let m = next_message(&mut rx).await;
        assert_eq!(m.pattern.as_deref(), Some("orders:*"));
        assert_eq!(m.channel, "orders:eu");
        assert_eq!(m.payload.as_bytes(), b"{\"id\":1}");
    }
}

#[tokio::test]
async fn channel_rules_that_leave_the_channel_out_say_so_in_fixed_words() {
    let args = [
        "--user",
        "zed-ro",
        "on",
        ">zed-pw",
        "~*",
        "resetchannels",
        "&allowed",
        "+@all",
    ];
    for server in [
        common::redis_with(&args).await,
        common::valkey_with(&args).await,
    ] {
        let (mut c, _rx) = subscriber(&server, Some(("zed-ro", "zed-pw"))).await;
        let budget = Budget::new(None);
        for to in [
            Subscription::Channel("news".into()),
            Subscription::Pattern("news:*".into()),
        ] {
            let err = pubsub::subscribe(&mut c, &budget, &to)
                .await
                .expect_err("refused");
            assert_eq!(err, to.denied());
            assert!(!err.contains("zed-ro") && !err.contains("NOPERM"), "{err}");
        }
        assert_eq!(
            Subscription::Channel("news".into()).denied(),
            "the server refused SUBSCRIBE on that channel for this user"
        );
        // The connection still works for an allowed channel.
        pubsub::subscribe(&mut c, &budget, &Subscription::Channel("allowed".into()))
            .await
            .expect("an allowed channel");
        assert_eq!(budget.taken(), 3);
    }
}

#[tokio::test]
async fn a_user_without_subscribe_is_refused_in_fixed_words() {
    let args = [
        "--user", "zed-ro", "on", ">zed-pw", "~*", "&*", "+ping", "+hello",
    ];
    for server in [
        common::redis_with(&args).await,
        common::valkey_with(&args).await,
    ] {
        let (mut c, _rx) = subscriber(&server, Some(("zed-ro", "zed-pw"))).await;
        let to = Subscription::Channel("news".into());
        let err = pubsub::subscribe(&mut c, &Budget::new(None), &to)
            .await
            .expect_err("refused");
        assert_eq!(
            err,
            "the server refused SUBSCRIBE on that channel for this user"
        );
    }
}

#[tokio::test]
async fn a_killed_subscriber_is_a_lost_connection() {
    for server in [common::redis().await, common::valkey().await] {
        let (mut c, mut rx) = subscriber(&server, None).await;
        pubsub::subscribe(
            &mut c,
            &Budget::new(None),
            &Subscription::Channel("news".into()),
        )
        .await
        .unwrap();
        let mut admin = server.admin().await;
        let killed: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("TYPE")
            .arg("pubsub")
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(killed, 1);
        let lost = timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    None => return true,
                    Some(p) => {
                        if pubsub::parse_push(p, 0) == Push::Lost {
                            return true;
                        }
                    }
                }
            }
        })
        .await
        .expect("the loss is noticed");
        assert!(lost);
        // What the tail then says.
        assert_eq!(CONNECTION_LOST, "the connection to the server was lost");
    }
}
