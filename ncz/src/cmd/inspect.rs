//! inspect — collect a redacted diagnostic snapshot.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::cli::Context;
use crate::cmd::{common, sandbox, status, version};
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{agent, Paths};

#[derive(Debug, Serialize)]
pub struct InspectReport {
    pub schema_version: u32,
    pub generated_at: String,
    pub version: Option<version::VersionReport>,
    pub status: Option<status::StatusReport>,
    pub services: Vec<ServiceReport>,
    pub recent_logs: Vec<LogSection>,
    pub sandbox: Option<sandbox::SandboxShowReport>,
    pub etc_dir: String,
    pub etc_missing: bool,
    pub etc_files: Vec<EtcFileReport>,
}

#[derive(Debug, Serialize)]
pub struct ServiceReport {
    pub agent: String,
    pub service: String,
    pub ok: bool,
    pub properties: BTreeMap<String, String>,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct LogSection {
    pub agent: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EtcFileReport {
    pub path: String,
    pub redacted_path: bool,
    pub lines: Vec<String>,
}

impl Render for InspectReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "== ncz inspect ==")?;
        writeln!(w, "{}", self.generated_at)?;
        writeln!(w)?;
        writeln!(w, "== version ==")?;
        if let Some(version) = &self.version {
            version.render_text(w)?;
        }
        writeln!(w)?;
        writeln!(w, "== status ==")?;
        if let Some(status) = &self.status {
            status.render_text(w)?;
        }
        writeln!(w)?;
        writeln!(w, "== services ==")?;
        for service in &self.services {
            writeln!(w, "-- {} --", service.agent)?;
            if service.ok {
                for (key, value) in &service.properties {
                    writeln!(w, "{key}={value}")?;
                }
            } else {
                writeln!(w, "unavailable: {}", service.error)?;
            }
        }
        writeln!(w)?;
        writeln!(w, "== recent logs ==")?;
        for section in &self.recent_logs {
            writeln!(w)?;
            writeln!(w, "-- {} --", section.agent)?;
            for line in &section.lines {
                writeln!(w, "{line}")?;
            }
        }
        writeln!(w)?;
        writeln!(w, "== sandbox ==")?;
        if let Some(sandbox) = &self.sandbox {
            sandbox.render_text(w)?;
        }
        writeln!(w)?;
        writeln!(w, "== /etc/nclawzero ==")?;
        if self.etc_missing {
            writeln!(w, "missing {}", self.etc_dir)?;
        } else {
            for file in &self.etc_files {
                writeln!(w)?;
                writeln!(w, "-- {} --", file.path)?;
                for line in &file.lines {
                    writeln!(w, "{line}")?;
                }
            }
        }
        Ok(())
    }
}

pub fn run(ctx: &Context) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = collect(ctx, &paths)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn collect(ctx: &Context, paths: &Paths) -> Result<InspectReport, NczError> {
    let generated_at =
        common::command_stdout(ctx.runner, "date", &["-Is"]).unwrap_or_else(|| "unknown".into());
    Ok(InspectReport {
        schema_version: common::SCHEMA_VERSION,
        generated_at,
        version: version::collect(ctx, paths).ok(),
        status: status::collect(ctx, paths).ok(),
        services: collect_services(ctx),
        recent_logs: collect_recent_logs(ctx),
        sandbox: sandbox::show(ctx, paths, None).ok(),
        etc_dir: paths.etc_dir.display().to_string(),
        etc_missing: !paths.etc_dir.is_dir(),
        etc_files: collect_etc_files(paths, ctx.show_secrets)?,
    })
}

fn collect_services(ctx: &Context) -> Vec<ServiceReport> {
    let mut services = Vec::new();
    for agent_name in agent::AGENTS {
        let service = agent::service_for(agent_name);
        let out = common::command_output(
            ctx.runner,
            "systemctl",
            &[
                "show",
                "--property=Id,LoadState,ActiveState,SubState,UnitFileState",
                &service,
            ],
        );
        let properties = if out.ok() {
            parse_properties(&out.stdout)
        } else {
            BTreeMap::new()
        };
        services.push(ServiceReport {
            agent: (*agent_name).to_string(),
            service,
            ok: out.ok(),
            properties,
            error: if out.ok() { String::new() } else { out.stderr },
        });
    }
    services
}

fn parse_properties(stdout: &str) -> BTreeMap<String, String> {
    let mut properties = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            properties.insert(key.to_string(), value.to_string());
        }
    }
    properties
}

fn collect_recent_logs(ctx: &Context) -> Vec<LogSection> {
    let mut sections = Vec::new();
    for agent_name in agent::AGENTS {
        let service = agent::service_for(agent_name);
        let out = common::command_output(
            ctx.runner,
            "journalctl",
            &["-u", &service, "-n", "80", "-o", "json", "--no-pager"],
        );
        let lines = if out.ok() {
            out.stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| redact_journal_line(line, ctx.show_secrets))
                .collect()
        } else {
            Vec::new()
        };
        sections.push(LogSection {
            agent: (*agent_name).to_string(),
            lines,
        });
    }
    sections
}

