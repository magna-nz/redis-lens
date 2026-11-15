//! The router without a socket (`tower::ServiceExt::oneshot`): the guard's refusals,
//! the headers on every response, the page's files, and the connection routes against
//! a temporary config file and a `MemoryStore` (never the real keychain).

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use tower::ServiceExt;

use super::auth::{CSP, Token};
use super::*;
use crate::model::{Auth, Endpoint, Env, Profile, SECRET_SERVICE, SecretRef};
use crate::secrets::MemoryStore;

const PORT: u16 = 7379;
const HOST: &str = "127.0.0.1:7379";
const ORIGIN: &str = "http://127.0.0.1:7379";

fn token() -> String {
    "0123456789abcdef".repeat(4)
}

fn temp_config(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-web-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("config.toml")
}

fn profile(name: &str, env: Env) -> Profile {
    Profile {
        name: name.into(),
        env,
        endpoint: Endpoint {
            host: "cache.internal".into(),
            port: 6380,
            tls: true,
        },
        db: 0,
        auth: Auth::None,
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        force_standalone: false,
        tags: Vec::new(),
    }
}

struct T {
    router: axum::Router,
    hub: Arc<session::Hub>,
    store: Arc<MemoryStore>,
    config: PathBuf,
    _shutdown: tokio::sync::watch::Sender<bool>,
}

fn setup(tag: &str, profiles: Vec<Profile>) -> T {
    let config = temp_config(tag);
    let mut c = crate::config::Config::default();
    for p in profiles {
        c.add(p, false).unwrap();
    }
    c.save(&config).unwrap();
    let store = Arc::new(MemoryStore::default());
    let hub = session::Hub::new(config.clone(), store.clone());
    let (tx, rx) = tokio::sync::watch::channel(false);
    let app = app_state(Arc::clone(&hub), PORT, Token::for_test(&token()), rx);
    T {
        router: router(app),
        hub,
        store,
        config,
        _shutdown: tx,
    }
}

fn bearer() -> String {
    format!("Bearer {}", token())
}

fn get(path: &str) -> axum::http::request::Builder {
    Request::get(path).header(header::HOST, HOST)
}

fn signed_get(path: &str) -> Request<Body> {
    get(path)
        .header(header::AUTHORIZATION, bearer())
        .body(Body::empty())
        .unwrap()
}

fn write(method: &str, path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST)
        .header(header::ORIGIN, ORIGIN)
        .header("x-redis-lens", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, bearer())
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn call(t: &T, req: Request<Body>) -> Response {
    t.router.clone().oneshot(req).await.unwrap()
}

