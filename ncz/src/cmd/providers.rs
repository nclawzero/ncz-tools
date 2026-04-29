//! providers — list, probe, create, remove, show, and select LLM providers.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::{Context, ProvidersAction};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent, agent_env, providers as provider_state, Paths};

const HEALTH_PROBE_MAX_BYTES: &str = "65536";

#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ProvidersReport {
    List(ProvidersListReport),
    Test(ProviderTestReport),
    SetPrimary(ProviderSetPrimaryReport),
    Add(ProviderAddReport),
    Remove(ProviderRemoveReport),
    Show(ProviderShowReport),
}

impl Render for ProvidersReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            ProvidersReport::List(report) => report.render_text(w),
            ProvidersReport::Test(report) => report.render_text(w),
            ProvidersReport::SetPrimary(report) => report.render_text(w),
            ProvidersReport::Add(report) => report.render_text(w),
            ProvidersReport::Remove(report) => report.render_text(w),
            ProvidersReport::Show(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ProvidersListReport {
    pub schema_version: u32,
    pub primary: String,
    pub providers: Vec<ProviderReport>,
}

#[derive(Debug, Serialize)]
pub struct ProviderReport {
    pub name: String,
    pub url: String,
    pub model: String,
    pub key_env: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub health_path: String,
    pub health: String,
    pub file: String,
}

#[derive(Debug, Serialize)]
pub struct ProviderTestReport {
    pub schema_version: u32,
    pub name: String,
    pub health: String,
}

#[derive(Debug, Serialize)]
pub struct ProviderSetPrimaryReport {
    pub schema_version: u32,
    pub name: String,
    pub active_agent: String,
    pub primary_provider_file: String,
    pub agent_provider_file: String,
    pub agent_provider_files: Vec<String>,
    pub updated_agent_provider_files: Vec<String>,
    pub restart_required: bool,
    pub restart_agents: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProviderAddReport {
    pub schema_version: u32,
    pub provider: ProviderReport,
}

#[derive(Debug, Serialize)]
pub struct ProviderRemoveReport {
    pub schema_version: u32,
    pub name: String,
    pub removed: bool,
}

#[derive(Debug, Serialize)]
pub struct ProviderShowReport {
    pub schema_version: u32,
    pub provider: ProviderReport,
}

impl Render for ProvidersListReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "Primary: {}",
            if self.primary.is_empty() {
                "none"
            } else {
                &self.primary
            }
        )?;
        for provider in &self.providers {
            render_provider_line(w, provider)?;
        }
        Ok(())
    }
}

impl Render for ProviderTestReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Provider {}: {}", self.name, self.health)
    }
}

impl Render for ProviderSetPrimaryReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Primary provider: {}", self.name)?;
        if self.restart_required {
            writeln!(w, "Restart required: {}", self.restart_agents.join(", "))?;
        }
        Ok(())
    }
}

impl Render for ProviderAddReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Provider added: {}", self.provider.name)
    }
}

impl Render for ProviderRemoveReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.removed {
            writeln!(w, "Provider removed: {}", self.name)
        } else {
            writeln!(w, "Provider absent: {}", self.name)
        }
    }
}

impl Render for ProviderShowReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        render_provider_line(w, &self.provider)
    }
}

pub fn run(ctx: &Context, action: ProvidersAction) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: ProvidersAction,
) -> Result<ProvidersReport, NczError> {
    match action {
        ProvidersAction::List => Ok(ProvidersReport::List(list(ctx, paths)?)),
        ProvidersAction::Test { name } => {
            Ok(ProvidersReport::Test(test_provider(ctx, paths, &name)?))
        }
        ProvidersAction::SetPrimary { name } => {
            Ok(ProvidersReport::SetPrimary(set_primary(paths, &name)?))
        }
        ProvidersAction::Add {
            name,
            url,
            model,
            key_env,
            provider_type,
            health_path,
            force,
        } => Ok(ProvidersReport::Add(add(
            ctx,
            paths,
            ProviderAddInput {
                name,
                url,
                model,
                key_env,
                provider_type,
                health_path,
                force,
            },
        )?)),
        ProvidersAction::Remove {
            name,
            drop_inline_credentials,
        } => Ok(ProvidersReport::Remove(remove_with_options(
            paths,
            &name,
            drop_inline_credentials,
        )?)),
        ProvidersAction::Show { name } => Ok(ProvidersReport::Show(show(ctx, paths, &name)?)),
    }
}

pub fn list(ctx: &Context, paths: &Paths) -> Result<ProvidersListReport, NczError> {
    let primary = provider_state::read_primary(paths)?.unwrap_or_default();
    let mut providers = Vec::new();
    for record in provider_state::read_all(paths)? {
        providers.push(provider_report(ctx, &record));
    }
    Ok(ProvidersListReport {
        schema_version: common::SCHEMA_VERSION,
        primary,
        providers,
    })
}

