//! SSH tunnels: reaching a profile's server, and its cluster nodes and replicas, through
//! a bastion with the system `ssh`.
//!
//! Owner: connections.
//!
//! Each endpoint gets its own forward, started the first time a connection needs it and
//! reused after that: `ssh -N -L 127.0.0.1:<free port>:<host>:<port>` with
//! `BatchMode=yes`, `StrictHostKeyChecking=yes` and `ExitOnForwardFailure=yes`, plus
//! `-p`, `-i`, `-l` and `UserKnownHostsFile` from the profile's [`SshTunnel`] and
//! keychain. ssh is run directly, never through a shell, with standard input and output
//! closed. Its standard error is read only to pick a fixed [`SshError`] and is never
//! shown, logged or kept. Forwards are killed when the [`Bastion`] is dropped.
//!
//! Known exposure: with `-l <user>` the SSH username is in ssh's argument list, so other
//! users of this machine can see it in the process list while a tunnel runs. Setting
//! `User` in `~/.ssh/config` for the bastion instead keeps it out of the argument list.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::model::{Endpoint, Profile, SshTunnel};
use crate::secrets::SecretStore;

use super::{ConnError, Resolved, Unusable, is_loopback_name};

/// How long a forward may take to accept connections before it is given up.
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// ssh's own `ConnectTimeout`, in seconds: below [`READY_TIMEOUT`], so an unreachable
/// bastion is reported as such rather than as a timeout.
const CONNECT_TIMEOUT_SECS: u64 = 8;
/// ssh's `ServerAliveInterval`, in seconds, so a dead bastion ends the forward.
const SERVER_ALIVE_SECS: u64 = 15;
/// How many local ports are tried when ssh can't listen on the one picked.
const PORT_ATTEMPTS: usize = 3;
/// How often readiness is checked.
const POLL: Duration = Duration::from_millis(50);
/// How long one readiness probe may take to connect.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
/// After ssh exits, how long its standard error may take to close.
const STDERR_GRACE: Duration = Duration::from_secs(1);
/// The most of ssh's standard error kept for classifying a failure.
const STDERR_MAX: usize = 16 * 1024;
/// The longest host name accepted for the bastion.
const MAX_HOST_LEN: usize = 253;
/// `CREATE_NO_WINDOW`: no console window flashes up for ssh on Windows.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Why an SSH tunnel could not be used. Fixed texts: never ssh's own output, the SSH
/// username, the argument list, or anything from the keychain.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SshError {
    #[error(
        "the SSH bastion host is not valid: give a host name, an address or a ~/.ssh/config alias \
         of at most 253 bytes, with no spaces, no leading '-' and no '@' (give the SSH user with \
         `conn add --ssh USER@HOST`, which stores it in the OS keychain)"
    )]
    InvalidHost,
    #[error(
        "the SSH {0} path can't be passed to ssh: it must be UTF-8 and contain no '%', '\"', \
         \"${{\" or control characters; move the file"
    )]
    InvalidPath(&'static str),
    #[error("this server address can't be forwarded through the SSH bastion")]
    InvalidTarget,
    #[error(
        "the bastion's host key is not in known_hosts, or does not match it; connect once with \
         ssh to check and add it (redis-lens checks host keys strictly and never adds one)"
    )]
    HostKey,
    #[error(
        "the SSH bastion refused the login; check the SSH user and key (an encrypted key must be \
         loaded in ssh-agent: redis-lens never asks for a passphrase)"
    )]
    Auth,
    #[error(
        "could not reach the SSH bastion: its name did not resolve, or the connection was refused \
         or timed out; check the bastion's host and port"
    )]
    Unreachable,
    #[error(
        "ssh could not listen on the local end of the forward (127.0.0.1); check that no \
         LocalForward in ~/.ssh/config clashes with it, and try again"
    )]
    ForwardFailed,
    #[error("the ssh program was not found; install OpenSSH or put ssh on the PATH")]
    NotFound,
    #[error("could not run ssh ({0:?})")]
    Spawn(std::io::ErrorKind),
    #[error(
        "the SSH tunnel was not ready within {} s; check the bastion with ssh yourself",
        READY_TIMEOUT.as_secs()
    )]
    Timeout,
    #[error(
        "ssh exited before the tunnel was ready; run ssh to the bastion yourself to see why \
         (redis-lens never shows ssh's output)"
    )]
    Exited,
}

impl From<SshError> for Unusable {
    fn from(e: SshError) -> Self {
        Self::Conn(ConnError::Ssh(e))
    }
}

/// Refuse a bastion host that ssh could read as an option, a user, or more than one
/// word. IPv6 addresses are allowed, bare or in brackets.
pub(crate) fn check_host(host: &str) -> Result<(), SshError> {
    let ok = !host.is_empty()
        && host.len() <= MAX_HOST_LEN
        && !host.starts_with('-')
        && !host.contains('@')
        && !host.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(SshError::InvalidHost)
    }
}

