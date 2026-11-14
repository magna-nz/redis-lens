//! The terminal UI.
//!
//! Owner: TUI and CLI wiring.
//!
//! - [`app`]: the state and its pure update functions (`handle_key`, `handle_paste`,
//!   `apply`), which return [`app::Effect`]s instead of doing IO.
//! - [`ui`]: pure rendering, `ui::draw(frame, &app)`, one module per view.
//! - [`tree`]: grouping patterns into the Patterns tree.
//! - This module: terminal setup, the event loop, and the tokio tasks that carry out
//!   effects and send [`app::Msg`]s back. All server IO goes through `conn`, `scan` and
//!   `live`.
//!
//! After a connect succeeds, the session's connection stays open as its live connection
//! ([`crate::live`]) until the next connect or quitting, whether the scan finished or
//! failed. The scan and the live connection take from one [`Budget`]. On a cluster,
//! the connection discovery opened to each shard stays open the same way, each node
//! with its own budget shared by its scan and its live requests (spec "Cluster live
//! views (P4)").
//!
//! The Pub/Sub live tail ([`Effect::Subscribe`]) runs on a connection of its own, kept
//! in the current [`LiveSession`] next to the live task, so a new connect, a rescan or
//! quitting drops it with the session; it takes from the session's budget too. A Monitor
//! run ([`Effect::Monitor`]) is kept the same way.

use std::collections::{HashMap, VecDeque};
use std::io::{Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::bail;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::execute;
use ratatui::DefaultTerminal;
use secrecy::ExposeSecret;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::budget::Budget;
use crate::config::Config;
use crate::conn::cluster::{ClusterTargets, ScanTargets};
use crate::conn::{self, ScanTarget};
use crate::live::pubsub::{Subscription, TAIL_KEPT, TailMessage};
use crate::live::{self, LiveError, LiveHandle};
use crate::model::{Credentials, Profile, ScanReport, ServerInfo, ServerMode};
use crate::scan::{self, ScanOptions, ScanProgress};
use crate::secrets::{KeyringStore, MemoryStore, SecretStore};
use crate::{diff, snapshot};

pub mod app;
pub mod clipboard;
pub mod editor;
pub mod keymap;
pub mod tree;
pub mod ui;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod snapshot_tests;
/// Snapshots of cluster scans: Nodes, Patterns narrowed to a node, the Hot keys node
/// column, and the cluster summary.
#[cfg(test)]
mod snapshot_tests_cluster;
/// Snapshots of the provider notices and the server context strip (quick-wins agent C).
#[cfg(test)]
mod snapshot_tests_server;

use app::{App, ConnId, Effect, Msg, ScanId, SecretSource};
use clipboard::CopyText;

const TICK: Duration = Duration::from_millis(100);

/// What arrives on the event loop's channel. `Msg` is boxed: a scan report makes it far
/// bigger than an input event.
enum Incoming {
    Input(Event),
    InputFailed(String),
    Msg(Box<Msg>),
}

type Tx = mpsc::UnboundedSender<Incoming>;

pub async fn run(config: Option<PathBuf>) -> anyhow::Result<()> {
    let path = match config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    let loaded = Config::load(&path);

    let mut terminal = ratatui::try_init()?;
    // ratatui's panic hook restores the terminal; also turn bracketed paste off.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableBracketedPaste);
        prev(info);
    }));
    let result = match execute!(stdout(), EnableBracketedPaste) {
        Ok(()) => event_loop(&mut terminal, path, loaded).await,
        Err(e) => Err(e.into()),
    };
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

fn spawn_input_thread(tx: Tx) {
    std::thread::spawn(move || {
        loop {
            // While `$EDITOR` runs (editor.rs) the terminal is the editor's: don't read.
            if editor::INPUT_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let msg = {
                let _gate = editor::INPUT_GATE.lock().unwrap_or_else(|e| e.into_inner());
                if editor::INPUT_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                match crossterm::event::poll(Duration::from_millis(50)) {
                    Ok(false) => continue,
                    Ok(true) => match crossterm::event::read() {
                        Ok(ev) => Incoming::Input(ev),
                        Err(e) => Incoming::InputFailed(e.to_string()),
                    },
                    Err(e) => Incoming::InputFailed(e.to_string()),
                }
            };
            let failed = matches!(msg, Incoming::InputFailed(_));
            if tx.send(msg).is_err() || failed {
                break;
            }
        }
    });
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    path: PathBuf,
    loaded: Result<Config, crate::config::ConfigError>,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    // The colours are set once, before the first frame: the theme from the file, and
    // NO_COLOR from the environment, which wins.
    let theme = loaded
        .as_ref()
        .map_or_else(|_| Default::default(), |c| c.settings.theme);
    ui::style::init(theme, ui::style::no_color_requested());
    let mut app = match loaded {
        Ok(cfg) => {
            let mut app = App::new(cfg.profiles);
            app.settings = cfg.settings;
            app
        }
        Err(e) => {
            let mut app = App::new(Vec::new());
            app.set_error(conn::parse::mask(&e.to_string()));
            app
        }
    };
    spawn_input_thread(tx.clone());
    let mut runner = Runner::new(tx, path);
    let mut pending = app.init();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        for effect in pending.drain(..) {
            // A copy goes to the same output the frames are drawn to, between frames.
            let editor = matches!(effect, Effect::Editor(_));
            if runner.run(effect, terminal.backend_mut()) {
                return Ok(());
            }
            if editor {
                // Back from `$EDITOR`: draw the whole screen again.
                terminal.clear()?;
            }
        }
        let mut list_rows = 0;
        terminal.draw(|f| {
            list_rows = ui::list_rows(f.area());
            ui::draw(f, &app);
        })?;
        // The page for PgUp and PgDn, from the area just drawn.
        app.set_list_rows(list_rows);
        tokio::select! {
            incoming = rx.recv() => match incoming {
                Some(Incoming::Input(Event::Key(k))) => pending = app.handle_key(k),
                Some(Incoming::Input(Event::Paste(s))) => pending = app.handle_paste(s),
                Some(Incoming::Input(_)) => {}
                Some(Incoming::InputFailed(e)) => bail!("could not read terminal input: {e}"),
                Some(Incoming::Msg(m)) => pending = app.apply(*m),
                None => return Ok(()),
            },
            _ = tick.tick() => pending = app.apply(Msg::TickAt(std::time::Instant::now())),
        }
    }
}

/// Carries out effects. Each one runs in its own task and reports back as a `Msg`.
struct Runner {
    tx: Tx,
    path: PathBuf,
    scan_task: Option<JoinHandle<()>>,
    /// The current session's live connection, shared with its connect task.
    live: LiveSlot,
    /// The newest run of each entry, for answering an [`Effect::Live`] that can't be
    /// sent.
    scans: HashMap<ConnId, ScanId>,
}

impl Drop for Runner {
    fn drop(&mut self) {
        if let Some(t) = self.scan_task.take() {
            t.abort();
        }
        self.close_live();
    }
}

/// The current session and, once its connect succeeded, its live task.
struct LiveSession {
    id: ConnId,
    scan: ScanId,
    handle: Option<LiveHandle>,
    /// Each node's command budget and where a connection of its own to it goes, by the
    /// node's index in the live task's table, once the connect succeeded: one entry on
    /// a standalone server.
    nodes: Vec<NodeRoute>,
    /// The Pub/Sub live tail, if one runs: dropping it aborts the task and drops its
    /// connection.
    tail: Option<TailTask>,
    /// The Monitor run, if one runs: dropping it aborts the task and drops its
    /// connection, which ends `MONITOR`.
    monitor: Option<TailTask>,
}

impl LiveSession {
    fn new(id: ConnId, scan: ScanId) -> Self {
        Self {
            id,
            scan,
            handle: None,
            nodes: Vec::new(),
            tail: None,
            monitor: None,
        }
    }
}

/// What a connection of its own to one node needs: the node's budget and, on a
/// cluster, its address and whether credentials go to it (`None`: the profile's own
/// endpoint).
#[derive(Clone)]
struct NodeRoute {
    budget: Arc<Budget>,
    addr: Option<conn::NodeAddr>,
}

/// A running live tail: its id and its task, aborted when this is dropped.
struct TailTask {
    tail: u64,
    _task: AbortOnDrop,
}

type LiveSlot = Arc<Mutex<Option<LiveSession>>>;

fn lock(slot: &LiveSlot) -> MutexGuard<'_, Option<LiveSession>> {
    // Every update is a plain assignment, so a panic elsewhere leaves it consistent.
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

