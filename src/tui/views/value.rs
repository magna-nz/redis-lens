//! State of the key detail: one key's metadata and its value, opened with `enter` on
//! the key list (spec "Key detail"). Drawn by `ui/value.rs`; its live requests are in
//! `src/live/values.rs`.
//!
//! The metadata comes from the key list's row, so it costs no command. The value is
//! read in pages of [`PAGE_ITEMS`](crate::live::values::PAGE_ITEMS): the first when the screen shows, the next with
//! `n`, up to [`MAX_ITEMS`] held. Collections list their items, and the selected one's
//! value shows below them; `tab` moves the focus between the two so a long value can
//! be scrolled. `f` shows the value in the next format, `y` copies it as shown, `esc`
//! (handled by `App`) goes back to the key list.
//!
//! Values are held as [`Raw`] inside [`Items`], whose `Debug` shows lengths only, so
//! the derived `Debug` below never prints a value. Notices are fixed texts.
//!
//! **Edits (P3).** `e`, `+`, `t`, `R`, `d` and `D` write through the gate
//! ([`Act::write`]), only on the key detail itself (not where 4 Streams or 6 Search
//! embed this state). `e`, `+`, `t` and `R` open a small [`Form`] first; a value too big
//! or with too many lines for it goes to `$EDITOR` ([`crate::tui::editor`]) instead.
//! Each is off with a reason when the gate would refuse it or the type can't do it.
//! After a write the value and the key's TTL and length are read again.

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::keys::{list_move, plain};
use super::{Act, Input, ViewState};
use crate::live::keys::{KeyRow, Reply as KeysReply, Request as KeysRequest};
use crate::live::values::{
    At, Items, Kind, MAX_ITEMS, Reply, Request, VECTOR_SAMPLE, field_ttls_supported,
};
use crate::live::write::{Reply as WriteReply, list_marker};
use crate::live::{CANCELLED, Dest, LiveError, LiveReply, LiveRequest, ReqId};
use crate::tui::app::Screen;
use crate::tui::clipboard::CopyText;
use crate::tui::editor::EditorJob;
use crate::value::{self, CopyRefusal, Format, Raw};
use crate::write::{Decision, NewKey, Ttl as WriteTtl, WriteOp};

/// What `y` says for a value with control characters.
pub const COPY_REFUSED: &str =
    "The value has control characters, so it is not copied: press f for hex, then y.";

/// Which part `↑↓` and the other move keys work on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// The list of fields, elements, members or entries.
    #[default]
    Items,
    /// The selected item's value, which scrolls.
    Value,
}

/// The items seen so far, by their bytes, so a field or member `HSCAN` or `SSCAN`
/// returns twice is listed once. `Debug` shows only how many.
#[derive(Default)]
struct Seen(HashSet<Vec<u8>>);

impl fmt::Debug for Seen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Seen(<{} items>)", self.0.len())
    }
}

/// What the value pane drew last: its lines and its height. Set by the draw function
/// so that `End` and the scroll keys know how far the value goes.
#[derive(Debug, Default)]
pub struct Drawn {
    lines: AtomicUsize,
    height: AtomicUsize,
}

impl Drawn {
    pub fn set(&self, lines: usize, height: usize) {
        self.lines.store(lines, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
    }

    /// How far the drawn text can scroll: its lines past the height.
    pub fn max_scroll(&self) -> usize {
        let lines = self.lines.load(Ordering::Relaxed);
        lines.saturating_sub(self.height.load(Ordering::Relaxed))
    }

    /// The height drawn, at least 1: what PgUp and PgDn scroll by.
    pub fn height(&self) -> usize {
        self.height.load(Ordering::Relaxed).max(1)
    }
}

#[derive(Debug, Default)]
pub struct ValueState {
    /// The key shown, as the key list loaded it: the metadata.
    pub row: Option<KeyRow>,
    /// The pattern of the key list it was opened from.
    pub pattern: String,
    /// The `TYPE` the first page read, and the type it means.
    pub type_name: Option<String>,
    pub kind: Option<Kind>,
    /// What was read so far.
    pub items: Option<Items>,
    /// Where the next page starts; `None` once everything is read.
    next: Option<At>,
    /// Pages answered.
    pub pages: u64,
    loading: Option<ReqId>,
    /// Why the last read failed, in redis-lens's own words.
    pub error: Option<String>,
    /// The last read was refused: `n` does not ask again.
    denied: bool,
    /// `TYPE` said `none`.
    pub gone: bool,
    /// `HTTL` is sent for hash pages, and the TTL column shows.
    pub field_ttls: bool,
    /// Why field TTLs are off for the session: `HTTL` was refused or unknown. Kept when
    /// another key is opened.
    pub httl_off: Option<String>,
    /// Other commands refused (`STRLEN`).
    pub refused: Vec<LiveError>,
    /// [`MAX_ITEMS`] are held: no more are loaded.
    pub capped: bool,
    /// The format `f` picked; `None` shows each value in its detected one.
    pub format: Option<Format>,
    /// The selected item.
    pub sel: usize,
    /// The tick the selection last changed.
    pub sel_since: u64,
    pub focus: Focus,
    /// The first line of the value pane shown.
    pub scroll: usize,
    seen: Seen,
    pub drawn: Drawn,
    /// Decoded, rendered and previewed values kept between frames.
    pub memo: value::Memo,
    /// The edit form open (`e`, `+`, `t`, `R`).
    pub form: Option<Form>,
    /// The write sent and not answered yet.
    writing: Option<(ReqId, WriteOp)>,
    /// A value out in `$EDITOR`: the job, what it edits, and the value it held.
    editing: Option<Editing>,
    /// `$EDITOR` to run: `App` takes it (`key_edits.rs`).
    pub editor: Option<EditorJob>,
    /// What `App` should do after a write: `App` takes them (`key_edits.rs`).
    pub nav: Vec<Nav>,
    /// The TTL and length read after a write.
    meta_loading: Option<ReqId>,
}

impl ValueState {
    /// `enter` on a key of the key list: show `row`. `App` calls this, then shows the
    /// screen. The key already open keeps what it loaded; another starts afresh.
    pub fn open(&mut self, row: KeyRow, pattern: String, act: &mut Act<'_>) {
        if self.row.as_ref().is_some_and(|r| r.key == row.key) && self.pattern == pattern {
            return;
        }
        let httl_off = self.httl_off.take();
        *self = Self {
            row: Some(row),
            pattern,
            httl_off,
            sel_since: act.tick,
            ..Self::default()
        };
    }

    /// A value another view has read already, shown with this screen's value pane:
    /// a Search result's fields as `Items::Hash`. Nothing more is loaded (`n` says so).
    pub fn preloaded(row: KeyRow, items: Items, tick: u64) -> Self {
        Self {
            row: Some(row),
            items: Some(items),
            pages: 1,
            sel_since: tick,
            ..Self::default()
        }
    }

    /// The type the value is read as: the first page's `TYPE`, else the key list's.
    pub fn kind(&self) -> Option<Kind> {
        self.kind
            .clone()
            .or_else(|| self.row.as_ref().map(|r| Kind::from(r.key_type)))
    }

    /// True while a page is being read.
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Everything this screen reads is read.
    pub fn done(&self) -> bool {
        self.pages > 0 && self.next.is_none()
    }

    /// Items loaded: fields, elements, members or entries.
    pub fn loaded(&self) -> usize {
        self.items.as_ref().map_or(0, Items::len)
    }

    /// True when the value has a list of items above it.
    pub fn has_list(&self) -> bool {
        matches!(
            self.items,
            Some(
                Items::Hash(_)
                    | Items::List(_)
                    | Items::Set(_)
                    | Items::ZSet(_)
                    | Items::Stream(_)
                    | Items::Vector(_)
            )
        )
    }

    /// The selected index, kept in range.
    pub fn selected(&self) -> usize {
        self.sel.min(self.loaded().saturating_sub(1))
    }

    /// The selected value's bytes: a field's value, an element, a member, the string
    /// or the document. `None` for a stream entry (it has several) or nothing loaded.
    pub fn selected_value(&self) -> Option<&Raw> {
        let i = self.selected();
        match self.items.as_ref()? {
            Items::String { value, .. } => Some(value),
            Items::Json { doc, .. } => Some(doc),
            Items::Hash(f) => f.get(i).map(|f| &f.value),
            Items::List(v) | Items::Set(v) | Items::Vector(v) => v.get(i),
            Items::ZSet(m) => m.get(i).map(|m| &m.member),
            Items::Stream(_) | Items::JsonNotRead { .. } | Items::NotRead => None,
        }
    }

    /// The bytes `f` detects a format from: the selected value, or the first field of
    /// the selected stream entry.
    pub fn format_source(&self) -> Option<&Raw> {
        match self.items.as_ref()? {
            Items::Stream(e) => e.get(self.selected())?.fields.first().map(|(_, v)| v),
            _ => self.selected_value(),
        }
    }

    /// The format `raw` is shown in: the one `f` picked, else its detected one.
    pub fn shown_format(&self, d: &value::Decoded) -> Format {
        self.format.unwrap_or(d.detected)
    }

    /// Sends the next page (the first when none came). False when nothing was sent.
    fn load(&mut self, act: &mut Act<'_>) -> bool {
        let Some(row) = &self.row else {
            return false;
        };
        let first = self.pages == 0;
        if first {
            let supported = act
                .server()
                .is_some_and(|s| field_ttls_supported(s.flavor, &s.version));
            self.field_ttls = supported && self.httl_off.is_none();
        }
        let req = Request {
            key: row.key.clone(),
            kind: self.kind().unwrap_or(Kind::String),
            at: if first { None } else { self.next.clone() },
            field_ttls: self.field_ttls,
            size: row.bytes,
        };
        // On a cluster, to the owner of the key's slot.
        let dest = Dest::key(row.key.as_bytes().to_vec());
        match act.send_to(dest, LiveRequest::Value(req)) {
            Some(id) => {
                self.loading = Some(id);
                true
            }
            None => false,
        }
    }

    /// Forgets a request that will never be answered (a rescan replaced the live
    /// connection), and reads the first page when none has come yet.
    fn keep_going(&mut self, act: &mut Act<'_>) {
        if self.loading.is_some_and(|id| !act.in_flight(id)) {
            self.loading = None;
        }
        let first = self.pages == 0 && !self.gone && self.error.is_none();
        if first && self.loading.is_none() && act.live_ready() {
            self.load(act);
        }
    }

    fn take(&mut self, page: crate::live::values::Page) {
        if let Some(t) = page.type_name {
            self.kind = Some(Kind::from_type(&t));
            self.type_name = Some(t);
        }
        for e in page.refused {
            let httl =
                matches!(&e, LiveError::Denied(c) | LiveError::Unsupported(c) if c == "HTTL");
            if httl {
                self.field_ttls = false;
                self.httl_off = Some(format!("Field TTLs off: {e}."));
            } else if !self.refused.contains(&e) {
                self.refused.push(e);
            }
        }
        self.pages += 1;
        self.next = page.next;
        self.error = None;
        self.denied = false;
        let room = MAX_ITEMS.saturating_sub(self.loaded());
        let seen = &mut self.seen.0;
        let mut cut = false;
        let items = match page.items {
            Items::Hash(f) => Items::Hash(cap_vec(
                f.into_iter()
                    .filter(|f| seen.insert(f.name.as_bytes().to_vec()))
                    .collect(),
                room,
                &mut cut,
            )),
            Items::Set(v) => Items::Set(cap_vec(
                v.into_iter()
                    .filter(|m| seen.insert(m.as_bytes().to_vec()))
                    .collect(),
                room,
                &mut cut,
            )),
            Items::List(v) => Items::List(cap_vec(v, room, &mut cut)),
            Items::Vector(v) => Items::Vector(cap_vec(v, room, &mut cut)),
            Items::ZSet(v) => Items::ZSet(cap_vec(v, room, &mut cut)),
            Items::Stream(v) => Items::Stream(cap_vec(v, room, &mut cut)),
            other => other,
        };
        if cut || (self.loaded() + items.len() >= MAX_ITEMS && self.next.is_some()) {
            self.capped = true;
        }
        self.items = Some(match (self.items.take(), items) {
            (Some(Items::Hash(mut a)), Items::Hash(b)) => {
                a.extend(b);
                Items::Hash(a)
            }
            (Some(Items::List(mut a)), Items::List(b)) => {
                a.extend(b);
                Items::List(a)
            }
            (Some(Items::Set(mut a)), Items::Set(b)) => {
                a.extend(b);
                Items::Set(a)
            }
            (Some(Items::ZSet(mut a)), Items::ZSet(b)) => {
                a.extend(b);
                Items::ZSet(a)
            }
            (Some(Items::Stream(mut a)), Items::Stream(b)) => {
                a.extend(b);
                Items::Stream(a)
            }
            (_, b) => b,
        });
    }

