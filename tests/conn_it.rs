//! Connecting, permission checks, Docker discovery and `conn add` against real servers.
#![cfg(feature = "integration-tests")]

mod common;

use redis_lens::cli::ConnAddArgs;
use redis_lens::cli::conn as cli_conn;
use redis_lens::config::Config;
use redis_lens::conn::{self, check_permissions};
use redis_lens::model::{
    Auth, Credentials, Endpoint, Env, PERM_FREQ, PERM_INFO, PERM_LABELS, PERM_MEMORY, PERM_SCAN,
    PERM_TYPE_TTL, PermStatus, PermissionReport, Profile,
};
use redis_lens::secrets::{MemoryStore, SecretStore};
use secrecy::{ExposeSecret, SecretString};

fn profile(name: &str, port: u16, auth: Auth) -> Profile {
    Profile {
        name: name.to_string(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        },
        db: 0,
        auth,
        read_only: None,
        prefer_replica: false,
    }
}

fn creds(user: Option<&str>, pw: &str) -> Credentials {
    Credentials {
        username: user.map(String::from),
        password: SecretString::from(pw.to_string()),
    }
}

async fn ping(conn: &mut redis::aio::MultiplexedConnection) {
    let pong: String = redis::cmd("PING").query_async(conn).await.unwrap();
    assert_eq!(pong, "PONG");
}

fn status<'a>(r: &'a PermissionReport, label: &str) -> &'a PermStatus {
    &r.checks.iter().find(|c| c.label == label).unwrap().status
}

fn assert_labels_in_order(r: &PermissionReport) {
    let labels: Vec<&str> = r.checks.iter().map(|c| c.label).collect();
    assert_eq!(labels, PERM_LABELS);
}

const LENS_RULES: [&str; 20] = [
    "--user",
    "lens",
    "on",
    ">lens-pw",
    "~*",
    "-@all",
    "+ping",
    "+hello",
    "+client|setinfo",
    "+client|setname",
    "+select",
    "+info",
    "+dbsize",
    "+scan",
    "+type",
    "+ttl",
    "+pttl",
    "+memory|usage",
    "+object|freq",
    "+auth",
];

