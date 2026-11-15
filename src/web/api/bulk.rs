//! Bulk expire and bulk delete over a pattern's keys (spec "Bulk actions"): the TUI's
//! `x` and `D` on Patterns, as routes, with the same steps and the same live requests.
//!
//! 1. `POST /api/bulk {pattern, action, ttl?}` checks the action can run (the gate, the
//!    replica and cluster checks of [`bulk_nodes`], `EXPIRE … NX` on the server, the
//!    live connection) and starts **the dry run**: [`Request::Count`] steps
//!    ([`crate::live::bulk::count`], reads only), one primary at a time on a cluster.
//!    Each step goes out as a `bulk` event. When nothing would change it says so and
//!    asks nothing.
//! 2. `POST /api/bulk/run {bulk}` asks **the question** (409): yes or no, or the pattern
//!    typed on `prod`. It is held as [`super::writes::Action::Bulk`]; the answer is
//!    checked by the gate again ([`super::writes::grant`]), with the saved profile read
//!    again and on the same run of the connection only. Only the [`Permit`] it gives
//!    starts **the run**: [`Request::Batch`] steps of
//!    [`BATCH`](crate::live::bulk::BATCH) keys, each sent once the last answered, each
//!    a `bulk` event. The saved profile is read again before every batch: one made
//!    read-only elsewhere stops the run there.
//! 3. `DELETE /api/bulk` stops: a dry run at once, a run after the batch on its way.
//!
//! One bulk action per connection, as the TUI's dialog. Every command goes through the
//! live layer and its budget; nothing is built here.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{App, Body, Question, Refused, error, ok};
use crate::live::bulk::{self, Batch, Count, FileAt, Reply, Request, Walk};
use crate::live::{Dest, Follow, LiveError, LiveReply};
use crate::tui::app::ConnId;
use crate::tui::app::bulk::{bulk_nodes, summary};
use crate::web::keyname::{self, Name};
use crate::web::session::{Current, Event};
use crate::write::{Answer, BulkAction, Decision, Permit, WriteOp};

/// The open bulk action of a connection.
#[derive(Debug, Clone)]
pub struct Bulk {
    /// Its number, unique for the dashboard's run: questions and stops name it.
    pub id: u64,
    conn: ConnId,
    /// The run of the session it was started in.
    run: u64,
    pub pattern: String,
    pub glob: String,
    pub action: BulkAction,
    /// The profile it runs on, escaped, and whether it is `prod`.
    pub profile: String,
    pub prod: bool,
    /// The name the key file is filed under (`docker:<name>` for a container).
    files_as: String,
    /// On a cluster, the primaries walked one at a time in slot order (indices into the
    /// session's nodes) and their announced addresses; empty on a standalone server.
    pub nodes: Vec<(usize, String)>,
    /// Where the walk is in `nodes`.
    pub at: usize,
    /// The dry run's counts per primary, in the order of `nodes`.
    pub per_node: Vec<Count>,
    /// The dry run's counts.
    pub count: Count,
    pub stage: Stage,
}

/// Where a bulk action is.
#[derive(Debug, Clone)]
pub enum Stage {
    /// The dry run, at `walk` of the node it is on.
    Counting {
        walk: Walk,
    },
    /// Counted: something would change, and the run waits for its question
    /// (`POST /api/bulk/run`).
    Counted,
    Running(Box<Run>),
    Done(Done),
}

/// A run under way.
#[derive(Debug, Clone)]
pub struct Run {
    permit: Permit,
    walk: Walk,
    carry: Vec<String>,
    /// Keys of the pattern handled so far.
    pub seen: u64,
    pub sent: u64,
    pub changed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub unreadable: u64,
    /// Keys that moved to another node during the run: left alone.
    pub moved: u64,
    pub file: Option<std::path::PathBuf>,
    /// A stop was asked for: no batch after the one on its way.
    pub stopping: bool,
}

/// How it ended, as lines safe to show.
#[derive(Debug, Clone)]
pub struct Done {
    pub lines: Vec<String>,
    /// It stopped on an error or a refusal.
    pub error: bool,
    /// The run it ended, when one started.
    run: Option<Box<Run>>,
}

static NEXT_BULK: AtomicU64 = AtomicU64::new(1);

/// The words of the routes, without the TUI's keys.
pub const ONE_AT_A_TIME: &str =
    "A bulk action is running already: wait for it to end, or stop it first.";
pub const NO_BULK: &str = "That bulk action is no longer open: start it again from its pattern.";
pub const NOT_COUNTED: &str = "The dry run has not finished: wait for its count.";
pub const NOTHING_TO_RUN: &str = "Nothing to run: the dry run found nothing to change.";
pub const RUNNING: &str = "That bulk action has started already.";
pub const BAD_ACTION: &str = "the action must be expire or delete";
pub const BAD_TTL: &str = "not a duration: use 90s, 10m, 24h or 7d";
pub const DRY_RUN_STOPPED: &str = "Dry run stopped. Nothing was sent.";
pub const BULK_CANCELLED: &str = "Bulk action cancelled. Nothing was sent.";
pub const STOPPING: &str = "Stopping after the batch on its way.";

