//! Tests added after the Sentinel verifier's review: fixed error texts, the loopback
//! rule for a Sentinel's replica, and Monitor's address.

// The loopback test needs a fake ssh, so it and its helpers are Unix only.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use super::*;
use crate::conn::tests::{FakeServer, STANDALONE, ep};
use crate::model::{Auth, Credentials};
use crate::secrets::MemoryStore;
use secrecy::SecretString;

const MASTER: &str = "mymaster";
const DATA_PW: &str = "data-pw-q4z";
const DATA_USER: &str = "data-user-q4z";

fn resp_bulk(s: &str) -> String {
    format!("${}\r\n{s}\r\n", s.len())
}

fn resp_addr(host: &str, port: u16) -> String {
    format!("*2\r\n{}{}", resp_bulk(host), resp_bulk(&port.to_string()))
}

async fn closed_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

/// A Sentinel profile asking the Sentinel at `sentinel_ip`:`sentinel_port`, whose own
/// `endpoint` is `endpoint_port` (which must never be dialled).
fn profile(sentinel_ip: &str, sentinel_port: u16, endpoint_port: u16) -> Profile {
    let mut p = crate::conn::ssh::tests::profile(None);
    p.name = "watched".into();
    p.endpoint = ep("127.0.0.1", endpoint_port, false);
    p.sentinel = Some(Sentinel {
        master: MASTER.into(),
        nodes: vec![SentinelNode {
            host: sentinel_ip.into(),
            port: sentinel_port,
        }],
        tls: false,
    });
    p
}

fn keychain(p: &mut Profile) -> MemoryStore {
    p.auth = Auth::Keychain;
    let store = MemoryStore::default();
    store
        .set(
            &p.secret_ref(),
            &Credentials {
                username: Some(DATA_USER.into()),
                password: SecretString::from(DATA_PW),
            },
        )
        .unwrap();
    store
}

fn master_info() -> Vec<(&'static str, String)> {
    vec![
        ("server", STANDALONE.to_string()),
        (
            "replication",
            "role:master\r\nconnected_slaves:0\r\nmaster_replid:abc123\r\n".into(),
        ),
    ]
}

/// A primary that fails with a server error naming a user, or is not there: the error
/// is a fixed text, never the server's.
#[tokio::test]
async fn a_primary_error_is_a_fixed_text() {
    let mut info = master_info();
    info.push(("reply:select", "-ERR something with secretuser\r\n".into()));
    let primary = FakeServer::start(&info).await;
    let s = FakeServer::start(&[(
        "reply:sentinel get-master-addr-by-name",
        resp_addr("127.0.0.1", primary.addr.port()),
    )])
    .await;
    let mut p = profile("127.0.0.1", s.addr.port(), closed_port().await);
    p.db = 3;
    let e = crate::conn::connect(&p, &MemoryStore::default())
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        SentinelError::PrimaryFailed(format!("127.0.0.1:{}", primary.addr.port())).to_string()
    );
    assert!(!e.to_string().contains("secretuser"), "{e}");

    let dead = closed_port().await;
    let s = FakeServer::start(&[(
        "reply:sentinel get-master-addr-by-name",
        resp_addr("127.0.0.1", dead),
    )])
    .await;
    let p = profile("127.0.0.1", s.addr.port(), closed_port().await);
    let e = crate::conn::connect(&p, &MemoryStore::default())
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        SentinelError::PrimaryFailed(format!("127.0.0.1:{dead}")).to_string()
    );
}

/// Monitor dials the primary the Sentinel named, never `profile.endpoint` (a closed
/// port here), and sends `MONITOR` to it twice: the check and the stream.
#[tokio::test]
async fn monitor_reaches_the_primary_the_sentinel_named() {
    let primary = FakeServer::start(&master_info()).await;
    let s = FakeServer::start(&[(
        "reply:sentinel get-master-addr-by-name",
        resp_addr("127.0.0.1", primary.addr.port()),
    )])
    .await;
    let p = profile("127.0.0.1", s.addr.port(), closed_port().await);
    let budget = crate::budget::Budget::new(None);
    let m = crate::conn::open_monitor(&p, &MemoryStore::default(), &budget, None)
        .await
        .unwrap();
    assert!(m.is_ok());
    assert_eq!(primary.count("MONITOR"), 2, "{:?}", primary.commands);
}

