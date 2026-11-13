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

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{Act, Input, LiveState, Owner, ViewState};
use crate::live::cmdline::{self, Args, CommandInfo, Docs, Gate, Output, Reply, Request};
use crate::live::command::{self, MASK, Secret};
use crate::live::keys::KeyRow;
use crate::live::{LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{Env, ServerMode};
use crate::tui::app::{App, Effect};
use crate::value;

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
/// Why the command line is off on a cluster.
pub const CLUSTER_OFF: &str = "The command line is not available on a cluster yet: it talks to \
     one server, and a cluster spreads its keys over its nodes.";

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
    let secrets = command::secrets(&words);
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
}

/// One command and what came of it.
#[derive(Clone, PartialEq)]
pub(crate) struct Entry {
    pub id: u64,
    /// The command as [`command::masked_line`] writes it; `None` for a note alone.
    pub line: Option<String>,
    pub body: Body,
}

impl Entry {
    fn bytes(&self) -> usize {
        let body = match &self.body {
            Body::Out(o) => o.lines().iter().map(String::len).sum(),
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
}

/// The one command on its way.
#[derive(Clone, PartialEq)]
enum Pending {
    /// Waiting for `COMMAND INFO` of `name` (request `id`) to decide.
    Info {
        id: ReqId,
        name: String,
        args: Args,
        entry: u64,
    },
    /// Sent as request `id`.
    Run { id: ReqId, entry: u64 },
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
            .finish()
    }
}

/// The first word as `COMMAND INFO` is asked about it: lower-case. `None` when it is
/// not something to ask about (empty, too long, control characters).
fn name_of(word: &[u8]) -> Option<String> {
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

    fn push_entry(&mut self, line: Option<String>, body: Body) -> u64 {
        let id = self.next_entry;
        self.next_entry += 1;
        self.entries.push(Entry { id, line, body });
        self.scroll.set(0);
        self.trim();
        id
    }

    fn trim(&mut self) {
        // Never the entry a pending command or question writes to.
        let keep = |s: &Self| match (&s.pending, &s.confirm) {
            (Some(Pending::Info { entry, .. } | Pending::Run { entry, .. }), _) => Some(*entry),
            (_, Some(c)) => Some(c.entry),
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
        let Some(first) = toks.first() else {
            return;
        };
        if toks.len() == 1 && first.end == self.input.len() {
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
            KeyCode::Esc => self.open = false,
            KeyCode::Enter => self.submit(act),
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
                .first()
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
        let args = match command::split(&text) {
            Ok(a) => a,
            Err(e) => {
                self.push_entry(Some(masked_input(&text)), Body::Note(e.message().into()));
                return;
            }
        };
        let line = command::masked_line(&args);
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
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
        let args = Args::new(args);
        if let Some(info) = self.infos.get(&name).cloned() {
            self.decide(args, info, entry, act);
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
                })
            }
            None => self.set_body(
                entry,
                Body::Failed("Not sent: the live connection is not open.".into()),
            ),
        }
    }

