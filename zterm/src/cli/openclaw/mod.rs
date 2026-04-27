//! OpenClaw backend — v0.2 WIP.
//!
//! OpenClaw uses a pure-WebSocket wire protocol (no HTTP control plane)
//! with an ed25519 challenge-response handshake. See
//! <https://github.com/openclaw/openclaw/blob/main/docs/gateway/protocol.md>
//! for the canonical spec (protocol version 3).
//!
//! Module layout (grows in slices):
//!
//! - `device` — persistent ed25519 device identity (keypair on disk,
//!   base64url encoding, fingerprint + signing helpers). **Landed.**
//! - `wire` — JSON frame types (req/res/event) + request-id
//!   correlation via oneshot channels; PendingRequests tracker. **Landed.**
//! - `client` — WebSocket lifecycle (connect + read/write loops +
//!   request-id correlation). **Slice 3a landed**; handshake + AgentClient
//!   trait impl follow in slices 3b/3c.

pub mod client;
pub mod device;
pub mod handshake;
pub mod wire;