/// Rules for the `lens` user, optionally leaving out one command rule.
fn lens_args(without: Option<&str>, extra: &[&'static str]) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = LENS_RULES
        .iter()
        .copied()
        .filter(|r| Some(*r) != without)
        .collect();
    v.extend_from_slice(extra);
    v
}

async fn seed(server: &common::TestServer) {
    let mut admin = server.admin().await;
    let _: () = redis::cmd("SET")
        .arg("seed:1")
        .arg("value")
        .query_async(&mut admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn connect_without_auth_redis_and_valkey() {
    let secrets = MemoryStore::default();
    for server in [common::redis().await, common::valkey().await] {
        let mut conn = conn::connect(&profile("p", server.port, Auth::None), &secrets)
            .await
            .unwrap();
        ping(&mut conn).await;
    }
}

#[tokio::test]
async fn connect_selects_the_db() {
    let server = common::redis().await;
    let secrets = MemoryStore::default();
    let mut p = profile("p", server.port, Auth::None);
    p.db = 3;
    let mut conn = conn::connect(&p, &secrets).await.unwrap();
    let info: redis::Value = redis::cmd("CLIENT")
        .arg("INFO")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(format!("{info:?}").contains("db=3"));
}

#[tokio::test]
async fn connect_with_keychain_credentials() {
    let server = common::redis_with(&lens_args(None, &[])).await;
    let secrets = MemoryStore::default();
    let p = profile("acl", server.port, Auth::Keychain);
    secrets
        .set(&p.secret_ref(), &creds(Some("lens"), "lens-pw"))
        .unwrap();
    let mut conn = conn::connect(&p, &secrets).await.unwrap();
    ping(&mut conn).await;
}

#[tokio::test]
async fn missing_keychain_entry_is_reported() {
    let secrets = MemoryStore::default();
    let p = profile("nobody", 1, Auth::Keychain);
    let e = conn::connect(&p, &secrets).await.unwrap_err();
    assert!(matches!(e, conn::ConnError::MissingCredentials(n) if n == "nobody"));
}

#[tokio::test]
async fn wrong_password_fails_without_leaking_it() {
    let server = common::redis_with(&lens_args(None, &[])).await;
    let secrets = MemoryStore::default();
    let p = profile("acl", server.port, Auth::Keychain);
    let bad = "definitely-not-the-pw-9137";
    secrets
        .set(&p.secret_ref(), &creds(Some("lens"), bad))
        .unwrap();
    let e = conn::connect(&p, &secrets).await.unwrap_err();
    assert!(!e.to_string().contains(bad), "{e}");
    assert!(!format!("{e:?}").contains(bad), "{e:?}");
}

#[tokio::test]
async fn entra_without_feature_is_unsupported() {
    if cfg!(feature = "entra-id") {
        return;
    }
    let secrets = MemoryStore::default();
    let e = conn::connect(&profile("az", 1, Auth::EntraId), &secrets)
        .await
        .unwrap_err();
    assert!(matches!(e, conn::ConnError::EntraUnsupported));
}

#[tokio::test]
async fn permissions_full_user_on_redis() {
    let server = common::redis_with(&lens_args(None, &[])).await;
    seed(&server).await;
    let secrets = MemoryStore::default();
    let p = profile("acl", server.port, Auth::Keychain);
    secrets
        .set(&p.secret_ref(), &creds(Some("lens"), "lens-pw"))
        .unwrap();
    let mut conn = conn::connect(&p, &secrets).await.unwrap();
    let r = check_permissions(&mut conn).await;
    assert_labels_in_order(&r);
    for l in [PERM_SCAN, PERM_TYPE_TTL, PERM_MEMORY, PERM_INFO] {
        assert_eq!(status(&r, l), &PermStatus::Allowed, "{l}");
    }
    assert!(
        matches!(status(&r, PERM_FREQ), PermStatus::Skipped(m) if m.contains("noeviction")),
        "{:?}",
        status(&r, PERM_FREQ)
    );
}

#[tokio::test]
async fn permissions_without_memory_usage() {
    let server = common::redis_with(&lens_args(Some("+memory|usage"), &[])).await;
    seed(&server).await;
    let secrets = MemoryStore::default();
    let p = profile("acl", server.port, Auth::Keychain);
    secrets
        .set(&p.secret_ref(), &creds(Some("lens"), "lens-pw"))
        .unwrap();
    let mut conn = conn::connect(&p, &secrets).await.unwrap();
    let r = check_permissions(&mut conn).await;
    assert!(
        matches!(status(&r, PERM_MEMORY), PermStatus::Denied(m) if m.contains("NOPERM")),
        "{:?}",
        status(&r, PERM_MEMORY)
    );
    assert_eq!(status(&r, PERM_SCAN), &PermStatus::Allowed);
    assert_eq!(status(&r, PERM_TYPE_TTL), &PermStatus::Allowed);
}

#[tokio::test]
async fn permissions_without_info_skips_freq() {
    let server = common::redis_with(&lens_args(Some("+info"), &[])).await;
    seed(&server).await;
    let secrets = MemoryStore::default();
    let p = profile("acl", server.port, Auth::Keychain);
    secrets
        .set(&p.secret_ref(), &creds(Some("lens"), "lens-pw"))
        .unwrap();
    let mut conn = conn::connect(&p, &secrets).await.unwrap();
    let r = check_permissions(&mut conn).await;
    assert!(matches!(status(&r, PERM_INFO), PermStatus::Denied(_)));
    assert!(matches!(status(&r, PERM_FREQ), PermStatus::Skipped(_)));
}

#[tokio::test]
async fn permissions_lfu_policy_allows_object_freq() {
    let server = common::redis_with(&["--maxmemory-policy", "allkeys-lfu"]).await;
    seed(&server).await;
    let secrets = MemoryStore::default();
    let mut conn = conn::connect(&profile("p", server.port, Auth::None), &secrets)
        .await
        .unwrap();
    let r = check_permissions(&mut conn).await;
    assert_eq!(status(&r, PERM_FREQ), &PermStatus::Allowed);
}

#[tokio::test]
async fn permissions_on_empty_db_use_probe_key() {
    let server = common::redis_with(&["--maxmemory-policy", "allkeys-lfu"]).await;
    let secrets = MemoryStore::default();
    let mut conn = conn::connect(&profile("p", server.port, Auth::None), &secrets)
        .await
        .unwrap();
    let r = check_permissions(&mut conn).await;
    for c in &r.checks {
        assert_eq!(c.status, PermStatus::Allowed, "{}", c.label);
    }
}

#[tokio::test]
async fn permissions_default_user_redis_and_valkey() {
    let secrets = MemoryStore::default();
    for server in [common::redis().await, common::valkey().await] {
        seed(&server).await;
        let mut conn = conn::connect(&profile("p", server.port, Auth::None), &secrets)
            .await
            .unwrap();
        let r = check_permissions(&mut conn).await;
        assert_labels_in_order(&r);
        for l in [PERM_SCAN, PERM_TYPE_TTL, PERM_MEMORY, PERM_INFO] {
            assert_eq!(status(&r, l), &PermStatus::Allowed, "{l}");
        }
        assert!(matches!(status(&r, PERM_FREQ), PermStatus::Skipped(_)));
    }
}

#[tokio::test]
async fn discover_finds_container_with_password() {
    let server = common::redis_with(&["--requirepass", "s3cret"]).await;
    let found = conn::docker::discover().await.unwrap();
    let ours = found
        .iter()
        .find(|d| d.profile.endpoint.port == server.port)
        .expect("our container is discovered");
    assert_eq!(ours.profile.endpoint.host, "127.0.0.1");
    assert_eq!(ours.profile.env, Env::Local);
    assert_eq!(ours.profile.auth, Auth::Keychain);
    assert!(!ours.profile.name.starts_with('/'));
    let c = ours.credentials.as_ref().expect("password found");
    assert_eq!(c.password.expose_secret(), "s3cret");
    assert!(!cli_conn::format_discovered(&found).contains("s3cret"));

    // The discovered credentials work for a connection.
    let secrets = MemoryStore::default();
    secrets.set(&ours.profile.secret_ref(), c).unwrap();
    let mut conn = conn::connect(&ours.profile, &secrets).await.unwrap();
    ping(&mut conn).await;
}

#[tokio::test]
async fn conn_add_keeps_credentials_out_of_the_config_file() {
    let dir = std::env::temp_dir().join(format!("redis-lens-conn-it-{}", std::process::id()));
    let path = dir.join("config.toml");
    let secrets = MemoryStore::default();
    let args = ConnAddArgs {
        name: "svc".into(),
        url: "rediss://uniq-user-77@cache.example.com:6380/2".into(),
        env: None,
        password_stdin: true,
        ask_password: false,
        entra: false,
        force: false,
        prefer_replica: false,
    };
    let msg = cli_conn::add(&path, &secrets, &args, Some("uniq-pw-4242\n".into()))
        .await
        .unwrap();
    assert!(
        msg.starts_with("Saved svc (cache.example.com:6380, OTHER)."),
        "{msg}"
    );

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("uniq-pw-4242"), "{text}");
    assert!(!text.contains("uniq-user-77"), "{text}");

    let p = Config::load(&path).unwrap().get("svc").unwrap().clone();
    let c = secrets.get(&p.secret_ref()).unwrap().unwrap();
    assert_eq!(c.username.as_deref(), Some("uniq-user-77"));
    assert_eq!(c.password.expose_secret(), "uniq-pw-4242");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Auth probe used by `discover`
// ---------------------------------------------------------------------------

use redis_lens::conn::docker::{DiscoveredAuth, probe_auth_within};

/// The probe with a timeout that a busy machine running many containers still meets
/// (discovery itself uses 1 s).
async fn probe_auth(ep: &Endpoint) -> DiscoveredAuth {
    probe_auth_within(ep, std::time::Duration::from_secs(5)).await
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint {
        host: "127.0.0.1".to_string(),
        port,
        tls: false,
    }
}

#[tokio::test]
async fn probe_no_auth_server_is_none() {
    for server in [common::redis().await, common::valkey().await] {
        assert_eq!(
            probe_auth(&endpoint(server.port)).await,
            DiscoveredAuth::None
        );
    }
}

#[tokio::test]
async fn probe_requirepass_server_is_required() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &["--requirepass", "probe-pw-1"]).await;
        assert_eq!(
            probe_auth(&endpoint(server.port)).await,
            DiscoveredAuth::Required,
            "valkey={valkey}"
        );
    }
}

