//! Sampling a live server and building a [`ScanReport`].
//!
//! Owner: scanner.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use redis::aio::ConnectionLike;
use redis::{FromRedisValue, Value};
use tokio::sync::watch;

use crate::model::{
    KeySample, KeyType, PERM_FREQ, PERM_MEMORY, PatternStats, PermissionReport, ScanReport,
    ServerFlavor, ServerInfo, ServerMode, Ttl,
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
    /// Of `keys_seen`, the keys that no longer existed when they were looked up: `TYPE`
    /// said `none` or `PTTL` said -2 (deleted or expired since `SCAN` returned them).
    pub vanished: u64,
    /// The `SCAN` cursor returned to 0 and every key it returned was looked at.
    pub complete: bool,
}

impl SampleOutcome {
    /// Keys seen that still existed when they were looked up: the samples, plus keys that
    /// exist but could not be read (`NOPERM`, not valid UTF-8). This is the denominator
    /// of [`estimate_scale`].
    pub fn keys_present(&self) -> u64 {
        self.keys_seen.saturating_sub(self.vanished)
    }
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
    sample_inner(conn, opts, with_sizes, with_freq, progress, None).await
}

/// [`sample_keys_detailed`], also publishing partial pattern stats to `partial` (throttled)
/// after each batch. Aggregating sends nothing to the server.
async fn sample_inner<C: ConnectionLike + Send>(
    conn: &mut C,
    opts: &ScanOptions,
    with_sizes: bool,
    with_freq: bool,
    progress: Option<&watch::Sender<ScanProgress>>,
    mut partial: Option<&mut PartialSink<'_>>,
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
    // Keys looked up that no longer existed.
    let mut vanished: u64 = 0;
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
            let batch_out = fetch_batch(conn, &batch, with_sizes, with_freq, &mut samples).await?;
            vanished += batch_out.vanished;
            if let Some(sink) = partial.as_deref_mut() {
                // Keys looked up so far that still existed; `pending` hasn't been looked up.
                let looked_up = (seen.len() - pending.len() + skipped.len()) as u64;
                let present = looked_up.saturating_sub(vanished);
                sink.maybe_publish(&samples, present, Instant::now());
            }
            limiter.spent(batch_out.sent).await;
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
        vanished,
        complete: reached_end && !dropped,
    })
}

/// What [`fetch_batch`] did.
struct BatchOutcome {
    /// Commands sent.
    sent: u64,
    /// Keys that no longer existed: `TYPE` said `none` or `PTTL` said -2.
    vanished: u64,
}

