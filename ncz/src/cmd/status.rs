//! status — TODO(codex): port from bash handler in
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/status.sh

use crate::cli::Context;
use crate::error::NczError;

pub fn run(_ctx: &Context) -> Result<i32, NczError> {
    Err(NczError::Precondition("status: not yet implemented".into()))
}
