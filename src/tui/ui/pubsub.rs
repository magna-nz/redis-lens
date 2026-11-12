//! Draws [`PubSubState`]. Owner: wave B, with `views/pubsub.rs`.
//!
//! The active channels with how many clients subscribe to each, and the totals below.
//! Counts only: no payload is ever read.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::format;
use crate::live::pubsub::MAX_CHANNELS;
use crate::tui::app::views::Ctx;
use crate::tui::app::views::pubsub::PubSubState;

use super::streams::live_wait;
use super::widgets::{bar, fit, pad_l, pad_r, panel, rows, scroll_offset};
use super::{notice_panels, split_body, style};

const SUBS_W: usize = 6;
const BAR_W: usize = 16;

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &PubSubState, ctx: &Ctx<'_>) {
    const TITLE: &str = "Pub/Sub";
    if let Some(off) = &state.off {
        let lines = vec![Line::from(Span::styled(
            format!(" ▲ {off}"),
            style::amber(),
        ))];
        notice_panels(frame, area, TITLE, "off", lines);
        return;
    }
    if !state.loaded {
        let text = live_wait(ctx).unwrap_or_else(|| "Reading the channels with PUBSUB…".into());
        let lines = vec![Line::from(Span::styled(format!(" {text}"), style::dim()))];
        notice_panels(frame, area, TITLE, "", lines);
        return;
    }
    let c = &state.counts;
    let (main, detail) = split_body(area);
    let mut right = String::new();
    if !ctx.live_ready() {
        right.push_str("not live · ");
    }
    if state.paused {
        right.push_str("paused · ");
    }
    right.push_str("active channels, most subscribers first");
    let inner = panel(frame, main, TITLE, &right);
    let w = usize::from(inner.width);

    if c.channels.is_empty() {
        let lines = vec![
            Line::from(Span::styled(" No active channels.", style::fgb())),
            Line::from(Span::styled(
                " A channel exists only while a client subscribes to it.",
                style::dim(),
            )),
        ];
        rows(frame, inner, lines, None);
        totals(frame, detail, state);
        return;
    }

    let has_counts = c.channels.iter().any(|ch| ch.subscribers.is_some());
    let kw = w.saturating_sub(1 + SUBS_W + 1 + BAR_W + 1).max(8);
    let mut head = vec![Span::styled(pad_r(" CHANNEL", kw), style::hd())];
    if has_counts {
        head.push(Span::styled(
            format!(" {}", pad_l("SUBS", SUBS_W)),
            style::hd(),
        ));
    }
    rows(frame, inner, vec![Line::from(head)], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let sel = state.sel.min(c.channels.len() - 1);
    let off = scroll_offset(sel, usize::from(body.height));
    let max = c
        .channels
        .iter()
        .filter_map(|ch| ch.subscribers)
        .max()
        .unwrap_or(0);
    let lines: Vec<Line<'static>> = c
        .channels
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .map(|ch| {
            let mut spans = fit(vec![Span::styled(format!(" {}", ch.name), style::fg())], kw);
            if let Some(n) = ch.subscribers {
                let ratio = if max == 0 { 0.0 } else { n as f64 / max as f64 };
                let (fill, track) = bar(ratio, BAR_W);
                spans.push(Span::styled(
                    format!(" {}", pad_l(&format::grouped(n), SUBS_W)),
                    style::fgb(),
                ));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(fill, style::ok()));
                spans.push(Span::styled(track, style::track()));
            }
            Line::from(spans)
        })
        .collect();
    rows(frame, body, lines, Some(sel - off));
    totals(frame, detail, state);
}

fn totals(frame: &mut Frame, area: Rect, state: &PubSubState) {
    let c = &state.counts;
    let inner = panel(frame, area, "Totals", "");
    let w = usize::from(inner.width);
    let label = |s: &str| Span::styled(pad_r(&format!(" {s}"), 33), style::dim());
    let dash = || "—".to_string();
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
            label("Active channels"),
            Span::styled(format::grouped(c.total_channels as u64), style::fgb()),
            Span::styled(cut, style::dim()),
        ]),
        Line::from(vec![
            label("Pattern subscriptions"),
            Span::styled(c.patterns.map_or_else(dash, format::grouped), style::fgb()),
            Span::styled("  (PSUBSCRIBE, PUBSUB NUMPAT)", style::dim()),
        ]),
        Line::from(vec![
            label("Subscribers on listed channels"),
            Span::styled(listed.map_or_else(dash, format::grouped), style::fgb()),
            Span::styled("  (direct SUBSCRIBE only)", style::dim()),
        ]),
    ];
    if let Some(ch) = c.channels.get(state.sel) {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            label("Selected"),
            Span::styled(ch.name.clone(), style::fg()),
            Span::styled(
                ch.subscribers.map_or_else(String::new, |n| {
                    format!("  ·  {} subscribers", format::grouped(n))
                }),
                style::dim(),
            ),
        ]));
    }
    for note in &c.notes {
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
    lines.push(Line::from(Span::styled(
        " Counts only: message payloads are never read.",
        style::dim(),
    )));
    rows(frame, inner, lines, None);
}
