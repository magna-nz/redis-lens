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

const CONNECTIONS_KEY: KeyHelp = k(
    "0",
    "0",
    "Connections",
    "Connections, the home screen",
    true,
);
const AREAS: KeyHelp = k(
    "1 2 3 4 5",
    "1-5",
    "Areas",
    "Overview, Browse, Analyze, Server, Pub/Sub",
    false,
);
const MORE: KeyHelp = k(
    "v",
    "v",
    "More",
    "Show more columns and details, or fewer again",
    true,
);
const HELP: KeyHelp = k("?", "?", "Help", "Help here; type to find any action", true);
const QUIT: KeyHelp = k("q ctrl-c", "q", "Quit", "Quit", false);

const COMMAND_LINE_KEY: KeyHelp = k(":", ":", "Command", "Open the command line", true);

const DATABASES_KEY: KeyHelp = k(
    "b",
    "b",
    "Database",
    "Switch the database (once connected)",
    false,
);

static GLOBAL: [KeyHelp; 7] = [
    CONNECTIONS_KEY,
    AREAS,
    MORE,
    HELP,
    COMMAND_LINE_KEY,
    DATABASES_KEY,
    QUIT,
];

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
    "Next panel",
    "Next or previous panel, nothing else",
    true,
);
const COPY_PATTERN: KeyHelp = k(
    "y",
    "y",
    "Copy",
    "Copy the selected pattern to the clipboard",
    true,
);
const COPY_KEY: KeyHelp = k(
    "y",
    "y",
    "Copy",
    "Copy the selected key name to the clipboard",
    true,
);
const COPY_CMD: KeyHelp = k(
    "Y",
    "Y",
    "Copy cmd",
    "Copy a redis-cli --scan command for the pattern",
    false,
);
const ANALYZE: KeyHelp = k(
    "r",
    "r",
    "Analyze",
    "Analyze again (Quick or Deep); old results stay",
    true,
);
const SNAPSHOT: KeyHelp = k(
    "s",
    "s",
    "Snapshot",
    "Save a snapshot of the analysis",
    true,
);
const REFRESH: KeyHelp = k("r", "r", "Refresh", "Refresh this panel now", true);
const PAUSE: KeyHelp = k("p", "p", "Pause", "Pause or resume polling", true);
/// `[` `]` on the screens that read the picked node of a cluster: only in `?`, so the
/// footers stay short.
const PICK_NODE: KeyHelp = k(
    "[ ]",
    "[ ]",
    "Node",
    "Pick the previous or next node (cluster only)",
    false,
);

static CONNECTIONS: [KeyHelp; 9] = [
    MOVE,
    PAGE,
    ENDS,
    k(
        "enter",
        "enter",
        "Connect",
        "Connect: reads the server's own figures, no keys",
        true,
    ),
    k("a", "a", "Add", "Add a connection", true),
    k(
        "w",
        "w",
        "Writes",
        "Turn writes on or off for the selected profile",
        true,
    ),
    k(
        "/",
        "/",
        "Filter",
        "Filter the profiles by name or tag",
        true,
    ),
    k("esc", "esc", "Clear", "Clear the filter", false),
    k(
        "r",
        "r",
        "Docker",
        "Look for Docker containers again",
        false,
    ),
];

static OVERVIEW: [KeyHelp; 9] = [
    k(
        "r",
        "r",
        "Analyze keys",
        "Analyze the keys: Quick (10,000) or Deep (100,000)",
        true,
    ),
    MOVE,
    PAGE,
    ENDS,
    k(
        "enter",
        "enter",
        "Go there",
        "Go to the selected finding's panel",
        true,
    ),
    k(
        "esc",
        "esc",
        "Stop",
        "Stop the analysis and keep what it has sampled",
        true,
    ),
    k("p", "p", "Pause", "Pause or resume the live figures", true),
    k(
        "+ -",
        "+-",
        "Interval",
        "Poll the live figures more or less often, 1 to 60 s",
        true,
    ),
    PICK_NODE,
];

