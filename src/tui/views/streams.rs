//! State of the Streams view (spec "4 Streams"). Owner: wave B. Drawn by
//! `ui/streams.rs`; its live requests are in `src/live/streams.rs`.
//!
//! Lists the streams the last scan found and polls `XINFO` for them every 5 s while the
//! view is shown (`p` pauses). The poll reads entry IDs only. See [`super`] for the
//! interface `App` calls.
//!
//! `enter` lists the selected stream's entries, one level deeper: a
//! [`ValueState`](super::value::ValueState), the key detail's own state, reads them in
//! pages of 100 with `XREVRANGE` (the key detail's `LiveRequest::Value`, no command of
//! its own) and `ui/value.rs` draws them, so `↑↓`, `tab`, `n`, `f` and `y` work as on
//! the key detail. `esc` goes back to the list. While the entries are shown the `XINFO`
//! poll stops; back on the list it reads at once when a poll is due.
//!
//! Writes (P3), each through [`Act::write`], the one gate:
//!
//! - `c` on a group, consumer or pending entry of the details claims the group's pending
//!   entries idle long enough for another consumer (`XAUTOCLAIM`), from a form.
//! - `A` on a group or consumer lists its pending entries (`XPENDING`, the first 100)
//!   under it, a read that works on a read-only profile too; on one of them, it
//!   acknowledges it (`XACK`).
//! - `X` trims the selected stream (`XTRIM MAXLEN ~ n` or `MINID ~ id`), from a form.
//!
//! After each, the stream (and its pending list, when open) is read again.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use self::form::{Field, Form, Step, write_failed, write_off};
use super::value::ValueState;
use super::{Act, Input, ViewState};
use crate::live::keys::KeyRow;
use crate::live::streams::{
    ConsumerInfo, GroupInfo, MAX_STREAMS, Pending, PendingEntry, Refused, Reply, Request,
    StreamInfo, Verdict, verdicts,
};
use crate::live::write::Reply as WriteReply;
use crate::live::{Dest, Follow, LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{KeyType, PatternStats, ScanReport, Ttl};
use crate::value::escape;
use crate::write::{Trim, WriteOp};

/// The forms of the write actions of this view and of Server, Pub/Sub and Search.
#[path = "form.rs"]
pub mod form;

/// How often `XINFO` is polled while the view is shown (spec "Shared command budget").
pub const POLL: Duration = Duration::from_secs(5);
/// Rows of the group and consumer list in the details panel that fit at once.
const DETAIL_PAGE: usize = 5;

/// What `r`, `p` and the list keys act on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    List,
    /// The groups and consumers of the selected stream.
    Detail,
}

/// One stream of the list, with what the last poll said about it.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRow {
    pub key: String,
    /// `None`: the key is gone, or no longer a stream, since the scan.
    pub info: Option<StreamInfo>,
    /// Every verdict that applies; the first one is shown in the list.
    pub verdicts: Vec<Verdict>,
}

/// A line of the details panel: a group, one of its consumers, or a pending entry
/// listed with `A`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetailRow<'a> {
    Group(&'a GroupInfo),
    Consumer(&'a GroupInfo, &'a ConsumerInfo),
    Pending(&'a GroupInfo, &'a PendingEntry),
}

/// The default `COUNT` of a claim.
pub const CLAIM_COUNT: u64 = 100;
/// The default min idle time of a claim from a group row: one minute.
pub const CLAIM_IDLE_MS: u64 = 60_000;

/// A form open on the view.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamForm {
    /// `c`: claim pending entries of `group` on `key`.
    Claim {
        key: String,
        group: String,
        form: Form,
    },
    /// `X`: trim `key`.
    Trim { key: String, form: Form },
}

impl StreamForm {
    pub fn form(&self) -> &Form {
        match self {
            Self::Claim { form, .. } | Self::Trim { form, .. } => form,
        }
    }

    fn form_mut(&mut self) -> &mut Form {
        match self {
            Self::Claim { form, .. } | Self::Trim { form, .. } => form,
        }
    }

    /// The op the form sends as it is filled now, or what is wrong with it.
    pub fn op(&self) -> Result<WriteOp, String> {
        match self {
            Self::Claim { key, group, form } => claim_op(key, group, form),
            Self::Trim { key, form } => trim_op(key, form),
        }
    }
}

/// The `XAUTOCLAIM` of a claim form.
pub fn claim_op(key: &str, group: &str, form: &Form) -> Result<WriteOp, String> {
    let consumer = form.value("to consumer");
    if consumer.is_empty() {
        return Err("Type the consumer that takes the entries.".into());
    }
    let min_idle_ms = form
        .value("min idle ms")
        .parse::<u64>()
        .map_err(|_| "The min idle time is a whole number of milliseconds.".to_string())?;
    let count = form
        .value("count")
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "The count is a whole number, 1 or more.".to_string())?;
    Ok(WriteOp::XAutoClaim {
        key: key.to_string(),
        group: group.to_string(),
        consumer: consumer.to_string(),
        min_idle_ms,
        count,
    })
}

/// A stream ID as `XTRIM MINID` takes it: `ms` or `ms-seq`.
fn is_stream_id(s: &str) -> bool {
    let mut parts = s.splitn(2, '-');
    let ms = parts.next().unwrap_or("");
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    digits(ms) && parts.next().is_none_or(digits)
}

/// The `XTRIM` of a trim form.
pub fn trim_op(key: &str, form: &Form) -> Result<WriteOp, String> {
    let value = form.value("value");
    let trim = if form.value("trim by") == "MINID" {
        if !is_stream_id(value) {
            return Err("MINID takes an entry ID, such as 1700000000000-0.".into());
        }
        Trim::MinId(value.to_string())
    } else {
        Trim::MaxLen(
            value
                .parse::<u64>()
                .map_err(|_| "MAXLEN takes how many entries to keep.".to_string())?,
        )
    };
    Ok(WriteOp::XTrim {
        key: key.to_string(),
        trim,
    })
}

/// A write of this view in flight.
#[derive(Debug, Clone, PartialEq)]
enum Sent {
    Claim { key: String, consumer: String },
    Ack { key: String, id: String },
    Trim { key: String },
}

impl Sent {
    fn key(&self) -> &str {
        match self {
            Self::Claim { key, .. } | Self::Ack { key, .. } | Self::Trim { key } => key,
        }
    }
}

/// The groups of `info`, each followed by its consumers.
pub fn detail_rows(info: &StreamInfo) -> Vec<DetailRow<'_>> {
    let mut out = Vec::new();
    for g in info.groups.iter().flatten() {
        out.push(DetailRow::Group(g));
        for c in g.readers.iter().flatten() {
            out.push(DetailRow::Consumer(g, c));
        }
    }
    out
}

/// The stream list, its groups and consumers.
#[derive(Debug, Default)]
pub struct StreamsState {
    /// The last poll's answer, in scan order.
    pub rows: Vec<StreamRow>,
    /// How many streams the scan found; more than `rows` when cut at [`MAX_STREAMS`].
    pub found: usize,
    /// Selected row of the list.
    pub sel: usize,
    /// Selected row of the details panel.
    pub detail_sel: usize,
    pub focus: Focus,
    pub paused: bool,
    /// An answer has arrived (so an empty list means no streams, not "not read yet").
    pub loaded: bool,
    /// Why the view is off, in redis-lens's own words. A refused or unsupported
    /// `XINFO STREAM` turns it off until `r`.
    pub off: Option<String>,
    /// Why the last poll failed. The previous rows stay.
    pub error: Option<String>,
    /// The poll in flight: one request per node that owns a listed stream (one on a
    /// standalone server), with the keys each reads.
    pending: Vec<(ReqId, Vec<String>)>,
    /// The poll's answers so far, its refusals, and its first failure.
    gathered: Vec<crate::live::streams::Fetched>,
    gathered_refused: Refused,
    gathered_error: Option<LiveError>,
    /// The keys the poll in flight reads, in scan order.
    poll_keys: Vec<String>,
    last_sent: Option<Instant>,
    /// Optional commands the server refused, not asked again until `r`.
    refused: Refused,
    /// What the request in flight was told to skip.
    refused_sent: Refused,
    /// `r` was pressed while a poll was in flight: that poll's answer skipped what `r`
    /// asks again, so its refusals are not kept, and one more read follows it.
    reread: bool,
    /// The entries of the stream last opened with `enter`, kept after `esc` so that
    /// opening it again shows what was loaded. Values are [`Raw`](crate::value::Raw):
    /// its `Debug` shows lengths only.
    pub entries: Option<Box<ValueState>>,
    /// The entries are on screen instead of the list.
    pub entries_open: bool,
    /// The pending entries `A` listed: of which stream, and what `XPENDING` said.
    pub pending_list: Option<(String, Pending)>,
    /// The `XPENDING` read in flight.
    pending_read: Option<ReqId>,
    /// The form open, if any (`c`, `X`).
    pub form: Option<StreamForm>,
    /// The write in flight, if any.
    write: Option<(ReqId, Sent)>,
}

