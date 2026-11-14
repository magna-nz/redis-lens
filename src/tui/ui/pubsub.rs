//! Draws [`PubSubState`]. Owner: wave B, with `views/pubsub.rs`.
//!
//! Before any subscription: the active channels with how many clients subscribe to each,
//! and the totals below. Once the live tail started: the channels on the left and the
//! tail on the right (TIME · CHANNEL · MESSAGE), newest at the bottom. Every server
//! string drawn here, channel names included, goes through [`value::escape`] or
//! [`value::preview`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::format;
use crate::live::pubsub::{MAX_CHANNELS, TAIL_KEPT};
use crate::tui::app::views::Ctx;
use crate::tui::app::views::pubsub::{PubSubState, Selected, Tail, TailRow, TailStatus};
use crate::value;

use super::analysis::{left_width, notice, paragraph};
use super::streams::live_wait;
use super::style;
use super::widgets::{
    Pane, Title, bar, fit, pad_l, pad_r, rows, scroll_offset, split_frame, width,
};

const SUBS_W: usize = 6;
const BAR_W: usize = 16;
/// `HH:MM:SS.mmm`
const TIME_W: usize = 12;

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &PubSubState, ctx: &Ctx<'_>) {
    draw_view(frame, area, state, ctx);
    if let (Some(form), Some(sends)) = (&state.publish, state.form_sends()) {
        super::streams::draw_form(frame, area, form, &sends, ctx);
    }
}

fn draw_view(frame: &mut Frame, area: Rect, state: &PubSubState, ctx: &Ctx<'_>) {
    const TITLE: &str = "Pub/Sub";
    if let Some(off) = &state.off {
        let lines = vec![Line::from(Span::styled(
            format!(" ▲ {off}"),
            style::amber(),
        ))];
        notice(frame, area, TITLE, "off", lines);
        return;
    }
    if state.tail.status != TailStatus::None {
        // The channels on the left, the tail on the right, in one frame.
        let left_w = if area.width >= 120 {
            35
        } else {
            (area.width * 2 / 7).clamp(21, 31)
        };
        let (title, tail_right) =
            tail_title(&state.tail, usize::from(area.width.saturating_sub(left_w)));
        let (chans, tail) = split_frame(
            frame,
            area,
            left_w,
            Title::new("Channels", &channels_right(state)),
            Title::new(&title, &tail_right),
            Pane::Right,
        );
        channels_narrow(frame, chans, state, ctx);
        draw_tail(frame, tail, &state.tail);
        return;
    }
    if !state.loaded {
        let text = live_wait(ctx).unwrap_or_else(|| "Reading the channels…".into());
        let lines = vec![Line::from(Span::styled(format!(" {text}"), style::dim()))];
        notice(frame, area, TITLE, "", lines);
        return;
    }
    let mut right = String::new();
    if !ctx.live_ready() {
        right.push_str("not live · ");
    }
    if state.paused {
        right.push_str("paused · ");
    }
    match ctx.node_title() {
        Some(t) => right.push_str(&t),
        None => right.push_str("most subscribers first"),
    }
    let selected = state.selected().map(|s| match s {
        Selected::Channel(ch) => value::escape(&ch.name),
        Selected::Shard(r) => value::escape(&r.name),
    });
    let (list, detail) = split_frame(
        frame,
        area,
        left_width(area.width),
        Title::new(TITLE, &right),
        Title::new(selected.as_deref().unwrap_or("Totals"), ""),
        Pane::Left,
    );
    let w = usize::from(list.width).saturating_sub(1);
    let (head, lines, sel) = list_lines(state, ctx, w, ctx.more && w >= 50);
    let body = if head.spans.is_empty() {
        list
    } else {
        below_header(list)
    };
    rows(frame, list, vec![head], None);
    let at = sel.unwrap_or(0);
    let off = scroll_offset(at, usize::from(body.height));
    let lines: Vec<Line<'static>> = lines
        .into_iter()
        .skip(off)
        .take(usize::from(body.height))
        .collect();
    rows(frame, body, lines, sel.map(|s| s - off));
    totals(frame, detail, state, ctx);
}

