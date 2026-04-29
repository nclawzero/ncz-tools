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
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::cli::agent::AgentClient;
use crate::cli::client::ZeroclawClient;
use crate::cli::url_safety::is_sensitive_url_query_key;

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

    fn resolved_zeroclaw_token(&self) -> Result<String> {
        match self.resolved_token() {
            Some(token) if !token.trim().is_empty() => Ok(token),
            Some(_) if zeroclaw_url_allows_blank_token(&self.url) => Ok(String::new()),
            _ => Err(missing_zeroclaw_token_error(self)),
        }
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
    /// If absent, the first successfully-instantiated workspace is used.
    /// If set, it must match a defined and instantiable workspace.
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
        apply_workspace_state(path, &mut cfg)?;
        cfg.validate()?;
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
        use std::collections::{HashMap, HashSet};
        let mut seen: HashSet<&str> = HashSet::new();
        let mut seen_ids: HashSet<&str> = HashSet::new();
        let mut seen_openclaw_namespaces: HashMap<String, String> = HashMap::new();
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
            if w.backend == Backend::Openclaw {
                for namespace in openclaw_primary_and_alias_namespaces(w) {
                    if let Some(existing) =
                        seen_openclaw_namespaces.insert(namespace.clone(), w.name.clone())
                    {
                        if existing != w.name {
                            return Err(anyhow!(
                                "openclaw session namespace '{}' is used by both workspace '{}' and '{}'",
                                namespace,
                                existing,
                                w.name
                            ));
                        }
                    }
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
        validate_workspace_url_safety(&config)?;
        match config.backend {
            Backend::Zeroclaw => {
                let token = config.resolved_zeroclaw_token()?;
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
        let client = openclaw_client_for_config(&self.config).await?;
        let boxed: Box<dyn AgentClient + Send + Sync> = Box::new(client);
        self.client = Some(Arc::new(Mutex::new(boxed)));
        Ok(())
    }

    /// Build the live OpenClaw client for a workspace config without
    /// mutating a `Workspace`. Runtime switch paths use this to avoid
    /// holding the shared `App` mutex across network activation.
    pub(crate) async fn activate_detached_client(
        config: &WorkspaceConfig,
    ) -> Result<Arc<Mutex<Box<dyn AgentClient + Send + Sync>>>> {
        match config.backend {
            Backend::Openclaw => {
                let client = openclaw_client_for_config(config).await?;
                let boxed: Box<dyn AgentClient + Send + Sync> = Box::new(client);
                Ok(Arc::new(Mutex::new(boxed)))
            }
            Backend::Zeroclaw => Err(anyhow!(
                "workspace '{}' (zeroclaw) should already be activated after instantiate; bug",
                config.name
            )),
            Backend::Nemoclaw => Err(anyhow!("nemoclaw backend is not yet implemented (v0.3)")),
        }
    }
}

async fn openclaw_client_for_config(
    config: &WorkspaceConfig,
) -> Result<crate::cli::openclaw::client::OpenClawClient> {
    use crate::cli::openclaw::client::{redacted_openclaw_url_for_error, OpenClawClient};
    use crate::cli::openclaw::device::DeviceIdentity;
    use crate::cli::openclaw::handshake::{ClientIdentity, HandshakeParams};

    validate_workspace_url_safety(config)?;
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
        token: config.resolved_token(),
    };

    let mut client = OpenClawClient::connect_and_handshake(&config.url, &device, &params)
        .await
        .with_context(|| {
            format!(
                "openclaw workspace '{}' connect+handshake to {}",
                config.name,
                redacted_openclaw_url_for_error(&config.url)
            )
        })?;
    client.set_session_namespace(openclaw_session_namespace(config));
    client.set_session_namespace_aliases(openclaw_session_namespace_aliases(config));

    Ok(client)
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
    let primary = openclaw_session_namespace(config);
    aliases.retain(|alias| alias != &primary);
    aliases
}

fn push_unique_namespace(namespaces: &mut Vec<String>, namespace: &str) {
    if !namespace.is_empty() && !namespaces.iter().any(|existing| existing == namespace) {
        namespaces.push(namespace.to_string());
    }
}

fn openclaw_primary_and_alias_namespaces(config: &WorkspaceConfig) -> Vec<String> {
    let mut namespaces = vec![openclaw_session_namespace(config)];
    for alias in &config.namespace_aliases {
        push_unique_namespace(&mut namespaces, alias.trim());
    }
    namespaces
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorkspaceState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    zeroclaw_workspaces: Vec<StableWorkspaceState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    openclaw_workspaces: Vec<OpenClawWorkspaceState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StableWorkspaceState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index: Option<usize>,
    name: String,
    url: String,
    id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    identity_aliases: Vec<OpenClawWorkspaceIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenClawWorkspaceState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index: Option<usize>,
    name: String,
    url: String,
    id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    identity_aliases: Vec<OpenClawWorkspaceIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    namespace_aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OpenClawWorkspaceIdentity {
    name: String,
    url: String,
}

fn workspace_state_path_for_config(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("workspace-state.toml")
}

struct WorkspaceStateLock {
    path: PathBuf,
    token: String,
}

impl Drop for WorkspaceStateLock {
    fn drop(&mut self) {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if workspace_state_lock_token(&text).as_deref() == Some(&self.token) => {
                if let Err(e) = std::fs::remove_file(&self.path) {
                    if e.kind() != ErrorKind::NotFound {
                        tracing::warn!(
                            "could not remove zterm workspace state lock {}: {e}",
                            self.path.display()
                        );
                    }
                }
            }
            Ok(_) | Err(_) => {
                // The lock was already removed or replaced after a stale-lock
                // recovery. Do not unlink a lock we no longer own.
            }
        }
    }
}

#[derive(Debug)]
struct WorkspaceStateLockMetadata {
    pid: Option<u32>,
    token: Option<String>,
}

const WORKSPACE_STATE_LOCK_METADATA_GRACE: Duration = Duration::from_secs(2);

fn workspace_state_lock_path(state_path: &Path) -> PathBuf {
    let name = state_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace-state.toml");
    state_path.with_file_name(format!(".{name}.lock"))
}

fn ensure_private_workspace_state_parent(state_path: &Path) -> Result<()> {
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating zterm state dir {}", parent.display()))?;
        harden_private_workspace_state_dir(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn harden_private_workspace_state_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(dir)
        .with_context(|| format!("checking zterm state dir {}", dir.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "refusing to use symlinked zterm state dir {}",
            dir.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(anyhow!(
            "zterm state path is not a directory: {}",
            dir.display()
        ));
    }
    if metadata.uid() != current_euid() {
        return Err(anyhow!(
            "refusing to use zterm state dir {} owned by uid {}",
            dir.display(),
            metadata.uid()
        ));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting private zterm state dir mode {}", dir.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn harden_private_workspace_state_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn current_euid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

fn open_private_workspace_state_create_new(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn harden_existing_workspace_state_file(path: &Path, label: &str) -> Result<bool> {
    let exists = harden_existing_workspace_state_file_impl(path, label)?;
    Ok(exists)
}

#[cfg(unix)]
fn harden_existing_workspace_state_file_impl(path: &Path, label: &str) -> Result<bool> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("checking zterm workspace {label} {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "refusing to use symlinked zterm workspace {label} {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(anyhow!(
            "zterm workspace {label} is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.uid() != current_euid() {
        return Err(anyhow!(
            "refusing to use zterm workspace {label} {} owned by uid {}",
            path.display(),
            metadata.uid()
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "setting private zterm workspace {label} mode {}",
            path.display()
        )
    })?;
    Ok(true)
}

#[cfg(not(unix))]
fn harden_existing_workspace_state_file_impl(path: &Path, _label: &str) -> Result<bool> {
    Ok(path.exists())
}

fn lock_workspace_state(state_path: &Path) -> Result<WorkspaceStateLock> {
    ensure_private_workspace_state_parent(state_path)?;

    let lock_path = workspace_state_lock_path(state_path);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match open_private_workspace_state_create_new(&lock_path) {
            Ok(mut file) => {
                let token = uuid::Uuid::new_v4().simple().to_string();
                if let Err(e) = write_workspace_state_lock_metadata(&mut file, &token) {
                    let _ = std::fs::remove_file(&lock_path);
                    return Err(e).with_context(|| {
                        format!(
                            "writing zterm workspace state lock metadata {}",
                            lock_path.display()
                        )
                    });
                }
                return Ok(WorkspaceStateLock {
                    path: lock_path,
                    token,
                });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists && Instant::now() < deadline => {
                if break_stale_workspace_state_lock(&lock_path)? {
                    continue;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Err(anyhow!(
                    "timed out waiting for zterm workspace state lock {}",
                    lock_path.display()
                ));
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "creating zterm workspace state lock {}",
                        lock_path.display()
                    )
                });
            }
        }
    }
}

fn write_workspace_state_lock_metadata(file: &mut File, token: &str) -> Result<()> {
    let created_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    writeln!(file, "pid = {}", std::process::id())?;
    writeln!(file, "created_unix_ms = {created_unix_ms}")?;
    writeln!(file, "token = \"{token}\"")?;
    file.sync_all()?;
    Ok(())
}

fn workspace_state_lock_token(text: &str) -> Option<String> {
    parse_workspace_state_lock_metadata(text).token
}

fn parse_workspace_state_lock_metadata(text: &str) -> WorkspaceStateLockMetadata {
    let mut pid = None;
    let mut token = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "pid" => {
                pid = value.trim().parse::<u32>().ok();
            }
            "token" => {
                token = Some(value.trim().trim_matches('"').to_string());
            }
            _ => {}
        }
    }
    WorkspaceStateLockMetadata { pid, token }
}

fn break_stale_workspace_state_lock(lock_path: &Path) -> Result<bool> {
    if !harden_existing_workspace_state_file(lock_path, "state lock")? {
        return Ok(true);
    }
    let text = match std::fs::read_to_string(lock_path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(true),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "reading zterm workspace state lock metadata {}",
                    lock_path.display()
                )
            });
        }
    };
    let metadata = parse_workspace_state_lock_metadata(&text);
    let modified_time = std::fs::metadata(lock_path)
        .and_then(|meta| meta.modified())
        .ok();
    let modified = modified_time
        .as_ref()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis());

    if let Some(pid) = metadata.pid {
        if process_exists(pid) {
            return Ok(false);
        }
    } else if modified_time
        .as_ref()
        .and_then(|mtime| SystemTime::now().duration_since(*mtime).ok())
        .map(|age| age < WORKSPACE_STATE_LOCK_METADATA_GRACE)
        .unwrap_or(true)
    {
        return Ok(false);
    }

    tracing::warn!(
        "breaking stale zterm workspace state lock {} (pid={:?}, mtime_unix_ms={:?})",
        lock_path.display(),
        metadata.pid,
        modified
    );
    match std::fs::remove_file(lock_path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e).with_context(|| {
            format!(
                "removing stale zterm workspace state lock {}",
                lock_path.display()
            )
        }),
    }
}

