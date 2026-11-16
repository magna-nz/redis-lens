//! The Go to box (`ctrl-k`, [`crate::tui::app::goto`]): near the top of the body, over
//! the dimmed screen, in the focus colour, titled `Go to` with the match count at the
//! right. Inside: the input line `› text▏`, a rule, the rows in their groups, a rule,
//! and the keys `↑↓ Pick   enter Go   tab Next group   esc Close` with, at the right,
//! `start with : for a command` (or, at the right of `↑↓ Pick`, that nothing is picked
//! or that the picked row is gone). Each group shows at most [`CAP`] rows (`… n more`
//! under them) until it is the group the picked row is in; the list scrolls to keep
//! the picked row in view, `↑ n more` and `↓ n more` on its first and last lines when
//! rows are left out above or below.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::patterns::flagged;
use super::style;
use super::widgets::{fit, focus_box, marker, rows, trunc, width};
use crate::tui::app::App;
use crate::tui::app::actions::short_reason;
use crate::tui::app::goto::{self as go_to, Go, Hit, Section};

/// Widest the box gets.
const MAX_W: u16 = 88;
/// Rows a group shows while the picked row is in another.
pub const CAP: usize = 3;

/// One line of the list: a heading, a blank between groups, a row (its index among
/// the hits), how many rows of a group are left out, or how many rows are scrolled
/// out above or below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Head(Section),
    Blank,
    Hit(usize),
    More(usize),
    Up(usize),
    Down(usize),
}

/// The list's lines, in order: each group under its heading, capped unless it holds
/// `sel`.
fn items(hits: &[Hit], sel: Option<usize>) -> Vec<Item> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < hits.len() {
        let section = hits[i].section;
        let end = (i..hits.len())
            .find(|&j| hits[j].section != section)
            .unwrap_or(hits.len());
        if !out.is_empty() {
            out.push(Item::Blank);
        }
        out.push(Item::Head(section));
        let open = sel.is_some_and(|s| (i..end).contains(&s));
        // The Find row is never left out: it is the way to the rest of the server.
        let finds: Vec<usize> = (i..end).filter(|&j| hits[j].go == Go::Find).collect();
        let names: Vec<usize> = (i..end).filter(|&j| hits[j].go != Go::Find).collect();
        let shown = if open {
            names.len()
        } else {
            CAP.min(names.len())
        };
        out.extend(names[..shown].iter().map(|&j| Item::Hit(j)));
        if shown < names.len() {
            out.push(Item::More(names.len() - shown));
        }
        out.extend(finds.into_iter().map(Item::Hit));
        i = end;
    }
    out
}

/// How many rows `items` stand for: a row one, a `… n more` its `n`.
fn rows_in(items: &[Item]) -> usize {
    items
        .iter()
        .map(|it| match it {
            Item::Hit(_) => 1,
            Item::More(n) => *n,
            _ => 0,
        })
        .sum()
}

/// The `room` lines of `list` on show: all of it when it fits; else a part that holds
/// the item at `at` (with its group's heading when that fits), `↑ n more` as its first
/// line when rows are left out above and `↓ n more` as its last when rows are below. A
/// `… n more` line is never the first one shown: its heading would be out of view.
fn window(list: &[Item], at: Option<usize>, room: usize) -> Vec<Item> {
    if list.len() <= room {
        return list.to_vec();
    }
    if room < 3 {
        return list[..room].to_vec();
    }
    let at = at.unwrap_or(0);
    // Two lines kept for the marks.
    let inner = room - 2;
    let head = list[..=at]
        .iter()
        .rposition(|it| matches!(it, Item::Head(_)))
        .unwrap_or(0);
    let mut first = if at - head < inner {
        head
    } else {
        at + 1 - inner
    };
    // The end in view: no blank lines under the last row while rows are out above
    // (one line for the `↑` mark).
    first = first.min(list.len() - (room - 1));
    // Pulled back into an earlier group: not as far as its `… n more` without its
    // heading.
    if first < head
        && let Some(m) = list[first..head]
            .iter()
            .rposition(|it| matches!(it, Item::More(_)))
    {
        first += m + 1;
    }
    // Only a heading above: no mark for it, the heading itself.
    if rows_in(&list[..first]) == 0 {
        first = 0;
    }
    // A blank or a `… n more` under a heading out of view is left out with it.
    while first > 0 && first < at && matches!(list[first], Item::Blank | Item::More(_)) {
        first += 1;
    }
    let up = first > 0;
    let mut end = (first + room - usize::from(up)).min(list.len());
    let down = end < list.len();
    if down {
        end -= 1;
    }
    let mut out = Vec::with_capacity(room);
    if up {
        out.push(Item::Up(rows_in(&list[..first])));
    }
    out.extend_from_slice(&list[first..end]);
    if down {
        out.push(Item::Down(rows_in(&list[end..])));
    }
    out
}

