//! Provider state for `/etc/nclawzero/providers.d/*.json`.
//!
//! JSON declarations are canonical in v0.2. Legacy `.env`/`.conf` files are
//! parsed in place for compatibility; reads never move inline secrets into the
//! shared agent environment.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::NczError;
use crate::state::{self, agent_env, url as url_state, Paths};

pub const OPENAI_COMPAT_PROVIDER_TYPE: &str = "openai-compat";
const DEFAULT_PROVIDER_TYPE: &str = OPENAI_COMPAT_PROVIDER_TYPE;
const DEFAULT_HEALTH_PATH: &str = "/health";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderDeclaration {
    pub schema_version: u32,
    pub name: String,
    pub url: String,
    pub model: String,
    pub key_env: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub health_path: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelDeclaration {
    pub id: String,
    pub context_length: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ProviderRecord {
    pub declaration: ProviderDeclaration,
    pub path: PathBuf,
    pub inline_secret: Option<String>,
    pub unmigratable_secret_field: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderModelCache {
    pub schema_version: u32,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_fingerprint: Option<String>,
    pub fetched_at: String,
    pub models: Vec<ModelDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineCredentialReplacement {
    pub file: String,
    pub key_env: String,
    pub secret: String,
}

#[derive(Debug)]
struct ParsedProvider {
    declaration: ProviderDeclaration,
    inline_secret: Option<String>,
    unmigratable_secret_field: Option<String>,
    unsupported_legacy_field: Option<String>,
}

struct JsonSecretCandidate {
    path: String,
    key: String,
    value: Option<String>,
    migratable: bool,
}

struct LegacyJsonSecret {
    credential: Option<(String, String)>,
    unmigratable_field: Option<String>,
}

struct LegacyEnvSecret {
    credential: Option<(String, String)>,
    unmigratable_field: Option<String>,
    selected_field: Option<String>,
}

struct EnvSecretCandidate {
    raw_key: String,
    normalized_key: String,
    value: String,
    known_legacy: bool,
}

pub fn read_primary(paths: &Paths) -> Result<Option<String>, NczError> {
    match fs::read_to_string(paths.primary_provider()) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(NczError::Io(e)),
    }
}

pub fn write_primary(paths: &Paths, name: &str) -> Result<(), NczError> {
    let body = format!("{name}\n");
    crate::state::atomic_write(&paths.primary_provider(), body.as_bytes(), 0o644)
}

pub fn read_all(paths: &Paths) -> Result<Vec<ProviderRecord>, NczError> {
    let mut records = Vec::new();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    let files = provider_files(paths)?;
    preflight_provider_files(&files)?;
    for path in files {
        let Some(record) = read_record(paths, &path)? else {
            continue;
        };
        if let Some(previous) = seen.get(&record.declaration.name) {
            if records.iter().any(|existing: &ProviderRecord| {
                existing.declaration.name == record.declaration.name
                    && legacy_collision_is_equivalent(&existing.declaration, &record.declaration)
            }) {
                continue;
            }
            return Err(NczError::Precondition(format!(
                "duplicate provider declaration {} in {} and {}",
                record.declaration.name,
                previous.display(),
                record.path.display()
            )));
        }
        seen.insert(record.declaration.name.clone(), record.path.clone());
        records.push(record);
    }
    records.sort_by(|a, b| a.declaration.name.cmp(&b.declaration.name));
    Ok(records)
}

pub fn migrate_legacy(paths: &Paths) -> Result<Vec<PathBuf>, NczError> {
    recover_legacy_migration_journals(paths)?;
    let migrations = legacy_migrations(paths)?;
    migrate_legacy_records(paths, migrations)
}

pub fn migrate_legacy_for_provider(paths: &Paths, name: &str) -> Result<Vec<PathBuf>, NczError> {
    recover_legacy_migration_journals(paths)?;
    let migrations = legacy_migrations_for_provider(paths, name)?;
    migrate_legacy_records(paths, migrations)
}

pub fn migrate_legacy_for_providers(
    paths: &Paths,
    names: &[String],
) -> Result<Vec<PathBuf>, NczError> {
    recover_legacy_migration_journals(paths)?;
    let mut migrations = Vec::new();
    for name in names {
        migrations.extend(legacy_migrations_for_provider(paths, name)?);
    }
    migrations.sort_by(|a, b| a.path.cmp(&b.path));
    migrations.dedup_by(|a, b| a.path == b.path);
    migrate_legacy_records(paths, migrations)
}

pub fn validate_legacy_migration_for_provider(paths: &Paths, name: &str) -> Result<(), NczError> {
    let migrations = legacy_migrations_for_provider(paths, name)?;
    validate_legacy_migration_candidates(paths, &migrations)
}

fn migrate_legacy_records(
    paths: &Paths,
    migrations: Vec<LegacyMigration>,
) -> Result<Vec<PathBuf>, NczError> {
    if migrations.is_empty() {
        return Ok(Vec::new());
    }

    let mut snapshot_paths = vec![paths.agent_env()];
    snapshot_paths.extend(legacy_migration_file_paths(&migrations));
    let snapshots = snapshot_files(&snapshot_paths)?;
    let result = validate_legacy_migration_candidates(paths, &migrations)
        .and_then(|()| migrate_legacy_inner(paths, &migrations));
    if let Err(err) = result {
        restore_file_snapshots(&snapshots)?;
        return Err(err);
    }

    let mut migrated: Vec<PathBuf> = migrations
        .into_iter()
        .map(|migration| migration.canonical_path)
        .collect();
    migrated.sort();
    migrated.dedup();
    Ok(migrated)
}

fn validate_legacy_migration_candidates(
    paths: &Paths,
    migrations: &[LegacyMigration],
) -> Result<(), NczError> {
    let mut seen: BTreeMap<String, (ProviderDeclaration, Option<String>, PathBuf)> =
        BTreeMap::new();
    for migration in migrations {
        if let Some(field) = &migration.unsupported_legacy_field {
            return Err(unsupported_legacy_field(&migration.path, field));
        }

        if let Some(secret) = &migration.inline_secret {
            if let Some(existing) = agent_env_value(paths, &migration.declaration.key_env)? {
                if existing != *secret {
                    return Err(NczError::Precondition(format!(
                        "legacy provider {} inline credential conflicts with existing {}; leaving legacy provider file in place",
                        migration.path.display(),
                        migration.declaration.key_env
                    )));
                }
            }
            reject_conflicting_legacy_binding(paths, migration)?;
        }

        if let Some((previous, previous_secret, previous_path)) =
            seen.get(&migration.declaration.name)
        {
            if !legacy_collision_is_equivalent(previous, &migration.declaration) {
                return Err(NczError::Precondition(format!(
                    "legacy provider {} conflicts with another declaration for {}",
                    migration.path.display(),
                    migration.declaration.name
                )));
            }
            if previous_secret != &migration.inline_secret {
                return Err(NczError::Precondition(format!(
                    "legacy provider {} inline credential conflicts with another declaration for {} in {}; leaving legacy provider files in place",
                    migration.path.display(),
                    migration.declaration.name,
                    previous_path.display()
                )));
            }
        } else if migration.canonical_path.exists() && migration.canonical_path != migration.path {
            let body = fs::read_to_string(&migration.canonical_path)?;
            let canonical_name = migration
                .canonical_path
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or(&migration.declaration.name)
                .to_string();
            let canonical = parse_json(&body, canonical_name)?;
            validate_declaration(&canonical)?;
            if !legacy_collision_is_equivalent(&migration.declaration, &canonical) {
                return Err(NczError::Precondition(format!(
                    "legacy provider {} conflicts with {}; leaving legacy provider file in place",
                    migration.path.display(),
                    migration.canonical_path.display()
                )));
            }
        }

        seen.entry(migration.declaration.name.clone()).or_insert_with(|| {
            (
                migration.declaration.clone(),
                migration.inline_secret.clone(),
                migration.path.clone(),
            )
        });
    }
    Ok(())
}

pub fn legacy_migration_snapshot_paths(paths: &Paths) -> Result<Vec<PathBuf>, NczError> {
    Ok(legacy_migration_file_paths(&legacy_migrations(paths)?))
}

pub fn legacy_migration_snapshot_paths_for_provider(
    paths: &Paths,
    name: &str,
) -> Result<Vec<PathBuf>, NczError> {
    Ok(legacy_migration_file_paths(&legacy_migrations_for_provider(
        paths, name,
    )?))
}

pub fn legacy_migration_snapshot_paths_for_providers(
    paths: &Paths,
    names: &[String],
) -> Result<Vec<PathBuf>, NczError> {
    let mut migrations = Vec::new();
    for name in names {
        migrations.extend(legacy_migrations_for_provider(paths, name)?);
    }
    migrations.sort_by(|a, b| a.path.cmp(&b.path));
    migrations.dedup_by(|a, b| a.path == b.path);
    Ok(legacy_migration_file_paths(&migrations))
}

struct LegacyMigration {
    path: PathBuf,
    legacy_fingerprint: String,
    declaration: ProviderDeclaration,
    inline_secret: Option<String>,
    unsupported_legacy_field: Option<String>,
    canonical_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyMigrationJournal {
    schema_version: u32,
    legacy_path: PathBuf,
    legacy_fingerprint: String,
    canonical_path: PathBuf,
    declaration: ProviderDeclaration,
    secret: Option<String>,
}

fn legacy_migrations(paths: &Paths) -> Result<Vec<LegacyMigration>, NczError> {
    let files = provider_files(paths)?;
    preflight_provider_files(&files)?;

    let mut migrations = Vec::new();
    for path in files {
        let body = match fs::read_to_string(&path) {
            Ok(body) => body,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(e) => return Err(NczError::Io(e)),
        };
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let parsed = if ext == "json" {
            let value: Value = serde_json::from_str(&body)?;
            if value.get("schema_version").is_some() {
                continue;
            }
            parse_legacy_json(value, fallback_name)?
        } else {
            parse_env(&body, fallback_name)?
        };
        if let Some(field) = &parsed.unmigratable_secret_field {
            return Err(unmigratable_legacy_secret(field));
        }
        validate_declaration(&parsed.declaration).map_err(|err| {
            NczError::Precondition(format!("invalid legacy provider {}: {err}", path.display()))
        })?;
        let canonical_path = declaration_path(paths, &parsed.declaration.name)?;
        migrations.push(LegacyMigration {
            path,
            legacy_fingerprint: legacy_file_fingerprint(&body),
            declaration: parsed.declaration,
            inline_secret: parsed.inline_secret,
            unsupported_legacy_field: parsed.unsupported_legacy_field,
            canonical_path,
        });
    }
    Ok(migrations)
}

fn legacy_migrations_for_provider(
    paths: &Paths,
    name: &str,
) -> Result<Vec<LegacyMigration>, NczError> {
    validate_name(name)?;
    let mut migrations = Vec::new();
    for path in matching_provider_files(paths, name)? {
        let body = fs::read_to_string(&path)?;
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let parsed = if ext == "json" {
            let value: Value = serde_json::from_str(&body)?;
            if value.get("schema_version").is_some() {
                continue;
            }
            parse_legacy_json(value, fallback_name)?
        } else {
            parse_env(&body, fallback_name)?
        };
        if let Some(field) = &parsed.unmigratable_secret_field {
            return Err(unmigratable_legacy_secret(field));
        }
        validate_declaration(&parsed.declaration).map_err(|err| {
            NczError::Precondition(format!("invalid legacy provider {}: {err}", path.display()))
        })?;
        migrations.push(LegacyMigration {
            canonical_path: declaration_path(paths, &parsed.declaration.name)?,
            path,
            legacy_fingerprint: legacy_file_fingerprint(&body),
            declaration: parsed.declaration,
            inline_secret: parsed.inline_secret,
            unsupported_legacy_field: parsed.unsupported_legacy_field,
        });
    }
    Ok(migrations)
}

fn legacy_migration_file_paths(migrations: &[LegacyMigration]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for migration in migrations {
        paths.push(migration.path.clone());
        paths.push(migration.canonical_path.clone());
        paths.push(legacy_migration_journal_path_for(paths_dir_for(&migration.canonical_path), &migration.declaration.name));
    }
    paths.sort();
    paths.dedup();
    paths
}

fn paths_dir_for(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

fn legacy_migration_journal_path(paths: &Paths, name: &str) -> PathBuf {
    legacy_migration_journal_path_for(&paths.providers_dir(), name)
}

fn legacy_migration_journal_path_for(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!(".ncz-migrate-{}.journal", journal_name_hex(name)))
}

fn journal_name_hex(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(name.len() * 2);
    for byte in name.as_bytes() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn legacy_file_fingerprint(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    format!("sha256:{}", hex_encode(&digest))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn write_legacy_migration_journal(
    paths: &Paths,
    migration: &LegacyMigration,
) -> Result<PathBuf, NczError> {
    let path = legacy_migration_journal_path(paths, &migration.declaration.name);
    let journal = LegacyMigrationJournal {
        schema_version: 1,
        legacy_path: migration.path.clone(),
        legacy_fingerprint: migration.legacy_fingerprint.clone(),
        canonical_path: migration.canonical_path.clone(),
        declaration: migration.declaration.clone(),
        secret: migration.inline_secret.clone(),
    };
    let mut body = serde_json::to_vec_pretty(&journal)?;
    body.push(b'\n');
    state::atomic_write(&path, &body, 0o600)?;
    Ok(path)
}

fn recover_legacy_migration_journals(paths: &Paths) -> Result<(), NczError> {
    for path in legacy_migration_journal_paths(paths)? {
        recover_legacy_migration_journal(paths, &path)?;
    }
    Ok(())
}

fn legacy_migration_journal_paths(paths: &Paths) -> Result<Vec<PathBuf>, NczError> {
    let mut journals = Vec::new();
    match fs::read_dir(paths.providers_dir()) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if name.starts_with(".ncz-migrate-") && name.ends_with(".journal") {
                    journals.push(entry.path());
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(NczError::Io(e)),
    }
    journals.sort();
    Ok(journals)
}

fn recover_legacy_migration_journal(paths: &Paths, path: &Path) -> Result<(), NczError> {
    let body = fs::read_to_string(path)?;
    let journal: LegacyMigrationJournal = serde_json::from_str(&body)?;
    if journal.schema_version != 1 {
        return Err(NczError::Precondition(format!(
            "unsupported legacy provider migration journal schema in {}",
            path.display()
        )));
    }
    validate_declaration(&journal.declaration)?;
    let expected_journal = legacy_migration_journal_path(paths, &journal.declaration.name);
    if path != expected_journal {
        return Err(NczError::Precondition(format!(
            "legacy provider migration journal {} does not match provider {}",
            path.display(),
            journal.declaration.name
        )));
    }
    let expected_canonical = declaration_path(paths, &journal.declaration.name)?;
    if journal.canonical_path != expected_canonical
        || journal.legacy_path.parent() != Some(paths.providers_dir().as_path())
    {
        return Err(NczError::Precondition(format!(
            "legacy provider migration journal {} contains unexpected paths",
            path.display()
        )));
    }
    reject_changed_legacy_journal_source(path, &journal)?;
    reject_conflicting_legacy_journal_recovery(paths, path, &journal)?;
    write_declaration(&journal.canonical_path, &journal.declaration)?;
    if journal.legacy_path.exists() {
        state::remove_file_durable(&journal.legacy_path)?;
    }
    if let Some(secret) = &journal.secret {
        agent_env::set(paths, &journal.declaration.key_env, secret)?;
    }
    agent_env::set_provider_binding(
        paths,
        &journal.declaration.name,
        &journal.declaration.key_env,
        &journal.declaration.url,
    )?;
    state::remove_file_durable(path)
}

fn reject_changed_legacy_journal_source(
    journal_path: &Path,
    journal: &LegacyMigrationJournal,
) -> Result<(), NczError> {
    match fs::read_to_string(&journal.legacy_path) {
        Ok(body) => {
            let current = legacy_file_fingerprint(&body);
            if current != journal.legacy_fingerprint {
                return Err(NczError::Precondition(format!(
                    "legacy provider migration journal {} no longer matches {}; leaving journal and legacy provider file in place for manual recovery",
                    journal_path.display(),
                    journal.legacy_path.display()
                )));
            }
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) => {}
        Err(e) => return Err(NczError::Io(e)),
    }
    Ok(())
}

fn reject_conflicting_legacy_journal_recovery(
    paths: &Paths,
    journal_path: &Path,
    journal: &LegacyMigrationJournal,
) -> Result<(), NczError> {
    let Some(secret) = &journal.secret else {
        return Err(NczError::Precondition(format!(
            "legacy provider migration journal {} is missing its inline credential; leaving journal in place for manual recovery",
            journal_path.display()
        )));
    };
    if let Some(existing) = read_record(paths, &journal.canonical_path)? {
        if existing.declaration != journal.declaration {
            return Err(NczError::Precondition(format!(
                "legacy provider migration journal {} conflicts with existing provider declaration {}; leaving journal in place for manual recovery",
                journal_path.display(),
                existing.path.display()
            )));
        }
        if existing
            .inline_secret
            .as_deref()
            .is_some_and(|inline_secret| inline_secret != secret.as_str())
        {
            return Err(NczError::Precondition(format!(
                "legacy provider migration journal {} conflicts with existing inline credential in {}; leaving journal in place for manual recovery",
                journal_path.display(),
                existing.path.display()
            )));
        }
    }
    if let Some(existing) = agent_env_value(paths, &journal.declaration.key_env)? {
        if existing != secret.as_str() {
            return Err(NczError::Precondition(format!(
                "legacy provider migration journal {} conflicts with existing {}; leaving journal in place for manual recovery",
                journal_path.display(),
                journal.declaration.key_env
            )));
        }
    }
    let entries = agent_env::read(paths)?;
    if agent_env::provider_binding_exists(&entries, &journal.declaration.name)?
        && !agent_env::provider_binding_matches(
            &entries,
            &journal.declaration.name,
            &journal.declaration.key_env,
            &journal.declaration.url,
        )?
    {
        return Err(NczError::Precondition(format!(
            "legacy provider migration journal {} conflicts with the existing provider binding for {}; leaving journal in place for manual recovery",
            journal_path.display(),
            journal.declaration.name
        )));
    }
    Ok(())
}

fn migrate_legacy_inner(paths: &Paths, migrations: &[LegacyMigration]) -> Result<(), NczError> {
    let mut canonical_declarations: BTreeMap<String, ProviderDeclaration> = BTreeMap::new();
    for migration in migrations {
        let mut needs_secret_write = false;
        if let Some(secret) = &migration.inline_secret {
            reject_conflicting_legacy_binding(paths, migration)?;
            match agent_env_value(paths, &migration.declaration.key_env)? {
                Some(existing) if existing == *secret => {
                    needs_secret_write = false;
                }
                Some(_) => {
                    return Err(NczError::Precondition(format!(
                        "legacy provider {} inline credential conflicts with existing {}; leaving legacy provider file in place",
                        migration.path.display(),
                        migration.declaration.key_env
                    )));
                }
                None => {
                    needs_secret_write = true;
                }
            }
        }
        let mut write_canonical = false;
        if let Some(existing) = canonical_declarations.get(&migration.declaration.name) {
            if !legacy_collision_is_equivalent(existing, &migration.declaration) {
                return Err(NczError::Precondition(format!(
                    "legacy provider {} conflicts with another declaration for {}",
                    migration.path.display(),
                    migration.declaration.name
                )));
            }
        } else if migration.canonical_path.exists() && migration.canonical_path != migration.path {
            let body = fs::read_to_string(&migration.canonical_path)?;
            let canonical_name = migration
                .canonical_path
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or(&migration.declaration.name)
                .to_string();
            let canonical = parse_json(&body, canonical_name)?;
            validate_declaration(&canonical)?;
            if !legacy_collision_is_equivalent(&migration.declaration, &canonical) {
                return Err(NczError::Precondition(format!(
                    "legacy provider {} conflicts with {}; leaving legacy provider file in place",
                    migration.path.display(),
                    migration.canonical_path.display()
                )));
            }
            canonical_declarations.insert(migration.declaration.name.clone(), canonical);
        } else {
            write_canonical = true;
            canonical_declarations.insert(
                migration.declaration.name.clone(),
                migration.declaration.clone(),
            );
        }

        let journal_path = if migration.inline_secret.is_some() {
            Some(write_legacy_migration_journal(paths, migration)?)
        } else {
            None
        };

        if write_canonical {
            write_declaration(&migration.canonical_path, &migration.declaration)?;
        }
        if migration.path != migration.canonical_path {
            state::remove_file_durable(&migration.path)?;
        }
        if let Some(secret) = &migration.inline_secret {
            if needs_secret_write {
                agent_env::set(paths, &migration.declaration.key_env, secret)?;
            }
            agent_env::set_provider_binding(
                paths,
                &migration.declaration.name,
                &migration.declaration.key_env,
                &migration.declaration.url,
            )?;
        }
        if let Some(path) = journal_path {
            state::remove_file_durable(&path)?;
        }
    }
    Ok(())
}

fn reject_conflicting_legacy_binding(
    paths: &Paths,
    migration: &LegacyMigration,
) -> Result<(), NczError> {
    let provider = &migration.declaration;
    let entries = agent_env::read(paths)?;
    if !agent_env::provider_binding_exists(&entries, &provider.name)? {
        return Ok(());
    }
    if agent_env::provider_binding_matches(&entries, &provider.name, &provider.key_env, &provider.url)?
    {
        return Ok(());
    }
    Err(NczError::Precondition(format!(
        "legacy provider {} has an inline credential, but provider {} already has a binding for a different key or URL; run `ncz api set {} --providers={}` to approve {}",
        migration.path.display(),
        provider.name,
        provider.key_env,
        provider.name,
        provider.url
    )))
}

fn agent_env_value(paths: &Paths, key: &str) -> Result<Option<String>, NczError> {
    Ok(agent_env::read(paths)?
        .into_iter()
        .find(|entry| entry.key == key && !entry.value.is_empty())
        .map(|entry| entry.value))
}

fn preflight_provider_files(files: &[PathBuf]) -> Result<(), NczError> {
    let mut parsed = Vec::new();
    for path in files {
        let body = match fs::read_to_string(path) {
            Ok(body) => body,
            Err(e) => {
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) {
                    continue;
                }
                return Err(NczError::Io(e));
            }
        };
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let canonical_json = ext == "json" && json_has_schema_version(&body)?;
        let declaration = parse_declaration_for_path(&body, &fallback_name, ext)?;
        validate_declaration(&declaration).map_err(|err| {
            NczError::Precondition(format!(
                "invalid provider declaration {}: {err}",
                path.display()
            ))
        })?;
        if canonical_json && fallback_name != declaration.name {
            return Err(NczError::Precondition(format!(
                "provider declaration filename {} does not match declared name {}",
                path.display(),
                declaration.name
            )));
        }
        parsed.push((path.clone(), ext.to_string(), declaration));
    }

    let mut seen: BTreeMap<String, (PathBuf, ProviderDeclaration)> = BTreeMap::new();
    for (path, _ext, declaration) in parsed {
        if let Some((previous_path, previous)) = seen.insert(
            declaration.name.clone(),
            (path.clone(), declaration.clone()),
        ) {
            if !legacy_collision_is_equivalent(&previous, &declaration) {
                return Err(NczError::Precondition(format!(
                    "duplicate provider declaration {} in {} and {}",
                    declaration.name,
                    previous_path.display(),
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

pub fn read(paths: &Paths, name: &str) -> Result<Option<ProviderRecord>, NczError> {
    validate_name(name)?;
    let mut records = Vec::new();
    for path in matching_provider_files(paths, name)? {
        let Some(record) = read_record(paths, &path)? else {
            continue;
        };
        records.push(record);
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    if records.len() > 1 {
        let first = &records[0];
        for record in records.iter().skip(1) {
            if !legacy_collision_is_equivalent(&first.declaration, &record.declaration) {
                return Err(NczError::Precondition(format!(
                    "duplicate provider declaration {} in {} and {}",
                    name,
                    first.path.display(),
                    record.path.display()
                )));
            }
        }
    }
    Ok(records.into_iter().next())
}

pub fn read_canonical(paths: &Paths, name: &str) -> Result<Option<ProviderRecord>, NczError> {
    let path = declaration_path(paths, name)?;
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(NczError::Io(e)),
    };
    let declaration = parse_json(&body, name.to_string())?;
    validate_declaration(&declaration)?;
    if declaration.name != name {
        return Err(NczError::Precondition(format!(
            "provider declaration filename {} does not match declared name {}",
            path.display(),
            declaration.name
        )));
    }
    Ok(Some(ProviderRecord {
        declaration,
        path,
        inline_secret: None,
        unmigratable_secret_field: None,
    }))
}

pub fn exists_without_migration(paths: &Paths, name: &str) -> Result<bool, NczError> {
    validate_name(name)?;
    for path in provider_files(paths)? {
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        if fallback_name == name {
            return Ok(true);
        }
        let body = fs::read_to_string(&path)?;
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let declaration = match parse_declaration_for_path(&body, &fallback_name, ext) {
            Ok(declaration) => declaration,
            Err(_) => continue,
        };
        if declaration.name == name {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn write(
    paths: &Paths,
    declaration: &ProviderDeclaration,
    force: bool,
) -> Result<PathBuf, NczError> {
    validate_declaration(declaration)?;
    let path = declaration_path(paths, &declaration.name)?;
    let mut conflicts = matching_provider_files(paths, &declaration.name)?;
    if path.exists() && !conflicts.iter().any(|candidate| candidate == &path) {
        conflicts.push(path.clone());
    }
    conflicts.sort();
    conflicts.dedup();
    if !conflicts.is_empty() && !force {
        return Err(NczError::Usage(format!(
            "provider declaration already exists: {} (use --force to replace)",
            declaration.name
        )));
    }
    let cache_path = model_cache_path(paths, &declaration.name)?;
    let mut snapshot_paths = conflicts.clone();
    snapshot_paths.push(path.clone());
    snapshot_paths.push(cache_path.clone());
    snapshot_paths.sort();
    snapshot_paths.dedup();
    let snapshots = snapshot_files(&snapshot_paths)?;
    let result = (|| {
        write_declaration(&path, declaration)?;
        if force {
            for conflict in conflicts.iter().filter(|conflict| *conflict != &path) {
                state::remove_file_durable(conflict)?;
            }
            state::remove_file_durable(&cache_path)?;
        }
        Ok(())
    })();
    if let Err(err) = result {
        restore_file_snapshots(&snapshots)?;
        return Err(err);
    }
    if force {
        state::remove_file_durable(&cache_path)?;
    }
    Ok(path)
}

pub fn remove(paths: &Paths, name: &str) -> Result<bool, NczError> {
    validate_name(name)?;
    let mut removed = false;
    let mut aliases = BTreeSet::from([name.to_string()]);
    let mut removal_paths = Vec::new();
    for path in provider_files(paths)? {
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        let body = fs::read_to_string(&path)?;
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let declaration = parse_declaration_for_path(&body, &fallback_name, ext);
        if fallback_name == name {
            aliases.insert(fallback_name);
            if let Ok(declaration) = declaration {
                if validate_name(&declaration.name).is_ok() {
                    aliases.insert(declaration.name);
                }
            }
            removed = true;
            removal_paths.push(path);
            continue;
        }
        let declaration = match declaration {
            Ok(declaration) => declaration,
            Err(_) => continue,
        };
        if declaration.name == name {
            if validate_name(&fallback_name).is_ok() {
                aliases.insert(fallback_name);
            }
            if validate_name(&declaration.name).is_ok() {
                aliases.insert(declaration.name);
            }
            removed = true;
            removal_paths.push(path);
        }
    }
    for alias in aliases {
        removal_paths.push(model_cache_path(paths, &alias)?);
    }
    removal_paths.sort();
    removal_paths.dedup();
    let snapshots = snapshot_files(&removal_paths)?;
    let result = (|| {
        for path in &removal_paths {
            state::remove_file_durable(path)?;
        }
        Ok(())
    })();
    if let Err(err) = result {
        restore_file_snapshots(&snapshots)?;
        return Err(err);
    }
    Ok(removed)
}

pub fn removal_aliases(paths: &Paths, name: &str) -> Result<BTreeSet<String>, NczError> {
    validate_name(name)?;
    let mut aliases = BTreeSet::from([name.to_string()]);
    for path in provider_files(paths)? {
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        let body = fs::read_to_string(&path)?;
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let declaration = parse_declaration_for_path(&body, &fallback_name, ext);
        if fallback_name == name {
            aliases.insert(fallback_name);
            if let Ok(declaration) = declaration {
                if validate_name(&declaration.name).is_ok() {
                    aliases.insert(declaration.name);
                }
            }
            continue;
        }
        let declaration = match declaration {
            Ok(declaration) => declaration,
            Err(_) => continue,
        };
        if declaration.name == name {
            if validate_name(&fallback_name).is_ok() {
                aliases.insert(fallback_name);
            }
            if validate_name(&declaration.name).is_ok() {
                aliases.insert(declaration.name);
            }
        }
    }
    Ok(aliases)
}

pub fn secret_bearing_replacement_files(
    paths: &Paths,
    name: &str,
) -> Result<Vec<String>, NczError> {
    Ok(inline_credential_replacements(paths, name)?
        .into_iter()
        .map(|replacement| replacement.file)
        .collect())
}

pub fn inline_credential_replacements(
    paths: &Paths,
    name: &str,
) -> Result<Vec<InlineCredentialReplacement>, NczError> {
    let mut replacements = Vec::new();
    for path in matching_provider_files(paths, name)? {
        let body = fs::read_to_string(&path)?;
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or(name)
            .to_string();
        if let Some((key_env, secret)) = inline_credential_for_path(&body, ext, &fallback_name)? {
            replacements.push(InlineCredentialReplacement {
                file: path.display().to_string(),
                key_env,
                secret,
            });
        }
    }
    Ok(replacements)
}

pub fn credential_references(paths: &Paths, key: &str) -> Result<Vec<String>, NczError> {
    agent_env::validate_public_key(key)?;
    let mut references = Vec::new();
    for path in provider_files(paths)? {
        let body = match fs::read_to_string(&path) {
            Ok(body) => body,
            Err(e) => {
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) {
                    continue;
                }
                return Err(NczError::Io(e));
            }
        };
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let declaration = parse_declaration_for_path(&body, &fallback_name, ext).map_err(|err| {
            NczError::Precondition(format!(
                "cannot inspect provider credential reference in {}: {err}",
                path.display()
            ))
        })?;
        if declaration.key_env == key {
            references.push(declaration.name);
        }
    }
    references.sort();
    references.dedup();
    Ok(references)
}

pub fn declaration_path(paths: &Paths, name: &str) -> Result<PathBuf, NczError> {
    validate_name(name)?;
    Ok(paths.providers_dir().join(format!("{name}.json")))
}

pub fn model_cache_path(paths: &Paths, name: &str) -> Result<PathBuf, NczError> {
    validate_name(name)?;
    Ok(paths.providers_dir().join(format!("{name}.models.json")))
}

pub fn read_model_cache(paths: &Paths, name: &str) -> Result<Option<ProviderModelCache>, NczError> {
    let path = model_cache_path(paths, name)?;
    match fs::read_to_string(path) {
        Ok(body) => {
            let cache: ProviderModelCache = serde_json::from_str(&body)?;
            if cache.schema_version != 1 || cache.provider != name {
                return Ok(None);
            }
            Ok(Some(cache))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(NczError::Io(e)),
    }
}

pub fn read_model_cache_for_provider(
    paths: &Paths,
    declaration: &ProviderDeclaration,
    credential_fingerprint: Option<&str>,
) -> Result<Option<ProviderModelCache>, NczError> {
    let Some(cache) = read_model_cache(paths, &declaration.name)? else {
        return Ok(None);
    };
    match &cache.provider_fingerprint {
        Some(fingerprint) if fingerprint == &provider_cache_fingerprint(declaration)? => {}
        _ => return Ok(None),
    }
    match (
        cache.credential_fingerprint.as_deref(),
        credential_fingerprint,
    ) {
        (Some(cached), Some(current)) if cached == current => {}
        (Some(_), _) => return Ok(None),
        (None, Some(_)) => return Ok(None),
        (None, None) => {}
    }
    Ok(Some(cache))
}

pub fn read_model_cache_for_provider_with_unavailable_credential(
    paths: &Paths,
    declaration: &ProviderDeclaration,
) -> Result<Option<ProviderModelCache>, NczError> {
    read_model_cache_for_provider(paths, declaration, None)
}

pub fn write_model_cache(paths: &Paths, cache: &ProviderModelCache) -> Result<PathBuf, NczError> {
    validate_name(&cache.provider)?;
    let path = model_cache_path(paths, &cache.provider)?;
    let mut body = serde_json::to_vec_pretty(cache)?;
    body.push(b'\n');
    state::atomic_write(&path, &body, 0o600)?;
    Ok(path)
}

pub fn remove_model_caches(paths: &Paths, providers: &[String]) -> Result<Vec<String>, NczError> {
    let mut removed = Vec::new();
    let mut names = providers.to_vec();
    names.sort();
    names.dedup();
    for provider in names {
        let path = model_cache_path(paths, &provider)?;
        let existed = path.exists();
        state::remove_file_durable(&path)?;
        if existed {
            removed.push(provider);
        }
    }
    Ok(removed)
}

pub fn provider_cache_fingerprint(declaration: &ProviderDeclaration) -> Result<String, NczError> {
    Ok(serde_json::to_string(&json!({
        "schema_version": declaration.schema_version,
        "name": declaration.name,
        "url": declaration.url,
        "model": declaration.model,
        "key_env": declaration.key_env,
        "type": declaration.provider_type,
        "health_path": declaration.health_path,
    }))?)
}

pub fn validate_declaration(declaration: &ProviderDeclaration) -> Result<(), NczError> {
    if declaration.schema_version != 1 {
        return Err(NczError::Usage(format!(
            "unsupported provider schema_version: {}",
            declaration.schema_version
        )));
    }
    reject_surrounding_whitespace("provider name", &declaration.name)?;
    reject_surrounding_whitespace("provider --url", &declaration.url)?;
    reject_surrounding_whitespace("provider --model", &declaration.model)?;
    reject_surrounding_whitespace("provider --key-env", &declaration.key_env)?;
    reject_surrounding_whitespace("provider --type", &declaration.provider_type)?;
    reject_surrounding_whitespace("provider --health-path", &declaration.health_path)?;
    validate_name(&declaration.name)?;
    if declaration.url.trim().is_empty() {
        return Err(NczError::Usage(
            "provider --url cannot be empty".to_string(),
        ));
    }
    validate_provider_url(&declaration.url)?;
    if declaration.model.trim().is_empty() {
        return Err(NczError::Usage(
            "provider --model cannot be empty".to_string(),
        ));
    }
    agent_env::validate_public_key(&declaration.key_env)?;
    let provider_type = declaration.provider_type.trim();
    if provider_type.is_empty() {
        return Err(NczError::Usage(
            "provider --type cannot be empty".to_string(),
        ));
    }
    if provider_type != OPENAI_COMPAT_PROVIDER_TYPE {
        return Err(NczError::Usage(format!(
            "unsupported provider --type {provider_type}; v0.2 supports only {OPENAI_COMPAT_PROVIDER_TYPE}"
        )));
    }
    reject_insecure_credential_url(&declaration.url)?;
    validate_health_path(&declaration.health_path)?;
    Ok(())
}

fn reject_surrounding_whitespace(label: &str, value: &str) -> Result<(), NczError> {
    if value != value.trim() {
        return Err(NczError::Usage(format!(
            "{label} cannot include leading or trailing whitespace"
        )));
    }
    Ok(())
}

pub fn validate_provider_url(url: &str) -> Result<(), NczError> {
    let trimmed = url.trim();
    if url_state::has_userinfo(trimmed) {
        return Err(NczError::Usage(
            "provider URL cannot include userinfo; use --key-env for credentials".to_string(),
        ));
    }
    if url_state::has_query_or_fragment(trimmed) {
        return Err(NczError::Usage(
            "provider URL cannot include query strings or fragments; use --key-env for credentials"
                .to_string(),
        ));
    }
    if url_state::contains_secret_path_material(trimmed) {
        return Err(NczError::Usage(
            "provider URL path cannot include credential-like material; use --key-env for credentials"
                .to_string(),
        ));
    }
    if trimmed != url {
        return Err(NczError::Usage("invalid provider URL".to_string()));
    }
    if trimmed.starts_with('-') {
        return Err(NczError::Usage("invalid provider URL".to_string()));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(NczError::Usage(
            "invalid provider URL scheme: expected http or https".to_string(),
        ));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(NczError::Usage("invalid provider URL".to_string()));
    }
    if !url_state::has_valid_authority(trimmed) {
        return Err(NczError::Usage("invalid provider URL".to_string()));
    }
    Ok(())
}

pub fn validate_health_path(path: &str) -> Result<(), NczError> {
    let trimmed = path.trim();
    if trimmed != path {
        return Err(NczError::Usage(format!(
            "invalid provider health path: {path}"
        )));
    }
    if trimmed.is_empty() || trimmed.starts_with('-') || trimmed.chars().any(char::is_whitespace) {
        return Err(NczError::Usage(format!(
            "invalid provider health path: {path}"
        )));
    }
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.contains("://")
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.split('/').any(|segment| segment == "..")
    {
        return Err(NczError::Usage(
            "provider health path must be a path without URL, query, fragment, or traversal"
                .to_string(),
        ));
    }
    if url_state::path_contains_secret_material(trimmed) {
        return Err(NczError::Usage(
            "provider health path cannot include credential-like material; use --key-env for credentials"
                .to_string(),
        ));
    }
    Ok(())
}

fn reject_insecure_credential_url(url: &str) -> Result<(), NczError> {
    let trimmed = url.trim();
    if !trimmed.to_ascii_lowercase().starts_with("http://") {
        return Ok(());
    }
    let Some(host) = url_state::host(trimmed) else {
        return Err(NczError::Usage("invalid provider URL".to_string()));
    };
    if url_state::is_loopback_host(host) {
        return Ok(());
    }
    Err(NczError::Usage(
        "provider URL with credentials cannot use plaintext HTTP; use https or a loopback provider URL"
            .to_string(),
    ))
}

pub fn validate_name(name: &str) -> Result<(), NczError> {
    if name.is_empty()
        || name.starts_with('.')
        || name.ends_with(".models")
        || name.contains("..")
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(NczError::Usage(format!("invalid provider name: {name}")));
    }
    Ok(())
}

pub fn models_from_value(value: Option<&Value>) -> Vec<ModelDeclaration> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::String(id) if !id.is_empty() => Some(ModelDeclaration {
                id: id.clone(),
                context_length: None,
            }),
            Value::Object(obj) => {
                let id = obj
                    .get("id")
                    .or_else(|| obj.get("name"))
                    .and_then(Value::as_str)?;
                Some(ModelDeclaration {
                    id: id.to_string(),
                    context_length: context_length_from_object(obj),
                })
            }
            _ => None,
        })
        .collect()
}

fn read_record(paths: &Paths, path: &Path) -> Result<Option<ProviderRecord>, NczError> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) => {
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) {
                return Ok(None);
            }
            return Err(NczError::Io(e));
        }
    };
    let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
    let fallback_name = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("unknown")
        .to_string();
    if ext == "json" {
        let canonical_json = json_has_schema_version(&body)?;
        let parsed = parse_json_full(&body, fallback_name.clone())?;
        validate_declaration(&parsed.declaration).map_err(|err| {
            NczError::Precondition(format!(
                "invalid provider declaration {}: {err}",
                path.display()
            ))
        })?;
        if canonical_json && fallback_name != parsed.declaration.name {
            return Err(NczError::Precondition(format!(
                "provider declaration filename {} does not match declared name {}",
                path.display(),
                parsed.declaration.name
            )));
        }
        if !canonical_json {
            let canonical_path = declaration_path(paths, &parsed.declaration.name)?;
            if canonical_path != path && canonical_path.exists() {
                let canonical_body = fs::read_to_string(&canonical_path)?;
                let canonical_name = canonical_path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .unwrap_or(&parsed.declaration.name)
                    .to_string();
                let canonical = parse_json(&canonical_body, canonical_name)?;
                validate_declaration(&canonical)?;
                if !legacy_collision_is_equivalent(&parsed.declaration, &canonical) {
                    return Err(NczError::Precondition(format!(
                        "legacy provider {} conflicts with {}; leaving legacy provider file in place",
                        path.display(),
                        canonical_path.display()
                    )));
                }
                return Ok(None);
            }
        }
        return Ok(Some(ProviderRecord {
            declaration: parsed.declaration,
            path: path.to_path_buf(),
            inline_secret: parsed.inline_secret,
            unmigratable_secret_field: parsed.unmigratable_secret_field,
        }));
    }

    let parsed = parse_env(&body, fallback_name)?;
    validate_declaration(&parsed.declaration).map_err(|err| {
        NczError::Precondition(format!("invalid legacy provider {}: {err}", path.display()))
    })?;
    let canonical_path = declaration_path(paths, &parsed.declaration.name)?;
    if canonical_path.exists() {
        let canonical_body = fs::read_to_string(&canonical_path)?;
        let canonical_name = canonical_path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or(&parsed.declaration.name)
            .to_string();
        let canonical = parse_json(&canonical_body, canonical_name)?;
        validate_declaration(&canonical)?;
        if !legacy_collision_is_equivalent(&parsed.declaration, &canonical) {
            return Err(NczError::Precondition(format!(
                "legacy provider {} conflicts with {}; leaving legacy provider file in place",
                path.display(),
                canonical_path.display()
            )));
        }
        return Ok(None);
    }
    Ok(Some(ProviderRecord {
        declaration: parsed.declaration,
        path: path.to_path_buf(),
        inline_secret: parsed.inline_secret,
        unmigratable_secret_field: parsed.unmigratable_secret_field,
    }))
}

fn provider_files(paths: &Paths) -> Result<Vec<PathBuf>, NczError> {
    let mut files = Vec::new();
    match fs::read_dir(paths.providers_dir()) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let path = entry.path();
                let ext = path.extension().and_then(OsStr::to_str);
                if matches!(ext, Some("env" | "conf" | "json")) && !is_model_cache_file(&path) {
                    files.push(path);
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(NczError::Io(e)),
    }
    files.sort();
    Ok(files)
}

fn matching_provider_files(paths: &Paths, name: &str) -> Result<Vec<PathBuf>, NczError> {
    validate_name(name)?;
    let mut files = Vec::new();
    for path in provider_files(paths)? {
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        if fallback_name == name {
            files.push(path);
            continue;
        }
        let body = fs::read_to_string(&path)?;
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let declaration = match parse_declaration_for_path(&body, &fallback_name, ext) {
            Ok(declaration) => declaration,
            Err(_) => continue,
        };
        if declaration.name == name {
            files.push(path);
        }
    }
    Ok(files)
}

fn parse_declaration_for_path(
    body: &str,
    fallback_name: &str,
    ext: &str,
) -> Result<ProviderDeclaration, NczError> {
    if ext == "json" {
        parse_json(body, fallback_name.to_string())
    } else {
        Ok(parse_env(body, fallback_name.to_string())?.declaration)
    }
}

fn legacy_collision_is_equivalent(
    legacy: &ProviderDeclaration,
    canonical: &ProviderDeclaration,
) -> bool {
    legacy.name == canonical.name
        && legacy.url == canonical.url
        && legacy.model == canonical.model
        && legacy.key_env == canonical.key_env
        && legacy.provider_type == canonical.provider_type
        && legacy.health_path == canonical.health_path
        && legacy.models == canonical.models
}

fn is_model_cache_file(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.ends_with(".models.json"))
}

fn parse_json(body: &str, fallback_name: String) -> Result<ProviderDeclaration, NczError> {
    Ok(parse_json_full(body, fallback_name)?.declaration)
}

fn json_has_schema_version(body: &str) -> Result<bool, NczError> {
    let value: Value = serde_json::from_str(body)?;
    Ok(value.get("schema_version").is_some())
}

fn parse_json_full(body: &str, fallback_name: String) -> Result<ParsedProvider, NczError> {
    let value: Value = serde_json::from_str(body)?;
    if value.get("schema_version").is_none() {
        return parse_legacy_json(value, fallback_name);
    }
    let schema_version = required_schema_version(&value)?;
    if schema_version != 1 {
        return Err(NczError::Precondition(format!(
            "unsupported provider schema_version: {schema_version}"
        )));
    }
    if let Some(field) = json_inline_secret_field(&value) {
        return Err(NczError::Precondition(format!(
            "provider JSON cannot include inline credential field {field}; use key_env"
        )));
    }
    if let Some(field) = unknown_canonical_json_field(&value) {
        return Err(NczError::Precondition(format!(
            "provider JSON contains unsupported field {field}"
        )));
    }
    let name = required_string_field(&value, "name")?;
    let key_env = required_string_field(&value, "key_env")?;
    Ok(ParsedProvider {
        declaration: ProviderDeclaration {
            schema_version: schema_version as u32,
            name,
            url: required_string_field(&value, "url")?,
            model: required_string_field(&value, "model")?,
            key_env,
            provider_type: required_string_field(&value, "type")?,
            health_path: required_string_field(&value, "health_path")?,
            models: models_from_value(value.get("models")),
        },
        inline_secret: None,
        unmigratable_secret_field: None,
        unsupported_legacy_field: None,
    })
}

fn parse_legacy_json(value: Value, fallback_name: String) -> Result<ParsedProvider, NczError> {
    let name = legacy_string_field(&value, &["name", "provider"]).unwrap_or(fallback_name);
    let url = legacy_string_field(&value, &["url", "base_url", "endpoint"]).unwrap_or_default();
    let explicit_key_env =
        legacy_string_field(&value, &[
            "key_env",
            "keyEnv",
            "api_key_env",
            "token_env",
            "auth_env",
            "authorization_env",
        ]);
    let legacy_secret = legacy_json_secret(&value, &name, explicit_key_env.as_deref());
    let unsupported_legacy_field = unsupported_legacy_json_field(&value);
    let key_env = explicit_key_env
        .or_else(|| {
            legacy_secret
                .credential
                .as_ref()
                .map(|(key_env, _)| key_env.clone())
        })
        .unwrap_or_else(|| "API_KEY".to_string());
    Ok(ParsedProvider {
        declaration: ProviderDeclaration {
            schema_version: 1,
            name,
            url: url.clone(),
            model: legacy_string_field(&value, &["model", "default_model"]).unwrap_or_default(),
            key_env,
            provider_type: legacy_string_field(&value, &["type", "provider_type"])
                .unwrap_or_else(|| DEFAULT_PROVIDER_TYPE.to_string()),
            health_path: normalize_legacy_health_value(
                legacy_string_field(&value, &["health_path", "health_url"]).as_deref(),
                &url,
            )?,
            models: models_from_value(value.get("models")),
        },
        inline_secret: legacy_secret.credential.map(|(_, secret)| secret),
        unmigratable_secret_field: legacy_secret.unmigratable_field,
        unsupported_legacy_field,
    })
}

fn parse_env(body: &str, fallback_name: String) -> Result<ParsedProvider, NczError> {
    let pairs = env_pairs(body)?;

    let name = lookup(&pairs, &["PROVIDER_NAME", "NAME"]).unwrap_or(fallback_name);
    let url =
        lookup(&pairs, &["PROVIDER_URL", "BASE_URL", "ENDPOINT", "URL"]).unwrap_or_default();
    let explicit_key_env = lookup(&pairs, &[
        "KEY_ENV",
        "API_KEY_ENV",
        "TOKEN_ENV",
        "AUTH_ENV",
        "AUTHORIZATION_ENV",
    ]);
    let legacy_secret = legacy_env_secret(&pairs, &name, explicit_key_env.as_deref());
    let unsupported_legacy_field =
        unsupported_legacy_env_field(&pairs, explicit_key_env.as_deref(), &legacy_secret);
    let key_env = explicit_key_env
        .or_else(|| {
            legacy_secret
                .credential
                .as_ref()
                .map(|(key_env, _)| key_env.clone())
        })
        .unwrap_or_else(|| "API_KEY".to_string());

    Ok(ParsedProvider {
        declaration: ProviderDeclaration {
            schema_version: 1,
            name,
            url: url.clone(),
            model: lookup(&pairs, &["MODEL", "DEFAULT_MODEL", "PROVIDER_MODEL"])
                .unwrap_or_default(),
            key_env,
            provider_type: lookup(&pairs, &["PROVIDER_TYPE", "TYPE"])
                .unwrap_or_else(|| DEFAULT_PROVIDER_TYPE.to_string()),
            health_path: normalize_legacy_health_value(
                lookup(&pairs, &["HEALTH_PATH", "PROVIDER_HEALTH_URL", "HEALTH_URL"]).as_deref(),
                &url,
            )?,
            models: Vec::new(),
        },
        inline_secret: legacy_secret.credential.map(|(_, secret)| secret),
        unmigratable_secret_field: legacy_secret.unmigratable_field,
        unsupported_legacy_field,
    })
}

fn normalize_legacy_health_value(
    value: Option<&str>,
    provider_url: &str,
) -> Result<String, NczError> {
    let Some(value) = value else {
        return Ok(DEFAULT_HEALTH_PATH.to_string());
    };
    let value = value.trim();
    if !(value.starts_with("http://") || value.starts_with("https://")) {
        return Ok(value.to_string());
    }
    if url_state::has_userinfo(value) || url_state::has_query_or_fragment(value) {
        return Err(NczError::Usage(
            "legacy provider health URL cannot include userinfo, query strings, or fragments"
                .to_string(),
        ));
    }
    if !same_origin(provider_url, value) {
        return Err(NczError::Usage(format!(
            "legacy provider health URL must use the same origin as provider URL: {value}"
        )));
    }
    Ok(url_path(value))
}

fn same_origin(left: &str, right: &str) -> bool {
    let Some((left_scheme, _)) = left.split_once("://") else {
        return false;
    };
    let Some((right_scheme, _)) = right.split_once("://") else {
        return false;
    };
    left_scheme.eq_ignore_ascii_case(right_scheme)
        && url_state::authority(left).is_some_and(|left_authority| {
            url_state::authority(right).is_some_and(|right_authority| {
                left_authority.eq_ignore_ascii_case(right_authority)
            })
        })
}

fn url_path(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return DEFAULT_HEALTH_PATH.to_string();
    };
    let path = rest
        .split_once('/')
        .map(|(_, path)| format!("/{path}"))
        .unwrap_or_else(|| "/".to_string());
    if path.is_empty() {
        "/".to_string()
    } else {
        path
    }
}

fn inline_credential_for_path(
    body: &str,
    ext: &str,
    fallback_name: &str,
) -> Result<Option<(String, String)>, NczError> {
    if ext == "json" {
        let value: Value = serde_json::from_str(body)?;
        if value.get("schema_version").is_some() {
            return canonical_json_inline_credential(&value, fallback_name);
        }
        let parsed = parse_legacy_json(value, fallback_name.to_string())?;
        if let Some(field) = &parsed.unmigratable_secret_field {
            return Err(unmigratable_legacy_secret(field));
        }
        return Ok(parsed
            .inline_secret
            .map(|secret| (parsed.declaration.key_env, secret)));
    }
    let parsed = parse_env(body, fallback_name.to_string())?;
    if let Some(field) = &parsed.unmigratable_secret_field {
        return Err(unmigratable_legacy_secret(field));
    }
    Ok(parsed
        .inline_secret
        .map(|secret| (parsed.declaration.key_env, secret)))
}

fn canonical_json_inline_credential(
    value: &Value,
    fallback_name: &str,
) -> Result<Option<(String, String)>, NczError> {
    let candidates = json_secret_candidates(value);
    let Some(candidate) = candidates.first() else {
        return Ok(None);
    };
    if candidates.len() > 1 {
        let fields = candidates
            .iter()
            .map(|candidate| candidate.path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(NczError::Precondition(format!(
            "provider {fallback_name} contains multiple inline credential fields ({fields}); preserve or remove them before replacement"
        )));
    }
    if !candidate.migratable {
        return Err(unmigratable_legacy_secret(&candidate.path));
    }
    let secret = candidate
        .value
        .as_deref()
        .and_then(|secret| normalize_legacy_json_secret_value(candidate, secret))
        .ok_or_else(|| unmigratable_legacy_secret(&candidate.path))?;
    let key_env = value
        .get("key_env")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| legacy_secret_json_key(fallback_name, &candidate.key));
    Ok(Some((key_env, secret)))
}

fn json_inline_secret_field(value: &Value) -> Option<String> {
    json_secret_candidates(value)
        .into_iter()
        .next()
        .map(|candidate| candidate.path)
}

fn json_secret_value_present(value: &Value) -> bool {
    match value {
        Value::String(value) => !value.is_empty(),
        Value::Array(values) => values.iter().any(json_secret_value_present),
        Value::Object(object) => !object.is_empty(),
        _ => false,
    }
}

fn json_secret_candidates(value: &Value) -> Vec<JsonSecretCandidate> {
    let mut candidates = Vec::new();
    collect_json_secret_candidates(value, "", false, false, &mut candidates);
    candidates
}

fn collect_json_secret_candidates(
    value: &Value,
    path: &str,
    credential_context: bool,
    header_context: bool,
    out: &mut Vec<JsonSecretCandidate>,
) {
    match value {
        Value::Object(object) => {
            let header_pair_object = if header_context {
                if let Some(candidate) = json_header_pair_candidate(object, path) {
                    out.push(candidate);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            for (key, value) in object {
                let child_path = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                if !(header_pair_object && json_header_pair_field_name(key))
                    && json_secret_candidate_name(key)
                    && !(json_secret_container_name(key) && matches!(value, Value::Object(_)))
                    && json_secret_value_present(value)
                {
                    let migratable = if header_context {
                        normalized_secret_field(key) == "AUTHORIZATION"
                    } else {
                        json_secret_field_name(key) && (path.is_empty() || credential_context)
                    };
                    out.push(JsonSecretCandidate {
                        path: child_path.clone(),
                        key: key.to_string(),
                        value: first_json_string(value),
                        migratable,
                    });
                } else if header_context
                    && !json_header_pair_field_name(key)
                    && json_secret_value_present(value)
                {
                    out.push(JsonSecretCandidate {
                        path: child_path.clone(),
                        key: key.to_string(),
                        value: first_json_string(value),
                        migratable: false,
                    });
                }
                collect_json_secret_candidates(
                    value,
                    &child_path,
                    credential_context || json_secret_container_name(key),
                    header_context || json_header_container_name(key),
                    out,
                );
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let child_path = format!("{path}[{index}]");
                collect_json_secret_candidates(
                    value,
                    &child_path,
                    credential_context,
                    header_context,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn json_header_pair_candidate(
    object: &serde_json::Map<String, Value>,
    path: &str,
) -> Option<JsonSecretCandidate> {
    let header_name = json_header_pair_string(object, &["name", "key", "header", "header_name"])?;
    let header_value = json_header_pair_string(object, &["value"])?;
    if header_value.is_empty() {
        return None;
    }
    let path = if path.is_empty() {
        header_name.clone()
    } else {
        format!("{path}.{header_name}")
    };
    let migratable = normalized_secret_field(&header_name) == "AUTHORIZATION";
    Some(JsonSecretCandidate {
        path,
        key: header_name,
        value: Some(header_value),
        migratable,
    })
}

fn json_header_pair_string(
    object: &serde_json::Map<String, Value>,
    names: &[&str],
) -> Option<String> {
    object.iter().find_map(|(key, value)| {
        if names
            .iter()
            .any(|name| normalized_secret_field(key) == normalized_secret_field(name))
        {
            value.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

fn json_header_pair_field_name(name: &str) -> bool {
    matches!(
        normalized_secret_field(name).as_str(),
        "NAME" | "KEY" | "HEADER" | "HEADERNAME" | "VALUE"
    )
}

fn legacy_json_secret(
    value: &Value,
    provider_name: &str,
    explicit_key_env: Option<&str>,
) -> LegacyJsonSecret {
    let candidates = json_secret_candidates(value);
    let Some(first_candidate) = candidates.first() else {
        return LegacyJsonSecret {
            credential: None,
            unmigratable_field: None,
        };
    };

    if let Some(key_env) = explicit_key_env {
        let selected = candidates
            .iter()
            .find(|candidate| legacy_field_matches_key_env(&candidate.key, key_env))
            .or_else(|| {
                candidates
                    .iter()
                    .find(|candidate| known_legacy_secret_field(&candidate.key))
            });
        let Some(candidate) = selected else {
            return LegacyJsonSecret {
                credential: None,
                unmigratable_field: Some(first_candidate.path.clone()),
            };
        };
        if !candidate.migratable {
            return LegacyJsonSecret {
                credential: None,
                unmigratable_field: Some(candidate.path.clone()),
            };
        }
        return legacy_secret_from_candidate_with_extra_check(
            &candidates,
            candidate,
            key_env.to_string(),
        );
    }

    if !first_candidate.migratable {
        return LegacyJsonSecret {
            credential: None,
            unmigratable_field: Some(first_candidate.path.clone()),
        };
    }
    let key_env = legacy_secret_json_key(provider_name, &first_candidate.key);
    legacy_secret_from_candidate_with_extra_check(&candidates, first_candidate, key_env)
}

fn legacy_secret_from_candidate_with_extra_check(
    candidates: &[JsonSecretCandidate],
    selected: &JsonSecretCandidate,
    key_env: String,
) -> LegacyJsonSecret {
    let mut result = legacy_secret_from_candidate(selected, key_env);
    if result.unmigratable_field.is_none() {
        result.unmigratable_field = candidates
            .iter()
            .find(|candidate| candidate.path != selected.path)
            .map(|candidate| candidate.path.clone());
    }
    result
}

fn legacy_secret_from_candidate(
    candidate: &JsonSecretCandidate,
    key_env: String,
) -> LegacyJsonSecret {
    match &candidate.value {
        Some(secret) => match normalize_legacy_json_secret_value(candidate, secret) {
            Some(secret) => LegacyJsonSecret {
                credential: Some((key_env, secret)),
                unmigratable_field: None,
            },
            None => LegacyJsonSecret {
                credential: None,
                unmigratable_field: Some(candidate.path.clone()),
            },
        },
        None => LegacyJsonSecret {
            credential: None,
            unmigratable_field: Some(candidate.path.clone()),
        },
    }
}

fn normalize_legacy_json_secret_value(
    candidate: &JsonSecretCandidate,
    secret: &str,
) -> Option<String> {
    normalize_legacy_secret_value(&candidate.key, secret)
}

fn normalize_legacy_secret_value(field: &str, secret: &str) -> Option<String> {
    if normalized_secret_field(field) != "AUTHORIZATION" {
        return Some(secret.to_string());
    }
    strip_bearer_prefix(secret).map(ToOwned::to_owned)
}

fn strip_bearer_prefix(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.len() <= "Bearer ".len() {
        return None;
    }
    let (scheme, rest) = trimmed.split_at("Bearer ".len());
    if scheme.eq_ignore_ascii_case("Bearer ") {
        let token = rest.trim_start();
        if !token.is_empty() {
            return Some(token);
        }
    }
    None
}

fn unmigratable_legacy_secret(field: &str) -> NczError {
    NczError::Precondition(format!(
        "legacy provider contains inline credential field {field} that cannot be migrated; move it to agent-env or remove it before migration"
    ))
}

fn unsupported_legacy_field(path: &Path, field: &str) -> NczError {
    NczError::Precondition(format!(
        "legacy provider {} contains unsupported field {field}; cannot auto-migrate without losing configuration; leaving legacy provider file in place",
        path.display()
    ))
}

fn unsupported_legacy_json_field(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    for (key, child) in object {
        if legacy_json_declaration_field(key) {
            continue;
        }
        if json_header_container_name(key) {
            if let Some(field) = unsupported_legacy_json_header_container(key, child) {
                return Some(field);
            }
            continue;
        }
        if json_secret_container_name(key) && matches!(child, Value::Object(_) | Value::Array(_)) {
            if let Some(field) = unsupported_legacy_json_secret_container(key, child) {
                return Some(field);
            }
            continue;
        }
        if json_secret_candidate_name(key) {
            continue;
        }
        return Some(key.clone());
    }
    None
}

fn legacy_json_declaration_field(name: &str) -> bool {
    matches!(
        name,
        "name"
            | "provider"
            | "url"
            | "base_url"
            | "endpoint"
            | "model"
            | "default_model"
            | "key_env"
            | "keyEnv"
            | "api_key_env"
            | "token_env"
            | "auth_env"
            | "authorization_env"
            | "type"
            | "provider_type"
            | "health_path"
            | "health_url"
            | "models"
    )
}

fn unsupported_legacy_json_header_container(path: &str, value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            if let Some(candidate) = json_header_pair_candidate(object, path) {
                if !candidate.migratable {
                    return Some(candidate.path);
                }
                return object
                    .keys()
                    .find(|key| !json_header_pair_field_name(key))
                    .map(|key| format!("{path}.{key}"));
            }
            for (key, child) in object {
                let child_path = format!("{path}.{key}");
                if json_secret_candidate_name(key) {
                    continue;
                }
                if json_secret_value_present(child) {
                    return Some(child_path);
                }
            }
            None
        }
        Value::Array(values) => values.iter().enumerate().find_map(|(index, child)| {
            unsupported_legacy_json_header_container(&format!("{path}[{index}]"), child)
        }),
        _ => None,
    }
}

fn unsupported_legacy_json_secret_container(path: &str, value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let child_path = format!("{path}.{key}");
                if json_header_container_name(key) {
                    if let Some(field) =
                        unsupported_legacy_json_header_container(&child_path, child)
                    {
                        return Some(field);
                    }
                    continue;
                }
                if json_secret_container_name(key)
                    && matches!(child, Value::Object(_) | Value::Array(_))
                {
                    if let Some(field) =
                        unsupported_legacy_json_secret_container(&child_path, child)
                    {
                        return Some(field);
                    }
                    continue;
                }
                if json_secret_candidate_name(key) {
                    continue;
                }
                return Some(child_path);
            }
            None
        }
        Value::Array(values) => values.iter().enumerate().find_map(|(index, child)| {
            unsupported_legacy_json_secret_container(&format!("{path}[{index}]"), child)
        }),
        _ => None,
    }
}

fn unsupported_legacy_env_field(
    pairs: &[(String, String)],
    explicit_key_env: Option<&str>,
    legacy_secret: &LegacyEnvSecret,
) -> Option<String> {
    for (key, _) in pairs {
        if legacy_env_declaration_field(key) {
            continue;
        }
        if explicit_key_env.is_some_and(|key_env| legacy_field_matches_key_env(key, key_env)) {
            continue;
        }
        if legacy_secret
            .selected_field
            .as_deref()
            .is_some_and(|selected| key.eq_ignore_ascii_case(selected))
        {
            continue;
        }
        if secret_field_name(key) {
            continue;
        }
        return Some(key.clone());
    }
    None
}

fn legacy_env_declaration_field(name: &str) -> bool {
    [
        "PROVIDER_NAME",
        "NAME",
        "PROVIDER_URL",
        "BASE_URL",
        "ENDPOINT",
        "URL",
        "KEY_ENV",
        "API_KEY_ENV",
        "TOKEN_ENV",
        "AUTH_ENV",
        "AUTHORIZATION_ENV",
        "MODEL",
        "DEFAULT_MODEL",
        "PROVIDER_MODEL",
        "PROVIDER_TYPE",
        "TYPE",
        "HEALTH_PATH",
        "PROVIDER_HEALTH_URL",
        "HEALTH_URL",
    ]
    .iter()
    .any(|field| name.eq_ignore_ascii_case(field))
}

fn first_json_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::Array(values) => values.iter().find_map(first_json_string),
        Value::Object(object) => object.values().find_map(first_json_string),
        _ => None,
    }
}

fn unknown_canonical_json_field(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    object
        .keys()
        .find(|key| !canonical_json_field(key))
        .map(String::as_str)
}

fn canonical_json_field(name: &str) -> bool {
    matches!(
        name,
        "schema_version" | "name" | "url" | "model" | "key_env" | "type" | "health_path" | "models"
    )
}

fn secret_field_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if key_env_metadata_field(&name) {
        return false;
    }
    let normalized = normalized_secret_field(&name);
    normalized == "AUTH"
        || normalized == "BEARER"
        || name == "key"
        || name.contains("token")
        || name.contains("secret")
        || name.contains("password")
        || name.contains("authorization")
        || name.contains("api_key")
        || name.contains("api-key")
        || name.contains("apikey")
        || name.ends_with("_key")
        || name.ends_with("-key")
}

fn json_secret_field_name(name: &str) -> bool {
    matches!(
        normalized_secret_field(name).as_str(),
        "KEY"
            | "APIKEY"
            | "OPENAIAPIKEY"
            | "ANTHROPICAPIKEY"
            | "TOGETHERAPIKEY"
            | "TOKEN"
            | "ACCESSTOKEN"
            | "BEARERTOKEN"
            | "SECRET"
            | "PASSWORD"
            | "AUTH"
            | "AUTHORIZATION"
            | "BEARER"
    )
}

fn json_secret_candidate_name(name: &str) -> bool {
    json_secret_field_name(name) || (!json_metadata_field_name(name) && secret_field_name(name))
}

fn json_metadata_field_name(name: &str) -> bool {
    matches!(
        normalized_secret_field(name).as_str(),
        "TOKENIZER"
            | "MAXTOKENS"
            | "MAXOUTPUTTOKENS"
            | "CONTEXTLENGTH"
            | "CONTEXTWINDOW"
            | "MAXCONTEXTLENGTH"
    )
}

fn json_secret_container_name(name: &str) -> bool {
    matches!(
        normalized_secret_field(name).as_str(),
        "AUTH"
            | "AUTHENTICATION"
            | "CREDENTIAL"
            | "CREDENTIALS"
            | "HEADER"
            | "HEADERS"
            | "HTTPHEADER"
            | "HTTPHEADERS"
            | "REQUESTHEADER"
            | "REQUESTHEADERS"
    )
}

fn json_header_container_name(name: &str) -> bool {
    matches!(
        normalized_secret_field(name).as_str(),
        "HEADER" | "HEADERS" | "HTTPHEADER" | "HTTPHEADERS" | "REQUESTHEADER" | "REQUESTHEADERS"
    )
}

fn key_env_metadata_field(name: &str) -> bool {
    matches!(
        name,
        "key_env"
            | "keyenv"
            | "api_key_env"
            | "api-key-env"
            | "apikey_env"
            | "token_env"
            | "auth_env"
            | "authorization_env"
    ) || name.ends_with("_env")
        || name.ends_with("-env")
}

fn env_pairs(body: &str) -> Result<Vec<(String, String)>, NczError> {
    let mut pairs = Vec::new();
    for line in body.lines() {
        if let Some((key, value)) = parse_legacy_env_assignment(line)? {
            pairs.push((key, value));
        }
    }
    Ok(pairs)
}

fn parse_legacy_env_assignment(line: &str) -> Result<Option<(String, String)>, NczError> {
    let assignment = line
        .trim_start()
        .strip_prefix("export ")
        .unwrap_or_else(|| line.trim_start());
    if assignment.is_empty() || assignment.starts_with('#') || assignment.starts_with(';') {
        return Ok(None);
    }
    let Some((key, raw_value)) = assignment.split_once('=') else {
        return Ok(None);
    };
    let key = key.trim();
    if agent_env::validate_key(key).is_err() {
        return Ok(None);
    }
    Ok(Some((key.to_string(), parse_legacy_env_value(raw_value)?)))
}

fn parse_legacy_env_value(raw: &str) -> Result<String, NczError> {
    let raw = raw.trim_start();
    if let Some(rest) = raw.strip_prefix('"') {
        return parse_legacy_double_quoted_value(rest);
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        return parse_legacy_single_quoted_value(rest);
    }
    Ok(parse_legacy_unquoted_value(raw.trim_end()))
}

fn parse_legacy_double_quoted_value(raw: &str) -> Result<String, NczError> {
    let mut out = String::new();
    let mut escaped = false;
    for (idx, ch) in raw.char_indices() {
        if escaped {
            match ch {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                _ => {
                    out.push('\\');
                    out.push(ch);
                }
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            ensure_legacy_env_trailing_comment(&raw[idx + ch.len_utf8()..])?;
            return Ok(out);
        } else {
            out.push(ch);
        }
    }
    Err(NczError::Usage(
        "unterminated double-quoted environment value".to_string(),
    ))
}

fn parse_legacy_single_quoted_value(raw: &str) -> Result<String, NczError> {
    let Some(idx) = raw.find('\'') else {
        return Err(NczError::Usage(
            "unterminated single-quoted environment value".to_string(),
        ));
    };
    ensure_legacy_env_trailing_comment(&raw[idx + 1..])?;
    Ok(raw[..idx].to_string())
}

fn parse_legacy_unquoted_value(raw: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if matches!(ch, '#' | ';') && out.trim_end().len() < out.len() {
            let value_len = out.trim_end().len();
            out.truncate(value_len);
            break;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out.trim_end().to_string()
}

fn ensure_legacy_env_trailing_comment(rest: &str) -> Result<(), NczError> {
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with('#') || rest.starts_with(';') {
        Ok(())
    } else {
        Err(NczError::Usage(
            "unexpected characters after quoted environment value".to_string(),
        ))
    }
}

fn write_declaration(path: &Path, declaration: &ProviderDeclaration) -> Result<(), NczError> {
    let mut body = serde_json::to_vec_pretty(declaration)?;
    body.push(b'\n');
    state::atomic_write(path, &body, 0o644)
}

struct FileSnapshot {
    path: PathBuf,
    body: Option<Vec<u8>>,
    mode: u32,
}

fn snapshot_files(paths: &[PathBuf]) -> Result<Vec<FileSnapshot>, NczError> {
    paths.iter().map(|path| snapshot_file(path)).collect()
}

fn snapshot_file(path: &Path) -> Result<FileSnapshot, NczError> {
    match fs::read(path) {
        Ok(body) => {
            let mode = fs::metadata(path)?.permissions().mode() & 0o777;
            Ok(FileSnapshot {
                path: path.to_path_buf(),
                body: Some(body),
                mode,
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FileSnapshot {
            path: path.to_path_buf(),
            body: None,
            mode: 0o644,
        }),
        Err(e) => Err(NczError::Io(e)),
    }
}

fn restore_file_snapshots(snapshots: &[FileSnapshot]) -> Result<(), NczError> {
    for snapshot in snapshots.iter().rev() {
        match &snapshot.body {
            Some(body) => state::atomic_write(&snapshot.path, body, snapshot.mode)?,
            None => state::remove_file_durable(&snapshot.path)?,
        }
    }
    Ok(())
}

fn legacy_secret_env_key(provider_name: &str, legacy_key: &str) -> String {
    let mut prefix = String::new();
    for ch in provider_name.chars() {
        if ch.is_ascii_alphanumeric() {
            prefix.push(ch.to_ascii_uppercase());
        } else if ch == '-' || ch == '.' {
            prefix.push('_');
        }
    }
    while prefix.contains("__") {
        prefix = prefix.replace("__", "_");
    }
    let prefix = prefix.trim_matches('_');
    let prefix = if prefix.is_empty() {
        "PROVIDER"
    } else {
        prefix
    };
    let suffix = if legacy_key.eq_ignore_ascii_case("TOKEN") {
        "TOKEN"
    } else if legacy_key.eq_ignore_ascii_case("SECRET") {
        "SECRET"
    } else if legacy_key.eq_ignore_ascii_case("PASSWORD") {
        "PASSWORD"
    } else {
        "API_KEY"
    };
    format!("{prefix}_{suffix}")
}

fn required_schema_version(value: &Value) -> Result<u64, NczError> {
    let Some(raw) = value.get("schema_version") else {
        return Err(NczError::Precondition(
            "provider schema_version is required".to_string(),
        ));
    };
    raw.as_u64().ok_or_else(|| {
        NczError::Precondition("provider schema_version must be an integer".to_string())
    })
}

fn required_string_field(value: &Value, name: &str) -> Result<String, NczError> {
    let Some(raw) = value.get(name) else {
        return Err(NczError::Precondition(format!(
            "provider field {name} is required"
        )));
    };
    let Some(raw) = raw.as_str() else {
        return Err(NczError::Precondition(format!(
            "provider field {name} must be a string"
        )));
    };
    if raw.is_empty() {
        return Err(NczError::Precondition(format!(
            "provider field {name} cannot be empty"
        )));
    }
    Ok(raw.to_string())
}

fn legacy_string_field(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .filter(|value| !value.is_empty())
    })
}

fn legacy_env_secret(
    pairs: &[(String, String)],
    provider_name: &str,
    explicit_key_env: Option<&str>,
) -> LegacyEnvSecret {
    let candidates = legacy_env_secret_candidates(pairs, explicit_key_env);
    if candidates.is_empty() {
        return LegacyEnvSecret {
            credential: None,
            unmigratable_field: None,
            selected_field: None,
        };
    }

    let selected_index = if let Some(key_env) = explicit_key_env {
        let normalized_key_env = normalized_secret_field(key_env);
        candidates
            .iter()
            .position(|candidate| candidate.normalized_key == normalized_key_env)
            .or_else(|| {
                candidates
                    .iter()
                    .position(|candidate| candidate.known_legacy)
            })
    } else {
        candidates
            .iter()
            .position(|candidate| candidate.known_legacy)
    };

    let mut selected_unmigratable_field = None;
    let mut selected_field = None;
    let credential = selected_index.and_then(|index| {
        let candidate = &candidates[index];
        selected_field = Some(candidate.raw_key.clone());
        let Some(secret) = normalize_legacy_secret_value(&candidate.raw_key, &candidate.value)
        else {
            selected_unmigratable_field = Some(candidate.raw_key.clone());
            return None;
        };
        let key_env = explicit_key_env
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| legacy_secret_env_key_for_raw(provider_name, &candidate.raw_key));
        Some((key_env, secret))
    });

    let unmigratable_field = selected_unmigratable_field.or_else(|| {
        candidates
            .iter()
            .enumerate()
            .find(|(index, _)| Some(*index) != selected_index)
            .map(|(_, candidate)| candidate.raw_key.clone())
    });

    LegacyEnvSecret {
        credential,
        unmigratable_field,
        selected_field,
    }
}

fn legacy_env_secret_candidates(
    pairs: &[(String, String)],
    explicit_key_env: Option<&str>,
) -> Vec<EnvSecretCandidate> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    let normalized_explicit = explicit_key_env.map(normalized_secret_field);
    for (raw_key, value) in pairs.iter().rev() {
        if value.is_empty() {
            continue;
        }
        let normalized_key = normalized_secret_field(raw_key);
        let explicit_match = normalized_explicit
            .as_deref()
            .is_some_and(|key_env| normalized_key == key_env);
        if !explicit_match && !secret_field_name(raw_key) {
            continue;
        }
        if !seen.insert(normalized_key.clone()) {
            continue;
        }
        candidates.push(EnvSecretCandidate {
            raw_key: raw_key.clone(),
            normalized_key,
            value: value.clone(),
            known_legacy: migratable_legacy_env_secret_field(raw_key),
        });
    }
    candidates
}

fn migratable_legacy_env_secret_field(name: &str) -> bool {
    if known_legacy_secret_field(name) {
        return true;
    }
    let normalized = normalized_secret_field(name);
    normalized.ends_with("APIKEY")
}

fn known_legacy_secret_field(name: &str) -> bool {
    matches!(
        normalized_secret_field(name).as_str(),
        "KEY" | "APIKEY" | "TOKEN" | "SECRET" | "PASSWORD" | "AUTH" | "AUTHORIZATION" | "BEARER"
    )
}

fn legacy_field_matches_key_env(field: &str, key_env: &str) -> bool {
    normalized_secret_field(field) == normalized_secret_field(key_env)
}

fn normalized_secret_field(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_uppercase())
        .collect()
}

fn legacy_secret_env_key_for_raw(provider_name: &str, raw_key: &str) -> String {
    if ["API_KEY", "TOKEN", "SECRET", "PASSWORD"]
        .iter()
        .any(|key| raw_key.eq_ignore_ascii_case(key))
    {
        legacy_secret_env_key(provider_name, raw_key)
    } else {
        secret_key_name_to_env(raw_key)
    }
}

fn legacy_secret_json_key(provider_name: &str, json_key: &str) -> String {
    if matches!(json_key, "api_key" | "token" | "secret" | "password") {
        legacy_secret_env_key(provider_name, json_key)
    } else {
        secret_key_name_to_env(json_key)
    }
}

fn secret_key_name_to_env(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else if ch == '-' || ch == '.' || ch == '_' {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "API_KEY".to_string()
    } else {
        out.to_string()
    }
}

fn lookup(pairs: &[(String, String)], keys: &[&str]) -> Option<String> {
    lookup_with_key(pairs, keys).map(|(_, value)| value)
}

fn lookup_with_key(pairs: &[(String, String)], keys: &[&str]) -> Option<(String, String)> {
    for wanted in keys {
        for (key, value) in pairs.iter().rev() {
            if key.eq_ignore_ascii_case(wanted) {
                return Some((key.to_ascii_uppercase(), value.clone()));
            }
        }
    }
    None
}

fn context_length_from_object(obj: &serde_json::Map<String, Value>) -> Option<u64> {
    [
        "context_length",
        "context_window",
        "max_context_length",
        "max_tokens",
    ]
    .iter()
    .find_map(|key| obj.get(*key).and_then(Value::as_u64))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::state::Paths;

    use super::*;

    fn test_paths(root: &std::path::Path) -> Paths {
        Paths {
            etc_dir: root.join("etc/nclawzero"),
            quadlet_dir: root.join("etc/containers/systemd"),
            lock_path: root.join("run/nclawzero.lock"),
        }
    }

    fn provider(name: &str) -> ProviderDeclaration {
        ProviderDeclaration {
            schema_version: 1,
            name: name.to_string(),
            url: "https://api.example.test".to_string(),
            model: "model-a".to_string(),
            key_env: "EXAMPLE_API_KEY".to_string(),
            provider_type: "openai-compat".to_string(),
            health_path: "/health".to_string(),
            models: Vec::new(),
        }
    }

    #[test]
    fn write_rejects_existing_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let declaration = provider("together");

        write(&paths, &declaration, false).unwrap();
        let err = write(&paths, &declaration, false).unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_credentialed_remote_plaintext_http_provider_urls() {
        let mut declaration = provider("remote");
        declaration.url = "http://api.example.test".to_string();

        let err = validate_declaration(&declaration).unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn allows_credentialed_loopback_plaintext_http_provider_urls() {
        let mut declaration = provider("local");
        declaration.url = "http://127.0.0.1:8080".to_string();

        validate_declaration(&declaration).unwrap();
    }

    #[test]
    fn rejects_reserved_provider_key_env() {
        let mut declaration = provider("local");
        declaration.key_env = "NCZ_PROVIDER_BINDING_6C6F63616C".to_string();

        let err = validate_declaration(&declaration).unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_whitespace_padded_provider_fields() {
        let mut declaration = provider("padded");
        declaration.url = " https://api.example.test".to_string();
        assert!(matches!(
            validate_declaration(&declaration).unwrap_err(),
            NczError::Usage(_)
        ));

        let mut declaration = provider("padded");
        declaration.provider_type = "openai-compat ".to_string();
        assert!(matches!(
            validate_declaration(&declaration).unwrap_err(),
            NczError::Usage(_)
        ));

        let mut declaration = provider("padded");
        declaration.health_path = " /health".to_string();
        assert!(matches!(
            validate_declaration(&declaration).unwrap_err(),
            NczError::Usage(_)
        ));
    }

    #[test]
    fn write_rejects_matching_legacy_provider_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=old-secret\n",
        )
        .unwrap();
        let mut declaration = provider("local");
        declaration.key_env = "NEW_API_KEY".to_string();

        let err = write(&paths, &declaration, false).unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
        assert!(paths.providers_dir().join("local.env").exists());
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn write_force_replaces_legacy_provider_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("old-name.conf"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\n",
        )
        .unwrap();
        let mut declaration = provider("local");
        declaration.key_env = "NEW_API_KEY".to_string();

        write(&paths, &declaration, true).unwrap();

        assert!(paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("old-name.conf").exists());
        assert!(!paths.agent_env().exists());
        let body = fs::read_to_string(paths.providers_dir().join("local.json")).unwrap();
        assert!(body.contains("NEW_API_KEY"));
    }

    #[test]
    fn write_force_replaces_filename_match_with_invalid_declared_name() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"../bad","url":"https://api.example.test","model":"old","key_env":"OLD_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        let mut declaration = provider("local");
        declaration.key_env = "NEW_API_KEY".to_string();

        write(&paths, &declaration, true).unwrap();

        let body = fs::read_to_string(paths.providers_dir().join("local.json")).unwrap();
        assert!(body.contains(r#""name": "local""#));
        assert!(body.contains("NEW_API_KEY"));
    }

    #[test]
    fn write_force_removes_stale_model_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let declaration = provider("together");
        write(&paths, &declaration, false).unwrap();
        write_model_cache(
            &paths,
            &ProviderModelCache {
                schema_version: 1,
                provider: "together".to_string(),
                provider_fingerprint: None,
                credential_fingerprint: None,
                fetched_at: "1".to_string(),
                models: vec![ModelDeclaration {
                    id: "old".to_string(),
                    context_length: None,
                }],
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(model_cache_path(&paths, "together").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        write(&paths, &declaration, true).unwrap();

        assert!(!model_cache_path(&paths, "together").unwrap().exists());
    }

    #[test]
    fn read_model_cache_ignores_provider_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            model_cache_path(&paths, "example").unwrap(),
            r#"{"schema_version":1,"provider":"other","fetched_at":"1","models":[{"id":"wrong","context_length":null}]}"#,
        )
        .unwrap();

        let cache = read_model_cache(&paths, "example").unwrap();

        assert!(cache.is_none());
    }

    #[test]
    fn read_model_cache_ignores_schema_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            model_cache_path(&paths, "example").unwrap(),
            r#"{"schema_version":2,"provider":"example","fetched_at":"1","models":[{"id":"future","context_length":null}]}"#,
        )
        .unwrap();

        let cache = read_model_cache(&paths, "example").unwrap();

        assert!(cache.is_none());
    }

    #[test]
    fn read_model_cache_for_provider_ignores_fingerprint_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let mut old_declaration = provider("example");
        old_declaration.model = "old".to_string();
        let new_declaration = provider("example");
        write_model_cache(
            &paths,
            &ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: Some(provider_cache_fingerprint(&old_declaration).unwrap()),
                credential_fingerprint: None,
                fetched_at: "1".to_string(),
                models: vec![ModelDeclaration {
                    id: "old".to_string(),
                    context_length: None,
                }],
            },
        )
        .unwrap();

        let cache = read_model_cache_for_provider(&paths, &new_declaration, None).unwrap();

        assert!(cache.is_none());
    }

    #[test]
    fn read_model_cache_for_provider_rejects_fingerprintless_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let declaration = provider("example");
        write_model_cache(
            &paths,
            &ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: None,
                credential_fingerprint: None,
                fetched_at: "1".to_string(),
                models: vec![ModelDeclaration {
                    id: "large".to_string(),
                    context_length: None,
                }],
            },
        )
        .unwrap();

        let cache = read_model_cache_for_provider(&paths, &declaration, None).unwrap();

        assert!(cache.is_none());
    }

    #[test]
    fn read_model_cache_for_provider_rejects_credential_cache_when_credential_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let declaration = provider("example");
        write_model_cache(
            &paths,
            &ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: Some(provider_cache_fingerprint(&declaration).unwrap()),
                credential_fingerprint: Some("credential-a".to_string()),
                fetched_at: "1".to_string(),
                models: vec![ModelDeclaration {
                    id: "large".to_string(),
                    context_length: None,
                }],
            },
        )
        .unwrap();

        let cache =
            read_model_cache_for_provider_with_unavailable_credential(&paths, &declaration)
                .unwrap();

        assert!(cache.is_none());
    }

    #[test]
    fn read_model_cache_for_provider_rejects_credential_cache_without_provider_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let declaration = provider("example");
        write_model_cache(
            &paths,
            &ProviderModelCache {
                schema_version: 1,
                provider: "example".to_string(),
                provider_fingerprint: None,
                credential_fingerprint: Some("credential-a".to_string()),
                fetched_at: "1".to_string(),
                models: vec![ModelDeclaration {
                    id: "large".to_string(),
                    context_length: None,
                }],
            },
        )
        .unwrap();

        let cache =
            read_model_cache_for_provider_with_unavailable_credential(&paths, &declaration)
                .unwrap();

        assert!(cache.is_none());
    }

    #[test]
    fn detects_secret_bearing_legacy_replacement_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nTOGETHER_API_KEY=old\n",
        )
        .unwrap();

        let files = secret_bearing_replacement_files(&paths, "local").unwrap();

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("local.env"));
    }

    #[test]
    fn detects_secret_bearing_canonical_replacement_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","openai_api_key":"secret","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();

        let files = secret_bearing_replacement_files(&paths, "local").unwrap();

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("local.json"));
    }

    #[test]
    fn rejects_unsupported_provider_type() {
        let mut declaration = provider("custom");
        declaration.provider_type = "custom".to_string();

        let err = validate_declaration(&declaration).unwrap_err();

        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn read_all_reads_legacy_env_without_migrating_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret\n",
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.name, "local");
        assert_eq!(records[0].declaration.key_env, "LOCAL_API_KEY");
        assert_eq!(records[0].inline_secret.as_deref(), Some("secret"));
        assert_eq!(records[0].path, paths.providers_dir().join("local.env"));
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(paths.providers_dir().join("local.env").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn read_named_ignores_unrelated_malformed_provider_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(paths.providers_dir().join("broken.json"), "{not json").unwrap();

        let record = read(&paths, "local").unwrap().unwrap();

        assert_eq!(record.declaration.name, "local");
    }

    #[test]
    fn read_all_normalizes_legacy_env_health_url_to_path() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=mini\nHEALTH_URL=http://127.0.0.1:8080/health\nAPI_KEY=secret\n",
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records[0].declaration.health_path, "/health");
    }

    #[test]
    fn read_all_normalizes_legacy_json_health_url_to_path() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080/v1","default_model":"mini","health_url":"http://127.0.0.1:8080/health","api_key":"secret"}"#,
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records[0].declaration.health_path, "/health");
    }

    #[test]
    fn read_all_rejects_cross_origin_legacy_health_url() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080/v1\nMODEL=mini\nHEALTH_URL=http://127.0.0.1:9090/health\nAPI_KEY=secret\n",
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Usage(message) if message.contains("same origin")));
    }

    #[test]
    fn read_all_treats_legacy_env_key_env_as_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY_ENV=LOCAL_API_KEY\nTOKEN_ENV=IGNORED_TOKEN_KEY\n",
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.key_env, "LOCAL_API_KEY");
        assert!(records[0].inline_secret.is_none());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn read_all_uses_custom_explicit_legacy_env_secret_field() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=FOO\nFOO=sk-live\n",
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.key_env, "FOO");
        assert_eq!(records[0].inline_secret.as_deref(), Some("sk-live"));
    }

    #[test]
    fn read_all_treats_legacy_json_key_env_as_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","token_env":"IGNORED_TOKEN_KEY"}"#,
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.key_env, "LOCAL_API_KEY");
        assert!(records[0].inline_secret.is_none());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn read_all_ignores_unrelated_legacy_env_token_when_key_env_is_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=OPENAI_API_KEY\nPROXY_TOKEN=proxy-secret\n",
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.key_env, "OPENAI_API_KEY");
        assert!(records[0].inline_secret.is_none());
    }

    #[test]
    fn read_all_prefers_explicit_legacy_env_secret_field_over_generic_field() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=OPENAI_API_KEY\nAPI_KEY=generic-secret\nOPENAI_API_KEY=real-secret\n",
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.key_env, "OPENAI_API_KEY");
        assert_eq!(records[0].inline_secret.as_deref(), Some("real-secret"));
    }

    #[test]
    fn read_all_ignores_unrelated_legacy_json_token_when_key_env_is_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"OPENAI_API_KEY","proxy_token":"proxy-secret"}"#,
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.key_env, "OPENAI_API_KEY");
        assert!(records[0].inline_secret.is_none());
    }

    #[test]
    fn read_all_does_not_migrate_legacy_secret_over_existing_agent_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=different\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret\n",
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert!(paths.providers_dir().join("local.env").exists());
        assert!(!paths.providers_dir().join("local.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=different\n"
        );
    }

    #[test]
    fn migrate_legacy_rejects_inline_secret_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=different\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret\n",
        )
        .unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("conflicts with existing LOCAL_API_KEY"))
        );
        assert!(paths.providers_dir().join("local.env").exists());
        assert!(!paths.providers_dir().join("local.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=different\n"
        );
    }

    #[test]
    fn migrate_legacy_rejects_unmigratable_env_secret_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=OPENAI_API_KEY\nPROXY_TOKEN=proxy-secret\n";
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("PROXY_TOKEN"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_unsupported_env_field_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nORG_ID=org-123\n";
        fs::write(&legacy_file, legacy).unwrap();

        let records = read_all(&paths).unwrap();
        assert_eq!(records.len(), 1);

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("unsupported field ORG_ID"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_unrelated_env_secret_without_provider_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy =
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nPROXY_TOKEN=proxy-secret\n";
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("PROXY_TOKEN"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_binds_matching_existing_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret\n",
        )
        .unwrap();

        let migrated = migrate_legacy(&paths).unwrap();

        assert_eq!(migrated, vec![paths.providers_dir().join("local.json")]);
        assert!(paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("local.env").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_recovers_journal_before_publishing_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret\n",
        )
        .unwrap();
        let migrations = legacy_migrations(&paths).unwrap();
        let migration = &migrations[0];
        write_declaration(&migration.canonical_path, &migration.declaration).unwrap();
        let journal = write_legacy_migration_journal(&paths, migration).unwrap();
        state::remove_file_durable(&migration.path).unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(!journal.exists());
        assert!(!paths.providers_dir().join("local.env").exists());
        assert!(paths.providers_dir().join("local.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_rejects_stale_journal_after_legacy_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        fs::write(
            &legacy_file,
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret\n",
        )
        .unwrap();
        let migrations = legacy_migrations(&paths).unwrap();
        let migration = &migrations[0];
        let journal = write_legacy_migration_journal(&paths, migration).unwrap();
        let changed_legacy = "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:9090\nMODEL=changed\nAPI_KEY=changed-secret\n";
        fs::write(&legacy_file, changed_legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("no longer matches"))
        );
        assert!(journal.exists());
        assert_eq!(fs::read_to_string(&legacy_file).unwrap(), changed_legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_stale_journal_after_key_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret\n",
        )
        .unwrap();
        let migrations = legacy_migrations(&paths).unwrap();
        let migration = &migrations[0];
        write_declaration(&migration.canonical_path, &migration.declaration).unwrap();
        let journal = write_legacy_migration_journal(&paths, migration).unwrap();
        state::remove_file_durable(&migration.path).unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=rotated\n").unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("conflicts with existing LOCAL_API_KEY"))
        );
        assert!(journal.exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=rotated\n"
        );
    }

    #[test]
    fn migrate_legacy_rejects_stale_journal_after_provider_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret\n",
        )
        .unwrap();
        let migrations = legacy_migrations(&paths).unwrap();
        let migration = &migrations[0];
        let journal = write_legacy_migration_journal(&paths, migration).unwrap();
        state::remove_file_durable(&migration.path).unwrap();
        let mut replacement = migration.declaration.clone();
        replacement.url = "https://api.example.test/v1".to_string();
        replacement.model = "replacement".to_string();
        write_declaration(&migration.canonical_path, &replacement).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("conflicts with existing provider declaration"))
        );
        assert!(journal.exists());
        let canonical = read_canonical(&paths, "local").unwrap().unwrap();
        assert_eq!(canonical.declaration, replacement);
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_conflicting_existing_provider_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::create_dir_all(&paths.etc_dir).unwrap();
        fs::write(paths.agent_env(), "LOCAL_API_KEY=secret\n").unwrap();
        agent_env::set_provider_binding(
            &paths,
            "local",
            "LOCAL_API_KEY",
            "https://api.old.test",
        )
        .unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=local\nPROVIDER_URL=https://api.new.test\nMODEL=mini\nKEY_ENV=LOCAL_API_KEY\nLOCAL_API_KEY=secret\n";
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("different key or URL"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(agent_env::provider_binding_matches(
            &agent_env::read(&paths).unwrap(),
            "local",
            "LOCAL_API_KEY",
            "https://api.old.test"
        )
        .unwrap());
    }

    #[test]
    fn migrate_legacy_migrates_custom_explicit_env_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=FOO\nFOO=sk-live\n",
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("local.env").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "FOO=sk-live\nNCZ_PROVIDER_BINDING_6C6F63616C=\"FOO http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_migrates_env_authorization_bearer_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=LOCAL_API_KEY\nAUTHORIZATION=Bearer env-secret\n",
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("local.env").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=env-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_migrates_env_auth_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=LOCAL_API_KEY\nAUTH=auth-secret\n",
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("local.env").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=auth-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_migrates_env_bearer_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=LOCAL_API_KEY\nBEARER=bearer-secret\n",
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("local.env").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=bearer-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_migrates_json_auth_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","auth":"json-secret"}"#,
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("local-legacy.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=json-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_rejects_mixed_env_auth_secret_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=API_KEY\nAPI_KEY=api-secret\nAUTH=auth-secret\n";
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("AUTH")));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_env_non_bearer_authorization_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local.env");
        let legacy = "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nKEY_ENV=LOCAL_API_KEY\nAUTHORIZATION=Basic bG9jYWw6c2VjcmV0\n";
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("AUTHORIZATION")));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_uses_last_duplicate_env_assignments() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nPROVIDER_URL=http://127.0.0.1:9090/v1\nMODEL=old\nMODEL=new\nKEY_ENV=OLD_API_KEY\nKEY_ENV=NEW_API_KEY\nNEW_API_KEY=old-secret\nNEW_API_KEY=new-secret\n",
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        let provider = read(&paths, "local").unwrap().unwrap().declaration;
        assert_eq!(provider.url, "http://127.0.0.1:9090/v1");
        assert_eq!(provider.model, "new");
        assert_eq!(provider.key_env, "NEW_API_KEY");
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "NEW_API_KEY=new-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"NEW_API_KEY http://127.0.0.1:9090/v1\"\n"
        );
        assert!(!paths.providers_dir().join("local.env").exists());
    }

    fn migrate_legacy_secret_from_env_line(line: &str) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            format!(
                "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\n{line}\n"
            ),
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        let entries = agent_env::read(&paths).unwrap();
        let secret = entries
            .iter()
            .find(|entry| entry.key == "LOCAL_API_KEY")
            .map(|entry| entry.value.clone())
            .unwrap();
        assert!(!paths.providers_dir().join("local.env").exists());
        secret
    }

    #[test]
    fn migrate_legacy_preserves_single_quoted_secret() {
        assert_eq!(
            migrate_legacy_secret_from_env_line("API_KEY='sk-live' # inline comment"),
            "sk-live"
        );
    }

    #[test]
    fn migrate_legacy_preserves_escaped_quote_in_double_quoted_secret() {
        assert_eq!(
            migrate_legacy_secret_from_env_line("API_KEY=\"a\\\"b\""),
            "a\"b"
        );
    }

    #[test]
    fn migrate_legacy_preserves_double_quoted_backslash_secret() {
        assert_eq!(
            migrate_legacy_secret_from_env_line("API_KEY=\"a\\\\b\""),
            "a\\b"
        );
    }

    #[test]
    fn migrate_legacy_strips_inline_comment_after_unquoted_secret() {
        assert_eq!(
            migrate_legacy_secret_from_env_line("API_KEY=sk-live # inline comment"),
            "sk-live"
        );
    }

    #[test]
    fn read_all_keeps_conflicting_legacy_provider_when_canonical_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"new","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=old\nAPI_KEY=secret\n",
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(paths.providers_dir().join("local.env").exists());
        assert_eq!(
            fs::read_to_string(paths.providers_dir().join("local.json")).unwrap(),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"new","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#
        );
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_static_model_collision_with_canonical_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health","models":["mini"]}"#,
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"https://api.example.test","default_model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health","models":["mini","large"]}"#,
        )
        .unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(paths.providers_dir().join("local-legacy.json").exists());
        assert_eq!(
            fs::read_to_string(paths.providers_dir().join("local.json")).unwrap(),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health","models":["mini"]}"#
        );
    }

    #[test]
    fn read_all_keeps_credential_distinct_legacy_provider_when_canonical_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=https://api.example.test\nMODEL=mini\nKEY_ENV=OTHER_API_KEY\nAPI_KEY=secret\n",
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(paths.providers_dir().join("local.env").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn read_all_keeps_invalid_legacy_file_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\n",
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(paths.providers_dir().join("local.env").exists());
        assert!(!paths.providers_dir().join("local.json").exists());
    }

    #[test]
    fn read_all_rejects_invalid_canonical_json_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"file:///tmp/provider","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
    }

    #[test]
    fn read_all_rejects_canonical_json_filename_name_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("old.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
    }

    #[test]
    fn read_all_rejects_unsupported_provider_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":2,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
    }

    #[test]
    fn read_all_reads_schema_less_legacy_json_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key":"secret"}"#,
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.name, "local");
        assert_eq!(records[0].declaration.url, "http://127.0.0.1:8080");
        assert_eq!(records[0].declaration.model, "mini");
        assert_eq!(records[0].declaration.key_env, "LOCAL_API_KEY");
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn read_all_reads_schema_less_legacy_json_provider_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key":"secret"}"#,
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.name, "local");
        assert_eq!(records[0].path, paths.providers_dir().join("local-legacy.json"));
    }

    #[test]
    fn read_all_skips_equivalent_schema_less_json_alias_when_canonical_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key":"secret"}"#,
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.name, "local");
        assert_eq!(records[0].path, paths.providers_dir().join("local.json"));
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_migrates_schema_less_json_provider_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key":"secret"}"#,
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(!paths.providers_dir().join("local-legacy.json").exists());
        assert!(paths.providers_dir().join("local.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_migrates_nested_json_header_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","headers":{"Authorization":"Bearer nested-secret"}}"#,
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(!paths.providers_dir().join("local-legacy.json").exists());
        assert!(paths.providers_dir().join("local.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=nested-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_migrates_json_header_name_value_array_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","headers":[{"name":"Authorization","value":"Bearer array-secret"}]}"#,
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(!paths.providers_dir().join("local-legacy.json").exists());
        assert!(paths.providers_dir().join("local.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=array-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_rejects_json_x_api_key_header_pair_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","headers":[{"name":"X-Api-Key","value":"array-secret"}]}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("X-Api-Key")));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_json_api_key_header_map_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","headers":{"api-key":"map-secret"}}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("headers.api-key"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_json_key_header_map_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","headers":{"key":"map-secret"}}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("headers.key")));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_json_proxy_authorization_header_pair_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","request_headers":[{"name":"Proxy-Authorization","value":"Bearer proxy-secret"}]}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("Proxy-Authorization"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_migrates_json_request_header_list_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","request_headers":[{"key":"Authorization","value":"Bearer request-secret"}]}"#,
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert!(!paths.providers_dir().join("local-legacy.json").exists());
        assert!(paths.providers_dir().join("local.json").exists());
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=request-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_rejects_unrecognized_json_header_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","headers":[{"name":"X-Custom-Header","value":"custom-value"}]}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("X-Custom-Header"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_non_bearer_authorization_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","headers":{"Authorization":"Basic bG9jYWw6c2VjcmV0"}}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(message) if message.contains("Authorization")));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_migrates_nested_json_token_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","auth":{"token":"nested-secret"}}"#,
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        let provider = read(&paths, "local").unwrap().unwrap().declaration;
        assert_eq!(provider.key_env, "LOCAL_TOKEN");
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_TOKEN=nested-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_TOKEN http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_migrates_json_array_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","credentials":[{"api_key":"array-secret"}]}"#,
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        let provider = read(&paths, "local").unwrap().unwrap().declaration;
        assert_eq!(provider.key_env, "LOCAL_API_KEY");
        assert_eq!(
            fs::read_to_string(paths.agent_env()).unwrap(),
            "LOCAL_API_KEY=array-secret\nNCZ_PROVIDER_BINDING_6C6F63616C=\"LOCAL_API_KEY http://127.0.0.1:8080\"\n"
        );
    }

    #[test]
    fn migrate_legacy_rejects_unmappable_json_secret_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key_env":"LOCAL_API_KEY","proxy_token":"proxy-secret"}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_unsupported_json_field_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","organization":"org-123"}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let records = read_all(&paths).unwrap();
        assert_eq!(records.len(), 1);

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("unsupported field organization"))
        );
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_rejects_mixed_json_secrets_before_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        let legacy_file = paths.providers_dir().join("local-legacy.json");
        let legacy = r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","api_key":"secret","headers":[{"proxy_token":"proxy-secret"}]}"#;
        fs::write(&legacy_file, legacy).unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert_eq!(fs::read_to_string(legacy_file).unwrap(), legacy);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn migrate_legacy_ignores_json_model_token_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local-legacy.json"),
            r#"{"provider":"local","base_url":"http://127.0.0.1:8080","default_model":"mini","models":[{"id":"mini","tokenizer":"cl100k","max_output_tokens":"8192"}]}"#,
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        let provider = read(&paths, "local").unwrap().unwrap().declaration;
        assert_eq!(provider.key_env, "API_KEY");
        assert!(provider.models.iter().any(|model| model.id == "mini"));
        assert!(!paths.agent_env().exists());
    }

    #[test]
    fn read_all_rejects_malformed_canonical_json_schema_fields() {
        let cases = [
            (
                "missing-key-env",
                r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","type":"openai-compat","health_path":"/health"}"#,
            ),
            (
                "empty-key-env",
                r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"","type":"openai-compat","health_path":"/health"}"#,
            ),
            (
                "wrong-key-env",
                r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":42,"type":"openai-compat","health_path":"/health"}"#,
            ),
            (
                "missing-name",
                r#"{"schema_version":1,"url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
            ),
            (
                "empty-name",
                r#"{"schema_version":1,"name":"","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
            ),
            (
                "wrong-name",
                r#"{"schema_version":1,"name":42,"url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
            ),
            (
                "wrong-schema",
                r#"{"schema_version":"1","name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
            ),
        ];

        for (case, body) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let paths = test_paths(tmp.path());
            fs::create_dir_all(paths.providers_dir()).unwrap();
            fs::write(paths.providers_dir().join("local.json"), body).unwrap();

            let err = match read_all(&paths) {
                Ok(_) => panic!("case {case} unexpectedly parsed"),
                Err(err) => err,
            };

            assert!(
                matches!(err, NczError::Precondition(_)),
                "case {case} returned {err:?}"
            );
        }
    }

    #[test]
    fn read_all_rejects_canonical_json_inline_secret() {
        for body in [
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"mini","key_env":"LOCAL_API_KEY","api_key":"secret","type":"openai-compat","health_path":"/health"}"#,
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"mini","key_env":"LOCAL_API_KEY","headers":{"Authorization":"Bearer secret"},"type":"openai-compat","health_path":"/health"}"#,
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"mini","key_env":"LOCAL_API_KEY","models":[{"id":"mini","api_key":"secret"}],"type":"openai-compat","health_path":"/health"}"#,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let paths = test_paths(tmp.path());
            fs::create_dir_all(paths.providers_dir()).unwrap();
            fs::write(paths.providers_dir().join("local.json"), body).unwrap();

            let err = read_all(&paths).unwrap_err();

            assert!(matches!(err, NczError::Precondition(_)));
            assert!(paths.providers_dir().join("local.json").exists());
        }
    }

    #[test]
    fn read_all_rejects_canonical_json_unknown_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health","extra":"value"}"#,
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
    }

    #[test]
    fn read_all_allows_canonical_json_model_token_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"https://api.example.test","model":"mini","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health","models":[{"id":"mini","tokenizer":"cl100k","max_output_tokens":"8192"}]}"#,
        )
        .unwrap();

        let records = read_all(&paths).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].declaration.name, "local");
        assert_eq!(records[0].declaration.models[0].id, "mini");
    }

    #[test]
    fn remove_deletes_matching_legacy_provider_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\n",
        )
        .unwrap();

        let removed = remove(&paths, "local").unwrap();

        assert!(removed);
        assert!(!paths.providers_dir().join("local.env").exists());
    }

    #[test]
    fn remove_deletes_matching_legacy_provider_with_invalid_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("bad$key.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\n",
        )
        .unwrap();

        let removed = remove(&paths, "local").unwrap();

        assert!(removed);
        assert!(!paths.providers_dir().join("bad$key.env").exists());
    }

    #[test]
    fn removal_aliases_ignore_invalid_legacy_filename_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("bad$key.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\n",
        )
        .unwrap();

        let aliases = removal_aliases(&paths, "local").unwrap();

        assert!(aliases.contains("local"));
        assert!(!aliases.contains("bad$key"));
    }

    #[test]
    fn remove_deletes_filename_match_with_invalid_declared_name() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"../bad","url":"https://api.example.test","model":"m","key_env":"LOCAL_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();

        let removed = remove(&paths, "local").unwrap();

        assert!(removed);
        assert!(!paths.providers_dir().join("local.json").exists());
    }

    #[test]
    fn remove_deletes_matching_json_alias_provider_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"prod","url":"https://api.example.test","model":"m","key_env":"PROD_API_KEY","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();
        write_model_cache(
            &paths,
            &ProviderModelCache {
                schema_version: 1,
                provider: "prod".to_string(),
                provider_fingerprint: None,
                credential_fingerprint: None,
                fetched_at: "1".to_string(),
                models: Vec::new(),
            },
        )
        .unwrap();

        let removed = remove(&paths, "prod").unwrap();

        assert!(removed);
        assert!(!paths.providers_dir().join("local.json").exists());
        assert!(!paths.providers_dir().join("prod.models.json").exists());
    }

    #[test]
    fn rejects_path_traversal_names() {
        let err = validate_name("../bad").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_model_cache_suffix_names() {
        let err = validate_name("foo.models").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_non_http_provider_urls() {
        let err = validate_provider_url("file:///tmp/provider").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_malformed_provider_url_authorities() {
        for url in [
            "https://:443",
            "https://api.example.test:",
            "https://api.example.test:not-a-port",
            "https://api.example.test:65536",
            "https://[::1",
            "https://::1:443",
        ] {
            let err = validate_provider_url(url).unwrap_err();
            assert!(matches!(err, NczError::Usage(_)));
        }
    }

    #[test]
    fn rejects_provider_urls_with_userinfo() {
        let err = validate_provider_url("https://token@api.example.test").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_malformed_provider_userinfo_without_echoing_secret() {
        for url in [
            "ftp://user:secret@api.example.test",
            " https://user:secret@api.example.test",
            "https://user:secret@:443",
        ] {
            let err = validate_provider_url(url).unwrap_err();
            let message = err.to_string();
            assert!(message.contains("userinfo"));
            assert!(!message.contains("secret"));
            assert!(!message.contains(url));
        }
    }

    #[test]
    fn rejects_malformed_provider_credential_urls_without_echoing_secret() {
        for url in [
            "ftp://api.example.test/v1?api_key=secret",
            "https://api.example.test:notaport/v1?api_key=secret",
            " ftp://api.example.test/v1#token=secret",
            "https://api.example.test:notaport/token/secret",
            "ftp://api.example.test/v1/sk-live",
            "https://api.example.test:sk-live/v1",
            "api_key=secret",
            "sk-live",
        ] {
            let err = validate_provider_url(url).unwrap_err();
            let message = err.to_string();
            assert!(!message.contains("secret"));
            assert!(!message.contains("sk-live"));
            assert!(!message.contains(url));
        }
    }

    #[test]
    fn rejects_provider_urls_with_query_credentials() {
        let err = validate_provider_url("https://api.example.test/v1?api_key=secret").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_provider_urls_with_fragment_credentials() {
        let err = validate_provider_url("https://api.example.test/v1#token=secret").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_provider_urls_with_path_credentials() {
        for url in [
            "https://api.example.test/token/sk-live",
            "https://api.example.test/v1;token=secret",
            "https://api.example.test/%74oken/secret",
            "https://api.example.test/v1/sk-live",
            "https://api.example.test/v1%2Fsk-live",
            "https://api.example.test/v1%3Btoken=secret",
        ] {
            let err = validate_provider_url(url).unwrap_err();
            assert!(
                matches!(err, NczError::Usage(_)),
                "provider URL was accepted: {url}"
            );
        }
    }

    #[test]
    fn rejects_absolute_provider_health_paths() {
        let err = validate_health_path("https://metadata.example.test/token").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
    }

    #[test]
    fn rejects_provider_health_paths_with_query_fragment_or_traversal() {
        for path in [
            "/health?token=secret",
            "/health#token=secret",
            "/../metadata",
            "/health check",
        ] {
            let err = validate_health_path(path).unwrap_err();
            assert!(matches!(err, NczError::Usage(_)));
        }
    }

    #[test]
    fn rejects_provider_health_paths_with_path_credentials() {
        for path in [
            "/token/sk-live",
            "/health;token=secret",
            "/%74oken/secret",
            "/health/sk-live",
        ] {
            let err = validate_health_path(path).unwrap_err();
            assert!(
                matches!(err, NczError::Usage(_)),
                "provider health path was accepted: {path}"
            );
        }
    }

    #[test]
    fn parses_static_models_as_strings_and_objects() {
        let value: Value = serde_json::json!({
            "models": [
                "small",
                {"id": "large", "context_length": 200000}
            ]
        });

        let models = models_from_value(value.get("models"));

        assert_eq!(models[0].id, "small");
        assert_eq!(models[1].context_length, Some(200000));
    }
}
