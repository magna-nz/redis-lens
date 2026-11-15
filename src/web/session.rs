//! The dashboard's session (spec "Web dashboard", Session): one active connection,
//! shared by every browser tab, separate from the TUI's.
//!
//! It runs on the TUI's own runtime without a terminal ([`Headless`]): a connect is the
//! TUI's [`Effect::Connect`] (the keychain, the route with SSH, Sentinel and cluster
//! discovery, the permission check and the connect figures, then the live task over
//! every node), the Pub/Sub tail is [`Effect::Subscribe`] and Monitor is
//! [`Effect::Monitor`]. What the runtime sends back ([`Msg`]) updates [`Current`] here,
//! resolves the request that asked ([`Hub::request`]: one `oneshot` per request) and
//! becomes server-sent events ([`Event`]).
//!
//! Routing state is the TUI's [`Session`] (nodes, picked node, refusals, the live
//! state), so the dashboard routes writes and typed commands with the TUI's own
//! `write_dest` and `command_to`.
//!
//! Credentials: a saved profile's entries are read once per connect from the
//! dashboard's store on a blocking thread ([`prefetch`]: the keychain has its own
//! timeout, `secrets::KEYCHAIN_TIMEOUT`), into a [`MemoryStore`] the runtime reads from
//! for that session, so no keychain call runs on the async runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::config::Config;
use crate::live::cmdline::CommandInfo;
use crate::live::pubsub::Subscription;
use crate::live::server::{CommandKeys, InfoSample};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{Auth, PermissionReport, Profile};
use crate::secrets::{MemoryStore, SecretError, SecretStore};
use crate::tui::Headless;
use crate::tui::app::views::LiveState;
use crate::tui::app::views::server::{DEFAULT_INTERVAL, INTERVALS, Point};
use crate::tui::app::{ConnId, Effect, History, Msg, Phase, SecretSource, Session};
use crate::web::dto;
use crate::write::{Permit, Settings};

/// What a live request gives back.
pub type LiveResult = Result<LiveReply, LiveError>;

/// How long a live request may wait for its answer (the budget's wait included).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a request was not answered in time, in redis-lens's words.
pub const TIMED_OUT: &str = "the server did not answer in time";

/// Events kept for an SSE client that reads slowly; past that it is told it missed some.
const EVENTS_KEPT: usize = 1024;

/// One server-sent event: its name and its JSON data.
#[derive(Debug, Clone)]
pub struct Event {
    pub name: &'static str,
    pub data: serde_json::Value,
}

impl Event {
    pub fn new(name: &'static str, data: impl Serialize) -> Self {
        Self {
            name,
            data: serde_json::to_value(data).unwrap_or(serde_json::Value::Null),
        }
    }

    /// A `notice` event: a sentence for the page to show.
    pub fn notice(text: impl Into<String>) -> Self {
        let text = crate::web::wording::server_free(&text.into());
        Self::new("notice", serde_json::json!({ "text": text }))
    }
}

/// A running command line ([`Hub::line_slot`]); the next one may start once it drops.
#[derive(Debug)]
pub struct LineSlot<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for LineSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// What to connect to.
#[derive(Debug, Clone)]
pub enum Target {
    /// A saved profile, by name.
    Profile(String),
    /// A container Docker discovery found, by container name.
    Docker(String),
}

/// The Pub/Sub tail of the session.
#[derive(Debug, Clone)]
pub struct Tail {
    pub id: u64,
    pub to: Subscription,
    pub node: usize,
    pub ready: bool,
}

/// The Monitor run of the session.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub run: u64,
    pub node: usize,
    pub started_at_unix_ms: Option<i64>,
}

/// The Overview's live figures: the picked node's last poll and its rates, and on a
/// cluster each node's last poll.
#[derive(Debug, Default)]
pub struct LiveFigures {
    pub node: usize,
    pub last: Option<(Instant, InfoSample)>,
    pub point: Option<Point>,
    /// One per node: its last `INFO`, and whether its last poll failed.
    pub shards: Vec<(Option<InfoSample>, bool)>,
    pub shards_at: Option<Instant>,
}

/// The current connection.
pub struct Current {
    pub id: ConnId,
    /// Where its credentials come from, for the tail and Monitor connections.
    pub source: SecretSource,
    pub docker: bool,
    /// The TUI's session: profile, server, nodes, picked node, refusals, live state.
    pub session: Session,
    pub perms: Option<PermissionReport>,
    pub settings: Settings,
    pending: HashMap<ReqId, oneshot::Sender<LiveResult>>,
    /// The write requests in flight, by their command name: a `NOPERM` turns it off.
    writes: HashMap<ReqId, String>,
    pub tail: Option<Tail>,
    pub monitor: Option<Monitor>,
    /// `COMMAND INFO` per lower-case name, for the command line: read once a session.
    pub infos: HashMap<String, Option<CommandInfo>>,
    /// `COMMAND LIST`, for completion.
    pub names: Option<Vec<String>>,
    /// The slow log's learned key specs, and whether its settings and keys are read.
    pub slow_specs: HashMap<String, CommandKeys>,
    pub slow_config: bool,
    pub slow_keys: bool,
    /// Why the slow log's settings, and its key names, are not read any more: kept, so
    /// every reply says it, not only the one that found out.
    pub slow_config_off: Option<String>,
    pub slow_keys_off: Option<String>,
    /// The picked node's last slow log and client list (the connect's reads, then each
    /// panel's): Overview's findings come from them, as the TUI's do.
    pub slow_entries: Vec<crate::live::server::SlowEntry>,
    pub client_conns: Vec<crate::live::server::ClientConn>,
    pub live: LiveFigures,
    /// The last `overview` event's data ([`Hub::emit_overview_if_changed`]).
    pub last_overview: Option<serde_json::Value>,
}

