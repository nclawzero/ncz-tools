//! Command dispatch. Each handler module exposes a single
//! `pub fn run(ctx: &Context, args: ...) -> Result<i32, NczError>`.
//!
//! v0.1 scaffold: every module returns `Err(NczError::Precondition("not yet
//! implemented"))` so dispatch compiles and shape is preserved. Codex on
//! ULTRA fills in the bodies one command at a time.

use crate::cli::{Command, Context};
use crate::error::NczError;

mod common;

pub mod api;
pub mod backup;
pub mod channel;
pub mod health;
pub mod inspect;
pub mod integrity;
pub mod logs;
pub mod mcp;
pub mod models;
pub mod pause;
pub mod providers;
pub mod restart;
pub mod resume;
pub mod sandbox;
pub mod selftest;
pub mod set_agent;
pub mod status;
pub mod update;
pub mod version;

pub fn dispatch(command: Command, ctx: &Context) -> Result<i32, NczError> {
    match command {
        Command::Status => status::run(ctx),
        Command::SetAgent { agent } => set_agent::run(ctx, &agent),
        Command::Logs { agent } => logs::run(ctx, agent.as_deref()),
        Command::Restart { agent } => restart::run(ctx, agent.as_deref()),
        Command::Pause { agent } => pause::run(ctx, agent.as_deref()),
        Command::Resume { agent } => resume::run(ctx, agent.as_deref()),
        Command::Version => version::run(ctx),
        Command::Api { action } => api::run(ctx, action),
        Command::Providers { action } => providers::run(ctx, action),
        Command::Models { action } => models::run(ctx, action),
        Command::Mcp { action } => mcp::run(ctx, action),
        Command::Backup { action } => backup::run(ctx, action),
        Command::Sandbox { action } => sandbox::run(ctx, action),
        Command::Integrity => integrity::run(ctx),
        Command::Update { check } => update::run(ctx, check),
        Command::Channel { channel } => channel::run(ctx, channel.as_deref()),
        Command::Health => health::run(ctx),
        Command::Inspect => inspect::run(ctx),
        Command::Selftest => selftest::run(ctx),
    }
}
