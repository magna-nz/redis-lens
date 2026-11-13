//! The small forms of the write actions on the Server, Streams, Pub/Sub and Search
//! views (spec "Write permissions"): the slow log settings (`c` on Slow log), a claim
//! (`c`) and a trim (`X`) on Streams, a message to publish (`P` on Pub/Sub) and an index
//! to create (`C` on Search). Drawn by `ui/streams.rs` (`draw_form`).
//!
//! A form only collects text. Its view turns the text into a [`WriteOp`] and sends it
//! with [`Act::write`], the one gate, which asks first when the table says so. What is
//! typed may be a value (a message to publish): it is drawn, and nowhere else. The
//! `Debug` of a form shows its labels and lengths, never the text.
//!
//! Keys: characters type into the focused field, `backspace` deletes, `ctrl-u` clears
//! the field, `tab` and `↓` go to the next field, `shift-tab` and `↑` to the previous
//! one, `←` `→` and `space` change a choice, `enter` goes to the next field and on the
//! last one sends, `esc` cancels. A paste types its text; a newline in it becomes a
//! space, or `, ` in a list field.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::super::{Act, Input};
use crate::live::{CANCELLED, LiveError};
use crate::write::{Decision, WriteOp};

/// The longest text a field takes, in characters.
pub const MAX_TEXT: usize = 4096;

/// One field of a form.
#[derive(Clone, PartialEq, Eq)]
pub struct Field {
    /// Shown before the field and as the key cap of the key bar.
    pub label: &'static str,
    /// What is typed; for a choice, the option chosen.
    pub text: String,
    /// The options of a choice; empty for a text field.
    pub options: &'static [&'static str],
    /// One dim line under the fields while this one has the focus.
    pub help: &'static str,
    /// A list: a pasted newline becomes `, `.
    pub list: bool,
}

impl Field {
    pub fn text(label: &'static str, help: &'static str) -> Self {
        Self {
            label,
            text: String::new(),
            options: &[],
            help,
            list: false,
        }
    }

    /// A choice between `options`, the first chosen.
    pub fn choice(
        label: &'static str,
        options: &'static [&'static str],
        help: &'static str,
    ) -> Self {
        Self {
            label,
            text: options.first().copied().unwrap_or_default().to_string(),
            options,
            help,
            list: false,
        }
    }

    /// The field starts with `text`.
    pub fn with(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// The field is a list (pasted newlines become `, `).
    pub fn as_list(mut self) -> Self {
        self.list = true;
        self
    }

    pub fn is_choice(&self) -> bool {
        !self.options.is_empty()
    }

    fn push(&mut self, add: &str) {
        let room = MAX_TEXT.saturating_sub(self.text.chars().count());
        let mut out = String::new();
        for c in add.chars() {
            match c {
                '\r' => {}
                '\n' if self.list => out.push_str(", "),
                '\n' | '\t' => out.push(' '),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        self.text.extend(out.chars().take(room));
    }

    fn step_choice(&mut self, by: isize) {
        let n = self.options.len();
        if n == 0 {
            return;
        }
        let at = self
            .options
            .iter()
            .position(|o| *o == self.text)
            .unwrap_or(0);
        let next = (at as isize + by).rem_euclid(n as isize) as usize;
        self.text = self.options[next].to_string();
    }
}

/// A form: what it acts on, its fields, and which one has the focus.
#[derive(Clone, PartialEq, Eq)]
pub struct Form {
    /// The box's title.
    pub title: String,
    /// Lines above the fields, saying what the form acts on. Escaped by the view.
    pub about: Vec<String>,
    pub fields: Vec<Field>,
    pub focus: usize,
}

/// What a key did to a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The form changed, or the key did nothing.
    Typed,
    /// `enter` on the last field: send.
    Submit,
    /// `esc`.
    Cancel,
}

impl Form {
    pub fn new(title: impl Into<String>, about: Vec<String>, fields: Vec<Field>) -> Self {
        Self {
            title: title.into(),
            about,
            fields,
            focus: 0,
        }
    }

    /// The text of the field `label`, trimmed. Empty when there is no such field.
    pub fn value(&self, label: &str) -> &str {
        self.fields
            .iter()
            .find(|f| f.label == label)
            .map_or("", |f| f.text.trim())
    }

    /// The text of the field `label` as typed, spaces kept: for a value.
    pub fn raw(&self, label: &str) -> &str {
        self.fields
            .iter()
            .find(|f| f.label == label)
            .map_or("", |f| f.text.as_str())
    }

    pub fn focused(&self) -> Option<&Field> {
        self.fields.get(self.focus)
    }

    fn last(&self) -> bool {
        self.focus + 1 >= self.fields.len()
    }

    fn next(&mut self, by: isize) {
        let n = self.fields.len().max(1) as isize;
        self.focus = (self.focus as isize + by).rem_euclid(n) as usize;
    }

    /// A key while the form is open: every key is the form's.
    pub fn key(&mut self, key: KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return Step::Cancel,
            KeyCode::Enter if self.last() => return Step::Submit,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.next(1),
            KeyCode::BackTab | KeyCode::Up => self.next(-1),
            _ => {}
        }
        let Some(field) = self.fields.get_mut(self.focus) else {
            return Step::Typed;
        };
        match key.code {
            KeyCode::Left if field.is_choice() => field.step_choice(-1),
            KeyCode::Right | KeyCode::Char(' ') if field.is_choice() => field.step_choice(1),
            _ if field.is_choice() => {}
            KeyCode::Backspace => {
                field.text.pop();
            }
            KeyCode::Char('u') if ctrl => field.text.clear(),
            KeyCode::Char(c) if !ctrl && !alt => field.push(&c.to_string()),
            _ => {}
        }
        Step::Typed
    }

    /// Pasted text, into the focused text field.
    pub fn paste(&mut self, text: &str) {
        if let Some(field) = self.fields.get_mut(self.focus)
            && !field.is_choice()
        {
            field.push(text);
        }
    }

    /// The key bar while the form is open: the focused field being typed.
    pub fn input(&self) -> Option<Input> {
        let field = self.focused()?;
        let (text, hint) = if field.is_choice() {
            (
                format!("‹ {} ›", field.text),
                "  ←→ change · tab next · esc cancel",
            )
        } else if self.last() {
            (
                field.text.clone(),
                "  tab next field · enter send · esc cancel",
            )
        } else {
            (
                field.text.clone(),
                "  tab next field · enter next · esc cancel",
            )
        };
        Some(Input {
            label: field.label,
            text,
            hint,
        })
    }
}