/// The keys of the streams the scan found: keys of type stream among the examples of every
/// pattern, and a pattern that is a literal key of type stream. A sampled scan keeps up to
/// five examples per pattern, so this may miss some.
pub fn stream_keys(report: &ScanReport) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in &report.patterns {
        for k in pattern_streams(p) {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

fn pattern_streams(p: &PatternStats) -> Vec<String> {
    let mut keys: Vec<String> = p
        .examples
        .iter()
        .filter(|e| e.key_type == KeyType::Stream)
        .map(|e| e.key.clone())
        .collect();
    // A pattern without placeholders or globs names one key.
    let literal = !p.pattern.contains(['{', '}', '*', '?', '[']);
    if keys.is_empty() && p.key_type == Some(KeyType::Stream) && literal {
        keys.push(p.pattern.clone());
    }
    keys
}

impl StreamsState {
    /// The selected stream.
    pub fn selected(&self) -> Option<&StreamRow> {
        self.rows.get(self.sel)
    }

    /// The details rows of the selected stream, with the pending entries `A` listed
    /// under their group or consumer.
    pub fn details(&self) -> Vec<DetailRow<'_>> {
        let Some(row) = self.selected() else {
            return Vec::new();
        };
        let Some(info) = row.info.as_ref() else {
            return Vec::new();
        };
        let rows = detail_rows(info);
        let Some((_, p)) = self.pending_list.as_ref().filter(|(k, _)| *k == row.key) else {
            return rows;
        };
        let mut out = Vec::with_capacity(rows.len() + p.entries.len());
        for r in rows {
            out.push(r);
            let (g, of) = match r {
                DetailRow::Group(g) => (g, None),
                DetailRow::Consumer(g, c) => (g, Some(&c.name)),
                DetailRow::Pending(..) => continue,
            };
            if g.name == p.group && of == p.consumer.as_ref() {
                out.extend(p.entries.iter().map(|e| DetailRow::Pending(g, e)));
            }
        }
        out
    }

    /// The pending list shown for the selected stream, if `A` listed one.
    pub fn shown_pending(&self) -> Option<&Pending> {
        let key = &self.selected()?.key;
        self.pending_list
            .as_ref()
            .filter(|(k, _)| k == key)
            .map(|(_, p)| p)
    }

    /// The selected row of the details, when they have the focus.
    fn detail_selected(&self) -> Option<DetailRow<'_>> {
        if self.focus != Focus::Detail {
            return None;
        }
        self.details().get(self.detail_sel).copied()
    }

    /// The op of the form open, as filled now, or what is wrong with it: for the
    /// form's "Sends:" line.
    pub fn form_sends(&self) -> Option<Result<String, String>> {
        self.form.as_ref().map(|f| f.op().map(|op| op.shown()))
    }

    /// The entries on screen, if they are.
    pub fn shown_entries(&self) -> Option<&ValueState> {
        self.entries.as_deref().filter(|_| self.entries_open)
    }

    /// `enter`: list the selected stream's entries. The stream opened last keeps what
    /// it loaded; another starts afresh.
    fn open_entries(&mut self, act: &mut Act<'_>) {
        let Some(row) = self.selected() else {
            act.info("No stream selected.");
            return;
        };
        let Some(info) = &row.info else {
            act.info("The key is gone, or no longer a stream. Rescan to update the list.");
            return;
        };
        let key_row = stream_row(&row.key, info);
        let entries = self.entries.get_or_insert_with(Box::default);
        entries.open(key_row, String::new(), act);
        self.entries_open = true;
        entries.on_show(act);
    }

    /// `r` with the entries open: read them again from the newest.
    fn reload_entries(&mut self, act: &mut Act<'_>) {
        let Some(entries) = self.entries.as_mut() else {
            return;
        };
        let Some(row) = entries.row.take() else {
            return;
        };
        **entries = ValueState::default();
        entries.open(row, String::new(), act);
        entries.on_show(act);
    }

    /// `c`: opens the claim form for the group of the selected details row.
    fn open_claim(&mut self, act: &mut Act<'_>) {
        let Some(key) = self.selected().map(|r| r.key.clone()) else {
            act.info("No stream selected.");
            return;
        };
        let (group, from, idle) = match self.detail_selected() {
            Some(DetailRow::Group(g)) => (g, None, CLAIM_IDLE_MS),
            Some(DetailRow::Consumer(g, c)) => (g, Some(c.name.clone()), c.idle_ms),
            Some(DetailRow::Pending(g, e)) => (g, Some(e.consumer.clone()), e.idle_ms),
            None => {
                act.info("Press tab and select a group or consumer: c claims its pending entries.");
                return;
            }
        };
        let sample = WriteOp::XAutoClaim {
            key: key.clone(),
            group: group.name.clone(),
            consumer: "c".into(),
            min_idle_ms: 0,
            count: CLAIM_COUNT,
        };
        if write_off(act, &sample) {
            return;
        }
        // Another consumer of the group, the most recently active, takes them.
        let to = group
            .readers
            .iter()
            .flatten()
            .filter(|c| Some(&c.name) != from.as_ref())
            .min_by_key(|c| c.idle_ms)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let mut about = vec![format!(
            "Stream {} · group {}",
            escape(&key),
            escape(&group.name)
        )];
        if let Some(from) = &from {
            about.push(format!(
                "From {}: entries idle at least min idle go to the new consumer.",
                escape(from)
            ));
        } else {
            about.push("Entries idle at least min idle go to the new consumer.".to_string());
        }
        let form = Form::new(
            "Claim pending entries",
            about,
            vec![
                Field::text("to consumer", "the consumer that takes the entries").with(to),
                Field::text(
                    "min idle ms",
                    "only entries delivered at least this long ago (XAUTOCLAIM claims any consumer's)",
                )
                .with((idle / 1000 * 1000).to_string()),
                Field::text("count", "the most entries claimed").with(CLAIM_COUNT.to_string()),
            ],
        );
        self.form = Some(StreamForm::Claim {
            key,
            group: group.name.clone(),
            form,
        });
    }

    /// `X`: opens the trim form for the selected stream.
    fn open_trim(&mut self, act: &mut Act<'_>) {
        let Some(row) = self.selected() else {
            act.info("No stream selected.");
            return;
        };
        let Some(info) = &row.info else {
            act.info("The key is gone, or no longer a stream. Rescan to update the list.");
            return;
        };
        let key = row.key.clone();
        let sample = WriteOp::XTrim {
            key: key.clone(),
            trim: Trim::MaxLen(0),
        };
        if write_off(act, &sample) {
            return;
        }
        let about = vec![
            format!(
                "Stream {} · {} entries",
                escape(&key),
                crate::format::grouped(info.length)
            ),
            "Approximate (~): the server may keep a few more.".to_string(),
        ];
        let form = Form::new(
            "Trim stream",
            about,
            vec![
                Field::choice(
                    "trim by",
                    &["MAXLEN", "MINID"],
                    "MAXLEN keeps the newest entries; MINID drops entries older than an ID",
                ),
                Field::text(
                    "value",
                    "MAXLEN: how many entries to keep · MINID: the oldest ID to keep",
                ),
            ],
        );
        self.form = Some(StreamForm::Trim { key, form });
    }

    /// A key while a form is open.
    fn form_key(&mut self, key: KeyEvent, act: &mut Act<'_>) {
        let Some(open) = self.form.as_mut() else {
            return;
        };
        match open.form_mut().key(key) {
            Step::Typed => {}
            Step::Cancel => self.form = None,
            Step::Submit => match open.op() {
                Err(why) => act.error(why),
                Ok(op) => {
                    let sent = match &op {
                        WriteOp::XAutoClaim { key, consumer, .. } => Sent::Claim {
                            key: key.clone(),
                            consumer: consumer.clone(),
                        },
                        _ => Sent::Trim {
                            key: op.name_to_type(),
                        },
                    };
                    self.form = None;
                    self.send_write(op, sent, act);
                }
            },
        }
    }

    fn send_write(&mut self, op: WriteOp, sent: Sent, act: &mut Act<'_>) {
        if let Some(id) = act.write(op) {
            self.write = Some((id, sent));
        }
    }

    /// `A`: on a pending entry, acknowledges it; on a group or consumer, lists its
    /// pending entries.
    fn ack_or_list(&mut self, act: &mut Act<'_>) {
        let Some(key) = self.selected().map(|r| r.key.clone()) else {
            act.info("No stream selected.");
            return;
        };
        let (group, consumer) = match self.detail_selected() {
            Some(DetailRow::Pending(g, e)) => {
                let op = WriteOp::XAck {
                    key: key.clone(),
                    group: g.name.clone(),
                    id: e.id.clone(),
                };
                let sent = Sent::Ack {
                    key,
                    id: e.id.clone(),
                };
                self.send_write(op, sent, act);
                return;
            }
            Some(DetailRow::Group(g)) => (g.name.clone(), None),
            Some(DetailRow::Consumer(g, c)) => (g.name.clone(), Some(c.name.clone())),
            None => {
                act.info("Press tab and select a group or consumer: A lists its pending entries.");
                return;
            }
        };
        // Listing them is a read: it works on a read-only profile too.
        if self.read_pending(key, group, consumer, act) {
            act.info(if act.writes_off().is_some() {
                "Listing the pending entries. Writes are off, so A can't acknowledge them."
            } else {
                "Listing the pending entries: select one and press A to acknowledge it."
            });
        }
    }

    /// Sends `XPENDING` for `group` (of `consumer`) of `key`. False when it can't.
    fn read_pending(
        &mut self,
        key: String,
        group: String,
        consumer: Option<String>,
        act: &mut Act<'_>,
    ) -> bool {
        // On a cluster, the stream's slot owner.
        let dest = Dest::key(key.as_bytes().to_vec());
        let req = Request::Pending {
            key,
            group,
            consumer,
            refused: self.refused,
        };
        match act.send_to(dest, LiveRequest::Streams(req)) {
            Some(id) => {
                self.pending_read = Some(id);
                true
            }
            None => {
                act.info("The live connection is not open, so the pending entries can't be read.");
                false
            }
        }
    }

    /// After a write on `key`: read it again, with its pending list when one is shown.
    fn reread_after(&mut self, key: &str, act: &mut Act<'_>) {
        if let Some((k, p)) = self.pending_list.as_ref().filter(|(k, _)| k == key) {
            let (k, g, c) = (k.clone(), p.group.clone(), p.consumer.clone());
            self.read_pending(k, g, c, act);
            return;
        }
        if self.in_flight(act) {
            self.reread = true;
        } else {
            self.fetch(act);
        }
    }

    /// The answer to the write in flight.
    fn write_reply(&mut self, sent: Sent, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        let reply = match result {
            Ok(LiveReply::Write(r)) => r,
            Ok(_) => return,
            Err(e) => {
                write_failed(act, &e);
                return;
            }
        };
        match (&sent, reply) {
            (Sent::Claim { consumer, .. }, WriteReply::Claimed { ids, deleted }) => {
                let mut text = format!(
                    "Claimed {} for {}.",
                    form::count(ids.len() as i64, "entry", "entries"),
                    escape(consumer)
                );
                if deleted > 0 {
                    text.push_str(&format!(
                        " {} no longer in the stream left the pending list.",
                        form::count(deleted as i64, "entry", "entries")
                    ));
                }
                act.info(text);
            }
            (Sent::Ack { id, .. }, WriteReply::Int(n)) => act.info(if n > 0 {
                format!("Acknowledged {}.", escape(id))
            } else {
                format!("{} was not pending any more.", escape(id))
            }),
            (Sent::Trim { .. }, WriteReply::Int(n)) => act.info(if n > 0 {
                format!("Trimmed {}.", form::count(n, "entry", "entries"))
            } else {
                "Trimmed 0 entries: with ~ the server removes whole blocks only.".to_string()
            }),
            _ => act.info("Done."),
        }
        self.reread_after(sent.key(), act);
    }

    /// The answer to an `XPENDING` read: the stream's row and its pending list.
    fn pending_reply(&mut self, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        match result {
            Ok(LiveReply::Streams(Reply::Info { streams, refused })) => {
                self.refused = refused;
                for mut f in streams {
                    let pending = f.info.as_mut().and_then(|i| i.pending.take());
                    if let Some(row) = self.rows.iter_mut().find(|r| r.key == f.key) {
                        row.verdicts = f.info.as_ref().map(verdicts).unwrap_or_default();
                        row.info = f.info;
                    }
                    match pending {
                        Some(p) => {
                            if let Some(why) = &p.error {
                                act.error(form::sentence(why));
                            }
                            self.pending_list = Some((f.key, p));
                        }
                        None => {
                            self.pending_list = None;
                            act.info("The key is gone, or no longer a stream.");
                        }
                    }
                }
                self.detail_sel = self.detail_sel.min(self.details().len().saturating_sub(1));
            }
            Ok(_) => {}
            Err(LiveError::Closed) => {}
            Err(e) => act.error(form::sentence(&e.to_string())),
        }
    }

    fn fetch(&mut self, act: &mut Act<'_>) {
        let Some(report) = act.report() else {
            return;
        };
        let keys = stream_keys(report);
        self.found = keys.len();
        self.last_sent = act.now;
        if keys.is_empty() {
            // Nothing to ask the server.
            self.rows.clear();
            self.sel = 0;
            self.loaded = true;
            return;
        }
        let keys: Vec<String> = keys.into_iter().take(MAX_STREAMS).collect();
        self.refused_sent = self.refused;
        self.reread = false;
        self.gathered.clear();
        self.gathered_refused = self.refused;
        self.gathered_error = None;
        self.pending.clear();
        for (dest, keys) in streams_by_node(act.session, &keys) {
            let req = LiveRequest::Streams(Request::Info {
                keys: keys.clone(),
                refused: self.refused,
            });
            if let Some(id) = act.send_to(dest, req) {
                self.pending.push((id, keys));
            }
        }
        self.poll_keys = keys;
    }

    fn in_flight(&mut self, act: &Act<'_>) -> bool {
        if self.pending.iter().any(|(id, _)| act.in_flight(*id)) {
            return true;
        }
        self.pending.clear();
        false
    }

    /// One node's answer to the poll. The poll is applied once every node answered:
    /// a node that failed keeps its streams' rows from the last poll.
    fn take_poll(&mut self, at: usize, result: Result<LiveReply, LiveError>) -> bool {
        let (_, keys) = self.pending.remove(at);
        match result {
            Ok(LiveReply::Streams(Reply::Info { streams, refused })) => {
                let g = &mut self.gathered_refused;
                g.time |= refused.time;
                g.groups |= refused.groups;
                g.consumers |= refused.consumers;
                g.memory |= refused.memory;
                self.gathered.extend(streams);
            }
            Ok(_) => {}
            Err(e) => {
                // The streams of that node keep what the last poll said.
                for k in keys {
                    if let Some(r) = self.rows.iter().find(|r| r.key == k) {
                        self.gathered.push(crate::live::streams::Fetched {
                            key: k,
                            info: r.info.clone(),
                        });
                    }
                }
                let worse = matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_));
                if self.gathered_error.is_none() || worse {
                    self.gathered_error = Some(e);
                }
            }
        }
        self.pending.is_empty()
    }

    fn apply(&mut self, list: Vec<crate::live::streams::Fetched>) {
        let selected = self.selected().map(|r| r.key.clone());
        self.rows = list
            .into_iter()
            .map(|f| {
                let verdicts = f.info.as_ref().map(verdicts).unwrap_or_default();
                StreamRow {
                    key: f.key,
                    info: f.info,
                    verdicts,
                }
            })
            .collect();
        self.sel = selected
            .and_then(|k| self.rows.iter().position(|r| r.key == k))
            .unwrap_or(self.sel)
            .min(self.rows.len().saturating_sub(1));
        self.detail_sel = self.detail_sel.min(self.details().len().saturating_sub(1));
        self.loaded = true;
        self.error = None;
    }
}

