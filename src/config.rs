//! Saved connection profiles, stored as TOML. Never holds credentials.
//!
//! Owner: connections.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Profile;
pub use crate::write::Settings;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("no profile named {0}")]
    NotFound(String),
    #[error("a profile named {0} already exists")]
    Exists(String),
    #[error("could not find a config directory for this user")]
    NoConfigDir,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// `[settings]`: absent from the file while every setting is at its default.
    #[serde(default, skip_serializing_if = "Settings::is_default")]
    pub settings: Settings,
    #[serde(default, rename = "profile")]
    pub profiles: Vec<Profile>,
}

impl Config {
    /// `<config dir>/redis-lens/config.toml`, e.g. `~/Library/Application Support/redis-lens/config.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let dirs =
            directories::ProjectDirs::from("", "", "redis-lens").ok_or(ConfigError::NoConfigDir)?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// A missing file is an empty config, not an error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Writes atomically (temp file and rename), creating the directory if needed.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let werr = |source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        };
        let text = toml::to_string_pretty(self).map_err(|e| {
            werr(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))
        })?;
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir).map_err(werr)?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".to_string());
        let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
        let result = (|| {
            std::fs::write(&tmp, text.as_bytes())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            }
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(werr(e));
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    /// Fails with [`ConfigError::Exists`] unless `replace` is true.
    pub fn add(&mut self, profile: Profile, replace: bool) -> Result<(), ConfigError> {
        match self.profiles.iter().position(|p| p.name == profile.name) {
            Some(_) if !replace => Err(ConfigError::Exists(profile.name)),
            Some(i) => {
                self.profiles[i] = profile;
                Ok(())
            }
            None => {
                self.profiles.push(profile);
                Ok(())
            }
        }
    }

    /// Turns writes on or off for profile `name` (spec "Write permissions": `w`,
    /// `conn set --writes`): `read_only` is set to `true` or `false`, and nothing else
    /// changes. Returns the profile as changed.
    pub fn set_writes(&mut self, name: &str, on: bool) -> Result<Profile, ConfigError> {
        let p = self
            .profiles
            .iter_mut()
            .find(|p| p.name == name)
            .ok_or_else(|| ConfigError::NotFound(name.to_string()))?;
        p.read_only = Some(!on);
        Ok(p.clone())
    }

    pub fn remove(&mut self, name: &str) -> Result<Profile, ConfigError> {
        match self.profiles.iter().position(|p| p.name == name) {
            Some(i) => Ok(self.profiles.remove(i)),
            None => Err(ConfigError::NotFound(name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Auth, Endpoint, Env};

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "redis-lens-config-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn profile(name: &str, port: u16) -> Profile {
        Profile {
            name: name.into(),
            env: Env::Stage,
            endpoint: Endpoint {
                host: "cache.example.com".into(),
                port,
                tls: true,
            },
            db: 2,
            auth: Auth::Keychain,
            read_only: Some(true),
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
        }
    }

    #[test]
    fn round_trip_creates_directory() {
        let dir = temp_dir("rt");
        let path = dir.join("nested").join("config.toml");
        let mut c = Config::default();
        c.add(profile("a", 1), false).unwrap();
        c.add(profile("b", 2), false).unwrap();
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[profile]]"), "{text}");
        assert_eq!(Config::load(&path).unwrap(), c);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // no temp files left behind
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = temp_dir("missing");
        let c = Config::load(&dir.join("nope.toml")).unwrap();
        assert!(c.profiles.is_empty());
    }

    #[test]
    fn bad_toml_is_parse_error() {
        let dir = temp_dir("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[[profile]\nname = ").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Parse { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_replace_remove() {
        let mut c = Config::default();
        c.add(profile("a", 1), false).unwrap();
        assert!(matches!(
            c.add(profile("a", 9), false),
            Err(ConfigError::Exists(n)) if n == "a"
        ));
        assert_eq!(c.get("a").unwrap().endpoint.port, 1);
        c.add(profile("a", 9), true).unwrap();
        assert_eq!(c.profiles.len(), 1);
        assert_eq!(c.get("a").unwrap().endpoint.port, 9);
        assert!(c.get("zzz").is_none());
        assert_eq!(c.remove("a").unwrap().name, "a");
        assert!(matches!(c.remove("a"), Err(ConfigError::NotFound(_))));
    }

    #[test]
    fn token_command_and_provider_round_trip() {
        use crate::model::Provider;
        let dir = temp_dir("token");
        let path = dir.join("config.toml");
        let mut p = profile("az", 10000);
        p.auth = Auth::Command;
        p.provider = Some(Provider::Azure);
        p.token_command = Some(vec![
            "az".into(),
            "account".into(),
            "get-access-token".into(),
            "--query".into(),
            "accessToken".into(),
        ]);
        let mut c = Config::default();
        c.add(p.clone(), false).unwrap();
        c.add(profile("plain", 1), false).unwrap();
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#"auth = "command""#), "{text}");
        assert!(text.contains(r#"provider = "azure""#), "{text}");
        assert!(
            text.contains(r#""az","#) || text.contains(r#""az""#),
            "the argv is stored as an array: {text}"
        );
        // Absent for the profile without them.
        assert_eq!(text.matches("token_command").count(), 1, "{text}");
        assert_eq!(text.matches("provider").count(), 1, "{text}");
        assert_eq!(Config::load(&path).unwrap(), c);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_files_without_the_new_fields_load() {
        let dir = temp_dir("old");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[[profile]]\nname = \"x\"\nauth = \"keychain\"\n[profile.endpoint]\nhost = \"h\"\nport = 6379\n",
        )
        .unwrap();
        let c = Config::load(&path).unwrap();
        let p = c.get("x").unwrap();
        assert_eq!(p.provider, None);
        assert_eq!(p.token_command, None);
        assert_eq!(p.ssh, None);
        assert_eq!(p.sentinel, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `[profile.sentinel]` round-trips with its nodes as `host:port` strings (IPv6 in
    /// brackets), `tls` is absent when false, and the file holds no Sentinel
    /// credential: those live in the keychain under `sentinel:profile:<name>`.
    #[test]
    fn sentinel_round_trip() {
        use crate::model::{Sentinel, SentinelNode};
        let dir = temp_dir("sentinel");
        let path = dir.join("config.toml");
        let node = |h: &str, port: u16| SentinelNode {
            host: h.into(),
            port,
        };
        let mut s = profile("watched", 6379);
        s.sentinel = Some(Sentinel {
            master: "mymaster".into(),
            nodes: vec![node("10.0.0.1", 26379), node("2001:db8::7", 5000)],
            tls: false,
        });
        let mut t = profile("tls", 6380);
        t.sentinel = Some(Sentinel {
            master: "other".into(),
            nodes: vec![node("sentinel.example.com", 26380)],
            tls: true,
        });
        let mut c = Config::default();
        c.add(s, false).unwrap();
        c.add(t, false).unwrap();
        c.add(profile("plain", 1), false).unwrap();
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[profile.sentinel]").count(), 2, "{text}");
        for node in [r#""10.0.0.1:26379""#, r#""[2001:db8::7]:5000""#] {
            assert!(text.contains(node), "{text}");
        }
        // Three endpoints with TLS, and the one Sentinel table with it.
        assert_eq!(text.matches("tls = true").count(), 4, "{text}");
        for word in ["user", "password", "secret"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
        let back = Config::load(&path).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.get("plain").unwrap().sentinel, None);
        // Written by hand: the port left out is 26379, and tls is false.
        std::fs::write(
            &path,
            "[[profile]]\nname = \"x\"\n[profile.endpoint]\nhost = \"10.0.0.5\"\nport = 6379\n[profile.sentinel]\nmaster = \"m\"\nnodes = [\"s1\", \"[::1]\"]\n",
        )
        .unwrap();
        let p = Config::load(&path).unwrap().get("x").unwrap().clone();
        let st = p.sentinel.unwrap();
        assert_eq!(st.nodes, vec![node("s1", 26379), node("::1", 26379)]);
        assert!(!st.tls);
        // A node that can't be read fails the load, without repeating it.
        std::fs::write(
            &path,
            "[[profile]]\nname = \"x\"\n[profile.endpoint]\nhost = \"h\"\nport = 6379\n[profile.sentinel]\nmaster = \"m\"\nnodes = [\"s1:0\"]\n",
        )
        .unwrap();
        assert!(Config::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The bastion round-trips through the file with every field, and the file holds no
    /// user: the SSH username lives in the keychain.
    #[test]
    fn ssh_tunnel_round_trip() {
        use crate::model::SshTunnel;
        let dir = temp_dir("ssh");
        let path = dir.join("config.toml");
        let mut full = profile("behind", 6379);
        full.ssh = Some(SshTunnel {
            host: "bastion.example.com".into(),
            port: Some(2222),
            identity_file: Some("/home/me/.ssh/id_ed25519".into()),
            known_hosts_file: Some("/home/me/Library/Application Support/kh".into()),
        });
        let mut v6 = profile("v6", 6380);
        v6.ssh = Some(SshTunnel {
            host: "2001:db8::1".into(),
            port: None,
            identity_file: None,
            known_hosts_file: None,
        });
        let mut c = Config::default();
        c.add(full, false).unwrap();
        c.add(v6, false).unwrap();
        c.add(profile("plain", 1), false).unwrap();
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[profile.ssh]").count(), 2, "{text}");
        assert!(text.contains(r#"host = "bastion.example.com""#), "{text}");
        assert!(text.contains("port = 2222"), "{text}");
        assert!(
            text.contains(r#"known_hosts_file = "/home/me/Library/Application Support/kh""#),
            "{text}"
        );
        for word in ["user", "password", "secret"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
        let back = Config::load(&path).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.get("plain").unwrap().ssh, None);
        // Written by hand, with only the host.
        std::fs::write(
            &path,
            "[[profile]]\nname = \"x\"\n[profile.endpoint]\nhost = \"10.0.0.5\"\nport = 6379\n[profile.ssh]\nhost = \"jump\"\n",
        )
        .unwrap();
        let p = Config::load(&path).unwrap().get("x").unwrap().clone();
        let t = p.ssh.unwrap();
        assert_eq!((t.host.as_str(), t.port), ("jump", None));
        assert_eq!((t.identity_file, t.known_hosts_file), (None, None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_and_writes_round_trip_and_touch_nothing_else() {
        let dir = temp_dir("writes");
        let path = dir.join("config.toml");
        let mut c = Config::default();
        c.add(profile("a", 6379), false).unwrap();
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("settings"), "{text}");
        let before = Config::load(&path).unwrap();
        let mut c = before.clone();
        let p = c.set_writes("a", true).unwrap();
        assert_eq!(p.read_only, Some(false));
        assert!(matches!(
            c.set_writes("nope", true),
            Err(ConfigError::NotFound(_))
        ));
        c.settings.skip_non_prod_confirm = true;
        c.save(&path).unwrap();
        let after = Config::load(&path).unwrap();
        assert!(after.settings.skip_non_prod_confirm);
        let mut expect = before.profiles[0].clone();
        expect.read_only = Some(false);
        assert_eq!(after.profiles, vec![expect]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[settings]") && text.contains("read_only = false"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_path_ends_with_config_toml() {
        if let Ok(p) = Config::default_path() {
            assert!(p.ends_with("config.toml"));
        }
    }
}
