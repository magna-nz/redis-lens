//! Sampling a live server and building a [`ScanReport`].
//!
//! Owner: scanner.

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use redis::aio::ConnectionLike;
use redis::{FromRedisValue, Value};
use tokio::sync::watch;

use crate::model::{
    KeySample, KeyType, PERM_FREQ, PERM_MEMORY, PermissionReport, ScanReport, ServerFlavor,
    ServerInfo, ServerMode, Ttl,
};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Stop after sampling this many keys.
    pub sample_limit: u64,
    /// `COUNT` hint passed to `SCAN`.
    pub scan_count: u32,
    /// Keys per pipelined batch of `TYPE` / `PTTL` / `MEMORY USAGE` / `OBJECT FREQ`.
    pub batch_size: usize,
    /// Upper bound on commands per second sent to the server. `None` means no limit.
    pub max_commands_per_sec: Option<u32>,
    /// Only scan keys matching this glob (`SCAN ... MATCH`).
    pub match_glob: Option<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sample_limit: 100_000,
            scan_count: 1_000,
            batch_size: 500,
            max_commands_per_sec: Some(5_000),
            match_glob: None,
        }
    }
}

impl ScanOptions {
    /// Defaults for a profile: no command budget for `Local` profiles, 5,000 commands per
    /// second for everything else.
    pub fn for_profile(profile: &crate::model::Profile) -> Self {
        Self {
            max_commands_per_sec: if profile.env == crate::model::Env::Local {
                None
            } else {
                Some(5_000)
            },
            ..Self::default()
        }
    }
}

/// Progress published while a scan runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub sampled: u64,
    /// The number of keys the scan expects to sample: `min(sample_limit, DBSIZE)`.
    pub target: u64,
    pub done: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Redis(#[from] redis::RedisError),
    #[error("unexpected reply from the server: {0}")]
    Protocol(String),
}

/// Read `INFO server`, `INFO memory` and `DBSIZE`.
pub async fn server_info<C: ConnectionLike + Send>(conn: &mut C) -> Result<ServerInfo, ScanError> {
    let server: String = redis::cmd("INFO").arg("server").query_async(conn).await?;
    let memory: String = redis::cmd("INFO").arg("memory").query_async(conn).await?;
    let dbsize: u64 = redis::cmd("DBSIZE").query_async(conn).await?;
    parse_info(&server, &memory, dbsize)
}

/// Look up `field` in the text of an `INFO` section.
fn info_field<'a>(text: &'a str, field: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (k, v) = line.trim().split_once(':')?;
        (k == field).then_some(v.trim())
    })
}

/// Build a [`ServerInfo`] from the text of `INFO server`, `INFO memory` and `DBSIZE`.
///
/// Valkey also reports a compatibility `redis_version`; `valkey_version` wins.
pub fn parse_info(server: &str, memory: &str, dbsize: u64) -> Result<ServerInfo, ScanError> {
    let (flavor, version) = if let Some(v) = info_field(server, "valkey_version") {
        (ServerFlavor::Valkey, v)
    } else if let Some(v) = info_field(server, "redis_version") {
        let flavor = if info_field(server, "server_name") == Some("valkey") {
            ServerFlavor::Valkey
        } else {
            ServerFlavor::Redis
        };
        (flavor, v)
    } else {
        return Err(ScanError::Protocol(
            "INFO server has no redis_version or valkey_version".into(),
        ));
    };
    let mode = match info_field(server, "redis_mode") {
        None | Some("standalone") => ServerMode::Standalone,
        Some("cluster") => ServerMode::Cluster,
        Some("sentinel") => ServerMode::Sentinel,
        Some(other) => {
            return Err(ScanError::Protocol(format!("unknown redis_mode {other:?}")));
        }
    };
    let number = |field: &str| -> Result<u64, ScanError> {
        match info_field(memory, field) {
            None => Ok(0),
            Some(v) => v
                .parse()
                .map_err(|_| ScanError::Protocol(format!("bad {field} in INFO memory: {v:?}"))),
        }
    };
    Ok(ServerInfo {
        flavor,
        version: version.to_string(),
        mode,
        used_memory: number("used_memory")?,
        maxmemory: number("maxmemory")?,
        maxmemory_policy: info_field(memory, "maxmemory_policy")
            .unwrap_or("noeviction")
            .to_string(),
        total_keys: dbsize,
    })
}