/// The key list's row for a stream of this view, for the entries: only the key, the
/// type and the length are used.
fn stream_row(key: &str, info: &StreamInfo) -> KeyRow {
    KeyRow {
        key: key.to_string(),
        key_type: KeyType::Stream,
        ttl: Ttl::Persistent,
        bytes: info.bytes,
        encoding: None,
        length: Some(info.length),
        freq: None,
    }
}

impl ViewState for StreamsState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        if let Some(entries) = self.entries.as_mut().filter(|_| self.entries_open) {
            entries.on_show(act);
            return;
        }
        if self.off.is_none() && !self.in_flight(act) && (!self.paused || !self.loaded) {
            self.fetch(act);
        }
    }

    fn on_tick(&mut self, now: Instant, act: &mut Act<'_>) {
        if let Some(entries) = self.entries.as_mut().filter(|_| self.entries_open) {
            // No XINFO poll while the entries are shown.
            entries.on_tick(now, act);
            return;
        }
        if self.off.is_some() || self.paused || !act.live_ready() || self.in_flight(act) {
            return;
        }
        if self.last_sent.is_none_or(|t| now.duration_since(t) >= POLL) {
            self.fetch(act);
            self.last_sent = Some(now);
        }
    }

    fn refresh(&mut self, act: &mut Act<'_>) {
        if self.entries_open {
            self.reload_entries(act);
            return;
        }
        // `r` tries again after the view turned itself off, and asks again for what was
        // refused (the ACL may have changed).
        self.off = None;
        self.refused = Refused::default();
        if self.in_flight(act) {
            self.reread = true;
        } else {
            self.fetch(act);
        }
    }

    fn input(&self) -> Option<Input> {
        self.form.as_ref().and_then(|f| f.form().input())
    }

    fn on_paste(&mut self, text: &str, _act: &mut Act<'_>) {
        if let Some(f) = self.form.as_mut() {
            f.form_mut().paste(text);
        }
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if self.form.is_some() {
            self.form_key(key, act);
            return true;
        }
        if self.entries_open {
            if key.code == KeyCode::Esc {
                self.entries_open = false;
                return true;
            }
            if let (KeyCode::Char('p'), true) = (key.code, plain(&key)) {
                // Pausing still works; the poll waits for the list anyway.
                self.paused = !self.paused;
                act.info(if self.paused {
                    "Polling paused. It stops while the entries are shown anyway."
                } else {
                    "Polling resumed: the list reads again when you go back to it."
                });
                return true;
            }
            return match self.entries.as_mut() {
                Some(entries) => entries.on_key(key, act),
                None => false,
            };
        }
        match key.code {
            KeyCode::Enter => {
                self.open_entries(act);
                true
            }
            KeyCode::Char('n' | 'f' | 'y') if plain(&key) => {
                act.info("Press enter to list the selected stream's entries first.");
                true
            }
            KeyCode::Char('c') if plain(&key) => {
                self.open_claim(act);
                true
            }
            KeyCode::Char('A') if plain(&key) => {
                self.ack_or_list(act);
                true
            }
            KeyCode::Char('X') if plain(&key) => {
                self.open_trim(act);
                true
            }
            KeyCode::Char('p') if plain(&key) => {
                self.paused = !self.paused;
                if self.paused {
                    act.info("Polling paused. Press p to resume, r to refresh once.");
                } else {
                    act.info("Polling resumed.");
                    // Read at once rather than after the rest of the interval.
                    self.last_sent = None;
                }
                true
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::List => Focus::Detail,
                    Focus::Detail => Focus::List,
                };
                true
            }
            _ => {
                let (cur, len, page) = match self.focus {
                    Focus::List => (self.sel, self.rows.len(), act.page),
                    Focus::Detail => (self.detail_sel, self.details().len(), DETAIL_PAGE),
                };
                let Some(to) = list_move(&key, cur, len, page) else {
                    return false;
                };
                match self.focus {
                    Focus::List => {
                        if to != self.sel {
                            self.detail_sel = 0;
                        }
                        self.sel = to;
                    }
                    Focus::Detail => self.detail_sel = to,
                }
                true
            }
        }
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        if let Some((wid, _)) = &self.write
            && *wid == id
        {
            let (_, sent) = self.write.take().expect("the write in flight");
            self.write_reply(sent, result, act);
            return;
        }
        if self.pending_read == Some(id) {
            self.pending_read = None;
            self.pending_reply(result, act);
            return;
        }
        // Only the answers to the poll in flight; an older one is stale. Any other is
        // a page of entries, or stale too (the entries state checks).
        let Some(at) = self.pending.iter().position(|(p, _)| *p == id) else {
            if let Some(entries) = self.entries.as_mut() {
                entries.on_reply(id, result, act);
            }
            return;
        };
        if !self.take_poll(at, result) {
            return;
        }
        let refused = self.gathered_refused;
        let mut streams = std::mem::take(&mut self.gathered);
        if act.session.is_cluster() {
            // Back in scan order, whichever node answered first.
            let order = |k: &str| self.poll_keys.iter().position(|p| p == k);
            streams.sort_by_key(|f| order(&f.key));
        }
        match self.gathered_error.take() {
            Some(e @ (LiveError::Denied(_) | LiveError::Unsupported(_))) => {
                self.off = Some(off_text(&e));
            }
            other => {
                self.refused = if self.reread {
                    // Keep only what this poll found refused; what it skipped is asked again.
                    let sent = self.refused_sent;
                    Refused {
                        time: refused.time && !sent.time,
                        groups: refused.groups && !sent.groups,
                        consumers: refused.consumers && !sent.consumers,
                        memory: refused.memory && !sent.memory,
                    }
                } else {
                    refused
                };
                if !streams.is_empty() || other.is_none() {
                    self.apply(streams);
                }
                match other {
                    // `App` says the connection closed.
                    Some(LiveError::Closed) | None => {}
                    Some(e) => self.error = Some(e.to_string()),
                }
            }
        }
        if self.reread && self.off.is_none() {
            self.fetch(act);
        }
    }
}

