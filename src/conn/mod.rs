//! Connecting to servers: connection strings, auth, permission checks, Docker discovery.
//!
//! Owner: connections.

use std::net::IpAddr;
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

async fn open(
    ep: &Endpoint,
    settings: RedisConnectionInfo,
    config: &AsyncConnectionConfig,
) -> Result<MultiplexedConnection, ConnError> {
    let addr = if ep.tls {
        ConnectionAddr::TcpTls {
            host: ep.host.clone(),
            port: ep.port,
            insecure: false,
            tls_params: None,
        }
    } else {
        ConnectionAddr::Tcp(ep.host.clone(), ep.port)
    };
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

/// Loopback: `localhost` (and `*.localhost`), `127.0.0.0/8`, `::1`, and `::ffff:127.x`.
fn is_loopback(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h.parse::<IpAddr>()
            .is_ok_and(|a| a.to_canonical().is_loopback())
}

/// Whether credentials may go to a replica candidate that could not be checked without
/// them. Only when it is on the primary's host (any two loopback names count as the same
/// host), or it is not a loopback address: a loopback address announced by a remote
/// primary names this machine, not the primary's, so whatever listens there is not the
/// replica.
fn may_send_credentials(primary_host: &str, candidate_host: &str) -> bool {
    let same_ip = matches!(
        (primary_host.parse::<IpAddr>(), candidate_host.parse::<IpAddr>()),
        (Ok(a), Ok(b)) if a.to_canonical() == b.to_canonical()
    );
    primary_host.eq_ignore_ascii_case(candidate_host)
        || same_ip
        || (is_loopback(primary_host) && is_loopback(candidate_host))
        || !is_loopback(candidate_host)
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
/// When the profile has credentials, the candidate is first asked for `INFO replication`
/// on a connection that sends none (no `AUTH`, no `HELLO`, no `CLIENT SETINFO`). If it
/// answers, it is checked there and credentials go only to a verified replica. If it
/// wants authentication, credentials are sent only when [`may_send_credentials`]
/// allows, and the candidate is checked again on the authenticated connection, which
/// is the one returned.
async fn verified_replica(
    auth: &ResolvedAuth,
    primary_host: &str,
    primary_replid: &str,
    ep: &Endpoint,
) -> Result<MultiplexedConnection, Unusable> {
    if auth.sends_credentials {
        let settings = RedisConnectionInfo::default().set_skip_set_lib_name();
        let mut probe = open(ep, settings, &base_config()).await?;
        match info_replication(&mut probe).await {
            Ok(info) if is_replica_of(&info, primary_replid) => {}
            Ok(_) => return Err(Unusable::NotAReplica),
            // A server reply such as NOAUTH: it can only be checked with credentials.
            Err(e) if e.code().is_some() || is_server_error(&e) => {
                if !may_send_credentials(primary_host, &ep.host) {
                    return Err(Unusable::WontSendCredentials);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    let mut c = auth.open(ep).await?;
    let info = info_replication(&mut c).await?;
    if is_replica_of(&info, primary_replid) {
        Ok(c)
    } else {
        Err(Unusable::NotAReplica)
    }
}

fn is_server_error(e: &redis::RedisError) -> bool {
    matches!(
        e.kind(),
        redis::ErrorKind::Server(_)
            | redis::ErrorKind::AuthenticationFailed
            | redis::ErrorKind::Extension
    )
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
            let attempt = verified_replica(&auth, &profile.endpoint.host, &replid, &ep);
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
    fn credentials_go_to_the_primary_host_or_a_non_loopback_address() {
        // Same host as the primary.
        assert!(may_send_credentials("127.0.0.1", "127.0.0.1"));
        assert!(may_send_credentials("cache.internal", "CACHE.internal"));
        assert!(may_send_credentials("::1", "0:0:0:0:0:0:0:1"));
        // Both loopback: the same machine.
        assert!(may_send_credentials("localhost", "127.0.0.1"));
        assert!(may_send_credentials("127.0.0.1", "::1"));
        // Non-loopback addresses the primary announced.
        assert!(may_send_credentials("cache.internal", "10.0.1.21"));
        assert!(may_send_credentials("127.0.0.1", "10.0.1.21"));
        assert!(may_send_credentials("10.0.1.20", "replica-1.internal"));
        assert!(may_send_credentials("10.0.1.20", "2001:db8::5"));
        // A loopback address announced by a remote primary names this machine.
        assert!(!may_send_credentials("cache.internal", "127.0.0.1"));
        assert!(!may_send_credentials("10.0.1.20", "127.0.0.2"));
        assert!(!may_send_credentials("10.0.1.20", "::1"));
        assert!(!may_send_credentials("10.0.1.20", "::ffff:127.0.0.1"));
        assert!(!may_send_credentials("10.0.1.20", "localhost"));
        assert!(!may_send_credentials("10.0.1.20", "evil.localhost"));
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
}
