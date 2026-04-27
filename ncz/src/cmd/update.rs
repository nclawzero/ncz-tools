//! update — TODO(codex): port from
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/update.sh

use crate::cli::Context;
use crate::error::NczError;

pub fn run(_ctx: &Context, _check: bool) -> Result<i32, NczError> {
    Err(NczError::Precondition("update: not yet implemented".into()))
}