/// The streams `keys` grouped by the node that owns each one's slot, in node order, for
/// one request per node (spec "Cluster live views (P4)", 4 Streams); on a standalone
/// server one group. A stream whose slot no node serves goes to the first group's node,
/// whose answer says so.
pub(crate) fn streams_by_node(
    session: &crate::tui::app::Session,
    keys: &[String],
) -> Vec<(Dest, Vec<String>)> {
    if !session.is_cluster() {
        return vec![(Dest::node(0), keys.to_vec())];
    }
    let mut by: std::collections::BTreeMap<usize, Vec<String>> = Default::default();
    for k in keys {
        let i = session.owner_of(k.as_bytes()).unwrap_or(session.picked);
        by.entry(i).or_default().push(k.clone());
    }
    // Each stream on its own owner: a redirect is followed only for a one-stream
    // request, since the others of the request belong where they are.
    by.into_iter()
        .map(|(i, keys)| {
            let follow = if keys.len() == 1 {
                Follow::Redirects
            } else {
                Follow::Never
            };
            (Dest::node(i).follow(follow), keys)
        })
        .collect()
}

/// Why a view is off, in redis-lens's own words ([`LiveError`]'s texts never hold the
/// server's).
pub(super) fn off_text(e: &LiveError) -> String {
    let why = e.to_string();
    let mut c = why.chars();
    let first = c.next().map(|f| f.to_uppercase().to_string());
    format!("{}{}.", first.unwrap_or_default(), c.as_str())
}

