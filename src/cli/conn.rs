//! `redis-lens conn ...` and `redis-lens discover`.
//!
//! Owner: connections. The logic lives in functions that take a config path and a
//! [`SecretStore`] so tests can run them against a temp dir and a `MemoryStore`;
//! `run` and `discover` wire them to the real keychain.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use secrecy::{ExposeSecret, SecretString};

use super::{ConnAddArgs, ConnCommand};
use crate::config::Config;
use crate::conn::docker::DiscoveredInstance;
use crate::conn::{self, parse_connection_string};
use crate::model::{Auth, Credentials, PermStatus, Profile, SecretRef};
use crate::secrets::{KeyringStore, SecretError, SecretStore};

fn config_path(config: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match config {
        Some(p) => Ok(p),
        None => Ok(Config::default_path()?),
    }
}

pub async fn run(config: Option<PathBuf>, cmd: ConnCommand) -> anyhow::Result<()> {
    let path = config_path(config)?;
    let secrets = KeyringStore;
    match cmd {
        ConnCommand::Add(args) => {
            let stdin_password = if args.password_stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("could not read the password from standard input")?;
                Some(buf)
            } else {
                None
            };
            println!("{}", add(&path, &secrets, &args, stdin_password).await?);
        }
        ConnCommand::List => println!("{}", list(&path)?),
        ConnCommand::Remove { name } => println!("{}", remove(&path, &secrets, &name)?),
        ConnCommand::Test { name } => println!("{}", test(&path, &secrets, &name).await?),
    }
    Ok(())
}

pub async fn discover(_config: Option<PathBuf>) -> anyhow::Result<()> {
    let found = conn::docker::discover().await?;
    println!("{}", format_discovered(&found));
    Ok(())
}

/// The keychain entry under `r` as it is now, so a failed save can put it back. An
/// entry that can't be decoded can't be restored either, so it counts as absent.
fn previous_credentials(
    secrets: &dyn SecretStore,
    r: &SecretRef,
) -> anyhow::Result<Option<Credentials>> {
    match secrets.get(r) {
        Ok(c) => Ok(c),
        Err(SecretError::Corrupt) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("could not read the existing keychain entry")),
    }
}

/// Store `creds` under `r`, then run `save` (the config write). If `save` fails, put
/// the keychain back as it was: restore the previous credentials, or delete the new
/// entry when there were none. Credentials go first so that a saved profile never
/// points at a missing entry.
pub(crate) fn set_credentials_then_save(
    secrets: &dyn SecretStore,
    r: &SecretRef,
    creds: &Credentials,
    save: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let previous = previous_credentials(secrets, r)?;
    secrets.set(r, creds)?;
    let Err(e) = save() else {
        return Ok(());
    };
    let restored = match &previous {
        Some(old) => secrets.set(r, old),
        None => secrets.delete(r),
    };
    match restored {
        Ok(()) => Err(e),
        Err(re) => Err(anyhow::anyhow!(
            "{e:#}; restoring the previous keychain entry also failed: {re}"
        )),
    }
}

fn trim_newline(s: &str) -> &str {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s)
}

/// Profile names starting with this are reserved: the TUI snapshots Docker containers
/// under `docker:<container>`.
pub const DOCKER_PREFIX: &str = "docker:";

/// Whether `name` starts with [`DOCKER_PREFIX`], in any case (`Docker:cache` too).
pub fn has_docker_prefix(name: &str) -> bool {
    name.get(..DOCKER_PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(DOCKER_PREFIX))
}

/// The warning printed when the URL carried a password. Never contains the password.
const URL_PASSWORD_WARNING: &str = "warning: the password was given in the URL, so it may now be in your shell history. \
     Prefer --password-stdin or --ask-password.";

/// Ask for a password on the terminal without echo.
fn prompt_password() -> anyhow::Result<String> {
    rpassword::prompt_password("Password: ").context("could not read the password")
}

/// Save a connection. `stdin_password` is everything read from standard input when
/// `--password-stdin` was given. Returns the line to print.
pub async fn add(
    path: &Path,
    secrets: &dyn SecretStore,
    args: &ConnAddArgs,
    stdin_password: Option<String>,
) -> anyhow::Result<String> {
    add_with(
        path,
        secrets,
        args,
        stdin_password,
        &mut std::io::stderr(),
        &prompt_password,
    )
    .await
}

