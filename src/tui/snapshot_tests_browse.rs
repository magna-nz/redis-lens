//! Browse › Keys in three panes from 120 columns (spec "Browse in three panes"): the
//! snapshots at 120x36 and 160x45, and the checks that the layout changes nothing a key
//! does, reads nothing as a cursor moves and never shows a list or a value under a
//! heading that names something else. Fixture key names only; no credential.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use super::app::views::testing;
use super::app::{App, Effect, Msg, Screen};
use super::fixtures;
use super::keymap;
use super::tree::{Root, Row};
use super::ui;
use crate::live::keys::KeyRow;
use crate::live::values::{Field, Items, Page, Reply};
use crate::live::{LiveError, LiveReply, LiveRequest};
use crate::model::{KeyType, Ttl};
use crate::value::Raw;

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn text(app: &App, w: u16, h: u16) -> String {
    lines(app, w, h).join("\n")
}

fn lines(app: &App, w: u16, h: u16) -> Vec<String> {
    let t = testing::render(app, w, h);
    let buf = t.backend().buffer();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn row(key: &str, ty: KeyType, len: u64, bytes: u64, ttl: Option<u64>) -> KeyRow {
    KeyRow {
        key: key.into(),
        key_type: ty,
        ttl: ttl.map_or(Ttl::Persistent, |s| Ttl::Expires(Duration::from_secs(s))),
        bytes: Some(bytes),
        encoding: Some("listpack".into()),
        length: Some(len),
        freq: None,
    }
}

fn carts() -> Vec<KeyRow> {
    vec![
        row("user:10293:cart", KeyType::Hash, 12, 3_174, Some(533_520)),
        row("user:88120:cart", KeyType::Hash, 31, 9_812, Some(86_400)),
        row("user:73:cart", KeyType::Hash, 4, 1_024, None),
        row("user:441207:cart", KeyType::Hash, 7, 2_210, Some(3_600)),
    ]
}

/// The row of `pattern` among the Patterns rows.
fn row_of(app: &App, pattern: &str) -> usize {
    let data = app.pattern_data().expect("patterns");
    let name = |i: usize| data.patterns[i].pattern.as_str();
    app.pattern_rows()
        .iter()
        .position(|r| match r {
            Row::Pattern { idx, .. } => name(*idx) == pattern,
            Row::Group { root } => matches!(
                app.patterns.tree.roots[*root],
                Root::Single(i) if name(i) == pattern
            ),
        })
        .unwrap_or_else(|| panic!("no row {pattern}"))
}

/// The scanned app (live connection ready) on the key list of `user:{id}:cart`,
/// holding [`carts`].
fn on_keys_of(mut app: App) -> App {
    testing::live_ready(&mut app);
    testing::area(&mut app, '2');
    app.patterns.sel = row_of(&app, "user:{id}:cart");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen(), Screen::KeyList);
    app.views.keys.rows = carts();
    app
}

fn on_keys() -> App {
    on_keys_of(testing::scanned())
}

fn hash() -> Items {
    let f = |n: &str, v: &str| Field {
        name: Raw::from(n),
        value: Raw::from(v),
        ttl: None,
    };
    Items::Hash(vec![
        f("sku:44812", r#"{"qty":2,"price":19.99}"#),
        f("sku:44813", r#"{"qty":1,"price":4.5}"#),
        f("updated", "1731840000"),
    ])
}

/// [`on_keys`], then `enter` on the first key with its value read.
fn on_value() -> App {
    let mut app = on_keys();
    let effects = app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen(), Screen::KeyDetail);
    let (id, _) = testing::requests(&effects)
        .into_iter()
        .find(|(_, r)| matches!(r, LiveRequest::Value(_)))
        .expect("a value read");
    testing::reply(
        &mut app,
        id,
        Ok(LiveReply::Value(Reply::Page(Page {
            type_name: Some("hash".into()),
            items: hash(),
            next: None,
            refused: Vec::new(),
        }))),
    );
    app
}

fn snap(name: &str, app: &App) {
    for (w, h) in [(120, 36), (160, 45)] {
        insta::assert_snapshot!(
            format!("browse_three_{name}_{w}x{h}"),
            testing::render(app, w, h).backend()
        );
    }
}

// -- Snapshots ----------------------------------------------------------------------

#[test]
fn focus_on_the_key_list() {
    snap("focus_keys", &on_keys());
}

#[test]
fn focus_on_the_value() {
    snap("focus_value", &on_value());
}

/// Back on Patterns from the value: the pattern whose keys were opened is still the
/// selected row, so its keys stay at the right.
#[test]
fn focus_on_patterns_with_the_opened_pattern_selected() {
    let mut app = on_value();
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen(), Screen::Patterns);
    snap("focus_patterns_opened", &app);
}

