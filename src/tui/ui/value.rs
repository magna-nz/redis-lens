//! Draws [`ValueState`]: the key detail (spec "Key detail"). The key's metadata on the
//! left; on the right its items (fields, elements, members or entries) above a divider
//! and the selected item's value below it, or for a string or JSON document just the
//! value.
//!
//! Every text that came from the server goes through [`value::escape`] or the value
//! renderer before it is drawn: values, field names, members, entry IDs, the type name
//! and the key name in the title. Rows too wide are cut with `…`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use crate::format;
use crate::live::values::{Items, JSON_MAX_SIZE, JSON_SHOWN, Kind, MAX_ITEMS, STRING_BYTES};
use crate::model::Ttl;
use crate::tui::app::views::value::{Focus, ValueState};
use crate::tui::app::views::{Ctx, LiveState};
use crate::value::{self, Raw};
use crate::verdict::{PatternVerdict, pattern_verdict};

use super::widgets::{fit, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset, trunc, width};
use super::{spinner, style};

/// The metadata labels' column.
const LABEL_W: usize = 13;
const TTL_W: usize = 8;
const SCORE_W: usize = 12;

pub fn draw(frame: &mut Frame, area: Rect, state: &ValueState, ctx: &Ctx<'_>) {
    let w = usize::from(area.width);
    let left_w = (w * 38 / 100).clamp(20, 42).min(w);
    let [left, right] = Layout::horizontal([
        Constraint::Length(u16::try_from(left_w).unwrap_or(u16::MAX)),
        Constraint::Min(0),
    ])
    .areas(area);
    meta(frame, left, state, ctx);
    value_panel(frame, right, state, ctx);
}

