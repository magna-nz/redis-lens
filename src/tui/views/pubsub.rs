//! State of the Pub/Sub view (spec "5 Pub/Sub"). Owner: wave B. Drawn by `ui/pubsub.rs`;
//! its live requests are in `src/live/pubsub.rs`.
//!
//! - **Counts:** the active channels, their subscriber counts and the pattern count,
//!   every 5 s while the view is shown (`p` pauses). They read no payload.
//! - **The live tail:** `enter` subscribes to the selected channel, `s` to a channel or
//!   pattern typed in. The subscription runs on a connection of its own in the runner
//!   ([`Act::subscribe`]); its messages arrive through [`PubSubState::tail_batch`],
//!   also while another view is shown. The last [`TAIL_KEPT`] stay in memory, never
//!   saved. `space` pauses what is shown, `c` clears, `/` filters, `esc` clears the
//!   filter or stops the tail.
//! - **Publish (P3):** `P` opens a form for a channel (the selected one to start with)
//!   and a message, and sends `PUBLISH` through [`Act::write`], the one gate. The
//!   message is a value: it is drawn in the form and nowhere else. On a shard channel
//!   it sends `SPUBLISH`, to the channel's slot owner.
//! - **Shard channels (P4):** a second list under the classic one, from every node of a
//!   cluster (one on a standalone server) on each counts poll (Redis 7.0 and later,
//!   Valkey). `S` subscribes to the selected one, or asks for one, with `SSUBSCRIBE` on
//!   its slot's owner. On a cluster the classic counts are the picked node's own.
//!
//! See [`super`] for the interface `App` calls.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};

use super::streams::form::{self, Field, Form, Step, write_failed, write_off};
use super::streams::{list_move, off_text, plain};
use super::{Act, Input, ViewState};
use crate::live::pubsub::{
    Counts, Reply, Request, ShardCounts, Subscription, TAIL_KEPT, TailMessage,
};
use crate::live::write::Reply as WriteReply;
use crate::live::{Dest, Follow, LiveError, LiveReply, LiveRequest, ReqId};
use crate::value::Raw;
use crate::write::WriteOp;

/// Why the shard channels are left out when the server lacks `PUBSUB SHARDCHANNELS`.
pub const SHARDS_UNSUPPORTED: &str =
    "The server does not support PUBSUB SHARDCHANNELS, so shard channels are left out.";
/// Why they are left out when the server refused it for this user.
pub const SHARDS_DENIED: &str =
    "The server refused PUBSUB SHARDCHANNELS for this user, so shard channels are left out.";

/// How often the counts are polled while the view is shown (spec "Shared command budget").
pub const POLL: Duration = Duration::from_secs(5);
/// The message rate is counted over this window.
pub const RATE_WINDOW: Duration = Duration::from_secs(5);
pub use crate::live::pubsub::{PAYLOAD_KEPT, PREVIEW_CHARS};
/// The longest text the `s` and `/` inputs take.
const MAX_INPUT: usize = 256;
/// The `S` prompt's hint once it was given a pattern
/// ([`crate::live::pubsub::SHARD_PATTERN`]), until the next key.
const SHARD_PATTERN_HINT: &str =
    "   a pattern can't be shard-subscribed: type a channel name · esc to cancel";

/// The channels with their subscriber counts, and the live tail.
#[derive(Debug, Default)]
pub struct PubSubState {
    pub counts: Counts,
    pub sel: usize,
    /// Polling the counts is paused (`p`).
    pub paused: bool,
    /// An answer has arrived (so an empty list means no channels, not "not read yet").
    pub loaded: bool,
    /// Why the view is off, in redis-lens's own words. `r` tries again.
    pub off: Option<String>,
    /// Why the last poll failed. The previous counts stay.
    pub error: Option<String>,
    pub tail: Tail,
    pending: Option<ReqId>,
    last_sent: Option<Instant>,
    editing: Option<Editing>,
    /// The `S` prompt was given a pattern: its hint says why until the next key.
    shard_refused: bool,
    /// The publish form (`P`), while it is open.
    pub publish: Option<Form>,
    /// The form publishes to a shard channel (`SPUBLISH`).
    pub publish_shard: bool,
    /// The `PUBLISH` or `SPUBLISH` in flight, and the shard channel's node.
    write: Option<(ReqId, Option<usize>)>,
    /// The shard channels of every node, most subscribers first (P4).
    pub shards: Vec<ShardRow>,
    /// How many shard channels the nodes have, over all of them.
    pub shards_total: usize,
    /// Why the shard channels are left out (an old server, or refused).
    pub shards_off: Option<String>,
    /// Why a shard count is missing, in redis-lens's own words.
    pub shards_notes: Vec<String>,
    /// The shard channels have been read once.
    pub shards_loaded: bool,
    /// The shard poll in flight: one request per node.
    shard_pending: Vec<(ReqId, usize)>,
    shard_gather: Vec<ShardRow>,
    shard_notes: Vec<String>,
    shard_total_acc: usize,
}

/// One shard channel, on the node that answered for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardRow {
    pub name: String,
    pub subscribers: Option<u64>,
    /// The node's index in the session's nodes (0 on a standalone server).
    pub node: usize,
}

/// The selected row of the view: a classic channel or a shard channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selected<'a> {
    Channel(&'a crate::live::pubsub::Channel),
    Shard(&'a ShardRow),
}

/// The `PUBLISH` of the publish form, or `SPUBLISH` for a shard channel.
pub fn publish_op(form: &Form, shard: bool) -> Result<WriteOp, String> {
    let channel = form.value("channel");
    if channel.is_empty() {
        return Err("Type the channel to publish to.".into());
    }
    let (channel, message) = (channel.to_string(), Raw::from(form.raw("message")));
    Ok(if shard {
        WriteOp::SPublish { channel, message }
    } else {
        WriteOp::Publish { channel, message }
    })
}

/// The text input open on the view.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Editing {
    /// `s`: the channel or pattern to subscribe to.
    Subscribe(String),
    /// `S`: the shard channel to subscribe to.
    ShardSubscribe(String),
    /// `/`: edits [`Tail::filter`] in place.
    Filter,
}

/// Where the live tail is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TailStatus {
    /// Never subscribed in this session.
    #[default]
    None,
    /// Connecting and sending `SUBSCRIBE`.
    Subscribing,
    /// Subscribed: messages arrive.
    Live,
    /// Stopped on purpose (`esc`, or a rescan replaced its run).
    Stopped,
    /// Ended on its own, with why in redis-lens's own words.
    Closed(String),
}