/// The focus on the items' value pane (`→` in the value): the border is the value's.
#[test]
fn focus_on_the_value_below_the_divider() {
    let mut app = on_value();
    app.handle_key(key(KeyCode::Right));
    snap("focus_value_pane", &app);
}

#[test]
fn with_v() {
    let mut app = on_keys();
    app.handle_key(ch('v'));
    assert!(app.more_here());
    snap("focus_keys_v", &app);
    app.handle_key(key(KeyCode::Enter));
    snap("focus_value_v", &app);
}

#[test]
fn a_value_that_could_not_be_read() {
    let mut app = on_keys();
    let effects = app.handle_key(key(KeyCode::Enter));
    let (id, _) = testing::requests(&effects)
        .into_iter()
        .find(|(_, r)| matches!(r, LiveRequest::Value(_)))
        .expect("a value read");
    testing::reply(&mut app, id, Err(LiveError::Denied("HSCAN".into())));
    snap("value_refused", &app);
}

#[test]
fn the_actions_menu_and_go_to_over_three_panes() {
    let mut app = on_keys();
    app.handle_key(ch('.'));
    assert!(app.menu.is_some());
    snap("actions_menu", &app);
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert!(app.goto.is_some());
    for c in "cart".chars() {
        app.handle_key(ch(c));
    }
    snap("goto", &app);
}

/// The key list of the find `*cart*`, holding [`carts`].
fn on_find() -> App {
    let mut app = on_keys();
    app.handle_key(key(KeyCode::Esc));
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert!(app.find.open, "{effects:?}");
    for c in "cart".chars() {
        app.handle_key(ch(c));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen(), Screen::KeyList);
    assert!(app.views.keys.find.is_some());
    app.views.keys.rows = carts();
    app
}

/// A find's results belong to no pattern: Patterns stays, no row marked open.
#[test]
fn find_results_in_three_panes() {
    let app = on_find();
    snap("find", &app);
    // No `›` in the Patterns pane: the gutter of its rows is blank.
    let l = lines(&app, 120, 36);
    let marked = l[4..33]
        .iter()
        .filter(|line| line.chars().nth(2) == Some('›'))
        .count();
    assert_eq!(marked, 0, "{}", l.join("\n"));
}

/// Before an analysis: the names tree, a pattern of names opened.
#[test]
fn names_only_in_three_panes() {
    snap("names_only", &on_names_only());
}

/// Before an analysis, the key list of the names pattern `user:{id}:cart`.
fn on_names_only() -> App {
    let mut app = App::new(fixtures::profiles());
    let effects = app.handle_key(key(KeyCode::Enter));
    let [Effect::Connect { id, scan, .. }] = effects[..] else {
        panic!("{effects:?}");
    };
    app.apply(Msg::Connected {
        id,
        scan,
        server: Some(fixtures::server()),
        perms: fixtures::perms_with_probes(),
        target: crate::conn::ScanTarget::Primary,
    });
    let mut effects = testing::live_ready(&mut app);
    effects.extend(testing::area(&mut app, '2'));
    let page = testing::requests(&effects)
        .into_iter()
        .find(|(_, r)| matches!(r, LiveRequest::Names(_)))
        .map(|(id, _)| id)
        .expect("a names page");
    let names = [
        "user:10293:cart",
        "user:88120:cart",
        "user:73:cart",
        "session:c41f0e9a",
        "session:7a2d9b14",
        "cache:product:88410",
    ];
    testing::reply(
        &mut app,
        page,
        Ok(LiveReply::Names(crate::live::names::Reply::Page(
            crate::live::names::Page {
                names: names.iter().map(|n| (*n).to_string()).collect(),
                cursor: 0,
                unreadable: 0,
            },
        ))),
    );
    assert!(app.pattern_data().is_some_and(|d| d.names));
    app.patterns.sel = row_of(&app, "user:{id}:cart");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen(), Screen::KeyList);
    app.views.keys.rows = carts();
    app
}

