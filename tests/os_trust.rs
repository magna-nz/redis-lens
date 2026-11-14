//! TLS against the real OS trust store, on macOS, Windows and Linux.
//!
//! CI generates a throwaway CA, installs it in the OS store (System keychain, Windows
//! `Root` store, `update-ca-certificates`) and passes its certificate and key through
//! `REDIS_LENS_OS_TRUST_CA` and `REDIS_LENS_OS_TRUST_CA_KEY` (PEM file paths). The tests
//! sign a `localhost` certificate with that CA, serve it from an in-process TLS server
//! that speaks just enough RESP, and connect through redis-lens, so through the trust
//! roots it loads from the OS store. A certificate from a CA that is not installed must
//! be refused.
//!
//! The tests are `#[ignore]`d, so a plain `cargo test` reports them as ignored and never
//! needs the OS store changed. CI runs them with `cargo test --test os_trust -- --ignored`
//! and they fail, not skip, when `REDIS_LENS_OS_TRUST_CA` is unset. `SSL_CERT_FILE` / `SSL_CERT_DIR`
//! replace the OS store, and on Linux cargo sets them for every process it starts
//! (through openssl-probe), so each test re-runs itself in a child process without them.
//! To check the test body itself without touching the OS store, point `SSL_CERT_FILE` at
//! the CA and set `REDIS_LENS_OS_TRUST_VIA_CERT_FILE=1`.
//!
//! Not Docker-gated: it runs (with `--ignored`) in the unit test job on every OS.

use std::io;
use std::sync::Arc;

use redis_lens::conn;
use redis_lens::model::{Auth, Endpoint, Env, Profile};
use redis_lens::secrets::MemoryStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const CA_ENV: &str = "REDIS_LENS_OS_TRUST_CA";
const KEY_ENV: &str = "REDIS_LENS_OS_TRUST_CA_KEY";
const VIA_CERT_FILE_ENV: &str = "REDIS_LENS_OS_TRUST_VIA_CERT_FILE";
/// Set in the child process that [`rerun_without_cert_env`] starts.
const CHILD_ENV: &str = "REDIS_LENS_OS_TRUST_CHILD";

/// Re-run test `name` in a child process without `SSL_CERT_FILE` and `SSL_CERT_DIR`, when
/// either is set and the test would run (cargo sets them on Linux, and they would replace
/// the OS store under test). Returns true when the child ran it and passed, so the caller
/// returns; false when the test should run here.
fn rerun_without_cert_env(name: &str) -> bool {
    let set = ["SSL_CERT_FILE", "SSL_CERT_DIR"]
        .iter()
        .any(|v| std::env::var_os(v).is_some());
    if !set
        || std::env::var_os(CA_ENV).is_none()
        || std::env::var_os(VIA_CERT_FILE_ENV).is_some()
        || std::env::var_os(CHILD_ENV).is_some()
    {
        return false;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            name,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD_ENV, "1")
        .env_remove("SSL_CERT_FILE")
        .env_remove("SSL_CERT_DIR")
        .output()
        .expect("run the test in a child process");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    print!("{stdout}");
    eprint!("{stderr}");
    assert!(out.status.success(), "child process failed: {}", out.status);
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the child did not run `{name}`"
    );
    true
}

/// The installed CA, as an issuer. Panics when the test is not set up.
fn installed_ca() -> rcgen::Issuer<'static, rcgen::KeyPair> {
    let ca_path = std::env::var_os(CA_ENV).unwrap_or_else(|| {
        panic!("{CA_ENV} is not set: no CA was installed in the OS store (CI sets it)")
    });
    let key_path =
        std::env::var_os(KEY_ENV).unwrap_or_else(|| panic!("{CA_ENV} is set but {KEY_ENV} is not"));
    if std::env::var_os(VIA_CERT_FILE_ENV).is_none() {
        for var in ["SSL_CERT_FILE", "SSL_CERT_DIR"] {
            assert!(
                std::env::var_os(var).is_none(),
                "{var} is set: it replaces the OS store this test checks. Unset it, or set \
                 {VIA_CERT_FILE_ENV}=1 to check the test body against SSL_CERT_FILE instead"
            );
        }
    }
    let ca_pem = std::fs::read_to_string(&ca_path)
        .unwrap_or_else(|e| panic!("read {CA_ENV} ({}): {e}", ca_path.display()));
    let key_pem = std::fs::read_to_string(&key_path)
        .unwrap_or_else(|e| panic!("read {KEY_ENV} ({}): {e}", key_path.display()));
    let key = rcgen::KeyPair::from_pem(&key_pem).expect("the CA key (PKCS#8 PEM)");
    rcgen::Issuer::from_ca_cert_pem(&ca_pem, key).expect("the CA certificate (PEM)")
}

