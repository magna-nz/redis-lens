//! `--config` and `REDIS_LENS_CONFIG` against the built binary. No server, no keychain:
//! `conn list` only reads the config file.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use redis_lens::config::Config;
use redis_lens::model::{Auth, Endpoint, Env, Profile};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "redis-lens-cli-config-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A config file holding one profile called `name`.
fn config_with(dir: &Path, file: &str, name: &str, port: u16) -> PathBuf {
    let path = dir.join(file);
    let mut config = Config::default();
    config
        .add(
            Profile {
                name: name.to_string(),
                env: Env::Local,
                endpoint: Endpoint {
                    host: "127.0.0.1".to_string(),
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
                tags: Vec::new(),
            },
            false,
        )
        .unwrap();
    config.save(&path).unwrap();
    path
}

fn run(args: &[&str], env: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_redis-lens"));
    cmd.args(args).env_remove("REDIS_LENS_CONFIG");
    if let Some(p) = env {
        cmd.env("REDIS_LENS_CONFIG", p);
    }
    cmd.output().expect("run redis-lens")
}

fn stdout(o: &Output) -> String {
    assert!(
        o.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn env_var_selects_the_config_file() {
    let dir = temp_dir("env");
    let a = config_with(&dir, "a.toml", "from-env-file", 6401);
    let out = stdout(&run(&["conn", "list"], Some(&a)));
    assert!(
        out.contains("from-env-file") && out.contains("127.0.0.1:6401"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_flag_selects_the_config_file() {
    let dir = temp_dir("flag");
    let b = config_with(&dir, "b.toml", "from-flag-file", 6402);
    let out = stdout(&run(
        &["--config", b.to_str().unwrap(), "conn", "list"],
        None,
    ));
    assert!(out.contains("from-flag-file"), "{out}");
    // The flag is global, so it also works after the subcommand.
    let out = stdout(&run(
        &["conn", "list", "--config", b.to_str().unwrap()],
        None,
    ));
    assert!(out.contains("from-flag-file"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_flag_overrides_the_env_var() {
    let dir = temp_dir("override");
    let a = config_with(&dir, "a.toml", "from-env-file", 6401);
    let b = config_with(&dir, "b.toml", "from-flag-file", 6402);
    let out = stdout(&run(
        &["--config", b.to_str().unwrap(), "conn", "list"],
        Some(&a),
    ));
    assert!(out.contains("from-flag-file"), "{out}");
    assert!(!out.contains("from-env-file"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_config_file_lists_nothing() {
    let dir = temp_dir("missing");
    let nowhere = dir.join("nope.toml");
    let out = stdout(&run(&["conn", "list"], Some(&nowhere)));
    assert!(out.starts_with("No saved connections"), "{out}");
    assert!(!nowhere.exists(), "list must not create the file");
    let _ = std::fs::remove_dir_all(&dir);
}
