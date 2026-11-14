//! Sentinel: asking a profile's Sentinels where its primary is, and checking the answer
//! before anything else is sent there (spec "Sentinel (P4)").
//!
//! Owner: connections.
//!
//! The Sentinels are asked in the configured order, each within 2 s, over the profile's
//! [`Route`] (behind SSH, each through its own forward, started first). They get their
//! own credentials only, from the keychain entry `sentinel:profile:<name>`, and never the
//! data node's; the data node never gets the Sentinels'. The address the first Sentinel
//! gives is refused when it is not one redis-lens connects to (as announced replica and
//! cluster addresses are), then reached once and pinned. The data node's credentials go
//! there only under the loopback rule, judged against the Sentinel that answered. The
//! primary must say `role:master`, and a cluster node is refused.
//!
//! Every error is a fixed text written here: never a server's error text (it can name
//! the user) and never a credential.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use redis::{RedisConnectionInfo, Value};
use secrecy::ExposeSecret;

use crate::model::{Endpoint, Profile, Sentinel, SentinelNode};
use crate::secrets::SecretStore;

use super::cluster::{credentials_allowed, int, is_cluster, items, pairs, text};
use super::{
    ConnError, Connection, Pinned, Replication, ResolvedAuth, Route, ScanTarget, base_config,
    info_replication, is_noperm, master_replid, noperm_text, open, parse_replication, replica_host,
    verify_candidate,
};

/// How long one Sentinel may take, connecting and answering together.
pub const SENTINEL_TIMEOUT: Duration = Duration::from_secs(2);

/// What the Sentinels answered, kept on the [`Connection`] for `conn test` and the TUI.
/// Holds no credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentinelAnswer {
    /// The Sentinel that named the primary.
    pub sentinel: SentinelNode,
    /// The name the Sentinels monitor the primary under.
    pub master: String,
    /// The primary's address as that Sentinel gave it, with the profile's TLS setting.
    pub primary: Endpoint,
}

/// Why no primary could be used. Fixed texts: never a server's error text, never a
/// credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SentinelError {
    #[error("this profile lists no Sentinel; add it again with --sentinel")]
    NoSentinels,
    #[error("no Sentinel knew the primary {0}")]
    UnknownMaster(String),
    #[error("every Sentinel refused redis-lens (check the Sentinel credentials)")]
    Refused,
    #[error("no Sentinel could be reached")]
    Unreachable,
    #[error("the address Sentinel gave can't be used: {0}")]
    BadAddress(&'static str),
    #[error(
        "the primary Sentinel gave ({0}) is on this machine and the Sentinel that answered \
         is not, so the profile's credentials are not sent to it"
    )]
    WontSendCredentials(String),
    #[error("the primary Sentinel gave ({0}) refused the profile's credentials")]
    PrimaryRefused(String),
    #[error("the primary refused INFO replication, so redis-lens can't check it is a primary")]
    NoReplicationInfo,
    #[error("the address Sentinel gave is not a primary (a failover may be running); try again")]
    NotPrimary,
    #[error("Sentinel and cluster can't be combined")]
    Cluster,
}

/// The fixed reasons of [`SentinelError::BadAddress`].
const BAD_HOST: &str = "the host is ?, empty, unspecified (0.0.0.0, ::) or a shorthand \
     numeric form";
const BAD_PORT: &str = "the port is 0 or not a port number";

/// The checked primary of a Sentinel profile, as [`open_primary`] gives it.
pub(super) struct Primary {
    /// The connection to the primary, with the profile's credentials and database, and
    /// [`Connection::sentinel`] set.
    pub conn: Connection,
    /// The addresses the primary was reached at, found once (behind SSH, its forward).
    pub addrs: Vec<SocketAddr>,
    answer: Arc<SentinelAnswer>,
    /// The primary's `INFO replication`, read to check its role.
    replication: String,
    /// The Sentinel that answered, kept for `SENTINEL REPLICAS`.
    sentinel: Connection,
}

/// What one Sentinel said.
enum Asked {
    /// It gave the primary's address, as text.
    Address {
        host: String,
        port: String,
        conn: Connection,
        /// The Sentinel is on the same machine as whatever reaches it.
        local: bool,
    },
    /// It doesn't know the master name (a nil reply).
    Unknown,
    /// It refused redis-lens: credentials or the ACL.
    Refused,
    /// It could not be reached in time, or answered something else.
    Unreachable,
}

/// The connection settings for the Sentinels: their own username and password from the
/// keychain entry `sentinel:profile:<name>` when there is one, else none. Never the data
/// node's. Sends no `CLIENT SETINFO` and selects no database (a Sentinel has neither).
fn sentinel_settings(
    profile: &Profile,
    secrets: &dyn SecretStore,
) -> Result<RedisConnectionInfo, ConnError> {
    let mut settings = RedisConnectionInfo::default().set_skip_set_lib_name();
    if let Some(c) = secrets.get(&profile.sentinel_secret_ref())? {
        if let Some(user) = &c.username {
            settings = settings.set_username(user);
        }
        let pw = c.password.expose_secret();
        if !pw.is_empty() {
            settings = settings.set_password(pw);
        }
    }
    Ok(settings)
}

