//! Release-channel state file: `/etc/nclawzero/channel` (stable|canary|beta).

use std::fs;

use crate::{error::NczError, state::Paths};

pub const CHANNELS: &[&str] = &["stable", "canary", "beta"];
pub const DEFAULT: &str = "stable";

pub fn read(paths: &Paths) -> Result<String, NczError> {
    match fs::read_to_string(paths.channel()) {
        Ok(s) => Ok(s.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DEFAULT.to_string()),
        Err(e) => Err(NczError::Io(e)),
    }
}

pub fn write(paths: &Paths, channel: &str) -> Result<(), NczError> {
    if !CHANNELS.contains(&channel) {
        return Err(NczError::Usage(format!("unknown channel: {channel}")));
    }
    let body = format!("{channel}\n");
    crate::state::atomic_write(&paths.channel(), body.as_bytes(), 0o644)
}
