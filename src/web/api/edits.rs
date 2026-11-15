//! The key edits of Browse (spec "Edits on a key's value (P3)" and `N` on the key list),
//! each through the one write gate: `POST /api/value/edit`, `/api/value/add`,
//! `/api/value/ttl`, `/api/value/rename`, `/api/value/remove`, `/api/value/delete` and
//! `POST /api/keys/add`.
//!
//! Each builds the TUI's own [`WriteOp`] from what the open value holds, as the TUI's key
//! detail does ([`ValueState::edit_target`], [`ValueState::ttl_field`],
//! [`value_view::Target::op`], [`value_view::parse_ttl`], [`value_view::parse_score`]).
//! Then [`super::writes::write_flow_then`]: the gate decides (refused, sent, or asked
//! about with a 409), [`super::writes::grant`] gives the permit, `Hub::write` sends it
//! through the live layer. No command is built here.
//!
//! After the write, what changed is read again as the TUI does it: the value from its
//! first page (its `rev` goes up, the selection stays), and the key's row (TTL, size,
//! length) for the key lists. A delete closes the value and drops the key from the key
//! lists; a rename renames it there; a TTL reads the row only. One write at a time per
//! value. Values are never logged; [`Bytes`] and the bodies print no value in `Debug`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::keys::KeyDto;
use super::values::NO_VIEW;
use super::{App, Body, Refused, SHAPE, error, error_with, ok};
use crate::live::keys::{KeyRow, Reply as KeysReply};
use crate::live::values::{Items, Kind};
use crate::live::write::Reply as WriteReply;
use crate::live::{CANCELLED, Dest, LiveError, LiveReply};
use crate::model::{KeyType, Ttl};
use crate::tui::app::ConnId;
use crate::tui::app::views::value::{self as value_view, ValueState};
use crate::value::{self, Raw};
use crate::web::keyname::Name;
use crate::web::session::Current;
use crate::write::{NewKey, Permit, Ttl as WriteTtl, WriteOp};

/// A value the page sends: text as typed (`"text"`), or bytes as hex
/// (`{"hex": "00ff"}`). `Debug` shows its length only.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Bytes {
    Text(String),
    Hex { hex: String },
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = match self {
            Bytes::Text(t) => t.len(),
            Bytes::Hex { hex } => hex.len() / 2,
        };
        write!(f, "Bytes(<{n} bytes>)")
    }
}

/// Why a value given as hex was refused (400).
pub const BAD_HEX: &str = "a value given as hex must be an even number of hex digits";

impl Bytes {
    /// The bytes, or 400 [`BAD_HEX`].
    pub fn raw(&self) -> Result<Raw, Refused> {
        match self {
            Bytes::Text(t) => Ok(Raw::from(t.as_str())),
            Bytes::Hex { hex } => hex_bytes(hex)
                .map(Raw::from)
                .ok_or_else(|| Refused::new(StatusCode::BAD_REQUEST, BAD_HEX)),
        }
    }
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).and_then(|d| u8::try_from(d).ok());
    s.as_bytes()
        .chunks(2)
        .map(|p| Some((digit(p[0])? << 4) | digit(p[1])?))
        .collect()
}

/// What to read again once a key edit's write answered.
pub enum After {
    /// The value open as `view`, and the key's row.
    Value { view: u64 },
    /// `N`: the key added, listed in key list `list` and opened.
    NewKey { list: Option<u64>, row: KeyRow },
}

/// Names only: never the key or a value.
impl std::fmt::Debug for After {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            After::Value { view } => write!(f, "Value({view})"),
            After::NewKey { list, .. } => write!(f, "NewKey({list:?})"),
        }
    }
}

/// The TUI's words, and the dashboard's where the TUI names a key.
pub const WRITING: &str = "The last write has not answered yet.";
pub const STALE: &str =
    "The value was read again since the page showed it: look at it again, then try once more.";
pub const GONE: &str = "The key no longer exists.";
pub const UNCHANGED: &str = "Not changed, so nothing was sent.";
pub const NO_NAME: &str = "Type the new name.";
pub const NO_KEY_NAME: &str = "Type the key's name.";
pub const NO_FIELD_NAME: &str = "Type the field's name.";
pub const ADD_OFF: &str =
    "Adding works on a hash, list, set, sorted set or stream: edit this value instead.";
pub const ADD_UNKNOWN: &str = "redis-lens does not add to this type.";
pub const REMOVE_OFF: &str =
    "Removing works on a field, element, member or entry: delete the key instead.";
pub const REMOVE_UNKNOWN: &str =
    "redis-lens does not delete items of this type: delete the key instead.";
pub const NOTHING_SELECTED: &str = "Nothing is selected to delete.";
pub const FIELD_TTL_HASH: &str = "Only a hash field has a TTL of its own.";
pub const BAD_TYPE: &str = "the type must be one of string, hash, list, set, zset or stream";
pub const CHANGED_SINCE: &str =
    "Not written: the value changed since it was loaded. It is read again now.";

