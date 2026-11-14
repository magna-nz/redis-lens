//! The TUI's connect and analyze runtime against real Redis and Valkey servers: a
//! connect reads no key and reads the slow log and the client list once, an ACL user
//! refused both still connects with reasons, a cluster connect reads no key on any
//! node, a Quick analysis samples at most 10,000 keys and reads the figures once, and
//! `esc` (the stop flag) keeps what an analysis has sampled.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::cluster::{cluster, parse_commandstats};
use redis::aio::MultiplexedConnection;
use redis_lens::conn;
use redis_lens::model::{Auth, Credentials, Endpoint, Env, PermStatus, Profile};
use redis_lens::scan::StopFlag;
use redis_lens::secrets::MemoryStore;
use redis_lens::tui::app::{App, Effect, Msg};
use redis_lens::tui::run_connect_for_tests;
use secrecy::SecretString;
use tokio::sync::mpsc;

/// More keys than a Quick analysis samples.
const KEYS: u64 = 25_000;
const QUICK: u64 = 10_000;

/// Commands that read a key. On connect, each may be sent at most as often as the
/// permission check probes it (it sends `SCAN 0 COUNT 10`, `TYPE`, `PTTL`, `MEMORY
/// USAGE` and `OBJECT FREQ` once each, on a key that need not exist), and `KEYS` and
/// `GET` never.
const KEY_COMMANDS: [&str; 10] = [
    "scan",
    "keys",
    "type",
    "pttl",
    "ttl",
    "memory|usage",
    "object|freq",
    "object",
    "get",
    "exists",
];

/// Server-level commands a connect may send beyond the permission check: the figures
/// (`INFO`, `DBSIZE`, `MEMORY STATS`), the slow log and the client list, a connection's
/// handshake, and this test's own `CONFIG RESETSTAT` and `INFO commandstats`.
const SERVER_LEVEL: [&str; 17] = [
    "info",
    "dbsize",
    "memory|stats",
    "memory",
    "slowlog|get",
    "slowlog",
    "client|list",
    "client|setinfo",
    "client|setname",
    "client",
    "config|resetstat",
    "config|get",
    "config",
    "hello",
    "ping",
    "select",
    "auth",
];

fn profile(port: u16, env: Env) -> Profile {
    Profile {
        name: "tui-connect".to_string(),
        env,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        tags: Vec::new(),
    }
}

/// `n` strings in three patterns, in pipelined batches.
async fn seed(admin: &mut MultiplexedConnection, n: u64) {
    for start in (0..n).step_by(5_000) {
        let mut pipe = redis::pipe();
        for i in start..(start + 5_000).min(n) {
            let key = match i % 3 {
                0 => format!("user:{i}"),
                1 => format!("session:{i}:data"),
                _ => format!("cart:{i}"),
            };
            pipe.cmd("SET").arg(&key).arg("v").ignore();
        }
        let () = pipe.query_async(admin).await.unwrap();
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

fn calls(stats: &HashMap<String, (u64, u64)>, name: &str) -> u64 {
    stats.get(name).map_or(0, |s| s.0)
}

/// Calls of a command's subcommand as Redis 7+ and Valkey count it (`slowlog|get`)
/// plus the whole command as Redis 6.2 counts every subcommand (`slowlog`).
fn calls_of(stats: &HashMap<String, (u64, u64)>, command: &str, sub: &str) -> u64 {
    calls(stats, &format!("{command}|{sub}")) + calls(stats, command)
}

/// Runs one connect (an analysis of `sample` keys with `Some`) to its end and returns
/// every message it sent.
async fn connect(profile: Profile, sample: Option<u64>) -> Vec<Msg> {
    connect_with(profile, None, sample).await
}

/// [`connect`] with credentials (an ACL user).
async fn connect_with(
    profile: Profile,
    credentials: Option<Credentials>,
    sample: Option<u64>,
) -> Vec<Msg> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(60),
        run_connect_for_tests(profile, credentials, sample, StopFlag::new(), tx),
    )
    .await
    .expect("the connect ends in time");
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        out.push(m);
    }
    out
}

fn no_failure(msgs: &[Msg], what: &str) {
    for m in msgs {
        assert!(
            !matches!(m, Msg::ConnectFailed { .. } | Msg::ScanFailed { .. }),
            "{what}: {m:?}"
        );
    }
}

