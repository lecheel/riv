//! Configuration management.
//!
//! Loads config from `~/.config/riv/config.toml` (or OS equivalent),
//! falling back to sensible defaults. Supports runtime modification.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// Add at the bottom of config.rs

/// Persistent command and search history.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HistoryData {
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub search: Vec<String>,
}

impl HistoryData {
    /// Load history from disk, returning default if missing or corrupt.
    pub fn load() -> Self {
        let path = match Config::history_path() {
            Ok(p) => p,
            Err(_) => return Self::default(),
        };

        if !path.exists() {
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save history to disk, capping each list at 1000 entries.
    pub fn save(&self) {
        let path = match Config::history_path() {
            Ok(p) => p,
            Err(_) => return,
        };

        const MAX_HISTORY: usize = 1000;
        let command = if self.command.len() > MAX_HISTORY {
            self.command[self.command.len() - MAX_HISTORY..].to_vec()
        } else {
            self.command.clone()
        };

        let search = if self.search.len() > MAX_HISTORY {
            self.search[self.search.len() - MAX_HISTORY..].to_vec()
        } else {
            self.search.clone()
        };

        let data = Self { command, search };

        if let Ok(content) = toml::to_string_pretty(&data) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, content);
        }
    }
}

impl Config {
    /// Return the history file path (`~/.config/riv/history.toml`).
    pub fn history_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::config_dir()?.join("history.toml"))
    }
}

// ── Error ───────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("TOML serialization error: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("Config directory not found and could not be created: {0}")]
    ConfigDir(String),
}

// ── Theme / Colors ──────────────────────────────────────────────────

/// Named color theme.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    SolarizedDark,
    SolarizedLight,
    Dracula,
    GruvboxDark,
    Nord,
}

impl std::fmt::Display for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Theme::Dark => write!(f, "dark"),
            Theme::Light => write!(f, "light"),
            Theme::SolarizedDark => write!(f, "solarized-dark"),
            Theme::SolarizedLight => write!(f, "solarized-light"),
            Theme::Dracula => write!(f, "dracula"),
            Theme::GruvboxDark => write!(f, "gruvbox-dark"),
            Theme::Nord => write!(f, "nord"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeiumConfig {
    pub enabled: bool,
    pub api_key: Option<String>,
    pub debounce_ms: u64,
    /// Whether to show inline ghost text.
    pub ghost_text: bool,
}

impl Default for CodeiumConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            debounce_ms: 150,
            ghost_text: true,
        }
    }
}

// ── LLM Configuration ──────────────────────────────────────────────

/// LLM backend type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LlmBackend {
    /// Ollama (local)
    Ollama { endpoint: String, model: String },
    /// llama.cpp server (OpenAI-compatible)
    LlamaCpp { endpoint: String, model: String },
    /// OpenAI or compatible API
    OpenAi {
        endpoint: String,
        model: String,
        api_key: String,
    },
}

impl Default for LlmBackend {
    fn default() -> Self {
        LlmBackend::Ollama {
            endpoint: "http://localhost:11434".to_string(),
            model: "llama3.2".to_string(),
        }
    }
}

/// LLM configuration section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Which LLM backend to use.
    pub backend: LlmBackend,
    /// Temperature for generation (0.0 - 2.0).
    pub temperature: f32,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    /// Whether LLM features are enabled.
    pub enabled: bool,
    pub streaming: bool,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: LlmBackend::Ollama {
                endpoint: "http://localhost:11434".to_string(),
                model: "llama3.2".to_string(),
            },
            temperature: 0.7,
            max_tokens: 4096,
            timeout_secs: 120,
            streaming: true, // Enable by default
        }
    }
}

// ── Config ──────────────────────────────────────────────────────────

