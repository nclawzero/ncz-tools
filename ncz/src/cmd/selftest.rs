//! selftest — TODO(codex): port from bash handler in
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/selftest.sh

use crate::cli::Context;
use crate::error::NczError;

pub fn run(_ctx: &Context) -> Result<i32, NczError> {
    Err(NczError::Precondition(
        "selftest: not yet implemented".into(),
    ))
}
