//! TUI state and the pure update functions. Nothing here does IO: key presses and
//! messages change the state and return [`Effect`]s for the runtime to carry out.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use secrecy::{ExposeSecret, SecretString};

use crate::conn::docker::DiscoveredInstance;
use crate::conn::parse::{ParsedConnection, mask, parse_connection_string};
use crate::model::{
    Auth, Credentials, PermStatus, PermissionReport, Profile, ScanReport, ServerInfo,
};
use crate::scan::ScanProgress;

use super::tree::{PatternTree, Root, Row};

/// Ticks (100 ms each) an info notice stays up.
const NOTICE_TICKS: u64 = 40;
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

#[derive(Debug, Clone)]
pub enum Source {
    Saved,
    /// Found in Docker. Credentials read from the container live in memory only.
    Docker {
        container: String,
        image: String,
        credentials: Option<Credentials>,
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
    /// Connect, check permissions, then scan with default options.
    Connect {
        id: ConnId,
        profile: Profile,
        secrets: SecretSource,
    },
    DiscoverDocker,
    /// Credentials to the keychain first, then the profile to the config file.
    SaveConnection {
        profile: Profile,
        credentials: Option<Credentials>,
    },
    Quit,
}

/// Results coming back from the runtime.
#[derive(Debug)]
pub enum Msg {
    Tick,
    DockerFound(Vec<DiscoveredInstance>),
    DockerFailed(String),
    Connected {
        id: ConnId,
        server: Option<ServerInfo>,
        perms: PermissionReport,
    },
    ConnectFailed {
        id: ConnId,
        error: String,
    },
    ScanProgress {
        id: ConnId,
        progress: ScanProgress,
    },
    ScanDone {
        id: ConnId,
        report: ScanReport,
    },
    ScanFailed {
        id: ConnId,
        error: String,
    },
    Saved(Profile),
    SaveFailed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Connecting,
    Scanning,
    Done,
    Failed(String),
}

/// The connection that Patterns shows.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: ConnId,
    pub profile: Profile,
    pub server: Option<ServerInfo>,
    pub progress: Option<ScanProgress>,
    pub phase: Phase,
    pub report: Option<ScanReport>,
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
    /// Keys of collapsed groups. Groups start expanded.
    pub collapsed: HashSet<String>,
    pub sel: usize,
    pub filter: String,
    /// The filter input is open.
    pub filtering: bool,
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
    pub notice: Option<Notice>,
    pub tick: u64,
    next_id: ConnId,
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
            notice: None,
            tick: 0,
            next_id: 1,
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

