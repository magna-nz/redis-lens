//! Draws [`SearchState`]. Owner: wave B, with `views/search.rs`.
//!
//! The indexes `FT._LIST` names, one row each, and the selected index's schema below,
//! or the results of the query last run on it. Figures the module does not return are
//! left out, with the reason, never shown as errors. One level deeper, over the whole
//! body: a result's fields (the key detail's value panel), or the `FT.EXPLAIN` or
//! `FT.PROFILE` text.
//!
//! Every server string is escaped before it is drawn: index names, prefixes, field
//! names, keys, and (through the value renderer) values.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::format;
use crate::live::search::{Doc, FieldInfo, IndexInfo};
use crate::model::ServerFlavor;
use crate::tui::app::views::Ctx;
use crate::tui::app::views::search::{Cmd, Deeper, Focus, Outcome, QueryRun, SearchState};
use crate::value::{self, escape};

use super::streams::live_wait;
use super::widgets::{fit, pad_l, pad_r, panel, rows, scroll_offset, trunc, width};
use super::{notice_panels, spinner, split_body, style};

const ON_W: usize = 5;
const DOCS_W: usize = 7;
const SIZE_W: usize = 6;
const INDEXED_W: usize = 8;
const FAILS_W: usize = 6;

/// Which search module the server runs, for the reasons.
fn module(ctx: &Ctx<'_>) -> &'static str {
    match ctx.server().map(|s| s.flavor) {
        Some(ServerFlavor::Redis) => "Redis Search",
        Some(ServerFlavor::Valkey) => "valkey-search",
        _ => "the Search module",
    }
}

/// `e` as a sentence: the error texts start in lower case.
fn sentence(e: &str) -> String {
    let mut c = e.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &SearchState, ctx: &Ctx<'_>) {
    draw_view(frame, area, state, ctx);
    if let (Some(form), Some(sends)) = (&state.create, state.form_sends()) {
        super::streams::draw_form(frame, area, form, &sends, ctx);
    }
}

fn draw_view(frame: &mut Frame, area: Rect, state: &SearchState, ctx: &Ctx<'_>) {
    const TITLE: &str = "Search";
    if let Some(off) = &state.off {
        let lines = vec![Line::from(Span::styled(
            format!(" ▲ {off}"),
            style::amber(),
        ))];
        notice_panels(frame, area, TITLE, "off", lines);
        return;
    }
    if state.no_module {
        let lines = vec![
            Line::from(Span::styled(
                " The Search module is not loaded on this server.",
                style::fgb(),
            )),
            Line::from(Span::styled(
                " FT._LIST is unknown, so there is no Redis Search (built into Redis 8)",
                style::dim(),
            )),
            Line::from(Span::styled(
                " or valkey-search to list indexes from. Press r to look again.",
                style::dim(),
            )),
        ];
        notice_panels(frame, area, TITLE, "module missing", lines);
        return;
    }
    if !state.loaded
        && let Some(e) = &state.error
    {
        let lines = vec![
            Line::from(Span::styled(format!(" ▲ {}.", sentence(e)), style::amber())),
            Line::from(Span::styled(" Press r to read again.", style::dim())),
        ];
        notice_panels(frame, area, TITLE, "failed", lines);
        return;
    }
    if !state.loaded {
        let text = live_wait(ctx).unwrap_or_else(|| "Reading the indexes with FT._LIST…".into());
        let lines = vec![Line::from(Span::styled(format!(" {text}"), style::dim()))];
        notice_panels(frame, area, TITLE, "", lines);
        return;
    }
    let name = module(ctx);
    if state.list.indexes.is_empty() {
        let lines = vec![
            Line::from(Span::styled(" The module has no indexes.", style::fgb())),
            Line::from(Span::styled(
                format!(" {name} is loaded; nothing was created with FT.CREATE yet."),
                style::dim(),
            )),
        ];
        notice_panels(frame, area, TITLE, name, lines);
        return;
    }

    if let Some(deeper) = &state.deeper {
        draw_deeper(frame, area, state, deeper, ctx);
        return;
    }
    let shown = state.shown_results();
    let (main, detail) = match shown {
        Some(_) => results_split(area, state.list.indexes.len()),
        None => split_body(area),
    };
    let list = &state.list;
    let cut = if list.total > list.indexes.len() {
        format!(" · first {} of {}", list.indexes.len(), list.total)
    } else {
        String::new()
    };
    let live = if ctx.live_ready() { "" } else { "not live · " };
    let inner = panel(frame, main, TITLE, &format!("{live}{name}{cut}"));
    let w = usize::from(inner.width);

    let any = |f: fn(&IndexInfo) -> bool| list.indexes.iter().any(f);
    let show_size = any(|i| i.bytes.is_some());
    let show_indexed = any(|i| i.progress.is_some() || i.indexing.is_some());
    let show_fails = any(|i| i.failures.is_some());
    let fixed = ON_W
        + 1
        + DOCS_W
        + 1
        + if show_size { SIZE_W + 1 } else { 0 }
        + if show_indexed { INDEXED_W + 1 } else { 0 }
        + if show_fails { FAILS_W + 1 } else { 0 };
    let avail = w.saturating_sub(1 + fixed).max(10);
    let pw = avail / 2;
    let nw = avail - pw;

    let mut head = vec![
        Span::styled(pad_r(" INDEX", nw), style::hd()),
        Span::styled(pad_r("PREFIXES", pw), style::hd()),
        Span::styled(format!(" {}", pad_r("ON", ON_W - 1)), style::hd()),
        Span::styled(format!(" {}", pad_l("DOCS", DOCS_W)), style::hd()),
    ];
    if show_size {
        head.push(Span::styled(
            format!(" {}", pad_l("SIZE", SIZE_W)),
            style::hd(),
        ));
    }
    if show_indexed {
        head.push(Span::styled(
            format!(" {}", pad_l("INDEXED", INDEXED_W)),
            style::hd(),
        ));
    }
    if show_fails {
        head.push(Span::styled(
            format!(" {}", pad_l("FAILS", FAILS_W)),
            style::hd(),
        ));
    }
    rows(frame, inner, vec![Line::from(head)], None);
    let has_error = state.error.is_some();
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1 + u16::from(has_error)),
        ..inner
    };
    let sel = state.sel.min(list.indexes.len() - 1);
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = list
        .indexes
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .map(|i| list_row(i, nw, pw, [show_size, show_indexed, show_fails]))
        .collect();
    rows(frame, body, lines, Some(sel - off));
    if let Some(e) = &state.error {
        let r = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        let note = format!(" ▲ {e}; showing the previous reading.");
        rows(
            frame,
            r,
            vec![Line::from(fit(vec![Span::styled(note, style::amber())], w))],
            None,
        );
    }
    match shown {
        Some(run) => results_panel(frame, detail, state, run, ctx),
        None => detail_panel(frame, detail, state, name),
    }
}

