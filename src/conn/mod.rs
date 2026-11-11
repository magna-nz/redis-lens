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
    Auth, Endpoint, PERM_FREQ, PERM_INFO, PERM_LABELS, PERM_MEMORY, PERM_SCAN, PERM_TYPE_TTL,
    PermCheck, PermStatus, PermissionReport, Profile,
};
use crate::secrets::{SecretError, SecretStore};

pub mod docker;
pub mod parse;
mod tls;

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
    #[error(transparent)]
    Redis(#[from] redis::RedisError),
    #[error("Docker: {0}")]
    Docker(String),
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_KEY: &str = "redis-lens:probe";

/// Open a connection for a profile, reading credentials from `secrets` when
/// `profile.auth` is `Auth::Keychain`. Selects `profile.db`.
///
/// Errors never contain the password: the server's `WRONGPASS` message does not echo it
/// and `RedisConnectionInfo`'s `Debug` redacts it.
pub async fn connect(
    profile: &Profile,
    secrets: &dyn SecretStore,
) -> Result<MultiplexedConnection, ConnError> {
    let auth = ResolvedAuth::new(profile, secrets)?;
    auth.open(&profile.endpoint).await
}

/// A profile's credentials and connection settings, resolved once: one keychain read,
/// or one Entra ID token provider, shared by every connection opened with it.
struct ResolvedAuth {
    /// Username, password and database.
    settings: RedisConnectionInfo,
    /// Timeouts, plus the Entra ID credentials provider when there is one.
    config: AsyncConnectionConfig,
    /// Whether connecting sends a username, password or token.
    sends_credentials: bool,
}

impl ResolvedAuth {
    fn new(profile: &Profile, secrets: &dyn SecretStore) -> Result<Self, ConnError> {
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
    let auth = ResolvedAuth::new(profile, secrets)?;
    let mut primary = auth.open(&profile.endpoint).await?;
    if !profile.prefer_replica {
        return Ok((primary, ScanTarget::Primary));
    }
    let fallback = |c, why: String| Ok((c, ScanTarget::PrimaryFallback(why)));
    let info = match info_replication(&mut primary).await {
        Ok(i) => i,
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

fn server_message(e: &redis::RedisError) -> String {
    match (e.code(), e.detail()) {
        (Some(c), Some(d)) => format!("{c} {d}"),
        (Some(c), None) => c.to_string(),
        (None, Some(d)) => d.to_string(),
        (None, None) => e.category().to_string(),
    }
}

/// `missing_key`: the probe key does not exist, so a plain `ERR` (such as
/// `ERR no such key`) says the command ran.
fn classify(r: Result<(), redis::RedisError>, missing_key: bool) -> PermStatus {
    match r {
        Ok(()) => PermStatus::Allowed,
        Err(e) if is_noperm(&e) => PermStatus::Denied(server_message(&e)),
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
        Err(e) => (classify(Err(e), false), PROBE_KEY.as_bytes().to_vec(), true),
    };

    // TYPE and PTTL
    let ty: Result<String, _> = redis::cmd("TYPE").arg(&probe_key).query_async(conn).await;
    let type_ttl = match ty {
        Ok(_) => {
            let r: Result<i64, _> = redis::cmd("PTTL").arg(&probe_key).query_async(conn).await;
            classify(r.map(|_| ()), missing)
        }
        Err(e) => classify(Err(e), missing),
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

    let mut statuses = [
        (PERM_SCAN, scan_status),
        (PERM_TYPE_TTL, type_ttl),
        (PERM_MEMORY, memory),
        (PERM_FREQ, freq),
        (PERM_INFO, info_status),
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
