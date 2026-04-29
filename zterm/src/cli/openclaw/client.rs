//! OpenClaw gateway WebSocket client — v0.2 slice 3a (plumbing only).
//!
//! This slice stands up the WebSocket lifecycle: connect + read loop +
//! write loop + pending-request correlation. The **handshake** (sending
//! `connect` with a signed device identity) and **first RPC** (`models.list`
//! etc.) land in slices 3b/3c.
//!
//! ### Architecture
//!
//! ```text
//!                         caller task
//!                            │
//!              send_request(method, params)
//!                            │
//!                            ▼
//!     ┌─────────────────────────────────────┐
//!     │ 1. register(id) → oneshot::Receiver │
//!     │ 2. mpsc::Sender<RequestFrame>       │
//!     │ 3. await receiver for ResponseFrame │
//!     └─────────────────────────────────────┘
//!                            │
//!              mpsc::Sender<RequestFrame>
//!                            │
//!                            ▼
//!                  ┌──────────────────┐
//!                  │ write_loop task  │  ← serializes + sends on WS
//!                  └──────────────────┘
//!                            │
//!                            ▼
//!                      [ WebSocket ]
//!                            ▲
//!                            │
//!                  ┌──────────────────┐
//!                  │  read_loop task  │  ← parses incoming frames
//!                  └──────────────────┘
//!                            │
//!          ┌─────────────────┼──────────────────┐
//!          ▼                 ▼                  ▼
//!     Frame::Res        Frame::Event        Frame::Req
//!     pending.resolve   event_tx.send       (never from server — log & drop)
//! ```
//!
//! The read loop is the **single owner** of the WebSocket stream half;
//! every other task interacts via the `mpsc` channel or the
//! `PendingRequests` map. No shared-mutable-WebSocket.

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use sha2::{Digest as _, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WsError, Message as WsMessage},
};

use super::wire::{Frame, PendingRequests, RequestFrame, ResponseFrame};

/// Default timeout for a single RPC round-trip. Applies to any caller
/// that uses the convenience `send_request` without its own timeout.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Channel capacity for the outbound request queue. Small on purpose —
/// backpressure the caller rather than buffer arbitrary requests.
const OUTBOUND_CAPACITY: usize = 16;

const DEFAULT_SESSION_NAMESPACE: &str = "openclaw";

/// Channel capacity for server-pushed events. Streaming turns can push
/// bursty deltas; bump higher than outbound.
const EVENT_CAPACITY: usize = 256;

/// WebSocket-connected openclaw gateway client. This struct is the
/// single outward-facing handle — caller tasks clone the `outbound_tx`
/// and `pending` / `event_rx` out of it to do real work.
///
/// Dropping this struct does **not** cancel the read/write loops; call
/// `disconnect()` explicitly for a graceful shutdown. The background
/// tasks also shut down on any WebSocket error and flip `connected` to
/// false — callers should recheck `is_connected()` after errors.
pub struct OpenClawClient {
    /// Shared in-flight-request tracker. Reader clones this to fan
    /// responses; callers register pending ids here.
    pending: PendingRequests,

    /// Mpsc sink for client → server frames. The write-loop task is
    /// the only consumer. Wrapped in Option so  can drop
    /// the sender (closing the channel) without moving out of .
    outbound_tx: Option<mpsc::Sender<RequestFrame>>,

    /// Event frames (`Frame::Event`) from the server. The read loop
    /// pushes, a single consumer pops. `None` after `disconnect()`.
    event_rx: Option<mpsc::Receiver<super::wire::EventFrame>>,

    /// Becomes `false` when read or write loop exits (including
    /// graceful disconnect).
    connected: Arc<AtomicBool>,

    /// Read + write loop join handles; retained for graceful shutdown.
    /// Result of the last successful handshake. None if the client
    /// has not handshaken yet (e.g. built via the raw connect for
    /// testing). AgentClient method impls can inspect this to branch
    /// on server version / advertised method set.
    hello_ok: Option<super::handshake::HelloOk>,

    /// Optional Turbo Vision stream sink. When installed, `submit_turn`
    /// emits Token/Usage/Finished frames just like `ZeroclawClient`.
    stream_sink: Option<crate::cli::agent::StreamSink>,

    /// Stable zterm workspace namespace for generated OpenClaw session
    /// keys. OpenClaw stores sessions in a backend namespace, so zterm
    /// includes the workspace namespace in its deterministic key to keep
    /// same-label workspaces from sharing transcripts.
    session_namespace: String,

    /// Previous zterm namespaces that remain readable during
    /// migration from mutable name+URL-derived keys.
    session_namespace_aliases: Vec<String>,

    read_task: Option<JoinHandle<()>>,
    write_task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for OpenClawClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenClawClient")
            .field("connected", &self.connected.load(Ordering::Relaxed))
            .field("event_rx_live", &self.event_rx.is_some())
            .field("session_namespace", &self.session_namespace)
            .field("session_namespace_aliases", &self.session_namespace_aliases)
            .finish()
    }
}

impl OpenClawClient {
    /// Open a WebSocket connection to the openclaw gateway at `url`.
    ///
    /// `url` must be a `ws://` or `wss://` URL (openclaw's http-listen
    /// error messages are explicit that the gateway is WebSocket-first,
    /// not HTTP). This function **does not perform the handshake** — it
    /// only establishes the transport. The server will send a
    /// `connect.challenge` event immediately; callers are responsible
    /// for pulling that event off `event_rx`, signing it with their
    /// `DeviceIdentity`, and sending a `connect` request before any
    /// other method call succeeds. (Slice 3b packages the handshake.)
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws_stream, _response) = connect_async(url)
            .await
            .with_context(|| format!("openclaw: WebSocket upgrade to {url} failed"))?;

        let (ws_sink, ws_stream) = ws_stream.split();
        let pending = PendingRequests::new();
        let (outbound_tx, outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<super::wire::EventFrame>(EVENT_CAPACITY);
        let connected = Arc::new(AtomicBool::new(true));

        let read_task = tokio::spawn(read_loop(
            ws_stream,
            pending.clone(),
            event_tx,
            connected.clone(),
        ));

        let write_task = tokio::spawn(write_loop(ws_sink, outbound_rx, connected.clone()));

        Ok(Self {
            pending,
            outbound_tx: Some(outbound_tx),
            event_rx: Some(event_rx),
            connected,
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: Some(read_task),
            write_task: Some(write_task),
        })
    }

    /// True while both read and write loops are live. Flips to false on
    /// any WebSocket error or graceful disconnect.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Take ownership of the event receiver. Only one consumer task is
    /// intended to own this at a time (slice 3b's handshake will take
    /// it first to pick off the `connect.challenge` event; the TUI will
    /// take it back for session-message streaming after handshake).
    pub fn take_event_rx(&mut self) -> Option<mpsc::Receiver<super::wire::EventFrame>> {
        self.event_rx.take()
    }

    /// Fire-and-await an RPC: generate a request id, register it in
    /// `pending`, push the frame onto the outbound queue, and await
    /// the correlated response. Times out after
    /// `DEFAULT_REQUEST_TIMEOUT`.
    pub async fn send_request(
        &self,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Result<ResponseFrame> {
        self.send_request_with_timeout(method, params, DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    /// Same as `send_request` but with an explicit timeout. Callers
    /// that know their method blocks on a long-running agent turn
    /// should bump this.
    pub async fn send_request_with_timeout(
        &self,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<ResponseFrame> {
        if !self.is_connected() {
            return Err(anyhow!("openclaw: connection closed"));
        }
        let req = RequestFrame::new(method, params);
        let id = req.id.clone();
        let mut pending = self.pending.register_guard(id.clone()).await;
        let Some(tx) = self.outbound_tx.as_ref() else {
            pending.cancel();
            return Err(anyhow!("openclaw: write loop dropped (connection closed)"));
        };
        let deadline = tokio::time::Instant::now() + timeout;
        match tokio::time::timeout_at(deadline, tx.send(req)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                pending.cancel();
                return Err(anyhow!("openclaw: write loop dropped (connection closed)"));
            }
            Err(_) => {
                pending.cancel();
                return Err(anyhow!(
                    "openclaw: request {id} timed out after {timeout:?}"
                ));
            }
        }

        match tokio::time::timeout_at(deadline, pending.receiver_mut()).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(_)) => {
                pending.cancel();
                Err(anyhow!(
                    "openclaw: request {id} abandoned (connection closed before response)"
                ))
            }
            Err(_) => {
                pending.cancel();
                Err(anyhow!(
                    "openclaw: request {id} timed out after {timeout:?}"
                ))
            }
        }
    }

    /// Graceful shutdown. Signals the write loop to exit, which closes
    /// the socket and causes the read loop to exit too. Abandons any
    /// in-flight requests (their callers see "connection closed").
    pub async fn disconnect(&mut self) {
        // Drop the sender so the write loop sees the mpsc close.
        self.outbound_tx.take();
        self.connected.store(false, Ordering::Relaxed);
        self.pending.abort_all().await;
        if let Some(t) = self.write_task.take() {
            let _ = t.await;
        }
        if let Some(t) = self.read_task.take() {
            // Read loop may be blocked on the socket; it will exit when
            // the socket closes. Wait at most a second.
            let _ = tokio::time::timeout(Duration::from_secs(1), t).await;
        }
    }
}

impl OpenClawClient {
    // ==================================================================
    // connect + handshake + AgentClient-facing method impls
    // ==================================================================

    /// High-level bootstrap: WebSocket upgrade + full handshake in one
    /// call. Returns a client whose hello_ok is populated and whose
    /// send_request can be used for any method in
    /// hello_ok.features.methods.
    pub async fn connect_and_handshake(
        url: &str,
        device: &super::device::DeviceIdentity,
        params: &super::handshake::HandshakeParams,
    ) -> anyhow::Result<Self> {
        Self::connect_and_handshake_with_timeout(url, device, params, DEFAULT_HANDSHAKE_TIMEOUT)
            .await
    }

