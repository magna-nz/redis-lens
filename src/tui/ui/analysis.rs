//! Draws [`AnalysisState`]: the Big keys panel with Recommendations beside it (spec "2
//! Analysis"), and the helpers the other Analyze panels share. Owner: wave B2, with
//! `views/analysis.rs`.
//!
//! Drawn only once the scan has finished (`ctx.report()` is `Some`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::analysis::{
    BIG_KEY_BYTES, EXPIRY_LABELS, Expiry, NEVER_EXPIRES_SHARE, Recommendation, Severity,
    length_unit, pattern_of, recommendations,
};
use crate::format;
use crate::model::{KeySample, ScanReport, Ttl};
use crate::tui::app::views::Ctx;
use crate::tui::app::views::analysis::AnalysisState;
use crate::tui::app::views::overview::{Depth, about_time, estimate_for};
use crate::tui::app::{App, Phase};
use crate::verdict::{TtlVerdict, explain};

use super::widgets::{
    Pane, Section, Title, bar, fit, marquee_spans, pad_l, pad_r, pattern_spans, rows,
    scroll_offset, sections, split_at, split_frame, trunc, width,
};
use super::{spinner, style};

// ---------------------------------------------------------------------------
// Helpers the other Analyze panels share
// ---------------------------------------------------------------------------

/// The mark of a row worth a look: amber, red for the worst. The glyph is the same
/// either way, so it reads without colour.
pub const MARK: &str = "▲";

/// When a column is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Always.
    Base,
    /// At wide widths, and with `v`.
    Wide,
    /// Only with `v`.
    More,
}

/// One column of a list: its header, width (cells, without the gap) and tier. The first
/// column of a list is the flexible name column; its `w` is the least it needs.
#[derive(Debug, Clone, Copy)]
pub struct Col {
    pub head: &'static str,
    pub w: usize,
    pub tier: Tier,
}

impl Col {
    pub const fn new(head: &'static str, w: usize, tier: Tier) -> Self {
        Self { head, w, tier }
    }
}

/// Which of `cols` (as indices, in order) to draw in a list `inner` cells wide, and the
/// width the flexible column `flex` gets (a cell of the width, and a space before it:
/// the same as every other column). [`Tier::Wide`] columns need `inner >= wide_at` or
/// `more`; [`Tier::More`] columns need `more`. When they do not all fit, the More
/// columns go first, then the Wide ones, last to first, so the flexible column keeps at
/// least `cols[flex].w` cells. A cell at the right edge stays free.
pub fn pick_flex(
    cols: &[Col],
    flex: usize,
    inner: usize,
    wide_at: usize,
    more: bool,
) -> (Vec<usize>, usize) {
    let mut keep: Vec<usize> = (0..cols.len())
        .filter(|&i| i != flex && cols[i].w > 0)
        .filter(|&i| match cols[i].tier {
            Tier::Base => true,
            Tier::Wide => more || inner >= wide_at,
            Tier::More => more,
        })
        .collect();
    let used = |keep: &[usize]| keep.iter().map(|&i| cols[i].w + 1).sum::<usize>();
    let need = cols.get(flex).map_or(0, |c| c.w);
    // One cell stays free at the right edge.
    let room = inner.saturating_sub(1);
    while !keep.is_empty() && room.saturating_sub(used(&keep)) < need {
        let at = [Tier::More, Tier::Wide, Tier::Base]
            .iter()
            .find_map(|t| keep.iter().rposition(|&i| cols[i].tier == *t))
            .unwrap_or(keep.len() - 1);
        keep.remove(at);
    }
    let width = room.saturating_sub(used(&keep));
    keep.push(flex);
    keep.sort_unstable();
    (keep, width)
}

/// [`pick_flex`] with the first column the flexible one (a name).
pub fn pick_cols(cols: &[Col], inner: usize, wide_at: usize, more: bool) -> (Vec<usize>, usize) {
    pick_flex(cols, 0, inner, wide_at, more)
}

