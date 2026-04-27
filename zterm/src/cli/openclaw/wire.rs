//! OpenClaw wire protocol: JSON frame types + request/response correlation.
//!
//! Every frame on the openclaw gateway's WebSocket is a JSON object
//! tagged by a top-level `type` discriminator:
//!
//! - `type: "req"` — client → server request. Carries an `id` the
//!   client generates and a `method` name.
//! - `type: "res"` — server → client response. Echoes the `id` of the
//!   request it answers. `ok: true` with `payload`, or `ok: false`
//!   with `error`.
//! - `type: "event"` — server → client event (no request id; not
//!   correlated to any request). Carries an `event` name and optional
//!   `payload` / `seq` / `stateVersion`.
//!
//! See openclaw protocol v3 documentation:
//! <https://github.com/openclaw/openclaw/blob/main/docs/gateway/protocol.md>
//! § framing (lines 183–189) and the Zod schemas at
//! `src/gateway/protocol/schema/frames.ts` in that repo.
//!
//! ### Correlation
//!
//! Every in-flight client request needs to route its response back to
//! the caller. This module owns `PendingRequests` — a `HashMap<String,
//! oneshot::Sender<ResponseFrame>>` — so the single read-loop task can
//! fan each incoming `Res` frame to the awaiting caller task by id.
//! Unknown / late responses are dropped. Events bypass this map and
//! go to a separate broadcast channel managed by the client.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

/// A parsed openclaw wire frame.
///
/// Serde tags on `type` for both directions:
/// - `Frame::Req` ↔ `{"type":"req", ...}`
/// - `Frame::Res` ↔ `{"type":"res", ...}`
/// - `Frame::Event` ↔ `{"type":"event", ...}`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Frame {
    Req(RequestFrame),
    Res(ResponseFrame),
    Event(EventFrame),
}

impl Frame {
    /// Parse a JSON text frame received from the server.
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| anyhow!("openclaw wire: failed to decode frame: {}", e))
    }

    /// Serialize a frame to JSON text for transmission.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| anyhow!("openclaw wire: failed to encode frame: {}", e))
    }
}

/// Client → server request frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFrame {
    /// Client-generated unique id. The server echoes this back in the
    /// matching `ResponseFrame`. Use `new_request_id()` to mint one.
    pub id: String,

    /// Dotted RPC method name, e.g. `"connect"`, `"models.list"`,
    /// `"sessions.create"`, `"sessions.send"`.
    pub method: String,

    /// Method-specific parameters as free-form JSON. Methods that
    /// take no params may omit this field entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl RequestFrame {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            id: new_request_id(),
            method: method.into(),
            params,
        }
    }
}

/// Server → client response frame (correlated to a prior `RequestFrame`
/// by `id`). Exactly one of `payload` / `error` is populated depending
/// on the `ok` flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFrame {
    pub id: String,
    pub ok: bool,

    /// Present when `ok == true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,

    /// Present when `ok == false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// Structured error body returned on `ok: false` responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Short machine-readable code, e.g. `"INVALID_REQUEST"`,
    /// `"UNAUTHORIZED"`, `"PAIRING_REQUIRED"`.
    pub code: String,

    /// Human-readable message.
    pub message: String,

    /// Optional structured context (rate-limit details, field
    /// validation errors, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Server → client event frame (not correlated to any request).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFrame {
    /// Dotted event name, e.g. `"connect.challenge"`,
    /// `"session.message"`, `"presence.update"`.
    pub event: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,

    /// Monotonic server sequence number for this event stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,

    /// Optional per-event state-version vector (presence + health
    /// counters) — opaque to zterm; we surface it via events for
    /// clients that want to reconcile.
    #[serde(rename = "stateVersion", skip_serializing_if = "Option::is_none")]
    pub state_version: Option<serde_json::Value>,
}

/// Mint a fresh request id. UUID v4 as a lowercase hyphenated string.
pub fn new_request_id() -> String {
    Uuid::new_v4().to_string()
}

/// Shared in-flight-request tracker. The read loop calls `resolve`
/// with each incoming `ResponseFrame`; the caller task holds the
/// receiving end of the `oneshot` channel registered here.
#[derive(Debug, Default, Clone)]
pub struct PendingRequests {
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<ResponseFrame>>>>,
}

