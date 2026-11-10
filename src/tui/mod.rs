//! The terminal UI.
//!
//! Owner: TUI and CLI wiring.
//!
//! - [`app`]: the state and its pure update functions (`handle_key`, `handle_paste`,
//!   `apply`), which return [`app::Effect`]s instead of doing IO.
//! - [`ui`]: pure rendering, `ui::draw(frame, &app)`, one module per view.
//! - [`tree`]: grouping patterns into the Patterns tree.
//! - This module: terminal setup, the event loop, and the tokio tasks that carry out
//!   effects and send [`app::Msg`]s back. All server IO goes through `conn` and `scan`.

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::bail;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::execute;
use ratatui::DefaultTerminal;
use secrecy::ExposeSecret;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::conn::{self, ScanTarget};
use crate::model::{Credentials, Profile, ScanReport};
use crate::scan::{self, ScanOptions, ScanProgress};
use crate::secrets::{KeyringStore, MemoryStore, SecretStore};
use crate::{diff, snapshot};

pub mod app;
pub mod tree;
pub mod ui;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod snapshot_tests;

use app::{App, ConnId, Effect, Msg, SecretSource};

const TICK: Duration = Duration::from_millis(100);

/// What arrives on the event loop's channel.
enum Incoming {
    Input(Event),
    InputFailed(String),
    Msg(Msg),
}

type Tx = mpsc::UnboundedSender<Incoming>;

pub async fn run(config: Option<PathBuf>) -> anyhow::Result<()> {
    let path = match config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    let loaded = Config::load(&path);

    let mut terminal = ratatui::try_init()?;
    // ratatui's panic hook restores the terminal; also turn bracketed paste off.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableBracketedPaste);
        prev(info);
    }));
    let result = match execute!(stdout(), EnableBracketedPaste) {
        Ok(()) => event_loop(&mut terminal, path, loaded).await,
        Err(e) => Err(e.into()),
    };
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

fn spawn_input_thread(tx: Tx) {
    std::thread::spawn(move || {
        loop {
            let msg = match crossterm::event::read() {
                Ok(ev) => Incoming::Input(ev),
                Err(e) => Incoming::InputFailed(e.to_string()),
            };
            let failed = matches!(msg, Incoming::InputFailed(_));
            if tx.send(msg).is_err() || failed {
                break;
            }
        }
    });
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    path: PathBuf,
    loaded: Result<Config, crate::config::ConfigError>,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = match loaded {
        Ok(cfg) => App::new(cfg.profiles),
        Err(e) => {
            let mut app = App::new(Vec::new());
            app.set_error(conn::parse::mask(&e.to_string()));
            app
        }
    };
    spawn_input_thread(tx.clone());
    let mut runner = Runner {
        tx,
        path,
        scan_task: None,
    };
    let mut pending = app.init();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        for effect in pending.drain(..) {
            if runner.run(effect) {
                return Ok(());
            }
        }
        terminal.draw(|f| ui::draw(f, &app))?;
        tokio::select! {
            incoming = rx.recv() => match incoming {
                Some(Incoming::Input(Event::Key(k))) => pending = app.handle_key(k),
                Some(Incoming::Input(Event::Paste(s))) => app.handle_paste(s),
                Some(Incoming::Input(_)) => {}
                Some(Incoming::InputFailed(e)) => bail!("could not read terminal input: {e}"),
                Some(Incoming::Msg(m)) => pending = app.apply(m),
                None => return Ok(()),
            },
            _ = tick.tick() => pending = app.apply(Msg::TickAt(std::time::Instant::now())),
        }
    }
}

/// Carries out effects. Each one runs in its own task and reports back as a `Msg`.
struct Runner {
    tx: Tx,
    path: PathBuf,
    scan_task: Option<JoinHandle<()>>,
}

impl Drop for Runner {
    fn drop(&mut self) {
        if let Some(t) = self.scan_task.take() {
            t.abort();
        }
    }
}

fn send(tx: &Tx, msg: Msg) -> bool {
    tx.send(Incoming::Msg(msg)).is_ok()
}

