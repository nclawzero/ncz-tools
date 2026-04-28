//! Backend-agnostic agent-client trait for the claw-family.
//!
//! `trait AgentClient` defines the surface every claw-family backend
//! (zeroclaw, openclaw, nemoclaw, API-compatible derivatives) must
//! expose for zterm's REPL to talk to it uniformly. Concrete impls:
//!
//! - `ZeroclawClient` in `client.rs` — v0.1 reference implementation
//!   (HTTP + WebSocket, Bearer token auth).
//! - `OpenClawClient` (v0.2, planned) — pure WebSocket with ed25519
//!   challenge-response handshake, JSON frames with req/res/event
//!   discriminator, protocol version 3. See
//!   <https://github.com/openclaw/openclaw/blob/main/docs/gateway/protocol.md>.
//!
//! Surfaces explicitly kept OUT of this trait:
//!
//! - **MNEMOS `/memory` commands.** MNEMOS is user-global memory,
//!   not agent-scoped. Shared across all workspaces; lives as inherent
//!   methods on the concrete client (or a separate `MnemosClient` in
//!   v0.2+ when multi-workspace lands).
//! - **Cron / skills / channels.** Claw-family shared surfaces but not
//!   every derivative implements them identically. Lands behind
//!   optional sub-traits (`CronSurface`, `SkillsSurface`, etc.) when
//!   the second concrete backend ships so we don't force stubbing.
//!
//! The trait is intentionally sized against the two-backend reality
//! (zeroclaw + openclaw) rather than pre-generalized. Widen only when
//! a third concrete need forces it.
//!
//! See `IMPLEMENTATION.md` `## v0.2 Backend Scope` for the full design
//! note, and `project_zterm_backend_scope` memory for the scope rule.

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

// Re-export the shared types rather than duplicate them. They originated
// in `client.rs` with `ZeroclawClient` and stay there until v0.2 moves
// them into a dedicated `types.rs` alongside the second backend.
pub use crate::cli::client::{Config, Model, Provider, Session};

/// Token accounting for the most recent completed turn.
///
/// Backends disagree on field names (`prompt_tokens` vs.
/// `input_tokens`, `completion_tokens` vs. `output_tokens`) and not
/// every response includes the model's context window. This struct
/// keeps the raw useful pieces while exposing `used_tokens()` and
/// `context_window` for the Turbo Vision status line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

impl TurnUsage {
    pub fn used_tokens(&self) -> Option<u64> {
        self.total_tokens
            .or_else(|| match (self.input_tokens, self.output_tokens) {
                (Some(input), Some(output)) => Some(input + output),
                (Some(input), None) => Some(input),
                (None, Some(output)) => Some(output),
                (None, None) => None,
            })
    }

    pub fn budget_pct(&self) -> Option<u8> {
        let used = self.used_tokens()?;
        let total = self.context_window?;
        if total == 0 {
            return None;
        }
        Some(((used.saturating_mul(100)) / total).min(100) as u8)
    }

    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let input_tokens = first_u64(
            value,
            &[
                "input_tokens",
                "prompt_tokens",
                "promptTokens",
                "inputTokens",
            ],
        );
        let output_tokens = first_u64(
            value,
            &[
                "output_tokens",
                "completion_tokens",
                "completionTokens",
                "outputTokens",
            ],
        );
        let total_tokens = first_u64(value, &["total_tokens", "totalTokens", "tokens_total"]);
        let context_window = first_u64(
            value,
            &[
                "context_window",
                "contextWindow",
                "context_length",
                "contextLength",
                "max_context_tokens",
                "maxContextTokens",
            ],
        );

        let usage = Self {
            input_tokens,
            output_tokens,
            total_tokens,
            context_window,
        };
        usage.used_tokens().map(|_| usage)
    }

    pub fn from_json_candidates(value: &serde_json::Value) -> Option<Self> {
        if let Some(usage) = Self::from_json(value) {
            return Some(usage);
        }
        let candidates = [
            value.get("usage"),
            value.get("token_usage"),
            value.get("tokenUsage"),
            value.get("metrics").and_then(|m| m.get("usage")),
            value.get("metadata").and_then(|m| m.get("usage")),
        ];
        candidates.into_iter().flatten().find_map(Self::from_json)
    }
}

fn first_u64(value: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
                .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
    })
}