/// A refusal of redis-lens itself (credentials or the ACL), by error kind and code.
fn is_refusal(e: &redis::RedisError) -> bool {
    e.kind() == redis::ErrorKind::AuthenticationFailed
        || is_noperm(e)
        || matches!(e.code(), Some("WRONGPASS" | "NOAUTH" | "NOPERM"))
}

fn conn_refusal(e: &ConnError) -> bool {
    matches!(e, ConnError::Redis(r) if is_refusal(r))
}

/// The reply of `SENTINEL GET-MASTER-ADDR-BY-NAME`: `Some(None)` for nil (the name is
/// unknown), `Some(Some((host, port)))` for an address, `None` for anything else.
pub(crate) fn parse_master_addr(v: &Value) -> Option<Option<(String, String)>> {
    match v {
        Value::Nil => Some(None),
        _ => match items(v)? {
            [] => Some(None),
            [h, p] => Some(Some((
                text(h)?,
                int(p).map_or_else(|| text(p), |n| Some(n.to_string()))?,
            ))),
            _ => None,
        },
    }
}

/// The address a Sentinel gave, if redis-lens connects to it: the rules for announced
/// addresses (spec "Cluster", step 2), with the profile's TLS setting.
pub(crate) fn usable_address(host: &str, port: &str, tls: bool) -> Result<Endpoint, &'static str> {
    let host = replica_host(host).ok_or(BAD_HOST)?;
    let port = match port.trim().parse::<u16>() {
        Ok(p) if p != 0 => p,
        _ => return Err(BAD_PORT),
    };
    Ok(Endpoint { host, port, tls })
}

/// Ask one Sentinel for `master`. Behind SSH its forward is started first, outside the
/// 2 s, which then covers connecting and the answer.
async fn ask(
    route: &Arc<Route>,
    ep: &Endpoint,
    settings: &RedisConnectionInfo,
    master: &str,
) -> Asked {
    if route.is_ssh() && route.reach(ep).await.is_err() {
        return Asked::Unreachable;
    }
    let attempt = async {
        let Ok(reached) = route.reach(ep).await else {
            return Asked::Unreachable;
        };
        let config = base_config()
            .set_connection_timeout(Some(SENTINEL_TIMEOUT))
            .set_response_timeout(Some(SENTINEL_TIMEOUT))
            .set_dns_resolver(Pinned(reached.addrs));
        let c = match open(ep, settings.clone(), &config).await {
            Ok(c) => c,
            Err(e) if conn_refusal(&e) => return Asked::Refused,
            Err(_) => return Asked::Unreachable,
        };
        let mut conn = Connection::new(c, route);
        let reply: Result<Value, _> = redis::cmd("SENTINEL")
            .arg("GET-MASTER-ADDR-BY-NAME")
            .arg(master)
            .query_async(&mut conn)
            .await;
        match reply {
            Err(e) if is_refusal(&e) => Asked::Refused,
            Err(_) => Asked::Unreachable,
            Ok(v) => match parse_master_addr(&v) {
                Some(None) => Asked::Unknown,
                Some(Some((host, port))) => Asked::Address {
                    host,
                    port,
                    conn,
                    local: reached.local,
                },
                None => Asked::Unreachable,
            },
        }
    };
    tokio::time::timeout(SENTINEL_TIMEOUT, attempt)
        .await
        .unwrap_or(Asked::Unreachable)
}

/// The error when no Sentinel gave an address: a Sentinel that doesn't know the name says
/// most, then a refusal, then none reached.
fn none_answered(master: &str, unknown: bool, refused: bool) -> SentinelError {
    if unknown {
        SentinelError::UnknownMaster(master.to_string())
    } else if refused {
        SentinelError::Refused
    } else {
        SentinelError::Unreachable
    }
}

/// Ask the Sentinels in order; the first that gives an address wins, and one that is
/// unreachable, refuses or doesn't know the name is skipped. Returns the Sentinel that
/// answered, its connection, whether it is on this machine, and the address it gave.
async fn ask_all(
    profile: &Profile,
    sentinel: &Sentinel,
    secrets: &dyn SecretStore,
    route: &Arc<Route>,
) -> Result<(SentinelNode, Connection, bool, Endpoint), ConnError> {
    if sentinel.nodes.is_empty() {
        return Err(SentinelError::NoSentinels.into());
    }
    let settings = sentinel_settings(profile, secrets)?;
    let (mut unknown, mut refused) = (false, false);
    for node in &sentinel.nodes {
        match ask(
            route,
            &node.endpoint(sentinel.tls),
            &settings,
            &sentinel.master,
        )
        .await
        {
            Asked::Address {
                host,
                port,
                conn,
                local,
            } => {
                let ep = usable_address(&host, &port, profile.endpoint.tls)
                    .map_err(SentinelError::BadAddress)?;
                return Ok((node.clone(), conn, local, ep));
            }
            Asked::Unknown => unknown = true,
            Asked::Refused => refused = true,
            Asked::Unreachable => {}
        }
    }
    Err(none_answered(&sentinel.master, unknown, refused).into())
}

