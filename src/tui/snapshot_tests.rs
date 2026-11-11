//! insta snapshots of rendered screens, drawn with ratatui's `TestBackend`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use super::app::{App, ConnId, Effect, Msg, ScanId};
use super::fixtures;
use super::tree::{Root, Row};
use super::ui;
use crate::conn::ScanTarget;
use crate::conn::docker::DiscoveredAuth;
use crate::model::{PermissionReport, ScanReport};
use crate::scan::ScanProgress;

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

fn connect_selected(app: &mut App) -> u64 {
    let effects = app.handle_key(key(KeyCode::Enter));
    match effects.as_slice() {
        [Effect::Connect { id, .. }] => *id,
        other => panic!("expected a Connect effect, got {other:?}"),
    }
}

/// The current session's run, for messages that should be accepted.
fn scan_of(app: &App) -> ScanId {
    app.session.as_ref().unwrap().scan
}

/// Connected to prod-cache-01 with `perms`, and `report` finished scanning.
fn scanned_with(report: ScanReport, perms: PermissionReport) -> (App, ConnId) {
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
        report,
    });
    (app, id)
}

/// Connected to prod-cache-01 with a finished scan.
fn scanned(sizes: bool) -> App {
    let perms = if sizes {
        fixtures::perms_ok()
    } else {
        fixtures::perms_memory_denied()
    };
    scanned_with(fixtures::report(sizes), perms).0
}

/// The history lookup came back: with the fixture diff, or with no snapshot.
fn set_history(app: &mut App, id: ConnId, diff: Option<crate::diff::ReportDiff>) {
    let taken_at_unix = app
        .session
        .as_ref()
        .unwrap()
        .report
        .as_ref()
        .unwrap()
        .taken_at_unix;
    app.apply(Msg::History {
        id,
        scan: scan_of(app),
        taken_at_unix,
        result: Ok(diff),
    });
}

/// A finished scan with hot keys and a diff against yesterday's snapshot.
fn scanned_with_history(sizes: bool) -> App {
    let mut report = fixtures::report(sizes);
    report.hot_keys = fixtures::hot_keys();
    let perms = if sizes {
        fixtures::perms_ok()
    } else {
        fixtures::perms_memory_denied()
    };
    let (mut app, id) = scanned_with(report, perms);
    set_history(&mut app, id, Some(fixtures::history(sizes)));
    app
}

fn press(app: &mut App, keys: &str) {
    for c in keys.chars() {
        app.handle_key(ch(c));
    }
}

/// Move the Patterns selection to the row labelled `label`.
fn select_pattern(app: &mut App, label: &str) {
    let report = app.session.as_ref().unwrap().report.clone().unwrap();
    let rows = app.pattern_rows();
    let at = rows
        .iter()
        .position(|r| match *r {
            Row::Group { root } => {
                matches!(&app.patterns.tree.roots[root], Root::Group(g) if g.label == label)
            }
            Row::Pattern { idx, .. } => report.patterns[idx].pattern == label,
        })
        .unwrap_or_else(|| panic!("no row {label}"));
    while app.patterns.sel < at {
        app.handle_key(ch('j'));
    }
}

/// Docker containers whose discovery found a password, no auth, auth it could not
/// satisfy, and nothing conclusive. The password is fixture data and never on screen.
fn docker_auth_app() -> App {
    let mut app = App::new(fixtures::profiles());
    let mut with_password = fixtures::docker_instance();
    with_password.container = "pw-redis-1".into();
    with_password.profile.name = "pw-redis-1".into();
    with_password.profile.auth = crate::model::Auth::Keychain;
    with_password.credentials = Some(crate::model::Credentials {
        username: None,
        password: secrecy::SecretString::from("Fixture-Container-Pw"),
    });
    with_password.auth = DiscoveredAuth::Password;
    app.apply(Msg::DockerFound(vec![
        with_password,
        fixtures::docker_instance(),
        fixtures::docker_instance_auth("acl-valkey-1", 6391, DiscoveredAuth::Required),
        fixtures::docker_instance_auth("odd-valkey-1", 6392, DiscoveredAuth::Unknown),
    ]));
    app
}

/// Select the entry called `name` on the Connections view.
fn select_entry(app: &mut App, name: &str) {
    for _ in 0..app.entries.len() + 1 {
        if app.selected_entry().is_some_and(|e| e.profile.name == name) {
            return;
        }
        app.handle_key(ch('j'));
    }
    panic!("no entry {name}");
}

#[test]
fn connections_docker_auth() {
    let mut app = docker_auth_app();
    select_entry(&mut app, "acl-valkey-1");
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    let row = |name: &str| {
        lines
            .iter()
            .find(|l| l.contains(&format!("{name} ")) && l.contains("127.0.0.1"))
            .unwrap_or_else(|| panic!("no row {name}"))
            .clone()
    };
    // The list's AUTH column shows what discovery found.
    for (name, label) in [
        ("pw-redis-1", "password"),
        ("myapp-redis-1", "none"),
        ("acl-valkey-1", "required"),
        ("odd-valkey-1", "unknown"),
    ] {
        let r = row(name);
        assert!(r.contains(&format!(" {label} ")), "{name}: {r}");
    }
    for line in &lines {
        assert!(
            !line.contains("Fixture-Container-Pw"),
            "password on screen: {line}"
        );
    }
    insta::assert_snapshot!(t.backend());
}

