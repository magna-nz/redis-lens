//! `redis-lens conn ...` and `redis-lens discover`.
//!
//! Owner: connections. The logic lives in functions that take a config path and a
//! [`SecretStore`] so tests can run them against a temp dir and a `MemoryStore`;
//! `run` and `discover` wire them to the real keychain.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use secrecy::{ExposeSecret, SecretString};

use super::{ConnAddArgs, ConnCommand, WritesArg};
use crate::config::Config;
use crate::conn::docker::DiscoveredInstance;
use crate::conn::token::display_command;
use crate::conn::{self, parse_connection_string, provider, ssh};
use crate::model::{
    Auth, Credentials, Env, PermStatus, Profile, SecretRef, Sentinel, SentinelNode, SshTunnel,
};
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
            // `--password-stdin` and `--sentinel-password-stdin` can't both be given, so
            // what standard input holds is the one asked for.
            let stdin_password = if args.password_stdin || args.sentinel_password_stdin {
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
        ConnCommand::Export { file, force } => {
            let mut stdout = std::io::stdout();
            if let Some(msg) = export(&path, file.as_deref(), force, &mut stdout)? {
                println!("{msg}");
            }
        }
        ConnCommand::Import { file, force, yes } => println!(
            "{}",
            import(
                &path,
                &secrets,
                &file,
                force,
                yes,
                &confirm_import_on_terminal
            )?
        ),
        ConnCommand::Set {
            name,
            writes,
            confirm,
            yes,
            tag,
            untag,
        } => println!(
            "{}",
            set(
                &path,
                &name,
                &SetChanges {
                    writes: writes.map(|w| w == WritesArg::On),
                    confirm: confirm.as_deref(),
                    yes,
                    tag: &tag,
                    untag: &untag,
                },
                &ask_on_terminal
            )?
        ),
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

/// The Sentinels from `--sentinel` and `--sentinel-master`, with `--sentinel-tls`, and the
/// Sentinels' own credentials for the keychain: `--sentinel-user` and a password from
/// `--sentinel-password-stdin` (`stdin`) or `--sentinel-ask-password` (`prompt`, after a
/// line on `warn` saying whose password it asks for). `None` without `--sentinel`.
/// Errors are fixed texts that never repeat a credential.
fn sentinel_from_args(
    args: &ConnAddArgs,
    stdin: Option<&str>,
    warn: &mut dyn std::io::Write,
    prompt: &dyn Fn() -> anyhow::Result<String>,
) -> anyhow::Result<Option<(Sentinel, Option<Credentials>)>> {
    let Some(spec) = &args.sentinel else {
        if args.sentinel_master.is_some()
            || args.sentinel_tls
            || args.sentinel_user.is_some()
            || args.sentinel_password_stdin
            || args.sentinel_ask_password
        {
            bail!("--sentinel-master and the other --sentinel-* flags need --sentinel");
        }
        return Ok(None);
    };
    let Some(master) = args.sentinel_master.as_deref() else {
        bail!("--sentinel needs --sentinel-master NAME, the name the Sentinels monitor");
    };
    if master.is_empty() || master.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("--sentinel-master: the name is empty or has spaces or control characters");
    }
    let mut nodes: Vec<SentinelNode> = Vec::new();
    for part in spec.split(',') {
        let node: SentinelNode = part
            .parse()
            .map_err(|e| anyhow::anyhow!("--sentinel: {e}"))?;
        if !nodes.contains(&node) {
            nodes.push(node);
        }
    }
    if args.password_stdin && args.sentinel_password_stdin {
        bail!("--password-stdin and --sentinel-password-stdin can't both read standard input");
    }
    if args.sentinel_password_stdin && args.sentinel_ask_password {
        bail!("give one of --sentinel-password-stdin and --sentinel-ask-password");
    }
    let password = if args.sentinel_password_stdin {
        let raw = stdin.ok_or_else(|| {
            anyhow::anyhow!("--sentinel-password-stdin was given but nothing was read")
        })?;
        Some(SecretString::from(trim_newline(raw).to_string()))
    } else if args.sentinel_ask_password {
        let _ = writeln!(warn, "The Sentinels' own password (not the server's):");
        Some(SecretString::from(prompt()?))
    } else {
        None
    };
    let user = args.sentinel_user.clone();
    if user
        .as_deref()
        .is_some_and(|u| u.is_empty() || u.chars().any(|c| c.is_whitespace() || c.is_control()))
    {
        bail!("--sentinel-user: the user is empty or has spaces or control characters");
    }
    let creds = match password {
        Some(p) if p.expose_secret().is_empty() => bail!("the Sentinel password is empty"),
        Some(password) => Some(Credentials {
            username: user,
            password,
        }),
        None if user.is_some() => bail!(
            "--sentinel-user needs the Sentinel's password: pass --sentinel-password-stdin or \
             --sentinel-ask-password"
        ),
        None => None,
    };
    Ok(Some((
        Sentinel {
            master: master.to_string(),
            nodes,
            tls: args.sentinel_tls,
        },
        creds,
    )))
}

/// `<master> via <first host:port>`, with `(+N)` for the other Sentinels.
pub(crate) fn sentinel_label(s: &Sentinel) -> String {
    let mut out = s.master.clone();
    if let Some(first) = s.nodes.first() {
        out.push_str(&format!(" via {first}"));
    }
    if s.nodes.len() > 1 {
        out.push_str(&format!(" (+{})", s.nodes.len() - 1));
    }
    out
}

/// Save a connection. `stdin_password` is everything read from standard input when
/// `--password-stdin` (or `--sentinel-password-stdin`, which can't be given with it) was
/// given. Returns the line to print.
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

/// Ask `question` on the terminal and return the line typed. Without a terminal there
/// is no one to ask, so it refuses with `hint`.
fn ask_on_terminal(question: &str, hint: &str) -> anyhow::Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so no one can be asked; {hint}");
    }
    eprint!("{question}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("could not read the answer")?;
    Ok(trim_newline(&answer).to_string())
}

/// The question asked before writes are turned on for `profile` (spec.md "Write
/// permissions", check 2). On a prod profile the name must be typed, or given with
/// `--confirm`, and `--yes` never replaces it. On the others `--yes` or `--confirm` with
/// the name answers it; otherwise `ask` asks `y`/`n`. `Ok` means writes may go on.
fn confirm_writes_on(
    profile: &Profile,
    confirm: Option<&str>,
    yes: bool,
    ask: &dyn Fn(&str, &str) -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    let name = profile.name.as_str();
    if profile.env == Env::Prod {
        let typed = match confirm {
            Some(c) => c.to_string(),
            None => ask(
                &format!("{name} is a prod profile. Type its name to turn writes on: "),
                &format!("pass --confirm {name} (--yes is not enough on a prod profile)"),
            )?,
        };
        if typed != name {
            bail!("writes stay off: the name given does not match {name}");
        }
        return Ok(());
    }
    match confirm {
        Some(c) if c == name => return Ok(()),
        Some(_) => bail!("writes stay off: the name given with --confirm does not match {name}"),
        None if yes => return Ok(()),
        None => {}
    }
    let answer = ask(
        &format!("Turn writes on for {name}? Each write still asks before it is sent. [y/N] "),
        "pass --yes to turn writes on without asking",
    )?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("writes stay off for {name}")
    }
}

/// `conn export [FILE] [--force]`. Without `FILE` the TOML goes to `stdout` and the
/// result is `None`; with one it is written there (atomically, mode 0600, never over an
/// existing file without `force`) and the result is the line to print. Reads the config
/// file only, never the keychain.
pub fn export(
    path: &Path,
    file: Option<&Path>,
    force: bool,
    stdout: &mut dyn std::io::Write,
) -> anyhow::Result<Option<String>> {
    let config = Config::load(path)?;
    if config.profiles.is_empty() {
        bail!("no saved connections to export");
    }
    let text = config.export_text()?;
    match file {
        None => {
            stdout
                .write_all(text.as_bytes())
                .context("could not write to standard output")?;
            Ok(None)
        }
        Some(f) => {
            crate::config::write_private(f, &text, force)?;
            let n = config.profiles.len();
            Ok(Some(format!(
                "Wrote {n} connection{} to {}. It holds no credentials.",
                if n == 1 { "" } else { "s" },
                f.display()
            )))
        }
    }
}

/// Ask on the terminal about the token command of an imported profile, as `conn add`
/// does. The command is escaped: it comes from a file.
fn confirm_import_on_terminal(name: &str, command: &str) -> anyhow::Result<bool> {
    if std::io::stdin().is_terminal() {
        eprintln!("Profile {name}:");
    }
    confirm_on_terminal(command)
}

