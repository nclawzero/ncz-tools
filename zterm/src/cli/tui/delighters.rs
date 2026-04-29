//! Small v0.3 flavor helpers that are testable without a terminal.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const CONNECT_SPLASH_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const CONNECT_SPLASH_MAX_BYTES: u64 = 4 * 1024;
const CONNECT_SPLASH_MAX_LINES: usize = 6;
const CONNECT_SPLASH_MAX_LINE_CHARS: usize = 96;
const STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const STATE_LOCK_POLL: Duration = Duration::from_millis(20);
static MUTATION_FENCE_DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

const WELCOME_QUOTES: &[&str] = &[
    "Turbo Pascal says: Hello, world!",
    "WordStar 7 loaded. Press ^KD to save.",
    "Paradox reports: table reindexed cleanly.",
    "dBASE V ready. SET TALK OFF.",
    "QEMM optimized upper memory. Conventional RAM smiles.",
    "Procomm Plus carrier detected. ANSI-BBS mode armed.",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZtermState {
    #[serde(default)]
    pub launches: u64,
    #[serde(default)]
    pub beep_on_error: bool,
    #[serde(default)]
    pub mutation_fences: BTreeMap<String, MutationFenceState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationFenceState {
    pub command: String,
    pub reason: String,
    pub created_at_unix: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dispatch_id: String,
}

#[derive(Debug, Clone)]
pub struct ForceClearMutationFenceResult {
    pub state: ZtermState,
    pub quarantined_state_path: Option<PathBuf>,
}

pub fn new_mutation_fence_dispatch_id() -> String {
    let seq = MUTATION_FENCE_DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{now}-{seq}", std::process::id())
}

pub fn sanitize_workspace_name(workspace: &str) -> String {
    let sanitized: String = workspace
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "workspace".to_string()
    } else {
        sanitized
    }
}

pub fn connect_splash_cache_path(base: &Path, workspace: &str) -> PathBuf {
    base.join("cache")
        .join("connect-splash")
        .join(format!("{}.txt", opaque_workspace_cache_key(workspace)))
}

pub fn default_connect_splash_cache_path(workspace: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|home| connect_splash_cache_path(&home.join(".zterm"), workspace))
}

pub fn default_state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".zterm").join("state.toml"))
}

pub fn is_cache_fresh(modified: SystemTime, now: SystemTime, ttl: Duration) -> bool {
    now.duration_since(modified)
        .map(|age| age <= ttl)
        .unwrap_or(true)
}

pub fn read_cached_connect_splash(path: &Path, now: SystemTime, ttl: Duration) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    if !is_cache_fresh(modified, now, ttl) {
        return None;
    }
    if metadata.len() > CONNECT_SPLASH_MAX_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let normalized = normalize_connect_splash(&text);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub fn write_connect_splash_cache(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_cache_dir_all(parent)?;
    }
    let normalized = normalize_connect_splash(text);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("splash");
    let tmp_path = parent.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp_path)?;
    file.write_all(normalized.as_bytes())?;
    drop(file);
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    harden_private_cache_file(path)
}

