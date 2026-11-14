//! Text helpers: fixed-width cells, truncation with `…`, bars and box-drawn panels.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

use super::style;

/// Display width of one character.
pub fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

pub fn width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// At most `w` cells wide, ending in `…` when cut.
pub fn trunc(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw > w - 1 {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

/// Exactly `w` cells, left-aligned.
pub fn pad_r(s: &str, w: usize) -> String {
    let t = trunc(s, w);
    let n = w.saturating_sub(width(&t));
    format!("{t}{}", " ".repeat(n))
}

/// Exactly `w` cells, right-aligned.
pub fn pad_l(s: &str, w: usize) -> String {
    let t = trunc(s, w);
    let n = w.saturating_sub(width(&t));
    format!("{}{t}", " ".repeat(n))
}

/// Ticks (100 ms each) a long name rests before it starts to scroll, and holds at its end.
const MARQUEE_REST: u64 = 10;
/// Ticks per column of scrolling: 5 columns a second.
const MARQUEE_STEP: u64 = 2;

/// How far a text `total` cells wide is scrolled in a cell `w` wide after `ticks`:
/// `None` at rest (the `trunc` view), else the first text column shown (at least 1).
/// Only for `total > w >= 2`. The cycle is rest, scroll, hold, and then it repeats.
fn marquee_offset(total: usize, w: usize, ticks: u64) -> Option<usize> {
    // The last window shows `w - 1` cells after the leading `…`.
    let max = (total - (w - 1)) as u64;
    // The last column step is reached after `max - 1` steps, and then the hold starts.
    let scroll = MARQUEE_STEP * (max - 1);
    let t = ticks % (MARQUEE_REST + scroll + MARQUEE_REST);
    if t < MARQUEE_REST {
        return None;
    }
    Some((((t - MARQUEE_REST) / MARQUEE_STEP + 1).min(max)) as usize)
}

/// `s` in a cell `w` wide, scrolling when it does not fit. `ticks` counts from when the
/// row was selected. Text that fits is returned unchanged. Otherwise for the first 10
/// ticks the result is `trunc(s, w)`; then the text slides left one column every 2
/// ticks until its end shows, holds there for 10 ticks, rests again and repeats.
/// A scrolled window is a leading `…` and then the text from the scrolled column on, so
/// it is never cut on the right until the end is reached. A double-width character that
/// the scroll cuts is dropped and a space pads, so the result is exactly `w` cells.
pub fn marquee(s: &str, w: usize, ticks: u64) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    marquee_spans(vec![Span::raw(s.to_string())], w, ticks)
        .iter()
        .map(|sp| sp.content.as_ref())
        .collect()
}

/// Like [`fit`] (padded to exactly `w` cells), but a cut text scrolls as in [`marquee`].
pub fn marquee_spans(spans: Vec<Span<'static>>, w: usize, ticks: u64) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| width(&s.content)).sum();
    if total <= w || w < 2 {
        return fit(spans, w);
    }
    let Some(from) = marquee_offset(total, w, ticks) else {
        return fit(spans, w);
    };
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 1; // The leading `…`.
    let mut col = 0;
    let mut full = false;
    for sp in &spans {
        let mut part = String::new();
        for c in sp.content.chars() {
            let cw = char_width(c);
            let start = col;
            col += cw;
            if start < from {
                continue;
            }
            if used + cw > w {
                full = true;
                break;
            }
            part.push(c);
            used += cw;
        }
        if !part.is_empty() {
            out.push(Span::styled(part, sp.style));
        }
        if full {
            break;
        }
    }
    let lead = Span::styled("…", out.first().map_or(Style::default(), |s| s.style));
    out.insert(0, lead);
    if used < w {
        out.push(Span::raw(" ".repeat(w - used)));
    }
    out
}

/// Exactly `w` cells, left-aligned; a cut text scrolls as in [`marquee`].
pub fn pad_r_marquee(s: &str, w: usize, ticks: u64) -> String {
    let t = marquee(s, w, ticks);
    let n = w.saturating_sub(width(&t));
    format!("{t}{}", " ".repeat(n))
}

/// Spans cut or padded to exactly `w` cells. A cut ends in `…`.
pub fn fit(spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| width(&s.content)).sum();
    if total <= w {
        let mut out = spans;
        if total < w {
            out.push(Span::raw(" ".repeat(w - total)));
        }
        return out;
    }
    let mut out = Vec::new();
    if w == 0 {
        return out;
    }
    let limit = w - 1;
    let mut used = 0;
    for s in spans {
        let sw = width(&s.content);
        if used + sw <= limit {
            used += sw;
            out.push(s);
            continue;
        }
        let mut part = String::new();
        for c in s.content.chars() {
            let cw = char_width(c);
            if used + cw > limit {
                break;
            }
            part.push(c);
            used += cw;
        }
        if !part.is_empty() {
            out.push(Span::styled(part, s.style));
        }
        out.push(Span::styled("…", s.style));
        used += 1;
        break;
    }
    if used < w {
        out.push(Span::raw(" ".repeat(w - used)));
    }
    out
}

