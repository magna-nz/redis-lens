//! Credentials in the OS keychain.
//!
//! Owner: connections. Contract only; bodies are stubs.
//!
//! Each profile has one keychain entry (see [`crate::model::Profile::secret_ref`])
//! holding both the username and the password. Nothing else in this crate may write
//! credentials anywhere.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::model::{Credentials, SecretRef};

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("the OS keychain is not available: {0}")]
    Unavailable(String),
    #[error("keychain error: {0}")]
    Store(String),
    #[error("the keychain entry is not in the expected format")]
    Corrupt,
}

/// Somewhere credentials can be kept.
pub trait SecretStore: Send + Sync {
    /// `Ok(None)` when there is no entry.
    fn get(&self, r: &SecretRef) -> Result<Option<Credentials>, SecretError>;
    fn set(&self, r: &SecretRef, creds: &Credentials) -> Result<(), SecretError>;
    /// Deleting an entry that does not exist is not an error.
    fn delete(&self, r: &SecretRef) -> Result<(), SecretError>;
}

/// The real store: macOS Keychain, Windows Credential Manager, or the Secret Service on Linux.
#[derive(Debug, Default)]
pub struct KeyringStore;

impl SecretStore for KeyringStore {
    fn get(&self, _r: &SecretRef) -> Result<Option<Credentials>, SecretError> {
        todo!("KeyringStore::get")
    }
    fn set(&self, _r: &SecretRef, _creds: &Credentials) -> Result<(), SecretError> {
        todo!("KeyringStore::set")
    }
    fn delete(&self, _r: &SecretRef) -> Result<(), SecretError> {
        todo!("KeyringStore::delete")
    }
}

/// In-memory store for tests and for credentials read from Docker containers.
#[derive(Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<SecretRef, Credentials>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, _r: &SecretRef) -> Result<Option<Credentials>, SecretError> {
        let _ = &self.entries;
        todo!("MemoryStore::get")
    }
    fn set(&self, _r: &SecretRef, _creds: &Credentials) -> Result<(), SecretError> {
        todo!("MemoryStore::set")
    }
    fn delete(&self, _r: &SecretRef) -> Result<(), SecretError> {
        todo!("MemoryStore::delete")
    }
}
