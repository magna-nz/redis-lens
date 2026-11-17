//! Colour by meaning (`docs/ui-redesign.md` section 4): the style of specific cells,
//! which the text snapshots cannot see. Bars are the data colour, `▲` and its sentence
//! and `never` the warning colour, PROD, `writes on`, errors and destructive actions the
//! danger colour, `›` the focus colour; and an info notice is not a warning. Each check
//! runs in the dark palette and without colour (`NO_COLOR`), where the roles fall back
//! to weight: danger and focus bold, warning and data plain.

use ratatui::buffer::Cell;
use ratatui::style::Modifier;

use super::*;
use crate::tui::app::Notice;
use crate::tui::ui::style::{self, AMBER, DATA, FOCUS, Mode, RED};

/// The role a cell was drawn in, as the test asks for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Is {
    Data,
    Warning,
    Danger,
    Focus,
}

/// Whether `cell` is drawn in role `is` in mode `m`. Dark: its colour (a badge's
/// fill for a reversed badge). Mono: no colour at all, bold for danger and focus, not
/// bold for warning and data.
fn drawn_as(cell: &Cell, is: Is, m: Mode) -> bool {
    match m {
        Mode::Dark => {
            let want = match is {
                Is::Data => DATA,
                Is::Warning => AMBER,
                Is::Danger => RED,
                Is::Focus => FOCUS,
            };
            cell.fg == want || cell.bg == want
        }
        Mode::Mono => {
            let bold = cell.modifier.contains(Modifier::BOLD);
            let plain = cell.fg == Color::Reset;
            match is {
                Is::Danger | Is::Focus => plain && bold,
                Is::Warning | Is::Data => plain && !bold,
            }
        }
        Mode::Light => unreachable!("the light palette has its own test in style.rs"),
    }
}

/// The cell at the first `text` from column `from` on the first row that contains
/// `row` and has `text` there, plus `dx` cells.
fn cell_of<'a>(
    t: &'a Terminal<TestBackend>,
    row: &str,
    text: &str,
    from: u16,
    dx: u16,
) -> &'a Cell {
    let lines = buffer_lines(t);
    let buf = t.backend().buffer();
    let want: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    let n = want.len() as u16;
    let at = |y: u16| {
        (from..buf.area.width.saturating_sub(n.saturating_sub(1)))
            .find(|&x| (0..n).all(|i| buf[(x + i, y)].symbol() == want[usize::from(i)]))
    };
    let (x, y) = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains(row))
        .find_map(|(y, _)| at(y as u16).map(|x| (x, y as u16)))
        .unwrap_or_else(|| panic!("no {text} on a row with {row}:\n{}", lines.join("\n")));
    &buf[(x + dx, y)]
}

fn check(t: &Terminal<TestBackend>, what: &str, cell: &Cell, is: Is, m: Mode) {
    assert!(
        drawn_as(cell, is, m),
        "{what}: expected {is:?} in {m:?}, got fg {:?} bg {:?} {:?}\n{}",
        cell.fg,
        cell.bg,
        cell.modifier,
        buffer_lines(t).join("\n")
    );
}

const MODES: [Mode; 2] = [Mode::Dark, Mode::Mono];

/// Browse › Keys: the SHARE bar is data, the `›` is focus, a flagged row's `▲` and the
/// first word of its sentence in the detail pane are warnings.
#[test]
fn bars_marks_sentences_and_the_marker() {
    let mut app = scanned(true);
    // cache:search:{hash}, a flagged pattern, two rows down.
    press(&mut app, "jj");
    for m in MODES {
        let t = style::with_mode(m, || render(&app, 120, 36));
        let bar = cell_of(&t, "session:{uuid}", "█", 30, 0);
        check(&t, "share bar", bar, Is::Data, m);
        let mark = cell_of(&t, "cache:*", "▲", 60, 0);
        check(&t, "▲ in the list", mark, Is::Warning, m);
        let marker = cell_of(&t, "cache:search:{hash}", "›", 0, 0);
        check(&t, "› marker", marker, Is::Focus, m);
        // The detail pane (from column 74): `▲ 12% of 1.12M keys never expire, …`, and
        // `never` among its example keys.
        let sentence = "keys never expire, while";
        let mark = cell_of(&t, sentence, "▲ ", 74, 0);
        check(&t, "▲ before the sentence", mark, Is::Warning, m);
        let first_word = cell_of(&t, sentence, "▲ ", 74, 2);
        assert_eq!(first_word.symbol(), "1");
        check(&t, "the sentence's first word", first_word, Is::Warning, m);
        let never = cell_of(&t, "cache:search:a17e", "never", 74, 0);
        check(&t, "never among the examples", never, Is::Warning, m);
    }
}

