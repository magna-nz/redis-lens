//! Draws [`KeysState`]: Browse › Keys, `→` on a pattern (mockup "Browse a pattern's
//! keys"). The pattern's keys on the left, the selected key on the right, both from
//! the row the list loaded, so the right pane costs no command. `→` opens the value
//! ([`super::value`]); the key list then stays on the left.
//!
//! At 80 columns the list shows KEY, SIZE and EXPIRES; at wider widths (or with `v`)
//! TYPE, STORED AS, LENGTH and, with an LFU policy, LFU join in; on a cluster NODE as
//! well. Long names are cut with `…` (the selected one scrolls). Every name from the
//! server is escaped.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::analysis::{length_unit, match_glob, past_compact_encoding};
use crate::format;
use crate::live::LiveError;
use crate::live::keys::KeyRow;
use crate::model::Ttl;
use crate::tui::app::App;
use crate::tui::app::views::keys::{KeySort, KeysState, TypeFilter};
use crate::tui::app::views::{Ctx, LiveState};
use crate::value::escape;

use super::patterns::wrap_words;
use super::widgets::{
    Pane, Title, fit, marquee_spans, pad_l, pad_r, rows, scroll_offset, split_at, split_frame,
    trunc,
};
use super::{spinner, style};

const TYPE_W: usize = 7;
const TTL_W: usize = 8;
const SIZE_W: usize = 6;
const ENC_W: usize = 11;
const LEN_W: usize = 7;
const LFU_W: usize = 4;
/// The NODE column on a cluster: the address cut with `…` to 15 cells, and a space.
pub const NODE_W: usize = 16;
/// The narrowest the key column gets before an optional column is hidden.
const KEY_MIN: usize = 14;
/// From this inner width on, the optional columns show without `v`.
const WIDE: usize = 60;

/// The key list's width from the frame's left edge to the divider: 41 of 80 columns
/// and 72 of 120, as in the mockups; with `v` it takes more.
pub fn list_width(total: u16, more: bool) -> u16 {
    let ratio = if more {
        0.7
    } else if total >= 110 {
        0.6
    } else {
        41.0 / 80.0
    };
    split_at(total, ratio)
}

/// Column layout: the key column's width and which optional ones show.
#[derive(Debug, Clone, Copy)]
struct Cols {
    key: usize,
    ty: bool,
    enc: bool,
    len: bool,
    lfu: bool,
    /// The NODE column after the key (a cluster).
    node: bool,
}

impl Cols {
    fn fixed(&self) -> usize {
        (SIZE_W + 1)
            + (TTL_W + 1)
            + if self.node { NODE_W } else { 0 }
            + if self.ty { TYPE_W } else { 0 }
            + if self.enc { ENC_W } else { 0 }
            + if self.len { LEN_W + 1 } else { 0 }
            + if self.lfu { LFU_W + 1 } else { 0 }
    }

