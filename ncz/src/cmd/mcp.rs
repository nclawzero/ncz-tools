//! mcp — manage Model Context Protocol server declarations.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::{Context, McpAction};
use crate::cmd::{api as api_cmd, common};
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent_env, mcp as mcp_state, providers as provider_state, Paths};

const MAX_AUTH_VALUE_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum McpReport {
    List(McpListReport),
    Add(McpAddReport),
    Remove(McpRemoveReport),
    Show(McpShowReport),
}

impl Render for McpReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            McpReport::List(report) => report.render_text(w),
            McpReport::Add(report) => report.render_text(w),
            McpReport::Remove(report) => report.render_text(w),
            McpReport::Show(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct McpListReport {
    pub schema_version: u32,
    pub servers: Vec<McpServerReport>,
}

#[derive(Debug, Serialize, Clone)]
pub struct McpServerReport {
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    pub command: Option<String>,
    pub url: Option<String>,
    pub auth_env_var: Option<String>,
    pub file: String,
}

#[derive(Debug, Serialize)]
pub struct McpAddReport {
    pub schema_version: u32,
    pub server: McpServerReport,
    pub changed: bool,
    pub revoked_provider_bindings: Vec<String>,
    pub revoked_mcp_bindings: Vec<String>,
    pub restart_required: bool,
    pub restart_agents: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct McpRemoveReport {
    pub schema_version: u32,
    pub name: String,
    pub removed: bool,
    pub restart_required: bool,
    pub restart_agents: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct McpShowReport {
    pub schema_version: u32,
    pub server: McpServerReport,
}

impl Render for McpListReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for server in &self.servers {
            render_server_line(w, server)?;
        }
        Ok(())
    }
}

impl Render for McpAddReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "MCP server added: {}", self.server.name)?;
        if !self.revoked_provider_bindings.is_empty() {
            writeln!(
                w,
                "provider bindings revoked: {}",
                self.revoked_provider_bindings.join(",")
            )?;
        }
        if !self.revoked_mcp_bindings.is_empty() {
            writeln!(
                w,
                "MCP bindings revoked: {}",
                self.revoked_mcp_bindings.join(",")
            )?;
        }
        if self.restart_required {
            writeln!(w, "restart required: {}", self.restart_agents.join(","))?;
        }
        Ok(())
    }
}

impl Render for McpRemoveReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.removed {
            writeln!(w, "MCP server removed: {}", self.name)?;
        } else {
            writeln!(w, "MCP server absent: {}", self.name)?;
        }
        if self.restart_required {
            writeln!(w, "restart required: {}", self.restart_agents.join(","))?;
        }
        Ok(())
    }
}

impl Render for McpShowReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        render_server_line(w, &self.server)
    }
}

pub fn run(ctx: &Context, action: McpAction) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: McpAction,
) -> Result<McpReport, NczError> {
    match action {
        McpAction::List => Ok(McpReport::List(list(ctx, paths)?)),
        McpAction::Add {
            name,
            transport,
            command,
            url,
            auth_env,
            auth_value_env,
        } => Ok(McpReport::Add(add(
            ctx,
            paths,
            name,
            transport,
            command,
            url,
            auth_env,
            auth_value_env,
        )?)),
        McpAction::Remove { name } => Ok(McpReport::Remove(remove(paths, &name)?)),
        McpAction::Show { name } => Ok(McpReport::Show(show(ctx, paths, &name)?)),
    }
}

pub fn list(ctx: &Context, paths: &Paths) -> Result<McpListReport, NczError> {
    Ok(McpListReport {
        schema_version: common::SCHEMA_VERSION,
        servers: mcp_state::read_all(paths)?
            .into_iter()
            .map(|record| server_report(ctx, record))
            .collect(),
    })
}

