//! `ctrl-k`: the Go to box (spec "Go to"). One input, and under it the rows of what
//! the text names, in groups: PATTERNS of the analysis (or of the names read so far),
//! KEYS already loaded and a last row that runs Find key with the text, SCREENS (every
//! area and panel with the keys that reach it) and ACTIONS (the Actions menu's lines
//! of the screen on show). Text that starts with `:` is one row that opens the command
//! line with the rest in its input.
//!
//! Typing reads nothing: the rows come from what the app holds already, rebuilt from
//! it every time they are asked for. Choosing a row closes the box and runs the code
//! its own screen runs (`switch`, `→` on a pattern, the key list's open, Find key, the
//! Actions menu's line, `:`), so Go to builds no request and sends no write itself. A
//! line that writes opens the Actions menu on it instead of running.
//!
//! `enter` runs the row drawn as picked, by identity ([`GoTo::pick`]): when that row is
//! no longer there it runs nothing, and never another row in its place.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::actions::{Menu, MenuLine, Status};
use super::views;
use super::{App, Area, Effect, Panel, Screen};
use crate::model::KeyType;
use crate::tui::tree::{Root, Row};
use crate::tui::ui::CONNECT_FIRST;
use crate::verdict::PatternVerdict;

/// The longest text, in bytes: Find key's.
pub const MAX_TEXT: usize = super::find::MAX_FIND;

/// Said when the box closes because the screen changed under it.
pub const SCREEN_CHANGED: &str = "The screen changed, so Go to closed.";

/// Why an ACTIONS row is off once what the box was opened on changed.
pub const ACTIONS_MOVED: &str =
    "The selection changed: close and reopen Go to (ctrl-k) to act on the new one.";

/// The ACTIONS heading once what the box was opened on changed.
pub const ACTIONS_MOVED_HEAD: &str = "the selection changed: close and reopen";

/// Said in the box when the row picked is no longer there.
pub const GONE: &str = "That row is gone: pick again.";

/// Said in the box while no row is picked.
pub const PICK_A_ROW: &str = "Nothing is picked: ↑↓ to pick a row.";

/// Said after a row had to clear a filter to show what it names.
pub const CLEARED_FILTER: &str = "Cleared the filter to show it.";

/// Said after a pattern row had to expand a group to show its pattern.
pub const EXPANDED_GROUP: &str = "Expanded its group to show it.";

/// `ctrl-k`.
pub fn is_goto(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('k' | 'K')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// The text after the `:` of a command, the spaces before the `:` left out; `None`
/// when the text is not one.
pub fn command_text(text: &str) -> Option<&str> {
    text.trim_start().strip_prefix(':')
}

/// The open box.
#[derive(Clone, PartialEq, Eq)]
pub struct GoTo {
    /// What is typed. After a `:` it can hold a credential: drawn only masked.
    pub text: String,
    /// The screen it was opened on: it closes when another one shows.
    pub screen: Screen,
    /// What its ACTIONS rows act on ([`App::menu_target`]), recorded once when it
    /// opened. Once that changes the ACTIONS rows are off.
    pub target: u64,
    /// The name its ACTIONS heading gives what they act on (`user:10293:cart`), as of
    /// opening; empty when there is none.
    pub on: String,
    /// The row drawn as picked, by identity: what `enter` runs. `None`: nothing is.
    /// Set to the default each time the text changes, and by `↑↓` and `tab`; never
    /// changed by anything else.
    pub pick: Option<HitId>,
    /// Said in the box's key line: the picked row went, or nothing is picked.
    pub say: Option<&'static str>,
}

/// The text's length only: after a `:` it can hold a password.
impl fmt::Debug for GoTo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoTo")
            .field("text_bytes", &self.text.len())
            .field("screen", &self.screen)
            .field("target", &self.target)
            .field("pick", &self.pick.as_ref().map(|s| s.0))
            .field("say", &self.say)
            .finish_non_exhaustive()
    }
}

/// The groups, in the order they are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// The one `:` row.
    Command,
    Patterns,
    Keys,
    Screens,
    Actions,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Self::Command => "COMMAND",
            Self::Patterns => "PATTERNS",
            Self::Keys => "KEYS",
            Self::Screens => "SCREENS",
            Self::Actions => "ACTIONS",
        }
    }
}

/// How well a row's name matches the text, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// The whole name.
    Exact,
    /// The start of the name.
    Prefix,
    /// The start of a word in it (after a space or punctuation).
    WordStart,
    /// Anywhere in it.
    Substring,
}

/// Where the text matched a name: its rank, and the matched characters (a range of
/// `char` indices into the name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub rank: Rank,
    pub start: usize,
    pub len: usize,
}

/// Where `needle` is in `hay`, ignoring case, at its best rank: the whole of it, its
/// start, a word's start, or anywhere. `None` when it is not there or `needle` is
/// empty.
pub fn match_text(hay: &str, needle: &str) -> Option<Match> {
    let fold = |s: &str| -> Vec<char> {
        s.chars()
            .map(|c| c.to_lowercase().next().unwrap_or(c))
            .collect()
    };
    let h = fold(hay);
    let n = fold(needle);
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    let at = |i: usize| h[i..i + n.len()] == n[..];
    if at(0) {
        let rank = if h.len() == n.len() {
            Rank::Exact
        } else {
            Rank::Prefix
        };
        return Some(Match {
            rank,
            start: 0,
            len: n.len(),
        });
    }
    let starts = (1..=h.len() - n.len()).filter(|&i| at(i));
    let mut first = None;
    for i in starts {
        if !h[i - 1].is_alphanumeric() {
            return Some(Match {
                rank: Rank::WordStart,
                start: i,
                len: n.len(),
            });
        }
        first.get_or_insert(i);
    }
    first.map(|start| Match {
        rank: Rank::Substring,
        start,
        len: n.len(),
    })
}

/// The best of two matches of the same row (a screen's `Area › Panel` and its panel
/// name alone): the better rank, the earlier one on a tie.
fn better(a: Option<Match>, b: Option<Match>) -> Option<Match> {
    match (a, b) {
        (Some(a), Some(b)) if b.rank < a.rank => Some(b),
        (a, b) => a.or(b),
    }
}

/// Where a KEYS row's key is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFrom {
    /// The key list on Browse › Keys: a pattern's keys or a find's results.
    List,
    /// The key whose value is open.
    Value,
    /// The analysis's big keys, at this index.
    BigKeys(usize),
    /// The analysis's hot keys, at this index.
    HotKeys(usize),
}

/// What `enter` on a row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Go {
    /// `→` on this pattern on Browse › Keys.
    Pattern(String),
    /// Opens this key the way the screen that holds it opens it.
    Key { name: String, from: KeyFrom },
    /// Find key with the text.
    Find,
    /// Switches to this screen, as its digit keys do.
    Screen(Screen),
    /// The Actions menu's line.
    Action(MenuLine),
    /// The command line, the rest of the text in its input.
    Command,
}

/// A row's identity: its group and what it names (a pattern, a key, a screen, an
/// action's key and name). What `enter` runs is found by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitId(pub Section, pub String);

/// One row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub section: Section,
    /// The name as drawn: escaped (and masked after a `:`).
    pub label: String,
    /// The matched characters of `label`, for the focus colour.
    pub matched: Option<(usize, usize)>,
    pub rank: Option<Rank>,
    /// At the right, dim: counts, a type and size.
    pub note: String,
    /// At the right, as key caps: the keys that do the same.
    pub keys: Vec<String>,
    /// Why it is off: listed dim, and `enter` only shows this.
    pub off: Option<String>,
    /// A pattern's verdict, for the `▲` mark (`None` from key names).
    pub verdict: Option<PatternVerdict>,
    /// The screen on show (a SCREENS row): never the default with empty text.
    pub here: bool,
    pub go: Go,
}

impl Hit {
    fn new(section: Section, label: String, go: Go) -> Self {
        Self {
            section,
            label,
            matched: None,
            rank: None,
            note: String::new(),
            keys: Vec::new(),
            off: None,
            verdict: None,
            here: false,
            go,
        }
    }

    pub fn id(&self) -> HitId {
        let what = match &self.go {
            Go::Pattern(p) => p.clone(),
            Go::Key { name, .. } => name.clone(),
            Go::Screen(s) => format!("{s:?}"),
            Go::Action(l) => format!("{} {}", l.token(), l.label),
            Go::Find => "find".into(),
            Go::Command => "command".into(),
        };
        HitId(self.section, what)
    }

    /// Counted in the title: everything but the Find and `:` rows.
    pub fn counts(&self) -> bool {
        !matches!(self.go, Go::Find | Go::Command)
    }

    fn matching(mut self, m: Match) -> Self {
        self.rank = Some(m.rank);
        self.matched = Some((m.start, m.len));
        self
    }
}

