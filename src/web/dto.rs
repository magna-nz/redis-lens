//! What the dashboard's API sends, as JSON. Built from the crate's own types by the
//! `of` functions here, which pick the fields: nothing here holds a credential, and
//! every text from a server or the config file is escaped for display
//! ([`value::escape`]), as the TUI draws it. Connection addresses are masked
//! ([`crate::conn::parse::mask`]).

use serde::Serialize;

use crate::conn::{self, provider, ssh};
use crate::live::cmdline::{Output, View};
use crate::live::monitor::Entry;
use crate::live::nodes::NodeInfo;
use crate::live::pubsub::TailMessage;
use crate::live::server::{ClientConn, InfoSample, Role, SlowEntry};
use crate::model::{
    Auth, Env, NodeRole, PermStatus, PermissionReport, Profile, ServerInfo, ServerMode,
};
use crate::scan::figures::{NodeFigures, ServerFigures};
use crate::tui::app::views::server::{Group, Point, build_groups, is_own, note_of};
use crate::value::escape;

/// `prod`, `stage`, `local` or `other`.
pub fn env_name(e: Env) -> &'static str {
    match e {
        Env::Prod => "prod",
        Env::Stage => "stage",
        Env::Local => "local",
        Env::Other => "other",
    }
}

pub fn auth_name(a: Auth) -> &'static str {
    match a {
        Auth::None => "none",
        Auth::Keychain => "keychain",
        Auth::EntraId => "entra-id",
        Auth::Command => "command",
    }
}

/// The address a profile connects to, as a URL without credentials (there are none in
/// a profile), masked all the same.
pub fn masked_url(p: &Profile) -> String {
    let scheme = if p.endpoint.tls { "rediss" } else { "redis" };
    escape(&conn::parse::mask(&format!(
        "{scheme}://{}/{}",
        p.endpoint, p.db
    )))
}

/// One saved connection, as the Connections list shows it. No credential: a profile has
/// none (they are in the keychain), and only flags say how it signs in.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Connection {
    pub name: String,
    pub env: &'static str,
    pub env_label: &'static str,
    pub url: String,
    pub endpoint: String,
    pub tls: bool,
    pub db: u32,
    /// `none`, `keychain`, `entra-id` or `command`.
    pub auth: &'static str,
    pub read_only: bool,
    pub writes: bool,
    pub tags: Vec<String>,
    pub provider: Option<&'static str>,
    pub provider_notice: Option<&'static str>,
    /// The bastion, `host[:port]`: the SSH user is in the keychain.
    pub ssh: Option<String>,
    /// `<master> via <host:port> (+N)`.
    pub sentinel: Option<String>,
    pub sentinel_tls: bool,
    pub token_command: bool,
    pub standalone: bool,
    pub prefer_replica: bool,
}

impl Connection {
    pub fn of(p: &Profile) -> Self {
        Self {
            name: escape(&p.name),
            env: env_name(p.env),
            env_label: p.env.label(),
            url: masked_url(p),
            endpoint: escape(&p.endpoint.to_string()),
            tls: p.endpoint.tls,
            db: p.db,
            auth: auth_name(p.auth),
            read_only: p.is_read_only(),
            writes: !p.is_read_only(),
            tags: p.tags.iter().map(|t| escape(t)).collect(),
            provider: p.provider.map(provider::label),
            provider_notice: provider::notice(p.provider),
            ssh: p.ssh.as_ref().map(|t| escape(&ssh::label(t))),
            sentinel: p
                .sentinel
                .as_ref()
                .map(|s| escape(&crate::cli::conn::sentinel_label(s))),
            sentinel_tls: p.sentinel.as_ref().is_some_and(|s| s.tls),
            token_command: p.token_command.is_some(),
            standalone: p.force_standalone,
            prefer_replica: p.prefer_replica,
        }
    }
}

/// A Redis or Valkey container Docker discovery found. The password it may have read
/// stays in the server.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Docker {
    pub container: String,
    pub image: String,
    pub url: String,
    pub endpoint: String,
    /// `password` (found, kept in memory), `none`, `required` (not found) or `unknown`.
    pub auth: &'static str,
}

