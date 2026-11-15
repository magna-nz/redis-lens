//! Connections: list, add, set, remove, test, Docker discovery, connect, disconnect,
//! pick a node. Adding and changing go through `redis-lens conn add` and `conn set`
//! themselves (`crate::cli::conn`), with their questions put to the browser: a token
//! command to save, writes to turn on. Passwords go in once, in the JSON body of an add,
//! straight to the [`crate::secrets::SecretStore`]; they are never echoed, and are
//! scrubbed from any error text as a second line of defence.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer};

use super::writes::Action;
use super::{App, Body, Question, error, ok};
use crate::cli::conn as cli;
use crate::cli::{ConnAddArgs, EnvArg, ProviderArg, WritesArg};
use crate::config::Config;
use crate::tui::app::ConnId;
use crate::web::dto;
use crate::web::session::{ConnectError, Target, prefetch_async};

/// A password from a request body: a [`SecretString`] as soon as it is read.
pub struct Secret(pub SecretString);

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(|s| Secret(SecretString::from(s)))
    }
}

impl Clone for Secret {
    fn clone(&self) -> Self {
        Secret(SecretString::from(self.0.expose_secret().to_string()))
    }
}

/// `POST /api/connections`: the options of `redis-lens conn add`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRequest {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub env: Option<EnvName>,
    /// The server's password: to the keychain only.
    #[serde(default)]
    pub password: Option<Secret>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// `true`: writes on (`--writes on`), `false`: off; absent: as the env says.
    #[serde(default)]
    pub writes: Option<bool>,
    #[serde(default)]
    pub provider: Option<ProviderName>,
    /// `[USER@]HOST[:PORT]` of an SSH bastion.
    #[serde(default)]
    pub ssh: Option<String>,
    #[serde(default)]
    pub ssh_identity: Option<std::path::PathBuf>,
    #[serde(default)]
    pub ssh_known_hosts: Option<std::path::PathBuf>,
    /// `HOST:PORT[,…]` of the Sentinels.
    #[serde(default)]
    pub sentinel: Option<String>,
    #[serde(default)]
    pub sentinel_master: Option<String>,
    #[serde(default)]
    pub sentinel_tls: bool,
    #[serde(default)]
    pub sentinel_user: Option<String>,
    /// The Sentinels' own password: to the keychain only.
    #[serde(default)]
    pub sentinel_password: Option<Secret>,
    /// The token command, program first: it prints the password at every connect.
    #[serde(default)]
    pub token_command: Vec<String>,
    /// Sign in with Microsoft Entra ID (a build with the `entra-id` feature).
    #[serde(default)]
    pub entra: bool,
    #[serde(default)]
    pub standalone: bool,
    #[serde(default)]
    pub prefer_replica: bool,
    /// Replace a profile of the same name.
    #[serde(default)]
    pub force: bool,
}

/// Holds passwords and a URL that may carry one: `Debug` shows the name only.
impl std::fmt::Debug for AddRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddRequest")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvName {
    Prod,
    Stage,
    Local,
    Other,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderName {
    Elasticache,
    ElasticacheServerless,
    Memorydb,
    Azure,
    Memorystore,
}

/// An add waiting for its question's answer.
pub struct AddAsked {
    req: AddRequest,
    /// The token command was confirmed.
    token_ok: bool,
    /// The profile name typed to turn writes on for a prod profile.
    writes_name: Option<String>,
}

/// What `conn add` asked, if it asked.
#[derive(Debug, Default)]
struct Asked {
    token: Option<String>,
    writes: Option<String>,
}

/// Why an add needs its question answered first.
const NEEDS_ANSWER: &str = "needs the question answered";

