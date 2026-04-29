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
use serde_json::Value;

use crate::error::NczError;
use crate::state::{self, agent_env, url as url_state, Paths};

pub const OPENAI_COMPAT_PROVIDER_TYPE: &str = "openai-compat";
const DEFAULT_PROVIDER_TYPE: &str = OPENAI_COMPAT_PROVIDER_TYPE;
const DEFAULT_HEALTH_PATH: &str = "/health";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderDeclaration {
    pub schema_version: u32,
    pub name: String,
    pub url: String,
    pub model: String,
    pub key_env: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub health_path: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderModelCache {
    pub schema_version: u32,
    pub provider: String,
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
    let mut seen = BTreeMap::new();
    let files = provider_files(paths)?;
    preflight_provider_files(&files)?;
    for path in files {
        let Some(record) = read_record(paths, &path)? else {
            continue;
        };
        if let Some(previous) = seen.insert(record.declaration.name.clone(), record.path.clone()) {
            return Err(NczError::Precondition(format!(
                "duplicate provider declaration {} in {} and {}",
                record.declaration.name,
                previous.display(),
                record.path.display()
            )));
        }
        records.push(record);
    }
    records.sort_by(|a, b| a.declaration.name.cmp(&b.declaration.name));
    Ok(records)
}

pub fn migrate_legacy(paths: &Paths) -> Result<Vec<PathBuf>, NczError> {
    let migrations = legacy_migrations(paths)?;

    if migrations.is_empty() {
        return Ok(Vec::new());
    }

    let mut snapshot_paths = vec![paths.agent_env()];
    snapshot_paths.extend(legacy_migration_file_paths(&migrations));
    let snapshots = snapshot_files(&snapshot_paths)?;
    let result = migrate_legacy_inner(paths, &migrations);
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

pub fn legacy_migration_snapshot_paths(paths: &Paths) -> Result<Vec<PathBuf>, NczError> {
    Ok(legacy_migration_file_paths(&legacy_migrations(paths)?))
}

struct LegacyMigration {
    path: PathBuf,
    declaration: ProviderDeclaration,
    inline_secret: Option<String>,
    canonical_path: PathBuf,
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
            parse_legacy_json(value, fallback_name)
        } else {
            parse_env(&body, fallback_name, &path)?
        };
        validate_declaration(&parsed.declaration).map_err(|err| {
            NczError::Precondition(format!("invalid legacy provider {}: {err}", path.display()))
        })?;
        let canonical_path = declaration_path(paths, &parsed.declaration.name)?;
        migrations.push(LegacyMigration {
            path,
            declaration: parsed.declaration,
            inline_secret: parsed.inline_secret,
            canonical_path,
        });
    }
    Ok(migrations)
}

fn legacy_migration_file_paths(migrations: &[LegacyMigration]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for migration in migrations {
        paths.push(migration.path.clone());
        paths.push(migration.canonical_path.clone());
    }
    paths.sort();
    paths.dedup();
    paths
}

fn migrate_legacy_inner(paths: &Paths, migrations: &[LegacyMigration]) -> Result<(), NczError> {
    let mut canonical_declarations: BTreeMap<String, ProviderDeclaration> = BTreeMap::new();
    for migration in migrations {
        if let Some(secret) = &migration.inline_secret {
            match agent_env_value(paths, &migration.declaration.key_env)? {
                Some(existing) if existing == *secret => {
                    agent_env::set_provider_binding(
                        paths,
                        &migration.declaration.name,
                        &migration.declaration.key_env,
                        &migration.declaration.url,
                    )?;
                }
                Some(_) => {
                    return Err(NczError::Precondition(format!(
                        "legacy provider {} inline credential conflicts with existing {}; leaving legacy provider file in place",
                        migration.path.display(),
                        migration.declaration.key_env
                    )));
                }
                None => {
                    agent_env::set(paths, &migration.declaration.key_env, secret)?;
                    agent_env::set_provider_binding(
                        paths,
                        &migration.declaration.name,
                        &migration.declaration.key_env,
                        &migration.declaration.url,
                    )?;
                }
            }
        }

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
            write_declaration(&migration.canonical_path, &migration.declaration)?;
            canonical_declarations.insert(
                migration.declaration.name.clone(),
                migration.declaration.clone(),
            );
        }

        if migration.path != migration.canonical_path {
            state::remove_file_durable(&migration.path)?;
        }
    }
    Ok(())
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
        let declaration = parse_declaration_for_path(&body, &fallback_name, ext, path)?;
        validate_declaration(&declaration).map_err(|err| {
            NczError::Precondition(format!(
                "invalid provider declaration {}: {err}",
                path.display()
            ))
        })?;
        if ext == "json" && fallback_name != declaration.name {
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
    Ok(read_all(paths)?.into_iter().find(|record| {
        record.declaration.name == name
            || record
                .path
                .file_stem()
                .and_then(OsStr::to_str)
                .is_some_and(|stem| stem == name)
    }))
}

