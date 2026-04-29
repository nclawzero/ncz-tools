use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use tracing::{info, warn};

use crate::cli::agent::AgentClient;
use crate::cli::client::Session;
use crate::cli::commands::{tokenize_slash_command, CommandHandler};
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
    workspace_sessions: HashMap<String, ReplSessionBinding>,
    fallback_session_name: String,
    reader: io::BufReader<io::Stdin>,
    model: String,
    provider: String,
    history: InputHistory,
    command_handler: CommandHandler,
    status_bar: StatusBar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplSessionBinding {
    id: String,
    name: String,
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
        let fallback_session_name = session.name.clone();

        Ok(Self {
            app,
            session,
            workspace_sessions: HashMap::new(),
            fallback_session_name,
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

    async fn current_workspace_name(&self) -> Result<String> {
        let app = self.app.lock().await;
        app.active_workspace()
            .map(|w| w.config.name.clone())
            .ok_or_else(|| anyhow::anyhow!("no active workspace"))
    }

    async fn current_storage_scope(&self) -> Result<storage::LocalWorkspaceScope> {
        let app = self.app.lock().await;
        let workspace = app
            .active_workspace()
            .ok_or_else(|| anyhow::anyhow!("no active workspace"))?;
        storage::workspace_scope(
            workspace.config.backend.as_str(),
            &workspace.config.name,
            workspace.config.id.as_deref(),
        )
    }

    fn remember_active_workspace_session(&mut self, workspace_name: String, session: &Session) {
        self.workspace_sessions.insert(
            workspace_name,
            ReplSessionBinding {
                id: session.id.clone(),
                name: session.name.clone(),
            },
        );
    }

    async fn load_active_workspace_session(&self, session_id: &str) -> Result<Session> {
        let active_client = self.resolve_active_client().await?;
        let locked = active_client.lock().await;
        locked.load_session(session_id).await
    }

    async fn resolve_or_create_active_workspace_session(&self, target: &str) -> Result<Session> {
        let active_client = self.resolve_active_client().await?;
        let resolution = {
            let locked = active_client.lock().await;
            plan_legacy_session_resolution(target, locked.list_sessions().await)?
        };

        match resolution {
            LegacySessionResolution::Existing(session) => Ok(session),
            LegacySessionResolution::Create => {
                let session = active_client.lock().await.create_session(target).await?;
                let scope = self.current_storage_scope().await?;
                if let Err(e) = save_legacy_session_metadata(&scope, &session) {
                    warn!(
                        "could not save local metadata for newly created session {}: {}",
                        session.id, e
                    );
                }
                Ok(session)
            }
        }
    }

    async fn ensure_session_for_active_workspace(&mut self) -> Result<String> {
        let workspace_name = self.current_workspace_name().await?;
        let session = if let Some(binding) = self.workspace_sessions.get(&workspace_name).cloned() {
            match self.load_active_workspace_session(&binding.id).await {
                Ok(session) => session,
                Err(_) => {
                    self.resolve_or_create_active_workspace_session(&binding.name)
                        .await?
                }
            }
        } else {
            self.resolve_or_create_active_workspace_session(&self.fallback_session_name)
                .await?
        };

        let session_id = session.id.clone();
        self.session = session.clone();
        self.status_bar.set_session(self.session.name.clone());
        self.remember_active_workspace_session(workspace_name, &session);
        Ok(session_id)
    }

    async fn apply_legacy_session_action(&mut self, action: LegacySessionAction) -> Result<()> {
        let active_client = self.resolve_active_client().await?;
        let target = action.target().to_string();

        let resolution = {
            let locked = active_client.lock().await;
            match action {
                LegacySessionAction::Switch { .. } => {
                    plan_legacy_session_resolution(&target, locked.list_sessions().await)?
                }
                LegacySessionAction::Create { .. } => LegacySessionResolution::Create,
            }
        };

        let session = match resolution {
            LegacySessionResolution::Existing(session) => session,
            LegacySessionResolution::Create => {
                let session = active_client.lock().await.create_session(&target).await?;
                let scope = self.current_storage_scope().await?;
                if let Err(e) = save_legacy_session_metadata(&scope, &session) {
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
        let workspace_name = self.current_workspace_name().await?;
        let session = self.session.clone();
        self.remember_active_workspace_session(workspace_name, &session);
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
                match self.handle_slash_command(&input).await {
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

            let session_id = match self.ensure_session_for_active_workspace().await {
                Ok(session_id) => session_id,
                Err(e) => {
                    ui::print_error(
                        "could not prepare session for active workspace",
                        Some(&e.to_string()),
                    );
                    continue;
                }
            };
            let active_client = match self.resolve_active_client().await {
                Ok(c) => c,
                Err(e) => {
                    ui::print_error("no active workspace", Some(&e.to_string()));
                    continue;
                }
            };
            let transcript_scope = match self.current_storage_scope().await {
                Ok(scope) => Some(scope),
                Err(e) => {
                    warn!(
                        "could not resolve transcript scope for session {}: {e}",
                        session_id
                    );
                    None
                }
            };
            append_repl_transcript_entry_best_effort(
                transcript_scope.as_ref(),
                &session_id,
                "user",
                &input,
            );
            let turn_res = {
                let mut guard = active_client.lock().await;
                guard.submit_turn(&session_id, &input).await
            };
            match turn_res {
                Ok(response) => {
                    if !response.is_empty() {
                        append_repl_transcript_entry_best_effort(
                            transcript_scope.as_ref(),
                            &session_id,
                            "assistant",
                            &response,
                        );
                    }
                    // Response already printed by streaming handler
                    // Update session metadata
                    if let Err(e) = self.update_session_metadata().await {
                        eprintln!("⚠️  Could not update session metadata: {}", e);
                    }
                }
                Err(e) => {
                    append_repl_transcript_entry_best_effort(
                        transcript_scope.as_ref(),
                        &session_id,
                        "error",
                        &e.to_string(),
                    );
                    eprintln!("\n❌ Error: {}", e);
                }
            }
        }

        // Save history on exit
        self.history.save_to_file()?;
        Ok(())
    }

    async fn handle_slash_command(&mut self, input: &str) -> Result<Option<String>> {
        if let Some(action) = legacy_session_action(input) {
            self.apply_legacy_session_action(action).await?;
            return Ok(Some(format!(
                "✅ Active backend session: {}\n",
                self.session.name
            )));
        }

        let preflight = command_session_preflight(input);
        let workspace_switch_target = workspace_switch_target(input);
        let workspace_before_dispatch =
            if preflight == CommandSessionPreflight::AfterWorkspaceSwitch {
                self.current_workspace_name().await.ok()
            } else {
                None
            };

        let command_session_id = if preflight == CommandSessionPreflight::BeforeDispatch {
            self.ensure_session_for_active_workspace().await?
        } else {
            self.session.id.clone()
        };

        let result = self
            .command_handler
            .handle(input, &command_session_id)
            .await?;

        if preflight == CommandSessionPreflight::AfterWorkspaceSwitch {
            let workspace_after_dispatch = self.current_workspace_name().await.ok();
            let successful_switch = matches!(
                (workspace_switch_target.as_deref(), workspace_after_dispatch.as_deref()),
                (Some(target), Some(active)) if target == active
            );
            if successful_switch || workspace_after_dispatch != workspace_before_dispatch {
                self.ensure_session_for_active_workspace()
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("workspace switched, but session setup failed: {e}")
                    })?;
            }
        }

        Ok(result)
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
        let scope = self.current_storage_scope().await?;
        let metadata = storage::load_scoped_session_metadata(&scope, &self.session.id)?;

        let updated = crate::cli::storage::SessionMetadata {
            last_active: Utc::now().to_rfc3339(),
            ..metadata
        };

        storage::save_scoped_session_metadata(&scope, &updated)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LegacySessionAction {
    Switch { target: String },
    Create { target: String },
}

impl LegacySessionAction {
    fn target(&self) -> &str {
        match self {
            LegacySessionAction::Switch { target } | LegacySessionAction::Create { target } => {
                target
            }
        }
    }
}

fn legacy_session_action(cmdline: &str) -> Option<LegacySessionAction> {
    let parts = tokenize_slash_command(cmdline).ok()?;
    if parts.first()?.as_str() != "/session" {
        return None;
    }

    match parts.get(1).map(String::as_str)? {
        "list" | "info" | "delete" => None,
        "switch" => Some(LegacySessionAction::Switch {
            target: single_remaining_session_target(&parts[2..])?,
        }),
        "create" => Some(LegacySessionAction::Create {
            target: single_remaining_session_target(&parts[2..])?,
        }),
        name if parts.len() == 2 => Some(LegacySessionAction::Switch {
            target: name.to_string(),
        }),
        _ => None,
    }
}

fn single_remaining_session_target(parts: &[String]) -> Option<String> {
    match parts {
        [target] if !target.is_empty() => Some(target.clone()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandSessionPreflight {
    None,
    BeforeDispatch,
    AfterWorkspaceSwitch,
}

fn command_session_preflight(cmdline: &str) -> CommandSessionPreflight {
    let Ok(parts) = tokenize_slash_command(cmdline) else {
        return CommandSessionPreflight::None;
    };
    let Some(command) = parts.first().map(String::as_str) else {
        return CommandSessionPreflight::None;
    };
    let subcommand = parts.get(1).map(String::as_str);

    match command {
        "/info" | "/status" | "/clear" | "/save" => CommandSessionPreflight::BeforeDispatch,
        "/session" if matches!(subcommand, Some("info") | Some("delete")) => {
            CommandSessionPreflight::BeforeDispatch
        }
        "/workspace" | "/workspaces"
            if matches!(subcommand, Some("switch")) && parts.get(2).is_some() =>
        {
            CommandSessionPreflight::AfterWorkspaceSwitch
        }
        _ => CommandSessionPreflight::None,
    }
}

fn workspace_switch_target(cmdline: &str) -> Option<String> {
    let parts = tokenize_slash_command(cmdline).ok()?;
    let command = parts.first()?.as_str();
    if !matches!(command, "/workspace" | "/workspaces") {
        return None;
    }
    if parts.get(1)?.as_str() != "switch" {
        return None;
    }
    let target = parts.get(2..)?.join(" ");
    if target.is_empty() {
        None
    } else {
        Some(target)
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

fn save_legacy_session_metadata(
    scope: &storage::LocalWorkspaceScope,
    session: &Session,
) -> Result<()> {
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
        storage::save_scoped_session_metadata(scope, &metadata)?;
    } else {
        warn!(
            "not saving local metadata for unsafe session id: {}",
            metadata.id
        );
    }
    Ok(())
}

fn append_repl_transcript_entry_best_effort(
    scope: Option<&storage::LocalWorkspaceScope>,
    session_id: &str,
    role: &str,
    content: &str,
) -> bool {
    let Some(scope) = scope else {
        return false;
    };
    if let Err(e) = storage::append_scoped_session_history(scope, session_id, role, content) {
        warn!("could not append {role} transcript entry for session {session_id}: {e}");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::agent::{AgentClient, StreamSink};
    use crate::cli::client::{Config, Model, Provider};
    use crate::cli::workspace::{App, Backend, Workspace, WorkspaceConfig};
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

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
            Some(LegacySessionAction::Switch {
                target: "research".to_string()
            })
        );
        assert_eq!(
            legacy_session_action("/session switch research"),
            Some(LegacySessionAction::Switch {
                target: "research".to_string()
            })
        );
        assert_eq!(
            legacy_session_action("/session create scratch"),
            Some(LegacySessionAction::Create {
                target: "scratch".to_string()
            })
        );
        assert_eq!(legacy_session_action("/session switch 'Research"), None);
        assert_eq!(
            legacy_session_action("/session switch 'Research Notes'"),
            Some(LegacySessionAction::Switch {
                target: "Research Notes".to_string()
            })
        );
        assert_eq!(legacy_session_action("/session research notes"), None);
        assert_eq!(
            legacy_session_action("/session switch research notes"),
            None
        );
        assert_eq!(legacy_session_action("/session create scratch copy"), None);
        assert_eq!(legacy_session_action("/session list"), None);
        assert_eq!(legacy_session_action("/session info"), None);
        assert_eq!(legacy_session_action("/session delete research"), None);
        assert_eq!(legacy_session_action("/session switch"), None);
        assert_eq!(
            command_session_preflight("/session delete 'Research"),
            CommandSessionPreflight::None
        );
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

    #[derive(Clone)]
    struct FakeWorkspaceClient {
        sessions: Vec<Session>,
        submitted: Arc<StdMutex<Vec<(String, String)>>>,
        deleted: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl AgentClient for FakeWorkspaceClient {
        async fn health(&self) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn get_config(&self) -> anyhow::Result<Config> {
            Ok(Config {
                agent: Default::default(),
            })
        }

        async fn put_config(&self, _config: &Config) -> anyhow::Result<()> {
            Ok(())
        }

        async fn list_providers(&self) -> anyhow::Result<Vec<Provider>> {
            Ok(Vec::new())
        }

        async fn get_models(&self, _provider: &str) -> anyhow::Result<Vec<Model>> {
            Ok(Vec::new())
        }

        async fn list_provider_models(&self, _provider: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
            Ok(self.sessions.clone())
        }

        async fn create_session(&self, name: &str) -> anyhow::Result<Session> {
            Ok(session(&format!("created-{name}"), name))
        }

        async fn load_session(&self, session_id: &str) -> anyhow::Result<Session> {
            self.sessions
                .iter()
                .find(|session| session.id == session_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("session not found"))
        }

        async fn delete_session(&self, session_id: &str) -> anyhow::Result<()> {
            self.deleted.lock().unwrap().push(session_id.to_string());
            Ok(())
        }

        async fn submit_turn(&mut self, session_id: &str, message: &str) -> anyhow::Result<String> {
            self.submitted
                .lock()
                .unwrap()
                .push((session_id.to_string(), message.to_string()));
            Ok(String::new())
        }

        fn set_stream_sink(&mut self, _sink: Option<StreamSink>) {}
    }

    fn workspace(
        id: usize,
        name: &str,
        sessions: Vec<Session>,
        submitted: Arc<StdMutex<Vec<(String, String)>>>,
        deleted: Arc<StdMutex<Vec<String>>>,
    ) -> Workspace {
        let fake = FakeWorkspaceClient {
            sessions,
            submitted,
            deleted,
        };
        let boxed: Box<dyn AgentClient + Send + Sync> = Box::new(fake);
        Workspace {
            id,
            config: WorkspaceConfig {
                id: None,
                name: name.to_string(),
                backend: Backend::Zeroclaw,
                url: format!("http://{name}.example"),
                token_env: None,
                token: None,
                label: None,
                namespace_aliases: Vec::new(),
            },
            client: Some(Arc::new(Mutex::new(boxed))),
            cron: None,
        }
    }

    #[tokio::test]
    async fn repl_workspace_switch_rebinds_session_before_next_turn() {
        let alpha_submitted = Arc::new(StdMutex::new(Vec::new()));
        let beta_submitted = Arc::new(StdMutex::new(Vec::new()));
        let alpha_deleted = Arc::new(StdMutex::new(Vec::new()));
        let beta_deleted = Arc::new(StdMutex::new(Vec::new()));
        let alpha = session("alpha-session", "chat");
        let beta = session("beta-session", "chat");
        let app = Arc::new(Mutex::new(App {
            workspaces: vec![
                workspace(
                    0,
                    "alpha",
                    vec![alpha.clone()],
                    Arc::clone(&alpha_submitted),
                    Arc::clone(&alpha_deleted),
                ),
                workspace(
                    1,
                    "beta",
                    vec![beta.clone()],
                    Arc::clone(&beta_submitted),
                    Arc::clone(&beta_deleted),
                ),
            ],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }));
        let mut repl = ReplLoop::new(
            Arc::clone(&app),
            alpha,
            "model".to_string(),
            "provider".to_string(),
        )
        .unwrap();

        repl.ensure_session_for_active_workspace().await.unwrap();
        app.lock().await.active = 1;
        let session_id = repl.ensure_session_for_active_workspace().await.unwrap();
        assert_eq!(session_id, "beta-session");

        let active_client = repl.resolve_active_client().await.unwrap();
        active_client
            .lock()
            .await
            .submit_turn(&repl.session.id, "hello beta")
            .await
            .unwrap();

        assert!(alpha_submitted.lock().unwrap().is_empty());
        assert_eq!(
            beta_submitted.lock().unwrap().as_slice(),
            &[("beta-session".to_string(), "hello beta".to_string())]
        );
    }

    #[tokio::test]
    async fn repl_workspace_switch_then_delete_active_new_workspace_session_is_blocked() {
        let alpha_submitted = Arc::new(StdMutex::new(Vec::new()));
        let beta_submitted = Arc::new(StdMutex::new(Vec::new()));
        let alpha_deleted = Arc::new(StdMutex::new(Vec::new()));
        let beta_deleted = Arc::new(StdMutex::new(Vec::new()));
        let alpha = session("alpha-session", "chat");
        let beta = session("beta-session", "chat");
        let app = Arc::new(Mutex::new(App {
            workspaces: vec![
                workspace(
                    0,
                    "alpha",
                    vec![alpha.clone()],
                    Arc::clone(&alpha_submitted),
                    Arc::clone(&alpha_deleted),
                ),
                workspace(
                    1,
                    "beta",
                    vec![beta.clone()],
                    Arc::clone(&beta_submitted),
                    Arc::clone(&beta_deleted),
                ),
            ],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }));
        let mut repl = ReplLoop::new(
            Arc::clone(&app),
            alpha,
            "model".to_string(),
            "provider".to_string(),
        )
        .unwrap();

        repl.ensure_session_for_active_workspace().await.unwrap();
        repl.handle_slash_command("/workspace switch beta")
            .await
            .unwrap();

        assert_eq!(repl.session.id, "beta-session");

        let out = repl
            .handle_slash_command("/session delete chat")
            .await
            .expect("delete command should complete")
            .expect("delete command should return output");

        assert!(out.contains("Cannot delete active session"));
        assert!(beta_deleted.lock().unwrap().is_empty());
        assert!(alpha_deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn legacy_repl_malformed_quoted_session_switch_does_not_rebind() {
        let submitted = Arc::new(StdMutex::new(Vec::new()));
        let deleted = Arc::new(StdMutex::new(Vec::new()));
        let chat = session("chat-session", "chat");
        let research = session("research-session", "Research");
        let app = Arc::new(Mutex::new(App {
            workspaces: vec![workspace(
                0,
                "alpha",
                vec![chat.clone(), research],
                Arc::clone(&submitted),
                Arc::clone(&deleted),
            )],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }));
        let mut repl = ReplLoop::new(
            Arc::clone(&app),
            chat,
            "model".to_string(),
            "provider".to_string(),
        )
        .unwrap();

        let out = repl
            .handle_slash_command("/session switch 'Research")
            .await
            .expect("malformed command should be handled by CommandHandler")
            .expect("parse error should be displayed");

        assert!(out.contains("Could not parse command"));
        assert!(out.contains("unterminated"));
        assert_eq!(repl.session.id, "chat-session");
        assert_eq!(repl.session.name, "chat");
    }

    #[tokio::test]
    async fn legacy_repl_quoted_session_switch_rebinds_to_single_target() {
        let submitted = Arc::new(StdMutex::new(Vec::new()));
        let deleted = Arc::new(StdMutex::new(Vec::new()));
        let chat = session("chat-session", "chat");
        let research_notes = session("research-notes-session", "Research Notes");
        let app = Arc::new(Mutex::new(App {
            workspaces: vec![workspace(
                0,
                "alpha",
                vec![chat, research_notes],
                Arc::clone(&submitted),
                Arc::clone(&deleted),
            )],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }));
        let mut repl = ReplLoop::new(
            Arc::clone(&app),
            session("chat-session", "chat"),
            "model".to_string(),
            "provider".to_string(),
        )
        .unwrap();

        let out = repl
            .handle_slash_command("/session switch 'Research Notes'")
            .await
            .expect("quoted command should switch")
            .expect("switch should report active session");

        assert_eq!(out, "✅ Active backend session: Research Notes\n");
        assert_eq!(repl.session.id, "research-notes-session");
        assert_eq!(repl.session.name, "Research Notes");
    }
}
