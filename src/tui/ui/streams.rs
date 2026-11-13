//! Draws [`StreamsState`]. Owner: wave B, with `views/streams.rs`.
//!
//! The streams the last scan found, one row each, and the selected stream's groups and
//! consumers below. With the entries open (`enter`), the whole body shows them, drawn by
//! the key detail's value panel ([`super::value::draw_value_panel`]).
//!
//! Every server string is escaped before it is drawn: key, group and consumer names,
//! entry IDs, and (through the value renderer) the entries' fields.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::format;
use ratatui::widgets::Clear;

use crate::live::streams::{ConsumerInfo, GroupInfo, Lag, StreamInfo, Verdict};
use crate::model::Env;
use crate::tui::app::views::streams::form::Form;
use crate::tui::app::views::streams::{DetailRow, Focus, StreamRow, StreamsState};
use crate::tui::app::views::{Ctx, LiveState};

use crate::value::escape;

use super::widgets::{char_width, fit, pad_l, pad_r, panel, rows, scroll_offset, trunc, width};
use super::{notice_panels, split_body, style};

const LEN_W: usize = 6;
const SIZE_W: usize = 6;
const AGE_W: usize = 6;
const GROUPS_W: usize = 3;
const LAG_W: usize = 6;
const PEND_W: usize = 6;
const VERDICT_W: usize = 11;
const ID_W: usize = 17;
/// From this inner width on, the list also shows the last ID.
const WIDE: usize = 100;

/// Why the live connection cannot be asked, if it cannot. Shared by the three views that
/// poll or look things up.
pub(super) fn live_wait(ctx: &Ctx<'_>) -> Option<String> {
    match ctx.live() {
        LiveState::Ready => None,
        LiveState::Waiting => Some("Waiting for the live connection…".to_string()),
        LiveState::Closed(None) => {
            Some("The live connection is closed. Scan again to open it.".to_string())
        }
        LiveState::Closed(Some(why)) => Some(format!("The live connection closed: {why}.")),
    }
}

/// A short age: `12s`, `5m`, `3h`, `41d`.
pub fn age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m", s / 60),
        3_600..=86_399 => format!("{}h", s / 3_600),
        _ => format!("{}d", s / 86_400),
    }
}