/// Labels and lengths only: a field may hold a value.
impl fmt::Debug for Form {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fields: Vec<String> = self
            .fields
            .iter()
            .map(|x| format!("{}: <{} chars>", x.label, x.text.chars().count()))
            .collect();
        f.debug_struct("Form")
            .field("title", &self.title)
            .field("fields", &fields)
            .field("focus", &self.focus)
            .finish()
    }
}

impl fmt::Debug for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Field({}: <{} chars>)",
            self.label,
            self.text.chars().count()
        )
    }
}

/// Before an action asks anything: true, with the reason as an error notice, when the
/// gate refuses `op` now (a read-only profile, or a command the server refused for
/// this user). `op` is an example of what the action sends.
pub fn write_off(act: &mut Act<'_>, op: &WriteOp) -> bool {
    match act.write_decision(op) {
        Decision::Refuse(why) => {
            act.error(why);
            true
        }
        _ => false,
    }
}

/// The notice for a write that failed. Nothing for a cancel: `App` says so.
pub fn write_failed(act: &mut Act<'_>, e: &LiveError) {
    if let LiveError::Failed(why) = e
        && why == CANCELLED
    {
        return;
    }
    act.error(sentence(&e.to_string()));
}

/// `s` with a capital first letter and a full stop.
pub fn sentence(s: &str) -> String {
    let mut c = s.chars();
    let mut out: String = c
        .next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default();
    if !out.ends_with(['.', '!', '?', '…']) {
        out.push('.');
    }
    out
}

