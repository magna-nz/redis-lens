//! State of the Search view (spec "6 Search"). Owner: wave B. Drawn by
//! `ui/search.rs`; its live requests are in `src/live/search.rs`.
//!
//! Stub: nothing is built yet, so the view draws "Not built yet". See
//! [`super`] for the interface `App` calls.

use super::ViewState;

/// The index list and the selected index's schema.
#[derive(Debug, Default)]
pub struct SearchState {}

impl ViewState for SearchState {}
