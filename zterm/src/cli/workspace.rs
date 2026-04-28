//! Multi-workspace data layer — v0.2 chunk D1.
//!
//! This module defines the static + runtime shape of zterm's
//! multi-workspace model. It is pure data plumbing: no UX, no
//! rendering, no hotkey routing. Those land in D2–D5 per
//! `docs/v0.2-roadmap.md`.
//!
//! ### Layering
//!
//! ```text
//!    [ ~/.zterm/config.toml ]              static
//!             ↓ parse
//!    WorkspaceConfig { name, backend, url, token_env?, ... }
//!             ↓ instantiate
//!    Workspace { id, label, client, session?, scrollback }   runtime
//!             ↓ aggregated into
//!    App { workspaces, active, shared_mnemos, config_path }
//! ```
//!
//! `Workspace::instantiate` builds a concrete client based on the
//! `backend` field (`zeroclaw` → `ZeroclawClient`, `openclaw` →
//! boxes into the openclaw trait path). The client is immediately
//! stored behind the same `Arc<Mutex<Box<dyn AgentClient>>>`
//! shape that `CommandHandler` (per chunk A-3) already expects, so
//! subsequent UX slices can swap which workspace's client the
//! command dispatcher sees by flipping an index — no type dance.
//!
//! ### Scope boundary
//!
//! D1 deliberately does NOT:
//! - Spawn per-workspace event-loop tasks (that's D4 background
//!   streaming).
//! - Render a tab bar or handle hotkeys (D2, D3).
//! - Wire into the REPL replacement loop (chunk-D-end).
//! - Handle per-workspace reconnect or failure-state badges (D5).
//!
//! Those depend on this data layer but none of them land in this
//! slice. Keeping D1 purely additive + testable means the runtime
//! wiring in later slices doesn't need to re-invent the data shape.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::cli::agent::AgentClient;
use crate::cli::client::ZeroclawClient;

/// Backend identifier as written in `~/.zterm/config.toml`.
/// Lower-case string enum for TOML ergonomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Zeroclaw,
    Openclaw,
    /// Placeholder for the v0.3 NemoClaw backend. Parses so users
    /// can add it to config early; `Workspace::instantiate` returns
    /// a clear error for now.
    Nemoclaw,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Zeroclaw => "zeroclaw",
            Backend::Openclaw => "openclaw",
            Backend::Nemoclaw => "nemoclaw",
        }
    }
}

/// Static workspace configuration as read from
/// `~/.zterm/config.toml`.
///
/// ```toml
/// [[workspaces]]
/// name = "zeroclaw-typhon"
/// backend = "zeroclaw"
/// url = "http://127.0.0.1:42617"
/// token_env = "ZEROCLAW_TOKEN_TYPHON"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Immutable workspace identifier used for zterm-owned
    /// backend namespaces. This is generated once during config
    /// migration and must survive display-name / URL edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Operator-facing name shown in the tab bar / selection UX.
    /// Must be unique within a single `AppConfig.workspaces` list.
    pub name: String,

    /// Which client implementation to build.
    pub backend: Backend,

    /// Gateway URL. HTTP for zeroclaw (`/api/config` + WS), WS for
    /// openclaw (`ws://host:port`). Scheme-validated at
    /// `Workspace::instantiate`.
    pub url: String,

    /// Name of an environment variable that holds the auth token.
    /// Preferred over inline `token` for obvious reasons. Either or
    /// both may be set; `token_env` wins when both are present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,

    /// Inline token. Useful for ephemeral / test workspaces; don't
    /// commit config.toml with this populated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Optional display override. Falls back to `name` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Previous zterm session namespaces that should remain visible
    /// while users migrate from the old mutable name+URL namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespace_aliases: Vec<String>,
}