fn verdict_style(v: Verdict) -> Style {
    match v {
        Verdict::Stuck | Verdict::Lagging => style::warn(),
        Verdict::NoReaders | Verdict::Unbounded => style::amber(),
        Verdict::Ok => style::ok(),
    }
}

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &StreamsState, ctx: &Ctx<'_>) {
    const TITLE: &str = "Streams";
    if let Some(entries) = state.shown_entries() {
        let key = entries.row.as_ref().map_or("", |r| r.key.as_str());
        let title = format!("Entries · {}", escape(key));
        super::value::draw_value_panel(frame, area, entries, ctx, &title, None);
        return;
    }
    if let Some(off) = &state.off {
        let lines = vec![Line::from(Span::styled(
            format!(" ▲ {off}"),
            style::amber(),
        ))];
        notice_panels(frame, area, TITLE, "off", lines);
        return;
    }
    if !state.loaded {
        let text = live_wait(ctx).unwrap_or_else(|| {
            if ctx.report().is_none() {
                "Waiting for the scan to find the streams…".to_string()
            } else {
                "Reading the streams with XINFO…".to_string()
            }
        });
        let lines = vec![Line::from(Span::styled(format!(" {text}"), style::dim()))];
        notice_panels(frame, area, TITLE, "", lines);
        return;
    }
    if state.rows.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                " The last scan found no streams.",
                style::fgb(),
            )),
            Line::from(Span::styled(
                " A sampled scan may miss some; a full scan (no sample limit) finds them all.",
                style::dim(),
            )),
        ];
        notice_panels(frame, area, TITLE, "from the last scan", lines);
        return;
    }

    let (main, detail) = split_body(area);
    let mut prefix = String::new();
    if !ctx.live_ready() {
        prefix.push_str("not live · ");
    }
    if state.paused {
        prefix.push_str("paused · ");
    }
    let cut = if state.found > state.rows.len() {
        format!(" · first {} of {}", state.rows.len(), state.found)
    } else {
        String::new()
    };
    let right = format!("{prefix}from the last scan, which may miss some{cut}");
    let inner = panel(frame, main, TITLE, &right);
    let w = usize::from(inner.width);
    let wide = w >= WIDE;
    let kw = key_width(w, wide);

    let mut head = vec![
        Span::styled(pad_r(" KEY", kw), style::hd()),
        Span::styled(format!(" {}", pad_l("LEN", LEN_W)), style::hd()),
        Span::styled(format!(" {}", pad_l("SIZE", SIZE_W)), style::hd()),
        Span::styled(format!(" {}", pad_l("OLDEST", AGE_W)), style::hd()),
        Span::styled(format!(" {}", pad_l("GRP", GROUPS_W)), style::hd()),
        Span::styled(format!(" {}", pad_l("LAG", LAG_W)), style::hd()),
        Span::styled(format!(" {}", pad_l("PEND", PEND_W)), style::hd()),
    ];
    if wide {
        head.push(Span::styled(
            format!(" {}", pad_r("LAST ID", ID_W)),
            style::hd(),
        ));
    }
    head.push(Span::styled(
        format!(" {}", pad_r("VERDICT", VERDICT_W)),
        style::hd(),
    ));
    rows(frame, inner, vec![Line::from(head)], None);

    let has_error = state.error.is_some();
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1 + u16::from(has_error)),
        ..inner
    };
    let sel = state.sel.min(state.rows.len() - 1);
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = state
        .rows
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .map(|r| list_row(r, kw, wide))
        .collect();
    rows(frame, body, lines, Some(sel - off));
    if let Some(e) = &state.error {
        let note = format!(" ▲ {e}; showing the previous reading.");
        let r = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        rows(
            frame,
            r,
            vec![Line::from(fit(vec![Span::styled(note, style::amber())], w))],
            None,
        );
    }
    detail_panel(frame, detail, state);
    if let (Some(open), Some(sends)) = (&state.form, state.form_sends()) {
        draw_form(frame, area, open.form(), &sends, ctx);
    }
}

fn key_width(inner: usize, wide: bool) -> usize {
    let fixed = 1
        + LEN_W
        + 1
        + SIZE_W
        + 1
        + AGE_W
        + 1
        + GROUPS_W
        + 1
        + LAG_W
        + 1
        + PEND_W
        + 1
        + VERDICT_W
        + if wide { 1 + ID_W } else { 0 };
    inner.saturating_sub(fixed).max(8)
}

/// The most lag any group has, if any group reports one.
fn max_lag(info: &StreamInfo) -> Option<u64> {
    info.groups
        .iter()
        .flatten()
        .filter_map(|g| match g.lag {
            Lag::Known(n) => Some(n),
            _ => None,
        })
        .max()
}

fn total_pending(info: &StreamInfo) -> Option<u64> {
    info.groups
        .as_ref()
        .map(|gs| gs.iter().map(|g| g.pending).sum())
}

