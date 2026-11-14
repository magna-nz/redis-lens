//! State of the Slots panel of 2 Analysis (spec "2 Analysis · Slots (P4)"): what each
//! slot range holds, from `CLUSTER SLOT-STATS`. Drawn by `ui/slots.rs`; its requests are
//! in `src/live/slots.rs`.
//!
//! The panel reads when it is first shown (or when the connection becomes ready while it
//! is shown) and on `r`, and never polls. A read sends one request to each node that owns
//! slots, at most [`AT_ONCE`] at a time, each from that node's budget. A rescan starts a
//! new run with other connections, so what was read is dropped and read again when the
//! panel is shown.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

use crossterm::event::KeyEvent;

use super::keys::{list_move, plain};
use super::{Act, ViewState};
use crate::conn::slot::key_slot;
use crate::live::nodes::NodeInfo;
use crate::live::slots::{NodeStats, Reply, Request, SlotStat};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{ScanReport, ServerFlavor};
use crate::pattern::pattern_for_key;
use crate::tui::app::{ConnId, ScanId};

/// Nodes asked at the same time.
pub const AT_ONCE: usize = 4;
/// Rows of the hottest slots list.
pub const HOTTEST: usize = 20;

/// Why the panel is off on a standalone server.
pub const NOT_CLUSTER: &str = "Not a cluster: this is one standalone server.";
/// Why the panel is off when the server refused the command.
pub const REFUSED: &str = "The server refused CLUSTER SLOT-STATS for this user.";
/// The note in the title when no CPU or network figure is above 0.
pub const NEEDS_ENABLED: &str = "CPU and network need cluster-slot-stats-enabled yes";

/// How the hottest slots are ordered; `o` cycles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    #[default]
    Keys,
    Cpu,
    NetIn,
    NetOut,
}

impl Order {
    pub fn next(self) -> Self {
        match self {
            Self::Keys => Self::Cpu,
            Self::Cpu => Self::NetIn,
            Self::NetIn => Self::NetOut,
            Self::NetOut => Self::Keys,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Keys => "keys",
            Self::Cpu => "CPU",
            Self::NetIn => "net in",
            Self::NetOut => "net out",
        }
    }

    fn value(self, s: &SlotStat) -> Option<u64> {
        match self {
            Self::Keys => s.keys,
            Self::Cpu => s.cpu_usec,
            Self::NetIn => s.net_in,
            Self::NetOut => s.net_out,
        }
    }
}

/// Why the whole panel is off, once a node said so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Off {
    /// A primary runs a version without the command.
    TooOld { node: String, runs: String },
    /// A node refused the command for this user.
    Refused,
}

impl Off {
    pub fn text(&self) -> String {
        match self {
            Self::TooOld { node, runs } => {
                format!("CLUSTER SLOT-STATS needs Redis 8.2 or Valkey 8.0; {node} runs {runs}")
            }
            Self::Refused => REFUSED.to_string(),
        }
    }
}

/// Where one node's read stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NodeState {
    /// Not asked: no slots, or not asked yet.
    #[default]
    Idle,
    Waiting,
    Loaded(NodeStats),
    /// In redis-lens's own words.
    Failed(String),
}

/// A metric summed over some slots; `None` when no slot carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    pub keys: Option<u64>,
    pub cpu_usec: Option<u64>,
    pub net_in: Option<u64>,
    pub net_out: Option<u64>,
    pub memory: Option<u64>,
}

fn sum(slots: &[SlotStat], f: impl Fn(&SlotStat) -> Option<u64>) -> Option<u64> {
    slots
        .iter()
        .filter_map(f)
        .reduce(|a, b| a.saturating_add(b))
}

impl Totals {
    pub fn of(slots: &[SlotStat]) -> Self {
        Self {
            keys: sum(slots, |s| s.keys),
            cpu_usec: sum(slots, |s| s.cpu_usec),
            net_in: sum(slots, |s| s.net_in),
            net_out: sum(slots, |s| s.net_out),
            memory: sum(slots, |s| s.memory),
        }
    }
}

/// What a range row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Waiting,
    Failed(String),
    Done(Totals),
}

/// One row of the range table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeRow {
    /// The node's index in the session's nodes.
    pub node: usize,
    pub start: u16,
    pub end: u16,
    pub outcome: Outcome,
}

