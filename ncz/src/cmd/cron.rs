//! cron — wrapper around zeroclaw's in-container cron CLI.

use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use crate::cli::{Context, CronAction};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::Paths;
use crate::sys::ProcessOutput;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum CronReport {
    List(CronListReport),
    Mutation(CronMutationReport),
}

impl Render for CronReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            CronReport::List(report) => report.render_text(w),
            CronReport::Mutation(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CronListReport {
    pub schema_version: u32,
    pub agent: String,
    pub entries: Vec<CronEntryReport>,
    #[serde(skip_serializing)]
    raw_stdout: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct CronEntryReport {
    pub id: String,
    pub schedule: String,
    pub command: String,
    pub status: String,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CronMutationReport {
    pub schema_version: u32,
    pub agent: String,
    pub id: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing)]
    raw_stdout: Option<String>,
}

impl Render for CronListReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if let Some(raw_stdout) = &self.raw_stdout {
            write!(w, "{raw_stdout}")?;
            if !raw_stdout.ends_with('\n') {
                writeln!(w)?;
            }
            return Ok(());
        }

        for entry in &self.entries {
            writeln!(
                w,
                "{:<18} status={:<8} schedule={} next={} last={} command={}",
                entry.id,
                entry.status,
                entry.schedule,
                entry.next_run.as_deref().unwrap_or("unknown"),
                entry.last_run.as_deref().unwrap_or("never"),
                entry.command
            )?;
        }
        Ok(())
    }
}

impl Render for CronMutationReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if let Some(raw_stdout) = &self.raw_stdout {
            write!(w, "{raw_stdout}")?;
            if !raw_stdout.ends_with('\n') {
                writeln!(w)?;
            }
            return Ok(());
        }
        writeln!(w, "Cron {}: {}", self.action, self.id)
    }
}

pub fn run(ctx: &Context, action: CronAction) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: CronAction,
) -> Result<CronReport, NczError> {
    match action {
        CronAction::List { agent } => Ok(CronReport::List(list(ctx, paths, agent.as_deref())?)),
        CronAction::Add {
            id,
            schedule,
            command,
            agent,
        } => Ok(CronReport::Mutation(add(
            ctx,
            paths,
            agent.as_deref(),
            &id,
            &schedule,
            &command,
        )?)),
        CronAction::AddAt { id, at, command } => Ok(CronReport::Mutation(add_at(
            ctx, paths, &id, &at, &command,
        )?)),
        CronAction::AddEvery { id, every, command } => Ok(CronReport::Mutation(add_every(
            ctx, paths, &id, &every, &command,
        )?)),
        CronAction::Once { id, command } => {
            Ok(CronReport::Mutation(once(ctx, paths, &id, &command)?))
        }
        CronAction::Remove { id, agent } => Ok(CronReport::Mutation(remove(
            ctx,
            paths,
            agent.as_deref(),
            &id,
        )?)),
        CronAction::Update {
            id,
            schedule,
            command,
        } => Ok(CronReport::Mutation(update(
            ctx,
            paths,
            &id,
            schedule.as_deref(),
            command.as_deref(),
        )?)),
        CronAction::Pause { id, agent } => Ok(CronReport::Mutation(pause(
            ctx,
            paths,
            agent.as_deref(),
            &id,
        )?)),
        CronAction::Resume { id, agent } => Ok(CronReport::Mutation(resume(
            ctx,
            paths,
            agent.as_deref(),
            &id,
        )?)),
    }
}

pub fn list(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
) -> Result<CronListReport, NczError> {
    let agent = require_zeroclaw(paths, requested_agent)?;
    common::require_tool(ctx.runner, "podman", &["--version"])?;
    let out = podman_exec(ctx, &agent, &["cron", "list"])?;
    let entries = parse_entries(&out.stdout)?;
    let raw_stdout = if entries.is_empty() && !looks_like_json(&out.stdout) {
        Some(out.stdout)
    } else {
        None
    };

    Ok(CronListReport {
        schema_version: common::SCHEMA_VERSION,
        agent,
        entries,
        raw_stdout,
    })
}