/// The live tail: what it subscribes to, the last [`TAIL_KEPT`] messages, the rate.
#[derive(Debug, Default)]
pub struct Tail {
    /// The current tail's id; messages of any other are stale.
    pub id: Option<u64>,
    pub to: Option<Subscription>,
    pub status: TailStatus,
    /// The last [`TAIL_KEPT`] messages, oldest first.
    pub rows: VecDeque<Arc<TailRow>>,
    /// While paused (`space`), the rows shown, as they were when it paused.
    frozen: Option<VecDeque<Arc<TailRow>>>,
    /// Messages received since the pause.
    pub new_while_paused: u64,
    /// Messages the runner left out because more than [`TAIL_KEPT`] arrived between
    /// two batches.
    pub dropped: u64,
    /// `/`: case-insensitive, on the channel and the message preview.
    pub filter: String,
    /// Messages per second over the last [`RATE_WINDOW`], as of the last tick.
    pub rate: f64,
    /// Messages counted at each tick, for the rate.
    window: VecDeque<(Instant, u64)>,
    /// Messages arrived since the last tick.
    uncounted: u64,
    /// Since when the rate counts: the subscribe, or the view coming on screen.
    since: Option<Instant>,
}

/// One message as the tail shows it: the message, and its one-line preview made once,
/// when it arrived ([`crate::value::preview`]: terminal safe). Its `Debug` shows neither the
/// payload nor the preview.
pub struct TailRow {
    pub msg: TailMessage,
    /// The preview in lower case, for the filter.
    lower: String,
}

impl TailRow {
    pub fn new(msg: TailMessage) -> Self {
        // The preview was made where the message was read (`TailMessage::new`), off the
        // screen's thread.
        let lower = msg.preview().to_lowercase();
        Self { msg, lower }
    }

    /// The payload on one line, escaped.
    pub fn preview(&self) -> &str {
        self.msg.preview()
    }

    /// True when `filter`, already in lower case, is in the channel or the preview.
    fn matches(&self, filter: &str) -> bool {
        filter.is_empty()
            || self.lower.contains(filter)
            || self.msg.channel.to_lowercase().contains(filter)
    }
}

impl fmt::Debug for TailRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TailRow")
            .field("msg", &self.msg)
            .field(
                "preview",
                &format_args!("<{} chars>", self.preview().chars().count()),
            )
            .finish()
    }
}

impl Tail {
    /// Subscribing or subscribed.
    pub fn active(&self) -> bool {
        matches!(self.status, TailStatus::Subscribing | TailStatus::Live)
    }

    pub fn paused(&self) -> bool {
        self.frozen.is_some()
    }

    /// The rows on screen: the frozen ones while paused, else the kept ones, then
    /// only those that match the filter. Oldest first.
    pub fn shown(&self) -> impl DoubleEndedIterator<Item = &TailRow> {
        let filter = self.filter.to_lowercase();
        self.frozen
            .as_ref()
            .unwrap_or(&self.rows)
            .iter()
            .map(|r| r.as_ref())
            .filter(move |r| r.matches(&filter))
    }

    /// How many rows the frozen or kept list has, before the filter.
    pub fn listed(&self) -> usize {
        self.frozen.as_ref().unwrap_or(&self.rows).len()
    }

    fn push(&mut self, msg: TailMessage) {
        if self.rows.len() == TAIL_KEPT {
            self.rows.pop_front();
        }
        self.rows.push_back(Arc::new(TailRow::new(msg)));
    }

    /// Starts counting the rate afresh from `now`.
    fn restart_rate(&mut self, now: Option<Instant>) {
        self.window.clear();
        self.uncounted = 0;
        self.since = now;
        self.rate = 0.0;
    }

    /// Credits the messages that arrived since the last tick at `now` and works out the
    /// rate over the last [`RATE_WINDOW`] (or since the count started, at least 1 s).
    fn tick(&mut self, now: Instant) {
        if !self.active() {
            self.restart_rate(None);
            return;
        }
        self.window
            .push_back((now, std::mem::take(&mut self.uncounted)));
        while self
            .window
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) >= RATE_WINDOW)
        {
            self.window.pop_front();
        }
        let since = *self.since.get_or_insert(now);
        let span = now
            .duration_since(since)
            .clamp(Duration::from_secs(1), RATE_WINDOW);
        let n: u64 = self.window.iter().map(|(_, n)| n).sum();
        self.rate = n as f64 / span.as_secs_f64();
    }
}

impl PubSubState {
    /// The `PUBLISH` the form sends as filled now, as the prompt shows it, or what is
    /// wrong: for the form's "Sends:" line. On screen only: it holds the message.
    pub fn form_sends(&self) -> Option<Result<String, String>> {
        self.publish
            .as_ref()
            .map(|f| publish_op(f, self.publish_shard).map(|op| op.shown()))
    }

    /// The rows of the list: the classic channels, then the shard channels.
    pub fn rows_len(&self) -> usize {
        self.counts.channels.len() + self.shards.len()
    }