/// The loopback rule for a Sentinel's replica is judged against the primary the
/// Sentinel named, not the candidate: with a remote primary (`primary.example`, behind
/// a fake ssh that maps each host name to a fake server), a candidate on 127.0.0.1 that
/// answers `NOAUTH` gets no credentials.
#[cfg(unix)]
#[tokio::test]
async fn a_loopback_replica_of_a_remote_primary_gets_no_credentials() {
    use crate::conn::ssh;
    const PROXY: &str = r#"
use strict; use warnings; use IO::Socket::INET; use IO::Select;
my %to = map { split /=/ } split /,/, shift @ARGV;
my ($spec) = map { $ARGV[$_ + 1] } grep { $ARGV[$_] eq '-L' } 0 .. $#ARGV;
my (undef, $port, $host) = split /:/, $spec;
my $target = $to{$host} or exit 255;
my $l = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => $port, Listen => 16, ReuseAddr => 1) or exit 255;
$SIG{CHLD} = 'IGNORE';
while (my $c = $l->accept) {
  my $pid = fork();
  if (defined $pid && $pid == 0) {
    my $up = IO::Socket::INET->new(PeerAddr => '127.0.0.1', PeerPort => $target) or exit 0;
    my $sel = IO::Select->new($c, $up);
    while (my @r = $sel->can_read) {
      for my $fh (@r) {
        my $n = sysread($fh, my $buf, 65536);
        exit 0 unless $n;
        syswrite($fh == $c ? $up : $c, $buf) or exit 0;
      }
    }
    exit 0;
  }
  close $c;
}
"#;
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-sentinel-m5-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("proxy.pl");
    std::fs::write(&script, PROXY).unwrap();

    let replica = FakeServer::start(&[
        ("server", STANDALONE.to_string()),
        (
            "replication",
            "-NOAUTH Authentication required.".to_string(),
        ),
    ])
    .await;
    let primary = FakeServer::start(&master_info()).await;
    let entry = [
        "ip",
        "127.0.0.1",
        "port",
        "6380",
        "flags",
        "slave",
        "master-link-status",
        "ok",
    ];
    let body: String = entry.iter().map(|f| resp_bulk(f)).collect();
    let s = FakeServer::start(&[
        (
            "reply:sentinel get-master-addr-by-name",
            resp_addr("primary.example", 6379),
        ),
        (
            "reply:sentinel replicas",
            format!("*1\r\n*{}\r\n{body}", entry.len()),
        ),
    ])
    .await;
    let t = ssh::tests::tunnel("b.example");
    let mut p = profile("sentinel.example", 26379, closed_port().await);
    p.ssh = Some(t.clone());
    p.prefer_replica = true;
    let store = keychain(&mut p);
    let map = format!(
        "sentinel.example={},primary.example={},127.0.0.1={}",
        s.addr.port(),
        primary.addr.port(),
        replica.addr.port()
    );
    let bastion = ssh::Bastion::new(&p, &t, &store).unwrap().with_launcher(
        vec!["perl".into(), script.clone().into(), map.into()],
        ssh::READY_TIMEOUT,
    );
    let route = Arc::new(Route::Ssh(bastion));
    let auth = ResolvedAuth::new(&p, &store).await.unwrap();
    let sentinel = p.sentinel.clone().unwrap();
    let primary_conn = open_primary(&p, &sentinel, &store, &route, &auth)
        .await
        .unwrap_or_else(|e| panic!("the remote primary was not opened: {e}"));
    let (_, target) = replica_step(&p, &route, &auth, primary_conn).await;
    let ScanTarget::PrimaryFallback(why) = target else {
        panic!("the loopback replica was used: {target:?}");
    };
    assert!(why.contains("loopback address"), "{why}");
    let sent = replica.commands.lock().unwrap().join("\n");
    assert!(sent.contains("INFO replication"), "{sent}");
    for word in ["AUTH", DATA_PW, DATA_USER, "HELLO"] {
        assert!(!sent.contains(word), "{word} reached the replica: {sent}");
    }
    let to_primary = primary.commands.lock().unwrap().join("\n");
    assert!(
        to_primary.contains(&format!("AUTH {DATA_USER} {DATA_PW}")),
        "{to_primary}"
    );
    drop(route);
    let _ = std::fs::remove_dir_all(&dir);
}
