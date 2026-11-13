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

const COMMAND_LINE_KEY: KeyHelp = k(":", ":", "Command", "Open the command line", false);

static GLOBAL: [KeyHelp; 5] = [VIEW_KEYS, HELP, ADD, COMMAND_LINE_KEY, QUIT];

/// The keys of the command line panel while it is open (`:`). Its input line replaces
/// the footer, so none of them is in a footer.
static COMMAND_LINE: [KeyHelp; 7] = [
    k(
        "enter",
        "enter",
        "Run",
        "Run the command (writes go through the write gate)",
        false,
    ),
    k(
        "↑↓",
        "↑↓",
        "History",
        "Step through this session's commands",
        false,
    ),
    k(
        "tab",
        "tab",
        "Complete",
        "Complete a command, subcommand or loaded key name",
        false,
    ),
    k("PgUp PgDn", "PgUp", "Scroll", "Scroll the output", false),
    k("ctrl-l", "ctrl-l", "Clear", "Clear the output", false),
    k(
        "ctrl-u",
        "ctrl-u",
        "Clear line",
        "Clear the input line",
        false,
    ),
    k(
        "esc",
        "esc",
        "Close",
        "Close the command line, or cancel a question",
        false,
    ),
];

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

static CONNECTIONS: [KeyHelp; 6] = [
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
    k(
        "w",
        "w",
        "Writes",
        "Turn writes on or off for the selected profile",
        false,
    ),
];

static PATTERNS: [KeyHelp; 14] = [
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
    k(
        "esc",
        "esc",
        "Clear",
        "Clear the filter, or show all nodes again",
        false,
    ),
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
    k(
        "x",
        "x",
        "Expire",
        "Set a TTL on every key of the pattern",
        false,
    ),
    k("D", "D", "Delete", "Delete every key of the pattern", false),
];

static KEY_LIST: [KeyHelp; 10] = [
    MOVE,
    PAGE,
    ENDS,
    k("/", "/", "Filter", "Filter the loaded keys by name", true),
    k("o", "o", "Sort", "Sort the loaded keys", true),
    k("n", "n", "More", "Load 1,000 more keys", true),
    k(
        "enter",
        "enter",
        "Open",
        "Show the selected key and its value",
        false,
    ),
    COPY_KEY,
    k(
        "esc",
        "esc",
        "Back",
        "Clear the filter, or go back to Patterns",
        true,
    ),
    k("N", "N", "New key", "Add a key", false),
];

static KEY_DETAIL: [KeyHelp; 14] = [
    k(
        "↑↓ j k",
        "↑↓",
        "Move",
        "Move the selection, or scroll the value",
        true,
    ),
    PAGE,
    ENDS,
    k(
        "tab shift-tab",
        "tab",
        "Value",
        "Move between the items and the value",
        true,
    ),
    k(
        "f",
        "f",
        "Format",
        "Show the value as text, hex, JSON or MessagePack",
        true,
    ),
    k("n", "n", "More", "Load the next 100 items", true),
    k(
        "y",
        "y",
        "Copy",
        "Copy the selected value as shown to the clipboard",
        false,
    ),
    k("esc", "esc", "Back", "Go back to the key list", true),
    k(
        "e",
        "e",
        "Edit",
        "Edit the selected field, element or value",
        false,
    ),
    k("+", "+", "Add", "Add a field, element or member", false),
    k("t", "t", "TTL", "Set or remove the TTL", false),
    k(
        "R",
        "R",
        "Rename",
        "Rename the key (never overwrites one)",
        false,
    ),
    k(
        "d",
        "d",
        "Delete item",
        "Delete the selected field, element or member",
        false,
    ),
    k("D", "D", "Delete key", "Delete the key", false),
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

static CLIENTS: [KeyHelp; 9] = [
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
    k("K", "K", "Kill", "Kill the selected connection", false),
];

static SLOW_LOG: [KeyHelp; 8] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    PAUSE,
    REFRESH,
    k(
        "c",
        "c",
        "Threshold",
        "Change the slow log threshold or length",
        false,
    ),
    k("R", "R", "Reset", "Reset the slow log", false),
];

static MONITOR: [KeyHelp; 6] = [
    k("↑↓ j k", "↑↓", "Scroll", "Scroll the lines", true),
    PAGE,
    ENDS,
    PANEL,
    k(
        "enter",
        "enter",
        "Start",
        "Start MONITOR after asking; it stops after 30 s or 10,000 lines",
        true,
    ),
    k("esc", "esc", "Stop", "Stop MONITOR now", true),
];