/// The scanned app on a three-node cluster, on the key list of `user:{id}:cart`.
fn on_cluster_keys() -> App {
    let mut app = testing::scanned();
    let (id, scan) = {
        let s = app.session.as_ref().unwrap();
        (s.id, s.scan)
    };
    app.apply(Msg::Nodes {
        id,
        scan,
        nodes: fixtures::live_nodes(),
    });
    let mut app = on_keys_of(app);
    assert!(app.views.keys.by_node(), "the walk goes by node");
    for (i, r) in carts().iter().enumerate() {
        app.views.keys.row_node.insert(r.key.clone(), i % 3);
    }
    app
}

/// On a cluster the focused key list: NODE from 140 columns.
#[test]
fn a_cluster_key_list_with_its_node_column() {
    let app = on_cluster_keys();
    // At 120 the list has no room for NODE beside a 28-cell name: the facts at its
    // right say where the selected key is. From 140 NODE joins, before STORED AS.
    let l = lines(&app, 120, 36);
    assert!(!l[3].contains("NODE"), "{}", l.join("\n"));
    assert!(
        l.join("\n").contains("on node 10.0.1.12:6379"),
        "{}",
        l.join("\n")
    );
    let l = lines(&app, 160, 45);
    assert!(l[3].contains("NODE"), "{}", l.join("\n"));
    snap("cluster", &app);
}

/// Go to the open value after another pattern was opened: the key list shown is not
/// the value's, so its bottom line says so and no pattern is marked as its parent.
#[test]
fn a_value_from_another_list_is_not_drawn_as_its_path() {
    let mut app = on_value();
    let open = app.views.value.row.as_ref().expect("a value").key.clone();
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen(), Screen::Patterns);
    app.patterns.sel = row_of(&app, "session:{uuid}");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen(), Screen::KeyList);
    app.views.keys.rows = vec![row("session:aa11", KeyType::String, 0, 120, None)];
    // Go to the value still open.
    app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    for c in open.chars() {
        app.handle_key(ch(c));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen(), Screen::KeyDetail, "{}", text(&app, 120, 36));
    assert_eq!(
        app.views.value.row.as_ref().map(|r| r.key.as_str()),
        Some(open.as_str())
    );
    for (w, h) in [(120, 36), (160, 45)] {
        let l = lines(&app, w, h);
        let all = l.join("\n");
        assert!(all.contains("the open key is not in this list"), "{all}");
        // Neither pattern carries the open mark.
        assert!(!all.contains('▸'), "{all}");
        assert!(
            !l.iter()
                .any(|x| x.chars().nth(2) == Some('›') && x.contains("cart")),
            "{all}"
        );
    }
}

// -- Behaviour ----------------------------------------------------------------------