impl Bulk {
    /// The question's title, as the TUI's dialog has it.
    pub fn title(&self) -> &'static str {
        match self.action {
            BulkAction::Delete => "Delete every key of the pattern",
            BulkAction::Expire { .. } => "Set a TTL on every key of the pattern",
        }
    }

    fn op(&self) -> WriteOp {
        WriteOp::Bulk {
            pattern: self.pattern.clone(),
            glob: self.glob.clone(),
            action: self.action,
        }
    }

    /// The live request's destination: the primary the walk is on, never following a
    /// redirect; on a standalone server its one node.
    fn dest(&self) -> Dest {
        match self.nodes.get(self.at) {
            Some((i, _)) => Dest::node(*i).follow(Follow::Never),
            None => Dest::node(0),
        }
    }

    /// The announced address of the node the walk is on, for the key file (cluster
    /// only).
    fn node(&self) -> Option<String> {
        self.nodes.get(self.at).map(|(_, a)| a.clone())
    }

    fn data(&self) -> [&str; 2] {
        [self.profile.as_str(), self.pattern.as_str()]
    }

    /// The bulk action as the page gets it.
    pub fn body(&self) -> serde_json::Value {
        let (stage, run, lines, error) = match &self.stage {
            Stage::Counting { .. } => ("counting", None, Vec::new(), false),
            Stage::Counted => ("counted", None, Vec::new(), false),
            Stage::Running(r) => ("running", Some(&**r), Vec::new(), false),
            Stage::Done(d) => ("done", d.run.as_deref(), d.lines.clone(), d.error),
        };
        let words = |l: &String| {
            crate::web::wording::server_free_with(
                l,
                &[
                    &self.profile,
                    &crate::value::escape(&self.pattern),
                    &escaped_file(run.and_then(|r| r.file.as_ref())),
                ],
            )
        };
        // The dry run's summary once its count is complete: a dry run stopped part way
        // counted only some keys.
        let summary: Vec<String> = if self.count.finished() {
            summary(&self.count, &self.action)
        } else {
            Vec::new()
        };
        serde_json::json!({
            "id": self.id,
            "pattern": Name::of(&self.pattern),
            "glob": crate::value::escape(&self.glob),
            "action": match self.action {
                BulkAction::Delete => "delete",
                BulkAction::Expire { .. } => "expire",
            },
            "ttl": match self.action {
                BulkAction::Expire { seconds } => Some(bulk::ttl_text(seconds)),
                BulkAction::Delete => None,
            },
            "ttl_secs": match self.action {
                BulkAction::Expire { seconds } => Some(seconds),
                BulkAction::Delete => None,
            },
            "title": self.title(),
            "profile": self.profile,
            "prod": self.prod,
            "stage": stage,
            "count": {
                "matched": self.count.matched,
                "with_ttl": self.count.with_ttl,
                "other_pattern": self.count.other_pattern,
                "unreadable": self.count.unreadable,
                "to_change": self.count.to_change(&self.action),
            },
            "summary": summary.iter().map(words).collect::<Vec<_>>(),
            "nodes": self.nodes.iter().zip(
                self.per_node.iter().map(Some).chain(std::iter::repeat(None))
            ).map(|((i, addr), c)| serde_json::json!({
                "node": i,
                "endpoint": crate::value::escape(addr),
                "matched": c.map(|c| c.matched),
            })).collect::<Vec<_>>(),
            "at": self.nodes.get(self.at).map(|(i, _)| *i),
            "run": run.map(|r| serde_json::json!({
                "seen": r.seen,
                "sent": r.sent,
                "changed": r.changed,
                "skipped": r.skipped,
                "failed": r.failed,
                "unreadable": r.unreadable,
                "moved": r.moved,
                "file": r.file.as_ref().map(|f| escaped_file(Some(f))),
                "stopping": r.stopping,
            })),
            "lines": lines.iter().map(words).collect::<Vec<_>>(),
            "error": error,
        })
    }
}

fn escaped_file(f: Option<&std::path::PathBuf>) -> String {
    f.map(|f| crate::value::escape(&f.display().to_string()))
        .unwrap_or_default()
}

fn n(v: u64) -> String {
    crate::format::grouped(v)
}

fn keys_word(v: u64) -> &'static str {
    if v == 1 { "key" } else { "keys" }
}

/// `{"bulk": <the body, or null>}`: what every route answers and every `bulk` event
/// holds.
fn reply_of(b: Option<&Bulk>) -> serde_json::Value {
    serde_json::json!({ "bulk": b.map(Bulk::body) })
}

/// The `bulk` event of connection `conn`'s bulk action as it is now.
fn emit(app: &App, conn: ConnId) {
    let data = app
        .hub
        .with(|c| (c.id == conn).then(|| reply_of(c.bulk.as_ref())))
        .flatten();
    if let Some(d) = data {
        app.hub.emit(Event::new("bulk", d));
    }
}

/// `GET /api/bulk`: the connection's bulk action, or `null`.
pub async fn get(State(app): State<App>) -> Response {
    ok(app
        .hub
        .with(|c| reply_of(c.bulk.as_ref()))
        .unwrap_or_else(|| reply_of(None)))
}

