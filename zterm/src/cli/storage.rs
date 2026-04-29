use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

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

/// Workspace-scoped sessions directory: ~/.zeroclaw/sessions/workspaces
pub fn workspace_sessions_dir() -> Result<PathBuf> {
    Ok(sessions_dir()?.join("workspaces"))
}

/// Input history file: ~/.zeroclaw/input_history.jsonl
pub fn history_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("input_history.jsonl"))
}

/// Local zterm storage scope for backend session files.
///
/// `SessionMetadata.id` remains the backend `Session.id`; this scope
/// is only the local filesystem namespace that prevents two active
/// workspaces from sharing metadata/history when their backends both
/// use the same session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalWorkspaceScope {
    backend: String,
    namespace: Option<String>,
    name: Option<String>,
    id: Option<String>,
}

impl LocalWorkspaceScope {
    pub fn new(
        backend: impl Into<String>,
        namespace: Option<String>,
        name: Option<String>,
        id: Option<String>,
    ) -> Result<Self> {
        let scope = Self {
            backend: backend.into(),
            namespace,
            name,
            id,
        };
        if scope.backend.trim().is_empty()
            || scope
                .namespace
                .as_deref()
                .map(|value| value.trim().is_empty())
                .unwrap_or(false)
            || scope
                .name
                .as_deref()
                .map(|value| value.trim().is_empty())
                .unwrap_or(false)
            || scope
                .id
                .as_deref()
                .map(|value| value.trim().is_empty())
                .unwrap_or(false)
        {
            return Err(anyhow!("invalid empty local workspace storage scope"));
        }
        Ok(scope)
    }

    pub fn identity(&self) -> String {
        let mut parts = vec![format!("backend={}", self.backend)];
        let has_immutable_workspace_identity = self.id.is_some()
            || self
                .namespace
                .as_deref()
                .map(|namespace| !namespace.contains("workspace="))
                .unwrap_or(false);
        if let Some(namespace) = &self.namespace {
            parts.push(format!("namespace={namespace}"));
        }
        if !has_immutable_workspace_identity {
            if let Some(name) = &self.name {
                parts.push(format!("workspace={name}"));
            }
        }
        if let Some(id) = &self.id {
            parts.push(format!("workspace_id={id}"));
        }
        parts.join(";")
    }

    fn path_component(&self) -> String {
        encode_path_component(&self.identity())
    }
}

pub fn workspace_scope(
    backend: &str,
    workspace_name: &str,
    workspace_id: Option<&str>,
) -> Result<LocalWorkspaceScope> {
    let namespace = match workspace_id {
        Some(id) => format!("backend={backend};workspace_id={id}"),
        None => format!("backend={backend};workspace={workspace_name}"),
    };
    let name = if workspace_id.is_some() {
        None
    } else {
        Some(workspace_name.to_string())
    };
    LocalWorkspaceScope::new(
        backend,
        Some(namespace),
        name,
        workspace_id.map(str::to_string),
    )
}

/// Session directory: ~/.zeroclaw/sessions/{session_id}
pub fn session_dir(session_id: &str) -> Result<PathBuf> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local storage"));
    }
    Ok(sessions_dir()?.join(session_id))
}

/// True when a session id is a single local filesystem path component.
pub fn is_safe_session_id(session_id: &str) -> bool {
    if session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains('\0')
    {
        return false;
    }

    let mut components = Path::new(session_id).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Session metadata: ~/.zeroclaw/sessions/{session_id}/meta.json
pub fn session_metadata_file(session_id: &str) -> Result<PathBuf> {
    Ok(session_dir(session_id)?.join("meta.json"))
}

/// Session history: ~/.zeroclaw/sessions/{session_id}/history.jsonl
pub fn session_history_file(session_id: &str) -> Result<PathBuf> {
    Ok(session_dir(session_id)?.join("history.jsonl"))
}

/// Workspace-scoped session directory:
/// ~/.zeroclaw/sessions/workspaces/{scope}/{session_id}
pub fn scoped_session_dir(scope: &LocalWorkspaceScope, session_id: &str) -> Result<PathBuf> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local storage"));
    }
    Ok(workspace_sessions_dir()?
        .join(scope.path_component())
        .join(session_id))
}

/// Workspace-scoped metadata file.
pub fn scoped_session_metadata_file(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(scoped_session_dir(scope, session_id)?.join("meta.json"))
}

/// Workspace-scoped session history file.
pub fn scoped_session_history_file(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(scoped_session_dir(scope, session_id)?.join("history.jsonl"))
}

/// Marker written when a transcript append fails after a backend turn
/// has already been submitted. `/save` refuses to export marked history.
pub fn scoped_session_history_incomplete_file(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(scoped_session_dir(scope, session_id)?.join("history.incomplete"))
}