fn args_of(r: &AddRequest, token_ok: bool, writes_name: Option<String>) -> ConnAddArgs {
    ConnAddArgs {
        name: r.name.clone(),
        url: r.url.clone(),
        env: r.env.map(|e| match e {
            EnvName::Prod => EnvArg::Prod,
            EnvName::Stage => EnvArg::Stage,
            EnvName::Local => EnvArg::Local,
            EnvName::Other => EnvArg::Other,
        }),
        password_stdin: r.password.is_some(),
        ask_password: false,
        entra: r.entra,
        force: r.force,
        prefer_replica: r.prefer_replica,
        standalone: r.standalone,
        provider: r.provider.map(|p| match p {
            ProviderName::Elasticache => ProviderArg::Elasticache,
            ProviderName::ElasticacheServerless => ProviderArg::ElasticacheServerless,
            ProviderName::Memorydb => ProviderArg::Memorydb,
            ProviderName::Azure => ProviderArg::Azure,
            ProviderName::Memorystore => ProviderArg::Memorystore,
        }),
        token_command: r.token_command.clone(),
        // A token command is confirmed by the question below, never skipped.
        yes: token_ok,
        writes: r
            .writes
            .map(|on| if on { WritesArg::On } else { WritesArg::Off }),
        confirm: writes_name,
        tags: r.tags.clone(),
        ssh: r.ssh.clone(),
        ssh_identity: r.ssh_identity.clone(),
        ssh_known_hosts: r.ssh_known_hosts.clone(),
        sentinel: r.sentinel.clone(),
        sentinel_master: r.sentinel_master.clone(),
        sentinel_tls: r.sentinel_tls,
        sentinel_user: r.sentinel_user.clone(),
        sentinel_password_stdin: false,
        sentinel_ask_password: r.sentinel_password.is_some(),
    }
}

/// Error or message text with everything the request held that is a credential taken
/// out (the passwords, and the URL's user and password), then masked.
fn clean(text: &str, r: &AddRequest) -> String {
    let mut t = text.to_string();
    for p in [&r.password, &r.sentinel_password].into_iter().flatten() {
        t = crate::tui::scrub(t, Some(p.0.expose_secret()));
    }
    if let Ok(parsed) = crate::conn::parse::parse_connection_string(&r.url)
        && let Some(c) = parsed.credentials
    {
        t = crate::tui::scrub(t, Some(c.password.expose_secret()));
        if let Some(u) = c.username.as_deref().filter(|u| u.len() > 1) {
            t = crate::tui::scrub(t, Some(u));
        }
    }
    if let Some(u) = r.sentinel_user.as_deref().filter(|u| u.len() > 1) {
        t = crate::tui::scrub(t, Some(u));
    }
    crate::value::escape(&crate::conn::parse::mask(&t))
}

/// Runs `conn add` on a blocking thread (it writes the keychain and the config file).
async fn run_add(
    app: &App,
    req: AddRequest,
    token_ok: bool,
    writes_name: Option<String>,
) -> Result<String, (String, Asked)> {
    if req.entra && !cfg!(feature = "entra-id") {
        return Err((
            "this build has no Entra ID sign-in (it needs the entra-id feature)".into(),
            Asked::default(),
        ));
    }
    let path = app.hub.config.clone();
    let store = Arc::clone(&app.hub.store);
    let handle = tokio::runtime::Handle::current();
    let joined = tokio::task::spawn_blocking(move || {
        let asked = Mutex::new(Asked::default());
        let args = args_of(&req, token_ok, writes_name);
        let stdin = req
            .password
            .as_ref()
            .map(|p| p.0.expose_secret().to_string());
        let sentinel_pw = req
            .sentinel_password
            .as_ref()
            .map(|p| p.0.expose_secret().to_string());
        let prompt = || -> anyhow::Result<String> {
            sentinel_pw
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no Sentinel password was given"))
        };
        let confirm = |shown: &str| -> anyhow::Result<bool> {
            asked.lock().unwrap_or_else(|e| e.into_inner()).token = Some(shown.to_string());
            Ok(false)
        };
        let ask = |question: &str, _hint: &str| -> anyhow::Result<String> {
            asked.lock().unwrap_or_else(|e| e.into_inner()).writes = Some(question.to_string());
            anyhow::bail!(NEEDS_ANSWER)
        };
        let mut warn = Vec::new();
        let r = handle.block_on(cli::add_asking(
            &path,
            store.as_ref(),
            &args,
            stdin,
            &mut warn,
            &prompt,
            &confirm,
            &ask,
        ));
        let asked = asked.into_inner().unwrap_or_else(|e| e.into_inner());
        r.map_err(|e| (clean(&format!("{e:#}"), &req), asked))
            .map(|m| clean(&m, &req))
    })
    .await;
    match joined {
        Ok(r) => r,
        Err(_) => Err(("the save failed".into(), Asked::default())),
    }
}

