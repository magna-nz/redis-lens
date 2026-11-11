//! The Patterns tree: grouping patterns under a shared first segment, filtering, and
//! the per-row summaries and verdicts the view shows. Pure data, no IO.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

use crate::model::{KeySample, KeyType, PatternStats};

/// The separators that split off the first segment, which names a group. Pattern
/// inference also splits on `.` (see [`same_up_to_var`]), but a group is the text
/// before the first of these.
const SEPARATORS: [char; 3] = [':', '/', '|'];

/// The placeholders pattern inference writes. `{var}` replaces a literal segment.
const PLACEHOLDERS: [&str; 6] = ["{id}", "{uuid}", "{ulid}", "{hash}", "{ip}", "{var}"];

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

/// How the Patterns view orders its rows. Groups sort by their totals and children
/// sort inside their group, by the same key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortBy {
    /// Estimated memory, largest first. Without sizes: estimated keys.
    #[default]
    Memory,
    /// Estimated keys, most first.
    Keys,
    /// Average size per sampled key, largest first; patterns without sizes last.
    AvgSize,
    /// Share of sampled keys without a TTL, largest first.
    NoTtl,
    /// Pattern (or group label), A to Z.
    Name,
}

impl SortBy {
    /// What the sort means for a scan: without sizes, memory and average size are
    /// unknown, so both sort by keys.
    pub fn effective(self, sizes: bool) -> Self {
        match self {
            Self::Memory | Self::AvgSize if !sizes => Self::Keys,
            s => s,
        }
    }

    /// The next sort for `o`: memory, keys, avg size, no-TTL %, name, then memory
    /// again. Without sizes memory and avg size are skipped: keys, no-TTL %, name.
    pub fn next(self, sizes: bool) -> Self {
        match (self.effective(sizes), sizes) {
            (Self::Memory, _) => Self::Keys,
            (Self::Keys, true) => Self::AvgSize,
            (Self::Keys, false) | (Self::AvgSize, _) => Self::NoTtl,
            (Self::NoTtl, _) => Self::Name,
            (Self::Name, true) => Self::Memory,
            (Self::Name, false) => Self::Keys,
        }
    }

    /// For "sorted by ...".
    pub fn label(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Keys => "keys",
            Self::AvgSize => "avg size",
            Self::NoTtl => "no-TTL %",
            Self::Name => "name",
        }
    }

    /// Name sorts A to Z; every other key largest first.
    pub fn ascending(self) -> bool {
        self == Self::Name
    }
}

/// What a row sorts by: a pattern's figures, or a group's totals as
/// [`summarize_group`] adds them up.
struct Rank<'a> {
    /// Estimated bytes with sizes, else estimated keys, as the share bar uses.
    weight: u64,
    keys: u64,
    avg: Option<u64>,
    no_ttl: u64,
    sampled: u64,
    name: &'a str,
}

impl<'a> Rank<'a> {
    fn of(p: &'a PatternStats, sizes: bool) -> Self {
        Self::sum(std::slice::from_ref(p).iter(), &p.pattern, sizes)
    }

    fn sum(
        kids: impl Iterator<Item = &'a PatternStats> + Clone,
        name: &'a str,
        sizes: bool,
    ) -> Self {
        let sampled: u64 = kids.clone().map(|k| k.sampled_keys).sum();
        let sampled_bytes = sum_opt(kids.clone().map(|k| k.sampled_bytes));
        Self {
            weight: kids.clone().map(|k| weight(k, sizes)).sum(),
            keys: kids.clone().map(|k| k.est_keys).sum(),
            avg: sampled_bytes.filter(|_| sampled > 0).map(|b| b / sampled),
            no_ttl: kids.map(|k| k.no_ttl).sum(),
            sampled,
            name,
        }
    }

    /// `self` before `other` gives `Less`. Ties go by name, A to Z.
    fn cmp(&self, other: &Self, sort: SortBy) -> Ordering {
        let primary = match sort {
            SortBy::Memory => other.weight.cmp(&self.weight),
            SortBy::Keys => other.keys.cmp(&self.keys),
            SortBy::AvgSize => other.avg.cmp(&self.avg),
            SortBy::NoTtl => other.no_ttl_cmp(self),
            SortBy::Name => Ordering::Equal,
        };
        primary.then_with(|| self.name.cmp(other.name))
    }