impl WorkspaceConfig {
    /// Resolve the effective auth token at instantiation time.
    /// `token_env` wins; then `token`; else `None`.
    pub fn resolved_token(&self) -> Option<String> {
        if let Some(var) = &self.token_env {
            if let Ok(v) = std::env::var(var) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        self.token.clone()
    }

    /// Display name for tabs — `label` if set, else `name`.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

/// Top-level config-file shape.
///
/// ```toml
/// active = "zeroclaw-typhon"
///
/// [[workspaces]]
/// name = "zeroclaw-typhon"
/// ...
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// Name of the workspace that should be active on boot.
    /// If absent or doesn't match a defined workspace, the first
    /// workspace in the list is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,

    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
}

impl AppConfig {
    /// Load from a TOML file at `path`. Returns an empty config
    /// (no workspaces) if the path doesn't exist — zterm runs
    /// single-workspace against legacy `.env` config in that case.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading zterm config from {}", path.display()))?;
        let mut cfg = Self::parse(&text)?;
        if migrate_openclaw_workspace_ids(&mut cfg) {
            cfg.validate()?;
            let migrated = toml::to_string_pretty(&cfg)
                .with_context(|| "serializing migrated zterm config TOML")?;
            std::fs::write(path, migrated)
                .with_context(|| format!("writing migrated zterm config to {}", path.display()))?;
        }
        Ok(cfg)
    }

    /// Parse a TOML string (tested directly, avoids disk I/O).
    pub fn parse(text: &str) -> Result<Self> {
        let cfg: AppConfig = toml::from_str(text).with_context(|| "parsing zterm config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Enforce: every workspace has a unique name; `active` (if
    /// set) matches one of them.
    pub fn validate(&self) -> Result<()> {
        use std::collections::HashSet;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut seen_ids: HashSet<&str> = HashSet::new();
        for w in &self.workspaces {
            if !seen.insert(w.name.as_str()) {
                return Err(anyhow!(
                    "workspace name '{}' appears more than once",
                    w.name
                ));
            }
            if let Some(id) = w.id.as_deref() {
                if id.trim().is_empty() {
                    return Err(anyhow!("workspace '{}' has an empty id", w.name));
                }
                if !seen_ids.insert(id) {
                    return Err(anyhow!("workspace id '{}' appears more than once", id));
                }
            }
        }
        if let Some(active) = &self.active {
            if !seen.contains(active.as_str()) {
                return Err(anyhow!(
                    "`active = \"{}\"` doesn't match any [[workspaces]] entry",
                    active
                ));
            }
        }
        Ok(())
    }

    /// Default config path: `$HOME/.zterm/config.toml`, with
    /// `$ZTERM_CONFIG_DIR` override for tests.
    pub fn default_path() -> Result<PathBuf> {
        if let Ok(dir) = std::env::var("ZTERM_CONFIG_DIR") {
            return Ok(PathBuf::from(dir).join("config.toml"));
        }
        let home = std::env::var("HOME")
            .with_context(|| "HOME not set; cannot locate default zterm config")?;
        Ok(PathBuf::from(home).join(".zterm").join("config.toml"))
    }
}

/// A single live workspace — one client, one active session, one
/// conversational scrollback.
///
/// Not `Clone` by design: each workspace owns its client handles.
/// `App` holds `Vec<Workspace>`.
pub struct Workspace {
    /// Dense index into `App.workspaces`. Assigned at construction;
    /// stable for the lifetime of the `App`.
    pub id: usize,

    /// Snapshot of the static config this workspace was built from.
    pub config: WorkspaceConfig,

    /// Trait-boxed agent client. `None` until the workspace is
    /// activated — zeroclaw fills at `instantiate` time, openclaw
    /// populates at `activate` after the async handshake lands.
    pub client: Option<Arc<Mutex<Box<dyn AgentClient + Send + Sync>>>>,

    /// Optional concrete `ZeroclawClient` for cron + `/models set`
    /// which aren't on the trait yet. `Some` only when `backend` is
    /// `Zeroclaw`. Future slices hoist cron onto the trait; until
    /// then the workspace preserves a typed reference to the same
    /// underlying client where applicable.
    pub cron: Option<ZeroclawClient>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("cron_available", &self.cron.is_some())
            .field("activated", &self.client.is_some())
            .finish()
    }
}

impl Workspace {
    /// Build a workspace from its static config. Constructs the
    /// concrete client, boxes it behind the trait, and wraps in
    /// the Arc<Mutex<_>> shape that `CommandHandler` expects.
    ///
    /// Does NOT perform a handshake / live connect — that's a
    /// separate concern the caller owns. Keeps D1 purely data.
    pub fn instantiate(id: usize, config: WorkspaceConfig) -> Result<Self> {
        match config.backend {
            Backend::Zeroclaw => {
                let token = config
                    .resolved_token()
                    .unwrap_or_else(|| "unset".to_string());
                let concrete = ZeroclawClient::new(config.url.clone(), token);
                let cron = Some(concrete.clone());
                let boxed: Box<dyn AgentClient + Send + Sync> = Box::new(concrete);
                Ok(Self {
                    id,
                    config,
                    client: Some(Arc::new(Mutex::new(boxed))),
                    cron,
                })
            }
            Backend::Openclaw => {
                // OpenClawClient requires an async WebSocket handshake
                // + device-key load. `instantiate` stays pure data;
                // `activate()` performs the live connect later.
                Ok(Self {
                    id,
                    config,
                    client: None,
                    cron: None,
                })
            }
            Backend::Nemoclaw => Err(anyhow!(
                "nemoclaw backend is declared in config but not yet implemented (v0.3)"
            )),
        }
    }

