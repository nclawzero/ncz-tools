//! api — manage the shared agent credential environment.

use std::collections::BTreeSet;
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

const MAX_CREDENTIAL_VALUE_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ApiReport {
    List(ApiListReport),
    Add(ApiMutationReport),
    Set(ApiMutationReport),
    Remove(ApiMutationReport),
}

impl Render for ApiReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            ApiReport::List(report) => report.render_text(w),
            ApiReport::Add(report) => report.render_text(w),
            ApiReport::Set(report) => report.render_text(w),
            ApiReport::Remove(report) => report.render_text(w),
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
    #[serde(skip)]
    pub action: String,
    pub key: String,
    pub value: Option<String>,
    pub changed: bool,
    pub shared_file: String,
    pub agent_override_files: Vec<String>,
    pub provider_bindings: Vec<String>,
    pub mcp_bindings: Vec<String>,
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
            _ => writeln!(w, "{} unchanged", self.key)?,
        }
        for path in &self.agent_override_files {
            writeln!(w, "override: {path}")?;
        }
        for provider in &self.provider_bindings {
            writeln!(w, "provider binding: {provider}")?;
        }
        for server in &self.mcp_bindings {
            writeln!(w, "mcp binding: {server}")?;
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
        } => Ok(ApiReport::Add(upsert(
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
        } => Ok(ApiReport::Set(upsert(
            ctx,
            paths,
            "set",
            &key,
            &resolve_value(value.as_deref(), value_env.as_deref(), value_stdin)?,
            &agents,
            &providers,
        )?)),
        ApiAction::Remove { key, force } => Ok(ApiReport::Remove(remove(paths, &key, force)?)),
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
    agent_env::validate_public_key(key)?;
    agent_env::validate_value(value)?;
    validate_value_size(value)?;
    if value.is_empty() {
        return Err(NczError::Usage(format!(
            "credential {key} value cannot be empty; use `ncz api remove {key}` to remove it"
        )));
    }
    for agent in agents {
        common::validate_agent(agent)?;
    }
    for provider in providers {
        provider_state::validate_name(provider)?;
    }
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let (override_agents, mut target_paths) = credential_upsert_targets(paths, key, agents)?;
    let cache_providers = model_cache_providers_for_key(paths, key, providers)?;
    if !providers.is_empty() {
        target_paths.extend(provider_state::legacy_migration_snapshot_paths_for_providers(
            paths, providers,
        )?);
    }
    target_paths.sort();
    target_paths.dedup();
    let snapshots = snapshot_paths(&target_paths)?;

    let result = (|| -> Result<
        (
            bool,
            bool,
            CredentialWriteOutcome,
            Vec<ProviderBinding>,
            Vec<String>,
            Vec<String>,
        ),
        NczError,
    > {
        if !providers.is_empty() {
            require_inline_provider_credentials_safe_for_upsert(paths, key, value, providers)?;
        }
        let migrated = if providers.is_empty() {
            false
        } else {
            !provider_state::migrate_legacy_for_providers(paths, providers)?.is_empty()
        };
        let provider_bindings = resolve_provider_bindings(paths, key, providers)?;
        let previous_shared_value = shared_credential_value(paths, key)?;
        let shared_value_changed = previous_shared_value.as_deref() != Some(value);
        let mut global_restart = migrated;
        let mut invalidate_caches = migrated || shared_value_changed;
        let mut revoked_provider_bindings = Vec::new();
        let mut revoked_mcp_bindings = Vec::new();
        if shared_value_changed {
            revoked_provider_bindings = agent_env::remove_provider_bindings_for_key(paths, key)?;
            revoked_mcp_bindings = agent_env::remove_mcp_bindings_for_key(paths, key)?;
            if !revoked_provider_bindings.is_empty() || !revoked_mcp_bindings.is_empty() {
                global_restart = true;
                invalidate_caches = true;
            }
        }
        let credential_write = set_credential_value_locked(paths, key, value, &override_agents)?;
        if credential_write.shared_changed {
            global_restart = true;
        }
        for binding in &provider_bindings {
            if agent_env::set_provider_binding(paths, &binding.provider, key, &binding.url)? {
                global_restart = true;
                invalidate_caches = true;
            }
        }
        Ok((
            global_restart,
            invalidate_caches,
            credential_write,
            provider_bindings,
            revoked_provider_bindings,
            revoked_mcp_bindings,
        ))
    })();
    let (
        global_restart,
        invalidate_caches,
        credential_write,
        provider_bindings,
        revoked_provider_bindings,
        revoked_mcp_bindings,
    ) = match result {
        Ok(result) => result,
        Err(err) => {
            restore_snapshots(&snapshots)?;
            return Err(err);
        }
    };
    let mut changed = global_restart || !credential_write.changed_override_agents.is_empty();
    if invalidate_caches && !remove_model_caches_best_effort(paths, &cache_providers).is_empty() {
        changed = true;
    }
    let restart_agents = credential_restart_agents(
        global_restart,
        &credential_write.changed_override_agents,
    );
    let restart_required = !restart_agents.is_empty();

    let mut provider_binding_names: Vec<String> = provider_bindings
        .into_iter()
        .map(|binding| binding.provider)
        .collect();
    provider_binding_names.extend(revoked_provider_bindings);
    provider_binding_names.sort();
    provider_binding_names.dedup();

    Ok(ApiMutationReport {
        schema_version: common::SCHEMA_VERSION,
        action: action.to_string(),
        key: key.to_string(),
        value: Some(common::mask_secret_value(value, ctx.show_secrets)),
        changed,
        shared_file: paths.agent_env().display().to_string(),
        agent_override_files: credential_write.agent_override_files,
        provider_bindings: provider_binding_names,
        mcp_bindings: revoked_mcp_bindings,
        restart_required,
        restart_agents,
    })
}

struct ProviderBinding {
    provider: String,
    url: String,
}

pub(crate) fn shared_credential_value(
    paths: &Paths,
    key: &str,
) -> Result<Option<String>, NczError> {
    Ok(agent_env::read(paths)?
        .into_iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.value))
}

