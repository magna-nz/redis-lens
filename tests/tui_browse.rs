//! Browse › Keys before an analysis against real Redis and Valkey servers: the tree
//! comes from key names read with `SCAN` (1,000 a call, through the budget), nothing
//! else is sent, and its patterns and counts match what was seeded.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::cluster::parse_commandstats;
use redis::aio::MultiplexedConnection;
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::scan::StopFlag;
use redis_lens::tui::{BrowseForTests, run_browse_for_tests, run_connect_for_tests};
use tokio::sync::mpsc;

fn profile(port: u16) -> Profile {
    Profile {
        name: "tui-browse".to_string(),
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
        tags: Vec::new(),
    }
}

/// `carts` + `orders` + `products` keys in three patterns, and one `queue:emails`, in
/// pipelined batches.
async fn seed(admin: &mut MultiplexedConnection, carts: u64, orders: u64, products: u64) {
    let mut keys: Vec<String> = Vec::new();
    keys.extend((0..carts).map(|i| format!("user:{i}:cart")));
    keys.extend((0..orders).map(|i| format!("order:{i}")));
    keys.extend((0..products).map(|i| format!("cache:product:{i}")));
    keys.push("queue:emails".to_string());
    for chunk in keys.chunks(5_000) {
        let mut pipe = redis::pipe();
        for k in chunk {
            pipe.cmd("SET").arg(k).arg("v").ignore();
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

/// What a connect alone sends: the baseline the names reading adds to.
async fn connect_only(port: u16, admin: &mut MultiplexedConnection) -> HashMap<String, (u64, u64)> {
    reset_stats(admin).await;
    let (tx, _rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(60),
        run_connect_for_tests(profile(port), None, None, StopFlag::new(), tx),
    )
    .await
    .expect("the connect ends in time");
    commandstats(admin).await
}

/// Connects, reads names on Browse with `more` presses of `n`, and returns what the
/// tree showed and what the server saw.
async fn browse(
    port: u16,
    admin: &mut MultiplexedConnection,
    more: usize,
) -> (BrowseForTests, HashMap<String, (u64, u64)>) {
    reset_stats(admin).await;
    let seen = tokio::time::timeout(
        Duration::from_secs(60),
        run_browse_for_tests(profile(port), None, more),
    )
    .await
    .expect("browse ends in time");
    let stats = commandstats(admin).await;
    (seen, stats)
}

/// The names reading sent `SCAN` and nothing else: one per page beyond the connect's
/// own, and no command the connect alone does not send more often.
fn only_scan(
    what: &str,
    seen: &BrowseForTests,
    stats: &HashMap<String, (u64, u64)>,
    baseline: &HashMap<String, (u64, u64)>,
) {
    assert_eq!(calls(stats, "keys"), 0, "{what}: {stats:?}");
    assert_eq!(
        calls(stats, "scan"),
        calls(baseline, "scan") + seen.pages,
        "{what}: one SCAN a page: {stats:?}"
    );
    for name in stats.keys() {
        if name == "scan" || name == "config|resetstat" || name == "config" {
            continue;
        }
        // INFO: this test's own commandstats read, and the connect's figures.
        assert!(
            calls(stats, name) <= calls(baseline, name) + u64::from(name == "info"),
            "{what}: {name} was sent {} times, a connect alone sends it {}: {stats:?}",
            calls(stats, name),
            calls(baseline, name)
        );
    }
    // The app asked for pages of names, and for nothing else.
    assert_eq!(
        seen.requests.len() as u64,
        seen.pages,
        "{what}: {:?}",
        seen.requests
    );
    for r in &seen.requests {
        assert!(r.starts_with("Names(Page"), "{what}: {r}");
    }
}

#[tokio::test]
async fn a_small_keyspace_is_read_whole_and_counted_exactly() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = if valkey {
            common::valkey().await
        } else {
            common::redis().await
        };
        let mut admin = server.admin().await;
        seed(&mut admin, 1_500, 900, 300).await;
        let baseline = connect_only(server.port, &mut admin).await;
        let (seen, stats) = browse(server.port, &mut admin, 0).await;

        assert_eq!(seen.error, None, "{what}");
        assert!(
            seen.complete,
            "{what}: 2,701 keys fit in the first pages: {seen:?}"
        );
        assert!(seen.pages >= 2 && seen.pages <= 8, "{what}: {}", seen.pages);
        assert_eq!(seen.names_read, 2_701, "{what}");
        let want: Vec<(String, u64)> = vec![
            ("cache:product:{id}".into(), 300),
            ("order:{id}".into(), 900),
            ("queue:emails".into(), 1),
            ("user:{id}:cart".into(), 1_500),
        ];
        assert_eq!(seen.patterns, want, "{what}");
        only_scan(what, &seen, &stats, &baseline);
    }
}

#[tokio::test]
async fn a_large_keyspace_is_estimated_and_n_reads_one_page_more() {
    for valkey in [false, true] {
        let what = if valkey { "valkey" } else { "redis" };
        let server = if valkey {
            common::valkey().await
        } else {
            common::redis().await
        };
        let mut admin = server.admin().await;
        // 30,001 keys: half carts, a third orders, a sixth products.
        seed(&mut admin, 15_000, 10_000, 5_000).await;
        let baseline = connect_only(server.port, &mut admin).await;

        let (first, stats) = browse(server.port, &mut admin, 0).await;
        assert_eq!(first.error, None, "{what}");
        assert_eq!(first.pages, 8, "{what}: eight pages first");
        assert!(!first.complete, "{what}");
        // About 1,000 names a page: SCAN's COUNT is a hint.
        assert!(
            first.names_read >= 4_000 && first.names_read < 30_001,
            "{what}: {}",
            first.names_read
        );
        only_scan(what, &first, &stats, &baseline);
        // The estimates scale the names seen to DBSIZE: within a fifth of the seed.
        let est: HashMap<&str, u64> = first
            .patterns
            .iter()
            .map(|(p, n)| (p.as_str(), *n))
            .collect();
        for (pattern, seeded) in [
            ("user:{id}:cart", 15_000u64),
            ("order:{id}", 10_000),
            ("cache:product:{id}", 5_000),
        ] {
            let got = *est
                .get(pattern)
                .unwrap_or_else(|| panic!("{what}: {pattern} in {est:?}"));
            assert!(
                got.abs_diff(seeded) <= seeded / 5,
                "{what}: {pattern} estimated {got}, seeded {seeded}"
            );
        }

        // `n`: one SCAN more.
        let (more, stats) = browse(server.port, &mut admin, 1).await;
        assert_eq!(more.error, None, "{what}");
        assert_eq!(more.pages, 9, "{what}");
        assert!(more.names_read > first.names_read, "{what}");
        only_scan(what, &more, &stats, &baseline);
    }
}
