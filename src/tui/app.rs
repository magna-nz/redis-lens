//! TUI state and the pure update functions. Nothing here does IO: key presses and
//! messages change the state and return [`Effect`]s for the runtime to carry out.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use secrecy::{ExposeSecret, SecretString};

use crate::conn::ScanTarget;
use crate::conn::docker::{DiscoveredAuth, DiscoveredInstance};
use crate::conn::parse::{ParsedConnection, mask, parse_connection_string};
use crate::conn::sentinel::SentinelAnswer;
use crate::diff::ReportDiff;
use crate::live::ReqId;
use crate::model::{
    Auth, Credentials, Endpoint, NodeReport, NodeStatus, PERM_FREQ, PERM_LABELS, PatternStats,
    PermStatus, PermissionReport, Profile, ScanReport, ServerInfo,
};
use crate::pattern::pattern_for_key;
use crate::scan::{PartialPatterns, ScanProgress};
use crate::snapshot::Trend;
use crate::verdict::ttl_verdict;

use super::clipboard::{self, CopyText};
use super::tree::{Collapsed, Group, PatternTree, Root, Row, RowId, SortBy};

/// Ticks (100 ms each) an info notice stays up.
const NOTICE_TICKS: u64 = 40;
/// How long a keychain connect may take before the TUI explains the wait.
pub const KEYCHAIN_HINT_AFTER: Duration = Duration::from_secs(2);
/// Shown while a connect is slow and the credentials come from the OS keychain.
pub const KEYCHAIN_HINT: &str =
    "Waiting for the OS keychain. If a permission dialog is open, choose Always Allow.";
const MAX_INPUT: usize = 2048;
const MAX_FILTER: usize = 64;

/// The view states that wave B fills in, one per view, and the interface `App` drives
/// them through. See the module's own documentation.
#[path = "views/mod.rs"]
pub mod views;

/// The one confirmation dialog of the write gate, and `w` on Connections.
#[path = "confirm.rs"]
pub mod confirm;

/// `x` and `D` on Patterns: bulk expire and delete, the dry run and the run.
#[path = "bulk.rs"]
pub mod bulk;

/// What the key detail's and key list's edits ask of `App`: going back after `D`, the
/// new key after `N`, `$EDITOR`.
#[path = "key_edits.rs"]
mod key_edits;

use views::{Act, Ctx, LiveState, Owner, TailCmd, ViewState, Views};

/// The seven views, switched with `0` to `6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Connections,
    Patterns,
    Analysis,
    Server,
    Streams,
    PubSub,
    Search,
}

impl View {
    pub const ALL: [View; 7] = [
        View::Connections,
        View::Patterns,
        View::Analysis,
        View::Server,
        View::Streams,
        View::PubSub,
        View::Search,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Connections => "Connections",
            Self::Patterns => "Patterns",
            Self::Analysis => "Analysis",
            Self::Server => "Server",
            Self::Streams => "Streams",
            Self::PubSub => "Pub/Sub",
            Self::Search => "Search",
        }
    }

    /// The key that switches to this view.
    pub fn digit(self) -> char {
        char::from(b'0' + self as u8)
    }

    /// The panels `tab` and `shift-tab` cycle through, in tab-bar order. Empty for a
    /// view without panels (Streams, Pub/Sub and Search pass `tab` to their view state).
    pub fn panels(self) -> &'static [Screen] {
        match self {
            Self::Analysis => &[
                Screen::Ttl,
                Screen::Diff,
                Screen::HotKeys,
                Screen::BigKeys,
                Screen::Expiry,
                Screen::Nodes,
            ],
            Self::Server => &[
                Screen::Overview,
                Screen::Clients,
                Screen::SlowLog,
                Screen::Monitor,
            ],
            _ => &[],
        }
    }
}

/// What is on screen: a view, one of its panels, or a drill-down level. Each has its
/// own key table ([`super::keymap::for_screen`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Connections,
    Patterns,
    /// The keys of one pattern, opened with `enter` on Patterns ([`views::keys`]).
    KeyList,
    /// One key's metadata and value, opened with `enter` on the key list
    /// ([`views::value`]).
    KeyDetail,
    Ttl,
    Diff,
    HotKeys,
    BigKeys,
    Expiry,
    /// The shards of a cluster scan (empty on a standalone one).
    Nodes,
    Overview,
    Clients,
    SlowLog,
    /// A time-limited `MONITOR` run ([`views::server`]).
    Monitor,
    Streams,
    PubSub,
    Search,
}

impl Screen {
    pub const ALL: [Screen; 17] = [
        Screen::Connections,
        Screen::Patterns,
        Screen::KeyList,
        Screen::KeyDetail,
        Screen::Ttl,
        Screen::Diff,
        Screen::HotKeys,
        Screen::BigKeys,
        Screen::Expiry,
        Screen::Nodes,
        Screen::Overview,
        Screen::Clients,
        Screen::SlowLog,
        Screen::Monitor,
        Screen::Streams,
        Screen::PubSub,
        Screen::Search,
    ];

    /// The view this screen belongs to.
    pub fn view(self) -> View {
        match self {
            Self::Connections => View::Connections,
            Self::Patterns | Self::KeyList | Self::KeyDetail => View::Patterns,
            Self::Ttl | Self::Diff | Self::HotKeys | Self::BigKeys | Self::Expiry | Self::Nodes => {
                View::Analysis
            }
            Self::Overview | Self::Clients | Self::SlowLog | Self::Monitor => View::Server,
            Self::Streams => View::Streams,
            Self::PubSub => View::PubSub,
            Self::Search => View::Search,
        }
    }

    /// The panel's name in the tab bar's second row.
    pub fn name(self) -> &'static str {
        match self {
            Self::KeyList => "Keys",
            Self::KeyDetail => "Key",
            Self::Ttl => "TTL",
            Self::Diff => "Diff",
            Self::HotKeys => "Hot keys",
            Self::BigKeys => "Big keys",
            Self::Expiry => "Expiry",
            Self::Nodes => "Nodes",
            Self::Overview => "Overview",
            Self::Clients => "Clients",
            Self::SlowLog => "Slow log",
            Self::Monitor => "Monitor",
            other => other.view().title(),
        }
    }

    /// The title of the help overlay and of this screen's section of the spec's key
    /// table, e.g. `Analysis · TTL`.
    pub fn title(self) -> &'static str {
        match self {
            Self::KeyList => "Patterns · key list",
            Self::KeyDetail => "Patterns · key detail",
            Self::Ttl => "Analysis · TTL",
            Self::Diff => "Analysis · Diff",
            Self::HotKeys => "Analysis · Hot keys",
            Self::BigKeys => "Analysis · Big keys",
            Self::Expiry => "Analysis · Expiry",
            Self::Nodes => "Analysis · Nodes",
            Self::Overview => "Server · Overview",
            Self::Clients => "Server · Clients",
            Self::SlowLog => "Server · Slow log",
            Self::Monitor => "Server · Monitor",
            other => other.view().title(),
        }
    }

    /// The view state that owns this screen, `None` for the screens `App` draws and
    /// handles itself (Connections, Patterns, TTL, Diff, Hot keys and Nodes).
    pub fn owner(self) -> Option<Owner> {
        Some(match self {
            Self::Connections
            | Self::Patterns
            | Self::Ttl
            | Self::Diff
            | Self::HotKeys
            | Self::Nodes => {
                return None;
            }
            Self::KeyList => Owner::Keys,
            Self::KeyDetail => Owner::Value,
            Self::BigKeys | Self::Expiry => Owner::Analysis,
            Self::Overview | Self::Clients | Self::SlowLog | Self::Monitor => Owner::Server,
            Self::Streams => Owner::Streams,
            Self::PubSub => Owner::PubSub,
            Self::Search => Owner::Search,
        })
    }

    /// Screens that show the finished scan (and `r` rescans, `s` saves a snapshot):
    /// Patterns and every Analysis panel.
    pub fn is_scan_view(self) -> bool {
        matches!(self.view(), View::Patterns | View::Analysis)
            && !matches!(self, Self::KeyList | Self::KeyDetail)
    }

    /// Screens that work on the live connection (`r` refreshes the panel).
    pub fn is_live_view(self) -> bool {
        matches!(
            self.view(),
            View::Server | View::Streams | View::PubSub | View::Search
        )
    }
}

/// Stable id of a row in the Connections list.
pub type ConnId = u64;

/// Id of one connect-and-scan run. A new one is allocated on every connect, also when
/// the same entry is connected again, so messages from a replaced run are told apart
/// from the current one even though they carry the same [`ConnId`].
pub type ScanId = u64;

#[derive(Debug, Clone)]
pub enum Source {
    Saved,
    /// Found in Docker. Credentials read from the container live in memory only.
    Docker {
        container: String,
        image: String,
        credentials: Option<Credentials>,
        /// What discovery found out about authentication, shown on the Connections view.
        auth: DiscoveredAuth,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    NotOpen,
    Connecting,
    Connected,
    /// Connected, but at least one permission check was denied.
    Limited,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ConnEntry {
    pub id: ConnId,
    pub profile: Profile,
    pub source: Source,
    pub status: ConnStatus,
    pub server: Option<ServerInfo>,
    pub perms: Option<PermissionReport>,
    /// Which server the last connect reached.
    pub target: Option<ScanTarget>,
    /// For a Sentinel profile: what the Sentinels answered at the last connect. Holds no
    /// credential.
    pub sentinel: Option<SentinelAnswer>,
    /// The last connect error. Never holds credentials.
    pub error: Option<String>,
    /// Added in this session through the add flow.
    pub just_saved: bool,
}

impl ConnEntry {
    /// The address shown for this connection: for a Sentinel profile the primary the
    /// Sentinels named at the last connect, else (and until then) the profile's endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        self.sentinel
            .as_ref()
            .map_or(&self.profile.endpoint, |a| &a.primary)
    }

    pub fn is_docker(&self) -> bool {
        matches!(self.source, Source::Docker { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerState {
    Searching,
    Found,
    /// Docker isn't running or can't be reached. Never fatal.
    Unavailable,
}

/// The selected row on the Connections view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnSel {
    Entry(ConnId),
    Add,
}

/// Where a connection's credentials come from.
#[derive(Debug, Clone)]
pub enum SecretSource {
    /// A saved profile: the OS keychain.
    Keychain,
    /// A Docker container: whatever was read from it, held in memory.
    Container(Option<Credentials>),
}

/// Work for the runtime. Effects are the only way the app reaches the outside world.
#[derive(Debug)]
pub enum Effect {
    /// Connect, check permissions, then scan with the profile's default options.
    Connect {
        id: ConnId,
        scan: ScanId,
        profile: Profile,
        secrets: SecretSource,
    },
    DiscoverDocker,
    /// Credentials to the keychain first, then the profile to the config file.
    SaveConnection {
        profile: Profile,
        credentials: Option<Credentials>,
    },
    /// Find the newest snapshot taken before `report` and diff it against `report`
    /// ([`Msg::History`]), then load the trend over the snapshots before it ([`Msg::Trend`]).
    LoadHistory {
        id: ConnId,
        scan: ScanId,
        report: Box<ScanReport>,
    },
    /// Write `report` to the snapshot directory.
    SaveSnapshot(Box<ScanReport>),
    /// Put the text on the clipboard with the terminal's OSC 52 sequence. Only a
    /// pattern, a key name or the suggested command can be built (see [`CopyText`]).
    Copy(CopyText),
    /// Send `req` on the live connection of session `id` ([`crate::live`]). The answer
    /// comes back as [`Msg::Live`].
    Live {
        id: ConnId,
        req_id: crate::live::ReqId,
        req: crate::live::LiveRequest,
    },
    /// Start the Pub/Sub live tail `tail` of session `id`'s run `scan` on `to`, on a
    /// connection of its own to `profile`'s endpoint (`conn::open_subscriber`), taking
    /// from that run's command budget. It replaces the session's other tail, if any.
    /// Its messages come back as [`Msg::TailReady`], [`Msg::Tail`] and
    /// [`Msg::TailClosed`]. A new connect or rescan of the session, and quitting, end it
    /// too.
    Subscribe {
        id: ConnId,
        scan: ScanId,
        tail: u64,
        profile: Profile,
        secrets: SecretSource,
        to: crate::live::pubsub::Subscription,
    },
    /// End session `id`'s live tail `tail` and drop its connection.
    Unsubscribe {
        id: ConnId,
        tail: u64,
    },
    /// Start the Monitor run `run` of session `id`'s run `scan`: `MONITOR` on a
    /// connection of its own to `profile`'s endpoint (`conn::open_monitor`), taking from
    /// that run's command budget, replacing the session's other Monitor run if any. Its
    /// messages come back as [`Msg::MonitorReady`], [`Msg::MonitorLines`] and
    /// [`Msg::MonitorClosed`]. It ends by itself after `crate::live::monitor::Limits`;
    /// [`Effect::StopMonitor`], a new connect or rescan of the session, and quitting end
    /// it too.
    Monitor {
        id: ConnId,
        scan: ScanId,
        run: u64,
        profile: Profile,
        secrets: SecretSource,
    },
    /// End session `id`'s Monitor run `run` and drop its connection.
    StopMonitor {
        id: ConnId,
        run: u64,
    },
    /// Turn writes on or off for the saved profile `name`: `read_only` in the config
    /// file, and nothing else ([`crate::config::Config::set_writes`]). Answered with
    /// [`Msg::WritesSaved`] or [`Msg::WritesFailed`].
    SetWrites {
        name: String,
        on: bool,
    },
    /// Edit a value in `$EDITOR` on a temporary file ([`crate::tui::editor`]), out of
    /// the TUI until it exits. Answered with [`Msg::Edited`].
    Editor(crate::tui::editor::EditorJob),
    Quit,
}

/// Results coming back from the runtime.
#[derive(Debug)]
pub enum Msg {
    /// [`Effect::Editor`] ended: the edited value, or why not. Values: on screen only.
    Edited {
        job: u64,
        result: Result<crate::value::Raw, String>,
    },
    /// [`Effect::SetWrites`] saved the profile.
    WritesSaved {
        name: String,
        on: bool,
    },
    /// [`Effect::SetWrites`] could not save it: why.
    WritesFailed {
        name: String,
        on: bool,
        error: String,
    },
    Tick,
    /// A tick that also carries the time, so the app can tell how long a connect has taken.
    TickAt(Instant),
    DockerFound(Vec<DiscoveredInstance>),
    DockerFailed(String),
    Connected {
        id: ConnId,
        scan: ScanId,
        server: Option<ServerInfo>,
        perms: PermissionReport,
        target: ScanTarget,
    },
    /// A Sentinel profile's connect asked the Sentinels: which one named the primary,
    /// and where. Sent before [`Msg::Connected`]. Holds no credential.
    SentinelResolved {
        id: ConnId,
        scan: ScanId,
        answer: SentinelAnswer,
    },
    ConnectFailed {
        id: ConnId,
        scan: ScanId,
        error: String,
    },
    ScanProgress {
        id: ConnId,
        scan: ScanId,
        progress: ScanProgress,
    },
    /// Pattern stats from the keys sampled so far. Shown on the Patterns view until the
    /// scan finishes or fails; never saved.
    ScanPartial {
        id: ConnId,
        scan: ScanId,
        partial: Arc<PartialPatterns>,
    },
    ScanDone {
        id: ConnId,
        scan: ScanId,
        report: Box<ScanReport>,
    },
    ScanFailed {
        id: ConnId,
        scan: ScanId,
        error: String,
    },
    Saved(Profile),
    SaveFailed(String),
    /// The diff against the previous snapshot of the scan taken at `taken_at_unix`:
    /// `Ok(None)` when there is no earlier snapshot.
    History {
        id: ConnId,
        scan: ScanId,
        taken_at_unix: i64,
        result: Result<Option<ReportDiff>, String>,
    },
    /// The trend over the [`TREND_SNAPSHOTS`] newest snapshots taken before the scan at
    /// `taken_at_unix` with its `MATCH` glob, with that scan as the last point. `None`
    /// when they could not be read. Accepted exactly when the same scan's
    /// [`Msg::History`] would be.
    Trend {
        id: ConnId,
        scan: ScanId,
        taken_at_unix: i64,
        trend: Option<Trend>,
    },
    SnapshotSaved(PathBuf),
    SnapshotFailed(String),
    /// Writing the OSC 52 sequence to the terminal failed.
    CopyFailed(String),
    /// The live connection of run `scan` is open; [`Effect::Live`] can be sent.
    LiveReady {
        id: ConnId,
        scan: ScanId,
    },
    /// The answer to the [`Effect::Live`] with `req_id`.
    Live {
        id: ConnId,
        scan: ScanId,
        req_id: crate::live::ReqId,
        result: Result<crate::live::LiveReply, crate::live::LiveError>,
    },
    /// The live connection of run `scan` has closed. `error` says why, in redis-lens's
    /// own words, when it was not closed on purpose.
    LiveClosed {
        id: ConnId,
        scan: ScanId,
        error: Option<String>,
    },
    /// The live tail `tail` of run `scan` is subscribed.
    TailReady {
        id: ConnId,
        scan: ScanId,
        tail: u64,
    },
    /// Messages of the live tail `tail`, oldest first, at most
    /// [`crate::live::pubsub::TAIL_KEPT`]; `dropped` more arrived since the last batch
    /// and were left out (the oldest of them).
    Tail {
        id: ConnId,
        scan: ScanId,
        tail: u64,
        batch: Vec<crate::live::pubsub::TailMessage>,
        dropped: u64,
    },
    /// The live tail `tail` has ended without being stopped: `error` says why, in
    /// redis-lens's own words.
    TailClosed {
        id: ConnId,
        scan: ScanId,
        tail: u64,
        error: Option<String>,
    },
    /// The Monitor run `run` of run `scan` has started, at `at_unix_ms` (UTC).
    MonitorReady {
        id: ConnId,
        scan: ScanId,
        run: u64,
        at_unix_ms: i64,
    },
    /// Lines of the Monitor run `run`, oldest first. Values: on screen only.
    MonitorLines {
        id: ConnId,
        scan: ScanId,
        run: u64,
        lines: Vec<crate::live::monitor::Entry>,
    },
    /// The Monitor run `run` has ended without being stopped: how, or why it could not
    /// start, in redis-lens's own words.
    MonitorClosed {
        id: ConnId,
        scan: ScanId,
        run: u64,
        end: Result<crate::live::monitor::End, String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Connecting,
    Scanning,
    Done,
    Failed(String),
}

/// The comparison of the current scan with the previous snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum History {
    /// No finished scan yet, or the snapshot lookup is still running.
    Pending,
    /// No snapshot of this profile is older than the current scan.
    NoSnapshot,
    /// The snapshot directory could not be read.
    Failed(String),
    Diff(ReportDiff),
}

/// The connection that the report views show.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: ConnId,
    /// The connect-and-scan run this session shows. Messages from any other run are
    /// ignored.
    pub scan: ScanId,
    pub profile: Profile,
    pub server: Option<ServerInfo>,
    /// Which server is scanned, known once connected.
    pub target: Option<ScanTarget>,
    pub progress: Option<ScanProgress>,
    pub phase: Phase,
    /// The latest partial pattern stats while the scan runs. Cleared when the scan
    /// finishes (the report replaces it) or fails, so it is only `Some` in
    /// [`Phase::Scanning`] with no report.
    pub partial: Option<Arc<PartialPatterns>>,
    pub report: Option<ScanReport>,
    pub history: History,
    /// The trend behind the Diff view's trend column, loaded with `history` and cleared
    /// with it.
    pub trend: Option<Trend>,
    /// `Some(run)` while `report` and `history` are those of `run`, an earlier run of
    /// this entry, kept on screen during a rescan until the new run's first partial
    /// results (or its report) replace them. They stay, still marked, when the rescan
    /// fails before that.
    pub previous: Option<ScanId>,
    /// This run was started with `r`: the list selections carry over to its results.
    pub rescan: bool,
    /// This run's live connection, for the views that poll or look things up.
    pub live: LiveState,
    /// The write commands the server refused for this user, from `ACL DRYRUN` on
    /// connect and every `NOPERM` since: those actions are off (spec "Write
    /// permissions", check 1).
    pub refusals: crate::write::Refusals,
    /// For a Sentinel profile: which Sentinel named the primary of this run, and where.
    pub sentinel: Option<SentinelAnswer>,
}

impl Session {
    /// The address this session's server is at: the primary the Sentinels named, or the
    /// profile's endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        self.sentinel
            .as_ref()
            .map_or(&self.profile.endpoint, |a| &a.primary)
    }

    /// The diff against the previous snapshot, when there is one.
    pub fn diff(&self) -> Option<&ReportDiff> {
        match &self.history {
            History::Diff(d) => Some(d),
            _ => None,
        }
    }

    /// The trend for the Diff view's trend column: only with at least
    /// [`TREND_MIN_POINTS`] points, since two are just the diff itself.
    pub fn trend(&self) -> Option<&Trend> {
        self.trend
            .as_ref()
            .filter(|t| t.points.len() >= TREND_MIN_POINTS)
    }
}

/// How many earlier snapshots the Diff view's trend column loads, besides the scan.
pub const TREND_SNAPSHOTS: usize = 7;
/// The fewest points (the scan included) the Diff view shows a trend for.
pub const TREND_MIN_POINTS: usize = 3;

/// Which node of a cluster report the Patterns view is narrowed to: its node ID and
/// the address it was scanned at, both from [`NodeReport`]. Found again by both in each
/// new report, so a rescan's report narrows to the same node, or not at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeKey {
    pub id: String,
    pub endpoint: Endpoint,
}

impl NodeKey {
    fn of(n: &NodeReport) -> Self {
        Self {
            id: n.id.clone(),
            endpoint: n.endpoint.clone(),
        }
    }

    /// The index of this node in `report`'s nodes.
    fn find(&self, report: &ScanReport) -> Option<usize> {
        report
            .nodes
            .as_ref()?
            .iter()
            .position(|n| n.id == self.id && n.endpoint == self.endpoint)
    }
}

/// The patterns the Patterns view shows.
#[derive(Debug, Clone, Copy)]
pub struct PatternData<'a> {
    pub patterns: &'a [PatternStats],
    pub sizes_available: bool,
    /// `Some` while these are partial results of a running scan.
    pub partial: Option<&'a PartialPatterns>,
}

/// The add-connection input. Holds what the user typed, which may include a
/// password, so it has no `Debug` and is only ever displayed through `mask`.
#[derive(Default)]
pub struct AddState {
    pub input: String,
    /// `Some` once the URL turned out to need a password and the user is typing it.
    pub password: Option<String>,
    /// A parse or save error, already masked.
    pub error: Option<String>,
    pub saving: bool,
}

impl AddState {
    pub fn parsed(&self) -> Option<Result<ParsedConnection, String>> {
        if self.input.trim().is_empty() {
            return None;
        }
        Some(parse_connection_string(&self.input).map_err(|e| mask(&e.to_string())))
    }
}

#[derive(Debug, Clone, Default)]
pub struct PatternsState {
    pub tree: PatternTree,
    /// Collapsed groups. Groups start expanded. See [`Collapsed`] for how a collapse
    /// carries over when the tree is rebuilt.
    pub collapsed: Collapsed,
    /// The row order, changed with `o`. Kept while the session's scan runs and
    /// finishes; a new connect starts again by memory.
    pub sort: SortBy,
    /// Index into the visible rows. Follows the selected row's identity (see
    /// [`App::selected_row_id`]) whenever the tree is rebuilt.
    pub sel: usize,
    pub filter: String,
    /// The filter input is open.
    pub filtering: bool,
    /// The key list of a pattern is open over the tree (`enter` on a pattern, `esc`
    /// back). Its state is in [`views::keys`].
    pub keys_open: bool,
    /// The key detail of a listed key is open over the key list (`enter` on a key,
    /// `esc` back to the list). Only with `keys_open`. Its state is in
    /// [`views::value`].
    pub detail_open: bool,
    /// `Some` while the view shows one cluster node's patterns (`enter` on 5 Nodes)
    /// instead of the merged ones; `esc` goes back. Cleared when the node is not in the
    /// report shown, or partial results (always merged) replace that report.
    pub node: Option<NodeKey>,
}

/// The selected row of the TTL, Diff, Hot keys and Nodes views. Each view keeps its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewSel {
    pub ttl: usize,
    pub diff: usize,
    pub hot: usize,
    pub nodes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    /// Tick at which an info notice disappears. Errors stay until a key press.
    pub until: Option<u64>,
}

impl Notice {
    /// An info notice shown at tick `tick`, gone a few seconds later.
    pub fn info(text: impl Into<String>, tick: u64) -> Self {
        Self {
            text: text.into(),
            error: false,
            until: Some(tick + NOTICE_TICKS),
        }
    }

    /// An error notice, shown until the next key.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: true,
            until: None,
        }
    }
}

/// The checks every copy goes through, at tick `tick`: the notice to show, and the copy
/// itself when the text may be copied. A pasted newline would run whatever follows it
/// in a shell, so text with control characters is refused, and so is text whose OSC 52
/// sequence is over the cap.
fn copy_checked(text: CopyText, tick: u64) -> (Notice, Option<Effect>) {
    if text.as_str().chars().any(char::is_control) {
        let n = format!(
            "This {} has control characters, so it is not copied.",
            text.what()
        );
        return (Notice::info(n, tick), None);
    }
    if clipboard::osc52(&text).is_none() {
        let n = format!("This {} is too long to copy.", text.what());
        return (Notice::info(n, tick), None);
    }
    let n = format!(
        "Copied {} to the clipboard (needs a terminal with OSC 52)",
        text.what()
    );
    (Notice::info(n, tick), Some(Effect::Copy(text)))
}

