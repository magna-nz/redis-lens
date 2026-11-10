//! insta snapshots of rendered screens, drawn with ratatui's `TestBackend`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use super::app::{App, ConnId, Effect, Msg};
use super::fixtures;
use super::tree::{Root, Row};
use super::ui;
use crate::conn::ScanTarget;
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

/// Connected to prod-cache-01 with `perms`, and `report` finished scanning.
fn scanned_with(report: ScanReport, perms: PermissionReport) -> (App, ConnId) {
    let mut app = App::new(fixtures::profiles());
    app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
    let id = connect_selected(&mut app);
    app.apply(Msg::Connected {
        id,
        server: Some(report.server.clone()),
        perms,
        target: ScanTarget::Primary,
    });
    app.apply(Msg::ScanDone { id, report });
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

#[test]
fn connections_view() {
    let mut app = App::new(fixtures::profiles());
    app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
    // Select staging-cache and connect: MEMORY USAGE is denied for this user.
    app.handle_key(ch('j'));
    let id = connect_selected(&mut app);
    app.apply(Msg::Connected {
        id,
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
        server: Some(fixtures::server()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Primary,
    });
    app.apply(Msg::ScanProgress {
        id,
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
        server: Some(fixtures::server()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Primary,
    });
    let partial = fixtures::partial(true);
    app.apply(Msg::ScanProgress {
        id,
        progress: ScanProgress {
            sampled: partial.sampled,
            target: partial.target,
            done: false,
        },
    });
    app.apply(Msg::ScanPartial { id, partial });
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
        server: Some(fixtures::server()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Primary,
    });
    let mut partial = (*fixtures::partial(true)).clone();
    partial.patterns.clear();
    partial.sampled = 0;
    app.apply(Msg::ScanPartial {
        id,
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
