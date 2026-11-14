//! The write gate's question (spec "Write permissions", check 3), and `w` on
//! Connections (check 2). `App` owns the one dialog: a view asks for a write with
//! [`super::views::Act::write`], and when the gate wants an answer the question opens
//! here, naming the profile and showing the exact command. Only the answer the gate
//! wants makes a [`Permit`]; any other key, `n` or `esc` cancels and the view hears
//! [`crate::live::CANCELLED`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::views::{Owner, WriteAsk};
use super::{App, ConnId, Effect, plain_char};
use crate::conn::WRITE_PROBES;
use crate::live::{CANCELLED, LiveError, LiveRequest};
use crate::model::{Env, PermStatus, PermissionReport};
use crate::write::{Answer, Decision, Permit, Refusals, WriteOp};

/// The most characters a typed name may have.
const MAX_TYPED: usize = 512;

/// What the question is about.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmFor {
    /// A view's write, waiting for its answer.
    Write {
        id: crate::live::ReqId,
        owner: Owner,
        op: WriteOp,
    },
    /// `w` on Connections: turning writes on for entry `id`.
    WritesOn { id: ConnId },
    /// `x` or `D` on Patterns, after the dry run (`super::bulk`): `summary` says what
    /// it found, as lines safe to draw.
    Bulk { op: WriteOp, summary: Vec<String> },
}

/// What answers the question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// `y` or `n`.
    YesNo,
    /// This exact text, then `enter`.
    Type(String),
}

/// The question on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub what: ConfirmFor,
    pub expect: Expect,
    /// What was typed so far, for [`Expect::Type`].
    pub typed: String,
    /// The profile it is about, and whether it is `prod`.
    pub profile: String,
    pub prod: bool,
}

impl Confirm {
    /// The dialog's title: what will happen.
    pub fn title(&self) -> String {
        match &self.what {
            ConfirmFor::Write { op, .. } | ConfirmFor::Bulk { op, .. } => op_title(op).to_string(),
            ConfirmFor::WritesOn { .. } => {
                format!("Turn on writes for {}", crate::value::escape(&self.profile))
            }
        }
    }

    /// The lines between the title and the keys, every one safe to draw.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        match &self.what {
            ConfirmFor::Write { op, .. } => {
                if let Some(what) = op_target(op) {
                    out.push(what);
                }
                if matches!(
                    op.kind(),
                    crate::write::Kind::Destructive | crate::write::Kind::Bulk
                ) {
                    out.push("This can't be undone.".to_string());
                }
                out.push(String::new());
                out.push(format!("Sends: {}", op.shown()));
            }
            ConfirmFor::Bulk { op, summary } => {
                if let Some(what) = op_target(op) {
                    out.push(what);
                }
                out.extend(summary.iter().cloned());
                out.push(match op {
                    WriteOp::Bulk {
                        action: crate::write::BulkAction::Expire { .. },
                        ..
                    } => "PERSIST on the keys in the key file undoes it.".to_string(),
                    _ => "This can't be undone.".to_string(),
                });
                out.push(String::new());
                out.push(format!("Sends: {}", op.shown()));
            }
            ConfirmFor::WritesOn { .. } => {
                if self.prod {
                    out.push("This is a PROD profile. With writes on, key edits,".to_string());
                } else {
                    out.push("With writes on, key edits,".to_string());
                }
                out.push("deletes and write commands reach the server.".to_string());
                out.push("Deletes, bulk actions and @dangerous commands still".to_string());
                out.push("ask each time. The setting is saved (read_only = false).".to_string());
            }
        }
        out.push(String::new());
        match &self.expect {
            Expect::YesNo => {}
            Expect::Type(name) => out.push(format!(
                "Type {} and press enter to {}:",
                crate::value::escape(name),
                match self.what {
                    ConfirmFor::Write { .. } => "send it",
                    ConfirmFor::Bulk { .. } => "start",
                    ConfirmFor::WritesOn { .. } => "turn writes on",
                }
            )),
        }
        out
    }

    /// The key bar while it is open: `(key, label)`.
    pub fn keys(&self) -> Vec<(&'static str, &'static str)> {
        let go = match &self.what {
            ConfirmFor::Write { op, .. } => op_verb(op),
            ConfirmFor::Bulk {
                op:
                    WriteOp::Bulk {
                        action: crate::write::BulkAction::Expire { .. },
                        ..
                    },
                ..
            } => "Set TTLs",
            ConfirmFor::Bulk { op, .. } => op_verb(op),
            ConfirmFor::WritesOn { .. } => "Turn on",
        };
        match self.expect {
            Expect::YesNo => vec![("y", go), ("n", "Cancel")],
            Expect::Type(_) => vec![("enter", go), ("esc", "Cancel")],
        }
    }
}

