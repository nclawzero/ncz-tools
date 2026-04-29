//! backup — create, verify, and restore host-side nclawzero state archives.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;

use crate::cli::{BackupAction, Context};
use crate::cmd::common;
use crate::error::NczError;
use crate::output::{self, Render};
use crate::state::{self, agent, backup as backup_state, Paths};
use crate::sys::systemd;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum BackupReport {
    Create(BackupCreateReport),
    Verify(BackupVerifyReport),
    Restore(BackupRestoreReport),
}

impl Render for BackupReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            BackupReport::Create(report) => report.render_text(w),
            BackupReport::Verify(report) => report.render_text(w),
            BackupReport::Restore(report) => report.render_text(w),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BackupCreateReport {
    pub schema_version: u32,
    pub archive: String,
    pub source_count: usize,
    pub redacted_count: usize,
    pub included_secrets: bool,
    pub volumes_included: bool,
}

#[derive(Debug, Serialize)]
pub struct BackupVerifyReport {
    pub schema_version: u32,
    pub archive: String,
    pub ok: bool,
    pub ok_count: usize,
    pub fail_count: usize,
    pub sources: Vec<backup_state::SourceValidation>,
}

#[derive(Debug, Serialize)]
pub struct BackupRestoreReport {
    pub schema_version: u32,
    pub archive: String,
    pub dry_run: bool,
    pub restored_count: usize,
    pub actions: Vec<RestoreAction>,
    pub skipped_redacted_agent_env_keys: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RestoreAction {
    pub path: String,
    pub action: String,
}

impl Render for BackupCreateReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "backup archive: {}", self.archive)?;
        writeln!(w, "sources: {}", self.source_count)?;
        writeln!(w, "redacted: {}", self.redacted_count)?;
        writeln!(
            w,
            "volumes: {}",
            if self.volumes_included {
                "included"
            } else {
                "excluded"
            }
        )
    }
}

impl Render for BackupVerifyReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for source in &self.sources {
            writeln!(
                w,
                "{} {}",
                if source.ok { "ok" } else { "fail" },
                source.path
            )?;
        }
        writeln!(
            w,
            "summary: {} ok, {} failed",
            self.ok_count, self.fail_count
        )
    }
}

impl Render for BackupRestoreReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for action in &self.actions {
            writeln!(w, "{} {}", action.action, action.path)?;
        }
        for key in &self.skipped_redacted_agent_env_keys {
            writeln!(w, "skipped redacted credential: {key}")?;
        }
        writeln!(
            w,
            "{}: {} restored",
            if self.dry_run { "dry-run" } else { "restore" },
            self.restored_count
        )
    }
}

pub fn run(ctx: &Context, action: BackupAction) -> Result<i32, NczError> {
    let paths = Paths::default();
    let BackupRunResult { report, code } = run_with_paths(ctx, &paths, action)?;
    output::emit(&report, ctx.json)?;
    Ok(code)
}

pub struct BackupRunResult {
    pub report: BackupReport,
    pub code: i32,
}

pub fn run_with_paths(
    ctx: &Context,
    paths: &Paths,
    action: BackupAction,
) -> Result<BackupRunResult, NczError> {
    match action {
        BackupAction::Create {
            to,
            include_secrets,
            exclude_volumes,
        } => Ok(BackupRunResult {
            report: BackupReport::Create(create(
                ctx,
                paths,
                &to,
                include_secrets,
                exclude_volumes,
            )?),
            code: 0,
        }),
        BackupAction::Verify { archive } => {
            let report = verify(&archive)?;
            let code = if report.ok { 0 } else { 3 };
            Ok(BackupRunResult {
                report: BackupReport::Verify(report),
                code,
            })
        }
        BackupAction::Restore {
            archive,
            dry_run,
            force,
        } => Ok(BackupRunResult {
            report: BackupReport::Restore(restore(ctx, paths, &archive, dry_run, force)?),
            code: 0,
        }),
    }
}

pub fn create(
    ctx: &Context,
    paths: &Paths,
    archive: &Path,
    include_secrets: bool,
    exclude_volumes: bool,
) -> Result<BackupCreateReport, NczError> {
    if include_secrets {
        eprintln!(
            "ncz: audit: backup create includes unredacted secrets in {}",
            archive.display()
        );
    }
    let mut sources = backup_state::discover_file_sources(paths, include_secrets)?;
    if !exclude_volumes {
        sources.extend(volume_sources(ctx)?);
    }
    let hostname = hostname(ctx);
    let manifest = backup_state::manifest(hostname, &sources);
    backup_state::write_archive(archive, &manifest, &sources)?;
    let redacted_count = manifest
        .sources
        .iter()
        .filter(|source| source.redacted)
        .count();
    Ok(BackupCreateReport {
        schema_version: common::SCHEMA_VERSION,
        archive: archive.display().to_string(),
        source_count: manifest.sources.len(),
        redacted_count,
        included_secrets: include_secrets,
        volumes_included: !exclude_volumes,
    })
}

