//! `ctrl-f`, Find key: a one-line prompt in place of the footer, from any screen once
//! connected. `enter` lists the keys of the whole server whose names match on Browse ›
//! Keys, in the key list ([`KeysState::open_find`](super::views::keys::KeysState::open_find)),
//! with only reads, so it works on read-only profiles and before any analysis. `esc` or
//! `←` on the results goes back to the screen the find started from.
//!
//! The dialogs, the command line, the add flow and the `?` help take every key while
//! they are open, so `ctrl-f` does nothing there. A list's own filter input (Patterns,
//! Connections, the key list) closes, keeping its text; any other text input of a view
//! (a form, a query) stays as it is under the prompt and is back after `esc`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::views::LiveState;
use super::{App, Effect, Owner, Screen};
use crate::live::keys::{find_glob, find_refusal};
use crate::tui::ui::CONNECT_FIRST;

/// The longest find text, in bytes.
pub const MAX_FIND: usize = 512;

/// `enter` before the live connection is open.
pub const NOT_YET: &str = "Still connecting. Press ctrl-f, then enter, once connected.";

/// `enter` once the live connection has closed.
pub const CLOSED: &str =
    "The live connection is closed, so keys can't be searched. Connect again on 0 Connections.";

/// The `ctrl-f` prompt, and where its results go back to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindPrompt {
    /// The prompt is open: it takes every key.
    pub open: bool,
    /// What is typed: a name, or a glob with `*`, `?` or `[`.
    pub text: String,
    /// The text last searched: `ctrl-f` starts from it.
    pub last: String,
    /// The screen `esc` or `←` on the results goes back to.
    pub back: Option<Screen>,
}

/// What a key did to the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindKey {
    /// Typed, or nothing to do: the prompt stays.
    Typing,
    /// `esc`: closed, nothing searched.
    Cancel,
    /// `enter` on text: search for it.
    Run(String),
}

/// `ctrl-f`.
pub fn is_find(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('f' | 'F')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

impl FindPrompt {
    /// Opens the prompt with the last search's text.
    pub fn start(&mut self) {
        self.open = true;
        self.text = self.last.clone();
    }

    /// A key while the prompt is open.
    pub fn key(&mut self, key: &KeyEvent) -> FindKey {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.open = false;
                self.text = self.last.clone();
                return FindKey::Cancel;
            }
            // Spaces around the text are left out: a name rarely starts or ends with
            // one, and a pasted name often does.
            KeyCode::Enter if self.text.trim().is_empty() => {}
            KeyCode::Enter => {
                self.open = false;
                self.text = self.text.trim().to_string();
                self.last = self.text.clone();
                return FindKey::Run(self.text.clone());
            }
            KeyCode::Backspace => {
                self.text.pop();
            }
            KeyCode::Char('u') if ctrl => self.text.clear(),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.push(c);
            }
            _ => {}
        }
        FindKey::Typing
    }

    /// Pasted text, control characters left out, up to [`MAX_FIND`] bytes.
    pub fn paste(&mut self, text: &str) {
        for c in text.chars() {
            if !self.push(c) && !c.is_control() {
                break;
            }
        }
    }

    /// Adds `c` unless it is a control character or would pass [`MAX_FIND`]. True when
    /// added.
    fn push(&mut self, c: char) -> bool {
        if c.is_control() || self.text.len() + c.len_utf8() > MAX_FIND {
            return false;
        }
        self.text.push(c);
        true
    }
}

impl App {
    /// `ctrl-f`: opens the prompt with the last search's text. Before connecting it
    /// says to connect first.
    pub(super) fn open_find(&mut self) -> Vec<Effect> {
        if self.session.is_none() {
            self.set_info(CONNECT_FIRST.trim_start());
            return Vec::new();
        }
        // A list's own filter input closes, keeping its text (as `enter` does).
        self.patterns.filtering = false;
        self.conn_filtering = false;
        self.views.keys.filtering = false;
        self.find.start();
        Vec::new()
    }

