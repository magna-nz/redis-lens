//! View 5: the shards of a cluster scan, one row per node scanned, with a detail panel
//! for the selected node. `enter` narrows 1 Patterns to that node.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::format;
use crate::model::{NodeReport, NodeRole, NodeStatus};
use crate::tui::app::App;

use super::widgets::{fit, pad_l, pad_r, panel, rows, scroll_offset, trunc, width};
use super::{mark_previous, notice_panels, report_or_wait, server_label, split_body, style};

const SLOTS_W: usize = 13;
const ROLE_W: usize = 9;
const STATUS_W: usize = 9;
const NUM_W: usize = 7;

/// The endpoint column: wide enough for `10.0.1.11:6379` and a gap, wider on wide
/// terminals for host names and IPv6.
fn endpoint_width(inner: usize) -> usize {
    if inner >= 100 { 24 } else { 17 }
}

/// Every column but the note, which takes what is left.
fn fixed(ep: usize) -> usize {
    1 + SLOTS_W + ep + ROLE_W + STATUS_W + 3 * (NUM_W + 2)
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

/// The status label and its style: `failed` red, `partial` amber, `scanned` teal.
pub fn status(s: NodeStatus) -> (&'static str, Style) {
    match s {
        NodeStatus::Scanned => ("scanned", style::ok()),
        NodeStatus::Partial => ("▲ partial", style::amber()),
        NodeStatus::Failed => ("✗ failed", style::warn()),
    }
}

/// The panel's right title: shard count, then how many failed or are partial.
fn summary(nodes: &[NodeReport]) -> String {
    let n = nodes.len();
    let count = |s: NodeStatus| nodes.iter().filter(|x| x.status == s).count();
    let mut out = format!("{n} shard{}", if n == 1 { "" } else { "s" });
    let replicas = nodes.iter().filter(|x| x.role == NodeRole::Replica).count();
    if replicas > 0 {
        out.push_str(&format!(" · {replicas} on replicas"));
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
    out.push_str(" · slot order · enter shows a node's patterns");
    out
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some((_, report)) = report_or_wait(frame, area, app, "Nodes") else {
        return;
    };
    let Some(nodes) = report.nodes.as_deref() else {
        notice_panels(
            frame,
            area,
            "Nodes",
            &mark_previous(app, "standalone"),
            vec![
                Line::from(Span::styled(
                    " Not a cluster: this scan read one standalone server.",
                    style::fgb(),
                )),
                Line::from(Span::styled(
                    " On a cluster this view lists each shard and the node scanned for it.",
                    style::dim(),
                )),
            ],
        );
        return;
    };
    if nodes.is_empty() {
        notice_panels(
            frame,
            area,
            "Nodes",
            &mark_previous(app, "0 shards"),
            vec![Line::from(Span::styled(
                " The cluster scan has no shards to show.",
                style::dim(),
            ))],
        );
        return;
    }
    let (main, detail) = split_body(area);
    let sel = app.sel.nodes.min(nodes.len() - 1);
    let inner = panel(frame, main, "Nodes", &mark_previous(app, &summary(nodes)));
    let iw = usize::from(inner.width);
    let ep = endpoint_width(iw);
    let note_w = iw.saturating_sub(fixed(ep));
    let mut head = vec![
        Span::styled(pad_r(" SLOTS", SLOTS_W + 1), style::hd()),
        Span::styled(pad_r("ENDPOINT", ep), style::hd()),
        Span::styled(pad_r("ROLE", ROLE_W), style::hd()),
        Span::styled(pad_r("STATUS", STATUS_W), style::hd()),
        Span::styled(format!("{}  ", pad_l("KEYS", NUM_W)), style::hd()),
        Span::styled(format!("{}  ", pad_l("SAMPLED", NUM_W)), style::hd()),
        Span::styled(format!("{}  ", pad_l("MEMORY", NUM_W)), style::hd()),
    ];
    if note_w >= 4 {
        head.push(Span::styled("NOTE", style::hd()));
    }
    rows(frame, inner, vec![Line::from(head)], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = nodes
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .map(|n| node_row(n, ep, note_w))
        .collect();
    rows(frame, body, lines, Some(sel - off));
    detail_panel(frame, detail, &nodes[sel]);
}

fn node_row(n: &NodeReport, ep: usize, note_w: usize) -> Line<'static> {
    let dash = || "—".to_string();
    let (st, st_style) = status(n.status);
    let keys = n
        .server
        .as_ref()
        .map_or_else(dash, |s| format::count(s.total_keys));
    let sampled = if n.status == NodeStatus::Failed {
        dash()
    } else {
        format::count(n.sampled_keys)
    };
    let mem = n
        .server
        .as_ref()
        .map_or_else(dash, |s| format::bytes(s.used_memory));
    // A cut cell keeps one free cell before the next column.
    let cell = |s: &str, w: usize| pad_r(&trunc(s, w - 1), w);
    let mut spans = vec![
        Span::styled(
            format!(" {}", cell(&slot_ranges(&n.slots), SLOTS_W)),
            style::fg(),
        ),
        Span::styled(cell(&n.endpoint.to_string(), ep), style::fgb()),
        Span::styled(pad_r(role(n.role), ROLE_W), style::dim()),
        Span::styled(pad_r(st, STATUS_W), st_style),
        Span::styled(format!("{}  ", pad_l(&keys, NUM_W)), style::fg()),
        Span::styled(format!("{}  ", pad_l(&sampled, NUM_W)), style::fg()),
        Span::styled(format!("{}  ", pad_l(&mem, NUM_W)), style::fg()),
    ];
    if let Some(note) = &n.note
        && note_w >= 4
    {
        spans.push(Span::styled(
            trunc(note, note_w.saturating_sub(1)),
            note_style(n),
        ));
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

/// `text` cut into lines of at most `w` cells at spaces (a word longer than `w` is
/// cut with `…`).
fn wrap(text: &str, w: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let need = if cur.is_empty() {
            width(word)
        } else {
            width(&cur) + 1 + width(word)
        };
        if need <= w || cur.is_empty() {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
            if width(&cur) > w {
                cur = trunc(&cur, w);
            }
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(&trunc(word, w));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn kv(label: &str, value: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(format!(" {}", pad_r(label, 10)), style::dim())];
    spans.extend(value);
    spans
}

fn detail_panel(frame: &mut Frame, area: Rect, n: &NodeReport) {
    let (st, _) = status(n.status);
    let right = format!(
        "{} · {}",
        role(n.role),
        st.trim_start_matches(['▲', '✗', ' '])
    );
    let inner = panel(frame, area, &n.endpoint.to_string(), &right);
    let w = usize::from(inner.width);
    let mut lines: Vec<Vec<Span<'static>>> = vec![
        vec![Span::styled(" NODE", style::hd())],
        kv("ID", vec![Span::styled(n.id.clone(), style::fg())]),
    ];
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
                            " · {} keys · {} used",
                            format::grouped(s.total_keys),
                            format::bytes(s.used_memory)
                        ),
                        style::dim(),
                    ),
                ];
                if s.maxmemory > 0 {
                    v.push(Span::styled(
                        format!(" / {}", format::bytes(s.maxmemory)),
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
                    " keys · {} pattern{}",
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
                " · sizes unavailable: MEMORY USAGE denied",
                style::warn(),
            ));
        }
        lines.push(kv("Sampled", v));
    }
    let mut out: Vec<Line<'static>> = lines.into_iter().map(|l| Line::from(fit(l, w))).collect();
    // The whole note, wrapped over the rows that are left.
    let room = usize::from(inner.height).saturating_sub(out.len());
    match &n.note {
        Some(note) if room > 0 => {
            let mark = if n.status == NodeStatus::Failed {
                "✗"
            } else {
                "▲"
            };
            let text = format!("{mark} {note}");
            let wrapped = wrap(&text, w.saturating_sub(1));
            let cut = wrapped.len() > room;
            for (i, l) in wrapped.into_iter().take(room).enumerate() {
                let l = if cut && i + 1 == room {
                    trunc(&format!("{l} …"), w.saturating_sub(1))
                } else {
                    l
                };
                out.push(Line::from(Span::styled(format!(" {l}"), note_style(n))));
            }
        }
        None if room > 0 => {
            out.push(Line::from(Span::styled(
                trunc(
                    " enter shows this node's own patterns on 1 Patterns; esc there goes back.",
                    w,
                ),
                style::dim(),
            )));
        }
        _ => {}
    }
    rows(frame, inner, out, None);
}

#[cfg(test)]
mod tests {
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
        let line = node_row(&n, 17, 0);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with(" 0-99,200,30… cache-node-0001… primary"),
            "{text}"
        );
    }

    #[test]
    fn wrap_keeps_words_and_width() {
        assert_eq!(wrap("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap("", 4), Vec::<String>::new());
        let long = wrap("abcdefghij k", 4);
        assert_eq!(long, ["abc…", "k"]);
        for l in wrap(
            "the scan of this node stopped early (a connection or server error); its results are partial",
            20,
        ) {
            assert!(width(&l) <= 20, "{l}");
        }
    }
}