/// Ensure config directory exists
pub fn ensure_config_dir() -> Result<()> {
    let dir = config_dir()?;
    create_private_dir_all(&dir)
}

/// Ensure sessions directory exists
pub fn ensure_sessions_dir() -> Result<()> {
    let dir = sessions_dir()?;
    create_private_dir_all(&dir)
}

/// Ensure session directory exists
pub fn ensure_session_dir(session_id: &str) -> Result<()> {
    let dir = session_dir(session_id)?;
    create_private_dir_all(&dir)
}

/// Ensure workspace-scoped session directory exists.
pub fn ensure_scoped_session_dir(scope: &LocalWorkspaceScope, session_id: &str) -> Result<()> {
    let dir = scoped_session_dir(scope, session_id)?;
    create_private_dir_all(&dir)
}

fn create_private_dir_all(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| anyhow!("Failed to create directory: {}", e))?;
    harden_private_dirs(dir)
}

#[cfg(unix)]
fn harden_private_dirs(dir: &Path) -> Result<()> {
    let root = config_dir()?;
    let mut dirs: Vec<PathBuf> = dir
        .ancestors()
        .filter(|path| path.starts_with(&root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();

    for path in dirs {
        if path.is_dir() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|e| anyhow!("Failed to set private directory permissions: {}", e))?;
        }
    }

    Ok(())
}

#[cfg(not(unix))]
fn harden_private_dirs(_dir: &Path) -> Result<()> {
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
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local metadata load"));
    }

    let file = session_metadata_file(session_id)?;
    let content =
        fs::read_to_string(&file).map_err(|e| anyhow!("Failed to read session metadata: {}", e))?;
    let metadata: SessionMetadata = serde_json::from_str(&content)
        .map_err(|e| anyhow!("Failed to parse session metadata: {}", e))?;
    if !is_safe_session_id(&metadata.id) {
        return Err(anyhow!("unsafe session id in local metadata"));
    }
    Ok(metadata)
}

/// Load workspace-scoped session metadata from file.
pub fn load_scoped_session_metadata(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<SessionMetadata> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local metadata load"));
    }

    let file = scoped_session_metadata_file(scope, session_id)?;
    let content =
        fs::read_to_string(&file).map_err(|e| anyhow!("Failed to read session metadata: {}", e))?;
    let metadata: SessionMetadata = serde_json::from_str(&content)
        .map_err(|e| anyhow!("Failed to parse session metadata: {}", e))?;
    if !is_safe_session_id(&metadata.id) {
        return Err(anyhow!("unsafe session id in local metadata"));
    }
    Ok(metadata)
}

/// Save session metadata to file
pub fn save_session_metadata(metadata: &SessionMetadata) -> Result<()> {
    if !is_safe_session_id(&metadata.id) {
        return Err(anyhow!("unsafe session id for local metadata save"));
    }

    ensure_session_dir(&metadata.id)?;
    let file = session_metadata_file(&metadata.id)?;
    let content = serde_json::to_string_pretty(&metadata)?;
    fs::write(&file, content).map_err(|e| anyhow!("Failed to write session metadata: {}", e))?;
    Ok(())
}

/// Save workspace-scoped session metadata to file.
pub fn save_scoped_session_metadata(
    scope: &LocalWorkspaceScope,
    metadata: &SessionMetadata,
) -> Result<()> {
    if !is_safe_session_id(&metadata.id) {
        return Err(anyhow!("unsafe session id for local metadata save"));
    }

    ensure_scoped_session_dir(scope, &metadata.id)?;
    let file = scoped_session_metadata_file(scope, &metadata.id)?;
    let content = serde_json::to_string_pretty(&metadata)?;
    fs::write(&file, content).map_err(|e| anyhow!("Failed to write session metadata: {}", e))?;
    Ok(())
}

/// Append one workspace-scoped transcript entry as JSONL.
pub fn append_scoped_session_history(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    role: &str,
    content: &str,
) -> Result<()> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history append"));
    }

    ensure_scoped_session_dir(scope, session_id)?;
    let file = scoped_session_history_file(scope, session_id)?;
    let mut out = open_private_append_file(&file)?;
    let entry = serde_json::json!({
        "role": role,
        "content": content,
    });
    writeln!(out, "{entry}").map_err(|e| anyhow!("Failed to append session history: {}", e))?;
    Ok(())
}

fn open_private_append_file(file: &Path) -> Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }

    let out = opts
        .open(file)
        .map_err(|e| anyhow!("Failed to open session history: {}", e))?;
    harden_private_file(file)?;
    Ok(out)
}

fn open_private_write_file(file: &Path) -> Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }

    let out = opts
        .open(file)
        .map_err(|e| anyhow!("Failed to open private storage file: {}", e))?;
    harden_private_file(file)?;
    Ok(out)
}