fn process_exists(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }

    #[cfg(unix)]
    {
        match std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
        {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                !stderr.contains("no such process")
            }
            Err(_) => true,
        }
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn load_workspace_state(path: &Path) -> Result<WorkspaceState> {
    if !harden_existing_workspace_state_file(path, "state sidecar")? {
        return Ok(WorkspaceState::default());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading zterm workspace state from {}", path.display()))?;
    toml::from_str(&text)
        .with_context(|| format!("parsing zterm workspace state from {}", path.display()))
}

fn save_workspace_state(path: &Path, state: &WorkspaceState) -> Result<()> {
    ensure_private_workspace_state_parent(path)?;
    let body =
        toml::to_string_pretty(state).with_context(|| "serializing zterm workspace state TOML")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace-state.toml");
    let tmp_path = path.with_file_name(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    {
        let mut tmp_file = open_private_workspace_state_create_new(&tmp_path)
            .with_context(|| format!("creating zterm workspace state {}", tmp_path.display()))?;
        use std::io::Write;
        tmp_file
            .write_all(body.as_bytes())
            .with_context(|| format!("writing zterm workspace state {}", tmp_path.display()))?;
        tmp_file
            .sync_all()
            .with_context(|| format!("syncing zterm workspace state {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "atomically replacing zterm workspace state {}",
            path.display()
        )
    })?;
    harden_existing_workspace_state_file(path, "state sidecar")?;
    if let Some(parent) = path.parent() {
        if let Ok(parent_dir) = File::open(parent) {
            parent_dir
                .sync_all()
                .with_context(|| format!("syncing zterm state dir {}", parent.display()))?;
        }
    }
    Ok(())
}

fn apply_workspace_state(config_path: &Path, cfg: &mut AppConfig) -> Result<()> {
    let has_openclaw = cfg
        .workspaces
        .iter()
        .any(|workspace| workspace.backend == Backend::Openclaw);
    let has_idless_zeroclaw = cfg.workspaces.iter().any(|workspace| {
        workspace.backend == Backend::Zeroclaw
            && workspace
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .is_none()
    });
    if !has_openclaw && !has_idless_zeroclaw {
        return Ok(());
    }

    let state_path = workspace_state_path_for_config(config_path);
    let _state_lock = lock_workspace_state(&state_path)?;
    let mut state = load_workspace_state(&state_path)?;
    let mut state_changed = false;
    let mut claimed_zeroclaw_entries = std::collections::HashSet::new();
    let mut zeroclaw_index = 0usize;

    for workspace in &mut cfg.workspaces {
        if workspace.backend != Backend::Zeroclaw {
            continue;
        }
        let workspace_index = zeroclaw_index;
        zeroclaw_index += 1;

        if workspace
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .is_some()
        {
            continue;
        }

        if let Some(entry_idx) = find_stable_workspace_state_entry(
            &state.zeroclaw_workspaces,
            workspace,
            workspace_index,
            &claimed_zeroclaw_entries,
        )? {
            if !claimed_zeroclaw_entries.insert(entry_idx) {
                return Err(anyhow!(
                    "zeroclaw workspace state entry {} matched more than one workspace",
                    entry_idx
                ));
            }
            let entry = &mut state.zeroclaw_workspaces[entry_idx];
            workspace.id = Some(entry.id.clone());
            state_changed |= refresh_stable_workspace_state_entry(
                entry,
                workspace_index,
                &workspace.name,
                &workspace.url,
            );
            continue;
        }

        if let Some(entry_idx) = find_plausible_stale_stable_workspace_state_entry(
            &state.zeroclaw_workspaces,
            workspace,
            workspace_index,
            &claimed_zeroclaw_entries,
        ) {
            let entry = &state.zeroclaw_workspaces[entry_idx];
            return Err(anyhow!(
                "zeroclaw workspace '{}' has no explicit id and partially matches persisted workspace-state entry {} \
                 (stored name='{}', url='{}', id='{}') at the same Zeroclaw index ({}). \
                 Refusing to reuse or mint a fresh id because this likely represents a renamed or URL-edited workspace. \
                 Add `id = \"{}\"` to the [[workspaces]] entry, or remove the stale entry from {} if this is a new workspace.",
                workspace.name,
                entry_idx,
                entry.name,
                entry.url,
                entry.id,
                workspace_index,
                entry.id,
                state_path.display()
            ));
        }

        let generated_id = format!("ws_{}", uuid::Uuid::new_v4().simple());
        let entry = StableWorkspaceState {
            index: Some(workspace_index),
            name: workspace.name.clone(),
            url: workspace.url.clone(),
            id: generated_id,
            identity_aliases: Vec::new(),
        };
        let mut next_state = state.clone();
        next_state.zeroclaw_workspaces.push(entry.clone());
        save_workspace_state(&state_path, &next_state).with_context(|| {
            format!(
                "could not persist generated zeroclaw workspace id for '{}' to {}; refusing to use an in-memory-only id",
                workspace.name,
                state_path.display()
            )
        })?;

        let verified_state = load_workspace_state(&state_path).with_context(|| {
            format!(
                "could not verify generated zeroclaw workspace id for '{}' in {}; refusing to use an in-memory-only id",
                workspace.name,
                state_path.display()
            )
        })?;
        let entry_idx = verified_state
            .zeroclaw_workspaces
            .iter()
            .position(|persisted| {
                persisted.id == entry.id
                    && workspace_identity_values_match(
                        &persisted.name,
                        &persisted.url,
                        &entry.name,
                        &entry.url,
                    )
            })
            .ok_or_else(|| {
                anyhow!(
                    "generated zeroclaw workspace id for '{}' was not present after persisting {}; refusing to use an in-memory-only id",
                    workspace.name,
                    state_path.display()
                )
            })?;

        let persisted = &verified_state.zeroclaw_workspaces[entry_idx];
        let persisted_id = persisted.id.clone();
        state = verified_state;
        claimed_zeroclaw_entries.insert(entry_idx);
        workspace.id = Some(persisted_id);
    }

    let mut claimed_state_entries = std::collections::HashSet::new();
    let configured_openclaw_identities = configured_openclaw_workspace_identities(cfg);
    let mut openclaw_index = 0usize;

    for workspace in &mut cfg.workspaces {
        if workspace.backend != Backend::Openclaw {
            continue;
        }
        let workspace_index = openclaw_index;
        openclaw_index += 1;

        if let Some(id) = workspace
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
        {
            if let Some(entry_idx) = state
                .openclaw_workspaces
                .iter()
                .position(|entry| entry.id == id)
            {
                if !claimed_state_entries.insert(entry_idx) {
                    return Err(anyhow!(
                        "openclaw workspace state entry {} matched more than one workspace",
                        entry_idx
                    ));
                }
                let entry = &state.openclaw_workspaces[entry_idx];
                for alias in &entry.namespace_aliases {
                    push_unique_namespace(&mut workspace.namespace_aliases, alias.trim());
                }
            }
            continue;
        }

        if let Some(entry_idx) =
            find_openclaw_workspace_state_entry(&state, workspace, &claimed_state_entries)?
        {
            if !claimed_state_entries.insert(entry_idx) {
                return Err(anyhow!(
                    "openclaw workspace state entry {} matched more than one workspace",
                    entry_idx
                ));
            }
            let entry = &mut state.openclaw_workspaces[entry_idx];
            workspace.id = Some(entry.id.clone());
            for alias in &entry.namespace_aliases {
                push_unique_namespace(&mut workspace.namespace_aliases, alias.trim());
            }
            state_changed |= refresh_openclaw_workspace_state_entry(
                entry,
                workspace_index,
                &workspace.name,
                &workspace.url,
            );
            continue;
        }

        if let Some(entry_idx) = find_plausible_stale_openclaw_workspace_state_entry(
            &state,
            workspace_index,
            &claimed_state_entries,
            &configured_openclaw_identities,
        ) {
            let entry = &state.openclaw_workspaces[entry_idx];
            return Err(anyhow!(
                "openclaw workspace '{}' has no explicit id and does not match persisted workspace-state entry {} \
                 (stored name='{}', url='{}', id='{}'), but that unclaimed entry has the same OpenClaw index ({}). \
                 Refusing to mint a fresh id because this likely represents a renamed or URL-edited workspace. \
                 Add `id = \"{}\"` and any needed `namespace_aliases` to the [[workspaces]] entry, or remove the stale entry from {} if this is a new workspace.",
                workspace.name,
                entry_idx,
                entry.name,
                entry.url,
                entry.id,
                workspace_index,
                entry.id,
                state_path.display()
            ));
        }

        let legacy_namespace = openclaw_legacy_session_namespace(workspace);
        let generated_id = format!("ws_{}", uuid::Uuid::new_v4().simple());
        let entry = OpenClawWorkspaceState {
            index: Some(workspace_index),
            name: workspace.name.clone(),
            url: workspace.url.clone(),
            id: generated_id.clone(),
            identity_aliases: Vec::new(),
            namespace_aliases: vec![legacy_namespace],
        };
        let mut next_state = state.clone();
        next_state.openclaw_workspaces.push(entry.clone());
        save_workspace_state(&state_path, &next_state).with_context(|| {
            format!(
                "could not persist generated openclaw workspace id for '{}' to {}; refusing to use an in-memory-only id",
                workspace.name,
                state_path.display()
            )
        })?;

        let verified_state = load_workspace_state(&state_path).with_context(|| {
            format!(
                "could not verify generated openclaw workspace id for '{}' in {}; refusing to use an in-memory-only id",
                workspace.name,
                state_path.display()
            )
        })?;
        let entry_idx = verified_state
            .openclaw_workspaces
            .iter()
            .position(|persisted| {
                persisted.id == entry.id
                    && openclaw_workspace_identity_values_match(
                        &persisted.name,
                        &persisted.url,
                        &entry.name,
                        &entry.url,
                    )
            })
            .ok_or_else(|| {
                anyhow!(
                    "generated openclaw workspace id for '{}' was not present after persisting {}; refusing to use an in-memory-only id",
                    workspace.name,
                    state_path.display()
                )
            })?;

        let persisted = &verified_state.openclaw_workspaces[entry_idx];
        let persisted_id = persisted.id.clone();
        let persisted_aliases = persisted.namespace_aliases.clone();
        state = verified_state;
        claimed_state_entries.insert(entry_idx);
        workspace.id = Some(persisted_id);
        for alias in &persisted_aliases {
            push_unique_namespace(&mut workspace.namespace_aliases, alias.trim());
        }
    }
    cfg.validate()?;
    if state_changed {
        if let Err(e) = save_workspace_state(&state_path, &state) {
            tracing::warn!("could not persist updated workspace state aliases: {e}");
        }
    }
    Ok(())
}

fn configured_openclaw_workspace_identities(cfg: &AppConfig) -> Vec<(String, String)> {
    cfg.workspaces
        .iter()
        .filter(|workspace| workspace.backend == Backend::Openclaw)
        .map(|workspace| (workspace.name.clone(), workspace.url.clone()))
        .collect()
}

fn normalize_workspace_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn find_stable_workspace_state_entry(
    entries: &[StableWorkspaceState],
    workspace: &WorkspaceConfig,
    workspace_index: usize,
    claimed_entries: &std::collections::HashSet<usize>,
) -> Result<Option<usize>> {
    let identity_matches: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(idx, entry)| {
            !claimed_entries.contains(idx)
                && stable_workspace_state_identity_matches(entry, workspace, workspace_index)
        })
        .map(|(idx, _)| idx)
        .collect();
    if identity_matches.len() > 1 {
        return Err(anyhow!(
            "zeroclaw workspace '{}' matches multiple persisted workspace-state entries",
            workspace.name
        ));
    }
    Ok(identity_matches.into_iter().next())
}

fn stable_workspace_state_identity_matches(
    entry: &StableWorkspaceState,
    workspace: &WorkspaceConfig,
    workspace_index: usize,
) -> bool {
    workspace_identity_matches(&entry.name, &entry.url, workspace)
        || entry
            .identity_aliases
            .iter()
            .any(|identity| workspace_identity_matches(&identity.name, &identity.url, workspace))
        || (entry.index == Some(workspace_index)
            && workspace_identity_matches(&entry.name, &entry.url, workspace))
}

fn find_plausible_stale_stable_workspace_state_entry(
    entries: &[StableWorkspaceState],
    workspace: &WorkspaceConfig,
    workspace_index: usize,
    claimed_entries: &std::collections::HashSet<usize>,
) -> Option<usize> {
    entries.iter().enumerate().position(|(idx, entry)| {
        !claimed_entries.contains(&idx)
            && entry.index == Some(workspace_index)
            && !stable_workspace_state_identity_matches(entry, workspace, workspace_index)
            && (entry.name == workspace.name
                || normalize_workspace_url(&entry.url) == normalize_workspace_url(&workspace.url)
                || entry.identity_aliases.iter().any(|identity| {
                    identity.name == workspace.name
                        || normalize_workspace_url(&identity.url)
                            == normalize_workspace_url(&workspace.url)
                }))
    })
}

fn find_openclaw_workspace_state_entry(
    state: &WorkspaceState,
    workspace: &WorkspaceConfig,
    claimed_state_entries: &std::collections::HashSet<usize>,
) -> Result<Option<usize>> {
    let identity_matches: Vec<usize> = state
        .openclaw_workspaces
        .iter()
        .enumerate()
        .filter(|(idx, entry)| {
            !claimed_state_entries.contains(idx)
                && openclaw_workspace_state_identity_matches(entry, workspace)
        })
        .map(|(idx, _)| idx)
        .collect();
    if identity_matches.len() > 1 {
        return Err(anyhow!(
            "openclaw workspace '{}' matches multiple persisted workspace-state entries",
            workspace.name
        ));
    }
    if let Some(idx) = identity_matches.into_iter().next() {
        return Ok(Some(idx));
    }

    Ok(None)
}

fn find_plausible_stale_openclaw_workspace_state_entry(
    state: &WorkspaceState,
    workspace_index: usize,
    claimed_state_entries: &std::collections::HashSet<usize>,
    configured_openclaw_identities: &[(String, String)],
) -> Option<usize> {
    state
        .openclaw_workspaces
        .iter()
        .enumerate()
        .find(|(idx, entry)| {
            !claimed_state_entries.contains(idx)
                && entry.index == Some(workspace_index)
                && !openclaw_state_entry_matches_any_configured_workspace(
                    entry,
                    configured_openclaw_identities,
                )
        })
        .map(|(idx, _)| idx)
}

fn openclaw_state_entry_matches_any_configured_workspace(
    entry: &OpenClawWorkspaceState,
    configured_openclaw_identities: &[(String, String)],
) -> bool {
    configured_openclaw_identities.iter().any(|(name, url)| {
        openclaw_workspace_identity_values_match(&entry.name, &entry.url, name, url)
            || entry.identity_aliases.iter().any(|identity| {
                openclaw_workspace_identity_values_match(&identity.name, &identity.url, name, url)
            })
    })
}

fn openclaw_workspace_state_identity_matches(
    entry: &OpenClawWorkspaceState,
    workspace: &WorkspaceConfig,
) -> bool {
    openclaw_workspace_identity_matches(&entry.name, &entry.url, workspace)
        || entry.identity_aliases.iter().any(|identity| {
            openclaw_workspace_identity_matches(&identity.name, &identity.url, workspace)
        })
}

fn openclaw_workspace_identity_matches(name: &str, url: &str, workspace: &WorkspaceConfig) -> bool {
    workspace_identity_matches(name, url, workspace)
}

fn workspace_identity_matches(name: &str, url: &str, workspace: &WorkspaceConfig) -> bool {
    name == workspace.name
        && normalize_workspace_url(url) == normalize_workspace_url(&workspace.url)
}

fn refresh_stable_workspace_state_entry(
    entry: &mut StableWorkspaceState,
    workspace_index: usize,
    workspace_name: &str,
    workspace_url: &str,
) -> bool {
    let mut changed = false;
    if entry.index != Some(workspace_index) {
        entry.index = Some(workspace_index);
        changed = true;
    }
    if !workspace_identity_values_match(&entry.name, &entry.url, workspace_name, workspace_url) {
        let previous = OpenClawWorkspaceIdentity {
            name: entry.name.clone(),
            url: entry.url.clone(),
        };
        if !entry.identity_aliases.iter().any(|identity| {
            workspace_identity_values_match(
                &identity.name,
                &identity.url,
                &previous.name,
                &previous.url,
            )
        }) {
            entry.identity_aliases.push(previous);
        }
        entry.name = workspace_name.to_string();
        entry.url = workspace_url.to_string();
        changed = true;
    }
    changed
}

fn workspace_identity_values_match(
    left_name: &str,
    left_url: &str,
    right_name: &str,
    right_url: &str,
) -> bool {
    left_name == right_name
        && normalize_workspace_url(left_url) == normalize_workspace_url(right_url)
}

fn refresh_openclaw_workspace_state_entry(
    entry: &mut OpenClawWorkspaceState,
    workspace_index: usize,
    workspace_name: &str,
    workspace_url: &str,
) -> bool {
    let mut changed = false;
    if entry.index != Some(workspace_index) {
        entry.index = Some(workspace_index);
        changed = true;
    }
    if !openclaw_workspace_identity_values_match(
        &entry.name,
        &entry.url,
        workspace_name,
        workspace_url,
    ) {
        let previous = OpenClawWorkspaceIdentity {
            name: entry.name.clone(),
            url: entry.url.clone(),
        };
        if !entry.identity_aliases.iter().any(|identity| {
            openclaw_workspace_identity_values_match(
                &identity.name,
                &identity.url,
                &previous.name,
                &previous.url,
            )
        }) {
            entry.identity_aliases.push(previous);
        }
        entry.name = workspace_name.to_string();
        entry.url = workspace_url.to_string();
        changed = true;
    }
    changed
}

fn openclaw_workspace_identity_values_match(
    left_name: &str,
    left_url: &str,
    right_name: &str,
    right_url: &str,
) -> bool {
    left_name == right_name
        && normalize_workspace_url(left_url) == normalize_workspace_url(right_url)
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
    /// logs (via tracing) and skips inactive entries that require async.
    /// Honors `config.active` when set; an unloadable active workspace
    /// fails closed instead of silently selecting another workspace.
    pub fn from_config(cfg: AppConfig, config_path: PathBuf) -> Result<Self> {
        validate_workspace_urls(&cfg)?;
        validate_zeroclaw_workspace_tokens(&cfg)?;

        let configured_active = cfg.active.clone();
        let mut workspaces: Vec<Workspace> = Vec::new();
        for (idx, wc) in cfg.workspaces.into_iter().enumerate() {
            match Workspace::instantiate(workspaces.len(), wc.clone()) {
                Ok(w) => workspaces.push(w),
                Err(e) => {
                    if configured_active.as_deref() == Some(wc.name.as_str()) {
                        return Err(anyhow!(
                            "active workspace '{}' failed to instantiate: {e}",
                            wc.name
                        ));
                    }
                    tracing::warn!(
                        "workspace '{}' (#{}) skipped at D1 instantiation: {e}",
                        wc.name,
                        idx
                    );
                }
            }
        }

        let active = match configured_active {
            Some(name) => workspaces
                .iter()
                .position(|w| w.config.name == name)
                .ok_or_else(|| anyhow!("active workspace '{name}' could not be instantiated"))?,
            None => 0,
        };

        Ok(Self {
            workspaces,
            active,
            shared_mnemos: crate::cli::mnemos::MnemosClient::from_env(),
            config_path,
        })
    }

    pub fn from_config_with_cli_token_override(
        mut cfg: AppConfig,
        config_path: PathBuf,
        token_override: Option<String>,
        workspace_override: Option<&str>,
    ) -> Result<Self> {
        apply_cli_token_override(&mut cfg, token_override.as_deref(), workspace_override)?;
        Self::from_config(cfg, config_path)
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
        let token = Some(token.unwrap_or_default());
        let config_path = AppConfig::default_path()
            .unwrap_or_else(|_| std::path::PathBuf::from("./.zterm-synthetic.toml"));
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
        validate_workspace_url_safety(&cfg)?;
        let mut app_cfg = AppConfig {
            active: Some("default".to_string()),
            workspaces: vec![cfg],
        };
        apply_workspace_state(&config_path, &mut app_cfg)?;
        let cfg = app_cfg
            .workspaces
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("synthetic zeroclaw workspace config disappeared"))?;
        let ws = Workspace::instantiate(0, cfg)?;
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
        Self::boot_or_synthesize_with_cli_token_override(remote, token, None, None)
    }

    pub fn boot_or_synthesize_with_cli_token_override(
        remote: impl Into<String>,
        token: Option<String>,
        token_override: Option<String>,
        workspace_override: Option<&str>,
    ) -> Result<Self> {
        let path = AppConfig::default_path()?;
        let cfg = AppConfig::load(&path)?;
        if !cfg.workspaces.is_empty() {
            return Self::from_config_with_cli_token_override(
                cfg,
                path,
                token_override,
                workspace_override,
            );
        }
        Self::synthesize_single_zeroclaw(remote, token)
    }
}