/// The head row for the columns [`pick_flex`] chose. `right` lists the columns that
/// are right-aligned.
pub fn head_row_at(
    cols: &[Col],
    keep: &[usize],
    flex: usize,
    width: usize,
    right: &[usize],
) -> Line<'static> {
    let mut spans = Vec::new();
    for &i in keep {
        let text = if i == flex {
            pad_r(&format!(" {}", cols[i].head), width)
        } else if right.contains(&i) {
            format!(" {}", pad_l(cols[i].head, cols[i].w))
        } else {
            format!(" {}", pad_r(cols[i].head, cols[i].w))
        };
        spans.push(Span::styled(text, style::hd()));
    }
    Line::from(spans)
}

/// [`head_row_at`] with the first column the flexible one.
pub fn head_row(cols: &[Col], keep: &[usize], name: usize, right: &[usize]) -> Line<'static> {
    head_row_at(cols, keep, 0, name, right)
}

/// `text` broken at spaces into lines of at most `w` cells. A word longer than a line
/// is cut with `…`.
pub fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let word = if width(word) > w {
            trunc(word, w)
        } else {
            word.to_string()
        };
        if line.is_empty() {
            line = word;
        } else if width(&line) + 1 + width(&word) <= w {
            line.push(' ');
            line.push_str(&word);
        } else {
            out.push(std::mem::replace(&mut line, word));
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// `text` broken at spaces into lines of at most `w` cells, a word longer than a line
/// split across lines instead of cut: for the write dialogs, where a key name or a
/// command must stay readable in full.
pub fn wrap_hard(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let mut rest = word;
        loop {
            let sep = usize::from(!line.is_empty());
            if width(&line) + sep + width(rest) <= w {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(rest);
                break;
            }
            if width(rest) <= w {
                out.push(std::mem::take(&mut line));
                continue;
            }
            // A word longer than a line: fill the line, carry on with the rest.
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let mut take = String::new();
            let mut used = 0;
            for c in rest.chars() {
                let cw = super::widgets::char_width(c);
                if used + cw > w {
                    break;
                }
                take.push(c);
                used += cw;
            }
            if take.is_empty() {
                // A cell wider than the line: nothing fits, show it anyway.
                take.extend(rest.chars().next());
            }
            rest = &rest[take.len()..];
            out.push(take);
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// A paragraph as lines of `w` cells: `first` leads the first line, `rest` the others,
/// and the text in `st` follows.
pub fn paragraph(
    first: Vec<Span<'static>>,
    rest: &str,
    text: &str,
    st: ratatui::style::Style,
    w: usize,
) -> Vec<Line<'static>> {
    let lead: usize = first.iter().map(|s| width(&s.content)).sum();
    let lead = lead.max(width(rest));
    wrap(text, w.saturating_sub(lead + 1))
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let mut spans = if i == 0 {
                first.clone()
            } else {
                vec![Span::raw(rest.to_string())]
            };
            spans.push(Span::styled(t, st));
            Line::from(spans)
        })
        .collect()
}

/// The TTL verdict in plain words, for the status column. The CLI keeps
/// [`TtlVerdict::label`].
pub fn plain_status(v: TtlVerdict) -> &'static str {
    match v {
        TtlVerdict::PersistentGrowing => "never expires, growing",
        TtlVerdict::MixedLikelyBug => "some never expire",
        TtlVerdict::PersistentStable => "never expires, stable",
        TtlVerdict::ShortLived => "short-lived",
        TtlVerdict::Ok => "ok",
    }
}

/// The style of the `▲` mark of a TTL verdict: red for a pattern that never expires and
/// grows, amber where some keys never expire. `None` for a row that needs no mark.
pub fn verdict_mark(v: TtlVerdict) -> Option<ratatui::style::Style> {
    match v {
        TtlVerdict::PersistentGrowing => Some(style::warn()),
        TtlVerdict::MixedLikelyBug => Some(style::amber()),
        _ => None,
    }
}

/// The one plain sentence (or two) the detail pane says about a pattern, from the
/// verdict's own explanation.
pub fn verdict_sentence(
    p: &crate::model::PatternStats,
    change: Option<&crate::diff::PatternDiff>,
) -> String {
    explain(p, change).join(" ")
}

/// One frame holding a single section titled `title`; returns its inner area.
fn one_section(frame: &mut Frame, area: Rect, title: &str, right: &str) -> Rect {
    sections(
        frame,
        area,
        &[Section {
            title: Title::new(title, right),
            rows: None,
        }],
    )[0]
}

