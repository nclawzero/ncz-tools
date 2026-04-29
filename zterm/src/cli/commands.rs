use anyhow::{anyhow, Result};
use chrono::Utc;
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::Mutex;
use tracing::warn;

use crate::cli::agent::AgentClient;
use crate::cli::client::ZeroclawClient;
use crate::cli::client::{Model, Provider, Session};
use crate::cli::input::InputHistory;
use crate::cli::storage;
use crate::cli::url_safety::{
    redact_url_secrets_lossy_for_display, redact_url_secrets_lossy_if_needed,
};
use crate::cli::workspace::{Backend, Workspace, WorkspaceConfig};

type AgentClientHandle = Arc<Mutex<Box<dyn AgentClient + Send + Sync>>>;
type WorkspaceActivationFuture = Pin<Box<dyn Future<Output = Result<AgentClientHandle>> + Send>>;
type WorkspaceActivator = Arc<dyn Fn(WorkspaceConfig) -> WorkspaceActivationFuture + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHandlerOutput {
    pub output: Option<String>,
    pub mutation_outcome_unknown: bool,
}

impl CommandHandlerOutput {
    fn known(output: Option<String>) -> Self {
        Self {
            output,
            mutation_outcome_unknown: false,
        }
    }

    fn new(output: Option<String>, mutation_outcome_unknown: bool) -> Self {
        Self {
            output,
            mutation_outcome_unknown,
        }
    }
}

/// Command handler.
///
/// Holds a shared Arc<Mutex<App>>. Every per-command helper
/// briefly locks it to resolve the active workspace client,
/// cron handle, or MNEMOS client. Chunk D-3b.
pub struct CommandHandler {
    app: std::sync::Arc<tokio::sync::Mutex<crate::cli::workspace::App>>,
    workspace_activator: WorkspaceActivator,
    workspace_switch_generation: AtomicU64,
}