static PATTERNS: [KeyHelp; 15] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    k(
        "→ l enter",
        "→",
        "Open",
        "Open a pattern's keys, or expand a group and go in",
        true,
    ),
    k(
        "← h",
        "←",
        "Fold",
        "Collapse a group, or go up to it",
        false,
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
    ANALYZE,
    SNAPSHOT,
    k(
        "x",
        "x",
        "Expire",
        "Set a TTL on every key of the pattern (writes)",
        false,
    ),
    k(
        "D",
        "D",
        "Delete",
        "Delete every key of the pattern (writes)",
        false,
    ),
];

static KEY_LIST: [KeyHelp; 12] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    k(
        "→ l enter",
        "→",
        "Value",
        "Show the selected key's value",
        true,
    ),
    k(
        "← h esc",
        "esc",
        "Back",
        "Clear the filters first, then back to the patterns",
        true,
    ),
    k("/", "/", "Filter", "Filter the loaded keys by name", true),
    k("o", "o", "Sort", "Sort the loaded keys", false),
    k("t", "t", "Type", "Show one type of the loaded keys", false),
    k("n", "n", "More keys", "Load 1,000 more keys", true),
    COPY_KEY,
    k("N", "N", "New key", "Add a key (writes)", false),
];

static KEY_DETAIL: [KeyHelp; 15] = [
    k(
        "↑↓ j k",
        "↑↓",
        "Move",
        "Move the selection, or scroll the value",
        true,
    ),
    PAGE,
    ENDS,
    PANEL,
    k("→ l", "→", "Value", "Move into the value pane", true),
    k(
        "← h esc",
        "esc",
        "Back",
        "Back to the items, then to the key list",
        true,
    ),
    k(
        "f",
        "f",
        "Format",
        "Show the value as text, hex, JSON or MessagePack",
        true,
    ),
    k("n", "n", "More", "Load the next 100 items", false),
    k(
        "y",
        "y",
        "Copy",
        "Copy the selected value as shown to the clipboard",
        true,
    ),
    k(
        "e",
        "e",
        "Edit",
        "Edit the selected field, element or value (writes)",
        true,
    ),
    k(
        "+",
        "+",
        "Add",
        "Add a field, element or member (writes)",
        true,
    ),
    k("t", "t", "TTL", "Set or remove the TTL (writes)", false),
    k(
        "R",
        "R",
        "Rename",
        "Rename the key, never over another (writes)",
        false,
    ),
    k(
        "d",
        "d",
        "Delete",
        "Delete the selected field, element or member (writes)",
        true,
    ),
    k("D", "D", "Delete key", "Delete the key (writes)", false),
];

/// The forecast one level down Analyze › Expiry.
const FORECAST: KeyHelp = k(
    "→ l enter",
    "→",
    "Forecast",
    "Expiry forecast: the memory TTLs free over time",
    true,
);

static TTL: [KeyHelp; 9] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    FORECAST,
    COPY_PATTERN,
    COPY_CMD,
    ANALYZE,
    SNAPSHOT,
];

static DIFF: [KeyHelp; 8] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    COPY_PATTERN,
    COPY_CMD,
    ANALYZE,
    SNAPSHOT,
];

static HOT: [KeyHelp; 8] = [
    MOVE, PAGE, ENDS, PANEL, COPY_KEY, COPY_CMD, ANALYZE, SNAPSHOT,
];

static BIG_KEYS: [KeyHelp; 7] = [MOVE, PAGE, ENDS, PANEL, COPY_KEY, ANALYZE, SNAPSHOT];

static EXPIRY: [KeyHelp; 4] = [
    PANEL,
    k(
        "← h esc",
        "←",
        "Back",
        "Back to the patterns ranked by TTL risk",
        true,
    ),
    ANALYZE,
    SNAPSHOT,
];

