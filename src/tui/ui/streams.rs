//! Draws [`StreamsState`]. Owner: wave B, with `views/streams.rs`.
//!
//! Browse › Streams: the streams the analysis found on the left, one row each, and the
//! selected stream's groups and consumers on the right. With the entries open (`enter`)
//! they take the right pane, drawn by the key's value body
//! ([`super::value::draw_value_body`]), and the list narrows to its keys.
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

use super::patterns::wrap_words;
use super::style;
use super::widgets::{
    Pane, Title, char_width, fit, pad_l, pad_r, panel, rows, scroll_offset, split_frame, trunc,
    width,
};

const LEN_W: usize = 6;
const SIZE_W: usize = 6;
const AGE_W: usize = 6;
const GROUPS_W: usize = 3;
const LAG_W: usize = 6;
const PEND_W: usize = 6;
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
            Some("The live connection is closed. Analyze again (r) to open it.".to_string())
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

/// The list's width: 41 of 80 columns and 72 of 120 like a pattern's keys; with the
/// entries open, the narrow key column of a key's value.
fn list_width(total: u16, more: bool, entries: bool) -> u16 {
    if entries {
        super::value::list_width(total)
    } else {
        super::keys::list_width(total, more)
    }
}

/// A Browse panel with nothing to list yet: the two panes of its frame, `lines` (each
/// wrapped to the left pane) on the left under `title` and `right`, the right pane
/// empty. Shared with Search.
pub(super) fn browse_notice(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    right: &str,
    lines: &[(String, Style)],
) {
    let (left, _) = split_frame(
        frame,
        area,
        super::keys::list_width(area.width, false),
        Title::new(title, right),
        Title::new("Details", ""),
        Pane::Left,
    );
    let w = usize::from(left.width);
    let mut out = vec![Line::default()];
    for (i, (text, st)) in lines.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        for l in wrap_words(text, w) {
            out.push(Line::from(fit(vec![Span::styled(l, *st)], w)));
        }
    }
    rows(frame, left, out, None);
}

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar: the streams on the left, the selected one's groups and
/// consumers on the right, or with the entries open (`enter`) its entries there.
pub fn draw(frame: &mut Frame, area: Rect, state: &StreamsState, ctx: &Ctx<'_>) {
    const TITLE: &str = "Streams";
    if let Some(off) = &state.off {
        browse_notice(
            frame,
            area,
            TITLE,
            "off",
            &[(format!(" ▲ {off}"), style::amber())],
        );
        return;
    }
    if !state.loaded {
        let lines = match live_wait(ctx) {
            Some(text) => vec![(format!(" {text}"), style::dim())],
            None if ctx.report().is_none() => vec![
                (
                    " Streams are found by an analysis: it samples the keys and lists the \
                     streams among them."
                        .to_string(),
                    style::fg(),
                ),
                (" r analyzes.".to_string(), style::dim()),
            ],
            None => vec![(" Reading the streams with XINFO…".to_string(), style::dim())],
        };
        browse_notice(frame, area, TITLE, "", &lines);
        return;
    }
    if state.rows.is_empty() {
        browse_notice(
            frame,
            area,
            TITLE,
            "",
            &[
                (" The analysis found no streams.".to_string(), style::fgb()),
                (
                    " It samples keys, so it may miss some; a Deep analysis (r) samples more."
                        .to_string(),
                    style::dim(),
                ),
            ],
        );
        return;
    }

    let entries = state.shown_entries();
    let sel = state.sel.min(state.rows.len() - 1);
    let sel_row = &state.rows[sel];
    let mut status: Vec<&str> = Vec::new();
    if !ctx.live_ready() {
        status.push("not live");
    }
    if state.paused {
        status.push("paused");
    }
    let left_right = status.join(" · ");
    let right_title = escape(&sel_row.key);
    let right_right = match entries {
        Some(e) => super::value::title_right(e, ctx),
        None => "stream".to_string(),
    };
    let focus = if entries.is_some() || state.focus == Focus::Detail {
        Pane::Right
    } else {
        Pane::Left
    };
    let (left, right) = split_frame(
        frame,
        area,
        list_width(area.width, ctx.more, entries.is_some()),
        Title::new(TITLE, &left_right),
        Title::new(&right_title, &right_right),
        focus,
    );
    let w = usize::from(left.width);
    let c = Cols::new(w, ctx.more, entries.is_some(), ctx.session.is_cluster());

    rows(frame, left, vec![header(&c)], None);
    let body = Rect {
        y: left.y + 1,
        height: left.height.saturating_sub(2),
        ..left
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = state
        .rows
        .iter()
        .enumerate()
        .skip(off)
        .take(usize::from(body.height))
        .map(|(i, r)| {
            let node = c
                .node
                .then(|| super::keys::node_name(ctx, ctx.session.owner_of(r.key.as_bytes())));
            let line = list_row(r, &c, node);
            if entries.is_some() && i != sel {
                dim_line(line)
            } else {
                line
            }
        })
        .collect();
    rows(frame, body, lines, Some(sel - off));
    // The bottom line: a failed refresh, else where the list comes from.
    let bottom = Rect {
        y: left.y + left.height.saturating_sub(1),
        height: u16::from(left.height >= 2),
        ..left
    };
    let note = match &state.error {
        Some(e) => Span::styled(
            format!(" ▲ {e}; showing the previous reading."),
            style::amber(),
        ),
        None if state.found > state.rows.len() => Span::styled(
            format!(" first {} of {} streams", state.rows.len(), state.found),
            style::dim(),
        ),
        None => Span::styled(
            format!(
                " {} · enter lists entries",
                plural(state.rows.len(), "stream")
            ),
            style::dim(),
        ),
    };
    rows(frame, bottom, vec![Line::from(fit(vec![note], w))], None);

    match entries {
        Some(e) => super::value::draw_value_body(frame, right, e, ctx),
        None => {
            let node = ctx
                .session
                .is_cluster()
                .then(|| super::keys::node_name(ctx, ctx.session.owner_of(sel_row.key.as_bytes())));
            detail_pane(frame, right, state, node)
        }
    }
    if let (Some(open), Some(sends)) = (&state.form, state.form_sends()) {
        draw_form(frame, area, open.form(), &sends, ctx);
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{} {what}s", format::grouped(n as u64))
    }
}

/// A line drawn dim, for the list beside an open stream's entries.
fn dim_line(line: Line<'static>) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|s| Span::styled(s.content, style::dim()))
            .collect::<Vec<_>>(),
    )
}

