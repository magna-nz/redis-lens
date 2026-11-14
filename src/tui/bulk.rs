//! `x` and `D` on Patterns: bulk expire and bulk delete by pattern (spec "Bulk
//! actions"). The flow is a dialog of its own, over Patterns, that takes every key while
//! it is open:
//!
//! 1. `x` asks for the TTL first (24 h to start with); `D` goes straight on.
//! 2. **The dry run**: [`crate::live::bulk::count`] steps over the live connection,
//!    counting the keys that infer to the pattern and those with a TTL. `esc` stops it.
//! 3. **The question**: the app's one confirm dialog ([`super::confirm`]), with the
//!    counts: `y`/`n`, or the pattern typed on `prod`. Only the [`Permit`] that
//!    [`Permit::grant`] gives for the answer starts the run.
//! 4. **The run**: one batch of [`crate::live::bulk::BATCH`] keys at a time, each sent
//!    once the previous one answered, with progress. `esc` stops after the batch on its
//!    way.
//! 5. **Done**: what was changed, the key file, and for an expire that `PERSIST` on those
//!    keys undoes it. Any key closes it.
//!
//! Before anything is sent, the action must be possible: a pattern (not a group) is
//! selected, the server is not a cluster and not a replica, the gate does not refuse
//! the op (a read-only profile, a command the server refused), the server has
//! `EXPIRE … NX` for `x`, and the live connection is open.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::confirm::{Confirm, ConfirmFor, Expect};
use super::{App, Effect, LiveState, plain_char};
use crate::conn::ScanTarget;
use crate::live::bulk::{self, Batch, Count, FileAt, Reply, Request, Walk};
use crate::live::{CANCELLED, LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::Env;
use crate::write::{Answer, BulkAction, Decision, Permit, WriteOp};

/// The most characters the TTL input takes.
const MAX_TTL_INPUT: usize = 32;

/// The open bulk action.
#[derive(Debug, Clone, PartialEq)]
pub struct Bulk {
    pub pattern: String,
    pub glob: String,
    /// `D`: delete; otherwise `x`: expire.
    pub delete: bool,
    /// The profile it runs on, and whether it is `prod`, for the dialog's title.
    pub profile: String,
    pub prod: bool,
    /// The name the key file is filed under: the report's (`docker:<name>` for a
    /// container), as snapshots are.
    files_as: String,
    pub stage: Stage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// `x`: the TTL being typed, and why the last one was not accepted.
    Ttl {
        input: String,
        error: Option<String>,
    },
    /// The dry run, waiting for step `req`.
    Counting {
        req: ReqId,
        action: BulkAction,
        count: Count,
    },
    /// Counted; the confirm dialog is open.
    Asking {
        action: BulkAction,
        count: Count,
    },
    Running(Run),
    Done(Done),
}

/// A run under way.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    req: ReqId,
    permit: Permit,
    pub action: BulkAction,
    /// The dry run's count of keys of the pattern.
    pub total: u64,
    /// Keys of the pattern handled so far.
    pub seen: u64,
    pub sent: u64,
    pub changed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub unreadable: u64,
    pub file: Option<PathBuf>,
    /// `esc` was pressed: no batch after the one on its way.
    pub stopping: bool,
}

/// How it ended, as lines safe to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub lines: Vec<String>,
    /// It stopped on an error.
    pub error: bool,
}

impl Bulk {
    /// The dialog's title.
    pub fn title(&self) -> &'static str {
        if self.delete {
            "Delete every key of the pattern"
        } else {
            "Set a TTL on every key of the pattern"
        }
    }

    /// The key bar while it is open: `(key, label)`.
    pub fn keys(&self) -> Vec<(&'static str, &'static str)> {
        match &self.stage {
            Stage::Ttl { .. } => vec![("enter", "Dry run"), ("esc", "Cancel"), ("ctrl-u", "Clear")],
            Stage::Counting { .. } => vec![("esc", "Stop the dry run")],
            Stage::Asking { .. } => Vec::new(),
            Stage::Running(r) if r.stopping => Vec::new(),
            Stage::Running(_) => vec![("esc", "Stop after this batch")],
            Stage::Done(_) => vec![("enter esc", "Close")],
        }
    }
}

/// What the TTL is called in the lines: `24h`.
fn ttl_of(action: &BulkAction) -> String {
    match action {
        BulkAction::Expire { seconds } => bulk::ttl_text(*seconds),
        BulkAction::Delete => String::new(),
    }
}

fn n(v: u64) -> String {
    crate::format::grouped(v)
}

fn keys_word(v: u64) -> &'static str {
    if v == 1 { "key" } else { "keys" }
}

/// The dry run's result, for the confirm dialog: lines safe to draw.
pub fn summary(count: &Count, action: &BulkAction) -> Vec<String> {
    let mut out = Vec::new();
    match action {
        BulkAction::Delete => out.push(format!(
            "Dry run: {} {} of the pattern will be deleted with UNLINK.",
            n(count.matched),
            keys_word(count.matched)
        )),
        BulkAction::Expire { .. } => {
            let change = count.to_change(action);
            out.push(format!(
                "Dry run: {} {} of the pattern, {} with a TTL already.",
                n(count.matched),
                keys_word(count.matched),
                n(count.with_ttl)
            ));
            out.push(format!(
                "{} {} without a TTL get {}; the others are left alone.",
                n(change),
                keys_word(change),
                ttl_of(action)
            ));
        }
    }
    if count.other_pattern > 0 {
        out.push(format!(
            "{} other {} matched the glob but belong to other patterns: left alone.",
            n(count.other_pattern),
            keys_word(count.other_pattern)
        ));
    }
    if count.unreadable > 0 {
        out.push(format!(
            "{} {} could not be read: left alone.",
            n(count.unreadable),
            keys_word(count.unreadable)
        ));
    }
    out
}