    /// The selected row, if any.
    pub fn selected(&self) -> Option<Selected<'_>> {
        let n = self.counts.channels.len();
        match self.counts.channels.get(self.sel) {
            Some(c) => Some(Selected::Channel(c)),
            None => self
                .shards
                .get(self.sel.checked_sub(n)?)
                .map(Selected::Shard),
        }
    }

    /// The selected row as (shard, name, node), to find it again after a poll.
    fn selected_key(&self) -> Option<(bool, String, usize)> {
        match self.selected()? {
            Selected::Channel(c) => Some((false, c.name.clone(), 0)),
            Selected::Shard(r) => Some((true, r.name.clone(), r.node)),
        }
    }

    /// Selects `key`'s row again, else keeps the position in range.
    fn reselect(&mut self, key: Option<(bool, String, usize)>) {
        let n = self.counts.channels.len();
        let at = key.and_then(|(shard, name, node)| {
            if shard {
                self.shards
                    .iter()
                    .position(|r| r.name == name && r.node == node)
                    .map(|i| n + i)
            } else {
                self.counts.channels.iter().position(|c| c.name == name)
            }
        });
        self.sel = at
            .unwrap_or(self.sel)
            .min(self.rows_len().saturating_sub(1));
    }

    /// `P`: opens the publish form, on the selected channel: `SPUBLISH` on a shard
    /// channel, `PUBLISH` otherwise.
    fn open_publish(&mut self, act: &mut Act<'_>) {
        let (channel, shard) = match self.selected() {
            Some(Selected::Channel(c)) => (c.name.clone(), false),
            Some(Selected::Shard(r)) => (r.name.clone(), true),
            None => (String::new(), false),
        };
        let sample = if shard {
            WriteOp::SPublish {
                channel: "c".into(),
                message: Raw::from(""),
            }
        } else {
            WriteOp::Publish {
                channel: "c".into(),
                message: Raw::from(""),
            }
        };
        if write_off(act, &sample) {
            return;
        }
        let (title, about, hint) = if shard {
            (
                "Publish to a shard channel",
                "SPUBLISH sends the message to the shard channel's subscribers now, on the \
                 node that owns its slot.",
                "the shard channel; the selected one to start with",
            )
        } else {
            (
                "Publish a message",
                "PUBLISH sends the message to the channel's subscribers now.",
                "the channel; the selected one to start with",
            )
        };
        let mut f = Form::new(
            title,
            vec![about.to_string()],
            vec![
                Field::text("channel", hint).with(channel),
                Field::text("message", "the message, as typed; it is shown here only"),
            ],
        );
        f.focus = 1;
        self.publish = Some(f);
        self.publish_shard = shard;
    }

    /// A key while the publish form is open.
    fn publish_key(&mut self, key: KeyEvent, act: &mut Act<'_>) {
        let Some(f) = self.publish.as_mut() else {
            return;
        };
        match f.key(key) {
            Step::Typed => {}
            Step::Cancel => self.publish = None,
            Step::Submit => match publish_op(f, self.publish_shard) {
                Err(why) => act.error(why),
                Ok(op) => {
                    self.publish = None;
                    let node = match &op {
                        WriteOp::SPublish { channel, .. } => {
                            act.session.owner_of(channel.as_bytes())
                        }
                        _ => None,
                    };
                    self.write = act.write(op).map(|id| (id, node));
                }
            },
        }
    }

    /// A counts poll: the classic counts on the picked node, and the shard channels on
    /// every node (each from its own node's budget), each part only when its last
    /// request answered.
    fn fetch(&mut self, act: &mut Act<'_>) {
        self.last_sent = act.now;
        if !self.classic_in_flight(act) {
            self.pending = act.send(LiveRequest::PubSub(Request::Counts));
        }
        if self.shards_off.is_none()
            && let Some(why) = act.server().and_then(crate::live::pubsub::shards_off)
        {
            self.shards_off = Some(why);
        }
        if self.shards_off.is_some() || self.shards_in_flight(act) {
            return;
        }
        let nodes: Vec<usize> = if act.session.is_cluster() {
            (0..act.session.nodes.len())
                .filter(|&i| act.session.nodes[i].connected)
                .collect()
        } else {
            vec![0]
        };
        self.shard_gather.clear();
        self.shard_notes.clear();
        self.shard_total_acc = 0;
        for i in nodes {
            let dest = Dest::node(i).follow(Follow::Never);
            if let Some(id) = act.send_to(dest, LiveRequest::PubSub(Request::ShardCounts)) {
                self.shard_pending.push((id, i));
            }
        }
    }

    fn classic_in_flight(&mut self, act: &Act<'_>) -> bool {
        match self.pending {
            Some(id) if act.in_flight(id) => true,
            Some(_) => {
                self.pending = None;
                false
            }
            None => false,
        }
    }

    fn shards_in_flight(&mut self, act: &Act<'_>) -> bool {
        if self.shard_pending.iter().any(|(id, _)| act.in_flight(*id)) {
            return true;
        }
        self.shard_pending.clear();
        false
    }

    /// Both parts of the poll are still in flight: nothing to send. Each part is sent
    /// again only once its own last request answered.
    fn all_in_flight(&mut self, act: &Act<'_>) -> bool {
        let classic = self.classic_in_flight(act);
        let shards = self.shards_in_flight(act) || self.shards_off.is_some();
        classic && shards
    }

    /// One node's shard channels. The list is replaced once every node answered.
    fn shard_reply(&mut self, at: usize, result: Result<LiveReply, LiveError>) {
        let (_, node) = self.shard_pending.remove(at);
        match result {
            Ok(LiveReply::PubSub(Reply::ShardCounts(ShardCounts {
                channels,
                total_channels,
                notes,
            }))) => {
                self.shard_total_acc += total_channels;
                self.shard_gather
                    .extend(channels.into_iter().map(|c| ShardRow {
                        name: c.name,
                        subscribers: c.subscribers,
                        node,
                    }));
                for n in notes {
                    if !self.shard_notes.contains(&n) {
                        self.shard_notes.push(n);
                    }
                }
            }
            Ok(_) => {}
            Err(LiveError::Unsupported(_)) => {
                self.shards_off = Some(SHARDS_UNSUPPORTED.into());
            }
            Err(LiveError::Denied(_)) => {
                self.shards_off = Some(SHARDS_DENIED.into());
            }
            Err(LiveError::Closed) => {}
            Err(LiveError::Failed(why)) => {
                let why = format!("one node's shard channels are left out: {why}");
                if !self.shard_notes.contains(&why) {
                    self.shard_notes.push(why);
                }
            }
        }
        if !self.shard_pending.is_empty() {
            return;
        }
        let key = self.selected_key();
        let mut rows = std::mem::take(&mut self.shard_gather);
        rows.sort_by(|a, b| {
            b.subscribers
                .cmp(&a.subscribers)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.node.cmp(&b.node))
        });
        self.shards = if self.shards_off.is_some() {
            Vec::new()
        } else {
            rows
        };
        self.shards_total = std::mem::take(&mut self.shard_total_acc);
        self.shards_notes = std::mem::take(&mut self.shard_notes);
        self.shards_loaded = true;
        self.reselect(key);
    }

    /// Subscribes to `to`, replacing the tail there was: a new subscription starts with
    /// an empty list, no filter and not paused.
    fn start(&mut self, to: Subscription, act: &mut Act<'_>) {
        let Some(id) = act.subscribe(to.clone()) else {
            act.info("The live connection is not open yet: the tail can't start.");
            return;
        };
        let mut tail = Tail {
            id: Some(id),
            to: Some(to),
            status: TailStatus::Subscribing,
            ..Tail::default()
        };
        tail.restart_rate(act.now);
        self.tail = tail;
    }

    /// `esc` with no filter: ends the tail. Its messages stay until `c` or the next
    /// subscription.
    fn stop(&mut self, act: &mut Act<'_>) {
        if let Some(id) = self.tail.id {
            act.stop_tail(id);
        }
        self.tail.status = TailStatus::Stopped;
        self.tail.restart_rate(None);
        act.info("Stopped the tail. Its messages stay until c clears them.");
    }

    fn toggle_pause(&mut self, act: &mut Act<'_>) {
        let t = &mut self.tail;
        if t.id.is_none() {
            act.info("Nothing to pause: subscribe first (enter or s).");
        } else if t.frozen.take().is_some() {
            t.new_while_paused = 0;
            act.info("Tail resumed.");
        } else {
            t.frozen = Some(t.rows.clone());
            t.new_while_paused = 0;
            act.info("Tail paused: messages still arrive. Press space to show them.");
        }
    }

    fn clear(&mut self, act: &mut Act<'_>) {
        let t = &mut self.tail;
        t.rows.clear();
        if t.frozen.is_some() {
            t.frozen = Some(VecDeque::new());
        }
        t.new_while_paused = 0;
        t.dropped = 0;
        act.info("Cleared the tail.");
    }

    /// A key while an input is open. Every key is the input's.
    fn edit_key(&mut self, key: KeyEvent, act: &mut Act<'_>) {
        let Some(editing) = self.editing.as_mut() else {
            return;
        };
        self.shard_refused = false;
        let text = match editing {
            Editing::Subscribe(t) | Editing::ShardSubscribe(t) => t,
            Editing::Filter => &mut self.tail.filter,
        };
        match key.code {
            KeyCode::Esc => {
                if self.editing == Some(Editing::Filter) {
                    self.tail.filter.clear();
                }
                self.editing = None;
            }
            KeyCode::Enter => match self.editing.take() {
                Some(Editing::Subscribe(t)) => match Subscription::parse(&t) {
                    Some(to) => self.start(to, act),
                    None => act.info("Type a channel or a pattern to subscribe to."),
                },
                Some(Editing::ShardSubscribe(t)) => match Subscription::parse_shard(&t) {
                    Ok(Some(to)) => self.start(to, act),
                    Ok(None) => act.info("Type a shard channel to subscribe to."),
                    Err(why) => {
                        // The prompt stays open: a pattern is never sent.
                        act.error(form::sentence(why));
                        self.shard_refused = true;
                        self.editing = Some(Editing::ShardSubscribe(t));
                    }
                },
                Some(Editing::Filter) | None => {}
            },
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) if plain(&key) => push_text(text, &c.to_string()),
            _ => {}
        }
    }

    /// The live tail `tail` is subscribed. Called by `App` for the current run only.
    pub fn tail_ready(&mut self, tail: u64) {
        if self.tail.id == Some(tail) && self.tail.status == TailStatus::Subscribing {
            self.tail.status = TailStatus::Live;
        }
    }

    /// Messages of the live tail `tail`, oldest first, and how many more the runner
    /// left out. Ignored for another tail, or once this one stopped.
    pub fn tail_batch(&mut self, tail: u64, batch: Vec<TailMessage>, dropped: u64) {
        let t = &mut self.tail;
        if t.id != Some(tail) || !t.active() {
            return;
        }
        t.status = TailStatus::Live;
        let n = batch.len() as u64 + dropped;
        t.dropped += dropped;
        t.uncounted += n;
        if t.paused() {
            t.new_while_paused += n;
        }
        for m in batch {
            t.push(m);
        }
    }

    /// The live tail `tail` ended on its own; `error` says why.
    pub fn tail_closed(&mut self, tail: u64, error: Option<String>) {
        let t = &mut self.tail;
        if t.id != Some(tail) || !t.active() {
            return;
        }
        t.status = match error {
            Some(why) => TailStatus::Closed(why),
            None => TailStatus::Stopped,
        };
        t.restart_rate(None);
    }

    /// What the running tail is subscribed to, if one runs (a database switch asks
    /// before it ends the tail, to subscribe again).
    pub fn running_subscription(&self) -> Option<Subscription> {
        self.tail.active().then(|| self.tail.to.clone()).flatten()
    }

    /// A database switch ended the tail with its run: subscribes again to `to`, as
    /// [`PubSubState::start`] does, keeping the rows and the filter on screen.
    pub fn resubscribe(&mut self, to: Subscription, act: &mut Act<'_>) {
        let old = std::mem::take(&mut self.tail);
        self.start(to, act);
        if self.tail.id.is_some() {
            self.tail.rows = old.rows;
            self.tail.filter = old.filter;
        } else {
            self.tail = old;
        }
    }

    /// The run the tail belongs to was replaced (a rescan): it is stopped.
    pub fn end_tail(&mut self) {
        if self.tail.active() {
            self.tail.status = TailStatus::Stopped;
            self.tail.restart_rate(None);
        }
    }
}

