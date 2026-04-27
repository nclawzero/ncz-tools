//! apt/apt-get wrappers. The bash version always passes
//! `DEBIAN_FRONTEND=noninteractive`; mirror that.

use crate::{error::NczError, sys::CommandRunner};

pub fn update(runner: &dyn CommandRunner) -> Result<(), NczError> {
    let out = runner.run("sudo", &["apt-get", "update"])?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: "apt-get update".into(),
            msg: out.stderr,
        });
    }
    Ok(())
}
