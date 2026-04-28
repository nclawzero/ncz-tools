//! set-agent — switch the active nclawzero agent runtime.

use std::fs;
use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent, quadlet, Paths};
use crate::sys::{podman, systemd, CommandRunner};

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
    let previous_state = agent_state_snapshot(paths)?;
    let running = running_agents_checked(ctx.runner)?;
    if current == target && running.len() == 1 && running[0] == target {
        ensure_agent_ready(ctx, target, health_timeout_secs)?;
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
    if image.is_empty() || !podman::image_exists(ctx.runner, &image)? {
        return Err(NczError::Precondition(format!(
            "container image for {target} is missing ({}); run 'ncz update' first",
            if image.is_empty() { "unknown" } else { &image }
        )));
    }

    systemd::daemon_reload(ctx.runner)?;

    // Quadlet-generated services live under /run/systemd/generator/ and are
    // marked transient; `systemctl enable` rejects them. Boot-time persistence
    // is owned by the [Install] section in each `.container` file (only
    // zeroclaw.container ships with [Install]). set-agent therefore only
    // toggles runtime via stop/start and does not touch enable/disable.
    let mut stopped_previously_running = false;
    for agent_name in agent::AGENTS {
        if *agent_name != target {
            let unit = agent::service_for(agent_name);
            if let Err(err) = systemd::stop(ctx.runner, &unit) {
                let failed_agent_was_running =
                    running.iter().any(|name| name.as_str() == *agent_name);
                if !failed_agent_was_running {
                    match systemd::is_stopped(ctx.runner, &unit) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(_) if !stopped_previously_running => return Err(err),
                        Err(_) => {}
                    }
                }
                if stopped_previously_running || failed_agent_was_running {
                    return Err(recover_after_stop_loop_failure(
                        ctx,
                        &running,
                        health_timeout_secs,
                        err,
                    ));
                }
                return Err(err);
            }
            if running.iter().any(|name| name.as_str() == *agent_name) {
                stopped_previously_running = true;
            }
        }
    }

    let service = agent::service_for(target);
    if let Err(err) = systemd::start(ctx.runner, &service) {
        return Err(recover_after_switch_failure(
            ctx,
            paths,
            target,
            &service,
            &running,
            health_timeout_secs,
            err,
            None,
        ));
    }

    if let Err(err) = ensure_agent_ready(ctx, target, health_timeout_secs) {
        return Err(recover_after_switch_failure(
            ctx,
            paths,
            target,
            &service,
            &running,
            health_timeout_secs,
            err,
            None,
        ));
    }
    let expected_running = vec![target.to_string()];
    if let Err(err) = require_running_agents(ctx.runner, &expected_running) {
        return Err(recover_after_switch_failure(
            ctx,
            paths,
            target,
            &service,
            &running,
            health_timeout_secs,
            err,
            None,
        ));
    }

    if let Err(err) = agent::write(paths, target) {
        return Err(recover_after_switch_failure(
            ctx,
            paths,
            target,
            &service,
            &running,
            health_timeout_secs,
            err,
            Some(&previous_state),
        ));
    }

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

enum AgentStateSnapshot {
    Present(Vec<u8>),
    Missing,
}

