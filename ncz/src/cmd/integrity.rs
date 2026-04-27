//! integrity — verify the installed nclawzero manifest with sha256sum.

use std::io::{self, Write};
use std::path::PathBuf;

use serde::Serialize;

use crate::cli::Context;
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::Paths;

#[derive(Debug, Serialize)]
pub struct IntegrityReport {
    pub schema_version: u32,
    pub manifest: String,
    pub ok: bool,
    pub output: String,
}

impl Render for IntegrityReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        write!(w, "{}", self.output)
    }
}

pub fn run(ctx: &Context) -> Result<i32, NczError> {
    let paths = Paths::default();
    let report = verify(ctx, &paths)?;
    output::emit(&report, ctx.json)?;
    Ok(0)
}

pub fn verify(ctx: &Context, paths: &Paths) -> Result<IntegrityReport, NczError> {
    let manifest = manifest_path(paths);
    if !manifest.is_file() {
        return Err(NczError::Precondition(format!(
            "missing integrity manifest: {}",
            manifest.display()
        )));
    }
    let manifest_arg = manifest.to_string_lossy().to_string();
    let out = ctx.runner.run("sha256sum", &["-c", &manifest_arg])?;
    if !out.ok() {
        return Err(NczError::Inconsistent(if out.stderr.is_empty() {
            out.stdout
        } else {
            out.stderr
        }));
    }
    Ok(IntegrityReport {
        schema_version: common::SCHEMA_VERSION,
        manifest: manifest.display().to_string(),
        ok: true,
        output: out.stdout,
    })
}

fn manifest_path(paths: &Paths) -> PathBuf {
    if paths.manifest().is_file() {
        paths.manifest()
    } else {
        PathBuf::from("/usr/share/nclawzero/manifest.sha256")
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
    fn integrity_happy_path_runs_sha256sum_check() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.manifest(), "abc  file\n").unwrap();
        let manifest = paths.manifest().to_string_lossy().to_string();
        let runner = FakeRunner::new();
        runner.expect("sha256sum", &["-c", &manifest], out(0, "file: OK\n", ""));

        let report = verify(&ctx(&runner), &paths).unwrap();
        assert_eq!(report.schema_version, 1);
        assert!(report.ok);
        assert_eq!(report.output, "file: OK\n");
    }

    #[test]
    fn integrity_error_path_reports_missing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let runner = FakeRunner::new();

        let err = verify(&ctx(&runner), &paths).unwrap_err();
        assert!(matches!(err, NczError::Precondition(_)));
    }
}
