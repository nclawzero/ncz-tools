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

/// The operator surface mirrored from the bash CLI. Order kept
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
    /// Manage API credentials in the shared agent environment.
    Api {
        #[command(subcommand)]
        action: ApiAction,
    },
    /// Manage LLM providers.
    Providers {
        #[command(subcommand)]
        action: ProvidersAction,
    },
    /// List, status, and refresh configured provider models.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Manage MCP server declarations.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
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
pub enum ApiAction {
    /// List keys from /etc/nclawzero/agent-env.
    List,
    /// Add or update a key in the shared agent environment.
    Add {
        /// Environment variable name.
        key: String,
        /// Value to store. Use env:NAME, -, --value-env, or --value-stdin to avoid argv history.
        value: Option<String>,
        /// Read the value from this process environment variable.
        #[arg(long = "value-env")]
        value_env: Option<String>,
        /// Read the value from stdin.
        #[arg(long = "value-stdin")]
        value_stdin: bool,
        /// Also write per-agent .env override stubs for these agents.
        ///
        /// v0.2 always writes /etc/nclawzero/agent-env; these override files are
        /// compatibility stubs for future per-agent credential routing.
        #[arg(long, value_delimiter = ',')]
        agents: Vec<String>,
        /// Bind this credential to existing providers for live model discovery.
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
    },
    /// Remove a key from the shared agent environment.
    Remove {
        /// Environment variable name.
        key: String,
        /// Remove even when providers or MCP servers still reference this key.
        #[arg(long)]
        force: bool,
    },
    /// Add or update a key in the shared agent environment.
    Set {
        /// Environment variable name.
        key: String,
        /// Value to store. Use env:NAME, -, --value-env, or --value-stdin to avoid argv history.
        value: Option<String>,
        /// Read the value from this process environment variable.
        #[arg(long = "value-env")]
        value_env: Option<String>,
        /// Read the value from stdin.
        #[arg(long = "value-stdin")]
        value_stdin: bool,
        /// Also write per-agent .env override stubs for these agents.
        ///
        /// v0.2 always writes /etc/nclawzero/agent-env; these override files are
        /// compatibility stubs for future per-agent credential routing.
        #[arg(long, value_delimiter = ',')]
        agents: Vec<String>,
        /// Bind this credential to existing providers for live model discovery.
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProvidersAction {
    /// List configured providers.
    List,
    /// Probe a provider's reachability.
    Test { name: String },
    /// Set the primary provider.
    SetPrimary { name: String },
    /// Add a provider declaration.
    Add {
        /// Provider name.
        name: String,
        /// Provider base URL.
        #[arg(long)]
        url: String,
        /// Default model id.
        #[arg(long)]
        model: String,
        /// Environment variable containing the provider API key.
        #[arg(long = "key-env")]
        key_env: String,
        /// Provider type.
        #[arg(long = "type", default_value = "openai-compat")]
        provider_type: String,
        /// Health endpoint path relative to --url.
        #[arg(long, default_value = "/health")]
        health_path: String,
        /// Replace an existing declaration.
        #[arg(long)]
        force: bool,
    },
    /// Remove a provider declaration.
    Remove {
        name: String,
        /// Delete legacy provider files even when they contain inline credentials.
        #[arg(long)]
        drop_inline_credentials: bool,
    },
    /// Show a provider declaration.
    Show { name: String },
}

#[derive(Subcommand, Debug)]
pub enum ModelsAction {
    /// List models across configured providers.
    List {
        /// Limit output to one provider.
        #[arg(long)]
        provider: Option<String>,
        /// Include providers/models that are currently unhealthy.
        #[arg(long)]
        show_unhealthy: bool,
    },
    /// Summarize per-model health.
    Status {
        /// Limit output to one provider.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Force-refresh one provider's model catalog cache.
    Discover {
        /// Provider name.
        provider: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// List MCP server declarations.
    List,
    /// Add an MCP server declaration.
    Add {
        /// MCP server name.
        name: String,
        /// Transport type: stdio or http.
        #[arg(long)]
        transport: String,
        /// Command for stdio transport.
        #[arg(long)]
        command: Option<String>,
        /// URL for http transport.
        #[arg(long)]
        url: Option<String>,
        /// Environment variable containing an MCP auth token.
        #[arg(long = "auth-env")]
        auth_env: Option<String>,
        /// Read and approve the MCP auth token from this process environment variable.
        #[arg(long = "auth-value-env")]
        auth_value_env: Option<String>,
    },
    /// Remove an MCP server declaration.
    Remove { name: String },
    /// Show an MCP server declaration.
    Show { name: String },
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
