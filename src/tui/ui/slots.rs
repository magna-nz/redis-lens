//! Analyze › Slots: the slot ranges of each node with their figures from
//! `CLUSTER SLOT-STATS`, the hottest slots, and the analysis's keys that hash to the
//! selected slot, in one frame of three sections. State in `views/slots.rs`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::format;
use crate::model::ServerFlavor;
use crate::tui::app::views::Ctx;
use crate::tui::app::views::slots::{
    NEEDS_ENABLED, NOT_CLUSTER, Outcome, RangeRow, SlotsState, Totals, keys_in_slot,
};

use super::analysis::{Col, MARK, Tier, head_row, notice, paragraph, pick_cols};
use super::style;
use super::widgets::{Section, Title, pad_l, pad_r, rows, scroll_offset, sections, trunc};

const NODE_W: usize = 19;
const NUM_W: usize = 7;
/// The first column (range or slot) never takes more than this many cells.
const FIRST_CAP: usize = 14;
/// Range rows shown before the table is cut.
const MAX_RANGE_ROWS: usize = 5;
/// The figures after the keys show from this many cells wide, and with `v`.
const WIDE_AT: usize = 90;

/// The columns of both tables: `first` names the first one (`RANGE` or `SLOT`).
fn columns(first: &'static str, memory: bool) -> Vec<Col> {
    let mut cols = vec![
        Col::new(first, 7, Tier::Base),
        Col::new("NODE", NODE_W, Tier::Base),
        Col::new("KEYS", NUM_W, Tier::Base),
        Col::new("CPU µs", NUM_W, Tier::Wide),
        Col::new("NET IN", NUM_W, Tier::Wide),
        Col::new("NET OUT", NUM_W, Tier::Wide),
    ];
    if memory {
        cols.push(Col::new("MEMORY", NUM_W, Tier::Wide));
    }
    cols
}

fn dash(v: Option<u64>, f: impl Fn(u64) -> String) -> String {
    v.map_or_else(|| "—".to_string(), f)
}

fn range_label(start: u16, end: u16) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn cell(s: &str, w: usize) -> String {
    pad_r(&trunc(s, w.saturating_sub(1)), w)
}

fn waiting(frame: &mut Frame, area: Rect, text: &str) {
    notice(
        frame,
        area,
        "Slots",
        "",
        vec![Line::from(Span::styled(format!(" {text}"), style::dim()))],
    );
}

