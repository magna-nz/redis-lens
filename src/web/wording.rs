//! The dashboard's own words for texts that the TUI and the CLI share with it.
//!
//! The write gate, the command line's refusals and `conn add`/`conn set` say what to do
//! in their own terms: a TUI key (`w on 0 Connections`, `Press b`), a screen number
//! (`Pub/Sub (5)`) or a CLI flag (`pass --force`). The page has none of those, so every
//! text a reply carries goes through [`web`] first. It maps the known sentences to the
//! dashboard's terms, and anything it still finds a TUI key or a flag in becomes
//! [`FALLBACK`]. What the TUI and the CLI say is not changed.
//!
//! [`server_free`] does the same for a server's or the client library's error text,
//! which the runtime sometimes passes on as a string (a connect that failed, a tail or
//! Monitor connection that could not open): it can hold the server's own words (rule 3),
//! so it becomes a fixed sentence.

/// What a text becomes when it still names a TUI key or a CLI flag after [`web`].
pub const FALLBACK: &str = "That could not be done as asked. Check the form and try again.";

/// Why a connect failed: the server wants a password, and none was sent.
pub const NEEDS_PASSWORD: &str = "the server asks for a password, and this connection has none: add the connection again with its password";
/// Why a connect failed: the server refused the user name or password.
pub const BAD_CREDENTIALS: &str =
    "the server refused the credentials: check the user name and password";
/// Why a connect failed when the server or the client library said why in its own
/// words.
pub const SERVER_REFUSED: &str = "the server refused the connection";

/// The sentences of the TUI and the CLI, and what the dashboard says instead. Applied in
/// order, as plain substrings.
const SENTENCES: &[(&str, &str)] = &[
    // The write gate (`write::read_only_reason`).
    (
        "Writes are turned on with w on 0 Connections.",
        "Turn writes on in Connections.",
    ),
    // The command line's refusals (`live::command::refused`).
    (
        "use Pub/Sub (5) to subscribe.",
        "use the Pub/Sub screen to subscribe.",
    ),
    (
        "use the Monitor panel in Server (3).",
        "use Monitor on the Server screen.",
    ),
    (
        "SELECT would switch the database under every view. Press b to switch it.",
        "SELECT would switch the database under every view.",
    ),
    (
        "connect first (0 Connections, then enter)",
        "connect first in Connections",
    ),
    // Cluster nodes (`live::nodes`): the dashboard connects again, it does not rescan.
    (
        "rescan to read the cluster's slots again",
        "connect again to read the cluster's slots",
    ),
    ("; rescan to connect again", "; connect again"),
    ("; rescan", "; connect again"),
    // `conn add` and `conn set`.
    (
        "; pass --force to replace it",
        ": tick Replace to overwrite it",
    ),
    ("pass --force to replace it", "tick Replace to overwrite it"),
    (
        "the name given with --confirm does not match",
        "the name typed does not match",
    ),
    (
        "nothing to change: give --writes, --tag, --untag, --standalone or --no-standalone",
        "nothing to change: give writes, a tag to add or remove, or standalone",
    ),
    (
        "the URL has a username but no password; pass --password-stdin or --ask-password",
        "the URL has a user name but no password: fill in the password",
    ),
    (
        "a token command replaces the password; leave out --password-stdin and --ask-password",
        "a token command replaces the password: leave the password empty",
    ),
    (
        "--password-stdin was given but nothing was read",
        "the password is empty",
    ),
    (
        "--sentinel-password-stdin was given but nothing was read",
        "the Sentinel password is empty",
    ),
    (
        "--entra and a token command are two ways to sign in; give one of them",
        "Entra ID and a token command are two ways to sign in: pick one of them",
    ),
    (
        "--confirm answers the question --writes on asks; give --writes on too",
        "the typed name answers the question turning writes on asks",
    ),
    (
        "; add it again with `redis-lens conn add <name> --url <url> -- <command>`",
        ": add it again with its token command",
    ),
    (
        "; run `redis-lens conn add` again",
        ": add the connection again",
    ),
    ("; rebuild with --features entra-id", ""),
    (
        "These need `conn add <name> --force ...` to store them:",
        "Add them again with Replace ticked to store them:",
    ),
    (" (--yes is not enough on a prod profile)", ""),
    ("--standalone conflicts with", "Standalone conflicts with"),
    // The question `conn set` and `conn add` ask before writes go on.
    (" [y/N] ", ""),
    ("[y/N]", ""),
    (
        "Type its name to turn writes on: ",
        "Type its name to turn writes on.",
    ),
];

