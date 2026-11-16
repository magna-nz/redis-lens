//! `.`: the Actions menu (spec "Actions menu"). It lists the focused pane's actions from
//! the key table ([`keymap::menu`]) in three groups: the selected row's, its parent's
//! and the screen's. Each line says what its key does now: the command a write sends,
//! `asks first` when the write gate will ask, or why it is off, from the same checks
//! the key's handler makes ([`App::plan`]). Running a line closes the menu and presses
//! its key: the menu never does anything itself, so a write still goes through the
//! gate and its question. Opening it reads nothing.

use crossterm::event::{KeyCode, KeyEvent};

use super::views::{self, InFlight, Plan};
use super::{App, Effect, Screen, copies, key_of, plain_char};
use crate::tui::keymap::{self, Group, KeyHelp};
use crate::write::Decision;

/// The open menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Menu {
    /// The screen it was opened on: its lines are that screen's.
    pub screen: Screen,
    /// The selected line, counting every line of every group.
    pub sel: usize,
    /// What it was opened on ([`App::menu_target`]): the screen, the focused pane and
    /// level, and the rows its headings name. When that changes under it (a reply
    /// switched the screen, a poll moved the selection) it closes instead of acting on
    /// something else.
    pub target: u64,
}

/// Said when the menu closes because what it was opened on changed.
pub const SCREEN_CHANGED: &str = "The screen changed, so the Actions menu closed.";

/// What a line's key does now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// It runs, and sends no write. At the right: nothing, or a note (`→ MessagePack`,
    /// the format `f` switches to).
    Ready(Option<String>),
    /// It sends this write now, without asking: the command's name (`HDEL`).
    Sends(String),
    /// It opens a form; what the form sends (`HSET`, `EXPIRE / PERSIST`, or `opens a
    /// form`), and whether its submit asks first.
    Form { sends: String, asks: bool },
    /// It opens a question (the write gate's, a bulk action's or Monitor's) first.
    Asks,
    /// It does nothing now: why, as the key would say it.
    Off(String),
}

/// One line of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuLine {
    pub group: Group,
    pub key: &'static KeyHelp,
    /// The action's name: the table's label, or what the key does on this row
    /// ([`App::menu_label`]).
    pub label: String,
    pub status: Status,
}

impl MenuLine {
    /// The key it presses: the first of its row's keys.
    pub fn token(&self) -> &'static str {
        keymap::first_key(self.key)
    }
}

/// What a form opened with `op` as its gate probe can send, for the right of its line:
/// the TTL forms set or remove one, and the new-key form's command depends on the type
/// picked in it.
fn form_sends(op: &crate::write::WriteOp) -> String {
    use crate::write::WriteOp;
    match op {
        WriteOp::KeyTtl { .. } => "EXPIRE / PERSIST".into(),
        WriteOp::FieldTtl { .. } => "HEXPIRE / HPERSIST".into(),
        WriteOp::NewKey { .. } => "opens a form".into(),
        op => op.command(),
    }
}

/// The groups in the order the menu shows them.
pub const GROUPS: [Group; 3] = [Group::Row, Group::Parent, Group::Screen];

/// The short form of an off reason for the right of a line: `read-only`, `needs Redis
/// 7.4`, `the server refused CLIENT KILL`, `no analysis yet`, `nothing selected`, or
/// the reason's first sentence. `enter` on the line shows the whole reason.
pub fn short_reason(why: &str) -> String {
    let lower = why.to_lowercase();
    if lower.contains("is read-only") {
        return "read-only".into();
    }
    if let Some(rest) = why.split("the server refused ").nth(1) {
        let cmd = rest.split(" for this user").next().unwrap_or(rest);
        return format!("the server refused {}", cmd.trim_end_matches('.'));
    }
    if lower.contains("redis 7.4") {
        return "needs Redis 7.4".into();
    }
    if lower.contains("no search module") {
        return "no Search module".into();
    }
    if lower.contains("nothing to save yet") || lower.contains("press a to analyze") {
        return "no analysis yet".into();
    }
    if lower.starts_with("run a query with / first") {
        return "run a query with / first".into();
    }
    if lower.contains("come from the analysis") {
        return "from the analysis".into();
    }
    if lower.contains("connect first") {
        return "not connected".into();
    }
    if lower.contains("live connection is not open") {
        return "live connection not open".into();
    }
    if lower.contains("selected") && (lower.starts_with("no ") || lower.starts_with("nothing")) {
        return "nothing selected".into();
    }
    let text = why.strip_prefix("Not sent: ").unwrap_or(why);
    let first = text
        .split(". ")
        .next()
        .unwrap_or(text)
        .split(": ")
        .next()
        .unwrap_or(text)
        .trim_end_matches(['.', '…']);
    lower_first_word(first)
}

