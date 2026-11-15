//! Browse › Search (spec "#### Browse › Search", the TUI's `6 Search`): the indexes of the
//! picked node (`FT._LIST`, then `FT.INFO` each), its queries (`FT.SEARCH`, `FT.EXPLAIN`,
//! `FT.PROFILE`, as `live::search` sends them: `LIMIT 0 50`), and the TUI's two Search
//! writes, `FT.CREATE` from its form and `FT.DROPINDEX` without `DD` (the documents stay),
//! each through the write gate.
//!
//! As in the TUI: the list is read when the view is shown, and on a refresh; a server
//! without the module (`FT._LIST` unknown) or a user it refuses turns the panel off with
//! the TUI's words (not asked again until a refresh); one query runs at a time; a query command the server refused or lacks is not
//! sent again until a refresh; an empty query is `*`; a query is one line of at most
//! 1,000 characters. A query the server could not run keeps the server's message
//! (escaped and cut by `live::search::query_error`), the one place where the server's
//! words reach the page, because they describe the query the user typed.
//!
//! The results of the last query are kept here (at most 50, values as [`Raw`], never
//! logged), and handed out a page at a time; a field's whole value is drawn on request,
//! as a key's value is.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::values::{Shown, WIDTH, format_of};
use super::{App, Body, Q, Refused, error, ok};
use crate::live::search::{Doc, IndexInfo, IndexList, MAX_RESULTS, Reply, Request, Results, Text};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest};
use crate::tui::app::ConnId;
use crate::tui::app::views::search::{Cmd, MAX_QUERY, NO_MODULE, create_op, failure_text};
use crate::tui::app::views::streams::form::{Field, Form};
use crate::tui::app::views::streams::off_text;
use crate::tui::ui::search::{left_out, module_name, progress_cell};
use crate::web::keyname::{self, Name, bytes_text};
use crate::web::session::Current;
use crate::write::WriteOp;

/// Results handed out at once.
pub const PAGE: usize = 10;

/// The no-module notice, as the TUI's.
pub const NOT_LOADED: &str = "The Search module is not loaded on this server.";
pub const NOT_LOADED_WHY: &str = "FT._LIST is unknown, so there is no Redis Search (built into Redis 8) or valkey-search to list indexes from.";
/// An empty list.
pub const NO_INDEXES: &str = "The module has no indexes.";
/// `FT.INFO` was not read.
pub const NO_INFO: &str = "FT.INFO is not available for this user: only the name is known.";
/// No fields came back.
pub const NO_FIELDS: &str = "The module returned no fields.";
/// No result matched.
pub const NO_MATCH: &str = "No documents match the query.";
/// The index is not in the list read.
pub const NOT_LISTED: &str = "That index is not in the list: read the indexes again.";
/// A query too long.
pub const TOO_LONG: &str = "A query is at most 1,000 characters.";
/// Results of an older query, or none.
pub const STALE: &str = "Those results are not the last query's: run it again.";
/// A result or field that is not there.
pub const NO_DOC: &str = "There is no such result, or no such field in it.";
/// Why `cmd` was given a query.
pub const LAST_ONLY: &str = "FT.EXPLAIN and FT.PROFILE use the query last run: send no query.";
/// Why the query command is not known.
pub const BAD_CMD: &str = "the command must be search, explain or profile";
/// Why a create's `on` is refused.
pub const BAD_ON: &str = "an index is on HASH or JSON";

/// The last query's results, on the index and node it ran on.
#[derive(Debug)]
struct Run {
    id: u64,
    index: String,
    query: String,
    cmd: Cmd,
    results: Results,
}

/// What Browse › Search knows of the connection's picked node (reset when another node
/// is picked, or the session starts a new run).
#[derive(Debug, Default)]
pub struct Search {
    list: Option<IndexList>,
    /// `FT._LIST` unknown: the server has no module.
    no_module: bool,
    /// `FT._LIST` refused, in redis-lens's words.
    off: Option<String>,
    /// Query commands found refused or missing, with why.
    cmd_off: Vec<(Cmd, String)>,
    last_query: Option<String>,
    /// The query command running, one at a time, and its request's token ([`Busy`]).
    busy: Option<(u64, Cmd)>,
    run: Option<Run>,
    next_run: u64,
}

