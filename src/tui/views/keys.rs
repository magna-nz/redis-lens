//! State of the key list: the keys of one pattern, opened with `enter` on a pattern on
//! Patterns (spec "1 Patterns, additions", "Key list"). Owner: wave B2. Drawn by
//! `ui/keys.rs`; its live requests are in `src/live/keys.rs`.
//!
//! The list loads [`PAGE_KEYS`](crate::live::keys::PAGE_KEYS) keys of the pattern when
//! it opens and as many more with each `m`. Only keys that infer to the pattern are
//! listed. `/` filters the loaded keys by name, `ctrl-t` by type, `s` sorts them, `y`
//! copies the selected key name and `Y` its pattern, `esc` clears the filter and then
//! goes back to Patterns. `enter` on a key is left to `App`, which opens the selected
//! key's detail ([`super::value`]), and so are `T` and `D`, the bulk actions of the
//! pattern. See [`super`] for the interface `App` calls.
//!
//! `+` (P3) adds a key from a form ([`Form::new_key`]) through the write gate; it never
//! overwrites one. Once added, `App` lists it and opens its detail ([`KeysState::opened`]).
//! `t`, `d` and `R` set the selected key's TTL, delete it and rename it: the writes `T`,
//! `D` and `R` send from its value, by name, without reading the value.
//!
//! The same list shows a find (`ctrl-f`, [`KeysState::open_find`]): the keys of the
//! whole server whose names match a glob, every key `SCAN` returns kept, with the same
//! pages, metadata and keys.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::value::{Form, FormKey, Nav};
use super::{Act, Ctx, Input, ViewState};
use crate::live::keys::{KeyRow, Page, Reply, Request};
use crate::live::write::Reply as WriteReply;
use crate::live::{CANCELLED, Dest, Follow, LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{KeyType, PERM_FREQ, PERM_MEMORY, PermStatus, Ttl};
use crate::tui::app::Screen;
use crate::tui::clipboard::CopyText;
use crate::write::{Decision, NewKey, WriteOp};

/// How the loaded keys are ordered. `s` cycles through the ones that have data.
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

/// The type `t` shows: all, or one of the types, with JSON and the unknown ones as
/// `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypeFilter {
    #[default]
    All,
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    Other,
}

impl TypeFilter {
    const ORDER: [TypeFilter; 8] = [
        Self::All,
        Self::String,
        Self::Hash,
        Self::List,
        Self::Set,
        Self::ZSet,
        Self::Stream,
        Self::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::String => "string",
            Self::Hash => "hash",
            Self::List => "list",
            Self::Set => "set",
            Self::ZSet => "zset",
            Self::Stream => "stream",
            Self::Other => "other",
        }
    }

    /// The next one in the cycle, `other` going back to `all`.
    pub fn next(self) -> Self {
        let at = Self::ORDER.iter().position(|t| *t == self).unwrap_or(0);
        Self::ORDER[(at + 1) % Self::ORDER.len()]
    }

    pub fn matches(self, t: KeyType) -> bool {
        match self {
            Self::All => true,
            Self::String => t == KeyType::String,
            Self::Hash => t == KeyType::Hash,
            Self::List => t == KeyType::List,
            Self::Set => t == KeyType::Set,
            Self::ZSet => t == KeyType::ZSet,
            Self::Stream => t == KeyType::Stream,
            Self::Other => matches!(t, KeyType::Json | KeyType::Other),
        }
    }
}

/// A write of the key list, sent and not answered yet.
#[derive(Debug, Clone, PartialEq)]
enum Writing {
    /// `+`: the new key's row.
    NewKey(KeyRow),
    /// `t`, `d` or `R` on the selected key.
    Key(WriteOp),
}

#[derive(Debug, Default)]
pub struct KeysState {
    /// The pattern whose keys are listed, as `PatternStats::pattern`. `None` for a find.
    pub pattern: Option<String>,
    /// A find (`ctrl-f`): the `SCAN MATCH` glob whose keys are listed instead of a
    /// pattern's ([`crate::live::keys::find_glob`]).
    pub find: Option<String>,
    /// The keys loaded so far, in the order they came.
    pub rows: Vec<KeyRow>,
    seen: HashSet<String>,
    /// Where the `SCAN` goes on.
    cursor: u64,
    /// Keys found but not loaded yet.
    carry: Vec<String>,
    /// The `SCAN` came back to 0 and every key found is loaded.
    pub done: bool,
    /// Pages answered so far. Only the first is loaded on its own; `m` loads the rest,
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
    /// `t`: only keys of this type, together with the name filter.
    pub type_filter: TypeFilter,
    /// The filter input is open.
    pub filtering: bool,
    pub sort: KeySort,
    /// The selected row among [`KeysState::visible`].
    pub sel: usize,
    /// The tick when the selection last changed, for the scrolling name.
    pub sel_since: u64,
    /// The `+`, `t` or `R` form, while it is open.
    pub form: Option<Form>,
    /// The key the `t` or `R` form is for.
    form_key: Option<String>,
    /// The write sent and not answered yet: its request, and what it was.
    writing: Option<(ReqId, Writing)>,
    /// A key `+` added, for `App` to list and open (`key_edits.rs`).
    pub opened: Option<KeyRow>,
    /// What `t`, `d` and `R` did, for `App` to tell the key's value if it is open
    /// (`key_edits.rs`).
    pub nav: Vec<Nav>,
    /// On a cluster, the nodes the walk reads, one at a time, in slot order (indices
    /// into the session's nodes): every connected node, or the one Patterns is narrowed
    /// to (spec "Cluster live views (P4)", Key list). Empty on a standalone server.
    pub walk: Vec<usize>,
    /// The nodes of the cluster left out of the walk: discovery has no connection.
    pub missing: Vec<usize>,
    /// Where the walk is in [`KeysState::walk`]: the node whose `SCAN` goes on.
    at: usize,
    /// Nodes walked to the end.
    pub nodes_done: usize,
    /// Keys loaded since the page was asked for (`m` or the first): a node that ends
    /// before a page is full goes on to the next node.
    page_rows: usize,
    /// The node each loaded key came from, by key (cluster only).
    pub row_node: HashMap<String, usize>,
    /// The node the page in flight reads.
    loading_node: Option<usize>,
}