impl PendingRequests {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending request by id; returns the receiver the
    /// caller should await for the matching response.
    pub async fn register(&self, id: String) -> oneshot::Receiver<ResponseFrame> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().await.insert(id, tx);
        rx
    }

    /// Deliver a response to its waiting caller. Returns `true` if
    /// the id was in flight, `false` if no caller was waiting (stale
    /// id, timed-out caller, or duplicate response).
    pub async fn resolve(&self, frame: ResponseFrame) -> bool {
        let mut map = self.inner.lock().await;
        if let Some(tx) = map.remove(&frame.id) {
            // Receiver may already be dropped (caller gave up). That's
            // fine — we still report "delivered to the map" as `true`.
            let _ = tx.send(frame);
            true
        } else {
            false
        }
    }

    /// Abandon all in-flight requests. Called by the read loop when
    /// the underlying WebSocket closes unexpectedly — waiting tasks
    /// see their `oneshot::Receiver` return `RecvError` and bubble up
    /// a connection-lost error.
    pub async fn abort_all(&self) {
        self.inner.lock().await.clear();
    }

    /// Number of in-flight requests (for debug / metrics).
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- serialization ----------

    #[test]
    fn request_frame_serializes_with_type_req() {
        let req = RequestFrame {
            id: "req-1".to_string(),
            method: "models.list".to_string(),
            params: None,
        };
        let frame = Frame::Req(req);
        let s = frame.to_json().unwrap();
        assert!(s.contains(r#""type":"req""#));
        assert!(s.contains(r#""method":"models.list""#));
        assert!(s.contains(r#""id":"req-1""#));
        assert!(!s.contains(r#""params":"#));
    }

    #[test]
    fn request_frame_with_params_serializes_params() {
        let req = RequestFrame::new("sessions.create", Some(json!({"name": "main"})));
        let frame = Frame::Req(req);
        let s = frame.to_json().unwrap();
        assert!(s.contains(r#""name":"main""#));
    }

    #[test]
    fn response_frame_ok_deserializes() {
        let s = r#"{"type":"res","id":"req-1","ok":true,"payload":{"models":[]}}"#;
        let frame = Frame::from_json(s).unwrap();
        match frame {
            Frame::Res(res) => {
                assert_eq!(res.id, "req-1");
                assert!(res.ok);
                assert!(res.payload.is_some());
                assert!(res.error.is_none());
            }
            _ => panic!("expected Frame::Res"),
        }
    }

    #[test]
    fn response_frame_error_deserializes() {
        let s = r#"{
            "type": "res",
            "id": "req-2",
            "ok": false,
            "error": {
                "code": "UNAUTHORIZED",
                "message": "token expired"
            }
        }"#;
        let frame = Frame::from_json(s).unwrap();
        match frame {
            Frame::Res(res) => {
                assert!(!res.ok);
                let err = res.error.expect("error field");
                assert_eq!(err.code, "UNAUTHORIZED");
                assert_eq!(err.message, "token expired");
                assert!(err.details.is_none());
                assert!(res.payload.is_none());
            }
            _ => panic!("expected Frame::Res"),
        }
    }

    #[test]
    fn event_frame_deserializes() {
        let s = r#"{
            "type": "event",
            "event": "session.message",
            "payload": {"state": "delta", "message": {"content": "hi"}},
            "seq": 42,
            "stateVersion": {"presence": 1, "health": 2}
        }"#;
        let frame = Frame::from_json(s).unwrap();
        match frame {
            Frame::Event(ev) => {
                assert_eq!(ev.event, "session.message");
                assert_eq!(ev.seq, Some(42));
                assert!(ev.state_version.is_some());
            }
            _ => panic!("expected Frame::Event"),
        }
    }

    #[test]
    fn unknown_type_is_a_decode_error() {
        let s = r#"{"type":"banana","id":"x"}"#;
        assert!(Frame::from_json(s).is_err());
    }

    #[test]
    fn round_trip_preserves_request_fields() {
        let original = Frame::Req(RequestFrame {
            id: "id-x".to_string(),
            method: "chat.abort".to_string(),
            params: Some(json!({"sessionKey": "s-1"})),
        });
        let encoded = original.to_json().unwrap();
        let decoded = Frame::from_json(&encoded).unwrap();
        // Structural compare via re-encoding — serde_json::Value compare
        // is the simplest way without implementing PartialEq.
        let rer = decoded.to_json().unwrap();
        let a: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        let b: serde_json::Value = serde_json::from_str(&rer).unwrap();
        assert_eq!(a, b);
    }

    // ---------- request id ----------

    #[test]
    fn new_request_id_is_unique_and_v4_shaped() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36); // UUID hyphenated form
        assert_eq!(a.chars().filter(|&c| c == '-').count(), 4);
    }

    // ---------- pending requests ----------

    #[tokio::test]
    async fn register_then_resolve_delivers_frame_to_waiter() {
        let pending = PendingRequests::new();
        let id = "req-abc".to_string();
        let rx = pending.register(id.clone()).await;
        assert_eq!(pending.len().await, 1);

        let frame = ResponseFrame {
            id: id.clone(),
            ok: true,
            payload: Some(json!({"value": 42})),
            error: None,
        };
        let delivered = pending.resolve(frame).await;
        assert!(delivered);
        assert_eq!(pending.len().await, 0);

        let got = rx.await.expect("oneshot should deliver");
        assert_eq!(got.id, id);
        assert_eq!(got.payload.unwrap()["value"], 42);
    }

    #[tokio::test]
    async fn resolve_with_unknown_id_returns_false() {
        let pending = PendingRequests::new();
        let frame = ResponseFrame {
            id: "nobody-waiting".to_string(),
            ok: true,
            payload: None,
            error: None,
        };
        assert!(!pending.resolve(frame).await);
    }

    #[tokio::test]
    async fn abort_all_clears_registered_requests() {
        let pending = PendingRequests::new();
        let _rx1 = pending.register("a".to_string()).await;
        let _rx2 = pending.register("b".to_string()).await;
        assert_eq!(pending.len().await, 2);

        pending.abort_all().await;
        assert!(pending.is_empty().await);
    }

    #[tokio::test]
    async fn dropped_receiver_is_silently_handled() {
        let pending = PendingRequests::new();
        let rx = pending.register("x".to_string()).await;
        drop(rx); // Caller gave up waiting.

        let frame = ResponseFrame {
            id: "x".to_string(),
            ok: true,
            payload: None,
            error: None,
        };
        // resolve() still returns true (delivered to the map) even
        // though the oneshot send silently fails — that's the correct
        // contract. Resolve != caller saw it.
        assert!(pending.resolve(frame).await);
    }
}
