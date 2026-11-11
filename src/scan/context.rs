//! Exact server-side figures (`INFO keysizes`, `INFO keyspace`, `MEMORY STATS`) set
//! beside the sampled estimates. Contract stub for the quick-wins wave (agent C builds it).

use crate::model::ScanReport;

/// Plain-text lines summarising `report.server_context` for the `scan` table and the
/// reports, e.g. "The sample explains 82% of dataset.bytes". Empty when there is none.
pub fn context_summary(_report: &ScanReport) -> Vec<String> {
    Vec::new()
}