/// `first` with its first word lower-cased when it is an ordinary word (`No key` →
/// `no key`); a command (`MONITOR`), a key name (`t`, `D`) or anything else stays.
fn lower_first_word(text: &str) -> String {
    let word = text.split(' ').next().unwrap_or("");
    let mut chars = word.chars();
    let ordinary = chars.next().is_some_and(char::is_uppercase)
        && word.chars().count() > 1
        && chars.all(char::is_lowercase);
    if !ordinary {
        return text.to_string();
    }
    let mut c = text.chars();
    c.next()
        .map(|f| f.to_lowercase().chain(c).collect())
        .unwrap_or_default()
}

impl App {
    /// `.`: opens the menu of the screen on show.
    pub(super) fn open_menu(&mut self) -> Vec<Effect> {
        let screen = self.screen();
        self.menu = Some(Menu {
            screen,
            sel: 0,
            target: self.menu_target(screen),
        });
        Vec::new()
    }

    /// What the menu acts on, as a number to compare: the screen, the focused pane and
    /// level, and the rows the headings name.
    pub fn menu_target(&self, screen: Screen) -> u64 {
        use std::hash::{Hash, Hasher};
        let v = &self.views;
        let level = format!(
            "{:?}",
            (
                v.value.focus,
                v.keys.find.is_some(),
                v.server.clients.drill.is_some(),
                (v.streams.focus, v.streams.entries_open),
                (
                    v.search.focus,
                    v.search.deeper.as_ref().map(std::mem::discriminant)
                ),
                &self.patterns.node,
            )
        );
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!(
            "{screen:?} {:?} {:?}",
            self.screen(),
            self.menu_heads(screen)
        )
        .hash(&mut h);
        level.hash(&mut h);
        h.finish()
    }

    /// Closes the menu when what it was opened on changed, saying so.
    pub(super) fn check_menu(&mut self) {
        if let Some(m) = self.menu
            && self.menu_target(m.screen) != m.target
        {
            self.close_menu_changed();
        }
    }

    /// Closes an open menu, saying the screen changed.
    /// What the change itself said (`Deleted user:1.`) stays, before it.
    pub(super) fn close_menu_changed(&mut self) {
        if self.menu.take().is_none() {
            return;
        }
        match self.notice.as_mut() {
            Some(n) if !n.text.contains(SCREEN_CHANGED) => {
                n.text = format!("{} {SCREEN_CHANGED}", n.text);
            }
            Some(_) => {}
            None => self.set_info(SCREEN_CHANGED),
        }
    }

