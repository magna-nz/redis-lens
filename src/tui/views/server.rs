//! State of the Server view: the Overview, Clients, Slow log and Monitor panels (spec "3
//! Server").
//! Owner: wave B. Drawn by `ui/server.rs`; its live requests are in `src/live/server.rs`.
//! See [`super`] for the interface `App` calls.
//!
//! Each panel polls only while it is on screen (spec "Shared command budget"): `on_show`
//! asks at once, `on_tick` asks again when the panel's interval has passed, and `p`
//! pauses all three. `r` asks now. A permission the server refuses, or a command it
//! does not know, turns that one panel off with fixed text ([`Poll::off`]); any other
//! failure leaves the last data on screen with a line saying the poll failed.
//!
//! Nothing here keeps a user name, a password or the server's own error text: the
//! `CLIENT LIST` user is dropped by the parser, and every message is built from
//! [`LiveError`], whose wording is fixed.
//!
//! The Monitor panel ([`Monitor`]) does not poll. `enter` asks first (`y`/`n`, or the
//! profile name on a `prod` profile), then starts a run with [`Act::monitor`]; the run
//! ends after [`MAX_TIME`] or [`MAX_LINES`] lines, with `esc`, or when the session's live
//! connection goes. Its lines (values, with credentials masked on arrival) stay here, in
//! memory, for the session. When it ends, the keys its lines name are ranked for Hot
//! keys ([`Monitor::ranking`]), placed by the key specs the Slow log learns too (one
//! cache, [`SlowLog`]); commands not in it are asked with `COMMAND INFO`.
//!
//! Writes (P3), each through [`Act::write`], the one gate, all `@dangerous`: `K` on a
//! group's connections kills the selected one (`CLIENT KILL ID`), but not the
//! connection redis-lens reads the list with ([`is_own`]); `c` on the Slow log opens a
//! form for `slowlog-log-slower-than` and `slowlog-max-len` (`CONFIG SET`, one setting
//! per write); `R` sends `SLOWLOG RESET`. The panel reads again after each.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::live::monitor::{self, End, Entry, MAX_LINES, MAX_TIME, Ranking};
use crate::live::server::{
    ClientConn, CommandKeys, InfoSample, Reply, Request, SlowConfig, SlowEntry, SlowLogReply,
    names_to_learn,
};
use crate::live::write::Reply as WriteReply;
use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::Env;
use crate::tui::app::Screen;
use crate::write::{SlowlogSetting, WriteOp};

use super::streams::form::{Field, Form, Step, write_failed, write_off};
use super::{Act, Input, ViewState};

/// Polls kept for the sparklines.
pub const HISTORY: usize = 60;
/// What `+` and `-` step through, in seconds (spec: 1 to 60 s).
pub const INTERVALS: [u64; 8] = [1, 2, 3, 5, 10, 15, 30, 60];
const DEFAULT_INTERVAL: usize = 1;
/// Clients poll every 5 s, the Slow log every 10 s.
pub const CLIENTS_EVERY: Duration = Duration::from_secs(5);
pub const SLOWLOG_EVERY: Duration = Duration::from_secs(10);
/// A connection idle this long (seconds) flags its group.
pub const IDLE_FLAG_SECS: u64 = 3600;
/// Rates over a shorter gap are noise.
const MIN_DT: Duration = Duration::from_millis(100);

/// The four panels: three that poll, and Monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Overview,
    Clients,
    SlowLog,
    Monitor,
}

impl Panel {
    pub const ALL: [Panel; 4] = [
        Panel::Overview,
        Panel::Clients,
        Panel::SlowLog,
        Panel::Monitor,
    ];

    pub fn of(screen: Screen) -> Option<Self> {
        match screen {
            Screen::Overview => Some(Self::Overview),
            Screen::Clients => Some(Self::Clients),
            Screen::SlowLog => Some(Self::SlowLog),
            Screen::Monitor => Some(Self::Monitor),
            _ => None,
        }
    }
}

/// The four panels' state. `act.screen` says which one is shown.
#[derive(Debug)]
pub struct ServerState {
    pub paused: bool,
    /// Index into [`INTERVALS`]: how often the Overview polls.
    interval: usize,
    pub overview: Overview,
    pub clients: Clients,
    pub slow: SlowLog,
    pub monitor: Monitor,
    /// The slow log settings form (`c`), while it is open.
    pub form: Option<Form>,
    /// The write in flight, if any.
    write: Option<(ReqId, Sent)>,
    /// The second setting of the form, sent once the first one went through.
    then: Option<WriteOp>,
    /// The per-shard summary of a cluster's Overview.
    pub shards: Shards,
}

/// The per-shard summary polls at most this often, or at the Overview's interval when
/// that is longer (spec "Cluster live views (P4)").
pub const SHARDS_EVERY: Duration = Duration::from_secs(5);

/// The per-shard summary below a cluster's Overview: one `INFO` per shard's node, each
/// from that node's budget, only while the Overview is shown and polling is not paused.
#[derive(Debug, Default)]
pub struct Shards {
    /// One per node of `session.nodes`, in slot order.
    pub rows: Vec<ShardRow>,
    sent: Option<Instant>,
}

/// One shard's row: its last `INFO`, and whether its last poll failed.
#[derive(Debug, Default)]
pub struct ShardRow {
    in_flight: Option<ReqId>,
    pub last: Option<InfoSample>,
    /// The node did not answer the last poll (drawn as ✗).
    pub failed: bool,
}

impl Shards {
    fn busy(&self, act: &Act<'_>) -> bool {
        self.rows
            .iter()
            .any(|r| r.in_flight.is_some_and(|id| act.in_flight(id)))
    }

    /// Sends one `INFO` to each node when the summary is due. Nothing on a standalone
    /// server.
    fn poll(&mut self, every: Duration, act: &mut Act<'_>) {
        let n = act.session.nodes.len();
        if n == 0 || !act.live_ready() || self.busy(act) {
            return;
        }
        let due = match (act.now, self.sent) {
            (Some(now), Some(sent)) => now.saturating_duration_since(sent) >= every,
            (_, None) => true,
            (None, Some(_)) => false,
        };
        if !due {
            return;
        }
        self.rows.resize_with(n, ShardRow::default);
        for (i, row) in self.rows.iter_mut().enumerate() {
            row.in_flight = act.send_to(
                crate::live::Dest::node(i),
                LiveRequest::Server(Request::Info),
            );
        }
        self.sent = act.now;
    }

    /// The answer to one of the summary's requests. False when `id` is not one.
    fn reply(&mut self, id: ReqId, result: &Result<LiveReply, LiveError>) -> bool {
        let Some(row) = self.rows.iter_mut().find(|r| r.in_flight == Some(id)) else {
            return false;
        };
        row.in_flight = None;
        match result {
            Ok(LiveReply::Server(Reply::Info { sample, .. })) => {
                row.last = Some((**sample).clone());
                row.failed = false;
            }
            _ => row.failed = true,
        }
        true
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            paused: false,
            interval: DEFAULT_INTERVAL,
            overview: Overview::default(),
            clients: Clients::default(),
            slow: SlowLog::default(),
            monitor: Monitor::default(),
            form: None,
            write: None,
            then: None,
            shards: Shards::default(),
        }
    }
}

/// A write of this view in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Kill(u64),
    Config,
    Reset,
}

/// True for the connection redis-lens read the list with: its last command is the
/// `CLIENT LIST` itself (`client|list`; `client` before Redis 7.0), and it was not idle.
/// redis-lens sets no client name, so this is all the list tells; another client that
/// ran `CLIENT LIST` in the same second looks the same, and is refused too.
pub fn is_own(c: &ClientConn) -> bool {
    matches!(c.cmd.as_str(), "client|list" | "client") && c.idle == 0
}

/// The `CONFIG SET`s of the slow log form, threshold first: only the settings typed
/// different from `known` (all typed ones when the settings are unknown).
pub fn slowlog_ops(form: &Form, known: Option<SlowConfig>) -> Result<Vec<WriteOp>, String> {
    let mut out = Vec::new();
    let t = form.value("threshold µs");
    if !t.is_empty() {
        let n = t
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= -1)
            .ok_or_else(|| "The threshold is a whole number of µs, -1 or more.".to_string())?;
        if known.is_none_or(|k| k.threshold_micros != n) {
            out.push(WriteOp::SlowlogConfig(SlowlogSetting::Threshold(n)));
        }
    }
    let m = form.value("max length");
    if !m.is_empty() {
        let n = m
            .parse::<u64>()
            .map_err(|_| "The length is a whole number of entries.".to_string())?;
        if known.is_none_or(|k| k.max_len != n) {
            out.push(WriteOp::SlowlogConfig(SlowlogSetting::MaxLen(n)));
        }
    }
    if out.is_empty() {
        return Err("Change the threshold or the length first.".into());
    }
    Ok(out)
}

impl ServerState {
    /// Another node of the cluster was picked: what Overview, Clients and the Slow log
    /// show belonged to the last one, so it goes (the sparklines start again). A
    /// running Monitor stays on its node; the per-shard summary covers every node.
    /// The command key specs are the same on every node and are kept.
    pub fn node_changed(&mut self) {
        self.overview = Overview::default();
        self.clients = Clients::default();
        let specs = std::mem::take(&mut self.slow.specs);
        self.slow = SlowLog::default();
        self.slow.specs = specs;
    }

    /// The `CONFIG SET`s the form sends as filled now, as the prompt shows them, or
    /// what is wrong: for the form's "Sends:" line.
    pub fn form_sends(&self) -> Option<Result<String, String>> {
        let f = self.form.as_ref()?;
        Some(slowlog_ops(f, self.slow.config).map(|ops| {
            ops.iter()
                .map(WriteOp::shown)
                .collect::<Vec<_>>()
                .join("; ")
        }))
    }

    /// `K`: kills the selected connection of the open group.
    fn kill(&mut self, act: &mut Act<'_>) {
        if self.clients.drill.is_none() {
            act.info("Press enter to list the group's connections, then K kills the selected one.");
            return;
        }
        let Some(c) = self.clients.selected_conn() else {
            act.info("No connection selected.");
            return;
        };
        let id = c.id;
        if is_own(c) {
            act.error(
                "Not sent: this looks like redis-lens's own connection (it was running CLIENT LIST).",
            );
            return;
        }
        self.send_write(WriteOp::ClientKill { id }, Sent::Kill(id), act);
    }

    /// `c` on the Slow log: opens the settings form.
    fn open_slowlog_form(&mut self, act: &mut Act<'_>) {
        let sample = WriteOp::SlowlogConfig(SlowlogSetting::Threshold(0));
        if write_off(act, &sample) {
            return;
        }
        let known = self.slow.config;
        let about = vec![match known {
            Some(_) => "The slow log's settings now are filled in. Change one or both.".to_string(),
            None => "The settings now are unknown. Fill in the ones to change.".to_string(),
        }];
        self.form = Some(Form::new(
            "Slow log settings",
            about,
            vec![
                Field::text(
                    "threshold µs",
                    "slowlog-log-slower-than: -1 turns the log off, 0 logs every command",
                )
                .with(known.map_or_else(String::new, |k| k.threshold_micros.to_string())),
                Field::text(
                    "max length",
                    "slowlog-max-len: the most entries the log keeps",
                )
                .with(known.map_or_else(String::new, |k| k.max_len.to_string())),
            ],
        ));
    }

    /// A key while the form is open.
    fn form_key(&mut self, key: KeyEvent, act: &mut Act<'_>) {
        let Some(f) = self.form.as_mut() else {
            return;
        };
        match f.key(key) {
            Step::Typed => {}
            Step::Cancel => self.form = None,
            Step::Submit => match slowlog_ops(f, self.slow.config) {
                Err(why) => act.error(why),
                Ok(mut ops) => {
                    self.form = None;
                    let first = ops.remove(0);
                    self.then = ops.pop();
                    self.send_write(first, Sent::Config, act);
                }
            },
        }
    }

    fn send_write(&mut self, op: WriteOp, sent: Sent, act: &mut Act<'_>) {
        match act.write(op) {
            Some(id) => self.write = Some((id, sent)),
            None => self.then = None,
        }
    }

    /// The answer to the write in flight; the panel reads again after it.
    fn write_reply(&mut self, sent: Sent, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        let reply = match result {
            Ok(LiveReply::Write(r)) => r,
            Ok(_) => return,
            Err(e) => {
                self.then = None;
                write_failed(act, &e);
                return;
            }
        };
        let panel = match sent {
            Sent::Kill(id) => {
                act.info(match reply {
                    WriteReply::Int(0) => format!("Connection {id} was already gone."),
                    _ => format!("Killed connection {id}."),
                });
                Panel::Clients
            }
            Sent::Config => {
                if let Some(next) = self.then.take() {
                    self.send_write(next, Sent::Config, act);
                    return;
                }
                act.info("Slow log settings changed.");
                Panel::SlowLog
            }
            Sent::Reset => {
                act.info("Slow log reset.");
                Panel::SlowLog
            }
        };
        if !self.poll(panel).busy(act) {
            self.send(panel, act);
        }
    }