/// With results shown, the index list takes what it needs (at least 5 rows, at most
/// half) and the results the rest.
fn results_split(area: Rect, indexes: usize) -> (Rect, Rect) {
    let h = usize::from(area.height);
    let list_h = (indexes + 3).clamp(5, (h / 2).max(5)).min(h);
    let list_h = u16::try_from(list_h).unwrap_or(area.height);
    let main = Rect {
        height: list_h,
        ..area
    };
    let detail = Rect {
        y: area.y + list_h,
        height: area.height - list_h,
        ..area
    };
    (main, detail)
}

/// `took` for the results line: `3 ms`, `<1 ms`.
fn took_text(d: Duration) -> String {
    match d.as_millis() {
        0 => "<1 ms".to_string(),
        n => format!("{} ms", format::grouped(n as u64)),
    }
}

/// The query as typed, escaped, on one line.
fn query_text(q: &str) -> String {
    escape(q)
}

/// What the keys of the results do, leaving out the commands the server lacks.
fn results_keys(state: &SearchState) -> String {
    let mut parts = vec!["/ query", "enter open"];
    if state.off_reason(Cmd::Explain).is_none() {
        parts.push("e explain");
    }
    if state.off_reason(Cmd::Profile).is_none() {
        parts.push("P profile");
    }
    parts.join(" · ")
}

/// A one-line preview of a result's first fields: `name=value name=value`.
fn fields_preview(doc: &Doc, w: usize, memo: &value::Memo) -> String {
    let mut out = String::new();
    for (name, v) in doc.fields.iter().take(4) {
        if width(&out) >= w {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&super::value::name_text(name.as_bytes()));
        out.push('=');
        out.push_str(&memo.preview(v, w.max(1)));
    }
    trunc(&out, w)
}

