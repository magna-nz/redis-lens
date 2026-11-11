//! TLS (`rediss://`) to a primary and to its replica, on Redis and Valkey.
//!
//! Each test trusts a throwaway CA generated at test time, through `SSL_CERT_FILE`,
//! which `rustls-native-certs` reads instead of the OS store. Verification is never
//! turned off: a server signed by another CA, or a certificate for another name, must be
//! refused.
//!
//! `SSL_CERT_FILE` is process-wide and redis-lens loads its trust roots once per
//! process, so it has to be set before the first TLS connection. Setting it from the
//! test would need `std::env::set_var`, which is `unsafe` in edition 2024, and the crate
//! forbids `unsafe_code`. So each test re-runs this binary for just itself, with
//! `SSL_CERT_FILE` set from the start, and the child process runs the body. The
//! certificates are generated once in the parent and handed over in a directory under
//! `CARGO_TARGET_TMPDIR`. Keep these tests in their own binary: anything else in it
//! would share the environment.
#![cfg(feature = "integration-tests")]

mod common;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{ServerTls, TestCa, TestServer};
use redis::aio::MultiplexedConnection;
use redis_lens::conn::{self, Replication, ScanTarget, check_permissions};
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::scan::{self, ScanOptions};
use redis_lens::secrets::MemoryStore;

const KEYS: u64 = 25;
/// Set in the child process: the directory holding the fixture files.
const DIR_ENV: &str = "REDIS_LENS_IT_TLS_DIR";
const CERT_FILE_ENV: &str = "SSL_CERT_FILE";
const TRUSTED_CA: &str = "trusted-ca.pem";
/// An empty file: an OS store with no certificates, so the bundled roots are used.
const EMPTY_STORE: &str = "empty.pem";

/// The server certificates the tests use. A [`ServerTls`] per server role.
struct Fixtures {
    /// Signed by the trusted CA, for `127.0.0.1` and `localhost`.
    good: ServerTls,
    /// Signed by the trusted CA, for a name the tests never connect to.
    wrong_name: ServerTls,
    /// Signed by another CA, for `127.0.0.1` and `localhost`.
    untrusted: ServerTls,
    /// The other CA, PEM.
    other_ca: String,
}

/// The fixture files, in the order [`fixture_dir`] writes them.
const FILES: [&str; 9] = [
    "good.crt",
    "good.key",
    "wrong-name.crt",
    "wrong-name.key",
    "untrusted.crt",
    "untrusted.key",
    "other-ca.pem",
    TRUSTED_CA,
    EMPTY_STORE,
];

/// Generate the CAs and certificates once per parent process and write them out.
fn fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("tls_it-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let trusted = TestCa::new("redis-lens test CA");
        let other = TestCa::new("redis-lens other CA");
        let good = trusted.server(&["127.0.0.1", "localhost"], &trusted);
        let wrong = trusted.server(&["wrong-name.invalid"], &trusted);
        // Its peer CA is the trusted one, so it can still check its primary as a replica.
        let untrusted = other.server(&["127.0.0.1", "localhost"], &trusted);
        let contents = [
            good.cert.clone(),
            good.key.clone(),
            wrong.cert.clone(),
            wrong.key.clone(),
            untrusted.cert.clone(),
            untrusted.key.clone(),
            other.pem(),
            trusted.pem(),
            String::new(),
        ];
        for (name, data) in FILES.into_iter().zip(contents) {
            std::fs::write(dir.join(name), data).expect("write a fixture file");
        }
        dir
    })
}

fn read_fixtures(dir: &Path) -> Fixtures {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).expect("read a fixture file");
    let trusted = read(TRUSTED_CA);
    let server = |name: &str| ServerTls {
        cert: read(&format!("{name}.crt")),
        key: read(&format!("{name}.key")),
        ca: trusted.clone(),
    };
    Fixtures {
        good: server("good"),
        wrong_name: server("wrong-name"),
        untrusted: server("untrusted"),
        other_ca: read("other-ca.pem"),
    }
}