static CLIENTS: [KeyHelp; 10] = [
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
    k(
        "K",
        "K",
        "Kill",
        "Kill the selected connection (writes)",
        false,
    ),
    PICK_NODE,
];

static SLOW_LOG: [KeyHelp; 9] = [
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
        "Change the slow log threshold or length (writes)",
        false,
    ),
    k("R", "R", "Reset", "Reset the slow log (writes)", false),
    PICK_NODE,
];

static MONITOR: [KeyHelp; 7] = [
    k("↑↓ j k", "↑↓", "Scroll", "Scroll the lines", true),
    PAGE,
    ENDS,
    PANEL,
    k(
        "enter",
        "enter",
        "Start",
        "Start MONITOR (asks; stops at 30 s or 10,000 lines)",
        true,
    ),
    k("esc", "esc", "Stop", "Stop MONITOR now", true),
    PICK_NODE,
];

/// `f` on the screens that show a value: a key's value, stream entries, a Search result.
const FORMAT: KeyHelp = k(
    "f",
    "f",
    "Format",
    "Show the value as text, hex, JSON or MessagePack",
    false,
);
/// `y` on the screens that show a value.
const COPY_VALUE: KeyHelp = k(
    "y",
    "y",
    "Copy",
    "Copy the selected value as shown to the clipboard",
    false,
);
/// `←` `h` on Streams and Search.
const LEFT: KeyHelp = k(
    "← h",
    "←",
    "Left",
    "Back to the left pane, or close the entries or result",
    false,
);

static STREAMS: [KeyHelp; 16] = [
    k(
        "↑↓ j k",
        "↑↓",
        "Move",
        "Move the selection, or scroll the value",
        true,
    ),
    PAGE,
    ENDS,
    PANEL,
    k(
        "→ l",
        "→",
        "Details",
        "Into the right pane: the details or an entry's value",
        true,
    ),
    LEFT,
    k(
        "enter",
        "enter",
        "Entries",
        "List the selected stream's entries",
        true,
    ),
    k("n", "n", "More", "Load 100 more entries", false),
    FORMAT,
    COPY_VALUE,
    k(
        "esc",
        "esc",
        "Back",
        "Close the entries, or back to the list",
        true,
    ),
    PAUSE,
    k(
        "r",
        "r",
        "Refresh",
        "Refresh this panel now, or read the entries again",
        false,
    ),
    k(
        "c",
        "c",
        "Claim",
        "Claim pending entries for another consumer (writes)",
        false,
    ),
    k(
        "A",
        "A",
        "Ack",
        "Acknowledge the selected pending entry (writes)",
        false,
    ),
    k("X", "X", "Trim", "Trim the stream (writes)", false),
];

static PUBSUB: [KeyHelp; 14] = [
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
    k(
        "S",
        "S",
        "Shard",
        "Subscribe to a shard channel (SSUBSCRIBE)",
        false,
    ),
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
    k(
        "P",
        "P",
        "Publish",
        "Publish; SPUBLISH on shard channels (writes)",
        false,
    ),
    PICK_NODE,
];

static SEARCH: [KeyHelp; 17] = [
    k(
        "↑↓ j k",
        "↑↓",
        "Move",
        "Move the selection, or scroll the text",
        true,
    ),
    PAGE,
    ENDS,
    PANEL,
    k(
        "→ l",
        "→",
        "Details",
        "Into the right pane: the details or a result's value",
        true,
    ),
    LEFT,
    k("/", "/", "Query", "Run a query on the selected index", true),
    k("enter", "enter", "Open", "Open the selected result", true),
    k("e", "e", "Explain", "Explain the query", false),
    k("P", "P", "Profile", "Profile the query", false),
    FORMAT,
    COPY_VALUE,
    k(
        "esc",
        "esc",
        "Back",
        "Back to the indexes and results",
        true,
    ),
    REFRESH,
    k("C", "C", "Create", "Create an index (writes)", false),
    k(
        "D",
        "D",
        "Drop",
        "Drop the selected index, its documents stay (writes)",
        false,
    ),
    PICK_NODE,
];