fn opaque_workspace_cache_key(workspace: &str) -> String {
    let digest = Sha256::digest(workspace.as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn create_private_cache_dir_all(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    harden_private_cache_dirs(dir)
}

#[cfg(unix)]
fn harden_private_cache_dirs(dir: &Path) -> io::Result<()> {
    let mut dirs = vec![dir.to_path_buf()];
    if dir.file_name().and_then(|name| name.to_str()) == Some("connect-splash") {
        if let Some(cache_dir) = dir.parent() {
            dirs.push(cache_dir.to_path_buf());
            if cache_dir.file_name().and_then(|name| name.to_str()) == Some("cache") {
                if let Some(root_dir) = cache_dir.parent() {
                    dirs.push(root_dir.to_path_buf());
                }
            }
        }
    }
    for dir in dirs {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn harden_private_cache_dirs(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn harden_private_cache_file(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn harden_private_cache_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn normalize_connect_splash(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .take(CONNECT_SPLASH_MAX_LINES)
        .map(|line| {
            line.chars()
                .take(CONNECT_SPLASH_MAX_LINE_CHARS)
                .collect::<String>()
        })
        .collect::<Vec<String>>()
        .join("\n")
}

pub fn local_connect_splash(workspace: &str) -> String {
    normalize_connect_splash(&format!(
        "ATZ\n\
         OK\n\
         ATDT {}\n\
         CONNECT 14400/ZTERM\n\
         CARRIER LOCKED\n\
         WORKSPACE READY",
        sanitize_workspace_name(workspace).to_ascii_uppercase()
    ))
}

pub fn load_state(path: &Path) -> ZtermState {
    load_state_checked(path).unwrap_or_default()
}

pub fn load_state_checked(path: &Path) -> io::Result<ZtermState> {
    load_state_unlocked(path)
}

fn load_state_unlocked(path: &Path) -> io::Result<ZtermState> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ZtermState::default()),
        Err(e) => Err(e),
    }
}

pub fn save_state(path: &Path, state: &ZtermState) -> io::Result<()> {
    with_state_lock(path, || save_state_unlocked(path, state))
}

fn save_state_unlocked(path: &Path, state: &ZtermState) -> io::Result<()> {
    save_state_unlocked_with(path, state, |file| file.sync_all(), sync_state_parent_dir)
}

fn save_state_unlocked_with(
    path: &Path,
    state: &ZtermState,
    sync_file: impl FnOnce(&fs::File) -> io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_state_dir(parent)?;
    }
    let body = toml::to_string_pretty(state).map_err(std::io::Error::other)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.toml");
    let tmp_path = parent.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp_path)?;
    let write_result = (|| {
        file.write_all(body.as_bytes())?;
        sync_file(&file)
    })();
    drop(file);
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    harden_private_state_file(path)?;
    sync_parent(parent)
}

#[cfg(unix)]
fn sync_state_parent_dir(parent: &Path) -> io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_state_parent_dir(_parent: &Path) -> io::Result<()> {
    Ok(())
}

fn with_state_lock<T>(path: &Path, update: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    with_state_lock_timeout(path, STATE_LOCK_TIMEOUT, STATE_LOCK_POLL, update)
}

fn with_state_lock_timeout<T>(
    path: &Path,
    timeout: Duration,
    poll: Duration,
    update: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    if let Some(parent) = path.parent() {
        create_private_state_dir(parent)?;
    }
    let lock_path = state_lock_path(path);
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let lock_file = opts.open(&lock_path)?;
    let started = Instant::now();
    loop {
        match lock_file.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {
                if started.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out after {:?} waiting for zterm state lock {}",
                            timeout,
                            lock_path.display()
                        ),
                    ));
                }
                std::thread::sleep(poll);
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    let result = update();
    let unlock_result = lock_file.unlock();
    match result {
        Ok(value) => unlock_result.map(|()| value),
        Err(e) => Err(e),
    }
}

fn state_lock_path(path: &Path) -> PathBuf {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.toml");
    path.with_file_name(format!("{filename}.lock"))
}

fn create_private_state_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    harden_private_state_dir(dir)
}

#[cfg(unix)]
fn harden_private_state_dir(dir: &Path) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn harden_private_state_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn harden_private_state_file(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn harden_private_state_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn record_launch() -> std::io::Result<(u64, Option<String>)> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm state",
        ));
    };
    record_launch_at(&path)
}

pub fn record_launch_at(path: &Path) -> std::io::Result<(u64, Option<String>)> {
    with_state_lock(path, || {
        let mut state = load_state_unlocked(path)?;
        state.launches = state.launches.saturating_add(1);
        let launches = state.launches;
        save_state_unlocked(path, &state)?;
        Ok((launches, welcome_quote_for_launch(launches)))
    })
}

pub fn set_beep_on_error(enabled: bool) -> std::io::Result<ZtermState> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm state",
        ));
    };
    set_beep_on_error_at(&path, enabled)
}

pub fn set_beep_on_error_at(path: &Path, enabled: bool) -> std::io::Result<ZtermState> {
    with_state_lock(path, || {
        let mut state = load_state_unlocked(path)?;
        state.beep_on_error = enabled;
        save_state_unlocked(path, &state)?;
        Ok(state)
    })
}

pub fn mutation_fence_for_workspace(
    workspace_key: &str,
) -> std::io::Result<Option<MutationFenceState>> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot read zterm mutation fence",
        ));
    };
    mutation_fence_for_workspace_at(&path, workspace_key)
}

