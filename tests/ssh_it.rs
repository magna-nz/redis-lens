//! Reaching servers through an SSH bastion (`Profile::ssh`), on Redis and Valkey.
//!
//! Every test runs an sshd container (see `common::ssh`) on its own Docker network with
//! only its SSH port published. The servers join the network unpublished, under
//! container names that only resolve inside it, so the only way to them is a forward
//! through the bastion. Each test makes its own ed25519 key and a known_hosts file with
//! the bastion's host key as read from the container.
#![cfg(feature = "integration-tests")]

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::ssh::{self as bastion, HiddenServer, Key, Place, SSH_USER, SshBastion};
use common::{ServerTls, TestCa};
use redis_lens::cli::ConnAddArgs;
use redis_lens::conn::ssh::SshError;
use redis_lens::conn::{self, ConnError, Replication, ScanTarget, check_permissions};
use redis_lens::model::{Auth, Credentials, Endpoint, Env, PermStatus, Profile, SshTunnel};
use redis_lens::scan::{self, ScanOptions};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::SecretString;

const KEYS: u64 = 30;
const LENS: &str = "lens";
const LENS_PW: &str = "lens-pw";
/// An ACL user `lens` and the default user off, so every command needs `AUTH`.
const LENS_ONLY: [&str; 10] = [
    "--user", "lens", "on", ">lens-pw", "~*", "&*", "+@all", "--user", "default", "off",
];
/// What a replica of a [`LENS_ONLY`] primary needs: a login for the replication link.
const MASTER_AUTH: [&str; 4] = ["--masteruser", "lens", "--masterauth", "lens-pw"];
/// `$CLI` logged in as `lens`.
const AS_LENS: &str = "$CLI --user lens --pass lens-pw";

/// One test's bastion, key, known_hosts file and network.
struct World {
    dir: PathBuf,
    key: Key,
    known_hosts: PathBuf,
    bastion: SshBastion,
}

impl World {
    async fn new(tag: &str) -> Self {
        let dir = bastion::temp_dir(tag);
        let key = bastion::keygen(&dir, "id_ed25519");
        let network = bastion::unique(tag);
        let bastion = SshBastion::start(&network, &key).await;
        let known_hosts = bastion.known_hosts(&dir);
        Self {
            dir,
            key,
            known_hosts,
            bastion,
        }
    }

    fn tunnel(&self) -> SshTunnel {
        self.bastion.tunnel(&self.key, &self.known_hosts)
    }

    /// A server on the network under a fresh container name.
    async fn server(&self, valkey: bool, args: &[&str]) -> HiddenServer {
        let name = bastion::unique("srv");
        HiddenServer::start(
            Place::Network(&self.bastion.network, &name),
            valkey,
            6379,
            args,
        )
        .await
    }

    /// A profile for `ep` through this bastion.
    fn profile(&self, ep: Endpoint, auth: Auth, prefer_replica: bool) -> Profile {
        Profile {
            name: "behind".into(),
            env: Env::Prod,
            endpoint: ep,
            db: 0,
            auth,
            read_only: None,
            prefer_replica,
            provider: None,
            token_command: None,
            ssh: Some(self.tunnel()),
            sentinel: None,
        }
    }

