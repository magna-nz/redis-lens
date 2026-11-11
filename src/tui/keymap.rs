//! The single source of truth for keys: the footer, the help overlay and the key
//! table in `spec.md` are all rendered from the data here.
//!
//! This module only describes keys. Handling them stays in `app.rs`; a test there
//! (or the wiring agent's) keeps the two in step.

use super::app::View;
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
    "0-4",
    "0-4",
    "View",
    "Switch view, 0 Connections to 4 Hot keys",
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
    k("enter", "enter", "Toggle", "Expand or collapse", false),
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

static TTL_DIFF: [KeyHelp; 7] = [MOVE, PAGE, ENDS, COPY_PATTERN, COPY_CMD, RESCAN, SNAPSHOT];

static HOT: [KeyHelp; 7] = [MOVE, PAGE, ENDS, COPY_KEY, COPY_CMD, RESCAN, SNAPSHOT];

/// Keys that work in every view.
pub fn global() -> &'static [KeyHelp] {
    &GLOBAL
}

/// Keys of one view, not counting [`global`].
pub fn for_view(view: View) -> &'static [KeyHelp] {
    match view {
        View::Connections => &CONNECTIONS,
        View::Patterns => &PATTERNS,
        View::Ttl | View::Diff => &TTL_DIFF,
        View::HotKeys => &HOT,
    }
}

fn find(view: View, keys: &str) -> (&'static str, &'static str) {
    let h = for_view(view)
        .iter()
        .chain(global())
        .find(|h| h.keys == keys)
        .unwrap_or_else(|| panic!("footer key {keys:?} is not in the table"));
    (h.footer_keys, h.label)
}

/// Footer hints as `(key, label)`, in display order. `has_report` is whether a scan
/// result exists; without one, views 2-4 have nothing to move through.
///
/// Rescan on Patterns, and the page and copy keys everywhere, are left out because
/// they do not fit in 80 columns; the overlay lists them.
pub fn footer(view: View, has_report: bool) -> Vec<(&'static str, &'static str)> {
    let order: &[&str] = match view {
        View::Connections => &["↑↓ j k", "enter", "a", "r", "0-4", "?", "q ctrl-c"],
        View::Patterns => &["↑↓ j k", "→ ← l h", "/", "o", "s", "0-4", "?", "q ctrl-c"],
        View::Ttl | View::Diff | View::HotKeys if has_report => {
            &["↑↓ j k", "r", "s", "0-4", "?", "q ctrl-c"]
        }
        _ => &["0-4", "a", "?", "q ctrl-c"],
    };
    order.iter().map(|keys| find(view, keys)).collect()
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
    for v in View::ALL {
        out.push_str(&format!("\n#### {}\n\n", v.title()));
        table(&mut out, for_view(v));
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
        for v in View::ALL {
            let mut seen = std::collections::HashSet::new();
            for h in global().iter().chain(for_view(v)) {
                for t in h.keys.split_whitespace() {
                    assert!(seen.insert(t), "{t:?} is bound twice in {}", v.title());
                }
            }
        }
    }

    #[test]
    fn every_view_has_entries() {
        assert!(!global().is_empty());
        for v in View::ALL {
            assert!(!for_view(v).is_empty(), "{}", v.title());
            assert!(
                for_view(v)
                    .iter()
                    .all(|h| !h.keys.is_empty() && !h.help.is_empty())
            );
        }
    }

    #[test]
    fn footer_is_a_subset_of_the_table() {
        for v in View::ALL {
            for report in [false, true] {
                for (key, label) in footer(v, report) {
                    let hit = global()
                        .iter()
                        .chain(for_view(v))
                        .find(|h| h.footer_keys == key && h.label == label)
                        .unwrap_or_else(|| panic!("{key} {label} missing in {}", v.title()));
                    assert!(hit.in_footer, "{key} {label} is not marked in_footer");
                }
            }
        }
    }

    #[test]
    fn footer_keeps_todays_hints() {
        let names = |v, r| footer(v, r).iter().map(|h| h.1).collect::<Vec<_>>();
        assert_eq!(
            names(View::Connections, false),
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
            names(View::Patterns, true),
            [
                "Move", "Fold", "Filter", "Sort", "Snapshot", "View", "Help", "Quit"
            ]
        );
        assert_eq!(
            names(View::Ttl, true),
            ["Move", "Rescan", "Snapshot", "View", "Help", "Quit"]
        );
        assert_eq!(names(View::Diff, false), ["View", "Add", "Help", "Quit"]);
        assert_eq!(names(View::HotKeys, false), ["View", "Add", "Help", "Quit"]);
    }

    #[test]
    fn footer_fits_80_columns() {
        for v in View::ALL {
            for report in [false, true] {
                let w = footer_width(&footer(v, report));
                assert!(w <= 80, "{} footer is {w} columns", v.title());
            }
        }
    }

    #[test]
    fn markdown_lists_every_view() {
        let md = markdown();
        for v in View::ALL {
            assert!(md.contains(&format!("#### {}\n", v.title())));
        }
        assert!(md.contains("| `?` |"));
    }
}