pub fn add(
    ctx: &Context,
    paths: &Paths,
    name: String,
    transport: String,
    command: Option<String>,
    url: Option<String>,
    auth_env: Option<String>,
    auth_value_env: Option<String>,
) -> Result<McpAddReport, NczError> {
    let declaration = mcp_state::McpDeclaration {
        schema_version: common::SCHEMA_VERSION,
        name,
        transport,
        command,
        url,
        auth_env,
    };
    mcp_state::validate_declaration(&declaration)?;
    let auth_value = resolve_auth_value(&declaration, auth_value_env.as_deref())?;
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let existing = mcp_state::read(paths, &declaration.name)?;
    if let Some(record) = &existing {
        if record.declaration != declaration {
            return Err(NczError::Usage(format!(
                "MCP declaration already exists: {}",
                declaration.name
            )));
        }
        if auth_value.is_none() {
            return Err(NczError::Usage(format!(
                "MCP declaration already exists: {}; pass --auth-value-env to refresh its credential approval",
                declaration.name
            )));
        }
    }
    let path = existing
        .as_ref()
        .map(|record| record.path.clone())
        .unwrap_or(mcp_state::declaration_path(paths, &declaration.name)?);
    let mut target_paths = vec![path.clone()];
    let override_agents = if auth_value.is_some() {
        let auth_env = declaration.auth_env.as_deref().ok_or_else(|| {
            NczError::Usage("--auth-value-env requires --auth-env".to_string())
        })?;
        let (override_agents, credential_paths) =
            api_cmd::credential_upsert_targets(paths, auth_env, &[])?;
        target_paths.extend(credential_paths);
        override_agents
    } else {
        Vec::new()
    };
    if auth_value.is_some() {
        target_paths.sort();
        target_paths.dedup();
    }
    let snapshots = snapshot_paths(&target_paths)?;
    let result = (|| -> Result<McpAddMutationOutcome, NczError> {
        let mut outcome = McpAddMutationOutcome::default();
        let mut changed_override_agents = Vec::new();
        if let Some(value) = auth_value.as_deref() {
            let auth_env = declaration.auth_env.as_deref().ok_or_else(|| {
                NczError::Usage("--auth-value-env requires --auth-env".to_string())
            })?;
            let previous_shared_value = api_cmd::shared_credential_value(paths, auth_env)?;
            if previous_shared_value.as_deref() != Some(value) {
                reject_shared_auth_rotation(paths, &declaration, auth_env)?;
            }
        }
        if existing.is_none() {
            if auth_value.is_none() {
                require_mcp_auth_binding(paths, &declaration)?;
            }
            mcp_state::write(paths, &declaration)?;
            outcome.declaration_changed = true;
        }
        if let Some(value) = auth_value.as_deref() {
            let auth_env = declaration.auth_env.as_deref().ok_or_else(|| {
                NczError::Usage("--auth-value-env requires --auth-env".to_string())
            })?;
            let credential_write =
                api_cmd::set_credential_value_locked(paths, auth_env, value, &override_agents)?;
            outcome.shared_credential_changed = credential_write.shared_changed;
            changed_override_agents = credential_write.changed_override_agents;
            outcome.mcp_binding_changed = set_mcp_auth_binding(paths, &declaration, auth_env)?;
        }
        outcome.changed_override_agents = changed_override_agents;
        require_mcp_auth_binding(paths, &declaration)?;
        Ok(outcome)
    })();
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            restore_snapshots(&snapshots)?;
            return Err(err);
        }
    };
    let global_restart = outcome.declaration_changed
        || outcome.shared_credential_changed
        || outcome.mcp_binding_changed
        || !outcome.revoked_provider_bindings.is_empty()
        || !outcome.revoked_mcp_bindings.is_empty();
    let changed = global_restart || !outcome.changed_override_agents.is_empty();
    let restart_agents = if changed {
        api_cmd::credential_restart_agents(global_restart, &outcome.changed_override_agents)
    } else {
        Vec::new()
    };
    Ok(McpAddReport {
        schema_version: common::SCHEMA_VERSION,
        server: server_report(ctx, mcp_state::McpRecord { declaration, path }),
        changed,
        revoked_provider_bindings: outcome.revoked_provider_bindings,
        revoked_mcp_bindings: outcome.revoked_mcp_bindings,
        restart_required: changed,
        restart_agents,
    })
}

#[derive(Default)]
struct McpAddMutationOutcome {
    declaration_changed: bool,
    shared_credential_changed: bool,
    mcp_binding_changed: bool,
    changed_override_agents: Vec<String>,
    revoked_provider_bindings: Vec<String>,
    revoked_mcp_bindings: Vec<String>,
}

pub fn remove(paths: &Paths, name: &str) -> Result<McpRemoveReport, NczError> {
    remove_with_binding_remover(paths, name, agent_env::remove_mcp_binding)
}