impl Runner {
    /// Returns true when the app should quit.
    fn run(&mut self, effect: Effect) -> bool {
        let tx = self.tx.clone();
        match effect {
            Effect::Quit => return true,
            Effect::DiscoverDocker => {
                tokio::spawn(async move {
                    let msg = match conn::docker::discover().await {
                        Ok(found) => Msg::DockerFound(found),
                        Err(e) => Msg::DockerFailed(e.to_string()),
                    };
                    send(&tx, msg);
                });
            }
            Effect::Connect {
                id,
                profile,
                secrets,
            } => {
                if let Some(t) = self.scan_task.take() {
                    t.abort();
                }
                self.scan_task = Some(tokio::spawn(connect_and_scan(id, profile, secrets, tx)));
            }
            Effect::SaveConnection {
                profile,
                credentials,
            } => {
                let path = self.path.clone();
                tokio::task::spawn_blocking(move || {
                    let msg =
                        match save_connection(&path, &KeyringStore, &profile, credentials.as_ref())
                        {
                            Ok(()) => Msg::Saved(profile),
                            Err(e) => Msg::SaveFailed(e.to_string()),
                        };
                    send(&tx, msg);
                });
            }
            Effect::LoadHistory { id, report } => {
                tokio::task::spawn_blocking(move || {
                    let taken_at_unix = report.taken_at_unix;
                    let result = load_history(&report).map_err(|e| e.to_string());
                    send(
                        &tx,
                        Msg::History {
                            id,
                            taken_at_unix,
                            result,
                        },
                    );
                });
            }
            Effect::SaveSnapshot(report) => {
                tokio::task::spawn_blocking(move || {
                    let msg =
                        match snapshot::default_dir().and_then(|d| snapshot::save(&d, &report)) {
                            Ok(path) => Msg::SnapshotSaved(path),
                            Err(e) => Msg::SnapshotFailed(e.to_string()),
                        };
                    send(&tx, msg);
                });
            }
        }
        false
    }
}

/// The diff between the newest snapshot older than `report` and `report`.
fn load_history(report: &ScanReport) -> Result<Option<diff::ReportDiff>, snapshot::SnapshotError> {
    let dir = snapshot::default_dir()?;
    let prev = snapshot::latest_before(
        &dir,
        &report.profile,
        report.taken_at_unix,
        report.match_glob.as_deref(),
    )?;
    Ok(prev.map(|p| diff::diff(&p, report)))
}

/// Save a new profile the way `conn add` does: credentials to the keychain first, so
/// a saved profile never points at a missing entry, then the config file. If the
/// config can't be written, the keychain entry is put back as it was. Refuses to
/// replace an existing profile, and checks that before touching the keychain.
fn save_connection(
    path: &Path,
    secrets: &dyn SecretStore,
    profile: &Profile,
    credentials: Option<&Credentials>,
) -> anyhow::Result<()> {
    let mut config = Config::load(path)?;
    if config.get(&profile.name).is_some() {
        bail!("a profile named {} already exists", profile.name);
    }
    config.add(profile.clone(), false)?;
    match credentials {
        Some(c) => {
            crate::cli::conn::set_credentials_then_save(secrets, &profile.secret_ref(), c, || {
                Ok(config.save(path)?)
            })
        }
        None => Ok(config.save(path)?),
    }
}

/// Remove a known secret from error text, as a second line of defence.
fn scrub(text: String, secret: Option<&str>) -> String {
    match secret {
        Some(s) if !s.is_empty() => text.replace(s, "******"),
        _ => text,
    }
}

/// A fallback reason is free text built from server replies and addresses: it goes
/// through the same `clean` as error messages before the TUI shows it.
fn clean_target(target: ScanTarget, clean: impl Fn(String) -> String) -> ScanTarget {
    match target {
        ScanTarget::PrimaryFallback(why) => ScanTarget::PrimaryFallback(clean(why)),
        t => t,
    }
}

/// Prefix of the report (and so snapshot) name of a Docker container, so a container
/// called `cache` never shares history with a saved profile called `cache`.
const DOCKER_PREFIX: &str = crate::cli::conn::DOCKER_PREFIX;

/// The name a scan of `profile` is reported and snapshotted under: the profile name for
/// saved profiles, `docker:<container>` for containers found in Docker.
fn report_name(profile: &Profile, secrets: &SecretSource) -> String {
    match secrets {
        SecretSource::Keychain => profile.name.clone(),
        SecretSource::Container(_) => format!("{DOCKER_PREFIX}{}", profile.name),
    }
}