fn no(text: impl Into<String>) -> Refused {
    Refused::new(StatusCode::UNPROCESSABLE_ENTITY, text)
}

/// A TUI sentence in the dashboard's terms.
fn words(text: &str) -> String {
    crate::web::wording::web(text)
}

/// The gate's own refusal for a read-only profile, before anything about the value is
/// looked at (the TUI's `may_write` asks the gate first): a read-only profile is refused
/// every edit with the same words, whatever the edit.
fn writable(c: &Current) -> Result<(), Refused> {
    let p = c.profile();
    if p.is_read_only() {
        return Err(no(crate::write::read_only_reason(p)).holding(&super::writes::profile_data(p)));
    }
    if !c.live_ready() {
        return Err(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE));
    }
    Ok(())
}

/// View `id` of the current connection, ready for an edit: no write of it on its way,
/// no page loading, read at `rev` (when the edit names an item), the key still there.
fn view_for_write(
    c: &mut Current,
    id: u64,
    rev: Option<u64>,
) -> Result<&mut super::values::View, Refused> {
    writable(c)?;
    let conn = c.id;
    let v = c
        .browse
        .values
        .get_mut(id)
        .filter(|v| v.conn() == conn)
        .ok_or_else(|| Refused::new(StatusCode::NOT_FOUND, NO_VIEW))?;
    if v.writing {
        return Err(Refused::new(StatusCode::CONFLICT, WRITING));
    }
    if v.loading() {
        return Err(Refused::new(
            StatusCode::CONFLICT,
            super::values::STILL_LOADING,
        ));
    }
    if rev.is_some_and(|r| r != v.rev) {
        return Err(Refused::new(StatusCode::CONFLICT, STALE));
    }
    if v.state.gone {
        return Err(no(GONE));
    }
    Ok(v)
}

fn key_of(s: &ValueState) -> Result<String, Refused> {
    s.row
        .as_ref()
        .map(|r| r.key.clone())
        .ok_or_else(|| Refused::new(StatusCode::NOT_FOUND, NO_VIEW))
}

/// Selects item `index` of what is loaded, or refuses an index past it (`selected`
/// clamps, so an index past the end would otherwise name the last item).
fn select(s: &mut ValueState, index: usize) -> Result<(), Refused> {
    if index >= s.loaded().max(1) {
        return Err(no(super::values::NO_ITEM));
    }
    s.sel = index;
    Ok(())
}

enum Prepared {
    Op(WriteOp),
    Unchanged,
}

/// An edit of view `view`: the saved profile read again (a profile made read-only
/// elsewhere refuses at once), the op made by `prepare` from what the view holds, then
/// the one write flow.
async fn flow(
    app: &App,
    view: u64,
    prepare: impl FnOnce(&mut Current) -> Result<Prepared, Refused>,
) -> Response {
    app.hub.follow_saved_profile().await;
    match app.hub.with_mut(prepare) {
        None => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => r.into_response(),
        Some(Ok(Prepared::Unchanged)) => ok(serde_json::json!({
            "sent": false,
            "done": false,
            "message": UNCHANGED,
            "reply": null,
            "key": key_name(app, view),
            "closed": false,
            "value": super::values::body_of(app, view),
            "row": row_of(app, view),
        })),
        Some(Ok(Prepared::Op(op))) => {
            super::writes::write_flow_then(app, op, Some(After::Value { view })).await
        }
    }
}

/// `POST /api/value/edit` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub view: u64,
    /// The `rev` of the value the page showed.
    pub rev: u64,
    /// The item: a hash field, list element, set or sorted set member; 0 for a string
    /// or a document.
    #[serde(default)]
    pub index: usize,
    /// The new value; for a sorted set member its new score, as text.
    pub value: Bytes,
}

impl std::fmt::Debug for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Edit")
            .field("view", &self.view)
            .field("rev", &self.rev)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

/// `POST /api/value/edit`: the TUI's `e`. `SET … KEEPTTL` (only while the string is
/// the one loaded), `HSET`, `LSET` (only while the element is the one loaded), a set
/// member replaced, a sorted set member's score, or `JSON.SET`.
pub async fn edit(State(app): State<App>, Body(b): Body<Edit>) -> Response {
    let value = match b.value.raw() {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    flow(&app, b.view, |c| {
        let v = view_for_write(c, b.view, Some(b.rev))?;
        let s = &mut v.state;
        let key = key_of(s)?;
        if s.items.is_some() {
            select(s, b.index)?;
        }
        let (target, now, _) = s.edit_target().map_err(|why| no(words(&why)))?;
        if now.as_bytes() == value.as_bytes() {
            return Ok(Prepared::Unchanged);
        }
        target
            .op(&key, value)
            .map(Prepared::Op)
            .map_err(|why| no(words(&why)))
    })
    .await
}

/// `POST /api/value/add` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Add {
    pub view: u64,
    /// A hash field's name, or a stream entry's field.
    #[serde(default)]
    pub field: Option<Bytes>,
    /// The value, element or member.
    pub value: Bytes,
    /// A sorted set member's score, as text (`1.5`, `inf`, `-inf`).
    #[serde(default)]
    pub score: Option<String>,
}