/// A short title for the dialog of `op`.
pub fn op_title(op: &WriteOp) -> &'static str {
    use WriteOp::*;
    match op {
        SetString { .. } | HSet { .. } | LSet { .. } | SReplace { .. } | JsonSet { .. } => {
            "Edit value"
        }
        ZScore { .. } => "Change score",
        HSetNx { .. } | RPush { .. } | SAdd { .. } | ZAdd { .. } | XAdd { .. } => "Add item",
        KeyTtl { .. } | FieldTtl { .. } => "Set TTL",
        Rename { .. } => "Rename key",
        HDel { .. } | SRem { .. } | ZRem { .. } | XDel { .. } | LDel { .. } => "Delete item",
        Unlink { .. } => "Delete key",
        NewKey { .. } => "Add key",
        ClientKill { .. } => "Kill connection",
        SlowlogConfig(_) => "Change slow log setting",
        SlowlogReset => "Reset the slow log",
        XAutoClaim { .. } => "Claim pending entries",
        XAck { .. } => "Acknowledge entry",
        XTrim { .. } => "Trim stream",
        Publish { .. } => "Publish message",
        SPublish { .. } => "Publish to a shard channel",
        FtCreate { .. } => "Create index",
        FtDropIndex { .. } => "Drop index",
        Bulk {
            action: crate::write::BulkAction::Delete,
            ..
        } => "Delete every key of the pattern",
        Bulk { .. } => "Set a TTL on every key of the pattern",
        Command { .. } => "Run command",
    }
}

/// The word on the go key.
fn op_verb(op: &WriteOp) -> &'static str {
    match op.kind() {
        crate::write::Kind::Destructive | crate::write::Kind::Bulk => "Delete",
        _ => "Send",
    }
}

/// What the op is done to, as one line: `Key user:1`.
fn op_target(op: &WriteOp) -> Option<String> {
    let name = op.name_to_type();
    if name.is_empty() {
        return None;
    }
    let what = match op {
        WriteOp::Publish { .. } | WriteOp::SPublish { .. } => "Channel",
        WriteOp::FtCreate { .. } | WriteOp::FtDropIndex { .. } => "Index",
        WriteOp::Bulk { .. } => "Pattern",
        WriteOp::ClientKill { .. } => "Client ID",
        WriteOp::Command { .. } => return None,
        _ => "Key",
    };
    Some(format!("{what} {}", crate::value::escape(&name)))
}

/// The write commands `ACL DRYRUN` said this user may not run (spec "Write
/// permissions", check 1).
pub fn refusals_of(perms: &PermissionReport) -> Refusals {
    let mut r = Refusals::default();
    for c in &perms.checks {
        if let PermStatus::Denied(_) = c.status
            && let Some((_, name, _)) = WRITE_PROBES.iter().find(|(l, _, _)| *l == c.label)
        {
            r.add(name);
        }
    }
    r
}

impl App {
    /// Opens the question for a view's write ([`super::views::Act::write`]).
    pub(super) fn ask_write(&mut self, ask: WriteAsk) -> Vec<Effect> {
        // A question is open already (another view's, a bulk run's): this one is not
        // asked, and its view hears so. Answers go only to the question shown.
        if self.confirm.is_some() {
            let why = "Not sent: answer the open question first.".to_string();
            self.set_error(why.clone());
            return self.write_failed(ask.id, ask.owner, why);
        }
        let Some(s) = self.session.as_ref() else {
            return Vec::new();
        };
        let expect = match &ask.decision {
            Decision::Type(name) => Expect::Type(name.clone()),
            _ => Expect::YesNo,
        };
        self.confirm = Some(Confirm {
            what: ConfirmFor::Write {
                id: ask.id,
                owner: ask.owner,
                op: ask.op,
            },
            expect,
            typed: String::new(),
            profile: s.profile.name.clone(),
            prod: s.profile.env == Env::Prod,
        });
        Vec::new()
    }

