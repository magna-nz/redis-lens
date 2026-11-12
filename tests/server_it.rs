//! The Server view's requests (Overview, Clients, Slow log), against real Redis and
//! Valkey servers, with a full user and with limited ACL users.
#![cfg(feature = "integration-tests")]

mod common;

use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::live::server::{
    ClientConn, CommandKeys, InfoSample, Reply, Request, Role, SlowLogReply, parse_client_list, run,
};
use redis_lens::live::{LiveError, LiveReply, LiveRequest, ReqId};
use redis_lens::tui::app::views::server::{GroupKey, build_groups, point};

async fn connect_as(server: &common::TestServer, user: &str, pw: &str) -> MultiplexedConnection {
    server.admin_as(user, pw).await
}

/// Both server kinds.
async fn servers() -> Vec<(&'static str, common::TestServer)> {
    vec![
        ("redis", common::redis().await),
        ("valkey", common::valkey().await),
    ]
}

async fn info(conn: &mut MultiplexedConnection, budget: &Budget) -> (Instant, InfoSample) {
    match run(conn, budget, Request::Info).await.expect("INFO") {
        Reply::Info { at, sample } => (at, *sample),
        other => panic!("not an INFO reply: {other:?}"),
    }
}

async fn clients(
    conn: &mut MultiplexedConnection,
    budget: &Budget,
) -> Result<Vec<ClientConn>, LiveError> {
    match run(conn, budget, Request::Clients).await? {
        Reply::Clients(list) => Ok(list),
        other => panic!("not a CLIENT LIST reply: {other:?}"),
    }
}

async fn slowlog(
    conn: &mut MultiplexedConnection,
    budget: &Budget,
    specs: std::collections::HashMap<String, CommandKeys>,
    config: bool,
    keys: bool,
) -> Result<SlowLogReply, LiveError> {
    let req = Request::SlowLog {
        specs,
        config,
        keys,
    };
    match run(conn, budget, req).await? {
        Reply::SlowLog(r) => Ok(*r),
        other => panic!("not a SLOWLOG reply: {other:?}"),
    }
}

async fn cmd(conn: &mut MultiplexedConnection, args: &[&str]) {
    let mut c = redis::cmd(args[0]);
    for a in &args[1..] {
        c.arg(*a);
    }
    let _: redis::Value = c.query_async(conn).await.expect("command");
}