/// The bastion host as ssh's destination: an IPv6 address without brackets (ssh would
/// take `[::1]` as a literal name), anything else as given.
fn destination(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .filter(|h| h.parse::<Ipv6Addr>().is_ok())
        .unwrap_or(host)
}

/// A file path as ssh will read it. ssh expands `%` tokens and `${VAR}` in
/// `IdentityFile` and `UserKnownHostsFile`, and checks `-i` before expanding, so a path
/// with `%` can't be given either way; `"` would end the quoting below.
pub(crate) fn check_path(path: &Path, what: &'static str) -> Result<String, SshError> {
    let s = path.to_str().ok_or(SshError::InvalidPath(what))?;
    let bad = s.is_empty()
        || s.contains('%')
        || s.contains('"')
        || s.contains("${")
        || s.chars().any(char::is_control);
    if bad {
        Err(SshError::InvalidPath(what))
    } else {
        Ok(s.to_string())
    }
}

/// `<host>:<port>` for ssh's `-L`: an IPv6 address in brackets, a name as given. Refuses
/// what ssh's forward syntax could misread.
fn forward_target(host: &str, port: u16) -> Result<String, SshError> {
    let bare = destination(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(match ip {
            IpAddr::V4(v4) => format!("{v4}:{port}"),
            IpAddr::V6(v6) => format!("[{v6}]:{port}"),
        });
    }
    let ok = !bare.is_empty()
        && bare.len() <= MAX_HOST_LEN
        && !bare.starts_with('-')
        && !bare
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, ':' | '/' | '[' | ']'));
    if ok {
        Ok(format!("{bare}:{port}"))
    } else {
        Err(SshError::InvalidTarget)
    }
}

/// The bastion as shown in `conn list` and `conn test`: its host, with the port when one
/// is set (an IPv6 address in brackets then). Never the user.
pub fn label(tunnel: &SshTunnel) -> String {
    let host = destination(&tunnel.host);
    match tunnel.port {
        Some(p) if host.contains(':') => format!("[{host}]:{p}"),
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    }
}

/// ssh's arguments for a forward from `127.0.0.1:<local>` to `target` (from
/// [`forward_target`]), in a fixed order.
fn ssh_args(
    tunnel: &SshTunnel,
    user: Option<&str>,
    local: u16,
    target: &str,
) -> Result<Vec<String>, SshError> {
    let mut a: Vec<String> = [
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ExitOnForwardFailure=yes",
    ]
    .map(String::from)
    .to_vec();
    a.extend([
        "-o".into(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "-o".into(),
        format!("ServerAliveInterval={SERVER_ALIVE_SECS}"),
        // Never share or become a connection master: a master with ControlPersist would
        // outlive the process killed on drop, and keep the forward open.
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "ControlPath=none".into(),
        "-L".into(),
        format!("127.0.0.1:{local}:{target}"),
    ]);
    if let Some(p) = tunnel.port {
        a.extend(["-p".into(), p.to_string()]);
    }
    if let Some(i) = &tunnel.identity_file {
        let i = check_path(i, "identity file")?;
        a.extend(["-i".into(), i, "-o".into(), "IdentitiesOnly=yes".into()]);
    }
    if let Some(k) = &tunnel.known_hosts_file {
        let k = check_path(k, "known_hosts file")?;
        // ssh splits this option's value at spaces unless it is quoted.
        let k = if k.chars().any(char::is_whitespace) {
            format!("\"{k}\"")
        } else {
            k
        };
        a.extend(["-o".into(), format!("UserKnownHostsFile={k}")]);
    }
    if let Some(u) = user {
        a.extend(["-l".into(), u.to_string()]);
    }
    a.push(destination(&tunnel.host).to_string());
    Ok(a)
}

/// Pick the fixed [`SshError`] for what ssh printed before it exited. Matches the
/// messages of OpenSSH (Linux, macOS and Windows builds).
fn classify(stderr: &str) -> SshError {
    let s = stderr.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| s.contains(n));
    if any(&[
        "host key verification failed",
        "remote host identification has changed",
        "host key is known for",
        "you have requested strict checking",
    ]) {
        SshError::HostKey
    } else if any(&[
        "permission denied (",
        "too many authentication failures",
        "no more authentication methods",
        "no supported authentication methods",
    ]) {
        SshError::Auth
    } else if any(&[
        "could not request local forwarding",
        "cannot listen to port",
        "address already in use",
    ]) {
        SshError::ForwardFailed
    } else if any(&[
        "could not resolve hostname",
        "name or service not known",
        "nodename nor servname",
        "temporary failure in name resolution",
        "no address associated with hostname",
        "connection refused",
        "timed out",
        "no route to host",
        "network is unreachable",
        "host is down",
        "host is unreachable",
    ]) {
        SshError::Unreachable
    } else {
        SshError::Exited
    }
}

/// What the standard-error reader has collected.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    /// Standard error reached its end.
    closed: bool,
    /// Once the forward is ready, nothing more is kept.
    discard: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One running forward. Dropping it kills and reaps the ssh process.
