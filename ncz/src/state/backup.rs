//! Backup archive helpers: source discovery, agent-env redaction, manifest
//! hashing, and a small regular-file tar.gz reader/writer.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::error::NczError;
use crate::state::Paths;

pub const MANIFEST_NAME: &str = "manifest.json";
pub const AGENT_ENV_PATH: &str = "/etc/nclawzero/agent-env";
pub const OPENCLAW_HOME_CONFIG_PATH: &str = "/var/lib/nclawzero/openclaw-home/openclaw.json";
pub const VOLUME_PREFIX: &str = "podman://volume/";
pub const VOLUME_NAMES: &[&str] = &["zeroclaw-data", "openclaw-data", "hermes-data"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BackupManifest {
    pub schema_version: u32,
    pub hostname: String,
    pub created_at: String,
    pub ncz_version: String,
    #[serde(default)]
    pub unsafe_live_volumes: bool,
    pub sources: Vec<BackupSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupSource {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub redacted: bool,
}

#[derive(Debug, Clone)]
pub struct ArchiveSource {
    pub source: BackupSource,
    pub archive_path: String,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub path: String,
    pub contents: Vec<u8>,
}

pub fn manifest(hostname: String, sources: &[ArchiveSource]) -> BackupManifest {
    manifest_with_options(hostname, sources, false)
}

pub fn manifest_with_options(
    hostname: String,
    sources: &[ArchiveSource],
    unsafe_live_volumes: bool,
) -> BackupManifest {
    BackupManifest {
        schema_version: 1,
        hostname,
        created_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string()),
        ncz_version: env!("CARGO_PKG_VERSION").to_string(),
        unsafe_live_volumes,
        sources: sources.iter().map(|source| source.source.clone()).collect(),
    }
}

pub fn discover_file_sources(
    paths: &Paths,
    include_secrets: bool,
) -> Result<Vec<ArchiveSource>, NczError> {
    let mut sources = Vec::new();
    collect_file(
        &mut sources,
        AGENT_ENV_PATH,
        &paths.agent_env(),
        !include_secrets,
    )?;
    collect_json_dir(
        &mut sources,
        "/etc/nclawzero/providers.d",
        &paths.providers_dir(),
        !include_secrets,
    )?;
    collect_json_dir(
        &mut sources,
        "/etc/nclawzero/mcp.d",
        &paths.mcp_dir(),
        !include_secrets,
    )?;
    collect_file(
        &mut sources,
        "/etc/nclawzero/agent",
        &paths.agent_state(),
        false,
    )?;
    collect_file(
        &mut sources,
        "/etc/nclawzero/channel",
        &paths.channel(),
        false,
    )?;
    collect_file(
        &mut sources,
        "/etc/nclawzero/primary-provider",
        &paths.primary_provider(),
        false,
    )?;
    collect_agent_primary_providers(&mut sources, paths)?;
    collect_file(
        &mut sources,
        OPENCLAW_HOME_CONFIG_PATH,
        &real_path(paths, OPENCLAW_HOME_CONFIG_PATH),
        false,
    )?;
    sources.sort_by(|a, b| a.source.path.cmp(&b.source.path));
    Ok(sources)
}

pub fn volume_source(name: &str, contents: Vec<u8>) -> ArchiveSource {
    source(format!("{VOLUME_PREFIX}{name}"), false, contents)
}

fn collect_json_dir(
    sources: &mut Vec<ArchiveSource>,
    manifest_dir: &str,
    real_dir: &Path,
    redact_inline_secrets: bool,
) -> Result<(), NczError> {
    let mut files = Vec::new();
    match fs::read_dir(real_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry.path().extension().is_some_and(|ext| ext == "json")
                {
                    files.push(entry.path());
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(NczError::Io(e)),
    }
    files.sort();
    for file in files {
        let Some(name) = file.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        collect_json_file(
            sources,
            &format!("{manifest_dir}/{name}"),
            &file,
            redact_inline_secrets,
        )?;
    }
    Ok(())
}

fn collect_json_file(
    sources: &mut Vec<ArchiveSource>,
    manifest_path: &str,
    real_path: &Path,
    redact_inline_secrets: bool,
) -> Result<(), NczError> {
    let contents = match fs::read(real_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(NczError::Io(e)),
    };
    if redact_inline_secrets {
        if let Ok(mut value) = serde_json::from_slice::<Value>(&contents) {
            if redact_inline_json_secrets(&mut value) {
                let contents = serde_json::to_vec_pretty(&value)?;
                sources.push(source(manifest_path.to_string(), true, contents));
                return Ok(());
            }
        }
    }
    sources.push(source(manifest_path.to_string(), false, contents));
    Ok(())
}

fn collect_agent_primary_providers(
    sources: &mut Vec<ArchiveSource>,
    paths: &Paths,
) -> Result<(), NczError> {
    let mut agents = Vec::new();
    match fs::read_dir(paths.agent_config_dir()) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    agents.push(entry.file_name());
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(NczError::Io(e)),
    }
    agents.sort();
    for agent in agents {
        let Some(agent) = agent.to_str() else {
            continue;
        };
        collect_file(
            sources,
            &format!("/etc/nclawzero/agents/{agent}/primary-provider"),
            &paths.agent_primary_provider(agent),
            false,
        )?;
    }
    Ok(())
}

fn collect_file(
    sources: &mut Vec<ArchiveSource>,
    manifest_path: &str,
    real_path: &Path,
    redact: bool,
) -> Result<(), NczError> {
    let contents = match fs::read(real_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(NczError::Io(e)),
    };
    let contents = if redact {
        redact_agent_env(&String::from_utf8_lossy(&contents)).into_bytes()
    } else {
        contents
    };
    sources.push(source(manifest_path.to_string(), redact, contents));
    Ok(())
}

fn source(path: String, redacted: bool, contents: Vec<u8>) -> ArchiveSource {
    let sha256 = sha256_hex(&contents);
    let size = contents.len() as u64;
    let archive_path = archive_path_for_source(&path);
    ArchiveSource {
        source: BackupSource {
            path,
            sha256,
            size,
            redacted,
        },
        archive_path,
        contents,
    }
}

fn redact_inline_json_secrets(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut redacted = false;
            for (key, child) in map {
                if is_inline_secret_key(key) && json_value_is_non_empty(child) {
                    *child = Value::String(format!("REDACTED:{key}"));
                    redacted = true;
                } else if redact_inline_json_secrets(child) {
                    redacted = true;
                }
            }
            redacted
        }
        Value::Array(items) => {
            let mut redacted = false;
            for item in items {
                if redact_inline_json_secrets(item) {
                    redacted = true;
                }
            }
            redacted
        }
        _ => false,
    }
}