/// ACL arguments for a user `lens` (password `lens-pw`) who can run only `allowed`.
fn lens(allowed: &[&'static str]) -> Vec<&'static str> {
    let mut v = vec![
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
    ];
    v.extend_from_slice(allowed);
    v
}

/// The major version the server reports (Valkey reports 7, as a Redis 7.2 stand-in).
async fn major_version(conn: &mut MultiplexedConnection) -> u64 {
    let text: String = redis::cmd("INFO")
        .arg("server")
        .query_async(conn)
        .await
        .unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix("redis_version:"))
        .and_then(|v| v.split('.').next()?.trim().parse().ok())
        .expect("redis_version")
}

fn assert_no_secret(text: &str) {
    for secret in ["lens-pw", "lens", "default"] {
        assert!(!text.contains(secret), "{secret:?} in {text:?}");
    }
}

#[tokio::test]
async fn info_is_read_and_rates_come_from_two_polls() {
    for (kind, server) in servers().await {
        let mut admin = server.admin().await;
        let budget = Budget::new(None);
        let (t0, a) = info(&mut admin, &budget).await;
        assert_eq!(budget.taken(), 1, "{kind}: one command per INFO");
        assert_eq!(a.role, Some(Role::Primary), "{kind}");
        assert!(a.uptime_secs.is_some(), "{kind}");
        assert!(a.clients.unwrap() >= 1, "{kind}");
        assert!(a.used_memory.unwrap() > 0, "{kind}");
        assert_eq!(a.maxmemory, Some(0), "{kind}: no limit");
        assert_eq!(a.replicas, Some(0), "{kind}");
        assert!(a.policy.is_some(), "{kind}");
        assert!(
            a.commands.is_some() && a.net_in.is_some() && a.net_out.is_some(),
            "{kind}"
        );
        assert!(a.cpu_user.is_some() && a.cpu_sys.is_some(), "{kind}");
        assert!(a.hits.is_some() && a.misses.is_some(), "{kind}");
        assert!(a.evicted.is_some() && a.expired.is_some(), "{kind}");
        assert!(a.rdb_ok.is_some() && a.aof_on == Some(false), "{kind}");

        // Some traffic: 200 writes, 30 hits, 20 misses.
        let mut pipe = redis::pipe();
        for i in 0..200 {
            pipe.cmd("SET").arg(format!("k{i}")).arg("v").ignore();
        }
        for i in 0..30 {
            pipe.cmd("GET").arg(format!("k{i}")).ignore();
        }
        for i in 0..20 {
            pipe.cmd("GET").arg(format!("absent{i}")).ignore();
        }
        let _: () = pipe.query_async(&mut admin).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (t1, b) = info(&mut admin, &budget).await;
        assert!(t1 > t0);

        let p = point(Some((t0, &a)), t1, &b, Duration::from_secs(30));
        assert!(p.ops.unwrap() > 0.0, "{kind}: {p:?}");
        assert!(
            p.net_in.unwrap() > 0.0 && p.net_out.unwrap() > 0.0,
            "{kind}: {p:?}"
        );
        assert!(p.cpu_user.is_some() && p.cpu_sys.is_some(), "{kind}: {p:?}");
        let hit = p.hit_rate.expect("a hit rate");
        assert!((hit - 0.6).abs() < 1e-9, "{kind}: {hit}");
        assert_eq!(p.evicted, Some(0.0), "{kind}");
    }
}

#[tokio::test]
async fn info_shows_a_memory_limit_and_a_policy() {
    for valkey in [false, true] {
        let args = ["--maxmemory", "64mb", "--maxmemory-policy", "allkeys-lfu"];
        let server = common::server_with(valkey, &args).await;
        let mut admin = server.admin().await;
        let (_, s) = info(&mut admin, &Budget::new(None)).await;
        assert_eq!(s.maxmemory, Some(64 * 1024 * 1024));
        assert_eq!(s.policy.as_deref(), Some("allkeys-lfu"));
    }
}

#[tokio::test]
async fn info_reads_a_replica_as_a_replica() {
    for valkey in [false, true] {
        let primary = if valkey {
            common::valkey().await
        } else {
            common::redis().await
        };
        let replica = common::replica_of(&primary, valkey, None).await;
        let mut conn = replica.admin().await;
        let mut tries = 0;
        loop {
            let (_, s) = info(&mut conn, &Budget::new(None)).await;
            if s.link_up == Some(true) {
                assert_eq!(s.role, Some(Role::Replica));
                break;
            }
            tries += 1;
            assert!(tries < 50, "the replica never linked up");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let (_, s) = info(&mut primary.admin().await, &Budget::new(None)).await;
        assert_eq!(s.replicas, Some(1));
    }
}

#[tokio::test]
async fn client_list_groups_by_library_name_and_network() {
    for (kind, server) in servers().await {
        let budget = Budget::new(None);
        let mut admin = server.admin().await;
        // Two named clients and one that says which library it is.
        let _a = named(&server, "billing").await;
        let _b = named(&server, "billing").await;
        let mut c = named(&server, "worker").await;
        let mut d = server.admin().await;
        let lib: Result<redis::Value, _> = redis::cmd("CLIENT")
            .arg("SETINFO")
            .arg("LIB-NAME")
            .arg("lens-test-lib")
            .query_async(&mut d)
            .await;
        let has_lib = lib.is_ok();
        if has_lib {
            cmd(&mut d, &["CLIENT", "SETINFO", "LIB-VER", "9.9"]).await;
        }
        cmd(&mut c, &["GET", "x"]).await;

        let list = clients(&mut admin, &budget).await.expect("CLIENT LIST");
        assert_eq!(budget.taken(), 1, "{kind}");
        assert!(list.len() >= 4, "{kind}: {list:?}");
        assert_eq!(
            list.iter().filter(|c| c.name == "billing").count(),
            2,
            "{kind}"
        );
        let worker = list.iter().find(|c| c.name == "worker").unwrap();
        assert_eq!(worker.cmd, "get", "{kind}");
        assert!(
            worker.addr.starts_with("127.0.0.1:") || worker.addr.contains(':'),
            "{kind}"
        );
        assert!(worker.mem > 0, "{kind}: memory is {}", worker.mem);
        assert!(worker.idle <= 5 && worker.age <= 30, "{kind}");

        let groups = build_groups(&list);
        let billing = groups
            .iter()
            .find(|g| g.key == GroupKey::Lib("billing".into()))
            .unwrap_or_else(|| panic!("{kind}: no billing group in {groups:?}"));
        assert_eq!(billing.connections(), 2, "{kind}");
        if has_lib {
            let lib = groups
                .iter()
                .find(|g| g.key == GroupKey::Lib("lens-test-lib 9.9".into()))
                .unwrap_or_else(|| panic!("{kind}: no lib group in {groups:?}"));
            assert_eq!(lib.connections(), 1, "{kind}");
        }
        // Every client connects from the docker host's address: one /24 holds them all.
        let nets: Vec<_> = groups.iter().filter(|g| g.key.kind() == "addr").collect();
        assert_eq!(nets.len(), 1, "{kind}: {nets:?}");
        assert_eq!(nets[0].connections(), list.len(), "{kind}");
        assert!(
            !nets[0].flagged(),
            "{kind}: nobody has been idle for an hour"
        );
    }
}

#[tokio::test]
async fn client_list_never_carries_the_user() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens(&["+client|list", "+client|setname"])).await;
        let mut conn = connect_as(&server, "lens", "lens-pw").await;
        let list = clients(&mut conn, &Budget::new(None))
            .await
            .expect("CLIENT LIST");
        assert!(!list.is_empty());
        assert_no_secret(&format!("{list:?}"));
        // The raw text does have it; the parser is what drops it.
        let raw: String = redis::cmd("CLIENT")
            .arg("LIST")
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(raw.contains("user=lens"));
        assert_no_secret(&format!("{:?}", parse_client_list(&raw)));
    }
}

