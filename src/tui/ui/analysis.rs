//! Draws [`AnalysisState`]. Owner: wave B, with `views/analysis.rs`.
//!
//! Stub: "Not built yet".

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::tui::app::views::Ctx;
use crate::tui::app::views::analysis::AnalysisState;

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &AnalysisState, ctx: &Ctx<'_>) {
    let _ = state;
    super::not_built(frame, area, ctx.screen.name(), ctx);
}