async fn text(res: Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn assert_security_headers(res: &Response, what: &str) {
    let h = res.headers();
    assert_eq!(h[header::CACHE_CONTROL], "no-store", "{what}");
    assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{what}");
    assert_eq!(h[header::REFERRER_POLICY], "no-referrer", "{what}");
    assert_eq!(h[header::X_FRAME_OPTIONS], "DENY", "{what}");
    assert_eq!(h[header::CONTENT_SECURITY_POLICY], CSP, "{what}");
}

#[tokio::test]
async fn every_response_has_the_security_headers() {
    let t = setup("headers", Vec::new());
    let cases: Vec<(&str, Request<Body>, StatusCode)> = vec![
        (
            "index",
            get("/").body(Body::empty()).unwrap(),
            StatusCode::OK,
        ),
        (
            "asset",
            get("/assets/app.js").body(Body::empty()).unwrap(),
            StatusCode::OK,
        ),
        (
            "missing asset",
            get("/assets/nope.js").body(Body::empty()).unwrap(),
            StatusCode::NOT_FOUND,
        ),
        (
            "no route",
            get("/nowhere").body(Body::empty()).unwrap(),
            StatusCode::NOT_FOUND,
        ),
        (
            "api without the token",
            get("/api/state").body(Body::empty()).unwrap(),
            StatusCode::UNAUTHORIZED,
        ),
        ("api", signed_get("/api/state"), StatusCode::OK),
        (
            "bad host",
            Request::get("/")
                .header(header::HOST, "evil.example:7379")
                .body(Body::empty())
                .unwrap(),
            StatusCode::FORBIDDEN,
        ),
        (
            "no sign-in route",
            get(&format!("/auth/{}", token()))
                .body(Body::empty())
                .unwrap(),
            StatusCode::NOT_FOUND,
        ),
        (
            "wrong method",
            write("PUT", "/api/state", "{}"),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            "bad body",
            write("POST", "/api/connect", "not json"),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (what, req, want) in cases {
        let res = call(&t, req).await;
        assert_eq!(res.status(), want, "{what}");
        assert_security_headers(&res, what);
    }
}

#[tokio::test]
async fn host_must_be_loopback_with_the_port_on_every_route() {
    let t = setup("host", Vec::new());
    for host in [
        "evil.example:7379",
        "127.0.0.1:80",
        "localhost",
        "127.0.0.1.nip.io:7379",
    ] {
        for path in ["/", "/assets/app.js", "/api/state", "/auth/x"] {
            let req = Request::get(path)
                .header(header::HOST, host)
                .header(header::AUTHORIZATION, bearer())
                .body(Body::empty())
                .unwrap();
            let res = call(&t, req).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{host} {path}");
            assert_eq!(text(res).await, r#"{"error":"forbidden"}"#);
        }
    }
    let res = call(
        &t,
        Request::get("/api/state")
            .header(header::HOST, "localhost:7379")
            .header(header::AUTHORIZATION, bearer())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_api_needs_the_bearer_token_and_a_cookie_does_not_sign_in() {
    let t = setup("bearer", Vec::new());
    let res = call(&t, get("/api/state").body(Body::empty()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(text(res).await, r#"{"error":"not signed in"}"#);
    let refused = [
        (header::AUTHORIZATION, format!("Bearer {}", "f".repeat(64))),
        (header::AUTHORIZATION, format!("Bearer {}0", token())),
        (header::AUTHORIZATION, token()),
        (header::AUTHORIZATION, format!("Basic {}", token())),
        // The old cookie no longer signs in.
        (header::COOKIE, format!("rl_session={}", token())),
    ];
    for (name, value) in refused {
        let req = get("/api/state")
            .header(name.clone(), value.clone())
            .body(Body::empty())
            .unwrap();
        let res = call(&t, req).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{name} {value}");
        assert!(!text(res).await.contains(&token()));
    }
    // Two Authorization headers: refused even when one is right.
    let two = get("/api/state")
        .header(header::AUTHORIZATION, bearer())
        .header(header::AUTHORIZATION, "Bearer x")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&t, two).await.status(), StatusCode::UNAUTHORIZED);
    // The scheme is case-insensitive.
    let lower = get("/api/state")
        .header(header::AUTHORIZATION, format!("bearer {}", token()))
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&t, lower).await.status(), StatusCode::OK);
    let res = call(&t, signed_get("/api/state")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = text(res).await;
    assert!(body.contains(r#""status":"idle""#), "{body}");
    assert!(res_has_no_cookie(&t).await);
    // The event stream too.
    let res = call(&t, get("/api/events").body(Body::empty()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = call(&t, signed_get("/api/events")).await;
    assert_eq!(res.status(), StatusCode::OK);
}

/// No response sets a cookie.
async fn res_has_no_cookie(t: &T) -> bool {
    for req in [
        get("/").body(Body::empty()).unwrap(),
        signed_get("/api/state"),
        get(&format!("/auth/{}", token()))
            .body(Body::empty())
            .unwrap(),
    ] {
        if call(t, req)
            .await
            .headers()
            .get(header::SET_COOKIE)
            .is_some()
        {
            return false;
        }
    }
    true
}

#[tokio::test]
async fn a_request_that_is_not_a_get_needs_origin_header_and_json() {
    let t = setup("writes", Vec::new());
    let base = || {
        Request::post("/api/disconnect")
            .header(header::HOST, HOST)
            .header(header::AUTHORIZATION, bearer())
    };
    let cases = [
        base()
            .header("x-redis-lens", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        base()
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        base()
            .header(header::ORIGIN, ORIGIN)
            .header("x-redis-lens", "1")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("{}"))
            .unwrap(),
        base()
            .header(header::ORIGIN, "http://evil.example")
            .header("x-redis-lens", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    ];
    for (i, req) in cases.into_iter().enumerate() {
        let res = call(&t, req).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "case {i}");
        assert_security_headers(&res, "refused write");
    }
    let res = call(&t, write("POST", "/api/disconnect", "")).await;
    assert_eq!(res.status(), StatusCode::OK);
    // With everything but the token: 401.
    let req = Request::post("/api/disconnect")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, ORIGIN)
        .header("x-redis-lens", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(call(&t, req).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_page_is_served_without_the_token_and_assets_have_their_types() {
    let t = setup("page", Vec::new());
    let file = |name: &str| {
        assets::FILES
            .iter()
            .find(|(p, _)| *p == name)
            .map(|(_, b)| b.to_vec())
            .unwrap()
    };
    let res = call(&t, get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert_eq!(text(res).await.into_bytes(), file("index.html"));
    let res = call(&t, signed_get("/")).await;
    assert_eq!(text(res).await.into_bytes(), file("index.html"));
    for (path, ct) in [
        ("app.js", "text/javascript; charset=utf-8"),
        ("views/cmdline.js", "text/javascript; charset=utf-8"),
        ("app.css", "text/css; charset=utf-8"),
        ("mark.svg", "image/svg+xml"),
        ("locked.html", "text/html; charset=utf-8"),
    ] {
        let res = call(
            &t,
            get(&format!("/assets/{path}")).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert_eq!(res.headers()[header::CONTENT_TYPE], ct, "{path}");
        assert_eq!(text(res).await.into_bytes(), file(path), "{path}");
    }
    for path in ["../Cargo.toml", "views/../app.js", "tests", "nope.js"] {
        let res = call(
            &t,
            get(&format!("/assets/{path}")).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn a_bad_body_is_refused_without_echoing_it() {
    let t = setup("body", Vec::new());
    let res = call(
        &t,
        write(
            "POST",
            "/api/connections",
            r#"{"name": 5, "url": "redis://h", "password": 123456789}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = text(res).await;
    assert_eq!(body, format!(r#"{{"error":"{}"}}"#, api::BAD_BODY));
    let res = call(
        &t,
        write(
            "POST",
            "/api/connections",
            r#"{"password": "S3cret-in-body"#,
        ),
    )
    .await;
    assert!(!text(res).await.contains("S3cret"));
}

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}

#[tokio::test]
async fn adding_a_connection_keeps_its_credentials_out_of_every_reply() {
    let t = setup("add", Vec::new());
    let res = call(
        &t,
        write(
            "POST",
            "/api/connections",
            r#"{"name":"cache","url":"redis://alice-user@127.0.0.1:6390/1",
                "password":"S3cret-body-pw","env":"stage","tags":["team-a"]}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = text(res).await;
    assert!(!body.contains("S3cret-body-pw"), "{body}");
    assert!(!body.contains("alice-user"), "{body}");
    let v = json(&body);
    assert_eq!(v["connection"]["url"], "redis://127.0.0.1:6390/1");
    assert_eq!(v["connection"]["auth"], "keychain");
    // The credentials went to the store, and only there.
    let r = SecretRef {
        service: SECRET_SERVICE.into(),
        account: "profile:cache".into(),
    };
    let saved = crate::secrets::SecretStore::get(t.store.as_ref(), &r)
        .unwrap()
        .unwrap();
    assert_eq!(saved.username.as_deref(), Some("alice-user"));
    let file = std::fs::read_to_string(&t.config).unwrap();
    assert!(
        !file.contains("S3cret") && !file.contains("alice-user"),
        "{file}"
    );
    let res = call(&t, signed_get("/api/connections")).await;
    let body = text(res).await;
    assert!(
        !body.contains("S3cret") && !body.contains("alice-user"),
        "{body}"
    );
    let v = json(&body);
    assert_eq!(v["connections"][0]["name"], "cache");
    assert_eq!(v["connections"][0]["tags"][0], "team-a");
    // A password in the URL goes to the store too, and an error never shows it.
    let res = call(
        &t,
        write(
            "POST",
            "/api/connections",
            r#"{"name":"cache","url":"redis://bob:url-S3cret@h:1"}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = text(res).await;
    assert!(body.contains("already exists"), "{body}");
    assert!(
        !body.contains("url-S3cret") && !body.contains("bob"),
        "{body}"
    );
    // Removing it removes its entry.
    let res = call(&t, write("DELETE", "/api/connections/cache", "")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        crate::secrets::SecretStore::get(t.store.as_ref(), &r)
            .unwrap()
            .is_none()
    );
}

async fn answer(t: &T, id: &str, typed: Option<&str>) -> Response {
    let body = match typed {
        Some(s) => serde_json::json!({ "typed": s }).to_string(),
        None => "{}".into(),
    };
    call(t, write("POST", &format!("/api/confirm/{id}"), &body)).await
}

#[tokio::test]
async fn a_token_command_is_confirmed_before_it_is_saved() {
    let t = setup("token", Vec::new());
    let add =
        r#"{"name":"tok","url":"redis://127.0.0.1:6390","token_command":["print-token","--x"]}"#;
    let res = call(&t, write("POST", "/api/connections", add)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let v = json(&text(res).await);
    assert_eq!(v["confirm"]["kind"], "ask");
    assert!(
        v["confirm"]["text"]
            .as_str()
            .unwrap()
            .contains("print-token"),
        "{v}"
    );
    // Nothing is saved before the answer.
    let cfg = crate::config::Config::load(&t.config).unwrap();
    assert!(cfg.get("tok").is_none());
    let id = v["confirm"]["id"].as_str().unwrap().to_string();
    let res = answer(&t, &id, None).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let cfg = crate::config::Config::load(&t.config).unwrap();
    assert_eq!(cfg.get("tok").unwrap().auth, Auth::Command);
    // The id is used up.
    assert_eq!(answer(&t, &id, None).await.status(), StatusCode::GONE);
    // A cancelled one is gone too.
    let res = call(
        &t,
        write(
            "POST",
            "/api/connections",
            &add.replace("\"tok\"", "\"tok2\""),
        ),
    )
    .await;
    let v = json(&text(res).await);
    let id = v["confirm"]["id"].as_str().unwrap().to_string();
    let res = call(&t, write("DELETE", &format!("/api/confirm/{id}"), "")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(answer(&t, &id, None).await.status(), StatusCode::GONE);
    assert!(
        crate::config::Config::load(&t.config)
            .unwrap()
            .get("tok2")
            .is_none()
    );
}

#[tokio::test]
async fn turning_writes_on_for_prod_needs_the_name_typed() {
    let t = setup("set", vec![profile("prod-cache", Env::Prod)]);
    let res = call(
        &t,
        write("PATCH", "/api/connections/prod-cache", r#"{"writes":true}"#),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let v = json(&text(res).await);
    assert_eq!(v["confirm"]["kind"], "type");
    assert_eq!(v["confirm"]["expect"], "prod-cache");
    let id = v["confirm"]["id"].as_str().unwrap().to_string();
    let res = answer(&t, &id, Some("prod")).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let cfg = crate::config::Config::load(&t.config).unwrap();
    assert!(cfg.get("prod-cache").unwrap().is_read_only());
    // Asked again, answered right.
    let res = call(
        &t,
        write("PATCH", "/api/connections/prod-cache", r#"{"writes":true}"#),
    )
    .await;
    let v = json(&text(res).await);
    let id = v["confirm"]["id"].as_str().unwrap().to_string();
    let res = answer(&t, &id, Some("prod-cache")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let cfg = crate::config::Config::load(&t.config).unwrap();
    assert!(!cfg.get("prod-cache").unwrap().is_read_only());
    // Off never asks.
    let res = call(
        &t,
        write(
            "PATCH",
            "/api/connections/prod-cache",
            r#"{"writes":false}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = call(
        &t,
        write("PATCH", "/api/connections/nope", r#"{"writes":false}"#),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn panels_without_a_connection_say_so() {
    let t = setup("idle", Vec::new());
    for path in [
        "/api/overview",
        "/api/server/clients",
        "/api/server/slowlog",
        "/api/pubsub/channels",
        "/api/cmdline/commands",
    ] {
        let res = call(&t, signed_get(path)).await;
        assert_eq!(res.status(), StatusCode::CONFLICT, "{path}");
        assert!(text(res).await.contains("not connected"), "{path}");
    }
    let res = call(&t, write("POST", "/api/cmdline", r#"{"line":"GET k"}"#)).await;
    let v = json(&text(res).await);
    assert_eq!(v["status"], "failed");
    let res = call(&t, write("POST", "/api/connect", r#"{"name":"nope"}"#)).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = call(&t, write("POST", "/api/server/monitor", "")).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_typed_line_is_refused_before_anything_is_sent() {
    let t = setup("cmdline", Vec::new());
    for (line, says) in [
        (r#"GET "unclosed"#, "quote"),
        ("101 GET k", "100"),
        ("MONITOR", "monitor on the server screen"),
        ("MULTI", "connection every view shares"),
    ] {
        let body = serde_json::json!({ "line": line }).to_string();
        let res = call(&t, write("POST", "/api/cmdline", &body)).await;
        assert_eq!(res.status(), StatusCode::OK, "{line}");
        let v = json(&text(res).await);
        assert_eq!(v["status"], "note", "{line}: {v}");
        assert!(
            v["text"].as_str().unwrap().to_lowercase().contains(says),
            "{line}: {v}"
        );
    }
    // Credentials typed on the line are masked in what comes back.
    let body = serde_json::json!({ "line": "AUTH bob S3cret-typed" }).to_string();
    let res = call(&t, write("POST", "/api/cmdline", &body)).await;
    let out = text(res).await;
    assert!(!out.contains("S3cret-typed"), "{out}");
}

#[test]
fn the_dashboard_never_shows_its_token_in_debug() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let d = start_with(
            Options {
                port: 0,
                config: temp_config("debug"),
            },
            Arc::new(MemoryStore::default()),
        )
        .unwrap();
        let open = d.open_url();
        let tok = open
            .strip_prefix(&format!("{}#token=", d.url()))
            .expect("the sign-in URL is the page with the token in its fragment")
            .to_string();
        assert_eq!(tok.len(), 64);
        assert!(!format!("{d:?}").contains(&tok));
        assert!(!d.url().contains(&tok));
        assert!(d.url().starts_with("http://127.0.0.1:"));
        d.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(5), d.wait())
            .await
            .expect("it shuts down");
    });
}

#[test]
fn a_port_in_use_is_its_own_error() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let e = start_with(
            Options {
                port,
                config: temp_config("inuse"),
            },
            Arc::new(MemoryStore::default()),
        )
        .unwrap_err();
        assert!(matches!(e, StartError::PortInUse(p) if p == port), "{e}");
    });
}

#[tokio::test]
async fn polling_pauses_and_steps_through_the_tuis_intervals_and_state_says_so() {
    let t = setup("polling", Vec::new());
    let s = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(s["polling"]["paused"], false);
    assert_eq!(
        s["polling"]["interval_secs"], 2,
        "the TUI's 2 s to start with"
    );
    assert_eq!(
        s["polling"]["intervals"],
        serde_json::json!([1, 2, 3, 5, 10, 15, 30, 60])
    );
    // Monitor's limits are known before a run starts.
    assert_eq!(s["monitor_limits"]["max_secs"], 30);
    assert_eq!(s["monitor_limits"]["max_lines"], 10_000);
    let res = call(&t, write("POST", "/api/polling", r#"{"interval_secs":7}"#)).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let res = call(
        &t,
        write(
            "POST",
            "/api/polling",
            r#"{"paused":true,"interval_secs":10}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let s = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(s["polling"]["paused"], true);
    assert_eq!(s["polling"]["interval_secs"], 10);
    assert!(t.hub.polling().paused);
    let res = call(&t, write("POST", "/api/polling", r#"{"paused":false}"#)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(!t.hub.polling().paused);
    assert_eq!(t.hub.polling().every(), std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn removing_an_unknown_connection_is_404_and_the_current_one_clears_the_state() {
    let mut gone = profile("gone", Env::Local);
    // Nothing listens there: the connect fails at once.
    gone.endpoint = Endpoint {
        host: "127.0.0.1".into(),
        port: 1,
        tls: false,
    };
    let t = setup("remove", vec![gone]);
    let res = call(&t, write("DELETE", "/api/connections/nope", "{}")).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = call(&t, write("POST", "/api/connect", r#"{"name":"gone"}"#)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let t0 = std::time::Instant::now();
    loop {
        let s = json(&text(call(&t, signed_get("/api/state")).await).await);
        if s["status"] == "failed" {
            break;
        }
        assert!(t0.elapsed().as_secs() < 20, "{s}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let res = call(&t, write("DELETE", "/api/connections/gone", "{}")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let s = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(s["status"], "idle", "{s}");
    assert_eq!(s["connection"], serde_json::Value::Null, "{s}");
}

#[tokio::test]
async fn a_containers_writes_change_for_the_session_off_at_once_and_on_after_asking() {
    let t = setup("dockerwrites", Vec::new());
    let mut p = profile("redis-dev", Env::Local);
    p.read_only = Some(false);
    t.hub.install_for_test(p, true);
    let res = call(&t, write("POST", "/api/session/writes", r#"{"on":false}"#)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let s = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(s["writes"], false, "{s}");
    let res = call(&t, write("POST", "/api/session/writes", r#"{"on":true}"#)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let v = json(&text(res).await);
    assert_eq!(v["confirm"]["kind"], "ask");
    let id = v["confirm"]["id"].as_str().unwrap().to_string();
    let res = call(&t, write("POST", &format!("/api/confirm/{id}"), "{}")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let s = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(s["writes"], true, "{s}");
    // The config file is not touched: a container has no saved profile.
    let c = crate::config::Config::load(&t.config).unwrap();
    assert!(c.profiles.is_empty());
}

#[tokio::test]
async fn monitor_asks_first_and_never_starts_on_the_request_itself() {
    let t = setup("monitorasks", Vec::new());
    for (env, kind) in [(Env::Local, "ask"), (Env::Prod, "type")] {
        let mut p = profile("cache", env);
        // Read-only profiles ask too: MONITOR writes nothing but slows the server.
        p.read_only = Some(true);
        p.endpoint.tls = false;
        t.hub.install_for_test(p, false);
        let res = call(&t, write("POST", "/api/server/monitor", "{}")).await;
        assert_eq!(res.status(), StatusCode::CONFLICT, "{env:?}");
        let v = json(&text(res).await);
        assert_eq!(v["confirm"]["kind"], kind, "{v}");
        let q = v["confirm"]["text"].as_str().unwrap();
        assert!(q.contains("30 s") && q.contains("10,000 lines"), "{q}");
        assert!(t.hub.with(|c| c.monitor.is_none()).unwrap());
        if env == Env::Prod {
            assert_eq!(v["confirm"]["expect"], "cache");
            let id = v["confirm"]["id"].as_str().unwrap();
            let res = call(
                &t,
                write("POST", &format!("/api/confirm/{id}"), r#"{"typed":"nope"}"#),
            )
            .await;
            assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert!(t.hub.with(|c| c.monitor.is_none()).unwrap());
        }
    }
}

/// Every reply these requests get, scanned for a TUI key, a screen number or a CLI flag
/// (`wording::BANNED`): the dashboard says it in its own terms.
#[tokio::test]
async fn no_reply_names_a_tui_key_a_screen_number_or_a_cli_flag() {
    let mut s = profile("sent", Env::Local);
    s.sentinel = Some(crate::model::Sentinel {
        master: "m".into(),
        nodes: vec![crate::model::SentinelNode {
            host: "127.0.0.1".into(),
            port: 26379,
        }],
        tls: false,
    });
    let mut ro = profile("ro", Env::Local);
    ro.read_only = Some(true);
    ro.endpoint.tls = false;
    let t = setup("wording", vec![profile("cache", Env::Local), s]);
    let mut bodies = Vec::new();
    for (m, path, body) in [
        (
            "POST",
            "/api/connections",
            r#"{"name":"cache","url":"redis://127.0.0.1:6379"}"#,
        ),
        (
            "POST",
            "/api/connections",
            r#"{"name":"u","url":"redis://user@127.0.0.1:6379"}"#,
        ),
        (
            "POST",
            "/api/connections",
            r#"{"name":"tp","url":"redis://127.0.0.1:6379","password":"p","token_command":["x"]}"#,
        ),
        (
            "POST",
            "/api/connections",
            r#"{"name":"s","url":"redis://h","ssh":"u@h:99999"}"#,
        ),
        ("PATCH", "/api/connections/cache", "{}"),
        ("PATCH", "/api/connections/sent", r#"{"standalone":true}"#),
        ("PATCH", "/api/connections/cache", r#"{"writes":true}"#),
    ] {
        let res = call(&t, write(m, path, body)).await;
        bodies.push(format!("{m} {path} {body}: {}", text(res).await));
    }
    t.hub.install_for_test(ro, false);
    for line in ["SUBSCRIBE a", "MONITOR", "SELECT 1", "SET k v", "3 SET k v"] {
        let body = serde_json::json!({ "line": line }).to_string();
        let res = call(&t, write("POST", "/api/cmdline", &body)).await;
        bodies.push(format!("{line}: {}", text(res).await));
    }
    let res = call(
        &t,
        write(
            "POST",
            "/api/pubsub/publish",
            r#"{"channel":"c","message":"m"}"#,
        ),
    )
    .await;
    bodies.push(text(res).await);
    bodies.push(text(call(&t, signed_get("/api/state")).await).await);
    for b in &bodies {
        // The JSON's own `--` never comes up; the text inside does.
        assert_eq!(super::wording::banned(b), None, "{b}");
    }
    // They still say something useful.
    let all = bodies.join("\n");
    assert!(all.contains("Turn writes on in Connections."), "{all}");
    assert!(all.contains("use the Pub/Sub screen"), "{all}");
    assert!(all.contains("tick Replace to overwrite it"), "{all}");
}

/// B1: a URL of another scheme, or a host that holds credentials, is refused with fixed
/// text and never saved; `valkey://` and `valkeys://` are `redis://` and `rediss://`.
#[tokio::test]
async fn other_schemes_and_userinfo_in_a_host_never_reach_the_config_or_a_reply() {
    let t = setup("schemes", Vec::new());
    for url in [
        "http://u:B1-pw-http@h:6379",
        "https://u:B1-pw-https@h:6379",
        "foo://B1-pw-foo@h",
        "u:B1-pw-bare@h:6379",
        "user:B1-pw-se@h:6379,password=x",
        "h@B1-pw-at:6379",
    ] {
        let body = serde_json::json!({ "name": "bad", "url": url }).to_string();
        let res = call(&t, write("POST", "/api/connections", &body)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{url}");
        let reply = text(res).await;
        assert!(!reply.contains("B1-pw"), "{url}: {reply}");
        let v = json(&reply);
        let e = v["error"].as_str().unwrap();
        assert!(
            e.contains(crate::conn::parse::UNSUPPORTED_SCHEME)
                || e.contains(crate::conn::parse::HOST_WITH_USERINFO),
            "{url}: {e}"
        );
    }
    let file = std::fs::read_to_string(&t.config).unwrap();
    assert!(!file.contains("B1-pw") && !file.contains("bad"), "{file}");
    // valkey:// and valkeys:// are accepted, the credentials go to the store.
    for (name, url, shown) in [
        ("vk", "valkey://vu:B1-pw-vk@h:6379/2", "redis://h:6379/2"),
        ("vks", "valkeys://vu:B1-pw-vks@h:6380", "rediss://h:6380/0"),
    ] {
        let body = serde_json::json!({ "name": name, "url": url }).to_string();
        let res = call(&t, write("POST", "/api/connections", &body)).await;
        assert_eq!(res.status(), StatusCode::CREATED, "{url}");
        let reply = text(res).await;
        assert!(!reply.contains("B1-pw") && !reply.contains("vu"), "{reply}");
        assert_eq!(json(&reply)["connection"]["url"], shown);
        let r = SecretRef {
            service: SECRET_SERVICE.into(),
            account: format!("profile:{name}"),
        };
        let saved = crate::secrets::SecretStore::get(t.store.as_ref(), &r)
            .unwrap()
            .unwrap();
        assert_eq!(saved.username.as_deref(), Some("vu"));
    }
    let file = std::fs::read_to_string(&t.config).unwrap();
    assert!(!file.contains("B1-pw") && !file.contains("vu"), "{file}");
    for path in ["/api/connections", "/api/state"] {
        let body = text(call(&t, signed_get(path)).await).await;
        assert!(!body.contains("B1-pw"), "{path}: {body}");
    }
}

/// S2: a name, a host or a tag holding `--` (every punycode host does) keeps every
/// message, the question before writes go on, and the read-only reasons.
#[tokio::test]
async fn names_and_hosts_with_two_dashes_keep_their_messages_and_questions() {
    let t = setup("dashes", Vec::new());
    let res = call(
        &t,
        write(
            "POST",
            "/api/connections",
            r#"{"name":"cache--eu","url":"redis://xn--bcher-kva.example:6391","env":"stage","writes":false,"tags":["team--a"]}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = text(res).await;
    assert!(!body.contains(super::wording::FALLBACK), "{body}");
    assert!(body.contains("cache--eu"), "{body}");
    // The question before writes go on says what yes does.
    let res = call(
        &t,
        write("PATCH", "/api/connections/cache--eu", r#"{"writes":true}"#),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let v = json(&text(res).await);
    let q = v["confirm"]["text"].as_str().unwrap();
    assert!(q.contains("cache--eu") && q.contains("writes"), "{q}");
    assert_ne!(q, super::wording::FALLBACK);
    // Connected to it, read-only: the state, the publish refusal and the command line's
    // note each say why.
    let p = crate::config::Config::load(&t.config)
        .unwrap()
        .get("cache--eu")
        .cloned()
        .unwrap();
    assert!(p.is_read_only());
    t.hub.install_for_test(p, false);
    let state = json(&text(call(&t, signed_get("/api/state")).await).await);
    let off = state["writes_off"].as_str().unwrap();
    assert!(off.contains("cache--eu"), "{off}");
    assert_ne!(off, super::wording::FALLBACK);
    let res = call(
        &t,
        write(
            "POST",
            "/api/pubsub/publish",
            r#"{"channel":"c","message":"m"}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let e = json(&text(res).await)["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(e.contains("cache--eu"), "{e}");
    let res = call(&t, write("POST", "/api/cmdline", r#"{"line":"SET a b"}"#)).await;
    let v = json(&text(res).await);
    let note = v["text"].as_str().unwrap();
    assert_ne!(note, super::wording::FALLBACK, "{v}");
}

/// Sets `name`'s writes in the config file as `w` in the TUI or `conn set` would.
fn flip_writes(t: &T, name: &str, on: bool) {
    let mut c = crate::config::Config::load(&t.config).unwrap();
    c.set_writes(name, on).unwrap();
    c.save(&t.config).unwrap();
}

/// S3 (rule 2): a profile made read-only in the config file while the dashboard is
/// connected refuses SET and PUBLISH at once, an answer to a question asked before is
/// refused, and the state says writes are off.
#[tokio::test]
async fn a_profile_made_read_only_elsewhere_refuses_writes_at_once() {
    let mut p = profile("cache", Env::Stage);
    p.read_only = Some(false);
    let mut pr = profile("prod1", Env::Prod);
    pr.read_only = Some(false);
    let t = setup("follow", vec![p.clone(), pr.clone()]);
    t.hub.install_for_test(p, false);
    let state = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(state["writes"], true, "{state}");
    flip_writes(&t, "cache", false);
    let res = call(
        &t,
        write(
            "POST",
            "/api/pubsub/publish",
            r#"{"channel":"c","message":"m"}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let e = json(&text(res).await);
    assert!(e["error"].as_str().unwrap().contains("read-only"), "{e}");
    // (The command line's SET needs a server's COMMAND INFO: web_p1_it checks it.)
    let state = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(state["writes"], false, "{state}");
    assert!(state["writes_off"].is_string(), "{state}");
    // The file says writes on again: the session follows.
    flip_writes(&t, "cache", true);
    let state = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(state["writes"], true, "{state}");

    // A question asked while writes were on, answered after they went off: refused.
    t.hub.install_for_test(pr, false);
    let res = call(
        &t,
        write(
            "POST",
            "/api/pubsub/publish",
            r#"{"channel":"c","message":"m"}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let v = json(&text(res).await);
    let id = v["confirm"]["id"].as_str().unwrap().to_string();
    flip_writes(&t, "prod1", false);
    let res = answer(&t, &id, Some("prod1")).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let e = json(&text(res).await);
    assert!(e["error"].as_str().unwrap().contains("read-only"), "{e}");
    // A config file that cannot be read turns writes off.
    std::fs::write(&t.config, "not = [valid").unwrap();
    let state = json(&text(call(&t, signed_get("/api/state")).await).await);
    assert_eq!(state["writes"], false, "{state}");
}

/// S4: a line longer than the TUI's panel takes is refused; one line runs at a time.
#[tokio::test]
async fn the_command_line_takes_the_tuis_input_cap_and_one_line_at_a_time() {
    let t = setup("cmdcaps", Vec::new());
    let long = "a".repeat(crate::tui::app::views::cmdline::MAX_INPUT + 1);
    let body = serde_json::json!({ "line": format!("GET {long}") }).to_string();
    let res = call(&t, write("POST", "/api/cmdline", &body)).await;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let e = json(&text(res).await);
    assert_eq!(e["error"], api::cmdline::TOO_LONG);
    // At the cap it is taken (and refused for another reason: no connection).
    let at = "a".repeat(crate::tui::app::views::cmdline::MAX_INPUT - 4);
    let body = serde_json::json!({ "line": format!("GET {at}") }).to_string();
    let res = call(&t, write("POST", "/api/cmdline", &body)).await;
    assert_eq!(res.status(), StatusCode::OK);
    // A line is running: the next one is refused with fixed words.
    let slot = t.hub.line_slot().expect("free");
    assert!(t.hub.line_slot().is_none());
    let res = call(&t, write("POST", "/api/cmdline", r#"{"line":"GET k"}"#)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let e = json(&text(res).await);
    assert_eq!(e["error"], api::cmdline::STILL_RUNNING);
    drop(slot);
    let res = call(&t, write("POST", "/api/cmdline", r#"{"line":"GET k"}"#)).await;
    assert_eq!(res.status(), StatusCode::OK);
    // The slot is free again after each line.
    assert!(t.hub.line_slot().is_some());
}

/// S8: a body over axum's limit is refused in fixed words; `HEAD` is a `GET` without a
/// body and needs the token the same way; `OPTIONS` is no preflight: refused, and no
/// response carries a CORS header.
#[tokio::test]
async fn a_large_body_head_and_options_are_handled_without_cors() {
    let t = setup("methods", Vec::new());
    let big = format!(r#"{{"line":"{}"}}"#, "S3cret".repeat(3 * 1024 * 1024 / 6));
    let res = call(&t, write("POST", "/api/cmdline", &big)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = text(res).await;
    assert_eq!(body, format!(r#"{{"error":"{}"}}"#, api::BAD_BODY));
    let head = |auth: bool| {
        let b = Request::head("/api/state").header(header::HOST, HOST);
        let b = if auth {
            b.header(header::AUTHORIZATION, bearer())
        } else {
            b
        };
        b.body(Body::empty()).unwrap()
    };
    let res = call(&t, head(false)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = call(&t, head(true)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(text(res).await.is_empty());
    let res = call(
        &t,
        Request::head("/")
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    for (origin, path) in [
        ("http://evil.example", "/api/connections"),
        (ORIGIN, "/api/connections"),
        (ORIGIN, "/"),
    ] {
        let req = Request::builder()
            .method("OPTIONS")
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, origin)
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization, x-redis-lens",
            )
            .body(Body::empty())
            .unwrap();
        let res = call(&t, req).await;
        assert!(
            res.status() == StatusCode::FORBIDDEN || res.status() == StatusCode::METHOD_NOT_ALLOWED,
            "{origin} {path}: {}",
            res.status()
        );
        assert_security_headers(&res, "options");
        for (name, _) in res.headers() {
            assert!(!name.as_str().starts_with("access-control-"), "{name}");
        }
    }
}

/// N2: a connection that does not finish its headers is closed after the header read
/// timeout, and past the cap a new connection waits until one closes.
#[tokio::test]
async fn the_server_times_out_slow_headers_and_caps_its_connections() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let serve_with = |limits: Limits| async move {
        let t = setup("limits", Vec::new());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(serve(l, t.router.clone(), rx, limits));
        (t, port, tx)
    };
    // Headers never finished: closed after the timeout.
    let (_t, port, _stop) = serve_with(Limits {
        header_timeout: std::time::Duration::from_millis(300),
        max_connections: 8,
    })
    .await;
    let mut slow = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    slow.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:7379\r\n")
        .await
        .unwrap();
    let t0 = std::time::Instant::now();
    let mut buf = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        slow.read_to_end(&mut buf),
    )
    .await
    .expect("closed after the header timeout")
    .ok();
    assert!(t0.elapsed() >= std::time::Duration::from_millis(250));
    // At the cap a third connection is not served until one of the two closes.
    let (_t, port, _stop) = serve_with(Limits {
        header_timeout: std::time::Duration::from_secs(10),
        max_connections: 2,
    })
    .await;
    let first = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let _second = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut third = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    third
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:7379\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut one = [0u8; 1];
    let early =
        tokio::time::timeout(std::time::Duration::from_millis(400), third.read(&mut one)).await;
    assert!(early.is_err(), "served past the cap");
    drop(first);
    let mut buf = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        third.read_to_end(&mut buf),
    )
    .await
    .expect("served once one closed")
    .unwrap();
    let head = String::from_utf8_lossy(&buf);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

// -- phase 2: Browse, Analyze, snapshots, databases --------------------------------

/// A session with `report` as its finished analysis and the live connection closed, so
/// no route sends anything (there is no server behind it).
fn analysed(
    t: &T,
    report: Option<crate::model::ScanReport>,
    perms: Option<crate::model::PermissionReport>,
) {
    t.hub.install_for_test(profile("cache", Env::Local), false);
    t.hub.with_mut(|c| {
        c.session.phase = crate::tui::app::Phase::Done;
        c.session.live = crate::tui::app::views::LiveState::Closed(None);
        c.session.server = report.as_ref().map(|r| r.server.clone());
        c.session.report = report;
        c.perms = perms;
    });
}

async fn get_json(t: &T, path: &str) -> (StatusCode, serde_json::Value) {
    let res = call(t, signed_get(path)).await;
    let status = res.status();
    (status, json(&text(res).await))
}

/// Every phase 2 route and method, as the router serves them.
const PHASE_TWO: &[(&str, &str)] = &[
    ("GET", "/api/browse"),
    ("POST", "/api/browse/more"),
    ("GET", "/api/dbs"),
    ("POST", "/api/db"),
    ("GET", "/api/keys?pattern=61"),
    ("GET", "/api/keys/more?list=1"),
    ("GET", "/api/find?q=a"),
    ("GET", "/api/value?key=61"),
    ("GET", "/api/value/more?view=1"),
    ("GET", "/api/value/item?view=1"),
    ("GET", "/api/analysis"),
    ("POST", "/api/analysis"),
    ("DELETE", "/api/analysis"),
    ("GET", "/api/analysis/big_keys"),
    ("GET", "/api/analysis/expiry"),
    ("GET", "/api/analysis/changes"),
    ("GET", "/api/analysis/hot_keys"),
    ("GET", "/api/analysis/nodes"),
    ("GET", "/api/analysis/slots"),
    ("GET", "/api/snapshots"),
    ("POST", "/api/snapshots"),
    ("GET", "/api/snapshots/compare?a=current&b=current"),
];

/// S3: every phase 2 route and method needs the token; every one that is not a GET
/// needs the dashboard's Origin, its header and a JSON body too, with the token given.
#[tokio::test]
async fn phase_two_routes_need_the_token_and_writes_the_guard() {
    let t = setup("p2guard", Vec::new());
    analysed(&t, Some(crate::tui::fixtures::report(true)), None);
    guarded(&t, PHASE_TWO).await;
}

/// Every route of `routes` needs the token, the loopback `Host`, and (all but `GET`) the
/// dashboard's Origin, its header and a JSON body.
async fn guarded(t: &T, routes: &[(&str, &str)]) {
    for (method, path) in routes {
        let req = Request::builder()
            .method(*method)
            .uri(*path)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header("x-redis-lens", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let res = call(t, req).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
        assert_security_headers(&res, path);
        let wrong_host = Request::builder()
            .method(*method)
            .uri(*path)
            .header(header::HOST, "evil.example:7379")
            .header(header::AUTHORIZATION, bearer())
            .body(Body::empty())
            .unwrap();
        let res = call(t, wrong_host).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {path}");
        if *method == "GET" {
            continue;
        }
        let refused = [
            ("http://evil.example", Some("1"), "application/json"),
            ("http://localhost:7380", Some("1"), "application/json"),
            (ORIGIN, None, "application/json"),
            (ORIGIN, Some("1"), "text/plain"),
        ];
        for (origin, marker, kind) in refused {
            let mut req = Request::builder()
                .method(*method)
                .uri(*path)
                .header(header::HOST, HOST)
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, kind)
                .header(header::AUTHORIZATION, bearer());
            if let Some(m) = marker {
                req = req.header("x-redis-lens", m);
            }
            let res = call(t, req.body(Body::from("{}")).unwrap()).await;
            assert_eq!(
                res.status(),
                StatusCode::FORBIDDEN,
                "{method} {path} from {origin} {marker:?} {kind}"
            );
        }
    }
}

/// S2: a query parameter missing or not of its type is refused with 400 and the one
/// fixed sentence, as JSON: never axum's plain-text rejection.
#[tokio::test]
async fn a_bad_or_missing_query_parameter_is_refused_as_json() {
    let t = setup("p2query", Vec::new());
    for path in [
        // Missing.
        "/api/find",
        "/api/keys",
        "/api/keys/more",
        "/api/value",
        "/api/value/more",
        "/api/value/item",
        "/api/snapshots/compare?a=current",
        // Malformed.
        "/api/keys/more?list=x",
        "/api/keys?pattern=61&node=first",
        "/api/value/more?view=abc",
        "/api/value/item?view=abc",
        "/api/value/item?view=1&index=-1",
        "/api/value/item?view=1&width=wide",
        "/api/browse?node=-1",
        "/api/analysis/slots?refresh=true",
    ] {
        let res = call(&t, signed_get(path)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(
            res.headers()[header::CONTENT_TYPE],
            "application/json",
            "{path}"
        );
        let body = text(res).await;
        assert!(!body.contains("deserialize"), "{path}: {body}");
        assert_eq!(
            json(&body)["error"],
            super::api::BAD_QUERY,
            "{path}: {body}"
        );
    }
    assert_eq!(super::wording::banned(super::api::BAD_QUERY), None);
    assert_eq!(
        super::wording::server_free(super::api::BAD_QUERY),
        super::api::BAD_QUERY
    );
}

#[tokio::test]
async fn without_a_report_the_panels_say_so_and_nothing_is_saved() {
    let t = setup("p2noreport", Vec::new());
    analysed(&t, None, None);
    for panel in ["big_keys", "expiry", "changes", "hot_keys", "nodes"] {
        let (status, v) = get_json(&t, &format!("/api/analysis/{panel}")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{panel}");
        assert_eq!(v["error"], super::api::analyze::NO_REPORT, "{panel}");
    }
    let res = call(&t, write("POST", "/api/snapshots", "")).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        json(&text(res).await)["error"],
        super::api::snapshots::NOTHING_TO_SAVE
    );
    let (_, s) = get_json(&t, "/api/state").await;
    assert_eq!(s["analysis"]["status"], "none", "{s}");
    assert_eq!(s["analysis"]["report"], serde_json::Value::Null, "{s}");
}

#[tokio::test]
async fn hot_keys_degrade_in_the_tuis_words() {
    let t = setup("p2hot", Vec::new());
    analysed(&t, Some(crate::tui::fixtures::report_lru()), None);
    let (status, v) = get_json(&t, "/api/analysis/hot_keys").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["off"],
        "The eviction policy is allkeys-lru, so Redis keeps no LFU counters. Set \
         maxmemory-policy to allkeys-lfu or volatile-lfu to see hot keys.",
        "{v}"
    );
    analysed(
        &t,
        Some(crate::tui::fixtures::report(true)),
        Some(crate::tui::fixtures::perms_freq_denied()),
    );
    let (_, v) = get_json(&t, "/api/analysis/hot_keys").await;
    assert_eq!(v["off"], "OBJECT FREQ was denied for this user.", "{v}");
    let mut hot = crate::tui::fixtures::report(true);
    hot.hot_keys = crate::tui::fixtures::hot_keys();
    analysed(&t, Some(hot), Some(crate::tui::fixtures::perms_ok()));
    let (_, v) = get_json(&t, "/api/analysis/hot_keys").await;
    assert_eq!(v["rows"][1]["key"]["name"], "queue:emails", "{v}");
    assert_eq!(v["rows"][1]["hot_and_big"], true, "{v}");
}

#[tokio::test]
async fn a_standalone_server_has_no_nodes_or_slots_and_a_cluster_no_databases() {
    let t = setup("p2cluster", Vec::new());
    analysed(&t, Some(crate::tui::fixtures::report(true)), None);
    let (_, v) = get_json(&t, "/api/analysis/nodes").await;
    assert_eq!(v["off"], super::api::analyze::NODES_NOT_CLUSTER);
    let (_, v) = get_json(&t, "/api/analysis/slots").await;
    assert_eq!(v["off"], crate::tui::app::views::slots::NOT_CLUSTER, "{v}");
    t.hub.with_mut(|c| {
        c.session.nodes = vec![crate::live::nodes::NodeInfo {
            id: "n1".into(),
            endpoint: Endpoint {
                host: "10.0.0.1".into(),
                port: 7000,
                tls: false,
            },
            role: crate::model::NodeRole::Primary,
            slots: vec![(0, 16383)],
            connected: true,
        }];
    });
    let (status, v) = get_json(&t, "/api/dbs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["off"], "A cluster only has database 0.", "{v}");
    let res = call(&t, write("POST", "/api/db", r#"{"db":1}"#)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_panels_come_from_the_report_with_names_escaped() {
    let t = setup("p2panels", Vec::new());
    let mut r = crate::tui::fixtures::report(true);
    r.patterns[0].examples[0].key = "evil\x1b[2Jkey".into();
    analysed(&t, Some(r), Some(crate::tui::fixtures::perms_ok()));
    let (status, v) = get_json(&t, "/api/browse").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["source"], "analysis");
    let body = v.to_string();
    assert!(!body.contains('\u{1b}'), "an escape reached the page");
    assert!(body.contains("evil\\\\x1b[2Jkey"), "{body}");
    let (_, e) = get_json(&t, "/api/analysis/expiry").await;
    assert!(e["rows"].as_array().is_some_and(|r| !r.is_empty()), "{e}");
    let (_, b) = get_json(&t, "/api/analysis/big_keys").await;
    assert_eq!(b["facts_read"], false, "{b}");
    assert_eq!(
        b["notes"][0], "Lengths and encodings are read once the live connection is open.",
        "{b}"
    );
    let (_, c) = get_json(&t, "/api/analysis/changes").await;
    assert_eq!(c["status"], "pending", "{c}");
    let (_, s) = get_json(&t, "/api/state").await;
    assert_eq!(s["analysis"]["status"], "done", "{s}");
    assert!(
        s["analysis"]["report"]["age_secs"].as_i64().unwrap() > 0,
        "{s}"
    );
    let (_, o) = get_json(&t, "/api/overview").await;
    assert!(o["analysis"]["findings"].is_array(), "{o}");
    let (status, a) = get_json(&t, "/api/analysis").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(a["estimates"][0]["depth"], "quick", "{a}");
    assert_eq!(a["estimates"][1]["sample_limit"], 100_000, "{a}");
}

#[tokio::test]
async fn bad_ids_globs_and_names_are_refused_in_fixed_words() {
    let t = setup("p2refused", Vec::new());
    analysed(&t, None, None);
    let (status, v) = get_json(&t, "/api/keys?pattern=zz").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], crate::web::keyname::BAD_ID);
    let (status, v) = get_json(&t, "/api/value?key=fffe").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    // Small 8: the live state is checked before the glob, as in the TUI.
    let (status, v) = get_json(&t, "/api/find?q=*a*b*c*d*e*f*g*h*i*").await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], super::api::keys::FIND_CLOSED);
    t.hub
        .with_mut(|c| c.session.live = crate::tui::app::views::LiveState::Ready);
    let (status, v) = get_json(&t, "/api/find?q=*a*b*c*d*e*f*g*h*i*").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(v["error"].as_str().unwrap().contains("at most 8"), "{v}");
    t.hub
        .with_mut(|c| c.session.live = crate::tui::app::views::LiveState::Closed(None));
    let (status, v) = get_json(&t, "/api/find?q=%01%02").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], super::api::keys::EMPTY_FIND);
    // The live connection is closed: a find says so.
    let (status, v) = get_json(&t, "/api/find?q=user").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"], super::api::keys::FIND_CLOSED);
    let (status, _) = get_json(&t, "/api/snapshots/compare?a=../config.toml&b=current").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, v) = get_json(&t, "/api/browse?sort=size").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], super::api::browse::BAD_SORT);
    let res = call(&t, write("POST", "/api/analysis", r#"{"depth":"full"}"#)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = call(&t, write("DELETE", "/api/analysis", "")).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        json(&text(res).await)["error"],
        crate::web::session::NO_ANALYSIS
    );
}

#[tokio::test]
async fn a_saved_snapshot_is_the_tuis_file_and_compares_with_the_current_report() {
    let dir = temp_config("p2snapdir").parent().unwrap().join("snapshots");
    let config = temp_config("p2snap");
    let store = Arc::new(MemoryStore::default());
    let hub = session::Hub::with_snapshots(config, store, Some(dir.clone()));
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let router = router(app_state(
        Arc::clone(&hub),
        PORT,
        Token::for_test(&token()),
        rx,
    ));
    let t = T {
        router,
        hub,
        store: Arc::new(MemoryStore::default()),
        config: dir.join("unused.toml"),
        _shutdown: _tx,
    };
    // An older analysis on disk, then the current one.
    let mut old = crate::tui::fixtures::previous_report(true);
    old.profile = "cache".into();
    crate::snapshot::save(&dir, &old).unwrap();
    let mut r = crate::tui::fixtures::report(true);
    r.profile = "cache".into();
    analysed(&t, Some(r.clone()), None);
    let res = call(&t, write("POST", "/api/snapshots", "")).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let saved = json(&text(res).await);
    let file = saved["file"].as_str().unwrap().to_string();
    assert_eq!(
        file,
        format!("{}.json", crate::snapshot::timestamp_name(r.taken_at_unix))
    );
    // The TUI's own reader reads it back as the same report.
    let on_disk = crate::snapshot::list(&dir, "cache").unwrap();
    assert_eq!(on_disk.len(), 2);
    assert_eq!(crate::snapshot::load(&on_disk[0].path).unwrap(), r);
    let (_, l) = get_json(&t, "/api/snapshots").await;
    assert_eq!(l["snapshots"].as_array().unwrap().len(), 2, "{l}");
    assert_eq!(l["saved"], true, "{l}");
    let older = l["snapshots"][1]["file"].as_str().unwrap().to_string();
    let (status, d) = get_json(&t, &format!("/api/snapshots/compare?a={older}&b={file}")).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["before_taken_at_unix"], old.taken_at_unix, "{d}");
    assert!(d["rows"].as_array().is_some_and(|r| !r.is_empty()), "{d}");
    let (status, _) = get_json(&t, &format!("/api/snapshots/compare?a=current&b={older}")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = get_json(
        &t,
        "/api/snapshots/compare?a=20200101T000000Z.json&b=current",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    let _ = std::fs::remove_dir_all(&dir);
}

// -- the verifier's findings: B1, S1, Small 1, 5, 6, 9 --------------------------------

/// Sends `req` in a task of its own: a request that waits while the test answers it.
fn spawn_call(t: &T, req: Request<Body>) -> tokio::task::JoinHandle<Response> {
    let router = t.router.clone();
    tokio::spawn(async move { router.oneshot(req).await.unwrap() })
}

/// Waits (a little) until `want` holds.
async fn until(what: &str, want: impl Fn() -> bool) {
    for _ in 0..500 {
        if want() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("{what}: not in time");
}

/// The browser goes while the request waits for the server: the future is dropped.
async fn dropped(t: &T, path: &str) {
    let gone = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        call(t, signed_get(path)),
    )
    .await;
    assert!(gone.is_err(), "{path} waits for its answer");
}

fn key_row(key: &str) -> crate::live::keys::KeyRow {
    crate::live::keys::KeyRow {
        key: key.into(),
        key_type: crate::model::KeyType::String,
        ttl: crate::model::Ttl::Persistent,
        bytes: None,
        encoding: None,
        length: None,
        freq: None,
    }
}

fn keys_page(keys: &[&str], cursor: u64) -> session::LiveResult {
    Ok(crate::live::LiveReply::Keys(
        crate::live::keys::Reply::Page(crate::live::keys::Page {
            rows: keys.iter().map(|k| key_row(k)).collect(),
            cursor,
            carry: Vec::new(),
            other_pattern: 0,
            unreadable: 0,
            refused: Vec::new(),
        }),
    ))
}

fn list_page(items: &[&str], more: bool) -> session::LiveResult {
    use crate::live::values::{At, Items, Page, Reply};
    Ok(crate::live::LiveReply::Value(Reply::Page(Page {
        type_name: Some("list".into()),
        items: Items::List(items.iter().map(|i| crate::value::Raw::from(*i)).collect()),
        next: more.then_some(At::Index(2)),
        refused: Vec::new(),
    })))
}

/// S1: a request the browser drops while it waits for the server leaves no name page,
/// key list or value loading: the answer still lands, and the next request works.
#[tokio::test]
async fn a_request_dropped_mid_read_leaves_nothing_loading() {
    let t = setup("p2dropped", Vec::new());
    t.hub.install_for_test(profile("cache", Env::Local), false);
    t.hub.hold_requests(true);

    // Browse › Keys from names.
    dropped(&t, "/api/browse").await;
    assert_eq!(t.hub.pending_len(), 1);
    assert!(t.hub.answer_oldest(Ok(crate::live::LiveReply::Names(
        crate::live::names::Reply::Page(crate::live::names::Page {
            names: vec!["user:1".into(), "user:2".into()],
            cursor: 0,
            unreadable: 0,
        })
    ))));
    let hub = Arc::clone(&t.hub);
    until("the page of names is taken", || {
        hub.with(|c| !c.browse.names.is_reading() && c.browse.names.pages == 1)
            .unwrap_or(false)
    })
    .await;
    let (status, b) = get_json(&t, "/api/browse?node=0").await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["source"], "names", "{b}");
    assert_eq!(b["names"]["reading"], false, "{b}");
    assert_eq!(b["rows"][0]["pattern"]["name"], "user:{id}", "{b}");
    // Small 9: names are not narrowed to a node, so the node is not echoed.
    assert_eq!(b["node"], serde_json::Value::Null, "{b}");
    let res = call(&t, write("POST", "/api/browse/more", "")).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let e = json(&text(res).await);
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .starts_with("Every key name is read"),
        "not stuck reading: {e}"
    );

    // A pattern's keys.
    let pattern: String = "user:{id}".bytes().map(|b| format!("{b:02x}")).collect();
    dropped(&t, &format!("/api/keys?pattern={pattern}")).await;
    assert!(t.hub.answer_oldest(keys_page(&["user:1"], 5)));
    until("the key list's page is taken", || {
        hub.with(|c| c.browse.lists.loading_of(1) == Some(false))
            .unwrap_or(false)
    })
    .await;
    let more = spawn_call(&t, signed_get("/api/keys/more?list=1"));
    until("the next page is asked for", || hub.pending_len() == 1).await;
    // A second "more" while it loads is refused, and sends nothing.
    let sent = t.hub.sent_count();
    let (status, v) = get_json(&t, "/api/keys/more?list=1").await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], super::api::keys::LOADING);
    assert_eq!(t.hub.sent_count(), sent);
    assert!(t.hub.answer_oldest(keys_page(&["user:2"], 0)));
    let res = more.await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let l = json(&text(res).await);
    // Small 6: a next page's reply holds only the rows it added.
    assert_eq!(l["rows"].as_array().unwrap().len(), 1, "{l}");
    assert_eq!(l["rows"][0]["name"], "user:2", "{l}");
    assert_eq!(
        (l["new_from"].clone(), l["loaded"].clone()),
        (1.into(), 2.into())
    );
    assert_eq!(l["done"], true, "{l}");
    assert_eq!(l["capped"], false, "{l}");

    // A value.
    let key: String = "queue".bytes().map(|b| format!("{b:02x}")).collect();
    dropped(&t, &format!("/api/value?key={key}&type=list")).await;
    assert!(t.hub.answer_oldest(list_page(&["a", "b"], true)));
    until("the value's page is taken", || {
        hub.with(|c| c.browse.values.loading_of(1) == Some(false))
            .unwrap_or(false)
    })
    .await;
    let more = spawn_call(&t, signed_get("/api/value/more?view=1"));
    until("the next page is asked for", || hub.pending_len() == 1).await;
    // Small 1: a second "more" while a page loads reads nothing twice.
    let sent = t.hub.sent_count();
    let (status, v) = get_json(&t, "/api/value/more?view=1").await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], super::api::values::STILL_LOADING);
    assert_eq!(t.hub.sent_count(), sent);
    assert!(t.hub.answer_oldest(list_page(&["c"], false)));
    let res = more.await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = json(&text(res).await);
    assert_eq!(v["loaded"], 3, "{v}");
    assert_eq!(v["items"][2]["value"], "c", "{v}");
    assert_eq!(v["loading"], false, "{v}");
    assert_eq!(t.hub.pending_len(), 0);
}

/// Small 5: two Big keys requests at once read the facts once, and Overview hears of
/// them (its findings use them).
#[tokio::test]
async fn big_keys_facts_are_read_once_and_overview_hears_of_them() {
    let t = setup("p2facts", Vec::new());
    analysed(
        &t,
        Some(crate::tui::fixtures::report(true)),
        Some(crate::tui::fixtures::perms_ok()),
    );
    t.hub
        .with_mut(|c| c.session.live = crate::tui::app::views::LiveState::Ready);
    t.hub.hold_requests(true);
    let mut l = t.hub.listen();
    let a = spawn_call(&t, signed_get("/api/analysis/big_keys"));
    let hub = Arc::clone(&t.hub);
    until("the facts are asked for", || hub.pending_len() == 1).await;
    let b = spawn_call(&t, signed_get("/api/analysis/big_keys"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(t.hub.sent_count(), 1, "read once");
    assert!(!b.is_finished(), "the second waits for the first's read");
    let facts = vec![(
        "big:1".to_string(),
        crate::analysis::KeyFacts {
            length: Some(3),
            encoding: Some("listpack".into()),
        },
    )];
    assert!(t.hub.answer_oldest(Ok(crate::live::LiveReply::Keys(
        crate::live::keys::Reply::Facts {
            facts,
            refused: Vec::new(),
        }
    ))));
    for res in [a.await.unwrap(), b.await.unwrap()] {
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json(&text(res).await)["facts_read"], true);
    }
    assert_eq!(t.hub.sent_count(), 1);
    let mut overview = false;
    while let Ok(e) = l.rx.try_recv() {
        overview |= e.name == "overview";
    }
    assert!(
        overview,
        "an overview event went out once the facts were in"
    );
}

/// A PUBLISH the gate asks about: the question's id.
async fn ask(t: &T) -> String {
    let req = write(
        "POST",
        "/api/pubsub/publish",
        r#"{"channel":"c","message":"m"}"#,
    );
    let res = call(t, req).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    json(&text(res).await)["confirm"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Answers the switch's or the analysis's connect as if it finished: the live
/// connection of the new run is ready (no server stands behind this test).
async fn new_run_ready(t: &T) {
    let hub = Arc::clone(&t.hub);
    // The connect to nowhere fails first; then the run is made ready by hand.
    until("the new run's connect ends", || {
        hub.with(|c| c.failed()).unwrap_or(false)
    })
    .await;
    t.hub.with_mut(|c| {
        c.session.phase = crate::tui::app::Phase::Connected;
        c.session.analysis = None;
        c.session.live = crate::tui::app::views::LiveState::Ready;
    });
}

/// B1 (rule 2): a write question asked before a database switch or an analysis is
/// refused when answered after it, in the CHANGED words, and nothing is sent: the yes
/// was for the database the question was asked on.
#[tokio::test]
async fn a_write_question_is_not_answered_on_a_new_run() {
    let mut p = profile("prod1", Env::Prod);
    p.read_only = Some(false);
    // Nothing listens there: the new runs' connects fail at once.
    p.endpoint = Endpoint {
        host: "127.0.0.1".into(),
        port: 1,
        tls: false,
    };
    let t = setup("p2b1", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    // In the same run the answer is sent (answered as closed: no server here).
    let id = ask(&t).await;
    let sent = t.hub.sent_count();
    let res = answer(&t, &id, Some("prod1")).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(t.hub.sent_count(), sent + 1, "the control write is sent");

    // A database switch between the question and the answer.
    let id = ask(&t).await;
    t.hub.with_mut(|c| c.browse.dbs = Some(vec![0, 1]));
    let res = call(&t, write("POST", "/api/db", r#"{"db":1}"#)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    new_run_ready(&t).await;
    let sent = t.hub.sent_count();
    let res = answer(&t, &id, Some("prod1")).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(&text(res).await)["error"], super::api::writes::CHANGED);
    assert_eq!(t.hub.sent_count(), sent, "nothing is sent to db 1");

    // An analysis between the question and the answer.
    let id = ask(&t).await;
    let res = call(&t, write("POST", "/api/analysis", r#"{"depth":"quick"}"#)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    new_run_ready(&t).await;
    let sent = t.hub.sent_count();
    let res = answer(&t, &id, Some("prod1")).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(&text(res).await)["error"], super::api::writes::CHANGED);
    assert_eq!(t.hub.sent_count(), sent, "nothing is sent");
}

// -- phase 3: key edits and bulk actions ----------------------------------------------

/// Every phase 3 route and method, as the router serves them.
const PHASE_THREE: &[(&str, &str)] = &[
    ("POST", "/api/value/edit"),
    ("POST", "/api/value/add"),
    ("POST", "/api/value/ttl"),
    ("POST", "/api/value/rename"),
    ("POST", "/api/value/remove"),
    ("POST", "/api/value/delete"),
    ("POST", "/api/keys/add"),
    ("GET", "/api/bulk"),
    ("POST", "/api/bulk"),
    ("DELETE", "/api/bulk"),
    ("POST", "/api/bulk/run"),
];

/// S3: every phase 3 route needs the token; every write route the guard's Origin,
/// header and JSON body.
#[tokio::test]
async fn phase_three_routes_need_the_token_and_writes_the_guard() {
    let t = setup("p3guard", Vec::new());
    let mut p = profile("cache", Env::Local);
    p.read_only = Some(false);
    t.hub.install_for_test(p, false);
    guarded(&t, PHASE_THREE).await;
}

fn hexed(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// A value open on view 1 as if its first page had been read: a hash of two fields.
fn open_hash(t: &T) -> u64 {
    use crate::live::values::{Field, Items, Kind};
    t.hub
        .with_mut(|c| {
            let conn = c.id;
            let mut row = key_row("user:1");
            row.key_type = crate::model::KeyType::Hash;
            let id = c.browse.values.add(conn, row, "user:{id}".into());
            let v = c.browse.values.get_mut(id).unwrap();
            v.state.items = Some(Items::Hash(
                ["name", "mail"]
                    .iter()
                    .map(|f| Field {
                        name: crate::value::Raw::from(*f),
                        value: crate::value::Raw::from("v"),
                        ttl: None,
                    })
                    .collect(),
            ));
            v.state.kind = Some(Kind::Hash);
            v.state.type_name = Some("hash".into());
            v.state.pages = 1;
            id
        })
        .unwrap()
}

/// Rule 2: a read-only profile is refused every phase 3 write with the gate's words,
/// whatever else the request names, and nothing reaches the live connection.
#[tokio::test]
async fn a_read_only_profile_is_refused_every_edit_and_bulk_action_and_nothing_is_sent() {
    let mut p = profile("cache", Env::Local);
    p.read_only = Some(true);
    let t = setup("p3ro", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    let view = open_hash(&t);
    t.hub.hold_requests(true);
    let pattern = hexed("user:{id}");
    let bodies = [
        (
            "/api/value/edit",
            format!(r#"{{"view":{view},"rev":0,"index":0,"value":"x"}}"#),
        ),
        (
            "/api/value/add",
            format!(r#"{{"view":{view},"field":"f","value":"x"}}"#),
        ),
        ("/api/value/ttl", format!(r#"{{"view":{view},"ttl":"1h"}}"#)),
        // Even a TTL the TUI's parser refuses is the gate's refusal first.
        ("/api/value/ttl", format!(r#"{{"view":{view},"ttl":"0"}}"#)),
        (
            "/api/value/rename",
            format!(r#"{{"view":{view},"name":"user:2"}}"#),
        ),
        (
            "/api/value/remove",
            format!(r#"{{"view":{view},"rev":0,"index":1}}"#),
        ),
        ("/api/value/delete", format!(r#"{{"view":{view}}}"#)),
        // A view that is not open: still the gate's words.
        ("/api/value/delete", r#"{"view":99}"#.to_string()),
        (
            "/api/keys/add",
            r#"{"name":"k","type":"string","value":"v"}"#.to_string(),
        ),
        (
            "/api/bulk",
            format!(r#"{{"pattern":"{pattern}","action":"delete"}}"#),
        ),
        (
            "/api/bulk",
            format!(r#"{{"pattern":"{pattern}","action":"expire","ttl":"1d"}}"#),
        ),
    ];
    for (path, body) in &bodies {
        let res = call(&t, write("POST", path, body)).await;
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path} {body}"
        );
        let e = json(&text(res).await);
        assert_eq!(
            e["error"], "Not sent: cache is read-only. Turn writes on in Connections.",
            "{path} {body}"
        );
    }
    assert_eq!(t.hub.sent_count(), 0, "nothing went to the live connection");
    assert_eq!(t.hub.pending_len(), 0);
    let (_, b) = get_json(&t, "/api/bulk").await;
    assert_eq!(b["bulk"], serde_json::Value::Null, "{b}");
}

/// Answers the oldest held request with `reply` once one waits.
async fn answer_next(t: &T, reply: session::LiveResult) {
    let hub = Arc::clone(&t.hub);
    until("a request waits", || hub.pending_len() == 1).await;
    assert!(t.hub.answer_oldest(reply));
}

fn hash_page(fields: &[(&str, &str)]) -> session::LiveResult {
    use crate::live::values::{Field, Items, Page, Reply};
    Ok(crate::live::LiveReply::Value(Reply::Page(Page {
        type_name: Some("hash".into()),
        items: Items::Hash(
            fields
                .iter()
                .map(|(f, v)| Field {
                    name: crate::value::Raw::from(*f),
                    value: crate::value::Raw::from(*v),
                    ttl: None,
                })
                .collect(),
        ),
        next: None,
        refused: Vec::new(),
    })))
}

fn wrote(r: crate::live::write::Reply) -> session::LiveResult {
    Ok(crate::live::LiveReply::Write(r))
}

/// An edit is sent through the gate at once outside prod, then the value is read again
/// from its first page (`rev` goes up) and the key's row: as the TUI's `e`. A stale
/// `rev`, an item past what is loaded and an unchanged value send nothing.
#[tokio::test]
async fn an_edit_writes_then_reads_the_value_and_its_row_again() {
    let mut p = profile("cache", Env::Local);
    p.read_only = Some(false);
    let t = setup("p3edit", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    let view = open_hash(&t);
    t.hub.hold_requests(true);

    for (body, status, error) in [
        (
            format!(r#"{{"view":{view},"rev":3,"index":0,"value":"x"}}"#),
            StatusCode::CONFLICT,
            super::api::edits::STALE,
        ),
        (
            format!(r#"{{"view":{view},"rev":0,"index":2,"value":"x"}}"#),
            StatusCode::UNPROCESSABLE_ENTITY,
            super::api::values::NO_ITEM,
        ),
        (
            r#"{"view":42,"rev":0,"index":0,"value":"x"}"#.to_string(),
            StatusCode::NOT_FOUND,
            super::api::values::NO_VIEW,
        ),
    ] {
        let res = call(&t, write("POST", "/api/value/edit", &body)).await;
        assert_eq!(res.status(), status, "{body}");
        assert_eq!(json(&text(res).await)["error"], error, "{body}");
    }
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/edit",
            &format!(r#"{{"view":{view},"rev":0,"index":1,"value":"v"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = json(&text(res).await);
    assert_eq!(v["sent"], false, "{v}");
    assert_eq!(v["message"], super::api::edits::UNCHANGED, "{v}");
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/edit",
            &format!(r#"{{"view":{view},"rev":0,"value":{{"hex":"0"}}}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(t.hub.sent_count(), 0, "none of those sent anything");

    let edit = spawn_call(
        &t,
        write(
            "POST",
            "/api/value/edit",
            &format!(r#"{{"view":{view},"rev":0,"index":1,"value":"new@example"}}"#),
        ),
    );
    // The write, then the value's first page, then the key's row.
    answer_next(&t, wrote(crate::live::write::Reply::Int(0))).await;
    // One write at a time per value: a second edit while the first is on its way.
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/delete",
            &format!(r#"{{"view":{view}}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    answer_next(&t, hash_page(&[("name", "v"), ("mail", "new@example")])).await;
    answer_next(&t, keys_page(&["user:1"], 0)).await;
    let res = edit.await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = json(&text(res).await);
    assert_eq!(v["sent"], true, "{v}");
    assert_eq!(v["done"], true, "{v}");
    assert_eq!(v["message"], "Saved.", "{v}");
    assert_eq!(v["reply"]["kind"], "int", "{v}");
    assert_eq!(v["key"]["name"], "user:1", "{v}");
    assert_eq!(v["value"]["rev"], 1, "{v}");
    assert_eq!(v["value"]["writing"], false, "{v}");
    assert_eq!(v["value"]["items"][1]["value"], "new@example", "{v}");
    assert_eq!(v["row"]["name"], "user:1", "{v}");
    assert_eq!(t.hub.sent_count(), 3);
    // The old rev is stale now.
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/remove",
            &format!(r#"{{"view":{view},"rev":0,"index":0}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(t.hub.sent_count(), 3);
}

/// On prod with writes on an edit asks yes or no, and a delete needs the key's name
/// typed and says it can't be undone; a wrong name sends nothing, the right one sends
/// the write, and the value closes with the key dropped from the lists.
#[tokio::test]
async fn on_prod_edits_ask_and_deletes_need_the_key_typed() {
    let mut p = profile("prod1", Env::Prod);
    p.read_only = Some(false);
    let t = setup("p3prod", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    let view = open_hash(&t);
    t.hub.hold_requests(true);
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/add",
            &format!(r#"{{"view":{view},"field":"age","value":"42"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let q = json(&text(res).await);
    assert_eq!(q["confirm"]["kind"], "ask", "{q}");
    assert_eq!(
        q["confirm"]["text"], "Send HSETNX user:1 age 42 to prod1?",
        "{q}"
    );
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/delete",
            &format!(r#"{{"view":{view}}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let q = json(&text(res).await);
    assert_eq!(q["confirm"]["kind"], "type", "{q}");
    assert_eq!(q["confirm"]["expect"], "user:1", "{q}");
    assert_eq!(
        q["confirm"]["text"], "Type user:1 to send UNLINK user:1 to prod1. This can't be undone.",
        "{q}"
    );
    let id = q["confirm"]["id"].as_str().unwrap().to_string();
    assert_eq!(t.hub.sent_count(), 0, "questions send nothing");
    let res = answer(&t, &id, Some("user:2")).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json(&text(res).await)["error"],
        "Not sent: the name typed did not match."
    );
    assert_eq!(t.hub.sent_count(), 0);
    // The question was used: ask again, answer right.
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/delete",
            &format!(r#"{{"view":{view}}}"#),
        ),
    )
    .await;
    let id = json(&text(res).await)["confirm"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let done = {
        let router = t.router.clone();
        let id = id.clone();
        tokio::spawn(async move {
            router
                .oneshot(write(
                    "POST",
                    &format!("/api/confirm/{id}"),
                    r#"{"typed":"user:1"}"#,
                ))
                .await
                .unwrap()
        })
    };
    answer_next(&t, wrote(crate::live::write::Reply::Int(1))).await;
    let res = done.await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = json(&text(res).await);
    assert_eq!(v["done"], true, "{v}");
    assert_eq!(v["closed"], true, "{v}");
    assert_eq!(v["message"], "Deleted user:1.", "{v}");
    assert_eq!(v["value"], serde_json::Value::Null, "{v}");
    assert_eq!(
        t.hub.sent_count(),
        1,
        "the write alone: nothing is read again"
    );
    let (status, _) = get_json(&t, &format!("/api/value/more?view={view}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the value closed");
}

fn count_reply(walk: crate::live::bulk::Walk, matched: u64) -> session::LiveResult {
    Ok(crate::live::LiveReply::Bulk(
        crate::live::bulk::Reply::Count(crate::live::bulk::Count {
            walk: Some(walk),
            matched,
            with_ttl: 0,
            other_pattern: 1,
            unreadable: 0,
        }),
    ))
}

/// A bulk delete: the dry run's steps as events, nothing asked when nothing would
/// change, then the question (yes or no off prod), and only its answer starts the
/// batches; a stop is honoured after the batch on its way. A database switch between
/// the question and the answer refuses it.
#[tokio::test]
async fn a_bulk_delete_counts_asks_then_runs_and_stops_when_asked() {
    use crate::live::bulk::{Batch, Reply, Walk};
    let mut p = profile("cache", Env::Local);
    p.read_only = Some(false);
    p.endpoint = Endpoint {
        host: "127.0.0.1".into(),
        port: 1,
        tls: false,
    };
    let t = setup("p3bulk", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    t.hub.hold_requests(true);
    let mut l = t.hub.listen();
    let pattern = hexed("cart:{id}");
    let body = format!(r#"{{"pattern":"{pattern}","action":"delete"}}"#);
    let res = call(&t, write("POST", "/api/bulk", &body)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let b = json(&text(res).await);
    assert_eq!(b["bulk"]["stage"], "counting", "{b}");
    assert_eq!(b["bulk"]["glob"], "cart:*", "{b}");
    let id = b["bulk"]["id"].as_u64().unwrap();
    // One at a time.
    let res = call(&t, write("POST", "/api/bulk", &body)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        json(&text(res).await)["error"],
        super::api::bulk::ONE_AT_A_TIME
    );
    let res = call(
        &t,
        write("POST", "/api/bulk/run", &format!(r#"{{"bulk":{id}}}"#)),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT, "not counted yet");
    answer_next(&t, count_reply(Walk::At(5), 2)).await;
    answer_next(&t, count_reply(Walk::Done, 1)).await;
    let hub = Arc::clone(&t.hub);
    until("counted", || {
        hub.with(|c| {
            c.bulk
                .as_ref()
                .is_some_and(|b| matches!(b.stage, super::api::bulk::Stage::Counted))
        })
        .unwrap_or(false)
    })
    .await;
    let (_, b) = get_json(&t, "/api/bulk").await;
    assert_eq!(b["bulk"]["count"]["matched"], 3, "{b}");
    assert_eq!(b["bulk"]["count"]["other_pattern"], 2, "{b}");
    assert_eq!(
        b["bulk"]["summary"][0], "Dry run: 3 keys of the pattern will be deleted with UNLINK.",
        "{b}"
    );
    let mut events = 0;
    while let Ok(e) = l.rx.try_recv() {
        events += usize::from(e.name == "bulk");
    }
    assert!(events >= 3, "{events} bulk events");
    let sent = t.hub.sent_count();
    assert_eq!(sent, 2, "the two dry-run steps");

    // Asked, then cancelled: closed, nothing sent.
    let res = call(
        &t,
        write("POST", "/api/bulk/run", &format!(r#"{{"bulk":{id}}}"#)),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let q = json(&text(res).await);
    assert_eq!(q["confirm"]["kind"], "ask", "{q}");
    let text_of = q["confirm"]["text"].as_str().unwrap().to_string();
    assert!(
        text_of.starts_with("Delete every key of the pattern cart:{id} on cache? Dry run: 3 keys"),
        "{text_of}"
    );
    assert!(text_of.ends_with("500 at a time."), "{text_of}");
    let qid = q["confirm"]["id"].as_str().unwrap().to_string();
    let res = call(&t, write("DELETE", &format!("/api/confirm/{qid}"), "")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let (_, b) = get_json(&t, "/api/bulk").await;
    assert_eq!(b["bulk"]["stage"], "done", "{b}");
    assert_eq!(b["bulk"]["lines"][0], crate::live::CANCELLED, "{b}");
    assert_eq!(t.hub.sent_count(), sent);

    // A stop is honoured exactly: a run held before each batch (`bulk::hold`) is
    // stopped before its first batch, then another between two batches; a third is
    // stopped while a batch is on its way. In no case is a batch sent after the stop.
    let batch = |walk: Walk, changed: u64| {
        Ok(crate::live::LiveReply::Bulk(Reply::Batch(Batch {
            walk,
            carry: Vec::new(),
            seen: changed,
            sent: changed,
            changed,
            skipped: 0,
            failed: 0,
            denied: false,
            moved: 0,
            unreadable: 0,
            file: None,
        })))
    };
    let counted_and_asked = |label: &'static str| {
        let t = &t;
        let body = &body;
        let hub = Arc::clone(&hub);
        async move {
            let res = call(t, write("POST", "/api/bulk", body)).await;
            assert_eq!(res.status(), StatusCode::ACCEPTED, "{label}");
            let id = json(&text(res).await)["bulk"]["id"].as_u64().unwrap();
            answer_next(t, count_reply(Walk::Done, 600)).await;
            until("counted", || {
                hub.with(|c| {
                    c.bulk
                        .as_ref()
                        .is_some_and(|b| matches!(b.stage, super::api::bulk::Stage::Counted))
                })
                .unwrap_or(false)
            })
            .await;
            let res = call(
                t,
                write("POST", "/api/bulk/run", &format!(r#"{{"bulk":{id}}}"#)),
            )
            .await;
            json(&text(res).await)["confirm"]["id"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let settle = || tokio::time::sleep(std::time::Duration::from_millis(30));
    use super::api::bulk::hold;

    // 1. Stopped after the answer, before the first batch went: none goes.
    let qid = counted_and_asked("before the first batch").await;
    hold::hold(&t.config);
    let res = answer(&t, &qid, None).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    assert_eq!(json(&text(res).await)["bulk"]["stage"], "running");
    until("the run waits before its first batch", || {
        hold::arrived(&t.config) == 1
    })
    .await;
    let sent = t.hub.sent_count();
    let res = call(&t, write("DELETE", "/api/bulk", "")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = json(&text(res).await);
    assert_eq!(v["text"], super::api::bulk::STOPPED, "{v}");
    assert_eq!(v["bulk"]["stage"], "done", "ended at once: {v}");
    assert_eq!(
        v["bulk"]["lines"][0],
        super::api::bulk::STOPPED_FIRST,
        "{v}"
    );
    assert_eq!(v["bulk"]["error"], false, "{v}");
    hold::release(&t.config);
    settle().await;
    assert_eq!(t.hub.sent_count(), sent, "no batch after the stop");
    assert_eq!(t.hub.pending_len(), 0, "no batch after the stop");

    // 2. Stopped between two batches: the first answered, the second not sent.
    let res = call(&t, write("DELETE", "/api/bulk", "")).await;
    assert_eq!(json(&text(res).await)["bulk"], serde_json::Value::Null);
    let qid = counted_and_asked("between batches").await;
    hold::hold(&t.config);
    let res = answer(&t, &qid, None).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    until("the run waits before its first batch", || {
        hold::arrived(&t.config) == 1
    })
    .await;
    hold::step(&t.config);
    answer_next(&t, batch(Walk::At(9), 500)).await;
    until("the run waits before its second batch", || {
        hold::arrived(&t.config) == 2
    })
    .await;
    let sent = t.hub.sent_count();
    let res = call(&t, write("DELETE", "/api/bulk", "")).await;
    let v = json(&text(res).await);
    assert_eq!(v["text"], super::api::bulk::STOPPED, "{v}");
    assert_eq!(v["bulk"]["stage"], "done", "{v}");
    assert_eq!(
        v["bulk"]["lines"][0], "Stopped after the batch: deleted 500 keys of cart:{id}.",
        "{v}"
    );
    assert_eq!(v["bulk"]["run"]["changed"], 500, "{v}");
    hold::release(&t.config);
    settle().await;
    assert_eq!(t.hub.sent_count(), sent, "no second batch after the stop");
    assert_eq!(t.hub.pending_len(), 0, "no second batch after the stop");
    let (_, b) = get_json(&t, "/api/bulk").await;
    assert_eq!(b["bulk"]["run"]["changed"], 500, "{b}");

    // 3. Stopped while a batch is on its way: that batch lands, no other is sent.
    let res = call(&t, write("DELETE", "/api/bulk", "")).await;
    assert_eq!(json(&text(res).await)["bulk"], serde_json::Value::Null);
    let qid = counted_and_asked("a batch on its way").await;
    let res = answer(&t, &qid, None).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let h = Arc::clone(&t.hub);
    until("the first batch is on its way", || h.pending_len() == 1).await;
    let sent = t.hub.sent_count();
    let res = call(&t, write("DELETE", "/api/bulk", "")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = json(&text(res).await);
    assert_eq!(v["text"], super::api::bulk::STOPPING, "{v}");
    assert_eq!(v["bulk"]["stage"], "running", "{v}");
    assert_eq!(v["bulk"]["run"]["stopping"], true, "{v}");
    assert!(t.hub.answer_oldest(batch(Walk::At(9), 500)));
    until("stopped", || {
        hub.with(|c| {
            c.bulk
                .as_ref()
                .is_some_and(|b| matches!(b.stage, super::api::bulk::Stage::Done(_)))
        })
        .unwrap_or(false)
    })
    .await;
    let (_, b) = get_json(&t, "/api/bulk").await;
    assert_eq!(
        b["bulk"]["lines"][0], "Stopped after the batch: deleted 500 keys of cart:{id}.",
        "{b}"
    );
    assert_eq!(b["bulk"]["run"]["changed"], 500, "{b}");
    settle().await;
    assert_eq!(t.hub.sent_count(), sent, "the batch on its way only");
    assert_eq!(t.hub.pending_len(), 0, "no batch after the stop");

    // A database switch between the question and its answer: refused, nothing sent.
    let res = call(&t, write("DELETE", "/api/bulk", "")).await;
    assert_eq!(json(&text(res).await)["bulk"], serde_json::Value::Null);
    let res = call(&t, write("POST", "/api/bulk", &body)).await;
    let id = json(&text(res).await)["bulk"]["id"].as_u64().unwrap();
    answer_next(&t, count_reply(Walk::Done, 4)).await;
    until("counted", || {
        hub.with(|c| {
            c.bulk
                .as_ref()
                .is_some_and(|b| matches!(b.stage, super::api::bulk::Stage::Counted))
        })
        .unwrap_or(false)
    })
    .await;
    let res = call(
        &t,
        write("POST", "/api/bulk/run", &format!(r#"{{"bulk":{id}}}"#)),
    )
    .await;
    let qid = json(&text(res).await)["confirm"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.hub.hold_requests(false);
    t.hub.with_mut(|c| c.browse.dbs = Some(vec![0, 1]));
    let res = call(&t, write("POST", "/api/db", r#"{"db":1}"#)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    new_run_ready(&t).await;
    t.hub.hold_requests(true);
    let sent = t.hub.sent_count();
    let res = answer(&t, &qid, None).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(&text(res).await)["error"], super::api::writes::CHANGED);
    assert_eq!(t.hub.sent_count(), sent, "nothing is sent to db 1");
}

// -- the phase 3 verifier's findings: S1, S2, S3, Small 5, 6, 8 ------------------------

fn writable_cache() -> Profile {
    let mut p = profile("cache", Env::Local);
    p.read_only = Some(false);
    // Nothing listens there: a new run's connect fails at once.
    p.endpoint = Endpoint {
        host: "127.0.0.1".into(),
        port: 1,
        tls: false,
    };
    p
}

/// S1 (rule 2): an op built from what the session held is decided only on that
/// connection and run: a new run since (a database switch) is refused with CHANGED
/// before anything is sent. A write not built from the session (`made_for` none) is
/// decided on the session as it is.
#[tokio::test]
async fn an_edit_is_decided_only_for_the_run_it_was_made_on() {
    let p = writable_cache();
    let t = setup("p3s1", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let app = app_state(Arc::clone(&t.hub), PORT, Token::for_test(&token()), rx);
    let op = crate::write::WriteOp::Unlink {
        key: "user:1".into(),
    };
    let made_for = t.hub.with(super::api::writes::MadeFor::of).unwrap();
    assert!(super::api::writes::decide(&app, &op, Some(made_for)).is_ok());
    t.hub.with_mut(|c| c.browse.dbs = Some(vec![0, 1]));
    let res = call(&t, write("POST", "/api/db", r#"{"db":1}"#)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    new_run_ready(&t).await;
    let r = super::api::writes::decide(&app, &op, Some(made_for)).unwrap_err();
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.text, super::api::writes::CHANGED);
    assert!(super::api::writes::decide(&app, &op, None).is_ok());
    // Another connection altogether: refused the same way.
    t.hub.install_for_test(writable_cache(), false);
    let r = super::api::writes::decide(&app, &op, Some(made_for)).unwrap_err();
    assert_eq!(r.text, super::api::writes::CHANGED);
}

/// S2, S3 (the TUI's bulk dialog takes every key until it ends): while a bulk action
/// counts or runs, a database switch, an analysis, a connect, a disconnect and every
/// key edit are refused in the dashboard's words, and nothing is sent. Small 8: once
/// it ended, a connect replaces it with a `bulk` event of `null`.
#[tokio::test]
async fn while_a_bulk_action_counts_or_runs_the_session_and_its_keys_stay_put() {
    use super::api::bulk::{BUSY, hold};
    use crate::live::bulk::{Batch, Reply, Walk};
    let p = writable_cache();
    let t = setup("p3busy", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    let view = open_hash(&t);
    t.hub.with_mut(|c| c.browse.dbs = Some(vec![0, 1]));
    t.hub.hold_requests(true);
    let pattern = hexed("user:{id}");
    let res = call(
        &t,
        write(
            "POST",
            "/api/bulk",
            &format!(r#"{{"pattern":"{pattern}","action":"delete"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let id = json(&text(res).await)["bulk"]["id"].as_u64().unwrap();
    let hub = Arc::clone(&t.hub);
    until("the count step waits", || hub.pending_len() == 1).await;
    let refused = |what: &'static str| {
        let t = &t;
        async move {
            let session = [
                ("/api/db", r#"{"db":1}"#.to_string()),
                ("/api/analysis", r#"{"depth":"quick"}"#.to_string()),
                ("/api/connect", r#"{"name":"cache"}"#.to_string()),
                ("/api/disconnect", "{}".to_string()),
            ];
            let edits = [
                (
                    "/api/value/edit",
                    format!(r#"{{"view":{view},"rev":0,"index":0,"value":"x"}}"#),
                ),
                (
                    "/api/value/add",
                    format!(r#"{{"view":{view},"field":"f","value":"x"}}"#),
                ),
                ("/api/value/ttl", format!(r#"{{"view":{view},"ttl":"1h"}}"#)),
                (
                    "/api/value/rename",
                    format!(r#"{{"view":{view},"name":"user:2"}}"#),
                ),
                (
                    "/api/value/remove",
                    format!(r#"{{"view":{view},"rev":0,"index":1}}"#),
                ),
                ("/api/value/delete", format!(r#"{{"view":{view}}}"#)),
                (
                    "/api/keys/add",
                    r#"{"name":"user:9","type":"string","value":"v"}"#.to_string(),
                ),
                // Phase 4: the stream and Search writes wait for it too.
                (
                    "/api/streams/ack",
                    r#"{"key":"73","group":"67","id":"1-0"}"#.to_string(),
                ),
                (
                    "/api/streams/claim",
                    r#"{"key":"73","group":"67","to":"c","min_idle_ms":"0","count":"1"}"#
                        .to_string(),
                ),
                (
                    "/api/streams/trim",
                    r#"{"key":"73","by":"MAXLEN","value":"1"}"#.to_string(),
                ),
                (
                    "/api/search/create",
                    r#"{"name":"i","on":"HASH","fields":"t TEXT"}"#.to_string(),
                ),
                ("/api/search/drop", r#"{"index":"69"}"#.to_string()),
            ];
            let sent = t.hub.sent_count();
            for (path, body) in session.iter().chain(&edits) {
                let res = call(t, write("POST", path, body)).await;
                assert_eq!(res.status(), StatusCode::CONFLICT, "{what}: {path}");
                assert_eq!(json(&text(res).await)["error"], BUSY, "{what}: {path}");
            }
            assert_eq!(t.hub.sent_count(), sent, "{what}: nothing sent");
            assert_eq!(
                t.hub.with(|c| c.session.scan),
                Some(1),
                "{what}: no new run"
            );
        }
    };
    refused("counting").await;

    // Counted and answered: running, held before its first batch.
    assert!(t.hub.answer_oldest(count_reply(Walk::Done, 5)));
    until("counted", || {
        hub.with(|c| {
            c.bulk
                .as_ref()
                .is_some_and(|b| matches!(b.stage, super::api::bulk::Stage::Counted))
        })
        .unwrap_or(false)
    })
    .await;
    let res = call(
        &t,
        write("POST", "/api/bulk/run", &format!(r#"{{"bulk":{id}}}"#)),
    )
    .await;
    let qid = json(&text(res).await)["confirm"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    hold::hold(&t.config);
    let res = answer(&t, &qid, None).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    until("held", || hold::arrived(&t.config) == 1).await;
    refused("running").await;
    // One batch on its way: still refused.
    hold::release(&t.config);
    until("the batch is on its way", || hub.pending_len() == 1).await;
    refused("a batch on its way").await;
    let res = call(&t, write("DELETE", "/api/bulk", "")).await;
    assert_eq!(json(&text(res).await)["text"], super::api::bulk::STOPPING);
    assert!(
        t.hub
            .answer_oldest(Ok(crate::live::LiveReply::Bulk(Reply::Batch(Batch {
                walk: Walk::At(3),
                carry: Vec::new(),
                seen: 5,
                sent: 5,
                changed: 5,
                skipped: 0,
                failed: 0,
                denied: false,
                moved: 0,
                unreadable: 0,
                file: None,
            }))))
    );
    until("done", || {
        hub.with(|c| c.bulk.as_ref().is_some_and(|b| !b.busy()))
            .unwrap_or(false)
    })
    .await;

    // Ended: an edit goes again, and a connect replaces the session; the page hears
    // that the bulk action went.
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/edit",
            &format!(r#"{{"view":{view},"rev":0,"index":0,"value":"v"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "unchanged, so nothing sent");
    let mut l = t.hub.listen();
    let res = call(&t, write("POST", "/api/connect", r#"{"name":"cache"}"#)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let mut closed = false;
    while let Ok(e) = l.rx.try_recv() {
        if e.name == "bulk" {
            closed = true;
            assert_eq!(
                serde_json::to_value(&e.data).unwrap(),
                serde_json::json!({ "bulk": null })
            );
        }
    }
    assert!(closed, "a bulk event of null");
}

/// Small 5: a write's value stays `writing` until it was read again, so no edit is made
/// from the items before the reread; the reply's body says `writing: false`.
#[tokio::test]
async fn a_value_stays_writing_until_it_was_read_again() {
    let p = writable_cache();
    let t = setup("p3s5", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    let view = open_hash(&t);
    t.hub.hold_requests(true);
    let edit = spawn_call(
        &t,
        write(
            "POST",
            "/api/value/edit",
            &format!(r#"{{"view":{view},"rev":0,"index":1,"value":"new"}}"#),
        ),
    );
    answer_next(&t, wrote(crate::live::write::Reply::Int(0))).await;
    // The reread is on its way.
    let hub = Arc::clone(&t.hub);
    until("the reread waits", || hub.pending_len() == 1).await;
    let writing = |t: &T| {
        t.hub
            .with(|c| c.browse.values.get(view).map(|v| v.writing))
            .flatten()
    };
    assert_eq!(writing(&t), Some(true), "still writing during the reread");
    let res = call(
        &t,
        write(
            "POST",
            "/api/value/remove",
            &format!(r#"{{"view":{view},"rev":0,"index":0}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(&text(res).await)["error"], super::api::edits::WRITING);
    answer_next(&t, hash_page(&[("name", "v"), ("mail", "new")])).await;
    answer_next(&t, keys_page(&["user:1"], 0)).await;
    let v = json(&text(edit.await.unwrap()).await);
    assert_eq!(v["value"]["rev"], 1, "{v}");
    assert_eq!(v["value"]["writing"], false, "{v}");
    assert_eq!(writing(&t), Some(false));
}

/// Small 6: a write that answers while a page of the value is on its way (another
/// tab's `GET /api/value/more`) waits for that page, then reads the value again: the
/// reply's body is the reread one and its `rev` moved.
#[tokio::test]
async fn a_write_answered_while_a_page_loads_still_reads_the_value_again() {
    let p = writable_cache();
    let t = setup("p3s6", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    let view = open_hash(&t);
    t.hub.hold_requests(true);
    let edit = spawn_call(
        &t,
        write(
            "POST",
            "/api/value/edit",
            &format!(r#"{{"view":{view},"rev":0,"index":1,"value":"new"}}"#),
        ),
    );
    let hub = Arc::clone(&t.hub);
    until("the write waits", || hub.pending_len() == 1).await;
    // Another tab's page read starts while the write is on its way.
    let set_loading = |on: bool| {
        t.hub.with_mut(|c| {
            c.browse.values.get_mut(view).unwrap().set_loading(on);
        });
    };
    set_loading(true);
    assert!(
        t.hub
            .answer_oldest(wrote(crate::live::write::Reply::Int(0)))
    );
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(t.hub.pending_len(), 0, "the reread waits for the page");
    // The other tab's page lands.
    set_loading(false);
    answer_next(&t, hash_page(&[("name", "v"), ("mail", "new")])).await;
    answer_next(&t, keys_page(&["user:1"], 0)).await;
    let v = json(&text(edit.await.unwrap()).await);
    assert_eq!(v["value"]["rev"], 1, "read again: {v}");
    assert_eq!(v["value"]["items"][1]["value"], "new", "{v}");
}

// -- phase 4: Streams, Search, Hot keys from Monitor ---------------------------------

/// Every phase 4 route and method, as the router serves them.
const PHASE_FOUR: &[(&str, &str)] = &[
    ("GET", "/api/streams"),
    ("GET", "/api/streams/pending?key=61&group=67"),
    ("POST", "/api/streams/ack"),
    ("POST", "/api/streams/claim"),
    ("POST", "/api/streams/trim"),
    ("GET", "/api/search"),
    ("POST", "/api/search/query"),
    ("GET", "/api/search/results?run=1"),
    ("GET", "/api/search/field?run=1&doc=0&field=0"),
    ("POST", "/api/search/create"),
    ("POST", "/api/search/drop"),
    ("GET", "/api/analysis/hot_keys/monitor"),
];

/// S3: every phase 4 route needs the token; every write route the guard's Origin,
/// header and JSON body.
#[tokio::test]
async fn phase_four_routes_need_the_token_and_writes_the_guard() {
    let t = setup("p4guard", Vec::new());
    let mut p = profile("cache", Env::Local);
    p.read_only = Some(false);
    t.hub.install_for_test(p, false);
    guarded(&t, PHASE_FOUR).await;
}

/// The fixture report with the streams `events:orders` and `events:audit`.
fn stream_report() -> crate::model::ScanReport {
    use crate::model::{KeySample, KeyType, Ttl};
    let mut r = crate::tui::fixtures::report(true);
    let mut p = r.patterns[0].clone();
    p.pattern = "events:{name}".into();
    p.key_type = Some(KeyType::Stream);
    p.examples = ["events:orders", "events:audit"]
        .iter()
        .map(|k| KeySample {
            key: (*k).into(),
            key_type: KeyType::Stream,
            bytes: Some(1024),
            ttl: Ttl::Persistent,
            lfu_freq: None,
            node: None,
        })
        .collect();
    r.patterns.push(p);
    r
}

fn a_stream(key: &str) -> crate::live::streams::StreamInfo {
    use crate::live::streams::{ConsumerInfo, GroupInfo, Lag, StreamInfo};
    StreamInfo {
        key: key.into(),
        length: 3,
        bytes: Some(900),
        first_id: Some("1700000000000-0".into()),
        last_id: "1700000000002-0".into(),
        oldest_age_ms: Some(5_000),
        entries_added: Some(3),
        groups: Some(vec![GroupInfo {
            name: "billing".into(),
            consumers: 1,
            pending: 2,
            last_delivered_id: "1700000000001-0".into(),
            lag: Lag::Known(1),
            readers: Some(vec![ConsumerInfo {
                name: "worker-1".into(),
                pending: 2,
                idle_ms: 1_000,
                inactive_ms: Some(1_000),
            }]),
        }]),
        notes: Vec::new(),
        pending: None,
    }
}

fn streams_reply(keys: &[&str]) -> session::LiveResult {
    use crate::live::streams::{Fetched, Refused, Reply};
    Ok(crate::live::LiveReply::Streams(Reply::Info {
        streams: keys
            .iter()
            .map(|k| Fetched {
                key: (*k).into(),
                info: (*k != "events:audit").then(|| a_stream(k)),
            })
            .collect(),
        refused: Refused::default(),
    }))
}

/// A session with a report of two streams, the live connection ready, on `p`.
fn streams_session(t: &T, p: Profile) {
    t.hub.install_for_test(p, false);
    t.hub.with_mut(|c| {
        let r = stream_report();
        c.session.server = Some(r.server.clone());
        c.session.report = Some(r);
    });
}

/// Before an analysis the panel says so in the TUI's words; then the streams of the
/// report are read in one request (standalone), a gone key said as gone, and a list
/// read a moment ago is answered without asking again.
#[tokio::test]
async fn streams_come_from_the_analysis_and_are_read_once_a_poll() {
    let t = setup("p4streams", Vec::new());
    t.hub.install_for_test(profile("cache", Env::Local), false);
    let (status, v) = get_json(&t, "/api/streams").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "no_report");
    assert_eq!(v["text"], super::api::streams::NO_ANALYSIS);

    let t = setup("p4streams2", Vec::new());
    streams_session(&t, profile("cache", Env::Local));
    t.hub.hold_requests(true);
    let list = spawn_call(&t, signed_get("/api/streams"));
    answer_next(&t, streams_reply(&["events:orders", "events:audit"])).await;
    let v = json(&text(list.await.unwrap()).await);
    assert_eq!(v["status"], "ready", "{v}");
    assert_eq!(v["rows"][0]["key"]["name"], "events:orders");
    assert_eq!(
        v["rows"][0]["groups"][0]["readers"][0]["name"]["name"],
        "worker-1"
    );
    assert_eq!(v["rows"][1]["gone"], true);
    assert_eq!(v["writes"], serde_json::Value::Null);
    let sent = t.hub.sent_count();
    let (_, again) = get_json(&t, "/api/streams").await;
    assert_eq!(again["rows"], v["rows"]);
    assert_eq!(t.hub.sent_count(), sent, "fresh: nothing sent");
}

/// Rule 3: a refused XINFO (the permission check) turns the panel off in the TUI's
/// words, and so does a refused XINFO STREAM, until a refresh asks again.
#[tokio::test]
async fn a_refused_xinfo_turns_streams_off_in_fixed_words() {
    use crate::model::{PermCheck, PermStatus, PermissionReport};
    let t = setup("p4xinfo", Vec::new());
    streams_session(&t, profile("cache", Env::Local));
    t.hub.with_mut(|c| {
        c.perms = Some(PermissionReport {
            checks: vec![PermCheck {
                label: crate::conn::PERM_XINFO,
                status: PermStatus::Denied("NOPERM user alice has no permissions".into()),
            }],
        })
    });
    let (_, v) = get_json(&t, "/api/streams").await;
    assert_eq!(
        v["off"],
        "The server refused XINFO for this user, so this view is off."
    );
    assert!(!v.to_string().contains("alice"));
    t.hub.with_mut(|c| c.perms = None);
    t.hub.hold_requests(true);
    let list = spawn_call(&t, signed_get("/api/streams"));
    answer_next(
        &t,
        Err(crate::live::LiveError::Denied("XINFO STREAM".into())),
    )
    .await;
    let v = json(&text(list.await.unwrap()).await);
    assert!(v["off"].as_str().unwrap().contains("XINFO STREAM"), "{v}");
    let sent = t.hub.sent_count();
    let (_, v) = get_json(&t, "/api/streams").await;
    assert!(v["off"].is_string());
    assert_eq!(t.hub.sent_count(), sent, "not asked again until a refresh");
}

/// Rule 2: on a read-only profile every stream write is refused with the gate's words
/// and nothing is sent; listing the pending entries still reads.
#[tokio::test]
async fn stream_writes_on_a_read_only_profile_send_nothing() {
    let t = setup("p4ro", Vec::new());
    streams_session(&t, profile("prod1", Env::Prod));
    t.hub.hold_requests(true);
    let list = spawn_call(&t, signed_get("/api/streams"));
    answer_next(&t, streams_reply(&["events:orders", "events:audit"])).await;
    let v = json(&text(list.await.unwrap()).await);
    assert!(v["writes"].is_string(), "{v}");
    let key = hexed("events:orders");
    let group = hexed("billing");
    let sent = t.hub.sent_count();
    for (path, body) in [
        (
            "/api/streams/ack",
            format!(r#"{{"key":"{key}","group":"{group}","id":"1700000000000-0"}}"#),
        ),
        (
            "/api/streams/claim",
            format!(
                r#"{{"key":"{key}","group":"{group}","to":"w2","min_idle_ms":"0","count":"10"}}"#
            ),
        ),
        (
            "/api/streams/trim",
            format!(r#"{{"key":"{key}","by":"MAXLEN","value":"1"}}"#),
        ),
    ] {
        let res = call(&t, write("POST", path, &body)).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        let v = json(&text(res).await);
        assert!(
            v["error"].as_str().unwrap().contains("read-only"),
            "{path}: {v}"
        );
    }
    assert_eq!(t.hub.sent_count(), sent, "no write sent");
    // XPENDING is a read.
    let pending = spawn_call(
        &t,
        signed_get(&format!("/api/streams/pending?key={key}&group={group}")),
    );
    let mut info = a_stream("events:orders");
    info.pending = Some(crate::live::streams::Pending {
        group: "billing".into(),
        consumer: None,
        entries: vec![crate::live::streams::PendingEntry {
            id: "1700000000000-0".into(),
            consumer: "worker-1".into(),
            idle_ms: 1_000,
            deliveries: 1,
        }],
        error: None,
    });
    answer_next(
        &t,
        Ok(crate::live::LiveReply::Streams(
            crate::live::streams::Reply::Info {
                streams: vec![crate::live::streams::Fetched {
                    key: "events:orders".into(),
                    info: Some(info),
                }],
                refused: Default::default(),
            },
        )),
    )
    .await;
    let v = json(&text(pending.await.unwrap()).await);
    assert_eq!(v["pending"]["entries"][0]["id"], "1700000000000-0", "{v}");
}

/// Rule 2: on a writable profile a stream write goes through the gate: the ack of an
/// entry not in the pending list shown is refused before anything is asked; a trim is
/// destructive, so the gate asks first and sends only once answered.
#[tokio::test]
async fn stream_writes_are_checked_then_asked_then_sent() {
    let p = writable_cache();
    let t = setup("p4write", vec![p.clone()]);
    streams_session(&t, p);
    t.hub.hold_requests(true);
    let list = spawn_call(&t, signed_get("/api/streams"));
    answer_next(&t, streams_reply(&["events:orders", "events:audit"])).await;
    list.await.unwrap();
    let key = hexed("events:orders");
    let group = hexed("billing");
    let res = call(
        &t,
        write(
            "POST",
            "/api/streams/ack",
            &format!(r#"{{"key":"{key}","group":"{group}","id":"1700000000000-0"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json(&text(res).await)["error"],
        super::api::streams::NOT_PENDING_LISTED
    );
    // A gone stream can't be trimmed; a bad MINID is the TUI's words.
    let audit = hexed("events:audit");
    let res = call(
        &t,
        write(
            "POST",
            "/api/streams/trim",
            &format!(r#"{{"key":"{audit}","by":"MAXLEN","value":"1"}}"#),
        ),
    )
    .await;
    assert_eq!(json(&text(res).await)["error"], super::api::streams::GONE);
    let res = call(
        &t,
        write(
            "POST",
            "/api/streams/trim",
            &format!(r#"{{"key":"{key}","by":"MINID","value":"x"}}"#),
        ),
    )
    .await;
    assert_eq!(
        json(&text(res).await)["error"],
        "MINID takes an entry ID, such as 1700000000000-0."
    );
    let sent = t.hub.sent_count();
    let res = call(
        &t,
        write(
            "POST",
            "/api/streams/trim",
            &format!(r#"{{"key":"{key}","by":"MAXLEN","value":"1"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let q = json(&text(res).await);
    assert!(
        q["confirm"]["text"]
            .as_str()
            .unwrap()
            .contains("XTRIM events:orders MAXLEN ~ 1"),
        "{q}"
    );
    assert_eq!(t.hub.sent_count(), sent, "asked, not sent");
    let id = q["confirm"]["id"].as_str().unwrap().to_string();
    let answered = {
        let t2 = t.router.clone();
        let req = write("POST", &format!("/api/confirm/{id}"), "{}");
        tokio::spawn(async move { t2.oneshot(req).await.unwrap() })
    };
    answer_next(&t, wrote(crate::live::write::Reply::Int(2))).await;
    let v = json(&text(answered.await.unwrap()).await);
    assert_eq!(v["reply"]["kind"], "int", "{v}");
    assert_eq!(v["reply"]["value"], 2);
}

/// Rule 3: without the module, Search is off in the TUI's words and its writes are
/// refused; a refused FT._LIST is off without the user's name; the panel is not asked
/// again until a refresh.
#[tokio::test]
async fn search_without_the_module_or_refused_is_off() {
    let p = writable_cache();
    let t = setup("p4search", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    t.hub.hold_requests(true);
    let list = spawn_call(&t, signed_get("/api/search"));
    answer_next(
        &t,
        Err(crate::live::LiveError::Unsupported("FT._LIST".into())),
    )
    .await;
    let v = json(&text(list.await.unwrap()).await);
    assert_eq!(v["off"], super::api::search::NOT_LOADED, "{v}");
    assert_eq!(v["no_module"], true);
    assert_eq!(v["detail"], super::api::search::NOT_LOADED_WHY);
    let sent = t.hub.sent_count();
    let (_, again) = get_json(&t, "/api/search").await;
    assert_eq!(again, v);
    let res = call(
        &t,
        write(
            "POST",
            "/api/search/create",
            r#"{"name":"i","on":"HASH","prefixes":"","fields":"t TEXT"}"#,
        ),
    )
    .await;
    assert_eq!(
        json(&text(res).await)["error"],
        crate::tui::app::views::search::NO_MODULE
    );
    assert_eq!(t.hub.sent_count(), sent, "nothing sent");
    // Refreshed: asked again, and refused this time.
    let list = spawn_call(&t, signed_get("/api/search?refresh=1"));
    answer_next(&t, Err(crate::live::LiveError::Denied("FT._LIST".into()))).await;
    let v = json(&text(list.await.unwrap()).await);
    assert_eq!(v["no_module"], false);
    assert!(v["off"].as_str().unwrap().contains("FT._LIST"), "{v}");
}

fn index_list() -> session::LiveResult {
    use crate::live::search::{IndexInfo, IndexList, Reply};
    Ok(crate::live::LiveReply::Search(Reply::Indexes(IndexList {
        indexes: vec![IndexInfo {
            name: "idx:p".into(),
            on: Some("HASH".into()),
            prefixes: vec!["p:".into()],
            docs: Some(2),
            bytes: Some(100),
            indexing: Some(false),
            progress: Some(1.0),
            failures: Some(0),
            schema: Vec::new(),
        }],
        total: 1,
        notes: Vec::new(),
    })))
}

/// One query at a time; explain uses the query last run; a command the server lacks is
/// off until a refresh and not sent again; the results come a page at a time.
#[tokio::test]
async fn search_queries_run_one_at_a_time_and_remember_what_is_off() {
    use crate::live::search::{Doc, Reply, Results};
    let p = writable_cache();
    let t = setup("p4query", vec![p.clone()]);
    t.hub.install_for_test(p, false);
    t.hub.hold_requests(true);
    let list = spawn_call(&t, signed_get("/api/search"));
    answer_next(&t, index_list()).await;
    let v = json(&text(list.await.unwrap()).await);
    assert_eq!(v["indexes"][0]["name"]["name"], "idx:p", "{v}");
    let idx = hexed("idx:p");
    // Explain before any query.
    let res = call(
        &t,
        write(
            "POST",
            "/api/search/query",
            &format!(r#"{{"index":"{idx}","cmd":"explain"}}"#),
        ),
    )
    .await;
    assert_eq!(
        json(&text(res).await)["error"],
        "Run a query first: FT.EXPLAIN uses the query last run."
    );
    let first = spawn_call(
        &t,
        write(
            "POST",
            "/api/search/query",
            &format!(r#"{{"index":"{idx}","cmd":"search","query":""}}"#),
        ),
    );
    let hub = Arc::clone(&t.hub);
    until("the query waits", || hub.pending_len() == 1).await;
    let res = call(
        &t,
        write(
            "POST",
            "/api/search/query",
            &format!(r#"{{"index":"{idx}","cmd":"search","query":"x"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        json(&text(res).await)["error"],
        "FT.SEARCH is still running: wait for its answer."
    );
    let docs = (0..12)
        .map(|i| Doc {
            key: format!("p:{i}"),
            fields: vec![(
                crate::value::Raw::from("title"),
                crate::value::Raw::from("red shoe"),
            )],
        })
        .collect();
    assert!(
        t.hub
            .answer_oldest(Ok(crate::live::LiveReply::Search(Reply::Results(
                Results {
                    total: 12,
                    docs,
                    took: std::time::Duration::from_millis(2),
                }
            ))))
    );
    let v = json(&text(first.await.unwrap()).await);
    assert_eq!(v["results"]["query"], "*", "an empty query is *: {v}");
    assert_eq!(v["results"]["docs"].as_array().unwrap().len(), 10);
    let run = v["results"]["run"].as_u64().unwrap();
    let (_, page) = get_json(&t, &format!("/api/search/results?run={run}&from=10")).await;
    assert_eq!(page["docs"].as_array().unwrap().len(), 2);
    let (_, field) = get_json(&t, &format!("/api/search/field?run={run}&doc=11&field=0")).await;
    assert_eq!(field["shown"]["lines"][0], "red shoe", "{field}");
    // Explain: the module lacks it; then it is not sent again.
    let explain = spawn_call(
        &t,
        write(
            "POST",
            "/api/search/query",
            &format!(r#"{{"index":"{idx}","cmd":"explain"}}"#),
        ),
    );
    answer_next(
        &t,
        Err(crate::live::LiveError::Unsupported("FT.EXPLAIN".into())),
    )
    .await;
    let v = json(&text(explain.await.unwrap()).await);
    assert!(v["off"].as_str().unwrap().contains("FT.EXPLAIN"), "{v}");
    let sent = t.hub.sent_count();
    let res = call(
        &t,
        write(
            "POST",
            "/api/search/query",
            &format!(r#"{{"index":"{idx}","cmd":"explain"}}"#),
        ),
    )
    .await;
    assert!(json(&text(res).await)["off"].is_string());
    assert_eq!(t.hub.sent_count(), sent);
    // A query the server can't run keeps its words about the query.
    let bad = spawn_call(
        &t,
        write(
            "POST",
            "/api/search/query",
            &format!(r#"{{"index":"{idx}","cmd":"search","query":"(("}}"#),
        ),
    );
    answer_next(
        &t,
        Err(crate::live::LiveError::Failed(
            "FT.SEARCH failed: Syntax error at offset 1 near ((".into(),
        )),
    )
    .await;
    let res = bad.await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json(&text(res).await)["error"],
        "FT.SEARCH failed: Syntax error at offset 1 near ((."
    );
    // The drop of a listed index asks first (destructive).
    let res = call(
        &t,
        write(
            "POST",
            "/api/search/drop",
            &format!(r#"{{"index":"{idx}"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let q = json(&text(res).await);
    assert!(
        q["confirm"]["text"]
            .as_str()
            .unwrap()
            .contains("FT.DROPINDEX idx:p"),
        "{q}"
    );
}

/// Hot keys from Monitor: a run's lines are kept while it runs, and once it is
/// stopped its commands' key positions are asked with COMMAND INFO through the budget,
/// then its keys ranked as the TUI ranks them, while the analysis has no hot keys.
#[tokio::test]
async fn a_stopped_monitor_run_ranks_its_keys_for_hot_keys() {
    use crate::live::server::{Begin, CommandKeys, Find, KeySpec, Reply};
    use crate::tui::app::Msg;
    let t = setup("p4hot", Vec::new());
    let conn = t.hub.install_for_test(profile("cache", Env::Local), false);
    t.hub.with_mut(|c| {
        let r = crate::tui::fixtures::report(true);
        c.session.server = Some(r.server.clone());
        c.session.report = Some(r);
    });
    let (_, v) = get_json(&t, "/api/analysis/hot_keys/monitor").await;
    assert_eq!(v["status"], "none");
    assert_eq!(v["text"], super::api::hot::NO_RUN);
    // A run, as `Hub::monitor` starts one.
    t.hub.with_mut(|c| {
        c.monitor = Some(session::Monitor {
            run: 7,
            node: 0,
            started_at_unix_ms: None,
        });
        c.hot.begin(7, 0);
    });
    t.hub.apply_for_test(Msg::MonitorReady {
        id: conn,
        scan: 1,
        run: 7,
        at_unix_ms: 1_700_000_000_000,
    });
    let line = |s: &str| crate::live::monitor::read_line(s).unwrap();
    t.hub.apply_for_test(Msg::MonitorLines {
        id: conn,
        scan: 1,
        run: 7,
        lines: vec![
            line(r#"1.0 [0 127.0.0.1:1] "GET" "user:1""#),
            line(r#"1.0 [0 127.0.0.1:1] "GET" "user:1""#),
            line(r#"1.0 [0 127.0.0.1:1] "GET" "user:2""#),
        ],
    });
    t.hub.hold_requests(true);
    assert!(t.hub.stop_monitor());
    let (_, v) = get_json(&t, "/api/analysis/hot_keys/monitor").await;
    assert_eq!(v["status"], "pending", "{v}");
    let mut learned = std::collections::HashMap::new();
    learned.insert(
        "get".to_string(),
        CommandKeys::Specs(vec![KeySpec {
            begin: Begin::Index(1),
            find: Find::Range {
                last: 0,
                step: 1,
                limit: 0,
            },
        }]),
    );
    answer_next(
        &t,
        Ok(crate::live::LiveReply::Server(Reply::CommandKeys(learned))),
    )
    .await;
    let hub = Arc::clone(&t.hub);
    until("ranked", || {
        hub.with(|c| matches!(c.hot.rank, super::api::hot::Rank::Done(_)))
            .unwrap_or(false)
    })
    .await;
    let (_, v) = get_json(&t, "/api/analysis/hot_keys/monitor").await;
    assert_eq!(v["status"], "ready", "{v}");
    assert_eq!(v["ranking"]["rows"][0]["key"]["name"], "user:1");
    assert_eq!(v["ranking"]["rows"][0]["count"], 2);
    assert_eq!(v["ranking"]["lines"], 3);
    assert_eq!(v["ranking"]["unknown"], 0);
    assert!(
        v["ranking"]["title"]
            .as_str()
            .unwrap()
            .ends_with("3 lines, at 22:13 UTC"),
        "{v}"
    );
}

// -- phase 4: the verifier's findings (S1-S4, Small 1, 2, 4, 8) ------------------------

fn primary(i: u8, slots: (u16, u16)) -> crate::live::nodes::NodeInfo {
    crate::live::nodes::NodeInfo {
        id: format!("n{i}"),
        endpoint: Endpoint {
            host: format!("10.0.0.{i}"),
            port: 7000,
            tls: false,
        },
        role: crate::model::NodeRole::Primary,
        slots: vec![slots],
        connected: true,
    }
}

/// A session on `p` on a cluster of two primaries, the live connection ready.
fn cluster_session(t: &T, p: Profile) {
    t.hub.install_for_test(p, false);
    t.hub.with_mut(|c| {
        c.session.nodes = vec![primary(1, (0, 8191)), primary(2, (8192, 16383))];
    });
}

/// The picked node's Search list read, answered with [`index_list`].
async fn search_listed(t: &T) -> serde_json::Value {
    let list = spawn_call(t, signed_get("/api/search"));
    answer_next(t, index_list()).await;
    let res = list.await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    json(&text(res).await)
}

/// The streams of the report read, answered with [`streams_reply`].
async fn streams_listed(t: &T) {
    let list = spawn_call(t, signed_get("/api/streams"));
    answer_next(t, streams_reply(&["events:orders", "events:audit"])).await;
    let res = list.await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// A write the gate asks about: the question's id.
async fn asked(t: &T, path: &str, body: &str) -> String {
    let res = call(t, write("POST", path, body)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT, "{path}");
    let v = json(&text(res).await);
    v["confirm"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{path}: {v}"))
        .to_string()
}

/// An answer that must not wait for a send: a write sent while requests are held
/// would wait for ever.
async fn answer_now(t: &T, id: &str, typed: Option<&str>) -> Response {
    tokio::time::timeout(std::time::Duration::from_secs(5), answer(t, id, typed))
        .await
        .expect("the answer was refused at once, not sent")
}

fn no_results() -> session::LiveResult {
    use crate::live::search::{Reply, Results};
    Ok(crate::live::LiveReply::Search(Reply::Results(Results {
        total: 0,
        docs: Vec::new(),
        took: std::time::Duration::from_millis(1),
    })))
}

fn query_body(cmd: &str, query: &str) -> String {
    let idx = hexed("idx:p");
    format!(r#"{{"index":"{idx}","cmd":"{cmd}","query":"{query}"}}"#)
}

/// S1 (rule 2): a node pick is part of a Search write's run. A drop asked on node 1 and
/// answered after node 2 was picked is refused with CHANGED and sent to neither; a write
/// made from node 1's list and decided after another pick (the window of the profile's
/// read) is refused the same way, and so is its grant.
#[tokio::test]
async fn a_search_write_is_for_the_node_it_was_made_on() {
    use super::api::writes::{CHANGED, MadeFor, decide, grant_write};
    let p = writable_cache();
    let t = setup("p4s1", vec![p.clone()]);
    cluster_session(&t, p);
    t.hub.hold_requests(true);
    search_listed(&t).await;
    let idx = hexed("idx:p");
    let id = asked(&t, "/api/search/drop", &format!(r#"{{"index":"{idx}"}}"#)).await;
    let res = call(&t, write("POST", "/api/node", r#"{"index":1}"#)).await;
    assert_eq!(res.status(), StatusCode::OK);
    // A send would be answered as closed (no server here), not held.
    t.hub.hold_requests(false);
    let sent = t.hub.sent_count();
    let res = answer_now(&t, &id, None).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(&text(res).await)["error"], CHANGED);
    assert_eq!(t.hub.sent_count(), sent, "sent to neither node");

    let (_tx, rx) = tokio::sync::watch::channel(false);
    let app = app_state(Arc::clone(&t.hub), PORT, Token::for_test(&token()), rx);
    let op = crate::write::WriteOp::FtDropIndex {
        index: "idx:p".into(),
    };
    let made = t.hub.with(MadeFor::on_picked).unwrap();
    assert_eq!(made.node, Some(1));
    let d = decide(&app, &op, Some(made)).unwrap();
    assert_eq!(d.dest, crate::live::Dest::node(1));
    assert!(t.hub.pick(0));
    let r = decide(&app, &op, Some(made)).unwrap_err();
    assert_eq!((r.status, r.text.as_str()), (StatusCode::CONFLICT, CHANGED));
    let r = grant_write(&app, d.made, op.clone(), &crate::write::Answer::Yes).unwrap_err();
    assert_eq!(r.text, CHANGED);
    // Decided without one, a write sent to the picked node is still for that node only.
    let d = decide(&app, &op, None).unwrap();
    assert_eq!(d.made.node, Some(0));
    assert!(t.hub.pick(1));
    let r = grant_write(&app, d.made, op, &crate::write::Answer::Yes).unwrap_err();
    assert_eq!(r.text, CHANGED);
    // A key's write goes to its owner whatever is picked: not bound to the node.
    let op = crate::write::WriteOp::Unlink {
        key: "user:1".into(),
    };
    let d = decide(&app, &op, Some(t.hub.with(MadeFor::of).unwrap())).unwrap();
    assert_eq!(d.made.node, None);
    assert!(t.hub.pick(0));
    assert!(grant_write(&app, d.made, op, &crate::write::Answer::Yes).is_ok());
    assert_eq!(t.hub.sent_count(), sent);
}

/// S2 (the TUI's bulk dialog takes every key until it ends): a stream write, a Search
/// write and a key edit asked before a bulk action started are refused when answered
/// while it counts, and while it runs, with BUSY, and nothing is sent.
#[tokio::test]
async fn a_write_asked_before_a_bulk_action_is_refused_while_it_counts_or_runs() {
    use super::api::bulk::{BUSY, hold};
    let p = writable_cache();
    let t = setup("p4s2", vec![p.clone()]);
    streams_session(&t, p);
    let view = open_hash(&t);
    t.hub.hold_requests(true);
    streams_listed(&t).await;
    search_listed(&t).await;
    let key = hexed("events:orders");
    let idx = hexed("idx:p");
    let mut questions = Vec::new();
    for _ in 0..2 {
        questions.push(vec![
            asked(
                &t,
                "/api/streams/trim",
                &format!(r#"{{"key":"{key}","by":"MAXLEN","value":"1"}}"#),
            )
            .await,
            asked(&t, "/api/search/drop", &format!(r#"{{"index":"{idx}"}}"#)).await,
            asked(&t, "/api/value/delete", &format!(r#"{{"view":{view}}}"#)).await,
        ]);
    }
    let pattern = hexed("user:{id}");
    let res = call(
        &t,
        write(
            "POST",
            "/api/bulk",
            &format!(r#"{{"pattern":"{pattern}","action":"delete"}}"#),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let bulk = json(&text(res).await)["bulk"]["id"].as_u64().unwrap();
    let hub = Arc::clone(&t.hub);
    until("the count step waits", || hub.pending_len() == 1).await;
    let refused = |what: &'static str, ids: Vec<String>| {
        let t = &t;
        async move {
            let sent = t.hub.sent_count();
            for id in ids {
                let res = answer_now(t, &id, None).await;
                assert_eq!(res.status(), StatusCode::CONFLICT, "{what}");
                assert_eq!(json(&text(res).await)["error"], BUSY, "{what}");
            }
            assert_eq!(t.hub.sent_count(), sent, "{what}: nothing sent");
        }
    };
    let running = questions.pop().unwrap();
    refused("counting", questions.pop().unwrap()).await;

    assert!(
        t.hub
            .answer_oldest(count_reply(crate::live::bulk::Walk::Done, 5))
    );
    until("counted", || {
        hub.with(|c| {
            c.bulk
                .as_ref()
                .is_some_and(|b| matches!(b.stage, super::api::bulk::Stage::Counted))
        })
        .unwrap_or(false)
    })
    .await;
    let qid = asked(&t, "/api/bulk/run", &format!(r#"{{"bulk":{bulk}}}"#)).await;
    hold::hold(&t.config);
    let res = answer(&t, &qid, None).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    until("held", || hold::arrived(&t.config) == 1).await;
    refused("running", running).await;
}

/// S3: a reply that came back just before a new run is not kept after it: the stream
/// list, a pending list, the Search list and a query's results each answer CHANGED and
/// leave the panel as the new run reset it.
#[tokio::test]
async fn a_reply_that_comes_back_on_a_new_run_is_not_kept() {
    use super::api::writes::CHANGED;
    let p = writable_cache();
    let t = setup("p4s3", vec![p.clone()]);
    streams_session(&t, p);
    t.hub.hold_requests(true);
    let hub = Arc::clone(&t.hub);
    let changed = |res: Response| async move {
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(json(&text(res).await)["error"], CHANGED);
    };
    let report = || {
        t.hub.with_mut(|c| c.session.report = Some(stream_report()));
    };

    // The list: answered, then the new run, then the handler takes the lock.
    let list = spawn_call(&t, signed_get("/api/streams"));
    until("the read waits", || hub.pending_len() == 1).await;
    assert!(t.hub.answer_oldest(streams_reply(&["events:orders"])));
    t.hub.new_run(None, Some(1)).unwrap();
    changed(list.await.unwrap()).await;
    assert_eq!(t.hub.with(|c| c.streams.is_empty()), Some(true));
    new_run_ready(&t).await;

    // A pending list.
    report();
    streams_listed(&t).await;
    let (key, group) = (hexed("events:orders"), hexed("billing"));
    let pending = spawn_call(
        &t,
        signed_get(&format!("/api/streams/pending?key={key}&group={group}")),
    );
    until("the read waits", || hub.pending_len() == 1).await;
    assert!(t.hub.answer_oldest(streams_reply(&["events:orders"])));
    t.hub.new_run(None, Some(0)).unwrap();
    changed(pending.await.unwrap()).await;
    assert_eq!(t.hub.with(|c| c.streams.is_empty()), Some(true));
    new_run_ready(&t).await;

    // The Search list.
    let list = spawn_call(&t, signed_get("/api/search"));
    until("the read waits", || hub.pending_len() == 1).await;
    assert!(t.hub.answer_oldest(index_list()));
    t.hub.new_run(None, Some(1)).unwrap();
    changed(list.await.unwrap()).await;
    assert_eq!(t.hub.with(|c| c.search.is_empty()), Some(true));
    new_run_ready(&t).await;

    // A query's results.
    search_listed(&t).await;
    let query = spawn_call(
        &t,
        write("POST", "/api/search/query", &query_body("search", "x")),
    );
    until("the query waits", || hub.pending_len() == 1).await;
    assert!(t.hub.answer_oldest(no_results()));
    t.hub.new_run(None, Some(0)).unwrap();
    changed(query.await.unwrap()).await;
    assert_eq!(t.hub.with(|c| c.search.is_empty()), Some(true));
}

/// S4: picking another node drops what Search found (its list, last query and
/// results); a new run drops what Streams and Search found.
#[tokio::test]
async fn a_pick_drops_what_search_found_and_a_new_run_what_both_panels_found() {
    let p = writable_cache();
    let t = setup("p4s4pick", vec![p.clone()]);
    cluster_session(&t, p);
    t.hub.with_mut(|c| c.session.report = Some(stream_report()));
    t.hub.hold_requests(true);
    search_listed(&t).await;
    let query = spawn_call(
        &t,
        write("POST", "/api/search/query", &query_body("search", "x")),
    );
    answer_next(&t, no_results()).await;
    assert_eq!(query.await.unwrap().status(), StatusCode::OK);
    assert_eq!(t.hub.with(|c| c.search.is_empty()), Some(false));
    assert!(t.hub.pick(1));
    assert_eq!(t.hub.with(|c| c.search.is_empty()), Some(true));
    let (_, v) = get_json(&t, "/api/search/results?run=1").await;
    assert_eq!(v["error"], super::api::search::STALE, "{v}");

    // Both panels hold what they read; an analysis starts a new run.
    let streams = spawn_call(&t, signed_get("/api/streams"));
    let hub = Arc::clone(&t.hub);
    until("the streams read waits", || hub.pending_len() >= 1).await;
    while t.hub.pending_len() > 0 {
        assert!(
            t.hub
                .answer_oldest(streams_reply(&["events:orders", "events:audit"]))
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(streams.await.unwrap().status(), StatusCode::OK);
    search_listed(&t).await;
    assert_eq!(t.hub.with(|c| c.streams.is_empty()), Some(false));
    assert_eq!(t.hub.with(|c| c.search.is_empty()), Some(false));
    t.hub
        .new_run(Some(crate::tui::app::views::overview::Depth::Quick), None)
        .unwrap();
    assert_eq!(t.hub.with(|c| c.streams.is_empty()), Some(true));
    assert_eq!(t.hub.with(|c| c.search.is_empty()), Some(true));
}

/// S4 (rule 2): on a read-only profile FT.CREATE and FT.DROPINDEX are refused with the
/// gate's words and nothing is sent; the list still reads and says why writes are off.
#[tokio::test]
async fn search_writes_on_a_read_only_profile_send_nothing() {
    let t = setup("p4s4ro", Vec::new());
    t.hub.install_for_test(profile("prod1", Env::Prod), false);
    t.hub.hold_requests(true);
    let v = search_listed(&t).await;
    assert!(v["writes"].as_str().unwrap().contains("read-only"), "{v}");
    let sent = t.hub.sent_count();
    let idx = hexed("idx:p");
    for (path, body) in [
        (
            "/api/search/create",
            r#"{"name":"i","on":"HASH","prefixes":"","fields":"t TEXT"}"#.to_string(),
        ),
        ("/api/search/drop", format!(r#"{{"index":"{idx}"}}"#)),
    ] {
        let res = call(&t, write("POST", path, &body)).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        let v = json(&text(res).await);
        assert!(
            v["error"].as_str().unwrap().contains("read-only"),
            "{path}: {v}"
        );
    }
    assert_eq!(t.hub.sent_count(), sent, "no write sent");
}

/// S4 (rule 2): a stream write's and a Search write's question answered after the
/// profile was made read-only in the config file is refused with the gate's words; one
/// answered after a new run is refused with CHANGED. Nothing is sent.
#[tokio::test]
async fn a_stream_or_search_answer_after_read_only_or_a_new_run_sends_nothing() {
    let p = writable_cache();
    let t = setup("p4s4late", vec![p.clone()]);
    streams_session(&t, p);
    t.hub.hold_requests(true);
    streams_listed(&t).await;
    search_listed(&t).await;
    let key = hexed("events:orders");
    let idx = hexed("idx:p");
    let trim = format!(r#"{{"key":"{key}","by":"MAXLEN","value":"1"}}"#);
    let drop = format!(r#"{{"index":"{idx}"}}"#);
    let ids = [
        asked(&t, "/api/streams/trim", &trim).await,
        asked(&t, "/api/search/drop", &drop).await,
    ];
    flip_writes(&t, "cache", false);
    let sent = t.hub.sent_count();
    for id in &ids {
        let res = answer_now(&t, id, None).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let v = json(&text(res).await);
        assert!(v["error"].as_str().unwrap().contains("read-only"), "{v}");
    }
    assert_eq!(t.hub.sent_count(), sent, "read-only: nothing sent");

    flip_writes(&t, "cache", true);
    let ids = [
        asked(&t, "/api/streams/trim", &trim).await,
        asked(&t, "/api/search/drop", &drop).await,
    ];
    t.hub.new_run(None, Some(1)).unwrap();
    new_run_ready(&t).await;
    let sent = t.hub.sent_count();
    for id in &ids {
        let res = answer_now(&t, id, None).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(json(&text(res).await)["error"], super::api::writes::CHANGED);
    }
    assert_eq!(t.hub.sent_count(), sent, "a new run: nothing sent");
}

/// A Monitor run as `Hub::monitor` starts one, ready, with `lines`.
fn monitor_run(t: &T, conn: crate::tui::app::ConnId, run: u64, lines: &[&str]) {
    use crate::tui::app::Msg;
    t.hub.with_mut(|c| {
        c.monitor = Some(session::Monitor {
            run,
            node: 0,
            started_at_unix_ms: None,
        });
        c.hot.begin(run, 0);
    });
    t.hub.apply_for_test(Msg::MonitorReady {
        id: conn,
        scan: 1,
        run,
        at_unix_ms: 1_700_000_000_000,
    });
    t.hub.apply_for_test(Msg::MonitorLines {
        id: conn,
        scan: 1,
        run,
        lines: lines
            .iter()
            .map(|s| crate::live::monitor::read_line(s).unwrap())
            .collect(),
    });
}

/// S4, Hot keys: commands the server never says anything about are asked with
/// COMMAND INFO at most MAX_LOOKUPS times, then the run is ranked with what is known;
/// a run that a new run ends is ranked at once, without asking.
#[tokio::test]
async fn hot_keys_ask_at_most_max_lookups_and_a_new_run_ranks_at_once() {
    use crate::live::server::Reply;
    use crate::tui::app::views::server::MAX_LOOKUPS;
    let t = setup("p4s4hot", Vec::new());
    let conn = t.hub.install_for_test(profile("cache", Env::Local), false);
    t.hub.with_mut(|c| {
        let r = crate::tui::fixtures::report(true);
        c.session.server = Some(r.server.clone());
        c.session.report = Some(r);
    });
    monitor_run(
        &t,
        conn,
        7,
        &[
            r#"1.0 [0 127.0.0.1:1] "FOO" "user:1""#,
            r#"1.0 [0 127.0.0.1:1] "FOO" "user:2""#,
        ],
    );
    t.hub.hold_requests(true);
    let sent = t.hub.sent_count();
    assert!(t.hub.stop_monitor());
    for _ in 0..MAX_LOOKUPS {
        answer_next(
            &t,
            Ok(crate::live::LiveReply::Server(Reply::CommandKeys(
                Default::default(),
            ))),
        )
        .await;
    }
    let hub = Arc::clone(&t.hub);
    until("ranked", || {
        hub.with(|c| matches!(c.hot.rank, super::api::hot::Rank::Done(_)))
            .unwrap_or(false)
    })
    .await;
    assert_eq!(t.hub.sent_count(), sent + MAX_LOOKUPS as usize);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(t.hub.pending_len(), 0, "no lookup past the bound");
    let (_, v) = get_json(&t, "/api/analysis/hot_keys/monitor").await;
    assert_eq!(v["ranking"]["unknown"], 2, "{v}");

    // A new run ends the next one: ranked at once, nothing asked.
    monitor_run(&t, conn, 8, &[r#"1.0 [0 127.0.0.1:1] "BAR" "user:3""#]);
    let sent = t.hub.sent_count();
    t.hub.new_run(None, Some(1)).unwrap();
    let ranked = t.hub.with(|c| match &c.hot.rank {
        super::api::hot::Rank::Done(r) => r.hot.ranking.lines,
        _ => 0,
    });
    assert_eq!(ranked, Some(1), "ranked at once");
    assert_eq!(t.hub.sent_count(), sent, "no COMMAND INFO");
    assert_eq!(t.hub.pending_len(), 0);
}

/// S4: every route of `routes` is served for its methods only: another one (with the
/// token and the guard's headers) is 405, and `Allow` names exactly the methods listed.
async fn only_their_methods(t: &T, routes: &[(&str, &str)]) {
    use std::collections::{BTreeMap, BTreeSet};
    let mut by_path: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (method, path) in routes {
        let path = path.split('?').next().unwrap();
        by_path.entry(path).or_default().insert(method.to_string());
    }
    for (path, methods) in by_path {
        let res = call(t, write("PUT", path, "{}")).await;
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED, "PUT {path}");
        let allow: BTreeSet<String> = res.headers()[header::ALLOW]
            .to_str()
            .unwrap()
            .split(',')
            .map(|m| m.trim().to_string())
            .filter(|m| m != "HEAD")
            .collect();
        assert_eq!(allow, methods, "{path}");
    }
}

#[tokio::test]
async fn every_route_is_served_for_its_registered_methods_only() {
    let t = setup("p4methods", Vec::new());
    let mut p = profile("cache", Env::Local);
    p.read_only = Some(false);
    t.hub.install_for_test(p, false);
    only_their_methods(&t, PHASE_TWO).await;
    only_their_methods(&t, PHASE_THREE).await;
    only_their_methods(&t, PHASE_FOUR).await;
}

/// Small 1, 4, 8: the Search list is read each time the view asks for it; `busy` names
/// the command as the page does; and a query of an earlier node never frees the slot of
/// one asked after a pick, so two never run at once.
#[tokio::test]
async fn the_search_list_is_read_when_shown_and_one_query_runs_at_a_time_across_picks() {
    let p = writable_cache();
    let t = setup("p4small", vec![p.clone()]);
    cluster_session(&t, p);
    t.hub.hold_requests(true);
    let hub = Arc::clone(&t.hub);
    search_listed(&t).await;
    let sent = t.hub.sent_count();
    search_listed(&t).await;
    assert_eq!(t.hub.sent_count(), sent + 1, "read again when shown");

    let a = spawn_call(
        &t,
        write("POST", "/api/search/query", &query_body("search", "a")),
    );
    until("a waits", || hub.pending_len() == 1).await;
    assert!(t.hub.pick(1));
    let list = spawn_call(&t, signed_get("/api/search"));
    until("the list waits", || hub.pending_len() == 2).await;
    assert!(t.hub.answer_newest(index_list()));
    assert_eq!(list.await.unwrap().status(), StatusCode::OK);
    let b = spawn_call(
        &t,
        write("POST", "/api/search/query", &query_body("search", "b")),
    );
    until("b waits", || hub.pending_len() == 2).await;
    let list = spawn_call(&t, signed_get("/api/search"));
    until("the list waits", || hub.pending_len() == 3).await;
    assert!(t.hub.answer_newest(index_list()));
    let v = json(&text(list.await.unwrap()).await);
    assert_eq!(v["busy"], "FT.SEARCH", "{v}");
    // a, of node 1, ends: b still holds the slot.
    assert!(t.hub.answer_oldest(no_results()));
    assert_eq!(a.await.unwrap().status(), StatusCode::CONFLICT);
    let sent = t.hub.sent_count();
    let res = call(
        &t,
        write("POST", "/api/search/query", &query_body("search", "c")),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        json(&text(res).await)["error"],
        "FT.SEARCH is still running: wait for its answer."
    );
    assert_eq!(t.hub.sent_count(), sent, "c is not sent");
    assert!(t.hub.answer_oldest(no_results()));
    assert_eq!(b.await.unwrap().status(), StatusCode::OK);
}

/// Small 2: `now=1` reads the streams at once (as the TUI's poll after a write) and
/// still leaves out what the server refused; `refresh=1` asks for it again.
#[tokio::test]
async fn after_a_write_the_streams_are_read_now_without_asking_again_what_was_refused() {
    use crate::live::streams::{Fetched, Refused, Reply};
    let t = setup("p4now", Vec::new());
    streams_session(&t, profile("cache", Env::Local));
    t.hub.hold_requests(true);
    let list = spawn_call(&t, signed_get("/api/streams"));
    let mut info = a_stream("events:orders");
    info.groups = None;
    answer_next(
        &t,
        Ok(crate::live::LiveReply::Streams(Reply::Info {
            streams: vec![Fetched {
                key: "events:orders".into(),
                info: Some(info),
            }],
            refused: Refused {
                groups: true,
                ..Refused::default()
            },
        })),
    )
    .await;
    assert_eq!(list.await.unwrap().status(), StatusCode::OK);
    let refused = |t: &T| t.hub.with(|c| c.streams.refused().groups).unwrap();
    assert!(refused(&t));
    let sent = t.hub.sent_count();
    let (_, _) = get_json(&t, "/api/streams").await;
    assert_eq!(t.hub.sent_count(), sent, "fresh: answered again");
    let now = spawn_call(&t, signed_get("/api/streams?now=1"));
    answer_next(&t, streams_reply(&["events:orders"])).await;
    assert_eq!(now.await.unwrap().status(), StatusCode::OK);
    assert_eq!(t.hub.sent_count(), sent + 1, "read at once");
    assert!(refused(&t), "XINFO GROUPS still left out");
    let again = spawn_call(&t, signed_get("/api/streams?refresh=1"));
    let hub = Arc::clone(&t.hub);
    until("the read waits", || hub.pending_len() == 1).await;
    assert!(!refused(&t), "a refresh asks again");
    assert!(t.hub.answer_oldest(streams_reply(&["events:orders"])));
    assert_eq!(again.await.unwrap().status(), StatusCode::OK);
}