    /// The move keys on the value pane: scroll it.
    fn scroll_key(&mut self, key: &KeyEvent) -> bool {
        let max = self.drawn.max_scroll();
        let page = self.drawn.height();
        let at = self.scroll.min(max);
        self.scroll = match (key.code, plain(key)) {
            (KeyCode::Up, _) | (_, Some('k')) => at.saturating_sub(1),
            (KeyCode::Down, _) | (_, Some('j')) => (at + 1).min(max),
            (KeyCode::PageUp, _) => at.saturating_sub(page),
            (KeyCode::PageDown, _) => (at + page).min(max),
            (KeyCode::Home, _) | (_, Some('g')) => 0,
            (KeyCode::End, _) | (_, Some('G')) => max,
            _ => return false,
        };
        true
    }

    fn more(&mut self, act: &mut Act<'_>) {
        if self.loading.is_some_and(|id| act.in_flight(id)) {
            act.info("Still loading the value…");
        } else if self.gone {
            act.info("The key no longer exists.");
        } else if self.denied {
            if let Some(e) = &self.error {
                act.info(e.clone());
            }
        } else if self.capped || (self.loaded() >= MAX_ITEMS && self.next.is_some()) {
            act.info(format!(
                "At most {} items are held, so no more are loaded.",
                crate::format::grouped(MAX_ITEMS as u64)
            ));
        } else if self.done() {
            act.info(match self.kind() {
                Some(Kind::Vector) => format!(
                    "Vector sets can't be read in pages: these are {VECTOR_SAMPLE} random elements."
                ),
                Some(Kind::String | Kind::Json) => "The value is loaded.".to_string(),
                Some(Kind::Other(_)) => "This type's value is not read.".to_string(),
                _ => format!(
                    "All {} items are loaded.",
                    crate::format::grouped(self.loaded() as u64)
                ),
            });
        } else if !self.load(act) {
            act.info("The live connection is not open, so no more can be loaded.");
        }
    }

    fn copy(&mut self, act: &mut Act<'_>) {
        if matches!(self.items, Some(Items::Stream(_))) {
            act.info("A stream entry has several fields, so y does not copy it.");
            return;
        }
        let Some(raw) = self.selected_value() else {
            act.info("Nothing selected to copy.");
            return;
        };
        let d = self.memo.decode(raw);
        match value::copy_text(&d, self.shown_format(&d)) {
            Ok(text) => act.copy(CopyText::value(&text)),
            Err(CopyRefusal::ControlCharacters) => act.info(COPY_REFUSED),
        }
    }
}

/// Why a read failed, for the screen: the error's own fixed words.
pub fn read_error(e: &LiveError) -> String {
    match e {
        LiveError::Closed => "The live connection is closed.".to_string(),
        e => format!("Can't read the value: {e}."),
    }
}

// ---------------------------------------------------------------------------
// Edits (P3)
// ---------------------------------------------------------------------------

/// The most bytes a value is edited in the inline prompt; a longer one, or one with a
/// newline or another control character, opens `$EDITOR`.
pub const INLINE_MAX: usize = 4 * 1024;
/// The longest TTL `t` takes: 100 years.
pub const TTL_MAX_SECS: u64 = 100 * 365 * 86_400;

/// What `App` should do after a write on the key detail.
#[derive(Debug, Clone, PartialEq)]
pub enum Nav {
    /// `D` deleted the key: back to the key list, without it.
    Deleted(String),
    /// `R` renamed the key.
    Renamed { old: String, new: String },
    /// The key's TTL and length read again after a write, for the key list's row.
    Row(KeyRow),
}

/// A value out in `$EDITOR`.
#[derive(Debug)]
struct Editing {
    job: u64,
    key: String,
    target: Target,
    before: Raw,
}

static NEXT_JOB: AtomicU64 = AtomicU64::new(1);
static NEXT_MARKER: AtomicU64 = AtomicU64::new(1);

/// What `e` edits, and how the new value is written.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// The whole string, compared with `old` before it is set.
    String {
        old: Raw,
    },
    Field {
        field: Raw,
    },
    List {
        index: u64,
        old: Raw,
    },
    Member {
        old: Raw,
    },
    /// A sorted set member's score.
    Score {
        member: Raw,
    },
    Json,
}

impl Target {
    /// The op that writes `value` (typed, or from `$EDITOR`).
    pub fn op(&self, key: &str, value: Raw) -> Result<WriteOp, String> {
        let key = key.to_string();
        Ok(match self.clone() {
            Target::String { old } => WriteOp::SetString { key, old, value },
            Target::Field { field } => WriteOp::HSet { key, field, value },
            Target::List { index, old } => WriteOp::LSet {
                key,
                index,
                old,
                value,
            },
            Target::Member { old } => WriteOp::SReplace { key, old, value },
            Target::Score { member } => WriteOp::ZScore {
                key,
                member,
                score: parse_score(&String::from_utf8_lossy(value.as_bytes()))?,
            },
            Target::Json => WriteOp::JsonSet { key, value },
        })
    }

    fn label(&self) -> &'static str {
        match self {
            Target::String { .. } | Target::Field { .. } => "value",
            Target::List { .. } => "element",
            Target::Member { .. } => "member",
            Target::Score { .. } => "score",
            Target::Json => "document",
        }
    }
}

/// What `+` adds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddTo {
    Hash,
    List,
    Set,
    ZSet,
    Stream,
}

/// What a [`Form`] sends.
#[derive(Debug, Clone, PartialEq)]
pub enum FormAction {
    /// `e`.
    Edit(Target),
    /// `+`.
    Add(AddTo),
    /// `t`: of the key, or of `field` when the form's first choice says so.
    Ttl { field: Option<Raw> },
    /// `R`.
    Rename,
    /// `N` on the key list.
    NewKey,
}

/// The types `N` creates, as the form's choice shows them.
pub const NEW_KEY_TYPES: [&str; 6] = ["string", "hash", "list", "set", "zset", "stream"];
const TTL_FOR: [&str; 2] = ["key", "field"];

/// One field of a [`Form`].
#[derive(Clone)]
pub struct FormField {
    pub label: &'static str,
    /// What is typed. A value: on screen only.
    pub text: String,
    /// For a choice, its options (empty for text), and which is picked.
    pub choices: &'static [&'static str],
    pub choice: usize,
}

impl FormField {
    fn text(label: &'static str, text: impl Into<String>) -> Self {
        Self {
            label,
            text: text.into(),
            choices: &[],
            choice: 0,
        }
    }

    fn choice(label: &'static str, choices: &'static [&'static str], choice: usize) -> Self {
        Self {
            label,
            text: String::new(),
            choices,
            choice,
        }
    }

    pub fn is_choice(&self) -> bool {
        !self.choices.is_empty()
    }

    /// The field as drawn: the choice, or the text with control characters escaped.
    pub fn shown(&self) -> String {
        if self.is_choice() {
            self.choices[self.choice.min(self.choices.len() - 1)].to_string()
        } else {
            value::escape(&self.text)
        }
    }

    fn raw(&self) -> Raw {
        Raw::from(self.text.as_str())
    }
}

/// A small form for an edit: its fields are typed one at a time; `tab` and `enter` move
/// to the next, `enter` on the last sends. Its texts are values: `Debug` shows only
/// what it does and how many fields it has.
#[derive(Clone)]
pub struct Form {
    /// The title, safe to draw.
    pub title: String,
    /// On the right of the title, safe to draw: the key, or the pattern.
    pub right: String,
    pub fields: Vec<FormField>,
    /// The field typed in.
    pub at: usize,
    /// Lines under the fields, safe to draw.
    pub notes: Vec<String>,
    /// Why the last `enter` sent nothing, in redis-lens's words.
    pub error: Option<String>,
    pub action: FormAction,
    /// For an edit, the text it started with: sending it unchanged sends nothing.
    original: Option<String>,
}

impl fmt::Debug for Form {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Form({:?}, <{} fields>)", self.action, self.fields.len())
    }
}

/// What a key did to a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKey {
    Typing,
    Cancel,
    Send,
}

impl Form {
    fn new(title: String, right: &str, fields: Vec<FormField>, action: FormAction) -> Self {
        Self {
            title,
            right: value::escape(right),
            fields,
            at: 0,
            notes: Vec::new(),
            error: None,
            action,
            original: None,
        }
    }

    /// `N` on the key list of `pattern`: the name starts with the pattern's text before
    /// its first placeholder.
    pub fn new_key(pattern: &str) -> Self {
        let prefix = pattern.split('{').next().unwrap_or("");
        let mut fields = vec![
            FormField::text("name", prefix),
            FormField::choice("type", &NEW_KEY_TYPES, 0),
        ];
        fields.extend(new_key_fields(0));
        let mut f = Self::new("Add key".into(), pattern, fields, FormAction::NewKey);
        f.notes
            .push("Never overwrites: an existing key is left as it is.".into());
        f
    }

    /// The key bar while the form is open.
    pub fn input(&self) -> Input {
        let f = &self.fields[self.at.min(self.fields.len() - 1)];
        Input {
            label: f.label,
            text: f.shown(),
            hint: if f.is_choice() {
                "  ←→ choose · enter next · esc cancel"
            } else if self.fields.len() > 1 {
                "  tab next · enter next/send · esc cancel"
            } else {
                "  enter send · esc cancel · ctrl-u clear"
            },
        }
    }

