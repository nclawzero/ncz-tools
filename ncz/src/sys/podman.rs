//! Podman wrappers. Parse JSON outputs only — never table form.

use crate::{error::NczError, sys::CommandRunner};

pub fn image_exists(runner: &dyn CommandRunner, image: &str) -> Result<bool, NczError> {
    let out = runner.run("podman", &["image", "exists", image])?;
    Ok(out.ok())
}

pub fn image_pull(runner: &dyn CommandRunner, image: &str) -> Result<(), NczError> {
    let out = runner.run("podman", &["image", "pull", image])?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("podman image pull {image}"),
            msg: out.stderr,
        });
    }
    Ok(())
}