impl Docker {
    pub fn of(d: &conn::docker::DiscoveredInstance) -> Self {
        use conn::docker::DiscoveredAuth as A;
        Self {
            container: escape(&d.container),
            image: escape(&d.image),
            url: masked_url(&d.profile),
            endpoint: escape(&d.profile.endpoint.to_string()),
            auth: match d.auth {
                A::Password => "password",
                A::None => "none",
                A::Required => "required",
                A::Unknown => "unknown",
            },
        }
    }
}

/// One line of the permission check.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Perm {
    pub label: &'static str,
    /// `allowed`, `denied` or `skipped`.
    pub status: &'static str,
    /// Why, for `denied` and `skipped`: redis-lens's own words.
    pub reason: Option<String>,
}

pub fn perms(r: &PermissionReport) -> Vec<Perm> {
    r.checks
        .iter()
        .map(|c| {
            let (status, reason) = match &c.status {
                PermStatus::Allowed => ("allowed", None),
                PermStatus::Denied(m) => (
                    "denied",
                    Some(escape(&crate::web::wording::perm_reason(c.label, true, m))),
                ),
                PermStatus::Skipped(m) => (
                    "skipped",
                    Some(escape(&crate::web::wording::perm_reason(c.label, false, m))),
                ),
            };
            Perm {
                label: c.label,
                status,
                reason,
            }
        })
        .collect()
}

/// What the server is: flavor, version, topology, and its memory and keys.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Server {
    /// `redis`, `valkey` or `unknown`.
    pub flavor: &'static str,
    pub version: String,
    /// `standalone`, `cluster` or `sentinel`.
    pub mode: &'static str,
    pub used_memory: u64,
    pub maxmemory: u64,
    pub policy: String,
    pub lfu: bool,
    pub keys: u64,
}

impl Server {
    pub fn of(s: &ServerInfo) -> Self {
        use crate::model::ServerFlavor as F;
        Self {
            flavor: match s.flavor {
                F::Redis => "redis",
                F::Valkey => "valkey",
                F::Unknown => "unknown",
            },
            version: escape(&s.version),
            mode: match s.mode {
                ServerMode::Standalone => "standalone",
                ServerMode::Cluster => "cluster",
                ServerMode::Sentinel => "sentinel",
            },
            used_memory: s.used_memory,
            maxmemory: s.maxmemory,
            policy: escape(s.policy_label()),
            lfu: s.lfu_enabled(),
            keys: s.total_keys,
        }
    }
}

/// The connect's figures of one server (or a cluster, summed): what Overview shows
/// before an analysis.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Figures {
    pub server: Server,
    pub keys: u64,
    pub with_ttl: Option<u64>,
    pub ttl_share: Option<f64>,
    pub dataset_bytes: Option<u64>,
    pub overhead_bytes: Option<u64>,
    pub big_keys: Option<u64>,
    /// Why parts are missing, in redis-lens's own words.
    pub gaps: Vec<&'static str>,
}

impl Figures {
    pub fn of(f: &ServerFigures) -> Self {
        let h = f.headline();
        Self {
            server: Server::of(&f.server),
            keys: h.keys,
            with_ttl: h.with_ttl,
            ttl_share: h.ttl_share,
            dataset_bytes: h.dataset_bytes,
            overhead_bytes: h.overhead_bytes,
            big_keys: h.big_keys,
            gaps: f.gaps(),
        }
    }
}

/// One node of a cluster (spec "Cluster live views (P4)").
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Node {
    pub index: usize,
    pub id: String,
    pub endpoint: String,
    /// `primary` or `replica`.
    pub role: &'static str,
    pub slots: Vec<(u16, u16)>,
    pub slot_count: u32,
    pub connected: bool,
}

impl Node {
    pub fn of(index: usize, n: &NodeInfo) -> Self {
        Self {
            index,
            id: escape(&n.id),
            endpoint: escape(&n.endpoint.to_string()),
            role: role_name(n.role),
            slots: n.slots.clone(),
            slot_count: n
                .slots
                .iter()
                .map(|(a, b)| u32::from(*b) - u32::from(*a) + 1)
                .sum(),
            connected: n.connected,
        }
    }
}

pub fn role_name(r: NodeRole) -> &'static str {
    match r {
        NodeRole::Primary => "primary",
        NodeRole::Replica => "replica",
    }
}

