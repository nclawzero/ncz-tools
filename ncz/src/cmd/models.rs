//! models — discover and report model catalogs across configured providers.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cli::{Context, ModelsAction};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent_env, providers as provider_state, url as url_state, Paths};

const MODEL_CATALOG_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ModelsReport {
    List(ModelsListReport),
    Status(ModelsStatusReport),
    Discover(ModelsDiscoverReport),
}

impl Render for ModelsReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            ModelsReport::List(report) => report.render_text(w),
            ModelsReport::Status(report) => report.render_text(w),
            ModelsReport::Discover(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ModelsListReport {
    pub schema_version: u32,
    pub models: Vec<ModelReport>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ModelReport {
    pub provider: String,
    pub id: String,
    pub configured: bool,
    pub healthy: bool,
    pub context_length: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ModelsStatusReport {
    pub schema_version: u32,
    pub models: Vec<ModelStatusReport>,
}

#[derive(Debug, Serialize)]
pub struct ModelStatusReport {
    pub provider: String,
    pub id: String,
    pub configured: bool,
    pub status: String,
    pub context_length: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ModelsDiscoverReport {
    pub schema_version: u32,
    pub provider: String,
    pub fetched_at: String,
    pub cache_file: String,
    pub models: Vec<provider_state::ModelDeclaration>,
}

#[derive(Debug, Clone)]
struct ModelEntry {
    report: ModelReport,
    degraded: bool,
}

#[derive(Debug)]
struct Catalog {
    models: Vec<provider_state::ModelDeclaration>,
    healthy: bool,
    degraded: bool,
    include_unhealthy_by_default: bool,
    configured_missing: bool,
}

struct DiscoverPreparation {
    record: provider_state::ProviderRecord,
    credential: DiscoverCredential,
}

struct DiscoverCredential {
    value: String,
    fingerprint: ProviderCredentialFingerprint,
}

#[derive(Debug)]
enum ModelQueryError {
    Config(String),
    Credential(String),
    Runtime(String),
}

impl std::fmt::Display for ModelQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelQueryError::Config(msg)
            | ModelQueryError::Credential(msg)
            | ModelQueryError::Runtime(msg) => f.write_str(msg),
        }
    }
}

impl Render for ModelsListReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for model in &self.models {
            writeln!(
                w,
                "{:<18} {:<40} configured={} healthy={} context_length={}",
                model.provider,
                model.id,
                model.configured,
                model.healthy,
                model
                    .context_length
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )?;
        }
        Ok(())
    }
}

impl Render for ModelsStatusReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for model in &self.models {
            writeln!(
                w,
                "{:<18} {:<40} {}",
                model.provider, model.id, model.status
            )?;
        }
        Ok(())
    }
}

impl Render for ModelsDiscoverReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "{}: {} models cached at {}",
            self.provider,
            self.models.len(),
            self.cache_file
        )
    }
}

pub fn run(ctx: &Context, action: ModelsAction) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: ModelsAction,
) -> Result<ModelsReport, NczError> {
    match action {
        ModelsAction::List {
            provider,
            show_unhealthy,
        } => Ok(ModelsReport::List(list(
            ctx,
            paths,
            provider.as_deref(),
            show_unhealthy,
        )?)),
        ModelsAction::Status { provider } => Ok(ModelsReport::Status(status(
            ctx,
            paths,
            provider.as_deref(),
        )?)),
        ModelsAction::Discover { provider } => {
            Ok(ModelsReport::Discover(discover(ctx, paths, &provider)?))
        }
    }
}

pub fn list(
    ctx: &Context,
    paths: &Paths,
    provider: Option<&str>,
    show_unhealthy: bool,
) -> Result<ModelsListReport, NczError> {
    Ok(ModelsListReport {
        schema_version: common::SCHEMA_VERSION,
        models: collect_entries(ctx, paths, provider, show_unhealthy)?
            .into_iter()
            .map(|entry| entry.report)
            .collect(),
    })
}

pub fn status(
    ctx: &Context,
    paths: &Paths,
    provider: Option<&str>,
) -> Result<ModelsStatusReport, NczError> {
    let models = collect_entries(ctx, paths, provider, true)?
        .into_iter()
        .map(|entry| ModelStatusReport {
            provider: entry.report.provider,
            id: entry.report.id,
            configured: entry.report.configured,
            status: if entry.report.healthy {
                "ok".to_string()
            } else if entry.degraded {
                "degraded".to_string()
            } else {
                "down".to_string()
            },
            context_length: entry.report.context_length,
        })
        .collect();

    Ok(ModelsStatusReport {
        schema_version: common::SCHEMA_VERSION,
        models,
    })
}

pub fn discover(
    ctx: &Context,
    paths: &Paths,
    provider: &str,
) -> Result<ModelsDiscoverReport, NczError> {
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let preparation = prepare_discover_locked(paths, provider)?;
    let discovered_provider = preparation.record.declaration.clone();
    let models = match query_models_with_secret(
        ctx,
        &discovered_provider,
        Some(&preparation.credential.value),
    ) {
        Ok(models) => models,
        Err(msg) => {
            let err = NczError::Precondition(format!(
                "could not discover models for provider {provider}: {msg}"
            ));
            return Err(err);
        }
    };
    if !configured_model_present(&models, &discovered_provider) {
        return Err(NczError::Precondition(format!(
            "provider {provider} configured model {} was not advertised by /v1/models",
            discovered_provider.model
        )));
    }
    let fetched_at = unix_timestamp();
    let cache_file = match finalize_discover_with_lock_held(
        paths,
        &preparation,
        &discovered_provider,
        &models,
        &fetched_at,
    ) {
        Ok(path) => path,
        Err(err) => return Err(err),
    };

    Ok(ModelsDiscoverReport {
        schema_version: common::SCHEMA_VERSION,
        provider: discovered_provider.name,
        fetched_at,
        cache_file: cache_file.display().to_string(),
        models,
    })
}

fn prepare_discover_locked(paths: &Paths, provider: &str) -> Result<DiscoverPreparation, NczError> {
    let record = provider_state::read(paths, provider)?
        .ok_or_else(|| NczError::Usage(format!("unknown provider: {provider}")))?;
    if let Some(field) = &record.unmigratable_secret_field {
        return Err(NczError::Precondition(format!(
            "could not discover models for provider {provider}: legacy provider {} contains inline credential field {field} that cannot be used safely; move it to agent-env or remove it before discovery",
            record.declaration.name
        )));
    }

    let migration_paths = provider_state::legacy_migration_snapshot_paths_for_provider(
        paths,
        &record.declaration.name,
    )?;
    if !migration_paths.is_empty() {
        provider_state::validate_legacy_migration_for_provider(paths, &record.declaration.name)?;
        reject_conflicting_legacy_binding(paths, &record)?;
    }

    let credential = match discover_credential(paths, &record) {
        Ok(credential) => credential,
        Err(msg) => {
            return Err(NczError::Precondition(format!(
                "could not discover models for provider {provider}: {msg}"
            )));
        }
    };
    Ok(DiscoverPreparation {
        record,
        credential,
    })
}

