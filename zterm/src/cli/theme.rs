/// ZTerm Color Theme (Zeroclaw Brand: Blues)
/// ANSI color codes aligned with zeroclaw UI theme
pub struct Theme;

impl Theme {
    /// Primary brand color: Cyan (zeroclaw primary blue)
    pub const CYAN: &'static str = "\x1b[36m";

    /// Bright cyan for emphasis
    pub const BRIGHT_CYAN: &'static str = "\x1b[96m";

    /// Blue for secondary elements
    pub const BLUE: &'static str = "\x1b[34m";

    /// Bright blue for highlights
    pub const BRIGHT_BLUE: &'static str = "\x1b[94m";

    /// Green for success/positive
    pub const GREEN: &'static str = "\x1b[32m";

    /// Bright green for emphasis
    pub const BRIGHT_GREEN: &'static str = "\x1b[92m";

    /// Yellow for warnings
    pub const YELLOW: &'static str = "\x1b[33m";

    /// Bright yellow for emphasis
    pub const BRIGHT_YELLOW: &'static str = "\x1b[93m";

    /// Red for errors
    pub const RED: &'static str = "\x1b[31m";

    /// Bright red for emphasis
    pub const BRIGHT_RED: &'static str = "\x1b[91m";

    /// Default/Reset color
    pub const RESET: &'static str = "\x1b[0m";

    /// Bold text modifier
    pub const BOLD: &'static str = "\x1b[1m";

    /// Dim text modifier
    pub const DIM: &'static str = "\x1b[2m";
}

/// Format text with color
pub fn colored(text: &str, color: &str) -> String {
    format!("{}{}{}", color, text, Theme::RESET)
}

/// Format text with bold
pub fn bold(text: &str) -> String {
    format!("{}{}{}", Theme::BOLD, text, Theme::RESET)
}

/// Format text with color and bold
pub fn bold_colored(text: &str, color: &str) -> String {
    format!("{}{}{}{}", Theme::BOLD, color, text, Theme::RESET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_colored() {
        let result = colored("hello", Theme::CYAN);
        assert!(result.contains("hello"));
        assert!(result.contains(Theme::RESET));
    }

    #[test]
    fn test_bold() {
        let result = bold("world");
        assert!(result.contains("world"));
        assert!(result.contains(Theme::BOLD));
    }
}
