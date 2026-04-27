//! health — TODO(codex): port from bash handler in
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/health.sh

use crate::cli::Context;
use crate::error::NczError;

pub fn run(_ctx: &Context) -> Result<i32, NczError> {
    Err(NczError::Precondition("health: not yet implemented".into()))
}