/// Run `body` in a child process of this test binary whose `SSL_CERT_FILE` is the
/// fixture `cert_file` (normally [`TRUSTED_CA`]) from its start. In the child, run it
/// directly.
fn in_tls_process<F, Fut>(name: &str, cert_file: &str, body: F)
where
    F: FnOnce(Fixtures) -> Fut,
    Fut: Future<Output = ()>,
{
    if let Ok(dir) = std::env::var(DIR_ENV) {
        let dir = PathBuf::from(dir);
        assert_eq!(
            std::env::var_os(CERT_FILE_ENV),
            Some(dir.join(cert_file).into_os_string()),
            "{CERT_FILE_ENV} must be set to the fixture before the first TLS connection"
        );
        let fixtures = read_fixtures(&dir);
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(body(fixtures));
        return;
    }
    let dir = fixture_dir();
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([name, "--exact", "--nocapture", "--test-threads", "1"])
        .env(DIR_ENV, dir)
        .env(CERT_FILE_ENV, dir.join(cert_file))
        .env_remove("SSL_CERT_DIR")
        .output()
        .expect("run the test in a child process");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Through `print!`, so the harness shows it only for a failing test.
    print!("{stdout}");
    eprint!("{stderr}");
    assert!(out.status.success(), "child process failed: {}", out.status);
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the child did not run `{name}`"
    );
}

/// A test that runs in its own process, see [`in_tls_process`]. The trusted CA is the
/// OS store unless another fixture file is given.
macro_rules! tls_test {
    ($name:ident, $body:expr) => {
        tls_test!($name, TRUSTED_CA, $body);
    };
    ($name:ident, $cert_file:expr, $body:expr) => {
        #[test]
        fn $name() {
            in_tls_process(stringify!($name), $cert_file, $body);
        }
    };
}

fn tls_endpoint(port: u16) -> Endpoint {
    Endpoint {
        host: "127.0.0.1".into(),
        port,
        tls: true,
    }
}

fn profile(port: u16, prefer_replica: bool) -> Profile {
    Profile {
        name: "tls".to_string(),
        env: Env::Local,
        endpoint: tls_endpoint(port),
        db: 0,
        auth: Auth::None,
        read_only: None,
        prefer_replica,
    }
}

/// A connection through redis-lens itself, so through the roots under test.
async fn lens(server: &TestServer) -> MultiplexedConnection {
    conn::connect(&profile(server.port, false), &MemoryStore::default())
        .await
        .expect("TLS connection to a server signed by the trusted CA")
}

async fn info(c: &mut MultiplexedConnection) -> String {
    redis::cmd("INFO")
        .arg("replication")
        .query_async(c)
        .await
        .unwrap()
}

async fn seed(server: &TestServer) {
    let mut c = lens(server).await;
    for i in 0..KEYS {
        let _: () = redis::cmd("SET")
            .arg(format!("user:{i}:profile"))
            .arg("x")
            .query_async(&mut c)
            .await
            .unwrap();
    }
}