#[derive(Debug)]
pub(crate) struct CredentialWriteOutcome {
    pub shared_changed: bool,
    pub changed_override_agents: Vec<String>,
    pub agent_override_files: Vec<String>,
}

pub(crate) fn credential_upsert_targets(
    paths: &Paths,
    key: &str,
    requested_agents: &[String],
) -> Result<(Vec<String>, Vec<PathBuf>), NczError> {
    let override_agents = override_agents_for_upsert(paths, key, requested_agents)?;
    let mut target_paths: Vec<PathBuf> = vec![paths.agent_env()];
    target_paths.extend(
        override_agents
            .iter()
            .map(|agent| paths.agent_env_override(agent)),
    );
    target_paths.sort();
    target_paths.dedup();
    Ok((override_agents, target_paths))
}

pub(crate) fn set_credential_value_locked(
    paths: &Paths,
    key: &str,
    value: &str,
    override_agents: &[String],
) -> Result<CredentialWriteOutcome, NczError> {
    let shared_changed = agent_env::set(paths, key, value)?;
    let mut changed_override_agents = Vec::new();
    let mut agent_override_files = Vec::new();
    for agent_name in override_agents {
        if agent_env::set_override(paths, agent_name, key, value)? {
            changed_override_agents.push(agent_name.clone());
        }
        agent_override_files.push(paths.agent_env_override(agent_name).display().to_string());
    }
    Ok(CredentialWriteOutcome {
        shared_changed,
        changed_override_agents,
        agent_override_files,
    })
}

pub(crate) fn credential_restart_agents(
    global_changed: bool,
    changed_override_agents: &[String],
) -> Vec<String> {
    let mut agents = Vec::new();
    if global_changed {
        agents.extend(agent::AGENTS.iter().map(|agent| (*agent).to_string()));
    }
    for agent in changed_override_agents {
        if common::validate_agent(agent).is_ok() && !agents.contains(agent) {
            agents.push(agent.clone());
        }
    }
    agents
}

fn override_agents_for_upsert(
    paths: &Paths,
    key: &str,
    requested_agents: &[String],
) -> Result<Vec<String>, NczError> {
    let mut agents: BTreeSet<String> = requested_agents.iter().cloned().collect();
    for agent_name in agent::AGENTS {
        if agents.contains(*agent_name) {
            continue;
        }
        if override_contains_key_if_regular(paths, agent_name, key)? {
            agents.insert(agent_name.to_string());
        }
    }
    Ok(agents.into_iter().collect())
}

