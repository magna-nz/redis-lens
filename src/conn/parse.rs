//! Parsing pasted connection strings.
//!
//! Owner: connections. Contract only; bodies are stubs.

use crate::model::{Credentials, Endpoint, Env};

use super::ConnError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnFormat {
    /// `redis://` or `rediss://`.
    RedisUri,
    /// `host:port,password=...,ssl=True,...`
    StackExchange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AzureKind {
    /// `*.redis.cache.windows.net`
    CacheForRedis,
    /// `*.redis.azure.net`
    ManagedRedis,
}

#[derive(Debug, Clone)]
pub struct ParsedConnection {
    pub format: ConnFormat,
    pub endpoint: Endpoint,
    pub db: u32,
    /// Username and password found in the string, if any. Goes to the keychain on save.
    pub credentials: Option<Credentials>,
    pub azure: Option<AzureKind>,
    /// Guessed from the host: localhost is `Local`, "prod" in the host is `Prod`, and so on.
    pub env_guess: Env,
    /// First label of the host, or `local-<port>` for localhost.
    pub name_guess: String,
}

/// Parse a `redis://` / `rediss://` URI or a StackExchange.Redis connection string.
pub fn parse_connection_string(_s: &str) -> Result<ParsedConnection, ConnError> {
    todo!("parse_connection_string")
}

/// The input with any password replaced by `******`, safe to display or log.
pub fn mask(_s: &str) -> String {
    todo!("mask")
}
