//! Command-line interface. Running with no subcommand opens the TUI.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::model::{Env, Provider};

pub mod compare;
pub mod conn;
pub mod diff;
pub mod expire;
pub mod scan;
pub mod snapshots;
pub mod trend;

/// `scan --check`: one or more rules failed. The failures are already printed. Exit code 1.
#[derive(Debug, thiserror::Error)]
#[error("{0} check(s) failed")]
pub struct ChecksFailed(pub usize);

/// `scan --check`: the scan itself failed, so no rule could be judged. Exit code 2, so a
/// CI gate can tell a failing rule from a broken scan.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct GateError(pub anyhow::Error);

/// `--keep N`: a count of at least 1. The newest snapshot is always kept, so 0 would
/// silently mean 1.
fn keep_count(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1 (the newest snapshot is always kept)".to_string()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

/// The process exit code for an error from [`run`]: 2 for [`GateError`], 1 otherwise
/// ([`ChecksFailed`] included).
pub fn exit_code(err: &anyhow::Error) -> u8 {
    if err.downcast_ref::<GateError>().is_some() {
        2
    } else {
        1
    }
}

#[derive(Debug, Parser)]
#[command(name = "redis-lens", version, about, long_about = None)]
pub struct Cli {
    /// Config file to use instead of the default location.
    #[arg(long, global = true, env = "REDIS_LENS_CONFIG")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the terminal UI. This is the default.
    Tui,
    /// Manage saved connections. Credentials go to the OS keychain.
    #[command(subcommand)]
    Conn(ConnCommand),
    /// Scan a saved connection and print the pattern report.
    Scan(ScanArgs),
    /// List Redis and Valkey containers running in Docker.
    Discover,
    /// Scan a saved connection and compare it with its previous snapshot.
    Diff(DiffArgs),
    /// List or prune a profile's saved snapshots.
    #[command(subcommand)]
    Snapshots(SnapshotsCommand),
    /// Show each pattern's memory over a profile's recent snapshots.
    Trend(TrendArgs),
    /// Compare the patterns of two connections, e.g. staging and prod.
    Compare(CompareArgs),
    /// Set a TTL on every key of a pattern that has none. A dry run unless --apply.
    Expire(expire::ExpireArgs),
}

#[derive(Debug, Subcommand)]
pub enum SnapshotsCommand {
    /// List a profile's snapshots, newest first.
    List {
        /// Profile name.
        profile: String,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete a profile's old snapshots. Only files in redis-lens's own snapshot
    /// directory are touched.
    #[command(group(clap::ArgGroup::new("retention").required(true).multiple(true).args(["keep", "keep_days"])))]
    Prune {
        /// Profile name.
        profile: String,
        /// Keep only the newest N snapshots (N is at least 1).
        #[arg(long, value_parser = keep_count)]
        keep: Option<usize>,
        /// Delete snapshots older than D days.
        #[arg(long)]
        keep_days: Option<u64>,
        /// Delete without asking. Without it, prune only lists what it would delete.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct TrendArgs {
    /// Profile name.
    pub profile: String,
    /// Only patterns containing this text.
    #[arg(long)]
    pub pattern: Option<String>,
    /// How many of the newest snapshots to use.
    #[arg(long, default_value_t = 7)]
    pub last: usize,
    /// Print as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct CompareArgs {
    /// The left profile (scanned unless --left-file is given).
    pub left: String,
    /// The right profile (scanned unless --right-file is given).
    pub right: String,
    /// Use this snapshot file for the left side instead of scanning.
    #[arg(long)]
    pub left_file: Option<PathBuf>,
    /// Use this snapshot file for the right side instead of scanning.
    #[arg(long)]
    pub right_file: Option<PathBuf>,
    /// Maximum number of keys to sample on each side.
    #[arg(long, default_value_t = 100_000)]
    pub sample: u64,
    /// Upper bound on commands per second for each scan (0 means no limit).
    #[arg(long)]
    pub max_rate: Option<u32>,
    /// Print as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConnCommand {
    /// Save a connection.
    ///
    /// A username or password in the URL is moved to the OS keychain and never
    /// written to the config file. Prefer --password-stdin or --ask-password so the
    /// password stays out of your shell history.
    Add(ConnAddArgs),
    /// List saved connections.
    List,
    /// Remove a saved connection and its keychain entry.
    Remove {
        /// Profile name.
        name: String,
    },
    /// Connect and run the permission check.
    Test {
        /// Profile name.
        name: String,
    },
    /// Write the saved connections as TOML `[[profile]]` tables, to FILE or standard
    /// output. No credential is in them: those stay in the OS keychain, which is never
    /// read here.
    Export {
        /// The file to write (mode 0600). Standard output without it.
        file: Option<std::path::PathBuf>,
        /// Replace FILE when it exists.
        #[arg(long, requires = "file")]
        force: bool,
    },
    /// Add the connections of a file written by `conn export`. All or none: nothing is
    /// saved when any is refused. Credentials are not in the file; store them with
    /// `conn add <name> --force ...` afterwards.
    Import {
        /// The file to read.
        file: std::path::PathBuf,
        /// Replace a profile that has the same name.
        #[arg(long)]
        force: bool,
        /// Answer yes to the question asked for a profile with a token command.
        #[arg(long)]
        yes: bool,
    },
    /// Change a saved connection: turn writes on or off, add or remove tags.
    #[command(group(
        clap::ArgGroup::new("change")
            .required(true)
            .multiple(true)
            .args(["writes", "tag", "untag"])
    ))]
    Set {
        /// Profile name.
        name: String,
        /// Turn writes on or off (`read_only` in the config file). Turning them on asks
        /// first; on a prod profile you type the profile name.
        #[arg(long, value_enum)]
        writes: Option<WritesArg>,
        /// The profile name, given instead of typing it when writes are turned on.
        #[arg(long, value_name = "NAME", requires = "writes")]
        confirm: Option<String>,
        /// Turn writes on without asking. Never enough on a prod profile: give --confirm.
        #[arg(long, requires = "writes")]
        yes: bool,
        /// Add a tag (1 to 32 letters, digits, `-`, `_`, `.` or `/`). Repeatable.
        #[arg(long, value_name = "TAG")]
        tag: Vec<String>,
        /// Remove a tag. Repeatable.
        #[arg(long, value_name = "TAG")]
        untag: Vec<String>,
    },
}