/// The key events a key table row stands for: `→ l enter` is three, `↑↓ j k` four.
fn events_of(keys: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    for tok in keys.split_whitespace() {
        let code = |c| key(c);
        match tok {
            "↑↓" => out.extend([code(KeyCode::Up), code(KeyCode::Down)]),
            "↑" => out.push(code(KeyCode::Up)),
            "↓" => out.push(code(KeyCode::Down)),
            "→" => out.push(code(KeyCode::Right)),
            "←" => out.push(code(KeyCode::Left)),
            "←→" => out.extend([code(KeyCode::Left), code(KeyCode::Right)]),
            "enter" => out.push(code(KeyCode::Enter)),
            "esc" => out.push(code(KeyCode::Esc)),
            "tab" => out.push(code(KeyCode::Tab)),
            "shift-tab" => out.push(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            "space" => out.push(ch(' ')),
            "PgUp" => out.push(code(KeyCode::PageUp)),
            "PgDn" => out.push(code(KeyCode::PageDown)),
            "Home" => out.push(code(KeyCode::Home)),
            "End" => out.push(code(KeyCode::End)),
            "<>" => out.extend([ch('<'), ch('>')]),
            t if t.starts_with("ctrl-") && t.chars().count() == 6 => {
                let c = t.chars().last().unwrap();
                out.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
            }
            t if t.chars().count() == 1 => out.push(ch(t.chars().next().unwrap())),
            _ => {}
        }
    }
    out
}

/// Every key of `screen`'s table and the global one.
fn table_keys(screen: Screen) -> Vec<KeyEvent> {
    let mut out: Vec<KeyEvent> = Vec::new();
    for k in keymap::for_screen(screen).iter().chain(keymap::global()) {
        for e in events_of(k.keys) {
            if !out.contains(&e) {
                out.push(e);
            }
        }
    }
    out
}

/// An app on `screen` of Browse › Keys, drawn at `w` × `h` with the page the runtime
/// would set after drawing it.
fn on(screen: Screen, w: u16, h: u16) -> App {
    let mut app = match screen {
        Screen::Patterns => {
            let mut app = on_value();
            app.handle_key(key(KeyCode::Esc));
            app.handle_key(key(KeyCode::Esc));
            app
        }
        Screen::KeyList => on_keys(),
        _ => on_value(),
    };
    assert_eq!(app.screen(), screen);
    testing::render(&app, w, h);
    let rows = ui::list_rows(Rect::new(0, 0, w, h), &app);
    app.set_list_rows(rows);
    app
}

/// What a key did: its effects (in words) and where it left the app.
fn outcome(app: &mut App, k: KeyEvent, w: u16, h: u16) -> (String, Screen, String) {
    let effects = app.handle_key(k);
    // Draw as the runtime does after each key.
    testing::render(app, w, h);
    let notice = app
        .notice
        .as_ref()
        .map(|n| n.text.clone())
        .unwrap_or_default();
    let sel = format!(
        "patterns {} · keys {} · value {} {:?}",
        app.patterns.sel, app.views.keys.sel, app.views.value.sel, app.views.value.focus
    );
    (
        format!("{effects:?}"),
        app.screen(),
        format!("{notice} | {sel}"),
    )
}

/// Every key of each Browse › Keys screen's table does at 120 and 160 columns what it
/// does at 80: the same effects, the same screen and the same selection in every pane.
/// Over read-only and writes-on profiles, names only, a find, a cluster, with and
/// without `v`, the focus on each pane (the value's items and its value pane) and the
/// cursor off the opened pattern. Layout never changes behaviour.
#[test]
fn every_key_does_the_same_at_120_and_at_80_columns() {
    type Build = fn() -> App;
    let writes_on: Build = || {
        let mut app = on_keys();
        app.session.as_mut().expect("a session").profile.read_only = Some(false);
        assert!(!app.session.as_ref().unwrap().profile.is_read_only());
        app
    };
    let bases: [(&str, Build); 5] = [
        ("read-only", on_keys),
        ("writes on", writes_on),
        ("names only", on_names_only),
        ("find", on_find),
        ("cluster", on_cluster_keys),
    ];
    // From a key list: where the focus goes before the key is pressed.
    type Step = fn(&mut App);
    let states: [(&str, Step); 5] = [
        ("key list", |_| {}),
        ("patterns", |app| {
            app.handle_key(key(KeyCode::Esc));
        }),
        ("patterns, off the opened one", |app| {
            app.handle_key(key(KeyCode::Esc));
            app.handle_key(key(KeyCode::Down));
        }),
        ("value", |app| {
            open_value(app);
        }),
        ("value pane", |app| {
            open_value(app);
            app.handle_key(key(KeyCode::Right));
        }),
    ];
    let build = |base: Build, state: fn(&mut App), v: bool, w: u16| {
        let mut app = base();
        if v {
            app.handle_key(ch('v'));
        }
        state(&mut app);
        testing::render(&app, w, 24);
        let rows = ui::list_rows(Rect::new(0, 0, w, 24), &app);
        app.set_list_rows(rows);
        app
    };
    for (base_name, base) in bases {
        for (state_name, state) in states {
            for v in [false, true] {
                let screen = build(base, state, v, 80).screen();
                for k in table_keys(screen) {
                    // Quitting ends the test run's app, not a layout question.
                    if k == ch('q') {
                        continue;
                    }
                    let at_80 = outcome(&mut build(base, state, v, 80), k, 80, 24);
                    for w in [120, 160] {
                        let at_w = outcome(&mut build(base, state, v, w), k, w, 24);
                        assert_eq!(
                            at_80, at_w,
                            "{base_name}, {state_name}, v {v}: {k:?} at {w}"
                        );
                    }
                }
            }
        }
    }
}

/// `enter` on the selected key, its value read (a hash).
fn open_value(app: &mut App) {
    let effects = app.handle_key(key(KeyCode::Enter));
    if let Some((id, _)) = testing::requests(&effects)
        .into_iter()
        .find(|(_, r)| matches!(r, LiveRequest::Value(_)))
    {
        testing::reply(
            app,
            id,
            Ok(LiveReply::Value(Reply::Page(Page {
                type_name: Some("hash".into()),
                items: hash(),
                next: None,
                refused: Vec::new(),
            }))),
        );
    }
}

/// Moving the selection in any pane reads nothing: no effect at all.
#[test]
fn moving_the_selection_in_any_pane_sends_nothing() {
    let moves = [
        key(KeyCode::Down),
        key(KeyCode::Up),
        ch('j'),
        ch('k'),
        key(KeyCode::PageDown),
        key(KeyCode::PageUp),
        key(KeyCode::End),
        key(KeyCode::Home),
    ];
    for screen in [Screen::Patterns, Screen::KeyList, Screen::KeyDetail] {
        let mut app = on(screen, 120, 36);
        for m in moves {
            let e = app.handle_key(m);
            assert!(e.is_empty(), "{screen:?} {m:?}: {e:?}");
            testing::render(&app, 120, 36);
        }
    }
}

/// The pane right of Patterns lists the opened pattern's keys only while that pattern
/// is selected: moving away shows the selected row's summary, under its own name.
#[test]
fn moving_off_the_opened_pattern_shows_the_summary() {
    let mut app = on(Screen::Patterns, 120, 36);
    let l = lines(&app, 120, 36);
    assert!(l[2].contains("‹ user:{id}:cart"), "{}", l[2]);
    assert!(l[3].contains("KEY"), "{}", l[3]);
    app.handle_key(key(KeyCode::Down));
    let t = text(&app, 120, 36);
    let l: Vec<&str> = t.lines().collect();
    assert!(!l[2].contains("‹ user:{id}:cart"), "{}", l[2]);
    assert!(!t.contains("user:10293:cart"), "{t}");
    assert!(t.contains("EXPIRES IN"), "{t}");
    // Back on it: the keys again, nothing read.
    let e = app.handle_key(key(KeyCode::Up));
    assert!(e.is_empty());
    assert!(lines(&app, 120, 36)[2].contains("‹ user:{id}:cart"));
}

/// The pane right of the key list shows the opened value only while its key is
/// selected; another key shows that key's facts under its name.
#[test]
fn moving_off_the_opened_key_shows_its_facts() {
    let mut app = on_value();
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen(), Screen::KeyList);
    let t = text(&app, 120, 36);
    assert!(t.contains("sku:44812"), "{t}");
    app.handle_key(key(KeyCode::Down));
    let t = text(&app, 120, 36);
    let top = t.lines().nth(2).unwrap();
    assert!(top.contains("─ user:10293:ca"), "{top}");
    assert!(!t.contains("sku:44812"), "{t}");
    assert!(t.contains("→ shows the value"), "{t}");
}

/// Below 120 columns nothing changes: two panes, as before.
#[test]
fn below_120_columns_two_panes() {
    for app in [on_keys(), on_value()] {
        for w in [80, 100, 119] {
            let l = lines(&app, w, 24);
            let tees = l[2].matches('┬').count();
            assert_eq!(tees, 1, "{w}: {}", l[2]);
        }
        let l = lines(&app, 120, 24);
        assert_eq!(l[2].matches('┬').count(), 2, "{}", l[2]);
    }
}

/// Every Browse › Keys screen at every size from 0x0 to 60x16, and at 120x10 and
/// 200x60, draws without a panic.
#[test]
fn three_panes_at_every_size_do_not_panic() {
    let mut apps = vec![on(Screen::Patterns, 80, 24), on_keys(), on_value()];
    let mut v = on_keys();
    v.handle_key(ch('v'));
    apps.push(v);
    for app in &apps {
        for w in 0..=60 {
            for h in 0..=16 {
                testing::render(app, w, h);
            }
        }
        for (w, h) in [(120, 10), (200, 60), (120, 3), (121, 4), (300, 5)] {
            testing::render(app, w, h);
        }
    }
}