/// `label` with its matched characters in the focus colour and, on a pattern, its
/// `{…}` parts in the placeholder colour.
fn label_spans(h: &Hit, base: Style) -> Vec<Span<'static>> {
    let pattern = h.section == Section::Patterns;
    let (ms, ml) = h.matched.unwrap_or((0, 0));
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut cur_style = base;
    let mut depth = 0;
    for (i, c) in h.label.chars().enumerate() {
        if pattern && c == '{' {
            depth += 1;
        }
        let st = if i >= ms && i < ms + ml {
            style::focus()
        } else if depth > 0 {
            style::ph()
        } else {
            base
        };
        if pattern && c == '}' && depth > 0 {
            depth -= 1;
        }
        if st != cur_style && !cur.is_empty() {
            out.push(Span::styled(std::mem::take(&mut cur), cur_style));
        }
        cur_style = st;
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, cur_style));
    }
    out
}

/// The right of a row: why it is off, its keys as caps, or its note (and `▲`).
fn right_spans(h: &Hit, connected: bool) -> Vec<Span<'static>> {
    if let Some(why) = &h.off {
        let short = if connected {
            short_reason(why)
        } else {
            "connect first".into()
        };
        return vec![Span::styled(format!("off: {short}"), style::dim())];
    }
    let mut out = Vec::new();
    if !h.note.is_empty() {
        out.push(Span::styled(h.note.clone(), style::dim()));
    }
    if h.verdict.is_some_and(flagged) {
        out.push(Span::styled("  ▲", style::amber()));
    }
    for (i, k) in h.keys.iter().enumerate() {
        if i > 0 || !out.is_empty() {
            out.push(Span::raw(" "));
        }
        out.push(Span::styled(format!(" {k} "), style::key()));
    }
    out
}

fn spans_w(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// One row `w` cells wide: the gutter, the name (cut first), the right.
fn hit_line(h: &Hit, selected: bool, w: usize, connected: bool) -> Line<'static> {
    let base = if h.off.is_some() || h.go == Go::Find {
        style::dim()
    } else {
        style::fg()
    };
    let right = right_spans(h, connected);
    let rw = spans_w(&right).min(w.saturating_sub(12));
    let right = fit(right, rw);
    // The gutter and a space, the name, two spaces, the right, a space.
    let name_w = w.saturating_sub(3 + rw + 2 + 1);
    let mut spans = marker(selected).to_vec();
    spans.push(Span::raw(" "));
    spans.extend(fit(label_spans(h, base), name_w));
    spans.push(Span::raw("  "));
    spans.extend(right);
    Line::from(spans)
}

/// The input line: `› text▏`, the text's end in view; masked after a `:`.
fn input_line(text: &str, w: usize) -> Line<'static> {
    if text.is_empty() {
        return Line::from(vec![
            Span::styled(" › ", style::focus()),
            Span::styled("▏", style::focus()),
            Span::styled(
                trunc(
                    "a pattern, a key, a screen or an action",
                    w.saturating_sub(4),
                ),
                style::dim(),
            ),
        ]);
    }
    let shown = match go_to::command_text(text) {
        Some(rest) => format!(":{}", crate::tui::app::views::cmdline::masked_input(rest)),
        None => crate::value::escape(text),
    };
    let room = w.saturating_sub(4);
    let chars: Vec<char> = shown.chars().collect();
    let shown = if width(&shown) > room {
        let mut tail = String::from("…");
        let mut used = 1;
        let mut keep = Vec::new();
        for c in chars.iter().rev() {
            let cw = super::widgets::char_width(*c);
            if used + cw > room {
                break;
            }
            used += cw;
            keep.push(*c);
        }
        tail.extend(keep.into_iter().rev());
        tail
    } else {
        shown
    };
    Line::from(vec![
        Span::styled(" › ", style::focus()),
        Span::styled(shown, style::fgb()),
        Span::styled("▏", style::focus()),
    ])
}