    /// A key while the prompt is open: it takes every key.
    pub(super) fn find_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match self.find.key(&key) {
            FindKey::Typing | FindKey::Cancel => Vec::new(),
            FindKey::Run(text) => self.run_find(&text),
        }
    }

    /// `enter` on `text`: the find's results on Browse › Keys, replacing the key list
    /// shown there (a pattern's keys or an earlier find).
    fn run_find(&mut self, text: &str) -> Vec<Effect> {
        let Some(s) = self.session.as_ref() else {
            self.set_info(CONNECT_FIRST.trim_start());
            return Vec::new();
        };
        // Until the live connection is open the cluster's nodes may not be known yet, so
        // a walk would read one node and call itself done. The prompt closes to show why
        // (a notice can't show beside it); `ctrl-f` brings the text back.
        let not_ready = match &s.live {
            LiveState::Ready => None,
            LiveState::Waiting => Some(NOT_YET),
            LiveState::Closed(_) => Some(CLOSED),
        };
        if let Some(why) = not_ready {
            self.set_info(why);
            return Vec::new();
        }
        if let Some(why) = find_refusal(text) {
            self.set_info(why);
            return Vec::new();
        }
        let here = self.screen();
        // A form open on a key's value is dropped: the find replaces the list the value
        // was opened from, and opening the same key again must start afresh.
        self.views.value.form = None;
        let on_results =
            self.views.keys.find.is_some() && matches!(here, Screen::KeyList | Screen::KeyDetail);
        // A new find from the results goes back where the first one started. One from a
        // pattern's keys goes back to the patterns: those keys are replaced.
        if !on_results || self.find.back.is_none() {
            self.find.back = Some(match here {
                Screen::KeyList | Screen::KeyDetail => Screen::Patterns,
                s => s,
            });
        }
        let glob = find_glob(text);
        let mut out = self
            .with_views(Owner::Keys, "open", |v, act| {
                let nodes = act.session.nodes.clone();
                v.keys.open_find(glob, &nodes, act);
            })
            .map_or_else(Vec::new, |(_, e)| e);
        if self.screen() == Screen::KeyList {
            // Already on the key list: `switch` would tell the view nothing.
            out.extend(self.on_active("show", |v, act| v.on_show(act)));
        } else {
            out.extend(self.switch(Screen::KeyList));
        }
        out
    }

    /// `esc` or `←` on a find's results (no filter left to clear): back to the screen
    /// the find started from. Browse › Keys shows the patterns again next time.
    pub(super) fn leave_find(&mut self) -> Vec<Effect> {
        let to = self.find.back.take().unwrap_or(Screen::Patterns);
        let out = self.switch(to);
        self.patterns.keys_open = false;
        self.patterns.detail_open = false;
        out
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::views::testing::{self, key};
    use super::*;
    use crate::conn::ScanTarget;
    use crate::live::keys::{KeyRow, Page, Reply, Request};
    use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
    use crate::model::{Env, KeyType, Ttl};
    use crate::tui::app::Msg;
    use crate::tui::fixtures;

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_text(app: &mut App, s: &str) -> Vec<Effect> {
        s.chars().flat_map(|c| app.handle_key(ch(c))).collect()
    }

    fn notice(app: &App) -> String {
        app.notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    fn text(t: &Terminal<TestBackend>) -> String {
        let buf = t.backend().buffer();
        let area = buf.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn last_line(app: &App, w: u16, h: u16) -> String {
        text(&testing::render(app, w, h))
            .lines()
            .last()
            .unwrap_or_default()
            .to_string()
    }

    /// The one find page request among `effects`.
    fn find_request(effects: &[Effect]) -> (ReqId, Request) {
        let reqs: Vec<_> = testing::requests(effects)
            .into_iter()
            .filter(|(_, r)| matches!(r, LiveRequest::Keys(_)))
            .collect();
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        match &reqs[0] {
            (id, LiveRequest::Keys(r @ Request::Find { .. })) => (*id, r.clone()),
            other => panic!("not a find: {other:?}"),
        }
    }

    fn row(key: &str, t: KeyType, bytes: u64, len: u64) -> KeyRow {
        KeyRow {
            key: key.into(),
            key_type: t,
            ttl: Ttl::Persistent,
            bytes: Some(bytes),
            encoding: Some("listpack".into()),
            length: Some(len),
            freq: Some(3),
        }
    }

    fn page(rows: Vec<KeyRow>, cursor: u64) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Keys(Reply::Page(Page {
            rows,
            cursor,
            carry: Vec::new(),
            other_pattern: 0,
            unreadable: 0,
            refused: Vec::new(),
        })))
    }

    fn carts() -> Vec<KeyRow> {
        vec![
            row("user:10293:cart", KeyType::Hash, 3_174, 12),
            row("cart:archive:2024", KeyType::List, 90_112, 1_204),
            row("promo:cart-banner", KeyType::String, 96, 41),
        ]
    }

    /// A scanned app with the live connection open, on `screen`.
    fn on(screen: Screen) -> App {
        testing::app_on(screen).0
    }

    /// `ctrl-f`, `text`, `enter`: the effects of `enter`.
    fn find(app: &mut App, text: &str) -> Vec<Effect> {
        app.handle_key(ctrl('f'));
        assert!(app.find.open, "the prompt opens");
        app.handle_key(ctrl('u'));
        type_text(app, text);
        app.handle_key(key(KeyCode::Enter))
    }

    /// Found from Overview, one page of three keys, the walk not done.
    fn results() -> App {
        let mut app = on(Screen::Overview);
        let (id, _) = find_request(&find(&mut app, "cart"));
        testing::reply(&mut app, id, page(carts(), 4_242));
        app
    }

    // -- The prompt --------------------------------------------------------------

    #[test]
    fn the_prompt_edits_its_text() {
        let mut p = FindPrompt::default();
        p.start();
        for c in "ab".chars() {
            assert_eq!(p.key(&ch(c)), FindKey::Typing);
        }
        p.key(&key(KeyCode::Backspace));
        assert_eq!(p.text, "a");
        // Control characters and alt are ignored.
        p.key(&ch('\u{1b}'));
        p.key(&ch('\n'));
        p.key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        p.key(&ctrl('x'));
        assert_eq!(p.text, "a");
        p.key(&ctrl('u'));
        assert_eq!(p.text, "");
        // An empty enter does nothing; the prompt stays.
        assert_eq!(p.key(&key(KeyCode::Enter)), FindKey::Typing);
        assert!(p.open);
        p.key(&ch('c'));
        assert_eq!(p.key(&key(KeyCode::Enter)), FindKey::Run("c".into()));
        assert!(!p.open);
        assert_eq!(p.last, "c");
        // The last text searched comes back; esc drops what was typed since.
        p.start();
        assert_eq!(p.text, "c");
        p.key(&ch('d'));
        assert_eq!(p.key(&key(KeyCode::Esc)), FindKey::Cancel);
        assert!(!p.open);
        p.start();
        assert_eq!(p.text, "c");
    }

    #[test]
    fn the_text_holds_at_most_512_bytes() {
        let mut p = FindPrompt::default();
        p.start();
        for _ in 0..600 {
            p.key(&ch('é'));
        }
        assert_eq!(p.text.len(), MAX_FIND);
        let mut p = FindPrompt::default();
        p.start();
        p.paste(&format!("a\nb\tc\u{7}{}", "x".repeat(600)));
        assert!(p.text.starts_with("abcx"), "{}", p.text);
        assert_eq!(p.text.len(), MAX_FIND);
        assert!(!p.text.chars().any(char::is_control));
    }

    #[test]
    fn before_connecting_it_says_connect_first() {
        let mut app = App::new(fixtures::profiles());
        assert!(app.handle_key(ctrl('f')).is_empty());
        assert!(!app.find.open);
        assert_eq!(
            notice(&app),
            "Connect first: open 0 Connections, pick a connection and press enter."
        );
    }

    #[test]
    fn ctrl_f_opens_the_prompt_on_every_screen() {
        for screen in Screen::ALL {
            if matches!(screen, Screen::Nodes | Screen::Slots) {
                // Panels of a cluster only: see snapshot_tests_cluster.rs.
                continue;
            }
            let mut app = on(screen);
            assert!(app.handle_key(ctrl('f')).is_empty(), "{screen:?}");
            assert!(app.find.open, "{screen:?}");
            assert_eq!(app.screen(), screen);
            let foot = last_line(&app, 80, 24);
            assert!(
                foot.contains("Find key:") && foot.contains("a name, or a pattern with * ? [ ]"),
                "{screen:?}: {foot}"
            );
            // Every key goes to it: `q` is text, not quit.
            let effects = app.handle_key(ch('q'));
            assert!(effects.is_empty(), "{screen:?}: {effects:?}");
            assert_eq!(app.find.text, "q");
        }
    }

    #[test]
    fn a_lists_filter_closes_keeping_its_text() {
        // Patterns.
        let mut app = on(Screen::Patterns);
        app.handle_key(ch('/'));
        type_text(&mut app, "us");
        app.handle_key(ctrl('f'));
        assert!(app.find.open && !app.patterns.filtering);
        assert_eq!(app.patterns.filter, "us");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (app.screen(), app.patterns.filter.as_str()),
            (Screen::Patterns, "us")
        );
        // Connections.
        let mut app = on(Screen::Connections);
        app.handle_key(ch('/'));
        type_text(&mut app, "pro");
        app.handle_key(ctrl('f'));
        assert!(app.find.open && !app.conn_filtering);
        assert_eq!(app.conn_filter, "pro");
        // The key list.
        let mut app = on(Screen::KeyList);
        app.handle_key(ch('/'));
        type_text(&mut app, "10");
        app.handle_key(ctrl('f'));
        assert!(app.find.open && !app.views.keys.filtering);
        assert_eq!(app.views.keys.filter, "10");
    }

    #[test]
    fn another_views_text_input_waits_under_the_prompt() {
        let mut app = on(Screen::PubSub);
        app.handle_key(ch('+'));
        type_text(&mut app, "orders");
        assert!(app.view_input().is_some());
        app.handle_key(ctrl('f'));
        assert!(app.find.open);
        type_text(&mut app, "x");
        app.handle_key(key(KeyCode::Esc));
        // Back as it was, its text kept.
        let input = app.view_input().expect("the subscribe input");
        assert_eq!(input.text, "orders");
    }

    #[test]
    fn ctrl_f_does_nothing_while_a_dialog_the_command_line_or_help_is_open() {
        // `?` help.
        let mut app = on(Screen::Patterns);
        app.handle_key(ch('?'));
        app.handle_key(ctrl('f'));
        assert!(app.help && !app.find.open);
        // The command line.
        let mut app = on(Screen::Patterns);
        app.handle_key(ch(':'));
        assert!(app.views.cmdline.open);
        app.handle_key(ctrl('f'));
        assert!(!app.find.open);
        // The database list.
        let mut app = on(Screen::Patterns);
        app.handle_key(ch('b'));
        assert!(app.dbs.is_some());
        app.handle_key(ctrl('f'));
        assert!(!app.find.open);
        // "Analyze the keys?".
        let mut app = on(Screen::Patterns);
        app.handle_key(ch('a'));
        assert!(app.analyze.is_some());
        app.handle_key(ctrl('f'));
        assert!(!app.find.open);
        // The add flow.
        let mut app = on(Screen::Connections);
        app.handle_key(ch('+'));
        assert!(app.add.is_some());
        app.handle_key(ctrl('f'));
        assert!(!app.find.open);
        // A bulk dialog, with writes on.
        let mut app = on(Screen::Patterns);
        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Local;
        s.profile.read_only = None;
        let at = app
            .pattern_rows()
            .iter()
            .position(|r| matches!(r, crate::tui::tree::Row::Pattern { .. }))
            .unwrap();
        app.patterns.sel = at;
        app.handle_key(ch('d'));
        assert!(app.bulk.is_some(), "{}", notice(&app));
        app.handle_key(ctrl('f'));
        assert!(!app.find.open);
        // The write gate's question.
        let mut app = on(Screen::KeyList);
        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Prod;
        s.profile.read_only = Some(false);
        app.handle_key(ch('+'));
        type_text(&mut app, "9");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "v");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.confirm.is_some());
        app.handle_key(ctrl('f'));
        assert!(!app.find.open);
    }

    #[test]
    fn help_finds_it_and_runs_it() {
        let mut app = on(Screen::Overview);
        app.handle_key(ch('?'));
        type_text(&mut app, "find keys");
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.help);
        assert!(app.find.open);
        assert_eq!(app.screen(), Screen::Overview);
    }

    // -- Running a find ----------------------------------------------------------

    #[test]
    fn enter_lists_the_matching_keys_with_a_find_request() {
        let mut app = on(Screen::Overview);
        // Empty: nothing is sent and the prompt stays.
        app.handle_key(ctrl('f'));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(testing::requests(&effects).is_empty());
        assert!(app.find.open);
        type_text(&mut app, "cart");
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(!app.find.open);
        assert_eq!(app.screen(), Screen::KeyList);
        let (_, req) = find_request(&effects);
        assert_eq!(
            req,
            Request::Find {
                glob: "*cart*".into(),
                cursor: 0,
                carry: Vec::new(),
                // The fixture's scan had sizes and an LFU policy, all allowed.
                sizes: true,
                freq: true,
            }
        );
        // Only reads, on a read-only (prod) profile.
        assert!(app.session.as_ref().unwrap().profile.is_read_only());
        assert_eq!(app.views.keys.find.as_deref(), Some("*cart*"));
        assert_eq!(app.views.keys.pattern, None);
    }

    #[test]
    fn a_glob_is_sent_as_typed() {
        let mut app = on(Screen::Patterns);
        let (_, req) = find_request(&find(&mut app, "user:*:cart"));
        assert!(matches!(req, Request::Find { glob, .. } if glob == "user:*:cart"));
        let mut app = on(Screen::Patterns);
        let (_, req) = find_request(&find(&mut app, "a\\b"));
        assert!(matches!(req, Request::Find { glob, .. } if glob == "*a\\\\b*"));
    }

    #[test]
    fn esc_in_the_prompt_leaves_everything_as_it_was() {
        let mut app = results();
        let rows = app.views.keys.rows.len();
        app.handle_key(ch('j'));
        let sel = app.views.keys.sel;
        app.handle_key(ctrl('f'));
        assert_eq!(app.find.text, "cart", "prefilled with the last search");
        type_text(&mut app, "zzz");
        let effects = app.handle_key(key(KeyCode::Esc));
        assert!(testing::requests(&effects).is_empty());
        assert!(!app.find.open);
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.find.as_deref(), Some("*cart*"));
        assert_eq!((app.views.keys.rows.len(), app.views.keys.sel), (rows, sel));
        app.handle_key(ctrl('f'));
        assert_eq!(app.find.text, "cart");
    }

    /// Find results are of no pattern: `T` and `D` (the pattern's bulk actions) say so
    /// and start nothing, on any profile.
    #[test]
    fn capital_t_and_d_on_find_results_start_no_bulk_action() {
        let mut app = results();
        let s = app.session.as_mut().unwrap();
        s.profile.env = crate::model::Env::Local;
        s.profile.read_only = None;
        for k in ['T', 'D'] {
            let effects = app.handle_key(ch(k));
            assert!(testing::requests(&effects).is_empty(), "{effects:?}");
            assert!(app.bulk.is_none() && app.confirm.is_none());
            assert_eq!(
                notice(&app),
                "T and D work on every key of a pattern: Find results are of no pattern."
            );
        }
        assert_eq!(app.screen(), Screen::KeyList);
    }

    /// `Y` on Find results copies the pattern the selected key belongs to.
    #[test]
    fn capital_y_on_find_results_copies_the_keys_pattern() {
        let mut app = results();
        let copied: Vec<_> = app
            .handle_key(ch('Y'))
            .into_iter()
            .filter_map(|e| match e {
                Effect::Copy(t) => Some(t.as_str().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(copied, ["cart:archive:{id}"], "{}", notice(&app));
    }

    #[test]
    fn the_results_page_on_with_n_and_open_a_value() {
        let mut app = results();
        assert_eq!(app.views.keys.rows.len(), 3);
        assert_eq!(app.views.keys.other_pattern, 0);
        assert!(last_line(&app, 120, 36).contains("ctrl-f Find key"));
        let foot = text(&testing::render(&app, 120, 36));
        assert!(foot.contains("3 loaded. m loads 1,000 more."), "{foot}");
        assert!(foot.contains("‹ Find: *cart*"), "{foot}");
        // Ticks load nothing more on their own.
        let effects = app.apply(Msg::TickAt(std::time::Instant::now()));
        assert!(testing::requests(&effects).is_empty());
        let (id, req) = find_request(&app.handle_key(ch('m')));
        assert!(
            matches!(req, Request::Find { cursor: 4_242, .. }),
            "{req:?}"
        );
        testing::reply(
            &mut app,
            id,
            page(vec![row("cart:9", KeyType::Set, 10, 1)], 0),
        );
        assert!(app.views.keys.done);
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("4 keys match."), "{all}");
        // `/`, `ctrl-t`, `s` and `y` work as on a pattern's keys.
        app.handle_key(ctrl('t'));
        assert_eq!(app.views.keys.visible().len(), 1);
        app.handle_key(ctrl('t'));
        app.handle_key(ctrl('t'));
        app.handle_key(ctrl('t'));
        app.handle_key(ctrl('t'));
        app.handle_key(ctrl('t'));
        app.handle_key(ctrl('t'));
        app.handle_key(ctrl('t'));
        assert_eq!(app.views.keys.visible().len(), 4);
        app.handle_key(ch('s'));
        assert_eq!(notice(&app), "Keys sorted by length.");
        app.handle_key(ch('/'));
        type_text(&mut app, "promo");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.views.keys.visible().len(), 1);
        let copied = app
            .handle_key(ch('y'))
            .into_iter()
            .any(|e| matches!(e, Effect::Copy(t) if t.as_str() == "promo:cart-banner"));
        assert!(copied);
        // `esc` clears the filter first and stays.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::KeyList);
        // `→` opens the value; `←` there is back on the results.
        app.handle_key(key(KeyCode::Home));
        let effects = app.handle_key(key(KeyCode::Right));
        assert_eq!(app.screen(), Screen::KeyDetail);
        assert!(
            testing::requests(&effects)
                .iter()
                .all(|(_, r)| matches!(r, LiveRequest::Value(_))),
            "only the value is read: {effects:?}"
        );
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("‹ Find: *cart*"), "{all}");
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.rows.len(), 4);
        // `esc` goes back to where the find started, and closes the results.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Overview);
        crate::tui::app::views::testing::area(&mut app, '2');
        assert_eq!(app.screen(), Screen::Patterns);
    }

    #[test]
    fn left_on_the_results_goes_back_to_where_the_find_started() {
        for (from, back) in [
            (Screen::Streams, Screen::Streams),
            (Screen::Clients, Screen::Clients),
            (Screen::Connections, Screen::Connections),
            (Screen::KeyList, Screen::Patterns),
            (Screen::KeyDetail, Screen::Patterns),
        ] {
            let mut app = on(from);
            find(&mut app, "cart");
            assert_eq!(app.screen(), Screen::KeyList, "{from:?}");
            app.handle_key(key(KeyCode::Left));
            assert_eq!(app.screen(), back, "{from:?}");
        }
    }

    #[test]
    fn a_new_find_replaces_the_old_and_keeps_where_to_go_back() {
        let mut app = results();
        let (_, req) = find_request(&find(&mut app, "banner"));
        assert!(matches!(req, Request::Find { glob, cursor: 0, .. } if glob == "*banner*"));
        assert!(app.views.keys.rows.is_empty());
        assert_eq!(app.views.keys.find.as_deref(), Some("*banner*"));
        // Also from a key's value opened from the results.
        let mut app = results();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyDetail);
        find(&mut app, "x");
        assert_eq!(app.screen(), Screen::KeyList);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Overview);
    }

    #[test]
    fn enter_before_the_live_connection_is_open_sends_nothing_and_says_why() {
        for (live, why) in [
            (LiveState::Waiting, NOT_YET),
            (LiveState::Closed(None), CLOSED),
        ] {
            let mut app = on(Screen::Overview);
            app.session.as_mut().unwrap().live = live;
            let effects = find(&mut app, "cart");
            assert!(testing::requests(&effects).is_empty(), "{effects:?}");
            assert_eq!(app.screen(), Screen::Overview);
            assert_eq!(app.views.keys.find, None);
            assert_eq!(notice(&app), why);
            // `ctrl-f` brings the text back.
            app.handle_key(ctrl('f'));
            assert_eq!(app.find.text, "cart");
        }
    }

    #[test]
    fn a_glob_with_too_many_wildcards_sends_nothing() {
        let mut app = on(Screen::Overview);
        let effects = find(&mut app, &"a*".repeat(9));
        assert!(testing::requests(&effects).is_empty(), "{effects:?}");
        assert_eq!(app.screen(), Screen::Overview);
        assert!(
            notice(&app).starts_with("A pattern may have at most 8 *"),
            "{}",
            notice(&app)
        );
    }

    #[test]
    fn spaces_around_the_text_are_left_out() {
        let mut app = on(Screen::Overview);
        let (_, req) = find_request(&find(&mut app, "  cart "));
        assert!(matches!(req, Request::Find { glob, .. } if glob == "*cart*"));
        assert_eq!(app.find.last, "cart");
        // Only spaces is like empty: the prompt stays.
        let mut app = on(Screen::Overview);
        let effects = find(&mut app, "   ");
        assert!(testing::requests(&effects).is_empty());
        assert!(app.find.open);
    }

    #[test]
    fn a_form_open_on_a_value_is_dropped_by_a_find() {
        let mut app = results();
        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Local;
        s.profile.read_only = None;
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyDetail);
        app.handle_key(ch('R'));
        assert!(app.views.value.form.is_some(), "R opens the rename form");
        find(&mut app, "cart");
        assert_eq!(app.screen(), Screen::KeyList);
        assert!(app.views.value.form.is_none());
    }

    #[test]
    fn a_late_page_of_a_replaced_find_is_ignored() {
        let mut app = on(Screen::Overview);
        let (old, _) = find_request(&find(&mut app, "cart"));
        let (new, _) = find_request(&find(&mut app, "banner"));
        assert_ne!(old, new);
        testing::reply(&mut app, old, page(carts(), 0));
        assert!(app.views.keys.rows.is_empty());
        assert!(!app.views.keys.done);
        testing::reply(
            &mut app,
            new,
            page(vec![row("promo:cart-banner", KeyType::String, 96, 41)], 0),
        );
        assert_eq!(app.views.keys.rows.len(), 1);
    }

    #[test]
    fn a_patterns_keys_replace_the_results() {
        let mut app = on(Screen::Patterns);
        find(&mut app, "cart");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Patterns);
        let at = app
            .pattern_rows()
            .iter()
            .position(|r| matches!(r, crate::tui::tree::Row::Pattern { .. }))
            .unwrap();
        app.patterns.sel = at;
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::KeyList);
        assert_eq!(app.views.keys.find, None);
        assert!(app.views.keys.pattern.is_some());
        let reqs = testing::requests(&effects);
        assert!(
            matches!(&reqs[..], [(_, LiveRequest::Keys(Request::Page { .. }))]),
            "{reqs:?}"
        );
        // `esc` there goes back to the patterns, as it always did.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Patterns);
    }

    #[test]
    fn it_works_before_any_analysis() {
        let mut app = App::new(fixtures::profiles());
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, scan, .. }] = effects[..] else {
            panic!("{effects:?}");
        };
        let report = fixtures::report(true);
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(report.server.clone()),
            perms: fixtures::perms_with_probes(),
            target: ScanTarget::Primary,
        });
        app.apply(Msg::LiveReady { id, scan });
        assert!(app.report().is_none());
        let (_, req) = find_request(&find(&mut app, "cart"));
        // MEMORY USAGE is allowed by the permission check; the policy is LFU.
        assert!(
            matches!(
                req,
                Request::Find {
                    sizes: true,
                    freq: true,
                    ..
                }
            ),
            "{req:?}"
        );
        assert_eq!(app.screen(), Screen::KeyList);
    }

    #[test]
    fn plus_on_the_results_starts_with_an_empty_name() {
        let mut app = results();
        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Local;
        s.profile.read_only = None;
        app.handle_key(ch('+'));
        let form = app.views.keys.form.as_ref().expect("the form");
        assert_eq!(form.fields[0].text, "");
    }

    #[test]
    fn plus_on_the_results_of_a_read_only_profile_sends_nothing() {
        let mut app = results();
        let effects = app.handle_key(ch('+'));
        assert!(app.views.keys.form.is_none());
        assert!(notice(&app).contains("read-only"), "{}", notice(&app));
        assert!(testing::requests(&effects).is_empty());
    }

    // -- What the results say ----------------------------------------------------

    #[test]
    fn no_match_says_so() {
        let mut app = on(Screen::Overview);
        let (id, _) = find_request(&find(&mut app, "nope"));
        testing::reply(&mut app, id, page(Vec::new(), 0));
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("No key matches *nope*."), "{all}");
        assert!(all.contains("ctrl-f searches again"), "{all}");
        // A first page that found nothing yet: m scans further.
        let mut app = on(Screen::Overview);
        let (id, _) = find_request(&find(&mut app, "nope"));
        testing::reply(&mut app, id, page(Vec::new(), 55));
        let all = text(&testing::render(&app, 120, 36));
        assert!(
            all.contains("No key matched yet. m scans further."),
            "{all}"
        );
        assert!(
            all.contains("No key matches *nope* in the part read so far. m scans further."),
            "{all}"
        );
        let (_, req) = find_request(&app.handle_key(ch('m')));
        assert!(matches!(req, Request::Find { cursor: 55, .. }));
    }

    #[test]
    fn one_match_is_counted_in_the_singular() {
        let mut app = on(Screen::Overview);
        let (id, _) = find_request(&find(&mut app, "banner"));
        testing::reply(&mut app, id, page(vec![carts().remove(2)], 0));
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("1 key matches."), "{all}");
    }

    #[test]
    fn a_refused_scan_says_keys_cant_be_searched_in_fixed_words() {
        let mut app = on(Screen::Overview);
        let (id, _) = find_request(&find(&mut app, "cart"));
        testing::reply(&mut app, id, Err(LiveError::Denied("SCAN".into())));
        let all = text(&testing::render(&app, 120, 36));
        assert!(
            all.contains("The server refused SCAN for this user, so keys can't be"),
            "{all}"
        );
        assert!(!all.contains("NOPERM"), "{all}");
        // Another failure, as the pattern's keys say it.
        let mut app = on(Screen::Overview);
        let (id, _) = find_request(&find(&mut app, "cart"));
        testing::reply(&mut app, id, Err(LiveError::Closed));
        let all = text(&testing::render(&app, 120, 36));
        assert!(all.contains("The live connection is closed."), "{all}");
    }

    #[test]
    fn control_characters_are_drawn_escaped() {
        let mut app = on(Screen::Overview);
        app.handle_key(ctrl('f'));
        // A bidi control is not a control character: it is kept, and drawn escaped.
        app.handle_paste("a\u{202e}b".into());
        assert_eq!(app.find.text, "a\u{202e}b");
        let foot = last_line(&app, 80, 24);
        assert!(foot.contains("a\\u{202e}b"), "{foot}");
        let (id, _) = find_request(&app.handle_key(key(KeyCode::Enter)));
        testing::reply(
            &mut app,
            id,
            page(vec![row("a\u{202e}b\u{1b}[2J", KeyType::String, 5, 1)], 0),
        );
        let all = text(&testing::render(&app, 120, 36));
        assert!(
            !all.contains('\u{202e}') && !all.contains('\u{1b}'),
            "{all}"
        );
        assert!(all.contains("Find: *a\\u{202e}b*"), "{all}");
        assert!(all.contains("a\\u{202e}b\\x1b[2J"), "{all}");
    }

    // -- Snapshots ---------------------------------------------------------------

    #[test]
    fn the_prompt_on_patterns_80x24_and_120x36() {
        let mut app = on(Screen::Patterns);
        app.handle_key(ctrl('f'));
        type_text(&mut app, "user:*:cart");
        insta::assert_snapshot!("find_prompt_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!(
            "find_prompt_120x36",
            testing::render(&app, 120, 36).backend()
        );
    }

    #[test]
    fn the_results_80x24_and_120x36() {
        let app = results();
        insta::assert_snapshot!(
            "find_results_80x24",
            testing::render(&app, 80, 24).backend()
        );
        insta::assert_snapshot!(
            "find_results_120x36",
            testing::render(&app, 120, 36).backend()
        );
    }

    #[test]
    fn no_match_80x24_and_120x36() {
        let mut app = on(Screen::Overview);
        let (id, _) = find_request(&find(&mut app, "nope"));
        testing::reply(&mut app, id, page(Vec::new(), 0));
        insta::assert_snapshot!(
            "find_no_match_80x24",
            testing::render(&app, 80, 24).backend()
        );
        insta::assert_snapshot!(
            "find_no_match_120x36",
            testing::render(&app, 120, 36).backend()
        );
    }
}
