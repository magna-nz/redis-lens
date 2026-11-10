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
    Auth, PERM_FREQ, PERM_INFO, PERM_LABELS, PERM_MEMORY, PERM_SCAN, PERM_TYPE_TTL, PermCheck,
    PermStatus, PermissionReport, Profile,
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
    let ep = &profile.endpoint;
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
