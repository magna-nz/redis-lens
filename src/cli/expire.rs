//! `redis-lens expire <name> <pattern> --ttl DURATION [--apply] [--yes | --confirm PATTERN]`
//! (spec "Bulk actions"): sets a TTL on every key of a pattern that has none.
//!
//! Without `--apply` it is a dry run: it counts the keys and changes nothing. With
//! `--apply` the write gate decides ([`crate::write::Permit::grant`]): a `prod` profile
//! needs `--confirm` with the exact pattern, and `--yes` never replaces it; other
//! profiles need `--yes` (or `--confirm` with the pattern) when there is no terminal to
//! ask `y`/`n` on. A read-only profile is refused, with exit code 2, before anything is
//! sent. The run is [`crate::live::bulk`]'s, so the key file and the batches are the
//! TUI's.
//!
//! The logic is in [`execute`], so tests run it against a temp config, a
//! `MemoryStore` and a temp key-file directory; [`run`] wires it to the keychain and the
//! terminal.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Args;

use super::GateError;
use crate::budget::Budget;
use crate::config::Config;
use crate::conn::{self, Replication};
use crate::live::bulk::{self, Count, KeyFile, Walk};
use crate::model::ServerMode;
use crate::scan::ScanOptions;
use crate::secrets::{KeyringStore, SecretStore};
use crate::write::{Answer, BulkAction, Decision, Permit, Refusals, WriteOp};

#[derive(Debug, Clone, Args)]
pub struct ExpireArgs {
    /// Profile name.
    pub profile: String,
    /// The pattern as the Patterns view shows it, e.g. "user:{id}:cart".
    pub pattern: String,
    /// The TTL to set on keys that have none: 90s, 10m, 24h, 7d.
    #[arg(long, value_parser = bulk::parse_ttl, value_name = "DURATION")]
    pub ttl: u64,
    /// Set the TTLs. Without it, only count the keys (a dry run).
    #[arg(long)]
    pub apply: bool,
    /// With --apply, don't ask y/n. Not enough on a prod profile: use --confirm.
    #[arg(long, requires = "apply", conflicts_with = "confirm")]
    pub yes: bool,
    /// With --apply, the exact pattern: needed on a prod profile.
    #[arg(long, requires = "apply", value_name = "PATTERN")]
    pub confirm: Option<String>,
    /// Upper bound on commands per second (0 means no limit). Defaults to the
    /// profile's: no limit for local profiles and 5,000 otherwise.
    #[arg(long)]
    pub max_rate: Option<u32>,
}

/// What `expire` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The dry run's counts.
    pub count: Count,
    /// The run, with `--apply` when there was something to change.
    pub run: Option<RunResult>,
}

/// What the run changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunResult {
    pub seen: u64,
    pub changed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub unreadable: u64,
    /// The key file, when a batch wrote one.
    pub file: Option<PathBuf>,
}

/// The answer the gate wants, from the flags, before anything is sent: `None` when it
/// must be asked on the terminal (`y`/`n`). Refuses a prod profile without `--confirm`
/// and a `--confirm` that is not the pattern.
fn answer_from_flags(decision: &Decision, args: &ExpireArgs) -> anyhow::Result<Option<Answer>> {
    match decision {
        Decision::Refuse(why) => Err(GateError(anyhow::anyhow!(
            "{why} From the command line: redis-lens conn set {} --writes on",
            crate::value::escape(&args.profile)
        ))
        .into()),
        Decision::Type(want) => match &args.confirm {
            Some(c) if c == want => Ok(Some(Answer::Typed(c.clone()))),
            Some(_) => bail!(
                "Not sent: the pattern given with --confirm does not match {}",
                crate::value::escape(want)
            ),
            None => bail!(
                "Not sent: this is a prod profile; pass --confirm {} (--yes is not enough on a \
                 prod profile)",
                crate::value::escape(want)
            ),
        },
        Decision::Ask => match &args.confirm {
            Some(c) if *c == args.pattern => Ok(Some(Answer::Yes)),
            Some(_) => bail!(
                "Not sent: the pattern given with --confirm does not match {}",
                crate::value::escape(&args.pattern)
            ),
            None if args.yes => Ok(Some(Answer::Yes)),
            None => Ok(None),
        },
        Decision::Run => Ok(Some(Answer::None)),
    }
}