impl Current {
    fn new(id: ConnId, profile: Profile, source: SecretSource, docker: bool) -> Self {
        Self {
            id,
            source,
            docker,
            session: Session {
                id,
                scan: 1,
                profile,
                server: None,
                target: None,
                progress: None,
                phase: Phase::Connecting,
                partial: None,
                report: None,
                history: History::Pending,
                trend: None,
                previous: None,
                rescan: false,
                live: LiveState::Waiting,
                refusals: crate::write::Refusals::default(),
                sentinel: None,
                nodes: Vec::new(),
                picked: 0,
                analysis: None,
                connected_at_unix: None,
                stopped: false,
                figures: None,
                last_snapshot: None,
            },
            perms: None,
            settings: Settings::default(),
            pending: HashMap::new(),
            writes: HashMap::new(),
            tail: None,
            monitor: None,
            infos: HashMap::new(),
            names: None,
            slow_specs: HashMap::new(),
            slow_config: true,
            slow_keys: true,
            slow_config_off: None,
            slow_keys_off: None,
            slow_entries: Vec::new(),
            client_conns: Vec::new(),
            live: LiveFigures::default(),
            last_overview: None,
        }
    }

    pub fn profile(&self) -> &Profile {
        &self.session.profile
    }

    pub fn live_ready(&self) -> bool {
        self.session.live == LiveState::Ready && !self.failed()
    }

    /// The connect failed (the session stays, to say why).
    pub fn failed(&self) -> bool {
        matches!(self.session.phase, Phase::Failed(_))
    }

    /// Answers every request still waiting: the live connection is gone.
    fn drain(&mut self) {
        for (_, tx) in self.pending.drain() {
            let _ = tx.send(Err(LiveError::Closed));
        }
        self.writes.clear();
    }
}

impl Drop for Current {
    fn drop(&mut self) {
        self.drain();
    }
}

/// The live figures' polling: paused or not, and how often, as the TUI's Overview
/// (`p`, and `+` `-` through [`INTERVALS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Polling {
    pub paused: bool,
    /// Index into [`INTERVALS`].
    pub interval: usize,
}

impl Default for Polling {
    fn default() -> Self {
        Self {
            paused: false,
            interval: DEFAULT_INTERVAL,
        }
    }
}

impl Polling {
    pub fn every(&self) -> Duration {
        Duration::from_secs(INTERVALS[self.interval])
    }
}

/// Why an interval is refused: it is not one the TUI steps through.
pub const BAD_INTERVAL: &str = "the interval must be one of 1, 2, 3, 5, 10, 15, 30 or 60 seconds";

struct Inner {
    polling: Polling,
    runner: Headless,
    next_conn: ConnId,
    next_req: u64,
    next_tail: u64,
    cur: Option<Current>,
}