/// `--writes on|off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WritesArg {
    On,
    Off,
}

#[derive(Debug, Args)]
pub struct ConnAddArgs {
    /// Profile name, e.g. prod-cache-01.
    pub name: String,
    /// redis://, rediss://, or a StackExchange.Redis string such as host:6380,ssl=True.
    #[arg(long)]
    pub url: String,
    /// Environment. Guessed from the host when omitted.
    #[arg(long, value_enum)]
    pub env: Option<EnvArg>,
    /// Read the password from standard input.
    #[arg(long, conflicts_with = "ask_password")]
    pub password_stdin: bool,
    /// Prompt for the password without echoing it.
    #[arg(long)]
    pub ask_password: bool,
    /// Sign in with Microsoft Entra ID (Azure). Needs the entra-id feature.
    #[arg(long)]
    pub entra: bool,
    /// Replace an existing profile with the same name.
    #[arg(long)]
    pub force: bool,
    /// Scan a replica instead of the primary when one is reachable.
    #[arg(long)]
    pub prefer_replica: bool,
    /// The managed service, when the host name doesn't show it.
    #[arg(long, value_enum)]
    pub provider: Option<ProviderArg>,
    /// Authenticate with a short-lived token printed by a command, run at every connect,
    /// e.g. `-- az account get-access-token --resource https://redis.azure.com --query
    /// accessToken -o tsv`. Give the program and its arguments after `--`; it is not run
    /// through a shell.
    #[arg(last = true, value_name = "TOKEN_COMMAND")]
    pub token_command: Vec<String>,
    /// Answer yes to the questions: saving a token command, and turning writes on for a
    /// profile that isn't prod.
    #[arg(long)]
    pub yes: bool,
    /// Writes on or off. Unset, a prod profile is read-only and the others are not.
    /// Turning writes on for a prod profile needs --confirm with its name.
    #[arg(long, value_enum)]
    pub writes: Option<WritesArg>,
    /// The profile name, given instead of typing it when writes are turned on.
    #[arg(long, value_name = "NAME")]
    pub confirm: Option<String>,
    /// Tag the profile (1 to 32 letters, digits, `-`, `_`, `.` or `/`). Repeatable.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
    /// Reach the server, and its cluster nodes and replicas, through an SSH bastion:
    /// `[USER@]HOST[:PORT]`, or a `Host` alias from ~/.ssh/config. Uses the system ssh
    /// with strict host-key checking; the username goes to the OS keychain.
    #[arg(long, value_name = "[USER@]HOST[:PORT]")]
    pub ssh: Option<String>,
    /// A private key file for the SSH bastion (ssh -i). An encrypted key must be loaded
    /// in the SSH agent: redis-lens never asks for a passphrase.
    #[arg(long, requires = "ssh", value_name = "FILE")]
    pub ssh_identity: Option<std::path::PathBuf>,
    /// A known_hosts file to check the bastion's host key against, instead of ssh's own.
    #[arg(long, requires = "ssh", value_name = "FILE")]
    pub ssh_known_hosts: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderArg {
    Elasticache,
    ElasticacheServerless,
    Memorydb,
    Azure,
    Memorystore,
}

impl From<ProviderArg> for Provider {
    fn from(p: ProviderArg) -> Self {
        match p {
            ProviderArg::Elasticache => Provider::Elasticache,
            ProviderArg::ElasticacheServerless => Provider::ElasticacheServerless,
            ProviderArg::Memorydb => Provider::Memorydb,
            ProviderArg::Azure => Provider::Azure,
            ProviderArg::Memorystore => Provider::Memorystore,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EnvArg {
    Prod,
    Stage,
    Local,
    Other,
}

impl From<EnvArg> for Env {
    fn from(e: EnvArg) -> Self {
        match e {
            EnvArg::Prod => Env::Prod,
            EnvArg::Stage => Env::Stage,
            EnvArg::Local => Env::Local,
            EnvArg::Other => Env::Other,
        }
    }
}

#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Profile name.
    pub profile: String,
    /// Maximum number of keys to sample.
    #[arg(long, default_value_t = 100_000)]
    pub sample: u64,
    /// Only scan keys matching this glob, e.g. "user:*".
    #[arg(long = "match")]
    pub match_glob: Option<String>,
    /// Print the report as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Upper bound on commands per second (0 means no limit). Defaults to no limit for
    /// local profiles and 5,000 otherwise.
    #[arg(long)]
    pub max_rate: Option<u32>,
    /// Save the report as a snapshot for later comparison.
    #[arg(long)]
    pub save: bool,
    /// With --save, keep only the newest N snapshots of this profile (N is at least 1).
    #[arg(long, requires = "save", value_parser = keep_count)]
    pub keep: Option<usize>,
    /// With --save, delete this profile's snapshots older than D days.
    #[arg(long, requires = "save")]
    pub keep_days: Option<u64>,
    /// Fail with exit code 1 when a rule fails, e.g. `no-ttl-pct<20`. Repeatable. A
    /// broken scan exits with 2.
    #[arg(long = "check", value_name = "RULE")]
    pub checks: Vec<String>,
    /// Read rules from the `[check]` table of a TOML file.
    #[arg(long, value_name = "FILE")]
    pub rules: Option<PathBuf>,
    /// Also fail rules that could not be judged, e.g. a size rule without MEMORY USAGE.
    #[arg(long)]
    pub strict: bool,
    /// Write a report in this format instead of the table.
    #[arg(long, value_enum, conflicts_with = "json")]
    pub report: Option<ReportFormat>,
    /// Where to write the report. Standard output when omitted.
    #[arg(short = 'o', long, requires = "report")]
    pub output: Option<PathBuf>,
    /// Include example key names in the report. Left out by default, since key names
    /// can hold user ids or emails.
    #[arg(long, requires = "report")]
    pub examples: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReportFormat {
    Md,
    Html,
    Csv,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Profile name.
    pub profile: String,
    /// Compare with this snapshot file instead of the newest one for the profile.
    #[arg(long)]
    pub against: Option<PathBuf>,
    /// Upper bound on commands per second (0 means no limit).
    #[arg(long)]
    pub max_rate: Option<u32>,
    /// Print the diff as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let config = cli.config;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => crate::tui::run(config).await,
        Command::Conn(cmd) => conn::run(config, cmd).await,
        Command::Scan(args) => scan::run(config, args).await,
        Command::Discover => conn::discover(config).await,
        Command::Diff(args) => diff::run(config, args).await,
        Command::Snapshots(cmd) => snapshots::run(config, cmd).await,
        Command::Trend(args) => trend::run(config, args).await,
        Command::Compare(args) => compare::run(config, args).await,
        Command::Expire(args) => expire::run(config, args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_needs_a_retention_rule() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv);
        let e = parse(&["redis-lens", "snapshots", "prune", "p"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(parse(&["redis-lens", "snapshots", "prune", "p", "--keep", "3"]).is_ok());
        assert!(parse(&["redis-lens", "snapshots", "prune", "p", "--keep-days", "7"]).is_ok());
        assert!(
            parse(&[
                "redis-lens",
                "snapshots",
                "prune",
                "p",
                "--keep",
                "3",
                "--keep-days",
                "7"
            ])
            .is_ok()
        );
    }

    #[test]
    fn keep_must_be_at_least_one() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv);
        for argv in [
            &["redis-lens", "snapshots", "prune", "p", "--keep", "0"][..],
            &["redis-lens", "scan", "p", "--save", "--keep", "0"][..],
        ] {
            let e = parse(argv).unwrap_err();
            assert_eq!(
                e.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{argv:?}"
            );
        }
        assert!(parse(&["redis-lens", "scan", "p", "--save", "--keep", "1"]).is_ok());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&anyhow::Error::from(ChecksFailed(2))), 1);
        assert_eq!(exit_code(&GateError(anyhow::anyhow!("boom")).into()), 2);
        assert_eq!(exit_code(&anyhow::anyhow!("other")), 1);
    }
}
