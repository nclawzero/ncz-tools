//! Clap definitions and the command-context glue passed to handlers.

use clap::{Parser, Subcommand};

use crate::sys::CommandRunner;

#[derive(Parser, Debug)]
#[command(
    name = "ncz",
    version,
    about = "nclawzero device-ops umbrella CLI",
    long_about = None,
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Show secret values verbatim (default: redact tokens, keys, passwords).
    #[arg(long, global = true)]
    pub show_secrets: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// The 14-noun operator surface mirrored from the bash CLI. Order kept
/// stable for `--help` legibility.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print device + active-agent status.
    Status,
    /// Switch the active agent (zeroclaw|openclaw|hermes).
    SetAgent {
        /// Agent name.
        agent: String,
    },
    /// Tail logs for the active or named agent.
    Logs {
        /// Optional agent name (defaults to the active agent).
        agent: Option<String>,
    },
    /// Restart the active or named agent.
    Restart { agent: Option<String> },
    /// Pause (stop) the active or named agent.
    Pause { agent: Option<String> },
    /// Resume (start) the active or named agent.
    Resume { agent: Option<String> },
    /// Print binary, runtime, and image versions.
    Version,
    /// Manage LLM providers (list|test|set-primary).
    Providers {
        #[command(subcommand)]
        action: ProvidersAction,
    },
    /// Inspect or manage the sandbox runtime policy.
    Sandbox {
        #[command(subcommand)]
        action: Option<SandboxAction>,
    },
    /// Verify the manifest hash of installed nclawzero packages.
    Integrity,
    /// Check for or apply pending updates.
    Update {
        /// Only check; do not apply.
        #[arg(long)]
        check: bool,
    },
    /// Switch release channel (stable|canary|beta).
    Channel {
        /// Channel name; omit to print the current channel.
        channel: Option<String>,
    },
    /// One-line health summary; non-zero exit on inconsistency.
    Health,
    /// Dump active configuration with secrets redacted by default.
    Inspect,
    /// Run all self-checks (sudoers, quadlet presence, etc.).
    Selftest,
}

#[derive(Subcommand, Debug)]
pub enum ProvidersAction {
    /// List configured providers.
    List,
    /// Probe a provider's reachability.
    Test { name: String },
    /// Set the primary provider.
    SetPrimary { name: String },
}

#[derive(Subcommand, Debug)]
pub enum SandboxAction {
    /// Print the active sandbox policy for an agent.
    Policy { agent: String },
}

/// Per-invocation context shared across handlers. Holds the parsed CLI plus
/// a borrowed `CommandRunner` so handlers can be unit-tested with a fake.
pub struct Context<'a> {
    pub json: bool,
    pub show_secrets: bool,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> Context<'a> {
    pub fn new(cli: &Cli, runner: &'a dyn CommandRunner) -> Self {
        Self {
            json: cli.json,
            show_secrets: cli.show_secrets,
            runner,
        }
    }
}
