//! Sampling a live server and building a [`ScanReport`].
//!
//! Owner: scanner.

pub mod cluster;
pub mod context;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::budget::Budget;

use redis::aio::ConnectionLike;
use redis::{FromRedisValue, Value};
use tokio::sync::watch;

use crate::model::{
    KeySample, KeySizes, KeyType, PERM_FREQ, PERM_MEMORY, PatternStats, PermissionReport,
    ScanReport, ServerFlavor, ServerInfo, ServerMode, Ttl,
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
    /// The database the connection has selected, for its `INFO keyspace` and
    /// `INFO keysizes` lines.
    pub db: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sample_limit: 100_000,
            scan_count: 1_000,
            batch_size: 500,
            max_commands_per_sec: Some(5_000),
            match_glob: None,
            db: 0,
        }
    }
}

impl ScanOptions {
    /// Defaults for a profile: 1,000 commands per second on ElastiCache Serverless
    /// ([`crate::conn::provider::SERVERLESS_MAX_RATE`]), no command budget for `Local`
    /// profiles, 5,000 commands per second for everything else. The profile's database.
    /// An explicit `--max-rate` replaces the budget afterwards.
    pub fn for_profile(profile: &crate::model::Profile) -> Self {
        let max_commands_per_sec =
            if profile.provider == Some(crate::model::Provider::ElasticacheServerless) {
                Some(crate::conn::provider::SERVERLESS_MAX_RATE)
            } else if profile.env == crate::model::Env::Local {
                None
            } else {
                Some(5_000)
            };
        Self {
            max_commands_per_sec,
            db: profile.db,
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
    /// A cluster scan where no node could be scanned at all; each node's note says why.
    #[error("no node of the cluster could be scanned")]
    NoNodes,
}

/// Read `INFO server`, `INFO memory` and `DBSIZE`, with no command budget.
pub async fn server_info<C: ConnectionLike + Send>(conn: &mut C) -> Result<ServerInfo, ScanError> {
    server_info_with_budget(conn, &Budget::new(None)).await
}

/// [`server_info`], taking its three commands from `budget`.
pub async fn server_info_with_budget<C: ConnectionLike + Send>(
    conn: &mut C,
    budget: &Budget,
) -> Result<ServerInfo, ScanError> {
    budget.take(1).await;
    let server: String = redis::cmd("INFO").arg("server").query_async(conn).await?;
    budget.take(1).await;
    let memory: String = redis::cmd("INFO").arg("memory").query_async(conn).await?;
    budget.take(1).await;
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
    let budget = Budget::new(opts.max_commands_per_sec);
    sample_keys_detailed_with_budget(conn, opts, with_sizes, with_freq, progress, &budget).await
}

/// [`sample_keys_detailed`], taking every command from `budget` instead of a budget of
/// its own built from `opts.max_commands_per_sec` (which is then not used).
pub async fn sample_keys_detailed_with_budget<C: ConnectionLike + Send>(
    conn: &mut C,
    opts: &ScanOptions,
    with_sizes: bool,
    with_freq: bool,
    progress: Option<&watch::Sender<ScanProgress>>,
    budget: &Budget,
) -> Result<SampleOutcome, ScanError> {
    let publish = progress.map(watch_progress);
    let (outcome, failure) = sample_inner(
        conn,
        opts,
        with_sizes,
        with_freq,
        publish.as_ref().map(|f| f as &ProgressFn<'_>),
        None,
        budget,
    )
    .await;
    match failure {
        Some(e) => Err(e),
        None => Ok(outcome),
    }
}

/// Reports `(keys sampled, done)` while sampling.
pub(crate) type ProgressFn<'a> = dyn Fn(u64, bool) + Sync + 'a;

/// Progress for one server on `tx`: sets `sampled` and `done`, and keeps whatever
/// target the caller set, including 0 for an empty database.
fn watch_progress(tx: &watch::Sender<ScanProgress>) -> impl Fn(u64, bool) + Sync + '_ {
    move |sampled, done| {
        tx.send_modify(|p| {
            p.sampled = sampled;
            p.done = done;
        });
    }
}

/// Called after each batch with the samples so far and the keys looked up so far that
/// still existed (the estimator's denominator). Sends nothing to the server.
pub(crate) trait PartialHook: Send {
    fn after_batch(&mut self, samples: &[KeySample], keys_present: u64, now: Instant);
}

/// [`sample_keys_detailed`], also calling `partial` after each batch (to publish partial
/// pattern stats, throttled). Aggregating sends nothing to the server.
///
/// A failed `SCAN` or batch ends the walk: the outcome then holds what was sampled
/// before it, with `keys_seen` counting only the keys looked up (or skipped as not
/// UTF-8), `complete` false, and no final `done` progress; the error comes second.
async fn sample_inner<C: ConnectionLike + Send>(
    conn: &mut C,
    opts: &ScanOptions,
    with_sizes: bool,
    with_freq: bool,
    progress: Option<&ProgressFn<'_>>,
    mut partial: Option<&mut (dyn PartialHook + '_)>,
    budget: &Budget,
) -> (SampleOutcome, Option<ScanError>) {
    let limit = usize::try_from(opts.sample_limit).unwrap_or(usize::MAX);
    let batch_size = opts.batch_size.max(1);
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
    // Keys sent TYPE/PTTL in a batch that answered.
    let mut answered: u64 = 0;
    let mut cursor: u64 = 0;

    let publish = |sampled: usize, done: bool| {
        if let Some(f) = progress {
            f(sampled as u64, done);
        }
    };
    let stopped = |samples: Vec<KeySample>, answered: u64, skipped: usize, vanished, e| {
        let outcome = SampleOutcome {
            samples,
            keys_seen: answered + skipped as u64,
            vanished,
            complete: false,
        };
        (outcome, Some(e))
    };

    while seen.len() < limit {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor).arg("COUNT").arg(opts.scan_count);
        if let Some(glob) = &opts.match_glob {
            cmd.arg("MATCH").arg(glob);
        }
        budget.take(1).await;
        let (next, keys): (u64, Vec<Vec<u8>>) = match cmd.query_async(conn).await {
            Ok(r) => r,
            Err(e) => return stopped(samples, answered, skipped.len(), vanished, e.into()),
        };
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
            let batch_out = match fetch_batch(
                conn,
                &batch,
                with_sizes,
                with_freq,
                &mut samples,
                budget,
            )
            .await
            {
                Ok(b) => b,
                Err(e) => return stopped(samples, answered, skipped.len(), vanished, e),
            };
            vanished += batch_out.vanished;
            answered += batch.len() as u64;
            if let Some(sink) = partial.as_deref_mut() {
                // Keys looked up so far that still existed; `pending` hasn't been looked up.
                let looked_up = (seen.len() - pending.len() + skipped.len()) as u64;
                let present = looked_up.saturating_sub(vanished);
                sink.after_batch(&samples, present, Instant::now());
            }
            publish(samples.len(), false);
        }

        if cursor == 0 {
            break;
        }
    }
    publish(samples.len(), true);
    let outcome = SampleOutcome {
        samples,
        keys_seen: (seen.len() + skipped.len()) as u64,
        vanished,
        complete: reached_end && !dropped,
    };
    (outcome, None)
}

/// What [`fetch_batch`] did.
struct BatchOutcome {
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
    budget: &Budget,
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
    // The whole pipeline counts: one command per key and per lookup.
    budget
        .take(u32::try_from(keys.len() * stride).unwrap_or(u32::MAX))
        .await;
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
            node: None,
        });
    }
    Ok(BatchOutcome { vanished })
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