/// `conn import FILE [--force] [--yes]` (spec.md "Small items (P4)"): every profile of
/// `file` is added, or none. Refused: a name that exists (unless `force`), a `docker:`
/// name, a name twice in the file, a bad tag, a bad bastion host, an empty token
/// command. `read_only = false` is dropped so an import never turns writes on. A token
/// command is shown and `confirm(name, command)` asked, unless `yes`. Nothing but the
/// config file is written, and `[settings]` in the file is ignored.
///
/// The keychain is never read for a credential to use, and nothing is stored in it. But
/// an imported profile must not pick up credentials stored under its name for another
/// place: a file could otherwise point `prod` at another host and be sent prod's
/// password at the next connect. So the entries under an imported name (the server's,
/// the bastion's, the Sentinels') are deleted, unless the profile replaces one that
/// sends them to the same place ([`same_destination`]). The output names each profile
/// that lost stored credentials. A failed save puts them back.
pub fn import(
    path: &Path,
    secrets: &dyn SecretStore,
    file: &Path,
    force: bool,
    yes: bool,
    confirm: &dyn Fn(&str, &str) -> anyhow::Result<bool>,
) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("could not read {}", file.display()))?;
    let mut incoming = Config::parse_profiles(&text, file)?;
    if incoming.is_empty() {
        bail!("{} has no [[profile]] to import", file.display());
    }
    let mut config = Config::load(path)?;

    // Every refusal is collected, so one run shows them all; names are escaped.
    let mut refused: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for p in &incoming {
        let shown = crate::value::escape(&p.name);
        if p.name.trim().is_empty() || p.name.chars().any(char::is_control) {
            refused.push("a profile has an empty name or one with control characters".into());
            continue;
        }
        if has_docker_prefix(&p.name) {
            refused.push(format!(
                "{shown}: names starting with {DOCKER_PREFIX} are reserved for discovered containers"
            ));
        }
        if seen.contains(&p.name.as_str()) {
            refused.push(format!("{shown}: the file has it twice"));
        }
        seen.push(&p.name);
        if config.get(&p.name).is_some() && !force {
            refused.push(format!(
                "{shown}: exists already; pass --force to replace it"
            ));
        }
        if let Some(t) = p.tags.iter().find(|t| !crate::model::valid_tag(t)) {
            refused.push(format!(
                "{shown}: {:?} is not a tag",
                crate::value::escape(t)
            ));
        }
        if let Some(t) = &p.ssh
            && ssh::check_host(&t.host).is_err()
        {
            refused.push(format!("{shown}: the SSH bastion host is not valid"));
        }
        if p.token_command
            .as_ref()
            .is_some_and(|c| c.is_empty() || c[0].trim().is_empty())
        {
            refused.push(format!("{shown}: the token command has no program"));
        }
        if let Some(s) = &p.sentinel
            && (s.nodes.is_empty()
                || s.master.is_empty()
                || s.master
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control()))
        {
            refused.push(format!(
                "{shown}: the Sentinel table needs a master name without spaces and at least one node"
            ));
        }
    }
    if !refused.is_empty() {
        bail!("nothing imported:\n  {}", refused.join("\n  "));
    }

    // Writes stay as the environment decides: an import never turns them on.
    let mut notes: Vec<String> = Vec::new();
    for p in &mut incoming {
        if p.read_only == Some(false) {
            p.read_only = None;
            notes.push(format!(
                "{}: read_only = false was dropped, so the {} default applies. \
                 Turn writes on with `conn set {} --writes on`.",
                crate::value::escape(&p.name),
                p.env.label(),
                crate::value::escape(&p.name)
            ));
        }
    }
    // Then every token command is shown, and nothing is saved before all are accepted.
    for p in &incoming {
        let Some(cmd) = &p.token_command else {
            continue;
        };
        if yes {
            continue;
        }
        let shown = crate::value::escape(&display_command(cmd));
        if !confirm(&crate::value::escape(&p.name), &shown)? {
            bail!(
                "nothing imported: the token command of {} was not confirmed",
                crate::value::escape(&p.name)
            );
        }
    }

    let n = incoming.len();
    let mut needs: Vec<String> = Vec::new();
    for p in &incoming {
        let mut what: Vec<&str> = Vec::new();
        match p.auth {
            Auth::Keychain => what.push("password or ACL user"),
            Auth::Command => what.push("username, if the server needs one"),
            Auth::EntraId | Auth::None => {}
        }
        if p.ssh.is_some() {
            what.push("SSH username, if the bastion needs one");
        }
        if p.sentinel.is_some() {
            what.push("Sentinel password, if the Sentinels need one");
        }
        if !what.is_empty() {
            needs.push(format!(
                "  {}: {}",
                crate::value::escape(&p.name),
                what.join("; ")
            ));
        }
    }

    // The keychain entries that must not outlive the import: every entry under an
    // imported name, unless the profile it replaces sends them to the same place.
    let mut stale: Vec<(String, SecretRef)> = Vec::new();
    for p in &incoming {
        if config
            .get(&p.name)
            .is_some_and(|old| same_destination(old, p))
        {
            continue;
        }
        for r in [p.secret_ref(), p.ssh_secret_ref(), p.sentinel_secret_ref()] {
            stale.push((p.name.clone(), r));
        }
    }
    let mut removed: Vec<(String, SecretRef, Credentials)> = Vec::new();
    let put_back = |removed: &[(String, SecretRef, Credentials)]| {
        for (_, r, c) in removed {
            let _ = secrets.set(r, c);
        }
    };
    for (name, r) in stale {
        let deleted = secrets.get(&r).and_then(|old| {
            if let Some(c) = old {
                secrets.delete(&r)?;
                removed.push((name.clone(), r.clone(), c));
            }
            Ok(())
        });
        if let Err(e) = deleted {
            put_back(&removed);
            return Err(anyhow::Error::new(e).context(format!(
                "nothing imported: the credentials stored under {} could not be removed from \
                 the keychain",
                crate::value::escape(&name)
            )));
        }
    }
    let names: Vec<String> = incoming
        .iter()
        .map(|p| crate::value::escape(&p.name))
        .collect();
    let saved = incoming
        .into_iter()
        .try_for_each(|p| config.add(p, force))
        .and_then(|()| config.save(path));
    if let Err(e) = saved {
        put_back(&removed);
        return Err(e.into());
    }
    let mut dropped: Vec<String> = Vec::new();
    for (name, _, _) in &removed {
        let shown = crate::value::escape(name);
        if !dropped.contains(&shown) {
            dropped.push(shown);
        }
    }

    let mut lines = vec![format!(
        "Imported {n} connection{}: {}.",
        if n == 1 { "" } else { "s" },
        names.join(", ")
    )];
    lines.extend(notes);
    if !dropped.is_empty() {
        lines.push(format!(
            "Credentials stored in the OS keychain under {} were removed: they were stored \
             for another address, bastion, Sentinel or kind of auth.",
            dropped.join(", ")
        ));
    }
    if !needs.is_empty() {
        lines.push(
            "No credentials came with them. These need `conn add <name> --force ...` to store them:"
                .into(),
        );
        lines.extend(needs);
    }
    Ok(lines.join("\n"))
}

/// Whether `new` sends the credentials stored under its name to the same place as
/// `old`: the same address and TLS, kind of auth, token command, bastion and Sentinels.
/// Only then may an import that replaces `old` keep its keychain entries.
fn same_destination(old: &Profile, new: &Profile) -> bool {
    old.endpoint == new.endpoint
        && old.auth == new.auth
        && old.token_command == new.token_command
        && old.ssh == new.ssh
        && old.sentinel == new.sentinel
}

/// The tags after `add` and `remove` are applied to `current`: each validated (see
/// [`crate::model::valid_tag`]), duplicates dropped, order kept. A tag in both lists is
/// an error, and so is a tag to remove that is not there.
fn checked_tags(
    current: &[String],
    add: &[String],
    remove: &[String],
) -> anyhow::Result<Vec<String>> {
    for t in add {
        if !crate::model::valid_tag(t) {
            bail!(
                "{t:?} is not a tag: use 1 to 32 characters of letters, digits, '-', '_', '.' and '/'"
            );
        }
        if remove.contains(t) {
            bail!("{t} is both added and removed");
        }
    }
    for t in remove {
        if !current.contains(t) {
            bail!("the profile has no tag {t}");
        }
    }
    let mut out: Vec<String> = Vec::new();
    for t in current.iter().chain(add) {
        if !remove.contains(t) && !out.contains(t) {
            out.push(t.clone());
        }
    }
    Ok(out)
}