    /// Compares no-TTL ratios exactly. No sampled keys counts as 0.
    fn no_ttl_cmp(&self, other: &Self) -> Ordering {
        let frac = |r: &Self| {
            if r.sampled == 0 {
                (0u128, 1u128)
            } else {
                (u128::from(r.no_ttl), u128::from(r.sampled))
            }
        };
        let (an, ad) = frac(self);
        let (bn, bd) = frac(other);
        (an * bd).cmp(&(bn * ad))
    }
}

fn weight(p: &PatternStats, sizes: bool) -> u64 {
    if sizes {
        p.est_bytes.unwrap_or(0)
    } else {
        p.est_keys
    }
}

impl PatternTree {
    /// [`PatternTree::build_sorted`] by memory (or keys without sizes), largest first.
    pub fn build(patterns: &[PatternStats], sizes: bool) -> Self {
        Self::build_sorted(patterns, sizes, SortBy::Memory)
    }

    /// Group patterns: two or more with the same first segment go under a parent.
    /// Roots are ordered by `sort` (groups by their totals), children by `sort` inside
    /// their group; ties go by name, A to Z.
    pub fn build_sorted(patterns: &[PatternStats], sizes: bool, sort: SortBy) -> Self {
        let sort = sort.effective(sizes);
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
        let roots: Vec<Root> = keys
            .into_iter()
            .zip(members)
            .map(|(key, mut idx)| match key {
                Some(key) if idx.len() >= 2 => {
                    let names: Vec<&str> =
                        idx.iter().map(|&i| patterns[i].pattern.as_str()).collect();
                    let label = group_label(&names);
                    idx.sort_by(|&a, &b| {
                        Rank::of(&patterns[a], sizes).cmp(&Rank::of(&patterns[b], sizes), sort)
                    });
                    Root::Group(Group {
                        key: key.to_string(),
                        label,
                        children: idx,
                    })
                }
                _ => Root::Single(idx[0]),
            })
            .collect();
        let order: Vec<usize> = {
            let ranks: Vec<Rank<'_>> = roots
                .iter()
                .map(|r| match r {
                    Root::Single(i) => Rank::of(&patterns[*i], sizes),
                    Root::Group(g) => {
                        Rank::sum(g.children.iter().map(|&i| &patterns[i]), &g.label, sizes)
                    }
                })
                .collect();
            let mut order: Vec<usize> = (0..roots.len()).collect();
            // Stable, so rows that tie on every key keep report order.
            order.sort_by(|&a, &b| ranks[a].cmp(&ranks[b], sort));
            order
        };
        let mut slots: Vec<Option<Root>> = roots.into_iter().map(Some).collect();
        Self {
            roots: order.into_iter().filter_map(|i| slots[i].take()).collect(),
        }
    }