/// The dashboard's one session and its event stream, shared by every request.
pub struct Hub {
    pub config: PathBuf,
    pub store: Arc<dyn SecretStore>,
    inner: Mutex<Inner>,
    events: broadcast::Sender<Event>,
    clients: AtomicUsize,
    /// A command line is running ([`Hub::line_slot`]).
    line_running: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("clients", &self.clients.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// An SSE client being connected: counted while it lives ([`Hub::events`]).
pub struct Listener {
    hub: Arc<Hub>,
    pub rx: broadcast::Receiver<Event>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.hub.clients.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The entries `profile` reads from the keychain (its own, its SSH user's, its
/// Sentinels'), read from `store` into memory. Blocking: run it on a blocking thread.
pub fn prefetch(store: &dyn SecretStore, profile: &Profile) -> Result<MemoryStore, SecretError> {
    let mem = MemoryStore::default();
    let mut refs = Vec::new();
    if matches!(profile.auth, Auth::Keychain | Auth::Command) {
        refs.push(profile.secret_ref());
    }
    if profile.ssh.is_some() {
        refs.push(profile.ssh_secret_ref());
    }
    if profile.sentinel.is_some() {
        refs.push(profile.sentinel_secret_ref());
    }
    for r in refs {
        if let Some(c) = store.get(&r)? {
            mem.set(&r, &c)?;
        }
    }
    Ok(mem)
}

/// [`prefetch`] on a blocking thread, the error in words safe to show.
pub async fn prefetch_async(
    store: Arc<dyn SecretStore>,
    profile: Profile,
) -> Result<MemoryStore, String> {
    tokio::task::spawn_blocking(move || prefetch(store.as_ref(), &profile))
        .await
        .map_err(|_| "the keychain read failed".to_string())?
        .map_err(|e| crate::conn::parse::mask(&e.to_string()))
}

/// The settings in the config file at `path`, or the defaults when it can't be read.
pub fn settings_of(path: &std::path::Path) -> Settings {
    Config::load(path).map(|c| c.settings).unwrap_or_default()
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    // Every update is a plain assignment, so a panic elsewhere leaves it consistent.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Hub {
    /// A hub with no connection, for the config file at `config`, reading saved
    /// profiles' credentials from `store`. Must be called inside a tokio runtime.
    pub fn new(config: PathBuf, store: Arc<dyn SecretStore>) -> Arc<Self> {
        let (runner, msgs) = Headless::new(config.clone(), Arc::new(MemoryStore::default()));
        let (events, _) = broadcast::channel(EVENTS_KEPT);
        let hub = Arc::new(Self {
            config,
            store,
            inner: Mutex::new(Inner {
                polling: Polling::default(),
                runner,
                next_conn: 1,
                next_req: 1,
                next_tail: 1,
                cur: None,
            }),
            events,
            clients: AtomicUsize::new(0),
            line_running: std::sync::atomic::AtomicBool::new(false),
        });
        tokio::spawn(pump(Arc::downgrade(&hub), msgs));
        hub
    }

    /// A new SSE client.
    pub fn listen(self: &Arc<Self>) -> Listener {
        self.clients.fetch_add(1, Ordering::SeqCst);
        Listener {
            hub: Arc::clone(self),
            rx: self.events.subscribe(),
        }
    }

    /// How many SSE clients are connected: the live figures are polled only while some
    /// are.
    pub fn listeners(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    pub fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn emit_all(&self, events: Vec<Event>) {
        for e in events {
            self.emit(e);
        }
    }

    /// Reads the current connection, if any.
    pub fn with<R>(&self, f: impl FnOnce(&Current) -> R) -> Option<R> {
        lock(&self.inner).cur.as_ref().map(f)
    }

    /// Changes the current connection, if any.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut Current) -> R) -> Option<R> {
        lock(&self.inner).cur.as_mut().map(f)
    }

    /// The session as `GET /api/state` and the `state` event show it.
    pub fn state(&self) -> State {
        let g = lock(&self.inner);
        State::of(g.cur.as_ref(), g.polling)
    }

    /// How the live figures are polled now.
    pub fn polling(&self) -> Polling {
        lock(&self.inner).polling
    }

    /// Pauses or resumes the live figures and sets their interval, in seconds (one of
    /// [`INTERVALS`]). Paused, no `INFO` is sent for them.
    pub fn set_polling(
        &self,
        paused: Option<bool>,
        every_secs: Option<u64>,
    ) -> Result<Polling, &'static str> {
        let p = {
            let mut g = lock(&self.inner);
            let mut p = g.polling;
            if let Some(secs) = every_secs {
                p.interval = INTERVALS
                    .iter()
                    .position(|s| *s == secs)
                    .ok_or(BAD_INTERVAL)?;
            }
            if let Some(paused) = paused {
                p.paused = paused;
            }
            g.polling = p;
            p
        };
        self.emit_state();
        Ok(p)
    }

    fn emit_state(&self) {
        let s = self.state();
        self.emit(Event::new("state", s));
    }

    /// Sends an `overview` event with Overview's connect-time parts
    /// ([`crate::web::api::overview::connect_parts`]: the findings, figures, checks, last
    /// analysis, ...) when they differ from the last one sent for this connection: the
    /// connect's slow log and client list land after the live connection is ready, with
    /// nothing else to say so. Not sent before the connect has read anything.
    pub(crate) fn emit_overview_if_changed(&self) {
        let changed = self
            .with_mut(|c| {
                if matches!(c.session.phase, Phase::Connecting) {
                    return None;
                }
                let now = crate::web::api::overview::connect_parts(c);
                if c.last_overview.as_ref() == Some(&now) {
                    return None;
                }
                c.last_overview = Some(now.clone());
                Some(now)
            })
            .flatten();
        if let Some(data) = changed {
            self.emit(Event::new("overview", data));
        }
    }

    /// The one command line a session runs at a time, as the TUI's panel runs one: held
    /// until the returned slot is dropped. `None` while another line runs.
    pub fn line_slot(&self) -> Option<LineSlot<'_>> {
        use std::sync::atomic::Ordering;
        self.line_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| LineSlot(&self.line_running))
    }

    /// Makes the connected saved profile's `read_only` and `env` what the config file says
    /// now (rule 2): `w` in the TUI beside it, or `conn set --writes off`, changes the file
    /// and not this session's copy. Called before every write is decided and before an
    /// answer is used, so a profile made read-only elsewhere refuses at once. A file that
    /// cannot be read, or that no longer has the profile, turns writes off. A container
    /// has no saved profile: its session-only setting stays. Emits `state` when it changed.
    pub async fn follow_saved_profile(&self) {
        let Some((id, name)) = self
            .with(|c| (!c.docker).then(|| (c.id, c.session.profile.name.clone())))
            .flatten()
        else {
            return;
        };
        let path = self.config.clone();
        let saved = tokio::task::spawn_blocking(move || {
            crate::config::Config::load(&path).map(|c| c.get(&name).map(|p| (p.read_only, p.env)))
        })
        .await;
        let (read_only, env) = match saved {
            Ok(Ok(Some((read_only, env)))) => (read_only, Some(env)),
            _ => (Some(true), None),
        };
        let changed = self.with_mut(|c| {
            if c.id != id {
                return false;
            }
            let p = &mut c.session.profile;
            let before = (p.read_only, p.env);
            p.read_only = read_only;
            if let Some(env) = env {
                p.env = env;
            }
            before != (p.read_only, p.env)
        });
        if changed == Some(true) {
            self.emit_state();
        }
    }

    /// Connects `target`, replacing the current connection. Returns once the connect has
    /// started; how it goes comes as `state` events.
    pub async fn connect(self: &Arc<Self>, target: Target) -> Result<(), ConnectError> {
        let config = self.config.clone();
        let (profile, source, docker, keychain) = match target {
            Target::Profile(name) => {
                let loaded = tokio::task::spawn_blocking(move || Config::load(&config))
                    .await
                    .map_err(|_| ConnectError::Failed("the config file could not be read".into()))?
                    .map_err(|e| ConnectError::Failed(crate::conn::parse::mask(&e.to_string())))?;
                let Some(profile) = loaded.get(&name).cloned() else {
                    return Err(ConnectError::NotFound);
                };
                let mem = prefetch_async(Arc::clone(&self.store), profile.clone())
                    .await
                    .map_err(ConnectError::Failed)?;
                (profile, SecretSource::Keychain, false, Some(mem))
            }
            Target::Docker(container) => {
                let found = crate::conn::docker::discover()
                    .await
                    .map_err(|e| ConnectError::Failed(crate::conn::parse::mask(&e.to_string())))?;
                let Some(d) = found.into_iter().find(|d| d.container == container) else {
                    return Err(ConnectError::NotFound);
                };
                (
                    d.profile,
                    SecretSource::Container(d.credentials),
                    true,
                    None,
                )
            }
        };
        let path = self.config.clone();
        let settings = tokio::task::spawn_blocking(move || settings_of(&path))
            .await
            .unwrap_or_default();
        {
            let mut g = lock(&self.inner);
            let id = g.next_conn;
            g.next_conn += 1;
            let mut cur = Current::new(id, profile.clone(), source.clone(), docker);
            cur.settings = settings;
            // The old session goes first: its requests are answered as closed.
            g.cur = Some(cur);
            // The store and the connect it is for go in under one lock: two connects at
            // once (two tabs) never start one with the other's credentials. The runner
            // takes the store when the connect starts.
            if let Some(mem) = keychain {
                g.runner.set_keychain(Arc::new(mem));
            }
            g.runner.run(Effect::Connect {
                id,
                scan: 1,
                profile,
                secrets: source,
                sample: None,
            });
        }
        self.emit_state();
        Ok(())
    }

    /// A session on `profile` as if its connect had finished, the live connection
    /// ready, without any server: for the router's unit tests.
    #[cfg(test)]
    pub(crate) fn install_for_test(&self, profile: Profile, docker: bool) -> ConnId {
        let mut g = lock(&self.inner);
        let id = g.next_conn;
        g.next_conn += 1;
        let source = SecretSource::Keychain;
        let mut c = Current::new(id, profile, source, docker);
        c.session.phase = Phase::Connected;
        c.session.live = LiveState::Ready;
        g.cur = Some(c);
        id
    }

    /// Ends the current connection, its tail and its Monitor run.
    pub fn disconnect(&self) {
        {
            let mut g = lock(&self.inner);
            g.runner.close();
            g.cur = None;
        }
        self.emit_state();
    }

    /// Picks node `index` of a cluster for the views that read one node. False when there
    /// is no such node.
    pub fn pick(&self, index: usize) -> bool {
        let ok = self
            .with_mut(|c| {
                if index >= c.session.nodes.len().max(1) {
                    return false;
                }
                if c.session.picked != index {
                    c.slow_entries.clear();
                    c.client_conns.clear();
                }
                c.session.picked = index;
                c.live.node = index;
                c.live.last = None;
                c.live.point = None;
                true
            })
            .unwrap_or(false);
        if ok {
            self.emit_state();
            self.emit_overview_if_changed();
        }
        ok
    }

    /// Sends `req` to `dest` on connection `conn` (the current one when `None`) and waits
    /// for its answer.
    pub async fn request(&self, conn: Option<ConnId>, dest: Dest, req: LiveRequest) -> LiveResult {
        let sent = self.send(conn, dest, req, None)?;
        self.wait(sent).await
    }

    /// Sends a write the gate let through, on connection `conn`: a `NOPERM` turns its
    /// command off for the session (spec "Write permissions", check 1).
    pub async fn write(&self, conn: ConnId, dest: Dest, permit: Permit) -> LiveResult {
        let name = permit.op().command();
        let sent = self.send(Some(conn), dest, LiveRequest::Write(permit), Some(name))?;
        self.wait(sent).await
    }

    /// The answer to a request [`Self::send`] sent, or [`TIMED_OUT`] after
    /// [`REQUEST_TIMEOUT`]: then its entry in `pending` goes, so a request never answered
    /// leaves nothing behind (a late answer finds no one waiting and is dropped). Its
    /// entry in `writes` stays until the answer comes, so a late `NOPERM` still turns the
    /// command off.
    async fn wait(&self, (conn, req_id, rx): Sent) -> LiveResult {
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(LiveError::Closed),
            Err(_) => {
                self.with_mut(|c| {
                    if c.id == conn {
                        c.pending.remove(&req_id);
                    }
                });
                Err(LiveError::Failed(TIMED_OUT.into()))
            }
        }
    }

