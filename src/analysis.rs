//! The rules behind the Analysis view (Big keys, Expiry, Recommendations) and the
//! `big key` and `leak?` verdicts of the Patterns VERDICT column (spec "2 Analysis",
//! "1 Patterns, additions"), and the key list's glob and inference filter. Pure
//! functions over the scan report and what the live connection read; no IO.
//!
//! Recommendations use report data plus `OBJECT ENCODING` and the lengths read for the
//! big keys, never `CONFIG GET` (ElastiCache blocks it).

use std::collections::HashMap;

use crate::diff::ReportDiff;
use crate::format;
use crate::model::{KeyType, PatternStats, ScanReport};
use crate::pattern::{PLACEHOLDERS, is_literal, pattern_for_key, tokens};
use crate::verdict::{TtlVerdict, ttl_verdict};

/// A key of at least this size is a big key.
pub const BIG_KEY_BYTES: u64 = 64 * 1024 * 1024;
/// A collection with at least this many items is a big key.
pub const BIG_KEY_ITEMS: u64 = 1_000_000;
/// A pattern with at least this share of its checked hashes, sets or sorted sets past
/// their compact encoding gets a recommendation.
pub const PAST_COMPACT_SHARE: f64 = 0.05;
/// Memory that never expires gets a recommendation when its share is over this.
pub const NEVER_EXPIRES_SHARE: f64 = 0.25;
/// A pattern with one of these segments holds locks, which should always expire.
pub const LOCK_SEGMENTS: [&str; 4] = ["lock", "locks", "mutex", "lease"];

// ---------------------------------------------------------------------------
// Patterns and keys
// ---------------------------------------------------------------------------

/// True when `a` and `b` are the same pattern except where one has `{var}` and the
/// other a literal segment: what pattern inference merges once a segment has 20
/// distinct values (`tenant.a1:cfg` and `tenant.{var}:cfg`). Equal patterns match too.
pub fn same_up_to_var(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (ta, tb) = (tokens(a), tokens(b));
    ta.len() == tb.len()
        && ta.iter().zip(&tb).all(|(x, y)| {
            x == y || (*x == "{var}" && is_literal(y)) || (*y == "{var}" && is_literal(x))
        })
}

/// True when `key` infers to `pattern` the way the scan infers it: its own pattern
/// ([`pattern_for_key`]) is `pattern`, or is `pattern` with a literal where `pattern`
/// has `{var}`. A `SCAN MATCH` glob matches more than that (`user:*:cart` also matches
/// `user:abc:cart`), so the key list keeps only the keys this accepts.
pub fn key_in_pattern(key: &str, pattern: &str) -> bool {
    same_up_to_var(pattern, &pattern_for_key(key))
}