impl CommandHandler {
    /// Create a new command handler.
    ///
    /// Takes the shared `Arc<Mutex<App>>` that ReplLoop also holds,
    /// so `/workspace switch` mutations are visible to both.
    pub fn new(app: std::sync::Arc<tokio::sync::Mutex<crate::cli::workspace::App>>) -> Self {
        Self {
            app,
            workspace_activator: Arc::new(|config| {
                Box::pin(async move { Workspace::activate_detached_client(&config).await })
            }),
            workspace_switch_generation: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    fn new_with_workspace_activator(
        app: std::sync::Arc<tokio::sync::Mutex<crate::cli::workspace::App>>,
        workspace_activator: WorkspaceActivator,
    ) -> Self {
        Self {
            app,
            workspace_activator,
            workspace_switch_generation: AtomicU64::new(0),
        }
    }

    async fn current_mnemos(&self) -> Option<crate::cli::mnemos::MnemosClient> {
        self.app.lock().await.shared_mnemos.clone()
    }

    async fn current_cron(&self) -> Option<ZeroclawClient> {
        self.app
            .lock()
            .await
            .active_workspace()
            .and_then(|w| w.cron.clone())
    }

    async fn current_inventory(&self) -> crate::cli::workspace::WorkspaceInventory {
        self.app.lock().await.inventory()
    }

    async fn current_agent_client(&self) -> Option<AgentClientHandle> {
        self.app
            .lock()
            .await
            .active_workspace()
            .and_then(|w| w.client.clone())
    }

    async fn current_storage_scope(&self) -> Option<storage::LocalWorkspaceScope> {
        let app = self.app.lock().await;
        let workspace = app.active_workspace()?;
        storage::workspace_scope(
            workspace.config.backend.as_str(),
            &workspace.config.name,
            workspace.config.id.as_deref(),
        )
        .ok()
    }

    fn workspace_switch_is_current(&self, switch_generation: u64) -> bool {
        self.workspace_switch_generation.load(Ordering::SeqCst) == switch_generation
    }

    /// Handle a slash command (maps to zeroclaw CLI)
    pub async fn handle(&self, input: &str, session_id: &str) -> Result<Option<String>> {
        Ok(self.handle_with_outcome(input, session_id).await?.output)
    }

    /// Handle a slash command and include whether a mutating backend action
    /// may already have been applied despite an error response.
    pub async fn handle_with_outcome(
        &self,
        input: &str,
        session_id: &str,
    ) -> Result<CommandHandlerOutput> {
        let parts_owned = match tokenize_slash_command(input) {
            Ok(parts) => parts,
            Err(e) => {
                return Ok(CommandHandlerOutput::known(Some(format!(
                    "❌ Could not parse command: {e}\n"
                ))));
            }
        };
        let parts: Vec<&str> = parts_owned.iter().map(String::as_str).collect();

        if parts.is_empty() {
            return Ok(CommandHandlerOutput::known(None));
        }

        let command = parts[0];
        let subcommand = parts.get(1).copied();
        let args = if parts.len() > 2 {
            &parts[2..]
        } else {
            &[] as &[&str]
        };

        let output = match command {
            // Core
            "/help" => self.handle_help().await,
            "/info" | "/status" => self.handle_info(session_id).await,
            "/exit" => Err(anyhow!("EXIT")),

            // Zeroclaw Agent & Daemon
            "/agent" => self.handle_agent(subcommand, args).await,
            "/daemon" | "/gateway" => self.handle_daemon(subcommand, args).await,
            "/service" => self.handle_service(subcommand).await,

            // Onboarding & Setup
            "/onboard" => self.handle_onboard(subcommand, args).await,

            // Diagnostics
            "/doctor" => self.handle_doctor(subcommand).await,

            // Memory, Cron, Skills
            "/memory" => return self.handle_memory(subcommand, args).await,
            "/workspace" | "/workspaces" => self.handle_workspace(subcommand, args).await,
            "/cron" => return self.handle_cron(subcommand, args).await,
            "/skill" | "/skills" => self.handle_skill(subcommand, args).await,

            // Provider & Model Management
            "/providers" => self.handle_providers().await,
            "/models" | "/model" => self.handle_models(subcommand, args).await,

            // Channels
            "/channels" | "/channel" => self.handle_channels(subcommand).await,

            // Hardware & Security
            "/hardware" => self.handle_hardware(subcommand).await,
            "/peripheral" => self.handle_peripheral(subcommand, args).await,
            "/estop" => self.handle_estop(subcommand, args).await,

            // Session Management (REPL-specific)
            "/clear" => match parse_clear_force(subcommand, args) {
                Ok(force) => self.handle_clear(session_id, force).await,
                Err(message) => Ok(Some(message)),
            },
            "/save" => match parse_save_filename(subcommand, args) {
                Ok(filename) => self.handle_save(session_id, filename).await,
                Err(message) => Ok(Some(message)),
            },
            "/history" => self.handle_history().await,
            "/config" => self.handle_config().await,
            "/session" => return self.handle_session(session_id, subcommand, args).await,
            "/mcp" => self.handle_mcp(subcommand).await,

            // Completion
            "/completions" => self.handle_completions(subcommand).await,

            _ => Ok(Some(format!(
                "❌ Unknown command: {command}\n   Type /help for available commands\n"
            ))),
        }?;
        Ok(CommandHandlerOutput::known(output))
    }

    /// Handle /help command.
    ///
    /// Returns the help text as a structured `String` so both the
    /// rustyline REPL (which `println!`s it) and the Turbo Vision
    /// UI (which appends it to the chat pane) can render it the
    /// same way. Output is carefully free of ANSI control codes so
    /// it renders correctly inside a `ChatPane`.
    async fn handle_help(&self) -> Result<Option<String>> {
        let body = "\n\
            📚 ZTerm — ZeroClaw Terminal Client\n\
            \n\
            Core:\n  \
              /help              This message\n  \
              /info              Session & model info\n  \
              /exit              Exit ZTerm\n\
            \n\
            Agent & Daemon:\n  \
              /agent             Interactive agent (current mode)\n  \
              /daemon            Daemon control (unsupported in v0.3.1)\n  \
              /service           Service control (unsupported in v0.3.1)\n\
            \n\
            Configuration:\n  \
              /onboard           Run setup wizard\n  \
              /providers         List 40+ AI providers\n  \
              /models list       Show available models\n  \
              /models set <m>    Set default model\n  \
              /channels list     List configured channels\n\
            \n\
            Data & Automation:\n  \
              /memory search <query> [limit]   Search MNEMOS\n  \
              /memory list [limit]              Recent memories\n  \
              /memory get <id>                  Retrieve one by id\n  \
              /memory post <content> [--category <cat>]   Save new\n  \
              /memory delete <id>               Remove one by id\n  \
              /memory stats                     MNEMOS stats\n  \
              /cron list         List scheduled tasks\n  \
              /cron add '...'    Create cron job\n  \
              /skill list        List installed skills\n\
            \n\
            System & Hardware:\n  \
              /doctor            Run diagnostics\n  \
              /hardware discover Find USB devices\n  \
              /peripheral list   List configured devices\n  \
              /estop status      Check emergency stop\n\
            \n\
            Local Session:\n  \
              /config            Show configuration\n  \
              /clear [--force]   Clear local transcript; backend context retained\n  \
              /save [file]       Export session\n  \
              /history           Show commands\n  \
              /session list      List sessions\n"
            .to_string();
        Ok(Some(body))
    }

    /// Handle /info command.
    ///
    /// Returns the active backend session block as a `String` so
    /// the TUI can render it in the chat pane. Local metadata is
    /// merged only when it exactly matches the backend session id.
    async fn handle_info(&self, session_id: &str) -> Result<Option<String>> {
        let Some(client) = self.current_agent_client().await else {
            return Ok(Some(
                "❌ Failed to load active backend session: no active workspace client\n"
                    .to_string(),
            ));
        };

        let load_result = {
            let locked = client.lock().await;
            locked.load_session(session_id).await
        };
        match load_result {
            Ok(session) => {
                let local_sessions = match self.current_storage_scope().await {
                    Some(scope) => storage::list_scoped_sessions(&scope).unwrap_or_default(),
                    None => Vec::new(),
                };
                let local_metadata = exact_local_metadata(&local_sessions, &session.id);
                Ok(Some(format_backend_session_info(&session, local_metadata)))
            }
            Err(e) => Ok(Some(format!(
                "❌ Failed to load active backend session '{session_id}': {e}\n"
            ))),
        }
    }

    /// Handle /session command with full CRUD
    async fn handle_session(
        &self,
        session_id: &str,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<CommandHandlerOutput> {
        let mut out = String::new();
        let mut mutation_outcome_unknown = false;
        match subcommand {
            Some("list") => {
                let Some(client) = self.current_agent_client().await else {
                    out.push_str("❌ Could not list sessions: no active workspace client\n");
                    out.push('\n');
                    return Ok(CommandHandlerOutput::known(Some(out)));
                };

                let list_result = {
                    let locked = client.lock().await;
                    locked.list_sessions().await
                };
                match list_result {
                    Ok(sessions) => {
                        let local_sessions = match self.current_storage_scope().await {
                            Some(scope) => {
                                storage::list_scoped_sessions(&scope).unwrap_or_default()
                            }
                            None => Vec::new(),
                        };
                        out.push_str(&format_backend_session_list(&sessions, &local_sessions));
                    }
                    Err(e) => {
                        out.push_str(&format!("❌ Could not list active backend sessions: {e}\n"));
                    }
                }
            }
            Some("delete") => match parse_single_session_target(args, "/session delete <name>") {
                Err(message) => out.push_str(&message),
                Ok(name) => {
                    let Some(client) = self.current_agent_client().await else {
                        out.push_str("❌ Failed to delete session: no active workspace client\n");
                        out.push('\n');
                        return Ok(CommandHandlerOutput::known(Some(out)));
                    };

                    let scope = self.current_storage_scope().await;
                    match resolve_delete_session_target(&client, scope.as_ref(), name).await {
                        Ok(target) if target.id == session_id => {
                            out.push_str(&format!(
                                "❌ Cannot delete active session '{}'; switch to another session before deleting it\n",
                                target.display_name()
                            ));
                        }
                        Ok(target) => match client.lock().await.delete_session(&target.id).await {
                            Ok(()) => {
                                let display = target.display_name().to_string();
                                if let Some(local_id) = target.local_id.as_deref() {
                                    if let Err(e) = scope
                                        .as_ref()
                                        .ok_or_else(|| anyhow!("no active workspace storage scope"))
                                        .and_then(|scope| {
                                            storage::delete_scoped_session(scope, local_id)
                                        })
                                    {
                                        out.push_str(&format!(
                                            "⚠️ Backend deleted session '{display}', but local metadata cleanup failed: {e}\n"
                                        ));
                                    } else {
                                        out.push_str(&format!(
                                            "✅ Deleted session: {display} ({})\n",
                                            target.id
                                        ));
                                    }
                                } else {
                                    out.push_str(&format!(
                                        "✅ Deleted session: {display} ({})\n",
                                        target.id
                                    ));
                                }
                            }
                            Err(e) => {
                                mutation_outcome_unknown = true;
                                out.push_str(&format!(
                                    "❌ Backend failed to delete session '{}': {e}\n",
                                    target.display_name()
                                ));
                            }
                        },
                        Err(e) => {
                            out.push_str(&format!("❌ Failed to resolve session '{name}': {e}\n"));
                        }
                    }
                }
            },
            Some("info") => {
                let Some(client) = self.current_agent_client().await else {
                    out.push_str(
                        "❌ Failed to load active backend session: no active workspace client\n",
                    );
                    out.push('\n');
                    return Ok(CommandHandlerOutput::known(Some(out)));
                };

                let load_result = {
                    let locked = client.lock().await;
                    locked.load_session(session_id).await
                };
                match load_result {
                    Ok(session) => {
                        let local_metadata = self
                            .current_storage_scope()
                            .await
                            .and_then(|scope| {
                                storage::load_scoped_session_metadata(&scope, &session.id).ok()
                            })
                            .filter(|metadata| metadata.id == session.id);
                        out.push_str(&format_backend_session_info(
                            &session,
                            local_metadata.as_ref(),
                        ));
                    }
                    Err(e) => {
                        out.push_str(&format!(
                            "❌ Failed to load active backend session '{session_id}': {e}\n"
                        ));
                    }
                }
            }
            Some("switch") | Some("create") => {
                let usage = format!("Usage: /session {} <name>", subcommand.unwrap_or("switch"));
                match parse_single_session_target(args, &usage) {
                    Ok(session_name) => {
                        out.push_str(&format!("🔄 Active backend session: '{session_name}'\n"));
                    }
                    Err(message) => out.push_str(&message),
                }
            }
            Some(session_name) => {
                if args.is_empty() {
                    out.push_str(&format!(
                        "🔄 Switching to backend session: '{session_name}'\n"
                    ));
                } else {
                    out.push_str(
                        "Usage: /session <name>\n❌ Session targets must be a single id/name token; extra tokens were not ignored.\n",
                    );
                }
            }
            None => {
                out.push_str("Usage: /session list         (show all sessions)\n");
                out.push_str("       /session <name>      (switch or create)\n");
                out.push_str("       /session switch <n>  (switch or create)\n");
                out.push_str("       /session create <n>  (create and switch)\n");
                out.push_str("       /session info        (current session details)\n");
                out.push_str("       /session delete <n>  (remove session)\n");
            }
        }
        out.push('\n');
        Ok(CommandHandlerOutput::new(
            Some(out),
            mutation_outcome_unknown,
        ))
    }

    /// Handle /history command
    async fn handle_history(&self) -> Result<Option<String>> {
        let out = match InputHistory::load_from_file() {
            Ok(history) => {
                if history.entries().is_empty() {
                    "No history available yet\n".to_string()
                } else {
                    let mut out = "\n📜 Command History:\n".to_string();
                    for (i, entry) in history.entries().iter().enumerate() {
                        out.push_str(&format!("  {}. {}\n", i + 1, entry));
                    }
                    out.push('\n');
                    out
                }
            }
            Err(_) => "No history available yet\n".to_string(),
        };
        Ok(Some(out))
    }

    // Zeroclaw Agent & Daemon
    async fn handle_agent(
        &self,
        subcommand: Option<&str>,
        _args: &[&str],
    ) -> Result<Option<String>> {
        let out = match subcommand {
            Some("-m") | Some("--message") => {
                anyhow::bail!(
                    "/agent -m is not supported in zterm; type the message directly at the prompt"
                );
            }
            Some("-p") | Some("--provider") => {
                anyhow::bail!(
                    "/agent -p/--provider is not supported in zterm; use /models set <provider>/<model> or /models list"
                );
            }
            _ => "✓ Agent mode active (already running)\n".to_string(),
        };
        Ok(Some(out))
    }

    async fn handle_daemon(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        let requested = match subcommand {
            Some("-p") | Some("--port") => {
                let port = args.first().copied().unwrap_or("42617");
                format!("daemon start on port {port}")
            }
            Some(other) => format!("daemon {other}"),
            None => "daemon status".to_string(),
        };
        let out = format!(
            "❌ /daemon is not wired to daemon control in zterm v0.3.1; no action taken for {requested}.\n"
        );
        Ok(Some(out))
    }

    async fn handle_service(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        let out = match subcommand {
            Some("install" | "status" | "start" | "stop" | "restart") => {
                "❌ /service is not wired to service control in zterm v0.3.1; no action taken.\n"
            }
            _ => "Usage: /service install|status|start|stop|restart\n",
        };
        Ok(Some(out.to_string()))
    }

    async fn handle_onboard(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        let mut out = "\n⚙️  Onboarding:\n".to_string();
        match subcommand {
            Some("--provider") => {
                let provider = args.first().copied().unwrap_or("openrouter");
                out.push_str(&format!("  Provider: {provider}\n"));
            }
            Some("--force") => out.push_str("  Config: Reset\n"),
            _ => out.push_str("  Config: ~/.zeroclaw/config.toml\n"),
        }
        out.push_str("  (Interactive setup in Phase 7+)\n\n");
        Ok(Some(out))
    }

    async fn handle_doctor(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        let mut out = "\n🏥 System Diagnostics:\n".to_string();
        match subcommand {
            Some("models") => {
                let providers = match self.current_agent_client().await {
                    Some(client) => client.lock().await.list_providers().await,
                    None => Err(anyhow::anyhow!("no active backend client")),
                };
                match providers {
                    Ok(providers) => {
                        let model_label = match self.current_agent_client().await {
                            Some(client) => client.lock().await.current_model_label(),
                            None => "(no active backend client)".to_string(),
                        };
                        out.push_str(&format!(
                            "  Provider catalog: [ok] {} provider(s) advertised\n",
                            providers.len()
                        ));
                        out.push_str(&format!("  Active model key: [info] {model_label}\n"));
                        out.push_str("  Provider connectivity: [unknown] not probed by v0.3.1\n");
                    }
                    Err(e) => out.push_str(&format!("  Provider catalog: [fail] {e}\n")),
                }
            }
            Some("traces") => out.push_str("  Execution traces: (Phase 7+)\n"),
            _ => {
                let health = match self.current_agent_client().await {
                    Some(client) => client.lock().await.health().await,
                    None => Err(anyhow::anyhow!("no active backend client")),
                };
                match health {
                    Ok(true) => out.push_str("  Gateway: [ok] reachable\n"),
                    Ok(false) => out.push_str("  Gateway: [fail] unhealthy\n"),
                    Err(e) => out.push_str(&format!("  Gateway: [fail] {e}\n")),
                }

                let config = match self.current_agent_client().await {
                    Some(client) => client.lock().await.get_config().await,
                    None => Err(anyhow::anyhow!("no active backend client")),
                };
                match config {
                    Ok(_) => out.push_str("  Config: [ok] backend returned config\n"),
                    Err(e) => out.push_str(&format!("  Config: [fail] {e}\n")),
                }

                match self.current_mnemos().await {
                    Some(mnemos) => match mnemos.stats().await {
                        Ok(_) => out.push_str("  Memory: [ok] MNEMOS stats reachable\n"),
                        Err(e) => out.push_str(&format!("  Memory: [fail] {e}\n")),
                    },
                    None => out.push_str("  Memory: [unknown] MNEMOS not configured\n"),
                }
                out.push_str("  Channels: [unknown] channel doctor not implemented in v0.3.1\n");
            }
        }
        out.push('\n');
        Ok(Some(out))
    }

    /// Handle /memory command with MNEMOS integration
    async fn handle_memory(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<CommandHandlerOutput> {
        // Backwards-compatible: "/memory <query>" (no subcommand) runs a search.
        let implicit: Vec<&str>;
        let (sub, rest): (&str, &[&str]) = match subcommand {
            Some(s)
                if matches!(
                    s,
                    "search"
                        | "list"
                        | "recent"
                        | "get"
                        | "post"
                        | "add"
                        | "delete"
                        | "rm"
                        | "stats"
                        | "help"
                ) =>
            {
                (s, args)
            }
            Some(s) => {
                implicit = vec![s];
                ("search", implicit.as_slice())
            }
            None => ("help", &[] as &[&str]),
        };

        let mut out = String::new();
        let mut mutation_outcome_unknown = false;
        match sub {
            "search" => {
                let (query, limit) = parse_search_args(rest);
                if query.is_empty() {
                    return Ok(CommandHandlerOutput::known(Some(
                        "Usage: /memory search <query> [limit]\n".to_string(),
                    )));
                }
                out.push_str(&format!("\n🔎 MNEMOS search: {query}\n"));
                let res = match self.current_mnemos().await {
                    Some(m) => m.search(&query, limit).await,
                    None => Ok(Vec::new()),
                };
                match res {
                    Ok(memories) if !memories.is_empty() => {
                        out.push_str(&format_memory_list(&memories));
                    }
                    Ok(_) => out.push_str("  (no matches)\n"),
                    Err(_) => {
                        out.push_str("❌ MNEMOS unavailable\n");
                        out.push_str("   check MNEMOS_URL / MNEMOS_TOKEN\n");
                    }
                }
            }
            "list" | "recent" => {
                let limit = rest
                    .first()
                    .and_then(|s| s.parse::<usize>().ok())
                    .map(cap_memory_limit)
                    .unwrap_or(10);
                out.push_str(&format!("\n📚 Recent memories (limit {limit}):\n"));
                let res = match self.current_mnemos().await {
                    Some(m) => m.list(limit).await,
                    None => Ok(Vec::new()),
                };
                match res {
                    Ok(memories) if !memories.is_empty() => {
                        out.push_str(&format_memory_list(&memories));
                    }
                    Ok(_) => out.push_str("  (MNEMOS empty or not configured)\n"),
                    Err(_) => out.push_str("❌ MNEMOS unavailable\n"),
                }
            }
            "get" => {
                let id = rest.join(" ");
                if id.is_empty() {
                    return Ok(CommandHandlerOutput::known(Some(
                        "Usage: /memory get <id>\n".to_string(),
                    )));
                }
                let res = match self.current_mnemos().await {
                    Some(m) => m.get(&id).await,
                    None => Ok(None),
                };
                match res {
                    Ok(Some(mem)) => {
                        out.push_str(&format!("\n📖 Memory [{id}]\n"));
                        if let Some(cat) = mem.get("category").and_then(|v| v.as_str()) {
                            out.push_str(&format!("  category: {cat}\n"));
                        }
                        if let Some(content) = mem.get("content").and_then(|v| v.as_str()) {
                            out.push('\n');
                            out.push_str(content);
                            out.push('\n');
                        }
                    }
                    Ok(None) => out.push_str(&format!("❌ Memory not found: {id}\n")),
                    Err(_) => {
                        out.push_str("❌ MNEMOS unavailable\n   check MNEMOS connection\n");
                    }
                }
            }
            "post" | "add" => {
                let (content, category) = parse_post_args(rest);
                if content.is_empty() {
                    return Ok(CommandHandlerOutput::known(Some(
                        "Usage: /memory post <content> [--category <cat>]\n\
                         Example: /memory post \"shipped zterm CI cleanup\" --category work\n"
                            .to_string(),
                    )));
                }
                let res = match self.current_mnemos().await {
                    Some(m) => {
                        let result = m.create(&content, category.as_deref()).await;
                        if result.is_err() {
                            mutation_outcome_unknown = true;
                        }
                        result
                    }
                    None => Err(anyhow::anyhow!(
                        "MNEMOS not configured (set MNEMOS_URL + MNEMOS_TOKEN)"
                    )),
                };
                match res {
                    Ok(result) => {
                        let id = result
                            .get("id")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| {
                                mutation_outcome_unknown = true;
                                "(unknown id)".to_string()
                            });
                        out.push_str(&format!("📝 Memory saved: {id}\n"));
                    }
                    Err(e) => out.push_str(&format!("❌ Failed to save memory: {e}\n")),
                }
            }
            "delete" | "rm" => {
                let id = rest.join(" ");
                if id.is_empty() {
                    return Ok(CommandHandlerOutput::known(Some(
                        "Usage: /memory delete <id>\n".to_string(),
                    )));
                }
                let res = match self.current_mnemos().await {
                    Some(m) => {
                        let result = m.delete(&id).await;
                        if result.is_err() {
                            mutation_outcome_unknown = true;
                        }
                        result
                    }
                    None => Err(anyhow::anyhow!(
                        "MNEMOS not configured (set MNEMOS_URL + MNEMOS_TOKEN)"
                    )),
                };
                match res {
                    Ok(()) => out.push_str(&format!("🗑️  Deleted memory {id}\n")),
                    Err(e) => out.push_str(&format!("❌ Delete failed: {e}\n")),
                }
            }
            "stats" => {
                out.push_str("\n📊 Memory Statistics:\n");
                let res = match self.current_mnemos().await {
                    Some(m) => m.stats().await,
                    None => Ok(serde_json::json!({ "status": "not_configured" })),
                };
                match res {
                    Ok(stats) => {
                        out.push_str(&serde_json::to_string_pretty(&stats).unwrap_or_default());
                        out.push('\n');
                    }
                    Err(_) => out.push_str("  (MNEMOS unavailable)\n"),
                }
            }
            _ => {
                out.push_str("Usage:\n");
                out.push_str("  /memory search <query> [limit]    — semantic/full-text search\n");
                out.push_str(
                    "  /memory list [limit]              — recent memories (alias: recent)\n",
                );
                out.push_str("  /memory get <id>                  — fetch one by id\n");
                out.push_str("  /memory post <content> [--category <cat>]\n");
                out.push_str(
                    "                                    — save a new memory (alias: add)\n",
                );
                out.push_str(
                    "  /memory delete <id>               — remove one by id (alias: rm)\n",
                );
                out.push_str(
                    "  /memory stats                     — MNEMOS storage / categories\n\n",
                );
                out.push_str("  Tip: '/memory <query>' (no subcommand) runs a search.\n");
                out.push_str("  Configure with MNEMOS_URL + MNEMOS_TOKEN in env / .env.\n");
            }
        }
        out.push('\n');
        Ok(CommandHandlerOutput::new(
            Some(out),
            mutation_outcome_unknown,
        ))
    }

    async fn handle_cron(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<CommandHandlerOutput> {
        let mut out = String::new();
        let mut mutation_outcome_unknown = false;
        match subcommand {
            Some("list") => {
                out.push_str("\n⏰ Scheduled Tasks:\n");
                let res = match self.current_cron().await {
                    Some(c) => c.list_cron_jobs().await,
                    None => Err(anyhow::anyhow!("cron not available on this backend")),
                };
                match res {
                    Ok(jobs) if !jobs.is_empty() => {
                        for (i, job) in jobs.iter().enumerate() {
                            let id = job.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                            let expr = job
                                .get("expression")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            let prompt = job.get("prompt").and_then(|v| v.as_str()).unwrap_or("?");
                            let short_id: String = id.chars().take(8).collect();
                            out.push_str(&format!(
                                "  {}. [{}] {} → {}\n",
                                i + 1,
                                short_id,
                                expr,
                                prompt
                            ));
                        }
                    }
                    Ok(_) => out.push_str("  (No scheduled tasks)\n"),
                    Err(e) => out.push_str(&format!("  (Gateway unavailable: {e})\n")),
                }
            }
            Some("add") => match parse_cron_add_args(args) {
                Err(message) => out.push_str(&message),
                Ok((expr, prompt)) => {
                    let res = match self.current_cron().await {
                        Some(c) => {
                            let result = c.create_cron_job(expr, &prompt).await;
                            if result.is_err() {
                                mutation_outcome_unknown = true;
                            }
                            result
                        }
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(id) => {
                            let short_id: String = id.chars().take(16).collect();
                            out.push_str(&format!("✅ Created cron job: {short_id}\n"));
                            out.push_str(&format!("   Expression: {expr} → {prompt}\n"));
                        }
                        Err(e) => {
                            out.push_str(&format!("❌ Failed to create cron job: {e}\n"));
                        }
                    }
                }
            },
            Some("add-at") => match parse_cron_add_at_args(args) {
                Err(message) => out.push_str(&message),
                Ok((datetime, prompt)) => {
                    let res = match self.current_cron().await {
                        Some(c) => {
                            let result = c.create_cron_at(datetime, &prompt).await;
                            if result.is_err() {
                                mutation_outcome_unknown = true;
                            }
                            result
                        }
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(id) => {
                            let short_id: String = id.chars().take(16).collect();
                            out.push_str(&format!("✅ Scheduled task: {short_id}\n"));
                            out.push_str(&format!("   Time: {datetime}\n"));
                            out.push_str(&format!("   Prompt: {prompt}\n"));
                        }
                        Err(e) => out.push_str(&format!("❌ Failed to schedule task: {e}\n")),
                    }
                }
            },
            Some("pause") => match parse_single_cron_target(args, "/cron pause <id>") {
                Err(message) => out.push_str(&message),
                Ok(id) => {
                    let res = match self.current_cron().await {
                        Some(c) => {
                            let result = c.pause_cron(id).await;
                            if result.is_err() {
                                mutation_outcome_unknown = true;
                            }
                            result
                        }
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(_) => out.push_str(&format!("⏸️  Paused job: {id}\n")),
                        Err(e) => out.push_str(&format!("❌ Failed to pause job: {e}\n")),
                    }
                }
            },
            Some("resume") => match parse_single_cron_target(args, "/cron resume <id>") {
                Err(message) => out.push_str(&message),
                Ok(id) => {
                    let res = match self.current_cron().await {
                        Some(c) => {
                            let result = c.resume_cron(id).await;
                            if result.is_err() {
                                mutation_outcome_unknown = true;
                            }
                            result
                        }
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(_) => out.push_str(&format!("▶️  Resumed job: {id}\n")),
                        Err(e) => out.push_str(&format!("❌ Failed to resume job: {e}\n")),
                    }
                }
            },
            Some("delete" | "remove") => match parse_single_cron_target(
                args,
                if subcommand == Some("delete") {
                    "/cron delete <id>"
                } else {
                    "/cron remove <id>"
                },
            ) {
                Err(message) => out.push_str(&message),
                Ok(id) => {
                    let res = match self.current_cron().await {
                        Some(c) => {
                            let result = c.delete_cron(id).await;
                            if result.is_err() {
                                mutation_outcome_unknown = true;
                            }
                            result
                        }
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(_) => out.push_str(&format!("🗑️  Deleted job: {id}\n")),
                        Err(e) => out.push_str(&format!("❌ Failed to delete job: {e}\n")),
                    }
                }
            },
            _ => {
                out.push_str("Usage: /cron list\n");
                out.push_str("       /cron add '<expr>' '<prompt>'\n");
                out.push_str("       /cron add-at '<datetime>' '<prompt>'\n");
                out.push_str("       /cron pause|resume|delete|remove <id>\n");
            }
        }
        out.push('\n');
        Ok(CommandHandlerOutput::new(
            Some(out),
            mutation_outcome_unknown,
        ))
    }

    /// Handle /skill command with zeroclaw integration
    async fn handle_skill(
        &self,
        subcommand: Option<&str>,
        _args: &[&str],
    ) -> Result<Option<String>> {
        let out = match subcommand {
            Some("list") => "⚡ Installed Skills: (none)\n\n",
            Some("install") => "  Installing: (Phase 7+)\n\n",
            Some("audit") => "  Auditing skills: (Phase 7+)\n\n",
            Some("remove") => "  Removing skill: (Phase 7+)\n\n",
            _ => "Usage: /skill list|install <path>|audit|remove\n\n",
        };
        Ok(Some(out.to_string()))
    }

    async fn handle_providers(&self) -> Result<Option<String>> {
        let Some(client) = self.current_agent_client().await else {
            return Ok(Some(
                "\n🤖 Configured Providers:\n  (no active workspace client)\n\n".to_string(),
            ));
        };

        let rows = {
            let locked = client.lock().await;
            let providers = match locked.list_providers().await {
                Ok(providers) => providers,
                Err(e) => {
                    return Ok(Some(format!(
                        "\n🤖 Configured Providers:\n  ❌ Failed to list providers: {e}\n\n"
                    )));
                }
            };

            let mut rows = Vec::new();
            for provider in providers {
                let models = locked
                    .list_provider_models(&provider.id)
                    .await
                    .map_err(|e| e.to_string());
                rows.push((provider, models));
            }
            rows
        };

        Ok(Some(format_provider_list(&rows)))
    }

    async fn handle_models(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        match subcommand {
            Some("list") | None => {
                let Some(client) = self.current_agent_client().await else {
                    return Ok(Some(
                        "\n📋 Available Models:\n\n  (no active workspace client)\n\n"
                            .to_string(),
                    ));
                };
                let (active, rows) = {
                    let locked = client.lock().await;
                    let active = locked.current_model_label();
                    let providers = match locked.list_providers().await {
                        Ok(providers) => providers,
                        Err(e) => {
                            return Ok(Some(format!(
                                "\n📋 Available Models:\n\n  ❌ Failed to list providers: {e}\n\n"
                            )));
                        }
                    };
                    let mut rows = Vec::new();
                    for provider in providers {
                        let models = locked
                            .get_models(&provider.id)
                            .await
                            .map_err(|e| e.to_string());
                        rows.push((provider, models));
                    }
                    (active, rows)
                };
                Ok(Some(format_model_list(&rows, &active)))
            }
            Some("set") => {
                if args.len() != 1 || args[0].trim().is_empty() {
                    return Ok(Some(
                        "Usage: /models set <key>\n   Run /models list to see available keys\n"
                            .to_string(),
                    ));
                }
                let key = args[0].trim().to_string();
                match self.current_cron().await {
                    Some(c) => {
                        if c.model_list().is_empty() {
                            match c.refresh_models().await {
                                Ok(list) if list.is_empty() => {
                                    return Ok(Some(
                                        "❌ Failed to set model key: /api/config advertised no model keys\n".to_string(),
                                    ));
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    return Ok(Some(format!(
                                        "❌ Failed to set model key: could not refresh /api/config: {e}\n"
                                    )));
                                }
                            }
                        }
                        match c.set_current_model(&key) {
                            Ok(()) => Ok(Some(format!(
                                "✅ Active model key: {key}\n   Future turns will send this key to the daemon.\n"
                            ))),
                            Err(e) => Ok(Some(format!("❌ Failed to set model key: {e}\n"))),
                        }
                    }
                    None => Ok(Some(
                        "/models set is only supported for zeroclaw workspaces; the active backend does not expose zterm-side model switching\n".to_string(),
                    )),
                }
            }
            Some("refresh") => match self.current_cron().await {
                Some(c) => match c.refresh_models().await {
                    Ok(list) => Ok(Some(format!(
                        "✅ Refreshed model list ({} entries)\n",
                        list.len()
                    ))),
                    Err(e) => Ok(Some(format!("❌ Failed to refresh /api/config: {e}\n"))),
                },
                None => Ok(Some(
                    "/models refresh is only supported for zeroclaw workspaces; this backend lists models live\n".to_string(),
                )),
            },
            Some("status") => {
                let Some(client) = self.current_agent_client().await else {
                    return Ok(Some(
                        "\n📊 Current Model:\n  (no active workspace client)\n\n".to_string(),
                    ));
                };
                let (active, rows) = {
                    let locked = client.lock().await;
                    let active = locked.current_model_label();
                    let providers = match locked.list_providers().await {
                        Ok(providers) => providers,
                        Err(e) => {
                            return Ok(Some(format!(
                                "\n📊 Current Model:\n  active: {active}\n  ❌ Failed to list providers: {e}\n\n"
                            )));
                        }
                    };
                    let mut rows = Vec::new();
                    for provider in providers {
                        let models = locked
                            .get_models(&provider.id)
                            .await
                            .map_err(|e| e.to_string());
                        rows.push((provider, models));
                    }
                    (active, rows)
                };
                Ok(Some(format_model_status(&rows, &active)))
            }
            _ => Ok(Some(
                "Usage: /models list|set <key>|refresh|status\n".to_string(),
            )),
        }
    }

    async fn handle_mcp(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        let action = subcommand.unwrap_or("status");
        let mut out = String::from("\n🔌 MCP\n");
        match action {
            "status" | "list" => {
                out.push_str(
                    "  MCP endpoints are not exposed by the active claw-family backend yet.\n",
                );
                out.push_str(
                    "  zterm will surface them here when the daemon advertises an MCP inventory.\n",
                );
            }
            _ => {
                out.push_str("Usage: /mcp status\n");
            }
        }
        out.push('\n');
        Ok(Some(out))
    }

    async fn handle_channels(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        let out = match subcommand {
            Some("list") => {
                "\n💬 Channels:\n  (None configured)\n  Available: Slack, Discord, Telegram, Matrix, Email, IRC\n\n"
            }
            Some("doctor") => "  Channel health: (Phase 7+)\n\n",
            _ => "Usage: /channel list|doctor\n\n",
        };
        Ok(Some(out.to_string()))
    }

    async fn handle_hardware(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        let out = match subcommand {
            Some("discover") => {
                "\n🔌 Hardware Discovery:\n  (No USB devices found)\n  Supports: STM32, Arduino, Raspberry Pi, ESP32\n\n"
            }
            Some("introspect") => "  Probing device... (Phase 7+)\n\n",
            _ => "Usage: /hardware discover|introspect <port>\n\n",
        };
        Ok(Some(out.to_string()))
    }

    async fn handle_peripheral(
        &self,
        subcommand: Option<&str>,
        _args: &[&str],
    ) -> Result<Option<String>> {
        let out = match subcommand {
            Some("list") => "📱 Peripherals: (none)\n\n",
            Some("add") => "  Adding peripheral... (Phase 7+)\n\n",
            Some("flash-nucleo") => "  Flashing STM32... (Phase 7+)\n\n",
            Some("flash") => "  Flashing Arduino... (Phase 7+)\n\n",
            _ => "Usage: /peripheral list|add|flash-nucleo|flash\n\n",
        };
        Ok(Some(out.to_string()))
    }

    async fn handle_estop(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        let out = match subcommand {
            Some("status") => {
                "\
🛑 Emergency Stop: Unknown
   Status backend is not implemented; treating E-stop state as unsupported until a real backend is wired.

"
                .to_string()
            }
            Some("--level") => {
                let level = args.first().copied().unwrap_or("<level>");
                format!(
                    "🛑 Emergency Stop: Unsupported\n   Requested level: {level}\n   No E-stop backend is implemented, so zterm did not change hardware or network state.\n\n"
                )
            }
            Some("resume") => {
                "\
🛑 Emergency Stop: Unsupported
   No E-stop backend is implemented, so zterm cannot verify or resume E-stop state.

"
                .to_string()
            }
            _ => "Usage: /estop status|--level <kill-all|network-kill|...>\n\n".to_string(),
        };
        Ok(Some(out))
    }

    async fn handle_completions(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        let out = match subcommand {
            Some("zsh") => "📝 Zsh completions: (Phase 7+)\n\n",
            Some("bash") => "📝 Bash completions: (Phase 7+)\n\n",
            Some("fish") => "📝 Fish completions: (Phase 7+)\n\n",
            _ => "Usage: /completions zsh|bash|fish\n\n",
        };
        Ok(Some(out.to_string()))
    }

    /// Handle /config command
    async fn handle_config(&self) -> Result<Option<String>> {
        let (path, source) = self.active_config_source().await?;
        Ok(Some(format_config_output_for_path(
            load_config_at(&path),
            &path,
            source,
        )))
    }

    async fn active_config_source(&self) -> Result<(PathBuf, &'static str)> {
        let app = self.app.lock().await;
        if app_uses_legacy_synthetic_config(&app) {
            return Ok(active_config_source_for_app(&app, storage::config_file()?));
        }
        Ok((app.config_path.clone(), "ZTerm workspace config"))
    }

    /// Handle /clear command
    async fn handle_clear(&self, session_id: &str, force: bool) -> Result<Option<String>> {
        let Some(scope) = self.current_storage_scope().await else {
            return Ok(Some(
                "No local session transcript found to clear; backend session context retained\n"
                    .to_string(),
            ));
        };

        let removed_history = if force {
            storage::force_clear_scoped_session_history(&scope, session_id)?
        } else {
            storage::clear_scoped_session_history(&scope, session_id)?
        };
        let mut touched_metadata = false;
        if let Ok(mut metadata) = storage::load_scoped_session_metadata(&scope, session_id) {
            touched_metadata = true;
            metadata.message_count = 0;
            metadata.last_active = Utc::now().to_rfc3339();
            if let Err(e) = storage::save_scoped_session_metadata(&scope, &metadata) {
                warn!("cleared session history, but metadata update failed for {session_id}: {e}");
            }
        }

        if removed_history || touched_metadata {
            return Ok(Some(
                if force {
                    "✓ Local session transcript force-cleared; backend session context retained\n"
                } else {
                    "✓ Local session transcript cleared; backend session context retained\n"
                }
                .to_string(),
            ));
        }
        Ok(Some(
            "No local session transcript found to clear; backend session context retained\n"
                .to_string(),
        ))
    }

    /// Handle /save command
    async fn handle_save(
        &self,
        session_id: &str,
        filename: Option<String>,
    ) -> Result<Option<String>> {
        let default_name = format!("session-{}.txt", Utc::now().format("%Y%m%d-%H%M%S"));
        let filename = filename.unwrap_or(default_name);

        if let Some(scope) = self.current_storage_scope().await {
            if let Ok(history_path) = storage::scoped_session_history_file(&scope, session_id) {
                if storage::scoped_session_history_is_incomplete(&scope, session_id)? {
                    return Ok(Some(
                        "❌ Session transcript is incomplete; refusing to save. Run /clear to discard the incomplete local history.\n"
                            .to_string(),
                    ));
                }
                if history_path.exists() {
                    let mut src = fs::File::open(&history_path)?;
                    match write_private_export_atomically(Path::new(&filename), |dst| {
                        io::copy(&mut src, dst)?;
                        Ok(())
                    }) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                            return Ok(Some(format!(
                                "❌ Refusing to overwrite existing file: {filename}\n"
                            )));
                        }
                        Err(e) => return Err(e.into()),
                    }
                    return Ok(Some(format!("✓ Session saved to {filename}\n")));
                } else {
                    return Ok(Some("No history to save\n".to_string()));
                }
            }
        }

        Ok(Some("No history to save\n".to_string()))
    }

    async fn handle_workspace(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        let inventory = self.current_inventory().await;
        match subcommand {
            Some("list") | None => {
                // Return the listing as a structured String so both
                // the rustyline REPL (which print!s it) and the
                // Turbo Vision UI (chat-pane append) render it the
                // same way.
                let mut out = String::new();
                if inventory.workspaces.is_empty() || inventory.is_synthetic_singleton() {
                    out.push('\n');
                    out.push_str("🗂  Workspaces (single-workspace mode)\n");
                    out.push_str(
                        "   Add [[workspaces]] entries to ~/.zterm/config.toml to enable multi-workspace.\n",
                    );
                    return Ok(Some(out));
                }
                out.push('\n');
                out.push_str(&format!(
                    "🗂  Workspaces ({} total):\n",
                    inventory.workspaces.len()
                ));
                for (i, w) in inventory.workspaces.iter().enumerate() {
                    let marker = if i == inventory.active_index {
                        "*"
                    } else {
                        " "
                    };
                    let label = w.label.clone().unwrap_or_else(|| w.name.clone());
                    let status = if w.activated {
                        "ok"
                    } else {
                        "not yet activated"
                    };
                    out.push_str(&format!(
                        "  {} {:>2}. {:<24} [{}] {} ({})\n",
                        marker,
                        i + 1,
                        label,
                        w.backend.as_str(),
                        display_workspace_url_for_output(&w.url),
                        status
                    ));
                }
                Ok(Some(out))
            }
            Some("info") => {
                if let Some(a) = inventory.active() {
                    let mut out = String::from("\n🗂  Active workspace\n");
                    out.push_str(&format!("   name:      {}\n", a.name));
                    if let Some(l) = &a.label {
                        out.push_str(&format!("   label:     {l}\n"));
                    }
                    out.push_str(&format!("   backend:   {}\n", a.backend.as_str()));
                    out.push_str(&format!(
                        "   url:       {}\n",
                        display_workspace_url_for_output(&a.url)
                    ));
                    out.push_str(&format!(
                        "   status:    {}\n",
                        if a.activated {
                            "activated"
                        } else {
                            "not yet activated"
                        }
                    ));
                    out.push('\n');
                    Ok(Some(out))
                } else {
                    Ok(Some("❌ no active workspace\n".to_string()))
                }
            }
            Some("switch") => {
                let name = args.join(" ");
                if name.is_empty() {
                    return Ok(Some(
                        "Usage: /workspace switch <name>\n   /workspace list to see names\n"
                            .to_string(),
                    ));
                }
                self.switch_workspace(&name).await
            }
            _ => {
                Ok(Some(
                    "Usage:\n  /workspace list         — enumerate configured workspaces\n  /workspace info         — details of the active workspace\n  /workspace switch <name>— change the active workspace\n"
                        .to_string(),
                ))
            }
        }
    }

    /// Execute a runtime workspace switch. Looks up the target by
    /// name, activates it if needed (may run a live openclaw
    /// handshake), and updates `App.active`. Subsequent commands
    /// pick up the new workspace's handles on their next lock.
    async fn switch_workspace(&self, name: &str) -> Result<Option<String>> {
        let target_idx = {
            let app = self.app.lock().await;
            app.workspaces.iter().position(|w| w.config.name == name)
        };
        let Some(target_idx) = target_idx else {
            return Ok(Some(format!(
                "❌ no workspace named \"{name}\"\n   /workspace list to see names\n"
            )));
        };
        let switch_generation = self
            .workspace_switch_generation
            .fetch_add(1, Ordering::SeqCst)
            + 1;

        let (activation, target_cron) = {
            let app = self.app.lock().await;
            if !app.workspaces[target_idx].is_activated() {
                (
                    Some(app.workspaces[target_idx].config.clone()),
                    app.workspaces[target_idx].cron.clone(),
                )
            } else {
                if !self.workspace_switch_is_current(switch_generation) {
                    return Ok(Some(workspace_switch_superseded_message(name)));
                }
                (None, app.workspaces[target_idx].cron.clone())
            }
        };

        if let Some(config) = activation {
            let activated_client = match (self.workspace_activator)(config).await {
                Ok(client) => client,
                Err(e) => {
                    if !self.workspace_switch_is_current(switch_generation) {
                        return Ok(Some(workspace_switch_superseded_message(name)));
                    }
                    return Ok(Some(format!("❌ failed to activate \"{name}\": {e}\n")));
                }
            };

            if let Some(cron) = &target_cron {
                if let Some(message) = refresh_workspace_models_for_switch(name, cron).await {
                    return Ok(Some(message));
                }
            }
            if !self.workspace_switch_is_current(switch_generation) {
                return Ok(Some(workspace_switch_superseded_message(name)));
            }

            let mut app = self.app.lock().await;
            if !self.workspace_switch_is_current(switch_generation) {
                return Ok(Some(workspace_switch_superseded_message(name)));
            }
            let Some(target_idx) = app.workspaces.iter().position(|w| w.config.name == name) else {
                return Ok(Some(format!(
                    "❌ workspace \"{name}\" disappeared during activation\n"
                )));
            };
            if !app.workspaces[target_idx].is_activated() {
                app.workspaces[target_idx].client = Some(activated_client);
            }
            app.active = target_idx;
        } else {
            if let Some(cron) = &target_cron {
                if let Some(message) = refresh_workspace_models_for_switch(name, cron).await {
                    return Ok(Some(message));
                }
            }
            if !self.workspace_switch_is_current(switch_generation) {
                return Ok(Some(workspace_switch_superseded_message(name)));
            }

            let mut app = self.app.lock().await;
            if !self.workspace_switch_is_current(switch_generation) {
                return Ok(Some(workspace_switch_superseded_message(name)));
            }
            let Some(target_idx) = app.workspaces.iter().position(|w| w.config.name == name) else {
                return Ok(Some(format!(
                    "❌ workspace \"{name}\" disappeared during activation\n"
                )));
            };
            app.active = target_idx;
        }

        Ok(Some(format!("✅ 🗂  switched to workspace: {name}\n")))
    }
}

async fn refresh_workspace_models_for_switch(
    workspace_name: &str,
    cron: &ZeroclawClient,
) -> Option<String> {
    match cron.refresh_models().await {
        Ok(list) if list.is_empty() => Some(format!(
            "❌ failed to refresh model state for \"{workspace_name}\": /api/config advertised no model keys\n"
        )),
        Ok(_) => None,
        Err(e) => Some(format!(
            "❌ failed to refresh model state for \"{workspace_name}\": {e}\n"
        )),
    }
}

fn workspace_switch_superseded_message(name: &str) -> String {
    format!("workspace switch to \"{name}\" was superseded by a newer request\n")
}

fn create_private_export_file(path: &Path) -> io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }

    let file = opts.open(path)?;
    harden_private_export_file(path)?;
    Ok(file)
}

fn write_private_export_atomically<F>(path: &Path, write_content: F) -> io::Result<()>
where
    F: FnOnce(&mut fs::File) -> io::Result<()>,
{
    write_private_export_atomically_with_sync(path, write_content, sync_parent_dir)
}

fn write_private_export_atomically_with_sync<F, S>(
    path: &Path,
    write_content: F,
    sync_parent: S,
) -> io::Result<()>
where
    F: FnOnce(&mut fs::File) -> io::Result<()>,
    S: Fn(&Path) -> io::Result<()>,
{
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "export path already exists",
        ));
    }

    let (temp_path, mut temp_file) = create_private_export_temp_file(path)?;
    let result = (|| {
        write_content(&mut temp_file)?;
        temp_file.flush()?;
        temp_file.sync_all()?;
        drop(temp_file);
        fs::hard_link(&temp_path, path)?;
        sync_parent(path)?;
        Ok(())
    })();

    let _ = fs::remove_file(&temp_path);
    let _ = sync_parent(&temp_path);
    result
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn create_private_export_temp_file(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let final_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "export path must name a file")
    })?;

    for _ in 0..16 {
        let mut temp_name = OsString::from(".");
        temp_name.push(final_name);
        temp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let temp_path = parent.join(temp_name);
        match create_private_export_file(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique export temp file",
    ))
}

#[cfg(unix)]
fn harden_private_export_file(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn harden_private_export_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

const CONFIG_SECRET_MASK: &str = "***REDACTED***";
const CONFIG_OUTPUT_MAX_BYTES: u64 = 512 * 1024;
const MEMORY_LIST_LIMIT_MAX: usize = 50;
const CONFIG_SECRET_KEY_FRAGMENTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "apikey",
    "authorization",
    "privatekey",
];

fn load_config_at(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path)
        .map_err(|e| anyhow!("Failed to read config {}: {}", path.display(), e))?;
    if metadata.len() > CONFIG_OUTPUT_MAX_BYTES {
        anyhow::bail!(
            "config {} is {} bytes, exceeding the {} byte display limit",
            path.display(),
            metadata.len(),
            CONFIG_OUTPUT_MAX_BYTES
        );
    }
    fs::read_to_string(path).map_err(|e| anyhow!("Failed to read config {}: {}", path.display(), e))
}

fn app_uses_legacy_synthetic_config(app: &crate::cli::workspace::App) -> bool {
    app.workspaces.len() == 1
        && app.workspaces[0].config.name == "default"
        && app.workspaces[0].config.backend == Backend::Zeroclaw
        && !config_file_declares_workspaces(&app.config_path)
}

fn active_config_source_for_app(
    app: &crate::cli::workspace::App,
    legacy_config_path: PathBuf,
) -> (PathBuf, &'static str) {
    if app_uses_legacy_synthetic_config(app) {
        return (legacy_config_path, "Legacy single-workspace config");
    }
    (app.config_path.clone(), "ZTerm workspace config")
}

fn config_file_declares_workspaces(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    toml::from_str::<toml::Value>(&content)
        .ok()
        .and_then(|value| {
            value
                .get("workspaces")
                .and_then(toml::Value::as_array)
                .map(|workspaces| !workspaces.is_empty())
        })
        .unwrap_or(false)
}