pub fn add(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
    id: &str,
    schedule: &str,
    command: &str,
) -> Result<CronMutationReport, NczError> {
    mutate(
        ctx,
        paths,
        requested_agent,
        id,
        "add",
        &[
            "cron",
            "add",
            id,
            "--schedule",
            schedule,
            "--command",
            command,
        ],
        Some(schedule),
        Some(command),
    )
}

pub fn add_at(
    ctx: &Context,
    paths: &Paths,
    id: &str,
    at: &str,
    command: &str,
) -> Result<CronMutationReport, NczError> {
    mutate(
        ctx,
        paths,
        None,
        id,
        "add-at",
        &["cron", "add-at", id, "--at", at, "--command", command],
        Some(at),
        Some(command),
    )
}

pub fn add_every(
    ctx: &Context,
    paths: &Paths,
    id: &str,
    every: &str,
    command: &str,
) -> Result<CronMutationReport, NczError> {
    mutate(
        ctx,
        paths,
        None,
        id,
        "add-every",
        &[
            "cron",
            "add-every",
            id,
            "--every",
            every,
            "--command",
            command,
        ],
        Some(every),
        Some(command),
    )
}

pub fn once(
    ctx: &Context,
    paths: &Paths,
    id: &str,
    command: &str,
) -> Result<CronMutationReport, NczError> {
    mutate(
        ctx,
        paths,
        None,
        id,
        "once",
        &["cron", "once", id, "--command", command],
        None,
        Some(command),
    )
}

pub fn remove(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
    id: &str,
) -> Result<CronMutationReport, NczError> {
    mutate(
        ctx,
        paths,
        requested_agent,
        id,
        "remove",
        &["cron", "remove", id],
        None,
        None,
    )
}

pub fn update(
    ctx: &Context,
    paths: &Paths,
    id: &str,
    schedule: Option<&str>,
    command: Option<&str>,
) -> Result<CronMutationReport, NczError> {
    if schedule.is_none() && command.is_none() {
        return Err(NczError::Usage(
            "cron update requires --schedule or --command".to_string(),
        ));
    }

    let agent = require_zeroclaw(paths, None)?;
    common::require_tool(ctx.runner, "podman", &["--version"])?;

    let mut args = vec!["cron", "update", id];
    if let Some(schedule) = schedule {
        args.extend(["--schedule", schedule]);
    }
    if let Some(command) = command {
        args.extend(["--command", command]);
    }
    let out = podman_exec(ctx, &agent, &args)?;
    Ok(mutation_report(
        agent, id, "update", schedule, command, out.stdout,
    ))
}

pub fn pause(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
    id: &str,
) -> Result<CronMutationReport, NczError> {
    mutate(
        ctx,
        paths,
        requested_agent,
        id,
        "pause",
        &["cron", "pause", id],
        None,
        None,
    )
}

pub fn resume(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
    id: &str,
) -> Result<CronMutationReport, NczError> {
    mutate(
        ctx,
        paths,
        requested_agent,
        id,
        "resume",
        &["cron", "resume", id],
        None,
        None,
    )
}

fn mutate(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
    id: &str,
    action: &str,
    args: &[&str],
    schedule: Option<&str>,
    command: Option<&str>,
) -> Result<CronMutationReport, NczError> {
    let agent = require_zeroclaw(paths, requested_agent)?;
    common::require_tool(ctx.runner, "podman", &["--version"])?;
    let out = podman_exec(ctx, &agent, args)?;
    Ok(mutation_report(
        agent, id, action, schedule, command, out.stdout,
    ))
}

fn mutation_report(
    agent: String,
    id: &str,
    action: &str,
    schedule: Option<&str>,
    command: Option<&str>,
    stdout: String,
) -> CronMutationReport {
    CronMutationReport {
        schema_version: common::SCHEMA_VERSION,
        agent,
        id: id.to_string(),
        action: action.to_string(),
        schedule: schedule.map(ToOwned::to_owned),
        command: command.map(ToOwned::to_owned),
        raw_stdout: if stdout.trim().is_empty() {
            None
        } else {
            Some(stdout)
        },
    }
}

fn require_zeroclaw(paths: &Paths, requested_agent: Option<&str>) -> Result<String, NczError> {
    let agent = common::resolve_agent(paths, requested_agent)?;
    if agent != "zeroclaw" {
        return Err(NczError::Precondition(format!(
            "ncz cron currently supports zeroclaw only; {agent} cron interface is deferred"
        )));
    }
    Ok(agent)
}