    /// A key while the form is open.
    pub fn key(&mut self, key: &KeyEvent) -> FormKey {
        let n = self.fields.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let at = self.at.min(n - 1);
        let f = &mut self.fields[at];
        let picked = f.choice;
        match key.code {
            KeyCode::Esc => return FormKey::Cancel,
            KeyCode::Enter if at + 1 >= n => return FormKey::Send,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.at = (at + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.at = (at + n - 1) % n,
            KeyCode::Left if f.is_choice() => {
                f.choice = (f.choice + f.choices.len() - 1) % f.choices.len();
            }
            KeyCode::Right | KeyCode::Char(' ') if f.is_choice() => {
                f.choice = (f.choice + 1) % f.choices.len();
            }
            KeyCode::Char(c) if f.is_choice() && !ctrl => {
                let c = c.to_ascii_lowercase();
                if let Some(i) = f.choices.iter().position(|o| o.starts_with(c)) {
                    f.choice = i;
                }
            }
            KeyCode::Backspace => {
                f.text.pop();
            }
            KeyCode::Char('u') if ctrl => f.text.clear(),
            KeyCode::Char(c)
                if !ctrl && !c.is_control() && f.text.len() + c.len_utf8() <= INLINE_MAX =>
            {
                f.text.push(c);
            }
            _ => {}
        }
        if self.fields[at].choice != picked {
            self.chose(at);
        }
        self.error = None;
        FormKey::Typing
    }

    /// Pasted text, into the field typed in, without control characters.
    pub fn paste(&mut self, text: &str) {
        let at = self.at.min(self.fields.len() - 1);
        let f = &mut self.fields[at];
        if f.is_choice() {
            return;
        }
        for c in text.chars().filter(|c| !c.is_control()) {
            if f.text.len() + c.len_utf8() > INLINE_MAX {
                self.error =
                    Some("Cut at 4 KiB: a longer value is edited with e in $EDITOR.".into());
                break;
            }
            f.text.push(c);
        }
    }

    /// A choice changed: `N`'s type picks the fields after it.
    fn chose(&mut self, at: usize) {
        if self.action == FormAction::NewKey && at == 1 {
            let mut fields = new_key_fields(self.fields[1].choice);
            for (new, old) in fields.iter_mut().zip(self.fields.iter().skip(2)) {
                new.text = old.text.clone();
            }
            self.fields.truncate(2);
            self.fields.extend(fields);
        }
    }

    fn text(&self, i: usize) -> &str {
        self.fields.get(i).map_or("", |f| f.text.as_str())
    }

    fn raw(&self, i: usize) -> Raw {
        Raw::from(self.text(i))
    }

    /// True when the form edits a value and its text is still the one it started with.
    pub fn unchanged(&self) -> bool {
        self.original.as_deref().is_some_and(|o| o == self.text(0))
    }

    /// The op the form sends for `key` (for `N`, the name typed), or which field is
    /// wrong and why.
    pub fn op(&self, key: &str) -> Result<WriteOp, (usize, String)> {
        let k = || key.to_string();
        let op = match &self.action {
            FormAction::Edit(t) => t.op(key, self.fields[0].raw()).map_err(|e| (0, e))?,
            FormAction::Add(to) => match to {
                AddTo::Hash => WriteOp::HSetNx {
                    key: k(),
                    field: self.raw(0),
                    value: self.raw(1),
                },
                AddTo::List => WriteOp::RPush {
                    key: k(),
                    value: self.raw(0),
                },
                AddTo::Set => WriteOp::SAdd {
                    key: k(),
                    member: self.raw(0),
                },
                AddTo::ZSet => WriteOp::ZAdd {
                    key: k(),
                    member: self.raw(0),
                    score: parse_score(self.text(1)).map_err(|e| (1, e))?,
                },
                AddTo::Stream => {
                    if self.text(0).is_empty() {
                        return Err((0, "Type the field's name.".into()));
                    }
                    WriteOp::XAdd {
                        key: k(),
                        fields: vec![(self.raw(0), self.raw(1))],
                    }
                }
            },
            FormAction::Ttl { field } => {
                let (i, of_field) = match self.fields.first() {
                    Some(f) if f.is_choice() => (1, f.choice == 1),
                    _ => (0, false),
                };
                let ttl = parse_ttl(self.text(i)).map_err(|e| (i, e))?;
                match (of_field, field) {
                    (true, Some(field)) => WriteOp::FieldTtl {
                        key: k(),
                        field: field.clone(),
                        ttl,
                    },
                    _ => WriteOp::KeyTtl { key: k(), ttl },
                }
            }
            FormAction::Rename => {
                let new = self.text(0);
                if new.is_empty() {
                    return Err((0, "Type the new name.".into()));
                }
                WriteOp::Rename {
                    key: k(),
                    new: new.to_string(),
                }
            }
            FormAction::NewKey => {
                let name = self.text(0);
                if name.is_empty() {
                    return Err((0, "Type the key's name.".into()));
                }
                let value = match self.fields[1].choice {
                    0 => NewKey::String(self.raw(2)),
                    1 => NewKey::Hash {
                        field: self.raw(2),
                        value: self.raw(3),
                    },
                    2 => NewKey::List(self.raw(2)),
                    3 => NewKey::Set(self.raw(2)),
                    4 => NewKey::ZSet {
                        member: self.raw(2),
                        score: parse_score(self.text(3)).map_err(|e| (3, e))?,
                    },
                    _ => {
                        if self.text(2).is_empty() {
                            return Err((2, "Type the field's name.".into()));
                        }
                        NewKey::Stream {
                            field: self.raw(2),
                            value: self.raw(3),
                        }
                    }
                };
                WriteOp::NewKey {
                    key: name.to_string(),
                    value,
                }
            }
        };
        match op.invalid() {
            Some(why) => Err((0, why)),
            None => Ok(op),
        }
    }
}

/// The value fields of `N` for the type at `t` in [`NEW_KEY_TYPES`].
fn new_key_fields(t: usize) -> Vec<FormField> {
    match t {
        0 => vec![FormField::text("value", "")],
        1 => vec![FormField::text("field", ""), FormField::text("value", "")],
        2 => vec![FormField::text("element", "")],
        3 => vec![FormField::text("member", "")],
        4 => vec![FormField::text("member", ""), FormField::text("score", "")],
        _ => vec![FormField::text("field", ""), FormField::text("value", "")],
    }
}

/// A TTL as typed: seconds, or a number with `s`, `m`, `h`, `d` or `w`, several in a
/// row (`1h30m`); `-`, `persist` or `none` removes it. Zero is refused: it would delete
/// the key.
pub fn parse_ttl(text: &str) -> Result<WriteTtl, String> {
    let t = text.trim();
    if t == "-" || t.eq_ignore_ascii_case("persist") || t.eq_ignore_ascii_case("none") {
        return Ok(WriteTtl::Persist);
    }
    let bad = || "Type a TTL such as 90s, 10m, 24h or 7d, or - to remove it.".to_string();
    if t.is_empty() {
        return Err(bad());
    }
    let mut total: u64 = 0;
    let mut num: Option<u64> = None;
    for c in t.chars() {
        if let Some(d) = c.to_digit(10) {
            num = Some(
                num.unwrap_or(0)
                    .checked_mul(10)
                    .and_then(|n| n.checked_add(u64::from(d)))
                    .ok_or_else(bad)?,
            );
            continue;
        }
        let unit = match c.to_ascii_lowercase() {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            'w' => 604_800,
            ' ' => continue,
            _ => return Err(bad()),
        };
        let n = num.take().ok_or_else(bad)?;
        total = n
            .checked_mul(unit)
            .and_then(|s| total.checked_add(s))
            .ok_or_else(bad)?;
    }
    if let Some(n) = num {
        total = total.checked_add(n).ok_or_else(bad)?;
    }
    if total == 0 {
        return Err("A TTL of 0 would delete the key: D deletes it.".into());
    }
    if total > TTL_MAX_SECS {
        return Err("That TTL is too long: at most 100 years.".into());
    }
    Ok(WriteTtl::Seconds(total))
}

/// A score as typed: a number, `inf`, `+inf` or `-inf`.
pub fn parse_score(text: &str) -> Result<f64, String> {
    match text.trim().parse::<f64>() {
        Ok(s) if !s.is_nan() => Ok(s),
        _ => Err("Type the score as a number, such as 1.5, or inf or -inf.".into()),
    }
}

/// The text of a value the inline prompt can edit: valid UTF-8 of at most
/// [`INLINE_MAX`] bytes, with nothing that would be drawn as an escape (no newline,
/// tab or other control character).
pub fn inline_text(raw: &Raw) -> Option<String> {
    if raw.len() > INLINE_MAX {
        return None;
    }
    let s = std::str::from_utf8(raw.as_bytes()).ok()?;
    (value::escape(s) == s).then(|| s.to_string())
}

/// The key list's page request for `row`'s key alone: a walk that is done (cursor 0)
/// with the key as its carry sends no `SCAN`, only the metadata pipeline for that key
/// (`TYPE`, `PTTL`, `OBJECT ENCODING`, the length, and `MEMORY USAGE` and `OBJECT
/// FREQ` when the key list read them).
pub fn meta_request(row: &KeyRow, pattern: &str) -> LiveRequest {
    LiveRequest::Keys(KeysRequest::Page {
        pattern: pattern.to_string(),
        cursor: 0,
        carry: vec![row.key.clone()],
        sizes: row.bytes.is_some(),
        freq: row.freq.is_some(),
    })
}

/// A field name, member or element for a form's title, safe to draw.
fn shown(raw: &Raw) -> String {
    value::escape(&String::from_utf8_lossy(raw.as_bytes()))
}

impl ValueState {
    fn key_name(&self) -> Option<String> {
        self.row.as_ref().map(|r| r.key.clone())
    }

    /// True when a write may start: the key is there, the gate would not refuse
    /// `probe` (a read-only profile, a command the server refused), the live connection
    /// is open and no other write waits for its answer. Otherwise says why.
    fn may_write(&self, probe: &WriteOp, act: &mut Act<'_>) -> bool {
        if let Decision::Refuse(why) = act.write_decision(probe) {
            act.error(why);
            return false;
        }
        if self.gone {
            act.info("The key no longer exists.");
            return false;
        }
        if !act.live_ready() {
            act.error("Not sent: the live connection is not open.");
            return false;
        }
        if self
            .writing
            .as_ref()
            .is_some_and(|(id, _)| act.in_flight(*id))
        {
            act.info("The last write has not answered yet.");
            return false;
        }
        true
    }

    /// The edit keys, on the key detail only. True when `c` is one.
    fn edit_key(&mut self, c: char, act: &mut Act<'_>) -> bool {
        if act.screen != Screen::KeyDetail || self.row.is_none() {
            return false;
        }
        match c {
            'e' => self.start_edit(act),
            '+' => self.start_add(act),
            't' => self.start_ttl(act),
            'R' => self.start_rename(act),
            'd' => self.delete_item(act),
            'D' => self.delete_key(act),
            _ => return false,
        }
        true
    }

    /// What `e` edits, and the value it holds now; or why it can't.
    fn edit_target(&self) -> Result<(Target, Raw, String), String> {
        let i = self.selected();
        let Some(items) = &self.items else {
            return Err(if self.gone {
                "The key no longer exists.".into()
            } else {
                "Nothing is loaded to edit yet.".into()
            });
        };
        Ok(match items {
            Items::String { value, len } => {
                if *len != Some(value.len() as u64) {
                    return Err(match len {
                        None => "e is off: the string's length is unknown (STRLEN was \
                                 refused), and an edit needs the whole value."
                            .into(),
                        Some(_) => "e is off: only the first 64 KiB of this string is loaded, \
                                    and an edit needs the whole value."
                            .into(),
                    });
                }
                (
                    Target::String { old: value.clone() },
                    value.clone(),
                    "Edit value".into(),
                )
            }
            Items::Json { doc, len } => {
                if *len > doc.len() {
                    return Err("e is off: only the first 64 KiB of this document is \
                                loaded, and an edit needs the whole document."
                        .into());
                }
                (Target::Json, doc.clone(), "Edit document".into())
            }
            Items::Hash(f) => {
                let f = f.get(i).ok_or("No field is selected.")?;
                (
                    Target::Field {
                        field: f.name.clone(),
                    },
                    f.value.clone(),
                    format!("Edit field {}", shown(&f.name)),
                )
            }
            Items::List(v) => {
                let e = v.get(i).ok_or("No element is selected.")?;
                (
                    Target::List {
                        index: i as u64,
                        old: e.clone(),
                    },
                    e.clone(),
                    format!("Edit element #{i}"),
                )
            }
            Items::Set(v) => {
                let m = v.get(i).ok_or("No member is selected.")?;
                (
                    Target::Member { old: m.clone() },
                    m.clone(),
                    "Edit member".into(),
                )
            }
            Items::ZSet(v) => {
                let m = v.get(i).ok_or("No member is selected.")?;
                (
                    Target::Score {
                        member: m.member.clone(),
                    },
                    m.score.clone(),
                    format!("Edit the score of {}", shown(&m.member)),
                )
            }
            Items::Stream(_) => {
                return Err(
                    "A stream entry can't be edited: + adds an entry, d deletes \
                            this one."
                        .into(),
                );
            }
            Items::Vector(_) => return Err("redis-lens does not edit vector sets.".into()),
            Items::JsonNotRead { .. } => {
                return Err("The document is not read, so it can't be edited here.".into());
            }
            Items::NotRead => {
                return Err(format!(
                    "redis-lens does not edit values of type {}.",
                    value::escape(self.type_name.as_deref().unwrap_or("this"))
                ));
            }
        })
    }

    /// `e`.
    fn start_edit(&mut self, act: &mut Act<'_>) {
        let Some(key) = self.key_name() else {
            return;
        };
        let (target, now, title) = match self.edit_target() {
            Ok(t) => t,
            Err(why) => {
                act.info(why);
                return;
            }
        };
        let Ok(probe) = target.op(&key, now.clone()).or_else(|_| {
            // A score that does not parse as typed still probes the command.
            target.op(&key, Raw::from("0"))
        }) else {
            return;
        };
        if !self.may_write(&probe, act) {
            return;
        }
        let ext = if target == Target::Json {
            "json"
        } else {
            "txt"
        };
        match inline_text(&now) {
            Some(text) => {
                let label = target.label();
                let mut form = Form::new(
                    title,
                    &key,
                    vec![FormField::text(label, text.clone())],
                    FormAction::Edit(target.clone()),
                );
                form.original = Some(text);
                if matches!(target, Target::String { .. } | Target::List { .. }) {
                    form.notes
                        .push("Sent only if the value is still the one loaded.".into());
                }
                if matches!(target, Target::Member { .. }) {
                    form.notes
                        .push("The member is removed and the new one added.".into());
                }
                self.form = Some(form);
            }
            None => {
                let job = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
                self.editor = Some(EditorJob {
                    id: job,
                    value: now.clone(),
                    ext,
                });
                self.editing = Some(Editing {
                    job,
                    key,
                    target,
                    before: now,
                });
            }
        }
    }

    /// `$EDITOR` ended for `job`.
    pub fn edited(&mut self, job: u64, result: Result<Raw, String>, act: &mut Act<'_>) {
        let Some(e) = self.editing.take_if(|e| e.job == job) else {
            return;
        };
        match result {
            Err(why) => act.error(format!("Not edited: {why}.")),
            Ok(after) if after == e.before => act.info("Not changed, so nothing was sent."),
            Ok(after) => {
                if self.row.as_ref().is_none_or(|r| r.key != e.key) {
                    act.error("Not sent: another key is shown now.");
                    return;
                }
                match e.target.op(&e.key, after) {
                    Ok(op) => self.send_write(op, act),
                    Err(why) => act.error(why),
                }
            }
        }
    }

