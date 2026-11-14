//! 1 Overview: "Now" (the live figures of the Server view's Overview poll) above
//! "Keys" (what the server says about its keys, then what an analysis found), and the
//! "Analyze the keys?" dialog.
//!
//! The navigation core drew this as a stub with today's content: [`draw`] is the one
//! entry point `ui::draw` calls for the Overview screen, [`draw_analyze`] the dialog's,
//! and [`analyze_prompt`] the lines a scan view shows before an analysis. The Overview
//! agent replaces the bodies; keep the three signatures.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::analysis::{Severity, recommendations};
use crate::format;
use crate::model::Profile;
use crate::tui::app::views::overview::{AnalyzeDialog, Depth, about_time, estimate};
use crate::tui::app::views::server::{HISTORY, Metric, sparkline};
use crate::tui::app::views::{Ctx, LiveState};
use crate::tui::app::{App, Phase, Session};

use super::widgets::{Section, Title, bar, dialog, fit, pad_l, pad_r, rows, sections, trunc};
use super::{CONNECT_FIRST, hh_mm, server, spinner, style};

/// The six figures "Now" shows; `v` shows all of [`Metric::ALL`].
const SHORT: [Metric; 6] = [
    Metric::Ops,
    Metric::Clients,
    Metric::Memory,
    Metric::HitRate,
    Metric::Evicted,
    Metric::Expired,
];

const LABEL_W: usize = 10;
const VALUE_W: usize = 9;
/// Rows of "Where the memory is".
const TOP_PATTERNS: usize = 3;

/// Draws Overview into `area`, the body between the tab row and the footer.
pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(ctx) = app.ctx() else {
        let inner = sections(
            frame,
            area,
            &[Section {
                title: Title::new("Overview", ""),
                rows: None,
            }],
        );
        let line = Line::from(Span::styled(CONNECT_FIRST, style::dim()));
        rows(frame, inner[0], vec![Line::default(), line], None);
        return;
    };
    let now = now_lines(&ctx, app, usize::from(area.width.saturating_sub(2)));
    let now_rows = u16::try_from(now.len()).unwrap_or(u16::MAX);
    let state = &app.views.server;
    let every = server::secs(state.interval());
    let now_right = if state.overview.poll.off.is_some() {
        "off".to_string()
    } else if state.paused {
        "INFO paused · p resumes".to_string()
    } else {
        server::right_title(state, &ctx, &every, "INFO") + " · p pauses"
    };
    let (keys, keys_right) =
        keys_lines(app, ctx.session, usize::from(area.width.saturating_sub(2)));
    let inner = sections(
        frame,
        area,
        &[
            Section {
                title: Title::new("Now", &now_right),
                rows: Some(now_rows),
            },
            Section {
                title: Title::new("Keys", &keys_right),
                rows: None,
            },
        ],
    );
    rows(frame, inner[0], now, None);
    rows(frame, inner[1], keys, None);
}

