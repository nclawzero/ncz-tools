//! MCP server declaration state.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::NczError;
use crate::state::{self, url as url_state, Paths};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpDeclaration {
    pub schema_version: u32,
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    pub url: Option<String>,
    pub auth_env: Option<String>,
}

#[derive(Debug, Clone)]
pub struct McpRecord {
    pub declaration: McpDeclaration,
    pub path: PathBuf,
}

pub fn read_all(paths: &Paths) -> Result<Vec<McpRecord>, NczError> {
    let mut records = Vec::new();
    let mut seen = BTreeMap::new();
    for path in sorted_files(&paths.mcp_dir())? {
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let body = fs::read_to_string(&path)?;
        let declaration: McpDeclaration = serde_json::from_str(&body)?;
        validate_declaration(&declaration)?;
        let stem = file_stem(&path);
        if stem != declaration.name {
            return Err(NczError::Precondition(format!(
                "MCP declaration filename {} does not match declared name {}",
                path.display(),
                declaration.name
            )));
        }
        if let Some(previous) = seen.insert(declaration.name.clone(), path.clone()) {
            return Err(NczError::Precondition(format!(
                "duplicate MCP declaration {} in {} and {}",
                declaration.name,
                previous.display(),
                path.display()
            )));
        }
        records.push(McpRecord { declaration, path });
    }
    records.sort_by(|a, b| a.declaration.name.cmp(&b.declaration.name));
    Ok(records)
}

pub fn read(paths: &Paths, name: &str) -> Result<Option<McpRecord>, NczError> {
    validate_name(name)?;
    Ok(read_all(paths)?
        .into_iter()
        .find(|record| record.declaration.name == name))
}

pub fn write(paths: &Paths, declaration: &McpDeclaration) -> Result<(), NczError> {
    validate_declaration(declaration)?;
    if !matching_files(paths, &declaration.name)?.is_empty() {
        return Err(NczError::Usage(format!(
            "MCP declaration already exists: {}",
            declaration.name
        )));
    }
    let path = declaration_path(paths, &declaration.name)?;
    write_declaration(&path, declaration)
}

pub fn remove(paths: &Paths, name: &str) -> Result<bool, NczError> {
    validate_name(name)?;
    let mut removed = false;
    for path in matching_files(paths, name)? {
        removed = true;
        state::remove_file_durable(&path)?;
    }
    Ok(removed)
}

pub fn auth_references(paths: &Paths, key: &str) -> Result<Vec<String>, NczError> {
    crate::state::agent_env::validate_key(key)?;
    let mut references: Vec<String> = read_all(paths)?
        .into_iter()
        .filter(|record| record.declaration.auth_env.as_deref() == Some(key))
        .map(|record| record.declaration.name)
        .collect();
    references.sort();
    references.dedup();
    Ok(references)
}

pub fn declaration_path(paths: &Paths, name: &str) -> Result<PathBuf, NczError> {
    validate_name(name)?;
    Ok(paths.mcp_dir().join(format!("{name}.json")))
}

pub fn validate_declaration(declaration: &McpDeclaration) -> Result<(), NczError> {
    if declaration.schema_version != 1 {
        return Err(NczError::Usage(format!(
            "unsupported MCP schema_version: {}",
            declaration.schema_version
        )));
    }
    validate_name(&declaration.name)?;
    match declaration.transport.as_str() {
        "stdio" => {
            let command = declaration.command.as_deref().unwrap_or("").trim();
            if command.is_empty() {
                return Err(NczError::Usage(
                    "stdio MCP declarations require --command".to_string(),
                ));
            }
            reject_inline_secret_command(command)?;
            if declaration.url.is_some() {
                return Err(NczError::Usage(
                    "stdio MCP declarations cannot also set --url".to_string(),
                ));
            }
        }
        "http" => {
            let url = declaration.url.as_deref().unwrap_or("").trim();
            if url.is_empty() {
                return Err(NczError::Usage(
                    "http MCP declarations require --url".to_string(),
                ));
            }
            validate_http_url(url)?;
            if declaration.auth_env.is_some() {
                reject_insecure_auth_url(url)?;
            }
            if declaration.command.is_some() {
                return Err(NczError::Usage(
                    "http MCP declarations cannot also set --command".to_string(),
                ));
            }
        }
        other => {
            return Err(NczError::Usage(format!(
                "invalid MCP transport: {other} (expected stdio or http)"
            )));
        }
    }
    if let Some(auth_env) = &declaration.auth_env {
        crate::state::agent_env::validate_key(auth_env)?;
    }
    Ok(())
}

fn reject_inline_secret_command(command: &str) -> Result<(), NczError> {
    let mut next_arg_is_credential = false;
    for token in shell_words(command)? {
        if next_arg_is_credential
            || is_secret_command_token(&token)
            || is_user_credential_equals_flag(&token)
            || is_inline_user_pass_token(&token)
        {
            return Err(NczError::Usage(
                "stdio MCP commands cannot include inline credentials; use --auth-env".to_string(),
            ));
        }
        next_arg_is_credential = is_user_credential_next_arg_flag(&token);
    }
    Ok(())
}