    /// A store with the SSH user, plus `lens` for keychain profiles.
    fn store(&self, p: &Profile) -> MemoryStore {
        let s = MemoryStore::default();
        s.set(
            &p.ssh_secret_ref(),
            &Credentials {
                username: Some(SSH_USER.into()),
                password: SecretString::from(String::new()),
            },
        )
        .unwrap();
        if p.auth == Auth::Keychain {
            s.set(
                &p.secret_ref(),
                &Credentials {
                    username: Some(LENS.into()),
                    password: SecretString::from(LENS_PW.to_string()),
                },
            )
            .unwrap();
        }
        s
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn seed(server: &HiddenServer, cli: &str) {
    server
        .sh(&format!(
            "for i in $(seq 1 {KEYS}); do echo \"SET user:$i:profile x\"; done | {cli} > /dev/null"
        ))
        .await;
    let n = server.sh(&format!("{cli} DBSIZE")).await;
    assert_eq!(n.trim(), KEYS.to_string());
}

async fn wait_for_replication(primary: &HiddenServer, replica: &HiddenServer, cli: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let up = replica
            .sh(&format!("{cli} INFO replication"))
            .await
            .contains("master_link_status:up");
        let listed = matches!(
            conn::parse_replication(&primary.sh(&format!("{cli} INFO replication")).await),
            Replication::Master { online } if !online.is_empty()
        );
        if up && listed {
            return;
        }
        assert!(Instant::now() < deadline, "replication did not come up");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn info(c: &mut conn::Connection) -> String {
    redis::cmd("INFO")
        .arg("replication")
        .query_async(c)
        .await
        .unwrap()
}

async fn scan_all(p: &Profile, c: &mut conn::Connection) -> redis_lens::model::ScanReport {
    let perms = check_permissions(c).await;
    for check in &perms.checks {
        if check.label != redis_lens::model::PERM_FREQ {
            assert_eq!(check.status, PermStatus::Allowed, "{}", check.label);
        }
    }
    scan::scan(&p.name, c, &ScanOptions::for_profile(p), &perms, None)
        .await
        .unwrap()
}

fn expect_ssh_error(r: Result<impl Sized, ConnError>) -> SshError {
    match r {
        Err(ConnError::Ssh(e)) => {
            let text = e.to_string();
            assert!(!text.contains(SSH_USER), "{text}");
            assert!(!format!("{e:?}").contains(SSH_USER));
            e
        }
        Err(e) => panic!("expected an SSH error, got {e}"),
        Ok(_) => panic!("expected an SSH error"),
    }
}

// --- A scan through the bastion -------------------------------------------------------

async fn scans_through_the_bastion(valkey: bool) {
    let w = World::new("scan").await;
    let server = w.server(valkey, &[]).await;
    seed(&server, "$CLI").await;
    let p = w.profile(server.endpoint(), Auth::None, false);
    let store = w.store(&p);

    // The container name means nothing here: without the bastion there is no way in.
    let direct = Profile {
        ssh: None,
        sentinel: None,
        ..p.clone()
    };
    assert!(conn::connect(&direct, &store).await.is_err());

    let (mut c, target) = conn::connect_for_scan(&p, &store).await.unwrap();
    assert_eq!(target, ScanTarget::Primary);
    assert_eq!(bastion::ssh_processes(w.bastion.port), 1);
    let report = scan_all(&p, &mut c).await;
    assert_eq!(report.sampled_keys, KEYS, "valkey={valkey}");
    assert_eq!(report.patterns[0].pattern, "user:{id}:profile");

    // The forward goes away with the connection (and the route it holds).
    let clone = c.clone();
    drop(c);
    assert_eq!(
        bastion::ssh_processes(w.bastion.port),
        1,
        "a clone keeps it"
    );
    drop(clone);
    assert_eq!(
        bastion::wait_for_ssh_processes(w.bastion.port, 0).await,
        0,
        "the ssh process outlived its connections"
    );
}

#[tokio::test]
async fn scans_through_the_bastion_redis() {
    scans_through_the_bastion(false).await;
}

#[tokio::test]
async fn scans_through_the_bastion_valkey() {
    scans_through_the_bastion(true).await;
}

// --- ACL auth through the bastion ------------------------------------------------------

async fn acl_auth_through_the_bastion(valkey: bool) {
    let w = World::new("acl").await;
    let server = w.server(valkey, &LENS_ONLY).await;
    seed(&server, AS_LENS).await;
    let p = w.profile(server.endpoint(), Auth::Keychain, false);
    let store = w.store(&p);
    let mut c = conn::connect(&p, &store).await.unwrap();
    let who: String = redis::cmd("ACL")
        .arg("WHOAMI")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(who, LENS);
    let report = scan_all(&p, &mut c).await;
    assert_eq!(report.sampled_keys, KEYS, "valkey={valkey}");

    // Without the credentials the server refuses, through the same kind of tunnel.
    let none = Profile {
        auth: Auth::None,
        ..p.clone()
    };
    let mut c = conn::connect(&none, &store).await.unwrap();
    let e = redis::cmd("DBSIZE")
        .query_async::<u64>(&mut c)
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some("NOAUTH"), "{e}");
}

#[tokio::test]
async fn acl_auth_through_the_bastion_redis() {
    acl_auth_through_the_bastion(false).await;
}

#[tokio::test]
async fn acl_auth_through_the_bastion_valkey() {
    acl_auth_through_the_bastion(true).await;
}

// --- conn add / list / test / remove ---------------------------------------------------

fn add_args(name: &str, url: &str, ssh: &str, w: &World) -> ConnAddArgs {
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
        ssh: Some(ssh.into()),
        ssh_identity: Some(w.key.private.clone()),
        ssh_known_hosts: Some(w.known_hosts.clone()),
        sentinel: None,
        sentinel_master: None,
        sentinel_tls: false,
        sentinel_user: None,
        sentinel_password_stdin: false,
        sentinel_ask_password: false,
    }
}

async fn conn_commands_through_the_bastion(valkey: bool) {
    use redis_lens::cli::conn as cli;
    let w = World::new("cli").await;
    let server = w.server(valkey, &LENS_ONLY).await;
    seed(&server, AS_LENS).await;
    let config = w.dir.join("config.toml");
    let store = MemoryStore::default();
    let url = format!("redis://{LENS}@{}:6379", server.host);
    let ssh = format!("{SSH_USER}@127.0.0.1:{}", w.bastion.port);
    let mut a = add_args("behind", &url, &ssh, &w);
    a.password_stdin = true;
    let msg = cli::add(&config, &store, &a, Some(format!("{LENS_PW}\n")))
        .await
        .unwrap();
    assert!(
        msg.contains(&format!(
            "Through SSH bastion 127.0.0.1:{}.",
            w.bastion.port
        )),
        "{msg}"
    );
    assert!(
        msg.contains("The SSH username is in the OS keychain."),
        "{msg}"
    );
    let text = std::fs::read_to_string(&config).unwrap();
    for secret in [SSH_USER, LENS_PW] {
        assert!(!msg.contains(secret), "{msg}");
        assert!(!text.contains(secret), "{secret} in {text}");
    }

    let listed = cli::list(&config).unwrap();
    assert!(
        listed.contains(&format!("127.0.0.1:{}", w.bastion.port)),
        "{listed}"
    );
    assert!(!listed.contains(SSH_USER), "{listed}");

    let out = cli::test(&config, &store, "behind").await.unwrap();
    assert!(out.starts_with("Connected to behind"), "{out}");
    assert!(
        out.contains(&format!(
            "Through SSH bastion 127.0.0.1:{}.",
            w.bastion.port
        )),
        "{out}"
    );
    assert!(out.contains("✓ SCAN"), "{out}");
    assert!(!out.contains(SSH_USER), "{out}");
    // `conn test` closed its forward when it returned.
    assert_eq!(bastion::wait_for_ssh_processes(w.bastion.port, 0).await, 0);

    // `scan` by profile name, as the CLI runs it.
    let (report, _, target) = redis_lens::cli::scan::scan_profile_with_target(
        &config, &store, "behind", 1000, None, None, None,
    )
    .await
    .unwrap();
    assert_eq!(target, ScanTarget::Primary);
    assert_eq!(report.sampled_keys, KEYS);

    // A wrong user in the keychain: refused, and the error never names the user.
    let mut wrong = add_args("wrong", &format!("redis://{}:6379", server.host), "", &w);
    wrong.ssh = Some(format!("nobody-{SSH_USER}@127.0.0.1:{}", w.bastion.port));
    cli::add(&config, &store, &wrong, None).await.unwrap();
    let e = cli::test(&config, &store, "wrong").await.unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("refused the login"), "{e}");
    assert!(!e.contains(SSH_USER), "{e}");

