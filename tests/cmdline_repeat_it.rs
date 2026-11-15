//! `N cmd` repeats and pipeline mode on the command line (spec "Command line panel"),
//! against real Redis and Valkey servers. The panel is driven as a user drives it
//! (`run_cmdline_for_tests`: connect, `:`, type, `enter`), with the TUI's own live
//! connection: a readonly repeat one at a time and in a pipeline, a write repeat on a
//! profile with writes on (the count applied), a write repeat refused on a read-only
//! profile with nothing sent, destructive and `@dangerous` repeats refused under
//! `skip_non_prod_confirm`, and the budget limiting a repeat. Also on Redis 6.2
//! (`REDIS_LENS_IT_REDIS_IMAGE=redis:6.2-alpine`).
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::cluster::parse_commandstats;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use redis::aio::MultiplexedConnection;
use redis_lens::budget::Budget;
use redis_lens::live::cmdline::{self, Args, CommandInfo, Reply, Request, pipeline_batch};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::tui::app::views::cmdline::{
    CmdlineForTests, CmdlineStep, REPEAT_ASKS, run_cmdline_for_tests,
};
use redis_lens::write::Settings;

fn profile(port: u16, env: Env, read_only: Option<bool>) -> Profile {
    Profile {
        name: "cmdline-it".to_string(),
        env,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        force_standalone: false,
        tags: Vec::new(),
    }
}

fn line(s: &str) -> CmdlineStep {
    CmdlineStep::Line(s.to_string())
}

fn ctrl(c: char) -> CmdlineStep {
    CmdlineStep::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
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

/// How many times the server ran `name` since the last reset.
async fn calls(c: &mut MultiplexedConnection, name: &str) -> u64 {
    commandstats(c).await.get(name).map_or(0, |s| s.0)
}

async fn get(c: &mut MultiplexedConnection, key: &str) -> Option<String> {
    redis::cmd("GET").arg(key).query_async(c).await.unwrap()
}

async fn panel(profile: Profile, steps: Vec<CmdlineStep>) -> CmdlineForTests {
    tokio::time::timeout(
        Duration::from_secs(90),
        run_cmdline_for_tests(profile, None, steps),
    )
    .await
    .expect("the panel is done in time")
}

/// The lines of the entry whose command is `cmd`.
fn entry<'a>(seen: &'a CmdlineForTests, cmd: &str) -> &'a [String] {
    seen.entries
        .iter()
        .find(|(l, _)| l.as_deref() == Some(cmd))
        .map(|(_, lines)| lines.as_slice())
        .unwrap_or_else(|| {
            let cmds: Vec<_> = seen.entries.iter().map(|(l, _)| l.clone()).collect();
            panic!("no entry {cmd}: {cmds:?}")
        })
}

fn sent(seen: &CmdlineForTests, what: &str) -> usize {
    seen.requests.iter().filter(|r| r.contains(what)).count()
}

async fn a_readonly_repeat(server: common::TestServer) {
    let mut admin = server.admin().await;
    let _: () = redis::cmd("SET")
        .arg("greeting")
        .arg("hi")
        .query_async(&mut admin)
        .await
        .unwrap();
    reset_stats(&mut admin).await;
    let seen = panel(
        profile(server.port, Env::Local, None),
        vec![line("3 GET greeting"), ctrl('p'), line("4 GET greeting")],
    )
    .await;
    // One at a time: three round trips, the replies numbered.
    let one = entry(&seen, "3 GET greeting");
    assert_eq!(
        &one[..3],
        ["#1 \"hi\"", "#2 \"hi\"", "#3 \"hi\""],
        "{one:?}"
    );
    assert!(
        one[3].starts_with("(3 replies in 3 round trips · "),
        "{one:?}"
    );
    // Pipeline mode: one pipeline (no budget on a local profile).
    let piped = entry(&seen, "4 GET greeting");
    assert_eq!(piped.len(), 5, "{piped:?}");
    assert_eq!(piped[3], "#4 \"hi\"");
    assert!(
        piped[4].starts_with("(4 replies in 1 pipeline · "),
        "{piped:?}"
    );
    // The server ran GET exactly 7 times.
    assert_eq!(calls(&mut admin, "get").await, 7);
    assert_eq!(sent(&seen, "Command(Run {"), 3, "{:?}", seen.requests);
    assert_eq!(sent(&seen, "Command(Batch {"), 1, "{:?}", seen.requests);
    // Nothing the requests print holds a value.
    assert!(seen.requests.iter().all(|r| !r.contains("greeting")));
}

