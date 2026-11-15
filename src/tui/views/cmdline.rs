//! The command line panel (spec "Command line panel"). Owner: part 4. Drawn by
//! `ui/cmdline.rs`, its requests in `src/live/cmdline.rs`.
//!
//! `:` opens it at the bottom of the screen, over the view, which keeps running. While
//! it is open every key except ctrl-c goes to it, and its input line replaces the key
//! bar. It is not a screen: [`App`] passes it keys, pastes and ticks itself (the
//! methods at the end of this file), and its replies come back as [`Owner::Cmdline`].
//!
//! - **What is sent.** A typed command is split like `redis-cli`
//!   ([`command::split`]), then decided by [`cmdline::gate`] with what `COMMAND INFO`
//!   said about its name, read once per name a session. Only readonly commands are
//!   sent; `@dangerous` ones ask first (`y`/`n`, or on a `prod` profile the profile
//!   name typed). One command runs at a time.
//! - **Credentials.** The input line is drawn token by token with the credential
//!   arguments masked ([`masked_input`]), as it is typed. The history keeps only
//!   [`command::masked_line`]. The cursor is always at the end of the line: typing
//!   adds there and backspace takes the last character, so a masked word is never
//!   edited half-seen.
//! - **Values.** Replies are held as [`Output`]; `Debug` of this state prints counts
//!   only, never the input, the history or a reply.
//! - **Views.** `ctrl-t` switches how every reply is drawn: text, table, JSON, raw
//!   ([`cmdline::View`]). Each [`Output`] holds all four, made when it came back.
//! - **Repeats.** `N cmd` (a leading count, at most [`cmdline::MAX_REPEAT`]) sends the
//!   command N times, one after another, each one checked as a single command is:
//!   readonly ones by [`cmdline::gate`] here and again where they are sent, writes
//!   through [`Act::write`], the write gate, every time. A `@dangerous` readonly one
//!   asks once for the whole run, naming the count; a write that would ask is not
//!   repeated. `esc` stops a run, keeping the replies so far. With pipeline mode on
//!   (`ctrl-p`), readonly repeats go [`cmdline::pipeline_batch`] at a time in one
//!   pipeline ([`Request::Batch`]); writes still go one at a time.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{Act, Input, LiveState, Owner, ViewState};
use crate::live::cmdline::{
    self, Args, CommandInfo, Docs, Gate, Output, Reply, Request, View, is_count_word,
};
use crate::live::command::{self, MASK, Secret};
use crate::live::keys::KeyRow;
use crate::live::{CANCELLED, Dest, LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::Env;
use crate::tui::app::{App, Effect};
use crate::value;
use crate::write::{Decision, Kind, WriteOp};

/// Why a repeated write that would ask first is not sent.
pub const REPEAT_ASKS: &str = "Not sent: this write asks first, and the write gate's \
     question covers one command, so it is not repeated. Send it without the count.";

/// Whether a write may be repeated: only a plain edit the write gate runs without a
/// question. A destructive or `@dangerous` write would ask without
/// `skip_non_prod_confirm`, so it is never repeated, whatever the setting; an edit on
/// `prod` asks, so it is not repeated either.
pub(crate) fn repeatable(op: &WriteOp, decision: &Decision) -> bool {
    op.kind() == Kind::Edit && *decision == Decision::Run
}

/// Why a write [`Act::write`] did not take was not sent.
const WRITE_NOT_SENT: &str =
    "Not sent: the live connection is not ready, or another write is waiting for an answer.";

/// The longest input line, in characters.
pub const MAX_INPUT: usize = 8_192;
/// Commands the history keeps.
pub const MAX_HISTORY: usize = 100;
/// Entries the output keeps.
pub const MAX_ENTRIES: usize = 100;
/// Bytes of replies the output keeps; the oldest entries go first.
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Rows `PgUp` and `PgDn` scroll the output by.
pub const SCROLL_STEP: usize = 5;

/// Why the helper is off on a server without `COMMAND DOCS`.
pub const HELPER_OLD: &str = "The helper is off: this server has no COMMAND DOCS (Redis 7.0 and later, and Valkey, have it).";

/// One argument of the input line: where it is in the text, and what it says.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Tok {
    pub start: usize,
    pub end: usize,
    pub word: Vec<u8>,
}

/// The input line split into arguments like [`command::split`], but never failing: an
/// unclosed quote runs to the end, and text after a closing quote goes on the word. For
/// drawing and completing while the line is typed.
pub(crate) fn tokens(line: &str) -> Vec<Tok> {
    let b = line.as_bytes();
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && space(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            return out;
        }
        let start = i;
        let mut word = Vec::new();
        let (mut dq, mut sq) = (false, false);
        while i < b.len() {
            let c = b[i];
            if dq {
                match c {
                    b'\\' if b.get(i + 1) == Some(&b'x') => {
                        match (
                            b.get(i + 2).and_then(|c| hex(*c)),
                            b.get(i + 3).and_then(|c| hex(*c)),
                        ) {
                            (Some(h), Some(l)) => {
                                word.push(h * 16 + l);
                                i += 4;
                            }
                            _ => {
                                word.push(b'x');
                                i += 2;
                            }
                        }
                    }
                    b'\\' if i + 1 < b.len() => {
                        word.push(match b[i + 1] {
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            b'b' => 0x08,
                            b'a' => 0x07,
                            c => c,
                        });
                        i += 2;
                    }
                    b'"' => {
                        dq = false;
                        i += 1;
                    }
                    c => {
                        word.push(c);
                        i += 1;
                    }
                }
            } else if sq {
                match c {
                    b'\\' if b.get(i + 1) == Some(&b'\'') => {
                        word.push(b'\'');
                        i += 2;
                    }
                    b'\'' => {
                        sq = false;
                        i += 1;
                    }
                    c => {
                        word.push(c);
                        i += 1;
                    }
                }
            } else {
                match c {
                    c if space(c) => break,
                    b'"' => {
                        dq = true;
                        i += 1;
                    }
                    b'\'' => {
                        sq = true;
                        i += 1;
                    }
                    c => {
                        word.push(c);
                        i += 1;
                    }
                }
            }
        }
        out.push(Tok {
            start,
            end: i.min(b.len()),
            word,
        });
    }
}

/// The input line as it is drawn: each argument as typed, escaped, except the
/// credential arguments ([`command::secrets`]), which show as [`MASK`] (an `ACL
/// SETUSER` rule keeps its first character). Masked from the first character typed.
pub fn masked_input(line: &str) -> String {
    let toks = tokens(line);
    let words: Vec<Vec<u8>> = toks.iter().map(|t| t.word.clone()).collect();
    // A leading repeat count is not the command: the secrets are found after it.
    let skip = cmdline::count_words(&words);
    let mut secrets = vec![None; skip];
    secrets.extend(command::secrets(&words[skip..]));
    let mut out = String::new();
    let mut pos = 0;
    for (t, s) in toks.iter().zip(secrets) {
        out.push_str(&value::escape(&line[pos..t.start]));
        match s {
            Some(Secret::Whole) => out.push_str(MASK),
            Some(Secret::Rule) => {
                if let Some(&c) = t.word.first() {
                    out.push(char::from(c));
                }
                out.push_str(MASK);
            }
            None => out.push_str(&value::escape(&line[t.start..t.end])),
        }
        pos = t.end;
    }
    out.push_str(&value::escape(&line[pos..]));
    out
}

/// What an entry of the output shows under its command.
#[derive(Clone, PartialEq)]
pub(crate) enum Body {
    /// Sent, or waiting for `COMMAND INFO`.
    Waiting,
    /// Waiting for the user to confirm.
    Asking,
    /// Not sent, or a note of the panel's own: why, safe to draw.
    Note(String),
    /// The request failed: redis-lens's own words ([`LiveError`]).
    Failed(String),
    /// The reply.
    Out(Output),
    /// The replies of a repeat (`N cmd`), numbered, and how far it got.
    Runs(Runs),
}

/// One reply of a repeat.
#[derive(Clone, PartialEq)]
pub(crate) struct RunOut {
    /// Its number in the run, from 1.
    pub n: u32,
    pub out: Output,
    /// Commands in the pipeline it came back in: 1 for a round trip of its own.
    pub batch: u32,
    /// The first reply of its pipeline: the one whose time counts for the batch.
    pub batch_start: bool,
}

/// How a repeat ended.
#[derive(Clone, PartialEq)]
pub(crate) enum RunEnd {
    /// Every command was sent and answered.
    Done,
    /// `esc`: what was sent was answered, nothing more is sent.
    Stopped,
    /// A command was not sent, or failed: why, safe to draw.
    Failed(String),
}

/// The replies of a repeat so far.
#[derive(Clone, PartialEq)]
pub(crate) struct Runs {
    pub times: u32,
    /// Commands sent so far.
    pub sent: u32,
    pub outs: Vec<RunOut>,
    /// The oldest replies dropped to keep the output under [`MAX_OUTPUT_BYTES`].
    pub dropped: u32,
    /// `esc` was pressed: the run ends once what was sent is answered.
    pub stopping: bool,
    pub end: Option<RunEnd>,
}

impl Runs {
    fn new(times: u32) -> Self {
        Self {
            times,
            sent: 0,
            outs: Vec::new(),
            dropped: 0,
            stopping: false,
            end: None,
        }
    }

    /// Replies come back so far, dropped ones included.
    pub fn answered(&self) -> u32 {
        self.dropped + u32::try_from(self.outs.len()).unwrap_or(u32::MAX)
    }

    fn bytes(&self) -> usize {
        self.outs.iter().map(|o| o.out.bytes()).sum()
    }
}

/// One command and what came of it.
#[derive(Clone, PartialEq)]
pub(crate) struct Entry {
    pub id: u64,
    /// The command as [`cmdline::masked_with_count`] writes it; `None` for a note
    /// alone.
    pub line: Option<String>,
    pub body: Body,
    /// On a cluster, the address of the node the command was sent to: its reply is
    /// headed `<address>>` (spec "Cluster live views (P4)", Command line).
    pub node: Option<String>,
    /// Why it went where it went, when that needs saying ([`super::KEYS_UNFOUND`]).
    pub route_note: Option<&'static str>,
    /// The command's flat array holds pairs ([`cmdline::pairs_hint`]), for the table.
    pub pairs: bool,
}

impl Entry {
    fn bytes(&self) -> usize {
        let body = match &self.body {
            Body::Out(o) => o.bytes(),
            Body::Runs(r) => r.bytes(),
            Body::Note(s) | Body::Failed(s) => s.len(),
            _ => 0,
        };
        body + self.line.as_ref().map_or(0, String::len)
    }
}

/// A request of the panel's own, by id.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    Info(String),
    Docs(String),
    Names,
    Run,
    Batch,
}

/// The one command on its way.
#[derive(Clone, PartialEq)]
enum Pending {
    /// Waiting for `COMMAND INFO` of `name` (request `id`) to decide; `times` from
    /// the line's repeat count.
    Info {
        id: ReqId,
        name: String,
        args: Args,
        entry: u64,
        times: u32,
    },
    /// Sent as request `id`.
    Run { id: ReqId, entry: u64 },
    /// A repeat's pipeline, sent as request `id`.
    Batch { id: ReqId, entry: u64 },
    /// A write handed to the write gate as request `id`: asking, or sent.
    Write { id: ReqId, entry: u64 },
}

/// The repeat on its way: what each of its commands is. Its progress is the entry's
/// [`Runs`].
#[derive(Clone, PartialEq)]
struct Repeat {
    entry: u64,
    args: Args,
    info: CommandInfo,
    /// A write, sent through the write gate each time; otherwise a readonly command.
    write: bool,
    /// A `@dangerous` readonly command whose question was answered yes.
    confirmed: bool,
    /// Where a readonly one goes (a write's gate routes it itself).
    dest: Option<Dest>,
    /// `esc` was pressed.
    stop: bool,
}

/// A readonly `@dangerous` command waiting for `y`, or for the profile name.
#[derive(Clone, PartialEq)]
pub(crate) struct Confirm {
    pub args: Args,
    pub info: CommandInfo,
    pub entry: u64,
    /// The masked command, as the prompt shows it.
    pub line: String,
    /// On a `prod` profile: the name typed so far.
    pub typed: Option<String>,
    pub profile: String,
    pub command: String,
    /// How many times it is sent: the line's repeat count, 1 without one. The
    /// question names it and is asked once for all of them.
    pub times: u32,
}

/// `COMMAND LIST`, asked once a session.
#[derive(Clone, PartialEq, Default)]
enum Names {
    #[default]
    NotAsked,
    Asked,
    Loaded(Vec<String>),
    /// The server has none, or refused it: names come from `COMMAND INFO` only.
    Off,
}

/// `tab` pressed again goes on to the next match.
#[derive(Clone, PartialEq)]
struct Completion {
    start: usize,
    matches: Vec<String>,
    at: usize,
    /// The input line right after the last `tab`.
    after: String,
}

#[derive(Default)]
pub struct CmdlineState {
    pub open: bool,
    /// What is typed. It can hold a credential: drawn only through [`masked_input`].
    input: String,
    history: Vec<String>,
    /// Where `↑↓` is in the history, and the line typed before it started.
    hist_at: Option<usize>,
    draft: Option<String>,
    pub(crate) entries: Vec<Entry>,
    next_entry: u64,
    /// Rows the output is scrolled up from the bottom; the draw clamps it.
    pub(crate) scroll: Cell<usize>,
    infos: HashMap<String, Option<CommandInfo>>,
    docs: HashMap<String, Option<Docs>>,
    /// Why the helper is off, once it is.
    docs_off: Option<String>,
    /// Why no command can be checked: `COMMAND INFO` was refused.
    info_off: Option<String>,
    names: Names,
    asked: HashMap<ReqId, Asked>,
    pending: Option<Pending>,
    pub(crate) confirm: Option<Confirm>,
    completion: Option<Completion>,
    /// A line of the panel's own under the output, until the next key.
    pub(crate) status: Option<String>,
    /// How replies are drawn: `ctrl-t` goes to the next view.
    pub(crate) view: View,
    /// Pipeline mode (`ctrl-p`): a readonly repeat goes in pipelines. Off at first.
    pub(crate) pipeline: bool,
    /// The repeat on its way, if one is.
    repeat: Option<Repeat>,
}