fn shell_words(command: &str) -> Result<Vec<String>, NczError> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut in_token = false;
    for ch in command.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            in_token = true;
        } else if quote == Some('\'') {
            if ch == '\'' {
                quote = None;
            } else {
                token.push(ch);
            }
        } else if quote == Some('"') {
            if ch == '"' {
                quote = None;
            } else if ch == '\\' {
                escaped = true;
            } else {
                token.push(ch);
            }
        } else if ch.is_whitespace() {
            if in_token {
                tokens.push(std::mem::take(&mut token));
                in_token = false;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            in_token = true;
        } else if ch == '\\' {
            escaped = true;
            in_token = true;
        } else {
            token.push(ch);
            in_token = true;
        }
    }
    if escaped || quote.is_some() {
        return Err(NczError::Usage(
            "stdio MCP command has invalid shell quoting".to_string(),
        ));
    }
    if in_token {
        tokens.push(token);
    }
    Ok(tokens)
}

fn is_user_credential_next_arg_flag(token: &str) -> bool {
    matches!(token, "-u" | "--user" | "--proxy-user")
}

fn is_user_credential_equals_flag(token: &str) -> bool {
    ["--user=", "--proxy-user="].iter().any(|flag| {
        token
            .strip_prefix(flag)
            .is_some_and(|value| !value.is_empty())
    })
}

fn is_inline_user_pass_token(token: &str) -> bool {
    token.contains(':') && !token.contains("://")
}

fn is_secret_command_token(raw_token: &str) -> bool {
    let lower = raw_token.to_ascii_lowercase();
    let token = lower.trim_matches(|ch| ch == '"' || ch == '\'');
    if is_header_flag(raw_token, token) {
        return true;
    }
    if contains_secret_header(token) {
        return true;
    }
    if let Some((name, value)) = token.split_once('=') {
        return (!value.is_empty() && is_secret_command_name(name))
            || contains_secret_header(value);
    }
    if let Some((name, value)) = token.split_once(':') {
        return !value.is_empty() && is_secret_command_name(name);
    }
    if token == "bearer" {
        return true;
    }
    if let Some(flag) = token.strip_prefix("--") {
        return is_secret_command_name(flag);
    }
    false
}

fn is_header_flag(raw_token: &str, token: &str) -> bool {
    raw_token.starts_with("-H") || token == "--header" || token.starts_with("--header=")
}

fn contains_secret_header(token: &str) -> bool {
    [
        "authorization:",
        "x-api-key:",
        "x-key:",
        "api-key:",
        "apikey:",
    ]
    .iter()
    .any(|marker| token.contains(marker))
}

fn is_secret_command_name(name: &str) -> bool {
    let name = name
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "auth"
            | "api"
            | "access"
            | "token"
            | "key"
            | "secret"
            | "password"
            | "credential"
            | "credentials"
            | "authorization"
            | "bearer"
    ) || name.contains("token")
        || name.contains("auth")
        || name.contains("access")
        || name.contains("credential")
        || name.contains("secret")
        || name.contains("password")
        || name.contains("authorization")
        || name == "apikey"
        || name.contains("api_key")
        || name.contains("api-key")
        || name.ends_with("_key")
        || name.ends_with("-key")
}

fn reject_insecure_auth_url(url: &str) -> Result<(), NczError> {
    if !url.to_ascii_lowercase().starts_with("http://") {
        return Ok(());
    }
    let Some(host) = url_state::host(url) else {
        return Err(NczError::Usage(format!("invalid MCP http URL: {url}")));
    };
    if url_state::is_loopback_host(host) {
        return Ok(());
    }
    Err(NczError::Usage(format!(
        "credentialed MCP http URL must use https or loopback: {url}"
    )))
}

fn validate_http_url(url: &str) -> Result<(), NczError> {
    if url.starts_with('-')
        || !(url.starts_with("http://") || url.starts_with("https://"))
        || url.chars().any(char::is_whitespace)
    {
        return Err(NczError::Usage(format!(
            "invalid MCP http URL: {url} (expected http or https)"
        )));
    }
    if url_state::authority(url).is_none() {
        return Err(NczError::Usage(format!("invalid MCP http URL: {url}")));
    }
    if url_state::has_userinfo(url) {
        return Err(NczError::Usage(
            "MCP http URL cannot include userinfo; use --auth-env for credentials".to_string(),
        ));
    }
    if url_state::has_query_or_fragment(url) {
        return Err(NczError::Usage(
            "MCP http URL cannot include query strings or fragments; use --auth-env for credentials"
                .to_string(),
        ));
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<(), NczError> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains("..")
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(NczError::Usage(format!("invalid MCP name: {name}")));
    }
    Ok(())
}