#[tokio::test]
async fn a_readonly_repeat_redis() {
    a_readonly_repeat(common::redis().await).await;
}

#[tokio::test]
async fn a_readonly_repeat_valkey() {
    a_readonly_repeat(common::valkey().await).await;
}

async fn a_write_repeat_with_writes_on(server: common::TestServer) {
    let mut admin = server.admin().await;
    reset_stats(&mut admin).await;
    // Pipeline mode on: writes still go one at a time, each through the write gate.
    let seen = panel(
        profile(server.port, Env::Local, Some(false)),
        vec![ctrl('p'), line("5 INCR counter")],
    )
    .await;
    assert_eq!(get(&mut admin, "counter").await.as_deref(), Some("5"));
    assert_eq!(calls(&mut admin, "incr").await, 5);
    let lines = entry(&seen, "5 INCR counter");
    assert_eq!(
        &lines[..5],
        [
            "#1 (integer) 1",
            "#2 (integer) 2",
            "#3 (integer) 3",
            "#4 (integer) 4",
            "#5 (integer) 5"
        ],
        "{lines:?}"
    );
    assert!(lines[5].starts_with("(5 replies in 5 round trips"));
    // Each one a write of its own with a permit from the gate.
    assert_eq!(sent(&seen, "Write(Permit(INCR))"), 5, "{:?}", seen.requests);
}

#[tokio::test]
async fn a_write_repeat_with_writes_on_redis() {
    a_write_repeat_with_writes_on(common::redis().await).await;
}

#[tokio::test]
async fn a_write_repeat_with_writes_on_valkey() {
    a_write_repeat_with_writes_on(common::valkey().await).await;
}

async fn a_write_repeat_on_a_read_only_profile(server: common::TestServer) {
    let mut admin = server.admin().await;
    for (env, read_only) in [(Env::Local, Some(true)), (Env::Prod, None)] {
        reset_stats(&mut admin).await;
        let seen = panel(
            profile(server.port, env, read_only),
            vec![
                line("5 INCR counter"),
                ctrl('p'),
                line("3 SET k v"),
                line("101 GET k"),
            ],
        )
        .await;
        for cmd in ["5 INCR counter", "3 SET k v"] {
            let lines = entry(&seen, cmd);
            assert_eq!(lines.len(), 1, "{env:?} {cmd}: {lines:?}");
            assert!(
                lines[0].contains("cmdline-it is read-only"),
                "{env:?} {cmd}: {lines:?}"
            );
        }
        let capped = entry(&seen, "101 GET k");
        assert!(capped[0].contains("at most 100"), "{capped:?}");
        // Nothing was sent: no write reached the server, no GET either.
        assert_eq!(sent(&seen, "Write("), 0, "{:?}", seen.requests);
        for name in ["incr", "set", "get"] {
            assert_eq!(calls(&mut admin, name).await, 0, "{env:?} {name}");
        }
        assert_eq!(get(&mut admin, "counter").await, None);
        assert_eq!(get(&mut admin, "k").await, None);
    }
}

#[tokio::test]
async fn a_write_repeat_on_a_read_only_profile_redis() {
    a_write_repeat_on_a_read_only_profile(common::redis().await).await;
}

#[tokio::test]
async fn a_write_repeat_on_a_read_only_profile_valkey() {
    a_write_repeat_on_a_read_only_profile(common::valkey().await).await;
}