    /// Rows currently visible on the Patterns view.
    pub fn pattern_rows(&self) -> Vec<Row> {
        match self.session.as_ref().and_then(|s| s.report.as_ref()) {
            Some(r) => self.patterns.tree.visible(
                &r.patterns,
                &self.patterns.collapsed,
                &self.patterns.filter,
            ),
            None => Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
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
            Some('s') if self.view != View::Connections => {
                self.set_info("Snapshots come in v0.2");
                return Vec::new();
            }
            _ => {}
        }
        match self.view {
            View::Connections => self.conn_key(key),
            View::Patterns => {
                self.patterns_key(key);
                Vec::new()
            }
            _ => Vec::new(),
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
        self.session = Some(Session {
            id,
            profile: profile.clone(),
            server: entry.server.clone(),
            progress: None,
            phase: Phase::Connecting,
            report: None,
        });
        self.patterns = PatternsState::default();
        self.view = View::Patterns;
        vec![Effect::Connect {
            id,
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
            _ => {}
        }
    }

    fn group_key(&self, root: usize) -> Option<String> {
        match self.patterns.tree.roots.get(root) {
            Some(Root::Group(g)) => Some(g.key.clone()),
            _ => None,
        }
    }

    fn expand(&mut self, rows: &[Row], sel: usize) {
        let Some(Row::Group { root }) = rows.get(sel).copied() else {
            return;
        };
        let Some(key) = self.group_key(root) else {
            return;
        };
        if !self.patterns.collapsed.remove(&key)
            && matches!(rows.get(sel + 1), Some(Row::Pattern { parent: Some(p), .. }) if *p == root)
        {
            self.patterns.sel = sel + 1;
        }
    }

    fn collapse(&mut self, rows: &[Row], sel: usize) {
        match rows.get(sel).copied() {
            Some(Row::Group { root }) => {
                if let Some(key) = self.group_key(root) {
                    self.patterns.collapsed.insert(key);
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
            && let Some(key) = self.group_key(root)
            && !self.patterns.collapsed.remove(&key)
        {
            self.patterns.collapsed.insert(key);
        }
    }

    // -----------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------

    fn is_session(&self, id: ConnId) -> bool {
        self.session.as_ref().is_some_and(|s| s.id == id)
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
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
            Msg::Connected { id, server, perms } => {
                if !self.is_session(id) {
                    return;
                }
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
                }
                if let Some(s) = self.session.as_mut() {
                    s.server = server;
                    s.phase = Phase::Scanning;
                }
            }
            Msg::ConnectFailed { id, error } => {
                if !self.is_session(id) {
                    return;
                }
                if let Some(e) = self.entry_mut(id) {
                    e.status = ConnStatus::Failed;
                    e.error = Some(error.clone());
                }
                if let Some(s) = self.session.as_mut() {
                    s.phase = Phase::Failed(error.clone());
                }
                self.set_error(format!("Could not connect: {error}"));
            }
            Msg::ScanProgress { id, progress } => {
                if let Some(s) = self.session.as_mut()
                    && s.id == id
                    && s.phase != Phase::Done
                {
                    s.progress = Some(progress);
                }
            }
            Msg::ScanDone { id, report } => {
                if !self.is_session(id) {
                    return;
                }
                if let Some(e) = self.entry_mut(id) {
                    e.server = Some(report.server.clone());
                }
                self.patterns.tree = PatternTree::build(&report.patterns, report.sizes_available);
                self.patterns.sel = 0;
                if let Some(s) = self.session.as_mut() {
                    s.server = Some(report.server.clone());
                    s.phase = Phase::Done;
                    s.report = Some(report);
                }
            }
            Msg::ScanFailed { id, error } => {
                if !self.is_session(id) {
                    return;
                }
                if let Some(s) = self.session.as_mut() {
                    s.phase = Phase::Failed(error.clone());
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

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(ch(c));
        }
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
    fn enter_connects_saved_profile_with_keychain() {
        let mut app = App::new(fixtures::profiles());
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::Connect {
                id,
                profile,
                secrets: SecretSource::Keychain,
            },
        ] = &effects[..]
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(profile.name, "staging-cache");
        assert_eq!(app.view, View::Patterns);
        assert_eq!(app.entry(*id).unwrap().status, ConnStatus::Connecting);
        assert_eq!(app.session.as_ref().unwrap().phase, Phase::Connecting);

        app.apply(Msg::Connected {
            id: *id,
            server: Some(fixtures::server()),
            perms: fixtures::perms_memory_denied(),
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
            error: "Connection refused".into(),
        });
        assert_eq!(app.entry(id).unwrap().status, ConnStatus::Failed);
        assert!(app.notice.as_ref().unwrap().error);
        // Messages for a connection that is no longer active are ignored.
        app.apply(Msg::ScanDone {
            id: 999,
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
            server: Some(fixtures::server()),
            perms: fixtures::perms_ok(),
        });
        app.apply(Msg::ScanDone {
            id,
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

    #[test]
    fn snapshot_key_shows_toast() {
        let mut app = scanned_app(true);
        app.handle_key(ch('s'));
        let n = app.notice.clone().unwrap();
        assert_eq!(n.text, "Snapshots come in v0.2");
        for _ in 0..NOTICE_TICKS {
            app.apply(Msg::Tick);
        }
        assert!(app.notice.is_none());
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