    let profile = redis_lens::config::Config::load(&config)
        .unwrap()
        .get("behind")
        .unwrap()
        .clone();
    cli::remove(&config, &store, "behind").unwrap();
    assert!(store.get(&profile.ssh_secret_ref()).unwrap().is_none());
    assert!(store.get(&profile.secret_ref()).unwrap().is_none());
    assert!(!std::fs::read_to_string(&config).unwrap().contains(SSH_USER));
}

#[tokio::test]
async fn conn_commands_through_the_bastion_redis() {
    conn_commands_through_the_bastion(false).await;
}

#[tokio::test]
async fn conn_commands_through_the_bastion_valkey() {
    conn_commands_through_the_bastion(true).await;
}

// --- prefer_replica through the bastion ------------------------------------------------

/// The replica announces its network IP (the default) and gets its own forward. Both
/// need credentials, so the replica is sent them: neither is loopback from the bastion.
async fn scans_a_replica_through_the_bastion(valkey: bool) {
    let w = World::new("replica").await;
    let primary = w.server(valkey, &LENS_ONLY).await;
    seed(&primary, AS_LENS).await;
    let mut args: Vec<&str> = LENS_ONLY.to_vec();
    args.extend(MASTER_AUTH);
    args.extend(["--replicaof", primary.host.as_str(), "6379"]);
    let replica = w.server(valkey, &args).await;
    wait_for_replication(&primary, &replica, AS_LENS).await;
    let replica_ip = replica.ip().await;

    let p = w.profile(primary.endpoint(), Auth::Keychain, true);
    let store = w.store(&p);
    let (mut c, target) = conn::connect_for_scan(&p, &store).await.unwrap();
    assert_eq!(
        target,
        ScanTarget::Replica(Endpoint {
            host: replica_ip,
            port: 6379,
            tls: false,
        }),
        "valkey={valkey}"
    );
    assert!(info(&mut c).await.contains("role:slave"));
    // One forward to the primary, one to the replica.
    assert_eq!(bastion::ssh_processes(w.bastion.port), 2);
    let report = scan_all(&p, &mut c).await;
    assert_eq!(report.sampled_keys, KEYS);
    drop(c);
    assert_eq!(bastion::wait_for_ssh_processes(w.bastion.port, 0).await, 0);
}

