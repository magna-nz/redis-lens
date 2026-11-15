//! The command line (spec "Command line panel"), with the TUI panel's rules: the line is
//! split like `redis-cli` ([`command::split`]), a leading count repeats it
//! ([`cmdline::parse_repeat`], at most [`cmdline::MAX_REPEAT`]), [`command::refused`]
//! and [`cmdline::gate`] decide with what `COMMAND INFO` said (read once per name a
//! session): readonly commands are sent, `@dangerous` ones ask first, scripts, blocking
//! commands and module commands not known to read are never sent, and a write goes
//! through the write gate ([`super::writes`]) and is repeated only when the TUI would
//! repeat it ([`repeatable`]). The live connection checks readonly commands again
//! ([`cmdline::run`]). Commands go where the TUI sends them (`command_to`).

use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::writes::{self, Action};
use super::{App, Body, Q, Question, SHAPE, error, ok};
use crate::live::cmdline::{self, Args, CommandInfo, Gate, Reply as CmdReply, Request};
use crate::live::command;
use crate::live::{CANCELLED, Dest, LiveError, LiveReply, LiveRequest};
use crate::model::Env;
use crate::tui::app::ConnId;
use crate::tui::app::views::cmdline::{
    HELPER_OLD, MAX_INPUT, MAX_OUTPUT_BYTES, REPEAT_ASKS, name_of, repeatable,
};
use crate::tui::app::views::{command_to, write_dest};
use crate::web::dto;
use crate::write::{Answer, Decision, WriteOp};

/// How a line's replies are shown: the masked line, the node it went to.
#[derive(Debug, Clone)]
pub struct Meta {
    pub line: String,
    pub pairs: bool,
    pub node: Option<String>,
    pub route_note: Option<&'static str>,
    pub times: u32,
}

/// A readonly `@dangerous` command waiting for its confirmation.
pub struct Asked {
    conn: ConnId,
    /// The run of the session it was asked in: a new run refuses the answer.
    run: u64,
    args: Args,
    info: CommandInfo,
    dest: Dest,
    pipeline: bool,
    /// The profile's name, typed on a `prod` profile.
    expect: Option<String>,
    meta: Meta,
}

/// What a typed line gave.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Ran {
    /// The line as the history keeps it: credentials masked.
    pub line: String,
    /// `ok` (replies), `note` (not sent, and why) or `failed`.
    pub status: &'static str,
    /// In the dashboard's terms ([`crate::web::wording::server_free`]).
    #[serde(serialize_with = "dashboard_words")]
    pub text: Option<String>,
    /// The cluster node it went to.
    pub node: Option<String>,
    #[serde(serialize_with = "dashboard_words")]
    pub route_note: Option<&'static str>,
    pub times: u32,
    pub sent: u32,
    pub replies: Vec<dto::Reply>,
    /// The oldest replies of a repeat dropped to keep the output under the TUI's
    /// [`MAX_OUTPUT_BYTES`], and the TUI's notice about it (`None` when none was).
    pub dropped: u32,
    pub dropped_note: Option<String>,
    /// The bytes of each reply kept, as the TUI's cap counts them.
    #[serde(skip)]
    sizes: Vec<usize>,
}

fn dashboard_words<S: serde::Serializer, T: AsRef<str>>(
    t: &Option<T>,
    s: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(
        &crate::web::wording::server_free_opt(t.as_ref().map(AsRef::as_ref)),
        s,
    )
}

impl Ran {
    fn new(meta: &Meta) -> Self {
        Self {
            line: meta.line.clone(),
            status: "ok",
            text: None,
            node: meta.node.clone(),
            route_note: meta.route_note,
            times: meta.times,
            sent: 0,
            replies: Vec::new(),
            dropped: 0,
            dropped_note: None,
            sizes: Vec::new(),
        }
    }

    fn note(line: String, why: impl Into<String>) -> Self {
        Self {
            line,
            status: "note",
            text: Some(why.into()),
            node: None,
            route_note: None,
            times: 1,
            sent: 0,
            replies: Vec::new(),
            dropped: 0,
            dropped_note: None,
            sizes: Vec::new(),
        }
    }