impl Search {
    /// Nothing is known: no list, no query, no results (a pick or a new run reset it).
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_none()
            && !self.no_module
            && self.off.is_none()
            && self.cmd_off.is_empty()
            && self.last_query.is_none()
            && self.busy.is_none()
            && self.run.is_none()
    }

    fn off_reason(&self, cmd: Cmd) -> Option<&str> {
        self.cmd_off
            .iter()
            .find(|(c, _)| *c == cmd)
            .map(|(_, why)| why.as_str())
    }
}

fn cmd_of(name: &str) -> Option<Cmd> {
    match name {
        "search" => Some(Cmd::Search),
        "explain" => Some(Cmd::Explain),
        "profile" => Some(Cmd::Profile),
        _ => None,
    }
}

fn cmd_name(c: Cmd) -> &'static str {
    match c {
        Cmd::Search => "search",
        Cmd::Explain => "explain",
        Cmd::Profile => "profile",
    }
}

/// The module this server runs, for the reasons.
fn module(c: &Current) -> &'static str {
    module_name(c.session.server.as_ref().map(|s| s.flavor))
}

/// Why the panel is off, as the TUI says it: the permission check first, then what the
/// last read found.
fn panel_off(c: &Current) -> Option<serde_json::Value> {
    let probe_skipped_module = c.perms.as_ref().is_some_and(|p| {
        p.checks.iter().any(|x| {
            x.label == crate::conn::PERM_SEARCH
                && matches!(&x.status, crate::model::PermStatus::Skipped(w) if w.contains("Search module"))
        })
    });
    if let Some(why) = super::streams::view_off(c, crate::conn::PERM_SEARCH) {
        return Some(serde_json::json!({
            "off": crate::web::wording::server_free(&why),
            "no_module": probe_skipped_module,
            "detail": probe_skipped_module.then_some(NOT_LOADED_WHY),
        }));
    }
    if c.search.no_module {
        return Some(serde_json::json!({
            "off": NOT_LOADED, "no_module": true, "detail": NOT_LOADED_WHY,
        }));
    }
    c.search.off.as_ref().map(|why| {
        serde_json::json!({ "off": crate::web::wording::server_free(why), "no_module": false, "detail": null })
    })
}

