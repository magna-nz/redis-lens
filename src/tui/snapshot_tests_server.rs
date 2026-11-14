//! Insta snapshots for provider notices and the server context strip.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::app::{App, ConnId, Effect, Msg, ScanId};
use super::fixtures;
use super::ui;
use crate::conn::ScanTarget;
use crate::model::{
    ContextGap, KeyType, KeysizeHistogram, KeyspaceInfo, MemoryStats, PERM_MEMORY_STATS, PermCheck,
    PermStatus, PermissionReport, Profile, Provider, ScanReport, ServerContext, ServerContextParts,
};
use crate::scan::ScanProgress;
use crate::scan::context::bias_warnings;

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn render(app: &App, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| ui::draw(f, app)).unwrap();
    terminal
}

fn buffer_lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buf = terminal.backend().buffer();
    let area = buf.area;
    (0..area.height)
        .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn connect_selected(app: &mut App) -> ConnId {
    let effects = app.handle_key(key(KeyCode::Enter));
    match effects.as_slice() {
        [Effect::Connect { id, .. }] => *id,
        other => panic!("expected a Connect effect, got {other:?}"),
    }
}

fn scan_of(app: &App) -> ScanId {
    app.session.as_ref().unwrap().scan
}

/// The fixture profiles with prod-cache-01 on ElastiCache Serverless.
fn serverless_profiles() -> Vec<Profile> {
    let mut p = fixtures::profiles();
    p[0].provider = Some(Provider::ElasticacheServerless);
    p[0].endpoint.host = "prod-cache-01-x1y2z3.serverless.use1.cache.amazonaws.com".into();
    p
}

/// Every check allowed, MEMORY STATS included.
fn perms_all() -> PermissionReport {
    let mut p = fixtures::perms_ok();
    p.checks.push(PermCheck {
        label: PERM_MEMORY_STATS,
        status: PermStatus::Allowed,
    });
    p
}

/// Every check allowed but MEMORY STATS, refused by the ACL.
fn perms_stats_denied() -> PermissionReport {
    let mut p = fixtures::perms_ok();
    p.checks.push(PermCheck {
        label: PERM_MEMORY_STATS,
        status: PermStatus::Denied("NOPERM MEMORY STATS is not allowed for this user".into()),
    });
    p
}

/// Exact `INFO keysizes` histograms that agree with the fixture report's type mix: one
/// bin per sampled type holding its share of the keyspace, plus two hashes of 64K items
/// or more.
fn agreeing_keysizes(report: &ScanReport) -> Vec<KeysizeHistogram> {
    let types = [
        (KeyType::String, "strings", "sizes"),
        (KeyType::List, "lists", "items"),
        (KeyType::Set, "sets", "items"),
        (KeyType::ZSet, "zsets", "items"),
        (KeyType::Hash, "hashes", "items"),
    ];
    let mut out = Vec::new();
    for (t, name, unit) in types {
        let est: u64 = report
            .patterns
            .iter()
            .filter(|p| p.key_type == Some(t))
            .map(|p| p.est_keys)
            .sum();
        if est == 0 {
            continue;
        }
        let mut bins = vec![(64, est)];
        if t == KeyType::Hash {
            bins.push((1 << 16, 2));
        }
        out.push(KeysizeHistogram {
            key_type: name.into(),
            unit: unit.into(),
            bins,
        });
    }
    out
}

/// The fixture report with a full server context: dataset.bytes 92% of used memory,
/// exact key counts, and keysizes that agree with the sample.
fn report_with_context() -> ScanReport {
    let mut r = fixtures::report(true);
    let dataset = (r.server.used_memory as f64 * 0.92) as u64;
    let keysizes = agreeing_keysizes(&r);
    r.server_context = Some(ServerContext::from(ServerContextParts {
        keyspace: Some(KeyspaceInfo {
            db: 0,
            keys: 7_920_000,
            expires: 5_100_000,
            avg_ttl_ms: 11_100_000,
            subexpiry: Some(0),
        }),
        memory: Some(MemoryStats {
            dataset_bytes: Some(dataset),
            overhead_total: Some(412 << 20),
            keys_count: Some(7_920_000),
            bytes_per_key: Some(1_960),
            fragmentation: Some(1.08),
        }),
        keysizes: Some(keysizes),
        missing: Vec::new(),
    }));
    assert!(bias_warnings(&r).is_empty(), "{:?}", bias_warnings(&r));
    r
}

/// MEMORY STATS denied, and keysizes that count every key as a hash while the sample is
/// mostly strings: a sampling-bias warning.
fn report_with_bias_and_gap() -> ScanReport {
    let mut r = fixtures::report(true);
    r.server_context = Some(ServerContext::from(ServerContextParts {
        keyspace: Some(KeyspaceInfo {
            db: 0,
            keys: 7_920_000,
            expires: 5_100_000,
            avg_ttl_ms: 11_100_000,
            subexpiry: None,
        }),
        memory: None,
        keysizes: Some(vec![KeysizeHistogram {
            key_type: "hashes".into(),
            unit: "items".into(),
            bins: vec![(4, 7_920_000)],
        }]),
        missing: vec![ContextGap::MemoryStatsDenied],
    }));
    assert!(!bias_warnings(&r).is_empty());
    r
}