    pub(crate) async fn connect_and_handshake_with_timeout(
        url: &str,
        device: &super::device::DeviceIdentity,
        params: &super::handshake::HandshakeParams,
        handshake_timeout: Duration,
    ) -> anyhow::Result<Self> {
        match tokio::time::timeout(handshake_timeout, async {
            let client = Self::connect(url).await?;
            Self::finish_handshake(client, device, params).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "openclaw: connect+handshake timed out after {:?}",
                handshake_timeout
            )),
        }
    }

    async fn finish_handshake(
        mut client: Self,
        device: &super::device::DeviceIdentity,
        params: &super::handshake::HandshakeParams,
    ) -> anyhow::Result<Self> {
        let mut event_rx = client
            .event_rx
            .take()
            .expect("fresh client must have event_rx");
        let hello_ok =
            super::handshake::perform_handshake(&client, &mut event_rx, device, params).await?;
        client.event_rx = Some(event_rx);
        client.hello_ok = Some(hello_ok);
        Ok(client)
    }

    #[cfg(test)]
    async fn finish_handshake_with_timeout(
        client: Self,
        device: &super::device::DeviceIdentity,
        params: &super::handshake::HandshakeParams,
        handshake_timeout: Duration,
    ) -> anyhow::Result<Self> {
        match tokio::time::timeout(
            handshake_timeout,
            Self::finish_handshake(client, device, params),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "openclaw: handshake timed out after {:?}",
                handshake_timeout
            )),
        }
    }

    /// Result of the last successful handshake. None if the client
    /// was built via the raw connect (e.g. for protocol-level tests).
    pub fn hello_ok(&self) -> Option<&super::handshake::HelloOk> {
        self.hello_ok.as_ref()
    }

    /// Set the zterm workspace namespace used for deterministic
    /// session-key generation. Callers should set this before exposing
    /// the client through `AgentClient`.
    pub fn set_session_namespace(&mut self, namespace: impl Into<String>) {
        let namespace = namespace.into();
        self.session_namespace = if namespace.trim().is_empty() {
            DEFAULT_SESSION_NAMESPACE.to_string()
        } else {
            namespace
        };
    }

    /// Set prior namespaces that should be considered visible for
    /// list/load/delete, but never used for new session keys.
    pub fn set_session_namespace_aliases<I, S>(&mut self, aliases: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.session_namespace_aliases.clear();
        for alias in aliases {
            let alias = alias.into();
            let alias = alias.trim();
            if !alias.is_empty()
                && alias != self.session_namespace
                && !self
                    .session_namespace_aliases
                    .iter()
                    .any(|existing| existing == alias)
            {
                self.session_namespace_aliases.push(alias.to_string());
            }
        }
    }

    /// Fire the server health RPC. Returns true on success; false
    /// on a server-reported error. Matches the semantics of
    /// ZeroclawClient::health for trait AgentClient.
    pub async fn rpc_health(&self) -> anyhow::Result<bool> {
        let res = self.send_request("health", None).await?;
        Ok(res.ok)
    }

    /// Fire models.list and return the parsed model catalog. Raw
    /// openclaw shape (id, name, provider, alias?, contextWindow?,
    /// reasoning?) — higher-level helpers on top of this reshape
    /// into the zterm-wide Provider / Model types.
    pub async fn rpc_models_list(&self) -> anyhow::Result<Vec<super::handshake::ModelChoice>> {
        use anyhow::Context;
        let res = self.send_request("models.list", None).await?;
        if !res.ok {
            let msg = res
                .error
                .as_ref()
                .map(|e| format!("{} ({})", e.message, e.code))
                .unwrap_or_else(|| "no error body".to_string());
            anyhow::bail!("openclaw: models.list failed: {}", msg);
        }
        let payload = res
            .payload
            .ok_or_else(|| anyhow::anyhow!("openclaw: models.list response missing payload"))?;
        #[derive(serde::Deserialize)]
        struct ModelsListResult {
            models: Vec<super::handshake::ModelChoice>,
        }
        let parsed: ModelsListResult = serde_json::from_value(payload)
            .context("openclaw: models.list payload did not match schema")?;
        Ok(parsed.models)
    }
    /// Fire `sessions.list` with the caller's filter options and
    /// return the parsed row set. Pass `SessionsListOpts::default()`
    /// for the sensible defaults openclaw ships (no derived titles
    /// or preview reads — both are per-session file I/O).
    pub async fn rpc_sessions_list(
        &self,
        opts: SessionsListOpts,
    ) -> anyhow::Result<super::handshake::OpenClawSessionsListResult> {
        use anyhow::Context;
        let mut params = serde_json::Map::new();
        if let Some(limit) = opts.limit {
            params.insert("limit".into(), serde_json::json!(limit));
        }
        if let Some(am) = opts.active_minutes {
            params.insert("activeMinutes".into(), serde_json::json!(am));
        }
        if opts.include_global {
            params.insert("includeGlobal".into(), serde_json::json!(true));
        }
        if opts.include_derived_titles {
            params.insert("includeDerivedTitles".into(), serde_json::json!(true));
        }
        if opts.include_last_message {
            params.insert("includeLastMessage".into(), serde_json::json!(true));
        }
        if let Some(s) = opts.search {
            params.insert("search".into(), serde_json::json!(s));
        }

        let params_val = if params.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(params))
        };
        let res = self.send_request("sessions.list", params_val).await?;
        if !res.ok {
            let err = res
                .error
                .as_ref()
                .map(|e| format!("{} ({})", e.message, e.code))
                .unwrap_or_else(|| "no error body".to_string());
            anyhow::bail!("openclaw: sessions.list failed: {}", err);
        }
        let payload = res
            .payload
            .ok_or_else(|| anyhow::anyhow!("openclaw: sessions.list response missing payload"))?;
        serde_json::from_value(payload)
            .context("openclaw: sessions.list payload did not match SessionsListResult schema")
    }

    /// Fire `sessions.send` and return the initial ack. This is the
    /// fire half of a streaming chat turn — the server acknowledges
    /// the turn synchronously with a `runId` and then broadcasts the
    /// response body via `session.message` events. See slice 3f for
    /// the streaming consumer.
    ///
    /// `key` must be a canonical session key returned by a prior
    /// `sessions.create` or `sessions.list`. The server will reject
    /// free-form keys.
    ///
    /// `opts.idempotency_key` is strongly recommended on retriable
    /// call sites (e.g. reconnect-then-resend) — the server dedupes
    /// by this key and re-acks the same runId for duplicate sends.
    /// If None the server generates a random UUID per call.
    pub async fn rpc_sessions_send(
        &self,
        key: &str,
        message: &str,
        opts: SessionsSendOpts,
    ) -> anyhow::Result<super::handshake::OpenClawSessionSendAck> {
        use anyhow::Context;
        let mut params = serde_json::Map::new();
        params.insert("key".into(), serde_json::json!(key));
        params.insert("message".into(), serde_json::json!(message));
        if let Some(t) = opts.thinking {
            params.insert("thinking".into(), serde_json::json!(t));
        }
        if let Some(ms) = opts.timeout_ms {
            params.insert("timeoutMs".into(), serde_json::json!(ms));
        }
        if let Some(idem) = opts.idempotency_key {
            params.insert("idempotencyKey".into(), serde_json::json!(idem));
        }

        let res = self
            .send_request("sessions.send", Some(serde_json::Value::Object(params)))
            .await?;
        if !res.ok {
            let err = res
                .error
                .as_ref()
                .map(|e| format!("{} ({})", e.message, e.code))
                .unwrap_or_else(|| "no error body".to_string());
            anyhow::bail!("openclaw: sessions.send failed: {}", err);
        }
        let payload = res
            .payload
            .ok_or_else(|| anyhow::anyhow!("openclaw: sessions.send response missing payload"))?;
        serde_json::from_value(payload)
            .context("openclaw: sessions.send payload did not match SessionSendAck schema")
    }

    pub async fn rpc_sessions_abort(&self, key: &str, run_id: Option<&str>) -> anyhow::Result<()> {
        let mut params = serde_json::Map::new();
        params.insert("key".into(), serde_json::json!(key));
        if let Some(run_id) = run_id {
            params.insert("runId".into(), serde_json::json!(run_id));
        }
        let res = self
            .send_request_with_timeout(
                "sessions.abort",
                Some(serde_json::Value::Object(params)),
                Duration::from_secs(5),
            )
            .await?;
        if !res.ok {
            let err = res
                .error
                .as_ref()
                .map(|e| format!("{} ({})", e.message, e.code))
                .unwrap_or_else(|| "no error body".to_string());
            anyhow::bail!("openclaw: sessions.abort failed: {}", err);
        }
        Ok(())
    }

    /// Fire `sessions.create` with optional filter / seed params.
    /// Returns the canonical key of the created session.
    pub async fn rpc_sessions_create(
        &self,
        opts: SessionsCreateOpts,
    ) -> anyhow::Result<super::handshake::OpenClawSessionCreateResult> {
        use anyhow::Context;
        let mut params = serde_json::Map::new();
        let requested_key = opts.key.clone();
        if let Some(v) = opts.key {
            params.insert("key".into(), serde_json::json!(v));
        }
        if let Some(v) = opts.agent_id {
            params.insert("agentId".into(), serde_json::json!(v));
        }
        if let Some(v) = opts.label {
            params.insert("label".into(), serde_json::json!(v));
        }
        if let Some(v) = opts.model {
            params.insert("model".into(), serde_json::json!(v));
        }
        if let Some(v) = opts.message {
            params.insert("message".into(), serde_json::json!(v));
        }

        let params_val = if params.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(params))
        };
        let res = self.send_request("sessions.create", params_val).await?;
        if !res.ok {
            let err = res
                .error
                .as_ref()
                .map(|e| format!("{} ({})", e.message, e.code))
                .unwrap_or_else(|| "no error body".to_string());
            anyhow::bail!("openclaw: sessions.create failed: {}", err);
        }
        let payload = res
            .payload
            .ok_or_else(|| anyhow::anyhow!("openclaw: sessions.create response missing payload"))?;
        let created: super::handshake::OpenClawSessionCreateResult = serde_json::from_value(
            payload,
        )
        .context("openclaw: sessions.create payload did not match SessionCreateResult schema")?;
        if let Some(requested_key) = requested_key {
            if created.key != requested_key {
                anyhow::bail!(
                    "openclaw: sessions.create returned mismatched session key '{}' for requested key '{}'; refusing to bind",
                    created.key,
                    requested_key
                );
            }
        }
        Ok(created)
    }
    /// Fire a chat turn and collect the assistant's reply from the
    /// `session.message` event stream. This is the high-level turn
    /// method the REPL uses — synchronous-feeling from the caller's
    /// perspective, even though the bytes come back as a stream of
    /// events over WebSocket.
    ///
    /// Flow:
    ///   1. `sessions.messages.subscribe(key)` — route session events
    ///      for this session key to our connId
    ///   2. `sessions.send(key, message, opts)` — fire the turn,
    ///      get the `runId`
    ///   3. Loop on `event_rx` pulling `session.message` events whose
    ///      `sessionKey` matches; return the first message with
    ///      `role == "assistant"`
    ///   4. `sessions.messages.unsubscribe(key)` — best-effort cleanup
    ///
    /// Returns the assistant message's `content` field as a String.
    /// Tool-call responses (where `content` is a structured object,
    /// not a plain string) are JSON-stringified — slice 3g can
    /// surface tool calls to the terminal UX properly; for now the
    /// REPL sees raw JSON.
    ///
    /// Borrow contract: takes `&mut self` because `event_rx` lives on
    /// the client as an `Option<Receiver>` and is consumed / restored
    /// across stages of this call. Don't interleave this call with
    /// `take_event_rx()` or another `send_and_collect` on the same
    /// client — single-consumer at a time.
    pub async fn rpc_sessions_send_and_collect(
        &mut self,
        key: &str,
        message: &str,
        opts: SessionsSendOpts,
        timeout: std::time::Duration,
    ) -> anyhow::Result<String> {
        // Stage 1: subscribe — server starts routing session.message
        // events for this key to our connection.
        let sub_res = self
            .send_request(
                "sessions.messages.subscribe",
                Some(serde_json::json!({ "key": key })),
            )
            .await?;
        if !sub_res.ok {
            let err = sub_res
                .error
                .as_ref()
                .map(|e| format!("{} ({})", e.message, e.code))
                .unwrap_or_else(|| "no error body".to_string());
            anyhow::bail!("openclaw: sessions.messages.subscribe failed: {}", err);
        }

        // Stage 2: fire the turn.
        let ack = self.rpc_sessions_send(key, message, opts).await?;
        let expected_run_id = ack.run_id.clone();

        // Stage 3: take event_rx for the collect loop, then put it back
        // on a best-effort basis so subsequent calls still work even if
        // the collect errors out.
        let event_rx = self
            .event_rx
            .take()
            .ok_or_else(|| anyhow::anyhow!("openclaw: event_rx already taken"));
        let mut event_rx = match event_rx {
            Ok(event_rx) => event_rx,
            Err(e) => {
                let e = self
                    .abort_acknowledged_run_after_failure(key, &expected_run_id, e)
                    .await;
                self.unsubscribe_session_messages_best_effort(key).await;
                return Err(e);
            }
        };

        let collect_res = collect_assistant_message(&mut event_rx, key, timeout).await;

        self.event_rx = Some(event_rx);

        let collect_res = match collect_res {
            Ok(message) => Ok(message),
            Err(e) => Err(self
                .abort_acknowledged_run_after_failure(key, &expected_run_id, e)
                .await),
        };

        // Stage 4: unsubscribe — best effort. A failure here is not
        // fatal (server GCs subscribers on disconnect) so we log and
        // return the collect result either way.
        self.unsubscribe_session_messages_best_effort(key).await;

        collect_res
    }

    /// Rich variant of `rpc_sessions_send_and_collect` that returns
    /// the full `TurnResult` (text + tool_calls + tool_results +
    /// thinking + run_id) accumulated across all assistant messages
    /// in the turn.
    ///
    /// Use this when the caller wants to surface tool activity
    /// (REPL renderer in slice B-2). The plain `..._and_collect`
    /// method is a thin wrapper that returns only the `.text`
    /// field for `AgentClient::submit_turn` compatibility.
    pub async fn rpc_sessions_send_and_collect_rich(
        &mut self,
        key: &str,
        message: &str,
        opts: SessionsSendOpts,
        timeout: std::time::Duration,
    ) -> anyhow::Result<super::handshake::TurnResult> {
        // Stage 1: subscribe.
        let sub_res = self
            .send_request(
                "sessions.messages.subscribe",
                Some(serde_json::json!({ "key": key })),
            )
            .await?;
        if !sub_res.ok {
            let err = sub_res
                .error
                .as_ref()
                .map(|e| format!("{} ({})", e.message, e.code))
                .unwrap_or_else(|| "no error body".to_string());
            anyhow::bail!("openclaw: sessions.messages.subscribe failed: {}", err);
        }

        // Stage 2: fire the turn.
        let ack = match self.rpc_sessions_send(key, message, opts).await {
            Ok(ack) => ack,
            Err(e) => {
                self.unsubscribe_session_messages_best_effort(key).await;
                return Err(e);
            }
        };
        let expected_run_id = ack.run_id.clone();
        let run_id = Some(expected_run_id.clone());

        // Stage 3: take event_rx for the collect loop.
        let event_rx = self
            .event_rx
            .take()
            .ok_or_else(|| anyhow::anyhow!("openclaw: event_rx already taken"));
        let mut event_rx = match event_rx {
            Ok(event_rx) => event_rx,
            Err(e) => {
                let e = self
                    .abort_acknowledged_run_after_failure(key, &expected_run_id, e)
                    .await;
                self.unsubscribe_session_messages_best_effort(key).await;
                return Err(e);
            }
        };

        let mut turn = super::handshake::TurnResult {
            run_id,
            ..Default::default()
        };
        let expected_message_id = ack_message_id(&ack).map(str::to_string);
        let expected_message_seq = ack_message_seq(&ack);
        let collect_res = collect_turn_result(
            &mut event_rx,
            key,
            &expected_run_id,
            expected_message_id.as_deref(),
            expected_message_seq,
            timeout,
            &mut turn,
        )
        .await;

        self.event_rx = Some(event_rx);

        let collect_err = match collect_res {
            Ok(()) => None,
            Err(e) => Some(
                self.abort_acknowledged_run_after_failure(key, &expected_run_id, e)
                    .await,
            ),
        };

        // Stage 4: unsubscribe (best effort).
        self.unsubscribe_session_messages_best_effort(key).await;

        if let Some(e) = collect_err {
            return Err(e);
        }
        Ok(turn)
    }

    async fn unsubscribe_session_messages_best_effort(&self, key: &str) {
        if let Err(e) = self
            .send_request(
                "sessions.messages.unsubscribe",
                Some(serde_json::json!({ "key": key })),
            )
            .await
        {
            tracing::debug!("openclaw: sessions.messages.unsubscribe error (ignored): {e}");
        }
    }

    async fn abort_acknowledged_run_after_failure(
        &self,
        key: &str,
        run_id: &str,
        failure: anyhow::Error,
    ) -> anyhow::Error {
        match self.rpc_sessions_abort(key, Some(run_id)).await {
            Ok(()) => anyhow::anyhow!(
                "openclaw: turn collection failed for run {run_id}; abort confirmed: {failure}"
            ),
            Err(abort_error) => anyhow::anyhow!(
                "openclaw: turn collection failed for run {run_id}; abort failed; run state unresolved: {failure}; abort error: {abort_error}"
            ),
        }
    }

    /// Fire `sessions.delete` — remove a session from the store.
    ///
    /// Sets `deleteTranscript: true` so the on-disk transcript file
    /// is removed too — zterm treats delete as a real delete, not
    /// a soft-hide.
    pub async fn rpc_sessions_delete(&self, key: &str) -> anyhow::Result<()> {
        let params = serde_json::json!({ "key": key, "deleteTranscript": true });
        let res = self.send_request("sessions.delete", Some(params)).await?;
        if !res.ok {
            let err = res
                .error
                .as_ref()
                .map(|e| format!("{} ({})", e.message, e.code))
                .unwrap_or_else(|| "no error body".to_string());
            anyhow::bail!("openclaw: sessions.delete failed: {}", err);
        }
        Ok(())
    }
}

impl Drop for OpenClawClient {
    fn drop(&mut self) {
        self.connected.store(false, Ordering::Relaxed);
        // Best-effort: abort loop tasks. Callers who need graceful
        // shutdown should call `disconnect` explicitly before Drop.
        if let Some(t) = self.read_task.take() {
            t.abort();
        }
        if let Some(t) = self.write_task.take() {
            t.abort();
        }
    }
}

// ======================================================================
// read / write loops (private)
// ======================================================================

async fn read_loop(
    mut ws_stream: impl StreamExt<Item = Result<WsMessage, WsError>> + Unpin + Send,
    pending: PendingRequests,
    event_tx: mpsc::Sender<super::wire::EventFrame>,
    connected: Arc<AtomicBool>,
) {
    while let Some(msg_res) = ws_stream.next().await {
        match msg_res {
            Ok(WsMessage::Text(text)) => match Frame::from_json(&text) {
                Ok(Frame::Res(res)) => {
                    let _delivered = pending.resolve(res).await;
                }
                Ok(Frame::Event(ev)) => {
                    if event_requires_reliable_delivery(&ev) {
                        deliver_reliable_event(&event_tx, ev);
                    } else {
                        match event_tx.try_send(ev) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(ev)) => {
                                tracing::debug!(
                                    "openclaw: dropping event {} because event channel is full",
                                    ev.event
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                // Receiver dropped — no one cares about events.
                                // Keep the read loop alive anyway so response
                                // correlation still works.
                            }
                        }
                    }
                }
                Ok(Frame::Req(_)) => {
                    tracing::warn!(
                        "openclaw: server sent a Req frame to client (unexpected); dropping"
                    );
                }
                Err(e) => {
                    tracing::warn!("openclaw: bad frame from server: {e}");
                }
            },
            Ok(WsMessage::Binary(_)) => {
                tracing::warn!("openclaw: binary frame received; protocol is text-only, dropping");
            }
            Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) | Ok(WsMessage::Frame(_)) => {
                // tungstenite handles Ping/Pong automatically at the
                // transport layer; nothing to do here.
            }
            Ok(WsMessage::Close(frame)) => {
                tracing::debug!("openclaw: server closed WebSocket: {frame:?}");
                break;
            }
            Err(e) => {
                tracing::warn!("openclaw: WebSocket read error: {e}");
                break;
            }
        }
    }
    connected.store(false, Ordering::Relaxed);
    pending.abort_all().await;
}

fn event_requires_reliable_delivery(event: &super::wire::EventFrame) -> bool {
    event.event == "session.message" || is_terminal_run_event_name(&event.event)
}

fn deliver_reliable_event(
    event_tx: &mpsc::Sender<super::wire::EventFrame>,
    event: super::wire::EventFrame,
) {
    match event_tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(event)) => {
            let event_name = event.event.clone();
            let event_tx = event_tx.clone();
            let handle = tokio::spawn(async move {
                if event_tx.send(event).await.is_err() {
                    tracing::debug!(
                        "openclaw: dropping reliable event {event_name} because receiver closed"
                    );
                }
            });
            drop(handle);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // Receiver dropped — no one cares about events. Keep the
            // read loop alive anyway so response correlation still works.
        }
    }
}

async fn write_loop(
    mut ws_sink: impl SinkExt<WsMessage, Error = WsError> + Unpin + Send,
    mut outbound_rx: mpsc::Receiver<RequestFrame>,
    connected: Arc<AtomicBool>,
) {
    while let Some(req) = outbound_rx.recv().await {
        let frame = Frame::Req(req);
        let json = match frame.to_json() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("openclaw: failed to encode outbound frame: {e}");
                continue;
            }
        };
        if let Err(e) = ws_sink.send(WsMessage::Text(json)).await {
            tracing::warn!("openclaw: WebSocket write error: {e}");
            break;
        }
    }
    // Best-effort graceful close.
    let _ = ws_sink.send(WsMessage::Close(None)).await;
    connected.store(false, Ordering::Relaxed);
}

// ----------------------------------------------------------------------
// unit tests — the WebSocket lifecycle itself needs a live peer, so
// this module's coverage is about plumbing invariants that compile to
// something testable without one: struct shape, timeout semantics on a
// closed connection.
// ----------------------------------------------------------------------