    /// True once `activate()` has populated the client.
    pub fn is_activated(&self) -> bool {
        self.client.is_some()
    }

    /// Perform the live-connect step for backends that need one.
    ///
    /// - Zeroclaw: no-op (client already populated at instantiate time).
    /// - Openclaw: load-or-create the device key, run the full
    ///   WebSocket connect + handshake (slice 3b/3c), replace
    ///   `self.client` with the live client.
    /// - Nemoclaw: errors — not implemented.
    ///
    /// Safe to call repeatedly; a no-op when already activated.
    pub async fn activate(&mut self) -> Result<()> {
        if self.client.is_some() {
            return Ok(());
        }
        match self.config.backend {
            Backend::Zeroclaw => Err(anyhow!(
                "workspace '{}' (zeroclaw) has no client after instantiate; bug",
                self.config.name
            )),
            Backend::Openclaw => self.activate_openclaw().await,
            Backend::Nemoclaw => Err(anyhow!("nemoclaw backend is not yet implemented (v0.3)")),
        }
    }

    async fn activate_openclaw(&mut self) -> Result<()> {
        use crate::cli::openclaw::client::OpenClawClient;
        use crate::cli::openclaw::device::DeviceIdentity;
        use crate::cli::openclaw::handshake::{ClientIdentity, HandshakeParams};

        let device_key_path = default_openclaw_device_key_path()?;
        let device = DeviceIdentity::load_or_create(&device_key_path).with_context(|| {
            format!(
                "loading openclaw device key at {}",
                device_key_path.display()
            )
        })?;

        let params = HandshakeParams {
            client: ClientIdentity {
                id: "cli".to_string(),
                display_name: Some("zterm".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                mode: "cli".to_string(),
                platform: std::env::consts::OS.to_string(),
                device_family: None,
            },
            role: "operator".to_string(),
            scopes: vec!["operator.read".to_string(), "operator.write".to_string()],
            token: self.config.resolved_token(),
        };

        let mut client = OpenClawClient::connect_and_handshake(&self.config.url, &device, &params)
            .await
            .with_context(|| {
                format!(
                    "openclaw workspace '{}' connect+handshake to {}",
                    self.config.name, self.config.url
                )
            })?;
        client.set_session_namespace(openclaw_session_namespace(&self.config));
        client.set_session_namespace_aliases(openclaw_session_namespace_aliases(&self.config));

        let boxed: Box<dyn AgentClient + Send + Sync> = Box::new(client);
        self.client = Some(Arc::new(Mutex::new(boxed)));
        Ok(())
    }
}

fn openclaw_session_namespace(config: &WorkspaceConfig) -> String {
    if let Some(id) = config
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return format!("backend={};workspace_id={}", config.backend.as_str(), id);
    }
    openclaw_legacy_session_namespace(config)
}

fn openclaw_legacy_session_namespace(config: &WorkspaceConfig) -> String {
    format!(
        "backend={};workspace={};url={}",
        config.backend.as_str(),
        config.name.trim(),
        config.url.trim_end_matches('/')
    )
}

fn openclaw_session_namespace_aliases(config: &WorkspaceConfig) -> Vec<String> {
    let mut aliases = Vec::new();
    for alias in &config.namespace_aliases {
        push_unique_namespace(&mut aliases, alias.trim());
    }
    push_unique_namespace(&mut aliases, &openclaw_legacy_session_namespace(config));
    let primary = openclaw_session_namespace(config);
    aliases.retain(|alias| alias != &primary);
    aliases
}

fn push_unique_namespace(namespaces: &mut Vec<String>, namespace: &str) {
    if !namespace.is_empty() && !namespaces.iter().any(|existing| existing == namespace) {
        namespaces.push(namespace.to_string());
    }
}

fn migrate_openclaw_workspace_ids(cfg: &mut AppConfig) -> bool {
    let mut changed = false;
    for workspace in &mut cfg.workspaces {
        if workspace.backend != Backend::Openclaw {
            continue;
        }
        let legacy_namespace = openclaw_legacy_session_namespace(workspace);
        if workspace
            .id
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            workspace.id = Some(format!("ws_{}", uuid::Uuid::new_v4().simple()));
            changed = true;
        }
        if !workspace
            .namespace_aliases
            .iter()
            .any(|alias| alias == &legacy_namespace)
        {
            workspace.namespace_aliases.push(legacy_namespace);
            changed = true;
        }
    }
    changed
}

