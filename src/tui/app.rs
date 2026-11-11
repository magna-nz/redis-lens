//! TUI state and the pure update functions. Nothing here does IO: key presses and
//! messages change the state and return [`Effect`]s for the runtime to carry out.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use secrecy::{ExposeSecret, SecretString};

use crate::conn::ScanTarget;
use crate::conn::docker::{DiscoveredAuth, DiscoveredInstance};
use crate::conn::parse::{ParsedConnection, mask, parse_connection_string};
use crate::diff::ReportDiff;
use crate::model::{
    Auth, Credentials, PERM_FREQ, PatternStats, PermStatus, PermissionReport, Profile, ScanReport,
    ServerInfo,
};
use crate::scan::{PartialPatterns, ScanProgress};
use crate::verdict::ttl_verdict;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Connections,
    Patterns,
    Ttl,
    Diff,
    HotKeys,
}

impl View {
    pub const ALL: [View; 5] = [
        View::Connections,
        View::Patterns,
        View::Ttl,
        View::Diff,
        View::HotKeys,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Connections => "Connections",
            Self::Patterns => "Patterns",
            Self::Ttl => "TTL",
            Self::Diff => "Diff",
            Self::HotKeys => "Hot keys",
        }
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
    /// The last connect error. Never holds credentials.
    pub error: Option<String>,
    /// Added in this session through the add flow.
    pub just_saved: bool,
}

impl ConnEntry {
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
    /// Find the newest snapshot taken before `report` and diff it against `report`.
    LoadHistory {
        id: ConnId,
        scan: ScanId,
        report: Box<ScanReport>,
    },
    /// Write `report` to the snapshot directory.
    SaveSnapshot(Box<ScanReport>),
    Quit,
}

/// Results coming back from the runtime.
#[derive(Debug)]
pub enum Msg {
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
        report: ScanReport,
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
    SnapshotSaved(PathBuf),
    SnapshotFailed(String),
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
}

impl Session {
    /// The diff against the previous snapshot, when there is one.
    pub fn diff(&self) -> Option<&ReportDiff> {
        match &self.history {
            History::Diff(d) => Some(d),
            _ => None,
        }
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
}

/// The selected row of the TTL, Diff and Hot keys views. Each view keeps its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewSel {
    pub ttl: usize,
    pub diff: usize,
    pub hot: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    /// Tick at which an info notice disappears. Errors stay until a key press.
    pub until: Option<u64>,
}

pub struct App {
    pub view: View,
    /// Saved profiles first, then Docker containers.
    pub entries: Vec<ConnEntry>,
    pub docker: DockerState,
    pub conn_sel: ConnSel,
    pub add: Option<AddState>,
    pub session: Option<Session>,
    pub patterns: PatternsState,
    pub sel: ViewSel,
    pub notice: Option<Notice>,
    pub tick: u64,
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
}

/// Everything that decides which row is selected, to spot a change. The Patterns row is
/// by identity, so the same pattern at another index is no change and another pattern
/// at the same index is one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelId {
    view: View,
    conn: ConnSel,
    /// Only tracked on the Patterns view.
    patterns: Option<RowId>,
    sel: ViewSel,
    session: Option<(ConnId, ScanId)>,
}

impl Default for SelId {
    fn default() -> Self {
        Self {
            view: View::Connections,
            conn: ConnSel::Add,
            patterns: None,
            sel: ViewSel::default(),
            session: None,
        }
    }
}