/// What one connection's handshake and the permission check send, as a connect opens
/// one connection and checks it. The handshake counts: Redis 6.2 lists its `CLIENT
/// SETINFO` under `client`, with `CLIENT LIST`.
async fn probes(
    server: &common::TestServer,
    admin: &mut MultiplexedConnection,
) -> HashMap<String, (u64, u64)> {
    reset_stats(admin).await;
    let mut c = conn::connect(&profile(server.port, Env::Local), &MemoryStore::default())
        .await
        .expect("connect")
        .into_inner();
    let _ = conn::check_all_permissions(&mut c).await;
    commandstats(admin).await
}

#[tokio::test]
async fn a_connect_reads_no_key() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        seed(&mut admin, KEYS).await;
        let baseline = probes(&server, &mut admin).await;

        reset_stats(&mut admin).await;
        let msgs = connect(profile(server.port, Env::Local), None).await;
        let stats = commandstats(&mut admin).await;
        no_failure(&msgs, what);

        // No key read beyond the permission check's own probes.
        assert_eq!(calls(&stats, "keys"), 0, "{what}: {stats:?}");
        assert_eq!(calls(&stats, "get"), 0, "{what}: {stats:?}");
        for k in KEY_COMMANDS {
            assert!(
                calls(&stats, k) <= calls(&baseline, k),
                "{what}: {k} sent {} times, the permission check sends it {} times: {stats:?}",
                calls(&stats, k),
                calls(&baseline, k)
            );
        }
        // Everything else is the permission check's or a server-level command.
        for name in stats.keys() {
            assert!(
                baseline.contains_key(name) || SERVER_LEVEL.contains(&name.as_str()),
                "{what}: {name} was sent: {stats:?}"
            );
        }
        // The figures: one DBSIZE. The slow log and the client list: read once each,
        // beyond the permission check's own `SLOWLOG LEN` and `CLIENT LIST ID` (Redis
        // 6.2 counts every subcommand under the command's name).
        assert_eq!(calls(&stats, "dbsize"), 1, "{what}: {stats:?}");
        assert_eq!(
            calls_of(&stats, "slowlog", "get"),
            calls_of(&baseline, "slowlog", "get") + 1,
            "{what}: SLOWLOG GET once: {stats:?}"
        );
        assert_eq!(
            calls_of(&stats, "client", "list"),
            calls_of(&baseline, "client", "list") + 1,
            "{what}: CLIENT LIST once: {stats:?}"
        );

        // The messages: the figures with the keyspace, the two reads, no scan.
        let figures = msgs
            .iter()
            .find_map(|m| match m {
                Msg::Figures { figures, .. } => Some(figures),
                _ => None,
            })
            .expect("figures sent");
        assert_eq!(figures.failed, None, "{what}");
        let f = figures.server.as_ref().expect("standalone figures");
        assert_eq!(f.server.total_keys, KEYS, "{what}");
        assert_eq!(f.headline().keys, KEYS, "{what}");
        let reads = msgs
            .iter()
            .find_map(|m| match m {
                Msg::ServerReads { slow, clients, .. } => Some((slow, clients)),
                _ => None,
            })
            .expect("the slow log and the client list read");
        assert!(matches!(reads.0, Some(Ok(_))), "{what}: {:?}", reads.0);
        assert!(matches!(reads.1, Some(Ok(_))), "{what}: {:?}", reads.1);
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, Msg::ScanProgress { .. } | Msg::ScanDone { .. })),
            "{what}: a connect does not analyze"
        );
    }
}

#[tokio::test]
async fn a_quick_analysis_samples_at_most_10000_keys() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        seed(&mut admin, KEYS).await;
        reset_stats(&mut admin).await;
        let msgs = connect(profile(server.port, Env::Local), Some(QUICK)).await;
        let stats = commandstats(&mut admin).await;
        no_failure(&msgs, what);
        let report = msgs
            .iter()
            .find_map(|m| match m {
                Msg::ScanDone { report, .. } => Some(report),
                _ => None,
            })
            .expect("a report");
        assert!(
            report.sampled_keys <= QUICK,
            "{what}: {}",
            report.sampled_keys
        );
        assert!(
            report.sampled_keys >= QUICK / 2,
            "{what}: {}",
            report.sampled_keys
        );
        assert_eq!(report.server.total_keys, KEYS, "{what}");
        // The server agrees: one TYPE per sampled key, plus the permission check's.
        assert!(calls(&stats, "type") <= QUICK + 1, "{what}: {stats:?}");
        // The figures are read once: the analysis starts from the connect's.
        assert_eq!(calls(&stats, "dbsize"), 1, "{what}: {stats:?}");
        assert!(
            msgs.iter().any(|m| matches!(m, Msg::Figures { .. })),
            "{what}: the analysis's connect shows its figures"
        );
        let target = msgs
            .iter()
            .filter_map(|m| match m {
                Msg::ScanProgress { progress, .. } => Some(progress.target),
                _ => None,
            })
            .max();
        assert_eq!(target, Some(QUICK), "{what}");
    }
}