/// A frame with a few lines of explanation in it, for an Analyze panel that has nothing
/// to list (no earlier snapshot, a feature that is off and why).
pub fn notice(frame: &mut Frame, area: Rect, title: &str, right: &str, lines: Vec<Line<'static>>) {
    let inner = one_section(frame, area, title, right);
    let mut all = vec![Line::default()];
    all.extend(lines);
    rows(frame, inner, all, None);
}

/// The panel of an Analyze screen that has no report to show: not connected, connecting,
/// failed, analyzing, or connected without an analysis ("No analysis yet.", what `needs`
/// the sample for, `r` with its cost, and a pointer to Overview). Returns false, drawing
/// nothing, when there is a report: the caller draws its own panel.
pub fn need_report(frame: &mut Frame, area: Rect, app: &App, title: &str, needs: &str) -> bool {
    if app.report().is_some() {
        return false;
    }
    let Some(s) = &app.session else {
        let inner = one_section(frame, area, title, "");
        let line = Line::from(Span::styled(super::CONNECT_FIRST, style::dim()));
        rows(frame, inner, vec![Line::default(), line], None);
        return true;
    };
    match &s.phase {
        Phase::Connected => {
            let inner = one_section(frame, area, title, "waits for an analysis");
            let w = usize::from(inner.width).saturating_sub(1);
            let mut lines = vec![
                Line::default(),
                Line::from(Span::styled(" No analysis yet.", style::fgb())),
                Line::default(),
            ];
            let intro = format!("{title} {needs} It needs a sample of the keys:");
            lines.extend(paragraph(vec![Span::raw(" ")], " ", &intro, style::fg(), w));
            lines.push(Line::default());
            // The engine's estimate, the same one the dialog and Overview show.
            let q = estimate_for(Depth::Quick, app);
            lines.push(Line::from(vec![
                Span::styled("   r", style::amber()),
                Span::styled(
                    format!(
                        "  Analyze: {} keys, about {} read-only commands, {}",
                        format::grouped(q.keys),
                        format::count(q.commands),
                        about_time(&q)
                    ),
                    style::fg(),
                ),
            ]));
            let d = estimate_for(Depth::Deep, app);
            lines.push(Line::from(Span::styled(
                format!(
                    "      (or {} keys, {}, for rare patterns and big keys)",
                    format::grouped(d.keys),
                    about_time(&d)
                ),
                style::dim(),
            )));
            lines.push(Line::default());
            let pointer = "What the server reports without a sample is on Overview (1): the key count, the share of keys with a TTL, and what keeps memory busy.";
            lines.extend(paragraph(
                vec![Span::raw(" ")],
                " ",
                pointer,
                style::dim(),
                w,
            ));
            super::provider_notice(&mut lines, &s.profile);
            rows(frame, inner, lines, None);
        }
        Phase::Connecting => {
            let inner = one_section(frame, area, title, &s.profile.name);
            let text = format!(
                " Connecting to {} ({}) and checking permissions…",
                s.profile.name,
                s.endpoint()
            );
            let lines = vec![
                Line::default(),
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(spinner(app.tick), style::amber()),
                    Span::styled(text, style::dim()),
                ]),
            ];
            rows(frame, inner, lines, None);
        }
        Phase::Failed(e) => {
            let inner = one_section(frame, area, title, &s.profile.name);
            let lines = vec![
                Line::default(),
                Line::from(Span::styled(format!(" ✗ {e}"), style::warn())),
                Line::from(Span::styled(
                    " Press 0 to go back to Connections and try again.",
                    style::dim(),
                )),
            ];
            rows(frame, inner, lines, None);
        }
        Phase::Scanning | Phase::Done => {
            let inner = one_section(frame, area, title, "analyzing");
            let pr = s.progress.unwrap_or_default();
            let ratio = if pr.target == 0 {
                0.0
            } else {
                pr.sampled as f64 / pr.target as f64
            };
            let (fill, track) = bar(ratio, usize::from(inner.width).saturating_sub(4).min(40));
            let counts = format!(
                "{} / {}",
                format::grouped(pr.sampled),
                format::grouped(pr.target)
            );
            let lines = vec![
                Line::default(),
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(spinner(app.tick), style::amber()),
                    Span::styled(" Sampled ", style::dim()),
                    Span::styled(counts, style::fgb()),
                    Span::styled(" keys", style::dim()),
                ]),
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled(fill, style::amber()),
                    Span::styled(track, style::track()),
                ]),
            ];
            rows(frame, inner, lines, None);
        }
    }
    true
}