impl PartialHook for PartialSink<'_> {
    fn after_batch(&mut self, samples: &[KeySample], keys_present: u64, now: Instant) {
        self.maybe_publish(samples, keys_present, now);
    }
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

/// Run a full scan: server info, the server context (see [`context`]), sampling, then
/// [`crate::pattern::aggregate`].
///
/// Uses `perms` to decide whether to ask for sizes, LFU counters and `MEMORY STATS`. The
/// same as [`scan_with_partial`] without partial results.
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
    let budget = Budget::new(opts.max_commands_per_sec);
    scan_with_budget(profile_name, conn, opts, perms, progress, partial, &budget).await
}

/// [`scan_with_partial`], taking every command (server info, the server context and the
/// sampling) from `budget` instead of a budget of its own built from
/// `opts.max_commands_per_sec` (which is then not used). The TUI passes the budget its
/// live views share.
pub async fn scan_with_budget<C: ConnectionLike + Send>(
    profile_name: &str,
    conn: &mut C,
    opts: &ScanOptions,
    perms: &PermissionReport,
    progress: Option<&watch::Sender<ScanProgress>>,
    partial: Option<&PartialSender>,
    budget: &Budget,
) -> Result<ScanReport, ScanError> {
    let server = server_info_with_budget(conn, budget).await?;
    let with_sizes = perms.allowed(PERM_MEMORY);
    let target = opts.sample_limit.min(server.total_keys);
    let has_match = opts.match_glob.is_some();

    let mut sink = partial.map(|tx| PartialSink {
        tx,
        total_keys: server.total_keys,
        target,
        has_match,
        sizes_available: with_sizes,
        throttle: Throttle::default(),
    });
    let publish = progress.map(watch_progress);
    let begin = || {
        if let Some(tx) = progress {
            tx.send_replace(ScanProgress {
                sampled: 0,
                target,
                done: false,
            });
        }
    };
    let node = sample_server(
        conn,
        &server,
        opts,
        perms,
        &begin,
        publish.as_ref().map(|f| f as &ProgressFn<'_>),
        sink.as_mut().map(|s| s as &mut dyn PartialHook),
        budget,
    )
    .await;
    if let Some(e) = node.error {
        return Err(e);
    }
    let scale = node.scale(server.total_keys, has_match);
    let server_context = node.context;
    let outcome = node.outcome;
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
        server_context: Some(server_context),
        key_sizes: with_sizes.then(|| {
            Box::new(KeySizes {
                big_keys: big_keys(&samples),
                ttl_bytes: ttl_bytes(&samples),
            })
        }),
        nodes: None,
    })
}

