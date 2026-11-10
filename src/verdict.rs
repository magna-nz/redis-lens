//! TTL risk verdicts per pattern, shared by the TUI's TTL view and the CLI.

use crate::diff::{Cause, PatternDiff};
use crate::format;
use crate::model::PatternStats;

/// Ordered from most to least worrying; `Ord` follows this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TtlVerdict {
    /// Persistent keys that grew since the previous snapshot: a likely leak.
    PersistentGrowing,
    /// Between 5% and 95% of keys have no TTL: a write path probably skips EXPIRE.
    MixedLikelyBug,
    /// Nothing expires and the pattern is stable or there is no history: probably intentional.
    PersistentStable,
    /// Everything expires within a minute.
    ShortLived,
    Ok,
}

impl TtlVerdict {
    /// Short label for tables, e.g. `mixed · likely bug`.
    pub fn label(self) -> &'static str {
        match self {
            Self::PersistentGrowing => "persistent · growing",
            Self::MixedLikelyBug => "mixed · likely bug",
            Self::PersistentStable => "persistent · stable",
            Self::ShortLived => "ok · short-lived",
            Self::Ok => "ok",
        }
    }

    /// True for verdicts the UI flags in red.
    pub fn is_warning(self) -> bool {
        matches!(self, Self::PersistentGrowing | Self::MixedLikelyBug)
    }
}

/// Judge one pattern. `change` is its entry in a diff against the previous snapshot,
/// if one exists. "Growing" means `delta_bytes` > max(1 MiB, 5% of before), or, without
/// sizes, `delta_keys` > 5% of the keys before; `Cause::New` with no TTL also counts.
/// `Cause::Unknown` (the other scan didn't look at these keys) never counts as growing.
pub fn ttl_verdict(stats: &PatternStats, change: Option<&PatternDiff>) -> TtlVerdict {
    let ratio = stats.no_ttl_ratio();
    if ratio >= PERSISTENT {
        if is_growing(change) {
            TtlVerdict::PersistentGrowing
        } else {
            TtlVerdict::PersistentStable
        }
    } else if ratio >= MIXED {
        TtlVerdict::MixedLikelyBug
    } else if short_lived(stats) {
        TtlVerdict::ShortLived
    } else {
        TtlVerdict::Ok
    }
}

/// At least this share of keys with no TTL counts as persistent.
const PERSISTENT: f64 = 0.95;
/// At least this share with no TTL (and less than [`PERSISTENT`]) counts as mixed.
const MIXED: f64 = 0.05;

fn short_lived(stats: &PatternStats) -> bool {
    stats.sampled_keys > 0 && stats.ttl_hist[1] as f64 / stats.sampled_keys as f64 >= 0.95
}

fn is_growing(change: Option<&PatternDiff>) -> bool {
    let Some(c) = change else { return false };
    match c.cause {
        Cause::New => return true,
        Cause::Unknown => return false,
        _ => {}
    }
    match c.delta_bytes {
        Some(d) => {
            let threshold = (c.bytes_before.unwrap_or(0) as f64 * 0.05).max(1024.0 * 1024.0);
            d as f64 > threshold
        }
        None => c.delta_keys as f64 > c.keys_before.unwrap_or(0) as f64 * 0.05,
    }
}

/// How long the keys that do expire live, in words, judged by the most common TTL bucket.
fn typical_ttl(stats: &PatternStats) -> Option<&'static str> {
    const WORDS: [&str; 6] = [
        "under a minute",
        "under an hour",
        "about a day",
        "about 7 days",
        "about 30 days",
        "more than 30 days",
    ];
    let (idx, count) = stats.ttl_hist[1..]
        .iter()
        .enumerate()
        .max_by_key(|(i, n)| (**n, std::cmp::Reverse(*i)))?;
    (*count > 0).then(|| WORDS[idx])
}

fn growth(change: Option<&PatternDiff>) -> Option<String> {
    let c = change?;
    if c.cause == Cause::New {
        return Some("It is new since the last snapshot.".to_string());
    }
    match c.delta_bytes {
        Some(d) if is_growing(change) => Some(format!(
            "It grew by {} since the last snapshot.",
            format::bytes(d as u64)
        )),
        None if is_growing(change) => Some(format!(
            "It gained {} keys since the last snapshot.",
            format::grouped(c.delta_keys as u64)
        )),
        _ => None,
    }
}

/// "All 100 keys never expire" when every sampled key has no TTL, otherwise
/// "97% of the 100 keys never expire". The share is capped at 99% below 100%.
fn persistent_share(stats: &PatternStats, keys: &str) -> String {
    if stats.sampled_keys > 0 && stats.no_ttl >= stats.sampled_keys {
        format!("All {keys} keys never expire")
    } else {
        let share = format::percent(stats.no_ttl_ratio().min(0.99));
        format!("{share} of the {keys} keys never expire")
    }
}