/// A connection named `name` that says nothing about its library (the client library
/// announces itself on connect; an empty `LIB-NAME` takes that back where the server
/// has the command).
async fn named(server: &common::TestServer, name: &str) -> MultiplexedConnection {
    let mut conn = server.admin().await;
    cmd(&mut conn, &["CLIENT", "SETNAME", name]).await;
    for what in ["LIB-NAME", "LIB-VER"] {
        let _: Result<redis::Value, _> = redis::cmd("CLIENT")
            .arg("SETINFO")
            .arg(what)
            .arg("")
            .query_async(&mut conn)
            .await;
    }
    conn
}

/// Sets the slow log to record everything, runs `cmds`, and gives back the log's entries.
async fn log_everything(admin: &mut MultiplexedConnection) {
    cmd(admin, &["CONFIG", "SET", "slowlog-log-slower-than", "0"]).await;
    cmd(admin, &["SLOWLOG", "RESET"]).await;
}

#[tokio::test]
async fn slow_log_has_key_names_but_never_arguments() {
    for (kind, server) in servers().await {
        let mut admin = server.admin().await;
        log_everything(&mut admin).await;
        cmd(&mut admin, &["SET", "session:42", "hunter2-top-secret"]).await;
        cmd(&mut admin, &["MGET", "ma", "mb", "mc"]).await;
        cmd(&mut admin, &["HSET", "cart:7", "item", "shoes-size-9"]).await;
        cmd(&mut admin, &["HGETALL", "cart:7"]).await;
        cmd(
            &mut admin,
            &["EVAL", "return 1", "2", "ek1", "ek2", "argv-secret"],
        )
        .await;
        cmd(&mut admin, &["XADD", "orders", "*", "f", "stream-secret"]).await;
        cmd(&mut admin, &["XINFO", "STREAM", "orders"]).await;
        cmd(&mut admin, &["LPUSH", "q", "a"]).await;
        cmd(&mut admin, &["LRANGE", "q", "0", "-1"]).await;
        cmd(&mut admin, &["KEYS", "*"]).await;
        cmd(&mut admin, &["ZADD", "z", "1", "m"]).await;
        cmd(&mut admin, &["ZUNIONSTORE", "zd", "2", "z", "z2"]).await;

        // Before 7.0 there are no key specs: only the legacy first/last/step triple, so
        // commands whose keys move (EVAL, ZUNIONSTORE) show the keys it can place, and
        // XINFO is not a container.
        let specs = major_version(&mut admin).await >= 7;
        let budget = Budget::new(None);
        let r = slowlog(&mut admin, &budget, Default::default(), true, true)
            .await
            .expect("SLOWLOG");
        assert!(r.keys_off.is_none(), "{kind}: {:?}", r.keys_off);
        let by = |name: &str| {
            r.entries
                .iter()
                .find(|e| e.command == name)
                .unwrap_or_else(|| panic!("{kind}: no {name} in {:?}", r.entries))
        };
        assert_eq!(by("SET").keys, ["session:42"], "{kind}");
        assert_eq!(by("MGET").keys, ["ma", "mb", "mc"], "{kind}");
        assert_eq!(by("HGETALL").keys, ["cart:7"], "{kind}");
        assert!(by("HGETALL").whole_key_read, "{kind}");
        if specs {
            assert_eq!(by("EVAL").keys, ["ek1", "ek2"], "{kind}");
            assert_eq!(by("XINFO STREAM").keys, ["orders"], "{kind}");
        } else {
            assert_eq!(by("XINFO").keys, ["orders"], "{kind}");
        }
        assert_eq!(by("XADD").keys, ["orders"], "{kind}");
        assert_eq!(by("LRANGE").keys, ["q"], "{kind}");
        assert!(by("LRANGE").whole_key_read, "{kind}");
        assert!(
            by("KEYS").keys.is_empty(),
            "{kind}: KEYS has a pattern, not a key"
        );
        let zunion = by("ZUNIONSTORE").keys.clone();
        if specs {
            assert_eq!(zunion, ["zd", "z", "z2"], "{kind}");
        } else {
            assert_eq!(zunion, ["zd"], "{kind}");
        }
        assert!(!by("SET").whole_key_read);
        assert!(by("SET").client.starts_with("127.0.0.1:") || by("SET").client.contains(':'));
        assert!(by("SET").unix > 1_600_000_000, "{kind}");
        // Newest first.
        assert!(r.entries.windows(2).all(|w| w[0].id > w[1].id), "{kind}");

        // Nothing the commands carried but their keys reaches the reply.
        let shown = format!("{r:?}");
        for value in [
            "hunter2",
            "top-secret",
            "shoes",
            "argv-secret",
            "stream-secret",
        ] {
            assert!(!shown.contains(value), "{kind}: {value} in {shown}");
        }

        // The threshold and length.
        let cfg = r.config.expect("asked").expect("allowed");
        assert_eq!((cfg.threshold_micros, cfg.max_len), (0, 128), "{kind}");

        // The commands learned come back, containers included.
        let learned: &[&str] = if specs {
            &[
                "set",
                "mget",
                "eval",
                "xinfo",
                "xinfo|stream",
                "zunionstore",
                "keys",
            ]
        } else {
            &["set", "mget", "eval", "xinfo", "zunionstore", "keys"]
        };
        for name in learned.iter().copied() {
            assert!(
                r.learned.contains_key(name),
                "{kind}: {name} not in {:?}",
                r.learned.keys()
            );
        }
        if specs {
            assert_eq!(r.learned["xinfo"], CommandKeys::Container, "{kind}");
        }
        // Commands: SLOWLOG GET, CONFIG GET, and one or two COMMAND INFO.
        assert!(
            (3..=4).contains(&budget.taken()),
            "{kind}: {}",
            budget.taken()
        );

        // With the threshold at 0 the first poll's own SLOWLOG GET and CONFIG GET are in
        // the log now, so the second poll learns those. Once everything is known, a poll
        // costs SLOWLOG GET alone.
        let mut specs = r.learned.clone();
        let second = slowlog(&mut admin, &Budget::new(None), specs.clone(), false, true)
            .await
            .expect("SLOWLOG again");
        assert!(
            second.learned.contains_key("command"),
            "{kind}: {:?}",
            second.learned.keys()
        );
        specs.extend(second.learned);
        let budget = Budget::new(None);
        let again = slowlog(&mut admin, &budget, specs, false, true)
            .await
            .expect("SLOWLOG a third time");
        assert_eq!(budget.taken(), 1, "{kind}");
        assert!(again.learned.is_empty() && again.config.is_none(), "{kind}");
        let e = again.entries.iter().find(|e| e.command == "MGET").unwrap();
        assert_eq!(e.keys, ["ma", "mb", "mc"], "{kind}");
    }
}