/// Keeps the command rate at or below a limit by sleeping.
struct Limiter {
    per_sec: Option<u32>,
    start: Instant,
    sent: u64,
}

impl Limiter {
    fn new(per_sec: Option<u32>) -> Self {
        Self {
            per_sec: per_sec.filter(|n| *n > 0),
            start: Instant::now(),
            sent: 0,
        }
    }

    /// Record `n` commands just sent and sleep until the average rate is within the limit.
    async fn spent(&mut self, n: u64) {
        self.sent += n;
        if let Some(rate) = self.per_sec {
            let due = Duration::from_secs_f64(self.sent as f64 / f64::from(rate));
            let elapsed = self.start.elapsed();
            if due > elapsed {
                tokio::time::sleep(due - elapsed).await;
            }
        }
    }
}

fn parse<T: FromRedisValue>(v: &Value) -> Option<T> {
    T::from_redis_value(v.clone()).ok()
}

/// What [`sample_keys_detailed`] found.
#[derive(Debug, Clone, Default)]
pub struct SampleOutcome {
    pub samples: Vec<KeySample>,
    /// Distinct keys `SCAN` returned and the scan looked at. Includes keys that vanished
    /// before `TYPE`/`PTTL` (TYPE `none`, PTTL -2) or that could not be read (for
    /// example `NOPERM` from a key ACL), so it can be larger than `samples.len()`.
    pub keys_seen: u64,
    /// The `SCAN` cursor returned to 0 and every key it returned was looked at.
    pub complete: bool,
}