fn remove_with_binding_remover<F>(
    paths: &Paths,
    name: &str,
    mut remove_binding: F,
) -> Result<McpRemoveReport, NczError>
where
    F: FnMut(&Paths, &str) -> Result<bool, NczError>,
{
    mcp_state::validate_name(name)?;
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let mut aliases = mcp_state::removal_aliases(paths, name)?;
    aliases.push(name.to_string());
    aliases.sort();
    aliases.dedup();
    let mut target_paths = vec![paths.agent_env()];
    target_paths.extend(mcp_state::removal_paths(paths, name)?);
    target_paths.sort();
    target_paths.dedup();
    let snapshots = snapshot_paths(&target_paths)?;
    let result = (|| -> Result<(bool, bool), NczError> {
        let mut binding_removed = false;
        for alias in &aliases {
            binding_removed |= remove_binding(paths, alias)?;
        }
        let removed = mcp_state::remove(paths, name)?;
        Ok((removed, binding_removed))
    })();
    let (removed, binding_removed) = match result {
        Ok(result) => result,
        Err(err) => {
            restore_snapshots(&snapshots)?;
            return Err(err);
        }
    };
    Ok(McpRemoveReport {
        schema_version: common::SCHEMA_VERSION,
        name: name.to_string(),
        removed,
        restart_required: removed || binding_removed,
        restart_agents: if removed || binding_removed {
            api_cmd::credential_restart_agents(true, &[])
        } else {
            Vec::new()
        },
    })
}

pub fn show(ctx: &Context, paths: &Paths, name: &str) -> Result<McpShowReport, NczError> {
    let record = mcp_state::read(paths, name)?
        .ok_or_else(|| NczError::Usage(format!("unknown MCP server: {name}")))?;
    Ok(McpShowReport {
        schema_version: common::SCHEMA_VERSION,
        server: server_report(ctx, record),
    })
}

fn server_report(ctx: &Context, record: mcp_state::McpRecord) -> McpServerReport {
    let name = record.declaration.name;
    let transport = record.declaration.transport;
    let command = record.declaration.command;
    let url = record.declaration.url;
    let redact_stdio = !ctx.show_secrets && transport == "stdio";
    let endpoint = match transport.as_str() {
        "stdio" if redact_stdio => "***".to_string(),
        "stdio" => command.clone().unwrap_or_default(),
        "http" => url.clone().unwrap_or_default(),
        _ => String::new(),
    };
    let command = if redact_stdio && command.is_some() {
        Some("***".to_string())
    } else {
        command
    };
    McpServerReport {
        name,
        transport,
        endpoint,
        command,
        url,
        auth_env_var: record.declaration.auth_env.map(|auth_env| {
            if ctx.show_secrets {
                auth_env
            } else {
                "***".to_string()
            }
        }),
        file: record.path.display().to_string(),
    }
}

fn render_server_line(w: &mut dyn Write, server: &McpServerReport) -> io::Result<()> {
    writeln!(
        w,
        "{:<18} transport={:<6} endpoint={} auth_env_var={}",
        server.name,
        server.transport,
        if server.endpoint.is_empty() {
            "unknown"
        } else {
            &server.endpoint
        },
        server.auth_env_var.as_deref().unwrap_or("none")
    )
}

fn resolve_auth_value(
    declaration: &mcp_state::McpDeclaration,
    auth_value_env: Option<&str>,
) -> Result<Option<String>, NczError> {
    let Some(value_env) = auth_value_env else {
        return Ok(None);
    };
    if declaration.auth_env.is_none() {
        return Err(NczError::Usage(
            "--auth-value-env requires --auth-env".to_string(),
        ));
    }
    agent_env::validate_key(value_env)?;
    let value = env::var(value_env).map_err(|_| {
        NczError::Precondition(format!("environment variable {value_env} is not set"))
    })?;
    if value.len() > MAX_AUTH_VALUE_BYTES {
        return Err(NczError::Usage(format!(
            "MCP auth token value exceeds {MAX_AUTH_VALUE_BYTES} bytes"
        )));
    }
    agent_env::validate_value(&value)?;
    if value.is_empty() {
        return Err(NczError::Usage(
            "MCP auth token value cannot be empty".to_string(),
        ));
    }
    Ok(Some(value))
}

