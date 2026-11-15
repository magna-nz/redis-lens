//! The web dashboard's phase 1 over real HTTP against real Redis and Valkey servers:
//! the guard's 401 and 403, signing in, connecting through the TUI's path, the state,
//! the Overview's figures (and the live ones over the event stream), Clients, the Slow
//! log with a credential argument masked, Monitor, the Pub/Sub tail and publishing
//! through the write gate (sent on a writable profile, asked on a writable prod one,
//! refused on a read-only one), the command line (reads, a refused write, a refused
//! script, a `@dangerous` read confirmed, a NOPERM in fixed words), and a cluster.
//!
//! The config file is a temporary one and credentials are in a `MemoryStore`: the real
//! keychain and the user's config are never touched.
#![cfg(feature = "integration-tests")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use redis_lens::config::Config;
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile};
use redis_lens::secrets::{MemoryStore, SecretStore};
use redis_lens::web::{self, Dashboard, Options};
use secrecy::SecretString;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(30);
const PASS: &str = "S3cret-masterauth-pw";
const LIM_PASS: &str = "lim-S3cret-pw";

fn temp_config(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-web-it-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("config.toml")
}

fn profile(name: &str, port: u16, env: Env, read_only: Option<bool>) -> Profile {
    Profile {
        name: name.into(),
        env,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port,
            tls: false,
        },
        db: 0,
        auth: Auth::None,
        read_only,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        force_standalone: false,
        tags: Vec::new(),
    }
}

async fn cmd(conn: &mut MultiplexedConnection, args: &[&str]) -> redis::Value {
    let mut c = redis::cmd(args[0]);
    for a in &args[1..] {
        c.arg(*a);
    }
    c.query_async(conn).await.expect("command")
}

// -- a small HTTP/1.1 client ------------------------------------------------------

struct Res {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Res {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

/// One request on a connection of its own. `host` is the `Host` header; a request that
/// is not a GET carries the origin, the header and a JSON body unless `bare`.
async fn raw(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    auth: Option<&str>,
    body: Option<&str>,
    bare: bool,
) -> Res {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let body = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: {a}\r\n"));
    }
    if method != "GET" && !bare {
        req.push_str(&format!(
            "Origin: http://127.0.0.1:{port}\r\nX-Redis-Lens: 1\r\nContent-Type: application/json\r\n"
        ));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(WAIT, s.read_to_end(&mut buf))
        .await
        .expect("the server answers")
        .unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, rest) = text.split_once("\r\n\r\n").expect("a head");
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_string()
    };
    Res {
        status,
        headers,
        body,
    }
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.push_str(&rest[..n]);
        s = &rest[n..];
        s = s.strip_prefix("\r\n").unwrap_or(s);
    }
    out
}

struct Dash {
    port: u16,
    /// `Bearer <token>`: the `Authorization` header every API call carries.
    auth: String,
    dashboard: Option<Dashboard>,
    store: Arc<MemoryStore>,
    config: PathBuf,
}

