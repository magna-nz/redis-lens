//! `redis-lens --dashboard` against the built binary: it prints the one line with the
//! sign-in URL, the URL signs a browser in, a second run on the same port fails with a
//! fixed message, and ctrl-c (SIGINT) stops it cleanly. No Redis is needed: the
//! dashboard starts without a connection.
#![cfg(feature = "integration-tests")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-dashboard-cli-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A port on 127.0.0.1 nothing listens on right now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn command(config: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_redis-lens"));
    cmd.args(args)
        .env("REDIS_LENS_CONFIG", config)
        .stdin(Stdio::null());
    cmd
}

/// The running `--dashboard`, its sign-in URL's port and token, and the rest of its
/// standard output as it comes.
struct Running {
    child: Child,
    port: u16,
    token: String,
    lines: mpsc::Receiver<String>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(config: &Path, port: u16) -> Running {
    let mut child = command(
        config,
        &["--dashboard", "--no-open", "--port", &port.to_string()],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let first = lines
        .recv_timeout(Duration::from_secs(30))
        .expect("the Dashboard: line");
    let url = first
        .strip_prefix("Dashboard: ")
        .unwrap_or_else(|| panic!("not the Dashboard: line: {first:?}"));
    let rest = url
        .strip_prefix(&format!("http://127.0.0.1:{port}/auth/"))
        .unwrap_or_else(|| panic!("not a sign-in URL on port {port}: {url}"));
    assert_eq!(rest.len(), 64, "a 32-byte hex token");
    assert!(rest.chars().all(|c| c.is_ascii_hexdigit()), "{rest}");
    let second = lines.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(second, "Press ctrl-c to stop.");
    Running {
        child,
        port,
        token: rest.to_string(),
        lines,
    }
}

/// One HTTP/1.1 GET on its own connection: the status code and the head and body.
fn get(port: u16, path: &str, cookie: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(c) = cookie {
        req.push_str(&format!("Cookie: {c}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let code = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {text:?}"));
    (code, text)
}

fn header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    let head = response.split("\r\n\r\n").next()?;
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn run(config: &Path, args: &[&str]) -> Output {
    command(config, args).output().unwrap()
}

#[test]
fn dashboard_signs_in_refuses_a_taken_port_and_stops_on_kill() {
    let dir = temp_dir("serve");
    let config = dir.join("config.toml");
    let port = free_port();
    let mut d = start(&config, port);

    // Without the cookie the API answers 401.
    let (code, _) = get(d.port, "/api/state", None);
    assert_eq!(code, 401);
    // The sign-in URL: 303 to `/`, with the session cookie.
    let (code, resp) = get(d.port, &format!("/auth/{}", d.token), None);
    assert_eq!(code, 303, "{resp}");
    assert_eq!(header(&resp, "location"), Some("/"), "{resp}");
    let cookie = header(&resp, "set-cookie").expect("a cookie");
    assert!(
        cookie.starts_with(&format!("rl_session={};", d.token)),
        "{cookie}"
    );
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(cookie.contains(attr), "{attr} in {cookie}");
    }
    let session = format!("rl_session={}", d.token);
    let (code, resp) = get(d.port, "/api/state", Some(&session));
    assert_eq!(code, 200, "{resp}");
    // A wrong token signs nothing in.
    let (code, _) = get(d.port, "/api/state", Some("rl_session=00"));
    assert_eq!(code, 401);

    // A second run on the same port fails with the fixed message, before printing a URL.
    let second = run(
        &config,
        &["--dashboard", "--no-open", "--port", &port.to_string()],
    );
    assert_eq!(second.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(!stdout.contains("Dashboard:"), "{stdout}");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert_eq!(
        stderr.trim_end(),
        format!(
            "Error: Port {port} on 127.0.0.1 is in use, so the dashboard did not start. Pick another with --port."
        )
    );

    // Nothing else is printed while it serves: the token is on the first line only.
    assert!(d.lines.try_recv().is_err());
    d.child.kill().unwrap();
    assert!(wait_exit(&mut d.child, Duration::from_secs(10)).is_some());
    std::fs::remove_dir_all(&dir).ok();
}

/// ctrl-c stops it: exit code 0, and the port is free again.
#[cfg(unix)]
#[test]
fn ctrl_c_stops_the_dashboard_cleanly() {
    let dir = temp_dir("sigint");
    let config = dir.join("config.toml");
    let port = free_port();
    let mut d = start(&config, port);
    // An event stream open at the time does not keep it running.
    let session = format!("rl_session={}", d.token);
    let mut events = TcpStream::connect(("127.0.0.1", port)).unwrap();
    events
        .write_all(
            format!(
                "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {session}\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let status = Command::new("kill")
        .args(["-INT", &d.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let exit = wait_exit(&mut d.child, Duration::from_secs(15)).expect("it stops on ctrl-c");
    assert_eq!(exit.code(), Some(0), "{exit:?}");
    let mut err = String::new();
    d.child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert_eq!(err, "", "nothing on standard error");
    assert!(
        TcpListener::bind(("127.0.0.1", port)).is_ok(),
        "the port is free"
    );
    drop(events);
    std::fs::remove_dir_all(&dir).ok();
}

/// The flags are for the TUI and the dashboard: with another subcommand they are a
/// usage error in redis-lens's words, and `--terminal` with `--dashboard` is refused.
#[test]
fn dashboard_flags_with_a_subcommand_are_refused() {
    let dir = temp_dir("usage");
    let config = dir.join("config.toml");
    let out = run(&config, &["--dashboard", "--no-open", "conn", "list"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim_end(),
        "Error: --dashboard is for the terminal UI and the dashboard, not for `conn`: leave it out, or run redis-lens without a subcommand."
    );
    assert!(out.stdout.is_empty());
    let out = run(&config, &["--dashboard", "--terminal"]);
    assert_eq!(out.status.code(), Some(2), "clap's own usage error");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot be used with"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(&dir).ok();
}
