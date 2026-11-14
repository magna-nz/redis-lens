//! Analyze › Nodes: the shards of a cluster analysis, one row per node read, with the
//! selected node's details below in the same frame. `enter` narrows Browse to that node.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::format;
use crate::model::{NodeReport, NodeRole, NodeStatus};
use crate::tui::app::App;

use super::analysis::{Col, MARK, Tier, head_row, need_report, notice, paragraph, pick_cols};
use super::widgets::{Section, Title, fit, pad_l, pad_r, rows, scroll_offset, sections, trunc};
use super::{mark_previous, server_label, style};

const SLOTS_W: usize = 12;
const ENDPOINT_W: usize = 16;
const ROLE_W: usize = 8;
const STATE_W: usize = 11;
const NUM_W: usize = 7;
/// The slot ranges of a node never take more than this many cells.
const SLOTS_CAP: usize = 18;
/// Rows the details get at least, when the list is long.
const DETAIL_ROWS: u16 = 8;

const NODE: usize = 1;
const ROLE: usize = 2;
const STATE: usize = 3;
const KEYS: usize = 4;
const MEMORY: usize = 5;
const SAMPLED: usize = 6;

/// The columns. The node address gets more room on wide terminals, for host names.
fn columns(endpoint_w: usize) -> [Col; 7] {
    [
        Col::new("SLOTS", SLOTS_W, Tier::Base),
        Col::new("NODE", endpoint_w, Tier::Base),
        Col::new("ROLE", ROLE_W, Tier::Base),
        Col::new("STATE", STATE_W, Tier::Base),
        Col::new("KEYS", NUM_W, Tier::Base),
        Col::new("MEMORY", NUM_W, Tier::Base),
        Col::new("SAMPLED", NUM_W, Tier::More),
    ]
}

fn endpoint_width(inner: usize) -> usize {
    if inner >= 100 { 24 } else { ENDPOINT_W }
}

