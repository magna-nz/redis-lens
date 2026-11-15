//! Browse › Streams (spec "#### Browse › Streams", the TUI's `4 Streams`): the streams the
//! analysis found, each read on the node that owns it with `XINFO STREAM`, `XINFO GROUPS`
//! and `XINFO CONSUMERS` (`live::streams`), with the TUI's verdicts; a group's (or a
//! consumer's) pending entries with `XPENDING`; and the TUI's three stream writes:
//! acknowledge a pending entry (`XACK`), claim a group's pending entries for another
//! consumer (`XAUTOCLAIM`) and trim the stream (`XTRIM`), each through the write gate.
//!
//! The entries themselves are the stream's value (`GET /api/value?key=<id>&type=stream`):
//! newest first with `XREVRANGE`, a page of 100 at a time, read again by opening it again.
//!
//! Reads only from here, but for the writes, which are the TUI's ops
//! (`views::streams::claim_op`, `trim_op`) sent by `writes::write_flow`. No entry value is
//! held here: `live::streams` reads IDs only.

use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{App, Body, Q, Refused, error, ok};
use crate::live::streams::{
    ConsumerInfo, Fetched, GroupInfo, Lag, MAX_STREAMS, Pending, Refused as Skip, Reply, Request,
    STUCK_IDLE_MS, StreamInfo, Verdict, verdicts,
};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest};
use crate::model::PermStatus;
use crate::tui::app::ConnId;
use crate::tui::app::views::streams::form::{Field, Form};
use crate::tui::app::views::streams::{
    CLAIM_COUNT, CLAIM_IDLE_MS, claim_op, is_stream_id, off_text, stream_keys, streams_by_node,
    trim_op,
};
use crate::tui::ui::streams::{age, left_out, max_lag, total_pending, verdict_reason};
use crate::web::keyname::{self, Name};
use crate::web::session::Current;
use crate::write::WriteOp;

/// A list read this recently is answered again without asking the server (two pages
/// open, or a page that came back), so the 5 s poll of each page costs one read.
const FRESH: Duration = Duration::from_secs(2);

/// What Browse › Streams knows of the connection: the last read, what the server refused
/// (not asked again until a refresh, as the TUI's `r`), and the pending list shown.
/// IDs and names only, never an entry's value.
#[derive(Debug, Default)]
pub struct Streams {
    rows: Vec<Fetched>,
    found: usize,
    loaded: bool,
    at: Option<Instant>,
    refused: Skip,
    /// `XINFO STREAM` refused or unknown: the panel is off until a refresh.
    off: Option<String>,
    /// Why the last read failed; the previous rows stay.
    error: Option<String>,
    /// The pending entries listed last, of which stream.
    pending: Option<(String, Pending)>,
}

impl Streams {
    /// Nothing is known: no rows, no refusal, no pending list (a new run reset it).
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
            && !self.loaded
            && self.at.is_none()
            && self.off.is_none()
            && self.error.is_none()
            && self.pending.is_none()
    }

    /// What the server refused, left out of each read until a refresh.
    #[cfg(test)]
    pub(crate) fn refused(&self) -> Skip {
        self.refused
    }
}

/// Before an analysis, as the TUI says it.
pub const NO_ANALYSIS: &str =
    "Streams are found by an analysis: it samples the keys and lists the streams among them.";
/// The analysis found none.
pub const NONE_FOUND: &str = "The analysis found no streams.";
/// Why it may have missed some.
pub const NONE_HINT: &str = "It samples keys, so it may miss some; a Deep analysis samples more.";
/// A key the analysis listed is gone.
pub const GONE: &str = "The key is gone, or no longer a stream.";
/// The stream named is not in the list.
pub const NOT_LISTED: &str =
    "That stream is not in the list: read the streams again, or analyze again to update it.";
/// The group named is not one of the stream's.
pub const NO_GROUP: &str = "That group is not one of the stream's: read the streams again.";
/// The entry is not in the pending list shown.
pub const NOT_PENDING_LISTED: &str =
    "That entry is not in the pending list shown: list the pending entries again.";
