//! Bulk expire and delete by pattern (spec "Bulk actions") against real servers: the
//! dry run, the runner with a permit from the gate, the key file, and `redis-lens
//! expire`. Every test runs on Redis and Valkey; with
//! `REDIS_LENS_IT_REDIS_IMAGE=redis:6.2-alpine`, expire is refused with the version
//! reason there and delete still runs.
#![cfg(feature = "integration-tests")]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use redis::aio::MultiplexedConnection;
use redis_lens::analysis::{key_in_pattern, match_glob};
use redis_lens::budget::Budget;
use redis_lens::cli::exit_code;
use redis_lens::cli::expire::{ExpireArgs, execute};
use redis_lens::config::Config;
use redis_lens::live::bulk::{self, KeyFile, Walk};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::secrets::MemoryStore;
use redis_lens::write::{Answer, BulkAction, Permit, Refusals, Settings, WriteOp};

const CART: &str = "user:{id}:cart";
const PROFILE: &str = "user:{id}:profile";
/// Matched by the cart glob `user:*:cart`, but of another pattern.
const ADMIN_CART: &str = "user:admin:cart";

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-bulk-it-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn profile(port: u16, env: Env, read_only: Option<bool>) -> Profile {
    Profile {
        name: "it".to_string(),
        env,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        tags: Vec::new(),
    }
}

fn temp_config(p: &Profile) -> PathBuf {
    let path = temp_dir("config").join("config.toml");
    Config {
        profiles: vec![p.clone()],
        ..Default::default()
    }
    .save(&path)
    .unwrap();
    path
}

/// 600 cart keys (every sixth with a TTL of an hour: 100), 50 profile keys without a
/// TTL, and `user:admin:cart` without one. Returns the cart keys without a TTL.
async fn seed(admin: &mut MultiplexedConnection) -> BTreeSet<String> {
    let mut no_ttl = BTreeSet::new();
    let mut pipe = redis::pipe();
    for i in 0..600 {
        let k = format!("user:{}:cart", 1000 + i);
        if i % 6 == 0 {
            pipe.cmd("SET")
                .arg(&k)
                .arg("v")
                .arg("EX")
                .arg(3600)
                .ignore();
        } else {
            pipe.cmd("SET").arg(&k).arg("v").ignore();
            no_ttl.insert(k);
        }
    }
    for i in 0..50 {
        pipe.cmd("SET")
            .arg(format!("user:{}:profile", 5000 + i))
            .arg("v")
            .ignore();
    }
    pipe.cmd("SET").arg(ADMIN_CART).arg("v").ignore();
    let () = pipe.query_async(admin).await.unwrap();
    assert!(!key_in_pattern(ADMIN_CART, CART));
    assert!(key_in_pattern("user:1000:cart", CART));
    no_ttl
}

async fn ttl(admin: &mut MultiplexedConnection, key: &str) -> i64 {
    redis::cmd("TTL").arg(key).query_async(admin).await.unwrap()
}

async fn exists(admin: &mut MultiplexedConnection, key: &str) -> bool {
    let n: i64 = redis::cmd("EXISTS")
        .arg(key)
        .query_async(admin)
        .await
        .unwrap();
    n == 1
}

/// Whether the server has `EXPIRE … NX`; `false` on Redis 6.2.
async fn has_expire_nx(admin: &mut MultiplexedConnection) -> bool {
    let info = redis_lens::scan::server_info(admin).await.unwrap();
    bulk::expire_off(&info).is_none()
}

fn bulk_op(pattern: &str, action: BulkAction) -> WriteOp {
    WriteOp::Bulk {
        pattern: pattern.to_string(),
        glob: match_glob(pattern),
        action,
    }
}

/// A permit from the gate on a local profile with writes on (the answer `y`).
fn permit(op: WriteOp, port: u16) -> Permit {
    Permit::grant(
        op,
        &profile(port, Env::Local, None),
        &Settings::default(),
        &Refusals::default(),
        &Answer::Yes,
    )
    .expect("a permit")
}

#[derive(Debug, Default)]
struct Totals {
    batches: u32,
    seen: u64,
    sent: u64,
    changed: u64,
    skipped: u64,
    failed: u64,
}

/// Every batch of `permit`, as the TUI and the CLI run them.
async fn run_all(
    conn: &mut MultiplexedConnection,
    budget: &Budget,
    permit: &Permit,
    file: &mut KeyFile,
) -> Totals {
    let mut t = Totals::default();
    let (mut walk, mut carry) = (Walk::Start, Vec::new());
    loop {
        let b = bulk::batch(conn, budget, permit, walk, carry, file)
            .await
            .unwrap();
        assert!(b.seen <= bulk::BATCH as u64);
        t.batches += 1;
        t.seen += b.seen;
        t.sent += b.sent;
        t.changed += b.changed;
        t.skipped += b.skipped;
        t.failed += b.failed;
        if b.finished() {
            return t;
        }
        (walk, carry) = (b.walk, b.carry);
    }
}

