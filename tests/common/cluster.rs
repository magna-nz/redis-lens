//! Cluster containers for integration tests (v0.3): one container running a whole
//! cluster on host ports published 1:1, announcing 127.0.0.1, so the host reaches
//! every node at the address the cluster announces. Filled in by the cluster agent.
//!
//! Every node runs in the one container, so they share a network namespace and gossip
//! over 127.0.0.1 on `port + 10000` (the bus port; Redis 6.2 has no `cluster-port`, so
//! ports stay below 55000). The cluster is made with `redis-cli --cluster create` (or
//! `valkey-cli`), and the helper waits for `cluster_state:ok` on every node and, with
//! replicas, for every replica to be online.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use testcontainers::core::{CmdWaitFor, ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use super::{free_port, redis_image, valkey_image};

/// A running cluster: 3 primaries, plus 3 replicas when asked.
pub struct TestCluster {
    /// Every node's port, which is also its host port and its announced port. The
    /// first three were given to `--cluster create` as primaries.
    pub ports: Vec<u16>,
    pub valkey: bool,
    container: ContainerAsync<GenericImage>,
}

/// Ports for `n` nodes from [`free_port`]: below 55000, distinct, and with no node's
/// bus port (`port + 10000`) equal to another node's port.
fn cluster_ports(n: usize) -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::with_capacity(n);
    for _ in 0..100_000 {
        if ports.len() == n {
            break;
        }
        let p = free_port();
        let clashes = ports.iter().any(|q| {
            *q == p || q.checked_add(10_000) == Some(p) || p.checked_add(10_000) == Some(*q)
        });
        if (1_024..55_000).contains(&p) && !clashes {
            ports.push(p);
        }
    }
    assert_eq!(ports.len(), n, "could not find {n} free ports below 55000");
    ports
}

/// Quote one word for `sh`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A cluster of Redis (7.4 by default, see [`super::redis_image`]) or Valkey 8.1, with a
/// replica per primary when `replicas`.
pub async fn cluster(valkey: bool, replicas: bool) -> TestCluster {
    cluster_with(valkey, replicas, &[]).await
}

/// [`cluster`], every node started with `extra` server arguments (one word each, as for
/// [`super::redis_with`]), for example ACL users. The default user stays as it is, so
/// it must stay usable for `--cluster create` and the helpers.
pub async fn cluster_with(valkey: bool, replicas: bool, extra: &[&str]) -> TestCluster {
    // A port from `free_port` can be taken by another process before Docker publishes
    // it: try again with new ports.
    for attempt in 1.. {
        match try_cluster(valkey, replicas, extra).await {
            Ok(c) => {
                c.wait_until_settled(replicas).await;
                return c;
            }
            Err(e)
                if attempt < 4
                    && (e.contains("address already in use")
                        || e.contains("ports are not available")) => {}
            Err(e) => panic!("start the cluster container (is Docker running?): {e}"),
        }
    }
    unreachable!()
}

