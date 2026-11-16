//! Go to (`ctrl-k`) against real Redis and Valkey servers, through the TUI's own app
//! state and runtime: opening the box, typing a pattern's name, moving and closing it
//! send the server nothing (no `SCAN`, no `KEYS`, no `TYPE`, `MEMORY` or any per-key
//! command), and choosing the pattern's row then sends what `→` on that pattern sends.
//! While the box is open on a screen that polls (1 Overview, `INFO` every 2 s), the
//! screen's own polls run and Go to adds nothing to them.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::cluster::parse_commandstats;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use redis::aio::MultiplexedConnection;
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::tui::run_goto_for_tests;
use tokio::sync::mpsc;

const PATTERN: &str = "user:{id}:cart";

fn profile(port: u16) -> Profile {
    Profile {
        name: "tui-goto".to_string(),
        env: Env::Local,
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
        force_standalone: false,
        tags: Vec::new(),
    }
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn typed(s: &str) -> Vec<KeyEvent> {
    s.chars().map(|c| key(KeyCode::Char(c))).collect()
}

fn ctrl_k() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
}

/// Twenty `user:<n>:cart` hashes and a few other keys.
async fn seed(admin: &mut MultiplexedConnection) {
    let mut pipe = redis::pipe();
    for i in 0..20 {
        pipe.cmd("HSET")
            .arg(format!("user:{i}:cart"))
            .arg("sku")
            .arg(i)
            .ignore();
    }
    for i in 0..5 {
        pipe.cmd("SET")
            .arg(format!("session:{i}"))
            .arg("v")
            .ignore();
    }
    let () = pipe.query_async(admin).await.unwrap();
}

async fn reset_stats(c: &mut MultiplexedConnection) {
    redis::cmd("CONFIG")
        .arg("RESETSTAT")
        .query_async::<()>(c)
        .await
        .unwrap();
}

/// The commands the server ran since the last reset, by name, with their calls: the
/// test's own `CONFIG RESETSTAT` left out, and `INFO` too unless `with_info` (the test's own
/// `INFO` is not counted in what it reads, so with `info` the app's polls show).
async fn ran_with(c: &mut MultiplexedConnection, with_info: bool) -> HashMap<String, u64> {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(c)
        .await
        .unwrap();
    parse_commandstats(&info)
        .into_iter()
        .filter(|(name, _)| !matches!(name.as_str(), "config|resetstat" | "config"))
        .filter(|(name, _)| with_info || name != "info")
        .map(|(name, (calls, _))| (name, calls))
        .filter(|(_, calls)| *calls > 0)
        .collect()
}

async fn ran(c: &mut MultiplexedConnection) -> HashMap<String, u64> {
    ran_with(c, false).await
}

/// Ticks of polling in a polled phase: 6 s, three of Overview's 2 s polls.
const POLLS: u32 = 60;

