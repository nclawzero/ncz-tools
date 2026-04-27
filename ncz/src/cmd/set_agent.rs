//! set_agent — TODO(codex): port from bash handler in
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/set-agent.sh

use crate::cli::Context;
use crate::error::NczError;

pub fn run(_ctx: &Context, _agent: &str) -> Result<i32, NczError> {
    Err(NczError::Precondition("set-agent: not yet implemented".into()))
}
