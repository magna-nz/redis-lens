//! State of the key list: the keys of one pattern, opened with `enter` on a pattern on
//! Patterns (spec "1 Patterns, additions", "Key list"). Owner: wave B2. Drawn by
//! `ui/keys.rs`; its live requests are in `src/live/keys.rs`.
//!
//! The list loads [`PAGE_KEYS`](crate::live::keys::PAGE_KEYS) keys of the pattern when
//! it opens and as many more with each `n`. Only keys that infer to the pattern are
//! listed. `/` filters the loaded keys by name, `o` sorts them, `y` copies the selected
//! key name, `esc` clears the filter and then goes back to Patterns. `enter` on a key
//! does nothing yet (phase 2). See [`super`] for the interface `App` calls.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{Act, Ctx, Input, ViewState};
use crate::live::keys::{KeyRow, Page, Reply, Request};
use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{PERM_FREQ, PERM_MEMORY, PermStatus, Ttl};
use crate::tui::clipboard::CopyText;

/// How the loaded keys are ordered. `o` cycles through the ones that have data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeySort {
    /// `MEMORY USAGE`, largest first. Skipped without sizes.
    #[default]
    Size,
    /// The length, largest first.
    Length,
    /// The TTL, longest first; keys without one first of all.
    Ttl,
    /// The LFU counter, highest first. Only with LFU counters.
    Freq,
    /// The name, A to Z.
    Name,
}

impl KeySort {
    const ORDER: [KeySort; 5] = [Self::Size, Self::Length, Self::Ttl, Self::Freq, Self::Name];

    pub fn label(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Length => "length",
            Self::Ttl => "TTL",
            Self::Freq => "LFU",
            Self::Name => "name",
        }
    }

    pub fn ascending(self) -> bool {
        self == Self::Name
    }

    fn available(self, sizes: bool, freq: bool) -> bool {
        match self {
            Self::Size => sizes,
            Self::Freq => freq,
            _ => true,
        }
    }

    /// The sort in use: `self`, or the next available one.
    pub fn effective(self, sizes: bool, freq: bool) -> Self {
        if self.available(sizes, freq) {
            self
        } else {
            self.next(sizes, freq)
        }
    }

    /// The next available sort after `self`.
    pub fn next(self, sizes: bool, freq: bool) -> Self {
        let at = Self::ORDER.iter().position(|s| *s == self).unwrap_or(0);
        (1..=Self::ORDER.len())
            .map(|i| Self::ORDER[(at + i) % Self::ORDER.len()])
            .find(|s| s.available(sizes, freq))
            .unwrap_or(Self::Name)
    }

    fn cmp(self, a: &KeyRow, b: &KeyRow) -> Ordering {
        // Largest first, a missing value last; ties by name.
        fn desc<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
            match (a, b) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        }
        let ttl_rank = |r: &KeyRow| match r.ttl {
            Ttl::Persistent => u128::MAX,
            Ttl::Expires(d) => d.as_millis(),
        };
        let by = match self {
            Self::Size => desc(a.bytes, b.bytes),
            Self::Length => desc(a.length, b.length),
            Self::Ttl => ttl_rank(b).cmp(&ttl_rank(a)),
            Self::Freq => desc(a.freq, b.freq),
            Self::Name => Ordering::Equal,
        };
        by.then_with(|| a.key.cmp(&b.key))
    }
}

#[derive(Debug, Default)]
pub struct KeysState {
    /// The pattern whose keys are listed, as `PatternStats::pattern`.
    pub pattern: Option<String>,
    /// The keys loaded so far, in the order they came.
    pub rows: Vec<KeyRow>,
    seen: HashSet<String>,
    /// Where the `SCAN` goes on.
    cursor: u64,
    /// Keys found but not loaded yet.
    carry: Vec<String>,
    /// The `SCAN` came back to 0 and every key found is loaded.
    pub done: bool,
    /// Pages answered so far. Only the first is loaded on its own; `n` loads the rest,
    /// even when a page found no key of the pattern.
    pub pages: u64,
    /// The page request in flight.
    loading: Option<ReqId>,
    /// Why the last page failed, in redis-lens's own words.
    pub error: Option<String>,
    /// Commands the server refused or lacks: their columns are off.
    pub refused: Vec<LiveError>,
    /// Keys the glob matched that infer to another pattern.
    pub other_pattern: u64,
    /// Keys that could not be read.
    pub unreadable: u64,
    /// `MEMORY USAGE` was sent (the permission check allowed it).
    pub sizes: bool,
    /// `OBJECT FREQ` was sent (LFU policy, and allowed).
    pub freq: bool,
    /// The policy is LFU, so the LFU column shows, even when `OBJECT FREQ` is refused.
    pub lfu: bool,
    pub filter: String,
    /// The filter input is open.
    pub filtering: bool,
    pub sort: KeySort,
    /// The selected row among [`KeysState::visible`].
    pub sel: usize,
    /// The tick when the selection last changed, for the scrolling name.
    pub sel_since: u64,
}

