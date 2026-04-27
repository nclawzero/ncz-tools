//! update — check for or apply approved OS and container updates.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent, quadlet, Paths};
use crate::sys::apt;

#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum UpdateReport {
    Check(UpdateCheckReport),
    Apply(UpdateApplyReport),
}

impl Render for UpdateReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            UpdateReport::Check(report) => report.render_text(w),
            UpdateReport::Apply(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct UpdateCheckReport {
    pub schema_version: u32,
    pub packages: Vec<String>,
    pub container_updates: String,
}

#[derive(Debug, Serialize)]
pub struct UpdateApplyReport {
    pub schema_version: u32,
    pub packages: Vec<String>,
    pub pulled_images: Vec<String>,
    pub warnings: Vec<String>,
}

impl Render for UpdateCheckReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w)?;
        writeln!(w, "OS package upgrades:")?;
        if self.packages.is_empty() {
            writeln!(
                w,
                "  none from nclawzero-internal, Raspberry Pi, or Tailscale origins"
            )?;
        } else {
            for package in &self.packages {
                writeln!(w, "  {package}")?;
            }
        }
        writeln!(w)?;
        writeln!(w, "Container image updates:")?;
        if self.container_updates.is_empty() {
            Ok(())
        } else {
            write!(w, "{}", self.container_updates)
        }
    }
}

impl Render for UpdateApplyReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.packages.is_empty() {
            writeln!(
                w,
                "No OS package upgrades from nclawzero-internal, Raspberry Pi, or Tailscale origins."
            )?;
        }
        for warning in &self.warnings {
            writeln!(w, "warning: {warning}")?;
        }
        Ok(())
    }
}

pub fn run(ctx: &Context, check: bool) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = run_with_paths(ctx, &paths, check)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn run_with_paths(ctx: &Context, paths: &Paths, check: bool) -> Result<UpdateReport, NczError> {
    let _lock = state::acquire_lock(&paths.lock_path)?;
    if check {
        Ok(UpdateReport::Check(check_updates(ctx)?))
    } else {
        Ok(UpdateReport::Apply(apply_updates(ctx, paths)?))
    }
}

fn check_updates(ctx: &Context) -> Result<UpdateCheckReport, NczError> {
    apt::update(ctx.runner)?;
    let packages = update_candidates(ctx)?;
    let container_updates = if podman_available(ctx) {
        ctx.runner
            .run("podman", &["auto-update", "--dry-run"])
            .map(|out| out.stdout)
            .unwrap_or_default()
    } else {
        "  podman unavailable\n".to_string()
    };
    Ok(UpdateCheckReport {
        schema_version: common::SCHEMA_VERSION,
        packages,
        container_updates,
    })
}

fn apply_updates(ctx: &Context, paths: &Paths) -> Result<UpdateApplyReport, NczError> {
    apt::update(ctx.runner)?;
    let packages = update_candidates(ctx)?;
    if !packages.is_empty() {
        apt_install_upgrades(ctx, &packages)?;
    }

    let mut pulled_images = Vec::new();
    let mut warnings = Vec::new();
    if podman_available(ctx) {
        let _ = ctx.runner.run("sudo", &["podman", "auto-update"]);
        for agent_name in agent::AGENTS {
            if let Some(image) = quadlet::image_for(&paths.agent_quadlet(agent_name))? {
                let out = ctx.runner.run("sudo", &["podman", "pull", &image]);
                match out {
                    Ok(out) if out.ok() => pulled_images.push(image),
                    _ => warnings.push(format!("failed to pull {image}")),
                }
            }
        }
    }

    Ok(UpdateApplyReport {
        schema_version: common::SCHEMA_VERSION,
        packages,
        pulled_images,
        warnings,
    })
}

fn update_candidates(ctx: &Context) -> Result<Vec<String>, NczError> {
    let out = ctx.runner.run("apt", &["list", "--upgradable"])?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: "apt list --upgradable".into(),
            msg: out.stderr,
        });
    }
    let mut packages = Vec::new();
    for line in out.stdout.lines().skip(1) {
        let package = line.split('/').next().unwrap_or("").trim();
        if !package.is_empty() && allowed_origin(ctx, package) {
            packages.push(package.to_string());
        }
    }
    Ok(packages)
}