#[tokio::test]
async fn slow_log_cuts_long_commands_without_taking_the_marker_for_a_key() {
    for (_, server) in servers().await {
        let mut admin = server.admin().await;
        log_everything(&mut admin).await;
        let names: Vec<String> = (0..60).map(|i| format!("del{i}")).collect();
        let mut c = redis::cmd("DEL");
        for n in &names {
            c.arg(n.as_str());
        }
        let _: i64 = c.query_async(&mut admin).await.unwrap();
        let r = slowlog(
            &mut admin,
            &Budget::new(None),
            Default::default(),
            false,
            true,
        )
        .await
        .unwrap();
        let del = r.entries.iter().find(|e| e.command == "DEL").unwrap();
        assert_eq!(del.keys.len(), 8);
        assert_eq!(del.keys[0], "del0");
        assert!(del.more_keys > 0 && del.more_keys <= 60 - 8);
        assert!(!del.keys.iter().any(|k| k.contains("more arguments")));
    }
}

// -- limited users ------------------------------------------------------------

#[tokio::test]
async fn a_user_without_info_gets_a_denied_error_in_our_words() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens(&["+client|list", "+slowlog|get"])).await;
        let mut conn = connect_as(&server, "lens", "lens-pw").await;
        let e = run(&mut conn, &Budget::new(None), Request::Info)
            .await
            .unwrap_err();
        assert_eq!(e, LiveError::Denied("INFO".into()), "valkey={valkey}");
        assert_no_secret(&e.to_string());
        // The connection is fine; the other panels' requests work.
        assert!(clients(&mut conn, &Budget::new(None)).await.is_ok());
    }
}

