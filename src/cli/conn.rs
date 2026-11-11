//! `redis-lens conn ...` and `redis-lens discover`.
//!
//! Owner: connections. The logic lives in functions that take a config path and a
//! [`SecretStore`] so tests can run them against a temp dir and a `MemoryStore`;
//! `run` and `discover` wire them to the real keychain.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use secrecy::{ExposeSecret, SecretString};

use super::{ConnAddArgs, ConnCommand};
use crate::config::Config;
use crate::conn::docker::DiscoveredInstance;
use crate::conn::token::display_command;
use crate::conn::{self, parse_connection_string, provider, ssh};
use crate::model::{Auth, Credentials, Env, PermStatus, Profile, SecretRef, SshTunnel};
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

/// What `conn add --ssh [USER@]HOST[:PORT]` names: the bastion, and the SSH user that
/// goes to the keychain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshTarget {
    pub user: Option<String>,
    /// An IPv6 address without brackets; anything else as given.
    pub host: String,
    pub port: Option<u16>,
}

/// Parse `[USER@]HOST[:PORT]`. The user is everything before the last `@` (as ssh
/// splits it); an IPv6 address takes brackets to carry a port (`[::1]:2222`). Errors are
/// fixed texts that never repeat the value, which may hold the user.
pub(crate) fn parse_ssh_target(s: &str) -> anyhow::Result<SshTarget> {
    let (user, rest) = match s.rsplit_once('@') {
        Some((u, rest)) => {
            if u.is_empty() || u.chars().any(char::is_control) {
                bail!("--ssh: the user before '@' is empty or has control characters");
            }
            (Some(u.to_string()), rest)
        }
        None => (None, s),
    };
    let port = |p: &str| -> anyhow::Result<u16> {
        match p.parse::<u16>() {
            Ok(n) if n != 0 => Ok(n),
            _ => bail!("--ssh: the port must be a number from 1 to 65535"),
        }
    };
    let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
        let Some((h, after)) = inner.split_once(']') else {
            bail!("--ssh: a '[' without its ']'; write an IPv6 address as [ADDRESS]:PORT");
        };
        if h.parse::<std::net::Ipv6Addr>().is_err() {
            bail!("--ssh: only an IPv6 address goes in brackets");
        }
        let p = match after {
            "" => None,
            a => match a.strip_prefix(':') {
                Some(p) => Some(port(p)?),
                None => bail!("--ssh: expected :PORT after the ']'"),
            },
        };
        (h, p)
    } else if rest.matches(':').count() > 1 {
        if rest.parse::<std::net::Ipv6Addr>().is_err() {
            bail!("--ssh: put an IPv6 address in brackets to give a port: [ADDRESS]:PORT");
        }
        (rest, None)
    } else if let Some((h, p)) = rest.split_once(':') {
        (h, Some(port(p)?))
    } else {
        (rest, None)
    };
    if ssh::check_host(host).is_err() {
        bail!(
            "--ssh: the bastion host must be a host name, an address or a ~/.ssh/config alias \
             of at most 253 bytes, with no spaces and no leading '-'"
        );
    }
    Ok(SshTarget {
        user,
        host: host.to_string(),
        port,
    })
}

/// The tunnel and SSH user from `--ssh`, `--ssh-identity` and `--ssh-known-hosts`, with
/// the files checked to exist. `None` without `--ssh`.
fn ssh_from_args(args: &ConnAddArgs) -> anyhow::Result<Option<(SshTunnel, Option<String>)>> {
    let Some(spec) = &args.ssh else {
        if args.ssh_identity.is_some() || args.ssh_known_hosts.is_some() {
            bail!("--ssh-identity and --ssh-known-hosts need --ssh");
        }
        return Ok(None);
    };
    let target = parse_ssh_target(spec)?;
    for (flag, path, what) in [
        ("--ssh-identity", &args.ssh_identity, "identity file"),
        (
            "--ssh-known-hosts",
            &args.ssh_known_hosts,
            "known_hosts file",
        ),
    ] {
        if let Some(p) = path {
            if !p.is_file() {
                bail!("{flag}: {} does not exist or is not a file", p.display());
            }
            ssh::check_path(p, what)?;
        }
    }
    Ok(Some((
        SshTunnel {
            host: target.host,
            port: target.port,
            identity_file: args.ssh_identity.clone(),
            known_hosts_file: args.ssh_known_hosts.clone(),
        },
        target.user,
    )))
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
    add_confirmed(
        path,
        secrets,
        args,
        stdin_password,
        warn,
        prompt,
        &confirm_on_terminal,
    )
    .await
}