/// Drain `session.message` events until we see an assistant reply
/// for the given session key, then return its `content` text.
///
/// Non-matching events (other sessions, non-message events) are
/// skipped in place. On timeout returns a clear error so callers
/// can retry or abort. On channel-closed returns an error — the
/// WebSocket has dropped out from under us.
async fn collect_assistant_message(
    event_rx: &mut tokio::sync::mpsc::Receiver<super::wire::EventFrame>,
    session_key: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "openclaw: session.message stream timed out after {:?}",
                timeout
            );
        }

        let rx_res = tokio::time::timeout(remaining, event_rx.recv()).await;
        let event = match rx_res {
            Ok(Some(ev)) => ev,
            Ok(None) => anyhow::bail!("openclaw: event channel closed mid-stream"),
            Err(_) => anyhow::bail!(
                "openclaw: session.message stream timed out after {:?}",
                timeout
            ),
        };

        if event.event != "session.message" {
            continue;
        }

        let payload = match &event.payload {
            Some(p) => p,
            None => continue,
        };

        // Only messages for this session.
        if payload
            .get("sessionKey")
            .and_then(|v| v.as_str())
            .map(|s| s != session_key)
            .unwrap_or(true)
        {
            continue;
        }

        let message = match payload.get("message") {
            Some(m) => m,
            None => continue,
        };

        let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");

        if role != "assistant" {
            continue;
        }

        // Parse content parts and return the display-text view —
        // concatenation of `text`-type parts only. Thinking +
        // tool-call parts are surfaced through AssistantContent's
        // other accessors (see slice 3h + handshake.rs).
        let parsed = match message.get("content") {
            Some(v) => super::handshake::AssistantContent::parse(v),
            None => super::handshake::AssistantContent { parts: Vec::new() },
        };

        // Common agent pattern: first assistant turn is pure tool_use
        // (run a command), second turn carries the actual text reply.
        // Skip tool-only messages and keep consuming — the subscribe
        // is still active, so the next session.message for this key
        // arrives on the same event_rx. Outer timeout bounds the wait.
        if parsed.is_tool_only() {
            tracing::debug!("openclaw: assistant turn was tool-only; waiting for text reply");
            continue;
        }

        return Ok(parsed.display_text());
    }
}

/// Like `collect_assistant_message` but accumulates tool_calls,
/// tool_results, and thinking from all intermediate assistant
/// messages into `turn`, and returns `()` only after the expected
/// run/message reaches an explicit terminal completion marker. The
/// caller owns the TurnResult; this function only mutates it in place.
async fn collect_turn_result(
    event_rx: &mut tokio::sync::mpsc::Receiver<super::wire::EventFrame>,
    session_key: &str,
    expected_run_id: &str,
    expected_ack_message_id: Option<&str>,
    expected_message_seq: Option<u64>,
    timeout: std::time::Duration,
    turn: &mut super::handshake::TurnResult,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut saw_terminal_completion = false;
    let mut expected_message_id: Option<String> = expected_ack_message_id.map(str::to_string);
    let mut pending_runless_messages: Vec<BufferedAssistantMessage> = Vec::new();
    let mut pending_runless_bytes = 0usize;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return finish_completed_or_timeout(timeout, turn, saw_terminal_completion);
        }

        let rx_res = tokio::time::timeout(remaining, event_rx.recv()).await;
        let event = match rx_res {
            Ok(Some(ev)) => ev,
            Ok(None) => anyhow::bail!("openclaw: event channel closed mid-stream"),
            Err(_) => return finish_completed_or_timeout(timeout, turn, saw_terminal_completion),
        };

        if event.event != "session.message" {
            if let Some(completion) = event_marks_expected_turn_completed(
                &event,
                session_key,
                expected_run_id,
                expected_message_id.as_deref(),
            ) {
                saw_terminal_completion = true;
                if expected_message_id.is_none() {
                    expected_message_id = completion.message_id.clone();
                }
                merge_buffered_runless_messages(
                    turn,
                    &mut pending_runless_messages,
                    &mut pending_runless_bytes,
                    expected_message_id.as_deref(),
                )?;
                if turn_can_finish(turn) {
                    return Ok(());
                }
            }
            continue;
        }
        let payload = match &event.payload {
            Some(p) => p,
            None => continue,
        };
        if payload
            .get("sessionKey")
            .and_then(|v| v.as_str())
            .map(|s| s != session_key)
            .unwrap_or(true)
        {
            continue;
        }
        let message = match payload.get("message") {
            Some(m) => m,
            None => continue,
        };
        let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant" {
            continue;
        }
        let message_id = session_message_id(payload, message).map(str::to_string);
        match session_message_run_id(payload, message) {
            Some(run_id) if run_id == expected_run_id => {
                if expected_message_id.is_none() {
                    expected_message_id = message_id.clone();
                }
                merge_buffered_runless_messages(
                    turn,
                    &mut pending_runless_messages,
                    &mut pending_runless_bytes,
                    expected_message_id.as_deref(),
                )?;
            }
            Some(run_id) => {
                tracing::debug!(
                    "openclaw: ignoring stale assistant message for session {session_key}: runId {run_id} != expected {expected_run_id}"
                );
                continue;
            }
            None => {
                if runless_message_matches_expected_turn(
                    payload,
                    message,
                    expected_message_id.as_deref(),
                    expected_message_seq,
                ) {
                    tracing::debug!(
                        "openclaw: accepting runId-less assistant message for session {session_key} using explicit message correlation"
                    );
                } else if let Some(message_id) = message_id {
                    tracing::debug!(
                        "openclaw: buffering runId-less assistant message for session {session_key}; waiting for expected run correlation"
                    );
                    let byte_len = payload.to_string().len();
                    if pending_runless_messages.len() >= RUNLESS_BUFFER_MAX_MESSAGES
                        || pending_runless_bytes.saturating_add(byte_len) > RUNLESS_BUFFER_MAX_BYTES
                    {
                        anyhow::bail!(
                            "openclaw: buffered runId-less assistant messages exceeded cap while waiting for run correlation"
                        );
                    }
                    pending_runless_bytes = pending_runless_bytes.saturating_add(byte_len);
                    pending_runless_messages.push(BufferedAssistantMessage {
                        message_id,
                        byte_len,
                        payload: payload.clone(),
                        message: message.clone(),
                    });
                    continue;
                } else {
                    tracing::debug!(
                        "openclaw: ignoring runId-less assistant message for session {session_key} without explicit run/message correlation"
                    );
                    continue;
                }
            }
        }

        merge_assistant_message_into_turn(turn, payload, message)?;

        let message_completed =
            message_marks_run_completed(payload, message) || saw_terminal_completion;

        // Text-bearing events can be deltas or intermediate snapshots;
        // success still requires the expected run/message completion
        // marker so we do not truncate a streaming reply.
        if message_completed && turn_can_finish(turn) {
            return Ok(());
        }
        tracing::debug!(
            "openclaw: intermediate assistant message ({} tool_calls, {} tool_results so far)",
            turn.tool_calls.len(),
            turn.tool_results.len()
        );
    }
}

struct BufferedAssistantMessage {
    message_id: String,
    byte_len: usize,
    payload: serde_json::Value,
    message: serde_json::Value,
}

fn merge_buffered_runless_messages(
    turn: &mut super::handshake::TurnResult,
    pending: &mut Vec<BufferedAssistantMessage>,
    pending_bytes: &mut usize,
    expected_message_id: Option<&str>,
) -> anyhow::Result<()> {
    let Some(expected_message_id) = expected_message_id else {
        return Ok(());
    };
    let mut idx = 0;
    while idx < pending.len() {
        if pending[idx].message_id == expected_message_id {
            let buffered = pending.remove(idx);
            *pending_bytes = pending_bytes.saturating_sub(buffered.byte_len);
            merge_assistant_message_into_turn(turn, &buffered.payload, &buffered.message)?;
        } else {
            idx += 1;
        }
    }
    Ok(())
}

fn merge_assistant_message_into_turn(
    turn: &mut super::handshake::TurnResult,
    payload: &serde_json::Value,
    message: &serde_json::Value,
) -> anyhow::Result<()> {
    let content = match message.get("content") {
        Some(v) => super::handshake::AssistantContent::parse(v),
        None => super::handshake::AssistantContent { parts: Vec::new() },
    };
    ensure_assistant_message_within_turn_limits(turn, &content, payload, message)?;

    let previous_text = turn.text.clone();
    let content_text = content.display_text();
    let append_text_delta = !content_text.is_empty() && message_is_text_delta(payload, message);
    turn.merge(&content);
    if append_text_delta {
        turn.text = previous_text;
        turn.text.push_str(&content_text);
    }
    turn.usage = crate::cli::agent::TurnUsage::from_json_candidates(message)
        .or_else(|| crate::cli::agent::TurnUsage::from_json_candidates(payload))
        .or(turn.usage);
    Ok(())
}

fn ensure_assistant_message_within_turn_limits(
    turn: &super::handshake::TurnResult,
    content: &super::handshake::AssistantContent,
    payload: &serde_json::Value,
    message: &serde_json::Value,
) -> anyhow::Result<()> {
    let content_text = content.display_text();
    let projected_text_len = if content_text.is_empty() {
        turn.text.len()
    } else if message_is_text_delta(payload, message) {
        turn.text.len().saturating_add(content_text.len())
    } else {
        content_text.len()
    };
    let incoming_thinking = content.thinking_text();
    let projected_thinking_len = if incoming_thinking.is_empty() {
        turn.thinking.len()
    } else {
        turn.thinking
            .len()
            .saturating_add(usize::from(!turn.thinking.is_empty()))
            .saturating_add(incoming_thinking.len())
    };
    let mut projected_tool_items = turn
        .tool_calls
        .len()
        .saturating_add(turn.tool_results.len());
    let mut projected_tool_bytes = turn_result_tool_bytes(turn);
    for part in &content.parts {
        match part {
            super::handshake::AssistantContentPart::ToolUse { raw, .. }
            | super::handshake::AssistantContentPart::ToolResult { raw } => {
                projected_tool_items = projected_tool_items.saturating_add(1);
                projected_tool_bytes = projected_tool_bytes.saturating_add(raw.to_string().len());
            }
            _ => {}
        }
    }
    let projected_bytes = projected_text_len
        .saturating_add(projected_thinking_len)
        .saturating_add(projected_tool_bytes);
    if projected_bytes > ACCEPTED_TURN_MAX_BYTES
        || projected_tool_items > ACCEPTED_TURN_MAX_TOOL_ITEMS
    {
        anyhow::bail!("openclaw: accepted assistant turn exceeded cap while collecting run output");
    }
    Ok(())
}

fn turn_result_tool_bytes(turn: &super::handshake::TurnResult) -> usize {
    turn.tool_calls
        .iter()
        .chain(turn.tool_results.iter())
        .map(|value| value.to_string().len())
        .fold(0usize, usize::saturating_add)
}

fn runless_message_matches_expected_turn(
    payload: &serde_json::Value,
    message: &serde_json::Value,
    expected_message_id: Option<&str>,
    expected_message_seq: Option<u64>,
) -> bool {
    expected_message_id
        .zip(session_message_id(payload, message))
        .map(|(expected, actual)| expected == actual)
        .unwrap_or(false)
        || expected_message_seq
            .zip(session_message_seq(payload, message))
            .map(|(expected, actual)| expected == actual)
            .unwrap_or(false)
}

fn session_message_run_id<'a>(
    payload: &'a serde_json::Value,
    message: &'a serde_json::Value,
) -> Option<&'a str> {
    message
        .get("runId")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("runId").and_then(|v| v.as_str()))
        .or_else(|| message.get("run_id").and_then(|v| v.as_str()))
        .or_else(|| payload.get("run_id").and_then(|v| v.as_str()))
}

fn session_message_id<'a>(
    payload: &'a serde_json::Value,
    message: &'a serde_json::Value,
) -> Option<&'a str> {
    message
        .get("messageId")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("messageId").and_then(|v| v.as_str()))
        .or_else(|| message.get("message_id").and_then(|v| v.as_str()))
        .or_else(|| payload.get("message_id").and_then(|v| v.as_str()))
        .or_else(|| message.get("id").and_then(|v| v.as_str()))
        .or_else(|| payload.get("id").and_then(|v| v.as_str()))
}

fn session_message_seq(payload: &serde_json::Value, message: &serde_json::Value) -> Option<u64> {
    value_message_seq(message).or_else(|| value_message_seq(payload))
}

