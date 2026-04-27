//! set-agent — switch the active nclawzero agent runtime.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent, quadlet, Paths};
use crate::sys::{podman, systemd};

#[derive(Debug, Serialize)]
pub struct SetAgentReport {
    pub schema_version: u32,
    pub agent: String,
    pub previous_agent: String,
    pub service: String,
    pub image: String,
    pub health_url: String,
    pub already_active: bool,
    pub reconciled: bool,
}

impl Render for SetAgentReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.already_active {
            writeln!(w, "Already {}.", self.agent)
        } else {
            if self.reconciled {
                writeln!(w, "Reconciling active agent {}.", self.agent)?;
            }
            writeln!(w, "Active agent: {}", self.agent)
        }
    }
}

pub fn run(ctx: &Context, target: &str) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = switch_agent(ctx, &paths, target, 30)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn switch_agent(
    ctx: &Context,
    paths: &Paths,
    target: &str,
    health_timeout_secs: u64,
) -> Result<SetAgentReport, NczError> {
    common::validate_agent(target)?;
    common::require_tool(ctx.runner, "systemctl", &["--version"])?;
    common::require_tool(ctx.runner, "podman", &["--version"])?;

    let _lock = state::acquire_lock(&paths.lock_path)?;
    let current = agent::read(paths)?;
    let running = common::running_agents(ctx.runner);
    if current == target && running.len() == 1 && running[0] == target {
        return Ok(SetAgentReport {
            schema_version: common::SCHEMA_VERSION,
            agent: target.to_string(),
            previous_agent: current,
            service: agent::service_for(target),
            image: String::new(),
            health_url: health_url(target)?,
            already_active: true,
            reconciled: false,
        });
    }

    let target_quadlet = paths.agent_quadlet(target);
    if !target_quadlet.is_file() || target_quadlet.metadata()?.len() == 0 {
        return Err(NczError::Precondition(format!(
            "missing quadlet for {target}: {}",
            target_quadlet.display()
        )));
    }

    let image = quadlet::image_for(&target_quadlet)?.unwrap_or_default();
    if !podman::image_exists(ctx.runner, &image)? {
        return Err(NczError::Precondition(format!(
            "container image for {target} is missing ({}); run 'ncz update' first",
            if image.is_empty() { "unknown" } else { &image }
        )));
    }

    systemd::daemon_reload(ctx.runner)?;

    for agent_name in agent::AGENTS {
        if *agent_name != target {
            let unit = agent::service_for(agent_name);
            let _ = systemd::stop(ctx.runner, &unit);
            let _ = systemd::disable(ctx.runner, &unit);
        }
    }

    let service = agent::service_for(target);
    systemd::enable(ctx.runner, &service)?;
    systemd::start(ctx.runner, &service)?;

    let port = agent::port_for(target)
        .ok_or_else(|| NczError::Usage(format!("unknown agent: {target}")))?;
    if !common::probe_local_health(ctx.runner, port, health_timeout_secs)? {
        let _ = systemd::stop(ctx.runner, &service);
        let _ = systemd::disable(ctx.runner, &service);
        if agent::AGENTS.contains(&current.as_str())
            && current != target
            && paths.agent_quadlet(&current).is_file()
        {
            let current_service = agent::service_for(&current);
            let _ = systemd::enable(ctx.runner, &current_service);
            let _ = systemd::start(ctx.runner, &current_service);
        }
        return Err(NczError::Precondition(format!(
            "health probe failed for {target}; rolled back to {current}"
        )));
    }

    agent::write(paths, target)?;
    Ok(SetAgentReport {
        schema_version: common::SCHEMA_VERSION,
        agent: target.to_string(),
        previous_agent: current.clone(),
        service,
        image,
        health_url: health_url(target)?,
        already_active: false,
        reconciled: current == target,
    })
}

fn health_url(agent_name: &str) -> Result<String, NczError> {
    let port = agent::port_for(agent_name)
        .ok_or_else(|| NczError::Usage(format!("unknown agent: {agent_name}")))?;
    Ok(format!("http://127.0.0.1:{port}/health"))
}

#[cfg(test)]
mod tests {
    use std::fs;

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

    fn write_quadlet(paths: &Paths, agent_name: &str, image: &str) {
        fs::create_dir_all(&paths.quadlet_dir).unwrap();
        fs::write(
            paths.agent_quadlet(agent_name),
            format!("[Container]\nImage={image}\n"),
        )
        .unwrap();
    }

    fn expect_running_probe(runner: &FakeRunner) {
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(3, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "hermes.service"],
            out(3, "", ""),
        );
    }

    #[test]
    fn set_agent_happy_path_switches_and_writes_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");

        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        expect_running_probe(&runner);
        runner.expect(
            "podman",
            &["image", "exists", "localhost/zeroclaw:latest"],
            out(0, "", ""),
        );
        runner.expect("sudo", &["systemctl", "daemon-reload"], out(0, "", ""));
        runner.expect(
            "sudo",
            &["systemctl", "stop", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "sudo",
            &["systemctl", "disable", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "sudo",
            &["systemctl", "stop", "hermes.service"],
            out(0, "", ""),
        );
        runner.expect(
            "sudo",
            &["systemctl", "disable", "hermes.service"],
            out(0, "", ""),
        );
        runner.expect(
            "sudo",
            &["systemctl", "enable", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);

        let report = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.previous_agent, "openclaw");
        assert_eq!(agent::read(&paths).unwrap(), "zeroclaw");
    }

    #[test]
    fn set_agent_error_path_rejects_missing_quadlet() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();

        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        expect_running_probe(&runner);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Precondition(_)));
    }
}
