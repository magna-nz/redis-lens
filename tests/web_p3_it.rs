//! The web dashboard's phase 3 over real HTTP against real Redis and Valkey servers and
//! a cluster: every key edit Browse offers (a string, a hash, a list, a set, a sorted
//! set, a stream; adding a key of each type; TTLs, rename, delete), each read back
//! through a separate admin connection; bulk expire and bulk delete over a pattern,
//! with the dry run's counts, the question, the key file, a cancel and a stop. The
//! write gate's rules hold over HTTP: a read-only profile sends no write at all (the
//! server's own `INFO commandstats` says so), a profile made read-only in the config
//! file stops at once, `prod` asks and needs the key or the pattern typed, an answer
//! after a database switch is refused, and a user without a command has it turned off
//! in redis-lens's words, its name and password in no reply or event.
//!
//! The config file, the snapshot directory and the bulk key files are temporary ones,
//! and credentials are in a `MemoryStore`: the user's own are never touched.
//!
//! With `REDIS_LENS_P3_CAPTURE=<file>` every reply and event is appended to that file as
//! JSON lines, for the examples in `docs/dashboard-api.md`.
#![cfg(feature = "integration-tests")]

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use redis_lens::config::Config;
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile};
use redis_lens::secrets::{MemoryStore, SecretStore};
use redis_lens::web::api::bulk::hold;
use redis_lens::web::wording;
use redis_lens::web::{self, Dashboard, Options};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(60);
const CAPTURE: &str = "REDIS_LENS_P3_CAPTURE";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-web-p3-{tag}-{}-{nanos}",
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
    config: PathBuf,
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
            config,
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

    /// Writes on or off for `name` in the config file, as `conn set` does beside it.
    fn flip_writes(&self, name: &str, on: bool) {
        let mut c = Config::load(&self.config).unwrap();
        c.set_writes(name, on).unwrap();
        c.save(&self.config).unwrap();
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
                        if name == "bulk" {
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

    async fn open(&self, key: &str) -> Value {
        let r = self.get(&format!("/api/value?key={}", hex(key))).await;
        assert_eq!(r.status, 200, "{key}: {}", r.body);
        r.json()
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

/// Starts a bulk delete of `job:{id}`: its id.
async fn job_delete(d: &Dash) -> Value {
    let r = d
        .post(
            "/api/bulk",
            json!({ "pattern": hex("job:{id}"), "action": "delete" }),
        )
        .await;
    assert_eq!(r.status, 202, "{}", r.body);
    r.json()["bulk"]["id"].clone()
}

/// Waits until the dashboard's bulk run held with [`hold::hold`] came to its point
/// (before a batch) `n` times.
async fn arrived(d: &Dash, n: u64) {
    let t0 = Instant::now();
    while hold::arrived(&d.config) < n {
        assert!(t0.elapsed() < WAIT, "the run did not reach batch {n}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The `bulk` event of bulk action `id` once it reaches `stage`.
async fn bulk_at(
    rx: &mut mpsc::UnboundedReceiver<(String, Value)>,
    id: &Value,
    stage: &str,
) -> Value {
    wait_for(rx, "bulk", |v| {
        v["bulk"]["id"] == *id && v["bulk"]["stage"] == stage
    })
    .await["bulk"]
        .clone()
}

/// The index of the item whose `field` is `want` in a value body.
fn index_of(v: &Value, field: &str, want: &str) -> u64 {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i[field] == want)
        .unwrap_or_else(|| panic!("no item with {field} {want}: {v}"))["index"]
        .as_u64()
        .unwrap()
}

/// A write reply that did what was asked, with what it says.
fn done(r: &Res, what: &str) -> Value {
    assert_eq!(r.status, 200, "{what}: {}", r.body);
    let v = r.json();
    assert_eq!(v["sent"], true, "{what}: {v}");
    assert_eq!(v["done"], true, "{what}: {v}");
    own_words(what, &v);
    v
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

/// Field TTLs are read on Redis 7.4 and later (Valkey 8.1 has none).
fn field_ttls(kind: &str) -> bool {
    let tag = common::redis_image().1;
    kind == "redis" && !tag.starts_with("6.") && !tag.starts_with("7.0") && !tag.starts_with("7.2")
}

// -- every edit of a key's value, per type -------------------------------------------

#[tokio::test]
async fn every_edit_changes_exactly_what_it_should_on_redis_and_valkey() {
    for (kind, server) in servers().await {
        edits(kind, &server).await;
    }
}

async fn edits(kind: &str, server: &common::TestServer) {
    let mut admin = server.admin().await;
    cmd(&mut admin, &["SET", "str:a", "hello", "EX", "100000"]).await;
    cmd(
        &mut admin,
        &["HSET", "user:1", "name", "ann", "mail", "a@x"],
    )
    .await;
    cmd(&mut admin, &["RPUSH", "queue:1", "a", "b", "c"]).await;
    cmd(&mut admin, &["SADD", "tags:1", "x", "y"]).await;
    cmd(&mut admin, &["ZADD", "rank:1", "1", "m1", "2", "m2"]).await;
    cmd(&mut admin, &["XADD", "events:1", "*", "f", "v"]).await;
    cmd(&mut admin, &["SET", "taken:1", "t"]).await;
    let d = Dash::start(
        vec![profile("local", server.port, Env::Local, None)],
        &format!("edits-{kind}"),
    )
    .await;
    d.connect("local").await;

    // A string: SET … KEEPTTL, only while the value is the one loaded.
    let v = d.open("str:a").await;
    let view = v["view"].clone();
    // Opened by name alone: its length is the STRLEN read, in bytes (the TUI's facts).
    assert_eq!(v["length"], 5, "{kind}: {v}");
    assert_eq!(v["meta"]["length"], 5, "{kind}: {v}");
    assert_eq!(v["meta"]["length_unit"], "bytes", "{kind}: {v}");
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 0, "index": 0, "value": "world" }),
        )
        .await;
    let w = done(&r, "string edit");
    assert_eq!(w["message"], "Saved.", "{kind}: {w}");
    assert_eq!(w["value"]["rev"], 1, "{kind}: {w}");
    assert_eq!(w["value"]["value"]["lines"][0], "world", "{kind}: {w}");
    assert_eq!(q::<String>(&mut admin, &["GET", "str:a"]).await, "world");
    let ttl: i64 = q(&mut admin, &["TTL", "str:a"]).await;
    assert!(ttl > 99_000, "{kind}: the TTL is kept: {ttl}");
    // Changed behind its back: not written, and read again.
    cmd(&mut admin, &["SET", "str:a", "other"]).await;
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 1, "value": "mine" }),
        )
        .await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    let w = r.json();
    assert_eq!(w["done"], false, "{kind}: {w}");
    assert_eq!(
        w["message"], "Not written: the value changed since it was loaded. It is read again now.",
        "{kind}: {w}"
    );
    assert_eq!(w["value"]["value"]["lines"][0], "other", "{kind}: {w}");
    assert_eq!(q::<String>(&mut admin, &["GET", "str:a"]).await, "other");
    // An edit naming the old rev is refused, and the same value sends nothing.
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 1, "value": "mine" }),
        )
        .await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 2, "value": "other" }),
        )
        .await;
    assert_eq!(r.json()["sent"], false, "{kind}: {}", r.body);
    // A string has no items to add or remove.
    let r = d
        .post_yes("/api/value/add", json!({ "view": view, "value": "x" }))
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    own_words("add to a string", &r.json());

    // A hash: HSET, HSETNX (never over a field), HDEL, and a field's TTL.
    let v = d.open("user:1").await;
    let view = v["view"].clone();
    // Opened by name alone: the unit is the read type's, not the guessed string's.
    assert_eq!(v["meta"]["type"], "hash", "{kind}: {v}");
    assert_eq!(v["meta"]["length_unit"], "fields", "{kind}: {v}");
    let name = index_of(&v, "name", "name");
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 0, "index": name, "value": "bob" }),
        )
        .await;
    let w = done(&r, "hash edit");
    assert_eq!(
        w["row"]["type"], "hash",
        "{kind}: the row says the type TYPE gave: {w}"
    );
    assert_eq!(
        q::<String>(&mut admin, &["HGET", "user:1", "name"]).await,
        "bob"
    );
    let r = d
        .post_yes(
            "/api/value/add",
            json!({ "view": view, "field": "age", "value": "42" }),
        )
        .await;
    done(&r, "hash add");
    assert_eq!(
        q::<String>(&mut admin, &["HGET", "user:1", "age"]).await,
        "42"
    );
    let r = d
        .post_yes(
            "/api/value/add",
            json!({ "view": view, "field": "age", "value": "7" }),
        )
        .await;
    let w2 = r.json();
    assert_eq!(w2["done"], false, "{kind}: {w2}");
    assert_eq!(
        w2["message"], "Not added: the field exists already. Edit it instead.",
        "{kind}"
    );
    assert_eq!(
        q::<String>(&mut admin, &["HGET", "user:1", "age"]).await,
        "42"
    );
    let v = d.get(&format!("/api/value/more?view={view}")).await.json();
    let rev = v["rev"].clone();
    let _ = w;
    let mail = index_of(&v, "name", "mail");
    let r = d
        .post_yes(
            "/api/value/remove",
            json!({ "view": view, "rev": rev, "index": mail }),
        )
        .await;
    let w = done(&r, "hash remove");
    assert_eq!(
        q::<i64>(&mut admin, &["HEXISTS", "user:1", "mail"]).await,
        0
    );
    assert_eq!(q::<i64>(&mut admin, &["HLEN", "user:1"]).await, 2);
    let rev = w["value"]["rev"].clone();
    let age = index_of(&w["value"], "name", "age");
    let r = d
        .post_yes(
            "/api/value/ttl",
            json!({ "view": view, "ttl": "1h", "field": age, "rev": rev }),
        )
        .await;
    if field_ttls(kind) {
        let w = done(&r, "field ttl");
        assert_eq!(w["message"], "Field TTL set.", "{kind}: {w}");
        let ttls: Vec<i64> = q(&mut admin, &["HTTL", "user:1", "FIELDS", "1", "age"]).await;
        assert!(ttls[0] > 3_500, "{kind}: {ttls:?}");
        assert_eq!(
            q::<Vec<i64>>(&mut admin, &["HTTL", "user:1", "FIELDS", "1", "name"]).await,
            [-1]
        );
    } else {
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "Field TTLs need Redis 7.4 or later, or Valkey 9.0 or later.",
            "{kind}"
        );
    }
    // The key's own TTL: set, removed, and 0 refused (it would delete the key).
    let r = d
        .post_yes("/api/value/ttl", json!({ "view": view, "ttl": "1h30m" }))
        .await;
    let w = done(&r, "key ttl");
    assert_eq!(w["message"], "TTL set to 1h 30m.", "{kind}: {w}");
    assert!(
        w["row"]["ttl_secs"].as_u64().unwrap() > 5_000,
        "{kind}: {w}"
    );
    let ttl: i64 = q(&mut admin, &["TTL", "user:1"]).await;
    assert!((5_000..=5_400).contains(&ttl), "{kind}: {ttl}");
    let r = d
        .post_yes("/api/value/ttl", json!({ "view": view, "ttl": "-" }))
        .await;
    done(&r, "persist");
    assert_eq!(q::<i64>(&mut admin, &["TTL", "user:1"]).await, -1);
    let r = d
        .post_yes("/api/value/ttl", json!({ "view": view, "ttl": "0" }))
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    assert_eq!(
        r.json()["error"],
        "A TTL of 0 would delete the key: delete the key instead."
    );

    // A list: LSET (only while the element is the one loaded), RPUSH, and LSET to a
    // marker then LREM for one element.
    let v = d.open("queue:1").await;
    let view = v["view"].clone();
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 0, "index": 1, "value": "B" }),
        )
        .await;
    let w = done(&r, "list edit");
    assert_eq!(
        q::<Vec<String>>(&mut admin, &["LRANGE", "queue:1", "0", "-1"]).await,
        ["a", "B", "c"]
    );
    let r = d
        .post_yes("/api/value/add", json!({ "view": view, "value": "d" }))
        .await;
    done(&r, "list add");
    let rev = d.get(&format!("/api/value/more?view={view}")).await.json()["rev"].clone();
    assert_eq!(rev, json!(2), "{kind}: {w}");
    let r = d
        .post_yes(
            "/api/value/remove",
            json!({ "view": view, "rev": rev, "index": 0 }),
        )
        .await;
    done(&r, "list remove");
    assert_eq!(
        q::<Vec<String>>(&mut admin, &["LRANGE", "queue:1", "0", "-1"]).await,
        ["B", "c", "d"]
    );

    // A set: a member replaced (SREM and SADD in MULTI/EXEC), SADD, SREM.
    let v = d.open("tags:1").await;
    let view = v["view"].clone();
    let x = index_of(&v, "value", "x");
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 0, "index": x, "value": "x2" }),
        )
        .await;
    let w = done(&r, "set edit");
    let mut members: Vec<String> = q(&mut admin, &["SMEMBERS", "tags:1"]).await;
    members.sort();
    assert_eq!(members, ["x2", "y"], "{kind}");
    let r = d
        .post_yes("/api/value/add", json!({ "view": view, "value": "z" }))
        .await;
    let w = {
        let _ = w;
        done(&r, "set add")
    };
    let y = index_of(&w["value"], "value", "y");
    let r = d
        .post_yes(
            "/api/value/remove",
            json!({ "view": view, "rev": w["value"]["rev"], "index": y }),
        )
        .await;
    done(&r, "set remove");
    let mut members: Vec<String> = q(&mut admin, &["SMEMBERS", "tags:1"]).await;
    members.sort();
    assert_eq!(members, ["x2", "z"], "{kind}");

    // A sorted set: a member's score (ZADD XX), ZADD NX with a score, ZREM.
    let v = d.open("rank:1").await;
    let view = v["view"].clone();
    let m1 = index_of(&v, "name", "m1");
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 0, "index": m1, "value": "5" }),
        )
        .await;
    done(&r, "zset edit");
    assert_eq!(q::<f64>(&mut admin, &["ZSCORE", "rank:1", "m1"]).await, 5.0);
    let r = d
        .post_yes(
            "/api/value/add",
            json!({ "view": view, "value": "m3", "score": "3.5" }),
        )
        .await;
    let w = done(&r, "zset add");
    assert_eq!(q::<f64>(&mut admin, &["ZSCORE", "rank:1", "m3"]).await, 3.5);
    let r = d
        .post_yes(
            "/api/value/add",
            json!({ "view": view, "value": "m3", "score": "x" }),
        )
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    let m2 = index_of(&w["value"], "name", "m2");
    let r = d
        .post_yes(
            "/api/value/remove",
            json!({ "view": view, "rev": w["value"]["rev"], "index": m2 }),
        )
        .await;
    done(&r, "zset remove");
    assert_eq!(q::<i64>(&mut admin, &["ZCARD", "rank:1"]).await, 2);
    assert_eq!(
        q::<Option<f64>>(&mut admin, &["ZSCORE", "rank:1", "m2"]).await,
        None
    );

    // A stream: XADD with one field, XDEL; an entry is not edited.
    let v = d.open("events:1").await;
    let view = v["view"].clone();
    let r = d
        .post_yes(
            "/api/value/edit",
            json!({ "view": view, "rev": 0, "index": 0, "value": "x" }),
        )
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    assert_eq!(
        r.json()["error"],
        "A stream entry can't be edited: add an entry, or delete this one."
    );
    let r = d
        .post_yes(
            "/api/value/add",
            json!({ "view": view, "field": "f2", "value": "v2" }),
        )
        .await;
    let w = done(&r, "stream add");
    assert_eq!(w["message"], "Added.", "{kind}: {w}");
    assert_eq!(q::<i64>(&mut admin, &["XLEN", "events:1"]).await, 2);
    // Newest first: the entry just added is item 0.
    let r = d
        .post_yes(
            "/api/value/remove",
            json!({ "view": view, "rev": w["value"]["rev"], "index": 1 }),
        )
        .await;
    done(&r, "stream remove");
    let left: Vec<redis::Value> = q(&mut admin, &["XRANGE", "events:1", "-", "+"]).await;
    assert_eq!(left.len(), 1, "{kind}");
    assert!(
        format!("{left:?}").contains("f2"),
        "{kind}: the older one went"
    );

    // Rename: RENAMENX, never over another key.
    let v = d.open("tags:1").await;
    let view = v["view"].clone();
    let r = d
        .post_yes(
            "/api/value/rename",
            json!({ "view": view, "name": "taken:1" }),
        )
        .await;
    let w = r.json();
    assert_eq!(
        (r.status, w["done"].clone()),
        (200, json!(false)),
        "{kind}: {w}"
    );
    assert_eq!(
        w["message"],
        "Not renamed: taken:1 exists already, and a rename never overwrites a key."
    );
    assert_eq!(q::<String>(&mut admin, &["GET", "taken:1"]).await, "t");
    let r = d
        .post_yes(
            "/api/value/rename",
            json!({ "view": view, "name": "tags:2" }),
        )
        .await;
    let w = done(&r, "rename");
    assert_eq!(w["key"]["name"], "tags:2", "{kind}: {w}");
    assert_eq!(q::<i64>(&mut admin, &["EXISTS", "tags:1"]).await, 0);
    assert_eq!(q::<i64>(&mut admin, &["SCARD", "tags:2"]).await, 2);

    // Delete: UNLINK, the value closes.
    let v = d.open("rank:1").await;
    let view = v["view"].clone();
    let r = d
        .post_yes("/api/value/delete", json!({ "view": view }))
        .await;
    let w = done(&r, "delete");
    assert_eq!(w["closed"], true, "{kind}: {w}");
    assert_eq!(q::<i64>(&mut admin, &["EXISTS", "rank:1"]).await, 0);
    let r = d.get(&format!("/api/value/more?view={view}")).await;
    assert_eq!(r.status, 404, "{kind}: {}", r.body);

    // Add a key of each type with its first content, from a key list: never over one.
    let f = d.get("/api/find?q=new:").await.json();
    let list = f["list"].clone();
    for (ty, body, check) in [
        ("string", json!({ "value": "s" }), vec!["GET", "new:string"]),
        (
            "hash",
            json!({ "field": "f", "value": "v" }),
            vec!["HGET", "new:hash", "f"],
        ),
        (
            "list",
            json!({ "value": "e" }),
            vec!["LINDEX", "new:list", "0"],
        ),
        (
            "set",
            json!({ "value": "m" }),
            vec!["SRANDMEMBER", "new:set"],
        ),
        (
            "zset",
            json!({ "value": "m", "score": "2" }),
            vec!["ZSCORE", "new:zset", "m"],
        ),
    ] {
        let mut b = body.clone();
        b["name"] = json!(format!("new:{ty}"));
        b["type"] = json!(ty);
        b["list"] = list.clone();
        let r = d.post_yes("/api/keys/add", b).await;
        let w = done(&r, ty);
        assert_eq!(w["message"], format!("Added new:{ty}."), "{kind}: {w}");
        assert_eq!(w["row"]["type"], ty, "{kind}: {w}");
        assert!(w["value"]["view"].is_u64(), "{kind}: the value opens: {w}");
        // One byte, field, element or member, in the type's unit.
        let unit = match ty {
            "string" => "bytes",
            "hash" => "fields",
            "list" => "items",
            _ => "members",
        };
        assert_eq!(w["row"]["length"], 1, "{kind} {ty}: {w}");
        assert_eq!(w["row"]["length_unit"], unit, "{kind} {ty}: {w}");
        assert_eq!(w["value"]["meta"]["length_unit"], unit, "{kind} {ty}: {w}");
        // The pattern the key belongs to: none known without an analysis, and never
        // the find's text (`new:`).
        assert_eq!(w["value"]["pattern"], Value::Null, "{kind} {ty}: {w}");
        let got: String = q(&mut admin, &check).await;
        assert!(
            ["s", "v", "e", "m", "2"].contains(&got.as_str()),
            "{kind} {ty}: {got}"
        );
        let t: String = q(&mut admin, &["TYPE", &format!("new:{ty}")]).await;
        assert_eq!(t, ty, "{kind}");
    }
    let r = d
        .post_yes(
            "/api/keys/add",
            json!({ "name": "new:stream", "type": "stream", "field": "f", "value": "v" }),
        )
        .await;
    done(&r, "stream key");
    assert_eq!(q::<i64>(&mut admin, &["XLEN", "new:stream"]).await, 1);
    // A new string's length is its bytes, in the reply and when it is opened again.
    let r = d
        .post_yes(
            "/api/keys/add",
            json!({ "list": list, "name": "new:hello", "type": "string", "value": "hello" }),
        )
        .await;
    let w = done(&r, "a 5-byte string");
    assert_eq!(w["row"]["length"], 5, "{kind}: {w}");
    assert_eq!(w["row"]["length_unit"], "bytes", "{kind}: {w}");
    assert_eq!(w["value"]["length"], 5, "{kind}: {w}");
    assert_eq!(w["value"]["meta"]["length"], 5, "{kind}: {w}");
    let v = d.open("new:hello").await;
    assert_eq!(v["length"], 5, "{kind}: {v}");
    assert_eq!(v["meta"]["length"], 5, "{kind}: {v}");
    assert_eq!(v["pattern"], Value::Null, "{kind}: {v}");
    let r = d
        .post_yes(
            "/api/keys/add",
            json!({ "name": "taken:1", "type": "string", "value": "over" }),
        )
        .await;
    let w = r.json();
    assert_eq!(w["done"], false, "{kind}: {w}");
    assert_eq!(
        w["message"],
        "Not added: taken:1 exists already, and adding a key never overwrites one."
    );
    assert_eq!(q::<String>(&mut admin, &["GET", "taken:1"]).await, "t");
    let r = d
        .post_yes(
            "/api/keys/add",
            json!({ "name": "taken:1", "type": "string", "value": "over", "list": 999 }),
        )
        .await;
    assert_eq!(r.status, 404, "{kind}: {}", r.body);
    // The key list lists the keys added from it.
    let l = d.get(&format!("/api/keys/more?list={list}")).await;
    assert_eq!(l.status, 409, "{kind}: complete: {}", l.body);
}

