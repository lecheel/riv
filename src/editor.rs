// src/editor.rs
//! Editor — the central coordinator for the text editor.
//!
//! Holds buffers, windows, the keybind manager, and the main state machine
//! that processes events and executes actions.

use std::time::Instant;

use crate::action::Action;
use crate::buffer::BufferKind;
use crate::buffer::{Buffer, BufferCollection, BufferId};
use crate::codeium::CodeiumManager;
use crate::command::CommandRegistry;
use crate::completion::CompletionEngine;
use crate::config::{Config, HistoryData};
use crate::dirty::DirtyState;
use crate::ed::build::BuildExt;
use crate::ed::git_commit::GitCommitExt;
use crate::ed::visual::BlockInsertState;
use crate::ed::GhostTextExt;
use crate::ed::GotoDefExt;
use crate::ed::LastAction;
use crate::ed::LspExt;
use crate::ed::MarksExt;
use crate::ed::RepeatableAction;
use crate::ed::ReplaceExt;
use crate::ed::SearchExt;
use crate::ed::ShortcutsExt;
use crate::ed::{
    BufferOpsExt, CommandExt, CompletionExt, EditingExt, FileOpsExt, GitExt, LlmExt, MovementExt, RepeatExt, RipgrepExt, VisualExt,
    WindowExt,
};
use crate::ed::{GitDiffExt, GitLogExt, GitStatusExt};
use crate::ed::{TextObjectExt, TextObjectKind, TextObjectOperator};
use crate::ghost_text::GhostTextManager;
use crate::keybind::{
    apply_custom_keybindings, default_command_keymap, default_insert_keymap, default_normal_keymap, default_visual_keymap, KeyBindManager,
    KeyBindResult,
};
use crate::llm::LlmPreset;
use crate::misc::format_shortcut_keys;
use crate::misc::parse_shortcut_keys;
use crate::mru::MruManager;
use crate::prompt::MiniInputPrompt;
use crate::prompt::PromptAction;
use crate::terminal::{Key, TerminalEvent};
use crate::vocab::VocabManager;
use crate::window::WindowManager;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpPhase {
    PendingChar1,
    PendingChar2,
    Active,
}

impl Default for JumpPhase {
    fn default() -> Self {
        Self::PendingChar1
    }
}

#[derive(Debug, Clone)]
pub struct JumpTarget {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Default)]
pub struct JumpState {
    pub active: bool,
    pub phase: JumpPhase,
    pub input: String,
    pub targets: Vec<JumpTarget>,
    pub labels: Vec<(usize, String)>,
}

// ── Editor modes ────────────────────────────────────────────────────

/// Editor modes, analogous to Vim modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
    VisualBlock,
    Command,
    LlmPrompt,
    OperatorPending,
}

impl Mode {
    /// Return the mode as a short string for display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Replace => "REPLACE",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
            Mode::VisualBlock => "V-BLOCK",
            Mode::Command => "COMMAND",
            Mode::OperatorPending => "OPERATOR",
            Mode::LlmPrompt => "LLM",
        }
    }

    /// Return the mode name as used in the keybind manager.
    pub fn keybind_name(&self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Replace => "insert",
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => "visual",
            Mode::Command => "command",
            Mode::OperatorPending => "normal",
            Mode::LlmPrompt => "command",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,
    Backward,
}

// ── Command result ──────────────────────────────────────────────────

/// Result of processing an editor command/action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    /// Nothing changed.
    NoOp,
    /// Content was modified; redraw needed.
    ContentChanged,
    /// Viewport changed; redraw needed.
    ViewChanged,
    /// Mode changed.
    ModeChanged(Mode),
    /// A message should be shown to the user.
    Message(String),
    /// An error occurred.
    Error(String),
    /// Editor should quit.
    Quit,
}

/// Pending state for multi-key sequences.
#[derive(Debug, Clone, Default)]
pub struct PendingState {
    /// Whether a brief-mode Ctrl-O is pending.
    pub brief_ctrl_o: bool,
    /// Any other pending keys.
    pub pending_keys: Vec<char>,
}

// ── Float popup ─────────────────────────────────────────────────────
/// A floating popup overlaid on the editor area. Used for hunk previews,
/// quick info, and other transient UI. Dismissed with ESC or any key.
#[derive(Debug, Clone)]
pub struct FloatPopup {
    /// Title shown at the top of the popup.
    pub title: String,
    /// Content lines (not including title bar or border).
    pub lines: Vec<String>,
    /// Width in columns (including border). 0 = auto-calculate.
    pub width: u16,
    /// Maximum height in rows (including title + border). 0 = auto.
    pub max_height: u16,
}

impl FloatPopup {
    pub fn new(title: impl Into<String>, lines: Vec<String>) -> Self {
        let title = title.into();
        let content_width = lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(20)
            .max(title.chars().count());
        let width = (content_width + 4).max(40).min(120) as u16;
        let max_height = (lines.len() + 3).min(20) as u16;
        Self {
            title,
            lines,
            width,
            max_height,
        }
    }
}

/// State for interactive substitute confirmation (`:s/pat/repl/gc`)
#[derive(Debug)]
pub struct SubstituteConfirmState {
    /// The compiled regex pattern
    pub regex: regex::Regex,
    /// The replacement string (regex‑crate format)
    pub replacement: String,
    /// Whether to replace all occurrences on a line (g flag)
    pub global: bool,
    /// The buffer we're operating on
    pub buffer_id: BufferId,
    /// Start of the line range (inclusive, 0‑based)
    pub start_line: usize,
    /// End of the line range (inclusive, 0‑based)
    pub end_line: usize,
    /// Total substitutions made so far
    pub subs_made: usize,
    /// Next line to search from
    pub next_line: usize,
    /// Next byte offset within next_line (for global mode)
    pub next_byte_offset: usize,
    /// Current match being confirmed: (line, start_byte, end_byte)
    pub current_match: Option<(usize, usize, usize)>,
}

// ── Editor ──────────────────────────────────────────────────────────

/// The main editor struct — coordinates all subsystems.
pub struct Editor {
    // ==================== Core Components ====================
    /// All open buffers.
    pub buffers: BufferCollection,
    /// Window manager.
    pub windows: WindowManager,
    /// Current editor mode (Normal/Insert/Visual).
    pub mode: Mode,
    /// Configuration.
    pub config: Config,
    /// Index of the active buffer in the buffers collection.
    pub active_buffer_idx: usize,
    // ==================== Input & Keybindings ====================
    /// Keybinding manager.
    pub keybinds: KeyBindManager,
    /// The operator pending character (e.g., 'd', 'y', 'c').
    pub pending_operator: Option<char>,
    /// Pending operator motion (e.g., 'w', 'b', 'j').
    pub pending_motion: Option<Action>,
    /// Accumulated digit characters for the numeric count prefix.
    pub pending_count: String,
    /// Resolved count for the current action.
    pub current_count: usize,
    /// Whether we're waiting for a register name after pressing " in Normal/Visual mode.
    pub register_pending: bool,
    /// When set, the next yank/paste operation uses this register instead of the default.
    pub pending_register: Option<char>,
    /// Whether we're waiting for a register name after pressing Ctrl-R in Insert mode.
    pub insert_register_pending: bool,
    /// Pending state for multi-key sequences (Brief mode, etc.).
    pub pending: PendingState,
    /// Whether we're waiting for a character target after pressing dt/df    
    pub inline_delete_pending: Option<char>,
    /// Current which-key hints (for rendering).
    pub which_key_hints: Vec<(String, String)>,
    /// Debounce timeout for which-key popup (milliseconds).
    pub which_key_debounce_timeout: u64,
    /// Timer for which-key debouncing.
    pub which_key_debounce_timer: Option<Instant>,
    /// Whether we're waiting for a register in command mode.
    pub cmd_waiting_register: bool,

    // ==================== Jump Mode ====================
    /// State for 2-char EasyMotion/AceJump style jumping.
    pub jump: JumpState,

    /// Search, Mark, and Tag subsystem state.
    pub search: crate::state::search::SearchState,

    // ==================== Clipboard & Registers ====================
    /// Yank (copy) register.
    pub yank_register: String,
    /// Named registers (a–z) for yank/paste via "xp.
    pub named_registers: std::collections::HashMap<char, String>,

    /// Popup and Overlay subsystem state.
    pub popup: crate::state::popup::PopupState,

    /// Git subsystem state.
    pub git: crate::state::git::GitState,

    /// LSP subsystem state.
    pub lsp: crate::state::lsp::LspState,

    // ==================== AI / Codeium ====================
    /// Ghost text manager (inline AI suggestions).
    pub ghost_text: GhostTextManager,
    /// Codeium server manager.
    pub codeium: CodeiumManager,
    /// Whether we're waiting for the user to paste a Codeium auth token.
    pub codeium_auth_pending: bool,
    /// Set to true on first tick to auto-start Codeium if configured.
    auto_start_codeium: bool,

    /// LLM subsystem state.
    pub llm: crate::state::llm::LlmState,

    // ==================== Completion ====================
    /// Completion engine (word-based, buffer-word, future LSP).
    pub completion: CompletionEngine,
    /// Command-line completion engine.
    pub command_completion: CompletionEngine,
    /// Timestamp of the last insert-mode edit, for undo group timeout.
    pub(crate) last_edit_time: Instant,
    /// Timeout (milliseconds) after which a new insert-mode keystroke starts a new undo group.
    pub(crate) undo_break_timeout_ms: u64,
    /// Local vocabulary manager for custom wordlists.
    pub vocab: VocabManager,

    pub completion_debounce_timer: Option<Instant>,
    pub completion_debounce_ms: u64,

