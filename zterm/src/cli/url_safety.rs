pub(crate) fn is_sensitive_url_query_key(key: &str) -> bool {
    const SENSITIVE_FRAGMENTS: &[&str] = &[
        "token", "secret", "password", "auth", "key", "bearer", "jwt", "sig",
    ];
    let normalized = normalize_url_query_key(key);
    SENSITIVE_FRAGMENTS
        .iter()
        .any(|fragment| normalized.contains(fragment))
}

fn normalize_url_query_key(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_url_query_key_matches_compound_variants() {
        for key in [
            "token",
            "api_token",
            "refresh-token",
            "client_secret",
            "session.token",
            "Authorization",
            "apiKey",
            "signature",
        ] {
            assert!(is_sensitive_url_query_key(key), "{key} should be sensitive");
        }
        assert!(!is_sensitive_url_query_key("room"));
        assert!(!is_sensitive_url_query_key("model"));
    }
}
