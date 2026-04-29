//! OpenClaw handshake canonical-payload construction (v2 / v3).
//!
//! The gateway verifies a client's `connect` request by signing
//! a canonical pipe-delimited string and ed25519-verifying the
//! signature against the public key in the request. This module
//! builds those canonical bytes **byte-for-byte identical** to the
//! openclaw reference implementation in
//! [`src/gateway/device-auth.ts`](https://github.com/openclaw/openclaw/blob/main/src/gateway/device-auth.ts).
//!
//! Any drift here is a silent authentication failure (the server
//! will reject the signature with no actionable reason in the
//! response), so the unit tests in this module intentionally port
//! openclaw's own `device-auth.test.ts` test vectors as ground truth.
//!
//! **Format reference:**
//!
//! ```text
//! v2: v2|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce
//! v3: v3|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce|platform|deviceFamily
//! ```
//!
//! Where:
//! - `scopes`   — comma-joined scope names in the order the client declared them
//! - `token` — the first non-empty of (auth.token, auth.deviceToken,
//!   auth.bootstrapToken); empty string if none
//! - `platform` / `deviceFamily` — trimmed + ASCII-only lowercased; empty if missing

/// Normalize a metadata field (platform / deviceFamily) the same way
/// openclaw's `normalizeDeviceMetadataForAuth` does:
///
/// 1. None / empty → empty string
/// 2. Trim leading/trailing ASCII whitespace
/// 3. Lowercase ASCII only (non-ASCII characters are preserved verbatim
///    — e.g. Turkish dotted-I `İ` stays `İ`, not `i`).
///
/// Matches `src/gateway/device-metadata-normalization.ts`.
pub fn normalize_metadata_for_auth(value: Option<&str>) -> String {
    match value {
        None => String::new(),
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                String::new()
            } else {
                trimmed.to_ascii_lowercase()
            }
        }
    }
}

/// Build the canonical v2 handshake-signing payload. v2 does NOT
/// include `platform` or `deviceFamily` fields.
///
/// `token` can be `None` for unauth'd connects — the field is emitted
/// as an empty string, preserving the pipe slot.
#[allow(clippy::too_many_arguments)]
pub fn build_v2_payload(
    device_id: &str,
    client_id: &str,
    client_mode: &str,
    role: &str,
    scopes: &[String],
    signed_at_ms: i64,
    token: Option<&str>,
    nonce: &str,
) -> String {
    let scopes_joined = scopes.join(",");
    let token_str = token.unwrap_or("");
    format!(
        "v2|{}|{}|{}|{}|{}|{}|{}|{}",
        device_id, client_id, client_mode, role, scopes_joined, signed_at_ms, token_str, nonce,
    )
}

