//! providers — list, probe, and select configured LLM providers.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::cli::{Context, ProvidersAction};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent, providers as provider_state, Paths};

#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ProvidersReport {
    List(ProvidersListReport),
    Test(ProviderTestReport),
    SetPrimary(ProviderSetPrimaryReport),
}

impl Render for ProvidersReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            ProvidersReport::List(report) => report.render_text(w),
            ProvidersReport::Test(report) => report.render_text(w),
            ProvidersReport::SetPrimary(report) => report.render_text(w),
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
    pub key: String,
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
            writeln!(
                w,
                "{:<18} health={:<10} url={} model={} key={}",
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
                provider.key
            )?;
        }
        Ok(())
    }
}

impl Render for ProviderTestReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Provider {}: ok", self.name)
    }
}

impl Render for ProviderSetPrimaryReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Primary provider: {}", self.name)
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
    }
}

pub fn list(ctx: &Context, paths: &Paths) -> Result<ProvidersListReport, NczError> {
    let primary = provider_state::read_primary(paths)?.unwrap_or_default();
    let mut providers = Vec::new();
    for file in provider_files(paths)? {
        let provider = ProviderConfig::from_file(&file)?;
        let health = provider_health(ctx, &provider);
        providers.push(ProviderReport {
            name: provider.name,
            url: provider.url,
            model: provider.model,
            key: common::mask_secret_value(&provider.key, ctx.show_secrets),
            health,
            file: file.display().to_string(),
        });
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
    let file = find_provider_file(paths, name)?
        .ok_or_else(|| NczError::Usage(format!("unknown provider: {name}")))?;
    let provider = ProviderConfig::from_file(&file)?;
    let health = provider_health(ctx, &provider);
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
    find_provider_file(paths, name)?
        .ok_or_else(|| NczError::Usage(format!("unknown provider: {name}")))?;

    let _lock = state::acquire_lock(&paths.lock_path)?;
    provider_state::write_primary(paths, name)?;
    let active_agent = agent::read(paths)?;
    let agent_provider_file = paths.agent_primary_provider(&active_agent);
    state::atomic_write(&agent_provider_file, format!("{name}\n").as_bytes(), 0o644)?;

    Ok(ProviderSetPrimaryReport {
        schema_version: common::SCHEMA_VERSION,
        name: name.to_string(),
        active_agent,
        primary_provider_file: paths.primary_provider().display().to_string(),
        agent_provider_file: agent_provider_file.display().to_string(),
    })
}

fn provider_files(paths: &Paths) -> Result<Vec<PathBuf>, NczError> {
    let mut files: Vec<PathBuf> = common::sorted_files(&paths.providers_dir())?
        .into_iter()
        .filter(|path| {
            matches!(
                path.extension().and_then(OsStr::to_str),
                Some("env" | "conf" | "json")
            )
        })
        .collect();
    files.sort();
    Ok(files)
}

fn find_provider_file(paths: &Paths, wanted: &str) -> Result<Option<PathBuf>, NczError> {
    for file in provider_files(paths)? {
        let provider = ProviderConfig::from_file(&file)?;
        if provider.name == wanted || file.file_name().and_then(OsStr::to_str) == Some(wanted) {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

fn provider_health(ctx: &Context, provider: &ProviderConfig) -> String {
    let health_url = if !provider.health_url.is_empty() {
        provider.health_url.clone()
    } else if !provider.url.is_empty() {
        format!("{}/health", provider.url.trim_end_matches('/'))
    } else {
        "/health".to_string()
    };

    if provider.url.is_empty() && health_url == "/health" {
        return "unknown".to_string();
    }

    if ctx
        .runner
        .run("curl", &["-fsS", "--max-time", "3", &health_url])
        .map(|out| out.ok())
        .unwrap_or(false)
    {
        "ok".to_string()
    } else {
        "unhealthy".to_string()
    }
}

#[derive(Debug)]
struct ProviderConfig {
    name: String,
    url: String,
    health_url: String,
    model: String,
    key: String,
}

impl ProviderConfig {
    fn from_file(path: &Path) -> Result<Self, NczError> {
        let body = fs::read_to_string(path)?;
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        if ext == "json" {
            Ok(Self::from_json(&body, fallback_name))
        } else {
            Ok(Self::from_env(&body, fallback_name))
        }
    }

    fn from_json(body: &str, fallback_name: String) -> Self {
        let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
        let field = |names: &[&str]| -> String {
            names
                .iter()
                .find_map(|name| value.get(*name).and_then(Value::as_str))
                .unwrap_or("")
                .to_string()
        };
        let name = field(&["name", "provider"]);
        Self {
            name: if name.is_empty() { fallback_name } else { name },
            url: field(&["url", "base_url", "endpoint"]),
            health_url: field(&["health_url"]),
            model: field(&["model", "default_model"]),
            key: field(&["api_key", "token"]),
        }
    }

    fn from_env(body: &str, fallback_name: String) -> Self {
        let lookup = |keys: &[&str]| -> String {
            for line in body.lines() {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                let key = key.trim();
                if keys.iter().any(|wanted| key.eq_ignore_ascii_case(wanted)) {
                    return common::strip_wrapping_quotes(value);
                }
            }
            String::new()
        };
        let name = lookup(&["PROVIDER_NAME", "NAME"]);
        Self {
            name: if name.is_empty() { fallback_name } else { name },
            url: lookup(&["PROVIDER_URL", "BASE_URL", "ENDPOINT", "URL"]),
            health_url: lookup(&["PROVIDER_HEALTH_URL", "HEALTH_URL"]),
            model: lookup(&["MODEL", "DEFAULT_MODEL", "PROVIDER_MODEL"]),
            key: lookup(&["API_KEY", "TOKEN", "SECRET", "PASSWORD"]),
        }
    }
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

    #[test]
    fn providers_list_happy_path_reads_configs_and_redacts_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(paths.primary_provider(), "local\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=abc\n",
        )
        .unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            "curl",
            &["-fsS", "--max-time", "3", "http://127.0.0.1:8080/health"],
            out(0, "", ""),
        );

        let report = list(&ctx(&runner), &paths).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.primary, "local");
        assert_eq!(report.providers[0].key, "***");
        assert_eq!(report.providers[0].health, "ok");
    }

    #[test]
    fn providers_test_error_path_rejects_unhealthy_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("bad.env"),
            "PROVIDER_URL=http://bad.example\n",
        )
        .unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            "curl",
            &["-fsS", "--max-time", "3", "http://bad.example/health"],
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
        fs::write(paths.providers_dir().join("local.env"), "NAME=local\n").unwrap();
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
        assert_eq!(
            fs::read_to_string(paths.primary_provider()).unwrap(),
            "local\n"
        );
        assert_eq!(
            fs::read_to_string(paths.agent_primary_provider("hermes")).unwrap(),
            "local\n"
        );
    }
}
