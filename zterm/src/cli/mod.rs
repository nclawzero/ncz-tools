use clap::{Parser, Subcommand};

pub mod agent;
pub mod aliases;
pub mod batch;
pub mod client;
pub mod commands;
pub mod error_handler;
pub mod input;
pub mod mnemos;
pub mod openclaw;
pub mod pagination;
pub mod pairing;
pub mod retry;
pub mod session_search;
pub mod storage;
pub mod streaming;
pub mod theme;
pub mod tui;
pub mod ui;
pub(crate) mod url_safety;
pub mod websocket;
pub mod workspace;

#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[derive(Parser, Debug)]
#[command(name = "zterm")]
#[command(about = "ZTerm: Terminal REPL for Zeroclaw", long_about = None)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, global = true)]
    pub log_level: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Launch interactive terminal REPL
    Tui {
        /// Session name (creates or switches to session)
        #[arg(long)]
        session_name: Option<String>,

        /// Remote gateway URL (default: http://localhost:8888)
        #[arg(long)]
        remote: Option<String>,

        /// Bearer token (if not configured locally)
        #[arg(long)]
        token: Option<String>,

        /// Workspace to boot into. If `~/.zterm/config.toml` has
        /// `[[workspaces]]` entries, this picks the initial active
        /// workspace (overriding the config's `active = "..."`
        /// field). Ignored in single-workspace / synthesized mode.
        /// Inside the REPL use `/workspace switch <name>` to
        /// change workspaces at runtime.
        #[arg(long)]
        workspace: Option<String>,

        /// Use the legacy rustyline REPL instead of the
        /// Turbo Vision UI (v0.3 `tv_ui` scaffold). Transition
        /// fallback while the Paradox 4.5-flavored UX lands in
        /// incremental E-1..E-8 slices.
        #[arg(long)]
        legacy_repl: bool,
    },
}
