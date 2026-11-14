//! The command line panel's live requests (`live::cmdline`) against real Redis and
//! Valkey servers: the flags from `COMMAND INFO` and the decision, a readonly command's
//! reply, a write that is never sent, `KEYS` asking first, `NOPERM`, and the helper
//! from `COMMAND DOCS` (off on Redis 6.2: run with `REDIS_LENS_IT_REDIS_IMAGE=redis:6.2-alpine`).
#![cfg(feature = "integration-tests")]

mod common;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::live::cmdline::{self, Args, CommandInfo, Gate, NOT_SENT, Reply, Request};
use redis_lens::live::{self, LiveError, LiveReply, LiveRequest};

async fn ask(conn: &mut MultiplexedConnection, req: Request) -> Result<Reply, LiveError> {
    cmdline::run(conn, &Budget::new(None), req).await
}

async fn info(conn: &mut MultiplexedConnection, name: &str) -> Option<CommandInfo> {
    match ask(conn, Request::Info { name: name.into() })
        .await
        .unwrap()
    {
        Reply::Info(i) => i,
        other => panic!("{other:?}"),
    }
}

fn words(line: &str) -> Vec<Vec<u8>> {
    redis_lens::live::command::split(line).unwrap()
}

/// What the panel does with `line`: reads the flags, decides, and when it may, sends.
async fn decide(conn: &mut MultiplexedConnection, line: &str) -> (Gate, Option<CommandInfo>) {
    let args = words(line);
    let name = String::from_utf8_lossy(&args[0]).to_ascii_lowercase();
    let i = info(conn, &name).await;
    (cmdline::gate(&args, i.as_ref()), i)
}

async fn send(
    conn: &mut MultiplexedConnection,
    line: &str,
    info: CommandInfo,
    confirmed: bool,
) -> Result<Reply, LiveError> {
    ask(
        conn,
        Request::Run {
            args: Args::new(words(line)),
            info,
            confirmed,
        },
    )
    .await
}

async fn get(conn: &mut MultiplexedConnection, key: &str) -> Option<String> {
    redis::cmd("GET").arg(key).query_async(conn).await.unwrap()
}

/// The major version, and whether it is Valkey.
async fn version(conn: &mut MultiplexedConnection) -> (u32, bool) {
    let info: String = redis::cmd("INFO")
        .arg("server")
        .query_async(conn)
        .await
        .unwrap();
    let field = |k: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}:")))
            .map(|v| v.trim().to_string())
    };
    let (v, valkey) = match field("valkey_version") {
        Some(v) => (v, true),
        None => (field("redis_version").unwrap(), false),
    };
    (v.split('.').next().unwrap().parse().unwrap(), valkey)
}