/// CLI flags that are left in a sentence [`SENTENCES`] does not know, and the form
/// field they are on the page. Longest first, so `--ssh-identity` is not read as
/// `--ssh`.
const FLAGS: &[(&str, &str)] = &[
    ("--sentinel-password-stdin", "Sentinel password"),
    ("--sentinel-ask-password", "Sentinel password"),
    ("--sentinel-master", "Sentinel master"),
    ("--sentinel-user", "Sentinel user"),
    ("--sentinel-tls", "Sentinel TLS"),
    ("--sentinel-*", "Sentinel"),
    ("--sentinel", "Sentinels"),
    ("--ssh-known-hosts", "SSH known hosts"),
    ("--ssh-identity", "SSH identity"),
    ("--ssh", "SSH"),
    ("--password-stdin", "password"),
    ("--ask-password", "password"),
    ("--prefer-replica", "Prefer replica"),
    ("--no-standalone", "Standalone off"),
    ("--standalone", "Standalone"),
    ("--provider", "Provider"),
    ("--writes", "Writes"),
    ("--untag", "Remove tag"),
    ("--tag", "Tag"),
    ("--url", "URL"),
    ("--env", "Environment"),
    ("--entra", "Entra ID"),
    ("--force", "Replace"),
    ("--confirm", "the typed name"),
    ("--yes", "yes"),
];

/// What a text the page shows must not hold: a TUI key, a screen number or a CLI flag.
pub const BANNED: &[&str] = &[
    "0 Connections",
    "(5)",
    "(3)",
    "Press ",
    "--",
    "[y/N]",
    " w on ",
    " w to ",
    " esc ",
    "esc to",
    "ctrl-",
];

/// Whether `text` holds `pattern` where a flag or a key would be: `--` and `ctrl-`
/// count at the start of the text or of a word (after a space, a bracket, a quote or a
/// backtick), and `--` only before a letter, so a name or host such as `cache--eu` or
/// `xn--bcher-kva.example` is not one. The other patterns count anywhere.
fn holds(text: &str, pattern: &str) -> bool {
    if pattern != "--" && pattern != "ctrl-" {
        return text.contains(pattern);
    }
    text.match_indices(pattern).any(|(i, m)| {
        let before = text[..i].chars().next_back();
        let starts_word = before.is_none_or(|c| c.is_whitespace() || "([{\"'`".contains(c));
        let after = text[i + m.len()..].chars().next();
        starts_word && (pattern != "--" || after.is_some_and(|c| c.is_ascii_lowercase()))
    })
}

/// The first [`BANNED`] pattern `text` holds.
pub fn banned(text: &str) -> Option<&'static str> {
    BANNED.iter().copied().find(|p| holds(text, p))
}

/// `text` with [`SENTENCES`] and [`FLAGS`] mapped, before the [`BANNED`] check.
fn mapped(text: &str) -> String {
    let mut t = text.to_string();
    for (from, to) in SENTENCES {
        if t.contains(from) {
            t = t.replace(from, to);
        }
    }
    for (flag, field) in FLAGS {
        if t.contains(flag) {
            t = t.replace(flag, field);
        }
    }
    t.trim_end().to_string()
}

/// `text` in the dashboard's terms: see the module docs.
pub fn web(text: &str) -> String {
    let t = mapped(text);
    if banned(&t).is_some() {
        FALLBACK.to_string()
    } else {
        t
    }
}

/// Where a piece of user data stood in a text while the rest is mapped and checked.
const HOLE: (char, char) = ('\u{E000}', '\u{E001}');

