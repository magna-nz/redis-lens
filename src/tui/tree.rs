//! The Patterns tree: grouping patterns under a shared first segment, filtering, and
//! the per-row summaries and verdicts the view shows. Pure data, no IO.

use std::collections::HashSet;

use crate::model::{KeySample, KeyType, PatternStats};

/// Same separators as pattern inference.
const SEPARATORS: [char; 3] = [':', '/', '|'];

/// A key at or above this size is "very large".
pub const LARGE_MAX: u64 = 50 * 1024 * 1024;
/// An average at or above this size is "very large".
pub const LARGE_AVG: u64 = 10 * 1024 * 1024;

/// The text before the first separator, or `None` when there is no separator.
pub fn first_segment(pattern: &str) -> Option<&str> {
    pattern.find(SEPARATORS).map(|i| &pattern[..i])
}

/// Split into segments and separators, keeping both: `a:b` -> `["a", ":", "b"]`.
fn tokens(pattern: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in pattern.char_indices() {
        if SEPARATORS.contains(&c) {
            if start < i {
                out.push(&pattern[start..i]);
            }
            out.push(&pattern[i..i + c.len_utf8()]);
            start = i + c.len_utf8();
        }
    }
    if start < pattern.len() {
        out.push(&pattern[start..]);
    }
    out
}

/// The longest shared run of segments, followed by `*`: `user:{id}:cart` and
/// `user:{id}:feed` give `user:{id}:*`.
fn group_label(patterns: &[&str]) -> String {
    let lists: Vec<Vec<&str>> = patterns.iter().map(|p| tokens(p)).collect();
    let Some(first) = lists.first() else {
        return "*".to_string();
    };
    let mut n = first.len();
    for l in &lists[1..] {
        n = n.min(l.len());
        n = (0..n).find(|&i| first[i] != l[i]).unwrap_or(n);
    }
    let mut label: String = first[..n].concat();
    label.push('*');
    label
}

/// Patterns sharing a first segment.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The shared first segment. Identifies the group for expand and collapse.
    pub key: String,
    /// What the row shows, e.g. `user:{id}:*`.
    pub label: String,
    /// Indices into the report's patterns, in report order.
    pub children: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Root {
    Single(usize),
    Group(Group),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PatternTree {
    pub roots: Vec<Root>,
}

/// One visible row of the Patterns table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// A parent row. `root` indexes [`PatternTree::roots`].
    Group { root: usize },
    /// A pattern. `idx` indexes the report's patterns; `parent` is the root index of
    /// its group, if it has one.
    Pattern { idx: usize, parent: Option<usize> },
}

fn weight(p: &PatternStats, sizes: bool) -> u64 {
    if sizes {
        p.est_bytes.unwrap_or(0)
    } else {
        p.est_keys
    }
}

impl PatternTree {
    /// Group patterns: two or more with the same first segment go under a parent.
    /// Roots are ordered by memory (or keys without sizes), largest first.
    pub fn build(patterns: &[PatternStats], sizes: bool) -> Self {
        let mut keys: Vec<Option<&str>> = Vec::new();
        let mut members: Vec<Vec<usize>> = Vec::new();
        for (i, p) in patterns.iter().enumerate() {
            match first_segment(&p.pattern) {
                Some(seg) => match keys.iter().position(|k| *k == Some(seg)) {
                    Some(at) => members[at].push(i),
                    None => {
                        keys.push(Some(seg));
                        members.push(vec![i]);
                    }
                },
                None => {
                    keys.push(None);
                    members.push(vec![i]);
                }
            }
        }
        let mut roots: Vec<(u64, Root)> = keys
            .into_iter()
            .zip(members)
            .map(|(key, idx)| {
                let w = idx.iter().map(|&i| weight(&patterns[i], sizes)).sum();
                let root = match key {
                    Some(key) if idx.len() >= 2 => {
                        let names: Vec<&str> =
                            idx.iter().map(|&i| patterns[i].pattern.as_str()).collect();
                        Root::Group(Group {
                            key: key.to_string(),
                            label: group_label(&names),
                            children: idx,
                        })
                    }
                    _ => Root::Single(idx[0]),
                };
                (w, root)
            })
            .collect();
        roots.sort_by_key(|r| std::cmp::Reverse(r.0));
        Self {
            roots: roots.into_iter().map(|(_, r)| r).collect(),
        }
    }

