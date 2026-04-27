use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

use crate::cli::storage;

/// Command aliases - custom shortcuts for frequently used commands
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandAliases {
    aliases: HashMap<String, String>,
}

impl CommandAliases {
    /// Load aliases from file
    pub fn load() -> Result<Self> {
        let aliases_file = storage::config_dir()?.join("aliases.toml");

        if !aliases_file.exists() {
            return Ok(Self {
                aliases: HashMap::new(),
            });
        }

        let content = fs::read_to_string(&aliases_file)?;
        let table: toml::Table = toml::from_str(&content)?;

        let mut aliases = HashMap::new();

        if let Some(alias_section) = table.get("aliases") {
            if let Some(alias_table) = alias_section.as_table() {
                for (key, value) in alias_table {
                    if let Some(cmd) = value.as_str() {
                        aliases.insert(key.clone(), cmd.to_string());
                    }
                }
            }
        }

        Ok(Self { aliases })
    }

    /// Save aliases to file
    pub fn save(&self) -> Result<()> {
        let mut table = toml::map::Map::new();
        let mut alias_table = toml::map::Map::new();

        for (alias, command) in &self.aliases {
            alias_table.insert(alias.clone(), toml::Value::String(command.clone()));
        }

        table.insert("aliases".to_string(), toml::Value::Table(alias_table));

        let content = toml::to_string_pretty(&toml::Value::Table(table))?;

        let aliases_file = storage::config_dir()?.join("aliases.toml");
        fs::write(&aliases_file, content)?;

        Ok(())
    }

    /// Add or update an alias
    pub fn add(&mut self, alias: &str, command: &str) -> Result<()> {
        // Validate alias name (alphanumeric + underscore)
        if !alias.chars().all(|c| c.is_alphanumeric() || c == '_') {
            anyhow::bail!("Alias name must contain only alphanumeric characters and underscores");
        }

        // Prevent overwriting built-in commands
        let builtin_commands = vec![
            "help", "info", "model", "session", "history", "memory", "skill", "config", "clear",
            "save", "exit", "cron",
        ];

        if builtin_commands.contains(&alias) {
            anyhow::bail!("Cannot create alias with reserved command name: {}", alias);
        }

        self.aliases.insert(alias.to_string(), command.to_string());
        self.save()?;

        Ok(())
    }

    /// Remove an alias
    pub fn remove(&mut self, alias: &str) -> Result<()> {
        if self.aliases.remove(alias).is_none() {
            anyhow::bail!("Alias '{}' not found", alias);
        }
        self.save()?;
        Ok(())
    }

    /// Expand an alias to its full command
    pub fn expand(&self, input: &str) -> String {
        // Check if input starts with an alias
        let parts: Vec<&str> = input.split_whitespace().collect();

        if parts.is_empty() {
            return input.to_string();
        }

        let first_word = parts[0];

        match self.aliases.get(first_word) {
            Some(expanded) => {
                // Replace alias with expanded command, keeping remaining args
                if parts.len() > 1 {
                    format!("{} {}", expanded, parts[1..].join(" "))
                } else {
                    expanded.clone()
                }
            }
            None => input.to_string(),
        }
    }

    /// List all aliases
    pub fn list(&self) -> Vec<(String, String)> {
        self.aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Check if an alias exists
    pub fn exists(&self, alias: &str) -> bool {
        self.aliases.contains_key(alias)
    }
}

/// Default aliases for quick access
pub fn get_default_aliases() -> HashMap<String, String> {
    let mut aliases = HashMap::new();

    aliases.insert("h".to_string(), "help".to_string());
    aliases.insert("ll".to_string(), "session list".to_string());
    aliases.insert("lm".to_string(), "models list".to_string());
    aliases.insert("m".to_string(), "memory".to_string());
    aliases.insert("sm".to_string(), "session create main".to_string());

    aliases
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alias_expand() {
        let mut aliases = CommandAliases {
            aliases: HashMap::new(),
        };
        aliases.aliases.insert("h".to_string(), "help".to_string());
        aliases
            .aliases
            .insert("ll".to_string(), "session list".to_string());

        assert_eq!(aliases.expand("h"), "help");
        assert_eq!(aliases.expand("h something"), "help something");
        assert_eq!(aliases.expand("ll"), "session list");
        assert_eq!(aliases.expand("notanalias"), "notanalias");
    }

    #[test]
    fn test_alias_reserved_names() {
        let mut aliases = CommandAliases {
            aliases: HashMap::new(),
        };

        let result = aliases.add("help", "something");
        assert!(result.is_err());
    }

    #[test]
    fn test_default_aliases() {
        let defaults = get_default_aliases();
        assert_eq!(defaults.get("h"), Some(&"help".to_string()));
        assert_eq!(defaults.get("ll"), Some(&"session list".to_string()));
    }
}