#[tokio::test]
async fn a_stopped_analysis_returns_what_it_sampled() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        seed(&mut admin, KEYS).await;
        // A prod profile: 5,000 commands a second, so 10,000 keys take several seconds.
        let p = profile(server.port, Env::Prod);
        let stop = StopFlag::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(run_connect_for_tests(
            p,
            None,
            Some(QUICK),
            stop.clone(),
            tx,
        ));
        let mut msgs = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(m) = rx.recv().await {
                let enough =
                    matches!(&m, Msg::ScanProgress { progress, .. } if progress.sampled >= 300);
                msgs.push(m);
                if enough {
                    break;
                }
            }
        })
        .await
        .expect("300 keys sampled in time");
        stop.stop();
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("the analysis stops in time")
            .unwrap();
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        no_failure(&msgs, what);
        let report = msgs
            .iter()
            .find_map(|m| match m {
                Msg::ScanDone { report, .. } => Some(report),
                _ => None,
            })
            .expect("a report, not an error");
        assert!(!report.complete, "{what}");
        assert!(
            report.sampled_keys >= 300,
            "{what}: {}",
            report.sampled_keys
        );
        assert!(
            report.sampled_keys < QUICK,
            "{what}: {}",
            report.sampled_keys
        );
        // Scaled from what it sampled: about the whole keyspace.
        let est: u64 = report.patterns.iter().map(|p| p.est_keys).sum();
        assert!(est.abs_diff(KEYS) <= KEYS / 100, "{what}: est {est}");
    }
}

/// An ACL user refused `SLOWLOG` and `CLIENT` (whole commands: Redis 6.2 has no ACL
/// rules for one subcommand): the connect sends neither, still connects and reads the
/// figures, and Overview says why the slow log and the idle clients are off.
#[tokio::test]
async fn a_user_refused_slowlog_and_client_list_still_connects_with_reasons() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = common::server_with(
            valkey,
            &[
                "--user", "lens", "on", ">lens-pw", "~*", "+@all", "-slowlog", "-client",
            ],
        )
        .await;
        let mut admin = server.admin().await;
        seed(&mut admin, 300).await;
        let mut p = profile(server.port, Env::Local);
        p.auth = Auth::Keychain;
        let lens = Credentials {
            username: Some("lens".into()),
            password: SecretString::from("lens-pw"),
        };
        reset_stats(&mut admin).await;
        let msgs = connect_with(p.clone(), Some(lens), None).await;
        let stats = commandstats(&mut admin).await;
        no_failure(&msgs, what);

        // Neither is sent beyond the probes the server refused (not counted as calls).
        assert_eq!(calls_of(&stats, "slowlog", "get"), 0, "{what}: {stats:?}");
        assert_eq!(calls_of(&stats, "client", "list"), 0, "{what}: {stats:?}");
        let perms = msgs
            .iter()
            .find_map(|m| match m {
                Msg::Connected { perms, .. } => Some(perms.clone()),
                _ => None,
            })
            .expect("connected");
        for label in [conn::PERM_SLOWLOG, conn::PERM_CLIENT_LIST] {
            let status = perms
                .checks
                .iter()
                .find(|c| c.label == label)
                .map(|c| &c.status);
            assert!(
                matches!(status, Some(PermStatus::Denied(_))),
                "{what}: {label}: {status:?}"
            );
        }
        let reads = msgs
            .iter()
            .find_map(|m| match m {
                Msg::ServerReads { slow, clients, .. } => Some((slow.is_none(), clients.is_none())),
                _ => None,
            })
            .expect("the reads message");
        assert_eq!(reads, (true, true), "{what}: neither was sent");
        let figures = msgs
            .iter()
            .find_map(|m| match m {
                Msg::Figures { figures, .. } => Some(figures),
                _ => None,
            })
            .expect("figures");
        assert_eq!(figures.failed, None, "{what}");
        assert_eq!(figures.server.as_ref().unwrap().server.total_keys, 300);

        // What Overview says, from these messages: each part off with its reason, in
        // redis-lens's words, never naming the user.
        let mut app = App::new(vec![p]);
        let effects = app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(
            matches!(effects[..], [Effect::Connect { id: 1, scan: 1, .. }]),
            "{effects:?}"
        );
        for m in msgs {
            app.apply(m);
        }
        let off = redis_lens::tui::app::views::overview::off_reasons(&app);
        assert!(
            off.iter().any(|r| r.starts_with("Slow log is off")),
            "{what}: {off:?}"
        );
        assert!(
            off.iter().any(|r| r.starts_with("Idle clients are off")),
            "{what}: {off:?}"
        );
        for r in &off {
            assert!(!r.contains("lens-pw"), "{r}");
            assert!(!r.contains("lens ") || r.contains("redis-lens"), "{r}");
        }
    }
}