/// The saved profile `name`, for the reply.
async fn saved(app: &App, name: &str) -> Option<dto::Connection> {
    let path = app.hub.config.clone();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        Config::load(&path)
            .ok()?
            .get(&name)
            .map(dto::Connection::of)
    })
    .await
    .ok()
    .flatten()
}

async fn add_step(app: &App, asked: AddAsked) -> Response {
    let name = asked.req.name.clone();
    let req = asked.req.clone();
    match run_add(app, asked.req, asked.token_ok, asked.writes_name.clone()).await {
        Ok(message) => {
            let connection = saved(app, &name).await;
            (
                StatusCode::CREATED,
                axum::Json(serde_json::json!({
                    "message": crate::web::wording::web(&message),
                    "connection": connection,
                })),
            )
                .into_response()
        }
        Err((
            _,
            Asked {
                token: Some(shown), ..
            },
        )) if !asked.token_ok => {
            let text = format!(
                "This profile will run this command at every connect and send what it prints \
                 as the password: {}. It is run directly, not through a shell. Save it?",
                crate::value::escape(&shown)
            );
            app.ask(
                Action::Add(Box::new(AddAsked {
                    req,
                    token_ok: true,
                    writes_name: asked.writes_name,
                })),
                Question::ask(text),
            )
        }
        Err((
            _,
            Asked {
                writes: Some(q), ..
            },
        )) => {
            let expect = req.name.clone();
            app.ask(
                Action::Add(Box::new(AddAsked {
                    req,
                    token_ok: asked.token_ok,
                    writes_name: Some(String::new()),
                })),
                Question::typed(crate::value::escape(&crate::web::wording::web(&q)), expect),
            )
        }
        Err((why, _)) => error(StatusCode::BAD_REQUEST, &why),
    }
}

/// `POST /api/connections`: saves a connection as `conn add` does. A token command, or
/// writes on for a prod profile, asks first (409).
pub async fn add(State(app): State<App>, Body(req): Body<AddRequest>) -> Response {
    add_step(
        &app,
        AddAsked {
            req,
            token_ok: false,
            writes_name: None,
        },
    )
    .await
}

/// The answer to an add's question: the token command is confirmed, or the profile
/// name typed (checked by `conn add` itself).
pub async fn add_answered(app: &App, mut asked: AddAsked, typed: Option<String>) -> Response {
    if asked.writes_name.is_some() {
        asked.writes_name = Some(typed.unwrap_or_default());
    }
    add_step(app, asked).await
}

/// `PATCH /api/connections/<name>`: the options of `conn set`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRequest {
    #[serde(default)]
    pub writes: Option<bool>,
    #[serde(default)]
    pub tag: Vec<String>,
    #[serde(default)]
    pub untag: Vec<String>,
    #[serde(default)]
    pub standalone: Option<bool>,
}

/// A set waiting for its question's answer.
pub struct SetAsked {
    name: String,
    req: SetRequest,
    /// The name must be typed (a prod profile); otherwise yes or no.
    typed: bool,
}

