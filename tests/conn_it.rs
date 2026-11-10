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
