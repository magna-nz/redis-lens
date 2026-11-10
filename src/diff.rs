//! Comparing two scan reports of the same server.
//!
//! Owner: engine (wave 4). Contract only; bodies are stubs.

use crate::model::ScanReport;

/// Why a pattern's memory changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// Not in the older report.
    New,
    /// Not in the newer report.
    Gone,
    /// The key count changed more than the average size did.
    MoreKeys,
    FewerKeys,
    /// The average size changed more than the key count did.
    BiggerValues,
    SmallerValues,
    /// Changed by less than 1% and less than 64 KiB (or 1% of keys when sizes are unavailable).
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PatternDiff {
    pub pattern: String,
    pub keys_before: Option<u64>,
    pub keys_after: Option<u64>,
    /// `None` when that report had no sizes.
    pub bytes_before: Option<u64>,
    pub bytes_after: Option<u64>,
    /// `bytes_after - bytes_before`, treating a missing side as 0. `None` when either
    /// report lacks sizes.
    pub delta_bytes: Option<i64>,
    pub delta_keys: i64,
    pub cause: Cause,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReportDiff {
    pub before_taken_at_unix: i64,
    pub after_taken_at_unix: i64,
    pub used_memory_before: u64,
    pub used_memory_after: u64,
    /// Sorted by |delta_bytes| (or |delta_keys| without sizes), largest first, then pattern.
    pub patterns: Vec<PatternDiff>,
}

impl ReportDiff {
    pub fn pattern(&self, pattern: &str) -> Option<&PatternDiff> {
        self.patterns.iter().find(|p| p.pattern == pattern)
    }
}

/// Compare an older report with a newer one, matching patterns by name.
pub fn diff(_before: &ScanReport, _after: &ScanReport) -> ReportDiff {
    todo!("diff::diff")
}
