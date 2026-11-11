//! Pattern inference: turning keys like `user:88213:cart` into `user:{id}:cart`.
//!
//! Owner: scanner.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::model::{KeySample, KeyType, PatternStats};

/// Segment separators that are kept in the output.
const SEPARATORS: [char; 3] = [':', '/', '|'];

/// Placeholders produced by [`pattern_for_key`] and by the cardinality collapse.
const PLACEHOLDERS: [&str; 6] = ["{id}", "{uuid}", "{ulid}", "{hash}", "{ip}", "{var}"];

/// Collapse threshold: this many distinct literals at one position become `{var}`.
const COLLAPSE_THRESHOLD: usize = 20;

/// The pattern for one key, from its own segments.
///
/// Splits on `:` `/` `|` (keeping the separators). A segment that is an IPv4 address
/// becomes `{ip}`; any other segment containing `.` is split further on `.`. Each
/// piece is then replaced, in this order, by `{uuid}`, `{ulid}`, `{id}` (digits
/// only), `{hash}` (hex, 16 or more characters), or kept as is.
pub fn pattern_for_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut start = 0;
    for (i, c) in key.char_indices() {
        if SEPARATORS.contains(&c) {
            push_segment(&mut out, &key[start..i]);
            out.push(c);
            start = i + c.len_utf8();
        }
    }
    push_segment(&mut out, &key[start..]);
    out
}

fn push_segment(out: &mut String, seg: &str) {
    if is_ipv4(seg) {
        out.push_str("{ip}");
    } else if seg.contains('.') {
        for (i, part) in seg.split('.').enumerate() {
            if i > 0 {
                out.push('.');
            }
            out.push_str(&classify(part));
        }
    } else {
        out.push_str(&classify(seg));
    }
}

fn classify(seg: &str) -> String {
    if is_uuid(seg) {
        "{uuid}".into()
    } else if is_ulid(seg) {
        "{ulid}".into()
    } else if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
        "{id}".into()
    } else if seg.len() >= 16 && seg.bytes().all(|b| b.is_ascii_hexdigit()) {
        "{hash}".into()
    } else {
        seg.to_string()
    }
}

fn is_ipv4(seg: &str) -> bool {
    let mut n = 0;
    for part in seg.split('.') {
        n += 1;
        if part.is_empty()
            || part.len() > 3
            || !part.bytes().all(|b| b.is_ascii_digit())
            || part.parse::<u16>().is_ok_and(|v| v > 255)
        {
            return false;
        }
    }
    n == 4
}