fn results_panel(
    frame: &mut Frame,
    area: Rect,
    state: &SearchState,
    run: &QueryRun,
    ctx: &Ctx<'_>,
) {
    let focused = state.focus == Focus::Schema;
    let hint = if focused {
        "↑↓ select · tab back"
    } else {
        "tab: results"
    };
    let what = if run.cmd == Cmd::Profile {
        format!("Results · {} · FT.PROFILE", escape(&run.index))
    } else {
        format!("Results · {}", escape(&run.index))
    };
    let inner = panel(frame, area, &what, hint);
    let w = usize::from(inner.width);
    let h = usize::from(inner.height);
    if w == 0 || h == 0 {
        return;
    }
    let q = query_text(&run.query);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let results = match &run.outcome {
        Outcome::Running => {
            lines.push(Line::from(fit(
                vec![Span::styled(
                    format!(" {} Running {} {q}…", spinner(ctx.tick), run.cmd.name()),
                    style::dim(),
                )],
                w,
            )));
            rows(frame, inner, lines, None);
            return;
        }
        Outcome::Failed(why) => {
            lines.push(Line::from(fit(
                vec![
                    Span::styled(" query ", style::dim()),
                    Span::styled(q, style::fg()),
                ],
                w,
            )));
            for l in wrap_text(&format!(" ✗ {why}"), w) {
                lines.push(Line::from(fit(vec![Span::styled(l, style::warn())], w)));
            }
            lines.push(Line::from(fit(
                vec![Span::styled(
                    " Press / to change the query.".to_string(),
                    style::dim(),
                )],
                w,
            )));
            rows(frame, inner, lines, None);
            return;
        }
        Outcome::Done(r) => r,
    };
    let n = results.docs.len();
    let count = if results.total as usize > n {
        format!(
            "{} of {} results",
            format::grouped(n as u64),
            format::grouped(results.total)
        )
    } else {
        format!(
            "{} result{}",
            format::grouped(results.total),
            if results.total == 1 { "" } else { "s" }
        )
    };
    lines.push(Line::from(fit(
        vec![
            Span::styled(" query ", style::dim()),
            Span::styled(q, style::fgb()),
            Span::styled(
                format!(" · {count} · {}", took_text(results.took)),
                style::dim(),
            ),
        ],
        w,
    )));
    let kw = (w * 35 / 100).clamp(10, 40);
    let fw = w.saturating_sub(kw + 2);
    lines.push(Line::from(vec![
        Span::styled(pad_r(" KEY", kw + 1), style::hd()),
        Span::styled(pad_r("FIELDS", fw), style::hd()),
    ]));
    let first = lines.len();
    // One row at the bottom for the keys.
    let room = h.saturating_sub(first + 1);
    let sel = state.result_sel.min(n.saturating_sub(1));
    let off = scroll_offset(sel, room);
    if n == 0 {
        lines.push(Line::from(Span::styled(
            " No documents match the query.",
            style::dim(),
        )));
    }
    for d in results.docs.iter().skip(off).take(room) {
        lines.push(Line::from(vec![
            Span::styled(
                pad_r(&format!(" {}", trunc(&escape(&d.key), kw)), kw + 1),
                style::fg(),
            ),
            Span::styled(pad_r(&fields_preview(d, fw, &state.memo), fw), style::dim()),
        ]));
    }
    let selected = (focused && n > 0).then(|| sel - off + first);
    let keys_row = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };
    let body = Rect {
        height: inner.height - 1,
        ..inner
    };
    rows(frame, body, lines, selected);
    rows(
        frame,
        keys_row,
        vec![Line::from(fit(
            vec![Span::styled(
                format!(" {}", results_keys(state)),
                style::dim(),
            )],
            w,
        ))],
        None,
    );
}