/// `POST /api/bulk` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    /// The pattern's id (hex), as `GET /api/browse` names it.
    pub pattern: String,
    /// `expire` or `delete`.
    pub action: String,
    /// For an expire: the TTL (`24h` when left out), as the TUI's input takes it.
    #[serde(default)]
    pub ttl: Option<String>,
}

/// `POST /api/bulk`: checks the action can run, then starts its dry run (202). Nothing
/// is sent when it can't: a read-only profile is refused with the gate's words before
/// any `SCAN`.
pub async fn start(State(app): State<App>, Body(b): Body<Start>) -> Response {
    let pattern = match keyname::decode(&b.pattern) {
        Ok(p) => p,
        Err(r) => return r.into_response(),
    };
    let action = match b.action.as_str() {
        "delete" => BulkAction::Delete,
        "expire" => {
            let seconds = match b.ttl.as_deref() {
                None => bulk::DEFAULT_TTL_SECS,
                Some(t) => match bulk::parse_ttl(t) {
                    Ok(s) => s,
                    // The TUI quotes what was typed; the dashboard never echoes input.
                    Err(e) if e.starts_with("not a duration") => {
                        return error(StatusCode::UNPROCESSABLE_ENTITY, BAD_TTL);
                    }
                    Err(e) => return error(StatusCode::UNPROCESSABLE_ENTITY, &e),
                },
            };
            BulkAction::Expire { seconds }
        }
        _ => return error(StatusCode::BAD_REQUEST, BAD_ACTION),
    };
    app.hub.follow_saved_profile().await;
    let made = app.hub.with_mut(|c| open(c, pattern, action));
    let conn = match made {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => return r.into_response(),
        Some(Ok(conn)) => conn,
    };
    emit(&app, conn);
    let task = app.clone();
    tokio::spawn(async move { count_loop(task, conn).await });
    accepted(&app)
}

/// 202 with the bulk action as it is now.
fn accepted(app: &App) -> Response {
    let body = app
        .hub
        .with(|c| reply_of(c.bulk.as_ref()))
        .unwrap_or_else(|| reply_of(None));
    (StatusCode::ACCEPTED, axum::Json(body)).into_response()
}

/// The checks of the TUI's `bulk_start`, then the bulk action in its dry run. The gate
/// is asked first, so a read-only profile is refused in its words whatever else holds.
fn open(c: &mut Current, pattern: String, action: BulkAction) -> Result<ConnId, Refused> {
    let glob = crate::analysis::match_glob(&pattern);
    let op = WriteOp::Bulk {
        pattern: pattern.clone(),
        glob: glob.clone(),
        action,
    };
    let refused = |why: String, c: &Current| {
        Refused::new(StatusCode::UNPROCESSABLE_ENTITY, why)
            .holding(&super::writes::profile_data(c.profile()))
    };
    if let Decision::Refuse(why) =
        crate::write::decide_op(&op, c.profile(), &c.settings, &c.session.refusals)
    {
        return Err(refused(why, c));
    }
    if c.bulk
        .as_ref()
        .is_some_and(|b| matches!(b.stage, Stage::Counting { .. } | Stage::Running(_)))
    {
        return Err(Refused::new(StatusCode::CONFLICT, ONE_AT_A_TIME));
    }
    let report_nodes = c.session.report.as_ref().is_some_and(|r| r.nodes.is_some());
    let nodes = bulk_nodes(&c.session, report_nodes).map_err(|why| refused(why, c))?;
    if matches!(action, BulkAction::Expire { .. })
        && let Some(why) = c.session.server.as_ref().and_then(bulk::expire_off)
    {
        return Err(refused(why, c));
    }
    if !c.live_ready() {
        return Err(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE));
    }
    let files_as = c
        .session
        .report
        .as_ref()
        .map_or_else(|| c.report_name(), |r| r.profile.clone());
    let per_node = vec![Count::default(); nodes.len()];
    c.bulk = Some(Bulk {
        id: NEXT_BULK.fetch_add(1, Ordering::Relaxed),
        conn: c.id,
        run: c.session.scan,
        pattern,
        glob,
        action,
        profile: crate::value::escape(&c.profile().name),
        prod: c.profile().env == crate::model::Env::Prod,
        files_as,
        nodes,
        at: 0,
        per_node,
        count: Count::default(),
        stage: Stage::Counting { walk: Walk::Start },
    });
    Ok(c.id)
}

/// The bulk action `id` of connection `conn`, when it is still the open one.
fn this(c: &mut Current, conn: ConnId, id: Option<u64>) -> Option<&mut Bulk> {
    if c.id != conn {
        return None;
    }
    c.bulk
        .as_mut()
        .filter(|b| b.conn == conn && id.is_none_or(|i| b.id == i))
}