/// The "Now" lines: the figures two to a row, a cluster's shards, the server line.
fn now_lines(ctx: &Ctx<'_>, app: &App, w: usize) -> Vec<Line<'static>> {
    let state = &app.views.server;
    let o = &state.overview;
    if let Some(off) = &o.poll.off {
        return vec![Line::from(Span::styled(
            format!(" ▲ {off}"),
            style::amber(),
        ))];
    }
    if !o.poll.got {
        let text = match ctx.live() {
            LiveState::Closed(why) => format!(
                " ▲ {}",
                why.as_deref().unwrap_or("The live connection is closed.")
            ),
            LiveState::Waiting => " Waiting for the live connection…".to_string(),
            LiveState::Ready => match &o.poll.failed {
                Some(f) => format!(" ▲ {f}"),
                None => " Waiting for the server…".to_string(),
            },
        };
        return vec![Line::from(Span::styled(text, style::dim()))];
    }
    let mut lines = Vec::new();
    if let Some(t) = server::trouble(&o.poll, ctx) {
        lines.push(t);
    }
    let list: &[Metric] = if ctx.more { &Metric::ALL } else { &SHORT };
    let col = w / 2;
    let spark_w = col.saturating_sub(1 + LABEL_W + VALUE_W + 2).min(HISTORY);
    for pair in list.chunks(2) {
        let mut spans = Vec::new();
        for m in pair {
            let (text, st, of) = figure(*m, o);
            let of = of.map(|of| format!(" of {of}")).unwrap_or_default();
            let spark_w = spark_w.min(col.saturating_sub(1 + LABEL_W + VALUE_W + 2 + of.len()));
            let cell = vec![
                Span::styled(format!(" {}", pad_r(m.label(), LABEL_W)), style::dim()),
                Span::styled(pad_l(&text, VALUE_W), st),
                Span::raw(" "),
                Span::styled(sparkline(&o.history, *m, spark_w), style::ok()),
                Span::styled(of, style::dim()),
            ];
            // Cut or padded to its column.
            spans.extend(fit(cell, col));
        }
        lines.push(Line::from(fit(spans, w)));
    }
    if ctx.session.is_cluster() {
        for (i, n) in ctx.session.nodes.iter().enumerate() {
            let mark = if i == ctx.session.picked { "›" } else { " " };
            let mut spans = vec![
                Span::styled(format!(" {mark}"), style::ok()),
                Span::styled(pad_r(&n.endpoint.to_string(), 22), style::fg()),
            ];
            match state.shards.rows.get(i) {
                _ if !n.connected => spans.push(Span::styled("✗ not connected", style::warn())),
                Some(r) if r.failed => spans.push(Span::styled("✗ no answer", style::warn())),
                Some(r) => match &r.last {
                    Some(s) => spans.extend(server::shard_cells(s)),
                    None => spans.push(Span::styled("…", style::dim())),
                },
                None => spans.push(Span::styled("…", style::dim())),
            }
            lines.push(Line::from(fit(spans, w)));
        }
    }
    // The server line, with the eviction policy, as one row.
    let mut server_line = server::server_lines(o.last.as_ref(), UNCUT);
    if let Some(first) = server_line.first_mut() {
        let mut spans = std::mem::take(&mut first.spans);
        // `server_lines` pads to the width it was given: drop the padding.
        while spans.last().is_some_and(|s| s.content.trim().is_empty()) {
            spans.pop();
        }
        if let Some(p) = o.last.as_ref().and_then(|l| l.policy.clone()) {
            spans.push(Span::styled(" · ", style::dim()));
            spans.push(Span::styled(p, style::fg()));
        }
        lines.push(Line::from(fit(spans, w)));
    }
    lines
}

/// Wide enough that `server_lines` cuts nothing.
const UNCUT: usize = 400;

/// A figure of "Now": its value and style, and for memory the limit it is "of".
fn figure(
    m: Metric,
    o: &crate::tui::app::views::server::Overview,
) -> (String, ratatui::style::Style, Option<String>) {
    let last = o.last.as_ref();
    if m == Metric::Memory
        && let Some(used) = last.and_then(|l| l.used_memory)
    {
        let max = last.and_then(|l| l.maxmemory).filter(|m| *m > 0);
        let st = match max {
            Some(max) if used as f64 / max as f64 >= 0.9 => style::warn(),
            _ => style::fgb(),
        };
        return (format::bytes(used), st, max.map(format::bytes));
    }
    let (text, st) = server::metric_text(m, o);
    (text, st, None)
}

/// The "Keys" lines and the section's right title.
fn keys_lines(app: &App, s: &Session, w: usize) -> (Vec<Line<'static>>, String) {
    let mut lines = Vec::new();
    if let Some(info) = &s.server {
        lines.push(Line::from(fit(
            vec![
                Span::styled(
                    format!(" {} keys", format::count(info.total_keys)),
                    style::fgb(),
                ),
                Span::styled(" · ", style::dim()),
                Span::styled(
                    format!("{} used", format::bytes(info.used_memory)),
                    style::fg(),
                ),
            ],
            w,
        )));
        lines.push(Line::default());
    }
    let p = &s.profile;
    let report = app.report();
    let right;
    match &s.phase {
        Phase::Connecting => {
            right = String::new();
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(spinner(app.tick), style::amber()),
                Span::styled(
                    trunc(
                        &format!(
                            " Connecting to {} ({}) and checking permissions…",
                            p.name,
                            s.endpoint()
                        ),
                        w.saturating_sub(2),
                    ),
                    style::dim(),
                ),
            ]));
            if let Some(hint) = app.keychain_hint() {
                lines.push(Line::from(Span::styled(format!(" {hint}"), style::amber())));
            }
        }
        Phase::Failed(e) => {
            right = String::new();
            lines.push(Line::from(Span::styled(
                trunc(&format!(" ✗ {e}"), w),
                style::warn(),
            )));
        }
        Phase::Scanning if app.analyzing() || report.is_none() => {
            let depth = s.analysis.unwrap_or_default();
            let e = estimate(depth, p);
            right = format!("about {} commands in all", format::count(e.commands));
            let pr = s.progress.unwrap_or_default();
            let ratio = if pr.target == 0 {
                0.0
            } else {
                pr.sampled as f64 / pr.target as f64
            };
            let (fill, track) = bar(ratio, 14);
            lines.push(Line::from(fit(
                vec![
                    Span::styled(" ANALYZING", style::hd()),
                    Span::styled(
                        format!(
                            "  sampling {} of {} keys   ",
                            format::grouped(pr.sampled),
                            format::grouped(pr.target.max(e.keys))
                        ),
                        style::dim(),
                    ),
                    Span::styled(fill, style::amber()),
                    Span::styled(track, style::track()),
                ],
                w,
            )));
            lines.push(Line::from(Span::styled(
                trunc(" Browse and Analyze fill in as it goes.  esc stops it.", w),
                style::dim(),
            )));
            if let Some(r) = report {
                lines.push(Line::default());
                findings(&mut lines, app, s, r, w);
            }
        }
        _ => match report {
            Some(r) => {
                right = format!(
                    "{} sampled keys · {}",
                    format::grouped(r.sampled_keys),
                    hh_mm(r.taken_at_unix)
                );
                findings(&mut lines, app, s, r, w);
            }
            None => {
                right = "no keys read".to_string();
                lines.push(Line::from(vec![
                    Span::styled(" WHAT USES THE MEMORY", style::hd()),
                    Span::styled("  needs a sample of the keys", style::dim()),
                ]));
                lines.extend(analyze_prompt(p, ""));
            }
        },
    }
    super::provider_notice(&mut lines, p);
    (lines, right)
}