/// Everything `expire` does after parsing arguments. `bulk_dir` holds the key files;
/// `ask(question, hint)` asks on the terminal and refuses with `hint` without one;
/// `progress` shows the dry run's and the run's progress (on stderr, in [`run`]).
pub async fn execute(
    config_path: &Path,
    secrets: &dyn SecretStore,
    args: &ExpireArgs,
    bulk_dir: &Path,
    ask: &dyn Fn(&str, &str) -> anyhow::Result<String>,
    out: &mut dyn Write,
    progress: &mut dyn FnMut(&str),
) -> anyhow::Result<Outcome> {
    let config = Config::load(config_path)?;
    let profile = config.get(&args.profile).ok_or_else(|| {
        anyhow::anyhow!(
            "no saved connection named '{}'; see `redis-lens conn list`",
            args.profile
        )
    })?;
    if args.pattern.is_empty() {
        bail!("no pattern given");
    }
    let action = BulkAction::Expire { seconds: args.ttl };
    let op = WriteOp::Bulk {
        pattern: args.pattern.clone(),
        glob: crate::analysis::match_glob(&args.pattern),
        action,
    };
    let refusals = Refusals::default();
    let decision = crate::write::decide_op(&op, profile, &config.settings, &refusals);
    // Read-only, or a prod profile without the pattern: refused before connecting.
    let flagged = if args.apply {
        answer_from_flags(&decision, args)?
    } else if let Decision::Refuse(_) = decision {
        answer_from_flags(&decision, args)?
    } else {
        None
    };

    let mut conn = conn::connect(profile, secrets).await?;
    let mut opts = ScanOptions::for_profile(profile);
    if let Some(rate) = args.max_rate {
        opts.max_commands_per_sec = (rate > 0).then_some(rate);
    }
    let budget = Budget::new(opts.max_commands_per_sec);
    let server = crate::scan::server_info_with_budget(&mut conn, &budget)
        .await
        .context("could not read INFO")?;
    if server.mode == ServerMode::Cluster {
        bail!("{}", bulk::CLUSTER_OFF);
    }
    if let Some(why) = bulk::expire_off(&server) {
        bail!("{why}");
    }
    budget.take(1).await;
    let info: String = redis::cmd("INFO")
        .arg("replication")
        .query_async(&mut conn)
        .await
        .context("could not read INFO replication")?;
    if conn::parse_replication(&info) == Replication::Replica {
        bail!("{}", bulk::REPLICA_OFF);
    }

    let WriteOp::Bulk { pattern, glob, .. } = &op else {
        unreachable!("built above");
    };
    let count = bulk::dry_run(&mut conn, &budget, pattern, glob, |c| {
        progress(&format!(
            "Dry run: {} keys of the pattern so far",
            crate::format::grouped(c.matched)
        ));
    })
    .await?;
    let change = count.to_change(&action);
    let name = crate::value::escape(pattern);
    writeln!(
        out,
        "Pattern:  {name}  (SCAN MATCH {})",
        crate::value::escape(glob)
    )?;
    writeln!(
        out,
        "Keys:     {} of the pattern, {} with a TTL already, {} without",
        crate::format::grouped(count.matched),
        crate::format::grouped(count.with_ttl),
        crate::format::grouped(change)
    )?;
    if count.other_pattern > 0 {
        writeln!(
            out,
            "Other:    {} keys matched the glob but belong to other patterns (left alone)",
            crate::format::grouped(count.other_pattern)
        )?;
    }
    if count.unreadable > 0 {
        writeln!(
            out,
            "Unread:   {} keys could not be read (left alone)",
            crate::format::grouped(count.unreadable)
        )?;
    }
    if !args.apply {
        writeln!(
            out,
            "Dry run: nothing was changed. Add --apply to set a TTL of {} on the {} keys without one.",
            bulk::ttl_text(args.ttl),
            crate::format::grouped(change)
        )?;
        return Ok(Outcome { count, run: None });
    }
    if change == 0 {
        writeln!(
            out,
            "Nothing to change: no key of the pattern is without a TTL."
        )?;
        return Ok(Outcome { count, run: None });
    }
    let answer = match flagged {
        Some(a) => a,
        None => {
            let typed = ask(
                &format!(
                    "Set a TTL of {} on {} keys of {name} on {}? PERSIST on the keys in the key \
                     file undoes it. [y/N] ",
                    bulk::ttl_text(args.ttl),
                    crate::format::grouped(change),
                    crate::value::escape(&profile.name)
                ),
                "pass --yes to set the TTLs without asking",
            )?;
            if matches!(typed.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                Answer::Yes
            } else {
                bail!("Not sent: it was not confirmed.");
            }
        }
    };
    let permit = Permit::grant(op.clone(), profile, &config.settings, &refusals, &answer)
        .map_err(|why| anyhow::anyhow!(why))?;

    let mut file = KeyFile::new(bulk_dir, &profile.name, &action, bulk::now_unix());
    let mut result = RunResult::default();
    let (mut walk, mut carry) = (Walk::Start, Vec::new());
    let stopped = loop {
        let b = match bulk::batch(&mut conn, &budget, &permit, walk, carry, &mut file).await {
            Ok(b) => b,
            Err(e) => break Some(e.to_string()),
        };
        result.seen += b.seen;
        result.changed += b.changed;
        result.skipped += b.skipped;
        result.failed += b.failed;
        result.unreadable += b.unreadable;
        if b.file.is_some() {
            result.file = b.file.clone();
        }
        progress(&format!(
            "Set a TTL on {} of {} keys",
            crate::format::grouped(result.changed),
            crate::format::grouped(change)
        ));
        if b.failed > 0 {
            break Some(if b.denied {
                "the server refused EXPIRE for this user".to_string()
            } else {
                format!(
                    "the server answered EXPIRE with an error for {} keys",
                    crate::format::grouped(b.failed)
                )
            });
        }
        if b.finished() {
            break None;
        }
        (walk, carry) = (b.walk, b.carry);
    };
    writeln!(
        out,
        "Set a TTL of {} on {} keys of {name}; {} left alone (a TTL already, or gone).",
        bulk::ttl_text(args.ttl),
        crate::format::grouped(result.changed),
        crate::format::grouped(result.skipped)
    )?;
    if let Some(f) = &result.file {
        writeln!(out, "Key names: {}", f.display())?;
        writeln!(out, "PERSIST on those keys undoes it.")?;
    }
    if let Some(why) = stopped {
        bail!("stopped: {why}");
    }
    Ok(Outcome {
        count,
        run: Some(result),
    })
}