fn allowed_origin(ctx: &Context, package: &str) -> bool {
    let out = ctx.runner.run("apt-cache", &["policy", package]);
    let Ok(out) = out else {
        return false;
    };
    if !out.ok() {
        return false;
    }
    let policy = out.stdout;
    policy.contains("origin 192.168.207.22")
        || policy.contains("origin pkgs.tailscale.com")
        || policy.contains("origin archive.raspberrypi.com")
        || policy.contains("o=nclawzero-internal")
        || policy.contains("o=Tailscale")
        || policy.contains("o=Raspberry Pi Foundation")
}

fn podman_available(ctx: &Context) -> bool {
    ctx.runner
        .run("podman", &["--version"])
        .map(|out| out.ok())
        .unwrap_or(false)
}

fn apt_install_upgrades(ctx: &Context, packages: &[String]) -> Result<(), NczError> {
    let mut args = vec![
        "env".to_string(),
        "DEBIAN_FRONTEND=noninteractive".to_string(),
        "apt-get".to_string(),
        "-y".to_string(),
        "-o".to_string(),
        "Dpkg::Options::=--force-confdef".to_string(),
        "-o".to_string(),
        "Dpkg::Options::=--force-confold".to_string(),
        "install".to_string(),
        "--only-upgrade".to_string(),
    ];
    args.extend(packages.iter().cloned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = ctx.runner.run("sudo", &refs)?;
    if out.ok() {
        Ok(())
    } else {
        Err(NczError::Exec {
            cmd: "apt-get install --only-upgrade".into(),
            msg: out.stderr,
        })
    }
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
    fn update_check_happy_path_filters_allowed_origins() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect("sudo", &["apt-get", "update"], out(0, "", ""));
        runner.expect(
            "apt",
            &["list", "--upgradable"],
            out(
                0,
                "Listing...\nncz/foo 1 amd64 [upgradable]\nother/bar 1 amd64 [upgradable]\n",
                "",
            ),
        );
        runner.expect(
            "apt-cache",
            &["policy", "ncz"],
            out(0, "  release o=nclawzero-internal\n", ""),
        );
        runner.expect(
            "apt-cache",
            &["policy", "other"],
            out(0, "  release o=Debian\n", ""),
        );

        let report = run_with_paths(&ctx(&runner), &paths, true).unwrap();
        let UpdateReport::Check(report) = report else {
            panic!("expected check report");
        };
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.packages, vec!["ncz"]);
    }

    #[test]
    fn update_error_path_propagates_apt_update_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();
        runner.expect("sudo", &["apt-get", "update"], out(1, "", "apt failed\n"));

        let err = run_with_paths(&ctx(&runner), &paths, true).unwrap_err();
        assert!(matches!(err, NczError::Exec { .. }));
    }

    #[test]
    fn update_apply_happy_path_installs_packages_and_pulls_images() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.quadlet_dir).unwrap();
        fs::write(
            paths.agent_quadlet("zeroclaw"),
            "[Container]\nImage=localhost/zeroclaw:v1\n",
        )
        .unwrap();
        let runner = FakeRunner::new();
        runner.expect("sudo", &["apt-get", "update"], out(0, "", ""));
        runner.expect(
            "apt",
            &["list", "--upgradable"],
            out(0, "Listing...\nncz/foo 1 amd64 [upgradable]\n", ""),
        );
        runner.expect(
            "apt-cache",
            &["policy", "ncz"],
            out(0, "  origin 192.168.207.22\n", ""),
        );
        runner.expect(
            "sudo",
            &[
                "env",
                "DEBIAN_FRONTEND=noninteractive",
                "apt-get",
                "-y",
                "-o",
                "Dpkg::Options::=--force-confdef",
                "-o",
                "Dpkg::Options::=--force-confold",
                "install",
                "--only-upgrade",
                "ncz",
            ],
            out(0, "", ""),
        );
        runner.expect("podman", &["--version"], out(0, "podman 5\n", ""));
        runner.expect("sudo", &["podman", "auto-update"], out(0, "", ""));
        runner.expect(
            "sudo",
            &["podman", "pull", "localhost/zeroclaw:v1"],
            out(0, "", ""),
        );

        let report = run_with_paths(&ctx(&runner), &paths, false).unwrap();
        let UpdateReport::Apply(report) = report else {
            panic!("expected apply report");
        };
        assert_eq!(report.packages, vec!["ncz"]);
        assert_eq!(report.pulled_images, vec!["localhost/zeroclaw:v1"]);
    }
}