fn value_message_seq(value: &serde_json::Value) -> Option<u64> {
    value
        .get("messageSeq")
        .or_else(|| value.get("message_seq"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
                .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
}

fn ack_message_id(ack: &super::handshake::OpenClawSessionSendAck) -> Option<&str> {
    ack.extra
        .get("messageId")
        .and_then(|v| v.as_str())
        .or_else(|| ack.extra.get("message_id").and_then(|v| v.as_str()))
        .or_else(|| ack.extra.get("assistantMessageId").and_then(|v| v.as_str()))
        .or_else(|| {
            ack.extra
                .get("assistant_message_id")
                .and_then(|v| v.as_str())
        })
}

fn ack_message_seq(ack: &super::handshake::OpenClawSessionSendAck) -> Option<u64> {
    ack.message_seq
        .map(u64::from)
        .or_else(|| value_message_seq(&serde_json::Value::Object(ack.extra.clone())))
}

#[derive(Debug, Clone)]
struct TurnCompletion {
    message_id: Option<String>,
}

fn event_marks_expected_turn_completed(
    event: &super::wire::EventFrame,
    session_key: &str,
    expected_run_id: &str,
    expected_message_id: Option<&str>,
) -> Option<TurnCompletion> {
    if !is_terminal_run_event_name(&event.event) {
        return None;
    }
    let payload = event.payload.as_ref()?;
    if !payload_matches_session_key(payload, session_key) {
        return None;
    }
    let message_id = event_payload_message_id(payload).map(str::to_string);
    if event_payload_run_id(payload)
        .map(|run_id| run_id == expected_run_id)
        .unwrap_or(false)
    {
        return Some(TurnCompletion { message_id });
    }
    let matches_expected_message = expected_message_id
        .zip(event_payload_message_id(payload))
        .map(|(expected, actual)| expected == actual)
        .unwrap_or(false);
    matches_expected_message.then_some(TurnCompletion { message_id })
}

fn is_terminal_run_event_name(event: &str) -> bool {
    matches!(
        event,
        "session.run.completed"
            | "session.run.complete"
            | "session.run.finished"
            | "sessions.run.completed"
            | "sessions.run.complete"
            | "sessions.run.finished"
            | "session.message.completed"
            | "session.message.finished"
    )
}

fn message_marks_run_completed(payload: &serde_json::Value, message: &serde_json::Value) -> bool {
    value_marks_completed(message) || value_marks_completed(payload)
}

fn message_is_text_delta(payload: &serde_json::Value, message: &serde_json::Value) -> bool {
    value_marks_delta(message) || value_marks_delta(payload)
}

fn value_marks_delta(value: &serde_json::Value) -> bool {
    for key in ["state", "kind", "phase"] {
        if value
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("delta"))
            .unwrap_or(false)
        {
            return true;
        }
    }
    for key in ["delta", "isDelta"] {
        if value.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn value_marks_completed(value: &serde_json::Value) -> bool {
    const COMPLETED_WORDS: &[&str] = &["completed", "complete", "succeeded", "success", "done"];
    for key in ["status", "state", "phase"] {
        if let Some(status) = value.get(key).and_then(|v| v.as_str()) {
            let normalized = status.to_ascii_lowercase();
            if COMPLETED_WORDS.contains(&normalized.as_str()) {
                return true;
            }
        }
    }
    for key in ["completed", "done", "final", "isFinal"] {
        if value.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn payload_matches_session_key(payload: &serde_json::Value, session_key: &str) -> bool {
    ["sessionKey", "session_key", "key"]
        .into_iter()
        .filter_map(|key| payload.get(key).and_then(|v| v.as_str()))
        .any(|candidate| candidate == session_key)
}

fn value_run_id(value: &serde_json::Value) -> Option<&str> {
    value
        .get("runId")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("run_id").and_then(|v| v.as_str()))
}

fn event_payload_run_id(payload: &serde_json::Value) -> Option<&str> {
    value_run_id(payload).or_else(|| {
        payload
            .get("message")
            .and_then(|message| value_run_id(message))
    })
}

fn value_message_id(value: &serde_json::Value) -> Option<&str> {
    value
        .get("messageId")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("message_id").and_then(|v| v.as_str()))
        .or_else(|| value.get("id").and_then(|v| v.as_str()))
}

fn event_payload_message_id(payload: &serde_json::Value) -> Option<&str> {
    value_message_id(payload).or_else(|| {
        payload
            .get("message")
            .and_then(|message| value_message_id(message))
    })
}

fn turn_can_finish_without_text(turn: &super::handshake::TurnResult) -> bool {
    turn.text.is_empty() && (!turn.tool_calls.is_empty() || !turn.tool_results.is_empty())
}

fn turn_can_finish(turn: &super::handshake::TurnResult) -> bool {
    !turn.text.is_empty() || turn_can_finish_without_text(turn)
}

fn finish_completed_or_timeout(
    timeout: std::time::Duration,
    turn: &super::handshake::TurnResult,
    saw_terminal_completion: bool,
) -> anyhow::Result<()> {
    if saw_terminal_completion && turn_can_finish(turn) {
        return Ok(());
    }
    anyhow::bail!(
        "openclaw: session.message stream timed out after {:?}",
        timeout
    );
}

/// Filter + include options for `OpenClawClient::rpc_sessions_list`.
///
/// `Default` yields no filters, no derived-title reads, no
/// last-message previews — the cheap "just enumerate keys" path.
/// Setting `include_derived_titles` or `include_last_message`
/// triggers a per-session file read on the server; cap the result
/// set with `limit` on large stores.
#[derive(Debug, Clone, Default)]
pub struct SessionsListOpts {
    pub limit: Option<u32>,
    pub active_minutes: Option<u32>,
    pub include_global: bool,
    pub include_derived_titles: bool,
    pub include_last_message: bool,
    pub search: Option<String>,
}

/// Options for `OpenClawClient::rpc_sessions_send`. Only the
/// commonly-needed openclaw fields are exposed; attachments are
/// out of scope for v0.2 (the schema takes an unknown[] — zterm's
/// terminal UX has no way to surface attached content anyway).
#[derive(Debug, Clone, Default)]
pub struct SessionsSendOpts {
    /// Optional thinking prefix to attach to the turn. Passes
    /// through to the underlying model as a reasoning directive
    /// (e.g. "think step by step" for models that support it).
    pub thinking: Option<String>,

    /// Server-side timeout in milliseconds. None uses the gateway's
    /// configured default (varies by model / provider).
    pub timeout_ms: Option<u64>,

    /// Caller-supplied idempotency key — the server dedupes by this
    /// key and re-acks the same runId on duplicate sends. Generate a
    /// UUID per logical turn and reuse across retries.
    pub idempotency_key: Option<String>,
}

/// Creation options for `OpenClawClient::rpc_sessions_create`.
///
/// All fields optional — openclaw will synthesize a key if the
/// caller doesn't supply one.
#[derive(Debug, Clone, Default)]
pub struct SessionsCreateOpts {
    pub key: Option<String>,
    pub agent_id: Option<String>,
    pub label: Option<String>,
    pub model: Option<String>,
    pub message: Option<String>,
}

#[cfg(test)]
pub(super) fn tests_support_new_fake(
    pending: PendingRequests,
    outbound_tx: Option<mpsc::Sender<RequestFrame>>,
    connected: bool,
) -> OpenClawClient {
    let (_event_tx, event_rx) = mpsc::channel::<super::wire::EventFrame>(1);
    OpenClawClient {
        pending,
        outbound_tx,
        event_rx: Some(event_rx),
        connected: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(connected)),
        hello_ok: None,
        stream_sink: None,
        session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
        session_namespace_aliases: Vec::new(),
        read_task: None,
        write_task: None,
    }
}

// =====================================================================
// AgentClient trait impl for OpenClawClient
//
// Maps the trait surface onto the RPC helpers in this module plus a
// small amount of reshape logic for list_providers / get_models /
// list_provider_models (all derive from models.list).
// =====================================================================

use crate::cli::agent::{AgentClient, StreamSink, TurnChunk};
use crate::cli::client::{Config, Model, Provider, Session};

const SUBMIT_TURN_DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const RUNLESS_BUFFER_MAX_MESSAGES: usize = 64;
const RUNLESS_BUFFER_MAX_BYTES: usize = 256 * 1024;
const ACCEPTED_TURN_MAX_BYTES: usize = 2 * 1024 * 1024;
const ACCEPTED_TURN_MAX_TOOL_ITEMS: usize = 256;

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn display_name_for_row(row: &super::handshake::OpenClawSessionRow) -> String {
    row.derived_title
        .clone()
        .or_else(|| row.display_name.clone())
        .or_else(|| row.label.clone())
        .unwrap_or_else(|| row.key.clone())
}

fn row_into_session(row: super::handshake::OpenClawSessionRow) -> Session {
    let name = display_name_for_row(&row);
    Session {
        id: row.key,
        name,
        model: String::new(),
        provider: String::new(),
    }
}

fn choice_into_model(m: super::handshake::ModelChoice) -> Model {
    Model {
        id: m.id,
        display_name: m.name,
        provider: m.provider,
        context_window: m.context_window.map(|n| n as usize),
        supports_reasoning: m.reasoning.unwrap_or(false),
    }
}

#[async_trait::async_trait]
impl AgentClient for OpenClawClient {
    async fn health(&self) -> anyhow::Result<bool> {
        self.rpc_health().await
    }

    async fn get_config(&self) -> anyhow::Result<Config> {
        anyhow::bail!(
            "openclaw: get_config not supported — openclaw gateway has no HTTP config              surface (pure-WS protocol). Use rpc_models_list() / rpc_sessions_list()              for discoverable state instead."
        )
    }

    async fn put_config(&self, _config: &Config) -> anyhow::Result<()> {
        anyhow::bail!(
            "openclaw: put_config not supported — config mutation is not part of the              gateway protocol. Edit ~/.openclaw/openclaw.json out-of-band and restart."
        )
    }

    async fn list_providers(&self) -> anyhow::Result<Vec<Provider>> {
        use std::collections::BTreeSet;
        let models = self.rpc_models_list().await?;
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut providers = Vec::new();
        for m in models {
            if seen.insert(m.provider.clone()) {
                providers.push(Provider {
                    id: m.provider.clone(),
                    name: m.provider,
                    requires_key: false,
                    api_key_env: None,
                });
            }
        }
        Ok(providers)
    }

    async fn get_models(&self, provider: &str) -> anyhow::Result<Vec<Model>> {
        let all = self.rpc_models_list().await?;
        Ok(all
            .into_iter()
            .filter(|m| m.provider == provider)
            .map(choice_into_model)
            .collect())
    }

    async fn list_provider_models(&self, provider: &str) -> anyhow::Result<Vec<String>> {
        let all = self.rpc_models_list().await?;
        Ok(all
            .into_iter()
            .filter(|m| m.provider == provider)
            .map(|m| m.id)
            .collect())
    }

    fn current_model_label(&self) -> String {
        "openclaw default".to_string()
    }

    async fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        let limit = 200;
        let mut sessions = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for namespace in self.session_namespaces() {
            let result = self
                .rpc_sessions_list(SessionsListOpts {
                    limit: Some(limit),
                    search: Some(session_key_prefix(namespace)),
                    ..Default::default()
                })
                .await?;
            ensure_session_list_not_truncated(&result, limit)?;
            for row in self.namespaced_session_rows(result.sessions) {
                if seen.insert(row.key.clone()) {
                    sessions.push(row_into_session(row));
                }
            }
        }
        let legacy_result = match self
            .rpc_sessions_list(SessionsListOpts {
                limit: Some(500),
                include_derived_titles: true,
                ..Default::default()
            })
            .await
        {
            Ok(result) => result,
            Err(e) if !sessions.is_empty() => {
                tracing::warn!(
                    "openclaw: legacy session compatibility scan failed; returning scoped sessions only: {e}"
                );
                return Ok(sessions);
            }
            Err(e) => return Err(e),
        };
        if let Err(e) = ensure_session_list_not_truncated(&legacy_result, 500) {
            if sessions.is_empty() {
                return Err(e);
            }
            tracing::warn!(
                "openclaw: legacy session compatibility scan was truncated; returning scoped sessions only: {e}"
            );
            return Ok(sessions);
        }
        for row in self.legacy_server_key_session_rows(legacy_result.sessions) {
            if seen.insert(row.key.clone()) {
                sessions.push(row_into_session(row));
            }
        }
        Ok(sessions)
    }

    async fn create_session(&self, name: &str) -> anyhow::Result<Session> {
        let key = stable_session_key(&self.session_namespace, name);
        let created = match self
            .rpc_sessions_create(SessionsCreateOpts {
                key: Some(key.clone()),
                label: Some(name.to_string()),
                ..Default::default()
            })
            .await
        {
            Ok(created) => created,
            Err(err) if is_already_exists_error(&err) => {
                return self.load_session_key_exact(&key).await;
            }
            Err(err) => return Err(err),
        };
        let created_label = created.label.as_deref().unwrap_or(name);
        if created.key != key {
            anyhow::bail!(
                "openclaw: sessions.create returned mismatched session key '{}' for requested key '{}' (label '{}'); refusing to bind",
                created.key,
                key,
                created_label
            );
        }
        Ok(Session {
            id: created.key.clone(),
            name: created.label.unwrap_or_else(|| created.key.clone()),
            model: String::new(),
            provider: String::new(),
        })
    }

    async fn load_session(&self, session_id: &str) -> anyhow::Result<Session> {
        let limit = 500;
        let result = self
            .rpc_sessions_list(SessionsListOpts {
                limit: Some(limit),
                search: Some(session_id.to_string()),
                ..Default::default()
            })
            .await?;
        for row in &result.sessions {
            if self.row_belongs_to_session_namespace(row)
                && (row.key == session_id || row.session_id.as_deref() == Some(session_id))
            {
                return Ok(row_into_session(row.clone()));
            }
        }
        ensure_targeted_session_list_not_truncated(&result, limit, session_id)?;
        anyhow::bail!("openclaw: session not found: {session_id}")
    }

    async fn delete_session(&self, session_id: &str) -> anyhow::Result<()> {
        let session = self.load_session(session_id).await?;
        self.rpc_sessions_delete(&session.id).await
    }

    async fn submit_turn(&mut self, session_id: &str, message: &str) -> anyhow::Result<String> {
        let result = self
            .rpc_sessions_send_and_collect_rich(
                session_id,
                message,
                SessionsSendOpts {
                    timeout_ms: Some(duration_millis_u64(SUBMIT_TURN_DEFAULT_TIMEOUT)),
                    idempotency_key: Some(uuid::Uuid::new_v4().to_string()),
                    ..Default::default()
                },
                SUBMIT_TURN_DEFAULT_TIMEOUT,
            )
            .await;

        match result {
            Ok(turn) => {
                if let Some(sink) = &self.stream_sink {
                    if !turn.text.is_empty() {
                        let _ = sink.send(TurnChunk::Token(turn.text.clone()));
                    }
                    if let Some(usage) = turn.usage {
                        let _ = sink.send(TurnChunk::Usage(usage));
                    }
                    let _ = sink.send(TurnChunk::Finished(Ok(turn.text.clone())));
                }
                Ok(turn.text)
            }
            Err(e) => {
                if let Some(sink) = &self.stream_sink {
                    let _ = sink.send(TurnChunk::Finished(Err(e.to_string())));
                }
                Err(e)
            }
        }
    }

    fn set_stream_sink(&mut self, sink: Option<StreamSink>) {
        self.stream_sink = sink;
    }
}

impl OpenClawClient {
    async fn load_session_key_exact(&self, key: &str) -> anyhow::Result<Session> {
        let limit = 500;
        let result = self
            .rpc_sessions_list(SessionsListOpts {
                limit: Some(limit),
                search: Some(key.to_string()),
                ..Default::default()
            })
            .await?;
        for row in &result.sessions {
            if row.key == key && self.row_belongs_to_session_namespace(row) {
                return Ok(row_into_session(row.clone()));
            }
        }
        ensure_targeted_session_list_not_truncated(&result, limit, key)?;
        anyhow::bail!("openclaw: session not found: {key}")
    }

    fn namespaced_session_rows(
        &self,
        rows: Vec<super::handshake::OpenClawSessionRow>,
    ) -> Vec<super::handshake::OpenClawSessionRow> {
        rows.into_iter()
            .filter(|row| self.row_belongs_to_session_namespace(row))
            .collect()
    }

    fn row_belongs_to_session_namespace(&self, row: &super::handshake::OpenClawSessionRow) -> bool {
        self.stable_row_belongs_to_session_namespace(row)
            || self.legacy_server_key_row_belongs_to_session_namespace(row)
    }

    fn stable_row_belongs_to_session_namespace(
        &self,
        row: &super::handshake::OpenClawSessionRow,
    ) -> bool {
        if self.session_key_has_session_namespace_prefix(&row.key) {
            return true;
        }
        row.label
            .as_deref()
            .map(|label| self.session_key_belongs_to_session_namespace(&row.key, label))
            .unwrap_or(false)
    }

    fn legacy_server_key_session_rows(
        &self,
        rows: Vec<super::handshake::OpenClawSessionRow>,
    ) -> Vec<super::handshake::OpenClawSessionRow> {
        rows.into_iter()
            .filter(|row| {
                !self.stable_row_belongs_to_session_namespace(row)
                    && self.legacy_server_key_row_belongs_to_session_namespace(row)
            })
            .collect()
    }

    fn legacy_server_key_row_belongs_to_session_namespace(
        &self,
        row: &super::handshake::OpenClawSessionRow,
    ) -> bool {
        self.row_metadata_matches_session_namespace(row)
    }

    fn session_key_belongs_to_session_namespace(&self, key: &str, label: &str) -> bool {
        self.session_namespaces().into_iter().any(|namespace| {
            key == stable_session_key(namespace, label)
                || key == legacy_stable_session_key(namespace, label)
        })
    }

    fn session_key_has_session_namespace_prefix(&self, key: &str) -> bool {
        self.session_namespaces()
            .into_iter()
            .any(|namespace| key.starts_with(&session_key_prefix(namespace)))
    }

    fn session_namespaces(&self) -> Vec<&str> {
        let mut namespaces = Vec::with_capacity(1 + self.session_namespace_aliases.len());
        namespaces.push(self.session_namespace.as_str());
        namespaces.extend(self.session_namespace_aliases.iter().map(String::as_str));
        namespaces
    }

    fn row_metadata_matches_session_namespace(
        &self,
        row: &super::handshake::OpenClawSessionRow,
    ) -> bool {
        let metadata = row_identity_metadata(row);
        if metadata.is_empty() {
            return false;
        }

        let identities: Vec<SessionNamespaceIdentity> = self
            .session_namespaces()
            .into_iter()
            .map(SessionNamespaceIdentity::from_namespace)
            .collect();

        if !metadata.session_namespaces.is_empty() {
            return identities
                .iter()
                .any(|identity| identity.matches_session_namespace_fields(&metadata));
        }

        identities
            .iter()
            .any(|identity| identity.matches_workspace_identity_fields(&metadata))
    }
}

#[derive(Debug)]
struct SessionNamespaceIdentity<'a> {
    namespace: &'a str,
    workspace_id: Option<&'a str>,
    workspace_name: Option<&'a str>,
    workspace_url: Option<&'a str>,
}

impl<'a> SessionNamespaceIdentity<'a> {
    fn from_namespace(namespace: &'a str) -> Self {
        Self {
            namespace,
            workspace_id: namespace_component(namespace, "workspace_id"),
            workspace_name: namespace_component(namespace, "workspace"),
            workspace_url: namespace_component(namespace, "url"),
        }
    }

    fn matches_session_namespace_fields(&self, metadata: &RowIdentityMetadata) -> bool {
        metadata
            .session_namespaces
            .iter()
            .any(|value| value.trim() == self.namespace)
    }

