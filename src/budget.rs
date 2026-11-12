//! The command budget one connected profile shares between its scan and its live views
//! (spec "Shared command budget"). Owner: wave A1.
//!
//! Contract: the signatures below are fixed. Replace the bodies and the placeholder field.

use std::sync::Arc;

/// Commands per second for one connected profile. Everything that sends commands for
/// that profile (the scan, the live connection's polls and lookups) takes from the same
/// `Budget`, so together they stay at or below `per_sec` on average.
#[derive(Debug)]
pub struct Budget {
    _todo: (),
}

impl Budget {
    /// `None` means no limit.
    pub fn new(per_sec: Option<u32>) -> Arc<Self> {
        let _ = per_sec;
        todo!("wave A1")
    }

    /// The limit, `None` for none.
    pub fn per_sec(&self) -> Option<u32> {
        todo!("wave A1")
    }

    /// Waits until `n` more commands fit the budget, then counts them as sent. A pipeline
    /// of `n` commands takes `n`.
    pub async fn take(&self, n: u32) {
        let _ = n;
        todo!("wave A1")
    }
}
