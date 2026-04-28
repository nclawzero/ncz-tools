//! pause — stop the active or named agent service.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{agent, Paths};
use crate::sys::systemd;

#[derive(Debug, Serialize)]
pub struct PauseReport {
    pub schema_version: u32,
    pub agent: String,
    pub service: String,
    pub paused: bool,
}

impl Render for PauseReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Paused {}.", self.agent)
    }
}

pub fn run(ctx: &Context, agent: Option<&str>) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = pause(ctx, &paths, agent)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn pause(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
) -> Result<PauseReport, NczError> {
    common::require_tool(ctx.runner, "systemctl", &["--version"])?;
    let agent = common::resolve_agent(paths, requested_agent)?;
    let service = agent::service_for(&agent);
    systemd::stop(ctx.runner, &service)?;
    Ok(PauseReport {
        schema_version: common::SCHEMA_VERSION,
        agent,
        service,
        paused: true,
    })
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

    fn expect_unit_state(
        runner: &FakeRunner,
        unit: &str,
        load_state: &str,
        active_state: &str,
        sub_state: &str,
    ) {
        runner.expect(
            "systemctl",
            &[
                "show",
                unit,
                "--property=LoadState",
                "--property=ActiveState",
                "--property=SubState",
            ],
            out(
                0,
                &format!(
                    "LoadState={load_state}\nActiveState={active_state}\nSubState={sub_state}\n"
                ),
                "",
            ),
        );
    }

    #[test]
    fn pause_happy_path_stops_named_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        expect_unit_state(&runner, "hermes.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "hermes.service"],
            out(0, "", ""),
        );
        expect_unit_state(&runner, "hermes.service", "loaded", "inactive", "dead");

        let report = pause(&ctx(&runner), &paths, Some("hermes")).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.agent, "hermes");
        assert!(report.paused);
        runner.assert_done();
    }

    #[test]
    fn pause_error_path_reports_missing_systemctl() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let err = pause(&ctx(&runner), &paths, Some("hermes")).unwrap_err();
        assert!(matches!(err, NczError::MissingDep(_)));
    }

    #[test]
    fn pause_error_path_reports_failed_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        expect_unit_state(&runner, "hermes.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "hermes.service"],
            out(1, "", "operation failed"),
        );
        expect_unit_state(&runner, "hermes.service", "loaded", "active", "running");

        let err = pause(&ctx(&runner), &paths, Some("hermes")).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }
}