fn validate_workspace_urls(cfg: &AppConfig) -> Result<()> {
    for workspace in &cfg.workspaces {
        validate_workspace_url_safety(workspace)?;
    }
    Ok(())
}

fn validate_workspace_url_safety(config: &WorkspaceConfig) -> Result<()> {
    let Ok(url) = reqwest::Url::parse(config.url.trim()) else {
        return Ok(());
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!(
            "workspace `{}` url must not embed username/password credentials; use token_env or token instead",
            config.name
        ));
    }
    if let Some(key) = url
        .query_pairs()
        .map(|(key, _)| key.into_owned())
        .find(|key| is_sensitive_url_query_key(key))
    {
        return Err(anyhow!(
            "workspace `{}` url must not contain sensitive query parameter `{key}`; use token_env or token instead",
            config.name
        ));
    }
    Ok(())
}

fn validate_zeroclaw_workspace_tokens(cfg: &AppConfig) -> Result<()> {
    for workspace in &cfg.workspaces {
        if workspace.backend == Backend::Zeroclaw {
            workspace.resolved_zeroclaw_token()?;
        }
    }
    Ok(())
}

fn missing_zeroclaw_token_error(config: &WorkspaceConfig) -> anyhow::Error {
    let source = match config.resolved_token() {
        Some(token) if token.trim().is_empty() => format!(
            "resolved token is blank and url `{}` is not localhost/loopback",
            config.url
        ),
        Some(_) => "resolved token is unusable".to_string(),
        None => match config.token_env.as_deref().filter(|name| !name.is_empty()) {
            Some(name) => {
                format!("token_env `{name}` is unset or empty and no inline token is set")
            }
            None => "no token_env or inline token is set".to_string(),
        },
    };
    anyhow!(
        "zeroclaw workspace `{}` has no resolved token ({source}); set token_env, set token, pass --token for the selected workspace, or set token = \"\" only for an explicitly unauthenticated local gateway",
        config.name
    )
}

