//! State of the Streams view (spec "4 Streams"). Owner: wave B. Drawn by
//! `ui/streams.rs`; its live requests are in `src/live/streams.rs`.
//!
//! Lists the streams the last scan found and polls `XINFO` for them every 5 s while the
//! view is shown (`p` pauses). Reads entry IDs only, never entry values (spec "Values"
//! is phase 2). See [`super`] for the interface `App` calls.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{Act, ViewState};
use crate::live::streams::{
    ConsumerInfo, GroupInfo, MAX_STREAMS, Refused, Reply, Request, StreamInfo, Verdict, verdicts,
};
use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{KeyType, PatternStats, ScanReport};

/// How often `XINFO` is polled while the view is shown (spec "Shared command budget").
pub const POLL: Duration = Duration::from_secs(5);
/// Rows of the group and consumer list in the details panel that fit at once.
const DETAIL_PAGE: usize = 5;

/// What `r`, `p` and the list keys act on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    List,
    /// The groups and consumers of the selected stream.
    Detail,
}

/// One stream of the list, with what the last poll said about it.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRow {
    pub key: String,
    /// `None`: the key is gone, or no longer a stream, since the scan.
    pub info: Option<StreamInfo>,
    /// Every verdict that applies; the first one is shown in the list.
    pub verdicts: Vec<Verdict>,
}

/// A line of the details panel: a group, or one of its consumers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetailRow<'a> {
    Group(&'a GroupInfo),
    Consumer(&'a GroupInfo, &'a ConsumerInfo),
}

/// The groups of `info`, each followed by its consumers.
pub fn detail_rows(info: &StreamInfo) -> Vec<DetailRow<'_>> {
    let mut out = Vec::new();
    for g in info.groups.iter().flatten() {
        out.push(DetailRow::Group(g));
        for c in g.readers.iter().flatten() {
            out.push(DetailRow::Consumer(g, c));
        }
    }
    out
}

/// The stream list, its groups and consumers.
#[derive(Debug, Default)]
pub struct StreamsState {
    /// The last poll's answer, in scan order.
    pub rows: Vec<StreamRow>,
    /// How many streams the scan found; more than `rows` when cut at [`MAX_STREAMS`].
    pub found: usize,
    /// Selected row of the list.
    pub sel: usize,
    /// Selected row of the details panel.
    pub detail_sel: usize,
    pub focus: Focus,
    pub paused: bool,
    /// An answer has arrived (so an empty list means no streams, not "not read yet").
    pub loaded: bool,
    /// Why the view is off, in redis-lens's own words. A refused or unsupported
    /// `XINFO STREAM` turns it off until `r`.
    pub off: Option<String>,
    /// Why the last poll failed. The previous rows stay.
    pub error: Option<String>,
    pending: Option<ReqId>,
    last_sent: Option<Instant>,
    /// Optional commands the server refused, not asked again until `r`.
    refused: Refused,
    /// What the request in flight was told to skip.
    refused_sent: Refused,
    /// `r` was pressed while a poll was in flight: that poll's answer skipped what `r`
    /// asks again, so its refusals are not kept, and one more read follows it.
    reread: bool,
}