/// The dry run: one count step at a time until the walk is over on every primary, a
/// stop, or a failure.
async fn count_loop(app: App, conn: ConnId) {
    let Some(id) = app.hub.with(|c| c.bulk.as_ref().map(|b| b.id)).flatten() else {
        return;
    };
    loop {
        let step = app
            .hub
            .with_mut(|c| {
                let b = this(c, conn, Some(id))?;
                let Stage::Counting { walk } = b.stage else {
                    return None;
                };
                Some((
                    b.dest(),
                    Request::Count {
                        pattern: b.pattern.clone(),
                        glob: b.glob.clone(),
                        walk,
                    },
                ))
            })
            .flatten();
        let Some((dest, req)) = step else {
            return;
        };
        let result = app.hub.bulk_step(conn, dest, req).await;
        let more = app
            .hub
            .with_mut(|c| {
                let b = this(c, conn, Some(id))?;
                // Stopped meanwhile: its answer is dropped.
                if !matches!(b.stage, Stage::Counting { .. }) {
                    return None;
                }
                Some(counted_step(b, result))
            })
            .flatten();
        emit(&app, conn);
        if more != Some(true) {
            return;
        }
    }
}

/// A dry-run step answered: the counts so far, then the next step (true), or the end.
fn counted_step(b: &mut Bulk, result: Result<LiveReply, LiveError>) -> bool {
    let c = match result {
        Ok(LiveReply::Bulk(Reply::Count(c))) => c,
        other => {
            let why = match other {
                Err(e) => e.to_string(),
                Ok(_) => "the answer could not be read".to_string(),
            };
            b.stage = Stage::Done(Done {
                lines: vec![format!("The dry run failed: {why}. Nothing was sent.")],
                error: true,
                run: None,
            });
            return false;
        }
    };
    b.count.add(&c);
    if let Some(p) = b.per_node.get_mut(b.at) {
        p.add(&c);
    }
    let mut walk = c.walk.unwrap_or(Walk::Done);
    if walk == Walk::Done && b.at + 1 < b.nodes.len() {
        // This primary is counted: the next one, from its own start.
        b.at += 1;
        walk = Walk::Start;
        b.count.walk = Some(Walk::Start);
    }
    if !b.count.finished() {
        b.stage = Stage::Counting { walk };
        return true;
    }
    if b.count.to_change(&b.action) == 0 {
        let pattern = crate::value::escape(&b.pattern);
        let text = if b.count.matched == 0 {
            format!("No keys of {pattern} were found. Nothing was sent.")
        } else {
            format!(
                "Every key of {pattern} ({}) has a TTL already. Nothing was sent.",
                n(b.count.matched)
            )
        };
        b.stage = Stage::Done(Done {
            lines: vec![text],
            error: false,
            run: None,
        });
    } else {
        b.stage = Stage::Counted;
    }
    false
}

/// `POST /api/bulk/run` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunIt {
    /// The bulk action's `id`.
    pub bulk: u64,
}

/// `POST /api/bulk/run`: after the dry run, the question (409): yes or no, or the
/// pattern typed on `prod`. Bulk actions always ask.
pub async fn run(State(app): State<App>, Body(r): Body<RunIt>) -> Response {
    app.hub.follow_saved_profile().await;
    let decided = app.hub.with_mut(|c| {
        let conn = c.id;
        let scan = c.session.scan;
        let (profile, settings, refusals) = (
            c.session.profile.clone(),
            c.settings,
            c.session.refusals.clone(),
        );
        let b = this(c, conn, Some(r.bulk))
            .ok_or_else(|| Refused::new(StatusCode::NOT_FOUND, NO_BULK))?;
        match &b.stage {
            Stage::Counted => {}
            Stage::Counting { .. } => return Err(Refused::new(StatusCode::CONFLICT, NOT_COUNTED)),
            Stage::Running(_) => return Err(Refused::new(StatusCode::CONFLICT, RUNNING)),
            Stage::Done(_) => {
                return Err(Refused::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    NOTHING_TO_RUN,
                ));
            }
        }
        if b.run != scan {
            return Err(Refused::new(StatusCode::CONFLICT, super::writes::CHANGED));
        }
        let op = b.op();
        let decision = crate::write::decide_op(&op, &profile, &settings, &refusals);
        if let Decision::Refuse(why) = &decision {
            // The TUI closes its dialog with the reason.
            b.stage = Stage::Done(Done {
                lines: vec![why.clone()],
                error: true,
                run: None,
            });
            return Err(Refused::new(StatusCode::UNPROCESSABLE_ENTITY, why.clone())
                .holding(&super::writes::profile_data(&profile)));
        }
        let question = question(b, &op, &decision);
        Ok((conn, scan, op, decision, question))
    });
    let (conn, scan, op, decision, question) = match decided {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => {
            if r.status == StatusCode::UNPROCESSABLE_ENTITY
                && let Some(c) = app.hub.with(|c| c.id)
            {
                emit(&app, c);
            }
            return r.into_response();
        }
        Some(Ok(d)) => d,
    };
    match (&decision, question) {
        // Never with the gate as it is (`skip_non_prod_confirm` never applies to bulk
        // actions); were that to change, the gate still decides.
        (Decision::Run, _) => answered(&app, (conn, scan), r.bulk, op, &Answer::None).await,
        (_, Some(q)) => app.ask(
            super::writes::Action::Bulk {
                conn,
                run: scan,
                bulk: r.bulk,
                op,
                decision,
            },
            q,
        ),
        (_, None) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            crate::web::wording::NO_QUESTION,
        ),
    }
}

