//! Command-line interface. Running with no subcommand opens the TUI.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::model::Env;

pub mod conn;
pub mod scan;

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
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let config = cli.config;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => crate::tui::run(config).await,
        Command::Conn(cmd) => conn::run(config, cmd).await,
        Command::Scan(args) => scan::run(config, args).await,
        Command::Discover => conn::discover(config).await,
    }
}