/// The ID is not one.
pub const BAD_ID: &str = "That is not a stream entry ID, such as 1700000000000-0.";
/// The groups are not read.
pub const GROUPS_OFF: &str = "XINFO GROUPS is not available for this user.";
/// The stream has no groups.
pub const NO_GROUPS: &str = "No consumer groups: nothing reads this stream.";
/// The trim form's note.
pub const APPROXIMATE: &str = "Approximate (~): the server may keep a few more.";

/// Why the view needing probe `label` is off, in the TUI's words, when the permission
/// check found it refused or not available.
pub(crate) fn view_off(c: &Current, label: &str) -> Option<String> {
    let check = c.perms.as_ref()?.checks.iter().find(|p| p.label == label)?;
    match &check.status {
        PermStatus::Allowed => None,
        PermStatus::Denied(_) => Some(format!(
            "The server refused {label} for this user, so this view is off."
        )),
        PermStatus::Skipped(why) => Some(format!(
            "{label} is not available: {}.",
            crate::value::escape(why)
        )),
    }
}

/// The node a stream lives on, on a cluster: its index and endpoint.
fn node_of(c: &Current, key: &str) -> (Option<usize>, Option<String>) {
    if !c.session.is_cluster() {
        return (None, None);
    }
    let i = c.session.owner_of(key.as_bytes());
    let endpoint = i
        .and_then(|i| c.session.nodes.get(i))
        .map(|n| crate::value::escape(&n.endpoint.to_string()));
    (i, endpoint)
}

fn lag_json(l: Lag) -> serde_json::Value {
    match l {
        Lag::Known(n) => serde_json::json!(n),
        Lag::Unknown | Lag::Unsupported => serde_json::Value::Null,
    }
}

fn consumer_json(c: &ConsumerInfo) -> serde_json::Value {
    serde_json::json!({
        "name": Name::of(&c.name),
        "pending": c.pending,
        "idle_ms": c.idle_ms,
        "idle": age(c.idle_ms),
        "inactive_ms": c.inactive_ms,
        "inactive": c.inactive_ms.map(age),
        "stuck": c.pending > 0 && c.idle_ms >= STUCK_IDLE_MS,
    })
}

/// A group, with what its claim form starts from: the most recently active consumer
/// takes the entries, idle at least a minute, 100 at most.
fn group_json(g: &GroupInfo) -> serde_json::Value {
    let to = g
        .readers
        .iter()
        .flatten()
        .min_by_key(|c| c.idle_ms)
        .map(|c| Name::of(&c.name));
    serde_json::json!({
        "name": Name::of(&g.name),
        "consumers": g.consumers,
        "pending": g.pending,
        "last_delivered_id": crate::value::escape(&g.last_delivered_id),
        "lag": lag_json(g.lag),
        "lagging": matches!(g.lag, Lag::Known(n) if n >= crate::live::streams::LAGGING_LAG),
        "readers": g.readers.as_ref().map(|r| r.iter().map(consumer_json).collect::<Vec<_>>()),
        "claim": { "to": to, "min_idle_ms": CLAIM_IDLE_MS, "count": CLAIM_COUNT },
    })
}

fn verdict_json(v: Verdict) -> serde_json::Value {
    serde_json::json!({ "label": v.label(), "text": verdict_reason(v) })
}

/// One stream as the page draws it: the list's columns, then the details pane.
fn row_json(c: &Current, f: &Fetched) -> serde_json::Value {
    let (node, endpoint) = node_of(c, &f.key);
    let Some(i) = &f.info else {
        return serde_json::json!({
            "key": Name::of(&f.key), "node": node, "endpoint": endpoint, "gone": true,
            "text": GONE,
        });
    };
    stream_json(&f.key, i, node, endpoint)
}