async fn connect_and_scan(id: ConnId, profile: Profile, secrets: SecretSource, tx: Tx) {
    let name = report_name(&profile, &secrets);
    let (store, secret): (Box<dyn SecretStore>, Option<String>) = match secrets {
        SecretSource::Keychain => (Box::new(KeyringStore), None),
        SecretSource::Container(creds) => {
            let store = MemoryStore::default();
            let secret = creds
                .as_ref()
                .map(|c| c.password.expose_secret().to_string());
            if let Some(c) = &creds {
                // MemoryStore::set cannot fail.
                let _ = store.set(&profile.secret_ref(), c);
            }
            (Box::new(store), secret)
        }
    };
    let clean = |e: String| conn::parse::mask(&scrub(e, secret.as_deref()));

    let (mut c, target) = match conn::connect_for_scan(&profile, store.as_ref()).await {
        Ok((c, target)) => (c, clean_target(target, clean)),
        Err(e) => {
            send(
                &tx,
                Msg::ConnectFailed {
                    id,
                    error: clean(e.to_string()),
                },
            );
            return;
        }
    };
    let perms = conn::check_permissions(&mut c).await;
    let server = scan::server_info(&mut c).await.ok();
    if !send(
        &tx,
        Msg::Connected {
            id,
            server,
            perms: perms.clone(),
            target,
        },
    ) {
        return;
    }

    let (ptx, mut prx) = watch::channel(ScanProgress::default());
    let ftx = tx.clone();
    let forward = tokio::spawn(async move {
        while prx.changed().await.is_ok() {
            let progress = *prx.borrow_and_update();
            if !send(&ftx, Msg::ScanProgress { id, progress }) {
                break;
            }
        }
    });
    let result = scan::scan(
        &name,
        &mut c,
        &ScanOptions::for_profile(&profile),
        &perms,
        Some(&ptx),
    )
    .await;
    drop(ptx);
    let _ = forward.await;
    send(
        &tx,
        match result {
            Ok(report) => Msg::ScanDone { id, report },
            Err(e) => Msg::ScanFailed {
                id,
                error: clean(e.to_string()),
            },
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Auth, Endpoint, Env};
    use secrecy::SecretString;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "redis-lens-tui-{tag}-{}-{nanos}",
                std::process::id()
            ))
            .join("config.toml")
    }

    fn profile(name: &str) -> Profile {
        Profile {
            name: name.into(),
            env: Env::Prod,
            endpoint: Endpoint {
                host: "prod-cache-02.internal".into(),
                port: 6380,
                tls: true,
            },
            db: 0,
            auth: Auth::Keychain,
            read_only: None,
            prefer_replica: false,
        }
    }

    #[test]
    fn save_writes_keychain_then_config_without_secrets() {
        let path = temp_path("save");
        let store = MemoryStore::default();
        let p = profile("prod-cache-02");
        let creds = Credentials {
            username: Some("lens-ro".into()),
            password: SecretString::from("Sup3rS3cret"),
        };
        save_connection(&path, &store, &p, Some(&creds)).unwrap();
        let got = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(got.password.expose_secret(), "Sup3rS3cret");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("prod-cache-02"));
        assert!(!text.contains("Sup3rS3cret"));
        assert!(!text.contains("lens-ro"));
        assert_eq!(Config::load(&path).unwrap().profiles, vec![p.clone()]);

        // A second save under the same name fails before touching the keychain.
        let other = Credentials {
            username: None,
            password: SecretString::from("other"),
        };
        assert!(save_connection(&path, &store, &p, Some(&other)).is_err());
        let still = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(still.password.expose_secret(), "Sup3rS3cret");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_save_leaves_keychain_as_it_was() {
        // A directory where `Config::save` writes its temp file makes the save fail
        // while the load still works.
        let path = temp_path("save-fails");
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
        std::fs::create_dir_all(tmp.join("blocker")).unwrap();

        let creds = Credentials {
            username: Some("lens-ro".into()),
            password: SecretString::from("Sup3rS3cret"),
        };
        // No entry before: none after.
        let store = MemoryStore::default();
        let p = profile("new-one");
        assert!(save_connection(&path, &store, &p, Some(&creds)).is_err());
        assert!(store.get(&p.secret_ref()).unwrap().is_none());

        // A leftover entry before: the same entry after.
        let old = Credentials {
            username: None,
            password: SecretString::from("leftover"),
        };
        store.set(&p.secret_ref(), &old).unwrap();
        assert!(save_connection(&path, &store, &p, Some(&creds)).is_err());
        let got = store.get(&p.secret_ref()).unwrap().unwrap();
        assert_eq!(got.username, None);
        assert_eq!(got.password.expose_secret(), "leftover");
        assert!(Config::load(&path).unwrap().profiles.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn docker_scans_are_filed_apart_from_saved_profiles() {
        let p = profile("cache");
        assert_eq!(report_name(&p, &SecretSource::Keychain), "cache");
        assert_eq!(
            report_name(&p, &SecretSource::Container(None)),
            "docker:cache"
        );
        let creds = Credentials {
            username: None,
            password: SecretString::from("pw"),
        };
        assert_eq!(
            report_name(&p, &SecretSource::Container(Some(creds))),
            "docker:cache"
        );
    }

    #[test]
    fn fallback_reasons_are_cleaned() {
        let secret = Some("hunter2");
        let clean = |e: String| conn::parse::mask(&scrub(e, secret));
        let t = clean_target(
            ScanTarget::PrimaryFallback(
                "10.0.0.1:6379: hunter2 refused for redis://u:s3cretAAA@h:6379".into(),
            ),
            clean,
        );
        let ScanTarget::PrimaryFallback(why) = t else {
            panic!("{t:?}");
        };
        assert!(!why.contains("hunter2"), "{why}");
        assert!(!why.contains("s3cretAAA"), "{why}");
        assert!(why.starts_with("10.0.0.1:6379: ******"), "{why}");
        assert_eq!(
            clean_target(ScanTarget::Primary, clean),
            ScanTarget::Primary
        );
    }

    #[test]
    fn scrub_removes_known_secret() {
        assert_eq!(
            scrub("bad password hunter2".into(), Some("hunter2")),
            "bad password ******"
        );
        assert_eq!(scrub("x".into(), Some("")), "x");
        assert_eq!(scrub("x".into(), None), "x");
    }
}