impl std::fmt::Debug for Add {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Add")
            .field("view", &self.view)
            .finish_non_exhaustive()
    }
}

/// `POST /api/value/add`: the TUI's `+`. `HSETNX` (never over a field), `RPUSH`,
/// `SADD`, `ZADD NX` with the score, `XADD` with one field and value.
pub async fn add(State(app): State<App>, Body(b): Body<Add>) -> Response {
    let value = match b.value.raw() {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    let field = match b.field.as_ref().map(Bytes::raw).transpose() {
        Ok(f) => f.unwrap_or_else(|| Raw::from("")),
        Err(r) => return r.into_response(),
    };
    flow(&app, b.view, |c| {
        let v = view_for_write(c, b.view, None)?;
        let key = key_of(&v.state)?;
        let op = match v.state.kind() {
            Some(Kind::Hash) => WriteOp::HSetNx { key, field, value },
            Some(Kind::List) => WriteOp::RPush { key, value },
            Some(Kind::Set) => WriteOp::SAdd { key, member: value },
            Some(Kind::ZSet) => WriteOp::ZAdd {
                key,
                member: value,
                score: value_view::parse_score(b.score.as_deref().unwrap_or(""))
                    .map_err(|why| no(words(&why)))?,
            },
            Some(Kind::Stream) => {
                if field.is_empty() {
                    return Err(no(NO_FIELD_NAME));
                }
                WriteOp::XAdd {
                    key,
                    fields: vec![(field, value)],
                }
            }
            Some(Kind::String | Kind::Json) => return Err(no(ADD_OFF)),
            _ => return Err(no(ADD_UNKNOWN)),
        };
        Ok(Prepared::Op(op))
    })
    .await
}

/// `POST /api/value/ttl` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTtl {
    pub view: u64,
    /// `90`, `90s`, `10m`, `24h`, `7d`, `1w`, `1h30m`; `-`, `persist` or `none` removes it.
    pub ttl: String,
    /// A hash field's index, for its own TTL; the key's otherwise.
    #[serde(default)]
    pub field: Option<usize>,
    /// The `rev` the field's index is of: needed with `field`.
    #[serde(default)]
    pub rev: Option<u64>,
}

/// `POST /api/value/ttl`: the TUI's `t`. `EXPIRE` or `PERSIST` for the key, `HEXPIRE`
/// or `HPERSIST` for a hash field where field TTLs are read.
pub async fn ttl(State(app): State<App>, Body(b): Body<SetTtl>) -> Response {
    if b.field.is_some() && b.rev.is_none() {
        return error(StatusCode::BAD_REQUEST, super::BAD_BODY);
    }
    flow(&app, b.view, |c| {
        let rev = b.field.and(b.rev);
        let v = view_for_write(c, b.view, rev)?;
        let ttl = value_view::parse_ttl(&b.ttl).map_err(|why| no(words(&why)))?;
        let key = key_of(&v.state)?;
        let Some(i) = b.field else {
            return Ok(Prepared::Op(WriteOp::KeyTtl { key, ttl }));
        };
        if !matches!(v.state.items, Some(Items::Hash(_))) {
            return Err(no(FIELD_TTL_HASH));
        }
        select(&mut v.state, i)?;
        match v.state.ttl_field() {
            (Some(field), _) => Ok(Prepared::Op(WriteOp::FieldTtl { key, field, ttl })),
            (None, Some(why)) => Err(no(words(&why))),
            (None, None) => Err(no(super::values::NO_ITEM)),
        }
    })
    .await
}

/// `POST /api/value/rename` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rename {
    pub view: u64,
    /// The new name, as text.
    pub name: String,
}

/// `POST /api/value/rename`: the TUI's `R`, `RENAMENX`: it never overwrites a key.
pub async fn rename(State(app): State<App>, Body(b): Body<Rename>) -> Response {
    flow(&app, b.view, |c| {
        let v = view_for_write(c, b.view, None)?;
        let key = key_of(&v.state)?;
        if b.name.is_empty() {
            return Err(no(NO_NAME));
        }
        Ok(Prepared::Op(WriteOp::Rename { key, new: b.name }))
    })
    .await
}

/// `POST /api/value/remove` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remove {
    pub view: u64,
    pub rev: u64,
    pub index: usize,
}