#[tokio::test]
async fn probe_server_with_default_user_off_is_required() {
    let args = [
        "--user", "default", "off", "--user", "lens", "on", ">lens-pw", "~*", "+@all",
    ];
    for valkey in [false, true] {
        let server = common::server_with(valkey, &args).await;
        assert_eq!(
            probe_auth(&endpoint(server.port)).await,
            DiscoveredAuth::Required,
            "valkey={valkey}"
        );
    }
}

#[tokio::test]
async fn probe_unreachable_port_is_unknown() {
    let port = common::free_port();
    assert_eq!(probe_auth(&endpoint(port)).await, DiscoveredAuth::Unknown);
}

#[tokio::test]
async fn probe_needs_only_ping() {
    // A default user that may only PING: the probe needs nothing else.
    let args = ["--user", "default", "on", "nopass", "~*", "-@all", "+ping"];
    for valkey in [false, true] {
        let server = common::server_with(valkey, &args).await;
        assert_eq!(
            probe_auth(&endpoint(server.port)).await,
            DiscoveredAuth::None,
            "valkey={valkey}"
        );
    }
}

/// The probe sends `PING` and nothing else: no `AUTH`, `HELLO`, `CLIENT SETINFO` or
/// `SELECT`. Counted by the server itself after `CONFIG RESETSTAT` (test setup).
#[tokio::test]
async fn probe_sends_only_ping() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut admin = server.admin().await;
        let _: () = redis::cmd("CONFIG")
            .arg("RESETSTAT")
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(
            probe_auth(&endpoint(server.port)).await,
            DiscoveredAuth::None,
            "valkey={valkey}"
        );
        let stats: String = redis::cmd("INFO")
            .arg("commandstats")
            .query_async(&mut admin)
            .await
            .unwrap();
        let names: Vec<(&str, &str)> = stats
            .lines()
            .filter_map(|l| l.strip_prefix("cmdstat_"))
            .filter_map(|l| l.split_once(':'))
            .collect();
        for (name, _) in &names {
            assert!(
                ["ping", "info", "config|resetstat", "config"].contains(name),
                "valkey={valkey}: the probe (or something) sent {name}: {stats}"
            );
        }
        let ping = names
            .iter()
            .find(|(n, _)| *n == "ping")
            .unwrap_or_else(|| panic!("valkey={valkey}: no PING counted: {stats}"));
        assert!(
            ping.1.starts_with("calls=1,"),
            "valkey={valkey}: {}",
            ping.1
        );
    }
}