/// The `▲` mark: a space, the mark, a space.
const MARK_W: usize = 3;
/// The narrowest the key column gets before an optional column goes.
const KEY_MIN: usize = 14;
/// From this inner width on, SIZE, OLDEST, GROUPS and PENDING show without `v`.
const WIDE_LIST: usize = 60;

/// Which columns the list shows: KEY, LEN, LAG and the `▲` of a stream worth a look
/// always (the right pane says why); NODE on a cluster, SIZE, OLDEST, GRP and PEND
/// from [`WIDE_LIST`] on or with `v`; LAST ID from [`WIDE`] on. When the key column
/// would get narrower than [`KEY_MIN`] they go: LAST ID, OLDEST, SIZE, GRP, NODE,
/// PEND. Beside the entries, only the keys.
#[derive(Debug, Clone, Copy)]
struct Cols {
    key: usize,
    only_keys: bool,
    node: bool,
    size: bool,
    age: bool,
    groups: bool,
    pend: bool,
    id: bool,
}

impl Cols {
    fn new(inner: usize, more: bool, only_keys: bool, cluster: bool) -> Self {
        if only_keys {
            return Self {
                key: inner,
                only_keys,
                node: false,
                size: false,
                age: false,
                groups: false,
                pend: false,
                id: false,
            };
        }
        let all = inner >= WIDE_LIST || more;
        let mut c = Self {
            key: 0,
            only_keys,
            node: cluster && all,
            size: all,
            age: all,
            groups: all,
            pend: all,
            id: inner >= WIDE,
        };
        for step in 0..6 {
            if inner >= c.fixed() + KEY_MIN {
                break;
            }
            // NODE late: on a cluster, where a stream lives is what to look up.
            match step {
                0 => c.id = false,
                1 => c.age = false,
                2 => c.size = false,
                3 => c.groups = false,
                4 => c.node = false,
                _ => c.pend = false,
            }
        }
        c.key = inner.saturating_sub(c.fixed()).max(8);
        c
    }

