//! View 4: the keys with the highest LFU counters (`OBJECT FREQ`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::format;
use crate::model::{KeySample, ScanReport, Ttl};
use crate::pattern::pattern_for_key;
use crate::tui::app::App;

use super::widgets::{
    bar, fit, marquee_spans, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset,
};
use super::{notice_panels, report_or_wait, split_body, style};

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

fn key_width(inner: usize) -> usize {
    let fixed = PATTERN_W + TYPE_W + FREQ_W + 2 + HEAT_W + 2 + SIZE_W + 2 + FLAG_W;
    inner.saturating_sub(fixed).max(12)
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
        notice_panels(frame, area, "Hot keys", &policy(report), lines);
        return;
    }
    let keys = &report.hot_keys;
    let (main, detail) = split_body(area);
    let sel = app.sel.hot.min(keys.len().saturating_sub(1));
    let right = format!(
        "{} · OBJECT FREQ on sampled keys · hottest first",
        policy(report)
    );
    let inner = panel(frame, main, "Hot keys", &right);
    let kw = key_width(usize::from(inner.width));
    let head = Line::from(vec![
        Span::styled(pad_r(" KEY", kw), style::hd()),
        Span::styled(pad_r("PATTERN", PATTERN_W), style::hd()),
        Span::styled(pad_r("TYPE", TYPE_W), style::hd()),
        Span::styled(format!("{}  ", pad_l("FREQ", FREQ_W)), style::hd()),
        Span::styled(pad_r("HEAT", HEAT_W + 2), style::hd()),
        Span::styled(format!("{}  ", pad_l("SIZE", SIZE_W)), style::hd()),
    ]);
    rows(frame, inner, vec![head], None);
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
        .map(|(i, k)| hot_row(k, kw, (off + i == sel).then(|| app.sel_ticks())))
        .collect();
    rows(frame, body, lines, Some(sel - off));
    if let Some(k) = keys.get(sel) {
        detail_panel(frame, detail, k);
    }
}

fn hot_row(k: &KeySample, kw: usize, scroll: Option<u64>) -> Line<'static> {
    let dash = || "—".to_string();
    let freq = k.lfu_freq.unwrap_or(0);
    let big = is_hot_big(k);
    let key = vec![Span::styled(format!(" {}", k.key), style::fg())];
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(key, kw.saturating_sub(1), ticks),
        None => fit(key, kw.saturating_sub(1)),
    };
    spans.push(Span::raw(" "));
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

fn detail_panel(frame: &mut Frame, area: Rect, k: &KeySample) {
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
        Line::default(),
    ];
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
