//! State of the Search view (spec "6 Search"). Owner: wave B. Drawn by `ui/search.rs`; its
//! live requests are in `src/live/search.rs`.
//!
//! Lists the indexes (`FT._LIST`, `FT.INFO`) and shows the schema of the selected one.
//! The list is read on show and with `r`; it does not poll. On the selected index:
//!
//! - `/` opens a query input; `enter` runs `FT.SEARCH <index> <query> LIMIT 0 50` (an
//!   empty query is `*`). The results replace the schema in the details pane while that
//!   index is selected, and `tab` moves between the indexes and the results.
//! - `enter` on a result opens its fields one level deeper, in the key detail's own
//!   [`ValueState`]: `↑↓`, `tab`, `f` and `y` work as there. `esc` goes back.
//! - `e` shows `FT.EXPLAIN` and `P` runs `FT.PROFILE`, one level deeper too, both for
//!   the query last run (the one `/` starts from), on the selected index.
//!
//! Writes (P3), through [`Act::write`], the one gate: `C` opens a form for an index
//! (name, `ON HASH` or `JSON`, prefixes, fields) and sends `FT.CREATE`; `D` drops the
//! selected index with `FT.DROPINDEX`, never with `DD`, so its documents stay. The list
//! is read again after each. See [`super`] for the interface `App` calls. Results
//! are held as [`Raw`](crate::value::Raw) and explain and profile text as [`Text`], so
//! the derived `Debug` never shows a value.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::streams::form::{self, Form, Step, write_failed, write_off};
use super::streams::{list_move, off_text, plain};
use super::value::{Drawn, ValueState};
use super::{Act, Input, ViewState};
use crate::live::keys::KeyRow;
use crate::live::search::{Doc, IndexInfo, IndexList, Reply, Request, Results, Text};
use crate::live::values::{Field, Items};
use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{KeyType, Ttl};
use crate::value::escape;
use crate::write::{IndexField, IndexOn, WriteOp};

/// Rows of the schema list in the details panel that fit at once.
const SCHEMA_PAGE: usize = 4;
/// Rows of the results list that `PgUp` and `PgDn` move by.
const RESULTS_PAGE: usize = 5;
/// The longest query the input takes, in characters.
pub const MAX_QUERY: usize = 1_000;

/// What `↑↓` moves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Indexes,
    /// The details pane: the schema of the selected index, or its results.
    Schema,
}

/// A query command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Search,
    Explain,
    Profile,
}

impl Cmd {
    pub fn name(self) -> &'static str {
        match self {
            Self::Search => "FT.SEARCH",
            Self::Explain => "FT.EXPLAIN",
            Self::Profile => "FT.PROFILE",
        }
    }

    /// The key that runs it.
    fn key(self) -> &'static str {
        match self {
            Self::Search => "/",
            Self::Explain => "e",
            Self::Profile => "P",
        }
    }
}

/// How far a query got.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome<T> {
    Running,
    Done(T),
    /// Why it failed, as a sentence: redis-lens's own words, or for a query the server
    /// could not run its message, escaped and cut (`live::search::query_error`).
    Failed(String),
}

/// The results of the query last run, on the index it ran on.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryRun {
    pub index: String,
    pub query: String,
    /// `FT.SEARCH`, or `FT.PROFILE`, whose results show here too.
    pub cmd: Cmd,
    pub outcome: Outcome<Results>,
}

impl QueryRun {
    /// The results, once they came.
    pub fn results(&self) -> Option<&Results> {
        match &self.outcome {
            Outcome::Done(r) => Some(r),
            _ => None,
        }
    }
}

/// One level below the indexes, over the whole body. `esc` goes back.
#[derive(Debug)]
pub enum Deeper {
    /// A result's fields, in the key detail's state.
    Document(Box<ValueState>),
    /// `FT.EXPLAIN` or `FT.PROFILE` (`cmd`) of `query` on `index`.
    Text {
        cmd: Cmd,
        index: String,
        query: String,
        text: Outcome<Text>,
    },
}

/// The indexes, the schema of the selected one, and its queries.
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
    /// A read went out since the view was last shown (or since `r`). A read that fails
    /// is not sent again on its own: the next show or `r` sends it.
    tried: bool,
    /// The query input, while it is open.
    pub input: Option<String>,
    /// The query last run: what `/` starts from, and what `e` and `P` use.
    pub last_query: Option<String>,
    /// The results of the query last run (`FT.SEARCH` or `FT.PROFILE`).
    pub results: Option<QueryRun>,
    /// Selected row of the results.
    pub result_sel: usize,
    /// What is open one level deeper, if anything.
    pub deeper: Option<Deeper>,
    /// The first line of an explain or profile text shown.
    pub text_scroll: usize,
    /// Set by the draw, so the scroll keys know how far the text goes.
    pub text_drawn: Drawn,
    /// The query command in flight, if any: one at a time.
    query_pending: Option<(ReqId, Cmd)>,
    /// Query commands the server refused or lacks, with why: not sent again until `r`.
    pub cmd_off: Vec<(Cmd, String)>,
    /// The results' previews, kept between frames. `Debug` shows only counts.
    pub memo: crate::value::Memo,
    /// The create form (`C`), while it is open.
    pub create: Option<Form>,
    /// The write in flight: its id, the index, and whether it creates (or drops).
    write: Option<(ReqId, String, bool)>,
}

/// Why `C` and `D` are off when the server has no Search module.
pub const NO_MODULE: &str =
    "Not available: the server has no Search module, so indexes can't be created or dropped.";

/// The field types the create form takes.
pub const FIELD_TYPES: [&str; 4] = ["TEXT", "TAG", "NUMERIC", "GEO"];