/// Pipeline the per-key commands for one batch and push a sample for each key that could
/// be read.
///
/// A key that has gone (`TYPE` `none`, `PTTL` -2) counts as vanished. A key whose `TYPE`
/// or `PTTL` failed (for example `NOPERM` from a key ACL) exists but is unreadable: it
/// is neither sampled nor vanished.
async fn fetch_batch<C: ConnectionLike + Send>(
    conn: &mut C,
    keys: &[String],
    with_sizes: bool,
    with_freq: bool,
    out: &mut Vec<KeySample>,
) -> Result<BatchOutcome, ScanError> {
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
    let mut vanished = 0;
    for (key, r) in keys.iter().zip(replies.chunks(stride)) {
        // An error (NOPERM) means the key exists but can't be read.
        let Some(type_reply) = parse::<String>(&r[0]) else {
            continue;
        };
        if type_reply == "none" {
            vanished += 1;
            continue;
        }
        let Some(pttl) = parse::<i64>(&r[1]) else {
            continue;
        };
        // `None` only for -2: the key expired or was deleted after TYPE.
        let Some(ttl) = Ttl::from_pttl(pttl) else {
            vanished += 1;
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
    Ok(BatchOutcome {
        sent: (keys.len() * stride) as u64,
        vanished,
    })
}

/// How many hot keys a report keeps.
const HOT_KEY_LIMIT: usize = 50;

/// The sampled keys with an LFU counter above 0: highest first, ties by key, top 50.
pub fn hot_keys(samples: &[KeySample]) -> Vec<KeySample> {
    let mut hot: Vec<KeySample> = samples
        .iter()
        .filter(|s| s.lfu_freq.is_some_and(|f| f > 0))
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
/// - 1.0 when no key seen still existed (`keys_present` 0): there is nothing to scale.
/// - Otherwise `total_keys / keys_present`, at least 1.0.
///
/// `keys_present` ([`SampleOutcome::keys_present`]) is the keys SCAN returned that still
/// existed when they were looked up: the samples, plus keys that exist but could not be
/// read (`NOPERM` from a key ACL, names that are not UTF-8). Unreadable keys stay in, so
/// a key ACL that hides most keys does not inflate the estimate of the keys it allows.
/// Keys that vanished between SCAN and the lookup are left out: they belong to some
/// pattern that the samples stand for, and counting them would shrink every estimate
/// by the vanished fraction.
fn estimate_scale(total_keys: u64, keys_present: u64, complete: bool, has_match: bool) -> f64 {
    if complete || has_match || keys_present == 0 {
        1.0
    } else {
        (total_keys as f64 / keys_present as f64).max(1.0)
    }
}

/// Pattern statistics aggregated from the keys sampled so far, published while a scan
/// runs (see [`scan_with_partial`]).
///
/// Holds pattern data only (patterns, counts, sizes, example key names), never values
/// or credentials. It is a preview: only the final [`ScanReport`] is ever saved to a
/// snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct PartialPatterns {
    /// Sorted like [`ScanReport::patterns`]. Estimates use [`estimate_scale`] on the
    /// counts so far, as the final report does on the final counts.
    pub patterns: Vec<PatternStats>,
    /// Keys sampled when these stats were aggregated.
    pub sampled: u64,
    /// The same as [`ScanProgress::target`].
    pub target: u64,
    /// The same as [`ScanReport::sizes_available`].
    pub sizes_available: bool,
}

/// Where [`scan_with_partial`] publishes [`PartialPatterns`]. Each update replaces the
/// previous one; the channel's initial value should be `None`.
pub type PartialSender = watch::Sender<Option<Arc<PartialPatterns>>>;

/// Shortest gap between two partial aggregations: at most four a second.
const PARTIAL_MIN_GAP: Duration = Duration::from_millis(250);
/// The gap is also at least this many times what the last aggregation took, so
/// re-aggregating a large sample takes at most a tenth of the scan's time.
const PARTIAL_COST_FACTOR: u32 = 9;

/// Decides when the next partial aggregation may run. Takes the time as an argument so
/// tests don't sleep.
#[derive(Debug, Default)]
struct Throttle {
    next: Option<Instant>,
}

impl Throttle {
    /// The first call is always ready.
    fn ready(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }

    /// An aggregation ran from `started` to `finished`.
    fn record(&mut self, started: Instant, finished: Instant) {
        let cost = finished.saturating_duration_since(started);
        self.next = Some(finished + PARTIAL_MIN_GAP.max(cost * PARTIAL_COST_FACTOR));
    }
}

/// Publishes partial pattern stats while sampling.
struct PartialSink<'a> {
    tx: &'a PartialSender,
    total_keys: u64,
    target: u64,
    has_match: bool,
    sizes_available: bool,
    throttle: Throttle,
}

impl PartialSink<'_> {
    /// Aggregate `samples` and publish them, unless the throttle says wait or there is
    /// nothing to show yet. `keys_present` is the estimator's denominator so far.
    fn maybe_publish(&mut self, samples: &[KeySample], keys_present: u64, now: Instant) {
        if samples.is_empty() || !self.throttle.ready(now) {
            return;
        }
        // The walk hasn't finished, so it isn't complete yet.
        let scale = estimate_scale(self.total_keys, keys_present, false, self.has_match);
        self.publish(crate::pattern::aggregate(samples, scale), samples.len());
        self.throttle.record(now, Instant::now());
    }

    fn publish(&self, patterns: Vec<PatternStats>, sampled: usize) {
        self.tx.send_replace(Some(Arc::new(PartialPatterns {
            patterns,
            sampled: sampled as u64,
            target: self.target,
            sizes_available: self.sizes_available,
        })));
    }
}