/// A pattern with its `{placeholders}` in `ph` and the rest in `base`.
pub fn pattern_spans(
    text: &str,
    base: ratatui::style::Style,
    ph: ratatui::style::Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        if open > 0 {
            out.push(Span::styled(rest[..open].to_string(), base));
        }
        out.push(Span::styled(rest[open..open + close + 1].to_string(), ph));
        rest = &rest[open + close + 1..];
    }
    if !rest.is_empty() {
        out.push(Span::styled(rest.to_string(), base));
    }
    out
}

/// A horizontal bar `w` cells wide: the filled part and the track.
pub fn bar(ratio: f64, w: usize) -> (String, String) {
    const PARTS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let min = usize::from(ratio > 0.0);
    let units = ((ratio * w as f64 * 8.0).round() as usize)
        .max(min)
        .min(w * 8);
    let full = units / 8;
    let rem = units % 8;
    let mut fill = "█".repeat(full);
    if rem > 0 {
        fill.push(PARTS[rem - 1]);
    }
    let len = full + usize::from(rem > 0);
    (fill, "░".repeat(w.saturating_sub(len)))
}

/// One block character for `v` out of `max`: `▁` for the smallest non-zero share up to
/// `█` for the largest.
pub fn level(v: u64, max: u64) -> char {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if max == 0 {
        return LEVELS[0];
    }
    let i = ((v as f64 / max as f64) * 8.0).ceil() as usize;
    LEVELS[i.clamp(1, 8) - 1]
}

/// A TTL histogram: one 5-cell column per bucket, a 3-character block bar scaled to
/// the largest bucket, or `·` when empty. The "none" bucket uses `none_style`, the
/// others teal.
pub fn ttl_hist_spans(hist: &[u64; 7], none_style: Style) -> Vec<Span<'static>> {
    let max = hist.iter().copied().max().unwrap_or(0);
    hist.iter()
        .enumerate()
        .map(|(i, &v)| {
            if v == 0 {
                Span::styled(pad_r("·", 5), style::track())
            } else {
                let st = if i == 0 { none_style } else { style::ok() };
                Span::styled(pad_r(&level(v, max).to_string().repeat(3), 5), st)
            }
        })
        .collect()
}