fn zeroclaw_url_allows_blank_token(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn apply_cli_token_override(
    cfg: &mut AppConfig,
    token_override: Option<&str>,
    workspace_override: Option<&str>,
) -> Result<()> {
    let Some(token) = token_override.filter(|token| !token.is_empty()) else {
        return Ok(());
    };
    let selected = if let Some(name) = workspace_override.filter(|name| !name.is_empty()) {
        cfg.workspaces
            .iter()
            .position(|workspace| workspace.name == name)
            .ok_or_else(|| {
                anyhow!(
                    "workspace '{}' not found; refusing to apply CLI token to a different workspace",
                    name
                )
            })?
    } else {
        cfg.active
            .as_deref()
            .and_then(|name| {
                cfg.workspaces
                    .iter()
                    .position(|workspace| workspace.name == name)
            })
            .unwrap_or(0)
    };
    if let Some(workspace) = cfg.workspaces.get_mut(selected) {
        workspace.token_env = None;
        workspace.token = Some(token.to_string());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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
    fn load_preserves_primary_openclaw_config_bytes_and_recovers_id_from_sidecar() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = br#"# user comment before unknown top-level state
unknown_top_level = "keep me byte-for-byte"

[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
unknown_workspace_field = { nested = "also ignored" }
"#;
        std::fs::write(&path, original).unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let workspace = &cfg.workspaces[0];
        let id = workspace
            .id
            .as_deref()
            .expect("state sidecar should assign id");
        assert!(id.starts_with("ws_"));
        assert!(workspace
            .namespace_aliases
            .iter()
            .any(|alias| { alias == "backend=openclaw;workspace=alpha;url=ws://old.example" }));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        let state = load_workspace_state(&workspace_state_path_for_config(&path)).unwrap();
        assert_eq!(state.openclaw_workspaces.len(), 1);
        assert_eq!(state.openclaw_workspaces[0].id, id);

        let reloaded = AppConfig::load(&path).unwrap();
        assert_eq!(reloaded.workspaces[0].id.as_deref(), Some(id));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_state_sidecar_uses_private_permissions_and_hardens_existing() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
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

        AppConfig::load(&path).unwrap();

        let state_path = workspace_state_path_for_config(&path);
        assert_eq!(
            std::fs::metadata(tmp.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        AppConfig::load(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_state_sidecar_rejects_symlink() {
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
        let state_path = workspace_state_path_for_config(&path);
        let target = tmp.path().join("target-state.toml");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, &state_path).unwrap();

        let err = AppConfig::load(&path).unwrap_err();

        assert!(err.to_string().contains("symlinked zterm workspace"));
    }

    #[test]
    fn readonly_primary_openclaw_config_loads_with_generated_sidecar_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = br#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
"#;
        std::fs::write(&path, original).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let workspace = &cfg.workspaces[0];
        let id = workspace
            .id
            .as_deref()
            .expect("state migration should assign id");
        assert!(id.starts_with("ws_"));
        assert!(workspace
            .namespace_aliases
            .iter()
            .any(|alias| { alias == "backend=openclaw;workspace=alpha;url=ws://old.example" }));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        let reloaded = AppConfig::load(&path).unwrap();
        assert_eq!(reloaded.workspaces[0].id.as_deref(), Some(id));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn generated_openclaw_id_errors_when_workspace_state_path_is_unsafe() {
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
        let state_path = workspace_state_path_for_config(&path);
        std::fs::create_dir(&state_path).unwrap();

        let err = AppConfig::load(&path).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("state sidecar is not a regular file"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn load_preserves_private_config_permissions_when_generating_openclaw_sidecar_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = br#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
token = "inline-secret-token"
"#;
        std::fs::write(&path, original).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let cfg = AppConfig::load(&path).unwrap();

        assert!(cfg.workspaces[0].id.as_deref().unwrap().starts_with("ws_"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn explicit_openclaw_workspace_id_survives_lost_state_sidecar() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[workspaces]]
id = "ws_explicit"
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
"#,
        )
        .unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let id = cfg.workspaces[0].id.clone().unwrap();
        let namespace = openclaw_session_namespace(&cfg.workspaces[0]);
        assert_eq!(namespace, format!("backend=openclaw;workspace_id={id}"));

        let _ = std::fs::remove_file(workspace_state_path_for_config(&path));

        let reloaded = AppConfig::load(&path).unwrap();
        assert_eq!(reloaded.workspaces[0].id.as_deref(), Some(id.as_str()));
        assert_eq!(
            openclaw_session_namespace(&reloaded.workspaces[0]),
            namespace
        );
    }

    #[test]
    fn concurrent_loads_converge_on_one_persisted_openclaw_workspace_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = r#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
"#;
        std::fs::write(&path, original).unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let cfg = AppConfig::load(&path).unwrap();
                cfg.workspaces[0].id.clone().unwrap()
            }));
        }

        let ids: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(ids[0], ids[1]);

        let state = load_workspace_state(&workspace_state_path_for_config(&path)).unwrap();
        assert_eq!(state.openclaw_workspaces.len(), 1);
        assert_eq!(state.openclaw_workspaces[0].id, ids[0]);
    }

    #[test]
    fn load_recovers_from_stale_workspace_state_lock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = r#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
"#;
        std::fs::write(&path, original).unwrap();

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("__zterm_no_such_test__")
            .spawn()
            .unwrap();
        let stale_pid = child.id();
        child.wait().unwrap();

        let lock_path = workspace_state_lock_path(&workspace_state_path_for_config(&path));
        std::fs::write(
            &lock_path,
            format!("pid = {stale_pid}\ncreated_unix_ms = 1\ntoken = \"stale\"\n"),
        )
        .unwrap();

        let cfg = AppConfig::load(&path).unwrap();

        assert!(cfg.workspaces[0].id.as_deref().unwrap().starts_with("ws_"));
        assert!(!lock_path.exists());
    }

    #[test]
    fn zeroclaw_load_assigns_sidecar_id_without_openclaw() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = r#"
[[workspaces]]
name = "alpha"
backend = "zeroclaw"
url = "http://localhost:8080"
token = ""
"#;
        std::fs::write(&path, original).unwrap();

        let lock_path = workspace_state_lock_path(&workspace_state_path_for_config(&path));

        let cfg = AppConfig::load(&path).unwrap();
        let state = load_workspace_state(&workspace_state_path_for_config(&path)).unwrap();

        assert_eq!(cfg.workspaces[0].backend, Backend::Zeroclaw);
        assert!(cfg.workspaces[0].id.as_deref().unwrap().starts_with("ws_"));
        assert_eq!(state.zeroclaw_workspaces.len(), 1);
        assert!(!lock_path.exists());
    }

    #[test]
    fn state_backed_openclaw_id_fails_closed_on_rename_without_config_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = r#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
