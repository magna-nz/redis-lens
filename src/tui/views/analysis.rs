//! State of the Analysis view's Big keys and Expiry panels (spec "2 Analysis").
//! Owner: wave B2. Drawn by `ui/analysis.rs`; its live requests are in
//! `src/live/keys.rs`.
//!
//! Big keys lists the report's 50 biggest sampled keys. When it comes on screen it reads
//! their lengths, and the encodings that the Recommendations need
//! ([`crate::analysis::keys_to_check`]), once per report. Expiry needs no command. See
//! [`super`] for the interface `App` calls.

use std::collections::HashMap;
use std::time::Instant;

use crossterm::event::KeyEvent;

use super::keys::{list_move, plain};
use super::{Act, ViewState};
use crate::analysis::{KeyFacts, keys_to_check};
use crate::live::keys::{FactKey, Reply, Request};
use crate::live::{Dest, Follow, LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::ScanReport;
use crate::tui::app::Screen;
use crate::tui::clipboard::CopyText;

/// Which report the facts belong to: a rescan brings another one.
type ReportId = (i64, u64, String);

fn report_id(r: &ScanReport) -> ReportId {
    (r.taken_at_unix, r.sampled_keys, r.profile.clone())
}

/// The Big keys and Expiry panels. TTL, Diff and Hot keys stay in `App`.
#[derive(Debug, Default)]
pub struct AnalysisState {
    /// The selected Big keys row.
    pub sel: usize,
    /// The tick when the selection last changed, for the scrolling name.
    pub sel_since: u64,
    /// Lengths and encodings read for the report on screen, by key.
    pub facts: HashMap<String, KeyFacts>,
    /// Commands the server refused or lacks while reading them.
    pub refused: Vec<LiveError>,
    /// Why reading them failed, in redis-lens's own words.
    pub error: Option<String>,
    /// The requests in flight, one per node on a cluster (each key is read on its
    /// slot's owner), for the report they read.
    loading: Option<(Vec<ReqId>, ReportId)>,
    loaded_for: Option<ReportId>,
}

impl AnalysisState {
    /// True while the lengths and encodings are being read.
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// True once they were read (or failed) for the report on screen.
    pub fn loaded(&self, report: &ScanReport) -> bool {
        self.loaded_for.as_ref() == Some(&report_id(report))
    }

    /// Reads the facts for the report on screen when they are missing.
    fn keep_going(&mut self, act: &mut Act<'_>) {
        if act.screen != Screen::BigKeys {
            return;
        }
        if self
            .loading
            .as_ref()
            .is_some_and(|(ids, _)| !ids.iter().any(|id| act.in_flight(*id)))
        {
            self.loading = None;
        }
        let Some(report) = act.report() else { return };
        let rid = report_id(report);
        if self.loaded_for.as_ref() == Some(&rid) || self.loading.is_some() || !act.live_ready() {
            return;
        }
        let big: std::collections::HashSet<&str> =
            report.big_keys().iter().map(|k| k.key.as_str()).collect();
        let keys: Vec<FactKey> = keys_to_check(report)
            .into_iter()
            .map(|(key, key_type)| FactKey {
                length: big.contains(key.as_str()),
                key,
                key_type,
            })
            .collect();
        if keys.is_empty() {
            self.reset_for(rid);
            return;
        }
        let mut ids = Vec::new();
        for (dest, keys) in split_by_node(act.session, keys) {
            if let Some(id) = act.send_to(dest, LiveRequest::Keys(Request::Facts { keys })) {
                ids.push(id);
            }
        }
        if !ids.is_empty() {
            self.facts.clear();
            self.refused.clear();
            self.error = None;
            self.loading = Some((ids, rid));
        }
    }

    /// One node's answer: its facts are added; the report counts as read once every
    /// node answered.
    fn take(&mut self, result: Result<LiveReply, LiveError>) {
        match result {
            Ok(LiveReply::Keys(Reply::Facts { facts, refused })) => {
                self.facts.extend(facts);
                for e in refused {
                    if !self.refused.contains(&e) {
                        self.refused.push(e);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                if self.error.is_none() {
                    self.error = Some(format!("Could not read the lengths: {e}."));
                }
            }
        }
    }

    fn reset_for(&mut self, rid: ReportId) {
        self.facts.clear();
        self.refused.clear();
        self.error = None;
        self.loaded_for = Some(rid);
    }
}

impl ViewState for AnalysisState {
    fn on_show(&mut self, act: &mut Act<'_>) {
        self.keep_going(act);
    }

    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        self.keep_going(act);
    }

    fn on_key(&mut self, key: KeyEvent, act: &mut Act<'_>) -> bool {
        if act.screen != Screen::BigKeys {
            return false;
        }
        let keys = act.report().map_or(&[][..], |r| r.big_keys());
        let len = keys.len();
        let sel = self.sel.min(len.saturating_sub(1));
        if let Some(next) = list_move(&key, sel, len, act) {
            if next != self.sel {
                self.sel = next;
                self.sel_since = act.tick;
            }
            return true;
        }
        if plain(&key) == Some('y') {
            match keys.get(sel) {
                Some(k) => {
                    let text = CopyText::key(&k.key);
                    act.copy(text);
                }
                None => act.info("Nothing selected to copy."),
            }
            return true;
        }
        false
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, _act: &mut Act<'_>) {
        let Some((ids, _)) = self.loading.as_mut() else {
            return;
        };
        let Some(at) = ids.iter().position(|w| *w == id) else {
            return;
        };
        ids.remove(at);
        let last = ids.is_empty();
        self.take(result);
        if last && let Some((_, rid)) = self.loading.take() {
            self.loaded_for = Some(rid);
        }
    }
}

/// `keys` grouped by the node that owns each key's slot, in node order, each group sent
/// without following a redirect (a key that moved is left without facts): on a
/// standalone server one group for its one node. Keys whose slot no node serves are
/// left out.
pub(crate) fn split_by_node(
    session: &crate::tui::app::Session,
    keys: Vec<FactKey>,
) -> Vec<(Dest, Vec<FactKey>)> {
    if !session.is_cluster() {
        return vec![(Dest::node(0), keys)];
    }
    let mut by: std::collections::BTreeMap<usize, Vec<FactKey>> = Default::default();
    for k in keys {
        if let Some(i) = session.owner_of(k.key.as_bytes()) {
            by.entry(i).or_default().push(k);
        }
    }
    by.into_iter()
        .map(|(i, keys)| (Dest::node(i).follow(Follow::Never), keys))
        .collect()
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::testing::{self, key};
    use super::*;
    use crate::model::{KeySample, KeyType};
    use crate::tui::app::{App, Effect, Msg};

    /// The fixture report with Big keys and Expiry figures, as a scan would write them:
    /// the big keys from every pattern's examples, and bytes per TTL bucket split like
    /// the keys.
    fn fill(r: &mut ScanReport) {
        let all: Vec<KeySample> = r.patterns.iter().flat_map(|p| p.examples.clone()).collect();
        r.key_sizes.get_or_insert_default().big_keys = crate::scan::big_keys(&all);
        let mut b = [0u64; 7];
        for p in &r.patterns {
            let bytes = p.sampled_bytes.unwrap_or(0);
            for (slot, n) in b.iter_mut().zip(p.ttl_hist) {
                *slot += bytes * n / p.sampled_keys.max(1);
            }
        }
        r.key_sizes.get_or_insert_default().ttl_bytes = b;
    }

    fn app_on(screen: Screen) -> (App, Vec<Effect>) {
        let mut app = testing::scanned();
        fill(
            app.session
                .as_mut()
                .and_then(|s| s.report.as_mut())
                .expect("a report"),
        );
        let mut out = testing::live_ready(&mut app);
        out.extend(testing::show(&mut app, screen));
        (app, out)
    }

    fn facts_request(effects: &[Effect]) -> (ReqId, Vec<FactKey>) {
        let reqs = testing::requests(effects);
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        match &reqs[0] {
            (id, LiveRequest::Keys(Request::Facts { keys })) => (*id, keys.clone()),
            other => panic!("not a facts request: {other:?}"),
        }
    }

    fn fact(len: Option<u64>, enc: &str) -> KeyFacts {
        KeyFacts {
            length: len,
            encoding: Some(enc.to_string()),
        }
    }

    /// Big keys with the lengths and encodings read.
    fn big_keys_loaded() -> App {
        let (mut app, effects) = app_on(Screen::BigKeys);
        let (id, keys) = facts_request(&effects);
        let facts: Vec<(String, KeyFacts)> = keys
            .iter()
            .map(|k| {
                let f = match k.key.as_str() {
                    "queue:emails" => fact(Some(2_104_332), "quicklist"),
                    "user:204551:feed" => fact(Some(1_310), "skiplist"),
                    "user:88213:cart" => fact(Some(1_204), "hashtable"),
                    _ if k.key_type == KeyType::String => fact(Some(1_800), "raw"),
                    _ => fact(k.length.then_some(12), "listpack"),
                };
                (k.key.clone(), f)
            })
            .collect();
        let reply = Ok(LiveReply::Keys(Reply::Facts {
            facts,
            refused: Vec::new(),
        }));
        testing::reply(&mut app, id, reply);
        app
    }

    #[test]
    fn big_keys_reads_lengths_for_the_big_keys_only_and_encodings_once() {
        let (app, effects) = app_on(Screen::BigKeys);
        let (_, keys) = facts_request(&effects);
        let report = app.session.as_ref().unwrap().report.as_ref().unwrap();
        assert_eq!(report.big_keys().len(), 50);
        let with_length: Vec<&str> = keys
            .iter()
            .filter(|k| k.length)
            .map(|k| k.key.as_str())
            .collect();
        let big: Vec<&str> = report.big_keys().iter().map(|k| k.key.as_str()).collect();
        assert_eq!(with_length, big);
        // The examples of hash, set and sorted set patterns, for their encodings.
        let want = keys_to_check(report);
        let asked: Vec<(String, KeyType)> =
            keys.iter().map(|k| (k.key.clone(), k.key_type)).collect();
        assert_eq!(asked, want);
        assert!(keys.iter().any(|k| k.key == "user:931:feed"));
        let mut names: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), keys.len(), "no key twice");
    }

    #[test]
    fn big_keys_reads_once_per_report() {
        let mut app = big_keys_loaded();
        let effects = app.handle_key(key(KeyCode::Tab));
        assert!(testing::requests(&effects).is_empty());
        let effects = app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.screen(), Screen::BigKeys);
        assert!(testing::requests(&effects).is_empty(), "{effects:?}");
    }

    #[test]
    fn big_keys_80x24() {
        let app = big_keys_loaded();
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn big_keys_120x36() {
        let mut app = big_keys_loaded();
        app.handle_key(key(KeyCode::Down));
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }

    #[test]
    fn big_keys_while_reading_lengths() {
        let (app, _) = app_on(Screen::BigKeys);
        let screen = format!("{:?}", testing::render(&app, 120, 36).backend());
        assert!(screen.contains("reading lengths"), "{screen}");
    }

    #[test]
    fn y_copies_the_selected_key_name() {
        let mut app = big_keys_loaded();
        app.handle_key(key(KeyCode::End));
        let effects = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        let report = app.session.as_ref().unwrap().report.as_ref().unwrap();
        let last = report.big_keys().last().unwrap().key.clone();
        let copied: Vec<&str> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Copy(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(copied, [last.as_str()]);
    }

    #[test]
    fn a_refused_encoding_skips_its_rule_in_fixed_words() {
        let (mut app, effects) = app_on(Screen::BigKeys);
        let (id, _) = facts_request(&effects);
        let reply = Ok(LiveReply::Keys(Reply::Facts {
            facts: Vec::new(),
            refused: vec![LiveError::Denied("OBJECT ENCODING".into())],
        }));
        testing::reply(&mut app, id, reply);
        let screen = format!("{:?}", testing::render(&app, 120, 36).backend());
        assert!(
            screen.contains("the server refused OBJECT ENCODING for this user"),
            "{screen}"
        );
        assert!(!screen.contains("reading lengths"), "{screen}");
    }

    #[test]
    fn without_sizes_big_keys_and_expiry_say_why() {
        let mut app = testing::scanned();
        if let Some(r) = app.session.as_mut().and_then(|s| s.report.as_mut()) {
            r.sizes_available = false;
        }
        testing::live_ready(&mut app);
        testing::show(&mut app, Screen::BigKeys);
        let screen = format!("{:?}", testing::render(&app, 80, 24).backend());
        assert!(
            screen.contains("No sizes: MEMORY USAGE is not allowed"),
            "{screen}"
        );
        testing::show(&mut app, Screen::Expiry);
        let screen = format!("{:?}", testing::render(&app, 80, 24).backend());
        assert!(
            screen.contains("No sizes: MEMORY USAGE is not allowed"),
            "{screen}"
        );
    }

    #[test]
    fn expiry_sends_nothing() {
        // Getting there passes Big keys, which asks for its facts once.
        let (mut app, effects) = app_on(Screen::Expiry);
        let (id, _) = facts_request(&effects);
        let mut out = Vec::new();
        for _ in 0..20 {
            out.extend(app.apply(Msg::Tick));
        }
        out.extend(app.handle_key(key(KeyCode::Down)));
        assert!(testing::requests(&out).is_empty(), "{out:?}");
        // A reply that lands while Expiry is on screen is still kept for Big keys.
        let reply = Ok(LiveReply::Keys(Reply::Facts {
            facts: Vec::new(),
            refused: Vec::new(),
        }));
        testing::reply(&mut app, id, reply);
        let report = app.session.as_ref().unwrap().report.as_ref().unwrap();
        assert!(app.views.analysis.loaded(report));
    }

    #[test]
    fn expiry_80x24() {
        let (app, _) = app_on(Screen::Expiry);
        insta::assert_snapshot!(testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn expiry_120x36() {
        let (app, _) = app_on(Screen::Expiry);
        insta::assert_snapshot!(testing::render(&app, 120, 36).backend());
    }
}
