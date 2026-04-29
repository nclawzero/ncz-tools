use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{self, Write};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::cli::agent::{StreamSink, TurnChunk, TurnUsage};

const ZEROCLAW_WS_TURN_TIMEOUT: Duration = Duration::from_secs(120);
const ZEROCLAW_TURN_RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;
const ZEROCLAW_WEBHOOK_ENVELOPE_MAX_BYTES: usize = 64 * 1024;
const ZEROCLAW_WEBHOOK_ERROR_BODY_MAX_BYTES: usize = 8 * 1024;

/// One row from the daemon's `[providers.models.<key>]` config table.
///
/// `key` is the provider-key the daemon resolves at request time
/// (e.g. `"primary"`, `"consult"`, `"together"`); `provider` is the
/// backend the daemon will dispatch to (`"gemini"`, `"openai_compat"`);
/// `model` is the upstream model identifier
/// (e.g. `"gemini-flash-latest"`, `"MiniMaxAI/MiniMax-M2.7"`).
///
/// zterm treats `key` as the authoritative selector — that's what
/// gets sent back to zeroclaw as the `model` field. The other two are
/// display-only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub key: String,
    pub provider: String,
    pub model: String,
}

/// Zeroclaw REST/WS API client
#[derive(Clone)]
pub struct ZeroclawClient {
    base_url: String,
    token: String,
    http_client: Client,
    /// When `Some`, `submit_turn` forwards the response through this
    /// sink as `TurnChunk` events instead of printing to stdout.
    /// Used by the Turbo Vision UI (tv_ui); the legacy rustyline REPL
    /// leaves this `None`.
    stream_sink: Option<StreamSink>,
    /// Live model list fetched from `/api/config` once at boot, plus
    /// the currently selected key. Behind a single shared `StdMutex`
    /// so `Clone`d copies (cron handle in `Workspace.cron`, trait-
    /// boxed copy in `Workspace.client`) all see the same `/models
    /// set <key>` mutation.
    model_state: Arc<StdMutex<ModelState>>,
}

/// Inner model selection state shared across cloned client handles.
#[derive(Debug, Clone, Default)]
struct ModelState {
    /// All keys advertised by `[providers.models.*]`. Empty before
    /// `refresh_models` has run successfully.
    list: Vec<ModelInfo>,
    /// Currently selected key. Defaults to `default_key_from_list`
    /// once the list is populated; `ZTERM_MODEL` env-var override
    /// wins when set; static `"primary"` fallback when neither
    /// applies (a neutral config-key string).
    current: Option<String>,
}

/// Config response from /api/config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    pub model: String,
    pub provider: String,
}

/// Provider info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub requires_key: bool,
    pub api_key_env: Option<String>,
}

/// Model info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub display_name: String,
    pub provider: String,
    pub context_window: Option<usize>,
    pub supports_reasoning: bool,
}

/// Session info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub model: String,
    pub provider: String,
}

#[derive(Debug, Clone)]
pub enum ClientError {
    Network(String),
    Auth(String),
    NotFound(String),
    Server(String),
    Timeout,
    Invalid(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ClientError::Auth(_) => write!(f, "Authentication failed. Check your API key."),
            ClientError::NotFound(_) => write!(f, "Resource not found."),
            ClientError::Timeout => write!(f, "Request timed out. Check your connection."),
            ClientError::Network(msg) => write!(f, "Network error: {}", msg),
            ClientError::Server(msg) => write!(f, "Server error: {}", msg),
            ClientError::Invalid(msg) => write!(f, "Invalid request: {}", msg),
        }
    }
}

impl std::error::Error for ClientError {}

