//! Analyze › Hot keys: the keys read most often, by their LFU counters. Without any, the
//! keys named most often by the last Monitor run of the session ([`monitor_ranking`]).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::format;
use crate::model::{KeySample, NodeReport, ScanReport, Ttl};
use crate::pattern::pattern_for_key;
use crate::tui::app::App;
use crate::tui::app::views::server::MonitorHot;
use crate::value::escape;

use super::analysis::{
    Col, MARK, Tier, head_row, left_width, need_report, notice, paragraph, pick_cols, title_right,
};
use super::widgets::{
    Pane, Title, bar, fit, marquee_spans, pad_l, pad_r, pattern_spans, rows, scroll_offset,
    split_frame, trunc,
};
use super::{mark_previous, style};

/// A hot key at least this big is flagged.
pub const HOT_BIG: u64 = 10 * 1024 * 1024;
/// A key needs at least this LFU counter to count as hot for the hot+big flag.
pub const HOT_FREQ: u8 = 100;

const PATTERN_W: usize = 24;
const TYPE_W: usize = 8;
const FREQ_W: usize = 4;
const HEAT_W: usize = 10;
const SIZE_W: usize = 6;
/// The node column of a cluster report: `10.0.1.11:6379`.
const NODE_W: usize = 15;

/// The list pane shows the Wide columns from this many cells.
const WIDE_AT: usize = 56;

/// The columns of the sampled ranking; `cluster` adds the node. Cells are found by
/// their headings.
fn columns(cluster: bool) -> Vec<Col> {
    let mut cols = vec![
        Col::new("KEY", 14, Tier::Base),
        Col::new("FREQ", FREQ_W, Tier::Base),
        Col::new("SIZE", SIZE_W, Tier::Base),
        Col::new("HEAT", HEAT_W, Tier::Wide),
        Col::new("TYPE", TYPE_W, Tier::Wide),
    ];
    if cluster {
        cols.push(Col::new("NODE", NODE_W, Tier::Wide));
    }
    cols.push(Col::new("PATTERN", PATTERN_W, Tier::More));
    cols
}

/// The address of the node `k` was sampled on, from [`KeySample::node`]: `None` on a
/// standalone report, `—` when the index is not in the report's nodes.
fn node_of(k: &KeySample, nodes: Option<&[NodeReport]>) -> Option<String> {
    let nodes = nodes?;
    Some(
        k.node
            .and_then(|i| nodes.get(usize::from(i)))
            .map_or_else(|| "—".to_string(), |n| n.endpoint.to_string()),
    )
}

fn is_hot_big(k: &KeySample) -> bool {
    k.bytes.is_some_and(|b| b >= HOT_BIG) && k.lfu_freq.is_some_and(|f| f >= HOT_FREQ)
}

fn policy(report: &ScanReport) -> String {
    format!("policy {}", report.server.maxmemory_policy)
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if need_report(
        frame,
        area,
        app,
        "Hot keys",
        "ranks the keys read most often.",
    ) {
        return;
    }
    let Some(report) = app.report() else {
        return;
    };
    if let Some(hot) = app.monitor_hot_keys() {
        monitor_ranking(frame, area, app, hot);
        return;
    }
    if let Some(reason) = app.hot_keys_reason() {
        let w = usize::from(area.width.saturating_sub(4));
        let mut lines = vec![Line::from(Span::styled(
            " No hot keys to show.",
            style::fgb(),
        ))];
        lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            &reason,
            style::dim(),
            w,
        ));
        lines.push(Line::default());
        lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            if app.views.server.monitor.ranking_pending() {
                "Ranking the keys of the last Monitor run…"
            } else {
                "Or rank keys by how often Monitor sees them: 4 Server › Monitor."
            },
            style::dim(),
            w,
        ));
        let right = mark_previous(app, &policy(report));
        notice(frame, area, "Hot keys", &right, lines);
        return;
    }
    let keys = &report.hot_keys;
    let more = app.more_here();
    let sel = app.sel.hot.min(keys.len().saturating_sub(1));
    let right = title_right(
        app,
        area.width,
        &format!("{} · hottest first", policy(report)),
    );
    let nodes = report.nodes.as_deref();
    let selected = keys.get(sel);
    let title = selected.map_or_else(String::new, |k| k.key.clone());
    let kind = selected.map_or_else(String::new, |k| k.key_type.as_str().to_string());
    let (left, detail) = split_frame(
        frame,
        area,
        left_width(area.width),
        Title::new("Hot keys", &right),
        Title::new(&title, &kind),
        Pane::Left,
    );
    let cols = columns(nodes.is_some());
    let (keep, kw) = pick_cols(&cols, usize::from(left.width), WIDE_AT, more);
    let right_aligned: Vec<usize> = (0..cols.len())
        .filter(|&i| matches!(cols[i].head, "FREQ" | "SIZE"))
        .collect();
    rows(
        frame,
        left,
        vec![head_row(&cols, &keep, kw, &right_aligned)],
        None,
    );
    let body = Rect {
        y: left.y + 1,
        height: left.height.saturating_sub(1),
        ..left
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = keys
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .enumerate()
        .map(|(i, k)| {
            let node = node_of(k, nodes);
            let scroll = (off + i == sel).then(|| app.sel_ticks());
            hot_row(&cols, &keep, k, kw, node, scroll)
        })
        .collect();
    rows(frame, body, lines, Some(sel - off));
    if let Some(k) = selected {
        detail_lines(frame, detail, k, node_of(k, nodes));
    }
}

