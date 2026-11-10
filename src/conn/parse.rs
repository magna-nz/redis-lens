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
                "password" => password = Some(v.to_string()),
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
pub fn mask(s: &str) -> String {
    mask_password_params(&mask_uri_userinfo(s))
}

/// `scheme://user:PASSWORD@host` -> `scheme://user:******@host`.
fn mask_uri_userinfo(s: &str) -> String {
    let Some(i) = s.find("://") else {
        return s.to_string();
    };
    let start = i + 3;
    let rest = &s[start..];
    // The password may hold unencoded '@' or '/', so take the last '@'.
    if let Some(at) = rest.rfind('@')
        && let Some(colon) = rest.find(':')
        && colon < at
    {
        return format!("{}{}{}", &s[..start + colon + 1], MASK, &rest[at..]);
    }
    s.to_string()
}

/// `password=VALUE` (StackExchange or URI query) -> `password=******`.
fn mask_password_params(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    while let Some(found) = lower[pos..].find("password=") {
        let key_start = pos + found;
        let at_boundary = key_start == 0
            || matches!(
                lower.as_bytes()[key_start - 1],
                b',' | b'?' | b'&' | b' ' | b';'
            );
        let val_start = key_start + "password=".len();
        if !at_boundary {
            out.push_str(&s[pos..val_start]);
            pos = val_start;
            continue;
        }
        out.push_str(&s[pos..val_start]);
        out.push_str(MASK);
        pos = value_end(s, val_start);
    }
    out.push_str(&s[pos..]);
    out
}

/// The value runs to the next `,key=` or `&key=`; anything else (including a stray
/// comma) is treated as part of the password.
fn value_end(s: &str, from: usize) -> usize {
    let b = s.as_bytes();
    for (off, &c) in b[from..].iter().enumerate() {
        if c == b',' || c == b'&' {
            let after = &s[from + off + 1..];
            let key_len = after
                .bytes()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_')
                .count();
            if key_len > 0 && after.as_bytes().get(key_len) == Some(&b'=') {
                return from + off;
            }
        }
    }
    s.len()
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
        fn mask_hides_stackexchange_password(p in "[A-Za-z0-9@:/?#!$&=%]{0,10}", core in "[A-Za-z0-9]{12,24}") {
            let pw = format!("PW{core}{p}");
            let s = format!("host.example.com:6380,password={pw},ssl=True,abortConnect=False");
            prop_assert!(!mask(&s).contains(&core));
            let s = format!("host.example.com:6380,ssl=True,password={pw}");
            prop_assert!(!mask(&s).contains(&core));
        }
    }
}