    /// Closes the question without an answer: the live connection went, or a new run
    /// replaced it. The request it held is forgotten with the others.
    pub(super) fn close_write_question(&mut self) {
        if matches!(
            self.confirm,
            Some(Confirm {
                what: ConfirmFor::Write { .. },
                ..
            })
        ) {
            self.confirm = None;
        }
    }

    /// A key while the question is open.
    pub(super) fn confirm_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(c) = self.confirm.as_mut() else {
            return Vec::new();
        };
        let answer = match &c.expect {
            Expect::YesNo => match (key.code, plain_char(&key)) {
                (_, Some('y' | 'Y')) => Some(Answer::Yes),
                (KeyCode::Esc, _) | (_, Some('n' | 'N')) => None,
                _ => return Vec::new(),
            },
            Expect::Type(_) => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => Some(Answer::Typed(c.typed.clone())),
                KeyCode::Backspace => {
                    c.typed.pop();
                    return Vec::new();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    c.typed.clear();
                    return Vec::new();
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && c.typed.chars().count() < MAX_TYPED =>
                {
                    c.typed.push(ch);
                    return Vec::new();
                }
                _ => return Vec::new(),
            },
        };
        let Some(c) = self.confirm.take() else {
            return Vec::new();
        };
        self.answer(c, answer)
    }

    /// Pasted text while the question is open: typed into the name, without newlines.
    pub(super) fn confirm_paste(&mut self, text: &str) {
        if let Some(c) = self.confirm.as_mut()
            && matches!(c.expect, Expect::Type(_))
        {
            for ch in text.chars().filter(|ch| !ch.is_control()) {
                if c.typed.chars().count() >= MAX_TYPED {
                    break;
                }
                c.typed.push(ch);
            }
        }
    }

    fn answer(&mut self, c: Confirm, answer: Option<Answer>) -> Vec<Effect> {
        match c.what {
            ConfirmFor::Write { id, owner, op } => {
                let Some(answer) = answer else {
                    self.set_info(CANCELLED);
                    return self.write_failed(id, owner, CANCELLED.to_string());
                };
                let Some(s) = self.session.as_ref() else {
                    return Vec::new();
                };
                if !self.live_pending.contains_key(&id) || s.live != super::LiveState::Ready {
                    return self.write_failed(
                        id,
                        owner,
                        "Not sent: the live connection is not open.".into(),
                    );
                }
                // Checked when it was asked; the node table is the same.
                let to = match super::views::write_dest(s, &op) {
                    Ok(to) => to,
                    Err(why) => {
                        self.set_error(why.clone());
                        return self.write_failed(id, owner, why);
                    }
                };
                match Permit::grant(op, &s.profile, &self.settings, &s.refusals, &answer) {
                    Ok(permit) => {
                        self.live_writes.insert(id, permit.op().command());
                        vec![Effect::Live {
                            id: s.id,
                            req_id: id,
                            to,
                            req: LiveRequest::Write(permit),
                        }]
                    }
                    Err(why) => {
                        self.set_error(why.clone());
                        self.write_failed(id, owner, why)
                    }
                }
            }
            ConfirmFor::Bulk { op, .. } => self.bulk_answer(op, answer),
            ConfirmFor::WritesOn { id } => match answer {
                None => {
                    self.set_info("Cancelled: writes stay off.");
                    Vec::new()
                }
                Some(Answer::Typed(t)) if Expect::Type(t.clone()) != c.expect => {
                    self.set_error("The name typed did not match: writes stay off.");
                    Vec::new()
                }
                Some(_) => self.set_writes(id, true),
            },
        }
    }

    /// Tells `owner` its write `id` was not sent, and why.
    fn write_failed(&mut self, id: crate::live::ReqId, owner: Owner, why: String) -> Vec<Effect> {
        if self.live_pending.remove(&id).is_none() {
            return Vec::new();
        }
        self.with_views(owner, "reply", |v, act| {
            v.get_mut(owner)
                .on_reply(id, Err(LiveError::Failed(why)), act)
        })
        .map_or_else(Vec::new, |(_, e)| e)
    }

    /// `w` on Connections: writes off at once; on after asking.
    pub(super) fn toggle_writes(&mut self) -> Vec<Effect> {
        let super::ConnSel::Entry(id) = self.conn_sel else {
            self.set_info("Select a connection first.");
            return Vec::new();
        };
        let Some(e) = self.entry(id) else {
            return Vec::new();
        };
        if !e.profile.is_read_only() {
            return self.set_writes(id, false);
        }
        let prod = e.profile.env == Env::Prod;
        let name = e.profile.name.clone();
        self.confirm = Some(Confirm {
            what: ConfirmFor::WritesOn { id },
            expect: if prod {
                Expect::Type(name.clone())
            } else {
                Expect::YesNo
            },
            typed: String::new(),
            profile: name,
            prod,
        });
        Vec::new()
    }

    /// Writes on or off for entry `id`. Off takes effect at once, before it is saved;
    /// on, once the config file has it. A Docker container has no saved profile, so it
    /// changes for this session only.
    pub(super) fn set_writes(&mut self, id: ConnId, on: bool) -> Vec<Effect> {
        let Some(e) = self.entry(id) else {
            return Vec::new();
        };
        let name = e.profile.name.clone();
        let docker = e.is_docker();
        if docker || !on {
            self.apply_writes(&name, docker.then_some(id), on);
        }
        if docker {
            self.set_info(format!(
                "Writes are {} for {} in this session only (a container has no saved profile).",
                if on { "on" } else { "off" },
                crate::value::escape(&name)
            ));
            return Vec::new();
        }
        if !on {
            self.set_info(format!(
                "Writes are off for {}.",
                crate::value::escape(&name)
            ));
        }
        vec![Effect::SetWrites { name, on }]
    }

    /// Sets the write state on the entries named `name` (or only entry `only`), and on
    /// the session when it shows one of them.
    fn apply_writes(&mut self, name: &str, only: Option<ConnId>, on: bool) {
        let mut ids = Vec::new();
        for e in &mut self.entries {
            let matches = match only {
                Some(id) => e.id == id,
                None => !e.is_docker() && e.profile.name == name,
            };
            if matches {
                e.profile.read_only = Some(!on);
                ids.push(e.id);
            }
        }
        if let Some(s) = self.session.as_mut()
            && ids.contains(&s.id)
        {
            s.profile.read_only = Some(!on);
        }
    }

    pub(super) fn writes_saved(&mut self, name: &str, on: bool) {
        self.apply_writes(name, None, on);
        if on {
            self.set_info(format!(
                "Writes are on for {} (saved to the config file).",
                crate::value::escape(name)
            ));
        }
    }

    pub(super) fn writes_failed(&mut self, name: &str, on: bool, error: &str) {
        self.set_error(format!(
            "Could not save writes {} for {}: {}",
            if on { "on" } else { "off" },
            crate::value::escape(name),
            crate::conn::parse::mask(error)
        ));
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::views::testing::{self, key};
    use super::super::views::{Owner, ViewState};
    use super::super::{App, ConnSel, Effect, Msg, Screen};
    use super::*;
    use crate::live::{LiveReply, ReqId};
    use crate::model::{Env, PermCheck};
    use crate::value::Raw;
    use crate::write::Settings;

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(ch(c));
        }
    }

    /// A scanned app with the live connection ready, on the key detail, its session's
    /// profile set to `env` with `read_only`.
    fn app_with(env: Env, read_only: Option<bool>) -> App {
        let (mut app, _) = testing::app_on(Screen::KeyDetail);
        let s = app.session.as_mut().unwrap();
        s.profile.env = env;
        s.profile.read_only = read_only;
        app
    }

    fn write(app: &mut App, op: WriteOp) -> (Option<ReqId>, Vec<Effect>) {
        app.with_views(Owner::Value, "key", |_, act| act.write(op))
            .unwrap()
    }

    fn writes(effects: &[Effect]) -> Vec<(ReqId, String)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Live {
                    req_id,
                    req: LiveRequest::Write(p),
                    ..
                } => Some((*req_id, p.op().command())),
                _ => None,
            })
            .collect()
    }

    fn hset() -> WriteOp {
        WriteOp::HSet {
            key: "user:10293:cart".into(),
            field: Raw::from("sku"),
            value: Raw::from("2"),
        }
    }

    fn unlink() -> WriteOp {
        WriteOp::Unlink {
            key: "user:10293:cart".into(),
        }
    }

    fn every_kind() -> Vec<WriteOp> {
        vec![
            hset(),
            unlink(),
            WriteOp::ClientKill { id: 3 },
            WriteOp::SPublish {
                channel: "orders".into(),
                message: Raw::from("hi"),
            },
            WriteOp::Bulk {
                pattern: "user:{id}:cart".into(),
                glob: "user:*:cart".into(),
                action: crate::write::BulkAction::Delete,
            },
        ]
    }

    #[test]
    fn nothing_reaches_the_connection_on_a_read_only_profile() {
        for (env, ro) in [
            (Env::Prod, None),
            (Env::Prod, Some(true)),
            (Env::Stage, Some(true)),
            (Env::Local, Some(true)),
        ] {
            for op in every_kind() {
                let mut app = app_with(env, ro);
                let (id, effects) = write(&mut app, op.clone());
                assert_eq!(id, None, "{op:?}");
                assert!(effects.is_empty(), "{op:?}: {effects:?}");
                assert!(app.confirm.is_none());
                let n = app.notice.as_ref().unwrap();
                assert!(n.error && n.text.contains("read-only"), "{}", n.text);
                // Every key afterwards, y and the profile's name included, still
                // sends nothing.
                for k in ['y', 'p', '\n'] {
                    let e = app.handle_key(ch(k));
                    assert!(writes(&e).is_empty());
                }
            }
        }
    }

    #[test]
    fn nothing_reaches_any_node_of_a_read_only_cluster() {
        for op in every_kind() {
            let mut app = app_with(Env::Prod, None);
            let s = app.session.as_mut().unwrap();
            s.server.as_mut().unwrap().mode = crate::model::ServerMode::Cluster;
            s.nodes = crate::tui::fixtures::live_nodes();
            for picked in 0..3 {
                app.session.as_mut().unwrap().picked = picked;
                let (id, effects) = write(&mut app, op.clone());
                assert_eq!(id, None, "{op:?}");
                assert!(effects.is_empty(), "{op:?}: {effects:?}");
                assert!(app.confirm.is_none());
                for k in ['y', 'p', '\n'] {
                    assert!(writes(&app.handle_key(ch(k))).is_empty());
                }
            }
        }
    }

    #[test]
    fn a_second_write_never_replaces_the_open_question() {
        let mut app = app_with(Env::Local, None);
        let (first, _) = write(&mut app, unlink());
        let other = WriteOp::Unlink {
            key: "other".into(),
        };
        let (_, effects) = write(&mut app, other);
        assert!(writes(&effects).is_empty());
        let n = app.notice.as_ref().unwrap();
        assert!(n.error && n.text.contains("open question"), "{}", n.text);
        let c = app.confirm.as_ref().expect("the first question");
        assert!(
            c.lines()
                .iter()
                .any(|l| l == "Sends: UNLINK user:10293:cart")
        );
        // y answers the first question, and only that one is sent.
        let effects = app.handle_key(ch('y'));
        assert_eq!(
            writes(&effects),
            vec![(first.unwrap(), "UNLINK".to_string())]
        );
    }

    #[test]
    fn an_edit_runs_and_a_delete_asks_outside_prod() {
        let mut app = app_with(Env::Local, None);
        let (id, effects) = write(&mut app, hset());
        assert_eq!(writes(&effects), vec![(id.unwrap(), "HSET".to_string())]);
        assert!(app.confirm.is_none());

        let (id, effects) = write(&mut app, unlink());
        let id = id.unwrap();
        assert!(effects.is_empty());
        let c = app.confirm.as_ref().expect("a question");
        assert_eq!(c.expect, Expect::YesNo);
        assert!(
            c.lines()
                .iter()
                .any(|l| l == "Sends: UNLINK user:10293:cart")
        );
        // Other keys do nothing; y sends with the id the view was given.
        assert!(app.handle_key(ch('x')).is_empty());
        let effects = app.handle_key(ch('y'));
        assert_eq!(writes(&effects), vec![(id, "UNLINK".to_string())]);
        assert!(app.confirm.is_none());
    }

    #[test]
    fn no_or_esc_cancels_and_the_view_hears_it() {
        for k in [ch('n'), key(KeyCode::Esc)] {
            let mut app = app_with(Env::Local, None);
            let (id, _) = write(&mut app, unlink());
            let calls = app.calls.len();
            let effects = app.handle_key(k);
            assert!(writes(&effects).is_empty());
            assert!(app.confirm.is_none());
            assert!(app.calls[calls..].contains(&(Owner::Value, "reply")));
            let id = id.unwrap();
            assert!(!app.live_pending.contains_key(&id));
        }
    }

    #[test]
    fn prod_asks_for_the_name_and_a_wrong_one_sends_nothing() {
        let mut app = app_with(Env::Prod, Some(false));
        let (_, e) = write(&mut app, hset());
        assert!(writes(&e).is_empty());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        assert_eq!(writes(&app.handle_key(ch('y'))).len(), 1);

        write(&mut app, unlink());
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("user:10293:cart".into())
        );
        // y is just a letter here.
        assert!(writes(&app.handle_key(ch('y'))).is_empty());
        app.handle_key(key(KeyCode::Backspace));
        type_text(&mut app, "user:10293:car");
        assert!(writes(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert!(app.confirm.is_none());

        write(&mut app, unlink());
        type_text(&mut app, "user:10293:cart");
        assert_eq!(writes(&app.handle_key(key(KeyCode::Enter))).len(), 1);

        // A dangerous op asks for the profile's name.
        write(&mut app, WriteOp::ClientKill { id: 3 });
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("prod-cache-01".into())
        );
        app.handle_paste("prod-cache-01".into());
        assert_eq!(writes(&app.handle_key(key(KeyCode::Enter))).len(), 1);
    }

    #[test]
    fn skip_non_prod_confirm_never_skips_prod_or_bulk() {
        let mut app = app_with(Env::Local, None);
        app.settings = Settings {
            skip_non_prod_confirm: true,
            ..Settings::default()
        };
        let (_, e) = write(&mut app, unlink());
        assert_eq!(writes(&e).len(), 1);
        let (_, e) = write(&mut app, every_kind().pop().unwrap());
        assert!(e.is_empty());
        assert!(app.confirm.is_some());
        app.handle_key(ch('n'));

        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Prod;
        s.profile.read_only = Some(false);
        let (_, e) = write(&mut app, unlink());
        assert!(e.is_empty());
        assert!(matches!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type(_)
        ));
    }

    #[test]
    fn turning_writes_off_while_asked_sends_nothing() {
        let mut app = app_with(Env::Local, None);
        write(&mut app, unlink());
        app.session.as_mut().unwrap().profile.read_only = Some(true);
        let e = app.handle_key(ch('y'));
        assert!(writes(&e).is_empty());
        assert!(app.notice.as_ref().unwrap().text.contains("read-only"));
    }

    #[test]
    fn a_noperm_answer_turns_the_action_off() {
        let mut app = app_with(Env::Local, None);
        let (id, _) = write(&mut app, hset());
        testing::reply(&mut app, id.unwrap(), Err(LiveError::Denied("HSET".into())));
        let (id, e) = write(&mut app, hset());
        assert_eq!(id, None);
        assert!(e.is_empty());
        assert_eq!(
            app.notice.as_ref().unwrap().text,
            "Not sent: the server refused HSET for this user."
        );
        // A read refused is not a write refused.
        let mut app = app_with(Env::Local, None);
        let id = app
            .with_views(Owner::Value, "key", |_, act| {
                act.send(LiveRequest::Command(crate::live::cmdline::Request::Names))
            })
            .unwrap()
            .0
            .unwrap();
        testing::reply(&mut app, id, Err(LiveError::Denied("HSET".into())));
        assert_eq!(writes(&write(&mut app, hset()).1).len(), 1);
    }

    #[test]
    fn acl_dryrun_denials_turn_actions_off_from_the_connect() {
        let mut perms = crate::tui::fixtures::perms_with_probes();
        perms.checks.push(PermCheck {
            label: crate::conn::PERM_ACL_DRYRUN,
            status: PermStatus::Allowed,
        });
        perms.checks.push(PermCheck {
            label: "write UNLINK",
            status: PermStatus::Denied("NOPERM UNLINK is not allowed for this user".into()),
        });
        perms.checks.push(PermCheck {
            label: "write HSET",
            status: PermStatus::Allowed,
        });
        let r = refusals_of(&perms);
        assert!(r.refused("UNLINK") && !r.refused("HSET"));
    }

    fn on_connections(app: &mut App, name: &str) -> ConnId {
        app.handle_key(ch('0'));
        let id = app
            .entries
            .iter()
            .find(|e| e.profile.name == name)
            .unwrap()
            .id;
        app.conn_sel = ConnSel::Entry(id);
        id
    }

    fn set_writes(effects: &[Effect]) -> Vec<(String, bool)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::SetWrites { name, on } => Some((name.clone(), *on)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn w_turns_writes_on_after_asking_and_off_at_once() {
        let mut app = testing::scanned();
        let id = on_connections(&mut app, "staging-cache");
        app.entry_mut(id).unwrap().profile.read_only = Some(true);
        assert!(app.handle_key(ch('w')).is_empty());
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
        assert!(app.handle_key(ch('n')).is_empty());
        app.handle_key(ch('w'));
        assert_eq!(
            set_writes(&app.handle_key(ch('y'))),
            vec![("staging-cache".to_string(), true)]
        );
        // Only once it is saved.
        assert!(app.entry(id).unwrap().profile.is_read_only());
        app.apply(Msg::WritesSaved {
            name: "staging-cache".into(),
            on: true,
        });
        assert!(!app.entry(id).unwrap().profile.is_read_only());
        // Off: at once, and saved.
        assert_eq!(
            set_writes(&app.handle_key(ch('w'))),
            vec![("staging-cache".to_string(), false)]
        );
        assert!(app.entry(id).unwrap().profile.is_read_only());
        assert!(app.confirm.is_none());
    }

    #[test]
    fn w_on_prod_needs_the_name_and_reaches_the_session() {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        let id = on_connections(&mut app, "prod-cache-01");
        assert_eq!(app.session.as_ref().unwrap().id, id);
        app.handle_key(ch('w'));
        assert_eq!(
            app.confirm.as_ref().unwrap().expect,
            Expect::Type("prod-cache-01".into())
        );
        assert!(set_writes(&app.handle_key(ch('y'))).is_empty());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.confirm.is_none());
        assert!(app.notice.as_ref().unwrap().text.contains("did not match"));
        app.handle_key(ch('w'));
        type_text(&mut app, "prod-cache-01");
        let e = app.handle_key(key(KeyCode::Enter));
        assert_eq!(set_writes(&e), vec![("prod-cache-01".to_string(), true)]);
        app.apply(Msg::WritesSaved {
            name: "prod-cache-01".into(),
            on: true,
        });
        assert!(!app.session.as_ref().unwrap().profile.is_read_only());
        let t = testing::render(&app, 100, 30);
        let text = format!("{:?}", t.backend());
        assert!(text.contains("WRITES ON"), "{text}");
    }

    #[test]
    fn w_on_a_container_changes_the_session_only() {
        let mut app = App::new(Vec::new());
        app.apply(Msg::DockerFound(vec![
            crate::tui::fixtures::docker_instance(),
        ]));
        let id = app.entries[0].id;
        app.conn_sel = ConnSel::Entry(id);
        app.entry_mut(id).unwrap().profile.read_only = Some(true);
        app.handle_key(ch('w'));
        let e = app.handle_key(ch('y'));
        assert!(set_writes(&e).is_empty());
        assert!(!app.entry(id).unwrap().profile.is_read_only());
    }

    #[test]
    fn the_question_names_the_profile_and_the_command() {
        let mut app = app_with(Env::Prod, Some(false));
        write(&mut app, unlink());
        let t = testing::render(&app, 80, 24);
        insta::assert_snapshot!("delete_key_prod_80x24", t.backend());
        let text = format!("{:?}", t.backend());
        assert!(text.contains("prod-cache-01 · PROD"), "{text}");
        assert!(text.contains("Sends   UNLINK user:10293:cart"), "{text}");
    }

    #[test]
    fn a_rescan_or_a_closed_connection_closes_the_question() {
        let mut app = app_with(Env::Local, None);
        write(&mut app, unlink());
        let (id, scan) = {
            let s = app.session.as_ref().unwrap();
            (s.id, s.scan)
        };
        app.apply(Msg::LiveClosed {
            id,
            scan,
            error: None,
        });
        assert!(app.confirm.is_none());
    }

    #[test]
    fn a_view_reply_arrives_for_a_sent_write() {
        let mut app = app_with(Env::Local, None);
        let (id, _) = write(&mut app, hset());
        let calls = app.calls.len();
        testing::reply(
            &mut app,
            id.unwrap(),
            Ok(LiveReply::Write(crate::live::write::Reply::Int(1))),
        );
        assert!(app.calls[calls..].contains(&(Owner::Value, "reply")));
        let _ = app.views.value.input();
    }
}