/// The list of the view, below its header: the classic channels, then the shard
/// channels (P4) under a header of their own, with a NODE column on a cluster. Returns
/// the header, the lines and the line of the selected row. `bars`: the wide layout,
/// with a bar per classic channel.
fn list_lines(
    state: &PubSubState,
    ctx: &Ctx<'_>,
    w: usize,
    bars: bool,
) -> (Line<'static>, Vec<Line<'static>>, Option<usize>) {
    let c = &state.counts;
    let has_counts = c.channels.iter().any(|ch| ch.subscribers.is_some())
        || state.shards.iter().any(|r| r.subscribers.is_some());
    let sw = if has_counts { SUBS_W + 1 } else { 0 };
    let bw = if bars { BAR_W + 1 } else { 0 };
    let kw = w.saturating_sub(sw + bw + usize::from(bars)).max(8);
    let mut head = vec![Span::styled(pad_r(" CHANNEL", kw), style::hd())];
    if has_counts {
        head.push(Span::styled(
            format!(" {}", pad_l("SUBS", SUBS_W)),
            style::hd(),
        ));
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut sel = None;
    if c.channels.is_empty() {
        lines.push(Line::from(fit(
            vec![Span::styled(" No active channels.", style::fgb())],
            w,
        )));
        if bars {
            lines.push(Line::from(fit(
                vec![Span::styled(
                    " A channel exists only while a client subscribes to it.",
                    style::dim(),
                )],
                w,
            )));
        }
    }
    let max = c
        .channels
        .iter()
        .filter_map(|ch| ch.subscribers)
        .max()
        .unwrap_or(0);
    for (i, ch) in c.channels.iter().enumerate() {
        if i == state.sel {
            sel = Some(lines.len());
        }
        let mut spans = fit(
            vec![Span::styled(
                format!(" {}", value::escape(&ch.name)),
                style::fg(),
            )],
            kw,
        );
        if let Some(n) = ch.subscribers {
            spans.push(Span::styled(
                format!(" {}", pad_l(&format::grouped(n), SUBS_W)),
                style::fgb(),
            ));
            if bars {
                let ratio = if max == 0 { 0.0 } else { n as f64 / max as f64 };
                let (fill, track) = bar(ratio, BAR_W);
                spans.push(Span::raw(" "));
                spans.push(Span::styled(fill, style::ok()));
                spans.push(Span::styled(track, style::track()));
            }
        }
        lines.push(Line::from(spans));
    }
    // The shard channels, once read or left out.
    if !state.shards_loaded && state.shards_off.is_none() {
        // No header over "No active channels" alone.
        let head = if c.channels.is_empty() {
            Line::default()
        } else {
            Line::from(head)
        };
        return (head, lines, sel);
    }
    let by_node = ctx.session.is_cluster();
    let nw = if by_node { super::keys::NODE_W } else { 0 };
    let skw = kw.saturating_sub(nw).max(8);
    let mut shard_head = vec![Span::styled(pad_r(" SHARD CHANNEL", skw), style::hd())];
    if by_node {
        shard_head.push(Span::styled(pad_r(" NODE", nw), style::hd()));
    }
    shard_head.push(Span::styled(
        format!(" {}", pad_l("SUBS", SUBS_W)),
        style::hd(),
    ));
    lines.push(Line::from(fit(shard_head, w)));
    if let Some(why) = &state.shards_off {
        lines.push(Line::from(fit(
            vec![Span::styled(format!(" ▲ {why}"), style::amber())],
            w,
        )));
        return (Line::from(head), lines, sel);
    }
    if state.shards.is_empty() {
        lines.push(Line::from(fit(
            vec![Span::styled(" No active shard channels.", style::dim())],
            w,
        )));
    }
    let n = c.channels.len();
    for (i, r) in state.shards.iter().enumerate() {
        if n + i == state.sel {
            sel = Some(lines.len());
        }
        let mut spans = fit(
            vec![Span::styled(
                format!(" {}", value::escape(&r.name)),
                style::fg(),
            )],
            skw,
        );
        if by_node {
            let name = super::keys::node_name(ctx, Some(r.node));
            spans.push(Span::styled(
                format!(" {}", pad_r(&super::widgets::trunc(&name, nw - 1), nw - 1)),
                style::dim(),
            ));
        }
        if let Some(k) = r.subscribers {
            spans.push(Span::styled(
                format!(" {}", pad_l(&format::grouped(k), SUBS_W)),
                style::fgb(),
            ));
        }
        lines.push(Line::from(spans));
    }
    (Line::from(head), lines, sel)
}

/// `inner` without its first row (the column header).
fn below_header(inner: Rect) -> Rect {
    Rect {
        y: inner.y + inner.height.min(1),
        height: inner.height.saturating_sub(1),
        ..inner
    }
}

/// The channel list beside the tail: names and subscriber counts, no bars.
fn channels_right(state: &PubSubState) -> String {
    let mut right = String::new();
    if state.paused {
        right.push_str("paused · ");
    }
    if state.loaded {
        right.push_str(&format::grouped(state.counts.total_channels as u64));
    }
    right
}

fn channels_narrow(frame: &mut Frame, inner: Rect, state: &PubSubState, ctx: &Ctx<'_>) {
    let w = usize::from(inner.width).saturating_sub(1);
    if !state.loaded {
        let text = format!(
            " {}",
            live_wait(ctx).unwrap_or_else(|| "Reading the channels…".into())
        );
        rows(
            frame,
            inner,
            vec![Line::from(fit(vec![Span::styled(text, style::dim())], w))],
            None,
        );
        return;
    }
    let (head, lines, sel) = list_lines(state, ctx, w, false);
    let body = if head.spans.is_empty() {
        inner
    } else {
        below_header(inner)
    };
    rows(frame, inner, vec![head], None);
    let at = sel.unwrap_or(0);
    let off = scroll_offset(at, usize::from(body.height));
    let lines: Vec<Line<'static>> = lines
        .into_iter()
        .skip(off)
        .take(usize::from(body.height))
        .collect();
    rows(frame, body, lines, sel.map(|s| s - off));
}

/// `HH:MM:SS.mmm`, UTC, of a receive stamp.
pub fn clock(at_unix_ms: i64) -> String {
    let ms = at_unix_ms.rem_euclid(86_400_000);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1_000 % 60,
        ms % 1_000
    )
}

