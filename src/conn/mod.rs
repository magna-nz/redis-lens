//! Connecting to servers: connection strings, auth, permission checks, Docker discovery.
//!
//! Owner: connections.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
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

pub mod cluster;
pub mod docker;
pub mod parse;
pub mod provider;
pub mod ssh;
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
    /// The SSH tunnel could not be opened. Never holds ssh's own output.
    #[error(transparent)]
    Ssh(#[from] ssh::SshError),
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
) -> Result<Connection, ConnError> {
    let route = Route::for_profile(profile, secrets).await?;
    let auth = ResolvedAuth::new(profile, secrets).await?;
    auth.open(&route, &profile.endpoint).await
}

/// How connections reach servers: directly, or through the profile's SSH bastion.
pub enum Route {
    Direct,
    /// Every endpoint, the profile's own and every announced node or replica, is reached
    /// through a local forward over this bastion (see [`ssh`]).
    Ssh(ssh::Bastion),
}

impl Route {
    /// The route for `profile`: through its SSH bastion when it has one (reading the SSH
    /// username from the keychain), otherwise direct. Starts no forward yet.
    pub async fn for_profile(
        profile: &Profile,
        secrets: &dyn SecretStore,
    ) -> Result<Arc<Self>, ConnError> {
        Ok(Arc::new(match &profile.ssh {
            None => Self::Direct,
            Some(tunnel) => Self::Ssh(ssh::Bastion::new(profile, tunnel, secrets)?),
        }))
    }

    /// The addresses to connect to for `ep`, fixed once, and whether `ep` counts as on
    /// the same machine as whatever reaches it. Direct: `ep`'s host resolved here
    /// ([`resolve`]). Through SSH: the local end of a forward to `ep` (started when
    /// first needed), with `local` judged from the bastion's side.
    async fn reach(&self, ep: &Endpoint) -> Result<Resolved, Unusable> {
        match self {
            Self::Direct => resolve(&ep.host, ep.port).await.map_err(Unusable::Resolve),
            Self::Ssh(bastion) => bastion.reach(ep).await,
        }
    }

    /// Whether `ep` is on the same machine as whatever reaches it, without starting a
    /// forward: direct, `ep`'s host resolved here (a name that no longer resolves counts
    /// as remote, which only restricts); through SSH, `ep`'s host is loopback by name, as
    /// seen from the bastion. The same judgement as [`Self::reach`]'s `local`.
    async fn is_local(&self, ep: &Endpoint) -> bool {
        match self {
            Self::Direct => resolve(&ep.host, ep.port).await.is_ok_and(|r| r.local),
            Self::Ssh(_) => is_loopback_name(&ep.host),
        }
    }

    fn is_ssh(&self) -> bool {
        matches!(self, Self::Ssh(_))
    }
}

/// A connection to one server, holding on to its [`Route`] so an SSH forward it goes
/// through stays open while the connection is in use. Use it as a
/// [`MultiplexedConnection`] (it derefs to one and implements [`ConnectionLike`]).
#[derive(Clone)]
pub struct Connection {
    conn: MultiplexedConnection,
    _route: Arc<Route>,
    /// An `INFO server` reply this connection already received, kept to answer the next
    /// `INFO server` instead of sending it again (see [`Self::keep_info_server`]).
    /// Shared by clones; taken once.
    info_server: Arc<std::sync::Mutex<Option<(std::time::Instant, redis::Value)>>>,
}

/// How long a kept `INFO server` reply may answer the next `INFO server`.
const INFO_SERVER_REUSE: Duration = Duration::from_secs(10);

impl Connection {
    fn new(conn: MultiplexedConnection, route: &Arc<Route>) -> Self {
        Self {
            conn,
            _route: Arc::clone(route),
            info_server: Arc::default(),
        }
    }

    /// Keep `reply`, an `INFO server` reply just read on this connection, so that the
    /// next `INFO server` sent through it (within [`INFO_SERVER_REUSE`]) is answered
    /// with it and not sent. Cluster detection reads `INFO server`, and the scan's first
    /// command is `INFO server` again: this saves the server one command per scan.
    fn keep_info_server(&self, reply: redis::Value) {
        *self
            .info_server
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((std::time::Instant::now(), reply));
    }

    /// The kept reply, when `cmd` is `INFO server` and the reply is fresh (see
    /// [`take_kept_info`]).
    fn take_info_server(&self, cmd: &redis::Cmd) -> Option<redis::Value> {
        let mut kept = self
            .info_server
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        take_kept_info(&mut kept, cmd, std::time::Instant::now())
    }

    /// The bare connection. Only for direct routes: through SSH the forward closes once
    /// every [`Connection`] that uses it is dropped.
    pub fn into_inner(self) -> MultiplexedConnection {
        self.conn
    }
}