#[tokio::test]
async fn a_user_without_client_list_turns_off_only_that_panel() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens(&["+info", "+slowlog|get"])).await;
        let mut conn = connect_as(&server, "lens", "lens-pw").await;
        let budget = Budget::new(None);
        let e = clients(&mut conn, &budget).await.unwrap_err();
        assert_eq!(
            e,
            LiveError::Denied("CLIENT LIST".into()),
            "valkey={valkey}"
        );
        assert_no_secret(&e.to_string());
        assert_eq!(budget.taken(), 1, "a refused command still counted");
        let (_, s) = info(&mut conn, &budget).await;
        assert!(s.uptime_secs.is_some());
    }
}

#[tokio::test]
async fn a_user_without_slowlog_get_is_denied() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens(&["+info"])).await;
        let mut conn = connect_as(&server, "lens", "lens-pw").await;
        let e = slowlog(
            &mut conn,
            &Budget::new(None),
            Default::default(),
            true,
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(
            e,
            LiveError::Denied("SLOWLOG GET".into()),
            "valkey={valkey}"
        );
        assert_no_secret(&e.to_string());
    }
}

#[tokio::test]
async fn a_user_without_config_get_still_sees_the_slow_log() {
    for valkey in [false, true] {
        let server = common::server_with(
            valkey,
            &lens(&["+slowlog|get", "+command|info", "+get", "+set"]),
        )
        .await;
        let mut admin = server.admin().await;
        log_everything(&mut admin).await;
        cmd(&mut admin, &["SET", "k1", "v"]).await;
        let mut conn = connect_as(&server, "lens", "lens-pw").await;
        let r = slowlog(
            &mut conn,
            &Budget::new(None),
            Default::default(),
            true,
            true,
        )
        .await
        .expect("the slow log is allowed");
        assert_eq!(
            r.config,
            Some(Err(LiveError::Denied("CONFIG GET".into()))),
            "valkey={valkey}"
        );
        assert!(r.keys_off.is_none(), "valkey={valkey}");
        let set = r
            .entries
            .iter()
            .find(|e| e.command == "SET")
            .expect("a SET");
        assert_eq!(set.keys, ["k1"], "valkey={valkey}");
        assert_no_secret(&format!("{:?}", r.config));
    }
}