fn stream_json(
    key: &str,
    i: &StreamInfo,
    node: Option<usize>,
    endpoint: Option<String>,
) -> serde_json::Value {
    let vs = verdicts(i);
    let first = vs.first().copied().unwrap_or(Verdict::Ok);
    let notes: Vec<String> = left_out(i)
        .iter()
        .map(|n| crate::web::wording::server_free(n))
        .collect();
    serde_json::json!({
        "key": Name::of(key),
        "node": node,
        "endpoint": endpoint,
        "gone": false,
        "length": i.length,
        "bytes": i.bytes,
        "first_id": i.first_id.as_deref().map(crate::value::escape),
        "last_id": crate::value::escape(&i.last_id),
        "oldest_age_ms": i.oldest_age_ms,
        "oldest": i.oldest_age_ms.map(age),
        "entries_added": i.entries_added,
        "group_count": i.groups.as_ref().map(Vec::len),
        "lag": max_lag(i),
        "pending": total_pending(i),
        "verdict": first.label(),
        "verdicts": vs.iter().filter(|v| **v != Verdict::Ok).map(|v| verdict_json(*v)).collect::<Vec<_>>(),
        "left_out": (!notes.is_empty()).then(|| format!("Left out: {}.", notes.join(" · "))),
        "groups": i.groups.as_ref().map(|g| g.iter().map(group_json).collect::<Vec<_>>()),
        "groups_text": match &i.groups {
            None => Some(GROUPS_OFF),
            Some(g) if g.is_empty() => Some(NO_GROUPS),
            Some(_) => None,
        },
    })
}

/// The list reply: the rows, how many the analysis found, and why the last read failed.
fn list_json(c: &Current) -> serde_json::Value {
    let s = &c.streams;
    let rows: Vec<serde_json::Value> = s.rows.iter().map(|f| row_json(c, f)).collect();
    let note = match &s.error {
        Some(e) => Some(format!("{e}; showing the previous reading.")),
        None if s.found > s.rows.len() => Some(format!(
            "first {} of {} streams",
            s.rows.len(),
            crate::format::grouped(s.found as u64)
        )),
        None => None,
    };
    serde_json::json!({
        "status": "ready",
        "cluster": c.session.is_cluster(),
        "rows": rows,
        "found": s.found,
        "shown": s.rows.len(),
        "note": note.map(|n| crate::web::wording::server_free(&n)),
        "error": s.error.as_deref().map(crate::web::wording::server_free),
        "writes": writes_off(c),
    })
}

/// Why the stream writes are off for this profile, if they are: the page leaves the
/// buttons out then, as the TUI's `write_off` does. Listing pending entries still works.
fn writes_off(c: &Current) -> Option<String> {
    let p = c.profile();
    p.is_read_only().then(|| {
        crate::web::wording::server_free_with(
            &crate::write::read_only_reason(p),
            &super::writes::profile_data(p),
        )
    })
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// `1`: read now, and ask again for what was refused (the TUI's `r`).
    #[serde(default)]
    refresh: Option<u8>,
    /// `1`: read now, as the TUI's poll after a write does, still leaving out what was
    /// refused (no command it refused is sent again).
    #[serde(default)]
    now: Option<u8>,
}

/// What a read of the list is to do.
enum Step {
    Answer(Response),
    Read {
        run: (ConnId, u64),
        by_node: Vec<(Dest, Vec<String>)>,
        keys: Vec<String>,
        refused: Skip,
    },
}

