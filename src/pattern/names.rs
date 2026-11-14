//! The pattern tree from key names alone, for Browse before an analysis: names read by
//! [`crate::live::names`] (`SCAN` only), patterns inferred exactly as for a scan
//! ([`super::pattern_for_key`], then the `{var}` collapse over every pattern seen), and
//! key counts estimated from `DBSIZE`:
//!
//! `est_keys = round(names_seen / names_read × DBSIZE)`
//!
//! Names carry no type, size or TTL: only counts. Owner: scanner.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::model::PatternStats;

/// Example names kept per pattern: the first by name.
const EXAMPLES: usize = 5;

/// One pattern of [`NamePatterns::patterns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamePattern {
    /// The pattern, e.g. `user:{id}:cart`, the same a scan infers for these names.
    pub pattern: String,
    /// Distinct names read that infer to it.
    pub names_seen: u64,
    /// The estimated keys: `names_seen` scaled by [`name_scale`], rounded.
    pub est_keys: u64,
    /// Up to five of its names, the first by name.
    pub examples: Vec<String>,
}

impl NamePattern {
    /// As a [`PatternStats`] row, for code that draws pattern rows: `sampled_keys` is
    /// `names_seen` and `est_keys` the estimate. Everything a name doesn't tell is
    /// empty: no type, no sizes, no examples, and `no_ttl` and `ttl_hist` are 0
    /// because the TTL is **unknown**, not because keys have one. Do not draw TTL,
    /// size or type figures from it.
    pub fn as_pattern_stats(&self) -> PatternStats {
        PatternStats {
            pattern: self.pattern.clone(),
            key_type: None,
            sampled_keys: self.names_seen,
            est_keys: self.est_keys,
            sampled_bytes: None,
            est_bytes: None,
            no_ttl: 0,
            ttl_hist: [0; 7],
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: Vec::new(),
        }
    }
}

/// Distinct names under one pattern of [`super::pattern_for_key`], before the collapse.
#[derive(Debug, Clone, Default)]
struct Raw {
    names: u64,
    /// The first [`EXAMPLES`] names by name, ascending.
    examples: Vec<String>,
}

impl Raw {
    fn add(&mut self, name: &str) {
        self.names += 1;
        keep_first(&mut self.examples, name);
    }
}

/// Keeps `v` the first [`EXAMPLES`] names by name, ascending.
fn keep_first(v: &mut Vec<String>, name: &str) {
    if v.len() == EXAMPLES && v.last().is_some_and(|l| l.as_str() <= name) {
        return;
    }
    let at = v.partition_point(|x| x.as_str() < name);
    if v.get(at).map(String::as_str) == Some(name) {
        return;
    }
    v.insert(at, name.to_string());
    v.truncate(EXAMPLES);
}

/// The factor that turns names seen into estimated keys:
/// - 1.0 when every name was read (`complete`: the walk ended on every node), or when
///   nothing was read;
/// - otherwise `dbsize / names_read`, at least 1.0 (keys added after `DBSIZE` never
///   shrink the estimate below what was seen).
///
/// Names read with `MATCH`: `DBSIZE` counts every key, not the matching ones, so pass
/// `complete` true to show names as seen.
pub fn name_scale(dbsize: u64, names_read: u64, complete: bool) -> f64 {
    if complete || names_read == 0 {
        1.0
    } else {
        (dbsize as f64 / names_read as f64).max(1.0)
    }
}

/// Builds the pattern tree from key names, a page at a time.
///
/// Add each page's names with [`NamePatterns::add`] (and its non-UTF-8 count with
/// [`NamePatterns::add_unreadable`]), then ask for [`NamePatterns::patterns`] whenever
/// the tree is drawn anew. Names already seen are not counted again (`SCAN` can return
/// a name twice). Adding is O(names) and `patterns` is O(patterns): the names
/// themselves are only kept to tell repeats apart.
#[derive(Debug, Clone, Default)]
pub struct NamePatterns {
    seen: HashSet<String>,
    unreadable: u64,
    raw: BTreeMap<String, Raw>,
}