    /// How many requests wait for an answer: for tests.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.with(|c| c.pending.len()).unwrap_or(0)
    }

    fn send(
        &self,
        conn: Option<ConnId>,
        dest: Dest,
        req: LiveRequest,
        write: Option<String>,
    ) -> Result<Sent, LiveError> {
        let mut g = lock(&self.inner);
        let req_id = ReqId(g.next_req);
        g.next_req += 1;
        let Inner { runner, cur, .. } = &mut *g;
        let Some(c) = cur.as_mut() else {
            return Err(LiveError::Closed);
        };
        if conn.is_some_and(|id| id != c.id) || !c.live_ready() {
            return Err(LiveError::Closed);
        }
        let (tx, rx) = oneshot::channel();
        c.pending.insert(req_id, tx);
        if let Some(name) = write {
            c.writes.insert(req_id, name);
        }
        let id = c.id;
        runner.run(Effect::Live {
            id,
            req_id,
            to: dest,
            req,
        });
        Ok((id, req_id, rx))
    }

    /// Starts the Pub/Sub tail on `to` on node `node`, replacing the session's other
    /// tail. Its messages come as `tail` events.
    pub fn subscribe(&self, to: Subscription, node: usize) -> Result<u64, String> {
        let started = {
            let mut g = lock(&self.inner);
            let tail = g.next_tail;
            g.next_tail += 1;
            let Inner { runner, cur, .. } = &mut *g;
            let Some(c) = cur.as_mut() else {
                return Err(LiveError::Closed.to_string());
            };
            if !c.live_ready() {
                return Err(LiveError::Closed.to_string());
            }
            if let Some(why) = c.session.own_connection_refused(node) {
                return Err(why);
            }
            c.tail = Some(Tail {
                id: tail,
                to: to.clone(),
                node,
                ready: false,
            });
            runner.run(Effect::Subscribe {
                id: c.id,
                scan: c.session.scan,
                tail,
                profile: c.session.profile.clone(),
                secrets: c.source.clone(),
                to,
                node,
            });
            tail
        };
        self.emit_state();
        Ok(started)
    }

    /// Ends the session's tail, if one runs.
    pub fn unsubscribe(&self) -> bool {
        let stopped = {
            let mut g = lock(&self.inner);
            let Inner { runner, cur, .. } = &mut *g;
            match cur.as_mut().and_then(|c| c.tail.take().map(|t| (c.id, t))) {
                Some((id, t)) => {
                    runner.run(Effect::Unsubscribe { id, tail: t.id });
                    Some(t.id)
                }
                None => None,
            }
        };
        if let Some(tail) = stopped {
            self.emit(Event::new(
                "tail",
                serde_json::json!({ "tail": tail, "kind": "closed", "error": null }),
            ));
            self.emit_state();
        }
        stopped.is_some()
    }

    /// Whether Monitor can start now on the picked node, as the TUI checks before it
    /// asks: the connection's id and profile, or why not.
    pub fn monitor_check(&self) -> Result<(ConnId, Profile), String> {
        let g = lock(&self.inner);
        let Some(c) = g.cur.as_ref() else {
            return Err(LiveError::Closed.to_string());
        };
        monitor_refused(c).map_or_else(|| Ok((c.id, c.session.profile.clone())), Err)
    }

    /// Starts Monitor on the picked node of connection `conn`: time-boxed by
    /// `live::monitor::Limits` as in the TUI. Its lines come as `monitor` events. Ask
    /// first ([`Self::monitor_check`]): this starts it.
    pub fn monitor(&self, conn: ConnId) -> Result<u64, String> {
        let run = {
            let mut g = lock(&self.inner);
            let run = g.next_tail;
            g.next_tail += 1;
            let Inner { runner, cur, .. } = &mut *g;
            let Some(c) = cur.as_mut() else {
                return Err(LiveError::Closed.to_string());
            };
            if c.id != conn {
                return Err(crate::web::api::writes::CHANGED.to_string());
            }
            if let Some(why) = monitor_refused(c) {
                return Err(why);
            }
            let node = c.session.picked;
            c.monitor = Some(Monitor {
                run,
                node,
                started_at_unix_ms: None,
            });
            runner.run(Effect::Monitor {
                id: c.id,
                scan: c.session.scan,
                run,
                profile: c.session.profile.clone(),
                secrets: c.source.clone(),
                node,
            });
            run
        };
        self.emit_state();
        Ok(run)
    }

    /// Stops the Monitor run, if one runs.
    pub fn stop_monitor(&self) -> bool {
        let stopped = {
            let mut g = lock(&self.inner);
            let Inner { runner, cur, .. } = &mut *g;
            match cur
                .as_mut()
                .and_then(|c| c.monitor.take().map(|m| (c.id, m)))
            {
                Some((id, m)) => {
                    runner.run(Effect::StopMonitor { id, run: m.run });
                    Some(m.run)
                }
                None => None,
            }
        };
        if let Some(run) = stopped {
            self.emit(Event::new(
                "monitor",
                serde_json::json!({ "run": run, "kind": "closed", "end": "stopped", "error": null }),
            ));
            self.emit_state();
        }
        stopped.is_some()
    }

    /// Ends everything: the server is shutting down.
    pub fn close(&self) {
        let mut g = lock(&self.inner);
        g.runner.close();
        g.cur = None;
    }

    /// One message from the runtime.
    fn apply(&self, msg: Msg) {
        let mut events = Vec::new();
        let mut state = false;
        {
            let mut g = lock(&self.inner);
            let Inner { runner, cur, .. } = &mut *g;
            let Some(c) = cur.as_mut() else {
                return;
            };
            let live_closed =
                matches!(&msg, Msg::LiveClosed { id, scan, .. } if this(c, *id, *scan));
            let was_failed = c.failed();
            apply_to(c, msg, &mut events, &mut state);
            // A connect that failed after it opened (no password): nothing of it runs on.
            if !was_failed && c.failed() {
                runner.close();
                c.drain();
                c.session.live = LiveState::Waiting;
            }
            // A Monitor run stops with the live connection, as in the TUI (spec "Monitor
            // (P2)").
            if live_closed && let Some(m) = c.monitor.take() {
                runner.run(Effect::StopMonitor {
                    id: c.id,
                    run: m.run,
                });
                events.push(Event::new(
                    "monitor",
                    serde_json::json!({
                        "run": m.run,
                        "kind": "closed",
                        "end": "lost",
                        "error": crate::live::CONNECTION_LOST,
                    }),
                ));
            }
        }
        if state {
            self.emit_state();
        }
        self.emit_all(events);
        self.emit_overview_if_changed();
    }
}