/// Take the kept `INFO server` reply when `cmd` is `INFO server` and the reply is at
/// most [`INFO_SERVER_REUSE`] old at `now`: it answers one command at most. A stale
/// reply is dropped; any other command leaves a fresh one in place.
fn take_kept_info(
    kept: &mut Option<(std::time::Instant, redis::Value)>,
    cmd: &redis::Cmd,
    now: std::time::Instant,
) -> Option<redis::Value> {
    let at = kept.as_ref()?.0;
    if now.saturating_duration_since(at) > INFO_SERVER_REUSE {
        *kept = None;
        return None;
    }
    if !is_info_server(cmd) {
        return None;
    }
    kept.take().map(|(_, v)| v)
}

/// Says only which kind of route the connection uses: nothing about the server, the
/// credentials or the forward.
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let route = match self._route.as_ref() {
            Route::Direct => "direct",
            Route::Ssh(_) => "ssh",
        };
        f.debug_struct("Connection").field("route", &route).finish()
    }
}

impl std::ops::Deref for Connection {
    type Target = MultiplexedConnection;
    fn deref(&self) -> &MultiplexedConnection {
        &self.conn
    }
}

impl std::ops::DerefMut for Connection {
    fn deref_mut(&mut self) -> &mut MultiplexedConnection {
        &mut self.conn
    }
}

/// `cmd` is exactly `INFO server` (any case).
fn is_info_server(cmd: &redis::Cmd) -> bool {
    let mut args = cmd.args_iter().map(|a| match a {
        redis::Arg::Simple(b) => Some(b),
        _ => None,
    });
    matches!(
        (args.next(), args.next(), args.next()),
        (Some(Some(a)), Some(Some(b)), None)
            if a.eq_ignore_ascii_case(b"INFO") && b.eq_ignore_ascii_case(b"server")
    )
}

impl ConnectionLike for Connection {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        if let Some(reply) = self.take_info_server(cmd) {
            return Box::pin(async move { Ok(reply) });
        }
        self.conn.req_packed_command(cmd)
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a redis::Pipeline,
        offset: usize,
        count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        self.conn.req_packed_commands(cmd, offset, count)
    }

    fn get_db(&self) -> i64 {
        self.conn.get_db()
    }
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

    /// Connect to `ep` over `route` with these credentials, the endpoint's TLS setting
    /// and the database. Direct, redis-rs resolves the host; through SSH the connection
    /// goes to the forward's local end, and TLS still checks `ep`'s host name.
    async fn open(&self, route: &Arc<Route>, ep: &Endpoint) -> Result<Connection, ConnError> {
        let c = match route.as_ref() {
            Route::Direct => open(ep, self.settings.clone(), &self.config).await?,
            Route::Ssh(_) => {
                let reached = route.reach(ep).await.map_err(Unusable::into_conn_error)?;
                let config = self.config.clone().set_dns_resolver(Pinned(reached.addrs));
                open(ep, self.settings.clone(), &config).await?
            }
        };
        Ok(Connection::new(c, route))
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
    /// A cluster, one node per shard. Holds [`cluster::ClusterTargets::summary`]: fixed
    /// text and counts only, never a server's reply.
    Cluster(String),
}

impl std::fmt::Display for ScanTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Primary => write!(f, "primary"),
            Self::Replica(ep) => write!(f, "replica {ep}"),
            Self::PrimaryFallback(why) => write!(f, "primary (replica unreachable: {why})"),
            Self::Cluster(summary) => write!(f, "{summary}"),
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
/// dot-separated labels of ASCII letters, digits, `-` and `_`, the last one not
/// numeric. That refuses the shorthand address forms a resolver may accept (`127.1`,
/// `0`, `2130706433`, `0x7f000001`): behind an SSH bastion they would reach the bastion
/// itself without being recognised as loopback by name.
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
    // No top-level domain is all digits (RFC 3696 §2), and `0x…` is a hex number.
    let numeric = |l: &str| {
        let hex = l.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("0x"))
            && l[2..].bytes().all(|b| b.is_ascii_hexdigit());
        hex || l.bytes().all(|b| b.is_ascii_digit())
    };
    let last = bare.rsplit('.').next().unwrap_or_default();
    (bare.len() <= 253 && bare.split('.').all(label_ok) && !numeric(last)).then(|| bare.to_string())
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

