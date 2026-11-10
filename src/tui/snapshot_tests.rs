//! insta snapshots of rendered screens, drawn with ratatui's `TestBackend`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::app::{App, Effect, Msg};
use super::fixtures;
use super::tree::{Root, Row};
use super::ui;
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

/// Connected to prod-cache-01 with a finished scan.
fn scanned(sizes: bool) -> App {
    let mut app = App::new(fixtures::profiles());
    app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
    let id = connect_selected(&mut app);
    app.apply(Msg::Connected {
        id,
        server: Some(fixtures::server()),
        perms: if sizes {
            fixtures::perms_ok()
        } else {
            fixtures::perms_memory_denied()
        },
    });
    app.apply(Msg::ScanDone {
        id,
        report: fixtures::report(sizes),
    });
    app
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
fn placeholder_tab() {
    let mut app = scanned(true);
    app.handle_key(ch('2'));
    let t = render(&app, 120, 36);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn small_terminals_do_not_panic() {
    let mut apps = vec![App::new(vec![]), scanned(true), scanned(false)];
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
