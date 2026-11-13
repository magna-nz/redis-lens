//! View 4: the keys with the highest LFU counters (`OBJECT FREQ`). Without any, the keys
//! named most often by the last Monitor run of the session ([`monitor_ranking`]).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::format;
use crate::model::{KeySample, NodeReport, ScanReport, Ttl};
use crate::pattern::pattern_for_key;
use crate::tui::app::App;
use crate::tui::app::views::server::MonitorHot;
use crate::value::escape;

use super::widgets::{
    bar, fit, marquee_spans, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset, trunc,
};
use super::{mark_previous, notice_panels, report_or_wait, split_body, style};

/// A hot key at least this big is flagged.
pub const HOT_BIG: u64 = 10 * 1024 * 1024;
/// A key needs at least this LFU counter to count as hot for the hot+big flag.
pub const HOT_FREQ: u8 = 100;

const PATTERN_W: usize = 24;
const TYPE_W: usize = 8;
const FREQ_W: usize = 4;
const HEAT_W: usize = 10;
const SIZE_W: usize = 6;
const FLAG_W: usize = 10;
/// The node column of a cluster report: `10.0.1.11:6379` and a gap.
const NODE_W: usize = 16;

/// The key column; `cluster` makes room for the node column.
fn key_width(inner: usize, cluster: bool) -> usize {
    let mut fixed = PATTERN_W + TYPE_W + FREQ_W + 2 + HEAT_W + 2 + SIZE_W + 2 + FLAG_W;
    if cluster {
        fixed += NODE_W;
    }
    inner.saturating_sub(fixed).max(12)
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
    let Some((_, report)) = report_or_wait(frame, area, app, "Hot keys") else {
        return;
    };
    if let Some(hot) = app.monitor_hot_keys() {
        monitor_ranking(frame, area, app, hot);
        return;
    }
    if let Some(reason) = app.hot_keys_reason() {
        let mut lines = vec![Line::from(Span::styled(
            " No hot keys to show.",
            style::fgb(),
        ))];
        lines.extend(
            reason
                .split_inclusive(". ")
                .map(|s| Line::from(Span::styled(format!(" {}", s.trim_end()), style::dim()))),
        );
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            if app.views.server.monitor.ranking_pending() {
                " Ranking the keys of the last MONITOR run…"
            } else {
                " Or rank keys by how often MONITOR sees them: Server (3) › Monitor."
            },
            style::dim(),
        )));
        let right = mark_previous(app, &policy(report));
        notice_panels(frame, area, "Hot keys", &right, lines);
        return;
    }
    let keys = &report.hot_keys;
    let (main, detail) = split_body(area);
    let sel = app.sel.hot.min(keys.len().saturating_sub(1));
    let right = mark_previous(
        app,
        &format!(
            "{} · OBJECT FREQ on sampled keys · hottest first",
            policy(report)
        ),
    );
    let inner = panel(frame, main, "Hot keys", &right);
    let nodes = report.nodes.as_deref();
    let kw = key_width(usize::from(inner.width), nodes.is_some());
    let mut head = vec![Span::styled(pad_r(" KEY", kw), style::hd())];
    if nodes.is_some() {
        head.push(Span::styled(pad_r("NODE", NODE_W), style::hd()));
    }
    head.extend([
        Span::styled(pad_r("PATTERN", PATTERN_W), style::hd()),
        Span::styled(pad_r("TYPE", TYPE_W), style::hd()),
        Span::styled(format!("{}  ", pad_l("FREQ", FREQ_W)), style::hd()),
        Span::styled(pad_r("HEAT", HEAT_W + 2), style::hd()),
        Span::styled(format!("{}  ", pad_l("SIZE", SIZE_W)), style::hd()),
    ]);
    rows(frame, inner, vec![Line::from(head)], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = keys
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .enumerate()
        .map(|(i, k)| {
            let node = node_of(k, nodes);
            hot_row(k, kw, node, (off + i == sel).then(|| app.sel_ticks()))
        })
        .collect();
    rows(frame, body, lines, Some(sel - off));
    if let Some(k) = keys.get(sel) {
        detail_panel(frame, detail, k, node_of(k, nodes));
    }
}

