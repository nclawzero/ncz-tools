//! sandbox — inspect runtime sandbox signals and policy additions.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use serde::Serialize;

use crate::cli::{Context, SandboxAction};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::Paths;

#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum SandboxReport {
    Show(SandboxShowReport),
    Policy(SandboxPolicyReport),
}

impl Render for SandboxReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            SandboxReport::Show(report) => report.render_text(w),
            SandboxReport::Policy(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SandboxShowReport {
    pub schema_version: u32,
    pub agent: String,
    pub landlock: String,
    pub seccomp: String,
    pub quadlet: String,
    pub capabilities: String,
    pub policy_file: String,
    pub policy_present: bool,
}

#[derive(Debug, Serialize)]
pub struct SandboxPolicyReport {
    pub schema_version: u32,
    pub agent: String,
    pub policy_file: String,
    pub lines: Vec<String>,
}

impl Render for SandboxShowReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Agent:       {}", self.agent)?;
        writeln!(w, "Landlock:    {}", self.landlock)?;
        writeln!(w, "Seccomp:     {}", self.seccomp)?;
        writeln!(w, "Quadlet:     {}", self.quadlet)?;
        writeln!(
            w,
            "Capabilities: {}",
            if self.capabilities.is_empty() {
                "not declared"
            } else {
                &self.capabilities
            }
        )?;
        writeln!(w, "Policy:      {}", self.policy_file)?;
        if !self.policy_present {
            writeln!(w, "Policy file is not present.")?;
        }
        Ok(())
    }
}

impl Render for SandboxPolicyReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for line in &self.lines {
            writeln!(w, "{line}")?;
        }
        Ok(())
    }
}

pub fn run(ctx: &Context, action: Option<SandboxAction>) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: Option<SandboxAction>,
) -> Result<SandboxReport, NczError> {
    match action {
        None => Ok(SandboxReport::Show(show(ctx, paths, None)?)),
        Some(SandboxAction::Policy { agent }) => {
            Ok(SandboxReport::Policy(policy(ctx, paths, &agent)?))
        }
    }
}

pub fn show(
    _ctx: &Context,
    paths: &Paths,
    requested_agent: Option<&str>,
) -> Result<SandboxShowReport, NczError> {
    let agent = common::resolve_agent(paths, requested_agent)?;
    let quadlet = paths.agent_quadlet(&agent);
    let policy = policy_file(paths, &agent);
    Ok(SandboxShowReport {
        schema_version: common::SCHEMA_VERSION,
        agent,
        landlock: landlock(),
        seccomp: seccomp(),
        quadlet: quadlet.display().to_string(),
        capabilities: capabilities(&quadlet),
        policy_present: policy.is_file(),
        policy_file: policy.display().to_string(),
    })
}

pub fn policy(
    ctx: &Context,
    paths: &Paths,
    agent_name: &str,
) -> Result<SandboxPolicyReport, NczError> {
    common::validate_agent(agent_name)?;
    let policy_file = policy_file(paths, agent_name);
    if !policy_file.is_file() {
        return Err(NczError::Precondition(format!(
            "missing policy file: {}",
            policy_file.display()
        )));
    }
    let body = fs::read_to_string(&policy_file)?;
    Ok(SandboxPolicyReport {
        schema_version: common::SCHEMA_VERSION,
        agent: agent_name.to_string(),
        policy_file: policy_file.display().to_string(),
        lines: body
            .lines()
            .map(|line| common::redact_line(line, ctx.show_secrets))
            .collect(),
    })
}

fn landlock() -> String {
    if PathBuf::from("/sys/kernel/security/landlock").is_dir()
        || fs::read_to_string("/proc/filesystems")
            .map(|body| body.split_whitespace().any(|field| field == "landlock"))
            .unwrap_or(false)
    {
        "available".to_string()
    } else {
        "unknown".to_string()
    }
}

fn seccomp() -> String {
    let Ok(body) = fs::read_to_string("/proc/1/status") else {
        return "unknown".to_string();
    };
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("Seccomp:") {
            return rest.trim().to_string();
        }
    }
    "unknown".to_string()
}

fn capabilities(quadlet: &std::path::Path) -> String {
    let Ok(body) = fs::read_to_string(quadlet) else {
        return String::new();
    };
    body.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let key = trimmed.split_once('=')?.0.trim();
            if matches!(
                key,
                "DropCapability" | "AddCapability" | "SecurityLabelDisable" | "NoNewPrivileges"
            ) {
                Some(trimmed.to_string())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn policy_file(paths: &Paths, agent_name: &str) -> PathBuf {
    let candidates = [
        paths
            .sandbox_dir()
            .join(agent_name)
            .join("policy-additions.yaml"),
        paths.etc_dir.join(agent_name).join("policy-additions.yaml"),
        paths
            .etc_dir
            .join(format!("policy-additions-{agent_name}.yaml")),
    ];
    for candidate in &candidates {
        if candidate.is_file() {
            return candidate.clone();
        }
    }
    candidates[0].clone()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::cli::Context;
    use crate::cmd::common::test_paths;
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
    fn sandbox_happy_path_reports_quadlet_policy_and_redacts_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::create_dir_all(&paths.quadlet_dir).unwrap();
        fs::write(paths.agent_state(), "zeroclaw\n").unwrap();
        fs::write(
            paths.agent_quadlet("zeroclaw"),
            "Image=example\nNoNewPrivileges=true\nDropCapability=all\n",
        )
        .unwrap();
        let policy_path = paths
            .sandbox_dir()
            .join("zeroclaw")
            .join("policy-additions.yaml");
        fs::create_dir_all(policy_path.parent().unwrap()).unwrap();
        fs::write(&policy_path, "api_key: abc\nallow: true\n").unwrap();
        let runner = FakeRunner::new();

        let show = show(&ctx(&runner), &paths, None).unwrap();
        assert_eq!(show.schema_version, 1);
        assert_eq!(show.capabilities, "NoNewPrivileges=true;DropCapability=all");

        let policy = policy(&ctx(&runner), &paths, "zeroclaw").unwrap();
        assert_eq!(policy.lines[0], "api_key: ***");
    }

    #[test]
    fn sandbox_error_path_reports_missing_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let err = policy(&ctx(&runner), &paths, "hermes").unwrap_err();
        assert!(matches!(err, NczError::Precondition(_)));
    }
}