/// `text` with every piece of `data` (three characters or more) taken out where it
/// stands as a whole word, and the pieces in the order of their holes.
fn take_out(text: &str, data: &[&str]) -> (String, Vec<String>) {
    let mut data: Vec<&str> = data
        .iter()
        .copied()
        .filter(|d| d.chars().count() >= 3)
        .collect();
    // Longest first, so a name that holds another is taken out whole.
    data.sort_by_key(|d| std::cmp::Reverse(d.len()));
    data.dedup();
    let mut t = text.to_string();
    let mut kept = Vec::new();
    for d in data {
        let hole = format!("{}{}{}", HOLE.0, kept.len(), HOLE.1);
        let mut out = String::with_capacity(t.len());
        let mut rest = t.as_str();
        let mut used = false;
        while let Some(i) = rest.find(d) {
            let before = rest[..i].chars().next_back();
            let after = rest[i + d.len()..].chars().next();
            let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric());
            out.push_str(&rest[..i]);
            if word(before) || word(after) {
                out.push_str(d);
            } else {
                out.push_str(&hole);
                used = true;
            }
            rest = &rest[i + d.len()..];
        }
        out.push_str(rest);
        t = out;
        if used {
            kept.push(d.to_string());
        } else {
            // No hole was made: the number is free for the next piece.
            continue;
        }
    }
    (t, kept)
}

/// `template` with its holes filled from `kept`.
fn put_back(template: &str, kept: &[String]) -> String {
    let mut t = template.to_string();
    for (i, d) in kept.iter().enumerate() {
        t = t.replace(&format!("{}{i}{}", HOLE.0, HOLE.1), d);
    }
    t
}

/// [`web`] of a text that holds user data (a profile name, a host, a tag): the mapping
/// and the [`BANNED`] check apply to the fixed words only, so a name such as `cache--eu`
/// or a host such as `xn--bcher-kva.example` never turns the text into [`FALLBACK`].
pub fn web_with(text: &str, data: &[&str]) -> String {
    let whole = web(text);
    if whole != FALLBACK || text == FALLBACK {
        return whole;
    }
    let (template, kept) = take_out(text, data);
    let t = web(&template);
    if t == FALLBACK {
        t
    } else {
        put_back(&t, &kept)
    }
}

/// A confirm question in the dashboard's terms, with `data` kept as [`web_with`] keeps
/// it. `None` when its fixed words still name a TUI key or a flag: the question is never
/// replaced by [`FALLBACK`] (a yes to a question that does not say what yes does), the
/// request fails instead with [`NO_QUESTION`].
pub fn question(text: &str, data: &[&str]) -> Option<String> {
    let whole = mapped(text);
    if banned(&whole).is_none() {
        return Some(whole);
    }
    let (template, kept) = take_out(text, data);
    let t = mapped(&template);
    banned(&t).is_none().then(|| put_back(&t, &kept))
}

/// Why a request that needed a question failed: the question could not be put in the
/// dashboard's terms ([`question`]). Nothing was changed.
pub const NO_QUESTION: &str =
    "That needs a question the dashboard cannot ask, so nothing was changed.";

/// [`web`] of an optional text.
pub fn web_opt(text: Option<&str>) -> Option<String> {
    text.map(web)
}

/// The error codes a Redis or Valkey server starts an error reply with, as the client
/// library shows them (`NOAUTH: ...`, `"WRONGPASS": ...`, `ResponseError: ...`). Any
/// other leading all-capitals word counts too ([`leading_code`]); these are the ones
/// that also count after a `: ` inside a longer text.
const SERVER_CODES: &[&str] = &[
    "ERR",
    "NOAUTH",
    "WRONGPASS",
    "NOPERM",
    "NOPROTO",
    "WRONGTYPE",
    "LOADING",
    "BUSY",
    "BUSYKEY",
    "BUSYGROUP",
    "NOGROUP",
    "MASTERDOWN",
    "MISCONF",
    "DENIED",
    "READONLY",
    "NOSCRIPT",
    "EXECABORT",
    "CLUSTERDOWN",
    "CROSSSLOT",
    "TRYAGAIN",
    "MOVED",
    "ASK",
    "NOREPLICAS",
    "NOQUORUM",
    "NOTBUSY",
    "NOSUB",
    "UNKILLABLE",
    "UNBLOCKED",
    "INPROG",
    "IOERR",
    "OOM",
];