/// Build the canonical v3 handshake-signing payload. v3 adds
/// `platform` and `deviceFamily` (both normalized) to the end.
///
/// zterm always sends v3 — the gateway auto-detects v3 vs. v2 at
/// verification time (see `resolveDeviceSignaturePayloadVersion`
/// in `handshake-auth-helpers.ts`), so there is no downside to always
/// emitting the richer format.
#[allow(clippy::too_many_arguments)]
pub fn build_v3_payload(
    device_id: &str,
    client_id: &str,
    client_mode: &str,
    role: &str,
    scopes: &[String],
    signed_at_ms: i64,
    token: Option<&str>,
    nonce: &str,
    platform: Option<&str>,
    device_family: Option<&str>,
) -> String {
    let scopes_joined = scopes.join(",");
    let token_str = token.unwrap_or("");
    let platform = normalize_metadata_for_auth(platform);
    let device_family = normalize_metadata_for_auth(device_family);
    format!(
        "v3|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        device_id,
        client_id,
        client_mode,
        role,
        scopes_joined,
        signed_at_ms,
        token_str,
        nonce,
        platform,
        device_family,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================================================================
    // Test vectors ported DIRECTLY from openclaw's own tests in
    // src/gateway/device-auth.test.ts. If any of these assertions ever
    // fails, the Rust implementation has drifted from the server's
    // expected canonical format — server-side signature verification
    // will fail silently in production. DO NOT weaken these tests.
    // ==================================================================

    #[test]
    fn openclaw_vector_v2_canonical() {
        let got = build_v2_payload(
            "dev-1",
            "openclaw-macos",
            "ui",
            "operator",
            &["operator.admin".to_string(), "operator.read".to_string()],
            1_700_000_000_000,
            None,
            "nonce-abc",
        );
        let expected =
            "v2|dev-1|openclaw-macos|ui|operator|operator.admin,operator.read|1700000000000||nonce-abc";
        assert_eq!(got, expected);
    }

    #[test]
    fn openclaw_vector_v3_canonical_with_whitespaced_metadata() {
        let got = build_v3_payload(
            "dev-1",
            "openclaw-macos",
            "ui",
            "operator",
            &["operator.admin".to_string(), "operator.read".to_string()],
            1_700_000_000_000,
            Some("tok-123"),
            "nonce-abc",
            Some("  IOS  "),
            Some("  iPhone  "),
        );
        let expected =
            "v3|dev-1|openclaw-macos|ui|operator|operator.admin,operator.read|1700000000000|tok-123|nonce-abc|ios|iphone";
        assert_eq!(got, expected);
    }

    #[test]
    fn openclaw_vector_v3_empty_metadata_preserves_trailing_pipes() {
        let got = build_v3_payload(
            "dev-2",
            "openclaw-ios",
            "ui",
            "operator",
            &["operator.read".to_string()],
            1_700_000_000_001,
            None,
            "nonce-def",
            None,
            None,
        );
        let expected = "v3|dev-2|openclaw-ios|ui|operator|operator.read|1700000000001||nonce-def||";
        assert_eq!(got, expected);
    }

    #[test]
    fn openclaw_vector_normalize_preserves_non_ascii_capital_i() {
        // Turkish-style dotted capital I survives normalization —
        // only ASCII A-Z gets lowercased. Keeping this behavior in
        // lockstep with the server is what prevents cross-runtime
        // (TS/Swift/Kotlin/Rust) signature-byte drift on Unicode inputs.
        assert_eq!(normalize_metadata_for_auth(Some("  İOS  ")), "İos");
    }

    #[test]
    fn openclaw_vector_normalize_ascii_lowercases_and_trims() {
        assert_eq!(normalize_metadata_for_auth(Some("  MAC  ")), "mac");
    }

    #[test]
    fn openclaw_vector_normalize_missing_is_empty_string() {
        assert_eq!(normalize_metadata_for_auth(None), "");
        assert_eq!(normalize_metadata_for_auth(Some("")), "");
        assert_eq!(normalize_metadata_for_auth(Some("   ")), "");
    }

    // ==================================================================
    // Additional Rust-side coverage not in openclaw's suite.
    // ==================================================================

    #[test]
    fn scopes_serialize_as_comma_join_preserving_order() {
        let got = build_v2_payload(
            "d",
            "c",
            "cli",
            "operator",
            &[
                "scope.c".to_string(),
                "scope.a".to_string(),
                "scope.b".to_string(),
            ],
            0,
            None,
            "n",
        );
        assert!(got.contains("|scope.c,scope.a,scope.b|"));
    }

    #[test]
    fn empty_scopes_emits_empty_slot() {
        let got = build_v2_payload("d", "c", "cli", "operator", &[], 0, None, "n");
        assert_eq!(got, "v2|d|c|cli|operator||0||n");
    }

    #[test]
    fn v3_has_one_more_pipe_than_v2_for_same_inputs() {
        let v2 = build_v2_payload("d", "c", "cli", "operator", &[], 1, Some("t"), "nonce");
        let v3 = build_v3_payload(
            "d",
            "c",
            "cli",
            "operator",
            &[],
            1,
            Some("t"),
            "nonce",
            None,
            None,
        );
        // v3 adds two metadata slots (platform, deviceFamily) separated
        // by pipes → 2 extra pipes (one per slot edge).
        let v2_pipes = v2.matches('|').count();
        let v3_pipes = v3.matches('|').count();
        assert_eq!(v3_pipes - v2_pipes, 2);
    }
}

// =====================================================================
// Async handshake flow (slice 3b-ii)
//
// Orchestrates the challenge-response handshake against a connected
// OpenClawClient:
//
//   1. Pull the first `connect.challenge` event off the event_rx
//      (server pushes it unprompted on WebSocket upgrade).
//   2. Build the canonical v3 signing payload from the challenge
//      nonce + caller-supplied ClientIdentity + scopes/role + token.
//   3. Sign with DeviceIdentity (slice 1).
//   4. Send a `connect` request with all of the above packaged per
//      openclaw protocol v3.
//   5. Parse the response payload as `HelloOk` and return it.
//
// The caller keeps the event_rx for session-level events after
// handshake — perform_handshake takes &mut Receiver so only the
// one challenge event is consumed.
// =====================================================================

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::client::OpenClawClient;
use super::device::DeviceIdentity;
use super::wire::EventFrame;

/// Identity the client advertises to the gateway in its `connect`
/// request. Shape matches openclaw's `ConnectParams.client` object
/// (see openclaw src/gateway/protocol/schema/frames.ts).
#[derive(Debug, Clone, Serialize)]
pub struct ClientIdentity {
    /// Stable client-product identifier. MUST be one of openclaw's
    /// `GATEWAY_CLIENT_IDS` enum values — free-form strings are
    /// rejected by the gateway's ConnectParamsSchema. For zterm the
    /// correct value is `"cli"` (matches `paired.json` records for
    /// every paired CLI device — see openclaw
    /// `src/gateway/protocol/client-info.ts::GATEWAY_CLIENT_IDS`).
    /// Use `display_name` to carry the zterm-specific branding.
    pub id: String,
    /// Optional human-facing product name. Openclaw surfaces this in
    /// device records and audit logs alongside the enum `id`. zterm
    /// sets this to `"zterm"` so pairing UIs can show something
    /// specific to the client rather than the generic `"cli"` tag.
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Client semver, e.g. `"0.1.0"`.
    pub version: String,
    /// Connection mode on the gateway. Must be one of openclaw's
    /// `GATEWAY_CLIENT_MODES` — `"cli"` for zterm.
    pub mode: String,
    /// OS family string used by the metadata-pinning audit log.
    /// Examples: `"linux"`, `"darwin"`, `"win32"`. Use the same
    /// value Node's `process.platform` would emit for parity with
    /// openclaw's own CLI.
    pub platform: String,
    /// Optional device-family label (e.g. `"generic"`, `"iPhone"`).
    /// None keeps the field off the wire; the canonical v3 payload
    /// emits an empty slot.
    pub device_family: Option<String>,
}

/// What the caller provides to `perform_handshake`. Role/scopes
/// map one-to-one onto openclaw's gateway authorization model; the
/// claimed scopes must be a subset of what the gateway is willing
/// to grant this device class (see paired.json for live examples).
#[derive(Debug, Clone)]
pub struct HandshakeParams {
    pub client: ClientIdentity,
    pub role: String,
    pub scopes: Vec<String>,
    /// Shared secret to include in the signed payload. For zterm's
    /// first bootstrap against a gateway that has `auth.mode: "none"`,
    /// this is typically `None`. If the gateway runs in `token` mode,
    /// pass the configured token here — it goes into the canonical
    /// signing string AND is echoed in the connect params as
    /// `auth.token`.
    pub token: Option<String>,
}

/// Server response to a `connect` request, matching
/// openclaw `HelloOkSchema` in
/// src/gateway/protocol/schema/frames.ts.
#[derive(Debug, Clone, Deserialize)]
pub struct HelloOk {
    #[serde(rename = "type")]
    pub kind: String, // always "hello-ok"
    pub protocol: u32,
    pub server: HelloOkServer,
    pub features: HelloOkFeatures,
    #[serde(default)]
    pub snapshot: serde_json::Value,
    #[serde(rename = "canvasHostUrl", skip_serializing_if = "Option::is_none")]
    pub canvas_host_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<HelloOkAuth>,
    pub policy: HelloOkPolicy,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloOkServer {
    pub version: String,
    #[serde(rename = "connId")]
    pub conn_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloOkFeatures {
    pub methods: Vec<String>,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloOkAuth {
    #[serde(rename = "deviceToken", skip_serializing_if = "Option::is_none")]
    pub device_token: Option<String>,
    pub role: String,
    pub scopes: Vec<String>,
    #[serde(rename = "issuedAtMs", skip_serializing_if = "Option::is_none")]
    pub issued_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloOkPolicy {
    #[serde(rename = "maxPayload")]
    pub max_payload: u64,
    #[serde(rename = "maxBufferedBytes")]
    pub max_buffered_bytes: u64,
    #[serde(rename = "tickIntervalMs")]
    pub tick_interval_ms: u64,
}

/// Build the JSON params for a `connect` request — canonical payload
/// signing included. Split out as a pure function so the
/// request-building side of the handshake can be exercised in unit
/// tests without a live WebSocket.
pub fn build_connect_params(
    device: &DeviceIdentity,
    params: &HandshakeParams,
    nonce: &str,
    signed_at_ms: i64,
) -> Result<serde_json::Value> {
    let canonical = build_v3_payload(
        device.device_id(),
        &params.client.id,
        &params.client.mode,
        &params.role,
        &params.scopes,
        signed_at_ms,
        params.token.as_deref(),
        nonce,
        Some(&params.client.platform),
        params.client.device_family.as_deref(),
    );
    let signature = device.sign_b64url(canonical.as_bytes());

    let mut out = serde_json::json!({
        "minProtocol": 3,
        "maxProtocol": 3,
        "client": {
            "id": params.client.id,
            "version": params.client.version,
            "mode": params.client.mode,
            "platform": params.client.platform,
        },
        "role": params.role,
        "scopes": params.scopes,
        "device": {
            "id": device.device_id(),
            "publicKey": device.public_key_b64url(),
            "signature": signature,
            "signedAt": signed_at_ms,
            "nonce": nonce,
        },
    });

    if let Some(name) = &params.client.display_name {
        out["client"]["displayName"] = serde_json::Value::String(name.clone());
    }
    if let Some(family) = &params.client.device_family {
        out["client"]["deviceFamily"] = serde_json::Value::String(family.clone());
    }
    if let Some(tok) = &params.token {
        out["auth"] = serde_json::json!({ "token": tok });
    }

    Ok(out)
}

/// Run the full handshake flow against a connected `OpenClawClient`.
/// Consumes one event (the `connect.challenge`) off the receiver.
///
/// Returns the parsed `HelloOk` on success; the caller retains the
/// event receiver for post-handshake session events.
pub async fn perform_handshake(
    client: &OpenClawClient,
    event_rx: &mut mpsc::Receiver<EventFrame>,
    device: &DeviceIdentity,
    params: &HandshakeParams,
) -> Result<HelloOk> {
    // Step 1: wait for the `connect.challenge` event.
    let challenge = loop {
        let ev = event_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("openclaw: event channel closed before challenge arrived"))?;
        if ev.event == "connect.challenge" {
            break ev;
        }
        // Any other pre-handshake event is unexpected but not fatal;
        // log and keep waiting.
        tracing::debug!("openclaw: pre-handshake event {} (ignored)", ev.event);
    };

    let payload = challenge
        .payload
        .ok_or_else(|| anyhow!("openclaw: connect.challenge has no payload"))?;
    let nonce = payload
        .get("nonce")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("openclaw: connect.challenge payload missing `nonce`"))?
        .to_string();

    // Step 2–3: build signed connect params.
    let signed_at_ms = chrono::Utc::now().timestamp_millis();
    let connect_params = build_connect_params(device, params, &nonce, signed_at_ms)?;

    // Step 4: send connect request, await response.
    let res = client
        .send_request("connect", Some(connect_params))
        .await
        .context("openclaw: connect request failed")?;

    if !res.ok {
        let err = res.error.unwrap_or_else(|| super::wire::ErrorBody {
            code: "UNKNOWN".to_string(),
            message: "connect failed with no error body".to_string(),
            details: None,
        });
        return Err(anyhow!(
            "openclaw: connect rejected: {} ({})",
            err.message,
            err.code
        ));
    }

    // Step 5: parse hello-ok payload.
    let payload = res
        .payload
        .ok_or_else(|| anyhow!("openclaw: connect response had no payload"))?;
    let hello_ok: HelloOk = serde_json::from_value(payload)
        .context("openclaw: connect response payload did not match hello-ok schema")?;

    if hello_ok.kind != "hello-ok" {
        return Err(anyhow!(
            "openclaw: connect response was type={:?}, expected \"hello-ok\"",
            hello_ok.kind
        ));
    }
    Ok(hello_ok)
}

/// Mirrors openclaw's ModelChoiceSchema (see
/// src/gateway/protocol/schema/agents-models-skills.ts). alias,
/// contextWindow, and reasoning are optional on the wire — we
/// preserve that here.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(
        default,
        rename = "contextWindow",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
}

/// One row from an openclaw `sessions.list` response.
///
/// Matches the subset of `GatewaySessionRow` that zterm actually
/// consumes. Openclaw sends many more optional fields (spawn
/// lineage, thinking levels, subagent scopes, abort flags, ...) —
/// we intentionally ignore them here; `serde(default)` keeps
/// forward-compat when openclaw adds fields.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenClawSessionRow {
    pub key: String,

    /// One of "direct" | "group" | "global" | "unknown".
    pub kind: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    #[serde(
        default,
        rename = "displayName",
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<String>,

    #[serde(
        default,
        rename = "derivedTitle",
        skip_serializing_if = "Option::is_none"
    )]
    pub derived_title: Option<String>,

    #[serde(default, rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,

    #[serde(default, rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Outer wrapper of a `sessions.list` response. Matches