/// The question for bulk action `b`, as the TUI's dialog asks it: what it does, on
/// which profile, the dry run's count, whether it can be undone, the command sent;
/// on `prod` the pattern to type. `None` when it can't be put in the dashboard's words.
pub fn question(b: &Bulk, op: &WriteOp, decision: &Decision) -> Option<Question> {
    let pattern = crate::value::escape(&b.pattern);
    let mut parts = summary(&b.count, &b.action);
    if !b.nodes.is_empty() {
        parts.push(format!(
            "It runs on each of the {} primaries in turn, in slot order.",
            b.nodes.len()
        ));
    }
    parts.push(match b.action {
        BulkAction::Expire { .. } => "PERSIST on the keys in the key file undoes it.".to_string(),
        BulkAction::Delete => "This can't be undone.".to_string(),
    });
    parts.push(format!("Sends: {}.", op.shown()));
    let rest = parts.join(" ");
    let title = b.title();
    let data = b.data();
    match decision {
        Decision::Ask => {
            let text = format!("{title} {pattern} on {}? {rest}", b.profile);
            crate::web::wording::question(&text, &data).map(Question::ask)
        }
        Decision::Type(want) => {
            let text = format!(
                "Type {} to start: {title} {pattern} on {}. {rest}",
                crate::value::escape(want),
                b.profile
            );
            crate::web::wording::question(&text, &data).map(|t| Question::typed(t, want.clone()))
        }
        Decision::Run | Decision::Refuse(_) => None,
    }
}

/// The question about bulk action `bulk` was answered: the gate gives the permit for
/// `answer` (the saved profile read again, the same run of the connection only), and
/// the run starts with it (202). Refused, the bulk action ends with the reason.
pub async fn answered(
    app: &App,
    (conn, run): (ConnId, u64),
    bulk: u64,
    op: WriteOp,
    answer: &Answer,
) -> Response {
    app.hub.follow_saved_profile().await;
    let open = app
        .hub
        .with_mut(|c| this(c, conn, Some(bulk)).map(|b| matches!(b.stage, Stage::Counted)))
        .flatten();
    match open {
        None => return error(StatusCode::GONE, NO_BULK),
        Some(false) => return error(StatusCode::CONFLICT, RUNNING),
        Some(true) => {}
    }
    let permit = match super::writes::grant(app, (conn, run), op, answer) {
        Ok(p) => p,
        Err(r) => {
            // A wrong name typed leaves the question's bulk action to be asked again,
            // as the TUI's typed name sends nothing; any other refusal ends it.
            if r.text != "Not sent: the name typed did not match." {
                let ended = app.hub.with_mut(|c| {
                    if let Some(b) = this(c, conn, Some(bulk)) {
                        b.stage = Stage::Done(Done {
                            lines: vec![r.text.clone()],
                            error: true,
                            run: None,
                        });
                    }
                });
                if ended.is_some() {
                    emit(app, conn);
                }
            }
            return r.into_response();
        }
    };
    let started = app
        .hub
        .with_mut(|c| {
            let b = this(c, conn, Some(bulk))?;
            if !matches!(b.stage, Stage::Counted) {
                return None;
            }
            // The run walks the primaries again from the first.
            b.at = 0;
            b.stage = Stage::Running(Box::new(Run {
                permit,
                walk: Walk::Start,
                carry: Vec::new(),
                seen: 0,
                sent: 0,
                changed: 0,
                skipped: 0,
                failed: 0,
                unreadable: 0,
                moved: 0,
                file: None,
                stopping: false,
            }));
            Some(())
        })
        .flatten();
    if started.is_none() {
        return error(StatusCode::CONFLICT, RUNNING);
    }
    emit(app, conn);
    let task = app.clone();
    tokio::spawn(async move { run_loop(task, conn, bulk).await });
    accepted(app)
}

/// The run: one batch at a time, each sent once the last answered, until the walk is
/// over on every primary, a stop, a refusal, or the profile turning read-only.
async fn run_loop(app: App, conn: ConnId, id: u64) {
    let mut first = true;
    loop {
        if !first {
            // Writes turned off elsewhere since the last batch: no further one.
            app.hub.follow_saved_profile().await;
        }
        first = false;
        let step = app
            .hub
            .with_mut(|c| {
                let read_only = c.profile().is_read_only().then(|| {
                    format!(
                        "{} is now read-only",
                        crate::value::escape(&c.profile().name)
                    )
                });
                let changed = c.session.scan;
                let b = this(c, conn, Some(id))?;
                if !matches!(b.stage, Stage::Running(_)) {
                    return None;
                }
                if let Some(why) = read_only {
                    finish(b, Some(why));
                    return None;
                }
                if b.run != changed {
                    finish(b, Some("the connection changed".to_string()));
                    return None;
                }
                let Stage::Running(r) = &b.stage else {
                    return None;
                };
                Some((
                    b.dest(),
                    r.permit.clone(),
                    r.walk,
                    r.carry.clone(),
                    r.file.clone(),
                    b.files_as.clone(),
                    b.node(),
                ))
            })
            .flatten();
        let Some((dest, permit, walk, carry, file, files_as, node)) = step else {
            emit(&app, conn);
            return;
        };
        let file = file.map_or_else(|| app.hub.bulk_file(files_as), FileAt::Path);
        let req = Request::Batch {
            permit,
            walk,
            carry,
            file,
            node,
        };
        let result = app.hub.bulk_step(conn, dest, req).await;
        let more = app
            .hub
            .with_mut(|c| {
                let mut refuse: Option<String> = None;
                let b = this(c, conn, Some(id))?;
                if !matches!(b.stage, Stage::Running(_)) {
                    return None;
                }
                let go = batch_done(b, result, &mut refuse);
                if let Some(command) = refuse {
                    c.session.refusals.add(&command);
                }
                Some(go)
            })
            .flatten();
        emit(&app, conn);
        if more != Some(true) {
            return;
        }
    }
}