fn require_mcp_auth_binding(
    paths: &Paths,
    declaration: &mcp_state::McpDeclaration,
) -> Result<(), NczError> {
    let Some(auth_env) = declaration.auth_env.as_deref() else {
        return Ok(());
    };
    let entries = agent_env::read(paths)?;
    if !entries
        .iter()
        .any(|entry| entry.key == auth_env && !entry.value.is_empty())
    {
        return Err(NczError::Precondition(format!(
            "MCP server {} requires non-empty credential {} in agent-env",
            declaration.name, auth_env
        )));
    }
    if mcp_auth_binding_matches(&entries, declaration, auth_env)? {
        return Ok(());
    }
    let approval_target = match declaration.transport.as_str() {
        "http" => declaration.url.as_deref().unwrap_or("this URL"),
        "stdio" => "this command",
        _ => "this declaration",
    };
    Err(NczError::Precondition(format!(
        "MCP credential {} is not bound to server {}; rerun `ncz mcp add {}` with --auth-value-env to approve {}",
        auth_env, declaration.name, declaration.name, approval_target
    )))
}

fn reject_shared_auth_rotation(
    paths: &Paths,
    declaration: &mcp_state::McpDeclaration,
    auth_env: &str,
) -> Result<(), NczError> {
    let provider_bindings = agent_env::provider_bindings_for_key(paths, auth_env)?;
    let mcp_bindings = agent_env::mcp_bindings_for_key(paths, auth_env)?;
    let provider_references = provider_state::credential_references(paths, auth_env)?;
    let mcp_references = mcp_state::auth_references(paths, auth_env)?;
    let entries = agent_env::read(paths)?;
    let mut references = Vec::new();
    references.extend(
        provider_bindings
            .into_iter()
            .map(|provider| format!("provider:{provider}")),
    );
    references.extend(
        provider_references
            .into_iter()
            .map(|provider| format!("provider:{provider}")),
    );
    for server in mcp_bindings {
        if server == declaration.name && mcp_auth_binding_matches(&entries, declaration, auth_env)?
        {
            continue;
        }
        references.push(format!("mcp:{server}"));
    }
    references.extend(
        mcp_references
            .into_iter()
            .filter(|server| server != &declaration.name)
            .map(|server| format!("mcp:{server}")),
    );
    references.sort();
    references.dedup();
    if references.is_empty() {
        return Ok(());
    }
    Err(NczError::Precondition(format!(
        "credential {auth_env} is already referenced by {}; rotate it with `ncz api set` or remove those references before using `ncz mcp add --auth-value-env`",
        references.join(",")
    )))
}

fn set_mcp_auth_binding(
    paths: &Paths,
    declaration: &mcp_state::McpDeclaration,
    auth_env: &str,
) -> Result<bool, NczError> {
    match declaration.transport.as_str() {
        "http" => {
            let url = declaration
                .url
                .as_deref()
                .ok_or_else(|| NczError::Usage("--auth-value-env requires --url".to_string()))?;
            agent_env::set_mcp_binding(paths, &declaration.name, auth_env, url)
        }
        "stdio" => {
            let command = declaration.command.as_deref().ok_or_else(|| {
                NczError::Usage("--auth-value-env requires --command".to_string())
            })?;
            agent_env::set_mcp_stdio_binding(paths, &declaration.name, auth_env, command)
        }
        _ => Ok(false),
    }
}

fn mcp_auth_binding_matches(
    entries: &[agent_env::AgentEnvEntry],
    declaration: &mcp_state::McpDeclaration,
    auth_env: &str,
) -> Result<bool, NczError> {
    match declaration.transport.as_str() {
        "http" => {
            let url = declaration.url.as_deref().unwrap_or_default();
            agent_env::mcp_binding_matches(entries, &declaration.name, auth_env, url)
        }
        "stdio" => {
            let command = declaration.command.as_deref().unwrap_or_default();
            agent_env::mcp_stdio_binding_matches(entries, &declaration.name, auth_env, command)
        }
        _ => Ok(false),
    }
}

struct FileSnapshot {
    path: PathBuf,
    body: Option<Vec<u8>>,
    mode: u32,
}

fn snapshot_paths(paths: &[PathBuf]) -> Result<Vec<FileSnapshot>, NczError> {
    paths.iter().map(|path| snapshot_path(path)).collect()
}

