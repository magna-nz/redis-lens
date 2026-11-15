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
