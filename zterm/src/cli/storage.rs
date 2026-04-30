use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Directory of per-turn pending transcript markers.
pub fn scoped_session_history_pending_dir(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(scoped_session_dir(scope, session_id)?.join("history.incomplete.d"))
}

/// Cross-process lock held while a turn is being submitted and its
/// terminal transcript entry is persisted.
pub fn scoped_session_history_turn_lock_dir(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(scoped_session_dir(scope, session_id)?.join("history.turn.lock"))
}

/// One pending marker owned by a single submitted turn.
pub fn scoped_session_history_pending_marker_file(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    marker_id: &str,
) -> Result<PathBuf> {
    if !is_safe_session_id(marker_id) {
        return Err(anyhow!("unsafe turn marker id for local history marker"));
    }
    Ok(scoped_session_history_pending_dir(scope, session_id)?.join(marker_id))
}

#[derive(Debug)]
#[must_use]
pub struct ScopedSessionTurnLock {
    lock_dir: PathBuf,
}

impl ScopedSessionTurnLock {
    pub fn release(&self) -> Result<bool> {
        release_scoped_session_turn_lock_path(&self.lock_dir)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionTurnLockOwner {
    pid: u32,
    created_at_unix: u64,
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
    create_private_dir_chain(dir)?;
    harden_private_dirs(dir)
}

#[cfg(unix)]
fn create_private_dir_chain(dir: &Path) -> Result<()> {
    let dirs = private_dir_chain(dir)?;
    for path in dirs {
        match fs::symlink_metadata(&path) {
            Ok(metadata) => harden_private_dir_metadata(&path, &metadata)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(create_err) => {
                        return Err(anyhow!(
                            "Failed to create private directory {}: {}",
                            path.display(),
                            create_err
                        ));
                    }
                }
                let metadata = fs::symlink_metadata(&path).map_err(|inspect_err| {
                    anyhow!(
                        "Failed to inspect private directory {}: {}",
                        path.display(),
                        inspect_err
                    )
                })?;
                harden_private_dir_metadata(&path, &metadata)?;
            }
            Err(e) => {
                return Err(anyhow!(
                    "Failed to inspect private directory {}: {}",
                    path.display(),
                    e
                ));
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir_chain(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| anyhow!("Failed to create directory: {}", e))
}

#[cfg(unix)]
fn harden_private_dirs(dir: &Path) -> Result<()> {
    for path in private_dir_chain(dir)? {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| anyhow!("Failed to inspect private directory: {}", e))?;
        harden_private_dir_metadata(&path, &metadata)?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn harden_private_dirs(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn private_dir_chain(dir: &Path) -> Result<Vec<PathBuf>> {
    let root = config_dir()?;
    let mut dirs: Vec<PathBuf> = dir
        .ancestors()
        .filter(|path| path.starts_with(&root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    Ok(dirs)
}

#[cfg(unix)]
fn harden_private_dir_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "Refusing to use symlinked private directory: {}",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(anyhow!(
            "Refusing to use non-directory private path: {}",
            path.display()
        ));
    }
    if metadata.uid() != current_euid() {
        return Err(anyhow!(
            "Refusing to use private directory {} owned by uid {}",
            path.display(),
            metadata.uid()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|e| anyhow!("Failed to set private directory permissions: {}", e))?;
    Ok(())
}

#[cfg(unix)]
fn current_euid() -> u32 {
    unsafe { libc::geteuid() as u32 }
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
    write_private_storage_file(&file, &content, "session metadata")?;
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
    write_private_storage_file(&file, &content, "session metadata")?;
    Ok(())
}

/// Append one workspace-scoped transcript entry as JSONL.
pub fn append_scoped_session_history(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    role: &str,
    content: &str,
) -> Result<()> {
    append_scoped_session_history_with_sync(scope, session_id, role, content, sync_parent_dir)
}

fn append_scoped_session_history_with_sync<F>(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    role: &str,
    content: &str,
    mut sync_new_file_parent: F,
) -> Result<()>
where
    F: FnMut(&Path) -> std::io::Result<()>,
{
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history append"));
    }

    ensure_scoped_session_dir(scope, session_id)?;
    let file = scoped_session_history_file(scope, session_id)?;
    let created = !file.exists();
    let mut out = open_private_append_file(&file)?;
    let entry = serde_json::json!({
        "role": role,
        "content": content,
    });
    writeln!(out, "{entry}").map_err(|e| anyhow!("Failed to append session history: {}", e))?;
    out.sync_all()
        .map_err(|e| anyhow!("Failed to sync session history: {}", e))?;
    if created {
        sync_new_file_parent(&file).map_err(|e| {
            anyhow!(
                "Failed to sync session history directory after create: {}",
                e
            )
        })?;
    }
    Ok(())
}

fn open_private_append_file(file: &Path) -> Result<fs::File> {
    ensure_private_file_leaf_safe(file, "append")?;
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
    }

    let out = opts
        .open(file)
        .map_err(|e| anyhow!("Failed to open session history: {}", e))?;
    harden_private_open_file(file, &out)?;
    Ok(out)
}

fn open_private_write_file(file: &Path) -> Result<fs::File> {
    ensure_private_file_leaf_safe(file, "write")?;
    let mut opts = fs::OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
    }

    let out = opts
        .open(file)
        .map_err(|e| anyhow!("Failed to open private storage file: {}", e))?;
    harden_private_open_file(file, &out)?;
    Ok(out)
}

fn write_private_storage_file(file: &Path, content: &str, label: &str) -> Result<()> {
    let mut out = open_private_write_file(file)?;
    out.write_all(content.as_bytes())
        .map_err(|e| anyhow!("Failed to write {label}: {}", e))?;
    out.sync_all()
        .map_err(|e| anyhow!("Failed to sync {label}: {}", e))?;
    Ok(())
}

fn ensure_private_file_leaf_safe(file: &Path, operation: &str) -> Result<()> {
    if let Some(parent) = file.parent() {
        harden_private_dirs(parent)?;
    }
    match fs::symlink_metadata(file) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "Refusing to {operation} private storage symlink: {}",
            file.display()
        )),
        Ok(metadata) if metadata.is_dir() => Err(anyhow!(
            "Refusing to {operation} private storage directory as file: {}",
            file.display()
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!("Failed to inspect private storage file: {}", e)),
    }
}

#[cfg(unix)]
fn harden_private_open_file(file: &Path, out: &fs::File) -> Result<()> {
    match fs::symlink_metadata(file) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(anyhow!(
                "Refusing to harden private storage symlink: {}",
                file.display()
            ));
        }
        Ok(metadata) if metadata.is_dir() => {
            return Err(anyhow!(
                "Refusing to harden private storage directory as file: {}",
                file.display()
            ));
        }
        Ok(_) => {}
        Err(e) => return Err(anyhow!("Failed to inspect private storage file: {}", e)),
    }
    out.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|e| anyhow!("Failed to set private file permissions: {}", e))?;
    Ok(())
}