fn snapshot_path(path: &Path) -> Result<FileSnapshot, NczError> {
    let body = match fs::read(path) {
        Ok(body) => Some(body),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(err) => return Err(NczError::Io(err)),
    };
    let mode = if body.is_some() {
        fs::metadata(path)?.permissions().mode() & 0o777
    } else {
        0o600
    };
    Ok(FileSnapshot {
        path: path.to_path_buf(),
        body,
        mode,
    })
}

fn restore_snapshots(snapshots: &[FileSnapshot]) -> Result<(), NczError> {
    for snapshot in snapshots.iter().rev() {
        match &snapshot.body {
            Some(body) => state::atomic_write(&snapshot.path, body, snapshot.mode)?,
            None => match state::remove_file_durable(&snapshot.path) {
                Ok(()) => {}
                Err(NczError::Io(err))
                    if matches!(
                        err.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) => {}
                Err(err) => return Err(err),
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;

    use crate::cli::{Context, McpAction};
    use crate::cmd::common::test_paths;
    use crate::sys::fake::FakeRunner;
    use crate::state::agent;

    use super::*;

    fn ctx<'a>(runner: &'a FakeRunner) -> Context<'a> {
        Context {
            json: false,
            show_secrets: false,
            runner,
        }
    }

    fn all_agents() -> Vec<String> {
        agent::AGENTS
            .iter()
            .map(|agent| (*agent).to_string())
            .collect()
    }

    #[test]
    fn mcp_add_writes_stdio_declaration_and_redacts_auth_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        agent_env::set_mcp_stdio_binding(
            &paths,
            "filesystem",
            "MCP_TOKEN",
            "mcp-filesystem /srv",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "filesystem".to_string(),
                transport: "stdio".to_string(),
                command: Some("mcp-filesystem /srv".to_string()),
                url: None,
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: None,
            },
        )
        .unwrap();

        let McpReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert_eq!(report.server.auth_env_var.as_deref(), Some("***"));
        assert_eq!(report.server.endpoint, "***");
        assert_eq!(report.server.command.as_deref(), Some("***"));
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert!(paths.mcp_dir().join("filesystem.json").exists());
    }

    #[test]
    fn mcp_add_ignores_invalid_active_agent_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "bad-agent\n").unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: None,
                auth_value_env: None,
            },
        )
        .unwrap();

        let McpReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert!(paths.mcp_dir().join("search.json").exists());
    }

    #[test]
    fn mcp_add_rejects_unbound_stdio_auth_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "filesystem".to_string(),
                transport: "stdio".to_string(),
                command: Some("mcp-filesystem /srv".to_string()),
                url: None,
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: None,
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("non-empty credential MCP_TOKEN"))
        );
        assert!(!paths.mcp_dir().join("filesystem.json").exists());
    }

    #[test]
    fn mcp_add_rejects_unapproved_stdio_auth_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "filesystem".to_string(),
                transport: "stdio".to_string(),
                command: Some("mcp-filesystem /srv".to_string()),
                url: None,
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: None,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("not bound")));
        assert!(!paths.mcp_dir().join("filesystem.json").exists());
    }

    #[test]
    fn mcp_show_redacts_stdio_command_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("filesystem.json"),
            r#"{"schema_version":1,"name":"filesystem","transport":"stdio","command":"mcp-filesystem /srv","url":null,"auth_env":null}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = show(&ctx(&runner), &paths, "filesystem").unwrap();

        assert_eq!(report.server.endpoint, "***");
        assert_eq!(report.server.command.as_deref(), Some("***"));
    }

    #[test]
    fn mcp_show_reveals_stdio_command_with_show_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("filesystem.json"),
            r#"{"schema_version":1,"name":"filesystem","transport":"stdio","command":"mcp-filesystem /srv","url":null,"auth_env":null}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();
        let ctx = Context {
            json: false,
            show_secrets: true,
            runner: &runner,
        };

        let report = show(&ctx, &paths, "filesystem").unwrap();

        assert_eq!(report.server.endpoint, "mcp-filesystem /srv");
        assert_eq!(
            report.server.command.as_deref(),
            Some("mcp-filesystem /srv")
        );
    }

    #[test]
    fn mcp_show_ignores_unrelated_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(paths.mcp_dir().join("broken.json"), "{").unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":null}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = show(&ctx(&runner), &paths, "search").unwrap();

        assert_eq!(report.server.name, "search");
        assert_eq!(report.server.endpoint, "https://mcp.example.test");
    }

    #[test]
    fn mcp_list_reads_declarations() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths).unwrap();

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.servers[0].name, "search");
        assert_eq!(report.servers[0].endpoint, "https://mcp.example.test");
        assert_eq!(report.servers[0].auth_env_var.as_deref(), Some("***"));
    }

    #[test]
    fn mcp_add_rejects_unbound_http_auth_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: None,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("not bound")));
        assert!(!paths.mcp_dir().join("search.json").exists());
    }

    #[test]
    fn mcp_add_with_auth_value_env_binds_http_auth_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        env::set_var("NCZ_TEST_MCP_TOKEN", "secret");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_TOKEN".to_string()),
            },
        )
        .unwrap();

        let McpReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert_eq!(report.server.auth_env_var.as_deref(), Some("***"));
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert!(paths.mcp_dir().join("search.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "MCP_TOKEN=secret\nNCZ_MCP_BINDING_736561726368=\"MCP_TOKEN https://mcp.example.test\"\n"
        );
    }

    #[test]
    fn mcp_add_with_auth_value_env_rejects_shared_key_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "MCP_TOKEN=old\n").unwrap();
        agent_env::set_mcp_binding(&paths, "old-search", "MCP_TOKEN", "https://old.example.test")
            .unwrap();
        agent_env::set_provider_binding(
            &paths,
            "example",
            "MCP_TOKEN",
            "https://api.example.test",
        )
        .unwrap();
        env::set_var("NCZ_TEST_MCP_ROTATED_TOKEN", "new");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_ROTATED_TOKEN".to_string()),
            },
        )
        .unwrap_err();

        assert!(matches!(
            err,
            NczError::Precondition(message)
                if message.contains("provider:example") && message.contains("mcp:old-search")
        ));
        assert!(!paths.mcp_dir().join("search.json").exists());
        let entries = agent_env::read(&paths).unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.key == "MCP_TOKEN" && entry.value == "old"));
        assert!(agent_env::mcp_binding_matches(
            &entries,
            "old-search",
            "MCP_TOKEN",
            "https://old.example.test"
        )
        .unwrap());
        assert!(agent_env::provider_binding_matches(
            &entries,
            "example",
            "MCP_TOKEN",
            "https://api.example.test"
        )
        .unwrap());
    }

    #[test]
    fn mcp_add_with_auth_value_env_rejects_unbound_provider_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(paths.agent_env(), "MCP_TOKEN=old\n").unwrap();
        fs::write(
            paths.providers_dir().join("example.json"),
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"mini","key_env":"MCP_TOKEN","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        env::set_var("NCZ_TEST_MCP_PROVIDER_SHARED_TOKEN", "new");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_PROVIDER_SHARED_TOKEN".to_string()),
            },
        )
        .unwrap_err();

        assert!(matches!(
            err,
            NczError::Precondition(message) if message.contains("provider:example")
        ));
        assert!(!paths.mcp_dir().join("search.json").exists());
        let entries = agent_env::read(&paths).unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.key == "MCP_TOKEN" && entry.value == "old"));
    }

    #[test]
    fn mcp_add_with_auth_value_env_rejects_unbound_mcp_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(paths.agent_env(), "MCP_TOKEN=old\n").unwrap();
        fs::write(
            paths.mcp_dir().join("old-search.json"),
            r#"{"schema_version":1,"name":"old-search","transport":"http","command":null,"url":"https://old.example.test","auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        env::set_var("NCZ_TEST_MCP_UNBOUND_SHARED_TOKEN", "new");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_UNBOUND_SHARED_TOKEN".to_string()),
            },
        )
        .unwrap_err();

        assert!(matches!(
            err,
            NczError::Precondition(message) if message.contains("mcp:old-search")
        ));
        assert!(!paths.mcp_dir().join("search.json").exists());
        assert!(paths.mcp_dir().join("old-search.json").exists());
        let entries = agent_env::read(&paths).unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.key == "MCP_TOKEN" && entry.value == "old"));
    }

    #[test]
    fn mcp_add_with_auth_value_env_same_value_and_binding_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        agent_env::set_mcp_binding(&paths, "search", "MCP_TOKEN", "https://mcp.example.test")
            .unwrap();
        let original_agent_env = fs::read_to_string(paths.agent_env()).unwrap();
        env::set_var("NCZ_TEST_MCP_SAME_TOKEN", "secret");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_SAME_TOKEN".to_string()),
            },
        )
        .unwrap();

        let McpReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert!(!report.changed);
        assert!(!report.restart_required);
        assert!(report.restart_agents.is_empty());
        assert!(report.revoked_provider_bindings.is_empty());
        assert!(report.revoked_mcp_bindings.is_empty());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            original_agent_env
        );
    }

    #[test]
    fn mcp_add_with_auth_value_env_writes_stdio_auth_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        env::set_var("NCZ_TEST_MCP_STDIO_TOKEN", "secret");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "filesystem".to_string(),
                transport: "stdio".to_string(),
                command: Some("mcp-filesystem /srv".to_string()),
                url: None,
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_STDIO_TOKEN".to_string()),
            },
        )
        .unwrap();

        let McpReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert_eq!(report.server.auth_env_var.as_deref(), Some("***"));
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert!(paths.mcp_dir().join("filesystem.json").exists());
        let agent_env_body = fs::read_to_string(paths.agent_env()).unwrap();
        assert!(agent_env_body.starts_with(
            "MCP_TOKEN=secret\nNCZ_MCP_BINDING_66696C6573797374656D=\"MCP_TOKEN stdio-sha256:"
        ));
        assert!(agent_env::mcp_stdio_binding_matches(
            &agent_env::read(&paths).unwrap(),
            "filesystem",
            "MCP_TOKEN",
            "mcp-filesystem /srv"
        )
        .unwrap());
    }

    #[test]
    fn mcp_add_with_auth_value_env_rebinds_existing_stdio_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("filesystem.json"),
            r#"{"schema_version":1,"name":"filesystem","transport":"stdio","command":"mcp-filesystem /srv","url":null,"auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        env::set_var("NCZ_TEST_MCP_REBOUND_TOKEN", "secret");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "filesystem".to_string(),
                transport: "stdio".to_string(),
                command: Some("mcp-filesystem /srv".to_string()),
                url: None,
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_REBOUND_TOKEN".to_string()),
            },
        )
        .unwrap();

        let McpReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert_eq!(report.server.name, "filesystem");
        assert!(paths.mcp_dir().join("filesystem.json").exists());
        assert!(agent_env::mcp_stdio_binding_matches(
            &agent_env::read(&paths).unwrap(),
            "filesystem",
            "MCP_TOKEN",
            "mcp-filesystem /srv"
        )
        .unwrap());
    }

    #[test]
    fn mcp_add_existing_declaration_still_rejects_without_auth_value_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("filesystem.json"),
            r#"{"schema_version":1,"name":"filesystem","transport":"stdio","command":"mcp-filesystem /srv","url":null,"auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "filesystem".to_string(),
                transport: "stdio".to_string(),
                command: Some("mcp-filesystem /srv".to_string()),
                url: None,
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: None,
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, NczError::Usage(message) if message.contains("already exists"))
        );
    }

    #[test]
    fn mcp_add_with_auth_value_env_updates_existing_agent_override_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.agent_env_override("hermes").parent().unwrap()).unwrap();
        fs::write(paths.agent_env(), "MCP_TOKEN=old\n").unwrap();
        fs::write(paths.agent_env_override("hermes"), "MCP_TOKEN=stale\nOTHER=1\n").unwrap();
        env::set_var("NCZ_TEST_MCP_ROTATED_TOKEN", "new");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_ROTATED_TOKEN".to_string()),
            },
        )
        .unwrap();

        let McpReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "MCP_TOKEN=new\nNCZ_MCP_BINDING_736561726368=\"MCP_TOKEN https://mcp.example.test\"\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("hermes")).unwrap(),
            "MCP_TOKEN=new\nOTHER=1\n"
        );
    }

    #[test]
    fn mcp_add_rejects_oversized_auth_value_env_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let oversized = "x".repeat(MAX_AUTH_VALUE_BYTES + 1);
        env::set_var("NCZ_TEST_MCP_OVERSIZED_TOKEN", oversized);
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "http".to_string(),
                command: None,
                url: Some("https://mcp.example.test".to_string()),
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_MCP_OVERSIZED_TOKEN".to_string()),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(message) if message.contains("exceeds")));
        assert!(!paths.agent_env().exists());
        assert!(!paths.mcp_dir().join("search.json").exists());
    }

    #[test]
    fn mcp_remove_deletes_auth_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        agent_env::set_mcp_binding(&paths, "search", "MCP_TOKEN", "https://mcp.example.test")
            .unwrap();

        let report = remove(&paths, "search").unwrap();

        assert!(report.removed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "MCP_TOKEN=secret\n");
    }

    #[test]
    fn mcp_remove_ignores_invalid_active_agent_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "bad-agent\n").unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        agent_env::set_mcp_binding(&paths, "search", "MCP_TOKEN", "https://mcp.example.test")
            .unwrap();

        let report = remove(&paths, "search").unwrap();

        assert!(report.removed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert!(!paths.mcp_dir().join("search.json").exists());
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "MCP_TOKEN=secret\n");
    }

    #[test]
    fn mcp_remove_deletes_declared_name_alias_auth_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.mcp_dir().join("old.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        agent_env::set_mcp_binding(&paths, "search", "MCP_TOKEN", "https://mcp.example.test")
            .unwrap();

        let report = remove(&paths, "old").unwrap();

        assert!(report.removed);
        assert!(report.restart_required);
        assert!(!paths.mcp_dir().join("old.json").exists());
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "MCP_TOKEN=secret\n");
    }

    #[test]
    fn mcp_remove_restores_declaration_when_binding_cleanup_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let declaration_path = paths.mcp_dir().join("search.json");
        let declaration_body = r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":"MCP_TOKEN"}"#;
        fs::write(&declaration_path, declaration_body).unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        agent_env::set_mcp_binding(&paths, "search", "MCP_TOKEN", "https://mcp.example.test")
            .unwrap();
        let agent_env_body = fs::read_to_string(paths.agent_env()).unwrap();
        let declaration_path_for_check = declaration_path.clone();

        let err = remove_with_binding_remover(&paths, "search", move |_, _| {
            assert!(declaration_path_for_check.exists());
            Err(NczError::Precondition(
                "simulated binding cleanup failure".to_string(),
            ))
        })
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(fs::read_to_string(declaration_path).unwrap(), declaration_body);
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), agent_env_body);
    }

    #[test]
    fn mcp_remove_absent_deletes_stale_requested_name_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        agent_env::set(&paths, "MCP_TOKEN", "secret").unwrap();
        agent_env::set_mcp_binding(&paths, "search", "MCP_TOKEN", "https://mcp.example.test")
            .unwrap();

        let report = remove(&paths, "search").unwrap();

        assert!(!report.removed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "MCP_TOKEN=secret\n");
    }

    #[test]
    fn mcp_remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());

        let report = remove(&paths, "missing").unwrap();

        assert!(!report.removed);
        assert!(!report.restart_required);
        assert!(report.restart_agents.is_empty());
    }

    #[test]
    fn mcp_json_reports_include_action_discriminator() {
        let server = McpServerReport {
            name: "search".to_string(),
            transport: "http".to_string(),
            endpoint: "https://mcp.example.test".to_string(),
            command: None,
            url: Some("https://mcp.example.test".to_string()),
            auth_env_var: Some("***".to_string()),
            file: "/etc/nclawzero/mcp.d/search.json".to_string(),
        };
        let reports = [
            (
                "list",
                McpReport::List(McpListReport {
                    schema_version: 1,
                    servers: vec![server.clone()],
                }),
            ),
            (
                "add",
                McpReport::Add(McpAddReport {
                    schema_version: 1,
                    server: server.clone(),
                    changed: true,
                    revoked_provider_bindings: Vec::new(),
                    revoked_mcp_bindings: Vec::new(),
                    restart_required: true,
                    restart_agents: vec!["zeroclaw".to_string()],
                }),
            ),
            (
                "remove",
                McpReport::Remove(McpRemoveReport {
                    schema_version: 1,
                    name: "search".to_string(),
                    removed: true,
                    restart_required: true,
                    restart_agents: vec!["zeroclaw".to_string()],
                }),
            ),
            (
                "show",
                McpReport::Show(McpShowReport {
                    schema_version: 1,
                    server,
                }),
            ),
        ];

        for (action, report) in reports {
            let value = serde_json::to_value(report).unwrap();

            assert_eq!(value["action"].as_str(), Some(action));
            assert_eq!(value["schema_version"].as_u64(), Some(1));
        }
    }
}