/// All-capitals words that start redis-lens's own sentences (a command it names, a
/// protocol), so they are not read as a server's error code at the start of a text.
/// None of them is a code a server answers with ([`SERVER_CODES`] is checked first).
const OWN_STARTS: &[&str] = &[
    "ACL",
    "AUTH",
    "CLIENT",
    "CLUSTER",
    "COMMAND",
    "CONFIG",
    "DBSIZE",
    "DEL",
    "DNS",
    "EVAL",
    "GET",
    "HELLO",
    "INFO",
    "KEYS",
    "LATENCY",
    "MEMORY",
    "MIGRATE",
    "MONITOR",
    "MULTI",
    "OBJECT",
    "PERSIST",
    "PING",
    "PSUBSCRIBE",
    "PUBLISH",
    "PUBSUB",
    "SCAN",
    "SELECT",
    "SENTINEL",
    "SET",
    "SLOWLOG",
    "SSH",
    "SSUBSCRIBE",
    "SUBSCRIBE",
    "TCP",
    "TLS",
    "TTL",
    "TYPE",
    "URL",
    "WATCH",
    "XINFO",
];

/// The client library's names for the kinds of error it reports, as its `Display`
/// writes them: a server reply's kind (`BusyLoading: detail`), and its own kinds after
/// a description (`... - AuthenticationFailed`, `... - Server(ResponseError)`). Both the
/// redis-rs 1.x names and the older ones.
const LIBRARY_KINDS: &[&str] = &[
    // redis::ServerErrorKind (Debug), redis-rs 1.x.
    "ResponseError",
    "ExecAbort",
    "BusyLoading",
    "NoScript",
    "Moved",
    "Ask",
    "TryAgain",
    "ClusterDown",
    "CrossSlot",
    "MasterDown",
    "ReadOnly",
    "NotBusy",
    "NoSub",
    "NoPerm",
    // redis::ErrorKind (Debug), redis-rs 1.x.
    "Parse",
    "AuthenticationFailed",
    "UnexpectedReturnType",
    "InvalidClientConfig",
    "Io",
    "Client",
    "Extension",
    "MasterNameNotFoundBySentinel",
    "NoValidReplicasFoundBySentinel",
    "EmptySentinelList",
    "ClusterConnectionNotFound",
    "Server",
    "Serialize",
    "RESP3NotSupported",
    // Older names.
    "ExtensionError",
    "TypeError",
    "ExecAbortError",
    "BusyLoadingError",
    "NoScriptError",
    "ClientError",
    "ParseError",
    "IoError",
    "NoAuth",
    "WrongPass",
    "Unexpected",
];

/// The all-capitals word `text` starts with (after a quote or a `-`), when it is
/// followed by a space, a colon, a quote or nothing: how a server's error reply starts,
/// raw (`MISCONF ...`) or as the library shows an unknown code (`"MISCONF": ...`).
fn leading_code(text: &str) -> Option<&str> {
    let t = text.trim_start().trim_start_matches(['"', '-']);
    let end = t
        .find(|c: char| !(c.is_ascii_uppercase() || c == '_'))
        .unwrap_or(t.len());
    let word = &t[..end];
    let rest = &t[end..];
    let ends_well = rest.is_empty() || rest.starts_with([' ', ':', '"']);
    (word.len() >= 2 && ends_well).then_some(word)
}

/// Whether `text` holds a library kind name in the forms its `Display` writes:
/// ` - Kind`, ` - Kind: `, ` - Kind(`, or `Kind: ` at a word's start.
fn holds_kind(text: &str) -> bool {
    LIBRARY_KINDS.iter().any(|k| {
        let after_dash = format!(" - {k}");
        let dash = text.match_indices(&after_dash).any(|(i, m)| {
            let rest = &text[i + m.len()..];
            rest.is_empty() || rest.starts_with([':', '(', ' ', ')', ']', ','])
        });
        let colon = format!("{k}: ");
        let leading = text.match_indices(&colon).any(|(i, _)| {
            text[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        });
        dash || leading
    })
}

/// Whether `text` holds a quoted all-capitals code followed by a colon anywhere
/// (`... failed: "MISCONF": ...`): how the library shows a code it does not know.
fn holds_quoted_code(text: &str) -> bool {
    text.match_indices('"').any(|(i, _)| {
        let rest = &text[i + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_uppercase() || c == '_'))
            .unwrap_or(rest.len());
        end >= 2 && rest[end..].starts_with("\":")
    })
}