/// The row picked when the text changes, or `None`: nothing is.
///
/// - The `:` row, when the text is a command (it is the only row then).
/// - With no text: the first screen that is on and not the one on show.
/// - Else, in order: a screen that is on whose label or panel name the text is or
///   starts (the best match first); the first pattern; the first loaded key; any other
///   screen that is on.
///
/// Never an ACTIONS row and never the Find row: those only by `↑↓` or `tab`.
pub fn default_index(hits: &[Hit], empty: bool) -> Option<usize> {
    if let [h] = hits
        && h.section == Section::Command
    {
        return Some(0);
    }
    let on_screen = |h: &Hit| h.section == Section::Screens && h.off.is_none();
    let pick = |f: &dyn Fn(&Hit) -> bool| hits.iter().position(f);
    if empty {
        return pick(&|h| on_screen(h) && !h.here);
    }
    let close = |h: &Hit| h.rank.is_some_and(|r| r <= Rank::Prefix);
    hits.iter()
        .enumerate()
        .filter(|(_, h)| on_screen(h) && close(h))
        .min_by_key(|(i, h)| (h.rank, *i))
        .map(|(i, _)| i)
        .or_else(|| pick(&|h| h.section == Section::Patterns))
        .or_else(|| pick(&|h| matches!(h.go, Go::Key { .. })))
        .or_else(|| pick(&on_screen))
}

/// `n keys · 1.2G` for a pattern; `~n keys` from key names.
fn pattern_note(p: &crate::model::PatternStats, sizes: bool, names: bool) -> String {
    let keys = crate::format::count(p.est_keys);
    if names {
        return format!("~{keys} keys");
    }
    match p.est_bytes.filter(|_| sizes) {
        Some(b) => format!("{keys} keys · {}", crate::format::bytes(b)),
        None => format!("{keys} keys"),
    }
}

/// `hash · 3.1K`, or the type alone without a size.
fn key_note(t: KeyType, bytes: Option<u64>) -> String {
    match bytes {
        Some(b) => format!("{} · {}", t.as_str(), crate::format::bytes(b)),
        None => t.as_str().to_string(),
    }
}

impl App {
    /// `ctrl-k`: opens the box on the screen on show, recording what its ACTIONS rows
    /// act on.
    pub(super) fn open_goto(&mut self) -> Vec<Effect> {
        let screen = self.screen();
        let heads = self.menu_heads(screen);
        let on = heads[..2]
            .iter()
            .map(|(_, name)| name.clone())
            .find(|n| !n.is_empty())
            .unwrap_or_default();
        self.goto = Some(GoTo {
            text: String::new(),
            screen,
            target: self.menu_target(screen),
            on,
            pick: None,
            say: None,
        });
        self.repick_goto();
        Vec::new()
    }

    /// Closes an open box, saying the screen changed. What the change itself said
    /// stays, before it.
    pub(super) fn close_goto_changed(&mut self) {
        if self.goto.take().is_none() {
            return;
        }
        self.add_info(SCREEN_CHANGED);
    }

    /// `text` in the notice, after what it says already.
    fn add_info(&mut self, text: &str) {
        match self.notice.as_mut() {
            Some(n) if !n.text.contains(text) => {
                n.text = format!("{} {text}", n.text);
            }
            Some(_) => {}
            None => self.set_info(text),
        }
    }

    /// After every key and message: closes the box when another screen shows than the
    /// one it was opened on; when the picked row is no longer among the rows, nothing
    /// is picked any more and the box says the row is gone.
    pub(super) fn check_goto(&mut self) {
        let Some(g) = self.goto.as_ref() else {
            return;
        };
        if g.screen != self.screen() {
            self.close_goto_changed();
            return;
        }
        let Some(id) = g.pick.clone() else {
            return;
        };
        if !self.goto_hits().iter().any(|h| h.id() == id)
            && let Some(g) = self.goto.as_mut()
        {
            g.pick = None;
            g.say = Some(GONE);
        }
    }

    /// True when what the box's ACTIONS rows were built for is no longer what is on
    /// screen: they are off then.
    pub fn goto_actions_moved(&self) -> bool {
        self.goto
            .as_ref()
            .is_some_and(|g| self.target_moved(g.screen, g.target))
    }

    /// The rows for the open box's text, rebuilt from what the app holds now.
    pub fn goto_hits(&self) -> Vec<Hit> {
        self.goto
            .as_ref()
            .map_or_else(Vec::new, |g| self.goto_hits_for(&g.text))
    }

    /// The rows for `text`, in the order they are shown: matched names ranked within
    /// their group (exact, prefix, word start, anywhere), the groups in [`Section`]
    /// order. Reads nothing from the server.
    pub fn goto_hits_for(&self, text: &str) -> Vec<Hit> {
        if let Some(rest) = command_text(text) {
            let label = format!(
                "Run in the command line: {}",
                views::cmdline::masked_input(rest)
            );
            let mut hit = Hit::new(Section::Command, label, Go::Command);
            hit.keys = vec![":".into()];
            hit.off = self.cmdline_off();
            return vec![hit];
        }
        let q = text.trim();
        let mut out = Vec::new();
        if !q.is_empty() {
            out.extend(self.pattern_hits(q));
            out.extend(self.key_hits(q));
            out.push(self.find_hit(q));
        }
        out.extend(self.screen_hits(q));
        out.extend(self.action_hits(q));
        out
    }

    /// The patterns of the analysis shown (or the names read so far) whose name holds
    /// `q`.
    fn pattern_hits(&self, q: &str) -> Vec<Hit> {
        let Some(d) = self.pattern_data() else {
            return Vec::new();
        };
        let diff = self.diff();
        let mut out: Vec<Hit> = d
            .patterns
            .iter()
            .filter_map(|p| {
                let label = crate::value::escape(&p.pattern);
                let m = match_text(&label, q)?;
                let mut h =
                    Hit::new(Section::Patterns, label, Go::Pattern(p.pattern.clone())).matching(m);
                h.note = pattern_note(p, d.sizes_available, d.names);
                h.verdict = (!d.names).then(|| {
                    crate::verdict::pattern_verdict(p, diff.and_then(|x| x.pattern(&p.pattern)))
                });
                Some(h)
            })
            .collect();
        out.sort_by_key(|h| h.rank);
        out
    }

    /// The keys the app holds already whose name holds `q`: the key list's, the open
    /// value's, the analysis's big and hot keys. Each once, by where it opens best. Not
    /// the key list's or the value's while they were read on another database than the
    /// session's ([`App::list_db`]).
    fn key_hits(&self, q: &str) -> Vec<Hit> {
        let db = self.session.as_ref().map(|s| s.profile.db);
        let other_db = |d: Option<u32>| d.is_some_and(|d| Some(d) != db);
        let mut keys: Vec<(&str, KeyType, Option<u64>, KeyFrom)> = Vec::new();
        if !other_db(self.list_db) {
            for r in &self.views.keys.rows {
                keys.push((&r.key, r.key_type, r.bytes, KeyFrom::List));
            }
        }
        if let Some(r) = &self.views.value.row
            && !other_db(self.value_db)
        {
            keys.push((&r.key, r.key_type, r.bytes, KeyFrom::Value));
        }
        if let Some(r) = self.report() {
            for (i, k) in r.big_keys().iter().enumerate() {
                keys.push((&k.key, k.key_type, k.bytes, KeyFrom::BigKeys(i)));
            }
            for (i, k) in r.hot_keys.iter().enumerate() {
                keys.push((&k.key, k.key_type, k.bytes, KeyFrom::HotKeys(i)));
            }
        }
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<Hit> = keys
            .into_iter()
            .filter(|(k, ..)| seen.insert(*k))
            .filter_map(|(k, t, bytes, from)| {
                let label = crate::value::escape(k);
                let m = match_text(&label, q)?;
                let go = Go::Key {
                    name: k.to_string(),
                    from,
                };
                let mut h = Hit::new(Section::Keys, label, go).matching(m);
                h.note = key_note(t, bytes);
                Some(h)
            })
            .collect();
        out.sort_by_key(|h| h.rank);
        out
    }

    /// `Find every key matching <glob>`: Find key with `q`, the glob shown the one it
    /// sends ([`crate::live::keys::find_glob`]).
    fn find_hit(&self, q: &str) -> Hit {
        let shown = crate::value::escape(&crate::live::keys::find_glob(q));
        let lead = "Find every key matching ";
        let start = lead.chars().count();
        let len = shown.chars().count();
        let mut h = Hit::new(Section::Keys, format!("{lead}{shown}"), Go::Find);
        h.matched = Some((start, len));
        h.keys = vec!["ctrl-f".into()];
        if self.session.is_none() {
            h.off = Some(CONNECT_FIRST.trim_start().into());
        }
        h
    }

    /// Every area and panel of this session, with the keys that reach each from here,
    /// whose `Area › Panel` or panel name holds `q` (all of them for empty `q`).
    fn screen_hits(&self, q: &str) -> Vec<Hit> {
        let here = self.screen();
        let order = [
            Area::Overview,
            Area::Browse,
            Area::Analyze,
            Area::Server,
            Area::PubSub,
            Area::Connections,
        ];
        let mut out = Vec::new();
        for area in order {
            let panels = self.panels_of(area);
            let rows: Vec<(String, Option<Panel>)> = if panels.is_empty() {
                vec![(area.title().to_string(), None)]
            } else {
                panels
                    .iter()
                    .map(|p| (format!("{} › {}", area.title(), p.name()), Some(*p)))
                    .collect()
            };
            for (label, panel) in rows {
                let to = match panel {
                    Some(p) => self.screen_of(p),
                    None => self.screen_of_area(area),
                };
                let m = if q.is_empty() {
                    None
                } else {
                    let whole = match_text(&label, q);
                    let alone = panel.and_then(|p| {
                        let at = label.chars().count() - p.name().chars().count();
                        match_text(p.name(), q).map(|m| Match {
                            start: m.start + at,
                            ..m
                        })
                    });
                    match better(whole, alone) {
                        Some(m) => Some(m),
                        None => continue,
                    }
                };
                let mut h = Hit::new(Section::Screens, label, Go::Screen(to));
                if let Some(m) = m {
                    h = h.matching(m);
                }
                h.here = match panel {
                    Some(p) => here.panel() == Some(p),
                    None => here.area() == area,
                };
                h.off = self.screen_off(area, panel);
                if h.here {
                    h.note = "here".into();
                } else {
                    h.keys = self.keys_to(area, panel);
                }
                out.push(h);
            }
        }
        out.sort_by_key(|h| h.rank);
        out
    }

