//! State of the Pub/Sub view (spec "5 Pub/Sub"). Owner: wave B. Drawn by
//! `ui/pubsub.rs`; its live requests are in `src/live/pubsub.rs`.
//!
//! Stub: nothing is built yet, so the view draws "Not built yet". See
//! [`super`] for the interface `App` calls.

use super::ViewState;

/// Channel and pattern counts.
#[derive(Debug, Default)]
pub struct PubSubState {}

impl ViewState for PubSubState {}
