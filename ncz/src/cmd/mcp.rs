//! mcp — manage Model Context Protocol server declarations.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::{Context, McpAction};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, mcp as mcp_state, Paths};

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum McpReport {
    List(McpListReport),
    Add(McpAddReport),
    Remove(McpRemoveReport),
    Show(McpShowReport),
}

impl Render for McpReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            McpReport::List(report) => report.render_text(w),
            McpReport::Add(report) => report.render_text(w),
            McpReport::Remove(report) => report.render_text(w),
            McpReport::Show(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct McpListReport {
    pub schema_version: u32,
    pub servers: Vec<McpServerReport>,
}

#[derive(Debug, Serialize, Clone)]
pub struct McpServerReport {
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    pub command: Option<String>,
    pub url: Option<String>,
    pub auth_env_var: Option<String>,
    pub file: String,
}

#[derive(Debug, Serialize)]
pub struct McpAddReport {
    pub schema_version: u32,
    pub server: McpServerReport,
}

#[derive(Debug, Serialize)]
pub struct McpRemoveReport {
    pub schema_version: u32,
    pub name: String,
    pub removed: bool,
}

#[derive(Debug, Serialize)]
pub struct McpShowReport {
    pub schema_version: u32,
    pub server: McpServerReport,
}

impl Render for McpListReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for server in &self.servers {
            render_server_line(w, server)?;
        }
        Ok(())
    }
}

impl Render for McpAddReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "MCP server added: {}", self.server.name)
    }
}

impl Render for McpRemoveReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.removed {
            writeln!(w, "MCP server removed: {}", self.name)
        } else {
            writeln!(w, "MCP server absent: {}", self.name)
        }
    }
}

impl Render for McpShowReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        render_server_line(w, &self.server)
    }
}

pub fn run(ctx: &Context, action: McpAction) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: McpAction,
) -> Result<McpReport, NczError> {
    match action {
        McpAction::List => Ok(McpReport::List(list(ctx, paths)?)),
        McpAction::Add {
            name,
            transport,
            command,
            url,
            auth_env,
        } => Ok(McpReport::Add(add(
            ctx, paths, name, transport, command, url, auth_env,
        )?)),
        McpAction::Remove { name } => Ok(McpReport::Remove(remove(paths, &name)?)),
        McpAction::Show { name } => Ok(McpReport::Show(show(ctx, paths, &name)?)),
    }
}

pub fn list(ctx: &Context, paths: &Paths) -> Result<McpListReport, NczError> {
    Ok(McpListReport {
        schema_version: common::SCHEMA_VERSION,
        servers: mcp_state::read_all(paths)?
            .into_iter()
            .map(|record| server_report(ctx, record))
            .collect(),
    })
}

pub fn add(
    ctx: &Context,
    paths: &Paths,
    name: String,
    transport: String,
    command: Option<String>,
    url: Option<String>,
    auth_env: Option<String>,
) -> Result<McpAddReport, NczError> {
    let declaration = mcp_state::McpDeclaration {
        schema_version: common::SCHEMA_VERSION,
        name,
        transport,
        command,
        url,
        auth_env,
    };
    mcp_state::validate_declaration(&declaration)?;
    let _lock = state::acquire_lock(&paths.lock_path)?;
    mcp_state::write(paths, &declaration)?;
    let path = mcp_state::declaration_path(paths, &declaration.name)?;
    Ok(McpAddReport {
        schema_version: common::SCHEMA_VERSION,
        server: server_report(
            ctx,
            mcp_state::McpRecord {
                declaration,
                path,
            },
        ),
    })
}

pub fn remove(paths: &Paths, name: &str) -> Result<McpRemoveReport, NczError> {
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let removed = mcp_state::remove(paths, name)?;
    Ok(McpRemoveReport {
        schema_version: common::SCHEMA_VERSION,
        name: name.to_string(),
        removed,
    })
}

pub fn show(ctx: &Context, paths: &Paths, name: &str) -> Result<McpShowReport, NczError> {
    let record = mcp_state::read(paths, name)?
        .ok_or_else(|| NczError::Usage(format!("unknown MCP server: {name}")))?;
    Ok(McpShowReport {
        schema_version: common::SCHEMA_VERSION,
        server: server_report(ctx, record),
    })
}