/// The key list: `never` in EXPIRES is a warning; an expiry is not.
#[test]
fn never_in_the_key_list() {
    let mut app = scanned(true);
    super::super::app::views::testing::live_ready(&mut app);
    for _ in 0..20 {
        if buffer_lines(&render(&app, 120, 36))[2].contains("user:{id}:cart") {
            break;
        }
        app.handle_key(ch('j'));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen(), Screen::KeyList);
    let mut never = super::super::app::views::testing::key_row();
    never.key = "user:73:cart".into();
    never.ttl = crate::model::Ttl::Persistent;
    app.views.keys.rows = vec![super::super::app::views::testing::key_row(), never];
    for m in MODES {
        let t = style::with_mode(m, || render(&app, 120, 36));
        let cell = cell_of(&t, "user:73:cart", "never", 10, 0);
        check(&t, "never", cell, Is::Warning, m);
        if m == Mode::Dark {
            let expires = cell_of(&t, "user:10293:cart", "6d", 10, 0);
            assert_ne!(expires.fg, AMBER, "an expiry is no warning");
        }
    }
}

/// The context bar: PROD's badge and `WRITES ON` are danger, `writes on` too,
/// `read-only` is a warning.
#[test]
fn prod_writes_on_and_read_only() {
    let prod = scanned_profile("prod-cache-01", |_| {});
    let prod_writes = scanned_profile("prod-cache-01", |p| p.read_only = Some(false));
    let staging = scanned_profile("staging-cache", |_| {});
    for m in MODES {
        let t = style::with_mode(m, || render(&prod, 120, 24));
        check(
            &t,
            "PROD badge",
            cell_of(&t, " PROD ", "PROD", 0, 0),
            Is::Danger,
            m,
        );
        check(
            &t,
            "read-only",
            cell_of(&t, "read-only", "read-only", 0, 0),
            Is::Warning,
            m,
        );
        let t = style::with_mode(m, || render(&prod_writes, 120, 24));
        check(
            &t,
            "WRITES ON badge",
            cell_of(&t, "WRITES ON", "WRITES", 0, 0),
            Is::Danger,
            m,
        );
        let t = style::with_mode(m, || render(&staging, 120, 24));
        check(
            &t,
            "writes on",
            cell_of(&t, "writes on", "writes on", 0, 1),
            Is::Danger,
            m,
        );
    }
}

/// The footer's notices: an error is danger, an info notice is plain text.
#[test]
fn error_and_info_notices() {
    let mut app = scanned(true);
    app.notice = Some(Notice::error("Could not copy: no clipboard"));
    for m in MODES {
        let t = style::with_mode(m, || render(&app, 120, 24));
        let cell = cell_of(&t, "Could not copy", "Could", 0, 0);
        check(&t, "error notice", cell, Is::Danger, m);
    }
    app.notice = Some(Notice::info("Copied the pattern", app.tick));
    for m in MODES {
        let t = style::with_mode(m, || render(&app, 120, 24));
        let cell = cell_of(&t, "Copied the pattern", "Copied", 0, 0);
        match m {
            Mode::Dark => {
                assert_ne!(cell.fg, AMBER, "an info notice is not amber");
                assert_ne!(cell.fg, RED);
            }
            _ => assert!(!cell.modifier.contains(Modifier::BOLD)),
        }
    }
}

/// The Actions menu: `Delete` is danger on a profile with writes on; a line that is
/// off is dim whatever it does; `Copy` is plain.
#[test]
fn delete_in_the_actions_menu() {
    let mut app = scanned_profile("staging-cache", |_| {});
    show(&mut app, Screen::Patterns);
    super::super::app::views::testing::live_ready(&mut app);
    app.handle_key(ch('.'));
    for m in MODES {
        let t = style::with_mode(m, || render(&app, 120, 36));
        let lines = buffer_lines(&t);
        let row = lines
            .iter()
            .find(|l| l.contains(" Delete ") && !l.contains("off:"))
            .unwrap_or_else(|| panic!("no Delete line:\n{}", lines.join("\n")))
            .clone();
        let cell = cell_of(&t, &row, "Delete", 60, 0);
        check(&t, "Delete in the menu", cell, Is::Danger, m);
        if m == Mode::Dark {
            let copy = cell_of(&t, "Copy cmd", "Copy cmd", 60, 0);
            assert_ne!(copy.fg, RED, "Copy is not destructive");
        }
    }
}