#[cfg(unix)]
fn harden_private_file(file: &Path) -> Result<()> {
    if file.exists() {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow!("Failed to set private file permissions: {}", e))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn harden_private_file(_file: &Path) -> Result<()> {
    Ok(())
}

/// Mark a transcript incomplete after a post-submit persistence failure.
pub fn mark_scoped_session_history_incomplete(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    reason: &str,
) -> Result<()> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history marker"));
    }

    ensure_scoped_session_dir(scope, session_id)?;
    let file = scoped_session_history_incomplete_file(scope, session_id)?;
    let mut out = open_private_write_file(&file)?;
    writeln!(out, "{reason}")
        .map_err(|e| anyhow!("Failed to write session history incomplete marker: {}", e))?;
    Ok(())
}

/// True when `/save` should refuse to export this scoped transcript.
pub fn scoped_session_history_is_incomplete(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<bool> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history marker check"));
    }
    Ok(scoped_session_history_incomplete_file(scope, session_id)?.exists())
}

/// Remove workspace-scoped transcript history for a session, leaving metadata intact.
pub fn clear_scoped_session_history(scope: &LocalWorkspaceScope, session_id: &str) -> Result<bool> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history clear"));
    }

    let mut removed = false;
    for file in [
        scoped_session_history_file(scope, session_id)?,
        scoped_session_history_incomplete_file(scope, session_id)?,
    ] {
        match fs::remove_file(&file) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow!("Failed to clear session history: {}", e)),
        }
    }
    Ok(removed)
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

/// List sessions for one local workspace scope.
pub fn list_scoped_sessions(scope: &LocalWorkspaceScope) -> Result<Vec<SessionMetadata>> {
    let scoped_dir = workspace_sessions_dir()?.join(scope.path_component());
    let mut sessions = Vec::new();

    if !scoped_dir.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&scoped_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if let Some(session_id) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(metadata) = load_scoped_session_metadata(scope, session_id) {
                    sessions.push(metadata);
                }
            }
        }
    }

    sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    Ok(sessions)
}

/// Delete a session and all its files
pub fn delete_session(session_id: &str) -> Result<()> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local deletion"));
    }

    let dir = session_dir(session_id)?;
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| anyhow!("Failed to delete session: {}", e))?;
    }
    Ok(())
}

/// Delete a workspace-scoped local session and all its files.
pub fn delete_scoped_session(scope: &LocalWorkspaceScope, session_id: &str) -> Result<()> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local deletion"));
    }

    let dir = scoped_session_dir(scope, session_id)?;
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