/// A bordered panel with `─ Title ` on the left and ` right ─` on the right of the
/// top border. Returns the inner area.
pub fn panel(frame: &mut Frame, area: Rect, title: &str, right: &str) -> Rect {
    let avail = usize::from(area.width).saturating_sub(4);
    let title = trunc(title, avail.saturating_sub(4).max(1));
    let mut block = Block::bordered()
        .border_style(style::border())
        .title_top(Line::from(vec![
            Span::styled("─ ", style::border()),
            Span::styled(title.clone(), style::title()),
            Span::styled(" ", style::border()),
        ]));
    let room = avail.saturating_sub(width(&title) + 4);
    if !right.is_empty() && room > 4 {
        block = block.title_top(
            Line::from(vec![
                Span::styled(" ", style::border()),
                Span::styled(trunc(right, room - 3), style::dim()),
                Span::styled(" ─", style::border()),
            ])
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

// ---------------------------------------------------------------------------
// Frames: list and details in one box, and stacked sections
// ---------------------------------------------------------------------------

/// Which pane of a [`split_frame`] has the keys: its title is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Left,
    Right,
}

/// One pane's title in a frame's top border: `─ Title ` on the left, ` right ─` on the
/// right, the right part dropped first when the pane is narrow.
#[derive(Debug, Clone, Copy, Default)]
pub struct Title<'a> {
    pub left: &'a str,
    pub right: &'a str,
}

impl<'a> Title<'a> {
    pub fn new(left: &'a str, right: &'a str) -> Self {
        Self { left, right }
    }
}

/// The spans of a border line `w` cells wide between `start` and `end` (corner or tee
/// characters): `start─ Title ───── right ─end`. The title in `title_style`.
fn border_line(
    w: usize,
    start: &str,
    end: &str,
    t: Title<'_>,
    title_style: Style,
) -> Vec<Span<'static>> {
    if w < 2 {
        return vec![Span::styled(start.to_string(), style::border())];
    }
    let inner = w - 2;
    let mut spans = vec![Span::styled(start.to_string(), style::border())];
    let mut used = 0;
    if !t.left.is_empty() && inner >= 5 {
        let title = trunc(t.left, inner.saturating_sub(4).max(1));
        spans.push(Span::styled("─ ", style::border()));
        used += 2 + width(&title) + 1;
        spans.push(Span::styled(title, title_style));
        spans.push(Span::styled(" ", style::border()));
    }
    let room = inner.saturating_sub(used + 1);
    let right = if !t.right.is_empty() && room > 6 {
        trunc(t.right, room - 4)
    } else {
        String::new()
    };
    let right_w = if right.is_empty() {
        0
    } else {
        width(&right) + 3
    };
    let fill = inner.saturating_sub(used + right_w);
    spans.push(Span::styled("─".repeat(fill), style::border()));
    if !right.is_empty() {
        spans.push(Span::styled(" ", style::border()));
        spans.push(Span::styled(right, style::dim()));
        spans.push(Span::styled(" ─", style::border()));
    }
    spans.push(Span::styled(end.to_string(), style::border()));
    spans
}

fn put(frame: &mut Frame, x: u16, y: u16, w: u16, spans: Vec<Span<'static>>) {
    frame.render_widget(Line::from(spans), Rect::new(x, y, w, 1));
}

fn put_char(frame: &mut Frame, x: u16, y: u16, c: &str) {
    frame.render_widget(
        Span::styled(c.to_string(), style::border()),
        Rect::new(x, y, 1, 1),
    );
}

/// The width of the left pane of a [`split_frame`] `total` cells wide: `ratio` of it
/// (0.0 to 1.0), and at least 12 cells on each side when there is room.
pub fn split_at(total: u16, ratio: f64) -> u16 {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let at = (f64::from(total) * ratio).round() as u16;
    if total < 26 {
        return at.min(total);
    }
    at.clamp(12, total - 12)
}

/// One frame holding a list on the left and the selected row's details on the right,
/// with one shared divider:
///
/// ```text
/// ┌─ Patterns ──────── by memory ─┬─ user:{id}:cart ───── hash ─┐
/// │ …                             │ …                           │
/// └───────────────────────────────┴─────────────────────────────┘
/// ```
///
/// `left_w` is the left pane's width counted from the frame's left edge to the divider
/// (see [`split_at`]). `focus` says which pane has the keys: its title is drawn
/// highlighted, the other dim. Returns the inner areas of the left and the right pane.
/// A frame too small for two panes gets everything on the left.
pub fn split_frame(
    frame: &mut Frame,
    area: Rect,
    left_w: u16,
    left: Title<'_>,
    right: Title<'_>,
    focus: Pane,
) -> (Rect, Rect) {
    if area.width < 4 || area.height < 2 {
        return (Rect::default(), Rect::default());
    }
    let left_w = left_w.clamp(1, area.width.saturating_sub(2));
    let (lt, rt) = match focus {
        Pane::Left => (style::title(), style::dim()),
        Pane::Right => (style::dim(), style::title()),
    };
    let x = area.x;
    let y = area.y;
    let bottom = y + area.height - 1;
    let div = x + left_w;
    let right_w = area.width - left_w;
    // Top: `┌─ left ─┬─ right ─┐`.
    put(
        frame,
        x,
        y,
        left_w,
        border_line(usize::from(left_w) + 1, "┌", "┬", left, lt),
    );
    put(
        frame,
        div,
        y,
        right_w,
        border_line(usize::from(right_w), "┬", "┐", right, rt),
    );
    for row in y + 1..bottom {
        put_char(frame, x, row, "│");
        put_char(frame, div, row, "│");
        put_char(frame, x + area.width - 1, row, "│");
    }
    let mut bot = vec![Span::styled("└", style::border())];
    bot.push(Span::styled(
        "─".repeat(usize::from(left_w.saturating_sub(1))),
        style::border(),
    ));
    bot.push(Span::styled("┴", style::border()));
    bot.push(Span::styled(
        "─".repeat(usize::from(right_w.saturating_sub(2))),
        style::border(),
    ));
    bot.push(Span::styled("┘", style::border()));
    put(frame, x, bottom, area.width, bot);
    let h = area.height.saturating_sub(2);
    let l = Rect::new(x + 1, y + 1, left_w.saturating_sub(1), h);
    let r = Rect::new(div + 1, y + 1, right_w.saturating_sub(2), h);
    (l, r)
}