fn server_report(ctx: &Context, record: mcp_state::McpRecord) -> McpServerReport {
    let name = record.declaration.name;
    let transport = record.declaration.transport;
    let command = record.declaration.command;
    let url = record.declaration.url;
    let redact_stdio = !ctx.show_secrets && transport == "stdio";
    let endpoint = match transport.as_str() {
        "stdio" if redact_stdio => "***".to_string(),
        "stdio" => command.clone().unwrap_or_default(),
        "http" => url.clone().unwrap_or_default(),
        _ => String::new(),
    };
    let command = if redact_stdio && command.is_some() {
        Some("***".to_string())
    } else {
        command
    };
    McpServerReport {
        name,
        transport,
        endpoint,
        command,
        url,
        auth_env_var: record.declaration.auth_env.map(|auth_env| {
            if ctx.show_secrets {
                auth_env
            } else {
                "***".to_string()
            }
        }),
        file: record.path.display().to_string(),
    }
}

fn render_server_line(w: &mut dyn Write, server: &McpServerReport) -> io::Result<()> {
    writeln!(
        w,
        "{:<18} transport={:<6} endpoint={} auth_env_var={}",
        server.name,
        server.transport,
        if server.endpoint.is_empty() {
            "unknown"
        } else {
            &server.endpoint
        },
        server.auth_env_var.as_deref().unwrap_or("none")
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::cli::{Context, McpAction};
    use crate::cmd::common::test_paths;
    use crate::sys::fake::FakeRunner;

    use super::*;

    fn ctx<'a>(runner: &'a FakeRunner) -> Context<'a> {
        Context {
            json: false,
            show_secrets: false,
            runner,
        }
    }

    #[test]
    fn mcp_add_writes_stdio_declaration_and_redacts_auth_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let report = run_with_paths(
            &ctx(&runner),
            &paths,
            McpAction::Add {
                name: "filesystem".to_string(),
                transport: "stdio".to_string(),
                command: Some("mcp-filesystem /srv".to_string()),
                url: None,
                auth_env: Some("MCP_TOKEN".to_string()),
            },
        )
        .unwrap();

        let McpReport::Add(report) = report else {
            panic!("expected add report");
        };
        assert_eq!(report.server.auth_env_var.as_deref(), Some("***"));
        assert_eq!(report.server.endpoint, "***");
        assert_eq!(report.server.command.as_deref(), Some("***"));
        assert!(paths.mcp_dir().join("filesystem.json").exists());
    }

    #[test]
    fn mcp_show_redacts_stdio_command_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("filesystem.json"),
            r#"{"schema_version":1,"name":"filesystem","transport":"stdio","command":"mcp-filesystem /srv","url":null,"auth_env":null}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = show(&ctx(&runner), &paths, "filesystem").unwrap();

        assert_eq!(report.server.endpoint, "***");
        assert_eq!(report.server.command.as_deref(), Some("***"));
    }

    #[test]
    fn mcp_show_reveals_stdio_command_with_show_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("filesystem.json"),
            r#"{"schema_version":1,"name":"filesystem","transport":"stdio","command":"mcp-filesystem /srv","url":null,"auth_env":null}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();
        let ctx = Context {
            json: false,
            show_secrets: true,
            runner: &runner,
        };

        let report = show(&ctx, &paths, "filesystem").unwrap();

        assert_eq!(report.server.endpoint, "mcp-filesystem /srv");
        assert_eq!(
            report.server.command.as_deref(),
            Some("mcp-filesystem /srv")
        );
    }

    #[test]
    fn mcp_list_reads_declarations() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.mcp_dir()).unwrap();
        fs::write(
            paths.mcp_dir().join("search.json"),
            r#"{"schema_version":1,"name":"search","transport":"http","command":null,"url":"https://mcp.example.test","auth_env":"MCP_TOKEN"}"#,
        )
        .unwrap();
        let runner = FakeRunner::new();

        let report = list(&ctx(&runner), &paths).unwrap();

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.servers[0].name, "search");
        assert_eq!(report.servers[0].endpoint, "https://mcp.example.test");
        assert_eq!(report.servers[0].auth_env_var.as_deref(), Some("***"));
    }

    #[test]
    fn mcp_remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());

        let report = remove(&paths, "missing").unwrap();

        assert!(!report.removed);
    }
}