struct Forward {
    local: SocketAddr,
    child: Child,
    stderr: Arc<Mutex<Captured>>,
}

impl Forward {
    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Slot = Arc<tokio::sync::Mutex<Option<Forward>>>;

/// A profile's bastion and the forwards opened through it so far.
pub struct Bastion {
    /// Boxed to keep [`super::Route`] small.
    tunnel: Box<SshTunnel>,
    /// From the keychain. Never shown: see the `Debug` impl.
    user: Option<String>,
    /// The program and any arguments before ssh's own: `["ssh"]`, or a fake in tests.
    launcher: Vec<OsString>,
    ready_timeout: Duration,
    /// One forward per (host, port) as announced, lowercased.
    forwards: Mutex<HashMap<(String, u16), Slot>>,
}

/// Shows the bastion's host and port and how many forwards it has; whether a user is
/// set, but never which.
impl std::fmt::Debug for Bastion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bastion")
            .field("host", &self.tunnel.host)
            .field("port", &self.tunnel.port)
            .field(
                "user",
                &if self.user.is_some() {
                    "<in the keychain>"
                } else {
                    "none"
                },
            )
            .field("forwards", &lock(&self.forwards).len())
            .finish_non_exhaustive()
    }
}

impl Bastion {
    /// Prepare the bastion for `profile`: check its host and paths and read its SSH
    /// username from the keychain ([`Profile::ssh_secret_ref`]; no entry means none).
    /// Starts no process.
    pub fn new(
        profile: &Profile,
        tunnel: &SshTunnel,
        secrets: &dyn SecretStore,
    ) -> Result<Self, ConnError> {
        check_host(&tunnel.host)?;
        if let Some(p) = &tunnel.identity_file {
            check_path(p, "identity file")?;
        }
        if let Some(p) = &tunnel.known_hosts_file {
            check_path(p, "known_hosts file")?;
        }
        let user = secrets
            .get(&profile.ssh_secret_ref())?
            .and_then(|c| c.username)
            .filter(|u| !u.is_empty());
        Ok(Self {
            tunnel: Box::new(tunnel.clone()),
            user,
            launcher: vec![OsString::from("ssh")],
            ready_timeout: READY_TIMEOUT,
            forwards: Mutex::new(HashMap::new()),
        })
    }

    /// Run `launcher` (a program and its first arguments) instead of `ssh`. Only the
    /// Unix tests use it: they launch shell and perl stand-ins.
    #[cfg(all(test, unix))]
    pub(crate) fn with_launcher(mut self, launcher: Vec<OsString>, ready: Duration) -> Self {
        self.launcher = launcher;
        self.ready_timeout = ready;
        self
    }

    /// The local end of the forward to `ep`, started if there is none yet (or the last
    /// one died), as the address to pin connections to. `local` is judged from the
    /// bastion's side: `ep`'s host is loopback by name (see `is_loopback_name`), since
    /// its addresses are resolved on the bastion, not here.
    pub(super) async fn reach(&self, ep: &Endpoint) -> Result<Resolved, Unusable> {
        let target = forward_target(&ep.host, ep.port)?;
        let slot = {
            let mut map = lock(&self.forwards);
            Arc::clone(
                map.entry((ep.host.to_ascii_lowercase(), ep.port))
                    .or_default(),
            )
        };
        let mut guard = slot.lock().await;
        let alive = guard.as_mut().map(|f| (f.running(), f.local));
        let local = match alive {
            Some((true, local)) => local,
            _ => {
                // A dead forward is reaped before a new one starts.
                *guard = None;
                let f = self.start(&target).await?;
                let local = f.local;
                *guard = Some(f);
                local
            }
        };
        Ok(Resolved {
            addrs: vec![local],
            local: is_loopback_name(&ep.host),
        })
    }

    /// Start a forward to `target`, trying another local port when ssh can't listen on
    /// the one picked.
    async fn start(&self, target: &str) -> Result<Forward, SshError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.start_on(free_port()?, target).await {
                Err(SshError::ForwardFailed) if attempt < PORT_ATTEMPTS => {}
                r => return r,
            }
        }
    }

    async fn start_on(&self, local: SocketAddr, target: &str) -> Result<Forward, SshError> {
        let args = ssh_args(&self.tunnel, self.user.as_deref(), local.port(), target)?;
        let (program, before) = self.launcher.split_first().ok_or(SshError::NotFound)?;
        let mut cmd = Command::new(program);
        cmd.args(before)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SshError::NotFound,
            kind => SshError::Spawn(kind),
        })?;
        let pipe = child.stderr.take();
        // From here on, dropping `forward` (an error, or the caller giving up) kills ssh.
        let mut forward = Forward {
            local,
            child,
            stderr: Arc::new(Mutex::new(Captured::default())),
        };
        if let Some(pipe) = pipe {
            read_stderr(pipe, Arc::clone(&forward.stderr))?;
        }

        let deadline = tokio::time::Instant::now() + self.ready_timeout;
        loop {
            match forward.child.try_wait() {
                Ok(Some(_)) => return Err(forward.failure().await),
                Ok(None) => {}
                Err(e) => return Err(SshError::Spawn(e.kind())),
            }
            let probe = tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(local));
            if let Ok(Ok(stream)) = probe.await {
                drop(stream);
                if forward.running() {
                    let mut c = lock(&forward.stderr);
                    c.discard = true;
                    c.bytes = Vec::new();
                    drop(c);
                    return Ok(forward);
                }
                continue;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SshError::Timeout);
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