    /// How often the Overview polls.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(INTERVALS[self.interval])
    }

    fn every(&self, panel: Panel) -> Duration {
        match panel {
            Panel::Overview => self.interval(),
            Panel::Clients => CLIENTS_EVERY,
            Panel::SlowLog => SLOWLOG_EVERY,
            // Never polls ([`Self::due`]).
            Panel::Monitor => Duration::MAX,
        }
    }

    /// The polling of `panel`; for Monitor, its `COMMAND INFO` lookups.
    pub fn poll(&self, panel: Panel) -> &Poll {
        match panel {
            Panel::Overview => &self.overview.poll,
            Panel::Clients => &self.clients.poll,
            Panel::SlowLog => &self.slow.poll,
            Panel::Monitor => &self.monitor.lookup,
        }
    }

    fn poll_mut(&mut self, panel: Panel) -> &mut Poll {
        match panel {
            Panel::Overview => &mut self.overview.poll,
            Panel::Clients => &mut self.clients.poll,
            Panel::SlowLog => &mut self.slow.poll,
            Panel::Monitor => &mut self.monitor.lookup,
        }
    }

    /// Sends `panel`'s request, if the live connection is open.
    fn send(&mut self, panel: Panel, act: &mut Act<'_>) {
        let req = match panel {
            Panel::Overview => Request::Info,
            Panel::Clients => Request::Clients,
            Panel::SlowLog => Request::SlowLog {
                specs: self.slow.specs.clone(),
                config: self.slow.config_wanted,
                keys: self.slow.keys_wanted,
            },
            Panel::Monitor => return,
        };
        if let Some(id) = act.send(LiveRequest::Server(req)) {
            let p = self.poll_mut(panel);
            p.in_flight = Some(id);
            p.sent = act.now;
        }
    }

    fn due(&self, panel: Panel, act: &Act<'_>) -> bool {
        let p = self.poll(panel);
        if panel == Panel::Monitor || p.off.is_some() || p.busy(act) {
            return false;
        }
        match (act.now, p.sent) {
            (Some(now), Some(sent)) => now.saturating_duration_since(sent) >= self.every(panel),
            (_, None) => true,
            (None, Some(_)) => false,
        }
    }

    fn failed(&mut self, panel: Panel, e: &LiveError) {
        let p = self.poll_mut(panel);
        match e {
            LiveError::Denied(_) | LiveError::Unsupported(_) => p.off = Some(off_text(e)),
            LiveError::Closed => p.failed = Some("The live connection is closed.".to_string()),
            LiveError::Failed(_) => p.failed = Some(format!("The last poll failed: {e}.")),
        }
    }

    fn apply(&mut self, reply: Reply) {
        match reply {
            Reply::Pong => {}
            Reply::Info { at, sample } => {
                let max_gap = self.interval() * 3 + Duration::from_secs(1);
                self.overview.add(at, *sample, max_gap);
            }
            Reply::Clients(list) => self.clients.set(list),
            Reply::SlowLog(r) => self.slow.set(*r),
            // Only the Monitor panel asks for these: see [`Self::monitor_learned`].
            Reply::CommandKeys(found) => self.slow.specs.extend(found),
        }
    }

    fn change_interval(&mut self, faster: bool, act: &mut Act<'_>) {
        let next = if faster {
            self.interval.saturating_sub(1)
        } else {
            (self.interval + 1).min(INTERVALS.len() - 1)
        };
        if next == self.interval {
            let (how, secs) = if faster {
                ("fastest", INTERVALS[0])
            } else {
                ("slowest", INTERVALS[INTERVALS.len() - 1])
            };
            act.info(format!("Already polling every {secs} s, the {how}."));
        } else {
            self.interval = next;
            act.info(format!("Overview polls every {} s.", INTERVALS[next]));
        }
    }
}

/// Why a panel is off: the fixed wording of `e`, never the server's.
fn off_text(e: &LiveError) -> String {
    let t = e.to_string();
    let mut c = t.chars();
    let head = c.next().map(|f| f.to_uppercase().collect::<String>());
    format!(
        "{}{}, so this panel is off.",
        head.unwrap_or_default(),
        c.as_str()
    )
}

impl ViewState for ServerState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        let Some(panel) = Panel::of(act.screen) else {
            return;
        };
        // Paused: fetch only what has never been fetched.
        if !(self.paused && self.poll(panel).got) && self.due(panel, act) {
            self.send(panel, act);
        }
        if panel == Panel::Overview && !self.paused {
            let every = self.interval().max(SHARDS_EVERY);
            self.shards.poll(every, act);
        }
    }

    fn on_tick(&mut self, now: Instant, act: &mut Act<'_>) {
        self.monitor_tick(now, act);
        let Some(panel) = Panel::of(act.screen) else {
            return;
        };
        if !self.paused && act.live_ready() && self.due(panel, act) {
            self.send(panel, act);
        }
        if panel == Panel::Overview && !self.paused {
            let every = self.interval().max(SHARDS_EVERY);
            self.shards.poll(every, act);
        }
    }

    fn refresh(&mut self, act: &mut Act<'_>) {
        let Some(panel) = Panel::of(act.screen) else {
            return;
        };
        // The Monitor panel does not poll: `r` is not one of its keys.
        if panel == Panel::Monitor {
            return;
        }
        // `r` also tries again a panel that went off: the ACL may have changed.
        let p = self.poll_mut(panel);
        p.off = None;
        p.failed = None;
        if !p.busy(act) {
            self.send(panel, act);
        }
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if self.form.is_some() {
            self.form_key(key, act);
            return true;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let Some(panel) = Panel::of(act.screen) else {
            return false;
        };
        if panel == Panel::Monitor {
            return self.monitor_key(key, act);
        }
        match (panel, key.code) {
            (_, KeyCode::Char('p')) => {
                self.paused = !self.paused;
                if self.paused {
                    act.info("Polling paused. r fetches once.");
                } else {
                    act.info("Polling resumed.");
                    if !self.poll(panel).busy(act) && self.poll(panel).off.is_none() {
                        self.send(panel, act);
                    }
                }
                true
            }
            (Panel::Overview, KeyCode::Char('+')) => {
                self.change_interval(true, act);
                true
            }
            (Panel::Overview, KeyCode::Char('-')) => {
                self.change_interval(false, act);
                true
            }
            (Panel::Clients, KeyCode::Char('K')) => {
                self.kill(act);
                true
            }
            (Panel::SlowLog, KeyCode::Char('c')) => {
                self.open_slowlog_form(act);
                true
            }
            (Panel::SlowLog, KeyCode::Char('R')) => {
                self.send_write(WriteOp::SlowlogReset, Sent::Reset, act);
                true
            }
            (Panel::Clients, _) => self.clients.key(key, act.page),
            (Panel::SlowLog, _) => self.slow.key(key, act.page),
            _ => false,
        }
    }

    fn input(&self) -> Option<Input> {
        match &self.form {
            Some(f) => f.input(),
            None => self.monitor_input(),
        }
    }

    fn on_paste(&mut self, text: &str, _act: &mut Act<'_>) {
        if let Some(f) = self.form.as_mut() {
            f.paste(text);
            return;
        }
        if let Some(Prompt::TypeName(t)) = self.monitor.prompt.as_mut() {
            let room = MAX_TYPED.saturating_sub(t.chars().count());
            t.extend(text.chars().filter(|c| !c.is_control()).take(room));
        }
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        if let Some((wid, sent)) = self.write
            && wid == id
        {
            self.write = None;
            self.write_reply(sent, result, act);
            return;
        }
        if self.shards.reply(id, &result) {
            return;
        }
        let Some(panel) = Panel::ALL
            .into_iter()
            .find(|p| self.poll(*p).in_flight == Some(id))
        else {
            return;
        };
        let now = act.now;
        let p = self.poll_mut(panel);
        p.in_flight = None;
        p.sent = p.sent.or(now);
        if panel == Panel::Monitor {
            self.monitor_learned(result, act);
            return;
        }
        match result {
            Ok(LiveReply::Server(reply)) => {
                p.failed = None;
                p.got = true;
                self.apply(reply);
            }
            Ok(_) => {}
            Err(e) => self.failed(panel, &e),
        }
    }
}

/// One panel's polling: what is in flight, when it was sent, and what went wrong.
#[derive(Debug, Default)]
pub struct Poll {
    in_flight: Option<ReqId>,
    sent: Option<Instant>,
    /// Why the panel is off, as a sentence in redis-lens's own words. No more polls.
    pub off: Option<String>,
    /// The last poll failed; the panel keeps what it had.
    pub failed: Option<String>,
    /// An answer has come in.
    pub got: bool,
}

impl Poll {
    fn busy(&self, act: &Act<'_>) -> bool {
        self.in_flight.is_some_and(|id| act.in_flight(id))
    }
}

/// Moves a selection by a list key. `None` for a key that is not one.
pub fn move_selection(sel: usize, len: usize, key: KeyEvent, page: usize) -> Option<usize> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    let last = len.saturating_sub(1);
    let page = page.max(1);
    Some(match key.code {
        KeyCode::Up | KeyCode::Char('k') => sel.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => sel.saturating_add(1).min(last),
        KeyCode::PageUp => sel.saturating_sub(page),
        KeyCode::PageDown => sel.saturating_add(page).min(last),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

/// The Overview: the last `INFO` and one [`Point`] per poll.
#[derive(Debug, Default)]
pub struct Overview {
    pub poll: Poll,
    pub last: Option<InfoSample>,
    last_at: Option<Instant>,
    /// Oldest first, at most [`HISTORY`].
    pub history: VecDeque<Point>,
}

impl Overview {
    fn add(&mut self, at: Instant, sample: InfoSample, max_gap: Duration) {
        let prev = self.last_at.zip(self.last.as_ref());
        let p = point(prev, at, &sample, max_gap);
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(p);
        self.last = Some(sample);
        self.last_at = Some(at);
    }

    /// The newest point.
    pub fn now(&self) -> Option<&Point> {
        self.history.back()
    }
}

/// What one poll adds to the Overview: gauges as they were, and rates over the time
/// since the previous poll. A rate that cannot be told is `None` (a gap in the line).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Point {
    pub ops: Option<f64>,
    pub used_memory: Option<u64>,
    /// Hits over hits plus misses since the previous poll, 0.0 to 1.0.
    pub hit_rate: Option<f64>,
    pub clients: Option<u64>,
    pub blocked: Option<u64>,
    pub net_in: Option<f64>,
    pub net_out: Option<f64>,
    pub evicted: Option<f64>,
    pub expired: Option<f64>,
    /// Percent of one core.
    pub cpu_user: Option<f64>,
    pub cpu_sys: Option<f64>,
    pub fragmentation: Option<f64>,
}

/// The things the Overview plots, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Ops,
    Memory,
    HitRate,
    Clients,
    Blocked,
    NetIn,
    NetOut,
    Evicted,
    Expired,
    CpuUser,
    CpuSys,
    Fragmentation,
}

impl Metric {
    pub const ALL: [Metric; 12] = [
        Metric::Ops,
        Metric::Memory,
        Metric::HitRate,
        Metric::Clients,
        Metric::Blocked,
        Metric::NetIn,
        Metric::NetOut,
        Metric::Evicted,
        Metric::Expired,
        Metric::CpuUser,
        Metric::CpuSys,
        Metric::Fragmentation,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ops => "ops/s",
            Self::Memory => "memory",
            Self::HitRate => "hit rate",
            Self::Clients => "clients",
            Self::Blocked => "blocked",
            Self::NetIn => "net in",
            Self::NetOut => "net out",
            Self::Evicted => "evicted/s",
            Self::Expired => "expired/s",
            Self::CpuUser => "cpu user",
            Self::CpuSys => "cpu sys",
            Self::Fragmentation => "frag",
        }
    }

    /// The value this metric plots from `p`.
    pub fn value(self, p: &Point) -> Option<f64> {
        match self {
            Self::Ops => p.ops,
            Self::Memory => p.used_memory.map(|v| v as f64),
            Self::HitRate => p.hit_rate,
            Self::Clients => p.clients.map(|v| v as f64),
            Self::Blocked => p.blocked.map(|v| v as f64),
            Self::NetIn => p.net_in,
            Self::NetOut => p.net_out,
            Self::Evicted => p.evicted,
            Self::Expired => p.expired,
            Self::CpuUser => p.cpu_user,
            Self::CpuSys => p.cpu_sys,
            Self::Fragmentation => p.fragmentation,
        }
    }
}

/// The sparkline of `metric` over the last `width` polls (at most [`HISTORY`]), oldest
/// first. All zeros is flat at the bottom; all equal and above zero, in the middle.
pub fn sparkline(history: &VecDeque<Point>, metric: Metric, width: usize) -> String {
    let width = width.min(HISTORY);
    let skip = history.len().saturating_sub(width);
    let values: Vec<Option<f64>> = history.iter().skip(skip).map(|p| metric.value(p)).collect();
    if values.iter().flatten().all(|v| *v <= 0.0) && values.iter().any(Option::is_some) {
        return values
            .iter()
            .map(|v| {
                if v.is_some() {
                    crate::format::SPARK_LEVELS[0]
                } else {
                    crate::format::SPARK_GAP
                }
            })
            .collect();
    }
    let scaled: Vec<Option<u64>> = values
        .iter()
        .map(|v| v.map(|v| (v.max(0.0) * 1000.0).round().min(1.0e18) as u64))
        .collect();
    crate::format::sparkline(&scaled)
}

/// Events per second between two counter readings `dt` apart. `None` when either is
/// missing, when the counter went down (the server restarted, or `CONFIG RESETSTAT`), or
/// when `dt` is too short to tell.
pub fn rate(prev: Option<u64>, cur: Option<u64>, dt: Duration) -> Option<f64> {
    let (p, c) = (prev?, cur?);
    (c >= p && dt >= MIN_DT).then(|| (c - p) as f64 / dt.as_secs_f64())
}

/// [`rate`] for fractional counters (CPU seconds).
pub fn rate_f(prev: Option<f64>, cur: Option<f64>, dt: Duration) -> Option<f64> {
    let (p, c) = (prev?, cur?);
    (c >= p && dt >= MIN_DT).then(|| (c - p) / dt.as_secs_f64())
}

/// The [`Point`] for `cur`, polled at `at`, from the previous poll `prev`. Rates are
/// `None` without a previous poll, after a restart (uptime went down), or when more than
/// `max_gap` passed (the panel was away).
pub fn point(
    prev: Option<(Instant, &InfoSample)>,
    at: Instant,
    cur: &InfoSample,
    max_gap: Duration,
) -> Point {
    let mut p = Point {
        used_memory: cur.used_memory,
        clients: cur.clients,
        blocked: cur.blocked,
        fragmentation: cur.fragmentation,
        ..Point::default()
    };
    let Some((then, old)) = prev else {
        return p;
    };
    let dt = at.saturating_duration_since(then);
    let restarted = matches!((old.uptime_secs, cur.uptime_secs), (Some(a), Some(b)) if b < a);
    if restarted || dt > max_gap {
        return p;
    }
    p.ops = rate(old.commands, cur.commands, dt);
    p.net_in = rate(old.net_in, cur.net_in, dt);
    p.net_out = rate(old.net_out, cur.net_out, dt);
    p.evicted = rate(old.evicted, cur.evicted, dt);
    p.expired = rate(old.expired, cur.expired, dt);
    p.cpu_user = rate_f(old.cpu_user, cur.cpu_user, dt).map(|v| v * 100.0);
    p.cpu_sys = rate_f(old.cpu_sys, cur.cpu_sys, dt).map(|v| v * 100.0);
    if let (Some(h0), Some(m0), Some(h1), Some(m1)) = (old.hits, old.misses, cur.hits, cur.misses)
        && h1 >= h0
        && m1 >= m0
    {
        let total = (h1 - h0) + (m1 - m0);
        p.hit_rate = (total > 0).then(|| (h1 - h0) as f64 / total as f64);
    }
    p
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// What a group of connections has in common.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupKey {
    /// `lib-name lib-ver`, else the client name, else `(no name)`.
    Lib(String),
    /// The /24 (IPv4) or /64 (IPv6) the client connects from.
    Net(String),
}

impl GroupKey {
    pub fn label(&self) -> &str {
        match self {
            Self::Lib(s) | Self::Net(s) => s,
        }
    }

    /// `lib` or `addr`: the kind, for the table.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Lib(_) => "lib",
            Self::Net(_) => "addr",
        }
    }
}

