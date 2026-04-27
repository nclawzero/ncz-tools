use anyhow::Result;
use std::io::{self, BufRead, Write};
use tracing::info;

use crate::cli::agent::AgentClient;
use crate::cli::client::Session;
use crate::cli::commands::CommandHandler;
use crate::cli::input::InputHistory;
use crate::cli::storage;
use crate::cli::theme::Theme;
use crate::cli::ui::{self, StatusBar};
use std::sync::Arc;
use tokio::sync::Mutex;

/// REPL loop state
pub struct ReplLoop {
    /// Shared App. ReplLoop + CommandHandler both lock this
    /// briefly to resolve the active workspace's client on each
    /// turn. Supports runtime /workspace switch (chunk D-3b).
    app: Arc<Mutex<crate::cli::workspace::App>>,
    session: Session,
    reader: io::BufReader<io::Stdin>,
    model: String,
    provider: String,
    history: InputHistory,
    command_handler: CommandHandler,
    status_bar: StatusBar,
}

impl ReplLoop {
    /// Create a new REPL loop around a shared Arc<Mutex<App>>.
    /// Active workspace client is resolved on every submit_turn.
    pub fn new(
        app: Arc<Mutex<crate::cli::workspace::App>>,
        session: Session,
        model: String,
        provider: String,
    ) -> Result<Self> {
        let history = InputHistory::load_from_file()?;
        let command_handler = CommandHandler::new(app.clone());
        let status_bar = StatusBar::new(model.clone(), provider.clone(), session.name.clone());

        Ok(Self {
            app,
            session,
            reader: io::BufReader::new(io::stdin()),
            model,
            provider,
            history,
            command_handler,
            status_bar,
        })
    }

    async fn resolve_active_client(
        &self,
    ) -> Result<Arc<Mutex<Box<dyn AgentClient + Send + Sync>>>> {
        let app = self.app.lock().await;
        app.active_workspace()
            .and_then(|w| w.client.clone())
            .ok_or_else(|| anyhow::anyhow!("no active workspace with an activated client"))
    }

    /// Run the REPL loop
    pub async fn run(&mut self) -> Result<()> {
        self.print_banner();

        loop {
            // Print status bar
            println!("\n{}", self.status_bar.render());

            // Print prompt with theme
            print!(
                "{}📝 You{}:{} ",
                Theme::BRIGHT_BLUE,
                Theme::RESET,
                Theme::CYAN
            );
            io::stdout().flush()?;

            // Read input
            let mut input = String::new();
            let bytes_read = self.reader.read_line(&mut input)?;
            print!("{}", Theme::RESET);

            if bytes_read == 0 {
                // EOF
                println!("\n👋 Goodbye!");
                break;
            }

            let input = input.trim().to_string();

            // Handle empty input
            if input.is_empty() {
                continue;
            }

            // Add to history
            self.history.push(input.clone());

            // Handle commands
            if input.starts_with('/') {
                match self.command_handler.handle(&input, &self.session.id).await {
                    Ok(Some(text)) => {
                        // Handlers that were refactored to return
                        // their output as a String (so the Turbo
                        // Vision UI can render them) — print it
                        // here so the rustyline REPL UX is
                        // unchanged.
                        print!("{}", text);
                        if !text.ends_with('\n') {
                            println!();
                        }
                    }
                    Ok(None) => {
                        // Handler printed directly to stdout.
                    }
                    Err(e) if e.to_string() == "EXIT" => {
                        println!("\n👋 Goodbye!");
                        self.history.save_to_file()?;
                        break;
                    }
                    Err(e) => {
                        ui::print_error(&e.to_string(), None);
                    }
                }
                continue;
            }

            // Submit turn and stream response
            info!("Submitting turn: {}", input);
            print!(
                "{}🤖 Agent{}:{} ",
                Theme::BRIGHT_GREEN,
                Theme::RESET,
                Theme::CYAN
            );
            io::stdout().flush()?;

            let active_client = match self.resolve_active_client().await {
                Ok(c) => c,
                Err(e) => {
                    ui::print_error("no active workspace", Some(&e.to_string()));
                    continue;
                }
            };
            let turn_res = {
                let mut guard = active_client.lock().await;
                guard.submit_turn(&self.session.id, &input).await
            };
            match turn_res {
                Ok(_response) => {
                    // Response already printed by streaming handler
                    // Update session metadata
                    if let Err(e) = self.update_session_metadata().await {
                        eprintln!("⚠️  Could not update session metadata: {}", e);
                    }
                }
                Err(e) => {
                    eprintln!("\n❌ Error: {}", e);
                }
            }
        }

        // Save history on exit
        self.history.save_to_file()?;
        Ok(())
    }

    /// Print REPL banner with theme colors
    fn print_banner(&self) {
        println!(
            "\n{}╔════════════════════════════════════════════════════════════╗{}",
            Theme::CYAN,
            Theme::RESET
        );
        println!(
            "{}║{}                   🎯 ZTerm Chat REPL{}                      {}║{}",
            Theme::CYAN,
            Theme::BRIGHT_CYAN,
            Theme::RESET,
            Theme::CYAN,
            Theme::RESET
        );
        println!(
            "{}╚════════════════════════════════════════════════════════════╝{}",
            Theme::CYAN,
            Theme::RESET
        );
        println!();
        println!(
            "{}Model{}:   {} ({})",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            self.model,
            self.provider
        );
        println!(
            "{}Session{}:  {}{}",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            self.session.name,
            Theme::RESET
        );
        println!();
        println!(
            "{}Commands{}: /help, /info, /exit, or just type to chat{}",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            Theme::RESET
        );
        println!();
    }

    /// Print help message with theme colors
    fn print_help(&self) {
        println!();
        println!("{}Available commands:{}", Theme::BRIGHT_CYAN, Theme::RESET);
        println!(
            "  {}❓ /help{} - Show this help",
            Theme::BRIGHT_BLUE,
            Theme::RESET
        );
        println!(
            "  {}ℹ️  /info{} - Show current session info",
            Theme::BRIGHT_BLUE,
            Theme::RESET
        );
        println!(
            "  {}🚪 /exit{} - Exit ZTerm",
            Theme::BRIGHT_BLUE,
            Theme::RESET
        );
        println!();
        println!(
            "{}Just type a message to chat with the agent!{}",
            Theme::BRIGHT_BLUE,
            Theme::RESET
        );
        println!();
    }

    /// Print session info with theme colors
    fn print_info(&self) {
        println!();
        println!("{}Session Information:{}", Theme::BRIGHT_CYAN, Theme::RESET);
        println!(
            "  {}Model{}:    {}",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            self.model
        );
        println!(
            "  {}Provider{}: {}",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            self.provider
        );
        println!(
            "  {}Session{}:  {}",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            self.session.name
        );
        println!(
            "  {}ID{}:       {}",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            self.session.id
        );
        println!();
    }

    /// Update session metadata
    async fn update_session_metadata(&self) -> Result<()> {
        // For now, just update the last_active time
        let metadata = storage::load_session_metadata(&self.session.id)?;

        let updated = crate::cli::storage::SessionMetadata {
            last_active: chrono::Utc::now().to_rfc3339(),
            ..metadata
        };

        storage::save_session_metadata(&updated)?;
        Ok(())
    }
}
