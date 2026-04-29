//! MCP server declaration state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::NczError;
use crate::state::{self, url as url_state, Paths};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
        let record = read_record(&path)?;
        let declaration = &record.declaration;
        if let Some(previous) = seen.insert(declaration.name.clone(), path.clone()) {
            return Err(NczError::Precondition(format!(
                "duplicate MCP declaration {} in {} and {}",
                declaration.name,
                previous.display(),
                path.display()
            )));
        }
        records.push(record);
    }
    records.sort_by(|a, b| a.declaration.name.cmp(&b.declaration.name));
    Ok(records)
}

pub fn read(paths: &Paths, name: &str) -> Result<Option<McpRecord>, NczError> {
    validate_name(name)?;
    let mut records = Vec::new();
    for path in matching_files(paths, name)? {
        records.push(read_record(&path)?);
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    if records.len() > 1 {
        let first = &records[0];
        let second = &records[1];
        return Err(NczError::Precondition(format!(
            "duplicate MCP declaration {} in {} and {}",
            name,
            first.path.display(),
            second.path.display()
        )));
    }
    Ok(records.into_iter().next())
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
    let removal_paths = matching_files(paths, name)?;
    let removed = !removal_paths.is_empty();
    remove_paths_with_rollback(&removal_paths, state::remove_file_durable)?;
    Ok(removed)
}

pub(crate) fn removal_paths(paths: &Paths, name: &str) -> Result<Vec<PathBuf>, NczError> {
    matching_files(paths, name)
}

pub(crate) fn removal_aliases(paths: &Paths, name: &str) -> Result<Vec<String>, NczError> {
    validate_name(name)?;
    let mut aliases = BTreeSet::new();
    for path in matching_files(paths, name)? {
        let stem = file_stem(&path);
        if validate_name(&stem).is_ok() {
            aliases.insert(stem);
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(declaration) = serde_json::from_str::<McpDeclaration>(&body) else {
            continue;
        };
        if validate_name(&declaration.name).is_ok() {
            aliases.insert(declaration.name);
        }
    }
    Ok(aliases.into_iter().collect())
}

pub fn auth_references(paths: &Paths, key: &str) -> Result<Vec<String>, NczError> {
    crate::state::agent_env::validate_public_key(key)?;
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
            let url = declaration.url.as_deref().unwrap_or("");
            if url_state::has_userinfo(url.trim()) {
                return Err(NczError::Usage(
                    "MCP http URL cannot include userinfo; use --auth-env for credentials"
                        .to_string(),
                ));
            }
            if url != url.trim() {
                return Err(NczError::Usage(
                    "MCP http URL cannot contain surrounding whitespace".to_string(),
                ));
            }
            if url.is_empty() {
                return Err(NczError::Usage(
                    "http MCP declarations require --url".to_string(),
                ));
            }
            validate_http_url(url)?;
            reject_remote_plaintext_http_url(url)?;
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
        crate::state::agent_env::validate_public_key(auth_env)?;
    }
    Ok(())
}

fn read_record(path: &Path) -> Result<McpRecord, NczError> {
    let body = fs::read_to_string(path)?;
    let declaration: McpDeclaration = serde_json::from_str(&body).map_err(|err| {
        NczError::Precondition(format!("invalid MCP declaration {}: {err}", path.display()))
    })?;
    validate_declaration(&declaration).map_err(|err| {
        NczError::Precondition(format!("invalid MCP declaration {}: {err}", path.display()))
    })?;
    let stem = file_stem(path);
    if stem != declaration.name {
        return Err(NczError::Precondition(format!(
            "MCP declaration filename {} does not match declared name {}",
            path.display(),
            declaration.name
        )));
    }
    Ok(McpRecord {
        declaration,
        path: path.to_path_buf(),
    })
}

fn reject_inline_secret_command(command: &str) -> Result<(), NczError> {
    let args = split_stdio_command_args(command)?;
    reject_inline_secret_tokens(&args, 0)
}

fn reject_inline_secret_tokens(args: &[String], depth: u8) -> Result<(), NczError> {
    for (idx, token) in args.iter().enumerate() {
        if is_shell_command_invocation(args, idx) {
            return Err(NczError::Usage(
                "stdio MCP commands cannot invoke shell -c; use a direct command and --auth-env"
                    .to_string(),
            ));
        }
        if contains_shell_expansion(token) {
            return Err(NczError::Usage(
                "stdio MCP commands cannot include shell expansions; use a direct command and --auth-env"
                    .to_string(),
            ));
        }
        if is_secret_command_token(token) {
            return Err(NczError::Usage(
                "stdio MCP commands cannot include inline credentials; use --auth-env".to_string(),
            ));
        }
        if is_env_injection_token(token) {
            return Err(NczError::Usage(
                "stdio MCP commands cannot inject inline environment values; use --auth-env"
                    .to_string(),
            ));
        }
        if is_secret_env_name_token(token)
            && args
                .get(idx + 1)
                .is_some_and(|next| !next.starts_with('-'))
        {
            return Err(NczError::Usage(
                "stdio MCP commands cannot include inline credentials; use --auth-env".to_string(),
            ));
        }
        if depth < 4 && token.chars().any(char::is_whitespace) {
            let nested = split_stdio_command_args(token)?;
            if nested.len() > 1 || nested.first().is_some_and(|arg| arg != token) {
                reject_inline_secret_tokens(&nested, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn is_shell_command_invocation(args: &[String], idx: usize) -> bool {
    is_shell_interpreter(&args[idx])
        && args
            .iter()
            .skip(idx + 1)
            .any(|token| is_shell_command_flag(token))
}

fn is_shell_interpreter(raw_token: &str) -> bool {
    let token = raw_token.trim_matches(|ch: char| ch == '"' || ch == '\'');
    let name = Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "mksh" | "ash"
    )
}

fn is_shell_command_flag(raw_token: &str) -> bool {
    let token = raw_token.trim_matches(|ch: char| ch == '"' || ch == '\'');
    if token == "-c" || token.starts_with("-c") {
        return true;
    }
    token.starts_with('-') && !token.starts_with("--") && token[1..].contains('c')
}

fn contains_shell_expansion(raw_token: &str) -> bool {
    let token = raw_token.trim_matches(|ch: char| ch == '"' || ch == '\'');
    token.contains('$') || token.contains('`')
}

fn split_stdio_command_args(command: &str) -> Result<Vec<String>, NczError> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars();
    let mut in_single = false;
    let mut in_double = false;
    let mut saw_arg = false;

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            saw_arg = true;
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            } else if ch == '\\' {
                if let Some(next) = chars.next() {
                    current.push(next);
                } else {
                    current.push('\\');
                }
            } else {
                current.push(ch);
            }
            saw_arg = true;
            continue;
        }

        if ch.is_whitespace() {
            if saw_arg {
                args.push(std::mem::take(&mut current));
                saw_arg = false;
            }
        } else if ch == '\'' {
            in_single = true;
            saw_arg = true;
        } else if ch == '"' {
            in_double = true;
            saw_arg = true;
        } else if ch == '\\' {
            if let Some(next) = chars.next() {
                current.push(next);
            } else {
                current.push('\\');
            }
            saw_arg = true;
        } else {
            current.push(ch);
            saw_arg = true;
        }
    }

    if in_single || in_double {
        return Err(NczError::Usage(
            "unterminated quote in stdio MCP command".to_string(),
        ));
    }
    if saw_arg {
        args.push(current);
    }
    Ok(args)
}

fn is_secret_command_token(raw_token: &str) -> bool {
    let lower = raw_token.to_ascii_lowercase();
    let token = lower.as_str();
    if contains_user_pass_form(token) {
        return true;
    }
    if contains_secret_url_token(raw_token) {
        return true;
    }
    if is_header_flag(raw_token, token) {
        return true;
    }
    if is_secret_header_name_token(token) {
        return true;
    }
    if contains_secret_header(token) {
        return true;
    }
    if token == "-u" || token.starts_with("-u=") || token.starts_with("-u:") {
        return true;
    }
    if token.starts_with("-u") && token.len() > 2 && !token.starts_with("--") {
        return true;
    }
    if is_secret_short_flag(token) {
        return true;
    }
    if let Some((name, value)) = token.split_once('=') {
        if !value.is_empty() && crate::state::agent_env::validate_key(name).is_ok() {
            return true;
        }
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
    if looks_like_inline_secret_value(raw_token) {
        return true;
    }
    false
}

fn is_env_injection_token(raw_token: &str) -> bool {
    let token = raw_token
        .trim_matches(|ch: char| ch == '"' || ch == '\'')
        .to_ascii_lowercase();
    matches!(
        token.as_str(),
        "env" | "-e" | "--env" | "--set-env" | "--set_env" | "--env-file" | "--env_file"
    ) || token.starts_with("-e=")
        || (token.starts_with("-e") && token.len() > 2 && !token.starts_with("--"))
        || token.starts_with("--env=")
        || token.starts_with("--set-env=")
        || token.starts_with("--set_env=")
        || token.starts_with("--env-file=")
        || token.starts_with("--env_file=")
}

fn is_secret_env_name_token(raw_token: &str) -> bool {
    let token = raw_token.trim_matches(|ch: char| ch == '"' || ch == '\'');
    crate::state::agent_env::validate_key(token).is_ok() && is_secret_command_name(token)
}

fn contains_secret_url_token(raw_token: &str) -> bool {
    let mut remaining = raw_token;
    while let Some(start) = find_url_start(remaining) {
        let url_and_tail = &remaining[start..];
        let end = url_and_tail
            .find(|ch: char| {
                ch.is_whitespace() || matches!(ch, '"' | '\'' | ')' | ']' | '}' | '<')
            })
            .unwrap_or(url_and_tail.len());
        let candidate = trim_url_token(&url_and_tail[..end]);
        if url_has_credential_material(candidate) {
            return true;
        }
        if end >= url_and_tail.len() {
            return false;
        }
        remaining = &url_and_tail[end..];
    }
    false
}

fn find_url_start(value: &str) -> Option<usize> {
    let lower = value.to_ascii_lowercase();
    match (lower.find("http://"), lower.find("https://")) {
        (Some(http), Some(https)) => Some(http.min(https)),
        (Some(http), None) => Some(http),
        (None, Some(https)) => Some(https),
        (None, None) => None,
    }
}

fn trim_url_token(token: &str) -> &str {
    token.trim_matches(|ch: char| matches!(ch, ',' | '.' | ':' | '>' | ';'))
}

fn url_has_credential_material(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && (url_state::has_userinfo(url)
            || url_state::has_query_or_fragment(url)
            || url_state::contains_secret_path_material(url))
}

fn is_secret_short_flag(token: &str) -> bool {
    if token.starts_with("--") {
        return false;
    }
    for flag in ["-a", "-k", "-p", "-t"] {
        if token == flag {
            return true;
        }
        if let Some(value) = token.strip_prefix(flag) {
            return !value.is_empty();
        }
    }
    false
}

fn looks_like_inline_secret_value(raw_token: &str) -> bool {
    let token = raw_token.trim_matches(|ch: char| {
        ch == '"' || ch == '\'' || ch == ',' || ch == '[' || ch == ']' || ch == '{' || ch == '}'
    });
    let lower = token.to_ascii_lowercase();
    lower.starts_with("sk-")
        || lower.starts_with("sk_")
        || lower.starts_with("ghp_")
        || lower.starts_with("github_pat_")
        || lower.starts_with("xoxb-")
        || lower.starts_with("xoxp-")
        || lower.starts_with("xoxa-")
        || (lower.starts_with("eyj") && token.matches('.').count() >= 2)
}

fn is_header_flag(raw_token: &str, token: &str) -> bool {
    raw_token.starts_with("-H")
        || matches!(
            token,
            "--header"
                | "--headers"
                | "--http-header"
                | "--http_headers"
                | "--http_header"
                | "--httpheader"
                | "--http-headers"
                | "--httpheaders"
                | "--request-header"
                | "--request_header"
                | "--requestheader"
                | "--request-headers"
                | "--request_headers"
                | "--requestheaders"
        )
        || token.starts_with("--header=")
        || token.starts_with("--headers=")
        || token.starts_with("--http-header=")
        || token.starts_with("--http_header=")
        || token.starts_with("--httpheader=")
        || token.starts_with("--http-headers=")
        || token.starts_with("--http_headers=")
        || token.starts_with("--httpheaders=")
        || token.starts_with("--request-header=")
        || token.starts_with("--request_header=")
        || token.starts_with("--requestheader=")
        || token.starts_with("--request-headers=")
        || token.starts_with("--request_headers=")
        || token.starts_with("--requestheaders=")
}

fn is_secret_header_name_token(token: &str) -> bool {
    let token = token
        .trim_matches(|ch: char| {
            ch == '"' || ch == '\'' || ch == ',' || ch == '[' || ch == ']' || ch == '{' || ch == '}'
        })
        .trim_end_matches(':');
    matches!(
        token,
        "authorization"
            | "proxy-authorization"
            | "proxy_authorization"
            | "x-api-key"
            | "x_api_key"
            | "x-key"
            | "x_key"
            | "api-key"
            | "api_key"
            | "apikey"
    ) || token.contains("api-key")
        || token.contains("api_key")
}

fn contains_secret_header(token: &str) -> bool {
    [
        "authorization:",
        "authorization\"",
        "authorization'",
        "x-api-key:",
        "x-api-key\"",
        "x-api-key'",
        "x-key:",
        "x-key\"",
        "x-key'",
        "api-key:",
        "api-key\"",
        "api-key'",
        "apikey:",
        "apikey\"",
        "apikey'",
    ]
    .iter()
    .any(|marker| token.contains(marker))
}

fn contains_user_pass_form(token: &str) -> bool {
    let value = token.rsplit_once('=').map_or(token, |(_, value)| value);
    if let Some((_, rest)) = value.split_once("://") {
        let authority = rest.split_once('/').map_or(rest, |(authority, _)| authority);
        return authority
            .split_once('@')
            .is_some_and(|(userinfo, _)| userinfo.split_once(':').is_some_and(has_nonempty_pair));
    }
    value.split_once(':').is_some_and(|pair| {
        has_nonempty_pair(pair) && !looks_like_host_port(pair.0, pair.1)
    })
}

fn has_nonempty_pair((left, right): (&str, &str)) -> bool {
    !left.is_empty() && !right.is_empty()
}

fn looks_like_host_port(left: &str, right: &str) -> bool {
    right.chars().all(|ch| ch.is_ascii_digit())
        && (left.contains('.') || left.eq_ignore_ascii_case("localhost"))
}

fn is_secret_command_name(name: &str) -> bool {
    let name = name
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .trim_start_matches('-')
        .to_ascii_lowercase();
    let compact: String = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    matches!(
        name.as_str(),
        "auth"
            | "api"
            | "access"
            | "token"
            | "key"
            | "secret"
            | "pass"
            | "passwd"
            | "pwd"
            | "passphrase"
            | "password"
            | "credential"
            | "credentials"
            | "authorization"
            | "bearer"
            | "headers"
            | "header-map"
            | "header_map"
            | "headermap"
            | "cookie"
            | "cookies"
            | "cookie-file"
            | "cookie_file"
            | "cookiefile"
            | "jwt"
            | "pat"
            | "session"
            | "session-token"
            | "session_token"
            | "user"
            | "proxy-user"
            | "proxy_user"
            | "proxyuser"
    ) || name.contains("token")
        || name.contains("auth")
        || name.contains("access")
        || name.contains("credential")
        || name.contains("jwt")
        || name.contains("secret")
        || name.contains("passphrase")
        || name.contains("password")
        || name.contains("cookie")
        || name.ends_with("_pass")
        || name.ends_with("-pass")
        || name.ends_with("_passwd")
        || name.ends_with("-passwd")
        || name.ends_with("_pwd")
        || name.ends_with("-pwd")
        || name.contains("authorization")
        || name.contains("session")
        || name == "pat"
        || name.ends_with("_pat")
        || name.ends_with("-pat")
        || name.contains("personal_access_token")
        || name == "apikey"
        || name.contains("api_key")
        || name.contains("api-key")
        || name.ends_with("_key")
        || name.ends_with("-key")
        || compact == "apikey"
        || compact.contains("apikey")
        || compact.contains("accesstoken")
        || compact.contains("bearertoken")
        || compact.contains("authorization")
        || compact.contains("credential")
        || compact.contains("password")
        || compact.contains("secret")
        || compact.contains("sessiontoken")
}

fn reject_remote_plaintext_http_url(url: &str) -> Result<(), NczError> {
    let trimmed = url.trim();
    if url_state::has_userinfo(trimmed) {
        return Err(NczError::Usage(
            "MCP http URL cannot include userinfo; use --auth-env for credentials".to_string(),
        ));
    }
    if url_state::has_query_or_fragment(trimmed) {
        return Err(NczError::Usage(
            "MCP http URL cannot include query strings or fragments; use --auth-env for credentials"
                .to_string(),
        ));
    }
    if url_state::contains_secret_path_material(trimmed) {
        return Err(NczError::Usage(
            "MCP http URL path cannot include credential-like material; use --auth-env for credentials"
                .to_string(),
        ));
    }
    if !url.to_ascii_lowercase().starts_with("http://") {
        return Ok(());
    }
    let Some(host) = url_state::host(url) else {
        return Err(NczError::Usage("invalid MCP http URL".to_string()));
    };
    if url_state::is_loopback_host(host) {
        return Ok(());
    }
    Err(NczError::Usage(
        "MCP http URL must use https or loopback".to_string(),
    ))
}

fn validate_http_url(url: &str) -> Result<(), NczError> {
    let trimmed = url.trim();
    if url_state::has_userinfo(trimmed) {
        return Err(NczError::Usage(
            "MCP http URL cannot include userinfo; use --auth-env for credentials".to_string(),
        ));
    }
    if url_state::has_query_or_fragment(trimmed) {
        return Err(NczError::Usage(
            "MCP http URL cannot include query strings or fragments; use --auth-env for credentials"
                .to_string(),
        ));
    }
    if url_state::contains_secret_path_material(trimmed) {
        return Err(NczError::Usage(
            "MCP http URL path cannot include credential-like material; use --auth-env for credentials"
                .to_string(),
        ));
    }
    if url.starts_with('-')
        || !(url.starts_with("http://") || url.starts_with("https://"))
        || url.chars().any(char::is_whitespace)
    {
        return Err(NczError::Usage(
            "invalid MCP http URL: expected http or https".to_string(),
        ));
    }
    if !url_state::has_valid_authority(url) {
        return Err(NczError::Usage("invalid MCP http URL".to_string()));
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
    let mode = if declaration.transport == "stdio" || declaration.auth_env.is_some() {
        0o600
    } else {
        0o644
    };
    state::atomic_write(path, &body_with_newline, mode)
}

fn remove_paths_with_rollback<F>(paths: &[PathBuf], mut remove_one: F) -> Result<(), NczError>
where
    F: FnMut(&Path) -> Result<(), NczError>,
{
    let snapshots = snapshot_files(paths)?;
    let result = (|| {
        for path in paths {
            remove_one(path)?;
        }
        Ok(())
    })();
    if let Err(err) = result {
        restore_file_snapshots(&snapshots)?;
        return Err(err);
    }
    Ok(())
}

struct FileSnapshot {
    path: PathBuf,
    body: Option<Vec<u8>>,
    mode: u32,
}

fn snapshot_files(paths: &[PathBuf]) -> Result<Vec<FileSnapshot>, NczError> {
    paths.iter().map(|path| snapshot_file(path)).collect()
}

fn snapshot_file(path: &Path) -> Result<FileSnapshot, NczError> {
    match fs::read(path) {
        Ok(body) => {
            let mode = fs::metadata(path)?.permissions().mode() & 0o777;
            Ok(FileSnapshot {
                path: path.to_path_buf(),
                body: Some(body),
                mode,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileSnapshot {
            path: path.to_path_buf(),
            body: None,
            mode: 0o644,
        }),
        Err(e) => Err(NczError::Io(e)),
    }
}

fn restore_file_snapshots(snapshots: &[FileSnapshot]) -> Result<(), NczError> {
    for snapshot in snapshots.iter().rev() {
        match &snapshot.body {
            Some(body) => state::atomic_write(&snapshot.path, body, snapshot.mode)?,
            None => state::remove_file_durable(&snapshot.path)?,
        }
    }
    Ok(())
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
        let declaration: McpDeclaration = match serde_json::from_str(&body) {
            Ok(declaration) => declaration,
            Err(_) => continue,
        };
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
    fn auth_bearing_http_declarations_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let declaration = McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("https://mcp.example.test/sse".to_string()),
            auth_env: Some("MCP_TOKEN".to_string()),
        };

        write(&paths, &declaration).unwrap();

        assert_eq!(
            fs::metadata(paths.mcp_dir().join("search.json"))
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
    fn remove_paths_with_rollback_restores_after_later_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        let first = paths.mcp_dir().join("first.json");
        let second = paths.mcp_dir().join("second.json");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        let mut calls = 0;

        let err = remove_paths_with_rollback(&[first.clone(), second.clone()], |path| {
            calls += 1;
            if calls == 2 {
                return Err(NczError::Precondition("simulated remove failure".to_string()));
            }
            crate::state::remove_file_durable(path)
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(fs::read_to_string(first).unwrap(), "first");
        assert_eq!(fs::read_to_string(second).unwrap(), "second");
    }

    #[test]
    fn write_ignores_unrelated_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(paths.mcp_dir().join("broken.json"), "{").unwrap();
        let declaration = McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("https://mcp.example.test".to_string()),
            auth_env: None,
        };

        write(&paths, &declaration).unwrap();

        assert!(paths.mcp_dir().join("search.json").exists());
        assert!(paths.mcp_dir().join("broken.json").exists());
    }

    #[test]
    fn remove_ignores_unrelated_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(paths.mcp_dir().join("broken.json"), "{").unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":null}"#,
        )
        .unwrap();

        assert!(remove(&paths, "search").unwrap());
        assert!(!paths.mcp_dir().join("search.json").exists());
        assert!(paths.mcp_dir().join("broken.json").exists());
    }

    #[test]
    fn read_ignores_unrelated_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(paths.mcp_dir().join("broken.json"), "{").unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":null}"#,
        )
        .unwrap();

        let record = read(&paths, "search").unwrap().unwrap();

        assert_eq!(record.declaration.name, "search");
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
    fn read_all_rejects_unknown_secret_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":null,"api_key":"secret"}"#,
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
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
    fn rejects_http_urls_with_surrounding_whitespace() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some(" https://mcp.example.test ".to_string()),
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_malformed_http_url_authorities() {
        for url in [
            "https://:443",
            "https://mcp.example.test:",
            "https://mcp.example.test:not-a-port",
            "https://mcp.example.test:65536",
            "https://[::1",
            "https://::1:443",
        ] {
            let err = validate_declaration(&McpDeclaration {
                schema_version: 1,
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some(url.to_string()),
                auth_env: None,
            })
            .unwrap_err();

            assert!(matches!(err, NczError::Usage(_)));
        }
    }

    #[test]
    fn rejects_stdio_commands_with_inline_secret_flags() {
        for command in [
            "mcp-server --token secret",
            "mcp-server --api-key=secret",
            "env TOKEN=secret mcp-server",
            "env MCP_TOKEN=secret mcp-server",
            "env MCP_TOKEN secret mcp-server",
            "env GITHUB_PAT=ghp_secret mcp-server",
            "env DEBUG=1 mcp-server",
            "mcp-server --env MCP_TOKEN secret",
            "mcp-server -e MCP_TOKEN secret",
            "mcp-server --set-env MCP_TOKEN secret",
            "mcp-server --env=MCP_TOKEN=secret",
            "mcp-server -eMCP_TOKEN=secret",
            "OPENAI_API_KEY=secret mcp-server",
            "GITHUB_PAT=ghp_secret mcp-server",
            "mcp-server OPENAI_API_KEY secret",
            "AUTH=secret mcp-server",
            "mcp-server --jwt eyJhbGciOiJIUzI1NiJ9",
            "mcp-server --session abc123",
            "mcp-server --auth sk-live",
            "mcp-server --access sk-live",
            "mcp-server --api sk-live",
            "mcp-server --api.key=hunter2",
            "mcp-server --api.key hunter2",
            "mcp-server --access.token abc123",
            "java -Dapi.key=hunter2 mcp-server",
            "java -Daccess.token=abc123 mcp-server",
            "mcp-server --pass hunter2",
            "mcp-server --pass=hunter2",
            "mcp-server --db-pass hunter2",
            "mcp-server --db-pass=hunter2",
            "mcp-server --passwd hunter2",
            "mcp-server --passwd=hunter2",
            "mcp-server --db-passwd hunter2",
            "mcp-server --db-passwd=hunter2",
            "mcp-server --pwd hunter2",
            "mcp-server --pwd=hunter2",
            "mcp-server --db-pwd hunter2",
            "mcp-server --db-pwd=hunter2",
            "mcp-server --passphrase hunter2",
            "mcp-server --passphrase=hunter2",
            "mcp-server -p hunter2",
            "mcp-server -phunter2",
            "mcp-server -t sk-live",
            "mcp-server -tsk-live",
            "mcp-server -k=sk-live",
            "mcp-server -a:sk-live",
            "mcp-server sk-live",
            "mcp-server ghp_1234567890abcdef",
            "mcp-server github_pat_1234567890abcdef",
            "mcp-server eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature",
            "mcp-server \"--token=secret\"",
            "mcp-server -H Authorization:Bearer-secret",
            "mcp-server -H 'X-Key: sk-live'",
            "mcp-server -H 'Authorization: Basic abc'",
            "mcp-server --header 'Authorization: Basic abc'",
            "mcp-server --header=x-api-key:secret",
            "mcp-proxy --http-header X-Api-Key abc123",
            "mcp-proxy --httpHeader X-Api-Key abc123",
            "mcp-proxy --headers X-Api-Key abc123",
            "mcp-proxy --request-header api-key abc123",
            "mcp-proxy X-Api-Key abc123",
            "mcp-proxy Authorization abc123",
            "mcp-proxy --headers={\"Authorization\":\"Bearer sk-live\"}",
            "mcp-proxy --headers='{\"X-API-Key\":\"sk-live\"}'",
            "mcp-proxy --cookie=SID=abc123",
            "mcp-proxy --cookie SID=abc123",
            "mcp-proxy --cookie-file /run/secrets/cookiejar",
            "mcp-proxy --session-cookie sid=abc123",
            "mcp-proxy --session_cookie=sid=abc123",
            "mcp-server 'Authorization: Basic abc'",
            "mcp-server --user alice:secret",
            "mcp-server --user 'alice:secret'",
            "mcp-server --user=alice:secret",
            "mcp-server --proxy-user alice:secret",
            "mcp-server --proxy-user=alice:secret",
            "mcp-server -u alice:secret",
            "mcp-server -ualice:secret",
            "mcp-server --profile alice:secret",
            "mcp-server \"--user=alice:secret\"",
            "mcp-server https://mcp.example.test/sse/sk-live",
            "mcp-server HTTPS://mcp.example.test/sse/sk-live",
            "mcp-server --endpoint=https://mcp.example.test/sse/sk-live",
            "mcp-server --endpoint=HtTpS://mcp.example.test/sse?token=secret",
            "mcp-server https://mcp.example.test/sse?token=secret",
            "mcp-server${IFS}--token${IFS}secret",
            "mcp-server $'--token secret'",
            "sh -c 'mcp-server --token secret'",
            "sh -c 'mcp-server${IFS}--token${IFS}secret'",
            "bash -lc $'mcp-server\\x20--token\\x20secret'",
            "sh -c 'mcp-server https://mcp.example.test/sse/sk-live'",
            "sh -c 'mcp-server HTTP://alice:secret@mcp.example.test'",
            "bash -c \"mcp-server --header 'Authorization: Bearer abc'\"",
            "zsh -c 'mcp-server https://alice:secret@mcp.example.test'",
            "dash -c 'TOKEN=secret mcp-server'",
        ] {
            let result = validate_declaration(&McpDeclaration {
                schema_version: 1,
                name: "search".to_string(),
                transport: "stdio".to_string(),
                command: Some(command.to_string()),
                url: None,
                auth_env: None,
            });

            assert!(
                matches!(result, Err(NczError::Usage(_))),
                "command was accepted: {command}"
            );
        }
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
    fn rejects_malformed_http_userinfo_without_echoing_secret() {
        for url in [
            "ftp://user:secret@mcp.example.test",
            " https://user:secret@mcp.example.test",
            "https://user:secret@:443",
        ] {
            let err = validate_declaration(&McpDeclaration {
                schema_version: 1,
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some(url.to_string()),
                auth_env: None,
            })
            .unwrap_err();
            let message = err.to_string();
            assert!(message.contains("userinfo"));
            assert!(!message.contains("secret"));
            assert!(!message.contains(url));
        }
    }

    #[test]
    fn rejects_malformed_http_credential_urls_without_echoing_secret() {
        for url in [
            "ftp://mcp.example.test/sse?token=secret",
            "https://mcp.example.test:notaport/sse?token=secret",
            " ftp://mcp.example.test/sse#token=secret",
            "https://mcp.example.test:notaport/token/secret",
            "ftp://mcp.example.test/sse/sk-live",
            "https://mcp.example.test:sk-live/sse",
            "api_key=secret",
            "sk-live",
        ] {
            let err = validate_declaration(&McpDeclaration {
                schema_version: 1,
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some(url.to_string()),
                auth_env: None,
            })
            .unwrap_err();
            let message = err.to_string();
            assert!(!message.contains("secret"));
            assert!(!message.contains("sk-live"));
            assert!(!message.contains(url));
        }
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
    fn rejects_http_urls_with_path_credentials() {
        for url in [
            "https://mcp.example.test/sse/token/sk-live",
            "https://mcp.example.test/sse;token=secret",
            "https://mcp.example.test/%74oken/secret",
            "https://mcp.example.test/api-key/secret",
            "https://mcp.example.test/sse/sk-live",
            "https://mcp.example.test/sse%2Fsk-live",
            "https://mcp.example.test/sse%3Btoken=secret",
        ] {
            let err = validate_declaration(&McpDeclaration {
                schema_version: 1,
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some(url.to_string()),
                auth_env: None,
            })
            .unwrap_err();

            assert!(
                matches!(err, NczError::Usage(_)),
                "URL was accepted: {url}"
            );
        }
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
    fn rejects_unauthenticated_remote_plaintext_http() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("http://mcp.example.test".to_string()),
            auth_env: None,
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_reserved_auth_env() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("https://mcp.example.test".to_string()),
            auth_env: Some("NCZ_PROVIDER_BINDING_736561726368".to_string()),
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
    fn rejects_unauthenticated_127_prefixed_hostname() {
        let err = validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("http://127.attacker.example".to_string()),
            auth_env: None,
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

    #[test]
    fn allows_unauthenticated_loopback_plaintext_http() {
        validate_declaration(&McpDeclaration {
            schema_version: 1,
            name: "local-search".to_string(),
            transport: "http".to_string(),
            command: None,
            url: Some("http://127.0.0.1:5555".to_string()),
            auth_env: None,
        })
        .unwrap();
    }
}
