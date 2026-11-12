//! Connecting to servers: connection strings, auth, permission checks, Docker discovery.
//!
//! Owner: connections.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use redis::aio::{ConnectionLike, MultiplexedConnection};
use redis::{
    AsyncConnectionConfig, ConnectionAddr, IntoConnectionInfo, RedisConnectionInfo, ServerErrorKind,
};
use secrecy::ExposeSecret;

use crate::model::{
    Auth, Endpoint, PERM_FREQ, PERM_INFO, PERM_LABELS, PERM_MEMORY, PERM_MEMORY_STATS, PERM_SCAN,
    PERM_TYPE_TTL, PermCheck, PermStatus, PermissionReport, Profile,
};
use crate::secrets::{SecretError, SecretStore};

pub mod docker;
pub mod parse;
pub mod provider;
mod tls;
pub mod token;

pub use parse::{AzureKind, ConnFormat, ParsedConnection, parse_connection_string};

#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error("could not parse the connection string: {0}")]
    Parse(String),
    #[error("{0}")]
    Secret(#[from] SecretError),
    #[error("no credentials in the keychain for profile {0}; run `redis-lens conn add` again")]
    MissingCredentials(String),
    #[error("this build has no Entra ID support; rebuild with --features entra-id")]
    EntraUnsupported,
    /// The token command gave no token. Never holds the token or the command's output.
    #[error(transparent)]
    Token(#[from] token::TokenError),
    #[error(transparent)]
    Redis(#[from] redis::RedisError),
    #[error("Docker: {0}")]
    Docker(String),
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_KEY: &str = "redis-lens:probe";

/// Open a connection for a profile, reading credentials from `secrets` when
/// `profile.auth` is `Auth::Keychain`, or running its token command for `Auth::Command`.
/// Selects `profile.db`.
///
/// Errors never contain the password or token: the server's `WRONGPASS` message does
/// not echo it, `RedisConnectionInfo`'s `Debug` redacts it, and a failed token command
/// is reported by program and exit status only.
pub async fn connect(
    profile: &Profile,
    secrets: &dyn SecretStore,
) -> Result<MultiplexedConnection, ConnError> {
    let auth = ResolvedAuth::new(profile, secrets).await?;
    auth.open(&profile.endpoint).await
}

/// Run the profile's token command (see [`token`]) on a blocking thread and return the
/// token. Runs again on every call: tokens are short-lived and never stored.
async fn token_for(profile: &Profile) -> Result<secrecy::SecretString, ConnError> {
    let argv = profile.token_command.clone().unwrap_or_default();
    match tokio::task::spawn_blocking(move || token::run_token_command(&argv)).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(token::TokenError::Spawn {
            program: profile
                .token_command
                .as_ref()
                .and_then(|a| a.first().cloned())
                .unwrap_or_default(),
            reason: "the task running it failed".into(),
        }
        .into()),
    }
}

/// A profile's credentials and connection settings, resolved once: one keychain read,
/// one token command run, or one Entra ID token provider, shared by every connection
/// opened with it.
struct ResolvedAuth {
    /// Username, password and database.
    settings: RedisConnectionInfo,
    /// Timeouts, plus the Entra ID credentials provider when there is one.
    config: AsyncConnectionConfig,
    /// Whether connecting sends a username, password or token.
    sends_credentials: bool,
}

impl ResolvedAuth {
    async fn new(profile: &Profile, secrets: &dyn SecretStore) -> Result<Self, ConnError> {
        let mut settings = RedisConnectionInfo::default().set_db(i64::from(profile.db));
        let mut config = base_config();
        let mut sends_credentials = false;
        match profile.auth {
            Auth::None => {}
            Auth::Keychain => {
                let creds = secrets
                    .get(&profile.secret_ref())?
                    .ok_or_else(|| ConnError::MissingCredentials(profile.name.clone()))?;
                if let Some(user) = &creds.username {
                    settings = settings.set_username(user);
                    sends_credentials = true;
                }
                let pw = creds.password.expose_secret();
                if !pw.is_empty() {
                    settings = settings.set_password(pw);
                    sends_credentials = true;
                }
            }
            Auth::EntraId => {
                config = entra_config(config)?;
                sends_credentials = true;
            }
            Auth::Command => {
                // The username, if any, is in the keychain (rule 1); no entry means none.
                // Read it before running the command, so the token is held for as short
                // a time as possible.
                let username = secrets.get(&profile.secret_ref())?.and_then(|c| c.username);
                let token = token_for(profile).await?;
                if let Some(user) = &username {
                    settings = settings.set_username(user);
                }
                settings = settings.set_password(token.expose_secret());
                sends_credentials = true;
            }
        }
        Ok(Self {
            settings,
            config,
            sends_credentials,
        })
    }

    /// Connect to `ep` with these credentials, the endpoint's TLS setting and the database.
    async fn open(&self, ep: &Endpoint) -> Result<MultiplexedConnection, ConnError> {
        open(ep, self.settings.clone(), &self.config).await
    }
}

fn base_config() -> AsyncConnectionConfig {
    AsyncConnectionConfig::new()
        .set_connection_timeout(Some(CONNECT_TIMEOUT))
        .set_response_timeout(Some(RESPONSE_TIMEOUT))
}

/// The address to connect to for `ep`. A TLS endpoint gets the process's trust roots
/// (see [`tls`]) and keeps its host name, which the certificate is checked against even
/// when a pinned resolver supplies the IP addresses. `roots` is only called for TLS, so
/// a plain connection never loads the OS certificate store.
fn connection_addr<'a>(ep: &Endpoint, roots: impl FnOnce() -> &'a tls::TlsRoots) -> ConnectionAddr {
    if ep.tls {
        roots().tls_addr(&ep.host, ep.port)
    } else {
        ConnectionAddr::Tcp(ep.host.clone(), ep.port)
    }
}

/// Every connection, to a primary or a replica candidate, is opened here.
async fn open(
    ep: &Endpoint,
    settings: RedisConnectionInfo,
    config: &AsyncConnectionConfig,
) -> Result<MultiplexedConnection, ConnError> {
    let addr = connection_addr(ep, tls::tls_roots);
    let info = addr.into_connection_info()?.set_redis_settings(settings);
    let client = redis::Client::open(info)?;
    Ok(client
        .get_multiplexed_async_connection_with_config(config)
        .await?)
}

/// Send one `PING` to `ep` on a connection that sends no credentials (no `AUTH`, no
/// `HELLO`, no `CLIENT SETINFO`, no `SELECT`) and return the reply. Used by Docker
/// discovery to see whether a server wants a password. Never writes. A probe that takes
/// longer than `timeout` fails with an I/O timeout error.
pub async fn ping_without_credentials(
    ep: &Endpoint,
    timeout: Duration,
) -> Result<String, ConnError> {
    let timed_out = || {
        ConnError::Redis(redis::RedisError::from(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the probe timed out",
        )))
    };
    let probe = async {
        let settings = RedisConnectionInfo::default().set_skip_set_lib_name();
        let config = base_config()
            .set_connection_timeout(Some(timeout))
            .set_response_timeout(Some(timeout));
        let mut c = open(ep, settings, &config).await?;
        Ok(redis::cmd("PING").query_async::<String>(&mut c).await?)
    };
    tokio::time::timeout(timeout, probe)
        .await
        .unwrap_or_else(|_| Err(timed_out()))
}

