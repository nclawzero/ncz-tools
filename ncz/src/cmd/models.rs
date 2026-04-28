//! models — discover and report model catalogs across configured providers.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use tempfile::NamedTempFile;

use crate::cli::{Context, ModelsAction};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent_env, providers as provider_state, url as url_state, Paths};

const MODEL_CATALOG_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(untagged)]
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

#[derive(Debug)]
enum ModelQueryError {
    Config(String),
    Runtime(String),
}

impl std::fmt::Display for ModelQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelQueryError::Config(msg) | ModelQueryError::Runtime(msg) => f.write_str(msg),
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
    provider_state::migrate_legacy(paths)?;
    let record = provider_state::read(paths, provider)?
        .ok_or_else(|| NczError::Usage(format!("unknown provider: {provider}")))?;
    let credential = provider_credential_fingerprint(paths, &record).map_err(|msg| {
        NczError::Precondition(format!(
            "could not discover models for provider {provider}: {msg}"
        ))
    })?;
    let models = query_models_with_secret(ctx, &record.declaration, Some(&credential.value))
        .map_err(|msg| {
            NczError::Precondition(format!(
                "could not discover models for provider {provider}: {msg}"
            ))
        })?;
    if !configured_model_present(&models, &record.declaration) {
        return Err(NczError::Precondition(format!(
            "provider {provider} configured model {} was not advertised by /v1/models",
            record.declaration.model
        )));
    }
    let fetched_at = unix_timestamp();
    let discovered_provider = record.declaration.clone();
    let cache = provider_state::ProviderModelCache {
        schema_version: common::SCHEMA_VERSION,
        provider: discovered_provider.name.clone(),
        fetched_at: fetched_at.clone(),
        models,
    };
    let current = provider_state::read_record_path(paths, &record.path)?.ok_or_else(|| {
        NczError::Precondition(format!(
            "provider {} was removed during discovery",
            discovered_provider.name
        ))
    })?;
    if current.declaration != discovered_provider {
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
    if current_credential != credential {
        return Err(NczError::Precondition(format!(
            "provider {} credential changed during discovery; retry discover",
            discovered_provider.name
        )));
    }
    let cache_file = provider_state::write_model_cache(paths, &cache)?;

    Ok(ModelsDiscoverReport {
        schema_version: common::SCHEMA_VERSION,
        provider: cache.provider,
        fetched_at,
        cache_file: cache_file.display().to_string(),
        models: cache.models,
    })
}

fn collect_entries(
    ctx: &Context,
    paths: &Paths,
    provider: Option<&str>,
    show_unhealthy: bool,
) -> Result<Vec<ModelEntry>, NczError> {
    {
        let _lock = state::acquire_lock(&paths.lock_path)?;
        provider_state::migrate_legacy(paths)?;
    }
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
        Err(ModelQueryError::Config(_)) => {
            return Catalog {
                models: ensure_configured_model(Vec::new(), provider),
                healthy: false,
                degraded: false,
                include_unhealthy_by_default: false,
                configured_missing: false,
            };
        }
        Err(ModelQueryError::Runtime(_)) => {}
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

    if let Ok(Some(cache)) = provider_state::read_model_cache(paths, &provider.name) {
        if !cache.models.is_empty() {
            return Catalog {
                models: ensure_configured_model(cache.models, provider),
                healthy: false,
                degraded: true,
                include_unhealthy_by_default: false,
                configured_missing: false,
            };
        }
    }

    Catalog {
        models: ensure_configured_model(Vec::new(), provider),
        healthy: false,
        degraded: false,
        include_unhealthy_by_default: false,
        configured_missing: false,
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
    let secret = provider_secret(paths, record)?;
    query_models_with_secret(ctx, &record.declaration, Some(&secret))
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
        args.push(config.path().display().to_string());
        curl_config = Some(config);
    }
    args.push("--".to_string());
    args.push(url);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = ctx
        .runner
        .run_stdout_limited("curl", &arg_refs, MODEL_CATALOG_MAX_BYTES)
        .map_err(|err| ModelQueryError::Runtime(err.to_string()))?;
    drop(curl_config);
    if !output.ok() {
        return Err(ModelQueryError::Runtime(output.stderr.trim().to_string()));
    }
    parse_models_response(&output.stdout).map_err(|err| ModelQueryError::Runtime(err.to_string()))
}