#[cfg(not(unix))]
fn harden_private_open_file(_file: &Path, _out: &fs::File) -> Result<()> {
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
    out.sync_all()
        .map_err(|e| anyhow!("Failed to sync session history incomplete marker: {}", e))?;
    sync_parent_dir(&file).map_err(|e| {
        anyhow!(
            "Failed to sync session history incomplete marker directory: {}",
            e
        )
    })?;
    Ok(())
}

/// Mark a submitted turn whose terminal transcript entry is not durable yet.
pub fn mark_scoped_session_history_pending_turn(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    marker_id: &str,
    reason: &str,
) -> Result<()> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history marker"));
    }
    if !is_safe_session_id(marker_id) {
        return Err(anyhow!("unsafe turn marker id for local history marker"));
    }

    ensure_scoped_session_dir(scope, session_id)?;
    let dir = scoped_session_history_pending_dir(scope, session_id)?;
    create_private_dir_all(&dir)?;
    let file = scoped_session_history_pending_marker_file(scope, session_id, marker_id)?;
    let mut out = open_private_write_file(&file)?;
    writeln!(out, "{reason}")
        .map_err(|e| anyhow!("Failed to write session history pending marker: {}", e))?;
    out.sync_all()
        .map_err(|e| anyhow!("Failed to sync session history pending marker: {}", e))?;
    sync_parent_dir(&file).map_err(|e| {
        anyhow!(
            "Failed to sync session history pending marker directory: {}",
            e
        )
    })?;
    Ok(())
}

/// Atomically acquire a cross-process turn lock and write this turn's
/// pending marker while the lock is held.
pub fn acquire_scoped_session_history_turn_lock(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    marker_id: &str,
    reason: &str,
) -> Result<ScopedSessionTurnLock> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history lock"));
    }
    if !is_safe_session_id(marker_id) {
        return Err(anyhow!("unsafe turn marker id for local history lock"));
    }

    let lock_dir = acquire_scoped_session_history_lock_dir(
        scope,
        session_id,
        "turn",
        &format!(
            "session `{session_id}` already has a turn in progress; wait for it to finish or run /clear --force if another zterm exited mid-turn"
        ),
    )?;
    if let Err(e) = write_session_turn_lock_owner(&lock_dir, &current_session_turn_lock_owner()) {
        let _ = release_scoped_session_turn_lock_path(&lock_dir);
        return Err(e);
    }

    if let Err(e) = ensure_scoped_session_history_complete(scope, session_id) {
        let _ = release_scoped_session_turn_lock_path(&lock_dir);
        return Err(e);
    }

    if let Err(e) = mark_scoped_session_history_pending_turn(scope, session_id, marker_id, reason) {
        let _ = release_scoped_session_turn_lock_path(&lock_dir);
        return Err(e);
    }

    Ok(ScopedSessionTurnLock { lock_dir })
}

