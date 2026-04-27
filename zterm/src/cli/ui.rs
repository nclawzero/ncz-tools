use crate::cli::theme::Theme;

/// Status bar display
pub struct StatusBar {
    pub model: String,
    pub provider: String,
    pub session_name: String,
}

impl StatusBar {
    /// Create a new status bar
    pub fn new(model: String, provider: String, session_name: String) -> Self {
        Self {
            model,
            provider,
            session_name,
        }
    }

    /// Render status bar with theme colors
    pub fn render(&self) -> String {
        let status = format!(
            "{}Model: {} {}Provider: {} {}Session: {}{}",
            Theme::BRIGHT_CYAN,
            self.model,
            Theme::BRIGHT_BLUE,
            self.provider,
            Theme::BRIGHT_CYAN,
            self.session_name,
            Theme::RESET
        );
        let line = format!("{}{}{}", Theme::BLUE, "─".repeat(70), Theme::RESET);
        format!("{}\n{}", status, line)
    }

    /// Update model
    pub fn set_model(&mut self, model: String) {
        self.model = model;
    }

    /// Update session
    pub fn set_session(&mut self, session_name: String) {
        self.session_name = session_name;
    }
}

/// Simple paginator
pub struct Paginator {
    items: Vec<String>,
    page_size: usize,
    current_page: usize,
}

impl Paginator {
    /// Create a new paginator
    pub fn new(items: Vec<String>, page_size: usize) -> Self {
        Self {
            items,
            page_size,
            current_page: 0,
        }
    }

    /// Get current page items
    pub fn current_page_items(&self) -> Vec<&String> {
        let start = self.current_page * self.page_size;
        let end = (start + self.page_size).min(self.items.len());

        self.items[start..end].iter().collect()
    }

    /// Render current page
    pub fn render(&self) -> String {
        let items = self.current_page_items();
        let total_pages = self.items.len().div_ceil(self.page_size);

        let mut output = String::new();
        for (i, item) in items.iter().enumerate() {
            output.push_str(&format!("{}. {}\n", i + 1, item));
        }

        if total_pages > 1 {
            let page_indicator = if self.current_page > 0 { "▲" } else { " " };
            let next_indicator = if self.current_page < total_pages - 1 {
                "▼"
            } else {
                " "
            };
            output.push_str(&format!(
                "{}  Page {}/{}  {}\n",
                page_indicator,
                self.current_page + 1,
                total_pages,
                next_indicator
            ));
        }

        output
    }

    /// Next page
    pub fn next_page(&mut self) -> bool {
        let total_pages = self.items.len().div_ceil(self.page_size);
        if self.current_page < total_pages - 1 {
            self.current_page += 1;
            true
        } else {
            false
        }
    }

    /// Previous page
    pub fn prev_page(&mut self) -> bool {
        if self.current_page > 0 {
            self.current_page -= 1;
            true
        } else {
            false
        }
    }
}

/// Code block formatter with theme colors
pub struct CodeBlockFormatter;

impl CodeBlockFormatter {
    /// Format text with code block detection and theme colors
    pub fn format(text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let mut output = String::new();
        let mut in_code_block = false;
        let mut language = String::new();

        for line in lines {
            if let Some(rest) = line.strip_prefix("```") {
                if in_code_block {
                    output.push_str(&format!(
                        "{}└─────────────────────────{}\n",
                        Theme::CYAN,
                        Theme::RESET
                    ));
                    in_code_block = false;
                    language.clear();
                } else {
                    language = rest.trim().to_string();
                    if language.is_empty() {
                        language = "code".to_string();
                    }
                    output.push_str(&format!(
                        "{}┌─ {}{}{}{}{}{}",
                        Theme::CYAN,
                        Theme::BRIGHT_BLUE,
                        language,
                        Theme::RESET,
                        Theme::CYAN,
                        "\n",
                        Theme::RESET
                    ));
                    in_code_block = true;
                }
            } else if in_code_block {
                output.push_str(&format!("{}│{} {}\n", Theme::CYAN, Theme::RESET, line));
            } else {
                output.push_str(&format!("{}\n", line));
            }
        }

        // Close any unclosed code block
        if in_code_block {
            output.push_str(&format!(
                "{}└─────────────────────────{}\n",
                Theme::CYAN,
                Theme::RESET
            ));
        }

        output
    }
}

/// Help panel with theme colors
pub fn print_help() {
    println!();
    println!(
        "{}╔════════════════════════════════════════════════════════════╗{}",
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}║{}                    Available Commands{}                    {}║{}",
        Theme::CYAN,
        Theme::BRIGHT_CYAN,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}╚════════════════════════════════════════════════════════════╝{}",
        Theme::CYAN,
        Theme::RESET
    );
    println!();
    println!(
        "  {}📋 /model{}       - Switch model (shows available models)",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}📂 /session{}     - List/create/switch sessions",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}🧠 /memory{}      - Search memory entries",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}🎯 /skill{}       - Enable/disable skills",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}⚙️  /config{}      - Re-run setup wizard",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}🗑️  /clear{}       - Clear session history",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}💾 /save{}  [file] - Save session transcript",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}ℹ️  /info{}        - Show current session info",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}❓ /help{}        - Show this help",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!(
        "  {}🚪 /exit{}        - Exit ZTerm",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    println!();
}

/// Error panel with theme colors
pub fn print_error(message: &str, suggestion: Option<&str>) {
    println!();
    println!("{}❌ {}{}", Theme::BRIGHT_RED, message, Theme::RESET);
    if let Some(hint) = suggestion {
        println!("{}💡 {}{}", Theme::BRIGHT_YELLOW, hint, Theme::RESET);
    }
    println!();
}

/// Success message with theme colors
pub fn print_success(message: &str) {
    println!("{}✅ {}{}", Theme::BRIGHT_GREEN, message, Theme::RESET);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_bar_render() {
        // Use neutral config-key strings here, NOT vendor model
        // names. zterm's `/models` selector dispatches by zeroclaw
        // provider-key (`primary`, `consult`, `together`); the
        // status bar mirrors the same key. Fixtures stay free of
        // vendor-specific model names.
        let bar = StatusBar::new(
            "primary".to_string(),
            "zeroclaw".to_string(),
            "main".to_string(),
        );
        let output = bar.render();
        assert!(output.contains("primary"));
        assert!(output.contains("zeroclaw"));
        assert!(output.contains("main"));
    }

    #[test]
    fn test_paginator() {
        let items: Vec<String> = (1..=25).map(|i| format!("Item {}", i)).collect();
        let mut paginator = Paginator::new(items, 10);

        assert_eq!(paginator.current_page_items().len(), 10);
        paginator.next_page();
        assert_eq!(paginator.current_page_items().len(), 10);
        paginator.next_page();
        assert_eq!(paginator.current_page_items().len(), 5);
    }

    #[test]
    fn test_code_block_formatter() {
        let text = "Here's some code:\n```rust\nfn main() {}\n```\nDone!";
        let formatted = CodeBlockFormatter::format(text);
        assert!(formatted.contains("┌─"));
        assert!(formatted.contains("rust"));
        assert!(formatted.contains("└─────"));
    }
}