"#;
        std::fs::write(&path, original).unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let id = cfg.workspaces[0].id.clone().unwrap();
        let old_namespace = "backend=openclaw;workspace=alpha;url=ws://old.example";
        assert!(cfg.workspaces[0]
            .namespace_aliases
            .iter()
            .any(|alias| alias == old_namespace));

        let renamed = r#"
[[workspaces]]
name = "renamed"
backend = "openclaw"
url = "ws://new.example"
"#;
        std::fs::write(&path, renamed).unwrap();

        let err = AppConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Refusing to mint a fresh id"));
        assert!(msg.contains("same OpenClaw index (0)"));
        assert!(msg.contains(&format!("id='{id}'")));
        assert!(msg.contains("Add `id = "));
        assert!(msg.contains("namespace_aliases"));
        assert!(msg.contains("remove the stale entry"));

        let state = load_workspace_state(&workspace_state_path_for_config(&path)).unwrap();
        let old_entry = state
            .openclaw_workspaces
            .iter()
            .find(|entry| entry.id == id)
            .expect("old state entry remains unclaimed");
        assert_eq!(old_entry.namespace_aliases, vec![old_namespace.to_string()]);
        assert_eq!(state.openclaw_workspaces.len(), 1);
    }

    #[test]
    fn openclaw_remove_then_add_at_same_index_fails_closed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = r#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://old.example"
