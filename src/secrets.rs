//! Credentials in the OS keychain.
//!
//! Owner: connections.
//!
//! Each profile has one keychain entry (see [`crate::model::Profile::secret_ref`])
//! holding both the username and the password. Nothing else in this crate may write
//! credentials anywhere.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

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
    /// The keychain call did not return in time. The field is the account name.
    #[error("{}", timeout_message(.0))]
    Timeout(String),
}

/// How long a keychain call may take before [`SecretError::Timeout`].
pub const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// The text for [`SecretError::Timeout`]. Names the entry's account, never its contents.
fn timeout_message(account: &str) -> String {
    let secs = KEYCHAIN_TIMEOUT.as_secs();
    if cfg!(target_os = "macos") {
        format!(
            "macOS is waiting for permission to use the keychain entry {account}. \
             Click \"Always Allow\" in the dialog, then try again."
        )
    } else {
        format!(
            "the OS keychain did not answer within {secs} s for {account} \
             (it may be locked or waiting for you); unlock it and try again."
        )
    }
}

/// Name of the worker thread that runs keychain calls.
const WORKER_NAME: &str = "keychain";

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// A panic hook that prints nothing for the keychain worker (a panic there is reported
/// as [`SecretError::Store`], and its message must not land on the TUI) and hands every
/// other panic to `prev`.
fn silence_keychain_worker(prev: PanicHook) -> PanicHook {
    Box::new(move |info| {
        if std::thread::current().name() != Some(WORKER_NAME) {
            prev(info);
        }
    })
}

/// Install [`silence_keychain_worker`] once, on top of whatever hook is current. It runs
/// on the first keychain call, which for the TUI is after the TUI has installed its own
/// hook, so ours sits outermost: a keychain panic never reaches the TUI's hook (which
/// would restore the terminal while the TUI is still running), and any other panic goes
/// through ours to the TUI's hook and on to the default one. A hook installed later
/// chains to ours through `take_hook` and keeps working.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(silence_keychain_worker(prev));
    });
}

/// Run `f` on a worker thread and wait up to `dur` for it. On timeout the worker is left
/// to finish on its own and [`SecretError::Timeout`] names `account`. A panic in `f`
/// prints nothing and gives a fixed [`SecretError::Store`] text, never the payload.
fn with_timeout<T: Send + 'static>(
    dur: Duration,
    account: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SecretError> {
    install_panic_hook();
    run_worker(dur, account, f)
}

fn run_worker<T: Send + 'static>(
    dur: Duration,
    account: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SecretError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(WORKER_NAME.into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|_| SecretError::Store("could not start a keychain thread".to_string()))?;
    match rx.recv_timeout(dur) {
        Ok(v) => Ok(v),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(SecretError::Timeout(account.to_string()))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(SecretError::Store("the keychain call failed".to_string()))
        }
    }
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
///
/// Each call runs on a worker thread and gives up after [`KEYCHAIN_TIMEOUT`]: on macOS a
/// rebuilt binary makes the keychain ask for access, and the call blocks until someone
/// answers the dialog.
#[derive(Debug, Default)]
pub struct KeyringStore;

impl SecretStore for KeyringStore {
    fn get(&self, r: &SecretRef) -> Result<Option<Credentials>, SecretError> {
        let owned = r.clone();
        with_timeout(KEYCHAIN_TIMEOUT, &r.account, move || {
            match entry(&owned)?.get_password() {
                Ok(s) => decode(&s).map(Some),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(map_err(e)),
            }
        })?
    }
    fn set(&self, r: &SecretRef, creds: &Credentials) -> Result<(), SecretError> {
        let owned = r.clone();
        let encoded = encode(creds);
        with_timeout(KEYCHAIN_TIMEOUT, &r.account, move || {
            entry(&owned)?.set_password(&encoded).map_err(map_err)
        })?
    }
    fn delete(&self, r: &SecretRef) -> Result<(), SecretError> {
        let owned = r.clone();
        with_timeout(KEYCHAIN_TIMEOUT, &r.account, move || {
            match entry(&owned)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(map_err(e)),
            }
        })?
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

    #[test]
    fn a_panicking_keychain_call_gives_a_fixed_error_without_the_payload() {
        let err = run_worker(Duration::from_secs(5), "acct", || -> u8 {
            panic!("hunter2-secret-payload")
        })
        .unwrap_err();
        assert!(matches!(&err, SecretError::Store(m) if m == "the keychain call failed"));
        assert!(!format!("{err} {err:?}").contains("hunter2"));
    }

    #[test]
    fn the_hook_is_silent_for_the_keychain_thread_and_forwards_other_threads() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let orig = std::panic::take_hook();
        let orig = std::sync::Arc::new(orig);
        let orig2 = orig.clone();
        let prev: PanicHook = Box::new(move |info| {
            let name = std::thread::current().name().unwrap_or("").to_string();
            if name.starts_with("hook-test-") {
                seen2.lock().unwrap().push(name);
            } else {
                // Panics from unrelated tests running meanwhile keep their normal output.
                orig2(info);
            }
        });
        std::panic::set_hook(silence_keychain_worker(prev));

        let r = run_worker(Duration::from_secs(5), "acct", || -> u8 { panic!("boom") });
        std::thread::Builder::new()
            .name("hook-test-other".into())
            .spawn(|| panic!("boom"))
            .unwrap()
            .join()
            .unwrap_err();

        let _ = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| orig(info)));
        assert!(matches!(r, Err(SecretError::Store(_))));
        assert_eq!(*seen.lock().unwrap(), vec!["hook-test-other".to_string()]);
    }

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

    #[test]
    fn with_timeout_returns_a_value_in_time() {
        let v = with_timeout(Duration::from_secs(5), "acct", || 42).unwrap();
        assert_eq!(v, 42);
    }

    #[test]
    fn with_timeout_gives_up_on_a_blocked_call() {
        let started = std::time::Instant::now();
        let e = with_timeout(Duration::from_millis(50), "profile:cache", || {
            std::thread::sleep(Duration::from_secs(3));
            "late"
        })
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(e, SecretError::Timeout(_)));
        let text = e.to_string();
        assert!(text.contains("profile:cache"), "{text}");
        if cfg!(target_os = "macos") {
            assert!(text.contains("Always Allow"), "{text}");
            assert!(text.contains("macOS is waiting for permission"), "{text}");
        } else {
            assert!(text.contains("did not answer within 15 s"), "{text}");
        }
    }

    #[test]
    fn with_timeout_reports_a_panicking_call_without_its_message() {
        let e = with_timeout(Duration::from_secs(5), "a", || -> u8 {
            panic!("hunter2-secret")
        })
        .unwrap_err();
        assert!(matches!(e, SecretError::Store(_)));
        assert!(!e.to_string().contains("hunter2"));
    }

    #[test]
    fn timeout_message_survives_the_tui_scrub_and_names_no_secret() {
        let e = crate::conn::ConnError::from(SecretError::Timeout("profile:cache".into()));
        let shown = crate::conn::parse::mask(&e.to_string());
        assert_eq!(shown, e.to_string());
        assert!(shown.contains("profile:cache"));
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
