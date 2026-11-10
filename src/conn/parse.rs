//! Parsing pasted connection strings.
//!
//! Owner: connections.

use secrecy::SecretString;

use crate::model::{Credentials, Endpoint, Env};

use super::ConnError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnFormat {
    /// `redis://` or `rediss://`.
    RedisUri,
    /// `host:port,password=...,ssl=True,...`
    StackExchange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AzureKind {
    /// `*.redis.cache.windows.net`
    CacheForRedis,
    /// `*.redis.azure.net`
    ManagedRedis,
}

#[derive(Debug, Clone)]
pub struct ParsedConnection {
    pub format: ConnFormat,
    pub endpoint: Endpoint,
    pub db: u32,
    /// Username and password found in the string, if any. Goes to the keychain on save.
    ///
    /// Some whenever the string has a password. When it names a user but no password
    /// this is Some with an empty password, so callers can tell a password is still
    /// needed.
    pub credentials: Option<Credentials>,
    pub azure: Option<AzureKind>,
    /// Guessed from the host: localhost is `Local`, "prod" in the host is `Prod`, and so on.
    pub env_guess: Env,
    /// First label of the host, or `local-<port>` for localhost.
    pub name_guess: String,
}

const DEFAULT_PORT: u16 = 6379;
const MASK: &str = "******";

/// Parse a `redis://` / `rediss://` URI or a StackExchange.Redis connection string.
///
/// Error messages never contain the input.
pub fn parse_connection_string(s: &str) -> Result<ParsedConnection, ConnError> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("redis://") || lower.starts_with("rediss://") {
        parse_uri(s)
    } else {
        parse_stackexchange(s)
    }
}

fn err(msg: &str) -> ConnError {
    ConnError::Parse(msg.to_string())
}

fn percent_decode(s: &str) -> Result<String, ConnError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| err("credentials are not valid UTF-8 after decoding"))
}

fn build(
    format: ConnFormat,
    host: String,
    port: u16,
    tls: bool,
    db: u32,
    username: Option<String>,
    password: Option<String>,
) -> Result<ParsedConnection, ConnError> {
    if host.is_empty() {
        return Err(err("missing host"));
    }
    let username = username.filter(|u| !u.is_empty() && u != "default");
    let credentials = match (username, password) {
        (user, Some(pw)) if !pw.is_empty() => Some(Credentials {
            username: user,
            password: SecretString::from(pw),
        }),
        (Some(user), _) => Some(Credentials {
            username: Some(user),
            password: SecretString::from(String::new()),
        }),
        _ => None,
    };
    let lower = host.to_ascii_lowercase();
    let azure = if lower.ends_with(".redis.cache.windows.net") {
        Some(AzureKind::CacheForRedis)
    } else if lower.ends_with(".redis.azure.net") {
        Some(AzureKind::ManagedRedis)
    } else {
        None
    };
    let name_guess = name_guess(&lower, port);
    Ok(ParsedConnection {
        format,
        env_guess: env_guess(&lower),
        endpoint: Endpoint { host, port, tls },
        db,
        credentials,
        azure,
        name_guess,
    })
}

fn is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn env_guess(host_lower: &str) -> Env {
    if is_local(host_lower) {
        Env::Local
    } else if host_lower.contains("prod") {
        Env::Prod
    } else if ["stag", "test", "dev", "qa"]
        .iter()
        .any(|k| host_lower.contains(k))
    {
        Env::Stage
    } else {
        Env::Other
    }
}

fn name_guess(host_lower: &str, port: u16) -> String {
    if is_local(host_lower) {
        return format!("local-{port}");
    }
    if host_lower.parse::<std::net::IpAddr>().is_ok() {
        return host_lower.replace(['.', ':'], "-");
    }
    host_lower
        .split('.')
        .next()
        .filter(|l| !l.is_empty())
        .unwrap_or(host_lower)
        .to_string()
}

fn parse_uri(s: &str) -> Result<ParsedConnection, ConnError> {
    let url = url::Url::parse(s).map_err(|e| ConnError::Parse(e.to_string()))?;
    let tls = url.scheme() == "rediss";
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(a)) => a.to_string(),
        Some(url::Host::Ipv6(a)) => a.to_string(),
        None => return Err(err("missing host")),
    };
    let port = url.port().unwrap_or(DEFAULT_PORT);
    let path = url.path().trim_matches('/');
    let db = if path.is_empty() {
        0
    } else {
        path.parse::<u32>()
            .map_err(|_| err("the database in the path must be a number"))?
    };
    let username = match url.username() {
        "" => None,
        u => Some(percent_decode(u)?),
    };
    let password = url.password().map(percent_decode).transpose()?;
    build(
        ConnFormat::RedisUri,
        host,
        port,
        tls,
        db,
        username,
        password,
    )
}