/// A cluster node's row of Overview's per-shard summary: its connect figures, and its
/// live ones once polled.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Shard {
    pub node: Node,
    pub figures: Option<Figures>,
    pub note: Option<String>,
    pub live: Option<Info>,
    /// The node did not answer its last live poll.
    pub failed: bool,
}

impl Shard {
    pub fn of(
        node: Node,
        f: Option<&NodeFigures>,
        live: Option<&InfoSample>,
        failed: bool,
    ) -> Self {
        Self {
            node,
            figures: f.and_then(|f| f.figures.as_ref()).map(Figures::of),
            note: f.and_then(|f| f.note.as_deref()).map(escape),
            live: live.map(Info::of),
            failed,
        }
    }
}

/// One `INFO` poll, as Overview reads it.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Info {
    pub uptime_secs: Option<u64>,
    pub role: Option<&'static str>,
    pub replicas: Option<u64>,
    pub link_up: Option<bool>,
    pub clients: Option<u64>,
    pub blocked: Option<u64>,
    pub used_memory: Option<u64>,
    pub maxmemory: Option<u64>,
    pub fragmentation: Option<f64>,
    pub policy: Option<String>,
    pub commands: Option<u64>,
    pub hits: Option<u64>,
    pub misses: Option<u64>,
    pub evicted: Option<u64>,
    pub expired: Option<u64>,
    pub rdb_ok: Option<bool>,
    pub aof_on: Option<bool>,
    pub aof_ok: Option<bool>,
    pub ops_now: Option<u64>,
    pub keys: Option<u64>,
}

impl Info {
    pub fn of(s: &InfoSample) -> Self {
        Self {
            uptime_secs: s.uptime_secs,
            role: s.role.map(|r| match r {
                Role::Primary => "primary",
                Role::Replica => "replica",
            }),
            replicas: s.replicas,
            link_up: s.link_up,
            clients: s.clients,
            blocked: s.blocked,
            used_memory: s.used_memory,
            maxmemory: s.maxmemory,
            fragmentation: s.fragmentation,
            policy: s.policy.as_deref().map(escape),
            commands: s.commands,
            hits: s.hits,
            misses: s.misses,
            evicted: s.evicted,
            expired: s.expired,
            rdb_ok: s.rdb_ok,
            aof_on: s.aof_on,
            aof_ok: s.aof_ok,
            ops_now: s.ops_now,
            keys: s.keys,
        }
    }
}

/// The rates of one poll over the one before (the TUI's Overview point).
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct Rates {
    pub ops: Option<f64>,
    pub used_memory: Option<u64>,
    pub hit_rate: Option<f64>,
    pub clients: Option<u64>,
    pub blocked: Option<u64>,
    pub net_in: Option<f64>,
    pub net_out: Option<f64>,
    pub evicted: Option<f64>,
    pub expired: Option<f64>,
    pub cpu_user: Option<f64>,
    pub cpu_sys: Option<f64>,
    pub fragmentation: Option<f64>,
}

impl Rates {
    pub fn of(p: &Point) -> Self {
        Self {
            ops: p.ops,
            used_memory: p.used_memory,
            hit_rate: p.hit_rate,
            clients: p.clients,
            blocked: p.blocked,
            net_in: p.net_in,
            net_out: p.net_out,
            evicted: p.evicted,
            expired: p.expired,
            cpu_user: p.cpu_user,
            cpu_sys: p.cpu_sys,
            fragmentation: p.fragmentation,
        }
    }
}

/// One connection of `CLIENT LIST`. It has no user: the parser drops `user=`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Client {
    pub id: u64,
    pub addr: String,
    pub name: String,
    pub lib: String,
    pub age: u64,
    pub idle: u64,
    pub cmd: String,
    pub mem: u64,
    pub db: Option<u64>,
    /// redis-lens's own connection.
    pub own: bool,
}

impl Client {
    pub fn of(c: &ClientConn) -> Self {
        Self {
            id: c.id,
            addr: escape(&c.addr),
            name: escape(&c.name),
            lib: escape(&crate::tui::app::views::server::lib_label(c)),
            age: c.age,
            idle: c.idle,
            cmd: escape(&c.cmd),
            mem: c.mem,
            db: c.db,
            own: is_own(c),
        }
    }
}