/// A row of the hottest slots list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotSlot {
    pub node: usize,
    pub stat: SlotStat,
}

/// The scan's example, big and hot keys that hash to `slot`, with their patterns, in
/// key order. Computed locally; nothing is sent.
pub fn keys_in_slot(report: &ScanReport, slot: u16) -> Vec<SlotKey> {
    let mut out: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    let mut push = |k: &str, what: &'static str| {
        if key_slot(k.as_bytes()) == slot {
            let kinds = out.entry(k.to_string()).or_default();
            if !kinds.contains(&what) {
                kinds.push(what);
            }
        }
    };
    for p in &report.patterns {
        for k in &p.examples {
            push(&k.key, "example");
        }
    }
    for k in report.big_keys() {
        push(&k.key, "big");
    }
    for k in &report.hot_keys {
        push(&k.key, "hot");
    }
    out.into_iter()
        .map(|(key, kinds)| SlotKey {
            pattern: pattern_for_key(&key),
            kinds: kinds.join(", "),
            key,
        })
        .collect()
}

/// A key of the scan that hashes to the selected slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotKey {
    pub key: String,
    pub pattern: String,
    /// `example`, `big`, `hot`, joined with `, `.
    pub kinds: String,
}

#[derive(Debug, Default)]
pub struct SlotsState {
    /// The run the data belongs to.
    run: Option<(ConnId, ScanId)>,
    /// Read (or being read) for this run.
    started: bool,
    /// One per session node.
    pub nodes: Vec<NodeState>,
    queue: VecDeque<usize>,
    inflight: HashMap<ReqId, usize>,
    pub off: Option<Off>,
    pub order: Order,
    /// The selected row of the hottest slots.
    pub sel: usize,
    /// The hottest slots by [`Self::order`].
    pub hottest: Vec<HotSlot>,
}

impl SlotsState {
    /// A read is under way.
    pub fn is_loading(&self) -> bool {
        !self.inflight.is_empty() || !self.queue.is_empty()
    }

    /// The panel has been asked to read for the run on screen.
    pub fn started(&self) -> bool {
        self.started
    }

    /// The flavor the nodes answered with: Redis shows the memory column.
    pub fn flavor(&self) -> Option<ServerFlavor> {
        self.nodes.iter().find_map(|n| match n {
            NodeState::Loaded(s) => Some(s.flavor),
            _ => None,
        })
    }