fn finalize_discover_with_lock_held(
    paths: &Paths,
    preparation: &DiscoverPreparation,
    discovered_provider: &provider_state::ProviderDeclaration,
    models: &[provider_state::ModelDeclaration],
    fetched_at: &str,
) -> Result<PathBuf, NczError> {
    let snapshot_paths = vec![provider_state::model_cache_path(
        paths,
        &discovered_provider.name,
    )?];
    let snapshots = snapshot_paths_for_restore(&snapshot_paths)?;

    let result = (|| -> Result<PathBuf, NczError> {
        let current = provider_state::read(paths, &discovered_provider.name)?.ok_or_else(|| {
            NczError::Precondition(format!(
                "provider {} was removed during discovery",
                discovered_provider.name
            ))
        })?;
        if provider_record_changed(&current, &preparation.record) {
            return Err(NczError::Precondition(format!(
                "provider {} changed during discovery; retry discover",
                discovered_provider.name
            )));
        }
        let current_credential = provider_credential_fingerprint(paths, &current).map_err(|msg| {
            NczError::Precondition(format!(
                "provider {} credential changed during discovery; retry discover: {msg}",
                discovered_provider.name
            ))
        })?;
        if current_credential != preparation.credential.fingerprint {
            return Err(NczError::Precondition(format!(
                "provider {} credential changed during discovery; retry discover",
                discovered_provider.name
            )));
        }
        let cache = provider_state::ProviderModelCache {
            schema_version: common::SCHEMA_VERSION,
            provider: discovered_provider.name.clone(),
            provider_fingerprint: Some(provider_state::provider_cache_fingerprint(
                discovered_provider,
            )?),
            credential_fingerprint: Some(current_credential.cache_fingerprint()),
            fetched_at: fetched_at.to_string(),
            models: models.to_vec(),
        };
        provider_state::write_model_cache(paths, &cache)
    })();

    match result {
        Ok(path) => Ok(path),
        Err(err) => {
            restore_snapshots(&snapshots)?;
            Err(err)
        }
    }
}

fn provider_record_changed(
    current: &provider_state::ProviderRecord,
    prepared: &provider_state::ProviderRecord,
) -> bool {
    current.declaration != prepared.declaration
        || current.inline_secret != prepared.inline_secret
        || current.unmigratable_secret_field != prepared.unmigratable_secret_field
}

fn reject_conflicting_legacy_binding(
    paths: &Paths,
    record: &provider_state::ProviderRecord,
) -> Result<(), NczError> {
    if record.inline_secret.is_none() {
        return Ok(());
    }
    let provider = &record.declaration;
    let entries = agent_env::read(paths)?;
    if !agent_env::provider_binding_exists(&entries, &provider.name)? {
        return Ok(());
    }
    if agent_env::provider_binding_matches(&entries, &provider.name, &provider.key_env, &provider.url)? {
        return Ok(());
    }
    Err(NczError::Precondition(format!(
        "legacy provider {} has an inline credential, but provider {} already has a binding for a different key or URL; run `ncz api set {} --providers={}` to approve {}",
        provider.name, provider.name, provider.key_env, provider.name, provider.url
    )))
}

fn collect_entries(
    ctx: &Context,
    paths: &Paths,
    provider: Option<&str>,
    show_unhealthy: bool,
) -> Result<Vec<ModelEntry>, NczError> {
    let records = provider_records(paths, provider)?;
    let mut entries = Vec::new();
    for record in records {
        let catalog = load_catalog(ctx, paths, &record);
        let mut seen = BTreeSet::new();
        for model in catalog.models {
            if !seen.insert(model.id.clone()) {
                continue;
            }
            let configured = model.id == record.declaration.model;
            let configured_missing = configured && catalog.configured_missing;
            if catalog.healthy
                || show_unhealthy
                || configured
                || catalog.include_unhealthy_by_default
            {
                entries.push(ModelEntry {
                    report: ModelReport {
                        provider: record.declaration.name.clone(),
                        id: model.id,
                        configured,
                        healthy: catalog.healthy && !configured_missing,
                        context_length: model.context_length,
                    },
                    degraded: catalog.degraded,
                });
            }
        }
    }
    entries.sort_by(|a, b| {
        a.report
            .provider
            .cmp(&b.report.provider)
            .then(a.report.id.cmp(&b.report.id))
    });
    Ok(entries)
}

fn provider_records(
    paths: &Paths,
    provider: Option<&str>,
) -> Result<Vec<provider_state::ProviderRecord>, NczError> {
    if let Some(provider) = provider {
        let record = provider_state::read(paths, provider)?
            .ok_or_else(|| NczError::Usage(format!("unknown provider: {provider}")))?;
        Ok(vec![record])
    } else {
        provider_state::read_all(paths)
    }
}

fn load_catalog(ctx: &Context, paths: &Paths, record: &provider_state::ProviderRecord) -> Catalog {
    let provider = &record.declaration;
    let query_result = query_models(ctx, paths, record);
    match query_result {
        Ok(models) => {
            let configured_missing = !configured_model_present(&models, provider);
            return Catalog {
                models: ensure_configured_model(models, provider),
                healthy: true,
                degraded: false,
                include_unhealthy_by_default: false,
                configured_missing,
            };
        }
        Err(ModelQueryError::Config(_))
        | Err(ModelQueryError::Credential(_))
        | Err(ModelQueryError::Runtime(_)) => {}
    }

    let cache = match provider_credential_fingerprint(paths, record) {
        Ok(credential) => {
            let fingerprint = credential.cache_fingerprint();
            provider_state::read_model_cache_for_provider(paths, provider, Some(&fingerprint))
        }
        Err(err) if credential_unavailable_allows_cache_fallback(&err) => {
            provider_state::read_model_cache_for_provider_with_unavailable_credential(
                paths, provider,
            )
        }
        Err(_) => Ok(None),
    };
    if let Ok(Some(cache)) = cache {
        if !cache.models.is_empty() {
            return Catalog {
                models: ensure_configured_model(cache.models, provider),
                healthy: false,
                degraded: true,
                include_unhealthy_by_default: true,
                configured_missing: false,
            };
        }
    }

    if !provider.models.is_empty() {
        return Catalog {
            models: ensure_configured_model(provider.models.clone(), provider),
            healthy: false,
            degraded: true,
            include_unhealthy_by_default: true,
            configured_missing: false,
        };
    }

    Catalog {
        models: ensure_configured_model(Vec::new(), provider),
        healthy: false,
        degraded: false,
        include_unhealthy_by_default: false,
        configured_missing: false,
    }
}

fn credential_unavailable_allows_cache_fallback(err: &ModelQueryError) -> bool {
    match err {
        ModelQueryError::Config(_) => true,
        ModelQueryError::Credential(message) => {
            message.starts_with("missing provider credential ")
        }
        ModelQueryError::Runtime(_) => false,
    }
}