    /// Keeps the reply `o`; past [`MAX_OUTPUT_BYTES`] the oldest go, as in the TUI's
    /// panel, and the TUI's notice says how many.
    fn push(&mut self, o: &crate::live::cmdline::Output, pairs: bool) {
        self.replies.push(dto::Reply::of(o, pairs));
        self.sizes.push(o.bytes());
        while self.replies.len() > 1 && self.sizes.iter().sum::<usize>() > MAX_OUTPUT_BYTES {
            self.replies.remove(0);
            self.sizes.remove(0);
            self.dropped += 1;
        }
        if self.dropped > 0 {
            let runs = crate::tui::app::views::cmdline::Runs {
                times: self.times,
                sent: self.sent,
                outs: Vec::new(),
                dropped: self.dropped,
                stopping: false,
                end: None,
            };
            self.dropped_note = Some(crate::tui::app::views::cmdline::dropped_text(&runs));
        }
    }

    fn with(mut self, status: &'static str, why: impl Into<String>) -> Self {
        self.status = status;
        self.text = Some(why.into());
        self
    }
}

/// `POST /api/cmdline` body.
#[derive(Deserialize)]
pub struct Line {
    pub line: String,
    /// Pipeline mode: a readonly repeat goes in pipelines.
    #[serde(default)]
    pub pipeline: bool,
}

/// The typed line is the user's: `Debug` leaves it out.
impl std::fmt::Debug for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Line")
            .field("pipeline", &self.pipeline)
            .finish_non_exhaustive()
    }
}

const NOT_OPEN: &str = "Not sent: the live connection is not open.";

/// What `COMMAND INFO` says of `name`, read once a session.
async fn info_of(app: &App, conn: ConnId, name: &str) -> Result<Option<CommandInfo>, LiveError> {
    let known = app
        .hub
        .with(|c| (c.id == conn).then(|| c.infos.get(name).cloned()).flatten())
        .flatten();
    if let Some(i) = known {
        return Ok(i);
    }
    let picked = app.hub.with(|c| c.session.picked).unwrap_or(0);
    let req = LiveRequest::Command(Request::Info {
        name: name.to_string(),
    });
    match app.hub.request(Some(conn), Dest::node(picked), req).await? {
        LiveReply::Command(CmdReply::Info(i)) => {
            app.hub.with_mut(|c| {
                if c.id == conn {
                    c.infos.insert(name.to_string(), i.clone());
                }
            });
            Ok(i)
        }
        _ => Err(LiveError::Failed(SHAPE.into())),
    }
}

/// Why a line is not run: it is longer than the TUI's panel takes.
pub const TOO_LONG: &str = "Not sent: a line holds at most 8,192 characters.";

/// Why a line is not run: the one before it has not finished.
pub const STILL_RUNNING: &str =
    "Not sent: the line before it is still running. Wait for its reply, then send it again.";

