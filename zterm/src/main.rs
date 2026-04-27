use anyhow::Result;
use clap::Parser;
use zterm::cli::{self, Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env if present (silent if missing). Enables local-dev
    // and test-time configuration of daemon URLs, tokens, and
    // optional MNEMOS memory integration without committing secrets.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Tui {
            session_name,
            remote,
            token,
            workspace,
            legacy_repl,
        } => cli::tui::run(session_name, remote, token, workspace, legacy_repl).await,
    }
}