impl ZeroclawClient {
    /// Create a new Zeroclaw API client
    pub fn new(base_url: String, token: String) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self {
            base_url,
            token,
            http_client,
            stream_sink: None,
            model_state: Arc::new(StdMutex::new(ModelState::default())),
        }
    }

    /// Install or clear the streaming sink. Inherent method mirrors the
    /// `AgentClient::set_stream_sink` trait entry so the client can be
    /// configured either through the trait object or directly.
    pub fn set_stream_sink(&mut self, sink: Option<StreamSink>) {
        self.stream_sink = sink;
    }

    fn session_url(&self, session_id: &str) -> Result<Url> {
        Self::session_url_for_base(&self.base_url, session_id)
    }

    fn session_url_for_base(base_url: &str, session_id: &str) -> Result<Url> {
        let mut url = Url::parse(base_url)
            .map_err(|e| anyhow!("Failed to parse zeroclaw base URL: {}", e))?;
        url.set_query(None);
        url.set_fragment(None);
        url.path_segments_mut()
            .map_err(|_| anyhow!("zeroclaw base URL cannot be used as a base URL"))?
            .extend(["api", "sessions", session_id]);
        Ok(url)
    }

    fn cron_job_url(&self, action: &str, job_id: &str) -> Result<Url> {
        Self::cron_job_url_for_base(&self.base_url, action, job_id)
    }

    fn cron_job_url_for_base(base_url: &str, action: &str, job_id: &str) -> Result<Url> {
        let mut url = Url::parse(base_url)
            .map_err(|e| anyhow!("Failed to parse zeroclaw base URL: {}", e))?;
        url.set_query(None);
        url.set_fragment(None);
        url.path_segments_mut()
            .map_err(|_| anyhow!("zeroclaw base URL cannot be used as a base URL"))?
            .extend(["api", "cron", action, job_id]);
        Ok(url)
    }

    /// Fetch `[providers.models.*]` from the daemon's `/api/config`
    /// and replace this client's cached model list. Called once at
    /// boot from `tui::run`. Failure is non-fatal — the cached list
    /// stays empty and the static `"primary"` fallback wins for the
    /// session.
    pub async fn refresh_models(&self) -> Result<Vec<ModelInfo>> {
        let toml_str = self.fetch_config_toml().await?;
        let cfg: toml::Value = toml::from_str(&toml_str)
            .map_err(|e| anyhow!("Failed to parse zeroclaw config TOML: {}", e))?;

        let mut list: Vec<ModelInfo> = Vec::new();
        if let Some(models) = cfg
            .get("providers")
            .and_then(|p| p.get("models"))
            .and_then(|m| m.as_table())
        {
            for (key, val) in models.iter() {
                let provider = val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                let model = val
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                list.push(ModelInfo {
                    key: key.to_string(),
                    provider,
                    model,
                });
            }
        }
        // Stable ordering: alphabetical by key. `BTreeMap` would also
        // work but `toml::Value`'s table iteration order isn't
        // guaranteed across editions, so sort here for predictable
        // `/models` output and tests.
        list.sort_by(|a, b| a.key.cmp(&b.key));

        // Pick a default key. Prefer `[providers] fallback = "<name>"`
        // when it matches a `name` field in the model list; else first
        // key in the (now sorted) list. ZTERM_MODEL env-var override is
        // applied at read time, not stored, so users can flip it
        // without restarting the daemon.
        let default_key = default_key_from_list(&list, &cfg);

        let mut state = self.model_state.lock().expect("model_state poisoned");
        state.list = list.clone();
        if state.current.is_none() {
            state.current = default_key;
        }
        Ok(list)
    }

    /// Current cached model list. Empty before `refresh_models` runs.
    pub fn model_list(&self) -> Vec<ModelInfo> {
        self.model_state
            .lock()
            .expect("model_state poisoned")
            .list
            .clone()
    }

    /// Currently-selected model key. Resolution order:
    ///   1. `ZTERM_MODEL` env var (caller override; never persisted).
    ///   2. The key set via `set_current_model` / `refresh_models`.
    ///   3. Static `"primary"` (a config-key string, not a provider
    ///      brand reference).
    pub fn current_model_key(&self) -> String {
        if let Ok(v) = std::env::var("ZTERM_MODEL") {
            if !v.is_empty() {
                return v;
            }
        }
        self.model_state
            .lock()
            .expect("model_state poisoned")
            .current
            .clone()
            .unwrap_or_else(|| "primary".to_string())
    }

    /// Set the current model key. Returns `Err` if the key isn't in
    /// the cached list (caller decides whether to surface that as
    /// `/models set` error text).
    pub fn set_current_model(&self, key: &str) -> Result<()> {
        let mut state = self.model_state.lock().expect("model_state poisoned");
        if !state.list.is_empty() && !state.list.iter().any(|m| m.key == key) {
            let known: Vec<String> = state.list.iter().map(|m| m.key.clone()).collect();
            return Err(anyhow!(
                "model key '{}' not found in /api/config (known: {})",
                key,
                known.join(", ")
            ));
        }
        state.current = Some(key.to_string());
        Ok(())
    }

    /// Health check
    pub async fn health(&self) -> Result<bool> {
        let url = format!("{}/api/health", self.base_url);
        match self.http_client.get(&url).send().await {
            Ok(res) => Ok(res.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    /// Get configuration
    pub async fn get_config(&self) -> Result<Config> {
        let url = format!("{}/api/config", self.base_url);
        let res = self
            .http_client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!(ClientError::Network(e.to_string())))?;

        match res.status().as_u16() {
            200 => {
                let config = res
                    .json::<Config>()
                    .await
                    .map_err(|e| anyhow!("Failed to parse config: {}", e))?;
                Ok(config)
            }
            401 | 403 => Err(anyhow!(ClientError::Auth("Unauthorized".to_string()))),
            404 => Err(anyhow!(ClientError::NotFound(
                "Config not found".to_string()
            ))),
            500..=599 => Err(anyhow!(ClientError::Server(res.status().to_string()))),
            _ => Err(anyhow!(ClientError::Invalid(res.status().to_string()))),
        }
    }

    /// Put configuration
    pub async fn put_config(&self, config: &Config) -> Result<()> {
        let url = format!("{}/api/config", self.base_url);
        let res = self
            .http_client
            .put(&url)
            .bearer_auth(&self.token)
            .json(config)
            .send()
            .await
            .map_err(|e| anyhow!(ClientError::Network(e.to_string())))?;

        match res.status().as_u16() {
            200 | 204 => Ok(()),
            401 | 403 => Err(anyhow!(ClientError::Auth("Unauthorized".to_string()))),
            500..=599 => Err(anyhow!(ClientError::Server(res.status().to_string()))),
            _ => Err(anyhow!(ClientError::Invalid(res.status().to_string()))),
        }
    }

    /// List providers
    pub async fn list_providers(&self) -> Result<Vec<Provider>> {
        // zeroclaw does not expose /api/providers directly. Read the daemon's
        // config via /api/config and synthesize the provider list from both
        // supported config schemas:
        //   A) [providers.models.<name>]               — sub-table style
        //   B) model_routes = [{ provider = ".." }]    — array-of-routes style
        //
        // Schema (B) matches the convention the web UI reads (upstream commit
        // 69c30bb, 2026-04-19). We union the two so zterm works against any
        // zeroclaw regardless of how its config is shaped.
        let toml_str = self.fetch_config_toml().await?;
        let cfg: toml::Value = toml::from_str(&toml_str)
            .map_err(|e| anyhow!("Failed to parse zeroclaw config TOML: {}", e))?;

        let mut names: std::collections::BTreeSet<String> = Default::default();

        // Schema A: [providers.models.<name>]
        if let Some(models) = cfg
            .get("providers")
            .and_then(|p| p.get("models"))
            .and_then(|m| m.as_table())
        {
            for k in models.keys() {
                names.insert(k.clone());
            }
        }

        // Schema B: model_routes = [{ provider = "...", model = "..." }, ...]
        if let Some(routes) = cfg.get("model_routes").and_then(|r| r.as_array()) {
            for route in routes {
                if let Some(provider) = route.get("provider").and_then(|v| v.as_str()) {
                    names.insert(provider.to_string());
                }
            }
        }

        Ok(names
            .into_iter()
            .map(|name| Provider {
                id: name.clone(),
                name,
                requires_key: true,
                api_key_env: None,
            })
            .collect())
    }

    /// Fetch the raw TOML config string from /api/config.
    async fn fetch_config_toml(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct ConfigEnvelope {
            content: String,
        }
        let url = format!("{}/api/config", self.base_url);
        let res = self
            .http_client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!(ClientError::Network(e.to_string())))?;
        match res.status().as_u16() {
            200 => {
                let envelope = res
                    .json::<ConfigEnvelope>()
                    .await
                    .map_err(|e| anyhow!("Failed to parse /api/config envelope: {}", e))?;
                Ok(envelope.content)
            }
            code => Err(anyhow!(ClientError::Invalid(format!(
                "/api/config returned {}",
                code
            )))),
        }
    }

    /// Get models for a provider
    pub async fn get_models(&self, provider: &str) -> Result<Vec<Model>> {
        // Read from both supported config schemas and union the results.
        //   A) [providers.models.<provider>] model = "..."
        //   B) model_routes = [{ provider = "...", model = "..." }, ...]
        //   C) root-level model / default_model string (added to every provider's list)
        //
        // The web UI (commit 69c30bb, 2026-04-19) uses smol-toml to read
        // schema B + the root default_model alias; zterm mirrors that behaviour.
        let toml_str = self.fetch_config_toml().await?;
        let cfg: toml::Value = toml::from_str(&toml_str)
            .map_err(|e| anyhow!("Failed to parse zeroclaw config TOML: {}", e))?;

        let mut ids: std::collections::BTreeSet<String> = Default::default();

        // Schema A
        if let Some(model_id) = cfg
            .get("providers")
            .and_then(|p| p.get("models"))
            .and_then(|m| m.get(provider))
            .and_then(|t| t.get("model"))
            .and_then(|v| v.as_str())
        {
            ids.insert(model_id.to_string());
        }

        // Schema B — filter routes whose provider matches
        if let Some(routes) = cfg.get("model_routes").and_then(|r| r.as_array()) {
            for route in routes {
                let route_provider = route.get("provider").and_then(|v| v.as_str());
                if route_provider == Some(provider) {
                    if let Some(m) = route.get("model").and_then(|v| v.as_str()) {
                        ids.insert(m.to_string());
                    }
                }
            }
        }

        // Schema C — root-level model / default_model (shown for every provider
        // since the root default is provider-agnostic in this schema)
        for key in ["model", "default_model"] {
            if let Some(m) = cfg.get(key).and_then(|v| v.as_str()) {
                ids.insert(m.to_string());
            }
        }

        Ok(ids
            .into_iter()
            .map(|id| Model {
                id: id.clone(),
                display_name: id,
                provider: provider.to_string(),
                context_window: None,
                supports_reasoning: false,
            })
            .collect())
    }

    /// List sessions
    pub async fn list_sessions(&self) -> Result<Vec<Session>> {
        let url = format!("{}/api/sessions", self.base_url);
        let res = self
            .http_client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!(ClientError::Network(e.to_string())))?;

        match res.status().as_u16() {
            200 => {
                let sessions = res
                    .json::<Vec<Session>>()
                    .await
                    .map_err(|e| anyhow!("Failed to parse sessions: {}", e))?;
                Ok(sessions)
            }
            _ => Err(anyhow!(ClientError::Invalid(res.status().to_string()))),
        }
    }

    /// Create a session (implicit - zeroclaw uses implicit session IDs)
    pub async fn create_session(&self, name: &str) -> Result<Session> {
        // zeroclaw v0.7.3 doesn't support POST /api/sessions
        // Sessions are implicit and accessed by ID via WebSocket
        // Use the name as both ID and display name. Model/provider
        // strings here are display-only metadata for the local
        // session record; the actual routing key is read from
        // `current_model_key()` per turn.
        Ok(Session {
            id: name.to_string(),
            name: name.to_string(),
            model: self.current_model_key(),
            provider: "zeroclaw".to_string(),
        })
    }

    /// Submit a turn (message) to a session via the `/ws/chat` endpoint.
    ///
    /// The WebSocket path preserves the selected zeroclaw model key in
    /// the message envelope and streams daemon `chunk` events into the
    /// installed `stream_sink`. If the WebSocket cannot be opened at
    /// all, fall back to the legacy one-shot webhook before any turn is
    /// sent.
    pub async fn submit_turn(&self, session_id: &str, message: &str) -> Result<String> {
        let model = self.current_model_key();

        if let Ok(url) = self.ws_chat_url(session_id) {
            if let Ok((ws_stream, _)) = connect_async(url.as_str()).await {
                return self
                    .submit_turn_ws_connected(ws_stream, session_id, message, &model)
                    .await;
            }
        }

        self.submit_turn_webhook(session_id, message, &model).await
    }

    fn ws_chat_url(&self, session_id: &str) -> Result<String> {
        let mut url = Url::parse(&self.base_url)
            .map_err(|e| anyhow!("Invalid zeroclaw base URL '{}': {}", self.base_url, e))?;

        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            "ws" => "ws",
            "wss" => "wss",
            other => return Err(anyhow!("Unsupported zeroclaw URL scheme '{}'", other)),
        };
        url.set_scheme(scheme)
            .map_err(|_| anyhow!("Failed to set WebSocket URL scheme"))?;
        url.set_path("/ws/chat");
        url.set_fragment(None);
        url.set_query(None);

        {
            let mut pairs = url.query_pairs_mut();
            if !session_id.is_empty() {
                pairs.append_pair("session_id", session_id);
                pairs.append_pair("name", session_id);
            }
            if !self.token.is_empty() {
                pairs.append_pair("token", &self.token);
            }
        }

        Ok(url.to_string())
    }

    async fn submit_turn_ws_connected(
        &self,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        session_id: &str,
        message: &str,
        model: &str,
    ) -> Result<String> {
        self.submit_turn_ws_connected_with_limits(
            ws_stream,
            session_id,
            message,
            model,
            ZEROCLAW_WS_TURN_TIMEOUT,
            ZEROCLAW_TURN_RESPONSE_MAX_BYTES,
        )
        .await
    }

    async fn submit_turn_ws_connected_with_limits(
        &self,
        mut ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        session_id: &str,
        message: &str,
        model: &str,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let payload = json!({
            "type": "message",
            "content": message,
            "session_id": session_id,
            "role": "user",
            "request_id": request_id,
            "model": model,
        });

        ws_stream
            .send(Message::Text(payload.to_string()))
            .await
            .map_err(|e| anyhow!("WebSocket send failed: {}", e))?;

        let mut response = String::new();
        let mut streamed = false;
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                let wrapped = anyhow!(
                    "WebSocket turn timed out after {}s before a response completed",
                    timeout.as_secs()
                );
                let _ = ws_stream.close(None).await;
                self.emit_failure(&wrapped);
                return Err(wrapped);
            }

            let frame = match tokio::time::timeout(remaining, ws_stream.next()).await {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => {
                    let wrapped = anyhow!(
                        "WebSocket turn timed out after {}s before a response completed",
                        timeout.as_secs()
                    );
                    let _ = ws_stream.close(None).await;
                    self.emit_failure(&wrapped);
                    return Err(wrapped);
                }
            };
            let frame = match frame {
                Ok(frame) => frame,
                Err(e) => {
                    let wrapped = anyhow!("WebSocket read failed: {}", e);
                    self.emit_failure(&wrapped);
                    return Err(wrapped);
                }
            };

            let text = match frame {
                Message::Text(text) => text,
                Message::Close(_) => break,
                _ => continue,
            };
            if text.len() > max_response_bytes.saturating_add(4096) {
                let wrapped = anyhow!(
                    "WebSocket response frame exceeded {} byte limit",
                    max_response_bytes
                );
                let _ = ws_stream.close(None).await;
                self.emit_failure(&wrapped);
                return Err(wrapped);
            }

            let event: serde_json::Value = match serde_json::from_str(&text) {
                Ok(event) => event,
                Err(_) => continue,
            };

            match event.get("type").and_then(|v| v.as_str()) {
                Some("chunk") => {
                    if let Some(delta) = event.get("content").and_then(|v| v.as_str()) {
                        if response.len().saturating_add(delta.len()) > max_response_bytes {
                            let wrapped = anyhow!(
                                "WebSocket response exceeded {} byte limit",
                                max_response_bytes
                            );
                            let _ = ws_stream.close(None).await;
                            self.emit_failure(&wrapped);
                            return Err(wrapped);
                        }
                        response.push_str(delta);
                        self.emit_token(delta)?;
                        streamed = true;
                    }
                }
                // Compatibility with the older helper in `websocket.rs`.
                Some("stream") => {
                    if let Some(delta) = event.get("data").and_then(|v| v.as_str()) {
                        if response.len().saturating_add(delta.len()) > max_response_bytes {
                            let wrapped = anyhow!(
                                "WebSocket response exceeded {} byte limit",
                                max_response_bytes
                            );
                            let _ = ws_stream.close(None).await;
                            self.emit_failure(&wrapped);
                            return Err(wrapped);
                        }
                        response.push_str(delta);
                        self.emit_token(delta)?;
                        streamed = true;
                    }
                }
                Some("done") => {
                    let full_response = event
                        .get("full_response")
                        .or_else(|| event.get("response"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(response.as_str())
                        .to_string();
                    if full_response.len() > max_response_bytes {
                        let wrapped = anyhow!(
                            "WebSocket response exceeded {} byte limit",
                            max_response_bytes
                        );
                        let _ = ws_stream.close(None).await;
                        self.emit_failure(&wrapped);
                        return Err(wrapped);
                    }
                    if !streamed && !full_response.is_empty() {
                        self.emit_token(&full_response)?;
                        streamed = true;
                    }
                    self.emit_usage(TurnUsage::from_json_candidates(&event));
                    self.emit_finished_ok(full_response.clone(), streamed);
                    return Ok(full_response);
                }
                Some("usage") => {
                    self.emit_usage(TurnUsage::from_json_candidates(&event));
                }
                Some("error") => {
                    let msg = event
                        .get("message")
                        .or_else(|| event.get("error"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("WebSocket turn failed");
                    let wrapped = anyhow!("WebSocket error: {}", msg);
                    self.emit_failure(&wrapped);
                    return Err(wrapped);
                }
                Some("session_start")
                | Some("connected")
                | Some("thinking")
                | Some("tool_call")
                | Some("tool_result")
                | Some("chunk_reset")
                | Some("agent_start")
                | Some("agent_end") => {}
                _ => {}
            }
        }

        let wrapped = anyhow!("WebSocket closed before a response completed");
        self.emit_failure(&wrapped);
        Err(wrapped)
    }

    async fn submit_turn_webhook(
        &self,
        session_id: &str,
        message: &str,
        model: &str,
    ) -> Result<String> {
        self.submit_turn_webhook_with_limit(
            session_id,
            message,
            model,
            ZEROCLAW_TURN_RESPONSE_MAX_BYTES,
        )
        .await
    }

    async fn submit_turn_webhook_with_limit(
        &self,
        session_id: &str,
        message: &str,
        model: &str,
        max_response_bytes: usize,
    ) -> Result<String> {
        let url = format!("{}/webhook", self.base_url);
        let payload = serde_json::json!({
            "message": message,
            "session": session_id,
            "model": model
        });

        let response = match self.http_client.post(&url).json(&payload).send().await {
            Ok(r) => r,
            Err(e) => {
                let wrapped = anyhow!("Webhook request failed: {}", e);
                self.emit_failure(&wrapped);
                return Err(wrapped);
            }
        };

        let status = response.status();
        if !status.is_success() {
            let body = match Self::read_response_body_limited(
                response,
                ZEROCLAW_WEBHOOK_ERROR_BODY_MAX_BYTES,
            )
            .await
            {
                Ok(body) => String::from_utf8_lossy(&body).trim().to_string(),
                Err(e) => format!("could not read error body: {e}"),
            };
            let wrapped = if body.is_empty() {
                anyhow!("Webhook request failed: HTTP {}", status)
            } else {
                anyhow!("Webhook request failed: HTTP {}: {}", status, body)
            };
            self.emit_failure(&wrapped);
            return Err(wrapped);
        }

        let body_limit = max_response_bytes.saturating_add(ZEROCLAW_WEBHOOK_ENVELOPE_MAX_BYTES);
        let body = match Self::read_response_body_limited(response, body_limit).await {
            Ok(body) => body,
            Err(e) => {
                self.emit_failure(&e);
                return Err(e);
            }
        };

        let json: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(j) => j,
            Err(e) => {
                let wrapped = anyhow!("Failed to parse response: {}", e);
                self.emit_failure(&wrapped);
                return Err(wrapped);
            }
        };

        let text = match json.get("response").and_then(|v| v.as_str()) {
            Some(text) => text.to_string(),
            None => {
                let wrapped = anyhow!("Webhook response missing string 'response' field");
                self.emit_failure(&wrapped);
                return Err(wrapped);
            }
        };

        if text.len() > max_response_bytes {
            let wrapped = anyhow!(
                "Webhook response exceeded {} byte limit",
                max_response_bytes
            );
            self.emit_failure(&wrapped);
            return Err(wrapped);
        }

        match &self.stream_sink {
            Some(sink) => {
                if let Some(usage) = TurnUsage::from_json_candidates(&json) {
                    let _ = sink.send(TurnChunk::Usage(usage));
                }
                let _ = sink.send(TurnChunk::Token(text.clone()));
                let _ = sink.send(TurnChunk::Finished(Ok(text.clone())));
            }
            None => {
                println!("{}", text);
            }
        }

        Ok(text)
    }

    async fn read_response_body_limited(
        response: reqwest::Response,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow!("Failed to read response body: {}", e))?;
            if body.len().saturating_add(chunk.len()) > max_bytes {
                anyhow::bail!("Webhook response body exceeded {} byte limit", max_bytes);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    fn emit_token(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        if let Some(sink) = &self.stream_sink {
            let _ = sink.send(TurnChunk::Token(text.to_string()));
        } else {
            print!("{}", text);
            io::stdout().flush()?;
        }
        Ok(())
    }

    fn emit_finished_ok(&self, text: String, printed_to_stdout: bool) {
        if let Some(sink) = &self.stream_sink {
            let _ = sink.send(TurnChunk::Finished(Ok(text)));
        } else if printed_to_stdout {
            println!();
        }
    }

    fn emit_usage(&self, usage: Option<TurnUsage>) {
        if let (Some(sink), Some(usage)) = (&self.stream_sink, usage) {
            let _ = sink.send(TurnChunk::Usage(usage));
        }
    }

    fn emit_failure(&self, e: &anyhow::Error) {
        if let Some(sink) = &self.stream_sink {
            let _ = sink.send(TurnChunk::Finished(Err(e.to_string())));
        }
    }

    // ===== MODEL MANAGEMENT =====

    /// Update default model
    pub async fn set_model(&self, provider: &str, model: &str) -> Result<()> {
        let url = format!("{}/api/config", self.base_url);
        let config = Config {
            agent: AgentConfig {
                provider: provider.to_string(),
                model: model.to_string(),
            },
        };

        self.http_client
            .put(&url)
            .bearer_auth(&self.token)
            .json(&config)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to set model: {}", e))?;

        Ok(())
    }

    /// List available models for a provider
    pub async fn list_provider_models(&self, provider: &str) -> Result<Vec<String>> {
        // Delegate to get_models for the config-based lookup and return just IDs.
        let models = self.get_models(provider).await.unwrap_or_default();
        Ok(models.into_iter().map(|m| m.id).collect())
    }

    // ===== SESSION MANAGEMENT =====

    /// Load specific session
    pub async fn load_session(&self, session_id: &str) -> Result<Session> {
        let url = self.session_url(session_id)?;
        let res = self
            .http_client
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to load session: {}", e))?;

        match res.status().as_u16() {
            200 => res
                .json::<Session>()
                .await
                .map_err(|e| anyhow!("Failed to parse session: {}", e)),
            404 => Err(anyhow!("Session not found")),
            _ => Err(anyhow!("Failed to load session: {}", res.status())),
        }
    }

    /// Delete a session
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let url = self.session_url(session_id)?;
        let res = self
            .http_client
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to delete session: {}", e))?;

        if res.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("Failed to delete session: {}", res.status()))
        }
    }

    // ===== CRON & AUTOMATION =====

    /// List scheduled cron jobs
    pub async fn list_cron_jobs(&self) -> Result<Vec<serde_json::Value>> {
        let url = format!("{}/api/cron/list", self.base_url);
        let res = self
            .http_client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to list cron jobs: {}", e))?;

        let status = res.status();
        if !status.is_success() {
            let body = res
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));
            if body.trim().is_empty() {
                return Err(anyhow!("Failed to list cron jobs: {status}"));
            }
            return Err(anyhow!("Failed to list cron jobs: {status}: {body}"));
        }

        res.json::<Vec<serde_json::Value>>()
            .await
            .map_err(|e| anyhow!("Failed to parse cron list response: {}", e))
    }

    /// Create a cron job
    pub async fn create_cron_job(&self, expression: &str, prompt: &str) -> Result<String> {
        let url = format!("{}/api/cron/add", self.base_url);
        let payload = json!({
            "expression": expression,
            "prompt": prompt,
            "timezone": "UTC"
        });

        let res = self
            .http_client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to create cron job: {}", e))?;

        match res.status().as_u16() {
            200 | 201 => {
                let json: serde_json::Value = res
                    .json()
                    .await
                    .map_err(|e| anyhow!("Failed to parse response: {}", e))?;
                Ok(json
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string())
            }
            _ => Err(anyhow!("Failed to create cron job")),
        }
    }

    /// Create a one-time scheduled task
    pub async fn create_cron_at(&self, datetime: &str, prompt: &str) -> Result<String> {
        let url = format!("{}/api/cron/add-at", self.base_url);
        let payload = json!({
            "datetime": datetime,
            "prompt": prompt
        });

        let res = self
            .http_client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to create task: {}", e))?;

        match res.status().as_u16() {
            200 | 201 => Ok("Task scheduled".to_string()),
            _ => Err(anyhow!("Failed to create task")),
        }
    }

    /// Pause a cron job
    pub async fn pause_cron(&self, job_id: &str) -> Result<()> {
        let url = self.cron_job_url("pause", job_id)?;
        let res = self
            .http_client
            .post(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to pause job: {}", e))?;

        ensure_success_response(res, "Failed to pause job").await
    }

    /// Resume a cron job
    pub async fn resume_cron(&self, job_id: &str) -> Result<()> {
        let url = self.cron_job_url("resume", job_id)?;
        let res = self
            .http_client
            .post(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to resume job: {}", e))?;

        ensure_success_response(res, "Failed to resume job").await
    }

    /// Delete a cron job
    pub async fn delete_cron(&self, job_id: &str) -> Result<()> {
        let url = self.cron_job_url("remove", job_id)?;
        let res = self
            .http_client
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to delete job: {}", e))?;

        ensure_success_response(res, "Failed to delete job").await
    }
}

async fn ensure_success_response(res: reqwest::Response, context: &str) -> Result<()> {
    let status = res.status();
    if status.is_success() {
        return Ok(());
    }

    let body = res
        .text()
        .await
        .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));
    if body.trim().is_empty() {
        Err(anyhow!("{context}: {status}"))
    } else {
        Err(anyhow!("{context}: {status}: {body}"))
    }
}