/// The `FT.CREATE` of the create form. Prefixes are separated by spaces or commas;
/// fields by commas (or newlines), each `name TYPE` or `path AS alias TYPE`.
pub fn create_op(f: &Form) -> Result<WriteOp, String> {
    let index = f.value("name");
    if index.is_empty() {
        return Err("Type the index's name.".into());
    }
    if index.contains(char::is_whitespace) {
        return Err("An index name has no spaces.".into());
    }
    let on = if f.value("on") == "JSON" {
        IndexOn::Json
    } else {
        IndexOn::Hash
    };
    let prefixes: Vec<String> = f
        .value("prefixes")
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    let mut fields = Vec::new();
    for entry in f
        .value("fields")
        .split([',', '\n'])
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let words: Vec<&str> = entry.split_whitespace().collect();
        let (name, alias, kind) = match words[..] {
            [name, kind] => (name, None, kind),
            [name, as_, alias, kind] if as_.eq_ignore_ascii_case("AS") => (name, Some(alias), kind),
            _ => {
                return Err(format!(
                    "Write each field as name TYPE, or $.path AS alias TYPE: {}.",
                    escape(entry)
                ));
            }
        };
        let kind = kind.to_ascii_uppercase();
        if !FIELD_TYPES.contains(&kind.as_str()) {
            return Err(format!(
                "A field's type is TEXT, TAG, NUMERIC or GEO, not {}.",
                escape(&kind)
            ));
        }
        if on == IndexOn::Json && !name.starts_with('$') {
            return Err(format!(
                "On JSON a field is a path such as $.title: {}.",
                escape(name)
            ));
        }
        fields.push(IndexField {
            name: name.to_string(),
            alias: alias.map(str::to_string),
            kind,
        });
    }
    if fields.is_empty() {
        return Err("Add at least one field, such as title TEXT.".into());
    }
    Ok(WriteOp::FtCreate {
        index: index.to_string(),
        on,
        prefixes,
        fields,
    })
}

impl SearchState {
    /// The `FT.CREATE` the form sends as filled now, as the prompt shows it, or what is
    /// wrong: for the form's "Sends:" line.
    pub fn form_sends(&self) -> Option<Result<String, String>> {
        self.create
            .as_ref()
            .map(|f| create_op(f).map(|op| op.shown()))
    }

    /// `C`: opens the create form.
    fn open_create(&mut self, act: &mut Act<'_>) {
        if self.no_module {
            act.error(NO_MODULE);
            return;
        }
        let sample = WriteOp::FtCreate {
            index: "i".into(),
            on: IndexOn::Hash,
            prefixes: Vec::new(),
            fields: vec![IndexField {
                name: "f".into(),
                alias: None,
                kind: "TEXT".into(),
            }],
        };
        if write_off(act, &sample) {
            return;
        }
        self.create = Some(Form::new(
            "Create an index",
            vec!["FT.CREATE indexes the keys with these prefixes, now and as they change.".into()],
            vec![
                form::Field::text("name", "the index's name, such as idx:products"),
                form::Field::choice("on", &["HASH", "JSON"], "hashes, or JSON documents"),
                form::Field::text(
                    "prefixes",
                    "key prefixes, separated by spaces or commas; empty: every key",
                )
                .as_list(),
                form::Field::text(
                    "fields",
                    "name TYPE, … (TEXT, TAG, NUMERIC, GEO); on JSON: $.path AS alias TYPE",
                )
                .as_list(),
            ],
        ));
    }

    /// A key while the create form is open.
    fn create_key(&mut self, key: KeyEvent, act: &mut Act<'_>) {
        let Some(f) = self.create.as_mut() else {
            return;
        };
        match f.key(key) {
            Step::Typed => {}
            Step::Cancel => self.create = None,
            Step::Submit => match create_op(f) {
                Err(why) => act.error(why),
                Ok(op) => {
                    self.create = None;
                    let index = op.name_to_type();
                    if let Some(id) = act.write(op) {
                        self.write = Some((id, index, true));
                    }
                }
            },
        }
    }

    /// `D`: drops the selected index; its documents stay.
    fn drop_index(&mut self, act: &mut Act<'_>) {
        if self.no_module {
            act.error(NO_MODULE);
            return;
        }
        let Some(index) = self.selected().map(|i| i.name.clone()) else {
            act.info("No index selected.");
            return;
        };
        let op = WriteOp::FtDropIndex {
            index: index.clone(),
        };
        if let Some(id) = act.write(op) {
            self.write = Some((id, index, false));
        }
    }

    /// The answer to the write in flight; the list is read again after it.
    fn write_reply(
        &mut self,
        index: String,
        create: bool,
        result: Result<LiveReply, LiveError>,
        act: &mut Act<'_>,
    ) {
        match result {
            Ok(_) if create => act.info(format!("Created index {}.", escape(&index))),
            Ok(_) => act.info(format!(
                "Dropped index {}. Its documents stay.",
                escape(&index)
            )),
            Err(e) => {
                write_failed(act, &e);
                return;
            }
        }
        if !self.in_flight(act) {
            self.fetch(act);
        }
    }

    /// The selected index.
    pub fn selected(&self) -> Option<&IndexInfo> {
        self.list.indexes.get(self.sel)
    }

    /// The query results of the selected index, when the query last run was on it.
    pub fn shown_results(&self) -> Option<&QueryRun> {
        let name = &self.selected()?.name;
        self.results.as_ref().filter(|r| &r.index == name)
    }

    /// Why `cmd` is off, if the server refused it or lacks it.
    pub fn off_reason(&self, cmd: Cmd) -> Option<&str> {
        self.cmd_off
            .iter()
            .find(|(c, _)| *c == cmd)
            .map(|(_, why)| why.as_str())
    }

    /// True while a query command is in flight.
    pub fn is_running(&self) -> bool {
        self.query_pending.is_some()
    }