/// `GET /api/streams[?refresh=1|now=1]`: the streams of the analysis, each read on its
/// owner. Before an analysis `status` is `no_report` (with the TUI's words; the page offers
/// to analyze), with none found `none`; else `ready` with `rows`. `{"off": …}` when the
/// permission check refused `XINFO`, or the server refused `XINFO STREAM`. A read of
/// another run than the session's when it answers stores nothing and answers
/// [`super::writes::CHANGED`].
pub async fn list(State(app): State<App>, Q(q): Q<ListQuery>) -> Response {
    let refresh = q.refresh == Some(1);
    let now = refresh || q.now == Some(1);
    let step = app.hub.with_mut(|c| {
        if !c.live_ready() {
            return Step::Answer(error(StatusCode::CONFLICT, super::NOT_LIVE));
        }
        if let Some(why) = view_off(c, crate::conn::PERM_XINFO) {
            return Step::Answer(super::off(&why));
        }
        if refresh {
            c.streams.off = None;
            c.streams.refused = Skip::default();
        }
        if let Some(why) = &c.streams.off {
            return Step::Answer(super::off(why));
        }
        let Some(report) = &c.session.report else {
            return Step::Answer(ok(serde_json::json!({
                "status": "no_report",
                "text": NO_ANALYSIS,
                "analysis_running": c.analysis_running(),
                "rows": [],
            })));
        };
        let keys = stream_keys(report);
        c.streams.found = keys.len();
        if keys.is_empty() {
            c.streams.rows.clear();
            c.streams.loaded = true;
            return Step::Answer(ok(serde_json::json!({
                "status": "none",
                "text": NONE_FOUND,
                "hint": NONE_HINT,
                "rows": [],
            })));
        }
        if !now && c.streams.loaded && c.streams.at.is_some_and(|t| t.elapsed() < FRESH) {
            return Step::Answer(ok(list_json(c)));
        }
        let keys: Vec<String> = keys.into_iter().take(MAX_STREAMS).collect();
        Step::Read {
            run: super::writes::run_of(c),
            by_node: streams_by_node(&c.session, &keys),
            keys,
            refused: c.streams.refused,
        }
    });
    let (run, by_node, keys, refused) = match step {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Step::Answer(r)) => return r,
        Some(Step::Read {
            run,
            by_node,
            keys,
            refused,
        }) => (run, by_node, keys, refused),
    };
    let conn = run.0;
    let reads = by_node.into_iter().map(|(dest, keys)| {
        let hub = std::sync::Arc::clone(&app.hub);
        let req = LiveRequest::Streams(Request::Info {
            keys: keys.clone(),
            refused,
        });
        async move { (keys, hub.request(Some(conn), dest, req).await) }
    });
    let answers = futures::future::join_all(reads).await;
    let body = app.hub.with_mut(|c| {
        // Read on another run (a new run reset the panel): nothing of it is kept.
        if super::writes::run_of(c) != run {
            return error(StatusCode::CONFLICT, super::writes::CHANGED);
        }
        gather(c, &keys, refused, answers).unwrap_or_else(|| ok(list_json(c)))
    });
    body.unwrap_or_else(|| error(StatusCode::CONFLICT, super::NOT_LIVE))
}

type Answers = Vec<(Vec<String>, Result<LiveReply, LiveError>)>;

/// The TUI's `take_poll` and its end: every node's answer into the rows, in the
/// analysis's order; a node that failed keeps its streams' last rows. A refused or
/// unsupported `XINFO STREAM` turns the panel off. `Some` is the reply to give instead
/// of the list.
fn gather(c: &mut Current, order: &[String], sent: Skip, answers: Answers) -> Option<Response> {
    let s = &mut c.streams;
    let mut got: Vec<Fetched> = Vec::new();
    let mut refused = sent;
    let mut failure: Option<LiveError> = None;
    for (keys, result) in answers {
        match result {
            Ok(LiveReply::Streams(Reply::Info {
                streams,
                refused: r,
            })) => {
                refused.time |= r.time;
                refused.groups |= r.groups;
                refused.consumers |= r.consumers;
                refused.memory |= r.memory;
                got.extend(streams);
            }
            Ok(_) => {}
            Err(e) => {
                for k in keys {
                    if let Some(r) = s.rows.iter().find(|r| r.key == k) {
                        got.push(r.clone());
                    }
                }
                let worse = matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_));
                if failure.is_none() || worse {
                    failure = Some(e);
                }
            }
        }
    }
    got.sort_by_key(|f| order.iter().position(|k| *k == f.key));
    match failure {
        Some(e @ (LiveError::Denied(_) | LiveError::Unsupported(_))) => {
            let why = off_text(&e);
            s.off = Some(why.clone());
            return Some(super::off(&why));
        }
        Some(LiveError::Closed) => return Some(error(StatusCode::CONFLICT, super::NOT_LIVE)),
        Some(e) => {
            if !got.is_empty() {
                s.rows = got;
            }
            if !s.loaded {
                return Some(super::live_error(&e));
            }
            s.error = Some(e.to_string());
        }
        None => {
            s.rows = got;
            s.loaded = true;
            s.error = None;
        }
    }
    s.refused = refused;
    s.at = Some(Instant::now());
    None
}