/// `SessionsListResultBase` in openclaw
/// `src/shared/session-types.ts`.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawSessionsListResult {
    pub ts: i64,
    pub path: String,
    pub count: u32,
    #[serde(default)]
    pub defaults: serde_json::Value,
    pub sessions: Vec<OpenClawSessionRow>,
}

/// Response payload for `sessions.create`. Openclaw returns the
/// created session's canonical `key` plus the full entry; zterm
/// keeps the key for downstream RPCs and exposes label / title to
/// the caller for display.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawSessionCreateResult {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The full entry payload openclaw writes to the session store.
    /// Free-form; zterm treats it as opaque.
    #[serde(default)]
    pub entry: serde_json::Value,
}

/// Immediate ack payload returned by a `sessions.send` call.
///
/// Openclaw acknowledges the turn synchronously with a `runId` and
/// metadata, then broadcasts the actual response via `session.message`
/// events on subscribed connections. zterm collects the streaming
/// response in slice 3f's streaming consumer.
///
/// Extra fields openclaw may attach (e.g. `messageSeq`,
/// `interruptedActiveRun`, `cached`) are preserved verbatim in
/// `extra` for callers that need them without us chasing the
/// upstream schema every time it grows.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawSessionSendAck {
    /// Gateway-assigned run id for this turn — required to correlate
    /// streaming events and to call `sessions.abort`.
    #[serde(rename = "runId")]
    pub run_id: String,

    /// Monotonic message ordinal the server assigned this user turn
    /// (useful for clients that want to detect gaps in an event
    /// stream after reconnect).
    #[serde(
        default,
        rename = "messageSeq",
        skip_serializing_if = "Option::is_none"
    )]
    pub message_seq: Option<u32>,

    /// True when `sessions.steer` interrupted an already-running turn.
    /// Always false for plain `sessions.send`. Kept here so the type
    /// covers both entrypoints.
    #[serde(default, rename = "interruptedActiveRun")]
    pub interrupted_active_run: bool,

    /// Any additional fields openclaw attaches — kept opaque so
    /// upstream schema additions don't break client parsing.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One part of an openclaw transcript-style assistant message.