#[tokio::test]
async fn a_user_without_command_info_sees_entries_without_key_names() {
    // The rules `+config|get` and the lack of `+command|info` need Redis 7's ACL
    // subcommands.
    if major_version(&mut common::redis().await.admin().await).await < 7 {
        return;
    }
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens(&["+slowlog|get", "+config|get"])).await;
        let mut admin = server.admin().await;
        log_everything(&mut admin).await;
        cmd(&mut admin, &["SET", "k1", "v"]).await;
        let mut conn = connect_as(&server, "lens", "lens-pw").await;
        let r = slowlog(
            &mut conn,
            &Budget::new(None),
            Default::default(),
            true,
            true,
        )
        .await
        .expect("the slow log is allowed");
        assert_eq!(
            r.keys_off,
            Some(LiveError::Denied("COMMAND INFO".into())),
            "valkey={valkey}: {:?}",
            r.keys_off
        );
        assert!(r.config.unwrap().is_ok(), "valkey={valkey}");
        let set = r
            .entries
            .iter()
            .find(|e| e.command == "SET")
            .expect("a SET");
        assert!(set.keys.is_empty(), "valkey={valkey}");
        assert_no_secret(&format!("{:?}", r.keys_off));

        // Asked not to look the keys up, it does not even try.
        let budget = Budget::new(None);
        let r = slowlog(&mut conn, &budget, Default::default(), false, false)
            .await
            .unwrap();
        assert_eq!(budget.taken(), 1);
        assert!(r.keys_off.is_none() && r.config.is_none());
    }
}

// -- through the live task ----------------------------------------------------

#[tokio::test]
async fn the_live_task_answers_server_requests() {
    use std::sync::Arc;
    use tokio::sync::mpsc;

    for (kind, server) in servers().await {
        let budget = Budget::new(None);
        let conn = server.admin().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = redis_lens::live::spawn(conn, Arc::clone(&budget), move |id, result| {
            let _ = tx.send((id, result));
        });
        assert!(handle.send(ReqId(1), LiveRequest::Server(Request::Info)));
        assert!(handle.send(ReqId(2), LiveRequest::Server(Request::Clients)));
        let slow = LiveRequest::Server(Request::SlowLog {
            specs: Default::default(),
            config: true,
            keys: true,
        });
        assert!(handle.send(ReqId(3), slow));
        let mut got = Vec::new();
        for _ in 0..3 {
            let (id, result) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("an answer in time")
                .expect("the task is alive");
            got.push((id.0, result));
        }
        got.sort_by_key(|(id, _)| *id);
        assert!(
            matches!(&got[0].1, Ok(LiveReply::Server(Reply::Info { .. }))),
            "{kind}"
        );
        assert!(
            matches!(&got[1].1, Ok(LiveReply::Server(Reply::Clients(l))) if !l.is_empty()),
            "{kind}"
        );
        assert!(
            matches!(&got[2].1, Ok(LiveReply::Server(Reply::SlowLog(_)))),
            "{kind}"
        );
    }
}

#[tokio::test]
async fn polls_respect_the_command_rate() {
    for (_, server) in servers().await {
        // 20 commands a second: ten INFOs take about half a second, not a few milliseconds.
        let mut conn = server.admin().await;
        let budget = Budget::new(Some(20));
        let t0 = tokio::time::Instant::now();
        for _ in 0..11 {
            info(&mut conn, &budget).await;
        }
        assert!(
            t0.elapsed() >= Duration::from_millis(400),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(budget.taken(), 11);
    }
}
