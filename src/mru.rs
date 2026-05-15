use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MruEntry {
    pub path: PathBuf,
    pub display_name: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Serialize, Deserialize)]
struct MruData {
    mru_files: Vec<MruEntry>,
}

pub struct MruManager {
    entries: VecDeque<MruEntry>,
    max_entries: usize,
    save_path: PathBuf,
}

impl MruManager {
    pub fn new(max_entries: usize, save_path: PathBuf) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries),
            max_entries,
            save_path,
        }
    }

    /// Remove entries whose files no longer exist on disk.
    pub fn prune_missing(&mut self) {
        self.entries.retain(|e| e.path.exists());
    }

    /// Remove a specific path from the MRU list.
    pub fn remove(&mut self, path: &PathBuf) {
        let resolved = if let Ok(canonical) = std::fs::canonicalize(path) {
            canonical
        } else {
            path.clone()
        };
        self.entries.retain(|e| e.path != resolved);
    }

    pub fn load(&mut self) {
        if !self.save_path.exists() {
            return;
        }
        if let Ok(contents) = std::fs::read_to_string(&self.save_path) {
            if let Ok(data) = serde_json::from_str::<MruData>(&contents) {
                self.entries.clear();
                for entry in data.mru_files {
                    self.entries.push_back(entry);
                }
            }
        }
    }

    pub fn save(&self) {
        let data = MruData {
            mru_files: self.entries.iter().cloned().collect(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&data) {
            if let Some(parent) = self.save_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&self.save_path, json).ok();
        }
    }

    pub fn touch(&mut self, path: PathBuf, line: usize, col: usize) {
        let resolved_path = if let Ok(canonical) = std::fs::canonicalize(&path) {
            canonical
        } else if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(&path)
        };

        // let display_name = resolved_path
        // .file_name()
        // .and_then(|n| n.to_str())
        // .unwrap_or("?")
        // .to_string();

        let display_name = resolved_path.to_string_lossy().to_string();

        self.entries.retain(|e| e.path != resolved_path);

        self.entries.push_front(MruEntry {
            path: resolved_path,
            display_name,
            line,
            col,
        });

        while self.entries.len() > self.max_entries {
            self.entries.pop_back();
        }
    }

    pub fn get_entries(&self) -> Vec<MruEntry> {
        self.entries.iter().cloned().collect()
    }

    pub fn get_by_index(&self, index: usize) -> Option<MruEntry> {
        self.entries.get(index).cloned()
    }

    pub fn update_position(&mut self, path: &PathBuf, line: usize, col: usize) {
        let resolved_path = if let Ok(canonical) = std::fs::canonicalize(path) {
            canonical
        } else if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };

        if let Some(entry) = self.entries.iter_mut().find(|e| e.path == resolved_path) {
            entry.line = line;
            entry.col = col;
        }
    }
}