    /// The lines of the open menu (or of the screen on show), in group order.
    pub fn menu_lines(&self) -> Vec<MenuLine> {
        let screen = self.menu.map_or_else(|| self.screen(), |m| m.screen);
        let rows: Vec<(Group, &'static KeyHelp)> = GROUPS
            .iter()
            .flat_map(|&g| keymap::menu(screen, g).into_iter().map(move |h| (g, h)))
            .collect();
        GROUPS
            .iter()
            .flat_map(|&g| {
                rows.iter()
                    .filter(move |(listed, h)| self.menu_group(screen, h, *listed) == Some(g))
                    .map(move |&(_, h)| MenuLine {
                        group: g,
                        key: h,
                        label: self.menu_label(screen, h),
                        status: self.status_of(self.plan(screen, keymap::first_key(h))),
                    })
            })
            .collect()
    }

    /// The group a key-table row is listed in now: its own, or the one of what it acts
    /// on in this state (Search's `f` and `y` act on an open result; Streams' `c` and
    /// `A` on the selected group, consumer or pending entry). `None`: not listed now.
    fn menu_group(&self, screen: Screen, h: &KeyHelp, listed: Group) -> Option<Group> {
        let t = keymap::first_key(h);
        let v = &self.views;
        match screen {
            Screen::Search => {
                use views::search::Deeper;
                let doc = matches!(v.search.deeper, Some(Deeper::Document(_)));
                match t {
                    "f" | "y" if doc => Some(Group::Row),
                    "f" | "y" => Some(Group::Screen),
                    "enter" if v.search.deeper.is_some() => None,
                    "d" | "enter" if doc => Some(Group::Parent),
                    "d" | "enter" => Some(Group::Row),
                    _ => Some(listed),
                }
            }
            Screen::Streams => {
                let entries = v.streams.shown_entries().is_some();
                let detail = v.streams.detail_selected().is_some();
                match t {
                    "f" | "y" if entries => Some(Group::Row),
                    "f" | "y" => Some(Group::Screen),
                    "c" | "A" | "-" | "enter" if entries => Some(Group::Parent),
                    "-" | "enter" if detail => Some(Group::Parent),
                    "c" | "A" | "-" | "enter" => Some(Group::Row),
                    _ => Some(listed),
                }
            }
            // In a group's connections `enter` does nothing: it lists a group's.
            Screen::Clients if t == "enter" && v.server.clients.drill.is_some() => None,
            _ => Some(listed),
        }
    }

    /// A line's name: the table's label, or what the key does on the row selected.
    pub fn menu_label(&self, screen: Screen, h: &KeyHelp) -> String {
        use views::streams::DetailRow;
        match (screen, keymap::first_key(h)) {
            (_, "v") => "More columns".into(),
            (Screen::Streams, "A") => match self.views.streams.detail_selected() {
                Some(DetailRow::Group(_) | DetailRow::Consumer(..)) => "Pending entries".into(),
                _ => h.label.to_string(),
            },
            _ => h.label.to_string(),
        }
    }

    /// A key while the menu is open: it takes every key. `↑↓` `j` `k` move, `enter`
    /// runs the selected line, a line's own key runs it, `esc` or `.` close.
    pub(super) fn menu_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(m) = self.menu else {
            return Vec::new();
        };
        let lines = self.menu_lines();
        let last = lines.len().saturating_sub(1);
        match (key.code, plain_char(&key)) {
            (KeyCode::Esc, _) | (_, Some('.')) => self.menu = None,
            (KeyCode::Up, _) | (_, Some('k')) => {
                self.menu = Some(Menu {
                    sel: m.sel.saturating_sub(1),
                    ..m
                });
            }
            (KeyCode::Down, _) | (_, Some('j')) => {
                self.menu = Some(Menu {
                    sel: (m.sel + 1).min(last),
                    ..m
                });
            }
            (KeyCode::Home, _) => self.menu = Some(Menu { sel: 0, ..m }),
            (KeyCode::End, _) => self.menu = Some(Menu { sel: last, ..m }),
            (KeyCode::Enter, _) => {
                if let Some(line) = lines.get(m.sel.min(last)) {
                    return self.run_line(line.clone());
                }
            }
            _ => {
                if let Some(line) = lines
                    .iter()
                    .find(|l| key_of(l.token()).is_some_and(|k| same_key(&k, &key)))
                {
                    return self.run_line(line.clone());
                }
            }
        }
        Vec::new()
    }

    /// Closes the menu and presses `line`'s key on the screen, through every check
    /// that key meets (the write gate's included). A line that is off runs nothing:
    /// its reason shows instead.
    fn run_line(&mut self, line: MenuLine) -> Vec<Effect> {
        // Never on anything but what the menu was opened on.
        self.check_menu();
        if self.menu.is_none() {
            return Vec::new();
        }
        self.menu = None;
        if let Status::Off(why) = &line.status {
            self.set_info(why.clone());
            return Vec::new();
        }
        match key_of(line.token()) {
            Some(k) => self.key_inner(k),
            None => Vec::new(),
        }
    }