/// A group of connections, as the TUI's Clients panel groups them.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClientGroup {
    /// `lib` (library or client name) or `addr` (network).
    pub kind: &'static str,
    pub label: String,
    pub connections: usize,
    pub oldest_age: u64,
    pub longest_idle: u64,
    pub idle_conns: usize,
    pub flagged: bool,
    pub top_cmd: String,
    pub mem: u64,
    /// Indexes into the connection list.
    pub members: Vec<usize>,
}

impl ClientGroup {
    pub fn of(g: &Group) -> Self {
        Self {
            kind: g.key.kind(),
            label: escape(g.key.label()),
            connections: g.connections(),
            oldest_age: g.oldest_age,
            longest_idle: g.longest_idle,
            idle_conns: g.idle_conns,
            flagged: g.flagged(),
            top_cmd: escape(&g.top_cmd),
            mem: g.mem,
            members: g.members.clone(),
        }
    }
}

/// The Clients panel: the connections and their groups.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Clients {
    pub node: Option<usize>,
    pub connections: Vec<Client>,
    pub groups: Vec<ClientGroup>,
}

impl Clients {
    pub fn of(node: Option<usize>, conns: &[ClientConn]) -> Self {
        Self {
            node,
            connections: conns.iter().map(Client::of).collect(),
            groups: build_groups(conns).iter().map(ClientGroup::of).collect(),
        }
    }
}

/// One slow log entry: the arguments only as the TUI's masked line.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Slow {
    pub id: u64,
    pub unix: i64,
    pub micros: u64,
    pub command: String,
    pub keys: Vec<String>,
    pub more_keys: usize,
    pub client: String,
    pub client_name: String,
    /// The command and its arguments, credentials masked
    /// ([`crate::live::command::masked_line`]).
    pub line: String,
    pub args_cut: u64,
    /// A known problem with the command, in redis-lens's words.
    pub note: Option<&'static str>,
}

impl Slow {
    pub fn of(e: &SlowEntry) -> Self {
        Self {
            id: e.id,
            unix: e.unix,
            micros: e.micros,
            command: escape(&e.command),
            keys: e.keys.iter().map(|k| escape(k)).collect(),
            more_keys: e.more_keys,
            client: escape(&e.client),
            client_name: escape(&e.client_name),
            line: escape(&String::from_utf8_lossy(e.args.as_bytes())),
            args_cut: e.args_cut,
            note: note_of(e).map(|n| n.text()),
        }
    }
}

/// One message of the Pub/Sub tail: its preview, already safe to draw.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TailMsg {
    pub at_unix_ms: i64,
    pub channel: String,
    pub pattern: Option<String>,
    pub preview: String,
    pub bytes: usize,
}

impl TailMsg {
    pub fn of(m: &TailMessage) -> Self {
        Self {
            at_unix_ms: m.at_unix_ms,
            channel: escape(&m.channel),
            pattern: m.pattern.as_deref().map(escape),
            preview: m.preview().to_string(),
            bytes: m.payload.len(),
        }
    }
}

/// One `MONITOR` line, credentials masked.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MonitorLine {
    pub unix: Option<i64>,
    pub millis: u32,
    pub db: String,
    pub client: String,
    pub line: String,
    pub cut: u64,
}

impl MonitorLine {
    pub fn of(e: &Entry) -> Self {
        Self {
            unix: e.unix(),
            millis: e.millis(),
            db: escape(&e.line.db),
            client: escape(&e.line.client),
            line: escape(&e.command_line()),
            cut: e.cut,
        }
    }
}

/// A command line reply in its four views (spec "Command line panel"), each line safe to
/// draw.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Reply {
    pub text: Vec<String>,
    pub table: Vec<String>,
    pub json: Vec<String>,
    pub raw: Vec<String>,
    pub cut: bool,
    pub error: bool,
    pub took_ms: f64,
}

impl Reply {
    pub fn of(o: &Output, pairs: bool) -> Self {
        Self {
            text: o.lines_in(View::Text, pairs).to_vec(),
            table: o.lines_in(View::Table, pairs).to_vec(),
            json: o.lines_in(View::Json, pairs).to_vec(),
            raw: o.lines_in(View::Raw, pairs).to_vec(),
            cut: o.cut,
            error: o.error,
            took_ms: o.took.as_secs_f64() * 1000.0,
        }
    }