/// Messages per second: one decimal under 10, whole numbers above.
fn rate(r: f64) -> String {
    if r < 10.0 {
        format!("{r:.1}")
    } else {
        format::grouped(r.round() as u64)
    }
}

/// The tail panel's title and the text on the right of its top border:
/// `Subscribed: <channel or pattern>` and `<rate> msg/s · <kept> kept (last 2,000)`.
/// When the right side would be cut in a panel `panel_w` wide, its last parts are left
/// out instead, the pause first kept.
fn tail_title(t: &Tail, panel_w: usize) -> (String, String) {
    let verb = match t.status {
        TailStatus::Subscribing => "Subscribing",
        TailStatus::Live => "Subscribed",
        _ => "Stopped",
    };
    let what = t.to.as_ref().map_or_else(String::new, |to| {
        let name = value::escape(to.name());
        if to.is_pattern() {
            format!("pattern {name}")
        } else if to.is_shard() {
            format!("shard {name}")
        } else {
            name
        }
    });
    let mut right = Vec::new();
    if t.paused() {
        right.push(format!(
            "paused · {} new",
            format::grouped(t.new_while_paused)
        ));
    }
    if t.status == TailStatus::Live {
        right.push(format!("{} msg/s", rate(t.rate)));
    }
    let kept = format::grouped(t.rows.len() as u64);
    right.push(format!(
        "{kept} kept (last {})",
        format::grouped(TAIL_KEPT as u64)
    ));
    let title = format!("{verb}: {what}");
    // What `panel` leaves for the right side: see `widgets::panel`.
    let room = panel_w
        .saturating_sub(4)
        .saturating_sub(width(&title) + 4)
        .saturating_sub(3);
    let mut text = right.join(" · ");
    if width(&text) > room {
        *right.last_mut().expect("kept") = format!("{kept} kept");
        text = right.join(" · ");
        while width(&text) > room && right.len() > 1 {
            right.pop();
            text = right.join(" · ");
        }
    }
    (title, text)
}