impl Dash {
    async fn start(profiles: Vec<Profile>, tag: &str) -> Self {
        let config = temp_config(tag);
        let mut c = Config::default();
        for p in profiles {
            c.add(p, false).unwrap();
        }
        c.save(&config).unwrap();
        let store = Arc::new(MemoryStore::default());
        let d = web::start_with(
            Options {
                port: 0,
                config: config.clone(),
            },
            store.clone(),
        )
        .expect("starts");
        let port = d.port();
        let open = d.open_url();
        let token = open
            .strip_prefix(&format!("http://127.0.0.1:{port}/#token="))
            .expect("the sign-in URL holds the token in its fragment")
            .to_string();
        assert_eq!(token.len(), 64);
        Self {
            port,
            auth: format!("Bearer {token}"),
            dashboard: Some(d),
            store,
            config,
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn config(&self) -> PathBuf {
        self.config.clone()
    }

    async fn get(&self, path: &str) -> Res {
        raw(
            self.port,
            "GET",
            path,
            &self.host(),
            Some(&self.auth),
            None,
            false,
        )
        .await
    }

    async fn send(&self, method: &str, path: &str, body: Value) -> Res {
        let b = body.to_string();
        raw(
            self.port,
            method,
            path,
            &self.host(),
            Some(&self.auth),
            Some(&b),
            false,
        )
        .await
    }

    async fn post(&self, path: &str, body: Value) -> Res {
        self.send("POST", path, body).await
    }

    /// Connects `name` and waits until its live connection is ready.
    async fn connect(&self, name: &str) -> Value {
        let r = self
            .post("/api/connect", serde_json::json!({ "name": name }))
            .await;
        assert_eq!(r.status, 202, "{}", r.body);
        let t0 = Instant::now();
        loop {
            let s = self.get("/api/state").await.json();
            if s["status"] == "failed" {
                panic!("the connect failed: {s}");
            }
            if s["status"] == "connected" && s["live"] == "ready" {
                return s;
            }
            assert!(t0.elapsed() < WAIT, "not connected in time: {s}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The event stream: `(event, data)` as they come.
    async fn events(&self) -> mpsc::UnboundedReceiver<(String, Value)> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let req = format!(
            "GET /api/events HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nAccept: text/event-stream\r\n\r\n",
            self.host(),
            self.auth
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut r = BufReader::new(s);
            let mut line = String::new();
            // The head.
            loop {
                line.clear();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut pending = String::new();
            let mut name = String::new();
            loop {
                // One chunk.
                line.clear();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let Ok(n) = usize::from_str_radix(line.trim(), 16) else {
                    continue;
                };
                if n == 0 {
                    return;
                }
                let mut chunk = vec![0u8; n + 2];
                if r.read_exact(&mut chunk).await.is_err() {
                    return;
                }
                pending.push_str(&String::from_utf8_lossy(&chunk[..n]));
                while let Some(i) = pending.find('\n') {
                    let l = pending[..i].trim_end_matches('\r').to_string();
                    pending.drain(..=i);
                    if let Some(e) = l.strip_prefix("event: ") {
                        name = e.to_string();
                    } else if let Some(d) = l.strip_prefix("data: ") {
                        let v = serde_json::from_str(d).unwrap_or(Value::Null);
                        if tx.send((name.clone(), v)).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        rx
    }
}

impl Drop for Dash {
    fn drop(&mut self) {
        if let Some(d) = self.dashboard.take() {
            d.shutdown();
        }
    }
}

/// Waits for an event named `name` whose data passes `want`.
async fn wait_for(
    rx: &mut mpsc::UnboundedReceiver<(String, Value)>,
    name: &str,
    want: impl Fn(&Value) -> bool,
) -> Value {
    let t0 = Instant::now();
    loop {
        let left = WAIT.saturating_sub(t0.elapsed());
        let got = tokio::time::timeout(left, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no {name} event in time"))
            .expect("the stream is open");
        if got.0 == name && want(&got.1) {
            return got.1;
        }
    }
}

async fn servers() -> Vec<(&'static str, common::TestServer)> {
    vec![
        ("redis", common::redis().await),
        ("valkey", common::valkey().await),
    ]
}

#[tokio::test]
async fn the_guard_refuses_over_real_http() {
    let d = Dash::start(Vec::new(), "guard").await;
    let host = d.host();
    // No token: 401, no detail.
    let r = raw(d.port, "GET", "/api/state", &host, None, None, false).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.body, r#"{"error":"not signed in"}"#);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    // A wrong token: 401.
    let wrong = format!("Bearer {}", "0".repeat(64));
    let r = raw(
        d.port,
        "GET",
        "/api/state",
        &host,
        Some(&wrong),
        None,
        false,
    )
    .await;
    assert_eq!(r.status, 401);
    // The event stream needs it too.
    let r = raw(d.port, "GET", "/api/events", &host, None, None, false).await;
    assert_eq!(r.status, 401);
    // There is no sign-in route and no cookie any more.
    let token = d.auth.strip_prefix("Bearer ").unwrap().to_string();
    let r = raw(
        d.port,
        "GET",
        &format!("/auth/{token}"),
        &host,
        None,
        None,
        false,
    )
    .await;
    assert_eq!(r.status, 404);
    assert!(r.header("set-cookie").is_none());
    assert!(!r.body.contains(&token));
    // DNS rebinding: another Host is 403, even signed in.
    let r = raw(
        d.port,
        "GET",
        "/api/state",
        &format!("evil.example:{}", d.port),
        Some(&d.auth),
        None,
        false,
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    // A write without the origin and header: 403.
    let r = raw(
        d.port,
        "POST",
        "/api/disconnect",
        &host,
        Some(&d.auth),
        Some("{}"),
        true,
    )
    .await;
    assert_eq!(r.status, 403);
    // Signed in: 200 and the security headers.
    let r = d.get("/api/state").await;
    assert_eq!(r.status, 200);
    assert!(
        r.header("content-security-policy")
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert!(r.header("set-cookie").is_none());
    // Without the token `/` is the page; an asset is served with its type.
    let r = raw(d.port, "GET", "/", &host, None, None, false).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    let r = raw(d.port, "GET", "/assets/app.js", &host, None, None, false).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
}

#[tokio::test]
async fn phase_one_end_to_end_on_redis_and_valkey() {
    for (kind, server) in servers().await {
        one_server(kind, &server).await;
    }
}

async fn one_server(kind: &str, server: &common::TestServer) {
    let mut admin = server.admin().await;
    cmd(
        &mut admin,
        &["CONFIG", "SET", "slowlog-log-slower-than", "0"],
    )
    .await;
    cmd(&mut admin, &["SLOWLOG", "RESET"]).await;
    cmd(
        &mut admin,
        &[
            "ACL",
            "SETUSER",
            "lim",
            "on",
            &format!(">{LIM_PASS}"),
            "~*",
            "&*",
            "+@all",
            "-get",
            "-client",
            "-config",
        ],
    )
    .await;
    let mut limited = profile("limited", server.port, Env::Local, None);
    limited.auth = Auth::Keychain;
    let d = Dash::start(
        vec![
            profile("rw", server.port, Env::Local, None),
            profile("prod-rw", server.port, Env::Prod, Some(false)),
            profile("ro", server.port, Env::Prod, None),
            limited.clone(),
        ],
        kind,
    )
    .await;
    d.store
        .set(
            &limited.secret_ref(),
            &Credentials {
                username: Some("lim".into()),
                password: SecretString::from(LIM_PASS.to_string()),
            },
        )
        .unwrap();
    let mut events = d.events().await;
    wait_for(&mut events, "state", |_| true).await;

    // Connect and the state.
    let s = d.connect("rw").await;
    assert_eq!(s["connection"]["name"], "rw", "{kind}");
    assert_eq!(s["server"]["flavor"], kind, "{kind}: {s}");
    assert_eq!(s["writes"], true, "{kind}");
    assert_eq!(s["nodes"].as_array().unwrap().len(), 0, "{kind}");

    // Overview: the connect's figures and the permission checks.
    let o = d.get("/api/overview").await.json();
    assert_eq!(o["figures"]["server"]["mode"], "standalone", "{kind}: {o}");
    assert!(o["figures"]["keys"].is_u64(), "{kind}: {o}");
    let perms = o["perms"].as_array().unwrap();
    assert!(!perms.is_empty(), "{kind}");
    assert!(perms.iter().any(|p| p["status"] == "allowed"), "{kind}");
    // The live figures come over the stream while it is open.
    let f = wait_for(&mut events, "figures", |v| v["info"].is_object()).await;
    assert!(f["info"]["used_memory"].is_u64(), "{kind}: {f}");
    let f = wait_for(&mut events, "figures", |v| v["rates"]["ops"].is_number()).await;
    assert!(f["rates"]["ops"].as_f64().unwrap() >= 0.0, "{kind}");

    // Clients: grouped.
    let c = d.get("/api/server/clients").await.json();
    assert!(
        !c["connections"].as_array().unwrap().is_empty(),
        "{kind}: {c}"
    );
    assert!(
        c["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["kind"] == "lib"),
        "{kind}"
    );
    assert!(!c.to_string().contains("\"user\""), "{kind}");

    // Slow log: a credential argument comes back masked.
    cmd(&mut admin, &["CONFIG", "SET", "masterauth", PASS]).await;
    let r = d.get("/api/server/slowlog").await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    assert!(!r.body.contains(PASS), "{kind}: {}", r.body);
    let sl = r.json();
    let lines: Vec<&str> = sl["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["line"].as_str().unwrap())
        .collect();
    assert!(
        lines.iter().any(|l| l.starts_with("CONFIG SET masterauth")),
        "{kind}: {lines:?}"
    );
    assert!(sl["settings"]["threshold_micros"].is_i64(), "{kind}: {sl}");

    // Overview's findings, by the TUI's own rules: the slowest slow-log entry, pointing
    // at Server > Slow log.
    let o = d.get("/api/overview").await.json();
    let found = o["findings"].as_array().unwrap();
    assert!(
        found.iter().any(|f| f["screen"] == "slow_log"
            && f["panel"] == "Slow log"
            && f["area"] == "Server"),
        "{kind}: {o}"
    );
    assert!(
        found
            .iter()
            .all(|f| !f["text"].as_str().unwrap().contains(PASS)),
        "{kind}"
    );

    // Monitor asks first (yes or no off prod), starts only on the answer; lines over
    // the stream, credentials masked; stopped on request.
    let r = d.post("/api/server/monitor", serde_json::json!({})).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    let q = r.json();
    assert_eq!(q["confirm"]["kind"], "ask", "{kind}: {q}");
    assert_eq!(
        d.get("/api/state").await.json()["monitor"],
        Value::Null,
        "{kind}: not started by the question"
    );
    let id = q["confirm"]["id"].as_str().unwrap();
    let r = d
        .post(&format!("/api/confirm/{id}"), serde_json::json!({}))
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    assert_eq!(r.json()["max_secs"], 30);
    wait_for(&mut events, "monitor", |v| v["kind"] == "ready").await;
    // The server hides some of these from MONITOR itself; what it shows is masked.
    cmd(&mut admin, &["CONFIG", "SET", "masterauth", PASS]).await;
    cmd(
        &mut admin,
        &["ACL", "SETUSER", "mon-tmp", "on", &format!(">{PASS}")],
    )
    .await;
    cmd(&mut admin, &["SET", "mon:key", "plain"]).await;
    let mut seen = Vec::new();
    let t0 = Instant::now();
    while !seen.iter().any(|l: &String| l == "SET mon:key plain") {
        assert!(t0.elapsed() < WAIT, "{kind}: {seen:?}");
        let v = wait_for(&mut events, "monitor", |v| v["kind"] == "lines").await;
        for l in v["lines"].as_array().unwrap() {
            seen.push(l["line"].as_str().unwrap().to_string());
        }
    }
    assert!(seen.iter().all(|l| !l.contains(PASS)), "{kind}: {seen:?}");
    let r = d
        .send("DELETE", "/api/server/monitor", serde_json::json!({}))
        .await;
    assert_eq!(r.json()["stopped"], true, "{kind}");

    // Pub/Sub: the tail, the counts, and a publish through the gate.
    let r = d
        .post(
            "/api/pubsub/subscribe",
            serde_json::json!({ "name": "news" }),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    wait_for(&mut events, "tail", |v| v["kind"] == "ready").await;
    let ch = d.get("/api/pubsub/channels").await.json();
    let news = ch["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "news")
        .cloned()
        .unwrap_or_else(|| panic!("{kind}: {ch}"));
    assert_eq!(news["subscribers"], 1, "{kind}");
    let r = d
        .post(
            "/api/pubsub/publish",
            serde_json::json!({ "channel": "news", "message": "hello web" }),
        )
        .await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    let p = r.json();
    assert_eq!(p["reply"]["kind"], "int", "{kind}: {p}");
    assert_eq!(p["reply"]["value"], 1, "{kind}: {p}");
    let t = wait_for(&mut events, "tail", |v| v["kind"] == "messages").await;
    assert_eq!(t["messages"][0]["channel"], "news", "{kind}: {t}");
    assert_eq!(t["messages"][0]["preview"], "hello web", "{kind}: {t}");
    let r = d
        .send("DELETE", "/api/pubsub/subscribe", serde_json::json!({}))
        .await;
    assert_eq!(r.json()["stopped"], true, "{kind}");

    // The command line.
    let run = |line: &str| serde_json::json!({ "line": line });
    let r = d.post("/api/cmdline", run("SET web:k v1")).await.json();
    assert_eq!(r["status"], "ok", "{kind}: {r}");
    assert_eq!(r["replies"][0]["text"][0], "OK", "{kind}: {r}");
    let r = d.post("/api/cmdline", run("GET web:k")).await.json();
    assert_eq!(r["replies"][0]["text"][0], "\"v1\"", "{kind}: {r}");
    assert!(r["replies"][0]["json"].is_array(), "{kind}");
    let r = d.post("/api/cmdline", run("3 GET web:k")).await.json();
    assert_eq!(r["replies"].as_array().unwrap().len(), 3, "{kind}: {r}");
    let r = d.post("/api/cmdline", run("101 GET web:k")).await.json();
    assert_eq!(r["status"], "note", "{kind}: {r}");
    let r = d
        .post("/api/cmdline", run("EVAL \"return 1\" 0"))
        .await
        .json();
    assert_eq!(r["status"], "note", "{kind}: {r}");
    assert!(
        r["text"].as_str().unwrap().contains("script"),
        "{kind}: {r}"
    );
    let r = d.post("/api/cmdline", run("BLPOP q 0")).await.json();
    assert_eq!(r["status"], "note", "{kind}: {r}");
    // `KEYS` asks first, then is sent.
    let r = d.post("/api/cmdline", run("KEYS web:*")).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    let q = r.json();
    assert_eq!(q["confirm"]["kind"], "ask", "{kind}: {q}");
    let id = q["confirm"]["id"].as_str().unwrap();
    let r = d
        .post(&format!("/api/confirm/{id}"), serde_json::json!({}))
        .await
        .json();
    assert_eq!(r["replies"][0]["text"][0], "1) \"web:k\"", "{kind}: {r}");
    // Help and completion.
    // `COMMAND LIST` and `COMMAND DOCS` are Redis 7.0 and later: on 6.2 both turn off
    // and say why, as in the TUI.
    let old = kind == "redis" && common::redis_image().1.starts_with("6.");
    let names = d.get("/api/cmdline/commands").await.json();
    if old {
        assert!(names["off"].is_string(), "{kind}: {names}");
    } else {
        assert!(
            names["names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n == "get"),
            "{kind}: {names}"
        );
    }
    let h = d.get("/api/cmdline/help?name=get").await;
    assert_eq!(h.status, 200, "{kind}: {}", h.body);
    if old {
        assert!(h.json()["off"].is_string(), "{kind}: {}", h.body);
    }

    // A writable prod profile: Monitor wants the name typed; the publish asks first,
    // then is sent once confirmed.
    d.connect("prod-rw").await;
    let r = d.post("/api/server/monitor", serde_json::json!({})).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    let q = r.json();
    assert_eq!(q["confirm"]["kind"], "type", "{kind}: {q}");
    assert_eq!(q["confirm"]["expect"], "prod-rw", "{kind}: {q}");
    let id = q["confirm"]["id"].as_str().unwrap();
    let r = d
        .post(
            &format!("/api/confirm/{id}"),
            serde_json::json!({ "typed": "prod" }),
        )
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    assert_eq!(d.get("/api/state").await.json()["monitor"], Value::Null);
    let r = d
        .post(
            "/api/pubsub/publish",
            serde_json::json!({ "channel": "news", "message": "m" }),
        )
        .await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    let q = r.json();
    assert_eq!(q["confirm"]["kind"], "ask", "{kind}");
    let id = q["confirm"]["id"].as_str().unwrap().to_string();
    let r = d
        .post(&format!("/api/confirm/{id}"), serde_json::json!({}))
        .await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    assert_eq!(r.json()["sent"], true, "{kind}");
    let r = d
        .post(&format!("/api/confirm/{id}"), serde_json::json!({}))
        .await;
    assert_eq!(r.status, 410, "{kind}: single use");

    // A read-only profile: writes are refused before anything is asked or sent.
    let s = d.connect("ro").await;
    assert_eq!(s["writes"], false, "{kind}");
    let r = d
        .post(
            "/api/pubsub/publish",
            serde_json::json!({ "channel": "news", "message": "m" }),
        )
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    assert!(
        r.json()["error"].as_str().unwrap().contains("read-only"),
        "{kind}"
    );
    let r = d.post("/api/cmdline", run("SET web:k v2")).await.json();
    assert_eq!(r["status"], "note", "{kind}: {r}");
    assert!(
        r["text"].as_str().unwrap().contains("read-only"),
        "{kind}: {r}"
    );
    let v: Option<String> = redis::cmd("GET")
        .arg("web:k")
        .query_async(&mut admin)
        .await
        .unwrap();
    assert_eq!(v.as_deref(), Some("v1"), "{kind}: nothing was written");

    // A user refused GET, CLIENT and CONFIG: the feature is off, in fixed words, and
    // the slow log says why its settings are missing in every reply, not only the
    // first.
    d.connect("limited").await;
    for n in 0..2 {
        let sl = d.get("/api/server/slowlog").await.json();
        assert_eq!(sl["settings"], Value::Null, "{kind} {n}: {sl}");
        assert!(sl["settings_off"].is_string(), "{kind} {n}: {sl}");
    }
    let r = d.post("/api/cmdline", run("GET web:k")).await.json();
    assert_eq!(r["status"], "failed", "{kind}: {r}");
    assert_eq!(
        r["text"], "the server refused GET for this user",
        "{kind}: {r}"
    );
    let c = d.get("/api/server/clients").await;
    assert_eq!(c.status, 200, "{kind}: {}", c.body);
    assert!(c.json()["off"].is_string(), "{kind}: {}", c.body);
    let all = format!("{}{}", r, c.body);
    assert!(
        !all.contains("lim\"") && !all.contains(LIM_PASS),
        "{kind}: {all}"
    );

    // Disconnect.
    let r = d
        .post("/api/disconnect", serde_json::json!({}))
        .await
        .json();
    assert_eq!(r["status"], "idle", "{kind}");
}

#[tokio::test]
async fn a_cluster_connects_with_its_nodes_and_routes_by_slot() {
    for valkey in [false, true] {
        let kind = if valkey { "valkey" } else { "redis" };
        let c = common::cluster::cluster(valkey, false).await;
        let d = Dash::start(
            vec![profile("cl", c.seed_port(), Env::Local, None)],
            &format!("cluster-{kind}"),
        )
        .await;
        let s = d.connect("cl").await;
        let nodes = s["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 3, "{kind}: {s}");
        assert!(nodes.iter().all(|n| n["connected"] == true), "{kind}");
        assert_eq!(s["server"]["mode"], "cluster", "{kind}");
        // Overview has one row per shard.
        let o = d.get("/api/overview").await.json();
        assert_eq!(o["shards"].as_array().unwrap().len(), 3, "{kind}: {o}");
        // A key goes to the owner of its slot, whichever node is picked.
        let r = d.post("/api/node", serde_json::json!({ "index": 2 })).await;
        assert_eq!(r.json()["picked"], 2, "{kind}");
        let r = d
            .post(
                "/api/cmdline",
                serde_json::json!({ "line": "SET web:{a}k v" }),
            )
            .await
            .json();
        assert_eq!(r["status"], "ok", "{kind}: {r}");
        assert!(r["node"].is_string(), "{kind}: {r}");
        let r = d
            .post(
                "/api/cmdline",
                serde_json::json!({ "line": "GET web:{a}k" }),
            )
            .await
            .json();
        assert_eq!(r["replies"][0]["text"][0], "\"v\"", "{kind}: {r}");
        // Keys in different slots are refused before anything is sent.
        let r = d
            .post(
                "/api/cmdline",
                serde_json::json!({ "line": "MGET web:{a}k web:{b}k" }),
            )
            .await
            .json();
        assert_eq!(r["status"], "note", "{kind}: {r}");
        // The picked node's clients.
        let cl = d.get("/api/server/clients").await.json();
        assert_eq!(cl["node"], 2, "{kind}: {cl}");
        let r = d.post("/api/node", serde_json::json!({ "index": 9 })).await;
        assert_eq!(r.status, 422, "{kind}");
    }
}

/// A server that needs a password, connected without one and with a wrong one: the
/// connect fails in redis-lens's own words, and no reply or event carries the server's
/// text (rule 3), on Redis and Valkey.
#[tokio::test]
async fn a_server_that_wants_a_password_fails_the_connect_in_fixed_words() {
    for (kind, server) in [
        (
            "redis",
            common::redis_with(&["--requirepass", "probe-pw-9"]).await,
        ),
        (
            "valkey",
            common::valkey_with(&["--requirepass", "probe-pw-9"]).await,
        ),
    ] {
        let none = profile("nopw", server.port, Env::Local, None);
        let mut wrong = profile("wrongpw", server.port, Env::Local, None);
        wrong.auth = Auth::Keychain;
        let d = Dash::start(vec![none, wrong.clone()], &format!("auth-{kind}")).await;
        d.store
            .set(
                &wrong.secret_ref(),
                &Credentials {
                    username: None,
                    password: SecretString::from("bad-pw-9".to_string()),
                },
            )
            .unwrap();
        let mut events = d.events().await;
        let mut seen = Vec::new();
        for name in ["nopw", "wrongpw"] {
            let r = d
                .post("/api/connect", serde_json::json!({ "name": name }))
                .await;
            assert_eq!(r.status, 202, "{kind}: {}", r.body);
            let t0 = Instant::now();
            let s = loop {
                let s = d.get("/api/state").await;
                let v = s.json();
                seen.push(s.body);
                if v["status"] == "failed" {
                    break v;
                }
                assert!(t0.elapsed() < WAIT, "{kind} {name}: {v}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            assert_ne!(s["live"], "ready", "{kind} {name}: {s}");
            let want = if name == "nopw" {
                web::wording::NEEDS_PASSWORD
            } else {
                web::wording::BAD_CREDENTIALS
            };
            assert_eq!(s["error"], want, "{kind} {name}: {s}");
            seen.push(d.get("/api/overview").await.body);
            seen.push(
                d.post(
                    &format!("/api/connections/{name}/test"),
                    serde_json::json!({}),
                )
                .await
                .body,
            );
            seen.push(d.get("/api/server/clients").await.body);
            seen.push(
                d.post("/api/cmdline", serde_json::json!({ "line": "GET k" }))
                    .await
                    .body,
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(e) = events.try_recv() {
            seen.push(e.1.to_string());
        }
        for b in &seen {
            for word in [
                "NOAUTH",
                "WRONGPASS",
                "ERR",
                "Authentication required",
                "AuthenticationFailed",
                "probe-pw-9",
                "bad-pw-9",
            ] {
                assert!(!b.contains(word), "{kind}: {word} in {b}");
            }
        }
    }
}

// -- a fake server that refuses everything with MISCONF --------------------------

const MISCONF: &str = "MISCONF Redis is configured to save RDB snapshots, but it's currently unable to persist to disk.";

/// The complete RESP commands at the start of `buf`, each as its first word in capitals,
/// and how many bytes they take.
fn whole_commands(buf: &[u8]) -> (Vec<String>, usize) {
    fn line(b: &[u8], at: usize) -> Option<(&[u8], usize)> {
        let rel = b.get(at..)?.windows(2).position(|w| w == b"\r\n")?;
        Some((&b[at..at + rel], at + rel + 2))
    }
    let (mut out, mut at) = (Vec::new(), 0);
    'cmds: while let Some((head, mut p)) = line(buf, at) {
        if head.first() != Some(&b'*') {
            // An inline command.
            let word = String::from_utf8_lossy(head)
                .split(' ')
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            out.push(word);
            at = p;
            continue;
        }
        let count: usize = std::str::from_utf8(&head[1..]).unwrap().parse().unwrap();
        let mut first = String::new();
        for i in 0..count {
            let Some((len, q)) = line(buf, p) else {
                break 'cmds;
            };
            let len: usize = std::str::from_utf8(&len[1..]).unwrap().parse().unwrap();
            if buf.len() < q + len + 2 {
                break 'cmds;
            }
            if i == 0 {
                first = String::from_utf8_lossy(&buf[q..q + len]).to_ascii_uppercase();
            }
            p = q + len + 2;
        }
        out.push(first);
        at = p;
    }
    (out, at)
}

/// A server on 127.0.0.1 that answers every command with `-MISCONF ...`, or, when
/// `ping_ok`, `PING` with `PONG` and every other command with `-MISCONF ...`.
async fn misconf_server(ping_ok: bool) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(k) = s.read(&mut chunk).await else {
                        return;
                    };
                    if k == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..k]);
                    let (cmds, used) = whole_commands(&buf);
                    buf.drain(..used);
                    let mut out = String::new();
                    for c in cmds {
                        if ping_ok && c == "PING" {
                            out.push_str("+PONG\r\n");
                        } else {
                            out.push_str(&format!("-{MISCONF}\r\n"));
                        }
                    }
                    if !out.is_empty() && s.write_all(out.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// Rule 3 with a server error code the old word list did not know: `MISCONF` on the
/// connect and on the Pub/Sub tail reaches neither the state, nor an event, nor a reply.
#[tokio::test]
async fn a_misconf_server_never_reaches_the_page_in_its_own_words() {
    for ping_ok in [false, true] {
        let port = misconf_server(ping_ok).await;
        let d = Dash::start(
            vec![profile("fake", port, Env::Local, Some(false))],
            &format!("misconf-{ping_ok}"),
        )
        .await;
        let mut events = d.events().await;
        let r = d
            .post("/api/connect", serde_json::json!({ "name": "fake" }))
            .await;
        assert_eq!(r.status, 202, "{}", r.body);
        let mut seen = Vec::new();
        let t0 = Instant::now();
        let state = loop {
            let s = d.get("/api/state").await;
            let v = s.json();
            seen.push(s.body);
            if v["status"] == "failed" || (v["status"] == "connected" && v["live"] != "connecting")
            {
                break v;
            }
            assert!(t0.elapsed() < WAIT, "{v}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        if state["status"] == "connected" {
            // The tail opens its own connection: MISCONF there closes it.
            seen.push(
                d.post(
                    "/api/pubsub/subscribe",
                    serde_json::json!({ "name": "chan1" }),
                )
                .await
                .body,
            );
            let tail = wait_for(&mut events, "tail", |v| {
                v["kind"] == "closed" || v["error"].is_string()
            })
            .await;
            seen.push(tail.to_string());
            for path in [
                "/api/overview",
                "/api/server/clients",
                "/api/server/slowlog",
                "/api/pubsub",
            ] {
                seen.push(d.get(path).await.body);
            }
            seen.push(
                d.post("/api/cmdline", serde_json::json!({ "line": "GET k" }))
                    .await
                    .body,
            );
        } else {
            assert_eq!(state["error"], web::wording::SERVER_REFUSED, "{state}");
        }
        seen.push(d.get("/api/state").await.body);
        while let Ok(Some(e)) =
            tokio::time::timeout(Duration::from_millis(300), events.recv()).await
        {
            seen.push(e.1.to_string());
        }
        for s in &seen {
            assert!(
                !s.contains("MISCONF") && !s.contains("persist to disk") && !s.contains("RDB"),
                "ping_ok={ping_ok}: {s}"
            );
        }
    }
}

/// Sets `name`'s writes in the config file at `path`, as `w` in the TUI beside the
/// dashboard or `conn set --writes` would.
fn flip_writes(path: &std::path::Path, name: &str, on: bool) {
    let mut c = Config::load(path).unwrap();
    c.set_writes(name, on).unwrap();
    c.save(path).unwrap();
}

/// S3 and S8 on Redis and Valkey: a profile made read-only in the file while connected
/// refuses `SET` on the command line and `PUBLISH` at once and the state says so; an
/// answer given after the profile went read-only, or after another connect, sends nothing.
#[tokio::test]
async fn writes_follow_the_config_file_and_answers_follow_the_connection() {
    for (kind, server) in servers().await {
        let mut admin = server.admin().await;
        let writable = profile("rw", server.port, Env::Stage, Some(false));
        let prod = profile("prodw", server.port, Env::Prod, Some(false));
        let d = Dash::start(vec![writable, prod], &format!("follow-{kind}")).await;
        let config = d.config();
        d.connect("rw").await;
        let r = d
            .post(
                "/api/cmdline",
                serde_json::json!({ "line": "SET s3-key before" }),
            )
            .await;
        assert_eq!(r.json()["sent"], 1, "{kind}: {}", r.body);
        flip_writes(&config, "rw", false);
        let r = d
            .post(
                "/api/cmdline",
                serde_json::json!({ "line": "SET s3-key after" }),
            )
            .await;
        let v = r.json();
        assert_eq!(v["sent"], 0, "{kind}: {v}");
        assert_eq!(v["status"], "note", "{kind}: {v}");
        let r = d
            .post(
                "/api/pubsub/publish",
                serde_json::json!({ "channel": "c", "message": "m" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        let s = d.get("/api/state").await.json();
        assert_eq!(s["writes"], false, "{kind}: {s}");
        assert!(s["writes_off"].is_string(), "{kind}: {s}");
        let got: Option<String> = redis::cmd("GET")
            .arg("s3-key")
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("before"), "{kind}");

        // A question, then the profile goes read-only: the answer sends nothing.
        d.connect("prodw").await;
        let ask = || {
            d.post(
                "/api/cmdline",
                serde_json::json!({ "line": "SET s3-key asked" }),
            )
        };
        let r = ask().await;
        assert_eq!(r.status, 409, "{kind}: {}", r.body);
        let id = r.json()["confirm"]["id"].as_str().unwrap().to_string();
        flip_writes(&config, "prodw", false);
        let r = d
            .post(
                &format!("/api/confirm/{id}"),
                serde_json::json!({ "typed": "prodw" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        // A question, then another connect: the answer sends nothing either.
        flip_writes(&config, "prodw", true);
        let r = ask().await;
        assert_eq!(r.status, 409, "{kind}: {}", r.body);
        let id = r.json()["confirm"]["id"].as_str().unwrap().to_string();
        flip_writes(&config, "rw", true);
        d.connect("rw").await;
        let r = d
            .post(
                &format!("/api/confirm/{id}"),
                serde_json::json!({ "typed": "prodw" }),
            )
            .await;
        assert!(
            r.status == 409 || r.status == 410,
            "{kind}: {} {}",
            r.status,
            r.body
        );
        let got: Option<String> = redis::cmd("GET")
            .arg("s3-key")
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("before"), "{kind}");
    }
}

/// S6 and S8 on Redis and Valkey: the dashboard's session takes from the process-wide
/// budget a TUI session of the same profile and node takes from (`budget::shared`, as
/// the TUI's connect calls it), and the connection test's probes take from it too.
#[tokio::test]
async fn the_dashboard_the_tui_and_the_connection_test_share_one_budget() {
    for (kind, server) in servers().await {
        let p = profile("shared", server.port, Env::Stage, Some(false));
        let rate = redis_lens::scan::ScanOptions::for_profile(&p).max_commands_per_sec;
        let node = p.endpoint.to_string();
        let d = Dash::start(vec![p], &format!("budget-{kind}")).await;
        d.connect("shared").await;
        // What the TUI's connect of the same profile would get: the dashboard's budget.
        let b = redis_lens::budget::shared("shared", &node, rate);
        assert!(
            b.taken() > 0,
            "{kind}: the dashboard's connect took from it"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        let before = b.taken();
        let r = d.get("/api/server/clients").await;
        assert_eq!(r.status, 200, "{kind}: {}", r.body);
        let after_clients = b.taken();
        assert!(after_clients > before, "{kind}: a live read takes from it");
        let r = d
            .post("/api/connections/shared/test", serde_json::json!({}))
            .await;
        assert_eq!(r.status, 200, "{kind}: {}", r.body);
        let probes = b.taken() - after_clients;
        assert!(probes >= 5, "{kind}: the test's probes took {probes}");
    }
}

/// `INFO` calls so far (`cmdstat_info:calls=N` in `INFO commandstats`, the same name on
/// Redis 6.2, 7 and Valkey). The read itself counts: each read adds one.
async fn info_calls(admin: &mut MultiplexedConnection) -> u64 {
    let text: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(admin)
        .await
        .unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix("cmdstat_info:calls="))
        .and_then(|rest| rest.split(',').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// S8 on Redis and Valkey, counted on the server: the live figures send `INFO` only
/// while an event stream is open and polling is not paused.
#[tokio::test]
async fn no_info_is_sent_while_paused_or_with_no_event_stream() {
    for (kind, server) in servers().await {
        let mut admin = server.admin().await;
        let d = Dash::start(
            vec![profile("polled", server.port, Env::Local, None)],
            &format!("paused-{kind}"),
        )
        .await;
        d.connect("polled").await;
        // Let the connect's own reads land.
        tokio::time::sleep(Duration::from_millis(500)).await;
        // No stream: nothing polls.
        let a = info_calls(&mut admin).await;
        tokio::time::sleep(Duration::from_millis(3_000)).await;
        let b = info_calls(&mut admin).await;
        assert_eq!(b - a, 1, "{kind}: no stream, only the count's own INFO");
        // A stream at 1 s: it polls.
        let r = d
            .post("/api/polling", serde_json::json!({ "interval_secs": 1 }))
            .await;
        assert_eq!(r.status, 200, "{kind}: {}", r.body);
        let mut events = d.events().await;
        wait_for(&mut events, "figures", |_| true).await;
        let c = info_calls(&mut admin).await;
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let e = info_calls(&mut admin).await;
        assert!(e - c >= 2, "{kind}: polled {}", e - c - 1);
        // Paused: at most the one in flight when the pause came.
        let r = d
            .post("/api/polling", serde_json::json!({ "paused": true }))
            .await;
        assert_eq!(r.status, 200, "{kind}: {}", r.body);
        let f = info_calls(&mut admin).await;
        tokio::time::sleep(Duration::from_millis(3_000)).await;
        let g = info_calls(&mut admin).await;
        assert!(g - f <= 2, "{kind}: paused, {} sent", g - f - 1);
        let h = info_calls(&mut admin).await;
        tokio::time::sleep(Duration::from_millis(3_000)).await;
        let i = info_calls(&mut admin).await;
        assert_eq!(i - h, 1, "{kind}: paused, nothing at all once settled");
        drop(events);
    }
}
