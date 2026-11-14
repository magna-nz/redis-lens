//! Write commands typed on the command line (spec "Command line panel", "Write
//! permissions"), against real Redis and Valkey servers: decided by the panel's gate,
//! granted by the write gate, run by `live::write`. Also on Redis 6.2
//! (`REDIS_LENS_IT_REDIS_IMAGE=redis:6.2-alpine`).
#![cfg(feature = "integration-tests")]

mod common;

use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::live::cmdline::{self, Args, CommandInfo, Gate, Reply, Request};
use redis_lens::live::write::{self, Reply as WriteReply};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::write::{Answer, Permit, Refusals, Settings, WriteOp};

fn profile(env: Env, read_only: Option<bool>) -> Profile {
    Profile {
        name: "it".to_string(),
        env,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port: 6379,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        tags: Vec::new(),
    }
}

fn words(line: &str) -> Vec<Vec<u8>> {
    redis_lens::live::command::split(line).unwrap()
}

/// The op the panel builds for `line`, after reading its flags; panics unless the
/// panel's gate calls it a write.
async fn op(conn: &mut MultiplexedConnection, line: &str) -> WriteOp {
    gated(conn, line)
        .await
        .unwrap_or_else(|g| panic!("{line}: {g:?}"))
}

/// The op for `line` when the panel's gate calls it a write, else what the gate said.
async fn gated(conn: &mut MultiplexedConnection, line: &str) -> Result<WriteOp, Gate> {
    let args = words(line);
    let name = String::from_utf8_lossy(&args[0]).to_ascii_lowercase();
    let info: CommandInfo = match cmdline::run(conn, &Budget::new(None), Request::Info { name })
        .await
        .unwrap()
    {
        Reply::Info(Some(i)) => i,
        other => panic!("{line}: {other:?}"),
    };
    match cmdline::gate(&args, Some(&info)) {
        Gate::Write => Ok(WriteOp::Command {
            args: Args::new(args),
            info,
        }),
        g => Err(g),
    }
}

/// Grants `op` on a writable local profile with `answer`, runs it, and returns the
/// reply's lines and whether it was an error.
async fn run(conn: &mut MultiplexedConnection, op: WriteOp, answer: Answer) -> (Vec<String>, bool) {
    let permit = Permit::grant(
        op,
        &profile(Env::Local, Some(false)),
        &Settings::default(),
        &Refusals::default(),
        &answer,
    )
    .expect("granted");
    match write::run(conn, &Budget::new(None), permit).await.unwrap() {
        WriteReply::Ran(out) => (out.lines().to_vec(), out.error),
        other => panic!("{other:?}"),
    }
}

async fn get(conn: &mut MultiplexedConnection, key: &str) -> Option<String> {
    redis::cmd("GET").arg(key).query_async(conn).await.unwrap()
}

async fn writes_run_through_the_gate(server: &common::TestServer) {
    let mut c = server.admin().await;
    // An edit runs at once outside prod.
    let set = op(&mut c, "SET greeting hello").await;
    let (lines, error) = run(&mut c, set, Answer::None).await;
    assert!(!error && lines[0].contains("OK"), "{lines:?}");
    assert_eq!(get(&mut c, "greeting").await.as_deref(), Some("hello"));

    // A delete needs the y the dialog asks for.
    let del = op(&mut c, "DEL greeting").await;
    let refused = Permit::grant(
        del.clone(),
        &profile(Env::Local, Some(false)),
        &Settings::default(),
        &Refusals::default(),
        &Answer::None,
    );
    assert!(refused.is_err());
    let (lines, _) = run(&mut c, del, Answer::Yes).await;
    assert!(lines[0].contains('1'), "{lines:?}");
    assert_eq!(get(&mut c, "greeting").await, None);

    // A server error is a line in redis-lens's escaped form, not a failure.
    let _: () = redis::cmd("RPUSH")
        .arg("q")
        .arg("a")
        .query_async(&mut c)
        .await
        .unwrap();
    let append = op(&mut c, "APPEND q x").await;
    let (lines, error) = run(&mut c, append, Answer::None).await;
    assert!(
        error && lines[0].starts_with("(error) WRONGTYPE"),
        "{lines:?}"
    );

    // A dangerous command (CONFIG SET) needs y outside prod; the test container only.
    // Before Redis 7.0 the server lists no subcommand flags, so it is refused there.
    let cfg = match gated(&mut c, "CONFIG SET maxmemory-policy allkeys-lfu").await {
        Ok(op) => op,
        Err(Gate::Refuse(why)) => {
            assert!(why.contains("subcommands"), "{why}");
            println!("CONFIG SET refused on this server: {why}");
            return;
        }
        Err(g) => panic!("{g:?}"),
    };
    let (lines, error) = run(&mut c, cfg, Answer::Yes).await;
    assert!(!error, "{lines:?}");
    let policy: Vec<String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("maxmemory-policy")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(policy[1], "allkeys-lfu");
}

