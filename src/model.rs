//! Types shared across modules.
//!
//! The scanner, the connection layer and the TUI all depend on these, so a change
//! here is a cross-module change. Keep them plain data.

use std::fmt;
use std::time::Duration;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The Redis data type of a key, as reported by `TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    Json,
    Other,
}

impl KeyType {
    /// Parse the reply of the `TYPE` command.
    pub fn from_type_reply(reply: &str) -> Self {
        match reply {
            "string" => Self::String,
            "hash" => Self::Hash,
            "list" => Self::List,
            "set" => Self::Set,
            "zset" => Self::ZSet,
            "stream" => Self::Stream,
            "ReJSON-RL" => Self::Json,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::List => "list",
            Self::Set => "set",
            Self::ZSet => "zset",
            Self::Stream => "stream",
            Self::Json => "json",
            Self::Other => "other",
        }
    }
}

/// Remaining time to live of a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ttl {
    /// The key has no expiry.
    Persistent,
    /// The key expires after this much time.
    Expires(Duration),
}

/// Labels for [`Ttl::bucket`], in bucket order.
pub const TTL_BUCKET_LABELS: [&str; 7] = ["none", "<1m", "<1h", "<1d", "<7d", "<30d", ">30d"];

impl Ttl {
    /// Parse a `PTTL` reply. `-2` means the key no longer exists and returns `None`.
    pub fn from_pttl(millis: i64) -> Option<Self> {
        match millis {
            -2 => None,
            n if n < 0 => Some(Self::Persistent),
            n => Some(Self::Expires(Duration::from_millis(n as u64))),
        }
    }

    /// Index into [`TTL_BUCKET_LABELS`].
    pub fn bucket(self) -> usize {
        const MINUTE: u64 = 60;
        const HOUR: u64 = 60 * MINUTE;
        const DAY: u64 = 24 * HOUR;
        match self {
            Self::Persistent => 0,
            Self::Expires(d) => match d.as_secs() {
                s if s < MINUTE => 1,
                s if s < HOUR => 2,
                s if s < DAY => 3,
                s if s < 7 * DAY => 4,
                s if s < 30 * DAY => 5,
                _ => 6,
            },
        }
    }
}

/// One sampled key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySample {
    pub key: String,
    pub key_type: KeyType,
    /// `MEMORY USAGE` in bytes. `None` when the user may not run `MEMORY USAGE`.
    pub bytes: Option<u64>,
    pub ttl: Ttl,
    /// `OBJECT FREQ` (LFU log counter, 0-255). `None` unless the eviction policy is LFU.
    pub lfu_freq: Option<u8>,
    /// On a cluster scan, the index into [`ScanReport::nodes`] of the node the key was
    /// sampled on. `None` on a standalone scan, and in snapshots written before v0.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<u16>,
}

/// Statistics for one inferred key pattern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatternStats {
    /// The pattern, with variable segments shown as placeholders, e.g. `user:{id}:cart`.
    pub pattern: String,
    /// The key type when every sampled key agrees, `None` when they differ.
    pub key_type: Option<KeyType>,
    /// Keys in the sample that matched this pattern.
    pub sampled_keys: u64,
    /// Estimated keys in the whole keyspace.
    pub est_keys: u64,
    /// Sum of `MEMORY USAGE` over the sampled keys. `None` without `MEMORY USAGE`.
    pub sampled_bytes: Option<u64>,
    /// Estimated bytes in the whole keyspace. `None` without `MEMORY USAGE`.
    pub est_bytes: Option<u64>,
    /// Sampled keys with no TTL.
    pub no_ttl: u64,
    /// Sampled keys per TTL bucket. Indices match [`TTL_BUCKET_LABELS`].
    pub ttl_hist: [u64; 7],
    pub size_p50: Option<u64>,
    pub size_p99: Option<u64>,
    pub size_max: Option<u64>,
    /// Up to five example keys, largest first.
    pub examples: Vec<KeySample>,
}

impl PatternStats {
    /// Share of sampled keys with no TTL, from 0.0 to 1.0.
    pub fn no_ttl_ratio(&self) -> f64 {
        if self.sampled_keys == 0 {
            0.0
        } else {
            self.no_ttl as f64 / self.sampled_keys as f64
        }
    }
}

// ---------------------------------------------------------------------------
// Servers and scan reports
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerFlavor {
    Redis,
    Valkey,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerMode {
    Standalone,
    Cluster,
    Sentinel,
}

/// What `INFO` and `DBSIZE` say about a server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub flavor: ServerFlavor,
    /// For example `7.4.2`.
    pub version: String,
    pub mode: ServerMode,
    /// `used_memory` from `INFO memory`, in bytes.
    pub used_memory: u64,
    /// `maxmemory` in bytes. `0` means no limit.
    pub maxmemory: u64,
    /// For example `allkeys-lfu`.
    pub maxmemory_policy: String,
    /// `DBSIZE` of the selected database.
    pub total_keys: u64,
}

impl ServerInfo {
    /// `OBJECT FREQ` only works when the eviction policy is LFU.
    pub fn lfu_enabled(&self) -> bool {
        self.maxmemory_policy.contains("lfu")
    }
}

/// The result of one scan. Serialized to disk as a snapshot for the Diff view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanReport {
    pub profile: String,
    /// Unix seconds when the scan finished.
    pub taken_at_unix: i64,
    pub server: ServerInfo,
    pub sampled_keys: u64,
    /// Sorted by estimated bytes (or estimated keys when sizes are unavailable), largest first.
    pub patterns: Vec<PatternStats>,
    /// False when `MEMORY USAGE` was not allowed, so every byte figure is `None`.
    pub sizes_available: bool,
    /// Up to 50 keys with the highest `OBJECT FREQ`, hottest first (ties by key). Empty when
    /// LFU counters were unavailable. Missing in snapshots written before v0.2.
    #[serde(default)]
    pub hot_keys: Vec<KeySample>,
    /// Every key in the database was looked at: the `SCAN` cursor returned to 0, no
    /// sample limit cut the walk short and no `MATCH` glob narrowed it. `false` in
    /// snapshots written before this field existed.
    #[serde(default)]
    pub complete: bool,
    /// The `SCAN MATCH` glob the scan used, if any.
    #[serde(default)]
    pub match_glob: Option<String>,
    /// Exact server-side figures (`INFO keysizes`, `INFO keyspace`, `MEMORY STATS`) to
    /// set beside the sampled estimates. `None` in snapshots written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_context: Option<ServerContext>,
    /// The Big keys and Expiry figures, from `MEMORY USAGE` of the sampled keys. `None`
    /// without sizes, and in snapshots written before it existed. Boxed so a
    /// [`ScanReport`] stays small (it travels in the TUI's messages). Read it through
    /// [`ScanReport::big_keys`] and [`ScanReport::ttl_bytes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_sizes: Option<Box<KeySizes>>,
    /// On a cluster scan, one entry per shard, in slot order: the node that was
    /// scanned for it and that node's own results. `None` on a standalone scan and in
    /// snapshots written before v0.3. On a cluster scan the report's other fields hold
    /// the merged results: `server` is the first scanned shard's flavor, version and policy with
    /// `used_memory`, `maxmemory` and `total_keys` summed over the shards, `patterns`
    /// are merged over the shards (estimates summed), and `complete` is true only when
    /// every shard was seen completely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<Vec<NodeReport>>,
}