/// "Things to look at" (the recommendations, `app.overview.sel` marked) and "Where
/// the memory is" (the biggest patterns).
fn findings(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    s: &Session,
    r: &crate::model::ScanReport,
    w: usize,
) {
    let recs = recommendations(r, s.diff(), &app.views.analysis.facts);
    lines.push(Line::from(Span::styled(" THINGS TO LOOK AT", style::hd())));
    if recs.is_empty() {
        lines.push(Line::from(Span::styled(
            "   Nothing stands out in the sample.",
            style::dim(),
        )));
    }
    let sel = app.overview.sel.min(recs.len().saturating_sub(1));
    for (i, rec) in recs.iter().enumerate() {
        let (mark, st) = match rec.severity {
            Severity::Warn => ("▲", style::warn()),
            Severity::Note => ("▲", style::amber()),
            Severity::Info => ("●", style::dim()),
        };
        let cursor = if i == sel { " › " } else { "   " };
        lines.push(Line::from(fit(
            vec![
                Span::styled(cursor, style::amber()),
                Span::styled(format!("{mark} "), st),
                Span::styled(rec.text.clone(), style::fg()),
            ],
            w,
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " WHERE THE MEMORY IS",
        style::hd(),
    )));
    let total: u64 = r
        .patterns
        .iter()
        .map(|p| p.est_bytes.unwrap_or(p.est_keys))
        .sum();
    for p in r.patterns.iter().take(TOP_PATTERNS) {
        let v = p.est_bytes.unwrap_or(p.est_keys);
        let ratio = if total == 0 {
            0.0
        } else {
            v as f64 / total as f64
        };
        let (fill, track) = bar(ratio, 16);
        let size = match p.est_bytes {
            Some(b) => format::bytes(b),
            None => format!("{} keys", format::count(p.est_keys)),
        };
        lines.push(Line::from(fit(
            vec![
                Span::raw("   "),
                Span::styled(pad_r(&p.pattern, 18), style::fg()),
                Span::raw(" "),
                Span::styled(fill, style::amber()),
                Span::styled(track, style::track()),
                Span::styled(pad_l(&format::percent(ratio), 5), style::fgb()),
                Span::styled(pad_l(&size, 7), style::fg()),
            ],
            w,
        )));
    }
}

/// The lines a screen that needs an analysis shows before one: what `r` would cost.
/// `title` names the screen (empty on Overview itself).
pub fn analyze_prompt(p: &Profile, title: &str) -> Vec<Line<'static>> {
    let e = estimate(Depth::Quick, p);
    let mut lines = Vec::new();
    if !title.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" No analysis yet: {title} needs a sample of the keys."),
            style::dim(),
        )));
    }
    lines.push(Line::from(vec![
        Span::styled("   r", style::amber()),
        Span::styled(
            format!(
                "  Analyze: {} keys, about {} read-only commands, {}.",
                format::grouped(e.keys),
                format::count(e.commands),
                about_time(&e)
            ),
            style::fg(),
        ),
    ]));
    lines
}

