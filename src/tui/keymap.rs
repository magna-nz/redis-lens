//! The single source of truth for keys: the footer, the help overlay and the key
//! table in `spec.md` are all rendered from the data here.
//!
//! This module only describes keys, one table per [`Screen`]. Handling them stays in
//! `app.rs` and, on the screens a view state owns (`app::views`), in that state; tests
//! in `app.rs` keep the table and the handlers in step. Every phase 1 key is here,
//! also those of views that are still stubs.
//!
//! Every row is tagged with its [`Verb`]: the key grammar (`docs/ui-redesign.md` §5)
//! gives each of its letters one meaning on every screen, and a test checks that no
//! screen binds one of them to a verb of another kind.

use super::app::Screen;
use super::ui::widgets::width;

/// What kind of action a key is, in the key grammar. A grammar key ([`grammar`]) means
/// one [`Verb`] on every screen; [`Verb::Screen`] is a screen's own verb, on a key the
/// grammar leaves free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Move,
    Open,
    Back,
    Area,
    Panel,
    Filter,
    Sort,
    More,
    MoreRows,
    Analyze,
    Refresh,
    Pause,
    /// `y`: the selected row's thing.
    Copy,
    /// `Y`: its parent's.
    CopyParent,
    Edit,
    Add,
    /// `t`: a TTL on the selected row.
    Ttl,
    /// `T`: a TTL on its parent.
    TtlParent,
    /// `d`: delete the selected row.
    Delete,
    /// `D`: delete its parent.
    DeleteParent,
    Rename,
    Format,
    Writes,
    Databases,
    Node,
    Help,
    Snapshot,
    FindKey,
    Command,
    Dashboard,
    /// `.`: the Actions menu.
    Actions,
    /// `ctrl-k`: the Go to box.
    GoTo,
    Quit,
    /// A screen's own verb, on a key the grammar leaves free.
    Screen,
}

/// Where an action sits in the Actions menu (`.`): with the selected row's actions,
/// with its parent's (a field's key, a key's pattern), with the screen's own (sort,
/// filter, refresh …), or not at all: moving, going somewhere and the keys that work
/// everywhere are no action of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Not in the menu.
    Nav,
    Row,
    Parent,
    Screen,
}

/// The menu group a row of `verb` goes in unless its row says otherwise
/// ([`KeyHelp::group`]): the lower-case verbs act on the selected row, the upper-case
/// ones on its parent, and `+` adds to the parent (a field to its key, a key to its
/// pattern).
const fn group_of(verb: Verb) -> Group {
    match verb {
        Verb::Copy | Verb::Edit | Verb::Ttl | Verb::Delete | Verb::Rename | Verb::Format => {
            Group::Row
        }
        Verb::CopyParent | Verb::TtlParent | Verb::DeleteParent | Verb::Add => Group::Parent,
        // A screen's `enter` acts on its selected row (connect, open, list).
        Verb::Open => Group::Row,
        Verb::Sort
        | Verb::More
        | Verb::MoreRows
        | Verb::Analyze
        | Verb::Refresh
        | Verb::Pause
        | Verb::Snapshot
        | Verb::Filter
        | Verb::Screen => Group::Screen,
        _ => Group::Nav,
    }
}

/// The letters of the key grammar and the one verb each means wherever it is bound.
pub fn grammar(key: &str) -> Option<Verb> {
    Some(match key {
        "a" => Verb::Analyze,
        "r" => Verb::Refresh,
        "space" => Verb::Pause,
        "s" => Verb::Sort,
        "m" => Verb::MoreRows,
        "v" => Verb::More,
        "/" => Verb::Filter,
        "y" => Verb::Copy,
        "Y" => Verb::CopyParent,
        "e" => Verb::Edit,
        "+" => Verb::Add,
        "t" => Verb::Ttl,
        "T" => Verb::TtlParent,
        "d" => Verb::Delete,
        "D" => Verb::DeleteParent,
        "R" => Verb::Rename,
        "f" => Verb::Format,
        "w" => Verb::Writes,
        "b" => Verb::Databases,
        "?" => Verb::Help,
        "." => Verb::Actions,
        "0" | "1" | "2" | "3" | "4" | "5" => Verb::Area,
        _ => return None,
    })
}

/// One row of the key table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHelp {
    /// What kind of action it is in the key grammar.
    pub verb: Verb,
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
    /// Its group in the Actions menu (`.`); [`Group::Nav`] keeps it out.
    pub group: Group,
}

impl KeyHelp {
    /// Whether running it can lose data: the delete keys (`d` `D`: Delete, Delete key,
    /// Delete all, End a connection, Reset the slow log, Drop an index) and Trim on a
    /// stream. The Actions menu and Go to draw these in the danger colour.
    pub fn destroys(&self) -> bool {
        matches!(self.verb, Verb::Delete | Verb::DeleteParent) || self.label == "Trim"
    }

    /// The same row in menu group `g`.
    const fn in_group(self, g: Group) -> Self {
        KeyHelp { group: g, ..self }
    }
}

const fn k(
    verb: Verb,
    keys: &'static str,
    footer_keys: &'static str,
    label: &'static str,
    help: &'static str,
    in_footer: bool,
) -> KeyHelp {
    KeyHelp {
        verb,
        keys,
        footer_keys,
        label,
        help,
        in_footer,
        group: group_of(verb),
    }
}

const CONNECTIONS_KEY: KeyHelp = k(
    Verb::Area,
    "0",
    "0",
    "Connections",
    "Connections, the home screen",
    true,
);
const AREAS: KeyHelp = k(
    Verb::Area,
    "1 2 3 4 5",
    "1-5",
    "Areas",
    "Overview, Browse, Analyze, Server, Pub/Sub; the current area's digit again: its next panel",
    false,
);
const MORE: KeyHelp = k(
    Verb::More,
    "v",
    "v",
    "More",
    "Show more columns and details, or fewer again",
    true,
);
const HELP: KeyHelp = k(
    Verb::Help,
    "?",
    "?",
    "Keys",
    "Every key here; type to search for an action",
    true,
);
const QUIT: KeyHelp = k(Verb::Quit, "q ctrl-c", "q", "Quit", "Quit", false);

/// `.` from every screen: the focused pane's actions, each with its key.
const ACTIONS_KEY: KeyHelp = k(
    Verb::Actions,
    ".",
    ".",
    "Actions",
    "Actions of the selected row, its parent and the screen, with their keys",
    true,
);

/// `ctrl-k` from every screen: the Go to box.
const GOTO_KEY: KeyHelp = k(
    Verb::GoTo,
    "ctrl-k",
    "^K",
    "Go to",
    "Go to a pattern, a loaded key, a screen or an action; reads nothing as you type",
    true,
);