async fn set_step(
    app: &App,
    name: String,
    req: SetRequest,
    yes: bool,
    confirm: Option<String>,
) -> Response {
    let path = app.hub.config.clone();
    let (n, r) = (name.clone(), req.clone());
    let joined = tokio::task::spawn_blocking(move || {
        let asked: Mutex<Option<String>> = Mutex::new(None);
        let ask = |question: &str, _hint: &str| -> anyhow::Result<String> {
            *asked.lock().unwrap_or_else(|e| e.into_inner()) = Some(question.to_string());
            anyhow::bail!(NEEDS_ANSWER)
        };
        let changes = cli::SetChanges {
            writes: r.writes,
            confirm: confirm.as_deref(),
            yes,
            tag: &r.tag,
            untag: &r.untag,
            standalone: r.standalone,
        };
        let result =
            cli::set(&path, &n, &changes, &ask).map_err(|e| (not_found(&e), format!("{e:#}")));
        let profile = Config::load(&path).ok().and_then(|c| c.get(&n).cloned());
        (
            result,
            asked.into_inner().unwrap_or_else(|e| e.into_inner()),
            profile,
        )
    })
    .await;
    let Ok((result, asked, profile)) = joined else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "the change failed");
    };
    match (result, asked) {
        (Ok(message), _) => {
            // The connected session follows the file: writes and tags at once.
            if let Some(p) = &profile {
                let changed = app.hub.with_mut(|c| {
                    if !c.docker && c.session.profile.name == p.name {
                        c.session.profile.read_only = p.read_only;
                        c.session.profile.tags = p.tags.clone();
                        true
                    } else {
                        false
                    }
                });
                if changed == Some(true) {
                    app.hub
                        .emit(crate::web::session::Event::new("state", app.hub.state()));
                }
            }
            ok(serde_json::json!({
                "message": crate::web::wording::web(&crate::value::escape(&crate::conn::parse::mask(&message))),
                "connection": profile.as_ref().map(dto::Connection::of),
            }))
        }
        (Err(_), Some(q)) => {
            let prod = profile
                .as_ref()
                .is_some_and(|p| p.env == crate::model::Env::Prod);
            let q = crate::value::escape(&crate::web::wording::web(&q));
            let question = if prod {
                Question::typed(q, name.clone())
            } else {
                Question::ask(q)
            };
            app.ask(
                Action::Set(SetAsked {
                    name,
                    req,
                    typed: prod,
                }),
                question,
            )
        }
        (Err((missing, why)), None) => {
            let status = if missing {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            error(
                status,
                &crate::value::escape(&crate::conn::parse::mask(&why)),
            )
        }
    }
}

/// `PATCH /api/connections/<name>`: changes a connection as `conn set` does. Turning
/// writes on asks first (409): yes or no, or on a prod profile its name typed.
pub async fn set(
    State(app): State<App>,
    Path(name): Path<String>,
    Body(req): Body<SetRequest>,
) -> Response {
    set_step(&app, name, req, false, None).await
}

/// The answer to a set's question.
pub async fn set_answered(app: &App, asked: SetAsked, typed: Option<String>) -> Response {
    if asked.typed {
        set_step(
            app,
            asked.name,
            asked.req,
            false,
            Some(typed.unwrap_or_default()),
        )
        .await
    } else {
        set_step(app, asked.name, asked.req, true, None).await
    }
}

/// Whether `e` says there is no profile by that name.
fn not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<crate::config::ConfigError>(),
            Some(crate::config::ConfigError::NotFound(_))
        )
    })
}

/// `DELETE /api/connections/<name>`: removes it, and its keychain entries. When the
/// session is on that profile (connected, connecting, or failed), it is closed and the
/// state cleared: a removed profile is not shown as the current one. An unknown name
/// is 404.
pub async fn remove(State(app): State<App>, Path(name): Path<String>) -> Response {
    let path = app.hub.config.clone();
    let store = Arc::clone(&app.hub.store);
    let n = name.clone();
    let joined = tokio::task::spawn_blocking(move || {
        cli::remove(&path, store.as_ref(), &n).map_err(|e| (not_found(&e), format!("{e:#}")))
    })
    .await;
    let forget = |app: &App| {
        let current = app
            .hub
            .with(|c| !c.docker && c.session.profile.name == name)
            .unwrap_or(false);
        if current {
            app.hub.disconnect();
            app.confirms().clear();
        }
    };
    match joined {
        Ok(Ok(message)) => {
            forget(&app);
            ok(
                serde_json::json!({ "message": crate::web::wording::web(&crate::value::escape(&message)) }),
            )
        }
        Ok(Err((true, _))) => error(StatusCode::NOT_FOUND, NO_SUCH),
        Ok(Err((false, why))) => {
            // The profile went, its keychain entry did not: the session goes all the same.
            forget(&app);
            error(
                StatusCode::BAD_REQUEST,
                &crate::value::escape(&crate::conn::parse::mask(&why)),
            )
        }
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "the change failed"),
    }
}

/// `POST /api/session/writes` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionWrites {
    pub on: bool,
}

