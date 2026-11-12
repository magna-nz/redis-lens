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

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};

use super::keys::{list_move, plain};
use super::{Act, ViewState};
use crate::live::keys::KeyRow;
use crate::live::values::{
    At, Items, Kind, MAX_ITEMS, Reply, Request, VECTOR_SAMPLE, field_ttls_supported,
};
use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
use crate::tui::clipboard::CopyText;
use crate::value::{self, CopyRefusal, Format, Raw};

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
        match act.send(LiveRequest::Value(req)) {
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

impl ViewState for ValueState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        self.keep_going(act);
    }

    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        self.keep_going(act);
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        let tick = act.tick;
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
            _ => false,
        }
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, _act: &mut Act<'_>) {
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
    fn the_key_detail_reads_one_server() {
        assert!(super::super::reads_one_server(Screen::KeyDetail));
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
}
