//! The `ACL DRYRUN` write probes (spec "Write permissions", check 1) against real
//! servers: which writes a user may send, in redis-lens's own words.
#![cfg(feature = "integration-tests")]

mod common;

use redis_lens::conn::{self, PERM_ACL_DRYRUN, WRITES_CHECKED_ON_USE};
use redis_lens::model::{PermCheck, PermStatus};

fn status<'a>(checks: &'a [PermCheck], label: &str) -> &'a PermStatus {
    &checks
        .iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("no {label} in {checks:?}"))
        .status
}

/// The server's major and minor version, from `INFO server`.
async fn version(c: &mut redis::aio::MultiplexedConnection) -> (u32, u32) {
    let info: String = redis::cmd("INFO")
        .arg("server")
        .query_async(c)
        .await
        .unwrap();
    let v = info
        .lines()
        .find_map(|l| l.strip_prefix("redis_version:"))
        .unwrap()
        .trim()
        .to_string();
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap());
    (parts.next().unwrap(), parts.next().unwrap())
}

fn skipped_only(checks: &[PermCheck]) -> bool {
    checks.len() == 1
        && checks[0].label == PERM_ACL_DRYRUN
        && checks[0].status == PermStatus::Skipped(WRITES_CHECKED_ON_USE.into())
}

#[tokio::test]
async fn the_default_user_may_send_every_write_it_has() {
    for valkey in [false, true] {
        let server = common::server_with(valkey, &[]).await;
        let mut c = server.admin().await;
        let checks = conn::check_write_permissions(&mut c).await;
        if !valkey && version(&mut c).await < (7, 0) {
            // No ACL DRYRUN before 7.0: writes are checked on use.
            assert!(skipped_only(&checks), "{checks:?}");
            continue;
        }
        assert_eq!(status(&checks, PERM_ACL_DRYRUN), &PermStatus::Allowed);
        for label in ["write SET", "write HSET", "write UNLINK", "write PUBLISH"] {
            assert_eq!(status(&checks, label), &PermStatus::Allowed, "{label}");
        }
        // A command the server doesn't have (no Search module here) is not refused.
        assert_eq!(
            status(&checks, "write FT.CREATE"),
            &PermStatus::Skipped(WRITES_CHECKED_ON_USE.into())
        );
        // The probe wrote nothing.
        let n: i64 = redis::cmd("DBSIZE").query_async(&mut c).await.unwrap();
        assert_eq!(n, 0);
    }
}

#[tokio::test]
async fn refused_writes_are_denied_and_key_rules_are_left_to_use() {
    for valkey in [false, true] {
        let server = common::server_with(
            valkey,
            &[
                "--user", "writer", "on", ">w-pw", "~app:*", "&*", "+@all", "-hset", "-unlink",
            ],
        )
        .await;
        let mut c = server.admin_as("writer", "w-pw").await;
        let checks = conn::check_write_permissions(&mut c).await;
        if !valkey && version(&mut c).await < (7, 0) {
            assert!(skipped_only(&checks), "{checks:?}");
            continue;
        }
        assert_eq!(status(&checks, PERM_ACL_DRYRUN), &PermStatus::Allowed);
        for (label, name) in [("write HSET", "HSET"), ("write UNLINK", "UNLINK")] {
            assert_eq!(
                status(&checks, label),
                &PermStatus::Denied(format!("NOPERM {name} is not allowed for this user")),
                "valkey {valkey}"
            );
        }
        // SET is allowed but not on the probe's key: known only when it is used.
        assert_eq!(
            status(&checks, "write SET"),
            &PermStatus::Skipped(WRITES_CHECKED_ON_USE.into()),
            "valkey {valkey}"
        );
        // Nothing names the user.
        let text = format!("{checks:?}");
        assert!(!text.contains("writer"), "{text}");
    }
}

#[tokio::test]
async fn a_user_without_acl_dryrun_has_writes_checked_on_use() {
    for valkey in [false, true] {
        let server = common::server_with(
            valkey,
            &["--user", "plain", "on", ">p-pw", "~*", "+@all", "-@admin"],
        )
        .await;
        let mut c = server.admin_as("plain", "p-pw").await;
        let checks = conn::check_write_permissions(&mut c).await;
        assert!(skipped_only(&checks), "valkey {valkey}: {checks:?}");
    }
}