pub fn mutation_fence_for_workspace_at(
    path: &Path,
    workspace_key: &str,
) -> std::io::Result<Option<MutationFenceState>> {
    Ok(load_state_unlocked(path)?
        .mutation_fences
        .get(workspace_key)
        .cloned())
}

pub fn set_mutation_fence_for_workspace(
    workspace_key: &str,
    fence: MutationFenceState,
) -> std::io::Result<ZtermState> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm mutation fence",
        ));
    };
    set_mutation_fence_for_workspace_at(&path, workspace_key, fence)
}

pub fn set_mutation_fence_for_workspace_at(
    path: &Path,
    workspace_key: &str,
    fence: MutationFenceState,
) -> std::io::Result<ZtermState> {
    with_state_lock(path, || {
        let mut state = load_state_unlocked(path)?;
        state
            .mutation_fences
            .insert(workspace_key.to_string(), fence);
        save_state_unlocked(path, &state)?;
        Ok(state)
    })
}

pub fn acquire_mutation_fence_for_workspace(
    workspace_key: &str,
    fence: MutationFenceState,
) -> std::io::Result<Result<ZtermState, MutationFenceState>> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm mutation fence",
        ));
    };
    acquire_mutation_fence_for_workspace_at(&path, workspace_key, fence)
}

pub fn acquire_mutation_fence_for_workspace_at(
    path: &Path,
    workspace_key: &str,
    fence: MutationFenceState,
) -> std::io::Result<Result<ZtermState, MutationFenceState>> {
    with_state_lock(path, || {
        let mut state = load_state_unlocked(path)?;
        if let Some(existing) = state.mutation_fences.get(workspace_key).cloned() {
            return Ok(Err(existing));
        }
        state
            .mutation_fences
            .insert(workspace_key.to_string(), fence);
        save_state_unlocked(path, &state)?;
        Ok(Ok(state))
    })
}

pub fn replace_mutation_fence_for_workspace(
    old_workspace_key: Option<&str>,
    workspace_key: &str,
    fence: MutationFenceState,
) -> std::io::Result<ZtermState> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm mutation fence",
        ));
    };
    replace_mutation_fence_for_workspace_at(&path, old_workspace_key, workspace_key, fence)
}

pub fn replace_mutation_fence_for_workspace_at(
    path: &Path,
    old_workspace_key: Option<&str>,
    workspace_key: &str,
    fence: MutationFenceState,
) -> std::io::Result<ZtermState> {
    with_state_lock(path, || {
        let mut state = load_state_unlocked(path)?;
        if let Some(old_key) = old_workspace_key.filter(|old_key| *old_key != workspace_key) {
            state.mutation_fences.remove(old_key);
        }
        state
            .mutation_fences
            .insert(workspace_key.to_string(), fence);
        save_state_unlocked(path, &state)?;
        Ok(state)
    })
}

pub fn replace_mutation_fence_for_workspace_if_dispatch(
    old_workspace_key: &str,
    old_dispatch_id: &str,
    workspace_key: &str,
    fence: MutationFenceState,
) -> std::io::Result<bool> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm mutation fence",
        ));
    };
    replace_mutation_fence_for_workspace_if_dispatch_at(
        &path,
        old_workspace_key,
        old_dispatch_id,
        workspace_key,
        fence,
    )
}

pub fn replace_mutation_fence_for_workspace_if_dispatch_at(
    path: &Path,
    old_workspace_key: &str,
    old_dispatch_id: &str,
    workspace_key: &str,
    fence: MutationFenceState,
) -> std::io::Result<bool> {
    with_state_lock(path, || {
        let mut state = load_state_unlocked(path)?;
        let owns_old = state
            .mutation_fences
            .get(old_workspace_key)
            .map(|existing| existing.dispatch_id == old_dispatch_id)
            .unwrap_or(false);
        if !owns_old {
            return Ok(false);
        }
        if old_workspace_key != workspace_key {
            if let Some(existing_target) = state.mutation_fences.get(workspace_key) {
                if existing_target.dispatch_id != old_dispatch_id {
                    return Ok(false);
                }
            }
            state.mutation_fences.remove(old_workspace_key);
        }
        state
            .mutation_fences
            .insert(workspace_key.to_string(), fence);
        save_state_unlocked(path, &state)?;
        Ok(true)
    })
}