/// `text` wrapped at spaces into lines at most `w` wide (a word longer than that is
/// cut by the caller's `fit`).
fn wrap(text: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && width(&line) + 1 + width(word) > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    out.push(line);
    out
}

fn draw_tail(frame: &mut Frame, inner: Rect, t: &Tail) {
    let w = usize::from(inner.width);
    let h = usize::from(inner.height);
    if h == 0 {
        return;
    }
    // Column widths: TIME, then CHANNEL, then the message takes the rest.
    let cw = (w.saturating_sub(TIME_W + 3) / 4).clamp(8, 24);
    let mw = w.saturating_sub(1 + TIME_W + 1 + cw + 1);
    let mut lines = vec![Line::from(fit(
        vec![Span::styled(
            format!(
                " {} {} MESSAGE",
                pad_r("TIME (UTC)", TIME_W),
                pad_r("CHANNEL", cw)
            ),
            style::hd(),
        )],
        w,
    ))];

    // What the tail says about itself, above the footnote.
    let mut notes: Vec<Line<'static>> = Vec::new();
    let shown_any = t.shown().next().is_some();
    match &t.status {
        TailStatus::Subscribing => notes.push(dim_line(
            " Subscribing on a connection of its own…".into(),
            w,
        )),
        TailStatus::Closed(why) => {
            let text = format!("▲ The tail stopped: {why}.");
            for (i, l) in wrap(&text, w.saturating_sub(3)).into_iter().enumerate() {
                let indent = if i == 0 { " " } else { "   " };
                notes.push(Line::from(fit(
                    vec![Span::styled(format!("{indent}{l}"), style::amber())],
                    w,
                )));
            }
        }
        TailStatus::Live if t.listed() == 0 => {
            notes.push(dim_line(" Waiting for messages…".into(), w));
        }
        TailStatus::Stopped if t.listed() == 0 => notes.push(dim_line(
            " The tail is stopped. enter or s subscribes again.".into(),
            w,
        )),
        _ => {}
    }
    if !t.filter.is_empty() && !shown_any && t.listed() > 0 {
        notes.push(dim_line(" No message matches the filter.".into(), w));
    }

    let mut foot = String::from(" ");
    if !t.filter.is_empty() {
        let n = t.shown().count();
        foot.push_str(&format!(
            "/ {} · {} of {} · ",
            value::escape(&t.filter),
            format::grouped(n as u64),
            format::grouped(t.listed() as u64)
        ));
    }
    if t.dropped > 0 {
        foot.push_str(&format!(
            "{} skipped, too many at once · ",
            format::grouped(t.dropped)
        ));
    }
    foot.push_str("Messages are shown, never saved.");

    let room = h.saturating_sub(1 + notes.len() + 1);
    let mut last: Vec<&TailRow> = t.shown().rev().take(room).collect();
    last.reverse();
    for r in last {
        let ch = value::escape(&r.msg.channel);
        lines.push(Line::from(fit(
            vec![
                Span::styled(format!(" {} ", clock(r.msg.at_unix_ms)), style::dim()),
                Span::styled(pad_r(&ch, cw), style::fg()),
                Span::raw(" "),
                Span::styled(r.preview().to_string(), style::fgb()),
            ],
            1 + TIME_W + 1 + cw + 1 + mw,
        )));
    }
    lines.extend(notes);
    // The footnote sits on the last row.
    while lines.len() + 1 < h {
        lines.push(Line::default());
    }
    lines.push(dim_line(foot, w));
    rows(frame, inner, lines, None);
}

fn dim_line(text: String, w: usize) -> Line<'static> {
    Line::from(fit(vec![Span::styled(text, style::dim())], w))
}