fn list_row(r: &StreamRow, kw: usize, wide: bool) -> Line<'static> {
    let dash = || "—".to_string();
    let mut spans = fit(
        vec![Span::styled(format!(" {}", escape(&r.key)), style::fg())],
        kw,
    );
    let Some(i) = &r.info else {
        spans.push(Span::styled(" gone since the scan", style::dim()));
        return Line::from(spans);
    };
    let cell = |s: String, w: usize, st: Style| Span::styled(format!(" {}", pad_l(&s, w)), st);
    let lag = max_lag(i);
    spans.extend([
        cell(format::count(i.length), LEN_W, style::fgb()),
        cell(
            i.bytes.map_or_else(dash, format::bytes),
            SIZE_W,
            style::fg(),
        ),
        cell(i.oldest_age_ms.map_or_else(dash, age), AGE_W, style::fg()),
        cell(
            i.groups.as_ref().map_or_else(dash, |g| g.len().to_string()),
            GROUPS_W,
            style::fg(),
        ),
        cell(
            lag.map_or_else(dash, format::count),
            LAG_W,
            if lag.is_some_and(|n| n >= crate::live::streams::LAGGING_LAG) {
                style::warn()
            } else {
                style::fg()
            },
        ),
        cell(
            total_pending(i).map_or_else(dash, format::count),
            PEND_W,
            style::fg(),
        ),
    ]);
    if wide {
        spans.push(Span::styled(
            format!(" {}", pad_r(&escape(&i.last_id), ID_W)),
            style::dim(),
        ));
    }
    let v = r.verdicts.first().copied().unwrap_or(Verdict::Ok);
    let more = if r.verdicts.len() > 1 { "+" } else { "" };
    spans.push(Span::styled(
        format!(" {}", pad_r(&format!("{}{more}", v.label()), VERDICT_W)),
        verdict_style(v),
    ));
    Line::from(spans)
}

fn detail_panel(frame: &mut Frame, area: Rect, state: &StreamsState) {
    let Some(sel) = state.selected() else {
        panel(frame, area, "Details", "");
        return;
    };
    let focused = state.focus == Focus::Detail;
    let hint = if focused {
        "↑↓ select · A pending · tab back"
    } else {
        "tab: groups · enter: entries"
    };
    let Some(info) = &sel.info else {
        let inner = panel(frame, area, &escape(&sel.key), "");
        rows(
            frame,
            inner,
            vec![Line::from(Span::styled(
                " The key is gone, or no longer a stream. Rescan to update the list.",
                style::dim(),
            ))],
            None,
        );
        return;
    };
    let inner = panel(frame, area, &escape(&sel.key), hint);
    let w = usize::from(inner.width);
    let dash = || "—".to_string();

    let facts = vec![
        Span::styled(" length ", style::dim()),
        Span::styled(format::grouped(info.length), style::fgb()),
        Span::styled(" · size ", style::dim()),
        Span::styled(info.bytes.map_or_else(dash, format::bytes), style::fgb()),
        Span::styled(" · oldest ", style::dim()),
        Span::styled(
            info.oldest_age_ms
                .map_or_else(|| "—".to_string(), |a| format!("{} ago", age(a))),
            style::fgb(),
        ),
        Span::styled(" · last ", style::dim()),
        Span::styled(escape(&info.last_id), style::fg()),
    ];

    let mut verdict_line = vec![Span::styled(" verdict ", style::dim())];
    for (n, v) in sel.verdicts.iter().enumerate() {
        if n > 0 {
            verdict_line.push(Span::styled(" · ", style::dim()));
        }
        verdict_line.push(Span::styled(v.label(), verdict_style(*v)));
    }
    for note in left_out(info) {
        verdict_line.push(Span::styled(format!(" · {note}"), style::dim()));
    }

    let all = state.details();
    let h = usize::from(inner.height).saturating_sub(3);
    let off = if focused {
        scroll_offset(state.detail_sel.min(all.len().saturating_sub(1)), h)
    } else {
        0
    };
    let nw = name_width(w);
    let mut lines = vec![
        Line::from(fit(facts, w)),
        Line::from(fit(verdict_line, w)),
        Line::from(vec![
            Span::styled(pad_r(" GROUP / CONSUMER", nw), style::hd()),
            Span::styled(
                format!(
                    " {} {} {} {} {}",
                    pad_l("PEND", PEND_W),
                    pad_l("IDLE", AGE_W),
                    pad_l("INACT", AGE_W),
                    pad_l("LAG", LAG_W),
                    "LAST DELIVERED"
                ),
                style::hd(),
            ),
        ]),
    ];
    if let Some(why) = state.shown_pending().and_then(|p| p.error.as_ref()) {
        lines.push(Line::from(fit(
            vec![Span::styled(
                format!(" ▲ Pending entries: {why}."),
                style::amber(),
            )],
            w,
        )));
    }
    if all.is_empty() {
        let text = match &info.groups {
            None => " XINFO GROUPS is not available for this user.",
            Some(_) => " No consumer groups: nothing reads this stream.",
        };
        lines.push(Line::from(Span::styled(text, style::dim())));
    }
    let shown = all.iter().skip(off).take(h);
    for row in shown {
        lines.push(detail_line(row, nw));
    }
    let selected = focused
        .then(|| state.detail_sel.checked_sub(off).map(|i| i + 3))
        .flatten();
    rows(frame, inner, lines, selected);
}

