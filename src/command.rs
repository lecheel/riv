// src/command.rs
//! Dynamic command registry for command-line (`:`) commands.
//!
//! Replaces the static `COMMAND_LIST` with a runtime registry that supports:
//! - Handler-backed commands (custom logic)
//! - Action-backed commands (simple dispatch to `Action`)
//! - Aliases (e.g. `quit` → `q`)
//! - Prefix-based completion (for command-line tab-complete)
//! - Fuzzy expansion (e.g. `:q` matches `:quit` if unambiguous)

use crate::action::Action;
use crate::editor::{CommandResult, Editor};
use std::collections::HashMap;
use std::collections::HashSet;

/// A command that can be invoked from the command line.
pub struct CommandEntry {
    /// Canonical command name (e.g. `"q"`).
    pub name: String,
    /// Human-readable description (shown in completion popups and `:help`).
    pub description: String,
    /// If `Some`, the command directly corresponds to an `Action`.
    /// Dispatched via `editor.process_action(action)`.
    pub action: Option<Action>,
    /// Custom handler for commands that aren't a simple action (e.g. `:set`, `:e`).
    /// Receives `&mut Editor` and the argument string after the command name.
    ///
    /// **Important**: This is a plain function pointer (`fn`), not a boxed closure.
    /// Function pointers are `Copy` and can be taken out of the registry without
    /// holding a reference, which avoids borrow‑checker conflicts when calling
    /// `handler(self, args)` while the registry is immutably borrowed.
    pub handler: Option<fn(&mut Editor, &str) -> CommandResult>,
}

/// Registry of all available commands and their aliases.
pub struct CommandRegistry {
    /// Canonical command entries keyed by name.
    commands: HashMap<String, CommandEntry>,
    /// Alias → canonical command name mapping (e.g. `"quit"` → `"q"`).
    aliases: HashMap<String, String>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    /// Register a command backed by an `Action`.
    ///
    /// Action-backed commands are dispatched by cloning the action and calling
    /// `editor.process_action(action)`.  Use this for commands that are trivial
    /// wrappers around editor actions (e.g. `:ShowHelp`).
    pub fn register_action(&mut self, name: &str, action: Action, desc: &str) {
        self.commands.insert(
            name.to_string(),
            CommandEntry {
                name: name.to_string(),
                description: desc.to_string(),
                action: Some(action),
                handler: None,
            },
        );
    }

    /// Register a command with a custom handler (static function pointer).
    ///
    /// Handler-backed commands receive `&mut Editor` and the raw argument
    /// string (everything after the command name and whitespace).
    /// Use this for commands with complex logic (e.g. `:set`, `:e <path>`).
    ///
    /// **Note**: `handler` must be a `fn` pointer – closures or other callables
    /// are not accepted because they may capture state and are not `Copy`.
    pub fn register_handler(&mut self, name: &str, handler: fn(&mut Editor, &str) -> CommandResult, desc: &str) {
        self.commands.insert(
            name.to_string(),
            CommandEntry {
                name: name.to_string(),
                description: desc.to_string(),
                action: None,
                handler: Some(handler),
            },
        );
    }

    /// Add an alias (e.g. `quit` → `q`).
    pub fn alias(&mut self, alias: &str, target: &str) {
        self.aliases.insert(alias.to_string(), target.to_string());
    }

    /// Resolve a name (or alias) to the canonical command name.
    /// Returns `None` if the name is unknown.

    /// Resolve a name (or alias) to the canonical command name.
    /// Returns `None` if the name is unknown.
    /// The returned `&str` is tied to the lifetime of `self` (the registry).
    pub fn resolve(&self, cmd: &str) -> Option<&str> {
        if self.commands.contains_key(cmd) {
            // Exact match: return the stored canonical name (from the map)
            self.commands.get(cmd).map(|e| e.name.as_str())
        } else {
            // Alias: return the target canonical name (also stored in map)
            self.aliases.get(cmd).map(|s| s.as_str())
        }
    }

    /// Get the entry for a canonical command name.
    pub fn get(&self, name: &str) -> Option<&CommandEntry> {
        self.commands.get(name)
    }

    /// Iterator over canonical command names only (for dedup).
    pub fn iter_names(&self) -> impl Iterator<Item = &String> {
        self.commands.keys()
    }

    /// Return **all** names (canonical + aliases) for completion and fuzzy matching.
    /// Each element is `(display_name, canonical_name)`.
    ///
    /// Example: `[("q", "q"), ("quit", "q"), ("qa", "q"), ("w", "w"), ("write", "w")]`
    pub fn all_names(&self) -> Vec<(&str, &str)> {
        let mut out = Vec::with_capacity(self.commands.len() + self.aliases.len());
        for name in self.commands.keys() {
            out.push((name.as_str(), name.as_str()));
        }
        for (alias, target) in &self.aliases {
            // Only include aliases whose target actually exists.
            if self.commands.contains_key(target) {
                out.push((alias.as_str(), target.as_str()));
            }
        }
        out
    }

    /// Return all unique display names (canonical + aliases) as a flat `Vec<&str>`.
    pub fn display_names(&self) -> Vec<&str> {
        let mut set = HashSet::with_capacity(self.commands.len() + self.aliases.len());
        for name in self.commands.keys() {
            set.insert(name.as_str());
        }
        for alias in self.aliases.keys() {
            set.insert(alias.as_str());
        }
        set.into_iter().collect()
    }

    /// Find all canonical names that start with the given prefix.
    /// Used for tab-completion and fuzzy expansion.
    pub fn prefix_match(&self, prefix: &str) -> Vec<&str> {
        let prefix_lower = prefix.to_lowercase();
        self.display_names()
            .into_iter()
            .filter(|name| name.to_lowercase().starts_with(&prefix_lower))
            .collect()
    }

    /// Number of registered canonical commands.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}