impl Unusable {
    /// For callers that need a [`ConnError`]: only [`Route::reach`]'s failures
    /// (`Resolve`, `Conn`) are expected here; the others get a fixed text.
    fn into_conn_error(self) -> ConnError {
        match self {
            Self::Conn(e) => e,
            Self::Resolve(e) => ConnError::Redis(redis::RedisError::from(e)),
            Self::NotAReplica => ConnError::Redis(redis::RedisError::from(std::io::Error::other(
                "not a replica of this primary",
            ))),
            Self::WontSendCredentials => ConnError::Redis(redis::RedisError::from(
                std::io::Error::other("credentials may not be sent to this address"),
            )),
        }
    }
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
    route: &Arc<Route>,
    db: u32,
    primary: &Endpoint,
    primary_replid: &str,
    ep: &Endpoint,
) -> Result<Connection, Unusable> {
    let candidate = route.reach(ep).await?;
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
            return Ok(Connection::new(probe, route));
        }
        Ok(_) => return Err(Unusable::NotAReplica),
        Err(e) if auth.sends_credentials && e.code() == Some("NOAUTH") => {}
        Err(e) => return Err(e.into()),
    }
    drop(probe);

    // Both judged the same way: from this machine when direct, from the bastion through
    // SSH, where a forward's local end says nothing about where the server is.
    let primary_local = route.is_local(primary).await;
    if !may_send_credentials(primary_local, candidate.local) {
        return Err(Unusable::WontSendCredentials);
    }
    let mut c = open(ep, auth.settings.clone(), &pinned(auth.config.clone())).await?;
    let info = info_replication(&mut c).await?;
    if is_replica_of(&info, primary_replid) {
        Ok(Connection::new(c, route))
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
) -> Result<(Connection, ScanTarget), ConnError> {
    let route = Route::for_profile(profile, secrets).await?;
    let auth = ResolvedAuth::new(profile, secrets).await?;
    connect_for_scan_with(profile, &route, &auth).await
}

/// [`connect_for_scan`] with the route and credentials already resolved, so a caller
/// (cluster discovery) can reuse them for further connections.
async fn connect_for_scan_with(
    profile: &Profile,
    route: &Arc<Route>,
    auth: &ResolvedAuth,
) -> Result<(Connection, ScanTarget), ConnError> {
    let primary = auth.open(route, &profile.endpoint).await?;
    if !profile.prefer_replica {
        return Ok((primary, ScanTarget::Primary));
    }
    Ok(replica_step(profile, route, auth, primary).await)
}

/// The `prefer_replica` step of [`connect_for_scan`] on an open connection to the
/// profile's own server (`primary`), which it reuses: returned as is when it is already
/// a replica, or as the fallback; otherwise the verified replica's connection.
async fn replica_step(
    profile: &Profile,
    route: &Arc<Route>,
    auth: &ResolvedAuth,
    mut primary: Connection,
) -> (Connection, ScanTarget) {
    let fallback = |c, why: String| (c, ScanTarget::PrimaryFallback(why));
    let info = match info_replication(&mut primary).await {
        Ok(i) => i,
        Err(e) => return fallback(primary, format!("INFO replication: {}", server_message(&e))),
    };
    match parse_replication(&info) {
        Replication::Replica => (primary, ScanTarget::Replica(profile.endpoint.clone())),
        Replication::Unknown => (primary, ScanTarget::Primary),
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
            // Through SSH the candidate's forward is started first, within ssh's own
            // readiness limit, so the 2 s below covers the check, not ssh's login.
            if route.is_ssh()
                && let Err(why) = route.reach(&ep).await
            {
                return fallback(primary, unusable_reason(&ep, why, true));
            }
            let attempt =
                verified_replica(auth, route, profile.db, &profile.endpoint, &replid, &ep);
            match tokio::time::timeout(REPLICA_TIMEOUT, attempt).await {
                Ok(Ok(c)) => (c, ScanTarget::Replica(ep)),
                Ok(Err(why)) => fallback(primary, unusable_reason(&ep, why, route.is_ssh())),
                Err(_) => fallback(primary, format!("{ep}: timed out")),
            }
        }
    }
}