    fn new(inner: usize, more: bool, lfu: bool, node: bool) -> Self {
        let all = inner >= WIDE || more;
        let mut c = Self {
            key: 0,
            ty: all,
            enc: all,
            len: all,
            lfu: all && lfu,
            node: node && all,
        };
        for step in 0..5 {
            if inner >= c.fixed() + KEY_MIN {
                break;
            }
            match step {
                0 => c.enc = false,
                1 => c.lfu = false,
                2 => c.len = false,
                3 => c.ty = false,
                _ => c.node = false,
            }
        }
        c.key = inner.saturating_sub(c.fixed()).max(8);
        c
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

/// The pattern's estimated keys: the analysis's, or before one the names' estimate.
fn estimate(app: &App, pattern: &str) -> Option<(u64, bool)> {
    let d = app.pattern_data()?;
    let p = d.patterns.iter().find(|p| p.pattern == pattern)?;
    let exact = d.names && app.patterns.names.complete();
    Some((p.est_keys, exact))
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(ctx) = app.ctx() else {
        return;
    };
    let state = &app.views.keys;
    let pattern = state.pattern.clone().unwrap_or_default();
    let visible = state.visible();
    let sel = state.sel.min(visible.len().saturating_sub(1));
    let row = state.selected();
    let left_title = format!("‹ {}", escape(&pattern));
    let left_right = title_right(state, &ctx, &pattern, visible.len());
    let (right_title, right_right) = match row {
        Some(r) => (escape(&r.key), r.key_type.as_str().to_string()),
        None => ("Keys".to_string(), String::new()),
    };
    let (left, right) = split_frame(
        frame,
        area,
        list_width(area.width, ctx.more),
        Title::new(&left_title, &left_right),
        Title::new(&right_title, &right_right),
        Pane::Left,
    );
    let c = Cols::new(
        usize::from(left.width),
        ctx.more,
        state.lfu,
        state.by_node(),
    );
    let sort = state.sort.effective(state.sizes, state.freq);
    rows(frame, left, vec![header(&c, sort)], None);
    let body = Rect {
        y: left.y + 1,
        height: left.height.saturating_sub(2),
        ..left
    };
    let lw = usize::from(left.width);
    if visible.is_empty() {
        rows(frame, body, empty_lines(state, &ctx, lw), None);
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
                    .then(|| node_name(&ctx, state.row_node.get(&row.key).copied()));
                key_row(row, &c, state, node, (i == sel).then_some(ticks))
            })
            .collect();
        rows(frame, body, lines, Some(sel - off));
    }
    let bottom = Rect {
        y: left.y + left.height.saturating_sub(1),
        height: u16::from(left.height >= 2),
        ..left
    };
    let est = estimate(app, &pattern);
    rows(
        frame,
        bottom,
        vec![Line::from(fit(
            vec![Span::styled(status(state, est, ctx.tick), style::dim())],
            lw,
        ))],
        None,
    );
    detail(frame, right, state, &ctx, &pattern);
    if let Some(form) = &state.form {
        super::value::draw_form(frame, area, form);
    }
}

fn title_right(state: &KeysState, ctx: &Ctx<'_>, pattern: &str, shown: usize) -> String {
    let sort = state.sort.effective(state.sizes, state.freq);
    let mut right = format!("by {}", sort.label());
    let typed = state.type_filter != TypeFilter::All;
    if !state.filter.is_empty() {
        right = format!("filter \"{}\" · {shown} rows · {right}", state.filter);
    } else if typed {
        right = format!("{shown} rows · {right}");
    }
    if typed {
        right = format!("type {} · {right}", state.type_filter.label());
    }
    // First, so a title cut short at 80 columns still says how far the walk is.
    if state.by_node() {
        right = format!(
            "{} of {} nodes · {right}",
            state.nodes_done,
            state.walk.len() + state.missing.len()
        );
    }
    if state.is_loading() {
        right = format!("{} loading · {right}", spinner(ctx.tick));
    }
    // The command only with `v` (brief: command names leave titles).
    if ctx.more {
        right = format!("SCAN MATCH {} · {right}", match_glob(pattern));
    }
    right
}

fn header(c: &Cols, sort: KeySort) -> Line<'static> {
    let arrow = |s: KeySort| match (sort == s, s.ascending()) {
        (true, true) => "▲",
        (true, false) => "▼",
        _ => "",
    };
    let key = format!(" KEY{}", if sort == KeySort::Name { " ▲" } else { "" });
    let mut spans = vec![Span::styled(pad_r(&key, c.key), style::hd())];
    if c.node {
        spans.push(Span::styled(pad_r("NODE", NODE_W), style::hd()));
    }
    spans.extend([
        Span::styled(
            format!(
                "{} ",
                pad_l(&format!("SIZE{}", arrow(KeySort::Size)), SIZE_W)
            ),
            style::hd(),
        ),
        Span::styled(
            format!(
                "{} ",
                pad_l(&format!("EXPIRES{}", arrow(KeySort::Ttl)), TTL_W)
            ),
            style::hd(),
        ),
    ]);
    if c.ty {
        spans.push(Span::styled(pad_r("TYPE", TYPE_W), style::hd()));
    }
    if c.enc {
        spans.push(Span::styled(pad_r("STORED AS", ENC_W), style::hd()));
    }
    if c.len {
        spans.push(Span::styled(
            format!(
                "{} ",
                pad_l(&format!("LENGTH{}", arrow(KeySort::Length)), LEN_W)
            ),
            style::hd(),
        ));
    }
    if c.lfu {
        spans.push(Span::styled(
            format!("{} ", pad_l(&format!("LFU{}", arrow(KeySort::Freq)), LFU_W)),
            style::hd(),
        ));
    }
    Line::from(spans)
}