/// Canonical path for zterm's openclaw device key. Shared across
/// all openclaw-backed workspaces so a paired device identity
/// survives workspace add/remove/rename.
///
/// `$ZTERM_CONFIG_DIR` override for tests; `$HOME/.zterm/` default.
fn default_openclaw_device_key_path() -> Result<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("ZTERM_CONFIG_DIR") {
        return Ok(std::path::PathBuf::from(dir).join("openclaw-device.pem"));
    }
    let home = std::env::var("HOME")
        .with_context(|| "HOME not set; cannot locate default zterm config dir")?;
    Ok(std::path::PathBuf::from(home)
        .join(".zterm")
        .join("openclaw-device.pem"))
}

/// Lightweight snapshot of the App's workspace configuration for
/// display-only consumers (CommandHandler `/workspace list` and
/// `/workspace info`). Decouples those consumers from owning a
/// reference to the full `App` — important because `App` is going
/// to end up behind `Arc<Mutex<_>>` in a later slice when workspace
/// switching lands. A cheap snapshot keeps the read-only surface
/// simple in the meantime.
#[derive(Debug, Clone)]
pub struct WorkspaceSummary {
    pub name: String,
    pub backend: Backend,
    pub url: String,
    pub label: Option<String>,
    pub activated: bool,
}

/// Snapshot of the full workspace inventory for the REPL.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceInventory {
    pub workspaces: Vec<WorkspaceSummary>,
    pub active_index: usize,
}

impl WorkspaceInventory {
    /// Currently-active workspace summary, if any.
    pub fn active(&self) -> Option<&WorkspaceSummary> {
        self.workspaces.get(self.active_index)
    }

    /// Empty inventory — used when the REPL booted via the legacy
    /// single-workspace synthesized path and there's no meaningful
    /// multi-workspace content to list.
    pub fn empty() -> Self {
        Self::default()
    }

    /// True if the inventory is just a one-workspace synthesized
    /// App (i.e. the `default` zeroclaw workspace created by
    /// `App::synthesize_single_zeroclaw`). UX layers can hide the
    /// `/workspace` commands in that case if they want.
    pub fn is_synthetic_singleton(&self) -> bool {
        self.workspaces.len() == 1 && self.workspaces[0].name == "default"
    }
}

/// Runtime root — owns all workspaces, tracks the active index,
/// and holds the shared MNEMOS client.
pub struct App {
    pub workspaces: Vec<Workspace>,
    pub active: usize,
    pub shared_mnemos: Option<crate::cli::mnemos::MnemosClient>,
    pub config_path: PathBuf,
}