fn acquire_scoped_session_history_lock_dir(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    operation: &str,
    already_exists_message: &str,
) -> Result<PathBuf> {
    ensure_scoped_session_dir(scope, session_id)?;
    let lock_dir = scoped_session_history_turn_lock_dir(scope, session_id)?;
    match fs::create_dir(&lock_dir) {
        Ok(()) => {
            harden_private_dirs(&lock_dir)?;
            sync_parent_dir(&lock_dir).map_err(|e| {
                anyhow!("Failed to sync session history {operation} lock directory: {e}")
            })?;
            Ok(lock_dir)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(anyhow!("{already_exists_message}"))
        }
        Err(e) => Err(anyhow!(
            "Failed to acquire session history {operation} lock: {e}"
        )),
    }
}

fn current_session_turn_lock_owner() -> SessionTurnLockOwner {
    SessionTurnLockOwner {
        pid: std::process::id(),
        created_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
    }
}

fn session_turn_lock_owner_file(lock_dir: &Path) -> PathBuf {
    lock_dir.join("owner.json")
}

fn write_session_turn_lock_owner(lock_dir: &Path, owner: &SessionTurnLockOwner) -> Result<()> {
    let file = session_turn_lock_owner_file(lock_dir);
    let mut out = open_private_write_file(&file)?;
    let content = serde_json::to_string(owner)
        .map_err(|e| anyhow!("Failed to serialize session history turn lock owner: {e}"))?;
    writeln!(out, "{content}")
        .map_err(|e| anyhow!("Failed to write session history turn lock owner: {e}"))?;
    out.sync_all()
        .map_err(|e| anyhow!("Failed to sync session history turn lock owner: {e}"))?;
    sync_parent_dir(&file)
        .map_err(|e| anyhow!("Failed to sync session history turn lock owner directory: {e}"))?;
    Ok(())
}

fn read_session_turn_lock_owner(lock_dir: &Path) -> Result<SessionTurnLockOwner> {
    let file = session_turn_lock_owner_file(lock_dir);
    let content = fs::read_to_string(&file)
        .map_err(|e| anyhow!("Failed to read session history turn lock owner: {e}"))?;
    serde_json::from_str(&content)
        .map_err(|e| anyhow!("Failed to parse session history turn lock owner: {e}"))
}

fn session_turn_lock_owner_is_live(lock_dir: &Path) -> Result<bool> {
    let owner = read_session_turn_lock_owner(lock_dir)?;
    process_id_is_live(owner.pid)
}

#[cfg(unix)]
fn process_id_is_live(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }
    if pid == std::process::id() {
        return Ok(true);
    }
    let status = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| anyhow!("Failed to check session history turn lock owner process: {e}"))?;
    Ok(status.success())
}

#[cfg(not(unix))]
fn process_id_is_live(pid: u32) -> Result<bool> {
    if pid == std::process::id() {
        Ok(true)
    } else {
        Err(anyhow!(
            "cannot prove session history turn lock owner process is stale on this platform"
        ))
    }
}

#[cfg(test)]
pub(crate) fn write_stale_scoped_session_history_turn_lock_owner_for_tests(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<()> {
    let lock_dir = scoped_session_history_turn_lock_dir(scope, session_id)?;
    write_session_turn_lock_owner(
        &lock_dir,
        &SessionTurnLockOwner {
            pid: 999_999_999,
            created_at_unix: 0,
        },
    )
}

/// True when `/save` should refuse to export this scoped transcript.
pub fn scoped_session_history_is_incomplete(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<bool> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history marker check"));
    }
    if scoped_session_history_incomplete_file(scope, session_id)?.exists() {
        return Ok(true);
    }

    let dir = scoped_session_history_pending_dir(scope, session_id)?;
    match fs::read_dir(&dir) {
        Ok(mut entries) => Ok(entries.next().transpose()?.is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow!(
            "Failed to read session history pending marker directory: {}",
            e
        )),
    }
}

