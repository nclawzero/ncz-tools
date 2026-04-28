//! Small v0.3 flavor helpers that are testable without a terminal.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const CONNECT_SPLASH_PROMPT: &str =
    "Generate a 4-6 line 1991 BBS modem connect sequence. Keep it ANSI-free.";
pub const CONNECT_SPLASH_TTL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn sanitize_workspace_name(workspace: &str) -> String {
    let sanitized: String = workspace
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "workspace".to_string()
    } else {
        sanitized
    }
}

pub fn connect_splash_cache_path(base: &Path, workspace: &str) -> PathBuf {
    base.join("cache")
        .join("connect-splash")
        .join(format!("{}.txt", sanitize_workspace_name(workspace)))
}

pub fn default_connect_splash_cache_path(workspace: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|home| connect_splash_cache_path(&home.join(".zterm"), workspace))
}

pub fn is_cache_fresh(modified: SystemTime, now: SystemTime, ttl: Duration) -> bool {
    now.duration_since(modified)
        .map(|age| age <= ttl)
        .unwrap_or(true)
}

pub fn read_cached_connect_splash(path: &Path, now: SystemTime, ttl: Duration) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    if !is_cache_fresh(modified, now, ttl) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let normalized = normalize_connect_splash(&text);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub fn write_connect_splash_cache(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, normalize_connect_splash(text))
}

pub fn normalize_connect_splash(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_cache_filename_is_sanitized() {
        assert_eq!(sanitize_workspace_name("../prod typhon"), ".._prod_typhon");
        assert_eq!(sanitize_workspace_name(""), "workspace");
    }

    #[test]
    fn cache_freshness_uses_mtime_and_ttl() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let fresh = now - Duration::from_secs(60);
        let stale = now - CONNECT_SPLASH_TTL - Duration::from_secs(1);

        assert!(is_cache_fresh(fresh, now, CONNECT_SPLASH_TTL));
        assert!(!is_cache_fresh(stale, now, CONNECT_SPLASH_TTL));
    }

    #[test]
    fn normalize_connect_splash_caps_to_six_non_empty_lines() {
        let input = "CONNECT 2400\n\nline2  \nline3\nline4\nline5\nline6\nline7\n";
        assert_eq!(
            normalize_connect_splash(input),
            "CONNECT 2400\nline2\nline3\nline4\nline5\nline6"
        );
    }
}