fn provider_secret(
    paths: &Paths,
    record: &provider_state::ProviderRecord,
) -> Result<String, ModelQueryError> {
    let provider = &record.declaration;
    let entries = agent_env::read(paths).map_err(|err| ModelQueryError::Config(err.to_string()))?;
    if let Some(value) = find_secret(&entries, &provider.key_env) {
        return Ok(value);
    }
    record.inline_secret.clone().ok_or_else(|| {
        ModelQueryError::Config(format!(
            "missing provider credential {} in agent-env or legacy provider file",
            provider.key_env
        ))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderCredentialFingerprint {
    value: String,
    source: CredentialSourceFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CredentialSourceFingerprint {
    AgentEnv(FileFingerprint),
    InlineProvider(FileFingerprint),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    body: Vec<u8>,
    dev: u64,
    ino: u64,
    len: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
}

fn provider_credential_fingerprint(
    paths: &Paths,
    record: &provider_state::ProviderRecord,
) -> Result<ProviderCredentialFingerprint, ModelQueryError> {
    let provider = &record.declaration;
    let entries = agent_env::read(paths).map_err(|err| ModelQueryError::Config(err.to_string()))?;
    if let Some(value) = find_secret(&entries, &provider.key_env) {
        return Ok(ProviderCredentialFingerprint {
            value,
            source: CredentialSourceFingerprint::AgentEnv(file_fingerprint(&paths.agent_env())?),
        });
    }
    if let Some(value) = &record.inline_secret {
        return Ok(ProviderCredentialFingerprint {
            value: value.clone(),
            source: CredentialSourceFingerprint::InlineProvider(file_fingerprint(&record.path)?),
        });
    }
    Err(ModelQueryError::Config(format!(
        "missing provider credential {} in agent-env or legacy provider file",
        provider.key_env
    )))
}

fn file_fingerprint(path: &std::path::Path) -> Result<FileFingerprint, ModelQueryError> {
    let body = fs::read(path).map_err(|err| {
        ModelQueryError::Config(format!(
            "could not read credential source {}: {err}",
            path.display()
        ))
    })?;
    let metadata = fs::metadata(path).map_err(|err| {
        ModelQueryError::Config(format!(
            "could not stat credential source {}: {err}",
            path.display()
        ))
    })?;
    Ok(FileFingerprint {
        body,
        dev: metadata.dev(),
        ino: metadata.ino(),
        len: metadata.len(),
        mtime_sec: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
    })
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
        return Err(format!("invalid provider URL: {url}"));
    };
    if url_state::is_loopback_host(host) {
        return Ok(());
    }
    Err(format!(
        "refusing to send provider credential over plaintext HTTP to {host}; use https or a loopback provider URL"
    ))
}

fn curl_header_config(secret: &str) -> io::Result<NamedTempFile> {
    if secret.contains('\n') || secret.contains('\r') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "provider secret contains a newline",
        ));
    }
    let mut file = NamedTempFile::new()?;
    fs::set_permissions(file.path(), fs::Permissions::from_mode(0o600))?;
    writeln!(
        file,
        "header = \"Authorization: Bearer {}\"",
        curl_config_escape(secret)
    )?;
    file.as_file().sync_all()?;
    Ok(file)
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
    } else if let Some(models) = value.get("models") {
        Some(models)
    } else {
        None
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

    use crate::cli::{Context, ModelsAction};
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

    #[test]
    fn models_list_queries_openai_compatible_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(
                0,
                r#"{"data":[{"id":"small","context_length":8192},{"id":"large","context_length":200000}]}"#,
                "",
            ),
        );

        let report = list(&ctx(&runner), &paths, None, false).unwrap();

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.models.len(), 2);
        assert!(report.models[0].healthy);
        assert!(report.models.iter().any(|model| model.configured));
    }

    #[test]
    fn models_list_uses_static_models_when_catalog_is_down() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health","models":["small",{"id":"large","context_length":200000}]}"#,
        );
        write_secret(&paths);
        let runner = FakeRunner::new();
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(7, "", "down\n"),
        );

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
        runner.expect_prefix(
            "curl",
            &["-q", "-fsS", "--noproxy", "*", "--proxy"],
            out(7, "", "down\n"),
        );

        let report = status(&ctx(&runner), &paths, None).unwrap();

        assert_eq!(report.models[0].status, "degraded");
    }

    #[test]
    fn models_status_treats_missing_credential_as_down_even_with_static_models() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_provider(
            &paths,
            r#"{"schema_version":1,"name":"example","url":"https://api.example.test","model":"small","key_env":"EXAMPLE_API_KEY","type":"openai-compat","health_path":"/health","models":["small","large"]}"#,
        );
        let runner = FakeRunner::new();

        let report = status(&ctx(&runner), &paths, None).unwrap();

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].id, "small");
        assert_eq!(report.models[0].status, "down");
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
        assert!(paths.providers_dir().join("example.json").exists());
        assert!(!paths.providers_dir().join("example.env").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "EXAMPLE_API_KEY=legacy\n"
        );
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
    fn models_discover_holds_lock_while_using_credential() {
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
            "curl -q -fsS --noproxy * --proxy  --max-time 5 --max-filesize 1048576 -K "
        ));
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
}