/// A TTL for a cell: `never` without one.
pub fn ttl_text(t: Ttl) -> String {
    match t {
        Ttl::Persistent => "never".to_string(),
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
    let text = vec![Span::styled(format!(" {}", escape(&r.key)), style::fg())];
    let w = c.key.saturating_sub(1);
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(text, w, ticks),
        None => fit(text, w),
    };
    let used: usize = spans
        .iter()
        .map(|s| super::widgets::width(&s.content))
        .sum();
    spans.push(Span::raw(" ".repeat(c.key.saturating_sub(used))));
    if let Some(node) = node {
        spans.push(Span::styled(
            pad_r(&trunc(&node, NODE_W - 1), NODE_W),
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
        Span::styled(format!("{} ", pad_l(&size, SIZE_W)), style::fg()),
        Span::styled(format!("{} ", pad_l(&ttl_text(r.ttl), TTL_W)), ttl_style),
    ]);
    if c.ty {
        spans.push(Span::styled(
            pad_r(r.key_type.as_str(), TYPE_W),
            style::dim(),
        ));
    }
    if c.enc {
        let enc = r.encoding.as_deref().map_or_else(dash, escape);
        let past = past_compact_encoding(r.key_type) == Some(enc.as_str());
        spans.push(Span::styled(
            pad_r(&trunc(&enc, ENC_W - 1), ENC_W),
            if past { style::amber() } else { style::dim() },
        ));
    }
    if c.len {
        let len = r.length.map_or_else(dash, format::grouped);
        spans.push(Span::styled(
            format!("{} ", pad_l(&len, LEN_W)),
            style::fg(),
        ));
    }
    if c.lfu {
        let lfu = r.freq.map_or_else(dash, |f| f.to_string());
        spans.push(Span::styled(
            format!("{} ", pad_l(&lfu, LFU_W)),
            style::fg(),
        ));
    }
    Line::from(spans)
}

/// What the list says while it has no rows.
fn empty_lines(state: &KeysState, ctx: &Ctx<'_>, w: usize) -> Vec<Line<'static>> {
    let (text, st) = if let Some(e) = &state.error {
        (format!(" ✗ {e}"), style::warn())
    } else if let LiveState::Closed(why) = ctx.live() {
        let why = why.as_deref().unwrap_or("it was closed");
        (
            format!(" ✗ The live connection is closed: {why}. Connect again to reopen it."),
            style::warn(),
        )
    } else if !state.filter.is_empty() && !state.rows.is_empty() {
        (
            format!(
                " No loaded key matches \"{}\". esc clears the filter.",
                state.filter
            ),
            style::dim(),
        )
    } else if state.type_filter != TypeFilter::All && !state.rows.is_empty() {
        (
            format!(
                " No loaded key is of type {}. t for another type, esc to clear.",
                state.type_filter.label()
            ),
            style::dim(),
        )
    } else if state.done {
        (
            " No key of this pattern was found.".to_string(),
            style::dim(),
        )
    } else if state.pages > 0 && !state.is_loading() {
        (
            " No key of this pattern in the part read so far. n reads further.".to_string(),
            style::dim(),
        )
    } else if !ctx.live_ready() {
        (
            format!(" {} Waiting for the live connection…", spinner(ctx.tick)),
            style::dim(),
        )
    } else {
        (
            format!(" {} Loading keys…", spinner(ctx.tick)),
            style::dim(),
        )
    };
    wrap_words(&text, w)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, st)))
        .collect()
}

/// The list's bottom line: how far it got, and `n`.
fn status(state: &KeysState, est: Option<(u64, bool)>, tick: u64) -> String {
    let n = format::grouped(state.rows.len() as u64);
    let of = match est {
        Some((e, true)) => format!(" of {}", format::grouped(e)),
        Some((e, false)) => format!(" of ~{}", format::count(e)),
        None => String::new(),
    };
    if state.done {
        format!(" All {n} keys loaded.")
    } else if state.is_loading() {
        format!(" {} {n}{of} loaded, loading more…", spinner(tick))
    } else if state.rows.is_empty() && state.pages > 0 {
        " No keys found yet. n reads further.".to_string()
    } else if state.rows.is_empty() {
        " No keys loaded yet.".to_string()
    } else {
        format!(" {n}{of} loaded. n loads 1,000 more.")
    }
}

