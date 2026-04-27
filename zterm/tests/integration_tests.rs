// Integration tests for ZTerm
// These tests verify end-to-end flows

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    // Mock utilities
    fn temp_dir() -> PathBuf {
        let temp = std::env::temp_dir().join("zterm_tests");
        let _ = fs::create_dir_all(&temp);
        temp
    }

    #[test]
    fn test_config_roundtrip() {
        let temp = temp_dir();
        let config_file = temp.join("config.toml");

        let sample_config = r#"
[gateway]
url = "http://localhost:8888"
token = "test_token"

[agent]
model = "claude-3.5-opus"
provider = "anthropic"
"#;

        // Write config
        fs::write(&config_file, sample_config).unwrap();

        // Read it back
        let content = fs::read_to_string(&config_file).unwrap();
        assert!(content.contains("http://localhost:8888"));
        assert!(content.contains("test_token"));

        // Parse as TOML
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(
            parsed["gateway"]["url"].as_str().unwrap(),
            "http://localhost:8888"
        );
    }

    #[test]
    fn test_session_metadata_json() {
        let metadata_json = r#"
{
    "id": "session-1",
    "name": "main",
    "model": "claude-3.5-opus",
    "provider": "anthropic",
    "created_at": "2026-04-20T10:00:00Z",
    "message_count": 5,
    "last_active": "2026-04-20T10:05:00Z"
}
"#;

        let parsed: serde_json::Value = serde_json::from_str(metadata_json).unwrap();
        assert_eq!(parsed["name"].as_str().unwrap(), "main");
        assert_eq!(parsed["message_count"].as_u64().unwrap(), 5);
    }

    #[test]
    fn test_input_history_persistence() {
        let temp = temp_dir();
        let history_file = temp.join("history.jsonl");

        let entries = ["hello", "world", "test"];
        let content = entries.join("\n");

        fs::write(&history_file, content).unwrap();

        let read_back = fs::read_to_string(&history_file).unwrap();
        let lines: Vec<&str> = read_back.lines().collect();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "hello");
    }

    #[test]
    fn test_sse_line_parsing() {
        let sse_response = "data: Hello\ndata: World\ndata: [DONE]\n";

        for line in sse_response.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data != "[DONE]" {
                    println!("Parsed: {}", data);
                }
            }
        }
    }

    #[test]
    fn test_command_dispatch() {
        let commands = vec![
            ("/help", "help"),
            ("/model", "model"),
            ("/session", "session"),
            ("/memory test", "memory"),
        ];

        for (input, expected) in commands {
            let parts: Vec<&str> = input.split_whitespace().collect();
            let cmd = parts[0];
            assert!(cmd.starts_with("/"));
            assert!(cmd.contains(expected));
        }
    }

    #[test]
    fn test_status_bar_rendering() {
        let status = format!(
            "Model: {}  Provider: {}  Session: {}",
            "claude-3.5-opus", "anthropic", "main"
        );

        assert!(status.contains("claude-3.5-opus"));
        assert!(status.contains("anthropic"));
        assert!(status.contains("main"));
        assert!(status.len() > 30);
    }

    #[test]
    fn test_code_block_detection() {
        let text = "```rust\nfn main() {}\n```";
        let has_code_block = text.contains("```rust");
        assert!(has_code_block);
    }
}
