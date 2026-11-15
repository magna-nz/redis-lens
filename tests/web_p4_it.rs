//! The web dashboard's phase 4 over real HTTP against real Redis and Valkey servers and
//! a cluster: Browse › Streams (the streams an analysis found, their groups, consumers
//! and pending entries, the entries newest first, and the three stream writes, each
//! read back through a separate admin connection), Browse › Search (off without the
//! module; on Redis 8 and the Valkey bundle an index created, queried, explained and
//! dropped through the dashboard), and Hot keys ranked from a Monitor run that saw the
//! traffic this test made. A read-only profile sends no write (the server's own `INFO
//! commandstats` says so), and a user without `XINFO` sees the panel off in
//! redis-lens's words, its name and password in no reply or event.
//!
//! The config file and the snapshot directory are temporary ones, and credentials are
//! in a `MemoryStore`: the user's own are never touched.
//!
//! With `REDIS_LENS_P4_CAPTURE=<file>` every reply (and the `monitor` events but its
//! lines) is appended to that file as JSON lines, for the examples in
//! `docs/dashboard-api.md`.
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
const CAPTURE: &str = "REDIS_LENS_P4_CAPTURE";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-web-p4-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

async fn q<T: redis::FromRedisValue>(conn: &mut MultiplexedConnection, args: &[&str]) -> T {
    let mut c = redis::cmd(args[0]);
    for a in &args[1..] {
        c.arg(*a);
    }
    c.query_async(conn).await.expect("query")
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
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
    dir: PathBuf,
    label: String,
    /// Every reply body and event data line, for the checks that something is nowhere.
    seen: Arc<Mutex<Vec<String>>>,
}

impl Dash {
    async fn start(profiles: Vec<Profile>, tag: &str) -> Self {
        Self::start_with(profiles, tag, Arc::new(MemoryStore::default())).await
    }

    async fn start_with(profiles: Vec<Profile>, tag: &str, store: Arc<MemoryStore>) -> Self {
        let dir = temp_dir(tag);
        let config = dir.join("config.toml");
        let mut c = Config::default();
        for p in profiles {
            c.add(p, false).unwrap();
        }
        c.save(&config).unwrap();
        let d = web::start_with_snapshots(
            Options {
                port: 0,
                config: config.clone(),
            },
            store,
            Some(dir.join("snapshots")),
        )
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
            dir,
            label: tag.to_string(),
            seen: Arc::default(),
        }
    }

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

    /// [`Dash::post`], a yes-or-no question answered yes (a delete asks on every
    /// environment): the answer's reply, or the first reply when it asked nothing.
    async fn post_yes(&self, path: &str, body: Value) -> Res {
        let r = self.post(path, body).await;
        if r.status != 409 {
            return r;
        }
        let c = r.json()["confirm"].clone();
        if c["kind"] != "ask" {
            return r;
        }
        self.post(
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({}),
        )
        .await
    }

    fn record(&self, method: &str, path: &str, body: Option<Value>, r: &Res) {
        self.seen.lock().unwrap().push(r.body.clone());
        let reply: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
        capture(
            &self.label,
            json!({ "method": method, "path": path, "body": body, "status": r.status, "reply": reply }),
        );
    }

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
                        if name == "monitor" && v["kind"] != "lines" {
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
        let _ = std::fs::remove_dir_all(&self.dir);
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

/// Every text a reply shows the user is redis-lens's own: no TUI key, screen or flag,
/// no server code.
fn own_words(what: &str, v: &Value) {
    const TEXTS: &[&str] = &["error", "message", "text", "lines", "summary", "notice"];
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
                        assert_eq!(wording::banned(t), None, "{what}: {t}");
                        for code in ["NOPERM", "ERR ", "NOAUTH", "WRONGPASS", "WRONGTYPE"] {
                            assert!(!t.contains(code), "{what}: {t}");
                        }
                    }
                }
                own_words(what, x);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| own_words(what, x)),
        _ => {}
    }
}

async fn servers() -> Vec<(&'static str, common::TestServer)> {
    vec![
        ("redis", common::redis().await),
        ("valkey", common::valkey().await),
    ]
}

fn redis6(kind: &str) -> bool {
    kind == "redis" && common::redis_image().1.starts_with("6.")
}

/// The stream and Search writes, as `INFO commandstats` names them.
const WRITES: &[&str] = &[
    "xack",
    "xautoclaim",
    "xclaim",
    "xtrim",
    "xadd",
    "xdel",
    "xgroup",
    "ft.create",
    "ft.dropindex",
    "ft._dropindexifx",
];