fn configured_model_present(
    models: &[provider_state::ModelDeclaration],
    provider: &provider_state::ProviderDeclaration,
) -> bool {
    models.iter().any(|model| model.id == provider.model)
}

fn ensure_configured_model(
    mut models: Vec<provider_state::ModelDeclaration>,
    provider: &provider_state::ProviderDeclaration,
) -> Vec<provider_state::ModelDeclaration> {
    if !provider.model.is_empty() && !models.iter().any(|model| model.id == provider.model) {
        models.push(provider_state::ModelDeclaration {
            id: provider.model.clone(),
            context_length: None,
        });
    }
    models
}

fn query_models(
    ctx: &Context,
    paths: &Paths,
    record: &provider_state::ProviderRecord,
) -> Result<Vec<provider_state::ModelDeclaration>, ModelQueryError> {
    let _lock = state::acquire_lock(&paths.lock_path)
        .map_err(|err| ModelQueryError::Config(err.to_string()))?;
    let current = provider_state::read(paths, &record.declaration.name)
        .map_err(|err| ModelQueryError::Config(err.to_string()))?
        .ok_or_else(|| {
            ModelQueryError::Config(format!(
                "provider {} was removed during model query",
                record.declaration.name
            ))
        })?;
    if current.declaration != record.declaration {
        return Err(ModelQueryError::Config(format!(
            "provider {} changed during model query; retry",
            record.declaration.name
        )));
    }
    if let Some(field) = &current.unmigratable_secret_field {
        return Err(ModelQueryError::Credential(format!(
            "legacy provider {} contains inline credential field {field} that is not approved for read-only model queries; move it to agent-env before live status checks",
            current.declaration.name
        )));
    }
    if current.inline_secret.is_some() {
        return Err(ModelQueryError::Credential(format!(
            "legacy inline credential for provider {} is not approved for read-only model queries; run `ncz models discover {}` to refresh the cache explicitly",
            current.declaration.name, current.declaration.name
        )));
    }
    let secret = provider_secret(paths, &current)?;
    query_models_with_secret(ctx, &current.declaration, Some(&secret))
}

fn query_models_with_secret(
    ctx: &Context,
    provider: &provider_state::ProviderDeclaration,
    secret: Option<&str>,
) -> Result<Vec<provider_state::ModelDeclaration>, ModelQueryError> {
    if provider.url.trim().is_empty() {
        return Err(ModelQueryError::Config("provider URL is empty".to_string()));
    }
    if provider.provider_type != provider_state::OPENAI_COMPAT_PROVIDER_TYPE {
        return Err(ModelQueryError::Config(format!(
            "unsupported provider type {}; v0.2 supports only {}",
            provider.provider_type,
            provider_state::OPENAI_COMPAT_PROVIDER_TYPE
        )));
    }
    provider_state::validate_provider_url(&provider.url)
        .map_err(|err| ModelQueryError::Config(err.to_string()))?;
    let url = models_url(&provider.url);
    let mut args = vec![
        "-q".to_string(),
        "-fsS".to_string(),
        "--noproxy".to_string(),
        "*".to_string(),
        "--proxy".to_string(),
        String::new(),
        "--max-time".to_string(),
        "5".to_string(),
        "--max-filesize".to_string(),
        MODEL_CATALOG_MAX_BYTES.to_string(),
    ];
    let mut curl_config = None;
    if let Some(secret) = secret {
        reject_insecure_credential_url(&provider.url).map_err(ModelQueryError::Config)?;
        let config =
            curl_header_config(secret).map_err(|err| ModelQueryError::Config(err.to_string()))?;
        args.push("-K".to_string());
        args.push("-".to_string());
        curl_config = Some(config);
    }
    args.push("--".to_string());
    args.push(url);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = if let Some(config) = curl_config {
        ctx.runner
            .run_stdout_limited_with_stdin(
                "curl",
                &arg_refs,
                config.as_bytes(),
                MODEL_CATALOG_MAX_BYTES,
            )
            .map_err(|err| ModelQueryError::Runtime(err.to_string()))?
    } else {
        ctx.runner
            .run_stdout_limited("curl", &arg_refs, MODEL_CATALOG_MAX_BYTES)
            .map_err(|err| ModelQueryError::Runtime(err.to_string()))?
    };
    if !output.ok() {
        return Err(ModelQueryError::Runtime(output.stderr.trim().to_string()));
    }
    parse_models_response(&output.stdout).map_err(|err| ModelQueryError::Runtime(err.to_string()))
}

struct FileSnapshot {
    path: PathBuf,
    body: Option<Vec<u8>>,
    mode: u32,
}