"#;
        std::fs::write(&path, original).unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let old_id = cfg.workspaces[0].id.clone().unwrap();

        let replacement = r#"
[[workspaces]]
name = "beta"
backend = "openclaw"
url = "ws://new.example"
"#;
        std::fs::write(&path, replacement).unwrap();

        let err = AppConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Refusing to mint a fresh id"));
        assert!(msg.contains("same OpenClaw index (0)"));
        assert!(msg.contains(&format!("id='{old_id}'")));
        assert!(msg.contains("Add `id = "));
        assert!(msg.contains("remove the stale entry"));

        let state = load_workspace_state(&workspace_state_path_for_config(&path)).unwrap();
        assert!(state
            .openclaw_workspaces
            .iter()
            .any(|entry| entry.id == old_id));
        assert_eq!(state.openclaw_workspaces.len(), 1);
    }

    #[test]
    fn openclaw_reorder_and_rename_does_not_inherit_index_state_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = r#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://a.example"

[[workspaces]]
name = "beta"
backend = "openclaw"
url = "ws://b.example"
"#;
        std::fs::write(&path, original).unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let alpha_id = cfg.workspaces[0].id.clone().unwrap();
        let beta_id = cfg.workspaces[1].id.clone().unwrap();

        let reordered_and_renamed = r#"
[[workspaces]]
name = "gamma"
backend = "openclaw"
url = "ws://b.example"

[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://a.example"
"#;
        std::fs::write(&path, reordered_and_renamed).unwrap();

        let reloaded = AppConfig::load(&path).unwrap();
        let gamma = &reloaded.workspaces[0];
        let alpha = &reloaded.workspaces[1];
        let gamma_id = gamma.id.as_deref().unwrap();
        assert_ne!(gamma_id, alpha_id);
        assert_ne!(gamma_id, beta_id);
        assert_eq!(alpha.id.as_deref(), Some(alpha_id.as_str()));
        assert!(gamma
            .namespace_aliases
            .iter()
            .any(|alias| alias == "backend=openclaw;workspace=gamma;url=ws://b.example"));
        assert!(!gamma
            .namespace_aliases
            .iter()
            .any(|alias| alias == "backend=openclaw;workspace=alpha;url=ws://a.example"));
        assert!(!gamma
            .namespace_aliases
            .iter()
            .any(|alias| alias == "backend=openclaw;workspace=beta;url=ws://b.example"));
    }

    #[test]
    fn zeroclaw_idless_rename_same_url_fails_closed() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let prior = std::env::var("ZTERM_CONFIG_DIR").ok();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("ZTERM_CONFIG_DIR", tmp.path());
        let path = tmp.path().join("config.toml");
        let original = r#"
