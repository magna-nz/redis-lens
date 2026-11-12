//! State of the Analysis view's Big keys and Expiry panels (spec "2 Analysis").
//! Owner: wave B. Drawn by `ui/analysis.rs`.
//!
//! Stub: nothing is built yet, so the view draws "Not built yet". See
//! [`super`] for the interface `App` calls.

use super::ViewState;

/// The Big keys and Expiry panels. TTL, Diff and Hot keys stay in `App`.
#[derive(Debug, Default)]
pub struct AnalysisState {}

impl ViewState for AnalysisState {}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use crate::tui::app::Screen;

    #[test]
    fn stub_big_keys_80x24() {
        let (app, effects) = testing::app_on(Screen::BigKeys);
        assert!(
            testing::requests(&effects).is_empty(),
            "the stub sends nothing"
        );
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn stub_expiry_120x36() {
        let (app, effects) = testing::app_on(Screen::Expiry);
        assert!(
            testing::requests(&effects).is_empty(),
            "the stub sends nothing"
        );
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }
}
