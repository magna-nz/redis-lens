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