fn index_json(i: &IndexInfo, module: &str) -> serde_json::Value {
    let info = i.docs.is_some() || !i.schema.is_empty() || i.on.is_some();
    let (progress, building) = progress_cell(i);
    let gaps = left_out(i, module);
    serde_json::json!({
        "name": Name::of(&i.name),
        "info": info,
        "info_text": (!info).then_some(NO_INFO),
        "on": i.on.as_deref().map(crate::value::escape),
        "prefixes": i.prefixes.iter().map(|p| crate::value::escape(p)).collect::<Vec<_>>(),
        "docs": i.docs,
        "bytes": i.bytes,
        "indexing": i.indexing,
        "progress": i.progress,
        "progress_text": progress,
        "building": building,
        "failures": i.failures,
        "left_out": (!gaps.is_empty()).then(|| format!("left out: {}", gaps.join(", "))),
        "schema": i.schema.iter().map(|f| serde_json::json!({
            "name": crate::value::escape(&f.name),
            "path": (f.identifier != f.name).then(|| crate::value::escape(&f.identifier)),
            "type": crate::value::escape(&f.ty),
            "flags": f.flags.iter().map(|x| crate::value::escape(x)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "fields_text": (info && i.schema.is_empty()).then_some(NO_FIELDS),
    })
}

/// The list reply.
fn list_json(c: &Current, list: &IndexList, error: Option<String>) -> serde_json::Value {
    let m = module(c);
    let s = &c.search;
    let node = c.session.is_cluster().then_some(c.session.picked);
    serde_json::json!({
        "module": m,
        "node": node,
        "node_title": c.session.node_title().map(|t| crate::value::escape(&t)),
        "indexes": list.indexes.iter().map(|i| index_json(i, m)).collect::<Vec<_>>(),
        "total": list.total,
        "first_of": (list.total > list.indexes.len()).then(|| format!(
            "first {} of {} indexes", list.indexes.len(), crate::format::grouped(list.total as u64)
        )),
        "notes": list.notes.iter().map(|n| crate::web::wording::server_free(n)).collect::<Vec<_>>(),
        "empty": list.indexes.is_empty().then(|| serde_json::json!({
            "title": NO_INDEXES,
            "text": format!("{m} is loaded; nothing was created with FT.CREATE yet."),
        })),
        "error": error.map(|e| crate::web::wording::server_free(&format!("{e}; showing the previous reading."))),
        "cmd_off": {
            "search": s.off_reason(Cmd::Search).map(crate::web::wording::server_free),
            "explain": s.off_reason(Cmd::Explain).map(crate::web::wording::server_free),
            "profile": s.off_reason(Cmd::Profile).map(crate::web::wording::server_free),
        },
        // As the page says it: `FT.SEARCH is still running`.
        "busy": s.busy.map(|(_, c)| c.name()),
        "last_query": s.last_query.as_deref().map(crate::value::escape),
        "last_query_copy": s.last_query.clone().filter(|q| crate::value::escape(q) == *q),
        "results": s.run.as_ref().map(|r| results_json(r, 0)),
        "writes": writes_off(c),
    })
}

/// Why the Search writes are off, as the panel leaves its buttons out.
fn writes_off(c: &Current) -> Option<String> {
    let p = c.profile();
    if p.is_read_only() {
        return Some(crate::web::wording::server_free_with(
            &crate::write::read_only_reason(p),
            &super::writes::profile_data(p),
        ));
    }
    c.search.no_module.then(|| NO_MODULE.to_string())
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// `1`: read again, and ask again for what was refused (the TUI's `r`).
    #[serde(default)]
    refresh: Option<u8>,
}

enum Step {
    Answer(Response),
    Read { run: (ConnId, u64), node: usize },
}

/// `GET /api/search[?refresh=1]`: the picked node's indexes, read each time (the page asks
/// when the view is shown, and after a write of this panel), as the TUI reads them when
/// its view is shown; `refresh=1` reads again and forgets what was refused. `{"off": …,
/// "no_module": bool, "detail": …}` without the module or when `FT._LIST` is refused, not
/// asked again until a refresh. A reply that comes back on another run or node than it
/// was asked on stores nothing and answers [`super::writes::CHANGED`].
pub async fn list(State(app): State<App>, Q(q): Q<ListQuery>) -> Response {
    let refresh = q.refresh == Some(1);
    let step = app.hub.with_mut(|c| {
        if !c.live_ready() {
            return Step::Answer(error(StatusCode::CONFLICT, super::NOT_LIVE));
        }
        if refresh {
            c.search.off = None;
            c.search.no_module = false;
            c.search.cmd_off.clear();
        }
        if let Some(off) = panel_off(c) {
            return Step::Answer(ok(off));
        }
        Step::Read {
            run: super::writes::run_of(c),
            node: c.session.picked,
        }
    });
    let (run, node) = match step {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Step::Answer(r)) => return r,
        Some(Step::Read { run, node }) => (run, node),
    };
    let conn = run.0;
    let result = app
        .hub
        .request(
            Some(conn),
            Dest::node(node),
            LiveRequest::Search(Request::Indexes),
        )
        .await;
    let body = app.hub.with_mut(|c| {
        // Answered on another run or node: nothing of it is kept.
        if super::writes::run_of(c) != run || c.session.picked != node {
            return error(StatusCode::CONFLICT, super::writes::CHANGED);
        }
        match result {
            Ok(LiveReply::Search(Reply::Indexes(list))) => {
                c.search.no_module = false;
                let body = list_json(c, &list, None);
                c.search.list = Some(list);
                ok(body)
            }
            Ok(_) => error(StatusCode::BAD_GATEWAY, super::SHAPE),
            Err(LiveError::Unsupported(_)) => {
                c.search.no_module = true;
                c.search.list = None;
                ok(panel_off(c))
            }
            Err(e @ LiveError::Denied(_)) => {
                c.search.off = Some(off_text(&e));
                ok(panel_off(c))
            }
            Err(LiveError::Closed) => error(StatusCode::CONFLICT, super::NOT_LIVE),
            Err(e) => match &c.search.list {
                Some(list) => ok(list_json(c, list, Some(e.to_string()))),
                None => super::live_error(&e),
            },
        }
    });
    body.unwrap_or_else(|| error(StatusCode::CONFLICT, super::NOT_LIVE))
}

/// A query as the TUI's input takes it: one line (a newline or tab is a space, other
/// control characters dropped), `*` when empty, at most [`MAX_QUERY`] characters.
pub fn query_of(typed: &str) -> Result<String, &'static str> {
    let mut q = String::new();
    for c in typed.chars() {
        match c {
            '\n' | '\r' | '\t' => q.push(' '),
            c if c.is_control() => {}
            c => q.push(c),
        }
    }
    if q.chars().count() > MAX_QUERY {
        return Err(TOO_LONG);
    }
    Ok(if q.trim().is_empty() {
        "*".to_string()
    } else {
        q
    })
}

/// What a page of results says: the query, how many, how long, and `PAGE` documents
/// from `from` with their first fields previewed.
fn results_json(r: &Run, from: usize) -> serde_json::Value {
    let docs: Vec<serde_json::Value> = r
        .results
        .docs
        .iter()
        .enumerate()
        .skip(from)
        .take(PAGE)
        .map(|(i, d)| doc_json(i, d))
        .collect();
    let took = match r.results.took.as_millis() {
        0 => "<1 ms".to_string(),
        n => format!("{} ms", crate::format::grouped(n as u64)),
    };
    let total = r.results.total;
    serde_json::json!({
        "run": r.id,
        "cmd": cmd_name(r.cmd),
        "index": Name::of(&r.index),
        "query": crate::value::escape(&r.query),
        "total": total,
        "count": format!(
            "{} match{}",
            crate::format::grouped(total),
            if total == 1 { "" } else { "es" }
        ),
        "took": took,
        "kept": r.results.docs.len(),
        "max": MAX_RESULTS,
        "from": from,
        "page": PAGE,
        "docs": docs,
        "empty": r.results.docs.is_empty().then_some(NO_MATCH),
    })
}

fn doc_json(i: usize, d: &Doc) -> serde_json::Value {
    serde_json::json!({
        "index": i,
        "key": Name::of(&d.key),
        "field_count": d.fields.len(),
        "fields": d.fields.iter().map(|(n, v)| serde_json::json!({
            "name": bytes_text(n.as_bytes()),
            "value": crate::value::preview(v.as_bytes(), super::values::PREVIEW),
        })).collect::<Vec<_>>(),
    })
}

fn text_json(t: &Text) -> serde_json::Value {
    serde_json::json!({
        "lines": t.lines(),
        "empty": t.is_empty().then_some("The server returned nothing."),
    })
}

/// `POST /api/search/query` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    /// The index's id (hex), from the list.
    index: String,
    /// `search`, `explain` or `profile`.
    cmd: String,
    /// For `search`: the query as typed. `explain` and `profile` use the last one.
    query: Option<String>,
}

/// The query is the user's text: `Debug` leaves it out.
impl std::fmt::Debug for Query {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Query")
            .field("cmd", &self.cmd)
            .finish_non_exhaustive()
    }
}