impl App {
    /// `x` (expire) or `D` (`delete`) on Patterns. Checks that the action can run, then
    /// opens the TTL input (`x`) or starts the dry run (`D`). Nothing is sent when it
    /// can't: a read-only profile is refused with its reason before any `SCAN`.
    pub(super) fn bulk_start(&mut self, delete: bool) -> Vec<Effect> {
        let Some(s) = self.session.as_ref() else {
            self.set_info("Nothing to change: connect first (0 Connections, then enter).");
            return Vec::new();
        };
        let Some(pattern) = self.selected_single_pattern() else {
            self.set_info("Select a pattern first: x and D work on one pattern, not a group.");
            return Vec::new();
        };
        let cluster = matches!(s.target, Some(ScanTarget::Cluster(_)))
            || s.server.as_ref().and_then(bulk::server_off).is_some()
            || self.report().is_some_and(|r| r.nodes.is_some());
        if cluster {
            self.set_error(bulk::CLUSTER_OFF);
            return Vec::new();
        }
        if matches!(s.target, Some(ScanTarget::Replica(_))) {
            self.set_error(bulk::REPLICA_OFF);
            return Vec::new();
        }
        let glob = crate::analysis::match_glob(&pattern);
        let action = if delete {
            BulkAction::Delete
        } else {
            BulkAction::Expire {
                seconds: bulk::DEFAULT_TTL_SECS,
            }
        };
        let op = WriteOp::Bulk {
            pattern: pattern.clone(),
            glob: glob.clone(),
            action,
        };
        if let Decision::Refuse(why) =
            crate::write::decide_op(&op, &s.profile, &self.settings, &s.refusals)
        {
            self.set_error(why);
            return Vec::new();
        }
        if !delete && let Some(why) = s.server.as_ref().and_then(bulk::expire_off) {
            self.set_error(why);
            return Vec::new();
        }
        if s.live != LiveState::Ready {
            self.set_error("Not available yet: the live connection is not open.");
            return Vec::new();
        }
        let files_as = self
            .report()
            .map_or_else(|| s.profile.name.clone(), |r| r.profile.clone());
        self.bulk = Some(Bulk {
            pattern,
            glob,
            delete,
            profile: s.profile.name.clone(),
            prod: s.profile.env == Env::Prod,
            files_as,
            stage: Stage::Ttl {
                input: bulk::ttl_text(bulk::DEFAULT_TTL_SECS),
                error: None,
            },
        });
        if delete {
            return self.bulk_count(BulkAction::Delete);
        }
        Vec::new()
    }

    /// Sends a bulk step on the live connection: its id and the effect. `None` when the
    /// live connection is not open.
    fn bulk_send(&mut self, req: Request) -> Option<(ReqId, Effect)> {
        let s = self.session.as_ref()?;
        if s.live != LiveState::Ready {
            return None;
        }
        let id = ReqId(self.next_req);
        self.next_req += 1;
        Some((
            id,
            Effect::Live {
                id: s.id,
                req_id: id,
                to: crate::live::Dest::node(s.picked),
                req: LiveRequest::Bulk(req),
            },
        ))
    }

    /// Starts the dry run of `action`.
    fn bulk_count(&mut self, action: BulkAction) -> Vec<Effect> {
        let Some(b) = self.bulk.as_ref() else {
            return Vec::new();
        };
        let req = Request::Count {
            pattern: b.pattern.clone(),
            glob: b.glob.clone(),
            walk: Walk::Start,
        };
        let Some((id, effect)) = self.bulk_send(req) else {
            return self.bulk_end_error("Not sent: the live connection is not open.".into());
        };
        if let Some(b) = self.bulk.as_mut() {
            b.stage = Stage::Counting {
                req: id,
                action,
                count: Count::default(),
            };
        }
        vec![effect]
    }

    /// Closes the dialog with an error line in the key bar.
    fn bulk_end_error(&mut self, why: String) -> Vec<Effect> {
        self.bulk = None;
        self.set_error(why);
        Vec::new()
    }

