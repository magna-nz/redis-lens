//! Draws [`ValueState`]: the key detail (spec "Key detail"). The key's metadata on the
//! left; on the right its items (fields, elements, members or entries) above a divider
//! and the selected item's value below it, or for a string or JSON document just the
//! value.
//!
//! Every text that came from the server goes through [`value::escape`] or the value
//! renderer before it is drawn: values, field names, members, entry IDs, the type name
//! and the key name in the title. Rows too wide are cut with `…`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Clear;

use crate::format;
use crate::live::values::{Items, JSON_MAX_SIZE, JSON_SHOWN, Kind, MAX_ITEMS, STRING_BYTES};
use crate::model::Ttl;
use crate::tui::app::App;
use crate::tui::app::views::value::{Focus, Form, ValueState};
use crate::tui::app::views::{Ctx, LiveState};
use crate::value::{self, Raw};

use super::widgets::{
    Pane, Title, fit, pad_l, pad_r, panel, rows, scroll_offset, split_frame, trunc, width,
};
use super::{spinner, style};

const TTL_W: usize = 8;
const SCORE_W: usize = 12;

/// Browse › Keys, `→` on a key: the key list on the left (dimmed: the keys are on the
/// right), the key's value on the right (mockup "A key's value"). `←` goes back to the
/// list; `→` moves into the value below the divider to scroll it.
pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(ctx) = app.ctx() else {
        return;
    };
    let state = &app.views.value;
    let keys = &app.views.keys;
    let left_title = format!("‹ {}", value::escape(keys.pattern.as_deref().unwrap_or("")));
    let (title, right) = match &state.row {
        Some(r) => (value::escape(&r.key), title_right(state, &ctx)),
        None => ("Key".to_string(), String::new()),
    };
    let (left, inner) = split_frame(
        frame,
        area,
        list_width(area.width),
        Title::new(&left_title, ""),
        Title::new(&title, &right),
        Pane::Right,
    );
    key_column(frame, left, app);
    let w = usize::from(inner.width);
    let mut head: Vec<Line<'static>> = Vec::new();
    if state.row.is_some() {
        head.push(Line::from(fit(facts(state, &ctx, w), w)));
        let mut notes: Vec<String> = state.refused.iter().map(|e| format!(" ▲ {e}.")).collect();
        if let (Some(Kind::Hash), Some(why)) = (&state.kind(), &state.httl_off) {
            notes.push(format!(" ▲ {why}"));
        }
        for n in notes {
            head.extend(
                wrap(&n, w)
                    .into_iter()
                    .map(|l| Line::from(fit(vec![Span::styled(l, style::amber())], w))),
            );
        }
        head.push(Line::default());
    }
    let used = u16::try_from(head.len())
        .unwrap_or(inner.height)
        .min(inner.height);
    rows(frame, inner, head, None);
    let body = Rect {
        y: inner.y + used,
        height: inner.height - used,
        ..inner
    };
    draw_value_body(frame, body, state, &ctx);
    if let Some(form) = &state.form {
        draw_form(frame, area, form);
    }
}

/// The key list's width on a key's value: 28 of 80 columns and 40 of 120.
pub(super) fn list_width(total: u16) -> u16 {
    let ratio = if total >= 110 { 1.0 / 3.0 } else { 0.35 };
    super::widgets::split_at(total, ratio)
}

/// The key list beside a value: its keys, the open one bright, the others dim; SIZE
/// and EXPIRES when there is room.
fn key_column(frame: &mut Frame, area: Rect, app: &App) {
    let keys = &app.views.keys;
    let open = app.views.value.row.as_ref().map(|r| r.key.as_str());
    let w = usize::from(area.width);
    let wide = w >= 34;
    let kw = if wide { w.saturating_sub(15) } else { w };
    let mut head = vec![Span::styled(pad_r(" KEY", kw), style::hd())];
    if wide {
        head.push(Span::styled(format!("{} ", pad_l("SIZE", 6)), style::hd()));
        head.push(Span::styled(pad_l("EXPIRES", 7), style::hd()));
    }
    rows(frame, area, vec![Line::from(head)], None);
    let body = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    let visible = keys.visible();
    let sel = keys.sel.min(visible.len().saturating_sub(1));
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = visible
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .map(|&i| {
            let r = &keys.rows[i];
            let st = if Some(r.key.as_str()) == open {
                style::fgb()
            } else {
                style::dim()
            };
            let mut spans = vec![Span::styled(
                pad_r(&format!(" {}", value::escape(&r.key)), kw),
                st,
            )];
            if wide {
                let size = r.bytes.map_or_else(|| "—".to_string(), format::bytes);
                spans.push(Span::styled(format!("{} ", pad_l(&size, 6)), st));
                spans.push(Span::styled(pad_l(&super::keys::ttl_text(r.ttl), 7), st));
            }
            Line::from(fit(spans, w))
        })
        .collect();
    rows(frame, body, lines, None);
}