/// The keys of the streams the scan found: keys of type stream among the examples of every
/// pattern, and a pattern that is a literal key of type stream. A sampled scan keeps up to
/// five examples per pattern, so this may miss some.
pub fn stream_keys(report: &ScanReport) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in &report.patterns {
        for k in pattern_streams(p) {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

fn pattern_streams(p: &PatternStats) -> Vec<String> {
    let mut keys: Vec<String> = p
        .examples
        .iter()
        .filter(|e| e.key_type == KeyType::Stream)
        .map(|e| e.key.clone())
        .collect();
    // A pattern without placeholders or globs names one key.
    let literal = !p.pattern.contains(['{', '}', '*', '?', '[']);
    if keys.is_empty() && p.key_type == Some(KeyType::Stream) && literal {
        keys.push(p.pattern.clone());
    }
    keys
}

impl StreamsState {
    /// The selected stream.
    pub fn selected(&self) -> Option<&StreamRow> {
        self.rows.get(self.sel)
    }

    /// The details rows of the selected stream.
    pub fn details(&self) -> Vec<DetailRow<'_>> {
        self.selected()
            .and_then(|r| r.info.as_ref())
            .map(detail_rows)
            .unwrap_or_default()
    }

    fn fetch(&mut self, act: &mut Act<'_>) {
        let Some(report) = act.report() else {
            return;
        };
        let keys = stream_keys(report);
        self.found = keys.len();
        self.last_sent = act.now;
        if keys.is_empty() {
            // Nothing to ask the server.
            self.rows.clear();
            self.sel = 0;
            self.loaded = true;
            return;
        }
        let keys: Vec<String> = keys.into_iter().take(MAX_STREAMS).collect();
        self.refused_sent = self.refused;
        self.reread = false;
        self.pending = act.send(LiveRequest::Streams(Request::Info {
            keys,
            refused: self.refused,
        }));
    }

    fn in_flight(&mut self, act: &Act<'_>) -> bool {
        match self.pending {
            Some(id) if act.in_flight(id) => true,
            Some(_) => {
                self.pending = None;
                false
            }
            None => false,
        }
    }

    fn apply(&mut self, list: Vec<crate::live::streams::Fetched>) {
        let selected = self.selected().map(|r| r.key.clone());
        self.rows = list
            .into_iter()
            .map(|f| {
                let verdicts = f.info.as_ref().map(verdicts).unwrap_or_default();
                StreamRow {
                    key: f.key,
                    info: f.info,
                    verdicts,
                }
            })
            .collect();
        self.sel = selected
            .and_then(|k| self.rows.iter().position(|r| r.key == k))
            .unwrap_or(self.sel)
            .min(self.rows.len().saturating_sub(1));
        self.detail_sel = self.detail_sel.min(self.details().len().saturating_sub(1));
        self.loaded = true;
        self.error = None;
    }
}

impl ViewState for StreamsState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        if self.off.is_none() && !self.in_flight(act) && (!self.paused || !self.loaded) {
            self.fetch(act);
        }
    }

    fn on_tick(&mut self, now: Instant, act: &mut Act<'_>) {
        if self.off.is_some() || self.paused || !act.live_ready() || self.in_flight(act) {
            return;
        }
        if self.last_sent.is_none_or(|t| now.duration_since(t) >= POLL) {
            self.fetch(act);
            self.last_sent = Some(now);
        }
    }

    fn refresh(&mut self, act: &mut Act<'_>) {
        // `r` tries again after the view turned itself off, and asks again for what was
        // refused (the ACL may have changed).
        self.off = None;
        self.refused = Refused::default();
        if self.in_flight(act) {
            self.reread = true;
        } else {
            self.fetch(act);
        }
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        match key.code {
            KeyCode::Char('p') if plain(&key) => {
                self.paused = !self.paused;
                if self.paused {
                    act.info("Polling paused. Press p to resume, r to refresh once.");
                } else {
                    act.info("Polling resumed.");
                    // Read at once rather than after the rest of the interval.
                    self.last_sent = None;
                }
                true
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::List => Focus::Detail,
                    Focus::Detail => Focus::List,
                };
                true
            }
            _ => {
                let (cur, len, page) = match self.focus {
                    Focus::List => (self.sel, self.rows.len(), act.page),
                    Focus::Detail => (self.detail_sel, self.details().len(), DETAIL_PAGE),
                };
                let Some(to) = list_move(&key, cur, len, page) else {
                    return false;
                };
                match self.focus {
                    Focus::List => {
                        if to != self.sel {
                            self.detail_sel = 0;
                        }
                        self.sel = to;
                    }
                    Focus::Detail => self.detail_sel = to,
                }
                true
            }
        }
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        // Only the answer to the request in flight; an older one is stale.
        if self.pending != Some(id) {
            return;
        }
        self.pending = None;
        match result {
            Ok(LiveReply::Streams(Reply::Info { streams, refused })) => {
                self.refused = if self.reread {
                    // Keep only what this poll found refused; what it skipped is asked again.
                    let sent = self.refused_sent;
                    Refused {
                        time: refused.time && !sent.time,
                        groups: refused.groups && !sent.groups,
                        consumers: refused.consumers && !sent.consumers,
                        memory: refused.memory && !sent.memory,
                    }
                } else {
                    refused
                };
                self.apply(streams);
            }
            Ok(_) => {}
            Err(e @ (LiveError::Denied(_) | LiveError::Unsupported(_))) => {
                self.off = Some(off_text(&e));
            }
            // `App` says the connection closed.
            Err(LiveError::Closed) => {}
            Err(LiveError::Failed(why)) => self.error = Some(why),
        }
        if self.reread && self.off.is_none() {
            self.fetch(act);
        }
    }
}

