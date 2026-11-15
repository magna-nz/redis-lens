//! Browse › Keys: the keys of a pattern (`GET /api/keys`), a find over the whole server
//! (`GET /api/find`), and their next pages (`GET /api/keys/more`).
//!
//! A list walks as the TUI's key list walks ([`crate::tui::app::views::keys`]): one
//! [`Request::Page`] (or [`Request::Find`]) at a time, `SCAN` with the pattern's glob
//! (or the find's), at most [`PAGE_KEYS`] keys a page, then each key's metadata
//! pipelined, through the shared budget. On a cluster the walk reads each connected
//! node in slot order, never following a redirect (the cursor is that node's); a node
//! that ends before a page is full goes on to the next one. The first page is read
//! when the list opens; every next one when the page asks.
//!
//! Lists are kept server side, at most [`KEPT`] of them (the oldest goes first), each
//! named by its `list` number; no more pages are read once one holds [`MAX_ROWS`] keys.
//! The first page's reply holds every row; a next page's holds only the rows it added
//! (from `new_from`).

use std::collections::{HashMap, HashSet, VecDeque};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::{App, Q, Refused, error, ok};
use crate::live::keys::{KeyRow, PAGE_KEYS, Page, Reply, Request, find_glob, find_refusal};
use crate::live::{Dest, Follow, LiveError, LiveReply, LiveRequest};
use crate::model::{PERM_FREQ, PERM_MEMORY, PermStatus, Ttl};
use crate::tui::app::ConnId;
use crate::tui::app::views::keys::{find_error, load_error};
use crate::web::keyname::{self, Name};

/// Key lists kept at once.
pub const KEPT: usize = 16;
/// Once a list holds this many keys, no more of its pages are read.
pub const MAX_ROWS: usize = 50_000;

/// Why a find was refused.
pub const EMPTY_FIND: &str = "Type a name or a pattern to find.";
pub const NO_LIST: &str =
    "That key list is gone (a newer one replaced it, or the session changed): open it again.";
pub const LIST_DONE: &str = "Every key of this list is loaded.";
pub const LOADING: &str = "This list is loading a page already: wait for it.";
pub const LIST_FULL: &str = "This list holds 50,000 keys or more, so no more are loaded. Find a narrower name to see others.";

/// One key list.
#[derive(Debug, Default)]
pub struct KeyList {
    pub id: u64,
    conn: ConnId,
    /// The pattern listed, as `PatternStats::pattern`; `None` for a find.
    pub pattern: Option<String>,
    /// A find's text as typed, and its `SCAN MATCH` glob.
    pub find: Option<(String, String)>,
    /// The node a pattern's list is narrowed to.
    pub narrowed: Option<usize>,
    pub rows: Vec<KeyRow>,
    seen: HashSet<String>,
    cursor: u64,
    carry: Vec<String>,
    pub done: bool,
    pub pages: u64,
    loading: bool,
    pub error: Option<String>,
    pub refused: Vec<LiveError>,
    pub other_pattern: u64,
    pub unreadable: u64,
    pub sizes: bool,
    pub freq: bool,
    pub lfu: bool,
    walk: Vec<usize>,
    missing: Vec<usize>,
    at: usize,
    pub nodes_done: usize,
    page_rows: usize,
    row_node: HashMap<String, usize>,
    loading_node: Option<usize>,
    /// Rows the last page added, from this index on.
    new_from: usize,
}

impl KeyList {
    fn by_node(&self) -> bool {
        !self.walk.is_empty() || !self.missing.is_empty()
    }

    fn current_node(&self) -> Option<usize> {
        self.walk.get(self.at).copied()
    }