async fn readonly_get_returns_the_value_with_a_time(server: &common::TestServer) {
    let mut admin = server.admin().await;
    let _: () = redis::cmd("SET")
        .arg("greeting")
        .arg("hello\nworld")
        .query_async(&mut admin)
        .await
        .unwrap();
    let mut c = server.admin().await;
    let (g, i) = decide(&mut c, "GET greeting").await;
    assert_eq!(g, Gate::Send);
    let budget = Budget::new(None);
    let before = budget.taken();
    let r = live::run(
        &mut c,
        &budget,
        LiveRequest::Command(Request::Run {
            args: Args::new(words("GET greeting")),
            info: i.unwrap(),
            confirmed: false,
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        budget.taken(),
        before + 1,
        "the command takes from the budget"
    );
    let LiveReply::Command(Reply::Ran(out)) = r else {
        panic!("{r:?}");
    };
    assert_eq!(out.lines(), ["\"hello\\nworld\""]);
    assert!(!out.error && !out.cut);
    assert!(out.took.as_nanos() > 0);
    // A nested reply, numbered like redis-cli.
    let _: () = redis::cmd("RPUSH")
        .arg("list")
        .arg("a")
        .arg("b")
        .query_async(&mut admin)
        .await
        .unwrap();
    let (g, i) = decide(&mut c, "LRANGE list 0 -1").await;
    assert_eq!(g, Gate::Send);
    let Reply::Ran(out) = send(&mut c, "LRANGE list 0 -1", i.unwrap(), false)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(out.lines(), ["1) \"a\"", "2) \"b\""]);
    // A server error about the command is shown, like redis-cli.
    let (_, i) = decide(&mut c, "LRANGE list x y").await;
    let Reply::Ran(out) = send(&mut c, "LRANGE list x y", i.unwrap(), false)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(out.error);
    assert!(
        out.lines()[0].starts_with("(error) ERR"),
        "{:?}",
        out.lines()
    );
    // Another code too: the server's own, as redis-cli shows it.
    let (_, i) = decide(&mut c, "GET list").await;
    let Reply::Ran(out) = send(&mut c, "GET list", i.unwrap(), false).await.unwrap() else {
        panic!()
    };
    assert!(
        out.error && out.lines()[0].starts_with("(error) WRONGTYPE"),
        "{:?}",
        out.lines()
    );
}

async fn a_write_is_refused_and_never_sent(server: &common::TestServer) {
    let mut admin = server.admin().await;
    let _: () = redis::cmd("SET")
        .arg("k")
        .arg("before")
        .query_async(&mut admin)
        .await
        .unwrap();
    let mut c = server.admin().await;
    // A write goes to the write gate (cmdline_write_it.rs), never to this runner.
    let (g, set) = decide(&mut c, "SET k after").await;
    assert_eq!(g, Gate::Write);
    // Even when the panel would ask the runner anyway, confirmed or with another
    // command's flags, nothing is sent.
    let e = send(&mut c, "SET k after", set.unwrap(), true)
        .await
        .unwrap_err();
    assert_eq!(e, LiveError::Failed(NOT_SENT.into()));
    let get_info = info(&mut c, "get").await.unwrap();
    let e = send(&mut c, "SET k after", get_info, true)
        .await
        .unwrap_err();
    assert_eq!(e, LiveError::Failed(NOT_SENT.into()));
    assert_eq!(get(&mut admin, "k").await.as_deref(), Some("before"));
    // A blocking read and a container command without a readonly subcommand.
    let (g, _) = decide(&mut c, "XREAD BLOCK 0 STREAMS s $").await;
    assert!(
        matches!(&g, Gate::Refuse(w) if w.contains("can block")),
        "{g:?}"
    );
    let (g, _) = decide(&mut c, "BLPOP q 0").await;
    assert!(matches!(g, Gate::Refuse(_)), "{g:?}");
    // No readonly flag: writes, for the write gate.
    let (g, _) = decide(&mut c, "FLUSHDB").await;
    assert_eq!(g, Gate::Write);
    // Before Redis 7.0 the server lists no subcommand flags: refused there.
    let (g, _) = decide(&mut c, "CONFIG GET maxmemory").await;
    assert!(matches!(g, Gate::Refuse(_) | Gate::Write), "{g:?}");
    let (g, _) = decide(&mut c, "NOSUCHCOMMAND x").await;
    assert!(
        matches!(&g, Gate::Refuse(w) if w.contains("no command NOSUCHCOMMAND")),
        "{g:?}"
    );
    let (g, _) = decide(&mut c, "OBJECT ENCODING k").await;
    assert_eq!(g, Gate::Send);
}

/// A script runs commands the gate never sees (the phase 2 verifier reset the slow
/// log with `EVAL_RO`): refused, and never sent even when the runner is asked. MEMORY
/// PURGE is refused too, on Redis 6.2 as on 7 (6.2 flags MEMORY readonly).
async fn scripts_are_refused_and_never_sent(server: &common::TestServer) {
    let mut admin = server.admin().await;
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("slowlog-log-slower-than")
        .arg("0")
        .query_async(&mut admin)
        .await
        .unwrap();
    let _: Option<String> = redis::cmd("GET")
        .arg("x")
        .query_async(&mut admin)
        .await
        .unwrap();
    let len = || async {
        let mut a = server.admin().await;
        redis::cmd("SLOWLOG")
            .arg("LEN")
            .query_async::<i64>(&mut a)
            .await
            .unwrap()
    };
    let before = len().await;
    assert!(before > 0);
    let mut c = server.admin().await;
    let line = "EVAL_RO \"return redis.call('slowlog','reset')\" 0";
    let (g, i) = decide(&mut c, line).await;
    assert!(matches!(g, Gate::Refuse(_)), "{g:?}");
    if let Some(i) = i {
        assert_eq!(
            send(&mut c, line, i, true).await.unwrap_err(),
            LiveError::Failed(NOT_SENT.into())
        );
    }
    assert!(len().await >= before, "the slow log was reset");
    // Never sent as a read: a write for the write gate where the server lists the
    // subcommand's flags, refused before Redis 7.0.
    let (g, _) = decide(&mut c, "MEMORY PURGE").await;
    assert!(matches!(g, Gate::Refuse(_) | Gate::Write), "{g:?}");
    let (g, _) = decide(&mut c, "MEMORY USAGE x").await;
    assert_eq!(g, Gate::Send);
}

async fn keys_asks_first(server: &common::TestServer) {
    let mut admin = server.admin().await;
    let _: () = redis::cmd("SET")
        .arg("user:1")
        .arg("x")
        .query_async(&mut admin)
        .await
        .unwrap();
    let mut c = server.admin().await;
    let (g, i) = decide(&mut c, "KEYS user:*").await;
    assert_eq!(g, Gate::Ask);
    let i = i.unwrap();
    assert_eq!(
        send(&mut c, "KEYS user:*", i.clone(), false)
            .await
            .unwrap_err(),
        LiveError::Failed(NOT_SENT.into())
    );
    let Reply::Ran(out) = send(&mut c, "KEYS user:*", i, true).await.unwrap() else {
        panic!()
    };
    assert_eq!(out.lines(), ["1) \"user:1\""]);
}

async fn noperm_gives_the_fixed_text(server: &common::TestServer) {
    let mut admin = server.admin().await;
    let _: () = redis::cmd("ACL")
        .arg(&[
            "SETUSER",
            "cli-user",
            "on",
            ">cli-pass",
            "~*",
            "+@all",
            "-get",
        ])
        .query_async(&mut admin)
        .await
        .unwrap();
    let mut c = server.admin_as("cli-user", "cli-pass").await;
    let (g, i) = decide(&mut c, "GET k").await;
    assert_eq!(g, Gate::Send);
    let e = send(&mut c, "GET k", i.unwrap(), false).await.unwrap_err();
    assert_eq!(e, LiveError::Denied("GET".into()));
    let text = e.to_string();
    assert_eq!(text, "the server refused GET for this user");
    assert!(!text.contains("cli-user"));
}

async fn the_helper_reads_command_docs(server: &common::TestServer) {
    let mut c = server.admin().await;
    let (major, valkey) = version(&mut c).await;
    let docs = ask(&mut c, Request::Docs { name: "get".into() }).await;
    let names = ask(&mut c, Request::Names).await;
    if valkey || major >= 7 {
        let Ok(Reply::Docs(Some(d))) = docs else {
            panic!("{docs:?}")
        };
        assert!(!d.summary.is_empty());
        assert_eq!(d.arguments, "key");
        assert_eq!(d.complexity, "O(1)");
        let Ok(Reply::Docs(Some(d))) = ask(
            &mut c,
            Request::Docs {
                name: "config".into(),
            },
        )
        .await
        else {
            panic!()
        };
        assert!(!d.subcommand("config", b"GET").unwrap().summary.is_empty());
        let Ok(Reply::Names(n)) = names else {
            panic!("{names:?}")
        };
        assert!(n.iter().any(|x| x == "get") && n.iter().any(|x| x == "xread"));
        let Ok(Reply::Docs(none)) = ask(
            &mut c,
            Request::Docs {
                name: "nosuch".into(),
            },
        )
        .await
        else {
            panic!()
        };
        assert_eq!(none, None);
    } else {
        // Redis 6.2: no COMMAND DOCS and no COMMAND LIST.
        assert_eq!(
            docs.unwrap_err(),
            LiveError::Unsupported("COMMAND DOCS".into())
        );
        assert_eq!(
            names.unwrap_err(),
            LiveError::Unsupported("COMMAND LIST".into())
        );
        // COMMAND INFO still works, so the decision does.
        let (g, _) = decide(&mut c, "GET k").await;
        assert_eq!(g, Gate::Send);
    }
}

async fn all(server: common::TestServer) {
    readonly_get_returns_the_value_with_a_time(&server).await;
    a_write_is_refused_and_never_sent(&server).await;
    scripts_are_refused_and_never_sent(&server).await;
    keys_asks_first(&server).await;
    noperm_gives_the_fixed_text(&server).await;
    the_helper_reads_command_docs(&server).await;
}

#[tokio::test]
async fn command_line_on_redis() {
    all(common::redis().await).await;
}

#[tokio::test]
async fn command_line_on_valkey() {
    all(common::valkey().await).await;
}