    /// The keys that reach `panel` of `area` (or `area`) from the screen on show: its
    /// digit, then the digit again once per panel between (`3 3`).
    fn keys_to(&self, area: Area, panel: Option<Panel>) -> Vec<String> {
        let d = area.digit().to_string();
        let Some(panel) = panel else {
            return vec![d];
        };
        let panels = self.panels_of(area);
        let n = panels.len();
        let pos = |p: Panel| panels.iter().position(|x| *x == p).unwrap_or(0);
        let to = pos(panel);
        let presses = if self.area == area {
            let from = self.screen().panel().map_or(0, pos);
            (to + n - from) % n
        } else {
            1 + (to + n - pos(self.shown_panel(area))) % n
        };
        vec![d; presses]
    }

    /// Why a SCREENS row is off: before connecting every area but Connections; once
    /// connected, a panel whose permission probe failed or whose view turned itself
    /// off, with the view's own reason.
    fn screen_off(&self, area: Area, panel: Option<Panel>) -> Option<String> {
        if area == Area::Connections {
            return None;
        }
        let Some(s) = self.session.as_ref() else {
            return Some(CONNECT_FIRST.trim_start().into());
        };
        let screen = panel.map_or_else(|| self.screen_of_area(area), Panel::top);
        let perms = self.entry(s.id).and_then(|e| e.perms.as_ref());
        if let Some(why) = views::probe_off(perms, screen) {
            return Some(why);
        }
        let v = &self.views;
        match screen {
            Screen::Streams => v.streams.off.clone(),
            Screen::Search if v.search.no_module => {
                Some("The server has no search module, so this view is off.".into())
            }
            Screen::Search => v.search.off.clone(),
            Screen::PubSub => v.pubsub.off.clone(),
            Screen::Slots => v.slots.off.as_ref().map(|o| o.text()),
            Screen::Clients => v.server.clients.poll.off.clone(),
            Screen::SlowLog => v.server.slow.poll.off.clone(),
            _ => None,
        }
    }

    /// The Actions menu's lines of the screen on show whose name holds `q`, with their
    /// keys, or why they are off. All off once what the box was opened on changed.
    fn action_hits(&self, q: &str) -> Vec<Hit> {
        let moved = self.goto_actions_moved();
        let mut out: Vec<Hit> = self
            .menu_lines()
            .into_iter()
            .filter_map(|l| {
                let m = if q.is_empty() {
                    None
                } else {
                    Some(match_text(&l.label, q)?)
                };
                let mut h = Hit::new(Section::Actions, l.label.clone(), Go::Action(l.clone()));
                if let Some(m) = m {
                    h = h.matching(m);
                }
                match &l.status {
                    _ if moved => h.off = Some(ACTIONS_MOVED.into()),
                    Status::Off(why) => h.off = Some(why.clone()),
                    _ => h.keys = vec![l.key.footer_keys.to_string()],
                }
                Some(h)
            })
            .collect();
        out.sort_by_key(|h| h.rank);
        out
    }

    /// The index among `hits` of the row picked, while it is there.
    pub fn goto_index(&self, hits: &[Hit]) -> Option<usize> {
        let id = self.goto.as_ref()?.pick.as_ref()?;
        hits.iter().position(|h| &h.id() == id)
    }

    /// The text changed: the default row ([`default_index`]) is the one picked.
    fn repick_goto(&mut self) {
        let hits = self.goto_hits();
        let connected = self.session.is_some();
        let Some(g) = self.goto.as_mut() else {
            return;
        };
        let empty = g.text.trim().is_empty();
        // Before connecting there is no default with no text: the selected entry's
        // Connect is no more than a `↓` away.
        g.pick = if empty && !connected {
            None
        } else {
            default_index(&hits, empty).map(|i| hits[i].id())
        };
        g.say = None;
    }

    /// A key while the box is open: it takes every key. Printable keys are text.
    pub(super) fn goto_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(g) = self.goto.as_mut() else {
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc if !g.text.is_empty() => g.text.clear(),
            KeyCode::Esc => {
                self.goto = None;
                return Vec::new();
            }
            KeyCode::Char('k' | 'K') if ctrl => {
                self.goto = None;
                return Vec::new();
            }
            KeyCode::Char('u') if ctrl => g.text.clear(),
            KeyCode::Backspace => {
                g.text.pop();
            }
            KeyCode::Enter => return self.goto_enter(None),
            // The Find row's own key.
            KeyCode::Char('f' | 'F') if ctrl => return self.goto_enter(Some(Go::Find)),
            KeyCode::Up | KeyCode::Down => {
                self.goto_move(key.code == KeyCode::Up);
                return Vec::new();
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.goto_jump(key.code == KeyCode::BackTab);
                return Vec::new();
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                push(&mut g.text, c);
            }
            _ => return Vec::new(),
        }
        self.repick_goto();
        Vec::new()
    }

    /// Pasted text, typed as the command line takes a paste
    /// ([`views::cmdline::paste_char`]): tabs and newlines as spaces, other control
    /// characters left out.
    pub(super) fn goto_paste(&mut self, text: &str) {
        let Some(g) = self.goto.as_mut() else {
            return;
        };
        for c in text.chars().filter_map(views::cmdline::paste_char) {
            if !push(&mut g.text, c) {
                break;
            }
        }
        self.repick_goto();
    }

    /// Picks row `to` of `hits`.
    fn goto_pick(&mut self, hits: &[Hit], to: usize) {
        if let (Some(g), Some(h)) = (self.goto.as_mut(), hits.get(to)) {
            g.pick = Some(h.id());
            g.say = None;
        }
    }

    /// `↑` or `↓`: the previous or next row; with none picked, the last or the first.
    fn goto_move(&mut self, up: bool) {
        let hits = self.goto_hits();
        if hits.is_empty() {
            return;
        }
        let to = match self.goto_index(&hits) {
            Some(i) if up => i.saturating_sub(1),
            Some(i) => (i + 1).min(hits.len() - 1),
            None if up => hits.len() - 1,
            None => 0,
        };
        self.goto_pick(&hits, to);
    }

    /// `tab` or `shift-tab`: the first row of the next or previous group, round; with
    /// none picked, of the first or the last group.
    fn goto_jump(&mut self, back: bool) {
        let hits = self.goto_hits();
        if hits.is_empty() {
            return;
        }
        let firsts: Vec<usize> = (0..hits.len())
            .filter(|&j| j == 0 || hits[j].section != hits[j - 1].section)
            .collect();
        let n = firsts.len();
        let to = match self.goto_index(&hits) {
            Some(i) => {
                let at = firsts.iter().rposition(|&f| f <= i).unwrap_or(0);
                firsts[if back { (at + n - 1) % n } else { (at + 1) % n }]
            }
            None if back => firsts[n - 1],
            None => firsts[0],
        };
        self.goto_pick(&hits, to);
    }

    /// `enter` (or `ctrl-f`, `only` the Find row): closes the box and does what the
    /// picked row does, through the code its own screen runs. Runs nothing, the box
    /// left open, when no row is picked, when the picked one is no longer there, or on
    /// an ACTIONS row once what the box was opened on changed.
    fn goto_enter(&mut self, only: Option<Go>) -> Vec<Effect> {
        let hits = self.goto_hits();
        let moved = self.goto_actions_moved();
        let Some(g) = self.goto.as_mut() else {
            return Vec::new();
        };
        let hit = match &only {
            Some(go) => hits.iter().find(|h| &h.go == go),
            None => {
                let Some(id) = g.pick.clone() else {
                    g.say.get_or_insert(PICK_A_ROW);
                    return Vec::new();
                };
                let Some(h) = hits.iter().find(|h| h.id() == id) else {
                    g.pick = None;
                    g.say = Some(GONE);
                    return Vec::new();
                };
                Some(h)
            }
        };
        let Some(hit) = hit.cloned() else {
            return Vec::new();
        };
        if hit.section == Section::Actions && moved {
            return Vec::new();
        }
        let Some(g) = self.goto.take() else {
            return Vec::new();
        };
        if let Some(why) = hit.off {
            self.set_info(why);
            return Vec::new();
        }
        match hit.go {
            Go::Pattern(p) => self.goto_pattern(p),
            Go::Key { name, from } => self.goto_open_key(&name, from),
            Go::Find => {
                // As typed into the Find prompt and `enter` pressed there.
                self.find.start();
                self.find.text = g.text;
                self.find_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            }
            Go::Screen(to) => self.switch(to),
            Go::Action(line) => match &line.status {
                // A write is a step further from the row it acts on than its key or
                // the menu, behind a dimmed screen: the menu opens on it instead, its
                // heading naming the row, and `enter` there sends it.
                Status::Sends(_) | Status::Form { .. } => self.menu_on(&g, &line),
                _ => self.press_line(line),
            },
            Go::Command => {
                let rest = command_text(&g.text).unwrap_or("");
                let mut out = self.open_cmdline();
                out.extend(self.cmdline_set_input(rest));
                out
            }
        }
    }

    /// Opens the Actions menu of the screen Go to was opened on, on `line`, for what
    /// the box was opened on.
    fn menu_on(&mut self, g: &GoTo, line: &MenuLine) -> Vec<Effect> {
        let at = self
            .menu_lines()
            .iter()
            .position(|l| l.token() == line.token() && l.label == line.label);
        match at {
            Some(sel) => {
                self.menu = Some(Menu {
                    screen: g.screen,
                    sel,
                    target: g.target,
                });
            }
            None => self.set_info(GONE),
        }
        Vec::new()
    }

    /// A PATTERNS row: its row selected on Browse › Keys (a filter that hides it
    /// cleared, its group expanded, either said), then `→` on it: its key list.
    fn goto_pattern(&mut self, pattern: String) -> Vec<Effect> {
        let mut said = None;
        if !self.select_pattern(&pattern) {
            if !self.patterns.filter.is_empty() {
                self.patterns.filter.clear();
                said = Some(CLEARED_FILTER);
            }
            if !self.select_pattern(&pattern) {
                self.expand_group_of(&pattern);
                if self.select_pattern(&pattern) {
                    said = Some(said.map_or(
                        EXPANDED_GROUP,
                        |_| "Cleared the filter and expanded its group to show it.",
                    ));
                }
            }
        }
        let out = self.open_keys(pattern);
        if let Some(s) = said {
            self.add_info(s);
        }
        out
    }

    /// Selects `pattern`'s row on the Patterns tree if it is in view. True when it is.
    fn select_pattern(&mut self, pattern: &str) -> bool {
        let Some(d) = self.pattern_data() else {
            return false;
        };
        let name = |i: usize| d.patterns.get(i).map(|p| p.pattern.as_str());
        let at = self.pattern_rows().iter().position(|r| match r {
            Row::Pattern { idx, .. } => name(*idx) == Some(pattern),
            Row::Group { root } => matches!(
                self.patterns.tree.roots.get(*root),
                Some(Root::Single(i)) if name(*i) == Some(pattern)
            ),
        });
        match at {
            Some(i) => {
                self.patterns.sel = i;
                true
            }
            None => false,
        }
    }

    /// Expands the collapsed group `pattern` is in, if any.
    fn expand_group_of(&mut self, pattern: &str) {
        let Some(d) = self.pattern_data() else {
            return;
        };
        let key = self.patterns.tree.groups().find_map(|g| {
            g.children
                .iter()
                .any(|&i| d.patterns.get(i).is_some_and(|p| p.pattern == pattern))
                .then(|| g.key.clone())
        });
        if let Some(k) = key {
            self.patterns.collapsed.expand(&k);
        }
    }

    /// A KEYS row: the key list's open on a listed key (a filter that hides it
    /// cleared, and said); the value's screen for the key whose value is open; Big
    /// keys and Hot keys, which open nothing, with the key selected.
    fn goto_open_key(&mut self, name: &str, from: KeyFrom) -> Vec<Effect> {
        match from {
            KeyFrom::List => {
                let tick = self.tick;
                let keys = &mut self.views.keys;
                let find = |k: &views::keys::KeysState| {
                    k.visible().iter().position(|&i| k.rows[i].key == name)
                };
                let mut cleared = false;
                let at = find(keys).or_else(|| {
                    keys.filter.clear();
                    keys.type_filter = Default::default();
                    cleared = true;
                    find(keys)
                });
                let Some(at) = at else {
                    self.set_info(GONE);
                    return Vec::new();
                };
                if keys.sel != at {
                    keys.sel = at;
                    keys.sel_since = tick;
                }
                let out = self.open_detail();
                if cleared {
                    self.add_info(CLEARED_FILTER);
                }
                out
            }
            KeyFrom::Value => self.switch(Screen::KeyDetail),
            KeyFrom::BigKeys(i) => {
                let a = &mut self.views.analysis;
                if a.sel != i {
                    a.sel = i;
                    a.sel_since = self.tick;
                }
                self.switch(Screen::BigKeys)
            }
            KeyFrom::HotKeys(i) => {
                self.sel.hot = i;
                self.switch(Screen::HotKeys)
            }
        }
    }
}