impl KeysState {
    /// `enter` on a pattern on Patterns: list `pattern`'s keys. `App` calls this, then
    /// [`ViewState::on_show`]. It runs every time, also for the pattern already open:
    /// that one keeps its loaded keys, filter and sort; another starts afresh.
    ///
    /// `nodes` is the cluster's node table as `App` has it (empty on a standalone
    /// server) and `narrowed` the node Patterns is narrowed to, if any: the walk reads
    /// that node only, or every connected node in slot order. A pattern opened again on
    /// another set of nodes starts afresh.
    pub fn open(
        &mut self,
        pattern: String,
        nodes: &[crate::live::nodes::NodeInfo],
        narrowed: Option<usize>,
        act: &mut Act<'_>,
    ) {
        let all: Vec<usize> = match narrowed {
            Some(i) if i < nodes.len() => vec![i],
            _ => (0..nodes.len()).collect(),
        };
        let (walk, missing): (Vec<usize>, Vec<usize>) =
            all.into_iter().partition(|&i| nodes[i].connected);
        if self.pattern.as_deref() == Some(pattern.as_str())
            && self.walk == walk
            && self.missing == missing
        {
            return;
        }
        *self = Self {
            pattern: Some(pattern),
            sel_since: act.tick,
            walk,
            missing,
            ..Self::default()
        };
    }

    /// `ctrl-f`: list the keys of the whole server that `glob` matches. `App` calls this,
    /// then [`ViewState::on_show`]. It always starts afresh, replacing a pattern's keys
    /// or an earlier find. On a cluster the walk reads every connected node in slot
    /// order, whatever node Patterns is narrowed to.
    pub fn open_find(
        &mut self,
        glob: String,
        nodes: &[crate::live::nodes::NodeInfo],
        act: &mut Act<'_>,
    ) {
        let (walk, missing): (Vec<usize>, Vec<usize>) =
            (0..nodes.len()).partition(|&i| nodes[i].connected);
        *self = Self {
            find: Some(glob),
            sel_since: act.tick,
            walk,
            missing,
            ..Self::default()
        };
    }

    /// What the list's title names, safe to draw: the pattern, or `Find: <glob>`.
    pub fn heading(&self) -> String {
        match (&self.find, &self.pattern) {
            (Some(glob), _) => format!("Find: {}", crate::value::escape(glob)),
            (None, p) => crate::value::escape(p.as_deref().unwrap_or("")),
        }
    }

    /// The `SCAN MATCH` glob the list sends: the find's, or the pattern's.
    pub fn glob(&self) -> Option<String> {
        match (&self.find, &self.pattern) {
            (Some(glob), _) => Some(glob.clone()),
            (None, Some(p)) => Some(crate::analysis::match_glob(p)),
            (None, None) => None,
        }
    }

    /// What a key's value opened from this list is told it came from: the pattern, or
    /// the find's glob.
    pub fn source(&self) -> String {
        self.find
            .clone()
            .or_else(|| self.pattern.clone())
            .unwrap_or_default()
    }

    /// True on a cluster: the walk goes node by node.
    pub fn by_node(&self) -> bool {
        !self.walk.is_empty() || !self.missing.is_empty()
    }

    /// The node the walk reads now, on a cluster.
    pub fn current_node(&self) -> Option<usize> {
        self.walk.get(self.at).copied()
    }