/// Pick a default model key from the parsed config.
///
/// Strategy: if `[providers] fallback` names a backend (e.g.
/// `"gemini"`), return the first model key whose `name` field
/// matches. Otherwise return the alphabetically-first key. `None`
/// when the list is empty.
fn default_key_from_list(list: &[ModelInfo], cfg: &toml::Value) -> Option<String> {
    let fallback_provider = cfg
        .get("providers")
        .and_then(|p| p.get("fallback"))
        .and_then(|v| v.as_str());

    if let Some(name) = fallback_provider {
        if let Some(found) = list.iter().find(|m| m.provider == name) {
            return Some(found.key.clone());
        }
    }
    list.first().map(|m| m.key.clone())
}

// ===================================================================
// AgentClient trait impl for ZeroclawClient
//
// Forwards each trait method to the existing inherent `async fn` of
// the same name. Deliberate no-behavior-change refactor: call sites
// continue to use the concrete `ZeroclawClient` today. The trait only
// comes into play when `OpenClawClient` lands alongside it and
// `CommandHandler.client` widens to `Arc<dyn AgentClient>`.
// ===================================================================

#[async_trait::async_trait]
impl crate::cli::agent::AgentClient for ZeroclawClient {
    async fn health(&self) -> Result<bool> {
        ZeroclawClient::health(self).await
    }

