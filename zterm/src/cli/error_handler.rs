use crate::cli::theme::Theme;

/// Error context with suggestions
pub struct ErrorContext {
    pub error: String,
    pub suggestion: Option<String>,
    pub command: Option<String>,
}

impl ErrorContext {
    /// Analyze error and provide helpful suggestions
    pub fn new(error: &str, command: Option<&str>) -> Self {
        let error = error.to_string();
        let cmd = command.map(|s| s.to_string());

        // Detect common errors and provide suggestions
        let suggestion = detect_suggestion(&error, command);

        Self {
            error,
            suggestion,
            command: cmd,
        }
    }

    /// Print formatted error with suggestion
    pub fn print(&self) {
        println!();
        println!(
            "{}❌ Error{}:{}",
            Theme::BRIGHT_RED,
            Theme::RESET,
            Theme::RESET
        );
        println!("   {}", self.error);

        if let Some(suggestion) = &self.suggestion {
            println!();
            println!(
                "{}💡 Suggestion{}:{}",
                Theme::BRIGHT_YELLOW,
                Theme::RESET,
                Theme::RESET
            );
            println!("   {}", suggestion);
        }

        println!();
    }

    /// Print simple error (no suggestion)
    pub fn print_simple(&self) {
        println!("{}❌ {}{}", Theme::BRIGHT_RED, self.error, Theme::RESET);
    }
}

/// Detect error type and provide helpful suggestion
fn detect_suggestion(error: &str, command: Option<&str>) -> Option<String> {
    let error_lower = error.to_lowercase();

    // Connection errors
    if error_lower.contains("connection refused") {
        return Some("Make sure the zeroclaw gateway is running on the configured URL".to_string());
    }

    if error_lower.contains("timeout") {
        return Some(
            "The gateway took too long to respond. Check your network connection or try again."
                .to_string(),
        );
    }

    // Specific 'not found' cases must match before the generic catch.
    if error_lower.contains("model not found") {
        return Some(
            "The specified model is not available. Use /models list to see available models."
                .to_string(),
        );
    }

    if error_lower.contains("session not found") {
        return Some("Session not found. Use /sessions to list available sessions.".to_string());
    }

    // Command errors (generic catch-all for 'not found' / 'unknown command')
    if error_lower.contains("unknown command") || error_lower.contains("not found") {
        if let Some(cmd) = command {
            return Some(format!(
                "'{cmd}' is not a valid command. Type /help to see available commands."
            ));
        } else {
            return Some("Type /help to see available commands.".to_string());
        }
    }

    if error_lower.contains("invalid argument") {
        if let Some(cmd) = command {
            return Some(format!(
                "Invalid arguments for '{cmd}'. Type /help for usage information."
            ));
        }
    }

    // Authentication errors
    if error_lower.contains("unauthorized") || error_lower.contains("401") {
        return Some(
            "Authentication failed. Check your API token in ~/.zeroclaw/config.toml".to_string(),
        );
    }

    if error_lower.contains("forbidden") || error_lower.contains("403") {
        return Some("You don't have permission for this action.".to_string());
    }

    // Model errors
    // Session errors
    if error_lower.contains("session not found") {
        return Some(
            "The session does not exist. Use /session list to see available sessions.".to_string(),
        );
    }

    // Memory/MNEMOS errors
    if error_lower.contains("memory") && error_lower.contains("offline") {
        return Some(
            "MNEMOS memory system is offline. Local memory will be unavailable.".to_string(),
        );
    }

    // Generic parsing errors
    if error_lower.contains("parse") || error_lower.contains("invalid json") {
        return Some(
            "Failed to parse response from gateway. This may be a temporary issue.".to_string(),
        );
    }

    // Default suggestion
    None
}

/// Format an error message with theme colors
pub fn format_error(message: &str) -> String {
    format!("{}❌ {}{}", Theme::BRIGHT_RED, message, Theme::RESET)
}

/// Format a warning message with theme colors
pub fn format_warning(message: &str) -> String {
    format!("{}⚠️  {}{}", Theme::BRIGHT_YELLOW, message, Theme::RESET)
}

/// Format a success message with theme colors
pub fn format_success(message: &str) -> String {
    format!("{}✅ {}{}", Theme::BRIGHT_GREEN, message, Theme::RESET)
}

/// Format an info message with theme colors
pub fn format_info(message: &str) -> String {
    format!("{}ℹ️  {}{}", Theme::BRIGHT_CYAN, message, Theme::RESET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_error() {
        let ctx = ErrorContext::new("connection refused", None);
        assert!(ctx.suggestion.is_some());
        assert!(ctx.suggestion.unwrap().contains("zeroclaw gateway"));
    }

    #[test]
    fn test_model_not_found() {
        let ctx = ErrorContext::new("model not found", None);
        assert!(ctx.suggestion.is_some());
        assert!(ctx.suggestion.unwrap().contains("/models"));
    }

    #[test]
    fn test_unknown_command() {
        let ctx = ErrorContext::new("unknown command", Some("foobar"));
        assert!(ctx.suggestion.is_some());
        assert!(ctx.suggestion.unwrap().contains("foobar"));
    }

    #[test]
    fn test_format_functions() {
        let err = format_error("test error");
        assert!(err.contains("❌"));
        assert!(err.contains("test error"));

        let success = format_success("operation complete");
        assert!(success.contains("✅"));
    }
}