#[derive(Debug, Deserialize)]
pub struct PendingQuery {
    /// The stream's id (hex).
    key: String,
    /// The group's id (hex).
    group: String,
    /// A consumer's id (hex): its entries only.
    consumer: Option<String>,
}

/// The stream `key` of the list, and its group `group`, or why not.
fn listed<'a>(c: &'a Current, key: &str, group: &str) -> Result<&'a StreamInfo, Refused> {
    let row = c
        .streams
        .rows
        .iter()
        .find(|r| r.key == key)
        .ok_or_else(|| Refused::new(StatusCode::NOT_FOUND, NOT_LISTED))?;
    let info = row
        .info
        .as_ref()
        .ok_or_else(|| Refused::new(StatusCode::UNPROCESSABLE_ENTITY, GONE))?;
    if !group.is_empty() && !info.groups.iter().flatten().any(|g| g.name == group) {
        return Err(Refused::new(StatusCode::UNPROCESSABLE_ENTITY, NO_GROUP));
    }
    Ok(info)
}

fn pending_json(key: &str, p: &Pending) -> serde_json::Value {
    serde_json::json!({
        "key": Name::of(key),
        "group": Name::of(&p.group),
        "consumer": p.consumer.as_deref().map(Name::of),
        "entries": p.entries.iter().map(|e| serde_json::json!({
            "id": crate::value::escape(&e.id),
            "consumer": Name::of(&e.consumer),
            "idle_ms": e.idle_ms,
            "idle": age(e.idle_ms),
            "deliveries": e.deliveries,
        })).collect::<Vec<_>>(),
        "max": crate::live::streams::MAX_PENDING,
        "error": p.error.as_deref().map(|e| crate::web::wording::server_free(
            &format!("Pending entries: {e}.")
        )),
    })
}

/// `GET /api/streams/pending?key=<id>&group=<id>[&consumer=<id>]`: the first 100 pending
/// entries of a group (or one consumer's), oldest first, with `XPENDING` on the stream's
/// owner: a read, so a read-only profile lists them too. The stream's row is read again
/// with them (`row`). Answered on another run than it was asked on, nothing of it is
/// kept ([`super::writes::CHANGED`]).
pub async fn pending(State(app): State<App>, Q(q): Q<PendingQuery>) -> Response {
    let key = match keyname::decode(&q.key) {
        Ok(k) => k,
        Err(r) => return r.into_response(),
    };
    let group = match keyname::decode(&q.group) {
        Ok(g) => g,
        Err(r) => return r.into_response(),
    };
    let consumer = match q.consumer.as_deref().map(keyname::decode).transpose() {
        Ok(c) => c,
        Err(r) => return r.into_response(),
    };
    let made = app.hub.with(|c| {
        if !c.live_ready() {
            return Err(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE));
        }
        if let Some(why) = view_off(c, crate::conn::PERM_XINFO).or(c.streams.off.clone()) {
            return Err(Refused::new(StatusCode::OK, why));
        }
        listed(c, &key, &group)?;
        Ok((super::writes::run_of(c), c.streams.refused))
    });
    let (run, refused) = match made {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) if r.status == StatusCode::OK => return super::off(&r.text),
        Some(Err(r)) => return r.into_response(),
        Some(Ok(m)) => m,
    };
    let req = LiveRequest::Streams(Request::Pending {
        key: key.clone(),
        group,
        consumer,
        refused,
    });
    let result = app
        .hub
        .request(Some(run.0), Dest::key(key.as_bytes().to_vec()), req)
        .await;
    let body = app.hub.with_mut(|c| {
        if super::writes::run_of(c) != run {
            return error(StatusCode::CONFLICT, super::writes::CHANGED);
        }
        match result {
            Ok(LiveReply::Streams(Reply::Info { streams, refused })) => {
                c.streams.refused = refused;
                let Some(mut f) = streams.into_iter().next() else {
                    return error(StatusCode::BAD_GATEWAY, super::SHAPE);
                };
                let p = f.info.as_mut().and_then(|i| i.pending.take());
                if let Some(row) = c.streams.rows.iter_mut().find(|r| r.key == f.key) {
                    row.info = f.info.clone();
                }
                let row = row_json(c, &f);
                match p {
                    Some(p) => {
                        let body = pending_json(&f.key, &p);
                        c.streams.pending = Some((f.key, p));
                        ok(serde_json::json!({ "pending": body, "row": row }))
                    }
                    None => {
                        c.streams.pending = None;
                        ok(serde_json::json!({ "pending": null, "row": row, "text": GONE }))
                    }
                }
            }
            Ok(_) => error(StatusCode::BAD_GATEWAY, super::SHAPE),
            Err(e) => super::live_error(&e),
        }
    });
    body.unwrap_or_else(|| error(StatusCode::CONFLICT, super::NOT_LIVE))
}