    /// The next page's request and where it goes, as the TUI's `load` builds it.
    fn next(&mut self) -> Option<(Dest, LiveRequest)> {
        let (cursor, carry, sizes, freq) = (self.cursor, self.carry.clone(), self.sizes, self.freq);
        let req = LiveRequest::Keys(match (&self.find, &self.pattern) {
            (Some((_, glob)), _) => Request::Find {
                glob: glob.clone(),
                cursor,
                carry,
                sizes,
                freq,
            },
            (None, pattern) => Request::Page {
                pattern: pattern.clone()?,
                cursor,
                carry,
                sizes,
                freq,
            },
        });
        let node = self.current_node();
        if self.by_node() && node.is_none() {
            return None;
        }
        self.loading_node = node;
        Some(match node {
            Some(i) => (Dest::node(i).follow(Follow::Never), req),
            None => (Dest::node(0), req),
        })
    }

    /// A page came: true when its node was walked to the end and the walk goes on with
    /// the next node, the page not being full yet (the TUI's `take_page`).
    fn take(&mut self, page: Page) -> bool {
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
                self.at += 1;
                self.done = false;
                next = self.page_rows < PAGE_KEYS;
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
        self.error = None;
        next
    }

    /// Why `GET /api/keys/more` reads no next page now (`live`: the live connection is
    /// ready), or `None` when it does.
    fn more_refused(&self, live: bool) -> Option<&'static str> {
        if self.loading {
            Some(LOADING)
        } else if self.done {
            Some(LIST_DONE)
        } else if self.rows.len() >= MAX_ROWS {
            Some(LIST_FULL)
        } else if !live {
            Some(super::NOT_LIVE)
        } else {
            None
        }
    }

    fn failed(&mut self, e: &LiveError) {
        self.loading_node = None;
        self.error = Some(if self.find.is_some() {
            find_error(e)
        } else {
            load_error(e)
        });
    }
}

/// The lists of the session, newest last.
#[derive(Debug, Default)]
pub struct Lists {
    next: u64,
    lists: VecDeque<KeyList>,
}