    /// The rows to show. A filter keeps patterns whose name contains it (ignoring
    /// case), and keeps a parent when its label or any child matches.
    pub fn visible(
        &self,
        patterns: &[PatternStats],
        collapsed: &HashSet<String>,
        filter: &str,
    ) -> Vec<Row> {
        let f = filter.to_lowercase();
        let matches = |s: &str| f.is_empty() || s.to_lowercase().contains(&f);
        let mut out = Vec::new();
        for (ri, root) in self.roots.iter().enumerate() {
            match root {
                Root::Single(i) => {
                    if matches(&patterns[*i].pattern) {
                        out.push(Row::Pattern {
                            idx: *i,
                            parent: None,
                        });
                    }
                }
                Root::Group(g) => {
                    let kids: Vec<usize> = g
                        .children
                        .iter()
                        .copied()
                        .filter(|&i| matches(&patterns[i].pattern))
                        .collect();
                    let self_match = matches(&g.label);
                    if !self_match && kids.is_empty() {
                        continue;
                    }
                    out.push(Row::Group { root: ri });
                    if collapsed.contains(&g.key) {
                        continue;
                    }
                    let show = if kids.is_empty() { &g.children } else { &kids };
                    out.extend(show.iter().map(|&i| Row::Pattern {
                        idx: i,
                        parent: Some(ri),
                    }));
                }
            }
        }
        out
    }
}

/// What a row and the detail panel show, for a single pattern or a whole group.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub label: String,
    /// `None` means mixed types.
    pub key_type: Option<KeyType>,
    pub est_keys: u64,
    pub est_bytes: Option<u64>,
    pub sampled_keys: u64,
    pub sampled_bytes: Option<u64>,
    pub no_ttl: u64,
    pub ttl_hist: [u64; 7],
    pub size_p50: Option<u64>,
    pub size_p99: Option<u64>,
    pub size_max: Option<u64>,
    pub examples: Vec<KeySample>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Between 5% and 95% of keys have no TTL. Holds the ratio.
    MixedTtl(f64),
    /// Very large keys. Holds the largest size seen.
    Large(u64),
    Ok,
    /// A parent row: how many children there are and how many have a warning.
    Group {
        children: usize,
        warnings: usize,
    },
}

impl Verdict {
    pub fn is_warning(self) -> bool {
        matches!(self, Self::MixedTtl(_) | Self::Large(_))
    }
}

fn ratio(n: u64, d: u64) -> f64 {
    if d == 0 { 0.0 } else { n as f64 / d as f64 }
}

impl Summary {
    pub fn no_ttl_ratio(&self) -> f64 {
        ratio(self.no_ttl, self.sampled_keys)
    }

    /// Average size per key, from the sample.
    pub fn avg_bytes(&self) -> Option<u64> {
        match self.sampled_bytes {
            Some(b) if self.sampled_keys > 0 => Some(b / self.sampled_keys),
            _ => None,
        }
    }

    pub fn is_group(&self) -> bool {
        matches!(self.verdict, Verdict::Group { .. })
    }
}

/// The verdict for one pattern.
pub fn verdict(p: &PatternStats) -> Verdict {
    let r = p.no_ttl_ratio();
    if p.sampled_keys > 0 && (0.05..=0.95).contains(&r) {
        return Verdict::MixedTtl(r);
    }
    let avg = match p.sampled_bytes {
        Some(b) if p.sampled_keys > 0 => Some(b / p.sampled_keys),
        _ => None,
    };
    let big_max = p.size_max.is_some_and(|m| m >= LARGE_MAX);
    let big_avg = avg.is_some_and(|a| a >= LARGE_AVG);
    if big_max || big_avg {
        return Verdict::Large(p.size_max.or(avg).unwrap_or(0));
    }
    Verdict::Ok
}

pub fn summarize(p: &PatternStats) -> Summary {
    Summary {
        label: p.pattern.clone(),
        key_type: p.key_type,
        est_keys: p.est_keys,
        est_bytes: p.est_bytes,
        sampled_keys: p.sampled_keys,
        sampled_bytes: p.sampled_bytes,
        no_ttl: p.no_ttl,
        ttl_hist: p.ttl_hist,
        size_p50: p.size_p50,
        size_p99: p.size_p99,
        size_max: p.size_max,
        examples: p.examples.clone(),
        verdict: verdict(p),
    }
}

fn sum_opt(it: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    it.fold(None, |acc, v| match (acc, v) {
        (None, v) => v,
        (a, None) => a,
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
    })
}

