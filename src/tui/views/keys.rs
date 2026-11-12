//! State of the key list: the keys of one pattern, opened with `enter` on a pattern on
//! Patterns (spec "1 Patterns, additions", "Key list"). Owner: wave B2. Drawn by
//! `ui/keys.rs`; its live requests are in `src/live/keys.rs`.
//!
//! Stub: it only remembers the pattern, so the screen can say which one. See [`super`]
//! for the interface `App` calls.

use super::{Act, ViewState};

#[derive(Debug, Default)]
pub struct KeysState {
    /// The pattern whose keys are listed, as `PatternStats::pattern`.
    pub pattern: Option<String>,
}

impl KeysState {
    /// `enter` on a pattern on Patterns: list `pattern`'s keys. `App` calls this, then
    /// [`ViewState::on_show`]. It runs every time, also for the pattern already open.
    pub fn open(&mut self, pattern: String, act: &mut Act<'_>) {
        let _ = act;
        self.pattern = Some(pattern);
    }
}

impl ViewState for KeysState {}
