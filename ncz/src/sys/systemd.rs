//! systemctl + journalctl wrappers. Use machine-readable outputs only:
//! `is-active`, `is-enabled`, `show --property=...`, `journalctl -o json`.
//! Never parse human `systemctl status` text.

use crate::{error::NczError, sys::CommandRunner};

pub fn is_active(runner: &dyn CommandRunner, unit: &str) -> Result<bool, NczError> {
    let out = runner.run("systemctl", &["is-active", "--quiet", unit])?;
    Ok(out.ok())
}

pub fn is_active_checked(runner: &dyn CommandRunner, unit: &str) -> Result<bool, NczError> {
    let out = runner.run("systemctl", &["is-active", "--quiet", unit])?;
    match out.status {
        0 => Ok(true),
        3 | 4 => Ok(false),
        _ => Err(NczError::Exec {
            cmd: format!("systemctl is-active {unit}"),
            msg: if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            },
        }),
    }
}

pub fn is_enabled(runner: &dyn CommandRunner, unit: &str) -> Result<bool, NczError> {
    let out = runner.run("systemctl", &["is-enabled", "--quiet", unit])?;
    Ok(out.ok())
}

pub fn daemon_reload(runner: &dyn CommandRunner) -> Result<(), NczError> {
    let out = runner.run("sudo", &["systemctl", "daemon-reload"])?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: "systemctl daemon-reload".into(),
            msg: out.stderr,
        });
    }
    Ok(())
}

pub fn start(runner: &dyn CommandRunner, unit: &str) -> Result<(), NczError> {
    let out = runner.run("sudo", &["systemctl", "start", unit])?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("systemctl start {unit}"),
            msg: out.stderr,
        });
    }
    Ok(())
}

pub fn restart(runner: &dyn CommandRunner, unit: &str) -> Result<(), NczError> {
    let out = runner.run("sudo", &["systemctl", "restart", unit])?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("systemctl restart {unit}"),
            msg: out.stderr,
        });
    }
    Ok(())
}

pub fn stop(runner: &dyn CommandRunner, unit: &str) -> Result<(), NczError> {
    let before = stop_state(runner, unit).ok();
    let out = runner.run("sudo", &["systemctl", "stop", unit])?;
    let stop_msg = output_msg(&out);
    let was_stopped = before.as_ref().is_some_and(StopState::is_stopped);
    match stop_state(runner, unit) {
        Ok(state) if out.ok() && state.is_stopped() => Ok(()),
        Ok(state) if was_stopped && state.is_stopped() && is_idempotent_stop_failure(&out) => {
            Ok(())
        }
        Ok(state) => Err(NczError::Exec {
            cmd: format!("systemctl stop {unit}"),
            msg: if out.ok() {
                format!("stop exited 0 but unit is not stopped; {}", state.describe())
            } else {
                format!("{stop_msg}; {}", state.describe())
            },
        }),
        Err(err) if out.ok() => Err(err),
        Err(_) => Err(NczError::Exec {
            cmd: format!("systemctl stop {unit}"),
            msg: stop_msg,
        }),
    }
}

fn is_idempotent_stop_failure(out: &crate::sys::ProcessOutput) -> bool {
    let msg = output_msg(out).to_ascii_lowercase();
    msg.contains("not loaded") || msg.contains("not active") || msg.contains("could not be found")
}

fn output_msg(out: &crate::sys::ProcessOutput) -> String {
    if out.stderr.is_empty() {
        out.stdout.clone()
    } else {
        out.stderr.clone()
    }
}

struct StopState {
    load_state: String,
    active_state: String,
    sub_state: String,
}

impl StopState {
    fn is_stopped(&self) -> bool {
        matches!(self.active_state.as_str(), "inactive" | "failed")
    }

    fn describe(&self) -> String {
        format!(
            "observed LoadState={} ActiveState={} SubState={}",
            self.load_state, self.active_state, self.sub_state
        )
    }
}

fn stop_state(runner: &dyn CommandRunner, unit: &str) -> Result<StopState, NczError> {
    let out = runner.run(
        "systemctl",
        &[
            "show",
            unit,
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
        ],
    )?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("systemctl show {unit}"),
            msg: if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            },
        });
    }

    let mut load_state = String::new();
    let mut active_state = String::new();
    let mut sub_state = String::new();
    for line in out.stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "LoadState" => load_state = value.trim().to_string(),
                "ActiveState" => active_state = value.trim().to_string(),
                "SubState" => sub_state = value.trim().to_string(),
                _ => {}
            }
        }
    }

    Ok(StopState {
        load_state,
        active_state,
        sub_state,
    })
}

pub fn enable(runner: &dyn CommandRunner, unit: &str) -> Result<(), NczError> {
    let out = runner.run("sudo", &["systemctl", "enable", unit])?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("systemctl enable {unit}"),
            msg: out.stderr,
        });
    }
    Ok(())
}