pub(crate) fn read_record_path(
    paths: &Paths,
    path: &Path,
) -> Result<Option<ProviderRecord>, NczError> {
    if !path.starts_with(paths.providers_dir()) {
        return Err(NczError::Precondition(format!(
            "provider path is outside providers.d: {}",
            path.display()
        )));
    }
    read_record(paths, path)
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
    }))
}

pub fn exists_without_migration(paths: &Paths, name: &str) -> Result<bool, NczError> {
    validate_name(name)?;
    for path in provider_files(paths)? {
        let body = fs::read_to_string(&path)?;
        let fallback_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string();
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let declaration = if ext == "json" {
            parse_json(&body, fallback_name.clone())?
        } else {
            parse_env(&body, fallback_name.clone(), &path)?.declaration
        };
        if declaration.name == name || fallback_name == name {
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
        let declaration = parse_declaration_for_path(&body, &fallback_name, ext, &path);
        if fallback_name == name {
            aliases.insert(fallback_name);
            if let Ok(declaration) = declaration {
                aliases.insert(declaration.name);
            }
            removed = true;
            removal_paths.push(path);
            continue;
        }
        let declaration = declaration?;
        if declaration.name == name {
            aliases.insert(fallback_name);
            aliases.insert(declaration.name);
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
        let declaration = parse_declaration_for_path(&body, &fallback_name, ext, &path);
        if fallback_name == name {
            aliases.insert(fallback_name);
            if let Ok(declaration) = declaration {
                aliases.insert(declaration.name);
            }
            continue;
        }
        let declaration = declaration?;
        if declaration.name == name {
            aliases.insert(fallback_name);
            aliases.insert(declaration.name);
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
        if let Some((key_env, secret)) =
            inline_credential_for_path(&body, ext, &fallback_name, &path)?
        {
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
    agent_env::validate_key(key)?;
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
        let declaration = parse_declaration_for_path(&body, &fallback_name, ext, &path)?;
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
        Ok(body) => Ok(Some(serde_json::from_str(&body)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(NczError::Io(e)),
    }
}

pub fn write_model_cache(paths: &Paths, cache: &ProviderModelCache) -> Result<PathBuf, NczError> {
    validate_name(&cache.provider)?;
    let path = model_cache_path(paths, &cache.provider)?;
    let mut body = serde_json::to_vec_pretty(cache)?;
    body.push(b'\n');
    state::atomic_write(&path, &body, 0o600)?;
    Ok(path)
}

pub fn validate_declaration(declaration: &ProviderDeclaration) -> Result<(), NczError> {
    if declaration.schema_version != 1 {
        return Err(NczError::Usage(format!(
            "unsupported provider schema_version: {}",
            declaration.schema_version
        )));
    }
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
    agent_env::validate_key(&declaration.key_env)?;
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

pub fn validate_provider_url(url: &str) -> Result<(), NczError> {
    let trimmed = url.trim();
    if trimmed.starts_with('-') {
        return Err(NczError::Usage(format!("invalid provider URL: {url}")));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(NczError::Usage(format!(
            "invalid provider URL scheme: {url} (expected http or https)"
        )));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(NczError::Usage(format!("invalid provider URL: {url}")));
    }
    if url_state::authority(trimmed).is_none() {
        return Err(NczError::Usage(format!("invalid provider URL: {url}")));
    }
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
    Ok(())
}

pub fn validate_health_path(path: &str) -> Result<(), NczError> {
    let trimmed = path.trim();
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
    Ok(())
}

fn reject_insecure_credential_url(url: &str) -> Result<(), NczError> {
    let trimmed = url.trim();
    if !trimmed.to_ascii_lowercase().starts_with("http://") {
        return Ok(());
    }
    let Some(host) = url_state::host(trimmed) else {
        return Err(NczError::Usage(format!("invalid provider URL: {url}")));
    };
    if url_state::is_loopback_host(host) {
        return Ok(());
    }
    Err(NczError::Usage(format!(
        "provider URL with credentials cannot use plaintext HTTP to {host}; use https or a loopback provider URL"
    )))
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
        let parsed = parse_json_full(&body, fallback_name.clone())?;
        validate_declaration(&parsed.declaration).map_err(|err| {
            NczError::Precondition(format!(
                "invalid provider declaration {}: {err}",
                path.display()
            ))
        })?;
        if fallback_name != parsed.declaration.name {
            return Err(NczError::Precondition(format!(
                "provider declaration filename {} does not match declared name {}",
                path.display(),
                parsed.declaration.name
            )));
        }
        return Ok(Some(ProviderRecord {
            declaration: parsed.declaration,
            path: path.to_path_buf(),
            inline_secret: parsed.inline_secret,
        }));
    }

    let parsed = parse_env(&body, fallback_name, path)?;
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
        let declaration = if ext == "json" {
            parse_json(&body, fallback_name)?
        } else {
            parse_env(&body, fallback_name, &path)?.declaration
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
    source: &Path,
) -> Result<ProviderDeclaration, NczError> {
    if ext == "json" {
        parse_json(body, fallback_name.to_string())
    } else {
        Ok(parse_env(body, fallback_name.to_string(), source)?.declaration)
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
}

fn is_model_cache_file(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.ends_with(".models.json"))
}

fn parse_json(body: &str, fallback_name: String) -> Result<ProviderDeclaration, NczError> {
    Ok(parse_json_full(body, fallback_name)?.declaration)
}

fn parse_json_full(body: &str, fallback_name: String) -> Result<ParsedProvider, NczError> {
    let value: Value = serde_json::from_str(body)?;
    if value.get("schema_version").is_none() {
        return Ok(parse_legacy_json(value, fallback_name));
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
    })
}

fn parse_legacy_json(value: Value, fallback_name: String) -> ParsedProvider {
    let name = legacy_string_field(&value, &["name", "provider"]).unwrap_or(fallback_name);
    let explicit_key_env =
        legacy_string_field(&value, &["key_env", "keyEnv", "api_key_env", "token_env"]);
    let legacy_secret = legacy_json_secret(&value, &name, explicit_key_env.as_deref());
    let key_env = explicit_key_env
        .or_else(|| legacy_secret.as_ref().map(|(key_env, _)| key_env.clone()))
        .unwrap_or_else(|| "API_KEY".to_string());
    ParsedProvider {
        declaration: ProviderDeclaration {
            schema_version: 1,
            name,
            url: legacy_string_field(&value, &["url", "base_url", "endpoint"]).unwrap_or_default(),
            model: legacy_string_field(&value, &["model", "default_model"]).unwrap_or_default(),
            key_env,
            provider_type: legacy_string_field(&value, &["type", "provider_type"])
                .unwrap_or_else(|| DEFAULT_PROVIDER_TYPE.to_string()),
            health_path: legacy_string_field(&value, &["health_path", "health_url"])
                .unwrap_or_else(|| DEFAULT_HEALTH_PATH.to_string()),
            models: models_from_value(value.get("models")),
        },
        inline_secret: legacy_secret.map(|(_, secret)| secret),
    }
}

fn parse_env(
    body: &str,
    fallback_name: String,
    source: &Path,
) -> Result<ParsedProvider, NczError> {
    let pairs = env_pairs(body, source)?;

    let name = lookup(&pairs, &["PROVIDER_NAME", "NAME"]).unwrap_or(fallback_name);
    let explicit_key_env = lookup(&pairs, &["KEY_ENV", "API_KEY_ENV", "TOKEN_ENV", "AUTH_ENV"]);
    let legacy_secret = legacy_env_secret(&pairs, &name, explicit_key_env.as_deref());
    let key_env = explicit_key_env
        .or_else(|| legacy_secret.as_ref().map(|(key_env, _)| key_env.clone()))
        .unwrap_or_else(|| "API_KEY".to_string());

    Ok(ParsedProvider {
        declaration: ProviderDeclaration {
            schema_version: 1,
            name,
            url: lookup(&pairs, &["PROVIDER_URL", "BASE_URL", "ENDPOINT", "URL"])
                .unwrap_or_default(),
            model: lookup(&pairs, &["MODEL", "DEFAULT_MODEL", "PROVIDER_MODEL"])
                .unwrap_or_default(),
            key_env,
            provider_type: lookup(&pairs, &["PROVIDER_TYPE", "TYPE"])
                .unwrap_or_else(|| DEFAULT_PROVIDER_TYPE.to_string()),
            health_path: lookup(
                &pairs,
                &["HEALTH_PATH", "PROVIDER_HEALTH_URL", "HEALTH_URL"],
            )
            .unwrap_or_else(|| DEFAULT_HEALTH_PATH.to_string()),
            models: Vec::new(),
        },
        inline_secret: legacy_secret.map(|(_, secret)| secret),
    })
}

fn inline_credential_for_path(
    body: &str,
    ext: &str,
    fallback_name: &str,
    source: &Path,
) -> Result<Option<(String, String)>, NczError> {
    if ext == "json" {
        let value: Value = serde_json::from_str(body)?;
        if value.get("schema_version").is_some() {
            return Ok(canonical_json_inline_credential(&value, fallback_name));
        }
        let parsed = parse_legacy_json(value, fallback_name.to_string());
        return Ok(parsed
            .inline_secret
            .map(|secret| (parsed.declaration.key_env, secret)));
    }
    let parsed = parse_env(body, fallback_name.to_string(), source)?;
    Ok(parsed
        .inline_secret
        .map(|secret| (parsed.declaration.key_env, secret)))
}

fn canonical_json_inline_credential(
    value: &Value,
    fallback_name: &str,
) -> Option<(String, String)> {
    let field = json_inline_secret_field(value)?;
    let secret = value.get(field)?.as_str()?.to_string();
    let key_env = value
        .get("key_env")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| legacy_secret_json_key(fallback_name, field));
    Some((key_env, secret))
}

fn json_inline_secret_field(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    object.iter().find_map(|(key, value)| {
        if secret_field_name(key) && value.as_str().is_some_and(|secret| !secret.is_empty()) {
            Some(key.as_str())
        } else {
            None
        }
    })
}

fn secret_field_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if key_env_metadata_field(&name) {
        return false;
    }
    name == "key"
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

fn env_pairs(body: &str, source: &Path) -> Result<Vec<(String, String)>, NczError> {
    body.lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            Some(
                agent_env::parse_environment_file_value(value)
                    .map(|value| (key.trim().to_string(), value))
                    .map_err(|err| {
                        NczError::Precondition(format!(
                            "failed to parse legacy provider env {} line {}: {err}",
                            source.display(),
                            idx + 1
                        ))
                    }),
            )
        })
        .collect()
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

fn legacy_json_secret(
    value: &Value,
    provider_name: &str,
    explicit_key_env: Option<&str>,
) -> Option<(String, String)> {
    let object = value.as_object()?;
    for (json_key, raw) in object {
        if !secret_field_name(json_key) {
            continue;
        }
        let Some(secret) = raw.as_str().filter(|secret| !secret.is_empty()) else {
            continue;
        };
        let key_env = explicit_key_env
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| legacy_secret_json_key(provider_name, json_key));
        return Some((key_env, secret.to_string()));
    }
    None
}

fn legacy_env_secret(
    pairs: &[(String, String)],
    provider_name: &str,
    explicit_key_env: Option<&str>,
) -> Option<(String, String)> {
    for (raw_key, value) in pairs {
        if secret_field_name(raw_key) && !value.is_empty() {
            let key_env = explicit_key_env
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| legacy_secret_env_key_for_raw(provider_name, raw_key));
            return Some((key_env, value.clone()));
        }
    }
    None
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
    for (key, value) in pairs {
        if keys.iter().any(|wanted| key.eq_ignore_ascii_case(wanted)) {
            return Some((key.to_ascii_uppercase(), value.clone()));
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
    fn test_force_replacement_aborts_on_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "API_KEY=\"secret\nMODEL=foo\n",
        )
        .unwrap();

        let err = inline_credential_replacements(&paths, "local").unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("local.env"))
        );
        assert!(paths.providers_dir().join("local.env").exists());
        assert!(!paths.providers_dir().join("local.json").exists());
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
    fn test_migrate_legacy_aborts_on_unparseable_quote() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "API_KEY=\"secret\nMODEL=foo\n",
        )
        .unwrap();

        let err = migrate_legacy(&paths).unwrap_err();

        assert!(
            matches!(err, NczError::Precondition(message) if message.contains("local.env"))
        );
        assert!(paths.providers_dir().join("local.env").exists());
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
    fn test_migrate_single_quoted_strips_quotes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY='sk-live'\n",
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert_eq!(
            agent_env::read(&paths)
                .unwrap()
                .into_iter()
                .find(|entry| entry.key == "LOCAL_API_KEY")
                .map(|entry| entry.value),
            Some("sk-live".to_string())
        );
    }

    #[test]
    fn test_migrate_double_quoted_processes_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=\"a\\\"b\"\n",
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert_eq!(
            agent_env::read(&paths)
                .unwrap()
                .into_iter()
                .find(|entry| entry.key == "LOCAL_API_KEY")
                .map(|entry| entry.value),
            Some("a\"b".to_string())
        );
    }

    #[test]
    fn test_migrate_unquoted_strips_inline_comment() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.env"),
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=secret # trailing\n",
        )
        .unwrap();

        migrate_legacy(&paths).unwrap();

        assert_eq!(
            agent_env::read(&paths)
                .unwrap()
                .into_iter()
                .find(|entry| entry.key == "LOCAL_API_KEY")
                .map(|entry| entry.value),
            Some("secret".to_string())
        );
    }

    #[test]
    fn test_migrate_double_quoted_backslash_n_becomes_newline() {
        let parsed = parse_env(
            "PROVIDER_NAME=local\nPROVIDER_URL=http://127.0.0.1:8080\nMODEL=mini\nAPI_KEY=\"line\\nnext\"\n",
            "local".to_string(),
            std::path::Path::new("local.env"),
        )
        .unwrap();

        assert_eq!(parsed.inline_secret.as_deref(), Some("line\nnext"));
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
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(paths.providers_dir()).unwrap();
        fs::write(
            paths.providers_dir().join("local.json"),
            r#"{"schema_version":1,"name":"local","url":"http://127.0.0.1:8080","model":"mini","key_env":"LOCAL_API_KEY","api_key":"secret","type":"openai-compat","health_path":"/health"}"#,
        )
        .unwrap();

        let err = read_all(&paths).unwrap_err();

        assert!(matches!(err, NczError::Precondition(_)));
        assert!(paths.providers_dir().join("local.json").exists());
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
    fn rejects_provider_urls_with_userinfo() {
        let err = validate_provider_url("https://token@api.example.test").unwrap_err();
        assert!(matches!(err, NczError::Usage(_)));
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