fn agent_state_snapshot(paths: &Paths) -> Result<AgentStateSnapshot, NczError> {
    match fs::read(paths.agent_state()) {
        Ok(contents) => Ok(AgentStateSnapshot::Present(contents)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(AgentStateSnapshot::Missing),
        Err(e) => Err(NczError::Io(e)),
    }
}

fn restore_agent_state(paths: &Paths, snapshot: &AgentStateSnapshot) -> Result<(), NczError> {
    match snapshot {
        AgentStateSnapshot::Present(contents) => {
            state::atomic_write(&paths.agent_state(), contents, 0o644)
        }
        AgentStateSnapshot::Missing => state::remove_file_durable(&paths.agent_state()),
    }
}

fn ensure_agent_ready(
    ctx: &Context,
    target: &str,
    health_timeout_secs: u64,
) -> Result<(), NczError> {
    let service = agent::service_for(target);
    if !systemd::is_active_checked(ctx.runner, &service)? {
        return Err(NczError::Precondition(format!("{target} is not active")));
    }

    let port = agent::port_for(target)
        .ok_or_else(|| NczError::Usage(format!("unknown agent: {target}")))?;
    if !common::probe_local_health(ctx.runner, port, health_timeout_secs)? {
        return Err(NczError::Precondition(format!(
            "health probe failed for {target}"
        )));
    }
    Ok(())
}

fn running_agents_checked(runner: &dyn CommandRunner) -> Result<Vec<String>, NczError> {
    let mut running = Vec::new();
    for agent_name in agent::AGENTS {
        let unit = agent::service_for(agent_name);
        if systemd::is_active_checked(runner, &unit)? {
            running.push((*agent_name).to_string());
        }
    }
    Ok(running)
}

fn require_running_agents(runner: &dyn CommandRunner, expected: &[String]) -> Result<(), NczError> {
    let mut observed = Vec::new();
    let mut state_errors = Vec::new();
    for agent_name in agent::AGENTS {
        let unit = agent::service_for(agent_name);
        let state = systemd::unit_state(runner, &unit)?;
        let should_run = expected
            .iter()
            .any(|expected_agent| expected_agent.as_str() == *agent_name);

        if state.is_active() {
            observed.push((*agent_name).to_string());
            if !should_run {
                state_errors.push(format!("{unit} active but not expected"));
            }
        } else if state.is_stopped() {
            if should_run {
                state_errors.push(format!("{unit} not active; {}", state.describe()));
            }
        } else {
            state_errors.push(format!("{unit} not terminal; {}", state.describe()));
        }
    }

    if observed == expected && state_errors.is_empty() {
        return Ok(());
    }

    let mut msg = format!(
        "expected active agents: {}; observed active agents: {}",
        format_agent_list(expected),
        format_agent_list(&observed)
    );
    if !state_errors.is_empty() {
        msg.push_str("; ");
        msg.push_str(&state_errors.join("; "));
    }

    Err(NczError::Inconsistent(msg))
}

fn format_agent_list(agents: &[String]) -> String {
    if agents.is_empty() {
        "none".to_string()
    } else {
        agents.join(", ")
    }
}

struct RollbackOutcome {
    restored_agents: Vec<String>,
    stopped_target: bool,
    stop_warnings: Vec<String>,
}

fn recover_after_switch_failure(
    ctx: &Context,
    paths: &Paths,
    target: &str,
    target_service: &str,
    running_before: &[String],
    health_timeout_secs: u64,
    err: NczError,
    state_snapshot: Option<&AgentStateSnapshot>,
) -> NczError {
    let original = err.to_string();
    let runtime_result = rollback_runtime(
        ctx,
        target,
        target_service,
        running_before,
        health_timeout_secs,
    );

    match runtime_result {
        Ok(outcome) => {
            let state_result = match state_snapshot {
                Some(snapshot) => restore_agent_state(paths, snapshot).map(|_| true),
                None => Ok(false),
            };
            let restored_state = match state_result {
                Ok(restored_state) => restored_state,
                Err(err) => {
                    return NczError::Inconsistent(format!(
                        "{original}; recovery failed: state restore failed: {err}"
                    ));
                }
            };
            let mut context = Vec::new();
            if restored_state {
                context.push("restored previous agent state".to_string());
            }
            context.push(rollback_success_context(outcome));
            with_recovery_context(err, context.join(" and "))
        }
        Err(err) => {
            let state_context = match state_snapshot {
                Some(snapshot) => match restore_agent_state(paths, snapshot) {
                    Ok(()) => "; restored previous agent state".to_string(),
                    Err(state_err) => format!("; state restore failed: {state_err}"),
                },
                None => String::new(),
            };
            NczError::Inconsistent(format!("{original}; recovery failed: {err}{state_context}"))
        }
    }
}

fn rollback_runtime(
    ctx: &Context,
    target: &str,
    target_service: &str,
    running_before: &[String],
    health_timeout_secs: u64,
) -> Result<RollbackOutcome, NczError> {
    let mut stop_warnings = Vec::new();
    let mut stopped_target = false;
    for agent_name in agent::AGENTS {
        if running_before
            .iter()
            .any(|running_agent| running_agent.as_str() == *agent_name)
        {
            continue;
        }
        let service = agent::service_for(agent_name);
        match systemd::stop(ctx.runner, &service) {
            Ok(()) => {
                if service == target_service && *agent_name == target {
                    stopped_target = true;
                }
            }
            Err(err) => stop_warnings.push(format!("stop {service}: {err}")),
        }
    }

    let mut failures = Vec::new();
    let restored_agents = match restore_running_agents(ctx, running_before, health_timeout_secs) {
        Ok(restored_agents) => restored_agents,
        Err(err) => {
            failures.push(err.to_string());
            Vec::new()
        }
    };

    if failures.is_empty() {
        if let Err(err) = require_running_agents(ctx.runner, running_before) {
            failures.push(format!("verify rollback active set: {err}"));
        }
    }

    if failures.is_empty() {
        Ok(RollbackOutcome {
            restored_agents,
            stopped_target,
            stop_warnings,
        })
    } else {
        failures.extend(stop_warnings);
        Err(NczError::Inconsistent(format!(
            "runtime rollback failed: {}",
            failures.join("; ")
        )))
    }
}

fn recover_after_stop_loop_failure(
    ctx: &Context,
    running_before: &[String],
    health_timeout_secs: u64,
    err: NczError,
) -> NczError {
    let original = err.to_string();
    match restore_running_agents(ctx, running_before, health_timeout_secs) {
        Ok(restored_agents) => match require_running_agents(ctx.runner, running_before) {
            Ok(()) => with_recovery_context(
                err,
                format!(
                    "restored pre-command running agents: {}",
                    restored_agents.join(", ")
                ),
            ),
            Err(recovery_err) => NczError::Inconsistent(format!(
                "{original}; failed to verify pre-command running agents after stop failure: {recovery_err}"
            )),
        },
        Err(recovery_err) => NczError::Inconsistent(format!(
            "{original}; failed to restore pre-command running agents after stop failure: {recovery_err}"
        )),
    }
}

fn restore_running_agents(
    ctx: &Context,
    running_before: &[String],
    health_timeout_secs: u64,
) -> Result<Vec<String>, NczError> {
    let mut failures = Vec::new();
    let mut restored_agents = Vec::new();
    for agent_name in running_before {
        let service = agent::service_for(agent_name);
        match systemd::start(ctx.runner, &service) {
            Ok(()) => match ensure_agent_ready(ctx, agent_name, health_timeout_secs) {
                Ok(()) => restored_agents.push(agent_name.clone()),
                Err(err) => failures.push(format!("verify {service}: {err}")),
            },
            Err(err) => failures.push(format!("start {service}: {err}")),
        }
    }

    if failures.is_empty() {
        Ok(restored_agents)
    } else {
        Err(NczError::Inconsistent(format!(
            "restore running agents failed: {}",
            failures.join("; ")
        )))
    }
}

fn rollback_success_context(outcome: RollbackOutcome) -> String {
    let mut context = Vec::new();
    if outcome.stopped_target {
        context.push("stopped failed target runtime".to_string());
    }
    if !outcome.restored_agents.is_empty() {
        context.push(format!(
            "restored pre-command running agents: {}",
            outcome.restored_agents.join(", ")
        ));
    }
    if !outcome.stop_warnings.is_empty() {
        context.push(format!(
            "verified rollback active set after stop warnings: {}",
            outcome.stop_warnings.join("; ")
        ));
    }
    if context.is_empty() {
        "restored pre-command runtime state".to_string()
    } else {
        context.join(" and ")
    }
}

fn with_recovery_context(err: NczError, context: String) -> NczError {
    match err {
        NczError::Precondition(msg) => NczError::Precondition(format!("{msg}; {context}")),
        NczError::Inconsistent(msg) => NczError::Inconsistent(format!("{msg}; {context}")),
        NczError::Exec { cmd, msg } => NczError::Exec {
            cmd,
            msg: format!("{msg}; {context}"),
        },
        NczError::Io(err) => NczError::Io(io::Error::new(err.kind(), format!("{err}; {context}"))),
        other => NczError::Inconsistent(format!("{other}; {context}")),
    }
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

    fn expect_stop_success(runner: &FakeRunner, unit: &str) {
        expect_unit_state(runner, unit, "loaded", "active", "running");
        runner.expect("sudo", &["systemctl", "stop", unit], out(0, "", ""));
        expect_unit_state(runner, unit, "loaded", "inactive", "dead");
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

    fn expect_no_running_probe(runner: &FakeRunner) {
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(3, "", ""),
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
    }

    fn expect_zeroclaw_and_openclaw_running_probe(runner: &FakeRunner) {
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
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

    fn expect_active_set_verification(runner: &FakeRunner, active_agents: &[&str]) {
        expect_active_set_verification_with_failed(runner, active_agents, &[]);
    }

    fn expect_active_set_verification_with_failed(
        runner: &FakeRunner,
        active_agents: &[&str],
        failed_agents: &[&str],
    ) {
        for agent_name in agent::AGENTS {
            let unit = agent::service_for(agent_name);
            if active_agents.contains(agent_name) {
                expect_unit_state(runner, &unit, "loaded", "active", "running");
            } else if failed_agents.contains(agent_name) {
                expect_unit_state(runner, &unit, "loaded", "failed", "failed");
            } else {
                expect_unit_state(runner, &unit, "loaded", "inactive", "dead");
            }
        }
    }

    #[test]
    fn recovery_context_preserves_io_exit_code() {
        let err = with_recovery_context(
            NczError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "permission denied",
            )),
            "restored previous agent state".to_string(),
        );

        assert_eq!(err.exit_code(), 2);
        assert!(matches!(err, NczError::Io(_)));
        assert!(err.to_string().contains("restored previous agent state"));
    }

    #[test]
    fn write_error_restores_state_when_runtime_rollback_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "zeroclaw\n").unwrap();

        let runner = FakeRunner::new();
        expect_unit_state(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(1, "", "stuck running"),
        );
        expect_unit_state(&runner, "zeroclaw.service", "loaded", "active", "running");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["zeroclaw", "openclaw"]);

        let err = recover_after_switch_failure(
            &ctx(&runner),
            &paths,
            "zeroclaw",
            "zeroclaw.service",
            &["openclaw".to_string()],
            1,
            NczError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "directory fsync failed",
            )),
            Some(&AgentStateSnapshot::Present(b"openclaw\n".to_vec())),
        );

        assert!(matches!(err, NczError::Inconsistent(_)));
        assert!(err.to_string().contains("stop zeroclaw.service"));
        assert!(err.to_string().contains("restored previous agent state"));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }

    #[test]
    fn write_error_restores_state_when_previous_restart_fails_during_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "zeroclaw\n").unwrap();

        let runner = FakeRunner::new();
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(1, "", "start failed"),
        );

        let err = recover_after_switch_failure(
            &ctx(&runner),
            &paths,
            "zeroclaw",
            "zeroclaw.service",
            &["openclaw".to_string()],
            1,
            NczError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "directory fsync failed",
            )),
            Some(&AgentStateSnapshot::Present(b"openclaw\n".to_vec())),
        );

        assert!(matches!(err, NczError::Inconsistent(_)));
        assert!(err.to_string().contains("start openclaw.service"));
        assert!(err.to_string().contains("restored previous agent state"));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }

    #[test]
    fn write_error_restores_state_when_stop_warning_still_reaches_expected_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "zeroclaw\n").unwrap();

        let runner = FakeRunner::new();
        expect_unit_state(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(1, "", "stop reported failure after stopping"),
        );
        expect_unit_state(&runner, "zeroclaw.service", "loaded", "inactive", "dead");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = recover_after_switch_failure(
            &ctx(&runner),
            &paths,
            "zeroclaw",
            "zeroclaw.service",
            &["openclaw".to_string()],
            1,
            NczError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "directory fsync failed",
            )),
            Some(&AgentStateSnapshot::Present(b"openclaw\n".to_vec())),
        );

        assert!(matches!(err, NczError::Io(_)));
        assert!(err.to_string().contains("restored previous agent state"));
        assert!(err.to_string().contains("verified rollback active set"));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_active_set_verification(&runner, &["zeroclaw"]);

        let report = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.previous_agent, "openclaw");
        assert_eq!(agent::read(&paths).unwrap(), "zeroclaw");
    }

    #[test]
    fn set_agent_allows_stale_failed_non_target() {
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
        expect_stop_success(&runner, "openclaw.service");
        expect_unit_state(&runner, "hermes.service", "loaded", "failed", "failed");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "hermes.service"],
            out(0, "", ""),
        );
        expect_unit_state(&runner, "hermes.service", "loaded", "failed", "failed");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_active_set_verification_with_failed(&runner, &["zeroclaw"], &["hermes"]);

        let report = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap();
        assert_eq!(report.agent, "zeroclaw");
        assert_eq!(agent::read(&paths).unwrap(), "zeroclaw");
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_rejects_transitional_non_running_non_target() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");

        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        expect_no_running_probe(&runner);
        runner.expect(
            "podman",
            &["image", "exists", "localhost/zeroclaw:latest"],
            out(0, "", ""),
        );
        runner.expect("sudo", &["systemctl", "daemon-reload"], out(0, "", ""));
        expect_unit_state(&runner, "openclaw.service", "loaded", "inactive", "dead");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "openclaw.service"],
            out(1, "", "job still running"),
        );
        expect_unit_state(
            &runner,
            "openclaw.service",
            "loaded",
            "deactivating",
            "stop-sigterm",
        );
        expect_unit_state(
            &runner,
            "openclaw.service",
            "loaded",
            "deactivating",
            "stop-sigterm",
        );

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        assert_eq!(agent::read(&paths).unwrap(), "openclaw");
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "sudo systemctl start zeroclaw.service"));
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "sudo systemctl stop hermes.service"));
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_rejects_extra_agent_after_successful_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_active_set_verification(&runner, &["zeroclaw", "hermes"]);
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Inconsistent(_)));
        let msg = err.to_string();
        assert!(msg.contains("restored pre-command running agents: openclaw"));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_reports_target_still_active_after_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_active_set_verification(&runner, &["zeroclaw", "openclaw"]);
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["zeroclaw", "openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Inconsistent(_)));
        assert!(err.to_string().contains("verify rollback active set"));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
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

    #[test]
    fn set_agent_already_active_requires_healthy_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();

        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        expect_running_probe(&runner);
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 500);

        let err = switch_agent(&ctx(&runner), &paths, "openclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Precondition(_)));
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("sudo systemctl ")));
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_aborts_on_running_probe_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();

        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(3, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(1, "", "Failed to connect to bus."),
        );

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("sudo systemctl ")));
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_rejects_failed_stop() {
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
        expect_unit_state(&runner, "openclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "openclaw.service"],
            out(1, "", "operation failed"),
        );
        expect_unit_state(&runner, "openclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        assert_eq!(agent::read(&paths).unwrap(), "openclaw");
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "sudo systemctl start zeroclaw.service"));
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_restores_current_after_later_stop_failure() {
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
        expect_stop_success(&runner, "openclaw.service");
        expect_unit_state(&runner, "hermes.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "hermes.service"],
            out(1, "", "operation failed"),
        );
        expect_unit_state(&runner, "hermes.service", "loaded", "active", "running");
        expect_unit_state(&runner, "hermes.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        assert_eq!(agent::read(&paths).unwrap(), "openclaw");
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "sudo systemctl start zeroclaw.service"));
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_restores_current_after_target_start_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(1, "", "start failed"),
        );
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        assert_eq!(agent::read(&paths).unwrap(), "openclaw");
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_does_not_unpause_current_after_target_start_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");

        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        expect_no_running_probe(&runner);
        runner.expect(
            "podman",
            &["image", "exists", "localhost/zeroclaw:latest"],
            out(0, "", ""),
        );
        runner.expect("sudo", &["systemctl", "daemon-reload"], out(0, "", ""));
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(1, "", "start failed"),
        );
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        expect_active_set_verification(&runner, &[]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        assert_eq!(agent::read(&paths).unwrap(), "openclaw");
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "sudo systemctl start openclaw.service"));
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_keeps_state_after_active_set_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_active_set_verification(&runner, &[]);
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Inconsistent(_)));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_rejects_activating_non_target_after_successful_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_unit_state(&runner, "zeroclaw.service", "loaded", "active", "running");
        expect_unit_state(
            &runner,
            "openclaw.service",
            "loaded",
            "activating",
            "start-post",
        );
        expect_unit_state(&runner, "hermes.service", "loaded", "inactive", "dead");
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Inconsistent(_)));
        assert!(err.to_string().contains("openclaw.service not terminal"));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_reports_post_start_probe_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(1, "", "Failed to connect to bus."),
        );
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_reports_final_active_probe_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        runner.expect(
            "systemctl",
            &[
                "show",
                "zeroclaw.service",
                "--property=LoadState",
                "--property=ActiveState",
                "--property=SubState",
            ],
            out(1, "", "Failed to connect to bus."),
        );
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_removes_state_created_after_missing_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");

        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        expect_running_probe(&runner);
        runner.expect(
            "podman",
            &["image", "exists", "localhost/openclaw:latest"],
            out(0, "", ""),
        );
        runner.expect("sudo", &["systemctl", "daemon-reload"], out(0, "", ""));
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &[]);
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "openclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Inconsistent(_)));
        assert!(!paths.agent_state().exists());
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_preserves_same_agent_reconcile_runtime_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "zeroclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");

        let runner = FakeRunner::new();
        runner.expect("systemctl", &["--version"], out(0, "systemd 255\n", ""));
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        expect_zeroclaw_and_openclaw_running_probe(&runner);
        runner.expect(
            "podman",
            &["image", "exists", "localhost/zeroclaw:latest"],
            out(0, "", ""),
        );
        runner.expect("sudo", &["systemctl", "daemon-reload"], out(0, "", ""));
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_active_set_verification(&runner, &[]);
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["zeroclaw", "openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Inconsistent(_)));
        assert_eq!(agent::read(&paths).unwrap(), "zeroclaw");
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "sudo systemctl stop zeroclaw.service"));
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_reports_failed_target_stop_during_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_active_set_verification(&runner, &[]);
        expect_unit_state(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(1, "", "stuck running"),
        );
        expect_unit_state(&runner, "zeroclaw.service", "loaded", "active", "running");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(18789, "/health", 200);
        expect_active_set_verification(&runner, &["zeroclaw", "openclaw"]);

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Inconsistent(_)));
        assert!(err.to_string().contains("stop zeroclaw.service"));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }

    #[test]
    fn set_agent_error_path_reports_failed_previous_start_during_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_state(), "openclaw\n").unwrap();
        write_quadlet(&paths, "zeroclaw", "localhost/zeroclaw:latest");
        write_quadlet(&paths, "openclaw", "localhost/openclaw:latest");

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
        expect_stop_success(&runner, "openclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect_http(42617, "/health", 200);
        expect_active_set_verification(&runner, &[]);
        expect_stop_success(&runner, "zeroclaw.service");
        expect_stop_success(&runner, "hermes.service");
        runner.expect(
            "sudo",
            &["systemctl", "start", "openclaw.service"],
            out(1, "", "start failed"),
        );

        let err = switch_agent(&ctx(&runner), &paths, "zeroclaw", 1).unwrap_err();
        assert!(matches!(err, NczError::Inconsistent(_)));
        assert!(err.to_string().contains("start openclaw.service"));
        assert_eq!(
            fs::read_to_string(paths.agent_state()).unwrap(),
            "openclaw\n"
        );
        runner.assert_done();
    }
}
