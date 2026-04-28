//! api — manage the shared agent credential environment.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::{ApiAction, Context};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent, agent_env, mcp as mcp_state, providers as provider_state, Paths};

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ApiReport {
    List(ApiListReport),
    Mutate(ApiMutationReport),
}

impl Render for ApiReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            ApiReport::List(report) => report.render_text(w),
            ApiReport::Mutate(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ApiListReport {
    pub schema_version: u32,
    pub entries: Vec<agent_env::RedactedAgentEnvEntry>,
}

#[derive(Debug, Serialize)]
pub struct ApiMutationReport {
    pub schema_version: u32,
    pub action: String,
    pub key: String,
    pub value: Option<String>,
    pub changed: bool,
    pub shared_file: String,
    pub agent_override_files: Vec<String>,
    pub provider_bindings: Vec<String>,
    pub restart_required: bool,
    pub restart_agents: Vec<String>,
}

impl Render for ApiListReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for entry in &self.entries {
            writeln!(w, "{}={}", entry.key, entry.value)?;
        }
        Ok(())
    }
}

impl Render for ApiMutationReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self.action.as_str() {
            "remove" if self.changed => writeln!(w, "{} removed", self.key)?,
            "remove" => writeln!(w, "{} was not set", self.key)?,
            _ if self.changed => writeln!(w, "{} updated", self.key)?,
            _ => writeln!(w, "{} added", self.key)?,
        }
        for path in &self.agent_override_files {
            writeln!(w, "override: {path}")?;
        }
        for provider in &self.provider_bindings {
            writeln!(w, "provider binding: {provider}")?;
        }
        if self.restart_required {
            writeln!(w, "restart required: {}", self.restart_agents.join(","))?;
        }
        Ok(())
    }
}

pub fn run(ctx: &Context, action: ApiAction) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: ApiAction,
) -> Result<ApiReport, NczError> {
    match action {
        ApiAction::List => Ok(ApiReport::List(list(ctx, paths)?)),
        ApiAction::Add {
            key,
            value,
            value_env,
            value_stdin,
            agents,
            providers,
        } => Ok(ApiReport::Mutate(upsert(
            ctx,
            paths,
            "add",
            &key,
            &resolve_value(value.as_deref(), value_env.as_deref(), value_stdin)?,
            &agents,
            &providers,
        )?)),
        ApiAction::Set {
            key,
            value,
            value_env,
            value_stdin,
            agents,
            providers,
        } => Ok(ApiReport::Mutate(upsert(
            ctx,
            paths,
            "set",
            &key,
            &resolve_value(value.as_deref(), value_env.as_deref(), value_stdin)?,
            &agents,
            &providers,
        )?)),
        ApiAction::Remove { key, force } => Ok(ApiReport::Mutate(remove(paths, &key, force)?)),
    }
}

pub fn list(ctx: &Context, paths: &Paths) -> Result<ApiListReport, NczError> {
    Ok(ApiListReport {
        schema_version: common::SCHEMA_VERSION,
        entries: agent_env::redacted_list(paths, ctx.show_secrets)?,
    })
}

fn upsert(
    ctx: &Context,
    paths: &Paths,
    action: &str,
    key: &str,
    value: &str,
    agents: &[String],
    providers: &[String],
) -> Result<ApiMutationReport, NczError> {
    agent_env::validate_key(key)?;
    agent_env::validate_value(value)?;
    for agent in agents {
        common::validate_agent(agent)?;
    }
    for provider in providers {
        provider_state::validate_name(provider)?;
    }
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let provider_bindings = resolve_provider_bindings(paths, key, providers)?;
    let mut target_paths: Vec<PathBuf> = vec![paths.agent_env()];
    target_paths.extend(agents.iter().map(|agent| paths.agent_env_override(agent)));
    let snapshots = snapshot_paths(&target_paths)?;

    let mut agent_override_files = Vec::new();
    let result = (|| -> Result<bool, NczError> {
        let mut changed = agent_env::set(paths, key, value)?;
        for agent_name in agents {
            if agent_env::set_override(paths, agent_name, key, value)? {
                changed = true;
            }
            agent_override_files.push(paths.agent_env_override(agent_name).display().to_string());
        }
        for binding in &provider_bindings {
            if agent_env::set_provider_binding(paths, &binding.provider, key, &binding.url)? {
                changed = true;
            }
        }
        Ok(changed)
    })();
    let changed = match result {
        Ok(changed) => changed,
        Err(err) => {
            restore_snapshots(&snapshots)?;
            return Err(err);
        }
    };

    Ok(ApiMutationReport {
        schema_version: common::SCHEMA_VERSION,
        action: action.to_string(),
        key: key.to_string(),
        value: Some(common::mask_secret_value(value, ctx.show_secrets)),
        changed,
        shared_file: paths.agent_env().display().to_string(),
        agent_override_files,
        provider_bindings: provider_bindings
            .into_iter()
            .map(|binding| binding.provider)
            .collect(),
        restart_required: true,
        restart_agents: restart_agents(),
    })
}