fn write_declaration(path: &Path, declaration: &McpDeclaration) -> Result<(), NczError> {
    let body = serde_json::to_vec_pretty(declaration)?;
    let mut body_with_newline = body;
    body_with_newline.push(b'\n');
    let mode = if declaration.transport == "stdio" {
        0o600
    } else {
        0o644
    };
    state::atomic_write(path, &body_with_newline, mode)
}

fn matching_files(paths: &Paths, name: &str) -> Result<Vec<PathBuf>, NczError> {
    validate_name(name)?;
    let mut files = Vec::new();
    for path in sorted_files(&paths.mcp_dir())? {
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let stem = file_stem(&path);
        if stem == name {
            files.push(path);
            continue;
        }
        let body = fs::read_to_string(&path)?;
        let declaration: McpDeclaration = serde_json::from_str(&body)?;
        if declaration.name == name {
            files.push(path);
        }
    }
    Ok(files)
}

fn file_stem(path: &std::path::Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn sorted_files(dir: &std::path::Path) -> Result<Vec<PathBuf>, NczError> {
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(NczError::Io(e)),
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

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
    fn write_and_read_mcp_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let declaration = McpDeclaration {
            schema_version: 1,
            name: "filesystem".to_string(),
            transport: "stdio".to_string(),
            command: Some("npx -y @modelcontextprotocol/server-filesystem /srv".to_string()),
            url: None,
            auth_env: None,
        };

        write(&paths, &declaration).unwrap();

        let records = read_all(&paths).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration, declaration);
        assert_eq!(
            fs::metadata(paths.mcp_dir().join("filesystem.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();

        assert!(!remove(&paths, "missing").unwrap());
    }

    #[test]
    fn remove_deletes_matching_declared_name_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("old.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":null}"#,
        )
        .unwrap();

        assert!(remove(&paths, "search").unwrap());
        assert!(!paths.mcp_dir().join("old.json").exists());
    }

    #[test]
    fn read_all_rejects_filename_name_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("old.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":null}"#,
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();
        assert!(matches!(err, NczError::Precondition(_)));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 2,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("https://mcp.example.test".to_string()),
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_path_traversal_names() {
        let err = validate_name("../bad").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_non_http_urls_for_http_transport() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("file:///tmp/search".to_string()),
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_stdio_commands_with_inline_secret_flags() {
        for command in [
            "mcp-server --token secret",
            "mcp-server --api-key=secret",
            "env TOKEN=secret mcp-server",
            "env MCP_TOKEN=secret mcp-server",
            "OPENAI_API_KEY=secret mcp-server",
            "AUTH=secret mcp-server",
            "mcp-server --auth sk-live",
            "mcp-server --access sk-live",
            "mcp-server --api sk-live",
            "mcp-server \"--token=secret\"",
            "mcp-server -H Authorization:Bearer-secret",
            "mcp-server -H 'X-Key: sk-live'",
            "mcp-server -H 'Authorization: Basic abc'",
            "mcp-server --header 'Authorization: Basic abc'",
            "mcp-server --header=x-api-key:secret",
            "mcp-server 'Authorization: Basic abc'",
        ] {
            let err = validate_declaration(&McpDeclaration {
                schema_version: 1,
                name: "search".to_string(),
                transport: "stdio".to_string(),
                command: Some(command.to_string()),
                url: None,
                auth_env: None,
            })
            .unwrap_err();

            assert!(matches!(err, NczError::Usage(_)));
        }
    }

    #[test]
    fn test_reject_user_flag_with_next_arg_pair() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "stdio".to_string(),
            command: Some("mcp-server --user alice:secret".to_string()),
            url: None,
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn test_reject_user_flag_with_equals_form() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "stdio".to_string(),
            command: Some("mcp-server --user=alice:secret".to_string()),
            url: None,
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn test_reject_short_u_flag() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "stdio".to_string(),
            command: Some("mcp-server -u alice:secret".to_string()),
            url: None,
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn test_reject_inline_user_pass_token() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "stdio".to_string(),
            command: Some("mcp-server alice:secret".to_string()),
            url: None,
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_http_urls_with_userinfo() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("https://token@mcp.example.test".to_string()),
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_http_urls_with_query_credentials() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("https://mcp.example.test/sse?token=secret".to_string()),
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_http_urls_with_fragment_credentials() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("https://mcp.example.test/sse#token=secret".to_string()),
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_auth_bearing_remote_plaintext_http() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("http://mcp.example.test".to_string()),
            auth_env: Some("MCP_TOKEN".to_string()),
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_auth_bearing_127_prefixed_hostname() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("http://127.attacker.example".to_string()),
            auth_env: Some("MCP_TOKEN".to_string()),
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn allows_auth_bearing_loopback_plaintext_http() {
        validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "local-search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("http://127.0.0.1:5555".to_string()),
            auth_env: Some("MCP_TOKEN".to_string()),
        })
        .unwrap();
    }
}
