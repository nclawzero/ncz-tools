//! channel — TODO(codex): port from
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/channel.sh

use crate::cli::Context;
use crate::error::NczError;

pub fn run(_ctx: &Context, _channel: Option<&str>) -> Result<i32, NczError> {
    Err(NczError::Precondition(
        "channel: not yet implemented".into(),
    ))
}