fn totals(frame: &mut Frame, inner: Rect, state: &PubSubState, ctx: &Ctx<'_>) {
    let c = &state.counts;
    let w = usize::from(inner.width);
    let label = |s: &str| Span::styled(pad_r(&format!(" {s}"), 16), style::dim());
    let dash = || "—".to_string();
    // The commands behind a figure are for `v`.
    let hint = |what: &str| {
        if ctx.more {
            format!("  ({what})")
        } else {
            String::new()
        }
    };
    let listed: Option<u64> = c
        .channels
        .iter()
        .map(|ch| ch.subscribers)
        .sum::<Option<u64>>();
    let cut = if c.total_channels > c.channels.len() {
        format!(
            " (the first {} of {})",
            format::grouped(MAX_CHANNELS as u64),
            format::grouped(c.total_channels as u64)
        )
    } else {
        String::new()
    };
    let mut lines = vec![
        Line::from(vec![
            label("Channels"),
            Span::styled(format::grouped(c.total_channels as u64), style::fgb()),
            Span::styled(cut, style::dim()),
        ]),
        Line::from(vec![
            label("Patterns"),
            Span::styled(c.patterns.map_or_else(dash, format::grouped), style::fgb()),
            Span::styled(hint("PSUBSCRIBE, PUBSUB NUMPAT"), style::dim()),
        ]),
        Line::from(vec![
            label("Subscribers"),
            Span::styled(listed.map_or_else(dash, format::grouped), style::fgb()),
            Span::styled(hint("direct SUBSCRIBE only"), style::dim()),
        ]),
    ];
    // The classic counts are the picked node's own subscribers.
    if let Some(n) = ctx
        .session
        .picked_node()
        .filter(|_| ctx.node_title().is_some())
    {
        lines.push(Line::from(fit(
            vec![
                label("Counted on"),
                Span::styled(n.endpoint.to_string(), style::fg()),
            ],
            w,
        )));
    }
    if state.shards_loaded && state.shards_off.is_none() {
        lines.push(Line::from(vec![
            label("Shard channels"),
            Span::styled(format::grouped(state.shards_total as u64), style::fgb()),
            Span::styled(hint("SSUBSCRIBE"), style::dim()),
        ]));
    }
    let selected = match state.selected() {
        Some(Selected::Channel(ch)) => Some((ch.name.as_str(), ch.subscribers, None)),
        Some(Selected::Shard(r)) => Some((r.name.as_str(), r.subscribers, Some(r.node))),
        None => None,
    };
    if let Some((name, subs, node)) = selected {
        lines.push(Line::default());
        let which = if node.is_some() {
            "Shard channel"
        } else {
            "Selected"
        };
        lines.push(Line::from(fit(
            vec![label(which), Span::styled(value::escape(name), style::fg())],
            w,
        )));
        let mut detail = subs.map_or_else(String::new, |n| {
            format!("{} subscribers", format::grouped(n))
        });
        if let Some(n) = node.and_then(|i| ctx.session.nodes.get(i)) {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str(&format!("on {}", n.endpoint));
        }
        if !detail.is_empty() {
            lines.push(Line::from(fit(
                vec![label(""), Span::styled(detail, style::dim())],
                w,
            )));
        }
    }
    for note in c.notes.iter().chain(&state.shards_notes) {
        lines.push(Line::from(fit(
            vec![Span::styled(format!(" ▲ left out: {note}"), style::amber())],
            w,
        )));
    }
    if let Some(e) = &state.error {
        lines.push(Line::from(fit(
            vec![Span::styled(
                format!(" ▲ {e}; showing the previous reading."),
                style::amber(),
            )],
            w,
        )));
    }
    lines.push(Line::default());
    lines.extend(paragraph(
        vec![Span::raw(" ")],
        " ",
        "Counts read no payloads. enter or s starts a live tail, S a shard one.",
        style::dim(),
        w,
    ));
    rows(frame, inner, lines, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_utc_with_milliseconds() {
        assert_eq!(clock(0), "00:00:00.000");
        // 2024-05-01 13:04:05.678 UTC
        assert_eq!(clock(1_714_568_645_678), "13:04:05.678");
        assert_eq!(clock(-1), "23:59:59.999");
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        assert_eq!(wrap("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap("abcdef", 3), ["abcdef"]);
        assert_eq!(wrap("", 3), [""]);
    }

    #[test]
    fn rates_have_one_decimal_below_ten() {
        assert_eq!(rate(0.0), "0.0");
        assert_eq!(rate(3.24), "3.2");
        assert_eq!(rate(1234.4), "1,234");
    }
}