/// Streaming chunk emitted by an `AgentClient` during `submit_turn` when a
/// sink has been installed via `AgentClient::set_stream_sink`. The TUI
/// consumes these to append tokens into the chat pane as they arrive from
/// the daemon. When no sink is installed the client falls back to its
/// legacy stdout-print path for the rustyline REPL.
#[derive(Debug, Clone)]
pub enum TurnChunk {
    /// A streamed token fragment. Concatenating every `Token` in order
    /// yields the same text that legacy stdout mode would have printed.
    Token(String),
    /// A short UX flourish that should be rendered with the TUI
    /// typewriter cadence instead of appended as an agent response.
    Typewriter(String),
    /// Token accounting for the completed or near-completed turn.
    /// Emitted when a backend surfaces usage data.
    Usage(TurnUsage),
    /// UI-only status reset used when the active turn context changes
    /// without a fresh usage report (new turn, workspace/session/model
    /// switch). Chat renderers should ignore this chunk.
    ClearUsage,
    /// The turn has completed — either with the full response text, or
    /// with an error. Emitted exactly once per submit. The UI should
    /// treat anything after this as a protocol bug.
    Finished(std::result::Result<String, String>),
}

/// Tokio sender half the TUI hands to the client before each turn.
/// `UnboundedSender` keeps the streaming path cheap and drop-safe: if
/// the TUI goes away mid-turn the client's sends silently fail rather
/// than block on a bounded channel.
pub type StreamSink = mpsc::UnboundedSender<TurnChunk>;

/// Backend-agnostic agent client.
///
/// Every method is `async` and returns `anyhow::Result`. Implementations
/// should map backend-specific errors into `anyhow::Error` via `map_err`
/// or `anyhow!("...")` macros — do not leak transport types.
#[async_trait]
pub trait AgentClient: Send + Sync {
    // ---------- liveness + config ----------

    /// Probe the daemon for basic reachability. Should be cheap and
    /// side-effect-free — HTTP `/health`-style or a WS `ping` RPC.
    async fn health(&self) -> Result<bool>;

    /// Fetch the daemon's current configuration envelope. Callers
    /// should treat the returned `Config.content` as opaque TOML text
    /// for zeroclaw; openclaw returns a structured response so its
    /// impl may synthesize the TOML representation for compatibility.
    async fn get_config(&self) -> Result<Config>;

    /// Push a new configuration envelope to the daemon.
    async fn put_config(&self, config: &Config) -> Result<()>;

    // ---------- providers + models ----------

    async fn list_providers(&self) -> Result<Vec<Provider>>;

    async fn get_models(&self, provider: &str) -> Result<Vec<Model>>;

    async fn list_provider_models(&self, provider: &str) -> Result<Vec<String>>;

    // ---------- sessions ----------

    async fn list_sessions(&self) -> Result<Vec<Session>>;

    async fn create_session(&self, name: &str) -> Result<Session>;

    async fn load_session(&self, session_id: &str) -> Result<Session>;

    async fn delete_session(&self, session_id: &str) -> Result<()>;

    // ---------- chat ----------

    /// Submit a user turn to the named session and return the final
    /// accumulated response text.
    ///
    /// **Takes `&mut self`** because streaming-aware implementations
    /// (e.g. `OpenClawClient`'s `rpc_sessions_send_and_collect`) need
    /// exclusive access to the event channel for the duration of the
    /// collect loop. `ZeroclawClient`'s inherent `submit_turn` only
    /// reads through `&self`; its trait-impl wrapper widens to
    /// `&mut self` to match, no behavior change.
    async fn submit_turn(&mut self, session_id: &str, message: &str) -> Result<String>;

    // ---------- streaming sink ----------

    /// Install or clear the streaming sink used by `submit_turn`.
    ///
    /// When `Some(sink)` is installed, streaming-capable implementations
    /// forward `TurnChunk::Token(_)` values to the sink as the daemon
    /// emits them, and end with exactly one `TurnChunk::Finished(_)`.
    /// When `None`, implementations fall back to their legacy behavior
    /// (typically `print!`-style stdout streaming for the rustyline
    /// REPL).
    ///
    /// Default impl is a no-op so backends without a streaming UI path
    /// (e.g. the current `OpenClawClient`) keep compiling. They can
    /// pick up real sink support in a later slice without widening the
    /// trait again.
    fn set_stream_sink(&mut self, _sink: Option<StreamSink>) {}
}

#[cfg(test)]
mod tests {
    use super::TurnUsage;

    #[test]
    fn usage_parses_openai_style_fields() {
        let value = serde_json::json!({
            "prompt_tokens": 123,
            "completion_tokens": 45,
            "total_tokens": 168,
            "context_window": 4096
        });

        let usage = TurnUsage::from_json(&value).unwrap();

        assert_eq!(usage.used_tokens(), Some(168));
        assert_eq!(usage.budget_pct(), Some(4));
    }

    #[test]
    fn usage_parses_anthropic_style_fields() {
        let value = serde_json::json!({
            "input_tokens": "1000",
            "output_tokens": 250,
            "contextWindow": 8000
        });

        let usage = TurnUsage::from_json(&value).unwrap();

        assert_eq!(usage.used_tokens(), Some(1250));
        assert_eq!(usage.budget_pct(), Some(15));
    }
}