fn file_names(path: &Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let names: Vec<String> = text
        .lines()
        .map(|l| bulk::unescape_name(l).unwrap())
        .collect();
    let set: BTreeSet<String> = names.iter().cloned().collect();
    assert_eq!(set.len(), names.len(), "each key once");
    set
}

#[cfg(unix)]
fn assert_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{}", path.display());
}

#[cfg(not(unix))]
fn assert_private(_: &Path) {}

async fn dry_run_expire_and_delete(server: common::TestServer) {
    let mut admin = server.admin().await;
    let no_ttl = seed(&mut admin).await;
    let budget = Budget::new(Some(50_000));
    let mut conn = server.admin().await;

    // The dry run counts the pattern's keys only, and those with a TTL.
    let count = bulk::dry_run(&mut conn, &budget, CART, &match_glob(CART), |_| {})
        .await
        .unwrap();
    assert!(count.finished());
    assert_eq!((count.matched, count.with_ttl), (600, 100));
    assert_eq!(count.other_pattern, 1, "user:admin:cart");
    assert_eq!(
        count.to_change(&BulkAction::Expire { seconds: 60 }),
        no_ttl.len() as u64
    );

    let dir = temp_dir("run");
    if has_expire_nx(&mut admin).await {
        // Expire: only keys of the pattern without a TTL change.
        let action = BulkAction::Expire { seconds: 86_400 };
        let p = permit(bulk_op(CART, action), server.port);
        let mut file = KeyFile::new(&dir, "it", &action, 1_700_000_000);
        let t = run_all(&mut conn, &budget, &p, &mut file).await;
        assert_eq!((t.changed, t.skipped, t.failed), (500, 100, 0), "{t:?}");
        assert_eq!(t.seen, 600);
        assert!(t.batches >= 2, "500 keys a batch: {t:?}");
        for k in &no_ttl {
            let s = ttl(&mut admin, k).await;
            assert!((86_000..=86_400).contains(&s), "{k}: {s}");
        }
        for i in (0..600).step_by(6) {
            let s = ttl(&mut admin, &format!("user:{}:cart", 1000 + i)).await;
            assert!((0..=3600).contains(&s), "an old TTL stays: {s}");
        }
        assert_eq!(ttl(&mut admin, ADMIN_CART).await, -1);
        assert_eq!(ttl(&mut admin, "user:5000:profile").await, -1);
        // The key file holds exactly the keys sent EXPIRE: those without a TTL.
        assert_eq!(file_names(file.path()), no_ttl);
        assert_private(file.path());
        assert!(
            file.path()
                .to_string_lossy()
                .ends_with("20231114T221320Z-expire.txt")
        );
    } else {
        // Redis 6.2: no EXPIRE NX, so the CLI refuses with the version reason.
        let p = profile(server.port, Env::Local, Some(false));
        let config = temp_config(&p);
        let e = run_cli(&config, &dir, cli_args(CART, true, true, None))
            .await
            .0
            .unwrap_err();
        assert!(e.to_string().contains("needs Redis 7.0 or later"), "{e}");
        assert_eq!(ttl(&mut admin, "user:1001:cart").await, -1);
    }

    // Delete: every key of the pattern, nothing else.
    let action = BulkAction::Delete;
    let p = permit(bulk_op(CART, action), server.port);
    let mut file = KeyFile::new(&dir, "it", &action, 1_700_000_000);
    let t = run_all(&mut conn, &budget, &p, &mut file).await;
    assert_eq!((t.changed, t.sent, t.failed), (600, 600, 0), "{t:?}");
    for i in [0, 1, 599] {
        assert!(!exists(&mut admin, &format!("user:{}:cart", 1000 + i)).await);
    }
    assert!(exists(&mut admin, ADMIN_CART).await);
    assert!(exists(&mut admin, "user:5049:profile").await);
    let names = file_names(file.path());
    assert_eq!(names.len(), 600);
    assert!(names.iter().all(|k| key_in_pattern(k, CART)));
    assert_private(file.path());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn redis_dry_run_expire_and_delete() {
    dry_run_expire_and_delete(common::redis().await).await;
}

#[tokio::test]
async fn valkey_dry_run_expire_and_delete() {
    dry_run_expire_and_delete(common::valkey().await).await;
}

fn cli_args(pattern: &str, apply: bool, yes: bool, confirm: Option<&str>) -> ExpireArgs {
    ExpireArgs {
        profile: "it".into(),
        pattern: pattern.into(),
        ttl: 90,
        apply,
        yes,
        confirm: confirm.map(str::to_string),
        max_rate: Some(10_000),
    }
}

async fn run_cli(
    config: &Path,
    dir: &Path,
    args: ExpireArgs,
) -> (anyhow::Result<redis_lens::cli::expire::Outcome>, String) {
    let mut out = Vec::new();
    let ask = |_: &str, hint: &str| -> anyhow::Result<String> {
        anyhow::bail!("no terminal in tests; {hint}")
    };
    let r = execute(
        config,
        &MemoryStore::default(),
        &args,
        dir,
        &ask,
        &mut out,
        &mut |_| {},
    )
    .await;
    (r, String::from_utf8(out).unwrap())
}

async fn cli_expire(server: common::TestServer) {
    let mut admin = server.admin().await;
    seed(&mut admin).await;
    let dir = temp_dir("cli");

    // Read-only: refused with exit code 2, nothing written, no key file.
    for (env, ro) in [(Env::Prod, None), (Env::Local, Some(true))] {
        let config = temp_config(&profile(server.port, env, ro));
        for apply in [false, true] {
            let (r, out) = run_cli(&config, &dir, cli_args(PROFILE, apply, apply, None)).await;
            let e = r.unwrap_err();
            assert_eq!(exit_code(&e), 2, "{e}");
            assert!(e.to_string().contains("read-only"), "{e}");
            assert!(out.is_empty());
        }
    }
    assert_eq!(ttl(&mut admin, "user:5000:profile").await, -1);
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "no key file"
    );

    if !has_expire_nx(&mut admin).await {
        let config = temp_config(&profile(server.port, Env::Local, None));
        let (r, _) = run_cli(&config, &dir, cli_args(PROFILE, false, false, None)).await;
        assert!(r.unwrap_err().to_string().contains("Redis 6.2"));
        return;
    }

    // Without --apply: a dry run, nothing changes.
    let config = temp_config(&profile(server.port, Env::Local, None));
    let (r, out) = run_cli(&config, &dir, cli_args(PROFILE, false, false, None)).await;
    let o = r.unwrap();
    assert_eq!((o.count.matched, o.count.with_ttl), (50, 0));
    assert!(o.run.is_none());
    assert!(out.contains("Dry run: nothing was changed"), "{out}");
    assert_eq!(ttl(&mut admin, "user:5000:profile").await, -1);

    // --apply with no terminal and no --yes: refused after the dry run.
    let (r, _) = run_cli(&config, &dir, cli_args(PROFILE, true, false, None)).await;
    assert!(r.unwrap_err().to_string().contains("pass --yes"));
    assert_eq!(ttl(&mut admin, "user:5000:profile").await, -1);

    // Prod with writes on: --yes is not enough, --confirm with the pattern is.
    let prod = temp_config(&profile(server.port, Env::Prod, Some(false)));
    let (r, _) = run_cli(&prod, &dir, cli_args(PROFILE, true, true, None)).await;
    assert!(r.unwrap_err().to_string().contains("--confirm"));
    let (r, out) = run_cli(&prod, &dir, cli_args(PROFILE, true, false, Some(PROFILE))).await;
    let run = r.unwrap().run.expect("a run");
    assert_eq!((run.changed, run.skipped, run.failed), (50, 0, 0));
    for i in 0..50 {
        let s = ttl(&mut admin, &format!("user:{}:profile", 5000 + i)).await;
        assert!((1..=90).contains(&s), "{s}");
    }
    // The carts were not touched.
    assert_eq!(ttl(&mut admin, "user:1001:cart").await, -1);
    let file = run.file.expect("a key file");
    assert!(file.starts_with(&dir));
    assert_eq!(file_names(&file).len(), 50);
    assert_private(&file);
    assert!(out.contains("PERSIST on those keys undoes it."), "{out}");

    // Again: every key has a TTL now, so nothing is sent.
    let (r, out) = run_cli(&config, &dir, cli_args(PROFILE, true, true, None)).await;
    assert!(r.unwrap().run.is_none());
    assert!(out.contains("Nothing to change"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn redis_cli_expire() {
    cli_expire(common::redis().await).await;
}

#[tokio::test]
async fn valkey_cli_expire() {
    cli_expire(common::valkey().await).await;
}

/// A read-only profile gets no permit, whatever the answer: the runner can't start.
#[test]
fn read_only_gets_no_permit() {
    for (env, ro) in [(Env::Prod, None), (Env::Stage, Some(true))] {
        for answer in [Answer::None, Answer::Yes, Answer::Typed(CART.into())] {
            let r = Permit::grant(
                bulk_op(CART, BulkAction::Delete),
                &profile(1, env, ro),
                &Settings::default(),
                &Refusals::default(),
                &answer,
            );
            assert!(r.unwrap_err().contains("read-only"));
        }
    }
}
