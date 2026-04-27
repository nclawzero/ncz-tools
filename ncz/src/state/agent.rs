//! Active-agent state file: `/etc/nclawzero/agent` (one of zeroclaw|openclaw|hermes).

use std::fs;

use crate::{error::NczError, state::Paths};

pub const AGENTS: &[&str] = &["zeroclaw", "openclaw", "hermes"];
pub const DEFAULT: &str = "zeroclaw";

pub fn read(paths: &Paths) -> Result<String, NczError> {
    match fs::read_to_string(paths.agent_state()) {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            if AGENTS.iter().any(|a| *a == trimmed) {
                Ok(trimmed)
            } else {
                Ok(DEFAULT.to_string())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DEFAULT.to_string()),
        Err(e) => Err(NczError::Io(e)),
    }
}

pub fn write(paths: &Paths, agent: &str) -> Result<(), NczError> {
    if !AGENTS.iter().any(|a| *a == agent) {
        return Err(NczError::Usage(format!("unknown agent: {agent}")));
    }
    let body = format!("{agent}\n");
    crate::state::atomic_write(&paths.agent_state(), body.as_bytes(), 0o644)
}

pub fn port_for(agent: &str) -> Option<u16> {
    match agent {
        "zeroclaw" => Some(42617),
        "openclaw" => Some(18789),
        "hermes" => Some(8642),
        _ => None,
    }
}

pub fn service_for(agent: &str) -> String {
    format!("{agent}.service")
}
