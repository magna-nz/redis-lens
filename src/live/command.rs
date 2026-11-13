//! Commands as words: splitting a typed line the way `redis-cli` does, finding the
//! arguments that are credentials, and writing a command back as one masked line
//! (spec "Command line panel", rule 1).
//!
//! The command line, the Monitor panel and the Slow log's arguments all show commands
//! through [`masked_line`], so a password typed, logged or seen by `MONITOR` is never
//! drawn or kept.

use std::fmt::Write as _;

/// What a credential argument is drawn as.
pub const MASK: &str = "***";

/// Why a line could not be split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitError {
    /// A `"` or `'` is not closed.
    Unclosed,
    /// A closing quote is followed by something other than a space.
    AfterQuote,
}

impl SplitError {
    pub fn message(self) -> &'static str {
        match self {
            SplitError::Unclosed => "A quote is not closed.",
            SplitError::AfterQuote => "A closing quote must be followed by a space.",
        }
    }
}

/// Splits `line` into arguments like `redis-cli` (`sdssplitargs`): words are separated
/// by spaces; `"…"` takes the escapes `\n \r \t \b \a \\ \"` and `\xHH`; `'…'` takes
/// `\'`. A quote inside a word starts quoting there, as in `redis-cli`.
pub fn split(line: &str) -> Result<Vec<Vec<u8>>, SplitError> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && is_space(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            return Ok(out);
        }
        let mut word = Vec::new();
        let (mut dq, mut sq) = (false, false);
        loop {
            if dq {
                match b.get(i) {
                    None => return Err(SplitError::Unclosed),
                    Some(b'\\') if b.get(i + 1) == Some(&b'x') => {
                        match (b.get(i + 2).and_then(hex), b.get(i + 3).and_then(hex)) {
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
                    Some(b'\\') if i + 1 < b.len() => {
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
                    Some(b'"') => {
                        if b.get(i + 1).is_some_and(|c| !is_space(*c)) {
                            return Err(SplitError::AfterQuote);
                        }
                        i += 1;
                        break;
                    }
                    Some(&c) => {
                        word.push(c);
                        i += 1;
                    }
                }
            } else if sq {
                match b.get(i) {
                    None => return Err(SplitError::Unclosed),
                    Some(b'\\') if b.get(i + 1) == Some(&b'\'') => {
                        word.push(b'\'');
                        i += 2;
                    }
                    Some(b'\'') => {
                        if b.get(i + 1).is_some_and(|c| !is_space(*c)) {
                            return Err(SplitError::AfterQuote);
                        }
                        i += 1;
                        break;
                    }
                    Some(&c) => {
                        word.push(c);
                        i += 1;
                    }
                }
            } else {
                match b.get(i) {
                    None => break,
                    Some(&c) if is_space(c) => break,
                    Some(b'"') => {
                        dq = true;
                        i += 1;
                    }
                    Some(b'\'') => {
                        sq = true;
                        i += 1;
                    }
                    Some(&c) => {
                        word.push(c);
                        i += 1;
                    }
                }
            }
        }
        out.push(word);
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn hex(c: &u8) -> Option<u8> {
    (*c as char).to_digit(16).map(|d| d as u8)
}

/// How a credential argument is masked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secret {
    /// Shown as [`MASK`].
    Whole,
    /// An `ACL SETUSER` rule: its first character (`>`, `<`, `#` or `!`) stays.
    Rule,
}

/// For each argument of `args` (0 is the command name), whether it is a credential:
/// - `AUTH`: every argument;
/// - `HELLO … AUTH <user> <pass>` and `… SETNAME`-free forms: the two after `AUTH`;
/// - `ACL SETUSER`: rules starting `>`, `<`, `#` or `!`;
/// - `CONFIG SET`: the value after `requirepass`, `masterauth` or `masteruser`;
/// - `MIGRATE … AUTH <pass>` and `… AUTH2 <user> <pass>`;
/// - `SENTINEL SET … auth-pass|auth-user <value>` and `SENTINEL CONFIG SET` the same.
pub fn secrets(args: &[Vec<u8>]) -> Vec<Option<Secret>> {
    let mut out = vec![None; args.len()];
    let is = |i: usize, w: &str| {
        args.get(i)
            .is_some_and(|a| a.eq_ignore_ascii_case(w.as_bytes()))
    };
    let mut mark = |i: usize, s: Secret| {
        if let Some(o) = out.get_mut(i) {
            *o = Some(s);
        }
    };
    if is(0, "auth") {
        for i in 1..args.len() {
            mark(i, Secret::Whole);
        }
    } else if is(0, "hello") {
        for i in 1..args.len() {
            if is(i, "auth") {
                mark(i + 1, Secret::Whole);
                mark(i + 2, Secret::Whole);
            }
        }
    } else if is(0, "acl") && is(1, "setuser") {
        for (i, a) in args.iter().enumerate().skip(3) {
            if matches!(a.first(), Some(b'>' | b'<' | b'#' | b'!')) {
                mark(i, Secret::Rule);
            }
        }
    } else if is(0, "config") && is(1, "set") {
        let mut i = 2;
        while i < args.len() {
            if ["requirepass", "masterauth", "masteruser"]
                .iter()
                .any(|p| is(i, p))
            {
                mark(i + 1, Secret::Whole);
            }
            i += 2;
        }
    } else if is(0, "migrate") {
        for i in 1..args.len() {
            if is(i, "auth") {
                mark(i + 1, Secret::Whole);
            } else if is(i, "auth2") {
                mark(i + 1, Secret::Whole);
                mark(i + 2, Secret::Whole);
            }
        }
    } else if is(0, "sentinel") {
        for i in 1..args.len() {
            if is(i, "auth-pass") || is(i, "auth-user") {
                mark(i + 1, Secret::Whole);
            }
        }
    }
    out
}

/// `args` as one line, credentials masked ([`secrets`]), each argument bare when it is
/// plain and quoted otherwise. Control and bidi characters, and bytes that are not
/// UTF-8, are written as `\xHH`, so the line is safe to draw and [`split`] reads it
/// back to the same arguments (masked ones as the mask).
pub fn masked_line(args: &[Vec<u8>]) -> String {
    let secrets = secrets(args);
    let mut out = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match secrets[i] {
            Some(Secret::Whole) => out.push_str(MASK),
            Some(Secret::Rule) => {
                out.push(char::from(a[0]));
                out.push_str(MASK);
            }
            None => push_word(a, &mut out),
        }
    }
    out
}

/// One argument, bare or quoted (see [`masked_line`]).
pub fn push_word(a: &[u8], out: &mut String) {
    let text = std::str::from_utf8(a).ok();
    let bare = text.is_some_and(|t| {
        !t.is_empty()
            && t.chars().all(|c| {
                !c.is_whitespace()
                    && !c.is_control()
                    && !crate::value::is_bidi_control(c)
                    && c != '"'
                    && c != '\''
                    && c != '\\'
            })
    });
    if bare {
        out.push_str(text.unwrap_or_default());
        return;
    }
    out.push('"');
    match text {
        Some(t) => {
            for c in t.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c.is_control() || crate::value::is_bidi_control(c) => {
                        let mut buf = [0u8; 4];
                        for b in c.encode_utf8(&mut buf).bytes() {
                            let _ = write!(out, "\\x{b:02x}");
                        }
                    }
                    c => out.push(c),
                }
            }
        }
        None => {
            for &b in a {
                match b {
                    b'"' => out.push_str("\\\""),
                    b'\\' => out.push_str("\\\\"),
                    0x20..=0x7e => out.push(char::from(b)),
                    _ => {
                        let _ = write!(out, "\\x{b:02x}");
                    }
                }
            }
        }
    }
    out.push('"');
}

