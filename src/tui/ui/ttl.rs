//! Analyze › Expiry: the patterns ranked by TTL risk on the left; on the right the
//! selected pattern (its sentence, its TTL histogram) and the memory the TTLs free across
//! all patterns. One panel: it holds what the TTL ranking and the forecast were.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::analysis::{Expiry, expiry};
use crate::diff::ReportDiff;
use crate::format;
use crate::model::{PatternStats, TTL_BUCKET_LABELS};
use crate::tui::app::App;
use crate::tui::clipboard::{NO_COMMAND, suggested_command};
use crate::verdict::{TtlVerdict, ttl_verdict};

use super::analysis::{
    Col, MARK, Tier, forecast_bars, head_row, left_width, need_report, paragraph, pick_cols,
    plain_status, title_right, verdict_mark, verdict_sentence,
};
use super::mark_previous;
use super::style;
use super::widgets::{
    Pane, Title, fit, level, marquee_spans, pad_l, pad_r, pattern_spans, rows, scroll_offset,
    split_frame, trunc, ttl_hist_spans,
};

const COLS: [Col; 5] = [
    Col::new("PATTERN", 14, Tier::Base),
    Col::new("KEYS", 6, Tier::Wide),
    Col::new("NO TTL", 6, Tier::Base),
    Col::new("STATUS", 24, Tier::Wide),
    Col::new("MEMORY", 7, Tier::More),
];
const KEYS: usize = 1;
const NO_TTL: usize = 2;
const STATUS: usize = 3;
const MEMORY: usize = 4;

/// The list pane shows the Wide columns from this many cells.
const WIDE_AT: usize = 56;
/// The detail pane shows the redis-cli line and the verdict word from this many cells.
const DETAIL_WIDE: usize = 44;

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if need_report(frame, area, app, "Expiry", "ranks patterns by TTL risk.") {
        return;
    }
    let (Some(session), Some(report)) = (app.session.as_ref(), app.report()) else {
        return;
    };
    let order = app.ttl_rows();
    let more = app.more_here();
    if order.is_empty() {
        let (left, _) = split_frame(
            frame,
            area,
            left_width(area.width),
            Title::new("Expiry", &mark_previous(app, "")),
            Title::new("Details", ""),
            Pane::Left,
        );
        rows(
            frame,
            left,
            vec![
                Line::default(),
                Line::from(Span::styled(" No keys were sampled.", style::dim())),
            ],
            None,
        );
        return;
    }
    let diff = session.diff();
    let sel = app.sel.ttl.min(order.len() - 1);
    let rows_: Vec<(&PatternStats, TtlVerdict)> = order
        .iter()
        .map(|&i| {
            let p = &report.patterns[i];
            (p, ttl_verdict(p, change(diff, p)))
        })
        .collect();
    let (p, v) = rows_[sel];

    let right = title_right(app, area.width, "riskiest first");
    let kind = p.key_type.map_or("", |t| t.as_str());
    let (left, detail) = split_frame(
        frame,
        area,
        left_width(area.width),
        Title::new("Expiry", &right),
        Title::new(&p.pattern, kind),
        Pane::Left,
    );
    let (keep, name) = pick_cols(&COLS, usize::from(left.width), WIDE_AT, more);
    rows(
        frame,
        left,
        vec![head_row(&COLS, &keep, name, &[KEYS, NO_TTL, MEMORY])],
        None,
    );
    let body = Rect {
        y: left.y + 1,
        height: left.height.saturating_sub(1),
        ..left
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = rows_
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .enumerate()
        .map(|(i, (p, v))| {
            let scroll = (off + i == sel).then(|| app.sel_ticks());
            ttl_row(&keep, name, p, *v, scroll)
        })
        .collect();
    rows(frame, body, lines, Some(sel - off));

    detail_lines(frame, detail, p, v, change(diff, p), expiry(report), more);
}

fn change<'a>(
    diff: Option<&'a ReportDiff>,
    p: &PatternStats,
) -> Option<&'a crate::diff::PatternDiff> {
    diff.and_then(|d| d.pattern(&p.pattern))
}