/// Ask `question` on the terminal and return the line typed. Without a terminal there
/// is no one to ask, so it refuses with `hint`.
fn ask_on_terminal(question: &str, hint: &str) -> anyhow::Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so no one can be asked; {hint}");
    }
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("could not read the answer")?;
    Ok(answer.trim_end_matches(['\r', '\n']).to_string())
}

pub async fn run(config: Option<PathBuf>, args: ExpireArgs) -> anyhow::Result<()> {
    let path = match config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    let dir = bulk::default_dir().context("could not find a data directory for the key file")?;
    let show = std::io::stderr().is_terminal();
    let mut progress = |line: &str| {
        if show {
            eprint!("\r\x1b[2K{line}");
        }
    };
    let r = execute(
        &path,
        &KeyringStore,
        &args,
        &dir,
        &ask_on_terminal,
        &mut std::io::stdout(),
        &mut progress,
    )
    .await;
    if show {
        eprint!("\r\x1b[2K");
    }
    r.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Auth, Endpoint, Env, Profile};
    use crate::secrets::MemoryStore;

    fn args(apply: bool, yes: bool, confirm: Option<&str>) -> ExpireArgs {
        ExpireArgs {
            profile: "p".into(),
            pattern: "user:{id}:cart".into(),
            ttl: 86_400,
            apply,
            yes,
            confirm: confirm.map(str::to_string),
            max_rate: None,
        }
    }

    fn profile(env: Env, read_only: Option<bool>) -> Profile {
        Profile {
            name: "p".into(),
            env,
            // Nothing listens here: a test that gets as far as connecting fails to.
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 1,
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
        }
    }

    fn decision(env: Env, read_only: Option<bool>) -> Decision {
        let op = WriteOp::Bulk {
            pattern: "user:{id}:cart".into(),
            glob: "user:*:cart".into(),
            action: BulkAction::Expire { seconds: 86_400 },
        };
        crate::write::decide_op(
            &op,
            &profile(env, read_only),
            &Default::default(),
            &Refusals::default(),
        )
    }

    #[test]
    fn prod_needs_confirm_and_yes_never_replaces_it() {
        let d = decision(Env::Prod, Some(false));
        let e = answer_from_flags(&d, &args(true, true, None)).unwrap_err();
        assert!(e.to_string().contains("--confirm user:{id}:cart"), "{e}");
        assert!(answer_from_flags(&d, &args(true, false, Some("user:*"))).is_err());
        assert_eq!(
            answer_from_flags(&d, &args(true, false, Some("user:{id}:cart"))).unwrap(),
            Some(Answer::Typed("user:{id}:cart".into()))
        );
    }

    #[test]
    fn other_profiles_take_yes_or_ask() {
        let d = decision(Env::Stage, Some(false));
        assert_eq!(
            answer_from_flags(&d, &args(true, true, None)).unwrap(),
            Some(Answer::Yes)
        );
        assert_eq!(
            answer_from_flags(&d, &args(true, false, Some("user:{id}:cart"))).unwrap(),
            Some(Answer::Yes)
        );
        assert_eq!(
            answer_from_flags(&d, &args(true, false, None)).unwrap(),
            None
        );
    }

    #[test]
    fn read_only_is_a_gate_error() {
        for (env, ro) in [(Env::Prod, None), (Env::Local, Some(true))] {
            let d = decision(env, ro);
            let e = answer_from_flags(&d, &args(true, false, Some("user:{id}:cart"))).unwrap_err();
            assert_eq!(crate::cli::exit_code(&e), 2);
            assert!(e.to_string().contains("read-only"), "{e}");
        }
    }

    fn temp_config(p: Profile) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("redis-lens-expire-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        Config {
            profiles: vec![p],
            ..Default::default()
        }
        .save(&path)
        .unwrap();
        path
    }

    async fn exec(p: Profile, a: ExpireArgs) -> (anyhow::Result<Outcome>, String) {
        let path = temp_config(p);
        let dir = path.parent().unwrap().join("bulk");
        let mut out = Vec::new();
        let asked = std::cell::Cell::new(false);
        let ask = |_: &str, _: &str| -> anyhow::Result<String> {
            asked.set(true);
            Ok("y".into())
        };
        let r = execute(
            &path,
            &MemoryStore::default(),
            &a,
            &dir,
            &ask,
            &mut out,
            &mut |_| {},
        )
        .await;
        assert!(!asked.get(), "nothing is asked before the dry run");
        assert!(!dir.exists(), "no key file");
        (r, String::from_utf8(out).unwrap())
    }

    #[tokio::test]
    async fn read_only_exits_2_before_connecting_with_or_without_apply() {
        for apply in [false, true] {
            let a = args(apply, false, None);
            let (r, out) = exec(profile(Env::Prod, None), a).await;
            let e = r.unwrap_err();
            assert_eq!(crate::cli::exit_code(&e), 2, "{e}");
            assert!(e.to_string().contains("p is read-only"), "{e}");
            assert!(out.is_empty());
        }
    }

    #[tokio::test]
    async fn prod_without_confirm_refuses_before_connecting() {
        let (r, out) = exec(profile(Env::Prod, Some(false)), args(true, true, None)).await;
        let e = r.unwrap_err();
        assert_eq!(crate::cli::exit_code(&e), 1);
        assert!(e.to_string().contains("--yes is not enough"), "{e}");
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn without_apply_it_only_connects_to_count() {
        // A dry run on a profile with writes on goes on to connect (and fails here,
        // since nothing listens): no answer is needed for it.
        let (r, _) = exec(profile(Env::Prod, Some(false)), args(false, false, None)).await;
        let e = r.unwrap_err().to_string();
        assert!(!e.contains("--confirm") && !e.contains("read-only"), "{e}");
    }

    #[test]
    fn flags_parse() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from([
            "redis-lens",
            "expire",
            "p",
            "user:{id}:cart",
            "--ttl",
            "7d",
        ])
        .unwrap();
        let Some(crate::cli::Command::Expire(a)) = cli.command else {
            panic!("expire");
        };
        assert_eq!((a.ttl, a.apply, a.yes), (604_800, false, false));
        // --yes and --confirm need --apply, and not both.
        for bad in [
            vec!["--ttl", "1h", "--yes"],
            vec!["--ttl", "1h", "--confirm", "x"],
            vec!["--ttl", "1h", "--apply", "--yes", "--confirm", "x"],
            vec!["--ttl", "0s"],
            vec!["--apply"],
        ] {
            let mut argv = vec!["redis-lens", "expire", "p", "user:{id}:cart"];
            argv.extend(bad.iter().copied());
            assert!(crate::cli::Cli::try_parse_from(&argv).is_err(), "{bad:?}");
        }
    }
}
