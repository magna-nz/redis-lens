//! The dashboard's guard (spec "Web dashboard", Security): the session token and its
//! cookie, the `Host` check against DNS rebinding, the checks on every request that is
//! not a `GET`, and the headers every response carries.
//!
//! - **Token.** 32 random bytes per run, in hex. `GET /auth/<token>` sets it as the
//!   `rl_session` cookie and redirects to `/`; every `/api/*` route needs that cookie
//!   (compared in constant time). The token is never logged and never in `Debug`.
//! - **Host.** Every request's `Host` must be `127.0.0.1:<port>` or `localhost:<port>`.
//! - **Not a GET.** `Origin` must be `http://127.0.0.1:<port>` or
//!   `http://localhost:<port>`, `X-Redis-Lens: 1` must be there, and the body is JSON
//!   (`Content-Type: application/json`).
//!
//! A refusal says nothing more than `forbidden` (403) or `not signed in` (401).

use std::fmt;

use axum::extract::{Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::api::error;

/// The cookie that carries the session token.
pub const COOKIE: &str = "rl_session";
/// The header every request that is not a `GET` must carry, with the value `1`.
pub const HEADER: &str = "x-redis-lens";

/// The text of a 401: nothing about why.
pub const NOT_SIGNED_IN: &str = "not signed in";
/// The text of a 403: nothing about why.
pub const FORBIDDEN: &str = "forbidden";

/// The Content-Security-Policy of every response.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; \
     form-action 'self'";

/// This run's session token: 32 random bytes as 64 hex digits. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// A new random token. Fails only when the OS has no random bytes to give.
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(Self(hex(&bytes)))
    }

    /// The token as it goes in the URL and the cookie.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether `given` is this token, compared in constant time.
    pub fn matches(&self, given: &str) -> bool {
        ct_eq(self.0.as_bytes(), given.as_bytes())
    }

    #[cfg(test)]
    pub(crate) fn for_test(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<hidden>)")
    }
}

/// `bytes` as lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    out
}

/// Equal in time that depends on the length only (the token's length is no secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// What the guard checks requests against: the port the server listens on and the token.
#[derive(Clone)]
pub struct Guard {
    pub port: u16,
    pub token: Token,
}

impl fmt::Debug for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Guard")
            .field("port", &self.port)
            .field("token", &self.token)
            .finish()
    }
}

impl Guard {
    /// `Host` is exactly `127.0.0.1:<port>` or `localhost:<port>`.
    pub fn host_ok(&self, headers: &HeaderMap) -> bool {
        let Some(host) = one(headers, header::HOST) else {
            return false;
        };
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }

    /// `Origin` is exactly `http://127.0.0.1:<port>` or `http://localhost:<port>`.
    pub fn origin_ok(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = one(headers, header::ORIGIN) else {
            return false;
        };
        origin == format!("http://127.0.0.1:{}", self.port)
            || origin == format!("http://localhost:{}", self.port)
    }

    /// The request carries the session cookie with this run's token.
    pub fn signed_in(&self, headers: &HeaderMap) -> bool {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(k, v)| k == COOKIE && self.token.matches(v))
    }

    /// The `Set-Cookie` value that signs a browser in.
    pub fn cookie(&self) -> String {
        format!(
            "{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/",
            self.token.expose()
        )
    }
}

/// The value of header `name` when it is there exactly once and is text.
fn one(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    let mut all = headers.get_all(name).iter();
    let first = all.next()?;
    if all.next().is_some() {
        return None;
    }
    first.to_str().ok()
}

/// `GET` and `HEAD` change nothing; everything else must pass [`writes_ok`].
pub fn is_safe(method: &Method) -> bool {
    method == Method::GET || method == Method::HEAD
}

/// The checks on a request that is not a `GET`: its `Origin`, the `X-Redis-Lens: 1`
/// header, and a JSON body.
pub fn writes_ok(guard: &Guard, headers: &HeaderMap) -> bool {
    let header_ok = one(headers, header::HeaderName::from_static(HEADER)) == Some("1");
    let json = one(headers, header::CONTENT_TYPE).is_some_and(|ct| {
        ct.split(';')
            .next()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
    });
    guard.origin_ok(headers) && header_ok && json
}

/// The guard, in front of every route: `Host`, then for anything but `GET` the origin,
/// header and body checks, then for `/api/*` the session cookie.
pub async fn guard(State(guard): State<Guard>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    if !guard.host_ok(headers) {
        return error(StatusCode::FORBIDDEN, FORBIDDEN);
    }
    if !is_safe(req.method()) && !writes_ok(&guard, headers) {
        return error(StatusCode::FORBIDDEN, FORBIDDEN);
    }
    let path = req.uri().path();
    if (path == "/api" || path.starts_with("/api/")) && !guard.signed_in(headers) {
        return error(StatusCode::UNAUTHORIZED, NOT_SIGNED_IN);
    }
    next.run(req).await
}