fn podman_exec(ctx: &Context, agent: &str, agent_args: &[&str]) -> Result<ProcessOutput, NczError> {
    let mut args = vec!["exec", agent, agent];
    args.extend_from_slice(agent_args);
    let out = ctx.runner.run("podman", &args)?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("podman {}", args.join(" ")),
            msg: if out.stderr.trim().is_empty() {
                out.stdout
            } else {
                out.stderr
            },
        });
    }
    Ok(out)
}

fn parse_entries(stdout: &str) -> Result<Vec<CronEntryReport>, NczError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || !looks_like_json(trimmed) {
        return Ok(vec![]);
    }

    let value: Value = serde_json::from_str(trimmed)?;
    let entries = match &value {
        Value::Array(items) => items,
        Value::Object(obj) => obj
            .get("entries")
            .or_else(|| obj.get("jobs"))
            .or_else(|| obj.get("tasks"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };

    entries.iter().map(entry_from_value).collect()
}

fn entry_from_value(value: &Value) -> Result<CronEntryReport, NczError> {
    let Value::Object(obj) = value else {
        return Err(NczError::Precondition(
            "zeroclaw cron list returned a non-object entry".to_string(),
        ));
    };

    let id = string_field(obj, &["id", "job_id", "jobId"]).unwrap_or_default();
    let schedule = string_field(obj, &["schedule", "expression", "cron", "expr"])
        .or_else(|| json_field(obj, "schedule"))
        .unwrap_or_default();
    let command = string_field(obj, &["command", "cmd", "prompt"]).unwrap_or_default();
    let status = string_field(obj, &["status"])
        .or_else(|| {
            bool_field(obj, "enabled")
                .map(|enabled| if enabled { "active" } else { "paused" }.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let last_run = string_field(obj, &["last_run", "lastRun"]);
    let next_run = string_field(obj, &["next_run", "nextRun"]);

    Ok(CronEntryReport {
        id,
        schedule,
        command,
        status,
        last_run,
        next_run,
    })
}

fn string_field(obj: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        obj.get(*name).and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            Value::Bool(flag) => Some(flag.to_string()),
            _ => None,
        })
    })
}

fn bool_field(obj: &serde_json::Map<String, Value>, name: &str) -> Option<bool> {
    obj.get(name).and_then(Value::as_bool)
}

fn json_field(obj: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    obj.get(name).and_then(|value| match value {
        Value::Null => None,
        other => Some(other.to_string()),
    })
}