/// How many big keys a report keeps.
pub const BIG_KEY_LIMIT: usize = 50;

/// The [`BIG_KEY_LIMIT`] sampled keys with the largest `MEMORY USAGE`: largest first,
/// ties by key name. Keys without a size are left out. Keeps a heap of at most
/// [`BIG_KEY_LIMIT`] + 1 entries, so the cost stays O(n log 50) and the memory bounded.
pub fn big_keys(samples: &[KeySample]) -> Vec<KeySample> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    // The rank: more bytes first, then the smaller name. `Reverse` makes the heap a
    // min-heap of ranks, so popping drops the smallest rank kept so far.
    let mut heap: BinaryHeap<Reverse<(u64, Reverse<&str>, usize)>> =
        BinaryHeap::with_capacity(BIG_KEY_LIMIT + 1);
    for (i, s) in samples.iter().enumerate() {
        let Some(bytes) = s.bytes else { continue };
        heap.push(Reverse((bytes, Reverse(s.key.as_str()), i)));
        if heap.len() > BIG_KEY_LIMIT {
            heap.pop();
        }
    }
    // Ascending `Reverse(rank)` is descending rank: largest first.
    heap.into_sorted_vec()
        .into_iter()
        .map(|Reverse((_, _, i))| samples[i].clone())
        .collect()
}

/// Sum of `MEMORY USAGE` per TTL bucket over the samples that have a size.
pub fn ttl_bytes(samples: &[KeySample]) -> [u64; 7] {
    let mut out = [0u64; 7];
    for s in samples {
        if let Some(b) = s.bytes {
            let slot = &mut out[s.ttl.bucket()];
            *slot = slot.saturating_add(b);
        }
    }
    out
}

/// What sampling one server gave: [`scan_with_partial`] and every node of a cluster scan
/// ([`cluster::scan_cluster`]) go through [`sample_server`].
pub(crate) struct ServerSample {
    /// The server context (see [`context`]).
    pub context: crate::model::ServerContext,
    /// What was sampled; on `error`, what was sampled before it.
    pub outcome: SampleOutcome,
    /// `MEMORY USAGE` was sent.
    pub with_sizes: bool,
    /// The walk stopped early on this error.
    pub error: Option<ScanError>,
}

