//! TTL risk verdicts per pattern, shared by the TUI's TTL view and the CLI.
//!
//! Owner: engine (wave 4). Contract only; bodies are stubs.

use crate::diff::PatternDiff;
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
pub fn ttl_verdict(_stats: &PatternStats, _change: Option<&PatternDiff>) -> TtlVerdict {
    todo!("verdict::ttl_verdict")
}

/// One or two plain sentences explaining the verdict for the detail panel.
pub fn explain(_stats: &PatternStats, _change: Option<&PatternDiff>) -> Vec<String> {
    todo!("verdict::explain")
}