fn encode_path_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("~{byte:02X}")),
        }
    }
    out
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

    #[test]
    fn safe_session_ids_are_single_path_components() {
        assert!(is_safe_session_id("sess-123"));
        assert!(is_safe_session_id("2026.04.28_alpha"));

        assert!(!is_safe_session_id(""));
        assert!(!is_safe_session_id("."));
        assert!(!is_safe_session_id(".."));
        assert!(!is_safe_session_id("../owned"));
        assert!(!is_safe_session_id("nested/session"));
        assert!(!is_safe_session_id("nested\\session"));
    }

    fn metadata(id: &str) -> SessionMetadata {
        SessionMetadata {
            id: id.to_string(),
            name: "Review regression".to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            message_count: 0,
            last_active: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn scope(name: &str) -> LocalWorkspaceScope {
        LocalWorkspaceScope::new(
            "zeroclaw",
            Some(format!("backend=zeroclaw;workspace={name}")),
            Some(name.to_string()),
            None,
        )
        .unwrap()
    }

    #[test]
    fn unsafe_session_id_paths_fail_before_joining_sessions_dir() {
        assert!(session_dir("../owned").is_err());
        assert!(session_metadata_file("../owned").is_err());
        assert!(session_history_file("../owned").is_err());
    }

    #[test]
    fn save_and_load_reject_unsafe_session_ids() {
        let unsafe_id = format!("../zterm-storage-review-{}", uuid::Uuid::new_v4());
        let escaped_path = sessions_dir().unwrap().join(&unsafe_id);

        assert!(save_session_metadata(&metadata(&unsafe_id)).is_err());
        assert!(load_session_metadata(&unsafe_id).is_err());
        assert!(
            !escaped_path.exists(),
            "unsafe metadata write escaped the sessions directory"
        );
    }

    #[test]
    fn id_bearing_workspace_scope_survives_workspace_rename() {
        let workspace_id = format!("ws_{}", uuid::Uuid::new_v4());
        let before = workspace_scope("openclaw", "alpha", Some(&workspace_id)).unwrap();
        let after = workspace_scope("openclaw", "renamed", Some(&workspace_id)).unwrap();

        assert_eq!(before.identity(), after.identity());
        assert!(!before.identity().contains("alpha"));
        assert!(!after.identity().contains("renamed"));
        assert_eq!(
            scoped_session_history_file(&before, "main").unwrap(),
            scoped_session_history_file(&after, "main").unwrap()
        );
    }

    #[test]
    fn append_scoped_session_history_writes_transcript_entries() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("history-{}", uuid::Uuid::new_v4()));

        append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        append_scoped_session_history(&scope, "main", "assistant", "hi there").unwrap();

        let history =
            fs::read_to_string(scoped_session_history_file(&scope, "main").unwrap()).unwrap();
        assert!(history.contains(r#""role":"user""#));
        assert!(history.contains(r#""content":"hello""#));
        assert!(history.contains(r#""role":"assistant""#));
        assert!(history.contains(r#""content":"hi there""#));

        assert!(clear_scoped_session_history(&scope, "main").unwrap());
        assert!(!scoped_session_history_file(&scope, "main")
            .unwrap()
            .exists());
        assert!(!clear_scoped_session_history(&scope, "main").unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn scoped_history_uses_private_unix_modes() {
        use std::os::unix::fs::PermissionsExt;

        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("private-{}", uuid::Uuid::new_v4()));

        append_scoped_session_history(&scope, "main", "user", "secret").unwrap();

        let history = scoped_session_history_file(&scope, "main").unwrap();
        assert_eq!(
            fs::metadata(&history).unwrap().permissions().mode() & 0o777,
            0o600
        );

        for dir in [
            config_dir().unwrap(),
            sessions_dir().unwrap(),
            workspace_sessions_dir().unwrap(),
            workspace_sessions_dir()
                .unwrap()
                .join(scope.path_component()),
            scoped_session_dir(&scope, "main").unwrap(),
        ] {
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                dir.display()
            );
        }

        mark_scoped_session_history_incomplete(&scope, "main", "append failed").unwrap();
        let marker = scoped_session_history_incomplete_file(&scope, "main").unwrap();
        assert_eq!(
            fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn clear_scoped_history_removes_incomplete_marker() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("incomplete-{}", uuid::Uuid::new_v4()));

        append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        mark_scoped_session_history_incomplete(&scope, "main", "assistant append failed").unwrap();

        assert!(scoped_session_history_is_incomplete(&scope, "main").unwrap());
        assert!(clear_scoped_session_history(&scope, "main").unwrap());
        assert!(!scoped_session_history_file(&scope, "main")
            .unwrap()
            .exists());
        assert!(!scoped_session_history_incomplete_file(&scope, "main")
            .unwrap()
            .exists());
        assert!(!scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn scoped_metadata_history_clear_and_delete_do_not_cross_contaminate() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let suffix = uuid::Uuid::new_v4();
        let alpha = scope(&format!("alpha-{suffix}"));
        let beta = scope(&format!("beta-{suffix}"));
        let mut alpha_meta = metadata("main");
        alpha_meta.name = "Alpha main".to_string();
        alpha_meta.message_count = 3;
        let mut beta_meta = metadata("main");
        beta_meta.name = "Beta main".to_string();
        beta_meta.message_count = 9;
        let alpha_history = scoped_session_history_file(&alpha, "main").unwrap();
        let beta_history = scoped_session_history_file(&beta, "main").unwrap();

        save_scoped_session_metadata(&alpha, &alpha_meta).unwrap();
        save_scoped_session_metadata(&beta, &beta_meta).unwrap();
        fs::write(&alpha_history, "alpha history\n").unwrap();
        fs::write(&beta_history, "beta history\n").unwrap();

        let mut cleared_alpha = load_scoped_session_metadata(&alpha, "main").unwrap();
        cleared_alpha.message_count = 0;
        save_scoped_session_metadata(&alpha, &cleared_alpha).unwrap();

        assert_eq!(
            load_scoped_session_metadata(&alpha, "main")
                .unwrap()
                .message_count,
            0
        );
        assert_eq!(
            load_scoped_session_metadata(&beta, "main")
                .unwrap()
                .message_count,
            9
        );
        assert_eq!(
            fs::read_to_string(&alpha_history).unwrap(),
            "alpha history\n"
        );
        assert_eq!(fs::read_to_string(&beta_history).unwrap(), "beta history\n");

        delete_scoped_session(&alpha, "main").unwrap();

        assert!(load_scoped_session_metadata(&alpha, "main").is_err());
        assert!(!alpha_history.exists());
        assert_eq!(
            load_scoped_session_metadata(&beta, "main")
                .unwrap()
                .message_count,
            9
        );
        assert_eq!(fs::read_to_string(&beta_history).unwrap(), "beta history\n");

        delete_scoped_session(&beta, "main").unwrap();
    }
}