pub fn verify(archive: &Path) -> Result<BackupVerifyReport, NczError> {
    let (manifest, entries) = backup_state::read_archive(archive)?;
    let sources = backup_state::validate_archive_sources(&manifest, &entries);
    let ok_count = sources.iter().filter(|source| source.ok).count();
    let fail_count = sources.len() - ok_count;
    Ok(BackupVerifyReport {
        schema_version: common::SCHEMA_VERSION,
        archive: archive.display().to_string(),
        ok: fail_count == 0,
        ok_count,
        fail_count,
        sources,
    })
}

pub fn restore(
    ctx: &Context,
    paths: &Paths,
    archive: &Path,
    dry_run: bool,
    force: bool,
) -> Result<BackupRestoreReport, NczError> {
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let (manifest, entries) = backup_state::read_archive(archive)?;
    let validations = backup_state::validate_archive_sources(&manifest, &entries);
    let failed: Vec<String> = validations
        .iter()
        .filter(|validation| !validation.ok)
        .map(|validation| validation.path.clone())
        .collect();
    if !failed.is_empty() {
        return Err(NczError::Inconsistent(format!(
            "backup archive hash mismatch: {}",
            failed.join(", ")
        )));
    }
    if !force && archive_restores_unredacted_agent_env(&manifest) && non_empty(&paths.agent_env())?
    {
        return Err(NczError::Precondition(format!(
            "{} exists and is non-empty; pass --force to overwrite",
            paths.agent_env().display()
        )));
    }

    let staging = std::env::temp_dir().join("ncz-restore-staging");
    if !dry_run {
        match fs::remove_dir_all(&staging) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(NczError::Io(e)),
        }
        fs::create_dir_all(&staging)?;
    }

    let mut actions = Vec::new();
    let mut skipped_redacted_agent_env_keys = Vec::new();
    let mut restored_count = 0;
    for source in &manifest.sources {
        if !backup_state::is_supported_source_path(&source.path) {
            return Err(NczError::Inconsistent(format!(
                "backup archive source is outside nclawzero state: {}",
                source.path
            )));
        }
        let entry = backup_state::archive_entry(&entries, &source.path).ok_or_else(|| {
            NczError::Inconsistent(format!("backup archive missing {}", source.path))
        })?;
        if source.path == backup_state::AGENT_ENV_PATH && source.redacted {
            skipped_redacted_agent_env_keys
                .extend(backup_state::redacted_agent_env_keys(&entry.contents));
            actions.push(RestoreAction {
                path: source.path.clone(),
                action: "skip-redacted".to_string(),
            });
            continue;
        }
        if let Some(volume) = backup_state::source_is_volume(&source.path) {
            actions.push(RestoreAction {
                path: source.path.clone(),
                action: if dry_run {
                    "would-restore-volume".to_string()
                } else {
                    "restore-volume".to_string()
                },
            });
            if !dry_run {
                restore_volume(ctx, &staging, volume, &entry.contents)?;
            }
            restored_count += 1;
            continue;
        }

        let target = backup_state::real_path(paths, &source.path);
        actions.push(RestoreAction {
            path: source.path.clone(),
            action: if dry_run {
                "would-write".to_string()
            } else {
                "write".to_string()
            },
        });
        if !dry_run {
            let staging_path = staging.join(source.path.trim_start_matches('/'));
            if let Some(parent) = staging_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&staging_path, &entry.contents)?;
            let mode = if source.path == backup_state::AGENT_ENV_PATH {
                0o600
            } else {
                0o644
            };
            state::atomic_write(&target, &entry.contents, mode)?;
        }
        restored_count += 1;
    }

    Ok(BackupRestoreReport {
        schema_version: common::SCHEMA_VERSION,
        archive: archive.display().to_string(),
        dry_run,
        restored_count,
        actions,
        skipped_redacted_agent_env_keys,
    })
}

fn hostname(ctx: &Context) -> String {
    ctx.runner
        .run("hostname", &[])
        .ok()
        .filter(|out| out.ok())
        .map(|out| out.stdout.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn volume_sources(ctx: &Context) -> Result<Vec<backup_state::ArchiveSource>, NczError> {
    let mut sources = Vec::new();
    for volume in backup_state::VOLUME_NAMES {
        let exists = ctx.runner.run("podman", &["volume", "exists", volume])?;
        if !exists.ok() {
            continue;
        }
        let path =
            std::env::temp_dir().join(format!("ncz-backup-{volume}-{}.tar", std::process::id()));
        let path_text = path.display().to_string();
        let out = ctx.runner.run(
            "podman",
            &["volume", "export", "--output", &path_text, volume],
        )?;
        if !out.ok() {
            return Err(NczError::Exec {
                cmd: format!("podman volume export {volume}"),
                msg: if out.stderr.is_empty() {
                    out.stdout
                } else {
                    out.stderr
                },
            });
        }
        let contents = fs::read(&path)?;
        let _ = fs::remove_file(&path);
        sources.push(backup_state::volume_source(volume, contents));
    }
    Ok(sources)
}

fn restore_volume(
    ctx: &Context,
    staging: &Path,
    volume: &str,
    contents: &[u8],
) -> Result<(), NczError> {
    let volume_archive = staging.join(format!("{volume}.tar"));
    fs::write(&volume_archive, contents)?;
    let unit = backup_state::volume_agent(volume).map(agent::service_for);
    if let Some(unit) = unit.as_deref() {
        systemd::stop(ctx.runner, unit)?;
    }
    let volume_archive_text = volume_archive.display().to_string();
    let out = ctx.runner.run(
        "podman",
        &["volume", "import", volume, &volume_archive_text],
    )?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("podman volume import {volume}"),
            msg: if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            },
        });
    }
    if let Some(unit) = unit.as_deref() {
        systemd::start(ctx.runner, unit)?;
    }
    Ok(())
}

