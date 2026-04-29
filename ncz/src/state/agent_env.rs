//! Shared credential environment file state.
//!
//! `/etc/nclawzero/agent-env` is loaded by quadlets through
//! `EnvironmentFile=`. The writer emits systemd-compatible assignments and
//! quotes values when needed; the reader tolerates `export KEY=value` from
//! hand-edited files.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

use crate::error::NczError;
use crate::state::{self, Paths};

const PROVIDER_BINDING_PREFIX: &str = "NCZ_PROVIDER_BINDING_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnvEntry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactedAgentEnvEntry {
    pub key: String,
    pub set: bool,
    pub value: String,
}

pub fn read(paths: &Paths) -> Result<Vec<AgentEnvEntry>, NczError> {
    read_path(&paths.agent_env())
}

pub fn read_override(paths: &Paths, agent: &str) -> Result<Vec<AgentEnvEntry>, NczError> {
    read_path(&paths.agent_env_override(agent))
}

pub fn redacted_list(
    paths: &Paths,
    show_secrets: bool,
) -> Result<Vec<RedactedAgentEnvEntry>, NczError> {
    Ok(read(paths)?
        .into_iter()
        .filter(|entry| !entry.key.starts_with(PROVIDER_BINDING_PREFIX))
        .map(|entry| RedactedAgentEnvEntry {
            key: entry.key,
            set: true,
            value: if show_secrets {
                entry.value
            } else {
                "***".to_string()
            },
        })
        .collect())
}

pub fn set(paths: &Paths, key: &str, value: &str) -> Result<bool, NczError> {
    set_path(&paths.agent_env(), key, value)
}

pub fn set_if_absent(paths: &Paths, key: &str, value: &str) -> Result<bool, NczError> {
    validate_key(key)?;
    validate_value(value)?;
    if read(paths)?
        .iter()
        .any(|entry| entry.key == key && !entry.value.is_empty())
    {
        return Ok(false);
    }
    set(paths, key, value)?;
    Ok(true)
}

pub fn set_override(paths: &Paths, agent: &str, key: &str, value: &str) -> Result<bool, NczError> {
    set_path(&paths.agent_env_override(agent), key, value)
}

pub fn remove(paths: &Paths, key: &str) -> Result<bool, NczError> {
    remove_path(&paths.agent_env(), key)
}

pub fn set_provider_binding(
    paths: &Paths,
    provider: &str,
    key_env: &str,
    url: &str,
) -> Result<bool, NczError> {
    validate_key(key_env)?;
    validate_value(url)?;
    if url.contains(' ') {
        return Err(NczError::Usage(format!(
            "provider URL for {provider} cannot contain spaces"
        )));
    }
    let binding_key = provider_binding_key(provider)?;
    let binding_value = format!("{key_env} {url}");
    set_path(&paths.agent_env(), &binding_key, &binding_value)
}

pub fn provider_binding_matches(
    entries: &[AgentEnvEntry],
    provider: &str,
    key_env: &str,
    url: &str,
) -> Result<bool, NczError> {
    let binding_key = provider_binding_key(provider)?;
    Ok(entries.iter().any(|entry| {
        entry.key == binding_key
            && parse_provider_binding_value(&entry.value).is_some_and(
                |(bound_key_env, bound_url)| bound_key_env == key_env && bound_url == url,
            )
    }))
}

pub fn remove_provider_bindings_for_key(
    paths: &Paths,
    key_env: &str,
) -> Result<Vec<String>, NczError> {
    validate_key(key_env)?;
    let entries = read(paths)?;
    let mut removed = Vec::new();
    for entry in entries {
        if !entry.key.starts_with(PROVIDER_BINDING_PREFIX) {
            continue;
        }
        let Some((bound_key_env, _)) = parse_provider_binding_value(&entry.value) else {
            continue;
        };
        if bound_key_env == key_env && remove_path(&paths.agent_env(), &entry.key)? {
            removed.push(provider_from_binding_key(&entry.key).unwrap_or(entry.key));
        }
    }
    Ok(removed)
}

pub fn remove_provider_bindings_for_providers(
    paths: &Paths,
    providers: &std::collections::BTreeSet<String>,
) -> Result<Vec<String>, NczError> {
    let mut removed = Vec::new();
    for provider in providers {
        validate_provider_binding_name(provider)?;
        let binding_key = provider_binding_key(provider)?;
        if remove_path(&paths.agent_env(), &binding_key)? {
            removed.push(provider.clone());
        }
    }
    Ok(removed)
}

pub fn remove_override(paths: &Paths, agent: &str, key: &str) -> Result<bool, NczError> {
    remove_path(&paths.agent_env_override(agent), key)
}

pub fn validate_key(key: &str) -> Result<(), NczError> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(NczError::Usage(
            "environment key cannot be empty".to_string(),
        ));
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(NczError::Usage(format!("invalid environment key: {key}")));
    }
    if chars.any(|ch| !(ch == '_' || ch.is_ascii_alphanumeric())) {
        return Err(NczError::Usage(format!("invalid environment key: {key}")));
    }
    Ok(())
}