/// Installs the live task `make` starts as session `(id, scan)`'s, then sends
/// [`Msg::LiveReady`], both under the slot's lock: an [`Effect::Live`] that follows the
/// message always finds the handle, and nothing can drop the handle (and so send
/// [`Msg::LiveClosed`]) before the message is sent. Does nothing and returns false when
/// the session is no longer current or already has a live task.
fn install_live(
    slot: &LiveSlot,
    id: ConnId,
    scan: ScanId,
    tx: &Tx,
    make: impl FnOnce() -> LiveHandle,
) -> bool {
    let mut guard = lock(slot);
    let Some(current) = guard
        .as_mut()
        .filter(|s| s.id == id && s.scan == scan && s.handle.is_none())
    else {
        return false;
    };
    current.handle = Some(make());
    send(tx, Msg::LiveReady { id, scan })
}

/// Starts session `(id, scan)`'s live task on `conn` with the scan's `budget` and
/// installs it (see [`install_live`]). Its answers come back as [`Msg::Live`] and its
/// end as [`Msg::LiveClosed`].
fn start_live(
    slot: &LiveSlot,
    id: ConnId,
    scan: ScanId,
    conn: conn::Connection,
    budget: Arc<Budget>,
    tx: &Tx,
) -> bool {
    // Before the live task, so a subscribe that follows `LiveReady` finds the budget.
    if let Some(s) = lock(slot).as_mut().filter(|s| s.id == id && s.scan == scan) {
        s.nodes = vec![NodeRoute {
            budget: Arc::clone(&budget),
            addr: None,
        }];
    }
    install_live(slot, id, scan, tx, || {
        let (reply_tx, close_tx) = (tx.clone(), tx.clone());
        // Held until the task ends: through SSH the forward stays open only while a
        // `Connection` that uses it is alive, and the scan's is dropped when it ends.
        let route = conn.clone();
        live::spawn_with_close(
            conn.into_inner(),
            budget,
            move |req_id, result| {
                send(
                    &reply_tx,
                    Msg::Live {
                        id,
                        scan,
                        req_id,
                        result,
                    },
                );
            },
            move |error| {
                drop(route);
                send(&close_tx, Msg::LiveClosed { id, scan, error });
            },
        )
    })
}

/// [`start_live`] for a cluster: one live task over every shard's connection, each with
/// its node's budget ([`conn::cluster::NodeTarget::budget`]). Only the nodes discovery
/// connected to are in it with a connection; the others answer that they are not
/// connected.
fn start_live_cluster(
    slot: &LiveSlot,
    id: ConnId,
    scan: ScanId,
    targets: &ClusterTargets,
    tx: &Tx,
) -> bool {
    let mut routes = Vec::new();
    let mut held = Vec::new();
    let mut table = Vec::new();
    for t in &targets.nodes {
        let budget = t.budget.clone().unwrap_or_else(|| Budget::new(None));
        routes.push(NodeRoute {
            budget: Arc::clone(&budget),
            addr: Some(conn::NodeAddr {
                endpoint: t.endpoint.clone(),
                credentials: t.credentials,
            }),
        });
        // Held until the task ends, for the SSH forwards (see `start_live`).
        held.extend(t.conn.clone());
        table.push(live::nodes::Node {
            info: node_info(t),
            conn: t.conn.clone().map(conn::Connection::into_inner),
            budget,
        });
    }
    if let Some(s) = lock(slot).as_mut().filter(|s| s.id == id && s.scan == scan) {
        s.nodes = routes;
    }
    install_live(slot, id, scan, tx, || {
        let (reply_tx, close_tx) = (tx.clone(), tx.clone());
        live::spawn_nodes(
            live::nodes::Table::cluster(table),
            move |req_id, result| {
                send(
                    &reply_tx,
                    Msg::Live {
                        id,
                        scan,
                        req_id,
                        result,
                    },
                );
            },
            move |error| {
                drop(held);
                send(&close_tx, Msg::LiveClosed { id, scan, error });
            },
        )
    })
}

/// What the views know of a shard's node (spec "Cluster live views (P4)").
fn node_info(t: &conn::cluster::NodeTarget) -> live::nodes::NodeInfo {
    live::nodes::NodeInfo {
        id: t.id.clone(),
        endpoint: t.endpoint.clone(),
        role: t.role,
        slots: t.slots.clone(),
        connected: t.conn.is_some(),
    }
}

fn send(tx: &Tx, msg: Msg) -> bool {
    tx.send(Incoming::Msg(Box::new(msg))).is_ok()
}

/// Write `text` to the terminal output `out` as an OSC 52 sequence and flush. The
/// sequence moves no cursor and draws nothing, so the next frame is unaffected.
/// Returns the message to send when it could not be written.
fn copy_to(out: &mut impl Write, text: &CopyText) -> Option<Msg> {
    match clipboard::write_osc52(out, text) {
        Ok(true) => None,
        // The app checks the length before it asks; this is a second line of defence.
        Ok(false) => Some(Msg::CopyFailed(format!(
            "the {} is too long to copy",
            text.what()
        ))),
        Err(e) => Some(Msg::CopyFailed(e.to_string())),
    }
}

impl Runner {
    fn new(tx: Tx, path: PathBuf) -> Self {
        Self {
            tx,
            path,
            scan_task: None,
            live: Arc::new(Mutex::new(None)),
            scans: HashMap::new(),
        }
    }

    /// Makes `session` (or none) the current one. Dropping the old session's handle
    /// ends its live task, which then sends [`Msg::LiveClosed`] without an error.
    fn replace_live(&self, session: Option<LiveSession>) {
        let old = std::mem::replace(&mut *lock(&self.live), session);
        drop(old);
    }

    fn close_live(&self) {
        self.replace_live(None);
    }

    /// Sends `req` to `to` on session `id`'s live task, or answers it with
    /// [`LiveError::Closed`] when `id` isn't the current session or has no live task.
    fn send_live(&self, id: ConnId, req_id: live::ReqId, to: live::Dest, req: live::LiveRequest) {
        let sent = {
            let guard = lock(&self.live);
            match guard.as_ref() {
                Some(LiveSession {
                    id: current,
                    handle: Some(handle),
                    ..
                }) if *current == id => handle.send_to(req_id, to, req),
                _ => false,
            }
        };
        if !sent {
            send(
                &self.tx,
                Msg::Live {
                    id,
                    scan: self.scans.get(&id).copied().unwrap_or_default(),
                    req_id,
                    result: Err(LiveError::Closed),
                },
            );
        }
    }

    /// Starts live tail `tail` of session `(id, scan)` on node `node`, replacing (and so
    /// dropping) the session's other tail. When the session is not current or has no
    /// such node (not connected), answers [`Msg::TailClosed`] at once.
    #[allow(clippy::too_many_arguments)]
    fn subscribe(
        &self,
        id: ConnId,
        scan: ScanId,
        tail: u64,
        profile: Profile,
        secrets: SecretSource,
        to: Subscription,
        node: usize,
    ) {
        let mut guard = lock(&self.live);
        let Some(s) = guard.as_mut().filter(|s| s.id == id && s.scan == scan) else {
            drop(guard);
            send(
                &self.tx,
                Msg::TailClosed {
                    id,
                    scan,
                    tail,
                    error: Some(LiveError::Closed.to_string()),
                },
            );
            return;
        };
        let Some(route) = s.nodes.get(node).cloned() else {
            drop(guard);
            send(
                &self.tx,
                Msg::TailClosed {
                    id,
                    scan,
                    tail,
                    error: Some(LiveError::Closed.to_string()),
                },
            );
            return;
        };
        // The old tail's connection goes first.
        s.tail = None;
        let tx = self.tx.clone();
        s.tail = Some(TailTask {
            tail,
            _task: AbortOnDrop(tokio::spawn(run_tail(
                TailRun { id, scan, tail },
                profile,
                secrets,
                to,
                route,
                tx,
            ))),
        });
    }

    /// Starts Monitor run `run` of session `(id, scan)` on node `node`, replacing (and so
    /// dropping) the session's other run. When the session is not current or has no
    /// such node, answers [`Msg::MonitorClosed`] at once.
    fn monitor(
        &self,
        id: ConnId,
        scan: ScanId,
        run: u64,
        profile: Profile,
        secrets: SecretSource,
        node: usize,
    ) {
        let mut guard = lock(&self.live);
        let found = guard
            .as_mut()
            .filter(|s| s.id == id && s.scan == scan)
            .and_then(|s| s.nodes.get(node).cloned().map(|b| (s, b)));
        let Some((s, route)) = found else {
            drop(guard);
            send(
                &self.tx,
                Msg::MonitorClosed {
                    id,
                    scan,
                    run,
                    end: Err(LiveError::Closed.to_string()),
                },
            );
            return;
        };
        s.monitor = None;
        let tx = self.tx.clone();
        s.monitor = Some(TailTask {
            tail: run,
            _task: AbortOnDrop(tokio::spawn(run_monitor(
                TailRun {
                    id,
                    scan,
                    tail: run,
                },
                profile,
                secrets,
                route,
                tx,
            ))),
        });
    }