#[tokio::test]
async fn typing_in_go_to_reads_nothing_and_a_pattern_row_reads_what_right_arrow_reads() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = if valkey {
            common::valkey().await
        } else {
            common::redis().await
        };
        let mut admin = server.admin().await;
        seed(&mut admin).await;

        // 0: open, type the pattern's name, move, clear, close.
        let mut typing = vec![ctrl_k()];
        typing.extend(typed(PATTERN));
        typing.extend([
            key(KeyCode::Down),
            key(KeyCode::Up),
            key(KeyCode::Tab),
            key(KeyCode::Esc),
            key(KeyCode::Esc),
        ]);
        // 1: Overview polls, the box closed, so the polls keep time alike in 2 and 3.
        // 2: Overview polls, the box closed. 3: the box opened and typed into, then
        // Overview polls under it.
        let mut opened = vec![ctrl_k()];
        opened.extend(typed("cart"));
        // 4: the box closed (the text cleared, then closed), then opened, the
        // pattern's name typed and enter on its row (the default).
        let mut choose = vec![key(KeyCode::Esc), key(KeyCode::Esc), ctrl_k()];
        choose.extend(typed(PATTERN));
        choose.push(key(KeyCode::Enter));
        // 5: the loaded keys forgotten, back to the patterns (the row Go to selected)
        // and `→` on it.
        let arrow = vec![key(KeyCode::Esc), key(KeyCode::Right)];
        let phases = vec![
            (false, typing, 0),
            (false, Vec::new(), POLLS),
            (false, Vec::new(), POLLS),
            (false, opened, POLLS),
            (false, choose, 0),
            (true, arrow, 0),
        ];
        let n = phases.len();

        let (ready_tx, mut ready) = mpsc::unbounded_channel();
        let (go, go_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(run_goto_for_tests(
            profile(server.port),
            None,
            phases,
            ready_tx,
            go_rx,
        ));
        let mut stats = Vec::new();
        let mut polled = Vec::new();
        for i in 0..=n {
            let at = tokio::time::timeout(Duration::from_secs(90), ready.recv())
                .await
                .expect("the phase starts in time")
                .expect("the run goes on");
            assert_eq!(at, i, "{what}");
            if i > 0 {
                polled.push(ran_with(&mut admin, true).await);
                stats.push(ran(&mut admin).await);
            }
            reset_stats(&mut admin).await;
            go.send(()).unwrap();
        }
        let phases = tokio::time::timeout(Duration::from_secs(90), run)
            .await
            .expect("the run ends in time")
            .expect("the run does not panic");

        // Typing, moving and closing: nothing asked of the live connection, nothing run.
        assert!(phases[0].requests.is_empty(), "{what}: {:?}", phases[0]);
        assert!(
            !phases[0].open && phases[0].screen == "Overview",
            "{what}: {:?}",
            phases[0]
        );
        assert!(stats[0].is_empty(), "{what}: the server ran {:?}", stats[0]);

        // Overview's polls with the box open: what they are with it closed, no more.
        assert!(!phases[2].open && phases[3].open, "{what}: {phases:?}");
        assert_eq!(phases[3].screen, "Overview", "{what}: {:?}", phases[3]);
        assert!(
            phases[2].requests.iter().any(|r| r.starts_with("Server(")),
            "{what}: Overview polls: {:?}",
            phases[2]
        );
        assert_eq!(
            phases[2].requests, phases[3].requests,
            "{what}: Go to adds no request to the polls"
        );
        assert!(polled[2].contains_key("info"), "{what}: {:?}", polled[2]);
        assert_eq!(polled[2], polled[3], "{what}: the server ran the same");
        assert!(!polled[3].contains_key("keys"), "{what}: {:?}", polled[3]);
        assert!(!polled[3].contains_key("scan"), "{what}: {:?}", polled[3]);

        // The pattern's row: its key list, read as `→` reads it.
        let (choose, arrow) = (&phases[4], &phases[5]);
        assert_eq!(choose.screen, "KeyList", "{what}: {choose:?}");
        assert_eq!(choose.key_list.as_deref(), Some(PATTERN), "{what}");
        assert!(!choose.open, "{what}");
        assert!(stats[4].contains_key("scan"), "{what}: {:?}", stats[4]);
        assert!(!stats[4].contains_key("keys"), "{what}: {:?}", stats[4]);

        assert_eq!(arrow.screen, "KeyList", "{what}: {arrow:?}");
        assert_eq!(arrow.key_list.as_deref(), Some(PATTERN), "{what}");
        let keys_requests = |p: &redis_lens::tui::GoToPhase| -> Vec<String> {
            p.requests
                .iter()
                .filter(|r| r.starts_with("Keys("))
                .cloned()
                .collect()
        };
        assert!(!keys_requests(choose).is_empty(), "{what}");
        assert_eq!(
            keys_requests(choose),
            keys_requests(arrow),
            "{what}: Go to and → read the same"
        );
        // Going back to the patterns reads nothing more here (the names are in), so the
        // server ran the same commands for both.
        assert_eq!(stats[4], stats[5], "{what}");
    }
}
