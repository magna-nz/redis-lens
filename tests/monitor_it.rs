//! The Monitor panel's run against real Redis and Valkey: `conn::open_monitor` and
//! `live::monitor::forward` see the commands other connections send, stop at their
//! limits, mask credentials, and say a refusal in fixed words.
#![cfg(feature = "integration-tests")]

mod common;

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn;
use redis_lens::live::error_for;
use redis_lens::live::monitor::{self, End, Entry, Limits};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;
use tokio::sync::mpsc;
use tokio::time::timeout;

const PASS: &str = "s3cret-monitor-pw";

async fn servers() -> Vec<(&'static str, common::TestServer)> {
    vec![
        ("redis", common::redis().await),
        ("valkey", common::valkey().await),
    ]
}

fn profile(port: u16, auth: Auth) -> Profile {
    Profile {
        name: "monitor".to_string(),
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

async fn cmd(conn: &mut MultiplexedConnection, args: &[&str]) {
    let mut c = redis::cmd(args[0]);
    for a in &args[1..] {
        c.arg(*a);
    }
    let _: redis::Value = c.query_async(conn).await.expect("command");
}

/// Starts a run with `limits` on `p`, and returns where its lines and its end arrive.
async fn start(
    p: &Profile,
    secrets: &MemoryStore,
    budget: &Budget,
    limits: Limits,
) -> (
    mpsc::UnboundedReceiver<Vec<Entry>>,
    tokio::task::JoinHandle<End>,
) {
    let mut m = conn::open_monitor(p, secrets, budget, None)
        .await
        .expect("connect")
        .expect("MONITOR allowed");
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        monitor::forward(&mut m.lines, limits, |b| tx.send(b).is_ok()).await
    });
    (rx, task)
}

async fn collect(mut rx: mpsc::UnboundedReceiver<Vec<Entry>>) -> Vec<Entry> {
    let mut all = Vec::new();
    while let Some(b) = rx.recv().await {
        all.extend(b);
    }
    all
}

#[tokio::test]
async fn monitor_sees_another_connections_commands_with_credentials_masked() {
    for (kind, server) in servers().await {
        let p = profile(server.port, Auth::None);
        let budget = Budget::new(None);
        let limits = Limits {
            lines: monitor::MAX_LINES,
            time: Duration::from_secs(3),
        };
        let (rx, task) = start(&p, &MemoryStore::default(), &budget, limits).await;
        // One MONITOR to check the permission, one for the run.
        assert_eq!(budget.taken(), 2, "{kind}");
        let mut admin = server.admin().await;
        cmd(&mut admin, &["SET", "monitor:key", "plain-value"]).await;
        cmd(
            &mut admin,
            &["ACL", "SETUSER", "mon-tmp", "on", &format!(">{PASS}")],
        )
        .await;
        cmd(&mut admin, &["CONFIG", "SET", "masterauth", PASS]).await;
        let end = timeout(Duration::from_secs(10), task)
            .await
            .expect("it stops by itself")
            .unwrap();
        assert_eq!(end, End::TimeUp, "{kind}");
        let lines = collect(rx).await;
        let shown: Vec<String> = lines.iter().map(Entry::command_line).collect();
        assert!(
            shown.iter().any(|l| l == "SET monitor:key plain-value"),
            "{kind}: {shown:?}"
        );
        let set = lines
            .iter()
            .find(|e| e.command_line().starts_with("SET monitor:key"))
            .unwrap();
        assert_eq!(set.line.db, "0", "{kind}");
        assert!(set.unix().is_some_and(|u| u > 1_600_000_000), "{kind}");
        for l in &shown {
            assert!(!l.contains(PASS), "{kind}: {l}");
        }
        for e in &lines {
            assert!(e.line.args.iter().all(|a| a != PASS.as_bytes()), "{kind}");
        }
        let debug = format!("{lines:?}");
        assert!(
            !debug.contains(PASS) && !debug.contains("plain-value"),
            "{kind}: {debug}"
        );
    }
}

#[tokio::test]
async fn a_run_stops_at_its_line_cap() {
    for (kind, server) in servers().await {
        let p = profile(server.port, Auth::None);
        let limits = Limits {
            lines: 5,
            time: Duration::from_secs(20),
        };
        let (rx, task) = start(&p, &MemoryStore::default(), &Budget::new(None), limits).await;
        let mut admin = server.admin().await;
        for i in 0..20 {
            cmd(&mut admin, &["SET", &format!("cap:{i}"), "x"]).await;
        }
        let end = timeout(Duration::from_secs(10), task)
            .await
            .expect("it stops at the cap")
            .unwrap();
        assert_eq!(end, End::LineCap, "{kind}");
        assert_eq!(collect(rx).await.len(), 5, "{kind}");
    }
}

#[tokio::test]
async fn a_user_without_monitor_is_refused_in_fixed_words() {
    for valkey in [false, true] {
        let server = common::server_with(
            valkey,
            &[
                "--user",
                "default",
                "off",
                "--user",
                "lens",
                "on",
                ">lens-pw",
                "~*",
                "+@read",
                "+ping",
                "+hello",
                "+client|setinfo",
                "+client|setname",
                "+select",
            ],
        )
        .await;
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
        let budget = Budget::new(None);
        let answer = conn::open_monitor(&p, &secrets, &budget, None)
            .await
            .expect("the connect works");
        let Err(e) = answer else {
            panic!("valkey={valkey}: MONITOR was allowed");
        };
        let text = error_for("MONITOR", &e).to_string();
        assert_eq!(
            text, "the server refused MONITOR for this user",
            "valkey={valkey}"
        );
        assert!(!text.contains("lens"));
        // Only the check was sent.
        assert_eq!(budget.taken(), 1, "valkey={valkey}");
    }
}

#[tokio::test]
async fn a_profile_on_an_unsupported_route_is_refused_before_connecting() {
    let mut p = profile(1, Auth::EntraId);
    assert!(conn::monitor_unsupported(&p).is_some());
    let budget = Budget::new(None);
    assert!(
        conn::open_monitor(&p, &MemoryStore::default(), &budget, None)
            .await
            .is_err()
    );
    assert_eq!(budget.taken(), 0);
    p.auth = Auth::None;
    assert!(conn::monitor_unsupported(&p).is_none());
}
