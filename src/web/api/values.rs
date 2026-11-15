//! A key's value (`GET /api/value`, `/api/value/more`, `/api/value/item`), read as the
//! TUI's key detail reads it: its own [`ValueState`], so the same commands, pages of
//! [`PAGE_ITEMS`](crate::live::values::PAGE_ITEMS), at most [`MAX_ITEMS`] held, strings
//! cut at 64 KiB, JSON documents only when their size is known and small enough, the
//! module types summarised once. On a cluster each read goes to the owner of the key's
//! slot.
//!
//! What the server sent is never logged or saved; it is in the reply only. Items are
//! listed with the TUI's one-line previews ([`value::preview`]); one item's whole
//! rendering, in the format asked (text, hex, JSON, MessagePack, PHP, Pickle, Protobuf,
//! Java, as `f` steps through them), comes from `/api/value/item`, with the text to copy
//! when the TUI would copy it ([`value::copy_text`]). Every line is escaped by
//! [`value::render`]; names and previews by [`value::escape`] and [`name_text`].
//!
//! Values open at once are kept server side, at most [`KEPT`], each named by `view`.

use std::collections::VecDeque;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::keys::{KeyDto, ttl_text};
use super::{App, Refused, error, ok};
use crate::live::keys::KeyRow;
use crate::live::values::{
    Items, JSON_MAX_SIZE, JSON_SHOWN, Kind, MAX_ITEMS, Module, STRING_BYTES, VECTOR_SAMPLE,
};
use crate::model::{KeyType, Ttl};
use crate::tui::app::ConnId;
use crate::tui::app::views::value::ValueState;
use crate::value::{self, CopyRefusal, Format, Raw};
use crate::web::keyname::{self, Name, bytes_text as name_text};

/// Values kept open at once.
pub const KEPT: usize = 8;
/// Characters of an item's one-line preview.
pub const PREVIEW: usize = 80;
/// The width an item is rendered at unless the page asks for another.
pub const WIDTH: usize = 100;

pub const NO_VIEW: &str = "That value is no longer open (a newer one replaced it, or the session changed): open it again.";
pub const NO_ITEM: &str = "There is no such item in what is loaded.";
pub const BAD_FORMAT: &str =
    "the format must be one of text, hex, json, msgpack, php, pickle, protobuf or java";
pub const STILL_LOADING: &str = "Still loading the value: wait for it.";
/// What copying says for a value with control characters (the TUI's, without its keys).
pub const COPY_REFUSED: &str =
    "The value has control characters, so it is not copied as text: show it as hex to copy it.";
pub const MODULE_NO_FORMAT: &str =
    "A module type's summary has no formats and is not copied: only values are.";
pub const STREAM_NO_COPY: &str =
    "A stream entry has several fields: open one field to see or copy its value.";

/// One value open.
#[derive(Debug)]
pub struct View {
    pub id: u64,
    conn: ConnId,
    pub state: ValueState,
    loading: bool,
}

/// The values open in the session, newest last, and what the session learned about
/// field TTLs (kept when another key opens, as in the TUI).
#[derive(Debug, Default)]
pub struct Views {
    next: u64,
    views: VecDeque<View>,
    httl_off: Option<String>,
}