/// Appends `add` to an input's `text`, without control characters, up to [`MAX_INPUT`]
/// characters.
fn push_text(text: &mut String, add: &str) {
    let room = MAX_INPUT.saturating_sub(text.chars().count());
    text.extend(add.chars().filter(|c| !c.is_control()).take(room));
}

impl ViewState for PubSubState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        // Messages that came while another view was shown are not in the rate.
        if self.tail.active() {
            self.tail.restart_rate(act.now);
        }
        if self.off.is_none() && !self.all_in_flight(act) && (!self.paused || !self.loaded) {
            self.fetch(act);
        }
    }

    fn on_tick(&mut self, now: Instant, act: &mut Act<'_>) {
        self.tail.tick(now);
        if self.off.is_some() || self.paused || !act.live_ready() || self.all_in_flight(act) {
            return;
        }
        if self.last_sent.is_none_or(|t| now.duration_since(t) >= POLL) {
            self.fetch(act);
            self.last_sent = Some(now);
        }
    }

    fn refresh(&mut self, act: &mut Act<'_>) {
        self.off = None;
        // A refusal of the shard channels is asked again (the ACL may have changed).
        self.shards_off = None;
        self.fetch(act);
    }

    fn input(&self) -> Option<Input> {
        if let Some(f) = &self.publish {
            return f.input();
        }
        match self.editing.as_ref()? {
            Editing::Subscribe(text) => Some(Input {
                label: "s",
                text: text.clone(),
                hint: "   channel, or pattern with * ? [ · enter to subscribe · esc to cancel",
            }),
            Editing::ShardSubscribe(text) => Some(Input {
                label: "S",
                text: text.clone(),
                hint: if self.shard_refused {
                    SHARD_PATTERN_HINT
                } else {
                    "   shard channel (no pattern) · enter to subscribe · esc to cancel"
                },
            }),
            Editing::Filter => Some(Input {
                label: "/",
                text: self.tail.filter.clone(),
                hint: "   channel or message · enter to apply · esc to clear",
            }),
        }
    }

    fn on_paste(&mut self, text: &str, _act: &mut Act<'_>) {
        if let Some(f) = self.publish.as_mut() {
            f.paste(text);
            return;
        }
        match self.editing.as_mut() {
            Some(Editing::Subscribe(t) | Editing::ShardSubscribe(t)) => push_text(t, text),
            Some(Editing::Filter) => push_text(&mut self.tail.filter, text),
            None => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if self.publish.is_some() {
            self.publish_key(key, act);
            return true;
        }
        if self.editing.is_some() {
            self.edit_key(key, act);
            return true;
        }
        if !plain(&key) {
            return false;
        }
        match key.code {
            KeyCode::Char('P') => {
                self.open_publish(act);
                return true;
            }
            KeyCode::Char('p') => {
                self.paused = !self.paused;
                if self.paused {
                    act.info("Polling paused. Press p to resume, r to refresh once.");
                } else {
                    act.info("Polling resumed.");
                    self.last_sent = None;
                }
                return true;
            }
            KeyCode::Enter => {
                let to = match self.selected() {
                    Some(Selected::Channel(ch)) => Some(Subscription::Channel(ch.name.clone())),
                    Some(Selected::Shard(r)) => Some(Subscription::Shard(r.name.clone())),
                    None => None,
                };
                match to {
                    Some(to) => self.start(to, act),
                    None => act.info("No channel selected: press s to type one."),
                }
                return true;
            }
            KeyCode::Char('s') => {
                self.editing = Some(Editing::Subscribe(String::new()));
                return true;
            }
            KeyCode::Char('S') => {
                if let Some(why) = self.shards_off.clone() {
                    act.error(why);
                } else if let Some(Selected::Shard(r)) = self.selected() {
                    let to = Subscription::Shard(r.name.clone());
                    self.start(to, act);
                } else {
                    self.editing = Some(Editing::ShardSubscribe(String::new()));
                }
                return true;
            }
            KeyCode::Char(' ') => {
                self.toggle_pause(act);
                return true;
            }
            KeyCode::Char('c') => {
                self.clear(act);
                return true;
            }
            KeyCode::Char('/') => {
                if self.tail.id.is_some() {
                    self.editing = Some(Editing::Filter);
                } else {
                    act.info("Nothing to filter yet: subscribe first (enter or s).");
                }
                return true;
            }
            KeyCode::Esc => {
                if !self.tail.filter.is_empty() {
                    self.tail.filter.clear();
                    return true;
                }
                if self.tail.active() {
                    self.stop(act);
                    return true;
                }
                return false;
            }
            _ => {}
        }
        let Some(to) = list_move(&key, self.sel, self.rows_len(), act.page) else {
            return false;
        };
        self.sel = to;
        true
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        if let Some((_, node)) = self.write.take_if(|(w, _)| *w == id) {
            // A shard channel's subscribers are counted on its own node.
            let on = node
                .and_then(|i| act.session.nodes.get(i))
                .map_or_else(String::new, |n| format!(" on {}", n.endpoint));
            match result {
                Ok(LiveReply::Write(WriteReply::Int(n))) => act.info(format!(
                    "Published to {}{on}.",
                    form::count(n, "subscriber", "subscribers")
                )),
                Ok(_) => act.info("Published."),
                Err(e) => write_failed(act, &e),
            }
            return;
        }
        if let Some(at) = self.shard_pending.iter().position(|(p, _)| *p == id) {
            self.shard_reply(at, result);
            return;
        }
        // Only the answer to the request in flight; an older one is stale.
        if self.pending != Some(id) {
            return;
        }
        self.pending = None;
        match result {
            Ok(LiveReply::PubSub(Reply::Counts(counts))) => {
                // The selection stays on its channel when the list moves.
                let key = self.selected_key();
                self.counts = counts;
                self.reselect(key);
                self.loaded = true;
                self.error = None;
            }
            Ok(_) => {}
            Err(e @ (LiveError::Denied(_) | LiveError::Unsupported(_))) => {
                self.off = Some(off_text(&e));
            }
            Err(LiveError::Closed) => {}
            Err(LiveError::Failed(why)) => self.error = Some(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{self, key};
    use super::*;
    use crate::live::pubsub::Channel;
    use crate::tui::app::{App, Effect, Msg, Screen, SecretSource};
    use crate::value;

    fn channel(name: &str, subscribers: u64) -> Channel {
        Channel {
            name: name.into(),
            subscribers: Some(subscribers),
        }
    }

    fn counts() -> Counts {
        Counts {
            channels: vec![
                channel("orders:eu", 12),
                channel("orders:us", 4),
                channel("news", 2),
                channel("alerts", 1),
                channel("idle", 0),
            ],
            total_channels: 5,
            patterns: Some(3),
            notes: Vec::new(),
        }
    }

    /// The live requests among `effects`, without the shard channel polls (their own
    /// tests are below).
    fn classic_requests(effects: &[Effect]) -> Vec<(ReqId, LiveRequest)> {
        testing::requests(effects)
            .into_iter()
            .filter(|(_, r)| *r != LiveRequest::PubSub(Request::ShardCounts))
            .collect()
    }

    fn at(base: Instant, ms: u64) -> Msg {
        Msg::TickAt(base + Duration::from_millis(ms))
    }

    fn tick(app: &mut App, t0: Instant, ms: u64) -> Option<ReqId> {
        let reqs = classic_requests(&app.apply(at(t0, ms)));
        assert!(reqs.len() <= 1, "{reqs:?}");
        reqs.first().map(|r| r.0)
    }

    fn answer(app: &mut App, id: ReqId, c: Counts) {
        testing::reply(app, id, Ok(LiveReply::PubSub(Reply::Counts(c))));
    }

    fn loaded() -> (App, Instant) {
        let t0 = Instant::now();
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        app.apply(Msg::TickAt(t0));
        let effects = testing::show(&mut app, Screen::PubSub);
        let reqs = classic_requests(&effects);
        let [(id, LiveRequest::PubSub(Request::Counts))] = &reqs[..] else {
            panic!("expected one Counts request, got {reqs:?}");
        };
        answer(&mut app, *id, counts());
        (app, t0)
    }

    #[test]
    fn on_show_asks_for_the_counts() {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::PubSub);
        let reqs = classic_requests(&effects);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1, LiveRequest::PubSub(Request::Counts));
    }

    #[test]
    fn polls_every_five_seconds_while_shown_and_p_pauses() {
        let (mut app, t0) = loaded();
        assert_eq!(tick(&mut app, t0, 4_900), None);
        let id = tick(&mut app, t0, 5_000).expect("a poll at 5 s");
        assert_eq!(tick(&mut app, t0, 9_000), None, "one at a time");
        answer(&mut app, id, counts());
        assert_eq!(tick(&mut app, t0, 9_900), None);
        assert!(tick(&mut app, t0, 10_000).is_some());
        // Paused: no polls; `r` still reads.
        let id = app.views.pubsub.pending.unwrap();
        answer(&mut app, id, counts());
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.views.pubsub.paused);
        assert_eq!(tick(&mut app, t0, 60_000), None);
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(classic_requests(&effects).len(), 1);
    }

    #[test]
    fn does_not_poll_while_hidden() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(tick(&mut app, t0, 60_000), None);
    }

    #[test]
    fn keys_move_the_selection_and_it_follows_its_channel() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.views.pubsub.sel, 2);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.views.pubsub.sel, 4);
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.views.pubsub.sel, 0);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.views.pubsub.counts.channels[1].name, "orders:us");
        // The next poll ranks orders:us first.
        let id = tick(&mut app, t0, 6_000).expect("a poll");
        let mut c = counts();
        c.channels[1].subscribers = Some(40);
        c.channels.sort_by_key(|c| std::cmp::Reverse(c.subscribers));
        answer(&mut app, id, c);
        assert_eq!(app.views.pubsub.sel, 0);
        assert_eq!(app.views.pubsub.counts.channels[0].name, "orders:us");
    }

    #[test]
    fn a_refused_pubsub_turns_the_view_off_with_fixed_text() {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::PubSub);
        let (id, _) = classic_requests(&effects).remove(0);
        testing::reply(
            &mut app,
            id,
            Err(LiveError::Denied("PUBSUB CHANNELS".into())),
        );
        assert_eq!(
            app.views.pubsub.off.as_deref(),
            Some("The server refused PUBSUB CHANNELS for this user.")
        );
        let t0 = Instant::now();
        app.apply(Msg::TickAt(t0));
        assert_eq!(tick(&mut app, t0, 60_000), None);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(classic_requests(&effects).len(), 1);
    }

    #[test]
    fn a_failed_poll_keeps_the_counts() {
        let (mut app, t0) = loaded();
        let id = tick(&mut app, t0, 6_000).expect("a poll");
        testing::reply(
            &mut app,
            id,
            Err(LiveError::Failed("PUBSUB CHANNELS failed".into())),
        );
        assert_eq!(app.views.pubsub.counts.channels.len(), 5);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn snapshots() {
        let (mut app, _) = loaded();
        insta::assert_snapshot!("counts_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("counts_120x36", testing::render(&app, 120, 36).backend());
        // A refused NUMSUB and NUMPAT: the counts are left out, with the reason.
        let mut c = counts();
        for ch in &mut c.channels {
            ch.subscribers = None;
        }
        c.patterns = None;
        c.notes = vec![
            "the server refused PUBSUB NUMPAT for this user".into(),
            "the server refused PUBSUB NUMSUB for this user".into(),
        ];
        let id = app.views.pubsub.pending.unwrap_or_else(|| {
            let t0 = Instant::now();
            app.apply(Msg::TickAt(t0));
            tick(&mut app, t0, 60_000).expect("a poll")
        });
        answer(&mut app, id, c);
        insta::assert_snapshot!("no_counts_80x24", testing::render(&app, 80, 24).backend());
        // No channels.
        let id = {
            let t0 = Instant::now();
            app.apply(Msg::TickAt(t0));
            tick(&mut app, t0, 120_000).expect("a poll")
        };
        answer(
            &mut app,
            id,
            Counts {
                patterns: Some(0),
                ..Counts::default()
            },
        );
        insta::assert_snapshot!("empty_80x24", testing::render(&app, 80, 24).backend());
    }

    // -- The live tail ----------------------------------------------------------

    const MARKER: &str = "s3cr3t-p4yload";

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn type_text(app: &mut App, text: &str) -> Vec<Effect> {
        let mut out = Vec::new();
        for c in text.chars() {
            out.extend(app.handle_key(ch(c)));
        }
        out
    }

    fn ids(app: &App) -> (u64, u64) {
        let s = app.session.as_ref().unwrap();
        (s.id, s.scan)
    }

    fn msg(at_unix_ms: i64, channel: &str, payload: &[u8]) -> TailMessage {
        TailMessage::new(at_unix_ms, channel.into(), None, payload.to_vec())
    }

    /// The subscribe among `effects`: its tail id and what it subscribes to.
    fn subscribe_of(effects: &[Effect]) -> Option<(u64, Subscription)> {
        let subs: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Subscribe { tail, to, .. } => Some((*tail, to.clone())),
                _ => None,
            })
            .collect();
        assert!(subs.len() <= 1, "{effects:?}");
        subs.into_iter().next()
    }

    fn batch(app: &mut App, tail: u64, msgs: Vec<TailMessage>, dropped: u64) {
        let (id, scan) = ids(app);
        app.apply(Msg::Tail {
            id,
            scan,
            tail,
            batch: msgs,
            dropped,
        });
    }

    /// Loaded, then `enter` on the first channel (orders:eu), subscribed.
    fn subscribed() -> (App, Instant, u64) {
        let (mut app, t0) = loaded();
        let effects = app.handle_key(key(KeyCode::Enter));
        let (tail, _) = subscribe_of(&effects).expect("a subscribe");
        let (id, scan) = ids(&app);
        app.apply(Msg::TailReady { id, scan, tail });
        assert_eq!(app.views.pubsub.tail.status, TailStatus::Live);
        (app, t0, tail)
    }

    fn shown(app: &App) -> Vec<String> {
        app.views
            .pubsub
            .tail
            .shown()
            .map(|r| r.preview().to_string())
            .collect()
    }

    fn screen_text(app: &App, w: u16, h: u16) -> String {
        let t = testing::render(app, w, h);
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn enter_subscribes_to_the_selected_channel() {
        let (mut app, _) = loaded();
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [
            Effect::Subscribe {
                id,
                scan,
                tail,
                profile,
                secrets,
                to,
                ..
            },
        ] = &effects[..]
        else {
            panic!("expected one subscribe, got {effects:?}");
        };
        assert_eq!((*id, *scan), ids(&app));
        assert_eq!(*to, Subscription::Channel("orders:us".into()));
        assert_eq!(profile.name, "prod-cache-01");
        assert!(matches!(secrets, SecretSource::Keychain));
        let t = &app.views.pubsub.tail;
        assert_eq!((t.id, &t.status), (Some(*tail), &TailStatus::Subscribing));
        // Ready once subscribed.
        app.apply(Msg::TailReady {
            id: *id,
            scan: *scan,
            tail: *tail,
        });
        assert_eq!(app.views.pubsub.tail.status, TailStatus::Live);
    }

    #[test]
    fn s_asks_for_a_channel_or_a_pattern() {
        let (mut app, _) = loaded();
        assert!(app.handle_key(ch('s')).is_empty());
        let input = app.view_input().expect("an input");
        assert_eq!(input.label, "s");
        // Every key is text, digits and q included.
        assert!(type_text(&mut app, "q1:*").is_empty());
        assert_eq!(app.view_input().unwrap().text, "q1:*");
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        type_text(&mut app, "orders:*");
        let effects = app.handle_key(key(KeyCode::Enter));
        let (_, to) = subscribe_of(&effects).expect("a subscribe");
        assert_eq!(to, Subscription::Pattern("q1orders:*".into()));
        assert!(app.view_input().is_none());

        // A name without * ? [ is a channel; a paste goes into the input, without
        // control characters.
        app.handle_key(ch('s'));
        app.handle_paste("news\n".into());
        let effects = app.handle_key(key(KeyCode::Enter));
        let (_, to) = subscribe_of(&effects).expect("a subscribe");
        assert_eq!(to, Subscription::Channel("news".into()));

        // esc cancels: nothing is sent.
        app.handle_key(ch('s'));
        type_text(&mut app, "x");
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(app.view_input().is_none());
    }

    #[test]
    fn batches_append_and_only_the_last_2000_stay() {
        let (mut app, _, tail) = subscribed();
        let many = |from: usize, n: usize| -> Vec<TailMessage> {
            (from..from + n)
                .map(|i| msg(i as i64, "orders:eu", format!("m{i}").as_bytes()))
                .collect()
        };
        batch(&mut app, tail, many(0, 1_500), 0);
        assert_eq!(app.views.pubsub.tail.rows.len(), 1_500);
        batch(&mut app, tail, many(1_500, 1_000), 7);
        let t = &app.views.pubsub.tail;
        assert_eq!(t.rows.len(), TAIL_KEPT);
        assert_eq!(t.rows.front().unwrap().preview(), "m500");
        assert_eq!(t.rows.back().unwrap().preview(), "m2499");
        assert_eq!(t.dropped, 7);
        assert!(screen_text(&app, 120, 36).contains("7 skipped"));
    }

    #[test]
    fn space_freezes_the_list_and_counts_what_is_new() {
        let (mut app, _, tail) = subscribed();
        batch(
            &mut app,
            tail,
            vec![msg(1, "a", b"one"), msg(2, "a", b"two")],
            0,
        );
        app.handle_key(ch(' '));
        assert!(app.views.pubsub.tail.paused());
        batch(&mut app, tail, vec![msg(3, "a", b"three")], 2);
        assert_eq!(shown(&app), ["one", "two"]);
        assert_eq!(app.views.pubsub.tail.rows.len(), 3);
        assert_eq!(app.views.pubsub.tail.new_while_paused, 3);
        assert!(screen_text(&app, 120, 36).contains("paused · 3 new"));
        app.handle_key(ch(' '));
        assert_eq!(shown(&app), ["one", "two", "three"]);
        assert!(!screen_text(&app, 120, 36).contains("paused"));
    }

    #[test]
    fn c_clears_the_tail() {
        let (mut app, _, tail) = subscribed();
        batch(&mut app, tail, vec![msg(1, "a", b"one")], 4);
        app.handle_key(ch('c'));
        let t = &app.views.pubsub.tail;
        assert!(t.rows.is_empty() && t.dropped == 0);
        assert_eq!(t.status, TailStatus::Live, "clearing does not stop it");
        batch(&mut app, tail, vec![msg(2, "a", b"two")], 0);
        assert_eq!(shown(&app), ["two"]);
    }

    #[test]
    fn slash_filters_and_esc_clears_then_stops() {
        let (mut app, _, tail) = subscribed();
        batch(
            &mut app,
            tail,
            vec![
                msg(1, "orders:eu", b"Hello there"),
                msg(2, "news", b"world"),
                msg(3, "NEWS:eu", b"other"),
            ],
            0,
        );
        app.handle_key(ch('/'));
        assert_eq!(app.view_input().unwrap().label, "/");
        type_text(&mut app, "HELLO");
        assert_eq!(shown(&app), ["Hello there"]);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.view_input().is_none());
        assert_eq!(app.views.pubsub.tail.filter, "HELLO");
        // The channel counts too.
        app.handle_key(ch('/'));
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Backspace));
        }
        type_text(&mut app, "news");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(shown(&app), ["world", "other"]);
        assert!(screen_text(&app, 120, 36).contains("/ news · 2 of 3"));
        // esc clears the filter first...
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(shown(&app).len(), 3);
        // ...then stops the tail.
        let effects = app.handle_key(key(KeyCode::Esc));
        assert!(
            matches!(effects[..], [Effect::Unsubscribe { tail: t, .. }] if t == tail),
            "{effects:?}"
        );
        assert_eq!(app.views.pubsub.tail.status, TailStatus::Stopped);
        // What arrives after that is not shown; the messages stay.
        batch(&mut app, tail, vec![msg(4, "a", b"late")], 0);
        assert_eq!(shown(&app).len(), 3);
        // esc in the filter input clears it.
        app.handle_key(ch('/'));
        type_text(&mut app, "zzz");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.views.pubsub.tail.filter.is_empty());
    }

    #[test]
    fn messages_of_a_replaced_tail_or_run_are_ignored() {
        let (mut app, _, old) = subscribed();
        let (id, scan) = ids(&app);
        // Another scan's messages.
        app.apply(Msg::Tail {
            id,
            scan: scan + 1,
            tail: old,
            batch: vec![msg(1, "a", b"other run")],
            dropped: 0,
        });
        assert!(shown(&app).is_empty());
        // A new subscription replaces the tail: the old one's messages are stale.
        app.handle_key(ch('s'));
        type_text(&mut app, "news");
        let (new, _) = subscribe_of(&app.handle_key(key(KeyCode::Enter))).unwrap();
        assert_ne!(new, old);
        batch(&mut app, old, vec![msg(2, "a", b"old tail")], 0);
        app.apply(Msg::TailReady {
            id,
            scan,
            tail: old,
        });
        app.apply(Msg::TailClosed {
            id,
            scan,
            tail: old,
            error: Some("x".into()),
        });
        let t = &app.views.pubsub.tail;
        assert_eq!(t.status, TailStatus::Subscribing);
        assert!(t.rows.is_empty());
        batch(&mut app, new, vec![msg(3, "news", b"new tail")], 0);
        assert_eq!(shown(&app), ["new tail"]);
    }

    #[test]
    fn a_rescan_ends_the_tail() {
        let (mut app, _, tail) = subscribed();
        batch(&mut app, tail, vec![msg(1, "a", b"kept")], 0);
        let (id, scan) = ids(&app);
        app.handle_key(ch('1'));
        let effects = app.handle_key(ch('r'));
        assert!(
            matches!(effects[..], [Effect::Connect { .. }]),
            "{effects:?}"
        );
        assert_eq!(app.views.pubsub.tail.status, TailStatus::Stopped);
        // The replaced run's tail is gone.
        app.apply(Msg::Tail {
            id,
            scan,
            tail,
            batch: vec![msg(2, "a", b"late")],
            dropped: 0,
        });
        assert_eq!(shown(&app), ["kept"]);
    }

    #[test]
    fn a_closed_tail_says_why_in_fixed_words() {
        let (mut app, _) = loaded();
        let (tail, _) = subscribe_of(&app.handle_key(key(KeyCode::Enter))).unwrap();
        let (id, scan) = ids(&app);
        app.apply(Msg::TailClosed {
            id,
            scan,
            tail,
            error: Some(Subscription::Channel("orders:eu".into()).denied()),
        });
        assert!(matches!(
            app.views.pubsub.tail.status,
            TailStatus::Closed(_)
        ));
        let text = screen_text(&app, 120, 36);
        assert!(
            text.contains(
                "The tail stopped: the server refused SUBSCRIBE on that channel for this user."
            ),
            "{text}"
        );
        insta::assert_snapshot!(
            "tail_refused_80x24",
            testing::render(&app, 80, 24).backend()
        );
    }

    #[test]
    fn the_rate_counts_the_last_five_seconds() {
        let (mut app, t0, tail) = subscribed();
        batch(
            &mut app,
            tail,
            (0..10).map(|i| msg(i, "a", b"x")).collect(),
            0,
        );
        app.apply(at(t0, 1_000));
        assert!((app.views.pubsub.tail.rate - 10.0).abs() < 1e-9);
        batch(
            &mut app,
            tail,
            (0..20).map(|i| msg(i, "a", b"x")).collect(),
            0,
        );
        app.apply(at(t0, 2_000));
        assert!((app.views.pubsub.tail.rate - 15.0).abs() < 1e-9);
        app.apply(at(t0, 6_500));
        // The first second's 10 fell out of the window; 20 over 5 s.
        assert!(
            (app.views.pubsub.tail.rate - 4.0).abs() < 1e-9,
            "{}",
            app.views.pubsub.tail.rate
        );
        app.apply(at(t0, 8_000));
        assert_eq!(app.views.pubsub.tail.rate, 0.0);
        // Messages that arrive while another view is shown are left out of the rate.
        app.handle_key(ch('1'));
        batch(
            &mut app,
            tail,
            (0..100).map(|i| msg(i, "a", b"x")).collect(),
            0,
        );
        app.apply(at(t0, 9_000));
        app.handle_key(ch('5'));
        app.apply(at(t0, 10_000));
        assert_eq!(app.views.pubsub.tail.rate, 0.0);
        assert_eq!(app.views.pubsub.tail.rows.len(), 130, "but they are kept");
    }

    #[test]
    fn payloads_never_reach_debug_notices_or_snapshots() {
        let (mut app, _, tail) = subscribed();
        let mut m = msg(1, "orders:eu", MARKER.as_bytes());
        m.pattern = Some("orders:*".into());
        batch(&mut app, tail, vec![m], 0);
        app.handle_key(ch(' '));
        app.handle_key(ch('c'));
        batch(
            &mut app,
            tail,
            vec![msg(2, "orders:eu", MARKER.as_bytes())],
            0,
        );
        app.handle_key(ch(' '));
        // On screen only.
        assert!(screen_text(&app, 120, 36).contains(MARKER));
        // `App` has no `Debug`; everything the tail holds is in its views and session.
        let debug = format!("{:?} {:?}", app.views, app.session);
        assert!(!value::leaks(&debug, MARKER), "Debug shows a payload");
        let notice = app
            .notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        assert!(!value::leaks(&notice, MARKER), "{notice}");
        // A snapshot holds the report only.
        app.handle_key(ch('2'));
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(report)] = &effects[..] else {
            panic!("expected a snapshot, got {effects:?}");
        };
        let json = serde_json::to_string(report).unwrap();
        assert!(!value::leaks(&json, MARKER));
        assert!(!value::leaks(&format!("{effects:?}"), MARKER));
    }

    #[test]
    fn tail_snapshots() {
        let (mut app, _, tail) = subscribed();
        // 2024-05-01 13:04:05.678 UTC onwards.
        let t = 1_714_568_645_678;
        let mut msgs = vec![
            msg(
                t,
                "orders:eu",
                br#"{"id":10293,"total":42.5,"items":["a","b"]}"#,
            ),
            msg(t + 12, "orders:eu", b"plain text with a newline\nin it"),
            msg(t + 250, "orders:eu", b"\x1b[2Jcleared?"),
            msg(t + 1_003, "orders:eu\x1b[31m", &[0x00, 0xff, 0x10, 0x80]),
        ];
        for i in 0..20 {
            msgs.push(msg(
                t + 2_000 + i,
                "orders:eu",
                format!("event {i}").as_bytes(),
            ));
        }
        batch(&mut app, tail, msgs, 0);
        for (w, h) in [(80u16, 24u16), (120, 36)] {
            let term = testing::render(&app, w, h);
            let raw: String = term
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                !raw.contains('\x1b'),
                "a raw ESC reached the terminal at {w}x{h}"
            );
            if h == 36 {
                assert!(
                    raw.contains("\\x1b[2Jcleared?"),
                    "the escape is drawn as text"
                );
            }
        }
        insta::assert_snapshot!("tail_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("tail_120x36", testing::render(&app, 120, 36).backend());
        // A pattern subscription, paused and filtered.
        app.handle_key(ch('s'));
        type_text(&mut app, "orders:*");
        let (tail, _) = subscribe_of(&app.handle_key(key(KeyCode::Enter))).unwrap();
        let (id, scan) = ids(&app);
        app.apply(Msg::TailReady { id, scan, tail });
        let mut m = msg(t, "orders:eu", b"\x1b[2J");
        m.pattern = Some("orders:*".into());
        batch(
            &mut app,
            tail,
            vec![m, msg(t + 5, "orders:us", b"second")],
            0,
        );
        app.handle_key(ch(' '));
        batch(&mut app, tail, vec![msg(t + 9, "orders:us", b"third")], 0);
        app.handle_key(ch('/'));
        type_text(&mut app, "us");
        app.handle_key(key(KeyCode::Enter));
        insta::assert_snapshot!(
            "tail_pattern_paused_80x24",
            testing::render(&app, 80, 24).backend()
        );
    }

    #[test]
    fn a_large_payload_keeps_only_its_start() {
        let big = vec![b'a'; 1 << 20];
        let row = TailRow::new(TailMessage::new(0, "c".into(), None, big));
        assert_eq!(row.msg.payload.len(), PAYLOAD_KEPT);
        assert!(row.preview().starts_with("aaaa"));
    }

    // -----------------------------------------------------------------------
    // Publish (P3)
    // -----------------------------------------------------------------------

    use super::super::streams::form::testing::{
        READ_ONLY, answer_anything, press, set_profile, writes,
    };
    use crate::live::write::Reply as WriteReply;
    use crate::model::Env;
    use crate::tui::app::confirm::Expect;

    fn notice(app: &App) -> String {
        app.notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    const MESSAGE: &str = "made-up order 4711";

    #[test]
    fn p_publishes_to_the_selected_channel_at_once_outside_prod() {
        let (mut app, _) = loaded();
        set_profile(&mut app, Env::Local, None);
        app.handle_key(ch('j'));
        assert!(app.handle_key(ch('P')).is_empty());
        let f = app.views.pubsub.publish.as_ref().expect("the form");
        assert_eq!(f.value("channel"), "orders:us");
        // The message has the focus, and every key is text: p does not pause.
        type_text(&mut app, MESSAGE);
        assert!(!app.views.pubsub.paused);
        insta::assert_snapshot!(
            "publish_form_80x24",
            testing::render(&app, 80, 24).backend()
        );
        assert!(!format!("{:?}", app.views).contains(MESSAGE));
        let e = press(&mut app, KeyCode::Enter);
        assert!(app.confirm.is_none());
        assert_eq!(
            writes(&e),
            vec![WriteOp::Publish {
                channel: "orders:us".into(),
                message: Raw::from(MESSAGE),
            }]
        );
        assert!(app.views.pubsub.publish.is_none());
        let (id, _) = classic_requests(&e)[0].clone();
        testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(3))));
        assert_eq!(notice(&app), "Published to 3 subscribers.");
        let (id, _) = {
            app.handle_key(ch('P'));
            type_text(&mut app, "x");
            classic_requests(&press(&mut app, KeyCode::Enter))[0].clone()
        };
        testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(1))));
        assert_eq!(notice(&app), "Published to 1 subscriber.");
    }

    #[test]
    fn publish_asks_y_n_on_prod_and_needs_a_channel() {
        let (mut app, _) = loaded();
        set_profile(&mut app, Env::Prod, Some(false));
        app.handle_key(ch('P'));
        // Shift-tab back to the channel, cleared: nothing to send.
        press(&mut app, KeyCode::BackTab);
        app.handle_key(KeyEvent::new(
            KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        press(&mut app, KeyCode::Enter);
        let e = press(&mut app, KeyCode::Enter);
        assert!(writes(&e).is_empty());
        assert_eq!(notice(&app), "Type the channel to publish to.");
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "news");
        press(&mut app, KeyCode::Tab);
        app.handle_paste("\nhi there".into());
        let f = app.views.pubsub.publish.as_ref().unwrap();
        assert_eq!((f.value("channel"), f.raw("message")), ("news", "hi there"));
        let e = press(&mut app, KeyCode::Enter);
        assert!(writes(&e).is_empty());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        assert_eq!(writes(&app.handle_key(ch('y'))).len(), 1);
        // esc closes the form without sending.
        app.handle_key(ch('P'));
        let e = press(&mut app, KeyCode::Esc);
        assert!(writes(&e).is_empty() && app.views.pubsub.publish.is_none());
    }

    #[test]
    fn no_publish_reaches_a_read_only_profile() {
        for (env, ro) in READ_ONLY {
            let (mut app, _) = loaded();
            set_profile(&mut app, env, ro);
            let mut e = app.handle_key(ch('P'));
            assert!(app.views.pubsub.publish.is_none());
            assert!(notice(&app).contains("read-only"), "{}", notice(&app));
            e.extend(type_text(&mut app, "hello"));
            e.extend(answer_anything(&mut app));
            assert!(writes(&e).is_empty(), "{env:?} {ro:?}");
        }
    }
}
