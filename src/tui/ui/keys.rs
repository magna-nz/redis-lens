//! Draws [`KeysState`]. Owner: wave B, with `views/keys.rs`.
//!
//! Stub: "Not built yet".

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::tui::app::views::Ctx;
use crate::tui::app::views::keys::KeysState;

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &KeysState, ctx: &Ctx<'_>) {
    let _ = state;
    super::not_built(frame, area, ctx.screen.name(), ctx);
}