/// `POST /api/value/remove`: the TUI's `d`. `HDEL`, `SREM`, `ZREM`, `XDEL`, or for a
/// list element `LSET` to a marker then `LREM` (only while the element is the one
/// loaded).
pub async fn remove(State(app): State<App>, Body(b): Body<Remove>) -> Response {
    flow(&app, b.view, |c| {
        let v = view_for_write(c, b.view, Some(b.rev))?;
        let key = key_of(&v.state)?;
        let i = b.index;
        let missing = || no(super::values::NO_ITEM);
        let op = match &v.state.items {
            Some(Items::Hash(f)) => WriteOp::HDel {
                key,
                field: f.get(i).ok_or_else(missing)?.name.clone(),
            },
            Some(Items::Set(m)) => WriteOp::SRem {
                key,
                member: m.get(i).ok_or_else(missing)?.clone(),
            },
            Some(Items::ZSet(m)) => WriteOp::ZRem {
                key,
                member: m.get(i).ok_or_else(missing)?.member.clone(),
            },
            Some(Items::Stream(e)) => WriteOp::XDel {
                key,
                id: e.get(i).ok_or_else(missing)?.id.clone(),
            },
            Some(Items::List(e)) => WriteOp::LDel {
                key,
                index: i as u64,
                old: e.get(i).ok_or_else(missing)?.clone(),
                marker: value_view::next_list_marker(),
            },
            Some(Items::String { .. } | Items::Json { .. } | Items::JsonNotRead { .. }) => {
                return Err(no(REMOVE_OFF));
            }
            Some(Items::Vector(_) | Items::Array(_) | Items::Module(_) | Items::NotRead) => {
                return Err(no(REMOVE_UNKNOWN));
            }
            None => return Err(no(NOTHING_SELECTED)),
        };
        Ok(Prepared::Op(op))
    })
    .await
}

/// `POST /api/value/delete` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    pub view: u64,
}

/// `POST /api/value/delete`: the TUI's `D`, `UNLINK`.
pub async fn delete(State(app): State<App>, Body(b): Body<Delete>) -> Response {
    flow(&app, b.view, |c| {
        let v = view_for_write(c, b.view, None)?;
        let key = key_of(&v.state)?;
        Ok(Prepared::Op(WriteOp::Unlink { key }))
    })
    .await
}

/// `POST /api/keys/add` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddKey {
    /// The key list it is added from: the new key is listed there.
    #[serde(default)]
    pub list: Option<u64>,
    pub name: String,
    /// `string`, `hash`, `list`, `set`, `zset` or `stream`.
    #[serde(rename = "type")]
    pub key_type: String,
    /// A hash's first field, or a stream entry's field.
    #[serde(default)]
    pub field: Option<Bytes>,
    pub value: Bytes,
    /// A sorted set's first member's score.
    #[serde(default)]
    pub score: Option<String>,
}

impl std::fmt::Debug for AddKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddKey")
            .field("list", &self.list)
            .field("type", &self.key_type)
            .finish_non_exhaustive()
    }
}

/// `POST /api/keys/add`: the TUI's `N`. The key with its first item, only when it does
/// not exist (`SET … NX`; for the other types after `WATCH` and `EXISTS`). Once added
/// it is listed in `list` and its value opens.
pub async fn add_key(State(app): State<App>, Body(b): Body<AddKey>) -> Response {
    let value = match b.value.raw() {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    let field = match b.field.as_ref().map(Bytes::raw).transpose() {
        Ok(f) => f.unwrap_or_else(|| Raw::from("")),
        Err(r) => return r.into_response(),
    };
    app.hub.follow_saved_profile().await;
    let checked = app.hub.with_mut(|c| {
        writable(c)?;
        let conn = c.id;
        if let Some(l) = b.list
            && c.browse.lists.of(conn, l).is_none()
        {
            return Err(Refused::new(StatusCode::NOT_FOUND, super::keys::NO_LIST));
        }
        Ok(())
    });
    match checked {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => return r.into_response(),
        Some(Ok(())) => {}
    }
    if b.name.is_empty() {
        return no(NO_KEY_NAME).into_response();
    }
    let (new, key_type) = match b.key_type.as_str() {
        "string" => (NewKey::String(value), KeyType::String),
        "hash" => (NewKey::Hash { field, value }, KeyType::Hash),
        "list" => (NewKey::List(value), KeyType::List),
        "set" => (NewKey::Set(value), KeyType::Set),
        "zset" => {
            let score = match value_view::parse_score(b.score.as_deref().unwrap_or("")) {
                Ok(s) => s,
                Err(why) => return no(words(&why)).into_response(),
            };
            (
                NewKey::ZSet {
                    member: value,
                    score,
                },
                KeyType::ZSet,
            )
        }
        "stream" => {
            if field.is_empty() {
                return no(NO_FIELD_NAME).into_response();
            }
            (NewKey::Stream { field, value }, KeyType::Stream)
        }
        _ => return no(BAD_TYPE).into_response(),
    };
    let row = KeyRow {
        key: b.name.clone(),
        key_type,
        ttl: Ttl::Persistent,
        bytes: None,
        encoding: None,
        length: Some(1),
        freq: None,
    };
    let op = WriteOp::NewKey {
        key: b.name,
        value: new,
    };
    super::writes::write_flow_then(&app, op, Some(After::NewKey { list: b.list, row })).await
}

/// Sends a key edit the gate let through, then reads again what it changed, in a task
/// of its own: a request the browser drops still lands, and never leaves the value
/// marked as writing.
pub async fn send(app: &App, conn: ConnId, dest: Dest, permit: Permit, after: After) -> Response {
    let task = app.clone();
    match tokio::spawn(async move { send_now(&task, conn, dest, permit, after).await }).await {
        Ok(r) => r,
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, SHAPE),
    }
}