/// The token of each query request, for every connection and node: a query of an
/// earlier node or run never clears a later one's slot ([`Busy`]).
static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);

/// Clears the running query when its request ends, however it ends: only its own, so a
/// query asked after a pick or a new run (which reset the slot) keeps its slot.
struct Busy {
    hub: Arc<crate::web::session::Hub>,
    conn: ConnId,
    token: u64,
}

impl Drop for Busy {
    fn drop(&mut self) {
        let (conn, token) = (self.conn, self.token);
        self.hub.with_mut(|c| {
            if c.id == conn && c.search.busy.is_some_and(|(t, _)| t == token) {
                c.search.busy = None;
            }
        });
    }
}

/// `POST /api/search/query`: `FT.SEARCH` with the query typed, or `FT.EXPLAIN` /
/// `FT.PROFILE` with the query last run, on an index of the list, on the picked node.
/// One at a time (409 while one runs). A command refused or missing answers
/// `{"off": …}` and is not sent again until a refresh. Search and profile keep their
/// results: the first page comes with the reply (`results`), the rest from
/// `GET /api/search/results`; explain and profile answer `text` too.
pub async fn query(State(app): State<App>, Body(b): Body<Query>) -> Response {
    let Some(cmd) = cmd_of(&b.cmd) else {
        return error(StatusCode::BAD_REQUEST, BAD_CMD);
    };
    let index = match keyname::decode(&b.index) {
        Ok(i) => i,
        Err(r) => return r.into_response(),
    };
    let typed = match (cmd, b.query.as_deref()) {
        (Cmd::Search, Some(q)) => match query_of(q) {
            Ok(q) => Some(q),
            Err(why) => return error(StatusCode::UNPROCESSABLE_ENTITY, why),
        },
        (Cmd::Search, None) => Some("*".to_string()),
        (_, Some(_)) => return error(StatusCode::UNPROCESSABLE_ENTITY, LAST_ONLY),
        (_, None) => None,
    };
    let made = app.hub.with_mut(|c| {
        if !c.live_ready() {
            return Err(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE));
        }
        if let Some(off) = panel_off(c) {
            return Err(Refused::new(
                StatusCode::OK,
                off["off"].as_str().unwrap_or_default().to_string(),
            ));
        }
        if let Some((_, running)) = c.search.busy {
            return Err(Refused::new(
                StatusCode::CONFLICT,
                format!("{} is still running: wait for its answer.", running.name()),
            ));
        }
        let listed = c
            .search
            .list
            .as_ref()
            .is_some_and(|l| l.indexes.iter().any(|i| i.name == index));
        if !listed {
            return Err(Refused::new(StatusCode::NOT_FOUND, NOT_LISTED));
        }
        if let Some(why) = c.search.off_reason(cmd) {
            return Err(Refused::new(StatusCode::OK, why.to_string()));
        }
        let query = match typed.clone() {
            Some(q) => q,
            None => c.search.last_query.clone().ok_or_else(|| {
                Refused::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("Run a query first: {} uses the query last run.", cmd.name()),
                )
            })?,
        };
        if cmd == Cmd::Search {
            c.search.last_query = Some(query.clone());
        }
        let token = NEXT_QUERY.fetch_add(1, Ordering::Relaxed);
        c.search.busy = Some((token, cmd));
        Ok((super::writes::run_of(c), c.session.picked, query, token))
    });
    let (run, node, query, token) = match made {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) if r.status == StatusCode::OK => return super::off(&r.text),
        Some(Err(r)) => return r.into_response(),
        Some(Ok(m)) => m,
    };
    let conn = run.0;
    let busy = Busy {
        hub: Arc::clone(&app.hub),
        conn,
        token,
    };
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
    // In a task of its own: a request the browser drops still lands, and frees the slot.
    let hub = Arc::clone(&app.hub);
    let result = tokio::spawn(async move {
        let r = hub
            .request(Some(conn), Dest::node(node), LiveRequest::Search(req))
            .await;
        drop(busy);
        r
    })
    .await
    .unwrap_or(Err(LiveError::Closed));
    let body = app.hub.with_mut(|c| {
        // Answered on another run or node: nothing of it is kept.
        if super::writes::run_of(c) != run || c.session.picked != node {
            return error(StatusCode::CONFLICT, super::writes::CHANGED);
        }
        answered(c, cmd, index, query, result)
    });
    body.unwrap_or_else(|| error(StatusCode::CONFLICT, super::NOT_LIVE))
}