fn writes_in(info: &str) -> Vec<String> {
    common::cluster::parse_commandstats(info)
        .into_iter()
        .filter(|(name, (calls, _))| *calls > 0 && WRITES.contains(&name.as_str()))
        .map(|(n, _)| n)
        .collect()
}

async fn analyze(d: &Dash, rx: &mut mpsc::UnboundedReceiver<(String, Value)>) -> Value {
    let r = d.post("/api/analysis", json!({ "depth": "quick" })).await;
    assert_eq!(r.status, 202, "{}", r.body);
    let run = r.json()["run"].clone();
    let done = wait_for(rx, "progress", |v| v["run"] == run && v["kind"] == "done").await;
    d.ready().await;
    done
}

/// A write that went through: the reply's `reply`.
fn sent(r: &Res, what: &str) -> Value {
    assert_eq!(r.status, 200, "{what}: {}", r.body);
    let v = r.json();
    assert_eq!(v["sent"], true, "{what}: {v}");
    own_words(what, &v);
    v["reply"].clone()
}

fn row<'a>(v: &'a Value, key: &str) -> &'a Value {
    v["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("no rows: {v}"))
        .iter()
        .find(|r| r["key"]["name"] == key)
        .unwrap_or_else(|| panic!("no row {key}: {v}"))
}

/// `p4s:orders`: five entries, the group `billing` with `w1` holding three and `w2` one;
/// `p4s:audit`: two entries, no group; `p4s:big`: 250 entries.
async fn seed_streams(admin: &mut MultiplexedConnection) -> Vec<String> {
    let mut ids = Vec::new();
    for i in 0..5 {
        let id: String = q(
            admin,
            &[
                "XADD",
                "p4s:orders",
                "*",
                "n",
                &i.to_string(),
                "kind",
                "order",
            ],
        )
        .await;
        ids.push(id);
    }
    cmd(admin, &["XGROUP", "CREATE", "p4s:orders", "billing", "0"]).await;
    cmd(
        admin,
        &[
            "XREADGROUP",
            "GROUP",
            "billing",
            "w1",
            "COUNT",
            "3",
            "STREAMS",
            "p4s:orders",
            ">",
        ],
    )
    .await;
    cmd(
        admin,
        &[
            "XREADGROUP",
            "GROUP",
            "billing",
            "w2",
            "COUNT",
            "1",
            "STREAMS",
            "p4s:orders",
            ">",
        ],
    )
    .await;
    for i in 0..2 {
        cmd(admin, &["XADD", "p4s:audit", "*", "who", &format!("u{i}")]).await;
    }
    for i in 0..250 {
        cmd(admin, &["XADD", "p4s:big", "*", "i", &i.to_string()]).await;
    }
    ids
}

async fn pending_count(admin: &mut MultiplexedConnection, consumer: Option<&str>) -> usize {
    let mut args = vec!["XPENDING", "p4s:orders", "billing", "-", "+", "100"];
    if let Some(c) = consumer {
        args.push(c);
    }
    let v: Vec<redis::Value> = q(admin, &args).await;
    v.len()
}

// -- Browse › Streams ------------------------------------------------------------------

#[tokio::test]
async fn streams_read_and_write_exactly_on_redis_and_valkey() {
    for (kind, server) in servers().await {
        streams(kind, &server).await;
    }
}