/// The width of the left pane of a list/details frame `total` cells wide: about half,
/// a little more when wide.
pub fn left_width(total: u16) -> u16 {
    split_at(total, if total >= 100 { 0.58 } else { 0.57 })
}

/// The right of a list pane's title: `right` after the previous-results marker, but on a
/// narrow screen (a pane too short for both) the marker alone, so it is never cut.
pub fn title_right(app: &App, screen_w: u16, right: &str) -> String {
    if screen_w < 100 && super::previous_marker(app).is_some() {
        super::mark_previous(app, "")
    } else {
        super::mark_previous(app, right)
    }
}

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &AnalysisState, ctx: &Ctx<'_>) {
    let Some(report) = ctx.report() else {
        return;
    };
    draw_big_keys(frame, area, state, report, ctx);
}

/// `right` with the previous-results marker first, as the other Analyze panels mark
/// results kept from the previous run (`ui::previous_marker`).
fn mark_previous(ctx: &Ctx<'_>, right: &str) -> String {
    let marker = ctx.session.previous.map(|_| match ctx.session.phase {
        crate::tui::app::Phase::Failed(_) => "analysis failed · previous results",
        _ => "analyzing · previous results",
    });
    match marker {
        Some(m) if right.is_empty() => m.to_string(),
        Some(m) => format!("{m} · {right}"),
        None => right.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Big keys
// ---------------------------------------------------------------------------

const BIG_COLS: [Col; 6] = [
    Col::new("KEY", 14, Tier::Base),
    Col::new("SIZE", 6, Tier::Base),
    Col::new("TYPE", 7, Tier::Wide),
    Col::new("TTL", 7, Tier::Wide),
    Col::new("PATTERN", 24, Tier::More),
    Col::new("LENGTH", 13, Tier::More),
];

/// Panes this narrow or wider show the Wide columns.
const BIG_WIDE_AT: usize = 44;

fn ttl_text(t: Ttl) -> String {
    match t {
        Ttl::Persistent => "never".to_string(),
        Ttl::Expires(d) => format::duration(d),
    }
}

fn draw_big_keys(
    frame: &mut Frame,
    area: Rect,
    state: &AnalysisState,
    report: &ScanReport,
    ctx: &Ctx<'_>,
) {
    let keys = report.big_keys();
    let sel = state.sel.min(keys.len().saturating_sub(1));
    let mut right = format!(
        "top {} of {} sampled",
        keys.len(),
        format::count(report.sampled_keys)
    );
    if state.is_loading() {
        right = format!("{} reading lengths · {right}", spinner(ctx.tick));
    }
    let selected = keys.get(sel);
    let detail_title = selected.map_or_else(String::new, |k| k.key.clone());
    let detail_right = selected.map_or("", |k| k.key_type.as_str());
    let (left, detail) = split_frame(
        frame,
        area,
        left_width(area.width),
        Title::new("Big keys", &mark_previous(ctx, &right)),
        Title::new(&detail_title, detail_right),
        Pane::Left,
    );
    if keys.is_empty() {
        let text = if report.sizes_available {
            " No sampled key had a size, so there are no big keys to rank."
        } else {
            " No sizes: MEMORY USAGE is not allowed for this user, so keys can't be ranked by size."
        };
        let w = usize::from(left.width);
        let mut lines = vec![Line::default()];
        lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            text.trim(),
            style::dim(),
            w,
        ));
        rows(frame, left, lines, None);
    } else {
        let (keep, kw) = pick_cols(&BIG_COLS, usize::from(left.width), BIG_WIDE_AT, ctx.more);
        rows(
            frame,
            left,
            vec![head_row(&BIG_COLS, &keep, kw, &[1, 3, 5])],
            None,
        );
        let body = Rect {
            y: left.y + 1,
            height: left.height.saturating_sub(1),
            ..left
        };
        let off = scroll_offset(sel, usize::from(body.height));
        let ticks = ctx.tick.saturating_sub(state.sel_since);
        let lines: Vec<Line<'static>> = keys
            .iter()
            .enumerate()
            .skip(off)
            .take(usize::from(body.height))
            .map(|(i, k)| {
                let len = length_cell(state, k, report);
                let pattern = pattern_of(report, &k.key);
                big_row(&keep, kw, k, &pattern, &len, (i == sel).then_some(ticks))
            })
            .collect();
        rows(frame, body, lines, Some(sel - off));
    }
    let recs = recommendations(report, ctx.session.diff(), &state.facts);
    let lines = match selected {
        Some(k) => key_lines(state, k, report, usize::from(detail.width)),
        None => Vec::new(),
    };
    big_detail(frame, detail, lines, state, &recs, ctx);
}