/// A group of connections and what they add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub key: GroupKey,
    /// Positions in [`Clients::conns`].
    pub members: Vec<usize>,
    /// Seconds.
    pub oldest_age: u64,
    pub longest_idle: u64,
    /// Connections idle for [`IDLE_FLAG_SECS`] or more.
    pub idle_conns: usize,
    /// The most common last command; `NULL` (no command yet) only when nothing else.
    pub top_cmd: String,
    pub mem: u64,
}

impl Group {
    pub fn connections(&self) -> usize {
        self.members.len()
    }

    /// A connection in the group has been idle for an hour or more.
    pub fn flagged(&self) -> bool {
        self.idle_conns > 0
    }
}

/// `lib-name lib-ver` where the server sends them (Redis 7.2 and later), else the client
/// name, else `(no name)`.
pub fn lib_label(c: &ClientConn) -> String {
    let lib = match (c.lib_name.is_empty(), c.lib_ver.is_empty()) {
        (false, false) => format!("{} {}", c.lib_name, c.lib_ver),
        (false, true) => c.lib_name.clone(),
        _ => String::new(),
    };
    if !lib.is_empty() {
        lib
    } else if !c.name.is_empty() {
        c.name.clone()
    } else {
        "(no name)".to_string()
    }
}

/// The network a client address belongs to: `10.0.1.0/24` for IPv4, the /64 for IPv6.
pub fn net_label(addr: &str) -> String {
    if addr.starts_with('/') || addr.starts_with('@') {
        return "(unix socket)".to_string();
    }
    let host = match addr.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => addr.rsplit_once(':').map_or(addr, |(h, _)| h),
    };
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => net_v4(ip.octets()),
        Ok(IpAddr::V6(ip)) => match ip.to_ipv4_mapped() {
            Some(v4) => net_v4(v4.octets()),
            None => {
                let mut s = ip.segments();
                s[4..].fill(0);
                format!("{}/64", Ipv6Addr::from(s))
            }
        },
        Err(_) => "(unknown)".to_string(),
    }
}

fn net_v4(o: [u8; 4]) -> String {
    format!("{}.{}.{}.0/24", o[0], o[1], o[2])
}

/// Every connection sits in two groups: one by library or name, one by network. Library
/// groups come first, each kind largest first, ties by label.
pub fn build_groups(conns: &[ClientConn]) -> Vec<Group> {
    let mut by_key: HashMap<GroupKey, Vec<usize>> = HashMap::new();
    for (i, c) in conns.iter().enumerate() {
        by_key
            .entry(GroupKey::Lib(lib_label(c)))
            .or_default()
            .push(i);
        by_key
            .entry(GroupKey::Net(net_label(&c.addr)))
            .or_default()
            .push(i);
    }
    let mut groups: Vec<Group> = by_key
        .into_iter()
        .map(|(key, members)| {
            let of = |f: fn(&ClientConn) -> u64| members.iter().map(move |i| f(&conns[*i]));
            let mut cmds: HashMap<&str, usize> = HashMap::new();
            for i in &members {
                *cmds.entry(conns[*i].cmd.as_str()).or_default() += 1;
            }
            let top_cmd = cmds
                .iter()
                .filter(|(c, _)| !c.is_empty() && **c != "NULL")
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .or_else(|| cmds.iter().next())
                .map_or_else(String::new, |(c, _)| (*c).to_string());
            Group {
                oldest_age: of(|c| c.age).max().unwrap_or(0),
                longest_idle: of(|c| c.idle).max().unwrap_or(0),
                idle_conns: of(|c| c.idle).filter(|i| *i >= IDLE_FLAG_SECS).count(),
                mem: of(|c| c.mem).fold(0, u64::saturating_add),
                top_cmd,
                members,
                key,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        matches!(a.key, GroupKey::Net(_))
            .cmp(&matches!(b.key, GroupKey::Net(_)))
            .then(b.connections().cmp(&a.connections()))
            .then_with(|| a.key.label().cmp(b.key.label()))
    });
    groups
}

/// The Clients panel: the groups, and the connections of one group.
#[derive(Debug, Default)]
pub struct Clients {
    pub poll: Poll,
    pub conns: Vec<ClientConn>,
    pub groups: Vec<Group>,
    /// Selected group.
    pub sel: usize,
    /// The group whose connections are listed (`enter`), until `esc`.
    pub drill: Option<GroupKey>,
    /// Selected connection of the open group.
    pub conn_sel: usize,
}

impl Clients {
    fn set(&mut self, list: Vec<ClientConn>) {
        let group = self.selected_group().map(|g| g.key.clone());
        let conn = self.selected_conn().map(|c| c.id);
        self.conns = list;
        self.groups = build_groups(&self.conns);
        if let Some(i) = group.and_then(|k| self.groups.iter().position(|g| g.key == k)) {
            self.sel = i;
        }
        self.sel = self.sel.min(self.groups.len().saturating_sub(1));
        if let Some(i) = conn.and_then(|id| self.open_conns().iter().position(|c| c.id == id)) {
            self.conn_sel = i;
        }
        self.conn_sel = self.conn_sel.min(self.open_conns().len().saturating_sub(1));
    }

    pub fn selected_group(&self) -> Option<&Group> {
        self.groups.get(self.sel)
    }

    /// The group `enter` opened, if it still exists.
    pub fn open_group(&self) -> Option<&Group> {
        let key = self.drill.as_ref()?;
        self.groups.iter().find(|g| &g.key == key)
    }

    /// The connections of the open group, longest idle first.
    pub fn open_conns(&self) -> Vec<&ClientConn> {
        let Some(g) = self.open_group() else {
            return Vec::new();
        };
        let mut v: Vec<&ClientConn> = g.members.iter().map(|i| &self.conns[*i]).collect();
        v.sort_by(|a, b| b.idle.cmp(&a.idle).then(a.id.cmp(&b.id)));
        v
    }

    pub fn selected_conn(&self) -> Option<&ClientConn> {
        self.open_conns().get(self.conn_sel).copied()
    }

    fn key(&mut self, key: KeyEvent, page: usize) -> bool {
        if self.drill.is_some() {
            if key.code == KeyCode::Esc {
                self.drill = None;
                return true;
            }
            let len = self.open_conns().len();
            return move_selection(self.conn_sel, len, key, page).is_some_and(|s| {
                self.conn_sel = s;
                true
            });
        }
        if key.code == KeyCode::Enter {
            if let Some(g) = self.selected_group() {
                self.drill = Some(g.key.clone());
                self.conn_sel = 0;
                return true;
            }
            return false;
        }
        move_selection(self.sel, self.groups.len(), key, page).is_some_and(|s| {
            self.sel = s;
            true
        })
    }
}

// ---------------------------------------------------------------------------
// Slow log
// ---------------------------------------------------------------------------

/// A known problem with a slow command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    Keys,
    Flush,
    WholeRead,
}

impl Note {
    pub fn text(self) -> &'static str {
        match self {
            Self::Keys => "KEYS walks every key and blocks the server. Use SCAN instead.",
            Self::Flush => "Deletes every key. Check who ran it and why.",
            Self::WholeRead => {
                "Whole-key read that was slow: the key is probably big. Read it in pages."
            }
        }
    }
}

/// The note for `e`, if its command is a known problem.
pub fn note_of(e: &SlowEntry) -> Option<Note> {
    match e.command.split(' ').next().unwrap_or("") {
        "KEYS" => Some(Note::Keys),
        "FLUSHALL" | "FLUSHDB" => Some(Note::Flush),
        _ if e.whole_key_read => Some(Note::WholeRead),
        _ => None,
    }
}

/// The Slow log panel.
#[derive(Debug)]
pub struct SlowLog {
    pub poll: Poll,
    /// Newest first.
    pub entries: Vec<SlowEntry>,
    pub config: Option<SlowConfig>,
    /// Why the threshold and length are unknown, when they are.
    pub config_note: Option<String>,
    /// Why key names are missing, when they are.
    pub keys_note: Option<String>,
    /// Key specs by command name, learned from `COMMAND INFO`.
    specs: HashMap<String, CommandKeys>,
    config_wanted: bool,
    keys_wanted: bool,
    pub sel: usize,
}

impl Default for SlowLog {
    fn default() -> Self {
        Self {
            poll: Poll::default(),
            entries: Vec::new(),
            config: None,
            config_note: None,
            keys_note: None,
            specs: HashMap::new(),
            config_wanted: true,
            keys_wanted: true,
            sel: 0,
        }
    }
}

impl SlowLog {
    fn set(&mut self, r: SlowLogReply) {
        let anchor = self.entries.get(self.sel).map(|e| e.id);
        self.specs.extend(r.learned);
        match r.config {
            Some(Ok(c)) => {
                self.config = Some(c);
                self.config_note = None;
            }
            Some(Err(e)) => {
                self.config_note = Some(format!("Threshold and length unknown: {e}."));
                if matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_)) {
                    self.config_wanted = false;
                }
            }
            None => {}
        }
        match r.keys_off {
            Some(e) => {
                if matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_)) {
                    self.keys_wanted = false;
                }
                self.keys_note = Some(format!("Key names are off: {e}."));
            }
            // Asked for and found: whatever stopped them before is over.
            None if self.keys_wanted => self.keys_note = None,
            // Not asked for, because they were refused: the reason stays on screen.
            None => {}
        }
        self.entries = r.entries;
        if let Some(i) = anchor.and_then(|id| self.entries.iter().position(|e| e.id == id)) {
            self.sel = i;
        }
        self.sel = self.sel.min(self.entries.len().saturating_sub(1));
    }

    pub fn selected(&self) -> Option<&SlowEntry> {
        self.entries.get(self.sel)
    }

    fn key(&mut self, key: KeyEvent, page: usize) -> bool {
        move_selection(self.sel, self.entries.len(), key, page).is_some_and(|s| {
            self.sel = s;
            true
        })
    }
}

// ---------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------

/// How many `COMMAND INFO` rounds a ranking asks at most (names, then containers'
/// subcommands, 64 names a round).
const MAX_LOOKUPS: u8 = 8;
/// The longest profile name the prompt takes.
const MAX_TYPED: usize = 256;

/// What the prompt before a run asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// `y` or `n`.
    YesNo,
    /// On a `prod` profile: the profile name, typed so far.
    TypeName(String),
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `esc`.
    Esc,
    /// [`MAX_TIME`] passed.
    TimeUp,
    /// [`MAX_LINES`] lines arrived.
    LineCap,
    /// Its connection was lost.
    Lost,
    /// The session's live connection closed.
    Closed,
    /// A rescan replaced the session's run.
    Rescan,
    /// The connection is no longer listed.
    Unlisted,
    /// It could not start: why, in redis-lens's own words.
    Failed(String),
}

impl Stop {
    /// One sentence for the panel.
    pub fn text(&self) -> String {
        match self {
            Self::Esc => "Stopped with esc.".to_string(),
            Self::TimeUp => format!("Stopped after {} s.", MAX_TIME.as_secs()),
            Self::LineCap => format!(
                "Stopped after {} lines.",
                crate::format::grouped(MAX_LINES as u64)
            ),
            Self::Lost => "Stopped: the connection to the server was lost.".to_string(),
            Self::Closed => "Stopped: the live connection closed.".to_string(),
            Self::Rescan => "Stopped by the rescan.".to_string(),
            Self::Unlisted => "Stopped: this connection is no longer listed.".to_string(),
            Self::Failed(why) => format!("MONITOR did not start: {why}."),
        }
    }
}

/// The Hot keys ranking of a finished run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorHot {
    pub ranking: Ranking,
    /// How long the run ran, in whole seconds.
    pub secs: u64,
    /// When it started, UTC.
    pub at_unix: i64,
    /// Why some keys could not be placed, when `COMMAND INFO` was refused.
    pub keys_note: Option<String>,
}

/// Where the ranking of the last finished run is.
#[derive(Debug, Default)]
enum Rank {
    #[default]
    None,
    /// Waiting for `COMMAND INFO`; `rounds` asked so far.
    Pending {
        rounds: u8,
    },
    Done(MonitorHot),
}

/// The Monitor panel.
#[derive(Debug, Default)]
pub struct Monitor {
    /// The prompt is open.
    pub prompt: Option<Prompt>,
    /// The run started and not ended yet (it may not be ready).
    pub run: Option<u64>,
    /// The run's connection answered: lines are coming.
    pub ready: bool,
    started: Option<Instant>,
    ended: Option<Instant>,
    started_unix: Option<i64>,
    /// How the last run ended.
    pub stopped: Option<Stop>,
    /// The last run's lines, oldest first, at most [`MAX_LINES`]. Values: on screen only.
    pub lines: Vec<Entry>,
    pub sel: usize,
    /// The selection follows the newest line.
    pub follow: bool,
    /// The `COMMAND INFO` lookups of the ranking.
    lookup: Poll,
    rank: Rank,
    /// On a cluster, the node the last run went to: it stays there until it ends.
    pub node: Option<crate::model::Endpoint>,
}

impl Monitor {
    /// The ranking of the last finished run, once its keys are placed.
    pub fn ranking(&self) -> Option<&MonitorHot> {
        match &self.rank {
            Rank::Done(h) => Some(h),
            _ => None,
        }
    }

    /// The last run's keys are being placed (`COMMAND INFO` is out).
    pub fn ranking_pending(&self) -> bool {
        matches!(self.rank, Rank::Pending { .. })
    }

    /// A run was started and has not ended.
    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// Time left in the run, once it is ready and the clock has been read.
    pub fn left(&self, now: Option<Instant>) -> Option<Duration> {
        let started = self.started?;
        Some(MAX_TIME.saturating_sub(now?.saturating_duration_since(started)))
    }

    /// How long the run ran (or has run so far), capped at [`MAX_TIME`].
    pub fn ran(&self, now: Option<Instant>) -> Duration {
        match (self.started, self.ended.or(now)) {
            (Some(s), Some(e)) => e.saturating_duration_since(s).min(MAX_TIME),
            _ => Duration::ZERO,
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.lines.get(self.sel)
    }

    fn push(&mut self, lines: Vec<Entry>) -> bool {
        let room = MAX_LINES.saturating_sub(self.lines.len());
        let full = lines.len() >= room;
        self.lines.extend(lines.into_iter().take(room));
        if self.follow {
            self.sel = self.lines.len().saturating_sub(1);
        }
        full
    }
}

impl ServerState {
    /// The input the prompt shows in the key bar.
    fn monitor_input(&self) -> Option<Input> {
        Some(match self.monitor.prompt.as_ref()? {
            Prompt::YesNo => Input {
                label: "MONITOR",
                text: String::new(),
                hint: "  slows the server · y start · n cancel",
            },
            Prompt::TypeName(t) => Input {
                label: "MONITOR",
                text: t.clone(),
                hint: "  type the profile name · enter start · esc cancel",
            },
        })
    }

