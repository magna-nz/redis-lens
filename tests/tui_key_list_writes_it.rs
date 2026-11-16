//! The key list's writes on the selected key (`t`, `R`, `d` on Browse › Keys › a
//! pattern's keys) against real Redis and Valkey servers, through the TUI's own app
//! state, gate and runtime: the TTL is set, the key renamed, then deleted, and the
//! server holds the result. A read-only profile sends none of them.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::cluster::parse_commandstats;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use redis::aio::MultiplexedConnection;
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::tui::{KeyListStep, run_key_list_for_tests};

const PATTERN: &str = "user:{id}:cart";
/// The new name `R` gives: of the same pattern, so the key list still lists it.
const RENAMED: &str = "user:999:cart";

fn profile(port: u16, read_only: Option<bool>) -> Profile {
    Profile {
        name: "tui-key-list".to_string(),
        env: Env::Local,
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

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn typed(s: &str) -> Vec<KeyEvent> {
    s.chars().map(ch).collect()
}

/// Twenty `user:<n>:cart` strings without a TTL.
async fn seed(admin: &mut MultiplexedConnection) {
    let mut pipe = redis::pipe();
    for i in 0..20 {
        pipe.cmd("SET")
            .arg(format!("user:{i}:cart"))
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

async fn ttl(admin: &mut MultiplexedConnection, k: &str) -> i64 {
    redis::cmd("TTL").arg(k).query_async(admin).await.unwrap()
}

async fn exists(admin: &mut MultiplexedConnection, k: &str) -> bool {
    let n: i64 = redis::cmd("EXISTS")
        .arg(k)
        .query_async(admin)
        .await
        .unwrap();
    n == 1
}

async fn run(port: u16, read_only: Option<bool>, keys: Vec<KeyEvent>) -> Vec<KeyListStep> {
    tokio::time::timeout(
        Duration::from_secs(90),
        run_key_list_for_tests(profile(port, read_only), None, PATTERN, keys),
    )
    .await
    .expect("the key list run ends in time")
}

#[tokio::test]
async fn t_r_and_d_on_the_key_list_write_the_selected_key() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = if valkey {
            common::valkey().await
        } else {
            common::redis().await
        };
        let mut admin = server.admin().await;
        seed(&mut admin).await;

        // t: the TTL form, 600 s, enter (a TTL runs at once off prod).
        let mut keys = vec![ch('t'), ctrl('u')];
        keys.extend(typed("600"));
        keys.push(key(KeyCode::Enter));
        // R: the rename form, a new name of the same pattern, enter.
        keys.extend([ch('R'), ctrl('u')]);
        keys.extend(typed(RENAMED));
        keys.push(key(KeyCode::Enter));
        let steps = run(server.port, None, keys).await;

        let first = steps[0].selected.clone().expect("a selected key");
        assert!(first.starts_with("user:"), "{what}: {first}");
        // After t and its enter: EXPIRE, through the gate.
        let after_ttl = &steps[5];
        assert!(
            after_ttl
                .requests
                .iter()
                .any(|r| r.starts_with("Write(Permit(EXPIRE")),
            "{what}: {after_ttl:?}"
        );
        // After R and its enter: RENAMENX, and the row renamed.
        let after_rename = steps.last().unwrap();
        assert!(
            after_rename
                .requests
                .iter()
                .any(|r| r.starts_with("Write(Permit(RENAMENX")),
            "{what}: {after_rename:?}"
        );
        assert!(
            after_rename.rows.iter().any(|r| r == RENAMED) && !after_rename.rows.contains(&first),
            "{what}: {:?}",
            after_rename.rows
        );
        assert!(!exists(&mut admin, &first).await, "{what}: {first} is gone");
        assert!(exists(&mut admin, RENAMED).await, "{what}");
        // The TTL `t` set (the seed set none), which moved with the key.
        let t = ttl(&mut admin, RENAMED).await;
        assert!(
            (1..=600).contains(&t),
            "{what}: TTL after the rename is {t}"
        );

        // d asks first, then y deletes the selected key with UNLINK, and its row goes.
        let steps = run(server.port, None, vec![ch('d'), ch('y')]).await;
        let gone = steps[0].selected.clone().expect("a selected key");
        assert!(steps[0].asking, "{what}: d asks first: {:?}", steps[0]);
        assert!(
            steps[0].requests.is_empty(),
            "{what}: nothing before y: {:?}",
            steps[0]
        );
        let done = &steps[1];
        assert!(
            done.requests
                .iter()
                .any(|r| r.starts_with("Write(Permit(UNLINK")),
            "{what}: {done:?}"
        );
        assert!(!done.rows.contains(&gone), "{what}: {:?}", done.rows);
        assert!(!exists(&mut admin, &gone).await, "{what}: {gone}");
        let first = [first, gone];
        // The other keys are untouched.
        for i in 0..20 {
            let k = format!("user:{i}:cart");
            if !first.contains(&k) && k != RENAMED {
                assert!(exists(&mut admin, &k).await, "{what}: {k}");
                assert_eq!(ttl(&mut admin, &k).await, -1, "{what}: {k}");
            }
        }
    }
}

#[tokio::test]
async fn a_read_only_profile_sends_no_key_list_write() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = if valkey {
            common::valkey().await
        } else {
            common::redis().await
        };
        let mut admin = server.admin().await;
        seed(&mut admin).await;
        reset_stats(&mut admin).await;
        // Each write key, then what would answer its form or question.
        let mut keys = Vec::new();
        for k in ['t', 'd', 'R', 'T', 'D', '+'] {
            // `y` answers a question; on the list it only copies the key name. No key
            // here leaves the key list.
            keys.extend([ch(k), ch('y')]);
        }
        let steps = run(server.port, Some(true), keys).await;
        for (i, s) in steps.iter().enumerate().step_by(2) {
            assert!(
                s.notice.as_deref().is_some_and(|n| n.contains("read-only")),
                "{what}: step {i}: {s:?}"
            );
            assert!(!s.asking, "{what}: step {i}: {s:?}");
        }
        for s in &steps {
            assert!(
                !s.requests
                    .iter()
                    .any(|r| r.starts_with("Write") || r.starts_with("Bulk")),
                "{what}: {s:?}"
            );
        }
        let stats = commandstats(&mut admin).await;
        for cmd in [
            "expire", "persist", "unlink", "del", "renamenx", "rename", "set", "hset",
        ] {
            assert_eq!(calls(&stats, cmd), 0, "{what}: {cmd}: {stats:?}");
        }
        for i in 0..20 {
            let k = format!("user:{i}:cart");
            assert!(exists(&mut admin, &k).await, "{what}: {k}");
            assert_eq!(ttl(&mut admin, &k).await, -1, "{what}: {k}");
        }
    }
}