/// A key with no Ctrl or Alt held.
pub(super) fn plain(key: &KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Where a move key takes the selection `cur` of a list of `len` rows: ↑↓ j k one row,
/// PgUp PgDn `page` rows, Home End g G the ends, clamped. `None` for any other key.
pub(super) fn list_move(key: &KeyEvent, cur: usize, len: usize, page: usize) -> Option<usize> {
    if !plain(key) {
        return None;
    }
    let last = len.saturating_sub(1);
    Some(match key.code {
        KeyCode::Up | KeyCode::Char('k') => cur.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => cur.saturating_add(1).min(last),
        KeyCode::PageUp => cur.saturating_sub(page),
        KeyCode::PageDown => cur.saturating_add(page).min(last),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::testing::{self, key};
    use super::*;
    use crate::live::streams::{Fetched, Lag};
    use crate::tui::app::{App, Effect, Msg, Screen};
    use crossterm::event::KeyCode;

    /// A poll's answer with nothing refused.
    fn poll_reply(streams: Vec<crate::live::streams::Fetched>) -> Reply {
        Reply::Info {
            streams,
            refused: Refused::default(),
        }
    }

    fn stream_pattern(pattern: &str, examples: &[&str]) -> PatternStats {
        let mut p = crate::tui::fixtures::report(true).patterns[0].clone();
        p.pattern = pattern.into();
        p.key_type = Some(KeyType::Stream);
        p.examples = examples
            .iter()
            .map(|k| crate::model::KeySample {
                key: (*k).into(),
                key_type: KeyType::Stream,
                bytes: Some(1024),
                ttl: crate::model::Ttl::Persistent,
                lfu_freq: None,
                node: None,
            })
            .collect();
        p
    }

    /// The fixture scan plus the streams `orders`, `events:{id}` (two keys) and a
    /// non-stream pattern with a stream among its examples.
    fn with_streams(app: &mut App) {
        let report = app
            .session
            .as_mut()
            .and_then(|s| s.report.as_mut())
            .expect("a report");
        report.patterns.push(stream_pattern("orders", &[]));
        report
            .patterns
            .push(stream_pattern("events:{id}", &["events:1", "events:2"]));
    }

    fn info(key: &str, length: u64, groups: Option<Vec<GroupInfo>>) -> StreamInfo {
        StreamInfo {
            key: key.into(),
            length,
            bytes: Some(2 * 1024 * 1024),
            first_id: Some("1700000000000-0".into()),
            last_id: "1700000900000-0".into(),
            oldest_age_ms: Some(3 * 60 * 60 * 1000),
            entries_added: Some(length + 4),
            groups,
            notes: Vec::new(),
            pending: None,
        }
    }

    fn consumer(name: &str, pending: u64, idle_ms: u64) -> ConsumerInfo {
        ConsumerInfo {
            name: name.into(),
            pending,
            idle_ms,
            inactive_ms: Some(idle_ms),
        }
    }

    fn group(name: &str, pending: u64, lag: Lag, readers: Vec<ConsumerInfo>) -> GroupInfo {
        GroupInfo {
            name: name.into(),
            consumers: readers.len() as u64,
            pending,
            last_delivered_id: "1700000800000-0".into(),
            lag,
            readers: Some(readers),
        }
    }

    pub(super) fn fetched() -> Vec<Fetched> {
        vec![
            Fetched {
                key: "orders".into(),
                info: Some(info(
                    "orders",
                    120_000,
                    Some(vec![
                        group(
                            "billing",
                            3,
                            Lag::Known(2_500),
                            vec![consumer("b-1", 3, 45 * 60 * 1000), consumer("b-2", 0, 1500)],
                        ),
                        group("audit", 0, Lag::Known(0), vec![consumer("a-1", 0, 20)]),
                    ]),
                )),
            },
            Fetched {
                key: "events:1".into(),
                info: Some(info("events:1", 42, Some(vec![]))),
            },
            Fetched {
                key: "events:2".into(),
                info: None,
            },
        ]
    }

    fn at(base: Instant, ms: u64) -> Msg {
        Msg::TickAt(base + Duration::from_millis(ms))
    }

    /// An app on Streams with the live connection ready and the streams above found by
    /// the scan, and the request on show answered with [`fetched`].
    fn loaded() -> (App, Instant) {
        let t0 = Instant::now();
        let mut app = testing::scanned();
        with_streams(&mut app);
        testing::live_ready(&mut app);
        app.apply(Msg::TickAt(t0));
        let effects = testing::show(&mut app, Screen::Streams);
        let reqs = testing::requests(&effects);
        let [(id, LiveRequest::Streams(Request::Info { .. }))] = &reqs[..] else {
            panic!("expected one Info request, got {reqs:?}");
        };
        testing::reply(&mut app, *id, Ok(LiveReply::Streams(poll_reply(fetched()))));
        (app, t0)
    }

    fn answer(app: &mut App, id: ReqId) {
        testing::reply(app, id, Ok(LiveReply::Streams(poll_reply(fetched()))));
    }

    /// Ticks to `ms` after `t0`; the request that sent, if any.
    fn tick(app: &mut App, t0: Instant, ms: u64) -> Option<ReqId> {
        let reqs = testing::requests(&app.apply(at(t0, ms)));
        assert!(reqs.len() <= 1, "{reqs:?}");
        reqs.first().map(|r| r.0)
    }

    #[test]
    fn finds_the_streams_of_the_scan() {
        let mut app = testing::scanned();
        with_streams(&mut app);
        let report = app.session.as_ref().unwrap().report.as_ref().unwrap();
        // `orders` is a literal key; `events:{id}` is read through its examples.
        assert_eq!(stream_keys(report), ["orders", "events:1", "events:2"]);
        // A pattern with a placeholder and no examples names no key.
        let mut p = stream_pattern("jobs:{id}", &[]);
        p.examples.clear();
        assert!(pattern_streams(&p).is_empty());
        // A stream among the examples of a mixed pattern counts.
        let mut mixed = stream_pattern("m:{id}", &["m:1"]);
        mixed.key_type = None;
        assert_eq!(pattern_streams(&mixed), ["m:1"]);
    }

    #[test]
    fn on_show_asks_for_the_streams_and_nothing_else() {
        let mut app = testing::scanned();
        with_streams(&mut app);
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::Streams);
        let reqs = testing::requests(&effects);
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].1,
            LiveRequest::Streams(Request::Info {
                keys: vec!["orders".into(), "events:1".into(), "events:2".into()],
                refused: Refused::default(),
            })
        );
    }

    #[test]
    fn nothing_is_asked_when_the_scan_found_no_streams() {
        let (mut app, effects) = testing::app_on(Screen::Streams);
        assert!(testing::requests(&effects).is_empty());
        assert!(app.views.streams.loaded);
        let t0 = Instant::now();
        app.apply(Msg::TickAt(t0));
        assert_eq!(tick(&mut app, t0, 6_000), None);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn polls_every_five_seconds_while_shown() {
        let (mut app, t0) = loaded();
        assert_eq!(tick(&mut app, t0, 4_900), None, "before 5 s");
        let id = tick(&mut app, t0, 5_000).expect("a poll at 5 s");
        // One request at a time: a slow answer is not polled again on top of it.
        assert_eq!(tick(&mut app, t0, 11_000), None);
        answer(&mut app, id);
        // The next one is due 5 s after the last was sent, and it is long past.
        assert!(tick(&mut app, t0, 11_100).is_some());
    }

    #[test]
    fn p_pauses_and_resumes_polling() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.views.streams.paused);
        assert_eq!(tick(&mut app, t0, 60_000), None);
        // `r` still reads once.
        let effects = app.handle_key(key(KeyCode::Char('r')));
        let [(id, _)] = testing::requests(&effects)[..] else {
            panic!("expected one request");
        };
        answer(&mut app, id);
        // Resuming reads at once.
        app.handle_key(key(KeyCode::Char('p')));
        assert!(!app.views.streams.paused);
        assert!(tick(&mut app, t0, 60_100).is_some());
    }

    #[test]
    fn r_during_a_poll_asks_again_for_what_was_refused() {
        let (mut app, t0) = loaded();
        // The next poll is told that XINFO GROUPS was refused.
        app.views.streams.refused.groups = true;
        let id = tick(&mut app, t0, 5_100).expect("poll");
        // `r` while it is in flight sends nothing yet.
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert!(testing::requests(&effects).is_empty());
        // That poll skipped GROUPS, so its answer still says refused.
        let effects = testing::reply(
            &mut app,
            id,
            Ok(LiveReply::Streams(Reply::Info {
                streams: fetched(),
                refused: Refused {
                    groups: true,
                    memory: true,
                    ..Refused::default()
                },
            })),
        );
        // One more read follows at once, asking for GROUPS again; MEMORY USAGE, newly
        // refused by that poll, stays skipped.
        let reqs = testing::requests(&effects);
        let [(_, LiveRequest::Streams(Request::Info { refused, .. }))] = &reqs[..] else {
            panic!("expected one Info request, got {reqs:?}");
        };
        assert_eq!(
            *refused,
            Refused {
                memory: true,
                ..Refused::default()
            }
        );
    }

    #[test]
    fn does_not_poll_while_hidden() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('0')));
        assert_eq!(tick(&mut app, t0, 60_000), None);
        // Back on screen it reads at once.
        let effects = app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(testing::requests(&effects).len(), 1);
    }

    #[test]
    fn a_reply_fills_the_list_with_verdicts() {
        let (app, _) = loaded();
        let s = &app.views.streams;
        assert_eq!(s.rows.len(), 3);
        // billing's consumer holds entries and has been idle 45 min; lag 2,500.
        assert_eq!(s.rows[0].verdicts, [Verdict::Stuck, Verdict::Lagging]);
        assert_eq!(s.rows[1].verdicts, [Verdict::NoReaders]);
        assert!(s.rows[2].info.is_none());
        assert!(s.rows[2].verdicts.is_empty());
    }

    #[test]
    fn keys_move_the_list_and_tab_moves_to_the_details() {
        let (mut app, _) = loaded();
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.views.streams.sel, 1);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.views.streams.sel, 2);
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.views.streams.sel, 0);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.views.streams.focus, Focus::Detail);
        // billing, b-1, b-2, audit, a-1.
        assert_eq!(app.views.streams.details().len(), 5);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.views.streams.detail_sel, 4);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.views.streams.detail_sel, 4, "clamped");
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.views.streams.focus, Focus::List);
    }

    #[test]
    fn the_selection_follows_its_stream_across_polls() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('j')));
        let mut list = fetched();
        list.swap(0, 1);
        let id = tick(&mut app, t0, 10_000).expect("a poll was sent");
        testing::reply(&mut app, id, Ok(LiveReply::Streams(poll_reply(list))));
        assert_eq!(app.views.streams.selected().unwrap().key, "events:1");
        assert_eq!(app.views.streams.sel, 0);
    }

    #[test]
    fn a_refused_xinfo_turns_the_view_off_with_fixed_text() {
        let mut app = testing::scanned();
        with_streams(&mut app);
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::Streams);
        let (id, _) = testing::requests(&effects).remove(0);
        testing::reply(&mut app, id, Err(LiveError::Denied("XINFO STREAM".into())));
        let off = app.views.streams.off.clone().unwrap();
        assert_eq!(off, "The server refused XINFO STREAM for this user.");
        // Off: no more polls.
        let t0 = Instant::now();
        app.apply(Msg::TickAt(t0));
        assert_eq!(tick(&mut app, t0, 60_000), None);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
        // `r` tries again.
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(testing::requests(&effects).len(), 1);
        assert!(app.views.streams.off.is_none());
    }

    #[test]
    fn a_failed_poll_keeps_the_rows_and_says_so() {
        let (mut app, t0) = loaded();
        let id = tick(&mut app, t0, 10_000).expect("a poll");
        testing::reply(
            &mut app,
            id,
            Err(LiveError::Failed("XINFO STREAM failed".into())),
        );
        assert_eq!(app.views.streams.rows.len(), 3);
        assert_eq!(
            app.views.streams.error.as_deref(),
            Some("XINFO STREAM failed")
        );
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn a_probe_denied_screen_is_off_without_the_view_being_called() {
        let mut app = testing::scanned();
        // `perms_probes_off` is the only fixture with probes off; use it for XINFO too.
        let mut perms = crate::tui::fixtures::perms_with_probes();
        for c in &mut perms.checks {
            if c.label == crate::tui::app::views::PROBE_XINFO {
                c.status = crate::model::PermStatus::Denied(
                    "NOPERM lens-ro has no permissions to run xinfo".into(),
                );
            }
        }
        let (id, scan) = {
            let s = app.session.as_ref().unwrap();
            (s.id, s.scan)
        };
        let _ = (id, scan);
        let entry = app
            .entries
            .iter_mut()
            .find(|e| Some(e.id) == app.session.as_ref().map(|s| s.id))
            .unwrap();
        entry.perms = Some(perms);
        testing::live_ready(&mut app);
        let effects: Vec<Effect> = testing::show(&mut app, Screen::Streams);
        assert!(testing::requests(&effects).is_empty());
        let screen = format!("{}", testing::render(&app, 80, 24).backend());
        // Wrapped in the list pane.
        assert!(
            screen.contains("The server refused XINFO for this"),
            "{screen}"
        );
        assert!(screen.contains("user, so this view is off."), "{screen}");
        assert!(!screen.contains("lens-ro"), "{screen}");
    }

    #[test]
    fn snapshots_80x24_and_120x36() {
        let (mut app, _) = loaded();
        insta::assert_snapshot!("list_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("list_120x36", testing::render(&app, 120, 36).backend());
        // Details focused, on the second group's consumer.
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::End));
        insta::assert_snapshot!("details_80x24", testing::render(&app, 80, 24).backend());
        app.handle_key(key(KeyCode::Char('p')));
        insta::assert_snapshot!("paused_120x36", testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn nothing_the_view_shows_holds_an_entry_value() {
        let (app, _) = loaded();
        let shown = format!("{:?}", app.views.streams);
        assert!(!shown.contains("hunter2"));
    }

    // ---------------------------------------------------------------------------------
    // Entries (phase 2)
    // ---------------------------------------------------------------------------------

    use crate::live::values::{
        At, Entry, Items, Kind, Page, Reply as ValueReply, Request as ValueRequest,
    };
    use crate::value::Raw;

    const MARK: &str = "s3cr3t-d0c";

    fn ch(c: char) -> crossterm::event::KeyEvent {
        key(KeyCode::Char(c))
    }

    fn value_requests(effects: &[Effect]) -> Vec<(ReqId, ValueRequest)> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(id, r)| match r {
                LiveRequest::Value(v) => Some((id, v)),
                _ => None,
            })
            .collect()
    }

    fn entry(id: &str, fields: &[(&str, &str)]) -> Entry {
        Entry {
            id: id.into(),
            fields: fields
                .iter()
                .map(|(k, v)| (Raw::from(*k), Raw::from(*v)))
                .collect(),
        }
    }

    fn entries_page(
        entries: Vec<Entry>,
        next: Option<At>,
        first: bool,
    ) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Value(ValueReply::Page(Page {
            type_name: first.then(|| "stream".to_string()),
            items: Items::Stream(entries),
            next,
            refused: Vec::new(),
        })))
    }

    fn orders_entries() -> Vec<Entry> {
        vec![
            entry(
                "1700000900000-0",
                &[("order", r#"{"id":44812,"state":"paid"}"#), ("by", "web")],
            ),
            entry(
                "1700000800000-0",
                &[("order", r#"{"id":44811,"state":"new"}"#)],
            ),
            entry("1700000700000-0", &[("note", "plain text")]),
        ]
    }

    fn notice(app: &App) -> String {
        app.notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    fn text(app: &App, w: u16, h: u16) -> String {
        let t = testing::render(app, w, h);
        let b = t.backend().buffer();
        let mut out = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                out.push_str(b[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// The loaded list, `enter` on `orders`, and the first page answered.
    fn entries_open() -> (App, Instant) {
        let (mut app, t0) = loaded();
        let effects = app.handle_key(key(KeyCode::Enter));
        let [(id, req)] = &value_requests(&effects)[..] else {
            panic!("expected one value request, got {effects:?}");
        };
        assert_eq!(
            *req,
            ValueRequest {
                key: "orders".into(),
                kind: Kind::Stream,
                at: None,
                field_ttls: true,
                size: Some(2 * 1024 * 1024),
            }
        );
        testing::reply(
            &mut app,
            *id,
            entries_page(
                orders_entries(),
                Some(At::Before("1700000700000-0".into())),
                true,
            ),
        );
        (app, t0)
    }

    #[test]
    fn enter_lists_the_entries_newest_first_and_n_loads_more() {
        let (mut app, t0) = entries_open();
        let s = &app.views.streams;
        assert!(s.entries_open);
        assert_eq!(s.shown_entries().unwrap().loaded(), 3);
        // n reads the next 100, before the last ID read.
        let effects = app.handle_key(ch('n'));
        let [(id, req)] = &value_requests(&effects)[..] else {
            panic!("{effects:?}");
        };
        assert_eq!(req.at, Some(At::Before("1700000700000-0".into())));
        assert_eq!(req.kind, Kind::Stream);
        testing::reply(
            &mut app,
            *id,
            entries_page(vec![entry("1700000600000-0", &[("a", "b")])], None, false),
        );
        assert_eq!(app.views.streams.shown_entries().unwrap().loaded(), 4);
        app.handle_key(ch('n'));
        assert_eq!(notice(&app), "All 4 items are loaded.");
        // No XINFO poll while the entries are shown.
        assert_eq!(tick(&mut app, t0, 60_000), None);
        // esc goes back to the list, which polls again at once (a poll is long due).
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(!app.views.streams.entries_open);
        let id = tick(&mut app, t0, 60_100).expect("a poll back on the list");
        answer(&mut app, id);
        // Opening the same stream again keeps what was loaded.
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(value_requests(&effects).is_empty());
        assert_eq!(app.views.streams.shown_entries().unwrap().loaded(), 4);
        // Another starts afresh.
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [(_, req)] = &value_requests(&effects)[..] else {
            panic!("{effects:?}");
        };
        assert_eq!((req.key.as_str(), req.at.clone()), ("events:1", None));
    }

    #[test]
    fn entries_stay_open_across_views_and_r_reads_them_again() {
        let (mut app, _) = entries_open();
        app.handle_key(ch('0'));
        let effects = app.handle_key(ch('2'));
        // Shown again: nothing asked, no XINFO either.
        assert!(testing::requests(&effects).is_empty(), "{effects:?}");
        assert!(app.views.streams.entries_open);
        // r reads the entries again from the newest.
        let effects = app.handle_key(ch('r'));
        let reqs = testing::requests(&effects);
        let [(_, LiveRequest::Value(req))] = &reqs[..] else {
            panic!("{reqs:?}");
        };
        assert_eq!((req.key.as_str(), req.at.clone()), ("orders", None));
        assert_eq!(app.views.streams.shown_entries().unwrap().loaded(), 0);
    }

    #[test]
    fn a_gone_stream_has_no_entries_to_list() {
        let (mut app, _) = loaded();
        app.handle_key(ch('G'));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(testing::requests(&effects).is_empty());
        assert!(!app.views.streams.entries_open);
        assert_eq!(
            notice(&app),
            "The key is gone, or no longer a stream. Rescan to update the list."
        );
        // n, f and y on the list say what to do first.
        app.handle_key(ch('n'));
        assert_eq!(
            notice(&app),
            "Press enter to list the selected stream's entries first."
        );
    }

    #[test]
    fn f_and_y_in_the_entries_work_as_on_the_key_detail() {
        let (mut app, _) = entries_open();
        app.handle_key(ch('f'));
        let f = app.views.streams.shown_entries().unwrap().format;
        // The first field of the newest entry is JSON: f moves on to MessagePack.
        assert_eq!(f, Some(crate::value::Format::MsgPack));
        assert_eq!(notice(&app), "Shown as MessagePack.");
        let all = text(&app, 120, 36);
        assert!(all.contains("shown as MessagePack"), "{all}");
        // A stream entry has several fields: y does not copy it (as on the key detail).
        let effects = app.handle_key(ch('y'));
        assert!(!effects.iter().any(|e| matches!(e, Effect::Copy(_))));
        assert_eq!(
            notice(&app),
            "A stream entry has several fields, so y does not copy it."
        );
        // → moves to the value, which scrolls.
        app.handle_key(key(KeyCode::Right));
        assert_eq!(
            app.views.streams.shown_entries().unwrap().focus,
            crate::tui::app::views::value::Focus::Value
        );
        // p still pauses, and esc goes back: to the items, then out of the entries.
        app.handle_key(ch('p'));
        assert!(app.views.streams.paused);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(
            app.views.streams.shown_entries().unwrap().focus,
            crate::tui::app::views::value::Focus::Items
        );
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.views.streams.entries_open);
    }

    #[test]
    fn entries_snapshots() {
        let (mut app, _) = entries_open();
        insta::assert_snapshot!("entries_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("entries_120x36", testing::render(&app, 120, 36).backend());
        app.handle_key(ch('j'));
        insta::assert_snapshot!(
            "entries_second_80x24",
            testing::render(&app, 80, 24).backend()
        );
    }

    #[test]
    fn escapes_in_entries_are_drawn_never_the_raw_bytes() {
        let (mut app, _) = loaded();
        let effects = app.handle_key(key(KeyCode::Enter));
        let (id, _) = value_requests(&effects).remove(0);
        let e = Entry {
            id: "1700000900000-0".into(),
            fields: vec![
                (
                    Raw::from("na\x1b[2Jme"),
                    Raw::from("ok\x1b[2J\u{9b}31m done"),
                ),
                (
                    Raw::from("bin"),
                    Raw::new(vec![0x9b, 0x1b, b'[', b'2', b'J', 0xff]),
                ),
            ],
        };
        testing::reply(&mut app, id, entries_page(vec![e], None, true));
        for (w, h) in [(80, 24), (120, 36)] {
            let all = text(&app, w, h);
            assert!(all.contains("\\x1b[2J"), "{all}");
            assert!(!all.contains('\x1b') && !all.contains('\u{9b}'), "{all}");
        }
        insta::assert_snapshot!(
            "entries_escapes_80x24",
            testing::render(&app, 80, 24).backend()
        );
    }

    /// Rule 1: an entry's values show on screen and nowhere else.
    #[test]
    fn entry_values_never_reach_debug_notices_or_snapshot_files() {
        let (mut app, _) = loaded();
        let effects = app.handle_key(key(KeyCode::Enter));
        let (id, _) = value_requests(&effects).remove(0);
        let reply = entries_page(
            vec![entry("1700000900000-0", &[(MARK, MARK), ("b", MARK)])],
            None,
            true,
        );
        let msg = format!(
            "{:?}",
            Msg::Live {
                id: app.session.as_ref().unwrap().id,
                scan: app.session.as_ref().unwrap().scan,
                req_id: id,
                result: reply.clone(),
            }
        );
        assert!(!crate::value::leaks(&msg, MARK), "{msg}");
        testing::reply(&mut app, id, reply);
        assert!(text(&app, 120, 36).contains(MARK));
        let mut effects = Vec::new();
        for k in ['f', 'y', 'j', 'n'] {
            effects.extend(app.handle_key(ch(k)));
            assert!(
                !crate::value::leaks(&notice(&app), MARK),
                "{}",
                notice(&app)
            );
        }
        let debug = format!("{effects:?} {:?} {:?}", app.views, app.session);
        assert!(!crate::value::leaks(&debug, MARK), "{debug}");
        // A snapshot file, saved from Patterns.
        app.handle_key(ch('3'));
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(report)] = &effects[..] else {
            panic!("{effects:?}");
        };
        assert!(!crate::value::leaks(
            &serde_json::to_string(report).unwrap(),
            MARK
        ));
    }

    // -----------------------------------------------------------------------
    // Writes (P3)
    // -----------------------------------------------------------------------

    use super::form::testing::{READ_ONLY, answer_anything, press, set_profile, type_text, writes};
    use crate::live::write::Reply as WriteReply;
    use crate::model::Env;
    use crate::tui::app::confirm::Expect;

    fn pending_entry(id: &str, consumer: &str, idle_ms: u64, deliveries: u64) -> PendingEntry {
        PendingEntry {
            id: id.into(),
            consumer: consumer.into(),
            idle_ms,
            deliveries,
        }
    }

    /// The answer to a `Request::Pending` for `orders`, group `billing`, consumer `b-1`.
    fn pending_reply() -> Result<LiveReply, LiveError> {
        let mut f = fetched().remove(0);
        f.info.as_mut().unwrap().pending = Some(Pending {
            group: "billing".into(),
            consumer: Some("b-1".into()),
            entries: vec![
                pending_entry("1700000600000-0", "b-1", 46 * 60 * 1000, 3),
                pending_entry("1700000610000-0", "b-1", 45 * 60 * 1000, 1),
                pending_entry("1700000620000-0", "b-1", 45 * 60 * 1000, 1),
            ],
            error: None,
        });
        Ok(LiveReply::Streams(Reply::Info {
            streams: vec![f],
            refused: Refused::default(),
        }))
    }

    /// Loaded, writes as `env`/`read_only`, the details focused on consumer `b-1`.
    fn on_consumer(env: Env, read_only: Option<bool>) -> App {
        let (mut app, _) = loaded();
        set_profile(&mut app, env, read_only);
        press(&mut app, KeyCode::Right);
        app.handle_key(ch('j'));
        assert!(matches!(
            app.views.streams.details()[app.views.streams.detail_sel],
            DetailRow::Consumer(_, c) if c.name == "b-1"
        ));
        app
    }

    /// [`on_consumer`] with its pending entries listed and the first one selected.
    fn on_pending(env: Env, read_only: Option<bool>) -> App {
        let mut app = on_consumer(env, read_only);
        let e = app.handle_key(ch('A'));
        let reqs = testing::requests(&e);
        let [(id, LiveRequest::Streams(req))] = &reqs[..] else {
            panic!("{reqs:?}");
        };
        assert_eq!(
            *req,
            Request::Pending {
                key: "orders".into(),
                group: "billing".into(),
                consumer: Some("b-1".into()),
                refused: Refused::default(),
            }
        );
        testing::reply(&mut app, *id, pending_reply());
        app.handle_key(ch('j'));
        app
    }

    fn streams_requests(effects: &[Effect]) -> Vec<Request> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(_, r)| match r {
                LiveRequest::Streams(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_lists_the_pending_entries_and_acks_the_selected_one() {
        let mut app = on_pending(Env::Local, None);
        let s = &app.views.streams;
        assert_eq!(s.details().len(), 8);
        assert!(matches!(
            s.details()[s.detail_sel],
            DetailRow::Pending(g, e) if g.name == "billing" && e.id == "1700000600000-0"
        ));
        insta::assert_snapshot!("pending_80x24", testing::render(&app, 80, 24).backend());
        let e = app.handle_key(ch('A'));
        assert!(app.confirm.is_none());
        assert_eq!(
            writes(&e),
            vec![WriteOp::XAck {
                key: "orders".into(),
                group: "billing".into(),
                id: "1700000600000-0".into(),
            }]
        );
        let (id, _) = testing::requests(&e)[0].clone();
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(1))));
        assert_eq!(notice(&app), "Acknowledged 1700000600000-0.");
        // The stream and its pending list are read again.
        assert!(matches!(
            streams_requests(&e)[..],
            [Request::Pending { .. }]
        ));

        let mut app = on_pending(Env::Prod, Some(false));
        app.handle_key(ch('A'));
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        assert_eq!(writes(&app.handle_key(ch('y'))).len(), 1);
    }

    #[test]
    fn c_claims_pending_entries_for_another_consumer() {
        let mut app = on_consumer(Env::Local, None);
        app.handle_key(ch('c'));
        let f = app.views.streams.form.as_ref().expect("the form").form();
        assert_eq!(f.value("to consumer"), "b-2");
        assert_eq!(f.value("min idle ms"), "2700000");
        assert_eq!(f.value("count"), "100");
        insta::assert_snapshot!("claim_form_80x24", testing::render(&app, 80, 24).backend());
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        let e = press(&mut app, KeyCode::Enter);
        assert!(app.confirm.is_none());
        assert_eq!(
            writes(&e),
            vec![WriteOp::XAutoClaim {
                key: "orders".into(),
                group: "billing".into(),
                consumer: "b-2".into(),
                min_idle_ms: 2_700_000,
                count: 100,
            }]
        );
        let (id, _) = testing::requests(&e)[0].clone();
        let e = testing::reply(
            &mut app,
            id,
            Ok(LiveReply::Write(WriteReply::Claimed {
                ids: vec!["1-0".into(), "2-0".into()],
                deleted: 1,
            })),
        );
        assert_eq!(
            notice(&app),
            "Claimed 2 entries for b-2. 1 entry no longer in the stream left the pending list."
        );
        assert!(matches!(streams_requests(&e)[..], [Request::Info { .. }]));

        // On the list, c says where to go.
        press(&mut app, KeyCode::Left);
        app.handle_key(ch('c'));
        assert!(app.views.streams.form.is_none());
        assert!(notice(&app).contains("Press tab"), "{}", notice(&app));
    }

    #[test]
    fn the_claim_and_trim_forms_check_what_is_typed() {
        let claim = |to: &str, idle: &str, count: &str| {
            let f = Form::new(
                "t",
                vec![],
                vec![
                    Field::text("to consumer", "").with(to),
                    Field::text("min idle ms", "").with(idle),
                    Field::text("count", "").with(count),
                ],
            );
            claim_op("s", "g", &f)
        };
        assert!(claim("", "0", "1").is_err());
        assert!(claim("c", "x", "1").is_err());
        assert!(claim("c", "0", "0").is_err());
        assert!(claim("c", "0", "1").is_ok());
        let trim = |by: &str, value: &str| {
            let mut f = Form::new(
                "t",
                vec![],
                vec![
                    Field::choice("trim by", &["MAXLEN", "MINID"], ""),
                    Field::text("value", "").with(value),
                ],
            );
            f.fields[0].text = by.into();
            trim_op("s", &f).map(|op| op.shown())
        };
        assert_eq!(trim("MAXLEN", "1000"), Ok("XTRIM s MAXLEN ~ 1000".into()));
        assert!(trim("MAXLEN", "-1").is_err());
        assert_eq!(
            trim("MINID", "1700000000000-5"),
            Ok("XTRIM s MINID ~ 1700000000000-5".into())
        );
        assert_eq!(trim("MINID", "17"), Ok("XTRIM s MINID ~ 17".into()));
        for bad in ["", "abc", "1-", "-1", "1-2-3"] {
            assert!(trim("MINID", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn x_trims_after_asking_and_types_the_key_on_prod() {
        let (mut app, _) = loaded();
        set_profile(&mut app, Env::Local, None);
        app.handle_key(ch('X'));
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "1000");
        insta::assert_snapshot!("trim_form_80x24", testing::render(&app, 80, 24).backend());
        let e = press(&mut app, KeyCode::Enter);
        assert!(writes(&e).is_empty());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        let e = app.handle_key(ch('y'));
        assert_eq!(
            writes(&e),
            vec![WriteOp::XTrim {
                key: "orders".into(),
                trim: Trim::MaxLen(1000),
            }]
        );
        let (id, _) = testing::requests(&e)[0].clone();
        testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(4))));
        assert_eq!(notice(&app), "Trimmed 4 entries.");

        set_profile(&mut app, Env::Prod, Some(false));
        app.handle_key(ch('X'));
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "1700000000000");
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("orders".into())
        );
        type_text(&mut app, "orders");
        let e = press(&mut app, KeyCode::Enter);
        assert_eq!(
            writes(&e),
            vec![WriteOp::XTrim {
                key: "orders".into(),
                trim: Trim::MinId("1700000000000".into()),
            }]
        );
    }

    #[test]
    fn no_stream_write_reaches_a_read_only_profile() {
        for (env, ro) in READ_ONLY {
            // c and X: off before the form opens.
            for k in ['c', 'X'] {
                let mut app = on_consumer(env, ro);
                let mut e = app.handle_key(ch(k));
                assert!(app.views.streams.form.is_none() && app.confirm.is_none());
                assert!(notice(&app).contains("read-only"), "{}", notice(&app));
                e.extend(answer_anything(&mut app));
                assert!(writes(&e).is_empty(), "{k} {env:?} {ro:?}");
            }
            // A lists the pending entries (a read), and acknowledges none.
            let mut app = on_pending(env, ro);
            let mut e = app.handle_key(ch('A'));
            assert!(app.confirm.is_none());
            assert!(notice(&app).contains("read-only"), "{}", notice(&app));
            e.extend(answer_anything(&mut app));
            assert!(writes(&e).is_empty(), "A {env:?} {ro:?}");
        }
    }

    #[test]
    fn the_form_takes_every_key_and_esc_closes_it() {
        let (mut app, _) = loaded();
        set_profile(&mut app, Env::Local, None);
        app.handle_key(ch('X'));
        press(&mut app, KeyCode::Tab);
        // Digits and q are text here, not views or quitting.
        type_text(&mut app, "12q");
        assert_eq!(app.screen(), Screen::Streams);
        let f = app.views.streams.form.as_ref().unwrap().form();
        assert_eq!(f.value("value"), "12q");
        assert!(format!("{:?}", app.views.streams).contains("value: <3 chars>"));
        let e = press(&mut app, KeyCode::Esc);
        assert!(writes(&e).is_empty() && app.views.streams.form.is_none());
    }

    #[test]
    fn before_an_analysis_the_list_says_so_and_r_analyzes() {
        use crate::tui::fixtures;
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, scan, .. }] = effects[..] else {
            panic!("unexpected effects {effects:?}");
        };
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(fixtures::server()),
            perms: fixtures::perms_with_probes(),
            target: crate::conn::ScanTarget::Primary,
        });
        app.apply(Msg::LiveReady { id, scan });
        let effects = testing::show(&mut app, Screen::Streams);
        // Nothing is asked for the streams: they come from an analysis. (Going
        // through Browse › Keys starts its key names.)
        assert!(
            testing::requests(&effects)
                .iter()
                .all(|(_, r)| matches!(r, crate::live::LiveRequest::Names(_))),
            "{effects:?}"
        );
        let screen = format!("{}", testing::render(&app, 80, 24).backend());
        assert!(
            screen.contains("Streams are found by an analysis"),
            "{screen}"
        );
        assert!(screen.contains("r analyzes."), "{screen}");
        assert!(app.analyze.is_none());
        app.handle_key(key(KeyCode::Char('r')));
        assert!(app.analyze.is_some(), "r opens the analyze dialog");
        insta::assert_snapshot!(
            "streams_before_an_analysis_120x36",
            testing::render(
                &{
                    app.handle_key(key(KeyCode::Esc));
                    app
                },
                120,
                36
            )
            .backend()
        );
    }
}
