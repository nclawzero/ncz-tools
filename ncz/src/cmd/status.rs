//! status — print device and active-agent state.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{agent, Paths};

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub host: String,
    pub kernel: String,
    pub active_agent: String,
    pub running_agents: String,
    pub state_inconsistent: bool,
    pub network: String,
    pub storage: String,
}

impl Render for StatusReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "Device:       {}",
            if self.host.is_empty() {
                "unknown"
            } else {
                &self.host
            }
        )?;
        writeln!(
            w,
            "Kernel:       {}",
            if self.kernel.is_empty() {
                "unknown"
            } else {
                &self.kernel
            }
        )?;
        writeln!(w, "Active agent: {}", self.active_agent)?;
        writeln!(
            w,
            "Running:      {}",
            if self.running_agents.is_empty() {
                "none"
            } else {
                &self.running_agents
            }
        )?;
        writeln!(w, "Network:      {}", self.network)?;
        writeln!(
            w,
            "Storage:      {}",
            if self.storage.is_empty() {
                "unknown"
            } else {
                &self.storage
            }
        )?;
        writeln!(
            w,
            "State:        {}",
            if self.state_inconsistent {
                "inconsistent"
            } else {
                "ok"
            }
        )
    }
}

pub fn run(ctx: &Context) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = collect(ctx, &paths)?;
    let code = if report.state_inconsistent { 3 } else { 0 };
    output::emit(&report, ctx.json)?;
    Ok(code)
}

pub fn collect(ctx: &Context, paths: &Paths) -> Result<StatusReport, NczError> {
    let active_agent = agent::read(paths)?;
    let running = common::running_agents(ctx.runner);
    let running_agents = running.join(",");
    let state_inconsistent = !agent::AGENTS.contains(&active_agent.as_str())
        || running.len() > 1
        || (running.len() == 1 && running[0] != active_agent);

    Ok(StatusReport {
        schema_version: common::SCHEMA_VERSION,
        host: common::command_stdout(ctx.runner, "hostname", &[]).unwrap_or_default(),
        kernel: common::command_stdout(ctx.runner, "uname", &["-r"]).unwrap_or_default(),
        active_agent,
        running_agents,
        state_inconsistent,
        network: common::network_status(ctx.runner, true),
        storage: storage(ctx),
    })
}

fn storage(ctx: &Context) -> String {
    let out = common::command_output(ctx.runner, "df", &["-h", "/"]);
    if !out.ok() {
        return String::new();
    }
    let Some(line) = out.stdout.lines().nth(1) else {
        return String::new();
    };
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() >= 5 {
        format!("{} used ({} free)", cols[4], cols[3])
    } else {
        String::new()
    }
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
    fn status_happy_path_reports_ok_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "zeroclaw\n").unwrap();

        let runner = FakeRunner::new();
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(3, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "hermes.service"],
            out(3, "", ""),
        );
        runner.expect("hostname", &[], out(0, "edge-01\n", ""));
        runner.expect("uname", &["-r"], out(0, "6.6.0\n", ""));
        runner.expect("ip", &["route", "get", "1.1.1.1"], out(0, "ok\n", ""));
        runner.expect(
            "df",
            &["-h", "/"],
            out(
                0,
                "Filesystem Size Used Avail Use% Mounted on\n/dev/root 10G 5G 5G 50% /\n",
                "",
            ),
        );

        let report = collect(&ctx(&runner), &paths).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.active_agent, "zeroclaw");
        assert_eq!(report.running_agents, "zeroclaw");
        assert!(!report.state_inconsistent);
        assert_eq!(report.storage, "50% used (5G free)");
    }

    #[test]
    fn status_error_path_reports_inconsistent_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "not-an-agent\n").unwrap();

        let runner = FakeRunner::new();
        for agent_name in agent::AGENTS {
            runner.expect(
                "systemctl",
                &["is-active", "--quiet", &format!("{agent_name}.service")],
                out(3, "", ""),
            );
        }

        let report = collect(&ctx(&runner), &paths).unwrap();
        assert!(report.state_inconsistent);
    }
}
