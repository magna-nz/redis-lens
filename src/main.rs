use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = redis_lens::cli::Cli::parse();
    match redis_lens::cli::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::from(redis_lens::cli::exit_code(&e))
        }
    }
}
