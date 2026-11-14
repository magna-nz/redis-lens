//! 2 Analysis · Slots: the slot ranges of each node with their figures from
//! `CLUSTER SLOT-STATS`, the hottest slots, and the scan's keys that hash to the selected
//! slot. State in `views/slots.rs`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use crate::format;
use crate::model::ServerFlavor;
use crate::tui::app::views::Ctx;
use crate::tui::app::views::slots::{
    NEEDS_ENABLED, NOT_CLUSTER, Outcome, RangeRow, SlotsState, Totals, keys_in_slot,
};

use super::widgets::{pad_l, pad_r, panel, rows, scroll_offset, trunc};
use super::{notice_panels, style};

const RANGE_W: usize = 12;
const NODE_W: usize = 19;
const NUM_W: usize = 7;
/// Range rows shown before the table is cut.
const MAX_RANGE_ROWS: usize = 5;

fn dash(v: Option<u64>, f: impl Fn(u64) -> String) -> String {
    v.map_or_else(|| "—".to_string(), f)
}

fn num_cell(s: &str) -> Span<'static> {
    Span::styled(format!("{}  ", pad_l(s, NUM_W)), style::fg())
}

fn head_cell(s: &str) -> Span<'static> {
    Span::styled(format!("{}  ", pad_l(s, NUM_W)), style::hd())
}

fn range_label(start: u16, end: u16) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn cell(s: &str, w: usize) -> String {
    pad_r(&trunc(s, w - 1), w)
}

fn waiting(frame: &mut Frame, area: Rect, text: &str) {
    notice_panels(
        frame,
        area,
        "Slots",
        "",
        vec![Line::from(Span::styled(format!(" {text}"), style::dim()))],
    );
}