pub fn clear_mutation_fence_for_workspace(workspace_key: &str) -> std::io::Result<ZtermState> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm mutation fence",
        ));
    };
    clear_mutation_fence_for_workspace_at(&path, workspace_key)
}

pub fn clear_mutation_fence_for_workspace_at(
    path: &Path,
    workspace_key: &str,
) -> std::io::Result<ZtermState> {
    with_state_lock(path, || {
        let mut state = load_state_unlocked(path)?;
        state.mutation_fences.remove(workspace_key);
        save_state_unlocked(path, &state)?;
        Ok(state)
    })
}

pub fn force_clear_mutation_fence_for_workspace(
    workspace_key: &str,
) -> std::io::Result<ForceClearMutationFenceResult> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm mutation fence",
        ));
    };
    force_clear_mutation_fence_for_workspace_at(&path, workspace_key)
}

pub fn force_clear_mutation_fence_for_workspace_at(
    path: &Path,
    workspace_key: &str,
) -> std::io::Result<ForceClearMutationFenceResult> {
    with_state_lock(path, || match load_state_unlocked(path) {
        Ok(mut state) => {
            state.mutation_fences.remove(workspace_key);
            save_state_unlocked(path, &state)?;
            Ok(ForceClearMutationFenceResult {
                state,
                quarantined_state_path: None,
            })
        }
        Err(load_err) => {
            let quarantined_state_path = quarantine_unreadable_state_file(path, &load_err)?;
            let state = ZtermState::default();
            save_state_unlocked(path, &state)?;
            Ok(ForceClearMutationFenceResult {
                state,
                quarantined_state_path: Some(quarantined_state_path),
            })
        }
    })
}

fn quarantine_unreadable_state_file(path: &Path, load_err: &io::Error) -> io::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        create_private_state_dir(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.toml");
    let quarantine_path = parent.join(format!("{filename}.corrupt.{}", uuid::Uuid::new_v4()));
    fs::rename(path, &quarantine_path).map_err(|rename_err| {
        io::Error::new(
            rename_err.kind(),
            format!(
                "could not quarantine unreadable zterm state {} after load failed: {load_err}; rename failed: {rename_err}",
                path.display()
            ),
        )
    })?;
    harden_private_state_file(&quarantine_path)?;
    sync_state_parent_dir(parent)?;
    Ok(quarantine_path)
}

pub fn clear_mutation_fence_for_workspace_if_dispatch(
    workspace_key: &str,
    dispatch_id: &str,
) -> std::io::Result<bool> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm mutation fence",
        ));
    };
    clear_mutation_fence_for_workspace_if_dispatch_at(&path, workspace_key, dispatch_id)
}

pub fn clear_mutation_fence_for_workspace_if_dispatch_at(
    path: &Path,
    workspace_key: &str,
    dispatch_id: &str,
) -> std::io::Result<bool> {
    with_state_lock(path, || {
        let mut state = load_state_unlocked(path)?;
        let owns_fence = state
            .mutation_fences
            .get(workspace_key)
            .map(|existing| existing.dispatch_id == dispatch_id)
            .unwrap_or(false);
        if !owns_fence {
            return Ok(false);
        }
        state.mutation_fences.remove(workspace_key);
        save_state_unlocked(path, &state)?;
        Ok(true)
    })
}

pub fn is_welcome_milestone(launches: u64) -> bool {
    launches == 5 || launches == 10 || (launches >= 25 && launches.is_multiple_of(25))
}