/// A batch answered: true when the next one is to be sent. `refuse` is set to the
/// command a `NOPERM` turns off for the session (no key of the batch changed: the
/// command, not a key, was refused), as the TUI does.
fn batch_done(
    b: &mut Bulk,
    result: Result<LiveReply, LiveError>,
    refuse: &mut Option<String>,
) -> bool {
    let batch: Batch = match result {
        Ok(LiveReply::Bulk(Reply::Batch(batch))) => batch,
        other => {
            // A `NOPERM` on the whole request turned the command off already
            // (`Hub::bulk_step`).
            let why = match other {
                Err(e) => e.to_string(),
                Ok(_) => "the answer could not be read".to_string(),
            };
            finish(b, Some(why));
            return false;
        }
    };
    let nodes = b.nodes.len();
    let at = b.at;
    let Stage::Running(r) = &mut b.stage else {
        return false;
    };
    r.moved += batch.moved;
    r.seen += batch.seen;
    r.sent += batch.sent;
    r.changed += batch.changed;
    r.skipped += batch.skipped;
    r.failed += batch.failed;
    r.unreadable += batch.unreadable;
    if batch.file.is_some() {
        r.file = batch.file.clone();
    }
    if batch.failed > 0 {
        let command = r.permit.op().command();
        let why = if batch.denied {
            if batch.changed == 0 {
                *refuse = Some(command.clone());
            }
            format!("the server refused {command} for this user")
        } else {
            format!(
                "the server answered {command} with an error for {} {}",
                n(batch.failed),
                keys_word(batch.failed)
            )
        };
        finish(b, Some(why));
        return false;
    }
    let mut batch = batch;
    let mut next_at = at;
    if batch.finished() && at + 1 < nodes {
        // This primary is done: the next one, from the start of its own keyspace.
        next_at = at + 1;
        batch.walk = Walk::Start;
        batch.carry = Vec::new();
    }
    let stop = r.stopping || batch.finished();
    r.walk = batch.walk;
    r.carry = batch.carry;
    b.at = next_at;
    if stop {
        finish(b, None);
        return false;
    }
    true
}

/// The run is over: finished (`why` is `None`), stopped when asked, or stopped on
/// `why`. The lines say what was done, as the TUI's dialog does.
fn finish(b: &mut Bulk, why: Option<String>) {
    let Stage::Running(r) = &b.stage else {
        return;
    };
    let r = (**r).clone();
    let pattern = crate::value::escape(&b.pattern);
    let did = match b.action {
        BulkAction::Delete => format!(
            "deleted {} {} of {pattern}",
            n(r.changed),
            keys_word(r.changed)
        ),
        BulkAction::Expire { seconds } => format!(
            "set a TTL of {} on {} {} of {pattern}",
            bulk::ttl_text(seconds),
            n(r.changed),
            keys_word(r.changed)
        ),
    };
    let mut lines = vec![match (&why, r.stopping) {
        (Some(why), _) => format!("Stopped: {why}. Before that, {did}."),
        (None, true) => format!("Stopped after the batch: {did}."),
        (None, false) => format!("Done: {did}."),
    }];
    if r.skipped > 0 {
        lines.push(match b.action {
            BulkAction::Delete => format!(
                "{} {} were gone already.",
                n(r.skipped),
                keys_word(r.skipped)
            ),
            BulkAction::Expire { .. } => format!(
                "{} {} left alone: a TTL already, or gone.",
                n(r.skipped),
                keys_word(r.skipped)
            ),
        });
    }
    if r.unreadable > 0 {
        lines.push(format!(
            "{} {} could not be read: left alone.",
            n(r.unreadable),
            keys_word(r.unreadable)
        ));
    }
    if r.moved > 0 {
        let t = bulk::moved_text(r.moved);
        let mut c = t.chars();
        let t: String = c
            .next()
            .map(|f| f.to_uppercase().chain(c).collect())
            .unwrap_or_default();
        lines.push(format!("{t}."));
    }
    if r.failed > 0 {
        lines.push(format!("{} {} failed.", n(r.failed), keys_word(r.failed)));
    }
    if let Some(f) = &r.file {
        let what = if b.nodes.is_empty() {
            "Key names"
        } else {
            "Key names, each after its node and a tab"
        };
        lines.push(format!("{what}: {}", escaped_file(Some(f))));
        if matches!(b.action, BulkAction::Expire { .. }) {
            lines.push("PERSIST on those keys undoes it.".to_string());
        }
    }
    b.stage = Stage::Done(Done {
        lines,
        error: why.is_some(),
        run: Some(Box::new(r)),
    });
}