/// Ask on the terminal whether to save `command` as the profile's token command. Without
/// a terminal there is no one to ask, so it refuses: pass `--yes` in scripts.
fn confirm_on_terminal(command: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "saving a token command needs confirmation, and standard input is not a terminal; \
             pass --yes to save it without asking"
        );
    }
    eprintln!(
        "This profile will run this command at every connect and send what it prints as \
         the password:\n  {command}\nIt is run directly, not through a shell. Save it? [y/N] "
    );
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("could not read the answer")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// [`add_with`] with the token-command confirmation injected too. `confirm` gets the
/// command as it would be typed and is only called for a token command without `--yes`.
pub async fn add_confirmed(
    path: &Path,
    secrets: &dyn SecretStore,
    args: &ConnAddArgs,
    stdin_password: Option<String>,
    warn: &mut dyn std::io::Write,
    prompt: &dyn Fn() -> anyhow::Result<String>,
    confirm: &dyn Fn(&str) -> anyhow::Result<bool>,
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

    // Checked before anything is written.
    let ssh = ssh_from_args(args)?;

    let (url_user, url_password) = match &parsed.credentials {
        Some(c) => (
            c.username.clone(),
            Some(c.password.expose_secret().to_string()).filter(|p| !p.is_empty()),
        ),
        None => (None, None),
    };

    let token_command = (!args.token_command.is_empty()).then(|| args.token_command.clone());
    let (auth, creds) = if let Some(cmd) = &token_command {
        if args.entra {
            bail!("--entra and a token command are two ways to sign in; give one of them");
        }
        if args.password_stdin || args.ask_password {
            bail!(
                "a token command replaces the password; leave out --password-stdin and --ask-password"
            );
        }
        if url_password.is_some() {
            bail!(
                "the URL has a password and there is a token command; the token is sent as the password, \
                 so take the password out of the URL"
            );
        }
        if cmd[0].trim().is_empty() {
            bail!("the token command after -- must start with a program");
        }
        let shown = display_command(cmd);
        if !args.yes && !confirm(&shown)? {
            bail!("not saved: the token command was not confirmed");
        }
        // Only a username from the URL goes to the keychain; without one there is no
        // entry at all.
        (
            Auth::Command,
            url_user.map(|u| Credentials {
                username: Some(u),
                password: SecretString::from(String::new()),
            }),
        )
    } else if args.entra {
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

    let provider = args
        .provider
        .map(Into::into)
        .or_else(|| provider::classify(&parsed.endpoint.host));
    let (tunnel, ssh_user) = ssh.unzip();
    let ssh_user = ssh_user.flatten();
    // Behind a bastion, a loopback host is the bastion's, not this machine's: not LOCAL.
    let env_guess = match (parsed.env_guess, &tunnel) {
        (Env::Local, Some(_)) => Env::Other,
        (guess, _) => guess,
    };
    let profile = Profile {
        name: args.name.clone(),
        env: args.env.map(Into::into).unwrap_or(env_guess),
        endpoint: parsed.endpoint,
        db: parsed.db,
        auth,
        read_only: None,
        prefer_replica: args.prefer_replica,
        provider,
        token_command,
        ssh: tunnel,
    };

    let r = profile.secret_ref();
    let ssh_ref = profile.ssh_secret_ref();
    let mut save = || -> anyhow::Result<()> {
        config.add(profile.clone(), args.force)?;
        config.save(path)?;
        Ok(())
    };
    // Credentials first, rolled back if the config can't be written.
    let save_with_credentials = || -> anyhow::Result<()> {
        match &creds {
            Some(c) => set_credentials_then_save(secrets, &r, c, save),
            None => save(),
        }
    };
    let ssh_creds = ssh_user.map(|u| Credentials {
        username: Some(u),
        password: SecretString::from(String::new()),
    });
    match &ssh_creds {
        // The SSH user the same way, around the rest: put back if anything fails.
        Some(c) => set_credentials_then_save(secrets, &ssh_ref, c, save_with_credentials)?,
        None => save_with_credentials()?,
    }
    // Old entries the saved profile no longer uses go only once the config no longer
    // points at them.
    if creds.is_none()
        && existing
            .as_ref()
            .is_some_and(|e| matches!(e.auth, Auth::Keychain | Auth::Command))
    {
        secrets
            .delete(&r)
            .context("the profile was saved, but its old keychain entry could not be deleted")?;
    }
    if ssh_creds.is_none() && existing.as_ref().is_some_and(|e| e.ssh.is_some()) {
        secrets.delete(&ssh_ref).context(
            "the profile was saved, but its old SSH username could not be deleted from the keychain",
        )?;
    }

    let mut msg = format!(
        "Saved {} ({}, {}).",
        profile.name,
        profile.endpoint,
        profile.env.label()
    );
    match (&profile.token_command, &creds) {
        (Some(cmd), c) => {
            msg.push_str(&format!(
                " Token command: {}. It runs at every connect.",
                display_command(cmd)
            ));
            if c.is_some() {
                msg.push_str(" The username is in the OS keychain.");
            }
        }
        (None, Some(_)) => msg.push_str(" Credentials are in the OS keychain."),
        (None, None) => {}
    }
    if let Some(t) = &profile.ssh {
        msg.push_str(&format!(" Through SSH bastion {}.", ssh::label(t)));
        if ssh_creds.is_some() {
            msg.push_str(" The SSH username is in the OS keychain.");
        }
    }
    if let Some(p) = profile.provider {
        msg.push_str(&format!(" Provider: {}.", provider::label(p)));
        if let Some(n) = provider::notice(Some(p)) {
            msg.push_str(&format!("\n{n}"));
        }
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
        Auth::Command => "command",
    }
}

/// An aligned table of the saved connections.
pub fn list(path: &Path) -> anyhow::Result<String> {
    let config = Config::load(path)?;
    if config.profiles.is_empty() {
        return Ok("No saved connections. Add one with `redis-lens conn add`.".to_string());
    }
    // The SSH column only when some profile goes through a bastion.
    let any_ssh = config.profiles.iter().any(|p| p.ssh.is_some());
    let rows: Vec<Vec<String>> = config
        .profiles
        .iter()
        .map(|p| {
            let mut row = vec![
                p.name.clone(),
                p.env.label().to_string(),
                p.endpoint.to_string(),
                if p.endpoint.tls { "tls" } else { "plain" }.to_string(),
                auth_label(p.auth).to_string(),
            ];
            if any_ssh {
                row.push(p.ssh.as_ref().map(ssh::label).unwrap_or_default());
            }
            row.push(p.provider.map_or("", provider::label).to_string());
            row
        })
        .collect();
    let header: &[&str] = if any_ssh {
        &["NAME", "ENV", "ENDPOINT", "TLS", "AUTH", "SSH", "PROVIDER"]
    } else {
        &["NAME", "ENV", "ENDPOINT", "TLS", "AUTH", "PROVIDER"]
    };
    Ok(table(header, &rows))
}

/// Remove a profile from the config, then its keychain entry. The entry is deleted
/// only after the config is saved, so a failed save leaves the keychain untouched.
pub fn remove(path: &Path, secrets: &dyn SecretStore, name: &str) -> anyhow::Result<String> {
    let mut config = Config::load(path)?;
    let profile = config.remove(name)?;
    config.save(path)?;
    let main = secrets
        .delete(&profile.secret_ref())
        .context("the profile was removed, but its keychain entry could not be deleted");
    let ssh = match profile.ssh {
        Some(_) => secrets.delete(&profile.ssh_secret_ref()).context(
            "the profile was removed, but its SSH username could not be deleted from the keychain",
        ),
        None => Ok(()),
    };
    main?;
    ssh?;
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
    if let Some(t) = &profile.ssh {
        lines.push(format!("Through SSH bastion {}.", ssh::label(t)));
    }
    if let Some(p) = profile.provider {
        let mut line = format!("Provider: {}.", provider::label(p));
        if let Some(n) = provider::notice(Some(p)) {
            line.push(' ');
            line.push_str(n);
        }
        lines.push(line);
    }
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
    use proptest::strategy::Strategy as _;

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
            provider: None,
            token_command: Vec::new(),
            yes: false,
            ssh: None,
            ssh_identity: None,
            ssh_known_hosts: None,
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

    fn token_args(name: &str, url: &str, cmd: &[&str]) -> ConnAddArgs {
        let mut a = args(name, url);
        a.token_command = cmd.iter().map(|s| s.to_string()).collect();
        a
    }

    const AZ: [&str; 9] = [
        "az",
        "account",
        "get-access-token",
        "--resource",
        "https://redis.azure.com",
        "--query",
        "accessToken",
        "-o",
        "tsv",
    ];

    async fn add_token(
        path: &Path,
        store: &MemoryStore,
        a: &ConnAddArgs,
        confirm: &dyn Fn(&str) -> anyhow::Result<bool>,
    ) -> anyhow::Result<String> {
        let no_prompt = || -> anyhow::Result<String> { panic!("no password prompt expected") };
        add_confirmed(path, store, a, None, &mut Vec::new(), &no_prompt, confirm).await
    }

    fn never(_: &str) -> anyhow::Result<bool> {
        panic!("no confirmation expected")
    }

    /// Replacing a password profile with a token command that has no username leaves
    /// no keychain entry behind: the old username and password are deleted.
    #[tokio::test]
    async fn replacing_with_a_token_command_without_a_username_deletes_the_old_entry() {
        let path = temp_path("token-replace");
        let store = MemoryStore::default();
        add(&path, &store, &args("c", "redis://u:old-pw@h"), None)
            .await
            .unwrap();
        assert!(entry(&store, "c").is_some());
        let mut a = token_args("c", "rediss://h2:6380", &["printf", "tok"]);
        a.force = true;
        a.yes = true;
        add_token(&path, &store, &a, &never).await.unwrap();
        assert_eq!(entry(&store, "c"), None);
        let p = Config::load(&path).unwrap().get("c").unwrap().clone();
        assert_eq!(p.auth, Auth::Command);
        assert_eq!(p.token_command, Some(vec!["printf".into(), "tok".into()]));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The same from a token-command profile with a username: replacing it with one
    /// without a username removes the old username from the keychain too.
    #[tokio::test]
    async fn replacing_a_token_profile_with_a_username_deletes_the_old_username() {
        let path = temp_path("token-replace-user");
        let store = MemoryStore::default();
        let mut first = token_args("c", "rediss://old-oid-77@h:6380", &["printf", "tok"]);
        first.yes = true;
        add_token(&path, &store, &first, &never).await.unwrap();
        assert!(
            entry(&store, "c").is_some(),
            "the username is in the keychain"
        );
        let mut second = token_args("c", "rediss://h:6380", &["printf", "tok2"]);
        second.force = true;
        second.yes = true;
        add_token(&path, &store, &second, &never).await.unwrap();
        assert_eq!(entry(&store, "c"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn token_command_goes_to_the_config_and_the_username_to_the_keychain() {
        let path = temp_path("token-user");
        let store = MemoryStore::default();
        let a = token_args(
            "az",
            "rediss://3f1c0a2e-uniq-oid@contoso.eastus.redis.azure.net:10000",
            &AZ,
        );
        let asked = std::cell::RefCell::new(Vec::new());
        let confirm = |cmd: &str| -> anyhow::Result<bool> {
            asked.borrow_mut().push(cmd.to_string());
            Ok(true)
        };
        let msg = add_token(&path, &store, &a, &confirm).await.unwrap();
        assert_eq!(
            asked.borrow().as_slice(),
            [
                "az account get-access-token --resource https://redis.azure.com --query accessToken -o tsv"
            ]
        );
        assert!(
            msg.contains("Token command: az account get-access-token"),
            "{msg}"
        );
        assert!(msg.contains("The username is in the OS keychain."), "{msg}");
        assert!(msg.contains("Provider: Azure."), "{msg}");
        assert!(!msg.contains("3f1c0a2e-uniq-oid"), "{msg}");

        let p = Config::load(&path).unwrap().get("az").unwrap().clone();
        assert_eq!(p.auth, Auth::Command);
        assert_eq!(
            p.token_command,
            Some(AZ.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        );
        assert_eq!(p.provider, Some(crate::model::Provider::Azure));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#"auth = "command""#), "{text}");
        assert!(text.contains("token_command = ["), "{text}");
        assert!(!text.contains("3f1c0a2e-uniq-oid"), "{text}");
        assert!(
            !text.contains("password") && !text.contains("username"),
            "{text}"
        );
        let c = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(c.username.as_deref(), Some("3f1c0a2e-uniq-oid"));
        assert_eq!(c.password.expose_secret(), "");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn token_command_without_a_username_makes_no_keychain_entry() {
        let path = temp_path("token-nouser");
        let store = MemoryStore::default();
        // A keychain profile first: switching it to a token command deletes its entry.
        add(
            &path,
            &store,
            &args("gcp", "redis://u:old-pw@10.0.0.5:6379"),
            None,
        )
        .await
        .unwrap();
        assert!(entry(&store, "gcp").is_some());
        let mut a = token_args(
            "gcp",
            "redis://10.0.0.5:6379",
            &["gcloud", "auth", "print-access-token"],
        );
        a.yes = true;
        a.force = true;
        a.provider = Some(crate::cli::ProviderArg::Memorystore);
        let msg = add_token(&path, &store, &a, &never).await.unwrap();
        assert!(!msg.contains("keychain"), "{msg}");
        assert!(msg.contains("Provider: Memorystore."), "{msg}");
        assert_eq!(entry(&store, "gcp"), None);
        let p = Config::load(&path).unwrap().get("gcp").unwrap().clone();
        assert_eq!(p.auth, Auth::Command);
        assert_eq!(p.provider, Some(crate::model::Provider::Memorystore));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("old-pw"), "{text}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn token_command_must_be_confirmed() {
        let path = temp_path("token-confirm");
        let store = MemoryStore::default();
        let a = token_args("t", "redis://svc@h", &["printf", "x"]);
        let no = |_: &str| -> anyhow::Result<bool> { Ok(false) };
        let e = add_token(&path, &store, &a, &no).await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "not saved: the token command was not confirmed"
        );
        assert!(!path.exists());
        assert_eq!(entry(&store, "t"), None);
        // No terminal: the prompt refuses, and nothing is saved.
        let refuse = |_: &str| -> anyhow::Result<bool> {
            bail!("standard input is not a terminal; pass --yes")
        };
        assert!(add_token(&path, &store, &a, &refuse).await.is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn token_command_conflicts() {
        let path = temp_path("token-conflict");
        let store = MemoryStore::default();
        let cmd = ["printf", "x"];
        // A password in the URL: an error that does not repeat it.
        let mut a = token_args("t", "redis://u:url-pw-777@h", &cmd);
        a.yes = true;
        let e = add_token(&path, &store, &a, &never)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("password"), "{e}");
        assert!(!e.contains("url-pw-777"), "{e}");
        let mut a = token_args("t", "redis://h", &cmd);
        a.yes = true;
        a.password_stdin = true;
        assert!(add_token(&path, &store, &a, &never).await.is_err());
        a.password_stdin = false;
        a.entra = true;
        assert!(add_token(&path, &store, &a, &never).await.is_err());
        let mut a = token_args("t", "redis://h", &["", "x"]);
        a.yes = true;
        assert!(add_token(&path, &store, &a, &never).await.is_err());
        assert!(!path.exists());
        assert_eq!(entry(&store, "t"), None);
    }

    #[tokio::test]
    async fn provider_comes_from_the_host_or_the_flag() {
        let path = temp_path("provider");
        let store = MemoryStore::default();
        let msg = add(
            &path,
            &store,
            &args(
                "sls",
                "rediss://c-1a2b3c.serverless.use1.cache.amazonaws.com:6379",
            ),
            None,
        )
        .await
        .unwrap();
        assert!(msg.contains("Provider: ElastiCache Serverless."), "{msg}");
        assert!(msg.contains("restricts MEMORY and OBJECT"), "{msg}");
        add(
            &path,
            &store,
            &args(
                "ec",
                "rediss://master.c.abc123.use1.cache.amazonaws.com:6379",
            ),
            None,
        )
        .await
        .unwrap();
        // The flag wins over the host name.
        let mut a = args("gcp", "redis://10.0.0.5:6379");
        a.provider = Some(crate::cli::ProviderArg::Memorystore);
        add(&path, &store, &a, None).await.unwrap();
        let mut a = args(
            "forced",
            "rediss://master.c.abc123.use1.cache.amazonaws.com:6379",
        );
        a.provider = Some(crate::cli::ProviderArg::ElasticacheServerless);
        add(&path, &store, &a, None).await.unwrap();
        add(&path, &store, &args("plain", "redis://localhost"), None)
            .await
            .unwrap();

        let cfg = Config::load(&path).unwrap();
        use crate::model::Provider;
        assert_eq!(
            cfg.get("sls").unwrap().provider,
            Some(Provider::ElasticacheServerless)
        );
        assert_eq!(cfg.get("ec").unwrap().provider, Some(Provider::Elasticache));
        assert_eq!(
            cfg.get("gcp").unwrap().provider,
            Some(Provider::Memorystore)
        );
        assert_eq!(
            cfg.get("forced").unwrap().provider,
            Some(Provider::ElasticacheServerless)
        );
        assert_eq!(cfg.get("plain").unwrap().provider, None);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#"provider = "elasticache-serverless""#),
            "{text}"
        );
        assert_eq!(text.matches("provider =").count(), 4, "{text}");

        let out = list(&path).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].ends_with("PROVIDER"), "{out}");
        let col = lines[0].find("PROVIDER").unwrap();
        let row = |name: &str| *lines.iter().find(|l| l.starts_with(name)).unwrap();
        assert_eq!(&row("sls ")[col..], "ElastiCache Serverless");
        assert_eq!(&row("ec ")[col..], "ElastiCache");
        assert_eq!(&row("gcp ")[col..], "Memorystore");
        assert!(row("plain").len() <= col, "{out}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn list_shows_command_auth() {
        let path = temp_path("list-cmd");
        let store = MemoryStore::default();
        let mut a = token_args("t", "redis://h", &["printf", "x"]);
        a.yes = true;
        add_token(&path, &store, &a, &never).await.unwrap();
        let out = list(&path).unwrap();
        assert!(out.lines().nth(1).unwrap().contains("command"), "{out}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    const SSH_USER: &str = "jump-user-x93k";

    fn ssh_ref(name: &str) -> SecretRef {
        SecretRef {
            service: crate::model::SECRET_SERVICE.to_string(),
            account: format!("ssh:profile:{name}"),
        }
    }

    fn ssh_user_in(store: &MemoryStore, name: &str) -> Option<String> {
        store.get(&ssh_ref(name)).unwrap().and_then(|c| c.username)
    }

    /// A temp dir with an identity file and a known_hosts file in it.
    fn key_files(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let path = temp_path(tag);
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let id = dir.join("id_ed25519");
        let kh = dir.join("known_hosts");
        std::fs::write(&id, "not a real key").unwrap();
        std::fs::write(&kh, "").unwrap();
        (path, id, kh)
    }

    fn ssh_args(name: &str, url: &str, ssh: &str) -> ConnAddArgs {
        let mut a = args(name, url);
        a.ssh = Some(ssh.into());
        a
    }

    fn target(user: Option<&str>, host: &str, port: Option<u16>) -> SshTarget {
        SshTarget {
            user: user.map(String::from),
            host: host.into(),
            port,
        }
    }

    #[test]
    fn ssh_targets_parse() {
        let ok = [
            (
                "bastion.example.com",
                target(None, "bastion.example.com", None),
            ),
            (
                "ops@bastion:2222",
                target(Some("ops"), "bastion", Some(2222)),
            ),
            ("10.0.0.1:22", target(None, "10.0.0.1", Some(22))),
            ("jump", target(None, "jump", None)),
            ("::1", target(None, "::1", None)),
            (
                "[2001:db8::1]:2222",
                target(None, "2001:db8::1", Some(2222)),
            ),
            ("u@[::1]", target(Some("u"), "::1", None)),
            // The user is everything before the last '@', as ssh splits it.
            (
                "me@corp.example@b",
                target(Some("me@corp.example"), "b", None),
            ),
        ];
        for (text, want) in ok {
            assert_eq!(parse_ssh_target(text).unwrap(), want, "{text}");
        }
        let bad = [
            "",
            "@host",
            "secret-user@",
            "secret-user@-oProxyCommand=x",
            "secret-user@host:0",
            "secret-user@host:99999",
            "secret-user@host:x",
            "secret-user@2001:db8::1:2222x",
            "secret-user@[2001:db8::1",
            "secret-user@[host]:22",
            "secret-user@[::1]2222",
            "secret-user@two words",
        ];
        for text in bad {
            let e = parse_ssh_target(text).unwrap_err().to_string();
            assert!(e.starts_with("--ssh:"), "{text}: {e}");
            assert!(!e.contains("secret-user"), "{text}: {e}");
        }
    }

    proptest::proptest! {
        /// Whatever `[USER@]HOST[:PORT]` is built from the parts parses back to them.
        #[test]
        fn ssh_targets_round_trip(
            user in proptest::option::of("[a-z_][a-z0-9_.@-]{0,15}"),
            host in proptest::prop_oneof![
                "[a-z][a-z0-9-]{0,20}(\\.[a-z][a-z0-9-]{0,10}){0,3}",
                proptest::prelude::any::<std::net::Ipv4Addr>().prop_map(|a| a.to_string()),
                proptest::prelude::any::<std::net::Ipv6Addr>().prop_map(|a| a.to_string()),
            ],
            port in proptest::option::of(1u16..),
        ) {
            let v6 = host.contains(':');
            let mut text = user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
            match (v6, port) {
                (true, Some(p)) => text.push_str(&format!("[{host}]:{p}")),
                (false, Some(p)) => text.push_str(&format!("{host}:{p}")),
                (_, None) => text.push_str(&host),
            }
            let got = parse_ssh_target(&text).unwrap();
            proptest::prop_assert_eq!(got, SshTarget { user, host, port });
        }

        /// Anything at all: no panic, and what parses is a host ssh can take.
        #[test]
        fn ssh_targets_never_panic(text in "\\PC{0,40}") {
            if let Ok(t) = parse_ssh_target(&text) {
                proptest::prop_assert!(ssh::check_host(&t.host).is_ok());
                proptest::prop_assert!(!t.host.contains('@'));
            }
        }
    }

    #[tokio::test]
    async fn add_ssh_stores_the_tunnel_and_the_user_only_in_the_keychain() {
        let (path, id, kh) = key_files("ssh-add");
        let store = MemoryStore::default();
        let mut a = ssh_args(
            "behind",
            "redis://u:redis-pw@10.0.0.5:6379",
            &format!("{SSH_USER}@bastion.example.com:2222"),
        );
        a.ssh_identity = Some(id.clone());
        a.ssh_known_hosts = Some(kh.clone());
        let msg = add_with(&path, &store, &a, None, &mut Vec::new(), &|| {
            panic!("no prompt")
        })
        .await
        .unwrap();
        assert!(
            msg.contains("Through SSH bastion bastion.example.com:2222."),
            "{msg}"
        );
        assert!(
            msg.contains("The SSH username is in the OS keychain."),
            "{msg}"
        );
        assert!(!msg.contains(SSH_USER), "{msg}");

        let p = Config::load(&path).unwrap().get("behind").unwrap().clone();
        assert_eq!(
            p.ssh,
            Some(SshTunnel {
                host: "bastion.example.com".into(),
                port: Some(2222),
                identity_file: Some(id),
                known_hosts_file: Some(kh),
            })
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(SSH_USER), "{text}");
        assert!(text.contains("[profile.ssh]"), "{text}");
        assert_eq!(ssh_user_in(&store, "behind").as_deref(), Some(SSH_USER));
        let c = store.get(&ssh_ref("behind")).unwrap().unwrap();
        assert_eq!(c.password.expose_secret(), "");
        // The profile's own credentials are separate.
        assert_eq!(
            entry(&store, "behind"),
            Some((Some("u".into()), "redis-pw".into()))
        );

        let out = list(&path).unwrap();
        assert!(out.lines().next().unwrap().contains("SSH"), "{out}");
        assert!(out.contains("bastion.example.com:2222"), "{out}");
        assert!(!out.contains(SSH_USER), "{out}");

        remove(&path, &store, "behind").unwrap();
        assert_eq!(ssh_user_in(&store, "behind"), None);
        assert_eq!(entry(&store, "behind"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn add_ssh_without_a_user_makes_no_ssh_entry_and_is_not_local() {
        let path = temp_path("ssh-nouser");
        let store = MemoryStore::default();
        let msg = add(
            &path,
            &store,
            &ssh_args("b", "redis://127.0.0.1:6379", "[2001:db8::1]:2222"),
            None,
        )
        .await
        .unwrap();
        assert!(
            msg.contains("Through SSH bastion [2001:db8::1]:2222."),
            "{msg}"
        );
        assert!(!msg.contains("keychain"), "{msg}");
        assert_eq!(ssh_user_in(&store, "b"), None);
        let p = Config::load(&path).unwrap().get("b").unwrap().clone();
        assert_eq!(p.ssh.as_ref().unwrap().host, "2001:db8::1");
        // 127.0.0.1 behind a bastion is on the bastion, not this machine.
        assert_eq!(p.env, Env::Other);
        // Without --ssh the column stays out of `conn list`.
        let plain = temp_path("ssh-nocol");
        add(&plain, &store, &args("p", "redis://localhost"), None)
            .await
            .unwrap();
        assert!(!list(&plain).unwrap().contains("SSH"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let _ = std::fs::remove_dir_all(plain.parent().unwrap());
    }

    #[tokio::test]
    async fn add_ssh_refuses_missing_files_and_bad_values_before_saving() {
        let (path, id, _) = key_files("ssh-missing");
        let store = MemoryStore::default();
        let mut a = ssh_args("b", "redis://u:pw@h", &format!("{SSH_USER}@bastion"));
        a.ssh_identity = Some(path.parent().unwrap().join("no-such-key"));
        let e = add(&path, &store, &a, None).await.unwrap_err().to_string();
        assert!(
            e.starts_with("--ssh-identity:") && e.contains("does not exist"),
            "{e}"
        );
        assert!(!e.contains(SSH_USER), "{e}");
        a.ssh_identity = Some(id);
        a.ssh_known_hosts = Some(path.parent().unwrap().join("no-such-kh"));
        let e = add(&path, &store, &a, None).await.unwrap_err().to_string();
        assert!(e.starts_with("--ssh-known-hosts:"), "{e}");
        // A directory is not a file either.
        a.ssh_known_hosts = Some(path.parent().unwrap().to_path_buf());
        assert!(add(&path, &store, &a, None).await.is_err());
        let e = add(
            &path,
            &store,
            &ssh_args("b", "redis://h", &format!("{SSH_USER}@-x")),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(!e.contains(SSH_USER), "{e}");
        // The key flags need --ssh.
        let mut lone = args("b", "redis://h");
        lone.ssh_known_hosts = Some("/x".into());
        assert!(add(&path, &store, &lone, None).await.is_err());
        assert!(!path.exists());
        assert_eq!(ssh_user_in(&store, "b"), None);
        assert_eq!(entry(&store, "b"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn force_replaces_or_removes_the_ssh_user() {
        let path = temp_path("ssh-force");
        let store = MemoryStore::default();
        add(
            &path,
            &store,
            &ssh_args("b", "redis://h", "old-user@bastion"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(ssh_user_in(&store, "b").as_deref(), Some("old-user"));
        let mut a = ssh_args("b", "redis://h", "new-user@bastion");
        a.force = true;
        add(&path, &store, &a, None).await.unwrap();
        assert_eq!(ssh_user_in(&store, "b").as_deref(), Some("new-user"));
        // Still through SSH, without a user: the entry goes.
        let mut a = ssh_args("b", "redis://h", "bastion");
        a.force = true;
        add(&path, &store, &a, None).await.unwrap();
        assert_eq!(ssh_user_in(&store, "b"), None);
        // And back, then no SSH at all.
        let mut a = ssh_args("b", "redis://h", "again@bastion");
        a.force = true;
        add(&path, &store, &a, None).await.unwrap();
        let mut a = args("b", "redis://h");
        a.force = true;
        add(&path, &store, &a, None).await.unwrap();
        assert_eq!(ssh_user_in(&store, "b"), None);
        assert_eq!(Config::load(&path).unwrap().get("b").unwrap().ssh, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn a_failed_save_puts_the_ssh_user_back() {
        let path = temp_path("ssh-rb");
        let store = MemoryStore::default();
        add(
            &path,
            &store,
            &ssh_args("b", "redis://u:old-pw@h", "old-user@bastion"),
            None,
        )
        .await
        .unwrap();
        let before = entry(&store, "b");
        make_unsaveable(&path);
        let mut a = ssh_args("b", "redis://v:new-pw@h2", "new-user@bastion");
        a.force = true;
        assert!(add(&path, &store, &a, None).await.is_err());
        assert_eq!(ssh_user_in(&store, "b").as_deref(), Some("old-user"));
        assert_eq!(entry(&store, "b"), before);
        // A new profile whose save fails leaves no SSH entry behind.
        assert!(
            add(
                &path,
                &store,
                &ssh_args("n", "redis://h", "u@bastion"),
                None
            )
            .await
            .is_err()
        );
        assert_eq!(ssh_user_in(&store, "n"), None);
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
                provider: None,
                token_command: None,
                ssh: None,
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
