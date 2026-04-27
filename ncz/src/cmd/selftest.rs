//! selftest — run lightweight dispatcher smoke checks.

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};

#[derive(Debug, Serialize)]
pub struct SelftestReport {
    pub schema_version: u32,
    pub binary: String,
    pub checks: Vec<SelftestCheck>,
    pub failures: u32,
}

#[derive(Debug, Serialize)]
pub struct SelftestCheck {
    pub name: String,
    pub ok: bool,
    pub exit_code: i32,
    pub stderr: String,
}

impl Render for SelftestReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for check in &self.checks {
            write!(w, "selftest: {:<18}", check.name)?;
            if check.ok {
                if check.name == "status" && check.exit_code == 3 {
                    writeln!(w, " ok (reported inconsistent state)")?;
                } else {
                    writeln!(w, " ok")?;
                }
            } else {
                writeln!(w, " failed (exit {})", check.exit_code)?;
            }
        }
        if self.failures == 0 {
            writeln!(w, "selftest: all checks passed")?;
        }
        Ok(())
    }
}

pub fn run(ctx: &Context) -> Result<i32, NczError> {
    let report = collect(ctx, None);
    let failures = report.failures;
    output::emit(&report, ctx.json)?;
    if failures > 0 {
        Err(NczError::Precondition(format!(
            "selftest failed: {failures} check(s)"
        )))
    } else {
        Ok(0)
    }
}

pub fn collect(ctx: &Context, binary: Option<&str>) -> SelftestReport {
    let binary = binary
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("NCZ_SELFTEST_BIN").ok())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|path| path.display().to_string())
        })
        .unwrap_or_else(|| "/usr/local/bin/ncz".to_string());

    let specs: [(&str, &[&str]); 5] = [
        ("help", &["help"]),
        ("version", &["version", "--json"]),
        ("status", &["status", "--json"]),
        ("sandbox", &["sandbox", "--json"]),
        ("providers list", &["providers", "list", "--json"]),
    ];
    let checks: Vec<SelftestCheck> = specs
        .iter()
        .map(|(name, args)| run_check(ctx, &binary, name, args))
        .collect();
    let failures = checks.iter().filter(|check| !check.ok).count() as u32;
    SelftestReport {
        schema_version: common::SCHEMA_VERSION,
        binary,
        checks,
        failures,
    }
}

fn run_check(ctx: &Context, binary: &str, name: &str, args: &[&str]) -> SelftestCheck {
    match ctx.runner.run(binary, args) {
        Ok(out) => {
            let ok = out.ok() || (name == "status" && out.status == 3);
            SelftestCheck {
                name: name.to_string(),
                ok,
                exit_code: out.status,
                stderr: out.stderr,
            }
        }
        Err(err) => SelftestCheck {
            name: name.to_string(),
            ok: false,
            exit_code: -1,
            stderr: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Context;
    use crate::cmd::common::out;
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
    fn selftest_happy_path_accepts_status_three() {
        let runner = FakeRunner::new();
        let bin = "/tmp/ncz";
        runner.expect(bin, &["help"], out(0, "", ""));
        runner.expect(bin, &["version", "--json"], out(0, "", ""));
        runner.expect(bin, &["status", "--json"], out(3, "", ""));
        runner.expect(bin, &["sandbox", "--json"], out(0, "", ""));
        runner.expect(bin, &["providers", "list", "--json"], out(0, "", ""));

        let report = collect(&ctx(&runner), Some(bin));
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.failures, 0);
    }

    #[test]
    fn selftest_error_path_counts_failed_check() {
        let runner = FakeRunner::new();
        let bin = "/tmp/ncz";
        runner.expect(bin, &["help"], out(0, "", ""));
        runner.expect(bin, &["version", "--json"], out(1, "", "bad\n"));
        runner.expect(bin, &["status", "--json"], out(0, "", ""));
        runner.expect(bin, &["sandbox", "--json"], out(0, "", ""));
        runner.expect(bin, &["providers", "list", "--json"], out(0, "", ""));

        let report = collect(&ctx(&runner), Some(bin));
        assert_eq!(report.failures, 1);
        assert_eq!(report.checks[1].stderr, "bad\n");
    }
}