fn looks_like_json(stdout: &str) -> bool {
    let trimmed = stdout.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::cli::{Context, CronAction};
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

    fn paths_with_agent(root: &std::path::Path, agent: &str) -> Paths {
        let paths = test_paths(root);
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), format!("{agent}\n")).unwrap();
        paths
    }

    fn expect_podman(runner: &FakeRunner) {
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
    }

    #[test]
    fn cron_list_wraps_zeroclaw_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &["exec", "zeroclaw", "zeroclaw", "cron", "list"],
            out(
                0,
                r#"{"entries":[{"id":"daily","schedule":"0 9 * * *","command":"echo ok","status":"active","last_run":null,"next_run":"2026-04-30T09:00:00Z"}]}"#,
                "",
            ),
        );

        let report = list(&ctx(&runner), &paths, None).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.agent, "zeroclaw");
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].id, "daily");
        assert_eq!(
            report.entries[0].next_run.as_deref(),
            Some("2026-04-30T09:00:00Z")
        );
        runner.assert_done();
    }

    #[test]
    fn cron_add_dispatches_to_podman_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &[
                "exec",
                "zeroclaw",
                "zeroclaw",
                "cron",
                "add",
                "daily",
                "--schedule",
                "0 9 * * *",
                "--command",
                "echo ok",
            ],
            out(0, "added\n", ""),
        );

        let report = add(
            &ctx(&runner),
            &paths,
            Some("zeroclaw"),
            "daily",
            "0 9 * * *",
            "echo ok",
        )
        .unwrap();
        assert_eq!(report.id, "daily");
        assert_eq!(report.action, "add");
        assert_eq!(report.schedule.as_deref(), Some("0 9 * * *"));
        runner.assert_done();
    }

    #[test]
    fn cron_add_at_dispatches_to_active_zeroclaw() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &[
                "exec",
                "zeroclaw",
                "zeroclaw",
                "cron",
                "add-at",
                "once",
                "--at",
                "2026-04-30T12:00:00Z",
                "--command",
                "echo once",
            ],
            out(0, "", ""),
        );

        let report = add_at(
            &ctx(&runner),
            &paths,
            "once",
            "2026-04-30T12:00:00Z",
            "echo once",
        )
        .unwrap();
        assert_eq!(report.action, "add-at");
        runner.assert_done();
    }

    #[test]
    fn cron_add_every_dispatches_to_active_zeroclaw() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &[
                "exec",
                "zeroclaw",
                "zeroclaw",
                "cron",
                "add-every",
                "heartbeat",
                "--every",
                "5m",
                "--command",
                "echo beat",
            ],
            out(0, "", ""),
        );

        let report = add_every(&ctx(&runner), &paths, "heartbeat", "5m", "echo beat").unwrap();
        assert_eq!(report.action, "add-every");
        runner.assert_done();
    }

    #[test]
    fn cron_once_dispatches_to_active_zeroclaw() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &[
                "exec",
                "zeroclaw",
                "zeroclaw",
                "cron",
                "once",
                "startup",
                "--command",
                "echo boot",
            ],
            out(0, "", ""),
        );

        let report = once(&ctx(&runner), &paths, "startup", "echo boot").unwrap();
        assert_eq!(report.action, "once");
        runner.assert_done();
    }

    #[test]
    fn cron_remove_dispatches_to_podman_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &["exec", "zeroclaw", "zeroclaw", "cron", "remove", "daily"],
            out(0, "", ""),
        );

        let report = remove(&ctx(&runner), &paths, None, "daily").unwrap();
        assert_eq!(report.action, "remove");
        runner.assert_done();
    }

    #[test]
    fn cron_update_dispatches_optional_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &[
                "exec",
                "zeroclaw",
                "zeroclaw",
                "cron",
                "update",
                "daily",
                "--schedule",
                "0 10 * * *",
                "--command",
                "echo later",
            ],
            out(0, "", ""),
        );

        let report = update(
            &ctx(&runner),
            &paths,
            "daily",
            Some("0 10 * * *"),
            Some("echo later"),
        )
        .unwrap();
        assert_eq!(report.action, "update");
        runner.assert_done();
    }

    #[test]
    fn cron_update_requires_a_field() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();

        let err = update(&ctx(&runner), &paths, "daily", None, None).unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn cron_pause_and_resume_dispatch_to_podman_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &["exec", "zeroclaw", "zeroclaw", "cron", "pause", "daily"],
            out(0, "", ""),
        );
        expect_podman(&runner);
        runner.expect(
            "podman",
            &["exec", "zeroclaw", "zeroclaw", "cron", "resume", "daily"],
            out(0, "", ""),
        );

        assert_eq!(
            pause(&ctx(&runner), &paths, None, "daily").unwrap().action,
            "pause"
        );
        assert_eq!(
            resume(&ctx(&runner), &paths, None, "daily").unwrap().action,
            "resume"
        );
        runner.assert_done();
    }

    #[test]
    fn cron_rejects_non_zeroclaw_before_podman() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "openclaw");
        let runner = FakeRunner::new();

        let err = list(&ctx(&runner), &paths, None).unwrap_err();
        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains(
            "ncz cron currently supports zeroclaw only; openclaw cron interface is deferred"
        ));
        runner.assert_done();
    }

    #[test]
    fn cron_action_dispatch_supports_cli_enum() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_agent(tmp.path(), "zeroclaw");
        let runner = FakeRunner::new();
        expect_podman(&runner);
        runner.expect(
            "podman",
            &["exec", "zeroclaw", "zeroclaw", "cron", "remove", "daily"],
            out(0, "", ""),
        );

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            CronAction::Remove {
                id: "daily".to_string(),
                agent: None,
            },
        )
        .unwrap();
        assert!(matches!(report, CronReport::Mutation(_)));
        runner.assert_done();
    }
}