/// `text` in lines at most `w` cells wide, broken at spaces. Its leading spaces start
/// the first line; the others are indented two more, past the `✗`.
fn wrap_text(text: &str, w: usize) -> Vec<String> {
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

/// One level deeper, over the whole body: a result's fields, or an explain or profile.
fn draw_deeper(frame: &mut Frame, area: Rect, state: &SearchState, deeper: &Deeper, ctx: &Ctx<'_>) {
    match deeper {
        Deeper::Document(doc) => {
            let key = doc.row.as_ref().map_or("", |r| r.key.as_str());
            let n = doc.loaded();
            let mut right = Vec::new();
            if let Some(raw) = doc.format_source() {
                let d = doc.memo.decode(raw);
                right.push(doc.shown_format(&d).label().to_string());
            }
            right.push(format!(
                "{} field{}",
                format::grouped(n as u64),
                if n == 1 { "" } else { "s" }
            ));
            right.push("esc back".to_string());
            let title = format!("Result · {}", escape(key));
            super::value::draw_value_panel(frame, area, doc, ctx, &title, Some(&right.join(" · ")));
        }
        Deeper::Text {
            cmd,
            index,
            query,
            text,
        } => {
            let what = match cmd {
                Cmd::Profile => "Profile",
                _ => "Explain",
            };
            let title = format!("{what} · {}", escape(index));
            let inner = panel(frame, area, &title, "↑↓ scroll · esc back");
            let w = usize::from(inner.width);
            if w == 0 || inner.height == 0 {
                return;
            }
            let mut lines = vec![Line::from(fit(
                vec![
                    Span::styled(format!(" {} ", cmd.name()), style::dim()),
                    Span::styled(query_text(query), style::fgb()),
                ],
                w,
            ))];
            match text {
                Outcome::Running => {
                    lines.push(Line::from(Span::styled(
                        format!(" {} Waiting for the answer…", spinner(ctx.tick)),
                        style::dim(),
                    )));
                    state.text_drawn.set(0, 0);
                    rows(frame, inner, lines, None);
                }
                Outcome::Failed(why) => {
                    for l in wrap_text(&format!(" ✗ {why}"), w) {
                        lines.push(Line::from(fit(vec![Span::styled(l, style::warn())], w)));
                    }
                    state.text_drawn.set(0, 0);
                    rows(frame, inner, lines, None);
                }
                Outcome::Done(t) => {
                    let height = usize::from(inner.height).saturating_sub(1);
                    state.text_drawn.set(t.len(), height);
                    let max = t.len().saturating_sub(height);
                    let off = state.text_scroll.min(max);
                    if t.is_empty() {
                        lines.push(Line::from(Span::styled(
                            " The server returned nothing.",
                            style::dim(),
                        )));
                    }
                    lines.extend(t.lines().iter().skip(off).take(height).map(|l| {
                        Line::from(fit(vec![Span::styled(format!(" {l}"), style::fg())], w))
                    }));
                    rows(frame, inner, lines, None);
                }
            }
        }
    }
}

fn progress_cell(i: &IndexInfo) -> (String, bool) {
    match (i.indexing, i.progress) {
        (Some(true), Some(p)) => (format::percent(p), true),
        (Some(true), None) => ("building".to_string(), true),
        (_, Some(p)) => (format::percent(p), false),
        (Some(false), None) => ("done".to_string(), false),
        (None, None) => ("—".to_string(), false),
    }
}

fn list_row(i: &IndexInfo, nw: usize, pw: usize, shown: [bool; 3]) -> Line<'static> {
    let dash = || "—".to_string();
    let mut spans = fit(
        vec![Span::styled(format!(" {}", escape(&i.name)), style::fg())],
        nw,
    );
    spans.extend(fit(
        vec![Span::styled(escape(&i.prefixes.join(", ")), style::ph())],
        pw,
    ));
    spans.push(Span::styled(
        format!(
            " {}",
            pad_r(&escape(i.on.as_deref().unwrap_or("—")), ON_W - 1)
        ),
        style::dim(),
    ));
    spans.push(Span::styled(
        format!(
            " {}",
            pad_l(&i.docs.map_or_else(dash, format::count), DOCS_W)
        ),
        style::fgb(),
    ));
    let [size, indexed, fails] = shown;
    if size {
        spans.push(Span::styled(
            format!(
                " {}",
                pad_l(&i.bytes.map_or_else(dash, format::bytes), SIZE_W)
            ),
            style::fg(),
        ));
    }
    if indexed {
        let (text, building) = progress_cell(i);
        spans.push(Span::styled(
            format!(" {}", pad_l(&text, INDEXED_W)),
            if building {
                style::amber()
            } else {
                style::ok()
            },
        ));
    }
    if fails {
        let n = i.failures;
        spans.push(Span::styled(
            format!(" {}", pad_l(&n.map_or_else(dash, format::count), FAILS_W)),
            if n.is_some_and(|n| n > 0) {
                style::warn()
            } else {
                style::fg()
            },
        ));
    }
    Line::from(spans)
}