/// Counts only: the input, the history and the replies are the user's.
impl fmt::Debug for CmdlineState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CmdlineState")
            .field("open", &self.open)
            .field("input_chars", &self.input.chars().count())
            .field("history", &self.history.len())
            .field("hist_at", &self.hist_at)
            .field("entries", &self.entries.len())
            .field("scroll", &self.scroll.get())
            .field("infos", &self.infos.len())
            .field("docs", &self.docs.len())
            .field("docs_off", &self.docs_off.is_some())
            .field("info_off", &self.info_off.is_some())
            .field("asked", &self.asked.len())
            .field("pending", &self.pending.is_some())
            .field("confirm", &self.confirm.is_some())
            .field("completion", &self.completion.is_some())
            .field("status", &self.status.is_some())
            .field("view", &self.view)
            .field("pipeline", &self.pipeline)
            .field("repeat", &self.repeat.is_some())
            .finish()
    }
}

/// The first word as `COMMAND INFO` is asked about it: lower-case. `None` when it is
/// not something to ask about (empty, too long, control characters).
pub(crate) fn name_of(word: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(word).ok()?;
    (!s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_graphic()))
        .then(|| s.to_ascii_lowercase())
}

/// A command or subcommand name completed in the case the user typed it in.
fn in_case(name: &str, typed: &[u8]) -> String {
    if typed.iter().any(u8::is_ascii_lowercase) {
        name.to_ascii_lowercase()
    } else {
        name.to_ascii_uppercase()
    }
}

impl CmdlineState {
    /// The history, masked, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// The input line as drawn.
    pub fn shown_input(&self) -> String {
        masked_input(&self.input)
    }

    pub fn running(&self) -> bool {
        self.pending.is_some()
    }

    /// A repeat is on its way: `esc` stops it.
    pub fn repeating(&self) -> bool {
        self.repeat.is_some()
    }

    fn push_entry(&mut self, line: Option<String>, body: Body) -> u64 {
        let id = self.next_entry;
        self.next_entry += 1;
        self.entries.push(Entry {
            id,
            line,
            body,
            node: None,
            route_note: None,
            pairs: false,
        });
        self.scroll.set(0);
        self.trim();
        id
    }

    fn trim(&mut self) {
        // Never the entry a pending command, repeat or question writes to.
        let keep = |s: &Self| match (&s.pending, &s.confirm, &s.repeat) {
            (_, _, Some(r)) => Some(r.entry),
            (
                Some(
                    Pending::Info { entry, .. }
                    | Pending::Run { entry, .. }
                    | Pending::Batch { entry, .. }
                    | Pending::Write { entry, .. },
                ),
                _,
                _,
            ) => Some(*entry),
            (_, Some(c), _) => Some(c.entry),
            _ => None,
        };
        let keep = keep(self);
        while self.entries.len() > 1
            && (self.entries.len() > MAX_ENTRIES
                || self.entries.iter().map(Entry::bytes).sum::<usize>() > MAX_OUTPUT_BYTES)
            && Some(self.entries[0].id) != keep
        {
            self.entries.remove(0);
        }
    }