/// `POST /api/cmdline` `{"line": "...", "pipeline": false}`: runs a typed line as the
/// TUI's panel does.
pub async fn run(State(app): State<App>, Body(l): Body<Line>) -> Response {
    if l.line.chars().count() > MAX_INPUT {
        return error(StatusCode::PAYLOAD_TOO_LARGE, TOO_LONG);
    }
    let Some(_slot) = app.hub.line_slot() else {
        return error(StatusCode::CONFLICT, STILL_RUNNING);
    };
    let all = match command::split(&l.line) {
        Ok(a) => a,
        Err(e) => {
            return ok(Ran::note(
                crate::tui::app::views::cmdline::masked_input(&l.line),
                e.message(),
            ));
        }
    };
    let line = cmdline::masked_with_count(&all);
    let times = match cmdline::parse_repeat(&all) {
        Ok(n) => n.unwrap_or(1),
        Err(why) => return ok(Ran::note(line, why)),
    };
    let args = all[cmdline::count_words(&all)..].to_vec();
    if args.is_empty() {
        return ok(Ran::note(line, "Nothing to send."));
    }
    if let Some(why) = command::refused(&args) {
        return ok(Ran::note(line, why));
    }
    let Some((conn, run)) = app
        .hub
        .with(|c| c.live_ready().then(|| writes::run_of(c)))
        .flatten()
    else {
        return ok(Ran::note(line, NOT_OPEN).with("failed", NOT_OPEN));
    };
    let Some(name) = name_of(&args[0]) else {
        let why = format!(
            "Not sent: the server has no command {}.",
            cmdline::typed_name(&args)
        );
        return ok(Ran::note(line, why));
    };
    let info = match info_of(&app, conn, &name).await {
        Ok(i) => i,
        Err(e) => return ok(Ran::note(line, e.to_string()).with("failed", e.to_string())),
    };
    let pairs = cmdline::pairs_hint(&args);
    let args = Args::new(args);
    let mut meta = Meta {
        line,
        pairs,
        node: None,
        route_note: None,
        times,
    };
    match cmdline::gate(args.as_slice(), info.as_ref()) {
        Gate::Refuse(why) => ok(Ran::note(meta.line, why)),
        Gate::Write => {
            let Some(info) = info else {
                return ok(Ran::note(meta.line, cmdline::NOT_SENT));
            };
            // The node, for the reply; the write itself goes where `write_dest` says.
            if let Err(r) = route(&app, conn, &args, &info, &mut meta) {
                return *r;
            }
            let op = WriteOp::Command { args, info };
            app.hub.follow_saved_profile().await;
            let decided = app.hub.with(|c| {
                let dest = write_dest(&c.session, &op);
                let d = crate::write::decide_op(&op, c.profile(), &c.settings, &c.session.refusals);
                (dest, d, crate::value::escape(&c.profile().name))
            });
            let Some((wdest, decision, profile)) = decided else {
                return ok(Ran::note(meta.line, NOT_OPEN).with("failed", NOT_OPEN));
            };
            let dest = match wdest {
                Ok(d) => d,
                Err(why) => return ok(Ran::note(meta.line, why)),
            };
            if let Decision::Refuse(why) = &decision {
                return ok(Ran::note(meta.line, why.clone()));
            }
            if times > 1 {
                if !repeatable(&op, &decision) {
                    return ok(Ran::note(meta.line, REPEAT_ASKS));
                }
                return ok(repeat_writes(&app, (conn, run), op, dest, &meta).await);
            }
            match &decision {
                Decision::Run => match writes::grant(&app, (conn, run), op, &Answer::None) {
                    Ok(permit) => send_write(&app, conn, dest, permit, meta).await,
                    Err(r) => r.into_response(),
                },
                _ => {
                    let Some(q) = writes::question(&op, &decision, &profile) else {
                        return error(StatusCode::INTERNAL_SERVER_ERROR, SHAPE);
                    };
                    app.ask(
                        Action::Write {
                            conn,
                            run,
                            op,
                            dest,
                            decision,
                            cmd: Some(meta),
                            edit: None,
                        },
                        q,
                    )
                }
            }
        }
        Gate::Send => {
            let Some(info) = info else {
                return ok(Ran::note(meta.line, cmdline::NOT_SENT));
            };
            let dest = match route(&app, conn, &args, &info, &mut meta) {
                Ok(d) => d,
                Err(r) => return *r,
            };
            ok(reads(&app, conn, args, info, dest, false, l.pipeline, &meta).await)
        }
        Gate::Ask => {
            let Some(info) = info else {
                return ok(Ran::note(meta.line, cmdline::NOT_SENT));
            };
            // Keys in different slots: refused before asking.
            let dest = match route(&app, conn, &args, &info, &mut meta) {
                Ok(d) => d,
                Err(r) => return *r,
            };
            let Some((prod, profile)) = app
                .hub
                .with(|c| (c.profile().env == Env::Prod, c.profile().name.clone()))
            else {
                return ok(Ran::note(meta.line, NOT_OPEN).with("failed", NOT_OPEN));
            };
            let cmd = args
                .as_slice()
                .get(1)
                .and_then(|s| info.subcommand(s))
                .unwrap_or(&info)
                .display();
            let what = if times > 1 {
                format!("{times} × {cmd}")
            } else {
                cmd
            };
            let line = command::masked_line(args.as_slice());
            let q = if prod {
                Question::typed(
                    format!(
                        "{what} is @dangerous: it can slow the server down. Type the profile name {} to send it: {line}",
                        crate::value::escape(&profile)
                    ),
                    profile.clone(),
                )
            } else {
                Question::ask(format!(
                    "{what} is @dangerous: it can slow the server down. Send it? {line}"
                ))
            };
            app.ask(
                Action::Command(Asked {
                    conn,
                    run,
                    args,
                    info,
                    dest,
                    pipeline: l.pipeline,
                    expect: prod.then_some(profile),
                    meta,
                }),
                q,
            )
        }
    }
}

