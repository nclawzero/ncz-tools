use anyhow::Result;
use chrono::Utc;
use std::io::{self, BufRead, Write};
use tracing::{info, warn};

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

    async fn apply_legacy_session_action(&mut self, action: LegacySessionAction<'_>) -> Result<()> {
        let active_client = self.resolve_active_client().await?;
        let target = action.target();

        let resolution = {
            let locked = active_client.lock().await;
            match action {
                LegacySessionAction::Switch { .. } => {
                    plan_legacy_session_resolution(target, locked.list_sessions().await)?
                }
                LegacySessionAction::Create { .. } => {
                    plan_legacy_session_create(target, locked.list_sessions().await)?;
                    LegacySessionResolution::Create
                }
            }
        };

        let session = match resolution {
            LegacySessionResolution::Existing(session) => session,
            LegacySessionResolution::Create => {
                let session = active_client.lock().await.create_session(target).await?;
                if let Err(e) = save_legacy_session_metadata(&session) {
                    warn!(
                        "could not save local metadata for newly created session {}: {}",
                        session.id, e
                    );
                }
                session
            }
        };

        self.session = session;
        self.status_bar.set_session(self.session.name.clone());
        Ok(())
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
                if let Some(action) = legacy_session_action(&input) {
                    match self.apply_legacy_session_action(action).await {
                        Ok(()) => {
                            println!("✅ Active backend session: {}", self.session.name);
                        }
                        Err(e) => {
                            ui::print_error(&e.to_string(), None);
                        }
                    }
                    continue;
                }

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
            last_active: Utc::now().to_rfc3339(),
            ..metadata
        };

        storage::save_session_metadata(&updated)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacySessionAction<'a> {
    Switch { target: &'a str },
    Create { target: &'a str },
}

impl<'a> LegacySessionAction<'a> {
    fn target(self) -> &'a str {
        match self {
            LegacySessionAction::Switch { target } | LegacySessionAction::Create { target } => {
                target
            }
        }
    }
}

fn legacy_session_action(cmdline: &str) -> Option<LegacySessionAction<'_>> {
    let mut parts = cmdline.split_whitespace();
    if parts.next()? != "/session" {
        return None;
    }

    match parts.next()? {
        "list" | "info" | "delete" => None,
        "switch" => Some(LegacySessionAction::Switch {
            target: parts.next()?,
        }),
        "create" => Some(LegacySessionAction::Create {
            target: parts.next()?,
        }),
        name => Some(LegacySessionAction::Switch { target: name }),
    }
}

#[derive(Debug)]
enum LegacySessionResolution {
    Existing(Session),
    Create,
}

fn plan_legacy_session_resolution(
    requested: &str,
    list_result: Result<Vec<Session>>,
) -> Result<LegacySessionResolution> {
    let sessions = list_result
        .map_err(|e| anyhow::anyhow!("could not list sessions from active backend: {e}"))?;
    match choose_legacy_session_by_id_or_name(&sessions, requested)? {
        Some(session) => Ok(LegacySessionResolution::Existing(session.clone())),
        None => Ok(LegacySessionResolution::Create),
    }
}

fn plan_legacy_session_create(requested: &str, list_result: Result<Vec<Session>>) -> Result<()> {
    let sessions = list_result
        .map_err(|e| anyhow::anyhow!("could not list sessions from active backend: {e}"))?;

    let conflicts: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id == requested || session.name == requested)
        .collect();
    if conflicts.is_empty() {
        return Ok(());
    }

    Err(duplicate_legacy_session_create_error(requested, conflicts))
}

fn choose_legacy_session_by_id_or_name<'a>(
    sessions: &'a [Session],
    requested: &str,
) -> Result<Option<&'a Session>> {
    let id_matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id == requested)
        .collect();
    match id_matches.as_slice() {
        [session] => return Ok(Some(*session)),
        [] => {}
        _ => {
            return Err(ambiguous_legacy_session_error(
                requested,
                "backend session id",
                id_matches,
            ));
        }
    }

    let name_matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.name == requested)
        .collect();
    match name_matches.as_slice() {
        [session] => Ok(Some(*session)),
        [] => Ok(None),
        _ => Err(ambiguous_legacy_session_error(
            requested,
            "session name",
            name_matches,
        )),
    }
}

fn duplicate_legacy_session_create_error(
    requested: &str,
    conflicts: Vec<&Session>,
) -> anyhow::Error {
    let candidates = conflicts
        .iter()
        .map(|session| format!("backend id={} name={}", session.id, session.name))
        .collect::<Vec<_>>()
        .join("; ");

    anyhow::anyhow!(
        "backend session id/name '{requested}' already exists; refusing explicit create. Candidates: {candidates}"
    )
}

fn ambiguous_legacy_session_error(
    requested: &str,
    label: &str,
    candidates: Vec<&Session>,
) -> anyhow::Error {
    let candidates = candidates
        .iter()
        .map(|session| format!("backend id={} name={}", session.id, session.name))
        .collect::<Vec<_>>()
        .join("; ");

    anyhow::anyhow!("ambiguous {label} '{requested}'; use an explicit id. Candidates: {candidates}")
}

fn save_legacy_session_metadata(session: &Session) -> Result<()> {
    let metadata = storage::SessionMetadata {
        id: session.id.clone(),
        name: session.name.clone(),
        model: session.model.clone(),
        provider: session.provider.clone(),
        created_at: Utc::now().to_rfc3339(),
        message_count: 0,
        last_active: Utc::now().to_rfc3339(),
    };

    if storage::is_safe_session_id(&metadata.id) {
        storage::save_session_metadata(&metadata)?;
    } else {
        warn!(
            "not saving local metadata for unsafe session id: {}",
            metadata.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, name: &str) -> Session {
        Session {
            id: id.to_string(),
            name: name.to_string(),
            model: "primary".to_string(),
            provider: "test".to_string(),
        }
    }

    #[test]
    fn legacy_session_action_parses_only_switch_create_and_bare() {
        assert_eq!(
            legacy_session_action("/session research"),
            Some(LegacySessionAction::Switch { target: "research" })
        );
        assert_eq!(
            legacy_session_action("/session switch research"),
            Some(LegacySessionAction::Switch { target: "research" })
        );
        assert_eq!(
            legacy_session_action("/session create scratch"),
            Some(LegacySessionAction::Create { target: "scratch" })
        );
        assert_eq!(legacy_session_action("/session list"), None);
        assert_eq!(legacy_session_action("/session info"), None);
        assert_eq!(legacy_session_action("/session delete research"), None);
        assert_eq!(legacy_session_action("/session switch"), None);
    }

    #[test]
    fn legacy_session_resolution_switch_selects_existing_backend_id() {
        let sessions = vec![
            session("sess-123", "Research"),
            session("sess-456", "sess-123"),
        ];

        let resolution = plan_legacy_session_resolution("sess-123", Ok(sessions))
            .expect("successful backend listing should resolve by id");

        match resolution {
            LegacySessionResolution::Existing(session) => assert_eq!(session.id, "sess-123"),
            LegacySessionResolution::Create => panic!("expected existing session resolution"),
        }
    }

    #[test]
    fn legacy_session_create_fails_on_existing_name() {
        let sessions = vec![session("sess-123", "Research")];

        let err = plan_legacy_session_create("Research", Ok(sessions)).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("already exists"));
        assert!(msg.contains("backend id=sess-123 name=Research"));
    }
}
