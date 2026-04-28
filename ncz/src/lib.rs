//! ncz — nclawzero device-ops umbrella CLI.
//!
//! Replaces the bash dispatcher previously shipped from
//! `pi-gen-nclawzero/stage-zeroclaw/06-install-ncz-cli/`. See
//! `pi-gen-nclawzero/NCZ-CLI-DESIGN.md` for the operator-facing surface.

pub mod cli;
pub mod cmd;
pub mod error;
pub mod output;
pub mod state;
pub mod sys;

pub use error::NczError;

use clap::error::ErrorKind;
use clap::Parser;

/// Library entry point. Parses argv, dispatches to a command handler, and
/// returns the resulting exit code (or an `NczError` whose `exit_code()`
/// the binary maps through `ExitCode`).
pub fn run() -> Result<i32, NczError> {
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) if matches!(err.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            err.print().map_err(NczError::Io)?;
            return Ok(0);
        }
        Err(err) => return Err(NczError::Usage(err.to_string())),
    };
    let runner = sys::RealRunner::new();
    let ctx = cli::Context::new(&cli, &runner);
    cmd::dispatch(cli.command, &ctx)
}