#[test]
fn docker_auth_detail_panel() {
    let mut app = docker_auth_app();
    for (name, auth, cred) in [
        (
            "pw-redis-1",
            "password (default user)",
            "from the container, memory only",
        ),
        ("myapp-redis-1", "none", "none"),
        (
            "acl-valkey-1",
            "required",
            "not found: save it as a profile with credentials",
        ),
        ("odd-valkey-1", "unknown", "could not tell"),
    ] {
        select_entry(&mut app, name);
        let lines = buffer_lines(&render(&app, 140, 40));
        let field = |key: &str| {
            lines
                .iter()
                .find_map(|l| {
                    l.split_once(&format!(" {key}  "))
                        .map(|(_, v)| v.trim_start().to_string())
                })
                .unwrap_or_else(|| panic!("{name}: no {key} field"))
        };
        assert!(
            field("Auth").starts_with(&format!("{auth}  ")),
            "{name}: {}",
            field("Auth")
        );
        assert!(
            field("Credential").starts_with(&format!("{cred}  ")),
            "{name}: {}",
            field("Credential")
        );
        for line in &lines {
            assert!(
                !line.contains("Fixture-Container-Pw"),
                "password on screen: {line}"
            );
        }
    }
}

#[test]
fn connections_view() {
    let mut app = App::new(fixtures::profiles());
    app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
    // Select staging-cache and connect: MEMORY USAGE is denied for this user.
    app.handle_key(ch('j'));
    let id = connect_selected(&mut app);
    app.apply(Msg::Connected {
        id,
        scan: scan_of(&app),
        server: Some(crate::model::ServerInfo {
            version: "7.2.5".into(),
            used_memory: 2_254_857_830,
            maxmemory: 0,
            total_keys: 85_964,
            ..fixtures::server()
        }),
        perms: fixtures::perms_memory_denied(),
        target: ScanTarget::Primary,
    });
    app.apply(Msg::ScanProgress {
        id,
        scan: scan_of(&app),
        progress: ScanProgress {
            sampled: 12_400,
            target: 85_964,
            done: false,
        },
    });
    app.handle_key(ch('0'));
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn scanning_progress_header() {
    let mut app = App::new(fixtures::profiles());
    let id = connect_selected(&mut app);
    app.apply(Msg::Connected {
        id,
        scan: scan_of(&app),
        server: Some(fixtures::server()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Primary,
    });
    app.apply(Msg::ScanProgress {
        id,
        scan: scan_of(&app),
        progress: ScanProgress {
            sampled: 12_400,
            target: 85_964,
            done: false,
        },
    });
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(lines[1].contains("Sampled 12,400 / 85,964"), "{}", lines[1]);
}

#[test]
fn add_preview_masks_password() {
    let mut app = App::new(fixtures::profiles());
    app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
    app.handle_key(ch('a'));
    app.handle_paste(fixtures::PROD_URL.to_string());
    let t = render(&app, 120, 36);
    for line in buffer_lines(&t) {
        assert!(!line.contains("Sup3rS3cret"), "password on screen: {line}");
    }
    // Narrow terminals must not leak it either.
    let narrow = render(&app, 60, 20);
    for line in buffer_lines(&narrow) {
        assert!(!line.contains("Sup3r"), "password on screen: {line}");
    }
    insta::assert_snapshot!(t.backend());
}

#[test]
fn add_password_step() {
    let mut app = App::new(vec![]);
    app.handle_key(ch('a'));
    app.handle_paste("redis://lens@staging-cache.internal:6379".into());
    app.handle_key(key(KeyCode::Enter));
    for c in "hunter2".chars() {
        app.handle_key(ch(c));
    }
    let t = render(&app, 100, 30);
    let lines = buffer_lines(&t);
    assert!(lines.iter().all(|l| !l.contains("hunter2")));
    assert!(lines.last().unwrap().contains("password"));
    let last = lines.last().unwrap();
    assert!(last.contains("••••••"));
    assert!(
        !last.contains("•••••••"),
        "bullets must not reveal the password length"
    );
}

#[test]
fn patterns_with_sizes() {
    let mut app = scanned(true);
    select_pattern(&mut app, "user:{id}:cart");
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

/// A finished scan of prod-cache-01 with a diff, then a connect to staging-cache that
/// has published partial results.
fn partial_after_previous_profile() -> App {
    let mut app = scanned_with_history(true);
    app.handle_key(ch('0'));
    app.handle_key(ch('j'));
    let id = connect_selected(&mut app);
    app.apply(Msg::Connected {
        id,
        scan: scan_of(&app),
        server: Some(fixtures::server()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Primary,
    });
    let partial = fixtures::partial(true);
    app.apply(Msg::ScanProgress {
        id,
        scan: scan_of(&app),
        progress: ScanProgress {
            sampled: partial.sampled,
            target: partial.target,
            done: false,
        },
    });
    app.apply(Msg::ScanPartial {
        id,
        scan: scan_of(&app),
        partial,
    });
    app
}

#[test]
fn patterns_partial() {
    let app = partial_after_previous_profile();
    assert_eq!(app.session.as_ref().unwrap().profile.name, "staging-cache");
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(
        lines[3].contains("partial · 3,200 / 100,000 sampled"),
        "{}",
        lines[3]
    );
    // The previous profile's diff is not shown against partial data.
    assert!(!lines[4].contains("Δ"), "{}", lines[4]);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn partial_marker_survives_narrow_terminals() {
    let app = partial_after_previous_profile();
    let lines = buffer_lines(&render(&app, 80, 24));
    assert!(
        lines[3].contains("partial · 3,200 / 100,000"),
        "{}",
        lines[3]
    );
}

#[test]
fn other_views_wait_while_partial() {
    let mut app = partial_after_previous_profile();
    for (view, title) in [('2', "TTL"), ('3', "Diff"), ('4', "Hot keys")] {
        app.handle_key(ch(view));
        let lines = buffer_lines(&render(&app, 120, 36));
        assert!(lines[3].contains(title), "{}", lines[3]);
        assert!(
            lines[5].contains("Sampled 3,200 / 100,000 keys"),
            "{view}: {}",
            lines[5]
        );
        // Nothing from prod-cache-01's report or diff.
        for line in &lines[3..] {
            assert!(!line.contains("cache:search"), "{view}: {line}");
        }
    }
}

#[test]
fn failed_scan_drops_partial_results_from_the_screen() {
    let mut app = partial_after_previous_profile();
    let id = app.session.as_ref().unwrap().id;
    app.apply(Msg::ScanFailed {
        id,
        scan: scan_of(&app),
        error: "connection reset by peer".into(),
    });
    app.handle_key(ch('x')); // clear the error notice
    let lines = buffer_lines(&render(&app, 120, 36));
    assert!(
        lines[5].contains("✗ connection reset by peer"),
        "{}",
        lines[5]
    );
    for line in &lines {
        assert!(!line.contains("partial"), "{line}");
        assert!(!line.contains("cache:search"), "{line}");
    }
}

#[test]
fn empty_partial_says_nothing_sampled_yet() {
    let mut app = App::new(fixtures::profiles());
    let id = connect_selected(&mut app);
    app.apply(Msg::Connected {
        id,
        scan: scan_of(&app),
        server: Some(fixtures::server()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Primary,
    });
    let mut partial = (*fixtures::partial(true)).clone();
    partial.patterns.clear();
    partial.sampled = 0;
    app.apply(Msg::ScanPartial {
        id,
        scan: scan_of(&app),
        partial: std::sync::Arc::new(partial),
    });
    let lines = buffer_lines(&render(&app, 120, 36));
    assert!(
        lines[3].contains("partial · 0 / 100,000 sampled"),
        "{}",
        lines[3]
    );
    assert!(
        lines.iter().any(|l| l.contains("No keys sampled yet.")),
        "{lines:#?}"
    );
}

#[test]
fn patterns_without_sizes() {
    let mut app = scanned(false);
    select_pattern(&mut app, "cache:search:{hash}");
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_80x24() {
    let mut app = scanned(true);
    select_pattern(&mut app, "queue:emails");
    let t = render(&app, 80, 24);
    insta::assert_snapshot!(t.backend());
}

/// A pattern whose glob has characters that are not safe in a shell: the detail panel
/// says why there is no command instead of showing one.
#[test]
fn patterns_detail_without_a_safe_command() {
    let mut report = fixtures::report(true);
    report.patterns[0].pattern = "evil:x'$(touch pwned)'".into();
    let (mut app, _) = scanned_with(report, fixtures::perms_ok());
    select_pattern(&mut app, "evil:x'$(touch pwned)'");
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_filter_applied() {
    let mut app = scanned(true);
    app.handle_key(ch('/'));
    for c in "user".chars() {
        app.handle_key(ch(c));
    }
    app.handle_key(key(KeyCode::Enter));
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_with_delta() {
    let mut app = scanned_with_history(true);
    select_pattern(&mut app, "cache:search:{hash}");
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(lines[4].contains("Δ"), "{}", lines[4]);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_delta_without_sizes_counts_keys() {
    let app = scanned_with_history(false);
    let lines = buffer_lines(&render(&app, 120, 36));
    let row = lines
        .iter()
        .find(|l| l.contains("cache:search:{hash}"))
        .unwrap();
    // 1.12M keys now against 860K before.
    assert!(row.contains("+260K"), "{row}");
}

#[test]
fn patterns_without_history_have_no_delta_column() {
    let lines = buffer_lines(&render(&scanned(true), 120, 36));
    assert!(lines.iter().all(|l| !l.contains('Δ')));
}

#[test]
fn ttl_view() {
    let mut app = scanned_with_history(true);
    press(&mut app, "2jj");
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn diff_view_with_history() {
    let mut app = scanned_with_history(true);
    press(&mut app, "3j");
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn diff_view_without_history() {
    let (mut app, id) = scanned_with(fixtures::report(true), fixtures::perms_ok());
    set_history(&mut app, id, None);
    app.handle_key(ch('3'));
    let t = render(&app, 120, 36);
    let text = buffer_lines(&t).join("\n");
    assert!(text.contains("No earlier snapshot for prod-cache-01."));
    assert!(text.contains("Press s to save one; the next scan compares against it."));
    insta::assert_snapshot!(t.backend());
}

#[test]
fn hot_keys_view() {
    let mut app = scanned_with_history(true);
    press(&mut app, "4j");
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn hot_keys_view_without_lfu() {
    let (mut app, _) = scanned_with(fixtures::report_lru(), fixtures::perms_freq_skipped());
    app.handle_key(ch('4'));
    let t = render(&app, 120, 36);
    let text = buffer_lines(&t).join("\n");
    assert!(text.contains("The eviction policy is allkeys-lru"));
    insta::assert_snapshot!(t.backend());
}

/// The foreground colour of the first cell on the row containing `row_text` whose
/// symbol starts `cell_text`, searching from column `from`.
fn colour_at(t: &Terminal<TestBackend>, row_text: &str, cell_text: &str, from: u16) -> Color {
    let lines = buffer_lines(t);
    let y = lines.iter().position(|l| l.contains(row_text)).unwrap() as u16;
    let buf = t.backend().buffer();
    let x = (from..buf.area.width)
        .find(|&x| buf[(x, y)].symbol() == cell_text)
        .unwrap_or_else(|| panic!("no {cell_text} on row {row_text}"));
    buf[(x, y)].fg
}

#[test]
fn change_and_verdict_colours() {
    use crate::tui::ui::style::{AMBER, DIM, RED, TEAL};
    let mut app = scanned_with_history(true);
    let t = render(&app, 120, 36);
    // Δ column: new is red, +232M amber, a shrink teal, unchanged dim.
    assert_eq!(colour_at(&t, "tmp:import:{ulid}", "+", 100), RED);
    assert_eq!(colour_at(&t, "queue:emails", "+", 100), AMBER);
    assert_eq!(colour_at(&t, "rl:{ip}:{id}", "-", 100), TEAL);
    assert_eq!(colour_at(&t, "lock:{var}", "·", 100), DIM);
    app.handle_key(ch('2'));
    let t = render(&app, 120, 36);
    // Warning rows have a red "none" bucket and verdict; stable ones amber and dim.
    assert_eq!(colour_at(&t, "user:{id}:cart", "▅", 50), RED);
    assert_eq!(colour_at(&t, "user:{id}:cart", "m", 90), RED);
    assert_eq!(colour_at(&t, "feature:{var}", "█", 50), AMBER);
    assert_eq!(colour_at(&t, "feature:{var}", "p", 90), DIM);
    assert_eq!(colour_at(&t, "session:{uuid}", "█", 50), TEAL);
    app.handle_key(ch('3'));
    let t = render(&app, 120, 36);
    // Diff bars: growth of 300M or more red, smaller growth amber, shrink teal.
    assert_eq!(colour_at(&t, "cache:search:{hash}", "█", 80), RED);
    assert_eq!(colour_at(&t, "queue:emails", "█", 80), AMBER);
    assert_eq!(colour_at(&t, "rl:{ip}:{id}", "█", 80), TEAL);
}

#[test]
fn every_tab_at_60x20_and_200x60() {
    let (denied, _) = scanned_with(fixtures::report(true), fixtures::perms_freq_denied());
    let (mut no_history, id) = scanned_with(fixtures::report_lru(), fixtures::perms_ok());
    set_history(&mut no_history, id, None);
    let mut failed = scanned(true);
    let id = failed.session.as_ref().unwrap().id;
    let taken_at_unix = failed.report().unwrap().taken_at_unix;
    failed.apply(Msg::History {
        id,
        scan: scan_of(&failed),
        taken_at_unix,
        result: Err("could not find a data directory for this user".into()),
    });
    let mut connecting = App::new(fixtures::profiles());
    connect_selected(&mut connecting);
    let mut apps = vec![
        App::new(vec![]),
        connecting,
        scanned(true),
        scanned_with_history(true),
        scanned_with_history(false),
        denied,
        no_history,
        failed,
    ];
    for app in &mut apps {
        for view in ['0', '1', '2', '3', '4'] {
            app.handle_key(ch(view));
            // Move to the end of each list too, so scrolling is exercised.
            for _ in 0..60 {
                app.handle_key(ch('j'));
            }
            assert_eq!(
                app.view,
                crate::tui::app::View::ALL[view as usize - '0' as usize]
            );
            for (w, h) in [(60, 20), (200, 60)] {
                render(app, w, h);
            }
        }
    }
    // Filtering the Patterns view swallows digits, so check that state on its own.
    let mut filtering = scanned_with_history(true);
    press(&mut filtering, "1/zz");
    for (w, h) in [(60, 20), (200, 60)] {
        render(&filtering, w, h);
    }
}

#[test]
fn small_terminals_do_not_panic() {
    let mut apps = vec![App::new(vec![]), scanned(true), scanned(false)];
    // History and hot keys, with and without sizes.
    apps.push(scanned_with_history(true));
    apps.push(scanned_with_history(false));
    // No earlier snapshot, and history whose other scan used a different MATCH.
    let (mut no_history, id) = scanned_with(fixtures::report(true), fixtures::perms_ok());
    set_history(&mut no_history, id, None);
    apps.push(no_history);
    let mut other = fixtures::previous_report(true);
    other.match_glob = Some("session:*".into());
    let mut report = fixtures::report(true);
    report.hot_keys = fixtures::hot_keys();
    let unsure = crate::diff::diff(&other, &report);
    assert!(
        unsure
            .patterns
            .iter()
            .any(|p| p.cause == crate::diff::Cause::Unknown)
    );
    let (mut unknown, id) = scanned_with(report, fixtures::perms_ok());
    set_history(&mut unknown, id, Some(unsure));
    apps.push(unknown);
    // Every reason the Hot keys view can be empty.
    apps.push(scanned_with(fixtures::report_lru(), fixtures::perms_freq_skipped()).0);
    apps.push(scanned_with(fixtures::report(true), fixtures::perms_freq_denied()).0);
    let mut empty = fixtures::report(true);
    empty.sampled_keys = 0;
    empty.patterns.clear();
    apps.push(scanned_with(empty, fixtures::perms_ok()).0);
    let mut adding = App::new(fixtures::profiles());
    adding.handle_key(ch('a'));
    adding.handle_paste(fixtures::PROD_URL.into());
    apps.push(adding);
    for app in &mut apps {
        for view in ['0', '1', '2', '3', '4'] {
            if app.add.is_none() {
                app.handle_key(ch(view));
            }
            for (w, h) in [(60, 20), (61, 21), (80, 24), (40, 10), (1, 1), (200, 60)] {
                render(app, w, h);
            }
        }
    }
}

#[test]
fn keychain_hint_and_scan_target_are_drawn() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let mut app = App::new(fixtures::profiles());
    app.apply(Msg::TickAt(t0));
    app.handle_key(ch('j'));
    let id = connect_selected(&mut app);
    let text = |app: &App| buffer_lines(&render(app, 140, 40)).join("\n");
    assert!(!text(&app).contains("Waiting for the OS keychain"));
    app.apply(Msg::TickAt(t0 + Duration::from_secs(3)));
    let t = text(&app);
    assert!(t.matches("Waiting for the OS keychain").count() >= 2, "{t}");
    assert!(t.contains("choose Always Allow."), "{t}");

    let ep = crate::model::Endpoint {
        host: "10.0.1.21".into(),
        port: 6379,
        tls: false,
    };
    app.apply(Msg::Connected {
        id,
        scan: scan_of(&app),
        server: Some(fixtures::server()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Replica(ep),
    });
    let t = text(&app);
    assert!(t.contains("replica 10.0.1.21:6379"), "{t}");
    assert!(!t.contains("Waiting for the OS keychain"), "{t}");
    app.handle_key(ch('0'));
    let t = text(&app);
    assert!(t.contains("scanning replica 10.0.1.21:6379"), "{t}");
}

const LONG_NAME: &str = "audit-trail-{id}-with-an-exceedingly-long-descriptive-pattern-name-{uuid}";

/// A finished scan whose biggest pattern (the first row, selected) has a very long name.
fn long_name_app() -> App {
    let mut report = fixtures::report(true);
    report.patterns[0].pattern = LONG_NAME.into();
    scanned_with(report, fixtures::perms_ok()).0
}

fn tick(app: &mut App, n: usize) {
    for _ in 0..n {
        app.apply(Msg::Tick);
    }
}

#[test]
fn patterns_narrow_rows_do_not_wrap() {
    let mut app = long_name_app();
    let before = buffer_lines(&render(&app, 60, 24));
    // The table's rows, between its header and its closing border.
    let end = before.iter().position(|l| l.starts_with('└')).unwrap();
    let table = &before[5..end];
    let report = app.report().unwrap();
    assert_eq!(table.len(), 7, "{before:#?}");
    assert!(report.patterns.len() >= table.len());
    // One line for the long name, cut with `…`, and none for its tail.
    assert!(table[0].starts_with("│   audit-t"), "{}", table[0]);
    assert!(table[0].contains('…'), "{}", table[0]);
    assert!(
        !table.iter().any(|l| l.contains("descriptive")),
        "{table:#?}"
    );
    for l in &before {
        assert_eq!(l.chars().count(), 60, "{l}");
    }
    // Scrolled it is still one line, in the same place, and not wider.
    tick(&mut app, 25);
    let after = buffer_lines(&render(&app, 60, 24));
    assert_ne!(after[5], before[5]);
    // Only the name scrolls: the indent and the (empty) fold mark stay in front.
    assert!(after[5].starts_with("│   …"), "{}", after[5]);
    assert!(after[6].starts_with("│ ▾ cache:"), "{}", after[6]);
    assert!(
        !after.iter().any(|l| l.contains("descriptive")),
        "{after:#?}"
    );
    for l in &after {
        assert_eq!(l.chars().count(), 60, "{l}");
    }
}

#[test]
fn patterns_long_name_mid_scroll() {
    let mut app = long_name_app();
    tick(&mut app, 10 + 2 * 12);
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

const LONG_GROUP: &str = "cache-of-a-service-with-an-exceedingly-long-descriptive-name";

/// A finished scan whose `cache:*` group (second row) has a very long label.
fn long_group_app() -> App {
    let mut report = fixtures::report(true);
    for p in &mut report.patterns {
        if let Some(rest) = p.pattern.strip_prefix("cache:") {
            p.pattern = format!("{LONG_GROUP}:{rest}");
        }
    }
    let mut app = scanned_with(report, fixtures::perms_ok()).0;
    select_pattern(&mut app, &format!("{LONG_GROUP}:*"));
    app
}

#[test]
fn patterns_long_group_name_mid_scroll() {
    let mut app = long_group_app();
    tick(&mut app, 10 + 2 * 12);
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    // The fold mark stays visible; the label scrolls after it.
    assert!(lines[6].starts_with("│ ▾ …"), "{}", lines[6]);
    // Collapsed, the mark changes and stays visible too.
    app.handle_key(key(KeyCode::Left));
    tick(&mut app, 10 + 2 * 12);
    let lines = buffer_lines(&render(&app, 120, 36));
    assert!(lines[6].starts_with("│ ▸ …"), "{}", lines[6]);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_long_child_name_keeps_its_indent() {
    let mut app = long_group_app();
    app.handle_key(key(KeyCode::Right));
    tick(&mut app, 10 + 2 * 12);
    let lines = buffer_lines(&render(&app, 120, 36));
    assert!(lines[7].starts_with("│     …"), "{}", lines[7]);
}

#[test]
fn patterns_sorted_by_keys() {
    let mut app = scanned(true);
    select_pattern(&mut app, "user:{id}:cart");
    app.handle_key(ch('o'));
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(lines[3].contains("sorted by keys"), "{}", lines[3]);
    assert!(lines[4].contains("KEYS▼"), "{}", lines[4]);
    assert!(!lines[4].contains("MEMORY▼"), "{}", lines[4]);
    // The selection stayed on the same pattern, now further down.
    assert!(
        lines.iter().any(|l| l.starts_with("┌─ user:{id}:cart ")),
        "{lines:#?}"
    );
    assert!(lines[12].contains("user:{id}:cart"), "{}", lines[12]);
    assert!(lines.last().unwrap().contains("Sorted by keys"));
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_sorted_by_name_shows_an_up_arrow() {
    let mut app = scanned(true);
    press(&mut app, "oooo");
    let lines = buffer_lines(&render(&app, 120, 36));
    assert!(lines[3].contains("sorted by name"), "{}", lines[3]);
    assert!(lines[4].starts_with("│ PATTERN ▲"), "{}", lines[4]);
    assert!(!lines[4].contains('▼'), "{}", lines[4]);
}

/// The table of the Patterns view (title, header, rows) rendered `w` wide.
fn table_at(app: &App, w: u16) -> Vec<String> {
    let lines = buffer_lines(&render(app, w, 24));
    let end = lines[3..].iter().position(|l| l.starts_with('└')).unwrap() + 3;
    lines[3..=end].to_vec()
}

/// Asserts the header shows exactly `shown` of the optional columns and that the
/// share percentage lines up between the header and every row.
fn check_columns(table: &[String], w: u16, shown: &[&str]) {
    let head = &table[1];
    for col in ["SHARE", "AVG", "Δ"] {
        assert_eq!(
            head.contains(col),
            shown.contains(&col),
            "{w}: {col} in {head}"
        );
    }
    let pct = |l: &str| l.chars().position(|c| c == '%');
    let at = pct(head).unwrap();
    for row in &table[2..table.len() - 1] {
        if row.trim_matches(|c| c == '│' || c == ' ').is_empty() {
            continue;
        }
        assert_eq!(pct(row), Some(at), "{w}: {row}\n{head}");
    }
    for l in table {
        assert_eq!(l.chars().count(), usize::from(w), "{l}");
    }
}

fn narrow_steps(app: &App, steps: &[(u16, &[&str])]) -> String {
    let mut out = String::new();
    for (w, shown) in steps {
        let table = table_at(app, *w);
        check_columns(&table, *w, shown);
        out.push_str(&format!("{w} wide:\n{}\n\n", table.join("\n")));
    }
    out
}

#[test]
fn patterns_narrow_columns() {
    let mut app = scanned(true);
    select_pattern(&mut app, "user:{id}:cart");
    // 72 is the narrowest with every column; then the share bar goes (71), then the
    // average size (63); below 55 the row is cut at the right edge.
    let snap = narrow_steps(
        &app,
        &[
            (72, &["SHARE", "AVG"]),
            (71, &["AVG"]),
            (64, &["AVG"]),
            (63, &[]),
            (55, &[]),
        ],
    );
    let cut = table_at(&app, 50);
    // Every column is hidden and the name is at its narrowest: the row is cut.
    assert!(cut[1].starts_with("│ PATTERN    TYPE"), "{}", cut[1]);
    assert!(!cut[1].contains("NO TTL"), "{}", cut[1]);
    insta::assert_snapshot!(format!("{snap}50 wide:\n{}", cut.join("\n")));
}

#[test]
fn patterns_narrow_columns_with_delta() {
    let mut app = scanned_with_history(true);
    select_pattern(&mut app, "user:{id}:cart");
    // 80 is the narrowest with every column; the share bar goes first (79), then Δ
    // (71), then the average size (63).
    let snap = narrow_steps(
        &app,
        &[
            (80, &["SHARE", "AVG", "Δ"]),
            (79, &["AVG", "Δ"]),
            (72, &["AVG", "Δ"]),
            (71, &["AVG"]),
            (64, &["AVG"]),
            (63, &[]),
            (55, &[]),
        ],
    );
    insta::assert_snapshot!(snap);
}

// ---------------------------------------------------------------------------
// Wave 8: rescan, help overlay, copy, page size
// ---------------------------------------------------------------------------

/// A finished scan with history, the second row selected on `view`, then `r`: the
/// rescan is connected and scanning, with no results of its own yet.
fn rescanning(view: char) -> (App, ConnId, ScanId) {
    let mut app = scanned_with_history(true);
    press(&mut app, &format!("{view}j"));
    let id = app.session.as_ref().unwrap().id;
    let effects = app.handle_key(ch('r'));
    let [Effect::Connect { scan, .. }] = effects[..] else {
        panic!("unexpected effects {effects:?}");
    };
    app.apply(Msg::Connected {
        id,
        scan,
        server: Some(fixtures::server()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Primary,
    });
    app.apply(Msg::ScanProgress {
        id,
        scan,
        progress: ScanProgress {
            sampled: 1_600,
            target: 100_000,
            done: false,
        },
    });
    (app, id, scan)
}

const RESCANNING: &str = "rescanning · previous results";
const RESCAN_FAILED: &str = "rescan failed · previous results";

#[test]
fn patterns_rescanning_shows_previous_results() {
    let (app, _, _) = rescanning('1');
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(lines[3].contains(RESCANNING), "{}", lines[3]);
    // The header says a scan runs; the Δ column of the old report stays.
    assert!(lines[0].contains("scanning"), "{}", lines[0]);
    assert!(lines[4].contains('Δ'), "{}", lines[4]);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn ttl_rescanning_shows_previous_results() {
    let (app, _, _) = rescanning('2');
    let t = render(&app, 120, 36);
    assert!(buffer_lines(&t)[3].contains(RESCANNING));
    insta::assert_snapshot!(t.backend());
}

#[test]
fn every_view_marks_previous_results_until_the_rescans_first_partial() {
    let (mut app, id, scan) = rescanning('1');
    let top = |app: &App, w| buffer_lines(&render(app, w, 36))[3].clone();
    for view in ['1', '2', '3', '4'] {
        app.handle_key(ch(view));
        assert!(top(&app, 120).contains(RESCANNING), "view {view}");
        // On a narrow terminal the marker is kept, the rest is cut.
        assert!(
            top(&app, 80).contains(RESCANNING),
            "view {view}: {}",
            top(&app, 80)
        );
    }
    // Diff without a snapshot, and Hot keys without LFU, mark it too.
    let (mut app2, id2) = scanned_with(fixtures::report_lru(), fixtures::perms_freq_skipped());
    set_history(&mut app2, id2, None);
    app2.handle_key(ch('3'));
    app2.handle_key(ch('r'));
    for view in ['3', '4'] {
        app2.handle_key(ch(view));
        assert!(top(&app2, 120).contains(RESCANNING), "view {view}");
    }
    // The first partial replaces them: Patterns shows partials, the rest wait.
    app.apply(Msg::ScanPartial {
        id,
        scan,
        partial: fixtures::partial(true),
    });
    app.handle_key(ch('1'));
    let line = top(&app, 120);
    assert!(line.contains("partial · 3,200 / 100,000 sampled"), "{line}");
    assert!(!line.contains("previous"), "{line}");
    for view in ['2', '3', '4'] {
        app.handle_key(ch(view));
        let text = buffer_lines(&render(&app, 120, 36)).join("\n");
        assert!(!text.contains("previous results"), "view {view}");
        assert!(text.contains("Sampled"), "view {view}");
    }
}

#[test]
fn patterns_rescan_failed_keeps_previous_results_marked() {
    let (mut app, id, scan) = rescanning('1');
    app.apply(Msg::ScanFailed {
        id,
        scan,
        error: "connection reset".into(),
    });
    let t = render(&app, 80, 24);
    let lines = buffer_lines(&t);
    assert!(lines[3].contains(RESCAN_FAILED), "{}", lines[3]);
    assert!(lines[23].contains("Scan failed: connection reset"));
    for view in ['2', '3', '4'] {
        app.handle_key(ch(view));
        assert!(
            buffer_lines(&render(&app, 120, 36))[3].contains(RESCAN_FAILED),
            "view {view}"
        );
    }
    insta::assert_snapshot!(t.backend());
}

#[test]
fn help_over_patterns_80x24() {
    let mut app = scanned_with_history(true);
    press(&mut app, "1j?");
    let t = render(&app, 80, 24);
    let text = buffer_lines(&t).join("\n");
    assert!(text.contains("Keys · Patterns"));
    assert!(text.contains("? or esc closes"));
    insta::assert_snapshot!(t.backend());
}

#[test]
fn help_over_connections_120x36() {
    let mut app = scanned_with_history(true);
    press(&mut app, "0?");
    let t = render(&app, 120, 36);
    let text = buffer_lines(&t).join("\n");
    assert!(text.contains("Keys · Connections"));
    assert!(text.contains("Look for Docker containers again"));
    insta::assert_snapshot!(t.backend());
}

#[test]
fn help_overlay_on_every_view_and_size_does_not_panic() {
    let mut app = scanned_with_history(true);
    for view in ['0', '1', '2', '3', '4'] {
        press(&mut app, &format!("{view}?"));
        for (w, h) in [(80, 24), (40, 12), (20, 5), (1, 1), (200, 60)] {
            render(&app, w, h);
        }
        let title = format!("Keys · {}", app.view.title());
        assert!(
            buffer_lines(&render(&app, 80, 24))
                .join("\n")
                .contains(&title)
        );
        app.handle_key(key(KeyCode::Esc));
        assert!(
            !buffer_lines(&render(&app, 80, 24))
                .join("\n")
                .contains(&title)
        );
    }
}

#[test]
fn detail_command_is_what_shift_y_copies() {
    let mut app = scanned_with_history(true);
    for (view, moves) in [('1', 1), ('1', 2), ('2', 3)] {
        press(&mut app, &format!("{view}g"));
        for _ in 0..moves {
            app.handle_key(ch('j'));
        }
        let effects = app.handle_key(ch('Y'));
        let [Effect::Copy(t)] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        let text = buffer_lines(&render(&app, 120, 36)).join("\n");
        assert!(
            text.contains(&format!("$ {}", t.as_str())),
            "{}",
            t.as_str()
        );
    }
}

/// The list rows a view draws, from its main panel: between the top border and the
/// bottom border, less the column header.
fn drawn_list_rows(lines: &[String]) -> usize {
    let top = lines.iter().position(|l| l.starts_with('┌')).unwrap();
    let bottom = top
        + lines[top..]
            .iter()
            .position(|l| l.starts_with('└'))
            .unwrap();
    (bottom - top - 1).saturating_sub(1)
}

#[test]
fn list_rows_is_what_every_list_view_draws() {
    let mut app = scanned_with_history(true);
    for (w, h) in [(80, 24), (120, 36), (60, 20), (200, 60), (80, 17), (80, 18)] {
        let rows = ui::list_rows(ratatui::layout::Rect::new(0, 0, w, h));
        for view in ['0', '1', '2', '3', '4'] {
            app.handle_key(ch(view));
            let lines = buffer_lines(&render(&app, w, h));
            assert_eq!(drawn_list_rows(&lines), rows, "view {view} at {w}x{h}");
        }
    }
    // 80x24 shows 7 rows. Too small for any row: 0, and PgDn still moves one.
    assert_eq!(ui::list_rows(ratatui::layout::Rect::new(0, 0, 80, 24)), 7);
    for h in [0, 1, 10, 16] {
        assert_eq!(
            ui::list_rows(ratatui::layout::Rect::new(0, 0, 80, h)),
            0,
            "{h}"
        );
    }
}

#[test]
fn page_down_from_the_top_selects_the_row_below_the_screen() {
    // With the page from the runtime's maths, PgDn from the top of the TTL list at
    // 80x24 (7 rows) selects row 7: the list scrolls one row and the selection is the
    // bottom row on screen.
    let mut app = scanned_with_history(true);
    app.handle_key(ch('2'));
    app.set_list_rows(ui::list_rows(ratatui::layout::Rect::new(0, 0, 80, 24)));
    app.handle_key(key(KeyCode::PageDown));
    assert_eq!(app.sel.ttl, 7);
    let t = render(&app, 80, 24);
    let buf = t.backend().buffer();
    let selected: Vec<u16> = (0..24u16)
        .filter(|&y| buf[(2, y)].bg == super::ui::style::sel().bg.unwrap())
        .collect();
    // Rows 5-11 are the list (border at 3, header at 4); the last one is selected.
    assert_eq!(selected, [11]);
}

#[test]
fn a_slow_keychain_during_a_rescan_shows_the_hint_in_the_header() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let mut app = scanned_with_history(true);
    app.apply(Msg::TickAt(t0));
    app.handle_key(ch('1'));
    app.handle_key(ch('r'));
    let text = |app: &App| buffer_lines(&render(app, 140, 40)).join("\n");
    assert!(!text(&app).contains("Waiting for the OS keychain"));
    app.apply(Msg::TickAt(t0 + Duration::from_secs(3)));
    let lines = buffer_lines(&render(&app, 140, 40));
    assert!(lines[1].contains(super::app::KEYCHAIN_HINT), "{}", lines[1]);
    // The previous results stay in the body.
    assert!(
        lines[3].contains("rescanning · previous results"),
        "{}",
        lines[3]
    );
}