fn provider_binding_key(provider: &str) -> Result<String, NczError> {
    validate_provider_binding_name(provider)?;
    let mut key = String::from(PROVIDER_BINDING_PREFIX);
    for byte in provider.as_bytes() {
        let _ = write!(&mut key, "{byte:02X}");
    }
    Ok(key)
}

fn provider_from_binding_key(key: &str) -> Option<String> {
    let hex = key.strip_prefix(PROVIDER_BINDING_PREFIX)?;
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::new();
    for idx in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[idx..idx + 2], 16).ok()?;
        bytes.push(byte);
    }
    String::from_utf8(bytes).ok()
}

fn validate_provider_binding_name(provider: &str) -> Result<(), NczError> {
    if provider.is_empty()
        || provider.starts_with('.')
        || provider.ends_with(".models")
        || provider.contains("..")
        || !provider
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(NczError::Usage(format!(
            "invalid provider name: {provider}"
        )));
    }
    Ok(())
}

fn parse_provider_binding_value(value: &str) -> Option<(&str, &str)> {
    let (key_env, url) = value.split_once(' ')?;
    if key_env.is_empty() || url.is_empty() || url.contains(' ') {
        return None;
    }
    Some((key_env, url))
}

fn read_path(path: &Path) -> Result<Vec<AgentEnvEntry>, NczError> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(NczError::Io(e)),
    };

    let mut entries: Vec<AgentEnvEntry> = Vec::new();
    for line in body.lines() {
        let Some((key, value)) = parse_assignment(line)? else {
            continue;
        };
        if let Some(entry) = entries.iter_mut().find(|entry| entry.key == key) {
            entry.value = value;
        } else {
            entries.push(AgentEnvEntry { key, value });
        }
    }
    Ok(entries)
}

pub fn set_path(path: &Path, key: &str, value: &str) -> Result<bool, NczError> {
    validate_key(key)?;
    validate_value(value)?;
    let body = read_to_string_or_empty(path)?;
    let assignment = format_assignment(key, value);
    let mut changed = false;
    let mut wrote_key = false;
    let mut lines = Vec::new();

    for line in body.lines() {
        if line_key(line) == Some(key) {
            if !wrote_key {
                if line != assignment {
                    changed = true;
                }
                lines.push(assignment.clone());
                wrote_key = true;
            } else {
                changed = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }

    if !wrote_key {
        lines.push(assignment);
        changed = true;
    }

    let mut out = lines.join("\n");
    out.push('\n');
    if changed || body != out {
        state::atomic_write(path, out.as_bytes(), 0o600)?;
        return Ok(true);
    }
    Ok(false)
}

fn remove_path(path: &Path, key: &str) -> Result<bool, NczError> {
    validate_key(key)?;
    let body = read_to_string_or_empty(path)?;
    if body.is_empty() {
        return Ok(false);
    }

    let mut removed = false;
    let mut lines = Vec::new();
    for line in body.lines() {
        if line_key(line) == Some(key) {
            removed = true;
        } else {
            lines.push(line.to_string());
        }
    }

    if removed {
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        state::atomic_write(path, out.as_bytes(), 0o600)?;
    }
    Ok(removed)
}

fn read_to_string_or_empty(path: &Path) -> Result<String, NczError> {
    match fs::read_to_string(path) {
        Ok(body) => Ok(body),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(NczError::Io(e)),
    }
}

pub fn validate_value(value: &str) -> Result<(), NczError> {
    if value.contains('\0') || value.contains('\n') || value.contains('\r') {
        return Err(NczError::Usage(
            "environment values cannot contain NUL or newline characters".to_string(),
        ));
    }
    Ok(())
}

pub fn parse_environment_file_value(raw: &str) -> Result<String, NczError> {
    parse_value(raw)
}

fn is_safe_unquoted_value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'_' | b'.' | b'/' | b'+' | b'=' | b':' | b'@' | b'%' | b'-'
        )
}

fn parse_assignment(line: &str) -> Result<Option<(String, String)>, NczError> {
    let assignment = normalized_assignment(line);
    if assignment.is_empty() || assignment.starts_with('#') || assignment.starts_with(';') {
        return Ok(None);
    }
    let Some((key, raw_value)) = assignment.split_once('=') else {
        return Ok(None);
    };
    let key = key.trim();
    if validate_key(key).is_err() {
        return Ok(None);
    }
    let value = parse_value(raw_value)?;
    validate_value(&value)?;
    Ok(Some((key.to_string(), value)))
}

fn parse_value(raw: &str) -> Result<String, NczError> {
    let raw = raw.trim_start();
    if let Some(rest) = raw.strip_prefix('"') {
        return parse_double_quoted(rest);
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        return parse_single_quoted(rest);
    }
    Ok(parse_unquoted(raw.trim_end()))
}

