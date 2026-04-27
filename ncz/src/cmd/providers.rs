//! providers — TODO(codex): port from
//! pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/files/usr/local/lib/ncz/providers.sh

use crate::cli::{Context, ProvidersAction};
use crate::error::NczError;

pub fn run(_ctx: &Context, action: ProvidersAction) -> Result<i32, NczError> {
    match action {
        ProvidersAction::List => Err(NczError::Precondition(
            "providers list: not yet implemented".into(),
        )),
        ProvidersAction::Test { name: _ } => Err(NczError::Precondition(
            "providers test: not yet implemented".into(),
        )),
        ProvidersAction::SetPrimary { name: _ } => Err(NczError::Precondition(
            "providers set-primary: not yet implemented".into(),
        )),
    }
}
