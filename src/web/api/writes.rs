//! Writes through the one write gate (spec "Write permissions"), and the questions the
//! dashboard holds for an answer (`POST /api/confirm/<id>`, `DELETE` to cancel).
//!
//! A write is a [`WriteOp`]: routed with the TUI's `write_dest`, decided with
//! [`write::decide_op`] (the profile, the action, the refusals), and sent only inside a
//! [`Permit`] that [`Permit::grant`] made, with the answer given. A decision of Ask or
//! Type is held as an [`Action::Write`] and the browser is asked; the answer is checked
//! by `grant` again, on the same connection and the same run only: a database switch
//! or an analysis started since the question refuses it ([`CHANGED`]), as the TUI drops
//! its question on a new run.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{App, Body, Question, Refused, SHAPE, error, ok};
use crate::live::{CANCELLED, Dest, LiveError, LiveReply};
use crate::tui::app::ConnId;
use crate::tui::app::views::write_dest;
use crate::web::dto;
use crate::write::{self, Answer, Decision, Permit, WriteOp};
use axum::response::IntoResponse;

/// What a held question does once answered.
pub enum Action {
    /// A write the gate asked about.
    Write {
        conn: ConnId,
        /// The run of the session the question was asked in ([`run_of`]).
        run: u64,
        op: WriteOp,
        dest: Dest,
        decision: Decision,
        /// For the command line: how its reply is drawn.
        cmd: Option<super::cmdline::Meta>,
    },
    /// A readonly `@dangerous` command (`KEYS`): sent once confirmed.
    Command(super::cmdline::Asked),
    /// `conn add` asking about a token command or about turning writes on.
    Add(Box<super::connections::AddAsked>),
    /// `conn set` asking before turning writes on.
    Set(super::connections::SetAsked),
    /// Monitor, which always asks before it starts; `expect` is the name to type.
    Monitor {
        conn: ConnId,
        expect: Option<String>,
    },
    /// Writes on for a container's session (it has no saved profile).
    SessionWrites { conn: ConnId },
    /// The same on a prod container: its name typed.
    SessionWritesTyped { conn: ConnId, expect: String },
}

/// Names only: the op's command, never its values.
impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Write { op, .. } => write!(f, "Write({})", op.command()),
            Action::Command(_) => f.write_str("Command"),
            Action::Add(_) => f.write_str("Add"),
            Action::Set(_) => f.write_str("Set"),
            Action::Monitor { .. } => f.write_str("Monitor"),
            Action::SessionWrites { .. } | Action::SessionWritesTyped { .. } => {
                f.write_str("SessionWrites")
            }
        }
    }
}

/// How a write went.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WriteReply {
    Ok,
    Int { value: i64 },
    Text { value: String },
    Changed,
    Exists,
    Claimed { ids: Vec<String>, deleted: usize },
    Ran { reply: dto::Reply },
}

impl WriteReply {
    pub fn of(r: &crate::live::write::Reply, pairs: bool) -> Self {
        use crate::live::write::Reply as R;
        match r {
            R::Ok => Self::Ok,
            R::Int(v) => Self::Int { value: *v },
            R::Text(s) => Self::Text { value: s.clone() },
            R::Changed => Self::Changed,
            R::Exists => Self::Exists,
            R::Claimed { ids, deleted } => Self::Claimed {
                ids: ids.clone(),
                deleted: *deleted,
            },
            R::Ran(out) => Self::Ran {
                reply: dto::Reply::of(out, pairs),
            },
        }
    }
}

/// The question to ask for `decision` about `op`, or `None` when none is asked.
pub fn question(op: &WriteOp, decision: &Decision, profile: &str) -> Option<Question> {
    let shown = op.shown();
    match decision {
        Decision::Ask => Some(Question::ask(format!("Send {shown} to {profile}?"))),
        Decision::Type(want) => Some(Question::typed(
            format!("Type {want} to send {shown} to {profile}."),
            want.clone(),
        )),
        Decision::Run | Decision::Refuse(_) => None,
    }
}

