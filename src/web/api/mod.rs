//! The dashboard's JSON API (documented in `docs/dashboard-api.md`).
//!
//! Conventions: success is the resource object; a failure is a 4xx or 5xx with
//! `{"error": "<fixed wording>"}`; a write the gate asks about is a 409 with
//! `{"confirm": {"id", "kind", "text", "expect"}}`, answered at `/api/confirm/<id>`. A
//! panel whose feature the server refuses answers 200 with `{"off": "<why>"}` (rule 3:
//! one feature off, said in redis-lens's own words).
//!
//! Request bodies are read by [`Body`] and query strings by [`Q`], whose refusals are one
//! fixed sentence each: never the parser's text, which could quote a value from the
//! request (a password), and always JSON.

use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use axum::extract::{FromRef, FromRequest, FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, watch};

use super::assets::Page;
use super::auth::Guard;
use super::confirm::Pending;
use super::session::{Event, Hub};
use crate::live::LiveError;

pub mod analyze;
pub mod browse;
pub mod bulk;
pub mod cmdline;
pub mod connections;
pub mod edits;
pub mod hot;
pub mod keys;
pub mod overview;
pub mod pubsub;
pub mod search;
pub mod server;
pub mod snapshots;
pub mod streams;
pub mod values;
pub mod writes;

/// What every route can reach.
#[derive(Clone)]
pub struct App {
    pub hub: Arc<Hub>,
    pub confirms: Arc<Mutex<Pending<writes::Action>>>,
    pub page: Page,
    /// Turns true when the server shuts down: the event streams end then.
    pub shutdown: watch::Receiver<bool>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("hub", &self.hub)
            .field("page", &self.page)
            .finish_non_exhaustive()
    }
}

impl App {
    pub fn confirms(&self) -> MutexGuard<'_, Pending<writes::Action>> {
        self.confirms.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keeps `action` and answers with the question to put to the user (409).
    pub fn ask(&self, action: writes::Action, question: Question) -> Response {
        match self.confirms().insert(action, Instant::now()) {
            Ok(id) => confirm(&id, question),
            Err(_) => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not make a confirmation id",
            ),
        }
    }
}

impl FromRef<App> for Guard {
    fn from_ref(app: &App) -> Guard {
        app.page.guard.clone()
    }
}

impl FromRef<App> for Page {
    fn from_ref(app: &App) -> Page {
        app.page.clone()
    }
}

/// `{"error": text}` with `status`.
pub fn error(status: StatusCode, text: &str) -> Response {
    error_with(status, text, &[])
}

/// [`error`] of a text that holds user data (`data`: a profile name, a host), which the
/// wording keeps as it is ([`crate::web::wording::server_free_with`]).
pub fn error_with(status: StatusCode, text: &str, data: &[&str]) -> Response {
    let text = crate::web::wording::server_free_with(text, data);
    (status, axum::Json(serde_json::json!({ "error": text }))).into_response()
}

/// A refusal as a value small enough to return in a `Result`: a status and the fixed
/// words of `{"error": ...}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub status: StatusCode,
    pub text: String,
    /// User data the text holds ([`error_with`]).
    pub data: Vec<String>,
}

impl Refused {
    pub fn new(status: StatusCode, text: impl Into<String>) -> Self {
        Self {
            status,
            text: text.into(),
            data: Vec::new(),
        }
    }

    /// The same refusal, its text holding `data` ([`error_with`]).
    pub fn holding(mut self, data: &[&str]) -> Self {
        self.data = data.iter().map(|d| d.to_string()).collect();
        self
    }
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let data: Vec<&str> = self.data.iter().map(String::as_str).collect();
        error_with(self.status, &self.text, &data)
    }
}

/// `{"off": why}`: a panel's feature is off for this server or user.
pub fn off(why: &str) -> Response {
    let why = crate::web::wording::server_free(why);
    axum::Json(serde_json::json!({ "off": why })).into_response()
}

/// 200 with `body` as JSON.
pub fn ok(body: impl Serialize) -> Response {
    axum::Json(body).into_response()
}

/// A question for the user: `ask` (yes or no) or `type` (the text in `expect`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Question {
    pub kind: &'static str,
    pub text: String,
    pub expect: Option<String>,
}