    /// `+`.
    fn start_add(&mut self, act: &mut Act<'_>) {
        let Some(key) = self.key_name() else {
            return;
        };
        let k = || key.clone();
        let (to, title, fields, note, probe) = match self.kind() {
            Some(Kind::Hash) => (
                AddTo::Hash,
                "Add field",
                vec![FormField::text("field", ""), FormField::text("value", "")],
                "HSETNX never overwrites a field: e edits one.",
                WriteOp::HSetNx {
                    key: k(),
                    field: Raw::from(""),
                    value: Raw::from(""),
                },
            ),
            Some(Kind::List) => (
                AddTo::List,
                "Add element",
                vec![FormField::text("element", "")],
                "Added at the end (RPUSH).",
                WriteOp::RPush {
                    key: k(),
                    value: Raw::from(""),
                },
            ),
            Some(Kind::Set) => (
                AddTo::Set,
                "Add member",
                vec![FormField::text("member", "")],
                "A member that is there already is left as it is.",
                WriteOp::SAdd {
                    key: k(),
                    member: Raw::from(""),
                },
            ),
            Some(Kind::ZSet) => (
                AddTo::ZSet,
                "Add member",
                vec![FormField::text("member", ""), FormField::text("score", "")],
                "ZADD NX: a member that is there already keeps its score.",
                WriteOp::ZAdd {
                    key: k(),
                    member: Raw::from(""),
                    score: 0.0,
                },
            ),
            Some(Kind::Stream) => (
                AddTo::Stream,
                "Add entry",
                vec![FormField::text("field", ""), FormField::text("value", "")],
                "XADD with an ID the server picks (*).",
                WriteOp::XAdd {
                    key: k(),
                    fields: vec![(Raw::from("f"), Raw::from(""))],
                },
            ),
            Some(Kind::String | Kind::Json) => {
                act.info("+ adds to a hash, list, set, sorted set or stream: e edits this value.");
                return;
            }
            _ => {
                act.info("redis-lens does not add to this type.");
                return;
            }
        };
        if !self.may_write(&probe, act) {
            return;
        }
        let mut form = Form::new(title.into(), &key, fields, FormAction::Add(to));
        form.notes.push(note.into());
        self.form = Some(form);
    }

    /// The selected hash field, and why field TTLs are off when they are.
    fn ttl_field(&self) -> (Option<Raw>, Option<String>) {
        let Some(Items::Hash(fields)) = &self.items else {
            return (None, None);
        };
        if !self.field_ttls {
            let why = self.httl_off.clone().unwrap_or_else(|| {
                "Field TTLs need Redis 7.4 or later, or Valkey 9.0 or later.".into()
            });
            return (None, Some(why));
        }
        (fields.get(self.selected()).map(|f| f.name.clone()), None)
    }

    /// `t`.
    fn start_ttl(&mut self, act: &mut Act<'_>) {
        let Some(key) = self.key_name() else {
            return;
        };
        let probe = WriteOp::KeyTtl {
            key: key.clone(),
            ttl: WriteTtl::Seconds(60),
        };
        if !self.may_write(&probe, act) {
            return;
        }
        let (field, off) = self.ttl_field();
        let mut fields = Vec::new();
        let mut title = "Set the key's TTL".to_string();
        if let Some(f) = &field {
            fields.push(FormField::choice("of", &TTL_FOR, 0));
            title = format!("Set the TTL of the key or field {}", shown(f));
        }
        fields.push(FormField::text("TTL", ""));
        let mut form = Form::new(title, &key, fields, FormAction::Ttl { field });
        form.notes
            .push("90s, 10m, 24h or 7d sets it; - or persist removes it.".into());
        if let Some(why) = off {
            form.notes.push(why);
        }
        self.form = Some(form);
    }

    /// `R`.
    fn start_rename(&mut self, act: &mut Act<'_>) {
        let Some(key) = self.key_name() else {
            return;
        };
        let probe = WriteOp::Rename {
            key: key.clone(),
            new: format!("{key}:"),
        };
        if !self.may_write(&probe, act) {
            return;
        }
        let mut form = Form::new(
            "Rename key".into(),
            &key,
            vec![FormField::text("new name", key.clone())],
            FormAction::Rename,
        );
        form.notes
            .push("RENAMENX: an existing key is never overwritten.".into());
        self.form = Some(form);
    }

    /// `d`: the selected field, element, member or entry.
    fn delete_item(&mut self, act: &mut Act<'_>) {
        let Some(key) = self.key_name() else {
            return;
        };
        let i = self.selected();
        let op = match &self.items {
            Some(Items::Hash(f)) => f.get(i).map(|f| WriteOp::HDel {
                key: key.clone(),
                field: f.name.clone(),
            }),
            Some(Items::Set(v)) => v.get(i).map(|m| WriteOp::SRem {
                key: key.clone(),
                member: m.clone(),
            }),
            Some(Items::ZSet(v)) => v.get(i).map(|m| WriteOp::ZRem {
                key: key.clone(),
                member: m.member.clone(),
            }),
            Some(Items::Stream(v)) => v.get(i).map(|e| WriteOp::XDel {
                key: key.clone(),
                id: e.id.clone(),
            }),
            Some(Items::List(v)) => v.get(i).map(|e| WriteOp::LDel {
                key: key.clone(),
                index: i as u64,
                old: e.clone(),
                marker: list_marker(NEXT_MARKER.fetch_add(1, Ordering::Relaxed)),
            }),
            Some(Items::String { .. } | Items::Json { .. } | Items::JsonNotRead { .. }) => {
                act.info("d deletes a field, element, member or entry: D deletes the key.");
                return;
            }
            Some(Items::Vector(_) | Items::NotRead) => {
                act.info("redis-lens does not delete items of this type: D deletes the key.");
                return;
            }
            None => None,
        };
        let Some(op) = op else {
            act.info(if self.gone {
                "The key no longer exists."
            } else {
                "Nothing is selected to delete."
            });
            return;
        };
        if self.may_write(&op, act) {
            self.send_write(op, act);
        }
    }

    /// `D`: the key.
    fn delete_key(&mut self, act: &mut Act<'_>) {
        let Some(key) = self.key_name() else {
            return;
        };
        let op = WriteOp::Unlink { key };
        if self.may_write(&op, act) {
            self.send_write(op, act);
        }
    }

    fn send_write(&mut self, op: WriteOp, act: &mut Act<'_>) {
        if let Some(id) = act.write(op.clone()) {
            self.writing = Some((id, op));
        }
    }

    /// A key while the form is open.
    fn form_key(&mut self, key: &KeyEvent, act: &mut Act<'_>) {
        let name = self.key_name();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        match form.key(key) {
            FormKey::Typing => {}
            FormKey::Cancel => {
                self.form = None;
                act.info(CANCELLED);
            }
            FormKey::Send => {
                let Some(name) = name else {
                    self.form = None;
                    return;
                };
                if form.unchanged() {
                    self.form = None;
                    act.info("Not changed, so nothing was sent.");
                    return;
                }
                match form.op(&name) {
                    Err((at, why)) => {
                        form.at = at;
                        form.error = Some(why);
                    }
                    Ok(op) => {
                        self.form = None;
                        self.send_write(op, act);
                    }
                }
            }
        }
    }

    /// Reads the value again from its first page, keeping the selection where it was.
    fn reload(&mut self, act: &mut Act<'_>) {
        self.items = None;
        self.next = None;
        self.pages = 0;
        self.seen = Seen::default();
        self.capped = false;
        self.error = None;
        self.denied = false;
        self.gone = false;
        self.loading = None;
        self.scroll = 0;
        self.load(act);
    }

    /// Reads the key's TTL and length again.
    /// The key list's own read of one key ([`meta_request`]): its TTL, size, encoding,
    /// length and LFU counter, as the key list shows them.
    fn read_meta(&mut self, act: &mut Act<'_>) {
        let Some(row) = &self.row else {
            return;
        };
        let dest = Dest::key(row.key.as_bytes().to_vec());
        self.meta_loading = act.send_to(dest, meta_request(row, &self.pattern));
    }

    fn meta_done(&mut self, result: Result<LiveReply, LiveError>) {
        let Ok(LiveReply::Keys(KeysReply::Page(page))) = result else {
            return;
        };
        let Some(r) = self.row.as_mut() else {
            return;
        };
        match page.rows.into_iter().find(|n| n.key == r.key) {
            Some(n) => {
                r.ttl = n.ttl;
                r.bytes = n.bytes.or(r.bytes);
                r.encoding = n.encoding.or(r.encoding.take());
                r.length = n.length.or(r.length);
                r.freq = n.freq.or(r.freq);
                self.nav.push(Nav::Row(r.clone()));
            }
            // Not unreadable, just not there: `TYPE` said `none`.
            None if page.unreadable == 0 => {
                self.gone = true;
                self.items = None;
                self.next = None;
            }
            None => {}
        }
    }

    /// The answer to the write `op`.
    fn write_done(&mut self, op: WriteOp, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        let reply = match result {
            Ok(LiveReply::Write(r)) => r,
            Ok(_) => return,
            // App said so already.
            Err(LiveError::Failed(s)) if s == CANCELLED => return,
            Err(e) => {
                act.error(format!("Not written: {e}."));
                return;
            }
        };
        if reply == WriteReply::Changed {
            act.error("Not written: the value changed since it was loaded. It is read again now.");
            self.reload(act);
            self.read_meta(act);
            return;
        }
        let n = match reply {
            WriteReply::Int(n) => Some(n),
            _ => None,
        };
        let mut reload = true;
        match &op {
            WriteOp::Unlink { key } => {
                if n == Some(0) {
                    act.info("The key no longer exists.");
                } else {
                    act.info(format!("Deleted {}.", value::escape(key)));
                }
                let httl_off = self.httl_off.take();
                *self = Self {
                    httl_off,
                    ..Self::default()
                };
                self.nav.push(Nav::Deleted(key.clone()));
                return;
            }
            WriteOp::Rename { key, new } => {
                if n == Some(0) {
                    act.error(format!(
                        "Not renamed: {} exists already, and RENAMENX never overwrites a key.",
                        value::escape(new)
                    ));
                } else {
                    if let Some(r) = self.row.as_mut() {
                        r.key = new.clone();
                    }
                    self.nav.push(Nav::Renamed {
                        old: key.clone(),
                        new: new.clone(),
                    });
                    act.info(format!("Renamed to {}.", value::escape(new)));
                }
                return;
            }
            WriteOp::KeyTtl { ttl, .. } => {
                reload = false;
                match (ttl, n) {
                    (WriteTtl::Persist, Some(0)) => act.info("The key had no TTL."),
                    (WriteTtl::Seconds(_), Some(0)) => {
                        act.error("Not set: the key no longer exists.");
                    }
                    (WriteTtl::Seconds(s), _) => act.info(format!(
                        "TTL set to {}.",
                        crate::format::duration(std::time::Duration::from_secs(*s))
                    )),
                    (WriteTtl::Persist, _) => act.info("TTL removed: the key no longer expires."),
                }
            }
            WriteOp::FieldTtl { ttl, .. } => act.info(match (ttl, n) {
                (_, Some(-2)) => "Not set: the field no longer exists.",
                (WriteTtl::Persist, Some(-1)) => "The field had no TTL.",
                (WriteTtl::Persist, _) => "Field TTL removed.",
                (WriteTtl::Seconds(_), _) => "Field TTL set.",
            }),
            WriteOp::HSetNx { .. } if n == Some(0) => {
                act.error("Not added: the field exists already. e edits it.");
                return;
            }
            WriteOp::SAdd { .. } | WriteOp::ZAdd { .. } if n == Some(0) => {
                act.error("Not added: the member exists already.");
                return;
            }
            WriteOp::HDel { .. }
            | WriteOp::SRem { .. }
            | WriteOp::ZRem { .. }
            | WriteOp::XDel { .. }
                if n == Some(0) =>
            {
                act.info("Nothing deleted: it was gone already.");
            }
            WriteOp::HDel { .. }
            | WriteOp::SRem { .. }
            | WriteOp::ZRem { .. }
            | WriteOp::XDel { .. }
            | WriteOp::LDel { .. } => act.info("Deleted."),
            WriteOp::HSetNx { .. }
            | WriteOp::RPush { .. }
            | WriteOp::SAdd { .. }
            | WriteOp::ZAdd { .. }
            | WriteOp::XAdd { .. } => act.info("Added."),
            _ => act.info("Saved."),
        }
        if reload {
            self.reload(act);
        }
        self.read_meta(act);
    }
}

impl ViewState for ValueState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        self.keep_going(act);
    }

    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        self.keep_going(act);
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        let tick = act.tick;
        if self.form.is_some() {
            self.form_key(&key, act);
            return true;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            if self.has_list() {
                self.focus = match self.focus {
                    Focus::Items => Focus::Value,
                    Focus::Value => Focus::Items,
                };
            } else {
                act.info("This value has no list: ↑↓ scroll it.");
            }
            return true;
        }
        if self.focus == Focus::Value || !self.has_list() {
            if self.scroll_key(&key) {
                return true;
            }
        } else if let Some(sel) = list_move(&key, self.selected(), self.loaded(), act) {
            if sel != self.selected() {
                self.sel = sel;
                self.sel_since = tick;
                self.scroll = 0;
            }
            return true;
        }
        match plain(&key) {
            Some('f') => {
                match self.format_source() {
                    Some(raw) => {
                        let from = self
                            .format
                            .unwrap_or_else(|| self.memo.decode(raw).detected);
                        let to = from.next();
                        self.format = Some(to);
                        self.scroll = 0;
                        act.info(format!("Shown as {}.", to.label()));
                    }
                    None => act.info("No value to format yet."),
                }
                true
            }
            Some('n') => {
                self.more(act);
                true
            }
            Some('y') => {
                self.copy(act);
                true
            }
            Some(c) => self.edit_key(c, act),
            _ => false,
        }
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        if let Some((_, op)) = self.writing.take_if(|(w, _)| *w == id) {
            self.write_done(op, result, act);
            return;
        }
        if self.meta_loading == Some(id) {
            self.meta_loading = None;
            self.meta_done(result);
            return;
        }
        if self.loading != Some(id) {
            return;
        }
        self.loading = None;
        match result {
            Ok(LiveReply::Value(Reply::Gone)) => {
                self.gone = true;
                self.items = None;
                self.next = None;
                self.error = None;
            }
            Ok(LiveReply::Value(Reply::Page(page))) => self.take(page),
            Ok(_) => {}
            Err(e) => {
                self.denied = matches!(e, LiveError::Denied(_));
                self.error = Some(read_error(&e));
            }
        }
    }

    fn input(&self) -> Option<Input> {
        self.form.as_ref().map(Form::input)
    }

    fn on_paste(&mut self, text: &str, _act: &mut Act<'_>) {
        if let Some(f) = self.form.as_mut() {
            f.paste(text);
        }
    }
}