async fn streams(kind: &str, server: &common::TestServer) {
    let mut admin = server.admin().await;
    let ids = seed_streams(&mut admin).await;
    let d = Dash::start(
        vec![
            profile("dev", server.port, Env::Other, None),
            profile("ro", server.port, Env::Local, Some(true)),
        ],
        &format!("streams-{kind}"),
    )
    .await;
    let mut rx = d.events().await;
    d.connect("dev").await;

    // Before an analysis: the TUI's words, and nothing to list.
    let v = d.get("/api/streams").await;
    assert_eq!(v.status, 200, "{kind}: {}", v.body);
    let v = v.json();
    assert_eq!(v["status"], "no_report", "{kind}: {v}");
    assert_eq!(
        v["text"],
        "Streams are found by an analysis: it samples the keys and lists the streams among them.",
        "{kind}"
    );
    analyze(&d, &mut rx).await;

    // Each stream, its groups and consumers.
    let v = d.get("/api/streams?refresh=1").await.json();
    assert_eq!(v["status"], "ready", "{kind}: {v}");
    own_words(kind, &v);
    let orders = row(&v, "p4s:orders");
    assert_eq!(orders["length"], 5, "{kind}: {orders}");
    assert_eq!(orders["pending"], 4, "{kind}: {orders}");
    assert_eq!(orders["first_id"], ids[0].as_str(), "{kind}");
    assert_eq!(orders["last_id"], ids[4].as_str(), "{kind}");
    let g = &orders["groups"][0];
    assert_eq!(g["name"]["name"], "billing", "{kind}: {g}");
    assert_eq!(g["pending"], 4, "{kind}");
    let readers = g["readers"].as_array().unwrap();
    let pend = |name: &str| {
        readers
            .iter()
            .find(|c| c["name"]["name"] == name)
            .unwrap_or_else(|| panic!("{kind}: no consumer {name}: {g}"))["pending"]
            .as_u64()
            .unwrap()
    };
    assert_eq!((pend("w1"), pend("w2")), (3, 1), "{kind}");
    if redis6(kind) {
        // No lag before Redis 7.0: left out, with why.
        assert_eq!(g["lag"], Value::Null, "{kind}: {g}");
        assert!(
            orders["left_out"]
                .as_str()
                .unwrap()
                .contains("lag: Redis 7.0 or later"),
            "{kind}: {orders}"
        );
    } else {
        assert_eq!(g["lag"], 1, "{kind}: {g}");
    }
    let audit = row(&v, "p4s:audit");
    assert_eq!(audit["verdict"], "no readers", "{kind}: {audit}");
    assert_eq!(
        audit["groups_text"], "No consumer groups: nothing reads this stream.",
        "{kind}"
    );

    // The pending entries of the group, then of one consumer: a read.
    let key = hex("p4s:orders");
    let group = hex("billing");
    let p = d
        .get(&format!("/api/streams/pending?key={key}&group={group}"))
        .await;
    assert_eq!(p.status, 200, "{kind}: {}", p.body);
    let p = p.json();
    assert_eq!(
        p["pending"]["entries"].as_array().unwrap().len(),
        4,
        "{kind}: {p}"
    );
    assert_eq!(p["pending"]["entries"][0]["id"], ids[0].as_str(), "{kind}");
    let p = d
        .get(&format!(
            "/api/streams/pending?key={key}&group={group}&consumer={}",
            hex("w1")
        ))
        .await
        .json();
    let w1: Vec<&str> = p["pending"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        w1,
        [ids[0].as_str(), ids[1].as_str(), ids[2].as_str()],
        "{kind}"
    );

    // The entries: newest first, read again by opening again.
    let e = d
        .get(&format!("/api/value?key={key}&type=stream"))
        .await
        .json();
    assert_eq!(e["items"][0]["name"], ids[4].as_str(), "{kind}: {e}");
    assert_eq!(e["items"][4]["name"], ids[0].as_str(), "{kind}");
    cmd(&mut admin, &["XADD", "p4s:orders", "*", "n", "late"]).await;
    let again = d
        .get(&format!("/api/value?key={key}&type=stream"))
        .await
        .json();
    assert_ne!(
        again["items"][0]["name"],
        ids[4].as_str(),
        "{kind}: read again"
    );

    // Read-only: every stream write refused with the gate's words, none sent.
    d.connect("ro").await;
    analyze(&d, &mut rx).await;
    let v = d.get("/api/streams").await.json();
    assert!(v["writes"].is_string(), "{kind}: {v}");
    d.get(&format!("/api/streams/pending?key={key}&group={group}"))
        .await;
    cmd(&mut admin, &["CONFIG", "RESETSTAT"]).await;
    for (path, body) in [
        (
            "/api/streams/ack",
            json!({ "key": key, "group": group, "id": ids[0] }),
        ),
        (
            "/api/streams/claim",
            json!({ "key": key, "group": group, "to": "w2", "min_idle_ms": "0", "count": "10" }),
        ),
        (
            "/api/streams/trim",
            json!({ "key": hex("p4s:big"), "by": "MAXLEN", "value": "10" }),
        ),
    ] {
        let r = d.post(path, body).await;
        assert_eq!(r.status, 422, "{kind}: {path}: {}", r.body);
        own_words(kind, &r.json());
    }
    let info: String = q(&mut admin, &["INFO", "commandstats"]).await;
    assert_eq!(writes_in(&info), Vec::<String>::new(), "{kind}: {info}");
    assert_eq!(pending_count(&mut admin, None).await, 4, "{kind}");

    // Writable: each write changes exactly what it should.
    d.connect("dev").await;
    analyze(&d, &mut rx).await;
    d.get("/api/streams").await;
    // Acknowledge an entry of the list shown.
    d.get(&format!("/api/streams/pending?key={key}&group={group}"))
        .await;
    let r = d
        .post_yes(
            "/api/streams/ack",
            json!({ "key": key, "group": group, "id": ids[0] }),
        )
        .await;
    let reply = sent(&r, "ack");
    assert_eq!(reply, json!({ "kind": "int", "value": 1 }), "{kind}");
    assert_eq!(pending_count(&mut admin, None).await, 3, "{kind}");
    assert_eq!(pending_count(&mut admin, Some("w1")).await, 2, "{kind}");
    // Read at once after the write, as the page does (`now=1`), inside the 2 s in which a
    // plain read answers from what was read.
    let v = d.get("/api/streams?now=1").await.json();
    assert_eq!(row(&v, "p4s:orders")["pending"], 3, "{kind}: {v}");
    // An entry not in the list shown is refused before anything is sent.
    let r = d
        .post(
            "/api/streams/ack",
            json!({ "key": key, "group": group, "id": "1-1" }),
        )
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    // Claim every pending entry for w2.
    let r = d
        .post_yes(
            "/api/streams/claim",
            json!({ "key": key, "group": group, "to": "w2", "min_idle_ms": "0", "count": "100" }),
        )
        .await;
    let reply = sent(&r, "claim");
    assert_eq!(reply["kind"], "claimed", "{kind}: {reply}");
    assert_eq!(reply["ids"].as_array().unwrap().len(), 3, "{kind}: {reply}");
    assert_eq!(pending_count(&mut admin, Some("w2")).await, 3, "{kind}");
    assert_eq!(pending_count(&mut admin, Some("w1")).await, 0, "{kind}");
    // The form's checks are the TUI's.
    let r = d
        .post(
            "/api/streams/claim",
            json!({ "key": key, "group": group, "to": "", "min_idle_ms": "0", "count": "1" }),
        )
        .await;
    assert_eq!(
        r.json()["error"],
        "Type the consumer that takes the entries.",
        "{kind}"
    );
    // Trim the big stream: approximate, whole blocks only, at least 10 kept.
    let big = hex("p4s:big");
    let r = d
        .post_yes(
            "/api/streams/trim",
            json!({ "key": big, "by": "MAXLEN", "value": "10" }),
        )
        .await;
    let reply = sent(&r, "trim");
    let trimmed = reply["value"].as_i64().unwrap();
    let left: i64 = q(&mut admin, &["XLEN", "p4s:big"]).await;
    assert!(trimmed > 0, "{kind}: {reply}");
    assert_eq!(left, 250 - trimmed, "{kind}");
    assert!(left >= 10, "{kind}: {left}");
    // MINID drops the entries older than the ID: here the oldest of `orders`.
    let r = d
        .post_yes(
            "/api/streams/trim",
            json!({ "key": key, "by": "MINID", "value": ids[1] }),
        )
        .await;
    sent(&r, "trim minid");
    let len: i64 = q(&mut admin, &["XLEN", "p4s:orders"]).await;
    assert!(len <= 6, "{kind}: {len}");
    let r = d
        .post(
            "/api/streams/trim",
            json!({ "key": key, "by": "MINID", "value": "yesterday" }),
        )
        .await;
    assert_eq!(
        r.json()["error"],
        "MINID takes an entry ID, such as 1700000000000-0.",
        "{kind}"
    );
    // Untouched: the other stream.
    let audit: i64 = q(&mut admin, &["XLEN", "p4s:audit"]).await;
    assert_eq!(audit, 2, "{kind}");
}