/// Sums a group's children. Percentiles can't be merged, so p50 and p99 are `None`.
pub fn summarize_group(g: &Group, patterns: &[PatternStats]) -> Summary {
    let kids: Vec<&PatternStats> = g.children.iter().map(|&i| &patterns[i]).collect();
    let first_type = kids.first().and_then(|k| k.key_type);
    let key_type = if kids.iter().all(|k| k.key_type == first_type) {
        first_type
    } else {
        None
    };
    let mut ttl_hist = [0u64; 7];
    for k in &kids {
        for (h, v) in ttl_hist.iter_mut().zip(k.ttl_hist) {
            *h += v;
        }
    }
    let mut examples: Vec<KeySample> = kids.iter().flat_map(|k| k.examples.clone()).collect();
    examples.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    examples.truncate(5);
    Summary {
        label: g.label.clone(),
        key_type,
        est_keys: kids.iter().map(|k| k.est_keys).sum(),
        est_bytes: sum_opt(kids.iter().map(|k| k.est_bytes)),
        sampled_keys: kids.iter().map(|k| k.sampled_keys).sum(),
        sampled_bytes: sum_opt(kids.iter().map(|k| k.sampled_bytes)),
        no_ttl: kids.iter().map(|k| k.no_ttl).sum(),
        ttl_hist,
        size_p50: None,
        size_p99: None,
        size_max: kids.iter().filter_map(|k| k.size_max).max(),
        examples,
        verdict: Verdict::Group {
            children: kids.len(),
            warnings: kids.iter().filter(|k| verdict(k).is_warning()).count(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pattern: &str, t: KeyType, keys: u64, bytes: u64) -> PatternStats {
        PatternStats {
            pattern: pattern.into(),
            key_type: Some(t),
            sampled_keys: keys,
            est_keys: keys,
            sampled_bytes: Some(bytes),
            est_bytes: Some(bytes),
            no_ttl: 0,
            ttl_hist: [0, 0, 0, keys, 0, 0, 0],
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: vec![],
        }
    }

    #[test]
    fn first_segments() {
        assert_eq!(first_segment("user:{id}:cart"), Some("user"));
        assert_eq!(first_segment("a/b"), Some("a"));
        assert_eq!(first_segment("plain"), None);
    }

    #[test]
    fn labels() {
        assert_eq!(
            group_label(&["user:{id}:cart", "user:{id}:feed"]),
            "user:{id}:*"
        );
        assert_eq!(
            group_label(&["cache:search:{hash}", "cache:product:{id}"]),
            "cache:*"
        );
    }

    #[test]
    fn groups_and_orders() {
        let pats = vec![
            p("session:{uuid}", KeyType::String, 10, 600),
            p("user:{id}:cart", KeyType::Hash, 5, 300),
            p("user:{id}:feed", KeyType::ZSet, 5, 250),
            p("lock:{var}", KeyType::String, 1, 10),
        ];
        let tree = PatternTree::build(&pats, true);
        assert_eq!(tree.roots.len(), 3);
        assert_eq!(tree.roots[0], Root::Single(0));
        assert!(matches!(tree.roots[1], Root::Group(ref g) if g.key == "user"));
        assert_eq!(tree.roots[2], Root::Single(3));
        let Root::Group(g) = &tree.roots[1] else {
            unreachable!()
        };
        let s = summarize_group(g, &pats);
        assert_eq!(s.est_keys, 10);
        assert_eq!(s.est_bytes, Some(550));
        assert_eq!(s.key_type, None);
    }

    #[test]
    fn filter_keeps_parents_of_matches() {
        let pats = vec![
            p("user:{id}:cart", KeyType::Hash, 5, 300),
            p("user:{id}:feed", KeyType::Hash, 5, 250),
            p("lock:{var}", KeyType::String, 1, 10),
        ];
        let tree = PatternTree::build(&pats, true);
        let rows = tree.visible(&pats, &HashSet::new(), "CART");
        assert_eq!(
            rows,
            vec![
                Row::Group { root: 0 },
                Row::Pattern {
                    idx: 0,
                    parent: Some(0)
                }
            ]
        );
    }

    #[test]
    fn verdicts() {
        let mut s = p("a:{id}", KeyType::Hash, 100, 1000);
        assert_eq!(verdict(&s), Verdict::Ok);
        s.no_ttl = 31;
        assert_eq!(verdict(&s), Verdict::MixedTtl(0.31));
        s.no_ttl = 100;
        s.size_max = Some(LARGE_MAX);
        assert_eq!(verdict(&s), Verdict::Large(LARGE_MAX));
        s.size_max = Some(1);
        s.sampled_bytes = Some(100 * LARGE_AVG);
        assert!(matches!(verdict(&s), Verdict::Large(_)));
    }
}