/// The `SCAN MATCH` glob for `pattern`: each placeholder becomes `*`, and `*`, `?`,
/// `[`, `]` and `\` in the literal parts are escaped with `\`, so they match only
/// themselves. `user:{id}:cart` is `user:*:cart`.
pub fn match_glob(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for t in tokens(pattern) {
        if PLACEHOLDERS.contains(&t) {
            out.push('*');
            continue;
        }
        for c in t.chars() {
            if matches!(c, '*' | '?' | '[' | ']' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

/// The report's pattern for `key`: the one it infers to ([`key_in_pattern`]), or its
/// own inferred pattern when the report has none.
pub fn pattern_of(report: &ScanReport, key: &str) -> String {
    let own = pattern_for_key(key);
    report
        .patterns
        .iter()
        .find(|p| same_up_to_var(&p.pattern, &own))
        .map_or(own, |p| p.pattern.clone())
}

/// What a key holds, by type, for "1,204 fields": `None` for types without a length.
pub fn length_unit(t: KeyType) -> Option<&'static str> {
    match t {
        KeyType::String => Some("bytes"),
        KeyType::Hash => Some("fields"),
        KeyType::List => Some("items"),
        KeyType::Set | KeyType::ZSet => Some("members"),
        KeyType::Stream => Some("entries"),
        KeyType::Json | KeyType::Other => None,
    }
}

/// The encoding a hash, set or sorted set has once it is past its compact one
/// (`listpack`, `intset`): `hashtable` or `skiplist`. `None` for other types.
pub fn past_compact_encoding(t: KeyType) -> Option<&'static str> {
    match t {
        KeyType::Hash | KeyType::Set => Some("hashtable"),
        KeyType::ZSet => Some("skiplist"),
        _ => None,
    }
}

fn type_plural(t: KeyType) -> &'static str {
    match t {
        KeyType::String => "strings",
        KeyType::Hash => "hashes",
        KeyType::List => "lists",
        KeyType::Set => "sets",
        KeyType::ZSet => "sorted sets",
        KeyType::Stream => "streams",
        KeyType::Json => "JSON documents",
        KeyType::Other => "keys",
    }
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// A pattern with a sampled key of at least [`BIG_KEY_BYTES`]. The scan does not read
/// lengths, so the item rule applies only on Big keys, where lengths are read.
pub fn is_big_pattern(p: &PatternStats) -> bool {
    p.size_max.is_some_and(|m| m >= BIG_KEY_BYTES)
}

/// A pattern with a `lock`, `locks`, `mutex` or `lease` segment (any case).
pub fn is_lock_pattern(pattern: &str) -> bool {
    tokens(pattern)
        .into_iter()
        .filter(|t| is_literal(t))
        .any(|t| LOCK_SEGMENTS.iter().any(|l| t.eq_ignore_ascii_case(l)))
}

/// A lock pattern with sampled keys that have no TTL: verdict `leak?`.
pub fn is_lock_leak(p: &PatternStats) -> bool {
    p.no_ttl > 0 && is_lock_pattern(&p.pattern)
}

// ---------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------

/// The Expiry panel's horizons, matching the TTL buckets' upper bounds.
pub const EXPIRY_LABELS: [&str; 4] = ["1 h", "1 d", "7 d", "30 d"];

/// Memory freed by TTL, scaled like the estimates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expiry {
    /// Memory that expires within each of [`EXPIRY_LABELS`], cumulative.
    pub within: [u64; 4],
    /// Memory that expires after more than 30 days.
    pub later: u64,
    /// Memory that never expires.
    pub never: u64,
    /// All of it: `within[3] + later + never`.
    pub total: u64,
}

impl Expiry {
    /// The share of memory that never expires, from 0.0 to 1.0.
    pub fn never_share(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.never as f64 / self.total as f64
        }
    }
}

/// The factor between the report's estimated and sampled bytes: what the scan scaled
/// sampled counts by (at least 1).
fn byte_scale(report: &ScanReport) -> f64 {
    let sampled: u64 = report.patterns.iter().filter_map(|p| p.sampled_bytes).sum();
    let est: u64 = report.patterns.iter().filter_map(|p| p.est_bytes).sum();
    if sampled == 0 {
        1.0
    } else {
        (est as f64 / sampled as f64).max(1.0)
    }
}

/// Expiry from the report's sampled bytes per TTL bucket, scaled like the estimates.
/// `None` without sizes (or for a snapshot from before the figures existed).
pub fn expiry(report: &ScanReport) -> Option<Expiry> {
    let b = *report.ttl_bytes()?;
    let scale = byte_scale(report);
    let s = |v: u64| (v as f64 * scale).round() as u64;
    let mut within = [0u64; 4];
    let mut acc = 0u64;
    // Buckets 1 and 2 (<1m, <1h) expire within an hour, then one bucket per horizon.
    for (i, w) in within.iter_mut().enumerate() {
        acc = acc.saturating_add(if i == 0 {
            b[1].saturating_add(b[2])
        } else {
            b[i + 2]
        });
        *w = s(acc);
    }
    let later = s(b[6]);
    let never = s(b[0]);
    let total = s(b.iter().fold(0u64, |a, v| a.saturating_add(*v)));
    Some(Expiry {
        within,
        later,
        never,
        total,
    })
}