impl Lists {
    fn add(&mut self, mut l: KeyList) -> u64 {
        self.next += 1;
        l.id = self.next;
        if self.lists.len() >= KEPT {
            self.lists.pop_front();
        }
        self.lists.push_back(l);
        self.next
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut KeyList> {
        self.lists.iter_mut().find(|l| l.id == id)
    }

    /// The row of `key` in any list, newest list first: what a key's value is opened
    /// with (its type, size and length).
    pub fn row(&self, key: &str) -> Option<KeyRow> {
        self.lists
            .iter()
            .rev()
            .find_map(|l| l.rows.iter().find(|r| r.key == key).cloned())
    }

    /// Tests: whether list `id` is loading a page; `None` when there is no such list.
    #[cfg(test)]
    pub(crate) fn loading_of(&self, id: u64) -> Option<bool> {
        self.lists.iter().find(|l| l.id == id).map(|l| l.loading)
    }

    /// Drops every list (a database switch). The numbers go on from where they were:
    /// one from before never names a list opened after.
    pub fn clear(&mut self) {
        self.lists.clear();
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenQuery {
    /// The pattern's id (hex), from `GET /api/browse`.
    pattern: String,
    /// A report node to narrow the walk to (a cluster's `node` of `GET /api/browse`).
    node: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct FindQuery {
    q: String,
}

#[derive(Debug, Deserialize)]
pub struct MoreQuery {
    list: u64,
}

/// `GET /api/keys?pattern=<id>[&node=<i>]`: the keys of a pattern, first page.
pub async fn open(State(app): State<App>, Q(q): Q<OpenQuery>) -> Response {
    let pattern = match keyname::decode(&q.pattern) {
        Ok(p) => p,
        Err(r) => return r.into_response(),
    };
    let made = app.hub.with_mut(|c| {
        if !c.live_ready() {
            return Err(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE));
        }
        // A pattern's walk reads the report node it is narrowed to, as Patterns narrowed
        // to a node does; the report's node index is the live node of the same address.
        let nodes = &c.session.nodes;
        let narrowed = q.node.and_then(|n| {
            let r = c.session.report.as_ref()?.nodes.as_ref()?.get(n)?;
            nodes
                .iter()
                .position(|i| i.id == r.id || i.endpoint == r.endpoint)
        });
        if q.node.is_some() && narrowed.is_none() {
            return Err(Refused::new(StatusCode::UNPROCESSABLE_ENTITY, NO_NODE));
        }
        let all: Vec<usize> = match narrowed {
            Some(i) => vec![i],
            None => (0..nodes.len()).collect(),
        };
        let (walk, missing) = all.into_iter().partition(|&i| nodes[i].connected);
        let l = start(
            c,
            KeyList {
                pattern: Some(pattern.clone()),
                narrowed,
                walk,
                missing,
                ..KeyList::default()
            },
        );
        Ok(c.browse.lists.add(l))
    });
    finish(&app, made).await
}

/// Why a pattern's list can't be narrowed to the node asked for.
pub const NO_NODE: &str = "That node is not one of this analysis's nodes.";

/// `GET /api/find?q=<text>`: the keys of the whole server whose names match `text`
/// (the TUI's Find key: text with `*`, `?` or `[` is a glob, any other text means
/// "contains"), first page.
pub async fn find(State(app): State<App>, Q(q): Q<FindQuery>) -> Response {
    let text = find_text(&q.q);
    if text.is_empty() {
        return error(StatusCode::BAD_REQUEST, EMPTY_FIND);
    }
    let made = app.hub.with_mut(|c| {
        // The live state first, then the glob, as the TUI checks them.
        match &c.session.live {
            _ if c.live_ready() => {}
            crate::tui::app::views::LiveState::Closed(_) => {
                return Err(Refused::new(StatusCode::CONFLICT, FIND_CLOSED));
            }
            _ => return Err(Refused::new(StatusCode::CONFLICT, FIND_NOT_YET)),
        }
        if let Some(why) = find_refusal(&text) {
            return Err(Refused::new(StatusCode::UNPROCESSABLE_ENTITY, why));
        }
        let nodes = &c.session.nodes;
        let (walk, missing) = (0..nodes.len()).partition(|&i| nodes[i].connected);
        let glob = find_glob(&text);
        let l = start(
            c,
            KeyList {
                find: Some((text.clone(), glob)),
                walk,
                missing,
                ..KeyList::default()
            },
        );
        Ok(c.browse.lists.add(l))
    });
    finish(&app, made).await
}

/// The TUI's find wording, without its keys.
pub const FIND_NOT_YET: &str = "Still connecting: find a key once connected.";
pub const FIND_CLOSED: &str =
    "The live connection is closed, so keys can't be searched. Connect again in Connections.";

/// The text a find runs with, as the TUI's prompt takes it: control characters
/// dropped, at most [`crate::tui::app::find::MAX_FIND`] bytes, trimmed.
pub fn find_text(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > crate::tui::app::find::MAX_FIND {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// `GET /api/keys/more?list=<n>`: the next page of a list.
pub async fn more(State(app): State<App>, Q(q): Q<MoreQuery>) -> Response {
    let ready = app.hub.with_mut(|c| {
        let conn = c.id;
        let live = c.live_ready();
        let Some(l) = c.browse.lists.get_mut(q.list).filter(|l| l.conn == conn) else {
            return Err(Refused::new(StatusCode::NOT_FOUND, NO_LIST));
        };
        if let Some(why) = l.more_refused(live) {
            return Err(Refused::new(StatusCode::CONFLICT, why));
        }
        l.page_rows = 0;
        l.new_from = l.rows.len();
        Ok(l.id)
    });
    finish(&app, ready).await
}

/// A new list as the TUI's first page sets it up: sizes and LFU counters as the
/// permission check, the report and the policy allow.
fn start(c: &crate::web::session::Current, mut l: KeyList) -> KeyList {
    let allowed = |label: &str| {
        c.perms
            .as_ref()
            .and_then(|p| p.checks.iter().find(|x| x.label == label))
            .is_some_and(|x| matches!(x.status, PermStatus::Allowed))
    };
    l.conn = c.id;
    l.sizes = c
        .session
        .report
        .as_ref()
        .map_or(allowed(PERM_MEMORY), |r| r.sizes_available);
    l.lfu = c.session.server.as_ref().is_some_and(|s| s.lfu_enabled());
    l.freq = l.lfu && allowed(PERM_FREQ);
    if l.by_node() && l.walk.is_empty() {
        // No node of the cluster is connected: nothing to walk.
        l.done = true;
    }
    l
}

/// Loads a page of list `made` (when it has one to load) and answers with the list.
async fn finish(app: &App, made: Option<Result<u64, Refused>>) -> Response {
    let id = match made {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => return r.into_response(),
        Some(Ok(id)) => id,
    };
    load_page(app, id).await;
    match app.hub.with(|c| list_body(c, id)).flatten() {
        Some(b) => ok(b),
        None => error(StatusCode::NOT_FOUND, NO_LIST),
    }
}

/// Reads one page of list `id`, going on to the next node of a cluster while the page
/// is not full, as the TUI does.
///
/// The page is read in a task of its own, awaited here: when the request that asked
/// goes (the browser closed it), the page still lands in the list, which never stays
/// loading.
async fn load_page(app: &App, id: u64) {
    let app = app.clone();
    let _ = tokio::spawn(async move { load_page_now(&app, id).await }).await;
}

async fn load_page_now(app: &App, id: u64) {
    loop {
        let step = app
            .hub
            .with_mut(|c| {
                let conn = c.id;
                let l = c.browse.lists.get_mut(id)?;
                if l.done || l.loading {
                    return None;
                }
                let (dest, req) = l.next()?;
                l.loading = true;
                Some((conn, dest, req))
            })
            .flatten();
        let Some((conn, dest, req)) = step else {
            return;
        };
        let result = app.hub.request(Some(conn), dest, req).await;
        let again = app
            .hub
            .with_mut(|c| {
                let l = c.browse.lists.get_mut(id).filter(|l| l.conn == conn)?;
                l.loading = false;
                Some(match result {
                    Ok(LiveReply::Keys(Reply::Page(page))) => l.take(page),
                    Ok(_) => false,
                    Err(e) => {
                        l.failed(&e);
                        false
                    }
                })
            })
            .flatten();
        if again != Some(true) {
            return;
        }
    }
}

/// One key of a list.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KeyDto {
    #[serde(flatten)]
    pub key: Name,
    #[serde(rename = "type")]
    pub key_type: &'static str,
    /// `never`, or how long until it expires (`4h 2m`).
    pub ttl: String,
    /// Seconds until it expires; `null` when it never does.
    pub ttl_secs: Option<u64>,
    pub bytes: Option<u64>,
    pub encoding: Option<String>,
    pub length: Option<u64>,
    /// What `length` counts: `bytes`, `fields`, `items`, `members` or `entries`.
    pub length_unit: Option<&'static str>,
    pub freq: Option<u8>,
    /// The cluster node it was read on.
    pub node: Option<usize>,
}

impl KeyDto {
    pub fn of(r: &KeyRow, node: Option<usize>) -> Self {
        Self {
            key: Name::of(&r.key),
            key_type: r.key_type.as_str(),
            ttl: ttl_text(r.ttl),
            ttl_secs: ttl_secs(r.ttl),
            bytes: r.bytes,
            encoding: r.encoding.as_deref().map(crate::value::escape),
            length: r.length,
            length_unit: crate::analysis::length_unit(r.key_type),
            freq: r.freq,
            node,
        }
    }
}

/// `never`, or the TUI's duration (`4h 2m`).
pub fn ttl_text(t: Ttl) -> String {
    match t {
        Ttl::Persistent => "never".to_string(),
        Ttl::Expires(d) => crate::format::duration(d),
    }
}

pub fn ttl_secs(t: Ttl) -> Option<u64> {
    match t {
        Ttl::Persistent => None,
        Ttl::Expires(d) => Some(d.as_secs()),
    }
}

/// The body of a list: the rows the last page added (every row, after the first
/// page), from `new_from` on.
fn list_body(c: &crate::web::session::Current, id: u64) -> Option<serde_json::Value> {
    let l = c.browse.lists.lists.iter().find(|l| l.id == id)?;
    let rows: Vec<KeyDto> = l
        .rows
        .get(l.new_from..)
        .unwrap_or_default()
        .iter()
        .map(|r| KeyDto::of(r, l.row_node.get(&r.key).copied()))
        .collect();
    let node = |i: &usize| {
        c.session
            .nodes
            .get(*i)
            .map(|n| crate::value::escape(&n.endpoint.to_string()))
    };
    let refused: Vec<String> = l
        .refused
        .iter()
        .map(|e| crate::web::wording::server_free(&format!("Without it, a column is empty: {e}.")))
        .collect();
    Some(serde_json::json!({
        "list": l.id,
        "kind": if l.find.is_some() { "find" } else { "pattern" },
        "pattern": l.pattern.as_deref().map(Name::of),
        "find": l.find.as_ref().map(|(text, glob)| serde_json::json!({
            "text": crate::value::escape(text),
            "glob": crate::value::escape(glob),
        })),
        "glob": l
            .find
            .as_ref()
            .map(|(_, g)| g.clone())
            .or_else(|| l.pattern.as_deref().map(crate::analysis::match_glob))
            .map(|g| crate::value::escape(&g)),
        "rows": rows,
        "loaded": l.rows.len(),
        "new_from": l.new_from,
        "pages": l.pages,
        "done": l.done,
        "error": l.error.as_deref().map(crate::web::wording::server_free),
        "refused": refused,
        "other_pattern": l.other_pattern,
        "unreadable": l.unreadable,
        "sizes": l.sizes,
        "lfu": l.lfu,
        "freq": l.freq,
        "page_keys": PAGE_KEYS,
        "capped": l.rows.len() >= MAX_ROWS,
        "max_rows": MAX_ROWS,
        "cluster": l.by_node().then(|| serde_json::json!({
            "walk": l.walk,
            "missing": l.missing.iter().filter_map(node).collect::<Vec<_>>(),
            "nodes_done": l.nodes_done,
            "at": l.current_node(),
            "narrowed": l.narrowed,
        })),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::KeyType;
    use std::time::Duration;

    fn row(key: &str) -> KeyRow {
        KeyRow {
            key: key.into(),
            key_type: KeyType::Hash,
            ttl: Ttl::Expires(Duration::from_secs(90)),
            bytes: Some(120),
            encoding: Some("listpack".into()),
            length: Some(3),
            freq: None,
        }
    }

    fn page(keys: &[&str], cursor: u64) -> Page {
        Page {
            rows: keys.iter().map(|k| row(k)).collect(),
            cursor,
            carry: Vec::new(),
            other_pattern: 0,
            unreadable: 0,
            refused: Vec::new(),
        }
    }

    #[test]
    fn find_text_drops_controls_and_cuts_at_the_tui_limit() {
        assert_eq!(find_text("  user:\x1b[1\n  "), "user:[1");
        let long = "é".repeat(400);
        let t = find_text(&long);
        assert!(t.len() <= crate::tui::app::find::MAX_FIND);
        assert!(t.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_find_with_too_many_stars_is_refused_in_the_tui_words() {
        let why = find_refusal("*a*b*c*d*e*f*g*h*i*").unwrap();
        assert!(why.contains("at most 8"), "{why}");
        assert!(find_refusal("**a**").is_none());
        assert_eq!(find_glob("cart"), "*cart*");
    }

    #[test]
    fn a_standalone_list_walks_one_cursor_and_dedupes() {
        let mut l = KeyList {
            pattern: Some("cart:{id}".into()),
            ..KeyList::default()
        };
        let (dest, req) = l.next().unwrap();
        assert_eq!(dest, Dest::node(0));
        assert!(matches!(
            req,
            LiveRequest::Keys(Request::Page { cursor: 0, .. })
        ));
        assert!(!l.take(page(&["cart:1", "cart:2"], 7)));
        assert!(!l.done);
        assert!(!l.take(page(&["cart:2", "cart:3"], 0)));
        assert!(l.done);
        assert_eq!(l.rows.len(), 3);
    }

    #[test]
    fn a_cluster_list_goes_on_to_the_next_node_while_the_page_is_not_full() {
        let mut l = KeyList {
            find: Some(("x".into(), "*x*".into())),
            walk: vec![0, 2],
            ..KeyList::default()
        };
        let (dest, _) = l.next().unwrap();
        assert_eq!(dest, Dest::node(0).follow(Follow::Never));
        assert!(l.take(page(&["x1"], 0)), "node 0 ended early: go on");
        let (dest, _) = l.next().unwrap();
        assert_eq!(dest, Dest::node(2).follow(Follow::Never));
        assert!(!l.take(page(&["x2"], 0)));
        assert!(l.done);
        assert_eq!(l.row_node.get("x2"), Some(&2));
        assert_eq!(l.nodes_done, 2);
    }

    #[test]
    fn a_full_list_reads_no_more_and_says_why() {
        let mut l = KeyList {
            pattern: Some("cart:{id}".into()),
            ..KeyList::default()
        };
        assert_eq!(l.more_refused(true), None);
        assert_eq!(l.more_refused(false), Some(super::super::NOT_LIVE));
        l.rows = (0..MAX_ROWS).map(|i| row(&format!("cart:{i}"))).collect();
        assert_eq!(l.more_refused(true), Some(LIST_FULL));
        l.loading = true;
        assert_eq!(l.more_refused(true), Some(LOADING));
        for t in [LIST_FULL, LOADING, LIST_DONE, NO_LIST, EMPTY_FIND, NO_NODE] {
            assert_eq!(crate::web::wording::banned(t), None, "{t}");
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
    }

    /// Small 2: a database switch drops the lists, but the numbers go on, so a list
    /// number from before never names one of the new database.
    #[test]
    fn list_numbers_go_on_after_a_database_switch() {
        let mut b = crate::web::api::browse::Browse::default();
        let first = b.lists.add(KeyList::default());
        b.new_run(true);
        assert!(b.lists.get_mut(first).is_none());
        assert!(b.lists.add(KeyList::default()) > first);
    }

    #[test]
    fn a_refused_scan_says_so_in_fixed_words() {
        let mut l = KeyList {
            find: Some(("x".into(), "*x*".into())),
            ..KeyList::default()
        };
        l.failed(&LiveError::Denied("SCAN".into()));
        assert_eq!(
            l.error.as_deref(),
            Some("The server refused SCAN for this user, so keys can't be searched.")
        );
    }

    #[test]
    fn a_key_row_names_its_key_losslessly() {
        let mut r = row("bad\x07key");
        r.ttl = Ttl::Persistent;
        let d = KeyDto::of(&r, Some(1));
        assert_eq!(d.key.name, "bad\\x07key");
        assert_eq!(d.key.copy, None);
        assert_eq!(keyname::decode(&d.key.id).unwrap(), "bad\x07key");
        assert_eq!(d.ttl, "never");
        assert_eq!(d.ttl_secs, None);
        assert_eq!(d.length_unit, Some("fields"));
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["type"], "hash");
        assert_eq!(v["name"], "bad\\x07key");
    }
}
