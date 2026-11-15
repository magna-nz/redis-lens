//! The web dashboard's phase 2 over real HTTP against real Redis and Valkey servers and
//! a cluster: the database list and switch (the tail subscribed again after it),
//! Browse › Keys from key names and from an analysis, a pattern's keys over several
//! pages, Find key, a key's value of every type (with its formats, its pages and the
//! cap on items held), an analysis run to completion over the event stream (and one
//! stopped, which is not saved), each Analyze panel, Overview's analysis part, and
//! snapshots saved, listed and compared. A limited ACL user turns features off in
//! redis-lens's own words, and a user's name and password reach no reply, event or
//! snapshot file.
//!
//! The config file and the snapshot directory are temporary ones and credentials are
//! in a `MemoryStore`: the user's config, snapshots and keychain are never touched.
//!
//! With `REDIS_LENS_P2_CAPTURE=<file>` every reply and event is appended to that file as
//! JSON lines, for the examples in `docs/dashboard-api.md`.
#![cfg(feature = "integration-tests")]

mod common;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use redis_lens::config::Config;
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile};
use redis_lens::secrets::{MemoryStore, SecretStore};
use redis_lens::web::wording;
use redis_lens::web::{self, Dashboard, Options};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(60);
const CAPTURE: &str = "REDIS_LENS_P2_CAPTURE";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-web-p2-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn profile(name: &str, port: u16) -> Profile {
    Profile {
        name: name.into(),
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port,
            tls: false,
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

async fn cmd(conn: &mut MultiplexedConnection, args: &[&[u8]]) -> redis::Value {
    let mut c = redis::cmd(std::str::from_utf8(args[0]).unwrap());
    for a in &args[1..] {
        c.arg(*a);
    }
    c.query_async(conn).await.expect("command")
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn capture(label: &str, what: Value) {
    let Ok(path) = std::env::var(CAPTURE) else {
        return;
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    let mut v = what;
    v["label"] = json!(label);
    writeln!(f, "{v}").unwrap();
}

// -- a small HTTP/1.1 client ------------------------------------------------------

struct Res {
    status: u16,
    body: String,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

async fn raw(port: u16, method: &str, path: &str, auth: &str, body: Option<&str>) -> Res {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAuthorization: {auth}\r\n"
    );
    if method != "GET" {
        req.push_str(&format!(
            "Origin: http://127.0.0.1:{port}\r\nX-Redis-Lens: 1\r\nContent-Type: application/json\r\n"
        ));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(WAIT * 2, s.read_to_end(&mut buf))
        .await
        .expect("the server answers")
        .unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, rest) = text.split_once("\r\n\r\n").expect("a head");
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_string()
    };
    Res { status, body }
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
    auth: String,
    dashboard: Option<Dashboard>,
    snapshots: PathBuf,
    label: String,
    /// Every reply body and event data line, for the checks that something is nowhere.
    seen: Arc<Mutex<Vec<String>>>,
}

impl Dash {
    async fn start(profiles: Vec<Profile>, tag: &str) -> Self {
        Self::start_with(profiles, tag, Arc::new(MemoryStore::default())).await
    }

    /// [`Self::start`] reading saved profiles' credentials from `store`.
    async fn start_with(profiles: Vec<Profile>, tag: &str, store: Arc<MemoryStore>) -> Self {
        let dir = temp_dir(tag);
        let config = dir.join("config.toml");
        let mut c = Config::default();
        for p in profiles {
            c.add(p, false).unwrap();
        }
        c.save(&config).unwrap();
        let snapshots = dir.join("snapshots");
        let d =
            web::start_with_snapshots(Options { port: 0, config }, store, Some(snapshots.clone()))
                .expect("starts");
        let port = d.port();
        let token = d
            .open_url()
            .split_once("#token=")
            .map(|(_, t)| t.to_string())
            .unwrap();
        Self {
            port,
            auth: format!("Bearer {token}"),
            dashboard: Some(d),
            snapshots,
            label: tag.to_string(),
            seen: Arc::default(),
        }
    }

    /// Every reply and event so far.
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    async fn get(&self, path: &str) -> Res {
        let r = raw(self.port, "GET", path, &self.auth, None).await;
        self.record("GET", path, None, &r);
        r
    }

    async fn send(&self, method: &str, path: &str, body: Value) -> Res {
        let b = body.to_string();
        let r = raw(self.port, method, path, &self.auth, Some(&b)).await;
        self.record(method, path, Some(body), &r);
        r
    }

    async fn post(&self, path: &str, body: Value) -> Res {
        self.send("POST", path, body).await
    }

    fn record(&self, method: &str, path: &str, body: Option<Value>, r: &Res) {
        self.seen.lock().unwrap().push(r.body.clone());
        let reply: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
        capture(
            &self.label,
            json!({ "method": method, "path": path, "body": body, "status": r.status, "reply": reply }),
        );
    }

    /// The state once `want` holds for it.
    async fn state_until(&self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        let t0 = Instant::now();
        loop {
            let r = raw(self.port, "GET", "/api/state", &self.auth, None).await;
            self.seen.lock().unwrap().push(r.body.clone());
            let s = r.json();
            if s["status"] == "failed" {
                panic!("the connect failed: {s}");
            }
            if want(&s) {
                return s;
            }
            assert!(t0.elapsed() < WAIT, "{what} not in time: {s}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn ready(&self) -> Value {
        self.state_until("ready", |s| {
            s["status"] == "connected"
                && s["live"] == "ready"
                && !matches!(
                    s["analysis"]["status"].as_str(),
                    Some("connecting" | "sampling" | "stopping")
                )
        })
        .await
    }

    async fn connect(&self, name: &str) -> Value {
        let r = self.post("/api/connect", json!({ "name": name })).await;
        assert_eq!(r.status, 202, "{}", r.body);
        self.ready().await
    }

    async fn events(&self) -> mpsc::UnboundedReceiver<(String, Value)> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let req = format!(
            "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: {}\r\nAccept: text/event-stream\r\n\r\n",
            self.port, self.auth
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let label = self.label.clone();
        let seen = Arc::clone(&self.seen);
        tokio::spawn(async move {
            let mut r = BufReader::new(s);
            let mut line = String::new();
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
                        seen.lock().unwrap().push(d.to_string());
                        let v: Value = serde_json::from_str(d).unwrap_or(Value::Null);
                        if name == "progress" {
                            capture(&label, json!({ "event": name, "data": v }));
                        }
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
        if let Some(dir) = self.snapshots.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

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

/// Runs a Quick analysis to its end over the event stream; returns the `done` event.
async fn analyze(d: &Dash, rx: &mut mpsc::UnboundedReceiver<(String, Value)>) -> Value {
    let r = d.post("/api/analysis", json!({ "depth": "quick" })).await;
    assert_eq!(r.status, 202, "{}", r.body);
    let run = r.json()["run"].clone();
    wait_for(rx, "progress", |v| {
        v["run"] == run && v["kind"] == "started"
    })
    .await;
    let done = wait_for(rx, "progress", |v| v["run"] == run && v["kind"] == "done").await;
    wait_for(rx, "progress", |v| {
        v["run"] == run && v["kind"] == "compared"
    })
    .await;
    d.ready().await;
    done
}

fn find_row<'a>(rows: &'a [Value], name: &str) -> Option<&'a Value> {
    rows.iter().find_map(|r| {
        if r["kind"] == "group" {
            r["children"]
                .as_array()
                .and_then(|c| c.iter().find(|k| k["pattern"]["name"] == name))
        } else {
            (r["pattern"]["name"] == name).then_some(r)
        }
    })
}

const EVIL: &str = "evil\u{1b}[31mkey";

/// Keys of every type in db 0, a few in db 1, and the LFU policy for Hot keys.
async fn seed(admin: &mut MultiplexedConnection) {
    cmd(
        admin,
        &[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lfu"],
    )
    .await;
    cmd(admin, &[b"SET", b"str:plain", b"hello world"]).await;
    cmd(admin, &[b"SET", b"str:json", br#"{"a":1,"b":[1,2]}"#]).await;
    cmd(admin, &[b"SET", b"str:bin", b"\x00\x01\xffbin\x1b"]).await;
    cmd(admin, &[b"SET", EVIL.as_bytes(), b"v"]).await;
    cmd(
        admin,
        &[
            b"HSET",
            b"user:1:profile",
            b"name",
            b"Ada",
            b"email",
            b"ada@example.com",
        ],
    )
    .await;
    cmd(admin, &[b"RPUSH", b"queue:jobs", b"j1", b"j2", b"j3"]).await;
    cmd(admin, &[b"SADD", b"tags:1", b"red", b"blue"]).await;
    cmd(
        admin,
        &[b"ZADD", b"board:scores", b"10", b"ann", b"20", b"bob"],
    )
    .await;
    cmd(
        admin,
        &[
            b"XADD",
            b"events:log",
            b"*",
            b"kind",
            b"login",
            b"who",
            b"ada",
        ],
    )
    .await;
    cmd(admin, &[b"XADD", b"events:log", b"*", b"kind", b"logout"]).await;
    for i in 0..60 {
        let k = format!("cart:{i}");
        cmd(admin, &[b"SET", k.as_bytes(), b"items", b"EX", b"3600"]).await;
    }
    for i in 0..5 {
        let k = format!("lock:job:{i}");
        cmd(admin, &[b"SET", k.as_bytes(), b"1"]).await;
    }
    for _ in 0..30 {
        cmd(admin, &[b"GET", b"str:plain"]).await;
    }
    cmd(admin, &[b"SELECT", b"1"]).await;
    for i in 0..3 {
        let k = format!("other:{i}");
        cmd(admin, &[b"SET", k.as_bytes(), b"x"]).await;
    }
    cmd(admin, &[b"SELECT", b"0"]).await;
}

async fn servers() -> Vec<(&'static str, common::TestServer)> {
    vec![
        ("redis", common::redis().await),
        ("valkey", common::valkey().await),
    ]
}

#[tokio::test]
async fn phase_two_end_to_end_on_redis_and_valkey() {
    for (kind, server) in servers().await {
        one_server(kind, &server).await;
    }
}

async fn one_server(kind: &str, server: &common::TestServer) {
    let redis6 = kind == "redis" && common::redis_image().1.starts_with("6.");
    // The admin connection is its own: `SELECT` in `seed` stays on it.
    let mut admin = redis::Client::open(server.url())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    seed(&mut admin).await;
    // The same server as a stage profile, whose command budget (5,000 a second) keeps
    // an analysis running long enough to be stopped.
    let mut staged = profile("staged", server.port);
    staged.env = Env::Stage;
    let d = Dash::start(vec![profile("local", server.port), staged], kind).await;
    let mut rx = d.events().await;
    let s = d.connect("local").await;
    assert_eq!(s["analysis"]["status"], "none", "{kind}: {s}");
    assert_eq!(s["analysis"]["db"], 0, "{kind}");

    // The databases, and a switch to db 1 and back.
    let dbs = d.get("/api/dbs").await.json();
    let rows = dbs["rows"].as_array().unwrap();
    assert!(
        rows.iter().any(|r| r["db"] == 0 && r["current"] == true),
        "{kind}: {dbs}"
    );
    assert!(
        rows.iter().any(|r| r["db"] == 1 && r["keys"] == 3),
        "{kind}: {dbs}"
    );
    let r = d.post("/api/db", json!({ "db": 7 })).await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    // A tail running across the switch is subscribed again on db 1's live connection.
    let r = d
        .post("/api/pubsub/subscribe", json!({ "name": "news" }))
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    let tail = r.json()["tail"].clone();
    wait_for(&mut rx, "tail", |v| {
        v["tail"] == tail && v["kind"] == "ready"
    })
    .await;
    let r = d.post("/api/db", json!({ "db": 1 })).await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    wait_for(&mut rx, "tail", |v| {
        v["tail"] == tail && v["kind"] == "closed"
    })
    .await;
    let again = wait_for(&mut rx, "tail", |v| v["kind"] == "ready").await;
    assert_ne!(again["tail"], tail, "{kind}: a new tail: {again}");
    let s = d.get("/api/state").await.json();
    assert_eq!(s["tail"]["name"], "news", "{kind}: {s}");
    assert_eq!(s["tail"]["ready"], true, "{kind}: {s}");
    let r = d.send("DELETE", "/api/pubsub/subscribe", json!({})).await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    let s = d.state_until("db 1", |s| s["analysis"]["db"] == 1).await;
    assert_eq!(s["connection"]["name"], "local", "{kind}");
    d.ready().await;
    let b = d.get("/api/browse").await.json();
    assert_eq!(b["source"], "names", "{kind}: {b}");
    assert!(
        find_row(b["rows"].as_array().unwrap(), "other:{id}").is_some(),
        "{kind}: {b}"
    );
    let dbs = d.get("/api/dbs").await.json();
    assert_eq!(dbs["current"], 1, "{kind}: {dbs}");
    let r = d.post("/api/db", json!({ "db": 0 })).await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    d.state_until("db 0", |s| s["analysis"]["db"] == 0).await;
    d.ready().await;

    // Browse › Keys from names: every name is read in the first page.
    let b = d.get("/api/browse?sort=keys").await.json();
    assert_eq!(b["source"], "names", "{kind}: {b}");
    assert_eq!(b["names"]["complete"], true, "{kind}: {b}");
    let rows = b["rows"].as_array().unwrap();
    let cart = find_row(rows, "cart:{id}").unwrap_or_else(|| panic!("{kind}: {b}"));
    assert_eq!(cart["est_keys"], 60, "{kind}: {cart}");
    assert_eq!(
        cart["verdict"],
        Value::Null,
        "{kind}: nothing but names is known"
    );
    let r = d.post("/api/browse/more", json!({})).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    assert!(
        r.json()["error"]
            .as_str()
            .unwrap()
            .starts_with("Every key name is read")
    );

    // A pattern's keys.
    let id = cart["pattern"]["id"].as_str().unwrap();
    let l = d.get(&format!("/api/keys?pattern={id}")).await.json();
    assert_eq!(l["rows"].as_array().unwrap().len(), 60, "{kind}: {l}");
    assert_eq!(l["done"], true, "{kind}");
    let first = &l["rows"][0];
    assert_eq!(first["type"], "string", "{kind}: {first}");
    assert!(
        first["ttl_secs"].as_u64().is_some_and(|t| t > 3000),
        "{kind}: {first}"
    );
    let r = d.get(&format!("/api/keys/more?list={}", l["list"])).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);

    // Find key: plain text means "contains"; a glob is used as typed; too many stars
    // are refused in the TUI's words.
    let f = d.get("/api/find?q=user").await.json();
    let names: Vec<&str> = f["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["user:1:profile"], "{kind}: {f}");
    assert_eq!(f["glob"], "*user*", "{kind}");
    let f = d.get(&format!("/api/find?q={}", enc("evil*"))).await;
    let fj = f.json();
    assert_eq!(
        fj["rows"][0]["name"], "evil\\x1b[31mkey",
        "{kind}: {}",
        f.body
    );
    assert_eq!(fj["rows"][0]["copy"], Value::Null, "{kind}");
    assert_eq!(fj["rows"][0]["id"], hex(EVIL), "{kind}");
    assert!(
        !f.body.contains('\u{1b}'),
        "{kind}: a raw escape reached the page"
    );
    let r = d
        .get(&format!("/api/find?q={}", enc("*a*b*c*d*e*f*g*h*i*")))
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);

    // A pattern's keys over several pages: the first reply holds every row, each next
    // one only the rows it added, from `new_from` on.
    let mut p = redis::pipe();
    for i in 0..2_500 {
        p.cmd("SET").arg(format!("page:{i}")).arg("v").ignore();
    }
    let () = p.query_async(&mut admin).await.unwrap();
    let l = d
        .get(&format!("/api/keys?pattern={}", hex("page:{id}")))
        .await
        .json();
    let list = l["list"].clone();
    let rows_of = |v: &Value| -> Vec<String> {
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    let mut names: std::collections::HashSet<String> = rows_of(&l).into_iter().collect();
    assert_eq!(l["new_from"], 0, "{kind}");
    assert_eq!(names.len() as u64, l["loaded"].as_u64().unwrap(), "{kind}");
    assert!(names.len() <= 1_000, "{kind}: one page at most");
    assert_eq!(l["done"], false, "{kind}");
    let (mut loaded, mut pages, mut done) = (l["loaded"].as_u64().unwrap(), 1, false);
    while !done {
        let r = d.get(&format!("/api/keys/more?list={list}")).await;
        assert_eq!(r.status, 200, "{kind}: {}", r.body);
        let m = r.json();
        assert_eq!(m["new_from"], loaded, "{kind}");
        let rows = rows_of(&m);
        let now = m["loaded"].as_u64().unwrap();
        assert_eq!(rows.len() as u64, now - loaded, "{kind}: only the new rows");
        names.extend(rows);
        (loaded, done) = (now, m["done"] == true);
        pages += 1;
        assert!(pages < 20, "{kind}: the list ends");
    }
    assert_eq!((names.len(), loaded), (2_500, 2_500), "{kind}");
    assert!(pages >= 3, "{kind}: {pages} pages");
    let r = d.get(&format!("/api/keys/more?list={list}")).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    assert_eq!(
        r.json()["error"],
        "Every key of this list is loaded.",
        "{kind}"
    );

    // A value over many pages, up to the 10,000 items held.
    let items: Vec<String> = (0..10_500).map(|i| format!("e{i}")).collect();
    for chunk in items.chunks(2_000) {
        let mut c = redis::cmd("RPUSH");
        c.arg("long:list").arg(chunk);
        let _: i64 = c.query_async(&mut admin).await.unwrap();
    }
    let v = d
        .get(&format!("/api/value?key={}", hex("long:list")))
        .await
        .json();
    assert_eq!(
        v["items"].as_array().unwrap().len(),
        100,
        "{kind}: one page"
    );
    assert_eq!(v["more"], true, "{kind}");
    let view = v["view"].clone();
    let mut last = v;
    let mut reads = 1;
    while last["capped"] != true {
        let r = d.get(&format!("/api/value/more?view={view}")).await;
        assert_eq!(r.status, 200, "{kind}: {}", r.body);
        last = r.json();
        reads += 1;
        assert!(reads <= 101, "{kind}: the value stops at the cap");
    }
    assert_eq!(last["loaded"], 10_000, "{kind}");
    assert_eq!(last["more"], false, "{kind}");
    assert_eq!(
        last["notice"], "At most 10,000 items are held, so no more are loaded.",
        "{kind}"
    );
    let held = last["items"].as_array().unwrap();
    assert_eq!(held.len(), 10_000, "{kind}");
    assert_eq!(
        (held[0]["value"].clone(), held[9_999]["value"].clone()),
        (json!("e0"), json!("e9999")),
        "{kind}: in order, none twice"
    );
    let again = d.get(&format!("/api/value/more?view={view}")).await.json();
    assert_eq!(again["loaded"], 10_000, "{kind}: nothing more is read");

    // A key's value, of every type.
    let open = |key: &str| format!("/api/value?key={}", hex(key));
    let v = d.get(&open("str:plain")).await.json();
    assert_eq!(v["kind"], "string", "{kind}: {v}");
    assert_eq!(v["value"]["lines"][0], "hello world", "{kind}: {v}");
    assert_eq!(v["value"]["copy"], "hello world", "{kind}");
    let v = d.get(&open("str:json")).await.json();
    assert_eq!(v["value"]["format"], "JSON", "{kind}: {v}");
    let r = d.get(&open("str:bin")).await;
    assert!(!r.body.contains('\u{1b}'), "{kind}: {}", r.body);
    let v = r.json();
    // Binary bytes are detected as hex, and the hex is what is copied.
    assert_eq!(v["value"]["detected"], "hex", "{kind}: {v}");
    assert_eq!(v["value"]["copy"], "0001ff62696e1b", "{kind}: {v}");
    let r = d
        .get(&format!(
            "/api/value/item?view={}&index=0&format=text",
            v["view"]
        ))
        .await;
    assert!(!r.body.contains('\u{1b}'), "{kind}: {}", r.body);
    assert_eq!(r.json()["shown"]["format"], "text", "{kind}: {}", r.body);
    let v = d.get(&open("user:1:profile")).await.json();
    assert_eq!(v["kind"], "hash", "{kind}: {v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 2, "{kind}: {v}");
    // Field TTLs (HTTL) exist from Redis 7.4; not on 6.2 nor on Valkey 8.1.
    assert_eq!(v["field_ttls"], kind == "redis" && !redis6, "{kind}: {v}");
    let item = d
        .get(&format!("/api/value/item?view={}&index=0", v["view"]))
        .await
        .json();
    assert!(item["shown"]["lines"][0].is_string(), "{kind}: {item}");
    let v = d.get(&open("queue:jobs")).await.json();
    assert_eq!(v["kind"], "list", "{kind}: {v}");
    assert_eq!(v["items"][2]["value"], "j3", "{kind}: {v}");
    assert_eq!(v["notice"], "All 3 items are loaded.", "{kind}: {v}");
    let more = d
        .get(&format!("/api/value/more?view={}", v["view"]))
        .await
        .json();
    assert_eq!(more["loaded"], 3, "{kind}: {more}");
    let v = d.get(&open("tags:1")).await.json();
    assert_eq!(v["kind"], "set", "{kind}: {v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 2, "{kind}");
    let v = d.get(&open("board:scores")).await.json();
    assert_eq!(v["kind"], "zset", "{kind}: {v}");
    assert_eq!(v["items"][0]["name"], "ann", "{kind}: {v}");
    assert_eq!(v["items"][0]["value"], "10", "{kind}: {v}");
    let v = d.get(&open("events:log")).await.json();
    assert_eq!(v["kind"], "stream", "{kind}: {v}");
    assert_eq!(
        v["meta"]["type"], "stream",
        "{kind}: the first read's TYPE: {v}"
    );
    assert_eq!(
        v["items"][0]["fields"][0]["value"], "logout",
        "{kind}: newest first: {v}"
    );
    let r = d
        .get(&format!("/api/value/item?view={}&index=1", v["view"]))
        .await;
    assert_eq!(r.status, 422, "{kind}: a stream entry opens one field");
    let field = d
        .get(&format!(
            "/api/value/item?view={}&index=1&field=1",
            v["view"]
        ))
        .await
        .json();
    assert_eq!(field["shown"]["lines"][0], "ada", "{kind}: {field}");
    let r = d.get(&open(EVIL)).await;
    assert!(!r.body.contains('\u{1b}'), "{kind}");
    assert_eq!(r.json()["key"]["name"], "evil\\x1b[31mkey", "{kind}");
    let v = d.get(&open("no:such:key")).await.json();
    assert_eq!(v["gone"], true, "{kind}: {v}");

    // An analysis, over the event stream.
    let a = d.get("/api/analysis").await.json();
    assert_eq!(a["estimates"][0]["depth"], "quick", "{kind}: {a}");
    let done = analyze(&d, &mut rx).await;
    assert!(
        done["sampled_keys"].as_u64().unwrap() >= 70,
        "{kind}: {done}"
    );
    assert_eq!(done["complete"], true, "{kind}: {done}");
    let s = d.get("/api/state").await.json();
    assert_eq!(s["analysis"]["status"], "done", "{kind}: {s}");
    assert!(
        s["analysis"]["report"]["age_secs"].as_i64().is_some(),
        "{kind}: {s}"
    );
    let b = d.get("/api/browse").await.json();
    assert_eq!(b["source"], "analysis", "{kind}: {b}");
    let lock = find_row(b["rows"].as_array().unwrap(), "lock:job:{id}")
        .unwrap_or_else(|| panic!("{kind}: {b}"));
    assert_eq!(lock["verdict"], "leak?", "{kind}: {lock}");
    let o = d.get("/api/overview").await.json();
    assert!(o["analysis"]["findings"].is_array(), "{kind}: {o}");
    let big = d.get("/api/analysis/big_keys").await.json();
    assert!(!big["rows"].as_array().unwrap().is_empty(), "{kind}: {big}");
    assert_eq!(big["facts_read"], true, "{kind}: {big}");
    let ex = d.get("/api/analysis/expiry").await.json();
    assert_eq!(
        ex["rows"][0]["status"], "persistent · stable",
        "{kind}: {ex}"
    );
    assert!(ex["freed"]["total"].as_u64().is_some(), "{kind}: {ex}");
    let ch = d.get("/api/analysis/changes").await.json();
    assert_eq!(ch["status"], "none", "{kind}: {ch}");
    let hot = d.get("/api/analysis/hot_keys").await.json();
    assert!(!hot["rows"].as_array().unwrap().is_empty(), "{kind}: {hot}");
    let nodes = d.get("/api/analysis/nodes").await.json();
    assert!(nodes["off"].is_string(), "{kind}: {nodes}");
    let slots = d.get("/api/analysis/slots").await.json();
    assert!(slots["off"].is_string(), "{kind}: {slots}");

    // Snapshots: save, analyze again, the comparison, save again, list, compare.
    let r = d.post("/api/snapshots", json!({})).await;
    assert_eq!(r.status, 201, "{kind}: {}", r.body);
    let first = r.json()["file"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    cmd(&mut admin, &[b"SET", b"cart:new", b"items"]).await;
    analyze(&d, &mut rx).await;
    let ch = d.get("/api/analysis/changes").await.json();
    assert_eq!(ch["status"], "diff", "{kind}: {ch}");
    // The key added since the snapshot shows: cart:{id} grew (or cart:new is new).
    let grew = ch["rows"].as_array().unwrap().iter().any(|r| {
        (r["pattern"]["name"] == "cart:{id}"
            && r["keys_after"].as_u64() > r["keys_before"].as_u64()
            && r["change"]["delta_keys"].as_i64() > Some(0))
            || (r["pattern"]["name"] == "cart:new" && r["change"]["cause"] == "new")
    });
    assert!(grew, "{kind}: {ch}");
    let r = d.post("/api/snapshots", json!({})).await;
    assert_eq!(r.status, 201, "{kind}: {}", r.body);
    let second = r.json()["file"].as_str().unwrap().to_string();
    assert_ne!(first, second);
    let l = d.get("/api/snapshots").await.json();
    assert_eq!(l["snapshots"].as_array().unwrap().len(), 2, "{kind}: {l}");
    assert_eq!(l["saved"], true, "{kind}");
    let cmp = d
        .get(&format!("/api/snapshots/compare?a={first}&b={second}"))
        .await
        .json();
    assert!(!cmp["rows"].as_array().unwrap().is_empty(), "{kind}: {cmp}");
    // The two snapshots are the TUI's files (a user's credentials in none of them is
    // checked by the limited user's test, whose profile has some).
    let files: Vec<PathBuf> = walk(&d.snapshots);
    assert_eq!(files.len(), 2, "{kind}: {files:?}");

    // An analysis stopped early keeps what it sampled, marked stopped, and is not
    // saved. Enough keys, and the stage profile's budget, that it can't finish first.
    for start in (0..20_000).step_by(5_000) {
        let mut p = redis::pipe();
        for i in start..start + 5_000 {
            p.cmd("SET").arg(format!("bulk:{i}")).arg("v").ignore();
        }
        let () = p.query_async(&mut admin).await.unwrap();
    }
    d.connect("staged").await;
    let r = d.post("/api/analysis", json!({ "depth": "deep" })).await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    let run = r.json()["run"].clone();
    let again = d.post("/api/analysis", json!({})).await;
    assert_eq!(again.status, 409, "{kind}: {}", again.body);
    wait_for(&mut rx, "progress", |v| {
        v["run"] == run && v["kind"] == "sampling" && v["sampled"].as_u64() > Some(0)
    })
    .await;
    let stop = d.send("DELETE", "/api/analysis", json!({})).await;
    assert_eq!(stop.status, 200, "{kind}: {}", stop.body);
    assert_eq!(
        stop.json()["text"],
        "Stopping the analysis. What it sampled so far is kept.",
        "{kind}"
    );
    let done = wait_for(&mut rx, "progress", |v| {
        v["run"] == run && v["kind"] == "done"
    })
    .await;
    assert_eq!(done["stopped"], true, "{kind}: {done}");
    assert!(
        done["sampled_keys"]
            .as_u64()
            .is_some_and(|n| n > 0 && n < 22_000),
        "{kind}: {done}"
    );
    d.ready().await;
    let stopped_words = "This analysis was stopped early, so its results are partial and are not saved. Analyze again to save one.";
    let r = d.post("/api/snapshots", json!({})).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    assert_eq!(r.json()["error"], stopped_words, "{kind}");
    let l = d.get("/api/snapshots").await.json();
    assert_eq!(l["can_save"], false, "{kind}: {l}");
    assert_eq!(l["save_off"], stopped_words, "{kind}: {l}");
    assert_eq!(walk(&d.snapshots).len(), 2, "{kind}: nothing was written");
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.extension().is_some_and(|x| x == "json") {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn a_cluster_browses_and_analyzes_shard_by_shard() {
    for valkey in [false, true] {
        let kind = if valkey { "valkey" } else { "redis" };
        let c = common::cluster::cluster(valkey, false).await;
        let keys: Vec<(String, String)> = (0..90)
            .map(|i| (format!("item:{i}"), "v".to_string()))
            .chain((0..30).map(|i| (format!("{{user{i}}}:name"), "n".to_string())))
            .collect();
        c.seed(&keys).await;
        let d = Dash::start(
            vec![profile("cl", c.seed_port())],
            &format!("cluster-{kind}"),
        )
        .await;
        let mut rx = d.events().await;
        let s = d.connect("cl").await;
        assert_eq!(s["nodes"].as_array().unwrap().len(), 3, "{kind}: {s}");
        let dbs = d.get("/api/dbs").await.json();
        assert_eq!(
            dbs["off"], "A cluster only has database 0.",
            "{kind}: {dbs}"
        );
        let b = d.get("/api/browse").await.json();
        assert_eq!(b["source"], "names", "{kind}: {b}");
        assert_eq!(b["names"]["nodes_done"], json!([3, 3]), "{kind}: {b}");
        let item = find_row(b["rows"].as_array().unwrap(), "item:{id}")
            .unwrap_or_else(|| panic!("{kind}: {b}"));
        assert_eq!(item["est_keys"], 90, "{kind}: {item}");
        let id = item["pattern"]["id"].as_str().unwrap();
        let l = d.get(&format!("/api/keys?pattern={id}")).await.json();
        assert_eq!(l["rows"].as_array().unwrap().len(), 90, "{kind}: {l}");
        assert_eq!(l["cluster"]["nodes_done"], 3, "{kind}: {l}");
        let f = d.get("/api/find?q=item:4").await.json();
        assert!(f["rows"].as_array().unwrap().len() >= 11, "{kind}: {f}");
        let v = d
            .get(&format!("/api/value?key={}", hex("item:42")))
            .await
            .json();
        assert_eq!(v["value"]["lines"][0], "v", "{kind}: {v}");
        assert_eq!(v["cluster"], true, "{kind}");
        let done = analyze(&d, &mut rx).await;
        assert_eq!(done["sampled_keys"], 120, "{kind}: {done}");
        let nodes = d.get("/api/analysis/nodes").await.json();
        assert_eq!(
            nodes["rows"].as_array().unwrap().len(),
            3,
            "{kind}: {nodes}"
        );
        let all = d.get("/api/browse").await.json();
        let b = d.get("/api/browse?node=0").await.json();
        assert_eq!(b["node"], 0, "{kind}: {b}");
        assert_eq!(b["nodes"].as_array().unwrap().len(), 3, "{kind}");
        // The rows are node 0's alone: fewer keys than the whole cluster's, but some.
        let keys = |v: &Value| -> u64 {
            v["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["est_keys"].as_u64().unwrap())
                .sum()
        };
        let (mine, every) = (keys(&b), keys(&all));
        assert!(mine > 0 && mine < every, "{kind}: {mine} of {every}");
        assert_eq!(every, 120, "{kind}: {all}");
        let r = d.get("/api/browse?node=9").await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        let slots = d.get("/api/analysis/slots").await.json();
        if valkey {
            // Valkey 8.0 and later have CLUSTER SLOT-STATS.
            assert!(
                !slots["ranges"].as_array().unwrap().is_empty(),
                "{kind}: {slots}"
            );
            assert!(
                !slots["hottest"].as_array().unwrap().is_empty(),
                "{kind}: {slots}"
            );
        } else {
            // Redis before 8.2 has not.
            let off = slots["off"]
                .as_str()
                .unwrap_or_else(|| panic!("{kind}: {slots}"));
            assert!(off.starts_with("CLUSTER SLOT-STATS needs"), "{kind}: {off}");
        }
        let big = d.get("/api/analysis/big_keys").await.json();
        assert_eq!(big["facts_read"], true, "{kind}: {big}");
        // On a cluster each row names its node by index, as every other route does, and
        // gives its address apart.
        for row in big["rows"].as_array().unwrap() {
            assert!(row["node"].is_u64(), "{kind}: {row}");
            assert!(
                row["endpoint"].as_str().is_some_and(|e| e.contains(':')),
                "{kind}: {row}"
            );
        }
        let r = d.post("/api/snapshots", json!({})).await;
        assert_eq!(r.status, 201, "{kind}: {}", r.body);
    }
}

// -- a limited ACL user, with a name and a password ------------------------------

/// The users' names and passwords: made up, long enough that no other text holds them.
const LIM_USER: &str = "lens_limited_q7z";
const LIM_PASS: &str = "Pw-limited-9q2x-secret";
const NOSCAN_USER: &str = "lens_noscan_k4w";
const NOSCAN_PASS: &str = "Pw-noscan-3m8v-secret";

/// A profile signing in as `user` with `pass`, both in `store` only (the keychain's
/// stand-in): the profile itself holds neither.
fn signed_in(name: &str, port: u16, user: &str, pass: &str, store: &MemoryStore) -> Profile {
    let mut p = profile(name, port);
    p.auth = Auth::Keychain;
    store
        .set(
            &p.secret_ref(),
            &Credentials {
                username: Some(user.into()),
                password: secrecy::SecretString::from(pass.to_string()),
            },
        )
        .unwrap();
    p
}

/// Every text a reply shows the user (`off`, `error`, `notice`, notes, refusals, why a
/// panel is empty) is redis-lens's own: no TUI key, screen or flag, no server code.
fn own_words(kind: &str, what: &str, v: &Value) {
    const TEXTS: &[&str] = &[
        "off",
        "error",
        "notice",
        "notes",
        "refused",
        "empty",
        "why",
        "text",
        "message",
        "field_ttls_off",
    ];
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if TEXTS.contains(&k.as_str()) {
                    let texts: Vec<&str> = match x {
                        Value::String(s) => vec![s.as_str()],
                        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                        _ => Vec::new(),
                    };
                    for t in texts {
                        assert_eq!(wording::banned(t), None, "{kind} {what}: {t}");
                        assert_eq!(wording::server_free(t), t, "{kind} {what}: {t}");
                        for code in ["NOPERM", "ERR ", "NOAUTH", "WRONGPASS"] {
                            assert!(!t.contains(code), "{kind} {what}: {t}");
                        }
                    }
                }
                own_words(kind, what, x);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| own_words(kind, what, x)),
        _ => {}
    }
}

/// S3 (rules 1 and 3): a user without MEMORY, OBJECT and LRANGE, and one without SCAN.
/// Each feature they can't use is off or empty in redis-lens's own words, nothing else
/// stops; their names and passwords reach no reply, no event and no snapshot file.
#[tokio::test]
async fn a_limited_user_degrades_in_own_words_and_its_credentials_go_nowhere() {
    for valkey in [false, true] {
        let kind = if valkey { "valkey" } else { "redis" };
        let lim_pass = format!(">{LIM_PASS}");
        let noscan_pass = format!(">{NOSCAN_PASS}");
        let server = common::server_with(
            valkey,
            &[
                "--user",
                LIM_USER,
                "on",
                &lim_pass,
                "~*",
                "+@all",
                "-memory",
                "-object",
                "-lrange",
                "--user",
                NOSCAN_USER,
                "on",
                &noscan_pass,
                "~*",
                "+@all",
                "-scan",
            ],
        )
        .await;
        let mut admin = server.admin().await;
        cmd(
            &mut admin,
            &[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lfu"],
        )
        .await;
        cmd(&mut admin, &[b"RPUSH", b"queue:jobs", b"j1", b"j2"]).await;
        cmd(&mut admin, &[b"SET", b"str:plain", b"hello"]).await;
        for i in 0..30 {
            let k = format!("cart:{i}");
            cmd(&mut admin, &[b"SET", k.as_bytes(), b"items"]).await;
        }
        let store = Arc::new(MemoryStore::default());
        let lim = signed_in("limited", server.port, LIM_USER, LIM_PASS, &store);
        let noscan = signed_in("noscan", server.port, NOSCAN_USER, NOSCAN_PASS, &store);
        let d = Dash::start_with(vec![lim, noscan], &format!("acl-{kind}"), store).await;
        let mut rx = d.events().await;
        // Neither the config file holds a credential.
        let config =
            std::fs::read_to_string(d.snapshots.parent().unwrap().join("config.toml")).unwrap();
        for secret in [LIM_USER, LIM_PASS, NOSCAN_USER, NOSCAN_PASS] {
            assert!(!config.contains(secret), "{kind}: the config file holds it");
        }

        // Without MEMORY, OBJECT and LRANGE.
        d.connect("limited").await;
        let b = d.get("/api/browse").await.json();
        assert_eq!(b["source"], "names", "{kind}: {b}");
        own_words(kind, "browse", &b);
        let l = d
            .get(&format!("/api/keys?pattern={}", hex("cart:{id}")))
            .await
            .json();
        assert_eq!(l["rows"].as_array().unwrap().len(), 30, "{kind}: {l}");
        assert_eq!(l["sizes"], false, "{kind}: no MEMORY USAGE: {l}");
        own_words(kind, "keys", &l);
        let v = d
            .get(&format!("/api/value?key={}", hex("queue:jobs")))
            .await
            .json();
        let why = v["error"].as_str().unwrap_or_else(|| panic!("{kind}: {v}"));
        assert!(why.contains("refused LRANGE"), "{kind}: {why}");
        assert_eq!(v["items"], json!([]), "{kind}: {v}");
        own_words(kind, "value", &v);
        let v = d
            .get(&format!("/api/value?key={}", hex("str:plain")))
            .await
            .json();
        assert_eq!(
            v["value"]["lines"][0], "hello",
            "{kind}: the rest is read: {v}"
        );
        let done = analyze(&d, &mut rx).await;
        assert_eq!(done["sampled_keys"], 32, "{kind}: {done}");
        let big = d.get("/api/analysis/big_keys").await.json();
        assert_eq!(big["report"]["sizes_available"], false, "{kind}: {big}");
        assert_eq!(big["rows"], json!([]), "{kind}: {big}");
        assert!(
            big["empty"]
                .as_str()
                .is_some_and(|t| t.contains("MEMORY USAGE is not allowed")),
            "{kind}: {big}"
        );
        own_words(kind, "big keys", &big);
        let hot = d.get("/api/analysis/hot_keys").await.json();
        assert_eq!(
            hot["off"], "OBJECT FREQ was denied for this user.",
            "{kind}: {hot}"
        );
        own_words(kind, "hot keys", &hot);
        for panel in ["expiry", "changes", "nodes", "slots"] {
            let p = d.get(&format!("/api/analysis/{panel}")).await.json();
            own_words(kind, panel, &p);
        }
        let o = d.get("/api/overview").await.json();
        own_words(kind, "overview", &o);
        let r = d.post("/api/snapshots", json!({})).await;
        assert_eq!(r.status, 201, "{kind}: {}", r.body);

        // Without SCAN: no names, no key list, no find, no analysis; the rest goes on.
        d.connect("noscan").await;
        let b = d.get("/api/browse").await.json();
        assert_eq!(
            b["names"]["error"],
            "The server refused SCAN for this user, so no key names can be read.",
            "{kind}: {b}"
        );
        own_words(kind, "browse", &b);
        let l = d
            .get(&format!("/api/keys?pattern={}", hex("cart:{id}")))
            .await
            .json();
        assert_eq!(
            l["error"], "The server refused SCAN for this user, so the keys can't be listed.",
            "{kind}: {l}"
        );
        own_words(kind, "keys", &l);
        let f = d.get("/api/find?q=cart").await.json();
        assert_eq!(
            f["error"], "The server refused SCAN for this user, so keys can't be searched.",
            "{kind}: {f}"
        );
        let v = d
            .get(&format!("/api/value?key={}", hex("queue:jobs")))
            .await
            .json();
        assert_eq!(v["items"][1]["value"], "j2", "{kind}: {v}");
        let r = d.post("/api/analysis", json!({ "depth": "quick" })).await;
        assert_eq!(r.status, 202, "{kind}: {}", r.body);
        let run = r.json()["run"].clone();
        let end = wait_for(&mut rx, "progress", |v| {
            v["run"] == run && matches!(v["kind"].as_str(), Some("failed" | "done"))
        })
        .await;
        // It failed, not the connection, and it says so in redis-lens's words.
        assert_eq!(end["kind"], "failed", "{kind}: {end}");
        assert_eq!(end["error"], wording::ANALYSIS_REFUSED, "{kind}: {end}");
        own_words(kind, "analysis", &end);
        let s = d.get("/api/state").await.json();
        assert_eq!(
            s["analysis"]["error"],
            wording::ANALYSIS_REFUSED,
            "{kind}: {s}"
        );
        let s = d.ready().await;
        own_words(kind, "state", &s);

        // A snapshot of the limited user's analysis, every reply and every event: no
        // user name and no password anywhere.
        let files = walk(&d.snapshots);
        assert_eq!(files.len(), 1, "{kind}: {files:?}");
        let mut texts: Vec<String> = files
            .iter()
            .map(|f| std::fs::read_to_string(f).unwrap())
            .collect();
        texts.extend(d.seen());
        assert!(texts.len() > 30, "{kind}: {} texts", texts.len());
        for t in &texts {
            for secret in [LIM_USER, LIM_PASS, NOSCAN_USER, NOSCAN_PASS] {
                assert!(!t.contains(secret), "{kind}: a credential reached {t}");
            }
        }
    }
}