// -- the gate: read-only, prod, a database switch, a profile turned read-only ---------

/// Write commands the dashboard could send for an edit or a bulk action.
const WRITES: &[&str] = &[
    "set", "hset", "hsetnx", "lset", "rpush", "sadd", "srem", "zadd", "zrem", "xadd", "xdel",
    "hdel", "lrem", "unlink", "del", "expire", "persist", "renamenx", "rename", "hexpire",
    "hpersist", "watch", "multi", "exec", "exists",
];

fn writes_in(info: &str) -> Vec<String> {
    common::cluster::parse_commandstats(info)
        .into_iter()
        .filter(|(name, (calls, _))| *calls > 0 && WRITES.contains(&name.as_str()))
        .map(|(n, _)| n)
        .collect()
}

#[tokio::test]
async fn the_gate_holds_over_http_on_redis_and_valkey() {
    for (kind, server) in servers().await {
        gate(kind, &server).await;
    }
}

async fn gate(kind: &str, server: &common::TestServer) {
    let mut admin = server.admin().await;
    cmd(&mut admin, &["HSET", "user:1", "name", "ann"]).await;
    cmd(&mut admin, &["SET", "str:a", "hello"]).await;
    for i in 0..5 {
        cmd(&mut admin, &["SET", &format!("cart:{i}"), "x"]).await;
    }
    cmd(&mut admin, &["SELECT", "1"]).await;
    cmd(&mut admin, &["SET", "other:1", "x"]).await;
    cmd(&mut admin, &["SELECT", "0"]).await;
    let d = Dash::start(
        vec![
            profile("ro", server.port, Env::Local, Some(true)),
            profile("prod1", server.port, Env::Prod, Some(false)),
            profile("dev", server.port, Env::Other, None),
        ],
        &format!("gate-{kind}"),
    )
    .await;

    // A read-only profile: every route refused with the gate's words, no write sent.
    d.connect("ro").await;
    let h = d.open("user:1").await;
    let s = d.open("str:a").await;
    cmd(&mut admin, &["CONFIG", "RESETSTAT"]).await;
    let (hv, sv) = (h["view"].clone(), s["view"].clone());
    let pattern = hex("cart:{id}");
    for (path, body) in [
        (
            "/api/value/edit",
            json!({ "view": sv, "rev": 0, "value": "x" }),
        ),
        (
            "/api/value/add",
            json!({ "view": hv, "field": "f", "value": "v" }),
        ),
        ("/api/value/ttl", json!({ "view": hv, "ttl": "1h" })),
        ("/api/value/rename", json!({ "view": hv, "name": "user:2" })),
        (
            "/api/value/remove",
            json!({ "view": hv, "rev": 0, "index": 0 }),
        ),
        ("/api/value/delete", json!({ "view": sv })),
        (
            "/api/keys/add",
            json!({ "name": "k", "type": "string", "value": "v" }),
        ),
        (
            "/api/bulk",
            json!({ "pattern": pattern, "action": "delete" }),
        ),
    ] {
        let r = d.post(path, body).await;
        assert_eq!(r.status, 422, "{kind} {path}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "Not sent: ro is read-only. Turn writes on in Connections.",
            "{kind} {path}"
        );
    }
    let info: String = q(&mut admin, &["INFO", "commandstats"]).await;
    assert_eq!(writes_in(&info), Vec::<String>::new(), "{kind}: {info}");
    assert!(
        !info.contains("cmdstat_scan:"),
        "{kind}: a bulk action on a read-only profile sends no SCAN: {info}"
    );

    // Prod with writes on: an edit asks yes or no; a delete needs the key typed; a bulk
    // action the pattern.
    d.connect("prod1").await;
    let s = d.open("str:a").await;
    let sv = s["view"].clone();
    let r = d
        .post(
            "/api/value/edit",
            json!({ "view": sv, "rev": 0, "value": "prod-edit" }),
        )
        .await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    let c = r.json()["confirm"].clone();
    assert_eq!(c["kind"], "ask", "{kind}: {c}");
    assert_eq!(
        c["text"],
        "Send SET str:a prod-edit KEEPTTL  (in MULTI/EXEC after WATCH, only if unchanged) to prod1?"
    );
    let r = d
        .post(
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    done(&r, "prod edit");
    assert_eq!(
        q::<String>(&mut admin, &["GET", "str:a"]).await,
        "prod-edit"
    );
    let r = d.post("/api/value/delete", json!({ "view": sv })).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    let c = r.json()["confirm"].clone();
    assert_eq!(c["kind"], "type", "{kind}: {c}");
    assert_eq!(c["expect"], "str:a", "{kind}: {c}");
    assert_eq!(
        c["text"],
        "Type str:a to send UNLINK str:a to prod1. This can't be undone."
    );
    let id = c["id"].as_str().unwrap().to_string();
    let r = d
        .post(&format!("/api/confirm/{id}"), json!({ "typed": "prod1" }))
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    assert_eq!(r.json()["error"], "Not sent: the name typed did not match.");
    assert_eq!(q::<i64>(&mut admin, &["EXISTS", "str:a"]).await, 1);
    let r = d
        .post(&format!("/api/confirm/{id}"), json!({ "typed": "str:a" }))
        .await;
    assert_eq!(r.status, 410, "{kind}: used once: {}", r.body);
    let r = d.post("/api/value/delete", json!({ "view": sv })).await;
    let id = r.json()["confirm"]["id"].as_str().unwrap().to_string();
    let r = d
        .post(&format!("/api/confirm/{id}"), json!({ "typed": "str:a" }))
        .await;
    done(&r, "prod delete");
    assert_eq!(q::<i64>(&mut admin, &["EXISTS", "str:a"]).await, 0);

    // An answer after a database switch is refused, and nothing is written to either.
    let h = d.open("user:1").await;
    let hv = h["view"].clone();
    let r = d
        .post(
            "/api/value/add",
            json!({ "view": hv, "field": "late", "value": "v" }),
        )
        .await;
    let id = r.json()["confirm"]["id"].as_str().unwrap().to_string();
    let dbs = d.get("/api/dbs").await;
    assert_eq!(dbs.status, 200, "{kind}: {}", dbs.body);
    let r = d.post("/api/db", json!({ "db": 1 })).await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    d.state_until("db 1", |s| {
        s["connection"]["db"] == 1 || s["analysis"]["db"] == 1
    })
    .await;
    d.ready().await;
    let r = d.post(&format!("/api/confirm/{id}"), json!({})).await;
    assert_eq!(r.status, 409, "{kind}: {}", r.body);
    assert_eq!(
        r.json()["error"],
        "Not sent: the connection changed since the question was asked."
    );
    assert_eq!(
        q::<i64>(&mut admin, &["HEXISTS", "user:1", "late"]).await,
        0
    );
    cmd(&mut admin, &["SELECT", "1"]).await;
    assert_eq!(q::<i64>(&mut admin, &["EXISTS", "user:1"]).await, 0);
    cmd(&mut admin, &["SELECT", "0"]).await;
    let dbs = d.get("/api/dbs").await.json();
    assert_eq!(dbs["current"], 1, "{kind}: {dbs}");
    let r = d.post("/api/db", json!({ "db": 0 })).await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    d.state_until("db 0", |s| s["analysis"]["db"] == 0).await;
    d.ready().await;

    // A question asked on prod, then writes turned off in the config file: refused.
    let h = d.open("user:1").await;
    let hv = h["view"].clone();
    let r = d
        .post(
            "/api/value/add",
            json!({ "view": hv, "field": "late", "value": "v" }),
        )
        .await;
    let id = r.json()["confirm"]["id"].as_str().unwrap().to_string();
    d.flip_writes("prod1", false);
    let r = d.post(&format!("/api/confirm/{id}"), json!({})).await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    assert_eq!(
        r.json()["error"],
        "Not sent: prod1 is read-only. Turn writes on in Connections."
    );
    assert_eq!(
        q::<i64>(&mut admin, &["HEXISTS", "user:1", "late"]).await,
        0
    );

    // A dev profile made read-only elsewhere refuses the next edit at once.
    d.connect("dev").await;
    let h = d.open("user:1").await;
    let hv = h["view"].clone();
    let r = d
        .post(
            "/api/value/add",
            json!({ "view": hv, "field": "dev", "value": "1" }),
        )
        .await;
    done(&r, "dev add");
    d.flip_writes("dev", false);
    let r = d
        .post(
            "/api/value/add",
            json!({ "view": hv, "field": "dev2", "value": "1" }),
        )
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    assert_eq!(
        q::<i64>(&mut admin, &["HEXISTS", "user:1", "dev2"]).await,
        0
    );
    for t in d.seen() {
        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
        own_words(kind, &v);
    }
}

// -- bulk actions --------------------------------------------------------------------

fn key_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(key_files(&p));
        } else if p.extension().is_some_and(|x| x == "txt") {
            out.push(p);
        }
    }
    out.sort();
    out
}

