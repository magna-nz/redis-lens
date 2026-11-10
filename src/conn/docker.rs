//! Finding Redis and Valkey containers in Docker.
//!
//! Owner: connections. Contract only; bodies are stubs.

use crate::model::{Credentials, Profile};

use super::ConnError;

#[derive(Debug, Clone)]
pub struct DiscoveredInstance {
    pub container: String,
    pub image: String,
    /// `env` is `Local`, `auth` is `Keychain` when a password was found, else `None`.
    pub profile: Profile,
    /// A password read from the container's command line (`--requirepass`) or
    /// environment (`REDIS_PASSWORD`). Kept in memory only.
    pub credentials: Option<Credentials>,
}

/// Running containers whose image is redis, valkey or redis-stack and that publish
/// container port 6379 on the host.
pub async fn discover() -> Result<Vec<DiscoveredInstance>, ConnError> {
    todo!("discover")
}