/// Marks view `id` as writing; false when a write of it is on its way already. A view
/// no longer open writes anyway: the write was asked for and confirmed.
fn start_writing(app: &App, conn: ConnId, id: u64) -> bool {
    app.hub
        .with_mut(|c| match c.browse.values.get_mut(id) {
            Some(v) if c.id == conn && v.conn() == conn => {
                if v.writing {
                    return false;
                }
                v.writing = true;
                true
            }
            _ => true,
        })
        .unwrap_or(true)
}

fn stop_writing(app: &App, conn: ConnId, id: u64) {
    app.hub.with_mut(|c| {
        if c.id == conn
            && let Some(v) = c.browse.values.get_mut(id)
        {
            v.writing = false;
        }
    });
}

async fn send_now(app: &App, conn: ConnId, dest: Dest, permit: Permit, after: After) -> Response {
    let op = permit.op().clone();
    let view = match &after {
        After::Value { view } => Some(*view),
        After::NewKey { .. } => None,
    };
    if let Some(v) = view
        && !start_writing(app, conn, v)
    {
        return error(StatusCode::CONFLICT, WRITING);
    }
    let result = app.hub.write(conn, dest, permit).await;
    if let Some(v) = view {
        stop_writing(app, conn, v);
    }
    answered(app, conn, &op, after, result).await
}

/// What the TUI's `write_done` does after the write `op` answered `reply`: whether it
/// did what was asked, what to say (the dashboard's words), and whether the value
/// (`reload`) and the key's row (`meta`) are read again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub done: bool,
    pub message: String,
    pub reload: bool,
    pub meta: bool,
}

/// The TUI's `write_done`, in the dashboard's words.
pub fn outcome(op: &WriteOp, reply: &WriteReply) -> Outcome {
    let n = match reply {
        WriteReply::Int(n) => Some(*n),
        _ => None,
    };
    let o = |done: bool, message: String, reload: bool, meta: bool| Outcome {
        done,
        message,
        reload,
        meta,
    };
    if *reply == WriteReply::Changed {
        return o(false, CHANGED_SINCE.into(), true, true);
    }
    match op {
        WriteOp::Unlink { key } => match n {
            Some(0) => o(false, GONE.into(), false, false),
            _ => o(
                true,
                format!("Deleted {}.", value::escape(key)),
                false,
                false,
            ),
        },
        WriteOp::Rename { new, .. } => match n {
            Some(0) => o(
                false,
                format!(
                    "Not renamed: {} exists already, and a rename never overwrites a key.",
                    value::escape(new)
                ),
                false,
                false,
            ),
            _ => o(
                true,
                format!("Renamed to {}.", value::escape(new)),
                false,
                false,
            ),
        },
        WriteOp::KeyTtl { ttl, .. } => match (ttl, n) {
            (WriteTtl::Persist, Some(0)) => o(false, "The key had no TTL.".into(), false, true),
            (WriteTtl::Seconds(_), Some(0)) => o(
                false,
                "Not set: the key no longer exists.".into(),
                false,
                true,
            ),
            (WriteTtl::Seconds(s), _) => o(
                true,
                format!(
                    "TTL set to {}.",
                    crate::format::duration(std::time::Duration::from_secs(*s))
                ),
                false,
                true,
            ),
            (WriteTtl::Persist, _) => o(
                true,
                "TTL removed: the key no longer expires.".into(),
                false,
                true,
            ),
        },
        WriteOp::FieldTtl { ttl, .. } => match (ttl, n) {
            (_, Some(-2)) => o(
                false,
                "Not set: the field no longer exists.".into(),
                true,
                true,
            ),
            (WriteTtl::Persist, Some(-1)) => o(false, "The field had no TTL.".into(), true, true),
            (WriteTtl::Persist, _) => o(true, "Field TTL removed.".into(), true, true),
            (WriteTtl::Seconds(_), _) => o(true, "Field TTL set.".into(), true, true),
        },
        WriteOp::HSetNx { .. } if n == Some(0) => o(
            false,
            "Not added: the field exists already. Edit it instead.".into(),
            false,
            false,
        ),
        WriteOp::SAdd { .. } | WriteOp::ZAdd { .. } if n == Some(0) => o(
            false,
            "Not added: the member exists already.".into(),
            false,
            false,
        ),
        WriteOp::HDel { .. }
        | WriteOp::SRem { .. }
        | WriteOp::ZRem { .. }
        | WriteOp::XDel { .. }
            if n == Some(0) =>
        {
            o(
                false,
                "Nothing deleted: it was gone already.".into(),
                true,
                true,
            )
        }
        WriteOp::HDel { .. }
        | WriteOp::SRem { .. }
        | WriteOp::ZRem { .. }
        | WriteOp::XDel { .. }
        | WriteOp::LDel { .. } => o(true, "Deleted.".into(), true, true),
        WriteOp::HSetNx { .. }
        | WriteOp::RPush { .. }
        | WriteOp::SAdd { .. }
        | WriteOp::ZAdd { .. }
        | WriteOp::XAdd { .. } => o(true, "Added.".into(), true, true),
        _ => o(true, "Saved.".into(), true, true),
    }
}