/// The one line of facts above a value: its length, size and expiry; at wider widths
/// the encoding, the LFU counter, field TTLs and, on a cluster, the node too.
fn facts(state: &ValueState, ctx: &Ctx<'_>, w: usize) -> Vec<Span<'static>> {
    let Some(row) = &state.row else {
        return Vec::new();
    };
    let kind = state.kind();
    let strlen = match &state.items {
        Some(Items::String { len, .. }) => *len,
        _ => None,
    };
    let mut parts: Vec<String> = Vec::new();
    match (row.length.or(strlen), &kind) {
        (Some(n), Some(Kind::String)) => parts.push(format!("{} bytes", format::grouped(n))),
        (Some(n), k) => parts.push(format!(
            "{} {}",
            format::grouped(n),
            length_label(k.as_ref(), n)
        )),
        (None, _) => {}
    }
    if let Some(b) = row.bytes {
        parts.push(format::bytes(b));
    }
    parts.push(match row.ttl {
        Ttl::Persistent => "never expires".to_string(),
        Ttl::Expires(d) => format!("expires in {}", format::duration(d)),
    });
    let first = format!(" {}", parts.join(" · "));
    // What joins the line when there is room, the node first: on a cluster, where
    // the key is.
    let mut more: Vec<String> = Vec::new();
    if let Some(n) = ctx
        .session
        .owner_of(row.key.as_bytes())
        .and_then(|i| ctx.session.nodes.get(i))
    {
        more.push(format!("on node {}", n.endpoint));
    }
    // A hash's field TTLs (Redis 7.4 and later), of the fields loaded.
    if let (Some(Kind::Hash), true, Some(Items::Hash(fields))) =
        (&kind, state.field_ttls, &state.items)
    {
        let with = fields
            .iter()
            .filter(|f| matches!(f.ttl, Some(Ttl::Expires(_))))
            .count();
        more.push(format!(
            "{} of {} fields expire",
            format::grouped(with as u64),
            format::grouped(fields.len() as u64)
        ));
    }
    if let Some(e) = &row.encoding {
        more.push(format!("stored as {}", value::escape(e)));
    }
    if ctx.server().is_some_and(|s| s.lfu_enabled()) {
        more.push(format!(
            "LFU {}",
            row.freq.map_or_else(|| "—".to_string(), |f| f.to_string())
        ));
    }
    let mut spans = vec![Span::styled(first.clone(), style::fgb())];
    let mut rest = String::new();
    for m in more {
        let next = if rest.is_empty() {
            m
        } else {
            format!("{rest} · {m}")
        };
        if width(&first) + 3 + width(&next) >= w {
            break;
        }
        rest = next;
    }
    if !rest.is_empty() {
        spans.push(Span::styled(format!("   {rest}"), style::dim()));
    }
    spans
}

/// Widest an edit form gets.
const FORM_W: u16 = 72;

/// An edit form ([`Form`]) in a box over `area`, near its bottom: each field with its
/// label, the one typed in marked with `›` and a cursor; then the notes, the last
/// error and the keys. Every text is escaped; a long one shows its end while typed in,
/// its start otherwise.
pub fn draw_form(frame: &mut Frame, area: Rect, form: &Form) {
    let w = area.width.min(FORM_W);
    let inner = usize::from(w.saturating_sub(2));
    let label_w = form
        .fields
        .iter()
        .map(|f| width(f.label))
        .max()
        .unwrap_or(0)
        + 1;
    // " label › text▏ "
    let room = inner.saturating_sub(label_w + 5).max(4);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, f) in form.fields.iter().enumerate() {
        let active = i == form.at;
        let shown = f.shown();
        let text = if f.is_choice() && active {
            format!("‹ {shown} ›")
        } else if active && width(&shown) > room {
            let chars: Vec<char> = shown.chars().collect();
            let mut keep = String::from("…");
            let mut start = chars.len();
            while start > 0 && width(&keep) + super::widgets::char_width(chars[start - 1]) < room {
                start -= 1;
                keep.insert(1, chars[start]);
            }
            keep
        } else {
            trunc(&shown, room)
        };
        let mut spans = vec![
            Span::styled(
                format!(" {}", pad_r(f.label, label_w)),
                if active { style::key() } else { style::dim() },
            ),
            Span::styled(if active { "› " } else { "  " }, style::amber()),
            Span::styled(text, if active { style::fgb() } else { style::fg() }),
        ];
        if active && !f.is_choice() {
            spans.push(Span::styled("▏", style::amber()));
        }
        lines.push(Line::from(fit(spans, inner)));
    }
    lines.push(Line::from(""));
    for n in &form.notes {
        for l in wrap(&format!(" {n}"), inner) {
            lines.push(Line::from(fit(vec![Span::styled(l, style::dim())], inner)));
        }
    }
    if let Some(e) = &form.error {
        for l in wrap(&format!(" ✗ {e}"), inner) {
            lines.push(Line::from(fit(vec![Span::styled(l, style::warn())], inner)));
        }
    }
    let last = form.at + 1 >= form.fields.len();
    let mut keys = vec![Span::raw(" ")];
    let mut pairs = Vec::new();
    if form.fields.len() > 1 {
        pairs.push(("tab", "Next field"));
    }
    pairs.push(("enter", if last { "Send" } else { "Next" }));
    pairs.push(("esc", "Cancel"));
    for (k, l) in pairs {
        keys.push(Span::styled(k, style::key()));
        keys.push(Span::styled(format!(" {l}   "), style::dim()));
    }
    lines.push(Line::from(fit(keys, inner)));

    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y
            + area
                .height
                .saturating_sub(h + 1)
                .min(area.height.saturating_sub(h)),
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let title = trunc(
        &form.title,
        inner.saturating_sub(width(&form.right) + 6).max(8),
    );
    let right = trunc(&form.right, inner.saturating_sub(width(&title) + 6));
    let body = panel(frame, rect, &title, &right);
    rows(frame, body, lines, None);
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

/// What the items of a value are called, `n` of them.
fn length_label(kind: Option<&Kind>, n: u64) -> &'static str {
    let one = n == 1;
    match kind {
        Some(Kind::Hash) if one => "field",
        Some(Kind::Hash) => "fields",
        Some(Kind::List) if one => "item",
        Some(Kind::List) => "items",
        Some(Kind::Set | Kind::ZSet) if one => "member",
        Some(Kind::Set | Kind::ZSet) => "members",
        Some(Kind::Stream) if one => "entry",
        Some(Kind::Stream) => "entries",
        Some(Kind::Vector) if one => "element",
        Some(Kind::Vector) => "elements",
        _ => "long",
    }
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

/// The right of a value's title: its type, then loading, the format shown, how many
/// items are loaded.
pub fn title_right(state: &ValueState, ctx: &Ctx<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = state
        .type_name
        .as_deref()
        .map(value::escape)
        .or_else(|| state.row.as_ref().map(|r| r.key_type.as_str().to_string()))
    {
        parts.push(t);
    }
    if state.is_loading() {
        parts.push(format!("{} loading", spinner(ctx.tick)));
    }
    // The format is in the line above the value; the title repeats it only with `v`.
    if ctx.more
        && let Some(raw) = state.format_source()
    {
        let d = state.memo.decode(raw);
        parts.push(state.shown_format(&d).label().to_string());
    }
    match &state.items {
        Some(Items::String { len, .. }) => {
            if let Some(n) = len.filter(|n| *n > STRING_BYTES as u64) {
                parts.push(format!("first 64 KiB of {}", format::bytes(n)));
            }
        }
        Some(Items::Json { .. } | Items::JsonNotRead { .. } | Items::NotRead) | None => {}
        // How much is loaded, while not all of it is.
        Some(items) => {
            let n = items.len() as u64;
            match state.row.as_ref().and_then(|r| r.length) {
                Some(m) if n < m => {
                    parts.push(format!("{} of {}", format::grouped(n), format::grouped(m)))
                }
                Some(_) => {}
                None => parts.push(format!("{} loaded", format::grouped(n))),
            }
        }
    }
    // The command only with `v` (brief: command names leave titles).
    if ctx.more
        && let Some(cmd) = state.kind().and_then(|k| k.command())
    {
        parts.push(cmd.to_string());
    }
    parts.join(" · ")
}

/// What a value pane holds, without a frame: the items and the selected one's value,
/// a string or document, or why there is nothing to show. For the right pane of a
/// key's value and the views that embed one (stream entries, a Search result).
pub fn draw_value_body(frame: &mut Frame, inner: Rect, state: &ValueState, ctx: &Ctx<'_>) {
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
    // One column less than the pane: `scrolled` starts each line with a space.
    let w = usize::from(area.width).saturating_sub(1).max(1);
    let (d, r) = state.memo.render(raw, |d| state.shown_format(d), w);
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
            // `scrolled` adds a space and each field's lines two more.
            let w = iw.saturating_sub(3).max(1);
            let mut body = Vec::new();
            for (name, v) in &e.fields {
                let (_, r) = state.memo.render(v, |d| state.shown_format(d), w);
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
                Items::ZSet(m) => {
                    format!("member · score {}", state.memo.preview(&m[sel].score, 24))
                }
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
                        cell(state.memo.preview(&f.value, vw), vw),
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
                        cell(state.memo.preview(e, ew), ew),
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
                .map(|m| vec![sp(), cell(state.memo.preview(m, w), w)])
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
                        cell(state.memo.preview(&m.member, mw), mw),
                        sp(),
                        Span::styled(
                            pad_l(&state.memo.preview(&m.score, SCORE_W), SCORE_W),
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
                            format!("{}={}", name_text(k.as_bytes()), state.memo.preview(v, fw))
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
