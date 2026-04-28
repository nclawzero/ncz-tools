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
    let has_multi_workspace = crate::cli::workspace::AppConfig::default_path()
        .ok()
        .and_then(|p| crate::cli::workspace::AppConfig::load(&p).ok())
        .map(|cfg| !cfg.workspaces.is_empty())
        .unwrap_or(false);

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

    let session = load_or_create_session(&active_client, &session_name).await?;
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

/// Load existing session or create new one. Goes through
/// the trait-boxed active-workspace client so openclaw and
/// zeroclaw backends both work here.
async fn load_or_create_session(
    client: &std::sync::Arc<
        tokio::sync::Mutex<Box<dyn crate::cli::agent::AgentClient + Send + Sync>>,
    >,
    session_name: &str,
) -> Result<Session> {
    // Try to load existing session metadata
    if let Ok(metadata) = storage::load_session_metadata(session_name) {
        info!("Found existing session: {}", session_name);
        return Ok(Session {
            id: metadata.id.clone(),
            name: metadata.name.clone(),
            model: metadata.model.clone(),
            provider: metadata.provider.clone(),
        });
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
        storage::save_session_metadata(&metadata)?;
    } else {
        warn!(
            "not saving local metadata for unsafe session id: {}",
            metadata.id
        );
    }

    Ok(session)
}