/// Whether `text` reads as a server's (or the client library's) error text rather than
/// redis-lens's own sentence.
fn from_server(text: &str) -> bool {
    let leading = leading_code(text)
        .is_some_and(|w| SERVER_CODES.contains(&w) || (is_code(w) && !OWN_STARTS.contains(&w)));
    leading
        || text.contains("An error was signalled by the server")
        || holds_kind(text)
        || holds_quoted_code(text)
        || SERVER_CODES
            .iter()
            .any(|c| text.contains(&format!(": {c} ")) || text.contains(&format!("\"{c}\"")))
}

/// [`server_free`] of a text that holds user data, kept as [`web_with`] keeps it.
pub fn server_free_with(text: &str, data: &[&str]) -> String {
    if from_server(text) {
        // A name such as `ERR-cache` is not a server's code: decide on the fixed words.
        let (template, _) = take_out(text, data);
        if from_server(&template) {
            return server_free(text);
        }
    }
    web_with(text, data)
}

/// `text` with any server or client-library error text it holds replaced by a fixed
/// sentence (rule 3), then in the dashboard's terms ([`web`]).
pub fn server_free(text: &str) -> String {
    if !from_server(text) {
        return web(text);
    }
    let auth = ["NOAUTH", "NoAuth", "Authentication required"];
    let creds = [
        "WRONGPASS",
        "WrongPass",
        "AuthenticationFailed",
        "invalid username-password",
    ];
    if auth.iter().any(|a| text.contains(a)) {
        NEEDS_PASSWORD.to_string()
    } else if creds.iter().any(|a| text.contains(a)) {
        BAD_CREDENTIALS.to_string()
    } else {
        SERVER_REFUSED.to_string()
    }
}

/// [`server_free`] of an optional text.
pub fn server_free_opt(text: Option<&str>) -> Option<String> {
    text.map(server_free)
}

/// A permission check's reason in redis-lens's own words. The check reads a refusal as
/// `NOPERM <command> is not allowed for this user` and any other error as the server's
/// own `<CODE> <detail>`; neither goes to the page as it is.
pub fn perm_reason(label: &str, denied: bool, reason: &str) -> String {
    if denied {
        if reason.starts_with("NOPERM ") {
            return format!("the server refused {label} for this user");
        }
        if reason.starts_with("NOAUTH") {
            return format!("{label} needs a password, and this connection has none");
        }
        if reason.starts_with("WRONGPASS") {
            return format!("the server refused the credentials, so {label} was not checked");
        }
        if from_server(reason) || reason.split(' ').next().is_some_and(is_code) {
            return format!("the server refused {label}");
        }
        return web(reason);
    }
    // A skipped check whose reason quotes a refusal: `INFO is not available: <why>`.
    if let Some((head, why)) = reason.split_once(": ")
        && (from_server(why) || why.split(' ').next().is_some_and(is_code))
    {
        return web(head);
    }
    web(reason)
}

