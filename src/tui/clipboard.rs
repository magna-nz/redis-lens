//! Copying to the system clipboard through the terminal's OSC 52 escape sequence.
//!
//! What may be copied is restricted by type: see [`CopyText`].

use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::format;

/// The longest OSC 52 sequence, in bytes, that is written. Many terminals drop
/// longer ones.
pub const MAX_OSC52_BYTES: usize = 100_000;

/// What kind of text a [`CopyText`] holds, for the notice ("Copied pattern").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKind {
    Pattern,
    Key,
    Command,
}

impl CopyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pattern => "pattern",
            Self::Key => "key",
            Self::Command => "command",
        }
    }
}

/// Text that may go to the clipboard.
///
/// There is deliberately no constructor from an arbitrary `String`, a profile, a
/// connection string or credentials: only a pattern name, a key name or the
/// suggested `redis-cli --scan` command can be built, so nothing credential-like
/// can reach the clipboard (CLAUDE.md rule 1). Keep it that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyText {
    kind: CopyKind,
    text: String,
}

impl CopyText {
    /// A key pattern such as `user:{id}:cart`.
    pub fn pattern(name: &str) -> Self {
        Self {
            kind: CopyKind::Pattern,
            text: name.to_string(),
        }
    }

    /// A key name from the keyspace.
    pub fn key(name: &str) -> Self {
        Self {
            kind: CopyKind::Key,
            text: name.to_string(),
        }
    }