    /// Indices into [`KeysState::rows`] after the filter, in the sort order.
    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        let mut idx: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                self.type_filter.matches(self.rows[i].key_type)
                    && (f.is_empty() || self.rows[i].key.to_lowercase().contains(&f))
            })
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
        if self.find.is_none() && self.pattern.is_none() {
            return false;
        }
        if self.rows.is_empty() && self.carry.is_empty() && self.cursor == 0 {
            // The first page: what to read comes from the permission check and policy.
            let allowed = |label: &str| matches!(act.check(label), Some(PermStatus::Allowed));
            self.sizes = act
                .report()
                .map_or(allowed(PERM_MEMORY), |r| r.sizes_available);
            self.lfu = act.server().is_some_and(|s| s.lfu_enabled());
            self.freq = self.lfu && allowed(PERM_FREQ);
        }
        let (cursor, carry, sizes, freq) = (self.cursor, self.carry.clone(), self.sizes, self.freq);
        let req = LiveRequest::Keys(match (&self.find, &self.pattern) {
            (Some(glob), _) => Request::Find {
                glob: glob.clone(),
                cursor,
                carry,
                sizes,
                freq,
            },
            (None, pattern) => Request::Page {
                pattern: pattern.clone().unwrap_or_default(),
                cursor,
                carry,
                sizes,
                freq,
            },
        });
        let node = self.current_node();
        if self.by_node() && node.is_none() {
            // Every node of the walk is done, or none is connected.
            return false;
        }
        // On a cluster the `SCAN` and the metadata pipeline run on one node, and a
        // redirect is never followed: the cursor belongs to that node.
        let sent = match node {
            Some(i) => act.send_to(Dest::node(i).follow(Follow::Never), req),
            None => act.send(req),
        };
        match sent {
            Some(id) => {
                self.loading = Some(id);
                self.loading_node = node;
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
            if self.by_node() && self.walk.is_empty() {
                // No node of the cluster is connected: nothing to walk.
                self.done = true;
                return;
            }
            self.page_rows = 0;
            self.load(act);
        }
    }

    /// A page came. True when the node it read was walked to the end and the walk goes
    /// on with the next node now, the page not being full yet.
    fn take_page(&mut self, page: Page, tick: u64) -> bool {
        let keep = self.selected().map(|r| r.key.clone());
        let node = self.loading_node.take();
        for row in page.rows {
            if self.seen.insert(row.key.clone()) {
                if let Some(n) = node {
                    self.row_node.insert(row.key.clone(), n);
                }
                self.rows.push(row);
                self.page_rows += 1;
            }
        }
        self.pages += 1;
        self.cursor = page.cursor;
        self.carry = page.carry;
        let node_done = self.cursor == 0 && self.carry.is_empty();
        let mut next = false;
        if !self.by_node() {
            self.done = node_done;
        } else if node_done {
            self.nodes_done = (self.at + 1).min(self.walk.len());
            if self.at + 1 < self.walk.len() {
                // The next node, from the start of its own keyspace.
                self.at += 1;
                self.done = false;
                next = self.page_rows < crate::live::keys::PAGE_KEYS;
            } else {
                self.done = true;
            }
        } else {
            self.done = false;
        }
        self.other_pattern += page.other_pattern;
        self.unreadable += page.unreadable;
        for e in page.refused {
            if !self.refused.contains(&e) {
                self.refused.push(e);
            }
        }
        self.reselect(keep, tick);
        next
    }

    /// Drops `key`, deleted on the key detail.
    pub fn drop_key(&mut self, key: &str) {
        let keep = self.selected().map(|r| r.key.clone());
        self.rows.retain(|r| r.key != key);
        self.seen.remove(key);
        self.row_node.remove(key);
        let tick = self.sel_since;
        self.reselect(keep, tick);
    }

    /// `old` was renamed `new` on the key detail.
    pub fn rename_key(&mut self, old: &str, new: &str) {
        for r in self.rows.iter_mut().filter(|r| r.key == old) {
            r.key = new.to_string();
        }
        if self.seen.remove(old) {
            self.seen.insert(new.to_string());
        }
        // `RENAMENX` keeps the key on its node: both names share a slot.
        if let Some(n) = self.row_node.remove(old) {
            self.row_node.insert(new.to_string(), n);
        }
    }

    /// The TTL and length of `row`'s key, read again after a write.
    pub fn update_row(&mut self, row: &KeyRow) {
        for r in self.rows.iter_mut().filter(|r| r.key == row.key) {
            r.ttl = row.ttl;
            r.length = row.length;
        }
    }

    /// A key `+` added: listed, and selected when the filter shows it.
    pub fn add_row(&mut self, row: KeyRow) {
        let key = row.key.clone();
        if self.seen.insert(key.clone()) {
            self.rows.push(row);
        }
        if let Some(i) = self.visible().iter().position(|&i| self.rows[i].key == key) {
            self.sel = i;
        }
    }

    /// `+`: the form for a new key, when the gate would let one through.
    fn start_new_key(&mut self, act: &mut Act<'_>) {
        if self.find.is_none() && self.pattern.is_none() {
            return;
        }
        // A find has no pattern: the name starts empty.
        let pattern = self.pattern.clone().unwrap_or_default();
        let probe = WriteOp::NewKey {
            key: "k".into(),
            value: NewKey::String(crate::value::Raw::from("")),
        };
        if let Decision::Refuse(why) = act.write_decision(&probe) {
            act.error(why);
            return;
        }
        if !act.live_ready() {
            act.error("Not sent: the live connection is not open.");
            return;
        }
        if self
            .writing
            .as_ref()
            .is_some_and(|(id, _)| act.in_flight(*id))
        {
            act.info("The last write has not answered yet.");
            return;
        }
        self.form = Some(Form::new_key(&pattern));
    }

    fn form_key(&mut self, key: &KeyEvent, act: &mut Act<'_>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        match form.key(key) {
            FormKey::Typing => {}
            FormKey::Cancel => {
                self.form = None;
                self.form_key = None;
                act.info(CANCELLED);
            }
            FormKey::Send => match form.op(self.form_key.as_deref().unwrap_or("")) {
                Err((at, why)) => {
                    form.at = at;
                    form.error = Some(why);
                }
                Ok(op) => {
                    self.form = None;
                    if self.form_key.take().is_some() {
                        // `t` or `R`: a write by the key's name.
                        if form_unchanged_rename(&op) {
                            act.info("Not changed, so nothing was sent.");
                            return;
                        }
                        self.send_key_write(op, act);
                        return;
                    }
                    let WriteOp::NewKey { key, value } = &op else {
                        return;
                    };
                    let row = KeyRow {
                        key: key.clone(),
                        key_type: match value {
                            NewKey::String(_) => KeyType::String,
                            NewKey::Hash { .. } => KeyType::Hash,
                            NewKey::List(_) => KeyType::List,
                            NewKey::Set(_) => KeyType::Set,
                            NewKey::ZSet { .. } => KeyType::ZSet,
                            NewKey::Stream { .. } => KeyType::Stream,
                        },
                        ttl: Ttl::Persistent,
                        bytes: None,
                        encoding: None,
                        // A string's length is its bytes; the other types start with one
                        // field, element, member or entry.
                        length: Some(match value {
                            NewKey::String(v) => u64::try_from(v.len()).unwrap_or(u64::MAX),
                            _ => 1,
                        }),
                        freq: None,
                    };
                    if let Some(id) = act.write(op) {
                        self.writing = Some((id, Writing::NewKey(row)));
                    }
                }
            },
        }
    }

    /// True when the selected key's writes may start: the gate would not refuse
    /// `probe`, the live connection is open and no other write of the list waits for
    /// its answer. Otherwise says why.
    fn may_write(&self, probe: &WriteOp, act: &mut Act<'_>) -> bool {
        if let Decision::Refuse(why) = act.write_decision(probe) {
            act.error(why);
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

    /// `t`, `d` or `R` (`c`) on the selected key: the same writes as `T`, `D` and `R`
    /// on its value, through the same gate and question, sent by name. Nothing of the
    /// key is read first.
    fn key_write(&mut self, c: char, act: &mut Act<'_>) {
        let Some(key) = self.selected().map(|r| r.key.clone()) else {
            act.info("No key selected.");
            return;
        };
        let probe = match c {
            't' => WriteOp::KeyTtl {
                key: key.clone(),
                ttl: crate::write::Ttl::Seconds(60),
            },
            'd' => WriteOp::Unlink { key: key.clone() },
            _ => WriteOp::Rename {
                key: key.clone(),
                new: format!("{key}:"),
            },
        };
        if !self.may_write(&probe, act) {
            return;
        }
        match c {
            't' => self.form = Some(Form::key_ttl(&key)),
            'd' => {
                self.send_key_write(probe, act);
                return;
            }
            _ => self.form = Some(Form::rename(&key)),
        }
        self.form_key = Some(key);
    }

    fn send_key_write(&mut self, op: WriteOp, act: &mut Act<'_>) {
        if let Some(id) = act.write(op.clone()) {
            self.writing = Some((id, Writing::Key(op)));
        }
    }

    /// The answer to `t`, `d` or `R`, in the words the key's value uses for them.
    fn key_written(
        &mut self,
        op: WriteOp,
        result: Result<LiveReply, LiveError>,
        act: &mut Act<'_>,
    ) {
        let n = match result {
            Ok(LiveReply::Write(WriteReply::Int(n))) => Some(n),
            Ok(LiveReply::Write(_)) => None,
            Ok(_) => return,
            Err(LiveError::Failed(s)) if s == CANCELLED => return,
            // RENAMENX on a key that has gone: the row goes, as after a delete.
            Err(LiveError::Failed(s)) if s.ends_with("the key no longer exists") => {
                if let WriteOp::Rename { key, .. }
                | WriteOp::KeyTtl { key, .. }
                | WriteOp::Unlink { key } = &op
                {
                    self.key_gone(key, act);
                }
                return;
            }
            Err(e) => {
                act.error(format!("Not written: {e}."));
                return;
            }
        };
        match op {
            WriteOp::Unlink { key } => {
                if n == Some(0) {
                    act.info("The key no longer exists.");
                } else {
                    act.info(format!("Deleted {}.", crate::value::escape(&key)));
                }
                self.drop_key(&key);
                self.nav.push(Nav::Deleted(key));
            }
            WriteOp::Rename { key, new } => {
                if n == Some(0) {
                    act.error(format!(
                        "Not renamed: {} exists already, and RENAMENX never overwrites a key.",
                        crate::value::escape(&new)
                    ));
                    return;
                }
                self.rename_key(&key, &new);
                act.info(format!("Renamed to {}.", crate::value::escape(&new)));
                self.nav.push(Nav::Renamed { old: key, new });
            }
            WriteOp::KeyTtl { key, ttl } => {
                use crate::write::Ttl as WriteTtl;
                let row_ttl = match (ttl, n) {
                    (WriteTtl::Persist, Some(0)) => {
                        act.info("The key had no TTL.");
                        return;
                    }
                    (WriteTtl::Seconds(_), Some(0)) => {
                        self.key_gone(&key, act);
                        return;
                    }
                    (WriteTtl::Seconds(s), _) => {
                        act.info(format!(
                            "TTL set to {}.",
                            crate::format::duration(std::time::Duration::from_secs(s))
                        ));
                        Ttl::Expires(std::time::Duration::from_secs(s))
                    }
                    (WriteTtl::Persist, _) => {
                        act.info("TTL removed: the key no longer expires.");
                        Ttl::Persistent
                    }
                };
                for r in self.rows.iter_mut().filter(|r| r.key == key) {
                    r.ttl = row_ttl;
                }
                if let Some(r) = self.rows.iter().find(|r| r.key == key) {
                    self.nav.push(Nav::Row(r.clone()));
                }
            }
            _ => {}
        }
    }

    /// A write from the list found its key gone: the row goes, and an open value of
    /// that key closes, as after `d`.
    fn key_gone(&mut self, key: &str, act: &mut Act<'_>) {
        act.info("The key no longer exists.");
        self.drop_key(key);
        self.nav.push(Nav::Deleted(key.to_string()));
    }

    /// The answer to `+`'s write.
    fn written(&mut self, row: KeyRow, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        let name = crate::value::escape(&row.key);
        match result {
            Ok(LiveReply::Write(WriteReply::Exists)) => act.error(format!(
                "Not added: {name} exists already, and + never overwrites a key."
            )),
            Ok(LiveReply::Write(WriteReply::Changed)) => act.error(format!(
                "Not added: {name} was written by someone else meanwhile."
            )),
            Ok(LiveReply::Write(_)) => {
                act.info(format!("Added {name}."));
                // On a cluster it went to its slot's owner.
                if let Some(n) = act.session.owner_of(row.key.as_bytes()) {
                    self.row_node.insert(row.key.clone(), n);
                }
                self.opened = Some(row);
            }
            Ok(_) => {}
            Err(LiveError::Failed(s)) if s == CANCELLED => {}
            Err(e) => act.error(format!("Not written: {e}.")),
        }
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

/// True for a `t` or `R` form left as it started: a rename to the same name.
fn form_unchanged_rename(op: &WriteOp) -> bool {
    matches!(op, WriteOp::Rename { key, new } if key == new)
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

/// Why a find's page failed, for the screen.
pub fn find_error(e: &LiveError) -> String {
    match e {
        LiveError::Denied(cmd) if cmd == "SCAN" => {
            "The server refused SCAN for this user, so keys can't be searched.".to_string()
        }
        e => load_error(e),
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
        if self.form.is_some() {
            self.form_key(&key, act);
            return true;
        }
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
            // `←` `h` `esc` clear the filters first, then go back to the patterns.
            (KeyCode::Esc | KeyCode::Left, _) | (_, Some('h'))
                if !self.filter.is_empty() || self.type_filter != TypeFilter::All =>
            {
                let keep = self.selected().map(|r| r.key.clone());
                self.filter.clear();
                self.type_filter = TypeFilter::All;
                self.reselect(keep, tick);
                true
            }
            (_, Some('/')) => {
                self.filtering = true;
                true
            }
            (_, Some('s')) => {
                let keep = self.selected().map(|r| r.key.clone());
                self.sort = self
                    .sort
                    .effective(self.sizes, self.freq)
                    .next(self.sizes, self.freq);
                self.reselect(keep, tick);
                act.info(format!("Keys sorted by {}.", self.sort.label()));
                true
            }
            (KeyCode::Char('t'), None) if key.modifiers == KeyModifiers::CONTROL => {
                let keep = self.selected().map(|r| r.key.clone());
                self.type_filter = self.type_filter.next();
                self.reselect(keep, tick);
                act.info(format!("Showing {} keys.", self.type_filter.label()));
                true
            }
            (_, Some('m')) => {
                if self.loading.is_some_and(|id| act.in_flight(id)) {
                    act.info("Still loading keys…");
                } else if self.done {
                    act.info(format!(
                        "All {} matching keys are loaded.",
                        crate::format::grouped(self.rows.len() as u64)
                    ));
                } else {
                    self.page_rows = 0;
                    if !self.load(act) {
                        act.info("The live connection is not open, so no more keys can be loaded.");
                    }
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
            // The parent of a key is its pattern: the list's, or for a find's result the
            // one its name infers to.
            (_, Some('Y')) => {
                match self.selected() {
                    Some(r) => {
                        let p = match (&self.find, &self.pattern) {
                            (None, Some(p)) => p.clone(),
                            _ => crate::pattern::pattern_for_key(&r.key),
                        };
                        act.copy(CopyText::pattern(&p));
                    }
                    None => act.info("Nothing selected to copy."),
                }
                true
            }
            (_, Some('+')) if act.screen == Screen::KeyList => {
                self.start_new_key(act);
                true
            }
            (_, Some(c @ ('t' | 'd' | 'R'))) if act.screen == Screen::KeyList => {
                self.key_write(c, act);
                true
            }
            _ => false,
        }
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        if let Some((_, w)) = self.writing.take_if(|(w, _)| *w == id) {
            match w {
                Writing::NewKey(row) => self.written(row, result, act),
                Writing::Key(op) => self.key_written(op, result, act),
            }
            return;
        }
        if self.loading != Some(id) {
            return;
        }
        self.loading = None;
        match result {
            Ok(LiveReply::Keys(Reply::Page(page))) => {
                if self.take_page(page, act.tick) {
                    self.load(act);
                }
            }
            Ok(_) => {}
            Err(e) => {
                self.loading_node = None;
                self.error = Some(if self.find.is_some() {
                    find_error(&e)
                } else {
                    load_error(&e)
                });
            }
        }
    }

    fn input(&self) -> Option<Input> {
        if let Some(f) = &self.form {
            return Some(f.input());
        }
        self.filtering.then(|| Input {
            label: "/",
            text: self.filter.clone(),
            hint: "   enter to apply · esc to clear",
        })
    }

    fn on_paste(&mut self, text: &str, act: &mut Act<'_>) {
        if let Some(f) = self.form.as_mut() {
            f.paste(text);
            return;
        }
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

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// A scanned app with the live connection ready, on the key list of `pattern`.
    fn open(pattern: &str) -> (App, Vec<Effect>) {
        let mut app = testing::scanned();
        let mut out = testing::live_ready(&mut app);
        out.extend(crate::tui::app::views::testing::area(&mut app, '2'));
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

    /// The carts with a few keys of other types mixed in.
    fn mixed() -> App {
        let (mut app, effects) = open("user:{id}:cart");
        let (id, _) = page_request(&effects);
        let mut rows = carts();
        for (i, t) in [
            KeyType::String,
            KeyType::String,
            KeyType::Json,
            KeyType::Other,
            KeyType::Stream,
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = row(&format!("user:9{i}:cart"), None, 10, "raw", 1, 0);
            r.key_type = t;
            rows.push(r);
        }
        testing::reply(&mut app, id, page(rows, 4_242));
        app
    }

    fn type_of(app: &App) -> Vec<KeyType> {
        let k = &app.views.keys;
        k.visible().iter().map(|&i| k.rows[i].key_type).collect()
    }

    #[test]
    fn ctrl_t_cycles_the_types_and_sends_nothing() {
        let mut app = mixed();
        let mut seen = Vec::new();
        for _ in 0..8 {
            let effects = app.handle_key(ctrl('t'));
            assert!(testing::requests(&effects).is_empty());
            seen.push(app.views.keys.type_filter.label());
        }
        assert_eq!(
            seen,
            [
                "string", "hash", "list", "set", "zset", "stream", "other", "all"
            ]
        );
        assert_eq!(app.views.keys.visible().len(), 14);
    }

    #[test]
    fn the_type_filter_combines_with_the_name_filter() {
        let mut app = mixed();
        app.handle_key(ctrl('t'));
        assert_eq!(type_of(&app), [KeyType::String; 2]);
        app.handle_key(ctrl('t'));
        assert_eq!(app.views.keys.visible().len(), 9);
        // other takes JSON and unknown types.
        for _ in 0..5 {
            app.handle_key(ctrl('t'));
        }
        assert_eq!(app.views.keys.type_filter, TypeFilter::Other);
        let mut other = type_of(&app);
        other.sort_by_key(|t| t.as_str());
        assert_eq!(other, [KeyType::Json, KeyType::Other]);
        // Back to strings, then a name filter on top.
        for _ in 0..2 {
            app.handle_key(ctrl('t'));
        }
        assert_eq!(app.views.keys.type_filter, TypeFilter::String);
        app.handle_key(ch('/'));
        app.handle_key(ch('1'));
        app.handle_key(key(KeyCode::Enter));
        let k = &app.views.keys;
        assert_eq!(k.visible().len(), 1);
        assert_eq!(k.selected().map(|r| r.key.as_str()), Some("user:91:cart"));
    }

    #[test]
    fn t_is_text_in_the_filter_input_and_esc_clears_both_filters() {
        let mut app = mixed();
        app.handle_key(ch('/'));
        app.handle_key(ch('t'));
        app.handle_key(ctrl('t'));
        assert_eq!(app.views.keys.type_filter, TypeFilter::All);
        assert_eq!(app.views.keys.filter, "t");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ctrl('t'));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.type_filter, TypeFilter::All);
        assert_eq!(app.views.keys.visible().len(), 14);
        app.handle_key(key(KeyCode::Esc));
        assert_ne!(app.screen(), Screen::KeyList);
    }

    #[test]
    fn left_and_h_clear_the_filters_first_like_esc() {
        for back in [key(KeyCode::Left), ch('h')] {
            let mut app = mixed();
            app.handle_key(ctrl('t'));
            app.handle_key(ch('/'));
            app.handle_key(ch('1'));
            app.handle_key(key(KeyCode::Enter));
            assert!(app.views.keys.visible().len() < 14);
            // The first `←` clears both filters and stays on the keys.
            app.handle_key(back);
            assert_eq!(app.screen(), Screen::KeyList, "{back:?}");
            assert_eq!(app.views.keys.filter, "");
            assert_eq!(app.views.keys.type_filter, TypeFilter::All);
            assert_eq!(app.views.keys.visible().len(), 14);
            // The next goes back to the patterns.
            app.handle_key(back);
            assert_eq!(app.screen(), Screen::Patterns, "{back:?}");
        }
    }

    #[test]
    fn key_list_with_a_type_filter_80x24() {
        let mut app = mixed();
        app.handle_key(ctrl('t'));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn key_list_with_a_type_and_nothing_of_it_80x24() {
        let mut app = loaded();
        app.handle_key(ctrl('t'));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn a_page_with_no_key_of_the_pattern_waits_for_m() {
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
        let (_, req) = page_request(&app.handle_key(ch('m')));
        assert!(matches!(req, Request::Page { cursor: 77, .. }));
    }

    #[test]
    fn m_loads_the_next_page_until_the_walk_is_done() {
        let mut app = loaded();
        let (id, req) = page_request(&app.handle_key(ch('m')));
        let Request::Page { cursor, .. } = req else {
            unreachable!()
        };
        assert_eq!(cursor, 4_242);
        // A second n while it loads sends nothing.
        assert!(testing::requests(&app.handle_key(ch('m'))).is_empty());
        assert_eq!(notice(&app), "Still loading keys…");
        // Duplicates SCAN returned again are dropped.
        let more = vec![
            row("user:10293:cart", None, 1, "listpack", 1, 1),
            row("user:20001:cart", Some(60), 1_024, "listpack", 3, 2),
        ];
        testing::reply(&mut app, id, page(more, 0));
        assert_eq!(app.views.keys.rows.len(), 10);
        assert!(app.views.keys.done);
        assert!(testing::requests(&app.handle_key(ch('m'))).is_empty());
        assert_eq!(notice(&app), "All 10 matching keys are loaded.");
    }

    #[test]
    fn sort_filter_copy_and_esc() {
        let mut app = loaded();
        let first = |app: &App| app.views.keys.selected().map(|r| r.key.clone());
        // By size, largest first.
        assert_eq!(first(&app).as_deref(), Some("user:10402:cart"));
        app.handle_key(ch('s'));
        assert_eq!(notice(&app), "Keys sorted by length.");
        app.handle_key(ch('s'));
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
    fn enter_opens_the_selected_key_and_esc_comes_back() {
        let mut app = loaded();
        let selected = app.views.keys.selected().cloned().unwrap();
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyDetail);
        assert_eq!(app.views.value.row.as_ref(), Some(&selected));
        assert_eq!(testing::requests(&effects).len(), 1, "{effects:?}");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.rows.len(), 9);

        // With no key loaded there is nothing to open.
        let (mut app, _) = open("user:{id}:cart");
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(testing::requests(&effects).is_empty(), "{effects:?}");
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(notice(&app), "No key selected to open.");
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
        // The right pane wraps the note: its words, in order, over two lines.
        assert!(
            screen.contains("▲ Column off: the server refused MEMORY"),
            "{screen}"
        );
        assert!(screen.contains("  USAGE for this user."), "{screen}");
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
        // The list pane wraps it under its `✗`.
        assert!(
            screen.contains("✗ The server refused SCAN for this"),
            "{screen}"
        );
        assert!(
            screen.contains("  user, so the keys can't be listed."),
            "{screen}"
        );
    }

    // -----------------------------------------------------------------------
    // N (P3)
    // -----------------------------------------------------------------------

    use crate::live::write::Reply as WReply;
    use crate::model::Env;
    use crate::write::{NewKey, WriteOp};

    fn writes(effects: &[Effect]) -> Vec<(ReqId, WriteOp)> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(id, r)| match r {
                LiveRequest::Write(p) => Some((id, p.op().clone())),
                _ => None,
            })
            .collect()
    }

    fn writable() -> App {
        let mut app = loaded();
        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Local;
        s.profile.read_only = None;
        app
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(ch(c));
        }
    }

    #[test]
    fn plus_adds_a_key_and_opens_it() {
        let mut app = writable();
        app.handle_key(ch('+'));
        let form = app.views.keys.form.as_ref().expect("the form");
        assert_eq!(form.fields[0].text, "user:");
        type_text(&mut app, "1:cart");
        app.handle_key(key(KeyCode::Tab));
        // The type: h picks hash, and its fields follow.
        app.handle_key(ch('h'));
        let labels: Vec<&str> = app
            .views
            .keys
            .form
            .as_ref()
            .unwrap()
            .fields
            .iter()
            .map(|f| f.label)
            .collect();
        assert_eq!(labels, ["name", "type", "field", "value"]);
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "sku");
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "2");
        let e = app.handle_key(key(KeyCode::Enter));
        let w = writes(&e);
        assert_eq!(
            w.iter().map(|(_, op)| op.clone()).collect::<Vec<_>>(),
            [WriteOp::NewKey {
                key: "user:1:cart".into(),
                value: NewKey::Hash {
                    field: crate::value::Raw::from("sku"),
                    value: crate::value::Raw::from("2")
                }
            }]
        );
        assert!(app.views.keys.form.is_none());
        let e = testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(1))));
        assert_eq!(app.screen(), Screen::KeyDetail);
        assert_eq!(app.views.value.row.as_ref().unwrap().key, "user:1:cart");
        assert!(app.views.keys.rows.iter().any(|r| r.key == "user:1:cart"));
        // Its value is read.
        assert_eq!(testing::requests(&e).len(), 1, "{e:?}");
    }

    #[test]
    fn a_new_string_keys_length_is_its_bytes() {
        let mut app = writable();
        app.handle_key(ch('+'));
        type_text(&mut app, "1:name");
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(ch('s'));
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "hello");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        assert!(
            matches!(
                &w[..],
                [(
                    _,
                    WriteOp::NewKey {
                        value: NewKey::String(_),
                        ..
                    }
                )]
            ),
            "{w:?}"
        );
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Ok)));
        let row = app.views.keys.rows.iter().find(|r| r.key == "user:1:name");
        assert_eq!(row.and_then(|r| r.length), Some(5), "{row:?}");
    }

    #[test]
    fn plus_never_overwrites_and_checks_what_is_typed() {
        let mut app = writable();
        app.handle_key(ch('+'));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        // No name: the form stays open and says why.
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        let form = app.views.keys.form.as_ref().unwrap();
        assert_eq!(
            (form.at, form.error.as_deref()),
            (0, Some("Type the key's name."))
        );
        type_text(&mut app, "user:10293:cart");
        app.handle_key(key(KeyCode::Tab));
        // A sorted set needs a number for its score.
        app.handle_key(ch('z'));
        app.handle_key(key(KeyCode::Tab));
        type_text(&mut app, "m");
        app.handle_key(key(KeyCode::Tab));
        type_text(&mut app, "x");
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert_eq!(app.views.keys.form.as_ref().unwrap().at, 3);
        app.handle_key(key(KeyCode::Backspace));
        type_text(&mut app, "1");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        assert!(matches!(
            &w[..],
            [(_, WriteOp::NewKey { value: NewKey::ZSet { score, .. }, .. })] if *score == 1.0
        ));
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Exists)));
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(
            notice(&app),
            "Not added: user:10293:cart exists already, and + never overwrites a key."
        );
        // esc cancels.
        app.handle_key(ch('+'));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.views.keys.form.is_none());
        assert_eq!(app.screen(), Screen::KeyList);
    }

    #[test]
    fn plus_sends_nothing_on_a_read_only_profile() {
        for (env, ro) in [(Env::Prod, None), (Env::Local, Some(true))] {
            let mut app = loaded();
            let s = app.session.as_mut().unwrap();
            s.profile.env = env;
            s.profile.read_only = ro;
            let mut effects = app.handle_key(ch('+'));
            assert!(app.views.keys.form.is_none());
            assert!(notice(&app).contains("read-only"), "{}", notice(&app));
            for c in "user:1\thy\n\nprod-cache-01\n".chars() {
                let k = match c {
                    '\t' => key(KeyCode::Tab),
                    '\n' => key(KeyCode::Enter),
                    c => ch(c),
                };
                effects.extend(app.handle_key(k));
            }
            effects.extend(app.handle_paste("x".into()));
            effects.extend(app.handle_key(ch('+')));
            effects.extend(app.handle_key(key(KeyCode::Enter)));
            assert!(writes(&effects).is_empty(), "{effects:?}");
        }
    }

    #[test]
    fn plus_on_prod_with_writes_on_asks() {
        let mut app = writable();
        app.session.as_mut().unwrap().profile.env = Env::Prod;
        app.session.as_mut().unwrap().profile.read_only = Some(false);
        app.handle_key(ch('+'));
        type_text(&mut app, "9");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "v");
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert!(app.confirm.is_some());
        assert_eq!(writes(&app.handle_key(ch('y'))).len(), 1);
    }

    #[test]
    fn capital_y_copies_the_pattern() {
        let mut app = loaded();
        let copied: Vec<String> = app
            .handle_key(ch('Y'))
            .into_iter()
            .filter_map(|e| match e {
                Effect::Copy(t) => Some(t.as_str().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(copied, ["user:{id}:cart"]);
    }

    fn selected_key(app: &App) -> String {
        app.views
            .keys
            .selected()
            .expect("a selected key")
            .key
            .clone()
    }

    /// `R`, `t` and `d` on the key list act on the selected key with the same writes
    /// the key's value sends (`RENAMENX`, `EXPIRE`, `UNLINK`), through the same gate,
    /// and update its row.
    #[test]
    fn r_t_and_d_on_the_key_list_write_the_selected_key_and_update_its_row() {
        let mut app = writable();
        let old = selected_key(&app);
        // R: rename, the form filled with the name; the same name sends nothing.
        assert!(writes(&app.handle_key(ch('R'))).is_empty());
        assert!(app.views.keys.form.is_some());
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        app.handle_key(ch('R'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "user:1:cart");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        assert_eq!(
            w.iter().map(|(_, op)| op.clone()).collect::<Vec<_>>(),
            [WriteOp::Rename {
                key: old.clone(),
                new: "user:1:cart".into()
            }]
        );
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(1))));
        assert!(app.views.keys.rows.iter().any(|r| r.key == "user:1:cart"));
        assert!(!app.views.keys.rows.iter().any(|r| r.key == old));
        assert_eq!(selected_key(&app), "user:1:cart");

        // t: the key's TTL.
        app.handle_key(ch('t'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "90");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        assert_eq!(
            w.iter().map(|(_, op)| op.clone()).collect::<Vec<_>>(),
            [WriteOp::KeyTtl {
                key: "user:1:cart".into(),
                ttl: crate::write::Ttl::Seconds(90)
            }]
        );
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(1))));
        let row = app
            .views
            .keys
            .rows
            .iter()
            .find(|r| r.key == "user:1:cart")
            .unwrap();
        assert_eq!(row.ttl, Ttl::Expires(Duration::from_secs(90)));

        // d: asks first (a delete), then UNLINK, and the row goes.
        assert!(writes(&app.handle_key(ch('d'))).is_empty());
        assert!(app.confirm.is_some());
        let w = writes(&app.handle_key(ch('y')));
        assert_eq!(
            w.iter().map(|(_, op)| op.clone()).collect::<Vec<_>>(),
            [WriteOp::Unlink {
                key: "user:1:cart".into()
            }]
        );
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(1))));
        assert!(!app.views.keys.rows.iter().any(|r| r.key == "user:1:cart"));
        assert_eq!(app.screen(), Screen::KeyList);
    }

    /// `t` or `R` on a key that has gone meanwhile: its row goes, as after `d`.
    #[test]
    fn t_and_r_on_a_key_that_has_gone_drop_its_row() {
        let mut app = writable();
        let gone = selected_key(&app);
        app.handle_key(ch('t'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "90");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(0))));
        assert_eq!(notice(&app), "The key no longer exists.");
        assert!(!app.views.keys.rows.iter().any(|r| r.key == gone));

        let gone = selected_key(&app);
        app.handle_key(ch('R'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "user:1:cart");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        testing::reply(
            &mut app,
            w[0].0,
            Err(LiveError::Failed(
                "RENAMENX failed: the key no longer exists".into(),
            )),
        );
        assert_eq!(notice(&app), "The key no longer exists.");
        assert!(!app.views.keys.rows.iter().any(|r| r.key == gone));
        assert!(!app.views.keys.rows.iter().any(|r| r.key == "user:1:cart"));
    }

    /// A rename to a name that exists changes nothing; `n` on the delete question
    /// sends nothing.
    #[test]
    fn a_taken_name_and_a_cancelled_delete_leave_the_row() {
        let mut app = writable();
        let old = selected_key(&app);
        app.handle_key(ch('R'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "user:10311:cart");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(0))));
        assert!(notice(&app).contains("exists already"), "{}", notice(&app));
        assert_eq!(selected_key(&app), old);
        app.handle_key(ch('d'));
        assert!(writes(&app.handle_key(ch('n'))).is_empty());
        assert!(app.views.keys.rows.iter().any(|r| r.key == old));
    }

    /// The value of the key open under the list follows a write made from the list.
    #[test]
    fn the_open_value_follows_a_write_from_the_key_list() {
        let mut app = writable();
        let old = selected_key(&app);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyDetail);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::KeyList);
        app.handle_key(ch('R'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "user:1:cart");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(1))));
        assert_eq!(app.views.value.row.as_ref().unwrap().key, "user:1:cart");
        assert_ne!(old, "user:1:cart");
        app.handle_key(ch('t'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "60");
        let w = writes(&app.handle_key(key(KeyCode::Enter)));
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(1))));
        assert_eq!(
            app.views.value.row.as_ref().unwrap().ttl,
            Ttl::Expires(Duration::from_secs(60))
        );
        app.handle_key(ch('d'));
        let w = writes(&app.handle_key(ch('y')));
        testing::reply(&mut app, w[0].0, Ok(LiveReply::Write(WReply::Int(1))));
        assert!(app.views.value.row.is_none());
        assert!(!app.patterns.detail_open);
    }

    /// On a read-only profile `t`, `d` and `R` say why and send nothing, whatever
    /// follows; a prod profile with writes on asks first.
    #[test]
    fn t_d_and_r_send_nothing_on_a_read_only_profile() {
        for (env, ro) in [(Env::Prod, None), (Env::Local, Some(true))] {
            for k in ['t', 'd', 'R'] {
                // Each key from a fresh key list, staying on it: no key below leaves it.
                let mut app = loaded();
                let s = app.session.as_mut().unwrap();
                s.profile.env = env;
                s.profile.read_only = ro;
                let mut effects = app.handle_key(ch(k));
                assert_eq!(app.screen(), Screen::KeyList, "{k}");
                assert!(
                    app.views.keys.form.is_none() && app.confirm.is_none(),
                    "{k}"
                );
                assert!(notice(&app).contains("read-only"), "{k}: {}", notice(&app));
                assert!(writes(&effects).is_empty(), "{k}: {effects:?}");
                for e in [ch('y'), key(KeyCode::Enter)] {
                    effects.extend(app.handle_key(e));
                }
                assert!(writes(&effects).is_empty(), "{k}: {effects:?}");
            }
        }
    }

    #[test]
    fn t_on_prod_with_writes_on_asks_first() {
        let mut app = writable();
        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Prod;
        s.profile.read_only = Some(false);
        app.handle_key(ch('t'));
        app.handle_key(ctrl('u'));
        type_text(&mut app, "90");
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert!(app.confirm.is_some());
        assert_eq!(writes(&app.handle_key(ch('y'))).len(), 1);
    }

    #[test]
    fn the_new_key_form_80x24() {
        let mut app = writable();
        app.handle_key(ch('+'));
        type_text(&mut app, "1:cart");
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(ch('h'));
        app.handle_key(key(KeyCode::Tab));
        type_text(&mut app, "sku:1");
        app.handle_key(key(KeyCode::Tab));
        type_text(&mut app, "2");
        insta::assert_snapshot!(
            "new_key_form_80x24",
            testing::render(&app, 80, 24).backend()
        );
    }
}