    async fn get_config(&self) -> Result<Config> {
        ZeroclawClient::get_config(self).await
    }

    async fn put_config(&self, config: &Config) -> Result<()> {
        ZeroclawClient::put_config(self, config).await
    }

    async fn list_providers(&self) -> Result<Vec<Provider>> {
        ZeroclawClient::list_providers(self).await
    }

    async fn get_models(&self, provider: &str) -> Result<Vec<Model>> {
        ZeroclawClient::get_models(self, provider).await
    }

    async fn list_provider_models(&self, provider: &str) -> Result<Vec<String>> {
        ZeroclawClient::list_provider_models(self, provider).await
    }

    fn current_model_label(&self) -> String {
        self.current_model_key()
    }

    async fn list_sessions(&self) -> Result<Vec<Session>> {
        ZeroclawClient::list_sessions(self).await
    }

    async fn create_session(&self, name: &str) -> Result<Session> {
        ZeroclawClient::create_session(self, name).await
    }

    async fn load_session(&self, session_id: &str) -> Result<Session> {
        ZeroclawClient::load_session(self, session_id).await
    }

    async fn delete_session(&self, session_id: &str) -> Result<()> {
        ZeroclawClient::delete_session(self, session_id).await
    }

    async fn submit_turn(&mut self, session_id: &str, message: &str) -> Result<String> {
        ZeroclawClient::submit_turn(self, session_id, message).await
    }