    fn matches_workspace_identity_fields(&self, metadata: &RowIdentityMetadata) -> bool {
        if let Some(workspace_id) = self.workspace_id {
            return metadata
                .workspace_ids
                .iter()
                .any(|value| value.trim() == workspace_id);
        }

        match (self.workspace_name, self.workspace_url) {
            (Some(name), Some(url)) => {
                let has_name = metadata
                    .workspace_names
                    .iter()
                    .any(|value| value.trim() == name);
                let has_url = metadata
                    .workspace_urls
                    .iter()
                    .any(|value| urls_equivalent(value.trim(), url));
                has_name && has_url
            }
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
struct RowIdentityMetadata {
    session_namespaces: Vec<String>,
    workspace_ids: Vec<String>,
    workspace_names: Vec<String>,
    workspace_urls: Vec<String>,
}

impl RowIdentityMetadata {
    fn is_empty(&self) -> bool {
        self.session_namespaces.is_empty()
            && self.workspace_ids.is_empty()
            && self.workspace_names.is_empty()
            && self.workspace_urls.is_empty()
    }
}

fn namespace_component<'a>(namespace: &'a str, key: &str) -> Option<&'a str> {
    namespace.split(';').find_map(|part| {
        let (part_key, value) = part.split_once('=')?;
        (part_key == key && !value.trim().is_empty()).then_some(value.trim())
    })
}

fn urls_equivalent(actual: &str, expected: &str) -> bool {
    actual.trim_end_matches('/') == expected.trim_end_matches('/')
}

fn row_identity_metadata(row: &super::handshake::OpenClawSessionRow) -> RowIdentityMetadata {
    let mut out = RowIdentityMetadata::default();
    for (key, value) in &row.extra {
        collect_identity_metadata_for_key(key, value, &mut out);
    }
    out
}

fn collect_identity_metadata_for_key(
    key: &str,
    value: &serde_json::Value,
    out: &mut RowIdentityMetadata,
) {
    let normalized_key = normalize_metadata_key(key);

    match value {
        serde_json::Value::String(s) => {
            if let Some(target) = identity_metadata_target(&normalized_key, out) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    target.push(trimmed.to_string());
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_identity_metadata_for_key(key, item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (child_key, value) in map {
                collect_identity_metadata_for_key(child_key, value, out);
            }
        }
        _ => {}
    }
}

fn normalize_metadata_key(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn identity_metadata_target<'a>(
    key: &str,
    metadata: &'a mut RowIdentityMetadata,
) -> Option<&'a mut Vec<String>> {
    match key {
        "sessionnamespace" | "ztermsessionnamespace" | "workspacenamespace" => {
            Some(&mut metadata.session_namespaces)
        }
        "workspaceid" | "ztermworkspaceid" => Some(&mut metadata.workspace_ids),
        "workspacename" => Some(&mut metadata.workspace_names),
        "workspaceurl" => Some(&mut metadata.workspace_urls),
        _ => None,
    }
}

fn ensure_targeted_session_list_not_truncated(
    result: &super::handshake::OpenClawSessionsListResult,
    requested_limit: u32,
    target: &str,
) -> anyhow::Result<()> {
    let returned = result.sessions.len() as u32;
    if result.count > returned || returned >= requested_limit {
        anyhow::bail!(
            "openclaw: targeted session lookup for '{target}' was truncated (returned {returned}, count {}, limit {requested_limit}); refusing to treat absence as not found",
            result.count
        );
    }
    Ok(())
}

fn ensure_session_list_not_truncated(
    result: &super::handshake::OpenClawSessionsListResult,
    requested_limit: u32,
) -> anyhow::Result<()> {
    let returned = result.sessions.len() as u32;
    if result.count > returned || returned >= requested_limit {
        anyhow::bail!(
            "openclaw: sessions.list response was truncated (returned {returned}, count {}, limit {requested_limit}); refusing to return a partial session list",
            result.count
        );
    }
    Ok(())
}

fn stable_session_key(namespace: &str, label: &str) -> String {
    let slug = session_label_slug(label);
    let digest = session_key_digest(namespace, label);
    format!("{}{slug}-{}", session_key_prefix(namespace), &digest[..16])
}

fn legacy_stable_session_key(namespace: &str, label: &str) -> String {
    let slug = session_label_slug(label);
    let digest = session_key_digest(namespace, label);
    format!("zterm-{slug}-{}", &digest[..16])
}

fn session_key_prefix(namespace: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.trim().as_bytes());
    let digest = hex_lower(&hasher.finalize());
    format!("zterm-ns-{}-", &digest[..12])
}

fn session_key_digest(namespace: &str, label: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.trim().as_bytes());
    hasher.update(b"\0");
    hasher.update(label.as_bytes());
    hex_lower(&hasher.finalize())
}

fn session_label_slug(label: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in label.trim().chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, '-' | '_' | '.') {
            Some(ch)
        } else if ch.is_whitespace() {
            Some('-')
        } else {
            None
        };

        if let Some(mapped) = mapped {
            if mapped == '-' {
                if last_dash {
                    continue;
                }
                last_dash = true;
            } else {
                last_dash = false;
            }
            slug.push(mapped);
        }
    }

    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "session" } else { slug };
    slug.chars().take(40).collect()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn is_already_exists_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    (msg.contains("already") && msg.contains("exist")) || msg.contains("conflict")
}

#[cfg(test)]
mod tests {
    use super::super::device::DeviceIdentity;
    use super::super::handshake::{ClientIdentity, HandshakeParams};
    use super::*;

    fn sample_handshake_params() -> HandshakeParams {
        HandshakeParams {
            client: ClientIdentity {
                id: "cli".to_string(),
                display_name: Some("zterm".to_string()),
                version: "0.1.0".to_string(),
                mode: "cli".to_string(),
                platform: "linux".to_string(),
                device_family: None,
            },
            role: "operator".to_string(),
            scopes: vec!["operator.read".to_string(), "operator.write".to_string()],
            token: None,
        }
    }

    #[test]
    fn openclaw_client_implements_agent_client() {
        // Compile-time assertion: dropping impl AgentClient for
        // OpenClawClient will fail the test suite here rather than
        // leave the trait dangling. Mirrors the identical check on
        // ZeroclawClient in cli/client.rs.
        fn assert_agent_client<T: crate::cli::agent::AgentClient>() {}
        assert_agent_client::<OpenClawClient>();
    }

