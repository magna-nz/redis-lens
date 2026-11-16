//! The Actions menu (`.`, [`crate::tui::app::actions`]): a box in the focus colour over
//! the screen, without dimming it, placed so the selected row stays readable (bottom
//! right when that leaves it in view, else another corner, else above or below it).
//! One group per heading (the selected row, its parent, the screen); each line has the
//! `›` gutter, its key as a key cap, its name and at the right the command a write
//! sends, what a form sends, `asks first`, or why it is off. A line that is off is dim.
//! When a line is short the name is cut first; the reason only when it alone is wider
//! than the line (`enter` on the line shows it whole).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::style;
use super::widgets::{focus_box, marker, rows, trunc, width};
use crate::tui::app::App;
use crate::tui::app::actions::{GROUPS, MenuLine, Status, short_reason};
use crate::tui::keymap::Group;

/// Widest the menu gets.
const MAX_W: u16 = 58;

/// One row of the menu as drawn: a heading, a blank between groups, or a line (its
/// index among the lines).
enum Row {
    Head(Group, &'static str, String),
    Blank,
    Line(usize),
}

/// The right of a line, and its style.
fn right_of(l: &MenuLine) -> (String, ratatui::style::Style) {
    match &l.status {
        Status::Ready(Some(t)) | Status::Sends(t) => (t.clone(), style::dim()),
        Status::Ready(None) => (String::new(), style::dim()),
        Status::Form { asks: true, .. } | Status::Asks => ("asks first".into(), style::amber()),
        Status::Form { sends, .. } => (sends.clone(), style::dim()),
        Status::Off(why) => (format!("off: {}", short_reason(why)), style::dim()),
    }
}

/// The rows of the menu, in order.
fn layout(app: &App, lines: &[MenuLine]) -> Vec<Row> {
    let screen = app.menu.map_or_else(|| app.screen(), |m| m.screen);
    let heads = app.menu_heads(screen);
    let mut out = Vec::new();
    for (g, (kind, name)) in GROUPS.iter().zip(heads) {
        let idx: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.group == *g)
            .map(|(i, _)| i)
            .collect();
        if idx.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Row::Blank);
        }
        let kind = if kind.is_empty() {
            match g {
                Group::Parent => "PARENT",
                _ => "ROW",
            }
        } else {
            kind
        };
        out.push(Row::Head(*g, kind, name));
        out.extend(idx.into_iter().map(Row::Line));
    }
    out
}

/// One line `w` cells wide: gutter, key cap (`cap` cells), name, right. The name is
/// cut to fit; the right never is unless there is no room for it at all.
fn line_of(l: &MenuLine, selected: bool, cap: usize, w: usize) -> Line<'static> {
    let off = matches!(l.status, Status::Off(_));
    let (right, rst) = right_of(l);
    let key = l.key.footer_keys;
    let cap_text = format!(" {key} ");
    let pad = cap.saturating_sub(width(&cap_text));
    // Gutter, cap, a space before the name, two before the right, one at the end.
    let fixed = 2 + cap + 1 + 1;
    let room = w.saturating_sub(fixed);
    let right = if right.is_empty() {
        right
    } else {
        trunc(&right, room.saturating_sub(4))
    };
    let rw = width(&right);
    let name_room = room.saturating_sub(if rw == 0 { 0 } else { rw + 2 });
    let name = trunc(&l.label, name_room);
    let gap = room.saturating_sub(width(&name) + rw);
    let mut spans = marker(selected).to_vec();
    spans.push(Span::styled(
        cap_text,
        if off { style::dim() } else { style::key() },
    ));
    spans.push(Span::raw(" ".repeat(pad + 1)));
    spans.push(Span::styled(
        name,
        if off { style::dim() } else { style::fg() },
    ));
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(right, rst));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// `<profile> · ENV · read-only` or `writes on`, for the title's right.
fn title_right(app: &App) -> String {
    let p = app
        .session
        .as_ref()
        .map(|s| &s.profile)
        .or_else(|| app.selected_entry().map(|e| &e.profile));
    match p {
        Some(p) => format!(
            "{} · {} · {}",
            crate::value::escape(&p.name),
            p.env.label(),
            if p.is_read_only() {
                "read-only"
            } else {
                "writes on"
            }
        ),
        None => String::new(),
    }
}

