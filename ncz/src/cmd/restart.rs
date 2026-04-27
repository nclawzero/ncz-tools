//! restart — restart the active or named agent service.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{agent, Paths};
use crate::sys::systemd;

#[derive(Debug, Serialize)]
pub struct RestartReport {
    pub schema_version: u32,
    pub agent: String,
    pub service: String,
    pub restarted: bool,
}

impl Render for RestartReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Restarted {}.", self.agent)
    }
}

pub fn run(ctx: &Context, agent: Option<&str>) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = restart(ctx, &paths, agent)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn restart(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
) -> Result<RestartReport, NczError> {
    common::require_tool(ctx.runner, "systemctl", &["--version"])?;
    let agent = common::resolve_agent(paths, requested_agent)?;
    let service = agent::service_for(&agent);
    systemd::restart(ctx.runner, &service)?;
    Ok(RestartReport {
        schema_version: common::SCHEMA_VERSION,
        agent,
        service,
        restarted: true,
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

    #[test]
    fn restart_happy_path_restarts_named_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect(
            "sudo",
            &["systemctl", "restart", "openclaw.service"],
            out(0, "", ""),
        );

        let report = restart(&ctx(&runner), &paths, Some("openclaw")).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.agent, "openclaw");
        assert!(report.restarted);
    }

    #[test]
    fn restart_error_path_reports_missing_systemctl() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let err = restart(&ctx(&runner), &paths, Some("openclaw")).unwrap_err();
        assert!(matches!(err, NczError::MissingDep(_)));
    }
}
