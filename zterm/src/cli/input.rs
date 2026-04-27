use anyhow::Result;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::collections::VecDeque;
use std::fs;

use crate::cli::storage;

/// Input history manager
pub struct InputHistory {
    entries: VecDeque<String>,
    current_index: Option<usize>,
    max_entries: usize,
}

impl InputHistory {
    /// Create a new input history
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries),
            current_index: None,
            max_entries,
        }
    }

    /// Load history from file
    pub fn load_from_file() -> Result<Self> {
        let mut history = Self::new(1000);

        if let Ok(path) = storage::history_file() {
            if path.exists() {
                if let Ok(content) = fs::read_to_string(&path) {
                    for line in content.lines() {
                        if !line.trim().is_empty() {
                            history.entries.push_back(line.trim().to_string());
                        }
                    }
                }
            }
        }

        Ok(history)
    }

    /// Save history to file
    pub fn save_to_file(&self) -> Result<()> {
        storage::ensure_config_dir()?;
        let path = storage::history_file()?;

        let content = self
            .entries
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        fs::write(&path, content)?;
        Ok(())
    }

    /// Add entry to history
    pub fn push(&mut self, entry: String) {
        if !entry.trim().is_empty() {
            self.entries.push_back(entry);
            if self.entries.len() > self.max_entries {
                self.entries.pop_front();
            }
        }
        self.current_index = None;
    }

    /// Navigate backward (↑)
    pub fn navigate_up(&mut self) -> Option<String> {
        let idx = match self.current_index {
            None => self.entries.len().saturating_sub(1),
            Some(i) => i.saturating_sub(1),
        };

        if idx < self.entries.len() {
            self.current_index = Some(idx);
            self.entries.get(idx).cloned()
        } else {
            None
        }
    }

    /// Navigate forward (↓)
    pub fn navigate_down(&mut self) -> Option<String> {
        match self.current_index {
            None => None,
            Some(i) => {
                if i + 1 < self.entries.len() {
                    self.current_index = Some(i + 1);
                    self.entries.get(i + 1).cloned()
                } else {
                    self.current_index = None;
                    Some(String::new())
                }
            }
        }
    }

    /// Get all entries
    pub fn entries(&self) -> &VecDeque<String> {
        &self.entries
    }

    /// Fuzzy search history
    pub fn search(&self, query: &str) -> Vec<(usize, String)> {
        let matcher = SkimMatcherV2::default();
        let mut matches: Vec<(usize, String, i64)> = Vec::new();

        for (idx, entry) in self.entries.iter().enumerate() {
            if let Some(score) = matcher.fuzzy_match(entry, query) {
                matches.push((idx, entry.clone(), score));
            }
        }

        // Sort by score descending
        matches.sort_by_key(|m| std::cmp::Reverse(m.2));

        matches
            .into_iter()
            .map(|(idx, entry, _)| (idx, entry))
            .collect()
    }
}

/// Tab completion provider
pub struct CompletionProvider {
    commands: Vec<&'static str>,
    models: Vec<String>,
    sessions: Vec<String>,
}

impl CompletionProvider {
    /// Create a new completion provider
    pub fn new(models: Vec<String>, sessions: Vec<String>) -> Self {
        Self {
            commands: vec![
                "/model", "/session", "/memory", "/skill", "/config", "/clear", "/help", "/save",
                "/exit", "/info",
            ],
            models,
            sessions,
        }
    }

    /// Get completions for input
    pub fn complete(&self, input: &str) -> Vec<String> {
        let trimmed = input.trim();

        // Command completion
        if let Some(rest) = trimmed.strip_prefix('/') {
            let prefix = rest.to_lowercase();
            return self
                .commands
                .iter()
                .filter(|cmd| cmd[1..].starts_with(&prefix))
                .map(|cmd| cmd.to_string())
                .collect();
        }

        // Model completion (after /model)
        if let Some(rest) = trimmed.strip_prefix("/model ") {
            let prefix = rest.to_lowercase();
            return self
                .models
                .iter()
                .filter(|m| m.to_lowercase().starts_with(&prefix))
                .cloned()
                .collect();
        }

        // Session completion (after /session)
        if let Some(rest) = trimmed.strip_prefix("/session ") {
            let prefix = rest.to_lowercase();
            return self
                .sessions
                .iter()
                .filter(|s| s.to_lowercase().starts_with(&prefix))
                .cloned()
                .collect();
        }

        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_history_push() {
        let mut history = InputHistory::new(10);
        history.push("hello".to_string());
        history.push("world".to_string());
        assert_eq!(history.entries.len(), 2);
    }

    #[test]
    fn test_input_history_navigate() {
        let mut history = InputHistory::new(10);
        history.push("first".to_string());
        history.push("second".to_string());

        let up = history.navigate_up();
        assert_eq!(up, Some("second".to_string()));

        let up2 = history.navigate_up();
        assert_eq!(up2, Some("first".to_string()));
    }

    #[test]
    fn test_completion_provider_commands() {
        let provider = CompletionProvider::new(vec![], vec![]);
        let completions = provider.complete("/mod");
        assert!(completions.contains(&"/model".to_string()));
    }
}