/// `DELETE /api/bulk`: stops the bulk action: a dry run at once (nothing was sent), a
/// run after the batch on its way, a counted one waiting for its question closes; one
/// that ended is cleared.
pub async fn stop(State(app): State<App>) -> Response {
    let done = app.hub.with_mut(|c| {
        let conn = c.id;
        let Some(b) = c.bulk.as_mut() else {
            return (conn, None);
        };
        let text = match &mut b.stage {
            Stage::Counting { .. } => {
                b.stage = Stage::Done(Done {
                    lines: vec![DRY_RUN_STOPPED.to_string()],
                    error: false,
                    run: None,
                });
                DRY_RUN_STOPPED
            }
            Stage::Counted => {
                b.stage = Stage::Done(Done {
                    lines: vec![BULK_CANCELLED.to_string()],
                    error: false,
                    run: None,
                });
                BULK_CANCELLED
            }
            Stage::Running(r) => {
                r.stopping = true;
                STOPPING
            }
            Stage::Done(_) => {
                c.bulk = None;
                return (conn, Some("Closed."));
            }
        };
        (conn, Some(text))
    });
    let Some((conn, text)) = done else {
        return error(StatusCode::CONFLICT, super::NOT_LIVE);
    };
    emit(&app, conn);
    let mut body = app
        .hub
        .with(|c| reply_of(c.bulk.as_ref()))
        .unwrap_or_else(|| reply_of(None));
    body["text"] = serde_json::json!(text.unwrap_or("No bulk action is open."));
    ok(body)
}