fn snapshot_paths_for_restore(paths: &[PathBuf]) -> Result<Vec<FileSnapshot>, NczError> {
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

fn provider_secret(
    paths: &Paths,
    record: &provider_state::ProviderRecord,
) -> Result<String, ModelQueryError> {
    let provider = &record.declaration;
    let entries = agent_env::read(paths).map_err(|err| ModelQueryError::Config(err.to_string()))?;
    if let Some(value) = find_secret(&entries, &provider.key_env) {
        require_provider_binding(&entries, provider)?;
        return Ok(value);
    }
    if record.inline_secret.is_some() {
        return Err(ModelQueryError::Credential(format!(
            "legacy inline credential for provider {} is not approved for read-only model queries; run a mutating provider or api command to migrate and bind {}",
            provider.name, provider.key_env
        )));
    }
    Err(ModelQueryError::Credential(format!(
        "missing provider credential {} in agent-env",
        provider.key_env
    )))
}

fn discover_credential(
    paths: &Paths,
    record: &provider_state::ProviderRecord,
) -> Result<DiscoverCredential, ModelQueryError> {
    let credential = provider_credential_fingerprint(paths, record)?;
    Ok(DiscoverCredential {
        value: credential.value.clone(),
        fingerprint: credential,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderCredentialFingerprint {
    value: String,
    source: CredentialSourceFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CredentialSourceFingerprint {
    AgentEnv(ProviderCredentialMarker),
    LegacyInline(ProviderCredentialMarker),
}

impl ProviderCredentialFingerprint {
    fn cache_fingerprint(&self) -> String {
        format!("ncz-v3:{}:{}", self.source.label(), self.source.cache_marker())
    }
}

impl CredentialSourceFingerprint {
    fn label(&self) -> &'static str {
        match self {
            CredentialSourceFingerprint::AgentEnv(_) => "agent-env",
            CredentialSourceFingerprint::LegacyInline(_) => "legacy-inline",
        }
    }

    fn cache_marker(&self) -> String {
        match self {
            CredentialSourceFingerprint::AgentEnv(fingerprint)
            | CredentialSourceFingerprint::LegacyInline(fingerprint) => fingerprint.cache_marker(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderCredentialMarker {
    tuple_hmac: String,
}

impl ProviderCredentialMarker {
    fn cache_marker(&self) -> String {
        format!("hmac-sha256={}", self.tuple_hmac)
    }
}

fn provider_credential_fingerprint(
    paths: &Paths,
    record: &provider_state::ProviderRecord,
) -> Result<ProviderCredentialFingerprint, ModelQueryError> {
    let provider = &record.declaration;
    let entries = agent_env::read(paths).map_err(|err| ModelQueryError::Config(err.to_string()))?;
    if let Some(value) = find_secret(&entries, &provider.key_env) {
        require_provider_binding(&entries, provider)?;
        return Ok(ProviderCredentialFingerprint {
            source: CredentialSourceFingerprint::AgentEnv(provider_credential_marker(
                provider, &value,
            )),
            value,
        });
    }
    if let Some(value) = &record.inline_secret {
        return Ok(ProviderCredentialFingerprint {
            source: CredentialSourceFingerprint::LegacyInline(provider_credential_marker(
                provider, value,
            )),
            value: value.clone(),
        });
    }
    Err(ModelQueryError::Credential(format!(
        "missing provider credential {} in agent-env",
        provider.key_env
    )))
}

fn require_provider_binding(
    entries: &[agent_env::AgentEnvEntry],
    provider: &provider_state::ProviderDeclaration,
) -> Result<(), ModelQueryError> {
    if agent_env::provider_binding_matches(
        entries,
        &provider.name,
        &provider.key_env,
        &provider.url,
    )
    .map_err(|err| ModelQueryError::Config(err.to_string()))?
    {
        return Ok(());
    }
    Err(ModelQueryError::Credential(format!(
        "provider credential {} is not bound to provider {}; run `ncz api set {} --providers={}` to approve live discovery",
        provider.key_env, provider.name, provider.key_env, provider.name
    )))
}

fn provider_credential_marker(
    provider: &provider_state::ProviderDeclaration,
    value: &str,
) -> ProviderCredentialMarker {
    let key = credential_fingerprint_key(provider);
    let tuple = format!(
        "provider\0{}\0key_env\0{}\0url\0{}\0value\0{}",
        provider.name, provider.key_env, provider.url, value
    );
    ProviderCredentialMarker {
        tuple_hmac: hmac_sha256_hex(key.as_bytes(), tuple.as_bytes()),
    }
}

fn credential_fingerprint_key(provider: &provider_state::ProviderDeclaration) -> String {
    format!(
        "ncz-model-cache-v3\0{}\0{}\0{}",
        provider.name, provider.key_env, provider.url
    )
}

fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    const BLOCK_BYTES: usize = 64;
    let mut key_block = [0u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_BYTES];
    let mut outer_pad = [0x5cu8; BLOCK_BYTES];
    for (idx, key_byte) in key_block.iter().enumerate() {
        inner_pad[idx] ^= key_byte;
        outer_pad[idx] ^= key_byte;
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    hex_encode(&outer.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn find_secret(entries: &[agent_env::AgentEnvEntry], key: &str) -> Option<String> {
    entries
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.value.clone())
        .filter(|value| !value.is_empty())
}

fn reject_insecure_credential_url(url: &str) -> Result<(), String> {
    if !url.to_ascii_lowercase().starts_with("http://") {
        return Ok(());
    }
    let Some(host) = url_state::host(url) else {
        return Err("invalid provider URL".to_string());
    };
    if url_state::is_loopback_host(host) {
        return Ok(());
    }
    Err(
        "refusing to send provider credential over plaintext HTTP; use https or a loopback provider URL"
            .to_string(),
    )
}

fn curl_header_config(secret: &str) -> io::Result<String> {
    if secret.contains('\n') || secret.contains('\r') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "provider secret contains a newline",
        ));
    }
    Ok(format!(
        "header = \"Authorization: Bearer {}\"\n",
        curl_config_escape(secret)
    ))
}

fn curl_config_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn models_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

fn parse_models_response(body: &str) -> Result<Vec<provider_state::ModelDeclaration>, NczError> {
    let value: Value = serde_json::from_str(body)?;
    let source = (if value.is_array() {
        Some(&value)
    } else if let Some(data) = value.get("data") {
        Some(data)
    } else {
        value.get("models")
    })
    .ok_or_else(|| {
        NczError::Precondition(
            "model catalog response did not include a data or models array".to_string(),
        )
    })?;

    let models = provider_state::models_from_value(Some(source));
    if models.is_empty() {
        return Err(NczError::Precondition(
            "model catalog response did not include any model ids".to_string(),
        ));
    }
    Ok(models)
}

fn unix_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    use crate::cli::{ApiAction, Context, ModelsAction};
    use crate::cmd::api;
    use crate::cmd::common::{out, test_paths};
    use crate::error::NczError;
    use crate::sys::fake::FakeRunner;
    use crate::sys::{CommandRunner, ProcessOutput};

    use super::*;

    fn ctx<'a>(runner: &'a dyn CommandRunner) -> Context<'a> {
        Context {
            json: false,
            show_secrets: false,
            runner,
        }
    }

    fn write_provider(paths: &Paths, body: &str) {
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(paths.providers_dir().join("example.json"), body).unwrap();
    }

    fn write_secret(paths: &Paths) {
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "EXAMPLE_API_KEY=secret\n").unwrap();
        if let Some(url) = provider_url_for_test(paths) {
            agent_env::set_provider_binding(paths, "example", "EXAMPLE_API_KEY", &url).unwrap();
        }
    }

    fn provider_url_for_test(paths: &Paths) -> Option<String> {
        if let Ok(Some(record)) = provider_state::read(paths, "example") {
            return Some(record.declaration.url);
        }
        let body = fs::read_to_string(paths.providers_dir().join("example.json")).ok()?;
        let value: serde_json::Value = serde_json::from_str(&body).ok()?;
        value
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    fn expect_model_catalog_down(runner: &FakeRunner) {
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(7, "", "down\n"),
        );
    }

    struct RotatingCredentialRunner {
        paths: Paths,
    }

    impl CommandRunner for RotatingCredentialRunner {
        fn run(&self, cmd: &str, _args: &[&str]) -> Result<ProcessOutput, NczError> {
            assert_eq!(cmd, "curl");
            fs::write(self.paths.agent_env(), "EXAMPLE_API_KEY=rotated\n").unwrap();
            Ok(out(0, r#"{"data":[{"id":"small"}]}"#, ""))
        }
    }

    struct LockProbeRunner {
        paths: Paths,
        observed_blocked: Arc<AtomicBool>,
    }

    impl CommandRunner for LockProbeRunner {
        fn run(&self, cmd: &str, _args: &[&str]) -> Result<ProcessOutput, NczError> {
            assert_eq!(cmd, "curl");
            let (tx, rx) = mpsc::channel();
            let lock_path = self.paths.lock_path.clone();
            thread::spawn(move || {
                let _guard = state::acquire_lock(&lock_path).unwrap();
                let _ = tx.send(());
            });
            if rx.recv_timeout(Duration::from_millis(50)).is_err() {
                self.observed_blocked.store(true, Ordering::SeqCst);
            }
            Ok(out(0, r#"{"data":[{"id":"small"}]}"#, ""))
        }
    }

    struct LegacyVisibilityProbeRunner {
        paths: Paths,
        observed_unmigrated: Arc<AtomicBool>,
    }

    impl CommandRunner for LegacyVisibilityProbeRunner {
        fn run(&self, cmd: &str, _args: &[&str]) -> Result<ProcessOutput, NczError> {
            assert_eq!(cmd, "curl");
            let legacy_path = self.paths.providers_dir().join("example.env");
            let canonical_path = self.paths.providers_dir().join("example.json");
            if legacy_path.exists() && !canonical_path.exists() && !self.paths.agent_env().exists()
            {
                self.observed_unmigrated.store(true, Ordering::SeqCst);
            }
            Ok(out(0, r#"{"data":[{"id":"small"}]}"#, ""))
        }
    }

    struct SecretProbeRunner {
        expected_secret: &'static str,
        rejected_secret: &'static str,
    }

    impl CommandRunner for SecretProbeRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> Result<ProcessOutput, NczError> {
            assert_eq!(cmd, "curl");
            panic!("secret-bearing curl calls must use stdin, got args: {args:?}");
        }

        fn run_stdout_limited_with_stdin(
            &self,
            cmd: &str,
            args: &[&str],
            stdin: &[u8],
            _stdout_limit_bytes: usize,
        ) -> Result<ProcessOutput, NczError> {
            assert_eq!(cmd, "curl");
            let config_arg = args
                .windows(2)
                .find_map(|window| (window[0] == "-K").then_some(window[1]))
                .expect("curl config arg");
            assert_eq!(config_arg, "-");
            let argv = args.join(" ");
            assert!(!argv.contains(self.expected_secret));
            assert!(!argv.contains(self.rejected_secret));
            let config = String::from_utf8(stdin.to_vec()).unwrap();
            assert!(config.contains(self.expected_secret));
            assert!(!config.contains(self.rejected_secret));
            Ok(out(0, r#"{"data":[{"id":"small"}]}"#, ""))
        }
    }

    #[test]
    fn models_list_falls_back_to_current_discover_cache_when_live_query_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let record = provider_state::read(&paths, "example").unwrap().unwrap();
        let credential_fingerprint = provider_credential_fingerprint(&paths, &record)
            .unwrap()
            .cache_fingerprint();
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: Some(
                    provider_state::provider_cache_fingerprint(&record.declaration).unwrap(),
                ),
                credential_fingerprint: Some(credential_fingerprint),
                fetched_at: "1".to_string(),
                models: vec![
                    provider_state::ModelDeclaration {
                        id: "small".to_string(),
                        context_length: Some(8192),
                    },
                    provider_state::ModelDeclaration {
                        id: "large".to_string(),
                        context_length: Some(200000),
                    },
                ],
            },
        )
        .unwrap();
        let runner = FakeRunner::new();
        expect_model_catalog_down(&runner);

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.models.len(), 2);
        assert!(report.models.iter().all(|model| !model.healthy));
        assert!(report.models.iter().any(|model| model.configured));
        runner.assert_done();
    }

    #[test]
    fn models_list_does_not_accept_api_forged_provider_binding_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "EXAMPLE_API_KEY=secret\n").unwrap();
        let runner = FakeRunner::new();

        let err = api::run_with_paths(
            &ctx(&runner),
            &paths,
            ApiAction::Add {
                key: "NCZ_PROVIDER_BINDING_6578616D706C65".to_string(),
                value: Some("EXAMPLE_API_KEY https://api.example.test".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "EXAMPLE_API_KEY=secret\n"
        );

        let report = list(&ctx(&runner), &paths, Some("example"), true).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].provider, "example");
        assert_eq!(report.models[0].id, "small");
        assert!(!report.models[0].healthy);
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn models_list_does_not_query_live_legacy_inline_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("example.env"),
            "PROVIDER_NAME=example\nPROVIDER_URL=https://api.example.test\nMODEL=small\nEXAMPLE_API_KEY=inline-secret\n",
        )
        .unwrap();
        fs::write(paths.agent_env(), "EXAMPLE_API_KEY=shared-secret\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "example",
            "EXAMPLE_API_KEY",
            "https://api.example.test",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths, Some("example"), false).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
        assert!(runner.calls.lock().unwrap().is_empty());
        assert!(paths.providers_dir().join("example.env").exists());
    }

    #[test]
    fn models_list_uses_static_models_for_credentialed_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health","models":["small",{"id":"large","context_length":200000}]}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        expect_model_catalog_down(&runner);

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 2);
        assert!(!report.models[0].healthy);
        assert_eq!(
            report
                .models
                .iter()
                .find(|model| model.id == "large")
                .and_then(|model| model.context_length),
            Some(200000)
        );
        runner.assert_done();
    }

    #[test]
    fn models_list_and_status_prefer_discover_cache_over_static_models() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health","models":["small","stale-static"]}"#,
        );
        write_secret(&paths);
        let record = provider_state::read(&paths, "example").unwrap().unwrap();
        let credential_fingerprint = provider_credential_fingerprint(&paths, &record)
            .unwrap()
            .cache_fingerprint();
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: Some(
                    provider_state::provider_cache_fingerprint(&record.declaration).unwrap(),
                ),
                credential_fingerprint: Some(credential_fingerprint),
                fetched_at: "1".to_string(),
                models: vec![
                    provider_state::ModelDeclaration {
                        id: "small".to_string(),
                        context_length: Some(8192),
                    },
                    provider_state::ModelDeclaration {
                        id: "cached-large".to_string(),
                        context_length: Some(200000),
                    },
                ],
            },
        )
        .unwrap();
        let runner = FakeRunner::new();
        expect_model_catalog_down(&runner);
        expect_model_catalog_down(&runner);

        let list_report = list(&ctx(&runner), &paths, None, false).unwrap();
        let status_report = status(&ctx(&runner), &paths, None).unwrap();

        assert!(list_report
            .models
            .iter()
            .any(|model| model.id == "cached-large" && model.context_length == Some(200000)));
        assert!(!list_report
            .models
            .iter()
            .any(|model| model.id == "stale-static"));
        assert!(status_report
            .models
            .iter()
            .any(|model| model.id == "cached-large" && model.status == "degraded"));
        assert!(!status_report
            .models
            .iter()
            .any(|model| model.id == "stale-static"));
        runner.assert_done();
    }

    #[test]
    fn models_status_marks_static_fallback_as_degraded() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health","models":["small"]}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        expect_model_catalog_down(&runner);

        let report = status(&ctx(&runner), &paths, None).unwrap();

        assert_eq!(report.models[0].status, "degraded");
        runner.assert_done();
    }

    #[test]
    fn models_status_uses_static_models_when_credential_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health","models":["small","large"]}"#,
        );
        let runner = FakeRunner::new();

        let report = status(&ctx(&runner), &paths, None).unwrap();

        assert_eq!(report.models.len(), 2);
        assert!(report
            .models
            .iter()
            .any(|model| model.id == "small" && model.status == "degraded"));
        assert!(report
            .models
            .iter()
            .any(|model| model.id == "large" && model.status == "degraded"));
    }

    #[test]
    fn models_status_reports_ok_for_live_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = SecretProbeRunner {
            expected_secret: "Authorization: Bearer secret",
            rejected_secret: "EXAMPLE_API_KEY=secret",
        };

        let report = status(&ctx(&runner), &paths, None).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
        assert_eq!(report.models[0].status, "ok");
    }

    #[test]
    fn models_status_reports_down_when_live_provider_query_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        expect_model_catalog_down(&runner);

        let report = status(&ctx(&runner), &paths, None).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
        assert_eq!(report.models[0].status, "down");
        runner.assert_done();
    }

    #[test]
    fn models_list_uses_cache_when_credential_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        let declaration = provider_state::read(&paths, "example")
            .unwrap()
            .unwrap()
            .declaration;
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: Some(
                    provider_state::provider_cache_fingerprint(&declaration).unwrap(),
                ),
                credential_fingerprint: None,
                fetched_at: "1".to_string(),
                models: vec![
                    provider_state::ModelDeclaration {
                        id: "small".to_string(),
                        context_length: Some(8192),
                    },
                    provider_state::ModelDeclaration {
                        id: "large".to_string(),
                        context_length: Some(200000),
                    },
                ],
            },
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 2);
        assert!(report
            .models
            .iter()
            .any(|model| model.id == "large" && model.context_length == Some(200000)));
        assert!(report.models.iter().all(|model| !model.healthy));
    }

    #[test]
    fn models_list_ignores_cache_for_wrong_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        fs::write(
            provider_state::model_cache_path(&paths, "example").unwrap(),
            r#"{"schema_version":1,"provider":"other","fetched_at":"1","models":[{"id":"large","context_length":200000}]}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
    }

    #[test]
    fn models_list_ignores_cache_for_changed_provider_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://old.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        let old_provider = provider_state::read(&paths, "example")
            .unwrap()
            .unwrap()
            .declaration;
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: Some(
                    provider_state::provider_cache_fingerprint(&old_provider).unwrap(),
                ),
                credential_fingerprint: None,
                fetched_at: "1".to_string(),
                models: vec![provider_state::ModelDeclaration {
                    id: "large".to_string(),
                    context_length: Some(200000),
                }],
            },
        )
        .unwrap();
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://new.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
    }

    #[test]
    fn models_list_uses_current_credential_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let record = provider_state::read(&paths, "example").unwrap().unwrap();
        let credential_fingerprint = provider_credential_fingerprint(&paths, &record)
            .unwrap()
            .cache_fingerprint();
        provider_state::write_model_cache(
            &paths,
            &provider_state::ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: Some(
                    provider_state::provider_cache_fingerprint(&record.declaration).unwrap(),
                ),
                credential_fingerprint: Some(credential_fingerprint),
                fetched_at: "1".to_string(),
                models: vec![
                    provider_state::ModelDeclaration {
                        id: "small".to_string(),
                        context_length: Some(8192),
                    },
                    provider_state::ModelDeclaration {
                        id: "large".to_string(),
                        context_length: Some(200000),
                    },
                ],
            },
        )
        .unwrap();
        let runner = FakeRunner::new();
        expect_model_catalog_down(&runner);

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 2);
        assert!(report
            .models
            .iter()
            .any(|model| model.id == "large" && model.context_length == Some(200000)));
        assert!(report.models.iter().all(|model| !model.healthy));
        runner.assert_done();
    }

    #[test]
    fn models_list_ignores_cache_after_credential_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let discover_runner = FakeRunner::new();
        discover_runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"},{"id":"large"}]}"#, ""),
        );
        discover(&ctx(&discover_runner), &paths, "example").unwrap();
        discover_runner.assert_done();
        agent_env::set(&paths, "EXAMPLE_API_KEY", "rotated").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "example",
            "EXAMPLE_API_KEY",
            "https://api.example.test",
        )
        .unwrap();
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(7, "", "down\n"),
        );

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
        assert!(!report.models[0].healthy);
    }

    #[test]
    fn models_list_and_status_ignore_discover_cache_when_credential_is_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let discover_runner = FakeRunner::new();
        discover_runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"},{"id":"large","context_length":200000}]}"#, ""),
        );
        discover(&ctx(&discover_runner), &paths, "example").unwrap();
        discover_runner.assert_done();
        fs::remove_file(paths.agent_env()).unwrap();
        let runner = FakeRunner::new();

        let list_report = list(&ctx(&runner), &paths, None, false).unwrap();
        let status_report = status(&ctx(&runner), &paths, None).unwrap();

        assert_eq!(list_report.models.len(), 1);
        assert_eq!(list_report.models[0].id, "small");
        assert!(list_report.models.iter().all(|model| !model.healthy));
        assert_eq!(status_report.models.len(), 1);
        assert!(status_report
            .models
            .iter()
            .any(|model| model.id == "small" && model.status == "down"));
        runner.assert_done();
    }

    #[test]
    fn models_list_ignores_discover_cache_when_provider_binding_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let discover_runner = FakeRunner::new();
        discover_runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"},{"id":"large","context_length":200000}]}"#, ""),
        );
        discover(&ctx(&discover_runner), &paths, "example").unwrap();
        discover_runner.assert_done();
        let mut providers = BTreeSet::new();
        providers.insert("example".to_string());
        agent_env::remove_provider_bindings_for_providers(&paths, &providers).unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
        assert!(!report.models[0].healthy);
        runner.assert_done();
    }

    #[test]
    fn models_list_ignores_discover_cache_when_credential_source_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let discover_runner = FakeRunner::new();
        discover_runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"},{"id":"large","context_length":200000}]}"#, ""),
        );
        discover(&ctx(&discover_runner), &paths, "example").unwrap();
        discover_runner.assert_done();
        fs::remove_file(paths.agent_env()).unwrap();
        fs::create_dir(paths.agent_env()).unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
        assert!(!report.models[0].healthy);
        runner.assert_done();
    }

    #[test]
    fn models_list_reads_legacy_provider_without_migrating() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("example.env"),
            "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nAPI_KEY=legacy\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
        assert!(!report.models[0].healthy);
        assert!(paths.providers_dir().join("example.env").exists());
        assert!(!paths.providers_dir().join("example.json").exists());
        assert!(!paths.agent_env().exists());
        runner.assert_done();
    }

    #[test]
    fn models_status_marks_missing_configured_model_as_down() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"missing","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"}]}"#, ""),
        );

        let report = status(&ctx(&runner), &paths, None).unwrap();

        let configured = report
            .models
            .iter()
            .find(|model| model.id == "missing")
            .unwrap();
        assert!(configured.configured);
        assert_eq!(configured.status, "down");
    }

    #[test]
    fn models_discover_writes_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"}]}"#, ""),
        );

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            ModelsAction::Discover {
                provider: "example".to_string(),
            },
        )
        .unwrap();

        let ModelsReport::Discover(report) = report else {
            panic!("expected discover report");
        };
        assert_eq!(report.models.len(), 1);
        let cache_body =
            fs::read_to_string(paths.providers_dir().join("example.models.json")).unwrap();
        assert!(cache_body.contains("ncz-v3:agent-env:hmac-sha256="));
        assert!(!cache_body.contains("secret"));
        assert!(!cache_body.contains("ncz-v1:"));
        assert!(!cache_body.contains("ncz-v2:"));
    }

    #[test]
    fn models_discover_refuses_rotated_key_until_reapproved() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let api_runner = FakeRunner::new();
        api::run_with_paths(
            &ctx(&api_runner),
            &paths,
            ApiAction::Set {
                key: "EXAMPLE_API_KEY".to_string(),
                value: Some("rotated".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: Vec::new(),
            },
        )
        .unwrap();
        let discover_runner = FakeRunner::new();

        let err = discover(&ctx(&discover_runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("not bound")));
        assert!(discover_runner.calls.lock().unwrap().is_empty());

        api::run_with_paths(
            &ctx(&api_runner),
            &paths,
            ApiAction::Set {
                key: "EXAMPLE_API_KEY".to_string(),
                value: Some("rotated".to_string()),
                value_env: None,
                value_stdin: false,
                agents: Vec::new(),
                providers: vec!["example".to_string()],
            },
        )
        .unwrap();
        discover_runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"}]}"#, ""),
        );

        discover(&ctx(&discover_runner), &paths, "example").unwrap();
        discover_runner.assert_done();
    }

    #[test]
    fn provider_credential_fingerprint_changes_for_same_length_secret_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let record = provider_state::read(&paths, "example").unwrap().unwrap();
        let original = provider_credential_fingerprint(&paths, &record)
            .unwrap()
            .cache_fingerprint();
        let body = fs::read_to_string(paths.agent_env()).unwrap();
        fs::write(paths.agent_env(), body.replace("secret", "rotato")).unwrap();

        let rotated = provider_credential_fingerprint(&paths, &record)
            .unwrap()
            .cache_fingerprint();

        assert_ne!(original, rotated);
        assert!(rotated.contains("hmac-sha256="));
        assert!(!rotated.contains("rotato"));
    }

    #[test]
    fn provider_credential_fingerprint_ignores_unrelated_agent_env_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let record = provider_state::read(&paths, "example").unwrap().unwrap();
        let original = provider_credential_fingerprint(&paths, &record)
            .unwrap()
            .cache_fingerprint();
        agent_env::set(&paths, "UNRELATED_API_KEY", "rotated").unwrap();

        let after_unrelated_edit = provider_credential_fingerprint(&paths, &record)
            .unwrap()
            .cache_fingerprint();

        assert_eq!(original, after_unrelated_edit);
    }

    #[test]
    fn models_discover_ignores_unrelated_malformed_provider_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        fs::write(paths.providers_dir().join("broken.json"), "{not json").unwrap();
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"}]}"#, ""),
        );

        let report = discover(&ctx(&runner), &paths, "example").unwrap();

        assert_eq!(report.models.len(), 1);
        assert!(paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_writes_cache_for_legacy_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("example.env"),
            "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nAPI_KEY=legacy\n",
        )
        .unwrap();
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"}]}"#, ""),
        );

        let report = discover(&ctx(&runner), &paths, "example").unwrap();

        assert_eq!(report.models.len(), 1);
        assert!(paths.providers_dir().join("example.models.json").exists());
        assert!(paths.providers_dir().join("example.env").exists());
        assert!(!paths.providers_dir().join("example.json").exists());
        assert!(!paths.agent_env().exists());
        let cache = provider_state::read_model_cache(&paths, "example")
            .unwrap()
            .unwrap();
        assert!(cache
            .credential_fingerprint
            .as_deref()
            .is_some_and(|fingerprint| fingerprint.starts_with("ncz-v3:legacy-inline:")));
    }

    #[test]
    fn models_discover_rejects_legacy_inline_secret_with_changed_binding_before_query() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        let legacy_file = paths.providers_dir().join("example.env");
        let legacy = "PROVIDER_NAME=example\nPROVIDER_URL=https://api.attacker.test/v1\nMODEL=small\nAPI_KEY=legacy\n";
        fs::write(&legacy_file, legacy).unwrap();
        agent_env::set(&paths, "EXAMPLE_API_KEY", "legacy").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "example",
            "EXAMPLE_API_KEY",
            "https://api.example.test/v1",
        )
        .unwrap();
        let original_agent_env = fs::read_to_string(paths.agent_env()).unwrap();
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("different key or URL"))
        );
        assert!(runner.calls.lock().unwrap().is_empty());
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("example.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            original_agent_env
        );
    }

    #[test]
    fn models_discover_rejects_unmigratable_legacy_env_secret_before_query() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("example.env");
        let legacy = "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nKEY_ENV=EXAMPLE_API_KEY\nEXAMPLE_API_KEY=legacy\nPROXY_TOKEN=proxy-secret\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("PROXY_TOKEN")));
        assert!(runner.calls.lock().unwrap().is_empty());
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("example.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn models_discover_rejects_unrelated_legacy_env_secret_before_query() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("example.env");
        let legacy =
            "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nPROXY_TOKEN=proxy-secret\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("PROXY_TOKEN")));
        assert!(runner.calls.lock().unwrap().is_empty());
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("example.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn models_discover_rejects_unmigratable_legacy_json_secret_before_query() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("example.json");
        let legacy = r#"{"provider":"example","base_url":"http://127.0.0.1:8080/v1","default_model":"small","api_key_env":"EXAMPLE_API_KEY","api_key":"legacy","headers":[{"proxy_token":"proxy-secret"}]}"#;
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("proxy_token")));
        assert!(runner.calls.lock().unwrap().is_empty());
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn models_discover_normalizes_legacy_authorization_header_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("example.json"),
            r#"{"provider":"example","base_url":"http://127.0.0.1:8080/v1","default_model":"small","api_key_env":"EXAMPLE_API_KEY","headers":{"Authorization":"Bearer legacy-token"}}"#,
        )
        .unwrap();
        let runner = SecretProbeRunner {
            expected_secret: "Authorization: Bearer legacy-token",
            rejected_secret: "Authorization: Bearer Bearer legacy-token",
        };

        let report = discover(&ctx(&runner), &paths, "example").unwrap();

        assert_eq!(report.models.len(), 1);
        assert!(!paths.agent_env().exists());
        assert!(paths.providers_dir().join("example.json").exists());
    }

    #[test]
    fn models_discover_rejects_non_bearer_legacy_authorization_before_query() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("example.json");
        let legacy = r#"{"provider":"example","base_url":"http://127.0.0.1:8080/v1","default_model":"small","api_key_env":"EXAMPLE_API_KEY","headers":{"Authorization":"Basic bG9jYWw6c2VjcmV0"}}"#;
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("Authorization")));
        assert!(runner.calls.lock().unwrap().is_empty());
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn models_discover_leaves_legacy_provider_when_catalog_query_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("example.env");
        let legacy =
            "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nAPI_KEY=legacy\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(7, "", "down\n"),
        );

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("example.json").exists());
        assert!(!paths.providers_dir().join("example.models.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn models_discover_leaves_legacy_provider_when_configured_model_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("example.env");
        let legacy =
            "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nAPI_KEY=legacy\n";
        fs::write(&legacy_file, legacy).unwrap();
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"other"}]}"#, ""),
        );

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("configured model"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("example.json").exists());
        assert!(!paths.providers_dir().join("example.models.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn models_discover_fails_when_configured_model_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"missing","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"}]}"#, ""),
        );

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_aborts_when_credential_changes_before_cache_write() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = RotatingCredentialRunner {
            paths: paths.clone(),
        };

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("credential changed"))
        );
        assert!(!paths.providers_dir().join("example.models.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "EXAMPLE_API_KEY=rotated\n"
        );
    }

    #[test]
    fn models_discover_holds_lock_while_querying_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let observed_blocked = Arc::new(AtomicBool::new(false));
        let runner = LockProbeRunner {
            paths: paths.clone(),
            observed_blocked: observed_blocked.clone(),
        };

        discover(&ctx(&runner), &paths, "example").unwrap();

        assert!(observed_blocked.load(Ordering::SeqCst));
    }

    #[test]
    fn models_discover_does_not_migrate_legacy_provider_during_query() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("example.env"),
            "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nAPI_KEY=legacy\n",
        )
        .unwrap();
        let observed_unmigrated = Arc::new(AtomicBool::new(false));
        let runner = LegacyVisibilityProbeRunner {
            paths: paths.clone(),
            observed_unmigrated: observed_unmigrated.clone(),
        };

        discover(&ctx(&runner), &paths, "example").unwrap();

        assert!(observed_unmigrated.load(Ordering::SeqCst));
        assert!(paths.providers_dir().join("example.env").exists());
        assert!(!paths.providers_dir().join("example.json").exists());
        assert!(!paths.agent_env().exists());
        assert!(paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_rejects_conflicting_legacy_inline_secrets_before_query() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("example.env"),
            "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nAPI_KEY=stale\n",
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("example-alias.env"),
            "PROVIDER_NAME=example\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=small\nAPI_KEY=fresh\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("inline credential conflicts"))
        );
        assert!(!paths.providers_dir().join("example.models.json").exists());
        assert!(paths.providers_dir().join("example.env").exists());
        assert!(paths.providers_dir().join("example-alias.env").exists());
        runner.assert_done();
    }

    #[test]
    fn models_list_holds_lock_while_querying_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let observed_blocked = Arc::new(AtomicBool::new(false));
        let runner = LockProbeRunner {
            paths: paths.clone(),
            observed_blocked: observed_blocked.clone(),
        };

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert!(report.models.iter().any(|model| model.healthy));
        assert!(observed_blocked.load(Ordering::SeqCst));
    }

    #[test]
    fn models_status_holds_lock_while_querying_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let observed_blocked = Arc::new(AtomicBool::new(false));
        let runner = LockProbeRunner {
            paths: paths.clone(),
            observed_blocked: observed_blocked.clone(),
        };

        let report = status(&ctx(&runner), &paths, None).unwrap();

        assert!(report.models.iter().any(|model| model.status == "ok"));
        assert!(observed_blocked.load(Ordering::SeqCst));
    }

    #[test]
    fn models_discover_disables_curl_ambient_config_and_proxy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"http://127.0.0.1:8080/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let previous_http_proxy = env::var_os("http_proxy");
        let previous_all_proxy = env::var_os("ALL_PROXY");
        env::set_var("http_proxy", "http://192.0.2.1:9");
        env::set_var("ALL_PROXY", "http://192.0.2.1:9");
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"}]}"#, ""),
        );

        discover(&ctx(&runner), &paths, "example").unwrap();

        let calls = runner.calls.lock().unwrap();
        assert!(calls[0].starts_with(
            "curl -q -fsS --noproxy * --proxy  --max-time 5 --max-filesize 1048576 -K - -- "
        ));
        assert!(!calls[0].contains("secret"));
        match previous_http_proxy {
            Some(value) => env::set_var("http_proxy", value),
            None => env::remove_var("http_proxy"),
        }
        match previous_all_proxy {
            Some(value) => env::set_var("ALL_PROXY", value),
            None => env::remove_var("ALL_PROXY"),
        }
    }

    #[test]
    fn models_discover_fails_when_declared_credential_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_uses_effective_duplicate_credential_value() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.agent_env(),
            "EXAMPLE_API_KEY=secret\nEXAMPLE_API_KEY=\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
        runner.assert_done();
    }

    #[test]
    fn models_discover_rejects_malformed_catalog_response() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"error":"denied"}"#, ""),
        );

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_rejects_oversized_catalog_response() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, &"x".repeat(MODEL_CATALOG_MAX_BYTES + 1), ""),
        );

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_rejects_empty_catalog_response() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[]}"#, ""),
        );

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_ignores_active_agent_credential_override_in_v02() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        fs::create_dir_all(paths.agent_env_override("hermes").parent().unwrap()).unwrap();
        fs::write(paths.agent_state(), "hermes\n").unwrap();
        fs::write(
            paths.agent_env_override("hermes"),
            "EXAMPLE_API_KEY=scoped\n",
        )
        .unwrap();
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_rejects_plaintext_remote_when_credentialed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"http://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_rejects_plaintext_127_prefixed_hostname() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"http://127.attacker.example","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();

        let err = discover(&ctx(&runner), &paths, "example").unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_discover_allows_plaintext_loopback_when_credentialed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"http://127.0.0.1:8080/v1","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(0, r#"{"data":[{"id":"small"}]}"#, ""),
        );

        let report = discover(&ctx(&runner), &paths, "example").unwrap();

        assert_eq!(report.models.len(), 1);
        assert!(paths.providers_dir().join("example.models.json").exists());
    }

    #[test]
    fn models_list_json_identifies_action_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        let report = ModelsReport::List(list(&ctx(&runner), &paths, None, false).unwrap());

        let value = serde_json::to_value(report).unwrap();

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["action"], "list");
        assert_eq!(value["models"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn models_status_json_identifies_action_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        let report = ModelsReport::Status(status(&ctx(&runner), &paths, None).unwrap());

        let value = serde_json::to_value(report).unwrap();

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["action"], "status");
        assert_eq!(value["models"].as_array().unwrap().len(), 0);
    }
}