impl App {
    /// Build an `App` from an `AppConfig`. Instantiates every
    /// workspace that can be instantiated in a pure-data manner;
    /// logs (via tracing) and skips any that require async.
    /// Honors `config.active` when resolvable; falls back to the
    /// first successfully-instantiated workspace.
    pub fn from_config(cfg: AppConfig, config_path: PathBuf) -> Result<Self> {
        let mut workspaces: Vec<Workspace> = Vec::new();
        for (idx, wc) in cfg.workspaces.into_iter().enumerate() {
            match Workspace::instantiate(workspaces.len(), wc.clone()) {
                Ok(w) => workspaces.push(w),
                Err(e) => tracing::warn!(
                    "workspace '{}' (#{}) skipped at D1 instantiation: {e}",
                    wc.name,
                    idx
                ),
            }
        }

        let active = match cfg.active {
            Some(name) => workspaces
                .iter()
                .position(|w| w.config.name == name)
                .unwrap_or(0),
            None => 0,
        };

        Ok(Self {
            workspaces,
            active,
            shared_mnemos: crate::cli::mnemos::MnemosClient::from_env(),
            config_path,
        })
    }

    /// Read `config.toml` from the canonical path, build the `App`.
    pub fn boot() -> Result<Self> {
        let path = AppConfig::default_path()?;
        let cfg = AppConfig::load(&path)?;
        Self::from_config(cfg, path)
    }