fn is_inline_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["apikey", "api_key", "token", "secret", "password", "bearer"]
        .iter()
        .any(|needle| key.contains(needle))
}

fn json_value_is_non_empty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

pub fn redact_agent_env(contents: &str) -> String {
    let mut out = String::new();
    for segment in contents.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        out.push_str(&redact_agent_env_line(line));
        out.push_str(newline);
    }
    if !contents.ends_with('\n') && contents.is_empty() {
        return out;
    }
    out
}

fn redact_agent_env_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return line.to_string();
    }
    let Some((key, _)) = line.split_once('=') else {
        return line.to_string();
    };
    let key = key.trim();
    if key.is_empty() {
        return line.to_string();
    }
    format!("{key}=REDACTED:{key}")
}

pub fn redacted_agent_env_keys(contents: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(contents)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            value
                .trim()
                .strip_prefix("REDACTED:")
                .map(|_| key.trim().to_string())
        })
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn write_archive(
    path: &Path,
    manifest: &BackupManifest,
    sources: &[ArchiveSource],
) -> Result<(), NczError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let mut gz = GzEncoder::new(file, Compression::default());
    let manifest_bytes = serde_json::to_vec_pretty(manifest)?;
    write_tar_file(&mut gz, MANIFEST_NAME, &manifest_bytes, 0o600)?;
    for source in sources {
        write_tar_file(&mut gz, &source.archive_path, &source.contents, 0o600)?;
    }
    gz.write_all(&[0_u8; 1024])?;
    gz.finish()?;
    Ok(())
}