pub struct App {
    pub view: View,
    /// The shown panel of Analysis and of Server, as an index into [`View::panels`].
    /// Each view keeps its own while you are elsewhere.
    pub analysis_panel: usize,
    pub server_panel: usize,
    /// Saved profiles first, then Docker containers.
    pub entries: Vec<ConnEntry>,
    pub docker: DockerState,
    pub conn_sel: ConnSel,
    pub add: Option<AddState>,
    pub session: Option<Session>,
    pub patterns: PatternsState,
    pub sel: ViewSel,
    /// The view states of the screens `App` does not draw itself.
    pub views: Views,
    pub notice: Option<Notice>,
    /// The `?` key overlay is open.
    pub help: bool,
    pub tick: u64,
    /// List rows visible on screen, set by the runtime from the terminal size (see
    /// [`App::set_list_rows`]). PgUp and PgDn move this far.
    list_rows: usize,
    /// The value of `tick` when the selected row last changed. The marquee that scrolls
    /// a long selected name counts from here (see [`App::sel_ticks`]).
    sel_since: u64,
    /// What the selection was at the last check, to spot a change.
    sel_id: SelId,
    /// The latest time given by `Msg::TickAt`. The app never reads the clock itself.
    now: Option<Instant>,
    /// When the current connect started, by the clock above.
    connect_started: Option<Instant>,
    next_id: ConnId,
    next_scan: ScanId,
    /// Live requests sent and not answered yet, with the view that sent each. Cleared
    /// when the live connection closes or a new run replaces it.
    live_pending: HashMap<ReqId, Owner>,
    next_req: u64,
    /// The id of the next Pub/Sub live tail ([`views::Act::subscribe`]).
    next_tail: u64,
    /// `[settings]` from the config file.
    pub settings: crate::write::Settings,
    /// The write gate's question on screen, if any. While it is open every key goes to
    /// it.
    pub confirm: Option<confirm::Confirm>,
    /// The bulk action's dialog (`x` or `D` on Patterns), if open. While it is open
    /// every key goes to it, after the confirm dialog.
    pub bulk: Option<bulk::Bulk>,
    /// Writes sent and not answered yet, with their command names: a `NOPERM` answer
    /// adds the name to the session's refusals.
    live_writes: HashMap<ReqId, String>,
    /// The view-state methods called, in order, so tests see a stub view's calls
    /// arrive: `(owner, "show" | "hide" | "key" | "tick" | "reply" | "refresh" |
    /// "paste" | "open")`.
    #[cfg(test)]
    calls: Vec<(Owner, &'static str)>,
}

/// Everything that decides which row is selected, to spot a change. The Patterns row is
/// by identity, so the same pattern at another index is no change and another pattern
/// at the same index is one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelId {
    screen: Screen,
    conn: ConnSel,
    /// Only tracked on the Patterns view.
    patterns: Option<RowId>,
    sel: ViewSel,
    session: Option<(ConnId, ScanId)>,
}

impl Default for SelId {
    fn default() -> Self {
        Self {
            screen: Screen::Connections,
            conn: ConnSel::Add,
            patterns: None,
            sel: ViewSel::default(),
            session: None,
        }
    }
}

/// What the Patterns view shows for `s`, narrowed to `node` when it is set and in the
/// report: see [`App::pattern_data`].
fn pattern_data_of<'a>(s: &'a Session, node: Option<&NodeKey>) -> Option<PatternData<'a>> {
    if let Some(r) = &s.report {
        if let Some(n) = node
            .and_then(|k| k.find(r))
            .and_then(|i| r.nodes.as_ref()?.get(i))
        {
            return Some(PatternData {
                patterns: &n.patterns,
                sizes_available: n.sizes_available,
                partial: None,
            });
        }
        return Some(PatternData {
            patterns: &r.patterns,
            sizes_available: r.sizes_available,
            partial: None,
        });
    }
    let p = s.partial.as_deref()?;
    Some(PatternData {
        patterns: &p.patterns,
        sizes_available: p.sizes_available,
        partial: Some(p),
    })
}