async fn no_write_is_granted_on_a_read_only_profile(server: &common::TestServer) {
    let mut c = server.admin().await;
    for line in ["SET k v", "DEL k", "FLUSHDB"] {
        let op = op(&mut c, line).await;
        for p in [profile(Env::Prod, None), profile(Env::Local, Some(true))] {
            for a in [Answer::None, Answer::Yes, Answer::Typed("it".into())] {
                let r = Permit::grant(
                    op.clone(),
                    &p,
                    &Settings::default(),
                    &Refusals::default(),
                    &a,
                );
                assert!(r.is_err(), "{line}");
            }
        }
    }
    let n: i64 = redis::cmd("DBSIZE").query_async(&mut c).await.unwrap();
    assert_eq!(n, 0);
}

async fn scripts_are_never_a_write(server: &common::TestServer) {
    let mut c = server.admin().await;
    let args = words("EVAL \"return redis.call('SET','k','v')\" 0");
    let info: CommandInfo = match cmdline::run(
        &mut c,
        &Budget::new(None),
        Request::Info {
            name: "eval".into(),
        },
    )
    .await
    .unwrap()
    {
        Reply::Info(Some(i)) => i,
        other => panic!("{other:?}"),
    };
    assert!(matches!(cmdline::gate(&args, Some(&info)), Gate::Refuse(_)));
    let op = WriteOp::Command {
        args: Args::new(args),
        info,
    };
    let r = Permit::grant(
        op,
        &profile(Env::Local, Some(false)),
        &Settings::default(),
        &Refusals::default(),
        &Answer::Yes,
    );
    assert!(r.unwrap_err().contains("script"));
    let n: i64 = redis::cmd("DBSIZE").query_async(&mut c).await.unwrap();
    assert_eq!(n, 0);
}

/// A transaction or a watch would hold the connection every view shares: refused
/// before its flags are read, writes on included.
async fn transactions_are_refused(server: &common::TestServer) {
    let mut c = server.admin().await;
    for line in [
        "MULTI",
        "EXEC",
        "DISCARD",
        "WATCH k",
        "UNWATCH",
        "SYNC",
        "READONLY",
        "CLIENT SETNAME x",
    ] {
        match gated(&mut c, line).await {
            Err(Gate::Refuse(why)) => {
                assert!(
                    why.contains("connection every view shares"),
                    "{line}: {why}"
                )
            }
            other => panic!("{line}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn on_redis() {
    let server = common::redis().await;
    no_write_is_granted_on_a_read_only_profile(&server).await;
    scripts_are_never_a_write(&server).await;
    transactions_are_refused(&server).await;
    writes_run_through_the_gate(&server).await;
}

#[tokio::test]
async fn on_valkey() {
    let server = common::valkey().await;
    no_write_is_granted_on_a_read_only_profile(&server).await;
    scripts_are_never_a_write(&server).await;
    transactions_are_refused(&server).await;
    writes_run_through_the_gate(&server).await;
}