/// A seeded TLS primary and a TLS replica with `replica_tls`, once the primary lists
/// the replica as online (its initial sync, keys included, is then done).
async fn pair(valkey: bool, fx: &Fixtures, replica_tls: &ServerTls) -> (TestServer, TestServer) {
    let primary = common::tls_server(valkey, &fx.good).await;
    seed(&primary).await;
    let replica = common::tls_replica_of(&primary, valkey, replica_tls).await;
    let mut p = lens(&primary).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !matches!(
        conn::parse_replication(&info(&mut p).await),
        Replication::Master { online } if !online.is_empty()
    ) {
        assert!(
            Instant::now() < deadline,
            "TLS replication did not come up (valkey={valkey})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    (primary, replica)
}

async fn scan_count(c: &mut MultiplexedConnection, port: u16) -> u64 {
    let perms = check_permissions(c).await;
    scan::scan(
        "tls",
        c,
        &ScanOptions::for_profile(&profile(port, false)),
        &perms,
        None,
    )
    .await
    .unwrap()
    .sampled_keys
}

tls_test!(
    a_rediss_profile_connects_and_scans,
    |fx: Fixtures| async move {
        for valkey in [false, true] {
            let server = common::tls_server(valkey, &fx.good).await;
            seed(&server).await;
            let (mut c, target) =
                conn::connect_for_scan(&profile(server.port, false), &MemoryStore::default())
                    .await
                    .unwrap();
            assert_eq!(target, ScanTarget::Primary, "valkey={valkey}");
            assert_eq!(
                scan_count(&mut c, server.port).await,
                KEYS,
                "valkey={valkey}"
            );
        }
    }
);

tls_test!(
    prefer_replica_scans_the_tls_replica,
    |fx: Fixtures| async move {
        for valkey in [false, true] {
            let (primary, replica) = pair(valkey, &fx, &fx.good).await;
            let (mut c, target) =
                conn::connect_for_scan(&profile(primary.port, true), &MemoryStore::default())
                    .await
                    .unwrap();
            assert_eq!(
                target,
                ScanTarget::Replica(tls_endpoint(replica.port)),
                "valkey={valkey}"
            );
            let replica_info = info(&mut c).await;
            assert!(replica_info.contains("role:slave"), "valkey={valkey}");
            let primary_info = info(&mut lens(&primary).await).await;
            assert!(conn::master_replid(&primary_info).is_some());
            assert_eq!(
                conn::master_replid(&replica_info),
                conn::master_replid(&primary_info)
            );
            assert_eq!(
                scan_count(&mut c, primary.port).await,
                KEYS,
                "valkey={valkey}"
            );
        }
    }
);

tls_test!(
    a_server_signed_by_another_ca_is_refused,
    |fx: Fixtures| async move {
        for valkey in [false, true] {
            let server = common::tls_server(valkey, &fx.untrusted).await;
            let err = conn::connect(&profile(server.port, false), &MemoryStore::default())
                .await
                .expect_err("a certificate from an untrusted CA must be refused");
            let msg = err.to_string();
            assert!(msg.contains("UnknownIssuer"), "valkey={valkey}: {msg}");

            // The server itself is fine: a client that trusts the other CA, with full
            // verification, gets through. So the refusal above is about trust.
            let client = redis::Client::build_with_tls(
                format!("rediss://127.0.0.1:{}", server.port),
                redis::TlsCertificates {
                    client_tls: None,
                    root_cert: Some(fx.other_ca.clone().into_bytes()),
                },
            )
            .unwrap();
            let mut c = client.get_multiplexed_async_connection().await.unwrap();
            let pong: String = redis::cmd("PING").query_async(&mut c).await.unwrap();
            assert_eq!(pong, "PONG");
        }
    }
);

/// Expect `prefer_replica` to fall back to the primary because the replica's certificate
/// is refused, and return the reason.
async fn expect_fallback(primary: &TestServer, replica: &TestServer, valkey: bool) -> String {
    let (mut c, target) =
        conn::connect_for_scan(&profile(primary.port, true), &MemoryStore::default())
            .await
            .unwrap();
    let ScanTarget::PrimaryFallback(why) = target else {
        panic!("expected a fallback, got {target:?} (valkey={valkey})");
    };
    assert!(
        why.contains(&format!("127.0.0.1:{}", replica.port)),
        "valkey={valkey}: {why}"
    );
    assert!(info(&mut c).await.contains("role:master"));
    why
}

tls_test!(
    a_replica_signed_by_another_ca_is_not_scanned,
    |fx: Fixtures| async move {
        for valkey in [false, true] {
            let (primary, replica) = pair(valkey, &fx, &fx.untrusted).await;
            let why = expect_fallback(&primary, &replica, valkey).await;
            assert!(why.contains("UnknownIssuer"), "valkey={valkey}: {why}");
        }
    }
);

tls_test!(
    a_replica_whose_certificate_names_another_host_is_not_scanned,
    |fx: Fixtures| async move {
        for valkey in [false, true] {
            let (primary, replica) = pair(valkey, &fx, &fx.wrong_name).await;
            let why = expect_fallback(&primary, &replica, valkey).await;
            assert!(
                why.contains("not valid for name \"127.0.0.1\""),
                "valkey={valkey}: {why}"
            );
        }
    }
);

tls_test!(
    an_empty_os_store_falls_back_to_the_bundled_roots_only,
    EMPTY_STORE,
    |fx: Fixtures| async move {
        for valkey in [false, true] {
            // The OS store is empty, so the bundled public roots are used, and the test
            // CA is not among them: the same server the other tests reach is refused.
            let server = common::tls_server(valkey, &fx.good).await;
            let err = conn::connect(&profile(server.port, false), &MemoryStore::default())
                .await
                .expect_err("only the bundled roots are trusted");
            let msg = err.to_string();
            assert!(msg.contains("UnknownIssuer"), "valkey={valkey}: {msg}");
        }
    }
);