    #[tokio::test]
    async fn connect_and_handshake_times_out_when_gateway_never_sends_challenge() {
        let (_event_tx, event_rx) = mpsc::channel::<super::super::wire::EventFrame>(1);
        let client = OpenClawClient {
            pending: PendingRequests::new(),
            outbound_tx: None,
            event_rx: Some(event_rx),
            connected: Arc::new(AtomicBool::new(true)),
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: None,
            write_task: None,
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let device = DeviceIdentity::create(&tmp.path().join("device.pem")).unwrap();
        let params = sample_handshake_params();

        let err = OpenClawClient::finish_handshake_with_timeout(
            client,
            &device,
            &params,
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("handshake timed out"));
    }

    #[test]
    fn row_into_session_uses_key_as_canonical_id_not_session_id() {
        let session = row_into_session(super::super::handshake::OpenClawSessionRow {
            key: "canonical-key".to_string(),
            kind: "direct".to_string(),
            label: Some("Label".to_string()),
            display_name: None,
            derived_title: None,
            updated_at: None,
            session_id: Some("compat-session-id".to_string()),
            extra: serde_json::Map::new(),
        });

        assert_eq!(session.id, "canonical-key");
        assert_eq!(session.name, "Label");
    }

    #[test]
    fn stable_session_key_is_deterministic_and_sanitized() {
        let first = stable_session_key("workspace:alpha", "Research Notes");
        let second = stable_session_key("workspace:alpha", "Research Notes");

        assert_eq!(first, second);
        assert!(first.starts_with(&session_key_prefix("workspace:alpha")));
        assert!(first.contains("research-notes"));
        assert!(!first.contains(' '));
        assert_ne!(first, stable_session_key("workspace:alpha", "Research"));
    }

    #[test]
    fn stable_session_key_is_namespaced() {
        let alpha = stable_session_key("backend=openclaw;workspace=alpha", "Research");
        let beta = stable_session_key("backend=openclaw;workspace=beta", "Research");

        assert_ne!(alpha, beta);
        assert_eq!(
            alpha,
            stable_session_key("backend=openclaw;workspace=alpha", "Research")
        );
        assert_ne!(
            session_key_prefix("backend=openclaw;workspace=alpha"),
            session_key_prefix("backend=openclaw;workspace=beta")
        );
    }

    #[test]
    fn namespace_match_accepts_legacy_hashed_session_keys() {
        let client = tests_support_new_fake(PendingRequests::new(), None, true);
        let legacy_key = legacy_stable_session_key(DEFAULT_SESSION_NAMESPACE, "Research");
        assert!(client.session_key_belongs_to_session_namespace(&legacy_key, "Research"));
    }

    #[test]
    fn stable_namespace_rows_do_not_require_optional_label() {
        let client = tests_support_new_fake(PendingRequests::new(), None, true);
        let row = super::super::handshake::OpenClawSessionRow {
            key: stable_session_key(DEFAULT_SESSION_NAMESPACE, "Research"),
            kind: "direct".to_string(),
            label: None,
            display_name: None,
            derived_title: None,
            updated_at: None,
            session_id: None,
            extra: serde_json::Map::new(),
        };

        assert!(client.row_belongs_to_session_namespace(&row));
    }

    #[test]
    fn legacy_metadata_matching_only_trusts_exact_identity_fields() {
        let mut client = tests_support_new_fake(PendingRequests::new(), None, true);
        let active_namespace =
            "backend=openclaw;workspace_id=ws_active;workspace=alpha;url=ws://shared";
        let foreign_namespace =
            "backend=openclaw;workspace_id=ws_foreign;workspace=beta;url=ws://shared";
        client.set_session_namespace(active_namespace);

        let exact_namespace_row = test_session_row_with_extra(
            "oc-server-exact-namespace",
            serde_json::json!({
                "metadata": {
                    "zterm": { "sessionNamespace": active_namespace }
                }
            }),
        );
        assert!(client.legacy_server_key_row_belongs_to_session_namespace(&exact_namespace_row));

        let exact_workspace_id_row = test_session_row_with_extra(
            "oc-server-exact-workspace-id",
            serde_json::json!({
                "metadata": {
                    "zterm": { "workspaceId": "ws_active" }
                }
            }),
        );
        assert!(client.legacy_server_key_row_belongs_to_session_namespace(&exact_workspace_id_row));

        let spoofed_container_row = test_session_row_with_extra(
            "oc-server-spoofed-container",
            serde_json::json!({
                "metadata": {
                    "zterm": {
                        "notes": format!("mentions {active_namespace} and ws_active"),
                        "workspace": {
                            "title": "ws_active"
                        }
                    }
                }
            }),
        );
        assert!(!client.legacy_server_key_row_belongs_to_session_namespace(&spoofed_container_row));

        let conflicting_namespace_row = test_session_row_with_extra(
            "oc-server-conflicting-namespace",
            serde_json::json!({
                "metadata": {
                    "zterm": {
                        "sessionNamespace": foreign_namespace,
                        "workspaceId": "ws_active"
                    }
                }
            }),
        );
        assert!(
            !client.legacy_server_key_row_belongs_to_session_namespace(&conflicting_namespace_row)
        );
    }

    #[test]
    fn legacy_metadata_matching_accepts_exact_name_and_url_without_workspace_id() {
        let mut client = tests_support_new_fake(PendingRequests::new(), None, true);
        client.set_session_namespace("backend=openclaw;workspace=alpha;url=ws://shared/");

        let row = test_session_row_with_extra(
            "oc-server-name-url",
            serde_json::json!({
                "metadata": {
                    "zterm": {
                        "workspaceName": "alpha",
                        "workspaceUrl": "ws://shared"
                    }
                }
            }),
        );

        assert!(client.legacy_server_key_row_belongs_to_session_namespace(&row));
    }

    fn test_session_row_with_extra(
        key: &str,
        extra: serde_json::Value,
    ) -> super::super::handshake::OpenClawSessionRow {
        super::super::handshake::OpenClawSessionRow {
            key: key.to_string(),
            kind: "direct".to_string(),
            label: Some("Research".to_string()),
            display_name: None,
            derived_title: None,
            updated_at: None,
            session_id: None,
            extra: match extra {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            },
        }
    }

    fn assistant_event(
        session_key: &str,
        run_id: &str,
        text: &str,
    ) -> super::super::wire::EventFrame {
        super::super::wire::EventFrame {
            event: "session.message".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "runId": run_id,
                "message": {
                    "role": "assistant",
                    "runId": run_id,
                    "content": [{ "type": "text", "text": text }]
                }
            })),
            seq: None,
            state_version: None,
        }
    }

    fn assistant_delta_event(
        session_key: &str,
        run_id: &str,
        message_id: &str,
        text: &str,
    ) -> super::super::wire::EventFrame {
        super::super::wire::EventFrame {
            event: "session.message".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "runId": run_id,
                "messageId": message_id,
                "state": "delta",
                "message": {
                    "id": message_id,
                    "role": "assistant",
                    "runId": run_id,
                    "content": [{ "type": "text", "text": text }]
                }
            })),
            seq: None,
            state_version: None,
        }
    }

    fn assistant_event_without_run_id(
        session_key: &str,
        text: &str,
    ) -> super::super::wire::EventFrame {
        super::super::wire::EventFrame {
            event: "session.message".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": text }]
                }
            })),
            seq: None,
            state_version: None,
        }
    }

    fn assistant_event_without_run_id_with_message_seq(
        session_key: &str,
        message_seq: u64,
        text: &str,
    ) -> super::super::wire::EventFrame {
        super::super::wire::EventFrame {
            event: "session.message".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "messageSeq": message_seq,
                "message": {
                    "role": "assistant",
                    "messageSeq": message_seq,
                    "content": [{ "type": "text", "text": text }]
                }
            })),
            seq: None,
            state_version: None,
        }
    }

    fn assistant_event_without_run_id_with_message_id(
        session_key: &str,
        message_id: &str,
        text: &str,
    ) -> super::super::wire::EventFrame {
        super::super::wire::EventFrame {
            event: "session.message".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "messageId": message_id,
                "message": {
                    "id": message_id,
                    "role": "assistant",
                    "content": [{ "type": "text", "text": text }]
                }
            })),
            seq: None,
            state_version: None,
        }
    }

    fn assistant_tool_event(session_key: &str, run_id: &str) -> super::super::wire::EventFrame {
        super::super::wire::EventFrame {
            event: "session.message".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "runId": run_id,
                "message": {
                    "role": "assistant",
                    "runId": run_id,
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "shell",
                            "input": { "cmd": "true" }
                        },
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool-1",
                            "content": "ok"
                        }
                    ]
                }
            })),
            seq: None,
            state_version: None,
        }
    }

    fn assistant_many_tools_event(
        session_key: &str,
        run_id: &str,
        count: usize,
    ) -> super::super::wire::EventFrame {
        let content = (0..count)
            .map(|idx| {
                serde_json::json!({
                    "type": "tool_use",
                    "name": "shell",
                    "input": { "idx": idx }
                })
            })
            .collect::<Vec<_>>();
        super::super::wire::EventFrame {
            event: "session.message".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "runId": run_id,
                "message": {
                    "role": "assistant",
                    "runId": run_id,
                    "content": content
                }
            })),
            seq: None,
            state_version: None,
        }
    }

    fn run_completed_event(session_key: &str, run_id: &str) -> super::super::wire::EventFrame {
        super::super::wire::EventFrame {
            event: "session.run.completed".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "runId": run_id,
                "status": "completed"
            })),
            seq: None,
            state_version: None,
        }
    }

    fn run_completed_event_with_message_id(
        session_key: &str,
        run_id: &str,
        message_id: &str,
    ) -> super::super::wire::EventFrame {
        super::super::wire::EventFrame {
            event: "session.run.completed".to_string(),
            payload: Some(serde_json::json!({
                "sessionKey": session_key,
                "runId": run_id,
                "messageId": message_id,
                "status": "completed"
            })),
            seq: None,
            state_version: None,
        }
    }

    #[tokio::test]
    async fn collect_turn_result_accumulates_text_deltas_until_completion() {
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(4);
        event_tx
            .send(assistant_delta_event(
                "session-a",
                "current-run",
                "message-1",
                "hello ",
            ))
            .await
            .unwrap();
        event_tx
            .send(assistant_delta_event(
                "session-a",
                "current-run",
                "message-1",
                "world",
            ))
            .await
            .unwrap();
        event_tx
            .send(run_completed_event("session-a", "current-run"))
            .await
            .unwrap();

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            None,
            Duration::from_millis(50),
            &mut turn,
        )
        .await
        .expect("text deltas followed by completion should collect");

        assert_eq!(turn.run_id.as_deref(), Some("current-run"));
        assert_eq!(turn.text, "hello world");
    }

    #[tokio::test]
    async fn collect_turn_result_accepts_run_id_less_text_with_ack_message_seq() {
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(4);
        event_tx
            .send(assistant_event_without_run_id_with_message_seq(
                "session-a",
                42,
                "compat text",
            ))
            .await
            .unwrap();
        event_tx
            .send(run_completed_event("session-a", "current-run"))
            .await
            .unwrap();

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            Some(42),
            Duration::from_millis(50),
            &mut turn,
        )
        .await
        .expect("runId-less text with expected ack messageSeq should collect");

        assert_eq!(turn.run_id.as_deref(), Some("current-run"));
        assert_eq!(turn.text, "compat text");
    }

    #[tokio::test]
    async fn collect_turn_result_accepts_buffered_run_id_less_text_when_completion_ties_message_id()
    {
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(4);
        event_tx
            .send(assistant_event_without_run_id_with_message_id(
                "session-a",
                "message-1",
                "compat text",
            ))
            .await
            .unwrap();
        event_tx
            .send(run_completed_event_with_message_id(
                "session-a",
                "current-run",
                "message-1",
            ))
            .await
            .unwrap();

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            None,
            Duration::from_millis(50),
            &mut turn,
        )
        .await
        .expect("completion with matching messageId should tie buffered runId-less text");

        assert_eq!(turn.run_id.as_deref(), Some("current-run"));
        assert_eq!(turn.text, "compat text");
    }

    #[tokio::test]
    async fn collect_turn_result_caps_unmatched_runless_message_buffer() {
        let (event_tx, mut event_rx) =
            mpsc::channel::<super::super::wire::EventFrame>(RUNLESS_BUFFER_MAX_MESSAGES + 1);
        for idx in 0..=RUNLESS_BUFFER_MAX_MESSAGES {
            event_tx
                .send(assistant_event_without_run_id_with_message_id(
                    "session-a",
                    &format!("stale-message-{idx}"),
                    "stale text",
                ))
                .await
                .unwrap();
        }

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        let err = collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            None,
            Duration::from_secs(60),
            &mut turn,
        )
        .await
        .expect_err("unmatched runId-less messages should be capped before timeout");

        assert!(err
            .to_string()
            .contains("buffered runId-less assistant messages exceeded cap"));
    }

    #[tokio::test]
    async fn collect_turn_result_caps_matching_run_text() {
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(1);
        let oversized = "x".repeat(ACCEPTED_TURN_MAX_BYTES + 1);
        event_tx
            .send(assistant_delta_event(
                "session-a",
                "current-run",
                "message-1",
                &oversized,
            ))
            .await
            .unwrap();

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        let err = collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            None,
            Duration::from_secs(60),
            &mut turn,
        )
        .await
        .expect_err("matching-run assistant text should be capped before accumulation");

        assert!(err
            .to_string()
            .contains("accepted assistant turn exceeded cap"));
        assert!(turn.text.is_empty());
    }

    #[tokio::test]
    async fn collect_turn_result_caps_matching_run_tool_items() {
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(1);
        event_tx
            .send(assistant_many_tools_event(
                "session-a",
                "current-run",
                ACCEPTED_TURN_MAX_TOOL_ITEMS + 1,
            ))
            .await
            .unwrap();

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        let err = collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            None,
            Duration::from_secs(60),
            &mut turn,
        )
        .await
        .expect_err("matching-run assistant tool items should be capped before accumulation");

        assert!(err
            .to_string()
            .contains("accepted assistant turn exceeded cap"));
        assert!(turn.tool_calls.is_empty());
        assert!(turn.tool_results.is_empty());
    }

    #[tokio::test]
    async fn collect_turn_result_rejects_stale_run_id_less_text_followed_by_expected_completion() {
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(4);
        event_tx
            .send(assistant_event_without_run_id("session-a", "stale text"))
            .await
            .unwrap();
        event_tx
            .send(run_completed_event("session-a", "current-run"))
            .await
            .unwrap();

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        let err = collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            None,
            Duration::from_millis(1),
            &mut turn,
        )
        .await
        .expect_err("unmatched runId-less text must fail closed");

        assert!(err.to_string().contains("timed out"));
        assert!(turn.text.is_empty());
    }

    #[tokio::test]
    async fn collect_turn_result_accepts_tool_only_completed_run() {
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(4);
        event_tx
            .send(assistant_tool_event("session-a", "current-run"))
            .await
            .unwrap();
        event_tx
            .send(run_completed_event("session-a", "current-run"))
            .await
            .unwrap();

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            None,
            Duration::from_millis(50),
            &mut turn,
        )
        .await
        .expect("tool-only completed run should not time out");

        assert_eq!(turn.run_id.as_deref(), Some("current-run"));
        assert!(turn.text.is_empty());
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_results.len(), 1);
    }

    #[tokio::test]
    async fn collect_turn_result_errors_on_tool_only_turn_without_completion_marker() {
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(4);
        event_tx
            .send(assistant_tool_event("session-a", "current-run"))
            .await
            .unwrap();

        let mut turn = super::super::handshake::TurnResult {
            run_id: Some("current-run".to_string()),
            ..Default::default()
        };
        let err = collect_turn_result(
            &mut event_rx,
            "session-a",
            "current-run",
            None,
            None,
            Duration::from_millis(1),
            &mut turn,
        )
        .await
        .expect_err("tool-only turn without completion marker should time out");

        assert!(err.to_string().contains("timed out"));
        assert!(turn.text.is_empty());
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_results.len(), 1);
    }

    #[tokio::test]
    async fn rich_send_collect_ignores_stale_same_session_run_events() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<super::super::wire::EventFrame>(EVENT_CAPACITY);
        let mut client = OpenClawClient {
            pending: pending.clone(),
            outbound_tx: Some(outbound_tx),
            event_rx: Some(event_rx),
            connected: Arc::new(AtomicBool::new(true)),
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: None,
            write_task: None,
        };

        let collect_task = tokio::spawn(async move {
            client
                .rpc_sessions_send_and_collect_rich(
                    "session-a",
                    "hello",
                    SessionsSendOpts::default(),
                    Duration::from_secs(1),
                )
                .await
        });

        let subscribe = outbound_rx
            .recv()
            .await
            .expect("subscribe request should be sent");
        assert_eq!(subscribe.method, "sessions.messages.subscribe");
        pending
            .resolve(ResponseFrame {
                id: subscribe.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let send = outbound_rx
            .recv()
            .await
            .expect("send request should be sent");
        assert_eq!(send.method, "sessions.send");
        event_tx
            .send(assistant_event("session-a", "old-run", "stale"))
            .await
            .expect("stale event should queue");
        pending
            .resolve(ResponseFrame {
                id: send.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "runId": "current-run",
                    "interruptedActiveRun": false
                })),
                error: None,
            })
            .await;
        event_tx
            .send(assistant_event("session-a", "current-run", "current"))
            .await
            .expect("current event should queue");
        event_tx
            .send(run_completed_event("session-a", "current-run"))
            .await
            .expect("completion event should queue");

        let unsubscribe = outbound_rx
            .recv()
            .await
            .expect("unsubscribe request should be sent");
        assert_eq!(unsubscribe.method, "sessions.messages.unsubscribe");
        pending
            .resolve(ResponseFrame {
                id: unsubscribe.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let turn = collect_task
            .await
            .expect("collect task should join")
            .expect("current run should collect");
        assert_eq!(turn.run_id.as_deref(), Some("current-run"));
        assert_eq!(turn.text, "current");
    }

    #[tokio::test]
    async fn rich_send_collect_unsubscribes_after_send_failure() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let (_event_tx, event_rx) = mpsc::channel::<super::super::wire::EventFrame>(EVENT_CAPACITY);
        let mut client = OpenClawClient {
            pending: pending.clone(),
            outbound_tx: Some(outbound_tx),
            event_rx: Some(event_rx),
            connected: Arc::new(AtomicBool::new(true)),
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: None,
            write_task: None,
        };

        let collect_task = tokio::spawn(async move {
            client
                .rpc_sessions_send_and_collect_rich(
                    "session-a",
                    "hello",
                    SessionsSendOpts::default(),
                    Duration::from_secs(1),
                )
                .await
        });

        let subscribe = outbound_rx
            .recv()
            .await
            .expect("subscribe request should be sent");
        pending
            .resolve(ResponseFrame {
                id: subscribe.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let send = outbound_rx
            .recv()
            .await
            .expect("send request should be sent");
        assert_eq!(send.method, "sessions.send");
        pending
            .resolve(ResponseFrame {
                id: send.id,
                ok: false,
                payload: None,
                error: Some(super::super::wire::ErrorBody {
                    code: "BOOM".to_string(),
                    message: "send failed".to_string(),
                    details: None,
                }),
            })
            .await;

        let unsubscribe = outbound_rx
            .recv()
            .await
            .expect("unsubscribe request should be sent after send failure");
        assert_eq!(unsubscribe.method, "sessions.messages.unsubscribe");
        pending
            .resolve(ResponseFrame {
                id: unsubscribe.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let err = collect_task
            .await
            .expect("collect task should join")
            .expect_err("send failure should propagate");
        assert!(err.to_string().contains("send failed"));
    }

    #[tokio::test]
    async fn rich_send_collect_aborts_acknowledged_run_after_collect_timeout() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let (_event_tx, event_rx) = mpsc::channel::<super::super::wire::EventFrame>(EVENT_CAPACITY);
        let mut client = OpenClawClient {
            pending: pending.clone(),
            outbound_tx: Some(outbound_tx),
            event_rx: Some(event_rx),
            connected: Arc::new(AtomicBool::new(true)),
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: None,
            write_task: None,
        };

        let collect_task = tokio::spawn(async move {
            client
                .rpc_sessions_send_and_collect_rich(
                    "session-a",
                    "hello",
                    SessionsSendOpts::default(),
                    Duration::from_millis(1),
                )
                .await
        });

        let subscribe = outbound_rx
            .recv()
            .await
            .expect("subscribe request should be sent");
        pending
            .resolve(ResponseFrame {
                id: subscribe.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let send = outbound_rx
            .recv()
            .await
            .expect("send request should be sent");
        assert_eq!(send.method, "sessions.send");
        pending
            .resolve(ResponseFrame {
                id: send.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "runId": "current-run",
                    "interruptedActiveRun": false
                })),
                error: None,
            })
            .await;

        let abort = outbound_rx
            .recv()
            .await
            .expect("abort request should be sent after collect timeout");
        assert_eq!(abort.method, "sessions.abort");
        assert_eq!(
            abort.params.as_ref().unwrap()["key"].as_str(),
            Some("session-a")
        );
        assert_eq!(
            abort.params.as_ref().unwrap()["runId"].as_str(),
            Some("current-run")
        );
        pending
            .resolve(ResponseFrame {
                id: abort.id,
                ok: true,
                payload: Some(serde_json::json!({ "aborted": 1 })),
                error: None,
            })
            .await;

        let unsubscribe = outbound_rx
            .recv()
            .await
            .expect("unsubscribe request should follow abort");
        assert_eq!(unsubscribe.method, "sessions.messages.unsubscribe");
        pending
            .resolve(ResponseFrame {
                id: unsubscribe.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let err = collect_task
            .await
            .expect("collect task should join")
            .expect_err("collect timeout should still return an error");
        let message = err.to_string();
        assert!(message.contains("timed out"));
        assert!(message.contains("abort confirmed"));
    }

    #[tokio::test]
    async fn rich_send_collect_unsubscribes_when_event_rx_unavailable() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let mut client = OpenClawClient {
            pending: pending.clone(),
            outbound_tx: Some(outbound_tx),
            event_rx: None,
            connected: Arc::new(AtomicBool::new(true)),
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: None,
            write_task: None,
        };

        let collect_task = tokio::spawn(async move {
            client
                .rpc_sessions_send_and_collect_rich(
                    "session-a",
                    "hello",
                    SessionsSendOpts::default(),
                    Duration::from_secs(1),
                )
                .await
        });

        let subscribe = outbound_rx
            .recv()
            .await
            .expect("subscribe request should be sent");
        pending
            .resolve(ResponseFrame {
                id: subscribe.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let send = outbound_rx
            .recv()
            .await
            .expect("send request should be sent");
        pending
            .resolve(ResponseFrame {
                id: send.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "runId": "current-run",
                    "interruptedActiveRun": false
                })),
                error: None,
            })
            .await;

        let abort = outbound_rx
            .recv()
            .await
            .expect("abort request should be sent after event_rx failure");
        assert_eq!(abort.method, "sessions.abort");
        assert_eq!(
            abort.params.as_ref().unwrap()["runId"].as_str(),
            Some("current-run")
        );
        pending
            .resolve(ResponseFrame {
                id: abort.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let unsubscribe = outbound_rx
            .recv()
            .await
            .expect("unsubscribe request should be sent after abort");
        assert_eq!(unsubscribe.method, "sessions.messages.unsubscribe");
        pending
            .resolve(ResponseFrame {
                id: unsubscribe.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;

        let err = collect_task
            .await
            .expect("collect task should join")
            .expect_err("event_rx failure should propagate");
        assert!(err.to_string().contains("event_rx already taken"));
        assert!(err.to_string().contains("abort confirmed"));
    }

    #[tokio::test]
    async fn create_session_sends_stable_key_with_label() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let expected_key = stable_session_key(DEFAULT_SESSION_NAMESPACE, "Research");

        let create_task = tokio::spawn(async move { client.create_session("Research").await });
        let req = outbound_rx
            .recv()
            .await
            .expect("create request should be sent");

        assert_eq!(req.method, "sessions.create");
        assert_eq!(
            req.params.as_ref().unwrap()["key"].as_str(),
            Some(expected_key.as_str())
        );
        assert_eq!(
            req.params.as_ref().unwrap()["label"].as_str(),
            Some("Research")
        );

        pending
            .resolve(ResponseFrame {
                id: req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "key": expected_key.clone(),
                    "label": "Research",
                    "entry": {}
                })),
                error: None,
            })
            .await;

        let session = create_task
            .await
            .expect("create task should join")
            .expect("create should succeed");
        assert_eq!(session.id, expected_key);
        assert_eq!(session.name, "Research");
    }

    #[tokio::test]
    async fn create_session_rejects_mismatched_created_key() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let expected_key = stable_session_key(DEFAULT_SESSION_NAMESPACE, "Research");

        let create_task = tokio::spawn(async move { client.create_session("Research").await });
        let req = outbound_rx
            .recv()
            .await
            .expect("create request should be sent");

        pending
            .resolve(ResponseFrame {
                id: req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "key": "foreign-session-key",
                    "label": "Research",
                    "entry": {}
                })),
                error: None,
            })
            .await;

        let err = create_task
            .await
            .expect("create task should join")
            .expect_err("mismatched create key must fail closed");
        assert!(err.to_string().contains("mismatched session key"));
        assert!(err.to_string().contains(&expected_key));
        assert!(err.to_string().contains("foreign-session-key"));
    }

    #[tokio::test]
    async fn create_session_rejects_legacy_alias_created_key() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let mut client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let primary_namespace = "backend=openclaw;workspace_id=ws_immutable";
        let legacy_namespace = "backend=openclaw;workspace=alpha;url=ws://old";
        client.set_session_namespace(primary_namespace);
        client.set_session_namespace_aliases([legacy_namespace]);
        let expected_key = stable_session_key(primary_namespace, "Research");
        let legacy_key = stable_session_key(legacy_namespace, "Research");

        let create_task = tokio::spawn(async move { client.create_session("Research").await });
        let req = outbound_rx
            .recv()
            .await
            .expect("create request should be sent");

        assert_eq!(
            req.params.as_ref().unwrap()["key"].as_str(),
            Some(expected_key.as_str())
        );

        pending
            .resolve(ResponseFrame {
                id: req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "key": legacy_key.clone(),
                    "label": "Research",
                    "entry": {}
                })),
                error: None,
            })
            .await;

        let err = create_task
            .await
            .expect("create task should join")
            .expect_err("legacy alias create key must fail closed");
        assert!(err.to_string().contains("mismatched session key"));
        assert!(err.to_string().contains(&expected_key));
        assert!(err.to_string().contains(&legacy_key));
    }

    #[tokio::test]
    async fn create_session_loads_stable_key_when_create_already_exists() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let expected_key = stable_session_key(DEFAULT_SESSION_NAMESPACE, "Research");

        let create_task = tokio::spawn(async move { client.create_session("Research").await });
        let create_req = outbound_rx
            .recv()
            .await
            .expect("create request should be sent");

        pending
            .resolve(ResponseFrame {
                id: create_req.id,
                ok: false,
                payload: None,
                error: Some(super::super::wire::ErrorBody {
                    code: "ALREADY_EXISTS".to_string(),
                    message: "session already exists".to_string(),
                    details: None,
                }),
            })
            .await;

        let list_req = outbound_rx
            .recv()
            .await
            .expect("load_session should list sessions after duplicate create");

        assert_eq!(list_req.method, "sessions.list");
        assert_eq!(list_req.params.as_ref().unwrap()["limit"], 500);

        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 1,
                    "defaults": {},
                    "sessions": [{
                        "key": expected_key.clone(),
                        "kind": "direct",
                        "label": "Research"
                    }]
                })),
                error: None,
            })
            .await;

        let session = create_task
            .await
            .expect("create task should join")
            .expect("duplicate create should load exact stable key");
        assert_eq!(session.id, expected_key);
        assert_eq!(session.name, "Research");
    }

    #[tokio::test]
    async fn duplicate_create_recovery_loads_only_namespaced_exact_key() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let mut client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        client.set_session_namespace("backend=openclaw;workspace=alpha;url=ws://shared");
        let expected_key = stable_session_key(
            "backend=openclaw;workspace=alpha;url=ws://shared",
            "Research",
        );
        let other_workspace_key = stable_session_key(
            "backend=openclaw;workspace=beta;url=ws://shared",
            "Research",
        );

        let create_task = tokio::spawn(async move { client.create_session("Research").await });
        let create_req = outbound_rx
            .recv()
            .await
            .expect("create request should be sent");

        pending
            .resolve(ResponseFrame {
                id: create_req.id,
                ok: false,
                payload: None,
                error: Some(super::super::wire::ErrorBody {
                    code: "ALREADY_EXISTS".to_string(),
                    message: "session already exists".to_string(),
                    details: None,
                }),
            })
            .await;

        let list_req = outbound_rx
            .recv()
            .await
            .expect("load_session should list sessions after duplicate create");

        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 2,
                    "defaults": {},
                    "sessions": [
                        {
                            "key": other_workspace_key,
                            "kind": "direct",
                            "label": "Research"
                        },
                        {
                            "key": expected_key.clone(),
                            "kind": "direct",
                            "label": "Research"
                        }
                    ]
                })),
                error: None,
            })
            .await;

        let session = create_task
            .await
            .expect("create task should join")
            .expect("duplicate create should load namespaced exact key");
        assert_eq!(session.id, expected_key);
        assert_eq!(session.name, "Research");
    }

    #[tokio::test]
    async fn duplicate_create_recovery_requires_exact_session_key() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let expected_key = stable_session_key(DEFAULT_SESSION_NAMESPACE, "Research");

        let create_task = tokio::spawn(async move { client.create_session("Research").await });
        let create_req = outbound_rx
            .recv()
            .await
            .expect("create request should be sent");

        pending
            .resolve(ResponseFrame {
                id: create_req.id,
                ok: false,
                payload: None,
                error: Some(super::super::wire::ErrorBody {
                    code: "ALREADY_EXISTS".to_string(),
                    message: "session already exists".to_string(),
                    details: None,
                }),
            })
            .await;

        let list_req = outbound_rx
            .recv()
            .await
            .expect("load_session should list sessions after duplicate create");

        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 1,
                    "defaults": {},
                    "sessions": [{
                        "key": "different-key",
                        "kind": "direct",
                        "label": "Research",
                        "sessionId": expected_key
                    }]
                })),
                error: None,
            })
            .await;

        let err = create_task
            .await
            .expect("create task should join")
            .expect_err("duplicate recovery must not load a compat sessionId match");
        assert!(err.to_string().contains("session not found"));
        assert!(err.to_string().contains(&expected_key));
    }

    #[tokio::test]
    async fn session_list_load_switch_and_delete_are_scoped_to_active_namespace() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let mut client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let active_namespace = "backend=openclaw;workspace=alpha;url=ws://shared";
        let foreign_namespace = "backend=openclaw;workspace=beta;url=ws://shared";
        client.set_session_namespace(active_namespace);
        let active_key = stable_session_key(active_namespace, "Research");
        let foreign_key = stable_session_key(foreign_namespace, "Research");
        let client = Arc::new(client);

        let list_client = Arc::clone(&client);
        let list_task = tokio::spawn(async move { list_client.list_sessions().await });
        let list_req = outbound_rx
            .recv()
            .await
            .expect("list request should be sent");
        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(two_workspace_research_sessions_payload(
                    &active_key,
                    &foreign_key,
                )),
                error: None,
            })
            .await;
        resolve_empty_legacy_session_scan(&pending, &mut outbound_rx).await;

        let sessions = list_task
            .await
            .expect("list task should join")
            .expect("list should succeed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, active_key);
        assert_eq!(sessions[0].name, "Research");
        let switch_name_matches: Vec<&Session> = sessions
            .iter()
            .filter(|session| session.name == "Research")
            .collect();
        assert_eq!(switch_name_matches.len(), 1);
        assert_eq!(switch_name_matches[0].id, active_key);

        let load_client = Arc::clone(&client);
        let foreign_key_for_load = foreign_key.clone();
        let load_task =
            tokio::spawn(async move { load_client.load_session(&foreign_key_for_load).await });
        let load_req = outbound_rx
            .recv()
            .await
            .expect("load request should be sent");
        pending
            .resolve(ResponseFrame {
                id: load_req.id,
                ok: true,
                payload: Some(two_workspace_research_sessions_payload(
                    &active_key,
                    &foreign_key,
                )),
                error: None,
            })
            .await;
        let err = load_task
            .await
            .expect("load task should join")
            .expect_err("foreign workspace key must not load");
        assert!(err.to_string().contains("session not found"));

        let delete_client = Arc::clone(&client);
        let foreign_key_for_delete = foreign_key.clone();
        let delete_task =
            tokio::spawn(
                async move { delete_client.delete_session(&foreign_key_for_delete).await },
            );
        let delete_list_req = outbound_rx
            .recv()
            .await
            .expect("delete should first validate with sessions.list");
        pending
            .resolve(ResponseFrame {
                id: delete_list_req.id,
                ok: true,
                payload: Some(two_workspace_research_sessions_payload(
                    &active_key,
                    &foreign_key,
                )),
                error: None,
            })
            .await;
        let err = delete_task
            .await
            .expect("delete task should join")
            .expect_err("foreign workspace key must not delete");
        assert!(err.to_string().contains("session not found"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), outbound_rx.recv())
                .await
                .is_err(),
            "foreign delete must not send sessions.delete after failed namespace validation"
        );
    }

    #[tokio::test]
    async fn session_list_includes_previous_namespace_aliases() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let mut client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let primary_namespace = "backend=openclaw;workspace_id=ws_immutable";
        let legacy_namespace = "backend=openclaw;workspace=alpha;url=ws://old";
        client.set_session_namespace(primary_namespace);
        client.set_session_namespace_aliases([legacy_namespace]);

        let legacy_key = stable_session_key(legacy_namespace, "Research");
        let list_task = tokio::spawn(async move { client.list_sessions().await });
        let list_req = outbound_rx
            .recv()
            .await
            .expect("list request should be sent");
        let primary_search = session_key_prefix(primary_namespace);
        assert_eq!(
            list_req.params.as_ref().unwrap()["search"].as_str(),
            Some(primary_search.as_str())
        );
        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 0,
                    "defaults": {},
                    "sessions": []
                })),
                error: None,
            })
            .await;

        let alias_req = outbound_rx
            .recv()
            .await
            .expect("alias list request should be sent");
        let alias_search = session_key_prefix(legacy_namespace);
        assert_eq!(
            alias_req.params.as_ref().unwrap()["search"].as_str(),
            Some(alias_search.as_str())
        );
        pending
            .resolve(ResponseFrame {
                id: alias_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 1,
                    "defaults": {},
                    "sessions": [{
                        "key": legacy_key,
                        "kind": "direct",
                        "label": "Research"
                    }]
                })),
                error: None,
            })
            .await;
        resolve_empty_legacy_session_scan(&pending, &mut outbound_rx).await;

        let sessions = list_task
            .await
            .expect("list task should join")
            .expect("alias list should succeed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, legacy_key);
        assert_eq!(sessions[0].name, "Research");
    }

    #[tokio::test]
    async fn list_sessions_filters_by_active_namespace_before_global_cap() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let mut client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let active_namespace = "backend=openclaw;workspace_id=ws_active";
        client.set_session_namespace(active_namespace);

        let active_one = stable_session_key(active_namespace, "Research");
        let active_two = stable_session_key(active_namespace, "Planning");
        let list_task = tokio::spawn(async move { client.list_sessions().await });
        let list_req = outbound_rx
            .recv()
            .await
            .expect("list request should be sent");
        assert_eq!(list_req.method, "sessions.list");
        assert_eq!(list_req.params.as_ref().unwrap()["limit"], 200);
        let active_search = session_key_prefix(active_namespace);
        assert_eq!(
            list_req.params.as_ref().unwrap()["search"].as_str(),
            Some(active_search.as_str())
        );

        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 2,
                    "defaults": {},
                    "sessions": [
                        { "key": active_one, "kind": "direct", "label": "Research" },
                        { "key": active_two, "kind": "direct", "label": "Planning" }
                    ]
                })),
                error: None,
            })
            .await;
        let legacy_req = outbound_rx
            .recv()
            .await
            .expect("legacy compatibility scan should be sent");
        assert_eq!(legacy_req.method, "sessions.list");
        assert_eq!(legacy_req.params.as_ref().unwrap()["limit"], 500);
        assert!(legacy_req.params.as_ref().unwrap().get("search").is_none());
        pending
            .resolve(ResponseFrame {
                id: legacy_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 600,
                    "defaults": {},
                    "sessions": []
                })),
                error: None,
            })
            .await;

        let sessions = list_task
            .await
            .expect("list task should join")
            .expect("active namespace list should not fail on truncated legacy global scan");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "Research");
        assert_eq!(sessions[1].name, "Planning");
    }

    #[tokio::test]
    async fn legacy_server_generated_session_keys_are_scoped_by_workspace_metadata() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let mut client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let active_namespace = "backend=openclaw;workspace_id=ws_active";
        let foreign_namespace = "backend=openclaw;workspace_id=ws_foreign";
        client.set_session_namespace(active_namespace);
        let server_key = "oc-server-generated-123";
        let foreign_server_key = "oc-server-generated-foreign";
        let client = Arc::new(client);

        let list_client = Arc::clone(&client);
        let list_task = tokio::spawn(async move { list_client.list_sessions().await });
        let stable_req = outbound_rx
            .recv()
            .await
            .expect("stable namespace scan should be sent first");
        assert_eq!(
            stable_req.params.as_ref().unwrap()["search"].as_str(),
            Some(session_key_prefix(active_namespace).as_str())
        );
        pending
            .resolve(ResponseFrame {
                id: stable_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 0,
                    "defaults": {},
                    "sessions": []
                })),
                error: None,
            })
            .await;

        let legacy_req = outbound_rx
            .recv()
            .await
            .expect("legacy server-key scan should be sent");
        assert_eq!(legacy_req.method, "sessions.list");
        assert_eq!(legacy_req.params.as_ref().unwrap()["limit"], 500);
        assert!(legacy_req.params.as_ref().unwrap().get("search").is_none());
        pending
            .resolve(ResponseFrame {
                id: legacy_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 2,
                    "defaults": {},
                    "sessions": [
                        {
                            "key": foreign_server_key,
                            "kind": "direct",
                            "label": "Research",
                            "metadata": {
                                "zterm": { "sessionNamespace": foreign_namespace }
                            }
                        },
                        {
                            "key": server_key,
                            "kind": "direct",
                            "label": "Research",
                            "metadata": {
                                "zterm": { "sessionNamespace": active_namespace }
                            }
                        }
                    ]
                })),
                error: None,
            })
            .await;

        let sessions = list_task
            .await
            .expect("list task should join")
            .expect("legacy server-key list should succeed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, server_key);
        assert_eq!(sessions[0].name, "Research");

        let load_client = Arc::clone(&client);
        let load_task = tokio::spawn(async move { load_client.load_session(server_key).await });
        let load_req = outbound_rx
            .recv()
            .await
            .expect("load should search by server-generated key");
        assert_eq!(
            load_req.params.as_ref().unwrap()["search"].as_str(),
            Some(server_key)
        );
        pending
            .resolve(ResponseFrame {
                id: load_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 1,
                    "defaults": {},
                    "sessions": [{
                        "key": server_key,
                        "kind": "direct",
                        "label": "Research",
                        "metadata": {
                            "zterm": { "sessionNamespace": active_namespace }
                        }
                    }]
                })),
                error: None,
            })
            .await;
        let loaded = load_task
            .await
            .expect("load task should join")
            .expect("metadata-gated server key should load");
        assert_eq!(loaded.id, server_key);

        let delete_client = Arc::clone(&client);
        let delete_task =
            tokio::spawn(async move { delete_client.delete_session(server_key).await });
        let delete_lookup_req = outbound_rx
            .recv()
            .await
            .expect("delete should validate by loading first");
        pending
            .resolve(ResponseFrame {
                id: delete_lookup_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 1,
                    "defaults": {},
                    "sessions": [{
                        "key": server_key,
                        "kind": "direct",
                        "label": "Research",
                        "metadata": {
                            "zterm": { "sessionNamespace": active_namespace }
                        }
                    }]
                })),
                error: None,
            })
            .await;
        let delete_req = outbound_rx
            .recv()
            .await
            .expect("validated server-key delete should be sent");
        assert_eq!(delete_req.method, "sessions.delete");
        assert_eq!(
            delete_req.params.as_ref().unwrap()["key"].as_str(),
            Some(server_key)
        );
        pending
            .resolve(ResponseFrame {
                id: delete_req.id,
                ok: true,
                payload: Some(serde_json::json!({})),
                error: None,
            })
            .await;
        delete_task
            .await
            .expect("delete task should join")
            .expect("metadata-gated server key should delete");
    }

    #[tokio::test]
    async fn legacy_server_keys_do_not_trust_titles_for_workspace_ownership() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let mut client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let active_namespace =
            "backend=openclaw;workspace_id=ws_active;workspace=alpha;url=ws://shared";
        let foreign_namespace =
            "backend=openclaw;workspace_id=ws_foreign;workspace=beta;url=ws://shared";
        client.set_session_namespace(active_namespace);
        let foreign_server_key = "oc-server-generated-foreign-title";
        let title = format!("Recovered for {active_namespace} ws_active alpha ws://shared");
        let client = Arc::new(client);

        let list_client = Arc::clone(&client);
        let list_task = tokio::spawn(async move { list_client.list_sessions().await });
        let stable_req = outbound_rx
            .recv()
            .await
            .expect("stable namespace scan should be sent first");
        pending
            .resolve(ResponseFrame {
                id: stable_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 0,
                    "defaults": {},
                    "sessions": []
                })),
                error: None,
            })
            .await;

        let legacy_req = outbound_rx
            .recv()
            .await
            .expect("legacy compatibility scan should be sent");
        pending
            .resolve(ResponseFrame {
                id: legacy_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 1,
                    "defaults": {},
                    "sessions": [{
                        "key": foreign_server_key,
                        "kind": "direct",
                        "label": "alpha",
                        "displayName": "alpha ws://shared",
                        "derivedTitle": title,
                        "metadata": {
                            "zterm": { "sessionNamespace": foreign_namespace }
                        }
                    }]
                })),
                error: None,
            })
            .await;
        let sessions = list_task
            .await
            .expect("list task should join")
            .expect("legacy title-spoof list should succeed");
        assert!(sessions.is_empty());

        let load_client = Arc::clone(&client);
        let load_task =
            tokio::spawn(async move { load_client.load_session(foreign_server_key).await });
        let load_req = outbound_rx
            .recv()
            .await
            .expect("load should search by server-generated key");
        pending
            .resolve(ResponseFrame {
                id: load_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 1,
                    "defaults": {},
                    "sessions": [{
                        "key": foreign_server_key,
                        "kind": "direct",
                        "label": "alpha",
                        "displayName": "alpha ws://shared",
                        "derivedTitle": title,
                        "metadata": {
                            "zterm": { "sessionNamespace": foreign_namespace }
                        }
                    }]
                })),
                error: None,
            })
            .await;
        let err = load_task
            .await
            .expect("load task should join")
            .expect_err("title-spoofed foreign legacy row must not load");
        assert!(err.to_string().contains("session not found"));

        let delete_client = Arc::clone(&client);
        let delete_task =
            tokio::spawn(async move { delete_client.delete_session(foreign_server_key).await });
        let delete_lookup_req = outbound_rx
            .recv()
            .await
            .expect("delete should validate by loading first");
        pending
            .resolve(ResponseFrame {
                id: delete_lookup_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 1,
                    "defaults": {},
                    "sessions": [{
                        "key": foreign_server_key,
                        "kind": "direct",
                        "label": "alpha",
                        "displayName": "alpha ws://shared",
                        "derivedTitle": title,
                        "metadata": {
                            "zterm": { "sessionNamespace": foreign_namespace }
                        }
                    }]
                })),
                error: None,
            })
            .await;
        let err = delete_task
            .await
            .expect("delete task should join")
            .expect_err("title-spoofed foreign legacy row must not delete");
        assert!(err.to_string().contains("session not found"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), outbound_rx.recv())
                .await
                .is_err(),
            "title-spoofed delete must not send sessions.delete"
        );
    }

    #[tokio::test]
    async fn list_sessions_fails_closed_when_sessions_list_hits_cap() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);

        let list_task = tokio::spawn(async move { client.list_sessions().await });
        let list_req = outbound_rx
            .recv()
            .await
            .expect("list request should be sent");
        assert_eq!(list_req.method, "sessions.list");
        assert_eq!(list_req.params.as_ref().unwrap()["limit"], 200);
        let default_search = session_key_prefix(DEFAULT_SESSION_NAMESPACE);
        assert_eq!(
            list_req.params.as_ref().unwrap()["search"].as_str(),
            Some(default_search.as_str())
        );

        let sessions: Vec<_> = (0..200)
            .map(|idx| {
                let label = format!("Session {idx}");
                serde_json::json!({
                    "key": stable_session_key(DEFAULT_SESSION_NAMESPACE, &label),
                    "kind": "direct",
                    "label": label
                })
            })
            .collect();
        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 200,
                    "defaults": {},
                    "sessions": sessions
                })),
                error: None,
            })
            .await;

        let err = list_task
            .await
            .expect("list task should join")
            .expect_err("capped list must fail closed");
        assert!(err.to_string().contains("truncated"));
        assert!(err.to_string().contains("partial session list"));
    }

    #[tokio::test]
    async fn targeted_load_fails_closed_when_sessions_list_is_truncated() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let target = stable_session_key(DEFAULT_SESSION_NAMESPACE, "Missing");
        let target_for_task = target.clone();

        let load_task = tokio::spawn(async move { client.load_session(&target_for_task).await });
        let list_req = outbound_rx
            .recv()
            .await
            .expect("load request should be sent");
        assert_eq!(list_req.method, "sessions.list");
        assert_eq!(list_req.params.as_ref().unwrap()["limit"], 500);
        assert_eq!(
            list_req.params.as_ref().unwrap()["search"].as_str(),
            Some(target.as_str())
        );

        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 501,
                    "defaults": {},
                    "sessions": []
                })),
                error: None,
            })
            .await;

        let err = load_task
            .await
            .expect("load task should join")
            .expect_err("truncated targeted lookup must fail closed");
        assert!(err.to_string().contains("truncated"));
        assert!(!err.to_string().contains("session not found"));
    }

    #[tokio::test]
    async fn targeted_delete_fails_closed_when_validation_list_is_truncated() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let target = stable_session_key(DEFAULT_SESSION_NAMESPACE, "Missing");
        let target_for_task = target.clone();

        let delete_task =
            tokio::spawn(async move { client.delete_session(&target_for_task).await });
        let list_req = outbound_rx
            .recv()
            .await
            .expect("delete validation list request should be sent");
        assert_eq!(list_req.method, "sessions.list");

        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 600,
                    "defaults": {},
                    "sessions": []
                })),
                error: None,
            })
            .await;

        let err = delete_task
            .await
            .expect("delete task should join")
            .expect_err("truncated delete validation must fail closed");
        assert!(err.to_string().contains("truncated"));
        if let Ok(Some(req)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), outbound_rx.recv()).await
        {
            panic!(
                "truncated delete validation must not send {}, id {}",
                req.method, req.id
            );
        }
    }

    #[tokio::test]
    async fn duplicate_create_recovery_fails_closed_when_exact_lookup_is_truncated() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);
        let expected_key = stable_session_key(DEFAULT_SESSION_NAMESPACE, "Research");

        let create_task = tokio::spawn(async move { client.create_session("Research").await });
        let create_req = outbound_rx
            .recv()
            .await
            .expect("create request should be sent");
        pending
            .resolve(ResponseFrame {
                id: create_req.id,
                ok: false,
                payload: None,
                error: Some(super::super::wire::ErrorBody {
                    code: "ALREADY_EXISTS".to_string(),
                    message: "session already exists".to_string(),
                    details: None,
                }),
            })
            .await;

        let list_req = outbound_rx
            .recv()
            .await
            .expect("duplicate create recovery should list sessions");
        assert_eq!(
            list_req.params.as_ref().unwrap()["search"].as_str(),
            Some(expected_key.as_str())
        );
        pending
            .resolve(ResponseFrame {
                id: list_req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 700,
                    "defaults": {},
                    "sessions": []
                })),
                error: None,
            })
            .await;

        let err = create_task
            .await
            .expect("create task should join")
            .expect_err("truncated duplicate recovery must fail closed");
        assert!(err.to_string().contains("truncated"));
        assert!(!err.to_string().contains("session not found"));
    }

    fn two_workspace_research_sessions_payload(
        active_key: &str,
        foreign_key: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "ts": 1,
            "path": "sessions.jsonl",
            "count": 2,
            "defaults": {},
            "sessions": [
                {
                    "key": foreign_key,
                    "kind": "direct",
                    "label": "Research"
                },
                {
                    "key": active_key,
                    "kind": "direct",
                    "label": "Research"
                }
            ]
        })
    }

    async fn resolve_empty_legacy_session_scan(
        pending: &PendingRequests,
        outbound_rx: &mut mpsc::Receiver<RequestFrame>,
    ) {
        let req = outbound_rx
            .recv()
            .await
            .expect("legacy compatibility scan should be sent");
        assert_eq!(req.method, "sessions.list");
        assert_eq!(req.params.as_ref().unwrap()["limit"], 500);
        assert_eq!(
            req.params.as_ref().unwrap()["includeDerivedTitles"],
            serde_json::json!(true)
        );
        assert!(req.params.as_ref().unwrap().get("search").is_none());
        pending
            .resolve(ResponseFrame {
                id: req.id,
                ok: true,
                payload: Some(serde_json::json!({
                    "ts": 1,
                    "path": "sessions.jsonl",
                    "count": 0,
                    "defaults": {},
                    "sessions": []
                })),
                error: None,
            })
            .await;
    }

    #[tokio::test]
    async fn read_loop_drops_saturated_events_without_starving_responses() {
        let pending = PendingRequests::new();
        let response_id = "req-response-after-event".to_string();
        let response_rx = pending.register(response_id.clone()).await;
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(1);
        event_tx
            .try_send(super::super::wire::EventFrame {
                event: "already.full".to_string(),
                payload: None,
                seq: None,
                state_version: None,
            })
            .expect("test event channel should start full");

        let unsolicited_event = Frame::Event(super::super::wire::EventFrame {
            event: "presence.update".to_string(),
            payload: Some(serde_json::json!({ "online": true })),
            seq: None,
            state_version: None,
        })
        .to_json()
        .unwrap();
        let response = Frame::Res(ResponseFrame {
            id: response_id.clone(),
            ok: true,
            payload: Some(serde_json::json!({ "done": true })),
            error: None,
        })
        .to_json()
        .unwrap();
        let stream = futures::stream::iter(vec![
            Ok(WsMessage::Text(unsolicited_event)),
            Ok(WsMessage::Text(response)),
        ]);
        let connected = Arc::new(AtomicBool::new(true));

        let read_task = tokio::spawn(read_loop(
            stream,
            pending.clone(),
            event_tx,
            Arc::clone(&connected),
        ));
        let got = tokio::time::timeout(Duration::from_millis(50), response_rx)
            .await
            .expect("response should not be blocked by saturated event channel")
            .expect("response oneshot should deliver");

        assert_eq!(got.id, response_id);
        assert_eq!(got.payload.unwrap()["done"], true);
        read_task.await.expect("read loop should join");
        assert!(!connected.load(Ordering::Relaxed));

        let retained = event_rx
            .try_recv()
            .expect("pre-filled event should remain in channel");
        assert_eq!(retained.event, "already.full");
        assert!(
            event_rx.try_recv().is_err(),
            "unsolicited event should have been dropped when channel was full"
        );
    }

    #[tokio::test]
    async fn read_loop_delivers_reliable_turn_event_after_best_effort_backlog() {
        let pending = PendingRequests::new();
        let response_id = "req-response-after-completion".to_string();
        let response_rx = pending.register(response_id.clone()).await;
        let (event_tx, mut event_rx) = mpsc::channel::<super::super::wire::EventFrame>(1);
        event_tx
            .try_send(super::super::wire::EventFrame {
                event: "already.full".to_string(),
                payload: None,
                seq: None,
                state_version: None,
            })
            .expect("test event channel should start full");

        let completion = Frame::Event(run_completed_event("session-a", "current-run"))
            .to_json()
            .unwrap();
        let response = Frame::Res(ResponseFrame {
            id: response_id.clone(),
            ok: true,
            payload: Some(serde_json::json!({ "done": true })),
            error: None,
        })
        .to_json()
        .unwrap();
        let stream = futures::stream::iter(vec![
            Ok(WsMessage::Text(completion)),
            Ok(WsMessage::Text(response)),
        ]);
        let connected = Arc::new(AtomicBool::new(true));

        let read_task = tokio::spawn(read_loop(
            stream,
            pending.clone(),
            event_tx,
            Arc::clone(&connected),
        ));
        let response = tokio::time::timeout(Duration::from_millis(50), response_rx)
            .await
            .expect("response should still be read while reliable event waits")
            .expect("response oneshot should deliver");
        assert_eq!(response.id, response_id);
        assert_eq!(response.payload.unwrap()["done"], true);

        let retained = event_rx
            .recv()
            .await
            .expect("pre-filled event should still be queued first");
        assert_eq!(retained.event, "already.full");
        let delivered = tokio::time::timeout(Duration::from_millis(50), event_rx.recv())
            .await
            .expect("reliable event should be delivered after backlog drains")
            .expect("reliable event channel should remain open");
        assert_eq!(delivered.event, "session.run.completed");
        read_task.await.expect("read loop should join");
        assert!(!connected.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn send_request_on_closed_connection_errors_cleanly() {
        // Manually build an OpenClawClient whose connected flag is
        // already false — exercises the early-return in send_request
        // without needing a live socket.
        let pending = PendingRequests::new();
        let (outbound_tx, _outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let (_event_tx, event_rx) = mpsc::channel::<super::super::wire::EventFrame>(EVENT_CAPACITY);

        let client = OpenClawClient {
            pending: pending.clone(),
            outbound_tx: Some(outbound_tx),
            event_rx: Some(event_rx),
            connected: Arc::new(AtomicBool::new(false)),
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: None,
            write_task: None,
        };

        assert!(!client.is_connected());
        let err = client
            .send_request("models.list", None)
            .await
            .expect_err("closed connection should error");
        assert!(err.to_string().contains("connection closed"));
    }

    #[tokio::test]
    async fn send_request_errors_when_write_loop_is_gone() {
        // connected=true, but the outbound receiver has already been
        // dropped → the send() in send_request fails.
        let pending = PendingRequests::new();
        let (outbound_tx, outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        drop(outbound_rx); // simulate write loop having exited

        let (_event_tx, event_rx) = mpsc::channel::<super::super::wire::EventFrame>(EVENT_CAPACITY);

        let client = OpenClawClient {
            pending: pending.clone(),
            outbound_tx: Some(outbound_tx),
            event_rx: Some(event_rx),
            connected: Arc::new(AtomicBool::new(true)),
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: None,
            write_task: None,
        };

        let err = client
            .send_request("models.list", None)
            .await
            .expect_err("dropped write loop should error");
        assert!(err.to_string().contains("write loop dropped"));
        assert!(
            pending.is_empty().await,
            "failed sends must cancel their pending request entry"
        );
    }

    #[tokio::test]
    async fn send_request_timeout_cancels_pending_request() {
        let pending = PendingRequests::new();
        let (outbound_tx, _outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);

        let err = client
            .send_request_with_timeout("models.list", None, Duration::from_millis(10))
            .await
            .expect_err("missing response should time out");

        assert!(err.to_string().contains("timed out"));
        assert!(
            pending.is_empty().await,
            "timed-out requests must not remain in the pending map"
        );
    }

    #[tokio::test]
    async fn send_request_timeout_covers_full_outbound_queue() {
        let pending = PendingRequests::new();
        let (outbound_tx, _outbound_rx) = mpsc::channel::<RequestFrame>(1);
        outbound_tx
            .try_send(RequestFrame::new("already.full", None))
            .expect("test queue should start full");
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);

        let err = client
            .send_request_with_timeout("models.list", None, Duration::from_millis(10))
            .await
            .expect_err("full outbound queue should time out");

        assert!(err.to_string().contains("timed out"));
        assert!(
            pending.is_empty().await,
            "enqueue timeouts must cancel their pending request entry"
        );
    }

    #[tokio::test]
    async fn dropped_send_request_cancels_pending_request() {
        let pending = PendingRequests::new();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let client = tests_support_new_fake(pending.clone(), Some(outbound_tx), true);

        let task = tokio::spawn(async move {
            client
                .send_request_with_timeout("models.list", None, Duration::from_secs(60))
                .await
        });
        let req = outbound_rx
            .recv()
            .await
            .expect("request should be sent before cancellation");
        assert_eq!(req.method, "models.list");
        assert_eq!(pending.len().await, 1);

        task.abort();
        let _ = task.await;

        assert!(
            pending.is_empty().await,
            "dropping an in-flight request future must clear its pending id"
        );
    }

    #[tokio::test]
    async fn take_event_rx_returns_some_then_none() {
        let pending = PendingRequests::new();
        let (outbound_tx, _outbound_rx) = mpsc::channel::<RequestFrame>(OUTBOUND_CAPACITY);
        let (_event_tx, event_rx) = mpsc::channel::<super::super::wire::EventFrame>(EVENT_CAPACITY);

        let mut client = OpenClawClient {
            pending,
            outbound_tx: Some(outbound_tx),
            event_rx: Some(event_rx),
            connected: Arc::new(AtomicBool::new(true)),
            hello_ok: None,
            stream_sink: None,
            session_namespace: DEFAULT_SESSION_NAMESPACE.to_string(),
            session_namespace_aliases: Vec::new(),
            read_task: None,
            write_task: None,
        };
        assert!(client.take_event_rx().is_some());
        assert!(client.take_event_rx().is_none());
    }
}