#[tokio::test]
async fn scans_a_replica_through_the_bastion_redis() {
    scans_a_replica_through_the_bastion(false).await;
}

#[tokio::test]
async fn scans_a_replica_through_the_bastion_valkey() {
    scans_a_replica_through_the_bastion(true).await;
}

/// A replica in the bastion's own network namespace, announced as `127.0.0.1:6380`, of
/// `primary_host`. Needs credentials.
async fn loopback_replica(w: &World, valkey: bool, primary_host: &str) -> HiddenServer {
    loopback_replica_announcing(w, valkey, primary_host, "127.0.0.1").await
}

/// A replica on the bastion's loopback, announcing `announce` as its address.
async fn loopback_replica_announcing(
    w: &World,
    valkey: bool,
    primary_host: &str,
    announce: &str,
) -> HiddenServer {
    let mut args: Vec<&str> = LENS_ONLY.to_vec();
    args.extend(MASTER_AUTH);
    args.extend([
        "--replicaof",
        primary_host,
        "6379",
        "--replica-announce-ip",
        announce,
        "--replica-announce-port",
        "6380",
    ]);
    HiddenServer::start(Place::OnBastion(&w.bastion), valkey, 6380, &args).await
}

/// The trust rule behind a bastion: a primary that is not loopback from the bastion,
/// announcing `127.0.0.1`, names the bastion itself, so the candidate gets no
/// credentials and the scan stays on the primary.
async fn an_announced_loopback_behind_the_bastion_gets_no_credentials(valkey: bool) {
    let w = World::new("trust").await;
    let primary = w.server(valkey, &LENS_ONLY).await;
    seed(&primary, AS_LENS).await;
    let replica = loopback_replica(&w, valkey, &primary.ip().await).await;
    wait_for_replication(&primary, &replica, AS_LENS).await;
    let before = replica.sh(&format!("{AS_LENS} ACL LOG")).await;

    let p = w.profile(primary.endpoint(), Auth::Keychain, true);
    let (mut c, target) = conn::connect_for_scan(&p, &w.store(&p)).await.unwrap();
    let ScanTarget::PrimaryFallback(why) = &target else {
        panic!("expected a fallback, got {target}");
    };
    assert!(
        why.contains("127.0.0.1:6380") && why.contains("as seen from the SSH bastion"),
        "{why}"
    );
    assert!(info(&mut c).await.contains("role:master"));
    // No failed AUTH reached the replica: nothing was sent to it.
    assert_eq!(replica.sh(&format!("{AS_LENS} ACL LOG")).await, before);
}