fn name_width(inner: usize) -> usize {
    inner
        .saturating_sub(1 + PEND_W + 1 + AGE_W + 1 + AGE_W + 1 + LAG_W + 1 + 16)
        .max(12)
}

fn detail_line(row: &DetailRow<'_>, nw: usize) -> Line<'static> {
    let dash = || "—".to_string();
    match row {
        DetailRow::Group(g) => Line::from(group_spans(g, nw)),
        DetailRow::Pending(_, e) => Line::from(vec![
            Span::styled(
                pad_r(&format!("       · {}", escape(&e.id)), nw),
                style::dim(),
            ),
            Span::styled(
                format!(" {}", pad_l(&format!("×{}", e.deliveries), PEND_W)),
                style::dim(),
            ),
            Span::styled(format!(" {}", pad_l(&age(e.idle_ms), AGE_W)), style::fg()),
            Span::styled(format!(" {}", escape(&e.consumer)), style::dim()),
        ]),
        DetailRow::Consumer(_, c) => {
            let idle_st = if c.pending > 0 && c.idle_ms >= crate::live::streams::STUCK_IDLE_MS {
                style::warn()
            } else {
                style::fg()
            };
            Line::from(vec![
                Span::styled(pad_r(&format!("     {}", escape(&c.name)), nw), style::fg()),
                Span::styled(
                    format!(" {}", pad_l(&c.pending.to_string(), PEND_W)),
                    style::fg(),
                ),
                Span::styled(format!(" {}", pad_l(&age(c.idle_ms), AGE_W)), idle_st),
                Span::styled(
                    format!(" {}", pad_l(&c.inactive_ms.map_or_else(dash, age), AGE_W)),
                    style::fg(),
                ),
            ])
        }
    }
}

fn group_spans(g: &GroupInfo, nw: usize) -> Vec<Span<'static>> {
    let lag = match g.lag {
        Lag::Known(n) => format::count(n),
        Lag::Unknown | Lag::Unsupported => "—".to_string(),
    };
    let lag_st = match g.lag {
        Lag::Known(n) if n >= crate::live::streams::LAGGING_LAG => style::warn(),
        _ => style::fg(),
    };
    vec![
        Span::styled(pad_r(&format!(" ▸ {}", escape(&g.name)), nw), style::fgb()),
        Span::styled(
            format!(" {}", pad_l(&g.pending.to_string(), PEND_W)),
            style::fgb(),
        ),
        Span::styled(format!(" {}", pad_l("", AGE_W)), style::fg()),
        Span::styled(format!(" {}", pad_l("", AGE_W)), style::fg()),
        Span::styled(format!(" {}", pad_l(&lag, LAG_W)), lag_st),
        Span::styled(
            format!(" {}", trunc(&escape(&g.last_delivered_id), 16)),
            style::dim(),
        ),
    ]
}

/// Why a figure is left out, for the server this stream came from.
fn left_out(info: &StreamInfo) -> Vec<String> {
    let mut out = Vec::new();
    let groups = info.groups.iter().flatten();
    if groups.clone().any(|g| g.lag == Lag::Unsupported) {
        out.push("lag: Redis 7.0 or later".to_string());
    }
    let consumers: Vec<&ConsumerInfo> = groups.flat_map(|g| g.readers.iter().flatten()).collect();
    if !consumers.is_empty() && consumers.iter().all(|c| c.inactive_ms.is_none()) {
        out.push("inactive: Redis 7.2 or later".to_string());
    }
    out.extend(info.notes.iter().cloned());
    out
}

