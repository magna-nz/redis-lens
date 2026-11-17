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
/// `s` cut to `w` cells in the middle: its start and its end kept around one `…`
/// (`cache:se…{hash}`), so names that share a prefix stay told apart. The start gets
/// the larger half.
pub fn trunc_middle(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w <= 2 {
        return trunc(s, w);
    }
    let room = w - 1;
    let tail_w = room / 2;
    let head_w = room - tail_w;
    let mut head = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw > head_w {
            break;
        }
        head.push(c);
        used += cw;
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let cw = char_width(c);
        if used + cw > tail_w {
            break;
        }
        tail.push(c);
        used += cw;
    }
    tail.reverse();
    format!("{head}…{}", tail.into_iter().collect::<String>())
}

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
/// the largest bucket, or `·` when empty. The "none" bucket uses `none_style` (the
/// warning colour when it is the flagged one), the others the data colour.
pub fn ttl_hist_spans(hist: &[u64; 7], none_style: Style) -> Vec<Span<'static>> {
    let max = hist.iter().copied().max().unwrap_or(0);
    hist.iter()
        .enumerate()
        .map(|(i, &v)| {
            if v == 0 {
                Span::styled(pad_r("·", 5), style::track())
            } else {
                let st = if i == 0 { none_style } else { style::data() };
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
/// characters): `start─ Title ───── right ─end`. The title in `title_style`, the line in
/// `line` (the normal border, or the focus colour on a focused pane).
fn border_line(
    w: usize,
    start: &str,
    end: &str,
    t: Title<'_>,
    title_style: Style,
    line: Style,
) -> Vec<Span<'static>> {
    if w < 2 {
        return vec![Span::styled(start.to_string(), line)];
    }
    let inner = w - 2;
    let mut spans = vec![Span::styled(start.to_string(), line)];
    let mut used = 0;
    if !t.left.is_empty() && inner >= 5 {
        let title = trunc(t.left, inner.saturating_sub(4).max(1));
        spans.push(Span::styled("─ ", line));
        used += 2 + width(&title) + 1;
        spans.push(Span::styled(title, title_style));
        spans.push(Span::styled(" ", line));
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
    spans.push(Span::styled("─".repeat(fill), line));
    if !right.is_empty() {
        spans.push(Span::styled(" ", line));
        spans.push(Span::styled(right, style::dim()));
        spans.push(Span::styled(" ─", line));
    }
    spans.push(Span::styled(end.to_string(), line));
    spans
}

fn put(frame: &mut Frame, x: u16, y: u16, w: u16, spans: Vec<Span<'static>>) {
    frame.render_widget(Line::from(spans), Rect::new(x, y, w, 1));
}

fn put_char(frame: &mut Frame, x: u16, y: u16, c: &str, st: Style) {
    frame.render_widget(Span::styled(c.to_string(), st), Rect::new(x, y, 1, 1));
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
/// (see [`split_at`]). `focus` says which pane has the keys: its title is drawn bold in
/// the focus colour and its border (with the shared divider) in the focus colour; the
/// other pane keeps the normal border and a dim title. Returns the inner areas of the left and the right pane.
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
    // The focused pane: its title bold in the focus colour, its border segment (and the
    // shared divider) in the focus colour; the other pane dim and normal.
    let (on, off) = (style::focus(), style::border());
    let (lt, rt, lb, rb) = match focus {
        Pane::Left => (style::title(), style::dim(), on, off),
        Pane::Right => (style::dim(), style::title(), off, on),
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
        border_line(usize::from(left_w) + 1, "┌", "┬", left, lt, lb),
    );
    put(
        frame,
        div,
        y,
        right_w,
        border_line(usize::from(right_w), "┬", "┐", right, rt, rb),
    );
    for row in y + 1..bottom {
        put_char(frame, x, row, "│", lb);
        put_char(frame, x + area.width - 1, row, "│", rb);
    }
    let mut bot = vec![Span::styled("└", lb)];
    bot.push(Span::styled(
        "─".repeat(usize::from(left_w.saturating_sub(1))),
        lb,
    ));
    bot.push(Span::styled("┴", on));
    bot.push(Span::styled(
        "─".repeat(usize::from(right_w.saturating_sub(2))),
        rb,
    ));
    bot.push(Span::styled("┘", rb));
    put(frame, x, bottom, area.width, bot);
    // The divider is the focused pane's.
    put_char(frame, div, y, "┬", on);
    for row in y + 1..bottom {
        put_char(frame, div, row, "│", on);
    }
    let h = area.height.saturating_sub(2);
    let l = Rect::new(x + 1, y + 1, left_w.saturating_sub(1), h);
    let r = Rect::new(div + 1, y + 1, right_w.saturating_sub(2), h);
    (l, r)
}

/// One frame holding panes side by side, one shared divider between two: the
/// generalisation of [`split_frame`] to any number of panes (Browse's three panes from
/// 120 columns, a list with its details at the right).
///
/// `widths` are each pane's width counted from its left edge (the frame's border or its
/// divider) to the next divider; the last pane takes what is left, its right border
/// included. A pane of width 0 is left out (its divider too), and its inner area is
/// empty. `focus` is the index of the pane that has the keys: its title bold in the
/// focus colour, its border segments and both dividers beside it in the focus colour;
/// the others keep the normal border and a dim title. Returns the inner area of each
/// pane, in order.
pub fn frame_panes(
    frame: &mut Frame,
    area: Rect,
    widths: &[u16],
    titles: &[Title<'_>],
    focus: usize,
) -> Vec<Rect> {
    let n = widths.len().min(titles.len());
    let mut out = vec![Rect::default(); n];
    if n == 0 || area.width < 4 || area.height < 2 {
        return out;
    }
    // The panes shown, as (index, x, width); the last takes the rest.
    let mut shown: Vec<(usize, u16, u16)> = Vec::new();
    let mut x = area.x;
    let end = area.x + area.width;
    for (i, &w) in widths.iter().enumerate().take(n) {
        if w == 0 || x >= end.saturating_sub(2) {
            continue;
        }
        let w = w.min(end - x);
        shown.push((i, x, w));
        x += w;
    }
    let Some(last) = shown.last_mut() else {
        return out;
    };
    last.2 = end - last.1;
    let (on, off) = (style::focus(), style::border());
    let y = area.y;
    let bottom = y + area.height - 1;
    let k = shown.len();
    for (j, &(i, px, pw)) in shown.iter().enumerate() {
        let focused = i == focus;
        let (title_st, line) = if focused {
            (style::title(), on)
        } else {
            (style::dim(), off)
        };
        let start = if j == 0 { "┌" } else { "┬" };
        let top = if j + 1 == k {
            border_line(usize::from(pw), start, "┐", titles[i], title_st, line)
        } else {
            border_line(usize::from(pw) + 1, start, "┬", titles[i], title_st, line)
        };
        put(frame, px, y, pw, top);
        // The bottom border under this pane.
        let mut bot = vec![Span::styled(if j == 0 { "└" } else { "┴" }, line)];
        let dashes = if j + 1 == k {
            pw.saturating_sub(2)
        } else {
            pw.saturating_sub(1)
        };
        bot.push(Span::styled("─".repeat(usize::from(dashes)), line));
        if j + 1 == k {
            bot.push(Span::styled("┘", line));
        }
        put(frame, px, bottom, pw, bot);
        if j == 0 {
            for row in y + 1..bottom {
                put_char(frame, px, row, "│", line);
            }
        }
        if j + 1 == k {
            for row in y + 1..bottom {
                put_char(frame, px + pw - 1, row, "│", line);
            }
        }
        let inner_w = if j + 1 == k {
            pw.saturating_sub(2)
        } else {
            pw.saturating_sub(1)
        };
        out[i] = Rect::new(px + 1, y + 1, inner_w, area.height.saturating_sub(2));
    }
    // The dividers: each is the focused pane's when it is beside it.
    for j in 1..k {
        let (left, _, _) = shown[j - 1];
        let (right, dx, _) = shown[j];
        let st = if left == focus || right == focus {
            on
        } else {
            off
        };
        put_char(frame, dx, y, "┬", st);
        for row in y + 1..bottom {
            put_char(frame, dx, row, "│", st);
        }
        put_char(frame, dx, bottom, "┴", st);
    }
    out
}

/// A list's rows in `area` as [`list`] draws them, in a pane that does not have the
/// keys: no selection background, and the row that is open (the pattern whose keys
/// are listed, the key whose value is shown) keeps a dim `▸` in the gutter, so the
/// path reads left to right. `None`: no row is open.
pub fn list_open(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, open: Option<usize>) {
    let n = lines.len().min(usize::from(area.height));
    for i in 0..n {
        let r = Rect {
            y: area.y + i as u16,
            height: 1,
            width: GUTTER.min(area.width),
            ..area
        };
        let mark = if open == Some(i) { "▸" } else { " " };
        let line = Line::from(vec![Span::raw(" "), Span::styled(mark, style::dim())]);
        frame.render_widget(line, r);
    }
    rows(frame, gutter_area(area), lines, None);
}

/// `spans` broken at spaces into lines that fit `w` cells (by display width), each
/// line after the first indented by `indent` cells. A word longer than a whole line
/// is broken across lines (hard-wrapped), never cut.
pub fn wrap_spans(spans: Vec<Span<'static>>, w: usize, indent: usize) -> Vec<Line<'static>> {
    // One cell short of the pane, as `wrap_words` keeps it, so no line ends in `…`.
    let w = w.saturating_sub(1).max(indent + 4);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;
    let new_line = |lines: &mut Vec<Vec<Span<'static>>>, used: &mut usize| {
        lines.push(vec![Span::raw(" ".repeat(indent))]);
        *used = indent;
    };
    for sp in spans {
        let st = sp.style;
        // Words with the spaces in front of them, so a break drops those spaces.
        let mut words: Vec<String> = Vec::new();
        let mut cur = String::new();
        for ch in sp.content.chars() {
            if ch == ' ' && !cur.trim().is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            words.push(cur);
        }
        for word in words {
            let mut word = word;
            if used + width(&word) > w && used > indent {
                new_line(&mut lines, &mut used);
                word = word.trim_start().to_string();
            }
            // What does not fit the line goes on the next, as many lines as it takes.
            let mut part = String::new();
            for c in word.chars() {
                let cw = width(c.encode_utf8(&mut [0; 4]));
                if used + width(&part) + cw > w && (used > indent || !part.is_empty()) {
                    if !part.is_empty() {
                        let done = std::mem::take(&mut part);
                        lines
                            .last_mut()
                            .expect("one line")
                            .push(Span::styled(done, st));
                    }
                    new_line(&mut lines, &mut used);
                    if c == ' ' {
                        continue;
                    }
                }
                part.push(c);
            }
            used += width(&part);
            lines
                .last_mut()
                .expect("one line")
                .push(Span::styled(part, st));
        }
    }
    lines.into_iter().map(Line::from).collect()
}

/// From this many columns a full-width list (Slow log, Clients, Nodes) has its details
/// to its right, over the full height, rather than under it.
pub const SIDE_AT: u16 = 120;

/// The list's share of the width when its details are to its right, in percent.
const SIDE_LIST_PCT: u16 = 60;

/// The list's width, border included, when its details go to its right at `total`
/// columns: about three fifths. Below [`SIDE_AT`], `None`: the details go under it.
pub fn side_split(total: u16) -> Option<u16> {
    // In u32: a terminal can be up to u16::MAX columns wide.
    (total >= SIDE_AT).then(|| {
        u16::try_from(u32::from(total) * u32::from(SIDE_LIST_PCT) / 100).unwrap_or(u16::MAX)
    })
}

/// A list and its details in one frame: from [`SIDE_AT`] columns side by side, the
/// list about 60% and the details the rest, both at full height; below it stacked as
/// [`sections`] with `list` and `details` (their `rows` apply only there). `focus` is
/// the pane with the keys (0 the list, 1 the details). Returns their inner areas.
pub fn list_and_details(
    frame: &mut Frame,
    area: Rect,
    list: Section<'_>,
    details: Section<'_>,
    focus: usize,
) -> (Rect, Rect) {
    let inner = match side_split(area.width) {
        Some(lw) => frame_panes(frame, area, &[lw, 1], &[list.title, details.title], focus),
        None => sections_focused(frame, area, &[list, details], focus),
    };
    (
        inner.first().copied().unwrap_or_default(),
        inner.get(1).copied().unwrap_or_default(),
    )
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
/// gets no room has an empty one. The first section has the keys: see
/// [`sections_focused`].
pub fn sections(frame: &mut Frame, area: Rect, list: &[Section<'_>]) -> Vec<Rect> {
    sections_focused(frame, area, list, 0)
}

/// [`sections`] where section `focus` has the keys (its list has the selection): its
/// title is bold in the focus colour, and its border, with the border under it, is in
/// the focus colour. The other sections keep the normal border and a dim title.
pub fn sections_focused(
    frame: &mut Frame,
    area: Rect,
    list: &[Section<'_>],
    focus: usize,
) -> Vec<Rect> {
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
    let (on, off) = (style::focus(), style::border());
    let mut side = off;
    for (i, sec) in list.iter().enumerate() {
        if y >= bottom {
            break;
        }
        let (start, end) = if i == 0 {
            ("┌", "┐")
        } else {
            ("├", "┤")
        };
        let st = if i == focus {
            style::title()
        } else {
            style::hd()
        };
        // The line above the focused section and the one under it are its own.
        let line = if i == focus || i == focus + 1 {
            on
        } else {
            off
        };
        put(
            frame,
            area.x,
            y,
            area.width,
            border_line(w, start, end, sec.title, st, line),
        );
        side = if i == focus { on } else { off };
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
            put_char(frame, area.x, row, "│", side);
            put_char(frame, area.x + area.width - 1, row, "│", side);
        }
        out[i] = Rect::new(area.x + 1, y, area.width - 2, h);
        y += h;
    }
    // Rows no section took stay inside the frame, and the bottom border belongs to the
    // last section.
    for row in y..bottom {
        put_char(frame, area.x, row, "│", side);
        put_char(frame, area.x + area.width - 1, row, "│", side);
    }
    let mut bot = vec![Span::styled("└", side)];
    bot.push(Span::styled("─".repeat(w.saturating_sub(2)), side));
    bot.push(Span::styled("┘", side));
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

/// A box at `rect` over the screen, not dimming it: cleared, its border in the focus
/// colour, `title` bold and `right` dim in the top border. Returns its inner area. The
/// Actions menu and the confirm sheet draw in one.
pub fn focus_box(frame: &mut Frame, rect: Rect, title: &str, right: &str) -> Rect {
    let rect = rect.intersection(frame.area());
    frame.render_widget(ratatui::widgets::Clear, rect);
    titled_box(frame, rect, Title::new(title, right), true)
}

/// A closed box at `rect` with `t` in its top border, drawn over what is there (nothing
/// cleared). `focused`: the box has the keys, its border in the focus colour and its
/// title bold in it; else the normal border and a dim title. Returns its inner area,
/// empty when `rect` is too small for a box.
pub fn titled_box(frame: &mut Frame, rect: Rect, t: Title<'_>, focused: bool) -> Rect {
    let rect = rect.intersection(frame.area());
    if rect.width < 4 || rect.height < 2 {
        return Rect::default();
    }
    let (title_st, line) = if focused {
        (style::title(), style::focus())
    } else {
        (style::dim(), style::border())
    };
    let w = usize::from(rect.width);
    put(
        frame,
        rect.x,
        rect.y,
        rect.width,
        border_line(w, "┌", "┐", t, title_st, line),
    );
    let bottom = rect.y + rect.height - 1;
    for row in rect.y + 1..bottom {
        put_char(frame, rect.x, row, "│", line);
        put_char(frame, rect.x + rect.width - 1, row, "│", line);
    }
    put(
        frame,
        rect.x,
        bottom,
        rect.width,
        vec![
            Span::styled("└", line),
            Span::styled("─".repeat(w.saturating_sub(2)), line),
            Span::styled("┘", line),
        ],
    );
    Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width - 2,
        rect.height.saturating_sub(2),
    )
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

/// Cells the marker gutter takes at the left of a list: a space and `›`.
pub const GUTTER: u16 = 2;

/// A width threshold set for a list's whole inner width, measured instead on the
/// width after the gutter ([`gutter_area`]): it still switches at the same terminal
/// width.
pub const fn after_gutter(at: usize) -> usize {
    at.saturating_sub(GUTTER as usize)
}

/// `area` less the marker gutter on its left: where a list's column header and rows
/// go, so they line up. See [`list`].
pub fn gutter_area(area: Rect) -> Rect {
    let g = GUTTER.min(area.width);
    Rect {
        x: area.x + g,
        width: area.width - g,
        ..area
    }
}

/// The marker on a list's selected row: `›` in the focus colour, after a space.
pub fn marker(selected: bool) -> [Span<'static>; 2] {
    [
        Span::raw(" "),
        Span::styled(if selected { "›" } else { " " }, style::focus()),
    ]
}

/// A list's rows in `area`, one per row from the top, after a 2-cell gutter: the
/// `selected` row gets the selection background across the whole row, gutter included,
/// and `›` in the focus colour in the gutter. The rows are drawn in
/// [`gutter_area`]`(area)`, so build them for its width, and draw the column header
/// there too.
pub fn list(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, selected: Option<usize>) {
    let n = lines.len().min(usize::from(area.height));
    for i in 0..n {
        let r = Rect {
            y: area.y + i as u16,
            height: 1,
            width: GUTTER.min(area.width),
            ..area
        };
        let on = selected == Some(i);
        let line = Line::from(marker(on).to_vec());
        frame.render_widget(if on { line.style(style::sel()) } else { line }, r);
    }
    rows(frame, gutter_area(area), lines, selected);
}

/// Lines in `area` where a list follows text: the first `full` lines (a status line,
/// facts) across the whole width, then the column header and the rows as a [`list`]
/// behind the marker gutter. `selected` counts from the first line.
pub fn list_below(
    frame: &mut Frame,
    area: Rect,
    mut lines: Vec<Line<'static>>,
    full: usize,
    selected: Option<usize>,
) {
    let full = full.min(lines.len()).min(usize::from(area.height));
    let rest = lines.split_off(full);
    rows(frame, area, lines, None);
    let n = full as u16;
    let below = Rect {
        y: area.y + n,
        height: area.height - n,
        ..area
    };
    list(
        frame,
        below,
        rest,
        selected.and_then(|s| s.checked_sub(full)),
    );
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
        let spans = ttl_hist_spans(&[0, 10, 0, 0, 0, 0, 5], style::warning());
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

    fn buffer_with(w: u16, h: u16, f: impl FnOnce(&mut Frame)) -> ratatui::buffer::Buffer {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(f).unwrap();
        t.backend().buffer().clone()
    }

    /// The focused pane's border, the shared divider with it, is in the focus colour
    /// and its title bold in it; the other pane keeps the normal border, a dim title.
    #[test]
    fn a_split_frame_draws_the_focused_pane_in_the_focus_colour() {
        let draw = |focus| {
            buffer_with(30, 4, |f| {
                split_frame(
                    f,
                    f.area(),
                    14,
                    Title::new("List", ""),
                    Title::new("Detail", ""),
                    focus,
                );
            })
        };
        let (on, off) = (style::FOCUS, style::BORDER);
        let b = draw(Pane::Left);
        // Left edge, top, bottom, the divider: focus; right edge: normal.
        for (x, y) in [(0, 0), (0, 1), (5, 0), (5, 3), (14, 0), (14, 1), (14, 3)] {
            assert_eq!(b[(x, y)].fg, on, "({x}, {y})");
        }
        for (x, y) in [(25, 0), (29, 0), (29, 1), (20, 3), (29, 3)] {
            assert_eq!(b[(x, y)].fg, off, "({x}, {y})");
        }
        assert_eq!(b[(3, 0)].style(), b[(3, 0)].style().patch(style::title()));
        assert_eq!(b[(17, 0)].fg, style::DIM);
        let b = draw(Pane::Right);
        for (x, y) in [(14, 0), (14, 1), (14, 3), (29, 1), (29, 3), (25, 0)] {
            assert_eq!(b[(x, y)].fg, on, "({x}, {y})");
        }
        for (x, y) in [(0, 0), (0, 1), (5, 3)] {
            assert_eq!(b[(x, y)].fg, off, "({x}, {y})");
        }
        assert_eq!(b[(17, 0)].fg, style::FOCUS);
        assert_eq!(b[(3, 0)].fg, style::DIM);
    }

    /// The first section has the keys: its border down to the one under it is in the
    /// focus colour, the next section's sides are not.
    #[test]
    fn sections_draw_the_first_in_the_focus_colour() {
        let b = buffer_with(20, 7, |f| {
            sections(
                f,
                f.area(),
                &[
                    Section {
                        title: Title::new("Now", ""),
                        rows: Some(1),
                    },
                    Section {
                        title: Title::new("Keys", ""),
                        rows: None,
                    },
                ],
            );
        });
        for (x, y) in [(0, 0), (19, 0), (0, 1), (19, 1), (0, 2), (10, 2), (19, 2)] {
            assert_eq!(b[(x, y)].fg, style::FOCUS, "({x}, {y})");
        }
        for (x, y) in [(0, 3), (19, 4), (0, 6), (10, 6)] {
            assert_eq!(b[(x, y)].fg, style::BORDER, "({x}, {y})");
        }
        assert_eq!(b[(3, 0)].fg, style::FOCUS);
        assert_ne!(b[(3, 2)].fg, style::FOCUS, "the second title is not");
    }

    /// A list's selected row: `›` in the focus colour in a 2-cell gutter, on the
    /// selection background across the row; the rows start after the gutter.
    #[test]
    fn a_list_marks_its_selected_row_in_the_gutter() {
        let area = Rect::new(1, 0, 12, 3);
        assert_eq!(gutter_area(area), Rect::new(3, 0, 10, 3));
        let b = buffer_with(14, 3, |f| {
            list(
                f,
                area,
                vec![Line::raw(" one"), Line::raw(" two"), Line::raw(" three")],
                Some(1),
            );
        });
        let row = |y: u16| (0..14).map(|x| b[(x, y)].symbol()).collect::<String>();
        assert_eq!(row(0), "    one       ");
        assert_eq!(row(1), "  › two       ");
        assert_eq!(row(2), "    three     ");
        assert_eq!(b[(2, 1)].fg, style::FOCUS);
        for x in 1..13 {
            assert_eq!(b[(x, 1)].bg, style::SEL, "{x}");
        }
        assert_ne!(b[(1, 0)].bg, style::SEL);
        // Nothing selected: no marker.
        let b = buffer_with(14, 1, |f| list(f, area, vec![Line::raw(" one")], None));
        assert_eq!(b[(2, 0)].symbol(), " ");
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

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wrap_spans_breaks_at_spaces_and_hard_wraps_a_long_word() {
        let l = wrap_spans(
            vec![Span::raw(
                " Node    a1f3c9e27b4d6e8f0a2c4e6b8d0f1a3c5e7b9d1f",
            )],
            21,
            9,
        );
        let t = plain(&l);
        // Nothing lost: the ID is broken over lines, each line within 20 cells.
        assert_eq!(
            t.concat().replace(' ', ""),
            "Nodea1f3c9e27b4d6e8f0a2c4e6b8d0f1a3c5e7b9d1f"
        );
        assert!(t.iter().all(|x| width(x) <= 20), "{t:?}");
        assert!(t.len() >= 3, "{t:?}");
        // Wide characters count two cells.
        let l = wrap_spans(vec![Span::raw(" 漢字漢字漢字漢字漢字漢字")], 11, 1);
        let t = plain(&l);
        assert!(t.iter().all(|x| width(x) <= 10), "{t:?}");
        assert_eq!(t.concat().replace(' ', ""), "漢字漢字漢字漢字漢字漢字");
        // Words that fit move to the next line whole.
        let t = plain(&wrap_spans(
            vec![Span::raw(" on node 10.0.1.13:6379")],
            18,
            1,
        ));
        assert_eq!(t, [" on node", " 10.0.1.13:6379"]);
        for w in 0..30 {
            let _ = wrap_spans(vec![Span::raw("abc defghijklmnop q")], w, 3);
        }
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