/// The LENGTH cell: the length and its unit, `…` while it is being read, `—` when it
/// is unknown.
fn length_cell(state: &AnalysisState, k: &KeySample, report: &ScanReport) -> String {
    match state.facts.get(&k.key).and_then(|f| f.length) {
        Some(n) => match length_unit(k.key_type) {
            Some(u) => format!("{} {u}", format::count(n)),
            None => format::count(n),
        },
        None if state.is_loading() && !state.loaded(report) => "…".to_string(),
        None => "—".to_string(),
    }
}

fn big_row(
    keep: &[usize],
    kw: usize,
    k: &KeySample,
    pattern: &str,
    len: &str,
    scroll: Option<u64>,
) -> Line<'static> {
    let text = vec![Span::styled(format!(" {}", k.key), style::fg())];
    let w = kw.saturating_sub(1);
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(text, w, ticks),
        None => fit(text, w),
    };
    spans.push(Span::raw(" "));
    let big = k.bytes.is_some_and(|b| b >= BIG_KEY_BYTES);
    let size = k.bytes.map_or_else(|| "—".to_string(), format::bytes);
    for &i in &keep[1..] {
        spans.push(Span::raw(" "));
        let w = BIG_COLS[i].w;
        match i {
            1 => {
                // The mark is for keys at or over the big-key limit.
                let (text, st) = if big {
                    (format!("{} {size}", MARK), style::warn())
                } else {
                    (size.clone(), style::fgb())
                };
                spans.push(Span::styled(pad_l(&text, w), st));
            }
            2 => spans.push(Span::styled(pad_r(k.key_type.as_str(), w), style::dim())),
            3 => {
                let st = match k.ttl {
                    Ttl::Persistent => style::dim(),
                    Ttl::Expires(_) => style::fg(),
                };
                spans.push(Span::styled(pad_l(&ttl_text(k.ttl), w), st));
            }
            4 => spans.extend(fit(pattern_spans(pattern, style::dim(), style::ph()), w)),
            _ => spans.push(Span::styled(pad_l(&trunc(len, w), w), style::fg())),
        }
    }
    Line::from(spans)
}

/// The facts of the selected key, as label and value lines.
fn key_lines(
    state: &AnalysisState,
    k: &KeySample,
    report: &ScanReport,
    w: usize,
) -> Vec<Line<'static>> {
    let size = k.bytes.map_or_else(|| "unknown".to_string(), format::bytes);
    let len = length_cell(state, k, report);
    let kv = |label: &str, value: String, st| {
        Line::from(fit(
            vec![
                Span::styled(format!(" {}", pad_r(label, 9)), style::dim()),
                Span::styled(value, st),
            ],
            w,
        ))
    };
    let mut lines = vec![
        kv("Size", size, style::fgb()),
        kv("Length", len, style::fg()),
        kv("TTL", ttl_text(k.ttl), style::fg()),
        kv("Pattern", pattern_of(report, &k.key), style::fg()),
    ];
    if let Some(enc) = state.facts.get(&k.key).and_then(|f| f.encoding.clone()) {
        lines.push(kv("Encoding", enc, style::fg()));
    }
    lines
}

fn rec_lines(r: &Recommendation, w: usize) -> Vec<Line<'static>> {
    let (mark, mark_style) = match r.severity {
        Severity::Warn => (MARK, style::warn()),
        Severity::Note => (MARK, style::amber()),
        Severity::Info => ("●", style::dim()),
    };
    let text = if r.hint.is_empty() {
        r.text.clone()
    } else {
        format!("{} {}", r.text, r.hint)
    };
    paragraph(
        vec![Span::styled(format!(" {mark} "), mark_style)],
        "   ",
        &text,
        style::fg(),
        w,
    )
}