[[workspaces]]
name = "alpha"
backend = "zeroclaw"
url = "http://127.0.0.1:42617"
token = ""
"#;
        std::fs::write(&path, original).unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let id = cfg.workspaces[0].id.clone().unwrap();

        let renamed = r#"
[[workspaces]]
name = "renamed"
backend = "zeroclaw"
url = "http://127.0.0.1:42617"
token = ""
"#;
        std::fs::write(&path, renamed).unwrap();

        let err = AppConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("partially matches persisted workspace-state entry"));
        assert!(msg.contains("same Zeroclaw index (0)"));
        assert!(msg.contains(&format!("id='{id}'")));
        assert!(msg.contains("Add `id = "));

        match prior {
            Some(value) => std::env::set_var("ZTERM_CONFIG_DIR", value),
            None => std::env::remove_var("ZTERM_CONFIG_DIR"),
        }
    }

    #[test]
    fn zeroclaw_explicit_workspace_id_survives_rename_and_preserves_incomplete_marker() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let prior = std::env::var("ZTERM_CONFIG_DIR").ok();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("ZTERM_CONFIG_DIR", tmp.path());
        let path = tmp.path().join("config.toml");
        let original = r#"
[[workspaces]]
name = "alpha"
backend = "zeroclaw"
url = "http://127.0.0.1:42617"
token = ""
"#;
        std::fs::write(&path, original).unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let id = cfg.workspaces[0].id.clone().unwrap();
        let before = crate::cli::storage::workspace_scope("zeroclaw", "alpha", Some(&id)).unwrap();
        crate::cli::storage::mark_scoped_session_history_incomplete(
            &before,
            "main",
            "unknown turn outcome",
        )
        .unwrap();

        let renamed = format!(
            r#"
[[workspaces]]
id = "{id}"
name = "renamed"
backend = "zeroclaw"
url = "http://127.0.0.1:42617"
token = ""
"#
        );
        std::fs::write(&path, renamed).unwrap();

        let reloaded = AppConfig::load(&path).unwrap();
        assert_eq!(reloaded.workspaces[0].id.as_deref(), Some(id.as_str()));
        let after = crate::cli::storage::workspace_scope("zeroclaw", "renamed", Some(&id)).unwrap();
        assert!(crate::cli::storage::scoped_session_history_is_incomplete(&after, "main").unwrap());

        match prior {
            Some(value) => std::env::set_var("ZTERM_CONFIG_DIR", value),
            None => std::env::remove_var("ZTERM_CONFIG_DIR"),
        }
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
        assert!(!aliases
            .iter()
            .any(|alias| alias == "backend=openclaw;workspace=renamed;url=ws://new.example"));
    }

    #[test]
    fn parse_rejects_openclaw_alias_vs_alias_collision() {
        let text = r#"
[[workspaces]]
name = "alpha"
backend = "openclaw"
url = "ws://a"
namespace_aliases = ["backend=openclaw;workspace=legacy;url=ws://shared"]

[[workspaces]]
name = "beta"
backend = "openclaw"
url = "ws://b"
namespace_aliases = ["backend=openclaw;workspace=legacy;url=ws://shared"]
"#;
        let err = AppConfig::parse(text).unwrap_err();
        assert!(err.to_string().contains("openclaw session namespace"));
        assert!(err.to_string().contains("alpha"));
        assert!(err.to_string().contains("beta"));
    }

    #[test]
    fn parse_rejects_openclaw_primary_vs_alias_collision() {
        let text = r#"
[[workspaces]]
id = "ws_alpha"
name = "alpha"
backend = "openclaw"
url = "ws://a"

[[workspaces]]
id = "ws_beta"
name = "beta"
backend = "openclaw"
url = "ws://b"
namespace_aliases = ["backend=openclaw;workspace_id=ws_alpha"]
"#;
        let err = AppConfig::parse(text).unwrap_err();
        assert!(err
            .to_string()
            .contains("backend=openclaw;workspace_id=ws_alpha"));
    }

    #[test]
    fn resolved_token_prefers_env_var() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
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

    #[test]
    fn instantiate_rejects_workspace_url_with_embedded_credentials() {
        let cfg = WorkspaceConfig {
            id: None,
            name: "oc".into(),
            backend: Backend::Openclaw,
            url: "ws://operator:secret@127.0.0.1:18789".into(),
            token_env: None,
            token: None,
            label: None,
            namespace_aliases: Vec::new(),
        };

        let err = Workspace::instantiate(0, cfg).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("url must not embed username/password credentials"));
        assert!(!msg.contains("secret"));
    }

    #[test]
    fn app_from_config_rejects_workspace_url_with_sensitive_query_key() {
        let cfg = AppConfig::parse(
            r#"
[[workspaces]]
name = "oc"
backend = "openclaw"
url = "ws://127.0.0.1:18789/ws?client_secret=credential-value&room=alpha"
"#,
        )
        .unwrap();

        let err = match App::from_config(cfg, PathBuf::from("/dev/null")) {
            Ok(_) => panic!("sensitive query key should fail closed"),
            Err(err) => err,
        };

        let msg = err.to_string();
        assert!(msg.contains("sensitive query parameter `client_secret`"));
        assert!(!msg.contains("credential-value"));
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
    fn app_from_config_rejects_unloadable_active_workspace() {
        let cfg = AppConfig::parse(
            r#"
active = "nc"

[[workspaces]]
name = "z1"
backend = "zeroclaw"
url = "http://a"
token = "t"

[[workspaces]]
name = "nc"
backend = "nemoclaw"
url = "ws://nc"
"#,
        )
        .unwrap();

        let err = match App::from_config(cfg, PathBuf::from("/dev/null")) {
            Ok(_) => panic!("unloadable active workspace should fail closed"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("active workspace 'nc' failed to instantiate"));
        assert!(msg.contains("nemoclaw backend is declared in config but not yet implemented"));
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
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

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

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn synthesize_single_zeroclaw_handles_missing_token() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let app = App::synthesize_single_zeroclaw("http://127.0.0.1:42617", None).unwrap();
        assert_eq!(app.workspaces.len(), 1);
        assert!(app.active_workspace().unwrap().is_activated());

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn synthesize_single_zeroclaw_rejects_missing_token_for_remote_url() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let err = match App::synthesize_single_zeroclaw("http://example.com:42617", None) {
            Ok(_) => panic!("remote synthetic zeroclaw without token should fail closed"),
            Err(err) => err,
        };

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        assert!(err.to_string().contains("resolved token is blank"));
        assert!(err.to_string().contains("not localhost/loopback"));
    }

    #[test]
    fn synthesize_single_zeroclaw_rejects_credential_bearing_remote_url() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let err = match App::synthesize_single_zeroclaw(
            "https://operator:embedded-password@example.com/ws?token=url-token",
            Some("bearer-token".to_string()),
        ) {
            Ok(_) => panic!("credential-bearing synthetic zeroclaw URL should fail closed"),
            Err(err) => err,
        };

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        let msg = err.to_string();
        assert!(msg.contains("url must not embed username/password credentials"));
        for leaked in ["operator", "embedded-password", "url-token"] {
            assert!(!msg.contains(leaked), "{leaked} leaked in {msg}");
        }

        let query_err = match App::synthesize_single_zeroclaw(
            "https://example.com/ws?refresh_token=url-token",
            Some("bearer-token".to_string()),
        ) {
            Ok(_) => panic!("token query synthetic zeroclaw URL should fail closed"),
            Err(err) => err,
        };
        let query_msg = query_err.to_string();
        assert!(query_msg.contains("sensitive query parameter `refresh_token`"));
        assert!(!query_msg.contains("url-token"));
    }

    #[test]
    fn from_config_rejects_zeroclaw_workspace_without_token() {
        let cfg = AppConfig::parse(
            r#"
[[workspaces]]
name = "z1"
backend = "zeroclaw"
url = "http://a"
"#,
        )
        .unwrap();

        let err = match App::from_config(cfg, PathBuf::from("/dev/null")) {
            Ok(_) => panic!("missing zeroclaw token should fail closed"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("zeroclaw workspace `z1`"));
        assert!(err.to_string().contains("no resolved token"));
    }

    #[test]
    fn from_config_rejects_unresolved_zeroclaw_token_env() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        std::env::remove_var("ZTERM_TEST_MISSING_TOKEN_ENV");
        let cfg = AppConfig::parse(
            r#"
[[workspaces]]
name = "z1"
backend = "zeroclaw"
url = "http://a"
token_env = "ZTERM_TEST_MISSING_TOKEN_ENV"
"#,
        )
        .unwrap();

        let err = match App::from_config(cfg, PathBuf::from("/dev/null")) {
            Ok(_) => panic!("unresolved zeroclaw token_env should fail closed"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("token_env `ZTERM_TEST_MISSING_TOKEN_ENV` is unset or empty"));
    }

    #[test]
    fn from_config_allows_explicit_empty_zeroclaw_token_for_local_unauth() {
        let cfg = AppConfig::parse(
            r#"
[[workspaces]]
name = "local"
backend = "zeroclaw"
url = "http://127.0.0.1:42617"
token = ""
"#,
        )
        .unwrap();

        let app = App::from_config(cfg, PathBuf::from("/dev/null")).unwrap();
        assert_eq!(app.workspaces.len(), 1);
        assert!(app.active_workspace().unwrap().is_activated());
    }

    #[test]
    fn from_config_rejects_explicit_empty_zeroclaw_token_for_remote_url() {
        let cfg = AppConfig::parse(
            r#"
[[workspaces]]
name = "remote"
backend = "zeroclaw"
url = "http://example.com:42617"
token = ""
"#,
        )
        .unwrap();

        let err = match App::from_config(cfg, PathBuf::from("/dev/null")) {
            Ok(_) => panic!("remote blank zeroclaw token should fail closed"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("resolved token is blank"));
        assert!(err.to_string().contains("not localhost/loopback"));
    }

    #[test]
    fn from_config_rejects_unset_token_env_with_blank_inline_remote_fallback() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        std::env::remove_var("ZTERM_TEST_BLANK_FALLBACK_TOKEN_ENV");
        let cfg = AppConfig::parse(
            r#"
[[workspaces]]
name = "remote"
backend = "zeroclaw"
url = "http://example.com:42617"
token_env = "ZTERM_TEST_BLANK_FALLBACK_TOKEN_ENV"
token = ""
"#,
        )
        .unwrap();

        let err = match App::from_config(cfg, PathBuf::from("/dev/null")) {
            Ok(_) => panic!("blank inline fallback should fail closed for remote zeroclaw"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("resolved token is blank"));
        assert!(err.to_string().contains("not localhost/loopback"));
    }

    #[test]
    fn boot_or_synthesize_falls_back_when_config_missing() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
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
        assert!(app
            .active_workspace()
            .unwrap()
            .config
            .id
            .as_deref()
            .is_some_and(|id| id.starts_with("ws_")));
        match prior {
            Some(v) => std::env::set_var("ZTERM_CONFIG_DIR", v),
            None => std::env::remove_var("ZTERM_CONFIG_DIR"),
        }
    }

    #[test]
    fn boot_or_synthesize_prefers_config_workspaces() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
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

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn boot_or_synthesize_cli_token_override_applies_to_requested_workspace() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/api/config")
            .match_header("authorization", "Bearer cli-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "agent": { "model": "m", "provider": "p" }
                })
                .to_string(),
            )
            .create_async()
            .await;
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
active = "primary"