fn format_config_output(config: Result<String>) -> String {
    format_config_output_inner(config, None)
}

fn format_config_output_for_path(config: Result<String>, path: &Path, source: &str) -> String {
    format_config_output_inner(config, Some((source, path)))
}

fn format_config_output_inner(config: Result<String>, source: Option<(&str, &Path)>) -> String {
    let mut out = "\n⚙️  Configuration:\n".to_string();
    if let Some((source, path)) = source {
        out.push_str(&format!("Source: {source} ({})\n\n", path.display()));
    }
    match config {
        Ok(content) => {
            out.push_str(&redact_config_secrets(&content));
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        Err(e) => {
            out.push_str(&format!("❌ Could not load config: {e}\n"));
        }
    }
    out.push('\n');
    out
}

fn redact_config_secrets(content: &str) -> String {
    if let Ok(mut parsed) = toml::from_str::<toml::Value>(content) {
        redact_toml_value(&mut parsed);
        if let Ok(redacted) = toml::to_string_pretty(&parsed) {
            return redacted;
        }
    }

    let mut out = String::with_capacity(content.len());
    let mut skip_until_multiline_secret: Option<&'static str> = None;

    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = split_line_ending(raw_line);
        if let Some(delimiter) = skip_until_multiline_secret {
            if line.contains(delimiter) {
                skip_until_multiline_secret = None;
            }
            continue;
        }

        let (redacted, delimiter) = redact_config_line(line);
        out.push_str(&redacted);
        out.push_str(ending);
        skip_until_multiline_secret = delimiter;
    }

    out
}

fn redact_toml_value(value: &mut toml::Value) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                if is_sensitive_config_key(key) {
                    *child = toml::Value::String(CONFIG_SECRET_MASK.to_string());
                } else {
                    redact_toml_value(child);
                }
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                redact_toml_value(item);
            }
        }
        toml::Value::String(text) => {
            if let Some(redacted) = redact_url_string_value(text) {
                *text = redacted;
            }
        }
        _ => {}
    }
}

fn redact_url_string_value(value: &str) -> Option<String> {
    redact_url_secrets_lossy_if_needed(value)
}

fn display_workspace_url_for_output(url: &str) -> String {
    redact_url_secrets_lossy_for_display(url)
}

fn split_line_ending(raw_line: &str) -> (&str, &str) {
    if let Some(line) = raw_line.strip_suffix("\r\n") {
        (line, "\r\n")
    } else if let Some(line) = raw_line.strip_suffix('\n') {
        (line, "\n")
    } else {
        (raw_line, "")
    }
}

fn redact_config_line(line: &str) -> (String, Option<&'static str>) {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return (line.to_string(), None);
    }

    let Some(eq_idx) = find_unquoted_char(line, '=') else {
        return (line.to_string(), None);
    };

    let key = &line[..eq_idx];
    if !is_sensitive_config_key(key) {
        return redact_sensitive_config_pairs_in_line(line);
    }

    let rhs = &line[eq_idx + 1..];
    let value_indent_len = rhs.len() - rhs.trim_start().len();
    let value_indent = &rhs[..value_indent_len];
    let value = &rhs[value_indent_len..];
    let multiline = multiline_secret_delimiter(value);
    let comment = find_unquoted_char(value, '#')
        .map(|idx| {
            let whitespace_before_comment = value[..idx]
                .chars()
                .rev()
                .take_while(|ch| ch.is_whitespace())
                .map(char::len_utf8)
                .sum::<usize>();
            &value[idx - whitespace_before_comment..]
        })
        .unwrap_or("");
    (
        format!(
            "{}{}\"{}\"{}",
            &line[..eq_idx + 1],
            value_indent,
            CONFIG_SECRET_MASK,
            comment
        ),
        multiline,
    )
}

fn redact_sensitive_config_pairs_in_line(line: &str) -> (String, Option<&'static str>) {
    let mut redacted = String::with_capacity(line.len());
    let mut cursor = 0;
    let mut search_from = 0;
    let mut delimiter = None;

    while let Some(eq_idx) = find_unquoted_char_from(line, '=', search_from) {
        let Some((key_start, key_end)) = config_key_bounds_before_equals(line, eq_idx) else {
            search_from = eq_idx + 1;
            continue;
        };

        let value_start =
            eq_idx + 1 + line[eq_idx + 1..].len() - line[eq_idx + 1..].trim_start().len();
        let value_end = find_unquoted_value_end(line, value_start);
        let value_trimmed_end = line[..value_end].trim_end().len();
        let replacement = if is_sensitive_config_key(&line[key_start..key_end]) {
            if delimiter.is_none() {
                delimiter = multiline_secret_delimiter(&line[value_start..value_trimmed_end]);
            }
            Some(CONFIG_SECRET_MASK.to_string())
        } else {
            redact_url_value_literal(&line[value_start..value_trimmed_end])
        };
        let Some(replacement) = replacement else {
            search_from = eq_idx + 1;
            continue;
        };

        redacted.push_str(&line[cursor..value_start]);
        redacted.push_str(&toml_basic_string_literal(&replacement));
        redacted.push_str(&line[value_trimmed_end..value_end]);
        cursor = value_end;
        search_from = value_end.saturating_add(1);
    }

    if cursor == 0 {
        (line.to_string(), None)
    } else {
        redacted.push_str(&line[cursor..]);
        (redacted, delimiter)
    }
}

fn redact_url_value_literal(value: &str) -> Option<String> {
    let parsed = toml::from_str::<toml::Value>(&format!("value = {}", value.trim())).ok()?;
    let text = parsed.get("value")?.as_str()?;
    redact_url_string_value(text)
}

fn toml_basic_string_literal(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn config_key_bounds_before_equals(line: &str, eq_idx: usize) -> Option<(usize, usize)> {
    let key_end = line[..eq_idx].trim_end().len();
    if key_end == 0 {
        return None;
    }

    let mut key_start = key_end;
    for (idx, ch) in line[..key_end].char_indices().rev() {
        if is_config_key_char(ch) {
            key_start = idx;
        } else {
            break;
        }
    }

    (key_start < key_end).then_some((key_start, key_end))
}

fn is_config_key_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')
}

fn find_unquoted_value_end(line: &str, value_start: usize) -> usize {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0usize;

    for (offset, ch) in line[value_start..].char_indices() {
        let idx = value_start + offset;
        match quote {
            Some('"') => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    quote = None;
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some(_) => unreachable!(),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if matches!(ch, '{' | '[' | '(') => depth += 1,
            None if matches!(ch, '}' | ']' | ')') && depth > 0 => depth -= 1,
            None if depth == 0 && matches!(ch, ',' | '}' | ']' | '#') => return idx,
            None => {}
        }
    }

    line.len()
}

fn is_sensitive_config_key(key: &str) -> bool {
    let lower = normalize_config_key(key);
    CONFIG_SECRET_KEY_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

fn normalize_config_key(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn multiline_secret_delimiter(value: &str) -> Option<&'static str> {
    ["\"\"\"", "'''"].into_iter().find(|delimiter| {
        value.trim_start().starts_with(delimiter) && value.matches(delimiter).count() == 1
    })
}

fn find_unquoted_char(input: &str, target: char) -> Option<usize> {
    find_unquoted_char_from(input, target, 0)
}

fn find_unquoted_char_from(input: &str, target: char, start: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in input.char_indices() {
        if idx < start {
            continue;
        }
        match quote {
            Some('"') => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    quote = None;
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some(_) => unreachable!(),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == target => return Some(idx),
            None => {}
        }
    }

    None
}

fn parse_single_session_target<'a>(
    args: &'a [&str],
    usage: &str,
) -> std::result::Result<&'a str, String> {
    match args {
        [] => Err(format!("{usage}\n")),
        [target] => {
            if target.is_empty() {
                Err(format!("{usage}\n"))
            } else {
                Ok(*target)
            }
        }
        [_target, ..] => Err(format!(
            "{usage}\n❌ Session targets must be a single id/name token; extra tokens were not ignored.\n"
        )),
    }
}

fn parse_single_cron_target<'a>(
    args: &'a [&str],
    usage: &str,
) -> std::result::Result<&'a str, String> {
    match args {
        [] => Err(format!("{usage}\n")),
        [target] => {
            if target.is_empty() {
                Err(format!("{usage}\n"))
            } else {
                Ok(*target)
            }
        }
        [_target, ..] => Err(format!(
            "{usage}\n❌ Cron job targets must be a single id token; extra tokens were not ignored.\n"
        )),
    }
}

fn parse_save_filename(
    filename: Option<&str>,
    args: &[&str],
) -> std::result::Result<Option<String>, String> {
    match (filename, args) {
        (None, []) => Ok(None),
        (Some(name), []) if !name.is_empty() => Ok(Some(name.to_string())),
        (Some(_), [_extra, ..]) => Err(
            "Usage: /save [file]\n❌ Save path must be a single token; quote paths containing spaces.\n"
                .to_string(),
        ),
        _ => Err("Usage: /save [file]\n".to_string()),
    }
}

fn parse_clear_force(subcommand: Option<&str>, args: &[&str]) -> std::result::Result<bool, String> {
    match (subcommand, args) {
        (None, []) => Ok(false),
        (Some("--force" | "force"), []) => Ok(true),
        _ => Err(
            "Usage: /clear [--force]\n❌ Use --force only to remove a stale local transcript turn lock.\n"
                .to_string(),
        ),
    }
}

pub(crate) fn tokenize_slash_command(input: &str) -> std::result::Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None;
    let mut token_started = false;

    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => match chars.next() {
                    Some(next) => current.push(next),
                    None => current.push('\\'),
                },
                _ => current.push(ch),
            },
            Some(_) => unreachable!(),
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    token_started = true;
                }
                '\\' => {
                    token_started = true;
                    match chars.next() {
                        Some(next) => current.push(next),
                        None => current.push('\\'),
                    }
                }
                ch if ch.is_whitespace() => {
                    if token_started {
                        tokens.push(std::mem::take(&mut current));
                        token_started = false;
                    }
                }
                _ => {
                    token_started = true;
                    current.push(ch);
                }
            },
        }
    }

    if let Some(ch) = quote {
        return Err(format!("unterminated {ch} quote"));
    }
    if token_started {
        tokens.push(current);
    }
    Ok(tokens)
}

fn parse_cron_add_args<'a>(args: &'a [&'a str]) -> std::result::Result<(&'a str, String), String> {
    let usage =
        "Usage: /cron add '<expr>' '<prompt>'\nExample: /cron add '0 9 * * *' 'Daily standup'\n";
    if args.len() < 2 {
        return Err(usage.to_string());
    }
    let expr = args[0].trim();
    let prompt = args[1..].join(" ");
    if expr.split_whitespace().count() != 5 {
        return Err(format!(
            "{usage}❌ Cron expression must contain exactly 5 fields.\n"
        ));
    }
    if prompt.trim().is_empty() {
        return Err(usage.to_string());
    }
    Ok((expr, prompt))
}

fn parse_cron_add_at_args<'a>(
    args: &'a [&'a str],
) -> std::result::Result<(&'a str, String), String> {
    let usage = "Usage: /cron add-at '<datetime>' '<prompt>'\nExample: /cron add-at '2026-04-21T10:00:00Z' 'Meeting'\n";
    if args.len() < 2 {
        return Err(usage.to_string());
    }
    let datetime = args[0].trim();
    let prompt = args[1..].join(" ");
    if chrono::DateTime::parse_from_rfc3339(datetime).is_err() {
        return Err(format!(
            "{usage}❌ Datetime must be RFC3339, such as 2026-04-21T10:00:00Z.\n"
        ));
    }
    if prompt.trim().is_empty() {
        return Err(usage.to_string());
    }
    Ok((datetime, prompt))
}

fn format_backend_session_list(
    sessions: &[Session],
    local_sessions: &[storage::SessionMetadata],
) -> String {
    let mut out = String::new();
    if sessions.is_empty() {
        out.push_str("\n📋 Sessions: (none yet)\n");
        out.push_str("  Create one with: /session <name>\n");
        return out;
    }

    out.push_str(&format!("\n📋 Sessions ({}):\n", sessions.len()));
    for (i, session) in sessions.iter().enumerate() {
        out.push_str(&format!(
            "  {}. {} ({})\n",
            i + 1,
            session.name,
            short_session_id(&session.id)
        ));
        out.push_str(&format!("     ID:    {}\n", session.id));
        out.push_str(&format!(
            "     Model: {}/{}\n",
            session.provider, session.model
        ));
        if let Some(metadata) = exact_local_metadata(local_sessions, &session.id) {
            out.push_str(&format!(
                "     Cached metadata: {} messages, last active {}\n",
                metadata.message_count,
                short_date(&metadata.last_active)
            ));
        }
    }
    out
}

fn format_backend_session_info(
    session: &Session,
    local_metadata: Option<&storage::SessionMetadata>,
) -> String {
    let mut out = String::new();
    out.push_str("\n📊 Current Session:\n");
    out.push_str(&format!("  Name:      {}\n", session.name));
    out.push_str(&format!("  ID:        {}\n", session.id));
    out.push_str(&format!(
        "  Model:     {}/{}\n",
        session.provider, session.model
    ));

    if let Some(metadata) = local_metadata.filter(|metadata| metadata.id == session.id) {
        out.push_str(&format!(
            "  Cached metadata: {} messages\n",
            metadata.message_count
        ));
        out.push_str(&format!(
            "  Cached created:  {}\n",
            short_date(&metadata.created_at)
        ));
        out.push_str(&format!(
            "  Cached last use: {}\n",
            short_date(&metadata.last_active)
        ));
    }

    out
}

fn format_provider_list(rows: &[(Provider, std::result::Result<Vec<String>, String>)]) -> String {
    let mut out = String::from("\n🤖 Configured Providers:\n");
    if rows.is_empty() {
        out.push_str("  (none advertised by active backend)\n\n");
        return out;
    }

    for (provider, models) in rows {
        out.push_str(&format!("  • {}\n", provider_display(provider)));
        match models {
            Ok(models) if models.is_empty() => {
                out.push_str("    models: (none advertised)\n");
            }
            Ok(models) => {
                out.push_str(&format!("    models: {}\n", models.join(", ")));
            }
            Err(e) => {
                out.push_str(&format!("    models: ❌ failed to list: {e}\n"));
            }
        }
    }
    out.push('\n');
    out
}

fn format_model_list(
    rows: &[(Provider, std::result::Result<Vec<Model>, String>)],
    active: &str,
) -> String {
    let mut out = String::from("\n📋 Available Models:\n\n");
    if rows.is_empty() {
        out.push_str("  (none advertised by active backend)\n\n");
        return out;
    }

    for (provider, models) in rows {
        out.push_str(&format!("  {}\n", provider_display(provider)));
        match models {
            Ok(models) if models.is_empty() => {
                out.push_str("    (none advertised)\n");
            }
            Ok(models) => {
                for model in models {
                    let marker = if provider.id == active || model.id == active {
                        "*"
                    } else {
                        " "
                    };
                    out.push_str(&format!("    {marker} {}\n", model.id));
                }
            }
            Err(e) => {
                out.push_str(&format!("    ❌ failed to list models: {e}\n"));
            }
        }
    }
    out.push_str(&format!("\n  active: {active}\n"));
    out.push_str("  use: /models set <key> (zeroclaw workspaces only)\n\n");
    out
}