/// The reply to a key edit's write, once what it changed is read again.
async fn answered(
    app: &App,
    conn: ConnId,
    op: &WriteOp,
    after: After,
    result: Result<LiveReply, LiveError>,
) -> Response {
    let key = op.name_to_type();
    let shown = value::escape(&key);
    let reply = match result {
        Ok(LiveReply::Write(r)) => r,
        Ok(_) => return error(StatusCode::BAD_GATEWAY, SHAPE),
        Err(LiveError::Failed(s)) if s == CANCELLED => {
            return error(StatusCode::UNPROCESSABLE_ENTITY, CANCELLED);
        }
        Err(LiveError::Closed) => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        // A `NOPERM` turned the command off for the session already (`Hub::write`).
        Err(e) => {
            return error_with(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("Not written: {e}."),
                &[&shown],
            );
        }
    };
    let wrote = super::writes::WriteReply::of(&reply, false);
    let view = match after {
        After::NewKey { list, row } => {
            return new_key_done(app, conn, list, row, &reply, wrote).await;
        }
        After::Value { view } => view,
    };
    let o = outcome(op, &reply);
    let mut closed = false;
    let mut name = key.clone();
    match op {
        WriteOp::Unlink { key } => {
            // Every value of the key closes, and no key list shows it any more.
            app.hub.with_mut(|c| {
                if c.id == conn {
                    let ids: Vec<u64> = c.browse.values.of_key(key).map(|v| v.id).collect();
                    for id in ids {
                        c.browse.values.close(id);
                    }
                    c.browse.lists.drop_key(key);
                }
            });
            closed = true;
        }
        WriteOp::Rename { key, new } if o.done => {
            app.hub.with_mut(|c| {
                if c.id != conn {
                    return;
                }
                for v in c.browse.values.of_key(key) {
                    if let Some(r) = v.state.row.as_mut() {
                        r.key = new.clone();
                    }
                }
                c.browse.lists.rename_key(key, new);
            });
            name = new.clone();
        }
        _ => {}
    }
    if o.reload {
        reload(app, conn, view).await;
    }
    if o.meta {
        read_row(app, conn, view).await;
    }
    let body = if closed {
        None
    } else {
        super::values::body_of(app, view)
    };
    let message =
        crate::web::wording::server_free_with(&o.message, &[&shown, &value::escape(&name)]);
    ok(serde_json::json!({
        "sent": true,
        "done": o.done,
        "message": message,
        "reply": wrote,
        "key": Name::of(&name),
        "closed": closed,
        "value": body,
        "row": if closed { None } else { row_of(app, view) },
    }))
}

/// The key of view `id`, as a name.
fn key_name(app: &App, id: u64) -> Option<Name> {
    app.hub
        .with(|c| {
            let v = c.browse.values.get(id)?;
            v.state.row.as_ref().map(|r| Name::of(&r.key))
        })
        .flatten()
}

/// The key's row as the view holds it: what the key lists show of it now. A row made
/// from the key alone guessed its type: the first read's `TYPE` says it (as the value's
/// `meta` does).
fn row_of(app: &App, id: u64) -> Option<KeyDto> {
    app.hub
        .with(|c| {
            let v = c.browse.values.get(id).filter(|v| v.conn() == c.id)?;
            let mut r = v.state.row.clone()?;
            if let Some(t) = v.state.type_name.as_deref() {
                r.key_type = crate::model::KeyType::from_type_reply(t);
            }
            Some(KeyDto::of(&r, c.session.owner_of(r.key.as_bytes())))
        })
        .flatten()
}

/// The TUI's `reload`: the value from its first page, the selection kept.
async fn reload(app: &App, conn: ConnId, id: u64) {
    let restarted = app
        .hub
        .with_mut(|c| match c.browse.values.get_mut(id) {
            Some(v) if c.id == conn && v.conn() == conn && !v.loading() => {
                v.restart();
                true
            }
            _ => false,
        })
        .unwrap_or(false);
    if restarted {
        // Why a read failed is in the value's body.
        let _ = super::values::read_body(app, id).await;
    }
}