pub fn test_provider(
    ctx: &Context,
    paths: &Paths,
    name: &str,
) -> Result<ProviderTestReport, NczError> {
    let record = provider_state::read(paths, name)?
        .ok_or_else(|| NczError::Usage(format!("unknown provider: {name}")))?;
    let health = provider_health(ctx, &record.declaration);
    if health == "ok" {
        Ok(ProviderTestReport {
            schema_version: common::SCHEMA_VERSION,
            name: name.to_string(),
            health,
        })
    } else {
        Err(NczError::Precondition(format!(
            "provider {name} smoke test failed ({health})"
        )))
    }
}

pub fn set_primary(paths: &Paths, name: &str) -> Result<ProviderSetPrimaryReport, NczError> {
    provider_state::validate_name(name)?;
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let active_agent = agent::read(paths)?;
    common::validate_agent(&active_agent)?;
    let agent_provider_file = paths.agent_primary_provider(&active_agent);
    let agent_provider_files = vec![(active_agent.clone(), agent_provider_file.clone())];
    let mut target_paths = vec![paths.agent_env(), paths.primary_provider()];
    target_paths.extend(agent_provider_files.iter().map(|(_, path)| path.clone()));
    target_paths.extend(provider_state::legacy_migration_snapshot_paths_for_provider(
        paths, name,
    )?);
    target_paths.sort();
    target_paths.dedup();
    let snapshots = snapshot_paths(&target_paths, 0o644)?;
    let agent_env_snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.path == paths.agent_env());
    let result = (|| -> Result<(String, bool, Vec<String>, Vec<String>), NczError> {
        provider_state::migrate_legacy_for_provider(paths, name)?;
        let record = provider_state::read(paths, name)?
            .ok_or_else(|| NczError::Usage(format!("unknown provider: {name}")))?;
        provider_state::validate_declaration(&record.declaration)?;
        require_provider_credential(paths, &record)?;
        let provider_name = record.declaration.name.clone();
        provider_state::write_primary(paths, &provider_name)?;
        let agent_provider_body = format!("{provider_name}\n");
        for (_, path) in &agent_provider_files {
            state::atomic_write(path, agent_provider_body.as_bytes(), 0o644)?;
        }
        let agent_env_changed = agent_env_snapshot
            .map(snapshot_changed)
            .transpose()?
            .unwrap_or(false);
        let primary_changed = snapshot_for_path(&snapshots, &paths.primary_provider())
            .map(snapshot_changed)
            .transpose()?
            .unwrap_or(false);
        let mut updated_agent_provider_files = Vec::new();
        let mut changed_agent_names = Vec::new();
        for (agent_name, path) in &agent_provider_files {
            if snapshot_for_path(&snapshots, path)
                .map(snapshot_changed)
                .transpose()?
                .unwrap_or(false)
            {
                updated_agent_provider_files.push(path.display().to_string());
                changed_agent_names.push(agent_name.clone());
            }
        }
        let restart_agents = if agent_env_changed || primary_changed {
            agent::AGENTS.iter().map(|agent| (*agent).to_string()).collect()
        } else {
            changed_agent_names
        };
        let restart_required = !restart_agents.is_empty();
        Ok((
            provider_name,
            restart_required,
            restart_agents,
            updated_agent_provider_files,
        ))
    })();
    let (provider_name, restart_required, restart_agents, updated_agent_provider_files) =
        match result {
            Ok(result) => result,
            Err(err) => {
                restore_snapshots(&snapshots)?;
                return Err(err);
            }
        };

    Ok(ProviderSetPrimaryReport {
        schema_version: common::SCHEMA_VERSION,
        name: provider_name,
        active_agent,
        primary_provider_file: paths.primary_provider().display().to_string(),
        agent_provider_file: agent_provider_file.display().to_string(),
        agent_provider_files: agent_provider_files
            .iter()
            .map(|(_, path)| path.display().to_string())
            .collect(),
        updated_agent_provider_files,
        restart_required,
        restart_agents,
    })
}

pub struct ProviderAddInput {
    pub name: String,
    pub url: String,
    pub model: String,
    pub key_env: String,
    pub provider_type: String,
    pub health_path: String,
    pub force: bool,
}

pub fn add(
    _ctx: &Context,
    paths: &Paths,
    input: ProviderAddInput,
) -> Result<ProviderAddReport, NczError> {
    let declaration = provider_state::ProviderDeclaration {
        schema_version: common::SCHEMA_VERSION,
        name: input.name,
        url: input.url,
        model: input.model,
        key_env: input.key_env,
        provider_type: input.provider_type,
        health_path: input.health_path,
        models: Vec::new(),
    };
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let mut revoked_binding_aliases = None;
    if input.force {
        let aliases = provider_state::removal_aliases(paths, &declaration.name)?;
        let inline_replacements =
            provider_state::inline_credential_replacements(paths, &declaration.name)?;
        if !inline_replacements.is_empty() {
            require_replacement_credentials_preserved(paths, &inline_replacements)?;
        }
        let primary_references = primary_references(paths, &aliases)?;
        if let Some((primary, path)) = primary_references.first() {
            return Err(NczError::Usage(format!(
                "provider {primary} is primary in {path}; run `ncz providers set-primary <other>` before replacing provider declarations"
            )));
        }
        let mut aliases_to_revoke = aliases;
        if provider_binding_matches_declaration(paths, &declaration)? {
            aliases_to_revoke.remove(&declaration.name);
        }
        revoked_binding_aliases = Some(aliases_to_revoke);
    }
    let path = if let Some(aliases) = revoked_binding_aliases {
        let snapshots = snapshot_paths(&[paths.agent_env()], 0o600)?;
        match (|| {
            agent_env::remove_provider_bindings_for_providers(paths, &aliases)?;
            provider_state::write(paths, &declaration, true)
        })() {
            Ok(path) => path,
            Err(err) => {
                restore_snapshots(&snapshots)?;
                return Err(err);
            }
        }
    } else {
        provider_state::write(paths, &declaration, false)?
    };
    Ok(ProviderAddReport {
        schema_version: common::SCHEMA_VERSION,
        provider: provider_report_unmeasured(&declaration, path.display().to_string()),
    })
}