/// Refuse new turns while the local transcript is known incomplete.
pub fn ensure_scoped_session_history_complete(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<()> {
    if scoped_session_history_is_incomplete(scope, session_id)? {
        return Err(anyhow!(
            "session `{session_id}` has an incomplete transcript; run /clear before submitting another turn"
        ));
    }
    Ok(())
}

/// Clear only the incomplete marker for a scoped transcript.
pub fn clear_scoped_session_history_incomplete_marker(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<bool> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history marker clear"));
    }

    let file = scoped_session_history_incomplete_file(scope, session_id)?;
    let mut removed = match fs::remove_file(&file) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(anyhow!(
                "Failed to clear session history incomplete marker: {}",
                e
            ))
        }
    };

    let pending_dir = scoped_session_history_pending_dir(scope, session_id)?;
    match fs::remove_dir_all(&pending_dir) {
        Ok(()) => removed = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow!(
                "Failed to clear session history pending marker directory: {}",
                e
            ))
        }
    }

    if removed {
        sync_parent_dir(&file).map_err(|e| {
            anyhow!(
                "Failed to sync session history incomplete marker directory after clear: {}",
                e
            )
        })?;
    }
    Ok(removed)
}

/// Clear one pending transcript marker, leaving any other submitted turn markers intact.
pub fn clear_scoped_session_history_pending_turn_marker(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    marker_id: &str,
) -> Result<bool> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history marker clear"));
    }
    if !is_safe_session_id(marker_id) {
        return Err(anyhow!(
            "unsafe turn marker id for local history marker clear"
        ));
    }

    let file = scoped_session_history_pending_marker_file(scope, session_id, marker_id)?;
    let removed = match fs::remove_file(&file) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(anyhow!(
                "Failed to clear session history pending marker: {}",
                e
            ))
        }
    };
    if removed {
        sync_parent_dir(&file).map_err(|e| {
            anyhow!(
                "Failed to sync session history pending marker directory after clear: {}",
                e
            )
        })?;
        let dir = scoped_session_history_pending_dir(scope, session_id)?;
        match fs::remove_dir(&dir) {
            Ok(()) => {
                sync_parent_dir(&dir).map_err(|e| {
                    anyhow!(
                        "Failed to sync session history pending marker parent after clear: {}",
                        e
                    )
                })?;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(e) => {
                return Err(anyhow!(
                    "Failed to remove empty session history pending marker directory: {}",
                    e
                ))
            }
        }
    }
    Ok(removed)
}

fn release_scoped_session_turn_lock_path(lock_dir: &Path) -> Result<bool> {
    let owner_file = session_turn_lock_owner_file(lock_dir);
    match fs::remove_file(&owner_file) {
        Ok(()) => {
            sync_parent_dir(&owner_file).map_err(|e| {
                anyhow!(
                    "Failed to sync session history turn lock owner parent after release: {}",
                    e
                )
            })?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow!(
                "Failed to release session history turn lock owner: {}",
                e
            ))
        }
    }
    match fs::remove_dir(lock_dir) {
        Ok(()) => {
            sync_parent_dir(lock_dir).map_err(|e| {
                anyhow!(
                    "Failed to sync session history turn lock parent after release: {}",
                    e
                )
            })?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow!(
            "Failed to release session history turn lock: {}",
            e
        )),
    }
}

/// Remove workspace-scoped transcript history for a session, leaving metadata intact.
pub fn clear_scoped_session_history(scope: &LocalWorkspaceScope, session_id: &str) -> Result<bool> {
    clear_scoped_session_history_with_sync(scope, session_id, sync_parent_dir)
}

pub fn force_clear_scoped_session_history(
    scope: &LocalWorkspaceScope,
    session_id: &str,
) -> Result<bool> {
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history clear"));
    }

    let lock_dir = scoped_session_history_turn_lock_dir(scope, session_id)?;
    match fs::metadata(&lock_dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(anyhow!(
                    "session history turn lock path is not a directory; refusing force clear"
                ));
            }
            if session_turn_lock_owner_is_live(&lock_dir)? {
                return Err(anyhow!(
                    "session `{session_id}` has a live turn lock; refusing /clear --force"
                ));
            }
            let clear_result =
                clear_scoped_session_history_files_with_sync(scope, session_id, sync_parent_dir);
            let release_result = release_scoped_session_turn_lock_path(&lock_dir);
            match (clear_result, release_result) {
                (Ok(_), Ok(_)) => Ok(true),
                (Ok(_), Err(release_err)) => Err(release_err),
                (Err(clear_err), Ok(_)) => Err(clear_err),
                (Err(clear_err), Err(release_err)) => Err(anyhow!(
                    "{clear_err}; additionally failed to release stale session history turn lock: {release_err}"
                )),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            clear_scoped_session_history(scope, session_id)
        }
        Err(e) => Err(anyhow!("Failed to inspect session history turn lock: {e}")),
    }
}