///
/// Openclaw's gateway stores messages in a content-parts shape where
/// each part is tagged by `type`. Known types:
///
/// - `"text"` — plain text content (the user-facing reply)
/// - `"thinking"` — model reasoning trace; hidden by default in
///   the REPL, surfaced only when the user asks
/// - `"tool_use"` — the model invoking a tool (a subsequent
///   `tool_result` part or event carries the response)
/// - `"tool_result"` — result body for a prior `tool_use`
/// - Anything else — preserved verbatim so forward-compat openclaw
///   additions do not fail parsing
#[derive(Debug, Clone)]
pub enum AssistantContentPart {
    Text(String),
    Thinking(String),
    ToolUse {
        name: Option<String>,
        raw: serde_json::Value,
    },
    ToolResult {
        raw: serde_json::Value,
    },
    Other {
        part_type: String,
        raw: serde_json::Value,
    },
}

impl AssistantContentPart {
    /// The user-facing text for this part, if any. Thinking / tool
    /// parts return None — they are surfaced through other channels
    /// in the UI.
    pub fn display_text(&self) -> Option<&str> {
        match self {
            AssistantContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        }
    }
}

/// Parsed assistant message content.
///
/// If the upstream content was a plain string, we wrap it in a
/// single `AssistantContentPart::Text` so callers have a uniform
/// shape. If it was an array of parts, we parse each part. If it
/// was something else entirely (unusual), we fall back to the
/// JSON-stringified form as a single Text part so nothing gets
/// silently dropped.
#[derive(Debug, Clone)]
pub struct AssistantContent {
    pub parts: Vec<AssistantContentPart>,
}

