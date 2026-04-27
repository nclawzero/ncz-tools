use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::storage;
use crate::cli::theme::Theme;

/// Extended session metadata with tags and search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadataEx {
    pub id: String,
    pub name: String,
    pub model: String,
    pub provider: String,
    pub created_at: String,
    pub last_active: String,
    pub message_count: usize,
    pub tags: Vec<String>,
    pub description: Option<String>,
}

impl SessionMetadataEx {
    /// Create from basic metadata
    pub fn from_basic(meta: &storage::SessionMetadata) -> Self {
        Self {
            id: meta.id.clone(),
            name: meta.name.clone(),
            model: meta.model.clone(),
            provider: meta.provider.clone(),
            created_at: meta.created_at.clone(),
            last_active: meta.last_active.clone(),
            message_count: meta.message_count,
            tags: Vec::new(),
            description: None,
        }
    }

    /// Add a tag
    pub fn add_tag(&mut self, tag: &str) {
        if !self.tags.contains(&tag.to_string()) {
            self.tags.push(tag.to_string());
        }
    }

    /// Remove a tag
    pub fn remove_tag(&mut self, tag: &str) {
        self.tags.retain(|t| t != tag);
    }

    /// Set description
    pub fn set_description(&mut self, description: &str) {
        self.description = Some(description.to_string());
    }

    /// Check if session matches search query
    pub fn matches(&self, query: &str) -> bool {
        let q = query.to_lowercase();

        // Search in name
        if self.name.to_lowercase().contains(&q) {
            return true;
        }

        // Search in description
        if let Some(desc) = &self.description {
            if desc.to_lowercase().contains(&q) {
                return true;
            }
        }

        // Search in tags
        for tag in &self.tags {
            if tag.to_lowercase().contains(&q) {
                return true;
            }
        }

        // Search in model name
        if self.model.to_lowercase().contains(&q) {
            return true;
        }

        // Search in provider name
        if self.provider.to_lowercase().contains(&q) {
            return true;
        }

        false
    }

    /// Format for display
    pub fn display(&self, show_metadata: bool) {
        println!(
            "{}●{} {} {} ({})",
            Theme::BRIGHT_BLUE,
            Theme::RESET,
            self.name,
            if show_metadata {
                format!("({})", self.model)
            } else {
                String::new()
            },
            self.provider
        );

        if let Some(desc) = &self.description {
            println!("  {}{}{}", Theme::BRIGHT_CYAN, desc, Theme::RESET);
        }

        if !self.tags.is_empty() {
            print!("  {}Tags:{} ", Theme::BRIGHT_BLUE, Theme::RESET);
            for (i, tag) in self.tags.iter().enumerate() {
                if i > 0 {
                    print!(", ");
                }
                print!("{}#{}{}", Theme::BRIGHT_CYAN, tag, Theme::RESET);
            }
            println!();
        }

        println!(
            "  {} messages | Last active: {}",
            self.message_count, self.last_active
        );
    }
}

/// Session search functionality
pub struct SessionSearch;

impl SessionSearch {
    /// Search sessions by query
    pub fn search(query: &str) -> Result<Vec<SessionMetadataEx>> {
        let sessions = storage::list_sessions()?;

        let mut results: Vec<SessionMetadataEx> = sessions
            .iter()
            .map(SessionMetadataEx::from_basic)
            .filter(|s| s.matches(query))
            .collect();

        // Sort by relevance (name match first, then last_active)
        results.sort_by(|a, b| {
            let a_match = a.name.to_lowercase() == query.to_lowercase();
            let b_match = b.name.to_lowercase() == query.to_lowercase();

            if a_match && !b_match {
                std::cmp::Ordering::Less
            } else if !a_match && b_match {
                std::cmp::Ordering::Greater
            } else {
                b.last_active.cmp(&a.last_active)
            }
        });

        Ok(results)
    }

    /// Get sessions with specific tag
    pub fn by_tag(tag: &str) -> Result<Vec<SessionMetadataEx>> {
        let sessions = storage::list_sessions()?;

        let results: Vec<SessionMetadataEx> = sessions
            .iter()
            .map(SessionMetadataEx::from_basic)
            .filter(|s| s.tags.iter().any(|t| t == tag))
            .collect();

        Ok(results)
    }

    /// Display search results
    pub fn display_results(results: &[SessionMetadataEx], query: Option<&str>) {
        if results.is_empty() {
            if let Some(q) = query {
                println!(
                    "{}No sessions found matching '{}'{}",
                    Theme::BRIGHT_YELLOW,
                    q,
                    Theme::RESET
                );
            }
            return;
        }

        println!();
        println!(
            "{}Found {} session(s):{}",
            Theme::BRIGHT_CYAN,
            results.len(),
            Theme::RESET
        );
        println!();

        for result in results {
            result.display(true);
            println!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_matches() {
        // Fixture uses neutral config-key strings and avoids
        // vendor-specific model names in test data.
        let session = SessionMetadataEx {
            id: "1".to_string(),
            name: "research".to_string(),
            model: "primary".to_string(),
            provider: "zeroclaw".to_string(),
            created_at: "2026-01-01".to_string(),
            last_active: "2026-01-02".to_string(),
            message_count: 5,
            tags: vec!["ai".to_string(), "research".to_string()],
            description: Some("AI research project".to_string()),
        };

        assert!(session.matches("research"));
        assert!(session.matches("primary"));
        assert!(session.matches("ai"));
        assert!(session.matches("zeroclaw"));
        assert!(!session.matches("nonexistent"));
    }

    #[test]
    fn test_tag_management() {
        let mut session = SessionMetadataEx {
            id: "1".to_string(),
            name: "test".to_string(),
            model: "test".to_string(),
            provider: "test".to_string(),
            created_at: "2026-01-01".to_string(),
            last_active: "2026-01-02".to_string(),
            message_count: 0,
            tags: vec![],
            description: None,
        };

        session.add_tag("important");
        assert!(session.tags.contains(&"important".to_string()));

        session.remove_tag("important");
        assert!(!session.tags.contains(&"important".to_string()));
    }
}