fn split_host_port(s: &str) -> Result<(String, u16), ConnError> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| err("unclosed [ in host"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| err("invalid port"))?,
            None if after.is_empty() => DEFAULT_PORT,
            None => return Err(err("unexpected text after ]")),
        };
        return Ok((host.to_string(), port));
    }
    if s.matches(':').count() > 1 {
        // A bare IPv6 address has no way to carry a port.
        return Ok((s.to_string(), DEFAULT_PORT));
    }
    match s.split_once(':') {
        Some((h, p)) => Ok((h.to_string(), p.parse().map_err(|_| err("invalid port"))?)),
        None => Ok((s.to_string(), DEFAULT_PORT)),
    }
}

fn parse_stackexchange(s: &str) -> Result<ParsedConnection, ConnError> {
    let mut endpoint: Option<(String, u16)> = None;
    let (mut user, mut password) = (None, None);
    let (mut tls, mut db) = (false, 0u32);
    for token in s.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match token.split_once('=') {
            None => {
                if endpoint.is_none() {
                    endpoint = Some(split_host_port(token)?);
                }
            }
            Some((k, v)) => match k.trim().to_ascii_lowercase().as_str() {
                "password" => password = Some(v.trim().to_string()),
                "user" | "username" => user = Some(v.trim().to_string()),
                "ssl" => tls = v.trim().eq_ignore_ascii_case("true"),
                "defaultdatabase" => {
                    db = v
                        .trim()
                        .parse()
                        .map_err(|_| err("defaultDatabase must be a number"))?;
                }
                _ => {}
            },
        }
    }
    let (host, port) = endpoint.ok_or_else(|| err("no host found"))?;
    build(
        ConnFormat::StackExchange,
        host,
        port,
        tls,
        db,
        user,
        password,
    )
}

/// The input with any password replaced by `******`, safe to display or log.
///
/// Tokenises the way [`parse_connection_string`] does: a string starting with
/// `redis://` or `rediss://` is a URI, anything else is a StackExchange string. It is
/// also used on free text such as error messages, so a non-URI input that contains
/// `://` gets the URI treatment too. When in doubt it hides more, never less.
pub fn mask(s: &str) -> String {
    let lower = s.trim_start().to_ascii_lowercase();
    if lower.starts_with("redis://") || lower.starts_with("rediss://") {
        return mask_uri(s);
    }
    let masked = mask_stackexchange(s);
    if masked.contains("://") {
        mask_uri(&masked)
    } else {
        masked
    }
}

/// Whether the text before a `=` names a password. The parser trims the key and
/// compares it case-insensitively; this also accepts `password` as the last word of
/// the key, so free text such as `error: password=...` is caught too.
fn is_password_key(k: &str) -> bool {
    k.trim()
        .rsplit(|c: char| c.is_whitespace() || matches!(c, ';' | '?' | '&'))
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("password"))
}

