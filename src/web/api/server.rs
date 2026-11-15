//! 3 Server: Clients (grouped as the TUI groups them), Slow log (arguments masked as
//! the TUI masks them), and Monitor (time-boxed as in the TUI; its lines come as
//! `monitor` events). Each reads the picked node of a cluster.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::writes::Action;
use super::{App, Question, SHAPE, error, live_error, off, ok};
use crate::conn::{PERM_CLIENT_LIST, PERM_SLOWLOG};
use crate::live::server::{Reply, Request};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest};
use crate::model::PermStatus;
use crate::tui::app::ConnId;
use crate::tui::app::views::server::monitor_asks_name;
use crate::web::dto;

/// Why the permission check turned `label` off, if it did: the check's own words.
fn probe_off(app: &App, label: &str) -> Option<String> {
    app.hub
        .with(|c| {
            let check = c.perms.as_ref()?.checks.iter().find(|p| p.label == label)?;
            match &check.status {
                PermStatus::Allowed => None,
                PermStatus::Denied(_) => Some(LiveError::Denied(label.to_string()).to_string()),
                PermStatus::Skipped(why) => Some(crate::value::escape(why)),
            }
        })
        .flatten()
}

fn picked(app: &App) -> Option<(ConnId, usize)> {
    app.hub
        .with(|c| c.live_ready().then_some((c.id, c.session.picked)))
        .flatten()
}

/// `GET /api/server/clients`: `CLIENT LIST` of the picked node, with its groups.
pub async fn clients(State(app): State<App>) -> Response {
    let Some((conn, node)) = picked(&app) else {
        return error(StatusCode::CONFLICT, super::NOT_LIVE);
    };
    if let Some(why) = probe_off(&app, PERM_CLIENT_LIST) {
        return off(&why);
    }
    let req = LiveRequest::Server(Request::Clients);
    match app.hub.request(Some(conn), Dest::node(node), req).await {
        Ok(LiveReply::Server(Reply::Clients(list))) => {
            let body = dto::Clients::of(Some(node), &list);
            app.hub.with_mut(|c| {
                if c.id == conn && c.session.picked == node {
                    c.client_conns = list;
                }
            });
            ok(body)
        }
        Ok(_) => error(StatusCode::BAD_GATEWAY, SHAPE),
        Err(e) => live_error(&e),
    }
}

/// `GET /api/server/slowlog`: `SLOWLOG GET 128` of the picked node, with its settings
/// and the key names its entries name, as the TUI's Slow log panel reads them.
pub async fn slowlog(State(app): State<App>) -> Response {
    let Some((conn, node)) = picked(&app) else {
        return error(StatusCode::CONFLICT, super::NOT_LIVE);
    };
    if let Some(why) = probe_off(&app, PERM_SLOWLOG) {
        return off(&why);
    }
    let Some((specs, config, keys)) = app
        .hub
        .with(|c| (c.slow_specs.clone(), c.slow_config, c.slow_keys))
    else {
        return error(StatusCode::CONFLICT, super::NOT_LIVE);
    };
    let req = LiveRequest::Server(Request::SlowLog {
        specs,
        config,
        keys,
    });
    match app.hub.request(Some(conn), Dest::node(node), req).await {
        Ok(LiveReply::Server(Reply::SlowLog(r))) => {
            let now_off = |e: &LiveError| crate::web::wording::server_free(&e.to_string());
            let config_off = match &r.config {
                Some(Err(e)) => Some(now_off(e)),
                _ => None,
            };
            let keys_now_off = r.keys_off.as_ref().map(now_off);
            // What was found out before: the settings and key names are not asked for
            // again, and every reply still says why.
            let (settings_off, keys_off) = app
                .hub
                .with_mut(|c| {
                    if c.id != conn {
                        return (config_off.clone(), keys_now_off.clone());
                    }
                    c.slow_specs.extend(r.learned.clone());
                    if let Some(why) = &config_off {
                        c.slow_config = false;
                        c.slow_config_off = Some(why.clone());
                    }
                    if let Some(why) = &keys_now_off {
                        c.slow_keys = false;
                        c.slow_keys_off = Some(why.clone());
                    }
                    if c.session.picked == node {
                        c.slow_entries = r.entries.clone();
                    }
                    (c.slow_config_off.clone(), c.slow_keys_off.clone())
                })
                .unwrap_or((config_off, keys_now_off));
            let settings = match &r.config {
                Some(Ok(s)) => Some(serde_json::json!({
                    "threshold_micros": s.threshold_micros,
                    "max_len": s.max_len,
                })),
                _ => None,
            };
            let settings_off = if settings.is_some() {
                None
            } else {
                Some(settings_off.unwrap_or_else(|| SETTINGS_UNKNOWN.to_string()))
            };
            ok(serde_json::json!({
                "node": node,
                "entries": r.entries.iter().map(dto::Slow::of).collect::<Vec<_>>(),
                "settings": settings,
                "settings_off": settings_off,
                "keys_off": keys_off,
            }))
        }
        Ok(_) => error(StatusCode::BAD_GATEWAY, SHAPE),
        Err(e) => live_error(&e),
    }
}