    /// What a line's [`Plan`] comes to, for its right side.
    fn status_of(&self, plan: Plan) -> Status {
        let sends = matches!(plan, Plan::Write(_));
        let form = !sends && matches!(plan, Plan::Form(_));
        match plan {
            Plan::Run(note) => Status::Ready(note),
            Plan::Asks => Status::Asks,
            Plan::Off(why) => Status::Off(why),
            Plan::Write(op) | Plan::Form(op) => {
                let Some(s) = self.session.as_ref() else {
                    return Status::Off(
                        "Nothing to change: connect first (0 Connections, then enter).".into(),
                    );
                };
                // Sent now: where it goes and the gate, as `Act::write` checks them.
                // Before a form: the gate alone, as the handler checks it.
                let decision = if sends {
                    if !self.ctx().is_some_and(|c| c.live_ready()) {
                        return Status::Off("Not sent: the live connection is not open.".into());
                    }
                    views::write_plan(s, &self.settings, &op)
                } else {
                    match crate::write::decide_op(&op, &s.profile, &self.settings, &s.refusals) {
                        Decision::Refuse(why) => Err(why),
                        d => Ok(d),
                    }
                };
                match decision {
                    Err(why) => Status::Off(why),
                    _ if form => Status::Form {
                        sends: form_sends(&op),
                        asks: !matches!(decision, Ok(Decision::Run)),
                    },
                    Ok(Decision::Run) => Status::Sends(op.command()),
                    Ok(_) => Status::Asks,
                }
            }
        }
    }

    /// What key-table key `token` would do on `screen` now, from the checks its
    /// handler makes: `App`'s own keys first, as [`App::key_inner`] takes them, then
    /// the view state's ([`views::ViewState::plan`]).
    pub(super) fn plan(&self, screen: Screen, token: &str) -> Plan {
        let off = |why: &str| Plan::Off(why.to_string());
        let report = self.report().is_some();
        match token {
            "ctrl-s" if screen.is_scan_view() => {
                return self.snapshot_block().map_or(Plan::Run(None), off);
            }
            "v" => return Plan::Run(None),
            "a" if screen.analyzes() || (screen == Screen::Streams && !report) => {
                return self.analyze_block().map_or(Plan::Run(None), off);
            }
            "a" if screen == Screen::Streams => {
                return off("The streams come from the analysis: a on 1 Overview analyzes again.");
            }
            "r" if screen == Screen::Streams && !report => {
                return off("Nothing to refresh before an analysis: a analyzes the keys.");
            }
            "r" if screen.is_live_view() => {
                if let Some(why) = self.refresh_block() {
                    return Plan::Off(why);
                }
            }
            "y" | "Y" if copies(screen) => {
                return match self.copy_pick(token == "Y") {
                    Some(Err(why)) => off(why),
                    _ => Plan::Run(None),
                };
            }
            _ => {}
        }
        match (screen, token) {
            (Screen::Overview, "esc") if !self.analysis_running() => {
                return off("No analysis is running.");
            }
            (Screen::Patterns, "m") => {
                return match self
                    .more_names_block()
                    .map(str::to_string)
                    .or_else(|| self.patterns.names.more_block())
                {
                    Some(why) => Plan::Off(why),
                    None => Plan::Run(None),
                };
            }
            (Screen::Patterns, "t" | "d") => {
                return match self.bulk_pattern() {
                    Err(why) => off(why),
                    Ok(p) => self.bulk_status(&p, token == "d"),
                };
            }
            (Screen::KeyList, "T" | "D") => {
                return match self.key_list_pattern() {
                    Err(why) => off(why),
                    Ok(p) => self.bulk_status(&p, token == "D"),
                };
            }
            _ => {}
        }
        let Some(ctx) = self.ctx() else {
            return Plan::Run(None);
        };
        let Some(owner) = self.active_owner() else {
            return Plan::Run(None);
        };
        let pending = &self.live_pending;
        let in_flight: InFlight<'_> = &|id| pending.contains_key(&id);
        self.views
            .get(owner)
            .plan(token, &ctx, in_flight)
            .unwrap_or(Plan::Run(None))
    }