/// The connection and the run of the session a write or a question is about: a new run
/// (a database switch, an analysis) keeps the connection's id but not the run.
pub fn run_of(c: &crate::web::session::Current) -> (ConnId, u64) {
    (c.id, c.session.scan)
}

/// What [`decide`] found: where the write goes and what the gate says about it.
#[derive(Debug)]
pub struct Decided {
    pub conn: ConnId,
    pub run: u64,
    pub dest: Dest,
    pub decision: Decision,
    /// The profile's name, escaped, for the question.
    pub profile: String,
}

/// Where `op` goes and what the gate says about it on the current connection, or why it
/// is not sent.
pub fn decide(app: &App, op: &WriteOp) -> Result<Decided, Refused> {
    app.hub
        .with(|c| {
            if !c.live_ready() {
                return Err(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE));
            }
            let dest = write_dest(&c.session, op)
                .map_err(|why| Refused::new(StatusCode::UNPROCESSABLE_ENTITY, why))?;
            let decision = write::decide_op(op, c.profile(), &c.settings, &c.session.refusals);
            let decision = match decision {
                // The gate's words name the profile: kept as they are ([`holding`]).
                Decision::Refuse(why) => {
                    return Err(Refused::new(StatusCode::UNPROCESSABLE_ENTITY, why)
                        .holding(&profile_data(c.profile())));
                }
                d => d,
            };
            let (conn, run) = run_of(c);
            Ok(Decided {
                conn,
                run,
                dest,
                decision,
                profile: crate::value::escape(&c.profile().name),
            })
        })
        .unwrap_or_else(|| Err(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE)))
}

/// The permit for `op` on connection `conn` in run `run` with `answer`, from the gate
/// itself. Another connection, or a new run of it (a database switch, an analysis), is
/// refused with [`CHANGED`]: the write was decided for what the session was then.
pub fn grant(
    app: &App,
    (conn, run): (ConnId, u64),
    op: WriteOp,
    answer: &Answer,
) -> Result<Permit, Refused> {
    app.hub
        .with(|c| {
            if run_of(c) != (conn, run) {
                return Err(Refused::new(StatusCode::CONFLICT, CHANGED));
            }
            Permit::grant(op, c.profile(), &c.settings, &c.session.refusals, answer).map_err(
                |why| {
                    Refused::new(StatusCode::UNPROCESSABLE_ENTITY, why)
                        .holding(&profile_data(c.profile()))
                },
            )
        })
        .unwrap_or_else(|| Err(Refused::new(StatusCode::CONFLICT, CHANGED)))
}

/// The user data the gate's words can hold about `p`: its name and host.
pub fn profile_data(p: &crate::model::Profile) -> [&str; 2] {
    [p.name.as_str(), p.endpoint.host.as_str()]
}

/// Why an answer is not used: the connection it was about is gone, or the session
/// started a new run on it (another database, an analysis).
pub const CHANGED: &str = "Not sent: the connection changed since the question was asked.";

/// Sends a granted write and says how it went.
pub async fn send(app: &App, conn: ConnId, dest: Dest, permit: Permit) -> Response {
    match app.hub.write(conn, dest, permit).await {
        Ok(LiveReply::Write(r)) => ok(serde_json::json!({
            "sent": true,
            "reply": WriteReply::of(&r, false),
        })),
        Ok(_) => error(StatusCode::BAD_GATEWAY, SHAPE),
        Err(LiveError::Failed(s)) if s == CANCELLED => {
            error(StatusCode::UNPROCESSABLE_ENTITY, CANCELLED)
        }
        Err(LiveError::Closed) => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Err(e) => error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
    }
}