/// The headers of every response, whatever answered it (a route, a refusal, an asset,
/// a 404).
pub async fn headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    set_security_headers(res.headers_mut());
    res
}

pub fn set_security_headers(h: &mut HeaderMap) {
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
}

/// `GET /auth/<token>`: with this run's token, sets the cookie and redirects (303) to
/// `/`; otherwise 401.
pub async fn sign_in(
    State(guard): State<Guard>,
    axum::extract::Path(given): axum::extract::Path<String>,
) -> Response {
    if !guard.token.matches(&given) {
        return error(StatusCode::UNAUTHORIZED, NOT_SIGNED_IN);
    }
    let mut res = StatusCode::SEE_OTHER.into_response();
    res.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    if let Ok(v) = HeaderValue::from_str(&guard.cookie()) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Guard {
        Guard {
            port: 7379,
            token: Token::for_test("ab".repeat(32).as_str()),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_random_token_is_64_hex_digits_and_never_in_debug() {
        let t = Token::random().unwrap();
        assert_eq!(t.expose().len(), 64);
        assert!(t.expose().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, Token::random().unwrap());
        let shown = format!(
            "{t:?} {:?}",
            Guard {
                port: 1,
                token: t.clone()
            }
        );
        assert!(!shown.contains(t.expose()), "{shown}");
    }

    #[test]
    fn the_token_matches_itself_only() {
        let g = guard();
        assert!(g.token.matches(&"ab".repeat(32)));
        assert!(!g.token.matches(&"ab".repeat(31)));
        assert!(!g.token.matches(&format!("{}ac", "ab".repeat(31))));
        assert!(!g.token.matches(""));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"a", b"b"));
    }

    #[test]
    fn host_must_be_loopback_with_the_port() {
        let g = guard();
        assert!(g.host_ok(&headers(&[("host", "127.0.0.1:7379")])));
        assert!(g.host_ok(&headers(&[("host", "localhost:7379")])));
        for bad in [
            "127.0.0.1",
            "localhost",
            "127.0.0.1:7380",
            "evil.example:7379",
            "127.0.0.1.evil.example:7379",
            "[::1]:7379",
            "",
        ] {
            assert!(!g.host_ok(&headers(&[("host", bad)])), "{bad}");
        }
        assert!(!g.host_ok(&HeaderMap::new()));
        // Two Host headers: refused.
        assert!(!g.host_ok(&headers(&[
            ("host", "127.0.0.1:7379"),
            ("host", "127.0.0.1:7379")
        ])));
    }

    #[test]
    fn a_write_needs_the_origin_the_header_and_json() {
        let g = guard();
        let ok = [
            ("origin", "http://127.0.0.1:7379"),
            ("x-redis-lens", "1"),
            ("content-type", "application/json"),
        ];
        assert!(writes_ok(&g, &headers(&ok)));
        assert!(writes_ok(
            &g,
            &headers(&[
                ("origin", "http://localhost:7379"),
                ("x-redis-lens", "1"),
                ("content-type", "application/json; charset=utf-8"),
            ])
        ));
        for i in 0..ok.len() {
            let missing: Vec<_> = ok
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, p)| *p)
                .collect();
            assert!(!writes_ok(&g, &headers(&missing)), "without {:?}", ok[i]);
        }
        for (k, bad) in [
            ("origin", "https://127.0.0.1:7379"),
            ("origin", "http://127.0.0.1:7380"),
            ("origin", "http://evil.example"),
            ("origin", "null"),
            ("x-redis-lens", "0"),
            ("content-type", "text/plain"),
            ("content-type", "application/x-www-form-urlencoded"),
        ] {
            let h: Vec<_> = ok
                .iter()
                .map(|(hk, hv)| if *hk == k { (*hk, bad) } else { (*hk, *hv) })
                .collect();
            assert!(!writes_ok(&g, &headers(&h)), "{k}: {bad}");
        }
    }

    #[test]
    fn the_cookie_signs_in_and_nothing_else_does() {
        let g = guard();
        let good = format!("{COOKIE}={}", "ab".repeat(32));
        assert!(g.signed_in(&headers(&[("cookie", &good)])));
        assert!(g.signed_in(&headers(&[("cookie", &format!("theme=dark; {good}"))])));
        assert!(!g.signed_in(&headers(&[(
            "cookie",
            &format!("{COOKIE}={}", "cd".repeat(32))
        )])));
        assert!(!g.signed_in(&headers(&[(
            "cookie",
            &format!("other={}", "ab".repeat(32))
        )])));
        assert!(!g.signed_in(&HeaderMap::new()));
        assert_eq!(
            g.cookie(),
            format!(
                "rl_session={}; HttpOnly; SameSite=Strict; Path=/",
                "ab".repeat(32)
            )
        );
    }
}
