//! Live smoke test: OpenClawClient end-to-end handshake against a real gateway.
//!
//! Gated by `#[ignore]` — ordinary `cargo test` skips it. Run explicitly
//! on a host where an openclaw gateway is reachable:
//!
//! ```sh
//! OPENCLAW_URL=ws://127.0.0.1:18789 \\
//!     cargo test --test openclaw_live -- --ignored --nocapture
//! ```
//!
//! The test:
//! 1. Loads (or generates in a tempdir) a zterm device keypair.
//! 2. WebSocket-upgrades to $OPENCLAW_URL (default
//!    `ws://127.0.0.1:18789`).
//! 3. Pulls the `connect.challenge` event off `event_rx`.
//! 4. Builds + signs a v3 canonical payload.
//! 5. Sends the `connect` request and asserts the server returns
//!    either a valid `hello-ok` OR a known error code that tells us
//!    about the pairing state. Both outcomes are informative.
//!
//! Result categories we handle cleanly:
//! - **`hello-ok`** — full handshake success. Asserts minimum schema
//!   shape (protocol version 3, features.methods includes `connect`).
//! - **`PAIRING_REQUIRED`** (or similar) — server recognized the
//!   request but the keypair isn't paired yet. That's expected for
//!   a fresh zterm device on an auth-configured gateway; the test
//!   records the error shape and exits ok (documenting what a
//!   first-time-pair flow looks like).
//! - **`UNAUTHORIZED`** — shared-secret mode and we provided no token.
//!   Same handling: expected, informative, test records + exits ok.
//! - **Any other error** — the test fails, printing the full response
//!   body so we can diagnose drift.

use std::env;
use std::time::Duration;
use tempfile::TempDir;

use zterm::cli::openclaw::client::{
    OpenClawClient, SessionsCreateOpts, SessionsListOpts, SessionsSendOpts,
};
use zterm::cli::openclaw::device::DeviceIdentity;
use zterm::cli::openclaw::handshake::{ClientIdentity, HandshakeParams};

fn openclaw_url() -> String {
    env::var("OPENCLAW_URL").unwrap_or_else(|_| "ws://127.0.0.1:18789".to_string())
}

fn expected_outcomes_allowlist() -> &'static [&'static str] {
    // Known error codes that count as "handshake reached the server"
    // without being a hard failure. Extend as we learn the gateway's
    // actual rejection shapes.
    &[
        "PAIRING_REQUIRED",
        "UNAUTHORIZED",
        "DEVICE_NOT_TRUSTED",
        "AWAITING_APPROVAL",
    ]
}

