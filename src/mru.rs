//--+ mru.rs
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MruEntry {
    pub path: PathBuf,
    pub display_name: String,
    pub line: usize,
    pub col: usize,

    /// How many times this file has been opened.
    #[serde(default)]
    pub open_count: usize,

    /// Timestamp of the last time this file was opened.
    #[serde(default = "default_last_opened")]
    pub last_opened: Option<SystemTime>,
}

fn default_last_opened() -> Option<SystemTime> {
    None
}

impl MruEntry {
    /// Return a human‑readable relative‑time string like "3m ago" or "2d ago".
    pub fn relative_time(&self) -> String {
        match self.last_opened {
            None => "never".to_string(),
            Some(then) => {
                let now = SystemTime::now();
                match now.duration_since(then) {
                    Ok(dur) => {
                        let secs = dur.as_secs();
                        if secs < 60 {
                            "just now".to_string()
                        } else if secs < 3600 {
                            format!("{}m ago", secs / 60)
                        } else if secs < 86400 {
                            format!("{}h ago", secs / 3600)
                        } else {
                            format!("{}d ago", secs / 86400)
                        }
                    }
                    Err(_) => "—".to_string(),
                }
            }
        }
    }
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

        let display_name = resolved_path.to_string_lossy().to_string();

        // Preserve open_count from existing entry (if any), then increment.
        let existing_count = self
            .entries
            .iter()
            .find(|e| e.path == resolved_path)
            .map(|e| e.open_count)
            .unwrap_or(0);

        self.entries.retain(|e| e.path != resolved_path);

        self.entries.push_front(MruEntry {
            path: resolved_path,
            display_name,
            line,
            col,
            open_count: existing_count + 1,
            last_opened: Some(SystemTime::now()),
        });

        while self.entries.len() > self.max_entries {
            self.entries.pop_back();
        }
    }

    /// Update only the cursor position (no count/timestamp bump).
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

    pub fn get_entries(&self) -> Vec<MruEntry> {
        self.entries.iter().cloned().collect()
    }

    pub fn get_by_index(&self, index: usize) -> Option<MruEntry> {
        self.entries.get(index).cloned()
    }

    /// Return entries sorted by open count (descending).
    pub fn entries_by_frequency(&self) -> Vec<MruEntry> {
        let mut v: Vec<MruEntry> = self.entries.iter().cloned().collect();
        v.sort_by(|a, b| b.open_count.cmp(&a.open_count));
        v
    }
}