// ---------------------------------------------------------------------------
// Recommendations
// ---------------------------------------------------------------------------

/// How much a recommendation matters. `Ord` puts the most important first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Likely a problem now: red.
    Warn,
    /// Worth a look: amber.
    Note,
    /// For information: dim.
    Info,
}

/// One line of the Recommendations list: what was found, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    pub severity: Severity,
    pub text: String,
    pub hint: String,
}

/// What the live connection read for one key: its length and encoding. `None` where
/// the command was refused, is unknown, or the key has gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyFacts {
    pub length: Option<u64>,
    pub encoding: Option<String>,
}

/// At most this many hash, set and sorted set patterns have their examples' encodings
/// checked: the biggest ones, since `report.patterns` is sorted by memory.
pub const MAX_ENCODING_PATTERNS: usize = 100;

/// The keys whose encoding the encoding rule looks at: every big key, and the examples
/// of the first [`MAX_ENCODING_PATTERNS`] hash, set and sorted set patterns. At most
/// `50 + 5 × 100`, no repeats.
pub fn keys_to_check(report: &ScanReport) -> Vec<(String, KeyType)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let compact = report
        .patterns
        .iter()
        .filter(|p| p.key_type.and_then(past_compact_encoding).is_some())
        .take(MAX_ENCODING_PATTERNS)
        .flat_map(|p| p.examples.iter());
    for k in report.big_keys().iter().chain(compact) {
        if seen.insert(k.key.as_str()) {
            out.push((k.key.clone(), k.key_type));
        }
    }
    out
}

fn big_key_hint(t: KeyType) -> &'static str {
    match t {
        KeyType::List => "Trim it with LTRIM, or split it, by day for example.",
        KeyType::Stream => "Cap it with XADD MAXLEN ~, or trim it with XTRIM.",
        KeyType::String | KeyType::Json => {
            "Split it, or compress it. Reading it blocks the server."
        }
        KeyType::Hash | KeyType::Set | KeyType::ZSet => {
            "Split it into smaller keys, by a hash of the member for example."
        }
        KeyType::Other => "Split it into smaller keys.",
    }
}

/// Big keys from `report.big_keys`: [`BIG_KEY_BYTES`] or more, or a collection of
/// [`BIG_KEY_ITEMS`] or more items by the length in `facts`.
fn big_key_recs(
    report: &ScanReport,
    facts: &HashMap<String, KeyFacts>,
    out: &mut Vec<Recommendation>,
) {
    for k in report.big_keys() {
        let size = k.bytes.filter(|b| *b >= BIG_KEY_BYTES);
        let items = facts
            .get(&k.key)
            .and_then(|f| f.length)
            .filter(|n| *n >= BIG_KEY_ITEMS && !matches!(k.key_type, KeyType::String));
        let ty = k.key_type.as_str();
        let text = match (size, items) {
            (None, None) => continue,
            (Some(b), None) => format!("{} is a single {} {ty}.", k.key, format::bytes(b)),
            (None, Some(n)) => format!(
                "{} holds {} {}.",
                k.key,
                format::grouped(n),
                length_unit(k.key_type).unwrap_or("items")
            ),
            (Some(b), Some(n)) => format!(
                "{} is a single {} {ty} of {} {}.",
                k.key,
                format::bytes(b),
                format::grouped(n),
                length_unit(k.key_type).unwrap_or("items")
            ),
        };
        out.push(Recommendation {
            severity: Severity::Warn,
            text,
            hint: big_key_hint(k.key_type).to_string(),
        });
    }
}

