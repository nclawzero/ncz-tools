use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Configuration directory: ~/.zeroclaw
pub fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not determine home directory"))?;
    Ok(home.join(".zeroclaw"))
}

/// Config file: ~/.zeroclaw/config.toml
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Sessions directory: ~/.zeroclaw/sessions
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("sessions"))
}

/// Input history file: ~/.zeroclaw/input_history.jsonl
pub fn history_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("input_history.jsonl"))
}

/// Session directory: ~/.zeroclaw/sessions/{session_id}
pub fn session_dir(session_id: &str) -> Result<PathBuf> {
    Ok(sessions_dir()?.join(session_id))
}

/// Session metadata: ~/.zeroclaw/sessions/{session_id}/meta.json
pub fn session_metadata_file(session_id: &str) -> Result<PathBuf> {
    Ok(session_dir(session_id)?.join("meta.json"))
}

/// Session history: ~/.zeroclaw/sessions/{session_id}/history.jsonl
pub fn session_history_file(session_id: &str) -> Result<PathBuf> {
    Ok(session_dir(session_id)?.join("history.jsonl"))
}

/// Ensure config directory exists
pub fn ensure_config_dir() -> Result<()> {
    let dir = config_dir()?;
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Ensure sessions directory exists
pub fn ensure_sessions_dir() -> Result<()> {
    let dir = sessions_dir()?;
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Ensure session directory exists
pub fn ensure_session_dir(session_id: &str) -> Result<()> {
    let dir = session_dir(session_id)?;
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Check if config exists
pub fn config_exists() -> Result<bool> {
    Ok(config_file()?.exists())
}

/// Session metadata structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    pub name: String,
    pub model: String,
    pub provider: String,
    pub created_at: String, // ISO 8601
    pub message_count: usize,
    pub last_active: String, // ISO 8601
}

/// Load session metadata from file
pub fn load_session_metadata(session_id: &str) -> Result<SessionMetadata> {
    let file = session_metadata_file(session_id)?;
    let content =
        fs::read_to_string(&file).map_err(|e| anyhow!("Failed to read session metadata: {}", e))?;
    let metadata: SessionMetadata = serde_json::from_str(&content)
        .map_err(|e| anyhow!("Failed to parse session metadata: {}", e))?;
    Ok(metadata)
}

/// Save session metadata to file
pub fn save_session_metadata(metadata: &SessionMetadata) -> Result<()> {
    ensure_session_dir(&metadata.id)?;
    let file = session_metadata_file(&metadata.id)?;
    let content = serde_json::to_string_pretty(&metadata)?;
    fs::write(&file, content).map_err(|e| anyhow!("Failed to write session metadata: {}", e))?;
    Ok(())
}

/// Load config from file
pub fn load_config() -> Result<String> {
    let file = config_file()?;
    fs::read_to_string(&file).map_err(|e| anyhow!("Failed to read config: {}", e))
}

/// Save config to file
pub fn save_config(content: &str) -> Result<()> {
    ensure_config_dir()?;
    let file = config_file()?;
    fs::write(&file, content).map_err(|e| anyhow!("Failed to write config: {}", e))?;
    Ok(())
}

/// List all sessions
pub fn list_sessions() -> Result<Vec<SessionMetadata>> {
    ensure_sessions_dir()?;
    let sessions_dir = sessions_dir()?;

    let mut sessions = Vec::new();

    if !sessions_dir.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if let Some(session_id) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(metadata) = load_session_metadata(session_id) {
                    sessions.push(metadata);
                }
            }
        }
    }

    // Sort by last_active descending
    sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));

    Ok(sessions)
}

/// Delete a session and all its files
pub fn delete_session(session_id: &str) -> Result<()> {
    let dir = session_dir(session_id)?;
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| anyhow!("Failed to delete session: {}", e))?;
    }
    Ok(())
}

/// Count total sessions
pub fn session_count() -> Result<usize> {
    Ok(list_sessions()?.len())
}

/// Update config with model settings (preserves other settings)
pub fn update_config_model(provider: &str, model: &str) -> Result<()> {
    ensure_config_dir()?;
    let file = config_file()?;

    let mut content = if file.exists() {
        fs::read_to_string(&file).unwrap_or_default()
    } else {
        String::new()
    };

    // Simple TOML update (find [zeroclaw] section or create it)
    if !content.contains("[zeroclaw]") {
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push_str("\n[zeroclaw]\n");
    }

    // Update or add provider line
    if content.contains("provider =") {
        content = content.replace(
            &format!(
                "provider = \"{}\"",
                extract_toml_value(&content, "provider").unwrap_or_default()
            ),
            &format!("provider = \"{}\"", provider),
        );
    } else {
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!("provider = \"{}\"\n", provider));
    }

    // Update or add model line
    if content.contains("model =") {
        content = content.replace(
            &format!(
                "model = \"{}\"",
                extract_toml_value(&content, "model").unwrap_or_default()
            ),
            &format!("model = \"{}\"", model),
        );
    } else {
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!("model = \"{}\"\n", model));
    }

    fs::write(&file, content).map_err(|e| anyhow!("Failed to update config: {}", e))?;

    Ok(())
}

/// Helper to extract TOML value
fn extract_toml_value(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        if line.starts_with(&format!("{} =", key)) {
            if let Some(val) = line.split('=').nth(1) {
                return Some(val.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_dir_exists() {
        let dir = config_dir();
        assert!(dir.is_ok());
    }

    #[test]
    fn test_ensure_directories() {
        let _ = ensure_config_dir();
        let _ = ensure_sessions_dir();
        assert!(config_dir().is_ok());
        assert!(sessions_dir().is_ok());
    }
}
