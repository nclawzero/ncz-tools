//! Parse Podman quadlet `.container` files for the bits ncz cares about.

use std::fs;
use std::path::Path;

use crate::error::NczError;

pub fn image_for(quadlet_path: &Path) -> Result<Option<String>, NczError> {
    if !quadlet_path.exists() {
        return Ok(None);
    }
    let body = fs::read_to_string(quadlet_path)?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Image") {
            if let Some(value) = rest.trim_start().strip_prefix('=') {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    return Ok(Some(v));
                }
            }
        }
    }
    Ok(None)
}

pub fn environment_files_for(quadlet_path: &Path) -> Result<Vec<String>, NczError> {
    if !quadlet_path.exists() {
        return Ok(Vec::new());
    }
    let body = fs::read_to_string(quadlet_path)?;
    let mut files = Vec::new();
    for line in body.lines() {
        let Some(value) = assignment_value(line, "EnvironmentFile") else {
            continue;
        };
        files.extend(value.split_whitespace().filter_map(normalize_environment_file));
    }
    files.sort();
    files.dedup();
    Ok(files)
}

pub fn loads_environment_file(quadlet_path: &Path, env_file: &Path) -> Result<bool, NczError> {
    let expected = env_file.display().to_string();
    Ok(environment_files_for(quadlet_path)?
        .iter()
        .any(|file| file == &expected))
}

fn assignment_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
        return None;
    }
    let rest = line.strip_prefix(key)?.trim_start();
    rest.strip_prefix('=').map(str::trim)
}

fn normalize_environment_file(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let raw = raw.strip_prefix('-').unwrap_or(raw).trim();
    let raw = raw
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            raw.strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(raw)
        .trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn image_for_reads_image_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("zeroclaw.container");
        fs::write(&path, "[Container]\nImage=localhost/zeroclaw:latest\n").unwrap();

        let image = image_for(&path).unwrap();

        assert_eq!(image.as_deref(), Some("localhost/zeroclaw:latest"));
    }

    #[test]
    fn environment_files_for_reads_optional_and_quoted_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("zeroclaw.container");
        fs::write(
            &path,
            "[Container]\nEnvironmentFile=-/etc/nclawzero/agent-env\nEnvironmentFile=\"/etc/nclawzero/zeroclaw/.env\"\n",
        )
        .unwrap();

        let files = environment_files_for(&path).unwrap();

        assert_eq!(
            files,
            vec![
                "/etc/nclawzero/agent-env".to_string(),
                "/etc/nclawzero/zeroclaw/.env".to_string()
            ]
        );
    }

    #[test]
    fn loads_environment_file_matches_optional_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("zeroclaw.container");
        fs::write(&path, "EnvironmentFile=-/etc/nclawzero/agent-env\n").unwrap();

        assert!(loads_environment_file(&path, Path::new("/etc/nclawzero/agent-env")).unwrap());
    }
}