/// One of the stacked sections of a [`sections`] frame.
#[derive(Debug, Clone, Copy)]
pub struct Section<'a> {
    pub title: Title<'a>,
    /// Rows inside the section; `None` takes what is left (shared evenly when more
    /// than one section has `None`).
    pub rows: Option<u16>,
}

/// One frame of sections stacked top to bottom, each under its own title, one border
/// between two (`├ ┤`):
///
/// ```text
/// ┌─ Now ───────────── INFO every 2 s ─┐
/// │ …                                  │
/// ├─ Keys ──────────────── no keys read ┤
/// │ …                                  │
/// └────────────────────────────────────┘
/// ```
///
/// Also the layout of a full-width list with its details below (Connections, Clients,
/// Slow log, Nodes). Returns the inner area of each section, in order; a section that
/// gets no room has an empty one. The first section's title is drawn highlighted.
pub fn sections(frame: &mut Frame, area: Rect, list: &[Section<'_>]) -> Vec<Rect> {
    let mut out = vec![Rect::default(); list.len()];
    if list.is_empty() || area.width < 4 || area.height < 2 {
        return out;
    }
    // One title row per section and the bottom border.
    let borders = u16::try_from(list.len())
        .unwrap_or(u16::MAX)
        .saturating_add(1);
    let room = area.height.saturating_sub(borders);
    let fixed: u16 = list.iter().filter_map(|s| s.rows).sum();
    let fills = list.iter().filter(|s| s.rows.is_none()).count() as u16;
    let mut spare = room.saturating_sub(fixed);
    let mut y = area.y;
    let bottom = area.y + area.height - 1;
    let w = usize::from(area.width);
    let mut fills_left = fills;
    for (i, sec) in list.iter().enumerate() {
        if y >= bottom {
            break;
        }
        let (start, end) = if i == 0 {
            ("┌", "┐")
        } else {
            ("├", "┤")
        };
        let st = if i == 0 { style::title() } else { style::hd() };
        put(
            frame,
            area.x,
            y,
            area.width,
            border_line(w, start, end, sec.title, st),
        );
        y += 1;
        let want = match sec.rows {
            Some(n) => n,
            None => {
                let share = spare.checked_div(fills_left).unwrap_or(0);
                fills_left = fills_left.saturating_sub(1);
                spare -= share;
                share
            }
        };
        let h = want.min(bottom.saturating_sub(y));
        for row in y..y + h {
            put_char(frame, area.x, row, "│");
            put_char(frame, area.x + area.width - 1, row, "│");
        }
        out[i] = Rect::new(area.x + 1, y, area.width - 2, h);
        y += h;
    }
    // Rows no section took stay inside the frame.
    for row in y..bottom {
        put_char(frame, area.x, row, "│");
        put_char(frame, area.x + area.width - 1, row, "│");
    }
    let mut bot = vec![Span::styled("└", style::border())];
    bot.push(Span::styled(
        "─".repeat(w.saturating_sub(2)),
        style::border(),
    ));
    bot.push(Span::styled("┘", style::border()));
    put(frame, area.x, bottom, area.width, bot);
    out
}

/// Dims what is drawn in `area`, behind a dialog: every cell in the dim colour (in
/// `NO_COLOR`, dim and not bold), so the dialog's words stand out. The text stays, so it
/// still reads.
pub fn dim(frame: &mut Frame, area: Rect) {
    let area = area.intersection(frame.area());
    let backdrop = style::backdrop();
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(backdrop);
            }
        }
    }
}