    fn set_body(&mut self, entry: u64, body: Body) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == entry) {
            e.body = body;
        }
        self.trim();
    }

    fn note(&mut self, text: impl Into<String>) {
        self.push_entry(None, Body::Note(text.into()));
    }

    /// Where `args` goes on a cluster ([`super::command_to`]), written on `entry` so
    /// its reply is headed by the node. `None`, with the refusal on `entry`, when its
    /// keys are in different slots.
    fn route(
        &mut self,
        args: &Args,
        info: &CommandInfo,
        entry: u64,
        act: &Act<'_>,
    ) -> Option<crate::live::Dest> {
        let to = match super::command_to(act.session, args.as_slice(), info) {
            Ok(to) => to,
            Err(why) => {
                self.set_body(entry, Body::Note(why));
                return None;
            }
        };
        let node = to
            .node
            .and_then(|i| act.session.nodes.get(i))
            .map(|n| value::escape(&n.endpoint.to_string()));
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == entry) {
            e.node = node;
            e.route_note = to.note;
        }
        Some(to.dest)
    }

    /// Sends one of the panel's own requests.
    fn ask(&mut self, req: Request, what: Asked, act: &mut Act<'_>) -> Option<ReqId> {
        let id = act.send(LiveRequest::Command(req))?;
        self.asked.insert(id, what);
        Some(id)
    }

    fn asked_info(&self, name: &str) -> Option<ReqId> {
        self.asked
            .iter()
            .find(|(_, a)| matches!(a, Asked::Info(n) if n == name))
            .map(|(id, _)| *id)
    }

    fn asked_docs(&self, name: &str) -> bool {
        self.asked
            .values()
            .any(|a| matches!(a, Asked::Docs(n) if n == name))
    }

    /// Opens the panel; the command names are asked for the first time.
    pub(crate) fn open(&mut self, act: &mut Act<'_>) {
        self.open = true;
        if self.names == Names::NotAsked && self.ask(Request::Names, Asked::Names, act).is_some() {
            self.names = Names::Asked;
        }
    }

    /// Once the first word is typed out (a space after it), its flags and docs are read,
    /// once a name, for the helper and the decision.
    fn prefetch(&mut self, act: &mut Act<'_>) {
        let toks = tokens(&self.input);
        // After a repeat count, the command is the next word.
        let skip = usize::from(toks.first().is_some_and(|t| is_count_word(&t.word)));
        let Some(first) = toks.get(skip) else {
            return;
        };
        if toks.len() == skip + 1 && first.end == self.input.len() {
            return;
        }
        // A bad count is refused before anything is sent: not even the flags.
        let words: Vec<Vec<u8>> = toks.iter().map(|t| t.word.clone()).collect();
        if cmdline::parse_repeat(&words).is_err() {
            return;
        }
        let Some(name) = name_of(&first.word) else {
            return;
        };
        if command::refused(std::slice::from_ref(&first.word)).is_some() {
            return;
        }
        if self.info_off.is_none()
            && !self.infos.contains_key(&name)
            && self.asked_info(&name).is_none()
        {
            self.ask(
                Request::Info { name: name.clone() },
                Asked::Info(name.clone()),
                act,
            );
        }
        if self.docs_off.is_none() && !self.docs.contains_key(&name) && !self.asked_docs(&name) {
            self.ask(Request::Docs { name: name.clone() }, Asked::Docs(name), act);
        }
    }

    /// A key while the panel is open. `keys` are the key list's loaded keys, for `tab`.
    pub(crate) fn key(&mut self, key: KeyEvent, keys: &[KeyRow], act: &mut Act<'_>) {
        self.status = None;
        if self.confirm.is_some() {
            self.confirm_key(key, act);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if key.code != KeyCode::Tab {
            self.completion = None;
        }
        match key.code {
            // A repeat on its way stops first; the next esc closes.
            KeyCode::Esc if self.repeat.is_some() => self.stop_repeat(),
            KeyCode::Esc => self.open = false,
            KeyCode::Enter => self.submit(act),
            KeyCode::Char('t') if ctrl => {
                self.view = self.view.next();
                self.scroll.set(0);
            }
            KeyCode::Char('p') if ctrl => self.pipeline = !self.pipeline,
            KeyCode::Up => self.history_step(true),
            KeyCode::Down => self.history_step(false),
            KeyCode::Tab => self.complete(keys, act),
            KeyCode::PageUp => self.scroll.set(self.scroll.get() + SCROLL_STEP),
            KeyCode::PageDown => self
                .scroll
                .set(self.scroll.get().saturating_sub(SCROLL_STEP)),
            KeyCode::Char('l') if ctrl => {
                self.entries.clear();
                self.scroll.set(0);
            }
            KeyCode::Char('u') if ctrl => self.edit(act, String::clear),
            KeyCode::Backspace => self.edit(act, |s| {
                s.pop();
            }),
            KeyCode::Char(c)
                if !ctrl && !alt && !c.is_control() && self.input.chars().count() < MAX_INPUT =>
            {
                self.edit(act, |s| s.push(c));
            }
            _ => {}
        }
    }

    fn edit(&mut self, act: &mut Act<'_>, f: impl FnOnce(&mut String)) {
        f(&mut self.input);
        self.hist_at = None;
        self.draft = None;
        self.prefetch(act);
    }

    /// Pasted text: one line, tabs and newlines as spaces, no control characters.
    pub(crate) fn paste(&mut self, text: &str, act: &mut Act<'_>) {
        if let Some(c) = self.confirm.as_mut() {
            if let Some(t) = c.typed.as_mut() {
                t.extend(text.chars().filter(|c| !c.is_control()).take(256));
            }
            return;
        }
        self.completion = None;
        let mut n = self.input.chars().count();
        for c in text.chars() {
            if n >= MAX_INPUT {
                break;
            }
            match c {
                '\n' | '\r' | '\t' => self.input.push(' '),
                c if c.is_control() => continue,
                c => self.input.push(c),
            }
            n += 1;
        }
        self.hist_at = None;
        self.draft = None;
        self.prefetch(act);
    }

    fn history_step(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() - 1;
        self.hist_at = match (self.hist_at, back) {
            (None, true) => {
                self.draft = Some(std::mem::take(&mut self.input));
                Some(last)
            }
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i < last => Some(i + 1),
            (Some(_), false) => {
                self.input = self.draft.take().unwrap_or_default();
                self.hist_at = None;
                return;
            }
        };
        if let Some(i) = self.hist_at {
            self.input = self.history[i].clone();
        }
    }

    /// `tab`: completes the word at the end of the line, a command name, a subcommand
    /// or a loaded key name. Again goes on to the next match.
    fn complete(&mut self, keys: &[KeyRow], act: &mut Act<'_>) {
        if let Some(c) = self.completion.as_mut()
            && c.after == self.input
            && c.matches.len() > 1
        {
            c.at = (c.at + 1) % c.matches.len();
            self.input.truncate(c.start);
            self.input.push_str(&c.matches[c.at]);
            c.after = self.input.clone();
            return;
        }
        self.completion = None;
        let toks = tokens(&self.input);
        let at_end =
            self.input.is_empty() || self.input.chars().last().is_some_and(char::is_whitespace);
        let (index, prefix, start) = match toks.last() {
            Some(t) if !at_end => (toks.len() - 1, t.word.clone(), t.start),
            _ => (toks.len(), Vec::new(), self.input.len()),
        };
        // After a repeat count the command is the second word.
        let skip = usize::from(toks.first().is_some_and(|t| is_count_word(&t.word)) && index > 0);
        let index = index - skip;
        let matches: Vec<String> = if index == 0 {
            if self.names == Names::NotAsked {
                self.open(act);
            }
            let lower = String::from_utf8_lossy(&prefix).to_ascii_lowercase();
            let mut all: Vec<&str> = match &self.names {
                Names::Loaded(n) => n.iter().map(String::as_str).collect(),
                _ => Vec::new(),
            };
            all.extend(
                self.infos
                    .iter()
                    .filter(|(_, i)| i.is_some())
                    .map(|(n, _)| n.as_str()),
            );
            all.sort_unstable();
            all.dedup();
            all.into_iter()
                .filter(|n| n.starts_with(&lower))
                .map(|n| in_case(n, &prefix))
                .collect()
        } else {
            let info = toks
                .get(skip)
                .and_then(|t| name_of(&t.word))
                .and_then(|n| self.infos.get(&n).cloned().flatten());
            match info {
                Some(info) if index == 1 && !info.subcommands.is_empty() => {
                    let lower = String::from_utf8_lossy(&prefix).to_ascii_lowercase();
                    let mut subs: Vec<String> = info
                        .subcommands
                        .iter()
                        .filter_map(|s| s.name.split_once('|').map(|(_, sub)| sub.to_string()))
                        .filter(|s| {
                            s.starts_with(&lower) && s.chars().all(|c| c.is_ascii_graphic())
                        })
                        .map(|s| in_case(&s, &prefix))
                        .collect();
                    subs.sort();
                    subs
                }
                _ => {
                    let mut found: Vec<&str> = keys
                        .iter()
                        .map(|k| k.key.as_str())
                        .filter(|k| k.as_bytes().starts_with(&prefix))
                        .collect();
                    found.sort_unstable();
                    let mut out: Vec<String> = found
                        .into_iter()
                        .map(|k| {
                            let mut w = String::new();
                            command::push_word(k.as_bytes(), &mut w);
                            w
                        })
                        .collect();
                    out.dedup();
                    out
                }
            }
        };
        let Some(first) = matches.first() else {
            self.status = Some("Nothing to complete.".into());
            return;
        };
        self.input.truncate(start);
        self.input.push_str(first);
        self.completion = Some(Completion {
            start,
            after: self.input.clone(),
            matches,
            at: 0,
        });
        self.prefetch(act);
    }

    /// `enter`: splits the line, keeps it in the history (masked), and decides.
    fn submit(&mut self, act: &mut Act<'_>) {
        if self.input.trim().is_empty() {
            return;
        }
        if self.pending.is_some() {
            self.status = Some("A command is still running: wait for its reply.".into());
            return;
        }
        let text = std::mem::take(&mut self.input);
        self.hist_at = None;
        self.draft = None;
        let all = match command::split(&text) {
            Ok(a) => a,
            Err(e) => {
                self.push_entry(Some(masked_input(&text)), Body::Note(e.message().into()));
                return;
            }
        };
        let line = cmdline::masked_with_count(&all);
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
        // `N cmd`: a bad count is refused before anything is sent, its flags included.
        let times = match cmdline::parse_repeat(&all) {
            Ok(n) => n.unwrap_or(1),
            Err(why) => {
                self.push_entry(Some(line), Body::Note(why));
                return;
            }
        };
        let args = all[cmdline::count_words(&all)..].to_vec();
        if let Some(why) = command::refused(&args) {
            self.push_entry(Some(line), Body::Note(why.into()));
            return;
        }
        if let Some(why) = &self.info_off {
            let why = why.clone();
            self.push_entry(Some(line), Body::Note(why));
            return;
        }
        if !act.live_ready() {
            self.push_entry(
                Some(line),
                Body::Failed("Not sent: the live connection is not open.".into()),
            );
            return;
        }
        let Some(name) = name_of(&args[0]) else {
            let why = format!(
                "Not sent: the server has no command {}.",
                cmdline::typed_name(&args)
            );
            self.push_entry(Some(line), Body::Note(why));
            return;
        };
        let entry = self.push_entry(Some(line), Body::Waiting);
        let pairs = cmdline::pairs_hint(&args);
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == entry) {
            e.pairs = pairs;
        }
        let args = Args::new(args);
        if let Some(info) = self.infos.get(&name).cloned() {
            self.decide(args, info, entry, times, act);
            return;
        }
        let id = match self.asked_info(&name) {
            Some(id) => Some(id),
            None => self.ask(
                Request::Info { name: name.clone() },
                Asked::Info(name.clone()),
                act,
            ),
        };
        match id {
            Some(id) => {
                self.pending = Some(Pending::Info {
                    id,
                    name,
                    args,
                    entry,
                    times,
                })
            }
            None => self.set_body(
                entry,
                Body::Failed("Not sent: the live connection is not open.".into()),
            ),
        }
    }

    /// What [`cmdline::gate`] says, done. `times` is the line's repeat count (1
    /// without one): every one of them is checked as a single command is.
    fn decide(
        &mut self,
        args: Args,
        info: Option<CommandInfo>,
        entry: u64,
        times: u32,
        act: &mut Act<'_>,
    ) {
        match cmdline::gate(args.as_slice(), info.as_ref()) {
            Gate::Refuse(why) => self.set_body(entry, Body::Note(why)),
            Gate::Write => {
                let Some(info) = info else {
                    return;
                };
                // On a cluster: keys in one slot, on a node that takes writes; the
                // write gate routes it the same way (`write_dest`).
                if self.route(&args, &info, entry, act).is_none() {
                    return;
                }
                let op = WriteOp::Command { args, info };
                if let Err(why) = super::write_dest(act.session, &op) {
                    self.set_body(entry, Body::Note(why));
                    return;
                }
                // The read-only reason, or a command the server refused, in the panel.
                let decision = act.write_decision(&op);
                if let Decision::Refuse(why) = decision {
                    self.set_body(entry, Body::Note(why));
                    return;
                }
                if times > 1 {
                    // The write gate's question is about one command: a write that
                    // asks is not repeated, and nothing is sent. That is any write but
                    // a plain edit, also when `skip_non_prod_confirm` takes the
                    // question away (spec "Repeat").
                    if !repeatable(&op, &decision) {
                        self.set_body(entry, Body::Note(REPEAT_ASKS.into()));
                        return;
                    }
                    let WriteOp::Command { args, info } = op else {
                        return;
                    };
                    self.start_repeat(args, info, true, false, None, times, entry, act);
                    return;
                }
                match act.write(op) {
                    Some(id) => {
                        self.pending = Some(Pending::Write { id, entry });
                        self.set_body(entry, Body::Waiting);
                    }
                    None => self.set_body(entry, Body::Failed(WRITE_NOT_SENT.into())),
                }
            }
            Gate::Send => {
                let Some(info) = info else {
                    return;
                };
                if times > 1 {
                    let Some(dest) = self.route(&args, &info, entry, act) else {
                        return;
                    };
                    self.start_repeat(args, info, false, false, Some(dest), times, entry, act);
                    return;
                }
                self.send(args, info, false, entry, act);
            }
            Gate::Ask => {
                let Some(info) = info else {
                    return;
                };
                // Keys in different slots: refused before asking.
                if self.route(&args, &info, entry, act).is_none() {
                    return;
                }
                let p = act.profile();
                let cmd = args
                    .as_slice()
                    .get(1)
                    .and_then(|s| info.subcommand(s))
                    .unwrap_or(&info)
                    .display();
                self.confirm = Some(Confirm {
                    line: command::masked_line(args.as_slice()),
                    args,
                    info,
                    entry,
                    typed: (p.env == Env::Prod).then(String::new),
                    profile: p.name.clone(),
                    command: cmd,
                    times,
                });
                self.set_body(entry, Body::Asking);
            }
        }
    }

    fn send(
        &mut self,
        args: Args,
        info: CommandInfo,
        confirmed: bool,
        entry: u64,
        act: &mut Act<'_>,
    ) {
        let Some(dest) = self.route(&args, &info, entry, act) else {
            return;
        };
        let req = Request::Run {
            args,
            info,
            confirmed,
        };
        let sent = act.send_to(dest, LiveRequest::Command(req));
        if let Some(id) = sent {
            self.asked.insert(id, Asked::Run);
        }
        match sent {
            Some(id) => {
                self.pending = Some(Pending::Run { id, entry });
                self.set_body(entry, Body::Waiting);
            }
            None => self.set_body(
                entry,
                Body::Failed("Not sent: the live connection is not open.".into()),
            ),
        }
    }

    /// Starts a repeat of `times` commands on `entry`: the first is sent now, each
    /// next one when the one before is answered ([`Self::next_repeat`]).
    #[allow(clippy::too_many_arguments)]
    fn start_repeat(
        &mut self,
        args: Args,
        info: CommandInfo,
        write: bool,
        confirmed: bool,
        dest: Option<Dest>,
        times: u32,
        entry: u64,
        act: &mut Act<'_>,
    ) {
        self.set_body(entry, Body::Runs(Runs::new(times)));
        self.repeat = Some(Repeat {
            entry,
            args,
            info,
            write,
            confirmed,
            dest,
            stop: false,
        });
        self.next_repeat(act);
    }

    fn runs_mut(&mut self, entry: u64) -> Option<&mut Runs> {
        self.entries
            .iter_mut()
            .find(|e| e.id == entry)
            .and_then(|e| match &mut e.body {
                Body::Runs(r) => Some(r),
                _ => None,
            })
    }

    /// Sends the repeat's next command, or pipeline, or ends it: when `esc` was
    /// pressed, when all were sent, or when the next one is not let through. Each one
    /// is checked as a single command would be, right before it is sent: a readonly one
    /// by [`cmdline::gate`] (and again by [`cmdline::run`]), a write by the write gate
    /// ([`Act::write_decision`], then [`Act::write`], which decides again and grants
    /// its permit).
    fn next_repeat(&mut self, act: &mut Act<'_>) {
        let Some(r) = self.repeat.clone() else {
            return;
        };
        let Some((sent, times)) = self.runs_mut(r.entry).map(|x| (x.sent, x.times)) else {
            self.repeat = None;
            return;
        };
        if r.stop {
            return self.end_repeat(RunEnd::Stopped);
        }
        if sent >= times {
            return self.end_repeat(RunEnd::Done);
        }
        if r.write {
            let op = WriteOp::Command {
                args: r.args,
                info: r.info,
            };
            let decision = act.write_decision(&op);
            if let Decision::Refuse(why) = decision {
                return self.end_repeat(RunEnd::Failed(why));
            }
            if !repeatable(&op, &decision) {
                return self.end_repeat(RunEnd::Failed(REPEAT_ASKS.into()));
            }
            let Some(id) = act.write(op) else {
                return self.end_repeat(RunEnd::Failed(WRITE_NOT_SENT.into()));
            };
            self.pending = Some(Pending::Write { id, entry: r.entry });
            if let Some(x) = self.runs_mut(r.entry) {
                x.sent += 1;
            }
            return;
        }
        match cmdline::gate(r.args.as_slice(), Some(&r.info)) {
            Gate::Send => {}
            Gate::Ask if r.confirmed => {}
            Gate::Refuse(why) => return self.end_repeat(RunEnd::Failed(why)),
            Gate::Ask | Gate::Write => {
                return self.end_repeat(RunEnd::Failed(cmdline::NOT_SENT.into()));
            }
        }
        let Some(dest) = r.dest else {
            return self.end_repeat(RunEnd::Failed(cmdline::NOT_SENT.into()));
        };
        let n = if self.pipeline {
            let rate = crate::scan::ScanOptions::for_profile(act.profile()).max_commands_per_sec;
            cmdline::pipeline_batch(rate, times - sent)
        } else {
            1
        };
        let req = if n > 1 {
            Request::Batch {
                args: r.args,
                info: r.info,
                confirmed: r.confirmed,
                times: n,
            }
        } else {
            Request::Run {
                args: r.args,
                info: r.info,
                confirmed: r.confirmed,
            }
        };
        let Some(id) = act.send_to(dest, LiveRequest::Command(req)) else {
            return self.end_repeat(RunEnd::Failed(
                "Not sent: the live connection is not open.".into(),
            ));
        };
        if n > 1 {
            self.asked.insert(id, Asked::Batch);
            self.pending = Some(Pending::Batch { id, entry: r.entry });
        } else {
            self.asked.insert(id, Asked::Run);
            self.pending = Some(Pending::Run { id, entry: r.entry });
        }
        if let Some(x) = self.runs_mut(r.entry) {
            x.sent += n;
        }
    }

    /// Ends the repeat, keeping its replies.
    fn end_repeat(&mut self, end: RunEnd) {
        let Some(r) = self.repeat.take() else {
            return;
        };
        if let Some(x) = self.runs_mut(r.entry) {
            x.end = Some(end);
            x.stopping = false;
        }
        self.scroll.set(0);
        self.trim();
    }

    /// `esc` while a repeat is on its way: nothing more is sent; the command already
    /// sent is answered and kept.
    fn stop_repeat(&mut self) {
        let Some(r) = self.repeat.as_mut() else {
            return;
        };
        r.stop = true;
        let entry = r.entry;
        if let Some(x) = self.runs_mut(entry) {
            x.stopping = true;
        }
        if self.pending.is_none() {
            self.end_repeat(RunEnd::Stopped);
        }
    }

    /// The repeat on `entry`, if one is.
    fn repeat_on(&self, entry: u64) -> bool {
        self.repeat.as_ref().is_some_and(|r| r.entry == entry)
    }

    /// Replies of the repeat on `entry` came back, from a pipeline of `batch`: kept,
    /// the oldest dropped past [`MAX_OUTPUT_BYTES`], then the next one goes.
    fn repeat_replies(&mut self, entry: u64, outs: Vec<Output>, act: &mut Act<'_>) {
        if let Some(x) = self.runs_mut(entry) {
            let batch = u32::try_from(outs.len()).unwrap_or(u32::MAX);
            for (i, out) in outs.into_iter().enumerate() {
                let n = x.answered() + 1;
                x.outs.push(RunOut {
                    n,
                    out,
                    batch,
                    batch_start: i == 0,
                });
            }
            while x.outs.len() > 1 && x.bytes() > MAX_OUTPUT_BYTES {
                x.outs.remove(0);
                x.dropped += 1;
            }
        }
        self.scroll.set(0);
        self.trim();
        self.next_repeat(act);
    }

    /// The command on `entry` failed or will never be answered: a repeat ends with
    /// `why`, keeping its replies; a single command shows it.
    fn failed(&mut self, entry: u64, why: String) {
        if self.repeat_on(entry) {
            self.end_repeat(RunEnd::Failed(why));
        } else {
            self.set_body(entry, Body::Failed(why));
        }
    }

    /// A key while a `@dangerous` command waits: `y` or `n`, or on `prod` the profile
    /// name and `enter`. `esc` cancels.
    fn confirm_key(&mut self, key: KeyEvent, act: &mut Act<'_>) {
        let Some(c) = self.confirm.as_mut() else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let yes = match (&mut c.typed, key.code) {
            (_, KeyCode::Esc) => false,
            (None, KeyCode::Char('y' | 'Y')) if !ctrl => true,
            (None, KeyCode::Char('n' | 'N')) if !ctrl => false,
            (None, _) => return,
            (Some(t), KeyCode::Enter) => {
                if *t != c.profile {
                    t.clear();
                    self.status = Some(format!(
                        "That is not the profile name. Type {} exactly, or esc to cancel.",
                        c.profile
                    ));
                    return;
                }
                true
            }
            (Some(t), KeyCode::Backspace) => {
                t.pop();
                return;
            }
            (Some(t), KeyCode::Char('u')) if ctrl => {
                t.clear();
                return;
            }
            (Some(t), KeyCode::Char(ch)) if !ctrl && !ch.is_control() => {
                if t.chars().count() < 256 {
                    t.push(ch);
                }
                return;
            }
            (Some(_), _) => return,
        };
        let Some(c) = self.confirm.take() else {
            return;
        };
        if yes && c.times > 1 {
            // One answer for the whole run: it named the count.
            if let Some(dest) = self.route(&c.args, &c.info, c.entry, act) {
                self.start_repeat(
                    c.args,
                    c.info,
                    false,
                    true,
                    Some(dest),
                    c.times,
                    c.entry,
                    act,
                );
            }
        } else if yes {
            self.send(c.args, c.info, true, c.entry, act);
        } else {
            self.set_body(c.entry, Body::Note("Not sent: cancelled.".into()));
        }
    }

    /// The input line, which replaces the key bar while the panel is open.
    pub fn input(&self) -> Input {
        match &self.confirm {
            Some(Confirm { typed: Some(t), .. }) => Input {
                label: "confirm",
                text: value::escape(t),
                hint: "  type the profile name · enter send · esc cancel",
            },
            Some(_) => Input {
                label: "y/n",
                text: String::new(),
                hint: "  y send · n or esc cancel",
            },
            None if self.repeat.is_some() => Input {
                label: ":",
                text: self.shown_input(),
                hint: "  esc stop the run · ctrl-t view",
            },
            None => Input {
                label: ":",
                text: self.shown_input(),
                hint: "  enter run · tab complete · esc close",
            },
        }
    }

    /// The helper under the output: what the typed command is and does, from
    /// `COMMAND INFO` and `COMMAND DOCS`. At most two lines, each safe to draw, and
    /// whether the first is a warning.
    pub(crate) fn helper(&self) -> Vec<(String, bool)> {
        if let Some(c) = &self.confirm {
            // A repeat asks once, naming the count.
            let (it, times) = match c.times {
                0 | 1 => ("it".to_string(), String::new()),
                n => (format!("it {n} times"), format!(" {n} times")),
            };
            let ask = match &c.typed {
                Some(_) => format!(
                    "Type {} and press enter to send {it}, or esc to cancel.",
                    c.profile
                ),
                None => format!("Send {it} to {}? y to send, n or esc to cancel.", c.profile),
            };
            // A wrong name says so in place of the question.
            let second = match &self.status {
                Some(s) => (s.clone(), true),
                None => (ask, false),
            };
            return vec![
                (
                    format!(
                        "▲ {} is @dangerous and asks first: {}{times}",
                        c.command, c.line
                    ),
                    true,
                ),
                second,
            ];
        }
        if let Some(s) = &self.status {
            return vec![(s.clone(), true)];
        }
        let toks = tokens(&self.input);
        let Some(first) = toks.first() else {
            return vec![(
                "Writes go through the write gate. ctrl-t view, ctrl-p pipeline, N cmd \
                 repeats; tab completes, ↑↓ history, ctrl-l clears."
                    .into(),
                false,
            )];
        };
        // `N cmd`: the count, then the command.
        let all: Vec<Vec<u8>> = toks.iter().map(|t| t.word.clone()).collect();
        let skip = usize::from(is_count_word(&first.word) && toks.len() > 1);
        let times = match cmdline::parse_repeat(&all) {
            Ok(n) => n.unwrap_or(1),
            Err(why) => return vec![(why, true)],
        };
        let words = all[skip..].to_vec();
        let Some(name) = words.first().and_then(|w| name_of(w)) else {
            return Vec::new();
        };
        if let Some(why) = command::refused(&words) {
            return vec![(why.to_string(), true)];
        }
        let info = match self.infos.get(&name) {
            Some(Some(i)) => i,
            Some(None) => {
                return vec![(
                    format!("Unknown command {}.", cmdline::typed_name(&words)),
                    true,
                )];
            }
            None => return Vec::new(),
        };
        let sub = words.get(1).filter(|_| !info.subcommands.is_empty());
        let cmd = sub.and_then(|s| info.subcommand(s)).unwrap_or(info);
        let docs = self.docs.get(&name).and_then(Option::as_ref);
        let docs = match (docs, sub) {
            (Some(d), Some(s)) => d.subcommand(&name, s).or(Some(d)),
            (d, _) => d,
        };
        let mut verdict = match cmdline::gate(&words, Some(info)) {
            Gate::Send => ("readonly".to_string(), false),
            Gate::Ask => ("readonly, asks first (@dangerous)".to_string(), true),
            Gate::Write => ("writes: through the write gate".to_string(), true),
            Gate::Refuse(_)
                if !cmd.has_flag("readonly")
                    && cmd.name == info.name
                    && sub.is_none()
                    && !info.subcommands.is_empty() =>
            {
                ("needs a subcommand".to_string(), true)
            }
            Gate::Refuse(_) => ("not sent here".to_string(), true),
        };
        if times > 1 {
            let how = if self.pipeline {
                "in pipelines"
            } else {
                "one at a time"
            };
            verdict.0 = format!("{}, sent {times} times {how}", verdict.0);
        }
        let mut first_line = cmd.display();
        if let Some(d) = docs.filter(|d| !d.arguments.is_empty()) {
            first_line.push(' ');
            first_line.push_str(&d.arguments);
        }
        if let Some(d) = docs.filter(|d| !d.complexity.is_empty()) {
            first_line.push_str(" · ");
            first_line.push_str(&d.complexity);
        }
        let mut second = format!(
            "{} · {}",
            verdict.0,
            crate::value::escape(&cmd.categories.join(" "))
        );
        if let Some(d) = docs.filter(|d| !d.summary.is_empty()) {
            second = format!("{} · {second}", d.summary);
        }
        vec![(first_line, verdict.1), (second, false)]
    }

    fn answer(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        let Some(what) = self.asked.remove(&id) else {
            return;
        };
        let reply = match result {
            Ok(LiveReply::Command(r)) => Ok(r),
            Ok(_) => Err(LiveError::Failed("an unexpected reply".into())),
            Err(e) => Err(e),
        };
        match what {
            Asked::Names => {
                self.names = match reply {
                    Ok(Reply::Names(n)) => Names::Loaded(n),
                    _ => Names::Off,
                };
            }
            Asked::Docs(name) => match reply {
                Ok(Reply::Docs(d)) => {
                    self.docs.insert(name, d);
                }
                Err(LiveError::Unsupported(_)) => self.helper_off(HELPER_OLD.into()),
                Err(e @ LiveError::Denied(_)) => {
                    self.helper_off(format!("The helper is off: {e}."))
                }
                _ => {
                    self.docs.insert(name, None);
                }
            },
            Asked::Info(name) => {
                let pending = match &self.pending {
                    Some(Pending::Info { id: p, .. }) if *p == id => self.pending.take(),
                    _ => None,
                };
                match reply {
                    Ok(Reply::Info(i)) => {
                        self.infos.insert(name, i.clone());
                        if let Some(Pending::Info {
                            args, entry, times, ..
                        }) = pending
                        {
                            self.decide(args, i, entry, times, act);
                        }
                    }
                    Ok(_) | Err(_) => {
                        let e = match reply {
                            Err(e) => e,
                            _ => LiveError::Failed(
                                "COMMAND INFO came back in an unexpected shape".into(),
                            ),
                        };
                        // Without the flags nothing is sent: fail closed.
                        let why = format!(
                            "Not sent: the command line could not read the command's flags ({e})."
                        );
                        if matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_)) {
                            self.info_off = Some(why.clone());
                        }
                        if let Some(Pending::Info { entry, .. }) = pending {
                            self.set_body(entry, Body::Note(why));
                        }
                    }
                }
            }
            Asked::Run => {
                let Some(Pending::Run { id: p, entry }) = self.pending.clone() else {
                    return;
                };
                if p != id {
                    return;
                }
                self.pending = None;
                if self.repeat_on(entry) {
                    match reply {
                        Ok(Reply::Ran(out)) => self.repeat_replies(entry, vec![out], act),
                        Ok(_) => self.failed(entry, SHAPE.into()),
                        Err(e) => self.failed(entry, e.to_string()),
                    }
                    return;
                }
                let body = match reply {
                    Ok(Reply::Ran(out)) => Body::Out(out),
                    Ok(_) => Body::Failed(SHAPE.into()),
                    Err(e) => Body::Failed(e.to_string()),
                };
                self.set_body(entry, body);
                self.scroll.set(0);
            }
            Asked::Batch => {
                let Some(Pending::Batch { id: p, entry }) = self.pending.clone() else {
                    return;
                };
                if p != id {
                    return;
                }
                self.pending = None;
                match reply {
                    Ok(Reply::Batch(outs)) if !outs.is_empty() => {
                        self.repeat_replies(entry, outs, act);
                    }
                    Ok(_) => self.failed(entry, SHAPE.into()),
                    Err(e) => self.failed(entry, e.to_string()),
                }
            }
        }
    }

    fn helper_off(&mut self, why: String) {
        if self.docs_off.is_none() {
            self.docs_off = Some(why.clone());
            self.note(why);
        }
    }

    /// Requests that will never be answered (the live connection closed, a rescan
    /// replaced it) are forgotten, and the command waiting on one says so; a repeat
    /// ends there, keeping its replies.
    fn forget_lost(&mut self, act: &Act<'_>) {
        // A write is the app's request, not in `asked`.
        if let Some(Pending::Write { id, entry }) = self.pending.clone()
            && !act.in_flight(id)
        {
            self.pending = None;
            self.failed(entry, NO_ANSWER.into());
        }
        let lost: Vec<ReqId> = self
            .asked
            .keys()
            .filter(|id| !act.in_flight(**id))
            .copied()
            .collect();
        for id in lost {
            let what = self.asked.remove(&id);
            if what == Some(Asked::Names) {
                self.names = Names::NotAsked;
            }
            let entry = match &self.pending {
                Some(
                    Pending::Info { id: p, entry, .. }
                    | Pending::Run { id: p, entry }
                    | Pending::Batch { id: p, entry }
                    | Pending::Write { id: p, entry },
                ) if *p == id => Some(*entry),
                _ => None,
            };
            if let Some(entry) = entry {
                self.pending = None;
                self.failed(entry, NO_ANSWER.into());
            }
        }
    }
}