    fn fetch(&mut self, act: &mut Act<'_>) {
        self.pending = act.send(LiveRequest::Search(Request::Indexes));
        self.tried |= self.pending.is_some();
        if self.pending.is_some() && !self.loaded {
            // Nothing read yet: the screen says it is reading, not the last failure.
            self.error = None;
        }
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

    /// Sends `cmd` with `query` on the selected index. False, with a notice, when it
    /// can't be sent.
    fn send_query(&mut self, cmd: Cmd, query: String, act: &mut Act<'_>) -> bool {
        // One query at a time: a second one would make the first one's answer stale and
        // leave its pane "Running" for good.
        if let Some((_, running)) = self.query_pending {
            act.info(format!(
                "{} is still running: wait for its answer.",
                running.name()
            ));
            return false;
        }
        let Some(index) = self.selected().map(|i| i.name.clone()) else {
            act.info("No index selected.");
            return false;
        };
        if let Some(why) = self.off_reason(cmd) {
            act.info(format!("{why} Press r to try again."));
            return false;
        }
        let req = match cmd {
            Cmd::Search => Request::Search {
                index: index.clone(),
                query: query.clone(),
            },
            Cmd::Explain => Request::Explain {
                index: index.clone(),
                query: query.clone(),
            },
            Cmd::Profile => Request::Profile {
                index: index.clone(),
                query: query.clone(),
            },
        };
        let Some(id) = act.send(LiveRequest::Search(req)) else {
            act.info("The live connection is not open, so the query can't run.");
            return false;
        };
        self.query_pending = Some((id, cmd));
        match cmd {
            Cmd::Search | Cmd::Profile => {
                self.results = Some(QueryRun {
                    index: index.clone(),
                    query: query.clone(),
                    cmd,
                    outcome: Outcome::Running,
                });
                self.result_sel = 0;
                self.focus = Focus::Schema;
            }
            Cmd::Explain => {}
        }
        self.deeper = match cmd {
            Cmd::Search => None,
            Cmd::Explain | Cmd::Profile => Some(Deeper::Text {
                cmd,
                index,
                query,
                text: Outcome::Running,
            }),
        };
        self.text_scroll = 0;
        true
    }

    /// `e` or `P`: the query last run, on the selected index.
    fn run_on_last(&mut self, cmd: Cmd, act: &mut Act<'_>) {
        match self.last_query.clone() {
            Some(q) => {
                self.send_query(cmd, q, act);
            }
            None => act.info(format!(
                "Run a query with / first: {} uses the query last run.",
                cmd.key()
            )),
        }
    }

    /// `enter` on the results: open the selected one's fields.
    fn open_result(&mut self, act: &mut Act<'_>) {
        let Some(run) = self.shown_results() else {
            act.info("Run a query with / first, then enter opens a result.");
            return;
        };
        let Some(results) = run.results() else {
            act.info("The query has no results to open.");
            return;
        };
        let Some(doc) = results
            .docs
            .get(self.result_sel.min(results.docs.len().saturating_sub(1)))
        else {
            act.info("No documents match the query.");
            return;
        };
        let state = document(doc, act.tick);
        self.deeper = Some(Deeper::Document(Box::new(state)));
    }

    /// A key while the query input is open.
    fn input_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        let Some(text) = self.input.as_mut() else {
            return false;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => {
                let typed = std::mem::take(text);
                self.input = None;
                // An empty query matches every document.
                let query = if typed.trim().is_empty() {
                    "*".to_string()
                } else {
                    typed
                };
                self.last_query = Some(query.clone());
                self.send_query(Cmd::Search, query, act);
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if ctrl => text.clear(),
            KeyCode::Char(c) if plain(&key) && text.chars().count() < MAX_QUERY => text.push(c),
            _ => {}
        }
        true
    }

    /// The move keys on an explain or profile text: scroll it.
    fn scroll_key(&mut self, key: &KeyEvent) -> bool {
        if !plain(key) {
            return false;
        }
        let max = self.text_drawn.max_scroll();
        let page = self.text_drawn.height();
        let at = self.text_scroll.min(max);
        self.text_scroll = match key.code {
            KeyCode::Up | KeyCode::Char('k') => at.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (at + 1).min(max),
            KeyCode::PageUp => at.saturating_sub(page),
            KeyCode::PageDown => (at + page).min(max),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => max,
            _ => return false,
        };
        true
    }

    /// The answer to the query command in flight.
    fn query_reply(&mut self, cmd: Cmd, result: Result<LiveReply, LiveError>) {
        match result {
            Ok(LiveReply::Search(Reply::Results(r))) => self.set_results(Outcome::Done(r)),
            Ok(LiveReply::Search(Reply::Explain(t))) => self.set_text(cmd, Outcome::Done(t)),
            Ok(LiveReply::Search(Reply::Profile { results, profile })) => {
                self.set_results(Outcome::Done(results));
                self.set_text(cmd, Outcome::Done(profile));
            }
            Ok(_) => {}
            Err(e) => {
                let why = failure_text(&e);
                if matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_))
                    && self.off_reason(cmd).is_none()
                {
                    self.cmd_off.push((cmd, why.clone()));
                }
                if cmd != Cmd::Explain {
                    self.set_results(Outcome::Failed(why.clone()));
                }
                self.set_text(cmd, Outcome::Failed(why));
            }
        }
    }

    fn set_results(&mut self, outcome: Outcome<Results>) {
        if let Some(run) = self.results.as_mut() {
            run.outcome = outcome;
        }
    }

    /// Fills the explain or profile text, if it is still open.
    fn set_text(&mut self, cmd: Cmd, outcome: Outcome<Text>) {
        if let Some(Deeper::Text { cmd: c, text, .. }) = self.deeper.as_mut()
            && *c == cmd
        {
            *text = outcome;
        }
    }
}

/// A result's fields in the key detail's state: a hash of what the module returned.
fn document(doc: &Doc, tick: u64) -> ValueState {
    let fields: Vec<Field> = doc
        .fields
        .iter()
        .map(|(name, value)| Field {
            name: name.clone(),
            value: value.clone(),
            ttl: None,
        })
        .collect();
    let row = KeyRow {
        key: doc.key.clone(),
        key_type: KeyType::Hash,
        ttl: Ttl::Persistent,
        bytes: None,
        encoding: None,
        length: Some(fields.len() as u64),
        freq: None,
    };
    ValueState::preloaded(row, Items::Hash(fields), tick)
}

/// Why a query command failed, as a sentence for the screen.
pub fn failure_text(e: &LiveError) -> String {
    match e {
        LiveError::Closed => "The live connection is closed.".to_string(),
        LiveError::Failed(why) => {
            let mut c = why.chars();
            let mut out: String = c
                .next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default();
            if !out.ends_with(['.', '!', '?', '…']) {
                out.push('.');
            }
            out
        }
        e => off_text(e),
    }
}

#[cfg(test)]
impl SearchState {
    /// [`ViewState::on_paste`] without an [`Act`], which it does not use.
    pub(crate) fn on_paste_for_test(&mut self, text: &str) {
        paste_into(self.input.as_mut(), text);
    }
}

/// Pasted text into the query input: one line, no control characters.
fn paste_into(input: Option<&mut String>, text: &str) {
    let Some(input) = input else {
        return;
    };
    for c in text.chars() {
        if input.chars().count() >= MAX_QUERY {
            break;
        }
        // A pasted newline would end the query: it becomes a space.
        match c {
            '\n' | '\r' | '\t' => input.push(' '),
            c if c.is_control() => {}
            c => input.push(c),
        }
    }
}