/// Where the line goes (the TUI's `command_to`), with the node noted in `meta`.
fn route(
    app: &App,
    conn: ConnId,
    args: &Args,
    info: &CommandInfo,
    meta: &mut Meta,
) -> Result<Dest, Box<Response>> {
    let to = app
        .hub
        .with(|c| {
            if c.id != conn {
                return Err(writes::CHANGED.to_string());
            }
            let to = command_to(&c.session, args.as_slice(), info)?;
            let node = to
                .node
                .and_then(|i| c.session.nodes.get(i))
                .map(|n| crate::value::escape(&n.endpoint.to_string()));
            Ok((to, node))
        })
        .unwrap_or_else(|| Err(NOT_OPEN.to_string()));
    match to {
        Ok((to, node)) => {
            meta.node = node;
            meta.route_note = to.note;
            Ok(to.dest)
        }
        Err(why) => Err(Box::new(ok(Ran::note(meta.line.clone(), why)))),
    }
}

/// A readonly command `meta.times` times, each one checked by [`cmdline::gate`] right
/// before it is sent (and again by the live connection); with `pipeline`, in pipelines
/// of [`cmdline::pipeline_batch`].
#[allow(clippy::too_many_arguments)]
async fn reads(
    app: &App,
    conn: ConnId,
    args: Args,
    info: CommandInfo,
    dest: Dest,
    confirmed: bool,
    pipeline: bool,
    meta: &Meta,
) -> Ran {
    let mut out = Ran::new(meta);
    let rate = app
        .hub
        .with(|c| crate::scan::ScanOptions::for_profile(c.profile()).max_commands_per_sec)
        .flatten();
    while out.sent < meta.times {
        match cmdline::gate(args.as_slice(), Some(&info)) {
            Gate::Send => {}
            Gate::Ask if confirmed => {}
            Gate::Refuse(why) => return out.with("failed", why),
            Gate::Ask | Gate::Write => return out.with("failed", cmdline::NOT_SENT),
        }
        let n = if pipeline && meta.times > 1 {
            cmdline::pipeline_batch(rate, meta.times - out.sent)
        } else {
            1
        };
        let req = if n > 1 {
            Request::Batch {
                args: args.clone(),
                info: info.clone(),
                confirmed,
                times: n,
            }
        } else {
            Request::Run {
                args: args.clone(),
                info: info.clone(),
                confirmed,
            }
        };
        match app
            .hub
            .request(Some(conn), dest.clone(), LiveRequest::Command(req))
            .await
        {
            Ok(LiveReply::Command(CmdReply::Ran(o))) => {
                out.push(&o, meta.pairs);
            }
            Ok(LiveReply::Command(CmdReply::Batch(os))) => {
                for o in &os {
                    out.push(o, meta.pairs);
                }
            }
            Ok(_) => return out.with("failed", SHAPE),
            Err(e) => return out.with("failed", e.to_string()),
        }
        out.sent += n;
    }
    out
}

