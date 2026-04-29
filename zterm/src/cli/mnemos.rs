//! MNEMOS memory-daemon client — user-global, not agent-scoped.
//!
//! MNEMOS is a separate daemon from the agent backends (zeroclaw,
//! openclaw, etc.). It holds user-global memory records and is
//! shared across all workspaces / backends inside a single zterm
//! session. Keeping this client on a dedicated type (rather than
//! welded to `ZeroclawClient`) aligns with the scope rule captured
//! in `project_zterm_backend_scope`: MNEMOS is orthogonal to the
//! agent control plane and survives backend-switching cleanly.
//!
//! Targets MNEMOS v3.0 on port 5002 (unified API, Bearer auth).
//!
//! Canonical endpoints:
//! - `POST   /memories/search` — full-text / semantic search
//! - `GET    /memories` — list recent (limit + offset)
//! - `GET    /memories/{id}` — fetch one
//! - `POST   /memories` — create
//! - `DELETE /memories/{id}` — delete one
//! - `GET    /stats` — storage + category breakdown
//!
//! MNEMOS is opt-in. If neither MNEMOS_URL nor MNEMOS_TOKEN are set
//! in the environment (or .env), `MnemosClient::from_env()` returns
//! None and every `/memory` command reports "not configured"
//! cleanly. No hardcoded endpoint or credential lives in source.
//!
//! Status: module added in v0.2 roadmap chunk A-1. A follow-up
//! slice migrates command dispatchers to call this client directly
//! and removes the duplicated methods on `ZeroclawClient`.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde_json::json;
use std::time::Duration;

const MNEMOS_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// MNEMOS v3.0 REST client. Cheap to clone — wraps a reqwest
/// client + a base URL + a bearer token.
#[derive(Debug, Clone)]
pub struct MnemosClient {
    http: Client,
    base_url: String,
    token: String,
}

impl MnemosClient {
    /// Build from explicit URL + token. Normalizes trailing slash.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http: Client::new(),
            base_url,
            token: token.into(),
        }
    }

    /// Read MNEMOS configuration from environment (MNEMOS_URL +
    /// MNEMOS_TOKEN). Both must be set and non-empty; returns None
    /// otherwise so callers can cleanly disable memory features.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("MNEMOS_URL").ok()?;
        let token = std::env::var("MNEMOS_TOKEN").ok()?;
        if url.trim().is_empty() || token.trim().is_empty() {
            return None;
        }
        Some(Self::new(url, token))
    }

    /// `POST /memories/search` — full-text / semantic search.
    /// Returns empty on transport failure rather than erroring,
    /// matching the semantics v0.1 /memory commands expect.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
        let url = format!("{}/memories/search", self.base_url);
        let payload = json!({ "query": query, "limit": limit });
        let res = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&payload)
            .timeout(MNEMOS_REQUEST_TIMEOUT)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(data) => Ok(unwrap_memory_envelope(&data)),
                Err(_) => Ok(vec![]),
            },
            _ => Ok(vec![]),
        }
    }

    /// `GET /stats` — category + storage counts. Returns a status
    /// marker JSON object on transport failure so `/memory stats`
    /// can show a friendly "offline" without bubbling the error.
    pub async fn stats(&self) -> Result<serde_json::Value> {
        let url = format!("{}/stats", self.base_url);
        let res = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(MNEMOS_REQUEST_TIMEOUT)
            .send()
            .await;
        match res {
            Ok(r) => r
                .json::<serde_json::Value>()
                .await
                .or_else(|_| Ok(json!({ "status": "unavailable" }))),
            Err(_) => Ok(json!({ "status": "offline" })),
        }
    }

    /// `GET /memories?limit=N` — recent memories.
    pub async fn list(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        let url = format!("{}/memories?limit={}", self.base_url, limit);
        let res = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(MNEMOS_REQUEST_TIMEOUT)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or(json!({}));
                Ok(unwrap_memory_envelope(&data))
            }
            _ => Ok(vec![]),
        }
    }

    /// `GET /memories/{id}` — fetch one. Returns None on 404 /
    /// transport failure.
    pub async fn get(&self, id: &str) -> Result<Option<serde_json::Value>> {
        let url = format!("{}/memories/{}", self.base_url, id);
        let res = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(MNEMOS_REQUEST_TIMEOUT)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => Ok(r.json::<serde_json::Value>().await.ok()),
            _ => Ok(None),
        }
    }

    /// `POST /memories` — create a new memory record.
    pub async fn create(&self, content: &str, category: Option<&str>) -> Result<serde_json::Value> {
        let url = format!("{}/memories", self.base_url);
        let mut payload = json!({ "content": content });
        if let Some(c) = category {
            payload["category"] = json!(c);
        }
        let r = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&payload)
            .timeout(MNEMOS_REQUEST_TIMEOUT)
            .send()
            .await?;
        if r.status().is_success() {
            Ok(r.json::<serde_json::Value>().await.unwrap_or(json!({})))
        } else {
            Err(anyhow!("MNEMOS create failed: HTTP {}", r.status()))
        }
    }

    /// `DELETE /memories/{id}`.
    pub async fn delete(&self, id: &str) -> Result<()> {
        let url = format!("{}/memories/{}", self.base_url, id);
        let r = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .timeout(MNEMOS_REQUEST_TIMEOUT)
            .send()
            .await?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("MNEMOS delete failed: HTTP {}", r.status()))
        }
    }
}

/// Normalize MNEMOS v3.0 response envelopes into a plain
/// `Vec<Value>`. v3.0 `/memories` and `/memories/search` both
/// return `{ "count": N, "memories": [...] }`. We also accept the
/// legacy v2.x `{ "results": [...] }` shape and a bare top-level
/// array, so zterm works unchanged against older deployments
/// during the rollout.
pub fn unwrap_memory_envelope(data: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(arr) = data.get("memories").and_then(|v| v.as_array()) {
        return arr.clone();
    }
    if let Some(arr) = data.get("results").and_then(|v| v.as_array()) {
        return arr.clone();
    }
    if let Some(arr) = data.as_array() {
        return arr.clone();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwrap_v3() {
        let payload = json!({
            "count": 2,
            "memories": [ {"id": "mem_a"}, {"id": "mem_b"} ]
        });
        let out = unwrap_memory_envelope(&payload);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["id"], "mem_a");
    }

    #[test]
    fn unwrap_v2_legacy() {
        let payload = json!({ "results": [ {"id": "m1"} ] });
        assert_eq!(unwrap_memory_envelope(&payload).len(), 1);
    }

    #[test]
    fn unwrap_bare_array() {
        let payload = json!([ {"id": "m1"}, {"id": "m2"} ]);
        assert_eq!(unwrap_memory_envelope(&payload).len(), 2);
    }

    #[test]
    fn unwrap_empty() {
        let payload = json!({ "count": 0, "memories": [] });
        assert!(unwrap_memory_envelope(&payload).is_empty());
    }

    #[test]
    fn unwrap_unknown_shape() {
        let payload = json!({ "status": "offline" });
        assert!(unwrap_memory_envelope(&payload).is_empty());
    }

    #[test]
    fn new_trims_trailing_slash() {
        let c = MnemosClient::new("http://host:5002/", "tok");
        assert_eq!(c.base_url, "http://host:5002");
    }
}