pub fn draw(frame: &mut Frame, area: Rect, st: &SlotsState, ctx: &Ctx<'_>) {
    let off = |frame: &mut Frame, why: String| {
        let w = usize::from(area.width.saturating_sub(2));
        let lines = paragraph(
            vec![Span::styled(format!(" {MARK} "), style::amber())],
            "   ",
            &why,
            style::amber(),
            w,
        );
        notice(frame, area, "Slots", "off", lines);
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
    // The sections: ranges (header and rows), the hottest slots, the selected slot.
    let range_h = u16::try_from(ranges.len().min(MAX_RANGE_ROWS) + 1).unwrap_or(6);
    let detail_h = 4u16;

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
    let hot_right = format!("by {} · o changes the order", st.order.label());
    let selected = st.selected();
    let detail_title = selected.map_or_else(
        || "Details".to_string(),
        |h| format!("Slot {}", h.stat.slot),
    );
    let detail_right = selected.map_or_else(String::new, |h| nodes[h.node].endpoint.to_string());
    let inner = sections(
        frame,
        area,
        &[
            Section {
                title: Title::new("Slot ranges", &right),
                rows: Some(range_h),
            },
            Section {
                title: Title::new("Hottest slots", &hot_right),
                rows: None,
            },
            Section {
                title: Title::new(&detail_title, &detail_right),
                rows: Some(detail_h),
            },
        ],
    );

    // The ranges.
    let w = usize::from(inner[0].width);
    let cols = columns("RANGE", memory);
    let (keep, first_w) = pick_cols(&cols, w, WIDE_AT, ctx.more);
    let first_w = first_w.min(FIRST_CAP);
    let numeric: Vec<usize> = (2..cols.len()).collect();
    let mut lines = vec![head_row(&cols, &keep, first_w, &numeric)];
    for r in ranges.iter().take(MAX_RANGE_ROWS) {
        lines.push(range_line(
            r,
            nodes[r.node].endpoint.to_string(),
            &cols,
            &keep,
            first_w,
            w,
        ));
    }
    rows(frame, inner[0], lines, None);

    // The hottest slots.
    let cols = columns("SLOT", false);
    let (keep, first_w) = pick_cols(&cols, w, WIDE_AT, ctx.more);
    let first_w = first_w.min(FIRST_CAP);
    let numeric: Vec<usize> = (2..cols.len()).collect();
    let body = Rect {
        y: inner[1].y + 1,
        height: inner[1].height.saturating_sub(1),
        ..inner[1]
    };
    rows(
        frame,
        inner[1],
        vec![head_row(&cols, &keep, first_w, &numeric)],
        None,
    );
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
                let mut spans = vec![Span::styled(
                    pad_r(&format!(" {}", s.slot), first_w),
                    style::fgb(),
                )];
                for &i in &keep[1..] {
                    spans.push(Span::raw(" "));
                    let cw = cols[i].w;
                    spans.push(match cols[i].head {
                        "NODE" => {
                            Span::styled(cell(&nodes[h.node].endpoint.to_string(), cw), style::fg())
                        }
                        "KEYS" => {
                            Span::styled(pad_l(&dash(s.keys, format::count), cw), style::fg())
                        }
                        "CPU µs" => {
                            Span::styled(pad_l(&dash(s.cpu_usec, format::count), cw), style::fg())
                        }
                        "NET IN" => {
                            Span::styled(pad_l(&dash(s.net_in, format::bytes), cw), style::fg())
                        }
                        _ => Span::styled(pad_l(&dash(s.net_out, format::bytes), cw), style::fg()),
                    });
                }
                Line::from(spans)
            })
            .collect();
        rows(frame, body, lines, Some(sel - off));
    }

    // The selected slot's keys.
    let Some(h) = selected else {
        return;
    };
    let keys = ctx
        .report()
        .map(|r| keys_in_slot(r, h.stat.slot))
        .unwrap_or_default();
    let w = usize::from(inner[2].width);
    let mut lines = Vec::new();
    if keys.is_empty() {
        lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            "No example, big or hot key of the analysis hashes to this slot.",
            style::dim(),
            w,
        ));
    }
    for k in keys.iter().take(usize::from(inner[2].height)) {
        let tail = format!("  {}  ({})", k.pattern, k.kinds);
        let key_w = w.saturating_sub(tail.chars().count() + 1).max(8);
        lines.push(Line::from(vec![
            Span::styled(format!(" {}", trunc(&k.key, key_w)), style::fg()),
            Span::styled(trunc(&tail, w.saturating_sub(key_w + 1)), style::dim()),
        ]));
    }
    rows(frame, inner[2], lines, None);
}

fn range_line(
    r: &RangeRow,
    node: String,
    cols: &[Col],
    keep: &[usize],
    first_w: usize,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        pad_r(
            &format!(
                " {}",
                trunc(&range_label(r.start, r.end), first_w.saturating_sub(2))
            ),
            first_w,
        ),
        style::fg(),
    )];
    let totals = match &r.outcome {
        Outcome::Done(t) => Some(*t),
        _ => None,
    };
    for &i in &keep[1..] {
        spans.push(Span::raw(" "));
        let cw = cols[i].w;
        if i == 1 {
            spans.push(Span::styled(cell(&node, cw), style::fgb()));
            // Not done: the rest of the row says why.
            match &r.outcome {
                Outcome::Waiting => {
                    spans.push(Span::styled("…", style::dim()));
                    break;
                }
                Outcome::Failed(why) => {
                    let used = first_w + 1 + cw + 1;
                    spans.push(Span::styled(
                        trunc(&format!("✗ {why}"), width.saturating_sub(used).max(2)),
                        style::warn(),
                    ));
                    break;
                }
                Outcome::Done(_) => {}
            }
            continue;
        }
        let Some(Totals {
            keys,
            cpu_usec,
            net_in,
            net_out,
            memory,
        }) = totals
        else {
            continue;
        };
        let text = match cols[i].head {
            "KEYS" => dash(keys, format::count),
            "CPU µs" => dash(cpu_usec, format::count),
            "NET IN" => dash(net_in, format::bytes),
            "NET OUT" => dash(net_out, format::bytes),
            _ => dash(memory, format::bytes),
        };
        spans.push(Span::styled(pad_l(&text, cw), style::fg()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_figures_after_the_keys_come_with_width_or_v() {
        let cols = columns("RANGE", true);
        let (keep, _) = pick_cols(&cols, 78, WIDE_AT, false);
        assert_eq!(keep, vec![0, 1, 2]);
        let (keep, _) = pick_cols(&cols, 110, WIDE_AT, false);
        assert_eq!(keep, vec![0, 1, 2, 3, 4, 5, 6]);
        let (keep, _) = pick_cols(&cols, 78, WIDE_AT, true);
        assert!(keep.len() > 3);
        // No memory column off Redis.
        assert_eq!(columns("SLOT", false).len(), 6);
    }
}