/// One write, start to end: refused, sent, or asked about.
pub async fn write_flow(app: &App, op: WriteOp) -> Response {
    app.hub.follow_saved_profile().await;
    let Decided {
        conn,
        run,
        dest,
        decision,
        profile,
    } = match decide(app, &op) {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    match &decision {
        Decision::Refuse(why) => error(StatusCode::UNPROCESSABLE_ENTITY, why),
        Decision::Run => match grant(app, (conn, run), op, &Answer::None) {
            Ok(p) => send(app, conn, dest, p).await,
            Err(r) => r.into_response(),
        },
        Decision::Ask | Decision::Type(_) => {
            let Some(q) = question(&op, &decision, &profile) else {
                return error(StatusCode::INTERNAL_SERVER_ERROR, SHAPE);
            };
            app.ask(
                Action::Write {
                    conn,
                    run,
                    op,
                    dest,
                    decision,
                    cmd: None,
                },
                q,
            )
        }
    }
}

/// The answer to a question.
#[derive(Default, Deserialize)]
pub struct Answered {
    #[serde(default)]
    pub typed: Option<String>,
}

/// The typed answer is the user's text: `Debug` leaves it out.
impl std::fmt::Debug for Answered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Answered").finish_non_exhaustive()
    }
}

/// Why an answer finds nothing to answer.
pub const GONE: &str = "that question is no longer open: it was answered, cancelled or expired";

/// `POST /api/confirm/<id>` `{"typed": "..."}`: answers the question; the action runs
/// with that answer, checked again.
pub async fn answer(
    State(app): State<App>,
    Path(id): Path<String>,
    Body(a): Body<Answered>,
) -> Response {
    // An answer to a command line's question runs that line: one at a time, as a typed
    // line does (`POST /api/cmdline` holds the same slot). Taken before the question is,
    // so a refused answer leaves the question open.
    let line = app
        .confirms()
        .peek_with(&id, |a| {
            matches!(a, Action::Command(_) | Action::Write { cmd: Some(_), .. })
        })
        .unwrap_or(false);
    let _slot = match line.then(|| app.hub.line_slot()) {
        Some(None) => return error(StatusCode::CONFLICT, super::cmdline::STILL_RUNNING),
        Some(slot) => slot,
        None => None,
    };
    let taken = app.confirms().take(&id, std::time::Instant::now());
    let Some(action) = taken else {
        return error(StatusCode::GONE, GONE);
    };
    match action {
        Action::Write {
            conn,
            run,
            op,
            dest,
            decision,
            cmd,
        } => {
            let answer = match decision {
                Decision::Type(_) => Answer::Typed(a.typed.unwrap_or_default()),
                _ => Answer::Yes,
            };
            // The profile may have gone read-only since the question was asked.
            app.hub.follow_saved_profile().await;
            let permit = match grant(&app, (conn, run), op, &answer) {
                Ok(p) => p,
                Err(r) => return r.into_response(),
            };
            match cmd {
                Some(meta) => super::cmdline::send_write(&app, conn, dest, permit, meta).await,
                None => send(&app, conn, dest, permit).await,
            }
        }
        Action::Command(asked) => super::cmdline::confirmed(&app, asked, a.typed).await,
        Action::Add(asked) => super::connections::add_answered(&app, *asked, a.typed).await,
        Action::Set(asked) => super::connections::set_answered(&app, asked, a.typed).await,
        Action::Monitor { conn, expect } => {
            super::server::monitor_answered(&app, conn, expect, a.typed).await
        }
        Action::SessionWrites { conn } => super::connections::session_writes_on(&app, conn),
        Action::SessionWritesTyped { conn, expect } => {
            super::connections::session_writes_typed(&app, conn, &expect, a.typed.as_deref())
        }
    }
}

/// `DELETE /api/confirm/<id>`: cancels the question; nothing is sent.
pub async fn cancel(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.confirms().take(&id, std::time::Instant::now()) {
        Some(_) => ok(serde_json::json!({ "cancelled": true, "text": CANCELLED })),
        None => error(StatusCode::GONE, GONE),
    }
}