impl Views {
    fn add(&mut self, conn: ConnId, row: KeyRow, pattern: String) -> u64 {
        self.next += 1;
        let mut state = ValueState::default();
        state.row = Some(row);
        state.pattern = pattern;
        state.httl_off = self.httl_off.clone();
        if self.views.len() >= KEPT {
            self.views.pop_front();
        }
        self.views.push_back(View {
            id: self.next,
            conn,
            state,
            loading: false,
        });
        self.next
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut View> {
        self.views.iter_mut().find(|v| v.id == id)
    }

    fn get(&self, id: u64) -> Option<&View> {
        self.views.iter().find(|v| v.id == id)
    }

    pub fn clear(&mut self) {
        self.views.clear();
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenQuery {
    /// The key's id (hex), from a key list or a panel.
    key: String,
    /// Its type, when the page knows it and no list or report has it.
    #[serde(rename = "type")]
    key_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ViewQuery {
    view: u64,
}

#[derive(Debug, Deserialize)]
pub struct ItemQuery {
    view: u64,
    #[serde(default)]
    index: usize,
    /// A stream entry's field.
    field: Option<usize>,
    format: Option<String>,
    width: Option<usize>,
}

/// `GET /api/value?key=<id>[&type=<t>]`: opens a key's value and reads its first page.
/// The key's metadata (type, TTL, size, length) is its row in the newest key list that
/// has it, else what the analysis sampled of it, else only the type given.
pub async fn open(State(app): State<App>, Query(q): Query<OpenQuery>) -> Response {
    let key = match keyname::decode(&q.key) {
        Ok(k) => k,
        Err(r) => return r.into_response(),
    };
    let made = app.hub.with_mut(|c| {
        if !c.live_ready() {
            return Err(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE));
        }
        let row = c
            .browse
            .lists
            .row(&key)
            .or_else(|| c.session.report.as_ref().and_then(|r| sampled_row(r, &key)))
            .unwrap_or_else(|| bare_row(&key, q.key_type.as_deref()));
        let pattern = c
            .session
            .report
            .as_ref()
            .map(|r| crate::analysis::pattern_of(r, &key))
            .unwrap_or_default();
        let conn = c.id;
        Ok(c.browse.values.add(conn, row, pattern))
    });
    let id = match made {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => return r.into_response(),
        Some(Ok(id)) => id,
    };
    read(&app, id).await
}

/// `GET /api/value/more?view=<n>`: the next page, as `n` on the key detail. When there
/// is none to read the reply says why in `notice`, as the TUI does.
pub async fn more(State(app): State<App>, Query(q): Query<ViewQuery>) -> Response {
    let known = app.hub.with(|c| {
        c.browse
            .values
            .get(q.view)
            .filter(|v| v.conn == c.id)
            .map(|v| v.loading)
    });
    match known.flatten() {
        None => error(StatusCode::NOT_FOUND, NO_VIEW),
        Some(true) => error(StatusCode::CONFLICT, STILL_LOADING),
        Some(false) => read(&app, q.view).await,
    }
}

/// Reads the next page of view `id` when there is one to read, then answers with it.
async fn read(app: &App, id: u64) -> Response {
    let step = app
        .hub
        .with_mut(|c| {
            let conn = c.id;
            let server = c.session.server.clone();
            let v = c.browse.values.get_mut(id).filter(|v| v.conn == conn)?;
            Some((
                conn,
                can_read(&v.state).then(|| {
                    v.loading = true;
                    v.state.next_request(server.as_ref())
                }),
            ))
        })
        .flatten();
    let Some((conn, req)) = step else {
        return error(StatusCode::NOT_FOUND, NO_VIEW);
    };
    if let Some(Some((dest, req))) = req {
        let result = app
            .hub
            .request(Some(conn), dest, crate::live::LiveRequest::Value(req))
            .await;
        app.hub.with_mut(|c| {
            if c.id != conn {
                return;
            }
            let Some(v) = c.browse.values.get_mut(id) else {
                return;
            };
            v.loading = false;
            v.state.answer(result);
            if v.state.httl_off.is_some() {
                c.browse.values.httl_off = v.state.httl_off.clone();
            }
        });
    } else if req.is_some() {
        app.hub.with_mut(|c| {
            if let Some(v) = c.browse.values.get_mut(id) {
                v.loading = false;
            }
        });
    }
    match app
        .hub
        .with(|c| {
            c.browse
                .values
                .get(id)
                .map(|v| body(v, c.session.nodes.len()))
        })
        .flatten()
    {
        Some(b) => ok(b),
        None => error(StatusCode::NOT_FOUND, NO_VIEW),
    }
}

/// Whether a page is to be read: the first, or a next one the TUI's `n` would read.
fn can_read(s: &ValueState) -> bool {
    if s.pages == 0 {
        return !s.gone && !s.refused_read();
    }
    !(s.gone || s.refused_read() || s.done() || held_all(s))
}

/// [`MAX_ITEMS`] are held: no more are loaded.
fn held_all(s: &ValueState) -> bool {
    s.capped || (s.loaded() >= MAX_ITEMS && !s.done())
}

/// What the TUI's `n` says when it reads nothing more.
pub fn notice(s: &ValueState) -> Option<String> {
    if s.gone {
        return Some("The key no longer exists.".to_string());
    }
    if s.refused_read() {
        return s.error.clone();
    }
    if held_all(s) {
        return Some(format!(
            "At most {} items are held, so no more are loaded.",
            crate::format::grouped(MAX_ITEMS as u64)
        ));
    }
    if !s.done() {
        return None;
    }
    Some(match s.kind() {
        Some(Kind::Vector) => format!(
            "Vector sets can't be read in pages: these are {VECTOR_SAMPLE} random elements."
        ),
        Some(Kind::String | Kind::Json) => "The value is loaded.".to_string(),
        Some(k) if k.is_module() => "The value is loaded.".to_string(),
        Some(Kind::Other(_)) => "This type's value is not read.".to_string(),
        _ => format!(
            "All {} items are loaded.",
            crate::format::grouped(s.loaded() as u64)
        ),
    })
}

/// The row a key sampled by the analysis has: its type, size and TTL.
fn sampled_row(r: &crate::model::ScanReport, key: &str) -> Option<KeyRow> {
    let k = r
        .big_keys()
        .iter()
        .chain(&r.hot_keys)
        .chain(r.patterns.iter().flat_map(|p| &p.examples))
        .find(|k| k.key == key)?;
    Some(KeyRow {
        key: key.to_string(),
        key_type: k.key_type,
        ttl: k.ttl,
        bytes: k.bytes,
        encoding: None,
        length: None,
        freq: k.lfu_freq,
    })
}

/// A row with only the key and the type the page gave (the first read's `TYPE` says
/// the real one).
fn bare_row(key: &str, key_type: Option<&str>) -> KeyRow {
    let key_type = match key_type {
        Some("json") => KeyType::Json,
        Some(t) => KeyType::from_type_reply(t),
        None => KeyType::String,
    };
    KeyRow {
        key: key.to_string(),
        key_type,
        ttl: Ttl::Persistent,
        bytes: None,
        encoding: None,
        length: None,
        freq: None,
    }
}

/// `string`, `hash`, …, `json`, `vector`, `array`, `module` or `other`.
pub fn kind_name(k: &Kind) -> &'static str {
    match k {
        Kind::String => "string",
        Kind::Hash => "hash",
        Kind::List => "list",
        Kind::Set => "set",
        Kind::ZSet => "zset",
        Kind::Stream => "stream",
        Kind::Json => "json",
        Kind::Vector => "vector",
        Kind::Array => "array",
        k if k.is_module() => "module",
        _ => "other",
    }
}

fn preview(raw: &Raw) -> String {
    value::preview(raw.as_bytes(), PREVIEW)
}

/// One item of a collection, as the TUI's list shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Item {
    pub index: usize,
    /// A hash field's name, a sorted set's member, a stream entry's ID.
    pub name: Option<String>,
    /// The value's one-line preview (a sorted set: its score; a stream entry: none).
    pub value: Option<String>,
    /// A hash field's TTL (`never` or a duration), when field TTLs are read.
    pub ttl: Option<String>,
    /// An array element's index.
    pub at: Option<u64>,
    /// A stream entry's fields, previewed.
    pub fields: Option<Vec<Field>>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: String,
}

fn item(index: usize) -> Item {
    Item {
        index,
        name: None,
        value: None,
        ttl: None,
        at: None,
        fields: None,
    }
}

/// The items loaded, previewed.
pub fn items(items: &Items) -> Vec<Item> {
    match items {
        Items::Hash(f) => f
            .iter()
            .enumerate()
            .map(|(i, f)| Item {
                name: Some(name_text(f.name.as_bytes())),
                value: Some(preview(&f.value)),
                ttl: f.ttl.map(ttl_text),
                ..item(i)
            })
            .collect(),
        Items::List(v) | Items::Set(v) | Items::Vector(v) => v
            .iter()
            .enumerate()
            .map(|(i, e)| Item {
                value: Some(preview(e)),
                ..item(i)
            })
            .collect(),
        Items::ZSet(m) => m
            .iter()
            .enumerate()
            .map(|(i, m)| Item {
                name: Some(preview(&m.member)),
                value: Some(value::preview(m.score.as_bytes(), 24)),
                ..item(i)
            })
            .collect(),
        Items::Stream(e) => e
            .iter()
            .enumerate()
            .map(|(i, e)| Item {
                name: Some(value::escape(&e.id)),
                fields: Some(
                    e.fields
                        .iter()
                        .map(|(k, v)| Field {
                            name: name_text(k.as_bytes()),
                            value: preview(v),
                        })
                        .collect(),
                ),
                ..item(i)
            })
            .collect(),
        Items::Array(v) => v
            .iter()
            .enumerate()
            .map(|(i, e)| Item {
                at: Some(e.index),
                value: Some(preview(&e.value)),
                ..item(i)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A module type's summary, as the TUI's module pane shows it.
fn module(m: &Module, kind: Option<&Kind>) -> serde_json::Value {
    let pairs = |v: &[(Raw, Raw)]| -> Vec<serde_json::Value> {
        v.iter()
            .map(|(k, v)| serde_json::json!({ "name": name_text(k.as_bytes()), "value": value::preview(v.as_bytes(), 200) }))
            .collect()
    };
    serde_json::json!({
        "label": kind.and_then(Kind::module_label),
        "info": pairs(&m.info),
        "samples": m.samples.iter().map(|s| serde_json::json!({
            "ts_ms": s.ts,
            "value": value::preview(s.value.as_bytes(), 40),
        })).collect::<Vec<_>>(),
        "top": pairs(&m.top),
        "stats": m.stats.iter().map(|(l, v)| serde_json::json!({
            "name": l,
            "value": value::preview(v.as_bytes(), 40),
        })).collect::<Vec<_>>(),
        "missing": m.missing.iter().map(|t| crate::web::wording::server_free(t)).collect::<Vec<_>>(),
        "unavailable": m.unavailable.as_deref().map(crate::web::wording::server_free),
    })
}

/// The body of an open value.
fn body(v: &View, nodes: usize) -> serde_json::Value {
    let s = &v.state;
    let kind = s.kind();
    let row = s.row.as_ref();
    let type_name = s
        .type_name
        .as_deref()
        .map(value::escape)
        .or_else(|| row.map(|r| r.key_type.as_str().to_string()));
    let mut message: Option<String> = None;
    let mut shown: Option<Shown> = None;
    let mut list: Vec<Item> = Vec::new();
    let mut module_part = serde_json::Value::Null;
    let mut cut: Option<String> = None;
    match &s.items {
        Some(Items::NotRead) => {
            message = Some(format!(
                "redis-lens does not read values of type {}.",
                type_name.clone().unwrap_or_default()
            ));
        }
        Some(Items::JsonNotRead { .. }) => {
            let max = crate::format::bytes(JSON_MAX_SIZE);
            message = Some(match row.and_then(|r| r.bytes) {
                Some(b) => format!(
                    "The document is {}, over the {max} redis-lens reads, so it is not read.",
                    crate::format::bytes(b)
                ),
                None => format!(
                    "The document's size is unknown (no MEMORY USAGE), so it is not read: \
                     redis-lens reads documents of at most {max}."
                ),
            });
        }
        Some(Items::String { value, len }) => {
            if let Some(n) = len.filter(|n| *n > STRING_BYTES as u64) {
                cut = Some(format!("first 64 KiB of {}", crate::format::bytes(n)));
            }
            shown = Some(Shown::of("value", value, s.format, WIDTH, cut.clone()));
        }
        Some(Items::Json { doc, len }) => {
            if *len > JSON_SHOWN {
                cut = Some(format!(
                    "cut at {} of {}",
                    crate::format::bytes(JSON_SHOWN as u64),
                    crate::format::bytes(*len as u64)
                ));
            }
            shown = Some(Shown::of("document", doc, s.format, WIDTH, cut.clone()));
        }
        Some(Items::Module(m)) => module_part = module(m, kind.as_ref()),
        Some(other) => list = items(other),
        None => {}
    }
    let refused: Vec<String> = s
        .refused
        .iter()
        .map(|e| crate::web::wording::server_free(&format!("Left out: {e}.")))
        .collect();
    let mut meta = row.map(|r| serde_json::json!(KeyDto::of(r, None)));
    // A row made from the key alone guessed its type: the first read's TYPE says it.
    if let (Some(m), Some(t)) = (meta.as_mut(), type_name.as_ref()) {
        m["type"] = serde_json::json!(t);
    }
    serde_json::json!({
        "view": v.id,
        "key": row.map(|r| Name::of(&r.key)),
        "type": type_name,
        "kind": kind.as_ref().map(kind_name),
        "command": kind.as_ref().and_then(Kind::command),
        "meta": meta,
        "pattern": (!s.pattern.is_empty()).then(|| Name::of(&s.pattern)),
        "loading": v.loading,
        "pages": s.pages,
        "loaded": s.loaded(),
        "length": row.and_then(|r| r.length),
        "done": s.done(),
        "more": can_read(s) && s.pages > 0,
        "capped": held_all(s),
        "gone": s.gone,
        "error": s.error.as_deref().map(crate::web::wording::server_free),
        "notice": notice(s).map(|t| crate::web::wording::server_free(&t)),
        "refused": refused,
        "field_ttls": s.field_ttls,
        "field_ttls_off": s.httl_off.as_deref().map(crate::web::wording::server_free),
        "items": list,
        "value": shown,
        "cut": cut,
        "message": message,
        "module": module_part,
        "cluster": nodes > 0,
        "max_items": MAX_ITEMS,
    })
}

/// One value drawn in one format, as the TUI's value pane draws it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Shown {
    /// The format the lines are in (`text`, `hex`, `JSON`, …).
    pub format: &'static str,
    /// The format the bytes look like.
    pub detected: &'static str,
    /// The format asked for, when one was.
    pub picked: Option<&'static str>,
    /// Every format there is, in the order the TUI's `f` steps through them.
    pub formats: Vec<&'static str>,
    pub bytes: usize,
    pub compression: Option<&'static str>,
    pub truncated: bool,
    /// Why the format asked for was not used, in redis-lens's words.
    pub note: Option<&'static str>,
    /// The TUI's header line: `value · JSON · 1,204 bytes · auto-detected`.
    pub header: String,
    /// Escaped lines, at most `value::MAX_LINES`.
    pub lines: Vec<String>,
    /// The text to copy, in the format shown; `null` when it is refused.
    pub copy: Option<String>,
    pub copy_refused: Option<&'static str>,
}

impl Shown {
    pub fn of(
        label: &str,
        raw: &Raw,
        picked: Option<Format>,
        width: usize,
        cut: Option<String>,
    ) -> Self {
        let d = value::decode(raw.as_bytes());
        let format = picked.unwrap_or(d.detected);
        let r = value::render(&d, format, width);
        let mut parts = vec![label.to_string()];
        let n = format!("{} bytes", crate::format::grouped(raw.len() as u64));
        match picked {
            None => {
                parts.push(r.format.label().to_string());
                parts.push(n);
                parts.push("auto-detected".to_string());
            }
            Some(f) => {
                parts.push(n);
                parts.push(format!("shown as {}", f.label()));
            }
        }
        if let Some(c) = d.compression {
            parts.push(format!("decompressed from {}", c.label()));
        }
        if d.truncated {
            parts.push("truncated at 1 MiB".to_string());
        }
        if let Some(note) = r.note {
            parts.push(note.to_string());
        }
        parts.extend(cut);
        let (copy, copy_refused) = match value::copy_text(&d, format) {
            Ok(t) => (Some(t), None),
            Err(CopyRefusal::ControlCharacters) => (None, Some(COPY_REFUSED)),
        };
        Self {
            format: r.format.label(),
            detected: d.detected.label(),
            picked: picked.map(Format::label),
            formats: Format::ALL.iter().map(|f| f.label()).collect(),
            bytes: raw.len(),
            compression: d.compression.map(|c| c.label()),
            truncated: d.truncated,
            note: r.note,
            header: parts.join(" · "),
            lines: r.lines().to_vec(),
            copy,
            copy_refused,
        }
    }
}

/// A format by the name the page sends: the label, any case, or `msgpack`.
pub fn format_of(name: &str) -> Option<Format> {
    let n = name.to_ascii_lowercase();
    Format::ALL
        .into_iter()
        .find(|f| f.label().to_ascii_lowercase() == n || (n == "msgpack" && *f == Format::MsgPack))
}

/// `GET /api/value/item?view=<n>&index=<i>[&field=<j>][&format=<f>][&width=<w>]`: one
/// item's whole value, rendered: a hash field's value, a list, set, vector or array
/// element, a sorted set member, a stream entry's field `field`, or (index 0) the
/// string or document.
pub async fn item_route(State(app): State<App>, Query(q): Query<ItemQuery>) -> Response {
    let picked = match q.format.as_deref().map(format_of) {
        Some(None) => return error(StatusCode::BAD_REQUEST, BAD_FORMAT),
        Some(Some(f)) => Some(f),
        None => None,
    };
    let width = q.width.unwrap_or(WIDTH).clamp(16, 400);
    let shown = app.hub.with(|c| {
        let v = c.browse.values.get(q.view).filter(|v| v.conn == c.id)?;
        Some(shown_item(&v.state, q.index, q.field, picked, width))
    });
    match shown.flatten() {
        None => error(StatusCode::NOT_FOUND, NO_VIEW),
        Some(Err(why)) => error(StatusCode::UNPROCESSABLE_ENTITY, why),
        Some(Ok(b)) => ok(b),
    }
}

/// Item `index` (field `field` of a stream entry) of `s`, rendered.
pub fn shown_item(
    s: &ValueState,
    index: usize,
    field: Option<usize>,
    picked: Option<Format>,
    width: usize,
) -> Result<serde_json::Value, &'static str> {
    let Some(items) = s.items.as_ref() else {
        return Err(NO_ITEM);
    };
    let (label, raw, name): (&str, &Raw, Option<String>) = match items {
        Items::String { value, .. } if index == 0 => ("value", value, None),
        Items::Json { doc, .. } if index == 0 => ("document", doc, None),
        Items::Hash(f) => {
            let f = f.get(index).ok_or(NO_ITEM)?;
            ("value", &f.value, Some(name_text(f.name.as_bytes())))
        }
        Items::List(v) | Items::Set(v) | Items::Vector(v) => {
            ("element", v.get(index).ok_or(NO_ITEM)?, None)
        }
        Items::ZSet(m) => {
            let m = m.get(index).ok_or(NO_ITEM)?;
            (
                "member",
                &m.member,
                Some(format!("score {}", value::preview(m.score.as_bytes(), 24))),
            )
        }
        Items::Array(v) => {
            let e = v.get(index).ok_or(NO_ITEM)?;
            ("element", &e.value, Some(format!("index {}", e.index)))
        }
        Items::Stream(e) => {
            let e = e.get(index).ok_or(NO_ITEM)?;
            let Some(j) = field else {
                return Err(STREAM_NO_COPY);
            };
            let (k, v) = e.fields.get(j).ok_or(NO_ITEM)?;
            ("value", v, Some(name_text(k.as_bytes())))
        }
        Items::Module(_) => return Err(MODULE_NO_FORMAT),
        _ => return Err(NO_ITEM),
    };
    let shown = Shown::of(label, raw, picked, width, None);
    Ok(serde_json::json!({ "index": index, "field": field, "name": name, "shown": shown }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::values::{Entry, Field as VField, Member, Page, Reply};
    use crate::live::{LiveError, LiveReply};

    fn state(kind: KeyType, items: Items, next: bool) -> ValueState {
        let mut s = ValueState::default();
        s.row = Some(bare_row("k", Some(kind.as_str())));
        s.answer(Ok(LiveReply::Value(Reply::Page(Page {
            type_name: Some(kind.as_str().into()),
            items,
            next: next.then_some(crate::live::values::At::Index(100)),
            refused: Vec::new(),
        }))));
        s
    }

    #[test]
    fn a_string_with_control_characters_is_escaped_and_not_copied_as_text() {
        let s = state(
            KeyType::String,
            Items::String {
                value: Raw::from("hi\x1b[31m there\n"),
                len: Some(15),
            },
            false,
        );
        let v = shown_item(&s, 0, None, None, 80).unwrap();
        let lines = v["shown"]["lines"].as_array().unwrap();
        assert!(lines.iter().all(|l| !l.as_str().unwrap().contains('\x1b')));
        assert_eq!(v["shown"]["copy"], serde_json::Value::Null);
        assert_eq!(v["shown"]["copy_refused"], COPY_REFUSED);
        // As hex it copies.
        let hex = shown_item(&s, 0, None, Some(Format::Hex), 80).unwrap();
        assert_eq!(hex["shown"]["format"], "hex");
        assert_eq!(hex["shown"]["copy"], "68691b5b33316d2074686572650a");
    }

    #[test]
    fn json_is_detected_and_its_header_reads_as_the_tuis() {
        let s = state(
            KeyType::String,
            Items::String {
                value: Raw::from(r#"{"a":1}"#),
                len: Some(7),
            },
            false,
        );
        let v = shown_item(&s, 0, None, None, 80).unwrap();
        assert_eq!(v["shown"]["format"], "JSON");
        assert_eq!(
            v["shown"]["header"],
            "value · JSON · 7 bytes · auto-detected"
        );
        assert_eq!(v["shown"]["copy"], r#"{"a":1}"#);
        assert_eq!(notice(&s).as_deref(), Some("The value is loaded."));
    }

    #[test]
    fn collections_preview_their_items_and_name_fields_safely() {
        let h = Items::Hash(vec![VField {
            name: Raw::from(&b"f\xffx"[..]),
            value: Raw::from("v\x07"),
            ttl: Some(Ttl::Persistent),
        }]);
        let it = items(&h);
        assert_eq!(it[0].name.as_deref(), Some("f\\xffx"));
        assert!(!it[0].value.as_ref().unwrap().contains('\x07'));
        assert_eq!(it[0].ttl.as_deref(), Some("never"));
        let z = items(&Items::ZSet(vec![Member {
            member: Raw::from("m"),
            score: Raw::from("1.5"),
        }]));
        assert_eq!(z[0].name.as_deref(), Some("m"));
        assert_eq!(z[0].value.as_deref(), Some("1.5"));
        let st = Items::Stream(vec![Entry {
            id: "1-0".into(),
            fields: vec![(Raw::from("a"), Raw::from("b"))],
        }]);
        let it = items(&st);
        assert_eq!(it[0].fields.as_ref().unwrap()[0].value, "b");
    }

    #[test]
    fn a_stream_entry_opens_one_field_and_a_module_has_no_formats() {
        let s = state(
            KeyType::Stream,
            Items::Stream(vec![Entry {
                id: "1-0".into(),
                fields: vec![(Raw::from("a"), Raw::from("b"))],
            }]),
            false,
        );
        assert_eq!(shown_item(&s, 0, None, None, 80), Err(STREAM_NO_COPY));
        let v = shown_item(&s, 0, Some(0), None, 80).unwrap();
        assert_eq!(v["shown"]["lines"][0], "b");
        assert_eq!(shown_item(&s, 3, Some(0), None, 80), Err(NO_ITEM));
        let m = state(KeyType::Other, Items::Module(Box::default()), false);
        assert_eq!(shown_item(&m, 0, None, None, 80), Err(MODULE_NO_FORMAT));
    }

    #[test]
    fn paging_stops_at_the_cap_and_says_why() {
        let many = Items::List(
            (0..MAX_ITEMS)
                .map(|i| Raw::from(i.to_string().as_str()))
                .collect(),
        );
        let s = state(KeyType::List, many, true);
        assert!(held_all(&s));
        assert!(!can_read(&s));
        assert_eq!(
            notice(&s).as_deref(),
            Some("At most 10,000 items are held, so no more are loaded.")
        );
        let few = state(KeyType::List, Items::List(vec![Raw::from("a")]), true);
        assert!(can_read(&few));
        assert_eq!(notice(&few), None);
    }

    #[test]
    fn a_refused_read_is_not_asked_again_and_says_so_in_fixed_words() {
        let mut s = ValueState::default();
        s.row = Some(bare_row("k", None));
        s.answer(Err(LiveError::Denied("GETRANGE".into())));
        assert!(s.refused_read());
        assert!(!can_read(&s));
        assert_eq!(
            notice(&s).as_deref(),
            Some("Can't read the value: the server refused GETRANGE for this user.")
        );
        let gone = {
            let mut s = ValueState::default();
            s.row = Some(bare_row("k", None));
            s.answer(Ok(LiveReply::Value(Reply::Gone)));
            s
        };
        assert!(!can_read(&gone));
        assert_eq!(notice(&gone).as_deref(), Some("The key no longer exists."));
    }

    #[test]
    fn a_key_opened_by_name_alone_takes_its_type_from_the_first_read() {
        let mut s = ValueState::default();
        s.row = Some(bare_row("k", None));
        s.answer(Ok(LiveReply::Value(Reply::Page(Page {
            type_name: Some("stream".into()),
            items: Items::Stream(Vec::new()),
            next: None,
            refused: Vec::new(),
        }))));
        let v = View {
            id: 1,
            conn: 1,
            state: s,
            loading: false,
        };
        let b = body(&v, 0);
        assert_eq!(b["type"], "stream");
        assert_eq!(b["meta"]["type"], "stream");
        assert_eq!(b["kind"], "stream");
    }

    #[test]
    fn formats_are_named_as_the_page_sends_them() {
        assert_eq!(format_of("hex"), Some(Format::Hex));
        assert_eq!(format_of("JSON"), Some(Format::Json));
        assert_eq!(format_of("msgpack"), Some(Format::MsgPack));
        assert_eq!(format_of("messagepack"), Some(Format::MsgPack));
        assert_eq!(format_of("yaml"), None);
    }
}