/// The TUI's `query_reply`.
fn answered(
    c: &mut Current,
    cmd: Cmd,
    index: String,
    query: String,
    result: Result<LiveReply, LiveError>,
) -> Response {
    let keep = |c: &mut Current, results: Results| -> serde_json::Value {
        c.search.next_run += 1;
        let run = Run {
            id: c.search.next_run,
            index: index.clone(),
            query: query.clone(),
            cmd,
            results,
        };
        let body = results_json(&run, 0);
        c.search.run = Some(run);
        body
    };
    match result {
        Ok(LiveReply::Search(Reply::Results(r))) => {
            let results = keep(c, r);
            ok(serde_json::json!({ "cmd": cmd_name(cmd), "results": results, "text": null }))
        }
        Ok(LiveReply::Search(Reply::Explain(t))) => ok(serde_json::json!({
            "cmd": cmd_name(cmd),
            "index": Name::of(&index),
            "query": crate::value::escape(&query),
            "results": null,
            "text": text_json(&t),
        })),
        Ok(LiveReply::Search(Reply::Profile { results, profile })) => {
            let results = keep(c, results);
            ok(serde_json::json!({
                "cmd": cmd_name(cmd),
                "index": Name::of(&index),
                "query": crate::value::escape(&query),
                "results": results,
                "text": text_json(&profile),
            }))
        }
        Ok(_) => error(StatusCode::BAD_GATEWAY, super::SHAPE),
        Err(LiveError::Closed) => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Err(e @ (LiveError::Denied(_) | LiveError::Unsupported(_))) => {
            let why = failure_text(&e);
            if c.search.off_reason(cmd).is_none() {
                c.search.cmd_off.push((cmd, why.clone()));
            }
            super::off(&why)
        }
        Err(e) => error(StatusCode::UNPROCESSABLE_ENTITY, &query_failed(cmd, &e)),
    }
}