static NODES: [KeyHelp; 7] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    k(
        "enter",
        "enter",
        "This node only",
        "Only this node's patterns in Browse › Keys",
        true,
    ),
    ANALYZE,
    SNAPSHOT,
];

static SLOTS: [KeyHelp; 6] = [
    k(
        "↑↓ j k",
        "↑↓",
        "Move",
        "Move through the hottest slots",
        true,
    ),
    PAGE,
    ENDS,
    PANEL,
    k(
        "o",
        "o",
        "Order",
        "Sort the hottest slots by keys, CPU, net in or out",
        true,
    ),
    k(
        "r",
        "r",
        "Refresh",
        "Read the slot stats again on every node",
        true,
    ),
];

/// Keys that work on every screen.
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
        Screen::Overview => &OVERVIEW,
        Screen::Patterns => &PATTERNS,
        Screen::KeyList => &KEY_LIST,
        Screen::KeyDetail => &KEY_DETAIL,
        Screen::Streams => &STREAMS,
        Screen::Search => &SEARCH,
        Screen::BigKeys => &BIG_KEYS,
        Screen::Ttl => &TTL,
        Screen::Expiry => &EXPIRY,
        Screen::Diff => &DIFF,
        Screen::HotKeys => &HOT,
        Screen::Nodes => &NODES,
        Screen::Slots => &SLOTS,
        Screen::Clients => &CLIENTS,
        Screen::SlowLog => &SLOW_LOG,
        Screen::Monitor => &MONITOR,
        Screen::PubSub => &PUBSUB,
    }
}

fn find_key(screen: Screen, keys: &str) -> (&'static str, &'static str) {
    let h = for_screen(screen)
        .iter()
        .chain(global())
        .find(|h| h.keys == keys)
        .unwrap_or_else(|| panic!("footer key {keys:?} is not in the table"));
    (h.footer_keys, h.label)
}

/// What the footer depends on besides the screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FooterCtx {
    /// A session exists (the live screens have something to work on).
    pub ready: bool,
    /// An analysis finished (or its results are kept during the next one).
    pub analyzed: bool,
    /// An analysis runs.
    pub analyzing: bool,
    /// Writes are on for the session's profile.
    pub writes: bool,
}

/// The 3 to 5 keys that matter on `screen`, as `(key, label)` in display order. The
/// footer puts [`footer_right`] after them, against the right edge.
pub fn footer(screen: Screen, c: FooterCtx) -> Vec<(&'static str, &'static str)> {
    const MOVE: &str = "↑↓ j k";
    const TAB: &str = "tab shift-tab";
    const OPEN: &str = "→ l enter";
    const BACK: &str = "← h esc";
    let order: &[&str] = match screen {
        Screen::Connections => &[MOVE, "enter", "a", "w", "/"],
        Screen::Overview if c.analyzing => &["esc", "p", "+ -"],
        Screen::Overview if c.analyzed => &[MOVE, "enter", "r", "v"],
        Screen::Overview if c.ready => &["r", MOVE, "enter", "v"],
        Screen::Patterns if c.analyzed => &[MOVE, OPEN, "/", "o", "v"],
        Screen::Patterns => &[MOVE, OPEN, "/", "r", TAB],
        Screen::KeyList => &[MOVE, OPEN, "/", "n", BACK],
        Screen::KeyDetail if c.writes => &[MOVE, "e", "+", "d", "f"],
        Screen::KeyDetail => &[MOVE, "→ l", "f", "y", BACK],
        Screen::Ttl if c.analyzed => &[MOVE, TAB, OPEN, "s"],
        Screen::Expiry if c.analyzed => &[TAB, BACK, "r", "s"],
        Screen::BigKeys | Screen::Diff | Screen::HotKeys if c.analyzed => {
            &[MOVE, TAB, "y", "r", "s"]
        }
        Screen::Nodes if c.analyzed => &[MOVE, "enter", TAB],
        Screen::BigKeys
        | Screen::Ttl
        | Screen::Expiry
        | Screen::Diff
        | Screen::HotKeys
        | Screen::Nodes
            if c.ready =>
        {
            &["r", TAB]
        }
        Screen::Streams if c.ready => &[MOVE, "→ l", "enter", "p", "esc"],
        Screen::Search if c.ready => &[MOVE, "→ l", "/", "enter", "esc"],
        Screen::PubSub if c.ready => &[MOVE, "enter", "space", "/"],
        Screen::Slots if c.ready => &[MOVE, TAB, "o", "r"],
        Screen::Clients if c.ready => &[MOVE, TAB, "enter", "r"],
        Screen::SlowLog if c.ready => &[MOVE, TAB, "p", "r"],
        Screen::Monitor if c.ready => &[TAB, "enter", "esc", MOVE],
        s if s.area().has_panels() => &[TAB, "0"],
        _ => &["0"],
    };
    order.iter().map(|keys| find_key(screen, keys)).collect()
}