/// Slot ranges as `0-5460` or `0-99,200-5460`; a single slot as `42`.
pub fn slot_ranges(slots: &[(u16, u16)]) -> String {
    if slots.is_empty() {
        return "—".to_string();
    }
    slots
        .iter()
        .map(|&(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The number of slots in `slots` (inclusive ranges).
fn slot_count(slots: &[(u16, u16)]) -> u32 {
    slots
        .iter()
        .map(|&(a, b)| u32::from(b.saturating_sub(a)) + 1)
        .sum()
}

fn role(r: NodeRole) -> &'static str {
    match r {
        NodeRole::Primary => "primary",
        NodeRole::Replica => "replica",
    }
}

/// The state in words with its glyph, and its style: `✓ analyzed` teal, `▲ partial`
/// amber, `✗ failed` red.
pub fn status(s: NodeStatus) -> (&'static str, Style) {
    match s {
        NodeStatus::Scanned => ("✓ analyzed", style::ok()),
        NodeStatus::Partial => ("▲ partial", style::amber()),
        NodeStatus::Failed => ("✗ failed", style::warn()),
    }
}

/// The frame's right title: shard count, then how many were read on a replica, failed
/// or are partial.
fn summary(nodes: &[NodeReport]) -> String {
    let n = nodes.len();
    let count = |s: NodeStatus| nodes.iter().filter(|x| x.status == s).count();
    let mut out = format!("{n} shard{}", if n == 1 { "" } else { "s" });
    let replicas = nodes.iter().filter(|x| x.role == NodeRole::Replica).count();
    if replicas > 0 {
        out.push_str(&format!(
            " · {replicas} read on {}",
            if replicas == 1 {
                "a replica"
            } else {
                "replicas"
            }
        ));
    }
    for (s, label) in [
        (NodeStatus::Failed, "failed"),
        (NodeStatus::Partial, "partial"),
    ] {
        let c = count(s);
        if c > 0 {
            out.push_str(&format!(" · {c} {label}"));
        }
    }
    out
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if need_report(
        frame,
        area,
        app,
        "Nodes",
        "shows how the analysis went on each shard of the cluster.",
    ) {
        return;
    }
    let Some(report) = app.report() else {
        return;
    };
    let Some(nodes) = report.nodes.as_deref() else {
        notice(
            frame,
            area,
            "Nodes",
            &mark_previous(app, "standalone"),
            vec![
                Line::from(Span::styled(
                    " Not a cluster: this analysis read one standalone server.",
                    style::fgb(),
                )),
                Line::from(Span::styled(
                    " On a cluster this panel lists each shard and the node read for it.",
                    style::dim(),
                )),
            ],
        );
        return;
    };
    if nodes.is_empty() {
        notice(
            frame,
            area,
            "Nodes",
            &mark_previous(app, "0 shards"),
            vec![Line::from(Span::styled(
                " The cluster analysis has no shards to show.",
                style::dim(),
            ))],
        );
        return;
    }
    let sel = app.sel.nodes.min(nodes.len() - 1);
    let n = &nodes[sel];
    let (st, _) = status(n.status);
    let node_title = n.endpoint.to_string();
    let node_right = format!(
        "{} · {}",
        role(n.role),
        st.trim_start_matches(['✓', '▲', '✗', ' '])
    );
    // The list takes what it needs, leaving the details their rows.
    let usable = area.height.saturating_sub(3);
    let list_rows = u16::try_from(nodes.len() + 1)
        .unwrap_or(u16::MAX)
        .min(usable.saturating_sub(DETAIL_ROWS).max(3));
    let inner = sections(
        frame,
        area,
        &[
            Section {
                title: Title::new("Nodes", &mark_previous(app, &summary(nodes))),
                rows: Some(list_rows),
            },
            Section {
                title: Title::new(&node_title, &node_right),
                rows: None,
            },
        ],
    );
    let (list, detail) = (inner[0], inner[1]);
    let iw = usize::from(list.width);
    let cols = columns(endpoint_width(iw));
    let (keep, slots_w) = pick_cols(&cols, iw, usize::MAX, app.more_here());
    let slots_w = slots_w.min(SLOTS_CAP);
    rows(
        frame,
        list,
        vec![head_row(&cols, &keep, slots_w, &[KEYS, MEMORY, SAMPLED])],
        None,
    );
    let body = Rect {
        y: list.y + 1,
        height: list.height.saturating_sub(1),
        ..list
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = nodes
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .map(|n| node_row(n, &cols, &keep, slots_w))
        .collect();
    rows(frame, body, lines, Some(sel - off));
    detail_lines(frame, detail, n, report.sampled_keys);
}

fn node_row(n: &NodeReport, cols: &[Col], keep: &[usize], slots_w: usize) -> Line<'static> {
    let dash = || "—".to_string();
    let (st, st_style) = status(n.status);
    // A cut cell keeps one free cell before the next column.
    let cell = |s: &str, w: usize| pad_r(&trunc(s, w.saturating_sub(1)), w);
    let mut spans = vec![Span::styled(
        format!(
            " {}",
            cell(&slot_ranges(&n.slots), slots_w.saturating_sub(1))
        ),
        style::fg(),
    )];
    for &i in &keep[1..] {
        spans.push(Span::raw(" "));
        let w = cols[i].w;
        match i {
            NODE => spans.push(Span::styled(cell(&n.endpoint.to_string(), w), style::fgb())),
            ROLE => spans.push(Span::styled(pad_r(role(n.role), w), style::dim())),
            STATE => spans.push(Span::styled(pad_r(st, w), st_style)),
            KEYS => {
                let keys = n
                    .server
                    .as_ref()
                    .map_or_else(dash, |s| format::count(s.total_keys));
                spans.push(Span::styled(pad_l(&keys, w), style::fg()));
            }
            MEMORY => {
                let mem = n
                    .server
                    .as_ref()
                    .map_or_else(dash, |s| format::bytes(s.used_memory));
                spans.push(Span::styled(pad_l(&mem, w), style::fg()));
            }
            SAMPLED => {
                let sampled = if n.status == NodeStatus::Failed {
                    dash()
                } else {
                    format::count(n.sampled_keys)
                };
                spans.push(Span::styled(pad_l(&sampled, w), style::fg()));
            }
            _ => {}
        }
    }
    Line::from(spans)
}

/// A failed node's note is red, a partial one's or a replica fallback's amber.
fn note_style(n: &NodeReport) -> Style {
    match n.status {
        NodeStatus::Failed => style::warn(),
        _ => style::amber(),
    }
}

fn kv(label: &str, value: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(format!(" {}", pad_r(label, 8)), style::dim())];
    spans.extend(value);
    spans
}

fn detail_lines(frame: &mut Frame, inner: Rect, n: &NodeReport, analyzed: u64) {
    let w = usize::from(inner.width);
    let mut lines: Vec<Vec<Span<'static>>> =
        vec![kv("Node", vec![Span::styled(n.id.clone(), style::fg())])];
    if n.role == NodeRole::Replica {
        lines.push(kv(
            "Primary",
            vec![Span::styled(n.primary_id.clone(), style::fg())],
        ));
    }
    let count = slot_count(&n.slots);
    lines.push(kv(
        "Slots",
        vec![
            Span::styled(slot_ranges(&n.slots), style::fg()),
            Span::styled(
                format!(" ({} slot{})", format::grouped(u64::from(count)), {
                    if count == 1 { "" } else { "s" }
                }),
                style::dim(),
            ),
        ],
    ));
    lines.push(kv(
        "Server",
        match &n.server {
            Some(s) => {
                let mut v = vec![
                    Span::styled(server_label(s), style::fg()),
                    Span::styled(
                        format!(
                            " · {} keys · {}",
                            format::count(s.total_keys),
                            format::bytes(s.used_memory)
                        ),
                        style::dim(),
                    ),
                ];
                if s.maxmemory > 0 {
                    v.push(Span::styled(
                        format!(" of {}", format::bytes(s.maxmemory)),
                        style::dim(),
                    ));
                }
                v
            }
            None => vec![Span::styled("INFO not read", style::dim())],
        },
    ));
    if n.status != NodeStatus::Failed {
        let mut v = vec![
            Span::styled(format::grouped(n.sampled_keys), style::fg()),
            Span::styled(
                format!(
                    " of the {} analyzed keys, {} pattern{}",
                    format::grouped(analyzed),
                    n.patterns.len(),
                    if n.patterns.len() == 1 { "" } else { "s" }
                ),
                style::dim(),
            ),
        ];
        if n.complete {
            v.push(Span::styled(" · every key seen", style::dim()));
        }
        if !n.sizes_available {
            v.push(Span::styled(
                " · no sizes: MEMORY USAGE denied",
                style::warn(),
            ));
        }
        lines.push(kv("Sample", v));
    }
    let mut out: Vec<Line<'static>> = lines.into_iter().map(|l| Line::from(fit(l, w))).collect();
    out.push(Line::default());
    out.extend(paragraph(
        vec![Span::raw(" ")],
        " ",
        "enter shows only this node's patterns in Browse; esc there shows all again.",
        style::dim(),
        w,
    ));
    // The whole note, wrapped over the rows that are left.
    if let Some(note) = n.note.as_deref().map(plain_note) {
        out.push(Line::default());
        let (mark, st) = if n.status == NodeStatus::Failed {
            ("✗", note_style(n))
        } else {
            (MARK, note_style(n))
        };
        out.extend(paragraph(
            vec![Span::styled(format!(" {mark} "), st)],
            "   ",
            &note,
            st,
            w,
        ));
    }
    rows(frame, inner, out, None);
}

/// A node's note in the TUI's words: the engine's notes say "scan" (the CLI's report
/// prints them as they are); here an analysis is an analysis.
pub fn plain_note(note: &str) -> String {
    use crate::scan::cluster::{NOTE_INFO_FAILED, NOTE_STOPPED};
    note.replace(
        NOTE_STOPPED,
        "the analysis of this node stopped early (a connection or server error); its results are partial",
    )
    .replace(
        NOTE_INFO_FAILED,
        "INFO or DBSIZE could not be read on this node, so it was not analyzed",
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn engine_notes_say_analysis_on_screen() {
        use crate::scan::cluster::{NOTE_INFO_FAILED, NOTE_STOPPED};
        let joined = format!("10.0.1.12:6379: slow to answer; {NOTE_STOPPED}");
        let plain = super::plain_note(&joined);
        assert_eq!(
            plain,
            "10.0.1.12:6379: slow to answer; the analysis of this node stopped early (a connection or server error); its results are partial"
        );
        assert!(!super::plain_note(NOTE_INFO_FAILED).contains("scan"));
        assert!(super::plain_note(NOTE_INFO_FAILED).ends_with("not analyzed"));
        // Any other note is shown as it is.
        assert_eq!(super::plain_note("could not connect"), "could not connect");
    }

    use super::*;

    #[test]
    fn slot_ranges_are_compact() {
        assert_eq!(slot_ranges(&[]), "—");
        assert_eq!(slot_ranges(&[(0, 5460)]), "0-5460");
        assert_eq!(slot_ranges(&[(42, 42)]), "42");
        assert_eq!(
            slot_ranges(&[(0, 99), (200, 200), (300, 5460)]),
            "0-99,200,300-5460"
        );
        assert_eq!(slot_count(&[(0, 5460)]), 5461);
        assert_eq!(slot_count(&[(0, 16_383)]), 16_384);
        assert_eq!(slot_count(&[(42, 42), (0, 1)]), 3);
    }

    #[test]
    fn cut_cells_keep_a_gap_before_the_next_column() {
        let mut n = crate::tui::fixtures::cluster_report()
            .nodes
            .unwrap()
            .remove(0);
        n.slots = vec![(0, 99), (200, 200), (300, 5460)];
        n.endpoint.host = "cache-node-0001.example.internal".into();
        let cols = columns(17);
        let (keep, slots_w) = pick_cols(&cols, 78, usize::MAX, false);
        let line = node_row(&n, &cols, &keep, slots_w.min(14));
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with(" 0-99,200,300-5460 ") || text.starts_with(" 0-99,200,30…"),
            "{text}"
        );
        assert!(text.contains("cache-node-0001…"), "{text}");
    }

    #[test]
    fn every_state_has_a_word_and_a_glyph() {
        for s in [NodeStatus::Scanned, NodeStatus::Partial, NodeStatus::Failed] {
            let (text, _) = status(s);
            let (glyph, word) = text.split_once(' ').unwrap();
            assert!(["✓", "▲", "✗"].contains(&glyph), "{text}");
            assert!(word.len() > 3, "{text}");
        }
    }

    #[test]
    fn the_summary_counts_replicas_failures_and_partials() {
        let mut nodes = crate::tui::fixtures::cluster_report().nodes.unwrap();
        assert_eq!(summary(&nodes), "3 shards");
        nodes[1].role = NodeRole::Replica;
        nodes[2].status = NodeStatus::Partial;
        assert_eq!(
            summary(&nodes),
            "3 shards · 1 read on a replica · 1 partial"
        );
    }

    #[test]
    fn the_more_column_needs_v() {
        let cols = columns(16);
        let (keep, _) = pick_cols(&cols, 78, usize::MAX, false);
        assert!(!keep.contains(&SAMPLED));
        let (keep, _) = pick_cols(&cols, 100, usize::MAX, true);
        assert!(keep.contains(&SAMPLED));
    }
}