/// A write repeated: each one decided and granted by the write gate right before it is
/// sent, one at a time.
async fn repeat_writes(
    app: &App,
    (conn, run): (ConnId, u64),
    op: WriteOp,
    dest: Dest,
    meta: &Meta,
) -> Ran {
    let mut out = Ran::new(meta);
    while out.sent < meta.times {
        app.hub.follow_saved_profile().await;
        let decision = app.hub.with(|c| {
            (writes::run_of(c) == (conn, run)).then(|| {
                crate::write::decide_op(&op, c.profile(), &c.settings, &c.session.refusals)
            })
        });
        let Some(Some(decision)) = decision else {
            return out.with("failed", writes::CHANGED);
        };
        if let Decision::Refuse(why) = decision {
            return out.with("failed", why);
        }
        if !repeatable(&op, &decision) {
            return out.with("failed", REPEAT_ASKS);
        }
        let permit = match writes::grant(app, (conn, run), op.clone(), &Answer::None) {
            Ok(p) => p,
            Err(_) => return out.with("failed", cmdline::NOT_SENT),
        };
        match app.hub.write(conn, dest.clone(), permit).await {
            Ok(LiveReply::Write(crate::live::write::Reply::Ran(o))) => {
                out.push(&o, meta.pairs);
            }
            Ok(_) => return out.with("failed", SHAPE),
            Err(e) => return out.with("failed", e.to_string()),
        }
        out.sent += 1;
    }
    out
}

/// Sends one granted command-line write and answers with its reply.
pub async fn send_write(
    app: &App,
    conn: ConnId,
    dest: Dest,
    permit: crate::write::Permit,
    meta: Meta,
) -> Response {
    let mut out = Ran::new(&meta);
    match app.hub.write(conn, dest, permit).await {
        Ok(LiveReply::Write(crate::live::write::Reply::Ran(o))) => {
            out.push(&o, meta.pairs);
            out.sent = 1;
        }
        Ok(_) => out = out.with("failed", SHAPE),
        Err(LiveError::Failed(s)) if s == CANCELLED => out = out.with("note", s),
        Err(e) => out = out.with("failed", e.to_string()),
    }
    ok(out)
}

/// The answer to a `@dangerous` readonly command's question: on `prod` the profile's
/// name must be typed, as in the TUI.
pub async fn confirmed(app: &App, a: Asked, typed: Option<String>) -> Response {
    // Asked about another run of the session (another database): not sent there.
    if app.hub.with(writes::run_of) != Some((a.conn, a.run)) {
        return error(StatusCode::CONFLICT, writes::CHANGED);
    }
    if let Some(want) = &a.expect
        && typed.as_deref() != Some(want.as_str())
    {
        return ok(Ran::new(&a.meta).with(
            "note",
            "Not sent: the name typed did not match.".to_string(),
        ));
    }
    let out = reads(
        app, a.conn, a.args, a.info, a.dest, true, a.pipeline, &a.meta,
    )
    .await;
    ok(out)
}

/// `GET /api/cmdline/commands`: the command names for completion (`COMMAND LIST`, read
/// once a session).
pub async fn commands(State(app): State<App>) -> Response {
    let Some((conn, known, picked)) = app
        .hub
        .with(|c| {
            c.live_ready()
                .then(|| (c.id, c.names.clone(), c.session.picked))
        })
        .flatten()
    else {
        return error(StatusCode::CONFLICT, super::NOT_LIVE);
    };
    if let Some(names) = known {
        return ok(serde_json::json!({ "names": names }));
    }
    let req = LiveRequest::Command(Request::Names);
    match app.hub.request(Some(conn), Dest::node(picked), req).await {
        Ok(LiveReply::Command(CmdReply::Names(names))) => {
            let names: Vec<String> = names.iter().map(|n| crate::value::escape(n)).collect();
            app.hub.with_mut(|c| {
                if c.id == conn {
                    c.names = Some(names.clone());
                }
            });
            ok(serde_json::json!({ "names": names }))
        }
        Ok(_) => error(StatusCode::BAD_GATEWAY, SHAPE),
        Err(e) => super::live_error(&e),
    }
}