#[tokio::test]
async fn discover_reports_auth_for_each_kind_of_container() {
    for valkey in [false, true] {
        let open = common::server_with(valkey, &[]).await;
        let pw = common::server_with(valkey, &["--requirepass", "disc-pw-2"]).await;
        let acl = common::server_with(
            valkey,
            &[
                "--user", "default", "off", "--user", "lens", "on", ">lens-pw", "~*", "+@all",
            ],
        )
        .await;
        let found = conn::docker::discover().await.unwrap();
        let find = |port: u16| {
            found
                .iter()
                .find(|d| d.profile.endpoint.port == port)
                .expect("container is discovered")
        };
        let d = find(open.port);
        assert_eq!(d.auth, DiscoveredAuth::None, "valkey={valkey}");
        assert_eq!(d.profile.auth, Auth::None);
        let d = find(pw.port);
        assert_eq!(d.auth, DiscoveredAuth::Password);
        assert_eq!(d.profile.auth, Auth::Keychain);
        let d = find(acl.port);
        assert_eq!(d.auth, DiscoveredAuth::Required, "valkey={valkey}");
        assert_eq!(d.profile.auth, Auth::None);
        assert!(d.credentials.is_none());

        let out = cli_conn::format_discovered(&found);
        for word in ["none", "password", "required"] {
            assert!(out.contains(word), "{out}");
        }
        assert!(!out.contains("disc-pw-2"), "{out}");
    }
}

// ---------------------------------------------------------------------------
// `conn test`
// ---------------------------------------------------------------------------

fn temp_config(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("redis-lens-conn-it-{tag}-{}", std::process::id()))
        .join("config.toml")
}

fn save_profile(path: &std::path::Path, p: &Profile) {
    let mut config = Config::load(path).unwrap();
    config.add(p.clone(), true).unwrap();
    config.save(path).unwrap();
}

