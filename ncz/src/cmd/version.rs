//! version — print nclawzero, OS, kernel, and agent image versions.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{agent, quadlet, Paths};
use crate::sys::podman;

#[derive(Debug, Serialize)]
pub struct VersionReport {
    pub schema_version: u32,
    pub nclawzero: String,
    pub os: String,
    pub kernel: String,
    pub agents: BTreeMap<String, String>,
}

impl Render for VersionReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "nclawzero: {}", self.nclawzero)?;
        writeln!(w, "OS:        {}", self.os)?;
        writeln!(w, "Kernel:    {}", self.kernel)?;
        for agent_name in agent::AGENTS {
            let version = self
                .agents
                .get(*agent_name)
                .map(String::as_str)
                .unwrap_or("not-installed");
            writeln!(w, "{:<10} {}", format!("{agent_name}:"), version)?;
        }
        Ok(())
    }
}

pub fn run(ctx: &Context) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = collect(ctx, &paths)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn collect(ctx: &Context, paths: &Paths) -> Result<VersionReport, NczError> {
    let podman_available = ctx
        .runner
        .run("podman", &["--version"])
        .map(|out| out.ok())
        .unwrap_or(false);
    let mut agents = BTreeMap::new();
    for agent_name in agent::AGENTS {
        agents.insert(
            (*agent_name).to_string(),
            agent_version(ctx, paths, agent_name, podman_available)?,
        );
    }

    Ok(VersionReport {
        schema_version: common::SCHEMA_VERSION,
        nclawzero: distro_version(ctx, paths)?,
        os: os_pretty_name(),
        kernel: common::command_stdout(ctx.runner, "uname", &["-r"]).unwrap_or_default(),
        agents,
    })
}

fn distro_version(ctx: &Context, paths: &Paths) -> Result<String, NczError> {
    if let Some(line) = common::read_first_line(&paths.version())? {
        if !line.is_empty() {
            return Ok(line);
        }
    }
    let out = ctx.runner.run(
        "dpkg-query",
        &["-W", "-f=${Version}\\n", "nclawzero-rdp-init"],
    );
    if let Ok(out) = out {
        if out.ok() {
            let version = out.stdout.lines().next().unwrap_or("").trim();
            if !version.is_empty() {
                return Ok(version.to_string());
            }
        }
    }
    Ok("unknown".to_string())
}

fn os_pretty_name() -> String {
    let Ok(body) = fs::read_to_string("/etc/os-release") else {
        return "unknown".to_string();
    };
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
            return common::strip_wrapping_quotes(value);
        }
    }
    "unknown".to_string()
}

fn agent_version(
    ctx: &Context,
    paths: &Paths,
    agent_name: &str,
    podman_available: bool,
) -> Result<String, NczError> {
    let image = match quadlet::image_for(&paths.agent_quadlet(agent_name))? {
        Some(image) => image,
        None => return Ok("not-installed".to_string()),
    };
    if !podman_available || !podman::image_exists(ctx.runner, &image).unwrap_or(false) {
        return Ok(format!("image-missing:{image}"));
    }

    let out = ctx.runner.run(
        "podman",
        &["image", "inspect", "--format", "{{json .Labels}}", &image],
    );
    if let Ok(out) = out {
        if out.ok() {
            if let Some(label) = image_label(&out.stdout, "org.opencontainers.image.version")
                .or_else(|| image_label(&out.stdout, "org.opencontainers.image.revision"))
            {
                if !label.is_empty() {
                    return Ok(label);
                }
            }
        }
    }
    Ok(image)
}

fn image_label(stdout: &str, label: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(stdout.trim()).ok()?;
    value
        .get(label)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::cli::Context;
    use crate::cmd::common::{out, test_paths};
    use crate::error::NczError;
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
    fn version_happy_path_reads_package_and_image_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::create_dir_all(&paths.quadlet_dir).unwrap();
        fs::write(paths.version(), "2026.04\n").unwrap();
        fs::write(
            paths.agent_quadlet("zeroclaw"),
            "[Container]\nImage=localhost/zeroclaw:v1\n",
        )
        .unwrap();

        let runner = FakeRunner::new();
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        runner.expect(
            "podman",
            &["image", "exists", "localhost/zeroclaw:v1"],
            out(0, "", ""),
        );
        runner.expect(
            "podman",
            &[
                "image",
                "inspect",
                "--format",
                "{{json .Labels}}",
                "localhost/zeroclaw:v1",
            ],
            out(0, "{\"org.opencontainers.image.version\":\"1.2.3\"}\n", ""),
        );
        runner.expect("uname", &["-r"], out(0, "6.6.0\n", ""));

        let report = collect(&ctx(&runner), &paths).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.nclawzero, "2026.04");
        assert_eq!(report.agents["zeroclaw"], "1.2.3");
        assert_eq!(report.agents["openclaw"], "not-installed");
    }

    #[test]
    fn version_error_path_reports_unreadable_version_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.version()).unwrap();
        let runner = FakeRunner::new();

        let err = collect(&ctx(&runner), &paths).unwrap_err();
        assert!(matches!(err, NczError::Io(_)));
    }
}
