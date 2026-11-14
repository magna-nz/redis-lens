//! Sentinel containers for integration tests (P4): one container running a primary, a
//! replica and Sentinels, every one on a host port published 1:1 and announcing
//! 127.0.0.1, so the host reaches each at the address the Sentinels give.
//!
//! Like [`super::cluster`], the servers share the container's network namespace and
//! talk over 127.0.0.1. The image's own `redis-server --sentinel` (or `valkey-server
//! --sentinel`) runs the Sentinels. The helper waits until the replica is online and
//! every Sentinel lists it, then the container prints a marker (Sentinel doesn't print
//! "Ready to accept connections").

use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use testcontainers::core::{CmdWaitFor, ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use super::{free_port, redis_image, valkey_image};

/// The name the Sentinels monitor the primary under.
pub const MASTER: &str = "mymaster";

/// The Sentinels' default user's password when they have an ACL user: their admin, also
/// used between Sentinels. Never given to redis-lens.
pub const SENTINEL_ADMIN_PW: &str = "sentinel-admin-pw-7c";

/// How the Sentinels themselves authenticate clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelAuth {
    None,
    /// `requirepass`: a password only.
    Password(&'static str),
    /// An ACL user with the minimal rules the spec names; the default user has
    /// [`SENTINEL_ADMIN_PW`].
    Acl {
        user: &'static str,
        password: &'static str,
    },
}

/// What to start.
#[derive(Debug, Clone, Copy)]
pub struct SentinelSetup {
    pub valkey: bool,
    /// How many Sentinels (each with quorum 1).
    pub sentinels: usize,
    /// The data nodes' `requirepass` (primary and replica), when they have one.
    pub data_password: Option<&'static str>,
    pub sentinel_auth: SentinelAuth,
}

impl SentinelSetup {
    pub fn new(valkey: bool) -> Self {
        Self {
            valkey,
            sentinels: 2,
            data_password: None,
            sentinel_auth: SentinelAuth::None,
        }
    }
}

/// A primary, its replica and the Sentinels watching them.
pub struct TestSentinel {
    pub primary_port: u16,
    pub replica_port: u16,
    pub sentinel_ports: Vec<u16>,
    pub setup: SentinelSetup,
    container: ContainerAsync<GenericImage>,
}

/// Quote one word for `sh`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Distinct free ports.
fn ports(n: usize) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(n);
    while out.len() < n {
        let p = free_port();
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Start what `setup` says. Ports taken by another process before Docker publishes them
/// are tried again with new ones.
pub async fn sentinel(setup: SentinelSetup) -> TestSentinel {
    for attempt in 1.. {
        match try_start(setup).await {
            Ok(s) => {
                s.wait_until_settled().await;
                return s;
            }
            Err(e)
                if attempt < 4
                    && (e.contains("address already in use")
                        || e.contains("ports are not available")) => {}
            Err(e) => panic!("start the Sentinel container (is Docker running?): {e}"),
        }
    }
    unreachable!()
}

async fn try_start(setup: SentinelSetup) -> Result<TestSentinel, String> {
    let all = ports(2 + setup.sentinels);
    let (primary, replica) = (all[0], all[1]);
    let sentinels = all[2..].to_vec();
    let (image, bin, cli) = if setup.valkey {
        (valkey_image(), "valkey-server", "valkey-cli")
    } else {
        (redis_image(), "redis-server", "redis-cli")
    };
    let data_auth = setup
        .data_password
        .map(|pw| format!("--requirepass {0} --masterauth {0}", sh_quote(pw)))
        .unwrap_or_default();
    let data_cli = setup
        .data_password
        .map(|pw| format!("-a {} --no-auth-warning", sh_quote(pw)))
        .unwrap_or_default();
    let (sentinel_conf, sentinel_cli) = sentinel_auth_lines(setup.sentinel_auth);
    let auth_pass = setup
        .data_password
        .map(|pw| format!("sentinel auth-pass {MASTER} {pw}\n"))
        .unwrap_or_default();
    let sentinel_list = sentinels
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let common = "--protected-mode no --save '' --appendonly no --repl-ping-replica-period 1 \
                  --replica-announce-ip 127.0.0.1";
    let script = format!(
        r#"set -e
{bin} --port {primary} {common} {data_auth} --daemonize yes --logfile /tmp/primary.log
{bin} --port {replica} {common} {data_auth} --replicaof 127.0.0.1 {primary} --daemonize yes --logfile /tmp/replica.log
until {cli} -p {primary} {data_cli} info replication 2>/dev/null | grep -q 'state=online'; do sleep 0.2; done
for s in {sentinel_list}; do
  cat > /tmp/sentinel-$s.conf <<EOF
port $s
pidfile /tmp/sentinel-$s.pid
protected-mode no
sentinel announce-ip 127.0.0.1
sentinel monitor {MASTER} 127.0.0.1 {primary} 1
sentinel down-after-milliseconds {MASTER} 2000
sentinel failover-timeout {MASTER} 6000
sentinel parallel-syncs {MASTER} 1
{auth_pass}{sentinel_conf}
EOF
  {bin} /tmp/sentinel-$s.conf --sentinel --daemonize yes --logfile /tmp/sentinel-$s.log
done
for s in {sentinel_list}; do
  until {cli} -p $s {sentinel_cli} sentinel replicas {MASTER} 2>/dev/null | grep -q '{replica}'; do sleep 0.2; done
done
echo SENTINEL-READY
exec tail -f /dev/null
"#
    );
    let mut image = GenericImage::new(image.0, image.1)
        .with_wait_for(WaitFor::message_on_stdout("SENTINEL-READY"));
    for p in &all {
        image = image.with_exposed_port(p.tcp());
    }
    let mut request = image
        .with_cmd(["sh".to_string(), "-c".to_string(), script])
        .with_startup_timeout(Duration::from_secs(120));
    for p in &all {
        request = request.with_mapped_port(*p, p.tcp());
    }
    let container = request.start().await.map_err(|e| e.to_string())?;
    Ok(TestSentinel {
        primary_port: primary,
        replica_port: replica,
        sentinel_ports: sentinels,
        setup,
        container,
    })
}

/// The Sentinel config lines for `auth`, and the CLI arguments that log in as the
/// Sentinels' admin.
fn sentinel_auth_lines(auth: SentinelAuth) -> (String, String) {
    let admin = |pw: &str| {
        (
            format!("requirepass {pw}\nsentinel sentinel-pass {pw}\n"),
            format!("-a {} --no-auth-warning", sh_quote(pw)),
        )
    };
    match auth {
        SentinelAuth::None => (String::new(), String::new()),
        SentinelAuth::Password(pw) => admin(pw),
        SentinelAuth::Acl { user, password } => {
            let (mut conf, cli) = admin(SENTINEL_ADMIN_PW);
            conf.push_str(&format!(
                "user {user} on >{password} resetchannels -@all +ping +hello +auth \
                 +sentinel|get-master-addr-by-name +sentinel|replicas\n"
            ));
            (conf, cli)
        }
    }
}

impl TestSentinel {
    fn cli_name(&self) -> &'static str {
        if self.setup.valkey {
            "valkey-cli"
        } else {
            "redis-cli"
        }
    }

    fn sentinel_cli_auth(&self) -> Vec<String> {
        match self.setup.sentinel_auth {
            SentinelAuth::None => vec![],
            SentinelAuth::Password(pw) => vec!["-a".into(), pw.into(), "--no-auth-warning".into()],
            SentinelAuth::Acl { .. } => vec![
                "-a".into(),
                SENTINEL_ADMIN_PW.into(),
                "--no-auth-warning".into(),
            ],
        }
    }

    /// Run the CLI in the container with these arguments.
    async fn cli(&self, args: &[String]) {
        let mut cmd = vec![self.cli_name().to_string()];
        cmd.extend(args.iter().cloned());
        self.container
            .exec(ExecCommand::new(cmd).with_cmd_ready_condition(CmdWaitFor::exit()))
            .await
            .expect("exec in the Sentinel container");
    }

    fn data_url(&self, port: u16) -> String {
        match self.setup.data_password {
            Some(pw) => format!("redis://:{pw}@127.0.0.1:{port}"),
            None => format!("redis://127.0.0.1:{port}"),
        }
    }

    /// A connection to a data node (`port`) as its default user, for seeding data.
    pub async fn admin(&self, port: u16) -> MultiplexedConnection {
        redis::Client::open(self.data_url(port))
            .expect("valid url")
            .get_multiplexed_async_connection()
            .await
            .expect("connect to a data node")
    }

    /// A connection to a Sentinel as its admin.
    pub async fn sentinel_admin(&self, port: u16) -> MultiplexedConnection {
        self.try_sentinel_admin(port)
            .await
            .expect("connect to a Sentinel")
    }

    async fn try_sentinel_admin(&self, port: u16) -> redis::RedisResult<MultiplexedConnection> {
        let url = match self.setup.sentinel_auth {
            SentinelAuth::None => format!("redis://127.0.0.1:{port}"),
            SentinelAuth::Password(pw) => format!("redis://:{pw}@127.0.0.1:{port}"),
            SentinelAuth::Acl { .. } => format!("redis://:{SENTINEL_ADMIN_PW}@127.0.0.1:{port}"),
        };
        // A Sentinel has no CLIENT SETINFO to answer.
        let info =
            redis::IntoConnectionInfo::into_connection_info(url.as_str()).expect("valid url");
        let settings = info.redis_settings().clone().set_skip_set_lib_name();
        let config = redis::AsyncConnectionConfig::new()
            .set_connection_timeout(Some(Duration::from_secs(2)))
            .set_response_timeout(Some(Duration::from_secs(2)));
        redis::Client::open(info.set_redis_settings(settings))?
            .get_multiplexed_async_connection_with_config(&config)
            .await
    }

    /// The primary's port as the Sentinel on `port` reports it now.
    pub async fn primary_from(&self, port: u16) -> Option<u16> {
        let mut c = self.sentinel_admin(port).await;
        let r: Option<(String, String)> = redis::cmd("SENTINEL")
            .arg("GET-MASTER-ADDR-BY-NAME")
            .arg(MASTER)
            .query_async(&mut c)
            .await
            .ok()?;
        r.and_then(|(_, p)| p.parse().ok())
    }

    /// The server's version, from the primary's `INFO server`.
    pub async fn version(&self) -> String {
        let mut c = self.admin(self.primary_port).await;
        let info: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut c)
            .await
            .unwrap();
        let field = if self.setup.valkey {
            "valkey_version:"
        } else {
            "redis_version:"
        };
        info.lines()
            .find_map(|l| l.trim().strip_prefix(field))
            .unwrap_or_default()
            .to_string()
    }

    /// Stop the Sentinel on `port` (its process is killed); the rest keep running.
    pub async fn stop_sentinel(&self, port: u16) {
        let kill = format!("kill $(cat /tmp/sentinel-{port}.pid)");
        self.container
            .exec(
                ExecCommand::new(["sh".to_string(), "-c".to_string(), kill])
                    .with_cmd_ready_condition(CmdWaitFor::exit()),
            )
            .await
            .expect("exec in the Sentinel container");
        // Docker's port proxy may still accept TCP connections: wait for PING to fail.
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.sentinel_answers(port).await {
            assert!(Instant::now() < deadline, "the Sentinel did not stop");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The Sentinel on `port` answers `PING` within 1 s.
    async fn sentinel_answers(&self, port: u16) -> bool {
        let ping = async {
            let mut c = self.try_sentinel_admin(port).await.ok()?;
            redis::cmd("PING").query_async::<String>(&mut c).await.ok()
        };
        matches!(
            tokio::time::timeout(Duration::from_secs(1), ping).await,
            Ok(Some(_))
        )
    }

    /// Wait until every Sentinel lists the replica as up: flags without `s_down`,
    /// `o_down` or `disconnected`, and `master-link-status` `ok` (a Sentinel learns that
    /// from the replica's own `INFO`, which it reads every few seconds).
    async fn wait_until_settled(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let mut ok = true;
            for p in &self.sentinel_ports {
                if !self.lists_replica_up(*p).await {
                    ok = false;
                }
            }
            if ok {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the Sentinels never listed the replica as up"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn lists_replica_up(&self, port: u16) -> bool {
        let Ok(mut c) = self.try_sentinel_admin(port).await else {
            return false;
        };
        let r: redis::RedisResult<Vec<Vec<String>>> = redis::cmd("SENTINEL")
            .arg("REPLICAS")
            .arg(MASTER)
            .query_async(&mut c)
            .await;
        let Ok(entries) = r else { return false };
        entries.iter().any(|e| {
            let field = |name: &str| {
                e.chunks(2)
                    .find(|kv| kv.len() == 2 && kv[0] == name)
                    .map(|kv| kv[1].as_str())
            };
            field("port") == Some(self.replica_port.to_string().as_str())
                && field("master-link-status") == Some("ok")
                && field("flags").is_some_and(|f| {
                    !f.split(',')
                        .any(|x| matches!(x, "s_down" | "o_down" | "disconnected"))
                })
        })
    }

    /// Ask the first Sentinel for a failover (`SENTINEL FAILOVER`) and wait until every
    /// running Sentinel names the replica as the primary. `false` when that doesn't
    /// happen within `wait`.
    pub async fn failover(&self, wait: Duration) -> bool {
        let first = self.sentinel_ports[0];
        let mut c = self.sentinel_admin(first).await;
        let r: redis::RedisResult<()> = redis::cmd("SENTINEL")
            .arg("FAILOVER")
            .arg(MASTER)
            .query_async(&mut c)
            .await;
        if r.is_err() {
            return false;
        }
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            let mut all = true;
            for p in &self.sentinel_ports {
                if self.primary_from(*p).await != Some(self.replica_port) {
                    all = false;
                }
            }
            if all && self.is_master(self.replica_port).await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        false
    }

    /// The data node on `port` says `role:master`.
    pub async fn is_master(&self, port: u16) -> bool {
        let mut c = self.admin(port).await;
        let info: String = redis::cmd("INFO")
            .arg("replication")
            .query_async(&mut c)
            .await
            .unwrap_or_default();
        info.contains("role:master")
    }
}
