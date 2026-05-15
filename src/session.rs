//! Session persistence — save/restore cursor positions across editor sessions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::buffer::CursorPosition;

/// Map of absolute file paths → saved cursor positions [line, col].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct PositionMap(HashMap<String, [usize; 2]>);

impl PositionMap {
    pub fn new() -> Self {
        Self::default()
    }

    fn positions_file() -> Option<PathBuf> {
        dirs::config_dir().map(|base| base.join("riv").join("positions.json"))
    }

    /// Load positions from disk.
    pub fn load() -> Self {
        let path = match Self::positions_file() {
            Some(p) => p,
            None => return Self::new(),
        };

        if !path.exists() {
            return Self::new();
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Self>(&content) {
                Ok(map) => map,
                Err(_e) => Self::new(),
            },
            Err(_e) => Self::new(),
        }
    }

    /// Save positions to disk.
    pub fn save(&self) {
        let path = match Self::positions_file() {
            Some(p) => p,
            None => return,
        };

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match serde_json::to_string_pretty(self) {
            Ok(content) => if let Err(_e) = std::fs::write(&path, content) {},
            Err(_e) => {}
        }
    }

    /// Record the cursor position for a file path (stored by absolute path).
    pub fn set(&mut self, path: &Path, pos: CursorPosition) {
        let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(path_str) = abs.to_str() {
            self.0.insert(path_str.to_string(), [pos.line, pos.col]);
        }
    }

    /// Get the saved cursor position for a file path.
    pub fn get(&self, path: &Path) -> Option<CursorPosition> {
        let abs = path.canonicalize().ok()?;
        let path_str = abs.to_str()?;
        let [line, col] = self.0.get(path_str)?;
        Some(CursorPosition::new(*line, *col))
    }

    /// Clean up entries for files that no longer exist on disk.
    pub fn cleanup(&mut self) {
        let dead: Vec<String> = self
            .0
            .keys()
            .filter(|p| !Path::new(p).exists())
            .cloned()
            .collect();
        for key in dead {
            self.0.remove(&key);
        }
    }
}
