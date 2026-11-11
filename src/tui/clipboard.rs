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

    /// The `redis-cli --scan` command for a pattern label, or `None` when the pattern
    /// has characters that are not safe in a shell (see [`suggested_command`]).
    pub fn suggested_command(label: &str) -> Option<Self> {
        Some(Self {
            kind: CopyKind::Command,
            text: suggested_command(label)?,
        })
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
/// --pattern "<glob>"`, or `None` when the glob holds a character outside
/// [`shell_safe`]. It carries no host or credentials.
///
/// Key names come from the server, and anyone who can write keys picks them, so pasting
/// the command must never run anything else, in any shell redis-lens runs beside: POSIX
/// shells, fish, PowerShell and cmd.exe. No single quoting works in all of them (cmd
/// has no single quotes, PowerShell also ends them at typographic quotes and has no
/// `\` escape), so instead of escaping, the command is only offered for globs made of
/// characters that are plain text inside double quotes everywhere.
pub fn suggested_command(label: &str) -> Option<String> {
    let glob = format::glob(label);
    glob.chars()
        .all(shell_safe)
        .then(|| format!("redis-cli --scan --pattern \"{glob}\""))
}

/// What the detail panel shows in place of the command when [`suggested_command`] has
/// none.
pub const NO_COMMAND: &str = "(no command: the pattern has characters that are not safe to paste into a shell; y copies the pattern)";

/// ASCII letters and digits and `_ : . / * ? { } [ ] @ # + = , ~ - |`. Inside double
/// quotes none of them expands, escapes or ends the string in POSIX shells, fish,
/// PowerShell or cmd.exe, and none is a control character. Left out on purpose, among
/// others: quotes of any kind, `$`, `` ` ``, `\`, `%`, `!`, `^`, `&`, `;`, `<`, `>`,
/// `(`, `)`, spaces and anything outside ASCII.
fn shell_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_:./*?{}[]@#+=,~-|".contains(c)
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
            suggested_command("user:{id}:cart").unwrap(),
            "redis-cli --scan --pattern \"user:*:cart\""
        );
        let t = CopyText::suggested_command("rl:{ip}:{id}").unwrap();
        assert_eq!(t.as_str(), "redis-cli --scan --pattern \"rl:*:*\"");
        assert_eq!(t.what(), "command");
        for bad in [" -a ", "--user", "--pass", " -u ", "redis://"] {
            assert!(!t.as_str().contains(bad), "{bad}");
        }
    }

    #[test]
    fn a_key_name_cannot_break_out_of_the_command() {
        assert_eq!(
            suggested_command("user:{id}:cart").as_deref(),
            Some("redis-cli --scan --pattern \"user:*:cart\"")
        );
        // POSIX command substitution, cmd.exe `&`, PowerShell typographic quotes and
        // `$(...)`, fish's `\`, a newline, `%` and `!` expansion, a double quote.
        for label in [
            "evil:x'$(touch pwned)'",
            "x&calc&",
            "x\u{2019};calc;\u{2019}",
            "x'$(calc)'",
            "a\\'b",
            "a\nrm -rf x",
            "a%PATH%",
            "a!x!",
            "a\"b",
            "a`b",
            "a b",
            "caf\u{e9}",
        ] {
            assert_eq!(suggested_command(label), None, "{label:?}");
            assert_eq!(CopyText::suggested_command(label), None, "{label:?}");
        }
    }

    proptest::proptest! {
        /// A command is offered exactly when every character of the glob is in the safe
        /// set (written out again here), and then it is the glob in double quotes.
        #[test]
        fn the_command_holds_only_safe_characters(label in "\\PC*|[\\x00-\\x7f]*|[a-z:{}*|#@\\[\\]-]*") {
            let glob = format::glob(&label);
            let safe = regex_lite_safe(&glob);
            match suggested_command(&label) {
                Some(cmd) => {
                    proptest::prop_assert!(safe);
                    proptest::prop_assert_eq!(cmd, format!("redis-cli --scan --pattern \"{glob}\""));
                }
                None => proptest::prop_assert!(!safe),
            }
        }
    }

    /// `^[A-Za-z0-9_:./*?{}\[\]@#+=,~|-]*$`, by hand.
    fn regex_lite_safe(s: &str) -> bool {
        s.bytes().all(|b| {
            matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9')
                || b"_:./*?{}[]@#+=,~|-".contains(&b)
        })
    }

    #[test]
    fn what_names_the_kind() {
        assert_eq!(CopyText::pattern("a:{id}").what(), "pattern");
        assert_eq!(CopyText::pattern("a:{id}").as_str(), "a:{id}");
        assert_eq!(CopyText::key("a:1").what(), "key");
    }
}
