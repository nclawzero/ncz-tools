use anyhow::{anyhow, Result};
use chrono::Utc;
use std::io::{self, Write};
use tracing::{info, warn};

use crate::cli::client::Session;
use crate::cli::pairing::PairingManager;
use crate::cli::storage::{self, SessionMetadata};

pub mod delighters;
pub mod onboarding;
pub mod repl;
pub mod rusty_repl;
pub mod splash;
pub mod themes;
pub mod tv_ui;

/// Run the ZTerm interactive REPL
pub async fn run(
    session_name: Option<String>,
    remote: Option<String>,
    token: Option<String>,
    workspace: Option<String>,
    legacy_repl: bool,
) -> Result<()> {
    info!("Starting ZTerm");

    // Ensure config directories exist
    storage::ensure_config_dir()?;
    storage::ensure_sessions_dir()?;

    // Determine if TTY
    let is_tty = atty::is(atty::Stream::Stdin);
    info!("TTY mode: {}", is_tty);

    // Check if config exists
    let config_exists = storage::config_exists()?;
    info!("Config exists: {}", config_exists);

    // If no config, run onboarding
    if !config_exists {
        info!("No config found. Running onboarding...");
        onboarding::run_onboarding().await?;
    }

    // Load config
    let config_content = storage::load_config()?;
    let config: toml::Value = toml::from_str(&config_content)?;

    // Extract API endpoint and token
    let gateway_url = remote.unwrap_or_else(|| {
        config
            .get("gateway")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("http://localhost:8888")
            .to_string()
    });

    let api_token = token.or_else(|| {
        config
            .get("gateway")
            .and_then(|v| v.get("token"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

    // Peek at ~/.zterm/config.toml's [[workspaces]] before the legacy
    // pairing flow. If multi-workspace mode is configured, each
    // workspace carries its own token/auth — the --remote URL + legacy
    // pairing block below are irrelevant and would spuriously fail
    // against port 8888 defaults. Synthesized (single-workspace) mode
    // keeps pairing as-is.
    let has_multi_workspace = has_configured_workspaces()?;

    // If no token, consult the gateway's `require_pairing` flag before
    // attempting the interactive pairing flow. Zeroclaw gateways with
    // `require_pairing: false` (local/loopback mode) emit no pairing
    // code and should be used with an empty bearer token. Multi-
    // workspace mode skips the whole legacy dance — each workspace
    // carries its own auth.
    let api_token = if let Some(token) = api_token {
        token
    } else if has_multi_workspace {
        info!("Multi-workspace config detected; skipping legacy pairing.");
        String::new()
    } else {
        let pairing_manager = PairingManager::new(&gateway_url);
        let pairing_required = pairing_manager.requires_pairing().await.unwrap_or(true);

        if !pairing_required {
            info!("Gateway reports pairing not required; proceeding without token.");
            String::new()
        } else {
            info!("No token found. Attempting pairing...");

            println!("\n🔐 Pairing Required");
            println!("Getting pairing code from gateway...");
            let pairing_code = pairing_manager.get_pairing_code().await?;
            println!("Pairing code: {}", pairing_code);
            println!("\nEnter the code from the pairing dashboard to complete pairing.");
            print!("Code confirmation: ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let confirmed_code = input.trim();

            if confirmed_code != pairing_code {
                return Err(anyhow!("Pairing code mismatch"));
            }

            let token = pairing_manager.complete_pairing(&pairing_code).await?;
            println!("✓ Pairing successful!");

            let mut config: toml::Value = toml::from_str(&storage::load_config()?)?;
            if config.get_mut("gateway").is_none() {
                config["gateway"] = toml::Value::Table(toml::map::Map::new());
            }
            config["gateway"]["token"] = toml::Value::String(token.clone());
            storage::save_config(&toml::to_string_pretty(&config)?)?;

            token
        }
    };

    info!("Gateway URL: {}", gateway_url);

    // Build a multi-workspace App. If ~/.zterm/config.toml has
    // [[workspaces]], use them. Otherwise synthesize a single
    // zeroclaw workspace from the legacy gateway_url + api_token.
    let mut app = crate::cli::workspace::App::boot_or_synthesize(
        gateway_url.clone(),
        Some(api_token.clone()),
    )?;

    // Honor the --workspace override if the user passed one and
    // the named workspace exists. Silently no-ops when running
    // in single-workspace / synthesized mode (only "default"
    // exists there; mismatches are informative, not fatal).
    if let Some(target) = workspace {
        let idx = app.workspaces.iter().position(|w| w.config.name == target);
        match idx {
            Some(i) => {
                info!("--workspace override: activating '{}'", target);
                app.active = i;
            }
            None => {
                let avail: Vec<_> = app
                    .workspaces
                    .iter()
                    .map(|w| w.config.name.clone())
                    .collect();
                eprintln!(
                    "⚠️  --workspace {target:?} not found in config (known: {avail:?}); \
                     staying on '{}'",
                    app.active_workspace()
                        .map(|w| w.config.name.as_str())
                        .unwrap_or("<none>")
                );
            }
        }
    }

    // Activate the active workspace (no-op for zeroclaw; runs the
    // openclaw handshake for openclaw workspaces).
    if let Some(ws) = app.active_workspace_mut() {
        ws.activate().await.map_err(|e| {
            anyhow!(
                "failed to activate workspace \'{}\' at boot: {e}",
                ws.config.name
            )
        })?;
    } else {
        return Err(anyhow!("no active workspace after boot"));
    }

    let active_ws = app
        .active_workspace()
        .expect("active workspace just activated");
    let active_client = active_ws
        .client
        .clone()
        .expect("workspace client populated after activate()");
    let active_storage_scope = local_storage_scope_for_workspace(active_ws)?;

    // Test connection through the trait
    info!("Testing gateway connection...");
    {
        let healthy = active_client.lock().await.health().await?;
        if !healthy {
            eprintln!("❌ Could not connect to gateway at {}", gateway_url);
            eprintln!("   Make sure the agent backend is running.");
            return Err(anyhow!("Gateway connection failed"));
        }
    }
    info!("✓ Gateway connection successful");

    // Load or create session (also through the trait now)
    let session_name = session_name.unwrap_or_else(|| "main".to_string());
    info!("Loading session: {}", session_name);

    let session =
        load_or_create_session(&active_client, &active_storage_scope, &session_name).await?;
    info!("Session loaded: {}", session.id);

    // Get current model/provider. The `~/.zterm/config.toml` value
    // is used only as a transient default for the splash + status
    // line until `refresh_models` lands the live `/api/config` data.
    // Defaults are neutral config-key strings so fallbacks do not
    // pin a vendor or upstream model name.
    let config_content = storage::load_config()?;
    let config: toml::Value = toml::from_str(&config_content)?;

    let local_default_model = config
        .get("agent")
        .and_then(|v| v.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("primary")
        .to_string();

    let provider = config
        .get("agent")
        .and_then(|v| v.get("provider"))
        .and_then(|v| v.as_str())
        .unwrap_or("zeroclaw")
        .to_string();

    // Refresh the model list from /api/config once at boot. If the
    // active workspace is zeroclaw, this populates the cached
    // `[providers.models.*]` table on the cron handle so `/models`
    // can list real keys, and seeds `current_model_key` with the
    // daemon's preferred default (per `[providers] fallback`).
    // Failure is non-fatal — `current_model_key()` falls back to
    // `"primary"` and `/models` shows an empty list with an advisory.
    let model = {
        let cron_opt = app.active_workspace().and_then(|w| w.cron.clone());
        match cron_opt {
            Some(c) => match c.refresh_models().await {
                Ok(_) => c.current_model_key(),
                Err(e) => {
                    tracing::warn!("tui: refresh_models failed: {e:#}");
                    local_default_model.clone()
                }
            },
            None => local_default_model.clone(),
        }
    };

    // Display splash screen (check config to see if enabled)
    let show_splash = config
        .get("ui")
        .and_then(|v| v.get("splash_screen"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true); // Default: show splash

    if show_splash {
        splash::display_splash(&session_name, &gateway_url, &model, &provider);
    }

    let shared_app = std::sync::Arc::new(tokio::sync::Mutex::new(app));

    if legacy_repl {
        info!("--legacy-repl: running rustyline REPL fallback");
        let mut repl = repl::ReplLoop::new(shared_app, session, model, provider)?;
        repl.run().await?;
    } else {
        tv_ui::run(shared_app, session, model, provider).await?;
    }

    Ok(())
}

fn has_configured_workspaces() -> Result<bool> {
    let path = crate::cli::workspace::AppConfig::default_path()?;
    let cfg = crate::cli::workspace::AppConfig::load(&path)?;
    Ok(!cfg.workspaces.is_empty())
}

/// Load existing session or create new one. Goes through
/// the trait-boxed active-workspace client so openclaw and
/// zeroclaw backends both work here.
async fn load_or_create_session(
    client: &std::sync::Arc<
        tokio::sync::Mutex<Box<dyn crate::cli::agent::AgentClient + Send + Sync>>,
    >,
    scope: &storage::LocalWorkspaceScope,
    session_name: &str,
) -> Result<Session> {
    let local_metadata = storage::load_scoped_session_metadata(scope, session_name).ok();
    load_or_create_session_with_metadata(client, scope, session_name, local_metadata).await
}

async fn load_or_create_session_with_metadata(
    client: &std::sync::Arc<
        tokio::sync::Mutex<Box<dyn crate::cli::agent::AgentClient + Send + Sync>>,
    >,
    scope: &storage::LocalWorkspaceScope,
    session_name: &str,
    local_metadata: Option<SessionMetadata>,
) -> Result<Session> {
    match client.lock().await.list_sessions().await {
        Ok(sessions) => {
            if let Some(session) = choose_boot_session_by_id_or_name(&sessions, session_name)? {
                info!("Found existing backend session: {}", session.id);
                return Ok(session.clone());
            }
        }
        Err(e) => {
            warn!("could not list backend sessions while booting '{session_name}': {e}");
        }
    }

    if let Some(metadata) = local_metadata {
        match client.lock().await.load_session(&metadata.id).await {
            Ok(session) if session.id == metadata.id => {
                info!(
                    "Validated cached session metadata against backend: {}",
                    session.id
                );
                return Ok(session);
            }
            Ok(session) => {
                warn!(
                    "ignoring cached session metadata for '{}': backend returned mismatched id '{}'",
                    metadata.id, session.id
                );
            }
            Err(e) => {
                warn!(
                    "ignoring stale cached session metadata for '{}': {e}",
                    metadata.id
                );
            }
        }
    }

    // Create new session
    info!("Creating new session: {}", session_name);
    let session = client.lock().await.create_session(session_name).await?;

    // Save metadata
    let metadata = SessionMetadata {
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

    Ok(session)
}

fn local_storage_scope_for_workspace(
    workspace: &crate::cli::workspace::Workspace,
) -> Result<storage::LocalWorkspaceScope> {
    storage::workspace_scope(
        workspace.config.backend.as_str(),
        &workspace.config.name,
        workspace.config.id.as_deref(),
    )
}

fn choose_boot_session_by_id_or_name<'a>(
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
            return Err(anyhow!(
                "ambiguous backend session id '{requested}' while booting"
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
        _ => Err(anyhow!(
            "ambiguous backend session name '{requested}' while booting"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::agent::{AgentClient, StreamSink};
    use crate::cli::client::{Config, Model, Provider};
    use std::sync::{Arc, Mutex as StdMutex, OnceLock};
    use tokio::sync::Mutex;

    #[derive(Clone)]
    struct BootFakeClient {
        list_sessions: Vec<Session>,
        load_sessions: Vec<Session>,
        create_session: Session,
        loaded: Arc<StdMutex<Vec<String>>>,
        created: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl AgentClient for BootFakeClient {
        async fn health(&self) -> Result<bool> {
            Ok(true)
        }

        async fn get_config(&self) -> Result<Config> {
            Ok(Config {
                agent: Default::default(),
            })
        }

        async fn put_config(&self, _config: &Config) -> Result<()> {
            Ok(())
        }

        async fn list_providers(&self) -> Result<Vec<Provider>> {
            Ok(Vec::new())
        }

        async fn get_models(&self, _provider: &str) -> Result<Vec<Model>> {
            Ok(Vec::new())
        }

        async fn list_provider_models(&self, _provider: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn list_sessions(&self) -> Result<Vec<Session>> {
            Ok(self.list_sessions.clone())
        }

        async fn create_session(&self, name: &str) -> Result<Session> {
            self.created.lock().unwrap().push(name.to_string());
            Ok(self.create_session.clone())
        }

        async fn load_session(&self, session_id: &str) -> Result<Session> {
            self.loaded.lock().unwrap().push(session_id.to_string());
            self.load_sessions
                .iter()
                .find(|session| session.id == session_id)
                .cloned()
                .ok_or_else(|| anyhow!("session not found"))
        }

        async fn delete_session(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }

        async fn submit_turn(&mut self, _session_id: &str, _message: &str) -> Result<String> {
            Ok(String::new())
        }

        fn set_stream_sink(&mut self, _sink: Option<StreamSink>) {}
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

    fn scope() -> storage::LocalWorkspaceScope {
        storage::workspace_scope("zeroclaw", "test", None).unwrap()
    }

    fn boxed_client(fake: BootFakeClient) -> Arc<Mutex<Box<dyn AgentClient + Send + Sync>>> {
        Arc::new(Mutex::new(Box::new(fake)))
    }

    fn env_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    struct EnvGuard {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn boot_returns_workspace_state_error_before_legacy_pairing_token_write() {
        let _env = env_lock().lock().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let zterm_config_dir = tempfile::TempDir::new().unwrap();
        let _home_guard = EnvGuard::set_path("HOME", home.path());
        let _zterm_config_guard = EnvGuard::set_path("ZTERM_CONFIG_DIR", zterm_config_dir.path());

        let legacy_config_dir = home.path().join(".zeroclaw");
        std::fs::create_dir_all(&legacy_config_dir).unwrap();
        let legacy_config_path = legacy_config_dir.join("config.toml");
        std::fs::write(
            &legacy_config_path,
            r#"
[gateway]
url = "http://127.0.0.1:1"
"#,
        )
        .unwrap();
        std::fs::write(
            zterm_config_dir.path().join("config.toml"),
            r#"
[[workspaces]]
name = "oc"
backend = "openclaw"
url = "ws://example.invalid"
"#,
        )
        .unwrap();
        std::fs::write(
            zterm_config_dir.path().join("workspace-state.toml"),
            "openclaw_workspaces = [",
        )
        .unwrap();

        let err = run(Some("main".to_string()), None, None, None, true)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");

        assert!(msg.contains("parsing zterm workspace state"));
        assert!(!std::fs::read_to_string(&legacy_config_path)
            .unwrap()
            .contains("token"));
    }

    #[tokio::test]
    async fn boot_prefers_active_backend_session_over_stale_cached_main() {
        let loaded = Arc::new(StdMutex::new(Vec::new()));
        let created = Arc::new(StdMutex::new(Vec::new()));
        let fake = BootFakeClient {
            list_sessions: vec![session("active-main", "main")],
            load_sessions: Vec::new(),
            create_session: session("created/main", "main"),
            loaded: Arc::clone(&loaded),
            created: Arc::clone(&created),
        };

        let selected = load_or_create_session_with_metadata(
            &boxed_client(fake),
            &scope(),
            "main",
            Some(metadata("foreign-main", "main")),
        )
        .await
        .expect("active backend main should resolve");

        assert_eq!(selected.id, "active-main");
        assert!(loaded.lock().unwrap().is_empty());
        assert!(created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn boot_does_not_return_stale_cross_workspace_cached_metadata() {
        let loaded = Arc::new(StdMutex::new(Vec::new()));
        let created = Arc::new(StdMutex::new(Vec::new()));
        let fake = BootFakeClient {
            list_sessions: Vec::new(),
            load_sessions: Vec::new(),
            create_session: session("created/main", "main"),
            loaded: Arc::clone(&loaded),
            created: Arc::clone(&created),
        };

        let selected = load_or_create_session_with_metadata(
            &boxed_client(fake),
            &scope(),
            "main",
            Some(metadata("foreign-main", "main")),
        )
        .await
        .expect("stale local metadata should fall through to create");

        assert_eq!(selected.id, "created/main");
        assert_eq!(loaded.lock().unwrap().as_slice(), ["foreign-main"]);
        assert_eq!(created.lock().unwrap().as_slice(), ["main"]);
    }
}