pub fn disable(runner: &dyn CommandRunner, unit: &str) -> Result<(), NczError> {
    let _ = runner.run("sudo", &["systemctl", "disable", unit])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::error::NczError;
    use crate::sys::{fake::FakeRunner, ProcessOutput};

    use super::*;

    fn out(status: i32, stdout: &str, stderr: &str) -> ProcessOutput {
        ProcessOutput {
            status,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    fn show(load_state: &str, active_state: &str, sub_state: &str) -> ProcessOutput {
        out(
            0,
            &format!(
                "LoadState={load_state}\nActiveState={active_state}\nSubState={sub_state}\n"
            ),
            "",
        )
    }

    fn expect_show(
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
            show(load_state, active_state, sub_state),
        );
    }

    #[test]
    fn stop_tolerates_idempotent_not_loaded_failure() {
        let runner = FakeRunner::new();
        expect_show(&runner, "missing.service", "not-found", "inactive", "dead");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "missing.service"],
            out(1, "", "Unit missing.service not loaded.\n"),
        );
        expect_show(&runner, "missing.service", "not-found", "inactive", "dead");

        stop(&runner, "missing.service").unwrap();
        runner.assert_done();
    }

    #[test]
    fn stop_attempts_not_loaded_active_unit() {
        let runner = FakeRunner::new();
        expect_show(&runner, "missing.service", "not-found", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "missing.service"],
            out(0, "", ""),
        );
        expect_show(&runner, "missing.service", "not-found", "inactive", "dead");

        stop(&runner, "missing.service").unwrap();
        runner.assert_done();
    }

    #[test]
    fn stop_reports_active_state_after_zero_stop() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(0, "", ""),
        );
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");

        let err = stop(&runner, "zeroclaw.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }

    #[test]
    fn stop_tolerates_failed_state_after_zero_stop() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(0, "", ""),
        );
        expect_show(&runner, "zeroclaw.service", "loaded", "failed", "failed");

        stop(&runner, "zeroclaw.service").unwrap();
        runner.assert_done();
    }

    #[test]
    fn stop_reports_deactivating_state_after_zero_stop() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(0, "", ""),
        );
        expect_show(
            &runner,
            "zeroclaw.service",
            "loaded",
            "deactivating",
            "stop-sigterm",
        );

        let err = stop(&runner, "zeroclaw.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }

    #[test]
    fn stop_tolerates_idempotent_inactive_failure() {
        let runner = FakeRunner::new();
        expect_show(&runner, "paused.service", "loaded", "inactive", "dead");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "paused.service"],
            out(1, "", "Unit paused.service is not active.\n"),
        );
        expect_show(&runner, "paused.service", "loaded", "inactive", "dead");

        stop(&runner, "paused.service").unwrap();
        runner.assert_done();
    }

    #[test]
    fn stop_reports_non_idempotent_failure() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(1, "", "Failed to stop zeroclaw.service.\n"),
        );
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");

        let err = stop(&runner, "zeroclaw.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }

    #[test]
    fn stop_reports_nonzero_when_running_unit_ends_inactive() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(1, "", "Failed to stop zeroclaw.service.\n"),
        );
        expect_show(&runner, "zeroclaw.service", "loaded", "inactive", "dead");

        let err = stop(&runner, "zeroclaw.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }

    #[test]
    fn stop_reports_sudo_failure_for_already_stopped_unit() {
        let runner = FakeRunner::new();
        expect_show(&runner, "paused.service", "loaded", "inactive", "dead");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "paused.service"],
            out(1, "", "sudo: a password is required\n"),
        );
        expect_show(&runner, "paused.service", "loaded", "inactive", "dead");

        let err = stop(&runner, "paused.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }

    #[test]
    fn stop_reports_non_unit_not_found_failure() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(1, "", "systemctl: command not found\n"),
        );
        runner.expect(
            "systemctl",
            &[
                "show",
                "zeroclaw.service",
                "--property=LoadState",
                "--property=ActiveState",
                "--property=SubState",
            ],
            out(1, "", "systemctl: command not found\n"),
        );

        let err = stop(&runner, "zeroclaw.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }

    #[test]
    fn stop_reports_failed_state_after_nonzero_stop() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(1, "", "Job failed.\n"),
        );
        expect_show(&runner, "zeroclaw.service", "loaded", "failed", "failed");

        let err = stop(&runner, "zeroclaw.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }

    #[test]
    fn stop_reports_deactivating_state_after_nonzero_stop() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(1, "", "Job timed out.\n"),
        );
        expect_show(
            &runner,
            "zeroclaw.service",
            "loaded",
            "deactivating",
            "stop-sigterm",
        );

        let err = stop(&runner, "zeroclaw.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }

    #[test]
    fn is_active_checked_reports_probe_failure() {
        let runner = FakeRunner::new();
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "openclaw.service"],
            out(1, "", "Failed to connect to bus.\n"),
        );

        let err = is_active_checked(&runner, "openclaw.service").unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
    }
    #[test]
    fn is_stopped_reports_deactivating_as_not_stopped() {
        let runner = FakeRunner::new();
        expect_show(
            &runner,
            "zeroclaw.service",
            "loaded",
            "deactivating",
            "stop-sigterm",
        );

        assert!(!is_stopped(&runner, "zeroclaw.service").unwrap());
        runner.assert_done();
    }

    #[test]
    fn is_stopped_reports_failed_as_stopped() {
        let runner = FakeRunner::new();
        expect_show(&runner, "zeroclaw.service", "loaded", "failed", "failed");

        assert!(is_stopped(&runner, "zeroclaw.service").unwrap());
        runner.assert_done();
    }
}