/// Patterns with [`PAST_COMPACT_SHARE`] or more of their checked keys past the compact
/// encoding. The checked keys are the pattern's examples and big keys in `facts`.
fn encoding_recs(
    report: &ScanReport,
    facts: &HashMap<String, KeyFacts>,
    out: &mut Vec<Recommendation>,
) {
    for p in &report.patterns {
        let Some(t) = p.key_type else { continue };
        let Some(past) = past_compact_encoding(t) else {
            continue;
        };
        let mut seen = std::collections::HashSet::new();
        let (mut checked, mut over) = (0u64, 0u64);
        let in_pattern = report
            .big_keys()
            .iter()
            .filter(|k| key_in_pattern(&k.key, &p.pattern));
        for k in p.examples.iter().chain(in_pattern) {
            if !seen.insert(k.key.as_str()) {
                continue;
            }
            let Some(enc) = facts.get(&k.key).and_then(|f| f.encoding.as_deref()) else {
                continue;
            };
            checked += 1;
            if enc == past {
                over += 1;
            }
        }
        if checked > 0 && over as f64 / checked as f64 >= PAST_COMPACT_SHARE {
            out.push(Recommendation {
                severity: Severity::Note,
                text: format!(
                    "{over} of the {checked} largest sampled {} {} {} {past}.",
                    p.pattern,
                    type_plural(t),
                    if over == 1 { "is" } else { "are" }
                ),
                hint: "Past the compact encoding they take several times the memory.".to_string(),
            });
        }
    }
}