impl NamePatterns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds names; returns how many were new.
    pub fn add<I, S>(&mut self, names: I) -> u64
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut new = 0;
        for name in names {
            let name: String = name.into();
            if self.seen.contains(&name) {
                continue;
            }
            self.raw
                .entry(super::pattern_for_key(&name))
                .or_default()
                .add(&name);
            self.seen.insert(name);
            new += 1;
        }
        new
    }

    /// Counts names that are not UTF-8 ([`crate::live::names::Page::unreadable`]): keys
    /// of no pattern, but read all the same, so they stay in the estimate's denominator.
    /// They can't be told apart, so a repeat counts again.
    pub fn add_unreadable(&mut self, n: u64) {
        self.unreadable = self.unreadable.saturating_add(n);
    }

    /// Names read: distinct UTF-8 names plus the unreadable ones. The estimate's
    /// denominator.
    pub fn names_read(&self) -> u64 {
        (self.seen.len() as u64).saturating_add(self.unreadable)
    }

    /// True when `name` was read.
    pub fn contains(&self, name: &str) -> bool {
        self.seen.contains(name)
    }

    /// The patterns so far, largest estimate first, then by name. `dbsize` is the
    /// database's `DBSIZE` (summed over the nodes on a cluster); `complete` says every
    /// name was read ([`crate::live::names::Walk::complete`]), see [`name_scale`].
    pub fn patterns(&self, dbsize: u64, complete: bool) -> Vec<NamePattern> {
        let scale = name_scale(dbsize, self.names_read(), complete);
        let names: BTreeSet<String> = self.raw.keys().cloned().collect();
        let mapping = super::collapse(&names);
        let mut merged: BTreeMap<String, Raw> = BTreeMap::new();
        for (old, raw) in &self.raw {
            let new = mapping.get(old).cloned().unwrap_or_else(|| old.clone());
            let m = merged.entry(new).or_default();
            m.names += raw.names;
            for e in &raw.examples {
                keep_first(&mut m.examples, e);
            }
        }
        let mut out: Vec<NamePattern> = merged
            .into_iter()
            .map(|(pattern, raw)| NamePattern {
                pattern,
                names_seen: raw.names,
                est_keys: (raw.names as f64 * scale).round() as u64,
                examples: raw.examples,
            })
            .collect();
        out.sort_by(|a, b| {
            b.est_keys
                .cmp(&a.est_keys)
                .then_with(|| a.pattern.cmp(&b.pattern))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{KeySample, KeyType, Ttl};
    use proptest::prelude::*;

    #[test]
    fn counts_and_estimates_from_names() {
        let mut b = NamePatterns::new();
        let users: Vec<String> = (0..600).map(|i| format!("user:{i}:cart")).collect();
        let sessions: Vec<String> = (0..300).map(|i| format!("session:{i}")).collect();
        assert_eq!(b.add(users.iter().cloned()), 600);
        assert_eq!(b.add(sessions), 300);
        b.add_unreadable(100);
        assert_eq!(b.names_read(), 1_000);
        // 1,000 names of 10,000 keys: scale 10.
        let p = b.patterns(10_000, false);
        let rows: Vec<(&str, u64, u64)> = p
            .iter()
            .map(|r| (r.pattern.as_str(), r.names_seen, r.est_keys))
            .collect();
        assert_eq!(
            rows,
            [("user:{id}:cart", 600, 6_000), ("session:{id}", 300, 3_000)]
        );
        assert_eq!(
            p[0].examples,
            [
                "user:0:cart",
                "user:100:cart",
                "user:101:cart",
                "user:102:cart",
                "user:103:cart"
            ]
        );
        // Every name read: as seen.
        let p = b.patterns(10_000, true);
        assert_eq!(p[0].est_keys, 600);
        // DBSIZE below the names read: never below what was seen.
        assert_eq!(b.patterns(10, false)[0].est_keys, 600);
        // Repeats count once.
        assert_eq!(b.add(users), 0);
        assert_eq!(b.names_read(), 1_000);
        assert!(b.contains("session:7") && !b.contains("session:7000"));
    }

    #[test]
    fn the_var_collapse_sees_names_from_every_page() {
        let mut b = NamePatterns::new();
        // 12 tenants, then 12 more: the collapse needs 20.
        b.add((0..12).map(|i| format!("cfg:t{i:02}x:flags")));
        assert_eq!(b.patterns(0, true).len(), 12);
        b.add((12..24).map(|i| format!("cfg:t{i:02}x:flags")));
        let p = b.patterns(0, true);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].pattern, "cfg:{var}:flags");
        assert_eq!(p[0].names_seen, 24);
        assert_eq!(p[0].examples.len(), 5);
        assert_eq!(p[0].examples[0], "cfg:t00x:flags");
    }

    #[test]
    fn nothing_read() {
        let b = NamePatterns::new();
        assert!(b.patterns(1_000, false).is_empty());
        assert_eq!(name_scale(1_000, 0, false), 1.0);
        assert_eq!(name_scale(1_000, 250, false), 4.0);
        assert_eq!(name_scale(1_000, 250, true), 1.0);
        assert_eq!(name_scale(100, 250, false), 1.0);
    }

    #[test]
    fn as_pattern_stats_holds_counts_only() {
        let mut b = NamePatterns::new();
        b.add(["a:1", "a:2"]);
        let s = b.patterns(20, false)[0].as_pattern_stats();
        assert_eq!((s.sampled_keys, s.est_keys), (2, 20));
        assert_eq!(s.key_type, None);
        assert_eq!(s.est_bytes, None);
        assert!(s.examples.is_empty());
    }

    #[test]
    fn examples_stay_the_first_five_by_name() {
        let mut v = Vec::new();
        for n in ["k9", "k3", "k7", "k1", "k5", "k2", "k8", "k3", "k0"] {
            keep_first(&mut v, n);
        }
        assert_eq!(v, ["k0", "k1", "k2", "k3", "k5"]);
    }

    fn sample(key: &str) -> KeySample {
        KeySample {
            key: key.into(),
            key_type: KeyType::String,
            bytes: None,
            ttl: Ttl::Persistent,
            lfu_freq: None,
            node: None,
        }
    }

    fn names() -> impl Strategy<Value = Vec<String>> {
        let one = prop_oneof![
            (0u32..500).prop_map(|n| format!("user:{n}")),
            (0u32..40).prop_map(|n| format!("cfg:t{n}x:flags")),
            "[a-c]{1,2}:[a-z]{1,3}",
            any::<String>(),
        ];
        prop::collection::vec(one, 0..200)
    }

    proptest! {
        /// Names give the patterns, counts and examples a scan of the same keys gives.
        #[test]
        fn names_infer_what_a_scan_infers(names in names()) {
            let mut b = NamePatterns::new();
            b.add(names.iter().cloned());
            let distinct: BTreeSet<&String> = names.iter().collect();
            let samples: Vec<KeySample> = distinct.iter().map(|k| sample(k)).collect();
            let mut want: Vec<(String, u64, Vec<String>)> = crate::pattern::aggregate(&samples, 1.0)
                .into_iter()
                .map(|p| (p.pattern, p.sampled_keys, p.examples.into_iter().map(|e| e.key).collect()))
                .collect();
            let mut got: Vec<(String, u64, Vec<String>)> = b
                .patterns(0, true)
                .into_iter()
                .map(|p| (p.pattern, p.names_seen, p.examples))
                .collect();
            want.sort();
            got.sort();
            prop_assert_eq!(got, want);
            prop_assert_eq!(b.names_read(), distinct.len() as u64);
        }

        /// Adding page by page gives what adding everything at once gives.
        #[test]
        fn pages_add_up(names in names(), cut in 0usize..200, dbsize in 0u64..1_000_000) {
            let cut = cut.min(names.len());
            let mut all = NamePatterns::new();
            all.add(names.iter().cloned());
            let mut paged = NamePatterns::new();
            paged.add(names[..cut].iter().cloned());
            paged.add(names[cut..].iter().cloned());
            prop_assert_eq!(paged.patterns(dbsize, false), all.patterns(dbsize, false));
        }

        /// Estimates add up to DBSIZE within rounding (half a key per pattern) when DBSIZE
        /// is at least the names read.
        #[test]
        fn estimates_add_up_to_dbsize(names in names(), extra in 0u64..1_000_000) {
            let mut b = NamePatterns::new();
            b.add(names.iter().cloned());
            let read = b.names_read();
            let dbsize = read + extra;
            let p = b.patterns(dbsize, false);
            let total: u64 = p.iter().map(|r| r.est_keys).sum();
            let slack = p.len() as u64;
            prop_assert!(total.abs_diff(if read == 0 { 0 } else { dbsize }) <= slack,
                "total {} dbsize {} patterns {}", total, dbsize, p.len());
        }
    }
}