impl ScanReport {
    /// [`KeySizes::big_keys`]; empty without sizes.
    pub fn big_keys(&self) -> &[KeySample] {
        self.key_sizes.as_deref().map_or(&[], |k| &k.big_keys)
    }

    /// [`KeySizes::ttl_bytes`]; `None` without sizes.
    pub fn ttl_bytes(&self) -> Option<&[u64; 7]> {
        self.key_sizes.as_deref().map(|k| &k.ttl_bytes)
    }
}

/// What a scan with sizes keeps about single keys beyond the patterns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeySizes {
    /// Up to 50 sampled keys with the largest `MEMORY USAGE`, largest first (ties by
    /// key), across every pattern: the Big keys panel. Key names only, never values.
    #[serde(default)]
    pub big_keys: Vec<KeySample>,
    /// Sum of `MEMORY USAGE` over the sampled keys in each TTL bucket, indices as in
    /// [`TTL_BUCKET_LABELS`] (index 0: no TTL): the Expiry panel. Sampled, not scaled.
    #[serde(default)]
    pub ttl_bytes: [u64; 7],
}

/// What was scanned for one shard of a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    Primary,
    Replica,
}

/// How far a shard's scan got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    /// Scanned to the end (the sample limit or the end of the keyspace).
    Scanned,
    /// Some results, but the scan of this node stopped early (an error mid-scan).
    Partial,
    /// No results for this shard: unreachable, refused, or failed before sampling.
    Failed,
}

