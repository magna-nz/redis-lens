//! insta snapshots of cluster scans: the Nodes view, Patterns narrowed to a node, the
//! Hot keys node column and the cluster summary in the header and on Connections.
//! Fixture endpoints are made-up 10.0.1.x addresses; nothing here holds a credential.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use super::app::{App, ConnId, Effect, Msg, ScanId, Screen};
use super::fixtures;
use super::ui;
use super::ui::style::{AMBER, DIM, RED, TEAL};
use crate::conn::ScanTarget;
use crate::model::ScanReport;

const HEALTHY: &str = "cluster · 3 shards";
const DEGRADED: &str = "cluster · 3 shards · 1 unreachable";
const REPLICAS: &str = "cluster · 3 shards · replicas 2/3";

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn press(app: &mut App, keys: &str) {
    for c in keys.chars() {
        app.handle_key(ch(c));
    }
}

/// Goes to `screen`: its view's digit, then `tab` to the panel.
fn show(app: &mut App, screen: Screen) {
    super::app::views::testing::show(app, screen);
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

fn text(t: &Terminal<TestBackend>) -> String {
    buffer_lines(t).join("\n")
}

fn scan_of(app: &App) -> ScanId {
    app.session.as_ref().unwrap().scan
}

/// prod-cache-01 connected as a cluster (`summary` from the connect) and `report`
/// finished scanning.
fn cluster_scanned(report: ScanReport, summary: &str) -> (App, ConnId) {
    let mut app = App::new(fixtures::profiles());
    let effects = app.handle_key(key(KeyCode::Enter));
    let [Effect::Connect { id, .. }] = effects[..] else {
        panic!("unexpected effects {effects:?}");
    };
    app.apply(Msg::Connected {
        id,
        scan: scan_of(&app),
        server: Some(report.server.clone()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Cluster(summary.into()),
    });
    app.apply(Msg::ScanDone {
        id,
        scan: scan_of(&app),
        report: Box::new(report),
    });
    (app, id)
}

/// The foreground colour of the first cell at or after column `from` on the row
/// containing `row_text` whose symbol is `cell_text`.
fn colour_at(t: &Terminal<TestBackend>, row_text: &str, cell_text: &str, from: u16) -> Color {
    let lines = buffer_lines(t);
    let y = lines
        .iter()
        .position(|l| l.contains(row_text))
        .unwrap_or_else(|| panic!("no row {row_text}")) as u16;
    let buf = t.backend().buffer();
    let x = (from..buf.area.width)
        .find(|&x| buf[(x, y)].symbol() == cell_text)
        .unwrap_or_else(|| panic!("no {cell_text} on row {row_text}"));
    buf[(x, y)].fg
}

/// The foreground colour of the first cell of `needle`, on the first row holding it.
fn colour_of(t: &Terminal<TestBackend>, needle: &str) -> Color {
    let lines = buffer_lines(t);
    let y = lines
        .iter()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no {needle} on screen:\n{}", lines.join("\n")));
    let x = column_of(&lines, y, needle) as u16;
    t.backend().buffer()[(x, y as u16)].fg
}

/// The column where `needle` starts on row `y`, in cells.
fn column_of(lines: &[String], y: usize, needle: &str) -> usize {
    let l = &lines[y];
    let at = l
        .find(needle)
        .unwrap_or_else(|| panic!("no {needle} in {l}"));
    l[..at].chars().count()
}

#[test]
fn nodes_healthy_three_shards() {
    let (mut app, _) = cluster_scanned(fixtures::cluster_report(), HEALTHY);
    show(&mut app, Screen::Nodes);
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    // The panel row of the tab bar names it.
    assert!(lines[3].contains("Nodes"), "{}", lines[3]);
    assert!(lines[4].starts_with("┌─ Nodes "), "{}", lines[4]);
    assert!(lines[4].contains("3 shards"), "{}", lines[4]);
    for (row, slots, ep) in [
        (6, "0-5460", "10.0.1.11:6379"),
        (7, "5461-10922", "10.0.1.12:6379"),
        (8, "10923-16383", "10.0.1.13:6379"),
    ] {
        let l = &lines[row];
        assert!(l.contains(slots) && l.contains(ep), "{l}");
        assert!(l.contains("primary") && l.contains("scanned"), "{l}");
    }
    // Columns line up with the header.
    for col in ["ENDPOINT", "ROLE", "STATUS"] {
        let at = column_of(&lines, 5, col);
        let first = match col {
            "ENDPOINT" => "10.0.1.11",
            "ROLE" => "primary",
            _ => "scanned",
        };
        assert_eq!(column_of(&lines, 6, first), at, "{col}");
    }
    assert_eq!(colour_of(&t, "scanned"), TEAL);
    // The detail panel is the selected (first) node's.
    assert!(
        lines.iter().any(|l| l.starts_with("┌─ 10.0.1.11:6379 ")),
        "{lines:#?}"
    );
    insta::assert_snapshot!(t.backend());
}

#[test]
fn nodes_with_a_failed_and_a_partial_node() {
    let (mut app, _) = cluster_scanned(fixtures::cluster_report_degraded(), DEGRADED);
    show(&mut app, Screen::Nodes);
    press(&mut app, "G");
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(lines[4].contains("1 failed · 1 partial"), "{}", lines[4]);
    // Failed red, partial amber, notes in the same colours.
    assert_eq!(colour_at(&t, "│ 10923-16383", "✗", 40), RED);
    assert_eq!(colour_at(&t, "│ 5461-10922", "▲", 40), AMBER);
    assert_eq!(colour_of(&t, fixtures::NOTE_UNREACHABLE), RED);
    // The failed node has no server figures and no sample.
    let failed = lines.iter().find(|l| l.contains("│ 10923-16383")).unwrap();
    assert_eq!(failed.matches('—').count(), 3, "{failed}");
    // The failed node is selected: its whole note is in the detail panel.
    assert!(text(&t).contains(&format!("✗ {}", fixtures::NOTE_UNREACHABLE)));
    // The header's summary is amber.
    assert_eq!(colour_of(&t, DEGRADED), AMBER);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn nodes_partial_note_wraps_in_the_detail_panel() {
    let (mut app, _) = cluster_scanned(fixtures::cluster_report_degraded(), DEGRADED);
    show(&mut app, Screen::Nodes);
    press(&mut app, "j");
    let t = render(&app, 80, 24);
    let all = text(&t);
    // Every word of the note is on screen, over two lines of the detail panel.
    let detail: String = buffer_lines(&t)[13..23].join(" ");
    for word in fixtures::NOTE_STOPPED.split_whitespace() {
        assert!(detail.contains(word), "{word} missing:\n{all}");
    }
}

#[test]
fn nodes_with_replicas_scanned() {
    let (mut app, _) = cluster_scanned(fixtures::cluster_report_replicas(), REPLICAS);
    show(&mut app, Screen::Nodes);
    press(&mut app, "G");
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(lines[4].contains("2 on replicas"), "{}", lines[4]);
    assert!(lines[6].contains("10.0.1.21:6379") && lines[6].contains("replica"));
    assert!(lines[7].contains("10.0.1.22:6379") && lines[7].contains("replica"));
    // The shard whose replica could not be used: its primary, with the reason.
    assert!(lines[8].contains("10.0.1.13:6379") && lines[8].contains("primary"));
    assert!(lines[8].contains("replica not used"), "{}", lines[8]);
    assert!(text(&t).contains("▲ replica not used: 10.0.1.23:6379: timed out"));
    // Not degraded: the summary in the header is dim.
    assert_eq!(colour_of(&t, REPLICAS), DIM);
    // A replica's detail names its primary.
    app.handle_key(ch('g'));
    let all = text(&render(&app, 120, 36));
    assert!(all.contains("Primary"), "{all}");
    insta::assert_snapshot!(t.backend());
}

#[test]
fn nodes_narrow_80x24_and_60x20() {
    let (mut app, _) = cluster_scanned(fixtures::cluster_report_degraded(), DEGRADED);
    show(&mut app, Screen::Nodes);
    let mut snap = String::new();
    for (w, h) in [(80, 24), (60, 20)] {
        let t = render(&app, w, h);
        let lines = buffer_lines(&t);
        for l in &lines {
            assert_eq!(l.chars().count(), usize::from(w), "{l}");
        }
        // Slots and endpoints always show; at 80 the note column has no room.
        assert!(lines[6].contains("0-5460") && lines[6].contains("10.0.1.11:6379"));
        assert!(!lines[5].contains("NOTE"), "{}", lines[5]);
        snap.push_str(&format!("{w}x{h}:\n{}\n\n", lines.join("\n")));
    }
    insta::assert_snapshot!(snap);
}

#[test]
fn nodes_on_a_standalone_report() {
    let mut report = fixtures::report(true);
    report.hot_keys = fixtures::hot_keys();
    let mut app = App::new(fixtures::profiles());
    let effects = app.handle_key(key(KeyCode::Enter));
    let [Effect::Connect { id, .. }] = effects[..] else {
        panic!("unexpected effects {effects:?}");
    };
    app.apply(Msg::Connected {
        id,
        scan: scan_of(&app),
        server: Some(report.server.clone()),
        perms: fixtures::perms_ok(),
        target: ScanTarget::Primary,
    });
    app.apply(Msg::ScanDone {
        id,
        scan: scan_of(&app),
        report: Box::new(report),
    });
    show(&mut app, Screen::Nodes);
    let t = render(&app, 80, 24);
    let all = text(&t);
    assert!(all.contains("Not a cluster: this scan read one standalone server."));
    // As on an empty Hot keys view, the footer is the view's own: r and s still work.
    assert!(
        buffer_lines(&t)[23].starts_with(" ↑↓ Move   tab Panel   enter Patterns"),
        "{}",
        buffer_lines(&t)[23]
    );
    insta::assert_snapshot!(t.backend());
}

#[test]
fn patterns_narrowed_to_a_node() {
    let (mut app, id) = cluster_scanned(fixtures::cluster_report(), HEALTHY);
    // With history, so the merged view has a Δ column the node view must not show.
    let taken_at_unix = app.report().unwrap().taken_at_unix;
    app.apply(Msg::History {
        id,
        scan: scan_of(&app),
        taken_at_unix,
        result: Ok(Some(fixtures::history(true))),
    });
    assert!(buffer_lines(&render(&app, 120, 36))[4].contains('Δ'));
    show(&mut app, Screen::Nodes);
    press(&mut app, "j");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen(), Screen::Patterns);
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(
        lines[3].starts_with("┌─ Patterns · node 10.0.1.12:6379 "),
        "{}",
        lines[3]
    );
    assert!(lines[3].contains("esc for all nodes"), "{}", lines[3]);
    assert!(!lines[4].contains('Δ'), "{}", lines[4]);
    // At 60 wide the title still names the node.
    let narrow = buffer_lines(&render(&app, 60, 20));
    assert!(narrow[3].contains("node 10.0.1.12"), "{}", narrow[3]);
    // esc: back to Nodes, then the merged rows with their Δ column on 1.
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen(), Screen::Nodes);
    app.handle_key(ch('1'));
    let back = buffer_lines(&render(&app, 120, 36));
    assert!(back[3].starts_with("┌─ Patterns "), "{}", back[3]);
    assert!(!back[3].contains("node"), "{}", back[3]);
    assert!(back[4].contains('Δ'), "{}", back[4]);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn hot_keys_with_the_node_column() {
    let (mut app, _) = cluster_scanned(fixtures::cluster_report(), HEALTHY);
    show(&mut app, Screen::HotKeys);
    press(&mut app, "j");
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(lines[5].contains("NODE"), "{}", lines[5]);
    let node_col = column_of(&lines, 5, "NODE");
    // Each key shows the node it was sampled on, under the heading.
    let report = app.report().unwrap();
    let nodes = report.nodes.as_ref().unwrap();
    for (i, k) in report.hot_keys.iter().enumerate() {
        let ep = nodes[usize::from(k.node.unwrap())].endpoint.to_string();
        assert_eq!(column_of(&lines, 6 + i, &ep), node_col, "{}", lines[6 + i]);
    }
    // The detail panel names the selected key's node too.
    assert!(text(&t).contains(" NODE   10.0.1.12:6379"), "{}", text(&t));
    // A standalone report keeps today's columns.
    let mut standalone = fixtures::report(true);
    standalone.hot_keys = fixtures::hot_keys();
    let (mut plain, _) = cluster_scanned(standalone, HEALTHY);
    show(&mut plain, Screen::HotKeys);
    let plain_lines = buffer_lines(&render(&plain, 120, 36));
    assert!(!plain_lines[5].contains("NODE"), "{}", plain_lines[5]);
    insta::assert_snapshot!(t.backend());
}

#[test]
fn hot_keys_node_column_cuts_a_long_address() {
    let mut report = fixtures::cluster_report();
    report.nodes.as_mut().unwrap()[0].endpoint.host = "cache-node-0001.internal".into();
    let (mut app, _) = cluster_scanned(report, HEALTHY);
    show(&mut app, Screen::HotKeys);
    let lines = buffer_lines(&render(&app, 120, 36));
    let row = &lines[6];
    // Cut to 15 cells, and a cell stays free before the pattern.
    assert!(row.contains(" cache-node-000… cache:"), "{row}");
}

/// The header (brand line, server line and tabs) of `app` drawn `w` wide.
fn header(app: &App, w: u16) -> String {
    buffer_lines(&render(app, w, 24))[..3].join("\n")
}

#[test]
fn header_with_the_cluster_summary() {
    let (healthy, _) = cluster_scanned(fixtures::cluster_report(), HEALTHY);
    let (degraded, _) = cluster_scanned(fixtures::cluster_report_degraded(), DEGRADED);
    let mut snap = String::new();
    for (app, summary) in [(&healthy, HEALTHY), (&degraded, DEGRADED)] {
        for w in [140, 120, 80] {
            let h = header(app, w);
            let first = h.lines().next().unwrap();
            // The summary, and on wide terminals the scan status beside it.
            assert!(
                first.contains(summary) || first.contains("cluster · 3 shards"),
                "{w}: {first}"
            );
            if w >= 120 {
                assert!(first.contains("scan complete"), "{w}: {first}");
            }
            snap.push_str(&format!("{summary} at {w}:\n{h}\n\n"));
        }
    }
    // Merged figures: summed memory and keys, Redis 7.4.2.
    assert!(snap.contains("Redis 7.4.2"));
    let t = render(&healthy, 120, 24);
    assert_eq!(colour_of(&t, HEALTHY), DIM);
    let t = render(&degraded, 120, 24);
    assert_eq!(colour_of(&t, DEGRADED), AMBER);
    insta::assert_snapshot!(snap);
}

#[test]
fn a_node_failing_mid_scan_turns_the_summary_amber() {
    // The connect saw every node; the report says one stopped early.
    let mut report = fixtures::cluster_report();
    report.nodes.as_mut().unwrap()[0].status = crate::model::NodeStatus::Partial;
    let (app, _) = cluster_scanned(report, HEALTHY);
    let t = render(&app, 120, 24);
    assert_eq!(colour_of(&t, HEALTHY), AMBER);
}

#[test]
fn connections_detail_shows_the_cluster_summary() {
    for (report, summary, colour) in [
        (fixtures::cluster_report(), HEALTHY, None),
        (fixtures::cluster_report_degraded(), DEGRADED, Some(AMBER)),
    ] {
        let (mut app, _) = cluster_scanned(report, summary);
        app.handle_key(ch('0'));
        let t = render(&app, 120, 36);
        let lines = buffer_lines(&t);
        let server = lines
            .iter()
            .find(|l| l.starts_with('│') && l.contains(" Server "))
            .unwrap_or_else(|| panic!("{lines:#?}"));
        assert!(
            server.contains(&format!("Redis 7.4.2 · {summary}")),
            "{server}"
        );
        // The Endpoint line stays the profile's.
        let endpoint = lines.iter().find(|l| l.contains(" Endpoint ")).unwrap();
        assert!(!endpoint.contains("scanning"), "{endpoint}");
        let got = colour_of(&t, &format!("Redis 7.4.2 · {summary}"));
        match colour {
            Some(c) => assert_eq!(got, c, "{summary}"),
            None => assert_ne!(got, AMBER, "{summary}"),
        }
    }
}

#[test]
fn every_cluster_screen_at_every_size_does_not_panic() {
    let mut apps = vec![
        cluster_scanned(fixtures::cluster_report(), HEALTHY).0,
        cluster_scanned(fixtures::cluster_report_degraded(), DEGRADED).0,
        cluster_scanned(fixtures::cluster_report_replicas(), REPLICAS).0,
    ];
    // A seed-only scan: one node with no known slots.
    let mut seed = fixtures::cluster_report_with(&[1.0]);
    let n = &mut seed.nodes.as_mut().unwrap()[0];
    n.slots.clear();
    n.note = Some(crate::conn::cluster::SHARDS_UNREADABLE.into());
    apps.push(cluster_scanned(seed, "cluster · seed node only (shards unknown)").0);
    // An empty node list and an out-of-range hot key node index.
    let mut odd = fixtures::cluster_report();
    odd.hot_keys[0].node = Some(99);
    apps.push(cluster_scanned(odd, HEALTHY).0);
    let mut none = fixtures::cluster_report();
    none.nodes = Some(Vec::new());
    apps.push(cluster_scanned(none, HEALTHY).0);
    for app in &mut apps {
        for screen in Screen::ALL {
            if matches!(screen, Screen::KeyList | Screen::KeyDetail) {
                continue;
            }
            show(app, screen);
            for _ in 0..3 {
                app.handle_key(ch('j'));
            }
            if screen == Screen::Nodes {
                app.handle_key(key(KeyCode::Enter));
            }
            for (w, h) in [(60, 20), (61, 21), (80, 24), (40, 10), (1, 1), (200, 60)] {
                render(app, w, h);
            }
        }
    }
    // The out-of-range index shows a dash, not a wrong node.
    let mut odd = fixtures::cluster_report();
    odd.hot_keys[0].node = Some(99);
    let (mut app, _) = cluster_scanned(odd, HEALTHY);
    show(&mut app, Screen::HotKeys);
    let lines = buffer_lines(&render(&app, 120, 36));
    assert!(lines[6].contains(" —  "), "{}", lines[6]);
}

#[test]
fn a_seed_only_scan_is_amber_everywhere_it_is_summed_up() {
    const SEED: &str = "cluster · seed node only (shards unknown)";
    let mut report = fixtures::cluster_report_with(&[1.0]);
    let n = &mut report.nodes.as_mut().unwrap()[0];
    n.slots.clear();
    // The seed itself was scanned; only the shard list is unknown.
    n.note = Some(crate::conn::cluster::SHARDS_UNREADABLE.into());
    let (mut app, _) = cluster_scanned(report, SEED);
    // At 120 the summary is cut to keep the scan status beside it; at 160 it is whole.
    assert_eq!(
        colour_of(&render(&app, 120, 36), "cluster · seed node only"),
        AMBER
    );
    assert!(text(&render(&app, 160, 36)).contains(SEED));
    app.handle_key(ch('0'));
    let t = render(&app, 140, 36);
    assert_eq!(colour_of(&t, &format!("Redis 7.4.2 · {SEED}")), AMBER);
    // On Nodes the slots are unknown, and the whole note is in the detail panel.
    show(&mut app, Screen::Nodes);
    let t = render(&app, 120, 36);
    let lines = buffer_lines(&t);
    assert!(lines[6].starts_with("│ —  "), "{}", lines[6]);
    let detail = lines[27..36].join(" ");
    for word in crate::conn::cluster::SHARDS_UNREADABLE.split_whitespace() {
        assert!(detail.contains(word), "{word}: {detail}");
    }
}

/// The views that read one server are off on a cluster, say why, and ask nothing.
#[test]
fn live_views_are_off_on_a_cluster_and_send_nothing() {
    let (mut app, _) = cluster_scanned(fixtures::cluster_report(), HEALTHY);
    for screen in [
        Screen::KeyList,
        Screen::KeyDetail,
        Screen::Overview,
        Screen::Clients,
        Screen::SlowLog,
        Screen::Streams,
        Screen::PubSub,
        Screen::Search,
    ] {
        let effects = super::app::views::testing::show(&mut app, screen);
        assert_eq!(app.screen(), screen);
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Live { .. })),
            "{screen:?}: {effects:?}"
        );
        let all = text(&render(&app, 120, 36));
        assert!(
            all.contains("Not available on a cluster yet: this view reads one server"),
            "{screen:?}: {all}"
        );
        // `r` on a live view says the same and sends nothing either.
        let effects = app.handle_key(ch('r'));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Live { .. })),
            "{screen:?}: {effects:?}"
        );
    }
    // The Analysis panels and Patterns still show the merged results.
    // (The notice from the last `r` is still in the key bar, so look above it.)
    show(&mut app, Screen::BigKeys);
    let lines = buffer_lines(&render(&app, 120, 36));
    let body = lines[..lines.len() - 1].join("\n");
    assert!(!body.contains("Not available on a cluster"), "{body}");
    assert!(
        body.contains("Lengths and encodings are not read on a cluster yet"),
        "{body}"
    );
}
