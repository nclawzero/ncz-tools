//! Small URL helpers for declaration validation.

use std::net::IpAddr;

pub fn authority(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    rest.split(&['/', '?', '#'][..])
        .next()
        .filter(|authority| !authority.is_empty())
}

pub fn host(url: &str) -> Option<&str> {
    let authority = authority(url)?;
    parse_authority(authority).map(|parts| parts.host)
}

pub fn has_valid_authority(url: &str) -> bool {
    authority(url).and_then(parse_authority).is_some()
}

pub fn has_userinfo(url: &str) -> bool {
    authority(url).is_some_and(|authority| authority.contains('@'))
}

pub fn has_query_or_fragment(url: &str) -> bool {
    url.split_once("://")
        .map(|(_, rest)| rest.contains('?') || rest.contains('#'))
        .unwrap_or(false)
}

pub fn contains_secret_path_material(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let Some((_, path_and_tail)) = rest.split_once('/') else {
        return false;
    };
    let path = path_and_tail
        .split(&['?', '#'][..])
        .next()
        .unwrap_or(path_and_tail);
    path_contains_secret_material(path)
}

pub fn path_contains_secret_material(path: &str) -> bool {
    let decoded_path = percent_decoded_ascii_lower(path);
    decoded_path.split('/').any(|segment| {
        segment
            .split(';')
            .any(is_secret_path_component_normalized)
    })
}

pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AuthorityParts<'a> {
    host: &'a str,
}

fn parse_authority(authority: &str) -> Option<AuthorityParts<'_>> {
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, rest) = bracketed.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        if !rest.is_empty() {
            let port = rest.strip_prefix(':')?;
            validate_port(port)?;
        }
        return Some(AuthorityParts { host });
    }
    if authority.contains('[') || authority.contains(']') || authority.matches(':').count() > 1 {
        return None;
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if host.is_empty() {
        return None;
    }
    if let Some(port) = port {
        validate_port(port)?;
    }
    Some(AuthorityParts { host })
}

fn validate_port(port: &str) -> Option<()> {
    if port.is_empty() || port.parse::<u16>().is_err() {
        return None;
    }
    Some(())
}

fn is_secret_path_component_normalized(normalized: &str) -> bool {
    if normalized.is_empty() || looks_like_inline_secret_value(normalized) {
        return !normalized.is_empty();
    }
    let (key, value) = normalized
        .split_once('=')
        .map_or((normalized, ""), |(key, value)| (key, value));
    if !value.is_empty() && looks_like_inline_secret_value(value) {
        return true;
    }
    let key = key.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-');
    matches!(
        key,
        "auth"
            | "authorization"
            | "bearer"
            | "token"
            | "access-token"
            | "access_token"
            | "key"
            | "api-key"
            | "api_key"
            | "apikey"
            | "x-api-key"
            | "x_api_key"
            | "secret"
            | "password"
            | "credential"
            | "credentials"
            | "jwt"
            | "pat"
            | "session"
            | "session-token"
            | "session_token"
    ) || key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("credential")
        || key.contains("authorization")
        || key.contains("session")
        || key.contains("api_key")
        || key.contains("api-key")
        || key.ends_with("_key")
        || key.ends_with("-key")
        || key.ends_with("_pat")
        || key.ends_with("-pat")
}

fn looks_like_inline_secret_value(raw_token: &str) -> bool {
    let token = raw_token.trim_matches(|ch: char| {
        ch == '"' || ch == '\'' || ch == ',' || ch == '[' || ch == ']' || ch == '{' || ch == '}'
    });
    let lower = token.to_ascii_lowercase();
    lower.starts_with("sk-")
        || lower.starts_with("sk_")
        || lower.starts_with("ghp_")
        || lower.starts_with("github_pat_")
        || lower.starts_with("xoxb-")
        || lower.starts_with("xoxp-")
        || lower.starts_with("xoxa-")
        || (lower.starts_with("eyj") && token.matches('.').count() >= 2)
}

fn percent_decoded_ascii_lower(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(((high << 4 | low) as char).to_ascii_lowercase());
                index += 3;
                continue;
            }
        }
        decoded.push((bytes[index] as char).to_ascii_lowercase());
        index += 1;
    }
    decoded
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_requires_literal_loopback_host() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("127.attacker.example"));
        assert!(!is_loopback_host("example.test"));
    }

    #[test]
    fn host_extracts_ipv6_bracketed_hosts() {
        assert_eq!(host("http://[::1]:8080/v1"), Some("::1"));
    }

    #[test]
    fn valid_authority_rejects_malformed_hosts_and_ports() {
        for url in [
            "https://:443",
            "https://api.example.test:",
            "https://api.example.test:not-a-port",
            "https://api.example.test:65536",
            "https://[::1",
            "https://::1:443",
        ] {
            assert!(!has_valid_authority(url), "accepted malformed URL: {url}");
        }
    }

    #[test]
    fn valid_authority_accepts_names_ports_and_bracketed_ipv6() {
        for url in [
            "https://api.example.test",
            "https://api.example.test:443/v1",
            "http://127.0.0.1:8080",
            "http://[::1]:8080/v1",
        ] {
            assert!(has_valid_authority(url), "rejected valid URL: {url}");
        }
    }

    #[test]
    fn secret_path_material_detects_url_paths_and_parameters() {
        for url in [
            "https://api.example.test/token/sk-live",
            "https://api.example.test/sse;token=secret",
            "https://api.example.test/%74oken/secret",
            "https://api.example.test/sse/sk-live",
            "https://api.example.test/v1%2Fsk-live",
            "https://api.example.test/v1%3Btoken=secret",
        ] {
            assert!(contains_secret_path_material(url), "missed {url}");
        }
        assert!(!contains_secret_path_material(
            "https://api.example.test/v1/models"
        ));
        assert!(path_contains_secret_material("/api-key/secret"));
        assert!(!path_contains_secret_material("/health"));
    }
}