    /// A reply that is one line, the same in every view.
    pub fn line(s: String) -> Self {
        Self {
            text: vec![s.clone()],
            table: vec![s.clone()],
            json: vec![s.clone()],
            raw: vec![s],
            cut: false,
            error: false,
            took_ms: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Endpoint;

    fn profile_from_url(url: &str) -> Profile {
        let parsed = conn::parse::parse_connection_string(url).unwrap();
        // The credentials go to the keychain; the profile holds none (see `conn add`).
        assert!(parsed.credentials.is_some());
        Profile {
            name: "cache".into(),
            env: Env::Prod,
            endpoint: parsed.endpoint,
            db: parsed.db,
            auth: Auth::Keychain,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            force_standalone: false,
            tags: vec!["team-a".into()],
        }
    }

    #[test]
    fn a_connection_never_carries_its_credentials() {
        let p = profile_from_url("rediss://alice-user:S3cret-pw@cache.internal:6380/2");
        let json = serde_json::to_string(&Connection::of(&p)).unwrap();
        assert!(!json.contains("alice-user"), "{json}");
        assert!(!json.contains("S3cret-pw"), "{json}");
        assert!(
            json.contains("\"url\":\"rediss://cache.internal:6380/2\""),
            "{json}"
        );
        assert!(json.contains("\"auth\":\"keychain\""), "{json}");
        assert!(json.contains("\"read_only\":true"), "{json}");
    }

    #[test]
    fn names_from_the_config_are_escaped() {
        let mut p = profile_from_url("redis://u:p@h:1");
        p.name = "evil\u{1b}[2J".into();
        let c = Connection::of(&p);
        assert_eq!(c.name, "evil\\x1b[2J");
        p.endpoint = Endpoint {
            host: "h".into(),
            port: 1,
            tls: false,
        };
        assert_eq!(Connection::of(&p).url, "redis://h:1/0");
    }

    #[test]
    fn a_docker_instance_never_carries_its_password() {
        let mut p = profile_from_url("redis://:container-pw@127.0.0.1:32768");
        p.name = "cache".into();
        let d = conn::docker::DiscoveredInstance {
            container: "cache".into(),
            image: "redis:7".into(),
            profile: p,
            credentials: Some(crate::model::Credentials {
                username: None,
                password: secrecy::SecretString::from("container-pw".to_string()),
            }),
            auth: conn::docker::DiscoveredAuth::Password,
        };
        let json = serde_json::to_string(&Docker::of(&d)).unwrap();
        assert!(!json.contains("container-pw"), "{json}");
        assert!(json.contains("\"auth\":\"password\""), "{json}");
    }
}

/// One of Overview's findings before an analysis ("FROM THE SERVER"), computed by the
/// TUI's own code (`views::overview::server_findings_of`): what was found, how much it
/// matters, and where it is looked into.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Finding {
    /// `warn`, `note` or `info`.
    pub severity: &'static str,
    pub text: String,
    /// The screen it points to: `big_keys`, `expiry`, `changes`, `hot_keys`,
    /// `slow_log`, `clients`, ...
    pub screen: &'static str,
    /// The area and panel names, as the TUI shows them (`Server`, `Slow log`).
    pub area: &'static str,
    pub panel: &'static str,
}

impl Finding {
    pub fn of(f: &crate::tui::app::views::overview::Finding) -> Self {
        use crate::analysis::Severity;
        use crate::tui::app::Screen;
        Self {
            severity: match f.severity {
                Severity::Warn => "warn",
                Severity::Note => "note",
                Severity::Info => "info",
            },
            text: escape(&f.text),
            screen: match f.to {
                Screen::Connections => "connections",
                Screen::Overview => "overview",
                Screen::Patterns | Screen::KeyList | Screen::KeyDetail => "keys",
                Screen::Streams => "streams",
                Screen::Search => "search",
                Screen::BigKeys => "big_keys",
                Screen::Ttl => "expiry",
                Screen::Diff => "changes",
                Screen::HotKeys => "hot_keys",
                Screen::Nodes => "nodes",
                Screen::Slots => "slots",
                Screen::Clients => "clients",
                Screen::SlowLog => "slow_log",
                Screen::Monitor => "monitor",
                Screen::PubSub => "pubsub",
            },
            area: f.to.area().title(),
            panel: f.place(),
        }
    }
}