/// One or two plain sentences explaining the verdict for the detail panel.
pub fn explain(stats: &PatternStats, change: Option<&PatternDiff>) -> Vec<String> {
    let keys = format::count(stats.est_keys);
    let never = format::percent(stats.no_ttl_ratio());
    match ttl_verdict(stats, change) {
        TtlVerdict::PersistentGrowing => {
            let mut v = vec![format!(
                "{} and the pattern is growing.",
                persistent_share(stats, &keys)
            )];
            v.extend(growth(change));
            v.push("This is probably a leak: nothing ever removes these keys.".to_string());
            v
        }
        TtlVerdict::MixedLikelyBug => {
            let rest = match typical_ttl(stats) {
                Some(t) => format!("the rest expire in {t}"),
                None => "the rest expire".to_string(),
            };
            vec![
                format!("{never} of {keys} keys never expire, while {rest}."),
                "A write path probably skips EXPIRE.".to_string(),
            ]
        }
        TtlVerdict::PersistentStable => {
            let history = match change.map(|c| c.cause) {
                Some(Cause::Unknown) => "was not in the earlier scan, which used a different MATCH",
                Some(_) => "is not growing",
                None => "has no earlier snapshot to compare with",
            };
            vec![
                format!(
                    "{} and the pattern {history}.",
                    persistent_share(stats, &keys)
                ),
                "This looks intentional.".to_string(),
            ]
        }
        TtlVerdict::ShortLived => vec![format!(
            "Nearly all of the {keys} keys expire within a minute."
        )],
        TtlVerdict::Ok => {
            let mut s = if stats.no_ttl == 0 {
                format!("All {keys} keys expire")
            } else {
                format!("{never} of {keys} keys never expire")
            };
            if let Some(t) = typical_ttl(stats) {
                s.push_str(&format!(", typically in {t}"));
            }
            s.push('.');
            vec![s]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn stats(sampled: u64, no_ttl: u64, hist: [u64; 7]) -> PatternStats {
        PatternStats {
            pattern: "p".into(),
            key_type: None,
            sampled_keys: sampled,
            est_keys: sampled,
            sampled_bytes: None,
            est_bytes: None,
            no_ttl,
            ttl_hist: hist,
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: vec![],
        }
    }

    fn change(cause: Cause, before: Option<u64>, delta: Option<i64>, dkeys: i64) -> PatternDiff {
        PatternDiff {
            pattern: "p".into(),
            keys_before: Some(1000),
            keys_after: Some(1000 + dkeys.max(0) as u64),
            bytes_before: before,
            bytes_after: None,
            delta_bytes: delta,
            delta_keys: dkeys,
            cause,
        }
    }

    fn persistent() -> PatternStats {
        stats(100, 100, [100, 0, 0, 0, 0, 0, 0])
    }

    #[test]
    fn persistent_without_history_is_stable() {
        assert_eq!(
            ttl_verdict(&persistent(), None),
            TtlVerdict::PersistentStable
        );
        // 95% is the threshold.
        let s = stats(100, 95, [95, 5, 0, 0, 0, 0, 0]);
        assert_eq!(ttl_verdict(&s, None), TtlVerdict::PersistentStable);
    }

    #[test]
    fn persistent_growth_by_bytes() {
        let big = change(Cause::MoreKeys, Some(100 * MB), Some(6 * MB as i64), 10);
        assert_eq!(
            ttl_verdict(&persistent(), Some(&big)),
            TtlVerdict::PersistentGrowing
        );
        // Under 5% of before.
        let small = change(Cause::MoreKeys, Some(100 * MB), Some(4 * MB as i64), 10);
        assert_eq!(
            ttl_verdict(&persistent(), Some(&small)),
            TtlVerdict::PersistentStable
        );
        // Under 1 MiB.
        let tiny = change(Cause::MoreKeys, Some(MB), Some(MB as i64 / 2), 10);
        assert_eq!(
            ttl_verdict(&persistent(), Some(&tiny)),
            TtlVerdict::PersistentStable
        );
        // Shrinking.
        let shrink = change(
            Cause::FewerKeys,
            Some(100 * MB),
            Some(-(20 * MB as i64)),
            -5,
        );
        assert_eq!(
            ttl_verdict(&persistent(), Some(&shrink)),
            TtlVerdict::PersistentStable
        );
    }

    #[test]
    fn persistent_growth_by_keys_without_sizes() {
        let grew = change(Cause::MoreKeys, None, None, 60);
        assert_eq!(
            ttl_verdict(&persistent(), Some(&grew)),
            TtlVerdict::PersistentGrowing
        );
        let flat = change(Cause::Unchanged, None, None, 50);
        assert_eq!(
            ttl_verdict(&persistent(), Some(&flat)),
            TtlVerdict::PersistentStable
        );
    }

    #[test]
    fn persistent_new_is_growing() {
        let mut new = change(Cause::New, None, Some(1000), 100);
        new.keys_before = None;
        assert_eq!(
            ttl_verdict(&persistent(), Some(&new)),
            TtlVerdict::PersistentGrowing
        );
        assert!(
            explain(&persistent(), Some(&new))
                .join(" ")
                .contains("new since the last snapshot")
        );
    }

    #[test]
    fn mixed_and_boundaries() {
        let s = stats(100, 31, [31, 0, 0, 0, 69, 0, 0]);
        assert_eq!(ttl_verdict(&s, None), TtlVerdict::MixedLikelyBug);
        let s = stats(100, 5, [5, 0, 0, 0, 95, 0, 0]);
        assert_eq!(ttl_verdict(&s, None), TtlVerdict::MixedLikelyBug);
        let s = stats(100, 4, [4, 0, 0, 0, 96, 0, 0]);
        assert_eq!(ttl_verdict(&s, None), TtlVerdict::Ok);
        let s = stats(100, 94, [94, 0, 0, 0, 6, 0, 0]);
        assert_eq!(ttl_verdict(&s, None), TtlVerdict::MixedLikelyBug);
    }

    #[test]
    fn short_lived_and_ok() {
        let s = stats(100, 0, [0, 95, 5, 0, 0, 0, 0]);
        assert_eq!(ttl_verdict(&s, None), TtlVerdict::ShortLived);
        let s = stats(100, 0, [0, 94, 6, 0, 0, 0, 0]);
        assert_eq!(ttl_verdict(&s, None), TtlVerdict::Ok);
        let s = stats(0, 0, [0; 7]);
        assert_eq!(ttl_verdict(&s, None), TtlVerdict::Ok);
    }

    #[test]
    fn ordering_and_flags() {
        assert!(TtlVerdict::PersistentGrowing < TtlVerdict::MixedLikelyBug);
        assert!(TtlVerdict::ShortLived < TtlVerdict::Ok);
        assert!(TtlVerdict::MixedLikelyBug.is_warning());
        assert!(!TtlVerdict::PersistentStable.is_warning());
    }

    #[test]
    fn all_only_at_one_hundred_percent() {
        let s = stats(100, 97, [97, 0, 0, 0, 3, 0, 0]);
        let t = explain(&s, None).join(" ");
        assert!(
            t.starts_with("97% of the 100 keys never expire and the pattern has no earlier"),
            "{t}"
        );
        // 999 of 1000 rounds to 100%, but it isn't all of them.
        let s = stats(1000, 999, [999, 1, 0, 0, 0, 0, 0]);
        let t = explain(&s, None).join(" ");
        assert!(t.starts_with("99% of the 1.0K keys never expire"), "{t}");
        let grew = change(Cause::MoreKeys, None, None, 60);
        let t = explain(&s, Some(&grew)).join(" ");
        assert!(
            t.starts_with("99% of the 1.0K keys never expire and the pattern is growing."),
            "{t}"
        );
        let t = explain(&persistent(), None).join(" ");
        assert!(t.starts_with("All 100 keys never expire"), "{t}");
    }

    #[test]
    fn unknown_is_never_growing() {
        let mut unsure = change(Cause::Unknown, None, Some(50 * MB as i64), 1000);
        unsure.keys_before = None;
        assert_eq!(
            ttl_verdict(&persistent(), Some(&unsure)),
            TtlVerdict::PersistentStable
        );
        let t = explain(&persistent(), Some(&unsure)).join(" ");
        assert!(!t.contains("grew") && !t.contains("new since"), "{t}");
        assert!(t.contains("different MATCH"), "{t}");
    }

    #[test]
    fn explains_mixed() {
        let mut s = stats(8000, 2480, [2480, 0, 0, 0, 5520, 0, 0]);
        s.est_keys = 8000;
        let text = explain(&s, None);
        assert_eq!(
            text,
            [
                "31% of 8.0K keys never expire, while the rest expire in about 7 days.",
                "A write path probably skips EXPIRE."
            ]
        );
    }

    #[test]
    fn explains_every_verdict() {
        let growing = change(Cause::MoreKeys, Some(100 * MB), Some(6 * MB as i64), 10);
        let t = explain(&persistent(), Some(&growing)).join(" ");
        assert!(
            t.contains("growing") && t.contains("grew by 6.0M") && t.contains("leak"),
            "{t}"
        );
        let t = explain(&persistent(), None).join(" ");
        assert!(
            t.contains("no earlier snapshot") && t.contains("intentional"),
            "{t}"
        );
        let flat = change(Cause::Unchanged, Some(MB), Some(0), 0);
        let t = explain(&persistent(), Some(&flat)).join(" ");
        assert!(t.contains("not growing"), "{t}");
        let t = explain(&stats(100, 0, [0, 100, 0, 0, 0, 0, 0]), None).join(" ");
        assert!(t.contains("within a minute"), "{t}");
        let t = explain(&stats(100, 2, [2, 0, 0, 98, 0, 0, 0]), None).join(" ");
        assert_eq!(t, "2% of 100 keys never expire, typically in about a day.");
        let t = explain(&stats(100, 0, [0, 0, 0, 0, 0, 0, 100]), None).join(" ");
        assert_eq!(t, "All 100 keys expire, typically in more than 30 days.");
    }
}