const COMMAND_LINE_KEY: KeyHelp = k(
    Verb::Command,
    ":",
    ":",
    "Command",
    "Open the command line",
    true,
);

/// Not in the footer: the `?` help and Go to's Find row show it.
const FIND_KEY: KeyHelp = k(
    Verb::FindKey,
    "ctrl-f",
    "ctrl-f",
    "Find key",
    "Find keys by name or pattern on the whole server",
    false,
);

const DATABASES_KEY: KeyHelp = k(
    Verb::Databases,
    "b",
    "b",
    "Database",
    "Switch the database (once connected)",
    false,
);

const DASHBOARD_KEY: KeyHelp = k(
    Verb::Dashboard,
    "ctrl-o",
    "ctrl-o",
    "Dashboard",
    "Open the web dashboard in your browser",
    false,
);

/// `w` from every screen: on Connections for the selected profile, elsewhere for the
/// connected one. Connections' footer shows it.
const WRITES_KEY: KeyHelp = k(
    Verb::Writes,
    "w",
    "w",
    "Writes",
    "Turn writes on or off: selected or connected profile",
    true,
);

static GLOBAL: [KeyHelp; 12] = [
    CONNECTIONS_KEY,
    AREAS,
    MORE,
    HELP,
    ACTIONS_KEY,
    GOTO_KEY,
    COMMAND_LINE_KEY,
    FIND_KEY,
    DATABASES_KEY,
    WRITES_KEY,
    DASHBOARD_KEY,
    QUIT,
];

/// The keys of the command line panel while it is open (`:`). Its input line replaces
/// the footer, so none of them is in a footer. The grammar does not reach in there:
/// the command line takes every key while it is open.
static COMMAND_LINE: [KeyHelp; 9] = [
    k(
        Verb::Screen,
        "enter",
        "enter",
        "Run",
        "Run the command (writes go through the write gate)",
        false,
    ),
    k(
        Verb::Screen,
        "↑↓",
        "↑↓",
        "History",
        "Step through this session's commands",
        false,
    ),
    k(
        Verb::Screen,
        "tab",
        "tab",
        "Complete",
        "Complete a command, subcommand or loaded key name",
        false,
    ),
    k(
        Verb::Move,
        "PgUp PgDn",
        "PgUp",
        "Scroll",
        "Scroll the output",
        false,
    ),
    k(
        Verb::Screen,
        "ctrl-l",
        "ctrl-l",
        "Clear",
        "Clear the output",
        false,
    ),
    k(
        Verb::Screen,
        "ctrl-u",
        "ctrl-u",
        "Clear line",
        "Clear the input line",
        false,
    ),
    k(
        Verb::Screen,
        "ctrl-t",
        "ctrl-t",
        "View",
        "Draw replies as text, a table, JSON or raw",
        false,
    ),
    k(
        Verb::Screen,
        "ctrl-p",
        "ctrl-p",
        "Pipeline",
        "Pipeline mode: a repeat goes in pipelines",
        false,
    ),
    k(
        Verb::Back,
        "esc",
        "esc",
        "Close",
        "Close the command line, stop a repeat, or cancel a question",
        false,
    ),
];

const MOVE: KeyHelp = k(
    Verb::Move,
    "↑↓ j k",
    "↑↓",
    "Move",
    "Move the selection",
    true,
);
const PAGE: KeyHelp = k(
    Verb::Move,
    "PgUp PgDn",
    "PgUp",
    "Page",
    "Move one page",
    false,
);
const ENDS: KeyHelp = k(
    Verb::Move,
    "Home End g G",
    "Home",
    "First/last",
    "Jump to the first or last row",
    false,
);
/// `tab` on a panel: one ring through every area and panel (`App::tab_ring_key`).
const PANEL: KeyHelp = k(
    Verb::Panel,
    "tab shift-tab",
    "tab",
    "Next",
    "Next or previous panel or area, in one ring",
    true,
);
/// `tab` on an area without panels (Connections, Overview, Pub/Sub): the same ring.
/// Never in a footer.
const AREA_TAB: KeyHelp = k(
    Verb::Panel,
    "tab shift-tab",
    "tab",
    "Next",
    "Next or previous area or panel, once connected",
    false,
);
const COPY_PATTERN: KeyHelp = k(
    Verb::Copy,
    "y",
    "y",
    "Copy",
    "Copy the selected pattern to the clipboard",
    true,
);
const COPY_KEY: KeyHelp = k(
    Verb::Copy,
    "y",
    "y",
    "Copy",
    "Copy the selected key name to the clipboard",
    true,
);
const COPY_CMD: KeyHelp = k(
    Verb::CopyParent,
    "Y",
    "Y",
    "Copy cmd",
    "Copy a redis-cli --scan command for the pattern",
    false,
)
.in_group(Group::Row);
const ANALYZE: KeyHelp = k(
    Verb::Analyze,
    "a",
    "a",
    "Analyze",
    "Analyze again (Quick or Deep); old results stay",
    true,
);
const SNAPSHOT: KeyHelp = k(
    Verb::Snapshot,
    "ctrl-s",
    "ctrl-s",
    "Save",
    "Save a snapshot of the analysis",
    true,
);
const REFRESH: KeyHelp = k(
    Verb::Refresh,
    "r",
    "r",
    "Refresh",
    "Refresh this panel now",
    true,
);
const PAUSE: KeyHelp = k(
    Verb::Pause,
    "space",
    "space",
    "Pause",
    "Pause or resume polling",
    true,
);
/// `[` `]` on the screens that read the picked node of a cluster: only in `?`, so the
/// footers stay short.
const PICK_NODE: KeyHelp = k(
    Verb::Node,
    "[ ]",
    "[ ]",
    "Node",
    "Pick the previous or next node (cluster only)",
    false,
);
/// `T` on the key list and `t` on Patterns: the bulk TTL of a pattern.
const BULK_TTL_HELP: &str = "Set a TTL on every key of the pattern (writes)";
/// `D` on the key list and `d` on Patterns: the bulk delete of a pattern.
const BULK_DELETE_HELP: &str = "Delete every key of the pattern (writes)";

static CONNECTIONS: [KeyHelp; 9] = [
    MOVE,
    PAGE,
    ENDS,
    AREA_TAB,
    k(
        Verb::Open,
        "enter",
        "enter",
        "Connect",
        "Connect: reads the server's own figures, no keys",
        true,
    ),
    k(Verb::Add, "+", "+", "Add", "Add a connection", true).in_group(Group::Screen),
    k(
        Verb::Filter,
        "/",
        "/",
        "Filter",
        "Filter the profiles by name or tag",
        true,
    ),
    k(Verb::Back, "esc", "esc", "Clear", "Clear the filter", false),
    k(
        Verb::Refresh,
        "r",
        "r",
        "Docker",
        "Look for Docker containers again",
        false,
    ),
];

