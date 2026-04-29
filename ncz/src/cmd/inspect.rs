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
use crate::state::{agent, url as url_state, Paths};

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
        let redacted_path = !show_secrets && common::redact_path(&file);
        let lines = if redacted_path {
            vec!["[redacted path]".to_string()]
        } else if is_agent_env_file(paths, &file) {
            fs::read_to_string(&file)?
                .lines()
                .map(|line| redact_agent_env_line(line, show_secrets))
                .collect()
        } else if is_mcp_file(paths, &file) {
            redact_mcp_file(&file, show_secrets)?
        } else if is_legacy_provider_env_file(paths, &file) {
            redact_legacy_provider_env_file(&file, show_secrets)?
        } else {
            fs::read_to_string(&file)?
                .lines()
                .map(|line| common::redact_line(line, show_secrets))
                .collect()
        };
        reports.push(EtcFileReport {
            path: if redacted_path {
                "[redacted path]".to_string()
            } else {
                file.display().to_string()
            },
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

fn is_mcp_file(paths: &Paths, file: &Path) -> bool {
    file.starts_with(paths.mcp_dir())
        && file
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "json")
}

fn is_legacy_provider_env_file(paths: &Paths, file: &Path) -> bool {
    file.starts_with(paths.providers_dir())
        && file
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext, "env" | "conf"))
}

fn redact_legacy_provider_env_file(
    file: &Path,
    show_secrets: bool,
) -> Result<Vec<String>, NczError> {
    Ok(fs::read_to_string(file)?
        .lines()
        .map(|line| redact_legacy_provider_env_line(line, show_secrets))
        .collect())
}

fn redact_legacy_provider_env_line(line: &str, show_secrets: bool) -> String {
    if show_secrets {
        return line.to_string();
    }
    let Some((left, _)) = line.split_once('=') else {
        return common::redact_line(line, show_secrets);
    };
    format!("{left}=***")
}

fn redact_mcp_file(file: &Path, show_secrets: bool) -> Result<Vec<String>, NczError> {
    let body = fs::read_to_string(file)?;
    if show_secrets {
        return Ok(body.lines().map(ToOwned::to_owned).collect());
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&body) else {
        return Ok(body
            .lines()
            .map(|line| common::redact_line(line, show_secrets))
            .collect());
    };
    if value
        .get("transport")
        .and_then(Value::as_str)
        .is_some_and(|transport| transport == "stdio")
    {
        if let Value::Object(obj) = &mut value {
            if obj.contains_key("command") {
                obj.insert("command".to_string(), Value::String("***".to_string()));
            }
        }
    }
    if let Value::Object(obj) = &mut value {
        if obj
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(mcp_url_needs_redaction)
        {
            obj.insert("url".to_string(), Value::String("***".to_string()));
        }
    }
    let rendered = serde_json::to_string_pretty(&value)?;
    Ok(rendered
        .lines()
        .map(|line| common::redact_line(line, show_secrets))
        .collect())
}

fn mcp_url_needs_redaction(url: &str) -> bool {
    url_state::has_userinfo(url)
        || url_state::has_query_or_fragment(url)
        || url_state::contains_secret_path_material(url)
}