pub fn draw(frame: &mut Frame, area: Rect, st: &SlotsState, ctx: &Ctx<'_>) {
    let off = |frame: &mut Frame, why: String| {
        let w = usize::from(area.width).saturating_sub(6).max(10);
        let lines = wrap(&why, w)
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let mark = if i == 0 { "▲ " } else { "  " };
                Line::from(Span::styled(format!(" {mark}{l}"), style::amber()))
            })
            .collect();
        notice_panels(frame, area, "Slots", "off", lines);
    };
    if !ctx.is_cluster() {
        return off(frame, NOT_CLUSTER.to_string());
    }
    if let Some(why) = &st.off {
        return off(frame, why.text());
    }
    let nodes = &ctx.session.nodes;
    if nodes.is_empty() {
        return waiting(frame, area, "The cluster's nodes are not known yet.");
    }
    if !st.started() {
        return waiting(frame, area, "Waiting for the connection…");
    }
    let ranges = st.range_rows(nodes);
    let memory = st.flavor() == Some(ServerFlavor::Redis);
    let range_h = u16::try_from(ranges.len().min(MAX_RANGE_ROWS) + 3).unwrap_or(8);
    let [top, mid, bottom] = Layout::vertical([
        Constraint::Length(range_h),
        Constraint::Min(3),
        Constraint::Length(6),
    ])
    .areas(area);

    // The ranges.
    let right = if st.cpu_and_network_zero() {
        NEEDS_ENABLED.to_string()
    } else {
        let n = ranges.len();
        let more = if st.is_loading() {
            " · reading…"
        } else {
            ""
        };
        format!("{n} range{}{more}", if n == 1 { "" } else { "s" })
    };
    let inner = panel(frame, top, "Slot ranges", &right);
    let mut head = vec![
        Span::styled(format!(" {}", pad_r("RANGE", RANGE_W)), style::hd()),
        Span::styled(pad_r("NODE", NODE_W), style::hd()),
        head_cell("KEYS"),
        head_cell("CPU µs"),
        head_cell("NET IN"),
        head_cell("NET OUT"),
    ];
    if memory {
        head.push(head_cell("MEMORY"));
    }
    let mut lines = vec![Line::from(head)];
    let fixed = 1 + RANGE_W + NODE_W;
    let rest = usize::from(inner.width).saturating_sub(fixed);
    for r in ranges.iter().take(MAX_RANGE_ROWS) {
        lines.push(range_line(
            r,
            nodes[r.node].endpoint.to_string(),
            memory,
            rest,
        ));
    }
    rows(frame, inner, lines, None);

    // The hottest slots.
    let right = format!("by {} · o changes the order", st.order.label());
    let inner = panel(frame, mid, "Hottest slots", &right);
    let head = Line::from(vec![
        Span::styled(" SLOT   ", style::hd()),
        Span::styled(pad_r("NODE", NODE_W), style::hd()),
        head_cell("KEYS"),
        head_cell("CPU µs"),
        head_cell("NET IN"),
        head_cell("NET OUT"),
    ]);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    rows(frame, inner, vec![head], None);
    if st.hottest.is_empty() {
        let text = if st.is_loading() {
            " Reading…".to_string()
        } else {
            format!(" No slot has a figure above 0 for {}.", st.order.label())
        };
        rows(
            frame,
            body,
            vec![Line::from(Span::styled(text, style::dim()))],
            None,
        );
    } else {
        let sel = st.sel.min(st.hottest.len() - 1);
        let off = scroll_offset(sel, usize::from(body.height));
        let lines: Vec<Line<'static>> = st
            .hottest
            .iter()
            .skip(off)
            .take(usize::from(body.height))
            .map(|h| {
                let s = &h.stat;
                Line::from(vec![
                    Span::styled(format!(" {}", pad_r(&s.slot.to_string(), 7)), style::fgb()),
                    Span::styled(
                        cell(&nodes[h.node].endpoint.to_string(), NODE_W),
                        style::fg(),
                    ),
                    num_cell(&dash(s.keys, format::count)),
                    num_cell(&dash(s.cpu_usec, format::count)),
                    num_cell(&dash(s.net_in, format::bytes)),
                    num_cell(&dash(s.net_out, format::bytes)),
                ])
            })
            .collect();
        rows(frame, body, lines, Some(sel - off));
    }

    // The selected slot's keys.
    let Some(h) = st.selected() else {
        panel(frame, bottom, "Details", "");
        return;
    };
    let inner = panel(
        frame,
        bottom,
        &format!("Slot {}", h.stat.slot),
        &nodes[h.node].endpoint.to_string(),
    );
    let keys = ctx
        .report()
        .map(|r| keys_in_slot(r, h.stat.slot))
        .unwrap_or_default();
    let w = usize::from(inner.width);
    let mut lines = Vec::new();
    if keys.is_empty() {
        lines.push(Line::from(Span::styled(
            " No example, big or hot key of the scan hashes to this slot.",
            style::dim(),
        )));
    }
    for k in keys.iter().take(usize::from(inner.height)) {
        let tail = format!("  {}  ({})", k.pattern, k.kinds);
        let key_w = w.saturating_sub(tail.chars().count() + 1).max(8);
        lines.push(Line::from(vec![
            Span::styled(format!(" {}", trunc(&k.key, key_w)), style::fg()),
            Span::styled(trunc(&tail, w.saturating_sub(key_w + 1)), style::dim()),
        ]));
    }
    rows(frame, inner, lines, None);
}

fn range_line(r: &RangeRow, node: String, memory: bool, rest: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!(" {}", cell(&range_label(r.start, r.end), RANGE_W)),
            style::fg(),
        ),
        Span::styled(cell(&node, NODE_W), style::fgb()),
    ];
    match &r.outcome {
        Outcome::Waiting => spans.push(Span::styled("…", style::dim())),
        Outcome::Failed(why) => spans.push(Span::styled(
            trunc(&format!("✗ {why}"), rest.max(2)),
            style::warn(),
        )),
        Outcome::Done(t) => {
            let Totals {
                keys,
                cpu_usec,
                net_in,
                net_out,
                memory: mem,
            } = *t;
            spans.push(num_cell(&dash(keys, format::count)));
            spans.push(num_cell(&dash(cpu_usec, format::count)));
            spans.push(num_cell(&dash(net_in, format::bytes)));
            spans.push(num_cell(&dash(net_out, format::bytes)));
            if memory {
                spans.push(num_cell(&dash(mem, format::bytes)));
            }
        }
    }
    Line::from(spans)
}

/// `text` cut into lines of at most `w` cells at spaces.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > w {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
