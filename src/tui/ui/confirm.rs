//! The write gate's question ([`crate::tui::app::confirm`]), drawn as the one confirm
//! sheet ([`super::sheet`]): the question in its top border with the profile and its
//! environment at the right, what it acts on, what is sent, whether it can be undone,
//! and the answer. What each question asks for (typing the name on prod, `y`/`n`
//! elsewhere) is the gate's, not drawn here.

use super::sheet::{Answer, Sheet, right_of};
use super::style;
use crate::tui::app::confirm::{Confirm, Expect};

/// The title as a question: "Delete key" is "Delete this key?".
fn question(title: &str) -> String {
    let q = match title {
        "Delete key" => "Delete this key?".to_string(),
        "Delete item" => "Delete this item?".to_string(),
        "Edit value" => "Save this change?".to_string(),
        "Add item" => "Add this item?".to_string(),
        "Add key" => "Add this key?".to_string(),
        "Rename key" => "Rename this key?".to_string(),
        "Set TTL" => "Set this TTL?".to_string(),
        "Kill connection" => "Close this connection?".to_string(),
        t => format!("{t}?"),
    };
    q.replace("??", "?")
}

/// Whether a line says if the change can be undone: it goes after `Sends`.
fn is_undo(line: &str) -> bool {
    line.contains("can't be undone") || line.contains("undoes it")
}

/// The sheet for `c`: what it acts on, `Sends   <command>`, whether it can be undone,
/// then the answer the gate wants. The text is the question's own ([`Confirm::lines`]).
pub fn sheet(c: &Confirm) -> Sheet {
    let mut lines = Vec::new();
    let mut sends = None;
    let mut undo = Vec::new();
    for text in c.lines() {
        if text.is_empty() || text.starts_with("Type ") {
            continue;
        }
        if let Some(cmd) = text.strip_prefix("Sends: ") {
            sends = Some(("Sends", cmd.to_string(), style::fgb()));
        } else if is_undo(&text) {
            // "This can't be undone." is the danger colour; "… undoes it" is plain.
            let st = if text.contains("can't be undone") {
                style::danger()
            } else {
                style::fg()
            };
            undo.push(("", text, st));
        } else {
            lines.push(("", text, style::fg()));
        }
    }
    lines.extend(sends);
    lines.extend(undo);
    let verb = c.keys().first().map_or("Send", |k| k.1);
    let answer = match &c.expect {
        Expect::YesNo => Answer::YesNo(verb),
        Expect::Type(name) => Answer::Type {
            name: name.clone(),
            typed: c.typed.clone(),
            verb,
        },
    };
    Sheet {
        title: question(&c.title()),
        right: right_of(&c.profile, c.env),
        lines,
        answer,
        wait: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_become_questions() {
        assert_eq!(question("Delete key"), "Delete this key?");
        assert_eq!(
            question("Set a TTL on every key of the pattern"),
            "Set a TTL on every key of the pattern?"
        );
        assert_eq!(
            question("Turn on writes for prod?"),
            "Turn on writes for prod?"
        );
    }

    #[test]
    fn a_long_name_is_split_not_cut() {
        use super::super::analysis::wrap_hard;
        use super::super::widgets::width;
        let name = "user:".to_string() + &"x".repeat(30);
        let lines = wrap_hard(&name, 12);
        assert!(lines.iter().all(|l| width(l) <= 12), "{lines:?}");
        assert_eq!(lines.concat(), name);
        assert_eq!(wrap_hard("", 4), vec![String::new()]);
        assert_eq!(wrap_hard("ab cd ef", 5), vec!["ab cd", "ef"]);
    }
}