fn parse_unquoted(raw: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '#'
            && (out.is_empty() || out.chars().last().is_some_and(char::is_whitespace))
        {
            while out.chars().last().is_some_and(char::is_whitespace) {
                out.pop();
            }
            break;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

fn parse_double_quoted(raw: &str) -> Result<String, NczError> {
    let mut out = String::new();
    let mut escaped = false;
    for (idx, ch) in raw.char_indices() {
        if escaped {
            if ch == 'n' {
                out.push('\n');
            } else if matches!(ch, '"' | '\\' | '$' | '`') {
                out.push(ch);
            } else {
                out.push('\\');
                out.push(ch);
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            ensure_trailing_ignorable(&raw[idx + ch.len_utf8()..])?;
            return Ok(out);
        } else {
            out.push(ch);
        }
    }
    Err(NczError::Usage(
        "unterminated double-quoted environment value".to_string(),
    ))
}

fn parse_single_quoted(raw: &str) -> Result<String, NczError> {
    let Some(idx) = raw.find('\'') else {
        return Err(NczError::Usage(
            "unterminated single-quoted environment value".to_string(),
        ));
    };
    ensure_trailing_ignorable(&raw[idx + 1..])?;
    Ok(raw[..idx].to_string())
}

fn ensure_trailing_ignorable(rest: &str) -> Result<(), NczError> {
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with('#') || rest.starts_with(';') {
        Ok(())
    } else {
        Err(NczError::Usage(
            "unexpected characters after quoted environment value".to_string(),
        ))
    }
}

fn format_assignment(key: &str, value: &str) -> String {
    if value.bytes().all(is_safe_unquoted_value_byte) {
        return format!("{key}={value}");
    }
    format!("{key}=\"{}\"", escape_double_quoted(value))
}

fn escape_double_quoted(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' | '\\' | '$' | '`' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn line_key(line: &str) -> Option<&str> {
    let assignment = normalized_assignment(line);
    if assignment.is_empty() || assignment.starts_with('#') || assignment.starts_with(';') {
        return None;
    }
    let (key, _) = assignment.split_once('=')?;
    let key = key.trim();
    validate_key(key).ok()?;
    Some(key)
}

fn normalized_assignment(line: &str) -> &str {
    line.trim_start()
        .strip_prefix("export ")
        .unwrap_or_else(|| line.trim_start())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::state::Paths;

    use super::*;

    fn test_paths(root: &std::path::Path) -> Paths {
        Paths {
            etc_dir: root.join("etc/nclawzero"),
            quadlet_dir: root.join("etc/containers/systemd"),
            lock_path: root.join("run/nclawzero.lock"),
        }
    }

    #[test]
    fn set_replaces_existing_key_and_removes_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.agent_env(),
            "FIRST=1\nTOGETHER_API_KEY=old\n# comment\nTOGETHER_API_KEY=older\n",
        )
        .unwrap();

        let replaced = set(&paths, "TOGETHER_API_KEY", "new").unwrap();

        assert!(replaced);
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "FIRST=1\nTOGETHER_API_KEY=new\n# comment\n"
        );
    }

    #[test]
    fn remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "A=1\n").unwrap();

        assert!(!remove(&paths, "B").unwrap());
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "A=1\n");
    }

    #[test]
    fn redacted_list_masks_values_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\n").unwrap();

        let entries = redacted_list(&paths, false).unwrap();

        assert_eq!(entries[0].key, "TOGETHER_API_KEY");
        assert_eq!(entries[0].value, "***");
        assert!(entries[0].set);
    }

    #[test]
    fn read_parses_systemd_quoted_values() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.agent_env(),
            "TOKEN=\"opaque token \\\"with quotes\\\" and \\\\ slash\" # comment\nOTHER='single quoted value'\nWEIRD=\"keep \\! bang\"\n",
        )
        .unwrap();

        let entries = read(&paths).unwrap();

        assert_eq!(entries[0].key, "TOKEN");
        assert_eq!(
            entries[0].value,
            "opaque token \"with quotes\" and \\ slash"
        );
        assert_eq!(entries[1].key, "OTHER");
        assert_eq!(entries[1].value, "single quoted value");
        assert_eq!(entries[2].key, "WEIRD");
        assert_eq!(entries[2].value, "keep \\! bang");
    }

    #[test]
    fn set_quotes_opaque_secret_values() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();

        set(&paths, "TOKEN", "secret token\"\\$`!").unwrap();

        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOKEN=\"secret token\\\"\\\\\\$\\`!\"\n"
        );
        assert_eq!(read(&paths).unwrap()[0].value, "secret token\"\\$`!");
    }

    #[test]
    fn set_rejects_newline_and_nul_values() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();

        for value in ["secret\ntoken", "secret\rtoken", "secret\0token"] {
            let err = set(&paths, "TOKEN", value).unwrap_err();
            assert!(matches!(err, NczError::Usage(_)));
        }
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn read_uses_last_duplicate_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOKEN=old\nOTHER=1\nexport TOKEN=\n").unwrap();

        let entries = read(&paths).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "TOKEN");
        assert_eq!(entries[0].value, "");
        assert_eq!(entries[1].key, "OTHER");
    }
}