async fn try_cluster(valkey: bool, replicas: bool, extra: &[&str]) -> Result<TestCluster, String> {
    let n = if replicas { 6 } else { 3 };
    let ports = cluster_ports(n);
    let (image, bin, cli) = if valkey {
        (valkey_image(), "valkey-server", "valkey-cli")
    } else {
        (redis_image(), "redis-server", "redis-cli")
    };
    let list = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let addrs = ports
        .iter()
        .map(|p| format!("127.0.0.1:{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    let extra = extra
        .iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        r#"for p in {list}; do
  mkdir -p /tmp/n$p
  {bin} --port $p --cluster-enabled yes --cluster-config-file /tmp/n$p/nodes.conf \
    --dir /tmp/n$p --cluster-announce-ip 127.0.0.1 --cluster-announce-port $p \
    --cluster-announce-bus-port $((p+10000)) --cluster-node-timeout 3000 \
    --protected-mode no --save '' --appendonly no --repl-ping-replica-period 1 \
    {extra} --daemonize yes --logfile /tmp/n$p/log
done
for p in {list}; do until {cli} -p $p ping >/dev/null 2>&1; do sleep 0.1; done; done
{cli} --cluster create {addrs} --cluster-replicas {r} --cluster-yes >/tmp/create.log 2>&1
for p in {list}; do
  until {cli} -p $p cluster info | grep -q cluster_state:ok; do sleep 0.2; done
done
echo CLUSTER-READY
exec tail -f /dev/null
"#,
        r = if replicas { 1 } else { 0 },
    );
    let mut image = GenericImage::new(image.0, image.1)
        .with_wait_for(WaitFor::message_on_stdout("CLUSTER-READY"));
    for p in &ports {
        image = image.with_exposed_port(p.tcp());
    }
    let mut request = image
        .with_cmd(["sh".to_string(), "-c".to_string(), script])
        .with_startup_timeout(Duration::from_secs(120));
    for p in &ports {
        request = request.with_mapped_port(*p, p.tcp());
    }
    let container = request.start().await.map_err(|e| e.to_string())?;
    Ok(TestCluster {
        ports,
        valkey,
        container,
    })
}

impl TestCluster {
    /// The port of the node used as the seed: the first primary.
    pub fn seed_port(&self) -> u16 {
        self.ports[0]
    }

    pub fn url(&self, port: u16) -> String {
        format!("redis://127.0.0.1:{port}")
    }

    /// A connection to one node as the default user.
    pub async fn admin(&self, port: u16) -> MultiplexedConnection {
        redis::Client::open(self.url(port))
            .expect("valid url")
            .get_multiplexed_async_connection()
            .await
            .expect("connect to a cluster node")
    }

    /// The server's version, from the seed's `INFO server`.
    pub async fn version(&self) -> String {
        let mut c = self.admin(self.seed_port()).await;
        let info: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut c)
            .await
            .unwrap();
        let field = if self.valkey {
            "valkey_version:"
        } else {
            "redis_version:"
        };
        info.lines()
            .find_map(|l| l.trim().strip_prefix(field))
            .unwrap_or_default()
            .to_string()
    }

    /// Redis 6.2: no `CLUSTER SHARDS`, and no `-command|subcommand` ACL rules.
    pub async fn is_redis6(&self) -> bool {
        !self.valkey && self.version().await.starts_with("6.")
    }

    /// Wait until every node says `cluster_state:ok`, and with `replicas` until every
    /// primary has an online replica and (where it exists) `CLUSTER SHARDS` reports
    /// every node `online`.
    async fn wait_until_settled(&self, replicas: bool) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if self.settled(replicas).await {
                return;
            }
            assert!(Instant::now() < deadline, "the cluster did not settle");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn settled(&self, replicas: bool) -> bool {
        let mut online_replicas = 0;
        for p in &self.ports {
            let mut c = self.admin(*p).await;
            let info: String = redis::cmd("CLUSTER")
                .arg("INFO")
                .query_async(&mut c)
                .await
                .unwrap_or_default();
            if !info.contains("cluster_state:ok") {
                return false;
            }
            let repl: String = redis::cmd("INFO")
                .arg("replication")
                .query_async(&mut c)
                .await
                .unwrap_or_default();
            if repl.contains("role:master") {
                online_replicas += repl.matches("state=online").count();
            }
        }
        if !replicas {
            return true;
        }
        if online_replicas < 3 {
            return false;
        }
        // The seed's own view (what CLUSTER SLOTS reports): every node connected,
        // three of them replicas.
        let mut c = self.admin(self.seed_port()).await;
        let nodes: String = redis::cmd("CLUSTER")
            .arg("NODES")
            .query_async(&mut c)
            .await
            .unwrap_or_default();
        let lines: Vec<&str> = nodes.lines().filter(|l| !l.trim().is_empty()).collect();
        let healthy = lines.len() == self.ports.len()
            && lines.iter().all(|l| {
                l.contains(" connected")
                    && !["fail", "handshake", "noaddr"]
                        .iter()
                        .any(|f| l.contains(f))
            });
        if !healthy || lines.iter().filter(|l| l.contains("slave")).count() != 3 {
            return false;
        }
        let shards: Result<redis::Value, _> = redis::cmd("CLUSTER")
            .arg("SHARDS")
            .query_async(&mut c)
            .await;
        match shards {
            // Redis 6.2: INFO replication is all there is.
            Err(_) => true,
            Ok(v) => {
                let words = flatten(&v);
                let health: Vec<&String> = words
                    .windows(2)
                    .filter(|w| w[0] == "health")
                    .map(|w| &w[1])
                    .collect();
                health.len() == self.ports.len() && health.iter().all(|h| *h == "online")
            }
        }
    }

    /// Write `keys` (key, value) through redis-rs's cluster client, which sends each
    /// key to the node serving its slot. Then wait until every replica has them.
    pub async fn seed(&self, keys: &[(String, String)]) {
        let client = redis::cluster::ClusterClient::new(vec![self.url(self.seed_port())])
            .expect("cluster client");
        let conn = client
            .get_async_connection()
            .await
            .expect("cluster connection");
        for chunk in keys.chunks(200) {
            let writes = chunk.iter().map(|(k, v)| {
                let mut c = conn.clone();
                async move {
                    redis::cmd("SET")
                        .arg(k)
                        .arg(v)
                        .query_async::<()>(&mut c)
                        .await
                        .expect("SET");
                }
            });
            futures::future::join_all(writes).await;
        }
        self.wait_for_replication().await;
    }

    /// Wait until every replica's `DBSIZE` equals its primary's.
    pub async fn wait_for_replication(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut by_id: HashMap<String, (String, u64)> = HashMap::new();
            for p in &self.ports {
                let mut c = self.admin(*p).await;
                let id: String = redis::cmd("CLUSTER")
                    .arg("MYID")
                    .query_async(&mut c)
                    .await
                    .unwrap();
                let size: u64 = redis::cmd("DBSIZE").query_async(&mut c).await.unwrap();
                let repl: String = redis::cmd("INFO")
                    .arg("replication")
                    .query_async(&mut c)
                    .await
                    .unwrap();
                by_id.insert(id, (repl, size));
            }
            let mut sizes: Vec<u64> = Vec::new();
            let mut primary_sizes: Vec<u64> = Vec::new();
            for (repl, size) in by_id.values() {
                if repl.contains("role:master") {
                    primary_sizes.push(*size);
                } else {
                    sizes.push(*size);
                }
            }
            sizes.sort_unstable();
            primary_sizes.sort_unstable();
            if sizes.is_empty() || sizes == primary_sizes {
                return;
            }
            assert!(Instant::now() < deadline, "the replicas did not catch up");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Run the cluster's CLI in the container (`redis-cli` or `valkey-cli`).
    pub async fn cli(&self, args: &[&str]) {
        let cli = if self.valkey {
            "valkey-cli"
        } else {
            "redis-cli"
        };
        let mut cmd = vec![cli.to_string()];
        cmd.extend(args.iter().map(|a| a.to_string()));
        self.container
            .exec(ExecCommand::new(cmd).with_cmd_ready_condition(CmdWaitFor::exit()))
            .await
            .expect("exec in the cluster container");
    }

    /// Stop the node on `port` (`SHUTDOWN NOSAVE`); the others keep running.
    pub async fn stop_node(&self, port: u16) {
        let p = port.to_string();
        self.cli(&["-p", &p, "shutdown", "nosave"]).await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while redis::Client::open(self.url(port))
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .is_ok()
        {
            assert!(Instant::now() < deadline, "the node did not stop");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `CONFIG RESETSTAT` on every node.
    pub async fn reset_stats(&self) {
        for p in &self.ports {
            let mut c = self.admin(*p).await;
            redis::cmd("CONFIG")
                .arg("RESETSTAT")
                .query_async::<()>(&mut c)
                .await
                .unwrap();
        }
    }

    /// `INFO commandstats` of one node: command name (`cluster|shards` on Redis 7+,
    /// `cluster` on 6.2) to `(calls, rejected_calls)`.
    pub async fn commandstats(&self, port: u16) -> HashMap<String, (u64, u64)> {
        let mut c = self.admin(port).await;
        let info: String = redis::cmd("INFO")
            .arg("commandstats")
            .query_async(&mut c)
            .await
            .unwrap();
        parse_commandstats(&info)
    }
}

/// `cmdstat_<name>:calls=N,...,rejected_calls=M,...` lines.
pub fn parse_commandstats(info: &str) -> HashMap<String, (u64, u64)> {
    info.lines()
        .filter_map(|l| {
            let (name, rest) = l.trim().strip_prefix("cmdstat_")?.split_once(':')?;
            let field = |f: &str| {
                rest.split(',')
                    .find_map(|kv| kv.strip_prefix(f)?.strip_prefix('='))
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            Some((name.to_string(), (field("calls"), field("rejected_calls"))))
        })
        .collect()
}

/// Every string in a reply, depth first.
fn flatten(v: &redis::Value) -> Vec<String> {
    match v {
        redis::Value::BulkString(b) => vec![String::from_utf8_lossy(b).into_owned()],
        redis::Value::SimpleString(s) => vec![s.clone()],
        redis::Value::Array(a) | redis::Value::Set(a) => a.iter().flat_map(flatten).collect(),
        redis::Value::Map(m) => m
            .iter()
            .flat_map(|(k, v)| flatten(k).into_iter().chain(flatten(v)))
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// A cluster that the host can't reach (for SSH tests)
// ---------------------------------------------------------------------------

/// The ports of a [`HiddenCluster`]'s nodes: fixed, since nothing is published. The
/// first three are the primaries.
pub const HIDDEN_PORTS: [u16; 6] = [7000, 7001, 7002, 7003, 7004, 7005];

/// Where a [`HiddenCluster`] runs, and what its nodes announce.
pub enum ClusterPlace<'a> {
    /// On a Docker network under a container name, announcing the container's address
    /// on that network.
    Network { network: &'a str, name: &'a str },
    /// In another container's network namespace (an SSH bastion's), announcing
    /// 127.0.0.1.
    Namespace(&'a str),
}

/// A cluster in one container with no published port: 3 primaries on
/// [`HIDDEN_PORTS`], plus 3 replicas when asked.
pub struct HiddenCluster {
    /// The container name on the network, or 127.0.0.1 in a namespace.
    pub host: String,
    /// The address every node announces.
    pub announced: String,
    pub ports: Vec<u16>,
    pub valkey: bool,
    container: ContainerAsync<GenericImage>,
}

/// Start a [`HiddenCluster`] in `place`, every node with `extra` server arguments (one
/// word each). The script waits until the cluster is settled (every node
/// `cluster_state:ok`; with replicas, every replica online and, where `CLUSTER SHARDS`
/// exists, every node `online`) before it says it is ready.
pub async fn hidden_cluster(
    place: ClusterPlace<'_>,
    valkey: bool,
    replicas: bool,
    extra: &[&str],
) -> HiddenCluster {
    let n = if replicas { 6 } else { 3 };
    let ports: Vec<u16> = HIDDEN_PORTS[..n].to_vec();
    let (image, bin, cli) = if valkey {
        (valkey_image(), "valkey-server", "valkey-cli")
    } else {
        (redis_image(), "redis-server", "redis-cli")
    };
    let list = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let primaries = list.split(' ').take(3).collect::<Vec<_>>().join(" ");
    let extra = extra
        .iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let announce = match place {
        ClusterPlace::Network { .. } => "$(hostname -i | awk '{print $1}')",
        ClusterPlace::Namespace(_) => "127.0.0.1",
    };
    let script = format!(
        r#"IP={announce}
for p in {list}; do
  mkdir -p /tmp/n$p
  {bin} --port $p --cluster-enabled yes --cluster-config-file /tmp/n$p/nodes.conf \
    --dir /tmp/n$p --cluster-announce-ip $IP --cluster-announce-port $p \
    --cluster-announce-bus-port $((p+10000)) --cluster-node-timeout 3000 \
    --protected-mode no --save '' --appendonly no --repl-ping-replica-period 1 \
    {extra} --daemonize yes --logfile /tmp/n$p/log
done
for p in {list}; do until {cli} -p $p ping >/dev/null 2>&1; do sleep 0.1; done; done
ADDRS=""; for p in {list}; do ADDRS="$ADDRS $IP:$p"; done
{cli} --cluster create $ADDRS --cluster-replicas {r} --cluster-yes >/tmp/create.log 2>&1
for p in {list}; do
  until {cli} -p $p cluster info | grep -q cluster_state:ok; do sleep 0.2; done
done
if [ {r} = 1 ]; then
  until [ "$({cli} -p 7000 cluster nodes | grep -c slave)" = 3 ] \
    && [ "$({cli} -p 7000 cluster nodes | grep -c ' connected')" = 6 ] \
    && ! {cli} -p 7000 cluster nodes | grep -qE 'fail|handshake|noaddr'; do sleep 0.2; done
  for p in {primaries}; do
    until {cli} -p $p info replication | grep -q state=online; do sleep 0.2; done
  done
  if {cli} -p 7000 cluster shards 2>/dev/null | grep -qx online; then
    until [ "$({cli} -p 7000 cluster shards | grep -cx online)" -ge 6 ]; do sleep 0.2; done
  fi
fi
echo "CLUSTER-READY $IP"
exec tail -f /dev/null
"#,
        r = if replicas { 1 } else { 0 },
    );
    let request = GenericImage::new(image.0, image.1)
        .with_wait_for(WaitFor::message_on_stdout("CLUSTER-READY"))
        .with_cmd(["sh".to_string(), "-c".to_string(), script])
        .with_startup_timeout(Duration::from_secs(120));
    let (request, host) = match place {
        ClusterPlace::Network { network, name } => (
            request.with_network(network).with_container_name(name),
            name.to_string(),
        ),
        ClusterPlace::Namespace(id) => (
            request.with_network(format!("container:{id}")),
            "127.0.0.1".to_string(),
        ),
    };
    let container = request
        .with_host_config_modifier(|hc| {
            hc.publish_all_ports = Some(false);
            hc.port_bindings = None;
        })
        .start()
        .await
        .expect("start the cluster container (is Docker running?)");
    let announced = match place {
        ClusterPlace::Network { .. } => container
            .get_bridge_ip_address()
            .await
            .expect("the cluster's network address")
            .to_string(),
        ClusterPlace::Namespace(_) => "127.0.0.1".to_string(),
    };
    HiddenCluster {
        host,
        announced,
        ports,
        valkey,
        container,
    }
}

impl HiddenCluster {
    /// Run `sh -c <script>` in the container and return its standard output; `$CLI` is
    /// the cluster's CLI.
    pub async fn sh(&self, script: &str) -> String {
        let cli = if self.valkey {
            "valkey-cli"
        } else {
            "redis-cli"
        };
        let script = format!("CLI='{cli} --no-auth-warning'; {script}");
        let mut exec = self
            .container
            .exec(
                ExecCommand::new(["sh", "-c", &script])
                    .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
            )
            .await
            .expect("run a command in the cluster container");
        String::from_utf8(exec.stdout_to_vec().await.expect("output")).expect("UTF-8")
    }

    pub async fn dbsize(&self, port: u16) -> u64 {
        self.sh(&format!("$CLI -p {port} DBSIZE"))
            .await
            .trim()
            .parse()
            .expect("DBSIZE")
    }

    /// Write `SET <prefix><i> x` for `i` in `0..n` with `redis-cli -c`, which follows
    /// the cluster's redirects. See [`Self::wait_for_keys`].
    pub async fn seed(&self, prefix: &str, n: u64) {
        self.sh(&format!(
            "i=0; while [ $i -lt {n} ]; do echo \"SET {prefix}$i x\"; i=$((i+1)); done \
             | $CLI -c -p 7000 > /dev/null"
        ))
        .await;
    }

    /// The keys on every primary, and on every node.
    pub async fn key_counts(&self) -> (u64, u64) {
        let (mut primaries, mut all) = (0, 0);
        for (i, p) in self.ports.iter().enumerate() {
            let n = self.dbsize(*p).await;
            all += n;
            if i < 3 {
                primaries += n;
            }
        }
        (primaries, all)
    }

    /// Wait until the primaries hold `keys` and, with replicas, the replicas too.
    pub async fn wait_for_keys(&self, keys: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let want_all = keys * (self.ports.len() as u64 / 3);
        loop {
            let (primaries, all) = self.key_counts().await;
            if primaries == keys && all == want_all {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the cluster holds {primaries}/{all} keys, not {keys}/{want_all}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Stop the node on `port` (`SHUTDOWN NOSAVE`).
    pub async fn stop_node(&self, port: u16) {
        self.sh(&format!(
            "$CLI -p {port} shutdown nosave >/dev/null 2>&1; \
             while $CLI -p {port} ping >/dev/null 2>&1; do sleep 0.1; done"
        ))
        .await;
    }

    /// `CONFIG RESETSTAT` on every node.
    pub async fn reset_stats(&self) {
        for p in &self.ports {
            self.sh(&format!("$CLI -p {p} CONFIG RESETSTAT >/dev/null"))
                .await;
        }
    }

    /// `INFO commandstats` of one node, as [`parse_commandstats`] reads it.
    pub async fn commandstats(&self, port: u16) -> HashMap<String, (u64, u64)> {
        parse_commandstats(&self.sh(&format!("$CLI -p {port} INFO commandstats")).await)
    }
}