    /// The rows to show. A filter keeps patterns whose name contains it (ignoring
    /// case), and keeps a parent when its label or any child matches.
    pub fn visible(
        &self,
        patterns: &[PatternStats],
        collapsed: &Collapsed,
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

    /// The groups of the tree, in order.
    pub fn groups(&self) -> impl Iterator<Item = &Group> {
        self.roots.iter().filter_map(|r| match r {
            Root::Group(g) => Some(g),
            Root::Single(_) => None,
        })
    }

    /// What a row is, independent of where it sits: a group by its key, a pattern by
    /// its name. `None` when `row` is not of this tree.
    pub fn row_id(&self, patterns: &[PatternStats], row: Row) -> Option<RowId> {
        match row {
            Row::Group { root } => match self.roots.get(root)? {
                Root::Group(g) => Some(RowId::Group(g.key.clone())),
                Root::Single(i) => Some(RowId::Pattern(patterns.get(*i)?.pattern.clone())),
            },
            Row::Pattern { idx, .. } => Some(RowId::Pattern(patterns.get(idx)?.pattern.clone())),
        }
    }

    /// The patterns a row stands for: a group's children, or the pattern itself.
    pub fn row_members(&self, patterns: &[PatternStats], row: Row) -> Vec<String> {
        let name = |i: usize| patterns.get(i).map(|p| p.pattern.clone());
        match row {
            Row::Group { root } => match self.roots.get(root) {
                Some(Root::Group(g)) => g.children.iter().filter_map(|&i| name(i)).collect(),
                Some(Root::Single(i)) => name(*i).into_iter().collect(),
                None => Vec::new(),
            },
            Row::Pattern { idx, .. } => name(idx).into_iter().collect(),
        }
    }

    /// Where the row that was `id`, standing for `members`, is now among `rows` (the
    /// visible rows of this tree):
    ///
    /// - a pattern: its row; else the row of the group it is in (collapsed); else a
    ///   pattern or group that it merged into or split from through `{var}`;
    /// - a group: the group with the same key; else a group holding one of its
    ///   patterns, also through `{var}` (its key changed); else one of its patterns
    ///   shown on its own (the group dropped to a single pattern).
    ///
    /// `None` when none of these is visible.
    pub fn find(
        &self,
        patterns: &[PatternStats],
        rows: &[Row],
        id: &RowId,
        members: &[String],
    ) -> Option<usize> {
        let name = |i: usize| patterns.get(i).map_or("", |p| p.pattern.as_str());
        let kids = |root: usize| match self.roots.get(root) {
            Some(Root::Group(g)) => g.children.as_slice(),
            _ => &[],
        };
        let pattern_row = |pred: &dyn Fn(&str) -> bool| {
            rows.iter()
                .position(|r| matches!(*r, Row::Pattern { idx, .. } if pred(name(idx))))
        };
        let group_row = |pred: &dyn Fn(&str) -> bool| {
            rows.iter().position(
                |r| matches!(*r, Row::Group { root } if kids(root).iter().any(|&i| pred(name(i)))),
            )
        };
        let exact = |n: &str| members.iter().any(|m| m == n);
        let close = |n: &str| members.iter().any(|m| same_up_to_var(m, n));
        match id {
            RowId::Pattern(p) => {
                let is = |n: &str| n == p;
                pattern_row(&is)
                    .or_else(|| group_row(&is))
                    .or_else(|| pattern_row(&close))
                    .or_else(|| group_row(&close))
            }
            RowId::Group(key) => rows
                .iter()
                .position(|r| {
                    matches!(*r, Row::Group { root }
                        if matches!(self.roots.get(root), Some(Root::Group(g)) if g.key == *key))
                })
                .or_else(|| group_row(&exact))
                .or_else(|| group_row(&close))
                .or_else(|| pattern_row(&exact))
                .or_else(|| pattern_row(&close)),
        }
    }
}

/// A Patterns row by identity, so the selection can follow it when the tree is rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowId {
    /// A group, by its key ([`Group::key`]).
    Group(String),
    /// A pattern, by its name. Also a single pattern at the top level.
    Pattern(String),
}

/// Segments and separators as pattern inference splits them: on `:` `/` `|` and `.`.
fn inference_tokens(pattern: &str) -> Vec<&str> {
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

/// True when `a` and `b` are the same pattern except where one has `{var}` and the
/// other a literal segment: what pattern inference merges once a segment has 20
/// distinct values (`tenant.a1:cfg` and `tenant.{var}:cfg`). Equal patterns match too.
pub fn same_up_to_var(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (ta, tb) = (inference_tokens(a), inference_tokens(b));
    let literal = |t: &str| !t.is_empty() && !PLACEHOLDERS.contains(&t);
    ta.len() == tb.len()
        && ta
            .iter()
            .zip(&tb)
            .all(|(x, y)| x == y || (*x == "{var}" && literal(y)) || (*y == "{var}" && literal(x)))
}

/// The collapsed groups of the Patterns view. A group starts expanded.
///
/// A collapse is remembered by the group's key and by its member patterns. When the
/// tree is rebuilt (new partial results, the finished scan, another sort),
/// [`Collapsed::reconcile`] keeps a group collapsed when
///
/// 1. its key was collapsed, whatever patterns it holds now; or
/// 2. its key is new but one of its patterns is a member of a collapsed group whose
///    key is gone, equal or up to `{var}` ([`same_up_to_var`]). This is a group whose
///    key changed because a segment of its first part merged into `{var}`
///    (`tenant.a1:*` and `tenant.a2:*` become `tenant.{var}:*`), or split back out.
///
/// A collapsed group that is gone, or dropped to a single pattern, stays remembered by
/// key, so it is collapsed again if it comes back. One carried over by rule 2 is
/// forgotten under its old key: expanding the new group expands it for good.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collapsed {
    /// Group key to the patterns it held when last seen.
    groups: BTreeMap<String, Vec<String>>,
}