/// Adds `c` to `text` unless it would pass [`MAX_TEXT`]. True when added.
fn push(text: &mut String, c: char) -> bool {
    if text.len() + c.len_utf8() > MAX_TEXT {
        return false;
    }
    text.push(c);
    true
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::live::keys::KeyRow;
    use crate::model::{Env, Ttl};
    use crate::tui::app::Msg;
    use crate::tui::app::views::testing::{self, key};
    use crate::tui::fixtures;

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_text(app: &mut App, s: &str) -> Vec<Effect> {
        s.chars().flat_map(|c| app.handle_key(ch(c))).collect()
    }

    /// `ctrl-k`, then `text`: every effect of both.
    fn open_with(app: &mut App, text: &str) -> Vec<Effect> {
        let mut e = app.handle_key(ctrl('k'));
        assert!(
            app.goto.is_some(),
            "ctrl-k opens Go to on {:?}",
            app.screen()
        );
        e.extend(type_text(app, text));
        e
    }

    /// The row `enter` would go to.
    fn picked(app: &App) -> Hit {
        let hits = app.goto_hits();
        let i = app.goto_index(&hits).expect("a row is picked");
        hits[i].clone()
    }

    /// The row picked, if any.
    fn pick_of(app: &App) -> Option<Hit> {
        let hits = app.goto_hits();
        app.goto_index(&hits).map(|i| hits[i].clone())
    }

    /// Picks the row `h` names, as `↑↓` would.
    fn pick(app: &mut App, h: &Hit) {
        app.goto.as_mut().unwrap().pick = Some(h.id());
    }

    /// Writes on, on a staging profile, with `skip_non_prod_confirm`: every write the
    /// gate lets through runs at once, without a question.
    fn writes_at_once(app: &mut App) {
        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Stage;
        s.profile.read_only = Some(false);
        app.settings = crate::write::Settings {
            skip_non_prod_confirm: true,
            ..crate::write::Settings::default()
        };
    }

    /// The writes and bulk steps among `e`.
    fn writes(e: &[Effect]) -> Vec<String> {
        use crate::live::LiveRequest;
        testing::requests(e)
            .into_iter()
            .filter(|(_, r)| matches!(r, LiveRequest::Write(_) | LiveRequest::Bulk(_)))
            .map(|(_, r)| format!("{r:?}"))
            .collect()
    }

    fn requests(e: &[Effect]) -> Vec<String> {
        testing::requests(e)
            .into_iter()
            .map(|(_, r)| format!("{r:?}"))
            .collect()
    }

    fn row(name: &str, t: KeyType, bytes: u64) -> KeyRow {
        KeyRow {
            key: name.into(),
            key_type: t,
            ttl: Ttl::Persistent,
            bytes: Some(bytes),
            encoding: Some("listpack".into()),
            length: Some(3),
            freq: None,
        }
    }

    /// Scanned (with the fixture's hot keys), live, on `screen`, with three keys
    /// loaded in the key list (as a pattern's keys would leave them).
    fn loaded(screen: Screen) -> App {
        let (mut app, _) = testing::app_on(Screen::KeyList);
        if let Some(r) = app.session.as_mut().and_then(|s| s.report.as_mut()) {
            r.hot_keys = fixtures::hot_keys();
        }
        app.views.keys.rows = vec![
            row("user:10402:cart", KeyType::Hash, 90_112),
            row("user:10612:cart", KeyType::Hash, 14_336),
            row("cart:1002", KeyType::Hash, 2_458),
        ];
        match screen {
            Screen::KeyList => {}
            Screen::Patterns => {
                app.handle_key(key(KeyCode::Esc));
            }
            s => {
                testing::show(&mut app, s);
            }
        }
        assert_eq!(app.screen(), screen);
        app
    }

    fn text(app: &App, w: u16, h: u16) -> String {
        let t = testing::render(app, w, h);
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -- Matching -----------------------------------------------------------------

    #[test]
    fn matches_rank_exact_prefix_word_start_then_anywhere() {
        let rank = |h: &str, n: &str| match_text(h, n).map(|m| m.rank);
        assert_eq!(rank("Overview", "overview"), Some(Rank::Exact));
        assert_eq!(rank("Server › Slow log", "SERVER"), Some(Rank::Prefix));
        assert_eq!(rank("user:{id}:cart", "cart"), Some(Rank::WordStart));
        assert_eq!(rank("Slow log", "log"), Some(Rank::WordStart));
        assert_eq!(rank("promo:cartbanner", "artb"), Some(Rank::Substring));
        assert_eq!(rank("Overview", "x"), None);
        assert_eq!(rank("Overview", ""), None);
        assert_eq!(rank("ab", "abc"), None);
        // The matched characters, by char index, for the focus colour.
        let m = match_text("user:{id}:cart", "CART").unwrap();
        assert_eq!((m.start, m.len), (10, 4));
        // A later word start beats an earlier substring.
        let m = match_text("xcart:cart", "cart").unwrap();
        assert_eq!((m.rank, m.start), (Rank::WordStart, 6));
        let m = match_text("é:Élan", "élan").unwrap();
        assert_eq!((m.rank, m.start), (Rank::WordStart, 2));
    }

    proptest::proptest! {
        #[test]
        fn any_part_of_a_name_matches_it(hay in "[a-zA-Z:{} ._-]{1,24}", a in 0usize..24, l in 1usize..8) {
            let chars: Vec<char> = hay.chars().collect();
            let a = a.min(chars.len() - 1);
            let l = l.min(chars.len() - a);
            let needle: String = chars[a..a + l].iter().collect();
            let m = match_text(&hay, &needle.to_uppercase());
            proptest::prop_assert!(m.is_some());
            let m = m.unwrap();
            let got: String = chars[m.start..m.start + m.len].iter().collect();
            proptest::prop_assert_eq!(got.to_lowercase(), needle.to_lowercase());
            proptest::prop_assert_eq!(m.rank == Rank::Exact, hay.to_lowercase() == needle.to_lowercase());
        }
    }

    #[test]
    fn rows_are_ranked_within_their_group_and_groups_keep_their_order() {
        let app = loaded(Screen::Overview);
        let hits = app.goto_hits_for("cart");
        let sections: Vec<Section> = hits.iter().map(|h| h.section).collect();
        let mut sorted = sections.clone();
        sorted.sort();
        assert_eq!(sections, sorted, "groups in order");
        let keys: Vec<&str> = hits
            .iter()
            .filter(|h| h.section == Section::Keys)
            .map(|h| h.label.as_str())
            .collect();
        // The prefix match first, then the word starts in the order they are held.
        assert_eq!(keys[0], "cart:1002");
        // The key list's keys, then the hot key.
        assert_eq!(
            &keys[1..4],
            ["user:10402:cart", "user:10612:cart", "user:88213:cart"]
        );
        assert_eq!(keys.last(), Some(&"Find every key matching *cart*"));
        // Each key once, though user:88213:cart is a hot key and a big key's example.
        let mut seen = keys.clone();
        seen.dedup();
        assert_eq!(seen.len(), keys.len());
    }

    // -- The default row -----------------------------------------------------------

    #[test]
    fn the_default_row() {
        let pick = |app: &mut App, text: &str| {
            app.goto = None;
            open_with(app, text);
            picked(app)
        };
        // `:` is the command row whatever follows, spaces before it too.
        for text in [":GET k", "  :AUTH a b", ":"] {
            let h = pick(&mut loaded(Screen::Overview), text);
            assert_eq!(h.go, Go::Command, "{text:?}");
        }
        let mut app = loaded(Screen::Overview);
        // An exact or prefix match on a screen beats everything.
        let h = pick(&mut app, "slow");
        assert_eq!(h.go, Go::Screen(Screen::SlowLog));
        let h = pick(&mut app, "overview");
        assert_eq!(h.go, Go::Screen(Screen::Overview));
        let h = pick(&mut app, "conn");
        assert_eq!(h.go, Go::Screen(Screen::Connections));
        // Else the first pattern.
        let h = pick(&mut app, "user:{id}:cart");
        assert_eq!(h.go, Go::Pattern("user:{id}:cart".into()));
        let h = pick(&mut app, "cart");
        assert_eq!(h.go, Go::Pattern("user:{id}:cart".into()));
        // Else the first loaded key.
        let h = pick(&mut app, "10612");
        assert_eq!(
            h.go,
            Go::Key {
                name: "user:10612:cart".into(),
                from: KeyFrom::List
            }
        );
        // Nothing else: nothing is picked, though the Find row is there.
        app.goto = None;
        open_with(&mut app, "zzqq");
        assert_eq!(pick_of(&app), None);
        assert_eq!(app.goto_hits().len(), 1);
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(
            app.goto.is_some(),
            "enter with nothing picked keeps the box"
        );
        assert_eq!(app.goto.as_ref().unwrap().say, Some(PICK_A_ROW));
        // A word only actions start: never an action.
        for text in ["d", "del", "delete", "copy", "y", "r", "refresh", "e"] {
            app.goto = None;
            open_with(&mut app, text);
            if let Some(h) = pick_of(&app) {
                assert!(
                    !matches!(h.go, Go::Action(_) | Go::Find | Go::Command),
                    "{text}: {h:?}"
                );
            }
        }
        // No text: the first screen that is not this one.
        let h = pick(&mut app, "");
        assert_eq!(h.label, "Browse › Keys");
        // Where `2` goes: the key list, left open.
        assert_eq!(h.go, Go::Screen(Screen::KeyList));
        assert!(!h.here);
        // `slow` then enter opens the Slow log and never starts a Find.
        app.goto = None;
        open_with(&mut app, "slow");
        let e = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::SlowLog);
        assert!(app.goto.is_none());
        assert!(
            !requests(&e).iter().any(|r| r.contains("Find")),
            "{:?}",
            requests(&e)
        );
    }

    // -- Typing reads nothing ------------------------------------------------------

    /// On every screen: opening the box, typing, moving, ticking and closing produce
    /// no effect at all, and change no screen.
    #[test]
    fn opening_typing_moving_and_closing_send_nothing_on_any_screen() {
        for screen in Screen::ALL {
            if matches!(screen, Screen::Nodes | Screen::Slots) {
                continue;
            }
            let mut app = loaded(screen);
            if screen == Screen::Connections {
                testing::show(&mut app, Screen::Connections);
            }
            let mut e = open_with(&mut app, "user:{id}:cart q.?yn 0123 :");
            for k in [
                key(KeyCode::Down),
                key(KeyCode::Down),
                key(KeyCode::Up),
                key(KeyCode::Tab),
                key(KeyCode::BackTab),
                key(KeyCode::Backspace),
                ctrl('u'),
            ] {
                e.extend(app.handle_key(k));
            }
            e.extend(app.handle_paste("cart\nsession".into()));
            for _ in 0..3 {
                e.extend(app.apply(Msg::Tick));
            }
            assert!(app.goto.is_some(), "{screen:?}");
            e.extend(type_text(&mut app, "slow"));
            e.extend(app.handle_key(key(KeyCode::Esc)));
            assert_eq!(app.goto.as_ref().map(|g| g.text.as_str()), Some(""));
            e.extend(app.handle_key(key(KeyCode::Esc)));
            assert!(app.goto.is_none(), "{screen:?}");
            // ctrl-k again closes too.
            e.extend(open_with(&mut app, "cart"));
            e.extend(app.handle_key(ctrl('k')));
            assert!(app.goto.is_none(), "{screen:?}");
            assert!(e.is_empty(), "{screen:?}: {e:?}");
            assert_eq!(app.screen(), screen);
        }
    }

    #[test]
    fn every_printable_key_is_text() {
        let mut app = loaded(Screen::Overview);
        let all: String = (' '..='~').collect();
        let e = open_with(&mut app, &all);
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(app.goto.as_ref().unwrap().text, all);
        assert_eq!(app.screen(), Screen::Overview);
        assert!(app.menu.is_none() && !app.help && !app.find.open);
        assert!(!app.views.cmdline.open);
        // `j` and `k` are text, not moves.
        app.handle_key(ctrl('u'));
        type_text(&mut app, "jk");
        assert_eq!(app.goto.as_ref().unwrap().text, "jk");
        // The default for `jk`, not a row moved to.
        let hits = app.goto_hits();
        let empty = false;
        assert_eq!(
            app.goto.as_ref().unwrap().pick,
            default_index(&hits, empty).map(|i| hits[i].id())
        );
    }

    #[test]
    fn go_to_and_the_actions_menu_are_never_open_together() {
        let mut app = loaded(Screen::Patterns);
        app.handle_key(ch('.'));
        assert!(app.menu.is_some());
        app.handle_key(ctrl('k'));
        assert!(app.goto.is_none(), "the menu takes every key");
        app.handle_key(key(KeyCode::Esc));
        open_with(&mut app, ".");
        assert!(app.menu.is_none());
        assert_eq!(app.goto.as_ref().unwrap().text, ".");
    }

    #[test]
    fn an_input_or_overlay_that_takes_the_keys_keeps_ctrl_k() {
        let mut app = loaded(Screen::Patterns);
        app.handle_key(ch('/'));
        app.handle_key(ctrl('k'));
        assert!(app.goto.is_none(), "the filter input takes it");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ch('?'));
        app.handle_key(ctrl('k'));
        assert!(app.goto.is_none(), "the help takes it");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ctrl('f'));
        app.handle_key(ctrl('k'));
        assert!(app.goto.is_none(), "the Find prompt takes it");
    }

    // -- enter does what the direct action does -------------------------------------

    #[test]
    fn a_pattern_row_does_what_right_arrow_on_that_pattern_does() {
        let mut app = loaded(Screen::Overview);
        app.views.keys = Default::default();
        open_with(&mut app, "user:{id}:feed");
        let via = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.pattern.as_deref(), Some("user:{id}:feed"));

        let mut direct = loaded(Screen::Patterns);
        direct.views.keys = Default::default();
        assert!(direct.select_pattern("user:{id}:feed"));
        let by_key = direct.handle_key(key(KeyCode::Right));
        assert_eq!(direct.screen(), Screen::KeyList);
        assert!(!requests(&by_key).is_empty(), "→ reads its keys");
        assert_eq!(requests(&via), requests(&by_key));
        // Back on Browse › Keys its row is the one selected.
        assert_eq!(app.patterns.sel, direct.patterns.sel);
    }

    #[test]
    fn a_key_row_opens_the_key_as_the_key_list_does() {
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "10612");
        let via = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyDetail);
        assert_eq!(
            app.views.value.row.as_ref().map(|r| r.key.as_str()),
            Some("user:10612:cart")
        );

        let mut direct = loaded(Screen::KeyList);
        let at = direct
            .views
            .keys
            .visible()
            .iter()
            .position(|&i| direct.views.keys.rows[i].key == "user:10612:cart")
            .unwrap();
        direct.views.keys.sel = at;
        let by_key = direct.handle_key(key(KeyCode::Enter));
        assert_eq!(direct.screen(), Screen::KeyDetail);
        assert!(!requests(&by_key).is_empty());
        assert_eq!(requests(&via), requests(&by_key));
    }

    #[test]
    fn big_and_hot_key_rows_select_the_key_on_their_panel() {
        let mut app = loaded(Screen::Overview);
        app.views.keys = Default::default();
        open_with(&mut app, "lock:billing");
        let h = picked(&app);
        let Go::Key {
            from: KeyFrom::HotKeys(i),
            ..
        } = h.go
        else {
            panic!("{h:?}");
        };
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::HotKeys);
        assert_eq!(app.sel.hot, i);
    }

    #[test]
    fn the_find_row_runs_find_key_with_the_text() {
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "zzqq");
        // Never the default: `↓` picks it.
        assert_eq!(pick_of(&app), None);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(picked(&app).go, Go::Find);
        let via = app.handle_key(key(KeyCode::Enter));

        let mut direct = loaded(Screen::Overview);
        direct.handle_key(ctrl('f'));
        direct.handle_key(ctrl('u'));
        type_text(&mut direct, "zzqq");
        let by_key = direct.handle_key(key(KeyCode::Enter));
        assert!(!requests(&by_key).is_empty());
        assert_eq!(requests(&via), requests(&by_key));
        assert_eq!(app.screen(), direct.screen());
        assert_eq!(app.find, direct.find);
        // ctrl-f in the box picks the Find row whatever is the default.
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "slow");
        let e = app.handle_key(ctrl('f'));
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.find.as_deref(), Some("*slow*"));
        assert!(!requests(&e).is_empty());
    }

    #[test]
    fn a_screen_row_does_what_its_digit_keys_do() {
        let app = loaded(Screen::Overview);
        let hits = app.goto_hits_for("");
        let screens: Vec<Hit> = hits
            .into_iter()
            .filter(|h| h.section == Section::Screens && !h.here)
            .collect();
        assert_eq!(screens.len(), 12, "{screens:?}");
        for h in screens {
            let mut via = loaded(Screen::Overview);
            via.handle_key(ctrl('k'));
            pick(&mut via, &h);
            let a = via.handle_key(key(KeyCode::Enter));

            let mut direct = loaded(Screen::Overview);
            let mut b = Vec::new();
            for k in &h.keys {
                b.extend(direct.handle_key(ch(k.chars().next().unwrap())));
            }
            assert_eq!(via.screen(), direct.screen(), "{}", h.label);
            let (a, b) = (requests(&a), requests(&b));
            if h.keys.len() == 1 {
                assert_eq!(a, b, "{}", h.label);
            } else {
                // `3 3` shows Big keys on the way, which reads its lengths: Go to
                // switches straight there, so it reads at most what the digits read.
                assert!(a.iter().all(|r| b.contains(r)), "{}: {a:?} {b:?}", h.label);
            }
        }
    }

    /// Each ACTIONS row that sends no write does what the Actions menu's line does: the
    /// same requests, the same question, the same notice, the same screen; one that
    /// writes (it sends now, or opens a form) opens the menu on its line instead. On a
    /// read-only production profile (writes refused), with writes on (writes ask
    /// first) and with `skip_non_prod_confirm` (writes run at once).
    #[test]
    fn an_action_row_does_what_the_menu_line_does() {
        let state = |app: &App| {
            (
                app.screen(),
                app.question(),
                app.bulk.is_some(),
                app.notice.as_ref().map(|n| n.text.clone()),
            )
        };
        let mut checked = (0, 0, 0, 0);
        for writes in 0..3 {
            for screen in [Screen::Patterns, Screen::KeyList, Screen::KeyDetail] {
                let make = || {
                    let mut app = loaded(screen);
                    if writes == 1 {
                        let s = app.session.as_mut().unwrap();
                        s.profile.env = Env::Prod;
                        s.profile.read_only = Some(false);
                    }
                    if writes == 2 {
                        writes_at_once(&mut app);
                    }
                    app
                };
                let lines = make().menu_lines();
                for (i, line) in lines.iter().enumerate() {
                    let mut menu = make();
                    menu.handle_key(ch('.'));
                    for _ in 0..i {
                        menu.handle_key(key(KeyCode::Down));
                    }
                    let a = menu.handle_key(key(KeyCode::Enter));

                    let mut via = make();
                    via.handle_key(ctrl('k'));
                    let hit = via
                        .goto_hits()
                        .into_iter()
                        .find(|h| h.go == Go::Action(line.clone()))
                        .unwrap_or_else(|| panic!("{screen:?}: {} missing", line.label));
                    pick(&mut via, &hit);
                    let b = via.handle_key(key(KeyCode::Enter));
                    let what = format!(
                        "{screen:?} writes={writes} {} {:?}",
                        line.label, line.status
                    );
                    assert!(via.goto.is_none(), "{what}");
                    if matches!(line.status, Status::Sends(_) | Status::Form { .. }) {
                        checked.3 += 1;
                        // The menu, on this line, for what Go to was opened on.
                        assert!(b.is_empty(), "{what}: {b:?}");
                        let m = via.menu.unwrap_or_else(|| panic!("{what}: no menu"));
                        assert_eq!(m.sel, i, "{what}");
                        assert_eq!(m.screen, screen, "{what}");
                        assert_eq!(via.question(), None, "{what}");
                        assert!(via.bulk.is_none(), "{what}");
                        // `enter` there runs it as the menu does.
                        let c = via.handle_key(key(KeyCode::Enter));
                        assert_eq!(requests(&a), requests(&c), "{what}");
                        assert_eq!(state(&menu), state(&via), "{what}");
                        continue;
                    }
                    assert_eq!(requests(&a), requests(&b), "{what}");
                    assert_eq!(state(&menu), state(&via), "{what}");
                    match &line.status {
                        Status::Off(why) => {
                            checked.0 += 1;
                            assert!(b.is_empty(), "{what}");
                            assert_eq!(via.notice.as_ref().map(|n| &n.text), Some(why));
                        }
                        Status::Asks => {
                            checked.1 += 1;
                            assert!(via.question().is_some() || via.bulk.is_some(), "{what}");
                        }
                        _ => checked.2 += 1,
                    }
                }
            }
        }
        assert!(
            checked.0 > 0 && checked.1 > 0 && checked.2 > 0 && checked.3 > 0,
            "{checked:?}"
        );
    }

    /// What the ACTIONS act on is recorded when the box opens and named in their
    /// heading; once it changes they are off (another key typed does not record it
    /// again), `enter` on one runs nothing, and the other groups keep working.
    #[test]
    fn the_actions_act_on_what_the_box_was_opened_on_or_nothing() {
        let mut app = loaded(Screen::KeyList);
        writes_at_once(&mut app);
        let first = app.views.keys.selected().unwrap().key.clone();
        open_with(&mut app, "del");
        assert_eq!(app.goto.as_ref().unwrap().on, first);
        let drawn = text(&app, 120, 36);
        assert!(drawn.contains(&format!("ACTIONS  on {first}")), "{drawn}");
        // A reload selects another key under the box; one more key typed.
        app.views.keys.sel = 2;
        app.apply(Msg::Tick);
        type_text(&mut app, "e");
        assert!(app.goto_actions_moved());
        let hits = app.goto_hits();
        let actions: Vec<&Hit> = hits
            .iter()
            .filter(|h| h.section == Section::Actions)
            .collect();
        assert!(!actions.is_empty());
        assert!(
            actions
                .iter()
                .all(|h| h.off.as_deref() == Some(ACTIONS_MOVED)),
            "{actions:?}"
        );
        let drawn = text(&app, 120, 36);
        assert!(drawn.contains(ACTIONS_MOVED_HEAD), "{drawn}");
        for a in actions {
            pick(&mut app, a);
            let e = app.handle_key(key(KeyCode::Enter));
            assert!(e.is_empty(), "{}: {e:?}", a.label);
            assert!(app.goto.is_some() && app.menu.is_none(), "{}", a.label);
        }
        // A screen row still goes.
        app.handle_key(ctrl('u'));
        type_text(&mut app, "overview");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::Overview);
    }

    #[test]
    fn the_command_row_fills_the_command_line_without_running_it() {
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, ":GET k");
        let hits = app.goto_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].label, "Run in the command line: GET k");
        let e = app.handle_key(key(KeyCode::Enter));
        assert!(app.goto.is_none());
        assert!(app.views.cmdline.open);
        assert_eq!(app.views.cmdline.shown_input(), "GET k");
        assert!(
            !requests(&e)
                .iter()
                .any(|r| r.contains("Run") || r.contains("Batch")),
            "{:?}",
            requests(&e)
        );
    }

    #[test]
    fn the_command_row_and_the_input_are_masked() {
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, ":AUTH default hunter2");
        let hits = app.goto_hits();
        assert_eq!(hits[0].label, "Run in the command line: AUTH *** ***");
        assert!(!format!("{:?}", app.goto).contains("hunter2"));
        for (w, h) in [(120, 36), (80, 24)] {
            let drawn = text(&app, w, h);
            assert!(!drawn.contains("hunter2"), "{drawn}");
            assert!(drawn.contains(":AUTH *** ***"), "{drawn}");
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.views.cmdline.shown_input(), "AUTH *** ***");
    }

    // -- What closes it -------------------------------------------------------------

    #[test]
    fn a_new_screen_under_it_closes_it() {
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "cart");
        // A reply that switches the screen does it through `switch`.
        app.switch(Screen::Patterns);
        app.after_change(false, true);
        assert!(app.goto.is_none());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains(SCREEN_CHANGED))
        );
    }

    // -- No credentials -------------------------------------------------------------

    #[test]
    fn no_connection_or_credential_is_a_row() {
        use secrecy::SecretString;
        let mut app = loaded(Screen::Overview);
        let mut found = fixtures::docker_instance();
        found.credentials = Some(crate::model::Credentials {
            username: Some("alice-admin".into()),
            password: SecretString::from("hunter2"),
        });
        found.auth = crate::conn::docker::DiscoveredAuth::Password;
        app.apply(Msg::DockerFound(vec![found]));
        for text in [
            "alice-admin",
            "hunter2",
            "myapp-redis",
            "prod-cache-01",
            "staging",
        ] {
            let hits = app.goto_hits_for(text);
            assert!(hits.iter().all(|h| h.go == Go::Find), "{text}: {hits:?}");
        }
        // Before connecting, too.
        let app = App::new(fixtures::profiles());
        for text in ["prod-cache-01", "local-6379"] {
            assert!(
                app.goto_hits_for(text).iter().all(|h| h.go == Go::Find),
                "{text}"
            );
        }
    }

    // -- Before connecting ----------------------------------------------------------

    #[test]
    fn before_connecting_only_connections_is_reachable() {
        let mut app = App::new(fixtures::profiles());
        app.handle_key(ctrl('k'));
        assert!(app.goto.is_some());
        let hits = app.goto_hits();
        let screens: Vec<&Hit> = hits
            .iter()
            .filter(|h| h.section == Section::Screens)
            .collect();
        assert!(!screens.is_empty());
        for h in &screens {
            assert_eq!(
                h.off.is_none(),
                h.go == Go::Screen(Screen::Connections),
                "{h:?}"
            );
        }
        assert!(hits.iter().any(|h| h.section == Section::Actions));
        // An off screen only says why.
        // No default before connecting: enter does nothing (it would connect).
        assert_eq!(pick_of(&app), None);
        let e = app.handle_key(key(KeyCode::Enter));
        assert!(e.is_empty(), "{e:?}");
        assert!(app.goto.is_some());
        let first = (*screens[0]).clone();
        pick(&mut app, &first);
        let e = app.handle_key(key(KeyCode::Enter));
        assert!(e.is_empty());
        assert_eq!(app.screen(), Screen::Connections);
        assert!(app.notice.as_ref().unwrap().text.contains("Connect first"));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for text in ["", "cart", "zzqq", ":AUTH a b", "s"] {
            let mut app = loaded(Screen::Overview);
            open_with(&mut app, text);
            app.handle_key(key(KeyCode::Down));
            for (w, h) in [
                (0, 0),
                (1, 1),
                (20, 5),
                (30, 9),
                (40, 12),
                (3, 30),
                (200, 2),
                (60, 10),
            ] {
                testing::render(&app, w, h);
            }
        }
    }

    // -- Writes -----------------------------------------------------------------------

    /// With writes on and running at once (`skip_non_prod_confirm`), on every screen:
    /// no row of Go to sends a write or starts a bulk action, whether picked as the
    /// default or with `↑↓`. A write's row opens the Actions menu on its line.
    #[test]
    fn no_key_sequence_in_go_to_sends_a_write() {
        let mut menus = 0;
        for screen in Screen::ALL {
            if matches!(screen, Screen::Nodes | Screen::Slots) {
                continue;
            }
            let make = || {
                let mut app = loaded(screen);
                writes_at_once(&mut app);
                if screen == Screen::Connections {
                    testing::show(&mut app, Screen::Connections);
                }
                app
            };
            // The default of each letter, and of each action's name.
            let mut texts: Vec<String> = ('a'..='z').map(String::from).collect();
            texts.extend(make().menu_lines().iter().map(|l| l.label.to_lowercase()));
            for text in &texts {
                let mut app = make();
                let mut e = open_with(&mut app, text);
                e.extend(app.handle_key(key(KeyCode::Enter)));
                assert!(writes(&e).is_empty(), "{screen:?} {text:?}: {e:?}");
                assert!(app.bulk.is_none(), "{screen:?} {text:?}");
            }
            // Every row, picked.
            let rows = {
                let mut app = make();
                open_with(&mut app, "");
                app.goto_hits()
            };
            for h in rows {
                let mut app = make();
                open_with(&mut app, "");
                pick(&mut app, &h);
                let e = app.handle_key(key(KeyCode::Enter));
                let what = format!("{screen:?} {}", h.label);
                assert!(writes(&e).is_empty(), "{what}: {e:?}");
                assert!(app.bulk.is_none(), "{what}");
                if let Go::Action(l) = &h.go
                    && matches!(l.status, Status::Sends(_) | Status::Form { .. })
                {
                    menus += 1;
                    let m = app.menu.unwrap_or_else(|| panic!("{what}: no menu"));
                    let lines = app.menu_lines();
                    assert_eq!(lines[m.sel].label, l.label, "{what}");
                    assert_eq!(lines[m.sel].token(), l.token(), "{what}");
                }
            }
        }
        assert!(menus > 5, "{menus}");
    }

    /// The reproduction: on the key list, `d` and `enter` sent UNLINK on the selected
    /// key; on Browse › Keys it opened the bulk delete and sent its count scan.
    #[test]
    fn d_and_enter_deletes_nothing() {
        for screen in [Screen::KeyList, Screen::Patterns] {
            let mut app = loaded(screen);
            writes_at_once(&mut app);
            let mut e = open_with(&mut app, "d");
            e.extend(app.handle_key(key(KeyCode::Enter)));
            assert!(writes(&e).is_empty(), "{screen:?}: {e:?}");
            assert!(app.bulk.is_none() && app.question().is_none(), "{screen:?}");
            assert!(
                !requests(&e).iter().any(|r| r.contains("Bulk")),
                "{screen:?}"
            );
        }
    }

    // -- The picked row --------------------------------------------------------------

    /// `enter` runs the row drawn as picked or nothing: never another row in its
    /// place, when a picked key leaves the list or a default key row goes.
    #[test]
    fn a_picked_row_that_is_gone_runs_nothing() {
        // A KEYS row picked with ↓, then the key leaves the list.
        let mut app = loaded(Screen::KeyList);
        writes_at_once(&mut app);
        open_with(&mut app, "cart");
        while pick_of(&app).is_none_or(|h| h.label != "user:10612:cart") {
            app.handle_key(key(KeyCode::Down));
        }
        app.views.keys.rows.retain(|r| r.key != "user:10612:cart");
        let e = app.handle_key(key(KeyCode::Enter));
        assert!(e.is_empty(), "{e:?}");
        let g = app.goto.as_ref().expect("still open");
        assert_eq!((g.pick.as_ref(), g.say), (None, Some(GONE)));
        for (w, h) in [(120, 36), (80, 24)] {
            assert!(text(&app, w, h).contains(GONE), "{w}x{h}");
        }
        // A default key row goes with a reply, before enter: said at once.
        let mut app = loaded(Screen::KeyList);
        open_with(&mut app, "10612");
        assert!(matches!(picked(&app).go, Go::Key { .. }));
        app.views.keys.rows.clear();
        app.apply(Msg::Tick);
        assert_eq!(app.goto.as_ref().unwrap().say, Some(GONE));
        let e = app.handle_key(key(KeyCode::Enter));
        assert!(e.is_empty(), "no Find in its place: {e:?}");
        assert!(app.goto.is_some());
        // ↓ picks again, and that runs.
        app.handle_key(key(KeyCode::Down));
        assert!(app.goto.as_ref().unwrap().say.is_none());
        assert_eq!(picked(&app).go, Go::Find);
    }

    // -- Pastes and the `:` row ------------------------------------------------------

    /// A paste with a newline or tab is one line, as the command line takes it; text
    /// that starts with `:` after spaces is a command. Neither shows the password: not
    /// drawn, not in `Debug`, not in the notice, not in the command line's input as
    /// shown, and not searched for.
    #[test]
    fn a_pasted_or_spaced_command_never_shows_its_password() {
        let cases: [(&str, bool, &str); 4] = [
            (":AUTH\nhunter2", true, "AUTH ***"),
            (":AUTH\tdefault\r\nhunter2", true, "AUTH ***  ***"),
            (" :AUTH default hunter2", true, "AUTH *** ***"),
            ("  :AUTH default hunter2", false, "AUTH *** ***"),
        ];
        for (input, paste, shown) in cases {
            let mut app = loaded(Screen::Overview);
            app.handle_key(ctrl('k'));
            if paste {
                app.handle_paste(input.into());
            } else {
                type_text(&mut app, input);
            }
            let what = format!("{input:?}");
            let hits = app.goto_hits();
            assert_eq!(hits.len(), 1, "{what}: {hits:?}");
            assert_eq!(hits[0].go, Go::Command, "{what}");
            assert_eq!(
                hits[0].label,
                format!("Run in the command line: {shown}"),
                "{what}"
            );
            assert_eq!(picked(&app).go, Go::Command, "{what}");
            assert!(!format!("{:?}", app.goto).contains("hunter2"), "{what}");
            for (w, h) in [(120, 36), (80, 24)] {
                let drawn = text(&app, w, h);
                assert!(!drawn.contains("hunter2"), "{what} {w}x{h}:\n{drawn}");
                assert!(
                    drawn.contains(&format!(":{shown}")),
                    "{what} {w}x{h}:\n{drawn}"
                );
            }
            let e = app.handle_key(key(KeyCode::Enter));
            assert!(app.goto.is_none(), "{what}");
            assert!(app.views.cmdline.open, "{what}");
            assert_eq!(app.views.cmdline.shown_input(), shown, "{what}");
            assert!(!app.find.last.contains("hunter2"), "{what}");
            assert!(!app.find.text.contains("hunter2"), "{what}");
            assert!(
                app.notice
                    .as_ref()
                    .is_none_or(|n| !n.text.contains("hunter2")),
                "{what}"
            );
            assert!(!format!("{e:?}").contains("hunter2"), "{what}: {e:?}");
            for (w, h) in [(120, 36), (80, 24)] {
                let drawn = text(&app, w, h);
                assert!(!drawn.contains("hunter2"), "{what} {w}x{h}:\n{drawn}");
            }
        }
    }

    // -- The Find row's glob ---------------------------------------------------------

    #[test]
    fn the_find_row_shows_the_glob_it_sends() {
        let app = loaded(Screen::Overview);
        let label = |text: &str| {
            app.goto_hits_for(text)
                .into_iter()
                .find(|h| h.go == Go::Find)
                .unwrap()
                .label
        };
        assert_eq!(label("cart"), "Find every key matching *cart*");
        assert_eq!(label("user:1*"), "Find every key matching user:1*");
        assert_eq!(label("u?er"), "Find every key matching u?er");
        assert_eq!(label("[ab]x"), "Find every key matching [ab]x");
        assert_eq!(label(r"a\b"), r"Find every key matching *a\\b*");
        // What enter sends is what the row said.
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "user:1*");
        app.handle_key(ctrl('f'));
        assert_eq!(app.views.keys.find.as_deref(), Some("user:1*"));
    }

    // -- Saying what it did ----------------------------------------------------------

    #[test]
    fn a_row_that_clears_a_filter_says_so() {
        // A pattern the Patterns filter hides.
        let mut app = loaded(Screen::Overview);
        app.views.keys = Default::default();
        app.patterns.filter = "zzqq".into();
        open_with(&mut app, "user:{id}:feed");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyList);
        assert!(app.patterns.filter.is_empty());
        let n = app
            .notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        assert!(n.contains(CLEARED_FILTER), "{n}");
        // A key the key list's filter hides.
        let mut app = loaded(Screen::Overview);
        app.views.keys.filter = "zzqq".into();
        open_with(&mut app, "10612");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyDetail);
        let n = app
            .notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        assert!(n.contains(CLEARED_FILTER), "{n}");
        // Nothing hidden: nothing said.
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "10612");
        app.handle_key(key(KeyCode::Enter));
        assert!(
            app.notice
                .as_ref()
                .is_none_or(|n| !n.text.contains(CLEARED_FILTER))
        );
    }

    #[test]
    fn a_big_key_row_restarts_the_marquee() {
        let mut app = loaded(Screen::Overview);
        app.views.keys = Default::default();
        if let Some(r) = app.session.as_mut().and_then(|s| s.report.as_mut()) {
            r.key_sizes = Some(
                crate::model::KeySizes {
                    big_keys: fixtures::hot_keys(),
                    ttl_bytes: [0; 7],
                }
                .into(),
            );
        }
        let big = app.report().unwrap().big_keys().to_vec();
        let i = big.len().checked_sub(1).expect("a big key");
        let k = &big[i];
        // Another row selected before.
        app.views.analysis.sel = i + 1;
        for _ in 0..5 {
            app.apply(Msg::Tick);
        }
        open_with(&mut app, &k.key);
        while pick_of(&app).is_none_or(|h| {
            !matches!(
                h.go,
                Go::Key {
                    from: KeyFrom::BigKeys(_),
                    ..
                }
            ) || h.label != k.key
        }) {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::BigKeys);
        assert_eq!(app.views.analysis.sel, i);
        assert_eq!(app.views.analysis.sel_since, app.tick);
    }

    // -- A database switch -----------------------------------------------------------

    /// After `b` switched the database, the key list and the open value still hold
    /// what was read on the old one (the views are not reset on a switch): Go to lists
    /// none of those keys until they are read again.
    #[test]
    fn after_a_database_switch_go_to_lists_no_key_of_the_old_one() {
        use crate::live::LiveReply;
        use crate::live::dbs::Reply;
        use crate::model::KeyspaceInfo;
        let mut app = loaded(Screen::KeyList);
        app.views.keys.sel = 0;
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyDetail);
        app.handle_key(key(KeyCode::Esc));
        let held = |app: &App| -> Vec<String> {
            app.goto_hits_for("cart")
                .into_iter()
                .filter_map(|h| match h.go {
                    Go::Key {
                        name,
                        from: KeyFrom::List | KeyFrom::Value,
                    } => Some(name),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(held(&app).len(), 3);
        let e = app.handle_key(ch('b'));
        let [(req, _)] = &testing::requests(&e)[..] else {
            panic!("{e:?}");
        };
        let info = |db| KeyspaceInfo {
            db,
            keys: 4,
            expires: 0,
            ..KeyspaceInfo::default()
        };
        testing::reply(
            &mut app,
            *req,
            Ok(LiveReply::Dbs(Reply::Keyspace(vec![info(0), info(3)]))),
        );
        app.handle_key(ch('j'));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.session.as_ref().unwrap().profile.db, 3);
        // Still held by the views: a matter of the key list, not of Go to.
        assert_eq!(app.views.keys.rows.len(), 3);
        assert!(app.views.value.row.is_some());
        assert!(held(&app).is_empty(), "{:?}", held(&app));
    }

    // -- Snapshots ------------------------------------------------------------------

    fn snap(name: &str, app: &App) {
        for (w, h) in [(120, 36), (80, 24)] {
            insta::assert_snapshot!(
                format!("goto_{name}_{w}x{h}"),
                testing::render(app, w, h).backend()
            );
        }
    }

    #[test]
    fn snapshots() {
        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "");
        snap("empty_on_overview", &app);

        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "cart");
        snap("cart", &app);

        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "se");
        snap("se_every_group", &app);

        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "slow");
        assert_eq!(picked(&app).go, Go::Screen(Screen::SlowLog));
        snap("slow", &app);

        let mut app = loaded(Screen::Overview);
        open_with(&mut app, "zzqq");
        snap("no_match", &app);

        let mut app = loaded(Screen::Overview);
        open_with(&mut app, ":AUTH default hunter2");
        snap("command_masked", &app);

        let mut app = App::new(fixtures::profiles());
        open_with(&mut app, "");
        snap("before_connecting", &app);

        let mut app = loaded(Screen::Overview);
        let (id, scan) = {
            let s = app.session.as_ref().unwrap();
            (s.id, s.scan)
        };
        app.apply(Msg::Nodes {
            id,
            scan,
            nodes: fixtures::live_nodes(),
        });
        open_with(&mut app, "s");
        snap("cluster", &app);

        // prod-cache-01: read-only production. The Delete action picked with `tab`.
        let mut app = loaded(Screen::Patterns);
        open_with(&mut app, "del");
        while pick_of(&app).is_none_or(|h| h.section != Section::Actions) {
            app.handle_key(key(KeyCode::Tab));
        }
        let h = picked(&app);
        assert!(h.section == Section::Actions && h.off.is_some(), "{h:?}");
        snap("read_only_prod_off_action", &app);

        // The key list's selection moved under the box: its ACTIONS are off.
        let mut app = loaded(Screen::KeyList);
        open_with(&mut app, "del");
        app.views.keys.sel = 2;
        app.apply(Msg::Tick);
        while pick_of(&app).is_none_or(|h| h.section != Section::Actions) {
            app.handle_key(key(KeyCode::Tab));
        }
        assert!(app.goto_actions_moved());
        snap("actions_target_changed", &app);
    }
}