    /// `enter` on the Monitor panel: opens the prompt, when a run can start.
    fn ask_monitor(&mut self, act: &mut Act<'_>) {
        if self.monitor.running() {
            act.info("MONITOR is already running. esc stops it.");
        } else if let Some(why) = crate::conn::monitor_unsupported(act.profile()) {
            act.info(why);
        } else if !act.live_ready() {
            act.info("The live connection is not open, so MONITOR cannot start.");
        } else {
            self.monitor.prompt = Some(if act.profile().env == Env::Prod {
                Prompt::TypeName(String::new())
            } else {
                Prompt::YesNo
            });
        }
    }

    fn start_monitor(&mut self, act: &mut Act<'_>) {
        let Some(run) = act.monitor() else {
            act.info("The live connection is not open, so MONITOR cannot start.");
            return;
        };
        let node = act.session.picked_node().map(|n| n.endpoint.clone());
        let m = &mut self.monitor;
        m.run = Some(run);
        m.node = node;
        m.ready = false;
        m.started = None;
        m.ended = None;
        m.started_unix = None;
        m.stopped = None;
        m.lines.clear();
        m.sel = 0;
        m.follow = true;
        act.info(format!(
            "MONITOR started: it stops after {} s or {} lines, or with esc.",
            MAX_TIME.as_secs(),
            crate::format::grouped(MAX_LINES as u64)
        ));
    }

    /// A key while the prompt is open, or on the Monitor panel.
    fn monitor_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if let Some(prompt) = self.monitor.prompt.as_mut() {
            match (prompt, key.code) {
                (_, KeyCode::Esc) | (Prompt::YesNo, KeyCode::Char('n' | 'N')) => {
                    self.monitor.prompt = None;
                    act.info("MONITOR not started.");
                }
                (Prompt::YesNo, KeyCode::Char('y' | 'Y')) => {
                    self.monitor.prompt = None;
                    self.start_monitor(act);
                }
                (Prompt::TypeName(t), KeyCode::Char(c)) => {
                    if t.chars().count() < MAX_TYPED {
                        t.push(c);
                    }
                }
                (Prompt::TypeName(t), KeyCode::Backspace) => {
                    t.pop();
                }
                (Prompt::TypeName(t), KeyCode::Enter) => {
                    let typed = std::mem::take(t);
                    self.monitor.prompt = None;
                    if typed == act.profile().name {
                        self.start_monitor(act);
                    } else {
                        act.error("MONITOR not started: that is not the profile name.");
                    }
                }
                _ => {}
            }
            // The prompt takes every key.
            return true;
        }
        match key.code {
            KeyCode::Enter => {
                self.ask_monitor(act);
                true
            }
            KeyCode::Esc => match self.monitor.run {
                Some(run) => {
                    act.stop_monitor(run);
                    act.info("MONITOR stopped.");
                    self.finish_monitor(Stop::Esc, Some(act));
                    true
                }
                None => false,
            },
            _ => {
                let m = &mut self.monitor;
                match move_selection(m.sel, m.lines.len(), key, act.page) {
                    Some(s) => {
                        m.sel = s;
                        m.follow = s + 1 >= m.lines.len();
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// The run `run` is ready: lines are coming.
    pub fn monitor_ready(&mut self, run: u64, at_unix_ms: i64, act: &mut Act<'_>) {
        if self.monitor.run != Some(run) {
            return;
        }
        self.monitor.ready = true;
        self.monitor.started = act.now;
        self.monitor.started_unix = Some(at_unix_ms.div_euclid(1000));
    }

    /// Lines of the run `run`. Past [`MAX_LINES`] the run stops.
    pub fn monitor_lines(&mut self, run: u64, lines: Vec<Entry>, act: &mut Act<'_>) {
        if self.monitor.run != Some(run) {
            return;
        }
        if self.monitor.push(lines) {
            act.stop_monitor(run);
            self.finish_monitor(Stop::LineCap, Some(act));
        }
    }

    /// The run `run` ended by itself, or could not start.
    pub fn monitor_closed(&mut self, run: u64, end: Result<End, String>, act: &mut Act<'_>) {
        if self.monitor.run != Some(run) {
            return;
        }
        let stop = match end {
            Ok(End::TimeUp) => Stop::TimeUp,
            Ok(End::LineCap) => Stop::LineCap,
            Ok(End::Lost | End::Gone) => Stop::Lost,
            Err(why) => Stop::Failed(why),
        };
        if matches!(stop, Stop::Failed(_)) {
            act.error(stop.text());
        }
        self.finish_monitor(stop, Some(act));
    }

    /// Ends the run, if one runs, without asking for anything (a rescan, or the live
    /// connection closed). Returns its id, for the caller to stop it.
    pub fn end_monitor(&mut self, why: Stop) -> Option<u64> {
        self.monitor.prompt = None;
        let run = self.monitor.run?;
        self.finish_monitor(why, None);
        Some(run)
    }

    /// The time limit, checked on every tick whichever panel is shown.
    fn monitor_tick(&mut self, now: Instant, act: &mut Act<'_>) {
        if let (Some(run), Some(started)) = (self.monitor.run, self.monitor.started)
            && now.saturating_duration_since(started) >= MAX_TIME
        {
            act.stop_monitor(run);
            self.finish_monitor(Stop::TimeUp, Some(act));
        }
    }

    fn finish_monitor(&mut self, why: Stop, act: Option<&mut Act<'_>>) {
        let m = &mut self.monitor;
        m.run = None;
        m.ready = false;
        m.ended = act.as_ref().and_then(|a| a.now);
        m.stopped = Some(why);
        if m.started.is_none() {
            // It never ran: the last ranking stays the last run's.
            return;
        }
        m.rank = Rank::Pending { rounds: 0 };
        match act {
            Some(act) => self.monitor_lookup(act),
            None => self.monitor_rank(),
        }
    }

    /// Asks `COMMAND INFO` for the commands of the last run whose key specs the cache
    /// lacks, or ranks the keys when there are none (or they may not be asked).
    fn monitor_lookup(&mut self, act: &mut Act<'_>) {
        let rounds = match self.monitor.rank {
            Rank::Pending { rounds } => rounds,
            _ => return,
        };
        let names = names_to_learn(
            &self.slow.specs,
            self.monitor.lines.iter().map(|e| e.line.args.as_slice()),
        );
        if names.is_empty() || !self.slow.keys_wanted || rounds >= MAX_LOOKUPS {
            self.monitor_rank();
            return;
        }
        match act.send(LiveRequest::Server(Request::CommandKeys { names })) {
            Some(id) => {
                self.monitor.lookup.in_flight = Some(id);
                self.monitor.rank = Rank::Pending { rounds: rounds + 1 };
            }
            None => self.monitor_rank(),
        }
    }

    /// The answer to a lookup: learn it, then ask again or rank.
    fn monitor_learned(&mut self, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        match result {
            Ok(LiveReply::Server(reply)) => self.apply(reply),
            Ok(_) => {}
            Err(e) => {
                if matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_)) {
                    self.slow.keys_wanted = false;
                    self.slow.keys_note = Some(format!("Key names are off: {e}."));
                }
                self.monitor_rank();
                return;
            }
        }
        self.monitor_lookup(act);
    }

    /// Ranks the last run's keys with what the cache knows now.
    fn monitor_rank(&mut self) {
        let m = &mut self.monitor;
        let ranking = monitor::rank(&m.lines, &self.slow.specs);
        let keys_note = (ranking.unknown > 0)
            .then(|| self.slow.keys_note.clone())
            .flatten();
        m.rank = Rank::Done(MonitorHot {
            ranking,
            secs: m.ran(None).as_secs(),
            at_unix: m.started_unix.unwrap_or(0),
            keys_note,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::testing::{self, key};
    use super::*;
    use crate::live::server::{Begin, Find, KeySpec, Role};
    use crate::tui::app::{App, Effect, Msg, Screen};

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A scanned app with the live connection ready and a clock, on `screen`.
    fn app_at(t0: Instant, screen: Screen) -> (App, Vec<Effect>) {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        app.apply(Msg::TickAt(t0));
        let out = testing::show(&mut app, screen);
        (app, out)
    }

    fn only_request(effects: &[Effect]) -> (ReqId, Request) {
        let reqs = testing::requests(effects);
        assert_eq!(reqs.len(), 1, "{effects:?}");
        let (id, LiveRequest::Server(r)) = reqs.into_iter().next().unwrap() else {
            panic!("not a server request: {effects:?}");
        };
        (id, r)
    }

    /// The server requests among `effects`, in order.
    fn server_requests(effects: &[Effect]) -> Vec<(ReqId, Request)> {
        testing::requests(effects)
            .into_iter()
            .map(|(id, r)| {
                let LiveRequest::Server(r) = r else {
                    panic!("not a server request: {r:?}");
                };
                (id, r)
            })
            .collect()
    }

    /// The last server request: when `show` tabs through panels to get to one, each
    /// panel on the way polls, and the one wanted is the last.
    fn last_request(effects: &[Effect]) -> (ReqId, Request) {
        server_requests(effects).pop().expect("a request")
    }

    fn answer(app: &mut App, id: ReqId, reply: Reply) -> Vec<Effect> {
        testing::reply(app, id, Ok(LiveReply::Server(reply)))
    }

    fn state(app: &App) -> &ServerState {
        &app.views.server
    }

    /// `sample(t)` is an INFO taken `t` seconds into a steady run: 100 commands, 1 KiB
    /// in and 2 KiB out a second, 0.05 s of user CPU a second.
    fn sample(t: u64) -> InfoSample {
        InfoSample {
            uptime_secs: Some(1000 + t),
            role: Some(Role::Primary),
            replicas: Some(1),
            clients: Some(10 + t % 3),
            blocked: Some(0),
            used_memory: Some(1_000_000 + t * 1000),
            maxmemory: Some(4_000_000),
            fragmentation: Some(1.1),
            policy: Some("allkeys-lfu".into()),
            commands: Some(10_000 + 100 * t),
            hits: Some(900 + 9 * t),
            misses: Some(100 + t),
            net_in: Some(50_000 + 1024 * t),
            net_out: Some(80_000 + 2048 * t),
            evicted: Some(t / 2),
            expired: Some(5 * t),
            cpu_user: Some(10.0 + 0.05 * t as f64),
            cpu_sys: Some(4.0 + 0.01 * t as f64),
            rdb_ok: Some(true),
            aof_on: Some(false),
            ..InfoSample::default()
        }
    }

    // -- rate math -----------------------------------------------------------

    #[test]
    fn rates_come_from_deltas() {
        assert_eq!(rate(Some(100), Some(300), secs(2)), Some(100.0));
        assert_eq!(rate(Some(5), Some(5), secs(2)), Some(0.0));
        // The counter went down (restart, RESETSTAT), or is missing, or dt is tiny.
        assert_eq!(rate(Some(300), Some(100), secs(2)), None);
        assert_eq!(rate(None, Some(100), secs(2)), None);
        assert_eq!(rate(Some(1), None, secs(2)), None);
        assert_eq!(rate(Some(1), Some(9), Duration::from_millis(10)), None);
        assert_eq!(
            rate_f(Some(1.0), Some(1.5), Duration::from_millis(500)),
            Some(1.0)
        );
    }

    #[test]
    fn a_point_has_gauges_at_once_and_rates_from_the_second_poll() {
        let t0 = Instant::now();
        let (a, b) = (sample(0), sample(2));
        let first = point(None, t0, &a, secs(7));
        assert_eq!(first.used_memory, Some(1_000_000));
        assert_eq!(first.clients, Some(10));
        assert_eq!(first.ops, None);
        assert_eq!(first.hit_rate, None);

        let p = point(Some((t0, &a)), t0 + secs(2), &b, secs(7));
        assert_eq!(p.ops, Some(100.0));
        assert_eq!(p.net_in, Some(1024.0));
        assert_eq!(p.net_out, Some(2048.0));
        assert_eq!(p.evicted, Some(0.5));
        assert_eq!(p.expired, Some(5.0));
        // 0.05 CPU seconds a second is 5 % of a core.
        assert!((p.cpu_user.unwrap() - 5.0).abs() < 1e-9);
        assert!((p.cpu_sys.unwrap() - 1.0).abs() < 1e-9);
        // 18 hits and 2 misses in the interval.
        assert!((p.hit_rate.unwrap() - 0.9).abs() < 1e-9);
        assert_eq!(p.used_memory, Some(1_002_000));
    }

    #[test]
    fn a_restart_or_a_long_gap_leaves_the_rates_out() {
        let t0 = Instant::now();
        let (a, b) = (sample(100), sample(0));
        // Uptime went down: the server restarted.
        let p = point(Some((t0, &a)), t0 + secs(2), &b, secs(7));
        assert_eq!(
            (p.ops, p.net_in, p.hit_rate, p.cpu_user),
            (None, None, None, None)
        );
        assert_eq!(p.clients, b.clients);
        // The panel was away for a minute.
        let (a, b) = (sample(0), sample(60));
        let p = point(Some((t0, &a)), t0 + secs(60), &b, secs(7));
        assert_eq!(p.ops, None);
        // No lookups in the interval: no hit rate, not 0 %.
        let mut c = sample(2);
        c.hits = sample(0).hits;
        c.misses = sample(0).misses;
        let p = point(Some((t0, &sample(0))), t0 + secs(2), &c, secs(7));
        assert_eq!(p.hit_rate, None);
        assert!(p.ops.is_some());
    }

    #[test]
    fn sparklines_scale_and_mark_gaps() {
        let mut h = VecDeque::new();
        for v in [0.0, 5.0, 10.0] {
            h.push_back(Point {
                ops: Some(v),
                ..Point::default()
            });
        }
        h.push_back(Point::default());
        assert_eq!(sparkline(&h, Metric::Ops, 60), "▁▅█·");
        // Only the newest `width` polls.
        assert_eq!(sparkline(&h, Metric::Ops, 3), "▁█·");
        // Idle is flat at the bottom, not in the middle.
        let idle: VecDeque<Point> = (0..3)
            .map(|_| Point {
                ops: Some(0.0),
                ..Point::default()
            })
            .collect();
        assert_eq!(sparkline(&idle, Metric::Ops, 60), "▁▁▁");
        assert_eq!(sparkline(&VecDeque::new(), Metric::Ops, 60), "");
    }

    // -- polling -------------------------------------------------------------

    #[test]
    fn each_panel_asks_for_its_own_data_when_shown() {
        let t0 = Instant::now();
        let (_, out) = app_at(t0, Screen::Overview);
        assert_eq!(only_request(&out).1, Request::Info);
        let (_, out) = app_at(t0, Screen::Clients);
        assert_eq!(last_request(&out).1, Request::Clients);
        let (_, out) = app_at(t0, Screen::SlowLog);
        assert_eq!(
            last_request(&out).1,
            Request::SlowLog {
                specs: HashMap::new(),
                config: true,
                keys: true
            }
        );
    }

    #[test]
    fn the_overview_polls_every_two_seconds_and_only_when_shown() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Overview);
        let (id, _) = only_request(&out);
        // Nothing more while the first poll is out.
        assert!(testing::requests(&app.apply(Msg::TickAt(t0 + secs(5)))).is_empty());
        answer(
            &mut app,
            id,
            Reply::Info {
                at: t0,
                sample: Box::new(sample(0)),
            },
        );
        assert!(
            testing::requests(&app.apply(Msg::TickAt(t0 + Duration::from_millis(1900)))).is_empty()
        );
        let out = app.apply(Msg::TickAt(t0 + secs(2)));
        assert_eq!(only_request(&out).1, Request::Info);

        // On another view it asks for nothing.
        app.handle_key(ch('0'));
        assert!(testing::requests(&app.apply(Msg::TickAt(t0 + secs(30)))).is_empty());
    }

    #[test]
    fn the_other_panels_poll_every_five_and_ten_seconds() {
        let t0 = Instant::now();
        for (screen, every) in [(Screen::Clients, 5), (Screen::SlowLog, 10)] {
            let (mut app, out) = app_at(t0, screen);
            let (id, req) = last_request(&out);
            let reply = match req {
                Request::Clients => Reply::Clients(vec![]),
                _ => Reply::SlowLog(Box::new(SlowLogReply {
                    entries: vec![],
                    config: None,
                    keys_off: None,
                    learned: HashMap::new(),
                })),
            };
            answer(&mut app, id, reply);
            assert!(
                testing::requests(
                    &app.apply(Msg::TickAt(t0 + secs(every) - Duration::from_millis(100)))
                )
                .is_empty()
            );
            assert_eq!(
                testing::requests(&app.apply(Msg::TickAt(t0 + secs(every)))).len(),
                1
            );
        }
    }

    #[test]
    fn plus_and_minus_change_the_interval_from_one_to_sixty_seconds() {
        let t0 = Instant::now();
        let (mut app, _) = app_at(t0, Screen::Overview);
        assert_eq!(state(&app).interval(), secs(2));
        app.handle_key(ch('+'));
        assert_eq!(state(&app).interval(), secs(1));
        app.handle_key(ch('+'));
        assert_eq!(state(&app).interval(), secs(1));
        for _ in 0..20 {
            app.handle_key(ch('-'));
        }
        assert_eq!(state(&app).interval(), secs(60));
        app.handle_key(ch('-'));
        assert_eq!(state(&app).interval(), secs(60));
        app.handle_key(ch('+'));
        assert!(state(&app).interval() < secs(60));
        // They do nothing on the other panels.
        testing::show(&mut app, Screen::Clients);
        let before = state(&app).interval();
        app.handle_key(ch('-'));
        assert_eq!(state(&app).interval(), before);
    }

    #[test]
    fn p_pauses_polling_and_resuming_fetches_at_once() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Overview);
        let (id, _) = only_request(&out);
        answer(
            &mut app,
            id,
            Reply::Info {
                at: t0,
                sample: Box::new(sample(0)),
            },
        );
        assert!(app.handle_key(ch('p')).is_empty());
        assert!(state(&app).paused);
        assert!(testing::requests(&app.apply(Msg::TickAt(t0 + secs(20)))).is_empty());
        // `r` on Overview analyzes the keys: it asks first, and fetches nothing.
        assert!(testing::requests(&app.handle_key(ch('r'))).is_empty());
        assert!(app.analyze.is_some());
        app.handle_key(key(KeyCode::Esc));
        // Resuming asks at once when nothing is out.
        let out = app.handle_key(ch('p'));
        assert!(!state(&app).paused);
        assert_eq!(only_request(&out).1, Request::Info);
    }

    #[test]
    fn a_paused_panel_not_yet_fetched_fetches_once_when_shown() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Overview);
        let (id, _) = only_request(&out);
        answer(
            &mut app,
            id,
            Reply::Info {
                at: t0,
                sample: Box::new(sample(0)),
            },
        );
        app.handle_key(ch('p'));
        let out = testing::show(&mut app, Screen::Clients);
        assert_eq!(only_request(&out).1, Request::Clients);
        // Back on the Overview, which has data: no new INFO. (The Slow log, passed on
        // the way and never fetched, fetches once.)
        let out = testing::show(&mut app, Screen::Overview);
        assert!(
            !server_requests(&out)
                .iter()
                .any(|(_, r)| *r == Request::Info)
        );
    }