pub fn remove(paths: &Paths, name: &str) -> Result<ProviderRemoveReport, NczError> {
    remove_with_options(paths, name, false)
}

pub fn remove_with_options(
    paths: &Paths,
    name: &str,
    drop_inline_credentials: bool,
) -> Result<ProviderRemoveReport, NczError> {
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let aliases = provider_state::removal_aliases(paths, name)?;
    reject_primary_references(paths, &aliases)?;
    if !drop_inline_credentials {
        let inline_replacements = provider_state::inline_credential_replacements(paths, name)?;
        if !inline_replacements.is_empty() {
            require_replacement_credentials_preserved(paths, &inline_replacements)?;
        }
    }
    let snapshots = snapshot_paths(&[paths.agent_env()], 0o600)?;
    let removed = match (|| {
        agent_env::remove_provider_bindings_for_providers(paths, &aliases)?;
        provider_state::remove(paths, name)
    })() {
        Ok(removed) => removed,
        Err(err) => {
            restore_snapshots(&snapshots)?;
            return Err(err);
        }
    };
    Ok(ProviderRemoveReport {
        schema_version: common::SCHEMA_VERSION,
        name: name.to_string(),
        removed,
    })
}

fn provider_binding_matches_declaration(
    paths: &Paths,
    declaration: &provider_state::ProviderDeclaration,
) -> Result<bool, NczError> {
    let entries = agent_env::read(paths)?;
    agent_env::provider_binding_matches(
        &entries,
        &declaration.name,
        &declaration.key_env,
        &declaration.url,
    )
}

pub fn show(ctx: &Context, paths: &Paths, name: &str) -> Result<ProviderShowReport, NczError> {
    let record = provider_state::read(paths, name)?
        .ok_or_else(|| NczError::Usage(format!("unknown provider: {name}")))?;
    Ok(ProviderShowReport {
        schema_version: common::SCHEMA_VERSION,
        provider: provider_report(ctx, &record),
    })
}

pub fn provider_health(ctx: &Context, provider: &provider_state::ProviderDeclaration) -> String {
    if provider.url.is_empty() {
        return "unknown".to_string();
    }
    let health_url = provider_url(&provider.url, &provider.health_path);
    if ctx
        .runner
        .run(
            "curl",
            &[
                "-q",
                "-fsS",
                "-o",
                "/dev/null",
                "--max-time",
                "3",
                "--max-filesize",
                HEALTH_PROBE_MAX_BYTES,
                "--noproxy",
                "*",
                "--proxy",
                "",
                "--",
                &health_url,
            ],
        )
        .map(|out| out.ok())
        .unwrap_or(false)
    {
        "ok".to_string()
    } else {
        "unhealthy".to_string()
    }
}

pub fn provider_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    format!("{base}{path}")
}

fn provider_report(ctx: &Context, record: &provider_state::ProviderRecord) -> ProviderReport {
    provider_report_from_parts(
        &record.declaration,
        record.path.display().to_string(),
        provider_health(ctx, &record.declaration),
    )
}

fn provider_report_from_parts(
    provider: &provider_state::ProviderDeclaration,
    file: String,
    health: String,
) -> ProviderReport {
    ProviderReport {
        name: provider.name.clone(),
        url: provider.url.clone(),
        model: provider.model.clone(),
        key_env: provider.key_env.clone(),
        provider_type: provider.provider_type.clone(),
        health_path: provider.health_path.clone(),
        health,
        file,
    }
}

fn provider_report_unmeasured(
    provider: &provider_state::ProviderDeclaration,
    file: String,
) -> ProviderReport {
    provider_report_from_parts(provider, file, "unknown".to_string())
}

fn render_provider_line(w: &mut dyn Write, provider: &ProviderReport) -> io::Result<()> {
    writeln!(
        w,
        "{:<18} health={:<10} url={} model={} key_env={} type={} health_path={}",
        provider.name,
        provider.health,
        if provider.url.is_empty() {
            "unknown"
        } else {
            &provider.url
        },
        if provider.model.is_empty() {
            "unknown"
        } else {
            &provider.model
        },
        if provider.key_env.is_empty() {
            "unknown"
        } else {
            &provider.key_env
        },
        provider.provider_type,
        provider.health_path
    )
}

