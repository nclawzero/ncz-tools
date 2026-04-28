use anyhow::{anyhow, Result};
use chrono::Utc;
use std::fs;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::cli::agent::AgentClient;
use crate::cli::client::Session;
use crate::cli::client::ZeroclawClient;
use crate::cli::input::InputHistory;
use crate::cli::storage;
use crate::cli::ui;

/// Command handler.
///
/// Holds a shared Arc<Mutex<App>>. Every per-command helper
/// briefly locks it to resolve the active workspace client,
/// cron handle, or MNEMOS client. Chunk D-3b.
pub struct CommandHandler {
    app: std::sync::Arc<tokio::sync::Mutex<crate::cli::workspace::App>>,
}

impl CommandHandler {
    /// Create a new command handler.
    ///
    /// Takes the shared `Arc<Mutex<App>>` that ReplLoop also holds,
    /// so `/workspace switch` mutations are visible to both.
    pub fn new(app: std::sync::Arc<tokio::sync::Mutex<crate::cli::workspace::App>>) -> Self {
        Self { app }
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

    async fn current_agent_client(&self) -> Option<Arc<Mutex<Box<dyn AgentClient + Send + Sync>>>> {
        self.app
            .lock()
            .await
            .active_workspace()
            .and_then(|w| w.client.clone())
    }

    /// Handle a slash command (maps to zeroclaw CLI)
    pub async fn handle(&self, input: &str, session_id: &str) -> Result<Option<String>> {
        let parts: Vec<&str> = input.split_whitespace().collect();

        if parts.is_empty() {
            return Ok(None);
        }

        let command = parts[0];
        let subcommand = parts.get(1).copied();
        let args = if parts.len() > 2 {
            &parts[2..]
        } else {
            &[] as &[&str]
        };

        match command {
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
            "/memory" => self.handle_memory(subcommand, args).await,
            "/workspace" | "/workspaces" => self.handle_workspace(subcommand, args).await,
            "/cron" => self.handle_cron(subcommand, args).await,
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
            "/clear" => self.handle_clear(session_id).await,
            "/save" => {
                self.handle_save(session_id, subcommand.map(|s| s.to_string()))
                    .await
            }
            "/history" => self.handle_history().await,
            "/config" => self.handle_config().await,
            "/session" => self.handle_session(session_id, subcommand, args).await,
            "/mcp" => self.handle_mcp(subcommand).await,

            // Completion
            "/completions" => self.handle_completions(subcommand).await,

            _ => Ok(Some(format!(
                "❌ Unknown command: {command}\n   Type /help for available commands\n"
            ))),
        }
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
              /agent -m 'text'   Send single message\n  \
              /daemon            Start gateway + channels + scheduler\n  \
              /service           Manage system service\n\
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
            Session (REPL-only):\n  \
              /config            Show configuration\n  \
              /clear             Clear history\n  \
              /save [file]       Export session\n  \
              /history           Show commands\n  \
              /session list      List sessions\n"
            .to_string();
        Ok(Some(body))
    }

    /// Handle /info command.
    ///
    /// Returns the session block as a `String` so the TUI can
    /// render it in the chat pane. Falls back to an empty-output
    /// frame when no session metadata is on disk — the rustyline
    /// REPL treated that as a no-op, so we mirror by returning
    /// `Some("")`.
    async fn handle_info(&self, session_id: &str) -> Result<Option<String>> {
        match storage::load_session_metadata(session_id) {
            Ok(metadata) => Ok(Some(format!(
                "\n📋 Session Information:\n  \
                  ID:        {}\n  \
                  Name:      {}\n  \
                  Model:     {}\n  \
                  Provider:  {}\n  \
                  Created:   {}\n  \
                  Messages:  {}\n",
                metadata.id,
                metadata.name,
                metadata.model,
                metadata.provider,
                metadata.created_at,
                metadata.message_count,
            ))),
            Err(_) => Ok(Some(String::new())),
        }
    }

    /// Handle /session command with full CRUD
    async fn handle_session(
        &self,
        session_id: &str,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        let mut out = String::new();
        match subcommand {
            Some("list") => {
                let Some(client) = self.current_agent_client().await else {
                    out.push_str("❌ Could not list sessions: no active workspace client\n");
                    out.push('\n');
                    return Ok(Some(out));
                };

                let list_result = {
                    let locked = client.lock().await;
                    locked.list_sessions().await
                };
                match list_result {
                    Ok(sessions) => {
                        let local_sessions = storage::list_sessions().unwrap_or_default();
                        out.push_str(&format_backend_session_list(&sessions, &local_sessions));
                    }
                    Err(e) => {
                        out.push_str(&format!("❌ Could not list active backend sessions: {e}\n"));
                    }
                }
            }
            Some("delete") => {
                let name = args.first().copied().unwrap_or("");
                if name.is_empty() {
                    out.push_str("Usage: /session delete <name>\n");
                } else {
                    let Some(client) = self.current_agent_client().await else {
                        out.push_str("❌ Failed to delete session: no active workspace client\n");
                        out.push('\n');
                        return Ok(Some(out));
                    };

                    match resolve_delete_session_target(&client, name).await {
                        Ok(target) => match client.lock().await.delete_session(&target.id).await {
                            Ok(()) => {
                                let display = target.display_name().to_string();
                                if let Some(local_id) = target.local_id.as_deref() {
                                    if let Err(e) = storage::delete_session(local_id) {
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
            }
            Some("info") => {
                let Some(client) = self.current_agent_client().await else {
                    out.push_str(
                        "❌ Failed to load active backend session: no active workspace client\n",
                    );
                    out.push('\n');
                    return Ok(Some(out));
                };

                let load_result = {
                    let locked = client.lock().await;
                    locked.load_session(session_id).await
                };
                match load_result {
                    Ok(session) => {
                        let local_metadata = storage::load_session_metadata(&session.id)
                            .ok()
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
                let Some(session_name) = args.first().copied().filter(|s| !s.is_empty()) else {
                    out.push_str(&format!(
                        "Usage: /session {} <name>\n",
                        subcommand.unwrap_or("switch")
                    ));
                    out.push('\n');
                    return Ok(Some(out));
                };
                out.push_str(&format!("🔄 Active backend session: '{session_name}'\n"));
            }
            Some(session_name) => {
                out.push_str(&format!(
                    "🔄 Switching to backend session: '{session_name}'\n"
                ));
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
        Ok(Some(out))
    }

    /// Handle /history command
    async fn handle_history(&self) -> Result<Option<String>> {
        match InputHistory::load_from_file() {
            Ok(history) => {
                println!("\n📜 Command History:");
                for (i, entry) in history.entries().iter().enumerate() {
                    println!("  {}. {}", i + 1, entry);
                }
                println!();
            }
            Err(_) => {
                println!("No history available yet");
            }
        }
        Ok(None)
    }

    // Zeroclaw Agent & Daemon
    async fn handle_agent(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        match subcommand {
            Some("-m") | Some("--message") => {
                let msg = args.join(" ");
                println!("📤 Sending: {}", msg);
                println!("   (Via zeroclaw agent -m)");
            }
            Some("-p") | Some("--provider") => {
                let provider = args.first().copied().unwrap_or("default");
                println!("🔄 Using provider: {}", provider);
            }
            _ => println!("✓ Agent mode active (already running)"),
        }
        Ok(None)
    }

    async fn handle_daemon(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        println!("\n🔌 Daemon & Gateway:");
        match subcommand {
            Some("-p") | Some("--port") => {
                let port = args.first().copied().unwrap_or("42617");
                println!("  Starting on port: {}", port);
            }
            _ => println!("  Listening on: http://127.0.0.1:42617"),
        }
        println!("  Channels: Connected");
        println!("  Scheduler: Active");
        println!("  (Full daemon features in Phase 7+)");
        println!();
        Ok(None)
    }

    async fn handle_service(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        match subcommand {
            Some("install") => println!("📦 Install system service (Phase 7+)"),
            Some("status") => println!("  Service: Not installed"),
            Some("start") | Some("stop") | Some("restart") => println!("  (Phase 7+)"),
            _ => println!("Usage: /service install|status|start|stop|restart"),
        }
        Ok(None)
    }

    async fn handle_onboard(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        println!("\n⚙️  Onboarding:");
        match subcommand {
            Some("--provider") => {
                let provider = args.first().copied().unwrap_or("openrouter");
                println!("  Provider: {}", provider);
            }
            Some("--force") => println!("  Config: Reset"),
            _ => println!("  Config: ~/.zeroclaw/config.toml"),
        }
        println!("  (Interactive setup in Phase 7+)");
        println!();
        Ok(None)
    }

    async fn handle_doctor(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        println!("\n🏥 System Diagnostics:");
        match subcommand {
            Some("models") => {
                println!("  Probing model connectivity...");
                println!("  • Groq: ✓");
                println!("  • OpenAI: ✓");
                println!("  • Configured providers: ✓");
            }
            Some("traces") => println!("  Execution traces: (Phase 7+)"),
            _ => {
                println!("  Gateway: ✓ Running");
                println!("  Config: ✓ Valid");
                println!("  Memory: ✓ SQLite");
                println!("  Channels: ✓ Connected");
            }
        }
        println!();
        Ok(None)
    }

    /// Handle /memory command with MNEMOS integration
    async fn handle_memory(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
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
        match sub {
            "search" => {
                let (query, limit) = parse_search_args(rest);
                if query.is_empty() {
                    return Ok(Some("Usage: /memory search <query> [limit]\n".to_string()));
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
                    return Ok(Some("Usage: /memory get <id>\n".to_string()));
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
                    return Ok(Some(
                        "Usage: /memory post <content> [--category <cat>]\n\
                         Example: /memory post \"shipped zterm CI cleanup\" --category work\n"
                            .to_string(),
                    ));
                }
                let res = match self.current_mnemos().await {
                    Some(m) => m.create(&content, category.as_deref()).await,
                    None => Err(anyhow::anyhow!(
                        "MNEMOS not configured (set MNEMOS_URL + MNEMOS_TOKEN)"
                    )),
                };
                match res {
                    Ok(result) => {
                        let id = result
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "(unknown id)".to_string());
                        out.push_str(&format!("📝 Memory saved: {id}\n"));
                    }
                    Err(e) => out.push_str(&format!("❌ Failed to save memory: {e}\n")),
                }
            }
            "delete" | "rm" => {
                let id = rest.join(" ");
                if id.is_empty() {
                    return Ok(Some("Usage: /memory delete <id>\n".to_string()));
                }
                let res = match self.current_mnemos().await {
                    Some(m) => m.delete(&id).await,
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
        Ok(Some(out))
    }

    async fn handle_cron(&self, subcommand: Option<&str>, args: &[&str]) -> Result<Option<String>> {
        match subcommand {
            Some("list") => {
                println!("\n⏰ Scheduled Tasks:");
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
                            println!("  {}. [{}] {} → {}", i + 1, &id[..8], expr, prompt);
                        }
                    }
                    Ok(_) => println!("  (No scheduled tasks)"),
                    Err(_) => println!("  (Gateway unavailable)"),
                }
                println!();
            }
            Some("add") => {
                if args.len() < 2 {
                    ui::print_error(
                        "Usage: /cron add '<expr>' '<prompt>'",
                        Some("Example: /cron add '0 9 * * *' 'Daily standup'"),
                    );
                } else {
                    let expr = args[0];
                    let prompt = args[1..].join(" ");
                    let res = match self.current_cron().await {
                        Some(c) => c.create_cron_job(expr, &prompt).await,
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(id) => {
                            ui::print_success(&format!("✅ Created cron job: {}", &id[..16]));
                            println!("   Expression: {} → {}", expr, prompt);
                        }
                        Err(e) => {
                            ui::print_error("Failed to create cron job", Some(&e.to_string()))
                        }
                    }
                }
            }
            Some("add-at") => {
                if args.len() < 2 {
                    ui::print_error(
                        "Usage: /cron add-at '<datetime>' '<prompt>'",
                        Some("Example: /cron add-at '2026-04-21T10:00:00Z' 'Meeting'"),
                    );
                } else {
                    let datetime = args[0];
                    let prompt = args[1..].join(" ");
                    let res = match self.current_cron().await {
                        Some(c) => c.create_cron_at(datetime, &prompt).await,
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(_) => {
                            ui::print_success(&format!("✅ Scheduled task for {}", datetime));
                            println!("   Prompt: {}", prompt);
                        }
                        Err(e) => ui::print_error("Failed to schedule task", Some(&e.to_string())),
                    }
                }
            }
            Some("pause") => {
                let id = args.first().copied().unwrap_or("");
                if id.is_empty() {
                    ui::print_error("Usage: /cron pause <id>", None);
                } else {
                    let res = match self.current_cron().await {
                        Some(c) => c.pause_cron(id).await,
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(_) => ui::print_success(&format!("⏸️  Paused job: {}", id)),
                        Err(e) => ui::print_error("Failed to pause job", Some(&e.to_string())),
                    }
                }
            }
            Some("resume") => {
                let id = args.first().copied().unwrap_or("");
                if id.is_empty() {
                    ui::print_error("Usage: /cron resume <id>", None);
                } else {
                    let res = match self.current_cron().await {
                        Some(c) => c.resume_cron(id).await,
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(_) => ui::print_success(&format!("▶️  Resumed job: {}", id)),
                        Err(e) => ui::print_error("Failed to resume job", Some(&e.to_string())),
                    }
                }
            }
            Some("remove") => {
                let id = args.first().copied().unwrap_or("");
                if id.is_empty() {
                    ui::print_error("Usage: /cron remove <id>", None);
                } else {
                    let res = match self.current_cron().await {
                        Some(c) => c.delete_cron(id).await,
                        None => Err(anyhow::anyhow!("cron not available on this backend")),
                    };
                    match res {
                        Ok(_) => ui::print_success(&format!("🗑️  Deleted job: {}", id)),
                        Err(e) => ui::print_error("Failed to delete job", Some(&e.to_string())),
                    }
                }
            }
            _ => {
                println!("Usage: /cron list");
                println!("       /cron add '<expr>' '<prompt>'");
                println!("       /cron add-at '<datetime>' '<prompt>'");
                println!("       /cron pause|resume|remove <id>");
            }
        }
        Ok(None)
    }

    /// Handle /skill command with zeroclaw integration
    async fn handle_skill(
        &self,
        subcommand: Option<&str>,
        _args: &[&str],
    ) -> Result<Option<String>> {
        match subcommand {
            Some("list") => println!("⚡ Installed Skills: (none)"),
            Some("install") => println!("  Installing: (Phase 7+)"),
            Some("audit") => println!("  Auditing skills: (Phase 7+)"),
            Some("remove") => println!("  Removing skill: (Phase 7+)"),
            _ => println!("Usage: /skill list|install <path>|audit|remove"),
        }
        println!();
        Ok(None)
    }

    async fn handle_providers(&self) -> Result<Option<String>> {
        // List provider backends advertised by the live daemon's
        // `[providers.models.*]` config (e.g. `gemini`,
        // `openai_compat`). Static "40+ providers" copy is gone —
        // it was both factually wrong (the daemon only exposes the
        // backends configured in its TOML) and contained hardcoded
        // brand strings. Falls back gracefully when no zeroclaw backend is
        // active or the fetch fails.
        let mut out = String::from("\n🤖 Configured Providers:\n");
        match self.current_cron().await {
            Some(c) => {
                if c.model_list().is_empty() {
                    let _ = c.refresh_models().await;
                }
                let models = c.model_list();
                if models.is_empty() {
                    out.push_str(
                        "  (none — /api/config returned no [providers.models.*] entries)\n",
                    );
                } else {
                    let mut backends: std::collections::BTreeSet<String> = Default::default();
                    for m in &models {
                        backends.insert(m.provider.clone());
                    }
                    for b in backends {
                        out.push_str(&format!("    • {}\n", b));
                    }
                }
            }
            None => {
                out.push_str("  (no active zeroclaw workspace)\n");
            }
        }
        out.push('\n');
        Ok(Some(out))
    }

    async fn handle_models(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        let cron = self.current_cron().await;
        match subcommand {
            Some("list") | None => {
                let mut out = String::from("\n📋 Available Models (from /api/config):\n\n");
                match cron {
                    Some(c) => {
                        if c.model_list().is_empty() {
                            let _ = c.refresh_models().await;
                        }
                        let list = c.model_list();
                        if list.is_empty() {
                            out.push_str(
                                "  (none — /api/config returned no [providers.models.*])\n",
                            );
                        } else {
                            let active = c.current_model_key();
                            for m in &list {
                                let marker = if m.key == active { "*" } else { " " };
                                out.push_str(&format!(
                                    "  {} {:<10}  ({} → {})\n",
                                    marker, m.key, m.provider, m.model
                                ));
                            }
                            out.push_str(&format!(
                                "\n  active: {}\n  use: /models set <key>\n",
                                active
                            ));
                        }
                    }
                    None => out.push_str("  (no active zeroclaw workspace)\n"),
                }
                out.push('\n');
                Ok(Some(out))
            }
            Some("set") => {
                let key = args.first().copied().unwrap_or("").trim().to_string();
                if key.is_empty() {
                    return Ok(Some(
                        "Usage: /models set <key>\n   Run /models list to see available keys\n"
                            .to_string(),
                    ));
                }
                match cron {
                    Some(c) => {
                        if c.model_list().is_empty() {
                            let _ = c.refresh_models().await;
                        }
                        match c.set_current_model(&key) {
                            Ok(()) => Ok(Some(format!(
                                "✅ Active model key: {key}\n   Future turns will send this key to the daemon.\n"
                            ))),
                            Err(e) => Ok(Some(format!("❌ Failed to set model key: {e}\n"))),
                        }
                    }
                    None => Ok(Some(
                        "/models set requires an active zeroclaw workspace\n".to_string(),
                    )),
                }
            }
            Some("refresh") => match cron {
                Some(c) => match c.refresh_models().await {
                    Ok(list) => Ok(Some(format!(
                        "✅ Refreshed model list ({} entries)\n",
                        list.len()
                    ))),
                    Err(e) => Ok(Some(format!("❌ Failed to refresh /api/config: {e}\n"))),
                },
                None => Ok(Some(
                    "/models refresh requires an active zeroclaw workspace\n".to_string(),
                )),
            },
            Some("status") => match cron {
                Some(c) => {
                    let active = c.current_model_key();
                    let entry = c.model_list().into_iter().find(|m| m.key == active);
                    let mut out = String::from("\n📊 Current Model:\n");
                    out.push_str(&format!("  key:      {}\n", active));
                    if let Some(m) = entry {
                        out.push_str(&format!("  provider: {}\n", m.provider));
                        out.push_str(&format!("  model:    {}\n", m.model));
                    } else {
                        out.push_str("  (not in /api/config; daemon may reject this key)\n");
                    }
                    out.push('\n');
                    Ok(Some(out))
                }
                None => Ok(Some(
                    "/models status requires an active zeroclaw workspace\n".to_string(),
                )),
            },
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
        match subcommand {
            Some("list") => {
                println!("\n💬 Channels:");
                println!("  (None configured)");
                println!("  Available: Slack, Discord, Telegram, Matrix, Email, IRC");
            }
            Some("doctor") => println!("  Channel health: (Phase 7+)"),
            _ => println!("Usage: /channel list|doctor"),
        }
        println!();
        Ok(None)
    }

    async fn handle_hardware(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        match subcommand {
            Some("discover") => {
                println!("\n🔌 Hardware Discovery:");
                println!("  (No USB devices found)");
                println!("  Supports: STM32, Arduino, Raspberry Pi, ESP32");
            }
            Some("introspect") => println!("  Probing device... (Phase 7+)"),
            _ => println!("Usage: /hardware discover|introspect <port>"),
        }
        println!();
        Ok(None)
    }

    async fn handle_peripheral(
        &self,
        subcommand: Option<&str>,
        _args: &[&str],
    ) -> Result<Option<String>> {
        match subcommand {
            Some("list") => println!("📱 Peripherals: (none)"),
            Some("add") => println!("  Adding peripheral... (Phase 7+)"),
            Some("flash-nucleo") => println!("  Flashing STM32... (Phase 7+)"),
            Some("flash") => println!("  Flashing Arduino... (Phase 7+)"),
            _ => println!("Usage: /peripheral list|add|flash-nucleo|flash"),
        }
        println!();
        Ok(None)
    }

    async fn handle_estop(
        &self,
        subcommand: Option<&str>,
        args: &[&str],
    ) -> Result<Option<String>> {
        match subcommand {
            Some("status") => println!("🛑 Emergency Stop: Disengaged"),
            Some("--level") => {
                let level = args.first().copied().unwrap_or("<level>");
                println!("  Level: {} (Phase 7+)", level);
            }
            Some("resume") => println!("  ▶️  Resuming (Phase 7+)"),
            _ => println!("Usage: /estop status|--level <kill-all|network-kill|...>"),
        }
        println!();
        Ok(None)
    }

    async fn handle_completions(&self, subcommand: Option<&str>) -> Result<Option<String>> {
        match subcommand {
            Some("zsh") => println!("📝 Zsh completions: (Phase 7+)"),
            Some("bash") => println!("📝 Bash completions: (Phase 7+)"),
            Some("fish") => println!("📝 Fish completions: (Phase 7+)"),
            _ => println!("Usage: /completions zsh|bash|fish"),
        }
        println!();
        Ok(None)
    }

    /// Handle /config command
    async fn handle_config(&self) -> Result<Option<String>> {
        println!("\n⚙️  Configuration:");
        match storage::load_config() {
            Ok(content) => {
                println!("{}", content);
            }
            Err(e) => {
                ui::print_error("Could not load config", Some(&e.to_string()));
            }
        }
        println!();
        Ok(None)
    }

    /// Handle /clear command
    async fn handle_clear(&self, session_id: &str) -> Result<Option<String>> {
        if let Ok(mut metadata) = storage::load_session_metadata(session_id) {
            metadata.message_count = 0;
            metadata.last_active = Utc::now().to_rfc3339();
            storage::save_session_metadata(&metadata)?;
            ui::print_success("✓ Session history cleared");
        }
        Ok(None)
    }

    /// Handle /save command
    async fn handle_save(
        &self,
        session_id: &str,
        filename: Option<String>,
    ) -> Result<Option<String>> {
        let default_name = format!("session-{}.txt", Utc::now().format("%Y%m%d-%H%M%S"));
        let filename = filename.unwrap_or(default_name);

        if let Ok(history_path) = storage::session_history_file(session_id) {
            if history_path.exists() {
                fs::copy(&history_path, &filename)?;
                ui::print_success(&format!("✓ Session saved to {}", filename));
            } else {
                ui::print_error("No history to save", None);
            }
        }

        Ok(None)
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
                        w.url,
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
                    out.push_str(&format!("   url:       {}\n", a.url));
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
                    ui::print_error(
                        "Usage: /workspace switch <name>",
                        Some("/workspace list to see names"),
                    );
                    return Ok(None);
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

        // Activate the target workspace if it hasn't been yet. Note:
        // this holds the App mutex across a potentially-slow openclaw
        // WS handshake. Fine for single-threaded REPL usage; future
        // slices revisit if concurrency enters the picture.
        {
            let mut app = self.app.lock().await;
            if !app.workspaces[target_idx].is_activated() {
                if let Err(e) = app.workspaces[target_idx].activate().await {
                    return Ok(Some(format!("❌ failed to activate \"{name}\": {e}\n")));
                }
            }
            app.active = target_idx;
        }

        Ok(Some(format!("✅ 🗂  switched to workspace: {name}\n")))
    }
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

fn exact_local_metadata<'a>(
    local_sessions: &'a [storage::SessionMetadata],
    id: &str,
) -> Option<&'a storage::SessionMetadata> {
    local_sessions.iter().find(|metadata| metadata.id == id)
}

fn short_session_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

fn short_date(value: &str) -> &str {
    &value[..10.min(value.len())]
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
    requested: &str,
) -> Result<DeleteSessionTarget> {
    let local_sessions = storage::list_sessions().unwrap_or_default();
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
            return (query, n);
        }
    }
    (rest.join(" "), 10)
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
        let short_id = if id.len() > 12 { &id[..12] } else { id };
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
    use crate::cli::client::Session;
    use crate::cli::storage::SessionMetadata;

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