async fn count_keys(admin: &mut MultiplexedConnection, glob: &str) -> usize {
    let mut cursor = 0u64;
    let mut n = 0;
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(glob)
            .arg("COUNT")
            .arg(1000)
            .query_async(admin)
            .await
            .unwrap();
        n += keys.len();
        cursor = next;
        if cursor == 0 {
            return n;
        }
    }
}

#[tokio::test]
async fn bulk_actions_hit_exactly_the_patterns_keys_on_redis_and_valkey() {
    for (kind, server) in servers().await {
        bulk(kind, &server).await;
    }
}

async fn bulk(kind: &str, server: &common::TestServer) {
    let mut admin = server.admin().await;
    let mut pipe = redis::pipe();
    for i in 0..1_200 {
        pipe.cmd("SET").arg(format!("cart:{i}")).arg("x").ignore();
    }
    for i in 0..50 {
        pipe.cmd("EXPIRE")
            .arg(format!("cart:{i}"))
            .arg(100_000)
            .ignore();
    }
    // The glob `cart:*` matches these, but they belong to another pattern.
    for i in 0..20 {
        pipe.cmd("SET")
            .arg(format!("cart:{i}:items"))
            .arg("x")
            .ignore();
    }
    for i in 0..10 {
        pipe.cmd("SET").arg(format!("user:{i}")).arg("x").ignore();
    }
    pipe.query_async::<()>(&mut admin).await.unwrap();
    let d = Dash::start(
        vec![
            profile("local", server.port, Env::Local, None),
            profile("stage1", server.port, Env::Stage, None),
            profile("prod1", server.port, Env::Prod, Some(false)),
        ],
        &format!("bulk-{kind}"),
    )
    .await;
    let mut rx = d.events().await;
    d.connect("local").await;
    let pattern = hex("cart:{id}");

    // Expire: EXPIRE … NX on the keys without a TTL (Redis 7.0 and later, every Valkey).
    let r = d
        .post(
            "/api/bulk",
            json!({ "pattern": pattern, "action": "expire", "ttl": "2h" }),
        )
        .await;
    if redis6(kind) {
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        let e = r.json()["error"].as_str().unwrap().to_string();
        assert!(e.contains("needs Redis 7.0 or later"), "{kind}: {e}");
    } else {
        assert_eq!(r.status, 202, "{kind}: {}", r.body);
        let b = r.json()["bulk"].clone();
        assert_eq!(b["stage"], "counting", "{kind}: {b}");
        let id = b["id"].clone();
        let b = bulk_at(&mut rx, &id, "counted").await;
        assert_eq!(b["count"]["matched"], 1_200, "{kind}: {b}");
        assert_eq!(b["count"]["with_ttl"], 50, "{kind}: {b}");
        assert_eq!(b["count"]["other_pattern"], 20, "{kind}: {b}");
        assert_eq!(b["count"]["to_change"], 1_150, "{kind}: {b}");
        own_words(kind, &b);
        let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
        assert_eq!(r.status, 409, "{kind}: {}", r.body);
        let c = r.json()["confirm"].clone();
        assert_eq!(c["kind"], "ask", "{kind}: {c}");
        assert!(
            c["text"]
                .as_str()
                .unwrap()
                .starts_with("Set a TTL on every key of the pattern cart:{id} on local? "),
            "{kind}: {c}"
        );
        let r = d
            .post(
                &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
                json!({}),
            )
            .await;
        assert_eq!(r.status, 202, "{kind}: {}", r.body);
        let b = bulk_at(&mut rx, &id, "done").await;
        assert_eq!(
            b["lines"][0], "Done: set a TTL of 2h on 1,150 keys of cart:{id}.",
            "{kind}: {b}"
        );
        assert_eq!(b["run"]["changed"], 1_150, "{kind}: {b}");
        assert_eq!(b["run"]["skipped"], 50, "{kind}: {b}");
        own_words(kind, &b);
        for i in [0, 49] {
            let t: i64 = q(&mut admin, &["TTL", &format!("cart:{i}")]).await;
            assert!(t > 7_200, "{kind}: cart:{i} kept its own TTL: {t}");
        }
        for i in [50, 700, 1_199] {
            let t: i64 = q(&mut admin, &["TTL", &format!("cart:{i}")]).await;
            assert!((7_000..=7_200).contains(&t), "{kind}: cart:{i}: {t}");
        }
        let t: i64 = q(&mut admin, &["TTL", "cart:3:items"]).await;
        assert_eq!(t, -1, "{kind}: another pattern's key is left alone");
        let files = key_files(&d.dir.join("bulk"));
        assert_eq!(files.len(), 1, "{kind}: {files:?}");
        let names = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(names.lines().count(), 1_150, "{kind}");
        assert!(
            names
                .lines()
                .all(|l| l.starts_with("cart:") && !l.contains(":items"))
        );
        // Nothing left to change: said, not asked.
        let r = d
            .post(
                "/api/bulk",
                json!({ "pattern": pattern, "action": "expire" }),
            )
            .await;
        let id = r.json()["bulk"]["id"].clone();
        let b = bulk_at(&mut rx, &id, "done").await;
        assert_eq!(
            b["lines"][0], "Every key of cart:{id} (1,200) has a TTL already. Nothing was sent.",
            "{kind}: {b}"
        );
        let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
    }

    // Delete, cancelled at the question: nothing deleted.
    let r = d
        .post(
            "/api/bulk",
            json!({ "pattern": pattern, "action": "delete" }),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    let id = r.json()["bulk"]["id"].clone();
    let b = bulk_at(&mut rx, &id, "counted").await;
    assert_eq!(b["count"]["to_change"], 1_200, "{kind}: {b}");
    assert_eq!(
        b["summary"][0], "Dry run: 1,200 keys of the pattern will be deleted with UNLINK.",
        "{kind}: {b}"
    );
    let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
    let c = r.json()["confirm"].clone();
    assert!(
        c["text"]
            .as_str()
            .unwrap()
            .contains("This can't be undone."),
        "{kind}: {c}"
    );
    let r = d
        .send(
            "DELETE",
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    let b = bulk_at(&mut rx, &id, "done").await;
    assert_eq!(b["lines"][0], "Cancelled: nothing was sent.", "{kind}: {b}");
    assert_eq!(count_keys(&mut admin, "cart:*").await, 1_220, "{kind}");

    // Delete, a dry run stopped: nothing deleted. The stop lands while it counts, or
    // just after its count (a local walk of 1,220 keys is quick): either way it ends at
    // once, in its own words, and nothing was sent.
    let r = d
        .post(
            "/api/bulk",
            json!({ "pattern": pattern, "action": "delete" }),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    let r = d.send("DELETE", "/api/bulk", json!({})).await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    let v = r.json();
    let text = v["text"].as_str().unwrap_or_default().to_string();
    assert!(
        text == "Dry run stopped. Nothing was sent."
            || text == "Bulk action cancelled. Nothing was sent.",
        "{kind}: {v}"
    );
    assert_eq!(v["bulk"]["stage"], "done", "{kind}: {v}");
    assert_eq!(v["bulk"]["lines"], json!([text]), "{kind}: {v}");
    assert_eq!(v["bulk"]["run"], Value::Null, "{kind}: {v}");
    // A count step still on its way is dropped when it lands: the stop stays.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = d.get("/api/bulk").await.json();
    assert_eq!(b["bulk"]["lines"], json!([text]), "{kind}: {b}");
    assert_eq!(count_keys(&mut admin, "cart:*").await, 1_220, "{kind}");

    // Delete, run to its end: exactly the pattern's keys.
    let r = d
        .post(
            "/api/bulk",
            json!({ "pattern": pattern, "action": "delete" }),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    let id = r.json()["bulk"]["id"].clone();
    bulk_at(&mut rx, &id, "counted").await;
    let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
    let c = r.json()["confirm"].clone();
    let r = d
        .post(
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    let b = bulk_at(&mut rx, &id, "done").await;
    assert_eq!(
        b["lines"][0], "Done: deleted 1,200 keys of cart:{id}.",
        "{kind}: {b}"
    );
    assert_eq!(count_keys(&mut admin, "cart:*").await, 20, "{kind}");
    assert_eq!(count_keys(&mut admin, "cart:*:items").await, 20, "{kind}");
    assert_eq!(count_keys(&mut admin, "user:*").await, 10, "{kind}");

    // On stage (a command budget): a stop is honoured exactly. The run is held before
    // each batch (`bulk::hold`, built for the integration tests only), so the stops
    // below land where the test says.
    let mut pipe = redis::pipe();
    for i in 0..4_000 {
        pipe.cmd("SET").arg(format!("job:{i}")).arg("x").ignore();
    }
    pipe.query_async::<()>(&mut admin).await.unwrap();
    // A key in db 1, so the database list offers it.
    cmd(&mut admin, &["SELECT", "1"]).await;
    cmd(&mut admin, &["SET", "other:db", "x"]).await;
    cmd(&mut admin, &["SELECT", "0"]).await;
    d.connect("stage1").await;
    let job = d.open("job:7").await["view"].clone();
    let dbs = d.get("/api/dbs").await;
    assert_eq!(dbs.status, 200, "{kind}: {}", dbs.body);
    let id = job_delete(&d).await;
    let b = bulk_at(&mut rx, &id, "counted").await;
    assert_eq!(b["count"]["matched"], 4_000, "{kind}: {b}");
    let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
    let c = r.json()["confirm"].clone();
    hold::hold(&d.config);
    let r = d
        .post(
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    arrived(&d, 1).await;
    // While it runs: no database switch, analysis, connect, disconnect or key edit
    // (the TUI's bulk dialog takes every key until it ends).
    for (path, body) in [
        ("/api/db", json!({ "db": 1 })),
        ("/api/analysis", json!({ "depth": "quick" })),
        ("/api/connect", json!({ "name": "local" })),
        ("/api/disconnect", json!({})),
        ("/api/value/delete", json!({ "view": job })),
        (
            "/api/keys/add",
            json!({ "name": "job:late", "type": "string", "value": "x" }),
        ),
    ] {
        let r = d.post(path, body).await;
        assert_eq!(r.status, 409, "{kind}: {path}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "A bulk action is running: wait for it to end, or stop it first.",
            "{kind}: {path}"
        );
    }
    let s = d.get("/api/state").await.json();
    assert_eq!(s["connection"]["db"], 0, "{kind}: {s}");
    assert_eq!(s["connection"]["name"], "stage1", "{kind}: {s}");
    assert_eq!(q::<i64>(&mut admin, &["EXISTS", "job:late"]).await, 0);
    // One batch, then a stop between it and the next: no further batch is sent.
    hold::step(&d.config);
    arrived(&d, 2).await;
    let r = d.send("DELETE", "/api/bulk", json!({})).await;
    assert_eq!(r.status, 200, "{kind}: {}", r.body);
    let v = r.json();
    assert_eq!(
        v["text"], "Stopped: no batch was on its way, and no more is sent.",
        "{kind}: {v}"
    );
    assert_eq!(v["bulk"]["stage"], "done", "{kind}: {v}");
    let changed = v["bulk"]["run"]["changed"].as_u64().unwrap();
    assert!(changed > 0 && changed <= 500, "{kind}: one batch: {v}");
    assert_eq!(
        v["bulk"]["lines"][0],
        format!("Stopped after the batch: deleted {changed} keys of job:{{id}}."),
        "{kind}: {v}"
    );
    hold::release(&d.config);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let left = count_keys(&mut admin, "job:*").await as u64;
    assert_eq!(left, 4_000 - changed, "{kind}: the one batch only");
    let b = d.get("/api/bulk").await.json();
    assert_eq!(b["bulk"]["run"]["changed"], changed, "{kind}: {b}");
    let s = d.get("/api/state").await.json();
    assert_eq!(s["connection"]["name"], "stage1", "{kind}: {s}");

    // A stop after the answer, before the first batch: nothing is sent at all.
    let id = job_delete(&d).await;
    bulk_at(&mut rx, &id, "counted").await;
    let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
    let c = r.json()["confirm"].clone();
    hold::hold(&d.config);
    let r = d
        .post(
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    arrived(&d, 1).await;
    let r = d.send("DELETE", "/api/bulk", json!({})).await;
    let v = r.json();
    assert_eq!(
        v["bulk"]["lines"][0], "Stopped before the first batch: nothing was sent.",
        "{kind}: {v}"
    );
    hold::release(&d.config);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        count_keys(&mut admin, "job:*").await as u64,
        left,
        "{kind}: nothing deleted"
    );

    // Not held, stopped as it runs: the batch on its way (if one is) lands, no other.
    let id = job_delete(&d).await;
    bulk_at(&mut rx, &id, "counted").await;
    let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
    let c = r.json()["confirm"].clone();
    let r = d
        .post(
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    let r = d.send("DELETE", "/api/bulk", json!({})).await;
    let v = r.json();
    let at_stop = v["bulk"]["run"]["changed"].as_u64().unwrap();
    let b = if v["text"] == "Stopping after the batch on its way." {
        bulk_at(&mut rx, &id, "done").await
    } else {
        assert_eq!(
            v["text"], "Stopped: no batch was on its way, and no more is sent.",
            "{kind}: {v}"
        );
        v["bulk"].clone()
    };
    let head = b["lines"][0].as_str().unwrap().to_string();
    assert!(head.starts_with("Stopped "), "{kind}: {b}");
    let changed = b["run"]["changed"].as_u64().unwrap();
    assert!(
        changed - at_stop <= 500,
        "{kind}: one batch at most after the stop: {b}"
    );
    assert!(changed < left, "{kind}: {changed} of {left}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let now = count_keys(&mut admin, "job:*").await as u64;
    assert_eq!(now, left - changed, "{kind}: exactly the keys it says");
    let left = now;

    // On prod the pattern is typed.
    d.connect("prod1").await;
    let r = d
        .post(
            "/api/bulk",
            json!({ "pattern": hex("job:{id}"), "action": "delete" }),
        )
        .await;
    let id = r.json()["bulk"]["id"].clone();
    bulk_at(&mut rx, &id, "counted").await;
    let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
    let c = r.json()["confirm"].clone();
    assert_eq!(c["kind"], "type", "{kind}: {c}");
    assert_eq!(c["expect"], "job:{id}", "{kind}: {c}");
    assert!(
        c["text"].as_str().unwrap().starts_with(
            "Type job:{id} to start: Delete every key of the pattern job:{id} on prod1."
        ),
        "{kind}: {c}"
    );
    let r = d
        .post(
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({ "typed": "prod1" }),
        )
        .await;
    assert_eq!(r.status, 422, "{kind}: {}", r.body);
    assert_eq!(count_keys(&mut admin, "job:*").await as u64, left, "{kind}");
    let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
    let c = r.json()["confirm"].clone();
    let r = d
        .post(
            &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
            json!({ "typed": "job:{id}" }),
        )
        .await;
    assert_eq!(r.status, 202, "{kind}: {}", r.body);
    let b = bulk_at(&mut rx, &id, "done").await;
    assert!(
        b["lines"][0]
            .as_str()
            .unwrap()
            .starts_with("Done: deleted "),
        "{kind}: {b}"
    );
    assert_eq!(count_keys(&mut admin, "job:*").await, 0, "{kind}");
    for t in d.seen() {
        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
        own_words(kind, &v);
    }
}

// -- a limited user: NOPERM turns the action off, in redis-lens's words --------------

const LIM_USER: &str = "lens_p3_limited_w8r";
const LIM_PASS: &str = "Pw-p3-limited-5t7y-secret";

#[tokio::test]
async fn a_refused_write_turns_its_action_off_without_naming_the_user() {
    for valkey in [false, true] {
        let kind = if valkey { "valkey" } else { "redis" };
        let pass = format!(">{LIM_PASS}");
        // Without ACL the permission check can't ask ACL DRYRUN: writes are checked by
        // the server on use, so the refusal comes from the write itself.
        let server = common::server_with(
            valkey,
            &[
                "--user", LIM_USER, "on", &pass, "~*", "+@all", "-acl", "-hset", "-unlink",
                "-watch",
            ],
        )
        .await;
        let mut admin = server.admin().await;
        cmd(&mut admin, &["HSET", "user:1", "name", "ann"]).await;
        cmd(&mut admin, &["SET", "str:1", "abc"]).await;
        for i in 0..30 {
            cmd(&mut admin, &["SET", &format!("cart:{i}"), "x"]).await;
        }
        let store = Arc::new(MemoryStore::default());
        let mut p = profile("limited", server.port, Env::Local, None);
        p.auth = Auth::Keychain;
        store
            .set(
                &p.secret_ref(),
                &Credentials {
                    username: Some(LIM_USER.into()),
                    password: secrecy::SecretString::from(LIM_PASS.to_string()),
                },
            )
            .unwrap();
        let d = Dash::start_with(vec![p], &format!("acl-{kind}"), store).await;
        let mut rx = d.events().await;
        d.connect("limited").await;
        // No figures meanwhile: a `state` event after a refusal is the refusal's own.
        let r = d.post("/api/polling", json!({ "paused": true })).await;
        assert_eq!(r.status, 200, "{kind}: {}", r.body);
        let v = d.open("user:1").await;
        let view = v["view"].clone();
        let idx = index_of(&v, "name", "name");
        let r = d
            .post(
                "/api/value/edit",
                json!({ "view": view, "rev": 0, "index": idx, "value": "bob" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "Not written: the server refused HSET for this user.",
            "{kind}"
        );
        // The refusal sends a `state` event at once: no page has to ask for it.
        let refused = |s: &Value, c: &str| {
            s["refused"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x == c))
        };
        let s = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(&mut rx, "state", |s| refused(s, "HSET")),
        )
        .await
        .unwrap_or_else(|_| panic!("{kind}: no state event after the refusal"));
        assert!(refused(&s, "HSET"), "{kind}: {s}");
        // Off for the session from then on: refused before anything is sent.
        let r = d
            .post(
                "/api/value/edit",
                json!({ "view": view, "rev": 0, "index": idx, "value": "bob" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "Not sent: the server refused HSET for this user.",
            "{kind}"
        );
        let s = d.get("/api/state").await.json();
        assert!(
            s["refused"].as_array().unwrap().iter().any(|c| c == "HSET"),
            "{kind}: {s}"
        );
        assert_eq!(
            q::<String>(&mut admin, &["HGET", "user:1", "name"]).await,
            "ann"
        );
        // A string edit needs WATCH too: refused, it is named WATCH, and it turns the
        // edit off as SET, the edit's own command, as the TUI does.
        let sv = d.open("str:1").await;
        let r = d
            .post(
                "/api/value/edit",
                json!({ "view": sv["view"], "rev": 0, "index": 0, "value": "xyz" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "Not written: the server refused WATCH for this user.",
            "{kind}"
        );
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(&mut rx, "state", |s| refused(s, "SET")),
        )
        .await
        .unwrap_or_else(|_| panic!("{kind}: no state event after the WATCH refusal"));
        let r = d
            .post(
                "/api/value/edit",
                json!({ "view": sv["view"], "rev": 0, "index": 0, "value": "xyz" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "Not sent: the server refused SET for this user.",
            "{kind}"
        );
        assert_eq!(q::<String>(&mut admin, &["GET", "str:1"]).await, "abc");
        // Other writes still go.
        let r = d
            .post("/api/value/ttl", json!({ "view": view, "ttl": "1h" }))
            .await;
        done(&r, "ttl");
        // A bulk delete whose UNLINK is refused stops, and UNLINK is off after it.
        let r = d
            .post(
                "/api/bulk",
                json!({ "pattern": hex("cart:{id}"), "action": "delete" }),
            )
            .await;
        assert_eq!(r.status, 202, "{kind}: {}", r.body);
        let id = r.json()["bulk"]["id"].clone();
        bulk_at(&mut rx, &id, "counted").await;
        let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
        let c = r.json()["confirm"].clone();
        let r = d
            .post(
                &format!("/api/confirm/{}", c["id"].as_str().unwrap()),
                json!({}),
            )
            .await;
        assert_eq!(r.status, 202, "{kind}: {}", r.body);
        let b = bulk_at(&mut rx, &id, "done").await;
        assert_eq!(
            b["lines"][0],
            "Stopped: the server refused UNLINK for this user. Before that, deleted 0 keys of cart:{id}.",
            "{kind}: {b}"
        );
        assert_eq!(b["error"], true, "{kind}: {b}");
        assert_eq!(count_keys(&mut admin, "cart:*").await, 30, "{kind}");
        let r = d
            .post(
                "/api/bulk",
                json!({ "pattern": hex("cart:{id}"), "action": "delete" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        assert_eq!(
            r.json()["error"],
            "Not sent: the server refused UNLINK for this user.",
            "{kind}"
        );
        let r = d.post("/api/value/delete", json!({ "view": view })).await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        for t in d.seen() {
            for secret in [LIM_USER, LIM_PASS] {
                assert!(!t.contains(secret), "{kind}: a credential reached {t}");
            }
            let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
            own_words(kind, &v);
        }
        let config = std::fs::read_to_string(&d.config).unwrap();
        assert!(!config.contains(LIM_USER) && !config.contains(LIM_PASS));
    }
}

// -- a cluster -------------------------------------------------------------------------

#[tokio::test]
async fn a_cluster_edits_on_the_keys_owner_and_bulk_deletes_per_primary() {
    for valkey in [false, true] {
        let kind = if valkey { "valkey" } else { "redis" };
        let c = common::cluster::cluster(valkey, false).await;
        let keys: Vec<(String, String)> = (0..300)
            .map(|i| (format!("item:{i}"), "v".to_string()))
            .chain((0..30).map(|i| (format!("{{user{i}}}:name"), "n".to_string())))
            .collect();
        c.seed(&keys).await;
        let client = redis::cluster::ClusterClient::new(vec![c.url(c.seed_port())]).unwrap();
        let mut cl = client.get_async_connection().await.unwrap();
        let d = Dash::start(
            vec![
                profile("cl", c.seed_port(), Env::Local, None),
                profile("cl-ro", c.seed_port(), Env::Local, Some(true)),
            ],
            &format!("cluster-{kind}"),
        )
        .await;
        let mut rx = d.events().await;

        // Read-only on a cluster: no write reaches any node.
        d.connect("cl-ro").await;
        let v = d.open("item:7").await;
        c.reset_stats().await;
        let r = d
            .post(
                "/api/value/edit",
                json!({ "view": v["view"], "rev": 0, "value": "x" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        let r = d
            .post(
                "/api/bulk",
                json!({ "pattern": hex("item:{id}"), "action": "delete" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        for p in &c.ports {
            let stats = c.commandstats(*p).await;
            for w in WRITES {
                assert!(
                    stats.get(*w).is_none_or(|(calls, _)| *calls == 0),
                    "{kind}: {w} reached node {p}: {stats:?}"
                );
            }
        }

        // Edits go to the key's slot owner.
        d.connect("cl").await;
        let v = d.open("item:7").await;
        assert_eq!(v["cluster"], true, "{kind}");
        let r = d
            .post(
                "/api/value/edit",
                json!({ "view": v["view"], "rev": 0, "value": "edited" }),
            )
            .await;
        let w = done(&r, "cluster edit");
        assert!(w["row"]["node"].is_u64(), "{kind}: {w}");
        let got: String = redis::cmd("GET")
            .arg("item:7")
            .query_async(&mut cl)
            .await
            .unwrap();
        assert_eq!(got, "edited", "{kind}");
        let v = d.open("{user1}:name").await;
        let r = d
            .post(
                "/api/value/rename",
                json!({ "view": v["view"], "name": "{user1}:renamed" }),
            )
            .await;
        done(&r, "cluster rename");
        let n: i64 = redis::cmd("EXISTS")
            .arg("{user1}:renamed")
            .query_async(&mut cl)
            .await
            .unwrap();
        assert_eq!(n, 1, "{kind}");
        // Names in two slots: refused before asking.
        let v = d.open("{user2}:name").await;
        let r = d
            .post(
                "/api/value/rename",
                json!({ "view": v["view"], "name": "{user3}:other" }),
            )
            .await;
        assert_eq!(r.status, 422, "{kind}: {}", r.body);
        let r = d
            .post(
                "/api/keys/add",
                json!({ "name": "fresh:1", "type": "list", "value": "e" }),
            )
            .await;
        done(&r, "cluster add key");
        let n: i64 = redis::cmd("LLEN")
            .arg("fresh:1")
            .query_async(&mut cl)
            .await
            .unwrap();
        assert_eq!(n, 1, "{kind}");

        // A bulk delete walks each primary in slot order.
        let r = d
            .post(
                "/api/bulk",
                json!({ "pattern": hex("item:{id}"), "action": "delete" }),
            )
            .await;
        assert_eq!(r.status, 202, "{kind}: {}", r.body);
        let id = r.json()["bulk"]["id"].clone();
        let b = bulk_at(&mut rx, &id, "counted").await;
        assert_eq!(b["count"]["matched"], 300, "{kind}: {b}");
        let nodes = b["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 3, "{kind}: {b}");
        let per: u64 = nodes.iter().map(|n| n["matched"].as_u64().unwrap()).sum();
        assert_eq!(per, 300, "{kind}: {b}");
        assert!(nodes.iter().all(|n| n["matched"].as_u64().unwrap() > 0));
        let r = d.post("/api/bulk/run", json!({ "bulk": id })).await;
        let q2 = r.json()["confirm"].clone();
        assert!(
            q2["text"]
                .as_str()
                .unwrap()
                .contains("It runs on each of the 3 primaries in turn, in slot order."),
            "{kind}: {q2}"
        );
        let r = d
            .post(
                &format!("/api/confirm/{}", q2["id"].as_str().unwrap()),
                json!({}),
            )
            .await;
        assert_eq!(r.status, 202, "{kind}: {}", r.body);
        let b = bulk_at(&mut rx, &id, "done").await;
        assert_eq!(
            b["lines"][0], "Done: deleted 300 keys of item:{id}.",
            "{kind}: {b}"
        );
        assert!(
            b["lines"][1]
                .as_str()
                .unwrap()
                .starts_with("Key names, each after its node and a tab: "),
            "{kind}: {b}"
        );
        for p in &c.ports[..3] {
            let mut a = c.admin(*p).await;
            assert_eq!(count_keys(&mut a, "item:*").await, 0, "{kind}: node {p}");
        }
        let files = key_files(&d.dir.join("bulk"));
        assert_eq!(files.len(), 1, "{kind}: {files:?}");
        let text = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(text.lines().count(), 300, "{kind}");
        assert!(text.lines().all(|l| l.split_once('\t').is_some()), "{kind}");
    }
}