const LIM_USER: &str = "lens_p4_limited_q3x";
const LIM_PASS: &str = "Pw-p4-limited-8k2m-secret";

/// Rule 3: a user without `XINFO` sees Streams off in the TUI's words; one without
/// `XINFO GROUPS` (Redis 7 and Valkey) sees its streams without their groups, said so.
/// Search is off without the module. The user's name and password are in no reply or
/// event.
#[tokio::test]
async fn a_limited_user_sees_streams_off_without_its_name() {
    for valkey in [false, true] {
        let kind = if valkey { "valkey" } else { "redis" };
        let pass = format!(">{LIM_PASS}");
        let groups_user = format!("{LIM_USER}g");
        let mut args = vec![
            "--user", LIM_USER, "on", &pass, "~*", "&*", "+@all", "-xinfo",
        ];
        let subcommands = valkey || !common::redis_image().1.starts_with("6.");
        if subcommands {
            args.extend([
                "--user",
                &groups_user,
                "on",
                &pass,
                "~*",
                "&*",
                "+@all",
                "-xinfo|groups",
            ]);
        }
        let server = common::server_with(valkey, &args).await;
        let mut admin = server.admin().await;
        seed_streams(&mut admin).await;
        let store = Arc::new(MemoryStore::default());
        let mut profiles = Vec::new();
        for (name, user) in [("limited", LIM_USER), ("nogroups", groups_user.as_str())] {
            let mut p = profile(name, server.port, Env::Local, None);
            p.auth = Auth::Keychain;
            store
                .set(
                    &p.secret_ref(),
                    &Credentials {
                        username: Some(user.into()),
                        password: secrecy::SecretString::from(LIM_PASS.to_string()),
                    },
                )
                .unwrap();
            profiles.push(p);
        }
        let d = Dash::start_with(profiles, &format!("acl-{kind}"), store).await;
        let mut rx = d.events().await;
        d.connect("limited").await;
        analyze(&d, &mut rx).await;
        let v = d.get("/api/streams").await.json();
        assert_eq!(
            v["off"], "The server refused XINFO for this user, so this view is off.",
            "{kind}: {v}"
        );
        let p = d
            .get(&format!(
                "/api/streams/pending?key={}&group={}",
                hex("p4s:orders"),
                hex("billing")
            ))
            .await
            .json();
        assert!(p["off"].is_string(), "{kind}: {p}");
        let s = d.get("/api/search").await.json();
        assert_eq!(s["no_module"], true, "{kind}: {s}");
        if subcommands {
            d.connect("nogroups").await;
            analyze(&d, &mut rx).await;
            let v = d.get("/api/streams").await.json();
            let orders = row(&v, "p4s:orders");
            assert_eq!(orders["groups"], Value::Null, "{kind}: {orders}");
            assert_eq!(
                orders["groups_text"], "XINFO GROUPS is not available for this user.",
                "{kind}"
            );
            assert_eq!(orders["length"], 5, "{kind}");
        }
        for seen in d.seen() {
            assert!(!seen.contains(LIM_USER), "{kind}: {seen}");
            assert!(!seen.contains(LIM_PASS), "{kind}: {seen}");
            assert!(!seen.contains("NOPERM"), "{kind}: {seen}");
        }
    }
}