/// StackExchange: split on `,`, split each token at its first `=`, and replace the
/// value of a `password` key. The key text is kept as it was.
fn mask_stackexchange(s: &str) -> String {
    s.split(',')
        .map(|token| match token.split_once('=') {
            Some((k, _)) if is_password_key(k) => format!("{k}={MASK}"),
            _ => token.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The colon that would separate a username from a password in a URI that has no `@`
/// yet. A bracketed IPv6 host is skipped, so its colons don't count.
fn half_typed_colon(rest: &str) -> Option<usize> {
    if rest.starts_with('[') {
        let close = rest.find(']')?;
        rest[close..].find(':').map(|c| close + c)
    } else {
        rest.find(':')
    }
}

/// URIs: the userinfo password and any `password` query parameter.
///
/// - `scheme://user:PASSWORD@host`: the password may hold unencoded `@`, `/` or `?`,
///   so it runs from the first `:` to the last `@`.
/// - No `@` yet (half-typed, such as `rediss://user:pa`): everything after the first
///   `:` is hidden, unless the text up to the next `/` is a 1 to 5 digit port.
/// - The query is split on `&`. Every `?` is tried as the start of the query, because
///   an unencoded `?` can also sit in the password.
fn mask_uri(s: &str) -> String {
    let Some(i) = s.find("://") else {
        return s.to_string();
    };
    let start = i + 3;
    let rest = &s[start..];
    // Byte ranges of `rest` to hide.
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    if let Some(at) = rest.rfind('@') {
        if let Some(colon) = rest[..at].find(':') {
            ranges.push((colon + 1, at));
        }
    } else if let Some(colon) = half_typed_colon(rest) {
        let after = &rest[colon + 1..];
        let head = after.split('/').next().unwrap_or("");
        let is_port = (1..=5).contains(&head.len()) && head.bytes().all(|b| b.is_ascii_digit());
        if !is_port {
            ranges.push((colon + 1, rest.len()));
        }
    }

    for (q, _) in rest.match_indices('?') {
        let mut pos = q + 1;
        for param in rest[q + 1..].split('&') {
            if let Some((k, _)) = param.split_once('=')
                && k.trim().eq_ignore_ascii_case("password")
            {
                ranges.push((pos + k.len() + 1, pos + param.len()));
            }
            pos += param.len() + 1;
        }
    }

    ranges.retain(|(a, b)| a < b);
    ranges.sort_unstable();
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..start]);
    let mut pos = 0;
    for (a, b) in ranges {
        if b <= pos {
            continue;
        }
        if a >= pos {
            out.push_str(&rest[pos..a]);
            out.push_str(MASK);
        }
        // An overlapping range extends the one already masked.
        pos = b;
    }
    out.push_str(&rest[pos..]);
    out
}

#[cfg(test)]
mod partial_mask_tests {
    use super::mask;

    #[test]
    fn half_typed_password_is_masked() {
        assert_eq!(mask("rediss://user:Sup3rS3cret"), "rediss://user:******");
        assert_eq!(mask("rediss://user:Sup3r"), "rediss://user:******");
        assert!(!mask("redis://lens:pw/0").contains("pw"));
    }

    #[test]
    fn host_and_port_are_left_alone() {
        assert_eq!(mask("redis://localhost:6379"), "redis://localhost:6379");
        assert_eq!(mask("redis://localhost:6379/0"), "redis://localhost:6379/0");
        assert_eq!(mask("redis://localhost:"), "redis://localhost:");
        assert_eq!(mask("redis://localhost"), "redis://localhost");
        assert_eq!(mask("redis://[::1]:6390/1"), "redis://[::1]:6390/1");
    }

    #[test]
    fn half_typed_password_tail_is_masked() {
        assert_eq!(mask("redis://u:ab/s3cretEEE"), "redis://u:******");
        assert_eq!(mask("redis://u:ab?s3cretEEE"), "redis://u:******");
        assert_eq!(mask("redis://u:123456/s3cretEEE"), "redis://u:******");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use secrecy::ExposeSecret;

    fn pw(p: &ParsedConnection) -> Option<&str> {
        p.credentials.as_ref().map(|c| c.password.expose_secret())
    }

    #[test]
    fn plain_uri_defaults() {
        let p = parse_connection_string("redis://example.com").unwrap();
        assert_eq!(p.format, ConnFormat::RedisUri);
        assert_eq!(p.endpoint.host, "example.com");
        assert_eq!(p.endpoint.port, 6379);
        assert!(!p.endpoint.tls);
        assert_eq!(p.db, 0);
        assert!(p.credentials.is_none());
        assert!(p.azure.is_none());
    }

    #[test]
    fn rediss_port_db_and_credentials() {
        let p = parse_connection_string("rediss://alice:s3cret@cache.example.com:6380/2").unwrap();
        assert!(p.endpoint.tls);
        assert_eq!(p.endpoint.port, 6380);
        assert_eq!(p.db, 2);
        let c = p.credentials.unwrap();
        assert_eq!(c.username.as_deref(), Some("alice"));
        assert_eq!(c.password.expose_secret(), "s3cret");
    }

    #[test]
    fn percent_decoding() {
        let p = parse_connection_string("redis://a%40b:p%40ss%3Aw%2Frd%25@h").unwrap();
        let c = p.credentials.unwrap();
        assert_eq!(c.username.as_deref(), Some("a@b"));
        assert_eq!(c.password.expose_secret(), "p@ss:w/rd%");
    }

    #[test]
    fn password_only_and_default_user() {
        let p = parse_connection_string("redis://:pw@h").unwrap();
        assert_eq!(p.credentials.as_ref().unwrap().username, None);
        assert_eq!(pw(&p), Some("pw"));
        let p = parse_connection_string("redis://default:pw@h").unwrap();
        assert_eq!(p.credentials.as_ref().unwrap().username, None);
    }

    #[test]
    fn username_without_password_has_empty_password() {
        let p = parse_connection_string("redis://alice@h").unwrap();
        let c = p.credentials.unwrap();
        assert_eq!(c.username.as_deref(), Some("alice"));
        assert_eq!(c.password.expose_secret(), "");
    }

    #[test]
    fn ipv6_uri() {
        let p = parse_connection_string("redis://[::1]:6390/1").unwrap();
        assert_eq!(p.endpoint.host, "::1");
        assert_eq!(p.endpoint.port, 6390);
        assert_eq!(p.db, 1);
        assert_eq!(p.env_guess, Env::Local);
        assert_eq!(p.name_guess, "local-6390");
        let p = parse_connection_string("redis://[::1]").unwrap();
        assert_eq!(p.endpoint.port, 6379);
    }

    #[test]
    fn stackexchange_full() {
        let p = parse_connection_string(
            "myhost.redis.cache.windows.net:6380,Password=abc=,USER=bob,ssl=True,defaultDatabase=3,abortConnect=False,foo=bar",
        )
        .unwrap();
        assert_eq!(p.format, ConnFormat::StackExchange);
        assert_eq!(p.endpoint.host, "myhost.redis.cache.windows.net");
        assert_eq!(p.endpoint.port, 6380);
        assert!(p.endpoint.tls);
        assert_eq!(p.db, 3);
        assert_eq!(p.azure, Some(AzureKind::CacheForRedis));
        let c = p.credentials.unwrap();
        assert_eq!(c.username.as_deref(), Some("bob"));
        assert_eq!(c.password.expose_secret(), "abc=");
    }

    #[test]
    fn stackexchange_defaults() {
        let p = parse_connection_string("example.com").unwrap();
        assert_eq!(p.endpoint.port, 6379);
        assert!(!p.endpoint.tls);
        assert!(p.credentials.is_none());
        let p = parse_connection_string("example.com,user=default,password=x").unwrap();
        assert_eq!(p.credentials.unwrap().username, None);
        let p = parse_connection_string("[::1]:6380,ssl=false").unwrap();
        assert_eq!(p.endpoint.host, "::1");
        assert_eq!(p.endpoint.port, 6380);
    }

    #[test]
    fn azure_kinds() {
        let p = parse_connection_string("rediss://x.eastus.redis.azure.net:10000").unwrap();
        assert_eq!(p.azure, Some(AzureKind::ManagedRedis));
        let p = parse_connection_string("redis://x.redis.cache.windows.net").unwrap();
        assert_eq!(p.azure, Some(AzureKind::CacheForRedis));
        let p = parse_connection_string("redis://example.com").unwrap();
        assert_eq!(p.azure, None);
    }

    #[test]
    fn env_guesses() {
        let cases = [
            ("localhost", Env::Local),
            ("127.0.0.1", Env::Local),
            ("cache-prod-01.example.com", Env::Prod),
            ("staging.example.com", Env::Stage),
            ("test-cache.example.com", Env::Stage),
            ("dev.example.com", Env::Stage),
            ("qa1.example.com", Env::Stage),
            ("cache.example.com", Env::Other),
        ];
        for (host, want) in cases {
            let p = parse_connection_string(&format!("redis://{host}")).unwrap();
            assert_eq!(p.env_guess, want, "{host}");
        }
    }

    #[test]
    fn name_guesses() {
        let p = parse_connection_string("redis://orders.abc.example.com:6379").unwrap();
        assert_eq!(p.name_guess, "orders");
        let p = parse_connection_string("redis://localhost:6390").unwrap();
        assert_eq!(p.name_guess, "local-6390");
        let p = parse_connection_string("127.0.0.1:7000").unwrap();
        assert_eq!(p.name_guess, "local-7000");
    }

    #[test]
    fn errors_do_not_echo_input() {
        for bad in [
            "redis://:topsecret@",
            "redis://u:topsecret@h:notaport",
            "redis://u:topsecret@h/xx",
            "topsecret:abc",
            "",
            ",password=topsecret",
        ] {
            let e = parse_connection_string(bad).err().unwrap().to_string();
            assert!(!e.contains("topsecret"), "{e}");
        }
    }

    #[test]
    fn mask_examples() {
        assert_eq!(
            mask("rediss://alice:s3cret@host:6380/1"),
            "rediss://alice:******@host:6380/1"
        );
        assert_eq!(mask("redis://:s3cret@host"), "redis://:******@host");
        assert_eq!(
            mask("h:6380,password=s3cret,ssl=True"),
            "h:6380,password=******,ssl=True"
        );
        assert_eq!(mask("h,Password=s3cret"), "h,Password=******");
        assert_eq!(mask("redis://host:6379/0"), "redis://host:6379/0");
        assert_eq!(mask("redis://u:p@w@host"), "redis://u:******@host");
    }

    #[test]
    fn mask_matches_parser_tokenising() {
        for s in [
            "host:6380, password = s3cretAAA ,ssl=True",
            "host:6380,password =s3cretBBB",
            "host:6380,\tpassword=s3cretCCC",
            "host:6380,password=abc&x=s3cretDDD",
            "redis://h?password=s3cretFFF",
            "redis://u:pw@h:6379/0?ssl=true& Password =s3cretGGG",
            "redis://h:6379/0?password=a@s3cretHHH",
            "redis://u:a?b@h?password=s3cretIII",
            "connect failed for redis://u:s3cretJJJ@h:6379",
            "error: password=s3cretKKK",
        ] {
            let m = mask(s);
            assert!(!m.contains("s3cret"), "{s:?} -> {m:?}");
        }
        assert_eq!(
            mask("host:6380, password = s3cretAAA ,ssl=True"),
            "host:6380, password =******,ssl=True"
        );
        assert_eq!(
            mask("host:6380,password=abc&x=s3cretDDD"),
            "host:6380,password=******"
        );
        assert_eq!(
            mask("redis://h?password=s3cretFFF"),
            "redis://h?password=******"
        );
        assert_eq!(
            mask("redis://u:pw@h:6380/1?ssl=true&password=x&y=1"),
            "redis://u:******@h:6380/1?ssl=true&password=******&y=1"
        );
        // Keys that only end in "password" are not passwords.
        assert_eq!(mask("h,xpassword=keep"), "h,xpassword=keep");
    }

    #[test]
    fn stackexchange_values_are_trimmed() {
        let p = parse_connection_string("host:6380, password = s3cretAAA ,ssl=True").unwrap();
        assert_eq!(pw(&p), Some("s3cretAAA"));
        assert!(p.endpoint.tls);
        let p = parse_connection_string("host:6380,\tpassword=s3cretCCC").unwrap();
        assert_eq!(pw(&p), Some("s3cretCCC"));
    }

    proptest! {
        #[test]
        fn parse_never_panics(s in "\\PC{0,80}") {
            let _ = parse_connection_string(&s);
            let _ = mask(&s);
        }

        #[test]
        fn parse_never_panics_on_uri_like(s in "(redis|rediss)://[ -~]{0,60}") {
            let _ = parse_connection_string(&s);
            let _ = mask(&s);
        }

        #[test]
        fn mask_hides_uri_password(p in "[A-Za-z0-9@:/?#!$&=%]{0,10}", core in "[A-Za-z0-9]{12,24}") {
            let pw = format!("PW{core}{p}");
            let uri = format!("rediss://user:{pw}@host.example.com:6380/1");
            prop_assert!(!mask(&uri).contains(&pw));
            prop_assert!(!mask(&uri).contains(&core));
        }

        #[test]
        fn mask_hides_uri_query_password(
            p in "[A-Za-z0-9@:/?#!$=%]{0,10}",
            core in "[A-Za-z0-9]{12,24}",
            w1 in "[ \t]{0,2}",
            w2 in "[ \t]{0,2}",
            key in "(password|Password|PASSWORD)",
            before in "(|ssl=true&|a=b&c=d&)",
            userinfo in "(|u:pw@|:x@)",
        ) {
            let pw = format!("PW{core}{p}");
            let uri = format!("redis://{userinfo}host.example.com:6380/1?{before}{w1}{key}{w2}={pw}&x=1");
            prop_assert!(!mask(&uri).contains(&core), "{}", mask(&uri));
            let uri = format!("redis://{userinfo}host.example.com?{before}{w1}{key}{w2}={pw}");
            prop_assert!(!mask(&uri).contains(&core), "{}", mask(&uri));
        }

        #[test]
        fn mask_hides_stackexchange_password(
            p in "[A-Za-z0-9@:/?#!$&=%]{0,10}",
            core in "[A-Za-z0-9]{12,24}",
            w1 in "[ \t]{0,3}",
            w2 in "[ \t]{0,3}",
            w3 in "[ \t]{0,3}",
            w4 in "[ \t]{0,3}",
            key in "(password|Password|PASSWORD)",
        ) {
            let pw = format!("PW{core}{p}");
            let field = format!("{w1}{key}{w2}={w3}{pw}{w4}");
            let s = format!("host.example.com:6380,{field},ssl=True,abortConnect=False");
            prop_assert!(!mask(&s).contains(&core), "{}", mask(&s));
            let parsed = parse_connection_string(&s).unwrap();
            prop_assert_eq!(super::tests::pw(&parsed), Some(pw.as_str()));
            let s = format!("host.example.com:6380,ssl=True,{field}");
            prop_assert!(!mask(&s).contains(&core), "{}", mask(&s));
        }
    }
}