    fn set_stream_sink(&mut self, sink: Option<StreamSink>) {
        ZeroclawClient::set_stream_sink(self, sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = ZeroclawClient::new(
            "http://localhost:8888".to_string(),
            "test_token".to_string(),
        );
        assert_eq!(client.base_url, "http://localhost:8888");
    }

    #[tokio::test]
    async fn pause_cron_rejects_http_error_status() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/api/cron/pause/job-1")
            .with_status(401)
            .with_body("denied")
            .create_async()
            .await;
        let client = ZeroclawClient::new(server.url(), "test_token".to_string());

        let err = client.pause_cron("job-1").await.unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("401"));
        assert!(msg.contains("denied"));
    }

    #[tokio::test]
    async fn resume_cron_rejects_http_error_status() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/api/cron/resume/job-1")
            .with_status(404)
            .with_body("missing")
            .create_async()
            .await;
        let client = ZeroclawClient::new(server.url(), "test_token".to_string());

        let err = client.resume_cron("job-1").await.unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("missing"));
    }

    #[tokio::test]
    async fn delete_cron_rejects_http_error_status() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("DELETE", "/api/cron/remove/job-1")
            .with_status(500)
            .with_body("broken")
            .create_async()
            .await;
        let client = ZeroclawClient::new(server.url(), "test_token".to_string());

        let err = client.delete_cron("job-1").await.unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("500"));
        assert!(msg.contains("broken"));
    }

    #[tokio::test]
    async fn list_cron_jobs_rejects_http_error_status() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/api/cron/list")
            .with_status(401)
            .with_body("denied")
            .create_async()
            .await;
        let client = ZeroclawClient::new(server.url(), "test_token".to_string());

        let err = client.list_cron_jobs().await.unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("401"));
        assert!(msg.contains("denied"));
    }

    #[tokio::test]
    async fn list_cron_jobs_rejects_malformed_json() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/api/cron/list")
            .with_status(200)
            .with_body("not-json")
            .create_async()
            .await;
        let client = ZeroclawClient::new(server.url(), "test_token".to_string());

        let err = client.list_cron_jobs().await.unwrap_err();

        assert!(err
            .to_string()
            .contains("Failed to parse cron list response"));
    }

    #[tokio::test]
    async fn webhook_submit_rejects_oversized_response_before_streaming() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/webhook")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({ "response": "12345678901234567" }).to_string())
            .create_async()
            .await;
        let mut client = ZeroclawClient::new(server.url(), String::new());
        let (sink, mut rx) = StreamSink::channel(8);
        client.set_stream_sink(Some(sink));

        let err = client
            .submit_turn_webhook_with_limit("main", "hello", "primary", 16)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Webhook response exceeded"));
        match rx.recv().await {
            Some(TurnChunk::Finished(Err(message))) => {
                assert!(message.contains("Webhook response exceeded"));
            }
            other => panic!("expected oversized webhook error, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn webhook_submit_rejects_non_success_json_without_assistant_text() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/webhook")
            .with_status(401)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({ "error": "denied" }).to_string())
            .create_async()
            .await;
        let mut client = ZeroclawClient::new(server.url(), String::new());
        let (sink, mut rx) = StreamSink::channel(8);
        client.set_stream_sink(Some(sink));

        let err = client
            .submit_turn_webhook_with_limit("main", "hello", "primary", 1024)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("HTTP 401"));
        assert!(err.to_string().contains("denied"));
        match rx.recv().await {
            Some(TurnChunk::Finished(Err(message))) => {
                assert!(message.contains("HTTP 401"));
            }
            other => panic!("expected webhook HTTP error, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn webhook_submit_rejects_missing_response_field() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/webhook")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({ "message": "ok without response" }).to_string())
            .create_async()
            .await;
        let mut client = ZeroclawClient::new(server.url(), String::new());
        let (sink, mut rx) = StreamSink::channel(8);
        client.set_stream_sink(Some(sink));

        let err = client
            .submit_turn_webhook_with_limit("main", "hello", "primary", 1024)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("missing string 'response' field"));
        match rx.recv().await {
            Some(TurnChunk::Finished(Err(message))) => {
                assert!(message.contains("missing string 'response' field"));
            }
            other => panic!("expected missing response error, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn webhook_submit_rejects_oversized_body_before_json_parse() {
        let mut server = mockito::Server::new_async().await;
        let oversized_body = format!(
            "{{\"response\":\"ok\",\"padding\":\"{}\"}}",
            "x".repeat(ZEROCLAW_WEBHOOK_ENVELOPE_MAX_BYTES + 32)
        );
        let _mock = server
            .mock("POST", "/webhook")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(oversized_body)
            .create_async()
            .await;
        let mut client = ZeroclawClient::new(server.url(), String::new());
        let (sink, mut rx) = StreamSink::channel(8);
        client.set_stream_sink(Some(sink));

        let err = client
            .submit_turn_webhook_with_limit("main", "hello", "primary", 16)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("response body exceeded"));
        match rx.recv().await {
            Some(TurnChunk::Finished(Err(message))) => {
                assert!(message.contains("response body exceeded"));
            }
            other => panic!("expected oversized body error, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cron_job_url_encodes_job_id_as_one_path_segment() {
        assert_eq!(
            ZeroclawClient::cron_job_url_for_base("https://example.test", "pause", "../x")
                .unwrap()
                .as_str(),
            "https://example.test/api/cron/pause/..%2Fx"
        );
        assert_eq!(
            ZeroclawClient::cron_job_url_for_base("https://example.test", "resume", "a/b")
                .unwrap()
                .as_str(),
            "https://example.test/api/cron/resume/a%2Fb"
        );
        assert_eq!(
            ZeroclawClient::cron_job_url_for_base("https://example.test", "remove", "x?y")
                .unwrap()
                .as_str(),
            "https://example.test/api/cron/remove/x%3Fy"
        );
    }

    #[test]
    fn zeroclaw_client_implements_agent_client() {
        // Compile-time assertion: a refactor that drops
        // `impl AgentClient for ZeroclawClient` will fail the test
        // suite here rather than leave the trait dangling. Pairs with
        // the identically-shaped assertion that OpenClawClient will
        // carry when v0.2 lands.
        fn assert_agent_client<T: crate::cli::agent::AgentClient>() {}
        assert_agent_client::<ZeroclawClient>();
    }

    #[test]
    fn default_key_picks_provider_fallback_match() {
        let list = vec![
            ModelInfo {
                key: "consult".into(),
                provider: "gemini".into(),
                model: "gemma-4-31b-it".into(),
            },
            ModelInfo {
                key: "primary".into(),
                provider: "gemini".into(),
                model: "gemini-flash-latest".into(),
            },
            ModelInfo {
                key: "together".into(),
                provider: "openai_compat".into(),
                model: "MiniMaxAI/MiniMax-M2.7".into(),
            },
        ];
        let cfg: toml::Value = toml::from_str(
            r#"
[providers]
fallback = "gemini"
"#,
        )
        .unwrap();
        // First gemini-named entry alphabetically is `consult`.
        assert_eq!(
            default_key_from_list(&list, &cfg),
            Some("consult".to_string())
        );
    }

    #[test]
    fn default_key_falls_back_to_first_when_no_fallback() {
        let list = vec![
            ModelInfo {
                key: "alpha".into(),
                provider: "openai_compat".into(),
                model: "x".into(),
            },
            ModelInfo {
                key: "beta".into(),
                provider: "gemini".into(),
                model: "y".into(),
            },
        ];
        let cfg: toml::Value = toml::from_str("").unwrap();
        assert_eq!(
            default_key_from_list(&list, &cfg),
            Some("alpha".to_string())
        );
    }

    #[test]
    fn current_model_key_static_fallback_is_primary() {
        let client = ZeroclawClient::new(
            "http://localhost:8888".to_string(),
            "test_token".to_string(),
        );
        std::env::remove_var("ZTERM_MODEL");
        // No refresh, no env — must be the neutral "primary" key.
        assert_eq!(client.current_model_key(), "primary");
    }

    #[test]
    fn ws_chat_url_targets_chat_endpoint_with_session_and_token() {
        let client = ZeroclawClient::new(
            "http://localhost:42617".to_string(),
            "test_token".to_string(),
        );

        let url = client.ws_chat_url("main session").unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.scheme(), "ws");
        assert_eq!(parsed.host_str(), Some("localhost"));
        assert_eq!(parsed.port(), Some(42617));
        assert_eq!(parsed.path(), "/ws/chat");
        assert_eq!(
            pairs.get("session_id").map(String::as_str),
            Some("main session")
        );
        assert_eq!(pairs.get("name").map(String::as_str), Some("main session"));
        assert_eq!(pairs.get("token").map(String::as_str), Some("test_token"));
    }

    #[test]
    fn ws_chat_url_translates_https_to_wss() {
        let client = ZeroclawClient::new("https://example.test".to_string(), String::new());

        let url = client.ws_chat_url("main").unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();

        assert_eq!(parsed.scheme(), "wss");
        assert_eq!(parsed.path(), "/ws/chat");
        assert!(parsed.query_pairs().all(|(key, _)| key.as_ref() != "token"));
    }

    async fn test_ws_stream(
        frames: Vec<serde_json::Value>,
        hold_open: Option<Duration>,
    ) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut server_ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = server_ws.next().await;
            for frame in frames {
                let _ = server_ws.send(Message::Text(frame.to_string())).await;
            }
            if let Some(duration) = hold_open {
                tokio::time::sleep(duration).await;
            } else {
                let _ = server_ws.close(None).await;
            }
        });
        let (client_ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        client_ws
    }

    #[tokio::test]
    async fn ws_submit_caps_accumulated_chunk_bytes() {
        let ws = test_ws_stream(
            vec![
                json!({ "type": "chunk", "content": "1234567890" }),
                json!({ "type": "chunk", "content": "abcdefghi" }),
            ],
            None,
        )
        .await;
        let client = ZeroclawClient::new("http://localhost:8888".to_string(), String::new());

        let err = client
            .submit_turn_ws_connected_with_limits(
                ws,
                "main",
                "hello",
                "primary",
                Duration::from_secs(5),
                16,
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("WebSocket response exceeded"));
    }

    #[tokio::test]
    async fn ws_submit_times_out_without_done_or_close() {
        let ws = test_ws_stream(
            vec![json!({ "type": "thinking", "content": "still working" })],
            Some(Duration::from_millis(100)),
        )
        .await;
        let client = ZeroclawClient::new("http://localhost:8888".to_string(), String::new());

        let err = client
            .submit_turn_ws_connected_with_limits(
                ws,
                "main",
                "hello",
                "primary",
                Duration::from_millis(10),
                1024,
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("WebSocket turn timed out"));
    }

    #[tokio::test]
    async fn ws_submit_rejects_chunk_then_close_without_done() {
        let ws = test_ws_stream(vec![json!({ "type": "chunk", "content": "partial" })], None).await;
        let client = ZeroclawClient::new("http://localhost:8888".to_string(), String::new());

        let err = client
            .submit_turn_ws_connected_with_limits(
                ws,
                "main",
                "hello",
                "primary",
                Duration::from_secs(5),
                1024,
            )
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("WebSocket closed before a response completed"));
    }

    #[test]
    fn session_url_encodes_session_id_as_one_path_segment() {
        assert_eq!(
            ZeroclawClient::session_url_for_base("https://example.test", "../owned")
                .unwrap()
                .as_str(),
            "https://example.test/api/sessions/..%2Fowned"
        );
        assert_eq!(
            ZeroclawClient::session_url_for_base("https://example.test", "a/b")
                .unwrap()
                .as_str(),
            "https://example.test/api/sessions/a%2Fb"
        );
        assert_eq!(
            ZeroclawClient::session_url_for_base("https://example.test", "x?y")
                .unwrap()
                .as_str(),
            "https://example.test/api/sessions/x%3Fy"
        );
    }

    #[test]
    fn set_current_model_rejects_unknown_key_when_list_populated() {
        let client = ZeroclawClient::new(
            "http://localhost:8888".to_string(),
            "test_token".to_string(),
        );
        // Seed a list directly to avoid needing a live daemon.
        {
            let mut state = client.model_state.lock().unwrap();
            state.list = vec![ModelInfo {
                key: "primary".into(),
                provider: "gemini".into(),
                model: "x".into(),
            }];
        }
        assert!(client.set_current_model("primary").is_ok());
        assert!(client.set_current_model("does-not-exist").is_err());
    }

    #[test]
    fn set_stream_sink_replaces_and_clears() {
        // Regression coverage for the E-2 trait wiring: setting a
        // sink, replacing it, and clearing it all route through
        // without panic and reach the stored slot. The full
        // Token/Finished emission path is covered end-to-end by the
        // TYPHON live-smoke since it requires a running HTTP daemon.
        let mut client = ZeroclawClient::new(
            "http://localhost:8888".to_string(),
            "test_token".to_string(),
        );
        assert!(client.stream_sink.is_none());

        let (tx, _rx) = StreamSink::channel(8);
        client.set_stream_sink(Some(tx));
        assert!(client.stream_sink.is_some());

        client.set_stream_sink(None);
        assert!(client.stream_sink.is_none());
    }
}
