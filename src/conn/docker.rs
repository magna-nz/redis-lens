//! Finding Redis and Valkey containers in Docker.
//!
//! Owner: connections.

use bollard::Docker;
use bollard::models::{ContainerSummary, PortSummaryTypeEnum};
use bollard::query_parameters::{InspectContainerOptionsBuilder, ListContainersOptionsBuilder};
use secrecy::SecretString;

use crate::model::{Auth, Credentials, Endpoint, Env, Profile};

use super::ConnError;

/// How long discovery waits for the `PING` that tells whether a server needs a password.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// What a credential-less `PING` says about authentication. A pure function of the
/// probe result: `PONG` means none; `NOAUTH`, `WRONGPASS` or an authentication failure
/// means credentials are required; anything else (refused, timeout, `NOPERM`, other
/// errors) means unknown.
pub fn classify_probe(r: &Result<String, ConnError>) -> DiscoveredAuth {
    match r {
        Ok(reply) if reply.eq_ignore_ascii_case("PONG") => DiscoveredAuth::None,
        Ok(_) => DiscoveredAuth::Unknown,
        Err(ConnError::Redis(e))
            if matches!(e.code(), Some("NOAUTH" | "WRONGPASS"))
                || e.kind() == redis::ErrorKind::AuthenticationFailed =>
        {
            DiscoveredAuth::Required
        }
        Err(_) => DiscoveredAuth::Unknown,
    }
}

/// Probe `ep` with `PING` and no credentials, and classify the result.
pub async fn probe_auth(ep: &Endpoint) -> DiscoveredAuth {
    probe_auth_within(ep, PROBE_TIMEOUT).await
}

/// [`probe_auth`] with its own timeout (tests use a longer one, so a loaded machine
/// doesn't turn an answer into `Unknown`).
pub async fn probe_auth_within(ep: &Endpoint, timeout: std::time::Duration) -> DiscoveredAuth {
    classify_probe(&super::ping_without_credentials(ep, timeout).await)
}

/// What discovery could tell about a container's authentication.
///
/// Discovery-only: a saved [`Profile`] still has [`Auth`], which is what goes to the
/// config file. "Unknown" is not a way to authenticate, so it never reaches `Profile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveredAuth {
    /// A password was found in the container's command line or environment.
    Password,
    /// The server answered without credentials.
    None,
    /// The server wants credentials that discovery could not find (an ACL file, a
    /// config file).
    Required,
    /// Discovery could not tell.
    Unknown,
}

impl DiscoveredAuth {
    /// The word shown in `discover` output and the Connections view.
    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::None => "none",
            Self::Required => "required",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredInstance {
    pub container: String,
    pub image: String,
    /// `env` is `Local`, `auth` is `Keychain` when a password was found, else `None`.
    pub profile: Profile,
    /// A password read from the container's command line (`--requirepass`) or
    /// environment (`REDIS_PASSWORD`). Kept in memory only.
    pub credentials: Option<Credentials>,
    /// What discovery found out about authentication. `Password` exactly when
    /// `credentials` is `Some`.
    pub auth: DiscoveredAuth,
}

const UNREACHABLE: &str = "Docker isn't running or can't be reached";

/// True for images of redis, valkey and redis-stack, e.g. `redis:7.4-alpine`,
/// `valkey/valkey:8.1-alpine`, `bitnami/redis:7.2`, `docker.io/library/redis`. Not for
/// look-alikes such as `redis-commander` or `myredisapp`.
pub fn is_redis_image(image: &str) -> bool {
    let no_digest = image.split('@').next().unwrap_or(image);
    let last = no_digest.rsplit('/').next().unwrap_or(no_digest);
    let repo = last.split(':').next().unwrap_or(last).to_ascii_lowercase();
    matches!(
        repo.as_str(),
        "redis" | "valkey" | "redis-stack" | "redis-stack-server"
    )
}

/// The host port published for container port 6379/tcp.
fn host_port(ports: &[bollard::models::PortSummary]) -> Option<u16> {
    ports
        .iter()
        .filter(|p| p.private_port == 6379)
        .filter(|p| {
            matches!(
                p.typ,
                None | Some(PortSummaryTypeEnum::TCP) | Some(PortSummaryTypeEnum::EMPTY)
            )
        })
        .find_map(|p| p.public_port)
}

/// A password from `--requirepass <pw>` in the command args, else `REDIS_PASSWORD` or
/// `VALKEY_PASSWORD` in the environment (`KEY=value` entries).
pub fn find_password(args: &[String], env: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--requirepass" {
            if let Some(v) = it.next().filter(|v| !v.is_empty()) {
                return Some(v.clone());
            }
        } else if let Some(v) = a.strip_prefix("--requirepass=")
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    ["REDIS_PASSWORD", "VALKEY_PASSWORD"]
        .iter()
        .find_map(|key| {
            env.iter()
                .filter_map(|e| e.split_once('='))
                .find(|(k, v)| k == key && !v.is_empty())
                .map(|(_, v)| v.to_string())
        })
}