/// The keys at the bottom of the box, and the `:` hint at the right when it fits. With
/// `say` (nothing is picked, the picked row is gone): `↑↓ Pick`, `esc Close` and that.
fn keys_line(w: usize, say: Option<&str>) -> Line<'static> {
    if let Some(say) = say {
        let mut out = vec![Span::raw(" ")];
        for (i, (k, l)) in [("↑↓", "Pick"), ("esc", "Close")].iter().enumerate() {
            if i > 0 {
                out.push(Span::raw("  "));
            }
            out.push(Span::styled(format!(" {k} "), style::key()));
            out.push(Span::styled(format!(" {l}"), style::dim()));
        }
        let used = spans_w(&out) + 3;
        out.push(Span::raw("   "));
        out.push(Span::styled(
            trunc(say, w.saturating_sub(used + 1)),
            style::amber(),
        ));
        return Line::from(out);
    }
    let mut left = vec![Span::raw(" ")];
    for (i, (k, l)) in [
        ("↑↓", "Pick"),
        ("enter", "Go"),
        ("tab", "Next group"),
        ("esc", "Close"),
    ]
    .iter()
    .enumerate()
    {
        if i > 0 {
            left.push(Span::raw("  "));
        }
        left.push(Span::styled(format!(" {k} "), style::key()));
        left.push(Span::styled(format!(" {l}"), style::dim()));
    }
    let right = vec![
        Span::styled("start with ", style::dim()),
        Span::styled(" : ", style::key()),
        Span::styled(" for a command", style::dim()),
    ];
    let (lw, rw) = (spans_w(&left), spans_w(&right));
    if lw + 2 + rw < w {
        left.push(Span::raw(" ".repeat(w - lw - rw - 1)));
        left.extend(right);
    }
    Line::from(left)
}

fn rule(w: usize) -> Line<'static> {
    Line::from(Span::styled("─".repeat(w), style::border()))
}

/// The title's right: how many rows match, not counting the Find row; nothing with no
/// text or after a `:`.
fn count_text(hits: &[Hit], empty: bool) -> String {
    if empty {
        return String::new();
    }
    match hits.iter().filter(|h| h.counts()).count() {
        0 => "no matches".into(),
        1 => "1 match".into(),
        n => format!("{n} matches"),
    }
}

/// Draws the open box in `body`.
pub fn draw(frame: &mut Frame, body: Rect, app: &App) {
    let Some(g) = &app.goto else {
        return;
    };
    let hits = app.goto_hits();
    let sel = app.goto_index(&hits);
    let say = g.say.or_else(|| sel.is_none().then_some(go_to::PICK_A_ROW));
    let w = body.width.saturating_sub(4).min(MAX_W);
    if w < 24 || body.height < 7 {
        return;
    }
    let iw = usize::from(w - 2);
    let list = items(&hits, sel);
    // Borders, input, two rules and the keys around the list; a row of the body free
    // above and below.
    let fixed = 2 + 4;
    let max_h = body.height.saturating_sub(2);
    let want = u16::try_from(list.len() + fixed).unwrap_or(u16::MAX);
    let h = want.min(max_h).max(7);
    let x = body.x + (body.width - w) / 2;
    let y = body.y + 1;
    let rect = Rect::new(x, y, w, h.min(body.height - 1));
    let inner = focus_box(
        frame,
        rect,
        "Go to",
        &count_text(
            &hits,
            g.text.trim().is_empty() || go_to::command_text(&g.text).is_some(),
        ),
    );
    let room = usize::from(inner.height).saturating_sub(4).max(1);
    let at = sel.and_then(|s| list.iter().position(|it| *it == Item::Hit(s)));
    let connected = app.session.is_some();
    let moved = app.goto_actions_moved();
    let mut lines = vec![input_line(&g.text, iw), rule(iw)];
    let mut selected = None;
    for it in window(&list, at, room) {
        lines.push(match it {
            Item::Blank => Line::default(),
            Item::Head(s) => {
                let mut spans = vec![Span::raw(" "), Span::styled(s.title(), style::hd())];
                let note = match s {
                    Section::Keys => Some(("already loaded".to_string(), style::dim())),
                    Section::Actions if moved => {
                        Some((go_to::ACTIONS_MOVED_HEAD.to_string(), style::amber()))
                    }
                    Section::Actions if !g.on.is_empty() => {
                        Some((format!("on {}", g.on), style::dim()))
                    }
                    _ => None,
                };
                if let Some((t, st)) = note {
                    let room = iw.saturating_sub(width(s.title()) + 4);
                    spans.push(Span::styled(format!("  {}", trunc(&t, room)), st));
                }
                Line::from(spans)
            }
            Item::More(k) => Line::from(Span::styled(format!("   … {k} more"), style::dim())),
            Item::Up(k) => Line::from(Span::styled(format!("   ↑ {k} more"), style::dim())),
            Item::Down(k) => Line::from(Span::styled(format!("   ↓ {k} more"), style::dim())),
            Item::Hit(i) => {
                let picked = sel == Some(i);
                if picked {
                    selected = Some(lines.len());
                }
                hit_line(&hits[i], picked, iw, connected)
            }
        });
    }
    while lines.len() < 2 + room {
        lines.push(Line::default());
    }
    lines.push(rule(iw));
    lines.push(keys_line(iw, say));
    rows(frame, inner, lines, selected);
}