/// A stream write's checks before the gate, in the order the TUI's checks come: the
/// gate's own refusal for a read-only profile, a bulk action running, the live
/// connection; then the stream (and group) in the list.
fn writable(c: &Current) -> Result<(), Refused> {
    super::edits::writable(c)?;
    if let Some(why) = view_off(c, crate::conn::PERM_XINFO).or(c.streams.off.clone()) {
        return Err(Refused::new(StatusCode::UNPROCESSABLE_ENTITY, why));
    }
    Ok(())
}

/// Builds a stream write from what the current session lists, then sends it through
/// the one write flow, for the connection and run it was made on.
async fn flow(app: &App, make: impl FnOnce(&Current) -> Result<WriteOp, Refused>) -> Response {
    app.hub.follow_saved_profile().await;
    let made = app.hub.with(|c| {
        writable(c)?;
        make(c).map(|op| (op, super::writes::MadeFor::of(c)))
    });
    match made {
        None => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => r.into_response(),
        Some(Ok((op, made_for))) => {
            super::writes::write_flow_then(app, op, None, Some(made_for)).await
        }
    }
}

fn decode(id: &str) -> Result<String, Refused> {
    keyname::decode(id)
}

/// `POST /api/streams/ack` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    key: String,
    group: String,
    /// The entry's ID, as the pending list shows it.
    id: String,
}

/// `POST /api/streams/ack`: the TUI's `A` on a pending entry, `XACK key group id`.
/// Only an entry of the pending list shown.
pub async fn ack(State(app): State<App>, Body(b): Body<Ack>) -> Response {
    flow(&app, |c| {
        let key = decode(&b.key)?;
        let group = decode(&b.group)?;
        if !is_stream_id(&b.id) {
            return Err(Refused::new(StatusCode::UNPROCESSABLE_ENTITY, BAD_ID));
        }
        listed(c, &key, &group)?;
        let shown = c.streams.pending.as_ref().is_some_and(|(k, p)| {
            *k == key && p.group == group && p.entries.iter().any(|e| e.id == b.id)
        });
        if !shown {
            return Err(Refused::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                NOT_PENDING_LISTED,
            ));
        }
        Ok(WriteOp::XAck {
            key,
            group,
            id: b.id.clone(),
        })
    })
    .await
}

/// `POST /api/streams/claim` body: the TUI's claim form, as typed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    key: String,
    group: String,
    /// The consumer that takes the entries, as typed.
    to: String,
    min_idle_ms: String,
    count: String,
}

/// Typed text: `Debug` leaves it out.
impl std::fmt::Debug for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claim").finish_non_exhaustive()
    }
}