fn scanned(report: ScanReport, perms: PermissionReport) -> App {
    let mut app = App::new(fixtures::profiles());
    app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
    let id = connect_selected(&mut app);
    app.apply(Msg::Connected {
        id,
        scan: scan_of(&app),
        server: Some(report.server.clone()),
        perms,
        target: ScanTarget::Primary,
    });
    app.apply(Msg::ScanDone {
        id,
        scan: scan_of(&app),
        report: Box::new(report),
    });
    // A connect lands on Overview; these look at Browse › Keys.
    super::app::views::testing::show(&mut app, super::app::Screen::Patterns);
    app
}

#[test]
fn connections_detail_shows_the_provider_notice() {
    let mut app = App::new(serverless_profiles());
    app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
    let t = render(&app, 120, 36);
    let text = buffer_lines(&t).join("\n");
    assert!(
        text.contains("ElastiCache Serverless restricts MEMORY and OBJECT"),
        "{text}"
    );
    assert!(
        text.contains("PROD · saved profile · ElastiCache Serverless"),
        "{text}"
    );
    insta::assert_snapshot!(t.backend());
}

#[test]
fn connecting_and_scanning_panels_show_the_provider_notice() {
    let mut app = App::new(serverless_profiles());
    let id = connect_selected(&mut app);
    let t = render(&app, 120, 36);
    assert!(
        buffer_lines(&t)
            .iter()
            .any(|l| l.contains("▲ ElastiCache Serverless restricts")),
        "{:?}",
        buffer_lines(&t)
    );
    insta::assert_snapshot!("connecting_panel_provider_notice", t.backend());

    let mut perms = fixtures::perms_ok();
    for c in &mut perms.checks {
        if c.label == crate::model::PERM_MEMORY || c.label == crate::model::PERM_FREQ {
            c.status = PermStatus::Denied("ERR unknown command 'MEMORY'".into());
        }
    }
    let connected = |app: &mut App, perms: PermissionReport| {
        app.apply(Msg::Connected {
            id,
            scan: scan_of(app),
            server: Some(fixtures::server()),
            perms,
            target: ScanTarget::Primary,
        })
    };
    connected(&mut app, perms.clone());
    // `r`, `enter`: a Quick analysis, so the Keys section shows its progress.
    app.handle_key(ch('r'));
    app.handle_key(key(KeyCode::Enter));
    connected(&mut app, perms);
    app.apply(Msg::ScanProgress {
        id,
        scan: scan_of(&app),
        progress: ScanProgress {
            sampled: 1_240,
            target: 10_000,
            done: false,
        },
    });
    let t = render(&app, 120, 36);
    insta::assert_snapshot!("scanning_panel_provider_notice", t.backend());
}

#[test]
fn no_notice_without_a_provider_that_degrades() {
    let mut profiles = fixtures::profiles();
    profiles[0].provider = Some(Provider::Elasticache);
    let mut app = App::new(profiles);
    let text = buffer_lines(&render(&app, 120, 36)).join("\n");
    assert!(text.contains("saved profile · ElastiCache"), "{text}");
    assert!(!text.contains('▲'), "{text}");
    connect_selected(&mut app);
    let text = buffer_lines(&render(&app, 120, 36)).join("\n");
    assert!(!text.contains('▲'), "{text}");
}

#[test]
fn patterns_server_context_strip() {
    let app = scanned(report_with_context(), perms_all());
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    // On the table's bottom border, which stays a border around it.
    let strip = lines
        .iter()
        .find(|l| l.contains("sample explains"))
        .expect("the strip");
    assert!(strip.starts_with("└─ "), "{strip}");
    assert!(strip.ends_with("┘"), "{strip}");
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_server_context_strip_narrow() {
    let app = scanned(report_with_context(), perms_all());
    let t = render(&app, 80, 24);
    let lines = buffer_lines(&t);
    let strip = lines
        .iter()
        .find(|l| l.contains("sample explains"))
        .expect("the strip");
    // Cut with an ellipsis, borders kept.
    assert!(strip.contains('…'), "{strip}");
    assert!(strip.starts_with("└─ ") && strip.ends_with("─┘"), "{strip}");
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_server_context_bias_and_missing_memory_stats() {
    let app = scanned(report_with_bias_and_gap(), perms_stats_denied());
    let t = render(&app, 120, 36);
    let text = buffer_lines(&t).join("\n");
    assert!(text.contains("▲ possible sampling bias"), "{text}");
    assert!(text.contains("no MEMORY STATS: denied"), "{text}");
    assert!(
        !text.contains("NOPERM"),
        "the server's text stays out: {text}"
    );
    insta::assert_snapshot!(t.backend());
}

#[test]
fn no_strip_without_a_server_context() {
    let with = buffer_lines(&render(
        &scanned(report_with_context(), perms_all()),
        120,
        36,
    ));
    let without = buffer_lines(&render(
        &scanned(fixtures::report(true), fixtures::perms_ok()),
        120,
        36,
    ));
    let differ: Vec<usize> = (0..with.len()).filter(|&i| with[i] != without[i]).collect();
    // Only the strip's border row differs.
    assert_eq!(differ.len(), 1, "{differ:?}");
    let row = &without[differ[0]];
    assert!(
        row.starts_with("└") && row.chars().skip(1).take(10).all(|c| c == '─'),
        "{row}"
    );
}

#[test]
fn connections_memory_stats_only_denied() {
    let mut app = scanned(report_with_context(), perms_stats_denied());
    app.handle_key(ch('0'));
    let t = render(&app, 120, 36);
    let text = buffer_lines(&t).join("\n");
    assert!(text.contains("✗ MEMORY STATS"), "{text}");
    assert!(
        text.contains("MEMORY STATS is denied, so Patterns can't say how much"),
        "{text}"
    );
    insta::assert_snapshot!(t.backend());
}