/// Why Monitor can't start on `c` now, as the TUI's `enter` on Monitor checks.
fn monitor_refused(c: &Current) -> Option<String> {
    if c.monitor.is_some() {
        return Some(MONITOR_RUNS.to_string());
    }
    if !c.live_ready() {
        return Some(LiveError::Closed.to_string());
    }
    if let Some(why) = crate::conn::monitor_unsupported(&c.session.profile) {
        return Some(why.to_string());
    }
    c.session.own_connection_refused(c.session.picked)
}

/// Why a second Monitor run does not start.
pub const MONITOR_RUNS: &str = "MONITOR is already running: stop it first.";

/// Why a connect that opened is a failed one: every command was refused for want of a
/// password (`NOAUTH`), or for the wrong one (`WRONGPASS`). The permission check holds
/// the server's code at the start of a refusal it does not know.
pub(crate) fn auth_refused(perms: &PermissionReport) -> Option<&'static str> {
    let denied = || {
        perms.checks.iter().filter_map(|c| match &c.status {
            crate::model::PermStatus::Denied(m) => Some(m.as_str()),
            _ => None,
        })
    };
    if denied().any(|m| m.starts_with("NOAUTH")) {
        Some(crate::web::wording::NEEDS_PASSWORD)
    } else if denied().any(|m| m.starts_with("WRONGPASS")) {
        Some(crate::web::wording::BAD_CREDENTIALS)
    } else {
        None
    }
}

/// How a connect could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    NotFound,
    /// In words safe to show (masked).
    Failed(String),
}

/// A request on its way: the connection, its id and where its answer comes.
type Sent = (ConnId, ReqId, oneshot::Receiver<LiveResult>);

/// Passes the runtime's messages on to the hub until either is gone.
async fn pump(hub: Weak<Hub>, mut msgs: mpsc::UnboundedReceiver<Msg>) {
    while let Some(m) = msgs.recv().await {
        let Some(h) = hub.upgrade() else {
            break;
        };
        h.apply(m);
    }
}

fn this(c: &Current, id: ConnId, scan: u64) -> bool {
    c.id == id && c.session.scan == scan
}

