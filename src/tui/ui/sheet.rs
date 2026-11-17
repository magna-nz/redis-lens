//! The one confirm (spec "One confirm"): every question that guards a write or a
//! costly read is this sheet, docked to the bottom of the body, full width, above the
//! footer. The rest of the screen is not dimmed, so the row being acted on stays in
//! view above it. Line 1 (the top border) is what it asks, with the profile and its
//! environment at the right; then what it acts on, `Sends   <command>`, whether it can
//! be undone; then the answer: `y <Verb>   n Cancel`, or on production the name to type
//! and the input. Its height is what the content needs.
//!
//! The sheet only draws: each question keeps its own state and keys (the write gate's
//! [`crate::tui::app::confirm`], Monitor's prompt, the command line's own question).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::analysis::wrap_hard;
use super::style;
use super::widgets::{focus_box, rows, trunc, width};
use crate::model::Env;

/// How the question is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// `y` and the go key's word, or `n`.
    YesNo(&'static str),
    /// This name typed, then `enter`; `typed` so far.
    Type {
        name: String,
        typed: String,
        verb: &'static str,
    },
}

/// One question, ready to draw: every text safe to draw already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sheet {
    pub title: String,
    /// `<profile> · ENV`.
    pub right: String,
    /// The body lines: `(label, text, style)`. A label (`Sends`) sits in a column of
    /// its own; an empty one starts the text at the left.
    pub lines: Vec<(&'static str, String, Style)>,
    pub answer: Answer,
    /// A reply opened it a moment ago: it takes no answer yet, and says `wait…` in
    /// place of its keys ([`crate::tui::app::App::in_grace`]).
    pub wait: bool,
}

/// `<profile> · ENV` for a sheet's top border.
pub fn right_of(profile: &str, env: Env) -> String {
    format!("{} · {}", crate::value::escape(profile), env.label())
}

/// The label column: ` Sends   `.
const LABEL_W: usize = 9;

/// Said on the answer line, and in the footer, while it waits.
pub const WAIT: &str = "wait…";

/// The style of the go key's word: the danger colour for a verb that loses data
/// (`Delete`, the word the write gate puts on every destructive or bulk delete), plain
/// for the others (`Send`, `Set TTLs`, `Start`, `Turn on`).
pub fn verb_style(verb: &str) -> Style {
    if verb == "Delete" {
        style::danger()
    } else {
        style::fg()
    }
}

impl Sheet {
    /// The footer while it is open: its answer keys, or only the cancel while it
    /// waits.
    pub fn keys(&self) -> Vec<(&'static str, &'static str)> {
        if self.wait {
            return vec![("esc", "Cancel")];
        }
        match &self.answer {
            Answer::YesNo(verb) => vec![("y", verb), ("n", "Cancel")],
            Answer::Type { verb, .. } => {
                vec![("enter", verb), ("esc", "Cancel"), ("ctrl-u", "Clear")]
            }
        }
    }