fn archive_restores_unredacted_agent_env(manifest: &backup_state::BackupManifest) -> bool {
    manifest
        .sources
        .iter()
        .any(|source| source.path == backup_state::AGENT_ENV_PATH && !source.redacted)
}

fn non_empty(path: &Path) -> Result<bool, NczError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len() > 0),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(NczError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use crate::cmd::common::{out, test_paths};
    use crate::sys::fake::FakeRunner;

    use super::*;

    fn ctx<'a>(runner: &'a FakeRunner) -> Context<'a> {
        Context {
            json: false,
            show_secrets: false,
            runner,
        }
    }

    fn expect_hostname(runner: &FakeRunner) {
        runner.expect("hostname", &[], out(0, "test-host\n", ""));
    }

    fn write_archive(
        archive: &Path,
        path: &str,
        contents: &[u8],
        redacted: bool,
    ) -> backup_state::BackupManifest {
        let source = backup_state::ArchiveSource {
            source: backup_state::BackupSource {
                path: path.to_string(),
                sha256: backup_state::sha256_hex(contents),
                size: contents.len() as u64,
                redacted,
            },
            archive_path: backup_state::archive_path_for_source(path),
            contents: contents.to_vec(),
        };
        let manifest = backup_state::manifest("test-host".to_string(), &[source.clone()]);
        backup_state::write_archive(archive, &manifest, &[source]).unwrap();
        manifest
    }

    #[test]
    fn create_redacts_agent_env_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "OPENAI_API_KEY=sk-live\n").unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        let runner = FakeRunner::new();
        expect_hostname(&runner);

        let report = create(&ctx(&runner), &paths, &archive, false, true).unwrap();
        let (manifest, entries) = backup_state::read_archive(&archive).unwrap();
        let entry = backup_state::archive_entry(&entries, backup_state::AGENT_ENV_PATH).unwrap();

        assert_eq!(report.redacted_count, 1);
        assert_eq!(manifest.sources[0].path, backup_state::AGENT_ENV_PATH);
        assert!(manifest.sources[0].redacted);
        assert_eq!(
            String::from_utf8(entry.contents.clone()).unwrap(),
            "OPENAI_API_KEY=REDACTED:OPENAI_API_KEY\n"
        );
        assert_eq!(
            fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
            0o600
        );
        runner.assert_done();
    }

    #[test]
    fn create_include_secrets_keeps_agent_env_values() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "OPENAI_API_KEY=sk-live\n").unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        let runner = FakeRunner::new();
        expect_hostname(&runner);

        let report = create(&ctx(&runner), &paths, &archive, true, true).unwrap();
        let (manifest, entries) = backup_state::read_archive(&archive).unwrap();
        let entry = backup_state::archive_entry(&entries, backup_state::AGENT_ENV_PATH).unwrap();

        assert!(report.included_secrets);
        assert!(!manifest.sources[0].redacted);
        assert_eq!(
            String::from_utf8(entry.contents.clone()).unwrap(),
            "OPENAI_API_KEY=sk-live\n"
        );
        runner.assert_done();
    }

    #[test]
    fn restore_refuses_on_non_empty_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "EXISTING=1\n").unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(&archive, backup_state::AGENT_ENV_PATH, b"NEW=2\n", false);
        let runner = FakeRunner::new();

        let err = restore(&ctx(&runner), &paths, &archive, false, false).unwrap_err();
        assert!(matches!(err, NczError::Precondition(_)));
    }

    #[test]
    fn restore_respects_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(&archive, "/etc/nclawzero/channel", b"canary\n", false);
        let runner = FakeRunner::new();

        let report = restore(&ctx(&runner), &paths, &archive, true, false).unwrap();

        assert!(report.dry_run);
        assert_eq!(report.restored_count, 1);
        assert!(!paths.channel().exists());
    }

    #[test]
    fn restore_skips_redacted_agent_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(
            &archive,
            backup_state::AGENT_ENV_PATH,
            b"OPENAI_API_KEY=REDACTED:OPENAI_API_KEY\n",
            true,
        );
        let runner = FakeRunner::new();

        let report = restore(&ctx(&runner), &paths, &archive, false, false).unwrap();

        assert_eq!(report.restored_count, 0);
        assert_eq!(
            report.skipped_redacted_agent_env_keys,
            vec!["OPENAI_API_KEY".to_string()]
        );
        assert!(!paths.agent_env().exists());
    }
}