/// Why a query failed, for the page: the server's message about the query as
/// `live::search` kept it, unless it reads as a server's error reply anyway, which the
/// dashboard never shows; then a fixed sentence.
pub fn query_failed(cmd: Cmd, e: &LiveError) -> String {
    let why = failure_text(e);
    let free = crate::web::wording::server_free(&why);
    if free == crate::web::wording::SERVER_REFUSED || free == crate::web::wording::FALLBACK {
        return format!("{} failed: the server could not run the query.", cmd.name());
    }
    free
}

#[derive(Debug, Deserialize)]
pub struct ResultsQuery {
    run: u64,
    #[serde(default)]
    from: usize,
}

/// `GET /api/search/results?run=<n>&from=<i>`: a page of the last query's results.
pub async fn results(State(app): State<App>, Q(q): Q<ResultsQuery>) -> Response {
    let body = app.hub.with(|c| match &c.search.run {
        Some(r) if r.id == q.run => ok(results_json(r, q.from.min(r.results.docs.len()))),
        _ => error(StatusCode::CONFLICT, STALE),
    });
    body.unwrap_or_else(|| error(StatusCode::CONFLICT, super::NOT_LIVE))
}

#[derive(Debug, Deserialize)]
pub struct FieldQuery {
    run: u64,
    doc: usize,
    field: usize,
    format: Option<String>,
    width: Option<usize>,
}

/// `GET /api/search/field?run=<n>&doc=<i>&field=<j>[&format=<f>][&width=<w>]`: one field
/// of a result, drawn whole in a format, as a key's value is (`/api/value/item`).
pub async fn field(State(app): State<App>, Q(q): Q<FieldQuery>) -> Response {
    let picked = match q.format.as_deref().map(format_of) {
        Some(None) => return error(StatusCode::BAD_REQUEST, super::values::BAD_FORMAT),
        Some(Some(f)) => Some(f),
        None => None,
    };
    let width = q.width.unwrap_or(WIDTH).clamp(16, 400);
    let body = app.hub.with(|c| {
        let Some(r) = c.search.run.as_ref().filter(|r| r.id == q.run) else {
            return error(StatusCode::CONFLICT, STALE);
        };
        let Some(doc) = r.results.docs.get(q.doc) else {
            return error(StatusCode::NOT_FOUND, NO_DOC);
        };
        let Some((name, value)) = doc.fields.get(q.field) else {
            return error(StatusCode::NOT_FOUND, NO_DOC);
        };
        let shown = Shown::of("value", value, picked, width, None);
        ok(serde_json::json!({
            "run": r.id,
            "doc": q.doc,
            "field": q.field,
            "key": Name::of(&doc.key),
            "name": bytes_text(name.as_bytes()),
            "shown": shown,
        }))
    });
    body.unwrap_or_else(|| error(StatusCode::CONFLICT, super::NOT_LIVE))
}

/// The checks of a Search write before the gate: the gate's own refusal for a read-only
/// profile, a bulk action, the live connection, then the module.
fn writable(c: &Current) -> Result<(), Refused> {
    super::edits::writable(c)?;
    if c.search.no_module || panel_off(c).is_some_and(|o| o["no_module"].as_bool().unwrap_or(false))
    {
        return Err(Refused::new(StatusCode::UNPROCESSABLE_ENTITY, NO_MODULE));
    }
    if let Some(off) = panel_off(c) {
        return Err(Refused::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            off["off"].as_str().unwrap_or_default().to_string(),
        ));
    }
    Ok(())
}