fn redact_agent_env_line(line: &str, show_secrets: bool) -> String {
    if show_secrets {
        return line.to_string();
    }
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return line.to_string();
    }
    if trimmed.starts_with('#') || trimmed.starts_with(';') {
        return common::redact_line(line, show_secrets);
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
        fs::write(
            paths.agent_env(),
            "FOO=plain\n# OLD_API_KEY=old-secret\n; TOKEN=old-token\nOPENAI_KEY=secret\n",
        )
        .unwrap();
        fs::write(paths.agent_env_override("hermes"), "BAR=override\n").unwrap();

        let files = collect_etc_files(&paths, false).unwrap();

        let shared = files
            .iter()
            .find(|file| file.path == paths.agent_env().display().to_string())
            .unwrap();
        assert_eq!(
            shared.lines,
            vec![
                "FOO=***",
                "# OLD_API_KEY=***",
                "; TOKEN=***",
                "OPENAI_KEY=***"
            ]
        );
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
            .find(|file| {
                file.path
                    == paths
                        .providers_dir()
                        .join("local.env")
                        .display()
                        .to_string()
            })
            .unwrap();
        assert_eq!(
            provider.lines,
            vec!["KEY=***", "OPENAI_KEY=***", "NAME=***"]
        );
    }

    #[test]
    fn inspect_redacts_custom_key_env_legacy_provider_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("custom.env"),
            "PROVIDER_NAME=custom\nKEY_ENV=FOO\nFOO=sk-live\n",
        )
        .unwrap();

        let files = collect_etc_files(&paths, false).unwrap();

        let provider = files
            .iter()
            .find(|file| {
                file.path
                    == paths
                        .providers_dir()
                        .join("custom.env")
                        .display()
                        .to_string()
            })
            .unwrap();
        assert_eq!(
            provider.lines,
            vec!["PROVIDER_NAME=***", "KEY_ENV=***", "FOO=***"]
        );
    }

    #[test]
    fn inspect_redacts_model_cache_credential_fingerprints() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("example.models.json"),
            r#"{"schema_version":1,"provider":"example","provider_fingerprint":"public","credential_fingerprint":"ncz-v1:agent-env:deadbeef","fetched_at":"1","models":[]}"#,
        )
        .unwrap();

        let files = collect_etc_files(&paths, false).unwrap();

        let cache = files
            .iter()
            .find(|file| {
                file.path
                    == paths
                        .providers_dir()
                        .join("example.models.json")
                        .display()
                        .to_string()
            })
            .unwrap();
        assert!(!cache.lines.iter().any(|line| line.contains("deadbeef")));
        assert!(cache
            .lines
            .iter()
            .any(|line| line.contains("credential_fingerprint") && line.contains("***")));
    }

    #[test]
    fn inspect_redacts_secret_bearing_paths_in_text_and_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("provider-token-secret.json"),
            "secret path contents\n",
        )
        .unwrap();

        let etc_files = collect_etc_files(&paths, false).unwrap();
        let report = InspectReport {
            schema_version: 1,
            generated_at: "2026-04-28T00:00:00+00:00".to_string(),
            version: None,
            status: None,
            services: Vec::new(),
            recent_logs: Vec::new(),
            sandbox: None,
            etc_dir: paths.etc_dir.display().to_string(),
            etc_missing: false,
            etc_files,
        };
        let mut rendered = Vec::new();
        report.render_text(&mut rendered).unwrap();
        let text = String::from_utf8(rendered).unwrap();
        let json = serde_json::to_string(&report).unwrap();

        assert!(!text.contains("provider-token-secret"));
        assert!(!json.contains("provider-token-secret"));
        assert!(text.contains("-- [redacted path] --"));
        assert!(json.contains(r#""path":"[redacted path]""#));
    }

    #[test]
    fn inspect_redacts_mcp_stdio_command_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"stdio","command":"search-mcp --user alice:secret","url":null,"auth_env":null}"#,
        )
        .unwrap();

        let files = collect_etc_files(&paths, false).unwrap();

        let mcp = files
            .iter()
            .find(|file| file.path == paths.mcp_dir().join("search.json").display().to_string())
            .unwrap();
        assert!(mcp.lines.iter().any(|line| line == r#"  "command": "***","#));
        assert!(!mcp.lines.iter().any(|line| line.contains("alice:secret")));
    }

    #[test]
    fn inspect_redacts_secret_bearing_mcp_urls_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test/sse/sk-live","auth_env":null}"#,
        )
        .unwrap();
        fs::write(
            paths.mcp_dir().join("query.json"),
            r#"{"schema_version":1,"name":"query","transport":"http","command":null,"url":"https://mcp.example.test/sse?token=secret","auth_env":null}"#,
        )
        .unwrap();

        let files = collect_etc_files(&paths, false).unwrap();

        for name in ["search", "query"] {
            let mcp = files
                .iter()
                .find(|file| file.path == paths.mcp_dir().join(format!("{name}.json")).display().to_string())
                .unwrap();
            assert!(mcp.lines.iter().any(|line| line.contains(r#""url": "***""#)));
            assert!(!mcp.lines.iter().any(|line| line.contains("sk-live")));
            assert!(!mcp.lines.iter().any(|line| line.contains("token=secret")));
        }
    }

    #[test]
    fn inspect_reveals_mcp_stdio_command_with_show_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"stdio","command":"search-mcp --socket /run/search.sock","url":null,"auth_env":null}"#,
        )
        .unwrap();

        let files = collect_etc_files(&paths, true).unwrap();

        let mcp = files
            .iter()
            .find(|file| file.path == paths.mcp_dir().join("search.json").display().to_string())
            .unwrap();
        assert!(mcp
            .lines
            .iter()
            .any(|line| line.contains("search-mcp --socket /run/search.sock")));
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