fn reject_primary_references(
    paths: &Paths,
    names: &std::collections::BTreeSet<String>,
) -> Result<(), NczError> {
    if let Some((primary, path)) = primary_reference_path(paths, names)? {
        return Err(primary_remove_error(&primary, path));
    }
    Ok(())
}

fn primary_reference_path(
    paths: &Paths,
    names: &std::collections::BTreeSet<String>,
) -> Result<Option<(String, String)>, NczError> {
    Ok(primary_references(paths, names)?.into_iter().next())
}

fn primary_references(
    paths: &Paths,
    names: &std::collections::BTreeSet<String>,
) -> Result<Vec<(String, String)>, NczError> {
    let mut references = Vec::new();
    if let Some(primary) = read_primary_reference(&paths.primary_provider())? {
        if names.contains(&primary) {
            references.push((primary, paths.primary_provider().display().to_string()));
        }
    }
    for agent_name in agent::AGENTS {
        let path = paths.agent_primary_provider(agent_name);
        if let Some(primary) = common::read_first_line(&path)? {
            if names.contains(&primary) {
                references.push((primary, path.display().to_string()));
            }
        }
    }
    Ok(references)
}

fn read_primary_reference(path: &Path) -> Result<Option<String>, NczError> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(NczError::Io(e)),
    };
    let mut lines = body.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(primary) = lines.next() else {
        return Ok(None);
    };
    provider_state::validate_name(primary).map_err(|_| {
        NczError::Precondition(format!(
            "malformed primary provider state in {}: invalid provider name",
            path.display()
        ))
    })?;
    if lines.next().is_some() {
        return Err(NczError::Precondition(format!(
            "malformed primary provider state in {}: expected one provider name",
            path.display()
        )));
    }
    Ok(Some(primary.to_string()))
}

fn primary_remove_error(name: &str, path: String) -> NczError {
    NczError::Usage(format!(
        "provider {name} is primary in {path}; run `ncz providers set-primary <other>` first"
    ))
}

fn require_provider_credential(
    paths: &Paths,
    record: &provider_state::ProviderRecord,
) -> Result<(), NczError> {
    let declaration = &record.declaration;
    let entries = agent_env::read(paths)?;
    if entries
        .iter()
        .any(|entry| entry.key == declaration.key_env && !entry.value.is_empty())
    {
        if agent_env::provider_binding_matches(
            &entries,
            &declaration.name,
            &declaration.key_env,
            &declaration.url,
        )? {
            return Ok(());
        }
        return Err(NczError::Precondition(format!(
            "provider credential {} is not bound to provider {}; run `ncz api set {} --providers={}` to approve {}",
            declaration.key_env, declaration.name, declaration.key_env, declaration.name, declaration.url
        )));
    }
    if record
        .inline_secret
        .as_ref()
        .is_some_and(|secret| !secret.is_empty())
    {
        return Err(NczError::Precondition(format!(
            "provider {} still has an inline credential after migration; move it to agent-env and bind {} before setting primary",
            declaration.name, declaration.key_env
        )));
    }
    Err(NczError::Precondition(format!(
        "provider {} requires non-empty credential {} in agent-env",
        declaration.name, declaration.key_env
    )))
}

fn require_replacement_credentials_preserved(
    paths: &Paths,
    replacements: &[provider_state::InlineCredentialReplacement],
) -> Result<(), NczError> {
    let entries = agent_env::read(paths)?;
    for replacement in replacements {
        let preserved = entries
            .iter()
            .any(|entry| entry.key == replacement.key_env && entry.value == replacement.secret);
        if !preserved {
            return Err(NczError::Precondition(format!(
                "legacy provider {} contains an inline credential for {}; set the same value in agent-env before replacing or removing it",
                replacement.file, replacement.key_env
            )));
        }
    }
    Ok(())
}

struct FileSnapshot {
    path: PathBuf,
    body: Option<Vec<u8>>,
    mode: u32,
}

fn snapshot_paths(paths: &[PathBuf], missing_mode: u32) -> Result<Vec<FileSnapshot>, NczError> {
    paths
        .iter()
        .map(|path| snapshot_path(path, missing_mode))
        .collect()
}

fn snapshot_path(path: &Path, missing_mode: u32) -> Result<FileSnapshot, NczError> {
    match fs::read(path) {
        Ok(body) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path)?.permissions().mode() & 0o777;
            Ok(FileSnapshot {
                path: path.to_path_buf(),
                body: Some(body),
                mode,
            })
        }
        Err(e) => {
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) {
                return Ok(FileSnapshot {
                    path: path.to_path_buf(),
                    body: None,
                    mode: missing_mode,
                });
            }
            Err(NczError::Io(e))
        }
    }
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

fn snapshot_changed(snapshot: &FileSnapshot) -> Result<bool, NczError> {
    match fs::read(&snapshot.path) {
        Ok(body) => Ok(snapshot.body.as_deref() != Some(body.as_slice())),
        Err(e) if matches!(
            e.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        ) => Ok(snapshot.body.is_some()),
        Err(e) => Err(NczError::Io(e)),
    }
}

