//! Connecting to servers: connection strings, auth, permission checks, Docker discovery.
//!
//! Owner: connections. Contract only; bodies are stubs.

use redis::aio::{ConnectionLike, MultiplexedConnection};

use crate::model::{PermissionReport, Profile};
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

/// Open a connection for a profile, reading credentials from `secrets` when
/// `profile.auth` is `Auth::Keychain`. Selects `profile.db`.
pub async fn connect(
    _profile: &Profile,
    _secrets: &dyn SecretStore,
) -> Result<MultiplexedConnection, ConnError> {
    todo!("connect")
}

/// Try each command the views need once and report which are allowed.
///
/// Never fails as a whole: an error on one command becomes a `Denied` entry. Checks
/// `OBJECT FREQ` only when the eviction policy is LFU, otherwise reports `Skipped`.
pub async fn check_permissions<C: ConnectionLike + Send>(_conn: &mut C) -> PermissionReport {
    todo!("check_permissions")
}
