//! The live connection, the shared command budget and the view permission probes,
//! against real Redis and Valkey servers.
#![cfg(feature = "integration-tests")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::conn::{
    self, PERM_CLIENT_LIST, PERM_PUBSUB, PERM_SEARCH, PERM_SLOWLOG, PERM_XINFO, VIEW_PERM_LABELS,
};
use redis_lens::live::{self, LiveError, LiveReply, LiveRequest, ReqId};
use redis_lens::model::{
    Auth, Credentials, Endpoint, Env, PERM_LABELS, PermStatus, PermissionReport, Profile,
};
use redis_lens::scan::{self, ScanOptions};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};

const WAIT: Duration = Duration::from_secs(10);

fn profile(port: u16, auth: Auth) -> Profile {
    Profile {
        name: "live".to_string(),
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
    }
}

async fn connect(server: &common::TestServer) -> MultiplexedConnection {
    conn::connect(&profile(server.port, Auth::None), &MemoryStore::default())
        .await
        .expect("connect")
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
    conn::connect(&p, &secrets).await.expect("connect as lens")
}

fn ping() -> LiveRequest {
    LiveRequest::Server(live::server::Request::Ping)
}

type Answer = (ReqId, Result<LiveReply, LiveError>);

/// A live task on `c` whose answers arrive on the returned receiver and whose close
/// reason arrives on the oneshot.
fn start(
    c: MultiplexedConnection,
    budget: Arc<Budget>,
) -> (
    live::LiveHandle,
    mpsc::UnboundedReceiver<Answer>,
    oneshot::Receiver<Option<String>>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (close_tx, close_rx) = oneshot::channel();
    let handle = live::spawn_with_close(
        c,
        budget,
        move |id, result| {
            let _ = tx.send((id, result));
        },
        move |why| {
            let _ = close_tx.send(why);
        },
    );
    (handle, rx, close_rx)
}

async fn next(rx: &mut mpsc::UnboundedReceiver<Answer>) -> Answer {
    timeout(WAIT, rx.recv())
        .await
        .expect("no answer in time")
        .expect("the live task dropped its reply callback")
}

/// The number of normal clients connected, as the admin connection sees it.
async fn clients(admin: &mut MultiplexedConnection) -> usize {
    let list: String = redis::cmd("CLIENT")
        .arg("LIST")
        .arg("TYPE")
        .arg("normal")
        .query_async(admin)
        .await
        .unwrap();
    list.lines().filter(|l| !l.trim().is_empty()).count()
}

fn status<'a>(r: &'a PermissionReport, label: &str) -> &'a PermStatus {
    &r.checks
        .iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("no {label} check"))
        .status
}

#[tokio::test]
async fn ping_goes_through_the_live_task() {
    for server in [common::redis().await, common::valkey().await] {
        let budget = Budget::new(None);
        let (handle, mut rx, _close) = start(connect(&server).await, Arc::clone(&budget));
        assert!(handle.send(ReqId(1), ping()));
        assert!(handle.send(ReqId(2), ping()));
        let mut got = vec![next(&mut rx).await, next(&mut rx).await];
        got.sort_by_key(|(id, _)| id.0);
        for (i, (id, result)) in got.into_iter().enumerate() {
            assert_eq!(id, ReqId(i as u64 + 1));
            assert_eq!(result, Ok(LiveReply::Server(live::server::Reply::Pong)));
        }
        // Each PING was taken from the budget.
        assert_eq!(budget.taken(), 2);
    }
}

