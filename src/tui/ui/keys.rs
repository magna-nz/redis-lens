//! Draws [`KeysState`]: the key list of one pattern (spec "1 Patterns, additions", "Key
//! list"). Owner: wave B2, with `views/keys.rs`.
//!
//! The table of loaded keys above, the selected key and the list's state below. Fits
//! 80 columns: the key column takes what the others leave, and long names are cut
//! with `…` (the selected one scrolls).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::analysis::{length_unit, match_glob, past_compact_encoding};
use crate::format;
use crate::live::LiveError;
use crate::live::keys::KeyRow;
use crate::model::Ttl;
use crate::tui::app::views::keys::{KeySort, KeysState};
use crate::tui::app::views::{Ctx, LiveState};

use super::widgets::{fit, marquee_spans, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset};
use super::{spinner, split_body, style};

const TYPE_W: usize = 7;
const TTL_W: usize = 8;
const SIZE_W: usize = 6;
const ENC_W: usize = 10;
const LEN_W: usize = 7;
const LFU_W: usize = 3;
/// The NODE column on a cluster: the address cut with `…` to 15 cells, and a space.
pub const NODE_W: usize = 16;
/// The narrowest the key column gets before the encoding column is hidden.
const KEY_MIN: usize = 16;

/// Column layout: the key column's width and whether the optional ones show.
#[derive(Debug, Clone, Copy)]
struct Cols {
    key: usize,
    enc: bool,
    lfu: bool,
    /// The NODE column after the key (a cluster).
    node: bool,
}

impl Cols {
    fn new(inner: usize, lfu: bool, node: bool) -> Self {
        let fixed = |enc: bool| {
            (if node { NODE_W } else { 0 })
                + (TYPE_W + 1)
                + (TTL_W + 2)
                + (SIZE_W + 2)
                + if enc { ENC_W + 1 } else { 0 }
                + (LEN_W + 2)
                + if lfu { LFU_W + 1 } else { 0 }
        };
        let enc = inner >= fixed(true) + KEY_MIN;
        Self {
            key: inner.saturating_sub(fixed(enc)).max(8),
            enc,
            lfu,
            node,
        }
    }
}

/// The announced address of node `i` of the session, for a NODE column or a detail
/// line; `—` when unknown.
pub fn node_name(ctx: &Ctx<'_>, i: Option<usize>) -> String {
    i.and_then(|i| ctx.session.nodes.get(i))
        .map_or_else(|| "—".to_string(), |n| n.endpoint.to_string())
}

/// The command a refused column is named by, and whether `e` is about it.
fn refused(state: &KeysState, cmd: &str) -> bool {
    state
        .refused
        .iter()
        .any(|e| matches!(e, LiveError::Denied(c) | LiveError::Unsupported(c) if c == cmd))
}

/// Sizes are off: not sent (the permission check refused `MEMORY USAGE`) or refused.
fn sizes_off(state: &KeysState) -> bool {
    !state.sizes || refused(state, "MEMORY USAGE")
}

