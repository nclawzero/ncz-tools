#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::NczError;
use crate::state::{self, agent, Paths};
use crate::sys::{systemd, CommandRunner, ProcessOutput};

pub const SCHEMA_VERSION: u32 = 1;

pub fn validate_agent(name: &str) -> Result<(), NczError> {
    if agent::AGENTS.contains(&name) {
        Ok(())
    } else {
        Err(NczError::Usage(format!("unknown agent: {name}")))
    }
}

pub fn resolve_agent(paths: &Paths, requested: Option<&str>) -> Result<String, NczError> {
    let agent = match requested {
        Some(name) => name.to_string(),
        None => state::agent::read(paths)?,
    };
    validate_agent(&agent)?;
    Ok(agent)
}

pub fn require_tool(
    runner: &dyn CommandRunner,
    name: &str,
    probe_args: &[&str],
) -> Result<(), NczError> {
    match runner.run(name, probe_args) {
        Ok(out) if out.ok() => Ok(()),
        _ => Err(NczError::MissingDep(format!("{name} is not available"))),
    }
}

pub fn running_agents(runner: &dyn CommandRunner) -> Vec<String> {
    let mut running = Vec::new();
    for agent_name in agent::AGENTS {
        let unit = agent::service_for(agent_name);
        if systemd::is_active(runner, &unit).unwrap_or(false) {
            running.push((*agent_name).to_string());
        }
    }
    running
}

pub fn enabled_agents(runner: &dyn CommandRunner) -> Vec<String> {
    let mut enabled = Vec::new();
    for agent_name in agent::AGENTS {
        let unit = agent::service_for(agent_name);
        if systemd::is_enabled(runner, &unit).unwrap_or(false) {
            enabled.push((*agent_name).to_string());
        }
    }
    enabled
}

pub fn command_stdout(runner: &dyn CommandRunner, cmd: &str, args: &[&str]) -> Option<String> {
    let out = runner.run(cmd, args).ok()?;
    if out.ok() {
        Some(out.stdout.trim().to_string())
    } else {
        None
    }
}

pub fn command_output(runner: &dyn CommandRunner, cmd: &str, args: &[&str]) -> ProcessOutput {
    runner.run(cmd, args).unwrap_or_else(|e| ProcessOutput {
        status: -1,
        stdout: String::new(),
        stderr: e.to_string(),
    })
}

pub fn network_status(runner: &dyn CommandRunner, with_ping: bool) -> String {
    if runner
        .run("ip", &["route", "get", "1.1.1.1"])
        .map(|out| out.ok())
        .unwrap_or(false)
    {
        return "ok".to_string();
    }
    if with_ping
        && runner
            .run("ping", &["-c", "1", "-W", "1", "1.1.1.1"])
            .map(|out| out.ok())
            .unwrap_or(false)
    {
        return "ok".to_string();
    }
    "down".to_string()
}

pub fn read_first_line(path: &Path) -> Result<Option<String>, NczError> {
    match fs::read_to_string(path) {
        Ok(body) => Ok(body.lines().next().map(|s| s.trim().to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(NczError::Io(e)),
    }
}

pub fn sorted_files(dir: &Path) -> Result<Vec<PathBuf>, NczError> {
    let mut files = Vec::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    files.push(entry.path());
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(NczError::Io(e)),
    }
    files.sort();
    Ok(files)
}

pub fn strip_wrapping_quotes(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn redact_line(line: &str, show_secrets: bool) -> String {
    if show_secrets {
        return line.to_string();
    }

    if let Some(redacted) = redact_secret_assignment(line) {
        return redacted;
    }

    let lower = line.to_ascii_lowercase();
    for needle in [
        "authorization",
        "api_key",
        "apikey",
        "api-key",
        "credential",
        "password",
        "secret",
        "token",
    ] {
        if let Some(start) = lower.find(needle) {
            let after_key = &line[start + needle.len()..];
            let sep_len = after_key
                .char_indices()
                .find_map(|(idx, ch)| {
                    if ch == '=' || ch == ':' || ch == '_' || ch == '-' || ch.is_whitespace() {
                        None
                    } else {
                        Some(idx)
                    }
                })
                .unwrap_or(after_key.len());
            return format!("{}***", &line[..start + needle.len() + sep_len]);
        }
    }
    line.to_string()
}

fn redact_secret_assignment(line: &str) -> Option<String> {
    for (idx, ch) in line.char_indices() {
        if ch != '=' && ch != ':' {
            continue;
        }
        let field = secret_field_candidate(&line[..idx]);
        if field.is_empty() || !is_secret_field_name(field) {
            continue;
        }
        let after_separator = idx + ch.len_utf8();
        let value_start = line[after_separator..]
            .char_indices()
            .find_map(|(offset, ch)| {
                if ch.is_whitespace() {
                    None
                } else {
                    Some(after_separator + offset)
                }
            })
            .unwrap_or(line.len());
        if value_start == line.len() {
            continue;
        }
        return Some(format!("{}***", &line[..value_start]));
    }
    None
}

fn secret_field_candidate(prefix: &str) -> &str {
    let trimmed = prefix.trim_end();
    let start = trimmed
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| {
            if ch.is_whitespace() || matches!(ch, '{' | '[' | ',') {
                Some(idx + ch.len_utf8())
            } else {
                None
            }
        })
        .unwrap_or(0);
    trimmed[start..].trim_matches(|ch| ch == '"' || ch == '\'')
}

fn is_secret_field_name(name: &str) -> bool {
    let name = name
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .to_ascii_lowercase();
    name == "key"
        || name.contains("token")
        || name.contains("secret")
        || name.contains("credential")
        || name.contains("password")
        || name.contains("authorization")
        || name.contains("api_key")
        || name.contains("api-key")
        || name.contains("apikey")
        || name.ends_with("_key")
        || name.ends_with("-key")
}

pub fn mask_secret_value(value: &str, show_secrets: bool) -> String {
    if show_secrets || value.is_empty() {
        value.to_string()
    } else {
        "***".to_string()
    }
}

pub fn redact_path(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase();
    ["key", "token", "secret", "password"]
        .iter()
        .any(|needle| text.contains(needle))
}

pub fn probe_local_health(
    runner: &dyn CommandRunner,
    port: u16,
    _timeout_secs: u64,
) -> Result<bool, NczError> {
    #[cfg(test)]
    {
        Ok(runner.http_get_local(port, "/health", 2)? == 200)
    }

    #[cfg(not(test))]
    {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(_timeout_secs.max(1));
        loop {
            if runner.http_get_local(port, "/health", 2)? == 200 {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
pub(crate) fn test_paths(root: &Path) -> Paths {
    Paths {
        etc_dir: root.join("etc/nclawzero"),
        quadlet_dir: root.join("etc/containers/systemd"),
        lock_path: root.join("run/nclawzero.lock"),
    }
}

#[cfg(test)]
pub(crate) fn out(status: i32, stdout: &str, stderr: &str) -> ProcessOutput {
    ProcessOutput {
        status,
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
    }
}