    /// The `redis-cli --scan` command for a pattern label.
    pub fn suggested_command(label: &str) -> Self {
        Self {
            kind: CopyKind::Command,
            text: suggested_command(label),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// What it is: "pattern", "key" or "command".
    pub fn what(&self) -> &'static str {
        self.kind.as_str()
    }
}

/// The command shown in the detail panel and copied by `Y`: `redis-cli --scan
/// --pattern` with the pattern's glob as one shell word. It carries no host or
/// credentials.
///
/// Key names come from the server, and anyone who can write keys picks them, so pasting
/// the command must never run anything else. The glob goes in single quotes, where a
/// POSIX shell treats nothing as special except `'` itself, written as `'\''`. Control
/// characters (a newline would also break the line on screen) and `\` (an escape inside
/// single quotes in fish) become the glob wildcard `?`, which still matches them.
pub fn suggested_command(label: &str) -> String {
    format!(
        "redis-cli --scan --pattern {}",
        shell_quote(&safe_glob(label))
    )
}

/// [`format::glob`] with control characters and `\` replaced by `?`.
fn safe_glob(label: &str) -> String {
    format::glob(label)
        .chars()
        .map(|c| if c.is_control() || c == '\\' { '?' } else { c })
        .collect()
}

/// `s` as one single-quoted POSIX shell word.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The OSC 52 "set clipboard" sequence for `t`, or `None` when it would be longer
/// than [`MAX_OSC52_BYTES`].
pub fn osc52(t: &CopyText) -> Option<String> {
    const PREFIX: &str = "\x1b]52;c;";
    const SUFFIX: &str = "\x07";
    let encoded_len = t.as_str().len().div_ceil(3) * 4;
    if PREFIX.len() + encoded_len + SUFFIX.len() > MAX_OSC52_BYTES {
        return None;
    }
    Some(format!("{PREFIX}{}{SUFFIX}", STANDARD.encode(t.as_str())))
}

/// Write the sequence and flush. `Ok(false)` when it is over the cap; nothing is
/// written then.
pub fn write_osc52(w: &mut impl Write, t: &CopyText) -> std::io::Result<bool> {
    let Some(seq) = osc52(t) else {
        return Ok(false);
    };
    w.write_all(seq.as_bytes())?;
    w.flush()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(seq: &str) -> Vec<u8> {
        let b64 = seq
            .strip_prefix("\x1b]52;c;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        STANDARD.decode(b64).unwrap()
    }

    #[test]
    fn ascii_exact_bytes() {
        let seq = osc52(&CopyText::key("user:1")).unwrap();
        assert_eq!(seq, "\x1b]52;c;dXNlcjox\x07");
        assert_eq!(decode(&seq), b"user:1");
    }

    #[test]
    fn utf8_round_trips() {
        let t = CopyText::key("café:日本");
        let seq = osc52(&t).unwrap();
        assert_eq!(decode(&seq), "café:日本".as_bytes());
        assert_eq!(
            seq,
            format!("\x1b]52;c;{}\x07", STANDARD.encode("café:日本"))
        );
    }

    #[test]
    fn cap_boundary() {
        // 8 bytes of framing; base64 is 4 bytes per 3 of input.
        let max_input = (MAX_OSC52_BYTES - 8) / 4 * 3;
        let ok = CopyText::key(&"a".repeat(max_input));
        let seq = osc52(&ok).unwrap();
        assert!(seq.len() <= MAX_OSC52_BYTES);
        assert_eq!(seq.len(), MAX_OSC52_BYTES);
        let too_long = CopyText::key(&"a".repeat(max_input + 1));
        assert!(osc52(&too_long).is_none());
    }

    #[test]
    fn write_respects_the_cap() {
        let mut out = Vec::new();
        assert!(write_osc52(&mut out, &CopyText::key("k")).unwrap());
        assert_eq!(out, b"\x1b]52;c;aw==\x07");
        let mut none = Vec::new();
        let big = CopyText::key(&"a".repeat(MAX_OSC52_BYTES));
        assert!(!write_osc52(&mut none, &big).unwrap());
        assert!(none.is_empty());
    }

    #[test]
    fn control_characters_stay_inside_the_base64() {
        let t = CopyText::key("evil\x1b]52;c;AAAA\x07\x1b\\\r\n\0");
        let seq = osc52(&t).unwrap();
        assert_eq!(seq.matches('\x1b').count(), 1);
        assert!(seq.starts_with('\x1b'));
        assert!(seq.ends_with('\x07'));
        assert_eq!(seq.matches('\x07').count(), 1);
        assert!(!seq.contains(['\r', '\n', '\0']));
        assert_eq!(decode(&seq), t.as_str().as_bytes());
    }

    #[test]
    fn suggested_command_format() {
        assert_eq!(
            suggested_command("user:{id}:cart"),
            "redis-cli --scan --pattern 'user:*:cart'"
        );
        let t = CopyText::suggested_command("rl:{ip}:{id}");
        assert_eq!(t.as_str(), "redis-cli --scan --pattern 'rl:*:*'");
        assert_eq!(t.what(), "command");
        for bad in [" -a ", "--user", "--pass", " -u ", "redis://"] {
            assert!(!t.as_str().contains(bad), "{bad}");
        }
    }

    /// Reads one POSIX shell word made of single-quoted runs and `\'`, as
    /// [`shell_quote`] writes it. `None` for anything else, e.g. an unquoted character
    /// or an unterminated quote. Also `None` when a `\` appears inside quotes, since
    /// fish would read it as an escape.
    fn unquote(word: &str) -> Option<String> {
        let mut out = String::new();
        let mut it = word.chars();
        while let Some(c) = it.next() {
            match c {
                '\'' => loop {
                    match it.next()? {
                        '\'' => break,
                        '\\' => return None,
                        c => out.push(c),
                    }
                },
                '\\' => out.push(it.next().filter(|c| *c == '\'')?),
                _ => return None,
            }
        }
        Some(out)
    }

    fn quoted_glob(cmd: &str) -> Option<String> {
        unquote(cmd.strip_prefix("redis-cli --scan --pattern ")?)
    }

    #[test]
    fn a_key_name_cannot_break_out_of_the_command() {
        let cmd = suggested_command("evil:x'$(touch pwned)'");
        assert_eq!(
            cmd,
            r"redis-cli --scan --pattern 'evil:x'\''$(touch pwned)'\'''"
        );
        assert_eq!(quoted_glob(&cmd).unwrap(), "evil:x'$(touch pwned)'");
        // A newline and fish's escape character become `?`.
        assert_eq!(
            suggested_command("a\nb\\'c\x1b"),
            r"redis-cli --scan --pattern 'a?b?'\''c?'"
        );
    }

    proptest::proptest! {
        /// Whatever the label, the command is `redis-cli --scan --pattern` plus exactly
        /// one shell word, and that word is the label's glob (control characters and
        /// `\` as `?`).
        #[test]
        fn the_pattern_is_always_one_quoted_word(label in "\\PC*|[\\x00-\\x7f]*") {
            let cmd = suggested_command(&label);
            let want: String = format::glob(&label)
                .chars()
                .map(|c| if c.is_control() || c == '\\' { '?' } else { c })
                .collect();
            proptest::prop_assert_eq!(quoted_glob(&cmd), Some(want));
            proptest::prop_assert!(!cmd.chars().any(char::is_control));
        }
    }

    #[test]
    fn what_names_the_kind() {
        assert_eq!(CopyText::pattern("a:{id}").what(), "pattern");
        assert_eq!(CopyText::pattern("a:{id}").as_str(), "a:{id}");
        assert_eq!(CopyText::key("a:1").what(), "key");
    }
}