    fn fixed(&self) -> usize {
        (1 + LEN_W)
            + (1 + LAG_W)
            + MARK_W
            + if self.node { super::keys::NODE_W } else { 0 }
            + if self.size { 1 + SIZE_W } else { 0 }
            + if self.age { 1 + AGE_W } else { 0 }
            + if self.groups { 1 + GROUPS_W } else { 0 }
            + if self.pend { 1 + PEND_W } else { 0 }
            + if self.id { 1 + ID_W } else { 0 }
    }
}

fn header(c: &Cols) -> Line<'static> {
    let mut head = vec![Span::styled(pad_r(" KEY", c.key), style::hd())];
    if c.only_keys {
        return Line::from(head);
    }
    let num = |label: &str, w: usize| Span::styled(format!(" {}", pad_l(label, w)), style::hd());
    if c.node {
        head.push(Span::styled(
            format!(" {}", pad_r("NODE", super::keys::NODE_W - 1)),
            style::hd(),
        ));
    }
    head.push(num("LEN", LEN_W));
    if c.size {
        head.push(num("SIZE", SIZE_W));
    }
    if c.age {
        head.push(num("OLDEST", AGE_W));
    }
    if c.groups {
        head.push(num("GRP", GROUPS_W));
    }
    head.push(num("LAG", LAG_W));
    if c.pend {
        head.push(num("PEND", PEND_W));
    }
    if c.id {
        head.push(Span::styled(
            format!(" {}", pad_r("LAST ID", ID_W)),
            style::hd(),
        ));
    }
    Line::from(head)
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

fn list_row(r: &StreamRow, c: &Cols, node: Option<String>) -> Line<'static> {
    let dash = || "—".to_string();
    let mut spans = fit(
        vec![Span::styled(format!(" {}", escape(&r.key)), style::fg())],
        c.key,
    );
    if c.only_keys {
        return Line::from(spans);
    }
    if let Some(node) = node {
        let w = super::keys::NODE_W - 1;
        spans.push(Span::styled(
            format!(" {}", pad_r(&trunc(&node, w), w)),
            style::dim(),
        ));
    }
    let Some(i) = &r.info else {
        spans.push(Span::styled(" gone", style::dim()));
        return Line::from(spans);
    };
    let cell = |s: String, w: usize, st: Style| Span::styled(format!(" {}", pad_l(&s, w)), st);
    let lag = max_lag(i);
    spans.push(cell(format::count(i.length), LEN_W, style::fgb()));
    if c.size {
        spans.push(cell(
            i.bytes.map_or_else(dash, format::bytes),
            SIZE_W,
            style::fg(),
        ));
    }
    if c.age {
        spans.push(cell(
            i.oldest_age_ms.map_or_else(dash, age),
            AGE_W,
            style::fg(),
        ));
    }
    if c.groups {
        spans.push(cell(
            i.groups.as_ref().map_or_else(dash, |g| g.len().to_string()),
            GROUPS_W,
            style::fg(),
        ));
    }
    spans.push(cell(
        lag.map_or_else(dash, format::count),
        LAG_W,
        if lag.is_some_and(|n| n >= crate::live::streams::LAGGING_LAG) {
            style::warn()
        } else {
            style::fg()
        },
    ));
    if c.pend {
        spans.push(cell(
            total_pending(i).map_or_else(dash, format::count),
            PEND_W,
            style::fg(),
        ));
    }
    if c.id {
        spans.push(Span::styled(
            format!(" {}", pad_r(&escape(&i.last_id), ID_W)),
            style::dim(),
        ));
    }
    let v = r.verdicts.first().copied().unwrap_or(Verdict::Ok);
    if v == Verdict::Ok {
        spans.push(Span::raw(" ".repeat(MARK_W)));
    } else {
        spans.push(Span::styled(" ▲ ", verdict_style(v)));
    }
    Line::from(spans)
}