fn is_uuid(seg: &str) -> bool {
    let b = seg.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn is_ulid(seg: &str) -> bool {
    let b = seg.as_bytes();
    b.len() == 26
        && b.iter().all(|c| {
            matches!(c.to_ascii_uppercase(), b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
        && b.iter().any(u8::is_ascii_digit)
        && b.iter().any(u8::is_ascii_alphabetic)
}

/// Split a pattern into alternating segments and single-character separators.
fn tokens(pattern: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in pattern.char_indices() {
        if SEPARATORS.contains(&c) || c == '.' {
            out.push(&pattern[start..i]);
            out.push(&pattern[i..i + 1]);
            start = i + 1;
        }
    }
    out.push(&pattern[start..]);
    out
}

fn is_literal(token: &str) -> bool {
    !token.is_empty() && !PLACEHOLDERS.contains(&token)
}

/// Merge patterns that differ only at one literal position when at least
/// [`COLLAPSE_THRESHOLD`] distinct values share it. Returns old pattern -> new pattern.
///
/// The first segment never collapses: it names the namespace (`user`, `order`, ...),
/// and many unrelated namespaces with the same shape are normal. That also means a
/// single-segment key never collapses.
fn collapse(patterns: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut current: BTreeMap<String, String> =
        patterns.iter().map(|p| (p.clone(), p.clone())).collect();
    loop {
        // (tokens with the position blanked, position) -> distinct literals
        type Group = (Vec<String>, usize);
        let mut groups: BTreeMap<Group, BTreeSet<String>> = BTreeMap::new();
        let live: BTreeSet<&String> = current.values().collect();
        for p in &live {
            let toks = tokens(p);
            // Even indices are segments; index 0 is the first segment and is skipped.
            for i in (2..toks.len()).step_by(2) {
                if is_literal(toks[i]) {
                    let mut sig: Vec<String> = toks.iter().map(|t| t.to_string()).collect();
                    sig[i] = String::new();
                    groups
                        .entry((sig, i))
                        .or_default()
                        .insert(toks[i].to_string());
                }
            }
        }
        let mut rewrite: BTreeMap<String, String> = BTreeMap::new();
        for ((sig, i), values) in &groups {
            if values.len() < COLLAPSE_THRESHOLD {
                continue;
            }
            for v in values {
                let mut toks = sig.clone();
                toks[*i] = v.clone();
                let old = toks.concat();
                toks[*i] = "{var}".into();
                rewrite.entry(old).or_insert_with(|| toks.concat());
            }
        }
        if rewrite.is_empty() {
            return current;
        }
        for new in current.values_mut() {
            if let Some(r) = rewrite.get(new.as_str()) {
                *new = r.clone();
            }
        }
    }
}

/// Group sampled keys into patterns and compute per-pattern statistics.
///
/// `scale` turns sampled counts and bytes into keyspace-wide estimates (the scanner's
/// `estimate_scale`: `total_keys` over the keys seen that still existed, at least 1). Patterns with the same shape that differ at one literal
/// segment other than the first, with 20 or more distinct values there, merge into one
/// `{var}` pattern.
/// Patterns are sorted by `est_bytes` (None counts as 0), then `est_keys`, then name,
/// largest first.
pub fn aggregate(samples: &[KeySample], scale: f64) -> Vec<PatternStats> {
    let mut by_pattern: BTreeMap<String, Vec<&KeySample>> = BTreeMap::new();
    for s in samples {
        by_pattern
            .entry(pattern_for_key(&s.key))
            .or_default()
            .push(s);
    }
    let names: BTreeSet<String> = by_pattern.keys().cloned().collect();
    let mapping = collapse(&names);
    let mut merged: BTreeMap<String, Vec<&KeySample>> = BTreeMap::new();
    for (old, group) in by_pattern {
        let new = mapping.get(&old).cloned().unwrap_or(old);
        merged.entry(new).or_default().extend(group);
    }

    let mut stats: Vec<PatternStats> = merged
        .into_iter()
        .map(|(pattern, group)| build_stats(pattern, group, scale))
        .collect();
    stats.sort_by(|a, b| {
        b.est_bytes
            .unwrap_or(0)
            .cmp(&a.est_bytes.unwrap_or(0))
            .then(b.est_keys.cmp(&a.est_keys))
            .then(a.pattern.cmp(&b.pattern))
    });
    stats
}

fn build_stats(pattern: String, group: Vec<&KeySample>, scale: f64) -> PatternStats {
    let sampled_keys = group.len() as u64;
    let mut sizes: Vec<u64> = group.iter().filter_map(|s| s.bytes).collect();
    sizes.sort_unstable();
    let sampled_bytes = (!sizes.is_empty()).then(|| sizes.iter().sum::<u64>());

    let mut ttl_hist = [0u64; 7];
    let mut no_ttl = 0;
    for s in &group {
        ttl_hist[s.ttl.bucket()] += 1;
        if s.ttl == crate::model::Ttl::Persistent {
            no_ttl += 1;
        }
    }

    let first_type = group[0].key_type;
    let key_type: Option<KeyType> = group
        .iter()
        .all(|s| s.key_type == first_type)
        .then_some(first_type);

    let mut examples: Vec<KeySample> = group.iter().map(|s| (*s).clone()).collect();
    examples.sort_by(|a, b| match (a.bytes, b.bytes) {
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.key.cmp(&b.key)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.key.cmp(&b.key),
    });
    examples.truncate(5);

    PatternStats {
        pattern,
        key_type,
        sampled_keys,
        est_keys: scaled(sampled_keys, scale),
        sampled_bytes,
        est_bytes: sampled_bytes.map(|b| scaled(b, scale)),
        no_ttl,
        ttl_hist,
        size_p50: percentile(&sizes, 50),
        size_p99: percentile(&sizes, 99),
        size_max: sizes.last().copied(),
        examples,
    }
}

fn scaled(n: u64, scale: f64) -> u64 {
    (n as f64 * scale).round() as u64
}

/// Nearest-rank percentile of an ascending slice.
fn percentile(sorted: &[u64], p: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    Some(sorted[rank - 1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Ttl;
    use proptest::prelude::*;
    use std::time::Duration;

    #[test]
    fn pattern_table() {
        let cases = [
            ("user:88213:cart", "user:{id}:cart"),
            (
                "session:9f1c2a7e-4b3d-4e0a-9c61-2f8d7b1e5a43",
                "session:{uuid}",
            ),
            (
                "session:9F1C2A7E-4B3D-4E0A-9C61-2F8D7B1E5A43",
                "session:{uuid}",
            ),
            ("tmp:import:01JCDX4N8Q2R7V3K9M5T6W1YZB", "tmp:import:{ulid}"),
            (
                "cache:search:3f9a6c01e2b7d48f5a90c3e1b6d27f84a05c9e13",
                "cache:search:{hash}",
            ),
            ("rl:10.0.4.12:29187460", "rl:{ip}:{id}"),
            ("feature:checkout-v2", "feature:checkout-v2"),
            ("queue:emails", "queue:emails"),
            ("a/b|c.d", "a/b|c.d"),
            ("", ""),
            ("plainkey", "plainkey"),
            ("cache:product:1", "cache:product:{id}"),
            ("v1.2", "v1.{id}"),
            ("file.123.txt", "file.{id}.txt"),
            ("300.1.1.1", "{id}.{id}.{id}.{id}"),
            ("a::b", "a::b"),
            ("short:abcdef0123456789", "short:{hash}"),
            ("short:abcdef012345678", "short:abcdef012345678"),
        ];
        for (key, want) in cases {
            assert_eq!(pattern_for_key(key), want, "key {key:?}");
        }
    }

    #[test]
    fn ulid_needs_digit_and_letter() {
        // 26 letters, no digit: not a ULID.
        assert_eq!(
            pattern_for_key("x:ABCDEFGHJKMNPQRSTVWXYZABCD"),
            "x:ABCDEFGHJKMNPQRSTVWXYZABCD"
        );
        // Contains U (not Crockford).
        assert_eq!(
            pattern_for_key("x:01JCDX4N8Q2R7V3K9M5T6W1YZU"),
            "x:01JCDX4N8Q2R7V3K9M5T6W1YZU"
        );
        assert_eq!(pattern_for_key("x:01jcdx4n8q2r7v3k9m5t6w1yzb"), "x:{ulid}");
    }

    fn sample(key: &str, ty: KeyType, bytes: Option<u64>, ttl: Ttl) -> KeySample {
        KeySample {
            key: key.into(),
            key_type: ty,
            bytes,
            ttl,
            lfu_freq: None,
        }
    }

    fn s(key: &str, bytes: Option<u64>) -> KeySample {
        sample(key, KeyType::String, bytes, Ttl::Persistent)
    }

    #[test]
    fn aggregate_math() {
        let mut samples: Vec<KeySample> = (1..=100u64)
            .map(|n| {
                sample(
                    &format!("user:{n}:cart"),
                    KeyType::Hash,
                    Some(n * 10),
                    if n % 4 == 0 {
                        Ttl::Persistent
                    } else {
                        Ttl::Expires(Duration::from_secs(600))
                    },
                )
            })
            .collect();
        samples.push(s("queue:emails", Some(5)));
        let out = aggregate(&samples, 2.5);
        assert_eq!(out.len(), 2);
        let p = &out[0];
        assert_eq!(p.pattern, "user:{id}:cart");
        assert_eq!(p.key_type, Some(KeyType::Hash));
        assert_eq!(p.sampled_keys, 100);
        assert_eq!(p.est_keys, 250);
        assert_eq!(p.sampled_bytes, Some(50_500));
        assert_eq!(p.est_bytes, Some(126_250));
        assert_eq!(p.no_ttl, 25);
        assert_eq!(p.ttl_hist[0], 25);
        assert_eq!(p.ttl_hist[2], 75);
        assert_eq!(p.size_p50, Some(500));
        assert_eq!(p.size_p99, Some(990));
        assert_eq!(p.size_max, Some(1000));
        assert_eq!(p.examples.len(), 5);
        assert_eq!(p.examples[0].key, "user:100:cart");
        assert_eq!(p.examples[4].key, "user:96:cart");
        let q = &out[1];
        assert_eq!(q.est_keys, 3); // round(2.5)
        assert_eq!(q.est_bytes, Some(13)); // round(12.5)
    }

    #[test]
    fn percentile_nearest_rank() {
        assert_eq!(percentile(&[], 50), None);
        assert_eq!(percentile(&[7], 99), Some(7));
        assert_eq!(percentile(&[1, 2, 3, 4], 50), Some(2));
        assert_eq!(percentile(&[1, 2, 3, 4], 99), Some(4));
    }

    #[test]
    fn all_none_bytes() {
        let out = aggregate(&[s("a:1", None), s("a:2", None)], 1.0);
        let p = &out[0];
        assert_eq!(p.sampled_bytes, None);
        assert_eq!(p.est_bytes, None);
        assert_eq!(p.size_p50, None);
        assert_eq!(p.size_p99, None);
        assert_eq!(p.size_max, None);
        assert_eq!(p.est_keys, 2);
    }

    #[test]
    fn partial_bytes_use_known_samples() {
        let out = aggregate(&[s("a:1", Some(10)), s("a:2", None)], 2.0);
        let p = &out[0];
        assert_eq!(p.sampled_bytes, Some(10));
        assert_eq!(p.est_bytes, Some(20));
        assert_eq!(p.size_max, Some(10));
        // None last in examples.
        assert_eq!(p.examples[0].key, "a:1");
        assert_eq!(p.examples[1].key, "a:2");
    }

    #[test]
    fn mixed_types_give_none() {
        let out = aggregate(
            &[
                sample("a:1", KeyType::String, None, Ttl::Persistent),
                sample("a:2", KeyType::Hash, None, Ttl::Persistent),
            ],
            1.0,
        );
        assert_eq!(out[0].key_type, None);
    }

    #[test]
    fn sort_order_and_example_ties() {
        let samples = vec![
            s("b:x", None),
            s("a:x", None),
            s("c:x", Some(100)),
            s("d:x", Some(100)),
            s("d:y", Some(100)),
        ];
        let out = aggregate(&samples, 1.0);
        let names: Vec<&str> = out.iter().map(|p| p.pattern.as_str()).collect();
        // d:x and d:y and c:x all 100 bytes, 1 key each; None patterns last, by name.
        assert_eq!(names, ["c:x", "d:x", "d:y", "a:x", "b:x"]);

        let ties = aggregate(
            &[s("k:2", Some(1)), s("k:1", Some(1)), s("k:3", Some(2))],
            1.0,
        );
        let keys: Vec<&str> = ties[0].examples.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["k:3", "k:1", "k:2"]);
    }

    fn tenants(n: usize) -> Vec<KeySample> {
        (1..=n)
            .map(|i| s(&format!("cfg:tenant-{i:04}x:flags"), Some(10)))
            .collect()
    }

    #[test]
    fn collapse_at_threshold() {
        let out = aggregate(&tenants(40), 1.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pattern, "cfg:{var}:flags");
        assert_eq!(out[0].sampled_keys, 40);
        let out = aggregate(&tenants(20), 1.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pattern, "cfg:{var}:flags");
    }

    #[test]
    fn first_segment_never_collapses() {
        let entities = [
            "user", "order", "cart", "product", "invoice", "payment", "session", "token",
            "account", "address", "review", "coupon", "shipment", "refund", "category", "vendor",
            "wishlist", "ticket", "message", "device",
        ];
        assert_eq!(entities.len(), 20);
        let samples: Vec<KeySample> = entities
            .iter()
            .flat_map(|e| (1..=3).map(move |n| s(&format!("{e}:{n}"), Some(1))))
            .collect();
        let out = aggregate(&samples, 1.0);
        assert_eq!(out.len(), 20);
        assert!(out.iter().all(|p| !p.pattern.contains("{var}")));
        assert!(out.iter().any(|p| p.pattern == "user:{id}"));
        assert!(out.iter().any(|p| p.pattern == "order:{id}"));

        // Single-segment keys stay as they are, however many there are.
        let singles: Vec<KeySample> = (0..30).map(|i| s(&format!("k{i}x"), Some(1))).collect();
        assert_eq!(aggregate(&singles, 1.0).len(), 30);

        // The old position-0 case no longer collapses.
        let old: Vec<KeySample> = (1..=25)
            .map(|i| s(&format!("tenant-{i:04}x:config"), Some(1)))
            .collect();
        assert_eq!(aggregate(&old, 1.0).len(), 25);
    }

    #[test]
    fn no_collapse_below_threshold() {
        let out = aggregate(&tenants(19), 1.0);
        assert_eq!(out.len(), 19);
    }

    #[test]
    fn collapse_keeps_other_shapes_apart() {
        let mut samples = tenants(25);
        samples.push(s("cfg:tenant-0001x:other", Some(1)));
        samples.push(s("queue:emails", Some(1)));
        let out = aggregate(&samples, 1.0);
        let names: BTreeSet<&str> = out.iter().map(|p| p.pattern.as_str()).collect();
        assert!(names.contains("cfg:{var}:flags"));
        assert!(names.contains("cfg:tenant-0001x:other"));
        assert!(names.contains("queue:emails"));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn collapse_with_ids_and_second_position() {
        // Two collapsible positions: both end up {var}.
        let samples: Vec<KeySample> = (0..20)
            .flat_map(|a| (0..20).map(move |b| s(&format!("p:a{a}x:b{b}x:{b}"), Some(1))))
            .collect();
        let out = aggregate(&samples, 1.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pattern, "p:{var}:{var}:{id}");
        assert_eq!(out[0].sampled_keys, 400);
    }

    proptest! {
        #[test]
        fn never_panics(key in any::<String>()) {
            let _ = pattern_for_key(&key);
        }

        #[test]
        fn digits_segment_is_irrelevant(
            prefix in "[a-z]{1,6}",
            suffix in "[a-z]{1,6}",
            a in "[0-9]{1,12}",
            b in "[0-9]{1,12}",
            sep in prop::sample::select(vec![':', '/', '|']),
        ) {
            let ka = format!("{prefix}{sep}{a}{sep}{suffix}");
            let kb = format!("{prefix}{sep}{b}{sep}{suffix}");
            prop_assert_eq!(pattern_for_key(&ka), pattern_for_key(&kb));
        }

        #[test]
        fn output_has_no_digit_only_segment(key in any::<String>()) {
            let out = pattern_for_key(&key);
            for seg in out.split([':', '/', '|', '.']) {
                prop_assert!(
                    seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit()),
                    "segment {seg:?} in {out:?}"
                );
            }
        }

        #[test]
        fn output_has_no_digit_only_segment_idish(key in "[0-9a-f:/|.-]{0,40}") {
            let out = pattern_for_key(&key);
            for seg in out.split([':', '/', '|', '.']) {
                prop_assert!(seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit()));
            }
        }
    }
}
