//! sessions -- aggregate agent session stores from the operator layer.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::{Context, SessionsAction};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent, agent_env, providers as provider_state, url as url_state, Paths};
use crate::sys::systemd;

const SESSION_API_TIMEOUT_SECS: u64 = 5;
const SESSION_API_MAX_BYTES: usize = 8 * 1024 * 1024;
const ZEROCLAW_AGENT: &str = "zeroclaw";
const UNSUPPORTED_REASON: &str =
    "session listing not yet implemented in ncz v0.3 slice 1; deferred to v0.3.1";

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum SessionsReport {
    List(SessionsListReport),
    Show(SessionsShowReport),
    Export(SessionsExportReport),
    Prune(SessionsPruneReport),
}

impl Render for SessionsReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            SessionsReport::List(report) => report.render_text(w),
            SessionsReport::Show(report) => report.render_text(w),
            SessionsReport::Export(report) => report.render_text(w),
            SessionsReport::Prune(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub agent: String,
    pub workspace: String,
    pub last_modified: String,
    pub message_count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkippedAgent {
    pub agent: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct SessionsListReport {
    pub schema_version: u32,
    pub sessions: Vec<SessionSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_agents: Vec<SkippedAgent>,
}

#[derive(Debug, Serialize)]
pub struct SessionsShowReport {
    pub schema_version: u32,
    pub agent: String,
    pub session_id: String,
    pub session: Value,
    pub messages: Vec<Value>,
    pub metadata: Value,
}

#[derive(Debug, Serialize)]
pub struct SessionsExportReport {
    pub schema_version: u32,
    pub agent: String,
    pub session_id: String,
    pub path: String,
    pub bytes: u64,
    pub mode: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionExportBundle {
    pub schema_version: u32,
    pub exported_at: String,
    pub agent: String,
    pub session: Value,
    pub messages: Vec<Value>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PrunedSession {
    pub id: String,
    pub agent: String,
    pub last_modified: String,
    pub deleted: bool,
}

#[derive(Debug, Serialize)]
pub struct SessionsPruneReport {
    pub schema_version: u32,
    pub before: String,
    pub dry_run: bool,
    pub sessions: Vec<PrunedSession>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_agents: Vec<SkippedAgent>,
}

struct SessionContent {
    session: Value,
    messages: Vec<Value>,
    metadata: Value,
}

impl Render for SessionsListReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for session in &self.sessions {
            writeln!(
                w,
                "{:<12} {:<32} {:<24} messages={} workspace={}",
                session.agent,
                session.id,
                session.last_modified,
                session.message_count,
                session.workspace
            )?;
        }
        for skipped in &self.skipped_agents {
            writeln!(w, "{}: {}", skipped.agent, skipped.reason)?;
        }
        Ok(())
    }
}

impl Render for SessionsShowReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Agent:   {}", self.agent)?;
        writeln!(w, "Session: {}", self.session_id)?;
        writeln!(w, "Metadata: {}", self.metadata)?;
        for (idx, message) in self.messages.iter().enumerate() {
            writeln!(w, "Message {}: {}", idx + 1, message)?;
        }
        Ok(())
    }
}

impl Render for SessionsExportReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "exported {}:{} to {} ({} bytes, mode {})",
            self.agent, self.session_id, self.path, self.bytes, self.mode
        )
    }
}

impl Render for SessionsPruneReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let action = if self.dry_run {
            "would delete"
        } else {
            "deleted"
        };
        for session in &self.sessions {
            writeln!(
                w,
                "{} {}:{} last_modified={}",
                action, session.agent, session.id, session.last_modified
            )?;
        }
        for skipped in &self.skipped_agents {
            writeln!(w, "{}: {}", skipped.agent, skipped.reason)?;
        }
        Ok(())
    }
}

