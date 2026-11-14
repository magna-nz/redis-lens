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
//! On a cluster it discovers the shards on their primaries (never a replica, whatever
//! `prefer_replica` says) and walks each primary in slot order, each with its own
//! budget and its own `SCAN`; a key that moved meanwhile is left alone and counted, and
//! each line of the key file starts with the node's address and a tab (spec "Cluster
//! live views (P4)").
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
    /// Keys that moved to another node of the cluster during the run: left alone.
    pub moved: u64,
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
    if let Some(why) = bulk::expire_off(&server) {
        bail!("{why}");
    }
    // A cluster node: `INFO server` says so (`redis_mode`, or `server_mode` on Valkey
    // 8), or, when it names neither, `cluster_enabled:1` in `INFO cluster`. A cluster
    // must never be mistaken for one server here: the run would write to one node only.
    let cluster = server.mode == ServerMode::Cluster || {
        budget.take(1).await;
        let info: Option<String> = redis::cmd("INFO")
            .arg("cluster")
            .query_async(&mut conn)
            .await
            .ok();
        info.as_deref().and_then(conn::cluster::cluster_mode) == Some(true)
    };
    // The servers the run walks, one at a time: the profile's own server, or on a
    // cluster every primary in slot order (spec "Cluster live views (P4)").
    let mut shards: Vec<Shard<conn::Connection>> = if cluster {
        drop(conn);
        // Discovery on the primaries: never a replica, whatever the profile prefers.
        let mut on_primaries = profile.clone();
        on_primaries.prefer_replica = false;
        let targets = conn::cluster::connect_targets(&on_primaries, secrets).await?;
        let conn::cluster::ScanTargets::Cluster(c) = targets else {
            bail!(
                "the cluster's shards could not be read, so the pattern can't be expired on every primary"
            );
        };
        let found = c
            .nodes
            .into_iter()
            .map(|n| (n.endpoint.to_string(), n.role, n.conn))
            .collect();
        primaries(found, opts.max_commands_per_sec).map_err(|why| anyhow::anyhow!(why))?
    } else {
        budget.take(1).await;
        let info: String = redis::cmd("INFO")
            .arg("replication")
            .query_async(&mut conn)
            .await
            .context("could not read INFO replication")?;
        if conn::parse_replication(&info) == Replication::Replica {
            bail!("{}", bulk::REPLICA_OFF);
        }
        vec![Shard {
            node: None,
            conn,
            budget,
        }]
    };

    let WriteOp::Bulk { pattern, glob, .. } = &op else {
        unreachable!("built above");
    };
    let mut count = Count::default();
    let mut per_node = Vec::with_capacity(shards.len());
    for s in &mut shards {
        let before = count;
        let c = bulk::dry_run(&mut s.conn, &s.budget, pattern, glob, |c| {
            progress(&format!(
                "Dry run: {} keys of the pattern so far",
                crate::format::grouped(before.matched + c.matched)
            ));
        })
        .await?;
        count.add(&c);
        per_node.push(c);
    }
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
    if shards.len() > 1 || shards.iter().any(|s| s.node.is_some()) {
        writeln!(
            out,
            "Nodes:    {} primaries, walked one at a time in slot order:",
            shards.len()
        )?;
        for (s, c) in shards.iter().zip(&per_node) {
            writeln!(
                out,
                "          {}: {} keys of the pattern, {} without a TTL",
                crate::value::escape(s.node.as_deref().unwrap_or("")),
                crate::format::grouped(c.matched),
                crate::format::grouped(c.to_change(&action))
            )?;
        }
    }
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
    let mut stopped = None;
    'nodes: for s in &mut shards {
        let (mut walk, mut carry) = (Walk::Start, Vec::new());
        loop {
            let b = match bulk::batch_on(
                &mut s.conn,
                &s.budget,
                &permit,
                walk,
                carry,
                &mut file,
                s.node.as_deref(),
            )
            .await
            {
                Ok(b) => b,
                Err(e) => {
                    stopped = Some(e.to_string());
                    break 'nodes;
                }
            };
            result.seen += b.seen;
            result.changed += b.changed;
            result.skipped += b.skipped;
            result.failed += b.failed;
            result.unreadable += b.unreadable;
            result.moved += b.moved;
            if b.file.is_some() {
                result.file = b.file.clone();
            }
            progress(&format!(
                "Set a TTL on {} of {} keys",
                crate::format::grouped(result.changed),
                crate::format::grouped(change)
            ));
            if b.failed > 0 {
                stopped = Some(if b.denied {
                    "the server refused EXPIRE for this user".to_string()
                } else {
                    format!(
                        "the server answered EXPIRE with an error for {} keys",
                        crate::format::grouped(b.failed)
                    )
                });
                break 'nodes;
            }
            if b.finished() {
                break;
            }
            (walk, carry) = (b.walk, b.carry);
        }
    }
    writeln!(
        out,
        "Set a TTL of {} on {} keys of {name}; {} left alone (a TTL already, or gone).",
        bulk::ttl_text(args.ttl),
        crate::format::grouped(result.changed),
        crate::format::grouped(result.skipped)
    )?;
    if result.moved > 0 {
        writeln!(out, "{}.", capital(&bulk::moved_text(result.moved)))?;
    }
    if let Some(f) = &result.file {
        if shards.iter().any(|s| s.node.is_some()) {
            writeln!(
                out,
                "Key names (each line: the node, a tab, the key): {}",
                f.display()
            )?;
        } else {
            writeln!(out, "Key names: {}", f.display())?;
        }
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

/// One server the run walks: a cluster primary (its announced address in `node`), or
/// the profile's standalone server (`node` is `None`), with its own budget.
struct Shard<C> {
    node: Option<String>,
    conn: C,
    budget: std::sync::Arc<Budget>,
}

/// The primaries of a cluster discovered on its primaries, each with a budget of its
/// own (`rate` per node): `(address, role, connection)` per shard, in slot order.
/// Refused when a shard has no connection (the run would miss its keys) or was
/// reached on a replica, which takes no writes.
fn primaries<C>(
    found: Vec<(String, crate::model::NodeRole, Option<C>)>,
    rate: Option<u32>,
) -> Result<Vec<Shard<C>>, String> {
    let mut out = Vec::with_capacity(found.len());
    for (addr, role, conn) in found {
        let addr = crate::value::escape(&addr);
        if role != crate::model::NodeRole::Primary {
            return Err(format!(
                "Not run: the shard at {addr} was reached on a replica, which takes no writes"
            ));
        }
        let Some(conn) = conn else {
            return Err(format!(
                "Not run: the primary {addr} could not be connected to, so the pattern can't \
                 be expired on every primary"
            ));
        };
        out.push(Shard {
            node: Some(addr),
            conn,
            budget: Budget::new(rate),
        });
    }
    if out.is_empty() {
        return Err("Not run: the cluster lists no primary".to_string());
    }
    Ok(out)
}

/// The first letter in upper case.
fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
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
            tags: Vec::new(),
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
    fn on_a_cluster_it_runs_on_every_primary_and_never_a_replica() {
        use crate::model::NodeRole::{Primary, Replica};
        let found = |roles: &[(crate::model::NodeRole, bool)]| {
            roles
                .iter()
                .enumerate()
                .map(|(i, &(r, c))| (format!("10.0.1.{i}:6379"), r, c.then_some(i)))
                .collect::<Vec<_>>()
        };
        let all = primaries(found(&[(Primary, true), (Primary, true)]), Some(10)).unwrap();
        assert_eq!(
            all.iter().map(|s| s.node.clone()).collect::<Vec<_>>(),
            [Some("10.0.1.0:6379".into()), Some("10.0.1.1:6379".into())]
        );
        // One budget per node, never one for the cluster.
        assert!(!std::sync::Arc::ptr_eq(&all[0].budget, &all[1].budget));
        let e = primaries(found(&[(Primary, true), (Replica, true)]), None)
            .err()
            .unwrap();
        assert!(e.contains("10.0.1.1:6379") && e.contains("replica"), "{e}");
        let e = primaries(found(&[(Primary, true), (Primary, false)]), None)
            .err()
            .unwrap();
        assert!(e.contains("could not be connected"), "{e}");
        assert!(primaries::<u8>(Vec::new(), None).is_err());
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
