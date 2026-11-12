//! The single source of truth for keys: the footer, the help overlay and the key
//! table in `spec.md` are all rendered from the data here.
//!
//! This module only describes keys, one table per [`Screen`]. Handling them stays in
//! `app.rs` and, on the screens a view state owns (`app::views`), in that state; tests
//! in `app.rs` keep the table and the handlers in step. Every phase 1 key is here,
//! also those of views that are still stubs.

use super::app::Screen;
use super::ui::widgets::width;

/// One row of the key table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHelp {
    /// All keys that do this, for the overlay and the spec, e.g. `"↑↓ j k"`.
    /// Whitespace separates alternatives; no alternative repeats within a view.
    pub keys: &'static str,
    /// Short form for the footer, e.g. `"↑↓"`.
    pub footer_keys: &'static str,
    /// Short label for the footer, e.g. `"Move"`.
    pub label: &'static str,
    /// One line for the overlay and the spec.
    pub help: &'static str,
    /// Whether the footer may show it (it still has to fit, see [`footer`]).
    pub in_footer: bool,
}

const fn k(
    keys: &'static str,
    footer_keys: &'static str,
    label: &'static str,
    help: &'static str,
    in_footer: bool,
) -> KeyHelp {
    KeyHelp {
        keys,
        footer_keys,
        label,
        help,
        in_footer,
    }
}

const VIEW_KEYS: KeyHelp = k(
    "0-6",
    "0-6",
    "View",
    "Switch view, 0 Connections to 6 Search",
    true,
);
const HELP: KeyHelp = k("?", "?", "Help", "Show or hide this key list", true);
const ADD: KeyHelp = k("a", "a", "Add", "Add a connection", true);
const QUIT: KeyHelp = k("q ctrl-c", "q", "Quit", "Quit", true);

static GLOBAL: [KeyHelp; 4] = [VIEW_KEYS, HELP, ADD, QUIT];

const MOVE: KeyHelp = k("↑↓ j k", "↑↓", "Move", "Move the selection", true);
const PAGE: KeyHelp = k("PgUp PgDn", "PgUp", "Page", "Move one page", false);
const ENDS: KeyHelp = k(
    "Home End g G",
    "Home",
    "First/last",
    "Jump to the first or last row",
    false,
);
const PANEL: KeyHelp = k(
    "tab shift-tab",
    "tab",
    "Panel",
    "Next or previous panel",
    true,
);
/// On Streams and Search: `tab` moves between the list and the selected row's details.
const FOCUS: KeyHelp = k(
    "tab shift-tab",
    "tab",
    "Details",
    "Move between the list and the details",
    true,
);
const COPY_PATTERN: KeyHelp = k(
    "y",
    "y",
    "Copy",
    "Copy the selected pattern to the clipboard",
    false,
);
const COPY_KEY: KeyHelp = k(
    "y",
    "y",
    "Copy",
    "Copy the selected key name to the clipboard",
    false,
);
const COPY_CMD: KeyHelp = k(
    "Y",
    "Y",
    "Copy cmd",
    "Copy a redis-cli --scan command for the pattern",
    false,
);
const RESCAN: KeyHelp = k(
    "r",
    "r",
    "Rescan",
    "Scan again, keeping old results meanwhile",
    true,
);
const SNAPSHOT: KeyHelp = k("s", "s", "Snapshot", "Save a scan snapshot file", true);
const REFRESH: KeyHelp = k("r", "r", "Refresh", "Refresh this panel now", true);
const PAUSE: KeyHelp = k("p", "p", "Pause", "Pause or resume polling", true);

static CONNECTIONS: [KeyHelp; 5] = [
    MOVE,
    PAGE,
    ENDS,
    k("enter", "enter", "Connect", "Connect and scan", true),
    k(
        "r",
        "r",
        "Rescan Docker",
        "Look for Docker containers again",
        true,
    ),
];

static PATTERNS: [KeyHelp; 12] = [
    MOVE,
    PAGE,
    ENDS,
    k(
        "→ ← l h",
        "→←",
        "Fold",
        "Expand or collapse the selected group",
        true,
    ),
    k(
        "enter",
        "enter",
        "Open",
        "Fold a group, or list a pattern's keys",
        true,
    ),
    k("/", "/", "Filter", "Filter patterns by name", true),
    k("esc", "esc", "Clear", "Clear the filter", false),
    k(
        "o",
        "o",
        "Sort",
        "Sort by memory, keys, avg size, no-TTL % or name",
        true,
    ),
    COPY_PATTERN,
    COPY_CMD,
    RESCAN,
    SNAPSHOT,
];

static KEY_LIST: [KeyHelp; 8] = [
    MOVE,
    PAGE,
    ENDS,
    k("/", "/", "Filter", "Filter the loaded keys by name", true),
    k("o", "o", "Sort", "Sort the loaded keys", true),
    k("n", "n", "More", "Load 1,000 more keys", true),
    COPY_KEY,
    k(
        "esc",
        "esc",
        "Back",
        "Clear the filter, or go back to Patterns",
        true,
    ),
];