fn override_agents_with_key(paths: &Paths, key: &str) -> Result<Vec<String>, NczError> {
    let mut agents = Vec::new();
    for agent_name in agent::AGENTS {
        if override_contains_key_if_regular(paths, agent_name, key)? {
            agents.push(agent_name.to_string());
        }
    }
    Ok(agents)
}

fn override_contains_key_if_regular(
    paths: &Paths,
    agent_name: &str,
    key: &str,
) -> Result<bool, NczError> {
    let path = paths.agent_env_override(agent_name);
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(agent_env::read_override(paths, agent_name)?
            .iter()
            .any(|entry| entry.key == key)),
        Ok(_) => Ok(false),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {
            Ok(false)
        }
        Err(e) => Err(NczError::Io(e)),
    }
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

fn require_inline_provider_credentials_safe_for_upsert(
    paths: &Paths,
    key: &str,
    value: &str,
    providers: &[String],
) -> Result<(), NczError> {
    let entries = agent_env::read(paths)?;
    for provider in providers {
        for replacement in provider_state::inline_credential_replacements(paths, provider)? {
            if replacement.key_env != key || replacement.secret == value {
                continue;
            }
            let preserved = entries
                .iter()
                .any(|entry| entry.key == replacement.key_env && entry.value == replacement.secret);
            if !preserved {
                return Err(NczError::Precondition(format!(
                    "legacy provider {} contains an inline credential for {}; set the same value in agent-env before rotating with --providers",
                    replacement.file, replacement.key_env
                )));
            }
        }
    }
    Ok(())
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
    let stdin = io::stdin();
    read_value_from_reader(stdin.lock())
}

