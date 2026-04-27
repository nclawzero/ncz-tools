//! Provider state. Codex fills in the schema details; this file is the
//! pinpoint where `/etc/nclawzero/providers.d/*` and
//! `/etc/nclawzero/primary-provider` reads/writes live.

use std::fs;

use crate::{error::NczError, state::Paths};

pub fn read_primary(paths: &Paths) -> Result<Option<String>, NczError> {
    match fs::read_to_string(paths.primary_provider()) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(NczError::Io(e)),
    }
}

pub fn write_primary(paths: &Paths, name: &str) -> Result<(), NczError> {
    let body = format!("{name}\n");
    crate::state::atomic_write(&paths.primary_provider(), body.as_bytes(), 0o644)
}
