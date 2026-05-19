//! Persistent LLM session storage.

use crate::llm::{LlmMessage, LlmPreset, LlmRole};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Serializable message for disk storage
#[derive(Serialize, Deserialize)]
struct SavedMessage {
    role: String,
    content: String,
}

/// Serializable session file
#[derive(Serialize, Deserialize)]
struct SavedSession {
    name: String,
    preset: String,
    messages: Vec<SavedMessage>,
    updated_at: u64,
}

/// Manages saving/loading LLM sessions to disk
pub struct SessionManager {
    dir: PathBuf,
}

impl SessionManager {
    pub fn new(config_dir: &Path) -> Self {
        let dir = config_dir.join("llm_sessions");
        let _ = fs::create_dir_all(&dir);
        Self { dir }
    }

    fn path_for(&self, name: &str) -> PathBuf {
        // Sanitize: only allow alphanumeric, underscore, hyphen
        let safe: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        self.dir.join(format!("{}.json", safe))
    }

    fn active_path(&self) -> PathBuf {
        self.dir.join("_active")
    }

    fn preset_to_str(p: LlmPreset) -> &'static str {
        match p {
            LlmPreset::Chat => "chat",
            LlmPreset::CheckEnglish => "check_english",
            LlmPreset::TranslateToChinese => "translate_zh",
            LlmPreset::TranslateToEnglish => "translate_en",
            LlmPreset::Explain => "explain",
            LlmPreset::Summarize => "summarize",
            LlmPreset::Custom => "custom",
        }
    }

    fn preset_from_str(s: &str) -> LlmPreset {
        match s {
            "check_english" => LlmPreset::CheckEnglish,
            "translate_zh" => LlmPreset::TranslateToChinese,
            "translate_en" => LlmPreset::TranslateToEnglish,
            "explain" => LlmPreset::Explain,
            "summarize" => LlmPreset::Summarize,
            "custom" => LlmPreset::Custom,
            _ => LlmPreset::Chat,
        }
    }

    fn role_from_str(s: &str) -> LlmRole {
        match s {
            "assistant" => LlmRole::Assistant,
            "system" => LlmRole::System,
            "error" => LlmRole::Error,
            _ => LlmRole::User,
        }
    }

    fn role_to_str(r: &LlmRole) -> &'static str {
        match r {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "system",
            LlmRole::Error => "error",
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }

    /// Save current session to disk
    pub fn save(&self, name: &str, preset: LlmPreset, messages: &[LlmMessage]) -> Result<(), String> {
        let saved_msgs: Vec<SavedMessage> = messages
            .iter()
            .filter(|m| m.role != LlmRole::Error) // Don't persist error messages
            .map(|m| SavedMessage {
                role: Self::role_to_str(&m.role).to_string(),
                content: m.content.clone(),
            })
            .collect();

        let session = SavedSession {
            name: name.to_string(),
            preset: Self::preset_to_str(preset).to_string(),
            messages: saved_msgs,
            updated_at: Self::now_secs(),
        };

        let json = serde_json::to_string_pretty(&session).map_err(|e| format!("serialize: {}", e))?;

        fs::write(self.path_for(name), json).map_err(|e| format!("write: {}", e))?;

        // Mark as active
        fs::write(self.active_path(), name).map_err(|e| format!("write active: {}", e))?;

        Ok(())
    }

    /// Load a session by name
    pub fn load(&self, name: &str) -> Result<(Vec<LlmMessage>, LlmPreset), String> {
        let path = self.path_for(name);
        if !path.exists() {
            return Err(format!("session '{}' not found", name));
        }

        let json = fs::read_to_string(&path).map_err(|e| format!("read: {}", e))?;
        let session: SavedSession = serde_json::from_str(&json).map_err(|e| format!("parse: {}", e))?;

        let messages: Vec<LlmMessage> = session
            .messages
            .into_iter()
            .map(|m| LlmMessage::new(Self::role_from_str(&m.role), m.content))
            .collect();

        let preset = Self::preset_from_str(&session.preset);

        Ok((messages, preset))
    }

    /// Load the last active session
    pub fn load_active(&self) -> Option<(String, Vec<LlmMessage>, LlmPreset)> {
        let name = fs::read_to_string(self.active_path()).ok()?;
        let name = name.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let (messages, preset) = self.load(&name).ok()?;
        Some((name, messages, preset))
    }

    /// List all saved sessions
    pub fn list(&self) -> Vec<SessionInfo> {
        let mut sessions = Vec::new();

        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().to_string();
                if file_name.starts_with('_') || !file_name.ends_with(".json") {
                    continue;
                }

                let name = file_name.replace(".json", "");
                let meta = entry.metadata().ok();

                let preset = fs::read_to_string(entry.path()).ok().and_then(|json| {
                    serde_json::from_str::<SavedSession>(&json)
                        .ok()
                        .map(|s| Self::preset_from_str(&s.preset))
                });

                let msg_count = fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|json| serde_json::from_str::<SavedSession>(&json).ok().map(|s| s.messages.len()))
                    .unwrap_or(0);

                sessions.push(SessionInfo {
                    name,
                    message_count: msg_count,
                    preset,
                    modified: meta.and_then(|m| m.modified().ok()),
                });
            }
        }

        // Sort by modified time, newest first
        sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
        sessions
    }

    /// Delete a session
    pub fn delete(&self, name: &str) -> Result<(), String> {
        let path = self.path_for(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("delete: {}", e))?;
        }
        Ok(())
    }

    /// Rename a session
    pub fn rename(&self, old: &str, new: &str) -> Result<(), String> {
        let old_path = self.path_for(old);
        let new_path = self.path_for(new);
        if !old_path.exists() {
            return Err(format!("session '{}' not found", old));
        }
        fs::rename(&old_path, &new_path).map_err(|e| format!("rename: {}", e))?;

        // Update active marker if this was the active session
        if let Ok(active) = fs::read_to_string(self.active_path()) {
            if active.trim() == old {
                let _ = fs::write(self.active_path(), new);
            }
        }

        Ok(())
    }
}

/// Info about a saved session (for listing)
pub struct SessionInfo {
    pub name: String,
    pub message_count: usize,
    pub preset: Option<LlmPreset>,
    pub modified: Option<std::time::SystemTime>,
}
