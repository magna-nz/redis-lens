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

    #[test]
    fn placeholders() {
        let s = pattern_spans("user:{id}:cart", style::fg(), style::ph());
        let parts: Vec<&str> = s.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(parts, ["user:", "{id}", ":cart"]);
    }
}