pub fn read_archive(path: &Path) -> Result<(BackupManifest, Vec<ArchiveEntry>), NczError> {
    let file = File::open(path)?;
    let mut gz = GzDecoder::new(file);
    let mut tar = Vec::new();
    gz.read_to_end(&mut tar)?;
    let entries = read_tar_entries(&tar)?;
    let manifest_entry = entries
        .iter()
        .find(|entry| entry.path == MANIFEST_NAME)
        .ok_or_else(|| {
            NczError::Inconsistent("backup archive missing manifest.json".to_string())
        })?;
    let manifest = serde_json::from_slice(&manifest_entry.contents)?;
    Ok((manifest, entries))
}

pub fn validate_archive_sources(
    manifest: &BackupManifest,
    entries: &[ArchiveEntry],
) -> Vec<SourceValidation> {
    manifest
        .sources
        .iter()
        .map(|source| {
            let archive_path = archive_path_for_source(&source.path);
            let entry = entries.iter().find(|entry| entry.path == archive_path);
            let (actual_sha256, actual_size, ok) = match entry {
                Some(entry) => {
                    let actual_sha256 = sha256_hex(&entry.contents);
                    let actual_size = entry.contents.len() as u64;
                    let ok = actual_sha256 == source.sha256 && actual_size == source.size;
                    (Some(actual_sha256), Some(actual_size), ok)
                }
                None => (None, None, false),
            };
            SourceValidation {
                path: source.path.clone(),
                expected_sha256: source.sha256.clone(),
                actual_sha256,
                expected_size: source.size,
                actual_size,
                ok,
                redacted: source.redacted,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceValidation {
    pub path: String,
    pub expected_sha256: String,
    pub actual_sha256: Option<String>,
    pub expected_size: u64,
    pub actual_size: Option<u64>,
    pub ok: bool,
    pub redacted: bool,
}

pub fn archive_entry<'a>(
    entries: &'a [ArchiveEntry],
    source_path: &str,
) -> Option<&'a ArchiveEntry> {
    let archive_path = archive_path_for_source(source_path);
    entries.iter().find(|entry| entry.path == archive_path)
}

pub fn archive_path_for_source(path: &str) -> String {
    if let Some(volume) = path.strip_prefix(VOLUME_PREFIX) {
        format!("sources/podman-volumes/{volume}.tar")
    } else {
        format!("sources/{}", path.trim_start_matches('/'))
    }
}

pub fn real_path(paths: &Paths, manifest_path: &str) -> PathBuf {
    if let Some(rest) = manifest_path.strip_prefix("/etc/nclawzero") {
        return paths.etc_dir.join(rest.trim_start_matches('/'));
    }
    if let Some(rest) = manifest_path.strip_prefix("/var/lib/nclawzero") {
        if paths.etc_dir == Path::new("/etc/nclawzero") {
            return PathBuf::from(manifest_path);
        }
        if let Some(root) = paths
            .etc_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        {
            return root
                .join("var/lib/nclawzero")
                .join(rest.trim_start_matches('/'));
        }
    }
    PathBuf::from(manifest_path)
}

pub fn source_is_volume(path: &str) -> Option<&str> {
    path.strip_prefix(VOLUME_PREFIX)
}

pub fn is_supported_source_path(path: &str) -> bool {
    matches!(
        path,
        AGENT_ENV_PATH
            | "/etc/nclawzero/agent"
            | "/etc/nclawzero/channel"
            | "/etc/nclawzero/primary-provider"
            | OPENCLAW_HOME_CONFIG_PATH
    ) || is_json_child(path, "/etc/nclawzero/providers.d/")
        || is_json_child(path, "/etc/nclawzero/mcp.d/")
        || is_agent_primary_provider(path)
        || source_is_volume(path).is_some_and(|volume| VOLUME_NAMES.contains(&volume))
}

fn is_json_child(path: &str, prefix: &str) -> bool {
    let Some(name) = path.strip_prefix(prefix) else {
        return false;
    };
    !name.is_empty() && !name.contains('/') && name.ends_with(".json")
}

fn is_agent_primary_provider(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/etc/nclawzero/agents/") else {
        return false;
    };
    let Some((agent, leaf)) = rest.split_once('/') else {
        return false;
    };
    !agent.is_empty() && !agent.contains("..") && leaf == "primary-provider"
}

pub fn volume_agent(volume: &str) -> Option<&'static str> {
    match volume {
        "zeroclaw-data" => Some("zeroclaw"),
        "openclaw-data" => Some("openclaw"),
        "hermes-data" => Some("hermes"),
        _ => None,
    }
}