/// [`add`] with the warning's destination and the `--ask-password` prompt injected.
pub async fn add_with(
    path: &Path,
    secrets: &dyn SecretStore,
    args: &ConnAddArgs,
    stdin_password: Option<String>,
    warn: &mut dyn std::io::Write,
    prompt: &dyn Fn() -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    if has_docker_prefix(&args.name) {
        bail!("names starting with {DOCKER_PREFIX} are reserved for discovered containers");
    }
    let parsed = parse_connection_string(&args.url)?;
    let mut config = Config::load(path)?;
    let existing = config.get(&args.name).cloned();
    if existing.is_some() && !args.force {
        bail!(
            "a profile named {} already exists; pass --force to replace it",
            args.name
        );
    }

    let (url_user, url_password) = match &parsed.credentials {
        Some(c) => (
            c.username.clone(),
            Some(c.password.expose_secret().to_string()).filter(|p| !p.is_empty()),
        ),
        None => (None, None),
    };

    let (auth, creds) = if args.entra {
        (Auth::EntraId, None)
    } else {
        let password: Option<SecretString> = if let Some(p) = url_password {
            // A failed warning write must not stop the save.
            let _ = writeln!(warn, "{URL_PASSWORD_WARNING}");
            Some(SecretString::from(p))
        } else if args.password_stdin {
            let raw = stdin_password.ok_or_else(|| {
                anyhow::anyhow!("--password-stdin was given but nothing was read")
            })?;
            Some(SecretString::from(trim_newline(&raw).to_string()))
        } else if args.ask_password {
            Some(SecretString::from(prompt()?))
        } else {
            None
        };
        match password {
            Some(p) if p.expose_secret().is_empty() => bail!("the password is empty"),
            Some(password) => (
                Auth::Keychain,
                Some(Credentials {
                    username: url_user,
                    password,
                }),
            ),
            None if url_user.is_some() => bail!(
                "the URL has a username but no password; pass --password-stdin or --ask-password"
            ),
            None => (Auth::None, None),
        }
    };

    let profile = Profile {
        name: args.name.clone(),
        env: args.env.map(Into::into).unwrap_or(parsed.env_guess),
        endpoint: parsed.endpoint,
        db: parsed.db,
        auth,
        read_only: None,
        prefer_replica: args.prefer_replica,
    };

    let r = profile.secret_ref();
    let mut save = || -> anyhow::Result<()> {
        config.add(profile.clone(), args.force)?;
        config.save(path)?;
        Ok(())
    };
    match &creds {
        // Credentials first, rolled back if the config can't be written.
        Some(c) => set_credentials_then_save(secrets, &r, c, save)?,
        None => {
            // Switching to no stored credentials: the old entry goes only once the
            // config no longer points at it.
            save()?;
            if existing.is_some_and(|e| e.auth == Auth::Keychain) {
                secrets.delete(&r).context(
                    "the profile was saved, but its old keychain entry could not be deleted",
                )?;
            }
        }
    }

    let mut msg = format!(
        "Saved {} ({}, {}).",
        profile.name,
        profile.endpoint,
        profile.env.label()
    );
    if creds.is_some() {
        msg.push_str(" Credentials are in the OS keychain.");
    }
    Ok(msg)
}

fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (w, c) in widths.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let last = cells.len() - 1;
        let s = cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last {
                    c.to_string()
                } else {
                    format!("{c:<w$}", w = widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        s.trim_end().to_string()
    };
    let mut out = vec![line(header.to_vec())];
    out.extend(
        rows.iter()
            .map(|r| line(r.iter().map(String::as_str).collect())),
    );
    out.join("\n")
}

fn auth_label(a: Auth) -> &'static str {
    match a {
        Auth::None => "none",
        Auth::Keychain => "keychain",
        Auth::EntraId => "entra-id",
    }
}

/// An aligned table of the saved connections.
pub fn list(path: &Path) -> anyhow::Result<String> {
    let config = Config::load(path)?;
    if config.profiles.is_empty() {
        return Ok("No saved connections. Add one with `redis-lens conn add`.".to_string());
    }
    let rows: Vec<Vec<String>> = config
        .profiles
        .iter()
        .map(|p| {
            vec![
                p.name.clone(),
                p.env.label().to_string(),
                p.endpoint.to_string(),
                if p.endpoint.tls { "tls" } else { "plain" }.to_string(),
                auth_label(p.auth).to_string(),
            ]
        })
        .collect();
    Ok(table(&["NAME", "ENV", "ENDPOINT", "TLS", "AUTH"], &rows))
}