impl AssistantContent {
    /// Parse a `content` field from a session.message payload.
    pub fn parse(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::String(s) => Self {
                parts: vec![AssistantContentPart::Text(s.clone())],
            },
            serde_json::Value::Array(arr) => {
                let parts = arr.iter().map(parse_one_part).collect();
                Self { parts }
            }
            other => Self {
                parts: vec![AssistantContentPart::Text(other.to_string())],
            },
        }
    }

    /// Concatenation of all `text`-typed parts. The primary user-
    /// facing view; hides thinking + tool metadata. Empty string if
    /// no text parts exist (e.g. a pure tool-call turn).
    pub fn display_text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let Some(t) = part.display_text() {
                out.push_str(t);
            }
        }
        out
    }

    /// Concatenation of all `thinking`-typed parts. For UIs that
    /// want to expose model reasoning on request.
    pub fn thinking_text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let AssistantContentPart::Thinking(t) = part {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        out
    }

    /// True if this message is nothing but tool-call activity (no
    /// user-facing text). Useful for REPLs to decide whether to
    /// keep waiting for a subsequent assistant reply.
    pub fn is_tool_only(&self) -> bool {
        !self.parts.is_empty()
            && self.parts.iter().all(|p| {
                matches!(
                    p,
                    AssistantContentPart::ToolUse { .. } | AssistantContentPart::ToolResult { .. }
                )
            })
    }
}

fn parse_one_part(value: &serde_json::Value) -> AssistantContentPart {
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            // Non-object inside a content array: preserve as raw text.
            return AssistantContentPart::Text(value.to_string());
        }
    };
    let part_type = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match part_type.as_str() {
        "text" => {
            let t = obj
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            AssistantContentPart::Text(t)
        }
        "thinking" => {
            // openclaw / some providers carry the trace in either
            // "thinking" or "text" on a thinking-type part. Accept
            // either — preserves forward-compat.
            let t = obj
                .get("thinking")
                .or_else(|| obj.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            AssistantContentPart::Thinking(t)
        }
        "tool_use" | "tool_call" => {
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            AssistantContentPart::ToolUse {
                name,
                raw: value.clone(),
            }
        }
        "tool_result" | "tool_response" => AssistantContentPart::ToolResult { raw: value.clone() },
        _ => AssistantContentPart::Other {
            part_type,
            raw: value.clone(),
        },
    }
}

/// Structured result of a chat-turn round-trip.
///
/// Captures everything worth surfacing to a REPL / TUI renderer:
/// - `text` — the user-facing final assistant reply (concatenation
///   of `text`-type content parts from the terminal assistant
///   message). This is what `AgentClient::submit_turn` returns.
/// - `tool_calls` — ordered list of tool_use content parts the
///   model emitted across all assistant messages in the turn.
///   Each entry is the raw openclaw part JSON; UIs can pick out
///   `name` and `input` / `arguments` as appropriate.
/// - `tool_results` — ordered list of tool_result content parts
///   (openclaw persists these in the transcript too). Paired 1:1
///   with `tool_calls` when the model completes a tool cycle
///   within the turn.
/// - `thinking` — concatenation of all thinking-type parts
///   (newline-joined if multiple). Folded by default in UIs.
/// - `run_id` — gateway-assigned runId for this turn, echoed
///   back from the initial sessions.send ack. Needed for
///   sessions.abort and for correlating retroactive events.
/// - `usage` — token accounting when the gateway/provider includes
///   it on the session message or event payload.
#[derive(Debug, Clone, Default)]
pub struct TurnResult {
    pub text: String,
    pub tool_calls: Vec<serde_json::Value>,
    pub tool_results: Vec<serde_json::Value>,
    pub thinking: String,
    pub run_id: Option<String>,
    pub usage: Option<crate::cli::agent::TurnUsage>,
}

impl TurnResult {
    /// True if the model only invoked tools this turn without
    /// producing a final text reply. Happens when the model
    /// decides the transcript doesn't need explicit narration
    /// after the tool run (rare but observable).
    pub fn is_tool_only(&self) -> bool {
        self.text.is_empty() && !self.tool_calls.is_empty()
    }

    /// Merge an AssistantContent snapshot into the running turn
    /// result. Accumulates tool_use / tool_result / thinking
    /// parts. The `text` field is REPLACED (not appended) each
    /// call — the last assistant message with text content wins,
    /// matching the "final reply" semantics.
    pub fn merge(&mut self, content: &AssistantContent) {
        let this_text = content.display_text();
        if !this_text.is_empty() {
            self.text = this_text;
        }
        for part in &content.parts {
            match part {
                AssistantContentPart::Thinking(t) => {
                    if !self.thinking.is_empty() {
                        self.thinking.push('\n');
                    }
                    self.thinking.push_str(t);
                }
                AssistantContentPart::ToolUse { raw, .. } => {
                    self.tool_calls.push(raw.clone());
                }
                AssistantContentPart::ToolResult { raw } => {
                    self.tool_results.push(raw.clone());
                }
                AssistantContentPart::Text(_) | AssistantContentPart::Other { .. } => {
                    // Text parts are accumulated via the display_text
                    // assignment above. Other-typed parts are preserved
                    // only as the raw structured JSON would be; for
                    // REPL purposes they are ignored here.
                }
            }
        }
    }
}

