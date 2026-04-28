//! Small v0.3 flavor helpers that are testable without a terminal.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

pub const CONNECT_SPLASH_PROMPT: &str =
    "Generate a 4-6 line 1991 BBS modem connect sequence. Keep it ANSI-free.";
pub const CONNECT_SPLASH_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const WELCOME_QUOTES: &[&str] = &[
    "Turbo Pascal says: Hello, world!",
    "WordStar 7 loaded. Press ^KD to save.",
    "Paradox reports: table reindexed cleanly.",
    "dBASE V ready. SET TALK OFF.",
    "QEMM optimized upper memory. Conventional RAM smiles.",
    "Procomm Plus carrier detected. ANSI-BBS mode armed.",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZtermState {
    #[serde(default)]
    pub launches: u64,
    #[serde(default)]
    pub beep_on_error: bool,
}

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

pub fn default_state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".zterm").join("state.toml"))
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

pub fn load_state(path: &Path) -> ZtermState {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ZtermState::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

pub fn save_state(path: &Path, state: &ZtermState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = toml::to_string_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

pub fn record_launch() -> std::io::Result<(u64, Option<String>)> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm state",
        ));
    };
    record_launch_at(&path)
}

pub fn record_launch_at(path: &Path) -> std::io::Result<(u64, Option<String>)> {
    let mut state = load_state(path);
    state.launches = state.launches.saturating_add(1);
    let launches = state.launches;
    save_state(path, &state)?;
    Ok((launches, welcome_quote_for_launch(launches)))
}

pub fn set_beep_on_error(enabled: bool) -> std::io::Result<ZtermState> {
    let Some(path) = default_state_path() else {
        return Err(std::io::Error::other(
            "no home directory; cannot persist zterm state",
        ));
    };
    set_beep_on_error_at(&path, enabled)
}

pub fn set_beep_on_error_at(path: &Path, enabled: bool) -> std::io::Result<ZtermState> {
    let mut state = load_state(path);
    state.beep_on_error = enabled;
    save_state(path, &state)?;
    Ok(state)
}

pub fn is_welcome_milestone(launches: u64) -> bool {
    launches == 5 || launches == 10 || (launches >= 25 && launches.is_multiple_of(25))
}

pub fn welcome_quote_for_launch(launches: u64) -> Option<String> {
    if !is_welcome_milestone(launches) {
        return None;
    }
    let quote = WELCOME_QUOTES
        .choose(&mut rand::thread_rng())
        .copied()
        .unwrap_or(WELCOME_QUOTES[0]);
    Some(format!("Welcome back #{launches}: {quote}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

    #[test]
    fn launch_counter_persists_to_state_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");

        assert_eq!(record_launch_at(&path).unwrap().0, 1);
        assert_eq!(record_launch_at(&path).unwrap().0, 2);

        let state = load_state(&path);
        assert_eq!(state.launches, 2);
        assert!(!state.beep_on_error);
    }

    #[test]
    fn beep_toggle_persists_without_resetting_launches() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");

        record_launch_at(&path).unwrap();
        let state = set_beep_on_error_at(&path, true).unwrap();

        assert_eq!(state.launches, 1);
        assert!(state.beep_on_error);
        assert!(load_state(&path).beep_on_error);
    }

    #[test]
    fn welcome_quotes_only_land_on_milestones() {
        assert!(!is_welcome_milestone(4));
        assert!(is_welcome_milestone(5));
        assert!(is_welcome_milestone(10));
        assert!(is_welcome_milestone(25));
        assert!(welcome_quote_for_launch(5)
            .unwrap()
            .starts_with("Welcome back #5:"));
        assert!(welcome_quote_for_launch(6).is_none());
    }
}
