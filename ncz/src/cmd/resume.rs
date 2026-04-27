//! resume — start the active or named agent service.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{agent, Paths};
use crate::sys::systemd;

#[derive(Debug, Serialize)]
pub struct ResumeReport {
    pub schema_version: u32,
    pub agent: String,
    pub service: String,
    pub resumed: bool,
}

impl Render for ResumeReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Resumed {}.", self.agent)
    }
}

pub fn run(ctx: &Context, agent: Option<&str>) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = resume(ctx, &paths, agent)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn resume(
    ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
) -> Result<ResumeReport, NczError> {
    common::require_tool(ctx.runner, "systemctl", &["--version"])?;
    let agent = common::resolve_agent(paths, requested_agent)?;
    let service = agent::service_for(&agent);
    systemd::start(ctx.runner, &service)?;
    Ok(ResumeReport {
        schema_version: common::SCHEMA_VERSION,
        agent,
        service,
        resumed: true,
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
    fn resume_happy_path_starts_named_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );

        let report = resume(&ctx(&runner), &paths, Some("zeroclaw")).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.agent, "zeroclaw");
        assert!(report.resumed);
    }

    #[test]
    fn resume_error_path_propagates_start_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(1, "", "failed\n"),
        );

        let err = resume(&ctx(&runner), &paths, Some("zeroclaw")).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
    }
}