/// What the Patterns view shows for `s`: see [`App::pattern_data`].
fn pattern_data_of(s: &Session) -> Option<PatternData<'_>> {
    if let Some(r) = &s.report {
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

fn clean_paste(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\r' | '\n')).collect()
}

impl App {
    pub fn new(profiles: Vec<Profile>) -> Self {
        let mut app = Self {
            view: View::Connections,
            entries: Vec::new(),
            docker: DockerState::Searching,
            conn_sel: ConnSel::Add,
            add: None,
            session: None,
            patterns: PatternsState::default(),
            sel: ViewSel::default(),
            notice: None,
            tick: 0,
            sel_since: 0,
            sel_id: SelId::default(),
            now: None,
            connect_started: None,
            next_id: 1,
            next_scan: 1,
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
        self.notice = Some(Notice {
            text: text.into(),
            error: true,
            until: None,
        });
    }

    pub fn set_info(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: false,
            until: Some(self.tick + NOTICE_TICKS),
        });
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

    /// What the Patterns view shows: the finished report's patterns, or the partial
    /// results while the scan runs, or `None`. `self.patterns.tree` is always built
    /// from these.
    pub fn pattern_data(&self) -> Option<PatternData<'_>> {
        pattern_data_of(self.session.as_ref()?)
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
        let Some(d) = self.session.as_ref().and_then(pattern_data_of) else {
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

    /// The number of rows in a list view.
    fn view_len(&self, view: View) -> usize {
        match view {
            View::Ttl => self.report().map_or(0, |r| r.patterns.len()),
            View::Diff => self.diff().map_or(0, |d| d.patterns.len()),
            View::HotKeys => self.report().map_or(0, |r| r.hot_keys.len()),
            View::Connections | View::Patterns => 0,
        }
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// Ticks since the selected row last changed. Drives the marquee on long names.
    pub fn sel_ticks(&self) -> u64 {
        self.tick.saturating_sub(self.sel_since)
    }

    /// Restarts the marquee when the selected row, the view or the session changed.
    /// A Patterns row counts by identity, not index.
    fn track_selection(&mut self) {
        let id = SelId {
            view: self.view,
            conn: self.conn_sel,
            patterns: if self.view == View::Patterns {
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
        if self.notice.as_ref().is_some_and(|n| n.error) {
            self.notice = None;
        }
        if self.add.is_some() {
            return self.add_key(key);
        }
        if self.view == View::Patterns && self.patterns.filtering {
            self.filter_key(key);
            return Vec::new();
        }
        match plain_char(&key) {
            Some('q') => return vec![Effect::Quit],
            Some(c @ '0'..='4') => {
                self.view = View::ALL[c as usize - '0' as usize];
                return Vec::new();
            }
            Some('a') => {
                self.start_add();
                return Vec::new();
            }
            Some('s') if self.view != View::Connections => return self.save_snapshot(),
            _ => {}
        }
        match self.view {
            View::Connections => self.conn_key(key),
            View::Patterns => {
                self.patterns_key(key);
                Vec::new()
            }
            View::Ttl | View::Diff | View::HotKeys => {
                self.list_key(key);
                Vec::new()
            }
        }
    }

    /// Only a finished report is ever saved, never partial results.
    fn save_snapshot(&mut self) -> Vec<Effect> {
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

    /// ↑↓ on the TTL, Diff and Hot keys views.
    fn list_key(&mut self, key: KeyEvent) {
        let len = self.view_len(self.view);
        let sel = match self.view {
            View::Ttl => &mut self.sel.ttl,
            View::Diff => &mut self.sel.diff,
            View::HotKeys => &mut self.sel.hot,
            View::Connections | View::Patterns => return,
        };
        let last = len.saturating_sub(1);
        let cur = (*sel).min(last);
        match (key.code, plain_char(&key)) {
            (KeyCode::Up, _) | (_, Some('k')) => *sel = cur.saturating_sub(1),
            (KeyCode::Down, _) | (_, Some('j')) => *sel = (cur + 1).min(last),
            (KeyCode::Home, _) | (_, Some('g')) => *sel = 0,
            (KeyCode::End, _) | (_, Some('G')) => *sel = last,
            _ => {}
        }
    }

    pub fn handle_paste(&mut self, text: String) {
        let text = clean_paste(&text);
        if let Some(add) = &mut self.add {
            if add.saving {
                return;
            }
            let target = add.password.as_mut().unwrap_or(&mut add.input);
            let room = MAX_INPUT.saturating_sub(target.len());
            target.extend(text.chars().take(room));
            add.error = None;
        } else if self.view == View::Patterns && self.patterns.filtering {
            let p = &mut self.patterns;
            let room = MAX_FILTER.saturating_sub(p.filter.chars().count());
            p.filter.extend(text.chars().take(room));
            p.sel = 0;
        } else if self.view == View::Connections {
            self.start_add();
            if let Some(add) = &mut self.add {
                add.input = text.chars().take(MAX_INPUT).collect();
            }
        }
    }

    fn start_add(&mut self) {
        self.view = View::Connections;
        self.add = Some(AddState::default());
        self.conn_sel = ConnSel::Add;
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
        match key.code {
            KeyCode::Up => self.move_conn(-1),
            KeyCode::Down => self.move_conn(1),
            KeyCode::Enter => match self.conn_sel {
                ConnSel::Add => self.start_add(),
                ConnSel::Entry(id) => return self.connect(id),
            },
            _ => match plain_char(&key) {
                Some('k') => self.move_conn(-1),
                Some('j') => self.move_conn(1),
                Some('r') => return self.rescan_docker(),
                _ => {}
            },
        }
        Vec::new()
    }

    fn move_conn(&mut self, dir: isize) {
        let items = self.selectable();
        let i = items.iter().position(|s| *s == self.conn_sel).unwrap_or(0);
        let j = i.saturating_add_signed(dir).min(items.len() - 1);
        self.conn_sel = items[j];
    }

    fn rescan_docker(&mut self) -> Vec<Effect> {
        if self.docker == DockerState::Searching {
            return Vec::new();
        }
        self.docker = DockerState::Searching;
        vec![Effect::DiscoverDocker]
    }

    fn connect(&mut self, id: ConnId) -> Vec<Effect> {
        if let Some(prev) = self.session.as_ref().map(|s| s.id)
            && prev != id
            && let Some(e) = self.entry_mut(prev)
            && e.status == ConnStatus::Connecting
        {
            e.status = ConnStatus::NotOpen;
        }
        let Some(entry) = self.entry_mut(id) else {
            return Vec::new();
        };
        entry.status = ConnStatus::Connecting;
        entry.error = None;
        let profile = entry.profile.clone();
        let secrets = match &entry.source {
            Source::Saved => SecretSource::Keychain,
            Source::Docker { credentials, .. } => SecretSource::Container(credentials.clone()),
        };
        let server = entry.server.clone();
        let scan = self.next_scan;
        self.next_scan += 1;
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
        });
        self.connect_started = self.now;
        self.patterns = PatternsState::default();
        self.sel = ViewSel::default();
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

    fn patterns_key(&mut self, key: KeyEvent) {
        let rows = self.pattern_rows();
        let last = rows.len().saturating_sub(1);
        let sel = self.patterns.sel.min(last);
        match (key.code, plain_char(&key)) {
            (KeyCode::Up, _) | (_, Some('k')) => self.patterns.sel = sel.saturating_sub(1),
            (KeyCode::Down, _) | (_, Some('j')) => self.patterns.sel = (sel + 1).min(last),
            (KeyCode::Right, _) | (_, Some('l')) => self.expand(&rows, sel),
            (KeyCode::Left, _) | (_, Some('h')) => self.collapse(&rows, sel),
            (KeyCode::Enter, _) => self.toggle(&rows, sel),
            (_, Some('/')) => {
                self.patterns.filtering = true;
            }
            (KeyCode::Esc, _) if !self.patterns.filter.is_empty() => {
                self.patterns.filter.clear();
                self.patterns.sel = 0;
            }
            (_, Some('o')) => self.cycle_sort(),
            _ => {}
        }
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
        if let Some(d) = self.session.as_ref().and_then(pattern_data_of) {
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
                return self.apply_inner(Msg::Tick);
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
            Msg::DockerFound(found) => self.set_docker(found),
            Msg::DockerFailed(_) => {
                self.set_docker(Vec::new());
                self.docker = DockerState::Unavailable;
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
                let limited = perms
                    .checks
                    .iter()
                    .any(|c| matches!(c.status, PermStatus::Denied(_)));
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
                if let Some(s) = self.session.as_mut() {
                    s.server = server;
                    s.target = Some(target);
                    s.phase = Phase::Scanning;
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
                // failed.
                let Some(s) = self.session.as_mut() else {
                    return Vec::new();
                };
                if s.id != id || s.scan != scan || s.phase != Phase::Scanning || s.report.is_some()
                {
                    return Vec::new();
                }
                let anchor = self.selection_anchor();
                if let Some(s) = self.session.as_mut() {
                    s.partial = Some(partial);
                }
                self.rebuild_patterns(anchor);
            }
            Msg::ScanDone { id, scan, report } => {
                if !self.is_session(id, scan) {
                    return Vec::new();
                }
                if let Some(e) = self.entry_mut(id) {
                    e.server = Some(report.server.clone());
                }
                // The selection follows its row from the partial results, if any.
                let anchor = self.selection_anchor();
                self.sel = ViewSel::default();
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
                }
                self.rebuild_patterns(anchor);
                return vec![effect];
            }
            Msg::History {
                id,
                scan,
                taken_at_unix,
                result,
            } => {
                let Some(s) = self.session.as_mut() else {
                    return Vec::new();
                };
                if s.id != id
                    || s.scan != scan
                    || s.report.as_ref().map(|r| r.taken_at_unix) != Some(taken_at_unix)
                {
                    return Vec::new();
                }
                match result {
                    Ok(Some(d)) => s.history = History::Diff(d),
                    Ok(None) => s.history = History::NoSnapshot,
                    Err(e) => {
                        s.history = History::Failed(e.clone());
                        self.set_error(format!("Could not load snapshots: {e}"));
                    }
                }
                self.sel.diff = 0;
            }
            Msg::SnapshotSaved(path) => {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into(),
                );
                self.set_info(format!("Saved snapshot {name}"));
            }
            Msg::SnapshotFailed(e) => self.set_error(format!("Could not save snapshot: {e}")),
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
        app.handle_key(ch('2'));
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
            report: report.clone(),
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
            app.handle_key(ch(view));
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
                report: fixtures::report(true),
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
            report: report.clone(),
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
            report: fixtures::report(true),
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
    fn tabs_switch_views() {
        let mut app = App::new(fixtures::profiles());
        assert_eq!(app.view, View::Connections);
        for (c, v) in ['1', '2', '3', '4', '0'].into_iter().zip([
            View::Patterns,
            View::Ttl,
            View::Diff,
            View::HotKeys,
            View::Connections,
        ]) {
            app.handle_key(ch(c));
            assert_eq!(app.view, v);
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
            report: fixtures::report(true),
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
            report: fixtures::report(true),
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
            report: fixtures::report(sizes),
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
            report: fixtures::report(true),
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
            report: fixtures::report(true),
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
            report: fixtures::report(true),
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
            report: report.clone(),
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
            report,
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
            app.handle_key(ch(v));
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
            report: report.clone(),
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

        app.handle_key(ch('2'));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ch('j'));
        assert_eq!(app.sel.ttl, 2);

        // No diff yet: nothing to move through.
        app.handle_key(ch('3'));
        app.handle_key(ch('j'));
        assert_eq!(app.sel.diff, 0);
        with_history(&mut app, id);
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.sel.diff, 3);

        app.handle_key(ch('4'));
        app.handle_key(ch('j'));
        assert_eq!(app.sel.hot, 1);

        // Patterns has its own selection too.
        app.handle_key(ch('1'));
        assert_eq!(app.patterns.sel, 0);

        app.handle_key(ch('2'));
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

        app.handle_key(ch('3'));
        assert_eq!(app.sel.diff, 3);
        app.handle_key(ch('G'));
        assert_eq!(app.sel.diff, fixtures::history(true).patterns.len() - 1);
        app.handle_key(ch('4'));
        assert_eq!(app.sel.hot, 1);
        for _ in 0..50 {
            app.handle_key(ch('j'));
        }
        assert_eq!(app.sel.hot, fixtures::hot_keys().len() - 1);

        // A new scan starts every list at the top.
        app.apply(Msg::ScanDone {
            id,
            scan: scan_of(&app),
            report: fixtures::report(true),
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
}