fn big_detail(
    frame: &mut Frame,
    inner: Rect,
    mut lines: Vec<Line<'static>>,
    state: &AnalysisState,
    recs: &[Recommendation],
    ctx: &Ctx<'_>,
) {
    let w = usize::from(inner.width);
    let h = usize::from(inner.height);
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(" THINGS TO LOOK AT", style::hd())));
    if recs.is_empty() {
        lines.push(Line::from(Span::styled(
            " ✓ Nothing to recommend from this analysis.",
            style::ok(),
        )));
    }
    let mut notes: Vec<Line<'static>> = Vec::new();
    if let Some(e) = &state.error {
        notes.extend(paragraph(
            vec![Span::styled(" ✗ ", style::warn())],
            "   ",
            e,
            style::warn(),
            w,
        ));
    }
    for e in &state.refused {
        notes.extend(paragraph(
            vec![Span::styled(format!(" {MARK} "), style::amber())],
            "   ",
            &format!("Without it, some rules are skipped: {e}."),
            style::amber(),
            w,
        ));
    }
    if !ctx.live_ready() && state.facts.is_empty() && !state.is_loading() {
        notes.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            "Lengths and encodings are read once the live connection is open.",
            style::dim(),
            w,
        ));
    }
    let room = h.saturating_sub(lines.len() + notes.len());
    let blocks: Vec<Vec<Line<'static>>> = recs.iter().map(|r| rec_lines(r, w)).collect();
    let total: usize = blocks.iter().map(Vec::len).sum();
    // Whole recommendations only, leaving a row for "and N more" when some are cut.
    let limit = if total > room {
        room.saturating_sub(1)
    } else {
        room
    };
    let mut used = 0;
    let mut shown = 0;
    for b in &blocks {
        if used + b.len() > limit {
            break;
        }
        used += b.len();
        shown += 1;
    }
    for b in blocks.into_iter().take(shown) {
        lines.extend(b);
    }
    if shown < recs.len() {
        lines.push(Line::from(Span::styled(
            format!(" … and {} more", recs.len() - shown),
            style::dim(),
        )));
    }
    lines.extend(notes);
    rows(frame, inner, lines, None);
}

// ---------------------------------------------------------------------------
// Memory freed by TTL (the bars of the Expiry panel, `ui/ttl.rs`)
// ---------------------------------------------------------------------------

const LABEL_W: usize = 14;
const VALUE_W: usize = 7;

/// The bars of memory freed by TTL: `within 1 h` … `never`, cumulative.
pub fn forecast_bars(e: &Expiry, w: usize, short: bool) -> Vec<Line<'static>> {
    let label_w = if short { 6 } else { LABEL_W };
    let bar_w = w
        .saturating_sub(1 + label_w + 2 + VALUE_W + 2 + 5)
        .clamp(4, 40);
    let row = |label: String, v: u64, never: bool| {
        let ratio = if e.total == 0 {
            0.0
        } else {
            v as f64 / e.total as f64
        };
        let (fill, track) = bar(ratio, bar_w);
        let fill_style = if never && e.never_share() > NEVER_EXPIRES_SHARE {
            style::warn()
        } else if never {
            style::amber()
        } else {
            style::ok()
        };
        Line::from(fit(
            vec![
                Span::styled(format!(" {}", pad_r(&label, label_w)), style::fg()),
                Span::styled(fill, fill_style),
                Span::styled(format!("{track}  "), style::track()),
                Span::styled(pad_l(&format::bytes(v), VALUE_W), style::fgb()),
                Span::styled(
                    format!("  {}", pad_l(&format::percent(ratio), 4)),
                    style::dim(),
                ),
            ],
            w,
        ))
    };
    let mut bars: Vec<Line<'static>> = EXPIRY_LABELS
        .iter()
        .zip(e.within)
        .map(|(label, v)| {
            let label = if short {
                label.to_string()
            } else {
                format!("within {label}")
            };
            row(label, v, false)
        })
        .collect();
    bars.push(row("never".to_string(), e.never, true));
    bars
}