/// What `conn set` changes. At least one part must be given.
#[derive(Debug, Default, Clone, Copy)]
pub struct SetChanges<'a> {
    /// `--writes on` (`true`) or `off`.
    pub writes: Option<bool>,
    pub confirm: Option<&'a str>,
    pub yes: bool,
    pub tag: &'a [String],
    pub untag: &'a [String],
}

/// `conn set <name> [--writes on|off] [--tag T]... [--untag T]...`: sets `read_only`
/// and the tags in the config file and nothing else, in one save, after every check has
/// passed. Turning writes on asks first when the profile is read-only now; turning them
/// off never asks.
pub fn set(
    path: &Path,
    name: &str,
    changes: &SetChanges<'_>,
    ask: &dyn Fn(&str, &str) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    if changes.writes.is_none() && changes.tag.is_empty() && changes.untag.is_empty() {
        bail!("nothing to change: give --writes, --tag or --untag");
    }
    let mut config = Config::load(path)?;
    let profile = config
        .get(name)
        .ok_or_else(|| crate::config::ConfigError::NotFound(name.to_string()))?
        .clone();
    let tags = checked_tags(&profile.tags, changes.tag, changes.untag)?;
    if changes.writes == Some(true) && profile.is_read_only() {
        confirm_writes_on(&profile, changes.confirm, changes.yes, ask)?;
    }
    let mut lines = Vec::new();
    if let Some(on) = changes.writes {
        config.set_writes(name, on)?;
        lines.push(if on {
            format!(
                "Writes are on for {name}. Each write still asks as the table in the docs says."
            )
        } else {
            format!("Writes are off for {name}: it is read-only.")
        });
    }
    if !changes.tag.is_empty() || !changes.untag.is_empty() {
        config.set_tags(name, tags.clone())?;
        lines.push(if tags.is_empty() {
            format!("{name} has no tags.")
        } else {
            format!("Tags of {name}: {}.", tags.join(", "))
        });
    }
    config.save(path)?;
    Ok(lines.join("\n"))
}

/// `conn set <name> --writes on|off` alone.
pub fn set_writes(
    path: &Path,
    name: &str,
    on: bool,
    confirm: Option<&str>,
    yes: bool,
    ask: &dyn Fn(&str, &str) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    set(
        path,
        name,
        &SetChanges {
            writes: Some(on),
            confirm,
            yes,
            ..SetChanges::default()
        },
        ask,
    )
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
    add_asking(
        path,
        secrets,
        args,
        stdin_password,
        warn,
        prompt,
        confirm,
        &ask_on_terminal,
    )
    .await
}