    pub fn active_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active)
    }

    pub fn active_workspace_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(self.active)
    }

    /// Build a one-workspace `App` from a `url` + optional `token`.
    /// Used as the v0.1 legacy fallback when the user has no
    /// `[[workspaces]]` config (single-client mode) and as a test
    /// primitive for workspace-aware code.
    pub fn synthesize_single_zeroclaw(
        url: impl Into<String>,
        token: Option<String>,
    ) -> Result<Self> {
        let cfg = WorkspaceConfig {
            id: None,
            name: "default".to_string(),
            backend: Backend::Zeroclaw,
            url: url.into(),
            token_env: None,
            token,
            label: None,
            namespace_aliases: Vec::new(),
        };
        let ws = Workspace::instantiate(0, cfg)?;
        let config_path = AppConfig::default_path()
            .unwrap_or_else(|_| std::path::PathBuf::from("./.zterm-synthetic.toml"));
        Ok(Self {
            workspaces: vec![ws],
            active: 0,
            shared_mnemos: crate::cli::mnemos::MnemosClient::from_env(),
            config_path,
        })
    }

    /// Preferred boot path. Tries to read
    /// `~/.zterm/config.toml`; if it contains `[[workspaces]]`
    /// entries, builds normally via `from_config`. If the config
    /// is absent or defines no workspaces, synthesizes a single
    /// zeroclaw workspace from the fallback `remote` + `token`
    /// arguments (typical v0.1 CLI flow).
    ///
    /// Callers thus always get exactly one `App` shape to drive
    /// the REPL from, regardless of whether the user upgraded
    /// their config to multi-workspace yet.
    /// Build a display-only `WorkspaceInventory` snapshot. Cheap
    /// clone of each workspace's config + its activated status.
    /// Safe to call every frame if a UX wants live badging.
    pub fn inventory(&self) -> WorkspaceInventory {
        WorkspaceInventory {
            workspaces: self
                .workspaces
                .iter()
                .map(|w| WorkspaceSummary {
                    name: w.config.name.clone(),
                    backend: w.config.backend,
                    url: w.config.url.clone(),
                    label: w.config.label.clone(),
                    activated: w.is_activated(),
                })
                .collect(),
            active_index: self.active,
        }
    }

    pub fn boot_or_synthesize(remote: impl Into<String>, token: Option<String>) -> Result<Self> {
        let path = AppConfig::default_path()?;
        let cfg = AppConfig::load(&path)?;
        if !cfg.workspaces.is_empty() {
            return Self::from_config(cfg, path);
        }
        Self::synthesize_single_zeroclaw(remote, token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_zeroclaw_config() -> &'static str {
        r#"
active = "zeroclaw-typhon"

[[workspaces]]
name = "zeroclaw-typhon"
backend = "zeroclaw"
url = "http://127.0.0.1:42617"
token_env = "ZEROCLAW_TOKEN_TYPHON"

[[workspaces]]
name = "zeroclaw-edge"
backend = "zeroclaw"
url = "http://192.168.207.62:42617"
token = "inline-token"
label = "remote edge"
"#
    }

    #[test]
    fn parse_valid_config() {
        let cfg = AppConfig::parse(sample_zeroclaw_config()).unwrap();
        assert_eq!(cfg.workspaces.len(), 2);
        assert_eq!(cfg.active.as_deref(), Some("zeroclaw-typhon"));
        assert_eq!(cfg.workspaces[0].backend, Backend::Zeroclaw);
        assert_eq!(cfg.workspaces[1].label.as_deref(), Some("remote edge"));
    }

    #[test]
    fn parse_rejects_duplicate_names() {
        let text = r#"
[[workspaces]]
name = "dupe"
backend = "zeroclaw"
url = "http://a"

[[workspaces]]
name = "dupe"
backend = "zeroclaw"
url = "http://b"
"#;
        let err = AppConfig::parse(text).unwrap_err();
        assert!(err.to_string().contains("more than once"));
    }

    #[test]
    fn parse_rejects_active_missing_workspace() {
        let text = r#"
active = "nonexistent"

[[workspaces]]
name = "real"
backend = "zeroclaw"
url = "http://a"
"#;
        let err = AppConfig::parse(text).unwrap_err();
        assert!(err.to_string().contains("doesn't match"));
    }

    #[test]
    fn parse_empty_config_is_ok() {
        let cfg = AppConfig::parse("").unwrap();
        assert!(cfg.workspaces.is_empty());
        assert!(cfg.active.is_none());
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let cfg = AppConfig::load(Path::new("/definitely/does/not/exist.toml")).unwrap();
        assert!(cfg.workspaces.is_empty());
    }

    #[test]
    fn load_migrates_openclaw_workspace_id_and_legacy_namespace_alias() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
"#,
        )
        .unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let workspace = &cfg.workspaces[0];
        let id = workspace.id.as_deref().expect("migration should assign id");
        assert!(id.starts_with("ws_"));
        assert!(workspace
            .namespace_aliases
            .iter()
            .any(|alias| { alias == "backend=openclaw;workspace=alpha;url=ws://old.example" }));

        let reloaded = AppConfig::load(&path).unwrap();
        assert_eq!(reloaded.workspaces[0].id.as_deref(), Some(id));
    }

    #[test]
    fn immutable_openclaw_namespace_survives_workspace_rename_and_url_change() {
        let cfg = WorkspaceConfig {
            id: Some("ws_stable".to_string()),
            name: "renamed".to_string(),
            backend: Backend::Openclaw,
            url: "ws://new.example/".to_string(),
            token_env: None,
            token: None,
            label: None,
            namespace_aliases: vec![
                "backend=openclaw;workspace=alpha;url=ws://old.example".to_string()
            ],
        };

        assert_eq!(
            openclaw_session_namespace(&cfg),
            "backend=openclaw;workspace_id=ws_stable"
        );
        let aliases = openclaw_session_namespace_aliases(&cfg);
        assert!(aliases
            .iter()
            .any(|alias| alias == "backend=openclaw;workspace=alpha;url=ws://old.example"));
        assert!(aliases
            .iter()
            .any(|alias| alias == "backend=openclaw;workspace=renamed;url=ws://new.example"));
    }

    #[test]
    fn resolved_token_prefers_env_var() {
        let cfg = WorkspaceConfig {
            id: None,
            name: "w".into(),
            backend: Backend::Zeroclaw,
            url: "http://a".into(),
            token_env: Some("ZTERM_TEST_TOKEN_VAR".into()),
            token: Some("inline".into()),
            label: None,
            namespace_aliases: Vec::new(),
        };
        std::env::set_var("ZTERM_TEST_TOKEN_VAR", "env-wins");
        assert_eq!(cfg.resolved_token().as_deref(), Some("env-wins"));
        std::env::remove_var("ZTERM_TEST_TOKEN_VAR");
        // Env var unset → falls back to inline
        assert_eq!(cfg.resolved_token().as_deref(), Some("inline"));
    }

    #[test]
    fn resolved_token_none_when_nothing_configured() {
        let cfg = WorkspaceConfig {
            id: None,
            name: "w".into(),
            backend: Backend::Zeroclaw,
            url: "http://a".into(),
            token_env: None,
            token: None,
            label: None,
            namespace_aliases: Vec::new(),
        };
        assert!(cfg.resolved_token().is_none());
    }

    #[test]
    fn display_label_falls_back_to_name() {
        let a = WorkspaceConfig {
            id: None,
            name: "prod".into(),
            backend: Backend::Zeroclaw,
            url: "http://a".into(),
            token_env: None,
            token: None,
            label: Some("Production".into()),
            namespace_aliases: Vec::new(),
        };
        assert_eq!(a.display_label(), "Production");
        let b = WorkspaceConfig { label: None, ..a };
        assert_eq!(b.display_label(), "prod");
    }

    #[test]
    fn instantiate_zeroclaw_workspace_populates_client() {
        let cfg = WorkspaceConfig {
            id: None,
            name: "w".into(),
            backend: Backend::Zeroclaw,
            url: "http://127.0.0.1:42617".into(),
            token_env: None,
            token: Some("tok".into()),
            label: None,
            namespace_aliases: Vec::new(),
        };
        let ws = Workspace::instantiate(0, cfg).unwrap();
        assert_eq!(ws.id, 0);
        assert!(ws.cron.is_some());
        assert!(
            ws.is_activated(),
            "zeroclaw workspace should be activated at instantiate"
        );
        let arc = ws.client.as_ref().expect("client set");
        assert_eq!(Arc::strong_count(arc), 1);
    }

    #[test]
    fn instantiate_openclaw_leaves_client_none() {
        let cfg = WorkspaceConfig {
            id: None,
            name: "oc".into(),
            backend: Backend::Openclaw,
            url: "ws://127.0.0.1:18789".into(),
            token_env: None,
            token: None,
            label: None,
            namespace_aliases: Vec::new(),
        };
        let ws = Workspace::instantiate(0, cfg).unwrap();
        assert!(
            !ws.is_activated(),
            "openclaw client should be None until activate()"
        );
        assert!(ws.cron.is_none());
    }

    #[tokio::test]
    async fn activate_zeroclaw_is_noop_returns_ok() {
        let cfg = WorkspaceConfig {
            id: None,
            name: "w".into(),
            backend: Backend::Zeroclaw,
            url: "http://127.0.0.1:42617".into(),
            token_env: None,
            token: Some("tok".into()),
            label: None,
            namespace_aliases: Vec::new(),
        };
        let mut ws = Workspace::instantiate(0, cfg).unwrap();
        assert!(ws.is_activated());
        ws.activate().await.unwrap();
        assert!(ws.is_activated());
    }

    #[test]
    fn app_from_config_instantiates_zeroclaw_and_openclaw_skips_nemoclaw() {
        let cfg = AppConfig::parse(
            r#"
active = "oc"

[[workspaces]]
name = "z1"
backend = "zeroclaw"
url = "http://a"
token = "t"

[[workspaces]]
name = "oc"
backend = "openclaw"
url = "ws://c"

[[workspaces]]
name = "nc"
backend = "nemoclaw"
url = "ws://nc"
"#,
        )
        .unwrap();
        let app = App::from_config(cfg, PathBuf::from("/dev/null")).unwrap();
        // Nemoclaw still skipped at D-1a; zeroclaw + openclaw both instantiate.
        assert_eq!(app.workspaces.len(), 2);
        let names: Vec<_> = app
            .workspaces
            .iter()
            .map(|w| w.config.name.clone())
            .collect();
        assert_eq!(names, vec!["z1".to_string(), "oc".to_string()]);
        // `active = "oc"` resolves to the openclaw workspace.
        assert_eq!(app.active_workspace().unwrap().config.name, "oc");
        // But the openclaw workspace is not activated yet — activate()
        // runs async at a later stage.
        assert!(!app.active_workspace().unwrap().is_activated());
    }

    #[test]
    fn app_active_resolves_to_zeroclaw_when_named() {
        let cfg = AppConfig::parse(
            r#"
active = "z1"

[[workspaces]]
name = "z1"
backend = "zeroclaw"
url = "http://a"
token = "t"

[[workspaces]]
name = "oc"
backend = "openclaw"
url = "ws://c"
"#,
        )
        .unwrap();
        let app = App::from_config(cfg, PathBuf::from("/dev/null")).unwrap();
        assert_eq!(app.active_workspace().unwrap().config.name, "z1");
        assert!(app.active_workspace().unwrap().is_activated());
    }

    #[test]
    fn synthesize_single_zeroclaw_builds_one_activated_workspace() {
        let app =
            App::synthesize_single_zeroclaw("http://127.0.0.1:42617", Some("tok".to_string()))
                .unwrap();
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.active, 0);
        let ws = app.active_workspace().unwrap();
        assert_eq!(ws.config.name, "default");
        assert_eq!(ws.config.backend, Backend::Zeroclaw);
        assert!(ws.is_activated());
        assert!(ws.cron.is_some());
    }

    #[test]
    fn synthesize_single_zeroclaw_handles_missing_token() {
        let app = App::synthesize_single_zeroclaw("http://a", None).unwrap();
        assert_eq!(app.workspaces.len(), 1);
        assert!(app.active_workspace().unwrap().is_activated());
    }

    #[test]
    fn boot_or_synthesize_falls_back_when_config_missing() {
        // ZTERM_CONFIG_DIR pointed at a tempdir with no config.toml
        // means AppConfig::load returns an empty config, which
        // triggers the synthesize fallback path.
        let tmp = tempfile::TempDir::new().unwrap();
        let prior = std::env::var("ZTERM_CONFIG_DIR").ok();
        std::env::set_var("ZTERM_CONFIG_DIR", tmp.path());
        let app =
            App::boot_or_synthesize("http://127.0.0.1:42617", Some("tok".to_string())).unwrap();
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.active_workspace().unwrap().config.name, "default");
        match prior {
            Some(v) => std::env::set_var("ZTERM_CONFIG_DIR", v),
            None => std::env::remove_var("ZTERM_CONFIG_DIR"),
        }
    }

    #[test]
    fn boot_or_synthesize_prefers_config_workspaces() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