    /// One row per slot range of each node, in slot order.
    pub fn range_rows(&self, nodes: &[NodeInfo]) -> Vec<RangeRow> {
        let mut out = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            for &(start, end) in &n.slots {
                let outcome = match self.nodes.get(i) {
                    Some(NodeState::Loaded(s)) => s
                        .ranges
                        .iter()
                        .find(|r| r.start == start && r.end == end)
                        .map_or(Outcome::Waiting, |r| Outcome::Done(Totals::of(&r.slots))),
                    Some(NodeState::Failed(why)) => Outcome::Failed(why.clone()),
                    _ => Outcome::Waiting,
                };
                out.push(RangeRow {
                    node: i,
                    start,
                    end,
                    outcome,
                });
            }
        }
        out.sort_by_key(|r| r.start);
        out
    }

    /// True when something was read and no CPU or network figure is above 0.
    pub fn cpu_and_network_zero(&self) -> bool {
        let mut seen = false;
        for n in &self.nodes {
            let NodeState::Loaded(s) = n else { continue };
            for r in &s.ranges {
                for st in &r.slots {
                    for v in [st.cpu_usec, st.net_in, st.net_out].into_iter().flatten() {
                        if v > 0 {
                            return false;
                        }
                        seen = true;
                    }
                }
            }
        }
        seen
    }

    fn rank(&mut self) {
        let order = self.order;
        let mut all: Vec<HotSlot> = Vec::new();
        for (node, n) in self.nodes.iter().enumerate() {
            if let NodeState::Loaded(s) = n {
                for r in &s.ranges {
                    for st in &r.slots {
                        if order.value(st).unwrap_or(0) > 0 {
                            all.push(HotSlot { node, stat: *st });
                        }
                    }
                }
            }
        }
        all.sort_by(|a, b| {
            order
                .value(&b.stat)
                .cmp(&order.value(&a.stat))
                .then(a.stat.slot.cmp(&b.stat.slot))
        });
        all.truncate(HOTTEST);
        self.hottest = all;
        self.sel = self.sel.min(self.hottest.len().saturating_sub(1));
    }

    /// The selected hottest slot.
    pub fn selected(&self) -> Option<&HotSlot> {
        self.hottest
            .get(self.sel.min(self.hottest.len().saturating_sub(1)))
    }

    /// Forgets everything read when the session is another run.
    fn sync(&mut self, act: &Act<'_>) {
        let run = Some((act.session.id, act.session.scan));
        if self.run != run {
            *self = Self {
                order: self.order,
                run,
                ..Self::default()
            };
        }
    }

    /// Starts a read of every node that owns slots, if the connection is ready.
    fn read(&mut self, act: &mut Act<'_>) {
        if !act.live_ready() || act.session.nodes.is_empty() {
            return;
        }
        self.started = true;
        self.off = None;
        self.queue.clear();
        self.inflight.clear();
        self.sel = 0;
        self.hottest.clear();
        self.nodes = vec![NodeState::Idle; act.session.nodes.len()];
        for (i, n) in act.session.nodes.iter().enumerate() {
            if n.slots.is_empty() {
                continue;
            }
            if n.connected {
                self.nodes[i] = NodeState::Waiting;
                self.queue.push_back(i);
            } else {
                self.nodes[i] = NodeState::Failed("not connected".to_string());
            }
        }
        self.pump(act);
    }

    /// Sends to queued nodes while fewer than [`AT_ONCE`] are in flight.
    fn pump(&mut self, act: &mut Act<'_>) {
        while self.off.is_none() && self.inflight.len() < AT_ONCE {
            let Some(i) = self.queue.pop_front() else {
                break;
            };
            let ranges = act.session.nodes[i].slots.clone();
            let req = LiveRequest::Slots(Request::Stats { ranges });
            match act.send_to(Dest::node(i), req) {
                Some(id) => {
                    self.inflight.insert(id, i);
                }
                None => {
                    self.queue.push_front(i);
                    break;
                }
            }
        }
    }

    fn stop(&mut self, off: Off) {
        self.off = Some(off);
        self.queue.clear();
        for n in &mut self.nodes {
            if *n == NodeState::Waiting {
                *n = NodeState::Idle;
            }
        }
    }
}

impl ViewState for SlotsState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        self.sync(act);
        if !self.started {
            self.read(act);
        }
    }

    /// Not a poll: only starts the first read when the connection became ready after the
    /// panel was shown.
    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        self.sync(act);
        if !self.started {
            self.read(act);
        }
    }

    fn refresh(&mut self, act: &mut Act<'_>) {
        self.sync(act);
        self.read(act);
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if let Some(next) = list_move(&key, self.sel, self.hottest.len(), act) {
            self.sel = next;
            return true;
        }
        if plain(&key) == Some('o') {
            self.order = self.order.next();
            self.rank();
            return true;
        }
        false
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        let Some(i) = self.inflight.remove(&id) else {
            return;
        };
        let endpoint = act.session.nodes.get(i).map(|n| n.endpoint.to_string());
        match result {
            Ok(LiveReply::Slots(Reply::Stats(stats))) if !stats.supported => {
                let runs = format!(
                    "{} {}",
                    match stats.flavor {
                        ServerFlavor::Redis => "Redis",
                        ServerFlavor::Valkey => "Valkey",
                        ServerFlavor::Unknown => "a server",
                    },
                    stats.version
                );
                self.stop(Off::TooOld {
                    node: endpoint.unwrap_or_default(),
                    runs,
                });
            }
            Ok(LiveReply::Slots(Reply::Stats(stats))) => {
                self.nodes[i] = NodeState::Loaded(stats);
            }
            Ok(_) => {
                self.nodes[i] = NodeState::Failed("unexpected reply".to_string());
            }
            Err(LiveError::Denied(_)) => self.stop(Off::Refused),
            Err(e) => self.nodes[i] = NodeState::Failed(e.to_string()),
        }
        self.rank();
        self.pump(act);
    }
}

#[cfg(test)]
#[path = "slots_tests.rs"]
mod tests;