fn read_value_from_reader<R: Read>(mut reader: R) -> Result<String, NczError> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((MAX_CREDENTIAL_VALUE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CREDENTIAL_VALUE_BYTES {
        return Err(value_too_large_error());
    }
    let mut value = String::from_utf8(bytes).map_err(|_| {
        NczError::Usage("credential value from stdin must be valid UTF-8".to_string())
    })?;
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
    validate_value_size(&value)?;
    Ok(value)
}

fn validate_value_size(value: &str) -> Result<(), NczError> {
    if value.len() > MAX_CREDENTIAL_VALUE_BYTES {
        return Err(value_too_large_error());
    }
    Ok(())
}

fn value_too_large_error() -> NczError {
    NczError::Usage(format!(
        "credential value exceeds {MAX_CREDENTIAL_VALUE_BYTES} bytes"
    ))
}

fn remove(paths: &Paths, key: &str, force: bool) -> Result<ApiMutationReport, NczError> {
    remove_with_cache_remover(paths, key, force, state::remove_file_durable)
}

fn remove_with_cache_remover<F>(
    paths: &Paths,
    key: &str,
    force: bool,
    mut remove_cache: F,
) -> Result<ApiMutationReport, NczError>
where
    F: FnMut(&Path) -> Result<(), NczError>,
{
    agent_env::validate_public_key(key)?;
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
    let cache_providers = if force {
        agent_env::provider_bindings_for_key_lenient(paths, key)?
    } else {
        agent_env::provider_bindings_for_key(paths, key)?
    };
    let cache_paths = model_cache_paths_for_removal(paths, &cache_providers)?;
    let override_agents = override_agents_with_key(paths, key)?;
    let mut target_paths = vec![paths.agent_env()];
    target_paths.extend(
        override_agents
            .iter()
            .map(|agent_name| paths.agent_env_override(agent_name)),
    );
    target_paths.extend(cache_paths);
    let snapshots = snapshot_paths(&target_paths)?;

    let result =
        (|| -> Result<(bool, bool, Vec<String>, Vec<String>, Vec<String>, Vec<String>), NczError> {
        let removed_key = agent_env::remove(paths, key)?;
        let removed_bindings = if force {
            agent_env::remove_provider_bindings_for_key_lenient(paths, key)?
        } else {
            agent_env::remove_provider_bindings_for_key(paths, key)?
        };
        let removed_mcp_bindings = if force {
            agent_env::remove_mcp_bindings_for_key_lenient(paths, key)?
        } else {
            agent_env::remove_mcp_bindings_for_key(paths, key)?
        };
        let mut removed_overrides = Vec::new();
        let mut changed_override_agents = Vec::new();
        for agent_name in &override_agents {
            if agent_env::remove_override(paths, agent_name, key)? {
                removed_overrides.push(paths.agent_env_override(agent_name).display().to_string());
                changed_override_agents.push(agent_name.clone());
            }
        }
        let removed_caches =
            remove_model_caches_transactional(paths, &cache_providers, &mut remove_cache)?;
        let global_restart =
            removed_key || !removed_bindings.is_empty() || !removed_mcp_bindings.is_empty();
        Ok((
            removed_key
                || !removed_bindings.is_empty()
                || !removed_mcp_bindings.is_empty()
                || !removed_overrides.is_empty()
                || !removed_caches.is_empty(),
            global_restart,
            removed_bindings,
            removed_mcp_bindings,
            removed_overrides,
            changed_override_agents,
        ))
    })();
    let (
        changed,
        global_restart,
        provider_bindings,
        mcp_bindings,
        agent_override_files,
        changed_override_agents,
    ) = match result {
        Ok(result) => result,
        Err(err) => {
            restore_snapshots(&snapshots)?;
            return Err(err);
        }
    };
    let restart_agents = credential_restart_agents(global_restart, &changed_override_agents);
    let restart_required = !restart_agents.is_empty();
    Ok(ApiMutationReport {
        schema_version: common::SCHEMA_VERSION,
        action: "remove".to_string(),
        key: key.to_string(),
        value: None,
        changed,
        shared_file: paths.agent_env().display().to_string(),
        agent_override_files,
        provider_bindings,
        mcp_bindings,
        restart_required,
        restart_agents,
    })
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

fn model_cache_providers_for_key(
    paths: &Paths,
    key: &str,
    providers: &[String],
) -> Result<Vec<String>, NczError> {
    let mut cache_providers = agent_env::provider_bindings_for_key(paths, key)?;
    cache_providers.extend(providers.iter().cloned());
    cache_providers.sort();
    cache_providers.dedup();
    Ok(cache_providers)
}

fn remove_model_caches_best_effort(paths: &Paths, providers: &[String]) -> Vec<String> {
    let mut removed = Vec::new();
    let mut names = providers.to_vec();
    names.sort();
    names.dedup();
    for provider in names {
        let Ok(path) = provider_state::model_cache_path(paths, &provider) else {
            continue;
        };
        let existed = path.exists();
        if state::remove_file_durable(&path).is_ok() && existed {
            removed.push(provider);
        }
    }
    removed
}

fn remove_model_caches_transactional<F>(
    paths: &Paths,
    providers: &[String],
    remove_cache: &mut F,
) -> Result<Vec<String>, NczError>
where
    F: FnMut(&Path) -> Result<(), NczError>,
{
    let mut removed = Vec::new();
    let mut names = providers.to_vec();
    names.sort();
    names.dedup();
    for provider in names {
        if provider_state::validate_name(&provider).is_err() {
            continue;
        }
        let path = provider_state::model_cache_path(paths, &provider)?;
        if !is_regular_file(&path)? {
            continue;
        }
        remove_cache(&path)?;
        removed.push(provider);
    }
    Ok(removed)
}

fn model_cache_paths_for_removal(
    paths: &Paths,
    providers: &[String],
) -> Result<Vec<PathBuf>, NczError> {
    let mut cache_paths = Vec::new();
    let mut names = providers.to_vec();
    names.sort();
    names.dedup();
    for provider in names {
        if provider_state::validate_name(&provider).is_err() {
            continue;
        }
        let path = provider_state::model_cache_path(paths, &provider)?;
        if is_regular_file(&path)? {
            cache_paths.push(path);
        }
    }
    Ok(cache_paths)
}

fn is_regular_file(path: &Path) -> Result<bool, NczError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(NczError::Io(e)),
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
    let mode = if body.is_some() {
        use std::os::unix::fs::PermissionsExt;
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
    use std::os::unix::fs::PermissionsExt;

    use crate::cli::{ApiAction, McpAction};
    use crate::cmd::common::test_paths;
    use crate::cmd::mcp as mcp_cmd;
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

    fn expect_mutation(report: ApiReport) -> ApiMutationReport {
        match report {
            ApiReport::Add(report) | ApiReport::Set(report) | ApiReport::Remove(report) => report,
            ApiReport::List(_) => panic!("expected mutation report"),
        }
    }

    fn mutation_report(action: &str) -> ApiMutationReport {
        ApiMutationReport {
            schema_version: common::SCHEMA_VERSION,
            action: action.to_string(),
            key: "TOGETHER_API_KEY".to_string(),
            value: None,
            changed: false,
            shared_file: "/etc/nclawzero/agent-env".to_string(),
            agent_override_files: Vec::new(),
            provider_bindings: Vec::new(),
            mcp_bindings: Vec::new(),
            restart_required: false,
            restart_agents: Vec::new(),
        }
    }

    #[test]
    fn api_json_reports_include_action_discriminators() {
        let list = ApiReport::List(ApiListReport {
            schema_version: common::SCHEMA_VERSION,
            entries: Vec::new(),
        });
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["action"].as_str(), Some("list"));
        assert_eq!(
            json["schema_version"].as_u64(),
            Some(common::SCHEMA_VERSION as u64)
        );

        for (report, action) in [
            (ApiReport::Add(mutation_report("add")), "add"),
            (ApiReport::Set(mutation_report("set")), "set"),
            (ApiReport::Remove(mutation_report("remove")), "remove"),
        ] {
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(json["action"].as_str(), Some(action));
            assert_eq!(
                json["schema_version"].as_u64(),
                Some(common::SCHEMA_VERSION as u64)
            );
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
    fn api_list_reports_empty_values_as_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=\n").unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths).unwrap();

        assert_eq!(report.entries[0].key, "TOGETHER_API_KEY");
        assert!(!report.entries[0].set);
        assert_eq!(report.entries[0].value, "");
        let mut rendered = Vec::new();
        report.render_text(&mut rendered).unwrap();
        assert_eq!(String::from_utf8(rendered).unwrap(), "TOGETHER_API_KEY=\n");
        let json = serde_json::to_value(ApiReport::List(report)).unwrap();
        assert_eq!(json["entries"][0]["set"].as_bool(), Some(false));
        assert_eq!(json["entries"][0]["value"].as_str(), Some(""));
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

        let report = expect_mutation(report);
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.value.as_deref(), Some("***"));
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
        assert!(report.agent_override_files.is_empty());
    }

    #[test]
    fn api_add_ignores_invalid_active_agent_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "bad-agent\n").unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "TOGETHER_API_KEY".to_string(),
                value: Some("secret".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
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

        let report = expect_mutation(report);
        assert_eq!(report.provider_bindings, vec!["together"]);
        let entries = agent_env::read(&paths).unwrap();
        assert!(agent_env::provider_binding_matches(
            &entries,
            "together",
            "TOGETHER_API_KEY",
            "https://api.example.test"
        )
        .unwrap());
    }

    #[test]
    fn api_set_invalidates_bound_provider_model_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("together.json"),
            r#"{"schema_version":1,"name":"together","url":"https://api.example.test","model":"m","key_env":"TOGETHER_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=old\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "together",
            "TOGETHER_API_KEY",
            "https://api.example.test",
        )
        .unwrap();
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "together".to_string(),
                provider_fingerprint: None,
                credential_fingerprint: Some("old".to_string()),
                fetched_at: "1".to_string(),
                models: Vec::new(),
            },
        )
        .unwrap();
        env::set_var("NCZ_TEST_ROTATED_CACHE_SECRET", "new");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_ROTATED_CACHE_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert_eq!(report.provider_bindings, vec!["together"]);
        let entries = agent_env::read(&paths).unwrap();
        assert!(!agent_env::provider_binding_matches(
            &entries,
            "together",
            "TOGETHER_API_KEY",
            "https://api.example.test"
        )
        .unwrap());
        assert!(!paths.providers_dir().join("together.models.json").exists());
    }

    #[test]
    fn api_set_rotation_revokes_mcp_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "MCP_TOKEN=old\n").unwrap();
        agent_env::set_mcp_binding(&paths, "search", "MCP_TOKEN", "https://mcp.example.test")
            .unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "MCP_TOKEN".to_string(),
                value: Some("new".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert_eq!(report.mcp_bindings, vec!["search"]);
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "MCP_TOKEN=new\n");
    }

    #[test]
    fn api_set_same_value_is_noop_without_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\n").unwrap();
        fs::set_permissions(paths.agent_env(), fs::Permissions::from_mode(0o600)).unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: Some("secret".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert!(!report.changed);
        assert!(!report.restart_required);
        assert!(report.restart_agents.is_empty());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
    }

    #[test]
    fn api_set_same_value_repairs_agent_env_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\n").unwrap();
        fs::set_permissions(paths.agent_env(), fs::Permissions::from_mode(0o644)).unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: Some("secret".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
        assert_eq!(
            fs::metadata(paths.agent_env())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn api_set_ignores_invalid_active_agent_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "bad-agent\n").unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=old\n").unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: Some("new".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=new\n"
        );
    }

    #[test]
    fn api_set_same_value_with_existing_provider_binding_preserves_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("together.json"),
            r#"{"schema_version":1,"name":"together","url":"https://api.example.test","model":"m","key_env":"TOGETHER_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "together",
            "TOGETHER_API_KEY",
            "https://api.example.test",
        )
        .unwrap();
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "together".to_string(),
                provider_fingerprint: None,
                credential_fingerprint: Some("secret".to_string()),
                fetched_at: "1".to_string(),
                models: Vec::new(),
            },
        )
        .unwrap();
        let cache_path = paths.providers_dir().join("together.models.json");
        assert!(cache_path.exists());
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: Some("secret".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: vec!["together".to_string()],
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert_eq!(report.provider_bindings, vec!["together"]);
        assert!(!report.changed);
        assert!(!report.restart_required);
        assert!(report.restart_agents.is_empty());
        assert!(cache_path.exists());
    }

    #[test]
    fn api_remove_invalidates_bound_provider_model_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("together.json"),
            r#"{"schema_version":1,"name":"together","url":"https://api.example.test","model":"m","key_env":"TOGETHER_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=old\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "together",
            "TOGETHER_API_KEY",
            "https://api.example.test",
        )
        .unwrap();
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "together".to_string(),
                provider_fingerprint: None,
                credential_fingerprint: Some("old".to_string()),
                fetched_at: "1".to_string(),
                models: Vec::new(),
            },
        )
        .unwrap();
        let runner = FakeRunner::new();

        run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Remove {
                key: "TOGETHER_API_KEY".to_string(),
                force: true,
            },
        )
        .unwrap();

        assert!(!paths.providers_dir().join("together.models.json").exists());
    }

    #[test]
    fn api_remove_rolls_back_when_bound_model_cache_delete_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("together.json"),
            r#"{"schema_version":1,"name":"together","url":"https://api.example.test","model":"m","key_env":"TOGETHER_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let declaration = provider_state::read(&paths, "together")
            .unwrap()
            .unwrap()
            .declaration;
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=old\nOTHER=1\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "together",
            "TOGETHER_API_KEY",
            "https://api.example.test",
        )
        .unwrap();
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "together".to_string(),
                provider_fingerprint: Some(
                    provider_state::provider_cache_fingerprint(&declaration).unwrap(),
                ),
                credential_fingerprint: Some("credential-a".to_string()),
                fetched_at: "1".to_string(),
                models: Vec::new(),
            },
        )
        .unwrap();
        let cache_path = provider_state::model_cache_path(&paths, "together").unwrap();

        let err = remove_with_cache_remover(
            &paths,
            "TOGETHER_API_KEY",
            true,
            |path| {
                if path == cache_path.as_path() {
                    return Err(NczError::Precondition(
                        "simulated cache removal failure".to_string(),
                    ));
                }
                state::remove_file_durable(path)
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        let agent_env = fs::read_to_string(paths.agent_env()).unwrap();
        assert!(agent_env.contains("TOGETHER_API_KEY=old"));
        assert!(agent_env.contains("NCZ_PROVIDER_BINDING_746F676574686572="));
        assert!(cache_path.exists());
    }

    #[test]
    fn api_set_provider_migrates_legacy_before_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=old\n",
        )
        .unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=old\n").unwrap();
        env::set_var("NCZ_TEST_ROTATED_API_SECRET", "new");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "LOCAL_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_ROTATED_API_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: vec!["local".to_string()],
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert!(report.changed);
        assert_eq!(report.provider_bindings, vec!["local"]);
        assert!(!paths.providers_dir().join("local.env").exists());
        assert!(paths.providers_dir().join("local.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=new\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn api_set_provider_rejects_mismatched_legacy_inline_secret_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy =
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=old\n";
        fs::write(&legacy_file, legacy).unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=other\n").unwrap();
        env::set_var("NCZ_TEST_REJECTED_ROTATION_SECRET", "new");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "LOCAL_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_REJECTED_ROTATION_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: vec!["local".to_string()],
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=other\n"
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
    }

    #[test]
    fn api_set_provider_rejects_unpreserved_legacy_inline_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy =
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=old\n";
        fs::write(&legacy_file, legacy).unwrap();
        env::set_var("NCZ_TEST_UNPRESERVED_ROTATION_SECRET", "new");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "LOCAL_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_UNPRESERVED_ROTATION_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: vec!["local".to_string()],
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("inline credential"))
        );
        assert!(!paths.agent_env().exists());
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
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

        let report = expect_mutation(report);
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\nOTHER=1\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("hermes")).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
        assert_eq!(report.restart_agents, all_agents());
    }

    #[test]
    fn api_scoped_set_reports_non_active_override_restart_when_shared_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.agent_env_override("hermes").parent().unwrap()).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\n").unwrap();
        fs::set_permissions(paths.agent_env(), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(paths.agent_env_override("hermes"), "TOGETHER_API_KEY=stale\n").unwrap();
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: Some("secret".to_string()),
                value_env: None,
                value_stdin: false,
                agents: vec!["hermes".to_string()],
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, vec!["hermes".to_string()]);
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("hermes")).unwrap(),
            "TOGETHER_API_KEY=secret\n"
        );
    }

    #[test]
    fn api_unscoped_set_updates_existing_agent_override_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.agent_env_override("hermes").parent().unwrap()).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=old\n").unwrap();
        fs::write(
            paths.agent_env_override("hermes"),
            "TOGETHER_API_KEY=stale\nOTHER=1\n",
        )
        .unwrap();
        env::set_var("NCZ_TEST_UNSCOPED_REPLACE_API_SECRET", "rotated");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_UNSCOPED_REPLACE_API_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert!(report.changed);
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=rotated\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_env_override("hermes")).unwrap(),
            "TOGETHER_API_KEY=rotated\nOTHER=1\n"
        );
        assert_eq!(
            report.agent_override_files,
            vec![paths.agent_env_override("hermes").display().to_string()]
        );
    }

    #[test]
    fn api_unscoped_set_ignores_missing_optional_override_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.etc_dir.join("zeroclaw"), "not a directory").unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=old\n").unwrap();
        env::set_var("NCZ_TEST_OPTIONAL_OVERRIDE_SECRET", "rotated");
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_OPTIONAL_OVERRIDE_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.agent_override_files.is_empty());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=rotated\n"
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
    fn api_stdin_value_reader_rejects_oversized_value() {
        let input = vec![b'a'; MAX_CREDENTIAL_VALUE_BYTES + 1];

        let err = read_value_from_reader(input.as_slice()).unwrap_err();

        assert!(matches!(err, NczError::Usage(message) if message.contains("exceeds")));
    }

    #[test]
    fn api_add_rejects_oversized_value_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let runner = FakeRunner::new();
        let huge = "a".repeat(MAX_CREDENTIAL_VALUE_BYTES + 1);

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "TOGETHER_API_KEY".to_string(),
                value: Some(huge),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(message) if message.contains("exceeds")));
        assert!(!paths.agent_env().exists());
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

        let report = expect_mutation(report);
        assert!(!report.changed);
        assert!(!report.restart_required);
    }

    #[test]
    fn api_remove_ignores_invalid_active_agent_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "bad-agent\n").unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\n").unwrap();
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "");
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, all_agents());
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.restart_required);
        assert_eq!(report.restart_agents, vec!["hermes".to_string()]);
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
    fn api_remove_ignores_missing_optional_override_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.etc_dir.join("zeroclaw"), "not a directory").unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\nOTHER=1\n").unwrap();
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert!(report.agent_override_files.is_empty());
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "OTHER=1\n");
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
    fn api_remove_force_reports_mcp_binding_and_mcp_add_rebinds_existing_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "MCP_TOKEN=old\n").unwrap();
        agent_env::set_mcp_stdio_binding(&paths, "search", "MCP_TOKEN", "search-mcp").unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"stdio","command":"search-mcp","url":null,"auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let removed = expect_mutation(
            run_with_paths(
                &ctx(&runner),
                &paths,
                ApiAction::Remove {
                    key: "MCP_TOKEN".to_string(),
                    force: true,
                },
            )
            .unwrap(),
        );

        assert_eq!(removed.mcp_bindings, vec!["search"]);
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "");
        env::set_var("NCZ_TEST_REBOUND_MCP_TOKEN", "new");
        run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "MCP_TOKEN".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_REBOUND_MCP_TOKEN".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();
        assert!(!agent_env::mcp_stdio_binding_matches(
            &agent_env::read(&paths).unwrap(),
            "search",
            "MCP_TOKEN",
            "search-mcp"
        )
        .unwrap());

        mcp_cmd::run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "search".to_string(),
                transport: "stdio".to_string(),
                command: Some("search-mcp".to_string()),
                url: None,
                auth_env: Some("MCP_TOKEN".to_string()),
                auth_value_env: Some("NCZ_TEST_REBOUND_MCP_TOKEN".to_string()),
            },
        )
        .unwrap();

        assert!(paths.mcp_dir().join("search.json").exists());
        assert!(agent_env::mcp_stdio_binding_matches(
            &agent_env::read(&paths).unwrap(),
            "search",
            "MCP_TOKEN",
            "search-mcp"
        )
        .unwrap());
    }

    #[test]
    fn api_set_rejects_empty_value_for_provider_reference() {
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
        env::set_var("NCZ_TEST_EMPTY_PROVIDER_SECRET", "");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Set {
                key: "TOGETHER_API_KEY".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_EMPTY_PROVIDER_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
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
    fn api_add_rejects_empty_value_for_mcp_reference() {
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
        env::set_var("NCZ_TEST_EMPTY_MCP_SECRET", "");
        let runner = FakeRunner::new();

        let err = run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "MCP_TOKEN".to_string(),
                value: None,
                value_env: Some("NCZ_TEST_EMPTY_MCP_SECRET".to_string()),
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
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

        let report = expect_mutation(report);
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "OTHER=1\n");
    }

    #[test]
    fn api_remove_force_tolerates_unrelated_malformed_agent_env_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.agent_env(),
            "TOGETHER_API_KEY=secret\nBROKEN=\"unterminated\nOTHER=1\n",
        )
        .unwrap();
        agent_env::set_provider_binding(
            &paths,
            "together",
            "TOGETHER_API_KEY",
            "https://api.example.test",
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert_eq!(report.provider_bindings, vec!["together"]);
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "BROKEN=\"unterminated\nOTHER=1\n"
        );
    }

    #[test]
    fn api_remove_ignores_broken_model_cache_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\nOTHER=1\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "together",
            "TOGETHER_API_KEY",
            "https://api.example.test",
        )
        .unwrap();
        fs::create_dir_all(
            provider_state::model_cache_path(&paths, "together").unwrap(),
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "OTHER=1\n");
    }

    #[test]
    fn api_remove_ignores_invalid_provider_binding_cache_names() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.agent_env(),
            "TOGETHER_API_KEY=secret\nOTHER=1\nNCZ_PROVIDER_BINDING_2E2E2F626164=\"TOGETHER_API_KEY https://api.example.test\"\n",
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

        let report = expect_mutation(report);
        assert!(report.changed);
        assert_eq!(fs::read_to_string(paths.agent_env()).unwrap(), "OTHER=1\n");
    }

    #[test]
    fn api_remove_fails_closed_on_broken_provider_reference_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "TOGETHER_API_KEY=secret\nOTHER=1\n").unwrap();
        fs::write(
            paths.providers_dir().join("future.json"),
            r#"{"schema_version":2,"name":"future","url":"https://api.example.test","model":"m","key_env":"TOGETHER_API_KEY","type":"openai-compat","health_path":"/health"}"#,
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

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "TOGETHER_API_KEY=secret\nOTHER=1\n"
        );
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