/// Builds a Search write from what the picked node lists, then sends it through the one
/// write flow, for the connection, run and node it was made on: a node picked since (in
/// the profile's read, or before the question is answered) refuses it with
/// [`super::writes::CHANGED`], so it never goes to a node it was not made for. The page
/// reads the list again after it, as the TUI does after a write.
async fn flow(app: &App, make: impl FnOnce(&Current) -> Result<WriteOp, Refused>) -> Response {
    app.hub.follow_saved_profile().await;
    let made = app.hub.with(|c| {
        writable(c)?;
        let op = make(c)?;
        Ok::<_, Refused>((op, super::writes::MadeFor::on_picked(c)))
    });
    match made {
        None => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => r.into_response(),
        Some(Ok((op, made_for))) => {
            super::writes::write_flow_then(app, op, None, Some(made_for)).await
        }
    }
}

/// `POST /api/search/create` body: the TUI's create form, as typed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    name: String,
    /// `HASH` or `JSON`.
    on: String,
    /// Prefixes separated by spaces or commas; empty: every key.
    #[serde(default)]
    prefixes: String,
    /// `name TYPE, …`; on JSON `$.path AS alias TYPE`.
    fields: String,
}

/// `POST /api/search/create`: the TUI's `C`, `FT.CREATE` checked as its form checks it
/// (`views::search::create_op`), on the picked node.
pub async fn create(State(app): State<App>, Body(b): Body<Create>) -> Response {
    if b.on != "HASH" && b.on != "JSON" {
        return error(StatusCode::BAD_REQUEST, BAD_ON);
    }
    flow(&app, |_| {
        let form = Form::new(
            "Create an index",
            Vec::new(),
            vec![
                Field::text("name", "").with(b.name.clone()),
                Field::choice("on", &["HASH", "JSON"], "").with(b.on.clone()),
                Field::text("prefixes", "")
                    .with(b.prefixes.clone())
                    .as_list(),
                Field::text("fields", "").with(b.fields.clone()).as_list(),
            ],
        );
        create_op(&form).map_err(|why| Refused::new(StatusCode::UNPROCESSABLE_ENTITY, why))
    })
    .await
}

/// `POST /api/search/drop` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropBody {
    /// The index's id (hex), from the list.
    index: String,
}

