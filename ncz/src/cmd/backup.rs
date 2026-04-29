//! backup — create, verify, and restore host-side nclawzero state archives.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
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
#[non_exhaustive]
pub struct BackupCreateReport {
    pub schema_version: u32,
    pub archive: String,
    pub source_count: usize,
    pub redacted_count: usize,
    pub included_secrets: bool,
    pub volumes_included: bool,
    pub unsafe_live_volumes: bool,
}

#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct BackupVerifyReport {
    pub schema_version: u32,
    pub archive: String,
    pub unsafe_live_volumes: bool,
    pub ok: bool,
    pub ok_count: usize,
    pub fail_count: usize,
    pub sources: Vec<backup_state::SourceValidation>,
}

#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct BackupRestoreReport {
    pub schema_version: u32,
    pub archive: String,
    pub dry_run: bool,
    pub unsafe_live_volumes: bool,
    pub restored_count: usize,
    pub actions: Vec<RestoreAction>,
    pub skipped_redacted_agent_env_keys: Vec<String>,
    pub skipped_redacted_provider_files: Vec<String>,
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
        )?;
        writeln!(
            w,
            "unsafe live volumes: {}",
            if self.unsafe_live_volumes {
                "enabled"
            } else {
                "disabled"
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
            "unsafe live volumes: {}",
            if self.unsafe_live_volumes {
                "enabled"
            } else {
                "disabled"
            }
        )?;
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
        for path in &self.skipped_redacted_provider_files {
            writeln!(w, "skipped redacted provider/MCP source: {path}")?;
        }
        writeln!(
            w,
            "unsafe live volumes: {}",
            if self.unsafe_live_volumes {
                "enabled"
            } else {
                "disabled"
            }
        )?;
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
            unsafe_live_volumes,
        } => Ok(BackupRunResult {
            report: BackupReport::Create(create_with_unsafe_live_volumes(
                ctx,
                paths,
                &to,
                include_secrets,
                exclude_volumes,
                unsafe_live_volumes,
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
    create_with_options(ctx, paths, archive, include_secrets, exclude_volumes, false)
}

pub fn create_with_unsafe_live_volumes(
    ctx: &Context,
    paths: &Paths,
    archive: &Path,
    include_secrets: bool,
    exclude_volumes: bool,
    unsafe_live_volumes: bool,
) -> Result<BackupCreateReport, NczError> {
    create_with_options(
        ctx,
        paths,
        archive,
        include_secrets,
        exclude_volumes,
        unsafe_live_volumes,
    )
}

pub(crate) fn create_with_options(
    ctx: &Context,
    paths: &Paths,
    archive: &Path,
    include_secrets: bool,
    exclude_volumes: bool,
    unsafe_live_volumes: bool,
) -> Result<BackupCreateReport, NczError> {
    let _lock = state::acquire_lock(&paths.lock_path)?;
    let captured_live_volumes = unsafe_live_volumes && !exclude_volumes;
    if include_secrets {
        eprintln!(
            "ncz: audit: backup create includes unredacted secrets in {}",
            archive.display()
        );
    }
    if captured_live_volumes {
        eprintln!(
            "ncz: audit: backup create exporting live Podman volumes without quiesce \
             into {}",
            archive.display()
        );
    }
    let mut sources = backup_state::discover_file_sources(paths, include_secrets)?;
    if !exclude_volumes {
        sources.extend(volume_sources(ctx, captured_live_volumes)?);
    }
    let hostname = hostname(ctx);
    let manifest = backup_state::manifest_with_options(hostname, &sources, captured_live_volumes);
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
        unsafe_live_volumes: captured_live_volumes,
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
        unsafe_live_volumes: manifest.unsafe_live_volumes,
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
    if manifest.unsafe_live_volumes {
        eprintln!(
            "ncz: warning: archive captured Podman volumes WITHOUT quiesce; \
             volume contents may be inconsistent"
        );
    }
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
    if !force {
        let collisions = restore_collisions(ctx, paths, &manifest, &entries)?;
        if !collisions.is_empty() {
            return Err(NczError::Precondition(collisions.describe()));
        }
    }

    let staging = if dry_run {
        None
    } else {
        Some(restore_staging_dir()?)
    };

    let mut actions = Vec::new();
    let mut skipped_redacted_agent_env_keys = Vec::new();
    let mut skipped_redacted_provider_files = Vec::new();
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
        if source.redacted {
            if source.path == backup_state::AGENT_ENV_PATH {
                skipped_redacted_agent_env_keys
                    .extend(backup_state::redacted_agent_env_keys(&entry.contents));
            } else {
                skipped_redacted_provider_files.push(source.path.clone());
            }
            actions.push(RestoreAction {
                path: source.path.clone(),
                action: if dry_run {
                    "would_skip_redacted".to_string()
                } else {
                    "skip-redacted".to_string()
                },
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
                restore_volume(
                    ctx,
                    staging.as_ref().unwrap().path(),
                    volume,
                    &entry.contents,
                )?;
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
            write_staging_file(
                staging.as_ref().unwrap().path(),
                &source.path,
                &entry.contents,
            )?;
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
        unsafe_live_volumes: manifest.unsafe_live_volumes,
        restored_count,
        actions,
        skipped_redacted_agent_env_keys,
        skipped_redacted_provider_files,
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

fn volume_sources(
    ctx: &Context,
    unsafe_live_volumes: bool,
) -> Result<Vec<backup_state::ArchiveSource>, NczError> {
    let mut sources = Vec::new();
    for volume in backup_state::VOLUME_NAMES {
        let exists = ctx.runner.run("podman", &["volume", "exists", volume])?;
        if !exists.ok() {
            continue;
        }
        let path =
            std::env::temp_dir().join(format!("ncz-backup-{volume}-{}.tar", std::process::id()));
        let path_text = path.display().to_string();
        export_volume(ctx, volume, &path_text, unsafe_live_volumes)?;
        let contents = fs::read(&path)?;
        let _ = fs::remove_file(&path);
        sources.push(backup_state::volume_source(volume, contents));
    }
    Ok(sources)
}

fn export_volume(
    ctx: &Context,
    volume: &str,
    output: &str,
    unsafe_live_volumes: bool,
) -> Result<(), NczError> {
    let unit = backup_state::volume_agent(volume).map(agent::service_for);
    let was_active = if !unsafe_live_volumes {
        if let Some(unit) = unit.as_deref() {
            let was_active = systemd::is_active_checked(ctx.runner, unit)?;
            if was_active {
                systemd::stop(ctx.runner, unit)?;
            }
            was_active
        } else {
            false
        }
    } else {
        false
    };
    let export_result = podman_export_volume(ctx, volume, output);
    if was_active {
        let start_result = systemd::start(ctx.runner, unit.as_deref().unwrap());
        if export_result.is_ok() {
            start_result?;
        }
    }
    export_result
}

fn podman_export_volume(ctx: &Context, volume: &str, output: &str) -> Result<(), NczError> {
    let out = ctx
        .runner
        .run("podman", &["volume", "export", "--output", output, volume])?;
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
    Ok(())
}

fn restore_staging_dir() -> Result<tempfile::TempDir, NczError> {
    Ok(tempfile::Builder::new()
        .prefix("ncz-restore-staging-")
        .tempdir()?)
}

#[derive(Debug, Default)]
struct RestoreCollisions {
    paths: Vec<String>,
}

impl RestoreCollisions {
    fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    fn describe(&self) -> String {
        format!(
            "restore would overwrite non-empty state; pass --force to overwrite: {}",
            self.paths.join(", ")
        )
    }
}

fn restore_collisions(
    ctx: &Context,
    paths: &Paths,
    manifest: &backup_state::BackupManifest,
    entries: &[backup_state::ArchiveEntry],
) -> Result<RestoreCollisions, NczError> {
    let mut collisions = RestoreCollisions::default();
    for source in &manifest.sources {
        if !backup_state::is_supported_source_path(&source.path) {
            return Err(NczError::Inconsistent(format!(
                "backup archive source is outside nclawzero state: {}",
                source.path
            )));
        }
        if source.redacted {
            continue;
        }
        if let Some(volume) = backup_state::source_is_volume(&source.path) {
            if volume_has_existing_files(ctx, volume)? {
                collisions.paths.push(source.path.clone());
            }
            continue;
        }
        let entry = backup_state::archive_entry(entries, &source.path).ok_or_else(|| {
            NczError::Inconsistent(format!("backup archive missing {}", source.path))
        })?;
        let target = backup_state::real_path(paths, &source.path);
        if existing_file_differs(&target, &entry.contents)? {
            collisions.paths.push(target.display().to_string());
        }
    }
    Ok(collisions)
}

fn existing_file_differs(path: &Path, contents: &[u8]) -> Result<bool, NczError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(NczError::Io(e)),
    };
    if metadata.len() == 0 {
        return Ok(false);
    }
    Ok(fs::read(path)? != contents)
}

fn volume_has_existing_files(ctx: &Context, volume: &str) -> Result<bool, NczError> {
    let exists = ctx.runner.run("podman", &["volume", "exists", volume])?;
    if !exists.ok() {
        return Ok(false);
    }
    let out = ctx.runner.run(
        "podman",
        &["volume", "inspect", "--format", "{{.Mountpoint}}", volume],
    )?;
    if !out.ok() {
        return Err(NczError::Exec {
            cmd: format!("podman volume inspect {volume}"),
            msg: if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            },
        });
    }
    let mountpoint = out.stdout.trim();
    if mountpoint.is_empty() {
        return Ok(false);
    }
    dir_has_any_entry(Path::new(mountpoint))
}

fn dir_has_any_entry(path: &Path) -> Result<bool, NczError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(NczError::Io(e)),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if dir_has_any_entry(&entry.path())? {
                return Ok(true);
            }
        } else {
            return Ok(true);
        }
    }
    Ok(false)
}

fn write_staging_file(staging: &Path, source_path: &str, contents: &[u8]) -> Result<(), NczError> {
    let staging_path = staging.join(source_path.trim_start_matches('/'));
    if let Some(parent) = staging_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if source_path == backup_state::AGENT_ENV_PATH {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging_path)?;
        file.write_all(contents)?;
        return Ok(());
    }
    fs::write(&staging_path, contents)?;
    Ok(())
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
    let was_active = if let Some(unit) = unit.as_deref() {
        let was_active = systemd::is_active_checked(ctx.runner, unit)?;
        systemd::stop(ctx.runner, unit)?;
        was_active
    } else {
        false
    };
    let import_result = import_volume(ctx, volume, &volume_archive);
    if was_active {
        let start_result = systemd::start(ctx.runner, unit.as_deref().unwrap());
        if import_result.is_ok() {
            start_result?;
        }
    }
    import_result
}