/// `f` on the screens that show a value: the key detail, stream entries, a Search result.
const FORMAT: KeyHelp = k(
    "f",
    "f",
    "Format",
    "Show the value as text, hex, JSON or MessagePack",
    true,
);
/// `y` on the screens that show a value.
const COPY_VALUE: KeyHelp = k(
    "y",
    "y",
    "Copy",
    "Copy the selected value as shown to the clipboard",
    false,
);

static STREAMS: [KeyHelp; 14] = [
    k(
        "↑↓ j k",
        "↑↓",
        "Move",
        "Move the selection, or scroll the value",
        true,
    ),
    PAGE,
    ENDS,
    k(
        "tab shift-tab",
        "tab",
        "Details",
        "Move between the list and the details, or the entries and the value",
        true,
    ),
    k(
        "enter",
        "enter",
        "Entries",
        "List the selected stream's entries",
        true,
    ),
    k("n", "n", "More", "Load 100 more entries", true),
    FORMAT,
    COPY_VALUE,
    k("esc", "esc", "Back", "Go back to the stream list", true),
    PAUSE,
    k(
        "r",
        "r",
        "Refresh",
        "Refresh this panel now, or read the entries again",
        true,
    ),
    k(
        "c",
        "c",
        "Claim",
        "Claim a consumer's pending entries for another consumer",
        false,
    ),
    k(
        "A",
        "A",
        "Ack",
        "Acknowledge the selected pending entry",
        false,
    ),
    k("X", "X", "Trim", "Trim the stream", false),
];

static PUBSUB: [KeyHelp; 12] = [
    MOVE,
    PAGE,
    ENDS,
    k(
        "enter",
        "enter",
        "Subscribe",
        "Subscribe to the selected channel",
        true,
    ),
    k("s", "s", "Sub", "Subscribe to a channel or pattern", false),
    k("space", "space", "Pause", "Pause or resume the tail", true),
    k("c", "c", "Clear", "Clear the tail", false),
    k("/", "/", "Filter", "Filter the tail", true),
    k(
        "esc",
        "esc",
        "Stop",
        "Clear the filter, or stop the tail",
        false,
    ),
    PAUSE,
    REFRESH,
    k("P", "P", "Publish", "Publish a message", false),
];

static SEARCH: [KeyHelp; 14] = [
    k(
        "↑↓ j k",
        "↑↓",
        "Move",
        "Move the selection, or scroll the text",
        true,
    ),
    PAGE,
    ENDS,
    k(
        "tab shift-tab",
        "tab",
        "Details",
        "Move between the list and the details, or the fields and the value",
        true,
    ),
    k("/", "/", "Query", "Run a query on the selected index", true),
    k("enter", "enter", "Open", "Open the selected result", true),
    k("e", "e", "Explain", "Explain the query", true),
    k("P", "P", "Profile", "Profile the query", true),
    FORMAT,
    COPY_VALUE,
    k(
        "esc",
        "esc",
        "Back",
        "Go back to the indexes and results",
        true,
    ),
    REFRESH,
    k("C", "C", "Create", "Create an index", false),
    k(
        "D",
        "D",
        "Drop",
        "Drop the selected index (its documents stay)",
        false,
    ),
];

static NODES: [KeyHelp; 7] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    k(
        "enter",
        "enter",
        "Patterns",
        "Show the selected node's patterns on 1 Patterns",
        true,
    ),
    RESCAN,
    SNAPSHOT,
];

/// Keys that work in every view.
pub fn global() -> &'static [KeyHelp] {
    &GLOBAL
}

/// Keys of the command line panel while it is open.
pub fn command_line() -> &'static [KeyHelp] {
    &COMMAND_LINE
}

