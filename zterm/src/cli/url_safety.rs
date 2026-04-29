pub(crate) fn is_sensitive_url_query_key(key: &str) -> bool {
    const SENSITIVE_FRAGMENTS: &[&str] = &[
        "token", "secret", "password", "auth", "key", "bearer", "jwt", "sig",
    ];
    let normalized = normalize_url_query_key(key);
    SENSITIVE_FRAGMENTS
        .iter()
        .any(|fragment| normalized.contains(fragment))
}

pub(crate) fn redact_url_secrets_for_display(value: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(value).ok()?;
    let mut changed = false;
    if !url.username().is_empty() {
        let _ = url.set_username("redacted");
        changed = true;
    }
    if url.password().is_some() {
        let _ = url.set_password(Some("redacted"));
        changed = true;
    }
    if url.query().is_some() {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| {
                if is_sensitive_url_query_key(&key) {
                    changed = true;
                    (key.into_owned(), "REDACTED".to_string())
                } else {
                    (key.into_owned(), value.into_owned())
                }
            })
            .collect();
        url.set_query(None);
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(
                pairs
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            );
        }
    }
    if url.fragment().is_some() {
        url.set_fragment(Some("REDACTED"));
        changed = true;
    }
    changed.then(|| url.to_string())
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

    #[test]
    fn url_secret_redaction_masks_fragments_and_compound_query_keys() {
        let redacted = redact_url_secrets_for_display(
            "wss://operator:embedded-password@gateway.example/ws?api_token=abc&room=alpha#access_token=secret",
        )
        .unwrap();

        assert_eq!(
            redacted,
            "wss://redacted:redacted@gateway.example/ws?api_token=REDACTED&room=alpha#REDACTED"
        );
        for leaked in ["operator", "embedded-password", "abc", "secret"] {
            assert!(!redacted.contains(leaked), "{leaked} leaked in {redacted}");
        }
    }

    #[test]
    fn url_secret_redaction_returns_none_for_clean_urls() {
        assert_eq!(
            redact_url_secrets_for_display("wss://gateway.example/ws?room=alpha"),
            None
        );
    }
}
