//! The database list and the switch (spec "Database switch (P4)"): `b` opens the list
//! (one `INFO keyspace`), `enter` switches the session to the selected database as a
//! rescan on another `db`, `s` also saves it as the profile's `db` after asking. The
//! list is an overlay `App` owns, like the bulk dialog: while it is open every key goes
//! to it.
//!
//! The switch is session-local: the session's own copy of the profile
//! (`Session::profile`) gets the new `db`, and the entry on Connections, the saved
//! profile, never does unless the config file was written by `s` and answered `y`.

use crossterm::event::{KeyCode, KeyEvent};

use super::views::LiveState;
use super::views::dbs::{
    DENIED, DbList, NO_PROFILE, OFF_CLOSED, OFF_CLUSTER, OFF_NOT_CONNECTED, OFF_NOT_READY,
    OFF_SCANNING, UNREAD,
};
use super::{App, Effect, Phase, plain_char};
use crate::conn::ScanTarget;
use crate::live::dbs::{Reply, Request};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest, ReqId};

impl App {
    /// `b`: opens the list, or says why not. Sends one `INFO keyspace`, and nothing at
    /// all on a cluster or while the connection is not ready.
    pub(super) fn open_dbs(&mut self) -> Vec<Effect> {
        let Some(s) = self.session.as_ref() else {
            self.set_info(OFF_NOT_CONNECTED);
            return Vec::new();
        };
        let why = if s.is_cluster() || matches!(s.target, Some(ScanTarget::Cluster(_))) {
            Some(OFF_CLUSTER)
        } else {
            match (&s.phase, &s.live) {
                (Phase::Connecting, _) => Some(OFF_NOT_READY),
                (Phase::Scanning, _) => Some(OFF_SCANNING),
                (_, LiveState::Waiting) => Some(OFF_NOT_READY),
                (_, LiveState::Closed(_)) => Some(OFF_CLOSED),
                _ => None,
            }
        };
        if let Some(why) = why {
            self.set_info(why);
            return Vec::new();
        }
        let (id, current, picked) = (s.id, s.profile.db, s.picked);
        let prod = s.profile.env == crate::model::Env::Prod;
        let saved_as = self
            .entry(id)
            .filter(|e| !e.is_docker())
            .map(|e| e.profile.name.clone());
        let req = ReqId(self.next_req);
        self.next_req += 1;
        self.dbs = Some(DbList::loading(current, saved_as, prod, req));
        vec![Effect::Live {
            id,
            req_id: req,
            to: Dest::node(picked),
            req: LiveRequest::Dbs(Request::Keyspace),
        }]
    }

    /// True when the list waits for the answer to request `id`.
    pub(super) fn dbs_waits_for(&self, id: ReqId) -> bool {
        self.dbs.as_ref().is_some_and(|l| l.waits_for(id))
    }