fn profile_for(name: String, port: u16, password: bool) -> Profile {
    Profile {
        name,
        env: Env::Local,
        endpoint: Endpoint {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        },
        db: 0,
        auth: if password { Auth::Keychain } else { Auth::None },
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
    }
}

/// Running containers whose image is redis, valkey or redis-stack and that publish
/// container port 6379 on the host.
pub async fn discover() -> Result<Vec<DiscoveredInstance>, ConnError> {
    let unreachable = || ConnError::Docker(UNREACHABLE.to_string());
    let docker = Docker::connect_with_local_defaults().map_err(|_| unreachable())?;
    let list = docker
        .list_containers(Some(ListContainersOptionsBuilder::new().build()))
        .await
        .map_err(|_| unreachable())?;

    let mut found = discover_with(list, |c| instance(&docker, c)).await;
    found.sort_by(|a, b| a.container.cmp(&b.container));
    Ok(found)
}

/// Run `f` on every container at once and keep the `Some` results in the order of `list`.
/// Probes that each wait out a timeout therefore cost one timeout in total, not one each.
async fn discover_with<T, Fut>(
    list: Vec<ContainerSummary>,
    f: impl Fn(ContainerSummary) -> Fut,
) -> Vec<T>
where
    Fut: std::future::Future<Output = Option<T>>,
{
    futures::future::join_all(list.into_iter().map(f))
        .await
        .into_iter()
        .flatten()
        .collect()
}

