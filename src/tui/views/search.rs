//! State of the Search view (spec "6 Search"). Owner: wave B. Drawn by `ui/search.rs`; its
//! live requests are in `src/live/search.rs`.
//!
//! Phase 1 lists the indexes (`FT._LIST`, `FT.INFO`) and shows the schema of the selected
//! one. No query is sent and nothing is created or dropped. The list is read on show and
//! with `r`; it does not poll. See [`super`] for the interface `App` calls.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};

use super::streams::{list_move, off_text};
use super::{Act, ViewState};
use crate::live::search::{IndexInfo, IndexList, Reply, Request};
use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};

/// Rows of the schema list in the details panel that fit at once.
const SCHEMA_PAGE: usize = 4;

/// What `↑↓` moves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Indexes,
    /// The fields of the selected index.
    Schema,
}

/// The indexes and the schema of the selected one.
#[derive(Debug, Default)]
pub struct SearchState {
    pub list: IndexList,
    pub sel: usize,
    pub schema_sel: usize,
    pub focus: Focus,
    /// An answer has arrived (so an empty list means no indexes, not "not read yet").
    pub loaded: bool,
    /// `FT._LIST` is unknown: the server has no Search module.
    pub no_module: bool,
    /// Why the view is off (the user may not run `FT._LIST`), in redis-lens's own words.
    pub off: Option<String>,
    /// Why the last read failed. The previous list stays.
    pub error: Option<String>,
    pending: Option<ReqId>,
}

impl SearchState {
    /// The selected index.
    pub fn selected(&self) -> Option<&IndexInfo> {
        self.list.indexes.get(self.sel)
    }

    fn fetch(&mut self, act: &mut Act<'_>) {
        self.pending = act.send(LiveRequest::Search(Request::Indexes));
    }

    fn in_flight(&mut self, act: &Act<'_>) -> bool {
        match self.pending {
            Some(id) if act.in_flight(id) => true,
            Some(_) => {
                self.pending = None;
                false
            }
            None => false,
        }
    }
}