/// `POST /api/session/writes` `{"on": bool}`: writes on or off for the connection the
/// session is on, as `w` on the TUI's Connections. A saved profile changes as
/// `PATCH /api/connections/<name>` changes it (saved, and turning writes on asks: its
/// name typed on `prod`). A Docker container has no saved profile, so it changes for
/// this session only: off at once, on after asking (yes or no, or its name typed on
/// `prod`), as the TUI does.
pub async fn session_writes(State(app): State<App>, Body(b): Body<SessionWrites>) -> Response {
    let Some((conn, docker, profile)) = app
        .hub
        .with(|c| (c.id, c.docker, c.session.profile.clone()))
    else {
        return error(StatusCode::CONFLICT, super::NOT_LIVE);
    };
    if !docker {
        let req = SetRequest {
            writes: Some(b.on),
            ..SetRequest::default()
        };
        return set_step(&app, profile.name, req, false, None).await;
    }
    if !b.on || !profile.is_read_only() {
        return set_session_writes(&app, conn, b.on);
    }
    let name = crate::value::escape(&profile.name);
    if profile.env == crate::model::Env::Prod {
        let q = Question::typed(
            format!(
                "{name} is a prod container. Type its name to turn writes on for this session."
            ),
            profile.name.clone(),
        );
        app.ask(
            Action::SessionWritesTyped {
                conn,
                expect: profile.name,
            },
            q,
        )
    } else {
        let q = Question::ask(format!(
            "Turn writes on for {name} in this session? Each write still asks as the write table says."
        ));
        app.ask(Action::SessionWrites { conn }, q)
    }
}

/// The answer to the question about a container's writes.
pub fn session_writes_on(app: &App, conn: ConnId) -> Response {
    set_session_writes(app, conn, true)
}

/// The answer to the typed question about a prod container's writes.
pub fn session_writes_typed(
    app: &App,
    conn: ConnId,
    expect: &str,
    typed: Option<&str>,
) -> Response {
    if typed != Some(expect) {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "writes stay off: the name typed does not match",
        );
    }
    set_session_writes(app, conn, true)
}

fn set_session_writes(app: &App, conn: ConnId, on: bool) -> Response {
    let done = app.hub.with_mut(|c| {
        if c.id != conn {
            return None;
        }
        c.session.profile.read_only = Some(!on);
        Some(crate::value::escape(&c.session.profile.name))
    });
    let Some(Some(name)) = done else {
        return error(StatusCode::CONFLICT, super::writes::CHANGED);
    };
    app.hub
        .emit(crate::web::session::Event::new("state", app.hub.state()));
    ok(serde_json::json!({
        "message": format!(
            "Writes are {} for {name} in this session only (a container has no saved profile).",
            if on { "on" } else { "off" }
        ),
        "state": app.hub.state(),
    }))
}

/// `GET /api/connections`: the saved connections.
pub async fn list(State(app): State<App>) -> Response {
    let path = app.hub.config.clone();
    let loaded = tokio::task::spawn_blocking(move || Config::load(&path)).await;
    let connected = app
        .hub
        .with(|c| (!c.docker).then(|| crate::value::escape(&c.profile().name)))
        .flatten();
    match loaded {
        Ok(Ok(cfg)) => ok(serde_json::json!({
            "connections": cfg.profiles.iter().map(dto::Connection::of).collect::<Vec<_>>(),
            "connected": connected,
        })),
        Ok(Err(e)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &crate::value::escape(&crate::conn::parse::mask(&e.to_string())),
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the config file could not be read",
        ),
    }
}

/// `POST /api/connections/<name>/test`: connects once and runs the permission check, as
/// `conn test` does. Nothing stays open.
pub async fn test(State(app): State<App>, Path(name): Path<String>) -> Response {
    let path = app.hub.config.clone();
    let n = name.clone();
    let profile =
        tokio::task::spawn_blocking(move || Config::load(&path).map(|c| c.get(&n).cloned())).await;
    let profile = match profile {
        Ok(Ok(Some(p))) => p,
        Ok(Ok(None)) => return error(StatusCode::NOT_FOUND, NO_SUCH),
        Ok(Err(e)) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &crate::value::escape(&crate::conn::parse::mask(&e.to_string())),
            );
        }
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the config file could not be read",
            );
        }
    };
    let mem = match prefetch_async(Arc::clone(&app.hub.store), profile.clone()).await {
        Ok(m) => m,
        Err(why) => return error(StatusCode::BAD_GATEWAY, &crate::value::escape(&why)),
    };
    let mut conn = match crate::conn::connect(&profile, &mem).await {
        Ok(c) => c,
        Err(e) => {
            return error(
                StatusCode::BAD_GATEWAY,
                &crate::value::escape(&crate::conn::parse::mask(&e.to_string())),
            );
        }
    };
    let report = crate::conn::check_permissions(&mut conn).await;
    let answer = conn.sentinel().cloned();
    drop(conn);
    // Open, but every command refused for want of a password: a failed test, as a
    // connect of the session is a failed one.
    if let Some(why) = crate::web::session::auth_refused(&report) {
        return error(StatusCode::BAD_GATEWAY, why);
    }
    let at = answer
        .as_ref()
        .map_or(&profile.endpoint, |a| &a.primary)
        .to_string();
    ok(serde_json::json!({
        "connected": true,
        "connection": dto::Connection::of(&profile),
        "at": crate::value::escape(&at),
        "sentinel": answer.as_ref().map(|a| crate::value::escape(&cli::sentinel_line(a))),
        "checks": dto::perms(&report),
    }))
}