/// What a verdict means, in a sentence, for the right pane.
fn verdict_reason(v: Verdict) -> &'static str {
    match v {
        Verdict::Stuck => {
            "Stuck: a consumer holds pending entries and has been idle 30 minutes or more."
        }
        Verdict::Lagging => "Lagging: a group is 1,000 or more entries behind.",
        Verdict::NoReaders => "No readers: no consumer group reads this stream.",
        Verdict::Unbounded => {
            "Unbounded: 100,000 entries or more, the oldest over 30 days old; nothing seems \
             to trim it."
        }
        Verdict::Ok => "Nothing stands out.",
    }
}

/// The right pane without the entries: the selected stream's facts, why it is marked,
/// and its groups and consumers (→ moves the selection there).
fn detail_pane(frame: &mut Frame, inner: Rect, state: &StreamsState, node: Option<String>) {
    let Some(sel) = state.selected() else {
        return;
    };
    let w = usize::from(inner.width);
    let Some(info) = &sel.info else {
        let mut lines = vec![];
        for l in wrap_words(
            " The key is gone, or no longer a stream. r analyzes again to update the list.",
            w,
        ) {
            lines.push(Line::from(Span::styled(l, style::dim())));
        }
        rows(frame, inner, lines, None);
        return;
    };
    let focused = state.focus == Focus::Detail;
    let wide = w >= WIDE_LIST;
    let dash = || "—".to_string();

    let length = if wide {
        format::grouped(info.length)
    } else {
        format::count(info.length)
    };
    let mut facts = vec![
        Span::styled(" ", style::dim()),
        Span::styled(length, style::fgb()),
        Span::styled(" entries · ", style::dim()),
        Span::styled(info.bytes.map_or_else(dash, format::bytes), style::fgb()),
    ];
    if let Some(a) = info.oldest_age_ms {
        facts.push(Span::styled(" · oldest ", style::dim()));
        facts.push(Span::styled(format!("{} ago", age(a)), style::fgb()));
    }
    if wide {
        facts.push(Span::styled(" · last ", style::dim()));
        facts.push(Span::styled(escape(&info.last_id), style::fg()));
    }
    let mut lines = vec![Line::from(fit(facts, w))];
    if let Some(n) = node {
        lines.push(Line::from(fit(
            vec![Span::styled(format!(" on node {n}"), style::dim())],
            w,
        )));
    }
    lines.push(Line::default());
    for v in sel.verdicts.iter().filter(|v| **v != Verdict::Ok) {
        for l in wrap_words(&format!(" ▲ {}", verdict_reason(*v)), w) {
            lines.push(Line::from(Span::styled(l, verdict_style(*v))));
        }
    }
    let notes = left_out(info);
    if !notes.is_empty() {
        for l in wrap_words(&format!(" Left out: {}.", notes.join(" · ")), w) {
            lines.push(Line::from(Span::styled(l, style::dim())));
        }
    }
    if let Some(why) = state.shown_pending().and_then(|p| p.error.as_ref()) {
        for l in wrap_words(&format!(" ▲ Pending entries: {why}."), w) {
            lines.push(Line::from(Span::styled(l, style::amber())));
        }
    }
    if lines.len() > 2 {
        lines.push(Line::default());
    }
    let nw = name_width(w, wide);
    let mut head = vec![
        Span::styled(pad_r(" GROUP / CONSUMER", nw), style::hd()),
        Span::styled(format!(" {}", pad_l("PEND", PEND_W)), style::hd()),
        Span::styled(format!(" {}", pad_l("IDLE", AGE_W)), style::hd()),
    ];
    if wide {
        head.push(Span::styled(
            format!(" {}", pad_l("INACT", AGE_W)),
            style::hd(),
        ));
    }
    head.push(Span::styled(
        format!(" {}", pad_l("LAG", LAG_W)),
        style::hd(),
    ));
    if wide {
        head.push(Span::styled(" LAST DELIVERED", style::hd()));
    }
    lines.push(Line::from(head));
    let top = lines.len();

    let all = state.details();
    if all.is_empty() {
        let text = match &info.groups {
            None => " XINFO GROUPS is not available for this user.",
            Some(_) => " No consumer groups: nothing reads this stream.",
        };
        for l in wrap_words(text, w) {
            lines.push(Line::from(Span::styled(l, style::dim())));
        }
    }
    // The last row says what the keys do here.
    let h = usize::from(inner.height).saturating_sub(top + 2);
    let off = if focused {
        scroll_offset(state.detail_sel.min(all.len().saturating_sub(1)), h)
    } else {
        0
    };
    for row in all.iter().skip(off).take(h) {
        lines.push(detail_line(row, nw, wide));
    }
    let selected = focused
        .then(|| state.detail_sel.checked_sub(off).map(|i| i + top))
        .flatten();
    rows(frame, inner, lines, selected);
    let hint = if focused {
        " ↑↓ groups and consumers · ← back"
    } else {
        " → groups · enter entries"
    };
    let bottom = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: u16::from(inner.height >= 2),
        ..inner
    };
    rows(
        frame,
        bottom,
        vec![Line::from(fit(vec![Span::styled(hint, style::dim())], w))],
        None,
    );
}