    /// Returns true when the app should quit. `out` is the terminal's output, for
    /// effects that write to the terminal itself.
    fn run(&mut self, effect: Effect, out: &mut impl Write) -> bool {
        let tx = self.tx.clone();
        match effect {
            Effect::Quit => {
                self.close_live();
                return true;
            }
            Effect::Live {
                id,
                req_id,
                to,
                req,
            } => self.send_live(id, req_id, to, req),
            Effect::Subscribe {
                id,
                scan,
                tail,
                profile,
                secrets,
                to,
                node,
            } => self.subscribe(id, scan, tail, profile, secrets, to, node),
            Effect::Monitor {
                id,
                scan,
                run,
                profile,
                secrets,
                node,
            } => self.monitor(id, scan, run, profile, secrets, node),
            Effect::StopMonitor { id, run } => {
                if let Some(s) = lock(&self.live).as_mut()
                    && s.id == id
                    && s.monitor.as_ref().is_some_and(|t| t.tail == run)
                {
                    s.monitor = None;
                }
            }
            Effect::Unsubscribe { id, tail } => {
                if let Some(s) = lock(&self.live).as_mut()
                    && s.id == id
                    && s.tail.as_ref().is_some_and(|t| t.tail == tail)
                {
                    s.tail = None;
                }
            }
            Effect::Copy(text) => {
                if let Some(msg) = copy_to(out, &text) {
                    send(&tx, msg);
                }
            }
            Effect::Editor(job) => {
                // Blocks the event loop while the editor runs: nothing is drawn then.
                let result = editor::run(&job, out);
                send(
                    &tx,
                    Msg::Edited {
                        job: job.id,
                        result,
                    },
                );
            }
            Effect::DiscoverDocker => {
                tokio::spawn(async move {
                    let msg = match conn::docker::discover().await {
                        Ok(found) => Msg::DockerFound(found),
                        Err(e) => Msg::DockerFailed(e.to_string()),
                    };
                    send(&tx, msg);
                });
            }
            Effect::Connect {
                id,
                scan,
                profile,
                secrets,
            } => {
                if let Some(t) = self.scan_task.take() {
                    t.abort();
                }
                self.scans.insert(id, scan);
                self.replace_live(Some(LiveSession::new(id, scan)));
                self.scan_task = Some(tokio::spawn(connect_and_scan(
                    id,
                    scan,
                    profile,
                    secrets,
                    tx,
                    Arc::clone(&self.live),
                )));
            }
            Effect::SetWrites { name, on } => {
                let path = self.path.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match save_writes(&path, &name, on) {
                        Ok(()) => Msg::WritesSaved { name, on },
                        Err(e) => Msg::WritesFailed {
                            name,
                            on,
                            error: e.to_string(),
                        },
                    };
                    send(&tx, msg);
                });
            }
            Effect::SaveConnection {
                profile,
                credentials,
            } => {
                let path = self.path.clone();
                tokio::task::spawn_blocking(move || {
                    let msg =
                        match save_connection(&path, &KeyringStore, &profile, credentials.as_ref())
                        {
                            Ok(()) => Msg::Saved(profile),
                            Err(e) => Msg::SaveFailed(e.to_string()),
                        };
                    send(&tx, msg);
                });
            }
            Effect::LoadHistory { id, scan, report } => {
                tokio::task::spawn_blocking(move || {
                    let taken_at_unix = report.taken_at_unix;
                    let result = load_history(&report).map_err(|e| e.to_string());
                    send(
                        &tx,
                        Msg::History {
                            id,
                            scan,
                            taken_at_unix,
                            result,
                        },
                    );
                    // A failure is already reported through the history; without a
                    // trend the Diff view just has no trend column.
                    let trend = snapshot::default_dir()
                        .ok()
                        .and_then(|dir| load_trend(&dir, &report).ok());
                    send(
                        &tx,
                        Msg::Trend {
                            id,
                            scan,
                            taken_at_unix,
                            trend,
                        },
                    );
                });
            }
            Effect::SaveSnapshot(report) => {
                tokio::task::spawn_blocking(move || {
                    let msg =
                        match snapshot::default_dir().and_then(|d| snapshot::save(&d, &report)) {
                            Ok(path) => Msg::SnapshotSaved(path),
                            Err(e) => Msg::SnapshotFailed(e.to_string()),
                        };
                    send(&tx, msg);
                });
            }
        }
        false
    }
}

/// The diff between the newest snapshot older than `report` and `report`.
fn load_history(report: &ScanReport) -> Result<Option<diff::ReportDiff>, snapshot::SnapshotError> {
    let dir = snapshot::default_dir()?;
    let prev = snapshot::latest_before(
        &dir,
        &report.profile,
        report.taken_at_unix,
        report.match_glob.as_deref(),
    )?;
    Ok(prev.map(|p| diff::diff(&p, report)))
}

/// The trend over the [`app::TREND_SNAPSHOTS`] newest snapshots in `dir` taken before
/// `report` with its `MATCH` glob, with `report` itself as the last point.
fn load_trend(dir: &Path, report: &ScanReport) -> Result<snapshot::Trend, snapshot::SnapshotError> {
    let mut reports = snapshot::recent_matching(
        dir,
        &report.profile,
        report.taken_at_unix,
        report.match_glob.as_deref(),
        app::TREND_SNAPSHOTS,
    )?;
    reports.push(report.clone());
    Ok(snapshot::trend(&reports))
}

/// `w` on Connections: `read_only` of profile `name` in the config file, and nothing
/// else (spec "0 Connections, additions").
fn save_writes(path: &Path, name: &str, on: bool) -> anyhow::Result<()> {
    let mut config = Config::load(path)?;
    config.set_writes(name, on)?;
    config.save(path)?;
    Ok(())
}

/// Save a new profile the way `conn add` does: credentials to the keychain first, so
/// a saved profile never points at a missing entry, then the config file. If the
/// config can't be written, the keychain entry is put back as it was. Refuses to
/// replace an existing profile, and checks that before touching the keychain.
fn save_connection(
    path: &Path,
    secrets: &dyn SecretStore,
    profile: &Profile,
    credentials: Option<&Credentials>,
) -> anyhow::Result<()> {
    let mut config = Config::load(path)?;
    if config.get(&profile.name).is_some() {
        bail!("a profile named {} already exists", profile.name);
    }
    config.add(profile.clone(), false)?;
    match credentials {
        Some(c) => {
            crate::cli::conn::set_credentials_then_save(secrets, &profile.secret_ref(), c, || {
                Ok(config.save(path)?)
            })
        }
        None => Ok(config.save(path)?),
    }
}

/// Remove a known secret from error text, as a second line of defence.
fn scrub(text: String, secret: Option<&str>) -> String {
    match secret {
        Some(s) if !s.is_empty() => text.replace(s, "******"),
        _ => text,
    }
}

/// A fallback reason is free text built from server replies and addresses: it goes
/// through the same `clean` as error messages before the TUI shows it. A cluster's
/// summary is fixed text and counts, cleaned all the same.
fn clean_target(target: ScanTarget, clean: impl Fn(String) -> String) -> ScanTarget {
    match target {
        ScanTarget::PrimaryFallback(why) => ScanTarget::PrimaryFallback(clean(why)),
        ScanTarget::Cluster(summary) => ScanTarget::Cluster(clean(summary)),
        t => t,
    }
}

/// Prefix of the report (and so snapshot) name of a Docker container, so a container
/// called `cache` never shares history with a saved profile called `cache`.
const DOCKER_PREFIX: &str = crate::cli::conn::DOCKER_PREFIX;

/// The name a scan of `profile` is reported and snapshotted under: the profile name for
/// saved profiles, `docker:<container>` for containers found in Docker.
fn report_name(profile: &Profile, secrets: &SecretSource) -> String {
    match secrets {
        SecretSource::Keychain => profile.name.clone(),
        SecretSource::Container(_) => format!("{DOCKER_PREFIX}{}", profile.name),
    }
}

