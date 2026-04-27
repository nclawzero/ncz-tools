//! systemctl + journalctl wrappers. Use machine-readable outputs only:
//! `is-active`, `is-enabled`, `show --property=...`, `journalctl -o json`.
//! Never parse human `systemctl status` text.

use crate::{error::NczError, sys::CommandRunner};

pub fn is_active(runner: &dyn CommandRunner, unit: &str) -> Result<bool, NczError> {
    let out = runner.run("systemctl", &["is-active", "--quiet", unit])?;
    Ok(out.ok())
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

pub fn stop(runner: &dyn CommandRunner, unit: &str) -> Result<(), NczError> {
    let _ = runner.run("sudo", &["systemctl", "stop", unit])?;
    Ok(())
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