pub fn draw(frame: &mut Frame, area: Rect, state: &KeysState, ctx: &Ctx<'_>) {
    let (main, detail) = split_body(area);
    let pattern = state.pattern.clone().unwrap_or_default();
    let visible = state.visible();
    let sel = state.sel.min(visible.len().saturating_sub(1));
    let inner = panel(
        frame,
        main,
        &format!("Patterns › {pattern}"),
        &title_right(state, ctx, &pattern, visible.len()),
    );
    let c = Cols::new(usize::from(inner.width), state.lfu, state.by_node());
    let sort = state.sort.effective(state.sizes, state.freq);
    rows(frame, inner, vec![header(&c, sort)], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if visible.is_empty() {
        rows(frame, body, empty_lines(state, ctx), None);
    } else {
        let off = scroll_offset(sel, usize::from(body.height));
        let ticks = ctx.tick.saturating_sub(state.sel_since);
        let lines: Vec<Line<'static>> = visible
            .iter()
            .enumerate()
            .skip(off)
            .take(usize::from(body.height))
            .map(|(i, &r)| {
                let row = &state.rows[r];
                let node = c
                    .node
                    .then(|| node_name(ctx, state.row_node.get(&row.key).copied()));
                key_row(row, &c, state, node, (i == sel).then_some(ticks))
            })
            .collect();
        rows(frame, body, lines, Some(sel - off));
    }
    detail_panel(frame, detail, state, ctx);
    if let Some(form) = &state.form {
        super::value::draw_form(frame, area, form);
    }
}

fn title_right(state: &KeysState, ctx: &Ctx<'_>, pattern: &str, shown: usize) -> String {
    let est = ctx
        .report()
        .and_then(|r| r.patterns.iter().find(|p| p.pattern == pattern))
        .map(|p| p.est_keys);
    let loaded = format::grouped(state.rows.len() as u64);
    let of = est.map_or_else(String::new, |n| format!(" of ~{}", format::count(n)));
    let mut right = format!("{loaded} loaded{of}");
    if !state.filter.is_empty() {
        right = format!("filter \"{}\" · {shown} rows · {right}", state.filter);
    }
    if state.by_node() {
        right = format!(
            "{right} · {} of {} nodes",
            state.nodes_done,
            state.walk.len() + state.missing.len()
        );
    }
    if state.is_loading() {
        right = format!("{} loading · {right}", spinner(ctx.tick));
    }
    format!("SCAN MATCH {} · {right}", match_glob(pattern))
}

fn header(c: &Cols, sort: KeySort) -> Line<'static> {
    let arrow = |s: KeySort| match (sort == s, s.ascending()) {
        (true, true) => "▲",
        (true, false) => "▼",
        _ => " ",
    };
    let key = format!(" KEY{}", if sort == KeySort::Name { " ▲" } else { "" });
    let mut spans = vec![Span::styled(pad_r(&key, c.key), style::hd())];
    if c.node {
        spans.push(Span::styled(pad_r("NODE", NODE_W), style::hd()));
    }
    spans.extend([
        Span::styled(pad_r("TYPE", TYPE_W + 1), style::hd()),
        Span::styled(
            format!("{}{} ", pad_l("TTL", TTL_W), arrow(KeySort::Ttl)),
            style::hd(),
        ),
        Span::styled(
            format!("{}{} ", pad_l("SIZE", SIZE_W), arrow(KeySort::Size)),
            style::hd(),
        ),
    ]);
    if c.enc {
        spans.push(Span::styled(pad_r("ENCODING", ENC_W + 1), style::hd()));
    }
    spans.push(Span::styled(
        format!("{}{} ", pad_l("LENGTH", LEN_W), arrow(KeySort::Length)),
        style::hd(),
    ));
    if c.lfu {
        spans.push(Span::styled(
            format!("{}{}", pad_l("LFU", LFU_W), arrow(KeySort::Freq)),
            style::hd(),
        ));
    }
    Line::from(spans)
}

fn ttl_text(t: Ttl) -> String {
    match t {
        Ttl::Persistent => "none".to_string(),
        Ttl::Expires(d) => format::duration(d),
    }
}

fn key_row(
    r: &KeyRow,
    c: &Cols,
    state: &KeysState,
    node: Option<String>,
    scroll: Option<u64>,
) -> Line<'static> {
    let dash = || "—".to_string();
    let text = vec![Span::styled(format!(" {}", r.key), style::fg())];
    let w = c.key.saturating_sub(1);
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(text, w, ticks),
        None => fit(text, w),
    };
    spans.push(Span::raw(" "));
    if let Some(node) = node {
        spans.push(Span::styled(
            pad_r(&super::widgets::trunc(&node, NODE_W - 1), NODE_W),
            style::dim(),
        ));
    }
    let ttl_style = match r.ttl {
        Ttl::Persistent => style::dim(),
        Ttl::Expires(_) => style::fg(),
    };
    let size = if sizes_off(state) {
        dash()
    } else {
        r.bytes.map_or_else(dash, format::bytes)
    };
    spans.extend([
        Span::styled(pad_r(r.key_type.as_str(), TYPE_W + 1), style::dim()),
        Span::styled(format!("{}  ", pad_l(&ttl_text(r.ttl), TTL_W)), ttl_style),
        Span::styled(format!("{}  ", pad_l(&size, SIZE_W)), style::fg()),
    ]);
    if c.enc {
        let enc = r.encoding.clone().unwrap_or_else(dash);
        let past = past_compact_encoding(r.key_type) == Some(enc.as_str());
        spans.push(Span::styled(
            pad_r(&super::widgets::trunc(&enc, ENC_W), ENC_W + 1),
            if past { style::amber() } else { style::dim() },
        ));
    }
    let len = r.length.map_or_else(dash, format::count);
    spans.push(Span::styled(
        format!("{}  ", pad_l(&len, LEN_W)),
        style::fg(),
    ));
    if c.lfu {
        let lfu = r.freq.map_or_else(dash, |f| f.to_string());
        spans.push(Span::styled(pad_l(&lfu, LFU_W), style::fg()));
    }
    Line::from(spans)
}