/// The `PrimaryFallback` reason for a replica candidate at `ep` that can't be used.
/// `ssh`: the route goes through a bastion, where loopback is judged.
fn unusable_reason(ep: &Endpoint, why: Unusable, ssh: bool) -> String {
    match why {
        Unusable::NotAReplica => format!("replica at {ep} is not a replica of this primary"),
        Unusable::WontSendCredentials if ssh => format!(
            "replica at {ep} needs credentials to be verified and is a loopback address, as \
             seen from the SSH bastion, that is not the primary's host"
        ),
        Unusable::WontSendCredentials => format!(
            "replica at {ep} needs credentials to be verified and is a loopback address that \
             is not the primary's host"
        ),
        Unusable::Resolve(e) => format!("{ep}: could not resolve the host: {e}"),
        Unusable::Conn(e) => format!("{ep}: {e}"),
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

fn server_message(e: &redis::RedisError) -> String {
    match (e.code(), e.detail()) {
        (Some(c), Some(d)) => format!("{c} {d}"),
        (Some(c), None) => c.to_string(),
        (None, Some(d)) => d.to_string(),
        (None, None) => e.category().to_string(),
    }
}

/// `missing_key`: the probe key does not exist, so a plain `ERR` (such as
/// `ERR no such key`) says the command ran, and so does a cluster redirect (see
/// [`is_redirect`]).
fn classify(r: Result<(), redis::RedisError>, missing_key: bool) -> PermStatus {
    match r {
        Ok(()) => PermStatus::Allowed,
        Err(e) if is_noperm(&e) => PermStatus::Denied(server_message(&e)),
        Err(e) if missing_key && is_redirect(&e) => PermStatus::Allowed,
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

/// `MOVED` or `ASK`: on a cluster node that doesn't own the probe key's slot (an empty
/// shard, say). Redis and Valkey check ACLs, key patterns included, before they
/// redirect, so the reply says the command would have been allowed.
fn is_redirect(e: &redis::RedisError) -> bool {
    matches!(
        e.kind(),
        redis::ErrorKind::Server(ServerErrorKind::Moved | ServerErrorKind::Ask)
    )
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
        Err(e) => (classify(Err(e), false), PROBE_KEY.as_bytes().to_vec(), true),
    };

    // TYPE and PTTL
    let ty: Result<String, _> = redis::cmd("TYPE").arg(&probe_key).query_async(conn).await;
    // A redirect says TYPE is allowed; PTTL still needs its own check.
    let type_ran = match &ty {
        Ok(_) => true,
        Err(e) => missing && is_redirect(e),
    };
    let type_ttl = if type_ran {
        let r: Result<i64, _> = redis::cmd("PTTL").arg(&probe_key).query_async(conn).await;
        classify(r.map(|_| ()), missing)
    } else {
        classify(ty.map(|_| ()), missing)
    };

    // MEMORY USAGE
    let mem: Result<Option<i64>, _> = redis::cmd("MEMORY")
        .arg("USAGE")
        .arg(&probe_key)
        .query_async(conn)
        .await;
    let memory = classify(mem.map(|_| ()), missing);

    // INFO memory
    let info: Result<String, _> = redis::cmd("INFO").arg("memory").query_async(conn).await;
    let (info_status, policy) = match info {
        Ok(text) => (PermStatus::Allowed, Ok(policy_from_info(&text))),
        Err(e) => {
            let status = classify(Err(e), false);
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
            classify(r.map(|_| ()), missing)
        }
    };

    // MEMORY STATS: no key involved, so any error other than NOPERM (an unknown
    // command on a managed service, say) is a denial with the server's reason.
    let stats: Result<redis::Value, _> = redis::cmd("MEMORY").arg("STATS").query_async(conn).await;
    let memory_stats = classify(stats.map(|_| ()), false);

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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Shorthand address forms are refused as announced hosts: behind a bastion, ssh
    /// would read `127.1` as 127.0.0.1 there, while the name check sees a remote host.
    #[test]
    fn announced_hosts_in_shorthand_address_form_are_refused() {
        for host in [
            "127.1",
            "127.0.1",
            "0",
            "2130706433",
            "0x7f000001",
            "0X7F.1",
            "017700000001",
            "10.1",
            "host.123",
            "a.0x",
            "cache.0xdb",
        ] {
            assert_eq!(replica_host(host), None, "{host}");
        }
        for (host, want) in [
            ("127.0.0.1", "127.0.0.1"),
            ("[::1]", "::1"),
            ("10.0.0.5", "10.0.0.5"),
            ("redis-1.internal", "redis-1.internal"),
            ("LOCALHOST", "LOCALHOST"),
            ("cache.db0x", "cache.db0x"),
            ("123.example", "123.example"),
            ("node-0", "node-0"),
        ] {
            assert_eq!(replica_host(host).as_deref(), Some(want), "{host}");
        }
        for host in ["0.0.0.0", "::", "localhost.", "bad host", ""] {
            assert_eq!(replica_host(host), None, "{host:?}");
        }
    }

    /// Loopback by name ignores case and recognises IPv4-mapped loopback.
    #[test]
    fn loopback_by_name_ignores_case_and_sees_mapped_addresses() {
        for host in [
            "LOCALHOST",
            "LocalHost",
            "DB.LOCALHOST",
            "localhost.",
            "::ffff:127.0.0.1",
            "::FFFF:127.0.0.9",
            "0.0.0.0",
            "::",
        ] {
            assert!(is_loopback_name(host), "{host}");
        }
        for host in ["::ffff:10.0.0.5", "localhost.example", "notlocalhost"] {
            assert!(!is_loopback_name(host), "{host}");
        }
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

    fn ssh_route(host: &str) -> Route {
        let t = ssh::tests::tunnel(host);
        let p = ssh::tests::profile(Some(t.clone()));
        Route::Ssh(ssh::Bastion::new(&p, &t, &crate::secrets::MemoryStore::default()).unwrap())
    }

    #[tokio::test]
    async fn a_profile_with_a_bastion_gets_an_ssh_route() {
        let store = crate::secrets::MemoryStore::default();
        let direct = ssh::tests::profile(None);
        let r = Route::for_profile(&direct, &store).await.unwrap();
        assert!(matches!(r.as_ref(), Route::Direct));
        let behind = ssh::tests::profile(Some(ssh::tests::tunnel("b.example")));
        let r = Route::for_profile(&behind, &store).await.unwrap();
        assert!(r.is_ssh());
        // A bad bastion fails before anything is started or read.
        let bad = ssh::tests::profile(Some(ssh::tests::tunnel("-oProxyCommand=x")));
        let Err(e) = Route::for_profile(&bad, &store).await else {
            panic!("expected an error");
        };
        assert!(
            matches!(e, ConnError::Ssh(ssh::SshError::InvalidHost)),
            "{e:?}"
        );
        let Err(e) = connect(&bad, &store).await else {
            panic!("expected an error");
        };
        assert!(
            matches!(e, ConnError::Ssh(ssh::SshError::InvalidHost)),
            "{e:?}"
        );
    }

    /// Behind a bastion, "on this machine" means on the bastion: judged from the host
    /// name, never from the forward's 127.0.0.1.
    #[tokio::test]
    async fn behind_a_bastion_loopback_is_judged_from_the_bastion() {
        let route = ssh_route("b.example");
        for host in ["127.0.0.1", "localhost", "::1", "db.localhost"] {
            assert!(route.is_local(&ep(host, 6379, false)).await, "{host}");
        }
        for host in ["10.0.0.5", "cache.internal", "2001:db8::5"] {
            assert!(!route.is_local(&ep(host, 6379, false)).await, "{host}");
        }
        // Direct, the same names are judged here.
        assert!(Route::Direct.is_local(&ep("127.0.0.1", 6379, false)).await);
        assert!(!Route::Direct.is_local(&ep("10.0.0.5", 6379, false)).await);
        assert!(
            !Route::Direct
                .is_local(&ep("nope.invalid", 6379, false))
                .await
        );
    }

    /// The trust rule behind a bastion: a primary at 10.0.0.5 that announces
    /// 127.0.0.1 names the bastion itself, so the candidate gets no credentials; a
    /// profile whose host is loopback on the bastion may send them to another loopback
    /// address there.
    #[tokio::test]
    async fn behind_a_bastion_an_announced_loopback_gets_no_credentials() {
        let route = ssh_route("b.example");
        let candidate = route.is_local(&ep("127.0.0.1", 6380, false)).await;
        let remote_primary = route.is_local(&ep("10.0.0.5", 6379, false)).await;
        assert!(!may_send_credentials(remote_primary, candidate));
        let primary_on_bastion = route.is_local(&ep("localhost", 6379, false)).await;
        assert!(may_send_credentials(primary_on_bastion, candidate));
        // A remote candidate is fine either way.
        let remote = route.is_local(&ep("10.0.0.6", 6379, false)).await;
        assert!(may_send_credentials(remote_primary, remote));
        // Direct, a profile on 127.0.0.1 (a tunnel the user opened by hand) counts as on
        // this machine: why `--ssh` is the way to reach a server through a bastion.
        let own_tunnel = Route::Direct.is_local(&ep("127.0.0.1", 16379, false)).await;
        assert!(may_send_credentials(own_tunnel, candidate));
    }

    #[test]
    fn fallback_reasons_say_where_loopback_was_judged() {
        let e = ep("127.0.0.1", 6380, false);
        let ssh = unusable_reason(&e, Unusable::WontSendCredentials, true);
        assert!(ssh.contains("as seen from the SSH bastion"), "{ssh}");
        let direct = unusable_reason(&e, Unusable::WontSendCredentials, false);
        assert!(!direct.contains("bastion"), "{direct}");
        assert_eq!(
            unusable_reason(&e, Unusable::NotAReplica, true),
            "replica at 127.0.0.1:6380 is not a replica of this primary"
        );
        let tunnel = unusable_reason(&e, ssh::SshError::Auth.into(), true);
        assert!(
            tunnel.starts_with("127.0.0.1:6380: the SSH bastion refused"),
            "{tunnel}"
        );
    }

    /// Every connection a profile opens goes through its route: the primary
    /// ([`ResolvedAuth::open`], so `connect`), the scan connect, and the replica check.
    /// The fake ssh accepts and closes, so each attempt fails, but at the forward.
    #[cfg(unix)]
    #[tokio::test]
    async fn connections_go_through_the_route() {
        use crate::secrets::MemoryStore;
        let fake = ssh::tests::fake::Fake::new("route");
        let route = Arc::new(Route::Ssh(fake.bastion("listen", None, ssh::READY_TIMEOUT)));
        // A name that only the bastion could resolve.
        let mut p = ssh::tests::profile(None);
        p.endpoint = ep("redis.invalid", 6379, false);
        let auth = ResolvedAuth::new(&p, &MemoryStore::default())
            .await
            .unwrap();

        let e = auth.open(&route, &p.endpoint).await.unwrap_err();
        assert!(!matches!(e, ConnError::Ssh(_)), "{e:?}");
        let pids = fake.starts();
        assert_eq!(pids.len(), 1);
        assert!(
            fake.argv(pids[0])
                .iter()
                .any(|a| a.ends_with(":redis.invalid:6379")),
            "{:?}",
            fake.argv(pids[0])
        );
        // The readiness probe, then the connection itself.
        assert!(fake.accepts(pids[0]) >= 2);

        let before = fake.accepts(pids[0]);
        assert!(connect_for_scan_with(&p, &route, &auth).await.is_err());
        assert_eq!(fake.starts().len(), 1, "the forward is reused");
        assert!(fake.accepts(pids[0]) > before);

        let candidate = ep("replica.invalid", 6380, false);
        let r = verified_replica(&auth, &route, 0, &p.endpoint, "id", &candidate).await;
        assert!(matches!(r, Err(Unusable::Conn(ConnError::Redis(_)))));
        let pids = fake.starts();
        assert_eq!(pids.len(), 2);
        assert!(
            fake.argv(pids[1])
                .iter()
                .any(|a| a.ends_with(":replica.invalid:6380"))
        );
        assert!(fake.accepts(pids[1]) >= 2);
    }

    /// A minimal RESP server for tests: answers `INFO <section>` from `info` (an empty
    /// text for a section it doesn't have), a command `X` with the raw RESP in an
    /// `info` entry named `reply:X` when there is one, and `+OK` to anything else, and
    /// records every connection and command.
    struct FakeServer {
        addr: SocketAddr,
        connections: Arc<std::sync::atomic::AtomicUsize>,
        commands: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeServer {
        async fn start(info: &[(&str, String)]) -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
            let info: Arc<Vec<(String, String)>> = Arc::new(
                info.iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                    .collect(),
            );
            let (conns, cmds) = (Arc::clone(&connections), Arc::clone(&commands));
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let (cmds, info) = (Arc::clone(&cmds), Arc::clone(&info));
                    tokio::spawn(async move {
                        let mut s = tokio::io::BufReader::new(stream);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            if s.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let Some(n) = line.trim().strip_prefix('*') else {
                                return;
                            };
                            let mut args = Vec::new();
                            for _ in 0..n.parse::<usize>().unwrap_or(0) {
                                line.clear();
                                s.read_line(&mut line).await.unwrap();
                                let len: usize = line.trim()[1..].parse().unwrap();
                                let mut buf = vec![0; len + 2];
                                s.read_exact(&mut buf).await.unwrap();
                                args.push(String::from_utf8_lossy(&buf[..len]).into_owned());
                            }
                            cmds.lock().unwrap().push(args.join(" "));
                            let reply = if args[0].eq_ignore_ascii_case("INFO") {
                                let section = args.get(1).map(|a| a.to_ascii_lowercase());
                                let text = info
                                    .iter()
                                    .find(|(k, _)| Some(k) == section.as_ref())
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or_default();
                                format!("${}\r\n{text}\r\n", text.len())
                            } else {
                                let key = format!("reply:{}", args[0].to_ascii_lowercase());
                                info.iter()
                                    .find(|(k, _)| *k == key)
                                    .map_or_else(|| "+OK\r\n".to_string(), |(_, v)| v.clone())
                            };
                            if s.get_mut().write_all(reply.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            Self {
                addr,
                connections,
                commands,
            }
        }

        fn connections(&self) -> usize {
            self.connections.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn count(&self, cmd: &str) -> usize {
            let cmds = self.commands.lock().unwrap();
            cmds.iter().filter(|c| c.eq_ignore_ascii_case(cmd)).count()
        }
    }

    const STANDALONE: &str = "# Server\r\nredis_version:7.4.0\r\nredis_mode:standalone\r\n";

    /// A primary at a fake server announcing one online replica at `replica_port`, and
    /// that replica.
    async fn fake_pair() -> (FakeServer, FakeServer) {
        let replica = FakeServer::start(&[
            ("server", STANDALONE.to_string()),
            (
                "replication",
                "role:slave\r\nmaster_host:127.0.0.1\r\nmaster_replid:abc123\r\n".into(),
            ),
        ])
        .await;
        let primary = FakeServer::start(&[
            ("server", STANDALONE.to_string()),
            (
                "replication",
                format!(
                    "role:master\r\nslave0:ip=127.0.0.1,port={},state=online,offset=1,lag=0\r\nmaster_replid:abc123\r\n",
                    replica.addr.port()
                ),
            ),
        ])
        .await;
        (primary, replica)
    }

    fn fake_profile(port: u16, prefer_replica: bool) -> Profile {
        let mut p = ssh::tests::profile(None);
        p.endpoint = ep("127.0.0.1", port, false);
        p.prefer_replica = prefer_replica;
        p
    }

    #[test]
    fn a_kept_info_server_reply_answers_one_info_server_while_fresh() {
        use std::time::Instant;
        let now = Instant::now();
        let reply = redis::Value::BulkString(b"redis_version:7.4.0".to_vec());
        let info = |section: &str| redis::cmd("INFO").arg(section).clone();
        let mut kept = Some((now, reply.clone()));
        // Other commands pass through and leave it.
        assert_eq!(take_kept_info(&mut kept, &info("memory"), now), None);
        assert_eq!(take_kept_info(&mut kept, &redis::cmd("INFO"), now), None);
        assert_eq!(
            take_kept_info(&mut kept, redis::cmd("INFO").arg("server").arg("x"), now),
            None
        );
        assert_eq!(take_kept_info(&mut kept, &redis::cmd("PING"), now), None);
        assert!(kept.is_some());
        // One INFO server, in any case, gets it; the next goes to the server.
        let later = now + INFO_SERVER_REUSE;
        assert_eq!(
            take_kept_info(&mut kept, redis::cmd("info").arg("SERVER"), later),
            Some(reply.clone())
        );
        assert_eq!(take_kept_info(&mut kept, &info("server"), later), None);
        // Too old: dropped, and the command goes to the server.
        let mut stale = Some((now, reply));
        let too_late = now + INFO_SERVER_REUSE + Duration::from_millis(1);
        assert_eq!(take_kept_info(&mut stale, &info("server"), too_late), None);
        assert!(stale.is_none());
    }

    /// Through a real connection: the kept reply is answered without reaching the
    /// server, once, and only for `INFO server`.
    #[tokio::test]
    async fn a_connection_answers_the_kept_info_server_without_sending_it() {
        let server = FakeServer::start(&[("server", STANDALONE.to_string())]).await;
        let p = fake_profile(server.addr.port(), false);
        let mut c = connect(&p, &crate::secrets::MemoryStore::default())
            .await
            .unwrap();
        c.keep_info_server(redis::Value::BulkString(b"kept".to_vec()));
        let clone = c.clone();
        let first: String = redis::cmd("INFO")
            .arg("memory")
            .query_async(&mut c)
            .await
            .unwrap();
        assert_eq!(first, "");
        let kept: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut c)
            .await
            .unwrap();
        assert_eq!(kept, "kept");
        assert_eq!(server.count("INFO server"), 0);
        // Taken: a clone shares it, so it is gone there too.
        let mut clone = clone;
        let fresh: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut clone)
            .await
            .unwrap();
        assert_eq!(fresh, STANDALONE);
        assert_eq!(server.count("INFO server"), 1);
    }

    /// A standalone `prefer_replica` scan through `connect_targets` connects to the
    /// primary once (the replica step reuses the seed), and the `INFO server` it reads
    /// to tell a cluster apart is the scan's own.
    #[tokio::test]
    async fn connect_targets_opens_a_standalone_primary_once() {
        use crate::secrets::MemoryStore;
        let (primary, replica) = fake_pair().await;
        let p = fake_profile(primary.addr.port(), true);
        let targets = cluster::connect_targets(&p, &MemoryStore::default())
            .await
            .unwrap();
        let cluster::ScanTargets::Single { target, .. } = &targets else {
            panic!("a standalone server was taken for a cluster");
        };
        assert_eq!(
            target,
            &ScanTarget::Replica(ep("127.0.0.1", replica.addr.port(), false))
        );
        assert_eq!(primary.connections(), 1, "{:?}", primary.commands);
        assert_eq!(primary.count("INFO server"), 1);
        assert_eq!(primary.count("INFO replication"), 1);

        // Scanned on the seed itself: its first INFO server is answered from detection.
        let (primary, _replica) = fake_pair().await;
        let p = fake_profile(primary.addr.port(), false);
        let mut targets = cluster::connect_targets(&p, &MemoryStore::default())
            .await
            .unwrap();
        let cluster::ScanTargets::Single { conn, target } = &mut targets else {
            panic!("a standalone server was taken for a cluster");
        };
        assert_eq!(target, &ScanTarget::Primary);
        let info: String = redis::cmd("INFO")
            .arg("server")
            .query_async(conn)
            .await
            .unwrap();
        assert_eq!(info, STANDALONE);
        assert_eq!(primary.connections(), 1);
        assert_eq!(
            primary.count("INFO server"),
            1,
            "detection and the scan's, once"
        );
    }

    /// Behind SSH, the replica's forward is started before the 2 s replica timeout:
    /// with a bastion that takes longer than that to log in, the replica is still used.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_slow_replica_forward_is_started_before_the_replica_timeout() {
        use crate::secrets::MemoryStore;
        // A fake ssh that waits `delay` before it listens, then proxies every
        // connection to the `-L` target.
        const SLOW_PROXY: &str = r#"
use strict; use warnings; use IO::Socket::INET; use IO::Select;
my $delay = shift @ARGV;
my ($spec) = map { $ARGV[$_ + 1] } grep { $ARGV[$_] eq '-L' } 0 .. $#ARGV;
my (undef, $port, $th, $tp) = split /:/, $spec;
select(undef, undef, undef, $delay);
my $l = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => $port, Listen => 16, ReuseAddr => 1) or exit 255;
$SIG{CHLD} = 'IGNORE';
while (my $c = $l->accept) {
  my $pid = fork();
  if (defined $pid && $pid == 0) {
    my $up = IO::Socket::INET->new(PeerAddr => $th, PeerPort => $tp) or exit 0;
    my $sel = IO::Select->new($c, $up);
    while (my @r = $sel->can_read) {
      for my $fh (@r) {
        my $n = sysread($fh, my $buf, 65536);
        exit 0 unless $n;
        syswrite($fh == $c ? $up : $c, $buf) or exit 0;
      }
    }
    exit 0;
  }
  close $c;
}
"#;
        let dir = std::env::temp_dir().join(format!(
            "redis-lens-slow-ssh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("slow-ssh.pl");
        std::fs::write(&script, SLOW_PROXY).unwrap();
        let delay = REPLICA_TIMEOUT + Duration::from_millis(500);

        let (primary, replica) = fake_pair().await;
        let p = fake_profile(primary.addr.port(), true);
        let t = ssh::tests::tunnel("b.example");
        let bastion = ssh::Bastion::new(
            &ssh::tests::profile(Some(t.clone())),
            &t,
            &MemoryStore::default(),
        )
        .unwrap()
        .with_launcher(
            vec![
                "perl".into(),
                script.clone().into(),
                format!("{}", delay.as_secs_f64()).into(),
            ],
            ssh::READY_TIMEOUT,
        );
        let route = Arc::new(Route::Ssh(bastion));
        let auth = ResolvedAuth::new(&p, &MemoryStore::default())
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let (c, target) = connect_for_scan_with(&p, &route, &auth).await.unwrap();
        assert_eq!(
            target,
            ScanTarget::Replica(ep("127.0.0.1", replica.addr.port(), false)),
            "the replica's forward was cut short by the replica timeout"
        );
        // Both forwards really took longer than the replica timeout to start.
        assert!(started.elapsed() >= delay * 2);
        assert!(replica.count("INFO replication") >= 1);
        drop(c);
        drop(route);
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(
            ScanTarget::Cluster("cluster · 3 shards".into()).to_string(),
            "cluster · 3 shards"
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
            ssh: None,
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
        assert!(matches!(classify(Err(noperm), true), PermStatus::Denied(_)));
        let nokey = redis::RedisError::from((
            redis::ErrorKind::Server(ServerErrorKind::ResponseError),
            "ERR",
            "no such key".to_string(),
        ));
        assert_eq!(classify(Err(nokey), true), PermStatus::Allowed);
        assert_eq!(classify(Ok(()), false), PermStatus::Allowed);
    }

    /// An empty shard that doesn't own the probe key's slot answers `MOVED` to every
    /// key command. That is not a denial: the ACL check comes before the redirect.
    #[test]
    fn a_redirect_on_the_missing_probe_key_is_allowed() {
        let moved = || {
            redis::RedisError::from((
                redis::ErrorKind::Server(ServerErrorKind::Moved),
                "MOVED",
                "2115 127.0.0.1:7000".to_string(),
            ))
        };
        let ask = redis::RedisError::from((
            redis::ErrorKind::Server(ServerErrorKind::Ask),
            "ASK",
            "2115 127.0.0.1:7000".to_string(),
        ));
        assert_eq!(classify(Err(moved()), true), PermStatus::Allowed);
        assert_eq!(classify(Err(ask), true), PermStatus::Allowed);
        // On a real key (one SCAN returned), a redirect proves nothing.
        assert!(matches!(
            classify(Err(moved()), false),
            PermStatus::Denied(_)
        ));
    }

    #[tokio::test]
    async fn an_empty_shard_that_redirects_the_probe_key_allows_sizes() {
        let moved = "-MOVED 2115 127.0.0.1:7000\r\n".to_string();
        let server = FakeServer::start(&[
            ("reply:scan", "*2\r\n$1\r\n0\r\n*0\r\n".into()),
            ("reply:type", moved.clone()),
            ("reply:pttl", moved.clone()),
            ("reply:memory", moved),
        ])
        .await;
        let client = redis::Client::open(format!("redis://{}", server.addr)).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let report = check_permissions(&mut conn).await;
        assert!(report.allowed(PERM_TYPE_TTL), "{report:?}");
        assert!(report.allowed(PERM_MEMORY), "{report:?}");
        // TYPE redirected, so PTTL was still checked on its own.
        assert_eq!(server.count("PTTL redis-lens:probe"), 1);
    }

    /// TYPE redirects but PTTL is denied: the pair is denied.
    #[tokio::test]
    async fn a_redirected_type_does_not_vouch_for_pttl() {
        let server = FakeServer::start(&[
            ("reply:scan", "*2\r\n$1\r\n0\r\n*0\r\n".into()),
            ("reply:type", "-MOVED 2115 127.0.0.1:7000\r\n".into()),
            (
                "reply:pttl",
                "-NOPERM this user has no permissions to run the 'pttl' command\r\n".into(),
            ),
        ])
        .await;
        let client = redis::Client::open(format!("redis://{}", server.addr)).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let report = check_permissions(&mut conn).await;
        assert!(!report.allowed(PERM_TYPE_TTL), "{report:?}");
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