/// Why a connection name finds nothing.
pub const NO_SUCH: &str = "there is no saved connection by that name";

/// `GET /api/docker`: Redis and Valkey containers Docker discovery finds.
pub async fn docker(State(_app): State<App>) -> Response {
    match crate::conn::docker::discover().await {
        Ok(found) => ok(serde_json::json!({
            "instances": found.iter().map(dto::Docker::of).collect::<Vec<_>>(),
        })),
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            &crate::value::escape(&crate::conn::parse::mask(&e.to_string())),
        ),
    }
}

/// `POST /api/connect` body: a saved profile or a container.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectTo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub docker: Option<String>,
}

/// `POST /api/connect` `{"name": "..."}` or `{"docker": "<container>"}`: connects as
/// the TUI does, replacing the current connection. Answers at once (202) with the state;
/// how the connect goes comes as `state` events.
pub async fn connect(State(app): State<App>, Body(to): Body<ConnectTo>) -> Response {
    let target = match (to.name, to.docker) {
        (Some(n), None) => Target::Profile(n),
        (None, Some(d)) => Target::Docker(d),
        _ => return error(StatusCode::BAD_REQUEST, "give one of name and docker"),
    };
    match app.hub.connect(target).await {
        Ok(()) => {
            // Questions about the old connection are not answered on the new one.
            app.confirms().clear();
            (StatusCode::ACCEPTED, axum::Json(app.hub.state())).into_response()
        }
        Err(ConnectError::NotFound) => error(StatusCode::NOT_FOUND, NO_SUCH),
        Err(ConnectError::Failed(why)) => {
            error(StatusCode::BAD_GATEWAY, &crate::value::escape(&why))
        }
    }
}

/// `POST /api/disconnect`.
pub async fn disconnect(State(app): State<App>) -> Response {
    app.hub.disconnect();
    app.confirms().clear();
    ok(app.hub.state())
}

/// `POST /api/node` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pick {
    pub index: usize,
}

/// `POST /api/node` `{"index": N}`: picks the cluster node the per-node views read.
pub async fn pick(State(app): State<App>, Body(p): Body<Pick>) -> Response {
    if app.hub.pick(p.index) {
        ok(app.hub.state())
    } else {
        error(StatusCode::UNPROCESSABLE_ENTITY, "there is no such node")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_add_request_never_shows_its_password() {
        let r: AddRequest = serde_json::from_str(
            r#"{"name":"cache","url":"redis://u:url-pw@h:1","password":"body-pw",
                "sentinel":"s:26379","sentinel_master":"m","sentinel_password":"s-pw"}"#,
        )
        .unwrap();
        let shown = format!("{r:?}");
        assert!(!shown.contains("pw"), "{shown}");
        let a = args_of(&r, false, None);
        assert!(a.password_stdin && a.sentinel_ask_password && !a.yes);
        let cleaned = clean("failed with body-pw and url-pw and s-pw", &r);
        assert!(!cleaned.contains("body-pw"), "{cleaned}");
        assert!(!cleaned.contains("url-pw"), "{cleaned}");
        assert!(!cleaned.contains("s-pw"), "{cleaned}");
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(
            serde_json::from_str::<AddRequest>(r#"{"name":"a","url":"redis://h","x":1}"#).is_err()
        );
    }
}
