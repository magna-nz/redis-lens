//! Snapshots: save the current analysis (`POST /api/snapshots`), list the saved ones
//! (`GET /api/snapshots`) and compare two (`GET /api/snapshots/compare`), as `s` in the
//! TUI and `scan --save` save them: the same directory (`snapshot::default_dir`), the
//! same writer ([`crate::snapshot::save`]) and so the same file format, which holds no
//! credential (a report has none: rule 1).
//!
//! Snapshots are named by their file name (`20261115T091500Z.json`, UTC), checked
//! against the timestamp form before any file is opened, inside the session's profile
//! directory only: a name never leads anywhere else. File system errors are never
//! passed on (they name paths): the reply says what could not be done.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{App, error, ok};
use crate::model::ScanReport;
use crate::snapshot;

pub const NOTHING_TO_SAVE: &str = "Nothing to save yet: analyze the keys first.";
pub const RUNNING: &str = "An analysis is running: save it when it finishes.";
pub const PARTIAL: &str = "These are partial results: save them when the analysis finishes.";
pub const STOPPED: &str = "This analysis was stopped early, so its results are partial and are not saved. Analyze again to save one.";
pub const SAVE_FAILED: &str =
    "The snapshot could not be saved: the snapshot directory could not be written.";
pub const LIST_FAILED: &str =
    "The snapshots could not be listed: the snapshot directory could not be read.";
pub const BAD_NAME: &str =
    "a snapshot is named by its file name, such as 20261115T091500Z.json, or current";
pub const NOT_FOUND: &str = "There is no such snapshot of this connection.";
pub const NOT_READ: &str = "That snapshot could not be read: it is not a valid snapshot.";
pub const SAME: &str = "Pick two different snapshots to compare.";

/// The report `POST /api/snapshots` would save, or why there is none, as the TUI's `s`
/// decides: only a finished report, never partial results, never one stopped early,
/// and not the previous run's while an analysis runs.
fn to_save(c: &crate::web::session::Current) -> Result<ScanReport, &'static str> {
    let s = &c.session;
    if c.analysis_running() && s.partial.is_none() && s.report.is_some() {
        return Err(RUNNING);
    }
    if crate::tui::app::views::overview::stopped_early(s) {
        return Err(STOPPED);
    }
    match &s.report {
        Some(r) if !c.analysis_running() => Ok(r.clone()),
        Some(_) => Err(RUNNING),
        None if s.partial.is_some() => Err(PARTIAL),
        None => Err(NOTHING_TO_SAVE),
    }
}

/// `POST /api/snapshots`: saves the current analysis (201 with its name).
pub async fn save(State(app): State<App>) -> Response {
    let Some(report) = app.hub.with(to_save) else {
        return error(StatusCode::CONFLICT, crate::web::session::NOT_CONNECTED);
    };
    let report = match report {
        Ok(r) => r,
        Err(why) => return error(StatusCode::CONFLICT, why),
    };
    let dir = match app.hub.snapshot_dir() {
        Ok(d) => d,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, SAVE_FAILED),
    };
    let (taken, sampled) = (report.taken_at_unix, report.sampled_keys);
    let saved = tokio::task::spawn_blocking(move || snapshot::save(&dir, &report)).await;
    match saved {
        Ok(Ok(path)) => {
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            app.hub.with_mut(|c| {
                c.session.last_snapshot = Some(crate::tui::app::views::overview::LastSnapshot {
                    taken_at_unix: taken,
                    sampled_keys: sampled,
                });
            });
            app.hub.emit_overview_if_changed();
            (
                StatusCode::CREATED,
                axum::Json(serde_json::json!({
                    "file": file,
                    "taken_at_unix": taken,
                    "taken_at_utc": snapshot::format_utc(taken),
                    "sampled_keys": sampled,
                    "text": format!("Saved snapshot {file}"),
                })),
            )
                .into_response()
        }
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, SAVE_FAILED),
    }
}

/// `GET /api/snapshots`: the saved snapshots of the session's connection, newest first,
/// and whether the current analysis can be saved.
pub async fn list(State(app): State<App>) -> Response {
    let Some((name, save_off, current)) = app.hub.with(|c| {
        (
            c.report_name(),
            to_save(c).err(),
            c.session.report.as_ref().map(|r| r.taken_at_unix),
        )
    }) else {
        return error(StatusCode::CONFLICT, crate::web::session::NOT_CONNECTED);
    };
    let Ok(dir) = app.hub.snapshot_dir() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, LIST_FAILED);
    };
    let n = name.clone();
    let listed = tokio::task::spawn_blocking(move || snapshot::list(&dir, &n)).await;
    let Ok(Ok(infos)) = listed else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, LIST_FAILED);
    };
    let rows: Vec<serde_json::Value> = infos
        .iter()
        .map(|i| {
            serde_json::json!({
                "file": format!("{}.json", snapshot::timestamp_name(i.taken_at_unix)),
                "taken_at_unix": i.taken_at_unix,
                "taken_at_utc": snapshot::format_utc(i.taken_at_unix),
                "current": current == Some(i.taken_at_unix),
            })
        })
        .collect();
    ok(serde_json::json!({
        "profile": crate::value::escape(&name),
        "snapshots": rows,
        "can_save": save_off.is_none(),
        "save_off": save_off,
        "saved": current.is_some_and(|t| infos.iter().any(|i| i.taken_at_unix == t)),
    }))
}