    // ==================== State & Quit ====================
    /// Status message (displayed in the status bar).
    pub status_message: Option<String>,
    /// Error message (displayed in the status bar).
    pub error_message: Option<String>,
    /// Infobar message (displayed on Line 3, used for formatter warnings).
    pub infobar_message: Option<String>,
    /// Whether the editor should quit.
    pub should_quit: bool,
    /// Whether we are waiting for 'y/n' confirmation to force quit with unsaved changes.
    pub force_quit_pending: bool,
    /// Whether a full redraw is needed.
    pub dirty: DirtyState,
    /// Block insert state (defined in visual module).
    pub block_insert: Option<BlockInsertState>,
    /// Whether the replace character is waiting for input.
    pub replace_char_pending: bool,
    /// Whether a paste operation is in progress (suppresses per-char LSP/completion).
    pub paste_in_progress: bool,
    /// Whether the editor should auto-start LSP.

    /// In-memory buffer positions for session-only buffer switching.
    /// Maps BufferId → (cursor_position, scroll_line).
    /// Covers ALL buffer kinds (including Ripgrep, GitDiff, etc.)
    pub buffer_positions: std::collections::HashMap<crate::buffer::BufferId, (crate::buffer::CursorPosition, usize)>,

    // ==================== Command System ====================
    /// Dynamic command registry for `:` commands.
    pub command_registry: CommandRegistry,
    /// Command line prompt state.
    pub command_prompt: MiniInputPrompt,
    /// Search line prompt state.
    // pub search_prompt: MiniInputPrompt,

    // ==================== Repeat & History ====================
    //-- struct Editor step 1 (anchor dont remove) --//
    /// Last action for dot repeat
    pub last_action: LastAction,
    /// Whether the last action is repeatable
    pub repeat_pending: bool,
    /// Last ripgrep search pattern.
    pub last_rg_pattern: Option<String>,
    /// Last ripgrep search root directory.
    pub last_rg_root_dir: Option<PathBuf>,
    /// Cached last ripgrep output for instant reuse.
    pub last_rg_output: Option<crate::ripgrep::RipgrepOutput>,
    /// Quickfix list results from ripgrep.
    pub quickfix_results: Vec<crate::ripgrep::RipgrepResult>,
    /// Current index in quickfix list.
    pub quickfix_index: usize,

    // ==================== MRU ====================
    /// Most-recently-used file manager.
    pub mru: MruManager,

    // ==================== Float Shortcuts ====================
    /// Whether the float shortcut menu is currently active (transient state).
    pub shortcut_active: bool,
    /// Parsed key sequences → action map for the float shortcut menu.
    pub active_shortcuts: Vec<(Vec<crate::terminal::Key>, crate::action::Action)>,
    /// Keys typed so far in a multi-key shortcut sequence.
    pub shortcut_pending_keys: Vec<crate::terminal::Key>,
    pub shortcut_visual_context: Option<String>,

    // ==================== Build ====================
    pub build: crate::state::build::BuildState,

    // === visual selectrion ===
    /// Last visual selection range (start_line, end_line), 0-based, inclusive.
    /// Persists after leaving visual mode — used by :'<,'> and :% commands.
    /// Updated when entering command mode from visual mode.
    pub visual_selection_range: Option<(usize, usize)>,

    // ==================== Terminal ====================
    /// Current buffer width (terminal columns).
    pub term_width: u16,
    /// Current buffer height (terminal rows).
    pub term_height: u16,

    // ==================== powerline ===================
    /// Cached function name for the powerline (updated in tick).
    pub current_function_name: Option<String>,
    /// Whether content changed since last function-name cache update.
    pub fn_name_needs_update: bool,
    /// Last (buffer_id, cursor_line) we computed the function name for.
    pub fn_name_cache_key: Option<(BufferId, usize)>,

    // ==================== Message Passing ====================
    /// App message receiver (async tasks → editor). Polled in tick().
    pub app_rx: crate::msgbox::AppReceiver,
    /// App message sender (editor → async tasks).
    pub app_tx: crate::msgbox::AppSender,
}

impl Editor {
    /// Send a message to the LSP task, with debug logging.
    fn lsp_send(&mut self, msg: crate::lsp::LspMessage) {
        if self.lsp.tx.is_closed() {
            self.lsp.connected = false;
            return;
        }
        match self.lsp.tx.send(msg) {
            Ok(()) => {}
            Err(_e) => {
                self.lsp.connected = false;
            }
        }
    }

    /// Create a new editor instance with default configuration.
    ///
    /// Initializes all subsystems including:
    /// - Buffer and window management
    /// - Keybindings (normal, insert, visual, command modes)
    /// - Command registry with built-in commands
    /// - LSP and Codeium integration
    /// - LLM runtime and channels
    /// - Search and command history
    /// - MRU tracking
    /// - Completion engines
    ///
    /// # Returns
    /// A fully initialized `Editor` instance ready to run.
    pub fn new(config: Config) -> Self {
        let leader = config.leader;
        let completion_trigger_len = config.completion_trigger_len;
        let enable_lsp = config.enable_lsp;
        let mut keybinds = KeyBindManager::new();
        keybinds.register_keymap("normal", default_normal_keymap());
        keybinds.register_keymap("insert", default_insert_keymap());
        keybinds.register_keymap("visual", default_visual_keymap());
        keybinds.register_keymap("command", default_command_keymap());

        let keybindings_ref = config.keybindings.clone();
        // Deserialize the raw TOML map into our structured KeyBindingsConfig
        let keybindings_config: crate::keybind::KeyBindingsConfig =
            toml::Value::Table(keybindings_ref.clone()).try_into().unwrap_or_default();
        apply_custom_keybindings(&mut keybinds, &keybindings_config, Some(leader));

        let history_data = HistoryData::load();

        let command_registry = crate::command_registry::build_command_registry();
        let codeium_debounce_ms = config.codeium.debounce_ms;
        let mut command_completion = CompletionEngine::new(1);
        command_completion.max_items = 20;

        let mut buffers = BufferCollection::new();
        let buffer_id = buffers.new_buffer();

        let mut windows = WindowManager::new();
        windows.create_window(buffer_id);

        let (app_tx, app_rx) = crate::msgbox::message_channel();

        // Create the runtime FIRST so we can spawn on it
        let llm = crate::state::llm::LlmState::new();

        let lsp_tx = if enable_lsp {
            let tx_for_lsp = app_tx.clone();
            let mut lsp_manager = crate::lsp::LspManager::new(tx_for_lsp);
            let sender = lsp_manager.get_sender();

            llm.runtime.spawn(async move {
                lsp_manager.run().await;
            });

            sender
        } else {
            // Dummy channel — receiver is dropped immediately, so
            // is_closed() returns true and lsp_send() silently no-ops.
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            tx
        };

        let command_history_len = history_data.command.len();

        let mut command_prompt = MiniInputPrompt::new();
        command_prompt.history = history_data.command.clone();
        command_prompt.history_index = command_history_len;

        let active_shortcuts = {
            let mut list = Vec::new();
            for (key_str, action_str) in &config.shortcuts {
                if let Some(keys) = parse_shortcut_keys(key_str) {
                    if let Some(action) = crate::keybind::parse_action_str(action_str) {
                        list.push((keys, action));
                    } else {
                        log::warn!("[config] shortcuts: unknown action '{}' for key '{}'", action_str, key_str);
                    }
                } else {
                    log::warn!(
                        "[config] shortcuts: invalid key '{}' (use chars like 'f' or sequences like 'gf'; modifiers not supported)",
                        key_str
                    );
                }
            }
            list.sort_by(
                |a: &(Vec<crate::terminal::Key>, crate::action::Action), b: &(Vec<crate::terminal::Key>, crate::action::Action)| {
                    a.0.len()
                        .cmp(&b.0.len())
                        .then_with(|| format_shortcut_keys(&a.0).cmp(&format_shortcut_keys(&b.0)))
                },
            );
            list
        };

        Editor {
            // Core Components
            buffers,
            windows,
            mode: Mode::Normal,
            config,
            active_buffer_idx: 0,
            keybinds,

            //-- impl Editor fn new() step 2 (anchor dont remove) --//
            // Input & Keybindings
            pending_operator: None,
            pending_motion: None,
            pending_count: String::new(),
            current_count: 1,
            pending_register: None, // for "a
            inline_delete_pending: None,
            register_pending: false, // waiting for a key after pressing
            insert_register_pending: false,
            pending: PendingState::default(),
            which_key_hints: Vec::new(),
            which_key_debounce_timeout: 200,
            which_key_debounce_timer: None,
            cmd_waiting_register: false,

            // Search & Navigation
            search: crate::state::search::SearchState::new(history_data.search.clone()),
            // position_map: crate::session::PositionMap::load(),
            jump: JumpState::default(),
            // Clipboard & Registers
            yank_register: String::new(),
            named_registers: std::collections::HashMap::new(),

            // Popups & Overlays
            popup: crate::state::popup::PopupState::new(),

            git: crate::state::git::GitState::new(),
            lsp: crate::state::lsp::LspState::new(lsp_tx),

            // AI / Codeium
            ghost_text: GhostTextManager::new(),
            codeium: CodeiumManager::new(codeium_debounce_ms),
            codeium_auth_pending: false,
            auto_start_codeium: true,

            // LLM Features
            llm,

            // Completion
            completion: CompletionEngine::new(completion_trigger_len),
            command_completion,
            completion_debounce_timer: None,
            completion_debounce_ms: 50,
            last_edit_time: Instant::now(),
            undo_break_timeout_ms: 2000,
            vocab: {
                let mut v = VocabManager::new(Config::config_dir().unwrap_or_else(|_| PathBuf::from(".")).join("vocab.json"));
                v.load(); // ← make sure this is called
                v
            },
            // State & Quit
            status_message: None,
            error_message: None,
            infobar_message: None,
            should_quit: false,
            force_quit_pending: false,
            dirty: DirtyState {
                full: true,
                ..Default::default()
            },
            block_insert: None,
            replace_char_pending: false,
            paste_in_progress: false,

            // Command System
            command_registry,
            command_prompt,

            // Repeat & History
            last_action: LastAction::default(),
            repeat_pending: false,
            last_rg_pattern: None,
            last_rg_root_dir: None,
            last_rg_output: None,
            quickfix_results: Vec::new(),
            quickfix_index: 0,

            visual_selection_range: None,
            build: crate::state::build::BuildState::new(),

            // MRU
            mru: {
                let mut m = MruManager::new(100, Config::config_dir().unwrap_or_else(|_| PathBuf::from(".")).join("mru.json"));
                m.load();
                m.prune_missing();
                m
            },

            buffer_positions: std::collections::HashMap::new(),
            shortcut_active: false,
            active_shortcuts,
            shortcut_pending_keys: Vec::new(),
            shortcut_visual_context: None,
            current_function_name: None,
            fn_name_needs_update: true,
            fn_name_cache_key: None,

            // Terminal
            term_width: 80,
            term_height: 24,

            // Message Passing
            app_rx,
            app_tx,
        }
    }