/// An all-capitals word of three letters or more: how a server error starts.
fn is_code(word: &str) -> bool {
    word.len() >= 3 && word.chars().all(|c| c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sentence the TUI, the CLI and the runtime have that the dashboard can pass
    /// on: none reaches the page with a TUI key, a screen number or a flag in it.
    fn known_texts() -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let p = crate::model::Profile {
            name: "cache".into(),
            env: crate::model::Env::Local,
            endpoint: crate::model::Endpoint {
                host: "h".into(),
                port: 1,
                tls: false,
            },
            db: 0,
            auth: crate::model::Auth::None,
            read_only: Some(true),
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            force_standalone: false,
            tags: Vec::new(),
        };
        out.push(crate::write::read_only_reason(&p));
        for name in [
            "subscribe",
            "psubscribe",
            "ssubscribe",
            "unsubscribe",
            "monitor",
            "post",
            "select",
            "auth",
            "multi",
            "client",
            "blpop",
            "eval",
        ] {
            if let Some(t) = crate::live::command::refused(&[name.as_bytes().to_vec()]) {
                out.push(t.to_string());
            }
            for sub in ["kill", "reply", "pause", "no-evict", "setname"] {
                if let Some(t) = crate::live::command::refused(&[
                    name.as_bytes().to_vec(),
                    sub.as_bytes().to_vec(),
                ]) {
                    out.push(t.to_string());
                }
            }
        }
        out.push(crate::tui::app::views::cmdline::REPEAT_ASKS.into());
        out.push(crate::tui::app::views::cmdline::HELPER_OLD.into());
        out.push(crate::conn::STANDALONE_SENTINEL.into());
        out.push(crate::conn::ConnError::EntraUnsupported.to_string());
        out.push(crate::conn::ConnError::MissingCredentials("cache".into()).to_string());
        out.push(crate::live::nodes::lost(&p.endpoint));
        out.push(crate::live::nodes::NO_OWNER.into());
        out.push(crate::config::ConfigError::Exists("cache".into()).to_string());
        out.extend(
            [
                "cache: exists already; pass --force to replace it",
                "a profile named cache already exists; pass --force to replace it",
                "writes stay off: the name given with --confirm does not match cache",
                "nothing to change: give --writes, --tag, --untag, --standalone or --no-standalone",
                "the URL has a username but no password; pass --password-stdin or --ask-password",
                "a token command replaces the password; leave out --password-stdin and --ask-password",
                "--password-stdin was given but nothing was read",
                "--entra and a token command are two ways to sign in; give one of them",
                "--ssh: the port must be a number from 1 to 65535",
                "--ssh-identity and --ssh-known-hosts need --ssh",
                "--sentinel-master and the other --sentinel-* flags need --sentinel",
                "--sentinel needs --sentinel-master NAME, the name the Sentinels monitor",
                "--sentinel-user: the user is empty or has spaces or control characters",
                "Turn writes on for cache? Each write still asks before it is sent. [y/N] ",
                "cache is a prod profile. Type its name to turn writes on: ",
                "the profile has no token command; add it again with `redis-lens conn add <name> --url <url> -- <command>`",
                "--confirm answers the question --writes on asks; give --writes on too",
            ]
            .map(String::from),
        );
        out
    }

    #[test]
    fn no_known_text_reaches_the_page_with_a_key_a_screen_number_or_a_flag() {
        for t in known_texts() {
            let w = web(&t);
            assert_eq!(banned(&w), None, "{t:?} became {w:?}");
            assert!(!w.is_empty(), "{t:?}");
        }
    }

    #[test]
    fn known_texts_keep_their_meaning_in_the_dashboards_terms() {
        assert_eq!(
            web("Not sent: cache is read-only. Writes are turned on with w on 0 Connections."),
            "Not sent: cache is read-only. Turn writes on in Connections."
        );
        assert_eq!(
            web("Not sent here: use Pub/Sub (5) to subscribe."),
            "Not sent here: use the Pub/Sub screen to subscribe."
        );
        assert_eq!(
            web("a profile named cache already exists; pass --force to replace it"),
            "a profile named cache already exists: tick Replace to overwrite it"
        );
        assert_eq!(
            web("Turn writes on for cache? Each write still asks before it is sent. [y/N] "),
            "Turn writes on for cache? Each write still asks before it is sent."
        );
        assert_eq!(
            web(crate::conn::STANDALONE_SENTINEL),
            "Standalone conflicts with a Sentinel profile: Sentinel already names one primary"
        );
        // Plain sentences pass as they are.
        assert_eq!(web("Removed cache."), "Removed cache.");
        // An unknown flag is never shown.
        assert_eq!(web("give --frobnicate"), FALLBACK);
    }

    #[test]
    fn server_and_library_error_text_becomes_a_fixed_sentence() {
        for (raw, want) in [
            ("NOAUTH: Authentication required.", NEEDS_PASSWORD),
            ("\"NOAUTH\": Authentication required.", NEEDS_PASSWORD),
            (
                "Password authentication failed - AuthenticationFailed",
                BAD_CREDENTIALS,
            ),
            (
                "WRONGPASS: invalid username-password pair or user is disabled.",
                BAD_CREDENTIALS,
            ),
            ("ERR unknown command 'X'", SERVER_REFUSED),
            (
                "An error was signalled by the server - ResponseError: ERR max clients",
                SERVER_REFUSED,
            ),
            (
                "LOADING Redis is loading the dataset in memory",
                SERVER_REFUSED,
            ),
        ] {
            assert_eq!(server_free(raw), want, "{raw}");
        }
        // redis-lens's own sentences and the OS's are kept.
        for own in [
            "Connection refused (os error 61)",
            "the server did not answer in time",
            "no Sentinel could be reached",
            "INFO or DBSIZE could not be read on this node",
            "XINFO STREAM failed",
            "TLS handshake failed: the certificate has expired",
            "SELECT would switch the database under every view",
            "the host is cache-eu: Ask the owner",
        ] {
            assert_eq!(server_free(own), own);
        }
    }

    /// The verifier's list (texts that came through unchanged before), and every form
    /// redis-rs 1.x's `Display` gives its errors: each becomes a fixed sentence.
    #[test]
    fn every_library_error_form_and_unknown_code_becomes_a_fixed_sentence() {
        let fixed = [NEEDS_PASSWORD, BAD_CREDENTIALS, SERVER_REFUSED];
        let mut texts: Vec<String> = [
            "\"MISCONF\": Redis is configured to save RDB snapshots, but it's currently unable to persist to disk.",
            "\"DENIED\": Redis is running in protected mode because protected mode is enabled",
            "\"NOPROTO\": unsupported protocol version",
            "BusyLoading: Redis is loading the dataset in memory",
            "NoScript: No matching script. Please use EVAL.",
            "ExecAbort: Transaction discarded because of previous errors.",
            "NotBusy: No scripts in execution right now.",
            "NoSub: 'unsubscribe' command without any subscriptions",
            "\"NOGROUP\": No such key 's' or consumer group 'g'",
            "\"BUSYGROUP\": Consumer Group name already exists",
            "\"IOERR\": error or timeout",
            "\"UNBLOCKED\": client unblocked via CLIENT UNBLOCK",
            "\"NOQUORUM\": -NOQUORUM not enough good replicas",
            "\"INPROG\": already in progress",
            "MISCONF Redis is configured to save RDB snapshots",
            "DENIED Redis is running in protected mode",
            "NOSUB not subscribed",
            "tail: the connection failed: \"MISCONF\": Redis is configured to save",
            "Pipeline failure: [(Index 0, error: \"MISCONF\": x)]",
            "Pipeline failure: [(Index 0, error: BusyLoading: x)]",
        ]
        .map(String::from)
        .to_vec();
        // Every server reply kind, through the library's own parser and Display.
        for code in [
            "ERR",
            "EXECABORT",
            "LOADING",
            "NOSCRIPT",
            "MOVED 1 h:1",
            "ASK 1 h:1",
            "TRYAGAIN",
            "CLUSTERDOWN",
            "CROSSSLOT",
            "MASTERDOWN",
            "READONLY",
            "NOTBUSY",
            "NOSUB",
            "NOPERM",
            "NOAUTH",
            "WRONGPASS",
            "MISCONF",
            "DENIED",
            "SOMENEWCODE",
        ] {
            let raw = format!("-{code} the server's own words\r\n");
            match redis::parse_redis_value(raw.as_bytes()).unwrap() {
                redis::Value::ServerError(e) => {
                    texts.push(e.to_string());
                    texts.push(redis::RedisError::from(e).to_string());
                }
                v => panic!("{v:?}"),
            }
        }
        // Every library kind, as `desc - Kind` and `desc - Kind: detail`.
        use redis::ErrorKind as K;
        for kind in [
            K::Parse,
            K::AuthenticationFailed,
            K::UnexpectedReturnType,
            K::InvalidClientConfig,
            K::Io,
            K::Client,
            K::Extension,
            K::MasterNameNotFoundBySentinel,
            K::NoValidReplicasFoundBySentinel,
            K::EmptySentinelList,
            K::ClusterConnectionNotFound,
            K::RESP3NotSupported,
        ] {
            texts.push(redis::RedisError::from((kind, "the library's words")).to_string());
            texts.push(
                redis::RedisError::from((kind, "the library's words", "detail".to_string()))
                    .to_string(),
            );
        }
        for t in &texts {
            let out = server_free(t);
            assert!(fixed.contains(&out.as_str()), "{t:?} became {out:?}");
        }
    }

    #[test]
    fn permission_reasons_never_carry_the_servers_text() {
        let cases = [
            (true, "NOAUTH Authentication required."),
            (true, "WRONGPASS invalid username-password pair"),
            (true, "NOPERM SCAN is not allowed for this user"),
            (true, "ERR something the server said"),
            (
                false,
                "INFO is not available: NOAUTH Authentication required.",
            ),
            (false, "INFO is not available: ERR nope"),
        ];
        for (denied, raw) in cases {
            let r = perm_reason("SCAN", denied, raw);
            for word in [
                "NOAUTH",
                "WRONGPASS",
                "ERR",
                "NOPERM",
                "Authentication required",
            ] {
                assert!(!r.contains(word), "{raw} became {r}");
            }
        }
        assert_eq!(
            perm_reason(
                "OBJECT FREQ",
                false,
                "eviction policy is noeviction, not LFU"
            ),
            "eviction policy is noeviction, not LFU"
        );
        assert_eq!(
            perm_reason("OBJECT FREQ", false, "INFO is not available: NOAUTH x"),
            "INFO is not available"
        );
    }

    /// S2: user data that holds a banned pattern keeps its text; a flag or a key in the
    /// fixed words still does not reach the page; a question is never the fallback.
    #[test]
    fn user_data_never_turns_a_text_into_the_fallback() {
        // `--` inside a word is not a flag, anywhere.
        for t in [
            "writes are off for cache--eu",
            "connected to xn--bcher-kva.example:6379",
            "tagged team--a",
        ] {
            assert_eq!(banned(t), None, "{t}");
            assert_eq!(web(t), t);
            assert_eq!(server_free(t), t);
        }
        for t in [
            "give --force",
            "--force replaces it",
            "(`--yes`)",
            "press ctrl-o",
        ] {
            assert!(
                banned(t).is_some() || banned(&t.replace("press", "Press ")).is_some(),
                "{t}"
            );
        }
        // Data that holds another pattern: kept, and the fixed words are still mapped.
        let p = crate::model::Profile {
            name: "Press (5) cache".into(),
            env: crate::model::Env::Local,
            endpoint: crate::model::Endpoint {
                host: "xn--bcher-kva.example".into(),
                port: 1,
                tls: false,
            },
            db: 0,
            auth: crate::model::Auth::None,
            read_only: Some(true),
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            force_standalone: false,
            tags: Vec::new(),
        };
        let reason = crate::write::read_only_reason(&p);
        assert_eq!(web(&reason), FALLBACK, "the old check loses it: {reason}");
        let shown = web_with(&reason, &[&p.name, &p.endpoint.host]);
        assert_ne!(shown, FALLBACK);
        assert!(shown.contains("Press (5) cache"), "{shown}");
        let without = web(&reason.replace(&p.name, "cache"));
        assert_eq!(shown, without.replace("cache", "Press (5) cache"));
        // A flag in the fixed words is still refused, data or not.
        assert_eq!(web_with("cache: pass --frobnicate", &["cache"]), FALLBACK);
        // Short data is never taken out: it would cut through the fixed words.
        assert_eq!(web_with("press w on 0 Connections", &["w"]), FALLBACK);
        // A question keeps its words or is refused, never the fallback.
        let q =
            "Turn writes on for Press (5) cache? Each write still asks before it is sent. [y/N] ";
        let asked = question(q, &["Press (5) cache"]).expect("asked");
        assert!(
            asked.contains("Press (5) cache") && !asked.contains("[y/N]"),
            "{asked}"
        );
        assert_eq!(question("Type it, then press ctrl-x", &[]), None);
        // A name that reads like a server code is the user's.
        assert_eq!(
            server_free_with("ERRSOME is read-only", &["ERRSOME"]),
            "ERRSOME is read-only"
        );
        assert_eq!(
            server_free_with("MISCONF on cache", &["cache"]),
            SERVER_REFUSED
        );
    }
}