fn clear_scoped_session_history_with_sync<F>(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    sync_removed_path_parent: F,
) -> Result<bool>
where
    F: FnMut(&Path) -> std::io::Result<()>,
{
    if !is_safe_session_id(session_id) {
        return Err(anyhow!("unsafe session id for local history clear"));
    }

    let lock_dir = acquire_scoped_session_history_lock_dir(
        scope,
        session_id,
        "clear",
        &format!(
            "session `{session_id}` has a turn in progress; /clear refused to preserve transcript lock; use /clear --force only if another zterm exited mid-turn"
        ),
    )?;

    let clear_result =
        clear_scoped_session_history_files_with_sync(scope, session_id, sync_removed_path_parent);

    let release_result = release_scoped_session_turn_lock_path(&lock_dir);
    match (clear_result, release_result) {
        (Ok(removed), Ok(_)) => Ok(removed),
        (Ok(_), Err(release_err)) => Err(release_err),
        (Err(clear_err), Ok(_)) => Err(clear_err),
        (Err(clear_err), Err(release_err)) => Err(anyhow!(
            "{clear_err}; additionally failed to release session history clear lock: {release_err}"
        )),
    }
}

fn clear_scoped_session_history_files_with_sync<F>(
    scope: &LocalWorkspaceScope,
    session_id: &str,
    sync_removed_path_parent: F,
) -> Result<bool>
where
    F: FnMut(&Path) -> std::io::Result<()>,
{
    let mut sync_removed_path_parent = sync_removed_path_parent;
    let mut removed = false;
    for file in [
        scoped_session_history_file(scope, session_id)?,
        scoped_session_history_incomplete_file(scope, session_id)?,
    ] {
        match fs::remove_file(&file) {
            Ok(()) => {
                removed = true;
                sync_removed_path_parent(&file).map_err(|e| {
                    anyhow!(
                        "Failed to sync session history directory after clear: {}",
                        e
                    )
                })?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow!("Failed to clear session history: {}", e)),
        }
    }
    let pending_dir = scoped_session_history_pending_dir(scope, session_id)?;
    match fs::read_dir(&pending_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry
                    .map_err(|e| anyhow!("Failed to read session history pending marker: {}", e))?;
                let path = entry.path();
                let file_type = entry.file_type().map_err(|e| {
                    anyhow!("Failed to inspect session history pending marker: {}", e)
                })?;
                if file_type.is_dir() {
                    return Err(anyhow!(
                        "Failed to clear session history: unexpected nested pending marker directory"
                    ));
                }
                match fs::remove_file(&path) {
                    Ok(()) => {
                        removed = true;
                        sync_removed_path_parent(&path).map_err(|e| {
                            anyhow!(
                                "Failed to sync session history pending marker directory after clear: {}",
                                e
                            )
                        })?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(anyhow!("Failed to clear session history: {}", e)),
                }
            }
            match fs::remove_dir(&pending_dir) {
                Ok(()) => {
                    removed = true;
                    sync_removed_path_parent(&pending_dir).map_err(|e| {
                        anyhow!(
                            "Failed to sync session history pending marker parent after clear: {}",
                            e
                        )
                    })?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(anyhow!("Failed to clear session history: {}", e)),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow!("Failed to clear session history: {}", e)),
    }
    Ok(removed)
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
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
    write_private_storage_file(&file, content, "config")?;
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

    write_private_storage_file(&file, &content, "config")?;

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

    #[cfg(unix)]
    #[test]
    fn append_scoped_session_history_refuses_symlink_leaf() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let scope = scope(&format!("history-symlink-{}", uuid::Uuid::new_v4()));
        let target = home.path().join("outside-history.jsonl");
        fs::write(&target, "original\n").unwrap();

        ensure_scoped_session_dir(&scope, "main").unwrap();
        let history = scoped_session_history_file(&scope, "main").unwrap();
        std::os::unix::fs::symlink(&target, &history).unwrap();

        let err =
            append_scoped_session_history(&scope, "main", "user", "must not write").unwrap_err();

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        assert!(err.to_string().contains("symlink"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
    }

    #[cfg(unix)]
    #[test]
    fn pending_turn_marker_refuses_symlink_leaf() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let scope = scope(&format!("pending-symlink-{}", uuid::Uuid::new_v4()));
        let target = home.path().join("outside-pending-marker");
        fs::write(&target, "original\n").unwrap();

        let pending_dir = scoped_session_history_pending_dir(&scope, "main").unwrap();
        fs::create_dir_all(&pending_dir).unwrap();
        let marker = scoped_session_history_pending_marker_file(&scope, "main", "turn-a").unwrap();
        std::os::unix::fs::symlink(&target, &marker).unwrap();

        let err = mark_scoped_session_history_pending_turn(&scope, "main", "turn-a", "pending")
            .unwrap_err();

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        assert!(err.to_string().contains("symlink"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
    }

    #[cfg(unix)]
    #[test]
    fn save_scoped_session_metadata_refuses_symlink_leaf() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let scope = scope(&format!("metadata-symlink-{}", uuid::Uuid::new_v4()));
        let target = home.path().join("outside-metadata.json");
        fs::write(&target, "original\n").unwrap();

        ensure_scoped_session_dir(&scope, "main").unwrap();
        let metadata_file = scoped_session_metadata_file(&scope, "main").unwrap();
        std::os::unix::fs::symlink(&target, &metadata_file).unwrap();

        let err = save_scoped_session_metadata(&scope, &metadata("main")).unwrap_err();

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        assert!(err.to_string().contains("symlink"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_session_dir_rejects_symlinked_config_root_before_descent() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let root = config_dir().unwrap();
        let target = home.path().join("shared-root");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &root).unwrap();

        let err = ensure_session_dir("main").unwrap_err();

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        assert!(err.to_string().contains("symlinked private directory"));
        assert!(
            !target.join("sessions").exists(),
            "session directory creation should not descend through symlinked config root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_session_dir_rejects_symlinked_sessions_root_before_descent() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        ensure_config_dir().unwrap();
        let sessions = sessions_dir().unwrap();
        let target = home.path().join("shared-sessions");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &sessions).unwrap();

        let err = ensure_session_dir("main").unwrap_err();

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        assert!(err.to_string().contains("symlinked private directory"));
        assert!(
            !target.join("main").exists(),
            "session directory creation should not descend through symlinked sessions root"
        );
    }

    #[test]
    fn append_scoped_session_history_requires_parent_sync_after_first_create() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("history-create-sync-{}", uuid::Uuid::new_v4()));
        let synced = std::cell::RefCell::new(Vec::new());

        let err =
            append_scoped_session_history_with_sync(&scope, "main", "user", "hello", |path| {
                synced.borrow_mut().push(path.to_path_buf());
                Err(std::io::Error::other("injected parent sync failure"))
            })
            .unwrap_err();

        assert!(err.to_string().contains("injected parent sync failure"));
        assert_eq!(
            synced.borrow().as_slice(),
            [scoped_session_history_file(&scope, "main").unwrap()]
        );
        assert!(
            scoped_session_history_file(&scope, "main")
                .unwrap()
                .exists(),
            "the append wrote the file, but the caller must see the parent sync failure"
        );
    }

    #[test]
    fn append_scoped_session_history_skips_parent_sync_for_existing_file() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("history-existing-sync-{}", uuid::Uuid::new_v4()));

        append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        append_scoped_session_history_with_sync(&scope, "main", "assistant", "hi", |_path| {
            Err(std::io::Error::other(
                "parent sync should not run for existing history file",
            ))
        })
        .unwrap();

        let history =
            fs::read_to_string(scoped_session_history_file(&scope, "main").unwrap()).unwrap();
        assert!(history.contains(r#""content":"hello""#));
        assert!(history.contains(r#""content":"hi""#));
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
        mark_scoped_session_history_pending_turn(&scope, "main", "turn-a", "pending").unwrap();
        let pending_marker =
            scoped_session_history_pending_marker_file(&scope, "main", "turn-a").unwrap();
        assert_eq!(
            fs::metadata(&pending_marker).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(scoped_session_history_pending_dir(&scope, "main").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
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
        assert!(!scoped_session_history_pending_dir(&scope, "main")
            .unwrap()
            .exists());
        assert!(!scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn clear_scoped_history_syncs_each_removed_transcript_path() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let scope = scope(&format!("durable-clear-{}", uuid::Uuid::new_v4()));

        append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        mark_scoped_session_history_incomplete(&scope, "main", "assistant append failed").unwrap();
        mark_scoped_session_history_pending_turn(&scope, "main", "turn-a", "turn A pending")
            .unwrap();
        mark_scoped_session_history_pending_turn(&scope, "main", "turn-b", "turn B pending")
            .unwrap();

        let history = scoped_session_history_file(&scope, "main").unwrap();
        let incomplete = scoped_session_history_incomplete_file(&scope, "main").unwrap();
        let pending_a =
            scoped_session_history_pending_marker_file(&scope, "main", "turn-a").unwrap();
        let pending_b =
            scoped_session_history_pending_marker_file(&scope, "main", "turn-b").unwrap();
        let pending_dir = scoped_session_history_pending_dir(&scope, "main").unwrap();
        let synced = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let synced_for_clear = std::rc::Rc::clone(&synced);

        assert!(
            clear_scoped_session_history_with_sync(&scope, "main", move |path| {
                synced_for_clear.borrow_mut().push(path.to_path_buf());
                Ok(())
            })
            .unwrap()
        );

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        let synced = synced.borrow();
        assert!(synced.contains(&history));
        assert!(synced.contains(&incomplete));
        assert!(synced.contains(&pending_a));
        assert!(synced.contains(&pending_b));
        assert!(synced.contains(&pending_dir));
        assert_eq!(synced.len(), 5);
    }

    #[test]
    fn clear_scoped_history_holds_turn_lock_during_removal() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("clear-serialized-{}", uuid::Uuid::new_v4()));

        append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        let checked = std::rc::Rc::new(std::cell::RefCell::new(false));
        let checked_for_clear = std::rc::Rc::clone(&checked);
        let scope_for_clear = scope.clone();

        assert!(
            clear_scoped_session_history_with_sync(&scope, "main", move |_path| {
                if !*checked_for_clear.borrow() {
                    *checked_for_clear.borrow_mut() = true;
                    assert!(
                        scoped_session_history_turn_lock_dir(&scope_for_clear, "main")
                            .unwrap()
                            .exists()
                    );
                    let err = acquire_scoped_session_history_turn_lock(
                        &scope_for_clear,
                        "main",
                        "turn-race",
                        "turn race pending",
                    )
                    .unwrap_err();
                    assert!(err.to_string().contains("turn in progress"));
                    assert!(!scoped_session_history_pending_marker_file(
                        &scope_for_clear,
                        "main",
                        "turn-race",
                    )
                    .unwrap()
                    .exists());
                }
                Ok(())
            })
            .unwrap()
        );

        assert!(*checked.borrow());
        assert!(!scoped_session_history_turn_lock_dir(&scope, "main")
            .unwrap()
            .exists());
    }

    #[test]
    fn clear_scoped_incomplete_marker_leaves_transcript_history() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("pending-clear-{}", uuid::Uuid::new_v4()));

        append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        mark_scoped_session_history_incomplete(&scope, "main", "turn pending").unwrap();
        mark_scoped_session_history_pending_turn(&scope, "main", "turn-a", "turn pending").unwrap();

        assert!(scoped_session_history_is_incomplete(&scope, "main").unwrap());
        assert!(clear_scoped_session_history_incomplete_marker(&scope, "main").unwrap());
        assert!(!scoped_session_history_is_incomplete(&scope, "main").unwrap());
        assert!(!scoped_session_history_pending_dir(&scope, "main")
            .unwrap()
            .exists());
        let history =
            fs::read_to_string(scoped_session_history_file(&scope, "main").unwrap()).unwrap();
        assert!(history.contains(r#""content":"hello""#));
        assert!(!clear_scoped_session_history_incomplete_marker(&scope, "main").unwrap());
    }

    #[test]
    fn pending_turn_marker_clear_is_owner_scoped() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("pending-owner-{}", uuid::Uuid::new_v4()));

        mark_scoped_session_history_pending_turn(&scope, "main", "turn-a", "turn A pending")
            .unwrap();
        mark_scoped_session_history_pending_turn(&scope, "main", "turn-b", "turn B pending")
            .unwrap();

        assert!(scoped_session_history_is_incomplete(&scope, "main").unwrap());
        assert!(
            clear_scoped_session_history_pending_turn_marker(&scope, "main", "turn-a").unwrap()
        );
        assert!(
            !scoped_session_history_pending_marker_file(&scope, "main", "turn-a")
                .unwrap()
                .exists()
        );
        assert!(
            scoped_session_history_pending_marker_file(&scope, "main", "turn-b")
                .unwrap()
                .exists()
        );
        assert!(scoped_session_history_is_incomplete(&scope, "main").unwrap());

        assert!(
            !clear_scoped_session_history_pending_turn_marker(&scope, "main", "turn-a").unwrap()
        );
        assert!(
            clear_scoped_session_history_pending_turn_marker(&scope, "main", "turn-b").unwrap()
        );
        assert!(!scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn turn_lock_serializes_pending_marker_acquisition() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("turn-lock-{}", uuid::Uuid::new_v4()));

        let lock =
            acquire_scoped_session_history_turn_lock(&scope, "main", "turn-a", "turn A pending")
                .unwrap();
        assert!(scoped_session_history_is_incomplete(&scope, "main").unwrap());
        assert!(scoped_session_history_turn_lock_dir(&scope, "main")
            .unwrap()
            .exists());

        let err =
            acquire_scoped_session_history_turn_lock(&scope, "main", "turn-b", "turn B pending")
                .unwrap_err();
        assert!(err.to_string().contains("turn in progress"));

        lock.release().unwrap();
        clear_scoped_session_history_pending_turn_marker(&scope, "main", "turn-a").unwrap();
        acquire_scoped_session_history_turn_lock(&scope, "main", "turn-b", "turn B pending")
            .unwrap()
            .release()
            .unwrap();
    }

    #[test]
    fn clear_scoped_history_refuses_live_turn_lock() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("clear-turn-lock-{}", uuid::Uuid::new_v4()));

        let lock =
            acquire_scoped_session_history_turn_lock(&scope, "main", "turn-a", "turn A pending")
                .unwrap();
        assert!(scoped_session_history_turn_lock_dir(&scope, "main")
            .unwrap()
            .exists());

        let err = clear_scoped_session_history(&scope, "main").unwrap_err();
        assert!(err.to_string().contains("turn in progress"));
        assert!(scoped_session_history_turn_lock_dir(&scope, "main")
            .unwrap()
            .exists());
        assert!(scoped_session_history_is_incomplete(&scope, "main").unwrap());

        assert!(lock.release().unwrap());
        assert!(clear_scoped_session_history(&scope, "main").unwrap());
        assert!(!scoped_session_history_is_incomplete(&scope, "main").unwrap());
    }

    #[test]
    fn force_clear_scoped_history_removes_stale_turn_lock_and_pending_markers() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("force-clear-turn-lock-{}", uuid::Uuid::new_v4()));
        append_scoped_session_history(&scope, "main", "user", "line").unwrap();
        let lock =
            acquire_scoped_session_history_turn_lock(&scope, "main", "turn-a", "turn A pending")
                .unwrap();
        mark_scoped_session_history_incomplete(&scope, "main", "incomplete").unwrap();
        std::mem::forget(lock);
        write_stale_scoped_session_history_turn_lock_owner_for_tests(&scope, "main").unwrap();

        assert!(scoped_session_history_turn_lock_dir(&scope, "main")
            .unwrap()
            .exists());
        assert!(force_clear_scoped_session_history(&scope, "main").unwrap());

        assert!(!scoped_session_history_turn_lock_dir(&scope, "main")
            .unwrap()
            .exists());
        assert!(!scoped_session_history_file(&scope, "main")
            .unwrap()
            .exists());
        assert!(!scoped_session_history_incomplete_file(&scope, "main")
            .unwrap()
            .exists());
        assert!(!scoped_session_history_pending_dir(&scope, "main")
            .unwrap()
            .exists());
    }

    #[test]
    fn force_clear_scoped_history_refuses_live_turn_lock() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!(
            "force-clear-live-turn-lock-{}",
            uuid::Uuid::new_v4()
        ));
        append_scoped_session_history(&scope, "main", "user", "line").unwrap();
        let lock =
            acquire_scoped_session_history_turn_lock(&scope, "main", "turn-a", "turn A pending")
                .unwrap();
        mark_scoped_session_history_incomplete(&scope, "main", "incomplete").unwrap();

        let err = force_clear_scoped_session_history(&scope, "main").unwrap_err();

        assert!(err.to_string().contains("live turn lock"));
        assert!(scoped_session_history_turn_lock_dir(&scope, "main")
            .unwrap()
            .exists());
        assert!(scoped_session_history_file(&scope, "main")
            .unwrap()
            .exists());
        assert!(scoped_session_history_incomplete_file(&scope, "main")
            .unwrap()
            .exists());
        lock.release().unwrap();
    }

    #[test]
    fn incomplete_history_blocks_new_turns_until_clear() {
        let _env = crate::cli::test_env_lock().lock().unwrap();
        let scope = scope(&format!("blocked-{}", uuid::Uuid::new_v4()));

        append_scoped_session_history(&scope, "main", "user", "hello").unwrap();
        mark_scoped_session_history_pending_turn(&scope, "main", "turn-a", "run state unresolved")
            .unwrap();

        let err = ensure_scoped_session_history_complete(&scope, "main").unwrap_err();
        assert!(err.to_string().contains("run /clear"));

        clear_scoped_session_history(&scope, "main").unwrap();
        ensure_scoped_session_history_complete(&scope, "main").unwrap();
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
