//! State of the Pub/Sub view (spec "5 Pub/Sub"). Owner: wave B. Drawn by `ui/pubsub.rs`;
//! its live requests are in `src/live/pubsub.rs`.
//!
//! Counts only: the active channels, their subscriber counts and the pattern count, every
//! 5 s while the view is shown (`p` pauses). It never subscribes and never reads a payload.
//! See [`super`] for the interface `App` calls.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};

use super::streams::{list_move, off_text, plain};
use super::{Act, ViewState};
use crate::live::pubsub::{Counts, Reply, Request};
use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};

/// How often the counts are polled while the view is shown (spec "Shared command budget").
pub const POLL: Duration = Duration::from_secs(5);

/// The channels with their subscriber counts.
#[derive(Debug, Default)]
pub struct PubSubState {
    pub counts: Counts,
    pub sel: usize,
    pub paused: bool,
    /// An answer has arrived (so an empty list means no channels, not "not read yet").
    pub loaded: bool,
    /// Why the view is off, in redis-lens's own words. `r` tries again.
    pub off: Option<String>,
    /// Why the last poll failed. The previous counts stay.
    pub error: Option<String>,
    pending: Option<ReqId>,
    last_sent: Option<Instant>,
}

impl PubSubState {
    fn fetch(&mut self, act: &mut Act<'_>) {
        self.last_sent = act.now;
        self.pending = act.send(LiveRequest::PubSub(Request::Counts));
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
}

impl ViewState for PubSubState {
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
        self.off = None;
        if !self.in_flight(act) {
            self.fetch(act);
        }
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if key.code == KeyCode::Char('p') && plain(&key) {
            self.paused = !self.paused;
            if self.paused {
                act.info("Polling paused. Press p to resume, r to refresh once.");
            } else {
                act.info("Polling resumed.");
                self.last_sent = None;
            }
            return true;
        }
        let Some(to) = list_move(&key, self.sel, self.counts.channels.len(), act.page) else {
            return false;
        };
        self.sel = to;
        true
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, _act: &mut Act<'_>) {
        if self.pending == Some(id) {
            self.pending = None;
        }
        match result {
            Ok(LiveReply::PubSub(Reply::Counts(counts))) => {
                // The selection stays on its channel when the list moves.
                let name = self.counts.channels.get(self.sel).map(|c| c.name.clone());
                self.counts = counts;
                self.sel = name
                    .and_then(|n| self.counts.channels.iter().position(|c| c.name == n))
                    .unwrap_or(self.sel)
                    .min(self.counts.channels.len().saturating_sub(1));
                self.loaded = true;
                self.error = None;
            }
            Ok(_) => {}
            Err(e @ (LiveError::Denied(_) | LiveError::Unsupported(_))) => {
                self.off = Some(off_text(&e));
            }
            Err(LiveError::Closed) => {}
            Err(LiveError::Failed(why)) => self.error = Some(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{self, key};
    use super::*;
    use crate::live::pubsub::Channel;
    use crate::tui::app::{App, Msg, Screen};

    fn channel(name: &str, subscribers: u64) -> Channel {
        Channel {
            name: name.into(),
            subscribers: Some(subscribers),
        }
    }

    fn counts() -> Counts {
        Counts {
            channels: vec![
                channel("orders:eu", 12),
                channel("orders:us", 4),
                channel("news", 2),
                channel("alerts", 1),
                channel("idle", 0),
            ],
            total_channels: 5,
            patterns: Some(3),
            notes: Vec::new(),
        }
    }

    fn at(base: Instant, ms: u64) -> Msg {
        Msg::TickAt(base + Duration::from_millis(ms))
    }

    fn tick(app: &mut App, t0: Instant, ms: u64) -> Option<ReqId> {
        let reqs = testing::requests(&app.apply(at(t0, ms)));
        assert!(reqs.len() <= 1, "{reqs:?}");
        reqs.first().map(|r| r.0)
    }

    fn answer(app: &mut App, id: ReqId, c: Counts) {
        testing::reply(app, id, Ok(LiveReply::PubSub(Reply::Counts(c))));
    }

    fn loaded() -> (App, Instant) {
        let t0 = Instant::now();
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        app.apply(Msg::TickAt(t0));
        let effects = testing::show(&mut app, Screen::PubSub);
        let reqs = testing::requests(&effects);
        let [(id, LiveRequest::PubSub(Request::Counts))] = &reqs[..] else {
            panic!("expected one Counts request, got {reqs:?}");
        };
        answer(&mut app, *id, counts());
        (app, t0)
    }

    #[test]
    fn on_show_asks_for_the_counts() {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::PubSub);
        let reqs = testing::requests(&effects);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1, LiveRequest::PubSub(Request::Counts));
    }