/// Renderer mode for `format_turn_result`. `Compact` hides
/// thinking + tool_result bodies (common case — the user saw the
/// tool run, doesn't need the raw args/output). `Verbose` shows
/// everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TurnRenderMode {
    #[default]
    Compact,
    Verbose,
}

/// Format a TurnResult for terminal display.
///
/// Output shape (Compact):
///   [tool: <name>]                      one per tool_use
///   [tool result]                       one per tool_result
///   <final assistant text>              the .text field
///
/// Output shape (Verbose): prefixes with
///   [thinking]
///   <thinking body>
///   [tool: <name>]
///   args: <pretty JSON>
///   [tool result]
///   <stringified result content>
///   <final assistant text>
///
/// ANSI-free on purpose here — the Theme layer is owned by
/// `src/cli/ui.rs` and `src/cli/theme.rs`; callers that want
/// color wrap the output. Keeping this function pure keeps the
/// unit tests legible and lets the renderer be used by
/// non-terminal consumers (logs, transcript exports).
pub fn format_turn_result(turn: &TurnResult, mode: TurnRenderMode) -> String {
    let mut out = String::new();

    if mode == TurnRenderMode::Verbose && !turn.thinking.is_empty() {
        out.push_str("[thinking]\n");
        out.push_str(&turn.thinking);
        if !turn.thinking.ends_with('\n') {
            out.push('\n');
        }
    }

    // Tool calls + results, interleaved by order of appearance in
    // the turn. We don't try to pair them strictly — openclaw's
    // transcript already orders them, so the natural arrival
    // order is the right render order.
    let mut ci = turn.tool_calls.iter();
    let mut ri = turn.tool_results.iter();
    let total = turn.tool_calls.len() + turn.tool_results.len();
    for _ in 0..total {
        // Simple round-robin: emit one call, then one result, etc.
        // Good-enough for display; strict pairing would require
        // threading tool_use_id through AssistantContentPart which
        // is a heavier change.
        if let Some(call) = ci.next() {
            let name = call
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<tool>");
            out.push_str(&format!("[tool: {}]\n", name));
            if mode == TurnRenderMode::Verbose {
                if let Some(input) = call.get("input").or_else(|| call.get("arguments")) {
                    let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
                    out.push_str("args:\n");
                    out.push_str(&pretty);
                    out.push('\n');
                }
            }
        }
        if let Some(result) = ri.next() {
            out.push_str("[tool result]\n");
            if mode == TurnRenderMode::Verbose {
                // Openclaw tool_result parts carry the payload in
                // `content` — can be a string or a structured object.
                if let Some(c) = result.get("content") {
                    let body = match c {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    out.push_str(&body);
                    if !body.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
        }
    }

    if !turn.text.is_empty() {
        out.push_str(&turn.text);
    }

    out
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_handshake_params(token: Option<&str>) -> HandshakeParams {
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
            token: token.map(|s| s.to_string()),
        }
    }

    #[test]
    fn build_connect_params_shape_matches_protocol_v3() {
        let tmp = TempDir::new().unwrap();
        let device = DeviceIdentity::create(&tmp.path().join("k.pem")).unwrap();
        let params = sample_handshake_params(None);
        let json = build_connect_params(&device, &params, "nonce-x", 1_700_000_000_000).unwrap();

        assert_eq!(json["minProtocol"], 3);
        assert_eq!(json["maxProtocol"], 3);
        assert_eq!(json["role"], "operator");
        assert_eq!(json["client"]["id"], "cli");
        assert_eq!(json["client"]["displayName"], "zterm");
        assert_eq!(json["client"]["mode"], "cli");
        assert_eq!(json["client"]["platform"], "linux");
        // scopes preserve caller order
        assert_eq!(
            json["scopes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["operator.read", "operator.write"]
        );
        // device block is populated
        assert_eq!(json["device"]["id"], device.device_id());
        assert_eq!(json["device"]["publicKey"], device.public_key_b64url());
        assert_eq!(json["device"]["nonce"], "nonce-x");
        assert_eq!(json["device"]["signedAt"], 1_700_000_000_000i64);
        assert!(json["device"]["signature"].is_string());
        // no auth block when token is None
        assert!(json["auth"].is_null());
    }

    #[test]
    fn build_connect_params_includes_auth_token_when_provided() {
        let tmp = TempDir::new().unwrap();
        let device = DeviceIdentity::create(&tmp.path().join("k.pem")).unwrap();
        let params = sample_handshake_params(Some("shared-secret"));
        let json = build_connect_params(&device, &params, "n", 1_700_000_000_000).unwrap();
        assert_eq!(json["auth"]["token"], "shared-secret");
    }

    #[test]
    fn build_connect_params_signature_verifies_against_public_key() {
        use ed25519_dalek::Verifier;
        let tmp = TempDir::new().unwrap();
        let device = DeviceIdentity::create(&tmp.path().join("k.pem")).unwrap();
        let params = sample_handshake_params(None);
        let json = build_connect_params(&device, &params, "nonce-x", 1_700_000_000_000).unwrap();

        // Reconstruct the canonical bytes the server will recompute.
        let canonical = build_v3_payload(
            device.device_id(),
            &params.client.id,
            &params.client.mode,
            &params.role,
            &params.scopes,
            1_700_000_000_000,
            params.token.as_deref(),
            "nonce-x",
            Some(&params.client.platform),
            params.client.device_family.as_deref(),
        );
        let sig_b64 = json["device"]["signature"].as_str().unwrap();
        let sig_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, sig_b64)
                .unwrap();
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes.try_into().unwrap());
        // Load the device's verifying key to check.
        let pk_b64 = json["device"]["publicKey"].as_str().unwrap();
        let pk_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, pk_b64)
                .unwrap();
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes.try_into().unwrap()).unwrap();
        vk.verify(canonical.as_bytes(), &sig)
            .expect("signature must verify against the embedded public key");
    }

    #[test]
    fn hello_ok_deserializes_full_payload() {
        let json = serde_json::json!({
            "type": "hello-ok",
            "protocol": 3,
            "server": { "version": "2026.4.22", "connId": "c-xyz" },
            "features": {
                "methods": ["connect", "models.list", "sessions.send"],
                "events": ["session.message", "presence.update"]
            },
            "snapshot": { "presence": {"stub": true} },
            "auth": {
                "deviceToken": "dev-token-abc",
                "role": "operator",
                "scopes": ["operator.read", "operator.write"],
                "issuedAtMs": 1_700_000_000_000i64
            },
            "policy": {
                "maxPayload": 26214400,
                "maxBufferedBytes": 52428800,
                "tickIntervalMs": 15000
            }
        });
        let hello_ok: HelloOk = serde_json::from_value(json).unwrap();
        assert_eq!(hello_ok.kind, "hello-ok");
        assert_eq!(hello_ok.protocol, 3);
        assert_eq!(hello_ok.server.version, "2026.4.22");
        assert_eq!(hello_ok.features.methods.len(), 3);
        assert_eq!(hello_ok.policy.max_payload, 26_214_400);
        assert_eq!(
            hello_ok.auth.unwrap().device_token.unwrap(),
            "dev-token-abc"
        );
    }

    #[test]
    fn hello_ok_deserializes_minimal_payload() {
        // auth, canvasHostUrl omitted — openclaw schema says both are optional.
        let json = serde_json::json!({
            "type": "hello-ok",
            "protocol": 3,
            "server": { "version": "v", "connId": "c" },
            "features": { "methods": [], "events": [] },
            "snapshot": {},
            "policy": {
                "maxPayload": 1,
                "maxBufferedBytes": 1,
                "tickIntervalMs": 1
            }
        });
        let hello_ok: HelloOk = serde_json::from_value(json).unwrap();
        assert!(hello_ok.auth.is_none());
        assert!(hello_ok.canvas_host_url.is_none());
    }

    #[test]
    fn assistant_content_parses_plain_string() {
        let c = AssistantContent::parse(&serde_json::json!("hello world"));
        assert_eq!(c.display_text(), "hello world");
        assert!(c.thinking_text().is_empty());
        assert!(!c.is_tool_only());
    }

    #[test]
    fn assistant_content_parses_openclaw_parts_array() {
        // The exact shape we've been seeing live from TYPHON.
        let c = AssistantContent::parse(&serde_json::json!([
            {"type": "thinking", "thinking": "the user asked for 4"},
            {"type": "text", "text": "4"}
        ]));
        assert_eq!(c.display_text(), "4");
        assert_eq!(c.thinking_text(), "the user asked for 4");
        assert_eq!(c.parts.len(), 2);
    }

    #[test]
    fn assistant_content_concatenates_multiple_text_parts() {
        let c = AssistantContent::parse(&serde_json::json!([
            {"type": "text", "text": "hello "},
            {"type": "thinking", "thinking": "picking a greeting"},
            {"type": "text", "text": "world"}
        ]));
        assert_eq!(c.display_text(), "hello world");
    }

    #[test]
    fn assistant_content_tool_only_detected() {
        let c = AssistantContent::parse(&serde_json::json!([
            {"type": "tool_use", "name": "shell", "id": "tool-1"}
        ]));
        assert!(c.is_tool_only());
        assert_eq!(c.display_text(), "");
    }

    #[test]
    fn assistant_content_unknown_type_preserved_as_other() {
        let c = AssistantContent::parse(&serde_json::json!([
            {"type": "future_part_shape", "stuff": 42}
        ]));
        assert_eq!(c.parts.len(), 1);
        match &c.parts[0] {
            AssistantContentPart::Other { part_type, .. } => {
                assert_eq!(part_type, "future_part_shape");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn assistant_content_parses_thinking_via_text_field() {
        // Some providers put the reasoning in the `text` field on a
        // thinking-type part. Accept that variant.
        let c = AssistantContent::parse(&serde_json::json!([
            {"type": "thinking", "text": "step one, step two"}
        ]));
        assert_eq!(c.thinking_text(), "step one, step two");
        assert_eq!(c.display_text(), "");
    }

    #[tokio::test]
    async fn perform_handshake_errors_when_event_channel_closes() {
        // Drive perform_handshake against a fake event_rx that is already
        // closed (no challenge will ever arrive) — the function should
        // surface a clear "channel closed" error rather than hanging.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<EventFrame>(1);
        drop(tx); // channel now closed

        // OpenClawClient requires real plumbing; we build a fake that's
        // flagged disconnected so no request ever actually goes out. The
        // "closed channel" path short-circuits before the send.
        let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel(1);
        let client = super::super::client::tests_support_new_fake(
            super::super::wire::PendingRequests::new(),
            Some(outbound_tx),
            false,
        );
        let tmp = TempDir::new().unwrap();
        let device = DeviceIdentity::create(&tmp.path().join("k.pem")).unwrap();
        let params = sample_handshake_params(None);

        let err = perform_handshake(&client, &mut rx, &device, &params)
            .await
            .expect_err("closed channel should error");
        assert!(err.to_string().contains("channel closed"));
    }

    #[test]
    fn turn_result_merge_accumulates_tool_calls_and_thinking() {
        use super::TurnResult;
        let mut tr = TurnResult::default();
        let first = AssistantContent::parse(&serde_json::json!([
            {"type": "thinking", "thinking": "step 1"},
            {"type": "tool_use", "name": "shell", "id": "t1"}
        ]));
        let second = AssistantContent::parse(&serde_json::json!([
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
        ]));
        let third = AssistantContent::parse(&serde_json::json!([
            {"type": "thinking", "thinking": "step 2"},
            {"type": "text", "text": "All done."}
        ]));
        tr.merge(&first);
        tr.merge(&second);
        tr.merge(&third);

        assert_eq!(tr.text, "All done.");
        assert_eq!(tr.tool_calls.len(), 1);
        assert_eq!(tr.tool_results.len(), 1);
        assert_eq!(tr.thinking, "step 1\nstep 2");
        assert!(!tr.is_tool_only());
    }

    #[test]
    fn turn_result_is_tool_only_when_no_text() {
        use super::TurnResult;
        let mut tr = TurnResult::default();
        let c = AssistantContent::parse(&serde_json::json!([
            {"type": "tool_use", "name": "bash", "id": "t1"}
        ]));
        tr.merge(&c);
        assert!(tr.is_tool_only());
    }

    #[test]
    fn turn_result_merge_replaces_text_not_appends() {
        use super::TurnResult;
        let mut tr = TurnResult::default();
        let a = AssistantContent::parse(&serde_json::json!([
            {"type": "text", "text": "first reply"}
        ]));
        let b = AssistantContent::parse(&serde_json::json!([
            {"type": "text", "text": "second reply"}
        ]));
        tr.merge(&a);
        tr.merge(&b);
        // Last wins — the 'final assistant message' pattern.
        assert_eq!(tr.text, "second reply");
    }

    #[test]
    fn format_turn_result_compact_plain_text() {
        use super::{format_turn_result, TurnRenderMode, TurnResult};
        let tr = TurnResult {
            text: "hello".to_string(),
            ..Default::default()
        };
        assert_eq!(format_turn_result(&tr, TurnRenderMode::Compact), "hello");
    }

    #[test]
    fn format_turn_result_compact_tool_call_then_text() {
        use super::{format_turn_result, TurnRenderMode, TurnResult};
        let tr = TurnResult {
            text: "Ran the command.".to_string(),
            tool_calls: vec![serde_json::json!({
                "type": "tool_use", "name": "shell",
                "input": {"cmd": "echo hi"}
            })],
            tool_results: vec![serde_json::json!({
                "type": "tool_result", "content": "hi"
            })],
            ..Default::default()
        };
        let out = format_turn_result(&tr, TurnRenderMode::Compact);
        assert!(out.contains("[tool: shell]"));
        assert!(out.contains("[tool result]"));
        assert!(out.ends_with("Ran the command."));
        // Compact mode hides args and result body
        assert!(!out.contains("echo hi"));
        assert!(!out.contains("\nhi\n"));
    }

    #[test]
    fn format_turn_result_verbose_shows_thinking_and_args() {
        use super::{format_turn_result, TurnRenderMode, TurnResult};
        let tr = TurnResult {
            text: "Done.".to_string(),
            tool_calls: vec![serde_json::json!({
                "type": "tool_use", "name": "shell",
                "input": {"cmd": "ls"}
            })],
            tool_results: vec![serde_json::json!({
                "type": "tool_result", "content": "file1\nfile2"
            })],
            thinking: "planning the approach".to_string(),
            ..Default::default()
        };
        let out = format_turn_result(&tr, TurnRenderMode::Verbose);
        assert!(out.starts_with("[thinking]"));
        assert!(out.contains("planning the approach"));
        assert!(out.contains("[tool: shell]"));
        assert!(out.contains("\"cmd\": \"ls\""));
        assert!(out.contains("[tool result]"));
        assert!(out.contains("file1\nfile2"));
        assert!(out.ends_with("Done."));
    }

    #[test]
    fn format_turn_result_tool_only_no_text() {
        use super::{format_turn_result, TurnRenderMode, TurnResult};
        let tr = TurnResult {
            tool_calls: vec![serde_json::json!({
                "type": "tool_use", "name": "bash"
            })],
            ..Default::default()
        };
        let out = format_turn_result(&tr, TurnRenderMode::Compact);
        assert_eq!(out, "[tool: shell]\n".replace("shell", "bash"));
    }

    #[test]
    fn format_turn_result_empty_is_empty_string() {
        use super::{format_turn_result, TurnRenderMode, TurnResult};
        let tr = TurnResult::default();
        assert!(format_turn_result(&tr, TurnRenderMode::Compact).is_empty());
    }
}