impl KeysState {
    /// `enter` on a pattern on Patterns: list `pattern`'s keys. `App` calls this, then
    /// [`ViewState::on_show`]. It runs every time, also for the pattern already open:
    /// that one keeps its loaded keys, filter and sort; another starts afresh.
    pub fn open(&mut self, pattern: String, act: &mut Act<'_>) {
        if self.pattern.as_deref() == Some(pattern.as_str()) {
            return;
        }
        *self = Self {
            pattern: Some(pattern),
            sel_since: act.tick,
            ..Self::default()
        };
    }

    /// Indices into [`KeysState::rows`] after the filter, in the sort order.
    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        let mut idx: Vec<usize> = (0..self.rows.len())
            .filter(|&i| f.is_empty() || self.rows[i].key.to_lowercase().contains(&f))
            .collect();
        let sort = self.sort.effective(self.sizes, self.freq);
        idx.sort_by(|&a, &b| sort.cmp(&self.rows[a], &self.rows[b]));
        idx
    }

    /// The selected row, if any.
    pub fn selected(&self) -> Option<&KeyRow> {
        let v = self.visible();
        v.get(self.sel.min(v.len().saturating_sub(1)))
            .map(|&i| &self.rows[i])
    }

    /// True while a page is being loaded.
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Selects the row of `key` if it is visible, else keeps the position in range.
    fn reselect(&mut self, key: Option<String>, tick: u64) {
        let v = self.visible();
        let at = key.and_then(|k| v.iter().position(|&i| self.rows[i].key == k));
        match at {
            Some(i) => {
                if i != self.sel {
                    self.sel = i;
                }
            }
            None => {
                let clamped = self.sel.min(v.len().saturating_sub(1));
                if clamped != self.sel {
                    self.sel = clamped;
                    self.sel_since = tick;
                }
            }
        }
    }

    fn move_to(&mut self, sel: usize, tick: u64) {
        let last = self.visible().len().saturating_sub(1);
        let sel = sel.min(last);
        if sel != self.sel {
            self.sel = sel;
            self.sel_since = tick;
        }
    }

    /// Sends the next page. False when nothing could be sent.
    fn load(&mut self, act: &mut Act<'_>) -> bool {
        let Some(pattern) = self.pattern.clone() else {
            return false;
        };
        if self.rows.is_empty() && self.carry.is_empty() && self.cursor == 0 {
            // The first page: what to read comes from the permission check and policy.
            let allowed = |label: &str| matches!(act.check(label), Some(PermStatus::Allowed));
            self.sizes = act
                .report()
                .map_or(allowed(PERM_MEMORY), |r| r.sizes_available);
            self.lfu = act.server().is_some_and(|s| s.lfu_enabled());
            self.freq = self.lfu && allowed(PERM_FREQ);
        }
        let req = LiveRequest::Keys(Request::Page {
            pattern,
            cursor: self.cursor,
            carry: self.carry.clone(),
            sizes: self.sizes,
            freq: self.freq,
        });
        match act.send(req) {
            Some(id) => {
                self.loading = Some(id);
                self.error = None;
                true
            }
            None => false,
        }
    }

    /// Forgets a request that will never be answered (a rescan replaced the live
    /// connection), and loads the first page when none has come yet.
    fn keep_going(&mut self, act: &mut Act<'_>) {
        if self.loading.is_some_and(|id| !act.in_flight(id)) {
            self.loading = None;
        }
        let first = self.pages == 0 && !self.done && self.error.is_none();
        if first && self.loading.is_none() && act.live_ready() {
            self.load(act);
        }
    }

    fn take_page(&mut self, page: Page, tick: u64) {
        let keep = self.selected().map(|r| r.key.clone());
        for row in page.rows {
            if self.seen.insert(row.key.clone()) {
                self.rows.push(row);
            }
        }
        self.pages += 1;
        self.cursor = page.cursor;
        self.carry = page.carry;
        self.done = self.cursor == 0 && self.carry.is_empty();
        self.other_pattern += page.other_pattern;
        self.unreadable += page.unreadable;
        for e in page.refused {
            if !self.refused.contains(&e) {
                self.refused.push(e);
            }
        }
        self.reselect(keep, tick);
    }

    fn filter_key(&mut self, key: KeyEvent, tick: u64) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(c);
            }
            _ => return,
        }
        self.sel = 0;
        self.sel_since = tick;
    }
}