/// The GROUP / CONSUMER column: what the others leave. INACT and LAST DELIVERED only
/// when `wide`.
fn name_width(inner: usize, wide: bool) -> usize {
    let fixed = if wide {
        1 + PEND_W + 1 + AGE_W + 1 + AGE_W + 1 + LAG_W + 1 + 16
    } else {
        1 + PEND_W + 1 + AGE_W + 1 + LAG_W
    };
    // One cell of margin on the right.
    inner.saturating_sub(fixed + 1).max(12)
}

fn detail_line(row: &DetailRow<'_>, nw: usize, wide: bool) -> Line<'static> {
    let dash = || "—".to_string();
    match row {
        DetailRow::Group(g) => Line::from(group_spans(g, nw, wide)),
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
            let mut spans = vec![
                Span::styled(pad_r(&format!("     {}", escape(&c.name)), nw), style::fg()),
                Span::styled(
                    format!(" {}", pad_l(&c.pending.to_string(), PEND_W)),
                    style::fg(),
                ),
                Span::styled(format!(" {}", pad_l(&age(c.idle_ms), AGE_W)), idle_st),
            ];
            if wide {
                spans.push(Span::styled(
                    format!(" {}", pad_l(&c.inactive_ms.map_or_else(dash, age), AGE_W)),
                    style::fg(),
                ));
            }
            Line::from(spans)
        }
    }
}

fn group_spans(g: &GroupInfo, nw: usize, wide: bool) -> Vec<Span<'static>> {
    let lag = match g.lag {
        Lag::Known(n) => format::count(n),
        Lag::Unknown | Lag::Unsupported => "—".to_string(),
    };
    let lag_st = match g.lag {
        Lag::Known(n) if n >= crate::live::streams::LAGGING_LAG => style::warn(),
        _ => style::fg(),
    };
    let mut spans = vec![
        Span::styled(pad_r(&format!(" ▸ {}", escape(&g.name)), nw), style::fgb()),
        Span::styled(
            format!(" {}", pad_l(&g.pending.to_string(), PEND_W)),
            style::fgb(),
        ),
        Span::styled(format!(" {}", pad_l("", AGE_W)), style::fg()),
    ];
    if wide {
        spans.push(Span::styled(format!(" {}", pad_l("", AGE_W)), style::fg()));
    }
    spans.push(Span::styled(format!(" {}", pad_l(&lag, LAG_W)), lag_st));
    if wide {
        spans.push(Span::styled(
            format!(" {}", trunc(&escape(&g.last_delivered_id), 16)),
            style::dim(),
        ));
    }
    spans
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
        // Led by the label `Sends`, as the confirm dialog shows it: `Sends   XADD …`.
        Ok(line) => {
            const LEAD: &str = " Sends   ";
            const UNDER: &str = "         ";
            let room = inner.saturating_sub(LEAD.len() + 1);
            for (i, part) in wrap(line, room).into_iter().enumerate() {
                let lead = if i == 0 { LEAD } else { UNDER };
                lines.push(Line::from(vec![
                    Span::styled(lead, style::dim()),
                    Span::styled(part, style::fgb()),
                ]));
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
