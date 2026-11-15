//! Development launcher for the web dashboard against the demo servers (`make demo-up`).
//!
//! It never touches the real config file or the OS keychain: profiles go in a temporary
//! config file and the demo passwords (read from `demo/users.acl`) in a `MemoryStore`.
//!
//! ```sh
//! make demo-up
//! REDIS_LENS_DASHBOARD_ASSETS=src/web/assets cargo run --example dashboard_dev -- 7391
//! ```
//!
//! It prints the URL to open, `http://127.0.0.1:<port>/#token=<hex>` (the fragment holds
//! this run's token; the page sends it as `Authorization: Bearer`), and serves until ctrl-c.

use std::sync::Arc;

use redis_lens::config::Config;
use redis_lens::model::{Auth, Credentials, Endpoint, Env, Profile};
use redis_lens::secrets::{MemoryStore, SecretStore};
use redis_lens::web::{self, Options};
use secrecy::SecretString;

fn profile(name: &str, port: u16, env: Env, auth: Auth, read_only: Option<bool>) -> Profile {
    Profile {
        name: name.into(),
        env,
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port,
            tls: false,
        },
        db: 0,
        auth,
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

/// The password of ACL user `user` in `demo/users.acl` (`user <name> on ><password> …`).
fn demo_password(acl: &str, user: &str) -> Option<String> {
    acl.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("user") && words.next() == Some(user))
            .then(|| words.find_map(|w| w.strip_prefix('>').map(str::to_string)))
            .flatten()
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(7391);
    let acl = std::fs::read_to_string("demo/users.acl")?;
    let dir = std::env::temp_dir().join(format!("redis-lens-dashboard-dev-{port}"));
    std::fs::create_dir_all(&dir)?;
    let config = dir.join("config.toml");

    let profiles = vec![
        // The demo's read-only ACL user, on a local profile and on a production one.
        ("demo-redis", 6390, Env::Local, Some("lens"), None),
        ("demo-prod", 6390, Env::Prod, Some("lens"), None),
        // An ACL user with very few permissions: most features degrade.
        ("demo-limited", 6390, Env::Stage, Some("lens-limited"), None),
        // Everything allowed: for the write flows.
        ("demo-admin", 6390, Env::Local, Some("admin"), Some(false)),
        ("demo-valkey", 6391, Env::Local, None, None),
    ];
    let store = Arc::new(MemoryStore::default());
    let mut cfg = Config::default();
    for (name, port, env, user, read_only) in profiles {
        let auth = if user.is_some() {
            Auth::Keychain
        } else {
            Auth::None
        };
        let p = profile(name, port, env, auth, read_only);
        if let Some(user) = user {
            let password = demo_password(&acl, user)
                .ok_or_else(|| anyhow::anyhow!("no demo user {user} in demo/users.acl"))?;
            store.set(
                &p.secret_ref(),
                &Credentials {
                    username: Some(user.to_string()),
                    password: SecretString::from(password),
                },
            )?;
        }
        cfg.profiles.push(p);
    }
    cfg.save(&config)?;

    let dashboard = web::start_with(Options { port, config }, store)?;
    println!("{}", dashboard.open_url());
    dashboard.wait().await;
    Ok(())
}