/// `skip_non_prod_confirm` takes the question away from one destructive or
/// `@dangerous` write on a profile that is not `prod`, but such a write is still not
/// repeated: nothing is sent. A plain edit still repeats.
async fn a_skipped_question_does_not_repeat(server: common::TestServer) {
    let mut admin = server.admin().await;
    let _: () = redis::cmd("RPUSH")
        .arg("jobs")
        .arg(&["a", "b", "c", "d"])
        .query_async(&mut admin)
        .await
        .unwrap();
    reset_stats(&mut admin).await;
    let seen = panel(
        profile(server.port, Env::Stage, Some(false)),
        vec![
            CmdlineStep::Settings(Settings {
                skip_non_prod_confirm: true,
                ..Settings::default()
            }),
            line("3 LPOP jobs"),
            line("3 FLUSHALL"),
            line("3 INCR c"),
        ],
    )
    .await;
    for cmd in ["3 LPOP jobs", "3 FLUSHALL"] {
        assert_eq!(entry(&seen, cmd), [REPEAT_ASKS], "{cmd}");
    }
    assert_eq!(calls(&mut admin, "lpop").await, 0);
    assert_eq!(calls(&mut admin, "flushall").await, 0);
    let len: u64 = redis::cmd("LLEN")
        .arg("jobs")
        .query_async(&mut admin)
        .await
        .unwrap();
    assert_eq!(len, 4);
    assert_eq!(get(&mut admin, "c").await.as_deref(), Some("3"));
    assert_eq!(calls(&mut admin, "incr").await, 3);
    assert_eq!(sent(&seen, "Write("), 3, "{:?}", seen.requests);
}

#[tokio::test]
async fn a_skipped_question_does_not_repeat_redis() {
    a_skipped_question_does_not_repeat(common::redis().await).await;
}

#[tokio::test]
async fn a_skipped_question_does_not_repeat_valkey() {
    a_skipped_question_does_not_repeat(common::valkey().await).await;
}

async fn info(c: &mut MultiplexedConnection, name: &str) -> CommandInfo {
    match cmdline::run(c, &Budget::new(None), Request::Info { name: name.into() })
        .await
        .unwrap()
    {
        Reply::Info(Some(i)) => i,
        other => panic!("{other:?}"),
    }
}

/// The budget limits a repeat: 20 commands at 20 a second take a second, sent one at
/// a time or in pipelines, as the panel splits them ([`pipeline_batch`]). The TUI's
/// budgets are none (local) or 5,000 a second, too fast to time, so this sends the
/// panel's requests through `live::cmdline` with a budget of its own.
async fn the_budget_limits_a_repeat(server: common::TestServer) {
    let mut c = server.admin().await;
    let mut admin = server.admin().await;
    let get = info(&mut c, "get").await;
    for pipelined in [false, true] {
        reset_stats(&mut admin).await;
        let budget = Budget::new(Some(20));
        let t0 = Instant::now();
        let mut left = 20u32;
        let mut trips = 0;
        while left > 0 {
            let n = if pipelined {
                pipeline_batch(budget.per_sec(), left)
            } else {
                1
            };
            let args = Args::new(vec![b"GET".to_vec(), b"k".to_vec()]);
            let req = if n > 1 {
                Request::Batch {
                    args,
                    info: get.clone(),
                    confirmed: false,
                    times: n,
                }
            } else {
                Request::Run {
                    args,
                    info: get.clone(),
                    confirmed: false,
                }
            };
            match cmdline::run(&mut c, &budget, req).await.unwrap() {
                Reply::Ran(o) => assert_eq!(o.lines(), ["(nil)"]),
                Reply::Batch(o) => assert_eq!(o.len(), n as usize),
                other => panic!("{other:?}"),
            }
            left -= n;
            trips += 1;
        }
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(950), "{pipelined}: {took:?}");
        assert!(took < Duration::from_secs(5), "{pipelined}: {took:?}");
        assert_eq!(budget.taken(), 20);
        assert_eq!(calls(&mut admin, "get").await, 20, "{pipelined}");
        // At 20 a second a pipeline holds 2: a tenth of a second of the budget.
        assert_eq!(trips, if pipelined { 10 } else { 20 });
    }
}

#[tokio::test]
async fn the_budget_limits_a_repeat_redis() {
    the_budget_limits_a_repeat(common::redis().await).await;
}

#[tokio::test]
async fn the_budget_limits_a_repeat_valkey() {
    the_budget_limits_a_repeat(common::valkey().await).await;
}