static OVERVIEW: [KeyHelp; 10] = [
    k(
        Verb::Analyze,
        "a",
        "a",
        "Analyze keys",
        "Analyze the keys: Quick (10,000) or Deep (100,000)",
        true,
    ),
    MOVE,
    PAGE,
    ENDS,
    AREA_TAB,
    k(
        Verb::Open,
        "enter",
        "enter",
        "Go there",
        "Go to the selected finding's panel",
        true,
    ),
    k(
        Verb::Back,
        "esc",
        "esc",
        "Stop",
        "Stop the analysis and keep what it has sampled",
        true,
    )
    .in_group(Group::Screen),
    k(
        Verb::Pause,
        "space",
        "space",
        "Pause",
        "Pause or resume the live figures",
        true,
    ),
    k(
        Verb::Screen,
        "< >",
        "<>",
        "Interval",
        "Poll the live figures less or more often, 1 to 60 s",
        true,
    )
    .in_group(Group::Nav),
    PICK_NODE,
];

static PATTERNS: [KeyHelp; 16] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    k(
        Verb::Open,
        "→ l enter",
        "→",
        "Open",
        "Open a pattern's keys, or expand a group and go in",
        true,
    ),
    k(
        Verb::Back,
        "← h",
        "←",
        "Fold",
        "Collapse a group, or go up to it",
        false,
    ),
    k(
        Verb::Filter,
        "/",
        "/",
        "Filter",
        "Filter patterns by name",
        true,
    ),
    k(
        Verb::MoreRows,
        "m",
        "m",
        "More rows",
        "Before an analysis: read 1,000 more key names",
        true,
    ),
    k(
        Verb::Back,
        "esc",
        "esc",
        "Clear",
        "Clear the filter, or show all nodes again",
        false,
    ),
    k(
        Verb::Sort,
        "s",
        "s",
        "Sort",
        "Sort by memory, keys, avg size, no-TTL % or name",
        true,
    ),
    COPY_PATTERN,
    COPY_CMD,
    ANALYZE,
    SNAPSHOT,
    k(Verb::Ttl, "t", "t", "Expire", BULK_TTL_HELP, false),
    k(Verb::Delete, "d", "d", "Delete", BULK_DELETE_HELP, false),
];

static KEY_LIST: [KeyHelp; 18] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    k(
        Verb::Open,
        "→ l enter",
        "→",
        "Value",
        "Show the selected key's value",
        true,
    ),
    k(
        Verb::Back,
        "← h esc",
        "esc",
        "Back",
        "Clear the filters first, then go back",
        true,
    ),
    k(
        Verb::Filter,
        "/",
        "/",
        "Filter",
        "Filter the loaded keys by name",
        true,
    ),
    k(Verb::Sort, "s", "s", "Sort", "Sort the loaded keys", false),
    k(
        Verb::Screen,
        "ctrl-t",
        "ctrl-t",
        "Type",
        "Show one type of the loaded keys",
        false,
    ),
    k(
        Verb::MoreRows,
        "m",
        "m",
        "More rows",
        "Load 1,000 more keys",
        true,
    ),
    COPY_KEY,
    k(
        Verb::CopyParent,
        "Y",
        "Y",
        "Copy pattern",
        "Copy the pattern of the selected key",
        false,
    ),
    k(Verb::Add, "+", "+", "New key", "Add a key (writes)", true),
    k(
        Verb::Ttl,
        "t",
        "t",
        "TTL",
        "Set or remove the selected key's TTL (writes)",
        false,
    ),
    k(
        Verb::Delete,
        "d",
        "d",
        "Delete",
        "Delete the selected key (writes)",
        false,
    ),
    k(
        Verb::Rename,
        "R",
        "R",
        "Rename",
        "Rename the selected key, never over another (writes)",
        false,
    ),
    k(
        Verb::TtlParent,
        "T",
        "T",
        "Expire all",
        BULK_TTL_HELP,
        false,
    ),
    k(
        Verb::DeleteParent,
        "D",
        "D",
        "Delete all",
        BULK_DELETE_HELP,
        false,
    ),
];

static KEY_DETAIL: [KeyHelp; 17] = [
    k(
        Verb::Move,
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
        Verb::Open,
        "→ l",
        "→",
        "Value",
        "Move into the value pane",
        true,
    )
    .in_group(Group::Nav),
    k(
        Verb::Back,
        "← h esc",
        "esc",
        "Back",
        "Back to the items, then to the key list",
        true,
    ),
    k(
        Verb::Format,
        "f",
        "f",
        "Format",
        "Show the value as text, hex, JSON, MessagePack, PHP, Pickle, Protobuf or Java",
        true,
    ),
    k(
        Verb::MoreRows,
        "m",
        "m",
        "More rows",
        "Load the next 100 items",
        false,
    ),
    k(
        Verb::Copy,
        "y",
        "y",
        "Copy",
        "Copy the selected value as shown to the clipboard",
        true,
    ),
    k(
        Verb::CopyParent,
        "Y",
        "Y",
        "Copy key",
        "Copy the key name to the clipboard",
        false,
    ),
    k(
        Verb::Edit,
        "e",
        "e",
        "Edit",
        "Edit the selected field, element or value (writes)",
        true,
    ),
    k(
        Verb::Add,
        "+",
        "+",
        "Add",
        "Add a field, element or member (writes)",
        true,
    ),
    k(
        Verb::Ttl,
        "t",
        "t",
        "Field TTL",
        "Set or remove the selected hash field's TTL (writes)",
        false,
    ),
    k(
        Verb::TtlParent,
        "T",
        "T",
        "TTL",
        "Set or remove the key's TTL (writes)",
        false,
    ),
    // On a value it renames the key the value is of: the parent's action.
    k(
        Verb::Rename,
        "R",
        "R",
        "Rename",
        "Rename the key, never over another (writes)",
        false,
    )
    .in_group(Group::Parent),
    k(
        Verb::Delete,
        "d",
        "d",
        "Delete",
        "Delete the selected field, element or member (writes)",
        true,
    ),
    k(
        Verb::DeleteParent,
        "D",
        "D",
        "Delete key",
        "Delete the key (writes)",
        true,
    ),
];

static TTL: [KeyHelp; 8] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
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

static CLIENTS: [KeyHelp; 10] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    k(
        Verb::Open,
        "enter",
        "enter",
        "List",
        "List the selected group's connections",
        true,
    ),
    k(Verb::Back, "esc", "esc", "Back", "Back to the groups", true),
    PAUSE,
    REFRESH,
    k(
        Verb::Delete,
        "d",
        "d",
        "End",
        "End the selected connection (writes)",
        true,
    ),
    PICK_NODE,
];