impl Collapsed {
    pub fn contains(&self, key: &str) -> bool {
        self.groups.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn collapse(&mut self, g: &Group, patterns: &[PatternStats]) {
        let members = g
            .children
            .iter()
            .filter_map(|&i| patterns.get(i).map(|p| p.pattern.clone()))
            .collect();
        self.groups.insert(g.key.clone(), members);
    }

    /// Expands the group `key`. False when it was not collapsed.
    pub fn expand(&mut self, key: &str) -> bool {
        self.groups.remove(key).is_some()
    }

    /// Applies the rules above to a rebuilt `tree`, and records each collapsed group's
    /// patterns as they are now.
    pub fn reconcile(&mut self, tree: &PatternTree, patterns: &[PatternStats]) {
        if self.groups.is_empty() {
            return;
        }
        let present: HashSet<&str> = tree.groups().map(|g| g.key.as_str()).collect();
        let old = std::mem::take(&mut self.groups);
        let mut carried: HashSet<&str> = HashSet::new();
        for g in tree.groups() {
            let members: Vec<String> = g
                .children
                .iter()
                .filter_map(|&i| patterns.get(i).map(|p| p.pattern.clone()))
                .collect();
            let from: Vec<&str> = old
                .iter()
                .filter(|(k, _)| !present.contains(k.as_str()))
                .filter(|(_, ms)| {
                    ms.iter()
                        .any(|m| members.iter().any(|n| same_up_to_var(m, n)))
                })
                .map(|(k, _)| k.as_str())
                .collect();
            if old.contains_key(&g.key) || !from.is_empty() {
                carried.extend(from);
                self.groups.insert(g.key.clone(), members);
            }
        }
        for (k, ms) in &old {
            if !present.contains(k.as_str()) && !carried.contains(k.as_str()) {
                self.groups.insert(k.clone(), ms.clone());
            }
        }
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
        let rows = tree.visible(&pats, &Collapsed::default(), "CART");
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

    /// `keys` sampled and estimated, `bytes` in total, `no_ttl` of them without a TTL.
    fn q(pattern: &str, keys: u64, bytes: Option<u64>, no_ttl: u64) -> PatternStats {
        PatternStats {
            sampled_bytes: bytes,
            est_bytes: bytes,
            no_ttl,
            ..p(pattern, KeyType::String, keys, 0)
        }
    }

    /// Each root's label (a group's label, or a pattern), with a group's children
    /// indented under it.
    fn order(tree: &PatternTree, pats: &[PatternStats]) -> Vec<String> {
        let mut out = Vec::new();
        for r in &tree.roots {
            match r {
                Root::Single(i) => out.push(pats[*i].pattern.clone()),
                Root::Group(g) => {
                    out.push(g.label.clone());
                    out.extend(g.children.iter().map(|&i| format!("  {}", pats[i].pattern)));
                }
            }
        }
        out
    }

    /// A group `a:*` of three small children whose total beats the singles, and
    /// singles that each beat every child on its own.
    fn sortable() -> Vec<PatternStats> {
        vec![
            // keys, bytes, no TTL
            q("big:{id}", 50, Some(5_000), 0), // avg 100, 0%
            q("a:x", 30, Some(1_200), 30),     // avg 40, 100%
            q("a:y", 20, Some(1_000), 5),      // avg 50, 25%
            q("a:z", 10, Some(2_000), 0),      // avg 200, 0%
            q("mid", 40, Some(4_000), 10),     // avg 100, 25%
        ]
    }

    #[test]
    fn sort_by_memory_groups_by_total_and_children_inside() {
        let pats = sortable();
        // big 5,000 > a:* 4,200 (summed) > mid 4,000; children by their own bytes.
        assert_eq!(
            order(
                &PatternTree::build_sorted(&pats, true, SortBy::Memory),
                &pats
            ),
            ["big:{id}", "a:*", "  a:z", "  a:x", "  a:y", "mid"]
        );
        // `build` is the memory sort.
        assert_eq!(
            PatternTree::build(&pats, true),
            PatternTree::build_sorted(&pats, true, SortBy::Memory)
        );
    }

    #[test]
    fn sort_by_keys() {
        let pats = sortable();
        assert_eq!(
            order(&PatternTree::build_sorted(&pats, true, SortBy::Keys), &pats),
            ["a:*", "  a:x", "  a:y", "  a:z", "big:{id}", "mid"]
        );
    }

    #[test]
    fn sort_by_avg_size() {
        let mut pats = sortable();
        // Without sizes a pattern sorts after every one with them.
        pats.push(q("nosize", 1000, None, 0));
        assert_eq!(
            order(
                &PatternTree::build_sorted(&pats, true, SortBy::AvgSize),
                &pats
            ),
            // big and mid tie at 100 and go by name; a:* is 4,200 / 60 = 70.
            [
                "big:{id}", "mid", "a:*", "  a:z", "  a:y", "  a:x", "nosize"
            ]
        );
    }

    #[test]
    fn sort_by_no_ttl() {
        let pats = sortable();
        assert_eq!(
            order(
                &PatternTree::build_sorted(&pats, true, SortBy::NoTtl),
                &pats
            ),
            // a:* is 35 / 60; mid 25%; big 0%. Children: 100%, 25%, 0%.
            ["a:*", "  a:x", "  a:y", "  a:z", "mid", "big:{id}"]
        );
    }

    #[test]
    fn no_ttl_ratios_compare_exactly() {
        // 1/3 against 333/1000: close as floats, different as fractions.
        let pats = vec![q("b", 1000, None, 333), q("a", 3, None, 1)];
        let tree = PatternTree::build_sorted(&pats, false, SortBy::NoTtl);
        assert_eq!(order(&tree, &pats), ["a", "b"]);
        // No sampled keys counts as 0%.
        let pats = vec![q("z", 0, None, 0), q("y", 10, None, 1)];
        let tree = PatternTree::build_sorted(&pats, false, SortBy::NoTtl);
        assert_eq!(order(&tree, &pats), ["y", "z"]);
    }

    #[test]
    fn sort_by_name_uses_group_labels() {
        let pats = sortable();
        assert_eq!(
            order(&PatternTree::build_sorted(&pats, true, SortBy::Name), &pats),
            ["a:*", "  a:x", "  a:y", "  a:z", "big:{id}", "mid"]
        );
    }

    #[test]
    fn ties_go_by_name() {
        // Equal on every figure, given in reverse name order.
        let pats = vec![
            q("c:{id}", 5, Some(50), 1),
            q("b:{id}:y", 5, Some(50), 1),
            q("b:{id}:x", 5, Some(50), 1),
            q("a:{id}", 10, Some(100), 2),
        ];
        for sort in [SortBy::Memory, SortBy::Keys, SortBy::AvgSize, SortBy::NoTtl] {
            let tree = PatternTree::build_sorted(&pats, true, sort);
            let got = order(&tree, &pats);
            // a:{id} and b:{id}:* tie on memory and keys (10, 100) and on avg and no TTL.
            assert_eq!(
                got,
                ["a:{id}", "b:{id}:*", "  b:{id}:x", "  b:{id}:y", "c:{id}"],
                "{sort:?}"
            );
        }
    }

    #[test]
    fn without_sizes_memory_and_avg_sort_by_keys() {
        let pats: Vec<PatternStats> = sortable()
            .into_iter()
            .map(|mut p| {
                p.est_bytes = None;
                p.sampled_bytes = None;
                p
            })
            .collect();
        let keys = PatternTree::build_sorted(&pats, false, SortBy::Keys);
        assert_eq!(
            PatternTree::build_sorted(&pats, false, SortBy::Memory),
            keys
        );
        assert_eq!(
            PatternTree::build_sorted(&pats, false, SortBy::AvgSize),
            keys
        );
        assert_eq!(PatternTree::build(&pats, false), keys);
    }

    #[test]
    fn sort_cycles() {
        let cycle = |sizes: bool| {
            let mut s = SortBy::default();
            let mut seen = vec![s.effective(sizes)];
            for _ in 0..5 {
                s = s.next(sizes);
                seen.push(s.effective(sizes));
            }
            seen
        };
        use SortBy::*;
        assert_eq!(cycle(true), [Memory, Keys, AvgSize, NoTtl, Name, Memory]);
        // Without sizes memory and avg size are skipped; memory starts as keys.
        assert_eq!(cycle(false), [Keys, NoTtl, Name, Keys, NoTtl, Name]);
        assert_eq!(AvgSize.next(false), NoTtl);
        assert!(Name.ascending() && !Memory.ascending());
        assert_eq!(NoTtl.label(), "no-TTL %");
    }

    #[test]
    fn var_matching() {
        assert!(same_up_to_var("tenant.a1:cfg", "tenant.{var}:cfg"));
        assert!(same_up_to_var("cfg:{var}:flags", "cfg:blue:flags"));
        assert!(same_up_to_var("p:{var}:{var}:{id}", "p:a1x:b2x:{id}"));
        assert!(same_up_to_var("a:b", "a:b"));
        // A placeholder is not a literal that {var} stands for.
        assert!(!same_up_to_var("cfg:{var}:flags", "cfg:{id}:flags"));
        assert!(!same_up_to_var("cfg:a:flags", "cfg:b:flags"));
        assert!(!same_up_to_var("cfg:{var}", "cfg:{var}:x"));
        assert!(!same_up_to_var("cfg:{var}", "cfg/a"));
    }

    fn names(v: &[&str]) -> Vec<PatternStats> {
        v.iter()
            .enumerate()
            .map(|(i, n)| q(n, 100 - i as u64, None, 0))
            .collect()
    }

    /// `collapsed` after collapsing the group `key` of `before`, then rebuilding from
    /// `after`.
    fn carry(
        before: &[&str],
        key: &str,
        after: &[&str],
    ) -> (Collapsed, PatternTree, Vec<PatternStats>) {
        let pats = names(before);
        let tree = PatternTree::build(&pats, false);
        let mut c = Collapsed::default();
        let g = tree.groups().find(|g| g.key == key).unwrap();
        c.collapse(g, &pats);
        let pats = names(after);
        let tree = PatternTree::build(&pats, false);
        c.reconcile(&tree, &pats);
        (c, tree, pats)
    }

    #[test]
    fn collapse_kept_when_the_group_gains_or_loses_members() {
        let (c, ..) = carry(
            &["user:{id}:cart", "user:{id}:feed"],
            "user",
            &["user:{id}:cart", "user:{id}:feed", "user:{id}:profile"],
        );
        assert!(c.contains("user"));
        let (c, ..) = carry(
            &["user:{id}:cart", "user:{id}:feed", "user:{id}:profile"],
            "user",
            &["user:{id}:cart", "user:{id}:profile"],
        );
        assert!(c.contains("user"));
    }

    #[test]
    fn a_single_that_gains_a_sibling_starts_expanded() {
        let pats = names(&["user:{id}:cart", "lock:{var}"]);
        let tree = PatternTree::build(&pats, false);
        assert!(tree.groups().next().is_none());
        let mut c = Collapsed::default();
        let pats = names(&["user:{id}:cart", "user:{id}:feed", "lock:{var}"]);
        let tree = PatternTree::build(&pats, false);
        c.reconcile(&tree, &pats);
        assert!(!c.contains("user"));
        assert_eq!(tree.visible(&pats, &c, "").len(), 4);
    }

    #[test]
    fn a_group_that_drops_to_a_single_is_remembered_by_key() {
        // Twenty values merge the two children into one {var} pattern: no group.
        let (mut c, tree, _) = carry(
            &["cfg:blue:flags", "cfg:red:flags"],
            "cfg",
            &["cfg:{var}:flags"],
        );
        assert!(tree.groups().next().is_none());
        assert!(c.contains("cfg"));
        // When it is a group again, it is collapsed again.
        let pats2 = names(&["cfg:{var}:flags", "cfg:{var}:rules"]);
        let tree2 = PatternTree::build(&pats2, false);
        c.reconcile(&tree2, &pats2);
        assert!(c.contains("cfg"));
        assert_eq!(tree2.visible(&pats2, &c, "").len(), 1);
    }

    #[test]
    fn a_group_whose_key_changes_through_var_stays_collapsed() {
        // `tenant.a1` and its siblings merge into `tenant.{var}` once 20 values show.
        let (c, tree, pats) = carry(
            &[
                "tenant.a1:cfg",
                "tenant.a1:flags",
                "tenant.a2:cfg",
                "tenant.a2:flags",
            ],
            "tenant.a1",
            &["tenant.{var}:cfg", "tenant.{var}:flags"],
        );
        assert!(c.contains("tenant.{var}"));
        // Carried over: the old key is forgotten.
        assert!(!c.contains("tenant.a1"));
        assert_eq!(c.len(), 1);
        assert_eq!(tree.visible(&pats, &c, "").len(), 1);
        // And back (a new scan that has seen fewer values): both splits are collapsed.
        let mut c = c;
        let pats = names(&[
            "tenant.a1:cfg",
            "tenant.a1:flags",
            "tenant.a2:cfg",
            "tenant.a2:flags",
        ]);
        let tree = PatternTree::build(&pats, false);
        c.reconcile(&tree, &pats);
        assert!(c.contains("tenant.a1") && c.contains("tenant.a2"));
        assert!(!c.contains("tenant.{var}"));
    }

    #[test]
    fn expanding_a_carried_group_forgets_it() {
        let (mut c, ..) = carry(
            &["tenant.a1:cfg", "tenant.a1:flags"],
            "tenant.a1",
            &["tenant.{var}:cfg", "tenant.{var}:flags"],
        );
        assert!(c.expand("tenant.{var}"));
        assert!(c.is_empty());
        assert!(!c.expand("tenant.{var}"));
        let pats = names(&["tenant.a1:cfg", "tenant.a1:flags"]);
        let tree = PatternTree::build(&pats, false);
        c.reconcile(&tree, &pats);
        assert!(!c.contains("tenant.a1"));
    }

    #[test]
    fn an_unrelated_new_group_starts_expanded() {
        let (c, ..) = carry(
            &["cfg:blue:flags", "cfg:red:flags"],
            "cfg",
            &["cfg:{var}:flags", "job:{id}:a", "job:{id}:b"],
        );
        assert!(!c.contains("job"));
        assert!(c.contains("cfg"));
    }

    #[test]
    fn find_follows_rows_by_identity() {
        let pats = names(&[
            "user:{id}:cart",
            "user:{id}:feed",
            "lock:{var}",
            "tenant.{var}:x",
            "tenant.{var}:y",
        ]);
        let tree = PatternTree::build(&pats, false);
        let mut c = Collapsed::default();
        let rows = tree.visible(&pats, &c, "");
        let at = |id: RowId, members: &[&str], rows: &[Row]| {
            let members: Vec<String> = members.iter().map(|m| m.to_string()).collect();
            tree.find(&pats, rows, &id, &members)
                .map(|i| tree.row_id(&pats, rows[i]).unwrap())
        };
        let pat = |n: &str| RowId::Pattern(n.into());
        let grp = |n: &str| RowId::Group(n.into());
        // The pattern itself.
        assert_eq!(
            at(pat("user:{id}:feed"), &["user:{id}:feed"], &rows),
            Some(pat("user:{id}:feed"))
        );
        // A group by key.
        assert_eq!(at(grp("user"), &[], &rows), Some(grp("user")));
        // A pattern inside a collapsed group: the group.
        let g = tree.groups().find(|g| g.key == "user").unwrap().clone();
        c.collapse(&g, &pats);
        let folded = tree.visible(&pats, &c, "");
        assert_eq!(
            at(pat("user:{id}:feed"), &["user:{id}:feed"], &folded),
            Some(grp("user"))
        );
        // A pattern that merged into {var}.
        assert_eq!(
            at(pat("tenant.b7:y"), &["tenant.b7:y"], &rows),
            Some(pat("tenant.{var}:y"))
        );
        // A group whose key changed through {var}.
        assert_eq!(
            at(grp("tenant.b7"), &["tenant.b7:x", "tenant.b7:y"], &rows),
            Some(grp("tenant.{var}"))
        );
        // A group that dropped to a single pattern.
        assert_eq!(
            at(grp("lock"), &["lock:{var}"], &rows),
            Some(pat("lock:{var}"))
        );
        // Gone.
        assert_eq!(at(pat("gone:{id}"), &["gone:{id}"], &rows), None);
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
