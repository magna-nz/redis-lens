//! State of the Server view: the Overview, Clients and Slow log panels (spec "3 Server").
//! Owner: wave B. Drawn by `ui/server.rs`; its live requests are in `src/live/server.rs`.
//!
//! Stub: nothing is built yet, so the view draws "Not built yet". See [`super`] for the
//! interface `App` calls. The test below is the example of a view test.

use super::ViewState;

/// The three panels. `act.screen` says which one is shown.
#[derive(Debug, Default)]
pub struct ServerState {}

impl ViewState for ServerState {}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use crate::tui::app::Screen;

    /// The example of a view test: an app on the Overview panel with a finished scan and
    /// a ready live connection, drawn at 80 × 24. A built view also checks what its
    /// `on_show` and `on_tick` send (`testing::requests`) and feeds the answers back
    /// with `testing::reply`.
    #[test]
    fn stub_overview_80x24() {
        let (app, effects) = testing::app_on(Screen::Overview);
        assert!(
            testing::requests(&effects).is_empty(),
            "the stub sends nothing"
        );
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }
}