/// Draws "Analyze the keys?" centred in `area`, over the dimmed screen.
pub fn draw_analyze(frame: &mut Frame, area: Rect, d: &AnalyzeDialog, app: &App) {
    let Some(s) = &app.session else {
        return;
    };
    let p = &s.profile;
    let right = format!("{} · {}", p.name, p.env.label());
    let w = area.width.min(72);
    let inner_w = usize::from(w.saturating_sub(2));
    let mut lines = vec![
        Line::from(Span::styled(
            trunc(
                " Sampling reads a few commands per key; nothing is written.",
                inner_w,
            ),
            style::fg(),
        )),
        Line::default(),
    ];
    for depth in Depth::ALL {
        let e = estimate(depth, p);
        let mark = if depth == d.pick { " › " } else { "   " };
        let st = if depth == d.pick {
            style::fgb()
        } else {
            style::fg()
        };
        lines.push(Line::from(fit(
            vec![
                Span::styled(mark, style::amber()),
                Span::styled(pad_r(depth.label(), 8), st),
                Span::styled(pad_r(&format!("{} keys", format::grouped(e.keys)), 16), st),
                Span::styled(
                    pad_r(&format!("about {} commands", format::count(e.commands)), 22),
                    style::fg(),
                ),
                Span::styled(about_time(&e), style::fg()),
            ],
            inner_w,
        )));
    }
    lines.push(Line::default());
    let limit = match estimate(Depth::Quick, p).per_sec {
        Some(n) => format!(
            " Both stay under this profile's limit of {} commands/s, and esc stops either one.",
            format::grouped(u64::from(n))
        ),
        None => " This profile has no command limit, and esc stops either one.".to_string(),
    };
    lines.push(Line::from(Span::styled(
        trunc(" Deep finds rare patterns and rare big keys.", inner_w),
        style::dim(),
    )));
    for chunk in wrap(&limit, inner_w) {
        lines.push(Line::from(Span::styled(chunk, style::dim())));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" enter", style::amber()),
        Span::styled(" Start   ", style::fg()),
        Span::styled("↑↓", style::amber()),
        Span::styled(" Pick   ", style::fg()),
        Span::styled("esc", style::amber()),
        Span::styled(" Cancel", style::fg()),
    ]));
    let h = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let body = dialog(frame, area, w, h, "Analyze the keys?", &right);
    rows(frame, body, lines, None);
}

