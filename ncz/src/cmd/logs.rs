//! logs — print recent logs for the active or named agent.

use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{agent, Paths};

#[derive(Debug, Serialize)]
pub struct LogsReport {
    pub schema_version: u32,
    pub agent: String,
    pub service: String,
    pub entries: Vec<LogEntry>,
}

#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub timestamp: Option<String>,
    pub priority: Option<String>,
    pub message: String,
}

impl Render for LogsReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for entry in &self.entries {
            writeln!(w, "{}", entry.message)?;
        }
        Ok(())
    }
}

pub fn run(ctx: &Context, agent: Option<&str>) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = collect(ctx, &paths, agent)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn collect(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
) -> Result<LogsReport, NczError> {
    let agent = common::resolve_agent(paths, requested_agent)?;
    let service = agent::service_for(&agent);
    let out = ctx
        .runner
        .run(
            "journalctl",
            &["-u", &service, "-n", "200", "-o", "json", "--no-pager"],
        )
        .map_err(|e| NczError::Exec {
            cmd: "journalctl".into(),
            msg: e.to_string(),
        })?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("journalctl -u {service}"),
            msg: out.stderr,
        });
    }

    Ok(LogsReport {
        schema_version: common::SCHEMA_VERSION,
        agent,
        service,
        entries: parse_entries(&out.stdout, ctx.show_secrets),
    })
}

fn parse_entries(stdout: &str, show_secrets: bool) -> Vec<LogEntry> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| parse_entry(line, show_secrets))
        .collect()
}

fn parse_entry(line: &str, show_secrets: bool) -> LogEntry {
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(obj)) => {
            let message = obj
                .get("MESSAGE")
                .and_then(Value::as_str)
                .unwrap_or(line)
                .to_string();
            LogEntry {
                timestamp: obj
                    .get("__REALTIME_TIMESTAMP")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                priority: obj
                    .get("PRIORITY")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                message: common::redact_line(&message, show_secrets),
            }
        }
        _ => LogEntry {
            timestamp: None,
            priority: None,
            message: common::redact_line(line, show_secrets),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Context;
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
    fn logs_happy_path_reads_and_redacts_journal_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect(
            "journalctl",
            &[
                "-u",
                "hermes.service",
                "-n",
                "200",
                "-o",
                "json",
                "--no-pager",
            ],
            out(
                0,
                "{\"MESSAGE\":\"api_key=abc123\",\"PRIORITY\":\"6\",\"__REALTIME_TIMESTAMP\":\"1\"}\n",
                "",
            ),
        );

        let report = collect(&ctx(&runner), &paths, Some("hermes")).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.entries[0].message, "api_key=***");
    }

    #[test]
    fn logs_error_path_propagates_journal_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect(
            "journalctl",
            &[
                "-u",
                "hermes.service",
                "-n",
                "200",
                "-o",
                "json",
                "--no-pager",
            ],
            out(1, "", "no journal\n"),
        );

        let err = collect(&ctx(&runner), &paths, Some("hermes")).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
    }
}
