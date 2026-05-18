// src/vocab.rs
//! Local vocabulary manager for custom word completion.

use std::collections::HashSet;
use std::path::PathBuf;

/// Manages a local vocabulary list stored as a JSON file.
pub struct VocabManager {
    words: HashSet<String>,
    path: PathBuf,
}

impl VocabManager {
    /// Create a new manager pointing to the given JSON file path.
    pub fn new(path: PathBuf) -> Self {
        let mut manager = Self {
            words: HashSet::new(),
            path,
        };
        manager.load();
        manager
    }

    /// Load words from the JSON file (if it exists).
    pub fn load(&mut self) {
        if let Ok(data) = std::fs::read_to_string(&self.path) {
            if let Ok(words) = serde_json::from_str::<Vec<String>>(&data) {
                self.words = words.into_iter().collect();
            }
        }
    }

    /// Save the current word set back to the JSON file.
    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(&self.words)?;
        std::fs::write(&self.path, json)
    }

    /// Add a word to the vocabulary. Returns `true` if it was newly inserted.
    pub fn add(&mut self, word: &str) -> bool {
        let word = word.trim().to_lowercase();
        if word.is_empty() {
            return false;
        }
        if self.words.insert(word) {
            let _ = self.save();
            true
        } else {
            false
        }
    }

    /// Return a reference to the stored words.
    pub fn words(&self) -> &HashSet<String> {
        &self.words
    }
}