/// Why the slow log's settings are missing when no reason was ever given.
pub const SETTINGS_UNKNOWN: &str = "the slow log's settings were not read";

/// `POST /api/server/monitor`: asks first, always (409), as the TUI does: yes or no,
/// or on a `prod` profile its name typed (the TUI's own rule,
/// `views::server::monitor_asks_name`), read-only profiles included. The answer
/// (`POST /api/confirm/<id>`) starts it on the picked node; it stops by itself after
/// `live::monitor::MAX_TIME` or `MAX_LINES` lines.
pub async fn monitor_start(State(app): State<App>) -> Response {
    let (conn, profile) = match app.hub.monitor_check() {
        Ok(c) => c,
        Err(why) => return error(StatusCode::UNPROCESSABLE_ENTITY, &why),
    };
    let name = crate::value::escape(&profile.name);
    let limits = format!(
        "It stops after {} s or {} lines.",
        crate::live::monitor::MAX_TIME.as_secs(),
        crate::format::grouped(crate::live::monitor::MAX_LINES as u64)
    );
    let why = "MONITOR shows every command the server runs, other users' values included, \
               and slows the server.";
    let typed = monitor_asks_name(&profile);
    let question = if typed {
        Question::typed(
            format!("{why} {limits} Type {name} to start it on {name}."),
            profile.name.clone(),
        )
    } else {
        Question::ask(format!("{why} {limits} Start it on {name}?"))
    };
    app.ask(
        Action::Monitor {
            conn,
            expect: typed.then(|| profile.name.clone()),
        },
        question,
    )
}

/// Why a typed answer did not start Monitor.
pub const NOT_STARTED: &str = "MONITOR not started: the name typed did not match.";

/// The answer to Monitor's question: starts it (202), when the name typed matches on a
/// `prod` profile and the connection is the one asked about.
pub async fn monitor_answered(
    app: &App,
    conn: ConnId,
    expect: Option<String>,
    typed: Option<String>,
) -> Response {
    if let Some(want) = expect
        && typed.as_deref() != Some(want.as_str())
    {
        return error(StatusCode::UNPROCESSABLE_ENTITY, NOT_STARTED);
    }
    match app.hub.monitor(conn) {
        Ok(run) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({
                "run": run,
                "max_lines": crate::live::monitor::MAX_LINES,
                "max_secs": crate::live::monitor::MAX_TIME.as_secs(),
            })),
        )
            .into_response(),
        Err(why) => error(StatusCode::UNPROCESSABLE_ENTITY, &why),
    }
}

/// `DELETE /api/server/monitor`: stops it.
pub async fn monitor_stop(State(app): State<App>) -> Response {
    ok(serde_json::json!({ "stopped": app.hub.stop_monitor() }))
}