fn ttl_row(
    keep: &[usize],
    name: usize,
    p: &PatternStats,
    v: TtlVerdict,
    scroll: Option<u64>,
) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(pattern_spans(&p.pattern, style::fg(), style::ph()));
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(spans, name, ticks),
        None => fit(spans, name),
    };
    let mark = verdict_mark(v);
    let pct = format::percent(p.no_ttl_ratio());
    for &i in &keep[1..] {
        spans.push(Span::raw(" "));
        let w = COLS[i].w;
        match i {
            KEYS => spans.push(Span::styled(
                pad_l(&format::count(p.est_keys), w),
                style::fg(),
            )),
            NO_TTL => {
                // Without the STATUS column the mark sits after the figure.
                let (text, st) = match mark {
                    Some(st) if !keep.contains(&STATUS) => (format!("{pct} {MARK}"), st),
                    Some(st) => (pct.clone(), st),
                    None => (pct.clone(), style::dim()),
                };
                spans.push(Span::styled(pad_l(&text, w), st));
            }
            STATUS => {
                let word = plain_status(v);
                let (text, st) = match mark {
                    Some(st) => (format!("{MARK} {word}"), st),
                    None => (word.to_string(), style::dim()),
                };
                spans.push(Span::styled(pad_r(&text, w), st));
            }
            MEMORY => {
                let m = p.est_bytes.map_or_else(|| "—".to_string(), format::bytes);
                spans.push(Span::styled(pad_l(&m, w), style::fg()));
            }
            _ => {}
        }
    }
    Line::from(spans)
}

/// The most common TTL bucket among keys that expire, as a range.
fn typical_ttl(p: &PatternStats) -> String {
    const RANGES: [&str; 6] = [
        "under 1m",
        "1m to 1h",
        "1h to 1d",
        "1d to 7d",
        "7d to 30d",
        "over 30d",
    ];
    p.ttl_hist[1..]
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .max_by_key(|(i, n)| (**n, std::cmp::Reverse(*i)))
        .map_or_else(|| "none expire".to_string(), |(i, _)| RANGES[i].to_string())
}

/// The widths of the seven cells of the TTL histogram in a pane `w` wide: five each when
/// they fit after a leading space, else the label and a space.
fn hist_widths(w: usize) -> [usize; 7] {
    if w >= 35 {
        return [5, 5, 5, 5, 5, 5, 5];
    }
    let mut out = [0; 7];
    for (o, l) in out.iter_mut().zip(TTL_BUCKET_LABELS) {
        *o = l.chars().count() + 1;
    }
    out
}

/// One block bar per bucket scaled to the largest, or `·` when empty, in cells of the
/// given widths: the same as [`ttl_hist_spans`] where they are five wide.
fn hist_cells(hist: &[u64; 7], widths: &[usize; 7], none_style: Style) -> Vec<Span<'static>> {
    if widths.iter().all(|w| *w == 5) {
        return ttl_hist_spans(hist, none_style);
    }
    let max = hist.iter().copied().max().unwrap_or(0);
    hist.iter()
        .zip(widths)
        .enumerate()
        .map(|(i, (&v, &w))| {
            if v == 0 {
                Span::styled(pad_r("·", w), style::track())
            } else {
                let st = if i == 0 { none_style } else { style::ok() };
                let bar = level(v, max).to_string().repeat(w.saturating_sub(2).max(1));
                Span::styled(pad_r(&bar, w), st)
            }
        })
        .collect()
}

/// `spans` without the spaces that end the last one.
fn trim_end(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    if let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end().to_string();
        last.content = trimmed.into();
    }
    spans
}

