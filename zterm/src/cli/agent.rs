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