/// Remove a profile from the config, then its keychain entry. The entry is deleted
/// only after the config is saved, so a failed save leaves the keychain untouched.
pub fn remove(path: &Path, secrets: &dyn SecretStore, name: &str) -> anyhow::Result<String> {
    let mut config = Config::load(path)?;
    let profile = config.remove(name)?;
    config.save(path)?;
    secrets
        .delete(&profile.secret_ref())
        .context("the profile was removed, but its keychain entry could not be deleted")?;
    Ok(format!("Removed {name}."))
}

/// Connect to a saved profile and report which commands are allowed.
pub async fn test(path: &Path, secrets: &dyn SecretStore, name: &str) -> anyhow::Result<String> {
    let config = Config::load(path)?;
    let profile = config
        .get(name)
        .ok_or_else(|| crate::config::ConfigError::NotFound(name.to_string()))?;
    let mut conn = conn::connect(profile, secrets).await?;
    let report = conn::check_permissions(&mut conn).await;
    let mut lines = vec![format!(
        "Connected to {} ({}, {}).",
        profile.name,
        profile.endpoint,
        profile.env.label()
    )];
    for c in &report.checks {
        lines.push(match &c.status {
            PermStatus::Allowed => format!("  ✓ {}", c.label),
            PermStatus::Denied(m) => format!("  ✗ {}: {m}", c.label),
            PermStatus::Skipped(r) => format!("  – {}: {r}", c.label),
        });
    }
    Ok(lines.join("\n"))
}