struct ProviderBinding {
    provider: String,
    url: String,
}

fn resolve_provider_bindings(
    paths: &Paths,
    key: &str,
    providers: &[String],
) -> Result<Vec<ProviderBinding>, NczError> {
    let mut bindings = Vec::new();
    for provider in providers {
        let record = provider_state::read(paths, provider)?
            .ok_or_else(|| NczError::Usage(format!("unknown provider: {provider}")))?;
        if record.declaration.key_env != key {
            return Err(NczError::Usage(format!(
                "provider {provider} references credential {}; cannot bind {key}",
                record.declaration.key_env
            )));
        }
        bindings.push(ProviderBinding {
            provider: record.declaration.name,
            url: record.declaration.url,
        });
    }
    Ok(bindings)
}

fn resolve_value(
    value: Option<&str>,
    value_env: Option<&str>,
    value_stdin: bool,
) -> Result<String, NczError> {
    let sources = (if value.is_some() { 1 } else { 0 })
        + (if value_env.is_some() { 1 } else { 0 })
        + (if value_stdin { 1 } else { 0 });
    if sources != 1 {
        return Err(NczError::Usage(
            "provide exactly one value source: --value-env VAR, --value-stdin, env:VAR, or -"
                .to_string(),
        ));
    }
    if let Some(source) = value {
        if source == "-" {
            return read_stdin_value();
        }
        if let Some(name) = source.strip_prefix("env:") {
            return read_env_value(name);
        }
        return Ok(source.to_string());
    }
    if let Some(name) = value_env {
        return read_env_value(name);
    }
    read_stdin_value()
}

fn read_env_value(name: &str) -> Result<String, NczError> {
    agent_env::validate_key(name)?;
    env::var(name)
        .map_err(|_| NczError::Precondition(format!("environment variable {name} is not set")))
}

fn read_stdin_value() -> Result<String, NczError> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
    Ok(value)
}

fn remove(paths: &Paths, key: &str, force: bool) -> Result<ApiMutationReport, NczError> {
    agent_env::validate_key(key)?;
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let references = if force {
        Vec::new()
    } else {
        credential_references(paths, key)?
    };
    if !references.is_empty() {
        return Err(NczError::Usage(format!(
            "credential {key} is still referenced by {}; use --force to remove anyway",
            references.join(", ")
        )));
    }
    let mut target_paths = vec![paths.agent_env()];
    target_paths.extend(
        agent::AGENTS
            .iter()
            .map(|agent_name| paths.agent_env_override(agent_name)),
    );
    let snapshots = snapshot_paths(&target_paths)?;

    let result = (|| -> Result<(bool, Vec<String>, Vec<String>), NczError> {
        let removed_key = agent_env::remove(paths, key)?;
        let removed_bindings = agent_env::remove_provider_bindings_for_key(paths, key)?;
        let mut removed_overrides = Vec::new();
        for agent_name in agent::AGENTS {
            if agent_env::remove_override(paths, agent_name, key)? {
                removed_overrides.push(paths.agent_env_override(agent_name).display().to_string());
            }
        }
        Ok((
            removed_key || !removed_bindings.is_empty() || !removed_overrides.is_empty(),
            removed_bindings,
            removed_overrides,
        ))
    })();
    let (changed, provider_bindings, agent_override_files) = match result {
        Ok(result) => result,
        Err(err) => {
            restore_snapshots(&snapshots)?;
            return Err(err);
        }
    };
    Ok(ApiMutationReport {
        schema_version: common::SCHEMA_VERSION,
        action: "remove".to_string(),
        key: key.to_string(),
        value: None,
        changed,
        shared_file: paths.agent_env().display().to_string(),
        agent_override_files,
        provider_bindings,
        restart_required: changed,
        restart_agents: if changed {
            restart_agents()
        } else {
            Vec::new()
        },
    })
}