/// `text` in lines at most `w` wide, broken at spaces; each line after the first
/// starts with a space, as the first does.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(8);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && super::widgets::width(&line) + 1 + super::widgets::width(word) > w {
            out.push(std::mem::take(&mut line));
        }
        line.push(' ');
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(trunc(&line, w));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::server::Reply;
    use crate::live::server::{InfoSample, Role};
    use crate::live::{LiveReply, LiveRequest};
    use crate::tui::app::views::testing;
    use crate::tui::app::{Effect, Msg, Screen};
    use crate::tui::fixtures;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::{Duration, Instant};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    /// An INFO taken `t` seconds into a steady run.
    fn sample(t: u64) -> InfoSample {
        InfoSample {
            uptime_secs: Some(1000 + t),
            role: Some(Role::Primary),
            replicas: Some(1),
            clients: Some(10 + t % 3),
            blocked: Some(0),
            used_memory: Some(1_000_000 + t * 1000),
            maxmemory: Some(4_000_000),
            fragmentation: Some(1.1),
            policy: Some("allkeys-lfu".into()),
            commands: Some(10_000 + 100 * t),
            hits: Some(900 + 9 * t),
            misses: Some(100 + t),
            net_in: Some(50_000 + 1024 * t),
            net_out: Some(80_000 + 2048 * t),
            evicted: Some(t / 2),
            expired: Some(5 * t),
            cpu_user: Some(10.0 + 0.05 * t as f64),
            cpu_sys: Some(4.0 + 0.01 * t as f64),
            rdb_ok: Some(true),
            aof_on: Some(false),
            ..InfoSample::default()
        }
    }

    /// Answers the Overview's INFO polls in `effects` and the next few, so "Now" has
    /// rates and a short history.
    fn feed_info(app: &mut App, mut effects: Vec<Effect>) {
        let t0 = Instant::now();
        for t in 0..4u64 {
            let at = t0 + Duration::from_secs(2 * t);
            effects.extend(app.apply(Msg::TickAt(at)));
            for (id, req) in testing::requests(&effects) {
                if matches!(req, LiveRequest::Server(_)) {
                    testing::reply(
                        app,
                        id,
                        Ok(LiveReply::Server(Reply::Info {
                            at,
                            sample: Box::new(sample(2 * t)),
                        })),
                    );
                }
            }
            effects.clear();
        }
    }

    /// prod-cache-01 connected, the live connection open, on Overview; `analyzed` with
    /// the fixture scan finished.
    fn overview_app(analyzed: bool) -> App {
        let mut app = if analyzed {
            testing::scanned()
        } else {
            let mut app = App::new(fixtures::profiles());
            let effects = app.handle_key(key(KeyCode::Enter));
            let [Effect::Connect { id, scan, .. }] = effects[..] else {
                panic!("expected a connect, got {effects:?}");
            };
            app.apply(Msg::Connected {
                id,
                scan,
                server: Some(fixtures::server()),
                perms: fixtures::perms_with_probes(),
                target: crate::tui::ScanTarget::Primary,
            });
            app
        };
        let mut out = testing::live_ready(&mut app);
        out.extend(testing::show(&mut app, Screen::Overview));
        feed_info(&mut app, out);
        app
    }

    fn snap(app: &App, w: u16, h: u16) -> String {
        testing::render(app, w, h).backend().to_string()
    }

    #[test]
    fn overview_before_an_analysis() {
        let app = overview_app(false);
        insta::assert_snapshot!("before_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("before_120x36", snap(&app, 120, 36));
    }

    #[test]
    fn overview_after_an_analysis() {
        let app = overview_app(true);
        insta::assert_snapshot!("after_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("after_120x36", snap(&app, 120, 36));
    }

    #[test]
    fn overview_with_all_figures() {
        let mut app = overview_app(true);
        app.handle_key(key(KeyCode::Char('v')));
        insta::assert_snapshot!("more_120x36", snap(&app, 120, 36));
    }

    #[test]
    fn the_analyze_dialog() {
        let mut app = overview_app(false);
        app.handle_key(key(KeyCode::Char('r')));
        insta::assert_snapshot!("analyze_80x24", snap(&app, 80, 24));
        app.handle_key(key(KeyCode::Down));
        insta::assert_snapshot!("analyze_deep_120x36", snap(&app, 120, 36));
    }

    #[test]
    fn help_opened_and_filtered() {
        let mut app = overview_app(true);
        app.handle_key(key(KeyCode::Char('?')));
        insta::assert_snapshot!("help_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("help_120x36", snap(&app, 120, 36));
        for c in "slow".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Down));
        insta::assert_snapshot!("help_slow_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("help_slow_120x36", snap(&app, 120, 36));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for analyzed in [false, true] {
            let mut app = overview_app(analyzed);
            for (w, h) in [(1, 1), (0, 0), (20, 5), (40, 12), (200, 3), (3, 40)] {
                testing::render(&app, w, h);
            }
            app.handle_key(key(KeyCode::Char('r')));
            for (w, h) in [(1, 1), (0, 0), (20, 5), (40, 12), (200, 3), (3, 40)] {
                testing::render(&app, w, h);
            }
        }
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        assert_eq!(
            wrap("one two three four", 10),
            [" one two", " three", " four"]
        );
        assert!(wrap("", 10).is_empty());
    }
}