/// Run a full scan: server info, sampling, then [`crate::pattern::aggregate`].
///
/// Uses `perms` to decide whether to ask for sizes and LFU counters. The same as
/// [`scan_with_partial`] without partial results.
pub async fn scan<C: ConnectionLike + Send>(
    profile_name: &str,
    conn: &mut C,
    opts: &ScanOptions,
    perms: &PermissionReport,
    progress: Option<&watch::Sender<ScanProgress>>,
) -> Result<ScanReport, ScanError> {
    scan_with_partial(profile_name, conn, opts, perms, progress, None).await
}

/// [`scan`], also publishing [`PartialPatterns`] to `partial` while it runs.
///
/// Partial stats are aggregated client-side from the samples already fetched: no extra
/// commands go to the server. They are throttled to at most four a second (less when
/// aggregating takes long), with one more at the end holding the final patterns. A scan
/// that fails publishes nothing more, and its last partial stays in the channel: the
/// caller must drop it on error.
pub async fn scan_with_partial<C: ConnectionLike + Send>(
    profile_name: &str,
    conn: &mut C,
    opts: &ScanOptions,
    perms: &PermissionReport,
    progress: Option<&watch::Sender<ScanProgress>>,
    partial: Option<&PartialSender>,
) -> Result<ScanReport, ScanError> {
    let server = server_info(conn).await?;
    let with_sizes = perms.allowed(PERM_MEMORY);
    let with_freq = perms.allowed(PERM_FREQ) && server.lfu_enabled();
    let target = opts.sample_limit.min(server.total_keys);
    let has_match = opts.match_glob.is_some();

    if let Some(tx) = progress {
        tx.send_replace(ScanProgress {
            sampled: 0,
            target,
            done: false,
        });
    }
    let mut sink = partial.map(|tx| PartialSink {
        tx,
        total_keys: server.total_keys,
        target,
        has_match,
        sizes_available: with_sizes,
        throttle: Throttle::default(),
    });
    let outcome = sample_inner(conn, opts, with_sizes, with_freq, progress, sink.as_mut()).await?;
    let scale = estimate_scale(
        server.total_keys,
        outcome.keys_present(),
        outcome.complete,
        has_match,
    );
    // With MATCH the walk only looks at matching keys, so it never saw the whole database.
    let complete = outcome.complete && !has_match;
    let samples = outcome.samples;
    let patterns = crate::pattern::aggregate(&samples, scale);
    if let Some(sink) = &sink {
        sink.publish(patterns.clone(), samples.len());
    }

    let taken_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    Ok(ScanReport {
        profile: profile_name.to_string(),
        taken_at_unix,
        server,
        sampled_keys: samples.len() as u64,
        patterns,
        sizes_available: with_sizes,
        hot_keys: hot_keys(&samples),
        complete,
        match_glob: opts.match_glob.clone(),
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
        // A counter of 0 means the key hasn't been read since its counter decayed.
        assert_eq!(keys, ["z", "a", "b"]);
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
            prefer_replica: false,
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
    fn scale_edge_cases() {
        // 1,000 of 10,000 keys seen still existed: scale 10, however many could be read.
        assert_eq!(estimate_scale(10_000, 1_000, false, false), 10.0);
        // A complete walk or MATCH reports the sample as it is.
        assert_eq!(estimate_scale(10_000, 1_000, true, false), 1.0);
        assert_eq!(estimate_scale(10_000, 1_000, false, true), 1.0);
        // Never below 1 (DBSIZE smaller than the keys looked up), and no division by
        // zero: nothing seen, or every key seen vanished.
        assert_eq!(estimate_scale(500, 1_000, false, false), 1.0);
        assert_eq!(estimate_scale(10_000, 0, false, false), 1.0);
        assert_eq!(estimate_scale(0, 0, true, false), 1.0);
        let all_vanished = SampleOutcome {
            samples: Vec::new(),
            keys_seen: 1_000,
            vanished: 1_000,
            complete: false,
        };
        assert_eq!(all_vanished.keys_present(), 0);
        assert_eq!(
            estimate_scale(10_000, all_vanished.keys_present(), false, false),
            1.0
        );
    }

    #[test]
    fn throttle_waits_at_least_a_quarter_second() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut t = Throttle::default();
        assert!(t.ready(t0), "the first aggregation runs at once");
        t.record(t0, t0 + ms(1));
        assert!(!t.ready(t0 + ms(100)));
        assert!(!t.ready(t0 + ms(250)));
        assert!(t.ready(t0 + ms(251)));
        // A slow aggregation (100 ms) pushes the next one out to 9x its cost.
        t.record(t0, t0 + ms(100));
        assert!(!t.ready(t0 + ms(999)));
        assert!(t.ready(t0 + ms(1_000)));
    }

    /// A keyspace held in memory, answering the commands the scanner sends. Any other
    /// command (`KEYS`, a write) panics.
    mod fake {
        use std::collections::HashMap;
        use std::sync::Arc;

        use redis::{Arg, Cmd, Pipeline, RedisFuture, Value};
        use tokio::sync::watch;

        use super::super::PartialPatterns;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Fate {
            Present,
            /// Deleted between SCAN and TYPE: TYPE `none`, PTTL -2.
            Gone,
            /// Expired between TYPE and PTTL: TYPE `string`, PTTL -2.
            Expired,
            /// Hidden by a key ACL: TYPE and PTTL fail with NOPERM.
            NoPerm,
            /// TYPE works, PTTL fails with NOPERM.
            PttlDenied,
        }

        pub struct Server {
            pub keys: Vec<(Vec<u8>, Fate)>,
            fates: HashMap<Vec<u8>, Fate>,
            pub dbsize: u64,
            /// Keys per SCAN reply.
            pub page: usize,
            /// Every command name received, in order.
            pub log: Vec<String>,
            /// Watches the partial channel: each SCAN records `(sampled, sum of est_keys)`
            /// of the partial published so far.
            pub observer: Option<watch::Receiver<Option<Arc<PartialPatterns>>>>,
            pub seen_partials: Vec<Option<(u64, u64)>>,
            /// Fail this SCAN call (0-based) with an I/O error.
            pub fail_scan: Option<usize>,
            scans: usize,
        }

        impl Server {
            pub fn new(keys: Vec<(Vec<u8>, Fate)>, page: usize) -> Self {
                let fates = keys.iter().cloned().collect();
                Self {
                    dbsize: keys.len() as u64,
                    keys,
                    fates,
                    page,
                    log: Vec::new(),
                    observer: None,
                    seen_partials: Vec::new(),
                    fail_scan: None,
                    scans: 0,
                }
            }

            /// `Err` when this is the SCAN call set to fail.
            fn fails(&mut self, cmd: &Cmd) -> bool {
                let is_scan = cmd
                    .args_iter()
                    .next()
                    .is_some_and(|a| matches!(a, Arg::Simple(b) if b == b"SCAN"));
                if !is_scan {
                    return false;
                }
                self.scans += 1;
                self.fail_scan == Some(self.scans - 1)
            }

            fn reply(&mut self, cmd: &Cmd) -> Value {
                let args: Vec<Vec<u8>> = cmd
                    .args_iter()
                    .filter_map(|a| match a {
                        Arg::Simple(b) => Some(b.to_vec()),
                        _ => None,
                    })
                    .collect();
                let name = String::from_utf8_lossy(&args[0]).to_uppercase();
                self.log.push(name.clone());
                let text = |t: &str| String::from_utf8_lossy(&args[1]) == t;
                let fate = || self.fates.get(args.last().unwrap()).copied();
                let noperm = || {
                    redis::parse_redis_value(b"-NOPERM no permissions to access a key\r\n").unwrap()
                };
                match name.as_str() {
                    "INFO" if text("server") => {
                        Value::BulkString(super::REDIS_SERVER.as_bytes().to_vec())
                    }
                    "INFO" => Value::BulkString(super::VALKEY_MEMORY.as_bytes().to_vec()),
                    "DBSIZE" => Value::Int(self.dbsize as i64),
                    "SCAN" => {
                        if let Some(rx) = &self.observer {
                            let seen = rx
                                .borrow()
                                .as_ref()
                                .map(|p| (p.sampled, p.patterns.iter().map(|p| p.est_keys).sum()));
                            self.seen_partials.push(seen);
                        }
                        let start: usize = String::from_utf8_lossy(&args[1]).parse().unwrap();
                        let end = (start + self.page).min(self.keys.len());
                        let next = if end >= self.keys.len() { 0 } else { end };
                        let keys = self.keys[start..end]
                            .iter()
                            .map(|(k, _)| Value::BulkString(k.clone()))
                            .collect();
                        Value::Array(vec![
                            Value::BulkString(next.to_string().into_bytes()),
                            Value::Array(keys),
                        ])
                    }
                    "TYPE" => match fate() {
                        Some(Fate::Present | Fate::Expired | Fate::PttlDenied) => {
                            Value::SimpleString("string".into())
                        }
                        Some(Fate::NoPerm) => noperm(),
                        Some(Fate::Gone) | None => Value::SimpleString("none".into()),
                    },
                    "PTTL" => match fate() {
                        Some(Fate::Present) => Value::Int(-1),
                        Some(Fate::NoPerm | Fate::PttlDenied) => noperm(),
                        Some(Fate::Gone | Fate::Expired) | None => Value::Int(-2),
                    },
                    "MEMORY" if text("USAGE") => match self.fates.get(&args[2]) {
                        Some(Fate::Present) => Value::Int(100),
                        Some(Fate::NoPerm) => noperm(),
                        _ => Value::Nil,
                    },
                    other => panic!("the scanner must not send {other}"),
                }
            }
        }

        impl redis::aio::ConnectionLike for Server {
            fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
                if self.fails(cmd) {
                    let e = redis::RedisError::from((redis::ErrorKind::Io, "connection reset"));
                    return Box::pin(std::future::ready(Err(e)));
                }
                let v = self.reply(cmd);
                Box::pin(std::future::ready(Ok(v)))
            }

            fn req_packed_commands<'a>(
                &'a mut self,
                pipeline: &'a Pipeline,
                offset: usize,
                count: usize,
            ) -> RedisFuture<'a, Vec<Value>> {
                let all: Vec<Value> = pipeline.cmd_iter().map(|c| self.reply(c)).collect();
                Box::pin(std::future::ready(Ok(all
                    .into_iter()
                    .skip(offset)
                    .take(count)
                    .collect())))
            }

            fn get_db(&self) -> i64 {
                0
            }
        }
    }

    use fake::Fate;

    fn perms_all() -> PermissionReport {
        use crate::model::{PERM_INFO, PERM_SCAN, PERM_TYPE_TTL, PermCheck, PermStatus};
        PermissionReport {
            checks: [PERM_SCAN, PERM_TYPE_TTL, PERM_MEMORY, PERM_FREQ, PERM_INFO]
                .into_iter()
                .map(|label| PermCheck {
                    label,
                    status: PermStatus::Allowed,
                })
                .collect(),
        }
    }

    fn opts(sample_limit: u64, batch_size: usize) -> ScanOptions {
        ScanOptions {
            sample_limit,
            batch_size,
            max_commands_per_sec: None,
            ..ScanOptions::default()
        }
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn est_total(r: &ScanReport) -> u64 {
        r.patterns.iter().map(|p| p.est_keys).sum()
    }

    /// `n` keys across three patterns, with `fate(i)` for key `i`.
    fn keyspace(n: usize, fate: impl Fn(usize) -> Fate) -> Vec<(Vec<u8>, Fate)> {
        (0..n)
            .map(|i| {
                let prefix = ["user", "cart", "sess"][i % 3];
                (format!("{prefix}:{i}").into_bytes(), fate(i))
            })
            .collect()
    }

    #[test]
    fn vanished_keys_do_not_shrink_the_estimate() {
        // 10,000 keys, 1,000 looked at, 30% of them gone by the time TYPE/PTTL ran.
        // Before: scale 10,000 / 1,000 = 10 over 700 samples, estimate 7,000.
        // After: scale 10,000 / 700 = 14.29, estimate 10,000.
        let keys = keyspace(10_000, |i| match i % 10 {
            0 | 1 => Fate::Gone,
            2 => Fate::Expired,
            _ => Fate::Present,
        });
        let mut server = fake::Server::new(keys, 100);
        let o = opts(1_000, 100);
        let outcome = run(sample_keys_detailed(&mut server, &o, true, false, None)).unwrap();
        assert_eq!(outcome.keys_seen, 1_000);
        assert_eq!(outcome.vanished, 300);
        assert_eq!(outcome.samples.len(), 700);
        assert!(!outcome.complete);

        let mut server = fake::Server::new(server.keys, 100);
        let report = run(scan("t", &mut server, &o, &perms_all(), None)).unwrap();
        assert_eq!(report.sampled_keys, 700);
        let est = est_total(&report);
        assert!(est.abs_diff(10_000) <= 2, "est {est}");
        let bytes: u64 = report.patterns.iter().filter_map(|p| p.est_bytes).sum();
        assert!(bytes.abs_diff(1_000_000) <= 200, "bytes {bytes}");
    }

    #[test]
    fn unreadable_keys_stay_in_the_denominator() {
        // The key ACL case: 10,000 keys `k:0`..`k:9999`, only `k:1*` (1,111) readable.
        // The first 1,000 keys hold 111 readable ones; they stand for 1,110 keys, not
        // for the whole database.
        let keys: Vec<(Vec<u8>, Fate)> = (0..10_000)
            .map(|i| {
                let k = format!("k:{i}");
                let fate = if k.starts_with("k:1") {
                    Fate::Present
                } else {
                    Fate::NoPerm
                };
                (k.into_bytes(), fate)
            })
            .collect();
        let mut server = fake::Server::new(keys, 100);
        let o = opts(1_000, 100);
        let outcome = run(sample_keys_detailed(&mut server, &o, true, false, None)).unwrap();
        assert_eq!((outcome.keys_seen, outcome.vanished), (1_000, 0));
        assert_eq!(outcome.samples.len(), 111);
        let mut server = fake::Server::new(server.keys, 100);
        let report = run(scan("t", &mut server, &o, &perms_all(), None)).unwrap();
        assert_eq!(est_total(&report), 1_110);
    }

    #[test]
    fn complete_walk_with_vanished_keys_reports_what_is_left() {
        // Every key was looked at: no scaling, the vanished keys are simply gone.
        let keys = keyspace(300, |i| {
            if i % 3 == 0 {
                Fate::Gone
            } else {
                Fate::Present
            }
        });
        let mut server = fake::Server::new(keys, 64);
        let report = run(scan("t", &mut server, &opts(1_000, 50), &perms_all(), None)).unwrap();
        assert!(report.complete);
        assert_eq!(report.sampled_keys, 200);
        assert_eq!(est_total(&report), 200);
    }

    #[test]
    fn every_key_vanished() {
        let keys = keyspace(1_000, |_| Fate::Gone);
        let mut server = fake::Server::new(keys, 100);
        let report = run(scan("t", &mut server, &opts(100, 50), &perms_all(), None)).unwrap();
        assert_eq!(report.sampled_keys, 0);
        assert!(report.patterns.is_empty());
    }

    #[test]
    fn partial_results_are_published_without_extra_commands() {
        let keys = keyspace(1_000, |_| Fate::Present);
        let o = opts(500, 100);

        let mut plain = fake::Server::new(keys.clone(), 100);
        let plain_report = run(scan("t", &mut plain, &o, &perms_all(), None)).unwrap();

        let (tx, rx) = watch::channel(None);
        let mut server = fake::Server::new(keys, 100);
        server.observer = Some(rx.clone());
        let report = run(scan_with_partial(
            "t",
            &mut server,
            &o,
            &perms_all(),
            None,
            Some(&tx),
        ))
        .unwrap();

        assert_eq!(server.log, plain.log, "partial results sent extra commands");
        assert_eq!(report.patterns, plain_report.patterns);
        // The first batch (100 keys) is published at once, scaled by 1,000 / 100.
        assert_eq!(server.seen_partials[0], None);
        assert_eq!(server.seen_partials[1], Some((100, 1_000)));
        // The last update holds the final patterns.
        let last = rx.borrow().clone().unwrap();
        assert_eq!(last.patterns, report.patterns);
        assert_eq!(last.sampled, 500);
        assert_eq!(last.target, 500);
        assert!(last.sizes_available);
    }

    #[test]
    fn unreadable_and_non_utf8_keys_count_as_present() {
        // 10,000 keys. The first 1,000 (one SCAN page) are looked at: 500 readable, 200
        // NOPERM on TYPE, 100 NOPERM on PTTL only, 100 not UTF-8, 100 gone. 900 still
        // exist, so the scale is 10,000 / 900.
        let mut keys = keyspace(900, |i| match i % 9 {
            0 | 1 => Fate::NoPerm,
            2 => Fate::PttlDenied,
            3 => Fate::Gone,
            _ => Fate::Present,
        });
        for i in 0..100u8 {
            keys.insert(usize::from(i) * 9, (vec![0xff, b':', i], Fate::Present));
        }
        keys.extend((0..9_000).map(|i| (format!("extra:{i}").into_bytes(), Fate::Present)));
        // The sample limit counts UTF-8 keys: 900 of them end the first page.
        let o = opts(900, 100);
        let mut server = fake::Server::new(keys, 1_000);
        let outcome = run(sample_keys_detailed(&mut server, &o, true, false, None)).unwrap();
        assert!(!outcome.complete);
        assert_eq!(outcome.keys_seen, 1_000);
        assert_eq!(outcome.vanished, 100);
        assert_eq!(outcome.keys_present(), 900);
        assert_eq!(outcome.samples.len(), 500);

        let mut server = fake::Server::new(server.keys, 1_000);
        let report = run(scan("t", &mut server, &o, &perms_all(), None)).unwrap();
        // 500 * 10,000 / 900 = 5,555.6, rounded per pattern.
        let est = est_total(&report);
        assert!(est.abs_diff(5_556) <= 2, "est {est}");
    }

    #[test]
    fn match_reports_the_sample_as_it_is() {
        // DBSIZE counts every key, not the matching ones: no scaling with MATCH, even
        // with vanished keys. (The fake ignores the glob.)
        let keys = keyspace(1_000, |i| {
            if i % 4 == 0 {
                Fate::Gone
            } else {
                Fate::Present
            }
        });
        let mut server = fake::Server::new(keys, 100);
        let o = ScanOptions {
            match_glob: Some("*".into()),
            ..opts(200, 50)
        };
        let report = run(scan("t", &mut server, &o, &perms_all(), None)).unwrap();
        assert!(!report.complete);
        assert_eq!(report.sampled_keys, 150);
        assert_eq!(est_total(&report), 150);
    }

    #[test]
    fn dbsize_below_the_keys_looked_up_clamps_to_one() {
        // Keys were added after DBSIZE: the estimate never goes below the sample.
        let keys = keyspace(1_000, |_| Fate::Present);
        let mut server = fake::Server::new(keys, 100);
        server.dbsize = 100;
        let report = run(scan("t", &mut server, &opts(500, 100), &perms_all(), None)).unwrap();
        assert!(!report.complete);
        assert_eq!(report.sampled_keys, 500);
        assert_eq!(est_total(&report), 500);
    }

    #[test]
    fn empty_database_samples_nothing() {
        let mut server = fake::Server::new(Vec::new(), 100);
        let (tx, rx) = watch::channel(None);
        let report = run(scan_with_partial(
            "t",
            &mut server,
            &opts(100, 50),
            &perms_all(),
            None,
            Some(&tx),
        ))
        .unwrap();
        assert!(report.complete);
        assert_eq!(report.sampled_keys, 0);
        assert!(report.patterns.is_empty());
        // The final update is still published, empty.
        let last = rx.borrow().clone().unwrap();
        assert!(last.patterns.is_empty());
        assert_eq!((last.sampled, last.target), (0, 0));
    }

    #[test]
    fn partial_sink_skips_empty_samples_and_waits_for_the_throttle() {
        let (tx, rx) = watch::channel(None);
        let mut sink = PartialSink {
            tx: &tx,
            total_keys: 1_000,
            target: 500,
            has_match: false,
            sizes_available: false,
            throttle: Throttle::default(),
        };
        let t0 = Instant::now();
        sink.maybe_publish(&[], 0, t0);
        assert!(rx.borrow().is_none(), "nothing sampled: nothing published");

        let samples: Vec<KeySample> = (0..10).map(|i| sample(&format!("k:{i}"), None)).collect();
        sink.maybe_publish(&samples, 20, t0);
        let first = rx.borrow().clone().unwrap();
        assert_eq!((first.sampled, first.target), (10, 500));
        assert!(!first.sizes_available);
        // 10 samples, 20 keys present of 1,000: scale 50.
        assert_eq!(first.patterns[0].est_keys, 500);

        // Too soon: the previous update stays.
        sink.maybe_publish(&samples[..5], 5, t0 + Duration::from_millis(100));
        assert_eq!(rx.borrow().as_ref().unwrap().sampled, 10);
        // Later: published.
        sink.maybe_publish(&samples[..5], 5, t0 + Duration::from_secs(10));
        assert_eq!(rx.borrow().as_ref().unwrap().sampled, 5);
    }

    #[test]
    fn failed_scan_returns_the_error_after_partials() {
        // The third SCAN fails: two batches were sampled and one partial published.
        let keys = keyspace(1_000, |_| Fate::Present);
        let mut server = fake::Server::new(keys, 100);
        server.fail_scan = Some(2);
        let (tx, rx) = watch::channel(None);
        let err = run(scan_with_partial(
            "t",
            &mut server,
            &opts(500, 100),
            &perms_all(),
            None,
            Some(&tx),
        ))
        .unwrap_err();
        assert!(matches!(err, ScanError::Redis(_)), "{err}");
        // The last partial stays in the channel; the caller (the TUI) drops it.
        assert!(rx.borrow().as_ref().unwrap().sampled < 500);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// However many SCAN-returned keys vanish before they are looked up, the
        /// estimated total of an incomplete scan is DBSIZE, within rounding (each of
        /// the three patterns rounds by at most 0.5).
        #[test]
        fn vanishing_keys_keep_the_total(
            n in 50usize..3_000,
            limit_pct in 1u64..95,
            vanish_pct in 0usize..95,
            seed in 0usize..1_000,
            page in 1usize..300,
            batch in 1usize..200,
        ) {
            let fate = |i: usize| {
                let h = (i.wrapping_mul(2_654_435_761).wrapping_add(seed)) % 100;
                match (h < vanish_pct, h % 2) {
                    (false, _) => Fate::Present,
                    (true, 0) => Fate::Gone,
                    (true, _) => Fate::Expired,
                }
            };
            let limit = (n as u64 * limit_pct / 100).max(1);
            let mut server = fake::Server::new(keyspace(n, fate), page);
            let report = run(scan("t", &mut server, &opts(limit, batch), &perms_all(), None))
                .unwrap();
            proptest::prop_assert!(!report.complete);
            let est = est_total(&report);
            if report.sampled_keys == 0 {
                proptest::prop_assert_eq!(est, 0);
            } else {
                proptest::prop_assert!(est.abs_diff(n as u64) <= 1, "est {} of {}", est, n);
            }
        }
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