/// Which server a scan reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanTarget {
    Primary,
    /// A replica, at this endpoint. Also used when the profile already points at one.
    Replica(Endpoint),
    /// `prefer_replica` was set but no replica could be used; the reason is shown.
    PrimaryFallback(String),
}

impl std::fmt::Display for ScanTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Primary => write!(f, "primary"),
            Self::Replica(ep) => write!(f, "replica {ep}"),
            Self::PrimaryFallback(why) => write!(f, "primary (replica unreachable: {why})"),
        }
    }
}

const REPLICA_TIMEOUT: Duration = Duration::from_secs(2);

/// What `INFO replication` says about a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replication {
    /// `role:master`, with the replicas reported as `state=online`, in order.
    Master { online: Vec<(String, u16)> },
    /// `role:slave` (or `role:replica`).
    Replica,
    /// Anything else, or no role line.
    Unknown,
}

/// Parse the text of `INFO replication` (Redis and Valkey).
pub fn parse_replication(info: &str) -> Replication {
    let mut role = None;
    let mut online = Vec::new();
    for line in info.lines() {
        let line = line.trim();
        if let Some(r) = line.strip_prefix("role:") {
            role = Some(r.trim().to_string());
        } else if let Some((key, rest)) = line.split_once(':')
            && key
                .strip_prefix("slave")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            let field = |name: &str| {
                rest.split(',')
                    .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
            };
            if field("state") == Some("online")
                && let (Some(ip), Some(port)) = (field("ip"), field("port"))
                && let Ok(port) = port.parse::<u16>()
                && port != 0
                && let Some(host) = replica_host(ip)
            {
                online.push((host, port));
            }
        }
    }
    match role.as_deref() {
        Some("master") => Replication::Master { online },
        Some("slave" | "replica") => Replication::Replica,
        _ => Replication::Unknown,
    }
}

/// The host part of a `slaveN:ip=…` entry, or `None` when it can't be connected to.
///
/// An IP address is returned in its normal form, without brackets; the unspecified
/// addresses (`0.0.0.0`, `::`) are refused. Anything else must look like a host name:
/// dot-separated labels of ASCII letters, digits, `-` and `_`.
fn replica_host(ip: &str) -> Option<String> {
    let ip = ip.trim();
    let bare = ip
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(ip);
    if let Ok(addr) = bare.parse::<IpAddr>() {
        return (!addr.to_canonical().is_unspecified()).then(|| addr.to_string());
    }
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    (bare.len() <= 253 && bare.split('.').all(label_ok)).then(|| bare.to_string())
}

/// Loopback by name alone: `localhost` (and `*.localhost`), `127.0.0.0/8`, `::1`, and
/// `::ffff:127.x`. [`resolve`] also looks at what a name resolves to.
fn is_loopback_name(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost") || h.parse::<IpAddr>().is_ok_and(is_local_ip)
}

/// An address that reaches this machine: loopback, or unspecified (`0.0.0.0`, `::`).
fn is_local_ip(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback() || ip.is_unspecified()
}

/// A host name resolved once, so the address that was checked is the one connected to.
struct Resolved {
    addrs: Vec<SocketAddr>,
    /// The name is loopback, or any address it resolves to is loopback or unspecified.
    local: bool,
}

async fn resolve(host: &str, port: u16) -> std::io::Result<Resolved> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the name has no addresses",
        ));
    }
    let local = is_loopback_name(host) || addrs.iter().any(|a| is_local_ip(a.ip()));
    Ok(Resolved { addrs, local })
}

/// A DNS resolver for redis-rs that always returns addresses resolved earlier. The host
/// name still goes to TLS for certificate checks; only the lookup is skipped, so a name
/// can't resolve to one address for the check and another for the connection.
struct Pinned(Vec<SocketAddr>);

