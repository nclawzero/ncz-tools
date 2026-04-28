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
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed
            .split_once(']')
            .map(|(host, _)| host)
            .filter(|host| !host.is_empty());
    }
    authority.split(':').next().filter(|host| !host.is_empty())
}

pub fn has_userinfo(url: &str) -> bool {
    authority(url).is_some_and(|authority| authority.contains('@'))
}

pub fn has_query_or_fragment(url: &str) -> bool {
    url.split_once("://")
        .map(|(_, rest)| rest.contains('?') || rest.contains('#'))
        .unwrap_or(false)
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
}