/// The question about bulk action `bulk` was cancelled: it closes, nothing sent, as the
/// TUI's `n` closes its dialog.
pub fn cancelled(app: &App, conn: ConnId, bulk: u64) {
    let closed = app.hub.with_mut(|c| {
        let b = this(c, conn, Some(bulk))?;
        if !matches!(b.stage, Stage::Counted) {
            return None;
        }
        b.stage = Stage::Done(Done {
            lines: vec![crate::live::CANCELLED.to_string()],
            error: false,
            run: None,
        });
        Some(())
    });
    if closed.flatten().is_some() {
        emit(app, conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(action: BulkAction, nodes: usize) -> Bulk {
        Bulk {
            id: 1,
            conn: 1,
            run: 1,
            pattern: "cart:{id}".into(),
            glob: "cart:*".into(),
            action,
            profile: "cache".into(),
            prod: false,
            files_as: "cache".into(),
            nodes: (0..nodes)
                .map(|i| (i, format!("10.0.0.{i}:6379")))
                .collect(),
            at: 0,
            per_node: vec![Count::default(); nodes],
            count: Count::default(),
            stage: Stage::Counting { walk: Walk::Start },
        }
    }

    fn count(walk: Walk, matched: u64, with_ttl: u64) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Bulk(Reply::Count(Count {
            walk: Some(walk),
            matched,
            with_ttl,
            other_pattern: 0,
            unreadable: 0,
        })))
    }

    fn running(b: &mut Bulk) {
        let op = b.op();
        b.stage = Stage::Running(Box::new(Run {
            permit: Permit::for_test(op),
            walk: Walk::Start,
            carry: Vec::new(),
            seen: 0,
            sent: 0,
            changed: 0,
            skipped: 0,
            failed: 0,
            unreadable: 0,
            moved: 0,
            file: None,
            stopping: false,
        }));
    }

    fn batch(walk: Walk, changed: u64, failed: u64, denied: bool) -> Batch {
        Batch {
            walk,
            carry: Vec::new(),
            seen: changed + failed,
            sent: changed + failed,
            changed,
            skipped: 0,
            failed,
            denied,
            moved: 0,
            unreadable: 0,
            file: Some("/tmp/x/20261116T000000Z-delete.txt".into()),
        }
    }

    fn lines(b: &Bulk) -> Vec<String> {
        b.body()["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_dry_run_counts_every_primary_in_turn_then_waits_for_its_question() {
        let mut b = bulk(BulkAction::Delete, 2);
        assert!(counted_step(&mut b, count(Walk::At(9), 3, 0)));
        assert!(matches!(b.stage, Stage::Counting { walk: Walk::At(9) }));
        // The first primary is done: the second from its start.
        assert!(counted_step(&mut b, count(Walk::Done, 2, 1)));
        assert!(matches!(b.stage, Stage::Counting { walk: Walk::Start }));
        assert_eq!(b.at, 1);
        // No summary of a count still going: a dry run stopped now counted some keys only.
        assert_eq!(b.body()["summary"], serde_json::json!([]));
        assert!(!counted_step(&mut b, count(Walk::Done, 4, 0)));
        assert!(matches!(b.stage, Stage::Counted));
        assert_eq!(b.count.matched, 9);
        let body = b.body();
        assert_eq!(body["count"]["to_change"], 9, "{body}");
        assert_eq!(body["nodes"][0]["matched"], 5, "{body}");
        assert_eq!(body["nodes"][1]["matched"], 4, "{body}");
        assert_eq!(
            body["summary"][0],
            "Dry run: 9 keys of the pattern will be deleted with UNLINK."
        );
        let q = question(&b, &b.op(), &Decision::Ask).unwrap();
        assert_eq!(q.kind, "ask");
        assert!(
            q.text
                .starts_with("Delete every key of the pattern cart:{id} on cache? Dry run: 9 keys"),
            "{}",
            q.text
        );
        assert!(q.text.contains("This can't be undone."), "{}", q.text);
        assert!(q.text.contains("each of the 2 primaries"), "{}", q.text);
        let q = question(&b, &b.op(), &Decision::Type("cart:{id}".into())).unwrap();
        assert_eq!(q.kind, "type");
        assert_eq!(q.expect.as_deref(), Some("cart:{id}"));
        assert!(
            q.text.starts_with("Type cart:{id} to start: "),
            "{}",
            q.text
        );
    }

    #[test]
    fn nothing_to_change_says_so_and_asks_nothing() {
        let mut b = bulk(BulkAction::Expire { seconds: 86_400 }, 0);
        assert!(!counted_step(&mut b, count(Walk::Done, 4, 4)));
        assert_eq!(
            lines(&b),
            ["Every key of cart:{id} (4) has a TTL already. Nothing was sent."]
        );
        let mut b = bulk(BulkAction::Delete, 0);
        assert!(!counted_step(&mut b, count(Walk::Done, 0, 0)));
        assert_eq!(
            lines(&b),
            ["No keys of cart:{id} were found. Nothing was sent."]
        );
        let mut b = bulk(BulkAction::Delete, 0);
        assert!(!counted_step(&mut b, Err(LiveError::Closed)));
        assert_eq!(
            lines(&b),
            ["The dry run failed: the live connection is closed. Nothing was sent."]
        );
        assert_eq!(b.body()["error"], true);
    }

    #[test]
    fn batches_go_on_until_the_walk_ends_or_a_stop_or_a_refusal() {
        let mut b = bulk(BulkAction::Delete, 0);
        running(&mut b);
        let mut refuse = None;
        let ok = |x: Batch| Ok(LiveReply::Bulk(Reply::Batch(x)));
        assert!(batch_done(
            &mut b,
            ok(batch(Walk::At(3), 500, 0, false)),
            &mut refuse
        ));
        assert!(!batch_done(
            &mut b,
            ok(batch(Walk::Done, 20, 0, false)),
            &mut refuse
        ));
        let l = lines(&b);
        assert_eq!(l[0], "Done: deleted 520 keys of cart:{id}.");
        assert!(l[1].starts_with("Key names: /tmp/x/"), "{l:?}");
        assert_eq!(refuse, None);
        // A stop asked for: no batch after the one on its way.
        let mut b = bulk(BulkAction::Expire { seconds: 3600 }, 0);
        running(&mut b);
        if let Stage::Running(r) = &mut b.stage {
            r.stopping = true;
        }
        assert!(!batch_done(
            &mut b,
            ok(batch(Walk::At(3), 5, 0, false)),
            &mut refuse
        ));
        let l = lines(&b);
        assert_eq!(
            l[0],
            "Stopped after the batch: set a TTL of 1h on 5 keys of cart:{id}."
        );
        assert!(
            l.contains(&"PERSIST on those keys undoes it.".to_string()),
            "{l:?}"
        );
        // A NOPERM on every key: the command is off for the session, without a name.
        let mut b = bulk(BulkAction::Delete, 0);
        running(&mut b);
        assert!(!batch_done(
            &mut b,
            ok(batch(Walk::At(3), 0, 7, true)),
            &mut refuse
        ));
        assert_eq!(refuse.as_deref(), Some("UNLINK"));
        let l = lines(&b);
        assert_eq!(
            l[0],
            "Stopped: the server refused UNLINK for this user. Before that, deleted 0 keys of cart:{id}."
        );
        assert_eq!(b.body()["error"], true);
        // On a cluster the run goes on with the next primary from its own start.
        let mut b = bulk(BulkAction::Delete, 2);
        running(&mut b);
        let mut refuse = None;
        assert!(batch_done(
            &mut b,
            ok(batch(Walk::Done, 3, 0, false)),
            &mut refuse
        ));
        assert_eq!(b.at, 1);
        assert!(matches!(&b.stage, Stage::Running(r) if r.walk == Walk::Start));
        assert!(!batch_done(
            &mut b,
            ok(batch(Walk::Done, 4, 0, false)),
            &mut refuse
        ));
        assert_eq!(lines(&b)[0], "Done: deleted 7 keys of cart:{id}.");
        assert!(lines(&b)[1].starts_with("Key names, each after its node and a tab: "));
    }

    #[test]
    fn every_text_is_the_dashboards_own() {
        for t in [
            ONE_AT_A_TIME,
            NO_BULK,
            NOT_COUNTED,
            NOTHING_TO_RUN,
            RUNNING,
            BAD_ACTION,
            BAD_TTL,
            DRY_RUN_STOPPED,
            BULK_CANCELLED,
            STOPPING,
        ] {
            assert_eq!(crate::web::wording::banned(t), None, "{t}");
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
        // The TUI's bulk refusals pass as they are.
        for t in [
            bulk::REPLICA_OFF,
            bulk::CLUSTER_REPLICA_OFF,
            bulk::SLOTS_UNKNOWN,
        ] {
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
    }
}