/// The right pane: the pattern's figures and sentence, its TTL histogram, and the
/// memory freed by TTL across all patterns.
fn detail_lines(
    frame: &mut Frame,
    inner: Rect,
    p: &PatternStats,
    v: TtlVerdict,
    change: Option<&crate::diff::PatternDiff>,
    all: Option<Expiry>,
    more: bool,
) {
    let w = usize::from(inner.width);
    let wide = w >= DETAIL_WIDE;
    let noun = if p.est_keys == 1 { "key" } else { "keys" };
    let mut summary = format!(" {} {noun}", format::count(p.est_keys));
    if let Some(b) = p.est_bytes {
        summary.push_str(&format!(" · {}", format::bytes(b)));
    }
    if wide {
        summary.push_str(&format!(" · {}", plain_status(v)));
    }
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(fit(vec![Span::styled(summary, style::fgb())], w)),
        Line::default(),
    ];
    let sentence = verdict_sentence(p, change);
    let (lead, lead_style) = match verdict_mark(v) {
        Some(st) => (format!(" {MARK} "), st),
        None => ("   ".to_string(), style::dim()),
    };
    lines.extend(paragraph(
        vec![Span::styled(lead, lead_style)],
        "   ",
        &sentence,
        style::fg(),
        w,
    ));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(" EXPIRES IN", style::hd())));
    // Seven cells of five when there is room (a leading space and 34 cells), else as
    // wide as each label and a space.
    let widths = hist_widths(w);
    let labels: Vec<Span<'static>> = TTL_BUCKET_LABELS
        .iter()
        .zip(&widths)
        .map(|(l, cw)| Span::styled(pad_r(l, *cw), style::dim()))
        .collect();
    let mut row = vec![Span::raw(" ")];
    row.extend(labels);
    lines.push(Line::from(fit(trim_end(row), w)));
    let mut hist = vec![Span::raw(" ")];
    hist.extend(hist_cells(
        &p.ttl_hist,
        &widths,
        verdict_mark(v).unwrap_or_else(style::amber),
    ));
    lines.push(Line::from(fit(trim_end(hist), w)));
    if more {
        lines.push(Line::default());
        lines.push(Line::from(fit(
            vec![
                Span::styled(" TYPICAL TTL ", style::dim()),
                Span::styled(typical_ttl(p), style::fgb()),
                Span::styled("   SAMPLED ", style::dim()),
                Span::styled(format::grouped(p.sampled_keys), style::fgb()),
            ],
            w,
        )));
    }
    if wide || more {
        lines.push(Line::default());
        // The same text `Y` copies. Without a command there is no `$ ` prompt, so copying
        // the line with the mouse gives nothing that looks like a command.
        let (prompt, cmd, cmd_style) = match suggested_command(&p.pattern) {
            Some(c) => (" $ ", c, style::cmd()),
            None => ("   ", NO_COMMAND.to_string(), style::dim()),
        };
        lines.push(Line::from(vec![
            Span::styled(prompt, style::dim()),
            Span::styled(trunc(&cmd, w.saturating_sub(3)), cmd_style),
        ]));
    }
    lines.push(Line::default());
    let head = if wide {
        " MEMORY FREED BY TTL  all patterns"
    } else {
        " FREED BY TTL, ALL"
    };
    lines.push(Line::from(Span::styled(head, style::hd())));
    match all {
        Some(e) => lines.extend(forecast_bars(&e, w, !wide)),
        None => lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            "No memory figures: MEMORY USAGE is not allowed for this user, or this analysis has none.",
            style::dim(),
            w,
        )),
    }
    rows(frame, inner, lines, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical_ttl_is_most_common_expiring_bucket() {
        let mut p = crate::tui::fixtures::report(true).patterns[0].clone();
        p.ttl_hist = [50, 0, 3, 9, 9, 0, 0];
        // Ties go to the shorter bucket.
        assert_eq!(typical_ttl(&p), "1h to 1d");
        p.ttl_hist = [10, 0, 0, 0, 0, 0, 0];
        assert_eq!(typical_ttl(&p), "none expire");
    }

    /// An analysis with the memory per TTL bucket and yesterday's snapshot to compare.
    fn analysed() -> crate::tui::app::App {
        use crate::tui::app::Msg;
        use crate::tui::app::views::testing;
        let mut app = testing::scanned();
        let s = app.session.as_mut().unwrap();
        let (id, scan) = (s.id, s.scan);
        let report = s.report.as_mut().unwrap();
        let taken_at_unix = report.taken_at_unix;
        let mut bytes = [0u64; 7];
        for p in &report.patterns {
            let b = p.sampled_bytes.unwrap_or(0);
            for (slot, n) in bytes.iter_mut().zip(p.ttl_hist) {
                *slot += b * n / p.sampled_keys.max(1);
            }
        }
        report.key_sizes.get_or_insert_default().ttl_bytes = bytes;
        app.apply(Msg::History {
            id,
            scan,
            taken_at_unix,
            result: Ok(Some(crate::tui::fixtures::history(true))),
        });
        testing::show(&mut app, crate::tui::app::Screen::Ttl);
        app
    }

    fn screen(app: &crate::tui::app::App, w: u16, h: u16) -> String {
        crate::tui::app::views::testing::render(app, w, h)
            .backend()
            .to_string()
    }

    /// The merged panel: patterns ranked by risk on the left, the selected one and the
    /// memory freed by TTL on the right.
    #[test]
    fn the_merged_expiry_panel_at_80x24_and_120x36() {
        let app = analysed();
        let small = screen(&app, 80, 24);
        // At 80: the ▲ mark and the share, no STATUS column; the sentence is on the right.
        assert!(small.contains("100% ▲"), "{small}");
        assert!(!small.contains("STATUS"), "{small}");
        assert!(small.contains("FREED BY TTL, ALL"), "{small}");
        insta::assert_snapshot!("expiry_merged_80x24", small);
        let wide = screen(&app, 120, 36);
        // At 120: the plain words in the status column, and the forecast bars.
        assert!(wide.contains("▲ never expires, growing"), "{wide}");
        assert!(wide.contains("MEMORY FREED BY TTL  all patterns"), "{wide}");
        assert!(wide.contains("within 1 h"), "{wide}");
        assert!(wide.contains("never "), "{wide}");
        // The CLI words stay out of the TUI.
        assert!(!wide.contains("persistent · growing"), "{wide}");
        assert!(!wide.contains("leak?"), "{wide}");
        insta::assert_snapshot!("expiry_merged_120x36", wide);
    }

    #[test]
    fn v_adds_the_memory_column_and_the_typical_ttl() {
        let mut app = analysed();
        let before = screen(&app, 120, 36);
        assert!(!before.contains("TYPICAL TTL"), "{before}");
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('v'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let more = screen(&app, 120, 36);
        assert!(more.contains("TYPICAL TTL"), "{more}");
        assert!(more.contains("MEMORY"), "{more}");
        insta::assert_snapshot!("expiry_merged_more_120x36", more);
    }

    #[test]
    fn expiry_before_an_analysis_says_what_it_needs() {
        use super::super::analysis::support::no_analysis;
        for (w, h) in [(80, 24), (120, 36)] {
            let app = no_analysis(crate::tui::app::Screen::Ttl);
            let s = screen(&app, w, h);
            assert!(s.contains("waits for an analysis"), "{s}");
            assert!(s.contains("No analysis yet."), "{s}");
            assert!(
                s.contains("Expiry ranks patterns by TTL risk. It needs a sample of the keys:"),
                "{s}"
            );
            assert!(s.contains("r  Analyze: "), "{s}");
            assert!(s.contains("is on Overview (1)"), "{s}");
            insta::assert_snapshot!(format!("expiry_no_analysis_{w}x{h}"), s);
        }
    }

    #[test]
    fn the_histogram_fits_a_narrow_pane() {
        assert_eq!(hist_widths(36), [5; 7]);
        let narrow = hist_widths(34);
        assert_eq!(narrow, [5, 4, 4, 4, 4, 5, 5]);
        assert!(narrow.iter().sum::<usize>() < 34);
    }

    #[test]
    fn the_columns_follow_the_width_and_v() {
        // 80 columns: the list pane is about 41 cells inside.
        let (keep, _) = pick_cols(&COLS, 41, WIDE_AT, false);
        assert_eq!(keep, vec![0, NO_TTL]);
        // `v` at 80 adds the keys where they fit, never at the name's cost.
        let (keep, name) = pick_cols(&COLS, 41, WIDE_AT, true);
        assert!(keep.contains(&KEYS) && name >= COLS[0].w, "{keep:?} {name}");
        // 120 columns.
        let (keep, _) = pick_cols(&COLS, 69, WIDE_AT, false);
        assert_eq!(keep, vec![0, KEYS, NO_TTL, STATUS]);
    }
}
