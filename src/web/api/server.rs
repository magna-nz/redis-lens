//! 3 Server: Clients (grouped as the TUI groups them), Slow log (arguments masked as
//! the TUI masks them), and Monitor (time-boxed as in the TUI; its lines come as
//! `monitor` events). Each reads the picked node of a cluster.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::{App, SHAPE, error, live_error, off, ok};
use crate::conn::{PERM_CLIENT_LIST, PERM_SLOWLOG};
use crate::live::server::{Reply, Request};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest};
use crate::model::PermStatus;
use crate::tui::app::ConnId;
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
        Ok(LiveReply::Server(Reply::Clients(list))) => ok(dto::Clients::of(Some(node), &list)),
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
            app.hub.with_mut(|c| {
                if c.id != conn {
                    return;
                }
                c.slow_specs.extend(r.learned.clone());
                if matches!(r.config, Some(Err(_))) {
                    c.slow_config = false;
                }
                if r.keys_off.is_some() {
                    c.slow_keys = false;
                }
            });
            let (settings, settings_off) = match &r.config {
                Some(Ok(s)) => (
                    Some(serde_json::json!({
                        "threshold_micros": s.threshold_micros,
                        "max_len": s.max_len,
                    })),
                    None,
                ),
                Some(Err(e)) => (None, Some(e.to_string())),
                None => (None, None),
            };
            ok(serde_json::json!({
                "node": node,
                "entries": r.entries.iter().map(dto::Slow::of).collect::<Vec<_>>(),
                "settings": settings,
                "settings_off": settings_off,
                "keys_off": r.keys_off.as_ref().map(ToString::to_string),
            }))
        }
        Ok(_) => error(StatusCode::BAD_GATEWAY, SHAPE),
        Err(e) => live_error(&e),
    }
}

/// `POST /api/server/monitor`: starts Monitor on the picked node. It stops by itself
/// after `live::monitor::MAX_TIME` or `MAX_LINES` lines, as in the TUI.
pub async fn monitor_start(State(app): State<App>) -> Response {
    match app.hub.monitor() {
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