    /// The body lines `w` cells wide: wrapped (`whole`), or one row each cut with `…`.
    fn body(&self, w: usize, whole: bool) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        for (label, text, st) in &self.lines {
            if label.is_empty() {
                let parts = if whole {
                    wrap_hard(text, w.saturating_sub(2))
                } else {
                    vec![trunc(text, w.saturating_sub(2))]
                };
                for part in parts {
                    out.push(Line::from(Span::styled(format!(" {part}"), *st)));
                }
                continue;
            }
            let room = w.saturating_sub(LABEL_W + 1);
            let parts = if whole {
                wrap_hard(text, room)
            } else {
                vec![trunc(text, room)]
            };
            for (i, part) in parts.into_iter().enumerate() {
                let lead = if i == 0 {
                    format!(" {label:<w$}", w = LABEL_W - 1)
                } else {
                    " ".repeat(LABEL_W)
                };
                out.push(Line::from(vec![
                    Span::styled(lead, style::dim()),
                    Span::styled(part, *st),
                ]));
            }
        }
        out
    }

    /// The answer, always whole: the keys (or `wait…`), or the name to type, wrapped
    /// over as many rows as it needs, and the input.
    fn answer_lines(&self, w: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Answer::Type { name, .. } = &self.answer {
            let ask = format!("Type {} and press enter", crate::value::escape(name));
            for part in wrap_hard(&ask, w.saturating_sub(2)) {
                out.push(Line::from(Span::styled(
                    format!(" {part}"),
                    style::warning(),
                )));
            }
        }
        if self.wait {
            out.push(Line::from(vec![
                Span::styled(format!(" {WAIT}"), style::warning()),
                Span::styled("   esc cancels", style::dim()),
            ]));
            return out;
        }
        match &self.answer {
            Answer::YesNo(verb) => out.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(" y ", style::key()),
                Span::styled(format!(" {verb}"), verb_style(verb)),
                Span::raw("   "),
                Span::styled(" n ", style::key()),
                Span::styled(" Cancel", style::fg()),
            ])),
            Answer::Type { typed, .. } => out.push(typed_line(typed, w)),
        }
        out
    }

    /// Every row inside the sheet for `room` rows of `w` cells: the body wrapped when
    /// it fits, else cut with `…` a row each, then left out from the end; a blank; the
    /// answer, never cut.
    fn rows(&self, w: usize, room: usize) -> Vec<Line<'static>> {
        let answer = self.answer_lines(w);
        let left = room.saturating_sub(answer.len());
        let mut body = self.body(w, true);
        if body.len() + 1 > left {
            body = self.body(w, false);
        }
        let blank = body.len() < left;
        body.truncate(left);
        let mut out = body;
        if blank {
            out.push(Line::default());
        }
        out.extend(answer);
        out
    }
}

/// ` › typed▏`, the start cut with `…` when it is longer than the room.
fn typed_line(typed: &str, w: usize) -> Line<'static> {
    let room = w.saturating_sub(4);
    let typed = crate::value::escape(typed);
    let shown = if width(&typed) > room {
        let chars: Vec<char> = typed.chars().collect();
        let mut keep = String::from("…");
        let start = chars.len().saturating_sub(room.saturating_sub(1));
        keep.extend(&chars[start..]);
        keep
    } else {
        typed
    };
    Line::from(vec![
        Span::styled(" › ", style::focus()),
        Span::styled(shown, style::fgb()),
        Span::styled("▏", style::focus()),
    ])
}

/// The title and the right of the top border for a box `w` wide: the environment
/// (after the last ` · `) always stays; the profile name is cut first, then the title.
fn border_texts(title: &str, right: &str, w: usize) -> (String, String) {
    // The border takes `┌─ ` + title + ` ` … ` ` + right + ` ─┐` and keeps 4 spare.
    let fixed = 10;
    let (profile, env) = right.rsplit_once(" · ").unwrap_or(("", right));
    let env_w = width(env);
    let title_room = w.saturating_sub(fixed + env_w + 3).max(1);
    let title = trunc(title, title_room);
    let room = w.saturating_sub(fixed + width(&title));
    if width(right) <= room {
        return (title, right.to_string());
    }
    let profile_room = room.saturating_sub(env_w + 3);
    if profile.is_empty() || profile_room < 2 {
        return (title, env.to_string());
    }
    (title, format!("{} · {env}", trunc(profile, profile_room)))
}