pub fn run(ctx: &Context, action: SessionsAction) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: SessionsAction,
) -> Result<SessionsReport, NczError> {
    match action {
        SessionsAction::List { agent } => {
            Ok(SessionsReport::List(list(ctx, paths, agent.as_deref())?))
        }
        SessionsAction::Show { session_id, agent } => Ok(SessionsReport::Show(show(
            ctx,
            paths,
            &session_id,
            agent.as_deref(),
        )?)),
        SessionsAction::Export {
            session_id,
            to,
            agent,
        } => Ok(SessionsReport::Export(export(
            ctx,
            paths,
            &session_id,
            &to,
            agent.as_deref(),
        )?)),
        SessionsAction::Prune {
            before,
            agent,
            dry_run,
        } => Ok(SessionsReport::Prune(prune(
            ctx,
            paths,
            &before,
            agent.as_deref(),
            dry_run,
        )?)),
    }
}

pub fn list(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
) -> Result<SessionsListReport, NczError> {
    let (sessions, skipped_agents) = collect_sessions(ctx, paths, requested_agent)?;
    Ok(SessionsListReport {
        schema_version: common::SCHEMA_VERSION,
        sessions,
        skipped_agents,
    })
}

pub fn show(
    ctx: &Context,
    paths: &Paths,
    session_id: &str,
    requested_agent: Option<&str>,
) -> Result<SessionsShowReport, NczError> {
    let resolved = resolve_session(ctx, paths, session_id, requested_agent)?;
    if ctx.show_secrets {
        eprintln!(
            "audit: ncz sessions show {} --agent={} used --show-secrets",
            resolved.id, resolved.agent
        );
    }
    let mut content = fetch_session_content(ctx, paths, &resolved.agent, &resolved.id)?;
    if !ctx.show_secrets {
        redact_content(&mut content);
    }
    Ok(SessionsShowReport {
        schema_version: common::SCHEMA_VERSION,
        agent: resolved.agent,
        session_id: resolved.id,
        session: content.session,
        messages: content.messages,
        metadata: content.metadata,
    })
}

pub fn export(
    ctx: &Context,
    paths: &Paths,
    session_id: &str,
    to: &Path,
    requested_agent: Option<&str>,
) -> Result<SessionsExportReport, NczError> {
    let resolved = resolve_session(ctx, paths, session_id, requested_agent)?;
    let content = fetch_session_content(ctx, paths, &resolved.agent, &resolved.id)?;
    let bundle = SessionExportBundle {
        schema_version: common::SCHEMA_VERSION,
        exported_at: exported_at(ctx),
        agent: resolved.agent.clone(),
        session: content.session,
        messages: content.messages,
        metadata: content.metadata,
    };
    let body = serde_json::to_vec_pretty(&bundle)?;
    state::atomic_write(to, &body, 0o600)?;
    let metadata = fs::metadata(to)?;
    Ok(SessionsExportReport {
        schema_version: common::SCHEMA_VERSION,
        agent: resolved.agent,
        session_id: resolved.id,
        path: to.display().to_string(),
        bytes: metadata.len(),
        mode: format!("{:03o}", metadata.permissions().mode() & 0o777),
    })
}

pub fn prune(
    ctx: &Context,
    paths: &Paths,
    before: &str,
    requested_agent: Option<&str>,
    dry_run: bool,
) -> Result<SessionsPruneReport, NczError> {
    validate_cutoff(before)?;
    let (sessions, skipped_agents) = collect_sessions(ctx, paths, requested_agent)?;
    let mut candidates: Vec<PrunedSession> = sessions
        .into_iter()
        .filter(|session| {
            !session.last_modified.is_empty() && session.last_modified.as_str() < before
        })
        .map(|session| PrunedSession {
            id: session.id,
            agent: session.agent,
            last_modified: session.last_modified,
            deleted: false,
        })
        .collect();

    if !dry_run && !candidates.is_empty() {
        let _lock = state::acquire_lock(&paths.lock_path)?;
        for session in &mut candidates {
            delete_session(ctx, paths, &session.agent, &session.id)?;
            session.deleted = true;
        }
    }

    Ok(SessionsPruneReport {
        schema_version: common::SCHEMA_VERSION,
        before: before.to_string(),
        dry_run,
        sessions: candidates,
        skipped_agents,
    })
}