async fn total_commands(admin: &mut MultiplexedConnection) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("stats")
        .query_async(admin)
        .await
        .unwrap();
    info.lines()
        .find_map(|l| l.strip_prefix("total_commands_processed:"))
        .expect("total_commands_processed")
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn the_scan_and_the_live_task_share_one_budget() {
    const RATE: u32 = 1_000;
    const PINGS: u64 = 100;
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        let mut pipe = redis::pipe();
        for i in 0..600 {
            pipe.cmd("SET").arg(format!("user:{i}")).arg("v").ignore();
        }
        let _: () = pipe.query_async(&mut admin).await.unwrap();

        let mut c = connect(&server).await;
        let perms = conn::check_permissions(&mut c).await;
        let opts = ScanOptions {
            batch_size: 50,
            scan_count: 100,
            max_commands_per_sec: None,
            ..ScanOptions::default()
        };
        let budget = Budget::new(Some(RATE));
        let (handle, mut rx, _close) = start(c.clone(), Arc::clone(&budget));
        let before = total_commands(&mut admin).await;
        let t0 = Instant::now();

        let scan = scan::scan_with_budget("p", &mut c, &opts, &perms, None, None, &budget);
        let pings = async {
            for i in 0..PINGS {
                assert!(handle.send(ReqId(i), ping()));
            }
            for _ in 0..PINGS {
                let (_, result) = next(&mut rx).await;
                assert_eq!(result, Ok(LiveReply::Server(live::server::Reply::Pong)));
            }
        };
        let (report, ()) = tokio::join!(scan, pings);
        let took = t0.elapsed();
        let report = report.unwrap();
        assert_eq!(report.sampled_keys, 600);

        // Every command the scan and the live task sent was taken from the budget: the
        // server counted exactly those, plus the INFO that read the count before.
        let after = total_commands(&mut admin).await;
        let taken = budget.taken();
        assert_eq!(after - before, taken + 1, "server and budget disagree");
        assert!(taken > 1_800 + PINGS, "{taken}");
        // Together they stayed under the limit.
        let min = Duration::from_secs_f64(taken as f64 / f64::from(RATE));
        assert!(took >= min, "{taken} commands in {took:?}, budget {min:?}");
    }
}

#[tokio::test]
async fn dropping_the_handle_closes_the_connection() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        let base = clients(&mut admin).await;
        let (handle, mut rx, close) = start(connect(&server).await, Budget::new(None));
        assert!(handle.send(ReqId(1), ping()));
        assert!(next(&mut rx).await.1.is_ok());
        assert_eq!(clients(&mut admin).await, base + 1);

        drop(handle);
        // Closed on purpose: no reason.
        let why = timeout(WAIT, close)
            .await
            .expect("on_close in time")
            .unwrap();
        assert_eq!(why, None);
        // The task held the last clone of the connection: the server sees it go.
        let deadline = Instant::now() + WAIT;
        while clients(&mut admin).await != base {
            assert!(Instant::now() < deadline, "the live connection stayed open");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test]
