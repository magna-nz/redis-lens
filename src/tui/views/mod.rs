//! The view states of phase 1, and the interface `App` drives them through.
//!
//! Owner of this file: wave A2. Each submodule belongs to the wave B agent that builds
//! its view, together with the matching draw module in `src/tui/ui/`:
//!
//! | State | Draw | Screens |
//! |---|---|---|
//! | [`keys::KeysState`] | `ui/keys.rs` | Patterns › key list ([`Screen::KeyList`]) |
//! | [`analysis::AnalysisState`] | `ui/analysis.rs` | Analysis › Big keys, Expiry |
//! | [`server::ServerState`] | `ui/server.rs` | Server › Overview, Clients, Slow log |
//! | [`streams::StreamsState`] | `ui/streams.rs` | Streams |
//! | [`pubsub::PubSubState`] | `ui/pubsub.rs` | Pub/Sub |
//! | [`search::SearchState`] | `ui/search.rs` | Search |
//!
//! A wave B agent edits only its two files (and `src/live/<view>.rs` for its requests),
//! never `app.rs`, `keymap.rs` or the spec's key table: every phase 1 key is already in
//! `keymap.rs`.
//!
//! # How `App` calls a view
//!
//! Every call gets an [`Act`]: read access to the session (through [`Ctx`], which
//! `Act` derefs to) plus the ways to act: [`Act::send`] for live requests,
//! [`Act::info`] and [`Act::error`] for the notice line, [`Act::copy`] for the
//! clipboard. A view never does IO itself, and never reads the clock: the time comes
//! with [`ViewState::on_tick`].
//!
//! - **Shown and hidden.** [`ViewState::on_show`] runs when one of the view's screens
//!   comes on screen (a digit, `tab`, `enter` from Patterns), after the switch, so
//!   `act.screen` is the screen now shown. [`ViewState::on_hide`] runs before it goes
//!   away, so `act.screen` is still the screen being hidden. Moving between two panels
//!   of the same view hides one and shows the other.
//! - **Keys.** `App` handles the keys that work everywhere (`0-6`, `?`, `a`, `q`), `tab`
//!   and `shift-tab` on Analysis and Server (the panels), `r` (rescan on Analysis,
//!   [`ViewState::refresh`] on Server, Streams, Pub/Sub and Search) and `s` (snapshot,
//!   Analysis only). Every other key goes to [`ViewState::on_key`] of the view on
//!   screen; return `true` when it did something. On the key list an unhandled `esc`
//!   goes back to Patterns, so handle `esc` yourself only to close something of your
//!   own first (a filter). While [`ViewState::input`] is `Some` every key except
//!   ctrl-c goes to `on_key` first, digits and `q` included, and pastes go to
//!   [`ViewState::on_paste`].
//! - **Ticks.** [`ViewState::on_tick`] runs about ten times a second, only for the view
//!   on screen, with the time from `Msg::TickAt`. Poll from here: compare `now` with the
//!   time of your last request. Polling views poll only while shown (spec "Shared
//!   command budget").
//! - **Replies.** [`Act::send`] returns the request's [`ReqId`]; the answer comes back
//!   to [`ViewState::on_reply`] of the view that sent it, with that id, whatever the
//!   [`LiveReply`] variant (so Big keys may send a `keys::Request`). A failure arrives
//!   as `Err` there too. Replies for a replaced run, after a rescan, never arrive: check
//!   [`Act::in_flight`] before waiting on an old id, and send again when it is false.
//! - **Off views.** When the view's permission probe was denied or skipped
//!   ([`Ctx::off_reason`]), `App` draws the reason instead of the view and calls none of
//!   the methods above for it. Without a session (nothing connected) it is the same.
//!   Views are reset to `Default` on every new connect, and kept across a rescan.
//!
//! # Drawing
//!
//! `ui/<view>.rs` has `pub fn draw(frame: &mut Frame, area: Rect, state: &State,
//! ctx: &Ctx<'_>)`. `area` is the body between the header (with the tab bar) and the
//! key bar. `ctx.screen` says which panel to draw. Big keys and Expiry are drawn only
//! once the scan has finished (`ctx.report()` is `Some`).
//!
//! # Tests
//!
//! Put a view's tests, insta snapshots included, in a `#[cfg(test)] mod tests` inside
//! its own `views/<name>.rs`. [`testing`] builds an app on any screen with a finished
//! scan and a ready live connection, renders it, and feeds it live replies. The stub in
//! `server.rs` has a working example.

use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::time::Instant;

use crossterm::event::KeyEvent;