active = "primary"

[[workspaces]]
name = "primary"
backend = "zeroclaw"
url = "http://configured-url"
token = "configured-tok"
"#,
        )
        .unwrap();
        let prior = std::env::var("ZTERM_CONFIG_DIR").ok();
        std::env::set_var("ZTERM_CONFIG_DIR", tmp.path());
        let app = App::boot_or_synthesize("http://fallback-url", Some("fallback-tok".to_string()))
            .unwrap();
        // Config wins — synthesized workspace not used.
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.active_workspace().unwrap().config.name, "primary");
        assert_eq!(
            app.active_workspace().unwrap().config.url,
            "http://configured-url"
        );
        match prior {
            Some(v) => std::env::set_var("ZTERM_CONFIG_DIR", v),
            None => std::env::remove_var("ZTERM_CONFIG_DIR"),
        }
    }
    #[test]
    fn inventory_snapshot_matches_workspaces() {
        let app = App::synthesize_single_zeroclaw("http://a", Some("t".to_string())).unwrap();
        let inv = app.inventory();
        assert_eq!(inv.workspaces.len(), 1);
        assert_eq!(inv.active_index, 0);
        assert_eq!(inv.workspaces[0].name, "default");
        assert_eq!(inv.workspaces[0].backend, Backend::Zeroclaw);
        assert!(inv.workspaces[0].activated);
        assert!(inv.is_synthetic_singleton());
    }

    #[test]
    fn inventory_snapshot_active_pointer() {
        let cfg = AppConfig::parse(
            r#"
active = "second"

[[workspaces]]
name = "first"
backend = "zeroclaw"
url = "http://a"
token = "t"

[[workspaces]]
name = "second"
backend = "zeroclaw"
url = "http://b"
token = "t"
"#,
        )
        .unwrap();
        let app = App::from_config(cfg, PathBuf::from("/dev/null")).unwrap();
        let inv = app.inventory();
        assert_eq!(inv.active().unwrap().name, "second");
        assert!(!inv.is_synthetic_singleton());
    }
}