/// The right pane: the selected key's length, size and expiry, why it is marked, and
/// what the list left out or could not read.
fn detail(frame: &mut Frame, inner: Rect, state: &KeysState, ctx: &Ctx<'_>, pattern: &str) {
    let w = usize::from(inner.width);
    let wide = w >= 40;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let para = |lines: &mut Vec<Line<'static>>, text: &str, st| {
        lines.extend(
            wrap_words(text, w)
                .into_iter()
                .map(|l| Line::from(Span::styled(l, st))),
        );
    };
    if let Some(r) = state.selected() {
        let mut facts: Vec<String> = Vec::new();
        if let Some(n) = r.length {
            facts.push(match length_unit(r.key_type) {
                Some(u) => format!("{} {u}", format::grouped(n)),
                None => format::grouped(n),
            });
        }
        if !sizes_off(state)
            && let Some(b) = r.bytes
        {
            facts.push(format::bytes(b));
        }
        let expires = match r.ttl {
            Ttl::Persistent => "never expires".to_string(),
            Ttl::Expires(d) => format!("expires in {}", format::duration(d)),
        };
        if wide {
            facts.push(expires);
            lines.push(Line::from(fit(
                vec![Span::styled(
                    format!(" {}", facts.join(" · ")),
                    style::fgb(),
                )],
                w,
            )));
        } else {
            if !facts.is_empty() {
                lines.push(Line::from(fit(
                    vec![Span::styled(
                        format!(" {}", facts.join(" · ")),
                        style::fgb(),
                    )],
                    w,
                )));
            }
            lines.push(Line::from(Span::styled(format!(" {expires}"), style::fg())));
        }
        if state.by_node() {
            lines.push(Line::from(fit(
                vec![Span::styled(
                    format!(
                        " on node {}",
                        node_name(ctx, state.row_node.get(&r.key).copied())
                    ),
                    style::dim(),
                )],
                w,
            )));
        }
        let enc = r.encoding.as_deref().unwrap_or("");
        if let Some(past) = past_compact_encoding(r.key_type).filter(|p| *p == enc) {
            let per = match length_unit(r.key_type) {
                Some("fields") => "field",
                _ => "member",
            };
            lines.push(Line::default());
            para(
                &mut lines,
                &format!(
                    " ▲ Stored as a {past}: past the compact size, so it takes more memory per {per}."
                ),
                style::amber(),
            );
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(" → shows the value", style::dim())));
    }
    let mut notes: Vec<(String, ratatui::style::Style)> = Vec::new();
    if state.other_pattern > 0 {
        notes.push((
            format!(
                " {} {} matched {} but belong to other patterns, so they are not listed.",
                format::grouped(state.other_pattern),
                if state.other_pattern == 1 {
                    "key"
                } else {
                    "keys"
                },
                match_glob(pattern)
            ),
            style::dim(),
        ));
    }
    if state.unreadable > 0 {
        notes.push((
            format!(
                " {} keys could not be read (refused by a key ACL, or not UTF-8).",
                format::grouped(state.unreadable)
            ),
            style::dim(),
        ));
    }
    for e in off_notes(state) {
        notes.push((e, style::amber()));
    }
    for &i in &state.missing {
        if let Some(n) = ctx.session.nodes.get(i) {
            notes.push((
                format!(
                    " ▲ {}",
                    crate::live::nodes::not_connected(&n.endpoint)
                        .replace("so nothing is sent to it", "so its keys are not listed")
                ),
                style::amber(),
            ));
        }
    }
    if let (Some(e), false) = (&state.error, state.rows.is_empty()) {
        notes.push((format!(" ✗ {e}"), style::warn()));
    }
    for (text, st) in notes {
        lines.push(Line::default());
        para(&mut lines, &text, st);
    }
    rows(frame, inner, lines, None);
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