impl redis::io::AsyncDNSResolver for Pinned {
    fn resolve<'a, 'b: 'a>(
        &'a self,
        _host: &'b str,
        _port: u16,
    ) -> redis::RedisFuture<'a, Box<dyn Iterator<Item = SocketAddr> + Send + 'a>> {
        let addrs = self.0.clone();
        Box::pin(async move {
            Ok(Box::new(addrs.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
        })
    }
}

/// Whether credentials may go to a candidate that answered `NOAUTH`: only when it is
/// not on this machine, or the primary is on this machine too. A loopback address
/// announced by a remote primary names the machine running redis-lens, not the replica.
/// Both flags come from [`resolve`].
fn may_send_credentials(primary_local: bool, candidate_local: bool) -> bool {
    !candidate_local || primary_local
}

/// Whether `INFO replication` from a candidate shows a replica of the primary whose
/// replication ID is `primary_replid`.
fn is_replica_of(info: &str, primary_replid: &str) -> bool {
    parse_replication(info) == Replication::Replica
        && master_replid(info).as_deref() == Some(primary_replid)
}

/// Why a replica candidate was not used.
enum Unusable {
    /// It answered, but it is not a replica of this primary.
    NotAReplica,
    /// It needs credentials to be checked and is not somewhere they may be sent.
    WontSendCredentials,
    /// Its host name did not resolve.
    Resolve(std::io::Error),
    Conn(ConnError),
}

impl From<ConnError> for Unusable {
    fn from(e: ConnError) -> Self {
        Self::Conn(e)
    }
}

impl From<redis::RedisError> for Unusable {
    fn from(e: redis::RedisError) -> Self {
        Self::Conn(e.into())
    }
}

async fn info_replication(c: &mut MultiplexedConnection) -> Result<String, redis::RedisError> {
    redis::cmd("INFO").arg("replication").query_async(c).await
}

/// Open a connection to the candidate at `ep` and check that it is a replica of the
/// primary with `primary_replid`, before anything is scanned.
///
/// The candidate's name is resolved once and every connection to it uses those
/// addresses. It is first asked for `INFO replication` on a connection that sends no
/// credentials (no `AUTH`, no `HELLO`, no `CLIENT SETINFO`). A candidate that answers
/// and passes is scanned on that same connection and never gets credentials. Only a
/// candidate that answers `NOAUTH` may get them, and only when the profile has some and
/// [`may_send_credentials`] allows; it is then checked again on the authenticated
/// connection, which is the one returned.
async fn verified_replica(
    auth: &ResolvedAuth,
    db: u32,
    primary: &Endpoint,
    primary_replid: &str,
    ep: &Endpoint,
) -> Result<MultiplexedConnection, Unusable> {
    let candidate = resolve(&ep.host, ep.port)
        .await
        .map_err(Unusable::Resolve)?;
    let pinned =
        |config: AsyncConnectionConfig| config.set_dns_resolver(Pinned(candidate.addrs.clone()));

    let settings = RedisConnectionInfo::default().set_skip_set_lib_name();
    let mut probe = open(ep, settings, &pinned(base_config())).await?;
    match info_replication(&mut probe).await {
        Ok(info) if is_replica_of(&info, primary_replid) => {
            if db != 0 {
                redis::cmd("SELECT")
                    .arg(db)
                    .query_async::<()>(&mut probe)
                    .await?;
            }
            return Ok(probe);
        }
        Ok(_) => return Err(Unusable::NotAReplica),
        Err(e) if auth.sends_credentials && e.code() == Some("NOAUTH") => {}
        Err(e) => return Err(e.into()),
    }
    drop(probe);

    // A primary whose name no longer resolves counts as remote, which only restricts.
    let primary_local = match resolve(&primary.host, primary.port).await {
        Ok(r) => r.local,
        Err(_) => false,
    };
    if !may_send_credentials(primary_local, candidate.local) {
        return Err(Unusable::WontSendCredentials);
    }
    let mut c = open(ep, auth.settings.clone(), &pinned(auth.config.clone())).await?;
    let info = info_replication(&mut c).await?;
    if is_replica_of(&info, primary_replid) {
        Ok(c)
    } else {
        Err(Unusable::NotAReplica)
    }
}

/// The `master_replid` field of `INFO replication`. A primary reports its own
/// replication ID there, and a replica reports the ID of the primary it follows.
pub fn master_replid(info: &str) -> Option<String> {
    info.lines()
        .find_map(|l| l.trim().strip_prefix("master_replid:"))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// Connect for a scan. Connects to the primary as [`connect`] does; with
/// `profile.prefer_replica` it then looks for an online replica and, within a 2 s
/// timeout, connects to the first one with the same credentials and TLS and checks that
/// it really is a replica of this primary (see [`verified_replica`]). A replica that
/// can't be used or verified leaves the scan on the primary as `PrimaryFallback`. A
/// profile that already points at a replica is scanned as is.
///
/// Credentials are resolved once, before any connection, and reused for the replica.
pub async fn connect_for_scan(
    profile: &Profile,
    secrets: &dyn SecretStore,
) -> Result<(MultiplexedConnection, ScanTarget), ConnError> {
    let auth = ResolvedAuth::new(profile, secrets).await?;
    let mut primary = auth.open(&profile.endpoint).await?;
    if !profile.prefer_replica {
        return Ok((primary, ScanTarget::Primary));
    }
    let fallback = |c, why: String| Ok((c, ScanTarget::PrimaryFallback(why)));
    let info = match info_replication(&mut primary).await {
        Ok(i) => i,
        Err(e) if is_noperm(&e) => return fallback(primary, noperm_text("INFO replication")),
        Err(e) => return fallback(primary, format!("INFO replication: {}", server_message(&e))),
    };
    match parse_replication(&info) {
        Replication::Replica => Ok((primary, ScanTarget::Replica(profile.endpoint.clone()))),
        Replication::Unknown => Ok((primary, ScanTarget::Primary)),
        Replication::Master { online } => {
            let Some((host, port)) = online.into_iter().next() else {
                return fallback(primary, "no online replica".to_string());
            };
            let Some(replid) = master_replid(&info) else {
                return fallback(
                    primary,
                    "the primary does not report master_replid, so no replica can be verified"
                        .to_string(),
                );
            };
            let ep = Endpoint {
                host,
                port,
                tls: profile.endpoint.tls,
            };
            let attempt = verified_replica(&auth, profile.db, &profile.endpoint, &replid, &ep);
            match tokio::time::timeout(REPLICA_TIMEOUT, attempt).await {
                Ok(Ok(c)) => Ok((c, ScanTarget::Replica(ep))),
                Ok(Err(Unusable::NotAReplica)) => fallback(
                    primary,
                    format!("replica at {ep} is not a replica of this primary"),
                ),
                Ok(Err(Unusable::WontSendCredentials)) => fallback(
                    primary,
                    format!(
                        "replica at {ep} needs credentials to be verified and is a loopback \
                         address that is not the primary's host"
                    ),
                ),
                Ok(Err(Unusable::Resolve(e))) => {
                    fallback(primary, format!("{ep}: could not resolve the host: {e}"))
                }
                Ok(Err(Unusable::Conn(e))) => fallback(primary, format!("{ep}: {e}")),
                Err(_) => fallback(primary, format!("{ep}: timed out")),
            }
        }
    }
}

#[cfg(not(feature = "entra-id"))]
fn entra_config(_config: AsyncConnectionConfig) -> Result<AsyncConnectionConfig, ConnError> {
    Err(ConnError::EntraUnsupported)
}

/// Token refresh runs in a background task started on the current tokio runtime.
#[cfg(feature = "entra-id")]
fn entra_config(config: AsyncConnectionConfig) -> Result<AsyncConnectionConfig, ConnError> {
    let mut provider = redis::EntraIdCredentialsProvider::new_developer_tools()?;
    provider.start(redis::RetryConfig::default());
    Ok(config.set_credentials_provider(provider))
}

/// Only an ACL refusal (`NOPERM`) is a real denial. Compared by error kind, never by
/// searching the message.
fn is_noperm(e: &redis::RedisError) -> bool {
    e.kind() == redis::ErrorKind::Server(ServerErrorKind::NoPerm)
}

/// What a check shows when the ACL refuses `command`. Fixed text: the server's own
/// `NOPERM` message names the ACL user (CLAUDE.md rule 1).
pub(crate) fn noperm_text(command: &str) -> String {
    format!("NOPERM {command} is not allowed for this user")
}

/// The server's error text, for errors other than `NOPERM` (which names the user).
fn server_message(e: &redis::RedisError) -> String {
    match (e.code(), e.detail()) {
        (Some(c), Some(d)) => format!("{c} {d}"),
        (Some(c), None) => c.to_string(),
        (None, Some(d)) => d.to_string(),
        (None, None) => e.category().to_string(),
    }
}

/// `missing_key`: the probe key does not exist, so a plain `ERR` (such as
/// `ERR no such key`) says the command ran. A `NOPERM` reads [`noperm_text`] for
/// `command`, never the server's text.
fn classify(command: &str, r: Result<(), redis::RedisError>, missing_key: bool) -> PermStatus {
    match r {
        Ok(()) => PermStatus::Allowed,
        Err(e) if is_noperm(&e) => PermStatus::Denied(noperm_text(command)),
        Err(e)
            if missing_key
                && e.kind() == redis::ErrorKind::Server(ServerErrorKind::ResponseError)
                && !server_message(&e)
                    .to_ascii_lowercase()
                    .contains("unknown command") =>
        {
            PermStatus::Allowed
        }
        Err(e) => PermStatus::Denied(server_message(&e)),
    }
}

fn policy_from_info(info: &str) -> Option<String> {
    info.lines()
        .find_map(|l| l.strip_prefix("maxmemory_policy:"))
        .map(|p| p.trim().to_string())
}

/// Try each command the views need once and report which are allowed.
///
/// Never fails as a whole: an error on one command becomes a `Denied` entry. Checks
/// `OBJECT FREQ` only when the eviction policy is LFU, otherwise reports `Skipped`.
/// Sends read-only commands only.
pub async fn check_permissions<C: ConnectionLike + Send>(conn: &mut C) -> PermissionReport {
    // SCAN
    let scan: Result<(String, Vec<Vec<u8>>), _> = redis::cmd("SCAN")
        .arg(0)
        .arg("COUNT")
        .arg(10)
        .query_async(conn)
        .await;
    let (scan_status, probe_key, missing) = match scan {
        Ok((_, keys)) => match keys.into_iter().next() {
            Some(k) => (PermStatus::Allowed, k, false),
            None => (PermStatus::Allowed, PROBE_KEY.as_bytes().to_vec(), true),
        },
        Err(e) => (
            classify("SCAN", Err(e), false),
            PROBE_KEY.as_bytes().to_vec(),
            true,
        ),
    };

    // TYPE and PTTL
    let ty: Result<String, _> = redis::cmd("TYPE").arg(&probe_key).query_async(conn).await;
    let type_ttl = match ty {
        Ok(_) => {
            let r: Result<i64, _> = redis::cmd("PTTL").arg(&probe_key).query_async(conn).await;
            classify("PTTL", r.map(|_| ()), missing)
        }
        Err(e) => classify("TYPE", Err(e), missing),
    };

    // MEMORY USAGE
    let mem: Result<Option<i64>, _> = redis::cmd("MEMORY")
        .arg("USAGE")
        .arg(&probe_key)
        .query_async(conn)
        .await;
    let memory = classify("MEMORY USAGE", mem.map(|_| ()), missing);

    // INFO memory
    let info: Result<String, _> = redis::cmd("INFO").arg("memory").query_async(conn).await;
    let (info_status, policy) = match info {
        Ok(text) => (PermStatus::Allowed, Ok(policy_from_info(&text))),
        Err(e) => {
            let status = classify("INFO", Err(e), false);
            let why = match &status {
                PermStatus::Denied(m) => m.clone(),
                _ => String::new(),
            };
            (status, Err(why))
        }
    };

    // OBJECT FREQ
    let freq = match policy {
        Err(why) => PermStatus::Skipped(format!("INFO is not available: {why}")),
        Ok(None) => PermStatus::Skipped("eviction policy is unknown, not LFU".to_string()),
        Ok(Some(p)) if !p.to_ascii_lowercase().contains("lfu") => {
            PermStatus::Skipped(format!("eviction policy is {p}, not LFU"))
        }
        Ok(Some(_)) => {
            let r: Result<Option<i64>, _> = redis::cmd("OBJECT")
                .arg("FREQ")
                .arg(&probe_key)
                .query_async(conn)
                .await;
            classify("OBJECT FREQ", r.map(|_| ()), missing)
        }
    };

    // MEMORY STATS: no key involved, so any error other than NOPERM (an unknown
    // command on a managed service, say) is a denial with the server's reason. A NOPERM
    // reads fixed text, since the server's names the user.
    let stats: Result<redis::Value, _> = redis::cmd("MEMORY").arg("STATS").query_async(conn).await;
    let memory_stats = classify("MEMORY STATS", stats.map(|_| ()), false);

    let mut statuses = [
        (PERM_SCAN, scan_status),
        (PERM_TYPE_TTL, type_ttl),
        (PERM_MEMORY, memory),
        (PERM_FREQ, freq),
        (PERM_INFO, info_status),
        (PERM_MEMORY_STATS, memory_stats),
    ];
    let mut checks = Vec::with_capacity(PERM_LABELS.len());
    for label in PERM_LABELS {
        if let Some(slot) = statuses.iter_mut().find(|(l, _)| *l == label) {
            checks.push(PermCheck {
                label,
                status: std::mem::replace(&mut slot.1, PermStatus::Skipped(String::new())),
            });
        }
    }
    PermissionReport { checks }
}

/// Labels for the view probes ([`check_view_permissions`]), in display order. Each one
/// turns a view or panel on or off (spec "Permission check, additions").
pub const PERM_SLOWLOG: &str = "SLOWLOG";
pub const PERM_CLIENT_LIST: &str = "CLIENT LIST";
pub const PERM_XINFO: &str = "XINFO";
pub const PERM_PUBSUB: &str = "PUBSUB";
/// `FT._LIST`: Redis Search or valkey-search. Without the module it reads as skipped
/// (unsupported), never denied.
pub const PERM_SEARCH: &str = "FT._LIST";
pub const VIEW_PERM_LABELS: [&str; 5] = [
    PERM_SLOWLOG,
    PERM_CLIENT_LIST,
    PERM_XINFO,
    PERM_PUBSUB,
    PERM_SEARCH,
];

/// A client ID no client has (`i64::MAX`): `CLIENT LIST ID` with it checks the ACL and
/// returns no client's line, and needs no `+client|id` to learn the connection's own ID.
const NO_CLIENT_ID: i64 = i64::MAX;

/// What a failed view probe of `command` reads as, in redis-lens's own words (the
/// server's text can name the ACL user). `NOPERM` is a denial; an unknown command, a
/// missing module or any other failure turns the view off as skipped, with the reason.
fn classify_view_probe(
    label: &str,
    command: &str,
    r: Result<redis::Value, redis::RedisError>,
) -> PermStatus {
    match r {
        Ok(_) => PermStatus::Allowed,
        Err(e) if is_noperm(&e) => PermStatus::Denied(noperm_text(command)),
        Err(e) if crate::live::is_unknown_command(&e) => {
            if label == PERM_SEARCH {
                PermStatus::Skipped("the Search module is not loaded".to_string())
            } else {
                PermStatus::Skipped(format!("the server does not support {command}"))
            }
        }
        Err(_) => PermStatus::Skipped(format!("{command} could not be checked")),
    }
}

/// Try one read-only command for each view that needs one and report which are
/// allowed: `SLOWLOG LEN`, `CLIENT LIST ID <an id no client has>`, `XINFO HELP`,
/// `PUBSUB NUMPAT` and `FT._LIST`.
///
/// Never fails as a whole, and never puts the server's error text in a status.
pub async fn check_view_permissions<C: ConnectionLike + Send>(conn: &mut C) -> Vec<PermCheck> {
    let probes: [(&'static str, &str, redis::Cmd); 5] = [
        (PERM_SLOWLOG, "SLOWLOG LEN", {
            let mut c = redis::cmd("SLOWLOG");
            c.arg("LEN");
            c
        }),
        (PERM_CLIENT_LIST, "CLIENT LIST", {
            let mut c = redis::cmd("CLIENT");
            c.arg("LIST").arg("ID").arg(NO_CLIENT_ID);
            c
        }),
        (PERM_XINFO, "XINFO", {
            let mut c = redis::cmd("XINFO");
            c.arg("HELP");
            c
        }),
        (PERM_PUBSUB, "PUBSUB NUMPAT", {
            let mut c = redis::cmd("PUBSUB");
            c.arg("NUMPAT");
            c
        }),
        (PERM_SEARCH, "FT._LIST", redis::cmd("FT._LIST")),
    ];
    let mut checks = Vec::with_capacity(probes.len());
    for (label, command, cmd) in probes {
        let r: Result<redis::Value, _> = cmd.query_async(conn).await;
        checks.push(PermCheck {
            label,
            status: classify_view_probe(label, command, r),
        });
    }
    checks
}

/// [`check_permissions`] followed by [`check_view_permissions`]: what the TUI checks on
/// connect. The scan command and `conn test` use [`check_permissions`] alone.
pub async fn check_all_permissions<C: ConnectionLike + Send>(conn: &mut C) -> PermissionReport {
    let mut report = check_permissions(conn).await;
    report.checks.extend(check_view_permissions(conn).await);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_probes_read_in_redis_lens_words() {
        let err = |kind, code: &'static str, detail: &str| {
            Err(redis::RedisError::from((
                redis::ErrorKind::Server(kind),
                code,
                detail.to_string(),
            )))
        };
        let noperm = || {
            err(
                ServerErrorKind::NoPerm,
                "NOPERM",
                "User lens-ro has no permissions to run the 'slowlog|len' command",
            )
        };
        assert_eq!(
            classify_view_probe(PERM_SLOWLOG, "SLOWLOG LEN", Ok(redis::Value::Int(0))),
            PermStatus::Allowed
        );
        let denied = classify_view_probe(PERM_SLOWLOG, "SLOWLOG LEN", noperm());
        assert_eq!(
            denied,
            PermStatus::Denied("NOPERM SLOWLOG LEN is not allowed for this user".into())
        );
        // A missing module is unsupported, not denied.
        let unknown = || {
            err(
                ServerErrorKind::ResponseError,
                "ERR",
                "unknown command 'FT._LIST', with args beginning with: ",
            )
        };
        assert_eq!(
            classify_view_probe(PERM_SEARCH, "FT._LIST", unknown()),
            PermStatus::Skipped("the Search module is not loaded".into())
        );
        assert_eq!(
            classify_view_probe(PERM_SEARCH, "FT._LIST", noperm()),
            PermStatus::Denied("NOPERM FT._LIST is not allowed for this user".into())
        );
        assert_eq!(
            classify_view_probe(PERM_XINFO, "XINFO", unknown()),
            PermStatus::Skipped("the server does not support XINFO".into())
        );
        // Anything else: skipped, and the server's text is not kept.
        let odd = classify_view_probe(
            PERM_CLIENT_LIST,
            "CLIENT LIST",
            err(
                ServerErrorKind::ResponseError,
                "ERR",
                "syntax error near lens-ro",
            ),
        );
        assert_eq!(
            odd,
            PermStatus::Skipped("CLIENT LIST could not be checked".into())
        );
    }

    #[test]
    fn policy_parsing() {
        let info = "# Memory\r\nused_memory:1\r\nmaxmemory_policy:allkeys-lfu\r\n";
        assert_eq!(policy_from_info(info).as_deref(), Some("allkeys-lfu"));
        assert_eq!(policy_from_info("# Memory\r\n"), None);
    }

    const REDIS7_MASTER: &str = "# Replication\r\nrole:master\r\nconnected_slaves:2\r\nslave0:ip=172.17.0.3,port=6379,state=online,offset=1540,lag=0\r\nslave1:ip=172.17.0.4,port=6380,state=online,offset=1540,lag=1\r\nmaster_failover_state:no-failover\r\nmaster_replid:8a5a1d0b8f0e5b4a3c9d1e2f3a4b5c6d7e8f9a0b\r\nmaster_replid2:0000000000000000000000000000000000000000\r\nmaster_repl_offset:1540\r\nsecond_repl_offset:-1\r\nrepl_backlog_active:1\r\nrepl_backlog_size:1048576\r\nrepl_backlog_first_byte_offset:1\r\nrepl_backlog_histlen:1540\r\n";
    const VALKEY8_MASTER: &str = "# Replication\r\nrole:master\r\nconnected_slaves:1\r\nslave0:ip=10.0.1.21,port=6379,state=online,offset=98,lag=0\r\nmaster_failover_state:no-failover\r\nmaster_replid:b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0\r\nmaster_replid2:0000000000000000000000000000000000000000\r\nmaster_repl_offset:98\r\nsecond_repl_offset:-1\r\nrepl_backlog_active:1\r\nrepl_backlog_size:10485760\r\nrepl_backlog_first_byte_offset:1\r\nrepl_backlog_histlen:98\r\n";
    const REDIS7_REPLICA: &str = "# Replication\r\nrole:slave\r\nmaster_host:172.17.0.2\r\nmaster_port:6379\r\nmaster_link_status:up\r\nmaster_last_io_seconds_ago:1\r\nmaster_sync_in_progress:0\r\nslave_read_repl_offset:1540\r\nslave_repl_offset:1540\r\nslave_priority:100\r\nslave_read_only:1\r\nreplica_announced:1\r\nconnected_slaves:0\r\n";
    const VALKEY8_REPLICA: &str = "# Replication\r\nrole:slave\r\nmaster_host:10.0.1.20\r\nmaster_port:6379\r\nmaster_link_status:up\r\nslave_read_repl_offset:98\r\nslave_repl_offset:98\r\nreplica_announced:1\r\nconnected_slaves:0\r\n";

    #[test]
    fn replication_parsing() {
        assert_eq!(
            parse_replication(REDIS7_MASTER),
            Replication::Master {
                online: vec![("172.17.0.3".into(), 6379), ("172.17.0.4".into(), 6380)]
            }
        );
        assert_eq!(
            parse_replication(VALKEY8_MASTER),
            Replication::Master {
                online: vec![("10.0.1.21".into(), 6379)]
            }
        );
        assert_eq!(parse_replication(REDIS7_REPLICA), Replication::Replica);
        assert_eq!(parse_replication(VALKEY8_REPLICA), Replication::Replica);
        assert_eq!(parse_replication("# Replication\r\n"), Replication::Unknown);
    }

    #[test]
    fn replication_skips_replicas_that_are_not_online() {
        let info = "role:master\r\nconnected_slaves:2\r\nslave0:ip=10.0.0.1,port=6379,state=wait_bgsave,offset=0,lag=0\r\nslave1:ip=10.0.0.2,port=6379,state=online,offset=5,lag=0\r\nslave_read_only:1\r\nslaveX:ip=10.0.0.9,port=1,state=online\r\nslave2:ip=10.0.0.3,port=bad,state=online\r\n";
        assert_eq!(
            parse_replication(info),
            Replication::Master {
                online: vec![("10.0.0.2".into(), 6379)]
            }
        );
        assert_eq!(
            parse_replication("role:master\r\nconnected_slaves:0\r\n"),
            Replication::Master { online: vec![] }
        );
    }

    #[test]
    fn replication_rejects_unusable_addresses() {
        let info = "role:master\r\nslave0:ip=0.0.0.0,port=6379,state=online\r\nslave1:ip=,port=6379,state=online\r\nslave2:ip=::,port=6379,state=online\r\nslave3:ip=[::],port=6379,state=online\r\nslave4:ip=::ffff:0.0.0.0,port=6379,state=online\r\nslave5:ip=bad host,port=6379,state=online\r\nslave6:ip=a/b,port=6379,state=online\r\nslave7:ip=host..x,port=6379,state=online\r\nslave8:ip=10.0.0.5,port=0,state=online\r\nslave9:port=6379,state=online\r\n";
        assert_eq!(
            parse_replication(info),
            Replication::Master { online: vec![] }
        );
    }

    #[test]
    fn replication_accepts_ipv6_and_host_names() {
        let info = "role:master\r\nslave0:ip=::1,port=6379,state=online\r\nslave1:ip=2001:DB8::5,port=6380,state=online\r\nslave2:ip=[fe80::1],port=6381,state=online\r\nslave3:ip=replica-1.cache.internal,port=6382,state=online\r\nslave4:ip=cache_replica,port=6383,state=online\r\n";
        assert_eq!(
            parse_replication(info),
            Replication::Master {
                online: vec![
                    ("::1".into(), 6379),
                    ("2001:db8::5".into(), 6380),
                    ("fe80::1".into(), 6381),
                    ("replica-1.cache.internal".into(), 6382),
                    ("cache_replica".into(), 6383),
                ]
            }
        );
        assert_eq!(parse_replication("role:replica\r\n"), Replication::Replica);
    }

    #[test]
    fn replication_ids() {
        assert_eq!(
            master_replid(REDIS7_MASTER).as_deref(),
            Some("8a5a1d0b8f0e5b4a3c9d1e2f3a4b5c6d7e8f9a0b")
        );
        assert_eq!(master_replid("role:master\r\nmaster_replid:\r\n"), None);
        assert_eq!(master_replid(REDIS7_REPLICA), None);
        let id = "8a5a1d0b8f0e5b4a3c9d1e2f3a4b5c6d7e8f9a0b";
        let replica = format!("{REDIS7_REPLICA}master_replid:{id}\r\n");
        assert!(is_replica_of(&replica, id));
        assert!(!is_replica_of(&replica, "0000"));
        // A primary with the same ID is not a replica of it.
        assert!(!is_replica_of(REDIS7_MASTER, id));
        // A replica that does not report an ID can't be verified.
        assert!(!is_replica_of(REDIS7_REPLICA, id));
    }

    #[test]
    fn credentials_go_only_off_this_machine_unless_the_primary_is_on_it() {
        assert!(may_send_credentials(false, false));
        assert!(may_send_credentials(true, false));
        assert!(may_send_credentials(true, true));
        assert!(!may_send_credentials(false, true));
    }

    #[tokio::test]
    async fn resolution_decides_what_is_on_this_machine() {
        for host in [
            "localhost",
            "127.0.0.1",
            "127.0.0.2",
            "::1",
            "0.0.0.0",
            "::",
        ] {
            let r = resolve(host, 6379).await.unwrap();
            assert!(r.local, "{host}");
            assert!(!r.addrs.is_empty(), "{host}");
        }
        // Names that resolve to loopback but are not recognisable from the text. The
        // system resolver on macOS and Linux accepts these legacy forms; where one
        // does not resolve there is nothing to connect to anyway.
        for host in ["127.1", "2130706433"] {
            assert!(!is_loopback_name(host), "{host}");
            if let Ok(r) = resolve(host, 6379).await {
                assert!(r.local, "{host} resolved to {:?}", r.addrs);
            }
        }
        for host in ["10.0.1.21", "2001:db8::5"] {
            let r = resolve(host, 6379).await.unwrap();
            assert!(!r.local, "{host}");
            assert_eq!(r.addrs[0].ip(), host.parse::<IpAddr>().unwrap());
        }
        // A remote primary announcing a name that resolves to loopback: refused.
        let primary = resolve("10.0.1.20", 6379).await.unwrap();
        let candidate = resolve("localhost", 6379).await.unwrap();
        assert!(!may_send_credentials(primary.local, candidate.local));
        if let Ok(candidate) = resolve("127.1", 6379).await {
            assert!(!may_send_credentials(primary.local, candidate.local));
        }
    }

    #[tokio::test]
    async fn pinned_resolver_ignores_the_name() {
        use redis::io::AsyncDNSResolver;
        let addr: SocketAddr = "10.0.1.21:6379".parse().unwrap();
        let got: Vec<_> = Pinned(vec![addr])
            .resolve("evil.example.com", 1)
            .await
            .unwrap()
            .collect();
        assert_eq!(got, vec![addr]);
    }

    #[test]
    fn scan_target_text() {
        let ep = Endpoint {
            host: "10.0.1.21".into(),
            port: 6379,
            tls: false,
        };
        assert_eq!(ScanTarget::Primary.to_string(), "primary");
        assert_eq!(
            ScanTarget::Replica(ep).to_string(),
            "replica 10.0.1.21:6379"
        );
        assert_eq!(
            ScanTarget::PrimaryFallback("timed out".into()).to_string(),
            "primary (replica unreachable: timed out)"
        );
    }

    #[cfg(unix)]
    fn command_profile(cmd: &[&str]) -> Profile {
        Profile {
            name: "tok".into(),
            env: crate::model::Env::Prod,
            endpoint: ep("cache.example", 6380, true),
            db: 2,
            auth: Auth::Command,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: Some(cmd.iter().map(|s| s.to_string()).collect()),
        }
    }

    /// The token is sent as the password, with the keychain's username when there is
    /// one, and appears nowhere else.
    #[cfg(unix)]
    #[tokio::test]
    async fn token_command_is_run_at_every_resolve() {
        use crate::model::Credentials;
        use crate::secrets::MemoryStore;
        let store = MemoryStore::default();
        let p = command_profile(&["echo", "secret-token-123"]);
        let auth = ResolvedAuth::new(&p, &store).await.unwrap();
        assert!(auth.sends_credentials);
        assert_eq!(auth.settings.username(), None);
        assert_eq!(auth.settings.password(), Some("secret-token-123"));
        assert_eq!(auth.settings.db(), 2);
        assert!(!format!("{:?}", auth.settings).contains("secret-token-123"));

        store
            .set(
                &p.secret_ref(),
                &Credentials {
                    username: Some("oid-1234".into()),
                    password: secrecy::SecretString::from(String::new()),
                },
            )
            .unwrap();
        let auth = ResolvedAuth::new(&p, &store).await.unwrap();
        assert_eq!(auth.settings.username(), Some("oid-1234"));
        assert_eq!(auth.settings.password(), Some("secret-token-123"));

        // Run again each time: a command whose output changes gives a new token.
        let dir = std::env::temp_dir().join(format!("redis-lens-tok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t");
        std::fs::write(&file, "first\n").unwrap();
        let cat = command_profile(&["cat", file.to_str().unwrap()]);
        let empty = MemoryStore::default();
        let a = ResolvedAuth::new(&cat, &empty).await.unwrap();
        assert_eq!(a.settings.password(), Some("first"));
        std::fs::write(&file, "second\n").unwrap();
        let a = ResolvedAuth::new(&cat, &empty).await.unwrap();
        assert_eq!(a.settings.password(), Some("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_token_command_never_shows_its_output() {
        use crate::secrets::MemoryStore;
        let store = MemoryStore::default();
        let p = command_profile(&[
            "/bin/sh",
            "-c",
            "echo secret-token-123; echo secret-err >&2; exit 7",
        ]);
        let Err(e) = ResolvedAuth::new(&p, &store).await else {
            panic!("expected an error");
        };
        assert!(matches!(e, ConnError::Token(_)), "{e:?}");
        for text in [e.to_string(), format!("{e:?}")] {
            assert!(!text.contains("secret-token-123"), "{text}");
            assert!(!text.contains("secret-err"), "{text}");
        }
        assert!(
            e.to_string().contains("/bin/sh failed (exit status 7)"),
            "{e}"
        );
        // Through `connect`, too, before any connection is tried.
        let e = connect(&p, &store).await.unwrap_err();
        assert!(matches!(e, ConnError::Token(_)), "{e:?}");
        // No command saved at all.
        let mut none = command_profile(&[]);
        none.token_command = None;
        let Err(e) = ResolvedAuth::new(&none, &store).await else {
            panic!("expected an error");
        };
        assert!(e.to_string().contains("no token command"), "{e}");
    }

    #[test]
    fn classification() {
        let noperm = redis::RedisError::from((
            redis::ErrorKind::Server(ServerErrorKind::NoPerm),
            "denied",
            "User x has no permissions".to_string(),
        ));
        // Fixed text: the server's names the ACL user.
        assert_eq!(
            classify("MEMORY USAGE", Err(noperm), true),
            PermStatus::Denied("NOPERM MEMORY USAGE is not allowed for this user".into())
        );
        let nokey = redis::RedisError::from((
            redis::ErrorKind::Server(ServerErrorKind::ResponseError),
            "ERR",
            "no such key".to_string(),
        ));
        assert_eq!(classify("TYPE", Err(nokey), true), PermStatus::Allowed);
        assert_eq!(classify("SCAN", Ok(()), false), PermStatus::Allowed);
    }

    fn ep(host: &str, port: u16, tls: bool) -> Endpoint {
        Endpoint {
            host: host.to_string(),
            port,
            tls,
        }
    }

    fn test_roots() -> tls::TlsRoots {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        let ca = params.self_signed(&key).unwrap().der().clone();
        tls::TlsRoots::from_choice(tls::choose_roots(vec![ca], 0))
    }

    #[test]
    fn plain_tcp_gets_no_tls_params_and_never_loads_the_roots() {
        let addr = connection_addr(&ep("127.0.0.1", 6379, false), || {
            panic!("the trust roots were loaded for a plain connection")
        });
        assert_eq!(addr, ConnectionAddr::Tcp("127.0.0.1".into(), 6379));
    }

    #[test]
    fn tls_gets_the_shared_roots_and_verification_stays_on() {
        let roots = test_roots();
        let ConnectionAddr::TcpTls {
            host,
            port,
            insecure,
            tls_params: Some(params),
        } = connection_addr(&ep("db.example", 6380, true), || &roots)
        else {
            panic!("expected TLS with the OS roots");
        };
        assert_eq!((host.as_str(), port, insecure), ("db.example", 6380, false));
        assert!(format!("{params:?}").contains("root_cert_store: Some("));
    }

    /// A replica candidate is opened through the same [`connection_addr`] as the primary:
    /// the same roots, and its own announced name for the certificate check (the pinned
    /// resolver only supplies addresses).
    #[test]
    fn a_replica_candidate_gets_the_same_roots_and_its_announced_name() {
        let roots = test_roots();
        let params = |addr: ConnectionAddr| match addr {
            ConnectionAddr::TcpTls {
                host,
                insecure: false,
                tls_params: Some(p),
                ..
            } => (host, format!("{p:?}")),
            other => panic!("unexpected {other:?}"),
        };
        let (primary_host, primary_roots) =
            params(connection_addr(&ep("primary.example", 6380, true), || {
                &roots
            }));
        let (replica_host, replica_roots) = params(connection_addr(
            &ep("replica-1.example", 6380, true),
            || &roots,
        ));
        assert_eq!(primary_host, "primary.example");
        assert_eq!(replica_host, "replica-1.example");
        assert_eq!(primary_roots, replica_roots);
    }
}
