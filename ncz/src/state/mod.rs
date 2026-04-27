//! State file IO. All `/etc/nclawzero/...` reads + writes route through here
//! so paths, permissions, and locking are centralized.
//!
//! Atomic write = `tempfile::NamedTempFile::new_in(parent)` → `persist()` →
//! directory `fsync`. The persist call uses `rename(2)` which is atomic on
//! the same filesystem.
//!
//! `/run/nclawzero.lock` is the single workspace-wide flock; mutating
//! commands take an exclusive lock for the duration of the operation.
//! Reads do not lock.

pub mod agent;
pub mod channel;
pub mod providers;
pub mod quadlet;

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use rustix::fs::{flock, FlockOperation};
use tempfile::NamedTempFile;

use crate::error::NczError;

/// Default base directories. Tests construct a `Paths` with a sandbox root.
#[derive(Debug, Clone)]
pub struct Paths {
    pub etc_dir: PathBuf,
    pub quadlet_dir: PathBuf,
    pub lock_path: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            etc_dir: PathBuf::from("/etc/nclawzero"),
            quadlet_dir: PathBuf::from("/etc/containers/systemd"),
            lock_path: PathBuf::from("/run/nclawzero.lock"),
        }
    }
}

impl Paths {
    pub fn agent_state(&self) -> PathBuf {
        self.etc_dir.join("agent")
    }
    pub fn channel(&self) -> PathBuf {
        self.etc_dir.join("channel")
    }
    pub fn primary_provider(&self) -> PathBuf {
        self.etc_dir.join("primary-provider")
    }
    pub fn manifest(&self) -> PathBuf {
        self.etc_dir.join("manifest.sha256")
    }
    pub fn agent_quadlet(&self, agent: &str) -> PathBuf {
        self.quadlet_dir.join(format!("{agent}.container"))
    }
}

/// RAII exclusive flock guard. Drop releases the lock.
pub struct LockGuard {
    _file: File,
}

pub fn acquire_lock(lock_path: &Path) -> Result<LockGuard, NczError> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    flock(&file, FlockOperation::LockExclusive).map_err(|e| {
        NczError::Precondition(format!("could not flock {}: {e}", lock_path.display()))
    })?;
    Ok(LockGuard { _file: file })
}

/// Write `contents` to `path` atomically, with `mode` permission bits.
pub fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> Result<(), NczError> {
    let parent = path
        .parent()
        .ok_or_else(|| NczError::Precondition(format!("path has no parent: {}", path.display())))?;
    fs::create_dir_all(parent)?;

    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(contents)?;
    tmp.as_file().sync_all()?;

    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(mode);
    fs::set_permissions(tmp.path(), perms)?;

    tmp.persist(path)
        .map_err(|e| NczError::Io(e.error))?;

    // fsync the parent directory so the rename survives crash.
    File::open(parent)?.sync_all()?;
    Ok(())
}