[[workspaces]]
name = "primary"
backend = "zeroclaw"
url = "http://primary.invalid"
token = "primary-token"

[[workspaces]]
name = "target"
backend = "zeroclaw"
url = "{}"
token_env = "ZTERM_TEST_STALE_TOKEN"
token = "stale-token"
"#,
                server.url()
            ),
        )
        .unwrap();
        let prior_config = std::env::var("ZTERM_CONFIG_DIR").ok();
        let prior_stale = std::env::var("ZTERM_TEST_STALE_TOKEN").ok();
        std::env::set_var("ZTERM_CONFIG_DIR", tmp.path());
        std::env::set_var("ZTERM_TEST_STALE_TOKEN", "env-stale-token");

        let app = App::boot_or_synthesize_with_cli_token_override(
            "http://fallback-url",
            Some("fallback-tok".to_string()),
            Some("cli-token".to_string()),
            Some("target"),
        )
        .unwrap();

        let target = app
            .workspaces
            .iter()
            .find(|workspace| workspace.config.name == "target")
            .expect("target workspace should load");
        assert_eq!(target.config.token.as_deref(), Some("cli-token"));
        assert!(target.config.token_env.is_none());
        let client = target.client.clone().expect("zeroclaw target is activated");
        client.lock().await.get_config().await.unwrap();

        match prior_config {
            Some(v) => std::env::set_var("ZTERM_CONFIG_DIR", v),
            None => std::env::remove_var("ZTERM_CONFIG_DIR"),
        }
        match prior_stale {
            Some(v) => std::env::set_var("ZTERM_TEST_STALE_TOKEN", v),
            None => std::env::remove_var("ZTERM_TEST_STALE_TOKEN"),
        }
    }

    #[test]
    fn cli_token_override_rejects_missing_workspace_without_mutating_tokens() {
        let mut cfg = AppConfig::parse(
            r#"
active = "primary"

[[workspaces]]
name = "primary"
backend = "zeroclaw"
url = "http://primary.example"
token = "primary-token"

[[workspaces]]
name = "target"
backend = "zeroclaw"
url = "http://target.example"
token_env = "TARGET_TOKEN"
token = "target-token"
"#,
        )
        .unwrap();

        let err = apply_cli_token_override(&mut cfg, Some("cli-token"), Some("typo")).unwrap_err();

        assert!(err.to_string().contains("refusing to apply CLI token"));
        let primary = cfg
            .workspaces
            .iter()
            .find(|workspace| workspace.name == "primary")
            .unwrap();
        assert_eq!(primary.token.as_deref(), Some("primary-token"));
        let target = cfg
            .workspaces
            .iter()
            .find(|workspace| workspace.name == "target")
            .unwrap();
        assert_eq!(target.token_env.as_deref(), Some("TARGET_TOKEN"));
        assert_eq!(target.token.as_deref(), Some("target-token"));
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