use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{PermStatus, PermissionReport, Profile, ScanReport, ServerInfo};
use crate::tui::clipboard::CopyText;

use super::{Effect, Notice, Screen, Session, copy_checked};

pub mod analysis;
pub mod keys;
pub mod pubsub;
pub mod search;
pub mod server;
pub mod streams;

/// Labels of the phase 1 view probes in [`PermissionReport::checks`], after the scan's
/// own checks (`model::PERM_LABELS`). Only the scan's checks make a connection
/// `Limited`; a denied or skipped probe turns its view off ([`Ctx::off_reason`]).
pub const PROBE_SLOWLOG: &str = "SLOWLOG";
pub const PROBE_CLIENT_LIST: &str = "CLIENT LIST";
pub const PROBE_XINFO: &str = "XINFO";
pub const PROBE_PUBSUB: &str = "PUBSUB";
pub const PROBE_FT_LIST: &str = "FT._LIST";
pub const PROBE_LABELS: [&str; 5] = [
    PROBE_SLOWLOG,
    PROBE_CLIENT_LIST,
    PROBE_XINFO,
    PROBE_PUBSUB,
    PROBE_FT_LIST,
];

/// The probe a screen needs, if any.
pub fn probe_of(screen: Screen) -> Option<&'static str> {
    match screen {
        Screen::SlowLog => Some(PROBE_SLOWLOG),
        Screen::Clients => Some(PROBE_CLIENT_LIST),
        Screen::Streams => Some(PROBE_XINFO),
        Screen::PubSub => Some(PROBE_PUBSUB),
        Screen::Search => Some(PROBE_FT_LIST),
        _ => None,
    }
}

/// Which view state a screen, or a live request, belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owner {
    Keys,
    Analysis,
    Server,
    Streams,
    PubSub,
    Search,
}

/// The session's live connection (spec "Browsing, server views and writes").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LiveState {
    /// Not open yet: connecting, scanning, or a rescan replaced the previous one.
    #[default]
    Waiting,
    /// [`Act::send`] works.
    Ready,
    /// Closed. Holds why, in redis-lens's own words, when it was not on purpose.
    Closed(Option<String>),
}

/// A text input a view has open (a filter, say). While a view returns one from
/// [`ViewState::input`], the key bar shows it instead of the keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// Shown in a key cap before the text, e.g. `/`.
    pub label: &'static str,
    pub text: String,
    /// Dim text after the cursor, e.g. `   enter to apply · esc to clear`.
    pub hint: &'static str,
}

/// One view's state. Every method has a default that does nothing, so a view
/// implements only what it needs. See the module documentation for when each runs.
pub trait ViewState: fmt::Debug {
    /// One of this view's screens came on screen (`act.screen`).
    fn on_show(&mut self, _act: &mut Act<'_>) {}

    /// One of this view's screens (`act.screen`) is about to go.
    fn on_hide(&mut self, _act: &mut Act<'_>) {}

    /// A key `App` does not handle itself. True when it did something.
    fn on_key(&mut self, _key: KeyEvent, _act: &mut Act<'_>) -> bool {
        false
    }

    /// About ten times a second while one of this view's screens is shown.
    fn on_tick(&mut self, _now: Instant, _act: &mut Act<'_>) {}

    /// The answer to a request this view sent with [`Act::send`].
    fn on_reply(&mut self, _id: ReqId, _result: Result<LiveReply, LiveError>, _act: &mut Act<'_>) {}

    /// `r` on Server, Streams, Pub/Sub or Search: fetch the current panel now.
    fn refresh(&mut self, _act: &mut Act<'_>) {}

    /// The text input this view has open, if any.
    fn input(&self) -> Option<Input> {
        None
    }

    /// Pasted text, only while [`ViewState::input`] is `Some`.
    fn on_paste(&mut self, _text: &str, _act: &mut Act<'_>) {}
}

/// Every view state. `App::views`; reset on each new connect.
#[derive(Debug, Default)]
pub struct Views {
    pub keys: keys::KeysState,
    pub analysis: analysis::AnalysisState,
    pub server: server::ServerState,
    pub streams: streams::StreamsState,
    pub pubsub: pubsub::PubSubState,
    pub search: search::SearchState,
}

impl Views {
    pub fn get(&self, owner: Owner) -> &dyn ViewState {
        match owner {
            Owner::Keys => &self.keys,
            Owner::Analysis => &self.analysis,
            Owner::Server => &self.server,
            Owner::Streams => &self.streams,
            Owner::PubSub => &self.pubsub,
            Owner::Search => &self.search,
        }
    }

    pub fn get_mut(&mut self, owner: Owner) -> &mut dyn ViewState {
        match owner {
            Owner::Keys => &mut self.keys,
            Owner::Analysis => &mut self.analysis,
            Owner::Server => &mut self.server,
            Owner::Streams => &mut self.streams,
            Owner::PubSub => &mut self.pubsub,
            Owner::Search => &mut self.search,
        }
    }
}

/// Read access to the session, for the handlers (through [`Act`]) and the draw
/// functions. Only `App` builds one, and only while a session exists.
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    /// What is on screen; on a view with panels, which panel.
    pub screen: Screen,
    /// The connected session: its profile, run, phase, report and live connection.
    pub session: &'a Session,
    /// The permission check of the session's entry, once connected: the scan's checks
    /// and the view probes ([`PROBE_LABELS`]).
    pub perms: Option<&'a PermissionReport>,
    /// Counts 100 ms ticks, for spinners.
    pub tick: u64,
    /// The latest time from `Msg::TickAt`. `None` before the first one.
    pub now: Option<Instant>,
    /// List rows that fit on screen, at least 1: what PgUp and PgDn move by.
    pub page: usize,
    /// Ticks since the selection last changed, for `widgets::pad_r_marquee`. Counts
    /// only selections `App` sees; a view that keeps its own selection tracks its own.
    pub sel_ticks: u64,
}

impl<'a> Ctx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        screen: Screen,
        session: &'a Session,
        perms: Option<&'a PermissionReport>,
        tick: u64,
        now: Option<Instant>,
        page: usize,
        sel_ticks: u64,
    ) -> Self {
        Self {
            screen,
            session,
            perms,
            tick,
            now,
            page,
            sel_ticks,
        }
    }

    pub fn profile(&self) -> &'a Profile {
        &self.session.profile
    }

    /// The finished scan, if any. During a rescan, the previous run's report until the
    /// new run's first results.
    pub fn report(&self) -> Option<&'a ScanReport> {
        self.session.report.as_ref()
    }

    pub fn server(&self) -> Option<&'a ServerInfo> {
        self.session.server.as_ref()
    }

