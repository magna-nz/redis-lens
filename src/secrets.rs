//! Credentials in the OS keychain.
//!
//! Owner: connections.
//!
//! Each profile has one keychain entry (see [`crate::model::Profile::secret_ref`])
//! holding both the username and the password. Nothing else in this crate may write
//! credentials anywhere.

use std::collections::HashMap;
use std::sync::Mutex;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

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

#[derive(Serialize, Deserialize)]
struct Stored {
    v: u32,
    username: Option<String>,
    password: String,
}

fn encode(creds: &Credentials) -> String {
    let stored = Stored {
        v: 1,
        username: creds.username.clone(),
        password: creds.password.expose_secret().to_string(),
    };
    serde_json::to_string(&stored).expect("plain struct serializes")
}

fn decode(s: &str) -> Result<Credentials, SecretError> {
    let stored: Stored = serde_json::from_str(s).map_err(|_| SecretError::Corrupt)?;
    if stored.v != 1 {
        return Err(SecretError::Corrupt);
    }
    Ok(Credentials {
        username: stored.username,
        password: SecretString::from(stored.password),
    })
}

/// Error text for keychain failures. Never includes entry contents.
fn map_err(e: keyring::Error) -> SecretError {
    match e {
        keyring::Error::NoDefaultStore => {
            SecretError::Unavailable("no keychain backend on this platform".to_string())
        }
        keyring::Error::NoStorageAccess(err) => SecretError::Unavailable(err.to_string()),
        keyring::Error::PlatformFailure(err) => SecretError::Unavailable(err.to_string()),
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => SecretError::Corrupt,
        keyring::Error::TooLong(attr, limit) => {
            SecretError::Store(format!("{attr} is longer than the limit of {limit}"))
        }
        _ => SecretError::Store("the keychain refused the operation".to_string()),
    }
}

fn entry(r: &SecretRef) -> Result<keyring::Entry, SecretError> {
    keyring::Entry::new(&r.service, &r.account).map_err(map_err)
}

/// The real store: macOS Keychain, Windows Credential Manager, or the Secret Service on Linux.
#[derive(Debug, Default)]
pub struct KeyringStore;

impl SecretStore for KeyringStore {
    fn get(&self, r: &SecretRef) -> Result<Option<Credentials>, SecretError> {
        match entry(r)?.get_password() {
            Ok(s) => decode(&s).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_err(e)),
        }
    }
    fn set(&self, r: &SecretRef, creds: &Credentials) -> Result<(), SecretError> {
        entry(r)?.set_password(&encode(creds)).map_err(map_err)
    }
    fn delete(&self, r: &SecretRef) -> Result<(), SecretError> {
        match entry(r)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    }
}

/// In-memory store for tests and for credentials read from Docker containers.
#[derive(Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<SecretRef, Credentials>>,
}

impl MemoryStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SecretRef, Credentials>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, r: &SecretRef) -> Result<Option<Credentials>, SecretError> {
        Ok(self.lock().get(r).cloned())
    }
    fn set(&self, r: &SecretRef, creds: &Credentials) -> Result<(), SecretError> {
        self.lock().insert(r.clone(), creds.clone());
        Ok(())
    }
    fn delete(&self, r: &SecretRef) -> Result<(), SecretError> {
        self.lock().remove(r);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sref(n: &str) -> SecretRef {
        SecretRef {
            service: "redis-lens-test".into(),
            account: n.into(),
        }
    }

    fn creds(user: Option<&str>, pw: &str) -> Credentials {
        Credentials {
            username: user.map(String::from),
            password: SecretString::from(pw.to_string()),
        }
    }

    #[test]
    fn memory_store_round_trip() {
        let m = MemoryStore::default();
        let r = sref("a");
        assert!(m.get(&r).unwrap().is_none());
        m.set(&r, &creds(Some("u"), "p")).unwrap();
        let got = m.get(&r).unwrap().unwrap();
        assert_eq!(got.username.as_deref(), Some("u"));
        assert_eq!(got.password.expose_secret(), "p");
        m.set(&r, &creds(None, "q")).unwrap();
        let got = m.get(&r).unwrap().unwrap();
        assert_eq!(got.username, None);
        assert_eq!(got.password.expose_secret(), "q");
        m.delete(&r).unwrap();
        assert!(m.get(&r).unwrap().is_none());
        m.delete(&r).unwrap();
    }

    #[test]
    fn memory_store_entries_are_independent() {
        let m = MemoryStore::default();
        m.set(&sref("a"), &creds(None, "1")).unwrap();
        m.set(&sref("b"), &creds(None, "2")).unwrap();
        m.delete(&sref("a")).unwrap();
        assert!(m.get(&sref("a")).unwrap().is_none());
        assert!(m.get(&sref("b")).unwrap().is_some());
    }

    #[test]
    fn json_round_trip() {
        let s = encode(&creds(Some("alice"), "p\"w\\d é"));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["username"], "alice");
        let back = decode(&s).unwrap();
        assert_eq!(back.username.as_deref(), Some("alice"));
        assert_eq!(back.password.expose_secret(), "p\"w\\d é");
    }

    #[test]
    fn json_null_username() {
        let s = encode(&creds(None, "pw"));
        assert!(s.contains("\"username\":null"));
        assert!(decode(&s).unwrap().username.is_none());
    }

    #[test]
    fn corrupt_json_is_corrupt_and_does_not_leak() {
        for bad in [
            "not json",
            "{}",
            "{\"v\":2,\"username\":null,\"password\":\"x\"}",
        ] {
            let e = decode(bad).err().unwrap();
            assert!(matches!(e, SecretError::Corrupt));
            assert!(!e.to_string().contains("not json"));
        }
    }

    /// Manual only: touches the real OS keychain.
    #[test]
    #[ignore = "uses the real OS keychain"]
    fn keyring_store_round_trip() {
        let store = KeyringStore;
        let r = sref(&format!("throwaway-{}", std::process::id()));
        store.set(&r, &creds(Some("u"), "pw")).unwrap();
        let got = store.get(&r).unwrap().unwrap();
        assert_eq!(got.username.as_deref(), Some("u"));
        assert_eq!(got.password.expose_secret(), "pw");
        store.delete(&r).unwrap();
        assert!(store.get(&r).unwrap().is_none());
        store.delete(&r).unwrap();
    }
}