/// The keys at the right of the footer: `: Command  ? Help`, or only `? Help` before
/// connecting.
pub fn footer_right(connected: bool) -> Vec<(&'static str, &'static str)> {
    let keys: &[&str] = if connected { &[":", "?"] } else { &["?"] };
    keys.iter()
        .map(|k| find_key(Screen::Connections, k))
        .collect()
}

/// Columns the footer's keys take as drawn: a space, then each key and ` {label}`,
/// three spaces between.
pub fn footer_width(hints: &[(&str, &str)]) -> usize {
    let w: usize = hints.iter().map(|(k, l)| width(k) + 1 + width(l)).sum();
    1 + w + 3 * hints.len().saturating_sub(1)
}

/// Columns [`footer_right`] takes: two spaces between its keys.
pub fn footer_right_width(hints: &[(&str, &str)]) -> usize {
    let w: usize = hints.iter().map(|(k, l)| width(k) + 1 + width(l)).sum();
    w + 2 * hints.len().saturating_sub(1)
}

/// One line of the `?` search: an action of a screen, a screen itself, or a key that
/// works everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// Where it runs: `None` for a key that works everywhere (it runs on the screen the
    /// help was opened on).
    pub screen: Option<Screen>,
    /// The key it presses there; `None` for a screen of its own (going there is all).
    pub key: Option<&'static KeyHelp>,
}

impl Hit {
    /// What it does, for the left of its line.
    pub fn text(&self) -> String {
        match (self.key, self.screen) {
            (Some(h), _) => h.help.to_string(),
            (None, Some(s)) => format!("Go to {}", s.title()),
            (None, None) => String::new(),
        }
    }

    /// Where it is, for the right of its line: `Server › Slow log`, `c on Slow log`,
    /// `everywhere`.
    pub fn place(&self) -> String {
        match (self.key, self.screen) {
            (Some(h), Some(s)) => format!("{} on {}", first_key(h), s.name()),
            (Some(h), None) => format!("{} everywhere", first_key(h)),
            (None, Some(s)) => s.title().to_string(),
            (None, None) => String::new(),
        }
    }
}

/// The first key of a row, the one `?` presses: `R` of `R`, `→` of `→ l enter`.
pub fn first_key(h: &KeyHelp) -> &'static str {
    h.keys.split_whitespace().next().unwrap_or(h.keys)
}

/// Moving keys are left out of the search: they are in every list.
fn searchable(h: &KeyHelp) -> bool {
    !matches!(first_key(h), "↑↓" | "PgUp" | "Home" | "tab")
}