/// A TUI connect to a cluster: every node's figures, no key read on any node, the slow
/// log and the client list read once on the first primary only; then a Quick analysis
/// reads each node's figures once.
#[tokio::test]
async fn a_cluster_connect_reads_no_key_and_the_first_primarys_reads_once() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let c = cluster(valkey, false).await;
        let keys: Vec<(String, String)> = (0..900)
            .map(|i| (format!("user:{i}"), "x".to_string()))
            .collect();
        c.seed(&keys).await;
        c.reset_stats().await;
        let msgs = connect(profile(c.seed_port(), Env::Local), None).await;
        no_failure(&msgs, what);

        let nodes = msgs
            .iter()
            .find_map(|m| match m {
                Msg::Nodes { nodes, .. } => Some(nodes.clone()),
                _ => None,
            })
            .expect("the node list");
        assert_eq!(nodes.len(), 3, "{what}");
        let figures = msgs
            .iter()
            .find_map(|m| match m {
                Msg::Figures { figures, .. } => Some(figures.clone()),
                _ => None,
            })
            .expect("figures");
        assert_eq!(figures.nodes.len(), 3, "{what}");
        assert!(figures.nodes.iter().all(|n| n.figures.is_some()), "{what}");
        assert_eq!(figures.server.as_ref().unwrap().server.total_keys, 900);
        let (node, slow_ok, clients_ok) = msgs
            .iter()
            .find_map(|m| match m {
                Msg::ServerReads {
                    node,
                    slow,
                    clients,
                    ..
                } => Some((
                    *node,
                    matches!(slow, Some(Ok(_))),
                    matches!(clients, Some(Ok(_))),
                )),
                _ => None,
            })
            .expect("the reads");
        assert!(slow_ok && clients_ok, "{what}");
        let first_primary = nodes
            .iter()
            .position(|n| n.role == redis_lens::model::NodeRole::Primary)
            .unwrap();
        assert_eq!(node, first_primary, "{what}");
        let read_port = nodes[node].endpoint.port;
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, Msg::ScanProgress { .. } | Msg::ScanDone { .. })),
            "{what}: a connect does not analyze"
        );

        let mut per_node = HashMap::new();
        for p in &c.ports {
            per_node.insert(*p, c.commandstats(*p).await);
        }
        let other = *c.ports.iter().find(|p| **p != read_port).unwrap();
        for (port, stats) in &per_node {
            // The permission check probes each key command once at most; no KEYS, no GET.
            assert_eq!(calls(stats, "keys"), 0, "{what} {port}: {stats:?}");
            assert_eq!(calls(stats, "get"), 0, "{what} {port}: {stats:?}");
            for k in KEY_COMMANDS {
                assert!(calls(stats, k) <= 1, "{what} {port}: {k}: {stats:?}");
            }
            assert_eq!(calls(stats, "dbsize"), 1, "{what} {port}: {stats:?}");
            // Every node gets the same probes; only the first primary is read once more.
            let more = u64::from(*port == read_port);
            for (cmd, sub) in [("slowlog", "get"), ("client", "list")] {
                assert_eq!(
                    calls_of(stats, cmd, sub),
                    calls_of(&per_node[&other], cmd, sub) + more,
                    "{what} {port}: {cmd} {sub}: {stats:?}"
                );
            }
        }

        // A Quick analysis: each node's figures once (the analysis starts from them).
        c.reset_stats().await;
        let msgs = connect(profile(c.seed_port(), Env::Local), Some(QUICK)).await;
        no_failure(&msgs, what);
        let report = msgs
            .iter()
            .find_map(|m| match m {
                Msg::ScanDone { report, .. } => Some(report),
                _ => None,
            })
            .expect("a report");
        assert_eq!(report.sampled_keys, 900, "{what}");
        for p in &c.ports {
            let stats = c.commandstats(*p).await;
            assert_eq!(calls(&stats, "dbsize"), 1, "{what} {p}: {stats:?}");
        }
    }
}