    pub fn live(&self) -> &'a LiveState {
        &self.session.live
    }

    /// True when [`Act::send`] would send.
    pub fn live_ready(&self) -> bool {
        self.session.live == LiveState::Ready
    }

    /// The status of the check labelled `label`, once the permission check ran.
    pub fn check(&self, label: &str) -> Option<&'a PermStatus> {
        self.perms?
            .checks
            .iter()
            .find(|c| c.label == label)
            .map(|c| &c.status)
    }

    /// Why `self.screen` is off, from its probe ([`probe_of`]): `None` when it is on,
    /// or not checked (yet). Never the server's own error text, which can name the
    /// ACL user.
    pub fn off_reason(&self) -> Option<String> {
        let label = probe_of(self.screen)?;
        match self.check(label)? {
            PermStatus::Allowed => None,
            PermStatus::Denied(_) => Some(format!(
                "The server refused {label} for this user, so this view is off."
            )),
            PermStatus::Skipped(why) => Some(format!("{label} is not available: {why}.")),
        }
    }
}

/// What a handler can do, besides reading the [`Ctx`] it derefs to.
pub struct Act<'a> {
    ctx: Ctx<'a>,
    owner: Owner,
    next_req: &'a mut u64,
    pending: &'a mut HashMap<ReqId, Owner>,
    notice: &'a mut Option<Notice>,
    effects: Vec<Effect>,
}

impl<'a> Deref for Act<'a> {
    type Target = Ctx<'a>;

    fn deref(&self) -> &Ctx<'a> {
        &self.ctx
    }
}

impl<'a> Act<'a> {
    pub(super) fn new(
        ctx: Ctx<'a>,
        owner: Owner,
        next_req: &'a mut u64,
        pending: &'a mut HashMap<ReqId, Owner>,
        notice: &'a mut Option<Notice>,
    ) -> Self {
        Self {
            ctx,
            owner,
            next_req,
            pending,
            notice,
            effects: Vec::new(),
        }
    }

    pub(super) fn into_effects(self) -> Vec<Effect> {
        self.effects
    }

    /// Sends `req` on the session's live connection. The answer comes back to this
    /// view's [`ViewState::on_reply`] with the returned id. `None`, and nothing sent,
    /// while the live connection is not [`LiveState::Ready`].
    pub fn send(&mut self, req: LiveRequest) -> Option<ReqId> {
        if !self.ctx.live_ready() {
            return None;
        }
        let id = ReqId(*self.next_req);
        *self.next_req += 1;
        self.pending.insert(id, self.owner);
        self.effects.push(Effect::Live {
            id: self.ctx.session.id,
            req_id: id,
            req,
        });
        Some(id)
    }