/// Pass progress and partial results on as messages until both senders are dropped.
async fn forward_progress(
    id: ConnId,
    scan: ScanId,
    mut progress: watch::Receiver<ScanProgress>,
    mut partial: watch::Receiver<Option<Arc<scan::PartialPatterns>>>,
    tx: Tx,
) {
    let (mut progress_open, mut partial_open) = (true, true);
    while progress_open || partial_open {
        let msg = tokio::select! {
            r = progress.changed(), if progress_open => match r {
                Ok(()) => Some(Msg::ScanProgress {
                    id,
                    scan,
                    progress: *progress.borrow_and_update(),
                }),
                Err(_) => {
                    progress_open = false;
                    None
                }
            },
            r = partial.changed(), if partial_open => match r {
                Ok(()) => partial
                    .borrow_and_update()
                    .clone()
                    .map(|partial| Msg::ScanPartial { id, scan, partial }),
                Err(_) => {
                    partial_open = false;
                    None
                }
            },
        };
        if let Some(msg) = msg
            && !send(&tx, msg)
        {
            break;
        }
    }
}

/// Aborts the task when dropped, so a forwarder never outlives the scan it serves:
/// when the scan task is aborted (a new connect replaced it), its forwarder stops too
/// instead of passing on the replaced scan's last progress and partials.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type ProgressTx = watch::Sender<ScanProgress>;
type PartialTx = watch::Sender<Option<Arc<scan::PartialPatterns>>>;

/// Run one scan and report it: start the forwarder, run the scan (`run`) with the
/// progress and partial senders, then send `ScanDone` or `ScanFailed`. Every progress and partial
/// message of this scan arrives before that final message, and none after it.
async fn scan_and_report<F, Fut>(id: ConnId, scan: ScanId, tx: Tx, run: F)
where
    F: FnOnce(ProgressTx, PartialTx) -> Fut,
    Fut: Future<Output = Result<ScanReport, String>>,
{
    let (ptx, prx) = watch::channel(ScanProgress::default());
    let (partial_tx, partial_rx) = watch::channel(None);
    let mut forward = AbortOnDrop(tokio::spawn(forward_progress(
        id,
        scan,
        prx,
        partial_rx,
        tx.clone(),
    )));
    // The future `run` returns owns the senders, so they are dropped with it as soon
    // as this await completes.
    // That ends the forwarder once it has passed on the last values: awaiting it here
    // puts every progress and partial message before ScanDone or ScanFailed.
    let result = run(ptx, partial_tx).await;
    let _ = (&mut forward.0).await;
    send(
        &tx,
        match result {
            Ok(report) => Msg::ScanDone {
                id,
                scan,
                report: Box::new(report),
            },
            Err(error) => Msg::ScanFailed { id, scan, error },
        },
    );
}

/// The store to read `profile`'s credentials from: the keychain for a saved profile,
/// memory for a Docker container's; with the container's password, to scrub from error
/// text (see [`scrub`]).
fn store_for(profile: &Profile, secrets: SecretSource) -> (Box<dyn SecretStore>, Option<String>) {
    match secrets {
        SecretSource::Keychain => (Box::new(KeyringStore), None),
        SecretSource::Container(creds) => {
            let store = MemoryStore::default();
            let secret = creds
                .as_ref()
                .map(|c| c.password.expose_secret().to_string());
            if let Some(c) = &creds {
                // MemoryStore::set cannot fail.
                let _ = store.set(&profile.secret_ref(), c);
            }
            (Box::new(store), secret)
        }
    }
}

/// Which live tail a message is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TailRun {
    id: ConnId,
    scan: ScanId,
    tail: u64,
}

/// The tail forwards messages at most this often.
const TAIL_BATCH_EVERY: Duration = Duration::from_millis(100);

/// The messages a tail received since its last batch: the newest [`TAIL_KEPT`], with
/// how many older ones were left out.
#[derive(Debug, Default)]
struct TailBatch {
    msgs: VecDeque<TailMessage>,
    dropped: u64,
}

impl TailBatch {
    fn push(&mut self, m: TailMessage) {
        if self.msgs.len() == TAIL_KEPT {
            self.msgs.pop_front();
            self.dropped += 1;
        }
        self.msgs.push_back(m);
    }

    /// The batch to send, oldest first, and the count left out; `None` when nothing
    /// arrived. Starts the next batch empty.
    fn take(&mut self) -> Option<(Vec<TailMessage>, u64)> {
        if self.msgs.is_empty() && self.dropped == 0 {
            return None;
        }
        let b = std::mem::take(self);
        Some((b.msgs.into(), b.dropped))
    }
}

/// The time now, UTC, in milliseconds since the Unix epoch: a tail message's stamp.
fn unix_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Why opening the tail's connection failed, in the words of a connect error (cleaned
/// with `clean`), or a fixed text when it is the tail's own (RESP3 or `HELLO` refused).
fn tail_open_error(e: &conn::ConnError, clean: impl Fn(String) -> String) -> String {
    match live::pubsub::open_error(e) {
        Some(fixed) => fixed.to_string(),
        None => clean(e.to_string()),
    }
}

/// One live tail, from connecting to its end: opens its own connection
/// (`conn::open_subscriber`), subscribes, then forwards what arrives in batches
/// ([`forward_tail`]) until the connection is lost. Every message about it is
/// [`Msg::TailReady`], [`Msg::Tail`] or [`Msg::TailClosed`]. Aborting the task (the
/// tail was replaced or stopped) drops the connection and sends nothing more.
async fn run_tail(
    run: TailRun,
    profile: Profile,
    secrets: SecretSource,
    to: Subscription,
    route: NodeRoute,
    tx: Tx,
) {
    let TailRun { id, scan, tail } = run;
    let budget = route.budget;
    let closed = |error: String| {
        send(
            &tx,
            Msg::TailClosed {
                id,
                scan,
                tail,
                error: Some(error),
            },
        );
    };
    let (store, secret) = store_for(&profile, secrets);
    let clean = |e: String| conn::parse::mask(&scrub(e, secret.as_deref()));
    let opened = conn::open_subscriber(&profile, store.as_ref(), route.addr.as_ref()).await;
    let (mut conn, mut pushes) = match opened {
        Ok(c) => c,
        Err(e) => return closed(tail_open_error(&e, clean)),
    };
    drop(store);
    if let Err(why) = live::pubsub::subscribe(&mut conn, &budget, &to).await {
        return closed(why);
    }
    if !send(&tx, Msg::TailReady { id, scan, tail }) {
        return;
    }
    let sent = forward_tail(&mut pushes, unix_ms_now, |batch, dropped| {
        send(
            &tx,
            Msg::Tail {
                id,
                scan,
                tail,
                batch,
                dropped,
            },
        )
    })
    .await;
    // Held until here: through SSH the forward stays open only while it is alive.
    drop(conn);
    if sent {
        closed(live::CONNECTION_LOST.to_string());
    }
}

/// One Monitor run, from connecting to its end: `conn::open_monitor` (one `MONITOR` from
/// the budget for its check, one for the run), then the lines in batches
/// (`live::monitor::forward`) until a limit or the connection's end. Every message about
/// it is [`Msg::MonitorReady`], [`Msg::MonitorLines`] or [`Msg::MonitorClosed`]. A
/// refusal is said in fixed words (`live::error_for`): never the server's text, which can
/// name the user. Aborting the task drops the connection and sends nothing more.
async fn run_monitor(
    run: TailRun,
    profile: Profile,
    secrets: SecretSource,
    route: NodeRoute,
    tx: Tx,
) {
    let TailRun {
        id,
        scan,
        tail: run,
    } = run;
    let budget = route.budget;
    let closed = |end: Result<live::monitor::End, String>| {
        send(&tx, Msg::MonitorClosed { id, scan, run, end });
    };
    let (store, secret) = store_for(&profile, secrets);
    let clean = |e: String| conn::parse::mask(&scrub(e, secret.as_deref()));
    let opened = conn::open_monitor(&profile, store.as_ref(), &budget, route.addr.as_ref()).await;
    drop(store);
    let mut mon = match opened {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return closed(Err(live::error_for("MONITOR", &e).to_string())),
        Err(e) => return closed(Err(clean(e.to_string()))),
    };
    if !send(
        &tx,
        Msg::MonitorReady {
            id,
            scan,
            run,
            at_unix_ms: unix_ms_now(),
        },
    ) {
        return;
    }
    let end = live::monitor::forward(&mut mon.lines, live::monitor::Limits::default(), |lines| {
        send(
            &tx,
            Msg::MonitorLines {
                id,
                scan,
                run,
                lines,
            },
        )
    })
    .await;
    // Closed before saying so, so the server stops feeding it at once.
    drop(mon);
    if end != live::monitor::End::Gone {
        closed(Ok(end));
    }
}

