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

/// The first [`BANNED`] pattern `text` holds.
pub fn banned(text: &str) -> Option<&'static str> {
    BANNED.iter().copied().find(|p| text.contains(p))
}

/// `text` in the dashboard's terms: see the module docs.
pub fn web(text: &str) -> String {
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
    let t = t.trim_end().to_string();
    if banned(&t).is_some() {
        FALLBACK.to_string()
    } else {
        t
    }
}

/// [`web`] of an optional text.
pub fn web_opt(text: Option<&str>) -> Option<String> {
    text.map(web)
}

/// The error codes a Redis or Valkey server starts an error reply with, as the client
/// library shows them (`NOAUTH: ...`, `"WRONGPASS": ...`, `ResponseError: ...`).
const SERVER_CODES: &[&str] = &[
    "ERR",
    "NOAUTH",
    "WRONGPASS",
    "NOPERM",
    "WRONGTYPE",
    "LOADING",
    "BUSY",
    "MASTERDOWN",
    "READONLY",
    "NOSCRIPT",
    "EXECABORT",
    "CLUSTERDOWN",
    "CROSSSLOT",
    "TRYAGAIN",
    "MOVED",
    "ASK",
    "NOREPLICAS",
    "NOTBUSY",
    "UNKILLABLE",
    "OOM",
];

/// The client library's names for the kinds of error it reports (`... - IoError`,
/// `... - AuthenticationFailed: detail`), as its `Display` ends them.
const LIBRARY_KINDS: &[&str] = &[
    "AuthenticationFailed",
    "ResponseError",
    "ExtensionError",
    "TypeError",
    "ExecAbortError",
    "BusyLoadingError",
    "NoScriptError",
    "InvalidClientConfig",
    "ClientError",
    "ParseError",
    "ReadOnly",
    "ClusterDown",
    "CrossSlot",
    "MasterDown",
    "TryAgain",
    "Moved",
    "Ask",
    "NoPerm",
    "NoAuth",
    "WrongPass",
    "Unexpected",
    "RESP3NotSupported",
    "Server(",
    "Extension",
];

/// Whether `text` reads as a server's (or the client library's) error text rather than
/// redis-lens's own sentence.
fn from_server(text: &str) -> bool {
    let first: String = text
        .trim_start_matches(['"', '-'])
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    let starts_with_code = SERVER_CODES.contains(&first.as_str())
        && text
            .trim_start_matches(['"', '-'])
            .get(first.len()..)
            .is_some_and(|rest| rest.starts_with([' ', ':', '"']) || rest.is_empty());
    starts_with_code
        || text.contains("An error was signalled by the server")
        || LIBRARY_KINDS
            .iter()
            .any(|k| text.contains(&format!(" - {k}")) || text.contains(&format!("{k}: ")))
        || SERVER_CODES
            .iter()
            .any(|c| text.contains(&format!("\"{c}\"")) || text.contains(&format!(": {c} ")))
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
            "ERRATIC is not a code",
        ] {
            assert_eq!(server_free(own), own);
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
}
