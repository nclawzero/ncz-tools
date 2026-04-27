//! sandbox — TODO(codex): port from
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/sandbox.sh

use crate::cli::{Context, SandboxAction};
use crate::error::NczError;

pub fn run(_ctx: &Context, action: Option<SandboxAction>) -> Result<i32, NczError> {
    match action {
        None => Err(NczError::Precondition(
            "sandbox: not yet implemented".into(),
        )),
        Some(SandboxAction::Policy { agent: _ }) => Err(NczError::Precondition(
            "sandbox policy: not yet implemented".into(),
        )),
    }
}