/// What a command whose answer never comes says.
const NO_ANSWER: &str = "No answer: the live connection closed before the reply came.";
/// What a reply of the wrong kind says.
const SHAPE: &str = "the reply came back in an unexpected shape";

impl ViewState for CmdlineState {
    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        self.forget_lost(act);
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
        if let Some(Pending::Write { id: p, entry }) = self.pending.clone()
            && p == id
        {
            self.pending = None;
            if self.repeat_on(entry) {
                match result {
                    Ok(LiveReply::Write(crate::live::write::Reply::Ran(out))) => {
                        self.repeat_replies(entry, vec![out], act);
                    }
                    Ok(_) => self.failed(entry, SHAPE.into()),
                    Err(e) => self.failed(entry, e.to_string()),
                }
                return;
            }
            let body = match result {
                Ok(LiveReply::Write(crate::live::write::Reply::Ran(out))) => Body::Out(out),
                Ok(_) => Body::Failed(SHAPE.into()),
                Err(LiveError::Failed(s)) if s == CANCELLED => Body::Note(s),
                Err(e) => Body::Failed(e.to_string()),
            };
            self.set_body(entry, body);
            self.scroll.set(0);
            return;
        }
        self.answer(id, result, act);
    }
}

/// How `App` drives the panel. Here rather than in `app.rs`, which only calls these.
impl App {
    /// Why `:` does not open the panel now, if it does not.
    pub fn cmdline_off(&self) -> Option<String> {
        let Some(s) = &self.session else {
            return Some(
                "Connect first: the command line talks to the connected server \
                 (0 Connections, then enter)."
                    .into(),
            );
        };
        match &s.live {
            LiveState::Ready => None,
            LiveState::Waiting => Some(
                "The command line opens once the live connection is ready, after the connect."
                    .into(),
            ),
            LiveState::Closed(_) => Some(
                "The live connection is closed, so the command line is off until the next \
                 connect or analysis (r on Overview)."
                    .into(),
            ),
        }
    }

    /// `:`.
    pub(in crate::tui) fn open_cmdline(&mut self) -> Vec<Effect> {
        if let Some(why) = self.cmdline_off() {
            self.set_info(why);
            return Vec::new();
        }
        self.with_views(Owner::Cmdline, "open", |v, act| v.cmdline.open(act))
            .map_or_else(Vec::new, |(_, e)| e)
    }

    /// A key while the panel is open.
    pub(in crate::tui) fn cmdline_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if self.session.is_none() {
            self.views.cmdline.open = false;
            return Vec::new();
        }
        self.with_views(Owner::Cmdline, "key", |v, act| {
            v.cmdline.key(key, &v.keys.rows, act)
        })
        .map_or_else(Vec::new, |(_, e)| e)
    }

    /// A paste while the panel is open.
    pub(in crate::tui) fn cmdline_paste(&mut self, text: &str) -> Vec<Effect> {
        self.with_views(Owner::Cmdline, "paste", |v, act| v.cmdline.paste(text, act))
            .map_or_else(Vec::new, |(_, e)| e)
    }

    /// Every tick while the panel is open, whatever is on screen.
    pub(in crate::tui) fn cmdline_tick(&mut self, now: Instant) -> Vec<Effect> {
        if !self.views.cmdline.open {
            return Vec::new();
        }
        self.with_views(Owner::Cmdline, "tick", |v, act| v.cmdline.on_tick(now, act))
            .map_or_else(Vec::new, |(_, e)| e)
    }

    /// The panel's input line, while it is open.
    pub fn cmdline_input(&self) -> Option<Input> {
        self.views.cmdline.open.then(|| self.views.cmdline.input())
    }
}

/// How a repeat's last line is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Running,
    Done,
    Stopped,
    Failed,
}

/// A repeat's replies in `view`, each numbered `#n` with its lines under it, and
/// whether each line is an error's.
pub(crate) fn numbered(r: &Runs, view: View, pairs: bool) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for o in &r.outs {
        let tag = format!("#{} ", o.n);
        let pad = " ".repeat(tag.len());
        for (i, l) in o.out.lines_in(view, pairs).iter().enumerate() {
            let lead = if i == 0 { &tag } else { &pad };
            out.push((format!("{lead}{l}"), o.out.error));
        }
    }
    out
}

/// What a repeat that dropped its oldest replies says above the rest.
pub(crate) fn dropped_text(r: &Runs) -> String {
    format!(
        "(the first {} replies were dropped: the output keeps at most 4 MiB)",
        r.dropped
    )
}

