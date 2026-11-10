//! Helpers for integration tests: start throwaway Redis and Valkey containers.
#![allow(dead_code)]

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

pub const REDIS_IMAGE: (&str, &str) = ("redis", "7.4-alpine");
pub const VALKEY_IMAGE: (&str, &str) = ("valkey/valkey", "8.1-alpine");

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
}

async fn start(image: (&str, &str), server_bin: &str, args: &[&str]) -> TestServer {
    start_on(image, server_bin, args, None).await
}

/// [`start`], publishing port 6379 on `host_port` when given.
async fn start_on(
    image: (&str, &str),
    server_bin: &str,
    args: &[&str],
    host_port: Option<u16>,
) -> TestServer {
    let mut cmd = vec![server_bin.to_string()];
    cmd.extend(args.iter().map(|a| a.to_string()));
    let container = GenericImage::new(image.0, image.1)
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_cmd(cmd);
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

/// Redis 7.4 with no auth.
pub async fn redis() -> TestServer {
    start(REDIS_IMAGE, "redis-server", &[]).await
}

/// Valkey 8.1 with no auth.
pub async fn valkey() -> TestServer {
    start(VALKEY_IMAGE, "valkey-server", &[]).await
}

/// Redis 7.4 started with extra server arguments, e.g. `&["--maxmemory-policy", "allkeys-lfu"]`.
///
/// Pass every word as its own argument. For an ACL user that is
/// `&["--user", "lens", "on", ">pw", "~*", "+@all"]`; a single `"lens on >pw"` string
/// becomes a username with spaces in it and Redis refuses to start.
pub async fn redis_with(args: &[&str]) -> TestServer {
    start(REDIS_IMAGE, "redis-server", args).await
}

/// Valkey 8.1 started with extra server arguments, e.g. `&["--maxmemory-policy", "allkeys-lfu"]`.
/// Pass every word as its own argument, as for [`redis_with`].
pub async fn valkey_with(args: &[&str]) -> TestServer {
    start(VALKEY_IMAGE, "valkey-server", args).await
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

/// A replica of `primary` (Redis 7.4, or Valkey 8.1 when `valkey`) that tells its primary
/// to list it as `127.0.0.1:<announce_port>`. With `None` it announces the port it is
/// published on, so the host can reach it through the primary's `INFO replication`.
pub async fn replica_of(
    primary: &TestServer,
    valkey: bool,
    announce_port: Option<u16>,
) -> TestServer {
    let host_port = free_port();
    let announce = announce_port.unwrap_or(host_port).to_string();
    let ip = primary.bridge_ip().await;
    let args = [
        "--replicaof",
        ip.as_str(),
        "6379",
        "--replica-announce-ip",
        "127.0.0.1",
        "--replica-announce-port",
        announce.as_str(),
    ];
    let (image, bin) = if valkey {
        (VALKEY_IMAGE, "valkey-server")
    } else {
        (REDIS_IMAGE, "redis-server")
    };
    start_on(image, bin, &args, Some(host_port)).await
}