    /// The answer to the list's `INFO keyspace`. A refusal or a failure leaves the
    /// current database as the only row, with a fixed reason.
    pub(super) fn dbs_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>) {
        if !self.dbs_waits_for(id) {
            return;
        }
        let Some(l) = self.dbs.as_mut() else {
            return;
        };
        match result {
            Ok(LiveReply::Dbs(Reply::Keyspace(infos))) => l.fill(&infos),
            Err(LiveError::Denied(_)) => l.unread(DENIED),
            Ok(_) | Err(_) => l.unread(UNREAD),
        }
    }

    /// The live connection closed: the list has nobody to ask.
    pub(super) fn dbs_closed(&mut self) {
        self.dbs = None;
    }

    /// The keys of the list while it is open.
    pub(super) fn dbs_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let page = self.page();
        let Some(l) = self.dbs.as_mut() else {
            return Vec::new();
        };
        if let Some(db) = l.ask {
            match (key.code, plain_char(&key)) {
                (_, Some('y')) => return self.dbs_save(db),
                (KeyCode::Esc, _) | (_, Some('n')) => l.ask = None,
                _ => {}
            }
            return Vec::new();
        }
        l.note = None;
        match (key.code, plain_char(&key)) {
            (KeyCode::Esc, _) => self.dbs = None,
            (KeyCode::Enter, _) => return self.dbs_enter(),
            (_, Some('s')) => {
                if l.saved_as.is_some() {
                    l.ask = Some(l.selected());
                } else {
                    l.note = Some(NO_PROFILE);
                }
            }
            _ => {
                if let Some(to) = super::list_move(&key, l.sel, l.rows.len(), page) {
                    l.sel = to;
                }
            }
        }
        Vec::new()
    }

    /// `enter`: the selected database. The current one just closes the list.
    fn dbs_enter(&mut self) -> Vec<Effect> {
        let Some(l) = self.dbs.as_ref() else {
            return Vec::new();
        };
        let db = l.selected();
        if db == l.current {
            self.dbs = None;
            return Vec::new();
        }
        self.switch_db(db)
    }

    /// `y` on the save question: switches (unless it is the current database) and
    /// saves `db` as the profile's `db`. A failed save still leaves the switch done.
    fn dbs_save(&mut self, db: u32) -> Vec<Effect> {
        let Some(l) = self.dbs.as_ref() else {
            return Vec::new();
        };
        let Some(name) = l.saved_as.clone() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if db == l.current {
            self.dbs = None;
        } else {
            out = self.switch_db(db);
            if out.is_empty() {
                // The switch did not start (the entry is gone): nothing to save for.
                return out;
            }
        }
        out.push(Effect::SaveDb { name, db });
        out
    }

    /// Switches the session to database `db`: a connect on it that reads no key
    /// ([`App::rescan`]: the previous database's results go). The views' live connection is opened again on the new
    /// index by the connect; a running Pub/Sub tail subscribes again to the same
    /// channel or pattern once the new live connection is ready; a running Monitor
    /// stops.
    fn switch_db(&mut self, db: u32) -> Vec<Effect> {
        self.dbs = None;
        let tail = self.views.pubsub.running_subscription();
        let Some(old) = self.session.as_mut().map(|s| {
            let old = s.profile.db;
            s.profile.db = db;
            old
        }) else {
            return Vec::new();
        };
        let effects = self.rescan();
        if effects.is_empty() {
            if let Some(s) = self.session.as_mut() {
                s.profile.db = old;
            }
            return effects;
        }
        self.db_resub = tail;
        self.set_info(format!(
            "Switched to db {db} for this session. The saved profile is unchanged."
        ));
        effects
    }

    /// Once the new run's live connection is ready: the tail that ran before a switch
    /// subscribes again.
    pub(super) fn resubscribe_after_switch(&mut self) -> Vec<Effect> {
        let Some(to) = self.db_resub.take() else {
            return Vec::new();
        };
        self.with_views(super::views::Owner::PubSub, "resubscribe", |v, act| {
            v.pubsub.resubscribe(to, act)
        })
        .map_or_else(Vec::new, |(_, e)| e)
    }

    /// [`super::Msg::DbSaved`]: the config file has `db` for `name`; the entries on
    /// Connections show it, and a new connect uses it.
    pub(super) fn db_saved(&mut self, name: &str, db: u32) {
        for e in &mut self.entries {
            if !e.is_docker() && e.profile.name == name {
                e.profile.db = db;
            }
        }
        self.set_info(format!(
            "Saved db {db} as the database of {} (config file).",
            crate::value::escape(name)
        ));
    }

    /// [`super::Msg::DbSaveFailed`]: the switch stands for this session, the file is as
    /// it was.
    pub(super) fn db_save_failed(&mut self, name: &str, db: u32, error: &str) {
        self.set_error(format!(
            "Could not save db {db} for {}: {}. The switch still applies to this session.",
            crate::value::escape(name),
            crate::conn::parse::mask(error)
        ));
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::views::dbs::Stage;
    use super::super::views::testing::{self, key};
    use super::super::{App, Msg, Screen};
    use super::*;
    use crate::live::pubsub::Subscription;
    use crate::model::KeyspaceInfo;
    use crate::tui::fixtures;

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ids(app: &App) -> (u64, u64) {
        let s = app.session.as_ref().unwrap();
        (s.id, s.scan)
    }

    fn ready() -> App {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        app
    }

    fn info(db: u32, keys: u64, expires: u64) -> KeyspaceInfo {
        KeyspaceInfo {
            db,
            keys,
            expires,
            ..KeyspaceInfo::default()
        }
    }

    /// `b`, answered with databases 0 and 3.
    fn opened(app: &mut App) -> ReqId {
        let effects = app.handle_key(ch('b'));
        let [(req, _)] = &testing::requests(&effects)[..] else {
            panic!("expected one request, got {effects:?}");
        };
        let req = *req;
        testing::reply(
            app,
            req,
            Ok(LiveReply::Dbs(Reply::Keyspace(vec![
                info(0, 4, 1),
                info(3, 7, 2),
            ]))),
        );
        req
    }

    fn notice(app: &App) -> String {
        app.notice.clone().expect("a notice").text
    }

    fn connect_of(effects: &[Effect]) -> (u64, crate::model::Profile) {
        let [Effect::Connect { scan, profile, .. }] = effects else {
            panic!("expected one connect, got {effects:?}");
        };
        (*scan, profile.clone())
    }

    /// The new run connected and scanned.
    fn finish(app: &mut App, scan: u64) {
        let (id, _) = ids(app);
        let report = fixtures::report(true);
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(report.server.clone()),
            perms: fixtures::perms_with_probes(),
            target: crate::conn::ScanTarget::Primary,
        });
        app.apply(Msg::ScanDone {
            id,
            scan,
            report: Box::new(report),
        });
    }

    #[test]
    fn b_opens_the_list_and_sends_exactly_one_info_keyspace() {
        let mut app = ready();
        let effects = app.handle_key(ch('b'));
        assert_eq!(effects.len(), 1, "{effects:?}");
        assert!(matches!(
            &effects[0],
            Effect::Live {
                req: LiveRequest::Dbs(Request::Keyspace),
                ..
            }
        ));
        assert!(app.dbs.is_some());
        // Moving and closing send nothing.
        assert!(app.handle_key(ch('j')).is_empty());
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(app.dbs.is_none());
    }

    #[test]
    fn b_works_on_every_screen_but_not_in_a_text_input() {
        for screen in Screen::ALL {
            // Nodes and Slots are not screens of the standalone fixture server.
            if matches!(screen, Screen::Connections | Screen::Nodes | Screen::Slots) {
                continue;
            }
            let (mut app, _) = testing::app_on(screen);
            let effects = app.handle_key(ch('b'));
            let reqs = testing::requests(&effects);
            assert!(
                reqs.iter()
                    .any(|(_, r)| matches!(r, LiveRequest::Dbs(Request::Keyspace))),
                "{}: {effects:?}",
                screen.title()
            );
        }
        // In the Pub/Sub subscribe input `b` is text.
        let (mut app, _) = testing::app_on(Screen::PubSub);
        app.handle_key(ch('s'));
        let effects = app.handle_key(ch('b'));
        assert!(app.dbs.is_none());
        assert!(effects.is_empty());
    }

    #[test]
    fn the_reply_lists_every_database_with_its_keys() {
        let mut app = ready();
        opened(&mut app);
        let l = app.dbs.as_ref().unwrap();
        assert_eq!(
            l.rows.iter().map(|r| (r.db, r.counts)).collect::<Vec<_>>(),
            [(0, Some((4, 1))), (3, Some((7, 2)))]
        );
        assert_eq!(l.selected(), 0);
    }

    #[test]
    fn a_refused_info_lists_the_current_database_with_a_fixed_reason() {
        let mut app = ready();
        let effects = app.handle_key(ch('b'));
        let req = testing::requests(&effects)[0].0;
        testing::reply(
            &mut app,
            req,
            Err(LiveError::Denied("INFO keyspace".into())),
        );
        let l = app.dbs.as_ref().unwrap();
        assert_eq!(l.rows.len(), 1);
        assert_eq!(l.stage, Stage::Unread(DENIED));
        // A failure of another kind has its own fixed words, never the server's.
        let mut app = ready();
        let effects = app.handle_key(ch('b'));
        let req = testing::requests(&effects)[0].0;
        testing::reply(
            &mut app,
            req,
            Err(LiveError::Failed("secret-user said no".into())),
        );
        let l = app.dbs.as_ref().unwrap();
        assert_eq!(l.stage, Stage::Unread(UNREAD));
        assert!(!text(&app, 80, 24).contains("secret-user"));
    }

    #[test]
    fn a_reply_after_the_list_closed_is_dropped() {
        let mut app = ready();
        let effects = app.handle_key(ch('b'));
        let req = testing::requests(&effects)[0].0;
        app.handle_key(key(KeyCode::Esc));
        let reply = Ok(LiveReply::Dbs(Reply::Keyspace(vec![])));
        assert!(testing::reply(&mut app, req, reply).is_empty());
        assert!(app.dbs.is_none());
    }

    #[test]
    fn b_is_off_with_a_reason_and_sends_nothing() {
        // Nothing connected.
        let mut app = App::new(fixtures::profiles());
        assert!(app.handle_key(ch('b')).is_empty());
        assert_eq!(notice(&app), OFF_NOT_CONNECTED);
        assert!(app.dbs.is_none());

        // Connecting.
        let mut app = App::new(fixtures::profiles());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.handle_key(ch('b')).is_empty());
        assert_eq!(notice(&app), OFF_NOT_READY);
        assert!(app.dbs.is_none());

        // Connected, analyzing (`r`, `enter`).
        let connected = |app: &mut App| {
            let (id, scan) = ids(app);
            app.apply(Msg::Connected {
                id,
                scan,
                server: None,
                perms: fixtures::perms_with_probes(),
                target: crate::conn::ScanTarget::Primary,
            });
            testing::live_ready(app);
        };
        connected(&mut app);
        app.handle_key(ch('r'));
        app.handle_key(key(KeyCode::Enter));
        connected(&mut app);
        assert!(app.handle_key(ch('b')).is_empty());
        assert_eq!(notice(&app), OFF_SCANNING);
        assert!(app.dbs.is_none());

        // Scanned, live connection not open yet.
        let mut app = testing::scanned();
        assert!(app.handle_key(ch('b')).is_empty());
        assert_eq!(notice(&app), OFF_NOT_READY);

        // Live connection closed.
        let mut app = ready();
        let (id, scan) = ids(&app);
        app.apply(Msg::LiveClosed {
            id,
            scan,
            error: None,
        });
        assert!(app.handle_key(ch('b')).is_empty());
        assert_eq!(notice(&app), OFF_CLOSED);

        // A cluster.
        let mut app = ready();
        let (id, scan) = ids(&app);
        app.apply(Msg::Nodes {
            id,
            scan,
            nodes: fixtures::live_nodes(),
        });
        assert!(app.handle_key(ch('b')).is_empty());
        assert_eq!(notice(&app), "A cluster only has database 0.");
        assert!(app.dbs.is_none());
    }

    #[test]
    fn enter_on_the_current_database_closes_without_a_rescan() {
        let mut app = ready();
        opened(&mut app);
        let scan = ids(&app).1;
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.dbs.is_none());
        assert_eq!(ids(&app).1, scan);
    }

    #[test]
    fn enter_on_another_database_connects_on_it_without_sampling_keys() {
        // The session had an analysis: the switch still samples nothing (keys are
        // sampled only on `r`), and the old database's results go.
        let mut app = ready();
        assert!(app.session.as_ref().unwrap().report.is_some());
        opened(&mut app);
        let (id, old) = ids(&app);
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { sample, .. }] = &effects[..] else {
            panic!("expected one connect, got {effects:?}");
        };
        assert_eq!(*sample, None, "a switch reads no key");
        let (scan, profile) = connect_of(&effects);
        assert_eq!(profile.db, 3);
        assert_ne!(scan, old);
        assert!(app.dbs.is_none());
        let s = app.session.as_ref().unwrap();
        assert_eq!(s.profile.db, 3);
        assert_eq!(s.previous, None);
        assert!(s.report.is_none(), "db 0's results are not shown for db 3");
        assert_eq!(s.analysis, None);
        assert_eq!(s.scan, scan);
        // The saved profile is not touched.
        assert_eq!(app.entry(id).unwrap().profile.db, 0);

        // Once connected, Overview offers `r` and nothing analyzes.
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(fixtures::report(true).server),
            perms: fixtures::perms_with_probes(),
            target: crate::conn::ScanTarget::Primary,
        });
        testing::live_ready(&mut app);
        app.handle_key(ch('1'));
        let text = format!("{:?}", testing::render(&app, 120, 36).backend());
        assert!(text.contains("r  Analyze: 10,000 keys"), "{text}");
        assert!(!text.contains("analyzing"), "{text}");
        assert!(text.contains("● connected"), "{text}");
    }

    #[test]
    fn a_rescan_stays_on_the_switched_database() {
        let mut app = ready();
        opened(&mut app);
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let (scan, _) = connect_of(&effects);
        finish(&mut app, scan);
        app.handle_key(ch('1'));
        app.handle_key(ch('r'));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(connect_of(&effects).1.db, 3);
    }

    #[test]
    fn the_switch_lasts_for_the_session_and_a_new_connect_uses_the_saved_db() {
        let mut app = ready();
        opened(&mut app);
        app.handle_key(ch('j'));
        app.handle_key(key(KeyCode::Enter));
        // Back to Connections and connect again: the profile's own db.
        app.handle_key(ch('0'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let (_, profile) = connect_of(&effects);
        assert_eq!(profile.db, 0);
        assert_eq!(app.session.as_ref().unwrap().profile.db, 0);
    }

    #[test]
    fn s_asks_first_and_n_saves_and_switches_nothing() {
        let mut app = ready();
        opened(&mut app);
        app.handle_key(ch('j'));
        assert!(app.handle_key(ch('s')).is_empty());
        assert_eq!(app.dbs.as_ref().unwrap().ask, Some(3));
        let scan = ids(&app).1;
        assert!(app.handle_key(ch('n')).is_empty());
        assert_eq!(app.dbs.as_ref().unwrap().ask, None);
        assert_eq!(ids(&app).1, scan);
        assert_eq!(app.session.as_ref().unwrap().profile.db, 0);
        // esc on the question goes back to the list, not out of it.
        app.handle_key(ch('s'));
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(app.dbs.is_some());
        assert_eq!(app.dbs.as_ref().unwrap().ask, None);
    }

    #[test]
    fn y_switches_and_saves_the_database() {
        let mut app = ready();
        opened(&mut app);
        let (id, _) = ids(&app);
        app.handle_key(ch('j'));
        app.handle_key(ch('s'));
        let effects = app.handle_key(ch('y'));
        assert_eq!(effects.len(), 2, "{effects:?}");
        assert_eq!(connect_of(&effects[..1]).1.db, 3);
        assert!(matches!(
            &effects[1],
            Effect::SaveDb { name, db: 3 } if name == "prod-cache-01"
        ));
        assert!(app.dbs.is_none());
        // Until the file is written the entry is as it was; then it shows the new db.
        assert_eq!(app.entry(id).unwrap().profile.db, 0);
        app.apply(Msg::DbSaved {
            name: "prod-cache-01".into(),
            db: 3,
        });
        assert_eq!(app.entry(id).unwrap().profile.db, 3);
        // A new connect now starts on it.
        app.handle_key(ch('0'));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(connect_of(&effects).1.db, 3);
    }

    #[test]
    fn a_failed_save_says_so_and_the_switch_stands() {
        let mut app = ready();
        opened(&mut app);
        let (id, _) = ids(&app);
        app.handle_key(ch('j'));
        app.handle_key(ch('s'));
        app.handle_key(ch('y'));
        app.apply(Msg::DbSaveFailed {
            name: "prod-cache-01".into(),
            db: 3,
            error: "permission denied".into(),
        });
        let n = app.notice.clone().unwrap();
        assert!(n.error);
        assert!(n.text.contains("Could not save db 3"), "{}", n.text);
        assert!(n.text.contains("still applies"), "{}", n.text);
        assert_eq!(app.session.as_ref().unwrap().profile.db, 3);
        assert_eq!(app.entry(id).unwrap().profile.db, 0);
    }

    #[test]
    fn s_on_the_current_database_saves_without_a_rescan() {
        let mut app = ready();
        opened(&mut app);
        app.handle_key(ch('s'));
        let effects = app.handle_key(ch('y'));
        assert!(matches!(&effects[..], [Effect::SaveDb { db: 0, .. }]));
    }

    fn docker_app() -> App {
        let mut app = App::new(vec![]);
        app.apply(Msg::DockerFound(vec![fixtures::docker_instance()]));
        let effects = app.handle_key(key(KeyCode::Enter));
        let (scan, _) = connect_of(&effects);
        finish(&mut app, scan);
        testing::live_ready(&mut app);
        app
    }

    #[test]
    fn a_container_can_switch_but_not_save() {
        let mut app = docker_app();
        opened(&mut app);
        app.handle_key(ch('j'));
        assert!(app.handle_key(ch('s')).is_empty());
        let l = app.dbs.as_ref().unwrap();
        assert_eq!(l.ask, None);
        assert_eq!(l.note, Some(NO_PROFILE));
        // enter still switches, for the session.
        let effects = app.handle_key(key(KeyCode::Enter));
        let (_, profile) = connect_of(&effects);
        assert_eq!(profile.db, 3);
        assert!(!effects.iter().any(|e| matches!(e, Effect::SaveDb { .. })));
    }

    #[test]
    fn a_running_monitor_stops_with_the_switch() {
        let mut app = testing::scanned();
        app.session.as_mut().unwrap().profile.env = crate::model::Env::Stage;
        testing::live_ready(&mut app);
        testing::show(&mut app, Screen::Monitor);
        app.handle_key(key(KeyCode::Enter));
        let effects = app.handle_key(ch('y'));
        assert!(effects.iter().any(|e| matches!(e, Effect::Monitor { .. })));
        assert!(app.views.server.monitor.running());
        opened(&mut app);
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let (scan, _) = connect_of(&effects);
        assert!(!app.views.server.monitor.running());
        // Starting it again asks first: nothing is sent by the key that starts it.
        finish(&mut app, scan);
        let (id, scan) = ids(&app);
        app.apply(Msg::LiveReady { id, scan });
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(!effects.iter().any(|e| matches!(e, Effect::Monitor { .. })));
    }

    #[test]
    fn a_running_tail_subscribes_again_to_the_same_channel_once_the_new_live_is_ready() {
        for to in ["orders:*", "news"] {
            let mut app = testing::scanned();
            testing::live_ready(&mut app);
            testing::show(&mut app, Screen::PubSub);
            app.handle_key(ch('s'));
            for c in to.chars() {
                app.handle_key(ch(c));
            }
            let effects = app.handle_key(key(KeyCode::Enter));
            let want = Subscription::parse(to).unwrap();
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::Subscribe { to, .. } if *to == want))
            );
            let (id, old_scan) = ids(&app);

            opened(&mut app);
            app.handle_key(ch('j'));
            let effects = app.handle_key(key(KeyCode::Enter));
            let (scan, _) = connect_of(&effects);
            // Not yet: the new live connection is not open.
            assert!(
                !effects
                    .iter()
                    .any(|e| matches!(e, Effect::Subscribe { .. }))
            );
            app.apply(Msg::Connected {
                id,
                scan,
                server: None,
                perms: fixtures::perms_with_probes(),
                target: crate::conn::ScanTarget::Primary,
            });
            let effects = app.apply(Msg::LiveReady { id, scan });
            let [
                Effect::Subscribe {
                    scan: s,
                    to: got,
                    profile,
                    ..
                },
            ] = &effects[..]
            else {
                panic!("expected one subscribe, got {effects:?}");
            };
            assert_eq!(*s, scan);
            assert_ne!(*s, old_scan);
            assert_eq!(*got, want);
            assert_eq!(profile.db, 3);
        }
    }

    #[test]
    fn a_stopped_tail_is_not_subscribed_again() {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        testing::show(&mut app, Screen::PubSub);
        app.handle_key(ch('s'));
        for c in "news".chars() {
            app.handle_key(ch(c));
        }
        app.handle_key(key(KeyCode::Enter));
        // esc ends the tail.
        app.handle_key(key(KeyCode::Esc));
        opened(&mut app);
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let (scan, _) = connect_of(&effects);
        let (id, _) = ids(&app);
        let effects = app.apply(Msg::LiveReady { id, scan });
        assert!(effects.is_empty(), "{effects:?}");
    }

    #[test]
    fn a_closed_live_connection_closes_the_list() {
        let mut app = ready();
        opened(&mut app);
        let (id, scan) = ids(&app);
        app.apply(Msg::LiveClosed {
            id,
            scan,
            error: None,
        });
        assert!(app.dbs.is_none());
    }

    fn text(app: &App, w: u16, h: u16) -> String {
        let t = testing::render(app, w, h);
        let b = t.backend().buffer();
        let mut out = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                out.push_str(b[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_header_shows_db_only_when_it_is_not_0() {
        let mut app = ready();
        let header: String = text(&app, 80, 24).lines().take(3).collect();
        assert!(!header.contains("db "), "{header}");
        app.session.as_mut().unwrap().profile.db = 3;
        let header: String = text(&app, 80, 24).lines().take(3).collect();
        assert!(header.contains("db 3"), "{header}");
    }

    #[test]
    fn the_list_open_80x24() {
        let mut app = ready();
        opened(&mut app);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn the_list_with_a_refused_info_80x24() {
        let mut app = ready();
        let effects = app.handle_key(ch('b'));
        let req = testing::requests(&effects)[0].0;
        testing::reply(
            &mut app,
            req,
            Err(LiveError::Denied("INFO keyspace".into())),
        );
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn the_save_question_80x24() {
        let mut app = ready();
        opened(&mut app);
        app.handle_key(ch('j'));
        app.handle_key(ch('s'));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn the_list_of_a_container_after_s_80x24() {
        let mut app = docker_app();
        opened(&mut app);
        app.handle_key(ch('s'));
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn the_header_after_a_switch_80x24() {
        let mut app = ready();
        opened(&mut app);
        app.handle_key(ch('j'));
        let effects = app.handle_key(key(KeyCode::Enter));
        let (scan, _) = connect_of(&effects);
        finish(&mut app, scan);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }
}