#[cfg(test)]
/// Apps and screens for the tests of the Analyze panels.
pub(super) mod support {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::tui::app::views::testing;
    use crate::tui::app::{App, Phase, Screen};

    /// Connected, no analysis yet, showing `screen`.
    pub fn no_analysis(screen: Screen) -> App {
        let mut app = testing::scanned();
        let s = app.session.as_mut().unwrap();
        s.report = None;
        s.phase = Phase::Connected;
        testing::show(&mut app, screen);
        app
    }

    /// The rows of [`super::need_report`] drawn alone into a `w` × `h` screen.
    pub fn draw_waiting(app: &App, w: u16, h: u16, title: &str, needs: &str) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| {
            let area = f.area();
            super::need_report(f, area, app, title, needs);
        })
        .unwrap();
        let buf = t.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Screen;
    use crate::tui::app::views::testing;

    const COLS: [Col; 5] = [
        Col::new("NAME", 12, Tier::Base),
        Col::new("A", 6, Tier::Base),
        Col::new("B", 8, Tier::Wide),
        Col::new("C", 10, Tier::More),
        Col::new("D", 10, Tier::More),
    ];

    #[test]
    fn little_by_default_more_with_v() {
        // Narrow: the name and the Base column only.
        let (keep, name) = pick_cols(&COLS, 40, 60, false);
        assert_eq!(keep, vec![0, 1]);
        assert_eq!(name, 40 - 1 - 7);
        // Wide adds the Wide column.
        let (keep, _) = pick_cols(&COLS, 60, 60, false);
        assert_eq!(keep, vec![0, 1, 2]);
        // `v` adds the rest when it fits.
        let (keep, name) = pick_cols(&COLS, 80, 60, true);
        assert_eq!(keep, vec![0, 1, 2, 3, 4]);
        assert_eq!(name, 80 - 1 - (7 + 9 + 11 + 11));
    }

    #[test]
    fn columns_that_do_not_fit_go_last_first() {
        // 12 for the name + 7 + 9 = 28: the More columns do not fit at 30.
        let (keep, name) = pick_cols(&COLS, 30, 20, true);
        assert_eq!(keep, vec![0, 1, 2]);
        assert!(name >= 12);
        // Not even the Base column fits.
        let (keep, name) = pick_cols(&COLS, 10, 20, true);
        assert_eq!(keep, vec![0]);
        assert_eq!(name, 9);
    }

    #[test]
    fn wrap_breaks_at_spaces_and_cuts_long_words() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("", 7), Vec::<String>::new());
        let cut = wrap("abcdefghijkl x", 5);
        assert_eq!(cut, vec!["abcd…", "x"]);
        assert!(wrap("a b c d e f", 3).iter().all(|l| width(l) <= 3));
    }

    #[test]
    fn plain_words_and_marks_follow_the_verdict() {
        assert_eq!(
            plain_status(TtlVerdict::PersistentGrowing),
            "never expires, growing"
        );
        assert_eq!(
            plain_status(TtlVerdict::MixedLikelyBug),
            "some never expire"
        );
        assert_eq!(plain_status(TtlVerdict::Ok), "ok");
        // The worst is red, the next amber, the rest unmarked.
        assert_eq!(
            verdict_mark(TtlVerdict::PersistentGrowing),
            Some(style::warn())
        );
        assert_eq!(
            verdict_mark(TtlVerdict::MixedLikelyBug),
            Some(style::amber())
        );
        for v in [
            TtlVerdict::PersistentStable,
            TtlVerdict::ShortLived,
            TtlVerdict::Ok,
        ] {
            assert_eq!(verdict_mark(v), None);
        }
    }

    #[test]
    fn wrap_hard_splits_long_words_and_keeps_every_character() {
        let name = format!("user:{}", "x".repeat(30));
        let lines = wrap_hard(&name, 12);
        assert!(lines.iter().all(|l| width(l) <= 12), "{lines:?}");
        assert_eq!(lines.concat(), name);
        assert_eq!(wrap_hard("", 4), vec![String::new()]);
        assert_eq!(wrap_hard("ab cd ef", 5), vec!["ab cd", "ef"]);
        // A word after a split one carries on on the same line.
        assert_eq!(wrap_hard("abcdefg hi", 5), vec!["abcde", "fg hi"]);
    }

    #[test]
    fn the_title_keeps_the_marker_whole_on_a_narrow_screen() {
        let (mut app, _) = testing::app_on(Screen::Ttl);
        let s = app.session.as_mut().unwrap();
        s.previous = Some(s.scan);
        s.phase = Phase::Scanning;
        let marker = super::super::previous_marker(&app).unwrap();
        assert_eq!(title_right(&app, 80, "riskiest first"), marker);
        assert_eq!(
            title_right(&app, 120, "riskiest first"),
            format!("{marker} · riskiest first")
        );
    }

    /// Big keys marks results kept from the previous run in the words of every other
    /// panel: analyzing, or the analysis failed.
    #[test]
    fn big_keys_marks_previous_results_as_the_other_panels() {
        let (mut app, _) = testing::app_on(Screen::BigKeys);
        let s = app.session.as_mut().unwrap();
        s.previous = Some(s.scan);
        s.phase = Phase::Scanning;
        let text = format!("{:?}", testing::render(&app, 120, 36).backend());
        assert!(text.contains("analyzing · previous results"), "{text}");
        assert!(!text.contains("rescan"), "{text}");
        app.session.as_mut().unwrap().phase = Phase::Failed("connection reset".into());
        let text = format!("{:?}", testing::render(&app, 120, 36).backend());
        assert!(
            text.contains("analysis failed · previous results"),
            "{text}"
        );
    }

    #[test]
    fn a_waiting_panel_says_what_it_needs_and_what_r_costs() {
        for (w, h) in [(80, 24), (120, 36)] {
            let app = support::no_analysis(Screen::BigKeys);
            let t = support::draw_waiting(&app, w, h, "Big keys", "ranks the biggest keys.");
            let text = t.join("\n");
            assert!(text.contains("No analysis yet."), "{text}");
            assert!(
                text.contains("Big keys ranks the biggest keys. It needs a sample of the keys:"),
                "{text}"
            );
            // The engine's own estimate, as the Analyze dialog shows it.
            let est = estimate_for(Depth::Quick, &app);
            assert!(
                text.contains(&format!(
                    "r  Analyze: {} keys, about {} read-only commands, {}",
                    format::grouped(est.keys),
                    format::count(est.commands),
                    about_time(&est)
                )),
                "{text}"
            );
            assert!(text.contains("is on Overview (1)"), "{text}");
            insta::assert_snapshot!(format!("no_analysis_big_keys_{w}x{h}"), t.join("\n"));
        }
    }

    #[test]
    fn the_estimate_follows_the_permissions_and_the_budget() {
        let app = support::no_analysis(Screen::Ttl);
        let quick = estimate_for(Depth::Quick, &app);
        let deep = estimate_for(Depth::Deep, &app);
        assert!(deep.commands > quick.commands);
        // A server with fewer keys than the sample costs less than the sample.
        assert!(quick.keys <= crate::scan::estimate::QUICK_SAMPLE);
        assert_eq!(
            quick.keys,
            crate::scan::estimate::QUICK_SAMPLE.min(
                app.session
                    .as_ref()
                    .unwrap()
                    .server
                    .as_ref()
                    .unwrap()
                    .total_keys
            )
        );
    }

    #[test]
    fn nothing_is_drawn_for_a_panel_that_has_its_report() {
        let (app, _) = testing::app_on(Screen::BigKeys);
        let t = support::draw_waiting(&app, 80, 24, "Big keys", "ranks the biggest keys.");
        assert!(t.iter().all(|l| l.trim().is_empty()), "{t:?}");
    }

    #[test]
    fn the_sentence_is_the_verdicts_explanation() {
        let mut p = crate::tui::fixtures::report(true).patterns[0].clone();
        p.sampled_keys = 100;
        p.no_ttl = 40;
        let s = verdict_sentence(&p, None);
        assert!(s.contains("never expire"), "{s}");
        assert!(s.contains("EXPIRE"), "{s}");
        p.no_ttl = 0;
        let s = verdict_sentence(&p, None);
        assert!(!s.contains("never expire,"), "{s}");
    }
}