fn restart_agents() -> Vec<String> {
    agent::AGENTS
        .iter()
        .map(|agent| (*agent).to_string())
        .collect()
}

fn credential_references(paths: &Paths, key: &str) -> Result<Vec<String>, NczError> {
    let mut references = Vec::new();
    for provider in provider_state::credential_references(paths, key)? {
        references.push(format!("provider:{provider}"));
    }
    for server in mcp_state::auth_references(paths, key)? {
        references.push(format!("mcp:{server}"));
    }
    references.sort();
    references.dedup();
    Ok(references)
}

struct FileSnapshot {
    path: PathBuf,
    body: Option<Vec<u8>>,
}

fn snapshot_paths(paths: &[PathBuf]) -> Result<Vec<FileSnapshot>, NczError> {
    paths.iter().map(|path| snapshot_path(path)).collect()
}

fn snapshot_path(path: &Path) -> Result<FileSnapshot, NczError> {
    let body = match fs::read(path) {
        Ok(body) => Some(body),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(e) => return Err(NczError::Io(e)),
    };
    Ok(FileSnapshot {
        path: path.to_path_buf(),
        body,
    })
}

fn restore_snapshots(snapshots: &[FileSnapshot]) -> Result<(), NczError> {
    for snapshot in snapshots.iter().rev() {
        match &snapshot.body {
            Some(body) => state::atomic_write(&snapshot.path, body, 0o600)?,
            None => {
                if let Err(err) = state::remove_file_durable(&snapshot.path) {
                    match err {
                        NczError::Io(io_err)
                            if matches!(
                                io_err.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                            ) => {}
                        other => return Err(other),
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;

    use crate::cli::ApiAction;
    use crate::cmd::common::test_paths;
    use crate::error::NczError;
    use crate::sys::fake::FakeRunner;

    use super::*;

    fn ctx<'a>(runner: &'a FakeRunner) -> Context<'a> {
        Context {
            json: false,
            show_secrets: false,
            runner,
        }
    }

    #[test]
    fn api_list_redacts_values() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\n").unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths).unwrap();

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.entries[0].key, "TOGETHER_API_KEY");
        assert_eq!(report.entries[0].value, "***");
    }

    #[test]
    fn api_add_writes_shared_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        env::set_var("NCZ_TEST_SHARED_API_SECRET", "secret");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_SHARED_API_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.value.as_deref(), Some("***"));
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, restart_agents());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
        assert!(report.agent_override_files.is_empty());
    }

    #[test]
    fn api_add_binds_credential_to_provider_for_live_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("together.json"),
            r#"{"schema_version":1,"name":"together","url":"https://api.example.test","model":"m","key_env":"TOGETHER_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        env::set_var("NCZ_TEST_BOUND_API_SECRET", "secret");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_BOUND_API_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: vec!["together".to_string()],
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert_eq!(report.provider_bindings, vec!["together"]);
        let entries = agent_env::read(&paths).unwrap();
        assert!(
            agent_env::provider_binding_matches(
                &entries,
                "together",
                "TOGETHER_API_KEY",
                "https://api.example.test"
            )
            .unwrap()
        );
    }

    #[test]
    fn api_add_writes_agent_override_stubs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        env::set_var("NCZ_TEST_SCOPED_API_SECRET", "secret");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_SCOPED_API_SECRET".to_string()),
                value_stdin: false,
                agents: vec!["zeroclaw".to_string(), "hermes".to_string()],
                providers: Vec::new(),
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert!(report.changed);
        assert_eq!(report.agent_override_files.len(), 2);
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("zeroclaw")).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("hermes")).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
    }

    #[test]
    fn api_scoped_set_writes_shared_and_override() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=old\nOTHER=1\n").unwrap();
        env::set_var("NCZ_TEST_SCOPED_REPLACE_API_SECRET", "secret");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_SCOPED_REPLACE_API_SECRET".to_string()),
                value_stdin: false,
                agents: vec!["hermes".to_string()],
                providers: Vec::new(),
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert!(report.changed);
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\nOTHER=1\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("hermes")).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
    }

    #[test]
    fn api_add_accepts_literal_argv_value() {
        assert_eq!(
            resolve_value(Some("secret"), None, false).unwrap(),
            "secret"
        );
    }

    #[test]
    fn api_remove_is_a_noop_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Remove {
                key: "TOGETHER_API_KEY".to_string(),
                force: false,
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert!(!report.changed);
        assert!(!report.restart_required);
    }

    #[test]
    fn api_remove_revokes_agent_override_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.agent_env_override("hermes").parent().unwrap()).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=shared\n").unwrap();
        fs::write(
            paths.agent_env_override("hermes"),
            "TOGETHER_API_KEY=override\nOTHER=1\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Remove {
                key: "TOGETHER_API_KEY".to_string(),
                force: false,
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, restart_agents());
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("hermes")).unwrap(),
            "OTHER=1\n"
        );
        assert_eq!(
            report.agent_override_files,
            vec![paths.agent_env_override("hermes").display().to_string()]
        );
    }

    #[test]
    fn api_remove_revokes_override_only_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.agent_env_override("hermes").parent().unwrap()).unwrap();
        fs::write(
            paths.agent_env_override("hermes"),
            "TOGETHER_API_KEY=override\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Remove {
                key: "TOGETHER_API_KEY".to_string(),
                force: false,
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(
            report.agent_override_files,
            vec![paths.agent_env_override("hermes").display().to_string()]
        );
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("hermes")).unwrap(),
            ""
        );
    }

    #[test]
    fn api_remove_rejects_provider_credential_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\n").unwrap();
        fs::write(
            paths.providers_dir().join("together.json"),
            r#"{"schema_version":1,"name":"together","url":"https://api.example.test","model":"m","key_env":"TOGETHER_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Remove {
                key: "TOGETHER_API_KEY".to_string(),
                force: false,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
    }

    #[test]
    fn api_remove_rejects_mcp_auth_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "MCP_TOKEN=secret\n").unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"stdio","command":"search-mcp","url":null,"auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Remove {
                key: "MCP_TOKEN".to_string(),
                force: false,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "MCP_TOKEN=secret\n"
        );
    }

    #[test]
    fn api_remove_force_removes_referenced_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\nOTHER=1\n").unwrap();
        fs::write(
            paths.providers_dir().join("together.json"),
            r#"{"schema_version":1,"name":"together","url":"https://api.example.test","model":"m","key_env":"TOGETHER_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Remove {
                key: "TOGETHER_API_KEY".to_string(),
                force: true,
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "OTHER=1\n");
    }

    #[test]
    fn api_remove_force_skips_broken_reference_declarations() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\nOTHER=1\n").unwrap();
        fs::write(paths.providers_dir().join("broken.json"), "{").unwrap();
        fs::write(paths.mcp_dir().join("broken.json"), "{").unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Remove {
                key: "TOGETHER_API_KEY".to_string(),
                force: true,
            },
        )
        .unwrap();

        let ApiReport::Mutate(report) = report else {
            panic!("expected mutation report");
        };
        assert!(report.changed);
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "OTHER=1\n");
    }

    #[test]
    fn api_add_validates_agents_before_writing_shared_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        env::set_var("NCZ_TEST_INVALID_AGENT_SECRET", "secret");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "TOGETHER_API_KEY".to_string(),
                value: Some("env:NCZ_TEST_INVALID_AGENT_SECRET".to_string()),
                value_env: None,
                value_stdin: false,
                agents: vec!["not-an-agent".to_string()],
                providers: Vec::new(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn api_add_rejects_agents_before_writing_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.agent_env_override("hermes").parent().unwrap()).unwrap();
        fs::write(paths.etc_dir.join("zeroclaw"), "not a directory").unwrap();
        env::set_var("NCZ_TEST_ROLLBACK_API_SECRET", "secret");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_ROLLBACK_API_SECRET".to_string()),
                value_stdin: false,
                agents: vec!["hermes".to_string(), "zeroclaw".to_string()],
                providers: Vec::new(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Io(_)));
        assert!(!paths.agent_env_override("hermes").exists());
        assert!(!paths.agent_env().exists());
    }
}