/// Walk the keyspace with `SCAN` and collect up to `opts.sample_limit` samples.
///
/// The same as [`sample_keys_detailed`], returning only the samples.
pub async fn sample_keys<C: ConnectionLike + Send>(
    conn: &mut C,
    opts: &ScanOptions,
    with_sizes: bool,
    with_freq: bool,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> Result<Vec<KeySample>, ScanError> {
    Ok(
        sample_keys_detailed(conn, opts, with_sizes, with_freq, progress)
            .await?
            .samples,
    )
}

/// Walk the keyspace with `SCAN`, look at up to `opts.sample_limit` keys, and report
/// how many keys were seen and whether the walk reached the end of the keyspace.
///
/// `with_sizes` and `with_freq` say whether `MEMORY USAGE` and `OBJECT FREQ` may be
/// sent; when false the matching [`KeySample`] fields are `None`. Keys that disappear
/// between `SCAN` and the batch are skipped. A failed `MEMORY USAGE` or `OBJECT FREQ`
/// (for example `NOPERM`) leaves that field `None` and does not fail the scan.
/// Keys that are not valid UTF-8 are skipped.
pub async fn sample_keys_detailed<C: ConnectionLike + Send>(
    conn: &mut C,
    opts: &ScanOptions,
    with_sizes: bool,
    with_freq: bool,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> Result<SampleOutcome, ScanError> {
    let limit = usize::try_from(opts.sample_limit).unwrap_or(usize::MAX);
    let batch_size = opts.batch_size.max(1);
    let mut limiter = Limiter::new(opts.max_commands_per_sec);
    let mut seen: HashSet<String> = HashSet::new();
    // Keys that are not valid UTF-8: not sampled, but still part of the keyspace.
    let mut skipped: HashSet<Vec<u8>> = HashSet::new();
    // Keys SCAN returned that were dropped because the limit was reached.
    let mut dropped = false;
    // Set once a SCAN reply brings the cursor back to 0. The cursor starts at 0 too,
    // so it can't be read on its own (a limit of 0 never scans).
    let mut reached_end = false;
    let mut pending: Vec<String> = Vec::new();
    let mut samples: Vec<KeySample> = Vec::new();
    let mut cursor: u64 = 0;

    // Keeps whatever target the caller set, including 0 for an empty database.
    let publish = |sampled: usize, done: bool| {
        if let Some(tx) = progress {
            tx.send_modify(|p| {
                p.sampled = sampled as u64;
                p.done = done;
            });
        }
    };

    while seen.len() < limit {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor).arg("COUNT").arg(opts.scan_count);
        if let Some(glob) = &opts.match_glob {
            cmd.arg("MATCH").arg(glob);
        }
        let (next, keys): (u64, Vec<Vec<u8>>) = cmd.query_async(conn).await?;
        limiter.spent(1).await;
        cursor = next;
        reached_end = cursor == 0;

        for raw in keys {
            if seen.len() >= limit {
                dropped = true;
                break;
            }
            let key = match String::from_utf8(raw) {
                Ok(k) => k,
                Err(e) => {
                    skipped.insert(e.into_bytes());
                    continue;
                }
            };
            if seen.insert(key.clone()) {
                pending.push(key);
            }
        }

        while pending.len() >= batch_size
            || (!pending.is_empty() && (cursor == 0 || seen.len() >= limit))
        {
            let n = pending.len().min(batch_size);
            let batch: Vec<String> = pending.drain(..n).collect();
            let sent = fetch_batch(conn, &batch, with_sizes, with_freq, &mut samples).await?;
            limiter.spent(sent).await;
            publish(samples.len(), false);
        }

        if cursor == 0 {
            break;
        }
    }
    publish(samples.len(), true);
    Ok(SampleOutcome {
        samples,
        keys_seen: (seen.len() + skipped.len()) as u64,
        complete: reached_end && !dropped,
    })
}

/// Pipeline the per-key commands for one batch. Returns the number of commands sent.
async fn fetch_batch<C: ConnectionLike + Send>(
    conn: &mut C,
    keys: &[String],
    with_sizes: bool,
    with_freq: bool,
    out: &mut Vec<KeySample>,
) -> Result<u64, ScanError> {
    let stride = 2 + usize::from(with_sizes) + usize::from(with_freq);
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    for key in keys {
        pipe.cmd("TYPE").arg(key);
        pipe.cmd("PTTL").arg(key);
        if with_sizes {
            pipe.cmd("MEMORY")
                .arg("USAGE")
                .arg(key)
                .arg("SAMPLES")
                .arg(5);
        }
        if with_freq {
            pipe.cmd("OBJECT").arg("FREQ").arg(key);
        }
    }
    let replies: Vec<Value> = pipe.query_async(conn).await?;
    if replies.len() != keys.len() * stride {
        return Err(ScanError::Protocol(format!(
            "pipeline returned {} replies, expected {}",
            replies.len(),
            keys.len() * stride
        )));
    }
    for (key, r) in keys.iter().zip(replies.chunks(stride)) {
        let Some(type_reply) = parse::<String>(&r[0]) else {
            continue;
        };
        if type_reply == "none" {
            continue;
        }
        let Some(ttl) = parse::<i64>(&r[1]).and_then(Ttl::from_pttl) else {
            continue;
        };
        let mut next = 2;
        let bytes = if with_sizes {
            next += 1;
            parse::<u64>(&r[next - 1])
        } else {
            None
        };
        let lfu_freq = if with_freq {
            parse::<u8>(&r[next])
        } else {
            None
        };
        out.push(KeySample {
            key: key.clone(),
            key_type: KeyType::from_type_reply(&type_reply),
            bytes,
            ttl,
            lfu_freq,
        });
    }
    Ok((keys.len() * stride) as u64)
}

/// How many hot keys a report keeps.
const HOT_KEY_LIMIT: usize = 50;

/// The sampled keys that have an LFU counter: highest first, ties by key, top 50.
pub fn hot_keys(samples: &[KeySample]) -> Vec<KeySample> {
    let mut hot: Vec<KeySample> = samples
        .iter()
        .filter(|s| s.lfu_freq.is_some())
        .cloned()
        .collect();
    hot.sort_by(|a, b| b.lfu_freq.cmp(&a.lfu_freq).then_with(|| a.key.cmp(&b.key)));
    hot.truncate(HOT_KEY_LIMIT);
    hot
}

/// The factor that turns sampled counts into keyspace-wide estimates.
///
/// - 1.0 when the scan saw the whole keyspace (`complete`).
/// - 1.0 with `MATCH`: DBSIZE counts every key, not just those matching the glob, so
///   it says nothing about how many matching keys exist.
/// - Otherwise `total_keys / keys_seen`, at least 1.0. `keys_seen` counts every key
///   SCAN returned, including ones that vanished or could not be read, so a key ACL
///   that hides most keys does not shrink the estimate.
fn estimate_scale(total_keys: u64, keys_seen: u64, complete: bool, has_match: bool) -> f64 {
    if complete || has_match || keys_seen == 0 {
        1.0
    } else {
        (total_keys as f64 / keys_seen as f64).max(1.0)
    }
}

/// Run a full scan: server info, sampling, then [`crate::pattern::aggregate`].
///
/// Uses `perms` to decide whether to ask for sizes and LFU counters.
pub async fn scan<C: ConnectionLike + Send>(
    profile_name: &str,
    conn: &mut C,
    opts: &ScanOptions,
    perms: &PermissionReport,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> Result<ScanReport, ScanError> {
    let server = server_info(conn).await?;
    let with_sizes = perms.allowed(PERM_MEMORY);
    let with_freq = perms.allowed(PERM_FREQ) && server.lfu_enabled();

    if let Some(tx) = progress {
        tx.send_replace(ScanProgress {
            sampled: 0,
            target: opts.sample_limit.min(server.total_keys),
            done: false,
        });
    }
    let outcome = sample_keys_detailed(conn, opts, with_sizes, with_freq, progress).await?;
    let scale = estimate_scale(
        server.total_keys,
        outcome.keys_seen,
        outcome.complete,
        opts.match_glob.is_some(),
    );
    let samples = outcome.samples;

    let taken_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    Ok(ScanReport {
        profile: profile_name.to_string(),
        taken_at_unix,
        server,
        sampled_keys: samples.len() as u64,
        patterns: crate::pattern::aggregate(&samples, scale),
        sizes_available: with_sizes,
        hot_keys: hot_keys(&samples),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(key: &str, freq: Option<u8>) -> KeySample {
        KeySample {
            key: key.into(),
            key_type: KeyType::String,
            bytes: None,
            ttl: Ttl::Persistent,
            lfu_freq: freq,
        }
    }

    #[test]
    fn hot_keys_sorted_and_filtered() {
        let samples = vec![
            sample("b", Some(5)),
            sample("none", None),
            sample("a", Some(5)),
            sample("z", Some(200)),
            sample("zero", Some(0)),
        ];
        let keys: Vec<_> = hot_keys(&samples).into_iter().map(|s| s.key).collect();
        assert_eq!(keys, ["z", "a", "b", "zero"]);
    }

    #[test]
    fn hot_keys_capped_at_fifty() {
        let samples: Vec<_> = (0..80)
            .map(|i| sample(&format!("k{i:02}"), Some(i as u8)))
            .collect();
        let hot = hot_keys(&samples);
        assert_eq!(hot.len(), 50);
        assert_eq!(hot[0].key, "k79");
        assert_eq!(hot[49].key, "k30");
    }

    #[test]
    fn for_profile_budget() {
        use crate::model::{Auth, Endpoint, Env, Profile};
        let mut p = Profile {
            name: "p".into(),
            env: Env::Local,
            endpoint: Endpoint {
                host: "h".into(),
                port: 1,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
        };
        assert_eq!(ScanOptions::for_profile(&p).max_commands_per_sec, None);
        p.env = Env::Prod;
        assert_eq!(
            ScanOptions::for_profile(&p).max_commands_per_sec,
            Some(5_000)
        );
        p.env = Env::Other;
        assert_eq!(
            ScanOptions::for_profile(&p).max_commands_per_sec,
            Some(5_000)
        );
    }

    const REDIS_SERVER: &str = "# Server\r\nredis_version:7.4.2\r\nredis_git_sha1:00000000\r\nredis_mode:standalone\r\nos:Linux 6.6 x86_64\r\nprocess_id:1\r\ntcp_port:6379\r\n";
    const REDIS_MEMORY: &str = "# Memory\r\nused_memory:1084912\r\nused_memory_human:1.03M\r\nmaxmemory:268435456\r\nmaxmemory_human:256.00M\r\nmaxmemory_policy:allkeys-lfu\r\nmem_fragmentation_ratio:9.1\r\n";
    const VALKEY_SERVER: &str = "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.1.3\r\nredis_mode:standalone\r\nos:Linux 6.6 x86_64\r\n";
    const VALKEY_MEMORY: &str =
        "# Memory\r\nused_memory:943120\r\nmaxmemory:0\r\nmaxmemory_policy:noeviction\r\n";

    #[test]
    fn scale_uses_keys_seen_not_samples() {
        // Saw 1,000 of 10,000 keys: scale 10, however many of them could be read.
        assert_eq!(estimate_scale(10_000, 1_000, false, false), 10.0);
        // A complete walk or MATCH reports the sample as it is.
        assert_eq!(estimate_scale(10_000, 1_000, true, false), 1.0);
        assert_eq!(estimate_scale(10_000, 1_000, false, true), 1.0);
        // Never below 1, and no division by zero.
        assert_eq!(estimate_scale(500, 1_000, false, false), 1.0);
        assert_eq!(estimate_scale(10_000, 0, false, false), 1.0);
        assert_eq!(estimate_scale(0, 0, true, false), 1.0);
    }

    #[test]
    fn parses_redis() {
        let info = parse_info(REDIS_SERVER, REDIS_MEMORY, 1234).unwrap();
        assert_eq!(info.flavor, ServerFlavor::Redis);
        assert_eq!(info.version, "7.4.2");
        assert_eq!(info.mode, ServerMode::Standalone);
        assert_eq!(info.used_memory, 1_084_912);
        assert_eq!(info.maxmemory, 268_435_456);
        assert_eq!(info.maxmemory_policy, "allkeys-lfu");
        assert_eq!(info.total_keys, 1234);
        assert!(info.lfu_enabled());
    }

    #[test]
    fn parses_valkey() {
        let info = parse_info(VALKEY_SERVER, VALKEY_MEMORY, 7).unwrap();
        assert_eq!(info.flavor, ServerFlavor::Valkey);
        assert_eq!(info.version, "8.1.3");
        assert_eq!(info.maxmemory, 0);
        assert_eq!(info.maxmemory_policy, "noeviction");
        assert!(!info.lfu_enabled());
    }

    #[test]
    fn parses_modes_and_errors() {
        let cluster = REDIS_SERVER.replace("standalone", "cluster");
        assert_eq!(
            parse_info(&cluster, REDIS_MEMORY, 0).unwrap().mode,
            ServerMode::Cluster
        );
        let sentinel = REDIS_SERVER.replace("standalone", "sentinel");
        assert_eq!(
            parse_info(&sentinel, REDIS_MEMORY, 0).unwrap().mode,
            ServerMode::Sentinel
        );
        assert!(parse_info("# Server\r\n", REDIS_MEMORY, 0).is_err());
        assert!(parse_info(REDIS_SERVER, "used_memory:abc\r\n", 0).is_err());
    }
}