/// What the module did not return for `i`, with why.
fn left_out(i: &IndexInfo, module: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |missing: bool, what: &str| {
        if missing {
            out.push(format!("{what} ({module} does not report it)"));
        }
    };
    add(i.docs.is_none(), "documents");
    add(i.bytes.is_none(), "size");
    add(
        i.progress.is_none() && i.indexing.is_none(),
        "indexing progress",
    );
    add(i.failures.is_none(), "indexing failures");
    out
}

fn detail_panel(frame: &mut Frame, area: Rect, state: &SearchState, module: &str) {
    let Some(i) = state.selected() else {
        panel(frame, area, "Details", "");
        return;
    };
    let focused = state.focus == Focus::Schema;
    let hint = if focused {
        "↑↓ select · tab back"
    } else {
        "tab: schema · / query"
    };
    let inner = panel(frame, area, &escape(&i.name), hint);
    let w = usize::from(inner.width);
    let dash = || "—".to_string();
    let info_read = i.docs.is_some() || !i.schema.is_empty() || i.on.is_some();

    let mut lines: Vec<Line<'static>> = Vec::new();
    if !info_read {
        lines.push(Line::from(Span::styled(
            " FT.INFO is not available for this user: only the name is known.",
            style::dim(),
        )));
        rows(frame, inner, lines, None);
        return;
    }
    lines.push(Line::from(fit(
        vec![
            Span::styled(" indexes ", style::dim()),
            Span::styled(i.on.as_deref().map_or_else(dash, escape), style::fgb()),
            Span::styled(" · prefixes ", style::dim()),
            Span::styled(
                if i.prefixes.is_empty() {
                    "none (every key)".to_string()
                } else {
                    escape(&i.prefixes.join(", "))
                },
                style::ph(),
            ),
        ],
        w,
    )));
    let mut facts = vec![Span::styled(" documents ", style::dim())];
    facts.push(Span::styled(
        i.docs.map_or_else(dash, format::grouped),
        style::fgb(),
    ));
    if let Some(b) = i.bytes {
        facts.push(Span::styled(" · size ", style::dim()));
        facts.push(Span::styled(format::bytes(b), style::fgb()));
    }
    if i.progress.is_some() || i.indexing.is_some() {
        let (text, building) = progress_cell(i);
        facts.push(Span::styled(" · indexed ", style::dim()));
        facts.push(Span::styled(
            if building {
                format!("{text} (building)")
            } else {
                text
            },
            if building {
                style::amber()
            } else {
                style::fgb()
            },
        ));
    }
    if let Some(n) = i.failures {
        facts.push(Span::styled(" · failures ", style::dim()));
        facts.push(Span::styled(
            format::grouped(n),
            if n > 0 { style::warn() } else { style::fgb() },
        ));
    }
    lines.push(Line::from(fit(facts, w)));
    let gaps = left_out(i, module);
    if !gaps.is_empty() {
        lines.push(Line::from(fit(
            vec![Span::styled(
                format!(" left out: {}", gaps.join(", ")),
                style::dim(),
            )],
            w,
        )));
    }
    let fw = (w / 4).max(10);
    lines.push(Line::from(vec![
        Span::styled(pad_r(" FIELD", fw), style::hd()),
        Span::styled(pad_r("PATH", fw), style::hd()),
        Span::styled(pad_r("TYPE", 9), style::hd()),
        Span::styled("OPTIONS", style::hd()),
    ]));
    let room = usize::from(inner.height).saturating_sub(lines.len());
    let first = lines.len();
    let off = if focused {
        scroll_offset(state.schema_sel.min(i.schema.len().saturating_sub(1)), room)
    } else {
        0
    };
    for f in i.schema.iter().skip(off).take(room) {
        lines.push(field_line(f, fw));
    }
    if i.schema.is_empty() {
        lines.push(Line::from(Span::styled(
            " The module returned no fields.",
            style::dim(),
        )));
    }
    let selected = focused
        .then(|| state.schema_sel.checked_sub(off).map(|n| n + first))
        .flatten();
    rows(frame, inner, lines, selected);
}

fn field_line(f: &FieldInfo, fw: usize) -> Line<'static> {
    let path = if f.identifier == f.name {
        String::new()
    } else {
        escape(&f.identifier)
    };
    Line::from(vec![
        Span::styled(pad_r(&format!(" {}", escape(&f.name)), fw), style::fg()),
        Span::styled(pad_r(&trunc(&path, fw - 1), fw), style::ph()),
        Span::styled(pad_r(&escape(&f.ty), 9), style::fgb()),
        Span::styled(escape(&f.flags.join(" ")), style::dim()),
    ])
}