/// The footer while the box is open.
pub fn footer() -> Vec<(&'static str, &'static str)> {
    vec![("↑↓", "Pick"), ("enter", "Go"), ("esc", "Close")]
}

/// Dim after the footer's keys.
pub const FOOTER_NOTE: &str = "typing reads nothing from the server";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::goto::Hit as H;

    fn hit(section: Section, n: usize) -> Vec<H> {
        (0..n)
            .map(|i| H {
                section,
                label: format!("{i}"),
                matched: None,
                rank: None,
                note: String::new(),
                keys: Vec::new(),
                off: None,
                verdict: None,
                here: false,
                go: Go::Pattern(format!("{i}")),
            })
            .collect()
    }

    #[test]
    fn a_group_is_capped_until_the_pick_is_in_it() {
        let mut hits = hit(Section::Patterns, 5);
        hits.extend(hit(Section::Screens, 2));
        let shown = items(&hits, Some(6));
        assert_eq!(
            shown,
            [
                Item::Head(Section::Patterns),
                Item::Hit(0),
                Item::Hit(1),
                Item::Hit(2),
                Item::More(2),
                Item::Blank,
                Item::Head(Section::Screens),
                Item::Hit(5),
                Item::Hit(6),
            ]
        );
        let open = items(&hits, Some(3));
        assert_eq!(open.iter().filter(|i| matches!(i, Item::Hit(_))).count(), 7);
        assert!(!open.iter().any(|i| matches!(i, Item::More(_))));
    }

    /// For lists of up to four groups of up to seven rows, every pick (or none) and
    /// every height: the part shown fits, holds the picked row, says how many rows are
    /// out of view above and below (the shown and the left-out ones add up), and never
    /// shows a `… n more` without its group's heading above it.
    #[test]
    fn the_window_holds_the_pick_and_marks_what_is_out_of_view() {
        let sections = [
            Section::Patterns,
            Section::Keys,
            Section::Screens,
            Section::Actions,
        ];
        let mut shapes: Vec<Vec<usize>> = vec![vec![]];
        for _ in 0..4 {
            let mut next = Vec::new();
            for s in &shapes {
                for n in 1..=7 {
                    let mut t = s.clone();
                    t.push(n);
                    next.push(t);
                }
            }
            shapes.extend(next.into_iter().filter(|s| s.len() <= 4));
            shapes.sort();
            shapes.dedup();
        }
        let mut checked = 0;
        for shape in shapes.iter().filter(|s| !s.is_empty()) {
            let mut hits = Vec::new();
            for (s, n) in sections.iter().zip(shape) {
                hits.extend(hit(*s, *n));
            }
            let picks = std::iter::once(None).chain((0..hits.len()).map(Some));
            for sel in picks {
                let list = items(&hits, sel);
                let total = rows_in(&list);
                let at = sel.and_then(|s| list.iter().position(|it| *it == Item::Hit(s)));
                for room in 3..=list.len() + 1 {
                    let w = window(&list, at, room);
                    let what = format!("{shape:?} sel={sel:?} room={room}: {w:?}");
                    assert!(w.len() <= room, "{what}");
                    if let Some(s) = sel {
                        assert!(w.contains(&Item::Hit(s)), "{what}");
                    }
                    let up = match w.first() {
                        Some(Item::Up(n)) => *n,
                        _ => 0,
                    };
                    let down = match w.last() {
                        Some(Item::Down(n)) => *n,
                        _ => 0,
                    };
                    assert_eq!(up + rows_in(&w) + down, total, "{what}");
                    for (i, it) in w.iter().enumerate() {
                        if matches!(it, Item::More(_)) {
                            assert!(w[..i].iter().any(|x| matches!(x, Item::Head(_))), "{what}");
                        }
                    }
                    assert!(!matches!(w.first(), Some(Item::More(_))), "{what}");
                    // Scrolled: the box is filled, unless the rows above would
                    // have shown a `… n more` without its heading, and a heading
                    // starts what is shown instead.
                    if up > 0 {
                        assert!(
                            w.len() + 2 >= room || matches!(w.get(1), Some(Item::Head(_))),
                            "{what}"
                        );
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 1000, "{checked}");
    }
}