/// `127.1` is 127.0.0.1 to ssh on the bastion but not loopback by name, so it is
/// refused as an announced host: the scan stays on the primary and the replica gets
/// nothing.
async fn a_shorthand_loopback_behind_the_bastion_gets_no_credentials(valkey: bool) {
    let w = World::new("shorthand").await;
    let primary = w.server(valkey, &LENS_ONLY).await;
    seed(&primary, AS_LENS).await;
    let replica = loopback_replica_announcing(&w, valkey, &primary.ip().await, "127.1").await;
    // `wait_for_replication` parses the reply as redis-lens does, which now refuses
    // `127.1`; wait for the raw entry instead.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let info = primary.sh(&format!("{AS_LENS} INFO replication")).await;
        if info.contains("ip=127.1,") && info.contains("state=online") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replication did not come up: {info}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Clients signed in as `lens` other than the CLIENT LIST call itself.
    let lens_clients = || async {
        let list = replica.sh(&format!("{AS_LENS} CLIENT LIST")).await;
        list.lines()
            .filter(|l| l.contains("user=lens") && !l.contains("cmd=client"))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(lens_clients().await, Vec::<String>::new());

    let p = w.profile(primary.endpoint(), Auth::Keychain, true);
    let (mut c, target) = conn::connect_for_scan(&p, &w.store(&p)).await.unwrap();
    assert_eq!(
        target,
        ScanTarget::PrimaryFallback("no online replica".into())
    );
    assert!(info(&mut c).await.contains("role:master"));
    assert_eq!(lens_clients().await, Vec::<String>::new());
}

#[tokio::test]
async fn a_shorthand_loopback_behind_the_bastion_gets_no_credentials_redis() {
    a_shorthand_loopback_behind_the_bastion_gets_no_credentials(false).await;
}

#[tokio::test]
async fn a_shorthand_loopback_behind_the_bastion_gets_no_credentials_valkey() {
    a_shorthand_loopback_behind_the_bastion_gets_no_credentials(true).await;
}

#[tokio::test]
async fn an_announced_loopback_behind_the_bastion_gets_no_credentials_redis() {
    an_announced_loopback_behind_the_bastion_gets_no_credentials(false).await;
}

#[tokio::test]
async fn an_announced_loopback_behind_the_bastion_gets_no_credentials_valkey() {
    an_announced_loopback_behind_the_bastion_gets_no_credentials(true).await;
}

/// The other side of the rule: a profile whose host is loopback on the bastion may send
/// credentials to a replica announced on the bastion's loopback.
async fn a_primary_on_the_bastion_may_use_its_loopback_replica(valkey: bool) {
    let w = World::new("onbastion").await;
    let primary = HiddenServer::start(Place::OnBastion(&w.bastion), valkey, 6379, &LENS_ONLY).await;
    seed(&primary, AS_LENS).await;
    let replica = loopback_replica(&w, valkey, "127.0.0.1").await;
    wait_for_replication(&primary, &replica, AS_LENS).await;

    let p = w.profile(primary.endpoint(), Auth::Keychain, true);
    let (mut c, target) = conn::connect_for_scan(&p, &w.store(&p)).await.unwrap();
    assert_eq!(
        target,
        ScanTarget::Replica(Endpoint {
            host: "127.0.0.1".into(),
            port: 6380,
            tls: false,
        })
    );
    assert!(info(&mut c).await.contains("role:slave"));
}

#[tokio::test]
async fn a_primary_on_the_bastion_may_use_its_loopback_replica_redis() {
    a_primary_on_the_bastion_may_use_its_loopback_replica(false).await;
}

#[tokio::test]
async fn a_primary_on_the_bastion_may_use_its_loopback_replica_valkey() {
    a_primary_on_the_bastion_may_use_its_loopback_replica(true).await;
}

// --- Failures --------------------------------------------------------------------------

async fn host_key_problems_give_host_key(valkey: bool) {
    let w = World::new("hostkey").await;
    let server = w.server(valkey, &[]).await;
    let mut p = w.profile(server.endpoint(), Auth::None, false);
    let store = w.store(&p);
    // Not listed at all.
    let empty = w.dir.join("empty_known_hosts");
    std::fs::write(&empty, "").unwrap();
    p.ssh.as_mut().unwrap().known_hosts_file = Some(empty);
    assert_eq!(
        expect_ssh_error(conn::connect(&p, &store).await),
        SshError::HostKey
    );
    // Listed with another key.
    let other = bastion::keygen(&w.dir, "not_the_host_key");
    let wrong = w
        .bastion
        .known_hosts_with(&w.dir, "wrong_known_hosts", &other.public);
    p.ssh.as_mut().unwrap().known_hosts_file = Some(wrong);
    assert_eq!(
        expect_ssh_error(conn::connect_for_scan(&p, &store).await),
        SshError::HostKey
    );
    assert_eq!(bastion::wait_for_ssh_processes(w.bastion.port, 0).await, 0);
}

#[tokio::test]
async fn host_key_problems_give_host_key_redis() {
    host_key_problems_give_host_key(false).await;
}

#[tokio::test]
async fn host_key_problems_give_host_key_valkey() {
    host_key_problems_give_host_key(true).await;
}

async fn a_wrong_key_gives_auth(valkey: bool) {
    let w = World::new("auth").await;
    let server = w.server(valkey, &[]).await;
    let mut p = w.profile(server.endpoint(), Auth::None, false);
    let store = w.store(&p);
    let other = bastion::keygen(&w.dir, "not_authorized");
    p.ssh.as_mut().unwrap().identity_file = Some(other.private);
    assert_eq!(
        expect_ssh_error(conn::connect(&p, &store).await),
        SshError::Auth
    );
    // The right key with no user: refused too (ssh logs in as this machine's user).
    let right = w.profile(server.endpoint(), Auth::None, false);
    assert_eq!(
        expect_ssh_error(conn::connect(&right, &MemoryStore::default()).await),
        SshError::Auth
    );
}

#[tokio::test]
async fn a_wrong_key_gives_auth_redis() {
    a_wrong_key_gives_auth(false).await;
}

#[tokio::test]
async fn a_wrong_key_gives_auth_valkey() {
    a_wrong_key_gives_auth(true).await;
}

/// No containers needed: nothing listens on the port.
#[tokio::test]
async fn an_unreachable_bastion_gives_unreachable() {
    let dir = bastion::temp_dir("unreachable");
    let key = bastion::keygen(&dir, "id");
    let kh = dir.join("known_hosts");
    std::fs::write(&kh, "").unwrap();
    let p = Profile {
        name: "nowhere".into(),
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
        ssh: Some(SshTunnel {
            host: "127.0.0.1".into(),
            port: Some(common::free_port()),
            identity_file: Some(key.private.clone()),
            known_hosts_file: Some(kh),
        }),
        sentinel: None,
    };
    let store = MemoryStore::default();
    store
        .set(
            &p.ssh_secret_ref(),
            &Credentials {
                username: Some(SSH_USER.into()),
                password: SecretString::from(String::new()),
            },
        )
        .unwrap();
    assert_eq!(
        expect_ssh_error(conn::connect(&p, &store).await),
        SshError::Unreachable
    );
    // A name that does not resolve.
    let mut nowhere = p.clone();
    nowhere.ssh.as_mut().unwrap().host = "bastion.invalid".into();
    assert_eq!(
        expect_ssh_error(conn::connect(&nowhere, &store).await),
        SshError::Unreachable
    );
    let _ = std::fs::remove_dir_all(dir);
}

// --- TLS through the bastion -----------------------------------------------------------

/// Set in the child process: the directory holding the TLS fixture files.
const TLS_DIR_ENV: &str = "REDIS_LENS_IT_SSH_TLS_DIR";
const CERT_FILE_ENV: &str = "SSL_CERT_FILE";

/// Run `body` in a child process of this test binary whose `SSL_CERT_FILE` is a fresh
/// CA from its start (redis-lens loads its trust roots once per process, and setting
/// the variable here would need `unsafe`; see `tests/tls_it.rs`). The parent writes the
/// CA, a server certificate for `name` (the container name the child gives the
/// server), and one for another name. Only this test uses TLS in this binary.
fn in_tls_process(test: &str, body: impl AsyncFnOnce(PathBuf)) {
    if let Ok(dir) = std::env::var(TLS_DIR_ENV) {
        let dir = PathBuf::from(dir);
        assert_eq!(
            std::env::var_os(CERT_FILE_ENV),
            Some(dir.join("ca.pem").into_os_string())
        );
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(body(dir));
        return;
    }
    let dir = bastion::temp_dir("tls");
    let ca = TestCa::new("redis-lens ssh test CA");
    let name = bastion::unique("tls");
    let good = ca.server(&[&name], &ca);
    let wrong = ca.server(&["wrong-name.invalid"], &ca);
    for (file, data) in [
        ("ca.pem", ca.pem()),
        ("name", name),
        ("good.crt", good.cert),
        ("good.key", good.key),
        ("wrong.crt", wrong.cert),
        ("wrong.key", wrong.key),
    ] {
        std::fs::write(dir.join(file), data).expect("write a fixture file");
    }
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([test, "--exact", "--nocapture", "--test-threads", "1"])
        .env(TLS_DIR_ENV, &dir)
        .env(CERT_FILE_ENV, dir.join("ca.pem"))
        .env_remove("SSL_CERT_DIR")
        .output()
        .expect("run the test in a child process");
    let stdout = String::from_utf8_lossy(&out.stdout);
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "child process failed: {}", out.status);
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the child did not run `{test}`"
    );
}