    #[test]
    fn polls_every_five_seconds_while_shown_and_p_pauses() {
        let (mut app, t0) = loaded();
        assert_eq!(tick(&mut app, t0, 4_900), None);
        let id = tick(&mut app, t0, 5_000).expect("a poll at 5 s");
        assert_eq!(tick(&mut app, t0, 9_000), None, "one at a time");
        answer(&mut app, id, counts());
        assert_eq!(tick(&mut app, t0, 9_900), None);
        assert!(tick(&mut app, t0, 10_000).is_some());
        // Paused: no polls; `r` still reads.
        let id = app.views.pubsub.pending.unwrap();
        answer(&mut app, id, counts());
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.views.pubsub.paused);
        assert_eq!(tick(&mut app, t0, 60_000), None);
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(testing::requests(&effects).len(), 1);
    }

    #[test]
    fn does_not_poll_while_hidden() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(tick(&mut app, t0, 60_000), None);
    }

    #[test]
    fn keys_move_the_selection_and_it_follows_its_channel() {
        let (mut app, t0) = loaded();
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.views.pubsub.sel, 2);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.views.pubsub.sel, 4);
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.views.pubsub.sel, 0);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.views.pubsub.counts.channels[1].name, "orders:us");
        // The next poll ranks orders:us first.
        let id = tick(&mut app, t0, 6_000).expect("a poll");
        let mut c = counts();
        c.channels[1].subscribers = Some(40);
        c.channels.sort_by_key(|c| std::cmp::Reverse(c.subscribers));
        answer(&mut app, id, c);
        assert_eq!(app.views.pubsub.sel, 0);
        assert_eq!(app.views.pubsub.counts.channels[0].name, "orders:us");
    }

    #[test]
    fn a_refused_pubsub_turns_the_view_off_with_fixed_text() {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        let effects = testing::show(&mut app, Screen::PubSub);
        let (id, _) = testing::requests(&effects).remove(0);
        testing::reply(
            &mut app,
            id,
            Err(LiveError::Denied("PUBSUB CHANNELS".into())),
        );
        assert_eq!(
            app.views.pubsub.off.as_deref(),
            Some("The server refused PUBSUB CHANNELS for this user.")
        );
        let t0 = Instant::now();
        app.apply(Msg::TickAt(t0));
        assert_eq!(tick(&mut app, t0, 60_000), None);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
        let effects = app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(testing::requests(&effects).len(), 1);
    }

    #[test]
    fn a_failed_poll_keeps_the_counts() {
        let (mut app, t0) = loaded();
        let id = tick(&mut app, t0, 6_000).expect("a poll");
        testing::reply(
            &mut app,
            id,
            Err(LiveError::Failed("PUBSUB CHANNELS failed".into())),
        );
        assert_eq!(app.views.pubsub.counts.channels.len(), 5);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn snapshots() {
        let (mut app, _) = loaded();
        insta::assert_snapshot!("counts_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("counts_120x36", testing::render(&app, 120, 36).backend());
        // A refused NUMSUB and NUMPAT: the counts are left out, with the reason.
        let mut c = counts();
        for ch in &mut c.channels {
            ch.subscribers = None;
        }
        c.patterns = None;
        c.notes = vec![
            "the server refused PUBSUB NUMPAT for this user".into(),
            "the server refused PUBSUB NUMSUB for this user".into(),
        ];
        let id = app.views.pubsub.pending.unwrap_or_else(|| {
            let t0 = Instant::now();
            app.apply(Msg::TickAt(t0));
            tick(&mut app, t0, 60_000).expect("a poll")
        });
        answer(&mut app, id, c);
        insta::assert_snapshot!("no_counts_80x24", testing::render(&app, 80, 24).backend());
        // No channels.
        let id = {
            let t0 = Instant::now();
            app.apply(Msg::TickAt(t0));
            tick(&mut app, t0, 120_000).expect("a poll")
        };
        answer(
            &mut app,
            id,
            Counts {
                patterns: Some(0),
                ..Counts::default()
            },
        );
        insta::assert_snapshot!("empty_80x24", testing::render(&app, 80, 24).backend());
    }
}