/// What `msg` changes in `c`: the events it makes go to `events`, and `state` is set
/// when the session's state changed.
fn apply_to(c: &mut Current, msg: Msg, events: &mut Vec<Event>, state: &mut bool) {
    match msg {
        Msg::Connected {
            id,
            scan,
            server,
            perms,
            target,
        } if this(c, id, scan) => {
            *state = true;
            // The server let the connection open but answers nothing without a
            // password (or refused the one sent): that is a failed connect, said in
            // fixed words, not a connected session with every check refused.
            if let Some(why) = auth_refused(&perms) {
                c.session.phase = Phase::Failed(why.to_string());
                c.perms = None;
                return;
            }
            c.session.refusals = crate::tui::app::confirm::refusals_of(&perms);
            c.session.server = server;
            c.session.target = Some(target);
            c.session.phase = Phase::Connected;
            c.session.connected_at_unix = Some(unix_secs());
            c.perms = Some(perms);
        }
        Msg::Figures { id, scan, figures } if this(c, id, scan) => {
            c.session.figures = Some(figures);
            *state = true;
        }
        Msg::SentinelResolved { id, scan, answer } if this(c, id, scan) => {
            c.session.sentinel = Some(answer);
            *state = true;
        }
        Msg::Nodes { id, scan, nodes } if this(c, id, scan) => {
            // The first primary, as the TUI picks it.
            c.session.picked = nodes
                .iter()
                .position(|n| n.role == crate::model::NodeRole::Primary)
                .unwrap_or(0);
            c.live.node = c.session.picked;
            c.live.shards = vec![(None, false); nodes.len()];
            c.session.nodes = nodes;
            *state = true;
        }
        Msg::ConnectFailed { id, scan, error } if this(c, id, scan) => {
            c.session.phase = Phase::Failed(error);
            *state = true;
        }
        Msg::LastSnapshot { id, scan, last } if this(c, id, scan) => {
            c.session.last_snapshot = last;
        }
        Msg::LiveReady { id, scan } if this(c, id, scan) && !c.failed() => {
            c.session.live = LiveState::Ready;
            *state = true;
        }
        Msg::LiveClosed { id, scan, error } if this(c, id, scan) => {
            c.drain();
            if let Some(e) = &error {
                events.push(Event::notice(format!(
                    "The live connection closed: {e}. Connect again to go on."
                )));
            }
            c.session.live = LiveState::Closed(error);
            // A tail and a Monitor run end with the live connection (their messages
            // say so); nothing new starts on it.
            *state = true;
        }
        Msg::Live {
            id, req_id, result, ..
        } if c.id == id => {
            if let Some(name) = c.writes.remove(&req_id)
                && let Err(LiveError::Denied(_)) = &result
            {
                c.session.refusals.add(&name);
            }
            if let Some(tx) = c.pending.remove(&req_id) {
                let _ = tx.send(result);
            }
        }
        Msg::TailReady { id, scan, tail } if this(c, id, scan) => {
            if let Some(t) = c.tail.as_mut().filter(|t| t.id == tail) {
                t.ready = true;
                events.push(Event::new(
                    "tail",
                    serde_json::json!({ "tail": tail, "kind": "ready" }),
                ));
                *state = true;
            }
        }
        Msg::Tail {
            id,
            scan,
            tail,
            batch,
            dropped,
        } if this(c, id, scan) && c.tail.as_ref().is_some_and(|t| t.id == tail) => {
            let messages: Vec<dto::TailMsg> = batch.iter().map(dto::TailMsg::of).collect();
            events.push(Event::new(
                "tail",
                serde_json::json!({
                    "tail": tail,
                    "kind": "messages",
                    "messages": messages,
                    "dropped": dropped,
                }),
            ));
        }
        Msg::TailClosed {
            id,
            scan,
            tail,
            error,
        } if this(c, id, scan) && c.tail.as_ref().is_some_and(|t| t.id == tail) => {
            c.tail = None;
            events.push(Event::new(
                "tail",
                serde_json::json!({
                    "tail": tail,
                    "kind": "closed",
                    "error": crate::web::wording::server_free_opt(error.as_deref()),
                }),
            ));
            *state = true;
        }
        Msg::MonitorReady {
            id,
            scan,
            run,
            at_unix_ms,
        } if this(c, id, scan) => {
            if let Some(m) = c.monitor.as_mut().filter(|m| m.run == run) {
                m.started_at_unix_ms = Some(at_unix_ms);
                events.push(Event::new(
                    "monitor",
                    serde_json::json!({
                        "run": run,
                        "kind": "ready",
                        "at_unix_ms": at_unix_ms,
                        "max_lines": crate::live::monitor::MAX_LINES,
                        "max_secs": crate::live::monitor::MAX_TIME.as_secs(),
                    }),
                ));
                *state = true;
            }
        }
        Msg::MonitorLines {
            id,
            scan,
            run,
            lines,
        } if this(c, id, scan) && c.monitor.as_ref().is_some_and(|m| m.run == run) => {
            let lines: Vec<dto::MonitorLine> = lines.iter().map(dto::MonitorLine::of).collect();
            events.push(Event::new(
                "monitor",
                serde_json::json!({ "run": run, "kind": "lines", "lines": lines }),
            ));
        }
        Msg::MonitorClosed { id, scan, run, end }
            if this(c, id, scan) && c.monitor.as_ref().is_some_and(|m| m.run == run) =>
        {
            c.monitor = None;
            let (end, error) = match end {
                Ok(crate::live::monitor::End::TimeUp) => ("time_up", None),
                Ok(crate::live::monitor::End::LineCap) => ("line_cap", None),
                Ok(crate::live::monitor::End::Lost) => {
                    ("lost", Some(crate::live::CONNECTION_LOST.to_string()))
                }
                Ok(crate::live::monitor::End::Gone) => ("stopped", None),
                Err(e) => ("failed", Some(e)),
            };
            events.push(Event::new(
                "monitor",
                serde_json::json!({
                    "run": run,
                    "kind": "closed",
                    "end": end,
                    "error": crate::web::wording::server_free_opt(error.as_deref()),
                }),
            ));
            *state = true;
        }
        // The slow log and client list the connect read once, on the picked node: for
        // Overview's findings, as in the TUI.
        Msg::ServerReads {
            id,
            scan,
            node,
            slow,
            clients,
        } if this(c, id, scan) && c.session.picked == node => {
            use crate::live::server::Reply as S;
            if let Some(Ok(LiveReply::Server(S::SlowLog(r)))) = slow {
                c.slow_entries = r.entries;
            }
            if let Some(Ok(LiveReply::Server(S::Clients(list)))) = clients {
                c.client_conns = list;
            }
        }
        // The scan's progress and the rest are for the TUI's views.
        _ => {}
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The session, as `GET /api/state` and the `state` event show it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct State {
    /// `idle`, `connecting`, `connected` or `failed`.
    pub status: &'static str,
    /// Why the connect failed, in redis-lens's words.
    pub error: Option<String>,
    pub connection: Option<dto::Connection>,
    /// Connected to a container Docker discovery found, not a saved profile.
    pub docker: bool,
    /// `waiting`, `ready` or `closed`.
    pub live: &'static str,
    pub live_error: Option<String>,
    pub connected_at_unix: Option<i64>,
    pub server: Option<dto::Server>,
    /// Which server is read: `primary`, `replica <addr>`, or the cluster's summary.
    pub target: Option<String>,
    pub sentinel: Option<SentinelDto>,
    /// A cluster's nodes; empty on a standalone server.
    pub nodes: Vec<dto::Node>,
    pub picked: usize,
    pub writes: bool,
    /// Why writes are off, when they are.
    pub writes_off: Option<String>,
    /// Write commands the server refused this session.
    pub refused: Vec<String>,
    pub tail: Option<TailDto>,
    pub monitor: Option<MonitorDto>,
    /// How the live figures are polled.
    pub polling: PollingDto,
    /// When a Monitor run stops by itself, known before one starts.
    pub monitor_limits: MonitorLimits,
    /// What this build can do.
    pub features: Features,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PollingDto {
    pub paused: bool,
    pub interval_secs: u64,
    /// The intervals that can be set.
    pub intervals: &'static [u64],
}

impl PollingDto {
    pub fn of(p: Polling) -> Self {
        Self {
            paused: p.paused,
            interval_secs: INTERVALS[p.interval],
            intervals: &INTERVALS,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MonitorLimits {
    pub max_lines: usize,
    pub max_secs: u64,
}

impl MonitorLimits {
    pub fn now() -> Self {
        Self {
            max_lines: crate::live::monitor::MAX_LINES,
            max_secs: crate::live::monitor::MAX_TIME.as_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SentinelDto {
    pub sentinel: String,
    pub master: String,
    pub primary: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TailDto {
    pub id: u64,
    /// `channel`, `pattern` or `shard`.
    pub kind: &'static str,
    pub name: String,
    pub node: usize,
    pub ready: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MonitorDto {
    pub run: u64,
    pub node: usize,
    pub started_at_unix_ms: Option<i64>,
    pub max_lines: usize,
    pub max_secs: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Features {
    pub entra_id: bool,
    pub version: &'static str,
}

pub fn sub_kind(s: &Subscription) -> &'static str {
    match s {
        Subscription::Channel(_) => "channel",
        Subscription::Pattern(_) => "pattern",
        Subscription::Shard(_) => "shard",
    }
}

impl State {
    fn of(c: Option<&Current>, polling: Polling) -> Self {
        let polling = PollingDto::of(polling);
        let monitor_limits = MonitorLimits::now();
        let features = Features {
            entra_id: cfg!(feature = "entra-id"),
            version: env!("CARGO_PKG_VERSION"),
        };
        let Some(c) = c else {
            return Self {
                status: "idle",
                error: None,
                connection: None,
                docker: false,
                live: "waiting",
                live_error: None,
                connected_at_unix: None,
                server: None,
                target: None,
                sentinel: None,
                nodes: Vec::new(),
                picked: 0,
                writes: false,
                writes_off: None,
                refused: Vec::new(),
                tail: None,
                monitor: None,
                polling,
                monitor_limits,
                features,
            };
        };
        let s = &c.session;
        let (status, error) = match &s.phase {
            Phase::Connecting => ("connecting", None),
            Phase::Failed(e) => (
                "failed",
                Some(crate::value::escape(&crate::web::wording::server_free(e))),
            ),
            _ => ("connected", None),
        };
        let (live, live_error) = match &s.live {
            LiveState::Waiting => ("waiting", None),
            LiveState::Ready => ("ready", None),
            LiveState::Closed(e) => ("closed", crate::web::wording::server_free_opt(e.as_deref())),
        };
        Self {
            status,
            error,
            connection: Some(dto::Connection::of(&s.profile)),
            docker: c.docker,
            live,
            live_error,
            connected_at_unix: s.connected_at_unix,
            server: s.server.as_ref().map(dto::Server::of),
            target: s
                .target
                .as_ref()
                .map(|t| crate::value::escape(&t.to_string())),
            sentinel: s.sentinel.as_ref().map(|a| SentinelDto {
                sentinel: crate::value::escape(&a.sentinel.to_string()),
                master: crate::value::escape(&a.master),
                primary: crate::value::escape(&a.primary.to_string()),
            }),
            nodes: s
                .nodes
                .iter()
                .enumerate()
                .map(|(i, n)| dto::Node::of(i, n))
                .collect(),
            picked: s.picked,
            writes: !s.profile.is_read_only(),
            writes_off: s.profile.is_read_only().then(|| {
                crate::value::escape(&crate::web::wording::web_with(
                    &crate::write::read_only_reason(&s.profile),
                    &crate::web::api::writes::profile_data(&s.profile),
                ))
            }),
            refused: s.refusals.iter().map(str::to_string).collect(),
            tail: c.tail.as_ref().map(|t| TailDto {
                id: t.id,
                kind: sub_kind(&t.to),
                name: crate::value::escape(t.to.name()),
                node: t.node,
                ready: t.ready,
            }),
            monitor: c.monitor.as_ref().map(|m| MonitorDto {
                run: m.run,
                node: m.node,
                started_at_unix_ms: m.started_at_unix_ms,
                max_lines: crate::live::monitor::MAX_LINES,
                max_secs: crate::live::monitor::MAX_TIME.as_secs(),
            }),
            polling,
            monitor_limits,
            features,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Credentials, Endpoint, Env, SshTunnel};
    use secrecy::SecretString;

    fn profile(auth: Auth) -> Profile {
        Profile {
            name: "cache".into(),
            env: Env::Local,
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 6379,
                tls: false,
            },
            db: 0,
            auth,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            force_standalone: false,
            tags: Vec::new(),
        }
    }

    fn creds(user: &str) -> Credentials {
        Credentials {
            username: Some(user.into()),
            password: SecretString::from("pw".to_string()),
        }
    }

    #[test]
    fn prefetch_reads_only_the_entries_the_profile_uses() {
        let store = MemoryStore::default();
        let mut p = profile(Auth::None);
        store.set(&p.secret_ref(), &creds("main")).unwrap();
        store.set(&p.ssh_secret_ref(), &creds("ssh")).unwrap();
        store
            .set(&p.sentinel_secret_ref(), &creds("sentinel"))
            .unwrap();
        let mem = prefetch(&store, &p).unwrap();
        assert!(mem.get(&p.secret_ref()).unwrap().is_none());
        p.auth = Auth::Keychain;
        p.ssh = Some(SshTunnel {
            host: "bastion".into(),
            port: None,
            identity_file: None,
            known_hosts_file: None,
        });
        let mem = prefetch(&store, &p).unwrap();
        let user = |r| mem.get(&r).unwrap().and_then(|c| c.username);
        assert_eq!(user(p.secret_ref()).as_deref(), Some("main"));
        assert_eq!(user(p.ssh_secret_ref()).as_deref(), Some("ssh"));
        assert!(mem.get(&p.sentinel_secret_ref()).unwrap().is_none());
    }

    /// The connect's slow log and client list land after the live connection is ready:
    /// an `overview` event says so, with the new findings, and only when they changed.
    #[tokio::test]
    async fn overview_findings_landing_send_one_overview_event() {
        let hub = Hub::new(
            std::env::temp_dir().join("redis-lens-web-overview-none.toml"),
            Arc::new(crate::secrets::MemoryStore::default()),
        );
        let id = hub.install_for_test(profile(Auth::None), false);
        let mut l = hub.listen();
        let idle = |n: u64| crate::live::server::ClientConn {
            id: n,
            addr: format!("10.0.9.17:{}", 50_000 + n),
            name: String::new(),
            lib_name: String::new(),
            lib_ver: String::new(),
            age: 7_200,
            idle: crate::tui::app::views::server::IDLE_FLAG_SECS,
            cmd: "get".into(),
            mem: 0,
            db: Some(0),
        };
        let conns: Vec<_> = (0..crate::tui::app::views::overview::IDLE_CLIENTS_MIN as u64)
            .map(idle)
            .collect();
        let reads = |conns: Vec<crate::live::server::ClientConn>| Msg::ServerReads {
            id,
            scan: 1,
            node: 0,
            slow: None,
            clients: Some(Ok(LiveReply::Server(crate::live::server::Reply::Clients(
                conns,
            )))),
        };
        let overview = |l: &mut Listener| {
            let mut got = Vec::new();
            while let Ok(e) = l.rx.try_recv() {
                if e.name == "overview" {
                    got.push(e.data);
                }
            }
            got
        };
        // Something that changes nothing of Overview's first: the first event says what
        // it holds now.
        hub.emit_overview_if_changed();
        let first = overview(&mut l);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["findings"], serde_json::json!([]));
        assert!(first[0].get("live").is_none() && first[0].get("shards").is_none());
        hub.apply(reads(conns.clone()));
        let got = overview(&mut l);
        assert_eq!(got.len(), 1, "{got:?}");
        let f = got[0]["findings"].as_array().unwrap();
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            f[0]["text"]
                .as_str()
                .unwrap()
                .contains("idle for over an hour"),
            "{f:?}"
        );
        // The same again: no event.
        hub.apply(reads(conns));
        assert!(overview(&mut l).is_empty());
        // Gone: an event with no findings.
        hub.apply(reads(Vec::new()));
        let got = overview(&mut l);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["findings"], serde_json::json!([]));
    }

    /// N10: a request that is never answered times out and leaves nothing in `pending`.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_request_leaves_nothing_pending() {
        let hub = Hub::new(
            std::env::temp_dir().join("redis-lens-web-n10-none.toml"),
            Arc::new(crate::secrets::MemoryStore::default()),
        );
        let id = hub.install_for_test(profile(Auth::None), false);
        // A request sent and never answered (no live task answers it here).
        let (tx, rx) = oneshot::channel();
        hub.with_mut(|c| c.pending.insert(ReqId(99), tx));
        assert_eq!(hub.pending_len(), 1);
        let h = Arc::clone(&hub);
        let asked = tokio::spawn(async move { h.wait((id, ReqId(99), rx)).await });
        tokio::time::sleep(REQUEST_TIMEOUT + Duration::from_secs(1)).await;
        let r = asked.await.unwrap();
        assert_eq!(r, Err(LiveError::Failed(TIMED_OUT.into())));
        assert_eq!(hub.pending_len(), 0);
    }

    #[test]
    fn the_idle_state_has_no_connection() {
        let s = State::of(None, Polling::default());
        assert_eq!(s.status, "idle");
        assert!(s.connection.is_none());
        assert_eq!(s.features.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_reply_resolves_its_request_and_a_nopermission_write_turns_the_command_off() {
        let mut c = Current::new(1, profile(Auth::None), SecretSource::Keychain, false);
        let (tx, mut rx) = oneshot::channel();
        c.pending.insert(ReqId(7), tx);
        c.writes.insert(ReqId(7), "PUBLISH".into());
        let (mut events, mut state) = (Vec::new(), false);
        apply_to(
            &mut c,
            Msg::Live {
                id: 1,
                scan: 1,
                req_id: ReqId(7),
                result: Err(LiveError::Denied("PUBLISH".into())),
            },
            &mut events,
            &mut state,
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            Err(LiveError::Denied("PUBLISH".into()))
        );
        assert!(c.session.refusals.refused("PUBLISH"));
        // Another connection's reply is not this one's.
        let (tx, mut rx) = oneshot::channel();
        c.pending.insert(ReqId(8), tx);
        apply_to(
            &mut c,
            Msg::Live {
                id: 2,
                scan: 1,
                req_id: ReqId(8),
                result: Ok(LiveReply::Server(crate::live::server::Reply::Pong)),
            },
            &mut events,
            &mut state,
        );
        assert!(rx.try_recv().is_err());
        // The live connection closing answers what still waits.
        apply_to(
            &mut c,
            Msg::LiveClosed {
                id: 1,
                scan: 1,
                error: Some(crate::live::CONNECTION_LOST.into()),
            },
            &mut events,
            &mut state,
        );
        assert_eq!(rx.try_recv().unwrap(), Err(LiveError::Closed));
        assert!(state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "notice");
        let st = State::of(Some(&c), Polling::default());
        assert_eq!(st.live, "closed");
        assert_eq!(st.live_error.as_deref(), Some(crate::live::CONNECTION_LOST));
    }
}
