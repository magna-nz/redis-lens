//! The web dashboard (spec "Web dashboard"): a page served on 127.0.0.1 by the same
//! process, an alternative to the TUI with the same features. Owner: dashboard.
//!
//! - [`start`] binds `127.0.0.1:<port>` (never another address), makes this run's
//!   session token and spawns the server; [`Dashboard::open_url`] is the URL that signs a
//!   browser in (the token in its fragment), [`Dashboard::url`] the one to show.
//!   [`open_in_browser`] opens it without putting it on a command line.
//! - [`auth`]: the token (`Authorization: Bearer`), the `Host`, `Origin` and header
//!   checks, and the headers every response carries.
//! - [`assets`]: the page's files, embedded from a fixed list.
//! - [`session`]: the dashboard's one connection, on the TUI's own runtime.
//! - [`api`]: the JSON routes and the event stream (`docs/dashboard-api.md`).
//!
//! The server logs nothing: no request, body or reply.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::routing::{get, post};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::secrets::{KeyringStore, SecretStore};

pub mod api;
pub mod assets;
pub mod auth;
pub mod confirm;
pub mod dto;
pub mod keyname;
pub mod session;
pub mod wording;

/// The port the dashboard listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 7379;

/// How to start the dashboard.
#[derive(Debug, Clone)]
pub struct Options {
    /// The port on 127.0.0.1. 0 picks a free one.
    pub port: u16,
    /// The config file the profiles are in.
    pub config: PathBuf,
}

/// Why the dashboard did not start. Never stops the TUI: it says so and goes on.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("port {0} on 127.0.0.1 is in use, so the dashboard is off (pick another with --port)")]
    PortInUse(u16),
    #[error("the dashboard could not listen on 127.0.0.1:{port}: {source}")]
    Bind {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("the dashboard could not make a session token: the OS gave no random bytes")]
    Random,
}

/// The running dashboard. Dropping it shuts the server down.
pub struct Dashboard {
    port: u16,
    token: auth::Token,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

/// Never the token.
impl fmt::Debug for Dashboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dashboard")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Dashboard {
    /// `http://127.0.0.1:<port>/`, without the token: for display.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// The URL that signs a browser in, `http://127.0.0.1:<port>/#token=<hex>`: the token
    /// is in the fragment, which the browser never sends to a server. Open it with
    /// [`open_in_browser`], never log it.
    pub fn open_url(&self) -> String {
        auth::Guard {
            port: self.port,
            token: self.token.clone(),
        }
        .sign_in_url()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Asks the server to stop: open event streams end, then it stops.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Until the server has stopped ([`Self::shutdown`], or it failed).
    pub async fn wait(mut self) {
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        opener::remove_all();
    }
}

/// How the server treats connections: hyper's header read timeout and a cap on the
/// connections open at once (a browser opens a few per tab, the event stream one).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub header_timeout: std::time::Duration,
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_timeout: std::time::Duration::from_secs(10),
            max_connections: 64,
        }
    }
}

/// Until `stop` turns true (the guard `wait_for` gives is dropped here, not held across
/// the caller's awaits).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|v| *v).await;
}

/// Accepts connections until `stop` turns true, each served by hyper's HTTP/1 with the
/// header read timeout, at most `limits.max_connections` at once (the next waits for one
/// to close). Then the open ones are given 2 s to finish.
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    mut stop: watch::Receiver<bool>,
    limits: Limits,
) {
    use hyper_util::rt::{TokioIo, TokioTimer};
    let open = Arc::new(tokio::sync::Semaphore::new(limits.max_connections));
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    loop {
        let permit = tokio::select! {
            p = Arc::clone(&open).acquire_owned() => p,
            () = stopped(&mut stop) => break,
        };
        let Ok(permit) = permit else { break };
        let stream = tokio::select! {
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                // Out of file descriptors and the like: wait, then go on.
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            },
            () = stopped(&mut stop) => break,
        };
        let service = hyper_util::service::TowerToHyperService::new(router.clone());
        let mut builder = hyper::server::conn::http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_timeout);
        let conn = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            let _ = conn.await;
            drop(permit);
        });
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), graceful.shutdown()).await;
}

/// Starts the dashboard: binds `127.0.0.1:<port>` and spawns the server, reading saved
/// profiles' credentials from the OS keychain. Call it inside a tokio runtime.
pub fn start(opts: Options) -> Result<Dashboard, StartError> {
    start_with(opts, Arc::new(KeyringStore))
}

/// [`start`] with the credential store given, for the integration tests (a
/// `MemoryStore`, never the real keychain).
#[doc(hidden)]
pub fn start_with(opts: Options, store: Arc<dyn SecretStore>) -> Result<Dashboard, StartError> {
    start_with_snapshots(opts, store, None)
}