/// The TUI's `read_meta` and `meta_done`: the key list's own read of one key (its TTL,
/// size, encoding, length and LFU counter), into the view's row and every key list's.
async fn read_row(app: &App, conn: ConnId, id: u64) {
    let asked = app
        .hub
        .with(|c| {
            let v = c
                .browse
                .values
                .get(id)
                .filter(|v| c.id == conn && v.conn() == conn)?;
            let row = v.state.row.clone()?;
            Some((
                Dest::key(row.key.as_bytes().to_vec()),
                value_view::meta_request(&row, &v.state.pattern),
                row.key,
            ))
        })
        .flatten();
    let Some((dest, req, key)) = asked else {
        return;
    };
    let result = app.hub.request(Some(conn), dest, req).await;
    let Ok(LiveReply::Keys(KeysReply::Page(page))) = result else {
        return;
    };
    app.hub.with_mut(|c| {
        if c.id != conn {
            return;
        }
        let Some(v) = c.browse.values.get_mut(id) else {
            return;
        };
        let Some(r) = v.state.row.as_mut().filter(|r| r.key == key) else {
            return;
        };
        match page.rows.into_iter().find(|n| n.key == r.key) {
            Some(n) => {
                r.ttl = n.ttl;
                r.bytes = n.bytes.or(r.bytes);
                r.encoding = n.encoding.or(r.encoding.take());
                r.length = n.length.or(r.length);
                r.freq = n.freq.or(r.freq);
                let row = r.clone();
                c.browse.lists.update_row(&row);
            }
            // Not unreadable, just not there: `TYPE` said `none`.
            None if page.unreadable == 0 => {
                v.state.gone = true;
                v.state.items = None;
            }
            None => {}
        }
    });
}