impl ServerSample {
    /// [`estimate_scale`] for this server's outcome and `DBSIZE`.
    pub fn scale(&self, total_keys: u64, has_match: bool) -> f64 {
        estimate_scale(
            total_keys,
            self.outcome.keys_present(),
            self.outcome.complete,
            has_match,
        )
    }
}

/// Read the server context, then sample, on one server whose `INFO` and `DBSIZE` are
/// `server`, taking every command from `budget`. A cluster scan gives each node a
/// [`Budget`] of its own at the full `opts.max_commands_per_sec`: the command budget is
/// per server. `perms` decides whether sizes, LFU counters and
/// `MEMORY STATS` are asked for. `begin` runs after the context is read and before the
/// first `SCAN`. Never fails as a whole: an error mid-walk is kept in
/// [`ServerSample::error`] with what was sampled before it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sample_server<C: ConnectionLike + Send>(
    conn: &mut C,
    server: &ServerInfo,
    opts: &ScanOptions,
    perms: &PermissionReport,
    begin: &(dyn Fn() + Sync),
    progress: Option<&ProgressFn<'_>>,
    partial: Option<&mut (dyn PartialHook + '_)>,
    budget: &Budget,
) -> ServerSample {
    // A few read-only commands; any part that fails is left out, never the scan.
    let context = context::read(conn, opts.db, perms, budget).await;
    let with_sizes = perms.allowed(PERM_MEMORY);
    let with_freq = perms.allowed(PERM_FREQ) && server.lfu_enabled();
    begin();
    let (outcome, error) =
        sample_inner(conn, opts, with_sizes, with_freq, progress, partial, budget).await;
    ServerSample {
        context,
        outcome,
        with_sizes,
        error,
    }
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
            node: None,
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

    fn sized(key: &str, bytes: Option<u64>, ttl: Ttl) -> KeySample {
        KeySample {
            bytes,
            ttl,
            ..sample(key, None)
        }
    }

    #[test]
    fn big_keys_keep_the_fifty_largest_largest_first_ties_by_name() {
        // 120 keys, sizes 0..120 shuffled, plus ties at the cut and keys without a size.
        let mut samples: Vec<KeySample> = (0..120u64)
            .map(|i| (i * 37) % 120)
            .map(|b| sized(&format!("k{b:03}"), Some(b * 10), Ttl::Persistent))
            .collect();
        samples.push(sized("tie-b", Some(710), Ttl::Persistent));
        samples.push(sized("tie-a", Some(710), Ttl::Persistent));
        samples.push(sized("no-size", None, Ttl::Persistent));
        let big = big_keys(&samples);
        assert_eq!(big.len(), BIG_KEY_LIMIT);
        assert_eq!(big[0].key, "k119");
        assert_eq!(big[0].bytes, Some(1190));
        // Largest first, and equal sizes by name.
        for w in big.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            assert!(
                a.bytes > b.bytes || (a.bytes == b.bytes && a.key < b.key),
                "{} {:?} before {} {:?}",
                a.key,
                a.bytes,
                b.key,
                b.bytes
            );
        }
        // k119..k072 are the first 48. Three keys share 710 (k071, tie-a, tie-b): by
        // name, k071 and tie-a fill the last two places and tie-b is cut.
        let tail: Vec<&str> = big[46..].iter().map(|s| s.key.as_str()).collect();
        assert_eq!(tail, ["k073", "k072", "k071", "tie-a"]);
        assert!(!big.iter().any(|s| s.key == "no-size"));
        // The same with the input in another order.
        samples.reverse();
        assert_eq!(big_keys(&samples), big);
    }

    #[test]
    fn big_keys_with_fewer_than_fifty_or_no_sizes() {
        let samples = vec![
            sized("a", Some(5), Ttl::Persistent),
            sized("b", Some(9), Ttl::Persistent),
            sized("c", None, Ttl::Persistent),
        ];
        let keys: Vec<_> = big_keys(&samples).into_iter().map(|s| s.key).collect();
        assert_eq!(keys, ["b", "a"]);
        assert!(big_keys(&[sized("c", None, Ttl::Persistent)]).is_empty());
        assert!(big_keys(&[]).is_empty());
    }

    #[test]
    fn ttl_bytes_sum_per_bucket() {
        let secs = |s: u64| Ttl::Expires(Duration::from_secs(s));
        let samples = vec![
            sized("none-1", Some(100), Ttl::Persistent),
            sized("none-2", Some(50), Ttl::Persistent),
            sized("min", Some(7), secs(59)),
            // A bucket starts at its bound: 60 s is <1h, not <1m.
            sized("hour", Some(11), secs(60)),
            sized("day", Some(13), secs(3_600)),
            sized("week", Some(17), secs(86_400)),
            sized("month", Some(19), secs(7 * 86_400)),
            sized("later", Some(23), secs(30 * 86_400)),
            sized("unsized", None, secs(10)),
        ];
        assert_eq!(ttl_bytes(&samples), [150, 7, 11, 13, 17, 19, 23]);
        assert_eq!(ttl_bytes(&[]), [0; 7]);
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
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
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
        assert_eq!(ScanOptions::for_profile(&p).db, 0);
        p.db = 4;
        assert_eq!(ScanOptions::for_profile(&p).db, 4);
    }

    #[test]
    fn serverless_gets_a_lower_default_rate() {
        use crate::model::{Auth, Endpoint, Env, Profile, Provider};
        let mut p = Profile {
            name: "p".into(),
            env: Env::Prod,
            endpoint: Endpoint {
                host: "c-1a2b3c.serverless.use1.cache.amazonaws.com".into(),
                port: 6379,
                tls: true,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
            prefer_replica: false,
            provider: Some(Provider::ElasticacheServerless),
            token_command: None,
            ssh: None,
            sentinel: None,
        };
        for env in [Env::Prod, Env::Stage, Env::Other, Env::Local] {
            p.env = env;
            assert_eq!(
                ScanOptions::for_profile(&p).max_commands_per_sec,
                Some(1_000),
                "{env:?}"
            );
        }
        // Other providers keep the usual budget.
        p.env = Env::Prod;
        for other in [Provider::Elasticache, Provider::Memorydb, Provider::Azure] {
            p.provider = Some(other);
            assert_eq!(
                ScanOptions::for_profile(&p).max_commands_per_sec,
                Some(5_000),
                "{other:?}"
            );
        }
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
    pub(super) mod fake {
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

        fn type_fate(server: &mut Server, key: &[u8]) -> Option<Fate> {
            *server.type_calls.entry(key.to_vec()).or_default() += 1;
            server.fates.get(key).copied()
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
            /// Make SCAN repeat keys: the first key of each page twice within the page,
            /// and the last key of the previous page again at the end of each page.
            pub duplicates: bool,
            /// How many times each key was sent `TYPE`.
            pub type_calls: HashMap<Vec<u8>, usize>,
            /// Refuse `INFO server` (for a node that fails before sampling).
            pub fail_info: bool,
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
                    duplicates: false,
                    type_calls: HashMap::new(),
                    fail_info: false,
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
                    "INFO" if text("server") && self.fail_info => {
                        redis::parse_redis_value(b"-ERR unavailable\r\n").unwrap()
                    }
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
                        let mut page: Vec<&Vec<u8>> =
                            self.keys[start..end].iter().map(|(k, _)| k).collect();
                        if self.duplicates && !page.is_empty() {
                            page.push(page[0]);
                            if start > 0 {
                                page.push(&self.keys[start - 1].0);
                            }
                        }
                        let keys = page
                            .into_iter()
                            .map(|k| Value::BulkString(k.clone()))
                            .collect();
                        Value::Array(vec![
                            Value::BulkString(next.to_string().into_bytes()),
                            Value::Array(keys),
                        ])
                    }
                    "TYPE" => match type_fate(self, &args[1]) {
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
                    "MEMORY" if text("STATS") => Value::Array(vec![
                        Value::BulkString(b"dataset.bytes".to_vec()),
                        Value::Int(self.dbsize as i64 * 100),
                        Value::BulkString(b"fragmentation".to_vec()),
                        Value::BulkString(b"1.5".to_vec()),
                    ]),
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

    /// A runtime with timers, for the tests that really sleep.
    fn run_timed<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// A runtime whose clock only moves when every task waits on a timer.
    fn run_paused<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(f)
    }

    /// Every command a scan sends (server info, the server context, `SCAN` and each
    /// pipelined lookup) is taken from the budget, and the budget sets the pace: on a
    /// paused clock, where commands cost nothing, the scan takes commands over rate.
    #[test]
    fn budget_wait_keeps_the_average_rate_within_budget() {
        for rate in [100u32, 5_000, 7] {
            let mut server = fake::Server::new(keyspace(300, |_| Fate::Present), 40);
            let budget = crate::budget::Budget::new(Some(rate));
            let (sent, took) = run_paused(async {
                let t0 = tokio::time::Instant::now();
                let o = opts(1_000, 20);
                scan_with_budget("t", &mut server, &o, &perms_all(), None, None, &budget)
                    .await
                    .unwrap();
                (server.log.len() as u64, t0.elapsed())
            });
            assert_eq!(budget.taken(), sent, "{:?}", server.log);
            let min = Duration::from_secs_f64(sent as f64 / f64::from(rate));
            assert!(took >= min, "rate {rate}: {took:?} < {min:?}");
            // And it does not wait far longer than needed.
            assert!(took <= min + Duration::from_millis(1), "{took:?}");
        }
    }

    /// The server-context commands (`INFO keyspace`, `INFO keysizes`, `MEMORY STATS`) each
    /// count against the scan's command budget, so `--max-rate` covers them too.
    #[test]
    fn context_commands_count_against_the_budget() {
        let mut server = fake::Server::new(keyspace(10, |_| Fate::Present), 10);
        let b = crate::budget::Budget::new(Some(1_000_000));
        run_timed(context::read(&mut server, 0, &perms_all(), &b));
        assert_eq!(server.log.len(), 3, "{:?}", server.log);
        assert_eq!(b.taken(), server.log.len() as u64);
        // A denied MEMORY STATS is not sent and not counted.
        let mut server = fake::Server::new(keyspace(10, |_| Fate::Present), 10);
        let b = crate::budget::Budget::new(Some(1_000_000));
        let denied = perms_with(
            crate::model::PERM_MEMORY_STATS,
            crate::model::PermStatus::Denied("NOPERM".into()),
        );
        run_timed(context::read(&mut server, 0, &denied, &b));
        assert_eq!(b.taken(), 2);
        assert_eq!(server.log.len(), 2, "{:?}", server.log);
    }

    #[test]
    fn budget_without_a_limit_never_waits() {
        for rate in [None, Some(0)] {
            let mut server = fake::Server::new(keyspace(300, |_| Fate::Present), 40);
            let mut o = opts(1_000, 20);
            o.max_commands_per_sec = rate;
            let took = run_paused(async {
                let t0 = tokio::time::Instant::now();
                scan("t", &mut server, &o, &perms_all(), None)
                    .await
                    .unwrap();
                t0.elapsed()
            });
            assert_eq!(took, Duration::ZERO, "{rate:?}");
            assert!(server.log.len() > 12, "{:?}", server.log);
        }
    }

    #[test]
    fn scan_with_a_rate_takes_at_least_commands_over_rate() {
        let keys = keyspace(120, |_| Fate::Present);
        let mut server = fake::Server::new(keys, 40);
        let mut o = opts(1_000, 20);
        o.max_commands_per_sec = Some(200);
        let t0 = Instant::now();
        run_timed(sample_keys_detailed(&mut server, &o, true, false, None)).unwrap();
        let took = t0.elapsed();
        let sent = server.log.len() as u64;
        assert!(sent > 12, "too few commands to mean anything: {sent}");
        let min = Duration::from_secs_f64(sent as f64 / 200.0);
        assert!(took >= min, "{sent} commands in {took:?}, budget {min:?}");
        assert!(!server.log.iter().any(|c| c == "KEYS"));
    }

    #[test]
    fn repeated_scan_keys_are_looked_up_once() {
        let keys = keyspace(300, |_| Fate::Present);
        let mut server = fake::Server::new(keys.clone(), 50);
        server.duplicates = true;
        let o = opts(1_000, 25);
        let outcome = run(sample_keys_detailed(&mut server, &o, true, false, None)).unwrap();
        assert!(outcome.complete);
        assert_eq!(outcome.keys_seen, 300);
        assert_eq!(outcome.samples.len(), 300);
        assert_eq!(outcome.keys_present(), 300);
        assert_eq!(server.type_calls.len(), 300);
        assert!(server.type_calls.values().all(|n| *n == 1));
        let mut got: Vec<_> = outcome.samples.iter().map(|s| s.key.clone()).collect();
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 300);

        // Counts are not inflated.
        let mut server = fake::Server::new(keys, 50);
        server.duplicates = true;
        let report = run(scan("t", &mut server, &o, &perms_all(), None)).unwrap();
        assert_eq!(report.sampled_keys, 300);
        assert_eq!(est_total(&report), 300);
    }

    #[test]
    fn repeated_scan_keys_do_not_skew_the_scale_estimate() {
        // 1,000 keys, 100 distinct keys sampled: the denominator is 100, not the
        // number of keys SCAN returned, so the estimate is the whole keyspace.
        let keys = keyspace(1_000, |_| Fate::Present);
        let mut server = fake::Server::new(keys, 30);
        server.duplicates = true;
        let o = opts(100, 10);
        let outcome = run(sample_keys_detailed(&mut server, &o, true, false, None)).unwrap();
        assert_eq!(outcome.keys_seen, 100);
        assert_eq!(outcome.samples.len(), 100);
        assert!(server.type_calls.values().all(|n| *n == 1));

        let mut server = fake::Server::new(server.keys, 30);
        server.duplicates = true;
        let report = run(scan("t", &mut server, &o, &perms_all(), None)).unwrap();
        assert_eq!(report.sampled_keys, 100);
        let est = est_total(&report);
        assert!(est.abs_diff(1_000) <= 2, "est {est}");
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

    fn perms_with(label: &'static str, status: crate::model::PermStatus) -> PermissionReport {
        let mut p = perms_all();
        p.checks.push(crate::model::PermCheck {
            label: crate::model::PERM_MEMORY_STATS,
            status: crate::model::PermStatus::Allowed,
        });
        for c in &mut p.checks {
            if c.label == label {
                c.status = status.clone();
            }
        }
        p
    }

    #[test]
    fn the_server_context_is_read_once_per_scan() {
        use crate::model::{ContextGap, PERM_MEMORY_STATS, PermStatus};
        let keys = keyspace(200, |_| Fate::Present);
        let mut server = fake::Server::new(keys, 100);
        let perms = perms_with(PERM_MEMORY, PermStatus::Denied("NOPERM".into()));
        let report = run(scan("t", &mut server, &opts(1_000, 50), &perms, None)).unwrap();
        let ctx = report.server_context.expect("a context");
        // INFO server, INFO memory, then INFO keyspace and INFO keysizes; one MEMORY
        // (STATS: MEMORY USAGE is denied here). Nothing else is new, nothing writes.
        let count = |name: &str| server.log.iter().filter(|c| *c == name).count();
        assert_eq!(count("INFO"), 4, "{:?}", server.log);
        assert_eq!(count("MEMORY"), 1, "{:?}", server.log);
        assert_eq!(ctx.memory.and_then(|m| m.dataset_bytes), Some(20_000));
        assert_eq!(ctx.memory.and_then(|m| m.fragmentation), Some(1.5));
        // The fake has no keysizes section and no db0 line.
        assert_eq!(ctx.keysizes, None);
        assert_eq!(ctx.keyspace.map(|k| k.keys), Some(0));
        assert_eq!(ctx.missing, vec![ContextGap::NoKeysizes]);

        // Denied by the permission check: not sent, and the reason is kept.
        for (status, gap) in [
            (
                PermStatus::Denied("NOPERM MEMORY STATS is not allowed for this user".into()),
                ContextGap::MemoryStatsDenied,
            ),
            (
                PermStatus::Denied("ERR unknown command 'MEMORY'".into()),
                ContextGap::MemoryStatsUnavailable,
            ),
        ] {
            let mut server = fake::Server::new(keyspace(10, |_| Fate::Present), 100);
            let mut perms = perms_with(PERM_MEMORY_STATS, status);
            perms.checks.retain(|c| c.label != PERM_MEMORY);
            let report = run(scan("t", &mut server, &opts(1_000, 50), &perms, None)).unwrap();
            assert!(
                !server.log.iter().any(|c| c == "MEMORY"),
                "{:?}",
                server.log
            );
            let ctx = report.server_context.unwrap();
            assert_eq!(ctx.memory, None);
            assert_eq!(ctx.missing, vec![ContextGap::NoKeysizes, gap]);
            // The snapshot JSON holds the fixed reason, never the server's text (which
            // names the user).
            let json = serde_json::to_string(&ctx).unwrap();
            assert!(
                !json.contains("lens-ro") && !json.contains("NOPERM"),
                "{json}"
            );
        }
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

    /// The routine every server's scan goes through (standalone, and each node of a
    /// cluster) keeps what it sampled before a failure, and counts only the keys it
    /// looked up.
    #[test]
    fn a_failed_walk_keeps_what_it_sampled() {
        let keys = keyspace(1_000, |i| {
            if i % 10 == 0 {
                Fate::Gone
            } else {
                Fate::Present
            }
        });
        let mut server = fake::Server::new(keys, 100);
        server.fail_scan = Some(2);
        let budget = Budget::new(None);
        let (outcome, error) = run(sample_inner(
            &mut server,
            &opts(10_000, 50),
            true,
            false,
            None,
            None,
            &budget,
        ));
        assert!(matches!(error, Some(ScanError::Redis(_))));
        assert!(!outcome.complete);
        assert_eq!(outcome.keys_seen, 200);
        assert_eq!(outcome.vanished, 20);
        assert_eq!(outcome.samples.len(), 180);
        // The same through sample_server, with the context read first.
        let mut server = fake::Server::new(server.keys, 100);
        server.fail_scan = Some(2);
        let info = run(server_info(&mut server)).unwrap();
        let s = run(sample_server(
            &mut server,
            &info,
            &opts(10_000, 50),
            &perms_all(),
            &|| {},
            None,
            None,
            &Budget::new(None),
        ));
        assert!(s.error.is_some());
        assert_eq!(s.outcome.samples.len(), 180);
        assert!(s.with_sizes);
        assert!(s.context.keyspace.is_some());
    }

    #[test]
    fn partial_denominator_leaves_out_pending_and_vanished_keys() {
        // 10,000 UTF-8 keys + 10 non-UTF-8 keys in the first page. Page 1,000, batch 100.
        // First batch (UTF-8 keys 0..99): 20 Gone, 10 NoPerm, 70 Present.
        let mut keys = keyspace(10_000, |i| {
            if i < 100 {
                match i % 10 {
                    0 | 1 => Fate::Gone,
                    2 => Fate::NoPerm,
                    _ => Fate::Present,
                }
            } else {
                Fate::Present
            }
        });
        for j in 0..10u8 {
            keys.insert(500 + usize::from(j), (vec![0xff, b':', j], Fate::Present));
        }
        let mut server = fake::Server::new(keys, 1_000);
        let (tx, rx) = watch::channel(None);
        server.observer = Some(rx.clone());
        run(scan_with_partial(
            "t",
            &mut server,
            &opts(5_000, 100),
            &perms_all(),
            None,
            Some(&tx),
        ))
        .unwrap();
        // Present after batch 1 = (990 seen - 890 pending) + 10 non-UTF-8 - 20 vanished = 90,
        // so 70 samples stand for 70 * 10,010 / 90 = 7,785.6 keys.
        let (sampled, est) = server.seen_partials[1].unwrap();
        assert_eq!(sampled, 70);
        assert!(est.abs_diff(7_786) <= 2, "est {est}");
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