fn collect_sessions(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
) -> Result<(Vec<SessionSummary>, Vec<SkippedAgent>), NczError> {
    let active_agents = active_agents(ctx, requested_agent)?;
    let mut sessions = Vec::new();
    let mut skipped_agents = Vec::new();
    for agent_name in active_agents {
        if agent_name != ZEROCLAW_AGENT {
            warn_unsupported_agent(&agent_name);
            skipped_agents.push(SkippedAgent {
                agent: agent_name,
                reason: UNSUPPORTED_REASON.to_string(),
            });
            continue;
        }
        sessions.extend(list_zeroclaw_sessions(ctx, paths)?);
    }
    sessions.sort_by(|a, b| {
        a.agent
            .cmp(&b.agent)
            .then_with(|| a.last_modified.cmp(&b.last_modified).reverse())
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok((sessions, skipped_agents))
}

fn active_agents(ctx: &Context, requested_agent: Option<&str>) -> Result<Vec<String>, NczError> {
    if let Some(agent_name) = requested_agent {
        common::validate_agent(agent_name)?;
        let unit = agent::service_for(agent_name);
        return Ok(if systemd::is_active(ctx.runner, &unit).unwrap_or(false) {
            vec![agent_name.to_string()]
        } else {
            Vec::new()
        });
    }
    Ok(common::running_agents(ctx.runner))
}

fn list_zeroclaw_sessions(ctx: &Context, paths: &Paths) -> Result<Vec<SessionSummary>, NczError> {
    let port = zeroclaw_gateway_port(paths)?;
    let value = get_zeroclaw_json(ctx, port, "/sessions")?;
    parse_sessions(value, ZEROCLAW_AGENT)
}

fn fetch_session_content(
    ctx: &Context,
    paths: &Paths,
    agent_name: &str,
    session_id: &str,
) -> Result<SessionContent, NczError> {
    if agent_name != ZEROCLAW_AGENT {
        return Err(NczError::Precondition(format!(
            "{agent_name}: {UNSUPPORTED_REASON}"
        )));
    }
    let port = zeroclaw_gateway_port(paths)?;
    let path = format!("/sessions/{}", percent_encode_path_segment(session_id));
    let value = get_zeroclaw_json(ctx, port, &path)?;
    Ok(content_from_value(value))
}

fn delete_session(
    ctx: &Context,
    paths: &Paths,
    agent_name: &str,
    session_id: &str,
) -> Result<(), NczError> {
    if agent_name != ZEROCLAW_AGENT {
        return Err(NczError::Precondition(format!(
            "{agent_name}: {UNSUPPORTED_REASON}"
        )));
    }
    let port = zeroclaw_gateway_port(paths)?;
    let path = format!("/sessions/{}", percent_encode_path_segment(session_id));
    let status = ctx
        .runner
        .http_delete_local(port, &path, SESSION_API_TIMEOUT_SECS)?;
    if (200..300).contains(&status) || status == 404 {
        Ok(())
    } else {
        Err(NczError::Exec {
            cmd: "http_delete_local".into(),
            msg: format!("DELETE {path} returned HTTP {status}"),
        })
    }
}

fn get_zeroclaw_json(ctx: &Context, port: u16, path: &str) -> Result<Value, NczError> {
    let (status, body) = ctx.runner.http_get_local_body(
        port,
        path,
        SESSION_API_TIMEOUT_SECS,
        SESSION_API_MAX_BYTES,
    )?;
    if status != 200 {
        return Err(NczError::Exec {
            cmd: "http_get_local_body".into(),
            msg: format!("GET {path} returned HTTP {status}"),
        });
    }
    Ok(serde_json::from_str(&body)?)
}

fn resolve_session(
    ctx: &Context,
    paths: &Paths,
    session_id: &str,
    requested_agent: Option<&str>,
) -> Result<SessionSummary, NczError> {
    let (sessions, _) = collect_sessions(ctx, paths, requested_agent)?;
    let matches: Vec<SessionSummary> = sessions
        .into_iter()
        .filter(|session| session.id == session_id)
        .collect();
    match matches.as_slice() {
        [] => Err(NczError::Usage(format!("session not found: {session_id}"))),
        [session] => Ok(session.clone()),
        many => {
            let agents: BTreeSet<&str> =
                many.iter().map(|session| session.agent.as_str()).collect();
            Err(NczError::Usage(format!(
                "session id {session_id} is ambiguous across agents: {}; pass --agent",
                agents.into_iter().collect::<Vec<_>>().join(",")
            )))
        }
    }
}

fn parse_sessions(value: Value, agent_name: &str) -> Result<Vec<SessionSummary>, NczError> {
    let array = if let Some(sessions) = value.get("sessions").and_then(Value::as_array) {
        sessions
    } else if let Some(data) = value.get("data").and_then(Value::as_array) {
        data
    } else if let Some(array) = value.as_array() {
        array
    } else {
        return Err(NczError::Precondition(
            "zeroclaw /sessions response did not contain a sessions array".to_string(),
        ));
    };

    let mut sessions = Vec::new();
    for item in array {
        let Some(id) = string_field(item, &["id", "session_id", "uuid"]) else {
            continue;
        };
        sessions.push(SessionSummary {
            id,
            agent: agent_name.to_string(),
            workspace: string_field(item, &["workspace", "cwd", "project"]).unwrap_or_default(),
            last_modified: string_field(item, &["last_modified", "updated_at", "modified_at"])
                .unwrap_or_default(),
            message_count: number_field(item, &["message_count", "messages_count"])
                .or_else(|| {
                    item.get("messages")
                        .and_then(Value::as_array)
                        .map(|messages| messages.len() as u64)
                })
                .unwrap_or(0),
        });
    }
    Ok(sessions)
}

fn content_from_value(value: Value) -> SessionContent {
    let session = value
        .get("session")
        .cloned()
        .unwrap_or_else(|| value.clone());
    let messages = value
        .get("messages")
        .or_else(|| {
            value
                .get("session")
                .and_then(|session| session.get("messages"))
        })
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let metadata = value
        .get("metadata")
        .or_else(|| {
            value
                .get("session")
                .and_then(|session| session.get("metadata"))
        })
        .cloned()
        .unwrap_or(Value::Null);
    SessionContent {
        session,
        messages,
        metadata,
    }
}

fn redact_content(content: &mut SessionContent) {
    redact_value(&mut content.session);
    for message in &mut content.messages {
        redact_value(message);
    }
    redact_value(&mut content.metadata);
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map.iter_mut() {
                if is_secret_key(key) {
                    *nested = Value::String("***".to_string());
                    continue;
                }
                if key.to_ascii_lowercase().contains("path") {
                    if let Some(text) = nested.as_str() {
                        if common::redact_path(Path::new(text)) {
                            *nested = Value::String("***".to_string());
                            continue;
                        }
                    }
                }
                redact_value(nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        Value::String(text) => {
            if common::redact_path(Path::new(text)) {
                *text = "***".to_string();
            } else {
                *text = common::redact_line(text, false);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "key"
        || key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("authorization")
        || key.contains("api_key")
        || key.contains("api-key")
        || key.contains("apikey")
        || key.ends_with("_key")
        || key.ends_with("-key")
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn number_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(Value::as_u64)
}

fn zeroclaw_gateway_port(paths: &Paths) -> Result<u16, NczError> {
    let entries = agent_env::read(paths)?;
    let mut keys = vec![
        "NCZ_ZEROCLAW_GATEWAY_URL".to_string(),
        "ZEROCLAW_GATEWAY_URL".to_string(),
        "ZEROCLAW_URL".to_string(),
        "NCZ_GATEWAY_URL".to_string(),
        "GATEWAY_URL".to_string(),
    ];
    if let Some(primary) = provider_state::read_primary(paths)? {
        let key = primary
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        keys.push(format!("NCZ_{key}_GATEWAY_URL"));
    }
    for key in keys {
        if let Some(value) = entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.value)
        {
            if let Some(port) = loopback_port(value) {
                return Ok(port);
            }
        }
    }
    agent::port_for(ZEROCLAW_AGENT)
        .ok_or_else(|| NczError::Usage(format!("unknown agent: {ZEROCLAW_AGENT}")))
}

fn loopback_port(url: &str) -> Option<u16> {
    let host = url_state::host(url)?;
    if !url_state::is_loopback_host(host) {
        return None;
    }
    let authority = url_state::authority(url)?;
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.starts_with('[') {
        let (_, after_bracket) = authority.split_once(']')?;
        return after_bracket.strip_prefix(':')?.parse().ok();
    }
    authority.rsplit_once(':')?.1.parse().ok()
}

fn exported_at(ctx: &Context) -> String {
    common::command_stdout(ctx.runner, "date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .unwrap_or_else(|| "unknown".to_string())
}

fn validate_cutoff(before: &str) -> Result<(), NczError> {
    let valid = before.len() >= 10
        && before.as_bytes().get(4) == Some(&b'-')
        && before.as_bytes().get(7) == Some(&b'-')
        && before[..4].chars().all(|ch| ch.is_ascii_digit())
        && before[5..7].chars().all(|ch| ch.is_ascii_digit())
        && before[8..10].chars().all(|ch| ch.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err(NczError::Usage(format!(
            "invalid --before date: {before} (expected YYYY-MM-DD or ISO timestamp)"
        )))
    }
}

fn percent_encode_path_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn warn_unsupported_agent(agent_name: &str) {
    eprintln!("{agent_name}: {UNSUPPORTED_REASON}");
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::cli::Context;
    use crate::cmd::common::{out, test_paths};
    use crate::sys::fake::FakeRunner;

    use super::*;

    fn ctx<'a>(runner: &'a FakeRunner, show_secrets: bool) -> Context<'a> {
        Context {
            json: false,
            show_secrets,
            runner,
        }
    }

    fn expect_active(runner: &FakeRunner, active: &[&str]) {
        for agent_name in agent::AGENTS {
            runner.expect(
                "systemctl",
                &["is-active", "--quiet", &agent::service_for(agent_name)],
                out(if active.contains(agent_name) { 0 } else { 3 }, "", ""),
            );
        }
    }

    fn expect_one_active(runner: &FakeRunner, agent_name: &str, active: bool) {
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", &agent::service_for(agent_name)],
            out(if active { 0 } else { 3 }, "", ""),
        );
    }

    fn sessions_body() -> &'static str {
        r#"{"sessions":[{"id":"s-new","workspace":"/work/new","last_modified":"2026-04-01T00:00:00Z","message_count":3},{"id":"s-old","workspace":"/work/old","last_modified":"2025-01-01T00:00:00Z","message_count":2}]}"#
    }

    fn session_body(secret: &str) -> String {
        format!(
            r#"{{
                "session": {{"id":"s-new","workspace":"/work/new","secret_path":"/tmp/token/file"}},
                "messages": [
                    {{"role":"user","content":"OPENAI_API_KEY={secret}"}},
                    {{"role":"assistant","content":"done","token":"{secret}"}}
                ],
                "metadata": {{"api_key":"{secret}","path":"/tmp/secret/config"}}
            }}"#
        )
    }

    #[test]
    fn sessions_list_aggregates_across_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();
        expect_active(&runner, &["zeroclaw", "openclaw"]);
        runner.expect_http_body(42617, "/sessions", 200, sessions_body());

        let report = list(&ctx(&runner, false), &paths, None).unwrap();

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.sessions.len(), 2);
        assert_eq!(report.sessions[0].agent, "zeroclaw");
        assert_eq!(report.skipped_agents[0].agent, "openclaw");
        runner.assert_done();
    }

    #[test]
    fn sessions_list_filters_by_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();
        expect_one_active(&runner, "zeroclaw", true);
        runner.expect_http_body(42617, "/sessions", 200, sessions_body());

        let report = list(&ctx(&runner, false), &paths, Some("zeroclaw")).unwrap();

        assert_eq!(report.sessions.len(), 2);
        assert!(report.skipped_agents.is_empty());
        runner.assert_done();
    }

    #[test]
    fn sessions_show_redacts_secrets_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();
        expect_one_active(&runner, "zeroclaw", true);
        runner.expect_http_body(42617, "/sessions", 200, sessions_body());
        runner.expect_http_body(42617, "/sessions/s-new", 200, &session_body("sk-live"));

        let report = show(&ctx(&runner, false), &paths, "s-new", Some("zeroclaw")).unwrap();
        let json = serde_json::to_string(&report).unwrap();

        assert!(!json.contains("sk-live"));
        assert!(json.contains("***"));
        runner.assert_done();
    }

    #[test]
    fn sessions_show_with_show_secrets_flag_includes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();
        expect_one_active(&runner, "zeroclaw", true);
        runner.expect_http_body(42617, "/sessions", 200, sessions_body());
        runner.expect_http_body(42617, "/sessions/s-new", 200, &session_body("sk-live"));

        let report = show(&ctx(&runner, true), &paths, "s-new", Some("zeroclaw")).unwrap();
        let json = serde_json::to_string(&report).unwrap();

        assert!(json.contains("sk-live"));
        runner.assert_done();
    }

    #[test]
    fn sessions_export_writes_bundle_with_correct_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let to = tmp.path().join("bundle.json");
        let runner = FakeRunner::new();
        expect_one_active(&runner, "zeroclaw", true);
        runner.expect_http_body(42617, "/sessions", 200, sessions_body());
        runner.expect_http_body(42617, "/sessions/s-new", 200, &session_body("sk-live"));
        runner.expect(
            "date",
            &["-u", "+%Y-%m-%dT%H:%M:%SZ"],
            out(0, "2026-04-29T00:00:00Z\n", ""),
        );

        let report = export(&ctx(&runner, false), &paths, "s-new", &to, Some("zeroclaw")).unwrap();
        let mode = fs::metadata(&to).unwrap().permissions().mode() & 0o777;
        let bundle: SessionExportBundle =
            serde_json::from_str(&fs::read_to_string(&to).unwrap()).unwrap();

        assert_eq!(report.mode, "600");
        assert_eq!(mode, 0o600);
        assert_eq!(bundle.schema_version, 1);
        assert_eq!(bundle.exported_at, "2026-04-29T00:00:00Z");
        assert_eq!(bundle.agent, "zeroclaw");
        runner.assert_done();
    }

    #[test]
    fn sessions_prune_dry_run_does_not_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();
        expect_one_active(&runner, "zeroclaw", true);
        runner.expect_http_body(42617, "/sessions", 200, sessions_body());

        let report = prune(
            &ctx(&runner, false),
            &paths,
            "2026-01-01",
            Some("zeroclaw"),
            true,
        )
        .unwrap();

        assert_eq!(report.sessions.len(), 1);
        assert!(!report.sessions[0].deleted);
        assert!(!paths.lock_path.exists());
        runner.assert_done();
    }

    #[test]
    fn sessions_prune_acquires_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();
        expect_one_active(&runner, "zeroclaw", true);
        runner.expect_http_body(42617, "/sessions", 200, sessions_body());
        runner.expect_http_delete(42617, "/sessions/s-old", 204);

        let report = prune(
            &ctx(&runner, false),
            &paths,
            "2026-01-01",
            Some("zeroclaw"),
            false,
        )
        .unwrap();

        assert!(paths.lock_path.exists());
        assert_eq!(report.sessions.len(), 1);
        assert!(report.sessions[0].deleted);
        runner.assert_done();
    }

    #[test]
    fn sessions_prune_skips_unimplemented_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();
        expect_active(&runner, &["zeroclaw", "hermes"]);
        runner.expect_http_body(42617, "/sessions", 200, sessions_body());

        let report = prune(&ctx(&runner, false), &paths, "2026-01-01", None, true).unwrap();

        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.skipped_agents[0].agent, "hermes");
        runner.assert_done();
    }
}
