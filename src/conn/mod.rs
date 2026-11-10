//! Connecting to servers: connection strings, auth, permission checks, Docker discovery.
//!
//! Owner: connections.

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
    connect_endpoint(profile, &profile.endpoint, secrets).await
}

/// [`connect`] to `ep` with the profile's credentials, TLS setting and database.
async fn connect_endpoint(
    profile: &Profile,
    ep: &Endpoint,
    secrets: &dyn SecretStore,
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
    let mut settings = RedisConnectionInfo::default().set_db(i64::from(profile.db));
    let mut config = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(CONNECT_TIMEOUT))
        .set_response_timeout(Some(RESPONSE_TIMEOUT));

    match profile.auth {
        Auth::None => {}
        Auth::Keychain => {
            let creds = secrets
                .get(&profile.secret_ref())?
                .ok_or_else(|| ConnError::MissingCredentials(profile.name.clone()))?;
            if let Some(user) = &creds.username {
                settings = settings.set_username(user);
            }
            let pw = creds.password.expose_secret();
            if !pw.is_empty() {
                settings = settings.set_password(pw);
            }
        }
        Auth::EntraId => config = entra_config(config)?,
    }

    let info = addr.into_connection_info()?.set_redis_settings(settings);
    let client = redis::Client::open(info)?;
    Ok(client
        .get_multiplexed_async_connection_with_config(&config)
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
    /// `role:slave`.
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
                && !ip.is_empty()
            {
                online.push((ip.to_string(), port));
            }
        }
    }
    match role.as_deref() {
        Some("master") => Replication::Master { online },
        Some("slave") => Replication::Replica,
        _ => Replication::Unknown,
    }
}

/// Connect for a scan. Connects to the primary as [`connect`] does; with
/// `profile.prefer_replica` it then looks for an online replica and connects to the
/// first one with the same credentials and TLS (2 s timeout). A replica that can't be
/// used leaves the scan on the primary as `PrimaryFallback`. A profile that already
/// points at a replica is scanned as is.
pub async fn connect_for_scan(
    profile: &Profile,
    secrets: &dyn SecretStore,
) -> Result<(MultiplexedConnection, ScanTarget), ConnError> {
    let mut primary = connect(profile, secrets).await?;
    if !profile.prefer_replica {
        return Ok((primary, ScanTarget::Primary));
    }
    let fallback = |c, why: String| Ok((c, ScanTarget::PrimaryFallback(why)));
    let info: Result<String, _> = redis::cmd("INFO")
        .arg("replication")
        .query_async(&mut primary)
        .await;
    let info = match info {
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
            let ep = Endpoint {
                host,
                port,
                tls: profile.endpoint.tls,
            };
            match tokio::time::timeout(REPLICA_TIMEOUT, connect_endpoint(profile, &ep, secrets))
                .await
            {
                Ok(Ok(c)) => Ok((c, ScanTarget::Replica(ep))),
                Ok(Err(e)) => fallback(primary, format!("{ep}: {e}")),
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