#[derive(Debug, Deserialize)]
pub struct CompareQuery {
    a: String,
    b: String,
}

/// A snapshot name's timestamp, or `None` for `current`; refuses anything else.
pub fn parse_name(name: &str) -> Result<Option<i64>, &'static str> {
    if name == "current" {
        return Ok(None);
    }
    let stem = name.strip_suffix(".json").ok_or(BAD_NAME)?;
    let t = snapshot::parse_timestamp(stem).ok_or(BAD_NAME)?;
    // Only the form `timestamp_name` writes: one name per second.
    (snapshot::timestamp_name(t) == stem)
        .then_some(Some(t))
        .ok_or(BAD_NAME)
}

/// `GET /api/snapshots/compare?a=<file|current>&b=<file|current>`: the Changes of the
/// newer of the two against the older, as the TUI's Changes panel shows them.
pub async fn compare(State(app): State<App>, Query(q): Query<CompareQuery>) -> Response {
    let (Ok(a), Ok(b)) = (parse_name(&q.a), parse_name(&q.b)) else {
        return error(StatusCode::BAD_REQUEST, BAD_NAME);
    };
    if a == b {
        return error(StatusCode::UNPROCESSABLE_ENTITY, SAME);
    }
    let Some((name, current)) = app.hub.with(|c| {
        (
            c.report_name(),
            c.session.report.clone().filter(|_| !c.analysis_running()),
        )
    }) else {
        return error(StatusCode::CONFLICT, crate::web::session::NOT_CONNECTED);
    };
    if (a.is_none() || b.is_none()) && current.is_none() {
        return error(StatusCode::CONFLICT, super::analyze::NO_REPORT);
    }
    let Ok(dir) = app.hub.snapshot_dir() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, LIST_FAILED);
    };
    let loaded = tokio::task::spawn_blocking(move || {
        let infos = snapshot::list(&dir, &name)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, LIST_FAILED))?;
        let load = |t: Option<i64>| -> Result<ScanReport, (StatusCode, &'static str)> {
            match t {
                None => current
                    .clone()
                    .ok_or((StatusCode::CONFLICT, super::analyze::NO_REPORT)),
                Some(t) => {
                    let info = infos
                        .iter()
                        .find(|i| i.taken_at_unix == t)
                        .ok_or((StatusCode::NOT_FOUND, NOT_FOUND))?;
                    let r = snapshot::load(&info.path)
                        .map_err(|_| (StatusCode::UNPROCESSABLE_ENTITY, NOT_READ))?;
                    if r.profile != name {
                        return Err((StatusCode::NOT_FOUND, NOT_FOUND));
                    }
                    Ok(r)
                }
            }
        };
        Ok::<_, (StatusCode, &'static str)>((load(a)?, load(b)?))
    })
    .await;
    let (ra, rb) = match loaded {
        Ok(Ok(p)) => p,
        Ok(Err((status, why))) => return error(status, why),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, LIST_FAILED),
    };
    let (before, after) = if ra.taken_at_unix <= rb.taken_at_unix {
        (ra, rb)
    } else {
        (rb, ra)
    };
    let d = crate::diff::diff(&before, &after);
    let note = crate::diff::node_set_note(&before, &after);
    let mut body = super::analyze::diff_body(&d, None, note);
    if let Some(o) = body.as_object_mut() {
        o.insert("before_sampled_keys".into(), before.sampled_keys.into());
        o.insert("after_sampled_keys".into(), after.sampled_keys.into());
    }
    ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_timestamp_names_and_current_are_taken() {
        assert_eq!(parse_name("current"), Ok(None));
        let t = parse_name("20261115T091500Z.json").unwrap().unwrap();
        assert_eq!(snapshot::timestamp_name(t), "20261115T091500Z");
        for bad in [
            "../config.toml",
            "20261115T091500Z",
            "20261115T091500Z.json/../../x",
            "/etc/passwd",
            "x.json",
            "",
        ] {
            assert!(parse_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_wording_has_no_tui_keys() {
        for t in [
            NOTHING_TO_SAVE,
            RUNNING,
            PARTIAL,
            STOPPED,
            SAVE_FAILED,
            LIST_FAILED,
            BAD_NAME,
            NOT_FOUND,
            NOT_READ,
            SAME,
        ] {
            assert_eq!(crate::web::wording::banned(t), None, "{t}");
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
    }
}