/// The commands the command line never sends because they would change its own
/// connection, with where to go instead (spec "Command line panel").
pub fn refused(args: &[Vec<u8>]) -> Option<&'static str> {
    let name = String::from_utf8_lossy(args.first()?).to_ascii_lowercase();
    let sub = args
        .get(1)
        .map(|a| String::from_utf8_lossy(a).to_ascii_lowercase());
    Some(match name.as_str() {
        "subscribe" | "psubscribe" | "ssubscribe" | "unsubscribe" | "punsubscribe"
        | "sunsubscribe" => "Not sent here: use Pub/Sub (5) to subscribe.",
        "monitor" => "Not sent here: use the Monitor panel in Server (3).",
        "select" => "Not sent here: SELECT would switch the database under every view.",
        "auth" | "hello" | "reset" | "quit" => {
            "Not sent: it would change the connection every view shares."
        }
        "client" if sub.as_deref() == Some("reply") => {
            "Not sent: it would change the connection every view shares."
        }
        _ => return None,
    })
}

/// A Monitor line split into its parts: `<time> [<db> <client>] "<arg>" "<arg>" …`.
#[derive(Clone, PartialEq)]
pub struct MonitorLine {
    /// Seconds since the epoch, as the server wrote it.
    pub time: String,
    pub db: String,
    /// `ip:port`, `unix:/path` or `lua`.
    pub client: String,
    pub args: Vec<Vec<u8>>,
}