/// `v` cut to `room` items; sets `cut` when it was longer.
fn cap_vec<T>(mut v: Vec<T>, room: usize, cut: &mut bool) -> Vec<T> {
    if v.len() > room {
        *cut = true;
        v.truncate(room);
    }
    v
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::testing::{self, key};
    use super::*;
    use crate::live::values::{Entry, Field, Member, Page};
    use crate::model::{KeyType, ServerFlavor, Ttl};
    use crate::tui::app::{App, Effect, Msg, Screen};
    use crate::tui::tree::{Root, Row};

    const MARK: &str = "s3cr3t-v4lue";

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn row(key: &str, key_type: KeyType, length: Option<u64>, bytes: Option<u64>) -> KeyRow {
        KeyRow {
            key: key.into(),
            key_type,
            ttl: Ttl::Expires(Duration::from_secs(533_520)),
            bytes,
            encoding: Some("listpack".into()),
            length,
            freq: Some(14),
        }
    }

    fn cart() -> KeyRow {
        row("user:10293:cart", KeyType::Hash, Some(3), Some(3_174))
    }

    /// The scanned app with the live connection ready, on the key list of
    /// `user:{id}:cart` holding only `rows`.
    fn key_list(rows: Vec<KeyRow>, server_version: Option<&str>) -> App {
        let mut app = testing::scanned();
        if let Some(v) = server_version {
            let s = app.session.as_mut().unwrap().server.as_mut().unwrap();
            s.version = v.to_string();
        }
        testing::live_ready(&mut app);
        app.handle_key(ch('1'));
        let at = {
            let data = app.pattern_data().expect("patterns");
            let name = |i: usize| data.patterns[i].pattern.as_str();
            app.pattern_rows()
                .iter()
                .position(|r| match r {
                    Row::Pattern { idx, .. } => name(*idx) == "user:{id}:cart",
                    Row::Group { root } => matches!(
                        app.patterns.tree.roots[*root],
                        Root::Single(i) if name(i) == "user:{id}:cart"
                    ),
                })
                .expect("the carts")
        };
        app.patterns.sel = at;
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyList);
        app.views.keys.rows = rows;
        app
    }

    /// The value requests among `effects`.
    fn value_requests(effects: &[Effect]) -> Vec<(ReqId, Request)> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(id, r)| match r {
                LiveRequest::Value(v) => Some((id, v)),
                _ => None,
            })
            .collect()
    }

    fn one_request(effects: &[Effect]) -> (ReqId, Request) {
        let reqs = value_requests(effects);
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        reqs.into_iter().next().unwrap()
    }

    /// The key detail of `r`, with its first page request.
    fn open_with(r: KeyRow, server_version: Option<&str>) -> (App, ReqId, Request) {
        let mut app = key_list(vec![r], server_version);
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyDetail);
        let (id, req) = one_request(&effects);
        (app, id, req)
    }

    fn open(r: KeyRow) -> (App, ReqId, Request) {
        open_with(r, None)
    }

    fn page(
        type_name: Option<&str>,
        items: Items,
        next: Option<At>,
    ) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Value(Reply::Page(Page {
            type_name: type_name.map(str::to_string),
            items,
            next,
            refused: Vec::new(),
        })))
    }

    fn field(name: &str, value: &str, ttl: Option<Ttl>) -> Field {
        Field {
            name: Raw::from(name),
            value: Raw::from(value),
            ttl,
        }
    }

    fn fields() -> Items {
        Items::Hash(vec![
            field(
                "sku:44812",
                r#"{"qty":2,"price":19.99}"#,
                Some(Ttl::Expires(Duration::from_secs(7_800))),
            ),
            field(
                "sku:44813",
                r#"{"qty":1,"price":4.50}"#,
                Some(Ttl::Persistent),
            ),
            field("coupon", "\"WINTER10\"", None),
        ])
    }

    fn notice(app: &App) -> String {
        app.notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    fn text(t: &Terminal<TestBackend>) -> String {
        let b = t.backend().buffer();
        let mut out = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                out.push_str(b[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn snap(app: &App, name: &str) {
        for (w, h) in [(80, 24), (120, 36)] {
            let t = testing::render(app, w, h);
            let all = text(&t);
            // The key bar never wraps, and no raw control character is drawn.
            assert!(
                !all.chars().any(|c| c.is_control() && c != '\n'),
                "{name}: a raw control character is on screen"
            );
            insta::assert_snapshot!(format!("{name}_{w}x{h}"), t.backend());
        }
    }

    #[test]
    fn opening_a_key_asks_for_its_first_page() {
        let (_, _, req) = open(cart());
        assert_eq!(
            req,
            Request {
                key: "user:10293:cart".into(),
                kind: Kind::Hash,
                at: None,
                // The fixture server is Redis 7.4.2.
                field_ttls: true,
                size: Some(3_174),
            }
        );
    }

    #[test]
    fn a_hash_with_field_ttls() {
        let (mut app, id, _) = open(cart());
        testing::reply(&mut app, id, page(Some("hash"), fields(), None));
        snap(&app, "hash");
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("Field TTL   1 of 3 loaded fields"), "{all}");
        assert!(all.contains("TTL"), "{all}");
        assert!(all.contains("2h 10m"), "{all}");
        assert!(all.contains("Verdict"), "{all}");
    }

    #[test]
    fn field_ttls_are_off_on_an_old_server_and_httl_is_never_asked() {
        for v in ["7.2.4", "6.2.14"] {
            let (mut app, id, req) = open_with(cart(), Some(v));
            assert!(!req.field_ttls, "{v}");
            testing::reply(
                &mut app,
                id,
                page(Some("hash"), fields(), Some(At::Cursor(9))),
            );
            let (_, next) = one_request(&app.handle_key(ch('n')));
            assert!(!next.field_ttls, "{v}");
            let all = text(&testing::render(&app, 120, 36));
            assert!(!all.contains("Field TTL"), "{all}");
            assert!(!all.contains(" TTL\n") && !all.contains("TTL │"), "{all}");
        }
        let (mut app, id, _) = open_with(cart(), Some("7.2.4"));
        testing::reply(&mut app, id, page(Some("hash"), fields(), None));
        snap(&app, "hash_old_server");
        let mut valkey = testing::scanned();
        let s = valkey.session.as_mut().unwrap().server.as_mut().unwrap();
        s.flavor = ServerFlavor::Valkey;
        s.version = "8.1.3".into();
        assert!(!field_ttls_supported(s.flavor, &s.version));
    }

    #[test]
    fn a_refused_httl_hides_the_column_from_then_on() {
        let (mut app, id, _) = open(cart());
        let reply = Ok(LiveReply::Value(Reply::Page(Page {
            type_name: Some("hash".into()),
            items: fields(),
            next: Some(At::Cursor(5)),
            refused: vec![LiveError::Denied("HTTL".into())],
        })));
        testing::reply(&mut app, id, reply);
        let all = text(&testing::render(&app, 120, 36));
        assert_eq!(
            app.views.value.httl_off.as_deref(),
            Some("Field TTLs off: the server refused HTTL for this user.")
        );
        assert!(
            all.contains("▲ Field TTLs off: the server refused"),
            "{all}"
        );
        assert!(all.contains("for this user."), "{all}");
        assert!(!all.contains("Field TTL  "), "{all}");
        let (_, req) = one_request(&app.handle_key(ch('n')));
        assert!(!req.field_ttls);
        // Another key does not ask either.
        app.handle_key(key(KeyCode::Esc));
        app.views.keys.rows = vec![row("user:7:cart", KeyType::Hash, Some(1), Some(80))];
        let (_, req) = one_request(&app.handle_key(key(KeyCode::Enter)));
        assert!(!req.field_ttls);
    }

    #[test]
    fn a_string() {
        let (mut app, id, req) = open(row("user:10293:cart", KeyType::String, Some(26), Some(80)));
        assert_eq!(req.kind, Kind::String);
        let items = Items::String {
            value: Raw::from("hello from the cart\nline 2"),
            len: Some(26),
        };
        testing::reply(&mut app, id, page(Some("string"), items, None));
        snap(&app, "string");
    }

    #[test]
    fn a_long_string_says_it_shows_the_first_64_kib() {
        let (mut app, id, _) = open(row("k", KeyType::String, Some(100_000), None));
        let items = Items::String {
            value: Raw::new(vec![b'a'; crate::live::values::STRING_BYTES]),
            len: Some(100_000),
        };
        testing::reply(&mut app, id, page(Some("string"), items, None));
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("first 64 KiB of"), "{all}");
    }

    #[test]
    fn escapes_are_drawn_never_the_raw_bytes() {
        let (mut app, id, _) = open(row("user:10293:cart", KeyType::String, Some(20), Some(80)));
        let items = Items::String {
            value: Raw::from("ok\x1b[2J\u{9b}31m done"),
            len: Some(20),
        };
        testing::reply(&mut app, id, page(Some("string"), items, None));
        snap(&app, "escapes");
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("\\x1b[2J"), "{all}");
        assert!(!all.contains('\x1b') && !all.contains('\u{9b}'), "{all}");
        // Binary: a raw 0x9b byte, and escapes in a field name too.
        let (mut app, id, _) = open(cart());
        let items = Items::Hash(vec![Field {
            name: Raw::from("na\x1b[2Jme"),
            value: Raw::new(vec![0x9b, 0x1b, b'[', b'2', b'J', 0xff]),
            ttl: None,
        }]);
        testing::reply(&mut app, id, page(Some("hash"), items, None));
        snap(&app, "binary");
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("na\\x1b[2Jme"), "{all}");
        assert!(!all.contains('\x1b') && !all.contains('\u{9b}'), "{all}");
    }

    #[test]
    fn a_list() {
        let (mut app, id, _) = open(row("queue:jobs", KeyType::List, Some(3), Some(200)));
        let items = Items::List(vec![
            Raw::from(r#"{"job":"resize","id":1}"#),
            Raw::from("plain text job"),
            Raw::from("third"),
        ]);
        testing::reply(&mut app, id, page(Some("list"), items, None));
        snap(&app, "list");
    }

    #[test]
    fn a_sorted_set() {
        let (mut app, id, _) = open(row("board:week", KeyType::ZSet, Some(2), Some(200)));
        let items = Items::ZSet(vec![
            Member {
                member: Raw::from("alice"),
                score: Raw::from("1500"),
            },
            Member {
                member: Raw::from("bob"),
                score: Raw::from("1200.5"),
            },
        ]);
        testing::reply(&mut app, id, page(Some("zset"), items, None));
        snap(&app, "zset");
    }

    #[test]
    fn a_stream() {
        let (mut app, id, _) = open(row("events:orders", KeyType::Stream, Some(2), Some(900)));
        let items = Items::Stream(vec![
            Entry {
                id: "1700000000002-0".into(),
                fields: vec![
                    (Raw::from("order"), Raw::from("44812")),
                    (Raw::from("state"), Raw::from("paid")),
                ],
            },
            Entry {
                id: "1700000000001-0".into(),
                fields: vec![(Raw::from("order"), Raw::from("44811"))],
            },
        ]);
        testing::reply(&mut app, id, page(Some("stream"), items, None));
        snap(&app, "stream");
        // A stream entry has several values: y does not copy it.
        let effects = app.handle_key(ch('y'));
        assert!(!effects.iter().any(|e| matches!(e, Effect::Copy(_))));
    }

    #[test]
    fn a_refused_read_says_why_and_is_not_sent_again() {
        let (mut app, id, _) = open(cart());
        testing::reply(&mut app, id, Err(LiveError::Denied("HSCAN".into())));
        snap(&app, "refused");
        let all = text(&testing::render(&app, 120, 36));
        assert!(
            all.contains("Can't read the value: the server refused HSCAN for this user."),
            "{all}"
        );
        assert!(!all.contains("NOPERM"));
        let effects = app.handle_key(ch('n'));
        assert!(value_requests(&effects).is_empty());
        assert_eq!(
            notice(&app),
            "Can't read the value: the server refused HSCAN for this user."
        );
        // Ticks do not ask again either.
        let t0 = std::time::Instant::now();
        let effects = app.apply(Msg::TickAt(t0));
        assert!(value_requests(&effects).is_empty());
    }

    #[test]
    fn a_gone_key() {
        let (mut app, id, _) = open(cart());
        testing::reply(&mut app, id, Ok(LiveReply::Value(Reply::Gone)));
        snap(&app, "gone");
        assert!(text(&testing::render(&app, 80, 24)).contains("The key no longer exists."));
        app.handle_key(ch('n'));
        assert_eq!(notice(&app), "The key no longer exists.");
    }

    #[test]
    fn other_types_json_and_vector_sets() {
        let (mut app, id, _) = open(row("idx:x", KeyType::Other, None, Some(80)));
        testing::reply(&mut app, id, page(Some("TSDB-TYPE"), Items::NotRead, None));
        let all = text(&testing::render(&app, 120, 36));
        assert!(
            all.contains("redis-lens does not read values of type TSDB-TYPE."),
            "{all}"
        );
        let (mut app, id, req) = open(row("doc:1", KeyType::Json, None, None));
        assert_eq!((req.kind, req.size), (Kind::Json, None));
        testing::reply(
            &mut app,
            id,
            page(Some("ReJSON-RL"), Items::JsonNotRead { size: None }, None),
        );
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("size is unknown"), "{all}");
        let (mut app, id, _) = open(row("vec:1", KeyType::Other, None, Some(80)));
        let items = Items::Vector(vec![Raw::from("e1"), Raw::from("e2")]);
        testing::reply(&mut app, id, page(Some("vectorset"), items, None));
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("vector sets can't be read in pages"), "{all}");
        app.handle_key(ch('n'));
        assert!(notice(&app).starts_with("Vector sets can't be read in pages"));
    }

    #[test]
    fn n_loads_the_next_page_until_the_end() {
        let (mut app, id, _) = open(row("queue:jobs", KeyType::String, Some(250), Some(200)));
        // TYPE said list, not the key list's string: the next pages read a list.
        let items = Items::List(
            (0..100)
                .map(|i| Raw::from(format!("j{i}").as_str()))
                .collect(),
        );
        testing::reply(
            &mut app,
            id,
            page(Some("list"), items, Some(At::Index(100))),
        );
        let (id, req) = one_request(&app.handle_key(ch('n')));
        assert_eq!(
            (req.kind.clone(), req.at.clone()),
            (Kind::List, Some(At::Index(100)))
        );
        assert!(value_requests(&app.handle_key(ch('n'))).is_empty());
        assert_eq!(notice(&app), "Still loading the value…");
        let items = Items::List((0..50).map(|_| Raw::from("x")).collect());
        testing::reply(&mut app, id, page(None, items, None));
        assert_eq!(app.views.value.loaded(), 150);
        assert!(value_requests(&app.handle_key(ch('n'))).is_empty());
        assert_eq!(notice(&app), "All 150 items are loaded.");
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("150 of 250"), "{all}");
    }

    #[test]
    fn at_most_10000_items_are_held() {
        let (mut app, mut id, _) = open(row("s", KeyType::Set, Some(50_000), Some(1)));
        for p in 0..101u64 {
            let items = Items::Set(
                (0..100)
                    .map(|i| Raw::from(format!("m{p}:{i}").as_str()))
                    .collect(),
            );
            testing::reply(
                &mut app,
                id,
                page(Some("set"), items, Some(At::Cursor(p + 1))),
            );
            let effects = app.handle_key(ch('n'));
            match value_requests(&effects).into_iter().next() {
                Some((next, _)) => id = next,
                None => break,
            }
        }
        assert_eq!(app.views.value.loaded(), MAX_ITEMS);
        assert!(app.views.value.capped);
        assert!(value_requests(&app.handle_key(ch('n'))).is_empty());
        assert_eq!(
            notice(&app),
            "At most 10,000 items are held, so no more are loaded."
        );
        // Members HSCAN or SSCAN return twice are listed once.
        let (mut app, id, _) = open(row("s", KeyType::Set, Some(2), Some(1)));
        let items = Items::Set(vec![Raw::from("a"), Raw::from("b")]);
        testing::reply(&mut app, id, page(Some("set"), items, Some(At::Cursor(3))));
        let (id, _) = one_request(&app.handle_key(ch('n')));
        let items = Items::Set(vec![Raw::from("b"), Raw::from("c")]);
        testing::reply(&mut app, id, page(None, items, None));
        assert_eq!(app.views.value.loaded(), 3);
    }

    #[test]
    fn f_steps_through_the_formats() {
        let (mut app, id, _) = open(cart());
        testing::reply(&mut app, id, page(Some("hash"), fields(), None));
        let detected = value::decode(app.views.value.selected_value().unwrap().as_bytes()).detected;
        let mut want = detected;
        for _ in 0..4 {
            want = want.next();
            app.handle_key(ch('f'));
            assert_eq!(app.views.value.format, Some(want));
            assert_eq!(notice(&app), format!("Shown as {}.", want.label()));
        }
        // It holds for every item of this key.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.views.value.format, Some(want));
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains(&format!("shown as {}", want.label())), "{all}");
    }

    #[test]
    fn y_copies_the_value_as_shown_and_refuses_control_characters() {
        let (mut app, id, _) = open(cart());
        let items = Items::Hash(vec![
            field("a", "plain", None),
            field("b", "two\nlines", None),
        ]);
        testing::reply(&mut app, id, page(Some("hash"), items, None));
        let effects = app.handle_key(ch('y'));
        let copied: Vec<&CopyText> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Copy(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(copied.len(), 1);
        assert_eq!((copied[0].as_str(), copied[0].what()), ("plain", "value"));
        app.handle_key(key(KeyCode::Down));
        let effects = app.handle_key(ch('y'));
        assert!(!effects.iter().any(|e| matches!(e, Effect::Copy(_))));
        assert_eq!(notice(&app), COPY_REFUSED);
    }

    #[test]
    fn tab_moves_the_focus_and_the_value_scrolls() {
        let (mut app, id, _) = open(cart());
        let long: String = (0..60).map(|i| format!("line {i}\n")).collect();
        let items = Items::Hash(vec![field("a", &long, None), field("b", "x", None)]);
        testing::reply(&mut app, id, page(Some("hash"), items, None));
        testing::render(&app, 80, 24);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.value.focus, Focus::Value);
        app.handle_key(key(KeyCode::End));
        let bottom = app.views.value.scroll;
        assert!(bottom > 0);
        app.handle_key(ch('k'));
        assert_eq!(app.views.value.scroll, bottom - 1);
        // The selection stays while the value scrolls.
        assert_eq!(app.views.value.selected(), 0);
        app.handle_key(key(KeyCode::BackTab));
        app.handle_key(ch('j'));
        assert_eq!((app.views.value.selected(), app.views.value.scroll), (1, 0));
    }

    #[test]
    fn esc_goes_back_and_the_detail_stays_open_across_views() {
        let (mut app, id, _) = open(cart());
        testing::reply(&mut app, id, page(Some("hash"), fields(), None));
        app.handle_key(ch('2'));
        let effects = app.handle_key(ch('1'));
        assert_eq!(app.screen(), Screen::KeyDetail);
        assert!(value_requests(&effects).is_empty());
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::KeyList);
        // The same key keeps what it loaded.
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(value_requests(&effects).is_empty());
        assert_eq!(app.views.value.loaded(), 3);
        // Another starts afresh.
        app.handle_key(key(KeyCode::Esc));
        app.views.keys.rows = vec![row("user:7:cart", KeyType::Hash, Some(1), Some(80))];
        let (_, req) = one_request(&app.handle_key(key(KeyCode::Enter)));
        assert_eq!(req.key, "user:7:cart");
        assert_eq!(app.views.value.loaded(), 0);
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Patterns);
    }

    #[test]
    fn the_footer_lists_the_detail_keys() {
        let (mut app, id, _) = open(cart());
        testing::reply(&mut app, id, page(Some("hash"), fields(), None));
        let all = text(&testing::render(&app, 80, 24));
        let last = all.lines().last().unwrap();
        assert!(
            last.contains("tab Value") && last.contains("f Format") && last.contains("esc Back"),
            "{last}"
        );
    }

    /// Rule 1: a value shows on screen and nowhere else.
    #[test]
    fn values_never_reach_debug_snapshots_or_copy_notices() {
        let (mut app, id, _) = open(cart());
        let items = Items::Hash(vec![
            field(MARK, "plain", None),
            field("b", MARK, Some(Ttl::Persistent)),
        ]);
        let reply = page(Some("hash"), items, None);
        let msg = format!(
            "{:?}",
            Msg::Live {
                id: app.session.as_ref().unwrap().id,
                scan: app.session.as_ref().unwrap().scan,
                req_id: id,
                result: reply.clone(),
            }
        );
        assert!(!value::leaks(&msg, MARK), "{msg}");
        testing::reply(&mut app, id, reply);
        assert!(text(&testing::render(&app, 120, 36)).contains(MARK));
        app.handle_key(key(KeyCode::Down));
        let effects = app.handle_key(ch('y'));
        let debug = format!("{effects:?} {:?} {:?}", app.views, app.session);
        assert!(!value::leaks(&debug, MARK), "{debug}");
        assert!(!value::leaks(&notice(&app), MARK), "{}", notice(&app));
        assert!(effects.iter().any(|e| matches!(e, Effect::Copy(_))));

        // A string too, then a snapshot file from Patterns.
        app.handle_key(key(KeyCode::Esc));
        app.views.keys.rows = vec![row("user:9:cart", KeyType::String, Some(12), Some(80))];
        let (id, _) = one_request(&app.handle_key(key(KeyCode::Enter)));
        let items = Items::String {
            value: Raw::from(MARK),
            len: Some(12),
        };
        testing::reply(&mut app, id, page(Some("string"), items, None));
        assert!(!value::leaks(&format!("{:?}", app.views), MARK));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Patterns);
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(report)] = &effects[..] else {
            panic!("{effects:?}");
        };
        let json = serde_json::to_string(report).unwrap();
        assert!(!value::leaks(&json, MARK));
        assert!(!value::leaks(&format!("{effects:?}"), MARK));
    }
    /// Terminal safety: what the server names (the key, a stream entry id, the `TYPE`
    /// reply) is escaped like a value, bidi controls included, which ratatui would
    /// otherwise draw as they are.
    #[test]
    fn names_from_the_server_are_escaped() {
        let evil = "user:\u{1b}[31mx\u{202e}gpj.exe:cart";
        let r = row(evil, KeyType::Stream, Some(1), Some(80));
        let (mut app, id, _) = open(r);
        let items = Items::Stream(vec![Entry {
            id: "1-0\u{2066}\u{7}".into(),
            fields: vec![(Raw::from("f\u{202e}"), Raw::from("v"))],
        }]);
        testing::reply(&mut app, id, page(Some("str\u{202e}eam"), items, None));
        for (w, h) in [(80, 24), (120, 36)] {
            let all = text(&testing::render(&app, w, h));
            for bad in ['\u{1b}', '\u{7}', '\u{202e}', '\u{2066}'] {
                assert!(!all.contains(bad), "{w}x{h}: {bad:?} drawn:\n{all}");
            }
            assert!(all.contains("\\x1b[31m") || all.contains("\\x1b"), "{all}");
            assert!(all.contains("\\u{202e}"), "{all}");
        }
    }

    /// A rendered line that fills the pane is drawn whole: the value is wrapped one
    /// column narrower than the pane, for the space each line starts with.
    #[test]
    fn a_full_width_line_is_not_cut() {
        let r = row("user:9:cart", KeyType::String, Some(400), Some(480));
        let (mut app, id, _) = open(r);
        let long: String = (0..400)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let items = Items::String {
            value: Raw::from(long.as_str()),
            len: Some(400),
        };
        testing::reply(&mut app, id, page(Some("string"), items, None));
        for (w, h) in [(80, 24), (120, 36)] {
            let all = text(&testing::render(&app, w, h));
            let shown: String = all
                .lines()
                .map(|l| l.replace('│', " "))
                .flat_map(|l| l.split_whitespace().map(str::to_string).collect::<Vec<_>>())
                .filter(|t| t.chars().all(|c| c.is_ascii_lowercase()) && t.len() > 10)
                .collect::<String>();
            assert!(
                long.starts_with(&shown),
                "{w}x{h}: a character was cut:\n{all}"
            );
            assert!(shown.len() > 200, "{w}x{h}:\n{all}");
        }
    }

    /// The leak checks catch a value printed as bytes, not only as text.
    #[test]
    fn the_leak_check_sees_bytes() {
        assert!(value::leaks(
            &format!("{:?}", MARK.as_bytes().to_vec()),
            MARK
        ));
        assert!(value::leaks(&format!("{:?}", MARK.as_bytes()), MARK));
        let hex: String = MARK.bytes().map(|b| format!("{b:02x}")).collect();
        assert!(value::leaks(&hex, MARK));
        assert!(!value::leaks("Raw(<12 bytes>)", MARK));
    }

    // -----------------------------------------------------------------------
    // Edits (P3)
    // -----------------------------------------------------------------------

    use crate::model::Env;
    use crate::tui::app::confirm::Expect;
    use crate::write::{Ttl as WTtl, WriteOp};

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(ch(c));
        }
    }

    /// The writes among `effects`.
    fn writes(effects: &[Effect]) -> Vec<WriteOp> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(_, r)| match r {
                LiveRequest::Write(p) => Some(p.op().clone()),
                _ => None,
            })
            .collect()
    }

    fn write_ids(effects: &[Effect]) -> Vec<ReqId> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(id, r)| matches!(r, LiveRequest::Write(_)).then_some(id))
            .collect()
    }

    fn set_profile(app: &mut App, env: Env, read_only: Option<bool>) {
        let s = app.session.as_mut().unwrap();
        s.profile.env = env;
        s.profile.read_only = read_only;
    }

    /// The key detail of `r` with `items` read, on a `local` profile with writes on.
    fn loaded(r: KeyRow, type_name: &str, items: Items) -> App {
        loaded_on(r, type_name, items, None)
    }

    fn loaded_on(r: KeyRow, type_name: &str, items: Items, version: Option<&str>) -> App {
        let (mut app, id, _) = open_with(r, version);
        set_profile(&mut app, Env::Local, None);
        testing::reply(&mut app, id, page(Some(type_name), items, None));
        app
    }

    fn hash_app() -> App {
        loaded(cart(), "hash", fields())
    }

    fn list_app() -> App {
        let items = Items::List(vec![Raw::from("a"), Raw::from("b"), Raw::from("a")]);
        loaded(
            row("queue:jobs", KeyType::List, Some(3), Some(90)),
            "list",
            items,
        )
    }

    fn set_app() -> App {
        let items = Items::Set(vec![Raw::from("red"), Raw::from("blue")]);
        loaded(row("tags", KeyType::Set, Some(2), Some(90)), "set", items)
    }

    fn zset_app() -> App {
        let items = Items::ZSet(vec![Member {
            member: Raw::from("bob"),
            score: Raw::from("1200.5"),
        }]);
        loaded(
            row("board", KeyType::ZSet, Some(1), Some(90)),
            "zset",
            items,
        )
    }

    fn stream_app() -> App {
        let items = Items::Stream(vec![Entry {
            id: "1-0".into(),
            fields: vec![(Raw::from("f"), Raw::from("v"))],
        }]);
        loaded(
            row("events", KeyType::Stream, Some(1), Some(90)),
            "stream",
            items,
        )
    }

    fn string_app(value: &str) -> App {
        let items = Items::String {
            value: Raw::from(value),
            len: Some(value.len() as u64),
        };
        let r = row(
            "greeting",
            KeyType::String,
            Some(value.len() as u64),
            Some(90),
        );
        loaded(r, "string", items)
    }

    /// `e`, the text cleared, `text` typed and sent.
    fn edit(app: &mut App, text: &str) -> Vec<Effect> {
        app.handle_key(ch('e'));
        assert!(app.views.value.form.is_some(), "{}", notice(app));
        app.handle_key(ctrl('u'));
        type_text(app, text);
        app.handle_key(key(KeyCode::Enter))
    }

    #[test]
    fn e_edits_the_selected_item_at_once_outside_prod() {
        let k = |s: &str| s.to_string();
        let mut app = hash_app();
        assert_eq!(
            writes(&edit(&mut app, "x")),
            [WriteOp::HSet {
                key: k("user:10293:cart"),
                field: Raw::from("sku:44812"),
                value: Raw::from("x")
            }]
        );
        assert!(app.confirm.is_none());

        let mut app = list_app();
        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            writes(&edit(&mut app, "new")),
            [WriteOp::LSet {
                key: k("queue:jobs"),
                index: 1,
                old: Raw::from("b"),
                value: Raw::from("new")
            }]
        );

        let mut app = set_app();
        assert_eq!(
            writes(&edit(&mut app, "green")),
            [WriteOp::SReplace {
                key: k("tags"),
                old: Raw::from("red"),
                value: Raw::from("green")
            }]
        );

        let mut app = zset_app();
        app.handle_key(ch('e'));
        assert_eq!(
            app.views.value.form.as_ref().unwrap().fields[0].text,
            "1200.5"
        );
        app.handle_key(ctrl('u'));
        type_text(&mut app, "nope");
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        let form = app.views.value.form.as_ref().expect("still open");
        assert!(form.error.as_deref().unwrap().contains("score"));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "7");
        assert_eq!(
            writes(&app.handle_key(key(KeyCode::Enter))),
            [WriteOp::ZScore {
                key: k("board"),
                member: Raw::from("bob"),
                score: 7.0
            }]
        );

        let mut app = string_app("hello");
        assert_eq!(
            writes(&edit(&mut app, "hi")),
            [WriteOp::SetString {
                key: k("greeting"),
                old: Raw::from("hello"),
                value: Raw::from("hi")
            }]
        );

        let (mut app, id, _) = open(row("doc:1", KeyType::Json, None, Some(80)));
        set_profile(&mut app, Env::Local, None);
        let doc = Raw::from(r#"{"a":1}"#);
        let items = Items::Json {
            len: doc.len(),
            doc,
        };
        testing::reply(&mut app, id, page(Some("ReJSON-RL"), items, None));
        assert_eq!(
            writes(&edit(&mut app, r#"{"a":2}"#)),
            [WriteOp::JsonSet {
                key: k("doc:1"),
                value: Raw::from(r#"{"a":2}"#)
            }]
        );
    }

    #[test]
    fn e_is_off_with_a_reason_where_it_cant_edit() {
        let mut app = stream_app();
        assert!(writes(&app.handle_key(ch('e'))).is_empty());
        assert!(app.views.value.form.is_none());
        assert!(notice(&app).contains("can't be edited"), "{}", notice(&app));

        // Only the first 64 KiB of a string is loaded.
        let (mut app, id, _) = open(row("big", KeyType::String, Some(100_000), None));
        set_profile(&mut app, Env::Local, None);
        let items = Items::String {
            value: Raw::new(vec![b'a'; crate::live::values::STRING_BYTES]),
            len: Some(100_000),
        };
        testing::reply(&mut app, id, page(Some("string"), items, None));
        let e = app.handle_key(ch('e'));
        assert!(writes(&e).is_empty());
        assert!(!e.iter().any(|e| matches!(e, Effect::Editor(_))));
        assert!(notice(&app).starts_with("e is off: only the first 64 KiB"));

        // Unchanged: nothing is sent.
        let mut app = string_app("same");
        app.handle_key(ch('e'));
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert_eq!(notice(&app), "Not changed, so nothing was sent.");
        // esc cancels.
        app.handle_key(ch('e'));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.views.value.form.is_none());
        assert_eq!(app.screen(), Screen::KeyDetail);
    }

    #[test]
    fn plus_adds_without_overwriting() {
        let k = |s: &str| s.to_string();
        let add = |app: &mut App, parts: &[&str]| {
            app.handle_key(ch('+'));
            assert!(app.views.value.form.is_some(), "{}", notice(app));
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    app.handle_key(key(KeyCode::Tab));
                }
                type_text(app, p);
            }
            writes(&app.handle_key(key(KeyCode::Enter)))
        };
        assert_eq!(
            add(&mut hash_app(), &["f", "v"]),
            [WriteOp::HSetNx {
                key: k("user:10293:cart"),
                field: Raw::from("f"),
                value: Raw::from("v")
            }]
        );
        assert_eq!(
            add(&mut list_app(), &["z"]),
            [WriteOp::RPush {
                key: k("queue:jobs"),
                value: Raw::from("z")
            }]
        );
        assert_eq!(
            add(&mut set_app(), &["m"]),
            [WriteOp::SAdd {
                key: k("tags"),
                member: Raw::from("m")
            }]
        );
        assert_eq!(
            add(&mut zset_app(), &["m", "2.5"]),
            [WriteOp::ZAdd {
                key: k("board"),
                member: Raw::from("m"),
                score: 2.5
            }]
        );
        assert_eq!(
            add(&mut stream_app(), &["f", "v"]),
            [WriteOp::XAdd {
                key: k("events"),
                fields: vec![(Raw::from("f"), Raw::from("v"))]
            }]
        );
        let mut app = string_app("x");
        app.handle_key(ch('+'));
        assert!(app.views.value.form.is_none());
        assert!(notice(&app).starts_with("+ adds to a hash"));
        // HSETNX on a field that is there: says so, and does not read again.
        let mut app = hash_app();
        add(&mut app, &["sku:44812", "v"]);
        let id = app.views.value.writing.as_ref().unwrap().0;
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(0))));
        assert!(testing::requests(&e).is_empty());
        assert!(notice(&app).starts_with("Not added: the field exists already"));
    }

    #[test]
    fn ttls_parse_and_t_sets_the_key_or_field_ttl() {
        assert_eq!(parse_ttl("90s"), Ok(WTtl::Seconds(90)));
        assert_eq!(parse_ttl("10m"), Ok(WTtl::Seconds(600)));
        assert_eq!(parse_ttl("24h"), Ok(WTtl::Seconds(86_400)));
        assert_eq!(parse_ttl("7d"), Ok(WTtl::Seconds(604_800)));
        assert_eq!(parse_ttl("1h30m"), Ok(WTtl::Seconds(5_400)));
        assert_eq!(parse_ttl(" 120 "), Ok(WTtl::Seconds(120)));
        assert_eq!(parse_ttl("-"), Ok(WTtl::Persist));
        assert_eq!(parse_ttl("persist"), Ok(WTtl::Persist));
        for bad in ["", "0", "0s", "1x", "h", "99999999999999999999", "200y"] {
            assert!(parse_ttl(bad).is_err(), "{bad}");
        }
        assert!(parse_ttl("5300w").is_err());

        let mut app = string_app("x");
        app.handle_key(ch('t'));
        type_text(&mut app, "10m");
        let e = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            writes(&e),
            [WriteOp::KeyTtl {
                key: "greeting".into(),
                ttl: WTtl::Seconds(600)
            }]
        );
        // A TTL change reads the TTL again, not the value.
        let e = testing::reply(
            &mut app,
            write_ids(&e)[0],
            Ok(LiveReply::Write(WriteReply::Int(1))),
        );
        assert_eq!(notice(&app), "TTL set to 10m 00s.");
        assert!(value_requests(&e).is_empty());
        assert_eq!(meta_requests(&e).len(), 1);
        app.handle_key(ch('t'));
        type_text(&mut app, "0");
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        app.handle_key(ctrl('u'));
        type_text(&mut app, "-");
        let e = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            &writes(&e)[..],
            [WriteOp::KeyTtl {
                ttl: WTtl::Persist,
                ..
            }]
        ));

        // A hash on Redis 7.4 (the fixture): the key or the selected field.
        let mut app = hash_app();
        app.handle_key(ch('t'));
        assert!(app.views.value.form.as_ref().unwrap().fields[0].is_choice());
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "1h");
        assert_eq!(
            writes(&app.handle_key(key(KeyCode::Enter))),
            [WriteOp::FieldTtl {
                key: "user:10293:cart".into(),
                field: Raw::from("sku:44812"),
                ttl: WTtl::Seconds(3_600)
            }]
        );
        // Redis 7.2: the key only, and the form says why.
        let mut app = loaded_on(cart(), "hash", fields(), Some("7.2.4"));
        app.handle_key(ch('t'));
        let form = app.views.value.form.as_ref().unwrap();
        assert!(!form.fields[0].is_choice());
        assert!(
            form.notes.iter().any(|n| n.contains("Redis 7.4")),
            "{form:?}"
        );
        type_text(&mut app, "1h");
        assert!(matches!(
            &writes(&app.handle_key(key(KeyCode::Enter)))[..],
            [WriteOp::KeyTtl { .. }]
        ));
    }

    #[test]
    fn r_renames_with_renamenx_and_says_when_the_name_is_taken() {
        let mut app = hash_app();
        app.views.keys.rows = vec![cart()];
        app.handle_key(ch('R'));
        assert_eq!(
            app.views.value.form.as_ref().unwrap().fields[0].text,
            "user:10293:cart"
        );
        // The same name sends nothing.
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        app.handle_key(ctrl('u'));
        type_text(&mut app, "user:2:cart");
        let e = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            writes(&e),
            [WriteOp::Rename {
                key: "user:10293:cart".into(),
                new: "user:2:cart".into()
            }]
        );
        let id = write_ids(&e)[0];
        testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(0))));
        assert!(notice(&app).contains("exists already"), "{}", notice(&app));
        assert_eq!(app.views.value.row.as_ref().unwrap().key, "user:10293:cart");

        app.handle_key(ch('R'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "user:2:cart");
        let id = write_ids(&app.handle_key(key(KeyCode::Enter)))[0];
        testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(1))));
        assert_eq!(app.views.value.row.as_ref().unwrap().key, "user:2:cart");
        assert_eq!(app.views.keys.rows[0].key, "user:2:cart");
        // Going back and opening it again keeps what was loaded.
        app.handle_key(key(KeyCode::Esc));
        let e = app.handle_key(key(KeyCode::Enter));
        assert!(value_requests(&e).is_empty());
    }

    #[test]
    fn d_asks_then_deletes_the_selected_item() {
        let mut app = hash_app();
        let e = app.handle_key(ch('d'));
        assert!(writes(&e).is_empty());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        assert_eq!(
            writes(&app.handle_key(ch('y'))),
            [WriteOp::HDel {
                key: "user:10293:cart".into(),
                field: Raw::from("sku:44812")
            }]
        );
        let del = |mut app: App| {
            app.handle_key(ch('d'));
            writes(&app.handle_key(ch('y')))
        };
        assert!(matches!(&del(set_app())[..], [WriteOp::SRem { .. }]));
        assert!(matches!(&del(zset_app())[..], [WriteOp::ZRem { .. }]));
        assert_eq!(
            del(stream_app()),
            [WriteOp::XDel {
                key: "events".into(),
                id: "1-0".into()
            }]
        );
        // A list element: by index, with its value, and a marker of its own.
        let mut markers = Vec::new();
        for _ in 0..2 {
            let mut app = list_app();
            app.handle_key(key(KeyCode::End));
            app.handle_key(ch('d'));
            let w = writes(&app.handle_key(ch('y')));
            let [
                WriteOp::LDel {
                    index, old, marker, ..
                },
            ] = &w[..]
            else {
                panic!("{w:?}");
            };
            assert_eq!((*index, old.clone()), (2, Raw::from("a")));
            markers.push(marker.clone());
        }
        assert_ne!(markers[0], markers[1]);
        // A string has no items: d says so.
        let mut app = string_app("x");
        app.handle_key(ch('d'));
        assert!(app.confirm.is_none());
        assert!(notice(&app).starts_with("d deletes a field"));
    }

    #[test]
    fn after_a_write_the_value_ttl_and_length_are_read_again() {
        let mut app = hash_app();
        app.views.keys.rows = vec![cart()];
        let e = edit(&mut app, "x");
        let id = write_ids(&e)[0];
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(0))));
        assert_eq!(notice(&app), "Saved.");
        let reqs = value_requests(&e);
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        assert_eq!(reqs[0].1.at, None);
        // The key's row, read the way the key list reads it: no SCAN, only this key.
        let meta = meta_requests(&e);
        assert_eq!(
            meta[0].1,
            LiveRequest::Keys(KeysRequest::Page {
                pattern: "user:{id}:cart".into(),
                cursor: 0,
                carry: vec!["user:10293:cart".into()],
                sizes: true,
                freq: true,
            })
        );
        let mut now = cart();
        now.ttl = Ttl::Persistent;
        now.length = Some(4);
        testing::reply(&mut app, meta[0].0, page_of(vec![now]));
        let r = app.views.value.row.as_ref().unwrap();
        assert_eq!((r.ttl, r.length), (Ttl::Persistent, Some(4)));
        assert_eq!(app.views.keys.rows[0].length, Some(4));
        assert_eq!(app.views.keys.rows[0].ttl, Ttl::Persistent);
        assert!(!app.views.value.gone);
        // The value read again; another edit, and no row this time: the key is gone.
        testing::reply(&mut app, reqs[0].0, page(Some("hash"), fields(), None));
        let id = write_ids(&edit(&mut app, "y"))[0];
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(0))));
        testing::reply(&mut app, meta_requests(&e)[0].0, page_of(Vec::new()));
        assert!(app.views.value.gone);
    }

    fn page_of(rows: Vec<KeyRow>) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Keys(KeysReply::Page(crate::live::keys::Page {
            rows,
            cursor: 0,
            carry: Vec::new(),
            other_pattern: 0,
            unreadable: 0,
            refused: Vec::new(),
        })))
    }

    fn meta_requests(effects: &[Effect]) -> Vec<(ReqId, LiveRequest)> {
        testing::requests(effects)
            .into_iter()
            .filter(|(_, r)| matches!(r, LiveRequest::Keys(_)))
            .collect()
    }

    #[test]
    fn a_value_changed_since_it_was_loaded_is_not_written_and_read_again() {
        let mut app = string_app("hello");
        let id = write_ids(&edit(&mut app, "hi"))[0];
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Changed)));
        assert!(writes(&e).is_empty());
        assert!(notice(&app).contains("changed since it was loaded"));
        assert!(app.notice.as_ref().unwrap().error);
        assert_eq!(value_requests(&e).len(), 1);
        assert_eq!(meta_requests(&e).len(), 1);
        // Until the value is read again there is nothing to edit.
        app.handle_key(ch('e'));
        assert!(app.views.value.form.is_none());
        assert_eq!(notice(&app), "Nothing is loaded to edit yet.");
    }

    #[test]
    fn capital_d_asks_then_goes_back_without_the_key() {
        let mut app = hash_app();
        app.views.keys.rows = vec![cart(), row("user:7:cart", KeyType::Hash, Some(1), Some(80))];
        app.handle_key(ch('D'));
        let c = app.confirm.as_ref().unwrap();
        assert_eq!(c.expect, Expect::YesNo);
        let e = app.handle_key(ch('y'));
        assert_eq!(
            writes(&e),
            [WriteOp::Unlink {
                key: "user:10293:cart".into()
            }]
        );
        let id = write_ids(&e)[0];
        testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(1))));
        assert_eq!(app.screen(), Screen::KeyList);
        let keys: Vec<&str> = app.views.keys.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["user:7:cart"]);
        assert_eq!(notice(&app), "Deleted user:10293:cart.");
        assert!(app.views.value.row.is_none());
    }

    #[test]
    fn on_prod_with_writes_on_edits_ask_and_deletes_need_the_key_name() {
        let mut app = hash_app();
        set_profile(&mut app, Env::Prod, Some(false));
        assert!(writes(&edit(&mut app, "x")).is_empty());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        let e = app.handle_key(ch('y'));
        assert_eq!(writes(&e).len(), 1);
        // Another write waits for this one's answer.
        app.handle_key(ch('D'));
        assert!(app.confirm.is_none());
        assert_eq!(notice(&app), "The last write has not answered yet.");
        testing::reply(
            &mut app,
            write_ids(&e)[0],
            Ok(LiveReply::Write(WriteReply::Int(0))),
        );

        app.handle_key(ch('D'));
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("user:10293:cart".into())
        );
        assert!(writes(&app.handle_key(ch('y'))).is_empty());
        app.handle_key(key(KeyCode::Backspace));
        type_text(&mut app, "user:10293:cart");
        assert!(matches!(
            &writes(&app.handle_key(key(KeyCode::Enter)))[..],
            [WriteOp::Unlink { .. }]
        ));
    }

    /// Rule: a read-only profile never gets a write, whatever is pressed.
    #[test]
    fn a_read_only_profile_gets_no_write_from_any_edit_key() {
        let follow: Vec<KeyEvent> = "yx1prod-cache-01"
            .chars()
            .map(ch)
            .chain([
                key(KeyCode::Enter),
                key(KeyCode::Tab),
                key(KeyCode::Right),
                key(KeyCode::Enter),
                ch('y'),
                key(KeyCode::Enter),
            ])
            .collect();
        let apps: [fn() -> App; 6] = [hash_app, list_app, set_app, zset_app, stream_app, || {
            string_app("x")
        }];
        for (env, ro) in [
            (Env::Prod, None),
            (Env::Prod, Some(true)),
            (Env::Local, Some(true)),
        ] {
            for make in apps {
                for k in ['e', '+', 't', 'R', 'd', 'D'] {
                    let mut app = make();
                    set_profile(&mut app, env, ro);
                    let mut effects = app.handle_key(ch(k));
                    if app.views.value.form.is_none() && app.confirm.is_none() {
                        let n = notice(&app);
                        assert!(
                            n.contains("read-only")
                                || n.contains("off")
                                || n.contains("adds to")
                                || n.contains("deletes a field")
                                || n.contains("can't be edited"),
                            "{k}: {n}"
                        );
                    }
                    assert!(app.views.value.form.is_none(), "{k}: a form opened");
                    assert!(app.confirm.is_none(), "{k}: a question opened");
                    for f in &follow {
                        effects.extend(app.handle_key(*f));
                    }
                    effects.extend(app.handle_paste("user:10293:cart".into()));
                    effects.extend(app.handle_key(key(KeyCode::Enter)));
                    assert!(writes(&effects).is_empty(), "{k}: {effects:?}");
                    assert!(
                        !effects.iter().any(|e| matches!(e, Effect::Editor(_))),
                        "{k}"
                    );
                }
            }
        }
        // The read-only reason is the one shown.
        let mut app = hash_app();
        set_profile(&mut app, Env::Prod, None);
        app.handle_key(ch('e'));
        assert!(notice(&app).contains("is read-only"), "{}", notice(&app));
    }

    #[test]
    fn a_big_or_multi_line_value_goes_to_the_editor() {
        let mut app = string_app("line 1\nline 2");
        let e = app.handle_key(ch('e'));
        assert!(app.views.value.form.is_none());
        let jobs: Vec<&EditorJob> = e
            .iter()
            .filter_map(|e| match e {
                Effect::Editor(j) => Some(j),
                _ => None,
            })
            .collect();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].value, Raw::from("line 1\nline 2"));
        assert_eq!(jobs[0].ext, "txt");
        let job = jobs[0].id;
        // A failed editor sends nothing and says why.
        let e = app.apply(Msg::Edited {
            job,
            result: Err("vi exited with exit status: 1, so nothing was sent".into()),
        });
        assert!(writes(&e).is_empty());
        assert!(notice(&app).starts_with("Not edited: vi exited"));

        let e = app.handle_key(ch('e'));
        let Some(Effect::Editor(j)) = e.iter().find(|e| matches!(e, Effect::Editor(_))) else {
            panic!("{e:?}");
        };
        let job = j.id;
        // Another job's answer is ignored.
        assert!(
            writes(&app.apply(Msg::Edited {
                job: job + 100,
                result: Ok(Raw::from("other"))
            }))
            .is_empty()
        );
        let e = app.apply(Msg::Edited {
            job,
            result: Ok(Raw::from("line 1\nline 3")),
        });
        assert_eq!(
            writes(&e),
            [WriteOp::SetString {
                key: "greeting".into(),
                old: Raw::from("line 1\nline 2"),
                value: Raw::from("line 1\nline 3")
            }]
        );
        // Over 4 KiB too, and unchanged sends nothing.
        let long = "a".repeat(INLINE_MAX + 1);
        let mut app = string_app(&long);
        let e = app.handle_key(ch('e'));
        let Some(Effect::Editor(j)) = e.iter().find(|e| matches!(e, Effect::Editor(_))) else {
            panic!("{e:?}");
        };
        let e = app.apply(Msg::Edited {
            job: j.id,
            result: Ok(Raw::from(long.as_str())),
        });
        assert!(writes(&e).is_empty());
        assert_eq!(notice(&app), "Not changed, so nothing was sent.");
        assert_eq!(inline_text(&Raw::from("ok")), Some("ok".into()));
        assert_eq!(inline_text(&Raw::from("a\tb")), None);
        assert_eq!(inline_text(&Raw::new(vec![0xff])), None);
    }

    #[test]
    fn values_typed_into_a_form_stay_on_screen() {
        let mut app = hash_app();
        app.handle_key(ch('e'));
        app.handle_key(ctrl('u'));
        app.handle_paste(MARK.into());
        let debug = format!("{:?} {:?}", app.views, app.view_input().map(|i| i.label));
        assert!(!value::leaks(&debug, MARK), "{debug}");
        assert!(text(&testing::render(&app, 80, 24)).contains(MARK));
        let e = app.handle_key(key(KeyCode::Enter));
        assert!(!value::leaks(&format!("{e:?}"), MARK));
        let id = write_ids(&e)[0];
        testing::reply(&mut app, id, Err(LiveError::Failed("HSET failed".into())));
        assert_eq!(notice(&app), "Not written: HSET failed.");
    }

    #[test]
    fn the_inline_edit_prompt_80x24() {
        let mut app = hash_app();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ch('e'));
        let t = testing::render(&app, 80, 24);
        let all = text(&t);
        assert!(all.contains("Edit field sku:44813"), "{all}");
        assert!(all.contains(r#"{"qty":1,"price":4.50}"#), "{all}");
        insta::assert_snapshot!("edit_prompt_80x24", t.backend());
        // The key bar still fits 80 columns with the form's keys.
        let last = all.lines().last().unwrap();
        assert!(last.contains("enter send"), "{last}");
    }
}
