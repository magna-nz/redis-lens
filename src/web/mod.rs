//! The web dashboard (spec "Web dashboard"): a page served on 127.0.0.1 by the same
//! process, an alternative to the TUI with the same features. Owner: dashboard.
//!
//! - [`start`] binds `127.0.0.1:<port>` (never another address), makes this run's
//!   session token and spawns the server; [`Dashboard::open_url`] is the URL that signs a
//!   browser in, [`Dashboard::url`] the one to show.
//! - [`auth`]: the token and cookie, the `Host`, `Origin` and header checks, and the
//!   headers every response carries.
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
pub mod session;

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

    /// The URL that signs a browser in: it holds the token. Open it, never log it.
    pub fn open_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/auth/{}",
            self.port,
            self.token.expose()
        )
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
    }
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
    let hub = session::Hub::new(opts.config, store);
    let app = app_state(Arc::clone(&hub), port, token.clone(), shutdown_rx.clone());
    let router = router(app);
    tokio::spawn(api::overview::poll(Arc::clone(&hub), shutdown_rx.clone()));
    let mut stop = shutdown_rx;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = stop.wait_for(|v| *v).await;
            })
            .await;
        hub.close();
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
    api::App {
        hub,
        confirms: Arc::new(Mutex::new(confirm::Pending::default())),
        page: assets::Page {
            assets: assets::Assets::from_env(),
            guard: auth::Guard { port, token },
        },
        shutdown,
    }
}

/// Every route, behind the guard, every response with the security headers.
pub(crate) fn router(app: api::App) -> Router {
    use api::{cmdline, connections, overview, pubsub, server, writes};
    let guard = app.page.guard.clone();
    Router::new()
        .route("/", get(assets::index))
        .route("/assets/{*path}", get(assets::asset))
        .route("/auth/{token}", get(auth::sign_in))
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
        .route("/api/node", post(connections::pick))
        .route("/api/overview", get(overview::get))
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

/// Opens `url` in the default browser: `open` on macOS, `xdg-open` on Linux and the
/// BSDs, `rundll32 url.dll,FileProtocolHandler` on Windows. Never through a shell; the
/// program is not waited for here.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reaped on a thread of its own, so it never lingers as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests;