    /// A key while the dialog is open (the confirm dialog takes them first).
    pub(super) fn bulk_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(b) = self.bulk.as_mut() else {
            return Vec::new();
        };
        match &mut b.stage {
            Stage::Ttl { input, error } => match key.code {
                KeyCode::Esc => {
                    self.bulk = None;
                    self.set_info(CANCELLED);
                }
                KeyCode::Enter => match bulk::parse_ttl(input) {
                    Ok(seconds) => return self.bulk_count(BulkAction::Expire { seconds }),
                    Err(e) => *error = Some(e),
                },
                KeyCode::Backspace => {
                    input.pop();
                    *error = None;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.clear();
                    *error = None;
                }
                _ => {
                    if let Some(c) = plain_char(&key)
                        && !c.is_control()
                        && input.chars().count() < MAX_TTL_INPUT
                    {
                        input.push(c);
                        *error = None;
                    }
                }
            },
            Stage::Counting { .. } => {
                if key.code == KeyCode::Esc {
                    // Its answer is dropped: no request is waited for any more.
                    self.bulk = None;
                    self.set_info("Dry run stopped. Nothing was sent.");
                }
            }
            // The question is the app's dialog, which gets keys first; without it (closed
            // elsewhere) esc still ends the run, never stuck.
            Stage::Asking { .. } => {
                if key.code == KeyCode::Esc && self.confirm.is_none() {
                    self.bulk = None;
                    self.set_info("Bulk action cancelled. Nothing was sent.");
                }
            }
            Stage::Running(r) => {
                if key.code == KeyCode::Esc {
                    r.stopping = true;
                }
            }
            Stage::Done(_) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.bulk = None;
                }
            }
        }
        Vec::new()
    }

    /// Pasted text while the dialog is open: typed into the TTL, without control
    /// characters.
    pub(super) fn bulk_paste(&mut self, text: &str) {
        if let Some(Bulk {
            stage: Stage::Ttl { input, error },
            ..
        }) = self.bulk.as_mut()
        {
            for c in text.chars().filter(|c| !c.is_control()) {
                if input.chars().count() >= MAX_TTL_INPUT {
                    break;
                }
                input.push(c);
            }
            *error = None;
        }
    }

    /// True when `id` is the bulk step the dialog waits for.
    pub(super) fn bulk_waits_for(&self, id: ReqId) -> bool {
        match self.bulk.as_ref().map(|b| &b.stage) {
            Some(Stage::Counting { req, .. }) => *req == id,
            Some(Stage::Running(r)) => r.req == id,
            _ => false,
        }
    }

    /// The answer to the bulk step `id` ([`App::bulk_waits_for`]).
    pub(super) fn bulk_reply(
        &mut self,
        id: ReqId,
        result: Result<LiveReply, LiveError>,
    ) -> Vec<Effect> {
        if !self.bulk_waits_for(id) {
            return Vec::new();
        }
        let Some(b) = self.bulk.as_mut() else {
            return Vec::new();
        };
        match (&mut b.stage, result) {
            (Stage::Counting { action, count, .. }, Ok(LiveReply::Bulk(Reply::Count(c)))) => {
                count.add(&c);
                let (action, count) = (*action, *count);
                if count.finished() {
                    return self.bulk_counted(action, count);
                }
                let req = Request::Count {
                    pattern: b.pattern.clone(),
                    glob: b.glob.clone(),
                    walk: c.walk.unwrap_or(Walk::Done),
                };
                match self.bulk_send(req) {
                    Some((id, effect)) => {
                        if let Some(Bulk {
                            stage: Stage::Counting { req, .. },
                            ..
                        }) = self.bulk.as_mut()
                        {
                            *req = id;
                        }
                        vec![effect]
                    }
                    None => {
                        self.bulk_end_error("Not sent: the live connection is not open.".into())
                    }
                }
            }
            (Stage::Counting { .. }, other) => {
                let why = match other {
                    Err(e) => e.to_string(),
                    Ok(_) => "the answer could not be read".to_string(),
                };
                self.bulk_end_error(format!("The dry run failed: {why}. Nothing was sent."))
            }
            (Stage::Running(_), Ok(LiveReply::Bulk(Reply::Batch(batch)))) => {
                self.bulk_batch_done(batch)
            }
            (Stage::Running(_), other) => {
                let why = match other {
                    Err(e) => {
                        if let LiveError::Denied(cmd) = &e
                            && let Some(s) = self.session.as_mut()
                        {
                            s.refusals.add(cmd);
                        }
                        e.to_string()
                    }
                    Ok(_) => "the answer could not be read".to_string(),
                };
                self.bulk_finish(Some(why));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The dry run is over: nothing to do, or the question.
    fn bulk_counted(&mut self, action: BulkAction, count: Count) -> Vec<Effect> {
        let Some(b) = self.bulk.as_mut() else {
            return Vec::new();
        };
        let Some(s) = self.session.as_ref() else {
            return Vec::new();
        };
        if count.to_change(&action) == 0 {
            let pattern = crate::value::escape(&b.pattern);
            let text = if count.matched == 0 {
                format!("No keys of {pattern} were found. Nothing was sent.")
            } else {
                format!(
                    "Every key of {pattern} ({}) has a TTL already. Nothing was sent.",
                    n(count.matched)
                )
            };
            b.stage = Stage::Done(Done {
                lines: vec![text.clone()],
                error: false,
            });
            self.set_info(text);
            return Vec::new();
        }
        // Another question is open: never replace it.
        if self.confirm.is_some() {
            return self.bulk_end_error(
                "Not started: answer the open question first, then press the key again."
                    .to_string(),
            );
        }
        let op = WriteOp::Bulk {
            pattern: b.pattern.clone(),
            glob: b.glob.clone(),
            action,
        };
        let expect = match crate::write::decide_op(&op, &s.profile, &self.settings, &s.refusals) {
            Decision::Refuse(why) => return self.bulk_end_error(why),
            // Bulk actions always ask (`skip_non_prod_confirm` never applies to them);
            // were that to change, the gate still decides.
            Decision::Run => return self.bulk_granted(op, &Answer::None),
            Decision::Ask => Expect::YesNo,
            Decision::Type(name) => Expect::Type(name),
        };
        b.stage = Stage::Asking { action, count };
        self.confirm = Some(Confirm {
            what: ConfirmFor::Bulk {
                summary: summary(&count, &action),
                op,
            },
            expect,
            typed: String::new(),
            profile: b.profile.clone(),
            prod: b.prod,
        });
        Vec::new()
    }

    /// The confirm dialog's answer about bulk `op`: `None` for `n` or `esc`.
    pub(super) fn bulk_answer(&mut self, op: WriteOp, answer: Option<Answer>) -> Vec<Effect> {
        if !matches!(
            self.bulk.as_ref().map(|b| &b.stage),
            Some(Stage::Asking { .. })
        ) {
            return Vec::new();
        }
        let Some(answer) = answer else {
            self.bulk = None;
            self.set_info(CANCELLED);
            return Vec::new();
        };
        self.bulk_granted(op, &answer)
    }

    /// Asks the gate for a permit for `op` with `answer`, and starts the run with it.
    fn bulk_granted(&mut self, op: WriteOp, answer: &Answer) -> Vec<Effect> {
        let Some(s) = self.session.as_ref() else {
            return Vec::new();
        };
        if s.live != LiveState::Ready {
            return self.bulk_end_error("Not sent: the live connection is not open.".into());
        }
        let permit = match Permit::grant(op, &s.profile, &self.settings, &s.refusals, answer) {
            Ok(p) => p,
            Err(why) => return self.bulk_end_error(why),
        };
        let WriteOp::Bulk { action, .. } = permit.op() else {
            return Vec::new();
        };
        let action = *action;
        let Some(b) = self.bulk.as_ref() else {
            return Vec::new();
        };
        let total = match &b.stage {
            Stage::Asking { count, .. } => count.matched,
            _ => 0,
        };
        let req = Request::Batch {
            permit: permit.clone(),
            walk: Walk::Start,
            carry: Vec::new(),
            file: FileAt::New {
                profile: b.files_as.clone(),
            },
        };
        let Some((id, effect)) = self.bulk_send(req) else {
            return self.bulk_end_error("Not sent: the live connection is not open.".into());
        };
        if let Some(b) = self.bulk.as_mut() {
            b.stage = Stage::Running(Run {
                req: id,
                permit,
                action,
                total,
                seen: 0,
                sent: 0,
                changed: 0,
                skipped: 0,
                failed: 0,
                unreadable: 0,
                file: None,
                stopping: false,
            });
        }
        vec![effect]
    }

    /// A batch answered: the next one, or the end.
    fn bulk_batch_done(&mut self, batch: Batch) -> Vec<Effect> {
        let Some(Bulk {
            stage: Stage::Running(r),
            files_as,
            ..
        }) = self.bulk.as_mut()
        else {
            return Vec::new();
        };
        r.seen += batch.seen;
        r.sent += batch.sent;
        r.changed += batch.changed;
        r.skipped += batch.skipped;
        r.failed += batch.failed;
        r.unreadable += batch.unreadable;
        if batch.file.is_some() {
            r.file = batch.file.clone();
        }
        if batch.failed > 0 {
            let command = r.permit.op().command();
            let why = if batch.denied {
                if batch.changed == 0
                    && let Some(s) = self.session.as_mut()
                {
                    s.refusals.add(&command);
                }
                format!("the server refused {command} for this user")
            } else {
                format!(
                    "the server answered {command} with an error for {} {}",
                    n(batch.failed),
                    keys_word(batch.failed)
                )
            };
            self.bulk_finish(Some(why));
            return Vec::new();
        }
        if r.stopping || batch.finished() {
            self.bulk_finish(None);
            return Vec::new();
        }
        // Writes turned off with `w` since the run started: no further batch.
        if let Some(s) = self.session.as_ref()
            && s.profile.is_read_only()
        {
            let why = format!("{} is now read-only", s.profile.name);
            self.bulk_finish(Some(why));
            return Vec::new();
        }
        let req = Request::Batch {
            permit: r.permit.clone(),
            walk: batch.walk,
            carry: batch.carry,
            file: r.file.clone().map_or_else(
                || FileAt::New {
                    profile: files_as.clone(),
                },
                FileAt::Path,
            ),
        };
        match self.bulk_send(req) {
            Some((id, effect)) => {
                if let Some(Bulk {
                    stage: Stage::Running(r),
                    ..
                }) = self.bulk.as_mut()
                {
                    r.req = id;
                }
                vec![effect]
            }
            None => {
                self.bulk_finish(Some("the live connection is not open".into()));
                Vec::new()
            }
        }
    }

    /// The run is over: finished (`why` is `None`), stopped with `esc`, or stopped on
    /// `why`. The dialog says what was done; the key bar says it too.
    fn bulk_finish(&mut self, why: Option<String>) {
        let Some(b) = self.bulk.as_mut() else {
            return;
        };
        let Stage::Running(r) = &b.stage else {
            return;
        };
        let pattern = crate::value::escape(&b.pattern);
        let did = match r.action {
            BulkAction::Delete => format!(
                "deleted {} {} of {pattern}",
                n(r.changed),
                keys_word(r.changed)
            ),
            BulkAction::Expire { .. } => format!(
                "set a TTL of {} on {} {} of {pattern}",
                ttl_of(&r.action),
                n(r.changed),
                keys_word(r.changed)
            ),
        };
        let mut lines = Vec::new();
        let head = match (&why, r.stopping) {
            (Some(why), _) => format!("Stopped: {why}. Before that, {did}."),
            (None, true) => format!("Stopped with esc after the batch: {did}."),
            (None, false) => format!("Done: {did}."),
        };
        lines.push(head.clone());
        if r.skipped > 0 {
            lines.push(match r.action {
                BulkAction::Delete => format!(
                    "{} {} were gone already.",
                    n(r.skipped),
                    keys_word(r.skipped)
                ),
                BulkAction::Expire { .. } => format!(
                    "{} {} left alone: a TTL already, or gone.",
                    n(r.skipped),
                    keys_word(r.skipped)
                ),
            });
        }
        if r.unreadable > 0 {
            lines.push(format!(
                "{} {} could not be read: left alone.",
                n(r.unreadable),
                keys_word(r.unreadable)
            ));
        }
        if r.failed > 0 {
            lines.push(format!("{} {} failed.", n(r.failed), keys_word(r.failed)));
        }
        if let Some(f) = &r.file {
            lines.push(format!(
                "Key names: {}",
                crate::value::escape(&f.display().to_string())
            ));
            if matches!(r.action, BulkAction::Expire { .. }) {
                lines.push("PERSIST on those keys undoes it.".to_string());
            }
        }
        let error = why.is_some();
        b.stage = Stage::Done(Done { lines, error });
        if error {
            self.set_error(head);
        } else {
            self.set_info(head);
        }
    }

    /// The live connection closed: a run stops and says so; a dry run or a question
    /// closes.
    pub(super) fn bulk_closed(&mut self) {
        if matches!(
            self.confirm,
            Some(Confirm {
                what: ConfirmFor::Bulk { .. },
                ..
            })
        ) {
            self.confirm = None;
        }
        match self.bulk.as_ref().map(|b| &b.stage) {
            Some(Stage::Running(_)) => {
                self.bulk_finish(Some("the live connection closed".into()));
            }
            Some(Stage::Done(_)) | None => {}
            Some(_) => self.bulk = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::views::testing::{self, key};
    use super::super::{App, Effect, Msg, Screen};
    use super::*;
    use crate::model::{PatternStats, ServerMode};
    use crate::tui::tree::Row;

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, s: &str) -> Vec<Effect> {
        let mut out = Vec::new();
        for c in s.chars() {
            out.extend(app.handle_key(ch(c)));
        }
        out
    }

    /// A scanned app on Patterns, live connection ready, the session's profile set to
    /// `env` with `read_only`, `user:{id}:cart` selected.
    fn app_with(env: Env, read_only: Option<bool>) -> App {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        testing::show(&mut app, Screen::Patterns);
        let s = app.session.as_mut().unwrap();
        s.profile.env = env;
        s.profile.read_only = read_only;
        select(&mut app, "user:{id}:cart");
        app
    }

    /// Selects `pattern`'s row (groups start expanded).
    fn select(app: &mut App, pattern: &str) {
        let patterns: Vec<PatternStats> = app.report().unwrap().patterns.clone();
        let rows = app.pattern_rows();
        app.patterns.sel = rows
            .iter()
            .position(|r| match r {
                Row::Pattern { idx, .. } => patterns[*idx].pattern == pattern,
                Row::Group { root } => matches!(
                    app.patterns.tree.roots.get(*root),
                    Some(crate::tui::tree::Root::Single(i)) if patterns[*i].pattern == pattern
                ),
            })
            .expect("the pattern's row");
        assert_eq!(app.selected_single_pattern().as_deref(), Some(pattern));
    }

    /// The bulk steps among `effects`.
    fn steps(effects: &[Effect]) -> Vec<(ReqId, Request)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Live {
                    req_id,
                    req: LiveRequest::Bulk(r),
                    ..
                } => Some((*req_id, r.clone())),
                _ => None,
            })
            .collect()
    }

    fn writes(effects: &[Effect]) -> usize {
        effects
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Effect::Live {
                        req: LiveRequest::Write(_) | LiveRequest::Bulk(Request::Batch { .. }),
                        ..
                    }
                )
            })
            .count()
    }

    fn counted(walk: Walk, matched: u64, with_ttl: u64) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Bulk(Reply::Count(Count {
            walk: Some(walk),
            matched,
            with_ttl,
            other_pattern: 2,
            unreadable: 0,
        })))
    }

    fn batch(walk: Walk, sent: u64, carry: Vec<String>) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Bulk(Reply::Batch(Batch {
            walk,
            carry,
            seen: sent,
            sent,
            changed: sent,
            skipped: 0,
            failed: 0,
            denied: false,
            unreadable: 0,
            file: Some(PathBuf::from(
                "/data/bulk/p-0123/20250101T000000Z-expire.txt",
            )),
        })))
    }

    /// `x` then enter on the TTL, and the dry run answered in two steps: 1,200 keys,
    /// 200 with a TTL. Returns the effects of the last step.
    fn dry_run_expire(app: &mut App) -> Vec<Effect> {
        assert!(app.handle_key(ch('x')).is_empty());
        let e = app.handle_key(key(KeyCode::Enter));
        let s = steps(&e);
        assert_eq!(s.len(), 1);
        let Request::Count {
            pattern,
            glob,
            walk,
        } = &s[0].1
        else {
            panic!("a count step");
        };
        assert_eq!(
            (pattern.as_str(), glob.as_str(), *walk),
            ("user:{id}:cart", "user:*:cart", Walk::Start)
        );
        let e = testing::reply(app, s[0].0, counted(Walk::At(77), 700, 100));
        let s = steps(&e);
        assert!(matches!(
            s[0].1,
            Request::Count {
                walk: Walk::At(77),
                ..
            }
        ));
        testing::reply(app, s[0].0, counted(Walk::Done, 500, 100))
    }

    #[test]
    fn x_and_d_on_a_read_only_profile_send_nothing_whatever_follows() {
        for (env, ro) in [
            (Env::Prod, None),
            (Env::Prod, Some(true)),
            (Env::Stage, Some(true)),
            (Env::Local, Some(true)),
        ] {
            for k in ['x', 'D'] {
                let mut app = app_with(env, ro);
                let e = app.handle_key(ch(k));
                assert!(e.is_empty(), "{e:?}");
                assert!(app.bulk.is_none() && app.confirm.is_none());
                let n = app.notice.as_ref().unwrap();
                assert!(n.error && n.text.contains("read-only"), "{}", n.text);
                let mut all = Vec::new();
                for c in ['y', '\n', 'x', 'D', 'u'] {
                    all.extend(app.handle_key(ch(c)));
                }
                all.extend(type_text(&mut app, "user:{id}:cart"));
                all.extend(app.handle_key(key(KeyCode::Enter)));
                assert!(steps(&all).is_empty(), "{all:?}");
                assert_eq!(writes(&all), 0);
            }
        }
    }

    #[test]
    fn expire_dry_run_then_yes_starts_the_run() {
        let mut app = app_with(Env::Local, None);
        let e = dry_run_expire(&mut app);
        assert!(e.is_empty());
        let c = app.confirm.as_ref().expect("the question");
        assert_eq!(c.expect, Expect::YesNo);
        let lines = c.lines();
        assert!(
            lines
                .iter()
                .any(|l| l == "Dry run: 1,200 keys of the pattern, 200 with a TTL already."),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("1,000 keys without a TTL get 24h"))
        );
        assert!(
            lines.iter().any(
                |l| l.contains("EXPIRE <key> 86400 NX") && l.contains("SCAN MATCH user:*:cart")
            )
        );
        // Other keys do nothing; y starts the first batch with the permit.
        assert!(app.handle_key(ch('x')).is_empty());
        let e = app.handle_key(ch('y'));
        let s = steps(&e);
        assert_eq!(s.len(), 1);
        let Request::Batch {
            permit,
            walk,
            carry,
            file,
        } = &s[0].1
        else {
            panic!("a batch");
        };
        assert_eq!(
            permit.op(),
            &WriteOp::Bulk {
                pattern: "user:{id}:cart".into(),
                glob: "user:*:cart".into(),
                action: BulkAction::Expire { seconds: 86_400 },
            }
        );
        assert_eq!((*walk, carry.len()), (Walk::Start, 0));
        assert_eq!(
            file,
            &FileAt::New {
                profile: "prod-cache-01".into()
            }
        );
        // The next batch goes on from the walk and the carry, into the same file.
        let e = testing::reply(&mut app, s[0].0, batch(Walk::At(9), 500, vec!["k".into()]));
        let s = steps(&e);
        let Request::Batch {
            walk, carry, file, ..
        } = &s[0].1
        else {
            panic!("a batch");
        };
        assert_eq!((*walk, carry.clone()), (Walk::At(9), vec!["k".to_string()]));
        assert!(matches!(file, FileAt::Path(_)));
        let e = testing::reply(&mut app, s[0].0, batch(Walk::Done, 500, Vec::new()));
        assert!(steps(&e).is_empty());
        let Some(Bulk {
            stage: Stage::Done(d),
            ..
        }) = &app.bulk
        else {
            panic!("done");
        };
        assert_eq!(
            d.lines[0],
            "Done: set a TTL of 24h on 1,000 keys of user:{id}:cart."
        );
        assert!(
            d.lines
                .iter()
                .any(|l| l == "PERSIST on those keys undoes it.")
        );
        assert!(
            d.lines
                .iter()
                .any(|l| l.starts_with("Key names: /data/bulk/"))
        );
        app.handle_key(key(KeyCode::Esc));
        assert!(app.bulk.is_none());
    }

    #[test]
    fn the_ttl_prompt_changes_the_ttl_and_says_what_is_wrong() {
        let mut app = app_with(Env::Local, None);
        app.handle_key(ch('x'));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut app, "soon");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        let Some(Bulk {
            stage: Stage::Ttl { error: Some(e), .. },
            ..
        }) = &app.bulk
        else {
            panic!("still the ttl");
        };
        assert!(e.starts_with("not a duration"), "{e}");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut app, "7d");
        let e = app.handle_key(key(KeyCode::Enter));
        assert_eq!(steps(&e).len(), 1);
        assert!(matches!(
            app.bulk.as_ref().unwrap().stage,
            Stage::Counting {
                action: BulkAction::Expire { seconds: 604_800 },
                ..
            }
        ));
    }

    #[test]
    fn a_ttl_can_be_pasted() {
        let mut app = app_with(Env::Local, None);
        app.handle_key(ch('x'));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.handle_paste("10m\n".into());
        let e = app.handle_key(key(KeyCode::Enter));
        assert_eq!(steps(&e).len(), 1);
        assert!(matches!(
            app.bulk.as_ref().unwrap().stage,
            Stage::Counting {
                action: BulkAction::Expire { seconds: 600 },
                ..
            }
        ));
    }

    #[test]
    fn no_is_cancelled_and_sends_nothing() {
        let mut app = app_with(Env::Stage, None);
        dry_run_expire(&mut app);
        let e = app.handle_key(ch('n'));
        assert!(e.is_empty());
        assert!(app.bulk.is_none() && app.confirm.is_none());
        assert_eq!(app.notice.as_ref().unwrap().text, CANCELLED);
    }

    #[test]
    fn esc_ends_a_run_whose_question_was_closed_elsewhere() {
        let mut app = app_with(Env::Local, None);
        dry_run_expire(&mut app);
        assert!(app.confirm.take().is_some());
        let e = app.handle_key(key(KeyCode::Esc));
        assert!(e.is_empty());
        assert!(app.bulk.is_none());
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("Nothing was sent")
        );
    }

    #[test]
    fn the_run_never_asks_over_an_open_question() {
        // Another question, opened while the dry run counted.
        let mut other = app_with(Env::Local, None);
        dry_run_expire(&mut other);
        let open = other.confirm.take().unwrap();

        let mut app = app_with(Env::Local, None);
        app.handle_key(ch('x'));
        let e = app.handle_key(key(KeyCode::Enter));
        let s = steps(&e);
        app.confirm = Some(open.clone());
        let e = testing::reply(&mut app, s[0].0, counted(Walk::Done, 500, 100));
        assert!(steps(&e).is_empty() && writes(&e) == 0, "{e:?}");
        assert!(app.bulk.is_none());
        let n = app.notice.as_ref().unwrap();
        assert!(n.error && n.text.contains("open question"), "{}", n.text);
        // The open question is untouched: still the other run's.
        assert_eq!(app.confirm.as_ref(), Some(&open));
    }

    #[test]
    fn prod_needs_the_pattern_typed_and_y_is_not_enough() {
        let mut app = app_with(Env::Prod, Some(false));
        app.handle_key(ch('D'));
        let Some(Bulk {
            stage: Stage::Counting { req, .. },
            ..
        }) = app.bulk.clone()
        else {
            panic!("counting");
        };
        testing::reply(&mut app, req, counted(Walk::Done, 40, 3));
        let c = app.confirm.as_ref().unwrap();
        assert_eq!(c.expect, Expect::Type("user:{id}:cart".into()));
        assert!(c.lines().iter().any(|l| l == "This can't be undone."));
        // y is text here; a wrong name sends nothing.
        let mut all = type_text(&mut app, "y");
        all.extend(app.handle_key(key(KeyCode::Enter)));
        assert_eq!(writes(&all), 0);
        assert!(app.bulk.is_none());
        assert!(app.notice.as_ref().unwrap().text.contains("did not match"));

        // Again, with the pattern typed.
        app.handle_key(ch('D'));
        let Some(Bulk {
            stage: Stage::Counting { req, .. },
            ..
        }) = app.bulk.clone()
        else {
            panic!("counting");
        };
        testing::reply(&mut app, req, counted(Walk::Done, 40, 3));
        type_text(&mut app, "user:{id}:cart");
        let e = app.handle_key(key(KeyCode::Enter));
        assert_eq!(writes(&e), 1);
    }

    #[test]
    fn skip_non_prod_confirm_never_skips_the_bulk_question() {
        let mut app = app_with(Env::Local, None);
        app.settings.skip_non_prod_confirm = true;
        let e = dry_run_expire(&mut app);
        assert_eq!(writes(&e), 0);
        assert_eq!(app.confirm.as_ref().unwrap().expect, Expect::YesNo);
    }

    #[test]
    fn esc_during_the_run_stops_after_the_batch() {
        let mut app = app_with(Env::Local, None);
        dry_run_expire(&mut app);
        let s = steps(&app.handle_key(ch('y')));
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        let e = testing::reply(&mut app, s[0].0, batch(Walk::At(3), 500, Vec::new()));
        assert!(steps(&e).is_empty(), "no batch after esc");
        let Some(Bulk {
            stage: Stage::Done(d),
            ..
        }) = &app.bulk
        else {
            panic!("done");
        };
        assert!(d.lines[0].starts_with("Stopped with esc after the batch"));
    }

    #[test]
    fn writes_turned_off_during_the_run_stop_it_before_the_next_batch() {
        let mut app = app_with(Env::Local, None);
        dry_run_expire(&mut app);
        let s = steps(&app.handle_key(ch('y')));
        app.session.as_mut().unwrap().profile.read_only = Some(true);
        let e = testing::reply(&mut app, s[0].0, batch(Walk::At(3), 500, Vec::new()));
        assert!(steps(&e).is_empty(), "no batch once read-only");
        let Some(Bulk {
            stage: Stage::Done(d),
            ..
        }) = &app.bulk
        else {
            panic!("done");
        };
        assert!(d.lines[0].contains("is now read-only"), "{:?}", d.lines);
    }

    #[test]
    fn esc_during_the_dry_run_closes_it_and_its_answer_is_dropped() {
        let mut app = app_with(Env::Local, None);
        let e = app.handle_key(ch('D'));
        let s = steps(&e);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.bulk.is_none());
        let e = testing::reply(&mut app, s[0].0, counted(Walk::Done, 4, 0));
        assert!(e.is_empty() && app.confirm.is_none());
    }

    #[test]
    fn a_refused_batch_stops_the_run_and_turns_the_action_off() {
        let mut app = app_with(Env::Local, None);
        let e = app.handle_key(ch('D'));
        let s = steps(&e);
        testing::reply(&mut app, s[0].0, counted(Walk::Done, 4, 0));
        let s = steps(&app.handle_key(ch('y')));
        let e = testing::reply(&mut app, s[0].0, Err(LiveError::Denied("UNLINK".into())));
        assert!(steps(&e).is_empty());
        assert!(app.notice.as_ref().unwrap().error);
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ch('D'));
        assert!(app.bulk.is_none());
        assert!(app.notice.as_ref().unwrap().text.contains("refused UNLINK"));
    }

    #[test]
    fn nothing_to_change_says_so_without_asking() {
        let mut app = app_with(Env::Local, None);
        app.handle_key(ch('x'));
        let s = steps(&app.handle_key(key(KeyCode::Enter)));
        testing::reply(&mut app, s[0].0, counted(Walk::Done, 5, 5));
        assert!(app.confirm.is_none());
        assert!(matches!(app.bulk.as_ref().unwrap().stage, Stage::Done(_)));
    }

    #[test]
    fn expire_is_off_before_redis_7() {
        let mut app = app_with(Env::Local, None);
        app.session
            .as_mut()
            .unwrap()
            .server
            .as_mut()
            .unwrap()
            .version = "6.2.14".into();
        assert!(app.handle_key(ch('x')).is_empty());
        assert!(app.bulk.is_none());
        let n = app.notice.as_ref().unwrap();
        assert!(
            n.text.contains("Redis 6.2.14") && n.text.contains("7.0"),
            "{}",
            n.text
        );
        // Delete still works there.
        assert_eq!(steps(&app.handle_key(ch('D'))).len(), 1);
        // Valkey has EXPIRE NX in every version.
        let mut app = app_with(Env::Local, None);
        let server = app.session.as_mut().unwrap().server.as_mut().unwrap();
        server.flavor = crate::model::ServerFlavor::Valkey;
        server.version = "7.2.5".into();
        app.handle_key(ch('x'));
        assert!(app.bulk.is_some());
    }

    #[test]
    fn off_on_a_cluster_and_on_a_replica() {
        let mut app = app_with(Env::Local, None);
        app.session.as_mut().unwrap().server.as_mut().unwrap().mode = ServerMode::Cluster;
        for k in ['x', 'D'] {
            assert!(app.handle_key(ch(k)).is_empty());
            assert!(app.bulk.is_none());
            assert_eq!(app.notice.as_ref().unwrap().text, bulk::CLUSTER_OFF);
        }
        let mut app = app_with(Env::Local, None);
        app.session.as_mut().unwrap().target = Some(ScanTarget::Replica(crate::model::Endpoint {
            host: "r".into(),
            port: 1,
            tls: false,
        }));
        assert!(app.handle_key(ch('D')).is_empty());
        assert_eq!(app.notice.as_ref().unwrap().text, bulk::REPLICA_OFF);
    }

    #[test]
    fn a_closed_connection_stops_the_run() {
        let mut app = app_with(Env::Local, None);
        let s = steps(&app.handle_key(ch('D')));
        testing::reply(&mut app, s[0].0, counted(Walk::Done, 4, 0));
        app.handle_key(ch('y'));
        let (id, scan) = {
            let s = app.session.as_ref().unwrap();
            (s.id, s.scan)
        };
        app.apply(Msg::LiveClosed {
            id,
            scan,
            error: Some("gone".into()),
        });
        let Some(Bulk {
            stage: Stage::Done(d),
            ..
        }) = &app.bulk
        else {
            panic!("done");
        };
        assert!(d.error && d.lines[0].contains("the live connection closed"));
    }

    #[test]
    fn a_group_is_not_a_pattern() {
        let mut app = app_with(Env::Local, None);
        let rows = app.pattern_rows();
        if let Some(i) = rows.iter().position(|r| {
            matches!(r, Row::Group { root } if matches!(
                app.patterns.tree.roots.get(*root),
                Some(crate::tui::tree::Root::Group(_))
            ))
        }) {
            app.patterns.sel = i;
            assert!(app.handle_key(ch('D')).is_empty());
            assert!(app.bulk.is_none());
        }
    }

    #[test]
    fn snapshot_ttl_prompt_80x24() {
        let mut app = app_with(Env::Stage, Some(false));
        app.handle_key(ch('x'));
        let t = testing::render(&app, 80, 24);
        insta::assert_snapshot!("bulk_ttl_80x24", t.backend());
    }

    #[test]
    fn snapshot_dry_run_question_on_prod_80x24() {
        let mut app = app_with(Env::Prod, Some(false));
        app.handle_key(ch('x'));
        dry_run_expire_from_ttl(&mut app);
        type_text(&mut app, "user:{id");
        let t = testing::render(&app, 80, 24);
        insta::assert_snapshot!("bulk_expire_question_prod_80x24", t.backend());
        let text = format!("{:?}", t.backend());
        assert!(text.contains("prod-cache-01 · PROD"), "{text}");
    }

    #[test]
    fn snapshot_delete_question_80x24() {
        let mut app = app_with(Env::Local, None);
        let s = steps(&app.handle_key(ch('D')));
        testing::reply(&mut app, s[0].0, counted(Walk::Done, 3_120, 7));
        let t = testing::render(&app, 80, 24);
        insta::assert_snapshot!("bulk_delete_question_80x24", t.backend());
    }

    #[test]
    fn snapshot_progress_and_done_80x24() {
        let mut app = app_with(Env::Local, None);
        dry_run_expire(&mut app);
        let s = steps(&app.handle_key(ch('y')));
        let e = testing::reply(&mut app, s[0].0, batch(Walk::At(9), 500, Vec::new()));
        let s = steps(&e);
        let t = testing::render(&app, 80, 24);
        insta::assert_snapshot!("bulk_progress_80x24", t.backend());
        app.handle_key(key(KeyCode::Esc));
        let t = testing::render(&app, 80, 24);
        insta::assert_snapshot!("bulk_stopping_80x24", t.backend());
        testing::reply(&mut app, s[0].0, batch(Walk::At(12), 500, Vec::new()));
        let t = testing::render(&app, 80, 24);
        insta::assert_snapshot!("bulk_done_80x24", t.backend());
    }

    /// [`dry_run_expire`] from the TTL input already open.
    fn dry_run_expire_from_ttl(app: &mut App) {
        let s = steps(&app.handle_key(key(KeyCode::Enter)));
        testing::reply(app, s[0].0, counted(Walk::Done, 1_200, 200));
    }

    #[test]
    fn durations() {
        for (t, s) in [
            ("90s", 90),
            ("10m", 600),
            ("24h", 86_400),
            ("7d", 604_800),
            ("1h30m", 5_400),
            ("45", 45),
        ] {
            assert_eq!(bulk::parse_ttl(t), Ok(s), "{t}");
        }
        for t in ["", "0", "0s", "h", "10x", "5m3", "-1s"] {
            assert!(bulk::parse_ttl(t).is_err(), "{t}");
        }
        for (s, t) in [(90, "90s"), (600, "10m"), (86_400, "24h"), (604_800, "7d")] {
            assert_eq!(bulk::ttl_text(s), t);
        }
    }
}
