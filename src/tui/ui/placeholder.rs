//! Views that arrive in v0.2.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::style;
use super::widgets::{panel, rows};
use crate::tui::app::View;

pub fn draw(frame: &mut Frame, area: Rect, view: View) {
    let inner = panel(frame, area, view.title(), "v0.2");
    let lines = vec![
        Line::default(),
        Line::from(Span::styled(
            format!(" {} comes in v0.2", view.title()),
            style::fgb(),
        )),
        Line::from(Span::styled(
            " Press 1 for Patterns or 0 for Connections.",
            style::dim(),
        )),
    ];
    rows(frame, inner, lines, None);
}
