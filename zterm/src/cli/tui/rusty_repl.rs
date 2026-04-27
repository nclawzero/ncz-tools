use anyhow::Result;
use rustyline::config::Builder;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::Editor;

use crate::cli::input::CompletionProvider;
use crate::cli::storage;

/// Advanced REPL input handler with rustyline
pub struct RustyLineEditor {
    editor: Editor<(), DefaultHistory>,
    models: Vec<String>,
    sessions: Vec<String>,
    completer: CompletionProvider,
}

impl RustyLineEditor {
    /// Create new editor with history file
    pub fn new(models: Vec<String>, sessions: Vec<String>) -> Result<Self> {
        // Get history file path
        let history_path = storage::history_file()?;

        // Create config with features
        let config = Builder::new()
            .auto_add_history(true)
            .bell_style(rustyline::config::BellStyle::Audible)
            .build();

        // Create editor
        let mut editor: Editor<(), DefaultHistory> = Editor::with_config(config)?;

        // Load history
        if history_path.exists() {
            let _ = editor.load_history(&history_path);
        }

        let completer = CompletionProvider::new(models.clone(), sessions.clone());

        Ok(Self {
            editor,
            models,
            sessions,
            completer,
        })
    }

    /// Read a line with tab completion and history
    pub fn readline(&mut self, prompt: &str) -> Result<Option<String>> {
        match self.editor.readline(prompt) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = self.editor.add_history_entry(line.as_str());
                }
                Ok(Some(line))
            }
            Err(ReadlineError::Interrupted) => Ok(None), // Ctrl+C
            Err(ReadlineError::Eof) => Ok(None),         // Ctrl+D
            Err(e) => Err(anyhow::anyhow!("Readline error: {}", e)),
        }
    }

    /// Get completions for input
    pub fn complete(&self, input: &str) -> Vec<String> {
        self.completer.complete(input)
    }

    /// Update available models for completion
    pub fn update_models(&mut self, models: Vec<String>) {
        self.models = models;
    }

    /// Update available sessions for completion
    pub fn update_sessions(&mut self, sessions: Vec<String>) {
        self.sessions = sessions;
    }

    /// Save history to file
    pub fn save_history(&mut self) -> Result<()> {
        let history_path = storage::history_file()?;
        self.editor.save_history(&history_path)?;
        Ok(())
    }
}

/// Format prompt with theme colors
pub fn format_prompt(label: &str, emoji: &str, color: &str) -> String {
    format!(
        "{}{}{}{}:{} ",
        color,
        emoji,
        label,
        crate::cli::theme::Theme::RESET,
        crate::cli::theme::Theme::CYAN
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_prompt() {
        let prompt = format_prompt("You", "📝", "\x1b[94m");
        assert!(prompt.contains("You"));
        assert!(prompt.contains("📝"));
    }
}