fn import_volume(ctx: &Context, volume: &str, volume_archive: &Path) -> Result<(), NczError> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use crate::cmd::common::{out, test_paths};
    use crate::sys::fake::FakeRunner;

    use super::*;

    static VOLUME_EXPORT_TEST_LOCK: Mutex<()> = Mutex::new(());

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

    fn expect_unit_state(runner: &FakeRunner, unit: &str, active: &str, sub: &str) {
        runner.expect(
            "systemctl",
            &[
                "show",
                unit,
                "--property=LoadState",
                "--property=ActiveState",
                "--property=SubState",
            ],
            out(
                0,
                &format!("LoadState=loaded\nActiveState={active}\nSubState={sub}\n"),
                "",
            ),
        );
    }

    fn backup_volume_export_path(volume: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ncz-backup-{volume}-{}.tar", std::process::id()))
    }

    fn write_archive(
        archive: &Path,
        path: &str,
        contents: &[u8],
        redacted: bool,
    ) -> backup_state::BackupManifest {
        write_archive_with_unsafe_live_volumes(archive, path, contents, redacted, false)
    }

    fn write_archive_with_unsafe_live_volumes(
        archive: &Path,
        path: &str,
        contents: &[u8],
        redacted: bool,
        unsafe_live_volumes: bool,
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
        let sources = std::slice::from_ref(&source);
        let manifest = backup_state::manifest_with_options(
            "test-host".to_string(),
            sources,
            unsafe_live_volumes,
        );
        backup_state::write_archive(archive, &manifest, sources).unwrap();
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
    fn backup_create_default_manifest_round_trips_unsafe_live_volumes_false() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let archive = tmp.path().join("backup.tar.gz");
        let runner = FakeRunner::new();
        expect_hostname(&runner);

        let report = create(&ctx(&runner), &paths, &archive, false, true).unwrap();
        let (manifest, _) = backup_state::read_archive(&archive).unwrap();
        let verify_report = verify(&archive).unwrap();

        assert!(!report.unsafe_live_volumes);
        assert!(!manifest.unsafe_live_volumes);
        assert!(!verify_report.unsafe_live_volumes);
        runner.assert_done();
    }

    #[test]
    fn backup_create_redacts_inline_apikey_in_provider_json_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("legacy.json"),
            r#"{"name":"legacy","api_key":"sk-live","nested":{"token":"tok-live"}}"#,
        )
        .unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        let runner = FakeRunner::new();
        expect_hostname(&runner);

        let report = create(&ctx(&runner), &paths, &archive, false, true).unwrap();
        let (manifest, entries) = backup_state::read_archive(&archive).unwrap();
        let entry = backup_state::archive_entry(&entries, "/etc/nclawzero/providers.d/legacy.json")
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&entry.contents).unwrap();

        assert_eq!(report.redacted_count, 1);
        assert!(manifest.sources[0].redacted);
        assert_eq!(json["api_key"], "REDACTED:api_key");
        assert_eq!(json["nested"]["token"], "REDACTED:token");
        runner.assert_done();
    }

    #[test]
    fn backup_create_with_include_secrets_preserves_inline_apikey_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let provider = r#"{"name":"legacy","api_key":"sk-live"}"#;
        fs::write(paths.providers_dir().join("legacy.json"), provider).unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        let runner = FakeRunner::new();
        expect_hostname(&runner);

        let report = create(&ctx(&runner), &paths, &archive, true, true).unwrap();
        let (manifest, entries) = backup_state::read_archive(&archive).unwrap();
        let entry = backup_state::archive_entry(&entries, "/etc/nclawzero/providers.d/legacy.json")
            .unwrap();

        assert_eq!(report.redacted_count, 0);
        assert!(!manifest.sources[0].redacted);
        assert_eq!(String::from_utf8(entry.contents.clone()).unwrap(), provider);
        runner.assert_done();
    }

    #[test]
    fn backup_create_quiesces_active_volume_unit_during_export() {
        let _guard = VOLUME_EXPORT_TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let archive = tmp.path().join("backup.tar.gz");
        let export_path = backup_volume_export_path("zeroclaw-data");
        let _ = fs::remove_file(&export_path);
        fs::write(&export_path, b"volume tar").unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            "podman",
            &["volume", "exists", "zeroclaw-data"],
            out(0, "", ""),
        );
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        expect_unit_state(&runner, "zeroclaw.service", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(0, "", ""),
        );
        expect_unit_state(&runner, "zeroclaw.service", "inactive", "dead");
        runner.expect(
            "podman",
            &[
                "volume",
                "export",
                "--output",
                &export_path.display().to_string(),
                "zeroclaw-data",
            ],
            out(0, "", ""),
        );
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );
        runner.expect(
            "podman",
            &["volume", "exists", "openclaw-data"],
            out(1, "", ""),
        );
        runner.expect(
            "podman",
            &["volume", "exists", "hermes-data"],
            out(1, "", ""),
        );
        expect_hostname(&runner);

        let report = create(&ctx(&runner), &paths, &archive, false, false).unwrap();

        assert_eq!(report.source_count, 1);
        assert!(!report.unsafe_live_volumes);
        assert!(!export_path.exists());
        runner.assert_done();
    }

    #[test]
    fn backup_create_with_unsafe_live_volumes_skips_quiesce() {
        let _guard = VOLUME_EXPORT_TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let archive = tmp.path().join("backup.tar.gz");
        let export_path = backup_volume_export_path("zeroclaw-data");
        let _ = fs::remove_file(&export_path);
        fs::write(&export_path, b"volume tar").unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            "podman",
            &["volume", "exists", "zeroclaw-data"],
            out(0, "", ""),
        );
        runner.expect(
            "podman",
            &[
                "volume",
                "export",
                "--output",
                &export_path.display().to_string(),
                "zeroclaw-data",
            ],
            out(0, "", ""),
        );
        runner.expect(
            "podman",
            &["volume", "exists", "openclaw-data"],
            out(1, "", ""),
        );
        runner.expect(
            "podman",
            &["volume", "exists", "hermes-data"],
            out(1, "", ""),
        );
        expect_hostname(&runner);

        let report =
            create_with_unsafe_live_volumes(&ctx(&runner), &paths, &archive, false, false, true)
                .unwrap();

        assert_eq!(report.source_count, 1);
        assert!(report.unsafe_live_volumes);
        assert!(!export_path.exists());
        runner.assert_done();
    }

    #[test]
    fn backup_manifest_persists_unsafe_live_volumes_for_verify() {
        let _guard = VOLUME_EXPORT_TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let archive = tmp.path().join("backup.tar.gz");
        let export_path = backup_volume_export_path("zeroclaw-data");
        let _ = fs::remove_file(&export_path);
        fs::write(&export_path, b"volume tar").unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            "podman",
            &["volume", "exists", "zeroclaw-data"],
            out(0, "", ""),
        );
        runner.expect(
            "podman",
            &[
                "volume",
                "export",
                "--output",
                &export_path.display().to_string(),
                "zeroclaw-data",
            ],
            out(0, "", ""),
        );
        runner.expect(
            "podman",
            &["volume", "exists", "openclaw-data"],
            out(1, "", ""),
        );
        runner.expect(
            "podman",
            &["volume", "exists", "hermes-data"],
            out(1, "", ""),
        );
        expect_hostname(&runner);

        let report =
            create_with_unsafe_live_volumes(&ctx(&runner), &paths, &archive, false, false, true)
                .unwrap();
        let (manifest, _) = backup_state::read_archive(&archive).unwrap();
        let verify_report = verify(&archive).unwrap();
        let mut verify_text = Vec::new();
        crate::output::Render::render_text(&verify_report, &mut verify_text).unwrap();
        let verify_text = String::from_utf8(verify_text).unwrap();

        assert!(report.unsafe_live_volumes);
        assert!(manifest.unsafe_live_volumes);
        assert!(verify_report.unsafe_live_volumes);
        assert!(verify_report.ok);
        assert!(verify_text.contains("unsafe live volumes: enabled\n"));
        assert!(!export_path.exists());
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
    fn restore_refuses_when_existing_provider_json_differs_unless_force() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("legacy.json"),
            br#"{"name":"old"}"#,
        )
        .unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(
            &archive,
            "/etc/nclawzero/providers.d/legacy.json",
            br#"{"name":"new"}"#,
            false,
        );
        let runner = FakeRunner::new();

        let err = restore(&ctx(&runner), &paths, &archive, false, false).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(
            fs::read(paths.providers_dir().join("legacy.json")).unwrap(),
            br#"{"name":"old"}"#
        );
        runner.assert_done();
    }

    #[test]
    fn restore_refuses_when_existing_volume_non_empty_unless_force() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let volume_dir = tmp.path().join("volume");
        fs::create_dir_all(&volume_dir).unwrap();
        fs::write(volume_dir.join("data.db"), b"existing").unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(
            &archive,
            "podman://volume/zeroclaw-data",
            b"tar contents",
            false,
        );
        let runner = FakeRunner::new();
        runner.expect(
            "podman",
            &["volume", "exists", "zeroclaw-data"],
            out(0, "", ""),
        );
        runner.expect(
            "podman",
            &[
                "volume",
                "inspect",
                "--format",
                "{{.Mountpoint}}",
                "zeroclaw-data",
            ],
            out(0, &format!("{}\n", volume_dir.display()), ""),
        );

        let err = restore(&ctx(&runner), &paths, &archive, false, false).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        runner.assert_done();
    }

    #[test]
    fn restore_with_force_overwrites_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("legacy.json"),
            br#"{"name":"old"}"#,
        )
        .unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(
            &archive,
            "/etc/nclawzero/providers.d/legacy.json",
            br#"{"name":"new"}"#,
            false,
        );
        let runner = FakeRunner::new();

        restore(&ctx(&runner), &paths, &archive, false, true).unwrap();

        assert_eq!(
            fs::read(paths.providers_dir().join("legacy.json")).unwrap(),
            br#"{"name":"new"}"#
        );
        runner.assert_done();
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
        assert!(!report.unsafe_live_volumes);
        assert_eq!(report.restored_count, 1);
        assert!(!paths.channel().exists());
    }

    #[test]
    fn restore_report_carries_unsafe_live_volumes_from_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let archive = tmp.path().join("backup.tar.gz");
        write_archive_with_unsafe_live_volumes(
            &archive,
            "podman://volume/zeroclaw-data",
            b"tar contents",
            false,
            true,
        );
        let runner = FakeRunner::new();

        let report = restore(&ctx(&runner), &paths, &archive, true, true).unwrap();
        let mut restore_text = Vec::new();
        crate::output::Render::render_text(&report, &mut restore_text).unwrap();
        let restore_text = String::from_utf8(restore_text).unwrap();

        assert!(report.dry_run);
        assert!(report.unsafe_live_volumes);
        assert_eq!(report.actions[0].action, "would-restore-volume");
        assert!(restore_text.contains("unsafe live volumes: enabled\n"));
        runner.assert_done();
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

    #[test]
    fn restore_skips_redacted_provider_json_does_not_overwrite_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("legacy.json"),
            br#"{"name":"legacy","api_key":"sk-live"}"#,
        )
        .unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(
            &archive,
            "/etc/nclawzero/providers.d/legacy.json",
            br#"{"name":"legacy","api_key":"REDACTED:api_key"}"#,
            true,
        );
        let runner = FakeRunner::new();

        let report = restore(&ctx(&runner), &paths, &archive, false, false).unwrap();

        assert_eq!(report.restored_count, 0);
        assert_eq!(
            report.skipped_redacted_provider_files,
            vec!["/etc/nclawzero/providers.d/legacy.json".to_string()]
        );
        assert_eq!(
            fs::read(paths.providers_dir().join("legacy.json")).unwrap(),
            br#"{"name":"legacy","api_key":"sk-live"}"#
        );
        runner.assert_done();
    }

    #[test]
    fn restore_force_does_not_overwrite_with_redacted_provider_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("legacy.json"),
            br#"{"name":"legacy","api_key":"sk-live"}"#,
        )
        .unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(
            &archive,
            "/etc/nclawzero/providers.d/legacy.json",
            br#"{"name":"legacy","api_key":"REDACTED:api_key"}"#,
            true,
        );
        let runner = FakeRunner::new();

        let report = restore(&ctx(&runner), &paths, &archive, false, true).unwrap();

        assert_eq!(report.restored_count, 0);
        assert_eq!(
            fs::read(paths.providers_dir().join("legacy.json")).unwrap(),
            br#"{"name":"legacy","api_key":"sk-live"}"#
        );
        runner.assert_done();
    }

    #[test]
    fn restore_dry_run_reports_redacted_skip_for_provider_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let archive = tmp.path().join("backup.tar.gz");
        write_archive(
            &archive,
            "/etc/nclawzero/providers.d/legacy.json",
            br#"{"name":"legacy","api_key":"REDACTED:api_key"}"#,
            true,
        );
        let runner = FakeRunner::new();

        let report = restore(&ctx(&runner), &paths, &archive, true, false).unwrap();

        assert_eq!(report.restored_count, 0);
        assert_eq!(report.actions[0].action, "would_skip_redacted");
        assert_eq!(
            report.skipped_redacted_provider_files,
            vec!["/etc/nclawzero/providers.d/legacy.json".to_string()]
        );
        assert!(!paths.providers_dir().join("legacy.json").exists());
        runner.assert_done();
    }

    #[test]
    fn restore_staging_dir_is_cleaned_up_after_completion() {
        let staging_path = {
            let staging = restore_staging_dir().unwrap();
            let staging_path = staging.path().to_path_buf();
            assert!(staging_path.exists());
            staging_path
        };

        assert!(!staging_path.exists());
    }

    #[test]
    fn restore_staging_files_have_0600_mode_for_agent_env() {
        let tmp = tempfile::tempdir().unwrap();

        write_staging_file(
            tmp.path(),
            backup_state::AGENT_ENV_PATH,
            b"OPENAI_API_KEY=sk-live\n",
        )
        .unwrap();

        let path = tmp.path().join("etc/nclawzero/agent-env");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn restore_volume_leaves_inactive_unit_inactive_after_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(3, "", ""),
        );
        expect_unit_state(&runner, "zeroclaw.service", "inactive", "dead");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(0, "", ""),
        );
        expect_unit_state(&runner, "zeroclaw.service", "inactive", "dead");
        runner.expect_prefix(
            "podman",
            &["volume", "import", "zeroclaw-data"],
            out(0, "", ""),
        );

        restore_volume(&ctx(&runner), tmp.path(), "zeroclaw-data", b"tar").unwrap();

        runner.assert_done();
    }

    #[test]
    fn restore_volume_restarts_active_unit_even_on_import_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            "systemctl",
            &["is-active", "--quiet", "zeroclaw.service"],
            out(0, "", ""),
        );
        expect_unit_state(&runner, "zeroclaw.service", "active", "running");
        runner.expect(
            "sudo",
            &["systemctl", "stop", "zeroclaw.service"],
            out(0, "", ""),
        );
        expect_unit_state(&runner, "zeroclaw.service", "inactive", "dead");
        runner.expect_prefix(
            "podman",
            &["volume", "import", "zeroclaw-data"],
            out(1, "", "import failed"),
        );
        runner.expect(
            "sudo",
            &["systemctl", "start", "zeroclaw.service"],
            out(0, "", ""),
        );

        let err = restore_volume(&ctx(&runner), tmp.path(), "zeroclaw-data", b"tar").unwrap_err();

        assert!(matches!(err, NczError::Exec { .. }));
        runner.assert_done();
    }
}
