use anyhow::{anyhow, Result};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream};
use tracing::{debug, info};

/// WebSocket handler for zeroclaw gateway communication
pub struct WebSocketHandler {
    url: String,
    token: Option<String>,
}

impl WebSocketHandler {
    pub fn new(gateway_url: &str, token: Option<String>) -> Self {
        Self::with_session(gateway_url, "main", token)
    }

    pub fn with_session(gateway_url: &str, _session_id: &str, token: Option<String>) -> Self {
        // Convert HTTP URL to WebSocket URL (e.g., http://localhost:42617 → ws://localhost:42617)
        let ws_url = if gateway_url.starts_with("https://") {
            gateway_url.replace("https://", "wss://")
        } else {
            gateway_url.replace("http://", "ws://")
        };

        Self {
            url: format!("{}/ws/chat", ws_url),
            token,
        }
    }

    /// Connect to zeroclaw WebSocket and stream a message
    pub async fn stream_turn(&self, message: &str) -> Result<String> {
        info!("Connecting to WebSocket: {}", self.url);

        // Connect to WebSocket
        let url = if let Some(token) = &self.token {
            format!("{}?token={}", self.url, token)
        } else {
            self.url.clone()
        };

        let (ws_stream, _) = connect_async(&url)
            .await
            .map_err(|e| anyhow!("WebSocket connection failed: {}", e))?;

        info!("WebSocket connected");

        // Send message
        let mut response = String::new();
        self.stream_messages(ws_stream, message, &mut response)
            .await?;

        Ok(response)
    }

    /// Stream messages through WebSocket connection
    async fn stream_messages(
        &self,
        mut ws_stream: tokio_tungstenite::WebSocketStream<MaybeTlsStream<TcpStream>>,
        message: &str,
        response: &mut String,
    ) -> Result<()> {
        info!("stream_messages: Starting (DEBUG)");

        // Send initialization/auth message if needed (some WS servers require handshake)
        // Try sending with complete message envelope including session info
        let session_id = format!("session-{}", Utc::now().timestamp());
        let request_id = format!("req-{}", Utc::now().timestamp_millis());

        // Send message with full envelope (session_id, role, request_id, model).
        //
        // The `model` field is a zeroclaw provider-key from
        // `[providers.models.*]` (e.g. `primary`, `consult`,
        // `together`) — NOT a literal upstream model identifier.
        // Resolution: ZTERM_MODEL env var override → static
        // `"primary"` (a neutral config-key string). This legacy
        // helper stays env-only; the TUI path carries the live
        // `/models set <key>` selection through `ZeroclawClient`.
        let model = std::env::var("ZTERM_MODEL").unwrap_or_else(|_| "primary".to_string());
        let request = json!({
            "type": "message",
            "content": message,
            "session_id": &session_id,
            "role": "user",
            "request_id": &request_id,
            "model": model
        });

        info!(
            "Sending message with envelope: {}",
            serde_json::to_string_pretty(&request)?
        );

        let msg = Message::Text(request.to_string());
        ws_stream
            .send(msg)
            .await
            .map_err(|e| anyhow!("Failed to send message: {}", e))?;

        debug!("Message sent: {}", message);

        // Receive streaming response
        let mut spinner_frame = 0;
        let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut response_received = false;

        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    debug!("Raw WebSocket message: {}", text);

                    if let Ok(json) = serde_json::from_str::<Value>(&text) {
                        match json.get("type").and_then(|v| v.as_str()) {
                            Some("chunk") => {
                                // Token data from /ws/chat
                                if let Some(data) = json.get("content").and_then(|v| v.as_str()) {
                                    response.push_str(data);
                                    print!("{}", data);
                                    std::io::Write::flush(&mut std::io::stdout())?;
                                    response_received = true;
                                }
                            }
                            Some("stream") => {
                                // Token data
                                if let Some(data) = json.get("data").and_then(|v| v.as_str()) {
                                    response.push_str(data);
                                    print!("{}", data);
                                    std::io::Write::flush(&mut std::io::stdout())?;
                                    response_received = true;
                                }
                            }
                            Some("done") => {
                                if !response_received {
                                    if let Some(full) = json
                                        .get("full_response")
                                        .or_else(|| json.get("response"))
                                        .and_then(|v| v.as_str())
                                    {
                                        response.push_str(full);
                                        print!("{}", full);
                                        std::io::Write::flush(&mut std::io::stdout())?;
                                    }
                                }
                                // Stream complete
                                println!();
                                debug!("Stream completed");
                                break;
                            }
                            Some("error") => {
                                // Error message
                                if let Some(error) = json
                                    .get("message")
                                    .or_else(|| json.get("error"))
                                    .and_then(|v| v.as_str())
                                {
                                    return Err(anyhow!("WebSocket error: {}", error));
                                }
                            }
                            _ => {
                                debug!("Unknown message type: {}", json);
                                // If we got a response but not in expected format, log it
                                if !response_received {
                                    eprintln!("⚠️  Received unexpected message format: {}", text);
                                }
                            }
                        }
                    } else {
                        // Non-JSON message
                        debug!("Non-JSON message received: {}", text);
                        if !response_received {
                            eprintln!("⚠️  Received non-JSON message: {}", text);
                        }
                    }
                }
                Ok(Message::Close(frame)) => {
                    debug!("WebSocket closed by server: {:?}", frame);
                    eprintln!("⚠️  WebSocket closed by server (no response received)");
                    break;
                }
                Ok(msg) => {
                    // Log ping/pong/binary for debugging
                    debug!("Received non-text frame: {:?}", msg);
                }
                Err(e) => {
                    return Err(anyhow!("WebSocket error: {}", e));
                }
            }

            // Update spinner
            spinner_frame = (spinner_frame + 1) % spinner_frames.len();
        }

        Ok(())
    }
}

/// WebSocket message types for zeroclaw protocol
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum WebSocketMessage {
    #[serde(rename = "message")]
    Message { content: String },

    #[serde(rename = "stream")]
    Stream { data: String },

    #[serde(rename = "done")]
    Done,

    #[serde(rename = "error")]
    Error { error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_url_conversion() {
        let handler = WebSocketHandler::new("http://localhost:42617", None);
        assert!(handler.url.starts_with("ws://"));
        assert!(handler.url.contains("42617"));
        assert!(handler.url.ends_with("/ws/chat"));
    }

    #[test]
    fn test_https_to_wss_conversion() {
        let handler = WebSocketHandler::new("https://example.com:42617", None);
        assert!(handler.url.starts_with("wss://"));
    }

    #[test]
    fn test_message_serialization() {
        let msg = WebSocketMessage::Message {
            content: "test".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"message\""));
    }
}