static TTL_DIFF: [KeyHelp; 8] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    COPY_PATTERN,
    COPY_CMD,
    RESCAN,
    SNAPSHOT,
];

static HOT: [KeyHelp; 8] = [
    MOVE, PAGE, ENDS, PANEL, COPY_KEY, COPY_CMD, RESCAN, SNAPSHOT,
];

static BIG_KEYS: [KeyHelp; 7] = [MOVE, PAGE, ENDS, PANEL, COPY_KEY, RESCAN, SNAPSHOT];

static EXPIRY: [KeyHelp; 3] = [PANEL, RESCAN, SNAPSHOT];

static OVERVIEW: [KeyHelp; 4] = [
    PANEL,
    PAUSE,
    k(
        "+ -",
        "+-",
        "Interval",
        "Poll more or less often, 1 to 60 s",
        true,
    ),
    REFRESH,
];

static CLIENTS: [KeyHelp; 8] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    k(
        "enter",
        "enter",
        "List",
        "List the selected group's connections",
        true,
    ),
    k("esc", "esc", "Back", "Back to the groups", false),
    PAUSE,
    REFRESH,
];

static SLOW_LOG: [KeyHelp; 6] = [MOVE, PAGE, ENDS, PANEL, PAUSE, REFRESH];

static STREAMS: [KeyHelp; 6] = [MOVE, PAGE, ENDS, FOCUS, PAUSE, REFRESH];

static PUBSUB: [KeyHelp; 5] = [MOVE, PAGE, ENDS, PAUSE, REFRESH];

static SEARCH: [KeyHelp; 5] = [MOVE, PAGE, ENDS, FOCUS, REFRESH];

/// Keys that work in every view.
pub fn global() -> &'static [KeyHelp] {
    &GLOBAL
}

/// Keys of one screen, not counting [`global`].
pub fn for_screen(screen: Screen) -> &'static [KeyHelp] {
    match screen {
        Screen::Connections => &CONNECTIONS,
        Screen::Patterns => &PATTERNS,
        Screen::KeyList => &KEY_LIST,
        Screen::Ttl | Screen::Diff => &TTL_DIFF,
        Screen::HotKeys => &HOT,
        Screen::BigKeys => &BIG_KEYS,
        Screen::Expiry => &EXPIRY,
        Screen::Overview => &OVERVIEW,
        Screen::Clients => &CLIENTS,
        Screen::SlowLog => &SLOW_LOG,
        Screen::Streams => &STREAMS,
        Screen::PubSub => &PUBSUB,
        Screen::Search => &SEARCH,
    }
}

fn find(screen: Screen, keys: &str) -> (&'static str, &'static str) {
    let h = for_screen(screen)
        .iter()
        .chain(global())
        .find(|h| h.keys == keys)
        .unwrap_or_else(|| panic!("footer key {keys:?} is not in the table"));
    (h.footer_keys, h.label)
}

/// Footer hints as `(key, label)`, in display order. `ready` is whether the screen has
/// something to work on: a scan result on Analysis, a session on Server, Streams,
/// Pub/Sub and Search. Without it they show how to get one.
///
/// Snapshot on Patterns, rescan on Patterns, and the page and copy keys everywhere,
/// are left out because they do not fit in 80 columns; the overlay lists them.
pub fn footer(screen: Screen, ready: bool) -> Vec<(&'static str, &'static str)> {
    const MOVE: &str = "↑↓ j k";
    const TAB: &str = "tab shift-tab";
    let order: &[&str] = match screen {
        Screen::Connections => &[MOVE, "enter", "a", "r", "0-6", "?", "q ctrl-c"],
        Screen::Patterns => &[MOVE, "→ ← l h", "enter", "/", "o", "0-6", "?", "q ctrl-c"],
        Screen::KeyList => &[MOVE, "/", "o", "n", "esc", "0-6", "?", "q ctrl-c"],
        Screen::Ttl | Screen::Diff | Screen::HotKeys | Screen::BigKeys if ready => {
            &[MOVE, TAB, "r", "s", "0-6", "?", "q ctrl-c"]
        }
        Screen::Expiry if ready => &[TAB, "r", "s", "0-6", "?", "q ctrl-c"],
        Screen::Overview if ready => &[TAB, "p", "+ -", "r", "0-6", "?", "q ctrl-c"],
        Screen::Clients if ready => &[TAB, MOVE, "enter", "r", "0-6", "?", "q ctrl-c"],
        Screen::SlowLog if ready => &[TAB, MOVE, "p", "r", "0-6", "?", "q ctrl-c"],
        Screen::Streams if ready => &[MOVE, TAB, "p", "r", "0-6", "?", "q ctrl-c"],
        Screen::PubSub if ready => &[MOVE, "p", "r", "0-6", "?", "q ctrl-c"],
        Screen::Search if ready => &[MOVE, TAB, "r", "0-6", "?", "q ctrl-c"],
        s if s.view().panels().is_empty() => &["0-6", "a", "?", "q ctrl-c"],
        _ => &[TAB, "0-6", "a", "?", "q ctrl-c"],
    };
    order.iter().map(|keys| find(screen, keys)).collect()
}