/// Draws `sheet` docked to the bottom of `body`, full width.
pub fn draw(frame: &mut Frame, body: Rect, sheet: &Sheet) {
    if body.width < 10 || body.height < 3 {
        return;
    }
    let inner_w = usize::from(body.width.saturating_sub(2));
    let room = usize::from(body.height.saturating_sub(2));
    let lines = sheet.rows(inner_w, room);
    let h = u16::try_from(lines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(body.height);
    let rect = Rect {
        y: body.y + body.height - h,
        height: h,
        ..body
    };
    let (title, right) = border_texts(&sheet.title, &sheet.right, usize::from(body.width));
    let inner = focus_box(frame, rect, &title, &right);
    // When even the answer is taller than the body, its end (the input) shows.
    let skip = lines.len().saturating_sub(usize::from(inner.height));
    rows(frame, inner, lines.into_iter().skip(skip).collect(), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(answer: Answer) -> Sheet {
        Sheet {
            title: "Delete this key?".into(),
            right: right_of("prod", Env::Prod),
            lines: vec![
                ("", "Key user:1".into(), style::fg()),
                ("Sends", "UNLINK user:1".into(), style::fgb()),
            ],
            answer,
            wait: false,
        }
    }

    fn text(s: &Sheet, w: u16, h: u16) -> Vec<String> {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, f.area(), s)).unwrap();
        let b = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| b[(x, y)].symbol()).collect())
            .collect()
    }

    /// Docked to the bottom, as tall as its content, the answer on its last line.
    #[test]
    fn a_sheet_docks_to_the_bottom_and_is_as_tall_as_it_needs() {
        let rows = text(&sheet(Answer::YesNo("Delete")), 40, 12);
        assert!(rows[..6].iter().all(|r| r.trim().is_empty()), "{rows:#?}");
        assert!(rows[6].starts_with("┌─ Delete this key?"), "{rows:#?}");
        assert!(rows[6].contains("prod · PROD"), "{rows:#?}");
        assert!(rows[8].contains("Sends   UNLINK user:1"), "{rows:#?}");
        assert!(rows[10].contains(" y  Delete    n  Cancel"), "{rows:#?}");
        assert!(rows[11].starts_with('└'));
        let typed = sheet(Answer::Type {
            name: "user:1".into(),
            typed: "user".into(),
            verb: "Delete",
        });
        let rows = text(&typed, 40, 12);
        assert!(
            rows.iter()
                .any(|r| r.contains("Type user:1 and press enter"))
        );
        assert!(rows.iter().any(|r| r.contains("› user▏")));
        assert_eq!(typed.keys()[0], ("enter", "Delete"));
    }

    /// In a body too short for it, the answer stays.
    #[test]
    fn a_short_body_keeps_the_answer() {
        let rows = text(&sheet(Answer::YesNo("Delete")), 40, 4);
        assert!(rows[2].contains("y  Delete"), "{rows:#?}");
    }

    /// At 60×12 on production with a long profile and a long name: the answer and
    /// the whole name to type stay, the body is cut with `…`, and the title keeps
    /// `PROD`, cutting the profile name first.
    #[test]
    fn a_narrow_short_sheet_keeps_the_answer_the_name_and_the_env() {
        let name = "user:10293:cart:a-rather-long-key-name-that-does-not-fit-on-one-line";
        let sheet = Sheet {
            title: "Delete this key?".into(),
            right: right_of("prod-cache-01-eu-west-1-primary-cluster", Env::Prod),
            lines: vec![
                ("", format!("Key {name}"), style::fg()),
                ("Sends", format!("UNLINK {name}"), style::fgb()),
                ("", "This can't be undone.".into(), style::fg()),
            ],
            answer: Answer::Type {
                name: name.into(),
                typed: String::new(),
                verb: "Delete",
            },
            wait: false,
        };
        let rows = text(&sheet, 60, 12);
        let all = rows.join("\n");
        insta::assert_snapshot!("narrow_prod_60x12", all);
        assert!(rows.iter().any(|r| r.contains("PROD")), "{all}");
        assert!(rows.iter().any(|r| r.contains("prod-cache")), "{all}");
        // The instruction, joined over its rows, holds the whole name.
        let ask: String = rows
            .iter()
            .filter(|r| !r.contains('…') && !r.contains("Sends") && !r.contains("Key "))
            .map(|r| r.trim_matches(|c| c == '│' || c == ' ').to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(ask.contains(name), "{all}");
        assert!(rows.iter().any(|r| r.contains("› ▏")), "{all}");
        assert!(
            rows.iter().any(|r| r.contains('…')),
            "the body is cut with …: {all}"
        );
    }

    /// While it waits, the answer line says so and the footer offers only the cancel.
    #[test]
    fn a_waiting_sheet_says_wait() {
        let mut s = sheet(Answer::YesNo("Delete"));
        s.wait = true;
        let rows = text(&s, 40, 12);
        assert!(rows.iter().any(|r| r.contains("wait…")), "{rows:#?}");
        assert!(!rows.iter().any(|r| r.contains(" y  Delete")), "{rows:#?}");
        assert_eq!(s.keys(), [("esc", "Cancel")]);
    }
}