/// `n` and the noun, singular for one: `1 subscriber`, `3 subscribers`.
pub fn count(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Helpers for the write tests of the views that use forms.
#[cfg(test)]
pub(crate) mod testing {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::live::LiveRequest;
    use crate::model::Env;
    use crate::tui::app::{App, Effect};
    use crate::write::WriteOp;

    pub fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    pub fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    pub fn type_text(app: &mut App, s: &str) -> Vec<Effect> {
        s.chars().flat_map(|c| app.handle_key(ch(c))).collect()
    }

    /// The writes among `effects`.
    pub fn writes(effects: &[Effect]) -> Vec<WriteOp> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Live {
                    req: LiveRequest::Write(p),
                    ..
                } => Some(p.op().clone()),
                _ => None,
            })
            .collect()
    }

    /// Sets the session's profile (prod-cache-01) to `env`, writes on or off.
    pub fn set_profile(app: &mut App, env: Env, read_only: Option<bool>) {
        let s = app.session.as_mut().expect("a session");
        s.profile.env = env;
        s.profile.read_only = read_only;
    }

    /// The read-only profiles: `prod` by default, and any with writes off.
    pub const READ_ONLY: [(Env, Option<bool>); 4] = [
        (Env::Prod, None),
        (Env::Prod, Some(true)),
        (Env::Stage, Some(true)),
        (Env::Local, Some(true)),
    ];

    /// Every key a user might answer a question or fill a form with: `y`, `enter`, the
    /// profile's name pasted, `enter` again. The effects of them all.
    pub fn answer_anything(app: &mut App) -> Vec<Effect> {
        let mut out = app.handle_key(ch('y'));
        out.extend(press(app, KeyCode::Enter));
        app.handle_paste("prod-cache-01".into());
        out.extend(press(app, KeyCode::Enter));
        out.extend(press(app, KeyCode::Enter));
        out.extend(app.handle_key(ch('y')));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn form() -> Form {
        Form::new(
            "Test",
            vec![],
            vec![
                Field::text("name", "a name"),
                Field::choice("on", &["HASH", "JSON"], "what"),
                Field::text("fields", "the fields").as_list(),
            ],
        )
    }

    #[test]
    fn keys_type_move_and_submit() {
        let mut f = form();
        for c in "ab".chars() {
            assert_eq!(f.key(k(KeyCode::Char(c))), Step::Typed);
        }
        f.key(k(KeyCode::Backspace));
        assert_eq!(f.value("name"), "a");
        f.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(f.value("name"), "");
        f.key(k(KeyCode::Char('x')));
        // enter goes to the next field, and sends on the last.
        assert_eq!(f.key(k(KeyCode::Enter)), Step::Typed);
        assert_eq!(f.focus, 1);
        // A choice changes with ←→ and space; letters do nothing.
        f.key(k(KeyCode::Char('z')));
        assert_eq!(f.value("on"), "HASH");
        f.key(k(KeyCode::Right));
        assert_eq!(f.value("on"), "JSON");
        f.key(k(KeyCode::Char(' ')));
        assert_eq!(f.value("on"), "HASH");
        f.key(k(KeyCode::Left));
        assert_eq!(f.value("on"), "JSON");
        f.key(k(KeyCode::Tab));
        f.paste("title TEXT\nprice NUMERIC\x1b");
        assert_eq!(f.value("fields"), "title TEXT, price NUMERIC");
        assert_eq!(f.key(k(KeyCode::Enter)), Step::Submit);
        f.key(k(KeyCode::BackTab));
        assert_eq!(f.focus, 1);
        f.key(k(KeyCode::Up));
        f.key(k(KeyCode::Up));
        assert_eq!(f.focus, 2);
        assert_eq!(f.key(k(KeyCode::Esc)), Step::Cancel);
    }

    #[test]
    fn a_paste_is_one_line_and_capped() {
        let mut f = form();
        f.paste("a\r\nb\tc");
        assert_eq!(f.value("name"), "a b c");
        f.paste(&"x".repeat(MAX_TEXT * 2));
        assert_eq!(f.fields[0].text.chars().count(), MAX_TEXT);
    }

    #[test]
    fn debug_never_shows_the_text() {
        let mut f = form();
        f.paste("hunter2");
        let d = format!("{f:?}");
        assert!(!d.contains("hunter2"), "{d}");
        assert!(d.contains("name: <7 chars>"), "{d}");
    }

    #[test]
    fn the_key_bar_shows_the_focused_field() {
        let mut f = form();
        f.paste("idx");
        let i = f.input().unwrap();
        assert_eq!((i.label, i.text.as_str()), ("name", "idx"));
        assert!(i.hint.contains("enter next"));
        f.key(k(KeyCode::Tab));
        assert_eq!(f.input().unwrap().text, "‹ HASH ›");
        f.key(k(KeyCode::Tab));
        assert!(f.input().unwrap().hint.contains("enter send"));
    }

    #[test]
    fn sentences_and_counts() {
        assert_eq!(
            sentence("the server refused X for this user"),
            "The server refused X for this user."
        );
        assert_eq!(sentence("Not sent: no."), "Not sent: no.");
        assert_eq!(count(1, "subscriber", "subscribers"), "1 subscriber");
        assert_eq!(count(0, "subscriber", "subscribers"), "0 subscribers");
    }
}