/// A CA that is installed nowhere.
fn other_ca() -> rcgen::CertifiedIssuer<'static, rcgen::KeyPair> {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "redis-lens uninstalled CA");
    let key = rcgen::KeyPair::generate().expect("CA key");
    rcgen::CertifiedIssuer::self_signed(params, key).expect("CA certificate")
}

/// A server certificate for `localhost` (and `127.0.0.1`) signed by `issuer`.
fn leaf(
    issuer: &rcgen::Issuer<'_, impl rcgen::SigningKey>,
) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("server params");
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "redis-lens OS trust test server");
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let key = rcgen::KeyPair::generate().expect("server key");
    let cert = params.signed_by(&key, issuer).expect("server certificate");
    let key = PrivateKeyDer::from_pem_slice(key.serialize_pem().as_bytes()).expect("server key");
    (cert.der().clone(), key)
}

/// Start a TLS server presenting `cert` on `localhost` and return its port. It listens
/// on 127.0.0.1 and, when it can, on ::1 with the same port, whichever `localhost`
/// resolves to. It answers `PING` with `PONG` and every other command with `OK`.
async fn serve(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> u16 {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("server TLS config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let v4 = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1");
    let port = v4.local_addr().expect("local address").port();
    let v6 = TcpListener::bind(("::1", port)).await.ok();
    for listener in std::iter::once(v4).chain(v6) {
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // A refused handshake is the client's doing; nothing to report here.
                    if let Ok(tls) = acceptor.accept(tcp).await {
                        let _ = answer(tls).await;
                    }
                });
            }
        });
    }
    port
}

async fn answer(stream: impl AsyncRead + AsyncWrite + Unpin) -> io::Result<()> {
    let (r, mut w) = tokio::io::split(stream);
    let mut r = BufReader::new(r);
    while let Some(args) = read_command(&mut r).await? {
        let reply: &[u8] = match args.first() {
            Some(cmd) if cmd.eq_ignore_ascii_case(b"PING") => b"+PONG\r\n",
            _ => b"+OK\r\n",
        };
        w.write_all(reply).await?;
        w.flush().await?;
    }
    Ok(())
}

/// One RESP command (an array of bulk strings), or `None` at the end of the stream.
async fn read_command(r: &mut (impl AsyncBufReadExt + Unpin)) -> io::Result<Option<Vec<Vec<u8>>>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let count = |line: &str, prefix: char| -> io::Result<usize> {
        line.trim_end()
            .strip_prefix(prefix)
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| io::Error::other(format!("unexpected RESP line {line:?}")))
    };
    let n = count(&line, '*')?;
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        line.clear();
        r.read_line(&mut line).await?;
        let len = count(&line, '$')?;
        let mut arg = vec![0; len + 2];
        r.read_exact(&mut arg).await?;
        arg.truncate(len);
        args.push(arg);
    }
    Ok(Some(args))
}

fn profile(port: u16) -> Profile {
    Profile {
        name: "os-trust".to_string(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "localhost".into(),
            port,
            tls: true,
        },
        db: 0,
        auth: Auth::None,
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
    }
}

#[tokio::test]
#[ignore = "needs REDIS_LENS_OS_TRUST_CA (a CA in the OS trust store); CI runs it with --ignored"]
async fn a_server_signed_by_the_installed_ca_is_trusted() {
    if rerun_without_cert_env("a_server_signed_by_the_installed_ca_is_trusted") {
        return;
    }
    let ca = installed_ca();
    let (cert, key) = leaf(&ca);
    let port = serve(cert, key).await;
    let mut c = conn::connect(&profile(port), &MemoryStore::default())
        .await
        .unwrap_or_else(|e| {
            panic!("a certificate from the CA in the OS store must be trusted: {e}")
        });
    let pong: String = redis::cmd("PING").query_async(&mut c).await.unwrap();
    assert_eq!(pong, "PONG");
}

#[tokio::test]
#[ignore = "needs REDIS_LENS_OS_TRUST_CA (a CA in the OS trust store); CI runs it with --ignored"]
async fn a_server_signed_by_a_ca_not_installed_is_refused() {
    if rerun_without_cert_env("a_server_signed_by_a_ca_not_installed_is_refused") {
        return;
    }
    installed_ca();
    let other = other_ca();
    let (cert, key) = leaf(&other);
    let port = serve(cert, key).await;
    let err = conn::connect(&profile(port), &MemoryStore::default())
        .await
        .expect_err("a certificate from a CA outside the OS store must be refused");
    let msg = err.to_string();
    assert!(msg.contains("UnknownIssuer"), "{msg}");

    // The server itself is fine: a client that trusts the other CA, with full
    // verification, gets through. So the refusal above is about trust.
    let client = redis::Client::build_with_tls(
        format!("rediss://localhost:{port}"),
        redis::TlsCertificates {
            client_tls: None,
            root_cert: Some(other.pem().into_bytes()),
        },
    )
    .unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let pong: String = redis::cmd("PING").query_async(&mut c).await.unwrap();
    assert_eq!(pong, "PONG");
}