/// [`add_confirmed`] with the `--writes on` question injected too.
#[allow(clippy::too_many_arguments)]
pub async fn add_asking(
    path: &Path,
    secrets: &dyn SecretStore,
    args: &ConnAddArgs,
    stdin_password: Option<String>,
    warn: &mut dyn std::io::Write,
    prompt: &dyn Fn() -> anyhow::Result<String>,
    confirm: &dyn Fn(&str) -> anyhow::Result<bool>,
    ask: &dyn Fn(&str, &str) -> anyhow::Result<String>,
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
    // Standard input holds the Sentinel password only with --sentinel-password-stdin
    // (which can't be given with --password-stdin).
    let sentinel_stdin = if args.sentinel_password_stdin {
        stdin_password.clone()
    } else {
        None
    };
    let tags = checked_tags(&[], &args.tags, &[])?;

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
    // The Sentinels and their own credentials, kept apart from the server's.
    let (sentinel, sentinel_creds) =
        sentinel_from_args(args, sentinel_stdin.as_deref(), &mut *warn, prompt)?.unzip();
    let sentinel_creds = sentinel_creds.flatten();

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
        read_only: args.writes.map(|w| w == WritesArg::Off),
        prefer_replica: args.prefer_replica,
        provider,
        token_command,
        ssh: tunnel,
        sentinel,
        tags,
    };
    if args.confirm.is_some() && args.writes != Some(WritesArg::On) {
        bail!("--confirm answers the question --writes on asks; give --writes on too");
    }
    // Asked only when it turns writes on for a profile that would be read-only.
    if args.writes == Some(WritesArg::On) && profile.env == Env::Prod {
        confirm_writes_on(&profile, args.confirm.as_deref(), args.yes, ask)?;
    }

    let r = profile.secret_ref();
    let ssh_ref = profile.ssh_secret_ref();
    let sentinel_ref = profile.sentinel_secret_ref();
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
    let save_with_ssh = || -> anyhow::Result<()> {
        match &ssh_creds {
            // The SSH user the same way, around the rest: put back if anything fails.
            Some(c) => set_credentials_then_save(secrets, &ssh_ref, c, save_with_credentials),
            None => save_with_credentials(),
        }
    };
    match &sentinel_creds {
        // The Sentinels' credentials the same way, around the rest.
        Some(c) => set_credentials_then_save(secrets, &sentinel_ref, c, save_with_ssh)?,
        None => save_with_ssh()?,
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
    if sentinel_creds.is_none() && existing.as_ref().is_some_and(|e| e.sentinel.is_some()) {
        secrets.delete(&sentinel_ref).context(
            "the profile was saved, but its old Sentinel credentials could not be deleted from \
             the keychain",
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
    if let Some(s) = &profile.sentinel {
        msg.push_str(&format!(
            " Asks Sentinel for {}{}: the primary is found at every connect.",
            sentinel_label(s),
            if s.tls { " (TLS)" } else { "" }
        ));
        if sentinel_creds.is_some() {
            msg.push_str(" The Sentinel credentials are in the OS keychain.");
        }
    }
    if !profile.tags.is_empty() {
        msg.push_str(&format!(" Tags: {}.", profile.tags.join(", ")));
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
    // The SENTINEL column only when some profile asks Sentinels.
    let any_sentinel = config.profiles.iter().any(|p| p.sentinel.is_some());
    // The TAGS column only when some profile has one.
    let any_tags = config.profiles.iter().any(|p| !p.tags.is_empty());
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
            if any_sentinel {
                row.push(p.sentinel.as_ref().map(sentinel_label).unwrap_or_default());
            }
            row.push(p.provider.map_or("", provider::label).to_string());
            if any_tags {
                row.push(p.tags.join(","));
            }
            row
        })
        .collect();
    let mut header = vec!["NAME", "ENV", "ENDPOINT", "TLS", "AUTH"];
    if any_ssh {
        header.push("SSH");
    }
    if any_sentinel {
        header.push("SENTINEL");
    }
    header.push("PROVIDER");
    if any_tags {
        header.push("TAGS");
    }
    Ok(table(&header, &rows))
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
    let sentinel = match profile.sentinel {
        Some(_) => secrets.delete(&profile.sentinel_secret_ref()).context(
            "the profile was removed, but its Sentinel credentials could not be deleted from the \
             keychain",
        ),
        None => Ok(()),
    };
    main?;
    ssh?;
    sentinel?;
    Ok(format!("Removed {name}."))
}

/// `conn test`'s line for a Sentinel profile: `Sentinel: <host:port> gave <master> at
/// <address>.`
pub fn sentinel_line(a: &conn::sentinel::SentinelAnswer) -> String {
    format!(
        "Sentinel: {} gave {} at {}.",
        a.sentinel, a.master, a.primary
    )
}

/// Connect to a saved profile and report which commands are allowed.
pub async fn test(path: &Path, secrets: &dyn SecretStore, name: &str) -> anyhow::Result<String> {
    let config = Config::load(path)?;
    let profile = config
        .get(name)
        .ok_or_else(|| crate::config::ConfigError::NotFound(name.to_string()))?;
    let mut conn = conn::connect(profile, secrets).await?;
    let report = conn::check_permissions(&mut conn).await;
    let answer = conn.sentinel().cloned();
    // A Sentinel profile is connected to the primary the Sentinels named.
    let at = answer
        .as_ref()
        .map_or(&profile.endpoint, |a| &a.primary)
        .to_string();
    let mut lines = vec![format!(
        "Connected to {} ({at}, {}).",
        profile.name,
        profile.env.label()
    )];
    if let Some(t) = &profile.ssh {
        lines.push(format!("Through SSH bastion {}.", ssh::label(t)));
    }
    if let Some(a) = &answer {
        lines.push(sentinel_line(a));
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
            writes: None,
            confirm: None,
            ssh: None,
            tags: Vec::new(),
            ssh_identity: None,
            ssh_known_hosts: None,
            sentinel: None,
            sentinel_master: None,
            sentinel_tls: false,
            sentinel_user: None,
            sentinel_password_stdin: false,
            sentinel_ask_password: false,
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

    const SENTINEL_USER: &str = "watch-user-r4q";
    const SENTINEL_PW: &str = "watch-pw-r4q";

    fn sentinel_entry(store: &MemoryStore, name: &str) -> Option<(Option<String>, String)> {
        let r = SecretRef {
            service: crate::model::SECRET_SERVICE.to_string(),
            account: format!("sentinel:profile:{name}"),
        };
        store
            .get(&r)
            .unwrap()
            .map(|c| (c.username, c.password.expose_secret().to_string()))
    }

    fn sentinel_args(name: &str, url: &str, nodes: &str) -> ConnAddArgs {
        let mut a = args(name, url);
        a.sentinel = Some(nodes.into());
        a.sentinel_master = Some("mymaster".into());
        a
    }

    #[tokio::test]
    async fn add_sentinel_keeps_its_credentials_apart_and_in_the_keychain_only() {
        let path = temp_path("sentinel-add");
        let store = MemoryStore::default();
        let mut a = sentinel_args(
            "watched",
            "redis://u:data-pw-1@10.0.0.5:6379/2",
            "10.0.0.1,[2001:db8::7]:5000,s3.example:26380",
        );
        a.sentinel_tls = true;
        a.sentinel_user = Some(SENTINEL_USER.into());
        a.sentinel_password_stdin = true;
        let mut warn = Vec::new();
        let msg = add_with(
            &path,
            &store,
            &a,
            Some(format!("{SENTINEL_PW}\n")),
            &mut warn,
            &|| panic!("no prompt"),
        )
        .await
        .unwrap();
        assert!(
            msg.contains("Asks Sentinel for mymaster via 10.0.0.1:26379 (+2) (TLS)"),
            "{msg}"
        );
        assert!(
            msg.contains("The Sentinel credentials are in the OS keychain."),
            "{msg}"
        );
        for secret in [SENTINEL_USER, SENTINEL_PW, "data-pw-1"] {
            assert!(!msg.contains(secret), "{msg}");
        }
        let p = Config::load(&path).unwrap().get("watched").unwrap().clone();
        let s = p.sentinel.clone().unwrap();
        assert_eq!(s.master, "mymaster");
        assert!(s.tls);
        assert_eq!(
            s.nodes.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["10.0.0.1:26379", "[2001:db8::7]:5000", "s3.example:26380"]
        );
        assert_eq!(p.db, 2);
        let text = std::fs::read_to_string(&path).unwrap();
        for secret in [SENTINEL_USER, SENTINEL_PW, "data-pw-1"] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("[profile.sentinel]"), "{text}");
        // Two entries, each with its own credentials.
        assert_eq!(
            sentinel_entry(&store, "watched"),
            Some((Some(SENTINEL_USER.into()), SENTINEL_PW.into()))
        );
        assert_eq!(
            entry(&store, "watched"),
            Some((Some("u".into()), "data-pw-1".into()))
        );

        let out = list(&path).unwrap();
        assert!(out.lines().next().unwrap().contains("SENTINEL"), "{out}");
        assert!(out.contains("mymaster via 10.0.0.1:26379 (+2)"), "{out}");
        assert!(!out.contains(SENTINEL_USER), "{out}");

        remove(&path, &store, "watched").unwrap();
        assert_eq!(sentinel_entry(&store, "watched"), None);
        assert_eq!(entry(&store, "watched"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn sentinel_passwords_by_prompt_or_none_and_force_replaces_or_deletes() {
        let path = temp_path("sentinel-force");
        let store = MemoryStore::default();
        // Asked on the terminal, after a line saying whose password it is: a
        // password-only Sentinel.
        let mut a = sentinel_args("w", "redis://h", "s1");
        a.sentinel_ask_password = true;
        let mut warn = Vec::new();
        add_with(&path, &store, &a, None, &mut warn, &|| {
            Ok(SENTINEL_PW.to_string())
        })
        .await
        .unwrap();
        let said = String::from_utf8(warn).unwrap();
        assert!(said.contains("Sentinels' own password"), "{said}");
        assert_eq!(
            sentinel_entry(&store, "w"),
            Some((None, SENTINEL_PW.into()))
        );
        assert_eq!(entry(&store, "w"), None, "the server has no credentials");
        // Replaced without Sentinel credentials: the entry goes.
        let mut a = sentinel_args("w", "redis://h", "s1");
        a.force = true;
        add(&path, &store, &a, None).await.unwrap();
        assert_eq!(sentinel_entry(&store, "w"), None);
        // Back, then no Sentinel at all.
        let mut a = sentinel_args("w", "redis://h", "s1");
        a.sentinel_password_stdin = true;
        a.force = true;
        add(&path, &store, &a, Some("pw2".into())).await.unwrap();
        assert_eq!(sentinel_entry(&store, "w"), Some((None, "pw2".into())));
        let mut a = args("w", "redis://h");
        a.force = true;
        add(&path, &store, &a, None).await.unwrap();
        assert_eq!(sentinel_entry(&store, "w"), None);
        assert_eq!(
            Config::load(&path).unwrap().get("w").unwrap().sentinel,
            None
        );
        // Without --sentinel there is no SENTINEL column.
        assert!(!list(&path).unwrap().contains("SENTINEL"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn a_failed_save_puts_the_sentinel_credentials_back() {
        let path = temp_path("sentinel-rb");
        let store = MemoryStore::default();
        let mut a = sentinel_args("w", "redis://u:old-pw@h", "s1");
        a.sentinel_password_stdin = true;
        add(&path, &store, &a, Some("old-sentinel-pw".into()))
            .await
            .unwrap();
        make_unsaveable(&path);
        let mut a = sentinel_args("w", "redis://v:new-pw@h", "s2");
        a.sentinel_user = Some("new".into());
        a.sentinel_password_stdin = true;
        a.force = true;
        assert!(
            add(&path, &store, &a, Some("new-sentinel-pw".into()))
                .await
                .is_err()
        );
        assert_eq!(
            sentinel_entry(&store, "w"),
            Some((None, "old-sentinel-pw".into()))
        );
        assert_eq!(
            entry(&store, "w"),
            Some((Some("u".into()), "old-pw".into()))
        );
        // A new profile whose save fails leaves no Sentinel entry behind.
        let mut a = sentinel_args("n", "redis://h", "s1");
        a.sentinel_password_stdin = true;
        assert!(add(&path, &store, &a, Some("x".into())).await.is_err());
        assert_eq!(sentinel_entry(&store, "n"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn add_sentinel_refuses_bad_values_before_saving() {
        let path = temp_path("sentinel-bad");
        let store = MemoryStore::default();
        let cases: Vec<(ConnAddArgs, Option<String>, &str)> = vec![
            (
                sentinel_args("b", "redis://h", "s1:0"),
                None,
                "--sentinel: ",
            ),
            (sentinel_args("b", "redis://h", "s1,"), None, "--sentinel: "),
            (sentinel_args("b", "redis://h", "::1"), None, ""),
            (
                sentinel_args("b", "redis://h", "[::1"),
                None,
                "--sentinel: ",
            ),
            (
                sentinel_args("b", "redis://h", "-oProxy"),
                None,
                "--sentinel: ",
            ),
            (
                {
                    let mut a = sentinel_args("b", "redis://h", "s1");
                    a.sentinel_master = Some("my master".into());
                    a
                },
                None,
                "--sentinel-master",
            ),
            (
                {
                    let mut a = sentinel_args("b", "redis://h", "s1");
                    a.sentinel_user = Some(SENTINEL_USER.into());
                    a
                },
                None,
                "--sentinel-user needs",
            ),
            (
                {
                    let mut a = sentinel_args("b", "redis://h", "s1");
                    a.sentinel_password_stdin = true;
                    a
                },
                Some("\n".into()),
                "the Sentinel password is empty",
            ),
            (
                {
                    let mut a = args("b", "redis://h");
                    a.sentinel_tls = true;
                    a
                },
                None,
                "need --sentinel",
            ),
            (
                {
                    let mut a = sentinel_args("b", "redis://h", "s1");
                    a.sentinel_password_stdin = true;
                    a.password_stdin = true;
                    a
                },
                Some("x".into()),
                "can't both read standard input",
            ),
        ];
        for (a, stdin, want) in cases {
            let r = add(&path, &store, &a, stdin).await;
            // A bare IPv6 Sentinel without a port is fine: the default port.
            if a.sentinel.as_deref() == Some("::1") {
                r.unwrap();
                let p = Config::load(&path).unwrap().get("b").unwrap().clone();
                assert_eq!(p.sentinel.unwrap().nodes[0].to_string(), "[::1]:26379");
                remove(&path, &store, "b").unwrap();
                continue;
            }
            let e = r.unwrap_err().to_string();
            assert!(e.contains(want), "{want}: {e}");
            assert!(!e.contains(SENTINEL_USER), "{e}");
        }
        assert!(Config::load(&path).unwrap().get("b").is_none());
        assert_eq!(sentinel_entry(&store, "b"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

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
                sentinel: None,
                tags: Vec::new(),
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

    fn no_question(_: &str, _: &str) -> anyhow::Result<String> {
        panic!("no question expected")
    }

    fn no_terminal(_: &str, hint: &str) -> anyhow::Result<String> {
        bail!("standard input is not a terminal, so no one can be asked; {hint}")
    }

    fn saved(path: &Path, name: &str) -> Option<bool> {
        Config::load(path).unwrap().get(name).unwrap().read_only
    }

    async fn add_env(
        path: &Path,
        store: &MemoryStore,
        name: &str,
        env: Option<super::super::EnvArg>,
    ) {
        let mut a = args(name, "redis://10.0.0.5:6379");
        a.env = env;
        add(path, store, &a, None).await.unwrap();
    }

    #[tokio::test]
    async fn set_writes_on_prod_needs_the_name_and_never_takes_yes() {
        use super::super::EnvArg;
        let path = temp_path("writes-prod");
        let store = MemoryStore::default();
        add_env(&path, &store, "prod-1", Some(EnvArg::Prod)).await;
        let before = std::fs::read_to_string(&path).unwrap();

        let e = set_writes(&path, "prod-1", true, None, true, &no_terminal).unwrap_err();
        assert!(e.to_string().contains("pass --confirm prod-1"), "{e}");
        let e = set_writes(&path, "prod-1", true, Some("prod-2"), true, &no_question).unwrap_err();
        assert!(e.to_string().contains("does not match"), "{e}");
        let typed_wrong = |_: &str, _: &str| Ok("prod".to_string());
        assert!(set_writes(&path, "prod-1", true, None, false, &typed_wrong).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let asked = std::cell::RefCell::new(String::new());
        let typed = |q: &str, _: &str| {
            *asked.borrow_mut() = q.to_string();
            Ok("prod-1".to_string())
        };
        set_writes(&path, "prod-1", true, None, false, &typed).unwrap();
        assert!(asked.borrow().contains("Type its name"));
        assert_eq!(saved(&path, "prod-1"), Some(false));

        // Off never asks.
        set_writes(&path, "prod-1", false, None, false, &no_question).unwrap();
        assert_eq!(saved(&path, "prod-1"), Some(true));
        set_writes(&path, "prod-1", true, Some("prod-1"), false, &no_question).unwrap();
        assert_eq!(saved(&path, "prod-1"), Some(false));
        // Already on: nothing to ask.
        set_writes(&path, "prod-1", true, None, false, &no_question).unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn set_writes_elsewhere_asks_y_n_or_takes_yes() {
        use super::super::EnvArg;
        let path = temp_path("writes-stage");
        let store = MemoryStore::default();
        add_env(&path, &store, "stage-1", Some(EnvArg::Stage)).await;
        assert_eq!(saved(&path, "stage-1"), None);
        set_writes(&path, "stage-1", false, None, false, &no_question).unwrap();
        assert_eq!(saved(&path, "stage-1"), Some(true));

        let e = set_writes(&path, "stage-1", true, None, false, &no_terminal).unwrap_err();
        assert!(e.to_string().contains("pass --yes"), "{e}");
        let no = |_: &str, _: &str| Ok("n".to_string());
        assert!(set_writes(&path, "stage-1", true, None, false, &no).is_err());
        assert_eq!(saved(&path, "stage-1"), Some(true));
        let yes = |_: &str, _: &str| Ok("Y".to_string());
        set_writes(&path, "stage-1", true, None, false, &yes).unwrap();
        assert_eq!(saved(&path, "stage-1"), Some(false));

        set_writes(&path, "stage-1", false, None, false, &no_question).unwrap();
        set_writes(&path, "stage-1", true, None, true, &no_question).unwrap();
        assert_eq!(saved(&path, "stage-1"), Some(false));

        let e = set_writes(&path, "nope", true, None, true, &no_question).unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn set_writes_touches_only_read_only_and_no_credentials() {
        let path = temp_path("writes-creds");
        let store = MemoryStore::default();
        add(
            &path,
            &store,
            &args("c", "redis://u:s3cret-pw@10.0.0.5"),
            None,
        )
        .await
        .unwrap();
        let mut before = Config::load(&path).unwrap().get("c").unwrap().clone();
        let entry_before = entry(&store, "c");
        set_writes(&path, "c", false, None, false, &no_question).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("s3cret-pw") && !text.contains("\"u\""),
            "{text}"
        );
        before.read_only = Some(true);
        assert_eq!(Config::load(&path).unwrap().get("c").unwrap(), &before);
        assert_eq!(entry(&store, "c"), entry_before);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn tags_of(path: &Path, name: &str) -> Vec<String> {
        Config::load(path).unwrap().get(name).unwrap().tags.clone()
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn add_tags_validated_and_listed() {
        let path = temp_path("tags-add");
        let store = MemoryStore::default();
        let mut a = args("t1", "redis://10.0.0.5:6379");
        a.tags = strs(&["team-a", "eu", "team-a", "a/b.c_d"]);
        let msg = add(&path, &store, &a, None).await.unwrap();
        assert!(msg.contains("Tags: team-a, eu, a/b.c_d."), "{msg}");
        assert_eq!(tags_of(&path, "t1"), ["team-a", "eu", "a/b.c_d"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("tags = ["), "{text}");

        // A bad tag saves nothing.
        for bad in ["", "has space", "ünï", &"x".repeat(33), "a,b"] {
            let mut a = args("t2", "redis://10.0.0.5:6379");
            a.tags = vec![bad.to_string()];
            let e = add(&path, &store, &a, None).await.unwrap_err();
            assert!(e.to_string().contains("is not a tag"), "{bad:?}: {e}");
        }
        assert!(Config::load(&path).unwrap().get("t2").is_none());

        // TAGS only while some profile has one.
        let listed = list(&path).unwrap();
        assert!(listed.lines().next().unwrap().ends_with("TAGS"), "{listed}");
        assert!(listed.contains("team-a,eu,a/b.c_d"), "{listed}");
        add(&path, &store, &args("plain", "redis://10.0.0.6:6379"), None)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let path = temp_path("tags-none");
        add(&path, &store, &args("plain", "redis://10.0.0.6:6379"), None)
            .await
            .unwrap();
        assert!(!list(&path).unwrap().contains("TAGS"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn set_tags_adds_removes_and_touches_nothing_else() {
        let path = temp_path("tags-set");
        let store = MemoryStore::default();
        add(
            &path,
            &store,
            &args("c", "redis://u:s3cret-pw@10.0.0.5"),
            None,
        )
        .await
        .unwrap();
        let entry_before = entry(&store, "c");
        let mut before = Config::load(&path).unwrap().get("c").unwrap().clone();
        let changes = |tag: &'static [String], untag: &'static [String]| SetChanges {
            tag,
            untag,
            ..SetChanges::default()
        };
        let add_tags: &'static [String] = Box::leak(strs(&["a", "b", "a"]).into_boxed_slice());
        let msg = set(&path, "c", &changes(add_tags, &[]), &no_question).unwrap();
        assert_eq!(msg, "Tags of c: a, b.");
        let rm: &'static [String] = Box::leak(strs(&["a"]).into_boxed_slice());
        set(&path, "c", &changes(&[], rm), &no_question).unwrap();
        assert_eq!(tags_of(&path, "c"), ["b"]);
        before.tags = strs(&["b"]);
        assert_eq!(Config::load(&path).unwrap().get("c").unwrap(), &before);
        assert_eq!(entry(&store, "c"), entry_before);

        // Refusals change nothing: a tag not there, a bad tag, both at once.
        let file = std::fs::read_to_string(&path).unwrap();
        let nope: &'static [String] = Box::leak(strs(&["zz"]).into_boxed_slice());
        assert!(set(&path, "c", &changes(&[], nope), &no_question).is_err());
        let bad: &'static [String] = Box::leak(strs(&["no good"]).into_boxed_slice());
        assert!(set(&path, "c", &changes(bad, &[]), &no_question).is_err());
        let b: &'static [String] = Box::leak(strs(&["b"]).into_boxed_slice());
        assert!(set(&path, "c", &changes(b, b), &no_question).is_err());
        assert!(set(&path, "missing", &changes(b, &[]), &no_question).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file);

        // The last tag removed leaves no `tags` key.
        let msg = set(&path, "c", &changes(&[], b), &no_question).unwrap();
        assert_eq!(msg, "c has no tags.");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("tags"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn set_with_nothing_to_change_is_refused_and_writes_and_tags_go_together() {
        use super::super::EnvArg;
        let path = temp_path("tags-writes");
        let store = MemoryStore::default();
        add_env(&path, &store, "s", Some(EnvArg::Stage)).await;
        let e = set(&path, "s", &SetChanges::default(), &no_question).unwrap_err();
        assert!(e.to_string().contains("nothing to change"), "{e}");
        // A refused writes question keeps the tags unchanged too.
        let t = strs(&["x"]);
        let e = set(
            &path,
            "s",
            &SetChanges {
                writes: Some(false),
                tag: &t,
                ..SetChanges::default()
            },
            &no_question,
        )
        .unwrap();
        assert!(
            e.contains("Writes are off for s") && e.contains("Tags of s: x."),
            "{e}"
        );
        assert_eq!(saved(&path, "s"), Some(true));
        assert_eq!(tags_of(&path, "s"), ["x"]);
        let no = |_: &str, _: &str| Ok("n".to_string());
        let t2 = strs(&["y"]);
        assert!(
            set(
                &path,
                "s",
                &SetChanges {
                    writes: Some(true),
                    tag: &t2,
                    ..SetChanges::default()
                },
                &no
            )
            .is_err()
        );
        assert_eq!(tags_of(&path, "s"), ["x"]);
        assert_eq!(saved(&path, "s"), Some(true));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn clap_conn_set_needs_a_change_and_takes_repeated_tags() {
        use clap::Parser;
        let parse = |argv: &[&str]| crate::cli::Cli::try_parse_from(argv);
        let e = parse(&["redis-lens", "conn", "set", "p"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let e = parse(&["redis-lens", "conn", "set", "p", "--yes"]).unwrap_err();
        assert!(e.use_stderr(), "{e}");
        assert!(parse(&["redis-lens", "conn", "set", "p", "--writes", "off"]).is_ok());
        assert!(
            parse(&[
                "redis-lens",
                "conn",
                "set",
                "p",
                "--tag",
                "a",
                "--tag",
                "b",
                "--untag",
                "c"
            ])
            .is_ok()
        );
        assert!(
            parse(&[
                "redis-lens",
                "conn",
                "add",
                "p",
                "--url",
                "redis://h",
                "--tag",
                "a",
                "--tag",
                "b"
            ])
            .is_ok()
        );
    }

    fn ok_confirm(_: &str, _: &str) -> anyhow::Result<bool> {
        Ok(true)
    }

    fn no_confirm(_: &str, _: &str) -> anyhow::Result<bool> {
        panic!("no question expected")
    }

    fn write_file(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn profile_text(name: &str, extra: &str) -> String {
        format!(
            "[[profile]]\nname = \"{name}\"\nenv = \"stage\"\n{extra}\n[profile.endpoint]\nhost = \"h\"\nport = 6379\ntls = false\n\n"
        )
    }

    #[tokio::test]
    async fn export_then_import_round_trips_and_holds_no_credential() {
        use super::super::EnvArg;
        let src = temp_path("exp-src");
        let store = MemoryStore::default();
        let mut a = args("a", "redis://alice:s3cret-pw@10.0.0.5:6380/3");
        a.env = Some(EnvArg::Prod);
        a.tags = strs(&["team-a", "eu"]);
        a.ssh = Some("jump-user@bastion.example.com:2222".into());
        add(&src, &store, &a, None).await.unwrap();
        let mut b = args("b", "redis://10.0.0.6");
        b.writes = Some(WritesArg::Off);
        add(&src, &store, &b, None).await.unwrap();
        add(
            &src,
            &store,
            &ConnAddArgs {
                token_command: strs(&["az", "account", "get-access-token"]),
                yes: true,
                ..args("c", "redis://bob@10.0.0.7")
            },
            None,
        )
        .await
        .unwrap();
        let mut cfg = Config::load(&src).unwrap();
        cfg.settings.skip_non_prod_confirm = true;
        cfg.save(&src).unwrap();

        let mut out = Vec::new();
        assert_eq!(export(&src, None, false, &mut out).unwrap(), None);
        let text = String::from_utf8(out).unwrap();
        for secret in ["s3cret-pw", "alice", "jump-user", "bob"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(!text.contains("[settings]"), "{text}");
        assert_eq!(text.matches("[[profile]]").count(), 3, "{text}");

        let file = src.parent().unwrap().join("out.toml");
        let msg = export(&src, Some(&file), false, &mut Vec::new())
            .unwrap()
            .unwrap();
        assert!(msg.contains("Wrote 3 connections"), "{msg}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);

        let dst = temp_path("exp-dst");
        let msg = import(
            &dst,
            &MemoryStore::default(),
            &file,
            false,
            true,
            &no_confirm,
        )
        .unwrap();
        assert!(msg.starts_with("Imported 3 connections: a, b, c."), "{msg}");
        let got = Config::load(&dst).unwrap();
        assert_eq!(got.profiles, Config::load(&src).unwrap().profiles);
        assert!(got.settings.is_default());
        let _ = std::fs::remove_dir_all(src.parent().unwrap());
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    #[tokio::test]
    async fn export_never_overwrites_without_force_and_writes_0600_atomically() {
        let src = temp_path("exp-file");
        let store = MemoryStore::default();
        add(&src, &store, &args("a", "redis://10.0.0.5"), None)
            .await
            .unwrap();
        let dir = src.parent().unwrap().to_path_buf();
        let file = dir.join("export.toml");
        std::fs::write(&file, "mine").unwrap();
        let e = export(&src, Some(&file), false, &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("--force"), "{e}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
        export(&src, Some(&file), true, &mut Vec::new()).unwrap();
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("[[profile]]")
        );
        let fresh = dir.join("fresh.toml");
        export(&src, Some(&fresh), false, &mut Vec::new()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [&file, &fresh] {
                let mode = std::fs::metadata(f).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{f:?}");
            }
        }
        // No temp file is left behind: config.toml, export.toml, fresh.toml.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
        // Nothing to export.
        let empty = temp_path("exp-empty");
        let e = export(&empty, None, false, &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("no saved connections"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_is_all_or_nothing_and_says_every_refusal() {
        let dst = temp_path("imp-refuse");
        let existing = profile_text("taken", "");
        let cfg = dst.clone();
        write_file(&cfg, &existing);
        let before = std::fs::read_to_string(&cfg).unwrap();
        let file = dst.parent().unwrap().join("in.toml");
        let text = [
            profile_text("fine", ""),
            profile_text("taken", ""),
            profile_text("Docker:cache", ""),
            profile_text("twice", ""),
            profile_text("twice", ""),
            profile_text("badtag", "tags = [\"no good\"]"),
            profile_text("badtoken", "token_command = [\"\"]"),
        ]
        .concat();
        write_file(&file, &text);
        let e = import(
            &dst,
            &MemoryStore::default(),
            &file,
            false,
            true,
            &no_confirm,
        )
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("nothing imported:"), "{e}");
        for part in [
            "taken: exists already; pass --force",
            "Docker:cache: names starting with docker:",
            "twice: the file has it twice",
            "badtag: \"no good\" is not a tag",
            "badtoken: the token command has no program",
        ] {
            assert!(e.contains(part), "{part} not in {e}");
        }
        assert!(!e.contains("fine"), "{e}");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);

        // --force replaces the one that exists; the docker: name stays refused.
        write_file(
            &file,
            &[profile_text("fine", ""), profile_text("taken", "db = 4")].concat(),
        );
        let msg = import(
            &dst,
            &MemoryStore::default(),
            &file,
            true,
            true,
            &no_confirm,
        )
        .unwrap();
        assert!(
            msg.starts_with("Imported 2 connections: fine, taken."),
            "{msg}"
        );
        let got = Config::load(&dst).unwrap();
        assert_eq!(got.get("taken").unwrap().db, 4);
        assert_eq!(got.profiles.len(), 2);
        write_file(&file, &profile_text("DOCKER:x", ""));
        assert!(
            import(
                &dst,
                &MemoryStore::default(),
                &file,
                true,
                true,
                &no_confirm
            )
            .is_err()
        );
        // Not a profile file, a missing file, an empty one.
        write_file(&file, "this is = not [toml");
        assert!(
            import(
                &dst,
                &MemoryStore::default(),
                &file,
                false,
                true,
                &no_confirm
            )
            .is_err()
        );
        write_file(&file, "# nothing\n");
        let e = import(
            &dst,
            &MemoryStore::default(),
            &file,
            false,
            true,
            &no_confirm,
        )
        .unwrap_err();
        assert!(e.to_string().contains("no [[profile]]"), "{e}");
        assert!(
            import(
                &dst,
                &MemoryStore::default(),
                &dst.with_file_name("nope.toml"),
                false,
                true,
                &no_confirm
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    #[test]
    fn import_drops_read_only_false_keeps_true_and_ignores_settings() {
        let dst = temp_path("imp-ro");
        let file = dst.parent().unwrap().join("in.toml");
        let text = format!(
            "[settings]\nskip_non_prod_confirm = true\n\n{}{}{}",
            profile_text("rw", "read_only = false"),
            profile_text("ro", "read_only = true"),
            profile_text("dflt", ""),
        );
        write_file(&file, &text);
        let msg = import(
            &dst,
            &MemoryStore::default(),
            &file,
            false,
            true,
            &no_confirm,
        )
        .unwrap();
        assert!(
            msg.contains("rw: read_only = false was dropped, so the STAGE default applies"),
            "{msg}"
        );
        assert_eq!(msg.matches("was dropped").count(), 1, "{msg}");
        let got = Config::load(&dst).unwrap();
        assert_eq!(got.get("rw").unwrap().read_only, None);
        assert_eq!(got.get("ro").unwrap().read_only, Some(true));
        assert_eq!(got.get("dflt").unwrap().read_only, None);
        assert!(got.settings.is_default());
        // A prod profile stays read-only whatever the file said.
        let file = dst.parent().unwrap().join("prod.toml");
        write_file(
            &file,
            &profile_text("p", "read_only = false").replace("stage", "prod"),
        );
        import(
            &dst,
            &MemoryStore::default(),
            &file,
            false,
            true,
            &no_confirm,
        )
        .unwrap();
        assert!(Config::load(&dst).unwrap().get("p").unwrap().is_read_only());
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    #[test]
    fn import_asks_about_a_token_command_unless_yes() {
        let dst = temp_path("imp-token");
        let file = dst.parent().unwrap().join("in.toml");
        let text = [
            profile_text("plain", ""),
            profile_text(
                "tok",
                "auth = \"command\"\ntoken_command = [\"az\", \"my token\"]",
            ),
        ]
        .concat();
        write_file(&file, &text);
        let asked = std::cell::RefCell::new(Vec::new());
        let record = |name: &str, cmd: &str| {
            asked.borrow_mut().push((name.to_string(), cmd.to_string()));
            Ok(false)
        };
        let e = import(&dst, &MemoryStore::default(), &file, false, false, &record).unwrap_err();
        assert!(e.to_string().contains("was not confirmed"), "{e}");
        assert_eq!(
            *asked.borrow(),
            [("tok".to_string(), "az 'my token'".to_string())]
        );
        // Declined: nothing, not even the plain profile, is saved.
        assert!(Config::load(&dst).unwrap().profiles.is_empty());
        // No terminal: the question itself refuses.
        let no_tty = |_: &str, _: &str| bail!("standard input is not a terminal");
        assert!(import(&dst, &MemoryStore::default(), &file, false, false, &no_tty).is_err());
        assert!(Config::load(&dst).unwrap().profiles.is_empty());
        // --yes skips the question; yes answers it.
        import(
            &dst,
            &MemoryStore::default(),
            &file,
            false,
            true,
            &no_confirm,
        )
        .unwrap();
        assert_eq!(Config::load(&dst).unwrap().profiles.len(), 2);
        let dst2 = temp_path("imp-token-y");
        import(
            &dst2,
            &MemoryStore::default(),
            &file,
            false,
            false,
            &ok_confirm,
        )
        .unwrap();
        assert_eq!(Config::load(&dst2).unwrap().profiles.len(), 2);
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
        let _ = std::fs::remove_dir_all(dst2.parent().unwrap());
    }

    #[test]
    fn import_lists_the_profiles_that_need_credentials() {
        let dst = temp_path("imp-creds");
        let file = dst.parent().unwrap().join("in.toml");
        let text = [
            profile_text("open", ""),
            profile_text("pw", "auth = \"keychain\""),
            profile_text("tok", "auth = \"command\"\ntoken_command = [\"az\"]"),
            profile_text("entra", "auth = \"entra-id\""),
        ]
        .concat()
            + "[[profile]]\nname = \"far\"\n[profile.endpoint]\nhost = \"h\"\nport = 1\n[profile.ssh]\nhost = \"jump\"\n";
        write_file(&file, &text);
        let msg = import(
            &dst,
            &MemoryStore::default(),
            &file,
            false,
            true,
            &no_confirm,
        )
        .unwrap();
        assert!(msg.contains("`conn add <name> --force ...`"), "{msg}");
        assert!(msg.contains("  pw: password or ACL user"), "{msg}");
        assert!(
            msg.contains("  tok: username, if the server needs one"),
            "{msg}"
        );
        assert!(
            msg.contains("  far: SSH username, if the bastion needs one"),
            "{msg}"
        );
        assert!(
            !msg.contains("  open:") && !msg.contains("  entra:"),
            "{msg}"
        );
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    /// The three keychain entries under `name` as plain values: the server's, the
    /// bastion's, the Sentinels'.
    fn entries(store: &MemoryStore, name: &str) -> [Option<(Option<String>, String)>; 3] {
        ["profile:", "ssh:profile:", "sentinel:profile:"].map(|prefix| {
            store
                .get(&SecretRef {
                    service: crate::model::SECRET_SERVICE.to_string(),
                    account: format!("{prefix}{name}"),
                })
                .unwrap()
                .map(|c| (c.username, c.password.expose_secret().to_string()))
        })
    }

    fn put_entries(store: &MemoryStore, name: &str) {
        for (prefix, pw) in [
            ("profile:", "prod-pw"),
            ("ssh:profile:", "ssh-pw"),
            ("sentinel:profile:", "sentinel-pw"),
        ] {
            store
                .set(
                    &SecretRef {
                        service: crate::model::SECRET_SERVICE.to_string(),
                        account: format!("{prefix}{name}"),
                    },
                    &Credentials {
                        username: Some("someone".into()),
                        password: SecretString::from(pw),
                    },
                )
                .unwrap();
        }
    }

    /// A file can't point a saved name at another host and be sent its password:
    /// `--force` deletes what the keychain holds under that name, and says so.
    #[test]
    fn import_force_removes_credentials_stored_for_another_place() {
        let dst = temp_path("imp-repoint");
        let file = dst.parent().unwrap().join("in.toml");
        let store = MemoryStore::default();
        write_file(&file, &profile_text("prod", "auth = \"keychain\""));
        import(&dst, &store, &file, false, true, &no_confirm).unwrap();
        put_entries(&store, "prod");
        put_entries(&store, "other");

        let moved = profile_text("prod", "auth = \"keychain\"").replace("\"h\"", "\"evil\"");
        write_file(&file, &moved);
        let msg = import(&dst, &store, &file, true, true, &no_confirm).unwrap();
        assert_eq!(entries(&store, "prod"), [None, None, None]);
        assert!(
            msg.contains("Credentials stored in the OS keychain under prod were removed"),
            "{msg}"
        );
        for secret in ["prod-pw", "ssh-pw", "sentinel-pw", "someone"] {
            assert!(!msg.contains(secret), "{msg}");
        }
        // Another profile's entries are not touched.
        assert!(entries(&store, "other").iter().all(Option::is_some));
        assert_eq!(
            Config::load(&dst)
                .unwrap()
                .get("prod")
                .unwrap()
                .endpoint
                .host,
            "evil"
        );
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    /// Each thing that decides where the credentials go counts as another place.
    #[test]
    fn import_force_counts_tls_auth_bastion_and_sentinel_as_another_place() {
        let base = profile_text("p", "auth = \"keychain\"");
        let changed = [
            base.replace("port = 6379", "port = 6380"),
            base.replace("tls = false", "tls = true"),
            base.replace(
                "auth = \"keychain\"",
                "auth = \"command\"\ntoken_command = [\"az\"]",
            ),
            format!(
                "{}[profile.ssh]\nhost = \"jump\"\n",
                base.trim_end_matches('\n').to_owned() + "\n"
            ),
            format!(
                "{}[profile.sentinel]\nmaster = \"m\"\nnodes = [\"s1:26379\"]\n",
                base.trim_end_matches('\n').to_owned() + "\n"
            ),
        ];
        for (i, text) in changed.iter().enumerate() {
            let dst = temp_path(&format!("imp-place-{i}"));
            let file = dst.parent().unwrap().join("in.toml");
            let store = MemoryStore::default();
            write_file(&file, &base);
            import(&dst, &store, &file, false, true, &no_confirm).unwrap();
            put_entries(&store, "p");
            write_file(&file, text);
            import(&dst, &store, &file, true, true, &no_confirm).unwrap();
            assert_eq!(entries(&store, "p"), [None, None, None], "case {i}: {text}");
            let _ = std::fs::remove_dir_all(dst.parent().unwrap());
        }
    }

    /// Replacing a profile with one that goes to the same place (new tags, another env)
    /// keeps its credentials and says nothing about them.
    #[test]
    fn import_force_to_the_same_place_keeps_the_credentials() {
        let dst = temp_path("imp-same");
        let file = dst.parent().unwrap().join("in.toml");
        let store = MemoryStore::default();
        write_file(&file, &profile_text("p", "auth = \"keychain\""));
        import(&dst, &store, &file, false, true, &no_confirm).unwrap();
        put_entries(&store, "p");
        let before = entries(&store, "p");

        write_file(
            &file,
            &profile_text("p", "auth = \"keychain\"\ntags = [\"eu\"]\ndb = 2"),
        );
        let msg = import(&dst, &store, &file, true, true, &no_confirm).unwrap();
        assert_eq!(entries(&store, "p"), before);
        assert!(!msg.contains("were removed"), "{msg}");
        assert_eq!(Config::load(&dst).unwrap().get("p").unwrap().tags, ["eu"]);
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    /// A name that is new to the config file can still have entries left in the
    /// keychain (a profile deleted by hand): they go too.
    #[test]
    fn import_removes_entries_left_under_a_new_name() {
        let dst = temp_path("imp-leftover");
        let file = dst.parent().unwrap().join("in.toml");
        let store = MemoryStore::default();
        put_entries(&store, "ghost");
        write_file(&file, &profile_text("ghost", "auth = \"keychain\""));
        let msg = import(&dst, &store, &file, false, true, &no_confirm).unwrap();
        assert_eq!(entries(&store, "ghost"), [None, None, None]);
        assert!(msg.contains("under ghost were removed"), "{msg}");
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    /// All or nothing covers the keychain: a refused import and a failed save leave
    /// every entry as it was.
    #[test]
    fn a_refused_or_failed_import_leaves_the_keychain_as_it_was() {
        let dst = temp_path("imp-rollback");
        let file = dst.parent().unwrap().join("in.toml");
        let store = MemoryStore::default();
        write_file(&file, &profile_text("p", "auth = \"keychain\""));
        import(&dst, &store, &file, false, true, &no_confirm).unwrap();
        put_entries(&store, "p");
        let before = entries(&store, "p");
        let moved = profile_text("p", "auth = \"keychain\"").replace("\"h\"", "\"evil\"");

        // Refused: the name exists and --force was not given.
        write_file(&file, &moved);
        assert!(import(&dst, &store, &file, false, true, &no_confirm).is_err());
        assert_eq!(entries(&store, "p"), before);

        // The save fails: the entries are put back and the profile is unchanged.
        make_unsaveable(&dst);
        assert!(import(&dst, &store, &file, true, true, &no_confirm).is_err());
        assert_eq!(entries(&store, "p"), before);
        assert_eq!(
            Config::load(&dst).unwrap().get("p").unwrap().endpoint.host,
            "h"
        );
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    #[test]
    fn import_checks_a_sentinel_table_and_lists_its_credentials() {
        let dst = temp_path("imp-sentinel");
        let file = dst.parent().unwrap().join("in.toml");
        let store = MemoryStore::default();
        let with = |table: &str| {
            format!(
                "{}[profile.sentinel]\n{table}\n",
                profile_text("s", "tags = [\"eu\"]")
                    .trim_end_matches('\n')
                    .to_owned()
                    + "\n"
            )
        };
        for bad in [
            "master = \"m\"\nnodes = []",
            "master = \"\"\nnodes = [\"s1\"]",
            "master = \"two words\"\nnodes = [\"s1\"]",
        ] {
            write_file(&file, &with(bad));
            let e = import(&dst, &store, &file, false, true, &no_confirm).unwrap_err();
            assert!(e.to_string().contains("s: the Sentinel table needs"), "{e}");
            assert!(Config::load(&dst).unwrap().profiles.is_empty());
        }
        write_file(
            &file,
            &with("master = \"m\"\nnodes = [\"s1\", \"s2:26380\"]\ntls = true"),
        );
        let msg = import(&dst, &store, &file, false, true, &no_confirm).unwrap();
        assert!(
            msg.contains("  s: Sentinel password, if the Sentinels need one"),
            "{msg}"
        );
        // Tags and a Sentinel table together survive an export and an import.
        let exported = dst.parent().unwrap().join("out.toml");
        export(&dst, Some(&exported), false, &mut Vec::new()).unwrap();
        let dst2 = temp_path("imp-sentinel-2");
        import(&dst2, &store, &exported, false, true, &no_confirm).unwrap();
        let got = Config::load(&dst2).unwrap();
        assert_eq!(got.profiles, Config::load(&dst).unwrap().profiles);
        let p = got.get("s").unwrap();
        assert_eq!(p.tags, ["eu"]);
        assert_eq!(p.sentinel.as_ref().unwrap().nodes.len(), 2);
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
        let _ = std::fs::remove_dir_all(dst2.parent().unwrap());
    }

    #[test]
    fn clap_conn_export_and_import() {
        use clap::Parser;
        let parse = |argv: &[&str]| crate::cli::Cli::try_parse_from(argv);
        assert!(parse(&["redis-lens", "conn", "export"]).is_ok());
        assert!(parse(&["redis-lens", "conn", "export", "f.toml", "--force"]).is_ok());
        assert!(parse(&["redis-lens", "conn", "export", "--force"]).is_err());
        assert!(parse(&["redis-lens", "conn", "import"]).is_err());
        assert!(parse(&["redis-lens", "conn", "import", "f", "--force", "--yes"]).is_ok());
    }

    #[tokio::test]
    async fn add_with_writes() {
        use super::super::EnvArg;
        let path = temp_path("add-writes");
        let store = MemoryStore::default();
        let no_prompt = || -> anyhow::Result<String> { panic!("no password prompt expected") };
        let run = |a: ConnAddArgs, ask: &'static dyn Fn(&str, &str) -> anyhow::Result<String>| {
            let path = path.clone();
            let store = &store;
            async move {
                add_asking(
                    &path,
                    store,
                    &a,
                    None,
                    &mut Vec::new(),
                    &no_prompt,
                    &never,
                    ask,
                )
                .await
            }
        };
        let mut a = args("local-off", "redis://127.0.0.1");
        a.writes = Some(WritesArg::Off);
        run(a, &no_question).await.unwrap();
        assert_eq!(saved(&path, "local-off"), Some(true));

        let mut a = args("local-on", "redis://127.0.0.1");
        a.writes = Some(WritesArg::On);
        run(a, &no_question).await.unwrap();
        assert_eq!(saved(&path, "local-on"), Some(false));

        let mut a = args("prod-on", "redis://10.0.0.5");
        a.env = Some(EnvArg::Prod);
        a.writes = Some(WritesArg::On);
        a.yes = true;
        let e = run(a, &no_terminal).await.unwrap_err();
        assert!(e.to_string().contains("--confirm prod-on"), "{e}");
        assert!(Config::load(&path).unwrap().get("prod-on").is_none());

        let mut a = args("prod-on", "redis://10.0.0.5");
        a.env = Some(EnvArg::Prod);
        a.writes = Some(WritesArg::On);
        a.confirm = Some("prod-on".into());
        run(a, &no_question).await.unwrap();
        assert_eq!(saved(&path, "prod-on"), Some(false));

        let mut a = args("x", "redis://10.0.0.5");
        a.confirm = Some("x".into());
        let e = run(a, &no_question).await.unwrap_err();
        assert!(e.to_string().contains("--writes on"), "{e}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
