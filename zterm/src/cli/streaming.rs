use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use reqwest::Client;
use std::io::{self, Write};
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;
use tokio::time::sleep;
use tokio_util::io::StreamReader;
use tracing::{debug, warn};

use crate::cli::agent::{StreamSink, TurnChunk};

/// Spinner animation frames
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Streaming response handler
pub struct StreamHandler {
    client: Client,
    buffer: String,
    last_flush: Instant,
    flush_interval: Duration,
    /// When `Some`, tokens are forwarded to the TUI via this channel
    /// instead of being printed to stdout. The legacy rustyline REPL
    /// leaves this `None` and keeps the existing spinner-plus-stream
    /// stdout UX.
    sink: Option<StreamSink>,
}

impl StreamHandler {
    /// Create a new stream handler (stdout path — legacy REPL).
    pub fn new(client: Client) -> Self {
        Self {
            client,
            buffer: String::new(),
            last_flush: Instant::now(),
            flush_interval: Duration::from_millis(50),
            sink: None,
        }
    }

    /// Install a TUI-bound streaming sink. When set, tokens are
    /// forwarded as `TurnChunk::Token(_)` rather than printed to
    /// stdout, and the spinner is suppressed (the TUI renders its
    /// own in E-7).
    pub fn with_sink(mut self, sink: StreamSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Stream a turn response from the gateway
    pub async fn stream_turn(&mut self, url: &str, token: &str) -> Result<String> {
        debug!("Starting SSE stream from {}", url);

        let mut attempts = 0;
        const MAX_RETRIES: u32 = 3;

        loop {
            match self.try_stream(url, token).await {
                Ok(response) => {
                    if self.sink.is_none() {
                        println!(); // New line after spinner (stdout path only)
                    }
                    return Ok(response);
                }
                Err(e) if attempts < MAX_RETRIES => {
                    let backoff = Duration::from_millis(500 * (attempts + 1) as u64);
                    warn!(
                        "Stream error (attempt {}): {}. Retrying in {:?}...",
                        attempts + 1,
                        e,
                        backoff
                    );

                    if self.sink.is_none() {
                        println!("❌ {}", e);
                        println!("🔄 Retrying in {:?}...", backoff);
                    }

                    sleep(backoff).await;
                    attempts += 1;
                }
                Err(e) => {
                    if self.sink.is_none() {
                        println!(); // New line after spinner (stdout path only)
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Try to stream once (internal, with retry)
    async fn try_stream(&mut self, url: &str, token: &str) -> Result<String> {
        self.buffer.clear();
        let mut response = String::new();
        let mut spinner_index = 0;
        let mut last_spinner_update = Instant::now();

        let req = self
            .client
            .post(url)
            .bearer_auth(token)
            .header("Accept", "text/event-stream");

        let res = req
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to stream: {}", e))?;

        if !res.status().is_success() {
            return Err(anyhow!("Stream error: HTTP {}", res.status()));
        }

        let body_stream = res.bytes_stream();
        let stream_reader = StreamReader::new(body_stream.map_err(std::io::Error::other));
        let mut reader = tokio::io::BufReader::new(stream_reader);

        // Read SSE lines
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break; // EOF
            }

            let trimmed = line.trim();

            // Parse SSE data line
            if let Some(data) = trimmed.strip_prefix("data: ") {
                if !data.is_empty() && data != "[DONE]" {
                    response.push_str(data);
                    self.buffer.push_str(data);

                    // Flush buffered output periodically. The TUI sink
                    // path forwards every chunk eagerly so the chat
                    // pane feels responsive; the stdout path keeps
                    // the original 50ms-interval flush to avoid
                    // thrashing the terminal.
                    match &self.sink {
                        Some(sink) => {
                            if !self.buffer.is_empty() {
                                let _ =
                                    sink.send(TurnChunk::Token(std::mem::take(&mut self.buffer)));
                            }
                        }
                        None => {
                            if self.last_flush.elapsed() >= self.flush_interval {
                                print!("{}", self.buffer);
                                io::stdout().flush()?;
                                self.buffer.clear();
                                self.last_flush = Instant::now();
                            }
                        }
                    }
                }
            }

            // Spinner is a stdout affordance for the legacy REPL only.
            // TUI sink path suppresses it — E-7 adds a TApplication
            // in-frame spinner in its place.
            if self.sink.is_none() && last_spinner_update.elapsed() >= Duration::from_millis(80) {
                print!("\r{} ", SPINNER_FRAMES[spinner_index]);
                io::stdout().flush()?;
                spinner_index = (spinner_index + 1) % SPINNER_FRAMES.len();
                last_spinner_update = Instant::now();
            }
        }

        // Flush any remaining buffered output.
        if !self.buffer.is_empty() {
            match &self.sink {
                Some(sink) => {
                    let _ = sink.send(TurnChunk::Token(std::mem::take(&mut self.buffer)));
                }
                None => {
                    print!("{}", self.buffer);
                    io::stdout().flush()?;
                }
            }
        }

        Ok(response)
    }
}

/// Simple spinner for operations
pub struct Spinner {
    frames: Vec<&'static str>,
    current: usize,
}

impl Spinner {
    /// Create a new spinner
    pub fn new() -> Self {
        Self {
            frames: SPINNER_FRAMES.to_vec(),
            current: 0,
        }
    }

    /// Get next spinner frame
    pub fn next_frame(&mut self) -> &'static str {
        let frame = self.frames[self.current];
        self.current = (self.current + 1) % self.frames.len();
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_creation() {
        let spinner = Spinner::new();
        assert_eq!(spinner.frames.len(), 10);
    }

    #[test]
    fn test_spinner_cycling() {
        let mut spinner = Spinner::new();
        let first = spinner.next_frame();
        let _ = spinner.next_frame();
        let _ = spinner.next_frame();
        // After cycling through all, should return to first
        for _ in 0..7 {
            spinner.next_frame();
        }
        let cycled = spinner.next_frame();
        assert_eq!(first, cycled);
    }
}