pub fn welcome_quote_for_launch(launches: u64) -> Option<String> {
    if !is_welcome_milestone(launches) {
        return None;
    }
    let quote = WELCOME_QUOTES
        .choose(&mut rand::thread_rng())
        .copied()
        .unwrap_or(WELCOME_QUOTES[0]);
    Some(format!("Welcome back #{launches}: {quote}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn workspace_cache_filename_is_sanitized() {
        assert_eq!(sanitize_workspace_name("../prod typhon"), ".._prod_typhon");
        assert_eq!(sanitize_workspace_name(""), "workspace");
    }

    #[test]
    fn workspace_cache_filename_is_opaque_hash() {
        let path = connect_splash_cache_path(Path::new("/tmp/.zterm"), "../prod typhon");
        let filename = path.file_name().unwrap().to_string_lossy();
        let stem = filename.strip_suffix(".txt").unwrap();

        assert_eq!(stem.len(), 32);
        assert!(stem.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(!filename.contains("prod"));
        assert!(!filename.contains("typhon"));
    }

    #[test]
    fn cache_freshness_uses_mtime_and_ttl() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let fresh = now - Duration::from_secs(60);
        let stale = now - CONNECT_SPLASH_TTL - Duration::from_secs(1);

        assert!(is_cache_fresh(fresh, now, CONNECT_SPLASH_TTL));
        assert!(!is_cache_fresh(stale, now, CONNECT_SPLASH_TTL));
    }

    #[test]
    fn normalize_connect_splash_caps_to_six_non_empty_lines() {
        let input = "CONNECT 2400\n\nline2  \nline3\nline4\nline5\nline6\nline7\n";
        assert_eq!(
            normalize_connect_splash(input),
            "CONNECT 2400\nline2\nline3\nline4\nline5\nline6"
        );
    }

    #[test]
    fn normalize_connect_splash_caps_line_length() {
        let long_line = "x".repeat(CONNECT_SPLASH_MAX_LINE_CHARS + 10);
        let normalized = normalize_connect_splash(&long_line);

        assert_eq!(normalized.chars().count(), CONNECT_SPLASH_MAX_LINE_CHARS);
    }

    #[test]
    fn connect_splash_cache_rejects_oversized_file_before_reading() {
        let dir = tempdir().unwrap();
        let path = connect_splash_cache_path(dir.path(), "prod typhon");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "x".repeat(CONNECT_SPLASH_MAX_BYTES as usize + 1)).unwrap();

        assert!(read_cached_connect_splash(&path, SystemTime::now(), CONNECT_SPLASH_TTL).is_none());
    }

    #[test]
    fn local_connect_splash_is_safe_and_normalized() {
        assert_eq!(
            local_connect_splash("prod typhon"),
            "ATZ\nOK\nATDT PROD_TYPHON\nCONNECT 14400/ZTERM\nCARRIER LOCKED\nWORKSPACE READY"
        );
    }

    #[test]
    #[cfg(unix)]
    fn connect_splash_cache_uses_private_unix_modes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let base = dir.path().join(".zterm");
        let path = connect_splash_cache_path(&base, "prod typhon");

        write_connect_splash_cache(&path, "line 1\nline 2").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for dir in [
            base.clone(),
            base.join("cache"),
            base.join("cache").join("connect-splash"),
        ] {
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                dir.display()
            );
        }
    }

    #[test]
    fn launch_counter_persists_to_state_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");

        assert_eq!(record_launch_at(&path).unwrap().0, 1);
        assert_eq!(record_launch_at(&path).unwrap().0, 2);

        let state = load_state(&path);
        assert_eq!(state.launches, 2);
        assert!(!state.beep_on_error);
    }

    #[test]
    fn beep_toggle_persists_without_resetting_launches() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");

        record_launch_at(&path).unwrap();
        let state = set_beep_on_error_at(&path, true).unwrap();

        assert_eq!(state.launches, 1);
        assert!(state.beep_on_error);
        assert!(load_state(&path).beep_on_error);
    }

    #[test]
    fn state_save_propagates_temp_file_sync_failure_without_final_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let state = ZtermState {
            launches: 7,
            beep_on_error: true,
            mutation_fences: BTreeMap::new(),
        };

        let err = save_state_unlocked_with(
            &path,
            &state,
            |_| Err(io::Error::other("injected temp sync failure")),
            |_| Ok(()),
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(!path.exists());
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".state.toml."))
            })
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "state save left temporary files after sync failure: {leftovers:?}"
        );
    }

    #[test]
    fn state_save_propagates_parent_directory_sync_failure() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let state = ZtermState {
            launches: 7,
            beep_on_error: true,
            mutation_fences: BTreeMap::new(),
        };

        let err = save_state_unlocked_with(
            &path,
            &state,
            |_| Ok(()),
            |_| Err(io::Error::other("injected directory sync failure")),
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(load_state_checked(&path).unwrap().launches, 7);
    }

    #[test]
    fn mutation_fence_persists_per_workspace() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let fence = MutationFenceState {
            command: "/cron add '0 9 * * *' standup".to_string(),
            reason: "slash command outcome unknown".to_string(),
            created_at_unix: 42,
            dispatch_id: "dispatch-prod".to_string(),
        };
        let other_fence = MutationFenceState {
            command: "/session create scratch".to_string(),
            reason: "session outcome unknown".to_string(),
            created_at_unix: 43,
            dispatch_id: "dispatch-dev".to_string(),
        };

        set_mutation_fence_for_workspace_at(&path, "id:prod", fence.clone()).unwrap();
        set_mutation_fence_for_workspace_at(&path, "id:dev", other_fence.clone()).unwrap();

        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:prod").unwrap(),
            Some(fence)
        );
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:dev").unwrap(),
            Some(other_fence.clone())
        );

        clear_mutation_fence_for_workspace_at(&path, "id:prod").unwrap();
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:prod").unwrap(),
            None
        );
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:dev").unwrap(),
            Some(other_fence)
        );
    }

    #[test]
    fn mutation_fence_acquire_refuses_to_replace_existing_owner() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let fence = MutationFenceState {
            command: "/memory post one".to_string(),
            reason: "first dispatch pending".to_string(),
            created_at_unix: 42,
            dispatch_id: "dispatch-one".to_string(),
        };
        let competing = MutationFenceState {
            command: "/memory post two".to_string(),
            reason: "second dispatch pending".to_string(),
            created_at_unix: 43,
            dispatch_id: "dispatch-two".to_string(),
        };

        assert!(
            acquire_mutation_fence_for_workspace_at(&path, "id:prod", fence.clone())
                .unwrap()
                .is_ok()
        );
        let existing =
            acquire_mutation_fence_for_workspace_at(&path, "id:prod", competing).unwrap();

        let existing = existing.expect_err("second acquire should return existing fence");
        assert_eq!(existing, fence.clone());
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:prod").unwrap(),
            Some(fence)
        );
    }

    #[test]
    fn mutation_fence_acquire_is_atomic_for_competing_writers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let writers = 8;
        let barrier = Arc::new(Barrier::new(writers));
        let handles = (0..writers)
            .map(|idx| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let dispatch_id = format!("dispatch-{idx}");
                    let fence = MutationFenceState {
                        command: format!("/memory post {idx}"),
                        reason: format!("dispatch {idx} pending"),
                        created_at_unix: idx as u64,
                        dispatch_id: dispatch_id.clone(),
                    };
                    barrier.wait();
                    let acquired = acquire_mutation_fence_for_workspace_at(&path, "id:prod", fence)
                        .unwrap()
                        .is_ok();
                    (dispatch_id, acquired)
                })
            })
            .collect::<Vec<_>>();

        let winners = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter_map(|(dispatch_id, acquired)| acquired.then_some(dispatch_id))
            .collect::<Vec<_>>();
        let persisted = mutation_fence_for_workspace_at(&path, "id:prod")
            .unwrap()
            .unwrap();

        assert_eq!(winners.len(), 1);
        assert_eq!(persisted.dispatch_id, winners[0]);
    }

    #[test]
    fn mutation_fence_replace_and_clear_require_matching_dispatch_owner() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let original = MutationFenceState {
            command: "/workspace switch beta".to_string(),
            reason: "switch pending".to_string(),
            created_at_unix: 42,
            dispatch_id: "owner-a".to_string(),
        };
        let replacement = MutationFenceState {
            command: "/workspace switch beta".to_string(),
            reason: "switch outcome unknown".to_string(),
            created_at_unix: 43,
            dispatch_id: "owner-a".to_string(),
        };

        set_mutation_fence_for_workspace_at(&path, "id:alpha", original.clone()).unwrap();

        assert!(!replace_mutation_fence_for_workspace_if_dispatch_at(
            &path,
            "id:alpha",
            "wrong-owner",
            "id:beta",
            replacement.clone()
        )
        .unwrap());
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:alpha").unwrap(),
            Some(original)
        );
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:beta").unwrap(),
            None
        );

        assert!(replace_mutation_fence_for_workspace_if_dispatch_at(
            &path,
            "id:alpha",
            "owner-a",
            "id:beta",
            replacement.clone()
        )
        .unwrap());
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:alpha").unwrap(),
            None
        );
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:beta").unwrap(),
            Some(replacement.clone())
        );

        assert!(!clear_mutation_fence_for_workspace_if_dispatch_at(
            &path,
            "id:beta",
            "wrong-owner"
        )
        .unwrap());
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:beta").unwrap(),
            Some(replacement)
        );
        assert!(
            clear_mutation_fence_for_workspace_if_dispatch_at(&path, "id:beta", "owner-a").unwrap()
        );
        assert_eq!(
            mutation_fence_for_workspace_at(&path, "id:beta").unwrap(),
            None
        );
    }

    #[test]
    fn malformed_state_fails_checked_load_and_is_not_rewritten_by_boot_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        fs::write(&path, "launches = ???\nmutation_fences = {}\n").unwrap();

        assert!(load_state_checked(&path).is_err());
        assert!(mutation_fence_for_workspace_at(&path, "id:prod").is_err());
        assert!(record_launch_at(&path).is_err());
        assert!(set_beep_on_error_at(&path, true).is_err());

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("launches = ???"));
    }

    #[test]
    fn regular_clear_malformed_state_fails_closed_without_rewrite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        fs::write(&path, "launches = ???\nmutation_fences = {}\n").unwrap();

        assert!(clear_mutation_fence_for_workspace_at(&path, "id:prod").is_err());

        let quarantined = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("state.toml.corrupt.")
            });
        assert!(!quarantined);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("launches = ???"));
    }

    #[test]
    fn force_clear_malformed_state_quarantines_and_rewrites() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        fs::write(&path, "launches = ???\nmutation_fences = {}\n").unwrap();

        let result = force_clear_mutation_fence_for_workspace_at(&path, "id:prod").unwrap();

        assert!(result.state.mutation_fences.is_empty());
        let quarantine_path = result.quarantined_state_path.unwrap();
        assert!(quarantine_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("state.toml.corrupt."));
        assert!(fs::read_to_string(&quarantine_path)
            .unwrap()
            .contains("launches = ???"));
        let recovered = load_state_checked(&path).unwrap();
        assert_eq!(recovered.launches, 0);
        assert!(!recovered.beep_on_error);
        assert!(recovered.mutation_fences.is_empty());
        assert!(mutation_fence_for_workspace_at(&path, "id:prod")
            .unwrap()
            .is_none());
    }

    #[test]
    fn state_lock_times_out_instead_of_blocking_indefinitely() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        create_private_state_dir(path.parent().unwrap()).unwrap();
        let lock_path = state_lock_path(&path);
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        lock_file.lock().unwrap();

        let err = with_state_lock_timeout(
            &path,
            Duration::from_millis(20),
            Duration::from_millis(1),
            || Ok(()),
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        lock_file.unlock().unwrap();
    }

    #[test]
    fn concurrent_state_updates_do_not_clobber_launches_or_beep() {
        let dir = tempdir().unwrap();
        let path = Arc::new(dir.path().join("state.toml"));
        let barrier = Arc::new(Barrier::new(3));
        let launches = 64;

        let launch_path = Arc::clone(&path);
        let launch_barrier = Arc::clone(&barrier);
        let launch_thread = thread::spawn(move || {
            launch_barrier.wait();
            for _ in 0..launches {
                record_launch_at(&launch_path).unwrap();
                thread::yield_now();
            }
        });

        let beep_path = Arc::clone(&path);
        let beep_barrier = Arc::clone(&barrier);
        let beep_thread = thread::spawn(move || {
            beep_barrier.wait();
            for idx in 0..launches {
                set_beep_on_error_at(&beep_path, idx % 2 == 0).unwrap();
                thread::yield_now();
            }
            set_beep_on_error_at(&beep_path, true).unwrap();
        });

        barrier.wait();
        launch_thread.join().unwrap();
        beep_thread.join().unwrap();

        let state = load_state(&path);
        assert_eq!(state.launches, launches);
        assert!(state.beep_on_error);
    }

    #[test]
    fn welcome_quotes_only_land_on_milestones() {
        assert!(!is_welcome_milestone(4));
        assert!(is_welcome_milestone(5));
        assert!(is_welcome_milestone(10));
        assert!(is_welcome_milestone(25));
        assert!(welcome_quote_for_launch(5)
            .unwrap()
            .starts_with("Welcome back #5:"));
        assert!(welcome_quote_for_launch(6).is_none());
    }
}
