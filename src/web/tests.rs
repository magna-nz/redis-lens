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
    let app = app_state(hub, PORT, Token::for_test(&token()), rx);
    T {
        router: router(app),
        store,
        config,
        _shutdown: tx,
    }
}

fn cookie() -> String {
    format!("rl_session={}", token())
}

fn get(path: &str) -> axum::http::request::Builder {
    Request::get(path).header(header::HOST, HOST)
}

fn signed_get(path: &str) -> Request<Body> {
    get(path)
        .header(header::COOKIE, cookie())
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
        .header(header::COOKIE, cookie())
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
            "api without cookie",
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
            "sign in",
            get(&format!("/auth/{}", token()))
                .body(Body::empty())
                .unwrap(),
            StatusCode::SEE_OTHER,
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
                .header(header::COOKIE, cookie())
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
            .header(header::COOKIE, cookie())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_api_needs_the_cookie() {
    let t = setup("cookie", Vec::new());
    let res = call(&t, get("/api/state").body(Body::empty()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(text(res).await, r#"{"error":"not signed in"}"#);
    let wrong = get("/api/state")
        .header(header::COOKIE, format!("rl_session={}", "f".repeat(64)))
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&t, wrong).await.status(), StatusCode::UNAUTHORIZED);
    let res = call(&t, signed_get("/api/state")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = text(res).await;
    assert!(body.contains(r#""status":"idle""#), "{body}");
    // The event stream too.
    let res = call(&t, get("/api/events").body(Body::empty()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_request_that_is_not_a_get_needs_origin_header_and_json() {
    let t = setup("writes", Vec::new());
    let base = || {
        Request::post("/api/disconnect")
            .header(header::HOST, HOST)
            .header(header::COOKIE, cookie())
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
    // With everything but the cookie: 401.
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
async fn sign_in_sets_the_cookie_and_redirects_once_the_token_matches() {
    let t = setup("signin", Vec::new());
    let res = call(
        &t,
        get(&format!("/auth/{}", token()))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], "/");
    assert_eq!(
        res.headers()[header::SET_COOKIE],
        format!("rl_session={}; HttpOnly; SameSite=Strict; Path=/", token()).as_str()
    );
    for wrong in ["nope".to_string(), "f".repeat(64), format!("{}0", token())] {
        let res = call(
            &t,
            get(&format!("/auth/{wrong}")).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{wrong}");
        assert!(res.headers().get(header::SET_COOKIE).is_none());
        let body = text(res).await;
        assert!(!body.contains(&token()), "{body}");
    }
}

#[tokio::test]
async fn the_page_is_index_or_locked_and_assets_have_their_types() {
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
    assert_eq!(text(res).await.into_bytes(), file("locked.html"));
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
        ("MONITOR", "monitor panel"),
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
        let tok = open.rsplit('/').next().unwrap().to_string();
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