/// Reads `pushes` until the connection is lost, passing the messages on to `batch` at
/// most every [`TAIL_BATCH_EVERY`], at most [`TAIL_KEPT`] at a time (the newest), each
/// stamped with `now` when it arrived. What is left when the connection goes is passed
/// on first. False when `batch` returned false (the app is gone).
async fn forward_tail(
    pushes: &mut mpsc::UnboundedReceiver<redis::PushInfo>,
    now: impl Fn() -> i64,
    mut batch: impl FnMut(Vec<TailMessage>, u64) -> bool,
) -> bool {
    let mut pending = TailBatch::default();
    let mut every = tokio::time::interval(TAIL_BATCH_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            push = pushes.recv() => {
                let lost = match push {
                    None => true,
                    Some(p) => match live::pubsub::parse_push(p, now()) {
                        live::pubsub::Push::Message(m) => {
                            pending.push(m);
                            false
                        }
                        live::pubsub::Push::Lost => true,
                        live::pubsub::Push::Other => false,
                    },
                };
                if lost {
                    return pending.take().is_none_or(|(b, d)| batch(b, d));
                }
            }
            _ = every.tick() => {
                if let Some((b, d)) = pending.take()
                    && !batch(b, d)
                {
                    return false;
                }
            }
        }
    }
}

async fn connect_and_scan(
    id: ConnId,
    scan_id: ScanId,
    profile: Profile,
    secrets: SecretSource,
    tx: Tx,
    live: LiveSlot,
) {
    let name = report_name(&profile, &secrets);
    let (store, secret) = store_for(&profile, secrets);
    let clean = |e: String| conn::parse::mask(&scrub(e, secret.as_deref()));

    // A standalone server, or every shard of a cluster.
    let mut targets = match conn::cluster::connect_targets(&profile, store.as_ref()).await {
        Ok(t) => t,
        Err(e) => {
            send(
                &tx,
                Msg::ConnectFailed {
                    id,
                    scan: scan_id,
                    error: clean(e.to_string()),
                },
            );
            return;
        }
    };
    // A Sentinel profile: which Sentinel named the primary, and where (fixed fields, no
    // credential), for the header and the Connections detail.
    if let ScanTargets::Single { conn, .. } = &targets
        && let Some(answer) = conn.sentinel()
        && !send(
            &tx,
            Msg::SentinelResolved {
                id,
                scan: scan_id,
                answer: answer.clone(),
            },
        )
    {
        return;
    }
    let target = clean_target(targets.target(), clean);
    // One budget for this session: the scan and the live connection both take from it.
    // The permission probes run once on connect and don't. On a cluster each node has a
    // budget of its own instead, shared by its scan and its live requests, and the
    // probes run on every node through its budget.
    let opts = ScanOptions::for_profile(&profile);
    let budget = Budget::new(opts.max_commands_per_sec);
    let (perms, server) = match &mut targets {
        ScanTargets::Single { conn, .. } => {
            let perms = conn::check_all_permissions(conn).await;
            (
                perms,
                scan::server_info_with_budget(conn, &budget).await.ok(),
            )
        }
        ScanTargets::Cluster(c) => {
            for node in &mut c.nodes {
                node.budget = Some(Budget::new(opts.max_commands_per_sec));
            }
            (
                c.check_all_permissions().await,
                cluster_server_info(c).await,
            )
        }
    };
    if !send(
        &tx,
        Msg::Connected {
            id,
            scan: scan_id,
            server,
            perms: perms.clone(),
            target,
        },
    ) {
        return;
    }
    // The live views read from the node the scan reads from (the replica when one is
    // preferred). The live task keeps its clone open after the scan ends. On a cluster,
    // the views learn the nodes first, then the live task starts over all of them.
    let started = match &targets {
        ScanTargets::Single { conn, .. } => {
            start_live(&live, id, scan_id, conn.clone(), Arc::clone(&budget), &tx)
        }
        ScanTargets::Cluster(c) => {
            let nodes = c.nodes.iter().map(node_info).collect();
            send(
                &tx,
                Msg::Nodes {
                    id,
                    scan: scan_id,
                    nodes,
                },
            ) && start_live_cluster(&live, id, scan_id, c, &tx)
        }
    };
    if !started {
        return;
    }

    scan_and_report(id, scan_id, tx, |ptx, partial_tx| async move {
        match &mut targets {
            ScanTargets::Single { conn, .. } => {
                scan::scan_with_budget(
                    &name,
                    conn,
                    &opts,
                    &perms,
                    Some(&ptx),
                    Some(&partial_tx),
                    &budget,
                )
                .await
            }
            ScanTargets::Cluster(_) => {
                scan::cluster::scan_targets(
                    &name,
                    &mut targets,
                    &opts,
                    &perms,
                    Some(&ptx),
                    Some(&partial_tx),
                )
                .await
            }
        }
        .map_err(|e| clean(e.to_string()))
    })
    .await;
}

/// The header's server line before a cluster scan finishes: `INFO` and `DBSIZE` of
/// every connected node, merged by [`merge_servers`]. `None` when no node answered.
async fn cluster_server_info(c: &mut ClusterTargets) -> Option<ServerInfo> {
    let mut found = Vec::new();
    for node in &mut c.nodes {
        if let Some(conn) = node.conn.as_mut()
            && let Ok(s) = scan::server_info(conn).await
        {
            found.push(s);
        }
    }
    merge_servers(found)
}