fn format_model_status(
    rows: &[(Provider, std::result::Result<Vec<Model>, String>)],
    active: &str,
) -> String {
    let mut out = String::from("\n📊 Current Model:\n");
    out.push_str(&format!("  active: {active}\n"));

    let mut matched = false;
    for (provider, models) in rows {
        if provider.id == active {
            matched = true;
            out.push_str(&format!("  provider: {}\n", provider_display(provider)));
            if let Ok(models) = models {
                if models.is_empty() {
                    out.push_str("  models:   (none advertised)\n");
                } else {
                    out.push_str(&format!(
                        "  models:   {}\n",
                        models
                            .iter()
                            .map(|m| m.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            break;
        }

        if let Ok(models) = models {
            if let Some(model) = models.iter().find(|model| model.id == active) {
                matched = true;
                out.push_str(&format!("  provider: {}\n", provider_display(provider)));
                out.push_str(&format!("  model:    {}\n", model.id));
                break;
            }
        }
    }

    if !matched {
        out.push_str("  selection: backend-managed or not present in advertised models\n");
    }
    out.push('\n');
    out
}

fn provider_display(provider: &Provider) -> String {
    if provider.name == provider.id || provider.name.is_empty() {
        provider.id.clone()
    } else {
        format!("{} ({})", provider.name, provider.id)
    }
}

fn exact_local_metadata<'a>(
    local_sessions: &'a [storage::SessionMetadata],
    id: &str,
) -> Option<&'a storage::SessionMetadata> {
    local_sessions.iter().find(|metadata| metadata.id == id)
}

fn short_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn short_date(value: &str) -> String {
    value.chars().take(10).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeleteSessionTarget {
    id: String,
    name: String,
    local_id: Option<String>,
}

impl DeleteSessionTarget {
    fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

async fn resolve_delete_session_target(
    client: &Arc<Mutex<Box<dyn AgentClient + Send + Sync>>>,
    scope: Option<&storage::LocalWorkspaceScope>,
    requested: &str,
) -> Result<DeleteSessionTarget> {
    let local_sessions = scope
        .map(|scope| storage::list_scoped_sessions(scope).unwrap_or_default())
        .unwrap_or_default();
    let backend_sessions = client.lock().await.list_sessions().await;

    match backend_sessions {
        Ok(sessions) => choose_delete_session_target(requested, Some(&sessions), &local_sessions),
        Err(_) => choose_delete_session_target(requested, None, &local_sessions),
    }
}

fn choose_delete_session_target(
    requested: &str,
    backend_sessions: Option<&[Session]>,
    local_sessions: &[storage::SessionMetadata],
) -> Result<DeleteSessionTarget> {
    let Some(sessions) = backend_sessions else {
        return choose_delete_session_target_without_backend(requested, local_sessions);
    };

    let exact_backend: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id == requested)
        .collect();
    match exact_backend.as_slice() {
        [session] => {
            return Ok(DeleteSessionTarget {
                id: session.id.clone(),
                name: session.name.clone(),
                local_id: unique_local_id_match(local_sessions, &session.id),
            });
        }
        [] => {}
        _ => {
            return Err(ambiguous_delete_target_error(
                requested,
                exact_backend,
                Vec::new(),
            ));
        }
    }

    let backend_name_matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.name == requested)
        .collect();
    if backend_name_matches.len() > 1 {
        return Err(ambiguous_delete_target_error(
            requested,
            backend_name_matches,
            Vec::new(),
        ));
    }

    if let Some(session) = backend_name_matches.first() {
        return Ok(DeleteSessionTarget {
            id: session.id.clone(),
            name: session.name.clone(),
            local_id: unique_local_id_match(local_sessions, &session.id),
        });
    }

    Err(anyhow!("not found in active backend sessions"))
}

fn choose_delete_session_target_without_backend(
    requested: &str,
    local_sessions: &[storage::SessionMetadata],
) -> Result<DeleteSessionTarget> {
    let exact_local_count = local_sessions
        .iter()
        .filter(|metadata| metadata.id == requested)
        .count();
    let hint = if exact_local_count == 0 {
        "no matching local metadata was trusted"
    } else {
        "local metadata is not trusted for backend deletes"
    };
    Err(anyhow!(
        "backend sessions unavailable; refusing backend delete while target cannot be verified against the active backend ({hint})"
    ))
}

fn unique_local_id_match(local_sessions: &[storage::SessionMetadata], id: &str) -> Option<String> {
    let mut matches = local_sessions
        .iter()
        .filter(|metadata| metadata.id == id)
        .filter(|metadata| storage::is_safe_session_id(&metadata.id))
        .map(|metadata| metadata.id.clone());
    let first = matches.next()?;
    if matches.next().is_none() {
        Some(first)
    } else {
        None
    }
}

fn ambiguous_delete_target_error(
    requested: &str,
    backend: Vec<&Session>,
    local: Vec<&storage::SessionMetadata>,
) -> anyhow::Error {
    let mut candidates = Vec::new();
    candidates.extend(
        backend
            .iter()
            .map(|session| format!("backend id={} name={}", session.id, session.name)),
    );
    candidates.extend(
        local
            .iter()
            .map(|metadata| format!("local id={} name={}", metadata.id, metadata.name)),
    );

    anyhow!(
        "ambiguous session name/label '{requested}'; use an explicit id. Candidates: {}",
        candidates.join("; ")
    )
}

fn parse_search_args(rest: &[&str]) -> (String, usize) {
    // Last arg is treated as limit if it parses as usize; otherwise
    // everything is treated as the query.
    if let Some(last) = rest.last() {
        if let Ok(n) = last.parse::<usize>() {
            let query = rest[..rest.len() - 1].join(" ");
            return (query, cap_memory_limit(n));
        }
    }
    (rest.join(" "), 10)
}

fn cap_memory_limit(limit: usize) -> usize {
    limit.min(MEMORY_LIST_LIMIT_MAX)
}

fn parse_post_args(rest: &[&str]) -> (String, Option<String>) {
    let mut category: Option<String> = None;
    let mut content_parts: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--category" | "-c" => {
                if i + 1 < rest.len() {
                    category = Some(rest[i + 1].to_string());
                    i += 2;
                    continue;
                }
            }
            _ => content_parts.push(rest[i]),
        }
        i += 1;
    }
    (content_parts.join(" "), category)
}