/// Discovered containers as a table. Passwords are never printed.
pub fn format_discovered(found: &[DiscoveredInstance]) -> String {
    if found.is_empty() {
        return "No running Redis or Valkey containers with a published port were found."
            .to_string();
    }
    let rows: Vec<Vec<String>> = found
        .iter()
        .map(|d| {
            vec![
                d.container.clone(),
                d.image.clone(),
                d.profile.endpoint.to_string(),
                d.auth.label().to_string(),
            ]
        })
        .collect();
    table(&["CONTAINER", "IMAGE", "ENDPOINT", "AUTH"], &rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::EnvArg;
    use crate::model::{Endpoint, Env};
    use crate::secrets::MemoryStore;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "redis-lens-cli-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("config.toml")
    }

    fn args(name: &str, url: &str) -> ConnAddArgs {
        ConnAddArgs {
            name: name.into(),
            url: url.into(),
            env: None,
            password_stdin: false,
            ask_password: false,
            entra: false,
            force: false,
            prefer_replica: false,
        }
    }

    #[tokio::test]
    async fn add_rejects_the_docker_prefix() {
        let path = temp_path("docker-prefix");
        let store = MemoryStore::default();
        let e = add(
            &path,
            &store,
            &args("docker:cache", "redis://localhost"),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "names starting with docker: are reserved for discovered containers"
        );
        assert!(!path.exists());
        for name in ["Docker:cache", "DOCKER:cache", "dOcKeR:x"] {
            let e = add(&path, &store, &args(name, "redis://localhost"), None)
                .await
                .unwrap_err();
            assert!(e.to_string().contains("reserved"), "{name}: {e}");
        }
        assert!(!path.exists());
        assert!(!has_docker_prefix("docker"));
        assert!(!has_docker_prefix("dockerx:cache"));
        assert!(!has_docker_prefix("dö"));
    }

    #[tokio::test]
    async fn add_prefer_replica_is_saved_only_when_set() {
        let path = temp_path("replica");
        let store = MemoryStore::default();
        let mut a = args("r", "redis://localhost:6390");
        a.prefer_replica = true;
        add(&path, &store, &a, None).await.unwrap();
        add(&path, &store, &args("n", "redis://localhost:6391"), None)
            .await
            .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.get("r").unwrap().prefer_replica);
        assert!(!cfg.get("n").unwrap().prefer_replica);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("prefer_replica").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn url_password_warns_without_echoing_it() {
        let path = temp_path("warn");
        let store = MemoryStore::default();
        let no_prompt = || -> anyhow::Result<String> { panic!("no prompt expected") };
        let mut out = Vec::new();
        add_with(
            &path,
            &store,
            &args("w", "redis://u:url-pw-123@localhost"),
            None,
            &mut out,
            &no_prompt,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("shell history"), "{text}");
        assert!(!text.contains("url-pw-123"), "{text}");

        let mut out = Vec::new();
        add_with(
            &path,
            &store,
            &args("nw", "redis://localhost"),
            None,
            &mut out,
            &no_prompt,
        )
        .await
        .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn ask_password_goes_to_store_only() {
        let path = temp_path("ask");
        let store = MemoryStore::default();
        let mut a = args("asked", "redis://svc@localhost:6390");
        a.ask_password = true;
        let prompt = || -> anyhow::Result<String> { Ok("prompted-pw-456".to_string()) };
        let mut out = Vec::new();
        let msg = add_with(&path, &store, &a, None, &mut out, &prompt)
            .await
            .unwrap();
        assert!(msg.contains("Credentials are in the OS keychain"));
        assert!(
            out.is_empty(),
            "no URL-password warning without a URL password"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("prompted-pw-456") && !text.contains("svc"),
            "{text}"
        );
        let p = Config::load(&path).unwrap().get("asked").unwrap().clone();
        assert_eq!(p.auth, Auth::Keychain);
        let c = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(c.username.as_deref(), Some("svc"));
        assert_eq!(c.password.expose_secret(), "prompted-pw-456");

        // An empty prompted password is refused and nothing is saved for it.
        let empty = || -> anyhow::Result<String> { Ok(String::new()) };
        let mut b = args("empty", "redis://localhost");
        b.ask_password = true;
        let e = add_with(&path, &store, &b, None, &mut Vec::new(), &empty)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("empty"), "{e}");
        assert!(Config::load(&path).unwrap().get("empty").is_none());
    }

    #[tokio::test]
    async fn add_without_credentials() {
        let path = temp_path("plain");
        let store = MemoryStore::default();
        let msg = add(&path, &store, &args("c", "redis://localhost:6390/1"), None)
            .await
            .unwrap();
        assert_eq!(msg, "Saved c (localhost:6390, LOCAL).");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.get("c").unwrap().auth, Auth::None);
        assert_eq!(cfg.get("c").unwrap().db, 1);
    }

    #[tokio::test]
    async fn add_stdin_password_goes_to_store_only() {
        let path = temp_path("stdin");
        let store = MemoryStore::default();
        let mut a = args("prod1", "rediss://svc@cache-prod.example.com:6380");
        a.password_stdin = true;
        let msg = add(&path, &store, &a, Some("hunter2-pw\r\n".into()))
            .await
            .unwrap();
        assert_eq!(
            msg,
            "Saved prod1 (cache-prod.example.com:6380, PROD). Credentials are in the OS keychain."
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("hunter2-pw") && !text.contains("svc"),
            "{text}"
        );
        let p = Config::load(&path).unwrap().get("prod1").unwrap().clone();
        assert_eq!(p.auth, Auth::Keychain);
        let c = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(c.username.as_deref(), Some("svc"));
        assert_eq!(c.password.expose_secret(), "hunter2-pw");
    }

    #[tokio::test]
    async fn username_without_password_source_is_an_error() {
        let path = temp_path("nopw");
        let store = MemoryStore::default();
        let e = add(&path, &store, &args("c", "redis://bob@h"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("--password-stdin"), "{e}");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn duplicate_needs_force_and_env_override() {
        let path = temp_path("dup");
        let store = MemoryStore::default();
        add(&path, &store, &args("c", "redis://h"), None)
            .await
            .unwrap();
        assert!(
            add(&path, &store, &args("c", "redis://h2"), None)
                .await
                .is_err()
        );
        let mut a = args("c", "redis://h2");
        a.force = true;
        a.env = Some(EnvArg::Prod);
        add(&path, &store, &a, None).await.unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.profiles.len(), 1);
        assert_eq!(cfg.get("c").unwrap().env, Env::Prod);
        assert_eq!(cfg.get("c").unwrap().endpoint.host, "h2");
    }

    #[tokio::test]
    async fn entra_stores_nothing() {
        let path = temp_path("entra");
        let store = MemoryStore::default();
        let mut a = args("az", "rediss://x.redis.azure.net:10000");
        a.entra = true;
        let msg = add(&path, &store, &a, None).await.unwrap();
        assert!(!msg.contains("keychain"));
        assert_eq!(
            Config::load(&path).unwrap().get("az").unwrap().auth,
            Auth::EntraId
        );
    }

    #[tokio::test]
    async fn list_and_remove() {
        let path = temp_path("list");
        let store = MemoryStore::default();
        assert!(list(&path).unwrap().starts_with("No saved"));
        let mut a = args("one", "redis://:pw-1@localhost");
        a.force = true;
        add(&path, &store, &a, None).await.unwrap();
        add(
            &path,
            &store,
            &args("second-name", "rediss://h.example.com:6380"),
            None,
        )
        .await
        .unwrap();
        let out = list(&path).unwrap();
        assert!(!out.contains("pw-1"));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].starts_with("NAME"));
        // aligned: ENV column starts at the same offset on every row
        let col = lines[0].find("ENV").unwrap();
        assert_eq!(lines[1].find("LOCAL"), Some(col), "{out}");
        assert_eq!(lines[2].find("OTHER"), Some(col), "{out}");

        let one = Config::load(&path).unwrap().get("one").unwrap().clone();
        assert!(store.get(&one.secret_ref()).unwrap().is_some());
        remove(&path, &store, "one").unwrap();
        assert!(store.get(&one.secret_ref()).unwrap().is_none());
        assert!(Config::load(&path).unwrap().get("one").is_none());
        assert!(remove(&path, &store, "one").is_err());
    }

    /// A config path that loads (missing or present) but can't be saved: a directory
    /// sits where `Config::save` writes its temp file. (A path whose parent is a regular
    /// file would already fail in `Config::load`, before the keychain is touched.)
    fn make_unsaveable(path: &Path) {
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
        std::fs::create_dir_all(tmp.join("blocker")).unwrap();
        assert!(Config::load(path).is_ok());
        assert!(Config::load(path).unwrap().save(path).is_err());
    }

    fn sref(name: &str) -> SecretRef {
        SecretRef {
            service: crate::model::SECRET_SERVICE.to_string(),
            account: format!("profile:{name}"),
        }
    }

    /// The entry as plain values, to compare store states.
    fn entry(store: &MemoryStore, name: &str) -> Option<(Option<String>, String)> {
        store
            .get(&sref(name))
            .unwrap()
            .map(|c| (c.username, c.password.expose_secret().to_string()))
    }

    #[tokio::test]
    async fn failed_save_removes_new_entry() {
        let path = temp_path("rb-new");
        make_unsaveable(&path);
        let store = MemoryStore::default();
        store
            .set(
                &sref("other"),
                &Credentials {
                    username: None,
                    password: SecretString::from("keep"),
                },
            )
            .unwrap();
        assert!(
            add(&path, &store, &args("c", "redis://u:new-pw@h"), None)
                .await
                .is_err()
        );
        assert_eq!(entry(&store, "c"), None);
        assert_eq!(entry(&store, "other"), Some((None, "keep".into())));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn failed_force_save_restores_old_credentials() {
        let path = temp_path("rb-force");
        let store = MemoryStore::default();
        add(&path, &store, &args("c", "redis://u:old-pw@h"), None)
            .await
            .unwrap();
        let before = entry(&store, "c");
        assert_eq!(before, Some((Some("u".into()), "old-pw".into())));
        make_unsaveable(&path);

        let mut a = args("c", "redis://v:new-pw@h2");
        a.force = true;
        assert!(add(&path, &store, &a, None).await.is_err());
        assert_eq!(entry(&store, "c"), before);
        assert_eq!(
            Config::load(&path).unwrap().get("c").unwrap().endpoint.host,
            "h"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn failed_force_switch_to_no_auth_keeps_entry() {
        let path = temp_path("rb-noauth");
        let store = MemoryStore::default();
        add(&path, &store, &args("c", "redis://u:old-pw@h"), None)
            .await
            .unwrap();
        let before = entry(&store, "c");
        make_unsaveable(&path);

        let mut a = args("c", "redis://h2");
        a.force = true;
        assert!(add(&path, &store, &a, None).await.is_err());
        assert_eq!(entry(&store, "c"), before);
        assert_eq!(
            Config::load(&path).unwrap().get("c").unwrap().auth,
            Auth::Keychain
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn failed_remove_keeps_entry() {
        let path = temp_path("rb-remove");
        let store = MemoryStore::default();
        add(&path, &store, &args("c", "redis://u:old-pw@h"), None)
            .await
            .unwrap();
        let before = entry(&store, "c");
        make_unsaveable(&path);

        assert!(remove(&path, &store, "c").is_err());
        assert_eq!(entry(&store, "c"), before);
        assert!(Config::load(&path).unwrap().get("c").is_some());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn discovered_output_hides_passwords() {
        let d = DiscoveredInstance {
            container: "cache".into(),
            image: "redis:7".into(),
            profile: Profile {
                name: "cache".into(),
                env: Env::Local,
                endpoint: Endpoint {
                    host: "127.0.0.1".into(),
                    port: 49153,
                    tls: false,
                },
                db: 0,
                auth: Auth::Keychain,
                read_only: None,
                prefer_replica: false,
            },
            credentials: Some(Credentials {
                username: None,
                password: SecretString::from("s3cret".to_string()),
            }),
            auth: crate::conn::docker::DiscoveredAuth::Password,
        };
        let out = format_discovered(&[d]);
        assert!(out.contains("127.0.0.1:49153") && out.contains("password"));
        assert!(!out.contains("s3cret"));
    }
}