/// What the table says while it has no rows.
fn empty_lines(state: &KeysState, ctx: &Ctx<'_>) -> Vec<Line<'static>> {
    let text = if let Some(e) = &state.error {
        return vec![Line::from(Span::styled(format!(" ✗ {e}"), style::warn()))];
    } else if let LiveState::Closed(why) = ctx.live() {
        let why = why.as_deref().unwrap_or("it was closed");
        return vec![Line::from(Span::styled(
            format!(" ✗ The live connection is closed: {why}. Scan again to reopen it."),
            style::warn(),
        ))];
    } else if !state.filter.is_empty() && !state.rows.is_empty() {
        format!(
            " No loaded key matches \"{}\". Press esc to clear the filter.",
            state.filter
        )
    } else if state.done {
        " No key of this pattern was found.".to_string()
    } else if state.pages > 0 && !state.is_loading() {
        " No key of this pattern in the part scanned so far. n scans further.".to_string()
    } else if !ctx.live_ready() {
        format!(" {} Waiting for the live connection…", spinner(ctx.tick))
    } else {
        format!(" {} Loading keys with SCAN…", spinner(ctx.tick))
    };
    vec![Line::from(Span::styled(text, style::dim()))]
}

fn detail_panel(frame: &mut Frame, area: Rect, state: &KeysState, ctx: &Ctx<'_>) {
    let row = state.selected();
    let (title, right) = match row {
        Some(r) => (r.key.clone(), r.key_type.as_str().to_string()),
        None => ("Keys".to_string(), String::new()),
    };
    let inner = panel(frame, area, &title, &right);
    let w = usize::from(inner.width);
    let label = |s: &str| Span::styled(pad_r(&format!(" {s}"), 10), style::dim());
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(r) = row {
        let len = match (r.length, length_unit(r.key_type)) {
            (Some(n), Some(u)) => format!("{} {u}", format::grouped(n)),
            (Some(n), None) => format::grouped(n),
            (None, _) => "—".to_string(),
        };
        let size = if sizes_off(state) {
            "— (no MEMORY USAGE)".to_string()
        } else {
            r.bytes.map_or_else(|| "—".to_string(), format::bytes)
        };
        lines.push(Line::from(vec![
            label("ENCODING"),
            Span::styled(
                r.encoding.clone().unwrap_or_else(|| "—".into()),
                style::fgb(),
            ),
            Span::styled("   LENGTH ", style::dim()),
            Span::styled(len, style::fgb()),
        ]));
        let mut second = vec![
            label("SIZE"),
            Span::styled(size, style::fgb()),
            Span::styled("   TTL ", style::dim()),
            Span::styled(ttl_text(r.ttl), style::fgb()),
        ];
        if state.lfu {
            second.push(Span::styled("   LFU ", style::dim()));
            second.push(Span::styled(
                r.freq.map_or_else(|| "—".into(), |f| f.to_string()),
                style::fgb(),
            ));
        }
        lines.push(Line::from(second));
        let mut third = vec![label("PATTERN")];
        let p = state.pattern.clone().unwrap_or_default();
        third.extend(pattern_spans(&p, style::fg(), style::ph()));
        if state.by_node() {
            // The node in full: the column cuts it.
            third.push(Span::styled("   NODE ", style::dim()));
            third.push(Span::styled(
                node_name(ctx, state.row_node.get(&r.key).copied()),
                style::fgb(),
            ));
        }
        lines.push(Line::from(third));
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(status(state), style::dim())));
    if state.other_pattern > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                " {} keys matched the glob but infer to other patterns, so they are left out.",
                format::grouped(state.other_pattern)
            ),
            style::dim(),
        )));
    }
    if state.unreadable > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                " {} keys could not be read (refused by a key ACL, or not UTF-8).",
                format::grouped(state.unreadable)
            ),
            style::dim(),
        )));
    }
    for e in off_notes(state) {
        lines.push(Line::from(Span::styled(e, style::amber())));
    }
    for &i in &state.missing {
        if let Some(n) = ctx.session.nodes.get(i) {
            lines.push(Line::from(Span::styled(
                format!(
                    " ▲ {}",
                    crate::live::nodes::not_connected(&n.endpoint)
                        .replace("so nothing is sent to it", "so its keys are not listed")
                ),
                style::amber(),
            )));
        }
    }
    if let (Some(e), false) = (&state.error, state.rows.is_empty()) {
        lines.push(Line::from(Span::styled(format!(" ✗ {e}"), style::warn())));
    }
    let lines = lines
        .into_iter()
        .map(|l| Line::from(fit(l.spans, w)))
        .collect();
    rows(frame, inner, lines, None);
}

/// How far the list got.
fn status(state: &KeysState) -> String {
    let n = format::grouped(state.rows.len() as u64);
    if state.done {
        format!(" All {n} keys of this pattern are loaded.")
    } else if state.is_loading() {
        format!(" {n} keys loaded, loading more…")
    } else if state.rows.is_empty() && state.pages > 0 {
        " No keys found yet. n scans further.".to_string()
    } else if state.rows.is_empty() {
        " No keys loaded yet.".to_string()
    } else {
        format!(" {n} keys loaded. n loads 1,000 more.")
    }
}

/// One line per column that is off, in redis-lens's own words.
fn off_notes(state: &KeysState) -> Vec<String> {
    let mut out = Vec::new();
    if !state.sizes {
        out.push(" ▲ No sizes: MEMORY USAGE is not allowed for this user.".to_string());
    }
    if state.lfu && !state.freq {
        out.push(" ▲ No LFU counters: OBJECT FREQ is not allowed for this user.".to_string());
    }
    for e in &state.refused {
        out.push(format!(" ▲ Column off: {e}."));
    }
    out
}