#[tokio::test]
#[ignore] // live — run with --ignored against a real gateway
async fn openclaw_live_handshake_against_typhon_gateway() {
    let url = openclaw_url();
    eprintln!("openclaw-live: connecting to {url}");

    // Fresh device keypair per test run. Pairing state is therefore
    // also fresh — expect PAIRING_REQUIRED on the first run against
    // any gateway with auth enforcement.
    let tmp = TempDir::new().expect("tempdir");
    let device = DeviceIdentity::create(&tmp.path().join("openclaw-device.pem")).expect("keypair");
    eprintln!(
        "openclaw-live: device_id={} publicKey={}",
        device.device_id(),
        device.public_key_b64url()
    );

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
        token: env::var("OPENCLAW_TOKEN").ok(),
    };

    let result = tokio::time::timeout(
        Duration::from_secs(15),
        OpenClawClient::connect_and_handshake(&url, &device, &params),
    )
    .await
    .expect("handshake timed out");

    match result {
        Ok(mut client) => {
            let hello_ok = client
                .hello_ok()
                .expect("hello_ok must be set on a handshaken client");
            eprintln!(
                "openclaw-live: hello-ok protocol={} server_version={} conn_id={}",
                hello_ok.protocol, hello_ok.server.version, hello_ok.server.conn_id
            );
            eprintln!(
                "openclaw-live: features.methods ({} total): {}",
                hello_ok.features.methods.len(),
                hello_ok
                    .features
                    .methods
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            assert_eq!(hello_ok.kind, "hello-ok");
            assert_eq!(hello_ok.protocol, 3);
            assert!(
                hello_ok.features.methods.iter().any(|m| m == "health"),
                "hello-ok features should list the post-handshake health method"
            );

            // Slice 3c RPC smokes: health + models.list
            let healthy = client.rpc_health().await.expect("health rpc");
            eprintln!("openclaw-live: rpc_health() => {healthy}");
            assert!(healthy, "health rpc should succeed on a live gateway");

            let models = client.rpc_models_list().await.expect("models.list rpc");
            eprintln!(
                "openclaw-live: models.list returned {} entries (first 5): {}",
                models.len(),
                models
                    .iter()
                    .take(5)
                    .map(|m| format!("{}/{}", m.provider, m.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            // Slice 3d: sessions.list baseline
            let list_opts = SessionsListOpts {
                limit: Some(10),
                include_derived_titles: false,
                include_last_message: false,
                ..Default::default()
            };
            let listing = client
                .rpc_sessions_list(list_opts)
                .await
                .expect("sessions.list rpc");
            eprintln!(
                "openclaw-live: sessions.list found {} sessions (path={})",
                listing.count, listing.path
            );
            for row in listing.sessions.iter().take(5) {
                eprintln!(
                    "  · key={} kind={} label={:?}",
                    row.key, row.kind, row.label
                );
            }

            // Slice 3d: sessions.create round-trip
            // Bootstrap-mode gateways (auth.mode:none loopback) should
            // accept a session with a caller-generated key and no
            // agentId. On stricter gateways this may fail; treat
            // failure as informational, not a test-wide fail.
            let create_key = format!("zterm-live-smoke-{}", chrono::Utc::now().timestamp_millis());
            let create_opts = SessionsCreateOpts {
                key: Some(create_key.clone()),
                label: Some(format!(
                    "zterm-live-smoke-{}",
                    chrono::Utc::now().timestamp_millis()
                )),
                ..Default::default()
            };
            let send_target_key = match client.rpc_sessions_create(create_opts).await {
                Ok(created) => {
                    eprintln!(
                        "openclaw-live: sessions.create ok — key={} label={:?}",
                        created.key, created.label
                    );
                    Some(created.key)
                }
                Err(e) => {
                    eprintln!("openclaw-live: sessions.create declined (informational): {e}");
                    None
                }
            };

            if let Some(session_key) = send_target_key {
                let send_opts = SessionsSendOpts {
                    idempotency_key: Some(format!(
                        "zterm-live-smoke-{}",
                        chrono::Utc::now().timestamp_millis()
                    )),
                    timeout_ms: Some(2_000),
                    ..Default::default()
                };
                match client
                    .rpc_sessions_send(&session_key, "hi — zterm live smoke", send_opts)
                    .await
                {
                    Ok(ack) => eprintln!(
                        "openclaw-live: sessions.send ok — runId={} messageSeq={:?}",
                        ack.run_id, ack.message_seq
                    ),
                    Err(e) => {
                        eprintln!("openclaw-live: sessions.send declined (informational): {e}")
                    }
                }

                // Slice 3f: send + collect assistant reply via stream.
                let collect_opts = SessionsSendOpts {
                    idempotency_key: Some(format!(
                        "zterm-collect-{}",
                        chrono::Utc::now().timestamp_millis()
                    )),
                    timeout_ms: Some(4_000),
                    ..Default::default()
                };
                match client
                    .rpc_sessions_send_and_collect(
                        &session_key,
                        "reply with just the number 4",
                        collect_opts,
                        std::time::Duration::from_secs(15),
                    )
                    .await
                {
                    Ok(text) => eprintln!(
                        "openclaw-live: send_and_collect ok — {} chars: {}",
                        text.len(),
                        text.chars().take(80).collect::<String>()
                    ),
                    Err(e) => {
                        eprintln!("openclaw-live: send_and_collect declined (informational): {e}")
                    }
                }

                // Slice B-1: rich variant — accumulates tool_calls +
                // thinking across assistant messages.
                let rich_opts = SessionsSendOpts {
                    idempotency_key: Some(format!(
                        "zterm-rich-{}",
                        chrono::Utc::now().timestamp_millis()
                    )),
                    timeout_ms: Some(4_000),
                    ..Default::default()
                };
                match client
                    .rpc_sessions_send_and_collect_rich(
                        &session_key,
                        "think briefly and then say hi",
                        rich_opts,
                        std::time::Duration::from_secs(15),
                    )
                    .await
                {
                    Ok(turn) => eprintln!(
                        "openclaw-live: send_and_collect_rich ok —                          text={}ch thinking={}ch tool_calls={} tool_results={} run_id={:?}",
                        turn.text.len(),
                        turn.thinking.len(),
                        turn.tool_calls.len(),
                        turn.tool_results.len(),
                        turn.run_id
                    ),
                    Err(e) => eprintln!(
                        "openclaw-live: send_and_collect_rich declined (informational): {e}"
                    ),
                }
            }

            client.disconnect().await;
            return;
        }
        Err(e) => {
            let msg = e.to_string();
            eprintln!("openclaw-live: handshake error: {msg}");
            let allowlist = expected_outcomes_allowlist();
            let recognized = allowlist.iter().any(|code| msg.contains(code));
            assert!(
                recognized,
                "unexpected handshake error (not in allowlist {:?}): {msg}",
                allowlist
            );
            eprintln!(
                "openclaw-live: recognized expected pre-pairing state — handshake reached server OK"
            );
        }
    }
}

#[tokio::test]
#[ignore] // live — run with --ignored against a real gateway
async fn openclaw_live_workspace_activate_against_typhon_gateway() {
    use zterm::cli::workspace::{Backend, Workspace, WorkspaceConfig};

    let url = openclaw_url();
    eprintln!("openclaw-live: workspace activate to {url}");

    // Point zterm's device-key path at a tempdir for this test so we
    // do not touch ~/.zterm/openclaw-device.pem.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::env::set_var("ZTERM_CONFIG_DIR", tmp.path());

    let cfg = WorkspaceConfig {
        id: None,
        name: "live-activate-test".to_string(),
        backend: Backend::Openclaw,
        url,
        token_env: None,
        token: std::env::var("OPENCLAW_TOKEN").ok(),
        label: None,
        namespace_aliases: Vec::new(),
    };

    let mut ws = Workspace::instantiate(0, cfg).expect("instantiate");
    assert!(
        !ws.is_activated(),
        "fresh openclaw workspace should not be activated"
    );

    let res = tokio::time::timeout(std::time::Duration::from_secs(15), ws.activate())
        .await
        .expect("activate timed out");

    match res {
        Ok(()) => {
            assert!(
                ws.is_activated(),
                "workspace should be activated after activate()"
            );
            eprintln!(
                "openclaw-live: workspace'{}' activated; client Arc strong_count={}",
                ws.config.name,
                std::sync::Arc::strong_count(ws.client.as_ref().unwrap())
            );
        }
        Err(e) => {
            let msg = e.to_string();
            eprintln!("openclaw-live: workspace activate error: {msg}");
            let allowlist = [
                "PAIRING_REQUIRED",
                "UNAUTHORIZED",
                "DEVICE_NOT_TRUSTED",
                "AWAITING_APPROVAL",
            ];
            let recognized = allowlist.iter().any(|c| msg.contains(c));
            assert!(
                recognized,
                "unexpected activate error (not in allowlist): {msg}",
            );
        }
    }
}