fn is_press(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

/// `y` and `Y` copy on Patterns, TTL, Diff and Hot keys.
fn copies(screen: Screen) -> bool {
    matches!(
        screen,
        Screen::Patterns | Screen::Ttl | Screen::Diff | Screen::HotKeys
    )
}

/// Where the credentials of `entry` come from, for a connect or a live tail.
fn secret_source(entry: &ConnEntry) -> SecretSource {
    match &entry.source {
        Source::Saved => SecretSource::Keychain,
        Source::Docker { credentials, .. } => SecretSource::Container(credentials.clone()),
    }
}

fn plain_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

/// Where a move key takes the selection `cur` of a list of `len` rows: ↑↓ j k one
/// row, PgUp PgDn `page` rows, Home End g G the ends, all clamped to the list. `None`
/// for any other key.
fn list_move(key: &KeyEvent, cur: usize, len: usize, page: usize) -> Option<usize> {
    let last = len.saturating_sub(1);
    Some(match (key.code, plain_char(key)) {
        (KeyCode::Up, _) | (_, Some('k')) => cur.saturating_sub(1),
        (KeyCode::Down, _) | (_, Some('j')) => cur.saturating_add(1).min(last),
        (KeyCode::PageUp, _) => cur.saturating_sub(page),
        (KeyCode::PageDown, _) => cur.saturating_add(page).min(last),
        (KeyCode::Home, _) | (_, Some('g')) => 0,
        (KeyCode::End, _) | (_, Some('G')) => last,
        _ => return None,
    })
}

fn clean_paste(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\r' | '\n')).collect()
}

impl App {
    pub fn new(profiles: Vec<Profile>) -> Self {
        let mut app = Self {
            view: View::Connections,
            analysis_panel: 0,
            server_panel: 0,
            entries: Vec::new(),
            docker: DockerState::Searching,
            conn_sel: ConnSel::Add,
            add: None,
            session: None,
            patterns: PatternsState::default(),
            sel: ViewSel::default(),
            views: Views::default(),
            notice: None,
            help: false,
            tick: 0,
            list_rows: 0,
            sel_since: 0,
            sel_id: SelId::default(),
            now: None,
            connect_started: None,
            next_id: 1,
            next_scan: 1,
            live_pending: HashMap::new(),
            next_req: 1,
            next_tail: 1,
            settings: crate::write::Settings::default(),
            confirm: None,
            bulk: None,
            live_writes: HashMap::new(),
            #[cfg(test)]
            calls: Vec::new(),
        };
        for p in profiles {
            let id = app.alloc_id();
            app.entries.push(ConnEntry {
                id,
                profile: p,
                source: Source::Saved,
                status: ConnStatus::NotOpen,
                server: None,
                perms: None,
                target: None,
                sentinel: None,
                error: None,
                just_saved: false,
            });
        }
        if let Some(first) = app.entries.first() {
            app.conn_sel = ConnSel::Entry(first.id);
        }
        app
    }

    /// Effects to run once at startup.
    pub fn init(&mut self) -> Vec<Effect> {
        self.docker = DockerState::Searching;
        vec![Effect::DiscoverDocker]
    }

    fn alloc_id(&mut self) -> ConnId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn is_connecting(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.phase == Phase::Connecting)
    }

    /// True when the session's connect has run longer than [`KEYCHAIN_HINT_AFTER`] and its
    /// credentials come from the OS keychain, so the wait is probably a permission dialog.
    pub fn waiting_for_keychain(&self) -> bool {
        let (Some(s), Some(started), Some(now)) =
            (self.session.as_ref(), self.connect_started, self.now)
        else {
            return false;
        };
        let from_keychain = s.profile.auth == Auth::Keychain
            && self
                .entry(s.id)
                .is_some_and(|e| matches!(e.source, Source::Saved));
        from_keychain
            && s.phase == Phase::Connecting
            && now.saturating_duration_since(started) > KEYCHAIN_HINT_AFTER
    }

    /// The hint to show beside a slow connect, if any.
    pub fn keychain_hint(&self) -> Option<&'static str> {
        self.waiting_for_keychain().then_some(KEYCHAIN_HINT)
    }

    pub fn set_error(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice::error(text));
    }

    pub fn set_info(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice::info(text, self.tick));
    }

    pub fn entry(&self, id: ConnId) -> Option<&ConnEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    fn entry_mut(&mut self, id: ConnId) -> Option<&mut ConnEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    pub fn saved(&self) -> impl Iterator<Item = &ConnEntry> {
        self.entries.iter().filter(|e| !e.is_docker())
    }

    pub fn docker_entries(&self) -> impl Iterator<Item = &ConnEntry> {
        self.entries.iter().filter(|e| e.is_docker())
    }

    /// Selectable rows on the Connections view, in display order.
    pub fn selectable(&self) -> Vec<ConnSel> {
        let mut out: Vec<ConnSel> = self.saved().map(|e| ConnSel::Entry(e.id)).collect();
        out.extend(self.docker_entries().map(|e| ConnSel::Entry(e.id)));
        out.push(ConnSel::Add);
        out
    }

    pub fn selected_entry(&self) -> Option<&ConnEntry> {
        match self.conn_sel {
            ConnSel::Entry(id) => self.entry(id),
            ConnSel::Add => None,
        }
    }

    /// What the Patterns view shows: the finished report's patterns (one node's, when
    /// narrowed to it), or the partial results while the scan runs, or `None`.
    /// `self.patterns.tree` is always built from these.
    pub fn pattern_data(&self) -> Option<PatternData<'_>> {
        pattern_data_of(self.session.as_ref()?, self.patterns.node.as_ref())
    }

    /// The node the Patterns view is narrowed to, with its index in the report's nodes:
    /// `None` when it shows the merged patterns.
    pub fn narrowed_node(&self) -> Option<(usize, &NodeReport)> {
        let r = self.report()?;
        let i = self.patterns.node.as_ref()?.find(r)?;
        Some((i, r.nodes.as_ref()?.get(i)?))
    }

    /// The nodes of a cluster report; `None` on a standalone report or before one.
    pub fn nodes(&self) -> Option<&[NodeReport]> {
        self.report()?.nodes.as_deref()
    }

    /// Rows currently visible on the Patterns view.
    pub fn pattern_rows(&self) -> Vec<Row> {
        match self.pattern_data() {
            Some(d) => self.patterns.tree.visible(
                d.patterns,
                &self.patterns.collapsed,
                &self.patterns.filter,
            ),
            None => Vec::new(),
        }
    }

    /// The selected Patterns row by identity: a group by its key, a pattern by its
    /// name. `None` when there are no rows.
    pub fn selected_row_id(&self) -> Option<RowId> {
        let d = self.pattern_data()?;
        self.patterns.tree.row_id(d.patterns, self.selected_row()?)
    }

    /// The selected row of the Patterns view, `None` when there are no rows.
    fn selected_row(&self) -> Option<Row> {
        let rows = self.pattern_rows();
        rows.get(self.patterns.sel.min(rows.len().saturating_sub(1)))
            .copied()
    }

    /// The selected row's identity and the patterns it stands for, to find it again
    /// after the tree is rebuilt.
    fn selection_anchor(&self) -> Option<(RowId, Vec<String>)> {
        let d = self.pattern_data()?;
        let row = self.selected_row()?;
        let tree = &self.patterns.tree;
        Some((
            tree.row_id(d.patterns, row)?,
            tree.row_members(d.patterns, row),
        ))
    }

    /// Rebuilds the Patterns tree from the current pattern data and sort, carries the
    /// collapsed groups over, and moves the selection to the row that was `anchor`
    /// (see [`PatternTree::find`]). When that row is gone the index stays, clamped to
    /// the rows.
    fn rebuild_patterns(&mut self, anchor: Option<(RowId, Vec<String>)>) {
        let node = self.patterns.node.clone();
        let Some(d) = self
            .session
            .as_ref()
            .and_then(|s| pattern_data_of(s, node.as_ref()))
        else {
            self.patterns.tree = PatternTree::default();
            self.patterns.sel = 0;
            return;
        };
        let p = &mut self.patterns;
        p.tree = PatternTree::build_sorted(d.patterns, d.sizes_available, p.sort);
        p.collapsed.reconcile(&p.tree, d.patterns);
        let rows = p.tree.visible(d.patterns, &p.collapsed, &p.filter);
        p.sel = anchor
            .and_then(|(id, members)| p.tree.find(d.patterns, &rows, &id, &members))
            .unwrap_or_else(|| p.sel.min(rows.len().saturating_sub(1)));
    }

    pub fn report(&self) -> Option<&ScanReport> {
        self.session.as_ref().and_then(|s| s.report.as_ref())
    }

    pub fn diff(&self) -> Option<&ReportDiff> {
        self.session.as_ref().and_then(|s| s.diff())
    }

    /// The trend for the Diff view, when there are enough snapshots for one.
    pub fn trend(&self) -> Option<&Trend> {
        self.session.as_ref().and_then(|s| s.trend())
    }

    /// Rows of the TTL view: indices into the report's patterns, most worrying verdict
    /// first, then by estimated memory (keys without sizes), then by name.
    pub fn ttl_rows(&self) -> Vec<usize> {
        let Some(r) = self.report() else {
            return Vec::new();
        };
        let diff = self.diff();
        let mut rows: Vec<usize> = (0..r.patterns.len()).collect();
        rows.sort_by_cached_key(|&i| {
            let p = &r.patterns[i];
            let verdict = ttl_verdict(p, diff.and_then(|d| d.pattern(&p.pattern)));
            let weight = if r.sizes_available {
                p.est_bytes.unwrap_or(0)
            } else {
                p.est_keys
            };
            (verdict, std::cmp::Reverse(weight), p.pattern.clone())
        });
        rows
    }

    /// Why the Hot keys view is empty, or `None` when it has keys (or no report yet).
    pub fn hot_keys_reason(&self) -> Option<String> {
        let s = self.session.as_ref()?;
        let r = s.report.as_ref()?;
        if !r.hot_keys.is_empty() {
            return None;
        }
        let denied = self
            .entry(s.id)
            .and_then(|e| e.perms.as_ref())
            .is_some_and(|p| {
                p.checks
                    .iter()
                    .any(|c| c.label == PERM_FREQ && matches!(c.status, PermStatus::Denied(_)))
            });
        Some(if denied {
            "OBJECT FREQ was denied for this user.".to_string()
        } else if !r.server.lfu_enabled() {
            format!(
                "The eviction policy is {}, so Redis keeps no LFU counters. Set \
                 maxmemory-policy to allkeys-lfu or volatile-lfu to see hot keys.",
                r.server.maxmemory_policy
            )
        } else if r.sampled_keys == 0 {
            "The scan sampled no keys.".to_string()
        } else {
            "No sampled key has an LFU counter above 0 yet.".to_string()
        })
    }

    /// Sets how many list rows the screen shows, the page for PgUp and PgDn. The
    /// runtime calls it on every frame with `ui::list_rows` of the terminal area.
    pub fn set_list_rows(&mut self, rows: usize) {
        self.list_rows = rows;
    }

    /// How far PgUp and PgDn move: the visible list rows, at least one. On a view with
    /// panels the tab bar's second row takes one of the rows `ui::list_rows` counts.
    fn page(&self) -> usize {
        let panel_row = usize::from(!self.view.panels().is_empty());
        self.list_rows.saturating_sub(panel_row).max(1)
    }

    /// What the selected Patterns row shows: a group's label (`user:{id}:*`) or a
    /// pattern's name.
    fn selected_pattern_label(&self) -> Option<String> {
        let d = self.pattern_data()?;
        match self.selected_row()? {
            Row::Group { root } => match self.patterns.tree.roots.get(root)? {
                Root::Group(g) => Some(g.label.clone()),
                Root::Single(i) => Some(d.patterns.get(*i)?.pattern.clone()),
            },
            Row::Pattern { idx, .. } => Some(d.patterns.get(idx)?.pattern.clone()),
        }
    }

    /// The pattern of the selected TTL row.
    fn selected_ttl_pattern(&self) -> Option<String> {
        let rows = self.ttl_rows();
        let i = *rows.get(self.sel.ttl.min(rows.len().saturating_sub(1)))?;
        Some(self.report()?.patterns.get(i)?.pattern.clone())
    }

    /// The pattern of the selected Diff row.
    fn selected_diff_pattern(&self) -> Option<String> {
        let d = self.diff()?;
        let p = d
            .patterns
            .get(self.sel.diff.min(d.patterns.len().saturating_sub(1)))?;
        Some(p.pattern.clone())
    }

    /// The name of the selected hot key, from the scan or, without LFU counters there,
    /// from the last Monitor run.
    fn selected_hot_key(&self) -> Option<String> {
        let keys = &self.report()?.hot_keys;
        if keys.is_empty() {
            let ranked = &self.monitor_hot_keys()?.ranking.keys;
            let (k, _) = ranked.get(self.sel.hot.min(ranked.len().saturating_sub(1)))?;
            return Some(k.clone());
        }
        let k = keys.get(self.sel.hot.min(keys.len().saturating_sub(1)))?;
        Some(k.key.clone())
    }

    /// The Hot keys ranking of the last Monitor run in this session, when the scan has
    /// no hot keys of its own (spec "4 Hot keys"). Never saved: it is not in the report.
    pub fn monitor_hot_keys(&self) -> Option<&views::server::MonitorHot> {
        let r = self.report()?;
        if !r.hot_keys.is_empty() {
            return None;
        }
        self.views.server.monitor.ranking()
    }

    /// The number of rows of the TTL, Diff, Hot keys and Nodes panels; 0 for other
    /// screens.
    fn view_len(&self, screen: Screen) -> usize {
        match screen {
            Screen::Ttl => self.report().map_or(0, |r| r.patterns.len()),
            Screen::Diff => self.diff().map_or(0, |d| d.patterns.len()),
            Screen::HotKeys => match self.monitor_hot_keys() {
                Some(m) => m.ranking.keys.len(),
                None => self.report().map_or(0, |r| r.hot_keys.len()),
            },
            Screen::Nodes => self.nodes().map_or(0, <[NodeReport]>::len),
            _ => 0,
        }
    }

    // -----------------------------------------------------------------------
    // Screens and view states
    // -----------------------------------------------------------------------

    /// What is on screen: the view, its current panel, or the key list over Patterns.
    pub fn screen(&self) -> Screen {
        let panel = |i: usize| {
            let p = self.view.panels();
            p[i.min(p.len() - 1)]
        };
        match self.view {
            View::Connections => Screen::Connections,
            View::Patterns if self.patterns.keys_open && self.patterns.detail_open => {
                Screen::KeyDetail
            }
            View::Patterns if self.patterns.keys_open => Screen::KeyList,
            View::Patterns => Screen::Patterns,
            View::Analysis => panel(self.analysis_panel),
            View::Server => panel(self.server_panel),
            View::Streams => Screen::Streams,
            View::PubSub => Screen::PubSub,
            View::Search => Screen::Search,
        }
    }

    /// The screen `view` shows when switched to: its last panel, or the key list (or
    /// the key detail over it) when it was left open.
    fn screen_of(&self, view: View) -> Screen {
        let panel = |i: usize| {
            let p = view.panels();
            p[i.min(p.len() - 1)]
        };
        match view {
            View::Analysis => panel(self.analysis_panel),
            View::Server => panel(self.server_panel),
            View::Patterns if self.patterns.keys_open && self.patterns.detail_open => {
                Screen::KeyDetail
            }
            View::Patterns if self.patterns.keys_open => Screen::KeyList,
            v => Screen::ALL
                .into_iter()
                .find(|s| s.view() == v)
                .unwrap_or(Screen::Connections),
        }
    }

    fn set_screen(&mut self, screen: Screen) {
        let view = screen.view();
        self.view = view;
        let at = view.panels().iter().position(|p| *p == screen);
        match (view, at) {
            (View::Analysis, Some(i)) => self.analysis_panel = i,
            (View::Server, Some(i)) => self.server_panel = i,
            (View::Patterns, _) => {
                self.patterns.keys_open = matches!(screen, Screen::KeyList | Screen::KeyDetail);
                self.patterns.detail_open = screen == Screen::KeyDetail;
            }
            _ => {}
        }
    }

    /// Read access for the view states and their draw functions; `None` without a
    /// session.
    pub fn ctx(&self) -> Option<Ctx<'_>> {
        let s = self.session.as_ref()?;
        let perms = self.entry(s.id).and_then(|e| e.perms.as_ref());
        Some(Ctx::new(
            self.screen(),
            s,
            perms,
            self.tick,
            self.now,
            self.page(),
            self.sel_ticks(),
            &self.settings,
        ))
    }

    /// The view state that owns the screen on show, when it is working: a session
    /// exists and the screen's permission probe did not turn it off.
    pub fn active_owner(&self) -> Option<Owner> {
        let owner = self.screen().owner()?;
        let ctx = self.ctx()?;
        ctx.off_reason().is_none().then_some(owner)
    }

    /// Runs `f` on the view states with an [`Act`] for `owner`, and returns its result
    /// and the effects it asked for. `None` without a session.
    fn with_views<R>(
        &mut self,
        owner: Owner,
        what: &'static str,
        f: impl FnOnce(&mut Views, &mut Act<'_>) -> R,
    ) -> Option<(R, Vec<Effect>)> {
        #[cfg(test)]
        self.calls.push((owner, what));
        #[cfg(not(test))]
        let _ = what;
        let screen = self.screen();
        let page = self.page();
        let sel_ticks = self.sel_ticks();
        let App {
            session,
            entries,
            views,
            notice,
            next_req,
            next_tail,
            live_pending,
            live_writes,
            settings,
            tick,
            now,
            ..
        } = self;
        let s = session.as_ref()?;
        let perms = entries
            .iter()
            .find(|e| e.id == s.id)
            .and_then(|e| e.perms.as_ref());
        let ctx = Ctx::new(screen, s, perms, *tick, *now, page, sel_ticks, settings);
        let mut act = Act::new(
            ctx,
            owner,
            next_req,
            live_pending,
            notice,
            next_tail,
            live_writes,
        );
        let r = f(views, &mut act);
        let (mut effects, tail, ask) = act.into_parts();
        if let Some(cmd) = tail {
            effects.extend(self.tail_effect(cmd));
        }
        if let Some(ask) = ask {
            effects.extend(self.ask_write(ask));
        }
        effects.extend(self.after_key_edits());
        Some((r, effects))
    }

    /// The effect for what a view asked of the live tail: the session's profile and
    /// where its credentials come from are added here, so no view holds them.
    fn tail_effect(&mut self, cmd: TailCmd) -> Option<Effect> {
        let s = self.session.as_ref()?;
        let (id, scan) = (s.id, s.scan);
        match cmd {
            TailCmd::Stop { tail } => Some(Effect::Unsubscribe { id, tail }),
            TailCmd::StopMonitor { run } => Some(Effect::StopMonitor { id, run }),
            TailCmd::Monitor { run } => {
                let profile = s.profile.clone();
                let Some(secrets) = self.entry(id).map(secret_source) else {
                    self.views.server.end_monitor(views::server::Stop::Unlisted);
                    return None;
                };
                Some(Effect::Monitor {
                    id,
                    scan,
                    run,
                    profile,
                    secrets,
                })
            }
            TailCmd::Subscribe { tail, to } => {
                let profile = s.profile.clone();
                let Some(secrets) = self.entry(id).map(secret_source) else {
                    self.views
                        .pubsub
                        .tail_closed(tail, Some("this connection is no longer listed".into()));
                    return None;
                };
                Some(Effect::Subscribe {
                    id,
                    scan,
                    tail,
                    profile,
                    secrets,
                    to,
                })
            }
        }
    }

    /// Calls `f`, the method named `what`, on the working view state of the screen on
    /// show, if any.
    fn on_active(
        &mut self,
        what: &'static str,
        f: impl FnOnce(&mut dyn ViewState, &mut Act<'_>),
    ) -> Vec<Effect> {
        let Some(owner) = self.active_owner() else {
            return Vec::new();
        };
        self.view_call(owner, what, f)
    }

    /// Calls `f`, the method named `what`, on `owner`'s view state, whether or not it
    /// is working now.
    fn view_call(
        &mut self,
        owner: Owner,
        what: &'static str,
        f: impl FnOnce(&mut dyn ViewState, &mut Act<'_>),
    ) -> Vec<Effect> {
        self.with_views(owner, what, |v, act| f(v.get_mut(owner), act))
            .map_or_else(Vec::new, |(_, e)| e)
    }

    /// Shows `to`: hides the screen on show first, then tells the new one.
    fn switch(&mut self, to: Screen) -> Vec<Effect> {
        if self.screen() == to {
            return Vec::new();
        }
        let mut out = self.on_active("hide", |v, act| v.on_hide(act));
        self.set_screen(to);
        out.extend(self.on_active("show", |v, act| v.on_show(act)));
        out
    }

    /// `tab` (`back` false) and `shift-tab` on a view with panels.
    fn cycle_panel(&mut self, back: bool) -> Vec<Effect> {
        let panels = self.view.panels();
        let Some(i) = panels.iter().position(|p| *p == self.screen()) else {
            return Vec::new();
        };
        let n = panels.len();
        let to = panels[if back { (i + n - 1) % n } else { (i + 1) % n }];
        self.switch(to)
    }

    /// Passes `key` to the view state on screen. True when it did something.
    fn key_to_view(&mut self, key: KeyEvent) -> (bool, Vec<Effect>) {
        let Some(owner) = self.active_owner() else {
            return (false, Vec::new());
        };
        self.with_views(owner, "key", |v, act| v.get_mut(owner).on_key(key, act))
            .unwrap_or_default()
    }

    /// The text input of the view state on screen, if one is open.
    pub fn view_input(&self) -> Option<views::Input> {
        self.views.get(self.active_owner()?).input()
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// Ticks since the selected row last changed. Drives the marquee on long names.
    pub fn sel_ticks(&self) -> u64 {
        self.tick.saturating_sub(self.sel_since)
    }

    /// Restarts the marquee when the selected row, the screen or the session changed.
    /// A Patterns row counts by identity, not index.
    fn track_selection(&mut self) {
        let id = SelId {
            screen: self.screen(),
            conn: self.conn_sel,
            patterns: if self.screen() == Screen::Patterns {
                self.selected_row_id()
            } else {
                None
            },
            sel: self.sel,
            session: self.session.as_ref().map(|s| (s.id, s.scan)),
        };
        if id != self.sel_id {
            self.sel_id = id;
            self.sel_since = self.tick;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let effects = self.key_inner(key);
        self.track_selection();
        effects
    }

    fn key_inner(&mut self, key: KeyEvent) -> Vec<Effect> {
        if !is_press(&key) {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return vec![Effect::Quit];
        }
        if self.help {
            // The overlay takes every key: ? and esc close it, q still quits.
            match (key.code, plain_char(&key)) {
                (KeyCode::Esc, _) | (_, Some('?')) => self.help = false,
                (_, Some('q')) => return vec![Effect::Quit],
                _ => {}
            }
            return Vec::new();
        }
        if self.notice.as_ref().is_some_and(|n| n.error) {
            self.notice = None;
        }
        // The write gate's question takes every key while it is open (confirm.rs).
        if self.confirm.is_some() {
            return self.confirm_key(key);
        }
        // So does a bulk action's dialog (bulk.rs).
        if self.bulk.is_some() {
            return self.bulk_key(key);
        }
        // The command line panel takes every key while it is open (views/cmdline.rs).
        if self.views.cmdline.open {
            return self.cmdline_key(key);
        }
        if self.add.is_some() {
            return self.add_key(key);
        }
        let screen = self.screen();
        if screen == Screen::Patterns && self.patterns.filtering {
            self.filter_key(key);
            return Vec::new();
        }
        // A view's own text input takes every key, as the Patterns filter does.
        if self.view_input().is_some() {
            return self.key_to_view(key).1;
        }
        match plain_char(&key) {
            Some('q') => return vec![Effect::Quit],
            Some(':') => return self.open_cmdline(),
            Some(c @ '0'..='6') => {
                let view = View::ALL[c as usize - '0' as usize];
                return self.switch(self.screen_of(view));
            }
            Some('a') => return self.start_add(),
            Some('?') => {
                self.help = true;
                return Vec::new();
            }
            Some('s') if screen.is_scan_view() => return self.save_snapshot(),
            Some('r') if screen.is_scan_view() => return self.rescan(),
            Some('r') if screen.is_live_view() => return self.refresh(),
            Some('y') if copies(screen) => {
                return self.copy(false);
            }
            Some('Y') if copies(screen) => {
                return self.copy(true);
            }
            _ => {}
        }
        if !self.view.panels().is_empty() && matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            return self.cycle_panel(key.code == KeyCode::BackTab);
        }
        match screen {
            Screen::Connections => self.conn_key(key),
            Screen::Patterns => self.patterns_key(key),
            Screen::Ttl | Screen::Diff | Screen::HotKeys => {
                self.list_key(key);
                Vec::new()
            }
            Screen::Nodes => self.nodes_key(key),
            _ => {
                let (handled, effects) = self.key_to_view(key);
                if !handled && screen == Screen::KeyList && key.code == KeyCode::Esc {
                    let mut out = effects;
                    out.extend(self.switch(Screen::Patterns));
                    return out;
                }
                if !handled && screen == Screen::KeyList && key.code == KeyCode::Enter {
                    let mut out = effects;
                    out.extend(self.open_detail());
                    return out;
                }
                if !handled && screen == Screen::KeyDetail && key.code == KeyCode::Esc {
                    let mut out = effects;
                    out.extend(self.switch(Screen::KeyList));
                    return out;
                }
                effects
            }
        }
    }

    /// `r` on Server, Streams, Pub/Sub and Search: the panel on screen fetches now.
    fn refresh(&mut self) -> Vec<Effect> {
        let Some(ctx) = self.ctx() else {
            self.set_info("Nothing to refresh: connect first (0 Connections, then enter).");
            return Vec::new();
        };
        if let Some(why) = ctx.off_reason() {
            self.set_info(why);
            return Vec::new();
        }
        self.on_active("refresh", |v, act| v.refresh(act))
    }

    /// Only a finished report is ever saved, never partial results, and not the
    /// previous run's report while a rescan runs.
    fn save_snapshot(&mut self) -> Vec<Effect> {
        let rescan_running = self.session.as_ref().is_some_and(|s| {
            s.rescan
                && s.partial.is_none()
                && matches!(s.phase, Phase::Connecting | Phase::Scanning)
        });
        if rescan_running {
            self.set_info("A scan is running. Press s again when it finishes to save it.");
            return Vec::new();
        }
        match self.report() {
            Some(r) => vec![Effect::SaveSnapshot(Box::new(r.clone()))],
            None if self.session.as_ref().is_some_and(|s| s.partial.is_some()) => {
                self.set_info(
                    "These are partial results. Press s again when the scan finishes to save it.",
                );
                Vec::new()
            }
            None => {
                self.set_info("Nothing to save yet: connect and wait for the scan to finish.");
                Vec::new()
            }
        }
    }

    /// `r` on Patterns and Analysis: scan the session's entry again as a new run,
    /// keeping the Patterns state, the list selections, the view, the view states and,
    /// until the new run's first results, the old report. The live connection of the
    /// replaced run goes with it: its unanswered requests are forgotten.
    fn rescan(&mut self) -> Vec<Effect> {
        let Some(s) = self.session.as_ref() else {
            self.set_info("Nothing to scan again: connect first (0 Connections, then enter).");
            return Vec::new();
        };
        if matches!(s.phase, Phase::Connecting | Phase::Scanning) {
            self.set_info("A scan is already running.");
            return Vec::new();
        }
        let id = s.id;
        let Some((scan, profile, secrets)) = self.start_run(id) else {
            self.set_info("This connection is no longer listed. Pick one on 0 Connections.");
            return Vec::new();
        };
        if let Some(s) = self.session.as_mut() {
            // The report on screen: an earlier rescan's kept one, or this run's own.
            s.previous = s.report.as_ref().and(s.previous.or(Some(s.scan)));
            s.scan = scan;
            s.profile = profile.clone();
            s.target = None;
            s.progress = None;
            s.phase = Phase::Connecting;
            s.partial = None;
            s.rescan = true;
            s.live = LiveState::Waiting;
        }
        self.live_pending.clear();
        self.live_writes.clear();
        self.confirm = None;
        self.bulk = None;
        // The live tail belongs to the replaced run: the runner drops it with it.
        self.views.pubsub.end_tail();
        // So does a Monitor run.
        self.views.server.end_monitor(views::server::Stop::Rescan);
        vec![Effect::Connect {
            id,
            scan,
            profile,
            secrets,
        }]
    }

    /// `y` (`command` false) and `Y` on Patterns, TTL, Diff and Hot keys: the selected
    /// row's pattern or key name, or the suggested command for its pattern.
    fn copy(&mut self, command: bool) -> Vec<Effect> {
        // `None` inside: a command was asked for and the pattern has no safe one.
        let of_pattern = |p: String| {
            if command {
                CopyText::suggested_command(&p)
            } else {
                Some(CopyText::pattern(&p))
            }
        };
        let text = match self.screen() {
            Screen::Patterns => self.selected_pattern_label().map(of_pattern),
            Screen::Ttl => self.selected_ttl_pattern().map(of_pattern),
            Screen::Diff => self.selected_diff_pattern().map(of_pattern),
            Screen::HotKeys => self.selected_hot_key().map(|k| {
                if command {
                    CopyText::suggested_command(&pattern_for_key(&k))
                } else {
                    Some(CopyText::key(&k))
                }
            }),
            _ => return Vec::new(),
        };
        let Some(text) = text else {
            self.set_info("Nothing selected to copy.");
            return Vec::new();
        };
        let Some(text) = text else {
            self.set_info(
                "No command to copy: the pattern has characters that are not safe to paste into a shell. y copies the pattern.",
            );
            return Vec::new();
        };
        let (notice, effect) = copy_checked(text, self.tick);
        self.notice = Some(notice);
        effect.into_iter().collect()
    }

    /// Moving through the TTL, Diff, Hot keys and Nodes panels. Returns whether `key`
    /// was a move key.
    fn list_key(&mut self, key: KeyEvent) -> bool {
        let screen = self.screen();
        let len = self.view_len(screen);
        let page = self.page();
        let sel = match screen {
            Screen::Ttl => &mut self.sel.ttl,
            Screen::Diff => &mut self.sel.diff,
            Screen::HotKeys => &mut self.sel.hot,
            Screen::Nodes => &mut self.sel.nodes,
            _ => return false,
        };
        match list_move(&key, (*sel).min(len.saturating_sub(1)), len, page) {
            Some(to) => {
                *sel = to;
                true
            }
            None => false,
        }
    }

    /// The Nodes panel: move, or `enter` to narrow Patterns to the selected node.
    fn nodes_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if self.list_key(key) || key.code != KeyCode::Enter {
            return Vec::new();
        }
        self.narrow_to_selected_node()
    }

    /// Shows the selected node's own patterns on the Patterns view. A node with no
    /// results (`failed`) is not narrowed to; a notice says why.
    fn narrow_to_selected_node(&mut self) -> Vec<Effect> {
        let Some(nodes) = self.nodes() else {
            return Vec::new();
        };
        let Some(n) = nodes.get(self.sel.nodes.min(nodes.len().saturating_sub(1))) else {
            return Vec::new();
        };
        if n.status == NodeStatus::Failed {
            let text = format!(
                "{} was not scanned, so it has no patterns to show.",
                n.endpoint
            );
            self.set_info(text);
            return Vec::new();
        }
        self.patterns.node = Some(NodeKey::of(n));
        self.patterns.sel = 0;
        self.patterns.keys_open = false;
        self.patterns.detail_open = false;
        let out = self.switch(Screen::Patterns);
        self.rebuild_patterns(None);
        out
    }

    /// `esc` on a narrowed Patterns view: back to the merged patterns, with the selected
    /// pattern still selected, and back to the Nodes panel.
    fn leave_node(&mut self) -> Vec<Effect> {
        let anchor = self.selection_anchor();
        self.patterns.node = None;
        self.rebuild_patterns(anchor);
        self.switch(Screen::Nodes)
    }

    /// Pasted text, from a bracketed paste. Returns the effects to run, like
    /// [`App::handle_key`].
    pub fn handle_paste(&mut self, text: String) -> Vec<Effect> {
        // The help overlay ignores everything but its own keys.
        if self.help {
            return Vec::new();
        }
        if self.confirm.is_some() {
            self.confirm_paste(&text);
            return Vec::new();
        }
        if self.bulk.is_some() {
            self.bulk_paste(&text);
            return Vec::new();
        }
        if self.views.cmdline.open {
            return self.cmdline_paste(&text);
        }
        let text = clean_paste(&text);
        if let Some(add) = &mut self.add {
            if add.saving {
                return Vec::new();
            }
            let target = add.password.as_mut().unwrap_or(&mut add.input);
            let room = MAX_INPUT.saturating_sub(target.len());
            target.extend(text.chars().take(room));
            add.error = None;
        } else if self.screen() == Screen::Patterns && self.patterns.filtering {
            let p = &mut self.patterns;
            let room = MAX_FILTER.saturating_sub(p.filter.chars().count());
            p.filter.extend(text.chars().take(room));
            p.sel = 0;
        } else if self.view_input().is_some() {
            return self.on_active("paste", |v, act| v.on_paste(&text, act));
        } else if self.view == View::Connections {
            let out = self.start_add();
            if let Some(add) = &mut self.add {
                add.input = text.chars().take(MAX_INPUT).collect();
            }
            return out;
        }
        Vec::new()
    }

    fn start_add(&mut self) -> Vec<Effect> {
        let out = self.switch(Screen::Connections);
        self.add = Some(AddState::default());
        self.conn_sel = ConnSel::Add;
        out
    }

    fn add_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(add) = self.add.as_mut() else {
            return Vec::new();
        };
        if add.saving {
            return Vec::new();
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let target = add.password.as_mut().unwrap_or(&mut add.input);
        match key.code {
            KeyCode::Esc => self.add = None,
            KeyCode::Enter => return self.submit_add(),
            KeyCode::Backspace => {
                target.pop();
                add.error = None;
            }
            KeyCode::Char('u') if ctrl => {
                target.clear();
                add.error = None;
            }
            _ => {
                if let Some(c) = plain_char(&key)
                    && target.len() < MAX_INPUT
                {
                    target.push(c);
                    add.error = None;
                }
            }
        }
        Vec::new()
    }

    /// A name no saved profile uses yet: `name`, else `name-2`, `name-3`, ...
    fn unique_name(&self, base: &str) -> String {
        // `docker:` is reserved for discovered containers.
        let base = if crate::cli::conn::has_docker_prefix(base) {
            format!("docker-{}", &base[crate::cli::conn::DOCKER_PREFIX.len()..])
        } else {
            base.to_string()
        };
        let base = base.as_str();
        let taken = |n: &str| self.saved().any(|e| e.profile.name == n);
        if !taken(base) {
            return base.to_string();
        }
        (2..)
            .map(|i| format!("{base}-{i}"))
            .find(|n| !taken(n))
            .unwrap_or_else(|| base.to_string())
    }

    /// The profile name the add flow will save under.
    pub fn add_name(&self, parsed: &ParsedConnection) -> String {
        self.unique_name(&parsed.name_guess)
    }

    fn submit_add(&mut self) -> Vec<Effect> {
        let Some(add) = self.add.as_ref() else {
            return Vec::new();
        };
        let parsed = match add.parsed() {
            None => {
                self.add_error("Paste or type a connection string.");
                return Vec::new();
            }
            Some(Err(e)) => {
                self.add_error(e);
                return Vec::new();
            }
            Some(Ok(p)) => p,
        };
        let needs_password = parsed
            .credentials
            .as_ref()
            .is_some_and(|c| c.password.expose_secret().is_empty());
        let credentials = match (&add.password, needs_password) {
            (None, true) => {
                if let Some(add) = self.add.as_mut() {
                    add.password = Some(String::new());
                    add.error = None;
                }
                return Vec::new();
            }
            (Some(pw), _) if pw.is_empty() => {
                self.add_error("The password is empty.");
                return Vec::new();
            }
            (Some(pw), _) => Some(Credentials {
                username: parsed.credentials.as_ref().and_then(|c| c.username.clone()),
                password: SecretString::from(pw.clone()),
            }),
            (None, false) => parsed.credentials.clone(),
        };
        let profile = Profile {
            name: self.add_name(&parsed),
            env: parsed.env_guess,
            endpoint: parsed.endpoint.clone(),
            db: parsed.db,
            auth: if credentials.is_some() {
                Auth::Keychain
            } else {
                Auth::None
            },
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
        };
        if let Some(add) = self.add.as_mut() {
            add.saving = true;
            add.error = None;
        }
        vec![Effect::SaveConnection {
            profile,
            credentials,
        }]
    }

    fn add_error(&mut self, e: impl Into<String>) {
        if let Some(add) = self.add.as_mut() {
            add.error = Some(e.into());
        }
    }

    fn conn_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let items = self.selectable();
        let cur = items.iter().position(|s| *s == self.conn_sel).unwrap_or(0);
        if let Some(to) = list_move(&key, cur, items.len(), self.page()) {
            // `selectable` always ends with the add row, so `to` is in range.
            if let Some(sel) = items.get(to) {
                self.conn_sel = *sel;
            }
            return Vec::new();
        }
        match key.code {
            KeyCode::Enter => match self.conn_sel {
                ConnSel::Add => return self.start_add(),
                ConnSel::Entry(id) => return self.connect(id),
            },
            _ => match plain_char(&key) {
                Some('r') => return self.rescan_docker(),
                Some('w') => return self.toggle_writes(),
                _ => {}
            },
        }
        Vec::new()
    }

    fn rescan_docker(&mut self) -> Vec<Effect> {
        if self.docker == DockerState::Searching {
            return Vec::new();
        }
        self.docker = DockerState::Searching;
        vec![Effect::DiscoverDocker]
    }

    /// What connecting and rescanning share: marks entry `id` as connecting (and a
    /// replaced session's entry, if it was still connecting, as not open), allocates a
    /// new run and starts the keychain-hint clock. Returns the run with the entry's
    /// profile and secret source, or `None` when the entry is gone.
    fn start_run(&mut self, id: ConnId) -> Option<(ScanId, Profile, SecretSource)> {
        if let Some(prev) = self.session.as_ref().map(|s| s.id)
            && prev != id
            && let Some(e) = self.entry_mut(prev)
            && e.status == ConnStatus::Connecting
        {
            e.status = ConnStatus::NotOpen;
        }
        let entry = self.entry_mut(id)?;
        entry.status = ConnStatus::Connecting;
        entry.error = None;
        let profile = entry.profile.clone();
        let secrets = secret_source(entry);
        let scan = self.next_scan;
        self.next_scan += 1;
        self.connect_started = self.now;
        Some((scan, profile, secrets))
    }

    fn connect(&mut self, id: ConnId) -> Vec<Effect> {
        let Some((scan, profile, secrets)) = self.start_run(id) else {
            return Vec::new();
        };
        let server = self.entry(id).and_then(|e| e.server.clone());
        // A new connect checks permissions again; until then no view is off because of
        // what the last connect found.
        if let Some(e) = self.entry_mut(id) {
            e.perms = None;
        }
        self.session = Some(Session {
            id,
            scan,
            profile: profile.clone(),
            server,
            target: None,
            progress: None,
            phase: Phase::Connecting,
            partial: None,
            report: None,
            history: History::Pending,
            trend: None,
            previous: None,
            rescan: false,
            live: LiveState::Waiting,
            refusals: crate::write::Refusals::default(),
            sentinel: None,
        });
        self.patterns = PatternsState::default();
        self.sel = ViewSel::default();
        // The previous session's view states and live requests go with it; nothing is
        // shown from them, so none is told it is hidden.
        self.views = Views::default();
        self.live_pending.clear();
        self.live_writes.clear();
        self.confirm = None;
        self.bulk = None;
        self.view = View::Patterns;
        vec![Effect::Connect {
            id,
            scan,
            profile,
            secrets,
        }]
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let p = &mut self.patterns;
        match key.code {
            KeyCode::Esc => {
                p.filter.clear();
                p.filtering = false;
                p.sel = 0;
            }
            KeyCode::Enter => p.filtering = false,
            KeyCode::Backspace => {
                p.filter.pop();
                p.sel = 0;
            }
            _ => {
                if let Some(c) = plain_char(&key)
                    && p.filter.chars().count() < MAX_FILTER
                {
                    p.filter.push(c);
                    p.sel = 0;
                }
            }
        }
    }

    fn patterns_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let rows = self.pattern_rows();
        let last = rows.len().saturating_sub(1);
        let sel = self.patterns.sel.min(last);
        if let Some(to) = list_move(&key, sel, rows.len(), self.page()) {
            self.patterns.sel = to;
            return Vec::new();
        }
        match (key.code, plain_char(&key)) {
            (KeyCode::Right, _) | (_, Some('l')) => self.expand(&rows, sel),
            (KeyCode::Left, _) | (_, Some('h')) => self.collapse(&rows, sel),
            (KeyCode::Enter, _) => match self.selected_single_pattern() {
                Some(pattern) => return self.open_keys(pattern),
                None => self.toggle(&rows, sel),
            },
            (_, Some('/')) => {
                self.patterns.filtering = true;
            }
            (KeyCode::Esc, _) if !self.patterns.filter.is_empty() => {
                self.patterns.filter.clear();
                self.patterns.sel = 0;
            }
            (KeyCode::Esc, _) if self.patterns.node.is_some() => return self.leave_node(),
            (_, Some('o')) => self.cycle_sort(),
            (_, Some('x')) => return self.bulk_start(false),
            (_, Some('D')) => return self.bulk_start(true),
            _ => {}
        }
        Vec::new()
    }

    /// The selected Patterns row's pattern when it is a single pattern (a group's child
    /// or a pattern of its own), not a group.
    fn selected_single_pattern(&self) -> Option<String> {
        let d = self.pattern_data()?;
        let idx = match self.selected_row()? {
            Row::Group { root } => match self.patterns.tree.roots.get(root)? {
                Root::Single(i) => *i,
                Root::Group(_) => return None,
            },
            Row::Pattern { idx, .. } => idx,
        };
        Some(d.patterns.get(idx)?.pattern.clone())
    }

    /// `enter` on a pattern: the key list of `pattern`, one level down.
    fn open_keys(&mut self, pattern: String) -> Vec<Effect> {
        let mut out = self
            .with_views(Owner::Keys, "open", |v, act| v.keys.open(pattern, act))
            .map_or_else(Vec::new, |(_, e)| e);
        out.extend(self.switch(Screen::KeyList));
        out
    }

    /// `enter` on the key list: the selected key's detail, one level down. With no key
    /// selected it says so and stays.
    fn open_detail(&mut self) -> Vec<Effect> {
        let Some(row) = self.views.keys.selected().cloned() else {
            self.set_info("No key selected to open.");
            return Vec::new();
        };
        let pattern = self.views.keys.pattern.clone().unwrap_or_default();
        let mut out = self
            .with_views(Owner::Value, "open", |v, act| {
                v.value.open(row, pattern, act)
            })
            .map_or_else(Vec::new, |(_, e)| e);
        out.extend(self.switch(Screen::KeyDetail));
        out
    }

    /// `o`: the next sort (see [`SortBy::next`]). The selected row stays selected.
    fn cycle_sort(&mut self) {
        let sizes = self.pattern_data().is_none_or(|d| d.sizes_available);
        let anchor = self.selection_anchor();
        self.patterns.sort = self.patterns.sort.next(sizes);
        self.rebuild_patterns(anchor);
        let label = self.patterns.sort.effective(sizes).label();
        self.set_info(format!("Sorted by {label}"));
    }

    fn group(&self, root: usize) -> Option<Group> {
        match self.patterns.tree.roots.get(root) {
            Some(Root::Group(g)) => Some(g.clone()),
            _ => None,
        }
    }

    /// Collapses `g`, remembering its patterns (see [`Collapsed`]).
    fn collapse_group(&mut self, g: &Group) {
        let node = self.patterns.node.as_ref();
        if let Some(d) = self.session.as_ref().and_then(|s| pattern_data_of(s, node)) {
            self.patterns.collapsed.collapse(g, d.patterns);
        }
    }

    fn expand(&mut self, rows: &[Row], sel: usize) {
        let Some(Row::Group { root }) = rows.get(sel).copied() else {
            return;
        };
        let Some(g) = self.group(root) else {
            return;
        };
        if !self.patterns.collapsed.expand(&g.key)
            && matches!(rows.get(sel + 1), Some(Row::Pattern { parent: Some(p), .. }) if *p == root)
        {
            self.patterns.sel = sel + 1;
        }
    }

    fn collapse(&mut self, rows: &[Row], sel: usize) {
        match rows.get(sel).copied() {
            Some(Row::Group { root }) => {
                if let Some(g) = self.group(root) {
                    self.collapse_group(&g);
                }
            }
            Some(Row::Pattern {
                parent: Some(root), ..
            }) => {
                if let Some(i) = rows.iter().position(|r| *r == Row::Group { root }) {
                    self.patterns.sel = i;
                }
            }
            _ => {}
        }
    }

    fn toggle(&mut self, rows: &[Row], sel: usize) {
        if let Some(Row::Group { root }) = rows.get(sel).copied()
            && let Some(g) = self.group(root)
            && !self.patterns.collapsed.expand(&g.key)
        {
            self.collapse_group(&g);
        }
    }

    // -----------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------

    /// True when `id` and `scan` are the current session's entry and run. A message
    /// from a replaced run of the same entry has the right `id` but an older `scan`.
    fn is_session(&self, id: ConnId, scan: ScanId) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.id == id && s.scan == scan)
    }

    /// True when a [`Msg::History`] or [`Msg::Trend`] of `id`'s run `scan` for the scan
    /// taken at `taken_at_unix` belongs on screen: this run's, or that of the previous
    /// run whose report is still kept, and for the report shown.
    fn history_applies(&self, id: ConnId, scan: ScanId, taken_at_unix: i64) -> bool {
        self.session.as_ref().is_some_and(|s| {
            // This run's, or that of the previous run whose report is still kept.
            let run = s.scan == scan || s.previous == Some(scan);
            s.id == id && run && s.report.as_ref().map(|r| r.taken_at_unix) == Some(taken_at_unix)
        })
    }

    /// Apply a message. Returns follow-up work, e.g. loading history after a scan.
    pub fn apply(&mut self, msg: Msg) -> Vec<Effect> {
        let effects = self.apply_inner(msg);
        self.track_selection();
        effects
    }

    fn apply_inner(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::TickAt(now) => {
                self.now = Some(now);
                if self.connect_started.is_none() && self.is_connecting() {
                    self.connect_started = Some(now);
                }
                let mut out = self.apply_inner(Msg::Tick);
                // Only the view on screen polls.
                out.extend(self.on_active("tick", |v, act| v.on_tick(now, act)));
                out.extend(self.cmdline_tick(now));
                return out;
            }
            Msg::Tick => {
                self.tick += 1;
                if self
                    .notice
                    .as_ref()
                    .and_then(|n| n.until)
                    .is_some_and(|t| self.tick >= t)
                {
                    self.notice = None;
                }
            }
            Msg::Edited { job, result } => return self.edited(job, result),
            Msg::WritesSaved { name, on } => self.writes_saved(&name, on),
            Msg::WritesFailed { name, on, error } => self.writes_failed(&name, on, &error),
            Msg::DockerFound(found) => self.set_docker(found),
            Msg::DockerFailed(_) => {
                self.set_docker(Vec::new());
                self.docker = DockerState::Unavailable;
            }
            Msg::SentinelResolved { id, scan, answer } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                if let Some(e) = self.entry_mut(id) {
                    e.sentinel = Some(answer.clone());
                }
                if let Some(s) = self.session.as_mut() {
                    s.sentinel = Some(answer);
                }
            }
            Msg::Connected {
                id,
                scan,
                server,
                perms,
                target,
            } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                self.connect_started = None;
                // Only the scan's own checks make a connection limited; a denied view
                // probe turns its view off and nothing else.
                let limited = perms.checks.iter().any(|c| {
                    PERM_LABELS.contains(&c.label) && matches!(c.status, PermStatus::Denied(_))
                });
                // The checks can turn the view on screen off, or back on (a rescan
                // after an ACL change): tell it, as if it was switched away or to.
                let before = self.active_owner();
                if let Some(e) = self.entry_mut(id) {
                    e.status = if limited {
                        ConnStatus::Limited
                    } else {
                        ConnStatus::Connected
                    };
                    e.server = server.clone();
                    e.perms = Some(perms);
                    e.target = Some(target.clone());
                }
                let refusals = self
                    .entry(id)
                    .and_then(|e| e.perms.as_ref())
                    .map(confirm::refusals_of)
                    .unwrap_or_default();
                if let Some(s) = self.session.as_mut() {
                    s.server = server;
                    s.target = Some(target);
                    s.phase = Phase::Scanning;
                    s.refusals = refusals;
                }
                let after = self.active_owner();
                if before != after {
                    let mut out = Vec::new();
                    if let Some(o) = before {
                        out.extend(self.view_call(o, "hide", |v, act| v.on_hide(act)));
                    }
                    if let Some(o) = after {
                        out.extend(self.view_call(o, "show", |v, act| v.on_show(act)));
                    }
                    return out;
                }
            }
            Msg::ConnectFailed { id, scan, error } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                self.connect_started = None;
                if let Some(e) = self.entry_mut(id) {
                    e.status = ConnStatus::Failed;
                    e.error = Some(error.clone());
                }
                if let Some(s) = self.session.as_mut() {
                    s.phase = Phase::Failed(error.clone());
                }
                self.set_error(format!("Could not connect: {error}"));
            }
            Msg::ScanProgress { id, scan, progress } => {
                if let Some(s) = self.session.as_mut()
                    && s.id == id
                    && s.scan == scan
                    && s.phase != Phase::Done
                {
                    s.progress = Some(progress);
                }
            }
            Msg::ScanPartial { id, scan, partial } => {
                // Only while this session's own scan runs: not from a replaced run of
                // the same entry, not before it is connected, not after it finished or
                // failed. A report on screen blocks it unless it is the previous run's,
                // kept during a rescan.
                let Some(s) = self.session.as_mut() else {
                    return Vec::new();
                };
                if s.id != id
                    || s.scan != scan
                    || s.phase != Phase::Scanning
                    || (s.report.is_some() && s.previous.is_none())
                {
                    return Vec::new();
                }
                // Taken from the rows on screen, the previous report's included.
                let anchor = self.selection_anchor();
                if let Some(s) = self.session.as_mut() {
                    if s.previous.take().is_some() {
                        // The first results of a rescan replace the previous report.
                        s.report = None;
                        s.history = History::Pending;
                        s.trend = None;
                    }
                    s.partial = Some(partial);
                }
                // Partial results are merged over the cluster: no node to narrow to.
                self.patterns.node = None;
                self.rebuild_patterns(anchor);
            }
            Msg::ScanDone { id, scan, report } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                let report = *report;
                if let Some(e) = self.entry_mut(id) {
                    e.server = Some(report.server.clone());
                }
                // The selection follows its row from the partial results, or from the
                // previous report of a rescan, if any.
                let anchor = self.selection_anchor();
                let rescan = self.session.as_ref().is_some_and(|s| s.rescan);
                if !rescan {
                    self.sel = ViewSel::default();
                }
                let effect = Effect::LoadHistory {
                    id,
                    scan,
                    report: Box::new(report.clone()),
                };
                if let Some(s) = self.session.as_mut() {
                    s.server = Some(report.server.clone());
                    s.phase = Phase::Done;
                    s.partial = None;
                    s.report = Some(report);
                    s.history = History::Pending;
                    s.trend = None;
                    s.previous = None;
                }
                // Narrowed to a node this report doesn't have: back to the merged rows.
                if self
                    .patterns
                    .node
                    .as_ref()
                    .is_some_and(|k| self.report().and_then(|r| k.find(r)).is_none())
                {
                    self.patterns.node = None;
                }
                self.rebuild_patterns(anchor);
                if rescan {
                    self.sel.ttl = self
                        .sel
                        .ttl
                        .min(self.view_len(Screen::Ttl).saturating_sub(1));
                    self.sel.hot = self
                        .sel
                        .hot
                        .min(self.view_len(Screen::HotKeys).saturating_sub(1));
                    self.sel.nodes = self
                        .sel
                        .nodes
                        .min(self.view_len(Screen::Nodes).saturating_sub(1));
                }
                return vec![effect];
            }
            Msg::History {
                id,
                scan,
                taken_at_unix,
                result,
            } => {
                if !self.history_applies(id, scan, taken_at_unix) {
                    return Vec::new();
                }
                let Some(s) = self.session.as_mut() else {
                    return Vec::new();
                };
                let rescan = s.rescan;
                match result {
                    Ok(Some(d)) => s.history = History::Diff(d),
                    Ok(None) => s.history = History::NoSnapshot,
                    Err(e) => {
                        s.history = History::Failed(e.clone());
                        self.set_error(format!("Could not load snapshots: {e}"));
                    }
                }
                self.sel.diff = if rescan {
                    self.sel
                        .diff
                        .min(self.view_len(Screen::Diff).saturating_sub(1))
                } else {
                    0
                };
            }
            Msg::Trend {
                id,
                scan,
                taken_at_unix,
                trend,
            } => {
                if !self.history_applies(id, scan, taken_at_unix) {
                    return Vec::new();
                }
                if let Some(s) = self.session.as_mut() {
                    s.trend = trend;
                }
            }
            Msg::SnapshotSaved(path) => {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into(),
                );
                self.set_info(format!("Saved snapshot {name}"));
            }
            Msg::SnapshotFailed(e) => self.set_error(format!("Could not save snapshot: {e}")),
            Msg::CopyFailed(e) => self.set_error(format!("Could not copy: {e}")),
            Msg::LiveReady { id, scan } => {
                if let Some(s) = self.session.as_mut()
                    && s.id == id
                    && s.scan == scan
                {
                    s.live = LiveState::Ready;
                }
            }
            Msg::Live {
                id,
                scan,
                req_id,
                result,
            } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                // A bulk action's step goes to its dialog (bulk.rs).
                if self.bulk_waits_for(req_id) {
                    return self.bulk_reply(req_id, result);
                }
                // Back to the view that sent it; an id no view waits for any more (its
                // connection closed in between) is dropped.
                let Some(owner) = self.live_pending.remove(&req_id) else {
                    return Vec::new();
                };
                if let Some(name) = self.live_writes.remove(&req_id)
                    && let Err(crate::live::LiveError::Denied(_)) = &result
                    && let Some(s) = self.session.as_mut()
                {
                    s.refusals.add(&name);
                }
                return self
                    .with_views(owner, "reply", |v, act| {
                        v.get_mut(owner).on_reply(req_id, result, act)
                    })
                    .map_or_else(Vec::new, |(_, e)| e);
            }
            Msg::LiveClosed { id, scan, error } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                self.live_pending.clear();
                self.live_writes.clear();
                self.close_write_question();
                self.bulk_closed();
                if let Some(e) = &error {
                    self.set_error(format!(
                        "The live connection closed: {e}. The server views stop until the next scan."
                    ));
                }
                if let Some(s) = self.session.as_mut() {
                    s.live = LiveState::Closed(error);
                }
                // A Monitor run stops with the live connection (spec "Monitor (P2)").
                if let Some(run) = self.views.server.end_monitor(views::server::Stop::Closed) {
                    return vec![Effect::StopMonitor { id, run }];
                }
            }
            Msg::MonitorReady {
                id,
                scan,
                run,
                at_unix_ms,
            } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                return self
                    .with_views(Owner::Server, "monitor_ready", |v, act| {
                        v.server.monitor_ready(run, at_unix_ms, act)
                    })
                    .map_or_else(Vec::new, |(_, e)| e);
            }
            Msg::MonitorLines {
                id,
                scan,
                run,
                lines,
            } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                return self
                    .with_views(Owner::Server, "monitor_lines", |v, act| {
                        v.server.monitor_lines(run, lines, act)
                    })
                    .map_or_else(Vec::new, |(_, e)| e);
            }
            Msg::MonitorClosed { id, scan, run, end } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                return self
                    .with_views(Owner::Server, "monitor_closed", |v, act| {
                        v.server.monitor_closed(run, end, act)
                    })
                    .map_or_else(Vec::new, |(_, e)| e);
            }
            Msg::TailReady { id, scan, tail } => {
                if self.is_session(id, scan) {
                    self.views.pubsub.tail_ready(tail);
                }
            }
            Msg::Tail {
                id,
                scan,
                tail,
                batch,
                dropped,
            } => {
                if self.is_session(id, scan) {
                    self.views.pubsub.tail_batch(tail, batch, dropped);
                }
            }
            Msg::TailClosed {
                id,
                scan,
                tail,
                error,
            } => {
                if self.is_session(id, scan) {
                    self.views.pubsub.tail_closed(tail, error);
                }
            }
            Msg::ScanFailed { id, scan, error } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                if let Some(s) = self.session.as_mut() {
                    s.phase = Phase::Failed(error.clone());
                    // Partial results of a failed scan must not stay up as if final.
                    if s.partial.take().is_some() {
                        self.patterns.tree = PatternTree::default();
                        self.patterns.sel = 0;
                    }
                }
                self.set_error(format!("Scan failed: {error}"));
            }
            Msg::Saved(profile) => {
                let id = self.alloc_id();
                let at = self
                    .entries
                    .iter()
                    .position(|e| e.is_docker())
                    .unwrap_or(self.entries.len());
                let mut line = format!(
                    "Saved {} ({}, {}).",
                    profile.name,
                    profile.endpoint,
                    profile.env.label()
                );
                if profile.auth == Auth::Keychain {
                    line.push_str(" Credentials are in the OS keychain.");
                }
                self.entries.insert(
                    at,
                    ConnEntry {
                        id,
                        profile,
                        source: Source::Saved,
                        status: ConnStatus::NotOpen,
                        server: None,
                        perms: None,
                        target: None,
                        sentinel: None,
                        error: None,
                        just_saved: true,
                    },
                );
                self.add = None;
                self.conn_sel = ConnSel::Entry(id);
                self.set_info(line);
            }
            Msg::SaveFailed(error) => {
                if let Some(add) = self.add.as_mut() {
                    add.saving = false;
                    add.error = Some(mask(&error));
                } else {
                    self.set_error(mask(&error));
                }
            }
        }
        Vec::new()
    }

    /// Replace the Docker rows, keeping the state of containers seen before.
    fn set_docker(&mut self, found: Vec<DiscoveredInstance>) {
        self.docker = DockerState::Found;
        let old: Vec<ConnEntry> = self
            .entries
            .iter()
            .filter(|e| e.is_docker())
            .cloned()
            .collect();
        self.entries.retain(|e| !e.is_docker());
        for inst in found {
            let keep = old.iter().find(|e| {
                matches!(&e.source, Source::Docker { container, .. } if *container == inst.container)
            });
            let source = Source::Docker {
                container: inst.container,
                image: inst.image,
                credentials: inst.credentials,
                auth: inst.auth,
            };
            let entry = match keep {
                Some(e) => ConnEntry {
                    profile: inst.profile,
                    source,
                    ..e.clone()
                },
                None => ConnEntry {
                    id: self.alloc_id(),
                    profile: inst.profile,
                    source,
                    status: ConnStatus::NotOpen,
                    server: None,
                    perms: None,
                    target: None,
                    sentinel: None,
                    error: None,
                    just_saved: false,
                },
            };
            self.entries.push(entry);
        }
        // Keep the selection, unless it vanished or nothing could be picked before.
        let nothing_before = old.is_empty() && self.saved().next().is_none() && self.add.is_none();
        if !self.selectable().contains(&self.conn_sel) || nothing_before {
            self.conn_sel = self
                .entries
                .first()
                .map_or(ConnSel::Add, |e| ConnSel::Entry(e.id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Env, PERM_MEMORY};
    use crate::tui::fixtures;
    use crate::tui::tree::Collapsed;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    /// The current session's run, for messages that should be accepted.
    fn scan_of(app: &App) -> ScanId {
        app.session.as_ref().unwrap().scan
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    /// Goes to `screen` with keys: its view's digit, then `tab` to the panel.
    fn show(app: &mut App, screen: Screen) {
        views::testing::show(app, screen);
    }

    /// The screens the five views of v0.2 became: `0` Connections, `1` Patterns, and
    /// `2`, `3`, `4` for the TTL, Diff and Hot keys panels of Analysis.
    const LEGACY: [Screen; 5] = [
        Screen::Connections,
        Screen::Patterns,
        Screen::Ttl,
        Screen::Diff,
        Screen::HotKeys,
    ];

    /// Goes to the screen that was view `c` (`'0'` to `'4'`) before Analysis had panels.
    fn go(app: &mut App, c: char) {
        show(app, LEGACY[c as usize - '0' as usize]);
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(ch(c));
        }
    }

    /// Connected to the first profile and scanning.
    fn scanning() -> (App, ConnId) {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, .. }] = effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        app.apply(Msg::Connected {
            id,
            scan: scan_of(&app),
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
        (app, id)
    }

    #[test]
    fn selection_change_restarts_the_marquee() {
        let (mut app, id) = scanning();
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: fixtures::partial(true),
        });
        app.handle_key(ch('1'));
        assert_eq!(app.sel_ticks(), 0);
        for _ in 0..7 {
            app.apply(Msg::Tick);
        }
        assert_eq!(app.sel_ticks(), 7);
        // Another partial result for the same selection does not restart it.
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: fixtures::partial(true),
        });
        assert_eq!(app.sel_ticks(), 7);
        // Moving the selection does.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.sel_ticks(), 0);
        app.apply(Msg::Tick);
        app.apply(Msg::Tick);
        assert_eq!(app.sel_ticks(), 2);
        // So does a view switch.
        go(&mut app, '2');
        assert_eq!(app.sel_ticks(), 0);
        app.apply(Msg::Tick);
        assert_eq!(app.sel_ticks(), 1);
    }

    #[test]
    fn partial_results_then_done_replaces_them() {
        let (mut app, id) = scanning();
        assert!(app.pattern_data().is_none());
        let partial = fixtures::partial(true);
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: partial.clone(),
        });
        let data = app.pattern_data().unwrap();
        assert_eq!(data.partial, Some(&*partial));
        assert_eq!(data.patterns, &partial.patterns[..]);
        assert!(!app.pattern_rows().is_empty());
        // Not a report: the TTL, Diff and Hot keys views keep waiting.
        assert!(app.report().is_none());
        assert!(app.ttl_rows().is_empty());
        assert!(app.hot_keys_reason().is_none());

        // Move down, then a newer partial with fewer patterns keeps the tree in step.
        app.handle_key(key(KeyCode::Down));
        let mut fewer = (*partial).clone();
        fewer.patterns.truncate(1);
        fewer.sampled = 6_400;
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: Arc::new(fewer),
        });
        assert_eq!(app.pattern_data().unwrap().patterns.len(), 1);
        assert_eq!(app.pattern_rows().len(), 1);

        let report = fixtures::report(true);
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(report.clone()),
        });
        let s = app.session.as_ref().unwrap();
        assert!(s.partial.is_none());
        let data = app.pattern_data().unwrap();
        assert!(data.partial.is_none());
        assert_eq!(data.patterns, &report.patterns[..]);
        assert_eq!(
            app.pattern_rows(),
            PatternTree::build(&report.patterns, true).visible(
                &report.patterns,
                &Collapsed::default(),
                ""
            )
        );
        // Now `s` saves the final report.
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(r)] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(**r, report);

        // A partial arriving after the scan finished is ignored.
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial,
        });
        assert!(app.session.as_ref().unwrap().partial.is_none());
        assert_eq!(app.pattern_data().unwrap().patterns, &report.patterns[..]);
    }

    #[test]
    fn snapshot_key_while_partial_shows_hint() {
        let (mut app, id) = scanning();
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: fixtures::partial(true),
        });
        // `s` does not save partial results, on any report view; it says why.
        for view in ['1', '2', '3', '4'] {
            go(&mut app, view);
            assert!(app.handle_key(ch('s')).is_empty(), "view {view}");
            let notice = app.notice.clone().unwrap();
            assert!(!notice.error);
            assert_eq!(
                notice.text,
                "These are partial results. Press s again when the scan finishes to save it."
            );
        }
    }

    #[test]
    fn cancelled_scan_drops_partial_results() {
        // Pressing enter again on the same connection cancels its scan (the runtime
        // aborts the task) and starts a new one under the same id, as a new run.
        let (mut app, id) = scanning();
        let old = scan_of(&app);
        app.apply(Msg::ScanPartial {
            id,
            scan: old,
            partial: fixtures::partial(true),
        });
        app.handle_key(ch('0'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::Connect {
                id: again,
                scan: new,
                ..
            },
        ] = effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(again, id);
        assert_ne!(new, old);
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.phase, Phase::Connecting);
        assert!(s.partial.is_none());
        assert!(app.pattern_data().is_none());
        assert!(app.patterns.tree.roots.is_empty());
        // A partial the cancelled scan sent before it stopped arrives while the new
        // one is still connecting: ignored.
        app.apply(Msg::ScanPartial {
            id,
            scan: old,
            partial: fixtures::partial(true),
        });
        assert!(app.pattern_data().is_none());
        // So is one of the new run before it is connected.
        app.apply(Msg::ScanPartial {
            id,
            scan: new,
            partial: fixtures::partial(true),
        });
        assert!(app.pattern_data().is_none());
        // The new scan's own partials show once it is connected.
        app.apply(Msg::Connected {
            id,
            scan: scan_of(&app),
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: fixtures::partial(false),
        });
        assert!(!app.pattern_data().unwrap().sizes_available);
    }

    #[test]
    fn switching_profile_while_partial_drops_it() {
        let (mut app, first) = scanning();
        let first_scan = scan_of(&app);
        app.apply(Msg::ScanPartial {
            id: first,
            scan: first_scan,
            partial: fixtures::partial(true),
        });
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ch('0'));
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id: second, .. }] = effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        assert_ne!(second, first);
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.profile.name, "staging-cache");
        assert!(s.partial.is_none());
        assert!(app.pattern_data().is_none());
        assert!(app.patterns.tree.roots.is_empty());
        assert_eq!(app.patterns.sel, 0);
        app.apply(Msg::Connected {
            id: second,
            scan: scan_of(&app),
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
        // The first profile's partials stay out even once the second is scanning.
        app.apply(Msg::ScanPartial {
            id: first,
            scan: first_scan,
            partial: fixtures::partial(true),
        });
        assert!(app.pattern_data().is_none());
    }

    #[test]
    fn partial_results_then_failure_clears_them() {
        let (mut app, id) = scanning();
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: fixtures::partial(true),
        });
        assert!(app.pattern_data().is_some());
        app.apply(Msg::ScanFailed {
            id,
            scan: scan_of(&app),
            error: "connection reset".into(),
        });
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.phase, Phase::Failed("connection reset".into()));
        assert!(s.partial.is_none());
        assert!(app.pattern_data().is_none());
        assert!(app.pattern_rows().is_empty());
        assert!(app.patterns.tree.roots.is_empty());
        // Late partials of the failed scan stay out.
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: fixtures::partial(true),
        });
        assert!(app.pattern_data().is_none());
        // `s` has nothing to save.
        assert!(app.handle_key(ch('s')).is_empty());
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .starts_with("Nothing to save")
        );
    }

    #[test]
    fn stale_partial_results_are_ignored() {
        let (mut app, id) = scanning();
        // Another connection's id.
        app.apply(Msg::ScanPartial {
            id: id + 100,
            scan: scan_of(&app),
            partial: fixtures::partial(true),
        });
        assert!(app.pattern_data().is_none());

        // Switch to the second profile: the first one's partials no longer apply, and
        // the new session takes none until it is connected.
        app.handle_key(ch('0'));
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id: second, .. }] = effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        assert_ne!(second, id);
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(&app),
            partial: fixtures::partial(true),
        });
        app.apply(Msg::ScanPartial {
            id: second,
            scan: scan_of(&app),
            partial: fixtures::partial(true),
        });
        assert!(app.pattern_data().is_none());
        assert!(app.patterns.tree.roots.is_empty());
    }

    /// Connected to the first profile (run `s1`), then Enter on it again (run `s2`):
    /// returns the app before `s2` is connected, with the entry id and both runs.
    fn reconnected_same_entry() -> (App, ConnId, ScanId, ScanId) {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, scan: s1, .. }] = effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        app.apply(Msg::Connected {
            id,
            scan: s1,
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
        assert_eq!(app.session.as_ref().unwrap().phase, Phase::Scanning);
        app.handle_key(ch('0'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::Connect {
                id: again,
                scan: s2,
                ..
            },
        ] = effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(again, id);
        assert_ne!(s2, s1);
        assert_eq!(scan_of(&app), s2);
        (app, id, s1, s2)
    }

    /// Every message the replaced run `s1` can still send, as the runtime would send it.
    fn stale_messages(id: ConnId, s1: ScanId) -> Vec<Msg> {
        let progress = ScanProgress {
            sampled: 9_000,
            target: 10_000,
            done: false,
        };
        vec![
            Msg::ScanPartial {
                id,
                scan: s1,
                partial: fixtures::partial(true),
            },
            Msg::ScanProgress {
                id,
                scan: s1,
                progress,
            },
            Msg::Connected {
                id,
                scan: s1,
                server: Some(fixtures::server()),
                perms: fixtures::perms_memory_denied(),
                target: ScanTarget::PrimaryFallback("stale".into()),
            },
            Msg::ConnectFailed {
                id,
                scan: s1,
                error: "stale connect error".into(),
            },
            Msg::ScanFailed {
                id,
                scan: s1,
                error: "stale scan error".into(),
            },
            Msg::ScanDone {
                id,
                scan: s1,
                report: Box::new(fixtures::report(true)),
            },
            Msg::History {
                id,
                scan: s1,
                taken_at_unix: fixtures::report(true).taken_at_unix,
                result: Ok(Some(fixtures::history(true))),
            },
        ]
    }

    /// Asserts the session is `s2`, connected and scanning, untouched by `s1`.
    fn assert_untouched(app: &App, id: ConnId, s2: ScanId, phase: Phase) {
        let s = app.session.as_ref().unwrap();
        assert_eq!((s.id, s.scan), (id, s2));
        assert_eq!(s.phase, phase);
        assert!(s.partial.is_none());
        assert!(s.progress.is_none());
        assert!(s.report.is_none());
        assert_eq!(s.history, History::Pending);
        assert!(app.pattern_data().is_none());
        assert!(app.patterns.tree.roots.is_empty());
        assert!(app.notice.is_none(), "{:?}", app.notice);
        let e = app.entry(id).unwrap();
        assert!(e.error.is_none());
        if phase == Phase::Scanning {
            assert_eq!(s.target, Some(ScanTarget::Primary));
            assert_eq!(e.status, ConnStatus::Connected);
            assert_eq!(e.target, Some(ScanTarget::Primary));
        } else {
            assert_eq!(e.status, ConnStatus::Connecting);
        }
    }

    #[test]
    fn replaced_run_of_the_same_entry_is_ignored() {
        let (mut app, id, s1, s2) = reconnected_same_entry();
        // Before s2 is connected: nothing from s1 gets in, not even its Connected.
        for msg in stale_messages(id, s1) {
            let what = format!("{msg:?}");
            assert!(app.apply(msg).is_empty(), "{what}");
            assert_untouched(&app, id, s2, Phase::Connecting);
        }
        app.apply(Msg::Connected {
            id,
            scan: s2,
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
        assert_untouched(&app, id, s2, Phase::Scanning);
        // After s2's Connected: same entry id, but s1's messages are all ignored.
        for msg in stale_messages(id, s1) {
            let what = format!("{msg:?}");
            assert!(app.apply(msg).is_empty(), "{what}");
            assert_untouched(&app, id, s2, Phase::Scanning);
        }

        // s2's own messages are accepted.
        app.apply(Msg::ScanProgress {
            id,
            scan: s2,
            progress: ScanProgress {
                sampled: 100,
                target: 10_000,
                done: false,
            },
        });
        assert_eq!(app.session.as_ref().unwrap().progress.unwrap().sampled, 100);
        let partial = fixtures::partial(false);
        app.apply(Msg::ScanPartial {
            id,
            scan: s2,
            partial: partial.clone(),
        });
        assert_eq!(app.pattern_data().unwrap().partial, Some(&*partial));
        let report = fixtures::report(true);
        let effects = app.apply(Msg::ScanDone {
            id,
            scan: s2,
            report: Box::new(report.clone()),
        });
        let [Effect::LoadHistory { id: got, scan, .. }] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!((*got, *scan), (id, s2));
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.phase, Phase::Done);
        assert_eq!(s.report.as_ref(), Some(&report));
        // A late History of s1 for the same scan time stays out; s2's gets in.
        app.apply(Msg::History {
            id,
            scan: s1,
            taken_at_unix: report.taken_at_unix,
            result: Ok(None),
        });
        assert_eq!(app.session.as_ref().unwrap().history, History::Pending);
        app.apply(Msg::History {
            id,
            scan: s2,
            taken_at_unix: report.taken_at_unix,
            result: Ok(None),
        });
        assert_eq!(app.session.as_ref().unwrap().history, History::NoSnapshot);
    }

    #[test]
    fn replaced_run_of_the_same_entry_does_not_mask_the_new_runs_failures() {
        // s2's connect failure is accepted.
        let (mut app, id, s1, s2) = reconnected_same_entry();
        app.apply(Msg::ConnectFailed {
            id,
            scan: s2,
            error: "Connection refused".into(),
        });
        assert_eq!(
            app.session.as_ref().unwrap().phase,
            Phase::Failed("Connection refused".into())
        );
        assert_eq!(app.entry(id).unwrap().status, ConnStatus::Failed);

        // s2's scan failure is accepted, also after s1 sent its own.
        let (mut app, id, s1b, s2b) = reconnected_same_entry();
        assert_eq!((s1b, s2b), (s1, s2));
        app.apply(Msg::Connected {
            id,
            scan: s2,
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
        app.apply(Msg::ScanDone {
            id,
            scan: s1,
            report: Box::new(fixtures::report(true)),
        });
        app.apply(Msg::ScanFailed {
            id,
            scan: s2,
            error: "connection reset".into(),
        });
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.phase, Phase::Failed("connection reset".into()));
        assert!(s.report.is_none());
    }

    #[test]
    fn digits_switch_views_and_tab_switches_panels() {
        let mut app = App::new(fixtures::profiles());
        assert_eq!(app.screen(), Screen::Connections);
        for v in View::ALL.into_iter().rev() {
            app.handle_key(ch(v.digit()));
            assert_eq!(app.view, v);
        }
        // Analysis starts on TTL; tab and shift-tab cycle its panels, both ways round.
        app.handle_key(ch('2'));
        let mut seen = vec![app.screen()];
        for _ in 0..6 {
            app.handle_key(key(KeyCode::Tab));
            seen.push(app.screen());
        }
        assert_eq!(
            seen,
            [
                Screen::Ttl,
                Screen::Diff,
                Screen::HotKeys,
                Screen::BigKeys,
                Screen::Expiry,
                Screen::Nodes,
                Screen::Ttl
            ]
        );
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.screen(), Screen::Nodes);
        // Each view keeps its panel while you are elsewhere.
        app.handle_key(ch('3'));
        assert_eq!(app.screen(), Screen::Overview);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.screen(), Screen::Monitor);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.screen(), Screen::SlowLog);
        app.handle_key(ch('2'));
        assert_eq!(app.screen(), Screen::Nodes);
        app.handle_key(ch('3'));
        assert_eq!(app.screen(), Screen::SlowLog);
        // Monitor is Server's fourth panel, and tab wraps around after it.
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.screen(), Screen::Monitor);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.screen(), Screen::Overview);
        // Views without panels pass tab on to their state; on the rest it does nothing.
        for v in [View::Connections, View::Patterns, View::Streams] {
            app.handle_key(ch(v.digit()));
            let before = app.screen();
            app.handle_key(key(KeyCode::Tab));
            assert_eq!(app.screen(), before);
        }
    }

    #[test]
    fn quit_keys() {
        let mut app = App::new(vec![]);
        assert!(matches!(app.handle_key(ch('q'))[..], [Effect::Quit]));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(app.handle_key(ctrl_c)[..], [Effect::Quit]));
        // Typing q into the add input is text, but Ctrl-C still quits.
        app.handle_key(ch('a'));
        assert!(app.handle_key(ch('q')).is_empty());
        assert_eq!(app.add.as_ref().unwrap().input, "q");
        assert!(matches!(app.handle_key(ctrl_c)[..], [Effect::Quit]));
    }

    #[test]
    fn startup_discovers_docker() {
        let mut app = App::new(vec![]);
        assert!(matches!(app.init()[..], [Effect::DiscoverDocker]));
        app.apply(Msg::DockerFailed("Docker isn't running".into()));
        assert_eq!(app.docker, DockerState::Unavailable);
        assert!(matches!(
            app.handle_key(ch('r'))[..],
            [Effect::DiscoverDocker]
        ));
        // A second r while searching does nothing.
        assert!(app.handle_key(ch('r')).is_empty());
    }

    #[test]
    fn connections_navigation() {
        let mut app = App::new(fixtures::profiles());
        app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
        let ids: Vec<ConnSel> = app.selectable();
        assert_eq!(ids.len(), 5);
        assert_eq!(app.conn_sel, ids[0]);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ch('j'));
        assert_eq!(app.conn_sel, ids[2]);
        for _ in 0..10 {
            app.handle_key(ch('j'));
        }
        assert_eq!(app.conn_sel, ConnSel::Add);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.conn_sel, ids[3]);
        for _ in 0..10 {
            app.handle_key(ch('k'));
        }
        assert_eq!(app.conn_sel, ids[0]);
    }

    #[test]
    fn keychain_hint_appears_after_two_seconds_on_a_fake_clock() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut app = App::new(fixtures::profiles());
        app.apply(Msg::TickAt(t0));
        app.handle_key(ch('j')); // staging-cache: keychain
        let id = app.session.as_ref().map(|s| s.id);
        assert!(id.is_none());
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, .. }] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        let id = *id;
        app.apply(Msg::TickAt(at(1900)));
        assert_eq!(app.keychain_hint(), None);
        app.apply(Msg::TickAt(at(2000)));
        assert_eq!(
            app.keychain_hint(),
            None,
            "exactly 2 s is not yet more than 2 s"
        );
        app.apply(Msg::TickAt(at(2100)));
        assert_eq!(
            app.keychain_hint(),
            Some(
                "Waiting for the OS keychain. If a permission dialog is open, choose Always Allow."
            )
        );
        app.apply(Msg::Connected {
            id,
            scan: scan_of(&app),
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
        assert_eq!(app.keychain_hint(), None);
    }

    #[test]
    fn keychain_hint_needs_a_keychain_profile_and_a_clock() {
        let t0 = Instant::now();
        // No password profile: never a hint.
        let mut app = App::new(fixtures::profiles());
        app.apply(Msg::TickAt(t0));
        app.handle_key(ch('j'));
        app.handle_key(ch('j')); // local-6379: no auth
        app.handle_key(key(KeyCode::Enter));
        app.apply(Msg::TickAt(t0 + Duration::from_secs(30)));
        assert_eq!(app.keychain_hint(), None);
        // Connecting before the first clock reading: the timer starts at the first one.
        let mut app = App::new(fixtures::profiles());
        app.handle_key(ch('j'));
        app.handle_key(key(KeyCode::Enter));
        app.apply(Msg::TickAt(t0));
        app.apply(Msg::TickAt(t0 + Duration::from_secs(1)));
        assert_eq!(app.keychain_hint(), None);
        app.apply(Msg::TickAt(t0 + Duration::from_millis(2500)));
        assert!(app.keychain_hint().is_some());
        // A failed connect clears it.
        let id = app.session.as_ref().unwrap().id;
        app.apply(Msg::ConnectFailed {
            id,
            scan: scan_of(&app),
            error: "denied".into(),
        });
        assert_eq!(app.keychain_hint(), None);
    }

    #[test]
    fn connected_session_remembers_the_scan_target() {
        let mut app = App::new(fixtures::profiles());
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, .. }] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        let target = ScanTarget::PrimaryFallback("10.0.1.21:6379: timed out".into());
        app.apply(Msg::Connected {
            id: *id,
            scan: scan_of(&app),
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: target.clone(),
        });
        assert_eq!(app.session.as_ref().unwrap().target, Some(target.clone()));
        assert_eq!(app.entry(*id).unwrap().target, Some(target));
    }

    #[test]
    fn guessed_names_avoid_the_docker_prefix() {
        let app = App::new(fixtures::profiles());
        assert_eq!(app.unique_name("docker:cache"), "docker-cache");
        assert_eq!(app.unique_name("Docker:cache"), "docker-cache");
        assert_eq!(app.unique_name("DOCKER:orders"), "docker-orders");
        assert_eq!(app.unique_name("staging-cache"), "staging-cache-2");
    }

    #[test]
    fn enter_connects_saved_profile_with_keychain() {
        let mut app = App::new(fixtures::profiles());
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::Connect {
                id,
                scan,
                profile,
                secrets: SecretSource::Keychain,
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(*scan, scan_of(&app));
        assert_eq!(profile.name, "staging-cache");
        assert_eq!(app.view, View::Patterns);
        assert_eq!(app.entry(*id).unwrap().status, ConnStatus::Connecting);
        assert_eq!(app.session.as_ref().unwrap().phase, Phase::Connecting);

        app.apply(Msg::Connected {
            id: *id,
            scan: scan_of(&app),
            server: Some(fixtures::server()),
            perms: fixtures::perms_memory_denied(),
            target: ScanTarget::Primary,
        });
        assert_eq!(app.entry(*id).unwrap().status, ConnStatus::Limited);
        assert!(
            !app.entry(*id)
                .unwrap()
                .perms
                .as_ref()
                .unwrap()
                .allowed(PERM_MEMORY)
        );
        assert_eq!(app.session.as_ref().unwrap().phase, Phase::Scanning);

        app.apply(Msg::ScanDone {
            id: *id,
            scan: scan_of(&app),
            report: Box::new(fixtures::report(true)),
        });
        assert_eq!(app.session.as_ref().unwrap().phase, Phase::Done);
        assert!(!app.pattern_rows().is_empty());
    }

    #[test]
    fn enter_connects_docker_with_container_credentials() {
        let mut app = App::new(vec![]);
        let mut inst = fixtures::docker_instance();
        inst.credentials = Some(Credentials {
            username: None,
            password: SecretString::from("from-container"),
        });
        inst.profile.auth = Auth::Keychain;
        app.apply(Msg::DockerFound(vec![inst]));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::Connect {
                secrets: SecretSource::Container(Some(c)),
                profile,
                ..
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(c.password.expose_secret(), "from-container");
        assert_eq!(profile.name, "myapp-redis-1");
    }

    #[test]
    fn failed_connect_shows_error_and_never_crashes() {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let Effect::Connect { id, .. } = effects[0] else {
            panic!()
        };
        app.apply(Msg::ConnectFailed {
            id,
            scan: scan_of(&app),
            error: "Connection refused".into(),
        });
        assert_eq!(app.entry(id).unwrap().status, ConnStatus::Failed);
        assert!(app.notice.as_ref().unwrap().error);
        // Messages for a connection that is no longer active are ignored.
        app.apply(Msg::ScanDone {
            id: 999,
            scan: scan_of(&app),
            report: Box::new(fixtures::report(true)),
        });
        assert!(app.session.as_ref().unwrap().report.is_none());
        // Any key clears the error.
        app.handle_key(ch('j'));
        assert!(app.notice.is_none());
    }

    fn scanned_app(sizes: bool) -> App {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let Effect::Connect { id, .. } = effects[0] else {
            panic!()
        };
        app.apply(Msg::Connected {
            id,
            scan: scan_of(&app),
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(fixtures::report(sizes)),
        });
        app
    }

    fn selected_label(app: &App) -> String {
        let report = app.session.as_ref().unwrap().report.as_ref().unwrap();
        match app.pattern_rows()[app.patterns.sel] {
            Row::Group { root } => match &app.patterns.tree.roots[root] {
                Root::Group(g) => g.label.clone(),
                Root::Single(_) => unreachable!(),
            },
            Row::Pattern { idx, .. } => report.patterns[idx].pattern.clone(),
        }
    }

    #[test]
    fn patterns_navigation_and_folding() {
        let mut app = scanned_app(true);
        let all = app.pattern_rows().len();
        assert_eq!(selected_label(&app), "session:{uuid}");
        app.handle_key(ch('j'));
        assert_eq!(selected_label(&app), "cache:*");
        // Collapse the group: its children disappear.
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.pattern_rows().len(), all - 2);
        assert_eq!(selected_label(&app), "cache:*");
        // Expand, then → again moves into the first child.
        app.handle_key(ch('l'));
        assert_eq!(app.pattern_rows().len(), all);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(selected_label(&app), "cache:search:{hash}");
        // ← on a child goes back to the parent.
        app.handle_key(ch('h'));
        assert_eq!(selected_label(&app), "cache:*");
        // Enter toggles.
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.pattern_rows().len(), all - 2);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.pattern_rows().len(), all);
        // Movement stops at the ends.
        for _ in 0..50 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.patterns.sel, all - 1);
        for _ in 0..50 {
            app.handle_key(ch('k'));
        }
        assert_eq!(app.patterns.sel, 0);
    }

    #[test]
    fn filter_applies_and_clears() {
        let mut app = scanned_app(true);
        app.handle_key(ch('/'));
        assert!(app.patterns.filtering);
        type_str(&mut app, "cartx");
        app.handle_key(key(KeyCode::Backspace));
        // q is text while filtering.
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(!app.patterns.filtering);
        assert_eq!(app.patterns.filter, "cart");
        let labels: Vec<String> = (0..app.pattern_rows().len())
            .map(|i| {
                app.patterns.sel = i;
                selected_label(&app)
            })
            .collect();
        assert_eq!(labels, ["user:{id}:*", "user:{id}:cart"]);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.patterns.filter, "");
        // Esc while typing clears too.
        app.handle_key(ch('/'));
        type_str(&mut app, "zzz");
        assert!(app.pattern_rows().is_empty());
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.patterns.filtering);
        assert_eq!(app.patterns.filter, "");
    }

    fn pat(n: &str) -> RowId {
        RowId::Pattern(n.into())
    }

    fn grp(n: &str) -> RowId {
        RowId::Group(n.into())
    }

    /// The fixture partial results with `patterns` instead.
    fn partial_of(patterns: Vec<PatternStats>) -> Arc<PartialPatterns> {
        let mut p = (*fixtures::partial(true)).clone();
        p.patterns = patterns;
        Arc::new(p)
    }

    /// A pattern called `name` with `bytes` of memory.
    fn named(name: &str, bytes: u64) -> PatternStats {
        let mut p = fixtures::report(true).patterns[0].clone();
        p.pattern = name.into();
        p.est_bytes = Some(bytes);
        p
    }

    fn send_partial(app: &mut App, id: ConnId, partial: Arc<PartialPatterns>) {
        app.apply(Msg::ScanPartial {
            id,
            scan: scan_of(app),
            partial,
        });
    }

    /// Moves the Patterns selection down to `id`.
    fn select(app: &mut App, id: &RowId) {
        app.handle_key(ch('1'));
        for _ in 0..app.pattern_rows().len() {
            if app.selected_row_id().as_ref() == Some(id) {
                return;
            }
            app.handle_key(ch('j'));
        }
        panic!("no row {id:?}");
    }

    #[test]
    fn selection_follows_the_pattern_across_partials() {
        let (mut app, id) = scanning();
        send_partial(&mut app, id, fixtures::partial(true));
        select(&mut app, &pat("user:{id}:cart"));
        let before = app.patterns.sel;
        app.apply(Msg::Tick);
        app.apply(Msg::Tick);
        assert_eq!(app.sel_ticks(), 2);
        // The cart pattern grows past everything: its group moves to the top.
        let mut grown = (*fixtures::partial(true)).clone();
        let cart = grown
            .patterns
            .iter_mut()
            .find(|p| p.pattern == "user:{id}:cart")
            .unwrap();
        cart.est_bytes = Some(1 << 40);
        send_partial(&mut app, id, Arc::new(grown));
        assert_eq!(app.selected_row_id(), Some(pat("user:{id}:cart")));
        assert_ne!(app.patterns.sel, before);
        assert_eq!(app.patterns.sel, 1);
        // The same pattern at another index: the marquee does not restart.
        assert_eq!(app.sel_ticks(), 2);
    }

    #[test]
    fn selection_follows_the_pattern_into_the_finished_scan() {
        let (mut app, id) = scanning();
        // Partial results without the biggest pattern, so indices shift in the finished
        // scan: session:{uuid} comes first and user:{id}:feed joins its group.
        let mut p = (*fixtures::partial(true)).clone();
        p.patterns.retain(|p| p.pattern != "session:{uuid}");
        send_partial(&mut app, id, Arc::new(p));
        select(&mut app, &pat("tmp:import:{ulid}"));
        let before = app.patterns.sel;
        app.apply(Msg::Tick);
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(fixtures::report(true)),
        });
        assert_eq!(app.selected_row_id(), Some(pat("tmp:import:{ulid}")));
        assert_eq!(app.patterns.sel, before + 2);
        assert_eq!(app.sel_ticks(), 1);
        // A group too.
        let (mut app, id) = scanning();
        send_partial(&mut app, id, fixtures::partial(true));
        select(&mut app, &grp("user"));
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(fixtures::report(true)),
        });
        assert_eq!(app.selected_row_id(), Some(grp("user")));
    }

    #[test]
    fn a_finished_scan_without_partials_starts_at_the_top() {
        let app = scanned_app(true);
        assert_eq!(app.patterns.sel, 0);
        assert_eq!(app.selected_row_id(), Some(pat("session:{uuid}")));
    }

    #[test]
    fn selected_pattern_gone_keeps_the_index_clamped() {
        let (mut app, id) = scanning();
        send_partial(&mut app, id, fixtures::partial(true));
        select(&mut app, &pat("cache:product:{id}"));
        assert_eq!(app.patterns.sel, 3);
        let mut p = (*fixtures::partial(true)).clone();
        p.patterns.retain(|p| p.pattern != "cache:product:{id}");
        send_partial(&mut app, id, Arc::new(p));
        // cache:* dropped to the single cache:search:{hash}, so its group row and
        // cache:product:{id} are gone; index 3 is now user:{id}:cart.
        assert_eq!(app.patterns.sel, 3);
        assert_eq!(app.selected_row_id(), Some(pat("user:{id}:cart")));
        // Fewer rows than the index: the last row.
        send_partial(
            &mut app,
            id,
            partial_of(vec![named("a:{id}", 10), named("b:{id}", 5)]),
        );
        assert_eq!(app.patterns.sel, 1);
        assert_eq!(app.selected_row_id(), Some(pat("b:{id}")));
        // No rows: 0.
        send_partial(&mut app, id, partial_of(vec![]));
        assert_eq!(app.patterns.sel, 0);
        assert_eq!(app.selected_row_id(), None);
    }

    #[test]
    fn a_group_that_drops_to_a_single_keeps_the_selection() {
        let (mut app, id) = scanning();
        send_partial(&mut app, id, fixtures::partial(true));
        select(&mut app, &grp("cache"));
        let mut p = (*fixtures::partial(true)).clone();
        p.patterns.retain(|p| p.pattern != "cache:product:{id}");
        send_partial(&mut app, id, Arc::new(p));
        assert_eq!(app.selected_row_id(), Some(pat("cache:search:{hash}")));
    }

    /// The marquee restarts when a message, not a key, changes the selected row. Fails
    /// if `App::apply` stops calling `track_selection`.
    #[test]
    fn a_message_that_changes_the_selected_row_restarts_the_marquee() {
        let (mut app, id) = scanning();
        send_partial(&mut app, id, fixtures::partial(true));
        select(&mut app, &pat("rl:{ip}:{id}"));
        for _ in 0..7 {
            app.apply(Msg::Tick);
        }
        assert_eq!(app.sel_ticks(), 7);
        // The selected pattern is gone from the next partial: another one is selected.
        let mut p = (*fixtures::partial(true)).clone();
        p.patterns.retain(|p| p.pattern != "rl:{ip}:{id}");
        send_partial(&mut app, id, Arc::new(p));
        assert_ne!(app.selected_row_id(), Some(pat("rl:{ip}:{id}")));
        assert_eq!(app.sel_ticks(), 0);
        app.apply(Msg::Tick);
        assert_eq!(app.sel_ticks(), 1);
        // The scan fails: its partial results go, and with them the selection.
        app.apply(Msg::ScanFailed {
            id,
            scan: scan_of(&app),
            error: "connection reset".into(),
        });
        assert_eq!(app.selected_row_id(), None);
        assert_eq!(app.sel_ticks(), 0);
    }

    #[test]
    fn a_pattern_that_moves_into_a_collapsed_group_selects_the_group() {
        let (mut app, id) = scanning();
        send_partial(
            &mut app,
            id,
            partial_of(vec![
                named("a:{id}:x", 30),
                named("a:{id}:y", 20),
                named("b:{id}", 10),
            ]),
        );
        select(&mut app, &grp("a"));
        app.handle_key(key(KeyCode::Left));
        assert!(app.patterns.collapsed.contains("a"));
        // `a` has one pattern for now (a new run that has seen less), selected.
        send_partial(
            &mut app,
            id,
            partial_of(vec![named("a:{id}:z", 30), named("b:{id}", 10)]),
        );
        select(&mut app, &pat("a:{id}:z"));
        // It has a sibling again: the group is collapsed, so the group row is selected.
        send_partial(
            &mut app,
            id,
            partial_of(vec![
                named("a:{id}:x", 30),
                named("a:{id}:z", 20),
                named("b:{id}", 10),
            ]),
        );
        assert_eq!(app.pattern_rows().len(), 2);
        assert_eq!(app.selected_row_id(), Some(grp("a")));
    }

    #[test]
    fn collapse_survives_partials_and_scan_done() {
        let (mut app, id) = scanning();
        send_partial(&mut app, id, fixtures::partial(true));
        select(&mut app, &grp("cache"));
        app.handle_key(key(KeyCode::Left));
        let folded = app.pattern_rows().len();
        send_partial(&mut app, id, fixtures::partial(true));
        assert_eq!(app.pattern_rows().len(), folded);
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(fixtures::report(true)),
        });
        assert!(app.patterns.collapsed.contains("cache"));
        assert_eq!(app.selected_row_id(), Some(grp("cache")));
        // Expanding works on the new tree.
        app.handle_key(key(KeyCode::Right));
        assert!(!app.patterns.collapsed.contains("cache"));
    }

    #[test]
    fn a_collapsed_group_whose_key_changes_through_var_stays_collapsed() {
        let (mut app, id) = scanning();
        send_partial(
            &mut app,
            id,
            partial_of(vec![
                named("tenant.a1:cfg", 40),
                named("tenant.a1:flags", 30),
                named("lock:{var}", 1),
            ]),
        );
        select(&mut app, &grp("tenant.a1"));
        app.handle_key(key(KeyCode::Left));
        // Twenty tenants later the first part merged into {var}.
        send_partial(
            &mut app,
            id,
            partial_of(vec![
                named("tenant.{var}:cfg", 400),
                named("tenant.{var}:flags", 300),
                named("lock:{var}", 1),
            ]),
        );
        assert!(app.patterns.collapsed.contains("tenant.{var}"));
        assert_eq!(app.pattern_rows().len(), 2);
        assert_eq!(app.selected_row_id(), Some(grp("tenant.{var}")));
    }

    #[test]
    fn o_cycles_the_sort_and_keeps_the_selection() {
        let mut app = scanned_app(true);
        select(&mut app, &pat("user:{id}:cart"));
        let before = app.patterns.sel;
        app.apply(Msg::Tick);
        app.apply(Msg::Tick);
        let mut seen = Vec::new();
        for _ in 0..5 {
            app.handle_key(ch('o'));
            let n = app.notice.clone().unwrap();
            assert!(!n.error);
            seen.push((app.patterns.sort, n.text));
            assert_eq!(app.selected_row_id(), Some(pat("user:{id}:cart")));
            let d = app.pattern_data().unwrap();
            assert_eq!(
                app.patterns.tree,
                PatternTree::build_sorted(d.patterns, true, app.patterns.sort)
            );
        }
        assert_eq!(
            seen,
            [
                (SortBy::Keys, "Sorted by keys".to_string()),
                (SortBy::AvgSize, "Sorted by avg size".to_string()),
                (SortBy::NoTtl, "Sorted by no-TTL %".to_string()),
                (SortBy::Name, "Sorted by name".to_string()),
                (SortBy::Memory, "Sorted by memory".to_string()),
            ]
        );
        assert_eq!(app.patterns.sel, before);
        // Re-sorting moved the row around but it stayed selected: no marquee restart.
        assert_eq!(app.sel_ticks(), 2);
    }

    #[test]
    fn o_without_sizes_skips_memory_and_avg_size() {
        let mut app = scanned_app(false);
        app.handle_key(ch('1'));
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.handle_key(ch('o'));
            seen.push(app.notice.clone().unwrap().text);
        }
        assert_eq!(
            seen,
            [
                "Sorted by no-TTL %",
                "Sorted by name",
                "Sorted by keys",
                "Sorted by no-TTL %"
            ]
        );
    }

    #[test]
    fn sort_survives_partials_and_scan_done_but_not_a_new_connect() {
        let (mut app, id) = scanning();
        send_partial(&mut app, id, fixtures::partial(true));
        app.handle_key(ch('o'));
        assert_eq!(app.patterns.sort, SortBy::Keys);
        send_partial(&mut app, id, fixtures::partial(true));
        assert_eq!(app.patterns.sort, SortBy::Keys);
        let p = fixtures::partial(true);
        assert_eq!(
            app.patterns.tree,
            PatternTree::build_sorted(&p.patterns, true, SortBy::Keys)
        );
        let report = fixtures::report(true);
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(report.clone()),
        });
        assert_eq!(app.patterns.sort, SortBy::Keys);
        assert_eq!(
            app.patterns.tree,
            PatternTree::build_sorted(&report.patterns, true, SortBy::Keys)
        );
        // Connecting another entry starts again by memory.
        app.handle_key(ch('0'));
        app.handle_key(ch('j'));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.patterns.sort, SortBy::Memory);
    }

    #[test]
    fn o_before_any_results_only_changes_the_sort() {
        let (mut app, _) = scanning();
        app.handle_key(ch('1'));
        app.handle_key(ch('o'));
        assert_eq!(app.patterns.sort, SortBy::Keys);
        assert!(app.patterns.tree.roots.is_empty());
        assert_eq!(app.patterns.sel, 0);
    }

    /// Connected to prod-cache-01 with `perms`, and `report` finished scanning.
    fn scanned_with(report: ScanReport, perms: PermissionReport) -> (App, ConnId) {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let Effect::Connect { id, .. } = effects[0] else {
            panic!()
        };
        app.apply(Msg::Connected {
            id,
            scan: scan_of(&app),
            server: Some(report.server.clone()),
            perms,
            target: ScanTarget::Primary,
        });
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(report),
        });
        (app, id)
    }

    fn with_history(app: &mut App, id: ConnId) {
        let taken_at_unix = app.report().unwrap().taken_at_unix;
        app.apply(Msg::History {
            id,
            scan: scan_of(app),
            taken_at_unix,
            result: Ok(Some(fixtures::history(true))),
        });
    }

    #[test]
    fn snapshot_key_saves_the_report() {
        let mut app = scanned_app(true);
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(r)] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(r.profile, "prod-cache-01");
        assert_eq!(r.taken_at_unix, app.report().unwrap().taken_at_unix);
        // Works from the other report views too, but not on Connections.
        for v in ['2', '3', '4'] {
            go(&mut app, v);
            assert!(matches!(
                app.handle_key(ch('s'))[..],
                [Effect::SaveSnapshot(_)]
            ));
        }
        app.handle_key(ch('0'));
        assert!(app.handle_key(ch('s')).is_empty());

        app.apply(Msg::SnapshotSaved(PathBuf::from(
            "/data/redis-lens/snapshots/prod-cache-01/20261110T140211Z.json",
        )));
        let n = app.notice.clone().unwrap();
        assert_eq!(n.text, "Saved snapshot 20261110T140211Z.json");
        assert!(!n.error);
        for _ in 0..NOTICE_TICKS {
            app.apply(Msg::Tick);
        }
        assert!(app.notice.is_none());

        app.apply(Msg::SnapshotFailed(
            "could not find a data directory for this user".into(),
        ));
        let n = app.notice.clone().unwrap();
        assert!(n.error);
        assert_eq!(
            n.text,
            "Could not save snapshot: could not find a data directory for this user"
        );
    }

    #[test]
    fn snapshot_key_without_report_explains() {
        let mut app = App::new(fixtures::profiles());
        app.handle_key(ch('1'));
        assert!(app.handle_key(ch('s')).is_empty());
        assert!(!app.notice.as_ref().unwrap().error);
    }

    #[test]
    fn scan_done_loads_history_and_stores_the_diff() {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let Effect::Connect { id, .. } = effects[0] else {
            panic!()
        };
        let report = fixtures::report(true);
        let effects = app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(report.clone()),
        });
        let [
            Effect::LoadHistory {
                id: got,
                scan: got_scan,
                report: r,
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(*got, id);
        assert_eq!(*got_scan, scan_of(&app));
        assert_eq!(**r, report);
        assert_eq!(app.session.as_ref().unwrap().history, History::Pending);
        assert!(app.diff().is_none());

        // A reply for another scan is ignored.
        app.apply(Msg::History {
            id,
            scan: scan_of(&app),
            taken_at_unix: report.taken_at_unix - 1,
            result: Ok(Some(fixtures::history(true))),
        });
        assert!(app.diff().is_none());
        app.apply(Msg::History {
            id: id + 100,
            scan: scan_of(&app),
            taken_at_unix: report.taken_at_unix,
            result: Ok(Some(fixtures::history(true))),
        });
        assert!(app.diff().is_none());
        // The same entry and scan time, but another run of it.
        app.apply(Msg::History {
            id,
            scan: scan_of(&app) + 100,
            taken_at_unix: report.taken_at_unix,
            result: Ok(Some(fixtures::history(true))),
        });
        assert!(app.diff().is_none());

        app.apply(Msg::History {
            id,
            scan: scan_of(&app),
            taken_at_unix: report.taken_at_unix,
            result: Ok(Some(fixtures::history(true))),
        });
        assert_eq!(app.diff(), Some(&fixtures::history(true)));

        app.apply(Msg::History {
            id,
            scan: scan_of(&app),
            taken_at_unix: report.taken_at_unix,
            result: Ok(None),
        });
        assert_eq!(app.session.as_ref().unwrap().history, History::NoSnapshot);
        assert!(app.notice.is_none());

        // A missing data directory is reported and the app carries on.
        app.apply(Msg::History {
            id,
            scan: scan_of(&app),
            taken_at_unix: report.taken_at_unix,
            result: Err("could not find a data directory for this user".into()),
        });
        assert!(matches!(
            app.session.as_ref().unwrap().history,
            History::Failed(_)
        ));
        let n = app.notice.as_ref().unwrap();
        assert!(n.error);
        assert!(n.text.contains("could not find a data directory"));
        assert!(!app.pattern_rows().is_empty());
    }

    #[test]
    fn list_views_keep_their_own_selection() {
        let mut report = fixtures::report(true);
        report.hot_keys = fixtures::hot_keys();
        let (mut app, id) = scanned_with(report, fixtures::perms_ok());

        go(&mut app, '2');
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ch('j'));
        assert_eq!(app.sel.ttl, 2);

        // No diff yet: nothing to move through.
        go(&mut app, '3');
        app.handle_key(ch('j'));
        assert_eq!(app.sel.diff, 0);
        with_history(&mut app, id);
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.sel.diff, 3);

        go(&mut app, '4');
        app.handle_key(ch('j'));
        assert_eq!(app.sel.hot, 1);

        // Patterns has its own selection too.
        app.handle_key(ch('1'));
        assert_eq!(app.patterns.sel, 0);

        go(&mut app, '2');
        assert_eq!(app.sel.ttl, 2);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.sel.ttl, 1);
        for _ in 0..50 {
            app.handle_key(ch('k'));
        }
        assert_eq!(app.sel.ttl, 0);
        for _ in 0..50 {
            app.handle_key(ch('j'));
        }
        assert_eq!(app.sel.ttl, app.ttl_rows().len() - 1);
        app.handle_key(ch('g'));
        assert_eq!(app.sel.ttl, 0);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.sel.ttl, app.ttl_rows().len() - 1);

        go(&mut app, '3');
        assert_eq!(app.sel.diff, 3);
        app.handle_key(ch('G'));
        assert_eq!(app.sel.diff, fixtures::history(true).patterns.len() - 1);
        go(&mut app, '4');
        assert_eq!(app.sel.hot, 1);
        for _ in 0..50 {
            app.handle_key(ch('j'));
        }
        assert_eq!(app.sel.hot, fixtures::hot_keys().len() - 1);

        // A new scan starts every list at the top.
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(fixtures::report(true)),
        });
        assert_eq!(app.sel, ViewSel::default());
    }

    #[test]
    fn ttl_rows_rank_by_verdict_then_memory() {
        let (mut app, id) = scanned_with(fixtures::report(true), fixtures::perms_ok());
        let names = |app: &App| -> Vec<String> {
            let r = app.report().unwrap();
            app.ttl_rows()
                .iter()
                .map(|&i| r.patterns[i].pattern.clone())
                .collect()
        };
        // Without history nothing counts as growing.
        assert_eq!(
            names(&app)[..4],
            [
                "cache:search:{hash}",
                "user:{id}:cart",
                "tmp:import:{ulid}",
                "queue:emails"
            ]
        );
        with_history(&mut app, id);
        assert_eq!(
            names(&app)[..4],
            [
                "tmp:import:{ulid}",
                "queue:emails",
                "user:{id}:profile",
                "cache:search:{hash}"
            ]
        );
    }

    #[test]
    fn hot_keys_reasons() {
        let mut report = fixtures::report(true);
        report.hot_keys = fixtures::hot_keys();
        let (app, _) = scanned_with(report, fixtures::perms_ok());
        assert_eq!(app.hot_keys_reason(), None);

        let (app, _) = scanned_with(fixtures::report(true), fixtures::perms_freq_denied());
        assert_eq!(
            app.hot_keys_reason().as_deref(),
            Some("OBJECT FREQ was denied for this user.")
        );

        let (app, _) = scanned_with(fixtures::report_lru(), fixtures::perms_freq_skipped());
        assert_eq!(
            app.hot_keys_reason().as_deref(),
            Some(
                "The eviction policy is allkeys-lru, so Redis keeps no LFU counters. Set \
                 maxmemory-policy to allkeys-lfu or volatile-lfu to see hot keys."
            )
        );

        let (app, _) = scanned_with(fixtures::report(true), fixtures::perms_ok());
        assert_eq!(
            app.hot_keys_reason().as_deref(),
            Some("No sampled key has an LFU counter above 0 yet.")
        );

        let mut empty = fixtures::report(true);
        empty.sampled_keys = 0;
        empty.patterns.clear();
        let (app, _) = scanned_with(empty, fixtures::perms_ok());
        assert_eq!(
            app.hot_keys_reason().as_deref(),
            Some("The scan sampled no keys.")
        );

        // Nothing to explain before a scan.
        assert_eq!(App::new(vec![]).hot_keys_reason(), None);
    }

    #[test]
    fn add_flow_paste_and_save() {
        let mut app = App::new(fixtures::profiles());
        app.handle_key(ch('a'));
        assert_eq!(app.conn_sel, ConnSel::Add);
        app.handle_paste(format!("{}\n", fixtures::PROD_URL));
        let add = app.add.as_ref().unwrap();
        assert_eq!(add.input, fixtures::PROD_URL);
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::SaveConnection {
                profile,
                credentials: Some(c),
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(profile.name, "prod-cache-02");
        assert_eq!(profile.env, Env::Prod);
        assert_eq!(profile.endpoint.port, 6380);
        assert!(profile.endpoint.tls);
        assert_eq!(profile.auth, Auth::Keychain);
        assert_eq!(c.username.as_deref(), Some("lens-ro"));
        assert_eq!(c.password.expose_secret(), "Sup3rS3cret");
        assert!(app.add.as_ref().unwrap().saving);

        let profile = profile.clone();
        app.apply(Msg::Saved(profile));
        assert!(app.add.is_none());
        let sel = app.selected_entry().unwrap();
        assert_eq!(sel.profile.name, "prod-cache-02");
        assert!(sel.just_saved);
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .starts_with("Saved prod-cache-02")
        );
    }

    #[test]
    fn add_flow_typing_and_password_step() {
        let mut app = App::new(vec![]);
        // Enter on the add row opens the input.
        app.handle_key(key(KeyCode::Enter));
        assert!(app.add.is_some());
        type_str(&mut app, "redis://lens@stage-cache.internal:6379x");
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.add.as_ref().unwrap().password.as_deref(), Some(""));
        // An empty password is refused.
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.add.as_ref().unwrap().error.is_some());
        type_str(&mut app, "pw");
        app.handle_paste("1".into());
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::SaveConnection {
                profile,
                credentials: Some(c),
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(profile.name, "stage-cache");
        assert_eq!(profile.env, Env::Stage);
        assert_eq!(c.username.as_deref(), Some("lens"));
        assert_eq!(c.password.expose_secret(), "pw1");
        // The URL itself was not changed by the password step.
        assert_eq!(
            app.add.as_ref().unwrap().input,
            "redis://lens@stage-cache.internal:6379"
        );
    }

    #[test]
    fn add_flow_errors_cancel_and_unique_names() {
        let mut app = App::new(fixtures::profiles());
        app.handle_key(ch('a'));
        type_str(&mut app, "redis://");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.add.as_ref().unwrap().error.is_some());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.add.is_none());

        // A name that is already saved gets a suffix; no credentials means no auth.
        app.handle_key(ch('a'));
        app.handle_paste("staging-cache.internal:6379".into());
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::SaveConnection {
                profile,
                credentials: None,
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(profile.name, "staging-cache-2");
        assert_eq!(profile.auth, Auth::None);

        app.apply(Msg::SaveFailed(
            "could not write rediss://u:hunter2@h: denied".into(),
        ));
        let add = app.add.as_ref().unwrap();
        assert!(!add.saving);
        assert!(!add.error.as_ref().unwrap().contains("hunter2"));
    }

    #[test]
    fn paste_on_connections_starts_add() {
        let mut app = App::new(vec![]);
        app.handle_paste("localhost:6379".into());
        assert_eq!(app.add.as_ref().unwrap().input, "localhost:6379");
    }

    #[test]
    fn docker_rescan_keeps_state() {
        let mut app = App::new(vec![]);
        app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
        let effects = app.handle_key(key(KeyCode::Enter));
        let Effect::Connect { id, .. } = effects[0] else {
            panic!()
        };
        app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
        assert_eq!(app.entry(id).unwrap().status, ConnStatus::Connecting);
    }

    // -----------------------------------------------------------------------
    // Rescan (`r` on views 1-4)
    // -----------------------------------------------------------------------

    /// prod-cache-01 scanned (with hot keys) and its history loaded.
    fn finished_with_history() -> (App, ConnId, ScanReport) {
        let mut report = fixtures::report(true);
        report.hot_keys = fixtures::hot_keys();
        let (mut app, id) = scanned_with(report.clone(), fixtures::perms_ok());
        with_history(&mut app, id);
        (app, id, report)
    }

    /// Presses `r` and returns the new run, checking it is a Connect of the same entry
    /// with the entry's profile and secret source.
    fn press_rescan(app: &mut App, id: ConnId) -> ScanId {
        let old = scan_of(app);
        let effects = app.handle_key(ch('r'));
        let [
            Effect::Connect {
                id: got,
                scan,
                profile,
                secrets,
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(*got, id);
        assert_ne!(*scan, old);
        assert_eq!(*profile, app.entry(id).unwrap().profile);
        assert!(matches!(secrets, SecretSource::Keychain));
        *scan
    }

    fn connected(app: &mut App, id: ConnId, scan: ScanId) {
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Primary,
        });
    }

    #[test]
    fn rescan_keeps_the_view_the_patterns_state_and_the_old_results_until_the_first_partial() {
        let (mut app, id, report) = finished_with_history();
        let s1 = scan_of(&app);
        // Sort by keys, collapse the cache group, select the cart pattern.
        app.handle_key(ch('1'));
        app.handle_key(ch('o'));
        select(&mut app, &grp("cache"));
        app.handle_key(key(KeyCode::Left));
        select(&mut app, &pat("user:{id}:cart"));
        // Move the list views too.
        for (view, moves) in [('2', 2), ('3', 3), ('4', 1)] {
            go(&mut app, view);
            for _ in 0..moves {
                app.handle_key(ch('j'));
            }
        }
        let sel = ViewSel {
            ttl: 2,
            diff: 3,
            hot: 1,
            nodes: 0,
        };
        assert_eq!(app.sel, sel);
        app.handle_key(ch('1'));
        let patterns = format!("{:?}", app.patterns);

        let s2 = press_rescan(&mut app, id);
        // Same view, same Patterns state and selections, the old results still up.
        assert_eq!(app.view, View::Patterns);
        assert_eq!(format!("{:?}", app.patterns), patterns);
        assert_eq!(app.sel, sel);
        let s = app.session.as_ref().unwrap();
        assert_eq!((s.id, s.scan, s.phase.clone()), (id, s2, Phase::Connecting));
        assert_eq!(s.previous, Some(s1));
        assert!(s.rescan);
        assert_eq!(s.report.as_ref(), Some(&report));
        assert_eq!(app.diff(), Some(&fixtures::history(true)));
        assert_eq!(app.entry(id).unwrap().status, ConnStatus::Connecting);

        connected(&mut app, id, s2);
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.phase, Phase::Scanning);
        assert_eq!(s.report.as_ref(), Some(&report));
        assert_eq!(s.previous, Some(s1));
        assert_eq!(format!("{:?}", app.patterns), patterns);

        // The first partial replaces the old results; the selection follows its row.
        send_partial(&mut app, id, fixtures::partial(true));
        let s = app.session.as_ref().unwrap();
        assert!(s.report.is_none());
        assert!(s.partial.is_some());
        assert_eq!(s.previous, None);
        assert_eq!(s.history, History::Pending);
        assert!(app.pattern_data().unwrap().partial.is_some());
        assert_eq!(app.selected_row_id(), Some(pat("user:{id}:cart")));
        assert_eq!(app.patterns.sort, SortBy::Keys);
        assert!(app.patterns.collapsed.contains("cache"));
        // TTL, Diff and Hot keys wait for the finished scan again.
        assert!(app.ttl_rows().is_empty());
        assert!(app.diff().is_none());
        assert_eq!(app.sel, sel);

        // The finished scan: everything is kept, clamped to the new rows.
        let effects = app.apply(Msg::ScanDone {
            id,
            scan: s2,
            report: Box::new(report.clone()),
        });
        assert!(matches!(effects[..], [Effect::LoadHistory { scan, .. }] if scan == s2));
        assert_eq!(app.selected_row_id(), Some(pat("user:{id}:cart")));
        assert_eq!(app.sel, sel);
        app.apply(Msg::History {
            id,
            scan: s2,
            taken_at_unix: report.taken_at_unix,
            result: Ok(Some(fixtures::history(true))),
        });
        assert_eq!(app.sel, sel);
        assert_eq!(app.view, View::Patterns);
        assert_eq!(app.session.as_ref().unwrap().phase, Phase::Done);
    }

    #[test]
    fn rescan_without_partials_goes_from_the_old_report_to_the_new_one() {
        let (mut app, id, _) = finished_with_history();
        select(&mut app, &pat("user:{id}:cart"));
        go(&mut app, '4');
        for _ in 0..5 {
            app.handle_key(ch('j'));
        }
        go(&mut app, '3');
        for _ in 0..6 {
            app.handle_key(ch('j'));
        }
        app.handle_key(ch('1'));
        let s2 = press_rescan(&mut app, id);
        connected(&mut app, id, s2);
        // A smaller result: fewer hot keys, the cart pattern moved.
        let mut report = fixtures::report(true);
        report.hot_keys = fixtures::hot_keys()[..2].to_vec();
        report.taken_at_unix += 60;
        report
            .patterns
            .iter_mut()
            .find(|p| p.pattern == "user:{id}:cart")
            .unwrap()
            .est_bytes = Some(1 << 40);
        app.apply(Msg::ScanDone {
            id,
            scan: s2,
            report: Box::new(report.clone()),
        });
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.previous, None);
        assert_eq!(s.report.as_ref(), Some(&report));
        assert_eq!(s.history, History::Pending);
        assert_eq!(app.selected_row_id(), Some(pat("user:{id}:cart")));
        // Clamped to the new rows.
        assert_eq!(app.sel.hot, 1);
        assert_eq!(app.sel.diff, 6);
        let mut small = fixtures::history(true);
        small.patterns.truncate(2);
        app.apply(Msg::History {
            id,
            scan: s2,
            taken_at_unix: report.taken_at_unix,
            result: Ok(Some(small)),
        });
        assert_eq!(app.sel.diff, 1);
    }

    #[test]
    fn a_failed_rescan_keeps_the_previous_results_marked_until_partials_replaced_them() {
        // The connect fails: the old report stays, still marked as previous.
        let (mut app, id, report) = finished_with_history();
        let s1 = scan_of(&app);
        let s2 = press_rescan(&mut app, id);
        app.apply(Msg::ConnectFailed {
            id,
            scan: s2,
            error: "Connection refused".into(),
        });
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.phase, Phase::Failed("Connection refused".into()));
        assert_eq!(s.report.as_ref(), Some(&report));
        assert_eq!(s.previous, Some(s1));
        assert!(!app.pattern_rows().is_empty());
        let n = app.notice.clone().unwrap();
        assert!(n.error);
        assert_eq!(n.text, "Could not connect: Connection refused");
        assert_eq!(app.entry(id).unwrap().status, ConnStatus::Failed);
        // `r` again is allowed and keeps the same previous run.
        app.handle_key(ch('x'));
        let s3 = press_rescan(&mut app, id);
        assert_eq!(app.session.as_ref().unwrap().previous, Some(s1));

        // The scan fails before any partial: the same.
        connected(&mut app, id, s3);
        app.apply(Msg::ScanFailed {
            id,
            scan: s3,
            error: "connection reset".into(),
        });
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.phase, Phase::Failed("connection reset".into()));
        assert_eq!(s.report.as_ref(), Some(&report));
        assert_eq!(s.previous, Some(s1));
        assert_eq!(
            app.notice.as_ref().unwrap().text,
            "Scan failed: connection reset"
        );

        // After partials replaced them, as any failed scan: nothing left.
        let (mut app, id, _) = finished_with_history();
        let s2 = press_rescan(&mut app, id);
        connected(&mut app, id, s2);
        send_partial(&mut app, id, fixtures::partial(true));
        app.apply(Msg::ScanFailed {
            id,
            scan: s2,
            error: "connection reset".into(),
        });
        let s = app.session.as_ref().unwrap();
        assert!(s.report.is_none() && s.partial.is_none() && s.previous.is_none());
        assert!(app.pattern_data().is_none());
        assert!(app.patterns.tree.roots.is_empty());
        assert_eq!(app.patterns.sel, 0);
    }

    #[test]
    fn rescan_ignores_the_replaced_runs_messages() {
        let (mut app, id, report) = finished_with_history();
        let s1 = scan_of(&app);
        let s2 = press_rescan(&mut app, id);
        let check = |app: &App, phase: Phase| {
            let s = app.session.as_ref().unwrap();
            assert_eq!((s.id, s.scan, s.phase.clone()), (id, s2, phase));
            assert_eq!(s.previous, Some(s1));
            assert_eq!(s.report.as_ref(), Some(&report));
            assert!(s.partial.is_none() && s.progress.is_none());
            assert_eq!(app.diff(), Some(&fixtures::history(true)));
            assert!(app.notice.is_none(), "{:?}", app.notice);
        };
        // s1 messages other than History change nothing, before and after s2 connects.
        let stale = || {
            stale_messages(id, s1)
                .into_iter()
                .filter(|m| !matches!(m, Msg::History { .. }))
        };
        for msg in stale() {
            let what = format!("{msg:?}");
            assert!(app.apply(msg).is_empty(), "{what}");
            check(&app, Phase::Connecting);
        }
        connected(&mut app, id, s2);
        for msg in stale() {
            let what = format!("{msg:?}");
            assert!(app.apply(msg).is_empty(), "{what}");
            check(&app, Phase::Scanning);
        }
        // Another entry's partial does not replace the old results either.
        app.apply(Msg::ScanPartial {
            id: id + 100,
            scan: s2,
            partial: fixtures::partial(true),
        });
        check(&app, Phase::Scanning);
        // s1's History is the kept report's own, so it applies to it.
        app.apply(Msg::History {
            id,
            scan: s1,
            taken_at_unix: report.taken_at_unix,
            result: Ok(None),
        });
        assert_eq!(app.session.as_ref().unwrap().history, History::NoSnapshot);
        // Once s2's partials replaced it, s1's History stays out.
        send_partial(&mut app, id, fixtures::partial(true));
        app.apply(Msg::History {
            id,
            scan: s1,
            taken_at_unix: report.taken_at_unix,
            result: Ok(Some(fixtures::history(true))),
        });
        assert_eq!(app.session.as_ref().unwrap().history, History::Pending);
        // And s1's partials stay out too.
        let before = app.session.as_ref().unwrap().partial.clone();
        app.apply(Msg::ScanPartial {
            id,
            scan: s1,
            partial: fixtures::partial(false),
        });
        assert_eq!(app.session.as_ref().unwrap().partial, before);
    }

    fn trend_msg(id: ConnId, scan: ScanId, taken_at_unix: i64) -> Msg {
        Msg::Trend {
            id,
            scan,
            taken_at_unix,
            trend: Some(fixtures::trend(true)),
        }
    }

    #[test]
    fn trend_is_kept_for_its_own_scan_only() {
        let (mut app, id, report) = finished_with_history();
        let s1 = scan_of(&app);
        assert!(app.trend().is_none());
        // Another entry, another run, or another scan time: ignored.
        for msg in [
            trend_msg(id + 100, s1, report.taken_at_unix),
            trend_msg(id, s1 + 100, report.taken_at_unix),
            trend_msg(id, s1, report.taken_at_unix - 1),
        ] {
            assert!(app.apply(msg).is_empty());
            assert!(app.session.as_ref().unwrap().trend.is_none());
        }
        app.apply(trend_msg(id, s1, report.taken_at_unix));
        assert_eq!(app.trend(), Some(&fixtures::trend(true)));
        // A trend too short to show is stored but not offered to the view.
        app.apply(Msg::Trend {
            id,
            scan: s1,
            taken_at_unix: report.taken_at_unix,
            trend: Some(fixtures::short_trend(true)),
        });
        assert!(app.session.as_ref().unwrap().trend.is_some());
        assert!(app.trend().is_none());
        // Unreadable snapshots: no trend.
        app.apply(Msg::Trend {
            id,
            scan: s1,
            taken_at_unix: report.taken_at_unix,
            trend: None,
        });
        assert!(app.session.as_ref().unwrap().trend.is_none());
    }

    #[test]
    fn replaced_run_of_the_same_entry_sends_no_trend() {
        let (mut app, id, s1, s2) = reconnected_same_entry();
        let at = fixtures::report(true).taken_at_unix;
        assert!(app.apply(trend_msg(id, s1, at)).is_empty());
        assert!(app.session.as_ref().unwrap().trend.is_none());
        connected(&mut app, id, s2);
        assert!(app.apply(trend_msg(id, s1, at)).is_empty());
        assert!(app.session.as_ref().unwrap().trend.is_none());
        app.apply(Msg::ScanDone {
            id,
            scan: s2,
            report: Box::new(fixtures::report(true)),
        });
        // A late trend of s1 for the same scan time stays out; s2's gets in.
        app.apply(trend_msg(id, s1, at));
        assert!(app.session.as_ref().unwrap().trend.is_none());
        app.apply(trend_msg(id, s2, at));
        assert_eq!(app.trend(), Some(&fixtures::trend(true)));
    }

    #[test]
    fn rescan_keeps_the_trend_with_the_old_report_until_it_is_replaced() {
        let (mut app, id, report) = finished_with_history();
        let s1 = scan_of(&app);
        app.apply(trend_msg(id, s1, report.taken_at_unix));
        let s2 = press_rescan(&mut app, id);
        connected(&mut app, id, s2);
        // The kept report keeps its trend, and s1's trend still applies to it.
        assert_eq!(app.trend(), Some(&fixtures::trend(true)));
        app.apply(Msg::Trend {
            id,
            scan: s1,
            taken_at_unix: report.taken_at_unix,
            trend: None,
        });
        assert!(app.trend().is_none());
        app.apply(trend_msg(id, s1, report.taken_at_unix));
        assert!(app.trend().is_some());
        // The first partial replaces the old report, and its trend goes with it.
        send_partial(&mut app, id, fixtures::partial(true));
        assert!(app.session.as_ref().unwrap().trend.is_none());
        app.apply(trend_msg(id, s1, report.taken_at_unix));
        assert!(app.session.as_ref().unwrap().trend.is_none());
        // The new report starts without one until its own arrives.
        app.apply(Msg::ScanDone {
            id,
            scan: s2,
            report: Box::new(report.clone()),
        });
        assert!(app.session.as_ref().unwrap().trend.is_none());
        app.apply(trend_msg(id, s2, report.taken_at_unix));
        assert!(app.trend().is_some());
    }

    #[test]
    fn rescan_without_partials_drops_the_old_trend() {
        let (mut app, id, report) = finished_with_history();
        let s1 = scan_of(&app);
        app.apply(trend_msg(id, s1, report.taken_at_unix));
        let s2 = press_rescan(&mut app, id);
        connected(&mut app, id, s2);
        let mut newer = report.clone();
        newer.taken_at_unix += 60;
        app.apply(Msg::ScanDone {
            id,
            scan: s2,
            report: Box::new(newer.clone()),
        });
        assert!(app.session.as_ref().unwrap().trend.is_none());
        // s1's trend was for the old report's time.
        app.apply(trend_msg(id, s1, report.taken_at_unix));
        assert!(app.session.as_ref().unwrap().trend.is_none());
        app.apply(trend_msg(id, s2, newer.taken_at_unix));
        assert!(app.trend().is_some());
    }

    #[test]
    fn a_partial_before_the_rescan_is_connected_is_ignored() {
        let (mut app, id, report) = finished_with_history();
        let s2 = press_rescan(&mut app, id);
        send_partial(&mut app, id, fixtures::partial(true));
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.scan, s2);
        assert_eq!(s.report.as_ref(), Some(&report));
        assert!(s.partial.is_none());
    }

    #[test]
    fn connecting_during_a_rescan_starts_afresh() {
        let (mut app, id, _) = finished_with_history();
        let s1 = scan_of(&app);
        app.handle_key(ch('o'));
        for _ in 0..NOTICE_TICKS {
            app.apply(Msg::Tick);
        }
        let s2 = press_rescan(&mut app, id);
        app.handle_key(ch('0'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { scan: s3, .. }] = effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.scan, s3);
        assert!(s.report.is_none() && s.previous.is_none() && !s.rescan);
        assert_eq!(app.patterns.sort, SortBy::Memory);
        // Neither earlier run gets in.
        for msg in stale_messages(id, s1)
            .into_iter()
            .chain(stale_messages(id, s2))
        {
            let what = format!("{msg:?}");
            assert!(app.apply(msg).is_empty(), "{what}");
            assert_untouched(&app, id, s3, Phase::Connecting);
        }
    }

    #[test]
    fn r_needs_a_finished_or_failed_scan() {
        // No session: connect first.
        let mut app = App::new(fixtures::profiles());
        for view in ['1', '2', '3', '4'] {
            go(&mut app, view);
            assert!(app.handle_key(ch('r')).is_empty());
            let n = app.notice.clone().unwrap();
            assert!(!n.error);
            assert!(n.text.contains("connect first"), "{}", n.text);
        }
        // Connecting, then scanning: already running.
        let mut app = App::new(fixtures::profiles());
        app.handle_key(key(KeyCode::Enter));
        let scan = scan_of(&app);
        for view in ['1', '2', '3', '4'] {
            go(&mut app, view);
            assert!(app.handle_key(ch('r')).is_empty());
            assert_eq!(
                app.notice.clone().unwrap().text,
                "A scan is already running."
            );
            assert_eq!(scan_of(&app), scan);
        }
        let (mut app, _) = scanning();
        assert!(app.handle_key(ch('r')).is_empty());
        assert_eq!(
            app.notice.clone().unwrap().text,
            "A scan is already running."
        );
        // A rescan that is running counts too.
        let (mut app, id, _) = finished_with_history();
        let s2 = press_rescan(&mut app, id);
        assert!(app.handle_key(ch('r')).is_empty());
        assert_eq!(scan_of(&app), s2);
        // Finished: works on every report view.
        let (mut app, id, _) = finished_with_history();
        for view in ['1', '2', '3', '4'] {
            go(&mut app, view);
            let s = press_rescan(&mut app, id);
            connected(&mut app, id, s);
            app.apply(Msg::ScanDone {
                id,
                scan: s,
                report: Box::new(fixtures::report(true)),
            });
        }
        // On Connections `r` still looks for Docker containers.
        app.apply(Msg::DockerFound(Vec::new()));
        app.handle_key(ch('0'));
        assert!(matches!(
            app.handle_key(ch('r'))[..],
            [Effect::DiscoverDocker]
        ));
        // In the filter input it is text.
        app.handle_key(ch('1'));
        app.handle_key(ch('/'));
        assert!(app.handle_key(ch('r')).is_empty());
        assert_eq!(app.patterns.filter, "r");
    }

    #[test]
    fn rescan_of_a_docker_container_uses_its_credentials_again() {
        let mut app = App::new(vec![]);
        let mut inst = fixtures::docker_instance();
        inst.credentials = Some(Credentials {
            username: None,
            password: SecretString::from("from-container"),
        });
        app.apply(Msg::DockerFound(vec![inst]));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, scan, .. }] = effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        app.apply(Msg::ConnectFailed {
            id,
            scan,
            error: "refused".into(),
        });
        let effects = app.handle_key(ch('r'));
        let [
            Effect::Connect {
                secrets: SecretSource::Container(Some(c)),
                profile,
                ..
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(c.password.expose_secret(), "from-container");
        assert_eq!(profile.name, "myapp-redis-1");
        // Nothing was on screen, so nothing is kept as previous.
        assert_eq!(app.session.as_ref().unwrap().previous, None);
    }

    #[test]
    fn rescan_of_an_entry_that_is_gone_explains() {
        let mut app = App::new(vec![]);
        app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, scan, .. }] = effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        app.apply(Msg::ScanDone {
            id,
            scan,
            report: Box::new(fixtures::report(true)),
        });
        app.apply(Msg::DockerFound(Vec::new()));
        assert!(app.handle_key(ch('r')).is_empty());
        assert!(
            app.notice
                .clone()
                .unwrap()
                .text
                .contains("no longer listed")
        );
        assert_eq!(scan_of(&app), scan);
    }

    #[test]
    fn s_during_a_rescan_waits_for_it() {
        let (mut app, id, report) = finished_with_history();
        let s2 = press_rescan(&mut app, id);
        let wait = "A scan is running. Press s again when it finishes to save it.";
        for view in ['1', '2', '3', '4'] {
            go(&mut app, view);
            assert!(app.handle_key(ch('s')).is_empty(), "view {view}");
            assert_eq!(app.notice.clone().unwrap().text, wait);
        }
        connected(&mut app, id, s2);
        assert!(app.handle_key(ch('s')).is_empty());
        assert_eq!(app.notice.clone().unwrap().text, wait);
        send_partial(&mut app, id, fixtures::partial(true));
        assert!(app.handle_key(ch('s')).is_empty());
        assert!(
            app.notice
                .clone()
                .unwrap()
                .text
                .starts_with("These are partial results")
        );
        let mut new = report.clone();
        new.taken_at_unix += 60;
        app.apply(Msg::ScanDone {
            id,
            scan: s2,
            report: Box::new(new.clone()),
        });
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(r)] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(**r, new);

        // A rescan that failed before any results: the previous report on screen can
        // be saved (the same file as before, it is that report's own snapshot).
        let (mut app, id, report) = finished_with_history();
        let s2 = press_rescan(&mut app, id);
        app.apply(Msg::ConnectFailed {
            id,
            scan: s2,
            error: "refused".into(),
        });
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(r)] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(**r, report);
    }

    // -----------------------------------------------------------------------
    // Help overlay
    // -----------------------------------------------------------------------

    #[test]
    fn help_opens_on_every_view_and_takes_every_key() {
        let (mut app, _, _) = finished_with_history();
        for screen in Screen::ALL {
            show(&mut app, screen);
            let shown = app.screen();
            assert!(app.handle_key(ch('?')).is_empty());
            assert!(app.help);
            let before = fingerprint(&app);
            for k in [
                ch('j'),
                ch('k'),
                key(KeyCode::Down),
                key(KeyCode::PageDown),
                key(KeyCode::End),
                key(KeyCode::Enter),
                key(KeyCode::Left),
                ch('1'),
                ch('a'),
                ch('s'),
                ch('r'),
                ch('y'),
                ch('Y'),
                ch('o'),
                ch('/'),
                ch('G'),
                key(KeyCode::Tab),
                key(KeyCode::BackTab),
                ch('p'),
                ch('n'),
            ] {
                assert!(app.handle_key(k).is_empty(), "{k:?}");
                assert_eq!(fingerprint(&app), before, "{k:?}");
            }
            app.handle_paste("redis://localhost:6379".into());
            assert_eq!(fingerprint(&app), before);
            assert!(matches!(app.handle_key(ch('q'))[..], [Effect::Quit]));
            assert!(matches!(
                app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))[..],
                [Effect::Quit]
            ));
            assert!(app.help);
            // Esc closes; so does ? again.
            app.handle_key(key(KeyCode::Esc));
            assert!(!app.help);
            app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
            assert!(app.help);
            app.handle_key(ch('?'));
            assert!(!app.help);
            assert_eq!(app.screen(), shown);
        }
    }

    #[test]
    fn question_mark_is_text_in_the_add_flow_and_the_filter() {
        let (mut app, _, _) = finished_with_history();
        app.handle_key(ch('/'));
        app.handle_key(ch('?'));
        assert!(!app.help);
        assert_eq!(app.patterns.filter, "?");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ch('a'));
        app.handle_key(ch('?'));
        assert!(!app.help);
        assert_eq!(app.add.as_ref().unwrap().input, "?");
    }

    // -----------------------------------------------------------------------
    // Copy (`y`, `Y`)
    // -----------------------------------------------------------------------

    /// The copy effects of pressing `k`, with the notice it left.
    fn copies(app: &mut App, k: char) -> (Vec<CopyText>, String) {
        let texts = app
            .handle_key(ch(k))
            .into_iter()
            .map(|e| match e {
                Effect::Copy(t) => t,
                other => panic!("unexpected effect {other:?}"),
            })
            .collect();
        (
            texts,
            app.notice.clone().map(|n| n.text).unwrap_or_default(),
        )
    }

    /// `Y` offers no command for a pattern with shell-unsafe characters, and `y` never
    /// copies text with control characters (a pasted newline would run what follows).
    #[test]
    fn unsafe_patterns_and_control_characters_are_not_copied() {
        let mut report = fixtures::report(true);
        report.patterns[0].pattern = "x&calc&".into();
        report.patterns[1].pattern = "nl:\nrm -rf x".into();
        let names = [
            report.patterns[0].pattern.clone(),
            report.patterns[1].pattern.clone(),
        ];
        let (mut app, _) = scanned_with(report, fixtures::perms_ok());
        app.handle_key(ch('1'));
        let mut saw = [false, false];
        for _ in 0..app.pattern_rows().len() {
            let label = selected_label(&app);
            if label == names[0] {
                saw[0] = true;
                let (texts, note) = copies(&mut app, 'Y');
                assert!(texts.is_empty());
                assert!(note.starts_with("No command to copy"), "{note}");
                // The pattern itself has no control characters, so `y` copies it.
                assert_eq!(copies(&mut app, 'y').0, [CopyText::pattern("x&calc&")]);
            }
            if label == names[1] {
                saw[1] = true;
                let (texts, note) = copies(&mut app, 'y');
                assert!(texts.is_empty());
                assert_eq!(
                    note,
                    "This pattern has control characters, so it is not copied."
                );
                assert!(copies(&mut app, 'Y').0.is_empty());
            }
            app.handle_key(ch('j'));
        }
        assert_eq!(saw, [true, true]);
    }

    #[test]
    fn y_and_shift_y_copy_the_selected_row_on_every_report_view() {
        let (mut app, _, report) = finished_with_history();
        let note =
            |what: &str| format!("Copied {what} to the clipboard (needs a terminal with OSC 52)");
        // Patterns: a single pattern, a group row (its label), a child.
        app.handle_key(ch('1'));
        assert_eq!(
            copies(&mut app, 'y'),
            (vec![CopyText::pattern("session:{uuid}")], note("pattern"))
        );
        app.handle_key(ch('j'));
        assert_eq!(selected_label(&app), "cache:*");
        assert_eq!(copies(&mut app, 'y').0, [CopyText::pattern("cache:*")]);
        let (cmd, text) = copies(&mut app, 'Y');
        assert_eq!(cmd, [CopyText::suggested_command("cache:*").unwrap()]);
        assert_eq!(cmd[0].as_str(), "redis-cli --scan --pattern \"cache:*\"");
        assert_eq!(text, note("command"));
        app.handle_key(ch('j'));
        assert_eq!(
            copies(&mut app, 'y').0,
            [CopyText::pattern("cache:search:{hash}")]
        );
        // TTL: the selected row's pattern.
        go(&mut app, '2');
        app.handle_key(ch('j'));
        let ttl = report.patterns[app.ttl_rows()[1]].pattern.clone();
        assert_eq!(copies(&mut app, 'y').0, [CopyText::pattern(&ttl)]);
        assert_eq!(
            copies(&mut app, 'Y').0,
            [CopyText::suggested_command(&ttl).unwrap()]
        );
        // Diff: the selected row's pattern.
        go(&mut app, '3');
        app.handle_key(ch('j'));
        let diff = fixtures::history(true).patterns[1].pattern.clone();
        assert_eq!(copies(&mut app, 'y').0, [CopyText::pattern(&diff)]);
        assert_eq!(
            copies(&mut app, 'Y').0,
            [CopyText::suggested_command(&diff).unwrap()]
        );
        // Hot keys: the key name, and the command for its pattern.
        go(&mut app, '4');
        app.handle_key(ch('j'));
        let (k, text) = copies(&mut app, 'y');
        assert_eq!(k, [CopyText::key("queue:emails")]);
        assert_eq!(text, note("key"));
        let (cmd, _) = copies(&mut app, 'Y');
        assert_eq!(
            cmd,
            [CopyText::suggested_command(&pattern_for_key("queue:emails")).unwrap()]
        );
        app.handle_key(ch('j'));
        let (cmd, _) = copies(&mut app, 'Y');
        assert_eq!(
            cmd[0].as_str(),
            "redis-cli --scan --pattern \"session:*\"",
            "the key's pattern, not the key"
        );
        // Partial results can be copied too: they are on screen.
        let (mut app, id) = scanning();
        send_partial(&mut app, id, fixtures::partial(true));
        assert_eq!(
            copies(&mut app, 'y').0,
            [CopyText::pattern("session:{uuid}")]
        );
    }

    #[test]
    fn copy_with_nothing_selected_explains() {
        let nothing = "Nothing selected to copy.";
        let mut app = App::new(fixtures::profiles());
        for view in ['1', '2', '3', '4'] {
            go(&mut app, view);
            for k in ['y', 'Y'] {
                assert_eq!(copies(&mut app, k), (vec![], nothing.to_string()));
            }
        }
        // A report without hot keys, a diff still loading, a filter matching nothing.
        let (mut app, _) = scanned_with(fixtures::report(true), fixtures::perms_ok());
        for view in ['3', '4'] {
            go(&mut app, view);
            assert_eq!(copies(&mut app, 'y'), (vec![], nothing.to_string()));
        }
        app.handle_key(ch('1'));
        app.handle_key(ch('/'));
        type_str(&mut app, "zzz");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(copies(&mut app, 'Y'), (vec![], nothing.to_string()));
    }

    #[test]
    fn copy_of_text_over_the_osc52_cap_is_refused() {
        let mut report = fixtures::report(true);
        report.hot_keys = fixtures::hot_keys();
        report.hot_keys[0].key = format!("blob:{}", "7".repeat(clipboard::MAX_OSC52_BYTES));
        let (mut app, _) = scanned_with(report, fixtures::perms_ok());
        go(&mut app, '4');
        let (texts, notice) = copies(&mut app, 'y');
        assert!(texts.is_empty());
        assert!(notice.contains("too long to copy"), "{notice}");
        // Its pattern is short, so the command can be copied.
        assert_eq!(copies(&mut app, 'Y').0.len(), 1);
    }

    #[test]
    fn y_is_unbound_on_connections_and_text_in_inputs() {
        let (mut app, _, _) = finished_with_history();
        app.handle_key(ch('0'));
        let before = fingerprint(&app);
        for k in ['y', 'Y'] {
            assert!(app.handle_key(ch(k)).is_empty());
        }
        assert_eq!(fingerprint(&app), before);
        app.handle_key(ch('1'));
        app.handle_key(ch('/'));
        assert!(app.handle_key(ch('y')).is_empty());
        assert!(app.handle_key(ch('Y')).is_empty());
        assert_eq!(app.patterns.filter, "yY");
    }

    #[test]
    fn a_failed_copy_is_an_error_notice() {
        let mut app = App::new(vec![]);
        app.apply(Msg::CopyFailed("Broken pipe (os error 32)".into()));
        let n = app.notice.clone().unwrap();
        assert!(n.error);
        assert_eq!(n.text, "Could not copy: Broken pipe (os error 32)");
    }

    /// CLAUDE.md rule 1: nothing `y` or `Y` copies holds a username, a password or a
    /// connection string, on any view and row, for a saved profile and for a Docker
    /// container whose credentials the app holds; Connections and the add flow never
    /// copy anything.
    #[test]
    fn copy_never_carries_credentials() {
        use crate::secrets::{MemoryStore, SecretStore};
        const USER: &str = "lens-ro";
        const PASSWORD: &str = "Sup3rS3cret";
        let forbidden = [
            USER,
            PASSWORD,
            fixtures::PROD_URL,
            "from-container",
            "Fixture-Container-Pw",
        ];
        let check = |t: &CopyText| {
            for f in forbidden {
                assert!(!t.as_str().contains(f), "{f:?} in {:?}", t.as_str());
            }
        };
        let creds = || Credentials {
            username: Some(USER.into()),
            password: SecretString::from(PASSWORD),
        };
        // The saved profile's credentials live in a (memory) keychain.
        let profiles = fixtures::profiles();
        let store = MemoryStore::default();
        store.set(&profiles[0].secret_ref(), &creds()).unwrap();
        // A container whose credentials the app holds in memory.
        let mut inst = fixtures::docker_instance();
        inst.credentials = Some(Credentials {
            username: Some(USER.into()),
            password: SecretString::from("Fixture-Container-Pw"),
        });
        inst.auth = DiscoveredAuth::Password;
        let mut report = fixtures::report(true);
        report.hot_keys = fixtures::hot_keys();

        let mut copied = 0;
        for docker in [false, true] {
            let mut app = App::new(profiles.clone());
            app.apply(Msg::DockerFound(vec![inst.clone()]));
            if docker {
                for _ in 0..3 {
                    app.handle_key(ch('j'));
                }
            }
            let effects = app.handle_key(key(KeyCode::Enter));
            let [Effect::Connect { id, scan, .. }] = effects[..] else {
                panic!("unexpected effects {effects:?}");
            };
            assert_eq!(app.entry(id).unwrap().is_docker(), docker);
            connected(&mut app, id, scan);
            app.apply(Msg::ScanDone {
                id,
                scan,
                report: Box::new(report.clone()),
            });
            with_history(&mut app, id);
            for view in LEGACY {
                show(&mut app, view);
                // Every row of the view, with groups expanded and then collapsed.
                for fold in [None, Some(KeyCode::Left)] {
                    app.handle_key(ch('g'));
                    for _ in 0..40 {
                        if let Some(f) = fold {
                            app.handle_key(key(f));
                        }
                        for k in ['y', 'Y'] {
                            for e in app.handle_key(ch(k)) {
                                let Effect::Copy(t) = e else {
                                    panic!("unexpected effect {e:?}");
                                };
                                assert_ne!(view, Screen::Connections, "copied on Connections");
                                check(&t);
                                copied += 1;
                            }
                            let n = app.notice.clone().map(|n| n.text).unwrap_or_default();
                            assert!(!n.contains(PASSWORD) && !n.contains(USER), "{n}");
                        }
                        app.handle_key(ch('j'));
                    }
                }
            }
            // The add flow: a pasted connection string with a password, then the
            // password step. `y` and `Y` are text there, never a copy.
            app.handle_key(ch('a'));
            app.handle_paste(fixtures::PROD_URL.into());
            for k in ['y', 'Y'] {
                assert!(app.handle_key(ch(k)).is_empty());
            }
            app.handle_key(key(KeyCode::Esc));
            app.handle_key(ch('a'));
            type_str(&mut app, "redis://lens-ro@stage-cache.internal:6379");
            app.handle_key(key(KeyCode::Enter));
            assert!(app.add.as_ref().unwrap().password.is_some());
            for k in ['y', 'Y'] {
                assert!(app.handle_key(ch(k)).is_empty());
            }
        }
        // The test is not vacuous.
        assert!(copied > 50, "only {copied} copies");
    }

    // -----------------------------------------------------------------------
    // Paging
    // -----------------------------------------------------------------------

    /// The selected index of the current list view.
    fn list_index(app: &App) -> usize {
        match app.screen() {
            Screen::Connections => app
                .selectable()
                .iter()
                .position(|s| *s == app.conn_sel)
                .unwrap(),
            Screen::Patterns => app.patterns.sel,
            Screen::Ttl => app.sel.ttl,
            Screen::Diff => app.sel.diff,
            Screen::HotKeys => app.sel.hot,
            Screen::Nodes => app.sel.nodes,
            s => panic!("{s:?} is not a list App keeps"),
        }
    }

    fn list_len(app: &App) -> usize {
        match app.screen() {
            Screen::Connections => app.selectable().len(),
            Screen::Patterns => app.pattern_rows().len(),
            Screen::Ttl => app.ttl_rows().len(),
            Screen::Diff => app.diff().map_or(0, |d| d.patterns.len()),
            Screen::HotKeys => app.report().map_or(0, |r| r.hot_keys.len()),
            Screen::Nodes => app.nodes().map_or(0, <[NodeReport]>::len),
            s => panic!("{s:?} is not a list App keeps"),
        }
    }

    /// prod-cache-01 scanned with hot keys and history, a Docker container listed. For
    /// the Nodes view, a scan of a 6-shard cluster instead (a standalone one has no rows).
    fn paging_app_for(screen: Screen) -> App {
        if screen == Screen::Nodes {
            let (mut app, _) = cluster_scanned(fixtures::cluster_report_with(&[1.0 / 6.0; 6]));
            app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
            return app;
        }
        paging_app()
    }

    /// prod-cache-01 scanned with hot keys and history, a Docker container listed.
    fn paging_app() -> App {
        let (mut app, _, _) = finished_with_history();
        app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
        app
    }

    #[test]
    fn page_keys_move_by_the_visible_rows_on_every_list_view() {
        for view in LEGACY.into_iter().chain([Screen::Nodes]) {
            for rows in [0, 1, 3, 1000] {
                let mut app = paging_app_for(view);
                show(&mut app, view);
                app.set_list_rows(rows);
                let len = list_len(&app);
                let last = len - 1;
                assert!(len >= 5, "{view:?} has {len} rows");
                // A page of 0 rows (a tiny terminal) still moves one. The panel row of
                // the tab bar takes one of the rows on Analysis.
                let panel_row = usize::from(!view.view().panels().is_empty());
                let page = rows.saturating_sub(panel_row).max(1);
                let at = |app: &App| list_index(app);
                let ctx = format!("{view:?}, {rows} rows");
                assert_eq!(at(&app), 0, "{ctx}");
                app.handle_key(key(KeyCode::PageDown));
                assert_eq!(at(&app), page.min(last), "{ctx}");
                app.handle_key(key(KeyCode::PageDown));
                assert_eq!(at(&app), (2 * page).min(last), "{ctx}");
                for _ in 0..len {
                    app.handle_key(key(KeyCode::PageDown));
                }
                assert_eq!(at(&app), last, "{ctx}: clamped to the last row");
                app.handle_key(key(KeyCode::PageUp));
                assert_eq!(at(&app), last.saturating_sub(page), "{ctx}");
                for _ in 0..len {
                    app.handle_key(key(KeyCode::PageUp));
                }
                assert_eq!(at(&app), 0, "{ctx}: clamped to the first row");
                app.handle_key(key(KeyCode::End));
                assert_eq!(at(&app), last, "{ctx}");
                app.handle_key(key(KeyCode::Home));
                assert_eq!(at(&app), 0, "{ctx}");
                app.handle_key(ch('G'));
                assert_eq!(at(&app), last, "{ctx}");
                app.handle_key(ch('g'));
                assert_eq!(at(&app), 0, "{ctx}");
            }
        }
    }

    #[test]
    fn page_keys_on_empty_lists_stay_at_the_top() {
        let mut app = App::new(fixtures::profiles());
        app.set_list_rows(5);
        for view in ['1', '2', '3', '4'] {
            go(&mut app, view);
            for k in [
                KeyCode::PageDown,
                KeyCode::End,
                KeyCode::PageUp,
                KeyCode::Home,
            ] {
                app.handle_key(key(k));
                assert_eq!(list_index(&app), 0, "{view} {k:?}");
            }
        }
        // Connections without entries has only the add row.
        let mut app = App::new(vec![]);
        app.apply(Msg::DockerFound(Vec::new()));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.conn_sel, ConnSel::Add);
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.conn_sel, ConnSel::Add);
    }

    #[test]
    fn paging_patterns_follows_the_row_and_restarts_the_marquee() {
        let mut app = paging_app();
        app.handle_key(ch('1'));
        app.set_list_rows(2);
        app.apply(Msg::Tick);
        app.apply(Msg::Tick);
        assert_eq!(app.sel_ticks(), 2);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.patterns.sel, 2);
        assert_eq!(app.sel_ticks(), 0);
        let id = app.selected_row_id();
        // The selected row stays selected when a partial reorders the rows (a rescan).
        let sid = app.session.as_ref().unwrap().id;
        let s2 = press_rescan(&mut app, sid);
        connected(&mut app, sid, s2);
        send_partial(&mut app, sid, fixtures::partial(true));
        assert_eq!(app.selected_row_id(), id);
        // On another view, PgDn moves that view's own selection.
        app.handle_key(ch('0'));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(list_index(&app), 2);
        assert_eq!(
            app.selected_row_id(),
            id,
            "Patterns keeps its own selection"
        );
    }

    // -----------------------------------------------------------------------
    // Views, panels, drill-down and the live connection
    // -----------------------------------------------------------------------

    fn calls_since(app: &App, n: usize) -> Vec<(Owner, &'static str)> {
        app.calls[n..].to_vec()
    }

    fn live_err(app: &mut App, req_id: ReqId) -> Vec<Effect> {
        let (id, scan) = (app.session.as_ref().unwrap().id, scan_of(app));
        app.apply(Msg::Live {
            id,
            scan,
            req_id,
            result: Err(crate::live::LiveError::Closed),
        })
    }

    #[test]
    fn view_states_are_told_when_they_show_and_hide() {
        let mut app = views::testing::scanned();
        let n = app.calls.len();
        app.handle_key(ch('3'));
        app.handle_key(key(KeyCode::Tab));
        // TTL and Nodes are App's own: nothing to tell.
        app.handle_key(ch('2'));
        assert_eq!(app.screen(), Screen::Ttl);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.screen(), Screen::Nodes);
        app.handle_key(key(KeyCode::BackTab));
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.screen(), Screen::BigKeys);
        app.handle_key(ch('4'));
        // Pressing the digit of the view on screen changes nothing.
        app.handle_key(ch('4'));
        assert_eq!(
            calls_since(&app, n),
            [
                (Owner::Server, "show"),
                (Owner::Server, "hide"),
                (Owner::Server, "show"),
                (Owner::Server, "hide"),
                (Owner::Analysis, "show"),
                (Owner::Analysis, "hide"),
                (Owner::Analysis, "show"),
                (Owner::Analysis, "hide"),
                (Owner::Streams, "show"),
            ]
        );
        // `a` leaves for Connections, so the view is hidden first.
        app.handle_key(ch('a'));
        assert_eq!(app.calls.last(), Some(&(Owner::Streams, "hide")));
    }

    #[test]
    fn only_the_view_on_screen_ticks_and_only_with_the_time() {
        let t0 = Instant::now();
        let mut app = views::testing::scanned();
        app.handle_key(ch('6'));
        let n = app.calls.len();
        app.apply(Msg::Tick);
        app.apply(Msg::TickAt(t0));
        app.handle_key(ch('1'));
        app.apply(Msg::TickAt(t0));
        assert_eq!(
            calls_since(&app, n),
            [(Owner::Search, "tick"), (Owner::Search, "hide")]
        );
        // Without a session no view is called at all.
        let mut idle = App::new(fixtures::profiles());
        for c in ['3', '4', '5', '6'] {
            idle.handle_key(ch(c));
            idle.apply(Msg::TickAt(t0));
            idle.handle_key(ch('j'));
            idle.handle_key(ch('r'));
        }
        assert!(idle.calls.is_empty(), "{:?}", idle.calls);
        assert_eq!(
            idle.notice.unwrap().text,
            "Nothing to refresh: connect first (0 Connections, then enter)."
        );
    }

    #[test]
    fn keys_go_to_the_view_on_screen_and_r_refreshes() {
        let mut app = views::testing::scanned();
        for (c, owner) in [
            ('3', Owner::Server),
            ('4', Owner::Streams),
            ('5', Owner::PubSub),
            ('6', Owner::Search),
        ] {
            app.handle_key(ch(c));
            let n = app.calls.len();
            for k in [
                ch('j'),
                ch('p'),
                ch('+'),
                key(KeyCode::Enter),
                key(KeyCode::Esc),
            ] {
                app.handle_key(k);
            }
            // r refreshes; s saves nothing here.
            assert!(app.handle_key(ch('r')).is_empty());
            assert!(app.handle_key(ch('s')).is_empty());
            let mut expected = vec![(owner, "key"); 5];
            expected.push((owner, "refresh"));
            expected.push((owner, "key"));
            assert_eq!(calls_since(&app, n), expected, "{c}");
            if owner == Owner::PubSub {
                // `s` opened the subscribe input there: close it.
                app.handle_key(key(KeyCode::Esc));
            }
        }
        // tab is the view's own on Streams, Pub/Sub and Search, App's on Server.
        let n = app.calls.len();
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(calls_since(&app, n), [(Owner::Search, "key")]);
        app.handle_key(ch('3'));
        let n = app.calls.len();
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(
            calls_since(&app, n),
            [(Owner::Server, "hide"), (Owner::Server, "show")]
        );
        // On Analysis r rescans and s saves, on Big keys too; y there is the view's.
        show(&mut app, Screen::BigKeys);
        assert!(matches!(
            app.handle_key(ch('s'))[..],
            [Effect::SaveSnapshot(_)]
        ));
        let n = app.calls.len();
        assert!(app.handle_key(ch('y')).is_empty());
        assert_eq!(calls_since(&app, n), [(Owner::Analysis, "key")]);
        assert!(matches!(
            app.handle_key(ch('r'))[..],
            [Effect::Connect { .. }]
        ));
    }

    #[test]
    fn a_rescan_whose_probe_changes_tells_the_view_on_screen() {
        let (mut app, id) = scanned_with(fixtures::report(true), fixtures::perms_ok());
        show(&mut app, Screen::SlowLog);
        assert_eq!(app.active_owner(), Some(Owner::Server));
        let connected = |app: &mut App, perms| {
            let _ = app.rescan();
            let scan = scan_of(app);
            let out = app.apply(Msg::Connected {
                id,
                scan,
                server: Some(fixtures::report(true).server),
                perms,
                target: ScanTarget::Primary,
            });
            app.apply(Msg::ScanDone {
                id,
                scan,
                report: Box::new(fixtures::report(true)),
            });
            out
        };
        // The ACL changed: SLOWLOG is refused now, so the view is hidden.
        let n = app.calls.len();
        connected(&mut app, fixtures::perms_probes_off());
        assert_eq!(app.active_owner(), None);
        assert_eq!(calls_since(&app, n), [(Owner::Server, "hide")]);
        // And allowed again: shown again.
        let n = app.calls.len();
        connected(&mut app, fixtures::perms_ok());
        assert_eq!(app.active_owner(), Some(Owner::Server));
        assert_eq!(calls_since(&app, n), [(Owner::Server, "show")]);
        // Unchanged: nobody is told.
        let n = app.calls.len();
        connected(&mut app, fixtures::perms_ok());
        assert!(calls_since(&app, n).is_empty());
    }

    #[test]
    fn a_new_connect_forgets_the_last_connects_checks() {
        let (mut app, id) = scanned_with(fixtures::report(true), fixtures::perms_probes_off());
        assert!(app.entry(id).unwrap().perms.is_some());
        let _ = app.connect(id);
        assert!(app.entry(id).unwrap().perms.is_none());
    }

    #[test]
    fn a_failed_probe_turns_its_view_off_but_not_the_connection() {
        let (mut app, id) = scanned_with(fixtures::report(true), fixtures::perms_probes_off());
        assert_eq!(app.entry(id).unwrap().status, ConnStatus::Connected);
        show(&mut app, Screen::SlowLog);
        assert_eq!(app.active_owner(), None);
        let n = app.calls.len();
        app.handle_key(ch('j'));
        app.apply(Msg::TickAt(Instant::now()));
        app.handle_key(ch('r'));
        assert!(calls_since(&app, n).is_empty());
        assert_eq!(
            app.notice.clone().unwrap().text,
            "The server refused SLOWLOG for this user, so this view is off."
        );
        // Leaving it tells nobody; the Monitor panel next to it works.
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.screen(), Screen::Monitor);
        assert_eq!(calls_since(&app, n), [(Owner::Server, "show")]);
        show(&mut app, Screen::Search);
        assert_eq!(app.active_owner(), None);
        app.handle_key(ch('r'));
        assert_eq!(
            app.notice.clone().unwrap().text,
            "FT._LIST is not available: no Search module."
        );
        // A denied scan check still makes the connection limited.
        let (app, id) = scanned_with(fixtures::report(true), fixtures::perms_memory_denied());
        assert_eq!(app.entry(id).unwrap().status, ConnStatus::Limited);
    }

    #[test]
    fn enter_on_a_pattern_opens_its_keys_and_esc_goes_back() {
        let mut app = scanned_app(true);
        // A group still folds.
        select(&mut app, &grp("cache"));
        let all = app.pattern_rows().len();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.pattern_rows().len(), all - 2);
        app.handle_key(key(KeyCode::Enter));
        // A child pattern opens its key list, one level down.
        app.handle_key(key(KeyCode::Right));
        assert_eq!(selected_label(&app), "cache:search:{hash}");
        let n = app.calls.len();
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.view, View::Patterns);
        assert_eq!(
            app.views.keys.pattern.as_deref(),
            Some("cache:search:{hash}")
        );
        assert_eq!(
            calls_since(&app, n),
            [(Owner::Keys, "open"), (Owner::Keys, "show")]
        );
        // Other views and back keep it open; esc closes it.
        app.handle_key(ch('2'));
        app.handle_key(ch('1'));
        assert_eq!(app.screen(), Screen::KeyList);
        app.handle_key(ch('j'));
        assert_eq!(app.calls.last(), Some(&(Owner::Keys, "key")));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Patterns);
        assert_eq!(selected_label(&app), "cache:search:{hash}");
        assert_eq!(app.calls.last(), Some(&(Owner::Keys, "hide")));
        // A pattern of its own at the top level opens too.
        app.handle_key(ch('g'));
        assert_eq!(selected_label(&app), "session:{uuid}");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.views.keys.pattern.as_deref(), Some("session:{uuid}"));
        // On Patterns esc clears the filter first, and never opens anything.
        app.handle_key(key(KeyCode::Esc));
        type_str(&mut app, "/session");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.patterns.filter, "");
        assert_eq!(app.screen(), Screen::Patterns);
        // A new connect starts on Patterns with fresh view states.
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ch('0'));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::Patterns);
        assert_eq!(app.views.keys.pattern, None);
    }

    #[test]
    fn live_messages_of_other_runs_are_ignored() {
        let mut app = views::testing::scanned();
        let s = app.session.as_ref().unwrap();
        let (id, scan) = (s.id, s.scan);
        assert_eq!(s.live, LiveState::Waiting);
        app.apply(Msg::LiveReady { id, scan: scan + 7 });
        app.apply(Msg::LiveReady { id: id + 7, scan });
        assert_eq!(app.session.as_ref().unwrap().live, LiveState::Waiting);
        app.apply(Msg::LiveReady { id, scan });
        assert_eq!(app.session.as_ref().unwrap().live, LiveState::Ready);
        app.apply(Msg::LiveClosed {
            id,
            scan: scan + 7,
            error: Some("connection reset".into()),
        });
        assert_eq!(app.session.as_ref().unwrap().live, LiveState::Ready);
        assert!(app.notice.is_none());
        // A rescan's run has a live connection of its own, not open yet.
        app.handle_key(ch('1'));
        app.handle_key(ch('r'));
        assert_eq!(app.session.as_ref().unwrap().live, LiveState::Waiting);
        app.apply(Msg::LiveReady { id, scan });
        assert_eq!(app.session.as_ref().unwrap().live, LiveState::Waiting);
    }

    #[test]
    fn replies_go_to_the_view_that_sent_them() {
        let mut app = views::testing::scanned();
        views::testing::live_ready(&mut app);
        // Fake two sent requests. Patterns sends none of its own, so on it these are the
        // only ones pending (a view that sends on show would take ReqId(1) itself).
        app.live_pending.insert(ReqId(1), Owner::Server);
        app.live_pending.insert(ReqId(2), Owner::Keys);
        app.handle_key(ch('1'));
        let n = app.calls.len();
        live_err(&mut app, ReqId(2));
        live_err(&mut app, ReqId(1));
        // Answered already, or never asked: dropped.
        live_err(&mut app, ReqId(1));
        live_err(&mut app, ReqId(9));
        assert_eq!(
            calls_since(&app, n),
            [(Owner::Keys, "reply"), (Owner::Server, "reply")]
        );
        assert!(app.live_pending.is_empty());
        // A reply for another run is dropped and the request stays pending.
        app.live_pending.insert(ReqId(3), Owner::Server);
        let (id, scan) = (app.session.as_ref().unwrap().id, scan_of(&app));
        app.apply(Msg::Live {
            id,
            scan: scan + 1,
            req_id: ReqId(3),
            result: Err(crate::live::LiveError::Closed),
        });
        assert_eq!(app.calls.len(), n + 2);
        assert!(app.live_pending.contains_key(&ReqId(3)));
    }

    #[test]
    fn a_closed_live_connection_is_shown_and_forgets_its_requests() {
        let mut app = views::testing::scanned();
        views::testing::live_ready(&mut app);
        app.live_pending.insert(ReqId(1), Owner::Server);
        let (id, scan) = (app.session.as_ref().unwrap().id, scan_of(&app));
        app.apply(Msg::LiveClosed {
            id,
            scan,
            error: Some("connection reset by peer".into()),
        });
        let s = app.session.as_ref().unwrap();
        assert_eq!(
            s.live,
            LiveState::Closed(Some("connection reset by peer".into()))
        );
        assert!(app.live_pending.is_empty());
        let n = app.notice.clone().unwrap();
        assert!(n.error);
        assert!(n.text.contains("connection reset by peer"), "{}", n.text);
        // Its reply can no longer reach a view.
        let calls = app.calls.len();
        live_err(&mut app, ReqId(1));
        assert_eq!(app.calls.len(), calls);
        // Closed on purpose: no notice.
        let mut quiet = views::testing::scanned();
        let (id, scan) = (quiet.session.as_ref().unwrap().id, scan_of(&quiet));
        quiet.apply(Msg::LiveClosed {
            id,
            scan,
            error: None,
        });
        assert_eq!(
            quiet.session.as_ref().unwrap().live,
            LiveState::Closed(None)
        );
        assert!(quiet.notice.is_none());
    }

    #[test]
    fn rescan_forgets_the_replaced_runs_live_requests() {
        let mut app = views::testing::scanned();
        views::testing::live_ready(&mut app);
        app.live_pending.insert(ReqId(1), Owner::Streams);
        app.handle_key(ch('1'));
        app.handle_key(ch('r'));
        assert!(app.live_pending.is_empty());
    }

    // -----------------------------------------------------------------------
    // The key table and the handlers agree
    // -----------------------------------------------------------------------

    /// Everything a key can change, as text.
    fn fingerprint(app: &App) -> String {
        format!(
            "{:?}",
            (
                app.screen(),
                app.conn_sel,
                app.add
                    .as_ref()
                    .map(|a| (a.input.clone(), a.password.is_some())),
                app.help,
                &app.notice,
                &app.patterns,
                app.sel,
                &app.docker,
                app.session.as_ref().map(|s| (s.scan, s.phase.clone())),
                &app.views,
                &app.confirm,
            )
        )
    }

    /// The key events of a key-table token.
    fn events_of(token: &str) -> Vec<KeyEvent> {
        match token {
            "↑↓" => vec![key(KeyCode::Up), key(KeyCode::Down)],
            "→" => vec![key(KeyCode::Right)],
            "←" => vec![key(KeyCode::Left)],
            "PgUp" => vec![key(KeyCode::PageUp)],
            "PgDn" => vec![key(KeyCode::PageDown)],
            "Home" => vec![key(KeyCode::Home)],
            "End" => vec![key(KeyCode::End)],
            "enter" => vec![key(KeyCode::Enter)],
            "esc" => vec![key(KeyCode::Esc)],
            "tab" => vec![key(KeyCode::Tab)],
            "shift-tab" => vec![key(KeyCode::BackTab)],
            "ctrl-c" => vec![KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)],
            "0-6" => ('0'..='6').map(ch).collect(),
            "space" => vec![ch(' ')],
            t if t.chars().count() == 1 => vec![ch(t.chars().next().unwrap())],
            t => panic!("no key events for token {t:?}"),
        }
    }

    /// The key-table token of a key event.
    fn token_of(k: &KeyEvent) -> String {
        match k.code {
            KeyCode::Up | KeyCode::Down => "↑↓".into(),
            KeyCode::Right => "→".into(),
            KeyCode::Left => "←".into(),
            KeyCode::PageUp => "PgUp".into(),
            KeyCode::PageDown => "PgDn".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Tab => "tab".into(),
            KeyCode::BackTab => "shift-tab".into(),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => "ctrl-c".into(),
            KeyCode::Char('0'..='6') => "0-6".into(),
            KeyCode::Char(' ') => "space".into(),
            KeyCode::Char(c) => c.to_string(),
            other => format!("{other:?}"),
        }
    }

    fn tokens(screen: Screen) -> Vec<&'static str> {
        super::super::keymap::global()
            .iter()
            .chain(super::super::keymap::for_screen(screen))
            .flat_map(|h| h.keys.split_whitespace())
            .collect()
    }

    /// A state where every key of `screen` has something to do: a finished scan with
    /// hot keys and history, Docker listed, the selection on the second row (on
    /// Patterns the expanded `cache:*` group), a page of 3 rows. On Nodes, a 6-shard
    /// cluster scan.
    fn busy_app(screen: Screen) -> App {
        let mut app = paging_app_for(screen);
        app.set_list_rows(3);
        show(&mut app, screen);
        app.handle_key(ch('j'));
        if screen == Screen::Patterns {
            assert_eq!(selected_label(&app), "cache:*");
        }
        app
    }

    /// Every key in the table does something on its screen: it changes the state,
    /// returns effects, or (on a screen a view state owns) reaches that state. What a
    /// view state then does with it is tested in its own module.
    #[test]
    fn every_key_in_the_table_is_handled_on_its_view() {
        for screen in Screen::ALL {
            for token in tokens(screen) {
                for k in events_of(token) {
                    let mut app = busy_app(screen);
                    if screen == Screen::Patterns && token == "esc" {
                        // Esc clears a filter: set one first.
                        app.handle_key(ch('/'));
                        app.handle_key(ch('e'));
                        app.handle_key(key(KeyCode::Enter));
                    }
                    if screen == Screen::Patterns && (token == "→" || token == "l") {
                        // On an expanded group → moves into it; collapsed, it expands.
                        app.handle_key(key(KeyCode::Left));
                    }
                    let before = fingerprint(&app);
                    let calls = app.calls.len();
                    let effects = app.handle_key(k);
                    let ctx = format!("{token:?} ({k:?}) on {screen:?}");
                    if token == "0-6" {
                        let KeyCode::Char(c) = k.code else {
                            unreachable!()
                        };
                        assert_eq!(app.view, View::ALL[c as usize - '0' as usize], "{ctx}");
                        continue;
                    }
                    assert!(
                        !effects.is_empty()
                            || fingerprint(&app) != before
                            || app.calls[calls..]
                                .iter()
                                .any(|c| matches!(c.1, "key" | "refresh")),
                        "{ctx} did nothing"
                    );
                }
            }
        }
    }

    /// No key outside the table changes anything, view states included.
    #[test]
    fn every_key_the_handlers_take_is_in_the_table() {
        let mut candidates: Vec<KeyEvent> = (' '..='~').map(ch).collect();
        candidates.extend(
            [
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::PageUp,
                KeyCode::PageDown,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Enter,
                KeyCode::Esc,
                KeyCode::Tab,
                KeyCode::BackTab,
                KeyCode::Backspace,
                KeyCode::Delete,
                KeyCode::Insert,
                KeyCode::F(1),
            ]
            .map(key),
        );
        candidates.push(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        for screen in Screen::ALL {
            let table = tokens(screen);
            for k in &candidates {
                let mut app = busy_app(screen);
                let before = fingerprint(&app);
                let effects = app.handle_key(*k);
                if effects.is_empty() && fingerprint(&app) == before {
                    continue;
                }
                let t = token_of(k);
                assert!(
                    table.contains(&t.as_str()),
                    "{k:?} ({t}) does something on {screen:?} but is not in the key table"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Clusters: the Nodes view and narrowing Patterns to a node
    // -----------------------------------------------------------------------

    fn summary_of(report: &ScanReport) -> String {
        let n = report.nodes.as_ref().map_or(0, Vec::len);
        format!("cluster · {n} shards")
    }

    /// prod-cache-01 connected as a cluster and `report` finished scanning.
    fn cluster_scanned(report: ScanReport) -> (App, ConnId) {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let Effect::Connect { id, .. } = effects[0] else {
            panic!()
        };
        app.apply(Msg::Connected {
            id,
            scan: scan_of(&app),
            server: Some(report.server.clone()),
            perms: fixtures::perms_ok(),
            target: ScanTarget::Cluster(summary_of(&report)),
        });
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: Box::new(report),
        });
        (app, id)
    }

    fn shown_patterns(app: &App) -> Vec<PatternStats> {
        app.pattern_data().unwrap().patterns.to_vec()
    }

    fn node_patterns(app: &App, i: usize) -> Vec<PatternStats> {
        app.nodes().unwrap()[i].patterns.clone()
    }

    #[test]
    fn enter_on_a_node_narrows_patterns_and_esc_goes_back() {
        let report = fixtures::cluster_report();
        let (mut app, _) = cluster_scanned(report.clone());
        show(&mut app, Screen::Nodes);
        assert_eq!(app.screen(), Screen::Nodes);
        app.handle_key(ch('j'));
        assert_eq!(app.sel.nodes, 1);
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        // Patterns, narrowed to the second node, from the top.
        assert_eq!(app.screen(), Screen::Patterns);
        let (i, n) = app.narrowed_node().unwrap();
        assert_eq!(
            (i, n.endpoint.to_string()),
            (1, "10.0.1.12:6379".to_string())
        );
        assert_eq!(shown_patterns(&app), node_patterns(&app, 1));
        assert_ne!(shown_patterns(&app), report.patterns);
        assert_eq!(app.patterns.sel, 0);
        // The tree is built from the node's rows.
        let rows = app.pattern_rows();
        assert!(!rows.is_empty());
        let node = node_patterns(&app, 1);
        assert!(rows.iter().all(|r| match *r {
            Row::Pattern { idx, .. } => idx < node.len(),
            Row::Group { .. } => true,
        }));
        // Moving and the other views keep the narrowing.
        app.handle_key(ch('j'));
        app.handle_key(ch('2'));
        app.handle_key(ch('1'));
        assert!(app.narrowed_node().is_some());
        // The selected pattern stays selected when esc goes back to the merged rows.
        let picked = selected_label(&app);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Nodes);
        assert!(app.narrowed_node().is_none());
        assert_eq!(app.patterns.node, None);
        assert_eq!(shown_patterns(&app), report.patterns);
        assert_eq!(app.sel.nodes, 1, "the Nodes selection is kept");
        app.handle_key(ch('1'));
        assert_eq!(selected_label(&app), picked);
        // esc on the merged rows, with no filter, does nothing.
        let before = fingerprint(&app);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(fingerprint(&app), before);
    }

    #[test]
    fn esc_clears_the_filter_before_it_leaves_the_node() {
        let (mut app, _) = cluster_scanned(fixtures::cluster_report());
        show(&mut app, Screen::Nodes);
        app.handle_key(key(KeyCode::Enter));
        type_str(&mut app, "/user");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.patterns.filter, "user");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.patterns.filter, "");
        assert_eq!(app.screen(), Screen::Patterns);
        assert!(app.narrowed_node().is_some());
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Nodes);
        assert!(app.narrowed_node().is_none());
    }

    #[test]
    fn a_failed_node_is_not_narrowed_to() {
        let (mut app, _) = cluster_scanned(fixtures::cluster_report_degraded());
        show(&mut app, Screen::Nodes);
        type_str(&mut app, "G");
        assert_eq!(app.sel.nodes, 2);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::Nodes);
        assert_eq!(app.patterns.node, None);
        let n = app.notice.clone().unwrap();
        assert!(!n.error);
        assert_eq!(
            n.text,
            "10.0.1.13:6379 was not scanned, so it has no patterns to show."
        );
        // A partial node can be narrowed to: it has results.
        app.handle_key(ch('k'));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.narrowed_node().map(|(i, _)| i), Some(1));
    }

    #[test]
    fn nodes_keys_on_a_standalone_report_change_nothing() {
        let (mut app, _, _) = finished_with_history();
        show(&mut app, Screen::Nodes);
        assert_eq!(app.screen(), Screen::Nodes);
        assert_eq!(app.nodes(), None);
        let before = fingerprint(&app);
        for k in [
            key(KeyCode::Enter),
            key(KeyCode::Down),
            key(KeyCode::End),
            key(KeyCode::Esc),
            ch('j'),
            ch('y'),
            ch('Y'),
        ] {
            assert!(app.handle_key(k).is_empty(), "{k:?}");
            assert_eq!(fingerprint(&app), before, "{k:?}");
        }
        // Rescan and snapshot still work from view 5.
        assert!(matches!(
            app.handle_key(ch('s'))[..],
            [Effect::SaveSnapshot(_)]
        ));
        assert!(matches!(
            app.handle_key(ch('r'))[..],
            [Effect::Connect { .. }]
        ));
    }

    #[test]
    fn s_saves_a_cluster_report_with_its_nodes() {
        let report = fixtures::cluster_report();
        let (mut app, _) = cluster_scanned(report.clone());
        for screen in [Screen::Patterns, Screen::HotKeys, Screen::Nodes] {
            show(&mut app, screen);
            let effects = app.handle_key(ch('s'));
            let [Effect::SaveSnapshot(r)] = &effects[..] else {
                panic!("unexpected effects {effects:?}");
            };
            assert_eq!(**r, report, "{screen:?}");
        }
        // Narrowed, `s` still saves the whole report, not the node's rows.
        show(&mut app, Screen::Nodes);
        app.handle_key(key(KeyCode::Enter));
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(r)] = &effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(**r, report);
    }

    #[test]
    fn y_copies_nothing_on_nodes() {
        let (mut app, _) = cluster_scanned(fixtures::cluster_report());
        show(&mut app, Screen::Nodes);
        for k in ['y', 'Y'] {
            assert!(app.handle_key(ch(k)).is_empty());
            assert_eq!(app.notice, None);
        }
    }

    /// The report a rescan of the cluster finishes with.
    fn rescan_to(app: &mut App, id: ConnId, report: ScanReport, partial: bool) {
        let s = press_rescan(app, id);
        connected(app, id, s);
        if partial {
            send_partial(app, id, fixtures::partial(true));
        }
        app.apply(Msg::ScanDone {
            id,
            scan: s,
            report: Box::new(report),
        });
    }

    #[test]
    fn a_rescan_keeps_the_node_until_partial_results_or_a_report_without_it() {
        let report = fixtures::cluster_report();
        let (mut app, id) = cluster_scanned(report.clone());
        show(&mut app, Screen::Nodes);
        type_str(&mut app, "j");
        app.handle_key(key(KeyCode::Enter));
        // While the rescan runs, the previous report stays narrowed.
        let s = press_rescan(&mut app, id);
        connected(&mut app, id, s);
        assert_eq!(app.narrowed_node().map(|(i, _)| i), Some(1));
        assert_eq!(shown_patterns(&app), node_patterns(&app, 1));
        // A new report with the same node: still narrowed to it.
        app.apply(Msg::ScanDone {
            id,
            scan: s,
            report: Box::new(report.clone()),
        });
        assert_eq!(app.narrowed_node().map(|(i, _)| i), Some(1));
        // Partial results are merged: the narrowing ends and stays ended.
        let s = press_rescan(&mut app, id);
        connected(&mut app, id, s);
        send_partial(&mut app, id, fixtures::partial(true));
        assert_eq!(app.patterns.node, None);
        assert_eq!(
            shown_patterns(&app),
            fixtures::partial(true).patterns.clone()
        );
        app.apply(Msg::ScanDone {
            id,
            scan: s,
            report: Box::new(report.clone()),
        });
        assert!(app.narrowed_node().is_none());
        assert_eq!(shown_patterns(&app), report.patterns);
        // A report whose node set changed: the node is gone, so are the narrowing and
        // the stale key.
        show(&mut app, Screen::Nodes);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.narrowed_node().map(|(i, _)| i), Some(1));
        let mut moved = report.clone();
        moved.nodes.as_mut().unwrap()[1].endpoint.host = "10.0.1.32".into();
        rescan_to(&mut app, id, moved.clone(), false);
        assert_eq!(app.patterns.node, None);
        assert_eq!(shown_patterns(&app), moved.patterns);
    }

    #[test]
    fn a_rescan_clamps_the_nodes_selection_and_a_new_connect_resets_it() {
        let (mut app, id) = cluster_scanned(fixtures::cluster_report_with(&[0.25; 4]));
        show(&mut app, Screen::Nodes);
        type_str(&mut app, "G");
        assert_eq!(app.sel.nodes, 3);
        rescan_to(&mut app, id, fixtures::cluster_report(), false);
        assert_eq!(app.sel.nodes, 2);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.narrowed_node().is_some());
        // Connecting again starts afresh: no narrowing, the first node.
        app.handle_key(ch('0'));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.patterns.node, None);
        assert_eq!(app.sel.nodes, 0);
    }

    #[test]
    fn a_narrowed_view_folds_and_sorts_the_nodes_rows() {
        let (mut app, _) = cluster_scanned(fixtures::cluster_report());
        show(&mut app, Screen::Nodes);
        app.handle_key(key(KeyCode::Enter));
        let node = node_patterns(&app, 0);
        // Sorted by keys, the node's own biggest pattern by keys comes first.
        app.handle_key(ch('o'));
        let top = node.iter().max_by_key(|p| p.est_keys).unwrap();
        let first = app.pattern_rows()[0];
        let label = match first {
            Row::Pattern { idx, .. } => node[idx].pattern.clone(),
            Row::Group { .. } => selected_label(&app),
        };
        assert!(
            label == top.pattern || label.ends_with(":*"),
            "{label} vs {}",
            top.pattern
        );
        // Collapsing a group of the node's rows works like on the merged ones.
        select(&mut app, &grp("cache"));
        app.handle_key(key(KeyCode::Left));
        assert!(
            app.pattern_rows()
                .iter()
                .all(|r| !matches!(r, Row::Pattern { parent: Some(_), idx } if node[*idx].pattern.starts_with("cache:")))
        );
    }
}