async fn instance(docker: &Docker, c: ContainerSummary) -> Option<DiscoveredInstance> {
    let image = c.image.clone()?;
    if !is_redis_image(&image) {
        return None;
    }
    let port = host_port(c.ports.as_deref()?)?;
    let id = c.id.clone()?;
    let name = c
        .names
        .as_ref()
        .and_then(|n| n.first())
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_else(|| id.chars().take(12).collect());

    // A failed inspect only means we cannot read a password; keep the container.
    let password = match docker
        .inspect_container(&id, Some(InspectContainerOptionsBuilder::new().build()))
        .await
    {
        Ok(info) => {
            let mut args = info.args.unwrap_or_default();
            let mut env = Vec::new();
            if let Some(cfg) = info.config {
                args.extend(cfg.cmd.unwrap_or_default());
                env = cfg.env.unwrap_or_default();
            }
            find_password(&args, &env)
        }
        Err(_) => None,
    };
    let profile = profile_for(name.clone(), port, password.is_some());
    // A found password needs no probe; otherwise ask the server with a bare PING.
    let auth = if password.is_some() {
        DiscoveredAuth::Password
    } else {
        probe_auth(&profile.endpoint).await
    };
    Some(DiscoveredInstance {
        auth,
        container: name,
        image,
        profile,
        credentials: password.map(|pw| Credentials {
            username: None,
            password: SecretString::from(pw),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovery_probes_run_concurrently_and_keep_order() {
        let list: Vec<ContainerSummary> = (0..8)
            .map(|i| ContainerSummary {
                id: Some(i.to_string()),
                ..Default::default()
            })
            .collect();
        let start = std::time::Instant::now();
        let got = discover_with(list, |c| async move {
            let id: u64 = c.id.unwrap().parse().unwrap();
            // Earlier containers take longer, so completion order is the reverse of list order.
            tokio::time::sleep(std::time::Duration::from_millis(400 - id * 20)).await;
            (id % 4 != 3).then_some(id)
        })
        .await;
        let took = start.elapsed();
        assert_eq!(got, vec![0, 1, 2, 4, 5, 6]);
        // Sequentially this is over 2 s; concurrently it is about 400 ms.
        assert!(
            took < std::time::Duration::from_millis(1200),
            "took {took:?}"
        );
    }

    #[test]
    fn discovered_auth_labels() {
        assert_eq!(DiscoveredAuth::Password.label(), "password");
        assert_eq!(DiscoveredAuth::None.label(), "none");
        assert_eq!(DiscoveredAuth::Required.label(), "required");
        assert_eq!(DiscoveredAuth::Unknown.label(), "unknown");
    }

    /// The error redis-rs builds from a server reply such as `-NOAUTH Authentication required.`
    fn server_reply(line: &str) -> ConnError {
        let v = redis::parse_redis_value(format!("-{line}\r\n").as_bytes()).unwrap();
        match v {
            redis::Value::ServerError(e) => ConnError::Redis(redis::RedisError::from(e)),
            other => panic!("not an error reply: {other:?}"),
        }
    }

    #[test]
    fn probe_classification() {
        let c = |r: Result<String, ConnError>| classify_probe(&r);
        assert_eq!(c(Ok("PONG".into())), DiscoveredAuth::None);
        assert_eq!(c(Ok("something else".into())), DiscoveredAuth::Unknown);
        for line in [
            "NOAUTH Authentication required.",
            "WRONGPASS invalid username-password pair or user is disabled.",
        ] {
            assert_eq!(
                c(Err(server_reply(line))),
                DiscoveredAuth::Required,
                "{line}"
            );
        }
        let auth_failed = ConnError::Redis(redis::RedisError::from((
            redis::ErrorKind::AuthenticationFailed,
            "auth",
        )));
        assert_eq!(c(Err(auth_failed)), DiscoveredAuth::Required);
        assert_eq!(
            c(Err(server_reply("NOPERM this user has no permissions"))),
            DiscoveredAuth::Unknown
        );
        assert_eq!(
            c(Err(server_reply("ERR something"))),
            DiscoveredAuth::Unknown
        );
        let io = |k| ConnError::Redis(redis::RedisError::from(std::io::Error::new(k, "x")));
        assert_eq!(
            c(Err(io(std::io::ErrorKind::ConnectionRefused))),
            DiscoveredAuth::Unknown
        );
        assert_eq!(
            c(Err(io(std::io::ErrorKind::TimedOut))),
            DiscoveredAuth::Unknown
        );
        assert_eq!(
            c(Err(ConnError::Docker("x".into()))),
            DiscoveredAuth::Unknown
        );
    }

    #[test]
    fn image_matching() {
        for yes in [
            "redis:7.4-alpine",
            "valkey/valkey:8.1-alpine",
            "redis/redis-stack-server:latest",
            "bitnami/redis:7.2",
            "docker.io/library/redis",
            "localhost:5000/redis:7",
            "redis@sha256:abcd",
        ] {
            assert!(is_redis_image(yes), "{yes}");
        }
        for no in [
            "redis-commander",
            "redis-commander:latest",
            "redislabs/redisinsight:latest",
            "redisinsight",
            "myredisapp",
            "nginx:1.27",
            "",
        ] {
            assert!(!is_redis_image(no), "{no}");
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn password_from_args() {
        assert_eq!(
            find_password(&s(&["redis-server", "--requirepass", "pw1"]), &[]).as_deref(),
            Some("pw1")
        );
        assert_eq!(
            find_password(&s(&["--requirepass=pw2"]), &[]).as_deref(),
            Some("pw2")
        );
    }

    #[test]
    fn password_from_env() {
        assert_eq!(
            find_password(&[], &s(&["PATH=/bin", "REDIS_PASSWORD=pw3"])).as_deref(),
            Some("pw3")
        );
        assert_eq!(
            find_password(&[], &s(&["VALKEY_PASSWORD=pw4"])).as_deref(),
            Some("pw4")
        );
        assert_eq!(find_password(&[], &s(&["REDIS_PASSWORD="])), None);
        assert_eq!(find_password(&s(&["redis-server"]), &s(&["A=b"])), None);
    }

    #[test]
    fn args_win_over_env() {
        assert_eq!(
            find_password(&s(&["--requirepass", "a"]), &s(&["REDIS_PASSWORD=b"])).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn host_port_picks_mapped_6379_tcp() {
        use bollard::models::PortSummary;
        let p = |private, public| PortSummary {
            ip: None,
            private_port: private,
            public_port: public,
            typ: Some(PortSummaryTypeEnum::TCP),
        };
        assert_eq!(
            host_port(&[p(6379, None), p(16379, Some(1)), p(6379, Some(49153))]),
            Some(49153)
        );
        assert_eq!(host_port(&[p(6379, None)]), None);
    }
}