    #[test]
    fn history_keeps_the_last_sixty_polls() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Overview);
        let (mut id, _) = only_request(&out);
        for i in 0..70u64 {
            let at = t0 + secs(2 * i);
            answer(
                &mut app,
                id,
                Reply::Info {
                    at,
                    sample: Box::new(sample(2 * i)),
                },
            );
            let out = app.apply(Msg::TickAt(at + secs(2)));
            id = only_request(&out).0;
        }
        let o = &state(&app).overview;
        assert_eq!(o.history.len(), HISTORY);
        assert_eq!(o.now().unwrap().ops, Some(100.0));
        assert_eq!(
            o.history.front().unwrap().used_memory,
            Some(1_000_000 + 2 * 10 * 1000)
        );
    }

    // -- errors --------------------------------------------------------------

    #[test]
    fn a_refused_command_turns_off_its_panel_with_fixed_text() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Overview);
        let (id, _) = only_request(&out);
        testing::reply(&mut app, id, Err(LiveError::Denied("INFO".into())));
        let off = state(&app).overview.poll.off.clone().unwrap();
        assert_eq!(
            off,
            "The server refused INFO for this user, so this panel is off."
        );
        // No more polls, and the other panels still work.
        assert!(testing::requests(&app.apply(Msg::TickAt(t0 + secs(60)))).is_empty());
        let out = testing::show(&mut app, Screen::Clients);
        assert_eq!(only_request(&out).1, Request::Clients);
        // Shown again it stays off and sends nothing (`r` there analyzes the keys).
        let out = testing::show(&mut app, Screen::Overview);
        assert!(server_requests(&out).is_empty());
        assert!(state(&app).overview.poll.off.is_some());
    }

    #[test]
    fn an_unknown_command_turns_a_panel_off_and_a_failed_poll_does_not() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Clients);
        let (id, _) = last_request(&out);
        testing::reply(
            &mut app,
            id,
            Err(LiveError::Unsupported("CLIENT LIST".into())),
        );
        assert_eq!(
            state(&app).clients.poll.off.as_deref(),
            Some("The server does not support CLIENT LIST, so this panel is off.")
        );

        let (mut app, out) = app_at(t0, Screen::Overview);
        let (id, _) = only_request(&out);
        answer(
            &mut app,
            id,
            Reply::Info {
                at: t0,
                sample: Box::new(sample(0)),
            },
        );
        let out = app.apply(Msg::TickAt(t0 + secs(2)));
        let (id, _) = only_request(&out);
        testing::reply(&mut app, id, Err(LiveError::Failed("INFO failed".into())));
        let o = &state(&app).overview;
        assert!(o.poll.off.is_none());
        assert_eq!(
            o.poll.failed.as_deref(),
            Some("The last poll failed: INFO failed.")
        );
        assert!(o.last.is_some(), "the last data stays");
        // It tries again at the next interval, and a good answer clears the line.
        let out = app.apply(Msg::TickAt(t0 + secs(4)));
        let (id, _) = only_request(&out);
        answer(
            &mut app,
            id,
            Reply::Info {
                at: t0 + secs(4),
                sample: Box::new(sample(4)),
            },
        );
        assert!(state(&app).overview.poll.failed.is_none());
    }

    // -- clients -------------------------------------------------------------

    fn conn(
        id: u64,
        addr: &str,
        name: &str,
        lib: (&str, &str),
        idle: u64,
        cmd: &str,
    ) -> ClientConn {
        ClientConn {
            id,
            addr: addr.into(),
            name: name.into(),
            lib_name: lib.0.into(),
            lib_ver: lib.1.into(),
            age: idle + 10,
            idle,
            cmd: cmd.into(),
            mem: 1000 * id,
            db: Some(0),
        }
    }

    pub(crate) fn fleet() -> Vec<ClientConn> {
        vec![
            conn(1, "10.0.1.5:50001", "", ("redis-py", "5.0.1"), 0, "get"),
            conn(2, "10.0.1.6:50002", "", ("redis-py", "5.0.1"), 4000, "get"),
            conn(3, "10.0.1.7:50003", "", ("redis-py", "5.0.1"), 2, "set"),
            conn(4, "10.0.2.9:50004", "", ("lettuce", "6.3"), 30, "hgetall"),
            conn(5, "10.0.2.9:50005", "billing", ("", ""), 5, "zadd"),
            conn(6, "[fd00:1:2:3::9]:50006", "", ("", ""), 1, "NULL"),
            conn(7, "127.0.0.1:50007", "", ("", ""), 0, "client|list"),
        ]
    }

    #[test]
    fn a_group_is_flagged_from_one_hour_idle() {
        for (idle, flagged) in [(IDLE_FLAG_SECS - 1, false), (IDLE_FLAG_SECS, true)] {
            let groups = build_groups(&[conn(1, "10.0.1.5:1", "", ("redis-py", "5"), idle, "get")]);
            assert!(
                groups.iter().all(|g| g.flagged() == flagged),
                "idle {idle}: {groups:?}"
            );
        }
    }

    #[test]
    fn clients_are_grouped_by_library_and_by_network() {
        let groups = build_groups(&fleet());
        let find = |k: GroupKey| groups.iter().find(|g| g.key == k).unwrap();
        let py = find(GroupKey::Lib("redis-py 5.0.1".into()));
        assert_eq!(py.connections(), 3);
        assert_eq!(py.oldest_age, 4010);
        assert_eq!(py.longest_idle, 4000);
        assert_eq!(py.idle_conns, 1);
        assert!(py.flagged());
        assert_eq!(py.top_cmd, "get");
        assert_eq!(py.mem, 6000);
        // The name stands in where there is no library; "(no name)" where neither.
        assert_eq!(find(GroupKey::Lib("billing".into())).connections(), 1);
        assert_eq!(find(GroupKey::Lib("(no name)".into())).connections(), 2);
        assert!(!find(GroupKey::Lib("lettuce 6.3".into())).flagged());
        // /24 for IPv4, /64 for IPv6.
        assert_eq!(find(GroupKey::Net("10.0.1.0/24".into())).connections(), 3);
        assert_eq!(find(GroupKey::Net("10.0.2.0/24".into())).connections(), 2);
        assert_eq!(
            find(GroupKey::Net("fd00:1:2:3::/64".into())).connections(),
            1
        );
        // A connection that has run nothing shows no command.
        assert_eq!(
            find(GroupKey::Net("fd00:1:2:3::/64".into())).top_cmd,
            "NULL"
        );
        // Libraries first, largest first; then networks.
        let kinds: Vec<_> = groups.iter().map(|g| g.key.kind()).collect();
        let first_net = kinds.iter().position(|k| *k == "addr").unwrap();
        assert!(kinds[..first_net].iter().all(|k| *k == "lib"));
        assert!(kinds[first_net..].iter().all(|k| *k == "addr"));
        assert_eq!(groups[0].key, GroupKey::Lib("redis-py 5.0.1".into()));
        // Every connection is in one group of each kind.
        let per_kind = |k: &str| {
            groups
                .iter()
                .filter(|g| g.key.kind() == k)
                .map(Group::connections)
                .sum::<usize>()
        };
        assert_eq!((per_kind("lib"), per_kind("addr")), (7, 7));
    }

    #[test]
    fn network_labels() {
        assert_eq!(net_label("10.1.2.3:6000"), "10.1.2.0/24");
        assert_eq!(net_label("[::1]:6000"), "::/64");
        assert_eq!(
            net_label("[2001:db8:1:2:3:4:5:6]:6000"),
            "2001:db8:1:2::/64"
        );
        assert_eq!(net_label("[::ffff:192.168.7.8]:1"), "192.168.7.0/24");
        assert_eq!(net_label("/tmp/redis.sock:0"), "(unix socket)");
        assert_eq!(net_label("@abstract:0"), "(unix socket)");
        assert_eq!(net_label("nonsense"), "(unknown)");
        assert_eq!(net_label(""), "(unknown)");
    }

    #[test]
    fn the_most_common_command_breaks_ties_by_name() {
        let c = |id, cmd: &str| conn(id, "1.2.3.4:1", "x", ("", ""), 0, cmd);
        let g = &build_groups(&[
            c(1, "set"),
            c(2, "get"),
            c(3, "NULL"),
            c(4, "NULL"),
            c(5, "NULL"),
        ])[0];
        assert_eq!(g.top_cmd, "get");
        let g = &build_groups(&[c(1, "NULL")])[0];
        assert_eq!(g.top_cmd, "NULL");
    }

    #[test]
    fn enter_lists_a_groups_connections_and_esc_goes_back() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Clients);
        let (id, _) = last_request(&out);
        answer(&mut app, id, Reply::Clients(fleet()));
        let c = &state(&app).clients;
        assert_eq!(c.groups.len(), 8);
        assert!(c.drill.is_none());

        // Move down and back up with the list keys.
        app.handle_key(ch('j'));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(state(&app).clients.sel, 2);
        app.handle_key(key(KeyCode::Home));
        assert_eq!(state(&app).clients.sel, 0);
        app.handle_key(ch('G'));
        let last = state(&app).clients.groups.len() - 1;
        assert_eq!(state(&app).clients.sel, last);
        app.handle_key(ch('g'));
        app.handle_key(key(KeyCode::PageDown));
        assert!(state(&app).clients.sel > 0);
        app.handle_key(key(KeyCode::PageUp));
        app.handle_key(ch('k'));
        app.handle_key(key(KeyCode::End));
        app.handle_key(ch('g'));

        // enter on the redis-py group lists its three connections, longest idle first.
        app.handle_key(key(KeyCode::Enter));
        let c = &state(&app).clients;
        assert_eq!(c.drill, Some(GroupKey::Lib("redis-py 5.0.1".into())));
        let ids: Vec<u64> = c.open_conns().iter().map(|c| c.id).collect();
        assert_eq!(ids, [2, 3, 1]);
        app.handle_key(ch('j'));
        assert_eq!(state(&app).clients.selected_conn().unwrap().id, 3);
        // enter does nothing deeper; esc goes back to the groups, on the same one.
        app.handle_key(key(KeyCode::Enter));
        assert!(state(&app).clients.drill.is_some());
        app.handle_key(key(KeyCode::Esc));
        let c = &state(&app).clients;
        assert!(c.drill.is_none());
        assert_eq!(c.sel, 0);
        // esc with nothing open is left to App.
        let mut st = ServerState::default();
        assert!(!st.clients.key(key(KeyCode::Esc), 5));
        assert!(!st.clients.key(key(KeyCode::Enter), 5), "no groups yet");
    }

    #[test]
    fn the_selection_follows_its_group_when_the_list_is_refreshed() {
        let mut c = Clients::default();
        c.set(fleet());
        let net = c
            .groups
            .iter()
            .position(|g| g.key == GroupKey::Net("10.0.2.0/24".into()))
            .unwrap();
        c.sel = net;
        c.drill = Some(GroupKey::Net("10.0.2.0/24".into()));
        c.conn_sel = 1;
        let picked = c.selected_conn().unwrap().id;
        // A new client in a bigger group moves everything down.
        let mut more = fleet();
        for id in 20..30 {
            more.push(conn(id, "10.0.9.1:1", "extra", ("", ""), 0, "get"));
        }
        c.set(more);
        assert_eq!(
            c.selected_group().unwrap().key,
            GroupKey::Net("10.0.2.0/24".into())
        );
        assert_eq!(c.selected_conn().unwrap().id, picked);
        // The group is gone: no connections, and the selection is in range.
        c.set(vec![]);
        assert!(c.open_group().is_none());
        assert_eq!(c.sel, 0);
        assert!(c.selected_conn().is_none());
    }

    // -- slow log ------------------------------------------------------------

    fn entry(id: u64, command: &str, keys: &[&str], micros: u64) -> SlowEntry {
        SlowEntry {
            id,
            unix: 1_700_000_000 + id as i64,
            micros,
            command: command.into(),
            keys: keys.iter().map(|k| (*k).into()).collect(),
            more_keys: 0,
            client: "10.0.1.5:50386".into(),
            client_name: "worker".into(),
            whole_key_read: false,
            args: crate::value::Raw::new(
                format!("{command} {}", keys.join(" "))
                    .trim_end()
                    .as_bytes()
                    .to_vec(),
            ),
            args_cut: 0,
        }
    }

    fn slow_reply(entries: Vec<SlowEntry>) -> SlowLogReply {
        SlowLogReply {
            entries,
            config: Some(Ok(SlowConfig {
                threshold_micros: 10_000,
                max_len: 128,
            })),
            keys_off: None,
            learned: HashMap::new(),
        }
    }

    #[test]
    fn notes_flag_the_known_problems() {
        assert_eq!(note_of(&entry(1, "KEYS", &[], 5)), Some(Note::Keys));
        assert_eq!(note_of(&entry(1, "FLUSHALL", &[], 5)), Some(Note::Flush));
        assert_eq!(note_of(&entry(1, "FLUSHDB", &[], 5)), Some(Note::Flush));
        assert_eq!(note_of(&entry(1, "GET", &["k"], 5)), None);
        let mut e = entry(1, "HGETALL", &["h"], 5);
        e.whole_key_read = true;
        assert_eq!(note_of(&e), Some(Note::WholeRead));
        for n in [Note::Keys, Note::Flush, Note::WholeRead] {
            assert!(n.text().len() < 100);
        }
    }

    #[test]
    fn learned_key_specs_are_sent_back_and_not_asked_for_again() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::SlowLog);
        let (id, _) = last_request(&out);
        let mut reply = slow_reply(vec![
            entry(2, "GET", &["a"], 20_000),
            entry(1, "KEYS", &[], 90_000),
        ]);
        reply.learned.insert(
            "get".into(),
            CommandKeys::Specs(vec![KeySpec {
                begin: Begin::Index(1),
                find: Find::Range {
                    last: 0,
                    step: 1,
                    limit: 0,
                },
            }]),
        );
        answer(&mut app, id, Reply::SlowLog(Box::new(reply)));
        assert_eq!(state(&app).slow.entries.len(), 2);
        assert_eq!(
            state(&app).slow.config,
            Some(SlowConfig {
                threshold_micros: 10_000,
                max_len: 128
            })
        );
        let out = app.apply(Msg::TickAt(t0 + secs(10)));
        let (_, req) = only_request(&out);
        let Request::SlowLog {
            specs,
            config,
            keys,
        } = req
        else {
            panic!("not a slow log request");
        };
        assert!(specs.contains_key("get"));
        assert!(config && keys);
    }

    #[test]
    fn a_refused_config_or_command_info_is_not_asked_again() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::SlowLog);
        let (id, _) = last_request(&out);
        let mut reply = slow_reply(vec![entry(1, "GET", &[], 20_000)]);
        reply.config = Some(Err(LiveError::Denied("CONFIG GET".into())));
        reply.keys_off = Some(LiveError::Denied("COMMAND INFO".into()));
        answer(&mut app, id, Reply::SlowLog(Box::new(reply)));
        let s = &state(&app).slow;
        assert_eq!(
            s.config_note.as_deref(),
            Some("Threshold and length unknown: the server refused CONFIG GET for this user.")
        );
        assert_eq!(
            s.keys_note.as_deref(),
            Some("Key names are off: the server refused COMMAND INFO for this user.")
        );
        assert!(s.poll.off.is_none(), "the panel itself stays on");
        let out = app.apply(Msg::TickAt(t0 + secs(10)));
        let (id, req) = only_request(&out);
        assert!(matches!(
            req,
            Request::SlowLog {
                config: false,
                keys: false,
                ..
            }
        ));

        // The next answer, to a poll that asked for neither, keeps both reasons.
        let mut reply = slow_reply(vec![entry(2, "GET", &[], 20_000)]);
        reply.config = None;
        reply.keys_off = None;
        answer(&mut app, id, Reply::SlowLog(Box::new(reply)));
        let s = &state(&app).slow;
        assert!(s.config_note.is_some());
        assert_eq!(
            s.keys_note.as_deref(),
            Some("Key names are off: the server refused COMMAND INFO for this user.")
        );
    }

    #[test]
    fn a_failed_command_info_is_asked_again_and_clears_when_it_works() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::SlowLog);
        let (id, _) = last_request(&out);
        let mut reply = slow_reply(vec![entry(1, "GET", &[], 20_000)]);
        reply.keys_off = Some(LiveError::Failed("COMMAND INFO failed".into()));
        answer(&mut app, id, Reply::SlowLog(Box::new(reply)));
        assert!(state(&app).slow.keys_note.is_some());
        let out = app.apply(Msg::TickAt(t0 + secs(10)));
        let (id, req) = only_request(&out);
        assert!(matches!(req, Request::SlowLog { keys: true, .. }));
        let mut reply = slow_reply(vec![entry(1, "GET", &[], 20_000)]);
        reply.keys_off = None;
        answer(&mut app, id, Reply::SlowLog(Box::new(reply)));
        assert!(state(&app).slow.keys_note.is_none());
    }

    #[test]
    fn the_slow_log_selection_follows_its_entry() {
        let mut s = SlowLog::default();
        s.set(slow_reply(vec![
            entry(3, "GET", &[], 1),
            entry(2, "SET", &[], 1),
            entry(1, "DEL", &[], 1),
        ]));
        s.sel = 1;
        s.set(slow_reply(vec![
            entry(5, "GET", &[], 1),
            entry(4, "GET", &[], 1),
            entry(3, "GET", &[], 1),
            entry(2, "SET", &[], 1),
        ]));
        assert_eq!(s.selected().unwrap().id, 2);
        s.set(slow_reply(vec![entry(9, "GET", &[], 1)]));
        assert_eq!(s.sel, 0);
        s.set(slow_reply(vec![]));
        assert!(s.selected().is_none());
    }

    #[test]
    fn list_keys_move_the_slow_log_selection() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::SlowLog);
        let (id, _) = last_request(&out);
        let entries = (1..=30)
            .rev()
            .map(|i| entry(i, "GET", &["k"], 20_000))
            .collect();
        answer(&mut app, id, Reply::SlowLog(Box::new(slow_reply(entries))));
        app.handle_key(ch('j'));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(state(&app).slow.sel, 2);
        app.handle_key(ch('G'));
        assert_eq!(state(&app).slow.sel, 29);
        app.handle_key(key(KeyCode::Home));
        assert_eq!(state(&app).slow.sel, 0);
        app.handle_key(key(KeyCode::PageDown));
        assert!(state(&app).slow.sel >= 1);
        // Keys that are not list keys are left alone.
        assert!(!app.views.server.slow.key(ch('x'), 5));
    }

    #[test]
    fn the_state_never_holds_a_user_or_a_password() {
        // The parser drops `user=`; whatever the state prints cannot contain it.
        let list = crate::live::server::parse_client_list(
            "id=1 addr=10.0.0.1:1 name= age=1 idle=1 cmd=get user=hunter2-user lib-name=x lib-ver=1\n",
        );
        let mut c = Clients::default();
        c.set(list);
        assert!(!format!("{c:?}").contains("hunter2"));
    }

    #[test]
    fn move_selection_handles_an_empty_list_and_modified_keys() {
        assert_eq!(move_selection(0, 0, ch('j'), 5), Some(0));
        assert_eq!(move_selection(0, 0, ch('G'), 5), Some(0));
        assert_eq!(move_selection(3, 10, ch('x'), 5), None);
        let ctrl = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(move_selection(3, 10, ctrl, 5), None);
        assert_eq!(move_selection(3, 10, key(KeyCode::PageDown), 0), Some(4));
    }

    // -- snapshots -----------------------------------------------------------

    /// Feeds `n` INFO polls two seconds apart, with a busy-quiet-busy ops pattern.
    fn feed_overview(app: &mut App, t0: Instant, n: u64) {
        let mut commands = 10_000u64;
        let mut hits = 900u64;
        let mut misses = 100u64;
        for i in 0..n {
            let at = t0 + secs(2 * i);
            let out = app.apply(Msg::TickAt(at));
            // The first poll went out when the panel was shown.
            let reqs = server_requests(&out);
            let id = match reqs.last() {
                Some((id, _)) => *id,
                None => app_pending_info(app),
            };
            let busy = 100 + 80 * ((i * 7) % 5);
            commands += 2 * busy;
            hits += 2 * busy * 9 / 10;
            misses += 2 * busy / 10;
            let mut s = sample(2 * i);
            s.commands = Some(commands);
            s.hits = Some(hits);
            s.misses = Some(misses);
            s.used_memory = Some(1_000_000 + 40_000 * i * (1 + i % 3));
            s.clients = Some(8 + (i * 5) % 7);
            s.evicted = Some(if i > 6 { (i - 6) * 3 } else { 0 });
            answer(
                app,
                id,
                Reply::Info {
                    at,
                    sample: Box::new(s),
                },
            );
        }
    }

    fn app_pending_info(app: &App) -> ReqId {
        state(app)
            .overview
            .poll
            .in_flight
            .expect("an INFO in flight")
    }

    fn overview_app(w: u16) -> App {
        let _ = w;
        let t0 = Instant::now();
        let (mut app, _) = app_at(t0, Screen::Overview);
        feed_overview(&mut app, t0, 14);
        app
    }

    #[test]
    fn overview_80x24() {
        let app = overview_app(80);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn overview_120x36() {
        let app = overview_app(120);
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn overview_paused_with_a_failed_poll_80x24() {
        let t0 = Instant::now();
        let (mut app, _) = app_at(t0, Screen::Overview);
        feed_overview(&mut app, t0, 4);
        // The next poll goes out, then `p` pauses before it is answered.
        app.apply(Msg::TickAt(t0 + secs(8)));
        app.handle_key(ch('p'));
        let id = app_pending_info(&app);
        testing::reply(&mut app, id, Err(LiveError::Failed("INFO failed".into())));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn overview_waiting_for_the_first_answer_80x24() {
        let (app, _) = app_at(Instant::now(), Screen::Overview);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn overview_off_when_info_is_refused_80x24() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Overview);
        let (id, _) = only_request(&out);
        testing::reply(&mut app, id, Err(LiveError::Denied("INFO".into())));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    /// Reconnects the app with the fixture's refused `SLOWLOG` probe, as a rescan
    /// after an ACL change would.
    fn probes_off(app: &mut App) {
        let s = app.session.as_ref().expect("a session");
        let (id, scan) = (s.id, s.scan);
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(crate::tui::fixtures::report(true).server),
            perms: crate::tui::fixtures::perms_probes_off(),
            target: crate::conn::ScanTarget::Primary,
        });
    }

    #[test]
    fn slow_log_off_when_its_probe_was_refused_80x24() {
        let t0 = Instant::now();
        let (mut app, _) = app_at(t0, Screen::SlowLog);
        probes_off(&mut app);
        // The other panels of the view still work.
        assert!(app.ctx().unwrap().off_reason().is_some());
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    fn clients_app() -> App {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::Clients);
        let (id, _) = last_request(&out);
        answer(&mut app, id, Reply::Clients(fleet()));
        app
    }

    #[test]
    fn clients_80x24() {
        let app = clients_app();
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn clients_120x36_second_group_selected() {
        let mut app = clients_app();
        app.handle_key(ch('j'));
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn clients_drill_down_80x24() {
        let mut app = clients_app();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(ch('j'));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn clients_drill_down_120x36() {
        let mut app = clients_app();
        for _ in 0..4 {
            app.handle_key(ch('j'));
        }
        app.handle_key(key(KeyCode::Enter));
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    fn slow_entries() -> Vec<SlowEntry> {
        let mut hg = entry(7, "HGETALL", &["cart:1002"], 45_300);
        hg.whole_key_read = true;
        let mut mget = entry(6, "MGET", &[], 22_000);
        mget.keys = (1..=8).map(|i| format!("user:{i}:profile")).collect();
        mget.more_keys = 4;
        mget.client = "10.0.2.9:41222".into();
        mget.client_name = String::new();
        vec![
            entry(9, "SET", &["session:9f3"], 12_400),
            entry(8, "KEYS", &[], 1_250_000),
            hg,
            mget,
            entry(5, "XINFO STREAM", &["orders"], 15_000),
            entry(4, "FLUSHDB", &[], 310_000),
            entry(3, "ZADD", &["leaderboard:2026-11"], 812),
            entry(
                2,
                "DEL",
                &["a-key-with-a-really-long-name:that:does:not:fit:in:a:narrow:column"],
                30_100,
            ),
            entry(1, "EVALSHA", &["lock:jobs", "lock:jobs:owner"], 18_000),
        ]
    }

    fn slow_app() -> App {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::SlowLog);
        let (id, _) = last_request(&out);
        answer(
            &mut app,
            id,
            Reply::SlowLog(Box::new(slow_reply(slow_entries()))),
        );
        app
    }

    #[test]
    fn slow_log_80x24() {
        let mut app = slow_app();
        app.handle_key(ch('j'));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn slow_log_120x36_with_a_note() {
        let mut app = slow_app();
        app.handle_key(ch('j'));
        app.handle_key(ch('j'));
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn slow_log_without_config_or_key_names_80x24() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::SlowLog);
        let (id, _) = last_request(&out);
        let mut reply = slow_reply(vec![
            entry(2, "GET", &[], 20_000),
            entry(1, "KEYS", &[], 90_000),
        ]);
        reply.config = Some(Err(LiveError::Denied("CONFIG GET".into())));
        reply.keys_off = Some(LiveError::Denied("COMMAND INFO".into()));
        answer(&mut app, id, Reply::SlowLog(Box::new(reply)));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn slow_log_empty_80x24() {
        let t0 = Instant::now();
        let (mut app, out) = app_at(t0, Screen::SlowLog);
        let (id, _) = last_request(&out);
        answer(&mut app, id, Reply::SlowLog(Box::new(slow_reply(vec![]))));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn small_terminals_do_not_panic() {
        let t0 = Instant::now();
        for screen in [Screen::Overview, Screen::Clients, Screen::SlowLog] {
            let (mut app, out) = app_at(t0, screen);
            let (id, req) = last_request(&out);
            let reply = match req {
                Request::Info => Reply::Info {
                    at: t0,
                    sample: Box::new(sample(0)),
                },
                Request::Clients => Reply::Clients(fleet()),
                Request::SlowLog { .. } => Reply::SlowLog(Box::new(slow_reply(slow_entries()))),
                Request::Ping | Request::CommandKeys { .. } => unreachable!(),
            };
            answer(&mut app, id, reply);
            app.handle_key(key(KeyCode::Enter));
            for (w, h) in [(80, 24), (40, 12), (20, 6), (10, 3), (1, 1), (200, 60)] {
                let _ = testing::render(&app, w, h);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Writes (P3)
    // -----------------------------------------------------------------------

    use super::super::streams::form::testing::{
        READ_ONLY, answer_anything, press, set_profile, type_text, writes,
    };
    use crate::live::write::Reply as WriteReply;
    use crate::model::Env;
    use crate::tui::app::confirm::Expect;

    /// On Clients with [`fleet`] listed, the profile set to `env` and `read_only`.
    fn on_clients(env: Env, read_only: Option<bool>) -> App {
        let (mut app, out) = app_at(Instant::now(), Screen::Clients);
        let (id, _) = last_request(&out);
        answer(&mut app, id, Reply::Clients(fleet()));
        set_profile(&mut app, env, read_only);
        app
    }

    /// Opens the group labelled `label` and selects its connection `conn`.
    fn open_conn(app: &mut App, label: &str, conn: u64) {
        let g = state(app)
            .clients
            .groups
            .iter()
            .position(|g| g.key.label() == label)
            .expect("the group");
        app.views.server.clients.sel = g;
        app.handle_key(key(KeyCode::Enter));
        let at = state(app)
            .clients
            .open_conns()
            .iter()
            .position(|c| c.id == conn)
            .expect("the connection");
        app.views.server.clients.conn_sel = at;
    }

    /// On the Slow log with its settings known (10 ms, 128 entries).
    fn on_slow_log(env: Env, read_only: Option<bool>) -> App {
        let (mut app, out) = app_at(Instant::now(), Screen::SlowLog);
        let (id, _) = last_request(&out);
        answer(
            &mut app,
            id,
            Reply::SlowLog(Box::new(slow_reply(vec![entry(1, "GET", &["k"], 12_000)]))),
        );
        set_profile(&mut app, env, read_only);
        app
    }

    fn notice(app: &App) -> String {
        app.notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    #[test]
    fn k_kills_the_selected_connection_after_asking_and_reads_again() {
        let mut app = on_clients(Env::Local, None);
        // On the groups, K says how to get to a connection.
        assert!(writes(&app.handle_key(ch('K'))).is_empty());
        assert!(notice(&app).contains("Press enter"), "{}", notice(&app));
        open_conn(&mut app, "redis-py 5.0.1", 2);
        let e = app.handle_key(ch('K'));
        assert!(writes(&e).is_empty());
        let c = app.confirm.as_ref().expect("a question");
        assert_eq!(c.expect, Expect::YesNo);
        assert!(c.lines().iter().any(|l| l == "Sends: CLIENT KILL ID 2"));
        let e = app.handle_key(ch('y'));
        assert_eq!(writes(&e), vec![WriteOp::ClientKill { id: 2 }]);
        let (id, _) = testing::requests(&e)[0].clone();
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Int(1))));
        assert_eq!(notice(&app), "Killed connection 2.");
        assert!(matches!(server_requests(&e)[..], [(_, Request::Clients)]));
    }

    #[test]
    fn k_on_prod_asks_for_the_profile_name() {
        let mut app = on_clients(Env::Prod, Some(false));
        open_conn(&mut app, "redis-py 5.0.1", 3);
        app.handle_key(ch('K'));
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("prod-cache-01".into())
        );
        assert!(writes(&app.handle_key(ch('y'))).is_empty());
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "prod-cache-01");
        let e = press(&mut app, KeyCode::Enter);
        assert_eq!(writes(&e), vec![WriteOp::ClientKill { id: 3 }]);
    }

    #[test]
    fn k_refuses_redis_lens_own_connection() {
        let mut app = on_clients(Env::Local, None);
        // Connection 7 ran the CLIENT LIST itself.
        assert!(is_own(&fleet()[6]));
        assert!(!is_own(&fleet()[0]), "idle 0, but another command");
        open_conn(&mut app, "127.0.0.0/24", 7);
        let e = app.handle_key(ch('K'));
        assert!(writes(&e).is_empty());
        assert!(app.confirm.is_none());
        assert!(notice(&app).contains("redis-lens's own connection"));
    }

    #[test]
    fn a_failed_or_cancelled_kill_says_why_and_reads_nothing() {
        let mut app = on_clients(Env::Local, None);
        open_conn(&mut app, "redis-py 5.0.1", 2);
        app.handle_key(ch('K'));
        let e = app.handle_key(ch('n'));
        assert!(writes(&e).is_empty() && testing::requests(&e).is_empty());
        assert_eq!(notice(&app), crate::live::CANCELLED);
        app.handle_key(ch('K'));
        let e = app.handle_key(ch('y'));
        let (id, _) = testing::requests(&e)[0].clone();
        let e = testing::reply(
            &mut app,
            id,
            Err(LiveError::Failed(
                "CLIENT KILL failed: no such client".into(),
            )),
        );
        assert!(testing::requests(&e).is_empty());
        assert_eq!(notice(&app), "CLIENT KILL failed: no such client.");
    }

    #[test]
    fn c_on_the_slow_log_sets_the_threshold_then_the_length() {
        let mut app = on_slow_log(Env::Local, None);
        app.handle_key(ch('c'));
        let f = state(&app).form.as_ref().expect("the form");
        assert_eq!(f.value("threshold µs"), "10000");
        assert_eq!(f.value("max length"), "128");
        // Unchanged, it sends nothing.
        press(&mut app, KeyCode::Enter);
        let e = press(&mut app, KeyCode::Enter);
        assert!(writes(&e).is_empty());
        assert_eq!(notice(&app), "Change the threshold or the length first.");
        // A threshold of 5 ms and 256 entries.
        press(&mut app, KeyCode::Tab);
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut app, "5000");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "56");
        assert_eq!(
            state(&app).form.as_ref().unwrap().value("max length"),
            "156"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut app, "256");
        insta::assert_snapshot!(
            "slowlog_form_80x24",
            testing::render(&app, 80, 24).backend()
        );
        let e = press(&mut app, KeyCode::Enter);
        assert!(writes(&e).is_empty());
        assert!(state(&app).form.is_none());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        let e = app.handle_key(ch('y'));
        assert_eq!(
            writes(&e),
            vec![WriteOp::SlowlogConfig(SlowlogSetting::Threshold(5000))]
        );
        let (id, _) = testing::requests(&e)[0].clone();
        // The second asks too, once the first went through.
        testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Ok)));
        let e = app.handle_key(ch('y'));
        assert_eq!(
            writes(&e),
            vec![WriteOp::SlowlogConfig(SlowlogSetting::MaxLen(256))]
        );
        let (id, _) = testing::requests(&e)[0].clone();
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Ok)));
        assert_eq!(notice(&app), "Slow log settings changed.");
        assert!(matches!(
            server_requests(&e)[..],
            [(_, Request::SlowLog { config: true, .. })]
        ));
    }

    #[test]
    fn the_slow_log_form_checks_what_is_typed() {
        let mut f = Form::new(
            "t",
            vec![],
            vec![
                Field::text("threshold µs", ""),
                Field::text("max length", ""),
            ],
        );
        let known = Some(SlowConfig {
            threshold_micros: 10,
            max_len: 128,
        });
        f.fields[0].text = "-2".into();
        assert!(slowlog_ops(&f, known).is_err());
        f.fields[0].text = "abc".into();
        assert!(slowlog_ops(&f, known).is_err());
        f.fields[0].text = "-1".into();
        assert_eq!(
            slowlog_ops(&f, known),
            Ok(vec![WriteOp::SlowlogConfig(SlowlogSetting::Threshold(-1))])
        );
        f.fields[1].text = "x".into();
        assert!(slowlog_ops(&f, known).is_err());
        // Unknown settings: what is typed is sent.
        f.fields[0].text = "10".into();
        f.fields[1].text = "128".into();
        assert!(slowlog_ops(&f, known).is_err());
        assert_eq!(slowlog_ops(&f, None).map(|v| v.len()), Ok(2));
    }

    #[test]
    fn dangerous_slow_log_writes_ask_for_the_profile_name_on_prod() {
        let mut app = on_slow_log(Env::Prod, Some(false));
        app.handle_key(ch('R'));
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("prod-cache-01".into())
        );
        app.handle_paste("prod-cache-01".into());
        let e = press(&mut app, KeyCode::Enter);
        assert_eq!(writes(&e), vec![WriteOp::SlowlogReset]);
        let (id, _) = testing::requests(&e)[0].clone();
        let e = testing::reply(&mut app, id, Ok(LiveReply::Write(WriteReply::Ok)));
        assert_eq!(notice(&app), "Slow log reset.");
        assert!(matches!(
            server_requests(&e)[..],
            [(_, Request::SlowLog { .. })]
        ));

        app.handle_key(ch('c'));
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "0");
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("prod-cache-01".into())
        );
    }

    #[test]
    fn r_resets_the_slow_log_after_y_outside_prod() {
        let mut app = on_slow_log(Env::Stage, Some(false));
        let e = app.handle_key(ch('R'));
        assert!(writes(&e).is_empty());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        assert_eq!(
            writes(&app.handle_key(ch('y'))),
            vec![WriteOp::SlowlogReset]
        );
    }

    #[test]
    fn no_write_reaches_a_read_only_profile_from_the_server_view() {
        for (env, ro) in READ_ONLY {
            let mut app = on_clients(env, ro);
            open_conn(&mut app, "redis-py 5.0.1", 2);
            let mut e = app.handle_key(ch('K'));
            assert!(app.confirm.is_none());
            assert!(notice(&app).contains("read-only"), "{}", notice(&app));
            e.extend(answer_anything(&mut app));
            assert!(writes(&e).is_empty(), "K {env:?} {ro:?}");

            for k in ['c', 'R'] {
                let mut app = on_slow_log(env, ro);
                let mut e = app.handle_key(ch(k));
                assert!(app.confirm.is_none() && state(&app).form.is_none());
                assert!(notice(&app).contains("read-only"), "{}", notice(&app));
                e.extend(answer_anything(&mut app));
                assert!(writes(&e).is_empty(), "{k} {env:?} {ro:?}");
            }
        }
    }
}

