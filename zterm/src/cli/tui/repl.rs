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
use crate::cli::tui::delighters;
use crate::cli::tui::tv_ui::sanitize_terminal_text;
use crate::cli::ui::{self, StatusBar};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const LEGACY_MUTATING_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacyMutationFenceOwner {
    key: String,
    dispatch_id: String,
}

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

impl ReplSessionBinding {
    fn from_session(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            name: session.name.clone(),
        }
    }
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
        let workspace_sessions = {
            let app_guard = app
                .try_lock()
                .map_err(|_| anyhow::anyhow!("could not seed active workspace session binding"))?;
            let mut sessions = HashMap::new();
            if let Some(workspace) = app_guard.active_workspace() {
                sessions.insert(
                    workspace.config.name.clone(),
                    ReplSessionBinding::from_session(&session),
                );
            }
            sessions
        };

        Ok(Self {
            app,
            session,
            workspace_sessions,
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

    async fn current_mutation_fence_key(&self) -> Result<String> {
        let app = self.app.lock().await;
        let workspace = app
            .active_workspace()
            .ok_or_else(|| anyhow::anyhow!("no active workspace"))?;
        Ok(legacy_mutation_fence_workspace_key(
            &workspace.config.name,
            workspace.config.id.as_deref(),
        ))
    }

    async fn active_mutation_fence_reason(&self) -> Option<String> {
        let key = self.current_mutation_fence_key().await.ok()?;
        match delighters::mutation_fence_for_workspace(&key) {
            Ok(Some(fence)) => Some(fence.reason),
            Ok(None) => None,
            Err(e) => Some(format!(
                "could not read zterm mutation-fence state: {e}; run /resync --force only after manual reconciliation"
            )),
        }
    }

    async fn mutation_fence_block_output(&self, input: &str) -> Option<String> {
        let reason = self.active_mutation_fence_reason().await?;
        if legacy_mutation_fence_allows_input(input) {
            return None;
        }
        Some(format!(
            "[blocked] mutation outcome is unknown; run /resync to inspect state, or /resync --force after manual reconciliation. Last status: {reason}\n"
        ))
    }

    async fn persist_mutation_fence_replacing(
        &self,
        reason: &str,
        old_owner: Option<&LegacyMutationFenceOwner>,
    ) -> String {
        let key = match self.current_mutation_fence_key().await {
            Ok(key) => key,
            Err(e) => return format!("{reason}; failed to identify active workspace: {e}"),
        };
        let dispatch_id = old_owner
            .map(|owner| owner.dispatch_id.as_str())
            .unwrap_or_default();
        let fence = legacy_mutation_fence_state_with_dispatch(
            &legacy_mutation_fence_command_from_reason(reason),
            reason,
            dispatch_id,
        );
        let result = if let Some(owner) = old_owner {
            delighters::replace_mutation_fence_for_workspace_if_dispatch(
                &owner.key,
                &owner.dispatch_id,
                &key,
                fence,
            )
        } else {
            delighters::set_mutation_fence_for_workspace(&key, fence).map(|_| true)
        };
        match result {
            Ok(true) => reason.to_string(),
            Ok(false) => format!(
                "{reason}; failed to persist fence: durable write-ahead fence is no longer owned by this dispatch"
            ),
            Err(e) => format!("{reason}; failed to persist fence: {e}"),
        }
    }

    async fn persist_mutation_fence(&self, reason: &str) -> String {
        self.persist_mutation_fence_replacing(reason, None).await
    }

    async fn write_ahead_mutation_fence_for_dispatch(
        &self,
        cmdline: &str,
    ) -> Result<LegacyMutationFenceOwner> {
        let key = self.current_mutation_fence_key().await?;
        let reason = legacy_write_ahead_mutation_fence_message(cmdline);
        let dispatch_id = delighters::new_mutation_fence_dispatch_id();
        let fence = legacy_mutation_fence_state_with_dispatch(cmdline, &reason, &dispatch_id);
        match delighters::acquire_mutation_fence_for_workspace(&key, fence)? {
            Ok(_) => Ok(LegacyMutationFenceOwner { key, dispatch_id }),
            Err(existing) => Err(anyhow::anyhow!(
                "mutation fence already active for this workspace: {}",
                existing.reason
            )),
        }
    }

    async fn handle_legacy_resync(&mut self, input: &str) -> Result<Option<String>> {
        let Ok(tokens) = tokenize_slash_command(input) else {
            return Ok(None);
        };
        if !matches!(
            tokens.first().map(String::as_str),
            Some("/resync" | "/sync")
        ) {
            return Ok(None);
        }
        let force = tokens.len() == 2
            && matches!(tokens.get(1).map(String::as_str), Some("--force" | "force"));
        if force {
            let key = self.current_mutation_fence_key().await?;
            delighters::clear_mutation_fence_for_workspace(&key)?;
            return Ok(Some(
                "[sync] mutation fence cleared by explicit /resync --force\n".to_string(),
            ));
        }
        if tokens.len() > 1 {
            return Ok(Some("[error] usage: /resync [--force]\n".to_string()));
        }

        let workspace = self
            .current_workspace_name()
            .await
            .unwrap_or_else(|_| "<unknown>".to_string());
        Ok(Some(format!(
            "[sync] refreshed workspace `{workspace}`; mutation fence remains until /resync --force\n"
        )))
    }

    fn remember_active_workspace_session(&mut self, workspace_name: String, session: &Session) {
        self.workspace_sessions
            .insert(workspace_name, ReplSessionBinding::from_session(session));
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
            LegacySessionResolution::Existing(session) => {
                let load_result = {
                    let locked = active_client.lock().await;
                    locked.load_session(&session.id).await
                };
                load_result.map_err(|e| {
                    anyhow::anyhow!(
                        "listed session '{}' matched '{}', but could not be loaded: {e}; refusing to create a replacement session",
                        session.id,
                        target
                    )
                })
            }
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

    async fn turn_session_id_for_active_workspace(&mut self) -> Result<String> {
        let workspace_name = self.current_workspace_name().await?;
        if let Some(binding) = self.workspace_sessions.get(&workspace_name).cloned() {
            self.session.id = binding.id.clone();
            self.session.name = binding.name;
            self.status_bar.set_session(self.session.name.clone());
            return Ok(binding.id);
        }

        let active_client = self.resolve_active_client().await?;
        let session = active_client
            .lock()
            .await
            .create_session(&self.fallback_session_name)
            .await?;
        let scope = self.current_storage_scope().await?;
        if let Err(e) = save_legacy_session_metadata(&scope, &session) {
            warn!(
                "could not save local metadata for newly created session {}: {}",
                session.id, e
            );
        }
        let session_id = session.id.clone();
        self.session = session.clone();
        self.status_bar.set_session(self.session.name.clone());
        self.remember_active_workspace_session(workspace_name, &session);
        Ok(session_id)
    }

    async fn verify_session_for_active_workspace(&mut self) -> Result<String> {
        let workspace_name = self.current_workspace_name().await?;
        let binding = self
            .workspace_sessions
            .get(&workspace_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no active session binding for workspace"))?;
        let session = self.load_active_workspace_session(&binding.id).await?;
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
            LegacySessionResolution::Existing(session) => {
                let load_result = {
                    let locked = active_client.lock().await;
                    locked.load_session(&session.id).await
                };
                load_result.map_err(|e| {
                    anyhow::anyhow!(
                        "listed session '{}' matched '{}', but could not be loaded: {e}; refusing to create a replacement session",
                        session.id,
                        target
                    )
                })?
            }
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

            if let Some(output) = self.mutation_fence_block_output(&input).await {
                print!("{output}");
                continue;
            }

            // Handle commands
            if input.starts_with('/') {
                match self.handle_slash_command(&input).await {
                    Ok(Some(text)) => {
                        // Handlers that were refactored to return
                        // their output as a String (so the Turbo
                        // Vision UI can render them) — print it
                        // here so the rustyline REPL UX is
                        // unchanged.
                        let safe_text = sanitize_legacy_slash_output(&text);
                        print!("{}", safe_text);
                        if !safe_text.ends_with('\n') {
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

            let session_id = match self.turn_session_id_for_active_workspace().await {
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
                Ok(scope) => scope,
                Err(e) => {
                    ui::print_error(
                        "could not resolve transcript scope; turn not submitted",
                        Some(&format!("session {session_id}: {e}")),
                    );
                    continue;
                }
            };
            if let Err(e) =
                storage::ensure_scoped_session_history_complete(&transcript_scope, &session_id)
            {
                ui::print_error("turn not submitted", Some(&e.to_string()));
                continue;
            }
            let pending_marker_id =
                match mark_repl_transcript_pending(&transcript_scope, &session_id) {
                    Ok(marker_id) => marker_id,
                    Err(e) => {
                        ui::print_error(
                            "could not persist pending transcript marker; turn not submitted",
                            Some(&e.to_string()),
                        );
                        continue;
                    }
                };
            if let Err(e) =
                append_repl_transcript_entry(&transcript_scope, &session_id, "user", &input)
            {
                let clear_error = clear_repl_transcript_pending_marker(
                    &transcript_scope,
                    &session_id,
                    &pending_marker_id,
                )
                .err();
                let mut detail = e.to_string();
                if let Some(clear_error) = clear_error {
                    detail.push_str(&format!(
                        "; additionally failed to clear pending transcript marker: {clear_error}"
                    ));
                }
                ui::print_error(
                    "could not persist user transcript; turn not submitted",
                    Some(&detail),
                );
                continue;
            }
            let turn_res = {
                let mut guard = active_client.lock().await;
                guard.submit_turn(&session_id, &input).await
            };
            match turn_res {
                Ok(response) => {
                    let mut transcript_incomplete = false;
                    if let Err(e) = append_repl_transcript_entry(
                        &transcript_scope,
                        &session_id,
                        "assistant",
                        &response,
                    ) {
                        transcript_incomplete = true;
                        surface_repl_transcript_incomplete(&transcript_scope, &session_id, &e);
                    }
                    if !transcript_incomplete {
                        if let Err(e) = clear_repl_transcript_pending_marker(
                            &transcript_scope,
                            &session_id,
                            &pending_marker_id,
                        ) {
                            ui::print_error(
                                "terminal transcript persisted, but pending marker remains",
                                Some(&e.to_string()),
                            );
                        }
                    }
                    // Response already printed by streaming handler
                    // Update session metadata
                    if let Err(e) = self.update_session_metadata().await {
                        eprintln!("⚠️  Could not update session metadata: {}", e);
                    }
                }
                Err(e) => {
                    let error_text = e.to_string();
                    let mut transcript_incomplete = false;
                    if let Err(append_error) = append_repl_transcript_entry(
                        &transcript_scope,
                        &session_id,
                        "error",
                        &error_text,
                    ) {
                        transcript_incomplete = true;
                        surface_repl_transcript_incomplete(
                            &transcript_scope,
                            &session_id,
                            &append_error,
                        );
                    }
                    if repl_submit_error_requires_incomplete_transcript(&error_text) {
                        transcript_incomplete = true;
                        surface_repl_transcript_incomplete_reason(
                            &transcript_scope,
                            &session_id,
                            &error_text,
                        );
                    }
                    if !transcript_incomplete {
                        if let Err(e) = clear_repl_transcript_pending_marker(
                            &transcript_scope,
                            &session_id,
                            &pending_marker_id,
                        ) {
                            ui::print_error(
                                "terminal transcript persisted, but pending marker remains",
                                Some(&e.to_string()),
                            );
                        }
                    }
                    eprintln!("\n❌ Error: {}", e);
                }
            }
        }

        // Save history on exit
        self.history.save_to_file()?;
        Ok(())
    }

    async fn handle_slash_command(&mut self, input: &str) -> Result<Option<String>> {
        if let Some(output) = self.handle_legacy_resync(input).await? {
            return Ok(Some(output));
        }
        if let Some(output) = self.mutation_fence_block_output(input).await {
            return Ok(Some(output));
        }

        if let Some(action) = legacy_session_action(input) {
            let mutation_fence_owner =
                match self.write_ahead_mutation_fence_for_dispatch(input).await {
                    Ok(owner) => owner,
                    Err(e) => {
                        return Ok(Some(format!(
                            "[blocked] {}\n",
                            legacy_write_ahead_persist_failure_message(input, &e)
                        )));
                    }
                };
            match tokio::time::timeout(
                LEGACY_MUTATING_COMMAND_TIMEOUT,
                self.apply_legacy_session_action(action),
            )
            .await
            {
                Ok(Ok(())) => {
                    if let Some(reason) =
                        clear_legacy_write_ahead_mutation_fence(&mutation_fence_owner)
                    {
                        return Ok(Some(format!("[blocked] {reason}\n")));
                    }
                }
                Ok(Err(e)) => {
                    let reason = legacy_mutating_unknown_outcome_message(input, &e.to_string());
                    let reason = self
                        .persist_mutation_fence_replacing(&reason, Some(&mutation_fence_owner))
                        .await;
                    return Ok(Some(format!("[blocked] {reason}\n")));
                }
                Err(_) => {
                    let reason = legacy_mutating_timeout_message(input);
                    let reason = self
                        .persist_mutation_fence_replacing(&reason, Some(&mutation_fence_owner))
                        .await;
                    return Ok(Some(format!("[blocked] {reason}\n")));
                }
            }
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
            self.verify_session_for_active_workspace().await?
        } else {
            self.session.id.clone()
        };

        let mutation_fence_owner = if legacy_slash_command_may_mutate_state(input) {
            match self.write_ahead_mutation_fence_for_dispatch(input).await {
                Ok(owner) => Some(owner),
                Err(e) => {
                    return Ok(Some(format!(
                        "[blocked] {}\n",
                        legacy_write_ahead_persist_failure_message(input, &e)
                    )));
                }
            }
        } else {
            None
        };

        let command_result = if mutation_fence_owner.is_some() {
            match tokio::time::timeout(
                LEGACY_MUTATING_COMMAND_TIMEOUT,
                self.command_handler
                    .handle_with_outcome(input, &command_session_id),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => {
                    let reason = legacy_mutating_unknown_outcome_message(input, &e.to_string());
                    let reason = self
                        .persist_mutation_fence_replacing(&reason, mutation_fence_owner.as_ref())
                        .await;
                    return Ok(Some(format!("[blocked] {reason}\n")));
                }
                Err(_) => {
                    let reason = legacy_mutating_timeout_message(input);
                    let reason = self
                        .persist_mutation_fence_replacing(&reason, mutation_fence_owner.as_ref())
                        .await;
                    return Ok(Some(format!("[blocked] {reason}\n")));
                }
            }
        } else {
            self.command_handler
                .handle_with_outcome(input, &command_session_id)
                .await?
        };
        let mutation_outcome_unknown = command_result.mutation_outcome_unknown;
        let result = command_result.output;

        if preflight == CommandSessionPreflight::AfterWorkspaceSwitch {
            let workspace_after_dispatch = self.current_workspace_name().await.ok();
            let successful_switch = matches!(
                (workspace_switch_target.as_deref(), workspace_after_dispatch.as_deref()),
                (Some(target), Some(active)) if target == active
            );
            if successful_switch || workspace_after_dispatch != workspace_before_dispatch {
                if let Err(e) = self.ensure_session_for_active_workspace().await {
                    let reason = legacy_workspace_switch_session_setup_failure_message(input, &e);
                    let reason = self
                        .persist_mutation_fence_replacing(&reason, mutation_fence_owner.as_ref())
                        .await;
                    return Ok(Some(legacy_output_with_blocked(
                        result.clone().unwrap_or_default(),
                        &reason,
                    )));
                }
            }
        }

        if mutation_outcome_unknown {
            let rendered = result.as_deref().unwrap_or_default();
            let reason = legacy_mutating_unknown_outcome_message(input, rendered);
            let reason = self
                .persist_mutation_fence_replacing(&reason, mutation_fence_owner.as_ref())
                .await;
            return Ok(Some(legacy_output_with_blocked(
                result.unwrap_or_default(),
                &reason,
            )));
        }

        if let Some(owner) = mutation_fence_owner {
            if let Some(reason) = clear_legacy_write_ahead_mutation_fence(&owner) {
                return Ok(Some(legacy_output_with_blocked(
                    result.unwrap_or_default(),
                    &reason,
                )));
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
        "/info" | "/status" => CommandSessionPreflight::BeforeDispatch,
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

fn legacy_mutation_fence_allows_input(input: &str) -> bool {
    super::mutation_fence_allows_recovery_input(input)
}

fn legacy_slash_command_may_mutate_state(cmdline: &str) -> bool {
    let tokens = match tokenize_slash_command(cmdline) {
        Ok(tokens) => tokens,
        Err(_) => return false,
    };
    let command = tokens.first().map(String::as_str);
    let subcommand = tokens.get(1).map(String::as_str);
    match (command, subcommand) {
        (Some("/clear" | "/save"), _) => true,
        (Some("/models" | "/model"), Some("set")) => true,
        (Some("/workspace" | "/workspaces"), Some("switch")) => true,
        (Some("/memory"), Some("post" | "add" | "delete" | "rm")) => true,
        (Some("/cron"), Some("add" | "add-at" | "pause" | "resume" | "delete" | "remove")) => true,
        (Some("/session"), Some("delete" | "switch" | "create")) => true,
        (Some("/session"), Some("list" | "info")) => false,
        (Some("/session"), Some(_)) => true,
        _ => false,
    }
}

fn legacy_mutation_fence_workspace_key(workspace: &str, workspace_id: Option<&str>) -> String {
    match workspace_id {
        Some(id) if !id.trim().is_empty() => format!("id:{}", id.trim()),
        _ => format!("name:{workspace}"),
    }
}

fn legacy_mutation_fence_now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn legacy_mutation_fence_state(command: &str, reason: &str) -> delighters::MutationFenceState {
    legacy_mutation_fence_state_with_dispatch(command, reason, "")
}

fn legacy_mutation_fence_state_with_dispatch(
    command: &str,
    reason: &str,
    dispatch_id: &str,
) -> delighters::MutationFenceState {
    delighters::MutationFenceState {
        command: sanitize_terminal_text(command).replace('`', "'"),
        reason: reason.to_string(),
        created_at_unix: legacy_mutation_fence_now_unix(),
        dispatch_id: dispatch_id.to_string(),
    }
}

fn legacy_write_ahead_mutation_fence_message(cmdline: &str) -> String {
    format!(
        "mutating slash command dispatched for `{}`; backend outcome is pending. Run /resync to inspect state, or /resync --force to clear this fence after manual reconciliation.",
        sanitize_terminal_text(cmdline).replace('`', "'")
    )
}

fn legacy_write_ahead_persist_failure_message(cmdline: &str, error: &anyhow::Error) -> String {
    format!(
        "could not persist durable mutation fence for `{}`; command not dispatched: {}",
        sanitize_terminal_text(cmdline).replace('`', "'"),
        sanitize_terminal_text(&error.to_string())
    )
}

fn clear_legacy_write_ahead_mutation_fence(owner: &LegacyMutationFenceOwner) -> Option<String> {
    match delighters::clear_mutation_fence_for_workspace_if_dispatch(
        &owner.key,
        &owner.dispatch_id,
    ) {
        Ok(true) => None,
        Ok(false) => Some(
            "mutating slash command completed, but zterm did not own the durable write-ahead mutation fence; run /resync --force after manual reconciliation"
                .to_string(),
        ),
        Err(e) => Some(format!(
            "mutating slash command completed, but zterm could not clear the durable write-ahead mutation fence: {e}; run /resync --force after manual reconciliation"
        )),
    }
}

fn legacy_output_with_blocked(mut output: String, reason: &str) -> String {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&format!("[blocked] {reason}\n"));
    output
}

fn legacy_mutating_timeout_message(cmdline: &str) -> String {
    format!(
        "slash command outcome unknown for `{}` after {:?}; the backend may still have applied the mutation. Run /resync to inspect state, or /resync --force to clear this fence after manual reconciliation.",
        sanitize_terminal_text(cmdline).replace('`', "'"),
        LEGACY_MUTATING_COMMAND_TIMEOUT
    )
}

fn legacy_mutating_unknown_outcome_message(cmdline: &str, rendered_text: &str) -> String {
    format!(
        "{} Backend/client returned an unknown mutating outcome: {}",
        legacy_mutating_timeout_message(cmdline),
        sanitize_terminal_text(rendered_text.trim())
    )
}

fn legacy_workspace_switch_session_setup_failure_message(
    cmdline: &str,
    error: &anyhow::Error,
) -> String {
    format!(
        "slash command outcome unknown for `{}`; workspace switch was applied, but session setup failed: {}. Run /resync to inspect state, or /resync --force to clear this fence after manual reconciliation.",
        sanitize_terminal_text(cmdline).replace('`', "'"),
        sanitize_terminal_text(&error.to_string())
    )
}

fn legacy_mutation_fence_command_from_reason(reason: &str) -> String {
    let Some(start) = reason.find(" for `") else {
        return String::new();
    };
    let rest = &reason[start + " for `".len()..];
    let Some(end) = rest.find('`') else {
        return String::new();
    };
    rest[..end].to_string()
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

fn append_repl_transcript_entry(
    scope: &storage::LocalWorkspaceScope,
    session_id: &str,
    role: &str,
    content: &str,
) -> Result<()> {
    storage::append_scoped_session_history(scope, session_id, role, content).map_err(|e| {
        anyhow::anyhow!("could not append {role} transcript entry for session {session_id}: {e}")
    })
}

fn mark_repl_transcript_pending(
    scope: &storage::LocalWorkspaceScope,
    session_id: &str,
) -> Result<String> {
    let marker_id = format!("turn-{}", uuid::Uuid::new_v4());
    storage::mark_scoped_session_history_pending_turn(
        scope,
        session_id,
        &marker_id,
        "turn submitted to backend; terminal transcript entry pending",
    )
    .map_err(|e| anyhow::anyhow!("could not persist pending transcript marker: {e}"))?;
    Ok(marker_id)
}

fn clear_repl_transcript_pending_marker(
    scope: &storage::LocalWorkspaceScope,
    session_id: &str,
    marker_id: &str,
) -> Result<()> {
    match storage::clear_scoped_session_history_pending_turn_marker(scope, session_id, marker_id) {
        Ok(true) => Ok(()),
        Ok(false) => {
            let reason = format!(
                "pending transcript marker {marker_id} was missing before turn completion; transcript may have been cleared concurrently"
            );
            surface_repl_transcript_incomplete_reason(scope, session_id, &reason);
            Err(anyhow::anyhow!(
                "{reason}; transcript marked incomplete and /save is disabled until /clear"
            ))
        }
        Err(e) => Err(anyhow::anyhow!(
            "could not clear pending transcript marker: {e}"
        )),
    }
}

fn surface_repl_transcript_incomplete(
    scope: &storage::LocalWorkspaceScope,
    session_id: &str,
    append_error: &anyhow::Error,
) {
    surface_repl_transcript_incomplete_reason(scope, session_id, &append_error.to_string());
}

fn surface_repl_transcript_incomplete_reason(
    scope: &storage::LocalWorkspaceScope,
    session_id: &str,
    reason: &str,
) {
    warn!("{reason}");
    match storage::mark_scoped_session_history_incomplete(scope, session_id, reason) {
        Ok(()) => ui::print_error(
            "transcript persistence failed; /save disabled until /clear",
            Some(reason),
        ),
        Err(marker_error) => ui::print_error(
            "transcript persistence failed and incomplete marker could not be written",
            Some(&format!("{reason}; marker error: {marker_error}")),
        ),
    }
}

fn repl_turn_collection_failure_requires_incomplete_transcript(message: &str) -> bool {
    message.contains("accepted assistant turn exceeded cap")
        || message.contains("buffered runId-less assistant messages exceeded cap")
}

fn repl_submit_error_requires_incomplete_transcript(message: &str) -> bool {
    repl_turn_collection_failure_requires_incomplete_transcript(message) || !message.is_empty()
}

fn sanitize_legacy_slash_output(output: &str) -> String {
    sanitize_terminal_text(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::agent::{AgentClient, StreamSink};
    use crate::cli::client::{Config, Model, Provider, ZeroclawClient};
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
    fn legacy_slash_output_sanitizes_terminal_controls() {
        let raw = "session \u{1b}]52;c;owned\u{7}\nmodel \u{1b}[31mred";

        let safe = sanitize_legacy_slash_output(raw);

        assert!(!safe.contains('\u{1b}'));
        assert!(!safe.contains('\u{7}'));
        assert!(safe.contains("<ESC>]52;c;owned^G"));
        assert!(safe.contains("<ESC>[31mred"));
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
    fn legacy_repl_session_switch_fails_closed_on_unloadable_list_row() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let submitted = Arc::new(StdMutex::new(Vec::new()));
            let deleted = Arc::new(StdMutex::new(Vec::new()));
            let created = Arc::new(StdMutex::new(Vec::new()));
            let display_only = session("legacy-server-key", "Research");
            let app = Arc::new(Mutex::new(App {
                workspaces: vec![workspace_with_session_client_options(
                    0,
                    "alpha",
                    vec![display_only],
                    Arc::clone(&submitted),
                    Arc::clone(&deleted),
                    None,
                    vec!["legacy-server-key".to_string()],
                    Arc::clone(&created),
                )],
                active: 0,
                shared_mnemos: None,
                config_path: PathBuf::from("test-config.toml"),
            }));
            let main = session("main", "Main");
            let repl = ReplLoop::new(
                Arc::clone(&app),
                main,
                "model".to_string(),
                "provider".to_string(),
            )
            .unwrap();

            let err = repl
                .resolve_or_create_active_workspace_session("legacy-server-key")
                .await
                .unwrap_err();

            let msg = err.to_string();
            assert!(msg.contains("listed session 'legacy-server-key'"));
            assert!(msg.contains("could not be loaded"));
            assert!(msg.contains("refusing to create a replacement session"));
            assert!(created.lock().unwrap().is_empty());
            assert!(submitted.lock().unwrap().is_empty());
        });

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn legacy_session_action_switch_fails_closed_on_unloadable_list_row() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let submitted = Arc::new(StdMutex::new(Vec::new()));
            let deleted = Arc::new(StdMutex::new(Vec::new()));
            let created = Arc::new(StdMutex::new(Vec::new()));
            let display_only = session("legacy-server-key", "Research");
            let app = Arc::new(Mutex::new(App {
                workspaces: vec![workspace_with_session_client_options(
                    0,
                    "alpha",
                    vec![display_only],
                    Arc::clone(&submitted),
                    Arc::clone(&deleted),
                    None,
                    vec!["legacy-server-key".to_string()],
                    Arc::clone(&created),
                )],
                active: 0,
                shared_mnemos: None,
                config_path: PathBuf::from("test-config.toml"),
            }));
            let main = session("main", "Main");
            let mut repl = ReplLoop::new(
                Arc::clone(&app),
                main.clone(),
                "model".to_string(),
                "provider".to_string(),
            )
            .unwrap();

            let err = repl
                .apply_legacy_session_action(LegacySessionAction::Switch {
                    target: "legacy-server-key".to_string(),
                })
                .await
                .unwrap_err();

            let msg = err.to_string();
            assert!(msg.contains("listed session 'legacy-server-key'"));
            assert!(msg.contains("could not be loaded"));
            assert!(msg.contains("refusing to create a replacement session"));
            assert_eq!(repl.session.id, main.id);
            assert!(created.lock().unwrap().is_empty());
            assert!(submitted.lock().unwrap().is_empty());
        });

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[derive(Clone)]
    struct FakeWorkspaceClient {
        sessions: Vec<Session>,
        created_sessions: Arc<StdMutex<Vec<Session>>>,
        submitted: Arc<StdMutex<Vec<(String, String)>>>,
        deleted: Arc<StdMutex<Vec<String>>>,
        list_sessions_error: Option<String>,
        load_reject_ids: Vec<String>,
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
            if let Some(error) = &self.list_sessions_error {
                anyhow::bail!("{error}");
            }
            Ok(self.sessions.clone())
        }

        async fn create_session(&self, name: &str) -> anyhow::Result<Session> {
            let session = session(&format!("created-{name}"), name);
            self.created_sessions.lock().unwrap().push(session.clone());
            Ok(session)
        }

        async fn load_session(&self, session_id: &str) -> anyhow::Result<Session> {
            if self.load_reject_ids.iter().any(|id| id == session_id) {
                anyhow::bail!("display-only session is not loadable");
            }
            if let Some(session) = self
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .cloned()
            {
                return Ok(session);
            }
            self.created_sessions
                .lock()
                .unwrap()
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

    #[test]
    fn legacy_repl_user_transcript_append_failure_is_returned() {
        let scope = storage::workspace_scope("zeroclaw", "default", None).unwrap();

        let err = append_repl_transcript_entry(&scope, "../unsafe", "user", "secret").unwrap_err();

        assert!(err
            .to_string()
            .contains("could not append user transcript entry"));
        assert!(err.to_string().contains("unsafe session id"));
    }

    #[test]
    fn legacy_repl_post_submit_failure_marks_history_incomplete() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = storage::workspace_scope(
            "zeroclaw",
            &format!("legacy-transcript-{}", uuid::Uuid::new_v4()),
            None,
        )
        .unwrap();
        storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        let append_error = anyhow::anyhow!(
            "could not append assistant transcript entry for session main: disk full"
        );

        surface_repl_transcript_incomplete(&scope, "main", &append_error);

        assert!(storage::scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn legacy_repl_missing_pending_marker_marks_history_incomplete() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = storage::workspace_scope(
            "zeroclaw",
            &format!("legacy-pending-cleared-{}", uuid::Uuid::new_v4()),
            None,
        )
        .unwrap();

        let turn = mark_repl_transcript_pending(&scope, "main").unwrap();
        append_repl_transcript_entry(&scope, "main", "user", "hello").unwrap();
        storage::clear_scoped_session_history(&scope, "main").unwrap();
        append_repl_transcript_entry(&scope, "main", "assistant", "hi").unwrap();

        let err = clear_repl_transcript_pending_marker(&scope, "main", &turn).unwrap_err();

        assert!(err
            .to_string()
            .contains("was missing before turn completion"));
        assert!(storage::scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn legacy_repl_collection_overflow_marks_history_incomplete() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = storage::workspace_scope(
            "openclaw",
            &format!("legacy-overflow-transcript-{}", uuid::Uuid::new_v4()),
            None,
        )
        .unwrap();
        storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        let reason =
            "openclaw: turn collection failed; accepted assistant turn exceeded cap".to_string();

        assert!(repl_turn_collection_failure_requires_incomplete_transcript(
            &reason
        ));
        surface_repl_transcript_incomplete_reason(&scope, "main", &reason);

        assert!(storage::scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn legacy_repl_submit_error_marks_history_incomplete() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = storage::workspace_scope(
            "zeroclaw",
            &format!("legacy-submit-error-transcript-{}", uuid::Uuid::new_v4()),
            None,
        )
        .unwrap();
        storage::append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        let reason = "WebSocket read failed: reset".to_string();

        assert!(repl_submit_error_requires_incomplete_transcript(&reason));
        surface_repl_transcript_incomplete_reason(&scope, "main", &reason);

        assert!(storage::scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    fn workspace(
        id: usize,
        name: &str,
        sessions: Vec<Session>,
        submitted: Arc<StdMutex<Vec<(String, String)>>>,
        deleted: Arc<StdMutex<Vec<String>>>,
    ) -> Workspace {
        workspace_with_list_sessions_error(id, name, sessions, submitted, deleted, None)
    }

    fn workspace_with_list_sessions_error(
        id: usize,
        name: &str,
        sessions: Vec<Session>,
        submitted: Arc<StdMutex<Vec<(String, String)>>>,
        deleted: Arc<StdMutex<Vec<String>>>,
        list_sessions_error: Option<&str>,
    ) -> Workspace {
        workspace_with_session_client_options(
            id,
            name,
            sessions,
            submitted,
            deleted,
            list_sessions_error,
            Vec::new(),
            Arc::new(StdMutex::new(Vec::new())),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn workspace_with_session_client_options(
        id: usize,
        name: &str,
        sessions: Vec<Session>,
        submitted: Arc<StdMutex<Vec<(String, String)>>>,
        deleted: Arc<StdMutex<Vec<String>>>,
        list_sessions_error: Option<&str>,
        load_reject_ids: Vec<String>,
        created_sessions: Arc<StdMutex<Vec<Session>>>,
    ) -> Workspace {
        let fake = FakeWorkspaceClient {
            sessions,
            created_sessions,
            submitted,
            deleted,
            list_sessions_error: list_sessions_error.map(str::to_string),
            load_reject_ids,
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

    #[test]
    fn legacy_repl_unknown_mutation_persists_and_blocks_fence() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut server = mockito::Server::new_async().await;
            let _mock = server
                .mock("POST", "/api/cron/add")
                .with_status(201)
                .with_body("{not-json")
                .create_async()
                .await;
            let submitted = Arc::new(StdMutex::new(Vec::new()));
            let deleted = Arc::new(StdMutex::new(Vec::new()));
            let alpha = session("alpha-session", "chat");
            let app = Arc::new(Mutex::new(App {
                workspaces: vec![workspace(
                    0,
                    "alpha",
                    vec![alpha.clone()],
                    Arc::clone(&submitted),
                    Arc::clone(&deleted),
                )],
                active: 0,
                shared_mnemos: None,
                config_path: PathBuf::from("test-config.toml"),
            }));
            app.lock().await.workspaces[0].cron =
                Some(ZeroclawClient::new(server.url(), "test_token".to_string()));
            let mut repl = ReplLoop::new(
                Arc::clone(&app),
                alpha,
                "model".to_string(),
                "provider".to_string(),
            )
            .unwrap();

            let out = repl
                .handle_slash_command("/cron add '0 9 * * *' 'standup'")
                .await
                .unwrap()
                .unwrap();
            assert!(out.contains("Failed to create cron job"));
            assert!(out.contains("[blocked]"));
            assert!(delighters::mutation_fence_for_workspace("name:alpha")
                .unwrap()
                .is_some());

            let blocked = repl
                .handle_slash_command("/cron add '0 9 * * *' 'standup'")
                .await
                .unwrap()
                .unwrap();
            assert!(blocked.contains("mutation outcome is unknown"));

            let cleared = repl
                .handle_slash_command("/resync --force")
                .await
                .unwrap()
                .unwrap();
            assert!(cleared.contains("mutation fence cleared"));
            assert!(delighters::mutation_fence_for_workspace("name:alpha")
                .unwrap()
                .is_none());
        });

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn legacy_repl_write_ahead_fence_persists_before_mutating_dispatch() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let submitted = Arc::new(StdMutex::new(Vec::new()));
            let deleted = Arc::new(StdMutex::new(Vec::new()));
            let alpha = session("alpha-session", "chat");
            let app = Arc::new(Mutex::new(App {
                workspaces: vec![workspace(
                    0,
                    "alpha",
                    vec![alpha.clone()],
                    Arc::clone(&submitted),
                    Arc::clone(&deleted),
                )],
                active: 0,
                shared_mnemos: None,
                config_path: PathBuf::from("test-config.toml"),
            }));
            let repl = ReplLoop::new(
                Arc::clone(&app),
                alpha,
                "model".to_string(),
                "provider".to_string(),
            )
            .unwrap();

            let owner = repl
                .write_ahead_mutation_fence_for_dispatch("/memory post remember this")
                .await
                .unwrap();

            assert_eq!(owner.key, "name:alpha");
            assert!(!owner.dispatch_id.is_empty());
            let fence = delighters::mutation_fence_for_workspace("name:alpha")
                .unwrap()
                .unwrap();
            assert_eq!(fence.command, "/memory post remember this");
            assert!(fence.reason.contains("backend outcome is pending"));
            assert_eq!(fence.dispatch_id, owner.dispatch_id);
            assert!(submitted.lock().unwrap().is_empty());
        });

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn legacy_repl_mutation_fence_blocks_plain_turn_input() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let submitted = Arc::new(StdMutex::new(Vec::new()));
            let deleted = Arc::new(StdMutex::new(Vec::new()));
            let alpha = session("alpha-session", "chat");
            let app = Arc::new(Mutex::new(App {
                workspaces: vec![workspace(
                    0,
                    "alpha",
                    vec![alpha.clone()],
                    Arc::clone(&submitted),
                    Arc::clone(&deleted),
                )],
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
            let reason = repl
                .persist_mutation_fence("slash command outcome unknown for `/cron add job`")
                .await;
            assert!(reason.contains("slash command outcome unknown"));

            let blocked = repl
                .mutation_fence_block_output("hello agent")
                .await
                .unwrap();

            assert!(blocked.contains("mutation outcome is unknown"));
            assert!(blocked.contains("/resync --force"));
            assert!(repl.mutation_fence_block_output("/help").await.is_none());
            assert!(repl
                .mutation_fence_block_output("/resync --force")
                .await
                .is_none());
            for input in [
                "/session list",
                "/session info",
                "/workspace info",
                "/workspaces",
                "/cron list",
                "/memory list",
                "/memory search deploy",
                "/models status",
                "/providers list",
                "/mcp status",
                "/config",
            ] {
                assert!(
                    repl.mutation_fence_block_output(input).await.is_none(),
                    "{input} should remain available for mutation-fence recovery"
                );
            }
            for input in [
                "/memory post remember this",
                "/memory delete mem-1",
                "/cron add backup",
                "/workspace switch beta",
                "/session create scratch",
                "/models set primary",
                "/clear",
            ] {
                assert!(
                    repl.mutation_fence_block_output(input).await.is_some(),
                    "{input} should stay blocked while mutation outcome is unknown"
                );
            }
            let usage = repl
                .handle_legacy_resync("/resync --force extra")
                .await
                .unwrap()
                .unwrap();
            assert!(usage.contains("usage: /resync [--force]"));
            assert!(delighters::mutation_fence_for_workspace("name:alpha")
                .unwrap()
                .is_some());
            assert!(submitted.lock().unwrap().is_empty());
        });

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn legacy_repl_workspace_switch_setup_failure_persists_fence() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let alpha_submitted = Arc::new(StdMutex::new(Vec::new()));
            let beta_submitted = Arc::new(StdMutex::new(Vec::new()));
            let alpha_deleted = Arc::new(StdMutex::new(Vec::new()));
            let beta_deleted = Arc::new(StdMutex::new(Vec::new()));
            let alpha = session("alpha-session", "chat");
            let app = Arc::new(Mutex::new(App {
                workspaces: vec![
                    workspace(
                        0,
                        "alpha",
                        vec![alpha.clone()],
                        Arc::clone(&alpha_submitted),
                        Arc::clone(&alpha_deleted),
                    ),
                    workspace_with_list_sessions_error(
                        1,
                        "beta",
                        Vec::new(),
                        Arc::clone(&beta_submitted),
                        Arc::clone(&beta_deleted),
                        Some("backend listing failed"),
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
            let out = repl
                .handle_slash_command("/workspace switch beta")
                .await
                .expect("post-switch setup failure should be surfaced")
                .expect("workspace switch should return blocked output");

            assert_eq!(app.lock().await.active, 1);
            assert!(out.contains("switched to workspace: beta"));
            assert!(out.contains("[blocked]"));
            assert!(out.contains("workspace switch was applied, but session setup failed"));
            assert!(out.contains("backend listing failed"));
            assert!(delighters::mutation_fence_for_workspace("name:beta")
                .unwrap()
                .is_some());

            let blocked = repl
                .handle_slash_command("/session list")
                .await
                .unwrap()
                .unwrap();
            assert!(blocked.contains("mutation outcome is unknown"));
        });

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
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
    #[allow(clippy::await_holding_lock)]
    async fn legacy_repl_boot_binding_serves_info_and_first_turn_without_create() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let submitted = Arc::new(StdMutex::new(Vec::new()));
        let deleted = Arc::new(StdMutex::new(Vec::new()));
        let created = Arc::new(StdMutex::new(Vec::new()));
        let boot = session("boot-session", "chat");
        let app = Arc::new(Mutex::new(App {
            workspaces: vec![workspace_with_session_client_options(
                0,
                "alpha",
                vec![boot.clone()],
                Arc::clone(&submitted),
                Arc::clone(&deleted),
                None,
                Vec::new(),
                Arc::clone(&created),
            )],
            active: 0,
            shared_mnemos: None,
            config_path: PathBuf::from("test-config.toml"),
        }));
        let mut repl = ReplLoop::new(
            Arc::clone(&app),
            boot,
            "model".to_string(),
            "provider".to_string(),
        )
        .unwrap();

        let info = repl
            .handle_slash_command("/info")
            .await
            .unwrap()
            .expect("/info should render against boot session");
        assert!(info.contains("Session"));
        assert!(created.lock().unwrap().is_empty());

        let session_id = repl.turn_session_id_for_active_workspace().await.unwrap();
        let active_client = repl.resolve_active_client().await.unwrap();
        active_client
            .lock()
            .await
            .submit_turn(&session_id, "hello")
            .await
            .unwrap();

        assert_eq!(session_id, "boot-session");
        assert!(created.lock().unwrap().is_empty());
        assert_eq!(
            submitted.lock().unwrap().as_slice(),
            &[("boot-session".to_string(), "hello".to_string())]
        );

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn repl_workspace_switch_then_delete_active_new_workspace_session_is_blocked() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

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

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn legacy_repl_malformed_quoted_session_switch_does_not_rebind() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

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

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn legacy_repl_quoted_session_switch_rebinds_to_single_target() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

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

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }
}