/// `N`'s write answered: not added (the key exists, or was written meanwhile), or
/// listed in `list` and opened, its first page read.
async fn new_key_done(
    app: &App,
    conn: ConnId,
    list: Option<u64>,
    row: KeyRow,
    reply: &WriteReply,
    wrote: super::writes::WriteReply,
) -> Response {
    let name = value::escape(&row.key);
    let not_added = |text: String| {
        ok(serde_json::json!({
            "sent": true,
            "done": false,
            "message": crate::web::wording::server_free_with(&text, &[&name]),
            "reply": wrote.clone(),
            "key": Name::of(&row.key),
            "closed": false,
            "value": null,
            "row": null,
        }))
    };
    match reply {
        WriteReply::Exists => {
            return not_added(format!(
                "Not added: {name} exists already, and adding a key never overwrites one."
            ));
        }
        WriteReply::Changed => {
            return not_added(format!(
                "Not added: {name} was written by someone else meanwhile."
            ));
        }
        _ => {}
    }
    let opened = app.hub.with_mut(|c| {
        if c.id != conn {
            return None;
        }
        // On a cluster it went to its slot's owner.
        let node = c.session.owner_of(row.key.as_bytes());
        let mut pattern = String::new();
        if let Some(l) = list.and_then(|l| c.browse.lists.of(conn, l)) {
            l.add_row(row.clone(), node);
            pattern = l.source();
        }
        Some((c.browse.values.add(conn, row.clone(), pattern), node))
    });
    let (body, node) = match opened.flatten() {
        Some((view, node)) => (super::values::read_body(app, view).await.ok(), node),
        None => (None, None),
    };
    ok(serde_json::json!({
        "sent": true,
        "done": true,
        "message": crate::web::wording::server_free_with(&format!("Added {name}."), &[&name]),
        "reply": wrote,
        "key": Name::of(&row.key),
        "closed": false,
        "value": body,
        "row": KeyDto::of(&row, node),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(s: &str) -> Raw {
        Raw::from(s)
    }

    #[test]
    fn values_come_as_text_or_hex_and_debug_shows_only_their_length() {
        let t: Bytes = serde_json::from_str(r#""secret value""#).unwrap();
        assert_eq!(t.raw().unwrap().as_bytes(), b"secret value");
        assert_eq!(format!("{t:?}"), "Bytes(<12 bytes>)");
        let h: Bytes = serde_json::from_str(r#"{"hex":"00ff1B"}"#).unwrap();
        assert_eq!(h.raw().unwrap().as_bytes(), &[0x00, 0xff, 0x1b]);
        assert!(!format!("{h:?}").contains("00ff"));
        for bad in [r#"{"hex":"0"}"#, r#"{"hex":"zz"}"#, r#"{"hex":"é1"}"#] {
            let b: Bytes = serde_json::from_str(bad).unwrap();
            let e = b.raw().unwrap_err();
            assert_eq!(e.status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(e.text, BAD_HEX);
        }
        // The bodies' Debug: never the value, the field or the name typed.
        let e: Edit =
            serde_json::from_str(r#"{"view":1,"rev":0,"index":2,"value":"pw-secret"}"#).unwrap();
        assert!(!format!("{e:?}").contains("pw-secret"));
        let a: AddKey = serde_json::from_str(
            r#"{"name":"k-secret","type":"hash","field":"f-secret","value":"v-secret"}"#,
        )
        .unwrap();
        let d = format!("{a:?}");
        assert!(!d.contains("secret"), "{d}");
        let a: Add = serde_json::from_str(r#"{"view":1,"field":"f-sec","value":"v-sec"}"#).unwrap();
        assert!(!format!("{a:?}").contains("sec\""));
        assert!(serde_json::from_str::<Edit>(r#"{"view":1,"rev":0,"value":"v","x":1}"#).is_err());
        let after = After::NewKey {
            list: Some(3),
            row: KeyRow {
                key: "k-secret".into(),
                key_type: KeyType::String,
                ttl: Ttl::Persistent,
                bytes: None,
                encoding: None,
                length: Some(1),
                freq: None,
            },
        };
        assert_eq!(format!("{after:?}"), "NewKey(Some(3))");
    }

    #[test]
    fn outcomes_follow_the_tuis_write_done_in_the_dashboards_words() {
        let k = || "user:1".to_string();
        let o = outcome(&WriteOp::Unlink { key: k() }, &WriteReply::Int(1));
        assert_eq!((o.done, o.message.as_str()), (true, "Deleted user:1."));
        assert!(!o.reload && !o.meta);
        let o = outcome(&WriteOp::Unlink { key: k() }, &WriteReply::Int(0));
        assert_eq!((o.done, o.message.as_str()), (false, GONE));
        let r = WriteOp::Rename {
            key: k(),
            new: "user:2".into(),
        };
        let o = outcome(&r, &WriteReply::Int(0));
        assert!(!o.done && o.message.contains("never overwrites"), "{o:?}");
        assert!(!o.reload && !o.meta);
        assert!(outcome(&r, &WriteReply::Int(1)).done);
        let ttl = WriteOp::KeyTtl {
            key: k(),
            ttl: WriteTtl::Seconds(90),
        };
        let o = outcome(&ttl, &WriteReply::Int(1));
        assert_eq!(o.message, "TTL set to 1m 30s.");
        assert!(!o.reload && o.meta, "a TTL reads the row only");
        let o = outcome(
            &WriteOp::KeyTtl {
                key: k(),
                ttl: WriteTtl::Persist,
            },
            &WriteReply::Int(0),
        );
        assert_eq!((o.done, o.message.as_str()), (false, "The key had no TTL."));
        let o = outcome(
            &WriteOp::HSetNx {
                key: k(),
                field: raw("f"),
                value: raw("v"),
            },
            &WriteReply::Int(0),
        );
        assert!(!o.done && !o.reload, "{o:?}");
        let o = outcome(
            &WriteOp::SetString {
                key: k(),
                old: raw("a"),
                value: raw("b"),
            },
            &WriteReply::Changed,
        );
        assert_eq!(o.message, CHANGED_SINCE);
        assert!(o.reload && o.meta && !o.done);
        let o = outcome(
            &WriteOp::HSet {
                key: k(),
                field: raw("f"),
                value: raw("v"),
            },
            &WriteReply::Int(0),
        );
        assert_eq!((o.done, o.message.as_str()), (true, "Saved."));
        assert!(o.reload && o.meta);
        let o = outcome(
            &WriteOp::LDel {
                key: k(),
                index: 0,
                old: raw("a"),
                marker: "m".into(),
            },
            &WriteReply::Ok,
        );
        assert_eq!(o.message, "Deleted.");
        // None of them names a TUI key or quotes a server.
        for m in [
            GONE,
            STALE,
            WRITING,
            UNCHANGED,
            ADD_OFF,
            REMOVE_OFF,
            REMOVE_UNKNOWN,
            FIELD_TTL_HASH,
            CHANGED_SINCE,
            NOTHING_SELECTED,
        ] {
            assert_eq!(crate::web::wording::banned(m), None, "{m}");
            assert_eq!(crate::web::wording::server_free(m), m, "{m}");
        }
    }

    #[test]
    fn the_tuis_edit_refusals_read_in_the_dashboards_terms() {
        for tui in [
            "e is off: only the first 64 KiB of this string is loaded, and an edit needs the \
             whole value.",
            "e is off: the string's length is unknown (STRLEN was refused), and an edit needs \
             the whole value.",
            "e is off: only the first 64 KiB of this document is loaded, and an edit needs the \
             whole document.",
            "A stream entry can't be edited: + adds an entry, d deletes this one.",
            "A TTL of 0 would delete the key: D deletes it.",
        ] {
            let w = words(tui);
            assert_ne!(w, crate::web::wording::FALLBACK, "{tui}");
            assert_ne!(w, tui, "{tui}");
            assert!(!w.starts_with("e is"), "{w}");
            assert!(!w.contains(" D ") && !w.contains(" + "), "{w}");
        }
        // The TUI's parsers' own words pass as they are.
        let e = value_view::parse_ttl("0").unwrap_err();
        assert_eq!(
            words(&e),
            "A TTL of 0 would delete the key: delete the key instead."
        );
        let e = value_view::parse_score("x").unwrap_err();
        assert_eq!(words(&e), e);
    }
}