/// One shard of a cluster scan: the node scanned for it and its own results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeReport {
    /// The cluster node ID (`CLUSTER SHARDS` / `CLUSTER SLOTS`) of the node scanned.
    pub id: String,
    /// The address the node was scanned at, as the cluster announced it (behind an SSH
    /// tunnel, the announced address, never the local forward). Never holds credentials.
    pub endpoint: Endpoint,
    /// Whether the shard's primary or one of its replicas was scanned.
    pub role: NodeRole,
    /// The node ID of the shard's primary; equal to `id` when `role` is `Primary`.
    pub primary_id: String,
    /// The slot ranges the shard serves, inclusive, in ascending order.
    pub slots: Vec<(u16, u16)>,
    /// `INFO` and `DBSIZE` of the node scanned. `None` when it could not be read.
    pub server: Option<ServerInfo>,
    pub status: NodeStatus,
    /// Why the status is not `Scanned`, or why a replica was wanted but the primary was
    /// scanned. Fixed text written by redis-lens, never a server's error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub sampled_keys: u64,
    /// The same as [`ScanReport::complete`], for this node alone.
    pub complete: bool,
    /// The same as [`ScanReport::sizes_available`], for this node alone.
    pub sizes_available: bool,
    /// This node's patterns, estimated for this node alone and sorted like
    /// [`ScanReport::patterns`]. `examples` is left empty (the merged rows hold them).
    pub patterns: Vec<PatternStats>,
}

/// Exact figures the server reports about itself: a [`ServerContextParts`], boxed so a
/// [`ScanReport`] stays small (it travels in the TUI's messages). Reads and writes its
/// fields through `Deref`; serialized as the parts themselves.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerContext(pub Box<ServerContextParts>);

impl std::ops::Deref for ServerContext {
    type Target = ServerContextParts;
    fn deref(&self) -> &ServerContextParts {
        &self.0
    }
}

impl std::ops::DerefMut for ServerContext {
    fn deref_mut(&mut self) -> &mut ServerContextParts {
        &mut self.0
    }
}

impl From<ServerContextParts> for ServerContext {
    fn from(parts: ServerContextParts) -> Self {
        Self(Box::new(parts))
    }
}

/// The parts of a [`ServerContext`]. Each part is optional: a section the server doesn't
/// have (`INFO keysizes` before Redis 8, Valkey) or a command it refuses (`MEMORY STATS`
/// on managed services) leaves it out, and [`ServerContextParts::missing`] says why.
/// Holds counts and sizes only: never key names, server error text or anything else
/// that could carry a username.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerContextParts {
    /// The scanned database's line of `INFO keyspace`. A database with no keys has no
    /// line, which reads as all zeros. `None` when `INFO keyspace` failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyspace: Option<KeyspaceInfo>,
    /// `MEMORY STATS`, when the permission check allowed it and the server answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryStats>,
    /// The scanned database's histograms from `INFO keysizes`, one per type the server
    /// reports, when the server has the section. Empty when it has the section but the
    /// database has no keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keysizes: Option<Vec<KeysizeHistogram>>,
    /// Why a part is missing, in the order the parts are read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<ContextGap>,
}

/// One `dbN:keys=…,expires=…,avg_ttl=…[,subexpiry=…]` line of `INFO keyspace`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyspaceInfo {
    pub db: u32,
    pub keys: u64,
    /// Keys with a TTL.
    pub expires: u64,
    /// Average TTL of the keys with one, in milliseconds (an estimate the server keeps).
    pub avg_ttl_ms: u64,
    /// Keys with field-level expiry (hash fields with a TTL). Redis 7.4 and later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subexpiry: Option<u64>,
}

/// The `MEMORY STATS` fields redis-lens uses. Each one is optional: fields differ
/// between versions and servers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryStats {
    /// `dataset.bytes`: used memory minus the server's own overhead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_bytes: Option<u64>,
    /// `overhead.total`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhead_total: Option<u64>,
    /// `keys.count`, over every database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys_count: Option<u64>,
    /// `keys.bytes-per-key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_key: Option<u64>,
    /// `fragmentation`: resident memory over allocated memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fragmentation: Option<f64>,
}

/// One `dbN_distrib_<type>_<unit>` line of `INFO keysizes`: a base-2 histogram that
/// the server keeps exactly, not a sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysizeHistogram {
    /// The type as the server names it: `strings`, `lists`, `sets`, `zsets`, `hashes`.
    pub key_type: String,
    /// `sizes` (string length in bytes) or `items` (elements in a collection).
    pub unit: String,
    /// `(lower bound, keys)`, ascending. A bin holds sizes from its bound up to, not
    /// including, twice the bound (the `0` bin holds only 0).
    pub bins: Vec<(u64, u64)>,
}