static SLOW_LOG: [KeyHelp; 10] = [
    MOVE,
    PAGE,
    ENDS,
    PANEL,
    PAUSE,
    REFRESH,
    COPY_LINE,
    k(
        Verb::Edit,
        "e",
        "e",
        "Threshold",
        "Change the slow log threshold or length (writes)",
        false,
    )
    .in_group(Group::Screen),
    k(
        Verb::DeleteParent,
        "D",
        "D",
        "Reset",
        "Reset the slow log (writes)",
        false,
    ),
    PICK_NODE,
];

static MONITOR: [KeyHelp; 8] = [
    k(
        Verb::Move,
        "↑↓ j k",
        "↑↓",
        "Scroll",
        "Scroll the lines",
        true,
    ),
    PAGE,
    ENDS,
    PANEL,
    k(
        Verb::Open,
        "enter",
        "enter",
        "Start",
        "Start MONITOR (asks; stops at 30 s or 10,000 lines)",
        true,
    )
    .in_group(Group::Screen),
    k(Verb::Back, "esc", "esc", "Stop", "Stop MONITOR now", true).in_group(Group::Screen),
    COPY_LINE,
    PICK_NODE,
];

/// `f` on the screens that show a value: a key's value, stream entries, a Search result.
const FORMAT: KeyHelp = k(
    Verb::Format,
    "f",
    "f",
    "Format",
    "Show the value as text, hex, JSON, MessagePack, PHP, Pickle, Protobuf or Java",
    true,
);
/// `y` on the screens that show a value.
const COPY_VALUE: KeyHelp = k(
    Verb::Copy,
    "y",
    "y",
    "Copy",
    "Copy the selected value as shown to the clipboard",
    true,
);
/// `y` on the Slow log and Monitor.
const COPY_LINE: KeyHelp = k(
    Verb::Copy,
    "y",
    "y",
    "Copy",
    "Copy the selected command line, masked",
    false,
);
/// `←` `h` on Streams and Search.
const LEFT: KeyHelp = k(
    Verb::Back,
    "← h",
    "←",
    "Left",
    "Back to the left pane, or close the entries or result",
    false,
);

static STREAMS: [KeyHelp; 17] = [
    k(
        Verb::Move,
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
        Verb::Open,
        "→ l",
        "→",
        "Details",
        "Into the right pane: the details or an entry's value",
        true,
    )
    .in_group(Group::Nav),
    LEFT,
    k(
        Verb::Open,
        "enter",
        "enter",
        "Entries",
        "List the selected stream's entries",
        true,
    ),
    k(
        Verb::MoreRows,
        "m",
        "m",
        "More rows",
        "Load 100 more entries",
        true,
    ),
    FORMAT,
    COPY_VALUE,
    k(
        Verb::Back,
        "esc",
        "esc",
        "Back",
        "Close the entries, or back to the list",
        true,
    ),
    PAUSE,
    k(
        Verb::Refresh,
        "r",
        "r",
        "Refresh",
        "Refresh this panel or the entries",
        false,
    ),
    k(
        Verb::Analyze,
        "a",
        "a",
        "Analyze",
        "Before an analysis: analyze the keys; the list of streams comes from it",
        true,
    ),
    k(
        Verb::Screen,
        "c",
        "c",
        "Claim",
        "Claim pending entries for another consumer (writes)",
        false,
    )
    .in_group(Group::Row),
    k(
        Verb::Screen,
        "A",
        "A",
        "Ack",
        "Acknowledge the selected pending entry (writes)",
        false,
    )
    .in_group(Group::Row),
    k(
        Verb::Screen,
        "-",
        "-",
        "Trim",
        "Trim the stream (writes)",
        false,
    )
    .in_group(Group::Row),
];