/// Widest a form gets.
const FORM_W: u16 = 76;

/// `text` cut into lines of at most `w` columns.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for ch in text.chars() {
        if width(&line) + char_width(ch) > w {
            out.push(std::mem::take(&mut line));
        }
        line.push(ch);
    }
    out.push(line);
    out
}

/// The last `w` columns of `s`, starting with `…` when cut: the end of what is typed
/// stays in sight.
fn tail(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut keep = Vec::new();
    let mut used = 1;
    for c in chars.iter().rev() {
        if used + char_width(*c) > w {
            break;
        }
        used += char_width(*c);
        keep.push(*c);
    }
    keep.reverse();
    let mut out = String::from("…");
    out.extend(keep);
    out
}

/// Draws the form of a write action (`views::streams::form`) centred in `area`, like
/// the write gate's question: the profile on the right of its title, the fields, the
/// focused field's help, and the exact command it will send (`sends`), or what is
/// wrong with what is typed. Everything typed is escaped. Shared by the Server,
/// Streams, Pub/Sub and Search views.
pub(super) fn draw_form(
    frame: &mut Frame,
    area: Rect,
    form: &Form,
    sends: &Result<String, String>,
    ctx: &Ctx<'_>,
) {
    let w = area.width.min(FORM_W);
    let inner = usize::from(w.saturating_sub(4));
    let mut lines: Vec<Line<'static>> = Vec::new();
    for about in &form.about {
        for part in wrap(about, inner) {
            lines.push(Line::from(Span::styled(format!(" {part}"), style::fg())));
        }
    }
    if !form.about.is_empty() {
        lines.push(Line::from(""));
    }
    let lw = form
        .fields
        .iter()
        .map(|f| width(f.label))
        .max()
        .unwrap_or(0);
    for (i, f) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let mut spans = vec![
            Span::styled(if focused { " › " } else { "   " }, style::amber()),
            Span::styled(
                pad_r(f.label, lw),
                if focused { style::fgb() } else { style::dim() },
            ),
            Span::raw("  "),
        ];
        if f.is_choice() {
            for o in f.options {
                let st = if *o == f.text {
                    style::key()
                } else {
                    style::dim()
                };
                spans.push(Span::styled(format!(" {o} "), st));
                spans.push(Span::raw(" "));
            }
        } else {
            let room = inner.saturating_sub(3 + lw + 2 + 1);
            spans.push(Span::styled(tail(&escape(&f.text), room), style::fgb()));
            if focused {
                spans.push(Span::styled("▏", style::amber()));
            }
        }
        lines.push(Line::from(spans));
    }
    if let Some(help) = form.focused().map(|f| f.help).filter(|h| !h.is_empty()) {
        for part in wrap(help, inner.saturating_sub(3)) {
            lines.push(Line::from(Span::styled(format!("   {part}"), style::dim())));
        }
    }
    lines.push(Line::from(""));
    match sends {
        Ok(line) => {
            for part in wrap(&format!("Sends: {line}"), inner) {
                lines.push(Line::from(Span::styled(format!(" {part}"), style::fgb())));
            }
        }
        Err(why) => {
            for part in wrap(&format!("▲ {why}"), inner) {
                lines.push(Line::from(Span::styled(format!(" {part}"), style::amber())));
            }
        }
    }

    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let profile = ctx.profile();
    let right = format!(
        "{}{}",
        escape(&profile.name),
        if profile.env == Env::Prod {
            " · PROD"
        } else {
            ""
        }
    );
    let title = trunc(&form.title, inner.saturating_sub(width(&right) + 2));
    let body = panel(frame, rect, &title, &right);
    rows(frame, body, lines, None);
}