/// Top-level configuration for the editor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Editor theme.
    pub theme: Theme,
    #[serde(default)]
    pub codeium: CodeiumConfig,
    /// Show line numbers.
    pub line_numbers: bool,
    #[serde(default)]
    pub relative_line_numbers: bool,

    /// Tab width (in spaces).
    pub tab_width: u8,
    pub indent_guides: bool,

    /// Whether to use actual tab characters.
    pub use_tabs: bool,

    /// Show whitespace characters.
    pub show_whitespace: bool,

    /// Word wrap lines.
    pub word_wrap: bool,

    /// Auto-save interval in seconds (0 = disabled).
    pub auto_save: u64,

    /// Number of undo levels to keep.
    pub undo_levels: usize,

    /// Scroll offset (minimum lines to keep above/below cursor).
    pub scroll_offset: usize,

    /// Case sensitivity for search.
    pub case_sensitive_search: bool,

    /// Enable incremental search highlighting.
    pub incremental_search: bool,

    /// Enable LSP features.
    pub enable_lsp: bool,

    /// Enable git integration.
    pub enable_git: bool,

    /// Enable auto-completion.
    pub enable_completion: bool,

    /// Completion trigger length.
    pub completion_trigger_len: usize,

    /// Status bar style.
    pub statusbar_style: String,

    /// Ruler (column guide).
    pub ruler: Option<u16>,

    /// Custom keybinding overrides (stored as TOML table).
    #[serde(default)]
    pub keybindings: toml::Table,

    /// Leader key (single character). Default is space.
    pub leader: char,

    /// Filetype-specific overrides.
    #[serde(default)]
    pub filetype: toml::Table,

    /// LLM configuration.
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub format_on_save: bool,

    /// Float shortcut menu mappings (key string → action string).
    #[serde(default)]
    pub shortcuts: std::collections::HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            line_numbers: true,
            relative_line_numbers: false,
            tab_width: 4,
            indent_guides: true,
            use_tabs: false,
            show_whitespace: false,
            word_wrap: false,
            auto_save: 0,
            undo_levels: 1000,
            scroll_offset: 5,
            case_sensitive_search: false,
            incremental_search: true,
            enable_lsp: true,
            enable_git: true,
            enable_completion: true,
            completion_trigger_len: 2,
            statusbar_style: "mode-filename-modified".to_string(),
            ruler: Some(80),
            keybindings: toml::Table::new(),
            leader: ' ',
            filetype: toml::Table::new(),
            llm: LlmConfig::default(),
            codeium: CodeiumConfig::default(),
            format_on_save: false,
            shortcuts: std::collections::HashMap::new(),
        }
    }
}

impl Config {
    /// Return the config directory path.
    pub fn config_dir() -> Result<PathBuf, ConfigError> {
        let base = dirs::config_dir().ok_or_else(|| {
            ConfigError::ConfigDir("Could not determine config directory".to_string())
        })?;
        Ok(base.join("riv"))
    }

    /// Return the config file path.
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    /// Load configuration from disk, falling back to defaults.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)?;

        Ok(config)
    }

    /// Save the current configuration to disk.
    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = Self::config_dir()?;
        fs::create_dir_all(&dir)?;

        let path = Self::config_path()?;
        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;

        Ok(())
    }

    /// Merge another config into this one (values from `other` take precedence).
    pub fn merge(&mut self, other: Config) {
        if other.theme != Theme::default() {
            self.theme = other.theme;
        }
        self.line_numbers = other.line_numbers;
        self.relative_line_numbers = other.relative_line_numbers;
        self.tab_width = other.tab_width;
        self.use_tabs = other.use_tabs;
        self.show_whitespace = other.show_whitespace;
        self.word_wrap = other.word_wrap;
        self.auto_save = other.auto_save;
        self.undo_levels = other.undo_levels;
        self.scroll_offset = other.scroll_offset;
        self.case_sensitive_search = other.case_sensitive_search;
        self.incremental_search = other.incremental_search;
        self.enable_lsp = other.enable_lsp;
        self.enable_git = other.enable_git;
        self.enable_completion = other.enable_completion;
        self.completion_trigger_len = other.completion_trigger_len;
        if !other.statusbar_style.is_empty() {
            self.statusbar_style = other.statusbar_style;
        }
        if other.ruler.is_some() {
            self.ruler = other.ruler;
        }
        // Merge LLM config if non-default
        self.llm = other.llm;
        self.codeium = other.codeium;
        self.format_on_save = other.format_on_save;
    }

    /// Create a string representation of the tab/space settings.
    pub fn indent_info(&self) -> String {
        if self.use_tabs {
            "tabwidth: ".to_string() + &self.tab_width.to_string()
        } else {
            "spaces: ".to_string() + &self.tab_width.to_string()
        }
    }
}
