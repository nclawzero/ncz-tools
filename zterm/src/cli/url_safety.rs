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

pub(crate) fn redact_url_secrets_lossy_if_needed(value: &str) -> Option<String> {
    redact_url_secrets_for_display(value).or_else(|| redact_raw_url_like_secrets(value))
}

pub(crate) fn redact_url_secrets_lossy_for_display(value: &str) -> String {
    redact_url_secrets_lossy_if_needed(value).unwrap_or_else(|| value.to_string())
}

fn redact_raw_url_like_secrets(value: &str) -> Option<String> {
    let mut redacted = value.to_string();
    let mut changed = false;

    if let Some(scheme_idx) = redacted.find("://") {
        let authority_start = scheme_idx + "://".len();
        let authority_end = redacted[authority_start..]
            .find(['/', '?', '#'])
            .map(|offset| authority_start + offset)
            .unwrap_or(redacted.len());
        if let Some(at_offset) = redacted[authority_start..authority_end].rfind('@') {
            let at_idx = authority_start + at_offset;
            if at_idx > authority_start {
                redacted.replace_range(authority_start..at_idx, "redacted:redacted");
                changed = true;
            }
        }
    }

    if let Some(fragment_start) = redacted.find('#') {
        redacted.replace_range(fragment_start + 1.., "REDACTED");
        changed = true;
    }

    if let Some(query_start) = redacted.find('?') {
        let query_value_start = query_start + 1;
        let query_end = redacted[query_value_start..]
            .find('#')
            .map(|offset| query_value_start + offset)
            .unwrap_or(redacted.len());
        let query = &redacted[query_value_start..query_end];
        let redacted_query = redact_raw_query_pairs(query, &mut changed);
        redacted.replace_range(query_value_start..query_end, &redacted_query);
    }

    changed.then_some(redacted)
}

fn redact_raw_query_pairs(query: &str, changed: &mut bool) -> String {
    let mut out = String::with_capacity(query.len());
    for segment in query.split_inclusive('&') {
        let (pair, separator) = segment
            .strip_suffix('&')
            .map(|pair| (pair, "&"))
            .unwrap_or((segment, ""));
        let key = pair.split_once('=').map(|(key, _)| key).unwrap_or(pair);
        if is_sensitive_url_query_key(key) {
            *changed = true;
            if let Some((key, _)) = pair.split_once('=') {
                out.push_str(key);
                out.push_str("=REDACTED");
            } else {
                out.push_str("REDACTED");
            }
        } else {
            out.push_str(pair);
        }
        out.push_str(separator);
    }
    out
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

    #[test]
    fn lossy_url_redaction_masks_malformed_url_secrets() {
        let redacted = redact_url_secrets_lossy_for_display(
            "wss://operator:embedded-password@[gateway/ws?api_token=abc&room=alpha#access_token=secret",
        );

        assert_eq!(
            redacted,
            "wss://redacted:redacted@[gateway/ws?api_token=REDACTED&room=alpha#REDACTED"
        );
        for leaked in ["operator", "embedded-password", "abc", "secret"] {
            assert!(!redacted.contains(leaked), "{leaked} leaked in {redacted}");
        }
    }

    #[test]
    fn lossy_url_redaction_leaves_plain_strings_unchanged() {
        assert_eq!(redact_url_secrets_lossy_if_needed("hello world"), None);
    }
}