fn hot_row(k: &KeySample, kw: usize, node: Option<String>, scroll: Option<u64>) -> Line<'static> {
    let dash = || "—".to_string();
    let freq = k.lfu_freq.unwrap_or(0);
    let big = is_hot_big(k);
    let key = vec![Span::styled(format!(" {}", k.key), style::fg())];
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(key, kw.saturating_sub(1), ticks),
        None => fit(key, kw.saturating_sub(1)),
    };
    spans.push(Span::raw(" "));
    if let Some(node) = node {
        // One cell stays free before the pattern, also when the address is cut.
        spans.push(Span::styled(
            pad_r(&trunc(&node, NODE_W - 1), NODE_W),
            style::dim(),
        ));
    }
    spans.extend(fit(
        pattern_spans(&pattern_for_key(&k.key), style::dim(), style::ph()),
        PATTERN_W - 1,
    ));
    spans.push(Span::raw(" "));
    let (fill, track) = bar(f64::from(freq) / 255.0, HEAT_W);
    spans.extend([
        Span::styled(pad_r(k.key_type.as_str(), TYPE_W), style::dim()),
        Span::styled(
            format!("{}  ", pad_l(&freq.to_string(), FREQ_W)),
            style::fgb(),
        ),
        Span::styled(fill, style::warn()),
        Span::styled(format!("{track}  "), style::track()),
        Span::styled(
            format!(
                "{}  ",
                pad_l(&k.bytes.map_or_else(dash, format::bytes), SIZE_W)
            ),
            if big { style::warn() } else { style::fg() },
        ),
        Span::styled(if big { "▲ hot+big" } else { "" }, style::warn()),
    ]);
    Line::from(spans)
}