/// Ask the Sentinels for the primary, then connect to it with the profile's credentials,
/// TLS and database and check it: `role:master` in `INFO replication`, and not a cluster
/// node (`INFO server`, kept on the connection to answer the scan's own). The data
/// node's credentials go to it only when it is not on this machine, or the Sentinel that
/// answered is on this machine too (behind SSH, judged from the bastion).
pub(super) async fn open_primary(
    profile: &Profile,
    sentinel: &Sentinel,
    secrets: &dyn SecretStore,
    route: &Arc<Route>,
    auth: &ResolvedAuth,
) -> Result<Primary, ConnError> {
    let (node, sentinel_conn, sentinel_local, ep) =
        ask_all(profile, sentinel, secrets, route).await?;
    let shown = ep.to_string();
    let reached = route
        .reach(&ep)
        .await
        .map_err(super::Unusable::into_conn_error)?;
    if !credentials_allowed(auth.sends_credentials, sentinel_local, reached.local) {
        return Err(SentinelError::WontSendCredentials(shown).into());
    }
    let config = auth
        .config
        .clone()
        .set_dns_resolver(Pinned(reached.addrs.clone()));
    let c = match open(&ep, auth.settings.clone(), &config).await {
        Ok(c) => c,
        Err(e) if conn_refusal(&e) => return Err(SentinelError::PrimaryRefused(shown).into()),
        Err(e) => return Err(e),
    };
    let mut conn = Connection::new(c, route);
    let replication = match info_replication(&mut conn).await {
        Ok(text) => text,
        Err(e) if is_refusal(&e) && !is_noperm(&e) => {
            return Err(SentinelError::PrimaryRefused(shown).into());
        }
        Err(_) => return Err(SentinelError::NoReplicationInfo.into()),
    };
    if !matches!(parse_replication(&replication), Replication::Master { .. }) {
        return Err(SentinelError::NotPrimary.into());
    }
    if is_cluster(&mut conn).await {
        return Err(SentinelError::Cluster.into());
    }
    let answer = Arc::new(SentinelAnswer {
        sentinel: node,
        master: sentinel.master.clone(),
        primary: ep,
    });
    conn.set_sentinel(&answer);
    Ok(Primary {
        conn,
        addrs: reached.addrs,
        answer,
        replication,
        sentinel: sentinel_conn,
    })
}

/// One entry of `SENTINEL REPLICAS`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SentinelReplica {
    pub ip: String,
    pub port: String,
    pub flags: String,
    pub link: String,
}

impl SentinelReplica {
    /// Not flagged `s_down`, `o_down` or `disconnected`, and its link to the primary is
    /// `ok`.
    fn is_up(&self) -> bool {
        self.link == "ok"
            && !self
                .flags
                .split(',')
                .any(|f| matches!(f.trim(), "s_down" | "o_down" | "disconnected"))
    }
}

/// Parse a `SENTINEL REPLICAS` reply: an array of field/value lists (RESP2) or maps
/// (RESP3). `None` when it is not shaped like one; entries that can't be read are
/// skipped.
pub(crate) fn parse_replicas(v: &Value) -> Option<Vec<SentinelReplica>> {
    let mut out = Vec::new();
    for entry in items(v)? {
        let Some(fields) = pairs(entry) else { continue };
        let mut r = SentinelReplica::default();
        for (k, v) in fields {
            let value = text(v).or_else(|| int(v).map(|n| n.to_string()));
            let Some(value) = value else { continue };
            match k.as_str() {
                "ip" => r.ip = value,
                "port" => r.port = value,
                "flags" => r.flags = value,
                "master-link-status" => r.link = value,
                _ => {}
            }
        }
        out.push(r);
    }
    Some(out)
}

/// The first replica, in the Sentinel's order, that is up and has a usable address.
pub(crate) fn first_usable(replicas: &[SentinelReplica], tls: bool) -> Option<Endpoint> {
    replicas
        .iter()
        .filter(|r| r.is_up())
        .find_map(|r| usable_address(&r.ip, &r.port, tls).ok())
}