/// Why part of [`ServerContext`] is missing. Fixed reasons, never the server's error
/// text, which can name the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextGap {
    /// `INFO keyspace` failed.
    KeyspaceUnavailable,
    /// The permission check did not allow `MEMORY STATS` (`NOPERM`).
    MemoryStatsDenied,
    /// The server does not have `MEMORY STATS` or refused it with something other
    /// than `NOPERM` (managed services), or its reply could not be read.
    MemoryStatsUnavailable,
    /// The server has no `INFO keysizes` section (Redis before 8, Valkey).
    NoKeysizes,
    /// A reason written by a newer version.
    #[serde(other)]
    Other,
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// Which environment a connection belongs to. `Prod` connections are read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Env {
    Prod,
    Stage,
    Local,
    #[default]
    Other,
}

impl Env {
    /// The badge text shown in the TUI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Prod => "PROD",
            Self::Stage => "STAGE",
            Self::Local => "LOCAL",
            Self::Other => "OTHER",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub tls: bool,
}

/// `host:port`, with an IPv6 address in brackets: `[::1]:6379`.
impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') && !self.host.starts_with('[') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// How a profile authenticates. Nothing secret lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    /// No `AUTH`.
    #[default]
    None,
    /// Username and password, both stored in the OS keychain under [`Profile::secret_ref`].
    Keychain,
    /// Microsoft Entra ID through the redis-rs developer-tools credential chain (az login and similar). Needs the `entra-id` feature.
    EntraId,
    /// A short-lived token printed by [`Profile::token_command`], run at every connect
    /// and sent as the password. The username, if any, is in the OS keychain under
    /// [`Profile::secret_ref`] (rule 1); the token is never stored anywhere.
    Command,
}

/// A managed service recognised from the host name, or set by hand. A hint only: the
/// permission check stays authoritative for what works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    /// Amazon ElastiCache (node-based).
    Elasticache,
    /// Amazon ElastiCache Serverless: `MEMORY USAGE` and `OBJECT FREQ` are restricted.
    ElasticacheServerless,
    /// Amazon MemoryDB.
    Memorydb,
    /// Azure Cache for Redis or Azure Managed Redis.
    Azure,
    /// Google Cloud Memorystore (private IPs, so only ever set by hand).
    Memorystore,
}

/// A saved connection, as written to the config file.
///
/// There is deliberately no field for a username or password: credentials only ever
/// live in the OS keychain (see [`crate::secrets`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub env: Env,
    pub endpoint: Endpoint,
    #[serde(default)]
    pub db: u32,
    #[serde(default)]
    pub auth: Auth,
    /// Force read-only on or off. `None` means read-only exactly when `env` is `Prod`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// Scan a replica of this server instead of the primary, when one is reachable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prefer_replica: bool,
    /// The managed service this profile is on, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    /// For [`Auth::Command`]: the program and its arguments (never a shell string),
    /// whose standard output is the token. Not a secret; the token it prints is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_command: Option<Vec<String>>,
    /// Reach the server (and its cluster nodes and replicas) through an SSH bastion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshTunnel>,
    /// Ask these Sentinels where the primary is at every connect, instead of
    /// connecting to `endpoint` (spec "Sentinel (P4)"). `endpoint` then only holds the
    /// address given to `conn add`, shown until the first connect, and is never dialled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentinel: Option<Sentinel>,
    /// Labels to group and find profiles by, see [`valid_tag`]. Never secret.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A tag is 1 to 32 characters of letters, digits, `-`, `_`, `.` and `/`.
pub fn valid_tag(tag: &str) -> bool {
    (1..=32).contains(&tag.chars().count())
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// The Sentinels a profile asks for its primary (`[profile.sentinel]`). Nothing secret
/// lives here: a Sentinel's own username and password, when it has them, are in the OS
/// keychain under [`Profile::sentinel_secret_ref`], apart from the data node's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sentinel {
    /// The name the Sentinels monitor the primary under.
    pub master: String,
    /// The Sentinels, asked in this order. Never empty in a profile `conn add` writes.
    pub nodes: Vec<SentinelNode>,
    /// Whether the Sentinels themselves speak TLS (the data node's TLS is the
    /// endpoint's).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls: bool,
}

/// One Sentinel's address, written as `host:port` in the config file (an IPv6 address
/// in brackets, `[::1]:26379`). The port is 26379 when left out.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SentinelNode {
    /// A host name or an IP address, an IPv6 address without brackets.
    pub host: String,
    pub port: u16,
}

/// The port a Sentinel listens on when none is given.
pub const SENTINEL_DEFAULT_PORT: u16 = 26379;