/// `POST /api/streams/claim`: the TUI's `c`, `XAUTOCLAIM key group to min-idle 0-0
/// COUNT n`, checked as its form checks it (`views::streams::claim_op`).
pub async fn claim(State(app): State<App>, Body(b): Body<Claim>) -> Response {
    flow(&app, |c| {
        let key = decode(&b.key)?;
        let group = decode(&b.group)?;
        listed(c, &key, &group)?;
        let form = Form::new(
            "Claim pending entries",
            Vec::new(),
            vec![
                Field::text("to consumer", "").with(b.to.clone()),
                Field::text("min idle ms", "").with(b.min_idle_ms.clone()),
                Field::text("count", "").with(b.count.clone()),
            ],
        );
        claim_op(&key, &group, &form).map_err(|why| {
            Refused::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                crate::web::wording::web(&why),
            )
        })
    })
    .await
}

/// `POST /api/streams/trim` body: the TUI's trim form.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrimBody {
    key: String,
    /// `MAXLEN` or `MINID`.
    by: String,
    value: String,
}

/// Why the trim's `by` is refused.
pub const BAD_BY: &str = "trim by MAXLEN or MINID";

/// `POST /api/streams/trim`: the TUI's `X`, `XTRIM key MAXLEN ~ n` or `MINID ~ id`,
/// checked as its form checks it (`views::streams::trim_op`). Destructive: the gate
/// asks, and on `prod` the stream's name is typed.
pub async fn trim(State(app): State<App>, Body(b): Body<TrimBody>) -> Response {
    flow(&app, |c| {
        let key = decode(&b.key)?;
        listed(c, &key, "")?;
        if b.by != "MAXLEN" && b.by != "MINID" {
            return Err(Refused::new(StatusCode::BAD_REQUEST, BAD_BY));
        }
        let by = Field::choice("trim by", &["MAXLEN", "MINID"], "").with(b.by.clone());
        let form = Form::new(
            "Trim stream",
            Vec::new(),
            vec![by, Field::text("value", "").with(b.value.clone())],
        );
        trim_op(&key, &form).map_err(|why| {
            Refused::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                crate::web::wording::web(&why),
            )
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::streams::PendingEntry;
    use crate::write::Trim;

    fn trim_of(op: &WriteOp) -> Option<&Trim> {
        match op {
            WriteOp::XTrim { trim, .. } => Some(trim),
            _ => None,
        }
    }

    fn consumer(name: &str, pending: u64, idle_ms: u64) -> ConsumerInfo {
        ConsumerInfo {
            name: name.into(),
            pending,
            idle_ms,
            inactive_ms: None,
        }
    }

    fn info() -> StreamInfo {
        StreamInfo {
            key: "events:orders".into(),
            length: 1200,
            bytes: Some(4096),
            first_id: Some("1700000000000-0".into()),
            last_id: "1700000005000-3".into(),
            oldest_age_ms: Some(90_000),
            entries_added: Some(1250),
            groups: Some(vec![GroupInfo {
                name: "billing".into(),
                consumers: 2,
                pending: 7,
                last_delivered_id: "1700000004000-0".into(),
                lag: Lag::Known(1500),
                readers: Some(vec![
                    consumer("worker-1", 5, 40 * 60 * 1000),
                    consumer("worker-2", 2, 3_000),
                ]),
            }]),
            notes: vec![],
            pending: None,
        }
    }

    #[test]
    fn a_stream_reads_as_the_tuis_list_and_details() {
        let v = stream_json("events:orders", &info(), None, None);
        assert_eq!(v["key"]["name"], "events:orders");
        assert_eq!(v["length"], 1200);
        assert_eq!(v["lag"], 1500);
        assert_eq!(v["pending"], 7);
        assert_eq!(v["oldest"], "1m");
        assert_eq!(v["verdict"], "stuck");
        let labels: Vec<&str> = v["verdicts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["stuck", "lagging"]);
        let g = &v["groups"][0];
        assert_eq!(g["name"]["name"], "billing");
        assert_eq!(g["lagging"], true);
        assert_eq!(g["readers"][0]["stuck"], true);
        assert_eq!(g["readers"][1]["stuck"], false);
        // The claim form starts with the most recently active consumer.
        assert_eq!(g["claim"]["to"]["name"], "worker-2");
        assert_eq!(g["claim"]["min_idle_ms"], CLAIM_IDLE_MS);
        // Before Redis 7.2 there is no inactive time: said, not blank.
        assert_eq!(v["left_out"], "Left out: inactive: Redis 7.2 or later.");
        assert_eq!(v["groups_text"], serde_json::Value::Null);
    }

    #[test]
    fn groups_refused_and_none_are_said_in_the_tuis_words() {
        let mut i = info();
        i.groups = None;
        assert_eq!(stream_json("s", &i, None, None)["groups_text"], GROUPS_OFF);
        i.groups = Some(vec![]);
        let v = stream_json("s", &i, None, None);
        assert_eq!(v["groups_text"], NO_GROUPS);
        assert_eq!(v["verdict"], "no readers");
    }

    #[test]
    fn names_with_control_characters_are_escaped_and_sent_back_as_hex() {
        let mut i = info();
        i.groups.as_mut().unwrap()[0].name = "g\x1b[31m".into();
        let v = stream_json("s\u{202e}x", &i, None, None);
        assert_eq!(v["key"]["copy"], serde_json::Value::Null);
        assert!(!v["key"]["name"].as_str().unwrap().contains('\u{202e}'));
        assert_eq!(v["groups"][0]["name"]["id"], keyname::id("g\x1b[31m"));
        assert!(!v.to_string().contains('\x1b'));
    }

    #[test]
    fn the_pending_list_holds_ids_consumers_and_times_only() {
        let p = Pending {
            group: "billing".into(),
            consumer: None,
            entries: vec![PendingEntry {
                id: "1700000000000-0".into(),
                consumer: "worker-1".into(),
                idle_ms: 125_000,
                deliveries: 3,
            }],
            error: None,
        };
        let v = pending_json("events:orders", &p);
        assert_eq!(v["entries"][0]["id"], "1700000000000-0");
        assert_eq!(v["entries"][0]["idle"], "2m");
        assert_eq!(v["entries"][0]["deliveries"], 3);
        assert_eq!(v["consumer"], serde_json::Value::Null);
    }

    #[test]
    fn the_forms_build_the_tuis_ops() {
        let form = Form::new(
            "",
            Vec::new(),
            vec![
                Field::text("to consumer", "").with("w2".to_string()),
                Field::text("min idle ms", "").with("60000".to_string()),
                Field::text("count", "").with("0".to_string()),
            ],
        );
        assert_eq!(
            claim_op("s", "g", &form).unwrap_err(),
            "The count is a whole number, 1 or more."
        );
        let by = Field::choice("trim by", &["MAXLEN", "MINID"], "").with("MINID");
        let form = Form::new(
            "",
            Vec::new(),
            vec![
                by,
                Field::text("value", "").with("1700000000000-0".to_string()),
            ],
        );
        let op = trim_op("s", &form).unwrap();
        assert_eq!(
            trim_of(&op),
            Some(&Trim::MinId("1700000000000-0".to_string()))
        );
    }

    #[test]
    fn the_words_are_the_dashboards_own() {
        for t in [
            NO_ANALYSIS,
            NONE_FOUND,
            NONE_HINT,
            GONE,
            NOT_LISTED,
            NO_GROUP,
            NOT_PENDING_LISTED,
            BAD_ID,
            GROUPS_OFF,
            NO_GROUPS,
            APPROXIMATE,
            "MINID takes an entry ID, such as 1700000000000-0.",
            "MAXLEN takes how many entries to keep.",
            "The server refused XINFO for this user, so this view is off.",
        ] {
            assert_eq!(crate::web::wording::banned(t), None, "{t}");
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
        for v in [
            Verdict::Stuck,
            Verdict::Lagging,
            Verdict::NoReaders,
            Verdict::Unbounded,
        ] {
            let t = verdict_reason(v);
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
    }
}