fn detail_panel(frame: &mut Frame, area: Rect, k: &KeySample, node: Option<String>) {
    let pattern = pattern_for_key(&k.key);
    let right = format!("{} · {pattern}", k.key_type.as_str());
    let inner = panel(frame, area, &k.key, &right);
    let w = usize::from(inner.width);
    let freq = k.lfu_freq.unwrap_or(0);
    let size = k
        .bytes
        .map_or_else(|| "— (no MEMORY USAGE)".to_string(), format::bytes);
    let (ttl, ttl_style) = match k.ttl {
        Ttl::Persistent => ("none".to_string(), style::dim()),
        Ttl::Expires(d) => (format::duration(d), style::fgb()),
    };
    let label = |s: &str| Span::styled(pad_r(&format!(" {s}"), 8), style::dim());
    let mut lines = vec![
        Line::from(Span::styled(" SELECTED KEY", style::hd())),
        Line::from(vec![
            label("FREQ"),
            Span::styled(freq.to_string(), style::fgb()),
            Span::styled(" (log counter, max 255)", style::dim()),
        ]),
        Line::from(vec![
            label("SIZE"),
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
    ];
    if let Some(node) = node {
        lines.push(Line::from(vec![
            label("NODE"),
            Span::styled(node, style::fgb()),
        ]));
    }
    lines.push(Line::default());
    if is_hot_big(k) {
        let note = format!(
            " ▲ {} is both hot and {}. Every read moves a lot of data; trim it or split it.",
            k.key,
            k.bytes.map_or_else(String::new, format::bytes)
        );
        lines.push(Line::from(fit(vec![Span::styled(note, style::warn())], w)));
    } else {
        lines.push(Line::from(Span::styled(
            " Frequently read keys are good candidates for a client-side cache.",
            style::dim(),
        )));
    }
    rows(frame, inner, lines, None);
}

const COUNT_W: usize = 8;
const SHARE_W: usize = 7;

/// `once`, `twice`, `3 times`.
fn times(n: u64) -> String {
    match n {
        1 => "once".to_string(),
        2 => "twice".to_string(),
        n => format!("{} times", format::grouped(n)),
    }
}

/// `from MONITOR, 30 s, 8,412 lines, at 13:04 UTC`.
fn monitor_source(hot: &MonitorHot) -> String {
    let s = hot.at_unix.rem_euclid(86_400);
    format!(
        "from MONITOR, {} s, {} lines, at {:02}:{:02} UTC",
        hot.secs,
        format::grouped(hot.ranking.lines),
        s / 3600,
        (s % 3600) / 60
    )
}

/// Hot keys ranked from the last Monitor run (spec "4 Hot keys"): key, pattern, how
/// often the run's lines name it. Never saved: it lives in the Server view's state.
fn monitor_ranking(frame: &mut Frame, area: Rect, app: &App, hot: &MonitorHot) {
    let (main, detail) = split_body(area);
    let keys = &hot.ranking.keys;
    let sel = app.sel.hot.min(keys.len().saturating_sub(1));
    let right = mark_previous(app, &monitor_source(hot));
    let inner = panel(frame, main, "Hot keys", &right);
    let w = usize::from(inner.width);
    let kw = w.saturating_sub(PATTERN_W + COUNT_W + SHARE_W + 2).max(12);
    let head = Line::from(fit(
        vec![Span::styled(
            format!(
                "{}{}{}{}",
                pad_r(" KEY", kw),
                pad_r("PATTERN", PATTERN_W),
                pad_l("COUNT", COUNT_W),
                pad_l("LINES", SHARE_W),
            ),
            style::hd(),
        )],
        w,
    ));
    rows(frame, inner, vec![head], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
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
                marquee_spans(key, kw.saturating_sub(1), app.sel_ticks())
            } else {
                fit(key, kw.saturating_sub(1))
            };
            spans.push(Span::raw(" "));
            spans.extend(fit(
                pattern_spans(&escape(&pattern_for_key(k)), style::dim(), style::ph()),
                PATTERN_W - 1,
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                pad_l(&format::grouped(*n), COUNT_W),
                style::fgb(),
            ));
            spans.push(Span::styled(
                pad_l(&format::percent(*n as f64 / total as f64), SHARE_W),
                style::dim(),
            ));
            Line::from(fit(spans, w))
        })
        .collect();
    if keys.is_empty() {
        lines.push(Line::from(Span::styled(
            " The last MONITOR run named no keys.",
            style::dim(),
        )));
    }
    rows(frame, body, lines, (!keys.is_empty()).then(|| sel - off));

    let dinner = panel(frame, detail, "From MONITOR", "");
    let dw = usize::from(dinner.width);
    let note = |t: String| Line::from(fit(vec![Span::styled(t, style::dim())], dw));
    let mut dl = Vec::new();
    if let Some((k, n)) = keys.get(sel) {
        dl.push(Line::from(fit(
            vec![
                Span::styled(format!(" {}", escape(k)), style::fgb()),
                Span::styled(
                    format!(
                        " · named {} in {} lines",
                        times(*n),
                        format::grouped(hot.ranking.lines)
                    ),
                    style::dim(),
                ),
            ],
            dw,
        )));
    }
    dl.push(Line::default());
    dl.push(note(
        " Ranked by how often the last MONITOR run named each key, because the scan".into(),
    ));
    dl.push(note(
        " has no LFU counters: a short sample, not the server's own counters.".into(),
    ));
    if hot.ranking.unknown > 0 {
        let n = hot.ranking.unknown;
        let (lines, verb) = if n == 1 {
            ("line", "has")
        } else {
            ("lines", "have")
        };
        dl.push(Line::from(fit(
            vec![Span::styled(
                format!(
                    " ▲ {} {lines} {verb} a command whose keys are unknown.",
                    format::grouped(n)
                ),
                style::amber(),
            )],
            dw,
        )));
        if let Some(why) = &hot.keys_note {
            dl.push(note(format!("   {why}")));
        }
    }
    rows(frame, dinner, dl, None);
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
}