/// Why a `host:port` could not be read as a Sentinel's address. Fixed texts that never
/// repeat the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SentinelAddrError {
    #[error("a Sentinel address is empty")]
    Empty,
    #[error("a Sentinel's port must be a number from 1 to 65535")]
    Port,
    #[error(
        "a Sentinel's host has a '[' without its ']', or something that is not an IPv6 address in brackets"
    )]
    Brackets,
    #[error("put a Sentinel's IPv6 address in brackets to give a port: [ADDRESS]:PORT")]
    BareIpv6,
    #[error(
        "a Sentinel's host must be a host name or an address of at most 253 bytes, with no \
         spaces, '@', '/' or leading '-'"
    )]
    Host,
}

impl std::str::FromStr for SentinelNode {
    type Err = SentinelAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(SentinelAddrError::Empty);
        }
        let port = |p: &str| match p.parse::<u16>() {
            Ok(n) if n != 0 => Ok(n),
            _ => Err(SentinelAddrError::Port),
        };
        let (host, port) = if let Some(inner) = s.strip_prefix('[') {
            let (h, after) = inner.split_once(']').ok_or(SentinelAddrError::Brackets)?;
            if h.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(SentinelAddrError::Brackets);
            }
            let p = match after {
                "" => SENTINEL_DEFAULT_PORT,
                a => port(a.strip_prefix(':').ok_or(SentinelAddrError::Brackets)?)?,
            };
            (h, p)
        } else if s.matches(':').count() > 1 {
            if s.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(SentinelAddrError::BareIpv6);
            }
            (s, SENTINEL_DEFAULT_PORT)
        } else if let Some((h, p)) = s.split_once(':') {
            (h, port(p)?)
        } else {
            (s, SENTINEL_DEFAULT_PORT)
        };
        let ok = !host.is_empty()
            && host.len() <= 253
            && !host.starts_with('-')
            && !host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '/' | '[' | ']'));
        if !ok {
            return Err(SentinelAddrError::Host);
        }
        Ok(Self {
            host: host.to_string(),
            port,
        })
    }
}

/// `host:port`, with an IPv6 address in brackets, as [`Endpoint`] shows it.
impl fmt::Display for SentinelNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl TryFrom<String> for SentinelNode {
    type Error = SentinelAddrError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<SentinelNode> for String {
    fn from(n: SentinelNode) -> Self {
        n.to_string()
    }
}

impl SentinelNode {
    /// This Sentinel as an endpoint to connect to, with the Sentinels' TLS setting.
    pub fn endpoint(&self, tls: bool) -> Endpoint {
        Endpoint {
            host: self.host.clone(),
            port: self.port,
            tls,
        }
    }
}

/// An SSH bastion that a profile's connections go through, with the system `ssh`
/// (`ssh -N -L`, `BatchMode=yes`, `StrictHostKeyChecking=yes`). Nothing secret lives
/// here: the SSH username, when given, is in the OS keychain under
/// [`Profile::ssh_secret_ref`], and key passphrases are never handled (an encrypted key
/// must be loaded in the SSH agent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnel {
    /// The bastion's host name or address, or a `Host` alias from `~/.ssh/config`.
    pub host: String,
    /// The bastion's SSH port. `None` leaves it to ssh (its config, else 22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// A private key file for `ssh -i`. `None` leaves it to ssh (its config, the agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<std::path::PathBuf>,
    /// A known_hosts file to check the bastion's host key against, instead of ssh's
    /// own (`UserKnownHostsFile`). Host keys are always checked strictly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hosts_file: Option<std::path::PathBuf>,
}

/// Service name for every keychain entry this tool creates.
pub const SECRET_SERVICE: &str = "redis-lens";

impl Profile {
    /// Where this profile's credentials live in the OS keychain.
    pub fn secret_ref(&self) -> SecretRef {
        SecretRef {
            service: SECRET_SERVICE.to_string(),
            account: format!("profile:{}", self.name),
        }
    }

    /// Where this profile's SSH username lives in the OS keychain (as the `username`
    /// of a [`Credentials`] with an empty password). A different prefix from
    /// [`Self::secret_ref`], so no profile name can make the two collide.
    pub fn ssh_secret_ref(&self) -> SecretRef {
        SecretRef {
            service: SECRET_SERVICE.to_string(),
            account: format!("ssh:profile:{}", self.name),
        }
    }