    /// A bulk action's plan: the op it asks about, or why it can't start.
    fn bulk_status(&self, pattern: &str, delete: bool) -> Plan {
        match self.bulk_plan(pattern, delete) {
            Ok((op, _)) => Plan::Form(op),
            Err((why, _)) => Plan::Off(why),
        }
    }

    /// What the groups are about, for their headings: a kind (`FIELD`, `KEY`,
    /// `PATTERN` …) and its name, safe to draw. Empty name: nothing selected.
    pub fn menu_heads(&self, screen: Screen) -> [(&'static str, String); 3] {
        use crate::live::values::Items;
        let esc = |s: &str| crate::value::escape(s);
        let raw = |r: &crate::value::Raw| esc(&String::from_utf8_lossy(r.as_bytes()));
        let v = &self.views;
        let key_name = || {
            v.value
                .row
                .as_ref()
                .map(|r| esc(&r.key))
                .unwrap_or_default()
        };
        let list_parent = || match (&v.keys.find, &v.keys.pattern) {
            (None, Some(p)) => ("PATTERN", esc(p)),
            _ => ("FIND", String::new()),
        };
        let screen_head = ("SCREEN", screen.name().to_string());
        let (row, parent) = match screen {
            Screen::Connections => (
                (
                    "CONNECTION",
                    self.selected_entry()
                        .map(|e| esc(&e.profile.name))
                        .unwrap_or_default(),
                ),
                ("", String::new()),
            ),
            Screen::Patterns | Screen::Ttl | Screen::Diff => (
                (
                    "PATTERN",
                    match screen {
                        Screen::Patterns => self.selected_pattern_label(),
                        Screen::Ttl => self.selected_ttl_pattern(),
                        _ => self.selected_diff_pattern(),
                    }
                    .map(|p| esc(&p))
                    .unwrap_or_default(),
                ),
                ("", String::new()),
            ),
            Screen::HotKeys => (
                (
                    "KEY",
                    self.selected_hot_key().map(|k| esc(&k)).unwrap_or_default(),
                ),
                ("", String::new()),
            ),
            Screen::KeyList => (
                (
                    "KEY",
                    v.keys.selected().map(|r| esc(&r.key)).unwrap_or_default(),
                ),
                list_parent(),
            ),
            Screen::KeyDetail => {
                let i = v.value.selected();
                let row = match &v.value.items {
                    Some(Items::Hash(f)) => ("FIELD", f.get(i).map(|f| raw(&f.name))),
                    Some(Items::List(e)) => ("ELEMENT", e.get(i).map(|_| format!("#{i}"))),
                    Some(Items::Set(m)) => ("MEMBER", m.get(i).map(raw)),
                    Some(Items::ZSet(m)) => ("MEMBER", m.get(i).map(|m| raw(&m.member))),
                    Some(Items::Stream(e)) => ("ENTRY", e.get(i).map(|e| esc(&e.id))),
                    _ => ("VALUE", Some(key_name())),
                };
                ((row.0, row.1.unwrap_or_default()), ("KEY", key_name()))
            }
            Screen::Streams => {
                use views::streams::DetailRow;
                let stream = (
                    "STREAM",
                    v.streams
                        .selected()
                        .map(|r| esc(&r.key))
                        .unwrap_or_default(),
                );
                if let Some(e) = v.streams.shown_entries() {
                    let id = match &e.items {
                        Some(Items::Stream(es)) => es.get(e.selected()).map(|x| esc(&x.id)),
                        _ => None,
                    };
                    (("ENTRY", id.unwrap_or_default()), stream)
                } else {
                    match v.streams.detail_selected() {
                        Some(DetailRow::Group(g)) => (("GROUP", esc(&g.name)), stream),
                        Some(DetailRow::Consumer(_, c)) => (("CONSUMER", esc(&c.name)), stream),
                        Some(DetailRow::Pending(_, p)) => (("ENTRY", esc(&p.id)), stream),
                        None => (stream, ("", String::new())),
                    }
                }
            }
            Screen::Search => {
                let index = (
                    "INDEX",
                    v.search
                        .selected()
                        .map(|i| esc(&i.name))
                        .unwrap_or_default(),
                );
                match &v.search.deeper {
                    Some(views::search::Deeper::Document(doc)) => (
                        (
                            "RESULT",
                            doc.key_name().map(|k| esc(&k)).unwrap_or_default(),
                        ),
                        index,
                    ),
                    _ => (index, ("", String::new())),
                }
            }
            Screen::Clients => (
                match v.server.clients.drill {
                    Some(_) => (
                        "CLIENT",
                        v.server
                            .clients
                            .selected_conn()
                            .map(|c| c.id.to_string())
                            .unwrap_or_default(),
                    ),
                    None => (
                        "GROUP",
                        v.server
                            .clients
                            .selected_group()
                            .map(|g| esc(g.key.label()))
                            .unwrap_or_default(),
                    ),
                },
                ("", String::new()),
            ),
            Screen::SlowLog => (
                (
                    "ENTRY",
                    v.server
                        .slow
                        .selected()
                        .map(|e| format!("#{}", e.id))
                        .unwrap_or_default(),
                ),
                ("SLOW LOG", String::new()),
            ),
            Screen::Monitor => {
                let m = &v.server.monitor;
                let line = m.lines.get(m.sel).map(|e| esc(&e.command_line()));
                (("LINE", line.unwrap_or_default()), ("", String::new()))
            }
            Screen::PubSub => {
                use views::pubsub::Selected;
                let name = match v.pubsub.selected() {
                    Some(Selected::Channel(c)) => esc(&c.name),
                    Some(Selected::Shard(r)) => esc(&r.name),
                    None => String::new(),
                };
                (("CHANNEL", name), ("", String::new()))
            }
            Screen::BigKeys => {
                let keys = self.report().map_or(&[][..], |r| r.big_keys());
                let sel = v.analysis.sel.min(keys.len().saturating_sub(1));
                let key = keys.get(sel).map(|k| esc(&k.key)).unwrap_or_default();
                (("KEY", key), ("", String::new()))
            }
            Screen::Overview => {
                let f = views::overview::findings(self);
                let text = f
                    .get(self.overview.sel)
                    .map(|f| esc(&f.text))
                    .unwrap_or_default();
                (("FINDING", text), ("", String::new()))
            }
            Screen::Nodes => {
                let node = self
                    .nodes()
                    .and_then(|n| n.get(self.sel.nodes))
                    .map(|n| n.endpoint.to_string())
                    .unwrap_or_default();
                (("NODE", node), ("", String::new()))
            }
            _ => (("ROW", String::new()), ("", String::new())),
        };
        [row, parent, screen_head]
    }
}

/// The same key, ignoring `shift` on a character (`D` comes with it or without).
fn same_key(a: &KeyEvent, b: &KeyEvent) -> bool {
    use crossterm::event::KeyModifiers as M;
    let mods = |k: &KeyEvent| k.modifiers & !M::SHIFT;
    a.code == b.code && mods(a) == mods(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_reasons_keep_commands_and_key_names() {
        assert_eq!(
            short_reason("MONITOR is not running."),
            "MONITOR is not running"
        );
        assert_eq!(
            short_reason("t and D work on a key."),
            "t and D work on a key"
        );
        assert_eq!(short_reason("No key is open."), "no key is open");
        assert_eq!(
            short_reason("Select a pattern first."),
            "select a pattern first"
        );
        assert_eq!(
            short_reason("Writes act on the stream: esc back."),
            "writes act on the stream"
        );
        assert_eq!(
            short_reason("prod-cache-01 is read-only: w turns writes on."),
            "read-only"
        );
    }
}