/// [`start_with`] saving and reading snapshots in `snapshots` instead of the TUI's
/// directory, for the integration tests (they never touch the user's snapshots).
#[doc(hidden)]
pub fn start_with_snapshots(
    opts: Options,
    store: Arc<dyn SecretStore>,
    snapshots: Option<PathBuf>,
) -> Result<Dashboard, StartError> {
    let std_listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, opts.port))
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrInUse => StartError::PortInUse(opts.port),
            _ => StartError::Bind {
                port: opts.port,
                source: e,
            },
        })?;
    let bind_err = |e| StartError::Bind {
        port: opts.port,
        source: e,
    };
    let port = std_listener.local_addr().map_err(bind_err)?.port();
    std_listener.set_nonblocking(true).map_err(bind_err)?;
    let listener = tokio::net::TcpListener::from_std(std_listener).map_err(bind_err)?;
    let token = auth::Token::random().map_err(|_| StartError::Random)?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let hub = session::Hub::with_snapshots(opts.config, store, snapshots);
    let app = app_state(Arc::clone(&hub), port, token.clone(), shutdown_rx.clone());
    let router = router(app);
    tokio::spawn(api::overview::poll(Arc::clone(&hub), shutdown_rx.clone()));
    let task = tokio::spawn(async move {
        serve(listener, router, shutdown_rx, Limits::default()).await;
        hub.close();
        opener::remove_all();
    });
    Ok(Dashboard {
        port,
        token,
        shutdown,
        task: Some(task),
    })
}

/// The state every route shares.
pub(crate) fn app_state(
    hub: Arc<session::Hub>,
    port: u16,
    token: auth::Token,
    shutdown: watch::Receiver<bool>,
) -> api::App {
    let confirms = Arc::new(Mutex::new(confirm::Pending::default()));
    tokio::spawn(confirm::prune_on_timer(
        Arc::downgrade(&confirms),
        confirm::PRUNE_EVERY,
        shutdown.clone(),
    ));
    api::App {
        hub,
        confirms,
        page: assets::Page {
            assets: assets::Assets::from_env(),
            guard: auth::Guard { port, token },
        },
        shutdown,
    }
}

/// Every route, behind the guard, every response with the security headers.
pub(crate) fn router(app: api::App) -> Router {
    use api::{
        analyze, browse, cmdline, connections, keys, overview, pubsub, server, snapshots, values,
        writes,
    };
    let guard = app.page.guard.clone();
    Router::new()
        .route("/", get(assets::index))
        .route("/assets/{*path}", get(assets::asset))
        .route("/api/state", get(api::state))
        .route("/api/events", get(api::events))
        .route(
            "/api/connections",
            get(connections::list).post(connections::add),
        )
        .route(
            "/api/connections/{name}",
            axum::routing::patch(connections::set).delete(connections::remove),
        )
        .route("/api/connections/{name}/test", post(connections::test))
        .route("/api/docker", get(connections::docker))
        .route("/api/connect", post(connections::connect))
        .route("/api/disconnect", post(connections::disconnect))
        .route("/api/session/writes", post(connections::session_writes))
        .route("/api/node", post(connections::pick))
        .route("/api/overview", get(overview::get))
        .route("/api/polling", post(overview::set_polling))
        .route("/api/server/clients", get(server::clients))
        .route("/api/server/slowlog", get(server::slowlog))
        .route(
            "/api/server/monitor",
            post(server::monitor_start).delete(server::monitor_stop),
        )
        .route("/api/pubsub/channels", get(pubsub::channels))
        .route(
            "/api/pubsub/subscribe",
            post(pubsub::subscribe).delete(pubsub::unsubscribe),
        )
        .route("/api/pubsub/publish", post(pubsub::publish))
        .route("/api/browse", get(browse::tree))
        .route("/api/browse/more", post(browse::more))
        .route("/api/dbs", get(browse::dbs))
        .route("/api/db", post(browse::switch))
        .route("/api/keys", get(keys::open))
        .route("/api/keys/more", get(keys::more))
        .route("/api/find", get(keys::find))
        .route("/api/value", get(values::open))
        .route("/api/value/more", get(values::more))
        .route("/api/value/item", get(values::item_route))
        .route(
            "/api/analysis",
            get(analyze::get).post(analyze::start).delete(analyze::stop),
        )
        .route("/api/analysis/big_keys", get(analyze::big_keys))
        .route("/api/analysis/expiry", get(analyze::expiry))
        .route("/api/analysis/changes", get(analyze::changes))
        .route("/api/analysis/hot_keys", get(analyze::hot_keys))
        .route("/api/analysis/nodes", get(analyze::nodes))
        .route("/api/analysis/slots", get(analyze::slots))
        .route("/api/snapshots", get(snapshots::list).post(snapshots::save))
        .route("/api/snapshots/compare", get(snapshots::compare))
        .route("/api/cmdline", post(cmdline::run))
        .route("/api/cmdline/commands", get(cmdline::commands))
        .route("/api/cmdline/help", get(cmdline::help))
        .route(
            "/api/confirm/{id}",
            post(writes::answer).delete(writes::cancel),
        )
        .fallback(api::not_found)
        .with_state(app)
        .layer(axum::middleware::from_fn_with_state(guard, auth::guard))
        .layer(axum::middleware::from_fn(auth::headers))
}

/// Opens `url` (the sign-in URL) in the default browser without putting it on any
/// command line, where `ps` would show it to every local user: it is written into a
/// small HTML file that sends the browser on (`opener`), in a directory only this user
/// can read, and that file is what `open` (macOS), `xdg-open` (Linux and the BSDs) or
/// `rundll32 url.dll,FileProtocolHandler` (Windows) gets. Never through a shell. The
/// file is deleted after [`opener::KEEP`] and when the dashboard stops.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    let path = opener::write(url)?;
    let mut child = match opener::command(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            opener::remove(&path);
            return Err(e);
        }
    };
    // Reaped on a thread of its own, so it never lingers as a zombie; the file goes
    // once the browser has had time to read it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    opener::remove_after(path, opener::KEEP);
    Ok(())
}

pub mod opener;

#[cfg(test)]
mod tests;
