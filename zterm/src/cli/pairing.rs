use anyhow::{anyhow, Result};
use serde_json::json;
use tracing::info;

/// Handle zeroclaw gateway pairing/authentication
pub struct PairingManager {
    gateway_url: String,
    client: reqwest::Client,
}

impl PairingManager {
    pub fn new(gateway_url: &str) -> Self {
        Self {
            gateway_url: gateway_url.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Get current pairing code
    pub async fn get_pairing_code(&self) -> Result<String> {
        let url = format!("{}/pair/code", self.gateway_url);
        info!("Fetching pairing code from: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to get pairing code: {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow!("Failed to get pairing code: {}", response.status()));
        }

        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| anyhow!("Failed to parse pairing response: {}", e))?;

        body.get("pairing_code")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("No code in pairing response"))
    }

    /// Complete pairing with bearer token
    pub async fn complete_pairing(&self, code: &str) -> Result<String> {
        let url = format!("{}/pair", self.gateway_url);
        info!("Completing pairing with code: {}", code);

        let response = self
            .client
            .post(&url)
            .json(&json!({ "code": code }))
            .send()
            .await
            .map_err(|e| anyhow!("Failed to complete pairing: {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow!("Pairing failed: {}", response.status()));
        }

        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| anyhow!("Failed to parse pairing response: {}", e))?;

        body.get("token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("No token in pairing response"))
    }

    /// Test if authentication is required
    pub async fn requires_pairing(&self) -> Result<bool> {
        let url = format!("{}/health", self.gateway_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Health check failed: {}", e))?;

        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| anyhow!("Failed to parse health response: {}", e))?;

        Ok(body
            .get("require_pairing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pairing_manager_creation() {
        let manager = PairingManager::new("http://localhost:42617");
        assert_eq!(manager.gateway_url, "http://localhost:42617");
    }
}