fn hot_row(
    cols: &[Col],
    keep: &[usize],
    k: &KeySample,
    kw: usize,
    node: Option<String>,
    scroll: Option<u64>,
) -> Line<'static> {
    let dash = || "—".to_string();
    let freq = k.lfu_freq.unwrap_or(0);
    let big = is_hot_big(k);
    let key = vec![Span::styled(format!(" {}", k.key), style::fg())];
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(key, kw, ticks),
        None => fit(key, kw),
    };
    for &i in &keep[1..] {
        spans.push(Span::raw(" "));
        let w = cols[i].w;
        match cols[i].head {
            "FREQ" => spans.push(Span::styled(pad_l(&freq.to_string(), w), style::fgb())),
            "SIZE" => {
                let size = k.bytes.map_or_else(dash, format::bytes);
                // Hot and big: the mark, then the size, in red.
                let (text, st) = if big {
                    (format!("{MARK} {size}"), style::warn())
                } else {
                    (size, style::fg())
                };
                spans.push(Span::styled(pad_l(&text, w), st));
            }
            "HEAT" => {
                let (fill, track) = bar(f64::from(freq) / 255.0, w);
                spans.push(Span::styled(fill, style::warn()));
                spans.push(Span::styled(track, style::track()));
            }
            "TYPE" => spans.push(Span::styled(pad_r(k.key_type.as_str(), w), style::dim())),
            "NODE" => {
                let node = node.clone().unwrap_or_default();
                spans.push(Span::styled(pad_r(&trunc(&node, w), w), style::dim()));
            }
            _ => spans.extend(fit(
                pattern_spans(&pattern_for_key(&k.key), style::dim(), style::ph()),
                w,
            )),
        }
    }
    Line::from(spans)
}

fn detail_lines(frame: &mut Frame, inner: Rect, k: &KeySample, node: Option<String>) {
    let w = usize::from(inner.width);
    let freq = k.lfu_freq.unwrap_or(0);
    let size = k
        .bytes
        .map_or_else(|| "— (no MEMORY USAGE)".to_string(), format::bytes);
    let (ttl, ttl_style) = match k.ttl {
        Ttl::Persistent => ("never expires".to_string(), style::dim()),
        Ttl::Expires(d) => (format::duration(d), style::fgb()),
    };
    let label = |s: &str| Span::styled(pad_r(&format!(" {s}"), 9), style::dim());
    let mut lines = vec![
        Line::from(fit(
            vec![
                label("Reads"),
                Span::styled(format!("{freq} of 255"), style::fgb()),
            ],
            w,
        )),
        Line::from(vec![
            label("Size"),
            Span::styled(
                size,
                if is_hot_big(k) {
                    style::warn()
                } else {
                    style::fgb()
                },
            ),
        ]),
        Line::from(vec![label("TTL"), Span::styled(ttl, ttl_style)]),
        Line::from(vec![
            label("Pattern"),
            Span::styled(pattern_for_key(&k.key), style::fg()),
        ]),
    ];
    if let Some(node) = node {
        lines.push(Line::from(vec![
            label("Node"),
            Span::styled(node, style::fgb()),
        ]));
    }
    lines.push(Line::default());
    if is_hot_big(k) {
        let note = format!(
            "This key is both hot and {}. Every read moves a lot of data; trim it or split it.",
            k.bytes.map_or_else(String::new, format::bytes)
        );
        lines.extend(paragraph(
            vec![Span::styled(format!(" {MARK} "), style::warn())],
            "   ",
            &note,
            style::warn(),
            w,
        ));
    } else {
        lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            "Frequently read keys are good candidates for a client-side cache.",
            style::dim(),
            w,
        ));
    }
    rows(frame, inner, lines, None);
}

