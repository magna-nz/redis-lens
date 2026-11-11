//! Helpers for integration tests: start throwaway Redis and Valkey containers.
#![allow(dead_code)]

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, CopyTargetOptions, GenericImage, ImageExt};

/// Environment variable that overrides the Redis image, as `name:tag`
/// (e.g. `redis:6.2-alpine`). Unset, the default is `redis:7.4-alpine`.
pub const REDIS_IMAGE_ENV: &str = "REDIS_LENS_IT_REDIS_IMAGE";
const DEFAULT_REDIS_IMAGE: (&str, &str) = ("redis", "7.4-alpine");

/// The Redis image tests run against: `redis:7.4-alpine` unless `REDIS_LENS_IT_REDIS_IMAGE`
/// overrides it. The tag is split off at the last `:` after the last `/`.
pub fn redis_image() -> (String, String) {
    match std::env::var(REDIS_IMAGE_ENV) {
        Ok(v) if !v.trim().is_empty() => {
            let v = v.trim();
            let name_start = v.rfind('/').map_or(0, |i| i + 1);
            match v[name_start..].rfind(':') {
                Some(i) => (
                    v[..name_start + i].to_string(),
                    v[name_start + i + 1..].to_string(),
                ),
                None => panic!("{REDIS_IMAGE_ENV} must be `name:tag`, got `{v}`"),
            }
        }
        _ => (
            DEFAULT_REDIS_IMAGE.0.to_string(),
            DEFAULT_REDIS_IMAGE.1.to_string(),
        ),
    }
}

const VALKEY_IMAGE: (&str, &str) = ("valkey/valkey", "8.1-alpine");

fn valkey_image() -> (String, String) {
    (VALKEY_IMAGE.0.to_string(), VALKEY_IMAGE.1.to_string())
}

pub struct TestServer {
    pub port: u16,
    container: ContainerAsync<GenericImage>,
}

impl TestServer {
    pub fn url(&self) -> String {
        format!("redis://127.0.0.1:{}", self.port)
    }

    /// The container's address on Docker's bridge network: how another container reaches it.
    pub async fn bridge_ip(&self) -> String {
        self.container
            .get_bridge_ip_address()
            .await
            .expect("bridge ip")
            .to_string()
    }

    /// A connection as the default user, for seeding data.
    pub async fn admin(&self) -> redis::aio::MultiplexedConnection {
        redis::Client::open(self.url())
            .expect("valid url")
            .get_multiplexed_async_connection()
            .await
            .expect("connect")
    }

    /// A connection as an ACL user, for servers whose default user is off.
    pub async fn admin_as(&self, user: &str, password: &str) -> redis::aio::MultiplexedConnection {
        redis::Client::open(format!("redis://{user}:{password}@127.0.0.1:{}", self.port))
            .expect("valid url")
            .get_multiplexed_async_connection()
            .await
            .expect("connect")
    }
}

async fn start(image: (String, String), server_bin: &str, args: &[&str]) -> TestServer {
    start_on(image, server_bin, args, None, &[]).await
}

