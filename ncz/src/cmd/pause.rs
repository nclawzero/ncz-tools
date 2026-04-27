//! pause — TODO(codex): port from bash handler in
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/pause.sh

use crate::cli::Context;
use crate::error::NczError;

pub fn run(_ctx: &Context, _agent: Option<&str>) -> Result<i32, NczError> {
    Err(NczError::Precondition("pause: not yet implemented".into()))
}