impl Question {
    pub fn ask(text: impl Into<String>) -> Self {
        Self {
            kind: "ask",
            text: text.into(),
            expect: None,
        }
    }

    pub fn typed(text: impl Into<String>, expect: impl Into<String>) -> Self {
        Self {
            kind: "type",
            text: text.into(),
            expect: Some(expect.into()),
        }
    }
}

/// 409 `{"confirm": {"id", "kind", "text", "expect"}}`.
pub fn confirm(id: &str, q: Question) -> Response {
    (
        StatusCode::CONFLICT,
        axum::Json(serde_json::json!({
            "confirm": { "id": id, "kind": q.kind, "text": q.text, "expect": q.expect }
        })),
    )
        .into_response()
}

/// What a failed live request means for a panel: off (refused, unsupported), no live
/// connection (409), or a failure (502). Always redis-lens's own words.
pub fn live_error(e: &LiveError) -> Response {
    match e {
        LiveError::Denied(_) | LiveError::Unsupported(_) => off(&e.to_string()),
        LiveError::Closed => error(StatusCode::CONFLICT, NOT_LIVE),
        LiveError::Failed(_) => error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

/// Why a panel can't read: no live connection.
pub const NOT_LIVE: &str = "not connected: connect first, or the live connection is closed";
/// Why a request body was refused.
pub const BAD_BODY: &str = "the request body is not the JSON this route takes";
/// Why a query string was refused: a parameter missing, or not of its type.
pub const BAD_QUERY: &str = "the query parameters are not the ones this route takes";
/// An answer of an unexpected shape.
pub const SHAPE: &str = "the live connection gave an answer of another kind";

/// A JSON request body. An empty body reads as `{}`. A refusal says [`BAD_BODY`] only.
pub struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|_| error(StatusCode::BAD_REQUEST, BAD_BODY))?;
        let text: &[u8] = if bytes.iter().all(u8::is_ascii_whitespace) {
            b"{}"
        } else {
            &bytes
        };
        serde_json::from_slice(text)
            .map(Body)
            .map_err(|_| error(StatusCode::BAD_REQUEST, BAD_BODY))
    }
}

/// A query string read into `T`. A parameter missing or of the wrong type is refused
/// with 400 `{"error": BAD_QUERY}`, never axum's plain-text rejection (which quotes the
/// parser's text).
pub struct Q<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Q<T> {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::try_from_uri(&parts.uri)
            .map(|axum::extract::Query(t)| Q(t))
            .map_err(|_| error(StatusCode::BAD_REQUEST, BAD_QUERY))
    }
}

/// `GET /api/state`.
pub async fn state(State(app): State<App>) -> Response {
    // Writes on or off as the config file says now.
    app.hub.follow_saved_profile().await;
    ok(app.hub.state())
}

/// `GET /api/events`: the one event stream. Starts with the current `state`; then
/// `state`, `figures`, `overview`, `progress`, `bulk`, `tail`, `monitor` and `notice` as
/// they happen. While at least
/// one stream is open the live figures are polled (`overview::poll`).
pub async fn events(
    State(app): State<App>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let listener = app.hub.listen();
    let first = Event::new("state", app.hub.state());
    let shutdown = app.shutdown.clone();
    let stream = futures::stream::unfold(
        (listener, Some(first), shutdown),
        |(mut l, first, mut shutdown)| async move {
            if let Some(f) = first {
                return Some((Ok(sse(&f)), (l, None, shutdown)));
            }
            if *shutdown.borrow() {
                return None;
            }
            let next = tokio::select! {
                r = l.rx.recv() => r,
                _ = shutdown.wait_for(|v| *v) => return None,
            };
            let e = match next {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(n)) => Event::notice(format!(
                    "{n} live updates were dropped: this page reads slower than they come."
                )),
                Err(broadcast::error::RecvError::Closed) => return None,
            };
            Some((Ok(sse(&e)), (l, None, shutdown)))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn sse(e: &Event) -> SseEvent {
    SseEvent::default()
        .event(e.name)
        .data(serde_json::to_string(&e.data).unwrap_or_else(|_| "null".into()))
}

/// The routes no other one answers: 404 as JSON.
pub async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}