/// Why a view is off, in redis-lens's own words ([`LiveError`]'s texts never hold the
/// server's).
pub(super) fn off_text(e: &LiveError) -> String {
    let why = e.to_string();
    let mut c = why.chars();
    let first = c.next().map(|f| f.to_uppercase().to_string());
    format!("{}{}.", first.unwrap_or_default(), c.as_str())
}

/// A key with no Ctrl or Alt held.
pub(super) fn plain(key: &KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Where a move key takes the selection `cur` of a list of `len` rows: ↑↓ j k one row,
/// PgUp PgDn `page` rows, Home End g G the ends, clamped. `None` for any other key.
pub(super) fn list_move(key: &KeyEvent, cur: usize, len: usize, page: usize) -> Option<usize> {
    if !plain(key) {
        return None;
    }
    let last = len.saturating_sub(1);
    Some(match key.code {
        KeyCode::Up | KeyCode::Char('k') => cur.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => cur.saturating_add(1).min(last),
        KeyCode::PageUp => cur.saturating_sub(page),
        KeyCode::PageDown => cur.saturating_add(page).min(last),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::testing::{self, key};
    use super::*;
    use crate::live::streams::{Fetched, Lag};
    use crate::tui::app::{App, Effect, Msg, Screen};
    use crossterm::event::KeyCode;

    /// A poll's answer with nothing refused.
    fn poll_reply(streams: Vec<crate::live::streams::Fetched>) -> Reply {
        Reply::Info {
            streams,
            refused: Refused::default(),
        }
    }

    fn stream_pattern(pattern: &str, examples: &[&str]) -> PatternStats {
        let mut p = crate::tui::fixtures::report(true).patterns[0].clone();
        p.pattern = pattern.into();
        p.key_type = Some(KeyType::Stream);
        p.examples = examples
            .iter()
            .map(|k| crate::model::KeySample {
                key: (*k).into(),
                key_type: KeyType::Stream,
                bytes: Some(1024),
                ttl: crate::model::Ttl::Persistent,
                lfu_freq: None,
                node: None,
            })
            .collect();
        p
    }

    /// The fixture scan plus the streams `orders`, `events:{id}` (two keys) and a
    /// non-stream pattern with a stream among its examples.
    fn with_streams(app: &mut App) {
        let report = app
            .session
            .as_mut()
            .and_then(|s| s.report.as_mut())
            .expect("a report");
        report.patterns.push(stream_pattern("orders", &[]));
        report
            .patterns
            .push(stream_pattern("events:{id}", &["events:1", "events:2"]));
    }

    fn info(key: &str, length: u64, groups: Option<Vec<GroupInfo>>) -> StreamInfo {
        StreamInfo {
            key: key.into(),
            length,
            bytes: Some(2 * 1024 * 1024),
            first_id: Some("1700000000000-0".into()),
            last_id: "1700000900000-0".into(),
            oldest_age_ms: Some(3 * 60 * 60 * 1000),
            entries_added: Some(length + 4),
            groups,
            notes: Vec::new(),
        }
    }

    fn consumer(name: &str, pending: u64, idle_ms: u64) -> ConsumerInfo {
        ConsumerInfo {
            name: name.into(),
            pending,
            idle_ms,
            inactive_ms: Some(idle_ms),
        }
    }

    fn group(name: &str, pending: u64, lag: Lag, readers: Vec<ConsumerInfo>) -> GroupInfo {
        GroupInfo {
            name: name.into(),
            consumers: readers.len() as u64,
            pending,
            last_delivered_id: "1700000800000-0".into(),
            lag,
            readers: Some(readers),
        }
    }

    pub(super) fn fetched() -> Vec<Fetched> {
        vec![
            Fetched {
                key: "orders".into(),
                info: Some(info(
                    "orders",
                    120_000,
                    Some(vec![
                        group(
                            "billing",
                            3,
                            Lag::Known(2_500),
                            vec![consumer("b-1", 3, 45 * 60 * 1000), consumer("b-2", 0, 1500)],
                        ),
                        group("audit", 0, Lag::Known(0), vec![consumer("a-1", 0, 20)]),
                    ]),
                )),
            },
            Fetched {
                key: "events:1".into(),
                info: Some(info("events:1", 42, Some(vec![]))),
            },
            Fetched {
                key: "events:2".into(),
                info: None,
            },
        ]
    }

    fn at(base: Instant, ms: u64) -> Msg {
        Msg::TickAt(base + Duration::from_millis(ms))
    }

    /// An app on Streams with the live connection ready and the streams above found by
    /// the scan, and the request on show answered with [`fetched`].
    fn loaded() -> (App, Instant) {
        let t0 = Instant::now();
        let mut app = testing::scanned();
        with_streams(&mut app);
        testing::live_ready(&mut app);
        app.apply(Msg::TickAt(t0));
        let effects = testing::show(&mut app, Screen::Streams);
        let reqs = testing::requests(&effects);
        let [(id, LiveRequest::Streams(Request::Info { .. }))] = &reqs[..] else {
            panic!("expected one Info request, got {reqs:?}");
        };
        testing::reply(&mut app, *id, Ok(LiveReply::Streams(poll_reply(fetched()))));
        (app, t0)
    }

    fn answer(app: &mut App, id: ReqId) {
        testing::reply(app, id, Ok(LiveReply::Streams(poll_reply(fetched()))));
    }

    /// Ticks to `ms` after `t0`; the request that sent, if any.
    fn tick(app: &mut App, t0: Instant, ms: u64) -> Option<ReqId> {
        let reqs = testing::requests(&app.apply(at(t0, ms)));
        assert!(reqs.len() <= 1, "{reqs:?}");
        reqs.first().map(|r| r.0)
    }

    #[test]
    fn finds_the_streams_of_the_scan() {
        let mut app = testing::scanned();
        with_streams(&mut app);
        let report = app.session.as_ref().unwrap().report.as_ref().unwrap();
        // `orders` is a literal key; `events:{id}` is read through its examples.
        assert_eq!(stream_keys(report), ["orders", "events:1", "events:2"]);
        // A pattern with a placeholder and no examples names no key.
        let mut p = stream_pattern("jobs:{id}", &[]);
        p.examples.clear();
        assert!(pattern_streams(&p).is_empty());
        // A stream among the examples of a mixed pattern counts.
        let mut mixed = stream_pattern("m:{id}", &["m:1"]);
        mixed.key_type = None;
        assert_eq!(pattern_streams(&mixed), ["m:1"]);
    }

    #[test]
    fn on_show_asks_for_the_streams_and_nothing_else() {
        let mut app = testing::scanned();
        with_streams(&mut app);
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::Streams);
        let reqs = testing::requests(&effects);
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].1,
            LiveRequest::Streams(Request::Info {
                keys: vec!["orders".into(), "events:1".into(), "events:2".into()],
                refused: Refused::default(),
            })
        );
    }

    #[test]
    fn nothing_is_asked_when_the_scan_found_no_streams() {
        let (mut app, effects) = testing::app_on(Screen::Streams);
        assert!(testing::requests(&effects).is_empty());
        assert!(app.views.streams.loaded);
        let t0 = Instant::now();
        app.apply(Msg::TickAt(t0));
        assert_eq!(tick(&mut app, t0, 6_000), None);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn polls_every_five_seconds_while_shown() {
        let (mut app, t0) = loaded();
        assert_eq!(tick(&mut app, t0, 4_900), None, "before 5 s");
        let id = tick(&mut app, t0, 5_000).expect("a poll at 5 s");
        // One request at a time: a slow answer is not polled again on top of it.
        assert_eq!(tick(&mut app, t0, 11_000), None);
        answer(&mut app, id);
        // The next one is due 5 s after the last was sent, and it is long past.
        assert!(tick(&mut app, t0, 11_100).is_some());
    }

    #[test]
    fn p_pauses_and_resumes_polling() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.views.streams.paused);
        assert_eq!(tick(&mut app, t0, 60_000), None);
        // `r` still reads once.
        let effects = app.handle_key(key(KeyCode::Char('r')));
        let [(id, _)] = testing::requests(&effects)[..] else {
            panic!("expected one request");
        };
        answer(&mut app, id);
        // Resuming reads at once.
        app.handle_key(key(KeyCode::Char('p')));
        assert!(!app.views.streams.paused);
        assert!(tick(&mut app, t0, 60_100).is_some());
    }

    #[test]
    fn r_during_a_poll_asks_again_for_what_was_refused() {
        let (mut app, t0) = loaded();
        // The next poll is told that XINFO GROUPS was refused.
        app.views.streams.refused.groups = true;
        let id = tick(&mut app, t0, 5_100).expect("poll");
        // `r` while it is in flight sends nothing yet.
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert!(testing::requests(&effects).is_empty());
        // That poll skipped GROUPS, so its answer still says refused.
        let effects = testing::reply(
            &mut app,
            id,
            Ok(LiveReply::Streams(Reply::Info {
                streams: fetched(),
                refused: Refused {
                    groups: true,
                    memory: true,
                    ..Refused::default()
                },
            })),
        );
        // One more read follows at once, asking for GROUPS again; MEMORY USAGE, newly
        // refused by that poll, stays skipped.
        let reqs = testing::requests(&effects);
        let [(_, LiveRequest::Streams(Request::Info { refused, .. }))] = &reqs[..] else {
            panic!("expected one Info request, got {reqs:?}");
        };
        assert_eq!(
            *refused,
            Refused {
                memory: true,
                ..Refused::default()
            }
        );
    }

    #[test]
    fn does_not_poll_while_hidden() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(tick(&mut app, t0, 60_000), None);
        // Back on screen it reads at once.
        let effects = app.handle_key(key(KeyCode::Char('4')));
        assert_eq!(testing::requests(&effects).len(), 1);
    }

    #[test]
    fn a_reply_fills_the_list_with_verdicts() {
        let (app, _) = loaded();
        let s = &app.views.streams;
        assert_eq!(s.rows.len(), 3);
        // billing's consumer holds entries and has been idle 45 min; lag 2,500.
        assert_eq!(s.rows[0].verdicts, [Verdict::Stuck, Verdict::Lagging]);
        assert_eq!(s.rows[1].verdicts, [Verdict::NoReaders]);
        assert!(s.rows[2].info.is_none());
        assert!(s.rows[2].verdicts.is_empty());
    }

    #[test]
    fn keys_move_the_list_and_tab_moves_to_the_details() {
        let (mut app, _) = loaded();
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.views.streams.sel, 1);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.views.streams.sel, 2);
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.views.streams.sel, 0);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.streams.focus, Focus::Detail);
        // billing, b-1, b-2, audit, a-1.
        assert_eq!(app.views.streams.details().len(), 5);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.views.streams.detail_sel, 4);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.views.streams.detail_sel, 4, "clamped");
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.views.streams.focus, Focus::List);
    }

    #[test]
    fn the_selection_follows_its_stream_across_polls() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('j')));
        let mut list = fetched();
        list.swap(0, 1);
        let id = tick(&mut app, t0, 10_000).expect("a poll was sent");
        testing::reply(&mut app, id, Ok(LiveReply::Streams(poll_reply(list))));
        assert_eq!(app.views.streams.selected().unwrap().key, "events:1");
        assert_eq!(app.views.streams.sel, 0);
    }

    #[test]
    fn a_refused_xinfo_turns_the_view_off_with_fixed_text() {
        let mut app = testing::scanned();
        with_streams(&mut app);
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::Streams);
        let (id, _) = testing::requests(&effects).remove(0);
        testing::reply(&mut app, id, Err(LiveError::Denied("XINFO STREAM".into())));
        let off = app.views.streams.off.clone().unwrap();
        assert_eq!(off, "The server refused XINFO STREAM for this user.");
        // Off: no more polls.
        let t0 = Instant::now();
        app.apply(Msg::TickAt(t0));
        assert_eq!(tick(&mut app, t0, 60_000), None);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
        // `r` tries again.
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(testing::requests(&effects).len(), 1);
        assert!(app.views.streams.off.is_none());
    }

    #[test]
    fn a_failed_poll_keeps_the_rows_and_says_so() {
        let (mut app, t0) = loaded();
        let id = tick(&mut app, t0, 10_000).expect("a poll");
        testing::reply(
            &mut app,
            id,
            Err(LiveError::Failed("XINFO STREAM failed".into())),
        );
        assert_eq!(app.views.streams.rows.len(), 3);
        assert_eq!(
            app.views.streams.error.as_deref(),
            Some("XINFO STREAM failed")
        );
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn a_probe_denied_screen_is_off_without_the_view_being_called() {
        let mut app = testing::scanned();
        // `perms_probes_off` is the only fixture with probes off; use it for XINFO too.
        let mut perms = crate::tui::fixtures::perms_with_probes();
        for c in &mut perms.checks {
            if c.label == crate::tui::app::views::PROBE_XINFO {
                c.status = crate::model::PermStatus::Denied(
                    "NOPERM lens-ro has no permissions to run xinfo".into(),
                );
            }
        }
        let (id, scan) = {
            let s = app.session.as_ref().unwrap();
            (s.id, s.scan)
        };
        let _ = (id, scan);
        let entry = app
            .entries
            .iter_mut()
            .find(|e| Some(e.id) == app.session.as_ref().map(|s| s.id))
            .unwrap();
        entry.perms = Some(perms);
        testing::live_ready(&mut app);
        let effects: Vec<Effect> = testing::show(&mut app, Screen::Streams);
        assert!(testing::requests(&effects).is_empty());
        let screen = format!("{}", testing::render(&app, 80, 24).backend());
        assert!(
            screen.contains("The server refused XINFO for this user"),
            "{screen}"
        );
        assert!(!screen.contains("lens-ro"), "{screen}");
    }

    #[test]
    fn snapshots_80x24_and_120x36() {
        let (mut app, _) = loaded();
        insta::assert_snapshot!("list_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("list_120x36", testing::render(&app, 120, 36).backend());
        // Details focused, on the second group's consumer.
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::End));
        insta::assert_snapshot!("details_80x24", testing::render(&app, 80, 24).backend());
        app.handle_key(key(KeyCode::Char('p')));
        insta::assert_snapshot!("paused_120x36", testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn nothing_the_view_shows_holds_an_entry_value() {
        let (app, _) = loaded();
        let shown = format!("{:?}", app.views.streams);
        assert!(!shown.contains("hunter2"));
    }
}
