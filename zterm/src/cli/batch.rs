use anyhow::Result;
use std::fs;
use std::path::Path;

use crate::cli::theme::Theme;

/// Batch command execution from files
pub struct BatchExecutor;

impl BatchExecutor {
    /// Load commands from a script file
    pub fn load_script(path: &Path) -> Result<Vec<String>> {
        if !path.exists() {
            anyhow::bail!("Script file not found: {}", path.display());
        }

        let content = fs::read_to_string(path)?;
        let commands: Vec<String> = content
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.to_string())
            .collect();

        Ok(commands)
    }

    /// Validate script before execution
    pub fn validate(commands: &[String]) -> Result<()> {
        for (i, cmd) in commands.iter().enumerate() {
            if cmd.is_empty() {
                anyhow::bail!("Empty command at line {}", i + 1);
            }

            // Basic validation - commands should start with / or be messages
            if !cmd.starts_with('/') && !cmd.is_empty() {
                // Valid message input
            } else if !cmd.starts_with('/') {
                anyhow::bail!("Invalid command at line {}: {}", i + 1, cmd);
            }
        }

        Ok(())
    }

    /// Display script info
    pub fn display_info(path: &Path, commands: &[String]) {
        println!();
        println!(
            "{}📋 Batch Script{}:{}",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            Theme::RESET
        );
        println!("   File: {}", path.display());
        println!("   Commands: {}", commands.len());
        println!();

        println!("{}Script Contents:{}", Theme::BRIGHT_CYAN, Theme::RESET);
        for (i, cmd) in commands.iter().enumerate() {
            println!(
                "   {}{}. {}{}",
                Theme::BRIGHT_BLUE,
                i + 1,
                cmd,
                Theme::RESET
            );
        }
        println!();
    }

    /// Format script error
    pub fn format_error(line_number: usize, error: &str) -> String {
        format!(
            "{}Error at line {}: {}{}",
            Theme::BRIGHT_RED,
            line_number,
            error,
            Theme::RESET
        )
    }
}

/// Batch result for reporting
#[derive(Debug, Clone)]
pub struct BatchResult {
    pub total_commands: usize,
    pub successful: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

impl BatchResult {
    /// Create new result tracker
    pub fn new(total: usize) -> Self {
        Self {
            total_commands: total,
            successful: 0,
            failed: 0,
            errors: Vec::new(),
        }
    }

    /// Record success
    pub fn add_success(&mut self) {
        self.successful += 1;
    }

    /// Record failure
    pub fn add_failure(&mut self, error: String) {
        self.failed += 1;
        self.errors.push(error);
    }

    /// Display summary
    pub fn display(&self) {
        println!();
        println!(
            "{}Batch Execution Summary:{}",
            Theme::BRIGHT_CYAN,
            Theme::RESET
        );
        println!(
            "  {}Total{}:     {} commands",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            self.total_commands
        );
        println!(
            "  {}✅ Successful{}: {} commands",
            Theme::BRIGHT_GREEN,
            Theme::RESET,
            self.successful
        );
        println!(
            "  {}❌ Failed{}:    {} commands",
            Theme::BRIGHT_RED,
            Theme::RESET,
            self.failed
        );

        if !self.errors.is_empty() {
            println!();
            println!("{}Errors:{}", Theme::BRIGHT_RED, Theme::RESET);
            for error in &self.errors {
                println!("  • {}", error);
            }
        }
        println!();
    }

    /// Get success rate as percentage
    pub fn success_rate(&self) -> f64 {
        if self.total_commands == 0 {
            0.0
        } else {
            (self.successful as f64 / self.total_commands as f64) * 100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_script() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        writeln!(file, "/help")?;
        writeln!(file, "/info")?;
        writeln!(file, "# This is a comment")?;
        writeln!(file, "/exit")?;

        let commands = BatchExecutor::load_script(file.path())?;
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], "/help");
        assert_eq!(commands[1], "/info");
        assert_eq!(commands[2], "/exit");

        Ok(())
    }

    #[test]
    fn test_batch_result() {
        let mut result = BatchResult::new(5);
        result.add_success();
        result.add_success();
        result.add_failure("Error 1".to_string());
        result.add_success();

        assert_eq!(result.successful, 3);
        assert_eq!(result.failed, 1);
        assert_eq!(result.success_rate(), 60.0);
    }
}