/// Keys of one screen, not counting [`global`].
pub fn for_screen(screen: Screen) -> &'static [KeyHelp] {
    match screen {
        Screen::Connections => &CONNECTIONS,
        Screen::Patterns => &PATTERNS,
        Screen::KeyList => &KEY_LIST,
        Screen::KeyDetail => &KEY_DETAIL,
        Screen::Ttl | Screen::Diff => &TTL_DIFF,
        Screen::HotKeys => &HOT,
        Screen::BigKeys => &BIG_KEYS,
        Screen::Expiry => &EXPIRY,
        Screen::Overview => &OVERVIEW,
        Screen::Clients => &CLIENTS,
        Screen::SlowLog => &SLOW_LOG,
        Screen::Monitor => &MONITOR,
        Screen::Streams => &STREAMS,
        Screen::PubSub => &PUBSUB,
        Screen::Search => &SEARCH,
        Screen::Nodes => &NODES,
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
/// Snapshot on Patterns and Nodes, rescan on Patterns, the page and copy keys everywhere,
/// and on Pub/Sub `s`, `c`, `esc`, `p` and `r`, are left out because they do not fit in
/// 80 columns; the overlay lists them.
pub fn footer(screen: Screen, ready: bool) -> Vec<(&'static str, &'static str)> {
    const MOVE: &str = "↑↓ j k";
    const TAB: &str = "tab shift-tab";
    let order: &[&str] = match screen {
        Screen::Connections => &[MOVE, "enter", "a", "r", "0-6", "?", "q ctrl-c"],
        Screen::Patterns => &[MOVE, "→ ← l h", "enter", "/", "o", "0-6", "?", "q ctrl-c"],
        Screen::KeyList => &[MOVE, "/", "o", "n", "esc", "0-6", "?", "q ctrl-c"],
        Screen::KeyDetail => &[MOVE, TAB, "f", "n", "esc", "0-6", "?", "q ctrl-c"],
        Screen::Ttl | Screen::Diff | Screen::HotKeys | Screen::BigKeys if ready => {
            &[MOVE, TAB, "r", "s", "0-6", "?", "q ctrl-c"]
        }
        Screen::Expiry if ready => &[TAB, "r", "s", "0-6", "?", "q ctrl-c"],
        Screen::Overview if ready => &[TAB, "p", "+ -", "r", "0-6", "?", "q ctrl-c"],
        Screen::Clients if ready => &[TAB, MOVE, "enter", "r", "0-6", "?", "q ctrl-c"],
        Screen::SlowLog if ready => &[TAB, MOVE, "p", "r", "0-6", "?", "q ctrl-c"],
        Screen::Monitor if ready => &[TAB, "enter", "esc", MOVE, "0-6", "?", "q ctrl-c"],
        Screen::Streams if ready => &[MOVE, TAB, "enter", "p", "0-6", "?", "q ctrl-c"],
        Screen::PubSub if ready => &[MOVE, "enter", "space", "/", "0-6", "?", "q ctrl-c"],
        Screen::Search if ready => &[MOVE, TAB, "/", "enter", "0-6", "?", "q ctrl-c"],
        Screen::Nodes if ready => &[MOVE, TAB, "enter", "r", "0-6", "?", "q ctrl-c"],
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
    out.push_str("\n#### Command line panel · keys\n\n");
    table(&mut out, command_line());
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
            names(Screen::Streams, true),
            [
                "Move", "Details", "Entries", "Pause", "View", "Help", "Quit"
            ]
        );
        assert_eq!(
            names(Screen::Search, true),
            ["Move", "Details", "Query", "Open", "View", "Help", "Quit"]
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
        for ready in [false, true] {
            assert_eq!(
                names(Screen::KeyDetail, ready),
                [
                    "Move", "Value", "Format", "More", "Back", "View", "Help", "Quit"
                ]
            );
        }
        assert_eq!(
            names(Screen::PubSub, true),
            [
                "Move",
                "Subscribe",
                "Pause",
                "Filter",
                "View",
                "Help",
                "Quit"
            ]
        );
        assert_eq!(
            names(Screen::Monitor, true),
            ["Panel", "Start", "Stop", "Scroll", "View", "Help", "Quit"]
        );
        assert_eq!(
            names(Screen::Monitor, false),
            ["Panel", "View", "Add", "Help", "Quit"]
        );
        assert_eq!(
            names(Screen::Nodes, true),
            [
                "Move", "Panel", "Patterns", "Rescan", "View", "Help", "Quit"
            ]
        );
    }

    /// `tab` is in the table exactly where it does something: on every panel of a view
    /// with panels, on Streams and Search, where it moves to the details, and on the key
    /// detail, where it moves to the value.
    #[test]
    fn tab_is_listed_on_every_panel() {
        for s in Screen::ALL {
            let listed = for_screen(s).iter().any(|h| h.keys == "tab shift-tab");
            let expected = !s.view().panels().is_empty()
                || matches!(s, Screen::Streams | Screen::Search | Screen::KeyDetail);
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

    #[test]
    fn command_line_keys_are_listed_once() {
        let mut seen = std::collections::HashSet::new();
        for h in command_line() {
            for t in h.keys.split_whitespace() {
                assert!(seen.insert(t), "{t:?} is bound twice on the command line");
            }
        }
        assert!(markdown().contains("#### Command line panel · keys\n"));
        assert!(global().iter().any(|h| h.keys == ":"));
    }
}