    /// What [`cmdline::gate`] says, done.
    fn decide(&mut self, args: Args, info: Option<CommandInfo>, entry: u64, act: &mut Act<'_>) {
        match cmdline::gate(args.as_slice(), info.as_ref()) {
            Gate::Refuse(why) => self.set_body(entry, Body::Note(why)),
            Gate::Send => {
                let Some(info) = info else {
                    return;
                };
                self.send(args, info, false, entry, act);
            }
            Gate::Ask => {
                let Some(info) = info else {
                    return;
                };
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
        let req = Request::Run {
            args,
            info,
            confirmed,
        };
        match self.ask(req, Asked::Run, act) {
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
        if yes {
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
            let ask = match &c.typed {
                Some(_) => format!(
                    "Type {} and press enter to send it, or esc to cancel.",
                    c.profile
                ),
                None => format!("Send it to {}? y to send, n or esc to cancel.", c.profile),
            };
            // A wrong name says so in place of the question.
            let second = match &self.status {
                Some(s) => (s.clone(), true),
                None => (ask, false),
            };
            return vec![
                (
                    format!("▲ {} is @dangerous and asks first: {}", c.command, c.line),
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
                "Readonly commands only. tab completes, ↑↓ steps through the history, \
                 ctrl-l clears."
                    .into(),
                false,
            )];
        };
        let Some(name) = name_of(&first.word) else {
            return Vec::new();
        };
        let words: Vec<Vec<u8>> = toks.iter().map(|t| t.word.clone()).collect();
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
        let verdict = match cmdline::gate(&words, Some(info)) {
            Gate::Send => ("readonly".to_string(), false),
            Gate::Ask => ("readonly, asks first (@dangerous)".to_string(), true),
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
        let mut first_line = cmd.display();
        if let Some(d) = docs.filter(|d| !d.arguments.is_empty()) {
            first_line.push(' ');
            first_line.push_str(&d.arguments);
        }
        if let Some(d) = docs.filter(|d| !d.complexity.is_empty()) {
            first_line.push_str(" · ");
            first_line.push_str(&d.complexity);
        }
        let mut second = format!("{} · {}", verdict.0, cmd.categories.join(" "));
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
                        if let Some(Pending::Info { args, entry, .. }) = pending {
                            self.decide(args, i, entry, act);
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
                let body = match reply {
                    Ok(Reply::Ran(out)) => Body::Out(out),
                    Ok(_) => Body::Failed("the reply came back in an unexpected shape".into()),
                    Err(e) => Body::Failed(e.to_string()),
                };
                self.set_body(entry, body);
                self.scroll.set(0);
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
    /// replaced it) are forgotten, and the command waiting on one says so.
    fn forget_lost(&mut self, act: &Act<'_>) {
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
                Some(Pending::Info { id: p, entry, .. } | Pending::Run { id: p, entry })
                    if *p == id =>
                {
                    Some(*entry)
                }
                _ => None,
            };
            if let Some(entry) = entry {
                self.pending = None;
                self.set_body(
                    entry,
                    Body::Failed(
                        "No answer: the live connection closed before the reply came.".into(),
                    ),
                );
            }
        }
    }
}

impl ViewState for CmdlineState {
    fn on_tick(&mut self, _now: Instant, act: &mut Act<'_>) {
        self.forget_lost(act);
    }

    fn on_reply(&mut self, id: ReqId, result: Result<LiveReply, LiveError>, act: &mut Act<'_>) {
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
        if s.server
            .as_ref()
            .is_some_and(|i| i.mode == ServerMode::Cluster)
        {
            return Some(CLUSTER_OFF.into());
        }
        match &s.live {
            LiveState::Ready => None,
            LiveState::Waiting => Some(
                "The command line opens once the live connection is ready, after the scan \
                 connects."
                    .into(),
            ),
            LiveState::Closed(_) => Some(
                "The live connection is closed, so the command line is off until the next \
                 scan."
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ch('a'));
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open);
        assert_eq!(app.add.as_ref().unwrap().input, ":");
    }

    #[test]
    fn off_before_the_live_connection_is_ready_and_on_a_cluster() {
        let mut app = App::new(crate::tui::fixtures::profiles());
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open);
        assert!(notice(&app).contains("Connect first"), "{}", notice(&app));
        let mut app = testing::scanned();
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open);
        assert!(notice(&app).contains("once the live connection is ready"));
        testing::live_ready(&mut app);
        app.session.as_mut().unwrap().server.as_mut().unwrap().mode = ServerMode::Cluster;
        app.handle_key(ch(':'));
        assert!(!app.views.cmdline.open);
        assert_eq!(notice(&app), CLUSTER_OFF);
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
        assert!(all.contains("> GET user:1"), "{all}");
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
        let after = enter_with(&mut app, "SET k v", Some(set));
        assert!(after.is_empty(), "{after:?}");
        let all = text(&app, 120, 36);
        assert!(all.contains("SET is not a readonly command"), "{all}");
        // Also on a profile that is not read-only: phase 2 sends no writes.
        app.session.as_mut().unwrap().profile.read_only = Some(false);
        app.session.as_mut().unwrap().profile.env = Env::Local;
        type_text(&mut app, "SET k v");
        let r = reqs(&app.handle_key(key(KeyCode::Enter)));
        assert!(r.is_empty(), "{r:?}");
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
        assert!(
            all.contains("ACL SETUSER is not a readonly command"),
            "{all}"
        );
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
        assert!(all.contains("> GET user:1"), "{all}");
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
}