/// One server line for a cluster, the way the finished report has it: the first node's
/// flavor, version and policy, with memory, the limit and keys summed over the nodes.
fn merge_servers(servers: Vec<ServerInfo>) -> Option<ServerInfo> {
    let mut it = servers.into_iter();
    let mut out = it.next()?;
    out.mode = ServerMode::Cluster;
    for s in it {
        out.used_memory = out.used_memory.saturating_add(s.used_memory);
        out.maxmemory = out.maxmemory.saturating_add(s.maxmemory);
        out.total_keys = out.total_keys.saturating_add(s.total_keys);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Auth, Endpoint, Env};
    use secrecy::SecretString;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "redis-lens-tui-{tag}-{}-{nanos}",
                std::process::id()
            ))
            .join("config.toml")
    }

    #[test]
    fn trend_uses_the_earlier_snapshots_with_the_same_glob() {
        let dir = temp_path("trend").parent().unwrap().to_path_buf();
        let report = fixtures::report(true);
        let t0 = report.taken_at_unix;
        // Nine earlier snapshots a day apart, one with another glob, one newer than the scan.
        for d in 1..=9 {
            let mut r = report.clone();
            r.taken_at_unix = t0 - d * 86_400;
            if d == 2 {
                r.match_glob = Some("user:*".into());
            }
            snapshot::save(&dir, &r).unwrap();
        }
        let mut newer = report.clone();
        newer.taken_at_unix = t0 + 60;
        snapshot::save(&dir, &newer).unwrap();
        let t = load_trend(&dir, &report).unwrap();
        let at: Vec<i64> = t.points.iter().map(|p| p.taken_at_unix).collect();
        // Seven earlier ones without a glob (day 2 skipped, days 9.. too old), then the scan.
        let expect: Vec<i64> = [8, 7, 6, 5, 4, 3, 1]
            .iter()
            .map(|d| t0 - d * 86_400)
            .chain([t0])
            .collect();
        assert_eq!(at, expect);
        assert_eq!(t.points.len(), app::TREND_SNAPSHOTS + 1);
        // Nothing saved yet: the scan alone.
        let empty = temp_path("trend-empty").parent().unwrap().to_path_buf();
        assert_eq!(load_trend(&empty, &report).unwrap().points.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn profile(name: &str) -> Profile {
        Profile {
            name: name.into(),
            env: Env::Prod,
            endpoint: Endpoint {
                host: "prod-cache-02.internal".into(),
                port: 6380,
                tls: true,
            },
            db: 0,
            auth: Auth::Keychain,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn save_writes_keychain_then_config_without_secrets() {
        let path = temp_path("save");
        let store = MemoryStore::default();
        let p = profile("prod-cache-02");
        let creds = Credentials {
            username: Some("lens-ro".into()),
            password: SecretString::from("Sup3rS3cret"),
        };
        save_connection(&path, &store, &p, Some(&creds)).unwrap();
        let got = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(got.password.expose_secret(), "Sup3rS3cret");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("prod-cache-02"));
        assert!(!text.contains("Sup3rS3cret"));
        assert!(!text.contains("lens-ro"));
        assert_eq!(Config::load(&path).unwrap().profiles, vec![p.clone()]);

        // A second save under the same name fails before touching the keychain.
        let other = Credentials {
            username: None,
            password: SecretString::from("other"),
        };
        assert!(save_connection(&path, &store, &p, Some(&other)).is_err());
        let still = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(still.password.expose_secret(), "Sup3rS3cret");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_save_leaves_keychain_as_it_was() {
        // A directory where `Config::save` writes its temp file makes the save fail
        // while the load still works.
        let path = temp_path("save-fails");
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
        std::fs::create_dir_all(tmp.join("blocker")).unwrap();

        let creds = Credentials {
            username: Some("lens-ro".into()),
            password: SecretString::from("Sup3rS3cret"),
        };
        // No entry before: none after.
        let store = MemoryStore::default();
        let p = profile("new-one");
        assert!(save_connection(&path, &store, &p, Some(&creds)).is_err());
        assert!(store.get(&p.secret_ref()).unwrap().is_none());

        // A leftover entry before: the same entry after.
        let old = Credentials {
            username: None,
            password: SecretString::from("leftover"),
        };
        store.set(&p.secret_ref(), &old).unwrap();
        assert!(save_connection(&path, &store, &p, Some(&creds)).is_err());
        let got = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(got.username, None);
        assert_eq!(got.password.expose_secret(), "leftover");
        assert!(Config::load(&path).unwrap().profiles.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn forwarder_passes_on_the_last_progress_and_partial_then_ends() {
        let (ptx, prx) = watch::channel(ScanProgress::default());
        let (partial_tx, partial_rx) = watch::channel(None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let forward = tokio::spawn(forward_progress(7, 70, prx, partial_rx, tx));
        let partial = crate::tui::fixtures::partial(true);
        ptx.send_replace(ScanProgress {
            sampled: 3_200,
            target: 100_000,
            done: false,
        });
        partial_tx.send_replace(Some(partial.clone()));
        // Dropped right after sending, as at the end of a scan: nothing is lost.
        drop(ptx);
        drop(partial_tx);
        forward.await.unwrap();

        let (mut last_progress, mut last_partial) = (None, None);
        while let Ok(incoming) = rx.try_recv() {
            match msg_of(incoming) {
                Msg::ScanProgress { id, scan, progress } => {
                    assert_eq!((id, scan), (7, 70));
                    last_progress = Some(progress);
                }
                Msg::ScanPartial { id, scan, partial } => {
                    assert_eq!((id, scan), (7, 70));
                    last_partial = Some(partial);
                }
                _ => panic!("unexpected message"),
            }
        }
        assert_eq!(last_progress.unwrap().sampled, 3_200);
        assert_eq!(last_partial.unwrap(), partial);
    }

    #[tokio::test]
    async fn forwarder_stops_when_the_app_is_gone() {
        let (ptx, prx) = watch::channel(ScanProgress::default());
        let (_partial_tx, partial_rx) = watch::channel(None);
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let forward = tokio::spawn(forward_progress(1, 10, prx, partial_rx, tx));
        ptx.send_replace(ScanProgress {
            sampled: 1,
            target: 2,
            done: false,
        });
        // Ends even though both senders are still alive.
        tokio::time::timeout(Duration::from_secs(5), forward)
            .await
            .expect("forwarder kept running")
            .unwrap();
    }

    /// Run `scan_and_report` with a scan that sends many progress and partial values,
    /// the last of them right before it returns `result`, then return every message.
    async fn messages_of_a_scan(result: Result<ScanReport, String>) -> Vec<Msg> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let partial = crate::tui::fixtures::partial(true);
        scan_and_report(3, 30, tx, |ptx, partial_tx| async move {
            for i in 1..=50 {
                ptx.send_replace(ScanProgress {
                    sampled: i,
                    target: 50,
                    done: false,
                });
                partial_tx.send_replace(Some(partial.clone()));
                if i % 7 == 0 {
                    // Let the forwarder pass some on while the scan runs.
                    tokio::task::yield_now().await;
                }
            }
            // The last values are set with no chance for the forwarder to run before
            // the scan returns.
            ptx.send_replace(ScanProgress {
                sampled: 50,
                target: 50,
                done: true,
            });
            partial_tx.send_replace(Some(partial.clone()));
            result
        })
        .await;
        let mut out = Vec::new();
        // Every sender is gone once scan_and_report returns: this ends.
        while let Some(incoming) = rx.recv().await {
            out.push(msg_of(incoming));
        }
        out
    }

    /// Checks that `msgs` is progress and partials of run (3, 30), the last progress
    /// being the scan's final value, then exactly one final message, returned.
    fn split_final(mut msgs: Vec<Msg>) -> Msg {
        let last = msgs.pop().expect("no messages");
        let (mut last_progress, mut partials) = (None, 0);
        for m in msgs {
            match m {
                Msg::ScanProgress { id, scan, progress } => {
                    assert_eq!((id, scan), (3, 30));
                    last_progress = Some(progress);
                }
                Msg::ScanPartial { id, scan, .. } => {
                    assert_eq!((id, scan), (3, 30));
                    partials += 1;
                }
                other => panic!("{other:?} before the end of the scan"),
            }
        }
        let p = last_progress.expect("no progress");
        assert!(
            p.done && p.sampled == 50,
            "the final progress came late: {p:?}"
        );
        assert!(partials > 0);
        last
    }

    #[tokio::test]
    async fn progress_and_partials_all_arrive_before_scan_done() {
        let report = crate::tui::fixtures::report(true);
        let last = split_final(messages_of_a_scan(Ok(report.clone())).await);
        let Msg::ScanDone {
            id,
            scan,
            report: r,
        } = last
        else {
            panic!("expected ScanDone last, got {last:?}");
        };
        assert_eq!((id, scan), (3, 30));
        assert_eq!(*r, report);
    }

    #[tokio::test]
    async fn progress_and_partials_all_arrive_before_scan_failed() {
        let last = split_final(messages_of_a_scan(Err("connection reset".into())).await);
        let Msg::ScanFailed { id, scan, error } = last else {
            panic!("expected ScanFailed last, got {last:?}");
        };
        assert_eq!((id, scan), (3, 30));
        assert_eq!(error, "connection reset");
    }

    #[tokio::test]
    async fn aborting_the_scan_task_stops_its_forwarder() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (keep_tx, keep_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(scan_and_report(4, 40, tx, |ptx, partial_tx| async move {
            ptx.send_replace(ScanProgress {
                sampled: 1,
                target: 10,
                done: false,
            });
            // Hand the senders out so they outlive the task: a forwarder that kept
            // running would pass on whatever they get next.
            let _ = keep_tx.send((ptx, partial_tx));
            std::future::pending::<Result<ScanReport, String>>().await
        }));
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no progress")
            .expect("channel closed");
        assert!(matches!(
            msg_of(first),
            Msg::ScanProgress {
                id: 4,
                scan: 40,
                ..
            }
        ));
        let (ptx, partial_tx) = keep_rx.await.unwrap();

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        ptx.send_replace(ScanProgress {
            sampled: 2,
            target: 10,
            done: false,
        });
        partial_tx.send_replace(Some(crate::tui::fixtures::partial(true)));
        // The forwarder held the last sender of the channel: it closes with nothing
        // more in it.
        let next = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the channel stayed open: the forwarder outlived the aborted scan");
        assert!(
            next.is_none(),
            "a message arrived after the scan was aborted"
        );
    }

    #[test]
    fn docker_scans_are_filed_apart_from_saved_profiles() {
        let p = profile("cache");
        assert_eq!(report_name(&p, &SecretSource::Keychain), "cache");
        assert_eq!(
            report_name(&p, &SecretSource::Container(None)),
            "docker:cache"
        );
        let creds = Credentials {
            username: None,
            password: SecretString::from("pw"),
        };
        assert_eq!(
            report_name(&p, &SecretSource::Container(Some(creds))),
            "docker:cache"
        );
    }

    #[test]
    fn fallback_reasons_are_cleaned() {
        let secret = Some("hunter2");
        let clean = |e: String| conn::parse::mask(&scrub(e, secret));
        let t = clean_target(
            ScanTarget::PrimaryFallback(
                "10.0.0.1:6379: hunter2 refused for redis://u:s3cretAAA@h:6379".into(),
            ),
            clean,
        );
        let ScanTarget::PrimaryFallback(why) = t else {
            panic!("{t:?}");
        };
        assert!(!why.contains("hunter2"), "{why}");
        assert!(!why.contains("s3cretAAA"), "{why}");
        assert!(why.starts_with("10.0.0.1:6379: ******"), "{why}");
        assert_eq!(
            clean_target(ScanTarget::Primary, clean),
            ScanTarget::Primary
        );
    }

    #[test]
    fn cluster_summaries_are_cleaned_too() {
        let secret = Some("hunter2");
        let clean = |e: String| conn::parse::mask(&scrub(e, secret));
        let summary = "cluster · 3 shards · replicas 2/3 · 1 unreachable";
        assert_eq!(
            clean_target(ScanTarget::Cluster(summary.into()), clean),
            ScanTarget::Cluster(summary.into())
        );
        let t = clean_target(ScanTarget::Cluster("cluster · hunter2".into()), clean);
        assert_eq!(t, ScanTarget::Cluster("cluster · ******".into()));
    }

    #[test]
    fn cluster_server_lines_sum_memory_limits_and_keys() {
        assert_eq!(merge_servers(Vec::new()), None);
        let node = |used: u64, max: u64, keys: u64, version: &str| ServerInfo {
            version: version.into(),
            used_memory: used,
            maxmemory: max,
            total_keys: keys,
            ..crate::tui::fixtures::server()
        };
        let one = merge_servers(vec![node(10, 100, 3, "7.4.2")]).unwrap();
        assert_eq!(one.mode, ServerMode::Cluster);
        assert_eq!(
            (one.used_memory, one.maxmemory, one.total_keys),
            (10, 100, 3)
        );
        let m = merge_servers(vec![
            node(10, 100, 3, "7.4.2"),
            node(20, 0, 4, "7.4.1"),
            node(30, 300, 5, "7.4.0"),
        ])
        .unwrap();
        // The first node's version and policy, the rest summed.
        assert_eq!(m.version, "7.4.2");
        assert_eq!(m.maxmemory_policy, "allkeys-lfu");
        assert_eq!((m.used_memory, m.maxmemory, m.total_keys), (60, 400, 12));
        assert_eq!(m.mode, ServerMode::Cluster);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spawned_scan_reports_the_whole_cluster_report() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let report = crate::tui::fixtures::cluster_report();
        let sent = report.clone();
        let _task = tokio::spawn(scan_and_report(
            3,
            30,
            tx,
            move |ptx, _partial| async move {
                ptx.send_replace(ScanProgress {
                    sampled: 1,
                    target: 2,
                    done: false,
                });
                Ok(sent)
            },
        ));
        let mut done = None;
        while let Some(incoming) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a message")
        {
            if let Incoming::Msg(m) = incoming
                && let Msg::ScanDone { id, scan, report } = *m
            {
                done = Some((id, scan, *report));
                break;
            }
        }
        let (id, scan, got) = done.expect("ScanDone");
        assert_eq!((id, scan), (3, 30));
        // The cluster report arrives whole: nodes and the hot keys' node indices.
        assert_eq!(got, report);
        assert!(got.nodes.is_some());
    }

    #[test]
    fn a_cluster_snapshot_keeps_its_nodes() {
        let dir = temp_path("cluster-snap").parent().unwrap().to_path_buf();
        let report = crate::tui::fixtures::cluster_report_degraded();
        let path = snapshot::save(&dir, &report).unwrap();
        let back = snapshot::load(&path).unwrap();
        assert_eq!(back, report);
        let nodes = back.nodes.unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[2].status, crate::model::NodeStatus::Failed);
        assert!(back.hot_keys.iter().all(|k| k.node.is_some()));
        // The diff and trend of the TUI load it like any snapshot.
        assert_eq!(load_trend(&dir, &report).unwrap().points.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrub_removes_known_secret() {
        assert_eq!(
            scrub("bad password hunter2".into(), Some("hunter2")),
            "bad password ******"
        );
        assert_eq!(scrub("x".into(), Some("")), "x");
        assert_eq!(scrub("x".into(), None), "x");
    }

    /// A terminal output that fails every write.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "broken pipe",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Records writes and flushes.
    #[derive(Default)]
    struct Recorder {
        bytes: Vec<u8>,
        flushed: usize,
    }

    impl Write for Recorder {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushed = self.bytes.len();
            Ok(())
        }
    }

    fn runner() -> (Runner, mpsc::UnboundedReceiver<Incoming>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Runner::new(tx, temp_path("unused")), rx)
    }

    fn ping() -> live::LiveRequest {
        live::LiveRequest::Server(live::server::Request::Ping)
    }

    /// The message in `incoming`, which must be one.
    fn msg_of(incoming: Incoming) -> Msg {
        match incoming {
            Incoming::Msg(m) => *m,
            _ => panic!("not a message"),
        }
    }

    /// The next message, which must be `Msg::Live` answered with `Closed`.
    fn expect_closed(rx: &mut mpsc::UnboundedReceiver<Incoming>) -> (ConnId, ScanId, u64) {
        match rx.try_recv().map(msg_of) {
            Ok(Msg::Live {
                id,
                scan,
                req_id,
                result: Err(LiveError::Closed),
            }) => (id, scan, req_id.0),
            _ => panic!("expected Msg::Live with Err(Closed)"),
        }
    }

    /// Makes `(id, scan)` the current session, as `Effect::Connect` does, without
    /// starting a connect task.
    fn connecting(runner: &mut Runner, id: ConnId, scan: ScanId) {
        runner.scans.insert(id, scan);
        runner.replace_live(Some(LiveSession::new(id, scan)));
    }

    #[test]
    fn live_requests_without_a_live_task_are_answered_closed() {
        let (mut runner, mut rx) = runner();
        let mut out = Recorder::default();
        // No session at all.
        let effect = Effect::Live {
            id: 5,
            req_id: live::ReqId(1),
            to: live::Dest::default(),
            req: ping(),
        };
        assert!(!runner.run(effect, &mut out));
        assert_eq!(expect_closed(&mut rx), (5, 0, 1));
        // Still connecting: no handle yet.
        connecting(&mut runner, 5, 50);
        runner.run(
            Effect::Live {
                id: 5,
                req_id: live::ReqId(2),
                to: live::Dest::default(),
                req: ping(),
            },
            &mut out,
        );
        assert_eq!(expect_closed(&mut rx), (5, 50, 2));
        // Another entry than the current one.
        connecting(&mut runner, 6, 60);
        runner.run(
            Effect::Live {
                id: 5,
                req_id: live::ReqId(3),
                to: live::Dest::default(),
                req: ping(),
            },
            &mut out,
        );
        assert_eq!(expect_closed(&mut rx), (5, 50, 3));
        assert!(out.bytes.is_empty());
    }

    #[test]
    fn a_live_task_is_installed_only_for_the_current_session() {
        let (mut runner, mut rx) = runner();
        connecting(&mut runner, 1, 10);
        // A replaced run's connect task finishing late installs nothing.
        let (stale, _stale_rx) = LiveHandle::detached();
        assert!(!install_live(&runner.live, 1, 9, &runner.tx, || stale));
        assert!(rx.try_recv().is_err());

        let (handle, mut queue) = LiveHandle::detached();
        assert!(install_live(&runner.live, 1, 10, &runner.tx, || handle));
        let Ok(Msg::LiveReady { id: 1, scan: 10 }) = rx.try_recv().map(msg_of) else {
            panic!("no LiveReady");
        };
        // Only once per session.
        let (again, _again_rx) = LiveHandle::detached();
        assert!(!install_live(&runner.live, 1, 10, &runner.tx, || again));

        // Requests for the session reach its task.
        runner.run(
            Effect::Live {
                id: 1,
                req_id: live::ReqId(7),
                to: live::Dest::default(),
                req: ping(),
            },
            &mut Recorder::default(),
        );
        let (req_id, _, req) = queue.try_recv().expect("queued");
        assert_eq!((req_id, req), (live::ReqId(7), ping()));
        assert!(rx.try_recv().is_err());

        // A new connect drops the old live task.
        connecting(&mut runner, 1, 11);
        assert!(queue.is_closed());
        runner.run(
            Effect::Live {
                id: 1,
                req_id: live::ReqId(8),
                to: live::Dest::default(),
                req: ping(),
            },
            &mut Recorder::default(),
        );
        assert_eq!(expect_closed(&mut rx), (1, 11, 8));
    }

    #[test]
    fn quitting_drops_the_live_task() {
        let (mut runner, _rx) = runner();
        connecting(&mut runner, 2, 20);
        let (handle, queue) = LiveHandle::detached();
        assert!(install_live(&runner.live, 2, 20, &runner.tx, || handle));
        assert!(!queue.is_closed());
        assert!(runner.run(Effect::Quit, &mut Recorder::default()));
        assert!(queue.is_closed());
        assert!(lock(&runner.live).is_none());
    }

    fn tail_msg(i: usize) -> TailMessage {
        TailMessage::new(i as i64, "news".into(), None, format!("m{i}").into_bytes())
    }

    fn message_push(i: usize) -> redis::PushInfo {
        redis::PushInfo {
            kind: redis::PushKind::Message,
            data: vec![
                redis::Value::BulkString(b"news".to_vec()),
                redis::Value::BulkString(format!("m{i}").into_bytes()),
            ],
        }
    }

    #[test]
    fn a_tail_batch_keeps_the_newest_2000_and_counts_the_rest() {
        let mut b = TailBatch::default();
        assert!(b.take().is_none());
        for i in 0..TAIL_KEPT + 5 {
            b.push(tail_msg(i));
        }
        let (msgs, dropped) = b.take().expect("a batch");
        assert_eq!((msgs.len(), dropped), (TAIL_KEPT, 5));
        assert_eq!(msgs[0].at_unix_ms, 5);
        assert_eq!(msgs.last().unwrap().at_unix_ms, (TAIL_KEPT + 4) as i64);
        // The next batch starts empty.
        assert!(b.take().is_none());
    }

    #[tokio::test]
    async fn the_tail_forwards_in_order_and_passes_on_the_rest_when_lost() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for i in 0..3 {
            tx.send(message_push(i)).unwrap();
        }
        // A subscribe confirmation is not a message.
        tx.send(redis::PushInfo {
            kind: redis::PushKind::Subscribe,
            data: vec![
                redis::Value::BulkString(b"news".to_vec()),
                redis::Value::Int(1),
            ],
        })
        .unwrap();
        tx.send(redis::PushInfo {
            kind: redis::PushKind::Disconnection,
            data: vec![],
        })
        .unwrap();
        let stamp = std::sync::atomic::AtomicI64::new(100);
        let mut got = Vec::new();
        let ok = forward_tail(
            &mut rx,
            || stamp.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            |b, d| {
                assert_eq!(d, 0);
                got.extend(b);
                true
            },
        )
        .await;
        assert!(ok);
        let payloads: Vec<_> = got.iter().map(|m| m.payload.as_bytes().to_vec()).collect();
        assert_eq!(payloads, [b"m0".to_vec(), b"m1".to_vec(), b"m2".to_vec()]);
        // Stamped when each arrived.
        let at: Vec<_> = got.iter().map(|m| m.at_unix_ms).collect();
        assert_eq!(at, [100, 101, 102]);

        // A closed receiver is a lost connection too.
        let (tx, mut rx) = mpsc::unbounded_channel::<redis::PushInfo>();
        drop(tx);
        assert!(forward_tail(&mut rx, || 0, |_, _| true).await);
    }

    #[tokio::test(start_paused = true)]
    async fn the_tail_sends_at_most_one_batch_every_100_ms() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let feed = tokio::spawn(async move {
            // 1,000 messages over one second, then the connection goes.
            for i in 0..1_000 {
                tx.send(message_push(i)).unwrap();
                if i % 10 == 9 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            tx.send(redis::PushInfo {
                kind: redis::PushKind::Disconnection,
                data: vec![],
            })
            .unwrap();
        });
        let start = tokio::time::Instant::now();
        let mut sent_at = Vec::new();
        let mut total = 0;
        forward_tail(
            &mut rx,
            || 0,
            |b, _| {
                sent_at.push(tokio::time::Instant::now());
                total += b.len();
                true
            },
        )
        .await;
        feed.await.unwrap();
        assert_eq!(total, 1_000);
        // One second of messages, in about ten batches: never two within 100 ms, except
        // the last one, passed on when the connection went.
        assert!(sent_at.len() <= 12, "{} batches", sent_at.len());
        for w in sent_at[..sent_at.len() - 1].windows(2) {
            assert!(
                w[1] - w[0] >= TAIL_BATCH_EVERY,
                "batches {:?} apart",
                w[1] - w[0]
            );
        }
        assert!(sent_at.last().unwrap().duration_since(start) >= Duration::from_millis(990));
    }

    #[tokio::test]
    async fn the_tail_stops_when_the_app_is_gone() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(message_push(0)).unwrap();
        let ended = tokio::time::timeout(
            Duration::from_secs(5),
            forward_tail(&mut rx, || 0, |_, _| false),
        )
        .await
        .expect("kept running");
        assert!(!ended);
        drop(tx);
    }

    #[test]
    fn a_subscribe_without_a_connected_session_is_answered_closed() {
        let (mut runner, mut rx) = runner();
        let subscribe = |id, scan| Effect::Subscribe {
            id,
            scan,
            tail: 3,
            profile: profile("p"),
            secrets: SecretSource::Keychain,
            to: Subscription::Channel("news".into()),
            node: 0,
        };
        // No session; then a session still connecting (no budget yet).
        runner.run(subscribe(1, 10), &mut Recorder::default());
        connecting(&mut runner, 1, 10);
        runner.run(subscribe(1, 10), &mut Recorder::default());
        // A replaced run.
        runner.run(subscribe(1, 9), &mut Recorder::default());
        for _ in 0..3 {
            match rx.try_recv().map(msg_of) {
                Ok(Msg::TailClosed {
                    id: 1,
                    tail: 3,
                    error: Some(e),
                    ..
                }) => assert_eq!(e, LiveError::Closed.to_string()),
                _ => panic!("expected TailClosed"),
            }
        }
        assert!(lock(&runner.live).as_ref().unwrap().tail.is_none());
    }

    #[tokio::test]
    async fn one_tail_per_session_and_unsubscribe_or_a_new_connect_drops_it() {
        let (mut runner, _rx) = runner();
        connecting(&mut runner, 1, 10);
        lock(&runner.live).as_mut().unwrap().nodes = vec![NodeRoute {
            budget: Budget::new(None),
            addr: None,
        }];
        // A tail whose task never ends, standing in for one that is subscribed.
        let install = |runner: &Runner, tail: u64| {
            let (keep_tx, keep_rx) = tokio::sync::oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                let _keep = keep_tx;
                std::future::pending::<()>().await;
            });
            lock(&runner.live).as_mut().unwrap().tail = Some(TailTask {
                tail,
                _task: AbortOnDrop(task),
            });
            keep_rx
        };
        let first = install(&runner, 1);
        // Unsubscribing another tail leaves it.
        runner.run(
            Effect::Unsubscribe { id: 1, tail: 2 },
            &mut Recorder::default(),
        );
        assert!(lock(&runner.live).as_ref().unwrap().tail.is_some());
        runner.run(
            Effect::Unsubscribe { id: 1, tail: 1 },
            &mut Recorder::default(),
        );
        assert!(lock(&runner.live).as_ref().unwrap().tail.is_none());
        // The aborted task dropped what it held.
        assert!(first.await.is_err());
        // A new connect drops the session's tail with it.
        let second = install(&runner, 2);
        connecting(&mut runner, 1, 11);
        assert!(second.await.is_err());
    }

    #[test]
    fn tail_open_errors_are_fixed_or_cleaned() {
        let clean = |e: String| conn::parse::mask(&scrub(e, Some("hunter2")));
        let resp3 = conn::ConnError::Redis(redis::RedisError::from((
            redis::ErrorKind::RESP3NotSupported,
            "no HELLO",
        )));
        assert_eq!(tail_open_error(&resp3, clean), live::pubsub::RESP3_REFUSED);
        let other = conn::ConnError::Redis(redis::RedisError::from(std::io::Error::other(
            "refused for redis://u:hunter2@h:6379",
        )));
        let text = tail_open_error(&other, clean);
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn copy_effect_writes_osc52_to_the_terminal_output_and_flushes() {
        let (mut runner, mut rx) = runner();
        let mut out = Recorder::default();
        let text = CopyText::suggested_command("user:{id}:cart").unwrap();
        assert!(!runner.run(Effect::Copy(text.clone()), &mut out));
        assert_eq!(out.bytes, clipboard::osc52(&text).unwrap().as_bytes());
        // Flushed after the whole sequence, so it never mixes with the next frame.
        assert_eq!(out.flushed, out.bytes.len());
        // Nothing to report on success.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_copy_that_cannot_be_written_becomes_an_error_message() {
        let (mut runner, mut rx) = runner();
        assert!(!runner.run(Effect::Copy(CopyText::key("user:1")), &mut Broken));
        let Ok(Msg::CopyFailed(e)) = rx.try_recv().map(msg_of) else {
            panic!("no CopyFailed");
        };
        assert_eq!(e, "broken pipe");
        // Over the cap: nothing is written, and the app hears why.
        let mut out = Recorder::default();
        let big = CopyText::key(&"k".repeat(clipboard::MAX_OSC52_BYTES));
        runner.run(Effect::Copy(big), &mut out);
        assert!(out.bytes.is_empty());
        let Ok(Msg::CopyFailed(e)) = rx.try_recv().map(msg_of) else {
            panic!("no CopyFailed");
        };
        assert_eq!(e, "the key is too long to copy");
    }

    #[test]
    fn other_effects_write_nothing_to_the_terminal() {
        let (mut runner, _rx) = runner();
        let mut out = Recorder::default();
        assert!(runner.run(Effect::Quit, &mut out));
        assert!(out.bytes.is_empty());
    }
}