static PUBSUB: [KeyHelp; 14] = [
    MOVE,
    PAGE,
    ENDS,
    AREA_TAB,
    k(
        Verb::Open,
        "enter",
        "enter",
        "Subscribe",
        "Subscribe to the selected channel",
        true,
    )
    .in_group(Group::Row),
    k(
        Verb::Add,
        "+",
        "+",
        "Sub",
        "Subscribe to a channel or pattern",
        true,
    )
    .in_group(Group::Screen),
    k(
        Verb::Screen,
        "S",
        "S",
        "Shard",
        "Subscribe to a shard channel (SSUBSCRIBE)",
        false,
    ),
    k(
        Verb::Pause,
        "space",
        "space",
        "Pause",
        "Pause or resume the tail; without a tail, the counts",
        true,
    ),
    k(
        Verb::Screen,
        "ctrl-l",
        "ctrl-l",
        "Clear",
        "Clear the tail",
        false,
    ),
    k(Verb::Filter, "/", "/", "Filter", "Filter the tail", true),
    k(
        Verb::Back,
        "esc",
        "esc",
        "Stop",
        "Clear the filter, or stop the tail",
        true,
    ),
    REFRESH,
    k(
        Verb::Screen,
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
        Verb::Move,
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
        Verb::Open,
        "→ l",
        "→",
        "Details",
        "Into the right pane: the details or a result's value",
        true,
    )
    .in_group(Group::Nav),
    LEFT,
    k(
        Verb::Filter,
        "/",
        "/",
        "Query",
        "Run a query on the selected index",
        true,
    ),
    k(
        Verb::Open,
        "enter",
        "enter",
        "Open",
        "Open the selected result",
        true,
    ),
    k(
        Verb::Screen,
        "x",
        "x",
        "Explain",
        "Explain the query",
        false,
    ),
    k(
        Verb::Screen,
        "X",
        "X",
        "Profile",
        "Profile the query",
        false,
    ),
    FORMAT,
    COPY_VALUE,
    k(
        Verb::Back,
        "esc",
        "esc",
        "Back",
        "Back to the indexes and results",
        true,
    ),
    REFRESH,
    k(
        Verb::Add,
        "+",
        "+",
        "Create",
        "Create an index (writes)",
        true,
    )
    .in_group(Group::Screen),
    k(
        Verb::Delete,
        "d",
        "d",
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
        Verb::Open,
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
        Verb::Move,
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
        Verb::Sort,
        "s",
        "s",
        "Sort",
        "Sort the hottest slots by keys, CPU, net in or out",
        true,
    ),
    k(
        Verb::Refresh,
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

/// What the footer depends on besides the screen: enough that it never shows a key
/// that does nothing in the current state.
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
    /// The value on screen could not be read: a module type's summary
    /// (`Items::Module`), a type redis-lens does not read (`Items::NotRead`), or the
    /// explain or profile text of a Search query. There is nothing to format or copy
    /// there, and `d` deletes no item (`D` deletes the key).
    pub not_read: bool,
    /// The view on screen is off: its permission probe was refused, the module or
    /// command it needs is missing, or the server is too old. No key that needs it.
    pub off: bool,
    /// One level down inside the screen: a group's connections on Clients, a stream's
    /// entries, a Search result. The footer shows the way back.
    pub drill: bool,
    /// Pub/Sub: a live tail is open.
    pub tail: bool,
    /// A key's value has a list of items (`→` moves into the value pane).
    pub list: bool,
    /// A key's value: `e` edits the selected row or the value.
    pub edit: bool,
    /// A key's value: `+` adds to it.
    pub add: bool,
    /// A key's value: `d` deletes the selected item.
    pub item: bool,
    /// The list on screen has more to load (`m` reads it): a pattern's keys or a
    /// stream's entries not read to the end.
    pub more: bool,
    /// The list on screen has no row: the keys that need a selected row are left out.
    pub empty: bool,
}

/// The 3 to 5 keys that matter on `screen`, as `(key, label)` in display order, and
/// only keys that do something in state `c`. The footer puts [`footer_right`] after
/// them, against the right edge.
pub fn footer(screen: Screen, c: FooterCtx) -> Vec<(&'static str, &'static str)> {
    const MOVE: &str = "↑↓ j k";
    const TAB: &str = "tab shift-tab";
    const OPEN: &str = "→ l enter";
    const BACK: &str = "← h esc";
    let value_keys: Vec<&str>;
    let order: &[&str] = match screen {
        Screen::Connections => &[MOVE, "enter", "+", "w", "/"],
        // A view that is off: only the way to another panel, or home.
        s if c.off && s.area().has_panels() => &[TAB, "0"],
        _ if c.off => &["0"],
        Screen::Overview if c.analyzing => &["esc", "space", "< >"],
        Screen::Overview if c.analyzed => &[MOVE, "enter", "a", "v"],
        Screen::Overview if c.ready => &["a", MOVE, "enter", "v"],
        Screen::Patterns if c.analyzed => &[MOVE, OPEN, "/", "s", TAB],
        Screen::Patterns => &[MOVE, OPEN, "/", "a", TAB],
        Screen::KeyList if c.empty && c.more && c.writes => &["m", "+", BACK],
        Screen::KeyList if c.empty && c.more => &["m", BACK],
        Screen::KeyList if c.empty && c.writes => &["+", BACK],
        Screen::KeyList if c.empty => &[BACK],
        Screen::KeyList if c.more => &[MOVE, OPEN, "/", "m", BACK],
        Screen::KeyList => &[MOVE, OPEN, "/", "y", BACK],
        Screen::KeyDetail if c.not_read && c.writes => &[MOVE, "D", BACK],
        Screen::KeyDetail if c.not_read => &[MOVE, BACK],
        Screen::KeyDetail if c.writes => {
            // The write keys this value takes, then the format and copy keys while
            // there is room, and always the way back.
            let mut v = vec![MOVE];
            v.extend(
                [(c.edit, "e"), (c.add, "+"), (c.item, "d")]
                    .iter()
                    .filter_map(|(on, k)| on.then_some(*k)),
            );
            for k in ["f", "y"] {
                if v.len() < 4 {
                    v.push(k);
                }
            }
            v.push(BACK);
            value_keys = v;
            &value_keys
        }
        Screen::KeyDetail if c.list => &[MOVE, "→ l", "f", "y", BACK],
        Screen::KeyDetail => &[MOVE, "f", "y", BACK],
        Screen::BigKeys | Screen::Ttl | Screen::Diff | Screen::HotKeys if c.analyzed => {
            &[MOVE, TAB, "y", "a", "ctrl-s"]
        }
        Screen::Nodes if c.analyzed => &[MOVE, "enter", TAB],
        Screen::BigKeys | Screen::Ttl | Screen::Diff | Screen::HotKeys | Screen::Nodes
            if c.ready =>
        {
            &["a", TAB]
        }
        // Streams are found by an analysis: before one, `a` analyzes.
        Screen::Streams if c.ready && !c.analyzed => &["a", TAB],
        Screen::Streams if c.ready && c.drill && c.more => &[MOVE, "m", "f", "y", "esc"],
        Screen::Streams if c.ready && c.drill => &[MOVE, "f", "y", "esc"],
        // An analysis that found no stream: a Deep one may find some.
        Screen::Streams if c.ready && c.empty => &["a", TAB],
        Screen::Streams if c.ready => &[MOVE, "→ l", "enter", TAB],
        Screen::Search if c.ready && c.drill && c.not_read => &[MOVE, "esc", TAB],
        Screen::Search if c.ready && c.drill => &[MOVE, "f", "y", "esc", TAB],
        Screen::Search if c.ready && c.empty && c.writes => &["+", "r", TAB],
        Screen::Search if c.ready && c.empty => &["r", TAB],
        Screen::Search if c.ready => &[MOVE, "→ l", "/", "enter", TAB],
        Screen::PubSub if c.ready && c.tail => &[MOVE, "space", "/", "esc"],
        Screen::PubSub if c.ready && c.empty => &["+", "space", "r"],
        Screen::PubSub if c.ready => &[MOVE, "enter", "+", "space", "r"],
        Screen::Slots if c.ready => &[MOVE, TAB, "s", "r"],
        Screen::Clients if c.ready && c.drill && c.writes => &[MOVE, "esc", "d", "space", "r"],
        Screen::Clients if c.ready && c.drill => &[MOVE, "esc", "space", "r"],
        Screen::Clients | Screen::SlowLog if c.ready && c.empty => &[TAB, "space", "r"],
        Screen::Clients if c.ready => &[MOVE, TAB, "enter", "r"],
        Screen::SlowLog if c.ready => &[MOVE, TAB, "space", "r"],
        Screen::Monitor if c.ready => &[TAB, "enter", "esc", MOVE],
        s if s.area().has_panels() => &[TAB, "0"],
        _ => &["0"],
    };
    order.iter().map(|keys| find_key(screen, keys)).collect()
}

/// The keys at the right of the footer: `. Actions   ^K Go to   ? Keys`, before
/// connecting too. `ctrl-f` and `:` are not there: they still work, and `?` and Go to
/// list them.
pub fn footer_right() -> Vec<(&'static str, &'static str)> {
    [".", "ctrl-k", "?"]
        .iter()
        .map(|k| find_key(Screen::Connections, k))
        .collect()
}

/// A footer key as drawn: `(key, label)`.
pub type Hint = (&'static str, &'static str);

/// The footer as drawn `w` columns wide: [`footer`] and [`footer_right`], four columns
/// apart at least. When they do not all fit, `. Actions` goes first, then `^K Go to`
/// (`?` lists both), then the screen's keys from the end, so its first, most important
/// ones stay, and its way back (`esc`) last of all; `?` stays while anything fits.
/// From 100 columns on nothing is left out.
pub fn footer_fit(screen: Screen, c: FooterCtx, w: usize) -> (Vec<Hint>, Vec<Hint>) {
    let mut left = footer(screen, c);
    let mut right = footer_right();
    let fits = |l: &[(&str, &str)], r: &[(&str, &str)]| {
        let rw = footer_right_width(r);
        footer_width(l) + if rw == 0 { 0 } else { 4 + rw } <= w
    };
    for drop in [".", "^K"] {
        if !fits(&left, &right) {
            right.retain(|(k, _)| *k != drop);
        }
    }
    // The screen's keys go from the end, but its way back (`esc`) goes last of all.
    while !left.is_empty() && !fits(&left, &right) {
        match left.iter().rposition(|(k, _)| *k != "esc") {
            Some(i) => left.remove(i),
            None => left.remove(left.len() - 1),
        };
    }
    if !fits(&left, &right) {
        right.clear();
    }
    (left, right)
}

/// Columns the footer's keys take as drawn: a space, then each key and ` {label}`,
/// three spaces between.
pub fn footer_width(hints: &[(&str, &str)]) -> usize {
    let w: usize = hints.iter().map(|(k, l)| width(k) + 1 + width(l)).sum();
    1 + w + 3 * hints.len().saturating_sub(1)
}

/// Columns [`footer_right`] takes: three spaces between its keys.
pub fn footer_right_width(hints: &[(&str, &str)]) -> usize {
    let w: usize = hints.iter().map(|(k, l)| width(k) + 1 + width(l)).sum();
    w + 3 * hints.len().saturating_sub(1)
}

/// The rows of `screen`'s Actions menu (`.`) in `group`, in table order: the screen's
/// rows of that group, and in [`Group::Screen`] the keys that work everywhere and act
/// on the screen (`v`). Only rows of the table, so the menu never lists a key that
/// does not exist; each runs by pressing its first key ([`first_key`]).
pub fn menu(screen: Screen, group: Group) -> Vec<&'static KeyHelp> {
    let mut out: Vec<&'static KeyHelp> = for_screen(screen)
        .iter()
        .filter(|h| h.group == group)
        .collect();
    if group == Group::Screen && screen != Screen::Connections {
        out.extend(global().iter().filter(|h| h.group == Group::Screen));
    }
    // On Connections `w` is the selected connection's: its writes on or off.
    if group == Group::Row && screen == Screen::Connections {
        out.push(&WRITES_KEY);
    }
    out
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

    /// Where it is, for the right of its line: `Server › Slow log`, `e on Slow log`,
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

    /// Every footer context: each flag on and off.
    fn contexts() -> Vec<FooterCtx> {
        (0..1u32 << 14)
            .map(|b| {
                let on = |i: u32| b & (1 << i) != 0;
                FooterCtx {
                    ready: on(0),
                    analyzed: on(1),
                    analyzing: on(2),
                    writes: on(3),
                    not_read: on(4),
                    off: on(5),
                    drill: on(6),
                    tail: on(7),
                    list: on(8),
                    edit: on(9),
                    add: on(10),
                    item: on(11),
                    more: on(12),
                    empty: on(13),
                }
            })
            .collect()
    }

    #[test]
    fn footer_is_a_subset_of_the_table() {
        for v in Screen::ALL {
            for c in contexts() {
                for (key, label) in footer(v, c).into_iter().chain(footer_right()) {
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
            // A value that could not be read, a view that is off or an empty list has
            // little to do.
            for c in contexts()
                .into_iter()
                .filter(|c| c.ready && c.analyzed && !c.off && !c.not_read && !c.empty)
            {
                let n = footer(v, c).len();
                assert!(
                    (3..=5).contains(&n),
                    "{} has {n} footer keys: {c:?}",
                    v.title()
                );
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
        // Browse footers show how to reach Streams and Search.
        assert_eq!(
            names(Screen::Patterns, connected),
            ["Move", "Open", "Filter", "Analyze", "Next"]
        );
        assert_eq!(
            names(Screen::Patterns, analyzed),
            ["Move", "Open", "Filter", "Sort", "Next"]
        );
        assert_eq!(
            names(
                Screen::KeyList,
                FooterCtx {
                    more: true,
                    ..analyzed
                }
            ),
            ["Move", "Value", "Filter", "More rows", "Back"]
        );
        // Read to the end: `m` has nothing to read, `y` takes its place.
        assert_eq!(
            names(Screen::KeyList, analyzed),
            ["Move", "Value", "Filter", "Copy", "Back"]
        );
        // An empty list: only what does something there.
        assert_eq!(
            names(
                Screen::KeyList,
                FooterCtx {
                    empty: true,
                    ..analyzed
                }
            ),
            ["Back"]
        );
        assert_eq!(
            names(
                Screen::KeyList,
                FooterCtx {
                    empty: true,
                    more: true,
                    writes: true,
                    ..analyzed
                }
            ),
            ["More rows", "New key", "Back"]
        );
        assert_eq!(
            names(
                Screen::KeyDetail,
                FooterCtx {
                    list: true,
                    ..analyzed
                }
            ),
            ["Move", "Value", "Format", "Copy", "Back"]
        );
        // A string has no items: `→` does nothing there.
        assert_eq!(
            names(Screen::KeyDetail, analyzed),
            ["Move", "Format", "Copy", "Back"]
        );
        assert_eq!(
            names(
                Screen::KeyDetail,
                FooterCtx {
                    writes: true,
                    list: true,
                    edit: true,
                    add: true,
                    item: true,
                    ..analyzed
                }
            ),
            ["Move", "Edit", "Add", "Delete", "Back"]
        );
        assert_eq!(
            names(Screen::Ttl, analyzed),
            ["Move", "Next", "Copy", "Analyze", "Save"]
        );
        assert_eq!(names(Screen::Ttl, connected), ["Analyze", "Next"]);
        // Before an analysis there are no streams yet: `a` analyzes.
        assert_eq!(names(Screen::Streams, connected), ["Analyze", "Next"]);
        assert_eq!(
            names(Screen::Streams, analyzed),
            ["Move", "Details", "Entries", "Next"]
        );
        assert_eq!(
            names(Screen::Search, connected),
            ["Move", "Details", "Query", "Open", "Next"]
        );
        assert_eq!(
            names(Screen::Nodes, analyzed),
            ["Move", "This node only", "Next"]
        );
        assert_eq!(
            names(Screen::Search, FooterCtx::default()),
            ["Next", "Connections"]
        );
        assert_eq!(names(Screen::PubSub, FooterCtx::default()), ["Connections"]);
        assert_eq!(
            footer_right(),
            [(".", "Actions"), ("^K", "Go to"), ("?", "Keys")]
        );
    }

    /// The footer of a view that is off names no key that needs the view.
    #[test]
    fn an_off_view_shows_no_key_that_needs_it() {
        let off = FooterCtx {
            ready: true,
            analyzed: true,
            off: true,
            ..FooterCtx::default()
        };
        for v in [
            Screen::Clients,
            Screen::SlowLog,
            Screen::Streams,
            Screen::Search,
            Screen::Slots,
        ] {
            assert_eq!(
                footer(v, off).iter().map(|h| h.0).collect::<Vec<_>>(),
                ["tab", "0"],
                "{}",
                v.title()
            );
        }
        assert_eq!(footer(Screen::PubSub, off), [("0", "Connections")]);
    }

    /// A read-only connection shows no write key anywhere; with writes on, a key's
    /// value shows only the write keys its value takes, and always its way back.
    #[test]
    fn write_keys_show_only_with_writes_on_and_the_value_keeps_its_way_back() {
        let writes_keys = |v: Screen| {
            for_screen(v)
                .iter()
                .filter(|h| h.help.contains("(writes)"))
                .map(|h| h.footer_keys)
                .collect::<Vec<_>>()
        };
        for v in Screen::ALL
            .into_iter()
            .filter(|v| *v != Screen::Connections)
        {
            for c in contexts().into_iter().filter(|c| !c.writes) {
                for (key, _) in footer(v, c) {
                    assert!(
                        !writes_keys(v).contains(&key),
                        "{key} is a write key in the read-only footer of {}",
                        v.title()
                    );
                }
            }
        }
        for c in contexts()
            .into_iter()
            .filter(|c| c.ready && c.writes && !c.off)
        {
            let keys: Vec<_> = footer(Screen::KeyDetail, c).iter().map(|h| h.0).collect();
            assert!(keys.contains(&"esc"), "{keys:?} {c:?}");
            for (on, key) in [(c.edit, "e"), (c.add, "+"), (c.item, "d")] {
                assert!(
                    on || c.not_read || !keys.contains(&key),
                    "{key} in {keys:?} {c:?}"
                );
            }
        }
    }

    /// Trimmed to fit, a footer keeps its way back: wherever `footer` shows `esc`, it
    /// is still there at 80 and at 100 columns.
    #[test]
    fn footer_fit_keeps_the_way_back_at_80_and_100_columns() {
        for v in Screen::ALL {
            for c in contexts() {
                if !footer(v, c).iter().any(|h| h.0 == "esc") {
                    continue;
                }
                for w in [80, 100] {
                    let (left, _) = footer_fit(v, c, w);
                    assert!(
                        left.iter().any(|h| h.0 == "esc"),
                        "{} at {w}: {left:?} {c:?}",
                        v.title()
                    );
                }
            }
        }
    }

    /// A drill-down shows its way back, and not the key that went down.
    #[test]
    fn a_drill_down_shows_its_way_back() {
        let c = FooterCtx {
            ready: true,
            analyzed: true,
            drill: true,
            ..FooterCtx::default()
        };
        for v in [Screen::Clients, Screen::Streams, Screen::Search] {
            let f = footer(v, c);
            assert!(f.contains(&("esc", "Back")), "{}: {f:?}", v.title());
            assert!(!f.iter().any(|h| h.0 == "enter"), "{}: {f:?}", v.title());
        }
        let up = footer(Screen::Clients, FooterCtx { drill: false, ..c });
        assert!(up.contains(&("enter", "List")) && !up.contains(&("esc", "Back")));
        // With writes on, `d` ends the selected connection of the group.
        let w = footer(Screen::Clients, FooterCtx { writes: true, ..c });
        assert!(w.contains(&("d", "End")), "{w:?}");
    }

    /// Pub/Sub's `space` pauses the tail while one is open and the counts otherwise:
    /// it is in both footers; `/` filters only a tail.
    #[test]
    fn pub_sub_footer_with_and_without_a_tail() {
        let c = FooterCtx {
            ready: true,
            ..FooterCtx::default()
        };
        let none = footer(Screen::PubSub, c);
        let tail = footer(Screen::PubSub, FooterCtx { tail: true, ..c });
        assert!(none.contains(&("space", "Pause")) && tail.contains(&("space", "Pause")));
        assert!(!none.iter().any(|h| h.0 == "/"), "{none:?}");
        assert!(tail.contains(&("/", "Filter")) && tail.contains(&("esc", "Stop")));
    }

    /// Each letter of the key grammar means one kind of action on every screen: a
    /// screen may add verbs on other keys, never give one of these another meaning.
    #[test]
    fn grammar_keys_mean_one_thing_on_every_screen() {
        let rows = Screen::ALL
            .into_iter()
            .map(|s| (s.title(), for_screen(s)))
            .chain([("Everywhere", global())]);
        for (title, rows) in rows {
            for h in rows {
                for t in h.keys.split_whitespace() {
                    if let Some(want) = grammar(t) {
                        assert_eq!(
                            h.verb, want,
                            "{t:?} on {title} is bound to {:?} ({}), the grammar says {want:?}",
                            h.verb, h.help
                        );
                    }
                }
            }
        }
        // And the other way round: a row of a grammar verb is on its grammar key (or
        // keys that move, open, go back and the like, which the grammar lists too).
        for s in Screen::ALL {
            for h in for_screen(s) {
                let grammar_verb = !matches!(
                    h.verb,
                    Verb::Screen
                        | Verb::Move
                        | Verb::Open
                        | Verb::Back
                        | Verb::Panel
                        | Verb::Node
                        | Verb::Snapshot
                );
                if grammar_verb {
                    assert!(
                        h.keys
                            .split_whitespace()
                            .any(|t| grammar(t) == Some(h.verb)),
                        "{} on {} is {:?} but not on its grammar key",
                        h.keys,
                        s.title(),
                        h.verb
                    );
                }
            }
        }
        // `ctrl-s` saves a snapshot wherever one can be saved, and plain `s` never does.
        for s in Screen::ALL {
            for h in for_screen(s) {
                if h.verb == Verb::Snapshot {
                    assert_eq!(h.keys, "ctrl-s", "{}", s.title());
                }
            }
        }
    }

    /// The keys the grammar retired are bound nowhere any more (their old screens
    /// included), or only to their grammar meaning.
    #[test]
    fn retired_keys_are_gone_from_the_table() {
        let bound = |s: Screen, key: &str| {
            for_screen(s)
                .iter()
                .any(|h| h.keys.split_whitespace().any(|t| t == key))
        };
        for s in Screen::ALL {
            for key in ["o", "n", "N", "p", "K", "C"] {
                assert!(!bound(s, key), "{key} is still bound on {}", s.title());
            }
        }
        assert!(!bound(Screen::Patterns, "x"));
        assert!(!bound(Screen::Streams, "X"));
        assert!(!bound(Screen::Search, "P") && !bound(Screen::Search, "e"));
        assert!(!bound(Screen::SlowLog, "c") && !bound(Screen::SlowLog, "R"));
        assert!(!bound(Screen::PubSub, "c") && !bound(Screen::PubSub, "s"));
        assert!(!bound(Screen::Overview, "+") && !bound(Screen::Overview, "-"));
        assert!(!bound(Screen::Overview, "r") && !bound(Screen::Patterns, "r"));
        assert!(!bound(Screen::Connections, "a"));
    }

    /// `tab` is in the table on every screen, as it goes through the ring of every
    /// area and panel: the panel row on an area with panels, the area row (never in a
    /// footer) on the others.
    #[test]
    fn tab_is_listed_on_every_screen() {
        for s in Screen::ALL {
            let rows: Vec<_> = for_screen(s)
                .iter()
                .filter(|h| h.keys == "tab shift-tab")
                .collect();
            assert_eq!(rows.len(), 1, "{}", s.title());
            let want = if s.area().has_panels() {
                PANEL
            } else {
                AREA_TAB
            };
            assert_eq!(*rows[0], want, "{}", s.title());
            assert_eq!(rows[0].label, "Next", "{}", s.title());
            // No footer of an area without panels shows it.
            if !s.area().has_panels() {
                for c in contexts() {
                    assert!(
                        footer(s, c).iter().all(|(k, _)| *k != "tab"),
                        "{} {c:?}",
                        s.title()
                    );
                }
            }
        }
    }

    /// At 80 columns every footer fits as [`footer_fit`] trims it: `?` always shows,
    /// `.` gives way before `^K`, `ctrl-f` and `:` are never there, and at most one of
    /// the screen's keys is left out (the last but its way back). From 100 columns
    /// nothing is left out.
    #[test]
    fn footer_fits_80_columns() {
        for v in Screen::ALL {
            for c in contexts() {
                let all = footer(v, c);
                let (left, right) = footer_fit(v, c, 80);
                let w = footer_width(&left) + 4 + footer_right_width(&right);
                assert!(w <= 80, "{} footer is {w} columns", v.title());
                // At most one key left out, the order kept, and never the way back.
                let kept: Vec<_> = all.iter().filter(|h| left.contains(h)).copied().collect();
                assert!(
                    left.len() + 1 >= all.len() && kept == left,
                    "{}: {left:?} of {all:?}",
                    v.title()
                );
                // `?` always stays; `.` goes before `^K`; `ctrl-f` and `:` are never there.
                let keys: Vec<&str> = right.iter().map(|h| h.0).collect();
                assert!(keys.contains(&"?"), "{}: {keys:?}", v.title());
                assert!(!keys.contains(&":"), "{}: {keys:?}", v.title());
                assert!(!keys.contains(&"ctrl-f"), "{}: {keys:?}", v.title());
                if keys.contains(&".") {
                    assert!(keys.contains(&"^K"), "{}: {keys:?}", v.title());
                }
                assert_eq!(
                    footer_fit(v, c, 100),
                    (all, footer_right()),
                    "{} at 100 columns",
                    v.title()
                );
            }
        }
    }

    /// When the footer is tight, `. Actions` goes first, then `^K Go to`, then the
    /// screen's keys from the end; `?` stays.
    #[test]
    fn the_right_side_gives_way_before_the_screens_keys() {
        let c = FooterCtx {
            ready: true,
            analyzed: true,
            more: true,
            ..FooterCtx::default()
        };
        let all = footer(Screen::KeyList, c);
        let full = footer_width(&all) + 4 + footer_right_width(&footer_right());
        assert_eq!(
            footer_fit(Screen::KeyList, c, full),
            (all.clone(), footer_right())
        );
        // One column short: `.` goes, the screen's keys stay.
        let (left, right) = footer_fit(Screen::KeyList, c, full - 1);
        assert_eq!(left, all);
        assert_eq!(right, [("^K", "Go to"), ("?", "Keys")]);
        // Shorter than that without `.`: `^K` goes next.
        let no_menu =
            footer_width(&all) + 4 + footer_right_width(&[("^K", "Go to"), ("?", "Keys")]);
        let (left, right) = footer_fit(Screen::KeyList, c, no_menu - 1);
        assert_eq!(left, all);
        assert_eq!(right, [("?", "Keys")]);
        // Then the screen's keys, the way back last.
        let only_help = footer_width(&all) + 4 + footer_right_width(&[("?", "Keys")]);
        let (left, right) = footer_fit(Screen::KeyList, c, only_help - 1);
        assert_eq!(left.len(), all.len() - 1);
        assert_eq!(left.last(), Some(&("esc", "Back")));
        assert_eq!(right, [("?", "Keys")]);
        // Before connecting: the same three.
        let (_, right) = footer_fit(Screen::Connections, FooterCtx::default(), 200);
        assert_eq!(right, footer_right());
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
        assert!(lines.contains(&("Reset the slow log (writes)".into(), "D on Slow log".into())));
        assert!(lines.contains(&(
            "Change the slow log threshold or length (writes)".into(),
            "e on Slow log".into()
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

    /// A module type's summary has nothing to format or copy: the footer leaves out
    /// `f Format` and `y Copy`, and offers `D` rather than `d`, which deletes no item
    /// there. The move keys stay: a module summary scrolls.
    #[test]
    fn key_detail_footer_on_an_unread_module_type() {
        let base = FooterCtx {
            ready: true,
            analyzed: true,
            not_read: true,
            ..FooterCtx::default()
        };
        let show = |c| {
            footer(Screen::KeyDetail, c)
                .iter()
                .map(|(k, l)| format!("{k} {l}"))
                .collect::<Vec<_>>()
        };
        insta::assert_debug_snapshot!("key_detail_footer_not_read", show(base));
        insta::assert_debug_snapshot!(
            "key_detail_footer_not_read_writes",
            show(FooterCtx {
                writes: true,
                ..base
            })
        );
        for c in [
            base,
            FooterCtx {
                writes: true,
                ..base
            },
        ] {
            let names: Vec<_> = footer(Screen::KeyDetail, c).iter().map(|h| h.1).collect();
            assert!(
                !names.contains(&"Format") && !names.contains(&"Copy"),
                "{names:?}"
            );
            // Every key is in the table and marked for the footer.
            for (key, label) in footer(Screen::KeyDetail, c) {
                assert!(
                    for_screen(Screen::KeyDetail)
                        .iter()
                        .chain(global())
                        .any(|h| h.footer_keys == key && h.label == label && h.in_footer),
                    "{key} {label}"
                );
            }
        }
        // A readable value keeps them.
        let readable = FooterCtx {
            not_read: false,
            ..base
        };
        let names: Vec<_> = footer(Screen::KeyDetail, readable)
            .iter()
            .map(|h| h.1)
            .collect();
        assert!(
            names.contains(&"Format") && names.contains(&"Copy"),
            "{names:?}"
        );
    }
}