/// `POST /api/search/drop`: the TUI's `D`, `FT.DROPINDEX` without `DD`: the documents
/// stay. Destructive: the gate asks, and on `prod` the index's name is typed.
pub async fn drop_index(State(app): State<App>, Body(b): Body<DropBody>) -> Response {
    let index = match keyname::decode(&b.index) {
        Ok(i) => i,
        Err(r) => return r.into_response(),
    };
    flow(&app, |c| {
        let listed = c
            .search
            .list
            .as_ref()
            .is_some_and(|l| l.indexes.iter().any(|i| i.name == index));
        if !listed {
            return Err(Refused::new(StatusCode::NOT_FOUND, NOT_LISTED));
        }
        Ok(WriteOp::FtDropIndex {
            index: index.clone(),
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::search::FieldInfo;
    use crate::value::Raw;
    use std::time::Duration;

    fn index() -> IndexInfo {
        IndexInfo {
            name: "idx:products".into(),
            on: Some("HASH".into()),
            prefixes: vec!["product:".into()],
            docs: Some(1200),
            bytes: None,
            indexing: Some(false),
            progress: Some(1.0),
            failures: Some(0),
            schema: vec![FieldInfo {
                identifier: "$.title".into(),
                name: "title".into(),
                ty: "TEXT".into(),
                flags: vec!["SORTABLE".into()],
            }],
        }
    }

    #[test]
    fn an_index_reads_as_the_tuis_details() {
        let v = index_json(&index(), "valkey-search");
        assert_eq!(v["name"]["name"], "idx:products");
        assert_eq!(v["info"], true);
        assert_eq!(v["progress_text"], "100%");
        assert_eq!(v["building"], false);
        assert_eq!(
            v["left_out"],
            "left out: size (valkey-search does not report it)"
        );
        assert_eq!(v["schema"][0]["path"], "$.title");
        assert_eq!(v["schema"][0]["flags"][0], "SORTABLE");
        let bare = IndexInfo {
            name: "only".into(),
            on: None,
            prefixes: vec![],
            docs: None,
            bytes: None,
            indexing: None,
            progress: None,
            failures: None,
            schema: vec![],
        };
        let v = index_json(&bare, "Redis Search");
        assert_eq!(v["info_text"], NO_INFO);
        assert_eq!(v["fields_text"], serde_json::Value::Null);
    }

    #[test]
    fn a_query_is_one_line_star_when_empty_and_at_most_1000_characters() {
        assert_eq!(query_of("").unwrap(), "*");
        assert_eq!(query_of("  ").unwrap(), "*");
        assert_eq!(query_of("@title:(red\nshoe)").unwrap(), "@title:(red shoe)");
        assert_eq!(query_of("a\x1bb").unwrap(), "ab");
        assert_eq!(query_of(&"x".repeat(1000)).unwrap().len(), 1000);
        assert_eq!(query_of(&"x".repeat(1001)), Err(TOO_LONG));
    }

    fn run(docs: usize) -> Run {
        Run {
            id: 3,
            index: "idx".into(),
            query: "*".into(),
            cmd: Cmd::Search,
            results: Results {
                total: 120,
                docs: (0..docs)
                    .map(|i| Doc {
                        key: format!("product:{i}"),
                        fields: vec![(Raw::from("title"), Raw::from("red\x1b[31m shoe"))],
                    })
                    .collect(),
                took: Duration::from_millis(1234),
            },
        }
    }

    #[test]
    fn results_are_paged_and_previews_escaped() {
        let r = run(23);
        let first = results_json(&r, 0);
        assert_eq!(first["docs"].as_array().unwrap().len(), PAGE);
        assert_eq!(first["kept"], 23);
        assert_eq!(first["count"], "120 matches");
        assert_eq!(first["took"], "1,234 ms");
        let last = results_json(&r, 20);
        assert_eq!(last["docs"].as_array().unwrap().len(), 3);
        assert_eq!(last["docs"][0]["index"], 20);
        assert_eq!(last["docs"][0]["key"]["name"], "product:20");
        assert!(!last.to_string().contains('\x1b'));
        assert_eq!(results_json(&run(0), 0)["empty"], NO_MATCH);
    }

    #[test]
    fn a_query_error_keeps_the_servers_words_about_the_query_only() {
        let e = LiveError::Failed("FT.SEARCH failed: Syntax error at offset 3 near shoe".into());
        assert_eq!(
            query_failed(Cmd::Search, &e),
            "FT.SEARCH failed: Syntax error at offset 3 near shoe."
        );
        // Text that reads as a server's reply is never shown.
        let e = LiveError::Failed("MISCONF something".into());
        assert_eq!(
            query_failed(Cmd::Profile, &e),
            "FT.PROFILE failed: the server could not run the query."
        );
    }

    #[test]
    fn the_create_form_is_the_tuis() {
        let form = Form::new(
            "",
            Vec::new(),
            vec![
                Field::text("name", "").with("idx:p"),
                Field::choice("on", &["HASH", "JSON"], "").with("JSON"),
                Field::text("prefixes", "").with("p:, q:").as_list(),
                Field::text("fields", "").with("title TEXT").as_list(),
            ],
        );
        assert_eq!(
            create_op(&form).unwrap_err(),
            "On JSON a field is a path such as $.title: title."
        );
    }

    #[test]
    fn the_words_are_the_dashboards_own() {
        for t in [
            NOT_LOADED,
            NOT_LOADED_WHY,
            NO_INDEXES,
            NO_INFO,
            NO_FIELDS,
            NO_MATCH,
            NOT_LISTED,
            TOO_LONG,
            STALE,
            NO_DOC,
            LAST_ONLY,
            NO_MODULE,
            "FT.SEARCH is still running: wait for its answer.",
            "Run a query first: FT.EXPLAIN uses the query last run.",
            "The server refused FT._LIST for this user, so this view is off.",
            "FT._LIST is not available: the Search module is not loaded.",
        ] {
            assert_eq!(crate::web::wording::banned(t), None, "{t}");
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
    }
}