fn redact_journal_line(line: &str, show_secrets: bool) -> String {
    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line) {
        if let Some(message) = obj.get("MESSAGE").and_then(Value::as_str) {
            return common::redact_line(message, show_secrets);
        }
    }
    common::redact_line(line, show_secrets)
}

fn collect_etc_files(paths: &Paths, show_secrets: bool) -> Result<Vec<EtcFileReport>, NczError> {
    if !paths.etc_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_files_recursive(&paths.etc_dir, 0, &mut files)?;
    files.sort();
    let mut reports = Vec::new();
    for file in files {
        let redacted_path = common::redact_path(&file);
        let lines = if redacted_path {
            vec!["[redacted path]".to_string()]
        } else if is_agent_env_file(paths, &file) {
            fs::read_to_string(&file)?
                .lines()
                .map(|line| redact_agent_env_line(line, show_secrets))
                .collect()
        } else {
            fs::read_to_string(&file)?
                .lines()
                .map(|line| common::redact_line(line, show_secrets))
                .collect()
        };
        reports.push(EtcFileReport {
            path: file.display().to_string(),
            redacted_path,
            lines,
        });
    }
    Ok(reports)
}

fn is_agent_env_file(paths: &Paths, file: &Path) -> bool {
    file == paths.agent_env()
        || agent::AGENTS
            .iter()
            .any(|agent_name| file == paths.agent_env_override(agent_name))
}

fn redact_agent_env_line(line: &str, show_secrets: bool) -> String {
    if show_secrets {
        return line.to_string();
    }
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return line.to_string();
    }
    let Some((left, _)) = line.split_once('=') else {
        return line.to_string();
    };
    format!("{left}=***")
}

fn collect_files_recursive(
    dir: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), NczError> {
    if depth >= 3 {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            files.push(path);
        } else if file_type.is_dir() {
            collect_files_recursive(&path, depth + 1, files)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::cli::Context;
    use crate::cmd::common::{out, test_paths};
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
    fn inspect_happy_path_redacts_etc_and_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.etc_dir.join("provider.conf"), "token=abc\nname=ok\n").unwrap();
        let runner = FakeRunner::new();
        runner.expect("date", &["-Is"], out(0, "2026-04-27T00:00:00+00:00\n", ""));
        runner.expect(
            "journalctl",
            &[
                "-u",
                "zeroclaw.service",
                "-n",
                "80",
                "-o",
                "json",
                "--no-pager",
            ],
            out(0, "{\"MESSAGE\":\"password=abc\"}\n", ""),
        );

        let report = collect(&ctx(&runner), &paths).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.recent_logs[0].lines[0], "password=***");
        assert_eq!(report.etc_files[0].lines[0], "token=***");
    }

    #[test]
    fn inspect_redacts_agent_env_values_for_arbitrary_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.agent_env_override("hermes").parent().unwrap()).unwrap();
        fs::write(paths.agent_env(), "FOO=plain\nOPENAI_KEY=secret\n").unwrap();
        fs::write(paths.agent_env_override("hermes"), "BAR=override\n").unwrap();

        let files = collect_etc_files(&paths, false).unwrap();

        let shared = files
            .iter()
            .find(|file| file.path == paths.agent_env().display().to_string())
            .unwrap();
        assert_eq!(shared.lines, vec!["FOO=***", "OPENAI_KEY=***"]);
        let scoped = files
            .iter()
            .find(|file| file.path == paths.agent_env_override("hermes").display().to_string())
            .unwrap();
        assert_eq!(scoped.lines, vec!["BAR=***"]);
    }

    #[test]
    fn inspect_redacts_legacy_provider_key_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "KEY=sk-live\nOPENAI_KEY=sk-openai\nNAME=local\n",
        )
        .unwrap();

        let files = collect_etc_files(&paths, false).unwrap();

        let provider = files
            .iter()
            .find(|file| file.path == paths.providers_dir().join("local.env").display().to_string())
            .unwrap();
        assert_eq!(
            provider.lines,
            vec!["KEY=***", "OPENAI_KEY=***", "NAME=local"]
        );
    }

    #[test]
    fn inspect_error_path_records_service_show_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect(
            "systemctl",
            &[
                "show",
                "--property=Id,LoadState,ActiveState,SubState,UnitFileState",
                "zeroclaw.service",
            ],
            out(1, "", "not found\n"),
        );

        let report = collect(&ctx(&runner), &paths).unwrap();
        assert!(!report.services[0].ok);
        assert_eq!(report.services[0].error, "not found\n");
    }
}