fn write_tar_file<W: Write>(
    w: &mut W,
    path: &str,
    contents: &[u8],
    mode: u32,
) -> Result<(), NczError> {
    if path.len() > 100 {
        return Err(NczError::Precondition(format!(
            "backup archive path too long for ustar: {path}"
        )));
    }
    let mut header = [0_u8; 512];
    write_bytes(&mut header[0..100], path.as_bytes());
    write_octal(&mut header[100..108], mode as u64);
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], contents.len() as u64);
    write_octal(&mut header[136..148], 0);
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    header[156] = b'0';
    write_bytes(&mut header[257..263], b"ustar\0");
    write_bytes(&mut header[263..265], b"00");
    let checksum: u32 = header.iter().map(|byte| *byte as u32).sum();
    write_checksum(&mut header[148..156], checksum);
    w.write_all(&header)?;
    w.write_all(contents)?;
    let padding = (512 - (contents.len() % 512)) % 512;
    if padding > 0 {
        w.write_all(&vec![0_u8; padding])?;
    }
    Ok(())
}

fn write_bytes(dst: &mut [u8], src: &[u8]) {
    let len = dst.len().min(src.len());
    dst[..len].copy_from_slice(&src[..len]);
}

fn write_octal(dst: &mut [u8], value: u64) {
    let text = format!("{value:0width$o}\0", width = dst.len() - 1);
    write_bytes(dst, text.as_bytes());
}

fn write_checksum(dst: &mut [u8], value: u32) {
    let text = format!("{value:06o}\0 ",);
    write_bytes(dst, text.as_bytes());
}

fn read_tar_entries(bytes: &[u8]) -> Result<Vec<ArchiveEntry>, NczError> {
    let mut entries = Vec::new();
    let mut offset = 0;
    while offset + 512 <= bytes.len() {
        let header = &bytes[offset..offset + 512];
        offset += 512;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let path = parse_string(&header[0..100]);
        let size = parse_octal(&header[124..136])? as usize;
        if path.is_empty() {
            return Err(NczError::Inconsistent(
                "backup archive has empty tar path".to_string(),
            ));
        }
        if offset + size > bytes.len() {
            return Err(NczError::Inconsistent(format!(
                "backup archive entry {path} is truncated"
            )));
        }
        entries.push(ArchiveEntry {
            path,
            contents: bytes[offset..offset + size].to_vec(),
        });
        offset += size;
        offset += (512 - (size % 512)) % 512;
    }
    Ok(entries)
}

fn parse_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn parse_octal(bytes: &[u8]) -> Result<u64, NczError> {
    let text = bytes
        .iter()
        .take_while(|byte| **byte != 0 && **byte != b' ')
        .map(|byte| *byte as char)
        .collect::<String>();
    if text.trim().is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text.trim(), 8)
        .map_err(|e| NczError::Inconsistent(format!("invalid tar size: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_agent_env_values_by_key() {
        let redacted = redact_agent_env("OPENAI_API_KEY=sk-live\n# keep\nEMPTY=\n");
        assert_eq!(
            redacted,
            "OPENAI_API_KEY=REDACTED:OPENAI_API_KEY\n# keep\nEMPTY=REDACTED:EMPTY\n"
        );
    }

    #[test]
    fn manifest_sha256_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        let sources = vec![source(
            "/etc/nclawzero/agent".to_string(),
            false,
            b"openclaw\n".to_vec(),
        )];
        let manifest = manifest("host".to_string(), &sources);

        write_archive(&archive, &manifest, &sources).unwrap();
        let (read_manifest, entries) = read_archive(&archive).unwrap();
        let validation = validate_archive_sources(&read_manifest, &entries);

        assert_eq!(read_manifest.sources, manifest.sources);
        assert_eq!(
            read_manifest.unsafe_live_volumes,
            manifest.unsafe_live_volumes
        );
        assert_eq!(validation.len(), 1);
        assert!(validation[0].ok);
    }

    #[test]
    fn manifest_defaults_missing_unsafe_live_volumes_to_false() {
        let json = r#"{
            "schema_version": 1,
            "hostname": "host",
            "created_at": "1970-01-01T00:00:00Z",
            "ncz_version": "0.0.0",
            "sources": []
        }"#;

        let manifest: BackupManifest = serde_json::from_str(json).unwrap();

        assert!(!manifest.unsafe_live_volumes);
    }
}