    /// Where the username and password of this profile's Sentinels live in the OS
    /// keychain, apart from the data node's (spec "Sentinel (P4)"). A prefix of its
    /// own, so no profile name can make it collide with another entry.
    pub fn sentinel_secret_ref(&self) -> SecretRef {
        SecretRef {
            service: SECRET_SERVICE.to_string(),
            account: format!("sentinel:profile:{}", self.name),
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only.unwrap_or(self.env == Env::Prod)
    }
}

/// Identifies one keychain entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretRef {
    pub service: String,
    pub account: String,
}

/// A username and password. Lives only in memory or in the OS keychain, never on disk
/// in our own files, and never in logs: `Debug` is redacted.
#[derive(Clone)]
pub struct Credentials {
    pub username: Option<String>,
    pub password: SecretString,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &"<redacted>")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Permission checks
// ---------------------------------------------------------------------------

/// Labels for the permission checks, in display order.
pub const PERM_SCAN: &str = "SCAN";
pub const PERM_TYPE_TTL: &str = "TYPE · TTL";
pub const PERM_MEMORY: &str = "MEMORY USAGE";
pub const PERM_FREQ: &str = "OBJECT FREQ";
pub const PERM_INFO: &str = "INFO";
/// `MEMORY STATS`, for the server context. Optional: without it only the dataset
/// figures are missing.
pub const PERM_MEMORY_STATS: &str = "MEMORY STATS";
pub const PERM_LABELS: [&str; 6] = [
    PERM_SCAN,
    PERM_TYPE_TTL,
    PERM_MEMORY,
    PERM_FREQ,
    PERM_INFO,
    PERM_MEMORY_STATS,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermStatus {
    Allowed,
    /// The server refused the command. Holds the server's error, e.g. `NOPERM ...`.
    Denied(String),
    /// Not checked, with the reason, e.g. "eviction policy is not LFU".
    Skipped(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermCheck {
    /// One of the `PERM_*` labels.
    pub label: &'static str,
    pub status: PermStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionReport {
    pub checks: Vec<PermCheck>,
}

impl PermissionReport {
    /// True when the check with this label ran and was allowed.
    pub fn allowed(&self, label: &str) -> bool {
        self.checks
            .iter()
            .any(|c| c.label == label && c.status == PermStatus::Allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_text_brackets_ipv6() {
        let ep = |host: &str| Endpoint {
            host: host.into(),
            port: 6379,
            tls: false,
        };
        assert_eq!(ep("::1").to_string(), "[::1]:6379");
        assert_eq!(ep("2001:db8::5").to_string(), "[2001:db8::5]:6379");
        assert_eq!(ep("10.0.1.21").to_string(), "10.0.1.21:6379");
        assert_eq!(ep("cache.internal").to_string(), "cache.internal:6379");
    }

    #[test]
    fn tag_validation() {
        for ok in ["a", "team-a", "a_b.c/d", "X9", &"x".repeat(32)] {
            assert!(valid_tag(ok), "{ok}");
        }
        for bad in ["", "a b", "a,b", "é", "a:b", "a\n", &"x".repeat(33)] {
            assert!(!valid_tag(bad), "{bad:?}");
        }
    }

    #[test]
    fn pttl_parsing() {
        assert_eq!(Ttl::from_pttl(-2), None);
        assert_eq!(Ttl::from_pttl(-1), Some(Ttl::Persistent));
        assert_eq!(
            Ttl::from_pttl(1500),
            Some(Ttl::Expires(Duration::from_millis(1500)))
        );
    }

    #[test]
    fn ttl_buckets_match_labels() {
        let s = Duration::from_secs;
        let cases = [
            (Ttl::Persistent, "none"),
            (Ttl::Expires(s(59)), "<1m"),
            (Ttl::Expires(s(60)), "<1h"),
            (Ttl::Expires(s(3_599)), "<1h"),
            (Ttl::Expires(s(3_600)), "<1d"),
            (Ttl::Expires(s(86_400)), "<7d"),
            (Ttl::Expires(s(7 * 86_400)), "<30d"),
            (Ttl::Expires(s(30 * 86_400)), ">30d"),
        ];
        for (ttl, label) in cases {
            assert_eq!(TTL_BUCKET_LABELS[ttl.bucket()], label, "{ttl:?}");
        }
    }

    #[test]
    fn prod_is_read_only_by_default() {
        let mut p = Profile {
            name: "p".into(),
            env: Env::Prod,
            endpoint: Endpoint {
                host: "h".into(),
                port: 6379,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            tags: Vec::new(),
        };
        assert!(p.is_read_only());
        p.env = Env::Local;
        assert!(!p.is_read_only());
        p.read_only = Some(true);
        assert!(p.is_read_only());
    }

    /// A report travels in the TUI's messages; a context adds one pointer to it.
    #[test]
    fn server_context_is_a_pointer() {
        assert_eq!(
            std::mem::size_of::<Option<ServerContext>>(),
            std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn credentials_debug_is_redacted() {
        let c = Credentials {
            username: Some("alice".into()),
            password: SecretString::from("hunter2"),
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("alice"));
        assert!(!dbg.contains("hunter2"));
    }

    #[test]
    fn profile_toml_has_no_secret_fields() {
        let p = Profile {
            name: "prod".into(),
            env: Env::Prod,
            endpoint: Endpoint {
                host: "h".into(),
                port: 6380,
                tls: true,
            },
            db: 0,
            auth: Auth::Keychain,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            tags: Vec::new(),
        };
        let text = toml::to_string(&p).unwrap();
        for word in ["password", "username", "user", "secret", "token"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
    }

    /// A snapshot written before `key_sizes` existed (the v0.2 shape,
    /// with `hot_keys` and `complete` but no server context) still loads, with no big
    /// keys and no TTL bytes; a new report round-trips with them.
    #[test]
    fn snapshots_without_key_sizes_still_load() {
        let old = r#"{
            "profile": "prod-cache-01",
            "taken_at_unix": 1794319331,
            "server": {
                "flavor": "redis", "version": "7.4.2", "mode": "standalone",
                "used_memory": 1000, "maxmemory": 0,
                "maxmemory_policy": "allkeys-lfu", "total_keys": 2
            },
            "sampled_keys": 2,
            "patterns": [{
                "pattern": "user:{id}:cart", "key_type": "hash",
                "sampled_keys": 2, "est_keys": 2,
                "sampled_bytes": 300, "est_bytes": 300,
                "no_ttl": 1, "ttl_hist": [1, 0, 0, 0, 1, 0, 0],
                "size_p50": 100, "size_p99": 200, "size_max": 200,
                "examples": [{
                    "key": "user:1:cart", "key_type": "hash", "bytes": 200,
                    "ttl": "Persistent", "lfu_freq": 3
                }]
            }],
            "sizes_available": true,
            "hot_keys": [],
            "complete": true
        }"#;
        let r: ScanReport = serde_json::from_str(old).expect("an old snapshot loads");
        assert!(r.big_keys().is_empty());
        assert_eq!(r.ttl_bytes(), None);
        assert_eq!(r.patterns[0].examples[0].key, "user:1:cart");

        // Empty and missing additions are not written, so a report without sizes
        // serializes exactly as before.
        let text = serde_json::to_string(&r).unwrap();
        assert!(
            !text.contains("key_sizes") && !text.contains("big_keys"),
            "{text}"
        );

        let mut new = r.clone();
        new.key_sizes = Some(Box::new(KeySizes {
            big_keys: new.patterns[0].examples.clone(),
            ttl_bytes: [200, 0, 0, 0, 100, 0, 0],
        }));
        let back: ScanReport = serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back, new);
    }

    #[test]
    fn ssh_secret_ref_never_collides_with_a_profile_entry() {
        let mut p = Profile {
            name: "a".into(),
            env: Env::Other,
            endpoint: Endpoint {
                host: "h".into(),
                port: 6379,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            tags: Vec::new(),
        };
        let ssh = p.ssh_secret_ref();
        assert_eq!(ssh.service, SECRET_SERVICE);
        assert_eq!(ssh.account, "ssh:profile:a");
        // No profile name gives a `profile:` entry equal to another's `ssh:` entry.
        p.name = "ssh:profile:a".into();
        assert!(!p.secret_ref().account.starts_with("ssh:"));
    }

    #[test]
    fn sentinel_secret_ref_is_its_own_entry() {
        let p: Profile =
            toml::from_str("name = \"a\"\n[endpoint]\nhost = \"h\"\nport = 6379\n").unwrap();
        let s = p.sentinel_secret_ref();
        assert_eq!(s.service, SECRET_SERVICE);
        assert_eq!(s.account, "sentinel:profile:a");
        assert_ne!(s, p.secret_ref());
        assert_ne!(s, p.ssh_secret_ref());
        assert_eq!(p.sentinel, None);
        assert!(!toml::to_string(&p).unwrap().contains("sentinel"));
    }

    #[test]
    fn sentinel_addresses_parse() {
        let n = |h: &str, port: u16| SentinelNode {
            host: h.into(),
            port,
        };
        let ok = |s: &str| s.parse::<SentinelNode>();
        assert_eq!(ok("s1"), Ok(n("s1", 26379)));
        assert_eq!(ok(" s1:5000 "), Ok(n("s1", 5000)));
        assert_eq!(ok("10.0.0.1:26380"), Ok(n("10.0.0.1", 26380)));
        assert_eq!(ok("[::1]:26380"), Ok(n("::1", 26380)));
        assert_eq!(ok("[2001:db8::1]"), Ok(n("2001:db8::1", 26379)));
        assert_eq!(ok("2001:db8::1"), Ok(n("2001:db8::1", 26379)));
        assert_eq!(ok(""), Err(SentinelAddrError::Empty));
        assert_eq!(ok("s1:0"), Err(SentinelAddrError::Port));
        assert_eq!(ok("s1:x"), Err(SentinelAddrError::Port));
        assert_eq!(ok("s1:"), Err(SentinelAddrError::Port));
        assert_eq!(ok("[s1]:1"), Err(SentinelAddrError::Brackets));
        assert_eq!(ok("[::1"), Err(SentinelAddrError::Brackets));
        assert_eq!(ok("[::1]x"), Err(SentinelAddrError::Brackets));
        assert_eq!(ok("a:b:c"), Err(SentinelAddrError::BareIpv6));
        for bad in ["-oProxyCommand", "u@h", "a b", "a/b", ":1"] {
            assert_eq!(ok(bad), Err(SentinelAddrError::Host), "{bad}");
        }
        assert_eq!(n("::1", 1).to_string(), "[::1]:1");
        assert_eq!(n("s1", 26379).to_string(), "s1:26379");
    }

    proptest::proptest! {
        /// Every address round-trips through its text, and parsing never panics.
        #[test]
        fn sentinel_addresses_round_trip(
            host in "[a-z0-9][a-z0-9.-]{0,30}|[0-9a-f:]{2,20}",
            port in 1u16..,
        ) {
            if let Ok(n) = format!("{host}:{port}").parse::<SentinelNode>() {
                let back: SentinelNode = n.to_string().parse().unwrap();
                proptest::prop_assert_eq!(back, n);
            }
            let v6 = SentinelNode { host: "2001:db8::5".into(), port };
            proptest::prop_assert_eq!(v6.to_string().parse::<SentinelNode>().unwrap(), v6);
        }

        #[test]
        fn sentinel_address_parsing_never_panics(text in "\\PC{0,40}") {
            let _ = text.parse::<SentinelNode>();
        }
    }

    #[test]
    fn ssh_tunnel_is_absent_from_toml_unless_set_and_holds_no_user() {
        let mut p = Profile {
            name: "bastioned".into(),
            env: Env::Prod,
            endpoint: Endpoint {
                host: "10.0.0.5".into(),
                port: 6379,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            tags: Vec::new(),
        };
        assert!(!toml::to_string(&p).unwrap().contains("ssh"));
        p.ssh = Some(SshTunnel {
            host: "bastion.example.com".into(),
            port: Some(2222),
            identity_file: None,
            known_hosts_file: Some("/tmp/kh".into()),
        });
        let text = toml::to_string(&p).unwrap();
        assert!(text.contains("bastion.example.com"), "{text}");
        assert!(!text.contains("identity_file"), "{text}");
        for word in ["password", "username", "user", "secret", "passphrase"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
        let back: Profile = toml::from_str(&text).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn reports_without_nodes_still_load_and_nodes_round_trip() {
        let sample = KeySample {
            key: "k".into(),
            key_type: KeyType::String,
            bytes: Some(10),
            ttl: Ttl::Persistent,
            lfu_freq: Some(3),
            node: None,
        };
        let json = serde_json::to_string(&sample).unwrap();
        assert!(!json.contains("node"), "{json}");
        let old: KeySample = serde_json::from_str(&json).unwrap();
        assert_eq!(old.node, None);
        let on_node = KeySample {
            node: Some(2),
            ..sample
        };
        let back: KeySample =
            serde_json::from_str(&serde_json::to_string(&on_node).unwrap()).unwrap();
        assert_eq!(back.node, Some(2));

        let node = NodeReport {
            id: "a".repeat(40),
            endpoint: Endpoint {
                host: "10.0.0.7".into(),
                port: 7000,
                tls: false,
            },
            role: NodeRole::Replica,
            primary_id: "b".repeat(40),
            slots: vec![(0, 5460)],
            server: None,
            status: NodeStatus::Failed,
            note: Some("unreachable".into()),
            sampled_keys: 0,
            complete: false,
            sizes_available: false,
            patterns: Vec::new(),
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains(r#""role":"replica""#), "{json}");
        assert!(json.contains(r#""status":"failed""#), "{json}");
        assert_eq!(serde_json::from_str::<NodeReport>(&json).unwrap(), node);
    }
}