#[tokio::test]
async fn conn_test_reports_success_and_permissions() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens_args(None, &[])).await;
        seed(&server).await;
        let secrets = MemoryStore::default();
        let p = profile("lens-profile", server.port, Auth::Keychain);
        secrets
            .set(&p.secret_ref(), &creds(Some("lens"), "lens-pw"))
            .unwrap();
        let path = temp_config(&format!("test-ok-{valkey}"));
        save_profile(&path, &p);

        let out = cli_conn::test(&path, &secrets, "lens-profile")
            .await
            .unwrap();
        assert!(out.starts_with("Connected to lens-profile"), "{out}");
        for label in [PERM_SCAN, PERM_TYPE_TTL, PERM_MEMORY, PERM_INFO] {
            assert!(out.contains(&format!("✓ {label}")), "{label}: {out}");
        }
        // noeviction, so OBJECT FREQ is skipped with a reason, not denied.
        assert!(out.contains(&format!("– {PERM_FREQ}")), "{out}");
        assert!(!out.contains("lens-pw"), "{out}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

#[tokio::test]
async fn conn_test_reports_denied_commands() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens_args(Some("+memory|usage"), &[])).await;
        seed(&server).await;
        let secrets = MemoryStore::default();
        let p = profile("limited", server.port, Auth::Keychain);
        secrets
            .set(&p.secret_ref(), &creds(Some("lens"), "lens-pw"))
            .unwrap();
        let path = temp_config(&format!("test-denied-{valkey}"));
        save_profile(&path, &p);
        let out = cli_conn::test(&path, &secrets, "limited").await.unwrap();
        assert!(
            out.contains(&format!("✗ {PERM_MEMORY}")),
            "valkey={valkey}: {out}"
        );
        assert!(
            out.contains(&format!("✓ {PERM_SCAN}")),
            "valkey={valkey}: {out}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

#[tokio::test]
async fn conn_test_wrong_password_fails_without_the_password() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &lens_args(None, &[])).await;
        let secrets = MemoryStore::default();
        let p = profile("badpw", server.port, Auth::Keychain);
        let bad = "wrong-pw-for-conn-test-5521";
        secrets
            .set(&p.secret_ref(), &creds(Some("lens"), bad))
            .unwrap();
        let path = temp_config(&format!("test-bad-{valkey}"));
        save_profile(&path, &p);
        let e = cli_conn::test(&path, &secrets, "badpw").await.unwrap_err();
        assert!(!format!("{e:#}").contains(bad), "{e:#}");
        assert!(!format!("{e:?}").contains(bad), "{e:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

#[tokio::test]
async fn conn_test_unknown_profile_is_an_error() {
    let secrets = MemoryStore::default();
    let path = temp_config("test-unknown");
    let e = cli_conn::test(&path, &secrets, "nope").await.unwrap_err();
    assert!(e.to_string().contains("nope"), "{e}");
}

// ---------------------------------------------------------------------------
// The minimal read-only ACL user from spec.md "Permission check"
// ---------------------------------------------------------------------------

/// The spec's line, word by word, with the password where it has `…`.
const SPEC_RO_RULES: &str = "on >{PW} ~* resetchannels -@all +ping +hello +client|setinfo +client|setname +select +info +dbsize +scan +type +ttl +pttl +memory|usage +object|freq";

#[tokio::test]
async fn spec_minimal_read_only_user_can_connect_check_and_scan() {
    use redis_lens::scan::{ScanOptions, scan};
    let pw = "ro-secret-pw-8731";
    for valkey in [false, true] {
        // LFU, so OBJECT FREQ is checked and used too.
        let server = common::server_with(valkey, &["--maxmemory-policy", "allkeys-lfu"]).await;
        let mut admin = server.admin().await;
        for i in 0..20 {
            let _: () = redis::cmd("SET")
                .arg(format!("user:{i}"))
                .arg("v")
                .query_async(&mut admin)
                .await
                .unwrap();
        }
        let mut setuser = redis::cmd("ACL");
        setuser.arg("SETUSER").arg("lens-ro");
        for word in SPEC_RO_RULES.replace("{PW}", pw).split_whitespace() {
            setuser.arg(word);
        }
        let _: () = setuser.query_async(&mut admin).await.unwrap();

        let secrets = MemoryStore::default();
        let p = profile("ro", server.port, Auth::Keychain);
        secrets
            .set(&p.secret_ref(), &creds(Some("lens-ro"), pw))
            .unwrap();
        let (mut conn, target) = conn::connect_for_scan(&p, &secrets)
            .await
            .unwrap_or_else(|e| panic!("valkey={valkey}: connect as lens-ro: {e}"));
        assert_eq!(target, conn::ScanTarget::Primary);
        let perms = check_permissions(&mut conn).await;
        assert_labels_in_order(&perms);
        for c in &perms.checks {
            assert_eq!(
                c.status,
                PermStatus::Allowed,
                "valkey={valkey}: {}",
                c.label
            );
        }
        let report = scan("ro", &mut conn, &ScanOptions::default(), &perms, None)
            .await
            .unwrap_or_else(|e| panic!("valkey={valkey}: scan as lens-ro: {e}"));
        assert_eq!(report.sampled_keys, 20, "valkey={valkey}");
        assert!(report.sizes_available);
    }
}