/// [`start`], publishing port 6379 on `host_port` when given, with `files` (container
/// path, contents) copied in before the server starts.
async fn start_on(
    image: (String, String),
    server_bin: &str,
    args: &[&str],
    host_port: Option<u16>,
    files: &[(&str, &[u8])],
) -> TestServer {
    let mut cmd = vec![server_bin.to_string()];
    cmd.extend(args.iter().map(|a| a.to_string()));
    let mut container = GenericImage::new(image.0, image.1)
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_cmd(cmd);
    for (path, data) in files {
        container = container.with_copy_to(
            CopyTargetOptions::new(*path).with_mode(0o644),
            data.to_vec(),
        );
    }
    let container = match host_port {
        Some(p) => container.with_mapped_port(p, 6379.tcp()),
        None => container,
    }
    .start()
    .await
    .expect("start container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(6379.tcp())
        .await
        .expect("mapped port");
    TestServer { port, container }
}

/// Redis with no auth (7.4 by default; see [`redis_image`]).
pub async fn redis() -> TestServer {
    start(redis_image(), "redis-server", &[]).await
}

/// Valkey 8.1 with no auth.
pub async fn valkey() -> TestServer {
    start(valkey_image(), "valkey-server", &[]).await
}

/// Redis (7.4 by default, overridable) started with extra server arguments, e.g. `&["--maxmemory-policy", "allkeys-lfu"]`.
///
/// Pass every word as its own argument. For an ACL user that is
/// `&["--user", "lens", "on", ">pw", "~*", "+@all"]`; a single `"lens on >pw"` string
/// becomes a username with spaces in it and Redis refuses to start.
pub async fn redis_with(args: &[&str]) -> TestServer {
    start(redis_image(), "redis-server", args).await
}

/// Valkey 8.1 started with extra server arguments, e.g. `&["--maxmemory-policy", "allkeys-lfu"]`.
/// Pass every word as its own argument, as for [`redis_with`].
pub async fn valkey_with(args: &[&str]) -> TestServer {
    start(valkey_image(), "valkey-server", args).await
}

/// A port that is free on the host right now. Another process could grab it before it is
/// used, which is acceptable for tests.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind a free port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A replica of `primary` (Redis, 7.4 by default and overridable, or Valkey 8.1 when `valkey`) that tells its primary
/// to list it as `127.0.0.1:<announce_port>`. With `None` it announces the port it is
/// published on, so the host can reach it through the primary's `INFO replication`.
pub async fn replica_of(
    primary: &TestServer,
    valkey: bool,
    announce_port: Option<u16>,
) -> TestServer {
    replica_of_with(primary, valkey, announce_port, &[]).await
}

/// [`replica_of`] with extra server arguments, one per word as for [`redis_with`]. For
/// a primary with ACL users that includes `--masteruser` and `--masterauth`.
pub async fn replica_of_with(
    primary: &TestServer,
    valkey: bool,
    announce_port: Option<u16>,
    extra: &[&str],
) -> TestServer {
    replica_start(primary, valkey, announce_port, extra, &[]).await
}

/// [`replica_of_with`], with `files` copied into the container first.
async fn replica_start(
    primary: &TestServer,
    valkey: bool,
    announce_port: Option<u16>,
    extra: &[&str],
    files: &[(&str, &[u8])],
) -> TestServer {
    let host_port = free_port();
    let announce = announce_port.unwrap_or(host_port).to_string();
    let ip = primary.bridge_ip().await;
    let mut args = vec![
        "--replicaof",
        ip.as_str(),
        "6379",
        "--replica-announce-ip",
        "127.0.0.1",
        "--replica-announce-port",
        announce.as_str(),
    ];
    args.extend_from_slice(extra);
    let (image, bin) = if valkey {
        (valkey_image(), "valkey-server")
    } else {
        (redis_image(), "redis-server")
    };
    start_on(image, bin, &args, Some(host_port), files).await
}

/// Redis (7.4 by default, overridable; or Valkey 8.1 when `valkey`) with extra server arguments.
pub async fn server_with(valkey: bool, args: &[&str]) -> TestServer {
    if valkey {
        valkey_with(args).await
    } else {
        redis_with(args).await
    }
}

/// A throwaway certificate authority for TLS tests, generated at test time.
pub struct TestCa {
    issuer: rcgen::CertifiedIssuer<'static, rcgen::KeyPair>,
}

impl TestCa {
    pub fn new(name: &str) -> Self {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().expect("CA key");
        let issuer = rcgen::CertifiedIssuer::self_signed(params, key).expect("CA certificate");
        Self { issuer }
    }

    /// The CA certificate, PEM.
    pub fn pem(&self) -> String {
        self.issuer.pem()
    }

    /// A certificate for these names (IP addresses become IP SANs), signed by this CA, for
    /// a server that verifies its own peers against `peer_ca`.
    pub fn server(&self, names: &[&str], peer_ca: &TestCa) -> ServerTls {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let mut params = rcgen::CertificateParams::new(names).expect("server params");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "redis-lens test server");
        // The replica also presents its certificate as a client on the replication link.
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = rcgen::KeyPair::generate().expect("server key");
        let cert = params
            .signed_by(&key, &self.issuer)
            .expect("server certificate");
        ServerTls {
            cert: cert.pem(),
            key: key.serialize_pem(),
            ca: peer_ca.pem(),
        }
    }
}

/// A server's TLS files, PEM: its certificate and key, and the CA it checks peers
/// against (for a replica, the primary's certificate on the replication link).
#[derive(Clone)]
pub struct ServerTls {
    pub cert: String,
    pub key: String,
    pub ca: String,
}

const TLS_CERT: &str = "/tls/server.crt";
const TLS_KEY: &str = "/tls/server.key";
const TLS_CA: &str = "/tls/ca.crt";

impl ServerTls {
    fn files(&self) -> [(&'static str, &[u8]); 3] {
        [
            (TLS_CERT, self.cert.as_bytes()),
            (TLS_KEY, self.key.as_bytes()),
            (TLS_CA, self.ca.as_bytes()),
        ]
    }
}

/// Serve TLS only, on 6379. Clients need no certificate; replication runs over TLS.
const TLS_ARGS: [&str; 14] = [
    "--tls-port",
    "6379",
    "--port",
    "0",
    "--tls-cert-file",
    TLS_CERT,
    "--tls-key-file",
    TLS_KEY,
    "--tls-ca-cert-file",
    TLS_CA,
    "--tls-auth-clients",
    "no",
    "--tls-replication",
    "yes",
];

/// Redis (7.4 by default, overridable) or Valkey 8.1 serving only TLS with `tls`.
pub async fn tls_server(valkey: bool, tls: &ServerTls) -> TestServer {
    let (image, bin) = if valkey {
        (valkey_image(), "valkey-server")
    } else {
        (redis_image(), "redis-server")
    };
    start_on(image, bin, &TLS_ARGS, None, &tls.files()).await
}

/// A TLS-only replica of the TLS `primary`, announced as `127.0.0.1:<published port>`
/// as [`replica_of`] does, replicating over TLS. Redis and Valkey check the primary's
/// certificate against `tls.ca` but not its host name, so the primary's certificate
/// needs no SAN for its bridge address.
pub async fn tls_replica_of(primary: &TestServer, valkey: bool, tls: &ServerTls) -> TestServer {
    replica_start(primary, valkey, None, &TLS_ARGS, &tls.files()).await
}