fn format_memory_list(memories: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for (i, mem) in memories.iter().enumerate() {
        let id = mem.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let content = mem.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let category = mem.get("category").and_then(|v| v.as_str()).unwrap_or("");
        let short_id: String = id.chars().take(12).collect();
        let preview: String = content.chars().take(80).collect();
        let suffix = if content.chars().count() > 80 {
            "…"
        } else {
            ""
        };
        if category.is_empty() {
            out.push_str(&format!(
                "  {}. [{}] {}{}\n",
                i + 1,
                short_id,
                preview,
                suffix
            ));
        } else {
            out.push_str(&format!(
                "  {}. [{}] ({}) {}{}\n",
                i + 1,
                short_id,
                category,
                preview,
                suffix
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::cli::agent::{AgentClient, StreamSink};
    use crate::cli::client::Config;
    use crate::cli::client::{Model, Provider, Session, ZeroclawClient};
    use crate::cli::storage::{self, SessionMetadata};
    use crate::cli::workspace::{App, Backend, Workspace, WorkspaceConfig};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::sync::Mutex;

    /// Mirror the parts→(command, subcommand, args) slicing used by the
    /// real dispatcher, so regression tests can hit every length class
    /// without standing up a live CommandHandler.
    fn split_parts<'a>(parts: &'a [&'a str]) -> (&'a str, Option<&'a str>, &'a [&'a str]) {
        let command = parts[0];
        let subcommand = parts.get(1).copied();
        let args = if parts.len() > 2 {
            &parts[2..]
        } else {
            &[] as &[&str]
        };
        (command, subcommand, args)
    }

    fn with_isolated_home<T>(f: impl FnOnce() -> T) -> T {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn run_async_with_isolated_home<F, T>(future_factory: impl FnOnce() -> F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        with_isolated_home(|| {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(future_factory())
        })
    }

    #[test]
    fn test_command_parsing() {
        let input = "/help";
        let parts: Vec<&str> = input.split_whitespace().collect();
        assert_eq!(parts[0], "/help");
    }

    #[test]
    fn test_single_token_command_does_not_panic() {
        // Regression: "/help" split_whitespace produces a 1-element Vec.
        // Earlier code did `&parts[2..]` unconditionally and panicked with
        // "range start index 2 out of range for slice of length 1" on the
        // first command entered in a fresh REPL session.
        let parts: Vec<&str> = "/help".split_whitespace().collect();
        let (command, subcommand, args) = split_parts(&parts);
        assert_eq!(command, "/help");
        assert_eq!(subcommand, None);
        assert!(args.is_empty());
    }

    #[test]
    fn test_two_token_command_has_subcommand_empty_args() {
        let parts: Vec<&str> = "/memory list".split_whitespace().collect();
        let (command, subcommand, args) = split_parts(&parts);
        assert_eq!(command, "/memory");
        assert_eq!(subcommand, Some("list"));
        assert!(args.is_empty());
    }

    #[test]
    fn test_three_plus_token_command_exposes_args_slice() {
        let parts: Vec<&str> = "/memory get mem_abc123".split_whitespace().collect();
        let (command, subcommand, args) = split_parts(&parts);
        assert_eq!(command, "/memory");
        assert_eq!(subcommand, Some("get"));
        assert_eq!(args, &["mem_abc123"]);
    }

    #[test]
    fn test_long_command_with_flags() {
        let parts: Vec<&str> = "/memory post hello world --category work"
            .split_whitespace()
            .collect();
        let (_, _, args) = split_parts(&parts);
        assert_eq!(args, &["hello", "world", "--category", "work"]);
    }

    #[test]
    fn slash_tokenizer_preserves_documented_cron_add_quotes() {
        let parts = super::tokenize_slash_command("/cron add '0 9 * * *' 'Daily standup'")
            .expect("quoted cron command should parse");
        assert_eq!(
            parts,
            vec![
                "/cron".to_string(),
                "add".to_string(),
                "0 9 * * *".to_string(),
                "Daily standup".to_string()
            ]
        );
        let args: Vec<&str> = parts[2..].iter().map(String::as_str).collect();
        let (expr, prompt) = super::parse_cron_add_args(&args).unwrap();
        assert_eq!(expr, "0 9 * * *");
        assert_eq!(prompt, "Daily standup");
    }

    #[test]
    fn slash_tokenizer_preserves_documented_cron_add_at_quotes() {
        let parts = super::tokenize_slash_command("/cron add-at '2026-04-21T10:00:00Z' 'Meeting'")
            .expect("quoted add-at command should parse");
        assert_eq!(
            parts,
            vec![
                "/cron".to_string(),
                "add-at".to_string(),
                "2026-04-21T10:00:00Z".to_string(),
                "Meeting".to_string()
            ]
        );
        let args: Vec<&str> = parts[2..].iter().map(String::as_str).collect();
        let (datetime, prompt) = super::parse_cron_add_at_args(&args).unwrap();
        assert_eq!(datetime, "2026-04-21T10:00:00Z");
        assert_eq!(prompt, "Meeting");
    }

    #[test]
    fn cron_add_rejects_malformed_quoted_expression_locally() {
        let parts = super::tokenize_slash_command("/cron add '0 9 * *' 'Daily standup'")
            .expect("quoted command should tokenize before validation");
        let args: Vec<&str> = parts[2..].iter().map(String::as_str).collect();
        let err = super::parse_cron_add_args(&args).unwrap_err();

        assert!(err.contains("exactly 5 fields"));
    }

    #[tokio::test]
    async fn cron_add_success_status_with_invalid_body_marks_unknown_outcome() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/api/cron/add")
            .with_status(201)
            .with_body("{not-json")
            .create_async()
            .await;
        let cron = ZeroclawClient::new(server.url(), "test_token".to_string());
        let handler = super::CommandHandler::new(app_with_fake_client_and_cron(cron).await);

        let result = handler
            .handle_with_outcome("/cron add '0 9 * * *' 'standup'", "main")
            .await
            .unwrap();

        assert!(result.mutation_outcome_unknown);
        assert!(result
            .output
            .unwrap()
            .contains("Failed to create cron job: Failed to parse response"));
    }

    #[tokio::test]
    async fn cron_add_success_status_without_id_marks_unknown_outcome() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/api/cron/add")
            .with_status(201)
            .with_body(r#"{"ok":true}"#)
            .create_async()
            .await;
        let cron = ZeroclawClient::new(server.url(), "test_token".to_string());
        let handler = super::CommandHandler::new(app_with_fake_client_and_cron(cron).await);

        let result = handler
            .handle_with_outcome("/cron add '0 9 * * *' 'standup'", "main")
            .await
            .unwrap();

        assert!(result.mutation_outcome_unknown);
        assert!(result
            .output
            .unwrap()
            .contains("missing a non-empty job id"));
    }

    #[tokio::test]
    async fn cron_add_at_success_status_without_id_marks_unknown_outcome() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/api/cron/add-at")
            .with_status(201)
            .with_body(r#"{"task":{"id":""}}"#)
            .create_async()
            .await;
        let cron = ZeroclawClient::new(server.url(), "test_token".to_string());
        let handler = super::CommandHandler::new(app_with_fake_client_and_cron(cron).await);

        let result = handler
            .handle_with_outcome("/cron add-at '2026-04-29T12:00:00Z' 'check'", "main")
            .await
            .unwrap();

        assert!(result.mutation_outcome_unknown);
        assert!(result
            .output
            .unwrap()
            .contains("missing a non-empty job id"));
    }

    #[tokio::test]
    async fn memory_post_success_status_without_id_marks_unknown_outcome() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/memories")
            .with_status(201)
            .with_body("{not-json")
            .create_async()
            .await;
        let mnemos = crate::cli::mnemos::MnemosClient::new(server.url(), "test_token");
        let handler = super::CommandHandler::new(app_with_fake_client_and_mnemos(mnemos).await);

        let result = handler
            .handle_with_outcome("/memory post 'remember this' --category work", "main")
            .await
            .unwrap();

        assert!(result.mutation_outcome_unknown);
        assert!(result
            .output
            .unwrap()
            .contains("Memory saved: (unknown id)"));
    }

    #[tokio::test]
    async fn memory_get_oversized_body_renders_bounded_not_found() {
        let mut server = mockito::Server::new_async().await;
        let body = format!(
            "{{\"id\":\"mem-big\",\"content\":\"{}\"}}",
            "x".repeat(600 * 1024)
        );
        let _mock = server
            .mock("GET", "/memories/mem-big")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;
        let mnemos = crate::cli::mnemos::MnemosClient::new(server.url(), "test_token");
        let handler = super::CommandHandler::new(app_with_fake_client_and_mnemos(mnemos).await);

        let output = handler
            .handle_with_outcome("/memory get mem-big", "main")
            .await
            .unwrap()
            .output
            .unwrap();

        assert!(output.contains("Memory not found: mem-big"));
        assert!(output.len() < 4096);
    }

    #[tokio::test]
    async fn memory_stats_oversized_body_renders_bounded_unavailable() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/stats")
            .with_status(200)
            .with_body("x".repeat(600 * 1024))
            .create_async()
            .await;
        let mnemos = crate::cli::mnemos::MnemosClient::new(server.url(), "test_token");
        let handler = super::CommandHandler::new(app_with_fake_client_and_mnemos(mnemos).await);

        let output = handler
            .handle_with_outcome("/memory stats", "main")
            .await
            .unwrap()
            .output
            .unwrap();

        assert!(output.contains("unavailable"));
        assert!(output.len() < 4096);
    }

    #[test]
    fn memory_search_limit_is_capped() {
        let (query, limit) = super::parse_search_args(&["semantic", "999999"]);

        assert_eq!(query, "semantic");
        assert_eq!(limit, super::MEMORY_LIST_LIMIT_MAX);
    }

    #[test]
    fn memory_list_truncates_unicode_ids_on_char_boundaries() {
        let memories = vec![serde_json::json!({
            "id": "memory-😀😀",
            "content": "hello",
            "category": "work"
        })];

        let out = super::format_memory_list(&memories);

        assert!(out.contains("[memory-😀😀]"));
    }

    #[test]
    fn slash_tokenizer_rejects_unterminated_quotes() {
        let err = super::tokenize_slash_command("/cron add '0 9 * * *").unwrap_err();
        assert!(err.contains("unterminated"));
    }

    #[test]
    fn session_delete_parser_rejects_multiword_or_extra_tokens() {
        assert_eq!(
            super::parse_single_session_target(&["Research"], "/session delete <name>").unwrap(),
            "Research"
        );

        let err =
            super::parse_single_session_target(&["Research", "Notes"], "/session delete <name>")
                .unwrap_err();

        assert!(err.contains("extra tokens were not ignored"));
        assert!(err.contains("/session delete <name>"));
    }

    #[test]
    fn session_switch_create_parser_rejects_extra_tokens() {
        assert_eq!(
            super::parse_single_session_target(&["scratch"], "/session create <name>").unwrap(),
            "scratch"
        );

        let err =
            super::parse_single_session_target(&["scratch", "copy"], "/session create <name>")
                .unwrap_err();

        assert!(err.contains("extra tokens were not ignored"));
    }

    #[test]
    fn cron_destructive_parser_rejects_empty_and_extra_targets() {
        assert_eq!(
            super::parse_single_cron_target(&["job-a"], "/cron remove <id>").unwrap(),
            "job-a"
        );

        let pause_err =
            super::parse_single_cron_target(&["job-a", "job-b"], "/cron pause <id>").unwrap_err();
        let resume_err =
            super::parse_single_cron_target(&["job-a", "job-b"], "/cron resume <id>").unwrap_err();
        let remove_err =
            super::parse_single_cron_target(&["job-a", "job-b"], "/cron remove <id>").unwrap_err();
        let empty_err = super::parse_single_cron_target(&[], "/cron remove <id>").unwrap_err();

        assert!(pause_err.contains("extra tokens were not ignored"));
        assert!(resume_err.contains("extra tokens were not ignored"));
        assert!(remove_err.contains("extra tokens were not ignored"));
        assert!(empty_err.contains("/cron remove <id>"));
    }

    #[tokio::test]
    async fn cron_delete_alias_dispatches_to_delete_cron() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("DELETE", "/api/cron/remove/job-a")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        let cron = ZeroclawClient::new(server.url(), "test_token".to_string());
        let handler = super::CommandHandler::new(app_with_fake_client_and_cron(cron).await);

        let result = handler
            .handle_with_outcome("/cron delete job-a", "main")
            .await
            .unwrap();

        mock.assert_async().await;
        assert!(!result.mutation_outcome_unknown);
        assert!(result.output.unwrap().contains("Deleted job: job-a"));
    }

    #[test]
    fn save_parser_rejects_extra_path_tokens() {
        assert_eq!(super::parse_save_filename(None, &[]).unwrap(), None);
        assert_eq!(
            super::parse_save_filename(Some("backup.txt"), &[]).unwrap(),
            Some("backup.txt".to_string())
        );

        let err = super::parse_save_filename(Some("session"), &["backup.txt"]).unwrap_err();
        assert!(err.contains("single token"));
        assert!(err.contains("quote paths containing spaces"));
    }

    #[test]
    fn clear_parser_accepts_only_force_flag() {
        assert!(!super::parse_clear_force(None, &[]).unwrap());
        assert!(super::parse_clear_force(Some("--force"), &[]).unwrap());
        assert!(super::parse_clear_force(Some("force"), &[]).unwrap());

        let err = super::parse_clear_force(Some("--force"), &["extra"]).unwrap_err();
        assert!(err.contains("Usage: /clear [--force]"));
        assert!(err.contains("stale local transcript turn lock"));

        let err = super::parse_clear_force(Some("now"), &[]).unwrap_err();
        assert!(err.contains("Usage: /clear [--force]"));
    }

    #[test]
    fn memory_list_formatter_is_structured_for_tui() {
        let memories = vec![serde_json::json!({
            "id": "memory-abcdef123456",
            "category": "work",
            "content": "short note"
        })];

        let out = super::format_memory_list(&memories);

        assert!(out.contains("[memory-abcde]"));
        assert!(out.contains("(work) short note"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn backend_session_list_formats_backend_rows_with_matching_cached_metadata() {
        let backend = vec![session("sess-123456789", "Research")];
        let mut matching = metadata("sess-123456789", "Stale Local Name");
        matching.message_count = 7;
        matching.last_active = "2026-04-27T12:34:56Z".to_string();
        let local_sessions = vec![matching];

        let out = super::format_backend_session_list(&backend, &local_sessions);

        assert!(out.contains("📋 Sessions (1):"));
        assert!(out.contains("Research (sess-123)"));
        assert!(out.contains("ID:    sess-123456789"));
        assert!(out.contains("Model: p/m"));
        assert!(out.contains("Cached metadata: 7 messages, last active 2026-04-27"));
        assert!(!out.contains("Stale Local Name"));
    }

    #[test]
    fn backend_session_list_truncates_multibyte_ids_on_char_boundaries() {
        let backend = vec![session("ééééééééé", "Unicode")];

        let out = super::format_backend_session_list(&backend, &[]);

        assert!(out.contains("Unicode (éééééééé)"));
        assert!(out.contains("ID:    ééééééééé"));
    }

    #[test]
    fn backend_session_list_suppresses_local_only_metadata() {
        let backend = vec![session("sess-backend", "Backend Only")];
        let local_sessions = vec![metadata("local-only", "Local Only")];

        let out = super::format_backend_session_list(&backend, &local_sessions);

        assert!(out.contains("Backend Only"));
        assert!(!out.contains("Local Only"));
        assert!(!out.contains("local-only"));
        assert!(!out.contains("Cached metadata"));
    }

    #[test]
    fn backend_session_info_merges_only_exact_cached_metadata() {
        let backend = session("sess-info", "Current");
        let mut matching = metadata("sess-info", "Cached Name");
        matching.message_count = 3;
        matching.created_at = "2026-01-02T00:00:00Z".to_string();
        matching.last_active = "2026-04-28T00:00:00Z".to_string();

        let out = super::format_backend_session_info(&backend, Some(&matching));

        assert!(out.contains("Name:      Current"));
        assert!(out.contains("ID:        sess-info"));
        assert!(out.contains("Model:     p/m"));
        assert!(out.contains("Cached metadata: 3 messages"));
        assert!(out.contains("Cached created:  2026-01-02"));
        assert!(out.contains("Cached last use: 2026-04-28"));
        assert!(!out.contains("Cached Name"));
    }

    #[test]
    fn backend_session_info_ignores_mismatched_cached_metadata() {
        let backend = session("sess-info", "Current");
        let local_only = metadata("local-only", "Local Only");

        let out = super::format_backend_session_info(&backend, Some(&local_only));

        assert!(out.contains("Name:      Current"));
        assert!(!out.contains("Cached metadata"));
        assert!(!out.contains("Local Only"));
    }

    #[test]
    fn provider_list_formats_model_ids_and_model_failures() {
        let rows = vec![
            (
                provider("openclaw"),
                Ok(vec!["fast".to_string(), "deep".to_string()]),
            ),
            (provider("broken"), Err("backend unavailable".to_string())),
        ];

        let out = super::format_provider_list(&rows);

        assert!(out.contains("🤖 Configured Providers:"));
        assert!(out.contains("• openclaw"));
        assert!(out.contains("models: fast, deep"));
        assert!(out.contains("models: ❌ failed to list: backend unavailable"));
    }

    #[test]
    fn model_list_marks_active_provider_key() {
        let rows = vec![(
            provider("primary"),
            Ok(vec![model("gemini-flash-latest", "primary")]),
        )];

        let out = super::format_model_list(&rows, "primary");

        assert!(out.contains("📋 Available Models:"));
        assert!(out.contains("primary"));
        assert!(out.contains("* gemini-flash-latest"));
        assert!(out.contains("active: primary"));
    }

    #[test]
    fn model_status_reports_backend_managed_selection_when_not_advertised() {
        let rows = vec![(provider("openclaw"), Ok(vec![model("fast", "openclaw")]))];

        let out = super::format_model_status(&rows, "openclaw default");

        assert!(out.contains("active: openclaw default"));
        assert!(out.contains("backend-managed"));
    }

    #[tokio::test]
    async fn model_set_fails_closed_when_catalog_refresh_fails() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/api/config")
            .with_status(503)
            .with_body("unavailable")
            .create_async()
            .await;
        let cron = ZeroclawClient::new(server.url(), "test_token".to_string());
        let handler = super::CommandHandler::new(app_with_fake_client_and_cron(cron.clone()).await);

        let out_result = handler
            .handle("/models set typo-model", "main")
            .await
            .map(|output| output.unwrap());

        let out = out_result.unwrap();

        assert!(out.contains("❌ Failed to set model key"));
        assert!(out.contains("could not refresh /api/config"));
        assert!(cron.cached_model_key_for_tests().is_none());
    }

    #[tokio::test]
    async fn model_set_fails_closed_when_catalog_is_empty() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/api/config")
            .with_status(200)
            .with_body(r#"{"content":"[providers]\nfallback = \"missing\"\n"}"#)
            .create_async()
            .await;
        let cron = ZeroclawClient::new(server.url(), "test_token".to_string());
        let handler = super::CommandHandler::new(app_with_fake_client_and_cron(cron.clone()).await);

        let out_result = handler
            .handle("/models set typo-model", "main")
            .await
            .map(|output| output.unwrap());

        let out = out_result.unwrap();

        assert!(out.contains("❌ Failed to set model key"));
        assert!(out.contains("advertised no model keys"));
        assert!(cron.cached_model_key_for_tests().is_none());
    }

    #[tokio::test]
    async fn model_set_rejects_extra_tokens_without_switching() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/api/config")
            .with_status(200)
            .with_body(
                r#"{"content":"[providers.models.primary]\nname = \"gemini\"\nmodel = \"gemini-flash-latest\"\n"}"#,
            )
            .create_async()
            .await;
        let cron = ZeroclawClient::new(server.url(), "test_token".to_string());
        let handler = super::CommandHandler::new(app_with_fake_client_and_cron(cron.clone()).await);

        let out = handler
            .handle("/models set primary extra", "main")
            .await
            .unwrap()
            .unwrap();

        assert!(out.contains("Usage: /models set <key>"));
        assert!(cron.cached_model_key_for_tests().is_none());
    }

    #[tokio::test]
    async fn model_set_accepts_quoted_single_key() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/api/config")
            .with_status(200)
            .with_body(
                r#"{"content":"[providers.models.\"primary key\"]\nname = \"gemini\"\nmodel = \"gemini-flash-latest\"\n"}"#,
            )
            .create_async()
            .await;
        let cron = ZeroclawClient::new(server.url(), "test_token".to_string());
        let handler = super::CommandHandler::new(app_with_fake_client_and_cron(cron.clone()).await);

        let out = handler
            .handle("/models set 'primary key'", "main")
            .await
            .unwrap()
            .unwrap();

        assert!(out.contains("✅ Active model key: primary key"));
        assert_eq!(
            cron.cached_model_key_for_tests().as_deref(),
            Some("primary key")
        );
    }

    fn session(id: &str, name: &str) -> Session {
        Session {
            id: id.to_string(),
            name: name.to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
        }
    }

    fn metadata(id: &str, name: &str) -> SessionMetadata {
        SessionMetadata {
            id: id.to_string(),
            name: name.to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            message_count: 0,
            last_active: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.to_string(),
            name: id.to_string(),
            requires_key: false,
            api_key_env: None,
        }
    }

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.to_string(),
            display_name: id.to_string(),
            provider: provider.to_string(),
            context_window: None,
            supports_reasoning: false,
        }
    }

    #[derive(Clone, Default)]
    struct FakeAgentClient {
        sessions: Vec<Session>,
        deleted: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl AgentClient for FakeAgentClient {
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
            Ok(session(name, name))
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

        async fn submit_turn(
            &mut self,
            _session_id: &str,
            _message: &str,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }

        fn set_stream_sink(&mut self, _sink: Option<StreamSink>) {}
    }

    fn app_with_fake_client(fake: FakeAgentClient) -> Arc<Mutex<App>> {
        let boxed: Box<dyn AgentClient + Send + Sync> = Box::new(fake);
        Arc::new(Mutex::new(App {
            workspaces: vec![Workspace {
                id: 0,
                config: WorkspaceConfig {
                    id: None,
                    name: "test".to_string(),
                    backend: Backend::Zeroclaw,
                    url: "http://127.0.0.1:8888".to_string(),
                    token_env: None,
                    token: None,
                    label: None,
                    namespace_aliases: Vec::new(),
                },
                client: Some(Arc::new(Mutex::new(boxed))),
                cron: None,
            }],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }))
    }

    async fn app_with_fake_client_and_cron(cron: ZeroclawClient) -> Arc<Mutex<App>> {
        let app = app_with_fake_client(FakeAgentClient::default());
        app.lock().await.workspaces[0].cron = Some(cron);
        app
    }

    async fn app_with_fake_client_and_mnemos(
        mnemos: crate::cli::mnemos::MnemosClient,
    ) -> Arc<Mutex<App>> {
        let app = app_with_fake_client(FakeAgentClient::default());
        app.lock().await.shared_mnemos = Some(mnemos);
        app
    }

    fn app_with_two_fake_clients(
        alpha_name: &str,
        alpha_fake: FakeAgentClient,
        beta_name: &str,
        beta_fake: FakeAgentClient,
    ) -> Arc<Mutex<App>> {
        let alpha_boxed: Box<dyn AgentClient + Send + Sync> = Box::new(alpha_fake);
        let beta_boxed: Box<dyn AgentClient + Send + Sync> = Box::new(beta_fake);
        Arc::new(Mutex::new(App {
            workspaces: vec![
                Workspace {
                    id: 0,
                    config: WorkspaceConfig {
                        id: None,
                        name: alpha_name.to_string(),
                        backend: Backend::Zeroclaw,
                        url: "http://alpha.example".to_string(),
                        token_env: None,
                        token: None,
                        label: None,
                        namespace_aliases: Vec::new(),
                    },
                    client: Some(Arc::new(Mutex::new(alpha_boxed))),
                    cron: None,
                },
                Workspace {
                    id: 1,
                    config: WorkspaceConfig {
                        id: None,
                        name: beta_name.to_string(),
                        backend: Backend::Zeroclaw,
                        url: "http://beta.example".to_string(),
                        token_env: None,
                        token: None,
                        label: None,
                        namespace_aliases: Vec::new(),
                    },
                    client: Some(Arc::new(Mutex::new(beta_boxed))),
                    cron: None,
                },
            ],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }))
    }

    fn fake_client_handle(fake: FakeAgentClient) -> Arc<Mutex<Box<dyn AgentClient + Send + Sync>>> {
        Arc::new(Mutex::new(Box::new(fake)))
    }

    fn app_with_pending_openclaw_workspace() -> Arc<Mutex<App>> {
        let alpha_boxed: Box<dyn AgentClient + Send + Sync> = Box::new(FakeAgentClient::default());
        Arc::new(Mutex::new(App {
            workspaces: vec![
                Workspace {
                    id: 0,
                    config: WorkspaceConfig {
                        id: None,
                        name: "alpha".to_string(),
                        backend: Backend::Zeroclaw,
                        url: "http://alpha.example".to_string(),
                        token_env: None,
                        token: None,
                        label: None,
                        namespace_aliases: Vec::new(),
                    },
                    client: Some(Arc::new(Mutex::new(alpha_boxed))),
                    cron: None,
                },
                Workspace {
                    id: 1,
                    config: WorkspaceConfig {
                        id: Some("ws_beta".to_string()),
                        name: "beta".to_string(),
                        backend: Backend::Openclaw,
                        url: "ws://beta.example".to_string(),
                        token_env: None,
                        token: None,
                        label: None,
                        namespace_aliases: Vec::new(),
                    },
                    client: None,
                    cron: None,
                },
            ],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }))
    }

    #[tokio::test]
    async fn advertised_local_commands_return_structured_tui_output() {
        let handler = super::CommandHandler::new(app_with_fake_client(FakeAgentClient::default()));

        for cmdline in [
            "/agent",
            "/cron list",
            "/clear",
            "/save",
            "/history",
            "/config",
            "/doctor",
            "/skill list",
            "/channels list",
            "/hardware discover",
            "/peripheral list",
            "/estop status",
        ] {
            let out = handler
                .handle(cmdline, "session-for-structured-output-test")
                .await
                .expect("command should not fail")
                .expect("advertised command should return TUI output");
            assert!(
                !out.trim().is_empty(),
                "{cmdline} should not complete silently"
            );
        }
    }

    #[tokio::test]
    async fn config_command_reads_active_zterm_config_not_legacy_file() {
        let home = tempfile::tempdir().unwrap();
        let legacy_dir = home.path().join(".zeroclaw");
        let zterm_dir = home.path().join(".zterm");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::create_dir_all(&zterm_dir).unwrap();
        std::fs::write(
            legacy_dir.join("config.toml"),
            r#"[gateway]
url = "http://legacy.example"
token = "legacy-secret"
"#,
        )
        .unwrap();
        let zterm_config_path = zterm_dir.join("config.toml");
        std::fs::write(
            &zterm_config_path,
            r#"active = "prod"

[[workspaces]]
name = "prod"
backend = "zeroclaw"
url = "http://zterm.example"
token = "zterm-secret"
"#,
        )
        .unwrap();

        let app = app_with_fake_client(FakeAgentClient::default());
        app.lock().await.config_path = zterm_config_path.clone();
        let handler = super::CommandHandler::new(app);

        let out = handler.handle("/config", "session").await.unwrap().unwrap();

        assert!(out.contains("Source: ZTerm workspace config"));
        assert!(out.contains(&zterm_config_path.display().to_string()));
        assert!(out.contains("http://zterm.example"));
        assert!(out.contains("token = \"***REDACTED***\""));
        assert!(!out.contains("zterm-secret"));
        assert!(!out.contains("http://legacy.example"));
        assert!(!out.contains("legacy-secret"));
    }

    #[test]
    fn config_command_labels_legacy_source_for_synthetic_singleton() {
        let home = tempfile::tempdir().unwrap();
        let legacy_dir = home.path().join(".zeroclaw");
        let zterm_dir = home.path().join(".zterm");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::create_dir_all(&zterm_dir).unwrap();
        let legacy_path = legacy_dir.join("config.toml");
        std::fs::write(
            &legacy_path,
            r#"[gateway]
url = "http://legacy.example"
token = "legacy-secret"
"#,
        )
        .unwrap();

        let app = App {
            workspaces: vec![Workspace {
                id: 0,
                config: WorkspaceConfig {
                    id: None,
                    name: "default".to_string(),
                    backend: Backend::Zeroclaw,
                    url: "http://cli.example".to_string(),
                    token_env: None,
                    token: None,
                    label: None,
                    namespace_aliases: Vec::new(),
                },
                client: None,
                cron: None,
            }],
            active: 0,
            shared_mnemos: None,
            config_path: zterm_dir.join("config.toml"),
        };

        let (path, source) = super::active_config_source_for_app(&app, legacy_path.clone());
        let out = super::format_config_output_for_path(super::load_config_at(&path), &path, source);

        assert!(out.contains("Source: Legacy single-workspace config"));
        assert_eq!(path, legacy_path);
        assert!(out.contains("http://legacy.example"));
        assert!(out.contains("token = \"***REDACTED***\""));
        assert!(!out.contains("legacy-secret"));
    }

    #[tokio::test]
    async fn agent_message_command_fails_closed_until_wired_to_submit_path() {
        let handler = super::CommandHandler::new(app_with_fake_client(FakeAgentClient::default()));

        let err = handler.handle("/agent -m hello", "s").await.unwrap_err();

        assert!(err.to_string().contains("/agent -m is not supported"));
    }

    #[tokio::test]
    async fn agent_provider_switch_fails_closed_until_model_switch_is_wired() {
        let handler = super::CommandHandler::new(app_with_fake_client(FakeAgentClient::default()));

        for cmdline in ["/agent -p local", "/agent --provider local"] {
            let err = handler.handle(cmdline, "s").await.unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("/agent -p/--provider"));
            assert!(msg.contains("not supported"));
            assert!(
                msg.contains("/models set") || msg.contains("/models list"),
                "{cmdline} should point at the supported model commands"
            );
        }
    }

    #[tokio::test]
    async fn daemon_and_service_commands_fail_closed_until_control_is_wired() {
        let handler = super::CommandHandler::new(app_with_fake_client(FakeAgentClient::default()));

        for cmdline in [
            "/daemon",
            "/daemon -p 42617",
            "/service status",
            "/service start",
        ] {
            let out = handler.handle(cmdline, "s").await.unwrap().unwrap();
            assert!(out.contains("❌"), "{cmdline} should render as an error");
            assert!(
                out.contains("no action taken"),
                "{cmdline} should not imply control happened"
            );
        }
    }

    #[tokio::test]
    async fn doctor_reports_real_or_unknown_status_not_static_green_checks() {
        let handler = super::CommandHandler::new(app_with_fake_client(FakeAgentClient::default()));

        let out = handler.handle("/doctor", "s").await.unwrap().unwrap();
        assert!(out.contains("Gateway: [ok]"));
        assert!(out.contains("Config: [ok]"));
        assert!(out.contains("Memory: [unknown]"));
        assert!(out.contains("Channels: [unknown]"));
        assert!(!out.contains('✓'));

        let models = handler
            .handle("/doctor models", "s")
            .await
            .unwrap()
            .unwrap();
        assert!(models.contains("Provider catalog: [ok]"));
        assert!(models.contains("Provider connectivity: [unknown]"));
        assert!(!models.contains("Groq"));
    }

    #[tokio::test]
    async fn workspace_commands_redact_url_fragment_secrets() {
        let app = app_with_two_fake_clients(
            "alpha",
            FakeAgentClient::default(),
            "beta",
            FakeAgentClient::default(),
        );
        {
            let mut app = app.lock().await;
            app.workspaces[0].config.url =
                "wss://alpha.example/ws#access_token=alpha-fragment-secret".to_string();
            app.workspaces[1].config.url =
                "wss://operator:embedded-password@[beta/ws?client_secret=beta-query-secret#token=beta-fragment-secret"
                    .to_string();
        }
        let handler = super::CommandHandler::new(app);

        let info = handler
            .handle("/workspace info", "s")
            .await
            .unwrap()
            .unwrap();
        assert!(info.contains("url:       wss://alpha.example/ws#REDACTED"));
        assert!(!info.contains("alpha-fragment-secret"));

        let list = handler
            .handle("/workspace list", "s")
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("wss://alpha.example/ws#REDACTED"));
        assert!(list.contains("wss://redacted:redacted@[beta/ws?client_secret=REDACTED#REDACTED"));
        for leaked in [
            "alpha-fragment-secret",
            "operator",
            "embedded-password",
            "beta-query-secret",
            "beta-fragment-secret",
        ] {
            assert!(!list.contains(leaked), "{leaked} leaked in {list}");
        }
    }

    #[tokio::test]
    async fn workspace_switch_releases_app_lock_while_activation_is_in_flight() {
        let app = app_with_pending_openclaw_workspace();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
        let release_rx = Arc::new(StdMutex::new(Some(release_rx)));
        let activator: super::WorkspaceActivator = Arc::new(move |config| {
            let entered_tx = Arc::clone(&entered_tx);
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                assert_eq!(config.name, "beta");
                if let Some(tx) = entered_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let release_rx = {
                    let mut guard = release_rx.lock().unwrap();
                    guard.take().expect("activation should run once")
                };
                let _ = release_rx.await;
                Ok(fake_client_handle(FakeAgentClient::default()))
            })
        });
        let handler =
            super::CommandHandler::new_with_workspace_activator(Arc::clone(&app), activator);

        let switch_task =
            tokio::spawn(async move { handler.handle("/workspace switch beta", "s").await });
        entered_rx.await.expect("activation should start");
        let guard = tokio::time::timeout(std::time::Duration::from_millis(100), app.lock())
            .await
            .expect("App mutex must not be held while activation is waiting");
        assert_eq!(guard.active, 0);
        drop(guard);

        release_tx.send(()).unwrap();
        let out = switch_task
            .await
            .expect("switch task should not panic")
            .expect("switch command should complete")
            .expect("switch command should return output");

        assert!(out.contains("switched to workspace: beta"));
        let app = app.lock().await;
        assert_eq!(app.active_workspace().unwrap().config.name, "beta");
        assert!(app.active_workspace().unwrap().is_activated());
    }

    #[tokio::test]
    async fn workspace_switch_refreshes_target_zeroclaw_model_state_before_activation() {
        let mut beta_server = mockito::Server::new_async().await;
        let beta_config = beta_server
            .mock("GET", "/api/config")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "content": r#"
[providers]
fallback = "gemini"

[providers.models.primary]
name = "openai_compat"
model = "gpt-test"

[providers.models.consult]
name = "gemini"
model = "gemini-test"
"#
                })
                .to_string(),
            )
            .create_async()
            .await;

        let alpha_client = ZeroclawClient::new("http://alpha.example".to_string(), String::new());
        let beta_client = ZeroclawClient::new(beta_server.url(), String::new());
        let beta_cron = beta_client.clone();
        let app = Arc::new(Mutex::new(App {
            workspaces: vec![
                Workspace {
                    id: 0,
                    config: WorkspaceConfig {
                        id: None,
                        name: "alpha".to_string(),
                        backend: Backend::Zeroclaw,
                        url: "http://alpha.example".to_string(),
                        token_env: None,
                        token: None,
                        label: None,
                        namespace_aliases: Vec::new(),
                    },
                    client: Some(Arc::new(Mutex::new(Box::new(alpha_client)))),
                    cron: None,
                },
                Workspace {
                    id: 1,
                    config: WorkspaceConfig {
                        id: None,
                        name: "beta".to_string(),
                        backend: Backend::Zeroclaw,
                        url: beta_server.url(),
                        token_env: None,
                        token: None,
                        label: None,
                        namespace_aliases: Vec::new(),
                    },
                    client: Some(Arc::new(Mutex::new(Box::new(beta_client)))),
                    cron: Some(beta_cron.clone()),
                },
            ],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }));
        let handler = super::CommandHandler::new(Arc::clone(&app));

        assert_eq!(beta_cron.cached_model_key_for_tests(), None);
        let out = handler
            .handle("/workspace switch beta", "s")
            .await
            .expect("workspace switch should complete")
            .expect("workspace switch should return output");

        beta_config.assert_async().await;
        assert!(out.contains("switched to workspace: beta"));
        assert_eq!(
            beta_cron.cached_model_key_for_tests().as_deref(),
            Some("consult")
        );
        let app = app.lock().await;
        assert_eq!(app.active_workspace().unwrap().config.name, "beta");
    }

    #[tokio::test]
    async fn workspace_switch_surfaces_zeroclaw_model_refresh_failure_without_switching() {
        let mut beta_server = mockito::Server::new_async().await;
        let beta_config = beta_server
            .mock("GET", "/api/config")
            .with_status(500)
            .with_header("content-type", "text/plain")
            .with_body("boom")
            .create_async()
            .await;

        let alpha_client = ZeroclawClient::new("http://alpha.example".to_string(), String::new());
        let beta_client = ZeroclawClient::new(beta_server.url(), String::new());
        let app = Arc::new(Mutex::new(App {
            workspaces: vec![
                Workspace {
                    id: 0,
                    config: WorkspaceConfig {
                        id: None,
                        name: "alpha".to_string(),
                        backend: Backend::Zeroclaw,
                        url: "http://alpha.example".to_string(),
                        token_env: None,
                        token: None,
                        label: None,
                        namespace_aliases: Vec::new(),
                    },
                    client: Some(Arc::new(Mutex::new(Box::new(alpha_client)))),
                    cron: None,
                },
                Workspace {
                    id: 1,
                    config: WorkspaceConfig {
                        id: None,
                        name: "beta".to_string(),
                        backend: Backend::Zeroclaw,
                        url: beta_server.url(),
                        token_env: None,
                        token: None,
                        label: None,
                        namespace_aliases: Vec::new(),
                    },
                    client: Some(Arc::new(Mutex::new(Box::new(beta_client.clone())))),
                    cron: Some(beta_client),
                },
            ],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }));
        let handler = super::CommandHandler::new(Arc::clone(&app));

        let out = handler
            .handle("/workspace switch beta", "s")
            .await
            .expect("workspace switch should complete with rendered failure")
            .expect("workspace switch should return output");

        beta_config.assert_async().await;
        assert!(out.contains("failed to refresh model state"));
        let app = app.lock().await;
        assert_eq!(app.active_workspace().unwrap().config.name, "alpha");
    }

    #[tokio::test]
    async fn concurrent_workspace_switch_keeps_last_requested_workspace() {
        let app = app_with_pending_openclaw_workspace();
        {
            let mut app = app.lock().await;
            app.workspaces.push(Workspace {
                id: 2,
                config: WorkspaceConfig {
                    id: Some("ws_gamma".to_string()),
                    name: "gamma".to_string(),
                    backend: Backend::Openclaw,
                    url: "ws://gamma.example".to_string(),
                    token_env: None,
                    token: None,
                    label: None,
                    namespace_aliases: Vec::new(),
                },
                client: None,
                cron: None,
            });
        }

        let (beta_entered_tx, beta_entered_rx) = tokio::sync::oneshot::channel();
        let (beta_release_tx, beta_release_rx) = tokio::sync::oneshot::channel();
        let beta_entered_tx = Arc::new(StdMutex::new(Some(beta_entered_tx)));
        let beta_release_rx = Arc::new(StdMutex::new(Some(beta_release_rx)));
        let activator: super::WorkspaceActivator = Arc::new(move |config| {
            let beta_entered_tx = Arc::clone(&beta_entered_tx);
            let beta_release_rx = Arc::clone(&beta_release_rx);
            Box::pin(async move {
                match config.name.as_str() {
                    "beta" => {
                        if let Some(tx) = beta_entered_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        let release_rx = {
                            let mut guard = beta_release_rx.lock().unwrap();
                            guard.take().expect("beta activation should run once")
                        };
                        let _ = release_rx.await;
                    }
                    "gamma" => {}
                    other => panic!("unexpected activation for {other}"),
                }
                Ok(fake_client_handle(FakeAgentClient::default()))
            })
        });
        let handler = Arc::new(super::CommandHandler::new_with_workspace_activator(
            Arc::clone(&app),
            activator,
        ));

        let beta_handler = Arc::clone(&handler);
        let beta_task =
            tokio::spawn(async move { beta_handler.handle("/workspace switch beta", "s").await });
        beta_entered_rx
            .await
            .expect("beta activation should be in flight");

        let gamma_out = handler
            .handle("/workspace switch gamma", "s")
            .await
            .expect("gamma switch should complete")
            .expect("gamma switch should return output");
        assert!(gamma_out.contains("switched to workspace: gamma"));
        assert_eq!(
            app.lock().await.active_workspace().unwrap().config.name,
            "gamma"
        );

        beta_release_tx.send(()).unwrap();
        let beta_out = beta_task
            .await
            .expect("beta switch task should not panic")
            .expect("beta switch command should complete")
            .expect("beta switch command should return output");
        assert!(beta_out.contains("superseded"));
        let app = app.lock().await;
        assert_eq!(app.active_workspace().unwrap().config.name, "gamma");
        assert!(app.active_workspace().unwrap().is_activated());
    }

    #[tokio::test]
    async fn invalid_workspace_switch_does_not_cancel_valid_in_flight_switch() {
        let app = app_with_pending_openclaw_workspace();
        let (beta_entered_tx, beta_entered_rx) = tokio::sync::oneshot::channel();
        let (beta_release_tx, beta_release_rx) = tokio::sync::oneshot::channel();
        let beta_entered_tx = Arc::new(StdMutex::new(Some(beta_entered_tx)));
        let beta_release_rx = Arc::new(StdMutex::new(Some(beta_release_rx)));
        let activator: super::WorkspaceActivator = Arc::new(move |config| {
            let beta_entered_tx = Arc::clone(&beta_entered_tx);
            let beta_release_rx = Arc::clone(&beta_release_rx);
            Box::pin(async move {
                assert_eq!(config.name, "beta");
                if let Some(tx) = beta_entered_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let release_rx = {
                    let mut guard = beta_release_rx.lock().unwrap();
                    guard.take().expect("beta activation should run once")
                };
                let _ = release_rx.await;
                Ok(fake_client_handle(FakeAgentClient::default()))
            })
        });
        let handler = Arc::new(super::CommandHandler::new_with_workspace_activator(
            Arc::clone(&app),
            activator,
        ));

        let beta_handler = Arc::clone(&handler);
        let beta_task =
            tokio::spawn(async move { beta_handler.handle("/workspace switch beta", "s").await });
        beta_entered_rx
            .await
            .expect("beta activation should be in flight");

        let missing_out = handler
            .handle("/workspace switch missing", "s")
            .await
            .expect("missing workspace switch should complete")
            .expect("missing workspace switch should return output");
        assert!(missing_out.contains("no workspace named \"missing\""));

        beta_release_tx.send(()).unwrap();
        let beta_out = beta_task
            .await
            .expect("beta switch task should not panic")
            .expect("beta switch command should complete")
            .expect("beta switch command should return output");
        assert!(beta_out.contains("switched to workspace: beta"));
        let app = app.lock().await;
        assert_eq!(app.active_workspace().unwrap().config.name, "beta");
        assert!(app.active_workspace().unwrap().is_activated());
    }

    #[test]
    fn config_output_masks_persisted_gateway_token() {
        let out = super::format_config_output(Ok(r#"[gateway]
url = "ws://gateway.example"
token = "persisted-token-value"

[providers.openai]
api_key = "persisted-api-key"
"#
        .to_string()));

        assert!(out.contains("⚙️  Configuration:"));
        assert!(out.contains("url = \"ws://gateway.example\""));
        assert!(out.contains("token = \"***REDACTED***\""));
        assert!(out.contains("api_key = \"***REDACTED***\""));
        assert!(!out.contains("persisted-token-value"));
        assert!(!out.contains("persisted-api-key"));
    }

    #[test]
    fn config_output_masks_inline_table_secrets() {
        let out = super::format_config_output(Ok(r#"
	gateway = { url = "ws://gateway.example", token = "inline-token-value" }
	provider = { name = "openai", api_key = "inline-provider-api-key" }

[providers]
openai = { model = "gpt", api_key = "inline-api-key" }
"#
        .to_string()));

        assert!(out.contains("***REDACTED***"));
        assert!(out.contains("ws://gateway.example"));
        assert!(out.contains("model = \"gpt\""));
        assert!(!out.contains("inline-token-value"));
        assert!(!out.contains("inline-provider-api-key"));
        assert!(!out.contains("inline-api-key"));
    }

    #[test]
    fn config_output_redacts_url_embedded_credentials_in_valid_toml() {
        let out = super::format_config_output(Ok(r#"
	[gateway]
		url = "wss://operator:embedded-password@gateway.example/ws?api_token=url-api-token&refresh_token=url-refresh-token&client_secret=url-client-secret&session_token=url-session-token&room=alpha#access_token=url-fragment-token"
		"#
        .to_string()));

        assert!(out.contains("url = \"wss://redacted:redacted@gateway.example/ws?api_token=REDACTED&refresh_token=REDACTED&client_secret=REDACTED&session_token=REDACTED&room=alpha#REDACTED\""));
        for leaked in [
            "operator",
            "embedded-password",
            "url-api-token",
            "url-refresh-token",
            "url-client-secret",
            "url-session-token",
            "url-fragment-token",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked in {out}");
        }
    }

    #[test]
    fn config_output_redacts_legacy_url_env_values_in_valid_toml() {
        let out = super::format_config_output(Ok(r#"
	ZEROCLAW_URL = "wss://legacy:legacy-password@gateway.example/ws?access_token=legacy-token&model=gpt"
	"#
        .to_string()));

        assert!(out.contains(
            "ZEROCLAW_URL = \"wss://redacted:redacted@gateway.example/ws?access_token=REDACTED&model=gpt\""
        ));
        for leaked in ["legacy:legacy-password", "legacy-token"] {
            assert!(!out.contains(leaked), "{leaked} leaked in {out}");
        }
    }

    #[test]
    fn config_output_redacts_malformed_url_like_secret_values() {
        let out = super::format_config_output(Ok(r#"
	[gateway]
		url = "wss://operator:embedded-password@[gateway/ws?access_token=url-token&room=alpha#token=fragment-token"
		"#
        .to_string()));

        assert!(out.contains("url = \"wss://redacted:redacted@[gateway/ws?access_token=REDACTED&room=alpha#REDACTED\""));
        for leaked in [
            "operator",
            "embedded-password",
            "url-token",
            "fragment-token",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked in {out}");
        }
    }

    #[test]
    fn config_output_masks_common_api_and_private_key_spellings() {
        let out = super::format_config_output(Ok(r#"
	[providers.openai]
apiKey = "camel-api-key"
apikey = "flat-api-key"
api-key = "dash-api-key"
private_key = "snake-private-key"
private-key = "dash-private-key"

[providers.inline]
openai = { model = "gpt", apiKey = "inline-camel-key", private-key = "inline-private-key" }
"#
        .to_string()));

        for leaked in [
            "camel-api-key",
            "flat-api-key",
            "dash-api-key",
            "snake-private-key",
            "dash-private-key",
            "inline-camel-key",
            "inline-private-key",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked in {out}");
        }
        assert!(out.contains("model = \"gpt\""));
        assert!(out.matches("***REDACTED***").count() >= 7);
    }

    #[test]
    fn fallback_config_redactor_normalizes_separators_and_case() {
        let out = super::redact_config_secrets(
            r#"
[unterminated
apiKey = "camel-api-key"
api-key = "dash-api-key"
private_key = "snake-private-key"
"#,
        );

        assert!(out.contains("apiKey = \"***REDACTED***\""));
        assert!(out.contains("api-key = \"***REDACTED***\""));
        assert!(out.contains("private_key = \"***REDACTED***\""));
        assert!(!out.contains("camel-api-key"));
        assert!(!out.contains("dash-api-key"));
        assert!(!out.contains("snake-private-key"));
    }

    #[test]
    fn fallback_config_redactor_masks_malformed_inline_table_secrets() {
        let out = super::redact_config_secrets(
            r#"
	[unterminated
gateway = { url = "ws://gateway.example", token = "inline-token-value" }
provider = { name = "openai", api_key = "inline-api-key", privateKey = "inline-private-key" }
provider_settings = { tokens = ["array-token-1", "array-token-2"], name = "kept" }
"#,
        );

        assert!(out.contains("url = \"ws://gateway.example\""));
        assert!(out.contains("token = \"***REDACTED***\""));
        assert!(out.contains("api_key = \"***REDACTED***\""));
        assert!(out.contains("privateKey = \"***REDACTED***\""));
        assert!(out.contains("tokens = \"***REDACTED***\""));
        assert!(out.contains("name = \"kept\""));
        assert!(!out.contains("inline-token-value"));
        assert!(!out.contains("inline-api-key"));
        assert!(!out.contains("inline-private-key"));
        assert!(!out.contains("array-token-1"));
        assert!(!out.contains("array-token-2"));
    }

    #[test]
    fn fallback_config_redactor_masks_url_embedded_credentials() {
        let out = super::redact_config_secrets(
            r#"
	[unterminated
		url = "wss://operator:embedded-password@gateway.example/ws?api_token=url-api-token&refresh_token=url-refresh-token&client_secret=url-client-secret&session_token=url-session-token&room=alpha#access_token=url-fragment-token"
		"#,
        );

        assert!(out.contains("url = \"wss://redacted:redacted@gateway.example/ws?api_token=REDACTED&refresh_token=REDACTED&client_secret=REDACTED&session_token=REDACTED&room=alpha#REDACTED\""));
        for leaked in [
            "operator",
            "embedded-password",
            "url-api-token",
            "url-refresh-token",
            "url-client-secret",
            "url-session-token",
            "url-fragment-token",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked in {out}");
        }
    }

    #[tokio::test]
    async fn estop_status_fails_closed_until_backend_exists() {
        let handler = super::CommandHandler::new(app_with_fake_client(FakeAgentClient::default()));

        let out = handler
            .handle("/estop status", "session-for-estop-test")
            .await
            .expect("command should not fail")
            .expect("estop status should return TUI output");

        assert!(out.contains("Emergency Stop: Unknown"));
        assert!(out.contains("not implemented"));
        assert!(!out.contains("Disengaged"));
    }

    #[tokio::test]
    async fn command_handler_refuses_to_delete_active_backend_session() {
        let deleted = Arc::new(StdMutex::new(Vec::new()));
        let fake = FakeAgentClient {
            sessions: vec![session("sess-active", "Research")],
            deleted: Arc::clone(&deleted),
        };
        let handler = super::CommandHandler::new(app_with_fake_client(fake));

        let out = handler
            .handle("/session delete Research", "sess-active")
            .await
            .expect("command should complete")
            .expect("delete command should return output");

        assert!(out.contains("Cannot delete active session"));
        assert!(out.contains("switch to another session"));
        assert!(deleted.lock().unwrap().is_empty());
    }

    #[test]
    fn command_handler_scopes_clear_save_and_delete_cleanup_to_active_workspace() {
        run_async_with_isolated_home(|| async {
            let suffix = uuid::Uuid::new_v4();
            let alpha_name = format!("alpha-{suffix}");
            let beta_name = format!("beta-{suffix}");
            let alpha_scope = storage::workspace_scope("zeroclaw", &alpha_name, None).unwrap();
            let beta_scope = storage::workspace_scope("zeroclaw", &beta_name, None).unwrap();
            let mut alpha_meta = metadata("main", "Alpha Main");
            alpha_meta.message_count = 4;
            let mut beta_meta = metadata("main", "Beta Main");
            beta_meta.message_count = 8;
            storage::save_scoped_session_metadata(&alpha_scope, &alpha_meta).unwrap();
            storage::save_scoped_session_metadata(&beta_scope, &beta_meta).unwrap();
            std::fs::write(
                storage::scoped_session_history_file(&alpha_scope, "main").unwrap(),
                "alpha history\n",
            )
            .unwrap();
            std::fs::write(
                storage::scoped_session_history_file(&beta_scope, "main").unwrap(),
                "beta history\n",
            )
            .unwrap();

            let alpha_deleted = Arc::new(StdMutex::new(Vec::new()));
            let beta_deleted = Arc::new(StdMutex::new(Vec::new()));
            let handler = super::CommandHandler::new(app_with_two_fake_clients(
                &alpha_name,
                FakeAgentClient {
                    sessions: vec![session("active", "Active"), session("main", "Main")],
                    deleted: Arc::clone(&alpha_deleted),
                },
                &beta_name,
                FakeAgentClient {
                    sessions: vec![session("main", "Main")],
                    deleted: Arc::clone(&beta_deleted),
                },
            ));

            let clear_out = handler.handle("/clear", "main").await.unwrap().unwrap();
            assert!(clear_out.contains("cleared"));
            let alpha_history = storage::scoped_session_history_file(&alpha_scope, "main").unwrap();
            assert_eq!(
                storage::load_scoped_session_metadata(&alpha_scope, "main")
                    .unwrap()
                    .message_count,
                0
            );
            assert!(
                !alpha_history.exists(),
                "/clear must remove persisted transcript history"
            );
            assert_eq!(
                storage::load_scoped_session_metadata(&beta_scope, "main")
                    .unwrap()
                    .message_count,
                8
            );

            let tempdir = tempfile::tempdir().unwrap();
            let save_path = tempdir.path().join("main-session.txt");
            let save_cmd = format!("/save {}", save_path.display());
            let save_out = handler.handle(&save_cmd, "main").await.unwrap().unwrap();
            assert!(save_out.contains("No history to save"));
            assert!(!save_path.exists());

            std::fs::write(&alpha_history, "alpha after clear\n").unwrap();
            let save_out = handler.handle(&save_cmd, "main").await.unwrap().unwrap();
            assert!(save_out.contains("Session saved"));
            assert_eq!(
                std::fs::read_to_string(&save_path).unwrap(),
                "alpha after clear\n"
            );

            let existing_path = tempdir.path().join("existing.txt");
            std::fs::write(&existing_path, "keep me\n").unwrap();
            let save_existing_cmd = format!("/save {}", existing_path.display());
            let save_existing_out = handler
                .handle(&save_existing_cmd, "main")
                .await
                .unwrap()
                .unwrap();
            assert!(save_existing_out.contains("Refusing to overwrite"));
            assert_eq!(
                std::fs::read_to_string(&existing_path).unwrap(),
                "keep me\n"
            );

            let save_extra_out = handler
                .handle("/save session backup.txt", "main")
                .await
                .unwrap()
                .unwrap();
            assert!(save_extra_out.contains("single token"));

            let delete_out = handler
                .handle("/session delete main", "active")
                .await
                .unwrap()
                .unwrap();
            assert!(delete_out.contains("Deleted session"));
            assert_eq!(alpha_deleted.lock().unwrap().as_slice(), ["main"]);
            assert!(beta_deleted.lock().unwrap().is_empty());
            assert!(storage::load_scoped_session_metadata(&alpha_scope, "main").is_err());
            assert!(!storage::scoped_session_history_file(&alpha_scope, "main")
                .unwrap()
                .exists());
            assert_eq!(
                storage::load_scoped_session_metadata(&beta_scope, "main")
                    .unwrap()
                    .message_count,
                8
            );
            assert_eq!(
                std::fs::read_to_string(
                    storage::scoped_session_history_file(&beta_scope, "main").unwrap()
                )
                .unwrap(),
                "beta history\n"
            );

            storage::delete_scoped_session(&beta_scope, "main").unwrap();
        });
    }

    #[test]
    fn save_exports_transcript_appended_by_turn_paths() {
        run_async_with_isolated_home(|| async {
            let suffix = uuid::Uuid::new_v4();
            let workspace_name = format!("save-{suffix}");
            let scope = storage::workspace_scope("zeroclaw", &workspace_name, None).unwrap();
            storage::save_scoped_session_metadata(&scope, &metadata("main", "Main")).unwrap();
            storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
            storage::append_scoped_session_history(&scope, "main", "assistant", "hi there")
                .unwrap();
            let handler = super::CommandHandler::new(app_with_two_fake_clients(
                &workspace_name,
                FakeAgentClient {
                    sessions: vec![session("main", "Main")],
                    deleted: Arc::new(StdMutex::new(Vec::new())),
                },
                "other",
                FakeAgentClient::default(),
            ));
            let tempdir = tempfile::tempdir().unwrap();
            let save_path = tempdir.path().join("turn-transcript.txt");
            let save_cmd = format!("/save {}", save_path.display());

            let out = handler.handle(&save_cmd, "main").await.unwrap().unwrap();

            assert!(out.contains("Session saved"));
            let saved = std::fs::read_to_string(&save_path).unwrap();
            assert!(saved.contains(r#""role":"user""#));
            assert!(saved.contains(r#""content":"hello""#));
            assert!(saved.contains(r#""role":"assistant""#));
            assert!(saved.contains(r#""content":"hi there""#));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                assert_eq!(
                    std::fs::metadata(&save_path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }

            storage::mark_scoped_session_history_incomplete(
                &scope,
                "main",
                "assistant append failed",
            )
            .unwrap();
            let blocked_path = tempdir.path().join("blocked-transcript.txt");
            let blocked_cmd = format!("/save {}", blocked_path.display());
            let blocked = handler.handle(&blocked_cmd, "main").await.unwrap().unwrap();
            assert!(blocked.contains("transcript is incomplete"));
            assert!(!blocked_path.exists());

            storage::delete_scoped_session(&scope, "main").unwrap();
        });
    }

    #[test]
    fn private_export_atomic_write_removes_temp_and_final_on_write_failure() {
        let tempdir = tempfile::tempdir().unwrap();
        let save_path = tempdir.path().join("partial-export.txt");

        let err = super::write_private_export_atomically(&save_path, |dst| {
            use std::io::Write;

            dst.write_all(b"partial transcript")?;
            Err(std::io::Error::other("injected write failure"))
        })
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert!(!save_path.exists());
        let leftovers = std::fs::read_dir(tempdir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "atomic export left temporary files behind: {leftovers:?}"
        );
    }

    #[test]
    fn private_export_atomic_write_reports_parent_sync_failure() {
        let tempdir = tempfile::tempdir().unwrap();
        let save_path = tempdir.path().join("sync-failure-export.txt");

        let err = super::write_private_export_atomically_with_sync(
            &save_path,
            |dst| {
                use std::io::Write;

                dst.write_all(b"transcript")?;
                Ok(())
            },
            |_path| Err(std::io::Error::other("injected parent sync failure")),
        )
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert!(err.to_string().contains("injected parent sync failure"));
    }

    #[test]
    fn clear_removes_transcript_history_even_when_metadata_is_missing() {
        run_async_with_isolated_home(|| async {
            let suffix = uuid::Uuid::new_v4();
            let workspace_name = format!("clear-missing-meta-{suffix}");
            let scope = storage::workspace_scope("zeroclaw", &workspace_name, None).unwrap();
            storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
            storage::mark_scoped_session_history_incomplete(
                &scope,
                "main",
                "assistant append failed",
            )
            .unwrap();
            let history = storage::scoped_session_history_file(&scope, "main").unwrap();
            let marker = storage::scoped_session_history_incomplete_file(&scope, "main").unwrap();
            assert!(storage::load_scoped_session_metadata(&scope, "main").is_err());

            let handler = super::CommandHandler::new(app_with_two_fake_clients(
                &workspace_name,
                FakeAgentClient {
                    sessions: vec![session("main", "Main")],
                    deleted: Arc::new(StdMutex::new(Vec::new())),
                },
                "other",
                FakeAgentClient::default(),
            ));

            let out = handler.handle("/clear", "main").await.unwrap().unwrap();

            assert!(out.contains("cleared"));
            assert!(out.contains("backend session context retained"));
            assert!(!history.exists());
            assert!(!marker.exists());
        });
    }

    #[test]
    fn clear_force_removes_stale_turn_lock_and_pending_markers() {
        run_async_with_isolated_home(|| async {
            let suffix = uuid::Uuid::new_v4();
            let workspace_name = format!("clear-force-{suffix}");
            let scope = storage::workspace_scope("zeroclaw", &workspace_name, None).unwrap();
            storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
            let lock = storage::acquire_scoped_session_history_turn_lock(
                &scope, "main", "turn-a", "pending",
            )
            .unwrap();
            storage::mark_scoped_session_history_incomplete(
                &scope,
                "main",
                "assistant append failed",
            )
            .unwrap();
            std::mem::forget(lock);
            storage::write_stale_scoped_session_history_turn_lock_owner_for_tests(&scope, "main")
                .unwrap();

            let history = storage::scoped_session_history_file(&scope, "main").unwrap();
            let marker = storage::scoped_session_history_incomplete_file(&scope, "main").unwrap();
            let pending_dir = storage::scoped_session_history_pending_dir(&scope, "main").unwrap();
            let lock_dir = storage::scoped_session_history_turn_lock_dir(&scope, "main").unwrap();
            assert!(history.exists());
            assert!(marker.exists());
            assert!(pending_dir.exists());
            assert!(lock_dir.exists());

            let handler = super::CommandHandler::new(app_with_two_fake_clients(
                &workspace_name,
                FakeAgentClient {
                    sessions: vec![session("main", "Main")],
                    deleted: Arc::new(StdMutex::new(Vec::new())),
                },
                "other",
                FakeAgentClient::default(),
            ));

            let out = handler
                .handle("/clear --force", "main")
                .await
                .unwrap()
                .unwrap();

            assert!(out.contains("force-cleared"));
            assert!(out.contains("backend session context retained"));
            assert!(!history.exists());
            assert!(!marker.exists());
            assert!(!pending_dir.exists());
            assert!(!lock_dir.exists());
        });
    }

    #[test]
    fn delete_resolver_prefers_backend_display_name_match() {
        let backend = vec![session("sess-123", "Research")];
        let target = super::choose_delete_session_target("Research", Some(&backend), &[]).unwrap();

        assert_eq!(target.id, "sess-123");
        assert_eq!(target.name, "Research");
        assert_eq!(target.local_id, None);
    }

    #[test]
    fn delete_resolver_fails_on_local_only_display_name_when_backend_list_available() {
        let local = metadata("local-456", "Planning");
        let local_sessions = vec![local];
        let err = super::choose_delete_session_target("Planning", Some(&[]), &local_sessions)
            .unwrap_err();

        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn delete_resolver_fails_when_backend_list_is_authoritative_and_no_match() {
        let backend = vec![session("sess-123", "Research")];
        let err = super::choose_delete_session_target("missing", Some(&backend), &[]).unwrap_err();

        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn delete_resolver_fails_closed_on_exact_local_id_when_backend_list_unavailable() {
        let local = vec![metadata("local-123", "Planning")];
        let err = super::choose_delete_session_target("local-123", None, &local).unwrap_err();

        assert!(err.to_string().contains("backend sessions unavailable"));
        assert!(err
            .to_string()
            .contains("local metadata is not trusted for backend deletes"));
    }

    #[test]
    fn delete_resolver_fails_closed_on_raw_id_when_backend_list_unavailable() {
        let err = super::choose_delete_session_target("raw-id", None, &[]).unwrap_err();

        assert!(err.to_string().contains("backend sessions unavailable"));
    }

    #[test]
    fn delete_resolver_fails_closed_on_local_name_when_backend_list_unavailable() {
        let local = vec![metadata("local-123", "Planning")];
        let err = super::choose_delete_session_target("Planning", None, &local).unwrap_err();

        assert!(err.to_string().contains("backend sessions unavailable"));
    }

    #[test]
    fn delete_resolver_fails_on_local_only_exact_id_when_backend_list_available() {
        let local = vec![metadata("local-123", "Planning")];
        let err = super::choose_delete_session_target("local-123", Some(&[]), &local).unwrap_err();

        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn delete_resolver_does_not_trust_unsafe_local_id_when_backend_list_available() {
        let local = vec![metadata("../owned", "Planning")];
        let err = super::choose_delete_session_target("../owned", Some(&[]), &local).unwrap_err();

        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn delete_resolver_does_not_cleanup_unsafe_backend_id_match() {
        let backend = vec![session("../owned", "Research")];
        let local = vec![metadata("../owned", "Research")];
        let target = super::choose_delete_session_target("../owned", Some(&backend), &local)
            .expect("backend id can be sent to backend but not used for local cleanup");

        assert_eq!(target.id, "../owned");
        assert_eq!(target.local_id, None);
    }

    #[test]
    fn delete_resolver_prefers_exact_backend_id_over_duplicate_names() {
        let backend = vec![
            session("sess-123", "Research"),
            session("sess-456", "Research"),
        ];
        let local = vec![
            metadata("local-123", "Research"),
            metadata("local-456", "Research"),
        ];

        let target =
            super::choose_delete_session_target("sess-456", Some(&backend), &local).unwrap();

        assert_eq!(target.id, "sess-456");
        assert_eq!(target.name, "Research");
        assert_eq!(target.local_id, None);
    }

    #[test]
    fn delete_resolver_uses_matching_safe_local_id_only_for_cleanup() {
        let backend = vec![session("sess-123", "Research")];
        let local = vec![metadata("sess-123", "Stale Local Name")];

        let target = super::choose_delete_session_target("Research", Some(&backend), &local)
            .expect("backend display name should select authoritative backend id");

        assert_eq!(target.id, "sess-123");
        assert_eq!(target.name, "Research");
        assert_eq!(target.local_id.as_deref(), Some("sess-123"));
    }

    #[test]
    fn delete_resolver_fails_closed_on_duplicate_backend_names() {
        let backend = vec![
            session("sess-123", "Research"),
            session("sess-456", "Research"),
        ];

        let err = super::choose_delete_session_target("Research", Some(&backend), &[]).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("ambiguous session name/label 'Research'"));
        assert!(msg.contains("sess-123"));
        assert!(msg.contains("sess-456"));
        assert!(msg.contains("explicit id"));
    }

    #[test]
    fn delete_resolver_ignores_duplicate_local_names_when_backend_list_available() {
        let local = vec![
            metadata("local-123", "Planning"),
            metadata("local-456", "Planning"),
        ];

        let err = super::choose_delete_session_target("Planning", Some(&[]), &local).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("not found"));
    }
}
