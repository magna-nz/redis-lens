use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = redis_lens::cli::Cli::parse();
    redis_lens::cli::run(cli).await
}