/// A command's help (`COMMAND DOCS`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Help {
    pub name: String,
    pub summary: String,
    pub since: String,
    pub complexity: String,
    pub arguments: String,
    pub subcommands: Vec<Help>,
}

impl Help {
    fn of(name: &str, d: &cmdline::Docs) -> Self {
        Self {
            name: crate::value::escape(name),
            summary: d.summary.clone(),
            since: d.since.clone(),
            complexity: d.complexity.clone(),
            arguments: d.arguments.clone(),
            subcommands: d
                .subcommands
                .iter()
                .map(|(n, sd)| Help::of(n, sd))
                .collect(),
        }
    }
}

/// `GET /api/cmdline/help?name=<command>`: what `COMMAND DOCS` says (Redis 7.0 and
/// later, and Valkey); `{"off": ...}` on an older server.
pub async fn help(State(app): State<App>, Q(q): Q<HashMap<String, String>>) -> Response {
    let Some(name) = q.get("name").and_then(|n| name_of(n.as_bytes())) else {
        return error(
            StatusCode::BAD_REQUEST,
            "give the command's name: ?name=GET",
        );
    };
    let Some((conn, picked)) = app
        .hub
        .with(|c| c.live_ready().then_some((c.id, c.session.picked)))
        .flatten()
    else {
        return error(StatusCode::CONFLICT, super::NOT_LIVE);
    };
    let req = LiveRequest::Command(Request::Docs { name: name.clone() });
    match app.hub.request(Some(conn), Dest::node(picked), req).await {
        Ok(LiveReply::Command(CmdReply::Docs(Some(d)))) => ok(Help::of(&name, &d)),
        Ok(LiveReply::Command(CmdReply::Docs(None))) => error(
            StatusCode::NOT_FOUND,
            "the server has no docs for that name",
        ),
        Ok(_) => error(StatusCode::BAD_GATEWAY, SHAPE),
        Err(LiveError::Unsupported(_)) => super::off(HELPER_OLD),
        Err(e) => super::live_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S4: a repeat's replies past the TUI's 4 MiB cap drop the oldest first, with the
    /// TUI's notice; one reply is always kept.
    #[test]
    fn a_repeats_output_keeps_under_the_tuis_cap() {
        let meta = Meta {
            line: "8 GET k".into(),
            pairs: false,
            node: None,
            route_note: None,
            times: 8,
        };
        let mut out = Ran::new(&meta);
        let big = redis::Value::BulkString(vec![b'a'; MAX_OUTPUT_BYTES / 6]);
        let o = cmdline::Output::of(&big, std::time::Duration::from_micros(1));
        for _ in 0..8 {
            out.push(&o, false);
        }
        assert!(out.dropped > 0, "{}", out.dropped);
        assert_eq!(out.replies.len() + out.dropped as usize, 8);
        assert!(out.sizes.iter().sum::<usize>() <= MAX_OUTPUT_BYTES);
        let note = out.dropped_note.clone().unwrap();
        assert_eq!(
            note,
            format!(
                "(the first {} replies were dropped: the output keeps at most 4 MiB)",
                out.dropped
            )
        );
        // A small repeat drops nothing and says nothing.
        let mut small = Ran::new(&meta);
        let one = cmdline::Output::of(&redis::Value::Int(1), std::time::Duration::ZERO);
        for _ in 0..8 {
            small.push(&one, false);
        }
        assert_eq!((small.dropped, small.replies.len()), (0, 8));
        assert!(small.dropped_note.is_none());
        let v = serde_json::to_value(&small).unwrap();
        assert_eq!(v["dropped"], 0);
        assert!(v["dropped_note"].is_null());
        assert!(v.get("sizes").is_none());
    }

    #[test]
    fn the_input_cap_is_the_tuis() {
        assert_eq!(MAX_INPUT, 8_192);
        assert!(TOO_LONG.contains("8,192"));
    }
}