/// The `prefer_replica` step of a Sentinel profile: `SENTINEL REPLICAS <master>` on the
/// Sentinel that answered gives the candidates, and the first usable one is verified
/// exactly like a standalone replica (see [`super::verified_replica`]). Otherwise the
/// scan runs on the primary with `PrimaryFallback` and the reason.
pub(super) async fn replica_step(
    profile: &Profile,
    route: &Arc<Route>,
    auth: &ResolvedAuth,
    primary: Primary,
) -> (Connection, ScanTarget) {
    let Primary {
        conn,
        answer,
        replication,
        mut sentinel,
        ..
    } = primary;
    let fallback = |c, why: String| (c, ScanTarget::PrimaryFallback(why));
    let listed = tokio::time::timeout(SENTINEL_TIMEOUT, async {
        let r: Result<Value, _> = redis::cmd("SENTINEL")
            .arg("REPLICAS")
            .arg(&answer.master)
            .query_async(&mut sentinel)
            .await;
        r
    })
    .await;
    drop(sentinel);
    let replicas = match listed {
        Ok(Ok(v)) => parse_replicas(&v),
        Ok(Err(e)) if is_noperm(&e) => return fallback(conn, noperm_text("SENTINEL REPLICAS")),
        _ => None,
    };
    let Some(replicas) = replicas else {
        return fallback(conn, "the Sentinel did not list the replicas".to_string());
    };
    let Some(ep) = first_usable(&replicas, profile.endpoint.tls) else {
        return fallback(conn, "the Sentinel lists no replica that is up".to_string());
    };
    let Some(replid) = master_replid(&replication) else {
        return fallback(
            conn,
            "the primary does not report master_replid, so no replica can be verified".to_string(),
        );
    };
    match verify_candidate(route, auth, profile.db, &answer.primary, &replid, &ep).await {
        Ok(mut c) => {
            c.set_sentinel(&answer);
            (c, ScanTarget::Replica(ep))
        }
        Err(why) => fallback(conn, why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Auth, Credentials};
    use crate::secrets::MemoryStore;
    use secrecy::SecretString;

    use super::super::cluster::{ScanTargets, connect_targets};
    use super::super::tests::{FakeServer, STANDALONE, ep};

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn resp_bulk(s: &str) -> String {
        format!("${}\r\n{s}\r\n", s.len())
    }

    fn resp_addr(host: &str, port: u16) -> String {
        format!("*2\r\n{}{}", resp_bulk(host), resp_bulk(&port.to_string()))
    }

    const NIL: &str = "*-1\r\n";
    const MASTER: &str = "mymaster";
    const SENTINEL_PW: &str = "sentinel-pw-k2v";
    const SENTINEL_USER: &str = "sentinel-user-k2v";
    const DATA_PW: &str = "data-pw-m8x";
    const DATA_USER: &str = "data-user-m8x";

    /// A port nothing listens on.
    async fn closed_port() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    }

    /// A Sentinel that answers `GET-MASTER-ADDR-BY-NAME` with `reply`, and `SENTINEL
    /// REPLICAS` with `replicas` when given.
    async fn fake_sentinel(reply: String, replicas: Option<String>) -> FakeServer {
        let mut info = vec![("reply:sentinel get-master-addr-by-name", reply)];
        if let Some(r) = replicas {
            info.push(("reply:sentinel replicas", r));
        }
        FakeServer::start(&info).await
    }

    async fn fake_primary(role: &str) -> FakeServer {
        FakeServer::start(&[
            ("server", STANDALONE.to_string()),
            (
                "replication",
                format!("role:{role}\r\nconnected_slaves:0\r\nmaster_replid:abc123\r\n"),
            ),
        ])
        .await
    }

    fn node(port: u16) -> SentinelNode {
        SentinelNode {
            host: "127.0.0.1".into(),
            port,
        }
    }

    /// A Sentinel profile asking these Sentinels, whose `endpoint` is `endpoint_port`
    /// (which must never be dialled).
    fn profile(sentinels: &[u16], endpoint_port: u16) -> Profile {
        let mut p = super::super::ssh::tests::profile(None);
        p.name = "watched".into();
        p.endpoint = ep("127.0.0.1", endpoint_port, false);
        p.sentinel = Some(Sentinel {
            master: MASTER.into(),
            nodes: sentinels.iter().map(|&s| node(s)).collect(),
            tls: false,
        });
        p
    }

    async fn connect(p: &Profile, store: &MemoryStore) -> Result<Connection, ConnError> {
        super::super::connect(p, store).await
    }

    fn err_text(r: Result<Connection, ConnError>) -> String {
        r.map(|_| ()).unwrap_err().to_string()
    }

    #[test]
    fn master_addr_replies() {
        assert_eq!(parse_master_addr(&Value::Nil), Some(None));
        assert_eq!(parse_master_addr(&Value::Array(vec![])), Some(None));
        assert_eq!(
            parse_master_addr(&Value::Array(vec![b("10.0.0.5"), b("6379")])),
            Some(Some(("10.0.0.5".into(), "6379".into())))
        );
        // RESP3 servers may send the port as a number.
        assert_eq!(
            parse_master_addr(&Value::Array(vec![b("::1"), Value::Int(6380)])),
            Some(Some(("::1".into(), "6380".into())))
        );
        assert_eq!(parse_master_addr(&Value::Okay), None);
        assert_eq!(parse_master_addr(&Value::Array(vec![b("h")])), None);
        assert_eq!(
            parse_master_addr(&Value::Array(vec![Value::Nil, b("1")])),
            None
        );
    }

    #[test]
    fn sentinel_addresses_follow_the_announced_address_rules() {
        let ok = |h: &str, p: &str| usable_address(h, p, false);
        assert_eq!(ok("10.0.0.5", "6379"), Ok(ep("10.0.0.5", 6379, false)));
        assert_eq!(ok("::1", "6379"), Ok(ep("::1", 6379, false)));
        assert_eq!(ok("[::1]", "6379"), Ok(ep("::1", 6379, false)));
        assert_eq!(
            usable_address("cache-1.internal", "6380", true),
            Ok(ep("cache-1.internal", 6380, true))
        );
        for host in [
            "?",
            "",
            "0.0.0.0",
            "::",
            "127.1",
            "0",
            "2130706433",
            "0x7f000001",
            "a b",
        ] {
            assert_eq!(ok(host, "6379"), Err(BAD_HOST), "{host}");
        }
        for port in ["0", "x", "", "70000", "-1"] {
            assert_eq!(ok("10.0.0.5", port), Err(BAD_PORT), "{port}");
        }
        assert_eq!(
            SentinelError::BadAddress(BAD_PORT).to_string(),
            "the address Sentinel gave can't be used: the port is 0 or not a port number"
        );
    }

    fn entry(ip: &str, port: &str, flags: &str, link: &str) -> Value {
        Value::Array(vec![
            b("name"),
            b(&format!("{ip}:{port}")),
            b("ip"),
            b(ip),
            b("port"),
            b(port),
            b("runid"),
            b("x"),
            b("flags"),
            b(flags),
            b("master-link-status"),
            b(link),
        ])
    }

    #[test]
    fn replicas_that_are_down_or_unlinked_are_skipped() {
        let reply = Value::Array(vec![
            entry("10.0.0.1", "6379", "slave,s_down", "ok"),
            entry("10.0.0.2", "6379", "slave,o_down", "ok"),
            entry("10.0.0.3", "6379", "slave,disconnected", "ok"),
            entry("10.0.0.4", "6379", "slave", "err"),
            entry("0.0.0.0", "6379", "slave", "ok"),
            entry("10.0.0.6", "0", "slave", "ok"),
            entry("10.0.0.7", "6380", "slave", "ok"),
            entry("10.0.0.8", "6379", "slave", "ok"),
            Value::Int(3),
        ]);
        let replicas = parse_replicas(&reply).unwrap();
        assert_eq!(replicas.len(), 8, "an unreadable entry is skipped");
        assert_eq!(
            first_usable(&replicas, false),
            Some(ep("10.0.0.7", 6380, false))
        );
        assert_eq!(first_usable(&replicas[..6], false), None);
        // RESP3: a map per entry, the port a number.
        let resp3 = Value::Array(vec![Value::Map(vec![
            (b("ip"), b("10.0.0.9")),
            (b("port"), Value::Int(6390)),
            (b("flags"), b("slave")),
            (b("master-link-status"), b("ok")),
        ])]);
        assert_eq!(
            first_usable(&parse_replicas(&resp3).unwrap(), true),
            Some(ep("10.0.0.9", 6390, true))
        );
        assert_eq!(parse_replicas(&Value::Okay), None);
        assert_eq!(parse_replicas(&Value::Array(vec![])), Some(vec![]));
    }

    #[test]
    fn errors_are_fixed_texts() {
        let all = [
            SentinelError::NoSentinels,
            SentinelError::UnknownMaster(MASTER.into()),
            SentinelError::Refused,
            SentinelError::Unreachable,
            SentinelError::BadAddress(BAD_HOST),
            SentinelError::WontSendCredentials("127.0.0.1:6379".into()),
            SentinelError::PrimaryRefused("10.0.0.5:6379".into()),
            SentinelError::NoReplicationInfo,
            SentinelError::NotPrimary,
            SentinelError::Cluster,
        ];
        for e in &all {
            let t = e.to_string();
            assert!(!t.is_empty());
            for word in ["password", "WRONGPASS", "NOPERM", "user "] {
                assert!(!t.contains(word), "{word} in {t}");
            }
        }
        assert_eq!(
            SentinelError::UnknownMaster(MASTER.into()).to_string(),
            "no Sentinel knew the primary mymaster"
        );
        assert_eq!(
            none_answered(MASTER, true, true),
            SentinelError::UnknownMaster(MASTER.into())
        );
        assert_eq!(none_answered(MASTER, false, true), SentinelError::Refused);
        assert_eq!(
            none_answered(MASTER, false, false),
            SentinelError::Unreachable
        );
    }

    /// The Sentinels are asked in order: one that can't be reached and one that doesn't
    /// know the name are skipped, and the next one's answer is used. The profile's
    /// `endpoint` is never dialled.
    #[tokio::test]
    async fn sentinels_are_asked_in_order_and_skipped_when_they_cant_answer() {
        let primary = fake_primary("master").await;
        let unknown = fake_sentinel(NIL.into(), None).await;
        let good = fake_sentinel(resp_addr("127.0.0.1", primary.addr.port()), None).await;
        let never = fake_sentinel(resp_addr("127.0.0.1", 1), None).await;
        let old_primary = fake_primary("master").await;
        let down = closed_port().await;
        let p = profile(
            &[
                down,
                unknown.addr.port(),
                good.addr.port(),
                never.addr.port(),
            ],
            old_primary.addr.port(),
        );
        let store = MemoryStore::default();
        let ask = format!("SENTINEL GET-MASTER-ADDR-BY-NAME {MASTER}");
        let c = connect(&p, &store).await.unwrap();
        let answer = c.sentinel().unwrap();
        assert_eq!(answer.sentinel, node(good.addr.port()));
        assert_eq!(answer.master, MASTER);
        assert_eq!(answer.primary, ep("127.0.0.1", primary.addr.port(), false));
        assert_eq!(unknown.count(&ask), 1);
        assert_eq!(never.connections(), 0, "the first answer wins");
        assert_eq!(
            old_primary.connections(),
            0,
            "the endpoint is never dialled"
        );
        assert_eq!(primary.count("INFO replication"), 1);
        // Asked again at every connect, and through connect_targets too.
        let targets = connect_targets(&p, &store).await.unwrap();
        let ScanTargets::Single { conn, target } = targets else {
            panic!("a Sentinel profile was taken for a cluster");
        };
        assert_eq!(target, ScanTarget::Primary);
        assert!(conn.sentinel().is_some());
        assert_eq!(good.count(&ask), 2);
        // A database other than 0 doesn't send connect_targets to the endpoint either.
        let mut db1 = p.clone();
        db1.db = 1;
        assert!(connect_targets(&db1, &store).await.is_ok());
        assert_eq!(old_primary.connections(), 0);
    }

    #[tokio::test]
    async fn no_answer_is_a_fixed_error() {
        let old_primary = fake_primary("master").await;
        let store = MemoryStore::default();
        let unknown = fake_sentinel(NIL.into(), None).await;
        let refusing = fake_sentinel(
            "-NOAUTH Authentication required for sentinel-user-k2v\r\n".into(),
            None,
        )
        .await;
        let down = closed_port().await;
        let end = old_primary.addr.port();
        assert_eq!(
            err_text(connect(&profile(&[down, unknown.addr.port()], end), &store).await),
            "no Sentinel knew the primary mymaster"
        );
        let refused = err_text(connect(&profile(&[refusing.addr.port(), down], end), &store).await);
        assert_eq!(
            refused,
            "every Sentinel refused redis-lens (check the Sentinel credentials)"
        );
        assert_eq!(
            err_text(connect(&profile(&[down], end), &store).await),
            "no Sentinel could be reached"
        );
        assert_eq!(
            err_text(connect(&profile(&[], end), &store).await),
            SentinelError::NoSentinels.to_string()
        );
        let unusable = fake_sentinel(resp_addr("0.0.0.0", 6379), None).await;
        assert_eq!(
            err_text(connect(&profile(&[unusable.addr.port()], end), &store).await),
            SentinelError::BadAddress(BAD_HOST).to_string()
        );
        assert_eq!(old_primary.connections(), 0);
    }

    /// The Sentinel's answer is checked before it is used: a replica, a cluster node, or
    /// a primary that refuses `INFO replication` fails the connect (fail closed).
    #[tokio::test]
    async fn the_answer_must_be_a_standalone_primary() {
        let store = MemoryStore::default();
        let cluster = FakeServer::start(&[
            (
                "server",
                "# Server\r\nredis_version:7.4.0\r\nredis_mode:cluster\r\n".into(),
            ),
            ("replication", "role:master\r\nmaster_replid:abc\r\n".into()),
        ])
        .await;
        let refusing = FakeServer::start(&[(
            "replication",
            "-NOPERM User data-user-m8x has no permissions to run the 'info' command".into(),
        )])
        .await;
        let cases = [
            (fake_primary("slave").await, SentinelError::NotPrimary),
            (cluster, SentinelError::Cluster),
            (refusing, SentinelError::NoReplicationInfo),
        ];
        for (server, want) in cases {
            let s = fake_sentinel(resp_addr("127.0.0.1", server.addr.port()), None).await;
            let p = profile(&[s.addr.port()], closed_port().await);
            assert_eq!(err_text(connect(&p, &store).await), want.to_string());
        }
        assert_eq!(
            SentinelError::Cluster.to_string(),
            "Sentinel and cluster can't be combined"
        );
    }

    /// The Sentinel gets only its own credentials and the primary only the data node's.
    #[tokio::test]
    async fn sentinel_and_data_node_credentials_never_cross() {
        let primary = fake_primary("master").await;
        let s = fake_sentinel(resp_addr("127.0.0.1", primary.addr.port()), None).await;
        let mut p = profile(&[s.addr.port()], closed_port().await);
        p.auth = Auth::Keychain;
        let store = MemoryStore::default();
        let creds = |user: Option<&str>, pw: &str| Credentials {
            username: user.map(String::from),
            password: SecretString::from(pw.to_string()),
        };
        store
            .set(&p.secret_ref(), &creds(Some(DATA_USER), DATA_PW))
            .unwrap();
        store
            .set(
                &p.sentinel_secret_ref(),
                &creds(Some(SENTINEL_USER), SENTINEL_PW),
            )
            .unwrap();
        let c = connect(&p, &store).await.unwrap();
        let sent = |f: &FakeServer| f.commands.lock().unwrap().join("\n");
        let (to_sentinel, to_primary) = (sent(&s), sent(&primary));
        assert!(
            to_sentinel.contains(&format!("AUTH {SENTINEL_USER} {SENTINEL_PW}")),
            "{to_sentinel}"
        );
        assert!(
            to_primary.contains(&format!("AUTH {DATA_USER} {DATA_PW}")),
            "{to_primary}"
        );
        for secret in [DATA_USER, DATA_PW] {
            assert!(
                !to_sentinel.contains(secret),
                "{secret} went to the Sentinel"
            );
        }
        for secret in [SENTINEL_USER, SENTINEL_PW] {
            assert!(!to_primary.contains(secret), "{secret} went to the primary");
        }
        // The Sentinel gets no CLIENT SETINFO or SELECT.
        assert!(
            !to_sentinel.contains("CLIENT") && !to_sentinel.contains("SELECT"),
            "{to_sentinel}"
        );
        let dbg = format!("{c:?} {:?}", c.sentinel());
        for secret in [DATA_USER, DATA_PW, SENTINEL_USER, SENTINEL_PW] {
            assert!(!dbg.contains(secret), "{dbg}");
        }

        // A password-only Sentinel (requirepass), and no entry at all: no AUTH.
        let s2 = fake_sentinel(resp_addr("127.0.0.1", primary.addr.port()), None).await;
        let mut q = profile(&[s2.addr.port()], closed_port().await);
        q.name = "other".into();
        let store = MemoryStore::default();
        store
            .set(&q.sentinel_secret_ref(), &creds(None, SENTINEL_PW))
            .unwrap();
        connect(&q, &store).await.unwrap();
        assert_eq!(s2.count(&format!("AUTH {SENTINEL_PW}")), 1);
        let s3 = fake_sentinel(resp_addr("127.0.0.1", primary.addr.port()), None).await;
        let r = profile(&[s3.addr.port()], closed_port().await);
        connect(&r, &MemoryStore::default()).await.unwrap();
        assert!(!sent(&s3).contains("AUTH"), "{}", sent(&s3));
    }

    /// This machine's address on its default route, when it has one that is not
    /// loopback: a Sentinel reached there counts as not on this machine.
    fn non_loopback_ip() -> Option<std::net::IpAddr> {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("192.0.2.1:9").ok()?;
        let ip = s.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }

    /// The loopback rule, judged against the Sentinel that answered: a Sentinel that is
    /// not on this machine naming a primary on this machine's loopback gets no data-node
    /// credentials there, and the primary is never connected to.
    #[tokio::test]
    async fn a_remote_sentinel_naming_a_loopback_primary_gets_no_credentials_sent() {
        let Some(ip) = non_loopback_ip() else {
            eprintln!("skipped: this machine has no non-loopback address");
            return;
        };
        let primary = fake_primary("master").await;
        let s = FakeServer::start_on(
            ip,
            &[(
                "reply:sentinel get-master-addr-by-name",
                resp_addr("127.0.0.1", primary.addr.port()),
            )],
        )
        .await;
        let reachable = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(s.addr),
        )
        .await;
        if !matches!(reachable, Ok(Ok(_))) {
            eprintln!("skipped: this machine's own address {ip} can't be connected to here");
            return;
        }
        let mut p = profile(&[], closed_port().await);
        p.sentinel.as_mut().unwrap().nodes = vec![SentinelNode {
            host: ip.to_string(),
            port: s.addr.port(),
        }];
        p.auth = Auth::Keychain;
        let store = MemoryStore::default();
        store
            .set(
                &p.secret_ref(),
                &Credentials {
                    username: Some(DATA_USER.into()),
                    password: SecretString::from(DATA_PW),
                },
            )
            .unwrap();
        let text = err_text(connect(&p, &store).await);
        assert_eq!(
            text,
            SentinelError::WontSendCredentials(format!("127.0.0.1:{}", primary.addr.port()))
                .to_string()
        );
        assert_eq!(primary.connections(), 0);
        // Without credentials there is nothing to keep back.
        p.auth = Auth::None;
        connect(&p, &store).await.unwrap();
    }

    /// The same rule behind SSH, judged from the bastion by name: a Sentinel named
    /// `sentinel.example` is not on the bastion, so a primary it names as `127.0.0.1`
    /// (the bastion's own loopback) gets no data-node credentials. A fake ssh forwards
    /// every `-L` to the fake Sentinel.
    #[cfg(unix)]
    #[tokio::test]
    async fn behind_ssh_a_remote_sentinel_naming_a_loopback_primary_gets_no_credentials() {
        const PROXY: &str = r#"
use strict; use warnings; use IO::Socket::INET; use IO::Select;
my $to = shift @ARGV;
my ($spec) = map { $ARGV[$_ + 1] } grep { $ARGV[$_] eq '-L' } 0 .. $#ARGV;
my (undef, $port) = split /:/, $spec;
my $l = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => $port, Listen => 16, ReuseAddr => 1) or exit 255;
$SIG{CHLD} = 'IGNORE';
while (my $c = $l->accept) {
  my $pid = fork();
  if (defined $pid && $pid == 0) {
    my $up = IO::Socket::INET->new(PeerAddr => '127.0.0.1', PeerPort => $to) or exit 0;
    my $sel = IO::Select->new($c, $up);
    while (my @r = $sel->can_read) {
      for my $fh (@r) {
        my $n = sysread($fh, my $buf, 65536);
        exit 0 unless $n;
        syswrite($fh == $c ? $up : $c, $buf) or exit 0;
      }
    }
    exit 0;
  }
  close $c;
}
"#;
        use super::super::ssh;
        let dir = std::env::temp_dir().join(format!(
            "redis-lens-sentinel-ssh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("proxy.pl");
        std::fs::write(&script, PROXY).unwrap();

        let primary = fake_primary("master").await;
        let s = fake_sentinel(resp_addr("127.0.0.1", primary.addr.port()), None).await;
        let t = ssh::tests::tunnel("b.example");
        let mut p = profile(&[], closed_port().await);
        p.ssh = Some(t.clone());
        p.sentinel.as_mut().unwrap().nodes = vec![SentinelNode {
            host: "sentinel.example".into(),
            port: 26379,
        }];
        p.auth = Auth::Keychain;
        let store = MemoryStore::default();
        store
            .set(
                &p.secret_ref(),
                &Credentials {
                    username: Some(DATA_USER.into()),
                    password: SecretString::from(DATA_PW),
                },
            )
            .unwrap();
        let bastion = ssh::Bastion::new(&p, &t, &store).unwrap().with_launcher(
            vec![
                "perl".into(),
                script.clone().into(),
                s.addr.port().to_string().into(),
            ],
            ssh::READY_TIMEOUT,
        );
        let route = Arc::new(Route::Ssh(bastion));
        let auth = ResolvedAuth::new(&p, &store).await.unwrap();
        let sentinel = p.sentinel.clone().unwrap();
        let r = open_primary(&p, &sentinel, &store, &route, &auth).await;
        let e = r.map(|_| ()).unwrap_err().to_string();
        assert_eq!(
            e,
            SentinelError::WontSendCredentials(format!("127.0.0.1:{}", primary.addr.port()))
                .to_string()
        );
        let to_sentinel = s.commands.lock().unwrap().join("\n");
        assert!(
            to_sentinel.contains("GET-MASTER-ADDR-BY-NAME"),
            "{to_sentinel}"
        );
        // The only server behind the forwards is the Sentinel: no data credential
        // reached it, and nothing reached the primary.
        assert!(!to_sentinel.contains(DATA_PW), "{to_sentinel}");
        assert_eq!(primary.connections(), 0);
        drop(route);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn resp_entry(port: u16, flags: &str) -> String {
        let fields = [
            "ip".to_string(),
            "127.0.0.1".into(),
            "port".into(),
            port.to_string(),
            "flags".into(),
            flags.into(),
            "master-link-status".into(),
            "ok".into(),
        ];
        let body: String = fields.iter().map(|f| resp_bulk(f)).collect();
        format!("*{}\r\n{body}", fields.len())
    }

    /// `prefer_replica`: the Sentinel's first replica that is up is verified like a
    /// standalone one and scanned; the Sentinel answer stays on its connection.
    #[tokio::test]
    async fn prefer_replica_takes_the_sentinels_first_replica_that_is_up() {
        let replica = FakeServer::start(&[
            ("server", STANDALONE.to_string()),
            (
                "replication",
                "role:slave\r\nmaster_host:127.0.0.1\r\nmaster_replid:abc123\r\n".into(),
            ),
        ])
        .await;
        let down = closed_port().await;
        let primary = fake_primary("master").await;
        let at_primary = resp_addr("127.0.0.1", primary.addr.port());
        let replicas = format!(
            "*2\r\n{}{}",
            resp_entry(down, "slave,s_down"),
            resp_entry(replica.addr.port(), "slave")
        );
        let s = fake_sentinel(at_primary.clone(), Some(replicas)).await;
        let mut p = profile(&[s.addr.port()], closed_port().await);
        p.prefer_replica = true;
        let store = MemoryStore::default();
        let (c, target) = super::super::connect_for_scan(&p, &store).await.unwrap();
        assert_eq!(
            target,
            ScanTarget::Replica(ep("127.0.0.1", replica.addr.port(), false))
        );
        assert_eq!(
            c.sentinel().unwrap().primary,
            ep("127.0.0.1", primary.addr.port(), false)
        );
        assert_eq!(replica.count("INFO replication"), 1);

        // No usable replica: the primary, with the reason.
        let only_down = format!("*1\r\n{}", resp_entry(down, "slave,o_down"));
        let s = fake_sentinel(at_primary.clone(), Some(only_down)).await;
        let mut p = profile(&[s.addr.port()], closed_port().await);
        p.prefer_replica = true;
        let targets = connect_targets(&p, &store).await.unwrap();
        assert_eq!(
            targets.target(),
            ScanTarget::PrimaryFallback("the Sentinel lists no replica that is up".into())
        );
        // A Sentinel user without `sentinel|replicas`.
        let noperm = "-NOPERM User x has no permissions to run the 'sentinel|replicas' command\r\n";
        let s = fake_sentinel(at_primary, Some(noperm.into())).await;
        let mut p = profile(&[s.addr.port()], closed_port().await);
        p.prefer_replica = true;
        let (_, target) = super::super::connect_for_scan(&p, &store).await.unwrap();
        assert_eq!(
            target,
            ScanTarget::PrimaryFallback(
                "NOPERM SENTINEL REPLICAS is not allowed for this user".into()
            )
        );
    }
}
