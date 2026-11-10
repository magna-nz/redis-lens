//! Helpers for integration tests: start throwaway Redis and Valkey containers.
#![allow(dead_code)]

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

pub const REDIS_IMAGE: (&str, &str) = ("redis", "7.4-alpine");
pub const VALKEY_IMAGE: (&str, &str) = ("valkey/valkey", "8.1-alpine");

pub struct TestServer {
    pub port: u16,
    _container: ContainerAsync<GenericImage>,
}

impl TestServer {
    pub fn url(&self) -> String {
        format!("redis://127.0.0.1:{}", self.port)
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
    let mut cmd = vec![server_bin.to_string()];
    cmd.extend(args.iter().map(|a| a.to_string()));
    let container = GenericImage::new(image.0, image.1)
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_cmd(cmd)
        .start()
        .await
        .expect("start container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(6379.tcp())
        .await
        .expect("mapped port");
    TestServer {
        port,
        _container: container,
    }
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