impl Forward {
    /// Why ssh exited, from what it printed: waits a moment for its standard error to
    /// close, then classifies it. The text goes no further.
    async fn failure(&self) -> SshError {
        let until = tokio::time::Instant::now() + STDERR_GRACE;
        while !lock(&self.stderr).closed && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let bytes = std::mem::take(&mut lock(&self.stderr).bytes);
        classify(&String::from_utf8_lossy(&bytes))
    }
}

/// A port on 127.0.0.1 that was free a moment ago.
fn free_port() -> Result<SocketAddr, SshError> {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .map_err(|_| SshError::ForwardFailed)
}

/// Copy ssh's standard error into `sink` on a helper thread until it closes, so ssh
/// never blocks on a full pipe. At most [`STDERR_MAX`] bytes are kept, and nothing once
/// the forward is ready.
fn read_stderr(
    mut pipe: impl Read + Send + 'static,
    sink: Arc<Mutex<Captured>>,
) -> Result<(), SshError> {
    std::thread::Builder::new()
        .name("ssh-stderr".into())
        .spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut c = lock(&sink);
                        if !c.discard {
                            let room = STDERR_MAX.saturating_sub(c.bytes.len());
                            c.bytes.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            lock(&sink).closed = true;
        })
        .map(drop)
        .map_err(|e| SshError::Spawn(e.kind()))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::model::{Auth, Credentials, Env};
    use crate::secrets::MemoryStore;
    use std::path::PathBuf;

    pub(crate) const USER: &str = "tunnel-user-q7z";

    pub(crate) fn tunnel(host: &str) -> SshTunnel {
        SshTunnel {
            host: host.into(),
            port: None,
            identity_file: None,
            known_hosts_file: None,
        }
    }

    pub(crate) fn profile(t: Option<SshTunnel>) -> Profile {
        Profile {
            name: "behind".into(),
            env: Env::Prod,
            endpoint: Endpoint {
                host: "10.0.0.5".into(),
                port: 6379,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: t,
            sentinel: None,
            tags: Vec::new(),
        }
    }

    pub(crate) fn store_with_user(p: &Profile, user: &str) -> MemoryStore {
        let s = MemoryStore::default();
        s.set(
            &p.ssh_secret_ref(),
            &Credentials {
                username: Some(user.into()),
                password: secrecy::SecretString::from(String::new()),
            },
        )
        .unwrap();
        s
    }

    fn args(t: &SshTunnel, user: Option<&str>) -> Vec<String> {
        ssh_args(t, user, 40001, "10.0.0.5:6379").unwrap()
    }

    const BASE: [&str; 17] = [
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-L",
        "127.0.0.1:40001:10.0.0.5:6379",
    ];

    fn expect(extra: &[&str]) -> Vec<String> {
        BASE.iter().chain(extra).map(|s| s.to_string()).collect()
    }

    #[test]
    fn argv_for_each_combination() {
        let mut t = tunnel("bastion.example.com");
        assert_eq!(args(&t, None), expect(&["bastion.example.com"]));
        t.port = Some(2222);
        assert_eq!(
            args(&t, None),
            expect(&["-p", "2222", "bastion.example.com"])
        );
        t.port = None;
        t.identity_file = Some("/keys/id_ed25519".into());
        assert_eq!(
            args(&t, None),
            expect(&[
                "-i",
                "/keys/id_ed25519",
                "-o",
                "IdentitiesOnly=yes",
                "bastion.example.com"
            ])
        );
        t.identity_file = None;
        t.known_hosts_file = Some("/keys/known_hosts".into());
        assert_eq!(
            args(&t, None),
            expect(&[
                "-o",
                "UserKnownHostsFile=/keys/known_hosts",
                "bastion.example.com"
            ])
        );
        t.known_hosts_file = None;
        assert_eq!(
            args(&t, Some("ops")),
            expect(&["-l", "ops", "bastion.example.com"])
        );
        let all = SshTunnel {
            host: "bastion.example.com".into(),
            port: Some(2222),
            identity_file: Some("/keys/id".into()),
            known_hosts_file: Some("/keys/kh".into()),
        };
        assert_eq!(
            args(&all, Some("ops")),
            expect(&[
                "-p",
                "2222",
                "-i",
                "/keys/id",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "UserKnownHostsFile=/keys/kh",
                "-l",
                "ops",
                "bastion.example.com"
            ])
        );
    }

    #[test]
    fn argv_for_ipv6_and_awkward_paths() {
        // An IPv6 bastion goes to ssh bare, an IPv6 target in brackets.
        let mut t = tunnel("[2001:db8::1]");
        let a = ssh_args(&t, None, 40001, &forward_target("::1", 6379).unwrap()).unwrap();
        assert_eq!(a[16], "127.0.0.1:40001:[::1]:6379");
        assert_eq!(a.last().unwrap(), "2001:db8::1");
        t.host = "fe80::1".into();
        assert_eq!(args(&t, None).last().unwrap(), "fe80::1");
        // A known_hosts path with a space is quoted; `-i` is one argument as it is.
        t.known_hosts_file = Some("/Users/me/Library/Application Support/kh".into());
        t.identity_file = Some("/Users/me/my keys/id".into());
        let a = args(&t, None);
        assert!(a.contains(&"/Users/me/my keys/id".to_string()), "{a:?}");
        assert!(
            a.contains(&"UserKnownHostsFile=\"/Users/me/Library/Application Support/kh\"".into()),
            "{a:?}"
        );
        for bad in ["/k/%h", "/k/\"x", "/k/${HOME}", "/k/a\nb", ""] {
            t.known_hosts_file = Some(bad.into());
            assert_eq!(
                ssh_args(&t, None, 1, "h:1"),
                Err(SshError::InvalidPath("known_hosts file")),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn forward_targets() {
        assert_eq!(forward_target("10.0.0.5", 6379).unwrap(), "10.0.0.5:6379");
        assert_eq!(
            forward_target("2001:DB8::5", 6379).unwrap(),
            "[2001:db8::5]:6379"
        );
        assert_eq!(forward_target("[::1]", 1).unwrap(), "[::1]:1");
        assert_eq!(
            forward_target("cache.internal", 6380).unwrap(),
            "cache.internal:6380"
        );
        for bad in ["", "a b", "-x", "a:b", "a/b", "[x]", "a\0"] {
            assert_eq!(
                forward_target(bad, 1),
                Err(SshError::InvalidTarget),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn hosts() {
        for ok in [
            "bastion.example.com",
            "jump",
            "10.0.0.1",
            "::1",
            "[2001:db8::1]",
            &"a".repeat(253),
        ] {
            assert_eq!(check_host(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "user@host",
            "two words",
            "tab\there",
            "nl\n",
            &"a".repeat(254),
        ] {
            assert_eq!(check_host(bad), Err(SshError::InvalidHost), "{bad:?}");
        }
        assert_eq!(label(&tunnel("b.example")), "b.example");
        let mut t = tunnel("::1");
        assert_eq!(label(&t), "::1");
        t.port = Some(2222);
        assert_eq!(label(&t), "[::1]:2222");
        t.host = "[::1]".into();
        assert_eq!(label(&t), "[::1]:2222");
        t.host = "b.example".into();
        assert_eq!(label(&t), "b.example:2222");
    }

    /// Real OpenSSH messages (9.x and 10.x on Linux and macOS).
    #[test]
    fn stderr_is_classified() {
        let cases = [
            (
                "No ED25519 host key is known for [127.0.0.1]:32771 and you have requested strict checking.\r\nHost key verification failed.\r\n",
                SshError::HostKey,
            ),
            (
                "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\nIT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\r\nHost key for [127.0.0.1]:32771 has changed and you have requested strict checking.\r\nHost key verification failed.\r\n",
                SshError::HostKey,
            ),
            (
                "tunnel-user-q7z@127.0.0.1: Permission denied (publickey).\r\n",
                SshError::Auth,
            ),
            (
                "Warning: Identity file /nope not accessible: No such file or directory.\nops@b: Permission denied (publickey,keyboard-interactive).\n",
                SshError::Auth,
            ),
            (
                "Received disconnect from 10.0.0.1 port 22:2: Too many authentication failures\r\nDisconnected from 10.0.0.1 port 22\r\n",
                SshError::Auth,
            ),
            (
                "bind [127.0.0.1]:41234: Address already in use\r\nchannel_setup_fwd_listener_tcpip: cannot listen to port: 41234\r\nCould not request local forwarding.\r\n",
                SshError::ForwardFailed,
            ),
            (
                "ssh: Could not resolve hostname nope.invalid: nodename nor servname provided, or not known\r\n",
                SshError::Unreachable,
            ),
            (
                "ssh: Could not resolve hostname nope.invalid: Name or service not known\r\n",
                SshError::Unreachable,
            ),
            (
                "ssh: Could not resolve hostname bastion: Temporary failure in name resolution\r\n",
                SshError::Unreachable,
            ),
            (
                "ssh: connect to host 127.0.0.1 port 1: Connection refused\r\n",
                SshError::Unreachable,
            ),
            (
                "ssh: connect to host 10.255.255.1 port 22: Operation timed out\r\n",
                SshError::Unreachable,
            ),
            (
                "ssh: connect to host 10.255.255.1 port 22: Connection timed out\r\n",
                SshError::Unreachable,
            ),
            (
                "Connection timed out during banner exchange\r\nConnection to 10.0.0.9 port 22 timed out\r\n",
                SshError::Unreachable,
            ),
            (
                "ssh: connect to host 10.0.0.9 port 22: No route to host\r\n",
                SshError::Unreachable,
            ),
            (
                "Unable to negotiate with 10.0.0.9 port 22: no matching host key type found. Their offer: ssh-dss\r\n",
                SshError::Exited,
            ),
            (
                "Bad owner or permissions on /home/u/.ssh/config\r\n",
                SshError::Exited,
            ),
            ("", SshError::Exited),
        ];
        for (text, want) in cases {
            assert_eq!(classify(text), want, "{text}");
        }
    }

    #[test]
    fn errors_and_debug_never_show_the_user() {
        let t = SshTunnel {
            host: "b.example".into(),
            port: Some(2222),
            identity_file: Some("/k/id".into()),
            known_hosts_file: None,
        };
        let p = profile(Some(t.clone()));
        let b = Bastion::new(&p, &t, &store_with_user(&p, USER)).unwrap();
        assert_eq!(b.user.as_deref(), Some(USER));
        let dbg = format!("{b:?}");
        assert!(!dbg.contains(USER), "{dbg}");
        assert!(
            dbg.contains("b.example") && dbg.contains("keychain"),
            "{dbg}"
        );
        let all = [
            SshError::InvalidHost,
            SshError::InvalidPath("identity file"),
            SshError::InvalidTarget,
            SshError::HostKey,
            SshError::Auth,
            SshError::Unreachable,
            SshError::ForwardFailed,
            SshError::NotFound,
            SshError::Spawn(std::io::ErrorKind::PermissionDenied),
            SshError::Timeout,
            SshError::Exited,
        ];
        for e in all {
            for text in [
                e.to_string(),
                format!("{e:?}"),
                ConnError::Ssh(e).to_string(),
            ] {
                assert!(!text.contains(USER), "{text}");
            }
        }
        // The classifier's result for a message that names the user doesn't either.
        let e = classify(&format!("{USER}@b.example: Permission denied (publickey)."));
        assert!(!e.to_string().contains(USER));
        assert!(e.to_string().contains("refused the login"), "{e}");
        assert!(
            SshError::HostKey
                .to_string()
                .contains("connect once with ssh")
        );
        // No keychain entry: no user, and no `-l`.
        let none = Bastion::new(&p, &t, &MemoryStore::default()).unwrap();
        assert_eq!(none.user, None);
        assert!(format!("{none:?}").contains("none"));
    }

    #[test]
    fn new_checks_the_host_and_paths() {
        let s = MemoryStore::default();
        let bad = tunnel("u@b.example");
        let e = Bastion::new(&profile(Some(bad.clone())), &bad, &s).unwrap_err();
        assert!(matches!(e, ConnError::Ssh(SshError::InvalidHost)), "{e:?}");
        let mut t = tunnel("b.example");
        t.identity_file = Some("/k/%d".into());
        let e = Bastion::new(&profile(Some(t.clone())), &t, &s).unwrap_err();
        assert!(
            matches!(e, ConnError::Ssh(SshError::InvalidPath("identity file"))),
            "{e:?}"
        );
        // Paths are kept as given: not resolved, not required to exist here.
        let mut t = tunnel("b.example");
        t.known_hosts_file = Some("relative/kh".into());
        let b = Bastion::new(&profile(Some(t.clone())), &t, &s).unwrap();
        assert_eq!(
            b.tunnel.known_hosts_file,
            Some(PathBuf::from("relative/kh"))
        );
    }

    /// A fake `ssh` (a Perl script, run as `perl <script> <dir> <mode> <ssh args…>`) that
    /// records each start and its arguments under `dir`, then behaves as `mode` says.
    #[cfg(unix)]
    pub(crate) mod fake {
        use super::*;

        const SCRIPT: &str = r#"
use strict; use warnings; use IO::Socket::INET;
my $dir = shift @ARGV; my $mode = shift @ARGV;
open(my $s, '>>', "$dir/starts") or die; print $s "$$\n"; close $s;
open(my $a, '>', "$dir/argv-$$") or die; print $a join("\0", @ARGV); close $a;
my $n = 0; if (open(my $c, '<', "$dir/starts")) { $n++ while <$c>; close $c; }
if ($mode eq 'fail') { print STDERR "No ED25519 host key is known for [127.0.0.1]:2222 and you have requested strict checking.\r\nHost key verification failed.\r\n"; exit 255; }
if ($mode eq 'exit0') { exit 0; }
if ($mode eq 'hang') { sleep 600; exit 0; }
if ($mode eq 'collide' || ($mode eq 'collide-once' && $n == 1)) { print STDERR "bind [127.0.0.1]:1: Address already in use\r\nCould not request local forwarding.\r\n"; exit 255; }
my ($spec) = map { $ARGV[$_ + 1] } grep { $ARGV[$_] eq '-L' } 0 .. $#ARGV;
my (undef, $port) = split /:/, $spec;
my $l = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => $port, Listen => 16, ReuseAddr => 1) or exit 255;
while (my $conn = $l->accept) { open(my $x, '>>', "$dir/accepts-$$"); print $x "1\n"; close $x; close $conn; }
"#;

        pub(crate) struct Fake {
            pub dir: PathBuf,
            script: PathBuf,
        }

        impl Fake {
            pub(crate) fn new(tag: &str) -> Self {
                let dir = std::env::temp_dir().join(format!(
                    "redis-lens-fake-ssh-{tag}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir_all(&dir).unwrap();
                let script = dir.join("fake-ssh.pl");
                std::fs::write(&script, SCRIPT).unwrap();
                Self { dir, script }
            }

            pub(crate) fn launcher(&self, mode: &str) -> Vec<OsString> {
                vec![
                    "perl".into(),
                    self.script.clone().into(),
                    self.dir.clone().into(),
                    mode.into(),
                ]
            }

            pub(crate) fn bastion(
                &self,
                mode: &str,
                user: Option<&str>,
                ready: Duration,
            ) -> Bastion {
                let mut t = tunnel("b.example");
                t.port = Some(2222);
                let p = profile(Some(t.clone()));
                let store = match user {
                    Some(u) => store_with_user(&p, u),
                    None => MemoryStore::default(),
                };
                Bastion::new(&p, &t, &store)
                    .unwrap()
                    .with_launcher(self.launcher(mode), ready)
            }

            /// The pids of every start so far, in order.
            pub(crate) fn starts(&self) -> Vec<u32> {
                std::fs::read_to_string(self.dir.join("starts"))
                    .unwrap_or_default()
                    .lines()
                    .map(|l| l.parse().unwrap())
                    .collect()
            }

            /// How many connections the forward started as `pid` accepted.
            pub(crate) fn accepts(&self, pid: u32) -> usize {
                std::fs::read_to_string(self.dir.join(format!("accepts-{pid}")))
                    .unwrap_or_default()
                    .lines()
                    .count()
            }

            pub(crate) fn argv(&self, pid: u32) -> Vec<String> {
                std::fs::read_to_string(self.dir.join(format!("argv-{pid}")))
                    .unwrap()
                    .split('\0')
                    .map(String::from)
                    .collect()
            }
        }

        impl Drop for Fake {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        pub(crate) fn alive(pid: u32) -> bool {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        }
    }

    #[cfg(unix)]
    fn ep(host: &str, port: u16) -> Endpoint {
        Endpoint {
            host: host.into(),
            port,
            tls: false,
        }
    }

    #[cfg(unix)]
    fn ssh_err(r: Result<Resolved, Unusable>) -> SshError {
        match r {
            Err(Unusable::Conn(ConnError::Ssh(e))) => e,
            Err(_) => panic!("not an SSH error"),
            Ok(r) => panic!("expected an error, got {:?}", r.addrs),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_ready_forward_is_reused_and_killed_on_drop() {
        let fake = fake::Fake::new("reuse");
        let b = fake.bastion("listen", Some(USER), READY_TIMEOUT);
        let target = ep("10.0.0.5", 6379);
        let first = b.reach(&target).await.map_err(|_| ()).unwrap();
        assert_eq!(first.addrs.len(), 1);
        assert_eq!(first.addrs[0].ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        // Judged from the bastion: 10.0.0.5 is not loopback, although the forward is.
        assert!(!first.local);
        // The forward accepts connections.
        tokio::net::TcpStream::connect(first.addrs[0])
            .await
            .unwrap();
        let again = b
            .reach(&ep("10.0.0.5", 6379))
            .await
            .map_err(|_| ())
            .unwrap();
        assert_eq!(again.addrs, first.addrs);
        assert_eq!(fake.starts().len(), 1);

        // Another endpoint gets its own forward; loopback as seen from the bastion.
        let other = b
            .reach(&ep("127.0.0.1", 6380))
            .await
            .map_err(|_| ())
            .unwrap();
        assert!(other.local);
        assert_ne!(other.addrs, first.addrs);
        let pids = fake.starts();
        assert_eq!(pids.len(), 2);

        // The exact argv ssh got, user included (the known exposure, see the module).
        let argv = fake.argv(pids[0]);
        let mut t = tunnel("b.example");
        t.port = Some(2222);
        let want = ssh_args(&t, Some(USER), first.addrs[0].port(), "10.0.0.5:6379").unwrap();
        assert_eq!(argv, want);
        assert!(fake.argv(pids[1]).contains(&format!(
            "127.0.0.1:{}:127.0.0.1:6380",
            other.addrs[0].port()
        )));

        for &pid in &pids {
            assert!(fake::alive(pid));
        }
        drop(b);
        for pid in pids {
            assert!(!fake::alive(pid), "{pid} still running");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_reaches_start_one_forward() {
        let fake = fake::Fake::new("concurrent");
        let b = fake.bastion("listen", None, READY_TIMEOUT);
        let target = ep("cache.internal", 6379);
        let (x, y) = tokio::join!(b.reach(&target), b.reach(&target));
        let (x, y) = (x.map_err(|_| ()).unwrap(), y.map_err(|_| ()).unwrap());
        assert_eq!(x.addrs, y.addrs);
        // The same name in another case is the same forward.
        let upper = b
            .reach(&ep("Cache.INTERNAL", 6379))
            .await
            .map_err(|_| ())
            .unwrap();
        assert_eq!(upper.addrs, x.addrs);
        let pids = fake.starts();
        assert_eq!(pids.len(), 1);
        // No user: no `-l`.
        assert!(!fake.argv(pids[0]).contains(&"-l".to_string()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dead_forward_is_started_again() {
        let fake = fake::Fake::new("restart");
        let b = fake.bastion("listen", None, READY_TIMEOUT);
        let target = ep("10.0.0.5", 6379);
        b.reach(&target).await.map_err(|_| ()).unwrap();
        let pid = fake.starts()[0];
        std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut restarted = false;
        while std::time::Instant::now() < deadline {
            b.reach(&target).await.map_err(|_| ()).unwrap();
            if fake.starts().len() == 2 {
                restarted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(restarted, "the dead forward was not replaced");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn early_exit_is_classified_and_never_echoed() {
        let fake = fake::Fake::new("fail");
        let b = fake.bastion("fail", Some(USER), READY_TIMEOUT);
        let e = ssh_err(b.reach(&ep("10.0.0.5", 6379)).await);
        assert_eq!(e, SshError::HostKey);
        assert!(!e.to_string().contains("127.0.0.1]:2222"), "{e}");
        // A failed start is not kept: the next reach starts ssh again.
        let e = ssh_err(b.reach(&ep("10.0.0.5", 6379)).await);
        assert_eq!(e, SshError::HostKey);
        assert_eq!(fake.starts().len(), 2);

        let quiet = fake.bastion("exit0", None, READY_TIMEOUT);
        assert_eq!(
            ssh_err(quiet.reach(&ep("10.0.0.5", 6379)).await),
            SshError::Exited
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_forward_that_never_gets_ready_times_out_and_is_killed() {
        let fake = fake::Fake::new("hang");
        // Long enough for a loaded CI runner to start the fake and record its pid
        // before the readiness timeout kills it (400 ms was not).
        let b = fake.bastion("hang", None, Duration::from_secs(2));
        let started = std::time::Instant::now();
        let e = ssh_err(b.reach(&ep("10.0.0.5", 6379)).await);
        assert_eq!(e, SshError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(8));
        let pids = fake.starts();
        assert_eq!(pids.len(), 1, "the fake ssh never recorded its start");
        assert!(!fake::alive(pids[0]), "the timed-out ssh is still running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_start_kills_ssh() {
        let fake = fake::Fake::new("cancel");
        let b = fake.bastion("hang", None, READY_TIMEOUT);
        let target = ep("10.0.0.5", 6379);
        let mut reach = Box::pin(b.reach(&target));
        // Wait for ssh to have started (a loaded CI runner can take more than a few
        // hundred ms), then abandon the start by dropping it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fake.starts().is_empty() {
            assert!(std::time::Instant::now() < deadline, "ssh never started");
            tokio::select! {
                r = &mut reach => panic!("the hanging bastion answered (ok: {})", r.is_ok()),
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
        drop(reach);
        let pid = fake.starts()[0];
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while fake::alive(pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!fake::alive(pid), "the abandoned ssh is still running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_port_collision_is_retried() {
        let fake = fake::Fake::new("collide");
        let b = fake.bastion("collide-once", None, READY_TIMEOUT);
        b.reach(&ep("10.0.0.5", 6379))
            .await
            .map_err(|_| ())
            .unwrap();
        assert_eq!(fake.starts().len(), 2);
        let always = fake::Fake::new("collide-always");
        let b = always.bastion("collide", None, READY_TIMEOUT);
        assert_eq!(
            ssh_err(b.reach(&ep("10.0.0.5", 6379)).await),
            SshError::ForwardFailed
        );
        assert_eq!(always.starts().len(), PORT_ATTEMPTS);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_ssh_program_is_not_found() {
        let fake = fake::Fake::new("missing");
        let b = fake
            .bastion("listen", None, READY_TIMEOUT)
            .with_launcher(vec!["/nonexistent/redis-lens/ssh".into()], READY_TIMEOUT);
        assert_eq!(
            ssh_err(b.reach(&ep("10.0.0.5", 6379)).await),
            SshError::NotFound
        );
        // An address ssh's forward syntax can't carry is refused before anything runs.
        let e = match b.reach(&ep("a/b", 1)).await {
            Err(Unusable::Conn(ConnError::Ssh(e))) => e,
            _ => panic!("expected an error"),
        };
        assert_eq!(e, SshError::InvalidTarget);
    }
}