/// Every screen and every action of every screen whose words hold all the words of
/// `query`, ignoring case: screens first, then actions in screen order, then the keys
/// that work everywhere. An action shared by several screens is listed on the first.
/// Nodes and Slots only on a cluster. Empty for an empty query.
pub fn find(query: &str, cluster: bool) -> Vec<Hit> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let matches = |text: &str| {
        let t = text.to_lowercase();
        words.iter().all(|w| t.contains(w.as_str()))
    };
    let shown = |s: &Screen| cluster || !matches!(s, Screen::Nodes | Screen::Slots);
    let mut out = Vec::new();
    for s in Screen::ALL.into_iter().filter(shown) {
        if matches(&format!("go to show {}", s.title())) {
            out.push(Hit {
                screen: Some(s),
                key: None,
            });
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for s in Screen::ALL.into_iter().filter(shown) {
        for h in for_screen(s).iter().filter(|h| searchable(h)) {
            if seen.contains(&h.help) || !matches(&format!("{} {}", h.help, s.title())) {
                continue;
            }
            seen.push(h.help);
            out.push(Hit {
                screen: Some(s),
                key: Some(h),
            });
        }
    }
    for h in global() {
        if matches(h.help) {
            out.push(Hit {
                screen: None,
                key: Some(h),
            });
        }
    }
    out
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
    /// the footer and the help can't drift apart.
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
    fn no_duplicate_key_within_a_screen() {
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
    fn every_screen_has_entries() {
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

    fn contexts() -> Vec<FooterCtx> {
        let mut out = Vec::new();
        for ready in [false, true] {
            for analyzed in [false, true] {
                for analyzing in [false, true] {
                    for writes in [false, true] {
                        out.push(FooterCtx {
                            ready,
                            analyzed,
                            analyzing,
                            writes,
                        });
                    }
                }
            }
        }
        out
    }

    #[test]
    fn footer_is_a_subset_of_the_table() {
        for v in Screen::ALL {
            for c in contexts() {
                for (key, label) in footer(v, c).into_iter().chain(footer_right(c.ready)) {
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
    fn footers_hold_three_to_five_keys_once_there_is_something_to_do() {
        for v in Screen::ALL {
            for c in contexts().into_iter().filter(|c| c.ready && c.analyzed) {
                let n = footer(v, c).len();
                assert!((3..=5).contains(&n), "{} has {n} footer keys", v.title());
            }
        }
    }

    #[test]
    fn footer_hints_per_screen() {
        let names = |v, c| footer(v, c).iter().map(|h| h.1).collect::<Vec<_>>();
        let connected = FooterCtx {
            ready: true,
            ..FooterCtx::default()
        };
        let analyzed = FooterCtx {
            analyzed: true,
            ..connected
        };
        assert_eq!(
            names(Screen::Connections, FooterCtx::default()),
            ["Move", "Connect", "Add", "Writes", "Filter"]
        );
        assert_eq!(
            names(Screen::Overview, connected),
            ["Analyze keys", "Move", "Go there", "More"]
        );
        assert_eq!(
            names(Screen::Overview, analyzed),
            ["Move", "Go there", "Analyze keys", "More"]
        );
        assert_eq!(
            names(
                Screen::Overview,
                FooterCtx {
                    analyzing: true,
                    ..connected
                }
            ),
            ["Stop", "Pause", "Interval"]
        );
        assert_eq!(
            names(Screen::Patterns, connected),
            ["Move", "Open", "Filter", "Analyze", "Next panel"]
        );
        assert_eq!(
            names(Screen::Patterns, analyzed),
            ["Move", "Open", "Filter", "Sort", "More"]
        );
        assert_eq!(
            names(Screen::KeyList, analyzed),
            ["Move", "Value", "Filter", "More keys", "Back"]
        );
        assert_eq!(
            names(Screen::KeyDetail, analyzed),
            ["Move", "Value", "Format", "Copy", "Back"]
        );
        assert_eq!(
            names(
                Screen::KeyDetail,
                FooterCtx {
                    writes: true,
                    ..analyzed
                }
            ),
            ["Move", "Edit", "Add", "Delete", "Format"]
        );
        assert_eq!(
            names(Screen::Ttl, analyzed),
            ["Move", "Next panel", "Forecast", "Snapshot"]
        );
        assert_eq!(names(Screen::Ttl, connected), ["Analyze", "Next panel"]);
        assert_eq!(
            names(Screen::Streams, connected),
            ["Move", "Details", "Entries", "Pause", "Back"]
        );
        assert_eq!(
            names(Screen::Search, connected),
            ["Move", "Details", "Query", "Open", "Back"]
        );
        assert_eq!(
            names(Screen::Nodes, analyzed),
            ["Move", "This node only", "Next panel"]
        );
        assert_eq!(
            names(Screen::Search, FooterCtx::default()),
            ["Next panel", "Connections"]
        );
        assert_eq!(names(Screen::PubSub, FooterCtx::default()), ["Connections"]);
        assert_eq!(footer_right(true), [(":", "Command"), ("?", "Help")]);
        assert_eq!(footer_right(false), [("?", "Help")]);
    }

    /// `tab` is in the table exactly where it does something: on the screens of an
    /// area with panels, where it switches panels and nothing else.
    #[test]
    fn tab_is_listed_on_every_panel_and_only_there() {
        for s in Screen::ALL {
            let listed = for_screen(s).iter().any(|h| h.keys == "tab shift-tab");
            assert_eq!(listed, s.area().has_panels(), "{}", s.title());
        }
    }

    #[test]
    fn footer_fits_80_columns() {
        for v in Screen::ALL {
            for c in contexts() {
                let w =
                    footer_width(&footer(v, c)) + 4 + footer_right_width(&footer_right(c.ready));
                assert!(w <= 80, "{} footer is {w} columns", v.title());
            }
        }
    }

    #[test]
    fn markdown_lists_every_screen() {
        let md = markdown();
        for v in Screen::ALL {
            assert!(md.contains(&format!("#### {}\n", v.title())));
        }
        assert!(md.contains("| `?` |"));
        // Digit 6 is no key any more.
        assert!(!md.contains("`6`") && !md.contains("0-6"));
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

    #[test]
    fn find_lists_screens_then_actions_with_their_place() {
        let hits = find("slow", false);
        let lines: Vec<(String, String)> = hits.iter().map(|h| (h.text(), h.place())).collect();
        assert_eq!(
            lines[0],
            ("Go to Server › Slow log".into(), "Server › Slow log".into())
        );
        assert!(lines.contains(&("Reset the slow log (writes)".into(), "R on Slow log".into())));
        assert!(lines.contains(&(
            "Change the slow log threshold or length (writes)".into(),
            "c on Slow log".into()
        )));
        // Every word must match; case does not matter.
        assert!(
            find("SLOW reset", false)
                .iter()
                .all(|h| h.text().contains("Reset"))
        );
        assert!(find("", false).is_empty());
        assert!(find("   ", false).is_empty());
    }

    #[test]
    fn find_leaves_out_the_cluster_panels_off_a_cluster() {
        assert!(
            find("slot", false)
                .iter()
                .all(|h| h.screen != Some(Screen::Slots))
        );
        assert!(
            find("slot", true)
                .iter()
                .any(|h| h.screen == Some(Screen::Slots))
        );
    }

    #[test]
    fn find_lists_a_shared_action_once_and_the_everywhere_keys() {
        let quit = find("quit", false);
        assert_eq!(quit.len(), 1);
        assert_eq!(quit[0].screen, None);
        assert_eq!(quit[0].place(), "q everywhere");
        let analyze = find("analyze again", false);
        assert_eq!(
            analyze
                .iter()
                .filter(|h| h.key.is_some_and(|k| k.help == ANALYZE.help))
                .count(),
            1
        );
    }
}
