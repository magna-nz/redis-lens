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
        };
        let text = toml::to_string(&p).unwrap();
        for word in ["password", "username", "user", "secret", "token"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
    }
}