async fn a_lost_connection_ends_the_task_with_a_reason() {
    for server in [common::redis().await, common::valkey().await] {
        let mut admin = server.admin().await;
        let mut c = connect(&server).await;
        let live_id: i64 = redis::cmd("CLIENT")
            .arg("ID")
            .query_async(&mut c)
            .await
            .unwrap();
        let (handle, mut rx, close) = start(c, Budget::new(None));
        let _: () = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(live_id)
            .query_async(&mut admin)
            .await
            .unwrap();
        // The next request finds the connection gone.
        let deadline = Instant::now() + WAIT;
        let mut req = 0;
        loop {
            req += 1;
            if !handle.send(ReqId(req), ping()) {
                break;
            }
            match next(&mut rx).await.1 {
                Err(LiveError::Closed) => break,
                // The kill may not have reached the client yet.
                Ok(_) => {
                    assert!(Instant::now() < deadline, "still answering after the kill");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        let why = timeout(WAIT, close)
            .await
            .expect("on_close in time")
            .unwrap();
        assert_eq!(why.as_deref(), Some(live::CONNECTION_LOST));
        // And the task takes no more requests.
        assert!(!handle.send(ReqId(999), ping()));
    }
}

/// The `lens` user of tests/conn_it.rs: what a scan needs and nothing for the views.
const LENS_RULES: [&str; 21] = [
    "--user",
    "lens",
    "on",
    ">lens-pw",
    "~*",
    "-@all",
    "+ping",
    "+hello",
    "+client|setinfo",
    "+client|setname",
    "+select",
    "+info",
    "+dbsize",
    "+scan",
    "+type",
    "+ttl",
    "+pttl",
    "+memory|usage",
    "+object|freq",
    "+memory|stats",
    "+auth",
];

/// The view commands spec "Permission check, additions" adds to the minimal user.
const VIEW_RULES: [&str; 7] = [
    "+slowlog|get",
    "+slowlog|len",
    "+client|list",
    "+xinfo",
    "+pubsub|channels",
    "+pubsub|numsub",
    "+pubsub|numpat",
];

fn assert_labels_in_order(r: &PermissionReport) {
    let labels: Vec<&str> = r.checks.iter().map(|c| c.label).collect();
    let expect: Vec<&str> = PERM_LABELS
        .iter()
        .chain(&VIEW_PERM_LABELS)
        .copied()
        .collect();
    assert_eq!(labels, expect);
}

#[tokio::test]
async fn a_user_without_the_view_commands_still_connects() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &LENS_RULES).await;
        let mut c = connect_as_lens(&server).await;
        let r = conn::check_all_permissions(&mut c).await;
        assert_labels_in_order(&r);
        // The scan's own checks are unaffected.
        for label in ["SCAN", "TYPE · TTL", "MEMORY USAGE", "INFO", "MEMORY STATS"] {
            assert_eq!(status(&r, label), &PermStatus::Allowed, "{label}");
        }
        for (label, command) in [
            (PERM_SLOWLOG, "SLOWLOG LEN"),
            (PERM_CLIENT_LIST, "CLIENT LIST"),
            (PERM_XINFO, "XINFO"),
            (PERM_PUBSUB, "PUBSUB NUMPAT"),
        ] {
            assert_eq!(
                status(&r, label),
                &PermStatus::Denied(format!("NOPERM {command} is not allowed for this user")),
                "valkey {valkey}"
            );
        }
        // No module: unsupported, even for a user with no rule for it.
        assert_eq!(
            status(&r, PERM_SEARCH),
            &PermStatus::Skipped("the Search module is not loaded".into())
        );
        // No status holds the user name.
        let text = format!("{r:?}");
        assert!(
            !text.contains("lens\"") && !text.contains("User lens"),
            "{text}"
        );
        // And the connection still works.
        let pong: String = redis::cmd("PING").query_async(&mut c).await.unwrap();
        assert_eq!(pong, "PONG");
    }
}

#[tokio::test]
async fn the_view_rules_allow_the_view_probes() {
    for valkey in [false, true] {
        let args: Vec<&str> = LENS_RULES.iter().chain(&VIEW_RULES).copied().collect();
        let server = common::server_with(valkey, &args).await;
        let mut c = connect_as_lens(&server).await;
        let r = conn::check_all_permissions(&mut c).await;
        for label in [PERM_SLOWLOG, PERM_CLIENT_LIST, PERM_XINFO, PERM_PUBSUB] {
            assert_eq!(status(&r, label), &PermStatus::Allowed, "{label}");
        }
    }
}

#[tokio::test]
async fn search_without_the_module_is_unsupported_not_denied() {
    for server in [common::redis().await, common::valkey().await] {
        let mut c = connect(&server).await;
        let checks = conn::check_view_permissions(&mut c).await;
        let labels: Vec<&str> = checks.iter().map(|c| c.label).collect();
        assert_eq!(labels, VIEW_PERM_LABELS);
        let r = PermissionReport { checks };
        assert_eq!(
            status(&r, PERM_SEARCH),
            &PermStatus::Skipped("the Search module is not loaded".into())
        );
        for label in [PERM_SLOWLOG, PERM_CLIENT_LIST, PERM_XINFO, PERM_PUBSUB] {
            assert_eq!(status(&r, label), &PermStatus::Allowed, "{label}");
        }
        // The CLIENT LIST probe returns no other client's line.
        let mut admin = server.admin().await;
        let line: String = redis::cmd("CLIENT")
            .arg("LIST")
            .arg("ID")
            .arg(i64::MAX)
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(line.trim(), "");
    }
}