/// A repeat's last line: how far it is, or how it ended, with the time its round
/// trips took (the waits for the budget not included).
pub(crate) fn runs_end(r: &Runs) -> (String, Tone) {
    let answered = r.answered();
    let times = r.times;
    match &r.end {
        None if r.stopping => (
            format!("stopping… {answered} of {times} answered; nothing more is sent"),
            Tone::Running,
        ),
        None => (
            format!("running… {answered} of {times} answered"),
            Tone::Running,
        ),
        Some(RunEnd::Done) => {
            let trips = r.outs.iter().filter(|o| o.batch_start).count();
            let total: Duration = r
                .outs
                .iter()
                .filter(|o| o.batch_start)
                .map(|o| o.out.took)
                .sum();
            let pipelined = r.outs.iter().any(|o| o.batch > 1);
            let how = match (trips, pipelined) {
                (1, true) => "in 1 pipeline".to_string(),
                (n, true) => format!("in {n} pipelines"),
                (1, false) => "in 1 round trip".to_string(),
                (n, false) => format!("in {n} round trips"),
            };
            let kept = if r.dropped > 0 {
                " (the replies kept)"
            } else {
                ""
            };
            (
                format!("({answered} replies {how}{kept} · {})", took_text(total)),
                Tone::Done,
            )
        }
        Some(RunEnd::Stopped) => (
            format!("■ Stopped with esc: {answered} of {times} answered, nothing more sent."),
            Tone::Stopped,
        ),
        Some(RunEnd::Failed(why)) => (
            format!("✗ {why} ({answered} of {times} answered)"),
            Tone::Failed,
        ),
    }
}