/// Columns the footer takes as drawn: a space, then each key and ` {label}   `,
/// without the trailing padding.
pub fn footer_width(hints: &[(&str, &str)]) -> usize {
    let w: usize = 1 + hints
        .iter()
        .map(|(k, l)| width(k) + 1 + width(l) + 3)
        .sum::<usize>();
    w.saturating_sub(3)
}

fn table(out: &mut String, rows: &[KeyHelp]) {
    out.push_str("| Keys | Action |\n|---|---|\n");
    for h in rows {
        let keys: Vec<String> = h
            .keys
            .split_whitespace()
            .map(|t| format!("`{t}`"))
            .collect();
        out.push_str(&format!("| {} | {} |\n", keys.join(" "), h.help));
    }
}

/// The whole key table as Markdown, for `spec.md`.
pub fn markdown() -> String {
    let mut out = String::from("#### Everywhere\n\n");
    table(&mut out, global());
    for s in Screen::ALL {
        out.push_str(&format!("\n#### {}\n\n", s.title()));
        table(&mut out, for_screen(s));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `spec.md` holds the key table exactly as [`markdown`] renders it, so the spec,
    /// the footer and the overlay can't drift apart.
    #[test]
    fn spec_lists_every_key() {
        // Windows checkouts may turn line endings into CRLF.
        let spec = include_str!("../../spec.md").replace("\r\n", "\n");
        assert!(
            spec.contains(&markdown()),
            "spec.md's key table differs from keymap::markdown():\n{}",
            markdown()
        );
    }

    #[test]
    fn no_duplicate_key_within_a_view() {
        for v in Screen::ALL {
            let mut seen = std::collections::HashSet::new();
            for h in global().iter().chain(for_screen(v)) {
                for t in h.keys.split_whitespace() {
                    assert!(seen.insert(t), "{t:?} is bound twice in {}", v.title());
                }
            }
        }
    }

    #[test]
    fn every_view_has_entries() {
        assert!(!global().is_empty());
        for v in Screen::ALL {
            assert!(!for_screen(v).is_empty(), "{}", v.title());
            assert!(
                for_screen(v)
                    .iter()
                    .all(|h| !h.keys.is_empty() && !h.help.is_empty())
            );
        }
    }

    #[test]
    fn footer_is_a_subset_of_the_table() {
        for v in Screen::ALL {
            for report in [false, true] {
                for (key, label) in footer(v, report) {
                    let hit = global()
                        .iter()
                        .chain(for_screen(v))
                        .find(|h| h.footer_keys == key && h.label == label)
                        .unwrap_or_else(|| panic!("{key} {label} missing in {}", v.title()));
                    assert!(hit.in_footer, "{key} {label} is not marked in_footer");
                }
            }
        }
    }

    #[test]
    fn footer_hints_per_screen() {
        let names = |v, r| footer(v, r).iter().map(|h| h.1).collect::<Vec<_>>();
        assert_eq!(
            names(Screen::Connections, false),
            [
                "Move",
                "Connect",
                "Add",
                "Rescan Docker",
                "View",
                "Help",
                "Quit"
            ]
        );
        assert_eq!(
            names(Screen::Patterns, true),
            [
                "Move", "Fold", "Open", "Filter", "Sort", "View", "Help", "Quit"
            ]
        );
        assert_eq!(
            names(Screen::Ttl, true),
            [
                "Move", "Panel", "Rescan", "Snapshot", "View", "Help", "Quit"
            ]
        );
        assert_eq!(
            names(Screen::Diff, false),
            ["Panel", "View", "Add", "Help", "Quit"]
        );
        assert_eq!(
            names(Screen::Search, false),
            ["View", "Add", "Help", "Quit"]
        );
        assert_eq!(
            names(Screen::Overview, true),
            [
                "Panel", "Pause", "Interval", "Refresh", "View", "Help", "Quit"
            ]
        );
        assert_eq!(
            names(Screen::KeyList, true),
            [
                "Move", "Filter", "Sort", "More", "Back", "View", "Help", "Quit"
            ]
        );
    }

    /// `tab` is in the table exactly where it does something: on every panel of a view
    /// with panels, and on Streams and Search, where it moves to the details.
    #[test]
    fn tab_is_listed_on_every_panel() {
        for s in Screen::ALL {
            let listed = for_screen(s).iter().any(|h| h.keys == "tab shift-tab");
            let expected =
                !s.view().panels().is_empty() || matches!(s, Screen::Streams | Screen::Search);
            assert_eq!(listed, expected, "{}", s.title());
        }
    }

    #[test]
    fn footer_fits_80_columns() {
        for v in Screen::ALL {
            for report in [false, true] {
                let w = footer_width(&footer(v, report));
                assert!(w <= 80, "{} footer is {w} columns", v.title());
            }
        }
    }

    #[test]
    fn markdown_lists_every_view() {
        let md = markdown();
        for v in Screen::ALL {
            assert!(md.contains(&format!("#### {}\n", v.title())));
        }
        assert!(md.contains("| `?` |"));
    }
}