/// Every rule of spec "2 Analysis", most important first; within a severity, in the
/// spec's order. `facts` holds what the live connection read for the keys of
/// [`keys_to_check`]; keys missing from it are left out of the rules that need it.
pub fn recommendations(
    report: &ScanReport,
    diff: Option<&ReportDiff>,
    facts: &HashMap<String, KeyFacts>,
) -> Vec<Recommendation> {
    let mut out = Vec::new();
    big_key_recs(report, facts, &mut out);
    encoding_recs(report, facts, &mut out);
    for p in report.patterns.iter().filter(|p| is_lock_leak(p)) {
        out.push(Recommendation {
            severity: Severity::Note,
            text: format!(
                "{}: {} of its keys never expire.",
                p.pattern,
                format::percent(p.no_ttl_ratio())
            ),
            hint: "A lock should always carry a TTL.".to_string(),
        });
    }
    if let Some(e) = expiry(report).filter(|e| e.never_share() > NEVER_EXPIRES_SHARE) {
        out.push(Recommendation {
            severity: Severity::Info,
            text: format!(
                "{} of memory never expires: {} of {}.",
                format::percent(e.never_share()),
                format::bytes(e.never),
                format::bytes(e.total)
            ),
            hint: "Fine for data that must stay; caches should expire.".to_string(),
        });
    }
    for p in &report.patterns {
        // A lock leak already says it: no second line for its persistent verdict.
        if is_lock_leak(p) {
            continue;
        }
        let change = diff.and_then(|d| d.pattern(&p.pattern));
        let (severity, text, hint) = match ttl_verdict(p, change) {
            TtlVerdict::Ok | TtlVerdict::ShortLived => continue,
            TtlVerdict::PersistentGrowing => (
                Severity::Warn,
                format!("{} never expires and is growing.", p.pattern),
                "Probably a leak: nothing removes these keys.",
            ),
            TtlVerdict::MixedLikelyBug => (
                Severity::Note,
                format!(
                    "{} of {} keys never expire while the rest do.",
                    format::percent(p.no_ttl_ratio()),
                    p.pattern
                ),
                "A write path probably skips EXPIRE.",
            ),
            TtlVerdict::PersistentStable => (
                Severity::Info,
                format!("{} never expires.", p.pattern),
                "Probably intentional, as it is not growing.",
            ),
        };
        out.push(Recommendation {
            severity,
            text,
            hint: hint.to_string(),
        });
    }
    // Stable: the spec's order stays within a severity.
    out.sort_by_key(|r| r.severity);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Cause, PatternDiff};
    use crate::model::{KeySample, ServerFlavor, ServerInfo, ServerMode, Ttl};
    use proptest::prelude::*;

    fn stats(pattern: &str, t: KeyType, sampled: u64, no_ttl: u64, bytes: u64) -> PatternStats {
        let mut hist = [0u64; 7];
        hist[0] = no_ttl;
        hist[4] = sampled - no_ttl;
        PatternStats {
            pattern: pattern.into(),
            key_type: Some(t),
            sampled_keys: sampled,
            est_keys: sampled * 10,
            sampled_bytes: Some(bytes),
            est_bytes: Some(bytes * 10),
            no_ttl,
            ttl_hist: hist,
            size_p50: Some(bytes / sampled.max(1)),
            size_p99: Some(bytes / sampled.max(1)),
            size_max: Some(bytes / sampled.max(1)),
            examples: Vec::new(),
        }
    }

    fn key(name: &str, t: KeyType, bytes: u64) -> KeySample {
        KeySample {
            key: name.into(),
            key_type: t,
            bytes: Some(bytes),
            ttl: Ttl::Persistent,
            lfu_freq: None,
        }
    }

    fn report(patterns: Vec<PatternStats>) -> ScanReport {
        ScanReport {
            profile: "p".into(),
            taken_at_unix: 0,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7.4.2".into(),
                mode: ServerMode::Standalone,
                used_memory: 0,
                maxmemory: 0,
                maxmemory_policy: "noeviction".into(),
                total_keys: 0,
            },
            sampled_keys: patterns.iter().map(|p| p.sampled_keys).sum(),
            patterns,
            sizes_available: true,
            hot_keys: Vec::new(),
            complete: false,
            match_glob: None,
            server_context: None,
            key_sizes: None,
        }
    }

    fn texts(r: &[Recommendation]) -> Vec<&str> {
        r.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn glob_replaces_placeholders_and_escapes_glob_characters() {
        assert_eq!(match_glob("user:{id}:cart"), "user:*:cart");
        assert_eq!(match_glob("rl:{ip}:{id}"), "rl:*:*");
        assert_eq!(match_glob("tenant.{var}:cfg"), "tenant.*:cfg");
        assert_eq!(match_glob("queue:emails"), "queue:emails");
        assert_eq!(
            match_glob("odd:a*b?[c]\\:{id}"),
            "odd:a\\*b\\?\\[c\\]\\\\:*"
        );
        // Braces that are not a placeholder stay literal.
        assert_eq!(match_glob("tpl:{name}"), "tpl:{name}");
    }

    #[test]
    fn the_filter_keeps_only_keys_of_the_pattern() {
        assert!(key_in_pattern("user:88213:cart", "user:{id}:cart"));
        // The glob user:*:cart matches it, but it is another pattern.
        assert!(!key_in_pattern("user:abc:cart", "user:{id}:cart"));
        assert!(!key_in_pattern("user:1:2:cart", "user:{id}:cart"));
        // {var} takes any literal at its place, never a placeholder.
        assert!(key_in_pattern("lock:billing-run", "lock:{var}"));
        assert!(!key_in_pattern("lock:42", "lock:{var}"));
        assert!(key_in_pattern("tenant.a1:cfg", "tenant.{var}:cfg"));
        assert!(!key_in_pattern("tenant.a1:cfg:x", "tenant.{var}:cfg"));
        assert!(key_in_pattern("queue:emails", "queue:emails"));
        assert!(!key_in_pattern("queue:emails2", "queue:emails"));
    }

    /// A Redis glob matcher for `*`, `?`, `[...]` and `\` escapes, enough to check
    /// that a key's glob matches it.
    fn glob_matches(glob: &[char], key: &[char]) -> bool {
        match glob.first() {
            None => key.is_empty(),
            Some('*') => (0..=key.len()).any(|i| glob_matches(&glob[1..], &key[i..])),
            Some('?') => !key.is_empty() && glob_matches(&glob[1..], &key[1..]),
            Some('\\') if glob.len() > 1 => {
                key.first() == Some(&glob[1]) && glob_matches(&glob[2..], &key[1..])
            }
            Some(c) => key.first() == Some(c) && glob_matches(&glob[1..], &key[1..]),
        }
    }

    fn segment() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-z]{1,8}",
            "[0-9]{1,9}",
            "[0-9a-f]{16,20}",
            "[a-z*?\\[\\]\\\\{}-]{1,6}",
            Just("01JCDX4N8Q2R7V3K9M5T6W1YZB".to_string()),
            Just("10.0.4.12".to_string()),
            Just("9f1c2a7e-4b3d-4e0a-9c61-2f8d7b1e5a43".to_string()),
        ]
    }

    fn key_strategy() -> impl Strategy<Value = String> {
        (
            prop::collection::vec(segment(), 1..5),
            prop::collection::vec(prop_oneof![Just(':'), Just('/'), Just('|')], 4),
        )
            .prop_map(|(segs, seps)| {
                let mut k = String::new();
                for (i, s) in segs.iter().enumerate() {
                    if i > 0 {
                        k.push(seps[i - 1]);
                    }
                    k.push_str(s);
                }
                k
            })
    }

    proptest! {
        /// A key's own pattern: its glob matches it and the filter keeps it.
        #[test]
        fn a_keys_glob_matches_it_and_the_filter_keeps_it(k in key_strategy()) {
            let p = pattern_for_key(&k);
            let g: Vec<char> = match_glob(&p).chars().collect();
            let kc: Vec<char> = k.chars().collect();
            prop_assert!(glob_matches(&g, &kc), "{} does not match {}", match_glob(&p), k);
            prop_assert!(key_in_pattern(&k, &p));
        }

        /// Every key the filter lists infers to the selected pattern, whatever the
        /// glob let through: its own pattern is the pattern, up to `{var}`.
        #[test]
        fn every_listed_key_infers_to_the_pattern(
            keys in prop::collection::vec(key_strategy(), 1..40),
            pick in any::<prop::sample::Index>(),
        ) {
            let pattern = pattern_for_key(pick.get(&keys));
            let listed: Vec<&String> = keys.iter().filter(|k| key_in_pattern(k, &pattern)).collect();
            prop_assert!(!listed.is_empty());
            for k in listed {
                prop_assert!(same_up_to_var(&pattern, &pattern_for_key(k)));
            }
            // None that infers to it is dropped.
            for k in &keys {
                if pattern_for_key(k) == pattern {
                    prop_assert!(key_in_pattern(k, &pattern));
                }
            }
        }
    }

    #[test]
    fn big_pattern_threshold_on_both_sides() {
        let mut p = stats("blob:{id}", KeyType::String, 1, 0, BIG_KEY_BYTES);
        assert!(is_big_pattern(&p));
        p.size_max = Some(BIG_KEY_BYTES - 1);
        assert!(!is_big_pattern(&p));
        p.size_max = None;
        assert!(!is_big_pattern(&p));
    }

    #[test]
    fn lock_leak_needs_a_lock_segment_and_a_key_without_ttl() {
        for name in [
            "lock:{id}",
            "locks:{var}",
            "app:mutex:{id}",
            "x.LEASE.{id}",
            "a/Lock",
        ] {
            assert!(is_lock_pattern(name), "{name}");
            assert!(
                is_lock_leak(&stats(name, KeyType::String, 10, 1, 100)),
                "{name}"
            );
            assert!(
                !is_lock_leak(&stats(name, KeyType::String, 10, 0, 100)),
                "{name}"
            );
        }
        for name in ["blocked:{id}", "lockout:{id}", "unlock", "{var}:x"] {
            assert!(!is_lock_pattern(name), "{name}");
        }
    }

    #[test]
    fn expiry_sums_buckets_and_scales_like_the_estimates() {
        // Sampled bytes 1,000 per pattern, estimated 10,000: a scale of 10.
        let mut r = report(vec![stats("a:{id}", KeyType::String, 10, 5, 1_000)]);
        assert_eq!(expiry(&r), None);
        r.key_sizes.get_or_insert_default().ttl_bytes = [400, 10, 20, 30, 40, 50, 450];
        let e = expiry(&r).unwrap();
        assert_eq!(e.within, [300, 600, 1_000, 1_500]);
        assert_eq!(e.later, 4_500);
        assert_eq!(e.never, 4_000);
        assert_eq!(e.total, 10_000);
        assert!((e.never_share() - 0.4).abs() < 1e-9);
        // A whole-keyspace scan: no scaling.
        r.patterns[0].est_bytes = Some(1_000);
        assert_eq!(expiry(&r).unwrap().never, 400);
    }

    #[test]
    fn never_expires_share_threshold_on_both_sides() {
        let mut r = report(vec![stats("a:{id}", KeyType::String, 10, 0, 1_000)]);
        r.patterns[0].est_bytes = Some(1_000);
        let rule = |r: &ScanReport| {
            recommendations(r, None, &HashMap::new())
                .iter()
                .any(|x| x.text.contains("of memory never expires"))
        };
        // 25% exactly is not over 25%.
        r.key_sizes.get_or_insert_default().ttl_bytes = [250, 0, 0, 750, 0, 0, 0];
        assert!(!rule(&r));
        r.key_sizes.get_or_insert_default().ttl_bytes = [251, 0, 0, 749, 0, 0, 0];
        assert!(rule(&r));
    }

    #[test]
    fn big_key_by_size_and_by_items_on_both_sides() {
        let mut r = report(vec![stats("q:{id}", KeyType::List, 4, 0, 100)]);
        r.key_sizes.get_or_insert_default().big_keys = vec![
            key("q:1", KeyType::List, BIG_KEY_BYTES),
            key("q:2", KeyType::List, BIG_KEY_BYTES - 1),
            key("q:3", KeyType::List, 10),
            key("q:4", KeyType::List, 10),
            key("s:1", KeyType::String, 10),
        ];
        let facts: HashMap<String, KeyFacts> = [
            ("q:3", BIG_KEY_ITEMS),
            ("q:4", BIG_KEY_ITEMS - 1),
            // A string's length is bytes, not items.
            ("s:1", BIG_KEY_ITEMS * 2),
        ]
        .into_iter()
        .map(|(k, n)| {
            (
                k.to_string(),
                KeyFacts {
                    length: Some(n),
                    encoding: None,
                },
            )
        })
        .collect();
        let recs = recommendations(&r, None, &facts);
        let warns: Vec<&str> = recs
            .iter()
            .filter(|x| x.severity == Severity::Warn)
            .map(|x| x.text.as_str())
            .collect();
        assert_eq!(
            warns,
            ["q:1 is a single 64M list.", "q:3 holds 1,000,000 items."],
            "{recs:#?}"
        );
    }

    #[test]
    fn past_compact_encoding_share_on_both_sides() {
        let mut p = stats("cart:{id}", KeyType::Hash, 100, 0, 100_000);
        // 20 examples: 1 over is exactly 5%; with 21 checked it is under.
        p.examples = (0..20)
            .map(|i| key(&format!("cart:{i}"), KeyType::Hash, 100))
            .collect();
        let mut r = report(vec![p]);
        let enc = |n: usize, over: usize| -> HashMap<String, KeyFacts> {
            (0..n)
                .map(|i| {
                    (
                        format!("cart:{i}"),
                        KeyFacts {
                            length: None,
                            encoding: Some(if i < over { "hashtable" } else { "listpack" }.into()),
                        },
                    )
                })
                .collect()
        };
        let fired = |r: &ScanReport, f: &HashMap<String, KeyFacts>| {
            texts(&recommendations(r, None, f))
                .iter()
                .any(|t| t.ends_with(" hashtable."))
        };
        assert!(fired(&r, &enc(20, 1)));
        assert!(!fired(&r, &enc(20, 0)));
        // A big key of the pattern counts too: 21 checked, 1 over is under 5%.
        r.key_sizes.get_or_insert_default().big_keys = vec![key("cart:99", KeyType::Hash, 100)];
        let mut f = enc(20, 1);
        f.insert(
            "cart:99".into(),
            KeyFacts {
                length: None,
                encoding: Some("listpack".into()),
            },
        );
        assert!(!fired(&r, &f));
        // Nothing checked: no rule.
        assert!(!fired(&r, &HashMap::new()));
        // Sorted sets go past compact as skiplist; strings never do.
        assert_eq!(past_compact_encoding(KeyType::ZSet), Some("skiplist"));
        assert_eq!(past_compact_encoding(KeyType::String), None);
    }

    #[test]
    fn ttl_verdicts_that_are_not_ok_and_lock_leaks() {
        let r = report(vec![
            stats("ok:{id}", KeyType::String, 100, 0, 100),
            // 5% with no TTL is mixed, 4% is not.
            stats("mixed:{id}", KeyType::String, 100, 5, 100),
            stats("fine:{id}", KeyType::String, 100, 4, 100),
            stats("forever:{id}", KeyType::String, 100, 100, 100),
            stats("lock:{id}", KeyType::String, 100, 100, 100),
            stats("grow:{id}", KeyType::String, 100, 100, 100),
        ]);
        let diff = ReportDiff {
            before_taken_at_unix: 0,
            after_taken_at_unix: 1,
            used_memory_before: 0,
            used_memory_after: 0,
            patterns: vec![PatternDiff {
                pattern: "grow:{id}".into(),
                keys_before: None,
                keys_after: Some(1_000),
                bytes_before: None,
                bytes_after: Some(1_000),
                delta_bytes: Some(1_000),
                delta_keys: 1_000,
                cause: Cause::New,
                partial: false,
                pattern_sampled_keys_after: 100,
                sampled_keys_after: 600,
            }],
        };
        let recs = recommendations(&r, Some(&diff), &HashMap::new());
        assert_eq!(
            texts(&recs),
            [
                "grow:{id} never expires and is growing.",
                "lock:{id}: 100% of its keys never expire.",
                "5% of mixed:{id} keys never expire while the rest do.",
                "forever:{id} never expires.",
            ],
            "{recs:#?}"
        );
    }

    #[test]
    fn keys_to_check_are_big_keys_and_compact_type_examples_once() {
        let mut h = stats("cart:{id}", KeyType::Hash, 2, 0, 100);
        h.examples = vec![
            key("cart:1", KeyType::Hash, 60),
            key("cart:2", KeyType::Hash, 40),
        ];
        let mut s = stats("blob:{id}", KeyType::String, 1, 0, 100);
        s.examples = vec![key("blob:1", KeyType::String, 100)];
        let mut r = report(vec![h, s]);
        r.key_sizes.get_or_insert_default().big_keys = vec![
            key("blob:1", KeyType::String, 100),
            key("cart:1", KeyType::Hash, 60),
        ];
        let keys: Vec<String> = keys_to_check(&r).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["blob:1", "cart:1", "cart:2"]);
    }

    #[test]
    fn encodings_are_checked_for_the_biggest_patterns_only() {
        let patterns = (0..MAX_ENCODING_PATTERNS + 20)
            .map(|i| {
                let mut p = stats(&format!("h{i}:{{id}}"), KeyType::Hash, 1, 0, 100);
                p.examples = vec![key(&format!("h{i}:1"), KeyType::Hash, 10)];
                p
            })
            .collect();
        let keys = keys_to_check(&report(patterns));
        assert_eq!(keys.len(), MAX_ENCODING_PATTERNS);
        assert_eq!(
            keys.last().unwrap().0,
            format!("h{}:1", MAX_ENCODING_PATTERNS - 1)
        );
    }

    #[test]
    fn pattern_of_finds_the_reports_pattern_up_to_var() {
        let r = report(vec![
            stats("lock:{var}", KeyType::String, 1, 0, 1),
            stats("user:{id}:cart", KeyType::Hash, 1, 0, 1),
        ]);
        assert_eq!(pattern_of(&r, "lock:billing-run"), "lock:{var}");
        assert_eq!(pattern_of(&r, "user:7:cart"), "user:{id}:cart");
        assert_eq!(pattern_of(&r, "other:7"), "other:{id}");
    }
}
