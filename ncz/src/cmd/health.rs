//! health — one-line health summary for scripts and humans.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::Paths;

#[derive(Debug, Serialize)]
pub struct HealthReport {
    pub schema_version: u32,
    pub result: String,
    pub active_agent: String,
    pub running_agents: String,
    pub network: String,
}

impl Render for HealthReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "{} active={} running={} network={}",
            self.result,
            self.active_agent,
            if self.running_agents.is_empty() {
                "none"
            } else {
                &self.running_agents
            },
            self.network
        )
    }
}

pub fn run(ctx: &Context) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = collect(ctx, &paths)?;
    let code = if report.result == "red" { 3 } else { 0 };
    output::emit(&report, ctx.json)?;
    Ok(code)
}

pub fn collect(ctx: &Context, paths: &Paths) -> Result<HealthReport, NczError> {
    let active_agent = crate::state::agent::read(paths)?;
    let running = common::running_agents(ctx.runner);
    let running_agents = running.join(",");
    let network = common::network_status(ctx.runner, false);

    let result = if running.len() > 1 || (running.len() == 1 && running[0] != active_agent) {
        "red"
    } else if running.is_empty() || network != "ok" {
        "yellow"
    } else {
        "green"
    };

    Ok(HealthReport {
        schema_version: common::SCHEMA_VERSION,
        result: result.to_string(),
        active_agent,
        running_agents,
        network,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::cli::Context;
    use crate::cmd::common::{out, test_paths};
    use crate::state::agent;
    use crate::sys::fake::FakeRunner;

    use super::*;

    fn ctx<'a>(runner: &'a FakeRunner) -> Context<'a> {
        Context {
            json: false,
            show_secrets: false,
            runner,
        }
    }

    fn expect_running(runner: &FakeRunner, active: &str) {
        for agent_name in agent::AGENTS {
            runner.expect(
                "systemctl",
                &["is-active", "--quiet", &format!("{agent_name}.service")],
                out(if *agent_name == active { 0 } else { 3 }, "", ""),
            );
        }
    }

    #[test]
    fn health_happy_path_reports_green() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "zeroclaw\n").unwrap();
        let runner = FakeRunner::new();
        expect_running(&runner, "zeroclaw");
        runner.expect("ip", &["route", "get", "1.1.1.1"], out(0, "", ""));

        let report = collect(&ctx(&runner), &paths).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.result, "green");
    }

    #[test]
    fn health_error_path_reports_red_for_running_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "zeroclaw\n").unwrap();
        let runner = FakeRunner::new();
        expect_running(&runner, "hermes");
        runner.expect("ip", &["route", "get", "1.1.1.1"], out(0, "", ""));

        let report = collect(&ctx(&runner), &paths).unwrap();
        assert_eq!(report.result, "red");
    }
}