impl std::fmt::Debug for MonitorLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Arguments are values: only their count.
        f.debug_struct("MonitorLine")
            .field("time", &self.time)
            .field("db", &self.db)
            .field("args", &self.args.len())
            .finish_non_exhaustive()
    }
}

/// Reads one `MONITOR` line. `None` for anything else (`OK`, a cut line).
pub fn parse_monitor(line: &str) -> Option<MonitorLine> {
    let (time, rest) = line.split_once(' ')?;
    let rest = rest.strip_prefix('[')?;
    let (inside, args) = rest.split_once("] ")?;
    let (db, client) = inside.split_once(' ')?;
    if time.is_empty() || !time.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    Some(MonitorLine {
        time: time.to_string(),
        db: db.to_string(),
        client: client.to_string(),
        args: split(args).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn words(s: &[&str]) -> Vec<Vec<u8>> {
        s.iter().map(|w| w.as_bytes().to_vec()).collect()
    }

    #[test]
    fn splits_like_redis_cli() {
        assert_eq!(split("GET  key").unwrap(), words(&["GET", "key"]));
        assert_eq!(
            split(r#"SET "a b" 'c d' "\x41\n""#).unwrap(),
            vec![
                b"SET".to_vec(),
                b"a b".to_vec(),
                b"c d".to_vec(),
                b"A\n".to_vec()
            ]
        );
        assert_eq!(split(r#"x 'it\'s'"#).unwrap(), words(&["x", "it's"]));
        assert_eq!(split("").unwrap(), Vec::<Vec<u8>>::new());
        assert_eq!(split(r#"GET "unclosed"#), Err(SplitError::Unclosed));
        assert_eq!(split(r#"GET "a"b"#), Err(SplitError::AfterQuote));
        assert_eq!(
            split(r#"GET """#).unwrap(),
            vec![b"GET".to_vec(), Vec::new()]
        );
    }

    #[test]
    fn masks_every_credential_form() {
        let cases = [
            ("AUTH s3cret", "AUTH ***"),
            ("auth bob s3cret", "auth *** ***"),
            (
                "HELLO 3 AUTH bob s3cret SETNAME x",
                "HELLO 3 AUTH *** *** SETNAME x",
            ),
            (
                "ACL SETUSER bob on >s3cret <old #abc !def ~* +@read",
                "ACL SETUSER bob on >*** <*** #*** !*** ~* +@read",
            ),
            (
                "CONFIG SET requirepass s3cret",
                "CONFIG SET requirepass ***",
            ),
            (
                "config set maxmemory 1gb MasterAuth s3cret masteruser bob",
                "config set maxmemory 1gb MasterAuth *** masteruser ***",
            ),
            (
                "MIGRATE h 6379 \"\" 0 5000 AUTH s3cret KEYS a",
                "MIGRATE h 6379 \"\" 0 5000 AUTH *** KEYS a",
            ),
            (
                "MIGRATE h 6379 k 0 5000 AUTH2 bob s3cret",
                "MIGRATE h 6379 k 0 5000 AUTH2 *** ***",
            ),
            (
                "SENTINEL SET m auth-pass s3cret",
                "SENTINEL SET m auth-pass ***",
            ),
            ("GET s3cret", "GET s3cret"),
        ];
        for (typed, shown) in cases {
            assert_eq!(masked_line(&split(typed).unwrap()), shown, "{typed}");
        }
    }

    #[test]
    fn a_trailing_auth_with_nothing_after_it_is_fine() {
        assert_eq!(masked_line(&split("MIGRATE AUTH").unwrap()), "MIGRATE AUTH");
        assert_eq!(
            masked_line(&split("CONFIG SET requirepass").unwrap()),
            "CONFIG SET requirepass"
        );
    }

    #[test]
    fn the_line_is_safe_to_draw() {
        let args = vec![
            b"SET".to_vec(),
            "a\u{1b}[31m\u{202e}b".as_bytes().to_vec(),
            vec![0xff, 0x00, b'"'],
        ];
        let line = masked_line(&args);
        assert!(
            !line
                .chars()
                .any(|c| c.is_control() || crate::value::is_bidi_control(c)),
            "{line:?}"
        );
        assert_eq!(split(&line).unwrap(), args);
    }

    #[test]
    fn refusals_point_somewhere() {
        assert!(
            refused(&words(&["subscribe", "c"]))
                .unwrap()
                .contains("Pub/Sub")
        );
        assert!(refused(&words(&["MONITOR"])).unwrap().contains("Monitor"));
        assert!(
            refused(&words(&["select", "1"]))
                .unwrap()
                .contains("SELECT")
        );
        assert!(refused(&words(&["CLIENT", "REPLY", "OFF"])).is_some());
        assert!(refused(&words(&["CLIENT", "LIST"])).is_none());
        assert!(refused(&words(&["GET", "k"])).is_none());
    }

    #[test]
    fn reads_monitor_lines() {
        let l = parse_monitor(r#"1700000000.123456 [0 127.0.0.1:52344] "SET" "user:1" "a\"b""#)
            .unwrap();
        assert_eq!(l.time, "1700000000.123456");
        assert_eq!((l.db.as_str(), l.client.as_str()), ("0", "127.0.0.1:52344"));
        assert_eq!(l.args, words(&["SET", "user:1", "a\"b"]));
        let l = parse_monitor(r#"1.5 [0 lua] "get" "k""#).unwrap();
        assert_eq!(l.client, "lua");
        assert!(parse_monitor("OK").is_none());
        assert!(parse_monitor("1.5 [0 x] \"cut").is_none());
        let dbg = format!(
            "{:?}",
            parse_monitor(r#"1.5 [0 a] "AUTH" "s3cr3t""#).unwrap()
        );
        assert!(!dbg.contains("s3cr3t"), "{dbg}");
    }

    proptest! {
        #[test]
        fn masked_lines_read_back(args in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..12), 1..6)) {
            let mut args = args;
            // Keep the first word plain so no credential rule masks anything.
            args[0] = b"x".to_vec();
            let line = masked_line(&args);
            prop_assert_eq!(split(&line).unwrap(), args);
            prop_assert!(!line.chars().any(|c| c.is_control()));
        }

        #[test]
        fn split_never_panics(s in "\\PC{0,40}") {
            let _ = split(&s);
        }

        #[test]
        fn a_password_never_shows(pass in "[a-zA-Z0-9]{8,16}") {
            for typed in [
                format!("AUTH {pass}"),
                format!("auth u {pass}"),
                format!("ACL SETUSER u on >{pass}"),
                format!("CONFIG SET requirepass {pass}"),
                format!("MIGRATE h 1 k 0 9 AUTH2 u {pass}"),
                format!("HELLO 3 AUTH u {pass}"),
            ] {
                let line = masked_line(&split(&typed).unwrap());
                prop_assert!(!line.contains(&pass), "{}", line);
            }
        }
    }
}