/// TLS goes through the forward, and the certificate is still checked against the
/// server's own name (here a container name that only resolves on the bastion), not
/// against the forward's 127.0.0.1.
async fn tls_through_the_bastion_checks_the_server_name(valkey: bool, dir: PathBuf) {
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).expect("read a fixture");
    let name = read("name");
    let ca = read("ca.pem");
    let good = ServerTls {
        cert: read("good.crt"),
        key: read("good.key"),
        ca: ca.clone(),
    };
    let wrong = ServerTls {
        cert: read("wrong.crt"),
        key: read("wrong.key"),
        ca,
    };
    let w = World::new("tls").await;
    let server =
        HiddenServer::start_tls(Place::Network(&w.bastion.network, &name), valkey, &good).await;
    let mut ep = server.endpoint();
    ep.tls = true;
    let p = w.profile(ep, Auth::None, false);
    let store = w.store(&p);
    let mut c = conn::connect(&p, &store)
        .await
        .expect("TLS through the bastion");
    let pong: String = redis::cmd("PING").query_async(&mut c).await.unwrap();
    assert_eq!(pong, "PONG");
    drop(c);

    // The same CA, but a certificate for another name: refused, and not by ssh.
    let other_name = bastion::unique("tls-wrong");
    let other = HiddenServer::start_tls(
        Place::Network(&w.bastion.network, &other_name),
        valkey,
        &wrong,
    )
    .await;
    let mut ep = other.endpoint();
    ep.tls = true;
    let p = w.profile(ep, Auth::None, false);
    match conn::connect(&p, &store).await {
        Err(ConnError::Redis(e)) => {
            let text = e.to_string().to_ascii_lowercase();
            assert!(text.contains("certificate"), "{text}");
        }
        Err(e) => panic!("expected a TLS error, got {e}"),
        Ok(_) => panic!("a certificate for another name was accepted"),
    }
}

#[test]
fn tls_through_the_bastion_checks_the_server_name_redis() {
    in_tls_process(
        "tls_through_the_bastion_checks_the_server_name_redis",
        async |dir| tls_through_the_bastion_checks_the_server_name(false, dir).await,
    );
}

#[test]
fn tls_through_the_bastion_checks_the_server_name_valkey() {
    in_tls_process(
        "tls_through_the_bastion_checks_the_server_name_valkey",
        async |dir| tls_through_the_bastion_checks_the_server_name(true, dir).await,
    );
}