/// A plain character key, without ctrl or alt.
pub(crate) fn plain(key: &KeyEvent) -> Option<char> {
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

/// Moves for the standard list keys: `↑↓` `j` `k`, `PgUp` `PgDn`, `Home` `End` `g` `G`.
/// The new selection, or `None` when `key` is not one of them.
pub(crate) fn list_move(key: &KeyEvent, sel: usize, len: usize, ctx: &Ctx<'_>) -> Option<usize> {
    let last = len.saturating_sub(1);
    let page = ctx.page.max(1);
    Some(match (key.code, plain(key)) {
        (KeyCode::Up, _) | (_, Some('k')) => sel.saturating_sub(1),
        (KeyCode::Down, _) | (_, Some('j')) => (sel + 1).min(last),
        (KeyCode::PageUp, _) => sel.saturating_sub(page),
        (KeyCode::PageDown, _) => (sel + page).min(last),
        (KeyCode::Home, _) | (_, Some('g')) => 0,
        (KeyCode::End, _) | (_, Some('G')) => last,
        _ => return None,
    })
}

/// Why a page failed, for the screen.
pub fn load_error(e: &LiveError) -> String {
    match e {
        LiveError::Denied(cmd) if cmd == "SCAN" => {
            "The server refused SCAN for this user, so the keys can't be listed.".to_string()
        }
        LiveError::Closed => "The live connection is closed.".to_string(),
        e => format!("Could not load the keys: {e}."),
    }
}

impl ViewState for KeysState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        self.keep_going(act);
    }

    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        self.keep_going(act);
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        let tick = act.tick;
        if self.filtering {
            let keep = self.selected().map(|r| r.key.clone());
            self.filter_key(key, tick);
            if key.code == KeyCode::Enter {
                self.reselect(keep, tick);
            }
            return true;
        }
        let len = self.visible().len();
        if let Some(sel) = list_move(&key, self.sel, len, act) {
            self.move_to(sel, tick);
            return true;
        }
        match (key.code, plain(&key)) {
            (KeyCode::Esc, _) if !self.filter.is_empty() => {
                let keep = self.selected().map(|r| r.key.clone());
                self.filter.clear();
                self.reselect(keep, tick);
                true
            }
            (_, Some('/')) => {
                self.filtering = true;
                true
            }
            (_, Some('o')) => {
                let keep = self.selected().map(|r| r.key.clone());
                self.sort = self
                    .sort
                    .effective(self.sizes, self.freq)
                    .next(self.sizes, self.freq);
                self.reselect(keep, tick);
                act.info(format!("Keys sorted by {}.", self.sort.label()));
                true
            }
            (_, Some('n')) => {
                if self.loading.is_some_and(|id| act.in_flight(id)) {
                    act.info("Still loading keys…");
                } else if self.done {
                    act.info(format!(
                        "All {} matching keys are loaded.",
                        crate::format::grouped(self.rows.len() as u64)
                    ));
                } else if !self.load(act) {
                    act.info("The live connection is not open, so no more keys can be loaded.");
                }
                true
            }
            (_, Some('y')) => {
                match self.selected() {
                    Some(r) => {
                        let text = CopyText::key(&r.key);
                        act.copy(text);
                    }
                    None => act.info("Nothing selected to copy."),
                }
                true
            }
            _ => false,
        }
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        if self.loading != Some(id) {
            return;
        }
        self.loading = None;
        match result {
            Ok(LiveReply::Keys(Reply::Page(page))) => self.take_page(page, act.tick),
            Ok(_) => {}
            Err(e) => self.error = Some(load_error(&e)),
        }
    }

    fn input(&self) -> Option<Input> {
        self.filtering.then(|| Input {
            label: "/",
            text: self.filter.clone(),
            hint: "   enter to apply · esc to clear",
        })
    }

    fn on_paste(&mut self, text: &str, act: &mut Act<'_>) {
        self.filter.extend(text.chars().filter(|c| !c.is_control()));
        self.sel = 0;
        self.sel_since = act.tick;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::testing::{self, key};
    use super::*;
    use crate::live::keys::{KeyRow, Page};
    use crate::model::KeyType;
    use crate::tui::app::{App, Effect, Screen};
    use crate::tui::tree::{Root, Row};

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// A scanned app with the live connection ready, on the key list of `pattern`.
    fn open(pattern: &str) -> (App, Vec<Effect>) {
        let mut app = testing::scanned();
        let mut out = testing::live_ready(&mut app);
        out.extend(app.handle_key(ch('1')));
        let at = {
            let data = app.pattern_data().expect("patterns");
            let name = |i: usize| data.patterns[i].pattern.as_str();
            app.pattern_rows()
                .iter()
                .position(|r| match r {
                    Row::Pattern { idx, .. } => name(*idx) == pattern,
                    Row::Group { root } => {
                        matches!(app.patterns.tree.roots[*root], Root::Single(i) if name(i) == pattern)
                    }
                })
                .expect("a row for the pattern")
        };
        app.patterns.sel = at;
        out.extend(app.handle_key(key(KeyCode::Enter)));
        assert_eq!(app.screen(), Screen::KeyList);
        (app, out)
    }

    /// The one page request among `effects`.
    fn page_request(effects: &[Effect]) -> (ReqId, Request) {
        let reqs = testing::requests(effects);
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        match &reqs[0] {
            (id, LiveRequest::Keys(r @ Request::Page { .. })) => (*id, r.clone()),
            other => panic!("not a page request: {other:?}"),
        }
    }

    fn row(key: &str, ttl: Option<u64>, bytes: u64, enc: &str, len: u64, freq: u8) -> KeyRow {
        KeyRow {
            key: key.into(),
            key_type: KeyType::Hash,
            ttl: ttl.map_or(Ttl::Persistent, |s| Ttl::Expires(Duration::from_secs(s))),
            bytes: Some(bytes),
            encoding: Some(enc.into()),
            length: Some(len),
            freq: Some(freq),
        }
    }

    fn carts() -> Vec<KeyRow> {
        vec![
            row("user:10293:cart", Some(533_520), 3_174, "listpack", 12, 14),
            row("user:10311:cart", Some(475_200), 2_458, "listpack", 9, 3),
            row("user:10388:cart", None, 3_686, "listpack", 14, 1),
            row(
                "user:10402:cart",
                Some(601_200),
                90_112,
                "hashtable",
                1_204,
                41,
            ),
            row("user:10417:cart", Some(7_680), 1_946, "listpack", 6, 7),
            row("user:10455:cart", None, 2_253, "listpack", 8, 0),
            row("user:10460:cart", Some(522_000), 3_072, "listpack", 11, 5),
            row(
                "user:10612:cart",
                Some(266_400),
                14_336,
                "hashtable",
                211,
                22,
            ),
            row(
                "user:204551000000000000000017:cart",
                Some(86_400),
                1_843,
                "listpack",
                6,
                4,
            ),
        ]
    }

    fn page(rows: Vec<KeyRow>, cursor: u64) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Keys(Reply::Page(Page {
            rows,
            cursor,
            carry: Vec::new(),
            other_pattern: 3,
            unreadable: 0,
            refused: Vec::new(),
        })))
    }

    /// The key list with one page of carts loaded, walk not finished.
    fn loaded() -> App {
        let (mut app, effects) = open("user:{id}:cart");
        let (id, _) = page_request(&effects);
        testing::reply(&mut app, id, page(carts(), 4_242));
        app
    }

    fn notice(app: &App) -> String {
        app.notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    #[test]
    fn opening_a_pattern_asks_for_its_first_page() {
        let (_, effects) = open("user:{id}:cart");
        let (_, req) = page_request(&effects);
        assert_eq!(
            req,
            Request::Page {
                pattern: "user:{id}:cart".into(),
                cursor: 0,
                carry: Vec::new(),
                // The fixture's scan had sizes and an LFU policy, all allowed.
                sizes: true,
                freq: true,
            }
        );
    }

    #[test]
    fn key_list_80x24() {
        let app = loaded();
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn key_list_120x36() {
        let mut app = loaded();
        app.handle_key(key(KeyCode::Down));
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn a_page_with_no_key_of_the_pattern_waits_for_n() {
        let (mut app, effects) = open("user:{id}:cart");
        let (id, _) = page_request(&effects);
        testing::reply(&mut app, id, page(Vec::new(), 77));
        let t0 = std::time::Instant::now();
        for i in 0..20 {
            let effects = app.apply(crate::tui::app::Msg::TickAt(
                t0 + std::time::Duration::from_millis(100 * i),
            ));
            assert!(testing::requests(&effects).is_empty(), "tick {i}");
        }
        let (_, req) = page_request(&app.handle_key(ch('n')));
        assert!(matches!(req, Request::Page { cursor: 77, .. }));
    }

    #[test]
    fn n_loads_the_next_page_until_the_walk_is_done() {
        let mut app = loaded();
        let (id, req) = page_request(&app.handle_key(ch('n')));
        let Request::Page { cursor, .. } = req else {
            unreachable!()
        };
        assert_eq!(cursor, 4_242);
        // A second n while it loads sends nothing.
        assert!(testing::requests(&app.handle_key(ch('n'))).is_empty());
        assert_eq!(notice(&app), "Still loading keys…");
        // Duplicates SCAN returned again are dropped.
        let more = vec![
            row("user:10293:cart", None, 1, "listpack", 1, 1),
            row("user:20001:cart", Some(60), 1_024, "listpack", 3, 2),
        ];
        testing::reply(&mut app, id, page(more, 0));
        assert_eq!(app.views.keys.rows.len(), 10);
        assert!(app.views.keys.done);
        assert!(testing::requests(&app.handle_key(ch('n'))).is_empty());
        assert_eq!(notice(&app), "All 10 matching keys are loaded.");
    }

    #[test]
    fn sort_filter_copy_and_esc() {
        let mut app = loaded();
        let first = |app: &App| app.views.keys.selected().map(|r| r.key.clone());
        // By size, largest first.
        assert_eq!(first(&app).as_deref(), Some("user:10402:cart"));
        app.handle_key(ch('o'));
        assert_eq!(notice(&app), "Keys sorted by length.");
        app.handle_key(ch('o'));
        app.handle_key(key(KeyCode::Home));
        // By TTL: keys without one first, then the longest.
        assert_eq!(first(&app).as_deref(), Some("user:10388:cart"));
        // The filter takes every key, digits and q included.
        app.handle_key(ch('/'));
        for c in "104".chars() {
            app.handle_key(ch(c));
        }
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.visible().len(), 4);
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.views.keys.filtering);
        let effects = app.handle_key(ch('y'));
        let copied: Vec<String> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Copy(t) => Some(t.as_str().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(copied, [first(&app).unwrap()]);
        assert!(notice(&app).starts_with("Copied key"), "{}", notice(&app));
        // esc clears the filter first, then goes back to Patterns.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.visible().len(), 9);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Patterns);
    }

    #[test]
    fn enter_on_a_key_does_nothing_yet() {
        let mut app = loaded();
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(effects.is_empty(), "{effects:?}");
        assert_eq!(app.screen(), Screen::KeyList);
    }

    #[test]
    fn the_same_pattern_keeps_its_keys_another_starts_afresh() {
        let mut app = loaded();
        app.handle_key(key(KeyCode::Esc));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(testing::requests(&effects).is_empty());
        assert_eq!(app.views.keys.rows.len(), 9);
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Home));
        let effects = app.handle_key(key(KeyCode::Enter));
        let (_, req) = page_request(&effects);
        let Request::Page { pattern, .. } = req else {
            unreachable!()
        };
        assert_ne!(pattern, "user:{id}:cart");
        assert!(app.views.keys.rows.is_empty());
    }

    #[test]
    fn a_refused_command_turns_off_its_column_in_fixed_words() {
        let (mut app, effects) = open("user:{id}:cart");
        let (id, _) = page_request(&effects);
        let mut rows = carts();
        for r in &mut rows {
            r.bytes = None;
        }
        let reply = Ok(LiveReply::Keys(Reply::Page(Page {
            rows,
            cursor: 0,
            carry: Vec::new(),
            other_pattern: 0,
            unreadable: 2,
            refused: vec![LiveError::Denied("MEMORY USAGE".into())],
        })));
        testing::reply(&mut app, id, reply);
        let screen = format!("{:?}", testing::render(&app, 120, 36).backend());
        assert!(
            screen.contains("Column off: the server refused MEMORY USAGE for this user."),
            "{screen}"
        );
        assert!(screen.contains("2 keys could not be read"), "{screen}");
        // Sorting skips the size column it no longer has... once sizes are off.
        assert!(!screen.contains("NOPERM"), "{screen}");
    }

    #[test]
    fn a_refused_scan_says_so_without_the_servers_text() {
        let (mut app, effects) = open("user:{id}:cart");
        let (id, _) = page_request(&effects);
        testing::reply(&mut app, id, Err(LiveError::Denied("SCAN".into())));
        let screen = format!("{:?}", testing::render(&app, 80, 24).backend());
        assert!(
            screen.contains("The server refused SCAN for this user, so the keys can't be"),
            "{screen}"
        );
    }
}