/// Draws the open menu in `body`.
pub fn draw(frame: &mut Frame, body: Rect, app: &App) {
    let Some(menu) = app.menu else {
        return;
    };
    let lines = app.menu_lines();
    let rows_all = layout(app, &lines);
    let w = body.width.min(MAX_W);
    // The rows and the two borders, as many as the body has.
    let want = u16::try_from(rows_all.len() + 2).unwrap_or(u16::MAX);
    let h = want.min(body.height);
    if w < 20 || h < 3 {
        return;
    }
    let rect = place(frame, body, w, h);
    let inner = focus_box(frame, rect, "Actions", &title_right(app));
    let iw = usize::from(inner.width);
    let room = usize::from(inner.height);
    let sel = menu.sel.min(lines.len().saturating_sub(1));
    let sel_row = rows_all
        .iter()
        .position(|r| matches!(r, Row::Line(i) if *i == sel))
        .unwrap_or(0);
    // Scrolled so the selected line shows, with its heading when it fits; the last
    // row says how many more there are below when they do not all fit.
    let fits = rows_all.len() <= room;
    let shown = if fits { room } else { room.saturating_sub(1) };
    let first = if fits {
        0
    } else {
        sel_row
            .saturating_sub(shown.saturating_sub(1))
            .min(rows_all.len() - shown)
    };
    let cap = lines
        .iter()
        .map(|l| width(l.key.footer_keys) + 2)
        .max()
        .unwrap_or(3);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut selected = None;
    for (n, row) in rows_all.iter().enumerate().skip(first).take(shown) {
        out.push(match row {
            Row::Blank => Line::default(),
            Row::Head(group, kind, name) => {
                // Only a row can be missing; a parent or the screen just has no name.
                let name = if name.is_empty() && *group == Group::Row {
                    Span::styled("nothing selected", style::dim())
                } else {
                    Span::styled(trunc(name, iw.saturating_sub(width(kind) + 4)), style::fg())
                };
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(kind.to_string(), style::hd()),
                    Span::raw("  "),
                    name,
                ])
            }
            Row::Line(i) => {
                if *i == sel {
                    selected = Some(n - first);
                }
                line_of(&lines[*i], *i == sel, cap, iw)
            }
        });
    }
    if !fits {
        let more = rows_all.len().saturating_sub(first + shown);
        out.push(Line::from(Span::styled(
            if more > 0 {
                format!(" ↓ {more} more")
            } else {
                format!(" ↑ {first} more")
            },
            style::dim(),
        )));
    }
    rows(frame, inner, out, selected);
}

/// The selected rows on screen: the `›` of each list's gutter in the focus colour
/// (bold without colour), drawn before the menu.
fn selected_rows(frame: &mut Frame, body: Rect) -> Vec<(u16, u16)> {
    let focus = style::focus();
    let buf = frame.buffer_mut();
    let mut out = Vec::new();
    for y in body.y..body.y + body.height {
        for x in body.x + 1..(body.x + body.width).saturating_sub(1) {
            let c = &buf[(x, y)];
            let marked = match focus.fg {
                Some(fg) => c.fg == fg,
                None => c.modifier.contains(ratatui::style::Modifier::BOLD),
            };
            if c.symbol() == "›"
                && marked
                && matches!(buf[(x - 1, y)].symbol(), "│" | " ")
                && buf[(x + 1, y)].symbol() == " "
            {
                out.push((x, y));
            }
        }
    }
    out
}

/// Where the menu goes, `w` × `h` in `body`: the first corner (bottom right, top right,
/// bottom left, top left) that leaves every selected row's start in view; else below
/// or above the selected rows, shorter (it scrolls); else bottom right, where the
/// group headings still name the row.
fn place(frame: &mut Frame, body: Rect, w: u16, h: u16) -> Rect {
    let rows = selected_rows(frame, body);
    let right = body.x + body.width - w;
    let bottom = body.y + body.height - h;
    let at = |x, y, h| Rect::new(x, y, w, h);
    // A row stays readable when its marker and the start of its name are not covered.
    let clear = |r: Rect| {
        rows.iter().all(|&(x, y)| {
            let covers_row = y >= r.y && y < r.y + r.height;
            let covers_name = r.x < x + 24 && r.x + r.width > x;
            !(covers_row && covers_name)
        })
    };
    for r in [
        at(right, bottom, h),
        at(right, body.y, h),
        at(body.x, bottom, h),
        at(body.x, body.y, h),
    ] {
        if clear(r) {
            return r;
        }
    }
    const MIN_H: u16 = 8;
    if let Some(low) = rows.iter().map(|r| r.1).max() {
        let below = body.y + body.height - (low + 1);
        if below >= MIN_H {
            let hh = h.min(below);
            return at(right, body.y + body.height - hh, hh);
        }
    }
    if let Some(high) = rows.iter().map(|r| r.1).min() {
        let above = high - body.y;
        if above >= MIN_H {
            return at(right, body.y, h.min(above));
        }
    }
    at(right, bottom, h)
}

/// The footer while the menu is open.
pub fn footer() -> Vec<(&'static str, &'static str)> {
    vec![("↑↓", "Pick"), ("enter", "Run"), ("esc", "Close")]
}

/// Dim after the menu's footer keys.
pub const FOOTER_NOTE: &str = "or press the action's own key";