/// On a cluster each stream is read on the node that owns it, pending entries too, and
/// a write goes to the owner; Search reads the picked node and is off there.
#[tokio::test]
async fn a_cluster_reads_each_stream_on_its_owner() {
    for valkey in [false, true] {
        let kind = if valkey { "valkey" } else { "redis" };
        let c = common::cluster::cluster(valkey, false).await;
        let client = redis::cluster::ClusterClient::new(vec![c.url(c.seed_port())]).unwrap();
        let mut cl = client.get_async_connection().await.unwrap();
        let names: Vec<String> = (0..6).map(|i| format!("p4c:{i}")).collect();
        for k in &names {
            for i in 0..3 {
                let _: String = redis::cmd("XADD")
                    .arg(k)
                    .arg("*")
                    .arg("i")
                    .arg(i)
                    .query_async(&mut cl)
                    .await
                    .unwrap();
            }
            let _: () = redis::cmd("XGROUP")
                .arg("CREATE")
                .arg(k)
                .arg("g")
                .arg("0")
                .query_async(&mut cl)
                .await
                .unwrap();
            let _: redis::Value = redis::cmd("XREADGROUP")
                .arg("GROUP")
                .arg("g")
                .arg("c1")
                .arg("COUNT")
                .arg(2)
                .arg("STREAMS")
                .arg(k)
                .arg(">")
                .query_async(&mut cl)
                .await
                .unwrap();
        }
        let d = Dash::start(
            vec![profile("cl", c.seed_port(), Env::Other, None)],
            &format!("cluster-{kind}"),
        )
        .await;
        let mut rx = d.events().await;
        d.connect("cl").await;
        analyze(&d, &mut rx).await;
        c.reset_stats().await;
        let v = d.get("/api/streams?refresh=1").await.json();
        assert_eq!(v["cluster"], true, "{kind}: {v}");
        // The analysis keeps five examples a pattern: the streams it found.
        let rows = v["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 5, "{kind}: {v}");
        let mut nodes = std::collections::HashSet::new();
        for r in rows {
            assert!(
                names.iter().any(|k| r["key"]["name"] == k.as_str()),
                "{kind}: {r}"
            );
            assert_eq!(r["length"], 3, "{kind}: {r}");
            assert_eq!(r["pending"], 2, "{kind}: {r}");
            nodes.insert(r["node"].as_u64().unwrap());
        }
        assert!(nodes.len() > 1, "{kind}: the streams live on several nodes");
        // XINFO went to every node that owns a stream, and to no other.
        let mut asked = 0;
        for p in &c.ports {
            let stats = c.commandstats(*p).await;
            // Redis 6.2 counts `XINFO` whole; later servers per subcommand.
            let xinfo = |name: &str| stats.get(name).is_some_and(|(n, _)| *n > 0);
            if xinfo("xinfo|stream") || xinfo("xinfo") {
                asked += 1;
            }
        }
        assert_eq!(asked, nodes.len(), "{kind}");
        // Pending, then an ack, on the owner.
        let k = rows[3]["key"]["name"].as_str().unwrap().to_string();
        let k = &k;
        let p = d
            .get(&format!(
                "/api/streams/pending?key={}&group={}",
                hex(k),
                hex("g")
            ))
            .await
            .json();
        let first = p["pending"]["entries"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let r = d
            .post_yes(
                "/api/streams/ack",
                json!({ "key": hex(k), "group": hex("g"), "id": first }),
            )
            .await;
        assert_eq!(sent(&r, "cluster ack")["value"], 1, "{kind}");
        let left: Vec<redis::Value> = redis::cmd("XPENDING")
            .arg(k)
            .arg("g")
            .arg("-")
            .arg("+")
            .arg(10)
            .query_async(&mut cl)
            .await
            .unwrap();
        assert_eq!(left.len(), 1, "{kind}");
        let s = d.get("/api/search").await.json();
        assert_eq!(s["no_module"], true, "{kind}: {s}");
    }
}

// -- Browse › Search -------------------------------------------------------------------

/// The test images have no Search module: off in the TUI's words, the writes refused,
/// nothing sent.
#[tokio::test]
async fn search_is_off_without_the_module_on_redis_and_valkey() {
    for (kind, server) in servers().await {
        let d = Dash::start(
            vec![profile("dev", server.port, Env::Other, None)],
            &format!("nosearch-{kind}"),
        )
        .await;
        d.connect("dev").await;
        let v = d.get("/api/search").await;
        assert_eq!(v.status, 200, "{kind}: {}", v.body);
        let v = v.json();
        capture("search off", v.clone());
        assert_eq!(v["no_module"], true, "{kind}: {v}");
        own_words(kind, &v);
        let mut admin = server.admin().await;
        cmd(&mut admin, &["CONFIG", "RESETSTAT"]).await;
        let r = d
            .post_yes(
                "/api/search/create",
                json!({ "name": "p4idx", "on": "HASH", "prefixes": "p4p:", "fields": "title TEXT" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "Not available: the server has no Search module, so indexes can't be created or dropped.",
            "{kind}"
        );
        let info: String = q(&mut admin, &["INFO", "commandstats"]).await;
        assert_eq!(writes_in(&info), Vec::<String>::new(), "{kind}");
    }
}

/// A server with a Search module, started here as `tests/search_it.rs` starts them.
struct ModuleServer {
    port: u16,
    valkey: bool,
    _container: testcontainers::ContainerAsync<testcontainers::GenericImage>,
}

async fn module_server(image: (&str, &str, &str)) -> ModuleServer {
    use testcontainers::core::{IntoContainerPort, WaitFor};
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{GenericImage, ImageExt};
    let container = GenericImage::new(image.0, image.1)
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_cmd(vec![image.2.to_string()])
        .start()
        .await
        .expect("start container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(6379.tcp())
        .await
        .expect("mapped port");
    ModuleServer {
        port,
        valkey: image.2 == "valkey-server",
        _container: container,
    }
}

#[tokio::test]
async fn search_creates_queries_and_drops_on_redis_8_and_valkey_search() {
    for image in [
        ("redis", "8.10.1", "redis-server"),
        ("valkey/valkey-bundle", "9.1.3", "valkey-server"),
    ] {
        let server = module_server(image).await;
        search_on(&server).await;
    }
}

async fn search_on(server: &ModuleServer) {
    let kind = if server.valkey {
        "valkey-search"
    } else {
        "redis 8"
    };
    let mut admin = redis::Client::open(format!("redis://127.0.0.1:{}", server.port))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    for i in 1..=3 {
        cmd(
            &mut admin,
            &[
                "HSET",
                &format!("p4p:{i}"),
                "title",
                if i == 3 { "blue hat" } else { "red shoe" },
                "price",
                &(i * 10).to_string(),
            ],
        )
        .await;
    }
    let d = Dash::start(
        vec![
            profile("dev", server.port, Env::Other, None),
            profile("ro", server.port, Env::Local, Some(true)),
        ],
        &format!("search-{}", if server.valkey { "valkey" } else { "redis" }),
    )
    .await;
    d.connect("dev").await;
    let v = d.get("/api/search").await.json();
    assert_eq!(v["indexes"], json!([]), "{kind}: {v}");
    assert_eq!(v["empty"]["title"], "The module has no indexes.", "{kind}");

    // FT.CREATE from the form, through the gate.
    let r = d
        .post(
            "/api/search/create",
            json!({ "name": "p4 idx", "on": "HASH", "prefixes": "", "fields": "title TEXT" }),
        )
        .await;
    assert_eq!(r.json()["error"], "An index name has no spaces.", "{kind}");
    let r = d
        .post_yes(
            "/api/search/create",
            json!({ "name": "p4idx", "on": "HASH", "prefixes": "p4p:", "fields": "title TEXT, price NUMERIC" }),
        )
        .await;
    sent(&r, "create");
    let list: Vec<String> = q(&mut admin, &["FT._LIST"]).await;
    assert_eq!(list, ["p4idx"], "{kind}");
    // Read again after the write; until it has indexed the three.
    let t0 = Instant::now();
    let v = loop {
        let v = d.get("/api/search?refresh=1").await.json();
        if v["indexes"][0]["docs"] == 3 && v["indexes"][0]["building"] == false {
            break v;
        }
        assert!(t0.elapsed() < WAIT, "{kind}: {v}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    own_words(kind, &v);
    // The list is read each time the view asks: an index made elsewhere shows at once.
    cmd(
        &mut admin,
        &[
            "FT.CREATE",
            "p4other",
            "ON",
            "HASH",
            "PREFIX",
            "1",
            "p4o:",
            "SCHEMA",
            "n",
            "NUMERIC",
        ],
    )
    .await;
    let names: Vec<Value> = d.get("/api/search").await.json()["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"]["name"].clone())
        .collect();
    assert!(names.contains(&json!("p4other")), "{kind}: {names:?}");
    cmd(&mut admin, &["FT.DROPINDEX", "p4other"]).await;
    let idx = &v["indexes"][0];
    assert_eq!(idx["name"]["name"], "p4idx", "{kind}");
    assert_eq!(idx["prefixes"], json!(["p4p:"]), "{kind}");
    let id = hex("p4idx");

    // FT.SEARCH, its results paged, a field drawn whole.
    let r = d
        .post(
            "/api/search/query",
            json!({ "index": id, "cmd": "search", "query": "@price:[0 100]" }),
        )
        .await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    let v = r.json();
    let res = &v["results"];
    assert_eq!(res["total"], 3, "{kind}: {v}");
    let run = res["run"].as_u64().unwrap();
    let doc = res["docs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["key"]["name"] == "p4p:3")
        .unwrap_or_else(|| panic!("{kind}: {v}"));
    let f = doc["fields"]
        .as_array()
        .unwrap()
        .iter()
        .position(|f| f["name"] == "title")
        .unwrap();
    let field = d
        .get(&format!(
            "/api/search/field?run={run}&doc={}&field={f}",
            doc["index"]
        ))
        .await
        .json();
    assert_eq!(field["shown"]["lines"][0], "blue hat", "{kind}: {field}");
    let page = d
        .get(&format!("/api/search/results?run={run}&from=0"))
        .await
        .json();
    assert_eq!(page["kept"], 3, "{kind}: {page}");
    let r = d
        .post(
            "/api/search/query",
            json!({ "index": id, "cmd": "search", "query": "@title:red" }),
        )
        .await;
    assert_eq!(r.json()["results"]["total"], 2, "{kind}: {}", r.body);
    // A query the module can't parse: its words about the query, in redis-lens's frame.
    let r = d
        .post(
            "/api/search/query",
            json!({ "index": id, "cmd": "search", "query": "@price:[" }),
        )
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    let why = r.json()["error"].as_str().unwrap().to_string();
    assert!(why.starts_with("FT.SEARCH failed: "), "{kind}: {why}");
    assert!(
        !why.contains("ERR ") && !why.contains("SEARCH_SYNTAX"),
        "{why}"
    );
    capture("search failed", r.json());

    // FT.EXPLAIN and FT.PROFILE on the query last run: Redis Search has both,
    // valkey-search neither, said once and not sent again.
    d.post(
        "/api/search/query",
        json!({ "index": id, "cmd": "search", "query": "@title:red" }),
    )
    .await;
    for cmd_name in ["explain", "profile"] {
        let r = d
            .post("/api/search/query", json!({ "index": id, "cmd": cmd_name }))
            .await;
        assert_eq!(r.status, 200, "{kind}: {cmd_name}: {}", r.body);
        let v = r.json();
        if server.valkey {
            assert!(v["off"].is_string(), "{kind}: {cmd_name}: {v}");
        } else {
            assert!(
                !v["text"]["lines"].as_array().unwrap().is_empty(),
                "{kind}: {cmd_name}: {v}"
            );
            if cmd_name == "profile" {
                assert_eq!(v["results"]["total"], 2, "{kind}: {v}");
            }
        }
    }
    if server.valkey {
        let v = d.get("/api/search").await.json();
        assert!(v["cmd_off"]["explain"].is_string(), "{kind}: {v}");
    }

    // Read-only: no Search write sent.
    d.connect("ro").await;
    d.get("/api/search").await;
    cmd(&mut admin, &["CONFIG", "RESETSTAT"]).await;
    let r = d.post("/api/search/drop", json!({ "index": id })).await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    let info: String = q(&mut admin, &["INFO", "commandstats"]).await;
    assert_eq!(writes_in(&info), Vec::<String>::new(), "{kind}: {info}");

    // FT.DROPINDEX without DD: the index goes, its documents stay.
    d.connect("dev").await;
    d.get("/api/search").await;
    let r = d.post_yes("/api/search/drop", json!({ "index": id })).await;
    sent(&r, "drop");
    let list: Vec<String> = q(&mut admin, &["FT._LIST"]).await;
    assert!(list.is_empty(), "{kind}: {list:?}");
    let n: i64 = q(&mut admin, &["EXISTS", "p4p:1", "p4p:2", "p4p:3"]).await;
    assert_eq!(n, 3, "{kind}");
}

// -- Hot keys from Monitor ---------------------------------------------------------------

/// Without LFU counters, a Monitor run that saw this test's traffic ranks its keys for
/// Hot keys: the question first, the run's keys placed with COMMAND INFO, most named
/// first.
#[tokio::test]
async fn a_monitor_run_ranks_the_keys_it_saw_on_redis_and_valkey() {
    for (kind, server) in servers().await {
        let mut admin = server.admin().await;
        for k in ["p4h:a", "p4h:b", "p4h:c"] {
            cmd(&mut admin, &["SET", k, "v"]).await;
        }
        let d = Dash::start(
            vec![profile("dev", server.port, Env::Other, None)],
            &format!("hot-{kind}"),
        )
        .await;
        let mut rx = d.events().await;
        d.connect("dev").await;
        analyze(&d, &mut rx).await;
        let h = d.get("/api/analysis/hot_keys").await.json();
        assert!(h["off"].is_string(), "{kind}: no LFU counters: {h}");
        let v = d.get("/api/analysis/hot_keys/monitor").await.json();
        assert_eq!(v["status"], "none", "{kind}: {v}");

        let r = d.post("/api/server/monitor", json!({})).await;
        assert_eq!(r.status, 409, "{kind}: {}", r.body);
        let qid = r.json()["confirm"]["id"].as_str().unwrap().to_string();
        let r = d.post(&format!("/api/confirm/{qid}"), json!({})).await;
        assert_eq!(r.status, 202, "{kind}: {}", r.body);
        wait_for(&mut rx, "monitor", |v| v["kind"] == "ready").await;
        for _ in 0..6 {
            cmd(&mut admin, &["GET", "p4h:a"]).await;
        }
        for _ in 0..3 {
            cmd(&mut admin, &["GET", "p4h:b"]).await;
        }
        cmd(&mut admin, &["MGET", "p4h:c", "p4h:b"]).await;
        // Until the run's lines show the last command.
        let t0 = Instant::now();
        loop {
            let v = wait_for(&mut rx, "monitor", |v| v["kind"] == "lines").await;
            if v["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["line"] == "MGET p4h:c p4h:b")
            {
                break;
            }
            assert!(t0.elapsed() < WAIT, "{kind}");
        }
        let r = d.send("DELETE", "/api/server/monitor", json!({})).await;
        assert_eq!(r.json()["stopped"], true, "{kind}");
        let t0 = Instant::now();
        let v = loop {
            let v = d.get("/api/analysis/hot_keys/monitor").await.json();
            if v["status"] == "ready" {
                break v;
            }
            assert_eq!(v["status"], "pending", "{kind}: {v}");
            assert!(t0.elapsed() < WAIT, "{kind}: {v}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        own_words(kind, &v);
        let rows: Vec<(String, u64)> = v["ranking"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["key"]["name"].as_str().unwrap().to_string(),
                    r["count"].as_u64().unwrap(),
                )
            })
            .filter(|(k, _)| k.starts_with("p4h:"))
            .collect();
        assert_eq!(
            rows,
            [
                ("p4h:a".to_string(), 6),
                ("p4h:b".to_string(), 4),
                ("p4h:c".to_string(), 1)
            ],
            "{kind}: {v}"
        );
        assert!(
            v["ranking"]["title"]
                .as_str()
                .unwrap()
                .starts_with("from Monitor, "),
            "{kind}: {v}"
        );
        assert_eq!(v["text"], redis_lens::web::api::hot::SOURCE, "{kind}");
    }
}