impl ViewState for SearchState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        // Read once; `r` reads again. A view that turned itself off waits for `r` too.
        // After a failed read, showing the view again tries once more.
        self.tried = false;
        if !self.loaded && self.off.is_none() && !self.no_module && !self.in_flight(act) {
            self.fetch(act);
        }
    }

    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        // A query that will never be answered (a rescan replaced the connection).
        if let Some((id, cmd)) = self.query_pending
            && !act.in_flight(id)
        {
            self.query_pending = None;
            self.query_reply(cmd, Err(LiveError::Closed));
        }
        // Not a polling view, but a read that was waiting for the live connection (the
        // view was shown before it opened) goes out as soon as it is ready. Once only:
        // a failed read waits for `r` or the next show.
        if !self.loaded
            && !self.tried
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
        // The ACL may have changed, or a module been loaded.
        self.cmd_off.clear();
        if !self.in_flight(act) {
            self.fetch(act);
        }
    }

    fn input(&self) -> Option<Input> {
        if let Some(f) = &self.create {
            return f.input();
        }
        self.input.as_ref().map(|text| Input {
            label: "/",
            text: text.clone(),
            hint: "   enter to run FT.SEARCH · esc to cancel",
        })
    }

    fn on_paste(&mut self, text: &str, _act: &mut Act<'_>) {
        if let Some(f) = self.create.as_mut() {
            f.paste(text);
            return;
        }
        paste_into(self.input.as_mut(), text);
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if self.create.is_some() {
            self.create_key(key, act);
            return true;
        }
        if self.input.is_some() {
            return self.input_key(key, act);
        }
        // Keys that work the same one level deeper: they go back first.
        match (key.code, plain(&key)) {
            (KeyCode::Char('C'), true) => {
                self.deeper = None;
                self.open_create(act);
                return true;
            }
            (KeyCode::Char('D'), true) => {
                self.deeper = None;
                self.drop_index(act);
                return true;
            }
            (KeyCode::Char('/'), true) => {
                if self.selected().is_none() {
                    act.info("No index selected.");
                } else {
                    self.deeper = None;
                    self.input = Some(self.last_query.clone().unwrap_or_default());
                }
                return true;
            }
            (KeyCode::Char('e'), true) => {
                self.run_on_last(Cmd::Explain, act);
                return true;
            }
            (KeyCode::Char('P'), true) => {
                self.run_on_last(Cmd::Profile, act);
                return true;
            }
            (KeyCode::Esc, _) if self.deeper.is_some() => {
                self.deeper = None;
                return true;
            }
            _ => {}
        }
        match self.deeper.as_mut() {
            Some(Deeper::Document(doc)) => {
                // Everything is loaded: `n` has nothing to do here.
                if matches!(key.code, KeyCode::Char('n')) {
                    return false;
                }
                return doc.on_key(key, act);
            }
            Some(Deeper::Text { .. }) => {
                if self.scroll_key(&key) {
                    return true;
                }
                if matches!(key.code, KeyCode::Char('f' | 'y')) && plain(&key) {
                    act.info("f and y work on a result's fields: press esc, then enter.");
                    return true;
                }
                return false;
            }
            None => {}
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Indexes => Focus::Schema,
                    Focus::Schema => Focus::Indexes,
                };
                return true;
            }
            KeyCode::Enter => {
                self.open_result(act);
                return true;
            }
            KeyCode::Char('f' | 'y') if plain(&key) => {
                act.info("Open a result with enter first: f and y work on its fields.");
                return true;
            }
            _ => {}
        }
        let schema_len = self.selected().map_or(0, |i| i.schema.len());
        let results_len = self
            .shown_results()
            .and_then(QueryRun::results)
            .map(|r| r.docs.len());
        let (cur, len, page) = match (self.focus, results_len) {
            (Focus::Indexes, _) => (self.sel, self.list.indexes.len(), act.page),
            (Focus::Schema, Some(n)) => (self.result_sel, n, RESULTS_PAGE),
            (Focus::Schema, None) => (self.schema_sel, schema_len, SCHEMA_PAGE),
        };
        let Some(to) = list_move(&key, cur, len, page) else {
            return false;
        };
        match (self.focus, results_len) {
            (Focus::Indexes, _) => {
                if to != self.sel {
                    self.schema_sel = 0;
                }
                self.sel = to;
            }
            (Focus::Schema, Some(_)) => self.result_sel = to,
            (Focus::Schema, None) => self.schema_sel = to,
        }
        true
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        if let Some((wid, _, _)) = &self.write
            && *wid == id
        {
            let (_, index, create) = self.write.take().expect("the write in flight");
            self.write_reply(index, create, result, act);
            return;
        }
        if let Some((qid, cmd)) = self.query_pending
            && qid == id
        {
            self.query_pending = None;
            self.query_reply(cmd, result);
            return;
        }
        // Only the answer to the request in flight; an older one is stale.
        if self.pending != Some(id) {
            return;
        }
        self.pending = None;
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
    fn a_failed_first_read_is_not_sent_again_on_ticks() {
        let (mut app, id) = shown();
        answer(
            &mut app,
            id,
            Err(LiveError::Failed("FT._LIST failed".into())),
        );
        let t0 = Instant::now();
        for i in 0..20 {
            let effects = app.apply(Msg::TickAt(t0 + std::time::Duration::from_millis(100 * i)));
            assert!(testing::requests(&effects).is_empty(), "tick {i}");
        }
        // The screen says why and how to try again, not that it is still reading.
        let screen = testing::render(&app, 80, 24).backend().to_string();
        assert!(screen.contains("FT._LIST failed"), "{screen}");
        assert!(screen.contains("Press r to read again"), "{screen}");
        assert!(!screen.contains("Reading the indexes"), "{screen}");
        // `r` reads again, and the screen says so.
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(testing::requests(&effects).len(), 1);
        let screen = testing::render(&app, 80, 24).backend().to_string();
        assert!(screen.contains("Reading the indexes"), "{screen}");
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
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.views.search.focus, Focus::Schema);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.views.search.schema_sel, 5);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.views.search.schema_sel, 5, "clamped");
        app.handle_key(key(KeyCode::Left));
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
        app.handle_key(key(KeyCode::Right));
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

    // ---------------------------------------------------------------------------------
    // Queries (phase 2)
    // ---------------------------------------------------------------------------------

    use crate::live::search::{self as live_search, Doc};
    use crate::tui::app::Effect;
    use crate::tui::clipboard::CopyText;
    use crate::value::Raw;
    use crossterm::event::KeyModifiers;
    use std::time::Duration;

    const MARK: &str = "s3cr3t-d0c";

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(ch(c));
        }
    }

    fn notice(app: &App) -> String {
        app.notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    fn text(app: &App, w: u16, h: u16) -> String {
        let t = testing::render(app, w, h);
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

    fn search_requests(effects: &[Effect]) -> Vec<(ReqId, Request)> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(id, r)| match r {
                LiveRequest::Search(s) => Some((id, s)),
                _ => None,
            })
            .collect()
    }

    fn one(effects: &[Effect]) -> (ReqId, Request) {
        let reqs = search_requests(effects);
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        reqs.into_iter().next().unwrap()
    }

    fn doc(key: &str, fields: &[(&str, &str)]) -> Doc {
        Doc {
            key: key.into(),
            fields: fields
                .iter()
                .map(|(k, v)| (Raw::from(*k), Raw::from(*v)))
                .collect(),
        }
    }

    fn results() -> Results {
        Results {
            total: 1_204,
            docs: vec![
                doc(
                    "product:1",
                    &[("title", "red shoe"), ("price", "10"), ("tags", "a,b")],
                ),
                doc(
                    "product:2",
                    &[
                        ("title", "blue shoe"),
                        ("price", "20"),
                        ("spec", r#"{"size":42,"colors":["blue","white"]}"#),
                    ],
                ),
                doc("product:3", &[("title", "two\nlines")]),
            ],
            took: Duration::from_millis(3),
        }
    }

    /// Loaded, `/`, `query` typed and run; the request it sent.
    fn queried(query: &str) -> (App, ReqId, Request) {
        let mut app = loaded(true);
        app.handle_key(ch('/'));
        type_text(&mut app, query);
        let effects = app.handle_key(key(KeyCode::Enter));
        let (id, req) = one(&effects);
        (app, id, req)
    }

    /// [`queried`] with [`results`] answered.
    fn with_results() -> App {
        let (mut app, id, _) = queried("@price:[0 100]");
        testing::reply(
            &mut app,
            id,
            Ok(LiveReply::Search(Reply::Results(results()))),
        );
        app
    }

    #[test]
    fn slash_opens_the_input_and_enter_runs_ft_search_as_typed() {
        let mut app = loaded(true);
        app.handle_key(ch('/'));
        let input = app.view_input().expect("the query input");
        assert_eq!((input.label, input.text.as_str()), ("/", ""));
        // Every key is text while it is open: digits, q, e and P included.
        type_text(&mut app, "@title:(q1 eP) ");
        assert_eq!(app.screen(), Screen::Search);
        assert_eq!(app.view_input().unwrap().text, "@title:(q1 eP) ");
        let (id, req) = one(&app.handle_key(key(KeyCode::Enter)));
        assert_eq!(
            req,
            Request::Search {
                index: "idx:products".into(),
                query: "@title:(q1 eP) ".into(),
            }
        );
        assert!(app.view_input().is_none());
        testing::reply(
            &mut app,
            id,
            Ok(LiveReply::Search(Reply::Results(results()))),
        );
        // An empty query matches everything; `/` starts from the query last run.
        app.handle_key(ch('/'));
        assert_eq!(app.view_input().unwrap().text, "@title:(q1 eP) ");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let (_, req) = one(&app.handle_key(key(KeyCode::Enter)));
        assert_eq!(
            req,
            Request::Search {
                index: "idx:products".into(),
                query: "*".into(),
            }
        );
        // esc cancels without sending.
        app.handle_key(ch('/'));
        type_text(&mut app, "x");
        assert!(search_requests(&app.handle_key(key(KeyCode::Esc))).is_empty());
        assert!(app.view_input().is_none());
    }

    #[test]
    fn a_pasted_query_is_one_line() {
        let mut app = loaded(true);
        app.handle_key(ch('/'));
        app.views.search.on_paste_for_test("a\nb\x1bc");
        assert_eq!(app.view_input().unwrap().text, "a bc");
    }

    #[test]
    fn results_show_under_their_index() {
        let mut app = with_results();
        let s = &app.views.search;
        assert_eq!(s.focus, Focus::Schema, "the results get the focus");
        assert_eq!(s.shown_results().unwrap().results().unwrap().docs.len(), 3);
        let all = text(&app, 80, 24);
        assert!(all.contains("3 of 1,204 results · 3 ms"), "{all}");
        assert!(all.contains("title=red shoe price=10"), "{all}");
        insta::assert_snapshot!("results_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("results_120x36", testing::render(&app, 120, 36).backend());
        // The move keys move the results; another index shows its schema.
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.views.search.result_sel, 2);
        app.handle_key(key(KeyCode::Left));
        app.handle_key(ch('j'));
        assert!(app.views.search.shown_results().is_none());
        assert!(text(&app, 80, 24).contains("FIELD"));
        app.handle_key(ch('k'));
        assert!(app.views.search.shown_results().is_some());
    }

    #[test]
    fn enter_opens_a_result_and_f_y_esc_work_on_its_fields() {
        let mut app = with_results();
        app.handle_key(ch('j'));
        app.handle_key(key(KeyCode::Enter));
        let Some(Deeper::Document(d)) = &app.views.search.deeper else {
            panic!("a document");
        };
        assert_eq!(d.row.as_ref().unwrap().key, "product:2");
        assert_eq!(d.loaded(), 3);
        insta::assert_snapshot!("document_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("document_120x36", testing::render(&app, 120, 36).backend());
        // y copies the selected field's value as shown.
        let effects = app.handle_key(ch('y'));
        let copied: Vec<&CopyText> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Copy(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].as_str(), "blue shoe");
        // The JSON field, as hex after f.
        app.handle_key(key(KeyCode::End));
        app.handle_key(ch('f'));
        let Some(Deeper::Document(d)) = &app.views.search.deeper else {
            panic!("a document");
        };
        assert_eq!(d.format, Some(crate::value::Format::MsgPack));
        assert!(text(&app, 120, 36).contains("shown as MessagePack"));
        // n has nothing to load here.
        let before = format!("{:?}", app.views.search);
        app.handle_key(ch('n'));
        assert_eq!(format!("{:?}", app.views.search), before);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.views.search.deeper.is_none());
        assert!(app.views.search.shown_results().is_some());
        // With no query run, enter says so.
        let mut fresh = loaded(true);
        fresh.handle_key(key(KeyCode::Enter));
        assert_eq!(
            notice(&fresh),
            "Run a query with / first, then enter opens a result."
        );
        fresh.handle_key(ch('y'));
        assert_eq!(
            notice(&fresh),
            "Open a result with enter first: f and y work on its fields."
        );
    }

    #[test]
    fn e_explains_the_query_last_run() {
        let mut app = loaded(true);
        app.handle_key(ch('e'));
        assert_eq!(
            notice(&app),
            "Run a query with / first: e uses the query last run."
        );
        let mut app = with_results();
        let (id, req) = one(&app.handle_key(ch('e')));
        assert_eq!(
            req,
            Request::Explain {
                index: "idx:products".into(),
                query: "@price:[0 100]".into(),
            }
        );
        insta::assert_snapshot!(
            "explain_waiting_80x24",
            testing::render(&app, 80, 24).backend()
        );
        let plan = "INTERSECT {\n  NUMERIC {0.000000 <= @price <= 100.000000}\n  UNION {\n    shoe\n    +shoe(expanded)\n  }\n}\n";
        let t = live_search::parse_explain(&redis::Value::BulkString(plan.as_bytes().to_vec()));
        testing::reply(&mut app, id, Ok(LiveReply::Search(Reply::Explain(t))));
        insta::assert_snapshot!("explain_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("explain_120x36", testing::render(&app, 120, 36).backend());
        app.handle_key(ch('j'));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.views.search.deeper.is_none());
    }

    /// One query at a time: `e` or `P` while one runs says so and sends nothing, and the
    /// running query's answer is still shown.
    #[test]
    fn a_second_query_waits_for_the_first() {
        let mut app = with_results();
        let (id, _) = one(&app.handle_key(ch('e')));
        for k in ['e', 'P'] {
            let effects = app.handle_key(ch(k));
            assert!(testing::requests(&effects).is_empty(), "{k}: {effects:?}");
            assert_eq!(
                notice(&app),
                "FT.EXPLAIN is still running: wait for its answer."
            );
        }
        let t = live_search::parse_explain(&redis::Value::BulkString(b"UNION {\n}\n".to_vec()));
        testing::reply(&mut app, id, Ok(LiveReply::Search(Reply::Explain(t))));
        assert!(!app.views.search.is_running());
        let (_, req) = one(&app.handle_key(ch('P')));
        assert!(matches!(req, Request::Profile { .. }), "{req:?}");
    }

    fn profile_value() -> redis::Value {
        use redis::Value::{Array, BulkString as B, Int, SimpleString as S};
        let b = |s: &str| B(s.as_bytes().to_vec());
        Array(vec![
            Array(vec![
                Int(1),
                b("product:1"),
                Array(vec![b("title"), b("red shoe"), b("price"), b("10")]),
            ]),
            Array(vec![
                S("Shards".into()),
                Array(vec![Array(vec![
                    S("Total profile time".into()),
                    b("0.19"),
                    S("Parsing time".into()),
                    b("0.08"),
                    S("Warning".into()),
                    Array(vec![S("None".into())]),
                    S("Iterators profile".into()),
                    Array(vec![
                        S("Type".into()),
                        S("NUMERIC".into()),
                        S("Term".into()),
                        S("10 - 20".into()),
                        S("Number of reading operations".into()),
                        Int(1),
                    ]),
                    S("Result processors profile".into()),
                    Array(vec![
                        Array(vec![
                            S("Type".into()),
                            S("Index".into()),
                            S("Time".into()),
                            b("0.002"),
                        ]),
                        Array(vec![
                            S("Type".into()),
                            S("Sorter".into()),
                            S("Time".into()),
                            b("0.001"),
                        ]),
                    ]),
                ])]),
                S("Coordinator".into()),
                Array(vec![]),
            ]),
        ])
    }

    #[test]
    fn p_profiles_the_query_and_shows_its_results_too() {
        let mut app = with_results();
        let (id, req) = one(&app.handle_key(ch('P')));
        assert_eq!(
            req,
            Request::Profile {
                index: "idx:products".into(),
                query: "@price:[0 100]".into(),
            }
        );
        let (mut results, profile) = live_search::parse_profile(&profile_value()).unwrap();
        results.took = Duration::from_millis(1);
        testing::reply(
            &mut app,
            id,
            Ok(LiveReply::Search(Reply::Profile { results, profile })),
        );
        insta::assert_snapshot!("profile_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("profile_120x36", testing::render(&app, 120, 36).backend());
        let all = text(&app, 80, 24);
        assert!(all.contains("Total profile time: 0.19"), "{all}");
        assert!(all.contains("- Type: Index"), "{all}");
        // Back on the indexes, the profile's results are shown.
        app.handle_key(key(KeyCode::Esc));
        let run = app.views.search.shown_results().unwrap();
        assert_eq!(run.cmd, Cmd::Profile);
        assert_eq!(run.results().unwrap().docs.len(), 1);
        insta::assert_snapshot!(
            "profile_results_80x24",
            testing::render(&app, 80, 24).backend()
        );
    }

    #[test]
    fn a_module_without_explain_or_profile_says_so_and_is_not_asked_again() {
        let mut app = with_results();
        app.session
            .as_mut()
            .unwrap()
            .server
            .as_mut()
            .unwrap()
            .flavor = ServerFlavor::Valkey;
        for (k, cmd) in [('e', "FT.EXPLAIN"), ('P', "FT.PROFILE")] {
            let (id, _) = one(&app.handle_key(ch(k)));
            testing::reply(&mut app, id, Err(LiveError::Unsupported(cmd.into())));
            let all = text(&app, 80, 24);
            let want = format!("The server does not support {cmd}.");
            assert!(all.contains(&want), "{all}");
            // Not sent again until r; the notice says why.
            assert!(search_requests(&app.handle_key(ch(k))).is_empty());
            assert_eq!(notice(&app), format!("{want} Press r to try again."));
        }
        app.handle_key(key(KeyCode::Esc));
        // The profile failed, so the results say so; a new search brings them back.
        let (id, _) = one(&app
            .handle_key(ch('/'))
            .into_iter()
            .chain(app.handle_key(key(KeyCode::Enter)))
            .collect::<Vec<_>>());
        testing::reply(
            &mut app,
            id,
            Ok(LiveReply::Search(Reply::Results(results()))),
        );
        let all = text(&app, 80, 24);
        assert!(all.contains("/ query · enter open"), "{all}");
        assert!(
            !all.contains("e explain") && !all.contains("P profile"),
            "{all}"
        );
        insta::assert_snapshot!(
            "valkey_no_explain_80x24",
            testing::render(&app, 80, 24).backend()
        );
        // r asks again.
        app.handle_key(ch('r'));
        assert!(app.views.search.cmd_off.is_empty());
        assert_eq!(search_requests(&app.handle_key(ch('e'))).len(), 1);
    }

    #[test]
    fn a_refused_query_says_so_in_fixed_words() {
        let (mut app, id, _) = queried("*");
        testing::reply(&mut app, id, Err(LiveError::Denied("FT.SEARCH".into())));
        let all = text(&app, 80, 24);
        assert!(
            all.contains("The server refused FT.SEARCH for this user."),
            "{all}"
        );
        assert!(!all.contains("NOPERM"));
        insta::assert_snapshot!("refused_80x24", testing::render(&app, 80, 24).backend());
        app.handle_key(ch('/'));
        assert!(search_requests(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert_eq!(
            notice(&app),
            "The server refused FT.SEARCH for this user. Press r to try again."
        );
    }

    #[test]
    fn a_syntax_error_shows_the_servers_message_escaped_and_cut() {
        let (mut app, id, _) = queried("@price:[");
        let long = format!(
            "Syntax error at offset 7 near \x1b[2Jprice{}",
            "x".repeat(400)
        );
        let wire = format!("-SEARCH_SYNTAX {long}\r\n");
        let redis::Value::ServerError(e) = redis::parse_redis_value(wire.as_bytes()).unwrap()
        else {
            panic!("a server error");
        };
        let err = live_search::query_error("FT.SEARCH", &redis::RedisError::from(e));
        let LiveError::Failed(why) = &err else {
            panic!("{err:?}");
        };
        assert!(
            why.starts_with("FT.SEARCH failed: Syntax error at offset 7 near \\x1b[2Jprice"),
            "{why}"
        );
        let msg = why.trim_start_matches("FT.SEARCH failed: ");
        assert_eq!(msg.chars().count(), live_search::QUERY_ERROR_CHARS);
        assert!(msg.ends_with('…'));
        testing::reply(&mut app, id, Err(err));
        for (w, h) in [(80, 24), (120, 36)] {
            let all = text(&app, w, h);
            assert!(all.contains("FT.SEARCH failed: Syntax error"), "{all}");
            assert!(!all.contains('\x1b'), "{all}");
        }
        insta::assert_snapshot!(
            "syntax_error_80x24",
            testing::render(&app, 80, 24).backend()
        );
    }

    #[test]
    fn escapes_in_results_are_drawn_never_the_raw_bytes() {
        let (mut app, id, _) = queried("*");
        let r = Results {
            total: 1,
            docs: vec![Doc {
                key: "k\x1b[2Jey".into(),
                fields: vec![
                    (
                        Raw::from("na\x1b[2Jme"),
                        Raw::from("ok\x1b[2J\u{9b}31m done"),
                    ),
                    (
                        Raw::from("bin"),
                        Raw::new(vec![0x9b, 0x1b, b'[', b'2', b'J', 0xff]),
                    ),
                ],
            }],
            took: Duration::ZERO,
        };
        testing::reply(&mut app, id, Ok(LiveReply::Search(Reply::Results(r))));
        let check = |app: &App| {
            for (w, h) in [(80, 24), (120, 36)] {
                let all = text(app, w, h);
                assert!(all.contains("\\x1b[2J"), "{all}");
                assert!(!all.contains('\x1b') && !all.contains('\u{9b}'), "{all}");
            }
        };
        check(&app);
        app.handle_key(key(KeyCode::Enter));
        check(&app);
        insta::assert_snapshot!(
            "document_escapes_80x24",
            testing::render(&app, 80, 24).backend()
        );
    }

    /// Rule 1: results, documents and profile text show on screen and nowhere else.
    #[test]
    fn results_never_reach_debug_notices_or_snapshot_files() {
        let (mut app, id, _) = queried("*");
        let r = Results {
            total: 1,
            docs: vec![doc(
                "product:9",
                &[(MARK, MARK), ("$", &format!("{{\"k\":\"{MARK}\"}}"))],
            )],
            took: Duration::ZERO,
        };
        let reply = Ok(LiveReply::Search(Reply::Results(r)));
        let msg = format!(
            "{:?}",
            Msg::Live {
                id: app.session.as_ref().unwrap().id,
                scan: app.session.as_ref().unwrap().scan,
                req_id: id,
                result: reply.clone(),
            }
        );
        assert!(!crate::value::leaks(&msg, MARK), "{msg}");
        testing::reply(&mut app, id, reply);
        assert!(text(&app, 120, 36).contains(MARK));
        let mut effects = app.handle_key(key(KeyCode::Enter));
        for k in ['y', 'j', 'f', 'y'] {
            effects.extend(app.handle_key(ch(k)));
            assert!(
                !crate::value::leaks(&notice(&app), MARK),
                "{}",
                notice(&app)
            );
        }
        assert!(effects.iter().any(|e| matches!(e, Effect::Copy(_))));
        // A profile that echoes the data.
        let (pid, _) = one(&app.handle_key(ch('P')));
        let profile = live_search::Text::new([format!("Term: {MARK}")]);
        let reply = Ok(LiveReply::Search(Reply::Profile {
            results: Results::default(),
            profile,
        }));
        assert!(!crate::value::leaks(&format!("{reply:?}"), MARK));
        testing::reply(&mut app, pid, reply);
        assert!(text(&app, 120, 36).contains(MARK));
        let debug = format!("{effects:?} {:?} {:?}", app.views, app.session);
        assert!(!crate::value::leaks(&debug, MARK), "{debug}");
        app.handle_key(ch('3'));
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(report)] = &effects[..] else {
            panic!("{effects:?}");
        };
        assert!(!crate::value::leaks(
            &serde_json::to_string(report).unwrap(),
            MARK
        ));
    }

    // -----------------------------------------------------------------------
    // Create and drop (P3)
    // -----------------------------------------------------------------------

    use super::super::streams::form::testing::{
        READ_ONLY, answer_anything, press, set_profile, writes,
    };
    use crate::live::write::Reply as WriteReply;
    use crate::model::Env;
    use crate::tui::app::confirm::Expect;

    fn create_form(fields: &[(&str, &str)]) -> Form {
        let mut f = Form::new(
            "t",
            vec![],
            vec![
                form::Field::text("name", ""),
                form::Field::choice("on", &["HASH", "JSON"], ""),
                form::Field::text("prefixes", ""),
                form::Field::text("fields", ""),
            ],
        );
        for (label, text) in fields {
            f.fields
                .iter_mut()
                .find(|x| x.label == *label)
                .unwrap()
                .text = (*text).into();
        }
        f
    }

    #[test]
    fn the_create_form_builds_ft_create() {
        let f = create_form(&[
            ("name", "idx:products"),
            ("prefixes", "product:, item:"),
            ("fields", "title TEXT, price numeric,\ntags TAG"),
        ]);
        let op = create_op(&f).unwrap();
        assert_eq!(
            op.shown(),
            "FT.CREATE idx:products ON HASH PREFIX 2 product: item: SCHEMA title TEXT price NUMERIC tags TAG"
        );
        let f = create_form(&[
            ("name", "idx:sessions"),
            ("on", "JSON"),
            ("fields", "$.user.id AS user TAG"),
        ]);
        assert_eq!(
            create_op(&f).unwrap().shown(),
            "FT.CREATE idx:sessions ON JSON SCHEMA $.user.id AS user TAG"
        );
        for (fields, why) in [
            (vec![("fields", "title TEXT")], "Type the index's name."),
            (
                vec![("name", "a b"), ("fields", "title TEXT")],
                "An index name has no spaces.",
            ),
            (
                vec![("name", "i")],
                "Add at least one field, such as title TEXT.",
            ),
            (
                vec![("name", "i"), ("fields", "title VECTOR")],
                "A field's type is TEXT, TAG, NUMERIC or GEO, not VECTOR.",
            ),
            (
                vec![("name", "i"), ("fields", "title")],
                "Write each field as name TYPE, or $.path AS alias TYPE: title.",
            ),
            (
                vec![("name", "i"), ("on", "JSON"), ("fields", "title TEXT")],
                "On JSON a field is a path such as $.title: title.",
            ),
        ] {
            assert_eq!(create_op(&create_form(&fields)), Err(why.to_string()));
        }
    }

    #[test]
    fn c_creates_an_index_at_once_outside_prod_and_reads_the_list_again() {
        let mut app = loaded(true);
        set_profile(&mut app, Env::Local, None);
        app.handle_key(ch('C'));
        type_text(&mut app, "idx:orders");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "order:");
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "$.total AS total NUMERIC, $.status AS status TAG");
        insta::assert_snapshot!(
            "create_index_form_80x24",
            testing::render(&app, 80, 24).backend()
        );
        let e = press(&mut app, KeyCode::Enter);
        assert!(app.confirm.is_none());
        let w = writes(&e);
        assert_eq!(
            w.iter().map(WriteOp::shown).collect::<Vec<_>>(),
            vec![
                "FT.CREATE idx:orders ON JSON PREFIX 1 order: SCHEMA $.total AS total NUMERIC $.status AS status TAG"
            ]
        );
        let (id, _) = testing::requests(&e)[0].clone();
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Ok)));
        assert_eq!(notice(&app), "Created index idx:orders.");
        assert_eq!(
            search_requests(&e)
                .into_iter()
                .map(|r| r.1)
                .collect::<Vec<_>>(),
            vec![Request::Indexes]
        );
    }

    #[test]
    fn d_drops_the_selected_index_never_its_documents() {
        let mut app = loaded(true);
        set_profile(&mut app, Env::Local, None);
        app.handle_key(ch('j'));
        assert!(writes(&app.handle_key(ch('D'))).is_empty());
        let c = app.confirm.as_ref().expect("a question");
        assert_eq!(c.expect, Expect::YesNo);
        assert!(
            c.lines()
                .iter()
                .any(|l| l == "Sends: FT.DROPINDEX idx:sessions")
        );
        let e = app.handle_key(ch('y'));
        assert_eq!(
            writes(&e),
            vec![WriteOp::FtDropIndex {
                index: "idx:sessions".into()
            }]
        );
        let (id, _) = testing::requests(&e)[0].clone();
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Ok)));
        assert_eq!(
            notice(&app),
            "Dropped index idx:sessions. Its documents stay."
        );
        assert_eq!(search_requests(&e).len(), 1);

        // On prod: the index's name is typed; create asks y/n.
        set_profile(&mut app, Env::Prod, Some(false));
        app.handle_key(ch('D'));
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("idx:sessions".into())
        );
        press(&mut app, KeyCode::Esc);
        app.handle_key(ch('C'));
        type_text(&mut app, "i");
        press(&mut app, KeyCode::Up);
        type_text(&mut app, "t TAG");
        let e = press(&mut app, KeyCode::Enter);
        assert!(writes(&e).is_empty());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
    }

    #[test]
    fn without_the_module_create_and_drop_say_why() {
        let (mut app, id) = shown();
        answer(&mut app, id, Err(LiveError::Unsupported("FT._LIST".into())));
        set_profile(&mut app, Env::Local, None);
        for k in ['C', 'D'] {
            let e = app.handle_key(ch(k));
            assert!(writes(&e).is_empty() && app.confirm.is_none());
            assert!(app.views.search.create.is_none());
            assert_eq!(notice(&app), NO_MODULE);
        }
    }

    #[test]
    fn no_index_is_created_or_dropped_on_a_read_only_profile() {
        for (env, ro) in READ_ONLY {
            for k in ['C', 'D'] {
                let mut app = loaded(true);
                set_profile(&mut app, env, ro);
                let mut e = app.handle_key(ch(k));
                assert!(app.views.search.create.is_none() && app.confirm.is_none());
                assert!(notice(&app).contains("read-only"), "{}", notice(&app));
                e.extend(answer_anything(&mut app));
                assert!(writes(&e).is_empty(), "{k} {env:?} {ro:?}");
            }
        }
    }
}