fn snapshot_for_path<'a>(snapshots: &'a [FileSnapshot], path: &Path) -> Option<&'a FileSnapshot> {
    snapshots.iter().find(|snapshot| snapshot.path == path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::cli::{Context, ProvidersAction};
    use crate::cmd::common::{out, test_paths};
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

    fn all_agents() -> Vec<String> {
        agent::AGENTS
            .iter()
            .map(|agent| (*agent).to_string())
            .collect()
    }

    fn expect_health(runner: &FakeRunner, url: &str, status: i32) {
        runner.expect(
            "curl",
            &[
                "-q",
                "-fsS",
                "-o",
                "/dev/null",
                "--max-time",
                "3",
                "--max-filesize",
                "65536",
                "--noproxy",
                "*",
                "--proxy",
                "",
                "--",
                url,
            ],
            out(status, "", ""),
        );
    }

    #[test]
    fn providers_list_happy_path_reads_json_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(paths.primary_provider(), "local\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{
  "schema_version": 1,
  "name": "local",
  "url": "http://127.0.0.1:8080",
  "model": "mini",
  "key_env": "LOCAL_API_KEY",
  "type": "openai-compat",
  "health_path": "/health"
}
"#,
        )
        .unwrap();
        let runner = FakeRunner::new();
        expect_health(&runner, "http://127.0.0.1:8080/health", 0);

        let report = list(&ctx(&runner), &paths).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.primary, "local");
        assert_eq!(report.providers[0].key_env, "LOCAL_API_KEY");
        assert_eq!(report.providers[0].health, "ok");
        runner.assert_done();
    }

    #[test]
    fn providers_list_reads_legacy_env_configs_without_migrating() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=abc\n",
        )
        .unwrap();
        let runner = FakeRunner::new();
        expect_health(&runner, "http://127.0.0.1:8080/health", 0);

        let report = list(&ctx(&runner), &paths).unwrap();

        assert_eq!(report.providers[0].name, "local");
        assert_eq!(report.providers[0].key_env, "LOCAL_API_KEY");
        assert_eq!(report.providers[0].health, "ok");
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(paths.providers_dir().join("local.env").exists());
        assert!(!paths.agent_env().exists());
        runner.assert_done();
    }

    #[test]
    fn providers_show_measures_health_without_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("bad.json"),
            r#"{"schema_version":1,"name":"bad","url":"https://bad.example","model":"m","key_env":"BAD_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();
        expect_health(&runner, "https://bad.example/health", 7);

        let report = show(&ctx(&runner), &paths, "bad").unwrap();

        assert_eq!(report.provider.health, "unhealthy");
        runner.assert_done();
    }

    #[test]
    fn providers_test_error_path_rejects_unhealthy_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("bad.json"),
            r#"{"schema_version":1,"name":"bad","url":"https://bad.example","model":"m","key_env":"BAD_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            "curl",
            &[
                "-q",
                "-fsS",
                "-o",
                "/dev/null",
                "--max-time",
                "3",
                "--max-filesize",
                "65536",
                "--noproxy",
                "*",
                "--proxy",
                "",
                "--",
                "https://bad.example/health",
            ],
            out(7, "", "failed\n"),
        );

        let err = test_provider(&ctx(&runner), &paths, "bad").unwrap_err();
        assert!(matches!(err, NczError::Precondition(_)));
    }

    #[test]
    fn providers_set_primary_happy_path_writes_global_and_agent_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "local",
            "LOCAL_API_KEY",
            "http://127.0.0.1:8080",
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap();

        let ProvidersReport::SetPrimary(report) = report else {
            panic!("expected set-primary report");
        };
        assert_eq!(report.schema_version, 1);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            report.agent_provider_files,
            vec![paths.agent_primary_provider("hermes").display().to_string()]
        );
        assert_eq!(
            report.updated_agent_provider_files,
            vec![paths.agent_primary_provider("hermes").display().to_string()]
        );
        assert_eq!(
            fs::read_to_string(paths.primary_provider()).unwrap(),
            "local\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("hermes")).unwrap(),
            "local\n"
        );
        assert!(!paths.agent_primary_provider("zeroclaw").exists());
        assert!(!paths.agent_primary_provider("openclaw").exists());
    }

    #[test]
    fn providers_set_primary_rejects_unbound_existing_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.attacker.test","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("not bound")));
        assert!(!paths.primary_provider().exists());
    }

    #[test]
    fn providers_set_primary_accepts_legacy_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=legacy\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap();

        let ProvidersReport::SetPrimary(report) = report else {
            panic!("expected set-primary report");
        };
        assert_eq!(report.name, "local");
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            fs::read_to_string(paths.primary_provider()).unwrap(),
            "local\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("hermes")).unwrap(),
            "local\n"
        );
        assert!(!paths.agent_primary_provider("zeroclaw").exists());
        assert!(!paths.agent_primary_provider("openclaw").exists());
        assert!(paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("local.env").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=legacy\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn providers_set_primary_rejects_conflicting_legacy_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "local",
            "LOCAL_API_KEY",
            "https://api.old.test",
        )
        .unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=local\nPROVIDER_URL=https://api.new.test\nMODEL=mini\nKEY_ENV=LOCAL_API_KEY\nLOCAL_API_KEY=secret\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("different key or URL"))
        );
        assert!(!paths.primary_provider().exists());
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(agent_env::provider_binding_matches(
            &agent_env::read(&paths).unwrap(),
            "local",
            "LOCAL_API_KEY",
            "https://api.old.test"
        )
        .unwrap());
    }

    #[test]
    fn providers_set_primary_legacy_migration_restarts_active_agent_for_agent_env_change() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.primary_provider(), "local\n").unwrap();
        for agent_name in agent::AGENTS {
            fs::create_dir_all(paths.agent_primary_provider(agent_name).parent().unwrap()).unwrap();
            fs::write(paths.agent_primary_provider(agent_name), "local\n").unwrap();
        }
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=legacy\n",
        )
        .unwrap();

        let report = set_primary(&paths, "local").unwrap();

        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=legacy\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn providers_set_primary_reports_restart_when_provider_files_change() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.primary_provider(), "old\n").unwrap();
        for agent_name in agent::AGENTS {
            fs::create_dir_all(paths.agent_primary_provider(agent_name).parent().unwrap()).unwrap();
            fs::write(paths.agent_primary_provider(agent_name), "old\n").unwrap();
        }
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "local",
            "LOCAL_API_KEY",
            "http://127.0.0.1:8080",
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("old.json"),
            r#"{"schema_version":1,"name":"old","url":"http://127.0.0.1:9090","model":"m","key_env":"OLD_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();

        let report = set_primary(&paths, "local").unwrap();

        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            fs::read_to_string(paths.primary_provider()).unwrap(),
            "local\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("hermes")).unwrap(),
            "local\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("zeroclaw")).unwrap(),
            "old\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("openclaw")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn providers_set_primary_preserves_inactive_agent_primary_references() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.primary_provider(), "old\n").unwrap();
        for agent_name in agent::AGENTS {
            fs::create_dir_all(paths.agent_primary_provider(agent_name).parent().unwrap()).unwrap();
            fs::write(paths.agent_primary_provider(agent_name), "old\n").unwrap();
        }
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "local",
            "LOCAL_API_KEY",
            "http://127.0.0.1:8080",
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("old.json"),
            r#"{"schema_version":1,"name":"old","url":"http://127.0.0.1:9090","model":"m","key_env":"OLD_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();

        set_primary(&paths, "local").unwrap();

        assert!(paths.providers_dir().join("old.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("hermes")).unwrap(),
            "local\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("zeroclaw")).unwrap(),
            "old\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("openclaw")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn providers_set_primary_unknown_provider_rolls_back_legacy_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy =
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=legacy\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "missing".to_string(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(message) if message.contains("unknown provider")));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
        assert!(!paths.primary_provider().exists());
        assert!(!paths.agent_primary_provider("hermes").exists());
    }

    #[test]
    fn providers_set_primary_rejects_missing_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.primary_provider().exists());
    }

    #[test]
    fn providers_set_primary_rejects_empty_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.primary_provider().exists());
    }

    #[test]
    fn providers_set_primary_uses_effective_duplicate_credential_value() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\nLOCAL_API_KEY=\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.primary_provider().exists());
    }

    #[test]
    fn providers_set_primary_rejects_invalid_active_agent_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.primary_provider(), "old\n").unwrap();
        fs::write(paths.agent_state(), "../escape\n").unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert_eq!(
            fs::read_to_string(paths.primary_provider()).unwrap(),
            "old\n"
        );
        assert!(!paths.etc_dir.join("escape/primary-provider").exists());
    }

    #[test]
    fn providers_set_primary_rolls_back_global_on_agent_write_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.primary_provider(), "old\n").unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(paths.agent_config_dir(), "not a directory").unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "local",
            "LOCAL_API_KEY",
            "http://127.0.0.1:8080",
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Io(_)));
        assert_eq!(
            fs::read_to_string(paths.primary_provider()).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn providers_set_primary_rejects_invalid_legacy_provider_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::SetPrimary {
                name: "local".to_string(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.primary_provider().exists());
        assert!(paths.providers_dir().join("local.env").exists());
    }

    #[test]
    fn providers_add_writes_canonical_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "example".to_string(),
                url: "https://api.example.test".to_string(),
                model: "model-a".to_string(),
                key_env: "EXAMPLE_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: false,
            },
        )
        .unwrap();

        let ProvidersReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert_eq!(report.provider.name, "example");
        assert_eq!(report.provider.health, "unknown");
        assert!(paths.providers_dir().join("example.json").exists());
        runner.assert_done();
    }

    #[test]
    fn providers_add_rejects_credentialed_remote_plaintext_http() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "example".to_string(),
                url: "http://api.example.test".to_string(),
                model: "model-a".to_string(),
                key_env: "EXAMPLE_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: false,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert!(!paths.providers_dir().join("example.json").exists());
    }

    #[test]
    fn providers_add_rejects_absolute_health_path() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "example".to_string(),
                url: "https://api.example.test".to_string(),
                model: "model-a".to_string(),
                key_env: "EXAMPLE_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "http://169.254.169.254/latest/meta-data".to_string(),
                force: false,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert!(!paths.providers_dir().join("example.json").exists());
    }

    #[test]
    fn providers_remove_rejects_primary() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.primary_provider(), "example\n").unwrap();

        let err = remove(&paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn providers_remove_revokes_provider_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("example.json"),
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"old","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(
            paths.agent_env(),
            "EXAMPLE_API_KEY=secret\nNCZ_PROVIDER_BINDING_6578616D706C65=\"EXAMPLE_API_KEY https://api.example.test\"\nOTHER=1\n",
        )
        .unwrap();

        let report = remove(&paths, "example").unwrap();

        assert!(report.removed);
        assert!(!paths.providers_dir().join("example.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "EXAMPLE_API_KEY=secret\nOTHER=1\n"
        );
    }

    #[test]
    fn providers_remove_rejects_unpreserved_legacy_inline_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy =
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=abc\n";
        fs::write(&legacy_file, legacy).unwrap();

        let err = remove(&paths, "local").unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("inline credential"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn providers_remove_can_drop_legacy_inline_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        fs::write(
            &legacy_file,
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=LOCAL_API_KEY\nPROXY_TOKEN=proxy-secret\n",
        )
        .unwrap();

        let report = remove_with_options(&paths, "local", true).unwrap();

        assert!(report.removed);
        assert!(!legacy_file.exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn providers_remove_allows_preserved_legacy_inline_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        fs::write(
            &legacy_file,
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=abc\n",
        )
        .unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=abc\n").unwrap();

        let report = remove(&paths, "local").unwrap();

        assert!(report.removed);
        assert!(!legacy_file.exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=abc\n"
        );
    }

    #[test]
    fn providers_remove_rolls_back_binding_on_remove_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let provider_file = paths.providers_dir().join("example.json");
        fs::write(
            &provider_file,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"old","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::create_dir(paths.providers_dir().join("example.models.json")).unwrap();
        let agent_env_body =
            "EXAMPLE_API_KEY=secret\nNCZ_PROVIDER_BINDING_6578616D706C65=\"EXAMPLE_API_KEY https://api.example.test\"\n";
        fs::write(paths.agent_env(), agent_env_body).unwrap();

        let err = remove(&paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Io(_)));
        assert!(provider_file.exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            agent_env_body
        );
    }

    #[test]
    fn providers_add_force_rejects_primary_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.primary_provider(), "example\n").unwrap();
        fs::write(paths.agent_env(), "OLD_API_KEY=old\nNEW_API_KEY=new\n").unwrap();
        let provider_file = paths.providers_dir().join("example.json");
        let original = r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"old","key_env":"OLD_API_KEY","type":"openai-compat","health_path":"/health"}"#;
        fs::write(&provider_file, original).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "example".to_string(),
                url: "https://api.example.test".to_string(),
                model: "new".to_string(),
                key_env: "NEW_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert_eq!(fs::read_to_string(provider_file).unwrap(), original);
    }

    #[test]
    fn providers_add_force_rejects_malformed_global_primary_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.primary_provider(), "example\nbackup\n").unwrap();
        fs::write(paths.agent_env(), "OLD_API_KEY=old\nNEW_API_KEY=new\n").unwrap();
        let provider_file = paths.providers_dir().join("example.json");
        let original = r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"old","key_env":"OLD_API_KEY","type":"openai-compat","health_path":"/health"}"#;
        fs::write(&provider_file, original).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "example".to_string(),
                url: "https://api.example.test".to_string(),
                model: "new".to_string(),
                key_env: "NEW_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("malformed primary provider state"))
        );
        assert_eq!(fs::read_to_string(provider_file).unwrap(), original);
    }

    #[test]
    fn providers_add_force_rejects_primary_legacy_alias_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.primary_provider(), "prod\n").unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=prod\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "local".to_string(),
                url: "http://127.0.0.1:8080".to_string(),
                model: "new".to_string(),
                key_env: "LOCAL_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert_eq!(
            fs::read_to_string(paths.primary_provider()).unwrap(),
            "prod\n"
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
    }

    #[test]
    fn providers_add_force_rejects_secret_bearing_legacy_without_rebound_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy =
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nOPENAI_API_KEY=old\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "local".to_string(),
                url: "http://127.0.0.1:8080".to_string(),
                model: "new".to_string(),
                key_env: "LOCAL_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
    }

    #[test]
    fn providers_add_force_rejects_custom_key_env_legacy_secret_without_rebound_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=FOO\nFOO=sk-live\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "local".to_string(),
                url: "http://127.0.0.1:8080".to_string(),
                model: "new".to_string(),
                key_env: "FOO".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("FOO")));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
    }

    #[test]
    fn providers_add_force_rejects_unmigratable_nested_legacy_json_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","headers":[{"proxy_token":"proxy-secret"}]}"#;
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "local".to_string(),
                url: "http://127.0.0.1:8080".to_string(),
                model: "new".to_string(),
                key_env: "LOCAL_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
    }

    #[test]
    fn providers_add_force_rejects_unmigratable_legacy_env_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=LOCAL_API_KEY\nPROXY_TOKEN=proxy-secret\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "local".to_string(),
                url: "http://127.0.0.1:8080".to_string(),
                model: "new".to_string(),
                key_env: "LOCAL_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("PROXY_TOKEN")));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
    }

    #[test]
    fn providers_add_force_rejects_mismatched_legacy_inline_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=different\n").unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy =
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=old-secret\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "local".to_string(),
                url: "http://127.0.0.1:8080".to_string(),
                model: "new".to_string(),
                key_env: "LOCAL_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("same value in agent-env"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
    }

    #[test]
    fn providers_add_force_rejects_multiple_canonical_inline_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=old-secret\n").unwrap();
        let original_file = paths.providers_dir().join("local.json");
        let original = r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health","api_key":"old-secret","proxy_token":"other-secret"}"#;
        fs::write(&original_file, original).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "local".to_string(),
                url: "http://127.0.0.1:8080".to_string(),
                model: "new".to_string(),
                key_env: "LOCAL_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("multiple inline credential fields"))
        );
        assert_eq!(fs::read_to_string(original_file).unwrap(), original);
    }

    #[test]
    fn providers_add_force_preserves_matching_provider_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("example.json"),
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"old","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(
            paths.agent_env(),
            "EXAMPLE_API_KEY=secret\nNCZ_PROVIDER_BINDING_6578616D706C65=\"EXAMPLE_API_KEY https://api.example.test\"\nOTHER=1\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "example".to_string(),
                url: "https://api.example.test".to_string(),
                model: "new".to_string(),
                key_env: "EXAMPLE_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "EXAMPLE_API_KEY=secret\nNCZ_PROVIDER_BINDING_6578616D706C65=\"EXAMPLE_API_KEY https://api.example.test\"\nOTHER=1\n"
        );
        assert!(
            fs::read_to_string(paths.providers_dir().join("example.json"))
                .unwrap()
                .contains(r#""model": "new""#)
        );
        runner.assert_done();
    }

    #[test]
    fn providers_add_force_revokes_provider_binding_when_url_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("example.json"),
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"old","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(
            paths.agent_env(),
            "EXAMPLE_API_KEY=secret\nNCZ_PROVIDER_BINDING_6578616D706C65=\"EXAMPLE_API_KEY https://api.example.test\"\nOTHER=1\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "example".to_string(),
                url: "https://api2.example.test".to_string(),
                model: "new".to_string(),
                key_env: "EXAMPLE_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "EXAMPLE_API_KEY=secret\nOTHER=1\n"
        );
        assert!(
            fs::read_to_string(paths.providers_dir().join("example.json"))
                .unwrap()
                .contains(r#""url": "https://api2.example.test""#)
        );
        runner.assert_done();
    }

    #[test]
    fn providers_add_force_rolls_back_binding_on_write_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let provider_file = paths.providers_dir().join("example.json");
        let original = r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"old","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#;
        fs::write(&provider_file, original).unwrap();
        fs::create_dir(paths.providers_dir().join("example.models.json")).unwrap();
        let agent_env_body =
            "EXAMPLE_API_KEY=secret\nNCZ_PROVIDER_BINDING_6578616D706C65=\"EXAMPLE_API_KEY https://api.example.test\"\n";
        fs::write(paths.agent_env(), agent_env_body).unwrap();
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ProvidersAction::Add {
                name: "example".to_string(),
                url: "https://api.example.test".to_string(),
                model: "new".to_string(),
                key_env: "EXAMPLE_API_KEY".to_string(),
                provider_type: "openai-compat".to_string(),
                health_path: "/health".to_string(),
                force: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Io(_)));
        assert_eq!(fs::read_to_string(provider_file).unwrap(), original);
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            agent_env_body
        );
    }

    #[test]
    fn providers_remove_rejects_agent_primary_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.agent_primary_provider("hermes").parent().unwrap()).unwrap();
        fs::write(paths.agent_primary_provider("hermes"), "example\n").unwrap();

        let err = remove(&paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn providers_remove_rejects_malformed_global_primary_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.primary_provider(), "example\nbackup\n").unwrap();
        let provider_file = paths.providers_dir().join("example.json");
        let original = r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"old","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#;
        fs::write(&provider_file, original).unwrap();

        let err = remove(&paths, "example").unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("malformed primary provider state"))
        );
        assert_eq!(fs::read_to_string(provider_file).unwrap(), original);
    }

    #[test]
    fn providers_remove_rejects_primary_reference_to_legacy_declared_name() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.primary_provider(), "prod\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=prod\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=abc\n",
        )
        .unwrap();

        let err = remove(&paths, "local").unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert!(paths.providers_dir().join("local.env").exists());
    }

    #[test]
    fn providers_json_keeps_action_discriminator() {
        let value = serde_json::to_value(ProvidersReport::List(ProvidersListReport {
            schema_version: 1,
            primary: String::new(),
            providers: Vec::new(),
        }))
        .unwrap();

        assert_eq!(value["action"], "list");
        assert_eq!(value["schema_version"], 1);
    }
}