    // ── Public helpers ──────────────────────────────────────────────

    /// Start Codeium. Loads API key from config > env > ~/.codeium/config.toml.
    pub fn start_codeium(&mut self) -> Result<(), String> {
        if !self.config.codeium.enabled {
            return Err("Codeium is disabled in config".to_string());
        }
        if self.codeium.is_connected {
            self.set_status("Codeium: already connected ✓".to_string());
            return Ok(()); // Already connected
        }
        if self.codeium.is_starting() {
            return Err("Codeium: server is already starting...".to_string());
        }

        // Load API key: config > env > ~/.codeium/config.toml
        let api_key = self
            .config
            .codeium
            .api_key
            .clone()
            .or_else(|| std::env::var("CODEIUM_API_KEY").ok())
            .or_else(crate::codeium::load_api_key_from_config)
            .ok_or_else(|| "Codeium: no API key. Run :codeium-auth or set CODEIUM_API_KEY env var".to_string())?;

        self.codeium
            .start(api_key, &self.llm.runtime)
            .map_err(|e| format!("Codeium: {}", e))?;

        self.set_status("Codeium: starting server...".to_string());
        Ok(())
    }

    /// Return the current mode as a display string.
    pub fn mode_display(&self) -> &'static str {
        self.mode.as_str()
    }

    /// Whether the editor is in Brief mode (future feature).
    pub fn is_brief_mode(&self) -> bool {
        false
    }

    /// Return a reference to the buffer in the active window (if any).
    pub fn current_buffer(&self) -> Option<&Buffer> {
        self.windows.active_window().and_then(|w| self.buffers.get(&w.buffer_id))
    }

    /// Ensure the cursor is visible in the active window's viewport.
    pub fn ensure_cursor_visible_all(&mut self) {
        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        let Some(buffer_id) = buffer_id else { return };
        let max_line = self.buffers.get(&buffer_id).map(|b| b.line_count()).unwrap_or(0);
        if let Some(window) = self.windows.active_window_mut() {
            window.ensure_cursor_visible(max_line);
        }
    }

    /// Set a status message (replaces any error).
    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.error_message = None;
    }

    /// Set an error message (replaces any status).
    pub fn set_error(&mut self, msg: String) {
        self.error_message = Some(msg);
        self.status_message = None;
    }

    /// Clear both status and error messages.
    pub fn clear_messages(&mut self) {
        self.status_message = None;
        self.error_message = None;
        self.infobar_message = None;
    }

    /// Set an infobar message (Line 3 — used for formatter warnings, etc.).
    pub fn set_infobar_message(&mut self, msg: String) {
        self.infobar_message = Some(msg);
        self.dirty.status_infobar = true;
    }

    /// Handle a terminal resize event.
    pub fn handle_resize(&mut self, width: u16, height: u16) {
        self.term_width = width;
        self.term_height = height;
        self.windows.resize_all(width, height);
        self.dirty.mark_all();
    }

    /// Process a terminal event.
    /// Automatically routes `CommandResult::Error` → `error_message` and
    /// `CommandResult::Message` → `status_message`.
    pub fn process_event(&mut self, event: Option<TerminalEvent>) -> CommandResult {
        let event = match event {
            Some(e) => e,
            None => return CommandResult::NoOp,
        };

        let result = match event {
            TerminalEvent::Key(key) => self.process_key(key),
            TerminalEvent::Resize(w, h) => {
                self.handle_resize(w, h);
                CommandResult::ViewChanged
            }
            TerminalEvent::Mouse(_mouse) => CommandResult::NoOp,
        };

        // ── Notify LSP of content changes (centralized) ──
        // This catches ContentChanged from ALL paths: process_key's NoMatch
        // branch (insert-mode typing) AND process_action's editing actions.
        if result == CommandResult::ContentChanged {
            self.invalidate_git_gutter();
            self.notify_lsp_change();
        }

        match &result {
            CommandResult::Error(msg) => {
                self.error_message = Some(msg.clone());
                self.status_message = None;
                self.dirty.status_powerline = true;
                self.dirty.status_cmdline = true;
                self.dirty.status_infobar = true;
                self.dirty.cursor = true;
            }
            CommandResult::Message(msg) => {
                self.status_message = Some(msg.clone());
                self.error_message = None;
                self.dirty.status_powerline = true;
                self.dirty.status_cmdline = true;
                self.dirty.status_infobar = true;
                self.dirty.cursor = true;
            }
            CommandResult::ContentChanged => {
                self.search.matches_dirty = true;
                self.fn_name_needs_update = true;
                if self.completion.active {
                    // Only mark the current line as dirty — skip full window redraw
                    // This prevents the popup from flashing
                    let cursor_line = self.windows.active_window().map(|w| w.cursor.position.line).unwrap_or(0);
                    self.dirty.mark_single_line(cursor_line);
                } else {
                    self.dirty.mark_insert();
                }
                // Dismiss infobar warnings once the user starts editing
                if self.infobar_message.is_some() {
                    self.infobar_message = None;
                    self.dirty.status_infobar = true;
                }
            }
            CommandResult::ViewChanged => {
                self.dirty.windows = true;
                self.dirty.status_powerline = true;
                self.dirty.status_cmdline = true;
                self.dirty.status_infobar = true;
                self.dirty.cursor = true;
            }
            CommandResult::ModeChanged(_) => {
                self.dirty.windows = true;
                self.dirty.status_powerline = true;
                self.dirty.status_cmdline = true;
                self.dirty.status_infobar = true;
                self.dirty.cursor = true;
            }
            _ => {}
        }
        // Safety net: always persist positions on quit, regardless of path
        if result == CommandResult::Quit {
            self.save_all_positions();
        }
        result
    }

    // =====================================================================
    // ── KEY PROCESSING ────────────────────────────────────────────────
    // =====================================================================

    fn process_key(&mut self, key: Key) -> CommandResult {
        // ── Force quit confirmation interception ──
        if self.force_quit_pending {
            match key {
                Key::Char('n') | Key::Char('N') => {
                    // NO: Don't save, just quit
                    self.save_all_positions();
                    self.should_quit = true;
                    return CommandResult::Quit;
                }
                Key::Char('y') | Key::Char('Y') => {
                    // YES: Save first, then quit
                    self.force_quit_pending = false;
                    match self.save() {
                        Ok(()) => {
                            self.save_all_positions();
                            self.should_quit = true;
                            return CommandResult::Quit;
                        }
                        Err(e) => {
                            self.set_infobar_message(format!("Save failed: {}", e));
                            return CommandResult::ViewChanged;
                        }
                    }
                }
                Key::Char('c') | Key::Char('C') | Key::Escape => {
                    // CANCEL: Don't save, don't quit
                    self.force_quit_pending = false;
                    self.clear_messages();
                    self.set_status("Quit cancelled.".to_string());
                    self.dirty.mark_all();
                    return CommandResult::ViewChanged;
                }
                _ => {
                    // Ignore other keys while waiting for n/y/c
                    return CommandResult::NoOp;
                }
            }
        }

        // ── Substitute confirmation interception ──
        if self.search.substitute_confirm.is_some() {
            match key {
                Key::Char('y') | Key::Char('Y') => {
                    return self.substitute_confirm_yes();
                }
                Key::Char('n') | Key::Char('N') => {
                    return self.substitute_confirm_no();
                }
                Key::Char('a') | Key::Char('A') => {
                    return self.substitute_confirm_all();
                }
                Key::Char('q') | Key::Char('Q') | Key::Escape => {
                    return self.substitute_confirm_quit();
                }
                Key::Char('l') | Key::Char('L') => {
                    return self.substitute_confirm_last();
                }
                _ => {
                    return CommandResult::NoOp;
                }
            }
        }

        // ── Jump mode (EasyMotion / AceJump) ──
        if self.jump.active {
            match key {
                Key::Escape | Key::Ctrl('c') => {
                    crate::ed::motion::cancel_jump(self);
                    self.dirty.mark_all();
                    return CommandResult::ViewChanged;
                }
                Key::Char(c) => {
                    let stay_in_jump = crate::ed::motion::handle_jump_key(self, c);
                    self.dirty.mark_all();
                    return if stay_in_jump {
                        CommandResult::NoOp
                    } else {
                        CommandResult::ViewChanged
                    };
                }
                _ => {
                    // Any other key cancels the jump
                    crate::ed::motion::cancel_jump(self);
                    self.dirty.mark_all();
                    return CommandResult::ViewChanged;
                }
            }
        }

        // ── SEARCH INPUT MODE ──
        if self.search.input_active {
            return self.process_search_input_key(key);
        }

        // ── LLM Input scratchpad special keys ──
        if let Some(result) = self.handle_llm_input_buffer_key(&key) {
            return result;
        }

        // ── LLM PROMPT MODE ──
        if self.mode == Mode::LlmPrompt {
            return self.process_llm_prompt_key(key);
        }

        // ── Popups take precedence over special buffer key bindings ──
        let popup_active = self.popup.buffer_list.is_some()
            || self.popup.mru.is_some()
            || self.popup.file_picker.is_some()
            || self.popup.function_list.is_some()
            || self.popup.keymap.is_some()
            || self.popup.help.is_some()
            || self.popup.mark_list.is_some();

        // ── Popup Key Dispatch ──
        if let Some(result) = self.handle_popup_keys(&key) {
            return result;
        }

        //-- process_key popup_active (anchor dont remove) --//
        // ── Build buffer special keys ── BUILD

        if !popup_active {
            if let Some(result) = self.handle_build_buffer_key(&key) {
                return result;
            }
            if let Some(result) = self.handle_git_status_buffer_key(&key) {
                return result;
            }
            if let Some(result) = self.handle_git_diff_buffer_key(&key) {
                return result;
            }
            if let Some(result) = self.handle_git_commit_buffer_key(&key) {
                return result;
            }
            if let Some(result) = self.handle_git_log_buffer_key(&key) {
                return result;
            }
        }

        // ── Mark pending (waiting for mark name after m) ── MARK
        if let Some(result) = self.handle_mark_pending_key(&key) {
            return result;
        }

        // ── Goto mark pending (waiting for mark name after `) ──
        if let Some(result) = self.handle_goto_mark_pending_key(&key) {
            return result;
        }

        // ── Inline delete pending (waiting for target char after dt/df) ──
        if let Some(mode) = self.inline_delete_pending.take() {
            match key {
                Key::Char(c) => {
                    let inclusive = mode == 'f';
                    return self.delete_inline_target(c, inclusive);
                }
                Key::Escape | Key::Ctrl('c') => {
                    // Cancel
                    return CommandResult::NoOp;
                }
                _ => {
                    // Invalid key — cancel pending state
                    return CommandResult::NoOp;
                }
            }
        }

        // ── Replace char pending (waiting for char after r) ──
        if self.replace_char_pending {
            self.replace_char_pending = false;
            let count = self.search.replace_count;
            match key {
                Key::Char(c) => {
                    return self.with_undo_group(|s| {
                        s.replace_chars_at_cursor(c, count);
                        CommandResult::ContentChanged
                    });
                }
                Key::Enter => {
                    return self.with_undo_group(|s| {
                        s.replace_char_with_newline(count);
                        CommandResult::ContentChanged
                    });
                }
                _ => {
                    // Cancel on Escape or any other key
                    return CommandResult::NoOp;
                }
            }
        }

        // ── Register pending (waiting for register name after ") ──
        if self.register_pending {
            self.register_pending = false;
            if let Key::Char(c) = key {
                // Allow a-z, ", +, *, and %
                if c.is_ascii_lowercase() || c == '"' || c == '+' || c == '*' || c == '%' {
                    self.pending_register = Some(c);
                    self.dirty.status_infobar = true;
                    return CommandResult::NoOp;
                }
            }
            // Invalid register character — cancel
            self.pending_register = None;
            return CommandResult::NoOp;
        }

        //-- Escape in Normal mode (ESCAPE) --//
        if self.mode == Mode::Normal && key == Key::Escape {
            self.clear_messages();
            self.pending_count.clear();
            self.which_key_hints.clear();
            self.cancel_which_key_debounce();
            self.keybinds.clear_pending();
            self.search.mark_pending = false;
            self.search.goto_mark_pending = false;
            self.shortcut_pending_keys.clear();
            if self.git.diff_popup.is_some() {
                self.git.diff_popup = None;
                self.dirty.diff = true;
                self.dirty.cursor = true;
                self.dirty.mark_all();
                return CommandResult::NoOp;
            }
            // Inside the existing Escape-in-Normal block, add:
            if self.register_pending {
                self.register_pending = false;
                self.pending_register = None;
            }
            if self.replace_char_pending {
                self.replace_char_pending = false;
            }
            if self.inline_delete_pending.is_some() {
                self.inline_delete_pending = None;
            }
            self.dirty.cursor = true;
            self.dirty.status_infobar = true;
            // self.dirty.mark_all();
            return CommandResult::NoOp;
        }

        // ── COUNT PREFIX ACCUMULATION ──
        if matches!(self.mode, Mode::Normal | Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
            if let Key::Char(c) = key {
                if c.is_ascii_digit() && (c != '0' || !self.pending_count.is_empty()) {
                    self.pending_count.push(c);
                    if self.pending_count.len() >= 5 {
                        let num: usize = self.pending_count.parse().unwrap_or(1);
                        if num > 99999 {
                            self.pending_count = "99999".to_string();
                        }
                    }
                    // self.refresh_which_key_hints();
                    // self.dirty.mark_all();
                    self.start_which_key_debounce();
                    self.dirty.status_infobar = true;
                    return CommandResult::NoOp;
                }
            }
        }

        // ── Completion navigation for insert/replace mode ──  COMP
        if (self.mode == Mode::Insert || self.mode == Mode::Replace) && self.completion.active {
            match key {
                Key::Up => {
                    self.select_prev_completion();
                    self.dirty.mark_completion_scroll();
                    return CommandResult::NoOp;
                }
                Key::Down => {
                    self.select_next_completion();
                    self.dirty.mark_completion_scroll();
                    return CommandResult::NoOp;
                }
                Key::Right => {
                    let is_mid_word = self
                        .windows
                        .active_window()
                        .and_then(|w| {
                            let col = w.cursor.position.col;
                            let line = w.cursor.position.line;
                            self.buffers.get(&w.buffer_id).and_then(|b| b.line_text(line)).map(|text| {
                                use unicode_segmentation::UnicodeSegmentation;
                                text.graphemes(true)
                                    .nth(col)
                                    .and_then(|g| g.chars().next())
                                    .map_or(false, |c| c.is_alphanumeric() || c == '_' || c == '-')
                            })
                        })
                        .unwrap_or(false);
                    if is_mid_word {
                        self.close_completion_popup();
                        self.ghost_text.clear();
                        // fall through to normal Right (MoveRight)
                    } else {
                        return self.confirm_completion();
                    }
                }
                Key::Left => {
                    self.completion.cancel();
                    self.dirty.windows = true;
                    self.dirty.cursor = true;
                    // Fall through to process the Left key normally
                }
                Key::Tab => {
                    if self.completion.selected_item().is_some() {
                        let result = self.confirm_completion();
                        return result;
                    }
                    self.completion.cancel();
                    self.dirty.windows = true;
                    self.dirty.cursor = true;
                    // Fall through
                }
                Key::BackTab => {
                    self.select_prev_completion();
                    self.dirty.mark_completion_scroll();
                    return CommandResult::NoOp;
                }
                Key::Enter => {
                    let result = self.confirm_completion();
                    return result;
                }
                Key::Escape => {
                    self.completion.cancel();
                    self.dirty.windows = true;
                    self.dirty.cursor = true;
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }
                _ => {}
            }
        }

        // ── Insert mode register prefix (Ctrl-R) ──
        if self.insert_register_pending {
            self.insert_register_pending = false;
            self.popup.register = None;
            self.dirty.mark_all();

            if let Key::Char(c) = key {
                if let Some(content) = self.resolve_register(c) {
                    if !content.is_empty() {
                        self.insert_text_at_cursor(&content);
                        return CommandResult::ContentChanged;
                    } else {
                        self.set_infobar_message(format!("Register '{}' is empty", c));
                        return CommandResult::ViewChanged;
                    }
                } else {
                    self.set_infobar_message(format!("Invalid register '{}'", c));
                    return CommandResult::ViewChanged;
                }
            }
            // Escape or other keys just cancel the pending state
            return CommandResult::NoOp;
        }

        // ── COMMAND MODE INPUT ──
        if self.mode == Mode::Command {
            // Show hints when Ctrl-R is initially pressed
            if key == Key::Ctrl('r') {
                self.cmd_waiting_register = true;
                self.which_key_hints = vec![
                    ("Register".to_string(), String::new()),
                    ("^W".to_string(), "word".to_string()),
                    ("^L".to_string(), "line".to_string()),
                    ("^A".to_string(), "whole line".to_string()),
                    ("%".to_string(), "file path".to_string()),
                    ("^P".to_string(), "abs file path".to_string()),
                    ("ESC".to_string(), "cancel".to_string()),
                ];
                self.which_key_debounce_timer = None;
                self.dirty.status_infobar = true;
                self.dirty.cursor = true; // Ensure cursor stays visible in prompt
                return CommandResult::ViewChanged; // Force render to show hints!
            }

            if self.cmd_waiting_register {
                // Clear waiting state and infobar hints
                self.cmd_waiting_register = false;
                let had_hints = !self.which_key_hints.is_empty();
                self.which_key_hints.clear();
                if had_hints {
                    self.dirty.status_infobar = true;
                }

                match key {
                    Key::Ctrl('w') => {
                        let word = self.word_under_cursor_in_current_buffer();
                        if !word.is_empty() {
                            self.command_prompt.buffer.insert_str(self.command_prompt.cursor, &word);
                            self.command_prompt.cursor += word.len();
                            self.trigger_command_completion();
                            self.dirty.mark_all();
                            return CommandResult::ViewChanged;
                        } else {
                            self.set_infobar_message("No word under cursor".to_string());
                            return CommandResult::ViewChanged;
                        }
                    }
                    Key::Ctrl('l') => {
                        let line = self.current_line_content();
                        if !line.is_empty() {
                            self.command_prompt.buffer.insert_str(self.command_prompt.cursor, &line);
                            self.command_prompt.cursor += line.len();
                            self.trigger_command_completion();
                            self.dirty.mark_all();
                            return CommandResult::ViewChanged;
                        } else {
                            self.set_infobar_message("Current line is empty".to_string());
                            return CommandResult::ViewChanged;
                        }
                    }
                    Key::Char('%') | Key::Ctrl('p') => {
                        // Insert absolute file path
                        let path = self
                            .current_buffer()
                            .and_then(|b| b.file_path.as_ref())
                            .and_then(|p| p.canonicalize().ok())
                            .and_then(|p| p.to_str().map(|s| s.to_string()))
                            .unwrap_or_default();

                        if !path.is_empty() {
                            self.command_prompt.buffer.insert_str(self.command_prompt.cursor, &path);
                            self.command_prompt.cursor += path.len();
                            self.trigger_command_completion();
                            self.dirty.mark_all();
                            return CommandResult::ViewChanged;
                        } else {
                            self.set_infobar_message("No file path for current buffer".to_string());
                            return CommandResult::ViewChanged;
                        }
                    }
                    Key::Ctrl('a') => {
                        // Insert whole line from cursor
                        let line = self.current_line_content();
                        self.command_prompt.buffer.insert_str(self.command_prompt.cursor, &line);
                        self.command_prompt.cursor += line.len();
                        self.trigger_command_completion();
                        self.dirty.mark_all();
                        return CommandResult::ViewChanged;
                    }
                    Key::Escape | Key::Ctrl('c') => {
                        // Cancel register wait, but let Escape fall through to cancel Command mode
                    }
                    _ => {
                        // Invalid register key, cancel wait and swallow the key.
                        // We must redraw to clear the hints from the infobar.
                        return CommandResult::ViewChanged;
                    }
                }
                // If we didn't return above (e.g. on Escape), fall through to normal command handling
            }

            if self.command_completion.active {
                match key {
                    Key::Tab => {
                        self.command_completion.select_next();
                        if let Some(item) = self.command_completion.selected_item() {
                            // item.text already contains the full command + arg, e.g. "e /tmp/foo"
                            self.command_prompt.buffer = item.text.clone();
                            self.command_prompt.cursor = self.command_prompt.buffer.len();
                        }
                        self.dirty.mark_all();
                        return CommandResult::ViewChanged;
                    }
                    Key::BackTab => {
                        self.command_completion.select_prev();
                        if let Some(item) = self.command_completion.selected_item() {
                            self.command_prompt.buffer = item.text.clone();
                            self.command_prompt.cursor = self.command_prompt.buffer.len();
                        }
                        self.dirty.mark_all();
                        return CommandResult::ViewChanged;
                    }
                    Key::Enter => {
                        self.command_completion.cancel();
                        return self.execute_command();
                    }
                    _ => {
                        self.command_completion.cancel();
                    }
                }
            }

            // ── Prefix-smart history navigation ──
            if key == Key::Up {
                let _ = self.command_history_up();
                self.trigger_command_completion();
                self.dirty.mark_all();
                return CommandResult::ViewChanged;
            }
            if key == Key::Down {
                let _ = self.command_history_down();
                self.trigger_command_completion();
                self.dirty.mark_all();
                return CommandResult::ViewChanged;
            }

            return match self.command_prompt.handle_key(&key) {
                PromptAction::Changed => {
                    self.trigger_command_completion();
                    self.dirty.mark_all();
                    CommandResult::ViewChanged
                }
                PromptAction::Submit => {
                    self.command_completion.cancel();
                    self.execute_command()
                }
                PromptAction::Cancel => {
                    self.command_prompt.clear();
                    self.enter_mode(Mode::Normal);
                    CommandResult::ModeChanged(Mode::Normal)
                }
                PromptAction::None => CommandResult::NoOp,
            };
        }

        if let Key::Paste(ref text) = key {
            return self.handle_paste(text.clone());
        }

        let mode_name = self.mode.keybind_name();

        match self.keybinds.process_key(mode_name, key) {
            Some(KeyBindResult::Action(action)) => {
                self.cancel_which_key_debounce();
                self.which_key_hints.clear();
                self.current_count = if self.pending_count.is_empty() {
                    1
                } else {
                    let n = self.pending_count.parse().unwrap_or(1).max(1);
                    self.pending_count.clear();
                    n
                };
                let result = self.process_action(action);
                // If a register was pending but the action didn't consume it
                // (e.g. "a j or "a i), clear it. Yank/Paste/Delete actions
                // call .take() on pending_register, so it's already None.
                if self.pending_register.is_some() {
                    self.pending_register = None;
                }
                self.current_count = 1;
                result
            }
            Some(KeyBindResult::Pending) => {
                self.start_which_key_debounce();
                self.dirty.status_infobar = true;
                CommandResult::NoOp
            }

            Some(KeyBindResult::NoMatch(raw_key)) => {
                self.cancel_which_key_debounce();
                self.which_key_hints.clear();
                self.pending_count.clear();
                self.pending_register = None;

                if self.mode == Mode::Insert {
                    if let Key::Char(c) = raw_key {
                        self.ensure_undo_group();
                        self.insert_char_at_cursor(c);
                        // TODO ghost_text later
                        // self.request_ghost_text();

                        // ── Completion: only update for trigger chars or when popup
                        //    is already active.  Calling unconditionally would hit
                        //    the auto-trigger path (Case 3) on every keystroke and
                        //    cause duplicate LSP requests in the log.
                        if c == '.' || c == ':' || self.completion.active {
                            self.maybe_update_completion();
                        }

                        return CommandResult::ContentChanged;
                    }
                } else if self.mode == Mode::Replace {
                    if let Key::Char(c) = raw_key {
                        self.ensure_undo_group();
                        self.overwrite_char_at_cursor(c);

                        if c == '.' || c == ':' || self.completion.active {
                            self.maybe_update_completion();
                        }

                        return CommandResult::ContentChanged;
                    }
                }
                CommandResult::NoOp
            }
            None => CommandResult::NoOp,
        }
    }

    // =====================================================================
    // ── ACTION DISPATCH ──────────────────────────────────────────────
    // =====================================================================

    pub(crate) fn process_action(&mut self, action: Action) -> CommandResult {
        // ── In Vim, most actions (especially movement) clear the status/error message. ──
        let is_passthrough = matches!(action, Action::None | Action::ShowHelp | Action::UndoBreak);

        if !is_passthrough && self.mode != Mode::Command && self.mode != Mode::LlmPrompt && !self.search.input_active {
            self.clear_messages();

            // Cancel force quit confirmation if the user performs another action
            if self.force_quit_pending {
                self.force_quit_pending = false;
            }
        }

        let reg = self.pending_register;

        match &action {
            // ── Mode changes ────────────────────────────
            Action::EnterNormalMode => self.enter_mode(Mode::Normal),
            Action::EnterInsertMode => self.enter_insert_mode_at_cursor(),
            Action::EnterAppendMode => self.enter_append_mode_at_cursor(),
            Action::EnterInsertLineStart => self.enter_insert_mode_line_start(),
            Action::EnterAppendLineEnd => self.enter_append_mode_line_end(),
            Action::EnterReplaceMode => self.enter_mode(Mode::Replace),
            Action::EnterVisualMode => self.enter_mode(Mode::Visual),
            Action::EnterVisualLineMode => self.enter_mode(Mode::VisualLine),
            Action::EnterVisualBlockMode => self.enter_mode(Mode::VisualBlock),
            Action::BlockInsert => self.block_insert(),
            Action::BlockAppend => self.block_append(),
            Action::EnterCommandMode => {
                self.command_prompt.clear();
                self.clear_messages();

                // ── Capture visual range and pre-fill '<,'> ──
                if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
                    self.visual_selection_range = self.visual_line_range();
                    self.command_prompt.buffer = "'<,'>".to_string();
                    self.command_prompt.cursor = self.command_prompt.buffer.len();
                }

                self.enter_mode(Mode::Command)
            }
            Action::TriggerCodeiumCompletion => {
                if self.codeium.is_connected {
                    // self.request_codeium();
                    // self.completion.active = true;
                    self.request_codeium_force(); // ← bypasses guards
                    self.dirty.mark_all();
                    crate::editor::CommandResult::ViewChanged
                } else {
                    crate::editor::CommandResult::Error("Codeium not connected. Use :codeium to start.".into())
                }
            }
            Action::RegisterPrefix => {
                self.register_pending = true;
                self.pending_register = None;
                CommandResult::NoOp
            }
            Action::InsertRegisterPrefix => {
                self.insert_register_pending = true;
                self.show_register_popup();
                CommandResult::NoOp
            }
            Action::SetMark => {
                self.search.mark_pending = true;
                CommandResult::NoOp
            }
            Action::GotoMark => {
                self.search.goto_mark_pending = true;
                CommandResult::NoOp
            }
            Action::JumpBack => self.jump_back(),

            // ── LLM ──────────────────────────────────────
            Action::LlmOpen => {
                let llm_id = self.ensure_llm_buffer();
                if let Some(window) = self.windows.active_window_mut() {
                    window.set_buffer(llm_id);
                }
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Action::LlmClose => {
                // Switch back to a normal buffer if we are in the LLM buffer
                if let Some(window) = self.windows.active_window() {
                    if let Some(buf) = self.buffers.get(&window.buffer_id) {
                        if buf.kind == BufferKind::Llm {
                            if let Some(other_id) = self.buffers.iter().find(|b| b.kind == BufferKind::Normal).map(|b| b.id) {
                                if let Some(w) = self.windows.active_window_mut() {
                                    w.set_buffer(other_id);
                                }
                            }
                        }
                    }
                }
                CommandResult::ViewChanged
            }
            Action::LlmQuickCheckEnglish => {
                let was_visual = matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock);

                let text = if was_visual {
                    self.get_selection_text().unwrap_or_default()
                } else {
                    self.current_line_content()
                };

                // Clear visual selection if active AND return to Normal mode
                if let Some(w) = self.windows.active_window_mut() {
                    w.selection_anchor = None;
                }
                if was_visual {
                    self.mode = Mode::Normal;
                    self.dirty.status_powerline = true;
                }

                if text.trim().is_empty() {
                    self.set_infobar_message("No text to check".to_string());
                    return CommandResult::ViewChanged;
                }

                // Mark this response for infobar + register 'e' instead of LLM buffer
                self.llm.infobar_response = true;
                self.llm.infobar_accumulator.clear();
                self.llm.active_preset = Some(LlmPreset::CheckEnglish);
                self.llm.active_context = Some(text.clone());

                self.set_status("Checking English…".to_string());
                self.dirty.status_infobar = true;

                // Send directly — do NOT enter LlmPrompt mode
                let result = self.llm_send_from_prompt(text);

                if was_visual {
                    CommandResult::ModeChanged(Mode::Normal)
                } else {
                    result
                }
            }
            Action::LlmQuickPrompt => {
                let context = if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
                    let text = self.get_selection_text();
                    if let Some(window) = self.windows.active_window_mut() {
                        window.selection_anchor = None;
                    }
                    text // Option<String> (Some if selection existed)
                } else {
                    None // No selection, act like the simple 1-line prompt
                };

                if let Some(selected_text) = context {
                    // ── We have a visual selection ──
                    // Open split scratchpad if single window, otherwise fallback to 1-line
                    if self.windows.len() == 1 {
                        self.llm.todo_prefix = true;
                        return self.llm_setup_split_session(selected_text);
                    } else {
                        self.llm.active_context = Some(selected_text);
                        self.llm.todo_prefix = true;
                        self.llm.active_preset = None;
                        self.llm.prompt.clear();
                        self.mode = Mode::LlmPrompt;
                        self.dirty.mark_all();
                        return CommandResult::ModeChanged(Mode::LlmPrompt);
                    }
                } else {
                    // ── No visual selection (Normal mode) ──
                    // Just use the simple 1-line prompt like before
                    self.llm.active_context = None;
                    self.llm.todo_prefix = false;
                    self.llm.active_preset = None;
                    self.llm.prompt.clear();
                    self.mode = Mode::LlmPrompt;
                    self.dirty.mark_all();
                    return CommandResult::ModeChanged(Mode::LlmPrompt);
                }
            }
            Action::LlmEnterPrompt => {
                self.llm.active_preset = None;
                self.llm.active_context = None;
                self.llm.prompt.clear();
                self.mode = Mode::LlmPrompt;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::LlmPrompt)
            }
            Action::LlmClearHistory => {
                self.llm.buffer.clear_history();
                self.dirty.mark_all();
                CommandResult::Message("LLM history cleared".to_string())
            }
            Action::LlmQuickTranslateChinese => {
                let ctx = self.shortcut_visual_context.take().or_else(|| self.get_selection_text()); // fallback for direct visual keybind
                self.llm_quick_action(LlmPreset::TranslateToChinese, &ctx.unwrap_or_default())
            }
            Action::LlmQuickTranslateEnglish => {
                self.llm.active_preset = Some(LlmPreset::TranslateToEnglish);
                self.llm.active_context = self.get_selection_text();
                self.llm.prompt.clear();
                self.mode = Mode::LlmPrompt;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::LlmPrompt)
            }
            Action::LlmQuickExplain => {
                self.llm.active_preset = Some(LlmPreset::Explain);
                self.llm.active_context = self.get_selection_text();
                self.llm.prompt.clear();
                self.mode = Mode::LlmPrompt;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::LlmPrompt)
            }
            Action::LlmQuickSummarize => {
                self.llm.active_preset = Some(LlmPreset::Summarize);
                self.llm.active_context = self.get_selection_text();
                self.llm.prompt.clear();
                self.mode = Mode::LlmPrompt;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::LlmPrompt)
            }

            // ── Movement (count-aware) ──────────────────
            Action::MoveLeft => {
                if self.mode == Mode::Insert || self.mode == Mode::Replace {
                    self.completion.cancel();
                }
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.move_cursor_left();
                }
                result
            }
            Action::MoveRight => {
                if self.mode == Mode::Insert || self.mode == Mode::Replace {
                    self.completion.cancel();
                }
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.move_cursor_right();
                }
                result
            }
            Action::MoveUp => {
                if self.mode == Mode::Insert || self.mode == Mode::Replace {
                    self.completion.cancel();
                }
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.move_cursor_up();
                }
                result
            }
            Action::MoveDown => {
                if self.mode == Mode::Insert || self.mode == Mode::Replace {
                    self.completion.cancel();
                }
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.move_cursor_down();
                }
                result
            }
            Action::MoveWordForward => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.move_word_forward();
                }
                result
            }
            Action::MoveWordBack => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.move_word_back();
                }
                result
            }
            Action::MoveWordEnd => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.move_word_end();
                }
                result
            }
            Action::MoveLineStart => self.move_line_start(),
            Action::MoveLineEnd => {
                if self.current_count > 1 {
                    self.move_to_column(self.current_count - 1)
                } else {
                    self.move_line_end()
                }
            }
            Action::MoveFileStart => {
                if self.current_count > 1 {
                    self.move_to_line(self.current_count)
                } else {
                    self.move_file_start()
                }
            }
            Action::MoveFileEnd => {
                if self.current_count > 1 {
                    self.move_to_line(self.current_count)
                } else {
                    self.move_file_end()
                }
            }
            Action::MoveToLine(n) => self.move_to_line(*n),
            Action::MoveToPosition { line, col } => self.move_to_position(*line, *col),
            Action::MatchBracket => self.match_bracket(),
            // ── Scrolling (count-aware) ────────────────
            Action::ScrollUp => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.scroll_up();
                }
                result
            }
            Action::ScrollDown => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.scroll_down();
                }
                result
            }
            Action::ScrollLeft => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.scroll_left();
                }
                result
            }
            Action::ScrollRight => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.scroll_right();
                }
                result
            }
            Action::PageUp => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.page_up();
                }
                result
            }
            Action::PageDown => {
                let mut result = CommandResult::ViewChanged;
                for _ in 0..self.current_count {
                    result = self.page_down();
                }
                result
            }
            Action::ScrollCenter => self.scroll_center(),

            // ── Editing ─────────────────────────────────
            Action::InsertChar(c) => {
                // Dismiss ghost text if user types something different
                if self.ghost_text.is_visible() && self.should_dismiss_ghost() {
                    self.dismiss_ghost_text();
                }

                self.ensure_undo_group();
                self.insert_char_at_cursor(*c);
                self.request_ghost_text();
                self.maybe_update_completion();

                CommandResult::ContentChanged
            }
            Action::InsertNewline => {
                self.ensure_undo_group();
                self.insert_newline_at_cursor();
                CommandResult::ContentChanged
            }
            Action::InsertTab => {
                self.ensure_undo_group();
                self.insert_tab_at_cursor();
                CommandResult::ContentChanged
            }

            Action::DeleteAroundBraces => self.operate_on_pair('{', '}', TextObjectKind::Around, TextObjectOperator::Delete),
            Action::DeleteInsideBraces => self.operate_on_pair('{', '}', TextObjectKind::Inner, TextObjectOperator::Delete),
            Action::DeleteAroundBrackets => self.operate_on_pair('[', ']', TextObjectKind::Around, TextObjectOperator::Delete),
            Action::DeleteInsideBrackets => self.operate_on_pair('[', ']', TextObjectKind::Inner, TextObjectOperator::Delete),
            Action::DeleteAroundParens => self.operate_on_pair('(', ')', TextObjectKind::Around, TextObjectOperator::Delete),
            Action::DeleteInsideParens => self.operate_on_pair('(', ')', TextObjectKind::Inner, TextObjectOperator::Delete),
            Action::DeleteAroundQuotes => self.operate_on_pair('"', '"', TextObjectKind::Around, TextObjectOperator::Delete),
            Action::DeleteInsideQuotes => self.operate_on_pair('"', '"', TextObjectKind::Inner, TextObjectOperator::Delete),

            Action::DeleteBuffer => {
                // Save cursor position before closing
                if let Some(window) = self.windows.active_window() {
                    if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                        if let Some(ref path) = buffer.file_path {
                            self.search.position_map.set(path, window.cursor.position);
                        }
                    }
                }
                self.delete_buffer(false)
            }

            Action::DeleteChar | Action::DeleteCharForward => {
                use crate::ed::DeleteDirection;
                self.record_action(
                    RepeatableAction::DeleteChars {
                        count: self.current_count,
                        direction: DeleteDirection::Right,
                    },
                    self.current_count,
                );
                self.with_undo_group(|s| {
                    for _ in 0..s.current_count {
                        s.delete_char_at_cursor();
                    }
                    CommandResult::ContentChanged
                })
            }

            Action::Backspace => {
                if self.mode == Mode::Insert {
                    self.ensure_undo_group();
                    self.delete_char_before_cursor();
                    self.maybe_update_completion();
                    CommandResult::ContentChanged
                } else if self.mode == Mode::Replace {
                    // In Replace mode, backspace moves left without deleting
                    self.move_cursor_left()
                } else {
                    CommandResult::NoOp
                }
            }

            Action::DeleteLine => {
                // Record BEFORE the undo group
                self.record_action(RepeatableAction::DeleteLine, self.current_count);
                // Then execute with undo group
                self.with_undo_group(|s| {
                    s.delete_n_lines(s.current_count);
                    CommandResult::ContentChanged
                })
            }
            Action::DeleteWord => {
                self.record_action(RepeatableAction::DeleteWordBack, self.current_count);
                self.with_undo_group(|s| {
                    for _ in 0..s.current_count {
                        s.delete_word_before_cursor();
                    }
                    CommandResult::ContentChanged
                })
            }
            Action::DeleteWordForward => {
                self.record_action(RepeatableAction::DeleteWordForward, self.current_count);
                self.with_undo_group(|s| {
                    for _ in 0..s.current_count {
                        s.delete_word_after_cursor();
                    }
                    CommandResult::ContentChanged
                })
            }
            Action::DeleteToLineEnd => {
                self.record_action(RepeatableAction::DeleteToLineEnd, self.current_count);
                self.with_undo_group(|s| {
                    s.delete_to_line_end();
                    CommandResult::ContentChanged
                })
            }
            Action::DeleteToFileEnd => self.with_undo_group(|s| s.delete_to_file_end()),
            Action::DeleteToLineStart => {
                self.record_action(RepeatableAction::DeleteToLineStart, self.current_count);
                self.with_undo_group(|s| {
                    s.delete_to_line_start();
                    CommandResult::ContentChanged
                })
            }

            Action::Register => {
                self.set_status("TODO.".to_string());
                // TODO impl
                CommandResult::NoOp
            }
            Action::DeleteAroundFunction => {
                self.record_action(RepeatableAction::DeleteAroundFunction, self.current_count);
                let result = self.operate_on_function(TextObjectKind::Around, TextObjectOperator::Delete);
                // Reparse tree after deletion so subsequent tree-sitter queries work
                if matches!(result, CommandResult::ContentChanged) {
                    if let Some(window) = self.windows.active_window() {
                        let buffer_id = window.buffer_id;
                        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                            buffer.reparse_tree();
                        }
                    }
                }
                result
            }
            Action::DeleteSelection => {
                match self.mode {
                    Mode::Visual => self.delete_visual_selection(),
                    Mode::VisualLine => self.delete_visual_line_selection(),
                    Mode::VisualBlock => self.delete_block_selection(),
                    _ => self.delete_current_line(),
                }
                CommandResult::ContentChanged
            }
            Action::ChangeSelection => {
                self.close_undo_group();
                match self.mode {
                    Mode::Visual => self.delete_visual_selection(),
                    Mode::VisualLine => self.delete_visual_line_selection(),
                    Mode::VisualBlock => self.delete_block_selection(),
                    _ => self.delete_current_line(),
                }
                self.mode = Mode::Insert;
                CommandResult::ModeChanged(Mode::Insert)
            }
            Action::ReplaceChar => {
                self.replace_char_pending = true;
                self.search.replace_count = self.current_count;
                CommandResult::NoOp
            }
            Action::OpenLineBelow => self.with_undo_group(|s| {
                for _ in 0..s.current_count.saturating_sub(1) {
                    s.open_line_below_raw();
                }
                s.open_line_below();
                CommandResult::ContentChanged
            }),
            Action::OpenLineAbove => self.with_undo_group(|s| {
                for _ in 0..s.current_count.saturating_sub(1) {
                    s.open_line_above_raw();
                }
                s.open_line_above();
                CommandResult::ContentChanged
            }),
            Action::JoinLines => self.with_undo_group(|s| {
                for _ in 0..s.current_count.saturating_sub(1) {
                    s.join_lines();
                }
                if s.current_count > 1 {
                    s.join_lines();
                }
                CommandResult::ContentChanged
            }),
            Action::Indent => {
                if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
                    self.with_undo_group(|s| s.indent_selection())
                } else {
                    self.with_undo_group(|s| {
                        s.indent_n_lines(s.current_count);
                        CommandResult::ContentChanged
                    })
                }
            }
            Action::Dedent => {
                if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
                    self.with_undo_group(|s| s.dedent_selection())
                } else {
                    self.with_undo_group(|s| {
                        s.dedent_n_lines(s.current_count);
                        CommandResult::ContentChanged
                    })
                }
            }
            Action::IndentTs => {
                self.with_undo_group(|s| {
                    // Visual mode: indent selected lines
                    if matches!(s.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
                        let range = if let Some(window) = s.windows.active_window() {
                            if let Some(anchor) = window.selection_anchor {
                                let head = window.cursor.position;
                                Some((anchor.line.min(head.line), anchor.line.max(head.line)))
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        s.record_action(RepeatableAction::IndentTs { count: s.current_count }, s.current_count);

                        let result = s.format_ts_indent(range);
                        if let Some(window) = s.windows.active_window_mut() {
                            window.selection_anchor = None;
                        }
                        s.mode = Mode::Normal;
                        s.dirty.mark_all();
                        return match result {
                            Ok(()) => CommandResult::ContentChanged,
                            Err(e) => CommandResult::Error(e),
                        };
                    }

                    let range = if let Some((start, end)) = s.find_function_lines() {
                        Some((start, end))
                    } else {
                        let line = s.windows.active_window().map(|w| w.cursor.position.line).unwrap_or(0);
                        Some((line, line + s.current_count.saturating_sub(1)))
                    };

                    s.record_action(RepeatableAction::IndentTs { count: s.current_count }, s.current_count);

                    match s.format_ts_indent(range) {
                        Ok(()) => {
                            s.dirty.mark_all();
                            CommandResult::ContentChanged
                        }
                        Err(e) => CommandResult::Error(e),
                    }
                })
            }
            Action::IndentTsToFileEnd => {
                let line = self.windows.active_window().map(|w| w.cursor.position.line).unwrap_or(0);
                let last_line = self.current_buffer().map(|b| b.line_count().saturating_sub(1)).unwrap_or(0);

                match self.format_ts_indent(Some((line, last_line))) {
                    Ok(()) => {
                        self.dirty.mark_all();
                        CommandResult::ContentChanged
                    }
                    Err(e) => CommandResult::Error(e),
                }
            }
            Action::IndentSelection => {
                match self.mode {
                    Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                        // indent_selection manages its own undo group internally
                        self.indent_selection()
                    }
                    _ => self.with_undo_group(|s| {
                        s.indent_n_lines(s.current_count);
                        CommandResult::ContentChanged
                    }),
                }
            }
            Action::DedentSelection => match self.mode {
                Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                    // dedent_selection manages its own undo group internally
                    self.dedent_selection()
                }
                _ => self.with_undo_group(|s| {
                    s.dedent_n_lines(s.current_count);
                    CommandResult::ContentChanged
                }),
            },
            // ── Yank / Paste (count-aware) ──────────────
            Action::YankLine => self.yank_n_lines(self.current_count),
            Action::YankSelection => {
                // The yank methods already return CommandResult, just pass through.
                match self.mode {
                    Mode::Visual => self.yank_visual_selection(),
                    Mode::VisualLine => self.yank_visual_line_selection(),
                    Mode::VisualBlock => self.yank_block_selection(),
                    _ => self.yank_line(),
                }
            }
            Action::PasteAfter => {
                self.record_action(
                    RepeatableAction::Paste {
                        register: '"',
                        after_cursor: true,
                    },
                    self.current_count,
                );
                self.with_undo_group(|s| {
                    for _ in 0..s.current_count {
                        s.paste_after();
                    }
                    CommandResult::ContentChanged
                })
            }
            Action::PasteBefore => self.with_undo_group(|s| {
                for _ in 0..s.current_count {
                    s.paste_before();
                }
                CommandResult::ContentChanged
            }),

            // ── System clipboard ────────────────────────
            Action::YankToClipboard => self.yank_selection_to_clipboard(),
            Action::PasteFromClipboard => self.paste_from_clipboard(),
            Action::ClipboardPasteLine => self.clipboard_paste_line(),
            Action::ClipboardReplaceBuffer => self.clipboard_replace_buffer(),

            // ── Undo / Redo ─────────────────────────────
            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::UndoBreak => {
                if let Some(window) = self.windows.active_window() {
                    let buffer_id = window.buffer_id;
                    if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                        buffer.break_undo_group(window.cursor.position);
                        self.set_status("Undo break".to_string());
                    }
                }
                CommandResult::NoOp
            }

            // ── Search ──────────────────────────────────
            Action::SearchForward => self.enter_search_forward(),
            Action::SearchBackward => self.enter_search_backward(),
            Action::SearchNext => self.search_next(),
            Action::SearchPrev => self.search_prev(),
            Action::SearchWordForward => self.search_word_forward(),

            Action::FunctionList => {
                return self.show_function_list();
            }

            // ── File operations ─────────────────────────
            Action::Save => match self.save() {
                Ok(()) => CommandResult::Message("File saved.".to_string()),
                Err(e) => CommandResult::Error(e.to_string()),
            },
            Action::SaveFmt => {
                match self.format_current_buffer_async(true) {
                    Ok(()) => {
                        self.set_status("Formatting…".into());
                        CommandResult::ViewChanged
                    }
                    Err(e) => {
                        // Async formatter unavailable or failed immediately (e.g., no formatter for this language).
                        // Fall back to a regular save.

                        // Temporarily disable format_on_save so self.save() doesn't
                        // redundantly try the synchronous formatter and show duplicate errors.
                        let prev_fmt = self.config.format_on_save;
                        self.config.format_on_save = false;

                        let result = match self.save() {
                            Ok(()) => CommandResult::Message(format!("File saved. ({})", e)),
                            Err(save_err) => CommandResult::Error(save_err.to_string()),
                        };

                        self.config.format_on_save = prev_fmt;
                        result
                    }
                }
            }
            Action::FormatDocument => match self.format_current_buffer_async(false) {
                Ok(()) => {
                    self.set_status("Formatting…".into());
                    CommandResult::ViewChanged
                }
                Err(e) => CommandResult::Error(e),
            },
            Action::SaveAs(path) => match self.save_as(path) {
                Ok(()) => CommandResult::Message(format!("Saved as {:?}", path)),
                Err(e) => CommandResult::Error(e.to_string()),
            },
            Action::OpenMru => self.open_mru(),
            Action::OpenFile(path) => match self.open_file(path) {
                Ok(_) => {
                    self.restore_cursor_position();
                    CommandResult::ViewChanged
                }
                Err(e) => CommandResult::Error(e.to_string()),
            },
            Action::NewFile => self.new_file(),
            Action::FindFile => self.find_file(),

            // ── Window management ───────────────────────
            Action::SplitHorizontal => self.split_horizontal(),
            Action::SplitVertical => self.split_vertical(),
            Action::NextWindow => self.next_window(),
            Action::PrevWindow => self.prev_window(),
            Action::CloseWindow => self.close_window(),

            // ── Command line ────────────────────────────
            Action::ExecuteCommand => self.execute_command(),
            Action::CommandHistoryUp => self.command_history_up(),
            Action::CommandHistoryDown => self.command_history_down(),

            // ── Completion ───────────────────────────────
            Action::TriggerCompletion => self.trigger_completion(),
            Action::SelectNextCompletion => self.select_next_completion(),
            Action::SelectPrevCompletion => self.select_prev_completion(),
            Action::ConfirmCompletion => {
                // Priority 1: Accept ghost text if visible
                if self.ghost_text.is_visible() {
                    self.accept_ghost_text();
                    return CommandResult::ContentChanged; // ← early return
                }

                // Priority 2: Confirm completion popup
                if self.completion.active {
                    self.confirm_completion()
                } else {
                    // Priority 3: Nothing active → insert a tab character
                    self.ensure_undo_group();
                    self.insert_tab_at_cursor();
                    CommandResult::ContentChanged
                }
            }
            Action::CancelCompletion => {
                self.completion.cancel();
                self.dirty.mark_all();
                CommandResult::NoOp
            }

            // ── Git ──────────────────────────────────────
            Action::GotoDefinition => {
                if self.lsp.connected {
                    self.push_jump_position();
                    self.request_lsp_goto_definition();
                } else {
                    self.goto_definition();
                }
                CommandResult::ViewChanged
            }

            Action::GotoDeclaration | Action::FindReferences | Action::HoverInfo | Action::CodeAction | Action::GitBlame => {
                self.set_status(format!("{} — not yet implemented", action.label()));
                CommandResult::NoOp
            }
            Action::GitStatus => self.git_status_open(""),
            Action::GitDiff => self.git_diff_open(""),
            Action::GitLog => self.git_log_open("", ""),
            Action::GitNextHunk => self.git_next_hunk(),
            Action::GitPrevHunk => self.git_prev_hunk(),
            Action::GitRevertHunk => self.git_revert_hunk(),
            Action::GitCommit => self.git_commit_generate(),
            Action::GitGutterToggle => {
                self.git.gutter_enabled = !self.git.gutter_enabled;
                if !self.git.gutter_enabled {
                    self.invalidate_git_gutter();
                }
                self.dirty.mark_all();
                let state = if self.git.gutter_enabled { "on" } else { "off" };
                CommandResult::Message(format!("Git gutter: {}", state))
            }
            Action::GitStageHunk | Action::GitUnstageHunk => {
                self.set_status(format!("{} — not yet implemented", action.label()));
                CommandResult::NoOp
            }
            Action::ShowShortcuts => {
                self.show_shortcuts();
                CommandResult::ViewChanged
            }
            Action::Guide => {
                crate::command_registry::guide_handler(self, "");
                CommandResult::ViewChanged
            }
            Action::EnterJumpMode => {
                crate::ed::motion::enter_jump_mode(self);
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Action::RunBuild => self.run_build(),
            Action::SwitchToBuild => self.switch_to_build_buffer(),
            Action::RipgrepUnderCursor => self.ripgrep_under_cursor(),
            Action::RipgrepInput => {
                self.command_prompt.clear();
                self.set_status("Ripgrep search: ".to_string());
                self.enter_mode(Mode::Command)
                // Note: We'll need a separate ripgrep input mode or reuse command mode
                // For now, redirect to command mode with :rg prefix
            }
            Action::ShowHunkDiff => self.git_show_hunk_diff(),
            Action::RipgrepGotoResult => self.ripgrep_goto_result(),
            Action::RipgrepClose => self.ripgrep_close_buffer(),
            Action::RipgrepLast => self.ripgrep_last(),
            Action::RipgrepNextResult => {
                self.record_action(RepeatableAction::RipgrepNextResult, self.current_count);
                self.ripgrep_next_result()
            }
            Action::RipgrepPrevResult => {
                self.record_action(RepeatableAction::RipgrepPrevResult, self.current_count);
                self.ripgrep_prev_result()
            }
            Action::ListBuffers => self.list_buffers(),
            Action::NextBuffer => self.next_buffer(),
            Action::PrevBuffer => self.prev_buffer(),

            Action::ToggleComment => self.toggle_comment_lines(self.current_count),
            Action::ToggleCommentAndMoveDown => {
                self.record_action(RepeatableAction::ToggleComment, self.current_count);
                let result = self.toggle_comment_lines(self.current_count);
                if !matches!(result, CommandResult::Error(_)) {
                    // Move cursor down by current_count (or just 1 if no count)
                    let move_count = if self.current_count > 1 { self.current_count } else { 1 };
                    for _ in 0..move_count {
                        self.move_cursor_down();
                    }
                }
                result
            }

            // ── Misc ────────────────────────────────────
            Action::Quit => {
                self.flush_lsp_changes();
                let _ = self.lsp.tx.send(crate::lsp::LspMessage::Shutdown);
                self.save_history();
                self.save_all_positions();
                let has_dirty = self.buffers.iter().any(|b| b.dirty);
                if has_dirty {
                    CommandResult::Error("Unsaved changes! Use :q! to force quit.".to_string())
                } else {
                    self.should_quit = true;
                    CommandResult::Quit
                }
            }
            Action::ForceQuit => {
                self.flush_lsp_changes();
                let _ = self.lsp.tx.send(crate::lsp::LspMessage::Shutdown);
                self.save_history();
                self.save_all_positions();
                let has_dirty = self.buffers.iter().any(|b| b.dirty);

                if has_dirty && !self.force_quit_pending {
                    self.force_quit_pending = true;
                    CommandResult::Message("Save changes? (y/n/c)".to_string())
                } else {
                    // If no dirty buffers, or if somehow called while pending, just quit
                    self.should_quit = true;
                    CommandResult::Quit
                }
            }
            Action::ShowHelp => self.show_hints(),
            Action::ToggleLineNumbers => {
                self.config.line_numbers = !self.config.line_numbers;
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Action::TagJump => {
                crate::ed::tag::tag_under_cursor(self);
                CommandResult::ViewChanged
            }
            Action::TagNext => crate::ed::tag::tag_next(self),
            Action::TagPrev => crate::ed::tag::tag_prev(self),
            Action::TagPop => crate::ed::tag::tag_pop(self),
            Action::GenerateTags => {
                let file_path = self.current_buffer().and_then(|b| b.file_path.clone());
                if let Some(ref path) = file_path {
                    self.search.tag_manager.init(path);
                }
                match self.search.tag_manager.generate_tags() {
                    Ok(msg) => CommandResult::Message(msg),
                    Err(err) => CommandResult::Error(err),
                }
            }

            Action::DeleteTill => {
                self.inline_delete_pending = Some('t');
                CommandResult::NoOp
            }
            Action::DeleteFind => {
                self.inline_delete_pending = Some('f');
                CommandResult::NoOp
            }

            //-- process_action Action::RipgrepLast (anchor dont remove) --//
            Action::ToggleWhitespace => {
                self.config.show_whitespace = !self.config.show_whitespace;
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Action::RepeatLastAction => self.repeat_last_action(),
            Action::None => CommandResult::NoOp,
            _ => {
                if self.mode != Mode::Command && self.mode != Mode::LlmPrompt && !self.search.input_active {
                    self.clear_messages();
                }
                // Cancel force quit confirmation if the user performs another action
                if self.force_quit_pending {
                    self.force_quit_pending = false;
                }
                CommandResult::NoOp
            }
        }
    }

    pub fn tick(&mut self) {
        // ── Flush debounced LSP changes ──
        if self.lsp.change_pending {
            if let Some(deadline) = self.lsp.change_deadline {
                if Instant::now() >= deadline {
                    self.flush_lsp_changes();
                }
            }
        }

        // Auto-start Codeium on first tick if configured
        if self.auto_start_codeium {
            self.auto_start_codeium = false;
            if self.config.codeium.enabled {
                if let Err(_e) = self.start_codeium() {}
            }
        }

        self.tick_codeium_startup();
        self.tick_git();
        self.update_which_key_debounce();
        self.poll_llm_responses();
        self.tick_build();
        self.tick_git_commit();

        // ── Ghost text: log current state before polling ──
        if !self.ghost_text.is_pending() {
            self.ghost_text.is_visible();
        }

        self.poll_codeium_ghost();
        self.validate_ghost_text();
        self.poll_app_messages();
        self.update_function_name_cache();

        // ── Deferred syntax reparse (only when popup is NOT active) ──
        let popup_active = self.completion.active;

        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if buffer.tree_dirty && !popup_active {
                    buffer.reparse_if_dirty();
                    self.dirty.windows = true;
                }
            }
        }
    }
} // end of imp editor