const COUNT_W: usize = 7;
const SHARE_W: usize = 6;

/// `once`, `twice`, `3 times`.
fn times(n: u64) -> String {
    match n {
        1 => "once".to_string(),
        2 => "twice".to_string(),
        n => format!("{} times", format::grouped(n)),
    }
}

/// `from Monitor, 30 s, 8,412 lines, at 13:04 UTC`.
fn monitor_source(hot: &MonitorHot) -> String {
    let s = hot.at_unix.rem_euclid(86_400);
    format!(
        "from Monitor, {} s, {} lines, at {:02}:{:02} UTC",
        hot.secs,
        format::grouped(hot.ranking.lines),
        s / 3600,
        (s % 3600) / 60
    )
}

const MONITOR_COLS: [Col; 4] = [
    Col::new("KEY", 14, Tier::Base),
    Col::new("COUNT", COUNT_W, Tier::Base),
    Col::new("LINES", SHARE_W, Tier::Wide),
    Col::new("PATTERN", PATTERN_W, Tier::More),
];

/// Hot keys ranked from the last Monitor run (spec "4 Hot keys"): key, how often the
/// run's lines name it, pattern with `v`. Never saved: it lives in the Server view's
/// state.
fn monitor_ranking(frame: &mut Frame, area: Rect, app: &App, hot: &MonitorHot) {
    let keys = &hot.ranking.keys;
    let more = app.more_here();
    let sel = app.sel.hot.min(keys.len().saturating_sub(1));
    let right = title_right(app, area.width, &monitor_source(hot));
    let (left, detail) = split_frame(
        frame,
        area,
        left_width(area.width),
        Title::new("Hot keys", &right),
        Title::new("From Monitor", ""),
        Pane::Left,
    );
    let w = usize::from(left.width);
    let (keep, kw) = pick_cols(&MONITOR_COLS, w, WIDE_AT, more);
    rows(
        frame,
        left,
        vec![head_row(&MONITOR_COLS, &keep, kw, &[1, 2])],
        None,
    );
    let body = Rect {
        y: left.y + 1,
        height: left.height.saturating_sub(1),
        ..left
    };
    let total = hot.ranking.lines.max(1);
    let off = scroll_offset(sel, usize::from(body.height));
    let mut lines: Vec<Line<'static>> = keys
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .enumerate()
        .map(|(i, (k, n))| {
            let key = vec![Span::styled(format!(" {}", escape(k)), style::fg())];
            let mut spans = if off + i == sel {
                marquee_spans(key, kw, app.sel_ticks())
            } else {
                fit(key, kw)
            };
            for &c in &keep[1..] {
                spans.push(Span::raw(" "));
                let cw = MONITOR_COLS[c].w;
                match c {
                    1 => spans.push(Span::styled(pad_l(&format::grouped(*n), cw), style::fgb())),
                    2 => spans.push(Span::styled(
                        pad_l(&format::percent(*n as f64 / total as f64), cw),
                        style::dim(),
                    )),
                    _ => spans.extend(fit(
                        pattern_spans(&escape(&pattern_for_key(k)), style::dim(), style::ph()),
                        cw,
                    )),
                }
            }
            Line::from(fit(spans, w))
        })
        .collect();
    if keys.is_empty() {
        lines.push(Line::from(Span::styled(
            " The last Monitor run named no keys.",
            style::dim(),
        )));
    }
    rows(frame, body, lines, (!keys.is_empty()).then(|| sel - off));

    let dw = usize::from(detail.width);
    let mut dl = Vec::new();
    if let Some((k, n)) = keys.get(sel) {
        dl.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            &format!(
                "{} · named {} in {} lines",
                escape(k),
                times(*n),
                format::grouped(hot.ranking.lines)
            ),
            style::fgb(),
            dw,
        ));
    }
    dl.push(Line::default());
    dl.extend(paragraph(
        vec![Span::raw(" ")],
        " ",
        "Ranked by how often the last Monitor run named each key, because the analysis has no LFU counters: a short sample, not the server's own counters.",
        style::dim(),
        dw,
    ));
    if hot.ranking.unknown > 0 {
        let n = hot.ranking.unknown;
        let (lines, verb) = if n == 1 {
            ("line", "has")
        } else {
            ("lines", "have")
        };
        dl.push(Line::default());
        dl.extend(paragraph(
            vec![Span::styled(format!(" {MARK} "), style::amber())],
            "   ",
            &format!(
                "{} {lines} {verb} a command whose keys are unknown.",
                format::grouped(n)
            ),
            style::amber(),
            dw,
        ));
        if let Some(why) = &hot.keys_note {
            dl.extend(paragraph(
                vec![Span::raw("   ")],
                "   ",
                why,
                style::dim(),
                dw,
            ));
        }
    }
    rows(frame, detail, dl, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::KeyType;

    fn key(bytes: u64, freq: u8) -> KeySample {
        KeySample {
            key: "k".into(),
            key_type: KeyType::List,
            bytes: Some(bytes),
            ttl: Ttl::Persistent,
            lfu_freq: Some(freq),
            node: None,
        }
    }

    #[test]
    fn hot_keys_before_an_analysis_says_what_it_needs() {
        use super::super::analysis::support::no_analysis;
        use crate::tui::app::views::testing;
        for (w, h) in [(80, 24), (120, 36)] {
            let app = no_analysis(crate::tui::app::Screen::HotKeys);
            let s = testing::render(&app, w, h).backend().to_string();
            assert!(s.contains("waits for an analysis"), "{s}");
            assert!(
                s.contains("Hot keys ranks the keys read most often."),
                "{s}"
            );
            assert!(s.contains("r  Analyze: "), "{s}");
            insta::assert_snapshot!(format!("hot_keys_no_analysis_{w}x{h}"), s);
        }
    }

    #[test]
    fn hot_big_needs_both() {
        assert!(is_hot_big(&key(HOT_BIG, HOT_FREQ)));
        assert!(is_hot_big(&key(600 * 1024 * 1024, 243)));
        // Big but barely read.
        assert!(!is_hot_big(&key(600 * 1024 * 1024, 5)));
        assert!(!is_hot_big(&key(HOT_BIG, HOT_FREQ - 1)));
        // Hot but small.
        assert!(!is_hot_big(&key(HOT_BIG - 1, 255)));
        let mut unknown = key(HOT_BIG, 255);
        unknown.bytes = None;
        assert!(!is_hot_big(&unknown));
    }

    #[test]
    fn the_node_column_is_only_on_a_cluster_and_comes_with_width() {
        let (keep, _) = pick_cols(&columns(false), 41, WIDE_AT, false);
        let heads: Vec<_> = keep.iter().map(|&i| columns(false)[i].head).collect();
        assert_eq!(heads, ["KEY", "FREQ", "SIZE"]);
        let cols = columns(true);
        let (keep, _) = pick_cols(&cols, 69, WIDE_AT, false);
        let heads: Vec<_> = keep.iter().map(|&i| cols[i].head).collect();
        assert!(
            heads.contains(&"NODE") && !heads.contains(&"PATTERN"),
            "{heads:?}"
        );
        let standalone: Vec<_> = columns(false).iter().map(|c| c.head).collect();
        assert!(!standalone.contains(&"NODE"));
    }
}
