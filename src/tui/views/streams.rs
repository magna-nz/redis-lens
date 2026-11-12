//! State of the Streams view (spec "4 Streams"). Owner: wave B. Drawn by
//! `ui/streams.rs`; its live requests are in `src/live/streams.rs`.
//!
//! Stub: nothing is built yet, so the view draws "Not built yet". See
//! [`super`] for the interface `App` calls.

use super::ViewState;

/// The stream list, its groups and consumers.
#[derive(Debug, Default)]
pub struct StreamsState {}

impl ViewState for StreamsState {}