/// A server string or name, safe to draw on one line: invalid UTF-8 as `\xNN`.
pub fn name_text(raw: &[u8]) -> String {
    let mut out = String::new();
    for chunk in raw.utf8_chunks() {
        out.push_str(&value::escape(chunk.valid()));
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

fn ttl_text(t: Ttl) -> String {
    match t {
        Ttl::Persistent => "none".to_string(),
        Ttl::Expires(d) => format::duration(d),
    }
}

fn length_label(kind: Option<&Kind>) -> &'static str {
    match kind {
        Some(Kind::Hash) => "Fields",
        Some(Kind::List) => "Items",
        Some(Kind::Set | Kind::ZSet) => "Members",
        Some(Kind::Stream) => "Entries",
        Some(Kind::Vector) => "Elements",
        _ => "Length",
    }
}

fn verdict_style(v: PatternVerdict) -> ratatui::style::Style {
    match v {
        PatternVerdict::Leak | PatternVerdict::Growing | PatternVerdict::Mixed => style::warn(),
        PatternVerdict::BigKey | PatternVerdict::Persistent => style::amber(),
        PatternVerdict::Ok => style::ok(),
    }
}

fn meta(frame: &mut Frame, area: Rect, state: &ValueState, ctx: &Ctx<'_>) {
    let Some(row) = &state.row else {
        panel(frame, area, "Key", "");
        return;
    };
    let inner = panel(frame, area, &value::escape(&row.key), "");
    let w = usize::from(inner.width);
    let kind = state.kind();
    let label = |s: &str| Span::styled(pad_r(&format!(" {s}"), LABEL_W), style::dim());
    let dash = || "—".to_string();
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let type_text = state
        .type_name
        .as_deref()
        .map_or_else(|| row.key_type.as_str().to_string(), value::escape);
    lines.push(vec![label("Type"), Span::styled(type_text, style::fgb())]);
    lines.push(vec![
        label("Encoding"),
        Span::styled(
            row.encoding.as_deref().map_or_else(dash, value::escape),
            style::fg(),
        ),
    ]);
    let strlen = match &state.items {
        Some(Items::String { len, .. }) => *len,
        _ => None,
    };
    let length = match (row.length.or(strlen), &kind) {
        (Some(n), Some(Kind::String)) => format!("{} bytes", format::grouped(n)),
        (Some(n), _) => format::grouped(n),
        (None, _) => dash(),
    };
    lines.push(vec![
        label(length_label(kind.as_ref())),
        Span::styled(length, style::fg()),
    ]);
    lines.push(vec![
        label("Size"),
        Span::styled(row.bytes.map_or_else(dash, format::bytes), style::fg()),
    ]);
    lines.push(vec![
        label("TTL"),
        Span::styled(ttl_text(row.ttl), style::fg()),
    ]);
    if let (Some(Kind::Hash), true, Some(Items::Hash(fields))) =
        (&kind, state.field_ttls, &state.items)
    {
        let with = fields
            .iter()
            .filter(|f| matches!(f.ttl, Some(Ttl::Expires(_))))
            .count();
        let of = format!(
            "{} of {} loaded",
            format::grouped(with as u64),
            format::grouped(fields.len() as u64)
        );
        let text = if LABEL_W + width(&of) + 7 <= w {
            format!("{of} fields")
        } else {
            of
        };
        lines.push(vec![label("Field TTL"), Span::styled(text, style::fg())]);
    }
    if ctx.server().is_some_and(|s| s.lfu_enabled()) {
        lines.push(vec![
            label("LFU"),
            Span::styled(row.freq.map_or_else(dash, |f| f.to_string()), style::fg()),
        ]);
    }
    let mut p = vec![label("Pattern")];
    p.extend(pattern_spans(
        &value::escape(&state.pattern),
        style::fg(),
        style::ph(),
    ));
    lines.push(p);
    let verdict = ctx
        .report()
        .and_then(|r| r.patterns.iter().find(|p| p.pattern == state.pattern))
        .map(|p| {
            let diff = ctx.session.diff().and_then(|d| d.pattern(&p.pattern));
            pattern_verdict(p, diff)
        });
    lines.push(vec![
        label("Verdict"),
        match verdict {
            Some(v) => Span::styled(v.label(), verdict_style(v)),
            None => Span::styled("—", style::dim()),
        },
    ]);
    let mut notes: Vec<String> = state.refused.iter().map(|e| format!(" ▲ {e}.")).collect();
    if let (Some(Kind::Hash), Some(why)) = (&kind, &state.httl_off) {
        notes.push(format!(" ▲ {why}"));
    }
    if !notes.is_empty() {
        lines.push(Vec::new());
    }
    let mut out: Vec<Line<'static>> = lines.into_iter().map(|l| Line::from(fit(l, w))).collect();
    for n in notes {
        out.extend(
            wrap(&n, w)
                .into_iter()
                .map(|l| Line::from(fit(vec![Span::styled(l, style::amber())], w))),
        );
    }
    rows(frame, inner, out, None);
}

/// `text` in lines at most `w` cells wide, broken at spaces. Its leading spaces start
/// the first line; the others are indented two more, past a `▲` or `✗`.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let body = text.trim_start_matches(' ');
    let indent = " ".repeat(text.len() - body.len());
    let more = format!("{indent}  ");
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in body.split(' ') {
        let lead = if out.is_empty() { &indent } else { &more };
        let candidate = if line.is_empty() {
            format!("{lead}{word}")
        } else {
            format!("{line} {word}")
        };
        if width(&candidate) > w && !line.is_empty() {
            out.push(line);
            line = format!("{more}{word}");
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

fn panel_title(kind: Option<&Kind>) -> &'static str {
    match kind {
        Some(Kind::Hash) => "Fields",
        Some(Kind::List) => "Items",
        Some(Kind::Set | Kind::ZSet) => "Members",
        Some(Kind::Stream) => "Entries",
        Some(Kind::Json) => "Document",
        Some(Kind::Vector) => "Elements",
        _ => "Value",
    }
}

fn title_right(state: &ValueState, ctx: &Ctx<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if state.is_loading() {
        parts.push(format!("{} loading", spinner(ctx.tick)));
    }
    if let Some(raw) = state.format_source() {
        let d = value::decode(raw.as_bytes());
        parts.push(state.shown_format(&d).label().to_string());
    }
    match &state.items {
        Some(Items::String { len, .. }) => {
            if let Some(n) = len.filter(|n| *n > STRING_BYTES as u64) {
                parts.push(format!("first 64 KiB of {}", format::bytes(n)));
            }
        }
        Some(Items::Json { .. } | Items::JsonNotRead { .. } | Items::NotRead) | None => {}
        Some(items) => {
            let n = format::grouped(items.len() as u64);
            let of = state.row.as_ref().and_then(|r| r.length).map_or_else(
                || format!("{n} loaded"),
                |m| format!("{n} of {}", format::grouped(m)),
            );
            parts.push(of);
        }
    }
    if let Some(cmd) = state.kind().and_then(|k| k.command()) {
        parts.push(cmd.to_string());
    }
    parts.join(" · ")
}

fn value_panel(frame: &mut Frame, area: Rect, state: &ValueState, ctx: &Ctx<'_>) {
    let kind = state.kind();
    let inner = panel(
        frame,
        area,
        panel_title(kind.as_ref()),
        &title_right(state, ctx),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let iw = usize::from(inner.width);
    let msg = |text: String, st| -> Vec<Line<'static>> {
        wrap(&text, iw)
            .into_iter()
            .map(|l| Line::from(fit(vec![Span::styled(l, st)], iw)))
            .collect()
    };
    let Some(items) = &state.items else {
        rows(frame, inner, msg_lines(state, ctx, iw), None);
        return;
    };
    match items {
        Items::NotRead => {
            let t = state
                .type_name
                .as_deref()
                .map_or_else(String::new, value::escape);
            rows(
                frame,
                inner,
                msg(
                    format!(" redis-lens does not read values of type {t}."),
                    style::dim(),
                ),
                None,
            );
        }
        Items::JsonNotRead { .. } => {
            let max = format::bytes(JSON_MAX_SIZE);
            let text = match state.row.as_ref().and_then(|r| r.bytes) {
                Some(s) => format!(
                    " The document is {}, over the {max} redis-lens reads, so it is not read.",
                    format::bytes(s)
                ),
                None => format!(
                    " The document's size is unknown (no MEMORY USAGE), so it is not read: \
                     redis-lens reads documents of at most {max}."
                ),
            };
            rows(frame, inner, msg(text, style::amber()), None);
        }
        Items::String { value, .. } => {
            value_pane(frame, inner, state, "value".into(), value, Vec::new());
        }
        Items::Json { doc, len } => {
            let notes = if *len > JSON_SHOWN {
                vec![format!(
                    "cut at {} of {}",
                    format::bytes(JSON_SHOWN as u64),
                    format::bytes(*len as u64)
                )]
            } else {
                Vec::new()
            };
            value_pane(frame, inner, state, "document".into(), doc, notes);
        }
        _ => collection(frame, inner, state, items),
    }
}

/// What the pane says before anything is read.
fn msg_lines(state: &ValueState, ctx: &Ctx<'_>, w: usize) -> Vec<Line<'static>> {
    let (text, st) = if let Some(e) = &state.error {
        (format!(" ✗ {e}"), style::warn())
    } else if state.gone {
        (" The key no longer exists.".to_string(), style::amber())
    } else if let LiveState::Closed(why) = ctx.live() {
        let why = why.as_deref().unwrap_or("it was closed");
        (
            format!(" ✗ The live connection is closed: {why}. Scan again to reopen it."),
            style::warn(),
        )
    } else if !ctx.live_ready() {
        (
            format!(" {} Waiting for the live connection…", spinner(ctx.tick)),
            style::dim(),
        )
    } else {
        (
            format!(" {} Reading the value…", spinner(ctx.tick)),
            style::dim(),
        )
    };
    wrap(&text, w)
        .into_iter()
        .map(|l| Line::from(fit(vec![Span::styled(l, st)], w)))
        .collect()
}

/// The header of a value: its label, format, size and what was done to it.
fn header(
    label: String,
    raw: &Raw,
    state: &ValueState,
    r: &value::Rendered,
    d: &value::Decoded,
) -> Vec<String> {
    let mut parts = vec![label];
    let n = format!("{} bytes", format::grouped(raw.len() as u64));
    match state.format {
        None => {
            parts.push(r.format.label().to_string());
            parts.push(n);
            parts.push("auto-detected".to_string());
        }
        Some(f) => {
            parts.push(n);
            parts.push(format!("shown as {}", f.label()));
        }
    }
    if let Some(c) = d.compression {
        parts.push(format!("decompressed from {}", c.label()));
    }
    if d.truncated {
        parts.push("truncated at 1 MiB".to_string());
    }
    if let Some(note) = r.note {
        parts.push(note.to_string());
    }
    parts
}

/// One value: a header line, then the rendered lines from `state.scroll`.
fn value_pane(
    frame: &mut Frame,
    area: Rect,
    state: &ValueState,
    label: String,
    raw: &Raw,
    notes: Vec<String>,
) {
    let w = usize::from(area.width);
    let d = value::decode(raw.as_bytes());
    let r = value::render(&d, state.shown_format(&d), w.max(1));
    let mut head = header(label, raw, state, &r, &d);
    head.extend(notes);
    let body: Vec<String> = r.lines().to_vec();
    scrolled(frame, area, state, head.join(" · "), body);
}

/// A header line, then `body` scrolled to `state.scroll`, which the draw records.
fn scrolled(frame: &mut Frame, area: Rect, state: &ValueState, head: String, body: Vec<String>) {
    let w = usize::from(area.width);
    let height = usize::from(area.height).saturating_sub(1);
    state.drawn.set(body.len(), height);
    let max = body.len().saturating_sub(height);
    let off = state.scroll.min(max);
    let head_style = if state.focus == Focus::Value || !state.has_list() {
        style::title()
    } else {
        style::hd()
    };
    let mut lines = vec![Line::from(fit(
        vec![Span::styled(format!(" {head}"), head_style)],
        w,
    ))];
    lines.extend(
        body.into_iter()
            .skip(off)
            .take(height)
            .map(|l| Line::from(fit(vec![Span::styled(format!(" {l}"), style::fg())], w))),
    );
    rows(frame, area, lines, None);
}

/// A collection: its items, a divider, and the selected item's value.
fn collection(frame: &mut Frame, area: Rect, state: &ValueState, items: &Items) {
    let h = usize::from(area.height);
    let iw = usize::from(area.width);
    let list_h = (h * 2 / 5).max(3).min(h);
    let value_h = h.saturating_sub(list_h + 1);
    let list = Rect {
        height: u16::try_from(list_h).unwrap_or(0),
        ..area
    };
    let divider = Rect {
        y: area.y + list.height,
        height: u16::from(h > list_h),
        ..area
    };
    let pane = Rect {
        y: divider.y + divider.height,
        height: u16::try_from(value_h).unwrap_or(0),
        ..area
    };
    let sel = state.selected();
    let n = items.len();
    let body_h = list_h.saturating_sub(1);
    let off = scroll_offset(sel, body_h);
    let (head, cells) = list_rows(state, items, iw, off, body_h);
    let mut lines = vec![Line::from(fit(head, iw))];
    if n == 0 {
        lines.push(Line::from(Span::styled(" No items.", style::dim())));
    }
    for (i, cell) in cells.into_iter().enumerate() {
        let mark = if off + i == sel { "›" } else { " " };
        let mut spans = vec![Span::styled(mark, style::amber())];
        spans.extend(cell);
        lines.push(Line::from(fit(spans, iw)));
    }
    let selected = (state.focus == Focus::Items && n > 0).then(|| sel - off + 1);
    rows(frame, list, lines, selected);
    rows(frame, divider, vec![divider_line(state, iw)], None);
    if n == 0 || pane.height == 0 {
        return;
    }
    match items {
        Items::Stream(entries) => {
            let e = &entries[sel];
            let w = iw.saturating_sub(2).max(1);
            let mut body = Vec::new();
            for (name, v) in &e.fields {
                let d = value::decode(v.as_bytes());
                let r = value::render(&d, state.shown_format(&d), w);
                body.push(name_text(name.as_bytes()));
                body.extend(r.lines().iter().map(|l| format!("  {l}")));
            }
            let how = match state.format {
                Some(f) => format!("shown as {}", f.label()),
                None => "auto-detected".to_string(),
            };
            let head = format!(
                "{} · {} fields · {how}",
                value::escape(&e.id),
                format::grouped(e.fields.len() as u64)
            );
            scrolled(frame, pane, state, head, body);
        }
        _ => {
            let Some(raw) = state.selected_value() else {
                return;
            };
            let label = match items {
                Items::Hash(f) => name_text(f[sel].name.as_bytes()),
                Items::List(_) => format!("#{sel}"),
                Items::ZSet(m) => format!(
                    "member · score {}",
                    value::preview(m[sel].score.as_bytes(), 24)
                ),
                Items::Vector(_) => "element".to_string(),
                _ => "member".to_string(),
            };
            value_pane(frame, pane, state, label, raw, Vec::new());
        }
    }
}

/// The divider between the list and the value, with what the list can't say itself.
fn divider_line(state: &ValueState, w: usize) -> Line<'static> {
    let note = if let Some(e) = &state.error {
        Some((format!("✗ {e}"), style::warn()))
    } else if state.capped {
        Some((
            format!(
                "at most {} items are held",
                format::grouped(MAX_ITEMS as u64)
            ),
            style::amber(),
        ))
    } else if matches!(state.items, Some(Items::Vector(_))) {
        Some((
            "vector sets can't be read in pages: these are random elements".to_string(),
            style::dim(),
        ))
    } else if !state.done() && !state.is_loading() && state.pages > 0 {
        Some(("n loads more".to_string(), style::dim()))
    } else {
        None
    };
    match note {
        None => Line::from(Span::styled("─".repeat(w), style::border())),
        Some((text, st)) => {
            let text = trunc(&text, w.saturating_sub(6));
            let rest = w.saturating_sub(width(&text) + 4);
            Line::from(vec![
                Span::styled("── ", style::border()),
                Span::styled(text, st),
                Span::styled(format!(" {}", "─".repeat(rest)), style::border()),
            ])
        }
    }
}

/// The list's header and the cells of rows `off..off + n`, each after the one-cell
/// selection mark.
fn list_rows(
    state: &ValueState,
    items: &Items,
    iw: usize,
    off: usize,
    n: usize,
) -> (Vec<Span<'static>>, Vec<Vec<Span<'static>>>) {
    // The selection mark, a space, and a space before the right border.
    let w = iw.saturating_sub(3);
    let hd = |s: &str, cw: usize| Span::styled(pad_r(s, cw), style::hd());
    let hd_r = |s: &str, cw: usize| Span::styled(pad_l(s, cw), style::hd());
    let cell = |s: String, cw: usize| Span::styled(pad_r(&s, cw), style::fg());
    let sp = || Span::raw(" ");
    match items {
        Items::Hash(fields) => {
            let ttl = state.field_ttls;
            let ttl_w = if ttl { TTL_W + 1 } else { 0 };
            let fw = (w * 3 / 10).clamp(6, 28);
            let vw = w.saturating_sub(fw + 1 + ttl_w);
            let mut head = vec![sp(), sp(), hd("FIELD", fw), sp(), hd("VALUE", vw)];
            if ttl {
                head.extend([sp(), hd_r("TTL", TTL_W)]);
            }
            let rows = fields
                .iter()
                .skip(off)
                .take(n)
                .map(|f| {
                    let mut c = vec![
                        sp(),
                        cell(trunc(&name_text(f.name.as_bytes()), fw), fw),
                        sp(),
                        cell(value::preview(f.value.as_bytes(), vw), vw),
                    ];
                    if ttl {
                        let t = match f.ttl {
                            Some(Ttl::Expires(d)) => format::duration(d),
                            _ => "—".to_string(),
                        };
                        c.extend([sp(), Span::styled(pad_l(&t, TTL_W), style::fg())]);
                    }
                    c
                })
                .collect();
            (head, rows)
        }
        Items::List(v) => {
            let iw_ = format!("{}", v.len().saturating_sub(1)).len().max(5);
            let ew = w.saturating_sub(iw_ + 1);
            let head = vec![sp(), sp(), hd_r("INDEX", iw_), sp(), hd("ELEMENT", ew)];
            let rows = v
                .iter()
                .enumerate()
                .skip(off)
                .take(n)
                .map(|(i, e)| {
                    vec![
                        sp(),
                        Span::styled(pad_l(&i.to_string(), iw_), style::dim()),
                        sp(),
                        cell(value::preview(e.as_bytes(), ew), ew),
                    ]
                })
                .collect();
            (head, rows)
        }
        Items::Set(v) | Items::Vector(v) => {
            let title = if matches!(items, Items::Set(_)) {
                "MEMBER"
            } else {
                "ELEMENT"
            };
            let head = vec![sp(), sp(), hd(title, w)];
            let rows = v
                .iter()
                .skip(off)
                .take(n)
                .map(|m| vec![sp(), cell(value::preview(m.as_bytes(), w), w)])
                .collect();
            (head, rows)
        }
        Items::ZSet(v) => {
            let mw = w.saturating_sub(SCORE_W + 1);
            let head = vec![sp(), sp(), hd("MEMBER", mw), sp(), hd_r("SCORE", SCORE_W)];
            let rows = v
                .iter()
                .skip(off)
                .take(n)
                .map(|m| {
                    vec![
                        sp(),
                        cell(value::preview(m.member.as_bytes(), mw), mw),
                        sp(),
                        Span::styled(
                            pad_l(&value::preview(m.score.as_bytes(), SCORE_W), SCORE_W),
                            style::fg(),
                        ),
                    ]
                })
                .collect();
            (head, rows)
        }
        Items::Stream(v) => {
            let idw = v
                .iter()
                .map(|e| width(&e.id))
                .max()
                .unwrap_or(0)
                .clamp(8, 24);
            let fw = w.saturating_sub(idw + 1);
            let head = vec![sp(), sp(), hd("ENTRY ID", idw), sp(), hd("FIELDS", fw)];
            let rows = v
                .iter()
                .skip(off)
                .take(n)
                .map(|e| {
                    let fields: Vec<String> = e
                        .fields
                        .iter()
                        .map(|(k, v)| {
                            format!(
                                "{}={}",
                                name_text(k.as_bytes()),
                                value::preview(v.as_bytes(), fw)
                            )
                        })
                        .collect();
                    vec![
                        sp(),
                        Span::styled(pad_r(&value::escape(&e.id), idw), style::dim()),
                        sp(),
                        cell(trunc(&fields.join(" "), fw), fw),
                    ]
                })
                .collect();
            (head, rows)
        }
        _ => (Vec::new(), Vec::new()),
    }
}