impl ViewState for SearchState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        // Read once; `r` reads again. A view that turned itself off waits for `r` too.
        if !self.loaded && self.off.is_none() && !self.no_module && !self.in_flight(act) {
            self.fetch(act);
        }
    }

    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        // Not a polling view, but a read that was waiting for the live connection (the
        // view was shown before it opened) goes out as soon as it is ready.
        if !self.loaded
            && self.off.is_none()
            && !self.no_module
            && act.live_ready()
            && !self.in_flight(act)
        {
            self.fetch(act);
        }
    }

    fn refresh(&mut self, act: &mut Act<'_>) {
        self.off = None;
        self.no_module = false;
        if !self.in_flight(act) {
            self.fetch(act);
        }
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.focus = match self.focus {
                Focus::Indexes => Focus::Schema,
                Focus::Schema => Focus::Indexes,
            };
            return true;
        }
        let schema_len = self.selected().map_or(0, |i| i.schema.len());
        let (cur, len, page) = match self.focus {
            Focus::Indexes => (self.sel, self.list.indexes.len(), act.page),
            Focus::Schema => (self.schema_sel, schema_len, SCHEMA_PAGE),
        };
        let Some(to) = list_move(&key, cur, len, page) else {
            return false;
        };
        match self.focus {
            Focus::Indexes => {
                if to != self.sel {
                    self.schema_sel = 0;
                }
                self.sel = to;
            }
            Focus::Schema => self.schema_sel = to,
        }
        true
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, _act: &mut Act<'_>) {
        if self.pending == Some(id) {
            self.pending = None;
        }
        match result {
            Ok(LiveReply::Search(Reply::Indexes(list))) => {
                let name = self.selected().map(|i| i.name.clone());
                self.list = list;
                self.sel = name
                    .and_then(|n| self.list.indexes.iter().position(|i| i.name == n))
                    .unwrap_or(self.sel)
                    .min(self.list.indexes.len().saturating_sub(1));
                self.schema_sel = self.schema_sel.min(
                    self.selected()
                        .map_or(0, |i| i.schema.len())
                        .saturating_sub(1),
                );
                self.loaded = true;
                self.no_module = false;
                self.error = None;
            }
            Ok(_) => {}
            // `FT._LIST` is not a command: the module is missing.
            Err(LiveError::Unsupported(_)) => {
                self.no_module = true;
                self.loaded = true;
                self.list = IndexList::default();
            }
            Err(e @ LiveError::Denied(_)) => self.off = Some(off_text(&e)),
            Err(LiveError::Closed) => {}
            Err(LiveError::Failed(why)) => self.error = Some(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{self, key};
    use super::*;
    use crate::live::search::FieldInfo;
    use crate::model::ServerFlavor;
    use crate::tui::app::{App, Msg, Screen};

    fn field(name: &str, path: &str, ty: &str, flags: &[&str]) -> FieldInfo {
        FieldInfo {
            identifier: path.into(),
            name: name.into(),
            ty: ty.into(),
            flags: flags.iter().map(|f| (*f).into()).collect(),
        }
    }

    fn list(with_size: bool) -> IndexList {
        IndexList {
            indexes: vec![
                IndexInfo {
                    name: "idx:products".into(),
                    on: Some("HASH".into()),
                    prefixes: vec!["product:".into()],
                    docs: Some(120_345),
                    bytes: with_size.then_some(48 * 1024 * 1024),
                    indexing: Some(false),
                    progress: Some(1.0),
                    failures: Some(0),
                    schema: vec![
                        field("title", "title", "TEXT", &["SORTABLE"]),
                        field("price", "price", "NUMERIC", &["SORTABLE"]),
                        field("tags", "tags", "TAG", &[]),
                        field("embedding", "embedding", "VECTOR", &[]),
                        field("brand", "brand", "TAG", &[]),
                        field("stock", "stock", "NUMERIC", &[]),
                    ],
                },
                IndexInfo {
                    name: "idx:sessions".into(),
                    on: Some("JSON".into()),
                    prefixes: vec!["session:".into(), "sess:".into()],
                    docs: Some(8_200),
                    bytes: with_size.then_some(3 * 1024 * 1024),
                    indexing: Some(true),
                    progress: Some(0.42),
                    failures: Some(17),
                    schema: vec![field("user", "$.user.id", "TAG", &[])],
                },
            ],
            total: 2,
            notes: Vec::new(),
        }
    }

    fn answer(app: &mut App, id: ReqId, result: Result<IndexList, LiveError>) {
        testing::reply(
            app,
            id,
            result.map(|l| LiveReply::Search(Reply::Indexes(l))),
        );
    }

    fn shown() -> (App, ReqId) {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::Search);
        let reqs = testing::requests(&effects);
        let [(id, LiveRequest::Search(Request::Indexes))] = &reqs[..] else {
            panic!("expected one Indexes request, got {reqs:?}");
        };
        (app, *id)
    }

    fn loaded(with_size: bool) -> App {
        let (mut app, id) = shown();
        answer(&mut app, id, Ok(list(with_size)));
        app
    }

    #[test]
    fn on_show_reads_the_indexes_once() {
        let (mut app, id) = shown();
        let t0 = Instant::now();
        app.apply(Msg::TickAt(t0));
        // Still in flight: nothing more, on ticks or on showing it again.
        assert!(
            testing::requests(&app.apply(Msg::TickAt(t0 + std::time::Duration::from_secs(30))))
                .is_empty()
        );
        answer(&mut app, id, Ok(list(true)));
        // No polling.
        assert!(
            testing::requests(&app.apply(Msg::TickAt(t0 + std::time::Duration::from_secs(300))))
                .is_empty()
        );
        // `r` reads again.
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(testing::requests(&effects).len(), 1);
    }

    #[test]
    fn a_missing_module_is_said_not_shown_as_an_error() {
        let (mut app, id) = shown();
        answer(&mut app, id, Err(LiveError::Unsupported("FT._LIST".into())));
        let s = &app.views.search;
        assert!(s.no_module && s.loaded && s.off.is_none() && s.error.is_none());
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
        // `r` looks again (a module can be loaded while the server runs).
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(testing::requests(&effects).len(), 1);
        assert!(!app.views.search.no_module);
    }

    #[test]
    fn a_refused_ft_list_turns_the_view_off_with_fixed_text() {
        let (mut app, id) = shown();
        answer(&mut app, id, Err(LiveError::Denied("FT._LIST".into())));
        assert_eq!(
            app.views.search.off.as_deref(),
            Some("The server refused FT._LIST for this user.")
        );
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn a_failed_read_keeps_the_list() {
        let mut app = loaded(true);
        let effects = app.handle_key(key(KeyCode::Char('r')));
        let (id, _) = testing::requests(&effects).remove(0);
        answer(
            &mut app,
            id,
            Err(LiveError::Failed("FT.INFO failed".into())),
        );
        assert_eq!(app.views.search.list.indexes.len(), 2);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn keys_move_the_indexes_and_the_schema() {
        let mut app = loaded(true);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.search.focus, Focus::Schema);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.views.search.schema_sel, 5);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.views.search.schema_sel, 5, "clamped");
        app.handle_key(key(KeyCode::BackTab));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.views.search.sel, 1);
        assert_eq!(
            app.views.search.schema_sel, 0,
            "a new index starts at its top"
        );
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.views.search.sel, 1, "clamped");
    }

    #[test]
    fn snapshots() {
        let mut app = loaded(true);
        insta::assert_snapshot!("redis_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("redis_120x36", testing::render(&app, 120, 36).backend());
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::End));
        insta::assert_snapshot!("schema_80x24", testing::render(&app, 80, 24).backend());
        // valkey-search reports no size: the column is left out, with the reason.
        let mut vk = loaded(false);
        vk.session.as_mut().unwrap().server.as_mut().unwrap().flavor = ServerFlavor::Valkey;
        insta::assert_snapshot!("valkey_80x24", testing::render(&vk, 80, 24).backend());
        insta::assert_snapshot!("valkey_120x36", testing::render(&vk, 120, 36).backend());
    }

    #[test]
    fn no_indexes_and_indexes_without_info() {
        let (mut app, id) = shown();
        answer(&mut app, id, Ok(IndexList::default()));
        insta::assert_snapshot!("none_80x24", testing::render(&app, 80, 24).backend());
        let effects = app.handle_key(key(KeyCode::Char('r')));
        let (id, _) = testing::requests(&effects).remove(0);
        answer(
            &mut app,
            id,
            Ok(IndexList {
                indexes: vec![IndexInfo {
                    name: "idx:a".into(),
                    ..IndexInfo::default()
                }],
                total: 1,
                notes: vec!["the server refused FT.INFO for this user".into()],
            }),
        );
        insta::assert_snapshot!("names_only_80x24", testing::render(&app, 80, 24).backend());
    }
}