/// The time a command took, as `0.42 ms`, `12 ms` or `1.30 s`.
pub fn took_text(d: Duration) -> String {
    let us = d.as_micros();
    if us < 10_000 {
        format!("{:.2} ms", us as f64 / 1000.0)
    } else if us < 1_000_000 {
        format!("{} ms", us / 1000)
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

/// An entry of the output as plain lines, in `view`, as the panel draws them but
/// without styles: what a repeat's numbered replies and last line say. Values: for
/// tests only, never logged.
pub(crate) fn entry_text(e: &Entry, view: View) -> Vec<String> {
    match &e.body {
        Body::Waiting => vec!["running…".into()],
        Body::Asking => vec!["Waiting for your answer below.".into()],
        Body::Note(why) => vec![why.clone()],
        Body::Failed(why) => vec![format!("(error) {why}")],
        Body::Out(o) => o.lines_in(view, e.pairs).to_vec(),
        Body::Runs(r) => {
            let mut out = Vec::new();
            if r.dropped > 0 {
                out.push(dropped_text(r));
            }
            out.extend(numbered(r, view, e.pairs).into_iter().map(|(l, _)| l));
            out.push(runs_end(r).0);
            out
        }
    }
}

/// One step of [`run_cmdline_for_tests`].
#[doc(hidden)]
pub enum CmdlineStep {
    /// Types the line and presses `enter`.
    Line(String),
    /// Presses the key: `ctrl-p`, `ctrl-t`, `y` for a question …
    Key(KeyEvent),
    /// Replaces the `[settings]` from here on (`skip_non_prod_confirm` …).
    Settings(crate::write::Settings),
}

/// What [`run_cmdline_for_tests`] saw. Holds replies: for tests only, so no `Debug`.
#[doc(hidden)]
#[derive(Default)]
pub struct CmdlineForTests {
    /// Each entry of the output: its masked command line, and its lines in the text
    /// view ([`entry_text`]).
    pub entries: Vec<(Option<String>, Vec<String>)>,
    /// What the app asked of the live connection, as `Debug` text (which holds no
    /// value: argument counts and command names only), in order.
    pub requests: Vec<String>,
}

/// The command line runtime, for the integration tests (`tests/cmdline_repeat_it.rs`):
/// connects `profile` without an analysis exactly as the TUI does, opens the panel
/// with `:`, and carries out `steps`, each time carrying out what the app asks of the
/// live connection until the panel is idle again or a question waits for its answer.
/// The live connection is closed again at the end.
#[doc(hidden)]
pub async fn run_cmdline_for_tests(
    profile: crate::model::Profile,
    credentials: Option<crate::model::Credentials>,
    steps: Vec<CmdlineStep>,
) -> CmdlineForTests {
    use std::sync::{Arc, Mutex};

    use tokio::sync::mpsc;

    use crate::tui::app::SecretSource;
    use crate::tui::{Incoming, LiveSession, LiveSlot, Run, connect_and_scan, lock};

    fn carry_out(effects: Vec<Effect>, slot: &LiveSlot, requests: &mut Vec<String>) {
        for e in effects {
            if let Effect::Live {
                req_id, to, req, ..
            } = e
            {
                requests.push(format!("{req:?}"));
                let guard = lock(slot);
                let handle = guard.as_ref().and_then(|s| s.handle.as_ref());
                assert!(
                    handle.is_some_and(|h| h.send_to(req_id, to, req)),
                    "the live connection is open"
                );
            }
        }
    }

    /// Applies the next message; false once there are none.
    async fn pump(
        app: &mut App,
        rx: &mut mpsc::UnboundedReceiver<Incoming>,
        slot: &LiveSlot,
        requests: &mut Vec<String>,
    ) -> bool {
        let incoming = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("the app gets an answer in time");
        let Some(Incoming::Msg(m)) = incoming else {
            return false;
        };
        let effects = app.apply(*m);
        carry_out(effects, slot, requests);
        true
    }

    /// The panel waits for a reply, and no question waits for the user.
    fn busy(app: &App) -> bool {
        let c = &app.views.cmdline;
        let asking = app.confirm.is_some() || c.confirm.is_some();
        !asking && (c.running() || !c.asked.is_empty())
    }

    let press = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
    let mut app = App::new(vec![profile.clone()]);
    let effects = app.handle_key(press(KeyCode::Enter));
    let (id, scan) = effects
        .iter()
        .find_map(|e| match e {
            Effect::Connect { id, scan, .. } => Some((*id, *scan)),
            _ => None,
        })
        .expect("enter connects");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let slot: LiveSlot = Arc::new(Mutex::new(Some(LiveSession::new(id, scan))));
    connect_and_scan(
        Run { id, scan },
        profile,
        SecretSource::Container(credentials),
        None,
        crate::scan::StopFlag::new(),
        tx,
        Arc::clone(&slot),
    )
    .await;
    let mut requests = Vec::new();
    while app.cmdline_off().is_some() {
        assert!(
            pump(&mut app, &mut rx, &slot, &mut requests).await,
            "the connect ended before the live connection was ready"
        );
    }
    let effects = app.handle_key(press(KeyCode::Char(':')));
    carry_out(effects, &slot, &mut requests);
    assert!(app.views.cmdline.open, "the panel opens");
    while busy(&app) {
        assert!(pump(&mut app, &mut rx, &slot, &mut requests).await);
    }
    for step in steps {
        match step {
            CmdlineStep::Line(line) => {
                for c in line.chars() {
                    let effects = app.handle_key(press(KeyCode::Char(c)));
                    carry_out(effects, &slot, &mut requests);
                }
                let effects = app.handle_key(press(KeyCode::Enter));
                carry_out(effects, &slot, &mut requests);
            }
            CmdlineStep::Key(k) => {
                let effects = app.handle_key(k);
                carry_out(effects, &slot, &mut requests);
            }
            CmdlineStep::Settings(settings) => app.settings = settings,
        }
        while busy(&app) {
            assert!(pump(&mut app, &mut rx, &slot, &mut requests).await);
        }
    }
    let entries = app
        .views
        .cmdline
        .entries
        .iter()
        .map(|e| (e.line.clone(), entry_text(e, View::Text)))
        .collect();
    drop(lock(&slot).take());
    CmdlineForTests { entries, requests }
}

#[cfg(test)]
mod tests {

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use redis::Value;

    use super::*;
    use crate::live::cmdline::{CommandInfo, Docs, Output, Reply, Request};
    use crate::model::{Env, KeyType, Ttl};
    use crate::tui::app::views::testing::{self, key};
    use crate::tui::app::{App, Effect, Msg, Screen};

    const MARK: &str = "hunter2-mark";

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
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

    fn notice(app: &App) -> String {
        app.notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    /// The command line's requests among `effects`.
    fn reqs(effects: &[Effect]) -> Vec<(ReqId, Request)> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(id, r)| match r {
                LiveRequest::Command(c) => Some((id, c)),
                _ => None,
            })
            .collect()
    }

    /// Scanned, live connection ready, on Patterns, the panel open. The `COMMAND LIST`
    /// it asked is answered with `names`.
    fn opened_with(names: &[&str]) -> App {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        assert_eq!(app.screen(), Screen::Patterns);
        let effects = app.handle_key(ch(':'));
        assert!(app.views.cmdline.open);
        let r = reqs(&effects);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].1, Request::Names);
        testing::reply(
            &mut app,
            r[0].0,
            Ok(LiveReply::Command(Reply::Names(
                names.iter().map(|s| (*s).to_string()).collect(),
            ))),
        );
        app
    }

    fn opened() -> App {
        opened_with(&["get", "getdel", "keys", "set"])
    }

    fn type_text(app: &mut App, s: &str) -> Vec<Effect> {
        let mut out = Vec::new();
        for c in s.chars() {
            out.extend(app.handle_key(ch(c)));
        }
        out
    }

    fn info(name: &str, flags: &[&str], cats: &[&str]) -> CommandInfo {
        CommandInfo {
            name: name.into(),
            flags: flags.iter().map(|s| (*s).into()).collect(),
            categories: cats.iter().map(|s| (*s).into()).collect(),
            subcommands: Vec::new(),
            keys: Vec::new(),
            keys_partial: false,
        }
    }

    fn get_info() -> CommandInfo {
        info("get", &["readonly", "fast"], &["@read", "@string", "@fast"])
    }

    fn keys_info() -> CommandInfo {
        info(
            "keys",
            &["readonly"],
            &["@keyspace", "@read", "@slow", "@dangerous"],
        )
    }

    fn reply(app: &mut App, id: ReqId, r: Reply) -> Vec<Effect> {
        testing::reply(app, id, Ok(LiveReply::Command(r)))
    }

    /// Types `line`, presses enter, and answers its `COMMAND INFO` with `info` (and its
    /// `COMMAND DOCS` with nothing). Returns the requests that followed.
    fn enter_with(app: &mut App, line: &str, info: Option<CommandInfo>) -> Vec<(ReqId, Request)> {
        let mut effects = type_text(app, line);
        effects.extend(app.handle_key(key(KeyCode::Enter)));
        let mut after = Vec::new();
        for (id, r) in reqs(&effects) {
            match r {
                Request::Info { .. } => after.extend(reply(app, id, Reply::Info(info.clone()))),
                Request::Docs { .. } => after.extend(reply(app, id, Reply::Docs(None))),
                other => panic!("unexpected {other:?}"),
            }
        }
        reqs(&after)
    }

    fn ran(lines: Value) -> Reply {
        Reply::Ran(Output::of(&lines, Duration::from_micros(420)))
    }

    #[test]
    fn colon_opens_the_panel_over_the_view_and_esc_closes_it() {
        let mut app = opened();
        let all = text(&app, 80, 24);
        assert!(all.contains("Command line"), "{all}");
        assert!(
            all.contains("enter run · tab complete · esc close"),
            "{all}"
        );
        // Every key is text while it is open: digits, q and ? included.
        type_text(&mut app, "1q?a");
        assert_eq!(app.screen(), Screen::Patterns);
        assert!(!app.help && app.add.is_none());
        assert_eq!(app.views.cmdline.shown_input(), "1q?a");
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.views.cmdline.open);
        assert!(!text(&app, 80, 24).contains("Command line"));
        // Opened again, the line is still there; no second COMMAND LIST.
        let effects = app.handle_key(ch(':'));
        assert!(reqs(&effects).is_empty());
        assert_eq!(app.views.cmdline.shown_input(), "1q?a");
    }

    #[test]
    fn colon_is_text_in_the_add_flow_the_filter_and_the_help_overlay() {
        let mut app = testing::scanned();
        testing::live_ready(&mut app);
        app.handle_key(ch('/'));
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open);
        assert_eq!(app.patterns.filter, ":");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ch('?'));
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open && app.help);
        // esc clears what is typed into the help, then closes it; `a` is on Connections.
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ch('0'));
        app.handle_key(ch('a'));
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open);
        assert_eq!(app.add.as_ref().unwrap().input, ":");
    }

    #[test]
    fn off_before_the_live_connection_is_ready() {
        let mut app = App::new(crate::tui::fixtures::profiles());
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open);
        assert!(notice(&app).contains("Connect first"), "{}", notice(&app));
        let mut app = testing::scanned();
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open);
        assert!(notice(&app).contains("once the live connection is ready"));
    }

    #[test]
    fn a_readonly_get_is_sent_and_its_reply_drawn_with_the_time() {
        let mut app = opened();
        let effects = type_text(&mut app, "GET user:1");
        // The flags and the docs are read once the name is typed out.
        let r = reqs(&effects);
        assert_eq!(
            r.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
            [
                Request::Info { name: "get".into() },
                Request::Docs { name: "get".into() }
            ]
        );
        // Enter waits for the flags already asked; it asks nothing more.
        assert!(reqs(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert!(app.views.cmdline.running());
        let run = reqs(&reply(&mut app, r[0].0, Reply::Info(Some(get_info()))));
        let [
            (
                id,
                Request::Run {
                    args, confirmed, ..
                },
            ),
        ] = &run[..]
        else {
            panic!("{run:?}");
        };
        assert_eq!(args.as_slice(), [b"GET".to_vec(), b"user:1".to_vec()]);
        assert!(!confirmed);
        reply(&mut app, *id, ran(Value::BulkString(b"hello".to_vec())));
        assert!(!app.views.cmdline.running());
        let all = text(&app, 80, 24);
        assert!(all.contains("› GET user:1"), "{all}");
        assert!(all.contains("\"hello\""), "{all}");
        assert!(all.contains("(0.42 ms)"), "{all}");
        // The flags are kept: the second GET goes out at once.
        let mut effects = type_text(&mut app, "get b");
        effects.extend(app.handle_key(key(KeyCode::Enter)));
        let r = reqs(&effects);
        assert!(matches!(&r[..], [(_, Request::Run { .. })]), "{r:?}");
    }

    #[test]
    fn a_write_is_refused_without_being_sent() {
        let mut app = opened();
        let set = info("set", &["write", "denyoom"], &["@write", "@string"]);
        let after = enter_all(&mut app, "SET k v", Some(set));
        assert!(writes(&after).is_empty(), "{after:?}");
        let all = text(&app, 120, 36);
        assert!(all.contains("prod-cache-01 is read-only"), "{all}");
        assert!(app.confirm.is_none());
    }

    /// Like [`enter_with`], but every effect that followed, writes included.
    fn enter_all(app: &mut App, line: &str, info: Option<CommandInfo>) -> Vec<Effect> {
        let mut effects = type_text(app, line);
        effects.extend(app.handle_key(key(KeyCode::Enter)));
        let mut after = Vec::new();
        for (id, r) in reqs(&effects) {
            match r {
                Request::Info { .. } => after.extend(reply(app, id, Reply::Info(info.clone()))),
                Request::Docs { .. } => after.extend(reply(app, id, Reply::Docs(None))),
                other => panic!("unexpected {other:?}"),
            }
        }
        after
    }

    fn writes(effects: &[Effect]) -> Vec<(ReqId, WriteOp)> {
        testing::requests(effects)
            .into_iter()
            .filter_map(|(id, r)| match r {
                LiveRequest::Write(p) => Some((id, p.op().clone())),
                _ => None,
            })
            .collect()
    }

    fn writable(app: &mut App, env: Env) {
        let s = app.session.as_mut().unwrap();
        s.profile.env = env;
        s.profile.read_only = Some(false);
    }

    fn set_info() -> CommandInfo {
        info(
            "set",
            &["write", "denyoom"],
            &["@write", "@string", "@slow"],
        )
    }

    fn del_info() -> CommandInfo {
        info("del", &["write"], &["@keyspace", "@write", "@slow"])
    }

    fn flushdb_info() -> CommandInfo {
        info(
            "flushdb",
            &["write"],
            &["@keyspace", "@write", "@slow", "@dangerous"],
        )
    }

    fn config_info() -> CommandInfo {
        let mut c = info("config", &[], &["@slow"]);
        c.subcommands = vec![info(
            "config|set",
            &["admin", "noscript", "loading", "stale"],
            &["@admin", "@slow", "@dangerous"],
        )];
        c
    }

    fn eval_info() -> CommandInfo {
        info(
            "eval",
            &["noscript", "stale", "skip_monitor"],
            &["@slow", "@scripting"],
        )
    }

    #[test]
    fn on_a_read_only_profile_no_write_reaches_the_connection() {
        for (line, i) in [
            ("SET k v", set_info()),
            ("DEL k", del_info()),
            ("FLUSHDB", flushdb_info()),
            ("CONFIG SET maxmemory-policy allkeys-lfu", config_info()),
            ("EVAL \"return 1\" 0", eval_info()),
        ] {
            for ro in [(Env::Prod, None), (Env::Local, Some(true))] {
                let mut app = opened();
                let s = app.session.as_mut().unwrap();
                s.profile.env = ro.0;
                s.profile.read_only = ro.1;
                let mut e = enter_all(&mut app, line, Some(i.clone()));
                for k in ["y", "prod-cache-01"] {
                    e.extend(type_text(&mut app, k));
                    e.extend(app.handle_key(key(KeyCode::Enter)));
                }
                assert!(writes(&e).is_empty(), "{line}: {e:?}");
                assert!(app.confirm.is_none(), "{line}");
            }
        }
    }

    #[test]
    fn a_write_goes_through_the_write_gate() {
        let mut app = opened();
        writable(&mut app, Env::Local);
        // An edit runs at once outside prod.
        let e = enter_all(&mut app, "SET k v", Some(set_info()));
        let w = writes(&e);
        assert_eq!(w.len(), 1, "{e:?}");
        let WriteOp::Command { args, .. } = &w[0].1 else {
            panic!("{w:?}")
        };
        assert_eq!(
            args.as_slice(),
            [b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]
        );
        // Its reply is drawn like a readonly command's.
        testing::reply(
            &mut app,
            w[0].0,
            Ok(LiveReply::Write(crate::live::write::Reply::Ran(
                Output::of(&redis::Value::Okay, std::time::Duration::from_millis(3)),
            ))),
        );
        assert!(text(&app, 120, 36).contains("OK"));

        // A delete asks y/n; n draws the cancel.
        let e = enter_all(&mut app, "DEL k", Some(del_info()));
        assert!(writes(&e).is_empty());
        let lines = app.confirm.as_ref().expect("a delete asks").lines();
        assert!(lines.iter().any(|l| l == "Sends: DEL k"), "{lines:?}");
        app.handle_key(ch('n'));
        assert!(text(&app, 120, 36).contains(CANCELLED));
        // FLUSHDB (dangerous) asks y/n here, and y sends it.
        enter_all(&mut app, "FLUSHDB", Some(flushdb_info()));
        assert!(app.confirm.is_some());
        assert_eq!(writes(&app.handle_key(ch('y'))).len(), 1);
    }

    #[test]
    fn on_prod_a_dangerous_write_needs_the_profile_name() {
        let mut app = opened();
        writable(&mut app, Env::Prod);
        enter_all(&mut app, "FLUSHDB", Some(flushdb_info()));
        assert!(writes(&app.handle_key(ch('y'))).is_empty());
        app.handle_key(key(KeyCode::Backspace));
        type_text(&mut app, "prod-cache-01");
        assert_eq!(writes(&app.handle_key(key(KeyCode::Enter))).len(), 1);
    }

    #[test]
    fn scripts_stay_refused_with_writes_on() {
        let mut app = opened();
        writable(&mut app, Env::Local);
        let e = enter_all(&mut app, "EVAL \"return 1\" 0", Some(eval_info()));
        assert!(writes(&e).is_empty());
        assert!(app.confirm.is_none());
        assert!(text(&app, 120, 36).contains("runs a script"));
    }

    #[test]
    fn a_masked_write_is_masked_in_the_question_and_the_history() {
        let mut app = opened();
        writable(&mut app, Env::Local);
        let line = format!("CONFIG SET requirepass {MARK}");
        enter_all(&mut app, &line, Some(config_info()));
        let c = app.confirm.as_ref().expect("CONFIG SET is dangerous: asks");
        assert!(
            c.lines()
                .iter()
                .any(|l| l == "Sends: CONFIG SET requirepass ***"),
            "{:?}",
            c.lines()
        );
        let all = text(&app, 120, 36);
        assert!(!value::leaks(&all, MARK), "{all}");
        assert!(!format!("{:?}", app.confirm).contains(MARK));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.views.cmdline.history(), ["CONFIG SET requirepass ***"]);
    }

    #[test]
    fn refused_and_unknown_commands_say_why() {
        let mut app = opened();
        let mut effects = type_text(&mut app, "SUBSCRIBE news");
        effects.extend(app.handle_key(key(KeyCode::Enter)));
        // Not even its flags are read.
        assert!(reqs(&effects).is_empty(), "{:?}", reqs(&effects));
        assert!(text(&app, 120, 36).contains("use Pub/Sub (5)"));
        assert!(enter_with(&mut app, "MONITOR", None).is_empty());
        assert!(text(&app, 120, 36).contains("Monitor panel"));
        assert!(enter_with(&mut app, "NOSUCH x", None).is_empty());
        assert!(text(&app, 120, 36).contains("the server has no command NOSUCH"));
        // A line that does not split.
        type_text(&mut app, "GET \"open");
        app.handle_key(key(KeyCode::Enter));
        assert!(text(&app, 120, 36).contains("A quote is not closed."));
    }

    #[test]
    fn a_dangerous_readonly_command_asks_first() {
        // prod: the profile name, typed exactly.
        let mut app = opened();
        let after = enter_with(&mut app, "KEYS user:*", Some(keys_info()));
        assert!(after.is_empty());
        let input = app.cmdline_input().unwrap();
        assert_eq!(input.label, "confirm");
        let all = text(&app, 120, 36);
        assert!(
            all.contains("KEYS is @dangerous and asks first: KEYS user:*"),
            "{all}"
        );
        assert!(all.contains("Type prod-cache-01 and press enter"), "{all}");
        insta::assert_snapshot!(
            "confirm_prod_80x24",
            testing::render(&app, 80, 24).backend()
        );
        // y is just a letter here, and a wrong name sends nothing.
        type_text(&mut app, "y");
        assert!(reqs(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert!(text(&app, 120, 36).contains("That is not the profile name"));
        type_text(&mut app, "prod-cache-0");
        assert!(reqs(&app.handle_key(key(KeyCode::Enter))).is_empty());
        type_text(&mut app, "prod-cache-01");
        let r = reqs(&app.handle_key(key(KeyCode::Enter)));
        let [(_, Request::Run { confirmed, .. })] = &r[..] else {
            panic!("{r:?}");
        };
        assert!(confirmed);

        // Not prod: y or n.
        let mut app = opened();
        app.session.as_mut().unwrap().profile.env = Env::Stage;
        enter_with(&mut app, "KEYS *", Some(keys_info()));
        assert_eq!(app.cmdline_input().unwrap().label, "y/n");
        assert!(text(&app, 120, 36).contains("Send it to prod-cache-01? y to send"));
        assert!(reqs(&app.handle_key(ch('x'))).is_empty());
        assert!(reqs(&app.handle_key(ch('n'))).is_empty());
        assert!(text(&app, 120, 36).contains("Not sent: cancelled."));
        type_text(&mut app, "KEYS *");
        app.handle_key(key(KeyCode::Enter));
        let r = reqs(&app.handle_key(ch('y')));
        assert!(
            matches!(
                &r[..],
                [(
                    _,
                    Request::Run {
                        confirmed: true,
                        ..
                    }
                )]
            ),
            "{r:?}"
        );
        // esc cancels a question without closing the panel.
        let mut app = opened();
        enter_with(&mut app, "KEYS *", Some(keys_info()));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.views.cmdline.open && app.views.cmdline.confirm.is_none());
    }

    #[test]
    fn credentials_are_masked_on_screen_in_the_history_and_in_debug() {
        let mut app = opened();
        let typed = format!("ACL SETUSER u >{MARK}");
        let mut effects = Vec::new();
        // Masked from the first character of the password on.
        let before = "ACL SETUSER u >".len();
        for (i, c) in typed.chars().enumerate() {
            effects.extend(app.handle_key(ch(c)));
            if i >= before {
                assert!(app.views.cmdline.shown_input().ends_with(">***"));
                let part = &MARK[..=(i - before)];
                if part.len() >= 3 {
                    let all = text(&app, 120, 36);
                    assert!(!all.contains(part), "{part}: {all}");
                }
            }
        }
        let shown = text(&app, 80, 24);
        assert!(
            !value::leaks(&shown, MARK) && shown.contains(">***"),
            "{shown}"
        );
        effects.extend(app.handle_key(key(KeyCode::Enter)));
        let mut acl = info("acl", &[], &["@slow"]);
        acl.subcommands = vec![info(
            "acl|setuser",
            &["admin", "noscript"],
            &["@admin", "@dangerous"],
        )];
        for (id, r) in reqs(&effects) {
            if matches!(r, Request::Info { .. }) {
                effects.extend(reply(&mut app, id, Reply::Info(Some(acl.clone()))));
            }
        }
        assert_eq!(app.views.cmdline.history(), ["ACL SETUSER u >***"]);
        let all = text(&app, 120, 36);
        // A write, refused on this read-only profile.
        assert!(all.contains("prod-cache-01 is read-only"), "{all}");
        assert!(!value::leaks(&all, MARK), "{all}");
        // ↑ brings back the masked line, not the password.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.views.cmdline.shown_input(), "ACL SETUSER u >***");
        app.handle_key(key(KeyCode::Down));
        // CONFIG SET requirepass, while it is typed.
        type_text(&mut app, &format!("CONFIG SET requirepass {MARK}"));
        assert_eq!(
            app.views.cmdline.shown_input(),
            "CONFIG SET requirepass ***"
        );
        assert!(!value::leaks(&text(&app, 80, 24), MARK));
        let debug = format!("{effects:?} {:?} {:?}", app.views, app.session);
        assert!(!value::leaks(&debug, MARK), "{debug}");
        assert!(!value::leaks(&notice(&app), MARK));
    }

    #[test]
    fn replies_never_reach_debug_or_snapshots() {
        let mut app = opened();
        let run = enter_with(&mut app, "GET k", Some(get_info()));
        let [(id, _)] = &run[..] else {
            panic!("{run:?}")
        };
        let result = Ok(LiveReply::Command(ran(Value::Array(vec![
            Value::BulkString(MARK.as_bytes().to_vec()),
            Value::Map(vec![(
                Value::Int(1),
                Value::BulkString(MARK.as_bytes().to_vec()),
            )]),
        ]))));
        let s = app.session.as_ref().unwrap();
        let msg = format!(
            "{:?}",
            Msg::Live {
                id: s.id,
                scan: s.scan,
                req_id: *id,
                result: result.clone(),
            }
        );
        assert!(!value::leaks(&msg, MARK), "{msg}");
        testing::reply(&mut app, *id, result);
        assert!(text(&app, 120, 36).contains(MARK));
        let debug = format!("{:?} {:?}", app.views, app.session);
        assert!(!value::leaks(&debug, MARK), "{debug}");
        app.handle_key(key(KeyCode::Esc));
        let effects = app.handle_key(ch('s'));
        let [Effect::SaveSnapshot(report)] = &effects[..] else {
            panic!("{effects:?}");
        };
        assert!(!value::leaks(&serde_json::to_string(report).unwrap(), MARK));
    }

    #[test]
    fn noperm_gives_the_fixed_text() {
        let mut app = opened();
        let run = enter_with(&mut app, "GET k", Some(get_info()));
        testing::reply(&mut app, run[0].0, Err(LiveError::Denied("GET".into())));
        let all = text(&app, 120, 36);
        assert!(
            all.contains("(error) the server refused GET for this user"),
            "{all}"
        );
    }

    #[test]
    fn the_helper_shows_the_docs_and_says_once_when_it_is_off() {
        let mut app = opened();
        let effects = type_text(&mut app, "GET ");
        let r = reqs(&effects);
        reply(&mut app, r[0].0, Reply::Info(Some(get_info())));
        reply(
            &mut app,
            r[1].0,
            Reply::Docs(Some(Docs {
                summary: "Returns the string value of a key.".into(),
                since: "1.0.0".into(),
                complexity: "O(1)".into(),
                arguments: "key".into(),
                subcommands: Vec::new(),
            })),
        );
        let all = text(&app, 120, 36);
        assert!(all.contains("GET key · O(1)"), "{all}");
        assert!(
            all.contains("Returns the string value of a key. · readonly · @read @string @fast"),
            "{all}"
        );
        insta::assert_snapshot!("helper_120x36", testing::render(&app, 120, 36).backend());
        // An older server: off, said once, and not asked again.
        app.handle_key(ctrl('u'));
        let r = reqs(&type_text(&mut app, "KEYS "));
        let docs = r
            .iter()
            .find(|(_, r)| matches!(r, Request::Docs { .. }))
            .unwrap();
        testing::reply(
            &mut app,
            docs.0,
            Err(LiveError::Unsupported("COMMAND DOCS".into())),
        );
        app.handle_key(ctrl('u'));
        let r = reqs(&type_text(&mut app, "SET "));
        assert!(
            r.iter().all(|(_, r)| !matches!(r, Request::Docs { .. })),
            "{r:?}"
        );
        let all = text(&app, 120, 36);
        assert_eq!(all.matches("The helper is off").count(), 1, "{all}");
    }

    #[test]
    fn tab_completes_names_subcommands_and_loaded_keys() {
        let mut app = opened();
        type_text(&mut app, "GE");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "GET");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "GETDEL");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "GET");
        app.handle_key(ctrl('u'));
        type_text(&mut app, "ke");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "keys");
        // Keys the key list loaded, quoted when they need it.
        app.handle_key(ctrl('u'));
        for k in ["user:1", "user:2 x", "other"] {
            app.views.keys.rows.push(crate::live::keys::KeyRow {
                key: k.into(),
                key_type: KeyType::String,
                ttl: Ttl::Persistent,
                bytes: None,
                encoding: None,
                length: None,
                freq: None,
            });
        }
        type_text(&mut app, "GET us");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "GET user:1");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "GET \"user:2 x\"");
        // Subcommands, from COMMAND INFO.
        app.handle_key(ctrl('u'));
        let r = reqs(&type_text(&mut app, "CONFIG "));
        let mut config = info("config", &[], &["@slow"]);
        config.subcommands = vec![
            info("config|get", &["admin"], &[]),
            info("config|set", &["admin"], &[]),
        ];
        reply(&mut app, r[0].0, Reply::Info(Some(config)));
        type_text(&mut app, "g");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "CONFIG get");
    }

    #[test]
    fn history_steps_and_ctrl_l_clears_the_output() {
        let mut app = opened();
        let run = enter_with(&mut app, "GET a", Some(get_info()));
        // One command at a time: enter waits, keeping the line.
        type_text(&mut app, "GET b");
        assert!(reqs(&app.handle_key(key(KeyCode::Enter))).is_empty());
        assert_eq!(app.views.cmdline.shown_input(), "GET b");
        assert!(text(&app, 120, 36).contains("A command is still running"));
        reply(&mut app, run[0].0, ran(Value::Nil));
        let r = reqs(&app.handle_key(key(KeyCode::Enter)));
        assert!(matches!(&r[..], [(_, Request::Run { .. })]), "{r:?}");
        type_text(&mut app, "draft");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.views.cmdline.shown_input(), "GET b");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.views.cmdline.shown_input(), "GET a");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.views.cmdline.shown_input(), "GET a");
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.views.cmdline.shown_input(), "draft");
        assert!(!app.views.cmdline.entries.is_empty());
        app.handle_key(ctrl('l'));
        assert!(app.views.cmdline.entries.is_empty());
        assert_eq!(app.views.cmdline.history().len(), 2);
    }

    #[test]
    fn a_reply_that_never_comes_is_reported() {
        let mut app = opened();
        let run = enter_with(&mut app, "GET k", Some(get_info()));
        assert_eq!(run.len(), 1);
        let s = app.session.as_ref().unwrap();
        let (id, scan) = (s.id, s.scan);
        app.apply(Msg::LiveClosed {
            id,
            scan,
            error: None,
        });
        app.apply(Msg::TickAt(std::time::Instant::now()));
        assert!(!app.views.cmdline.running());
        assert!(text(&app, 120, 36).contains("No answer: the live connection closed"));
    }

    #[test]
    fn panel_over_patterns_80x24_and_120x36() {
        let mut app = opened();
        let run = enter_with(&mut app, "GET user:1", Some(get_info()));
        reply(
            &mut app,
            run[0].0,
            ran(Value::BulkString(b"hello\nworld".to_vec())),
        );
        let run = enter_with(
            &mut app,
            "XREAD STREAMS s 0",
            Some(info("xread", &["readonly", "blocking"], &["@read"])),
        );
        let nested = Value::Array(vec![Value::Array(vec![
            Value::BulkString(b"s".to_vec()),
            Value::Array(vec![Value::Array(vec![
                Value::BulkString(b"1-0".to_vec()),
                Value::Array(vec![
                    Value::BulkString(b"f".to_vec()),
                    Value::BulkString(b"v".to_vec()),
                ]),
            ])]),
        ])]);
        reply(&mut app, run[0].0, ran(nested));
        let r = reqs(&type_text(&mut app, "TYPE user:1"));
        reply(
            &mut app,
            r[0].0,
            Reply::Info(Some(info(
                "type",
                &["readonly", "fast"],
                &["@keyspace", "@read", "@fast"],
            ))),
        );
        reply(
            &mut app,
            r[1].0,
            Reply::Docs(Some(Docs {
                summary: "Determines the type of value stored at a key.".into(),
                complexity: "O(1)".into(),
                arguments: "key".into(),
                ..Docs::default()
            })),
        );
        insta::assert_snapshot!("panel_80x24", testing::render(&app, 80, 24).backend());
        insta::assert_snapshot!("panel_120x36", testing::render(&app, 120, 36).backend());
        // PgUp scrolls the output back, and it stops at the top.
        for _ in 0..20 {
            app.handle_key(key(KeyCode::PageUp));
        }
        let all = text(&app, 80, 24);
        assert!(all.contains("› GET user:1"), "{all}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut app = opened();
        let run = enter_with(&mut app, "GET k", Some(get_info()));
        reply(&mut app, run[0].0, ran(Value::BulkString(vec![b'a'; 5000])));
        for (w, h) in [(1, 1), (0, 0), (20, 5), (40, 12), (200, 2), (3, 30)] {
            testing::render(&app, w, h);
        }
    }

    // -----------------------------------------------------------------------------
    // Result views, `N cmd` repeats and pipeline mode.
    // -----------------------------------------------------------------------------

    fn incr_info() -> CommandInfo {
        info(
            "incr",
            &["write", "denyoom", "fast"],
            &["@write", "@string", "@fast"],
        )
    }

    /// Types `line` and presses enter, for a command whose flags are read already:
    /// every effect, the command's own included.
    fn enter_cached(app: &mut App, line: &str) -> Vec<Effect> {
        let mut effects = type_text(app, line);
        effects.extend(app.handle_key(key(KeyCode::Enter)));
        effects
    }

    fn hgetall_info() -> CommandInfo {
        info("hgetall", &["readonly"], &["@read", "@hash", "@slow"])
    }

    fn wrote(app: &mut App, id: ReqId, v: Value) -> Vec<Effect> {
        testing::reply(
            app,
            id,
            Ok(LiveReply::Write(crate::live::write::Reply::Ran(
                Output::of(&v, Duration::from_millis(1)),
            ))),
        )
    }

    fn last_body(app: &App) -> Body {
        app.views.cmdline.entries.last().unwrap().body.clone()
    }

    #[test]
    fn a_readonly_repeat_goes_one_at_a_time_with_numbered_replies() {
        let mut app = opened();
        // The flags read are the command's, not the count's.
        let r = reqs(&type_text(&mut app, "3 GET k"));
        assert_eq!(
            r.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
            [
                Request::Info { name: "get".into() },
                Request::Docs { name: "get".into() }
            ]
        );
        assert!(reqs(&app.handle_key(key(KeyCode::Enter))).is_empty());
        let mut next = reqs(&reply(&mut app, r[0].0, Reply::Info(Some(get_info()))));
        for i in 1..=3 {
            // One on its way at a time: the next goes when this one is answered.
            let [
                (
                    id,
                    Request::Run {
                        args, confirmed, ..
                    },
                ),
            ] = &next[..]
            else {
                panic!("{i}: {next:?}");
            };
            assert_eq!(args.as_slice(), [b"GET".to_vec(), b"k".to_vec()]);
            assert!(!confirmed);
            assert!(app.views.cmdline.running() && app.views.cmdline.repeating());
            next = reqs(&reply(&mut app, *id, ran(Value::Int(i))));
        }
        assert!(next.is_empty(), "{next:?}");
        assert!(!app.views.cmdline.running() && !app.views.cmdline.repeating());
        let all = text(&app, 80, 24);
        assert!(all.contains("› 3 GET k"), "{all}");
        for i in 1..=3 {
            assert!(all.contains(&format!("#{i} (integer) {i}")), "{all}");
        }
        assert!(
            all.contains("(3 replies in 3 round trips · 1.26 ms)"),
            "{all}"
        );
        assert_eq!(app.views.cmdline.history(), ["3 GET k"]);
        insta::assert_snapshot!("repeat_80x24", testing::render(&app, 80, 24).backend());
    }

    #[test]
    fn a_bad_count_is_refused_before_anything_is_sent() {
        for (line, want) in [
            ("101 GET k", "at most 100 repeats a line; 101 is more."),
            ("100000000000000000000 GET k", "at most 100"),
            ("0 GET k", "a repeat count of 0 sends nothing."),
            ("-2 GET k", "a whole number from 1 to 100"),
        ] {
            let mut app = opened();
            let mut e = type_text(&mut app, line);
            // The helper says why while it is typed.
            assert!(
                app.views.cmdline.helper()[0].0.contains(want),
                "{line}: {:?}",
                app.views.cmdline.helper()
            );
            e.extend(app.handle_key(key(KeyCode::Enter)));
            // Not even the command's flags.
            assert!(reqs(&e).is_empty(), "{line}: {:?}", reqs(&e));
            let Body::Note(why) = last_body(&app) else {
                panic!("{line}")
            };
            assert!(why.contains(want) && !why.contains('\n'), "{line}: {why}");
            assert!(!app.views.cmdline.running());
        }
        // 100 is the most.
        let mut app = opened();
        let next = enter_with(&mut app, "100 GET k", Some(get_info()));
        assert_eq!(next.len(), 1, "{next:?}");
    }

    #[test]
    fn a_dangerous_repeat_asks_once_naming_the_count() {
        // prod: the profile name, typed once for the whole run.
        let mut app = opened();
        assert!(enter_with(&mut app, "3 KEYS user:*", Some(keys_info())).is_empty());
        let all = text(&app, 120, 36);
        assert!(
            all.contains("KEYS is @dangerous and asks first: KEYS user:* 3 times"),
            "{all}"
        );
        assert!(
            all.contains("Type prod-cache-01 and press enter to send it 3 times"),
            "{all}"
        );
        let mut e = type_text(&mut app, "prod-cache-01");
        e.extend(app.handle_key(key(KeyCode::Enter)));
        let mut next = reqs(&e);
        for i in 0..3 {
            let [(id, Request::Run { confirmed, .. })] = &next[..] else {
                panic!("{i}: {next:?}");
            };
            assert!(confirmed);
            // No second question.
            assert!(app.views.cmdline.confirm.is_none() && app.confirm.is_none());
            next = reqs(&reply(&mut app, *id, ran(Value::Array(vec![]))));
        }
        assert!(next.is_empty());
        assert!(text(&app, 120, 36).contains("(3 replies in 3 round trips"));

        // Elsewhere y, once; n sends none of them.
        let mut app = opened();
        app.session.as_mut().unwrap().profile.env = Env::Local;
        enter_with(&mut app, "4 KEYS user:*", Some(keys_info()));
        assert!(text(&app, 120, 36).contains("Send it 4 times to prod-cache-01?"));
        assert!(reqs(&app.handle_key(ch('n'))).is_empty());
        assert!(last_body(&app) == Body::Note("Not sent: cancelled.".into()));
        assert!(!app.views.cmdline.repeating());
    }

    #[test]
    fn a_write_repeat_goes_through_the_write_gate_every_time() {
        let mut app = opened();
        writable(&mut app, Env::Local);
        // Pipeline mode does not change writes: one at a time.
        app.handle_key(ctrl('p'));
        let e = enter_all(&mut app, "5 INCR counter", Some(incr_info()));
        let mut w = writes(&e);
        for i in 1..=5 {
            assert_eq!(w.len(), 1, "{i}: one at a time: {w:?}");
            let WriteOp::Command { args, .. } = &w[0].1 else {
                panic!("{w:?}")
            };
            assert_eq!(args.as_slice(), [b"INCR".to_vec(), b"counter".to_vec()]);
            // An edit outside prod runs without a question, each time.
            assert!(app.confirm.is_none());
            w = writes(&wrote(&mut app, w[0].0, Value::Int(i)));
        }
        assert!(w.is_empty());
        let all = text(&app, 120, 36);
        assert!(all.contains("#5 (integer) 5"), "{all}");
        assert!(all.contains("(5 replies in 5 round trips"), "{all}");

        // A refusal from the server mid-run turns INCR off, and the run ends there.
        let e = enter_cached(&mut app, "3 INCR counter");
        let w = writes(&e);
        let after = testing::reply(&mut app, w[0].0, Err(LiveError::Denied("INCR".into())));
        assert!(writes(&after).is_empty());
        let Body::Runs(r) = last_body(&app) else {
            panic!()
        };
        assert!(
            matches!(&r.end, Some(RunEnd::Failed(why)) if why.contains("refused INCR")),
            "{:?}",
            r.end.as_ref().map(|_| ())
        );
        assert!(!app.views.cmdline.running());
    }

    #[test]
    fn a_write_repeat_on_a_read_only_profile_sends_nothing() {
        for ro in [(Env::Prod, None), (Env::Local, Some(true))] {
            let mut app = opened();
            let s = app.session.as_mut().unwrap();
            s.profile.env = ro.0;
            s.profile.read_only = ro.1;
            let e = enter_all(&mut app, "5 INCR counter", Some(incr_info()));
            assert!(writes(&e).is_empty(), "{e:?}");
            assert!(app.confirm.is_none());
            let Body::Note(why) = last_body(&app) else {
                panic!()
            };
            assert!(why.contains("prod-cache-01 is read-only"), "{why}");
            assert!(!app.views.cmdline.running());
        }
    }

    #[test]
    fn a_write_repeat_that_would_ask_is_not_repeated() {
        let mut app = opened();
        writable(&mut app, Env::Local);
        for (line, i) in [("2 DEL k", del_info()), ("2 FLUSHDB", flushdb_info())] {
            let e = enter_all(&mut app, line, Some(i));
            assert!(writes(&e).is_empty(), "{line}");
            assert!(app.confirm.is_none(), "{line}");
            assert!(last_body(&app) == Body::Note(REPEAT_ASKS.into()), "{line}");
        }
        // On prod with writes on, an edit asks: not repeated either.
        let mut app = opened();
        writable(&mut app, Env::Prod);
        let e = enter_all(&mut app, "2 INCR counter", Some(incr_info()));
        assert!(writes(&e).is_empty());
        assert!(app.confirm.is_none());
        assert!(last_body(&app) == Body::Note(REPEAT_ASKS.into()));
        // Without the count it is asked as before.
        enter_all(&mut app, "INCR counter", Some(incr_info()));
        assert!(app.confirm.is_some());
    }

    #[test]
    fn skip_non_prod_confirm_does_not_repeat_destructive_or_dangerous_writes() {
        // The setting takes the question away for one command, but the spec says a
        // write that would ask without it is never repeated.
        let mut app = opened();
        writable(&mut app, Env::Stage);
        app.settings.skip_non_prod_confirm = true;
        let lpop = info("lpop", &["write", "fast"], &["@write", "@list", "@fast"]);
        let flushall = info(
            "flushall",
            &["write"],
            &["@keyspace", "@write", "@slow", "@dangerous"],
        );
        for (line, i) in [("3 LPOP jobs", lpop), ("3 FLUSHALL", flushall)] {
            let e = enter_all(&mut app, line, Some(i));
            assert!(writes(&e).is_empty(), "{line}: {e:?}");
            assert!(app.confirm.is_none(), "{line}");
            assert!(last_body(&app) == Body::Note(REPEAT_ASKS.into()), "{line}");
            assert!(!app.views.cmdline.running(), "{line}");
        }
        // A plain edit still repeats, one write at a time.
        let e = enter_all(&mut app, "3 INCR c", Some(incr_info()));
        let mut w = writes(&e);
        let mut sent = 0;
        while let [(id, op)] = &w[..] {
            let WriteOp::Command { args, .. } = op else {
                panic!()
            };
            assert_eq!(args.as_slice(), [b"INCR".to_vec(), b"c".to_vec()]);
            sent += 1;
            w = writes(&wrote(&mut app, *id, Value::Int(sent)));
        }
        assert_eq!(sent, 3);
        assert!(text(&app, 120, 36).contains("(3 replies in 3 round trips"));
    }

    #[test]
    fn esc_stops_a_repeat_and_keeps_the_replies() {
        let mut app = opened();
        let next = enter_with(&mut app, "5 GET k", Some(get_info()));
        let next = reqs(&reply(&mut app, next[0].0, ran(Value::Int(1))));
        // The second is on its way: esc sends nothing more, and the panel stays open.
        assert!(reqs(&app.handle_key(key(KeyCode::Esc))).is_empty());
        assert!(app.views.cmdline.open);
        assert!(text(&app, 80, 24).contains("stopping… 1 of 5 answered"));
        let after = reqs(&reply(&mut app, next[0].0, ran(Value::Int(2))));
        assert!(after.is_empty(), "{after:?}");
        assert!(!app.views.cmdline.running() && !app.views.cmdline.repeating());
        let all = text(&app, 80, 24);
        assert!(all.contains("#1 (integer) 1") && all.contains("#2 (integer) 2"));
        assert!(
            all.contains("Stopped with esc: 2 of 5 answered, nothing more sent."),
            "{all}"
        );
        // The next esc closes.
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.views.cmdline.open);

        // Stopped between pipelines too.
        let mut app = opened();
        app.session.as_mut().unwrap().profile.provider =
            Some(crate::model::Provider::ElasticacheServerless);
        app.handle_key(ctrl('p'));
        let next = enter_with(&mut app, "100 GET k", Some(get_info()));
        // 1,000 a second: pipelines of 100.
        let [(id, Request::Batch { times: 100, .. })] = &next[..] else {
            panic!("{next:?}")
        };
        app.handle_key(key(KeyCode::Esc));
        let outs = (0..100)
            .map(|i| Output::of(&Value::Int(i), Duration::ZERO))
            .collect();
        assert!(reqs(&reply(&mut app, *id, Reply::Batch(outs))).is_empty());
        let Body::Runs(r) = last_body(&app) else {
            panic!()
        };
        assert_eq!(r.answered(), 100);
        assert!(r.end == Some(RunEnd::Done) || r.end == Some(RunEnd::Stopped));
    }

    #[test]
    fn a_lost_reply_ends_a_repeat_with_the_replies_so_far() {
        let mut app = opened();
        let next = enter_with(&mut app, "3 GET k", Some(get_info()));
        reply(&mut app, next[0].0, ran(Value::Int(1)));
        let s = app.session.as_ref().unwrap();
        let (id, scan) = (s.id, s.scan);
        app.apply(Msg::LiveClosed {
            id,
            scan,
            error: None,
        });
        app.apply(Msg::TickAt(std::time::Instant::now()));
        assert!(!app.views.cmdline.running() && !app.views.cmdline.repeating());
        let all = text(&app, 120, 36);
        assert!(all.contains("#1 (integer) 1"), "{all}");
        assert!(
            all.contains("No answer: the live connection closed"),
            "{all}"
        );
        assert!(all.contains("(1 of 3 answered)"), "{all}");
    }

    #[test]
    fn pipeline_mode_sends_a_readonly_repeat_in_pipelines() {
        let mut app = opened();
        app.handle_key(ctrl('p'));
        assert!(app.views.cmdline.pipeline);
        assert!(text(&app, 120, 36).contains("· pipeline"));
        // The prod profile's budget is 5,000 a second: one pipeline holds 100.
        let next = enter_with(&mut app, "5 GET k", Some(get_info()));
        let [
            (
                id,
                Request::Batch {
                    args,
                    times,
                    confirmed,
                    ..
                },
            ),
        ] = &next[..]
        else {
            panic!("{next:?}")
        };
        assert_eq!((args.as_slice().len(), *times, *confirmed), (2, 5, false));
        let outs = (1..=5)
            .map(|i| Output::of(&Value::Int(i), Duration::from_micros(800)))
            .collect();
        assert!(reqs(&reply(&mut app, *id, Reply::Batch(outs))).is_empty());
        let all = text(&app, 120, 36);
        assert!(all.contains("#5 (integer) 5"), "{all}");
        assert!(all.contains("(5 replies in 1 pipeline · 0.80 ms)"), "{all}");
        // A small budget makes small pipelines: a tenth of a second each.
        app.session.as_mut().unwrap().profile.provider =
            Some(crate::model::Provider::ElasticacheServerless);
        let next = reqs(&enter_cached(&mut app, "100 GET k"));
        assert!(
            matches!(&next[..], [(_, Request::Batch { times: 100, .. })]),
            "{next:?}"
        );
        // Off again: one at a time.
        app.handle_key(ctrl('p'));
        assert!(!app.views.cmdline.pipeline);
    }

    #[test]
    fn ctrl_t_switches_how_replies_are_drawn() {
        let mut app = opened();
        let next = enter_with(&mut app, "HGETALL user:1", Some(hgetall_info()));
        let hash = Value::Array(
            ["name", "Ada", "visits", "12"]
                .iter()
                .map(|s| Value::BulkString(s.as_bytes().to_vec()))
                .collect(),
        );
        reply(&mut app, next[0].0, ran(hash));
        assert!(text(&app, 80, 24).contains("1) \"name\""));
        for (view, name, shows) in [
            (View::Table, "view_table_80x24", "2  visits  12"),
            (View::Json, "view_json_80x24", "\"visits\","),
            (View::Raw, "view_raw_80x24", "[bulk string] $6 \"visits\""),
        ] {
            app.handle_key(ctrl('t'));
            assert_eq!(app.views.cmdline.view, view);
            let all = text(&app, 80, 24);
            assert!(all.contains(shows), "{view:?}: {all}");
            assert!(all.contains(&format!("· {} view", view.label())), "{all}");
            insta::assert_snapshot!(name, testing::render(&app, 80, 24).backend());
        }
        app.handle_key(ctrl('t'));
        assert_eq!(app.views.cmdline.view, View::Text);
        // The view is kept for the next replies, and ctrl-t is not typed.
        assert_eq!(app.views.cmdline.shown_input(), "");
    }

    #[test]
    fn credentials_after_a_count_are_masked() {
        let mut app = opened();
        let line = format!("3 CONFIG SET requirepass {MARK}");
        type_text(&mut app, &line);
        assert_eq!(
            app.views.cmdline.shown_input(),
            "3 CONFIG SET requirepass ***"
        );
        assert!(!value::leaks(&text(&app, 120, 36), MARK));
        // Pressed enter: refused on this read-only profile, and kept masked.
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.views.cmdline.history(),
            ["3 CONFIG SET requirepass ***"]
        );
        let all = text(&app, 120, 36);
        assert!(!value::leaks(&all, MARK), "{all}");
        assert!(!format!("{:?}", app.views.cmdline).contains(MARK));
        let mut app = opened();
        type_text(&mut app, &format!("2 AUTH {MARK}"));
        assert_eq!(app.views.cmdline.shown_input(), "2 AUTH ***");
    }

    #[test]
    fn tab_completes_the_command_after_a_count() {
        let mut app = opened();
        type_text(&mut app, "3 GE");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "3 GET");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.views.cmdline.shown_input(), "3 GETDEL");
    }

    #[test]
    fn a_repeat_s_replies_never_reach_debug() {
        let mut app = opened();
        let next = enter_with(&mut app, "2 GET k", Some(get_info()));
        let next = reqs(&reply(
            &mut app,
            next[0].0,
            ran(Value::BulkString(MARK.as_bytes().to_vec())),
        ));
        assert!(app.views.cmdline.repeating());
        let d = format!("{:?}", app.views.cmdline);
        assert!(!d.contains(MARK), "{d}");
        assert!(d.contains("repeat: true"), "{d}");
        reply(&mut app, next[0].0, ran(Value::Int(1)));
        assert!(!format!("{:?}", app.views.cmdline).contains(MARK));
    }

    #[test]
    fn three_one_mib_replies_fit_under_the_output_cap() {
        // The cap counts the largest view of each reply, not all four together: three
        // replies of about 1 MiB (each about that size in every view) are all kept.
        let big = || Value::BulkString(vec![b'a'; 1_000_000]);
        let mut app = opened();
        let run = enter_with(&mut app, "GET a", Some(get_info()));
        reply(&mut app, run[0].0, ran(big()));
        for k in ["GET b", "GET c"] {
            let run = reqs(&enter_cached(&mut app, k));
            reply(&mut app, run[0].0, ran(big()));
        }
        let lines: Vec<_> = app
            .views
            .cmdline
            .entries
            .iter()
            .filter_map(|e| e.line.clone())
            .collect();
        assert_eq!(lines, ["GET a", "GET b", "GET c"]);

        // A repeat keeps all three too, and drops none.
        let mut next = reqs(&enter_cached(&mut app, "3 GET k"));
        while let [(id, _)] = &next[..] {
            next = reqs(&reply(&mut app, *id, ran(big())));
        }
        let Body::Runs(r) = last_body(&app) else {
            panic!()
        };
        assert_eq!((r.answered(), r.outs.len(), r.dropped), (3, 3, 0));
    }

    #[test]
    fn a_long_repeat_keeps_the_output_under_its_cap() {
        let mut app = opened();
        let mut next = enter_with(&mut app, "8 GET k", Some(get_info()));
        // 8 replies of about 0.7 MiB in each view: the oldest go first.
        let big = Value::BulkString(vec![b'a'; MAX_OUTPUT_BYTES / 6]);
        while let [(id, _)] = &next[..] {
            next = reqs(&reply(&mut app, *id, ran(big.clone())));
        }
        let Body::Runs(r) = last_body(&app) else {
            panic!()
        };
        assert_eq!(r.answered(), 8);
        assert!(r.dropped > 0 && !r.outs.is_empty());
        assert!(r.bytes() <= MAX_OUTPUT_BYTES || r.outs.len() == 1);
        assert_eq!(r.outs.last().unwrap().n, 8);
        let shown = entry_text(app.views.cmdline.entries.last().unwrap(), View::Text);
        assert!(shown[0].contains("replies were dropped"), "{:?}", shown[0]);
    }
}