/// The Monitor panel, the Slow log's arguments and the Hot keys ranking from Monitor.
#[cfg(test)]
mod monitor_tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent};

    use super::super::testing::{self, key};
    use super::*;
    use crate::live::server::{Begin, Find, KeySpec, SlowConfig, SlowLogReply};
    use crate::tui::app::{App, Effect, Msg, Screen};
    use crate::tui::fixtures;
    use crate::value::{self, Raw};

    /// A value: on screen only.
    const MARK: &str = "hunter2-monitor-secret";
    /// A credential: never anywhere.
    const PASS: &str = "s3cret-pass-xyz";

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn ids(app: &App) -> (u64, u64) {
        let s = app.session.as_ref().unwrap();
        (s.id, s.scan)
    }

    /// On the Monitor panel, connected (`prod`: the fixture's prod-cache-01) with the
    /// clock at `t0`.
    fn on_monitor(t0: Instant, prod: bool) -> App {
        let mut app = testing::scanned();
        if !prod {
            app.session.as_mut().unwrap().profile.env = Env::Stage;
        }
        testing::live_ready(&mut app);
        app.apply(Msg::TickAt(t0));
        testing::show(&mut app, Screen::Monitor);
        app
    }

    fn started(effects: &[Effect]) -> Option<u64> {
        effects.iter().find_map(|e| match e {
            Effect::Monitor { run, .. } => Some(*run),
            _ => None,
        })
    }

    fn stopped(effects: &[Effect]) -> Option<u64> {
        effects.iter().find_map(|e| match e {
            Effect::StopMonitor { run, .. } => Some(*run),
            _ => None,
        })
    }

    fn type_text(app: &mut App, s: &str) -> Vec<Effect> {
        s.chars().flat_map(|c| app.handle_key(ch(c))).collect()
    }

    fn line(i: usize, cmd: &str) -> Entry {
        monitor::read_line(&format!(
            r#"1763039045.{:06} [0 10.0.1.5:{}] {cmd}"#,
            i * 1000 % 1_000_000,
            50_000 + i % 7
        ))
        .unwrap()
    }

    fn ready(app: &mut App, run: u64) {
        let (id, scan) = ids(app);
        app.apply(Msg::MonitorReady {
            id,
            scan,
            run,
            at_unix_ms: 1_763_039_045_000,
        });
    }

    fn lines(app: &mut App, run: u64, lines: Vec<Entry>) -> Vec<Effect> {
        let (id, scan) = ids(app);
        app.apply(Msg::MonitorLines {
            id,
            scan,
            run,
            lines,
        })
    }

    fn closed(app: &mut App, run: u64, end: Result<End, String>) -> Vec<Effect> {
        let (id, scan) = ids(app);
        app.apply(Msg::MonitorClosed { id, scan, run, end })
    }

    /// A non-prod run started with `y`, ready.
    fn running(t0: Instant) -> (App, u64) {
        let mut app = on_monitor(t0, false);
        app.handle_key(key(KeyCode::Enter));
        let run = started(&app.handle_key(ch('y'))).expect("a run");
        ready(&mut app, run);
        (app, run)
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

    #[test]
    fn it_asks_y_or_n_first_and_n_sends_nothing() {
        let mut app = on_monitor(Instant::now(), false);
        let out = app.handle_key(key(KeyCode::Enter));
        assert!(started(&out).is_none());
        assert_eq!(app.views.server.monitor.prompt, Some(Prompt::YesNo));
        assert_eq!(app.view_input().unwrap().label, "MONITOR");
        // The prompt takes every key: a digit does not switch views.
        assert!(started(&app.handle_key(ch('3'))).is_none());
        assert_eq!(app.screen(), Screen::Monitor);
        assert!(app.handle_key(ch('n')).is_empty());
        assert!(app.views.server.monitor.prompt.is_none());
        assert!(!app.views.server.monitor.running());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        app.handle_key(key(KeyCode::Enter));
        let run = started(&app.handle_key(ch('y'))).expect("y starts it");
        assert_eq!(app.views.server.monitor.run, Some(run));
        // A second enter while it runs does not ask again.
        app.handle_key(key(KeyCode::Enter));
        assert!(app.views.server.monitor.prompt.is_none());
    }

    #[test]
    fn a_prod_profile_needs_its_name_typed() {
        let mut app = on_monitor(Instant::now(), true);
        // A read-only prod profile asks too.
        assert!(app.session.as_ref().unwrap().profile.is_read_only());
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.views.server.monitor.prompt,
            Some(Prompt::TypeName(String::new()))
        );
        // `y` is text here.
        assert!(started(&app.handle_key(ch('y'))).is_none());
        app.handle_key(key(KeyCode::Backspace));
        let out = type_text(&mut app, "prod-cache-0");
        assert!(started(&out).is_none());
        assert!(started(&app.handle_key(key(KeyCode::Enter))).is_none());
        assert!(!app.views.server.monitor.running());
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("not the profile name")
        );
        app.handle_key(key(KeyCode::Enter));
        let name = app.session.as_ref().unwrap().profile.name.clone();
        type_text(&mut app, &name);
        assert!(started(&app.handle_key(key(KeyCode::Enter))).is_some());
    }

    #[test]
    fn a_run_stops_at_ten_thousand_lines() {
        let (mut app, run) = running(Instant::now());
        let first: Vec<Entry> = (0..9_000).map(|i| line(i, r#""GET" "k""#)).collect();
        assert!(stopped(&lines(&mut app, run, first)).is_none());
        let more: Vec<Entry> = (0..1_500).map(|i| line(i, r#""GET" "k""#)).collect();
        assert_eq!(stopped(&lines(&mut app, run, more)), Some(run));
        let m = &app.views.server.monitor;
        assert_eq!(m.lines.len(), MAX_LINES);
        assert_eq!(m.stopped, Some(Stop::LineCap));
        assert!(!m.running());
        // Lines of the stopped run are ignored.
        lines(&mut app, run, vec![line(1, r#""GET" "k""#)]);
        assert_eq!(app.views.server.monitor.lines.len(), MAX_LINES);
    }

    #[test]
    fn a_run_stops_after_thirty_seconds_and_with_esc() {
        let t0 = Instant::now();
        let (mut app, run) = running(t0);
        assert!(stopped(&app.apply(Msg::TickAt(t0 + Duration::from_secs(29)))).is_none());
        assert!(app.views.server.monitor.running());
        let out = app.apply(Msg::TickAt(t0 + Duration::from_secs(30)));
        assert_eq!(stopped(&out), Some(run));
        assert_eq!(app.views.server.monitor.stopped, Some(Stop::TimeUp));
        assert_eq!(app.views.server.monitor.ran(None), MAX_TIME);

        let (mut app, run) = running(t0);
        assert_eq!(stopped(&app.handle_key(key(KeyCode::Esc))), Some(run));
        assert_eq!(app.views.server.monitor.stopped, Some(Stop::Esc));
    }

    #[test]
    fn a_run_stops_with_the_live_connection_and_a_rescan() {
        let (mut app, run) = running(Instant::now());
        let (id, scan) = ids(&app);
        let out = app.apply(Msg::LiveClosed {
            id,
            scan,
            error: Some("the connection to the server was lost".into()),
        });
        assert_eq!(stopped(&out), Some(run));
        assert_eq!(app.views.server.monitor.stopped, Some(Stop::Closed));

        let (mut app, _) = running(Instant::now());
        app.handle_key(ch('2'));
        app.handle_key(ch('r'));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.views.server.monitor.stopped, Some(Stop::Rescan));
    }

    #[test]
    fn a_refusal_is_said_in_fixed_words() {
        let mut app = on_monitor(Instant::now(), false);
        app.handle_key(key(KeyCode::Enter));
        let run = started(&app.handle_key(ch('y'))).unwrap();
        closed(
            &mut app,
            run,
            Err(LiveError::Denied("MONITOR".into()).to_string()),
        );
        let all = text(&app, 80, 24);
        assert!(
            all.contains("the server refused MONITOR for this user"),
            "{all}"
        );
        // It never ran: no ranking.
        assert!(app.views.server.monitor.ranking().is_none());
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    fn sample_lines() -> Vec<Entry> {
        let mut v = vec![
            line(1, &format!(r#""AUTH" "lens" "{PASS}""#)),
            line(2, r#""GET" "user:10293:cart""#),
            line(3, r#""HGETALL" "session:9f3""#),
            line(4, r#""GET" "user:10293:cart""#),
            line(5, &format!(r#""SET" "rate:1.2.3.4" "{MARK}" "EX" "60""#)),
            line(6, r#""XINFO" "STREAM" "orders""#),
            line(7, r#""GET" "feature:\x1b[31mflags\xe2\x80\xae""#),
            line(8, r#""PING""#),
        ];
        v.push(line(
            9,
            &format!(r#""CONFIG" "SET" "requirepass" "{PASS}""#),
        ));
        v.push(
            monitor::read_line(&format!(
                r#"1763039046.5 [0 lua] "MSET" "a" "{}" "b" "c""#,
                "v".repeat(2000)
            ))
            .unwrap(),
        );
        v
    }

    #[test]
    fn credentials_never_reach_the_screen_debug_or_a_snapshot() {
        let (mut app, run) = running(Instant::now());
        let (id, scan) = ids(&app);
        let msg = Msg::MonitorLines {
            id,
            scan,
            run,
            lines: sample_lines(),
        };
        assert!(!value::leaks(&format!("{msg:?}"), MARK));
        assert!(!value::leaks(&format!("{msg:?}"), PASS));
        let out = app.apply(msg);
        for (w, h) in [(80, 24), (120, 36)] {
            app.handle_key(key(KeyCode::Home));
            let all = text(&app, w, h);
            assert!(all.contains("AUTH *** ***"), "{all}");
            assert!(!all.contains(PASS), "{all}");
            assert!(
                !all.contains('\u{1b}') && !all.contains('\u{202e}'),
                "{all}"
            );
        }
        // The value of SET is a value: on screen when selected, never in Debug.
        let debug = format!("{out:?} {:?}", app.views);
        assert!(!value::leaks(&debug, MARK), "{debug}");
        assert!(!value::leaks(&debug, PASS), "{debug}");
        // Credentials are masked as the lines arrive: not even the state holds them.
        assert!(
            app.views.server.monitor.lines.iter().all(|e| e
                .line
                .args
                .iter()
                .all(|a| a != PASS.as_bytes()))
        );
        let out = app.handle_key(key(KeyCode::Esc));
        let debug = format!("{out:?} {:?} {:?}", app.views, app.notice);
        assert!(!value::leaks(&debug, MARK), "{debug}");
        // `s` from Patterns saves the scan only.
        app.handle_key(ch('2'));
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(report)] = &effects[..] else {
            panic!("{effects:?}");
        };
        let json = serde_json::to_string(report).unwrap();
        assert!(!value::leaks(&json, MARK) && !value::leaks(&json, PASS));
        assert!(!json.contains("rate:1.2.3.4"));
    }

    fn get_spec() -> CommandKeys {
        CommandKeys::Specs(vec![KeySpec {
            begin: Begin::Index(1),
            find: Find::Range {
                last: 0,
                step: 1,
                limit: 0,
            },
        }])
    }

    /// A finished run on a scan without LFU counters, its keys placed.
    fn ranked() -> App {
        let t0 = Instant::now();
        let (mut app, run) = running(t0);
        app.session.as_mut().unwrap().report = Some(fixtures::report_lru());
        lines(&mut app, run, sample_lines());
        app.apply(Msg::TickAt(t0 + Duration::from_secs(12)));
        let out = closed(&mut app, run, Ok(End::Lost));
        let reqs = testing::requests(&out);
        let [(id, LiveRequest::Server(Request::CommandKeys { names }))] = &reqs[..] else {
            panic!("{reqs:?}");
        };
        assert!(names.contains(&"get".to_string()) && names.contains(&"xinfo".to_string()));
        assert!(!names.iter().any(|n| n.contains(MARK)));
        assert!(app.views.server.monitor.ranking_pending());
        let mut found: HashMap<String, CommandKeys> = names
            .iter()
            .map(|n| (n.clone(), CommandKeys::Specs(vec![])))
            .collect();
        for n in ["get", "hgetall", "set"] {
            found.insert(n.into(), get_spec());
        }
        found.insert("xinfo".into(), CommandKeys::Container);
        let out = testing::reply(
            &mut app,
            *id,
            Ok(LiveReply::Server(Reply::CommandKeys(found))),
        );
        // Round two: the container's subcommand.
        let reqs = testing::requests(&out);
        let [(id, LiveRequest::Server(Request::CommandKeys { names }))] = &reqs[..] else {
            panic!("{reqs:?}");
        };
        assert_eq!(names, &["xinfo|stream"]);
        // Refused: rank with what is known.
        testing::reply(&mut app, *id, Err(LiveError::Denied("COMMAND INFO".into())));
        app
    }

    #[test]
    fn hot_keys_rank_the_last_run_and_never_save_it() {
        let mut app = ranked();
        let hot = app.monitor_hot_keys().expect("a ranking");
        assert_eq!(hot.ranking.keys[0], ("user:10293:cart".to_string(), 2));
        assert_eq!(hot.ranking.lines, 10);
        // XINFO STREAM (refused) is unknown; the cut MSET was learned without keys.
        assert_eq!(hot.ranking.unknown, 1);
        assert_eq!(hot.secs, 12);
        assert!(hot.keys_note.is_some());
        testing::show(&mut app, Screen::HotKeys);
        let all = text(&app, 120, 36);
        assert!(
            all.contains("from Monitor, 12 s, 10 lines, at 13:04 UTC"),
            "{all}"
        );
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(report)] = &effects[..] else {
            panic!("{effects:?}");
        };
        let json = serde_json::to_string(report).unwrap();
        assert!(!json.contains("user:10293:cart") || report.hot_keys.is_empty());
        assert!(!json.contains("session:9f3"), "the ranking is not saved");
        assert!(!value::leaks(&json, MARK));
        // y copies the key name, as on the LFU ranking.
        let effects = app.handle_key(ch('y'));
        assert!(effects.iter().any(|e| matches!(e, Effect::Copy(_))));
    }

    #[test]
    fn hot_keys_from_monitor_80x24() {
        let mut app = ranked();
        testing::show(&mut app, Screen::HotKeys);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn hot_keys_from_monitor_120x36() {
        let mut app = ranked();
        testing::show(&mut app, Screen::HotKeys);
        app.handle_key(ch('j'));
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn monitor_not_started_80x24() {
        let app = on_monitor(Instant::now(), false);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn monitor_asking_prod_80x24() {
        let mut app = on_monitor(Instant::now(), true);
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "prod-ca");
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn monitor_asking_y_or_n_120x36() {
        let mut app = on_monitor(Instant::now(), false);
        app.handle_key(key(KeyCode::Enter));
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    fn with_lines() -> App {
        let t0 = Instant::now();
        let (mut app, run) = running(t0);
        lines(&mut app, run, sample_lines());
        app.apply(Msg::TickAt(t0 + Duration::from_secs(7)));
        app
    }

    #[test]
    fn monitor_running_80x24() {
        let app = with_lines();
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn monitor_running_120x36() {
        let mut app = with_lines();
        app.handle_key(ch('k'));
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn monitor_stopped_80x24_selected_line_cut() {
        let mut app = with_lines();
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::End));
        let all = text(&app, 80, 24);
        assert!(all.contains("more arguments not kept"), "{all}");
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn small_terminals_do_not_panic() {
        let mut app = with_lines();
        for (w, h) in [(40, 12), (20, 6), (10, 3), (1, 1), (200, 60)] {
            let _ = testing::render(&app, w, h);
        }
        app.handle_key(key(KeyCode::Enter));
        for (w, h) in [(40, 12), (1, 1)] {
            let _ = testing::render(&app, w, h);
        }
    }

    #[test]
    fn slow_log_arguments_are_shown_masked_80x24() {
        let t0 = Instant::now();
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        app.apply(Msg::TickAt(t0));
        let out = testing::show(&mut app, Screen::SlowLog);
        let (id, _) = testing::requests(&out).pop().unwrap();
        let entry = |id: u64, command: &str, args: &str, cut: u64| SlowEntry {
            id,
            unix: 1_763_039_045 - id as i64,
            micros: 12_000 + id * 1000,
            command: command.into(),
            keys: vec![],
            more_keys: 0,
            client: "10.0.1.5:50386".into(),
            client_name: String::new(),
            whole_key_read: false,
            args: Raw::new(args.as_bytes().to_vec()),
            args_cut: cut,
        };
        let entries = vec![
            entry(3, "CONFIG SET", "CONFIG SET requirepass ***", 0),
            entry(2, "DEL", "DEL k1 k2 k3 k4 k5 k6 k7 k8 k9 k10 k11 k12", 40),
            entry(1, "SET", &format!("SET k {MARK}"), 0),
        ];
        let reply = SlowLogReply {
            entries,
            config: Some(Ok(SlowConfig {
                threshold_micros: 0,
                max_len: 128,
            })),
            keys_off: None,
            learned: HashMap::new(),
        };
        let msg = format!("{reply:?}");
        assert!(!value::leaks(&msg, MARK), "{msg}");
        testing::reply(
            &mut app,
            id,
            Ok(LiveReply::Server(Reply::SlowLog(Box::new(reply)))),
        );
        assert!(!value::leaks(&format!("{:?}", app.views), MARK));
        app.handle_key(ch('j'));
        let all = text(&app, 80, 24);
        assert!(all.contains("… 40 more arguments"), "{all}");
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
        app.handle_key(ch('k'));
        app.handle_key(ch('k'));
        assert!(text(&app, 120, 36).contains("CONFIG SET requirepass ***"));
        app.handle_key(ch('2'));
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(report)] = &effects[..] else {
            panic!("{effects:?}");
        };
        assert!(!value::leaks(&serde_json::to_string(report).unwrap(), MARK));
    }
}