/// A dialog box `w` × `h` centred in `area` (shrunk to fit): cleared, framed, with
/// `title` and `right` in the top border. Returns its inner area. Draw it after
/// [`dim`] so the screen behind it is dimmed.
pub fn dialog(frame: &mut Frame, area: Rect, w: u16, h: u16, title: &str, right: &str) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(ratatui::widgets::Clear, rect);
    panel(frame, rect, title, right)
}

/// Render lines one per row from the top of `area`. `selected` gets the selection
/// background across the whole row.
pub fn rows(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, selected: Option<usize>) {
    for (i, line) in lines.into_iter().enumerate() {
        if i >= usize::from(area.height) {
            break;
        }
        let r = Rect {
            y: area.y + i as u16,
            height: 1,
            ..area
        };
        let line = if selected == Some(i) {
            line.style(style::sel())
        } else {
            line
        };
        frame.render_widget(line, r);
    }
}

/// First row to show so that `selected` is on screen in a list `height` rows tall.
pub fn scroll_offset(selected: usize, height: usize) -> usize {
    if height == 0 {
        0
    } else {
        (selected + 1).saturating_sub(height)
    }
}

/// Left and right column widths for a two-part detail panel.
pub fn split_cols(inner: usize) -> (usize, usize) {
    let left = inner.saturating_sub(46).max(inner * 55 / 100);
    (left, inner.saturating_sub(left))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marquee_fits_unchanged() {
        assert_eq!(marquee("abc", 5, 0), "abc");
        assert_eq!(marquee("abc", 3, 999), "abc");
    }

    #[test]
    fn marquee_rests_for_a_second() {
        let s = "abcdefghijklmnop";
        for t in 0..10 {
            assert_eq!(marquee(s, 6, t), trunc(s, 6), "tick {t}");
        }
        assert_eq!(marquee(s, 6, 0), "abcde…");
    }

    #[test]
    fn marquee_scrolls_a_column_every_two_ticks() {
        let s = "abcdefghijklmnop";
        assert_eq!(marquee(s, 6, 10), "…bcdef");
        assert_eq!(marquee(s, 6, 11), "…bcdef");
        assert_eq!(marquee(s, 6, 12), "…cdefg");
        assert_eq!(marquee(s, 6, 16), "…efghi");
    }

    #[test]
    fn marquee_end_window_shows_the_end_and_holds() {
        let s = "abcdefghijklmnop"; // 16 wide, w 6: last offset 11, reached at tick 30.
        let end = "…lmnop";
        assert_eq!(marquee(s, 6, 28), "…klmno");
        assert_eq!(marquee(s, 6, 29), "…klmno");
        for t in 30..40 {
            assert_eq!(marquee(s, 6, t), end, "tick {t}");
        }
        assert_eq!(marquee(s, 6, 40), trunc(s, 6));
    }

    #[test]
    fn marquee_repeats() {
        let s = "abcdefghijklmnop";
        let cycle = 10 + 20 + 10;
        for t in 0..cycle {
            assert_eq!(marquee(s, 6, t), marquee(s, 6, t + cycle), "tick {t}");
            assert_eq!(marquee(s, 6, t), marquee(s, 6, t + 5 * cycle), "tick {t}");
        }
        assert_eq!(marquee(s, 6, cycle), trunc(s, 6));
    }

    #[test]
    fn marquee_keeps_the_width_with_double_width_text() {
        let s = "日本語のキー名前テスト";
        for w in [2, 3, 4, 5, 8, 9] {
            for t in 0..200 {
                assert_eq!(width(&marquee(s, w, t)), w, "w {w} tick {t}");
            }
        }
        let s = "a日b本c語d";
        for t in 0..200 {
            assert_eq!(width(&marquee(s, 4, t)), 4, "tick {t}");
        }
    }

    #[test]
    fn marquee_spans_keep_styles_and_width() {
        let spans = vec![
            Span::raw(" "),
            Span::styled("user:", style::fg()),
            Span::styled("{id}", style::ph()),
            Span::styled(":profile:settings", style::fg()),
        ];
        let out = marquee_spans(spans, 12, 16);
        let text: String = out.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(width(&text), 12);
        assert!(text.starts_with('…'));
        assert!(out.iter().any(|s| s.style == style::ph()) || !text.contains('{'));
    }

    #[test]
    fn truncation() {
        assert_eq!(trunc("abcdef", 4), "abc…");
        assert_eq!(trunc("abc", 4), "abc");
        assert_eq!(trunc("abc", 0), "");
        assert_eq!(pad_r("ab", 4), "ab  ");
        assert_eq!(pad_l("ab", 4), "  ab");
        let f = fit(vec![Span::raw("hello"), Span::raw("world")], 7);
        let s: String = f.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(s, "hellow…");
    }

    #[test]
    fn bars() {
        assert_eq!(bar(0.0, 4), (String::new(), "░░░░".into()));
        assert_eq!(bar(1.0, 4), ("████".into(), String::new()));
        assert_eq!(bar(0.5, 4).0, "██");
        assert_eq!(bar(0.001, 4).0, "▏");
        assert_eq!(bar(f64::NAN, 2).1, "░░");
    }

    #[test]
    fn levels_and_histograms() {
        assert_eq!(level(1, 100), '▁');
        assert_eq!(level(100, 100), '█');
        assert_eq!(level(0, 0), '▁');
        let spans = ttl_hist_spans(&[0, 10, 0, 0, 0, 0, 5], style::warn());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "·    ███  ·    ·    ·    ·    ▄▄▄  ");
    }

    fn draw_with(w: u16, h: u16, f: impl FnOnce(&mut Frame)) -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(f).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_split_frame_shares_one_divider() {
        let mut inner = (Rect::default(), Rect::default());
        let text = draw_with(30, 4, |f| {
            inner = split_frame(
                f,
                f.area(),
                14,
                Title::new("List", ""),
                Title::new("Detail", "r"),
                Pane::Left,
            );
        });
        assert_eq!(
            text,
            "┌─ List ──────┬─ Detail ─────┐\n\
             │             │              │\n\
             │             │              │\n\
             └─────────────┴──────────────┘"
        );
        assert_eq!(inner.0, Rect::new(1, 1, 13, 2));
        assert_eq!(inner.1, Rect::new(15, 1, 14, 2));
    }

    #[test]
    fn stacked_sections_share_tees_and_fill_the_rest() {
        let mut inner = Vec::new();
        let text = draw_with(20, 7, |f| {
            inner = sections(
                f,
                f.area(),
                &[
                    Section {
                        title: Title::new("Now", "x"),
                        rows: Some(1),
                    },
                    Section {
                        title: Title::new("Keys", ""),
                        rows: None,
                    },
                ],
            );
        });
        assert_eq!(
            text,
            "┌─ Now ──────── x ─┐\n\
             │                  │\n\
             ├─ Keys ───────────┤\n\
             │                  │\n\
             │                  │\n\
             │                  │\n\
             └──────────────────┘"
        );
        assert_eq!(inner, [Rect::new(1, 1, 18, 1), Rect::new(1, 3, 18, 3)]);
    }

    #[test]
    fn frames_never_panic_on_tiny_areas() {
        for (w, h) in [(0, 0), (1, 1), (3, 2), (4, 2), (5, 3), (200, 2)] {
            draw_with(w, h, |f| {
                split_frame(
                    f,
                    f.area(),
                    2,
                    Title::new("a", "b"),
                    Title::new("c", "d"),
                    Pane::Right,
                );
                sections(
                    f,
                    f.area(),
                    &[Section {
                        title: Title::new("a", "b"),
                        rows: Some(9),
                    }],
                );
                dim(f, f.area());
                dialog(f, f.area(), 50, 9, "t", "r");
            });
        }
        assert_eq!(split_at(100, 0.5), 50);
        assert_eq!(split_at(40, 0.05), 12);
        assert_eq!(split_at(40, 0.99), 28);
    }

    #[test]
    fn placeholders() {
        let s = pattern_spans("user:{id}:cart", style::fg(), style::ph());
        let parts: Vec<&str> = s.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(parts, ["user:", "{id}", ":cart"]);
    }
}