    /// True while the request `id` may still be answered. False once its answer came,
    /// or when it never will: the live connection closed, or a rescan replaced it.
    pub fn in_flight(&self, id: ReqId) -> bool {
        self.pending.contains_key(&id)
    }

    /// An info line in the key bar, gone after a few seconds.
    pub fn info(&mut self, text: impl Into<String>) {
        *self.notice = Some(Notice::info(text, self.ctx.tick));
    }

    /// An error line in the key bar, until the next key.
    pub fn error(&mut self, text: impl Into<String>) {
        *self.notice = Some(Notice::error(text));
    }

    /// Copies `text` with OSC 52, with the same checks and notices as `y` on the other
    /// views: control characters and text too long for the terminal are refused.
    pub fn copy(&mut self, text: CopyText) {
        let (notice, effect) = copy_checked(text, self.ctx.tick);
        *self.notice = Some(notice);
        self.effects.extend(effect);
    }
}

/// Helpers for view tests. See `server.rs` for an example.
#[cfg(test)]
pub mod testing {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::conn::ScanTarget;
    use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
    use crate::tui::app::{App, Effect, Msg, Screen};
    use crate::tui::fixtures;
    use crate::tui::tree::Row;
    use crate::tui::ui;

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Connected to prod-cache-01 (read-only), every permission check allowed, the
    /// fixture scan finished, with no history. The live connection is not open yet.
    pub fn scanned() -> App {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, scan, .. }] = effects[..] else {
            panic!("expected a connect, got {effects:?}");
        };
        let report = fixtures::report(true);
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(report.server.clone()),
            perms: fixtures::perms_with_probes(),
            target: ScanTarget::Primary,
        });
        app.apply(Msg::ScanDone { id, scan, report });
        app
    }

    /// The session's live connection is open.
    pub fn live_ready(app: &mut App) -> Vec<Effect> {
        let (id, scan) = ids(app);
        app.apply(Msg::LiveReady { id, scan })
    }

    /// Goes to `screen` with keys, as a user would: its view's digit, then `tab` to the
    /// panel; for the key list, `enter` on the first pattern row of Patterns.
    pub fn show(app: &mut App, screen: Screen) -> Vec<Effect> {
        let mut out = app.handle_key(KeyEvent::new(
            KeyCode::Char(screen.view().digit()),
            KeyModifiers::NONE,
        ));
        if screen == Screen::KeyList {
            let rows = app.pattern_rows();
            let first = rows
                .iter()
                .position(|r| match r {
                    Row::Pattern { .. } => true,
                    Row::Group { root } => matches!(
                        app.patterns.tree.roots.get(*root),
                        Some(crate::tui::tree::Root::Single(_))
                    ),
                })
                .expect("a pattern row");
            app.patterns.sel = first;
            out.extend(app.handle_key(key(KeyCode::Enter)));
        }
        for _ in 0..screen.view().panels().len() {
            if app.screen() == screen {
                break;
            }
            out.extend(app.handle_key(key(KeyCode::Tab)));
        }
        assert_eq!(app.screen(), screen);
        out
    }

    /// A scanned app with the live connection ready, on `screen`. Returns the effects
    /// of getting there (the view's `on_show`, say).
    pub fn app_on(screen: Screen) -> (App, Vec<Effect>) {
        let mut app = scanned();
        let mut out = live_ready(&mut app);
        out.extend(show(&mut app, screen));
        (app, out)
    }

    /// The live requests among `effects`.
    pub fn requests(effects: &[Effect]) -> Vec<(ReqId, LiveRequest)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Live { req_id, req, .. } => Some((*req_id, req.clone())),
                _ => None,
            })
            .collect()
    }

    /// The answer to request `req_id`, as the runtime would send it.
    pub fn reply(
        app: &mut App,
        req_id: ReqId,
        result: Result<LiveReply, LiveError>,
    ) -> Vec<Effect> {
        let (id, scan) = ids(app);
        app.apply(Msg::Live {
            id,
            scan,
            req_id,
            result,
        })
    }

    /// The whole screen drawn at `w` × `h`, for `insta::assert_snapshot!(t.backend())`.
    pub fn render(app: &App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| ui::draw(f, app)).unwrap();
        t
    }

    fn ids(app: &App) -> (crate::tui::app::ConnId, crate::tui::app::ScanId) {
        let s = app.session.as_ref().expect("a session");
        (s.id, s.scan)
    }
}
