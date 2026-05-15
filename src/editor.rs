// src/editor.rs
//! Editor — the central coordinator for the text editor.
//!
//! Holds buffers, windows, the keybind manager, and the main state machine
//! that processes events and executes actions.

use std::time::Instant;

use crate::action::Action;
use crate::buffer::BufferKind;
use crate::buffer::{Buffer, BufferCollection, BufferId, CursorPosition};
use crate::command::CommandRegistry;
use crate::completion::CompletionEngine;
use crate::config::{Config, HistoryData};
use crate::dirty::DirtyState;
use crate::git::DiffHunk;
use crate::keybind::{
    apply_custom_keybindings, default_command_keymap, default_insert_keymap, default_normal_keymap,
    default_visual_keymap, KeyBindManager, KeyBindResult,
};
use crate::mru::MruManager;
use crate::overlay::OverlayTracker;
use crate::popup::{FilePicker, HelpPopup, MruPopup};
use crate::terminal::{Key, TerminalEvent};
use crate::window::WindowManager;
use tokio::sync::mpsc;
// Import the block insert state from visual module (only definition)
use crate::codeium::CodeiumManager;
use crate::ed::build::BuildExt;
use crate::ed::visual::BlockInsertState;
use crate::ed::GhostTextExt;
use crate::ed::GotoDefExt;
use crate::ed::LastAction;
use crate::ed::LspExt;
use crate::ed::MarksExt;
use crate::ed::RepeatableAction;
use crate::ed::SearchExt;
use crate::ed::{
    BufferOpsExt, CommandExt, CompletionExt, EditingExt, FileOpsExt, GitExt, LlmExt, MovementExt,
    RepeatExt, RipgrepExt, VisualExt, WindowExt,
};
use crate::ed::{GitDiffExt, GitLogExt, GitStatusExt};
use crate::ed::{TextObjectExt, TextObjectKind, TextObjectOperator};
use crate::ghost_text::GhostTextManager;
use crate::llm::{LlmBuffer, LlmPreset};
use crate::popup::Scrollable;
use crate::prompt::MiniInputPrompt;
use crate::prompt::PromptAction;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;

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
        let width = (content_width + 4).min(80) as u16;
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
    /// Current which-key hints (for rendering).
    pub which_key_hints: Vec<(String, String)>,
    /// Debounce timeout for which-key popup (milliseconds).
    pub which_key_debounce_timeout: u64,
    /// Timer for which-key debouncing.
    pub which_key_debounce_timer: Option<Instant>,
    /// Whether we're waiting for a register in command mode.
    pub cmd_waiting_register: bool,

    // ==================== Search & Navigation ====================
    /// Current search direction (forward/backward).
    pub search_direction: Option<SearchDirection>,
    /// Whether search input is active.
    pub search_input_active: bool,
    /// Current search pattern.
    pub search_pattern: Option<String>,
    /// Current search match positions.
    pub search_matches: Vec<CursorPosition>,
    /// Index of current match in search results.
    pub current_search_match: usize,
    /// Whether search matches need recomputation.
    pub search_matches_dirty: bool,
    /// Buffer ID that the current search belongs to.
    pub search_buffer_id: Option<crate::buffer::BufferId>,
    /// Command line history prefix for filtering.
    pub command_line_history_prefix: Option<String>,
    /// Search line history prefix for filtering.
    pub search_line_history_prefix: Option<String>,
    /// Search command history.
    pub search_history: Vec<String>,
    /// Current index in search history.
    pub search_history_idx: usize,
    /// Named marks (a-z) → (buffer_id, cursor_position).
    pub marks:
        std::collections::HashMap<char, (crate::buffer::BufferId, crate::buffer::CursorPosition)>,
    /// Position before the last gd / cross-buffer jump (for `` jump-back).
    pub last_jump_mark: Option<(crate::buffer::BufferId, crate::buffer::CursorPosition)>,
    /// Whether we're waiting for a mark name after pressing `m`.
    pub mark_pending: bool,
    /// Whether we're waiting for a mark name after pressing `` ` ``.
    pub goto_mark_pending: bool,
    /// Session position persistence map.
    pub position_map: crate::session::PositionMap,
    /// Active substitute‑confirm state (set by :s/pat/repl/gc).
    pub substitute_confirm: Option<SubstituteConfirmState>,
    pub replace_count: usize,

    // ==================== Tags ====================
    /// Ctags manager for go-to-definition via tags file.
    pub tag_manager: crate::tags::TagManager,
    /// Current tag search results (for cycling with :tnext/:tprev).
    pub tag_results: Vec<crate::tags::TagEntry>,

    // ==================== Clipboard & Registers ====================
    /// Yank (copy) register.
    pub yank_register: String,
    /// Named registers (a–z) for yank/paste via "xp.
    pub named_registers: std::collections::HashMap<char, String>,

    // ==================== Popups & Overlays ====================
    /// Optional float popup (e.g. hunk preview). When `Some`, input is
    /// intercepted: ESC dismisses, other keys dismiss + execute.
    pub float_popup: Option<FloatPopup>,
    /// Interactive help popup (if active).
    pub help_popup: Option<HelpPopup>,
    /// Buffer list popup (if active). Intercepts input like help_popup.
    pub buffer_list_popup: Option<crate::popup::BufferListPopup>,
    /// File picker popup (if active). Intercepts input like help_popup.
    pub file_picker: Option<FilePicker>,
    /// Keymap popup (if active). Intercepts input like help_popup.
    pub keymap_popup: Option<crate::popup::KeymapPopup>,
    /// MRU popup (if active). Intercepts input like buffer_list_popup.
    pub mru_popup: Option<MruPopup>,
    /// Bottom-up register popup (triggered by :reg).
    pub register_popup: Option<Vec<String>>,
    /// Dynamic title for the register popup (e.g. "Registers" or "LLM Translate → 中文").
    pub register_popup_title: String,
    /// Overlay state tracker for rendering optimization.
    pub overlay: OverlayTracker,
    /// Format info popup (full formatter stderr output). Dismissed with ESC/q/Enter.
    pub fmt_info_popup: Option<Vec<String>>,
    /// Dynamic title for the format info popup.
    pub fmt_info_popup_title: String,

    // ==================== Git Integration ====================
    /// Automatic diff popup shown when cursor is near a git hunk.
    /// Non‑interactive — does not intercept input.
    pub diff_popup: Option<crate::ed::git::DiffPopup>,
    /// Whether the diff popup is active (diff mode).
    pub diff_mode_active: bool,
    /// Cached git provider for the current file's repository.
    pub git_provider: Option<crate::git::GitProvider>,
    /// Whether the git gutter sign column is enabled.
    pub git_gutter_enabled: bool,
    /// Cached diff hunks for the active buffer (for hunk revert).
    pub cached_diff_hunks: Vec<DiffHunk>,
    /// Timestamp of the last content change that invalidated the git gutter.
    pub git_gutter_dirty_since: Option<Instant>,
    /// Debounce interval (milliseconds) for git gutter recomputation after edits.
    pub git_gutter_debounce_ms: u64,
    /// Git log commit count (persisted for refresh).
    pub git_log_count: usize,
    /// Git log grep pattern (persisted for refresh).
    pub git_log_grep: String,
    // ==================== LSP Integration ====================
    /// LSP message sender (editor → async LSP task).
    pub lsp_tx: mpsc::UnboundedSender<crate::lsp::LspMessage>,
    /// Whether an LSP completion request is in flight.
    pub lsp_completion_pending: bool,
    /// Whether the LSP server has connected and initialized.
    pub lsp_connected: bool,
    /// Cached LSP diagnostics per URI.
    pub lsp_diagnostics: std::collections::HashMap<String, Vec<crate::lsp::Diagnostic>>,
    /// Current signature help state (for info bar display).
    pub lsp_signature_help: Option<crate::lsp::SignatureHelpState>,
    /// Inlay hints per URI.
    pub lsp_inlay_hints: std::collections::HashMap<String, Vec<crate::lsp::InlayHint>>,
    /// Whether an LSP didChange notification is pending (debounced).
    pub lsp_change_pending: bool,
    /// Deadline after which to send the pending LSP didChange.
    pub lsp_change_deadline: Option<Instant>,
    /// LSP change debounce interval in milliseconds.
    pub lsp_change_debounce_ms: u64,
    /// LSP document version counter.
    pub lsp_doc_version: i32,
    /// Whether formatting is pending.
    pub formatting_pending: bool,
    /// Buffer ID for pending formatting operation.
    pub formatting_buffer_id: Option<BufferId>,

    // ==================== AI / Codeium ====================
    /// Ghost text manager (inline AI suggestions).
    pub ghost_text: GhostTextManager,
    /// Codeium server manager.
    pub codeium: CodeiumManager,
    /// Whether we're waiting for the user to paste a Codeium auth token.
    pub codeium_auth_pending: bool,
    /// Set to true on first tick to auto-start Codeium if configured.
    auto_start_codeium: bool,

    // ==================== LLM Features ====================
    /// The LLM conversation buffer state (attached to BufferKind::Llm)
    pub llm_buffer: LlmBuffer,
    /// Whether to prefix the user's LLM prompt with "##TODO" (set by `'` in visual mode).
    pub llm_todo_prefix: bool,
    /// Handle to the LLM buffer ID (if created)
    pub llm_buffer_id: Option<BufferId>,
    /// Tokio runtime for async LLM operations.
    pub llm_runtime: tokio::runtime::Runtime,
    /// Channel sender for LLM async tasks to send responses back.
    pub llm_response_tx: mpsc::UnboundedSender<Result<String, String>>,
    /// Channel receiver polled in tick() for completed LLM responses.
    pub llm_response_rx: mpsc::UnboundedReceiver<Result<String, String>>,
    /// Handle to the running LLM async task (for abort on cancel).
    pub llm_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// The preset to use if the user triggers a quick LLM action
    pub llm_active_preset: Option<crate::llm::LlmPreset>,
    /// Context text (e.g., visual selection) for the active LLM prompt
    pub llm_active_context: Option<String>,
    /// Buffer ID the user was editing before opening the LLM split layout.
    pub llm_origin_buffer_id: Option<crate::buffer::BufferId>,
    /// Instead of being appended to the LLM conversation buffer, show in info bar.
    pub llm_infobar_response: bool,
    /// Accumulator for infobar-bound LLM responses (streaming chunks).
    pub llm_infobar_accumulator: String,
    pub llm_single_shot: bool,

    // ==================== Completion ====================
    /// Completion engine (word-based, buffer-word, future LSP).
    pub completion: CompletionEngine,
    /// Command-line completion engine.
    pub command_completion: CompletionEngine,
    /// Timestamp of the last insert-mode edit, for undo group timeout.
    pub(crate) last_edit_time: Instant,
    /// Timeout (milliseconds) after which a new insert-mode keystroke starts a new undo group.
    pub(crate) undo_break_timeout_ms: u64,

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
    auto_start_lsp: bool,

    // ==================== Command System ====================
    /// Dynamic command registry for `:` commands.
    pub command_registry: CommandRegistry,
    /// Command line prompt state.
    pub command_prompt: MiniInputPrompt,
    /// Search line prompt state.
    pub search_prompt: MiniInputPrompt,
    /// LLM prompt state.
    pub llm_prompt: MiniInputPrompt,

    // ==================== Repeat & History ====================
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

    /// Function list popup (if active).
    pub function_list_popup: Option<crate::popup::FunctionListPopup>,

    // ==================== MRU ====================
    /// Most-recently-used file manager.
    pub mru: MruManager,

    // ==================== Build ====================
    /// Parsed diagnostics from the last `:build` run.
    pub build_diagnostics: Vec<crate::ed::build::BuildDiagnostic>,
    /// Channel sender for background build thread.
    pub build_response_tx: std::sync::mpsc::Sender<crate::ed::build::BuildResult>,
    /// Channel receiver polled in tick() for completed build results.
    pub build_response_rx: std::sync::mpsc::Receiver<crate::ed::build::BuildResult>,
    /// Whether a build is currently in progress.
    pub build_in_progress: bool,
    /// Timestamp when the current build started.
    pub build_start_time: Option<std::time::Instant>,
    /// Current frame index for the build spinner animation.
    pub build_spinner_idx: usize,

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

    // ==================== Message Passing ====================
    /// App message receiver (async tasks → editor). Polled in tick().
    pub app_rx: crate::msgbox::AppReceiver,
    /// App message sender (editor → async tasks).
    pub app_tx: crate::msgbox::AppSender,
}

impl Editor {
    /// Send a message to the LSP task, with debug logging.
    fn lsp_send(&mut self, msg: crate::lsp::LspMessage) {
        if self.lsp_tx.is_closed() {
            self.lsp_connected = false;
            return;
        }
        match self.lsp_tx.send(msg) {
            Ok(()) => {}
            Err(_e) => {
                self.lsp_connected = false;
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
        let mut keybinds = KeyBindManager::new();
        keybinds.register_keymap("normal", default_normal_keymap());
        keybinds.register_keymap("insert", default_insert_keymap());
        keybinds.register_keymap("visual", default_visual_keymap());
        keybinds.register_keymap("command", default_command_keymap());

        let keybindings_ref = config.keybindings.clone();
        // Deserialize the raw TOML map into our structured KeyBindingsConfig
        let keybindings_config: crate::keybind::KeyBindingsConfig =
            toml::Value::Table(keybindings_ref.clone())
                .try_into()
                .unwrap_or_default();
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
        let llm_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for LLM");

        // Now create LspManager and spawn it on the runtime
        let lsp_tx = {
            let tx_for_lsp = app_tx.clone();
            let mut lsp_manager = crate::lsp::LspManager::new(tx_for_lsp);
            let sender = lsp_manager.get_sender();

            llm_runtime.spawn(async move {
                lsp_manager.run().await;
            });

            sender
        };

        let (llm_response_tx, llm_response_rx) =
            tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();

        let command_history_len = history_data.command.len();
        let search_history_len = history_data.search.len();

        let mut command_prompt = MiniInputPrompt::new();
        command_prompt.history = history_data.command.clone();
        command_prompt.history_index = command_history_len;

        let mut search_prompt = MiniInputPrompt::new();
        search_prompt.history = history_data.search.clone();
        search_prompt.history_index = search_history_len;
        let (build_tx, build_rx) = std::sync::mpsc::channel();

        Editor {
            // Core Components
            buffers,
            windows,
            mode: Mode::Normal,
            config,
            active_buffer_idx: 0,
            keybinds,

            // Input & Keybindings
            pending_operator: None,
            pending_motion: None,
            pending_count: String::new(),
            current_count: 1,
            pending_register: None,  // for "a
            register_pending: false, // waiting for a key after pressing
            insert_register_pending: false,
            pending: PendingState::default(),
            which_key_hints: Vec::new(),
            which_key_debounce_timeout: 200,
            which_key_debounce_timer: None,
            cmd_waiting_register: false,

            // Search & Navigation
            search_direction: None,
            search_input_active: false,
            search_pattern: None,
            search_matches: Vec::new(),
            current_search_match: 0,
            search_matches_dirty: false,
            search_buffer_id: None,
            command_line_history_prefix: None,
            search_line_history_prefix: None,
            search_history: history_data.search.clone(),
            search_history_idx: search_history_len,
            substitute_confirm: None,
            replace_count: 1,

            marks: std::collections::HashMap::new(),
            last_jump_mark: None,
            mark_pending: false,
            goto_mark_pending: false,
            position_map: crate::session::PositionMap::load(),

            tag_manager: crate::tags::TagManager::new(),
            tag_results: Vec::new(),
            // Clipboard & Registers
            yank_register: String::new(),
            named_registers: std::collections::HashMap::new(),

            // Popups & Overlays
            float_popup: None,
            help_popup: None,
            buffer_list_popup: None,
            file_picker: None,
            keymap_popup: None,
            mru_popup: None,
            register_popup: None,
            register_popup_title: "Registers".to_string(),
            overlay: OverlayTracker::default(),

            // Git Integration
            diff_popup: None,
            diff_mode_active: false,
            git_provider: None,
            git_gutter_enabled: true,
            cached_diff_hunks: Vec::new(),
            git_gutter_dirty_since: None,
            git_gutter_debounce_ms: 500,
            git_log_count: 0,
            git_log_grep: String::new(),

            // LSP Integration
            lsp_tx,
            lsp_completion_pending: false,
            lsp_connected: false,
            lsp_diagnostics: std::collections::HashMap::new(),
            lsp_signature_help: None,
            lsp_inlay_hints: std::collections::HashMap::new(),
            lsp_change_pending: false,
            lsp_change_deadline: None,
            lsp_change_debounce_ms: 20,
            lsp_doc_version: 0,
            formatting_pending: false,
            formatting_buffer_id: None,

            // AI / Codeium
            ghost_text: GhostTextManager::new(),
            codeium: CodeiumManager::new(codeium_debounce_ms),
            codeium_auth_pending: false,
            auto_start_codeium: true,

            // LLM Features
            llm_buffer: LlmBuffer::new(),
            llm_todo_prefix: false,
            llm_buffer_id: None,
            llm_runtime,
            llm_response_tx,
            llm_response_rx,
            llm_task_handle: None,
            llm_active_preset: None,
            llm_active_context: None,
            llm_origin_buffer_id: None,
            llm_infobar_response: false,
            llm_infobar_accumulator: String::new(),
            llm_single_shot: false,

            // Completion
            completion: CompletionEngine::new(2),
            command_completion,
            last_edit_time: Instant::now(),
            undo_break_timeout_ms: 2000,

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
            auto_start_lsp: true,

            // Command System
            command_registry,
            command_prompt,
            search_prompt,
            llm_prompt: MiniInputPrompt::new(),

            // Repeat & History
            last_action: LastAction::default(),
            repeat_pending: false,
            last_rg_pattern: None,
            last_rg_root_dir: None,
            last_rg_output: None,
            quickfix_results: Vec::new(),
            quickfix_index: 0,

            function_list_popup: None,
            fmt_info_popup: None,
            fmt_info_popup_title: "Format Info".to_string(),

            visual_selection_range: None,
            build_diagnostics: Vec::new(),
            build_response_tx: build_tx,
            build_response_rx: build_rx,
            build_in_progress: false,
            build_start_time: None,
            build_spinner_idx: 0,
            // MRU
            mru: {
                let mut m = MruManager::new(
                    100,
                    Config::config_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join("mru.json"),
                );
                m.load();
                m.prune_missing();
                m
            },

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
            .ok_or_else(|| {
                "Codeium: no API key. Run :codeium-auth or set CODEIUM_API_KEY env var".to_string()
            })?;

        self.codeium
            .start(api_key, &self.llm_runtime)
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
        self.windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
    }

    /// Ensure the cursor is visible in the active window's viewport.
    pub fn ensure_cursor_visible_all(&mut self) {
        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        let Some(buffer_id) = buffer_id else { return };
        let max_line = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
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
                self.search_matches_dirty = true;
                self.dirty.mark_insert();
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
        // ── Float popup interception ──
        if self.float_popup.is_some() {
            if key == Key::Escape {
                self.float_popup = None;
                self.overlay.float = None;
                self.dirty.mark_all();
                return CommandResult::NoOp;
            }
            let old_rect = self.overlay.float;
            self.float_popup = None;
            self.overlay.float = None;
            if let Some(rect) = old_rect {
                self.dirty.mark_popup_closed(rect);
            }
            self.dirty.cursor = true;
            // Fall through to process the key normally
        }

        // ── Register popup interception ──
        if self.register_popup.is_some() {
            if key == Key::Escape || key == Key::Char('q') || key == Key::Enter {
                self.register_popup = None;
                self.register_popup_title = "Registers".to_string();
                self.dirty.mark_all();
                return CommandResult::NoOp;
            }
            // Any other key dismisses the popup and falls through to normal processing
            self.register_popup = None;
            self.register_popup_title = "Registers".to_string();
            self.dirty.mark_all();
            // Fall through to process the key normally
        }

        // ── Format info popup interception ──
        if self.fmt_info_popup.is_some() {
            match key {
                Key::Escape | Key::Char('q') | Key::Enter => {
                    self.fmt_info_popup = None;
                    self.fmt_info_popup_title = "Format Info".to_string();
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }
                _ => {
                    // Any other key dismisses the popup and falls through
                    self.fmt_info_popup = None;
                    self.fmt_info_popup_title = "Format Info".to_string();
                    self.dirty.mark_all();
                    // Fall through to process the key normally
                }
            }
        }

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
        if self.substitute_confirm.is_some() {
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

        // ── SEARCH INPUT MODE ──
        if self.search_input_active {
            // ── Prefix-smart history navigation ──
            if key == Key::Up {
                let _ = self.search_history_up();
                // Live-update the search matches based on the new history entry
                let query = self.search_prompt.text();
                if query.is_empty() {
                    self.clear_messages();
                    self.search_matches.clear();
                } else {
                    let matches = self.find_all_matches(query);
                    self.search_matches = matches;
                    if self.search_matches.is_empty() {
                        self.set_infobar_message("No match".to_string());
                    } else {
                        self.set_status(format!("{} matches", self.search_matches.len()));
                    }
                }
                self.dirty.status_cmdline = true;
                self.dirty.cursor = true;
                self.dirty.windows = true;
                return CommandResult::NoOp;
            }
            if key == Key::Down {
                let _ = self.search_history_down();
                // Live-update the search matches
                let query = self.search_prompt.text();
                if query.is_empty() {
                    self.clear_messages();
                    self.search_matches.clear();
                } else {
                    let matches = self.find_all_matches(query);
                    self.search_matches = matches;
                    if self.search_matches.is_empty() {
                        self.set_infobar_message("No match".to_string());
                    } else {
                        self.set_status(format!("{} matches", self.search_matches.len()));
                    }
                }
                self.dirty.status_cmdline = true;
                self.dirty.cursor = true;
                self.dirty.windows = true;
                return CommandResult::NoOp;
            }

            return match self.search_prompt.handle_key(&key) {
                PromptAction::Changed => {
                    let query = self.search_prompt.text();
                    if query.is_empty() {
                        self.clear_messages();
                        self.search_matches.clear();
                    } else {
                        let matches = self.find_all_matches(query);
                        self.search_matches = matches;
                        if self.search_matches.is_empty() {
                            self.set_infobar_message("No match".to_string());
                        } else {
                            self.set_status(format!("{} matches", self.search_matches.len()));
                        }
                    }
                    self.dirty.status_cmdline = true;
                    self.dirty.cursor = true;
                    self.dirty.windows = true;
                    CommandResult::NoOp
                }
                PromptAction::Submit => {
                    let query = self.search_prompt.text().to_string();
                    self.search_prompt.push_history(query);
                    return self.execute_search();
                }
                PromptAction::Cancel => {
                    self.search_prompt.clear();
                    self.dirty.mark_all();
                    return self.cancel_search();
                }
                PromptAction::None => {
                    // If Backspace is pressed on empty search, cancel search
                    if key == Key::Backspace && self.search_prompt.is_empty() {
                        self.search_prompt.clear();
                        self.dirty.mark_all();
                        return self.cancel_search();
                    }
                    CommandResult::NoOp
                }
            };
        }

        // ── LLM Input scratchpad special keys ──
        if self.mode == Mode::Normal {
            if let Some(window) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                    if buffer.kind == BufferKind::LlmInput {
                        match key {
                            Key::Enter => {
                                return self.llm_send_input_buffer();
                            }
                            Key::Char('q') => {
                                return self.llm_close_split_session();
                            }
                            _ => {} // Fall through to normal Vim keybinds (j, k, i, o, dd, etc.)
                        }
                    }
                }
            }
        }

        // ── Popups take precedence over special buffer key bindings ──
        let popup_active = self.buffer_list_popup.is_some()
            || self.mru_popup.is_some()
            || self.file_picker.is_some()
            || self.function_list_popup.is_some()
            || self.keymap_popup.is_some()
            || self.help_popup.is_some();

        // ── Ripgrep buffer special keys ── RG
        if self.mode == Mode::Normal && !popup_active {
            if let Some(window) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                    if buffer.kind == BufferKind::Ripgrep {
                        match key {
                            Key::Enter => {
                                self.dirty.mark_all();
                                return self.ripgrep_goto_result();
                            }
                            Key::Char('q') | Key::Char('Q') => {
                                return self.ripgrep_close_buffer();
                            }
                            Key::Escape => {
                                return self.ripgrep_close_buffer();
                            }
                            _ => {
                                // Block most editing keys in ripgrep buffer
                                // Allow navigation (h/j/k/l, Ctrl-u/d, etc.) via keybinds
                            }
                        }
                    }
                }
            }
        }

        // ── Git status buffer special keys ── GIT STATUS
        if self.mode == Mode::Normal && !popup_active {
            if let Some(window) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                    if buffer.kind == BufferKind::GitStatus {
                        match key {
                            Key::Char('s') | Key::Char('S') => {
                                self.dirty.mark_all();
                                return self.git_status_toggle_stage();
                            }
                            Key::Char('a') | Key::Char('A') => {
                                self.dirty.mark_all();
                                return self.git_status_add_file();
                            }
                            Key::Enter => {
                                self.dirty.mark_all();
                                return self.git_status_goto_file();
                            }
                            Key::Char('r') | Key::Char('R') => {
                                self.dirty.mark_all();
                                return self.git_status_refresh();
                            }
                            Key::Char('q') | Key::Char('Q') => {
                                return self.git_status_close();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // ── Git diff buffer special keys ── GIT DIFF
        if self.mode == Mode::Normal && !popup_active {
            if let Some(window) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                    if buffer.kind == BufferKind::GitDiff {
                        match key {
                            Key::Enter => {
                                self.dirty.mark_all();
                                return self.git_diff_goto_file();
                            }
                            Key::Char('n') => {
                                self.dirty.mark_all();
                                return self.git_diff_next_hunk();
                            }
                            Key::Char('N') => {
                                self.dirty.mark_all();
                                return self.git_diff_prev_hunk();
                            }
                            Key::Char('r') | Key::Char('R') => {
                                self.dirty.mark_all();
                                return self.git_diff_refresh();
                            }
                            Key::Char('q') | Key::Char('Q') => {
                                return self.git_diff_close();
                            }
                            _ => {} // fall through to normal navigation keybinds
                        }
                    }
                }
            }
        }

        // ── Build buffer special keys ── BUILD
        if self.mode == Mode::Normal && !popup_active {
            if let Some(window) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                    if buffer.kind == BufferKind::Build {
                        match key {
                            Key::Enter => {
                                self.dirty.mark_all();
                                return self.build_goto_error();
                            }
                            Key::Char('y') => {
                                // Copy all build errors/warnings to the system clipboard
                                if self.build_diagnostics.is_empty() {
                                    return CommandResult::Message(
                                        "No errors/warnings to yank".to_string(),
                                    );
                                }

                                let mut yank_text = String::new();
                                for diag in &self.build_diagnostics {
                                    let severity_str = match diag.severity {
                                        crate::ed::build::BuildSeverity::Error => "error",
                                        crate::ed::build::BuildSeverity::Warning => "warning",
                                        crate::ed::build::BuildSeverity::Note => "note",
                                    };
                                    yank_text.push_str(&format!(
                                        "{}:{}:{}: {}: {}\n",
                                        diag.file_path.display(),
                                        diag.line_number,
                                        diag.column,
                                        severity_str,
                                        diag.message
                                    ));
                                }

                                self.yank_register = yank_text.clone();

                                return match crate::clipboard::set_text(&yank_text) {
                                    Ok(()) => CommandResult::Message(format!(
                                        "Yanked {} diagnostic(s) to system clipboard",
                                        self.build_diagnostics.len()
                                    )),
                                    Err(e) => {
                                        CommandResult::Error(format!("Clipboard error: {}", e))
                                    }
                                };
                            }
                            Key::Char('n') => {
                                return self.build_next_error();
                            }
                            Key::Char('N') => {
                                return self.build_prev_error();
                            }
                            Key::Char('q') | Key::Char('Q') => {
                                return self.build_close();
                            }
                            _ => {
                                // Fall through to normal navigation keybinds
                                // (j, k, Ctrl-u/d, G, gg, etc.)
                            }
                        }
                    }
                }
            }
        }

        // ── Git log buffer special keys ── GIT LOG
        if self.mode == Mode::Normal && !popup_active {
            if let Some(window) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                    if buffer.kind == BufferKind::GitLog {
                        match key {
                            Key::Enter => {
                                self.dirty.mark_all();
                                return self.git_log_goto_file();
                            }
                            Key::Char('d') | Key::Char('D') => {
                                self.dirty.mark_all();
                                return self.git_log_show_diff();
                            }
                            Key::Char('s') | Key::Char('S') => {
                                self.dirty.mark_all();
                                return self.git_log_save_file();
                            }
                            Key::Char('r') | Key::Char('R') => {
                                self.dirty.mark_all();
                                return self.git_log_refresh();
                            }
                            Key::Char('q') | Key::Char('Q') => {
                                return self.git_log_close();
                            }
                            _ => {} // fall through to normal navigation keybinds
                        }
                    }
                }
            }
        }

        // ── Buffer list popup navigation ── BUIFFER
        if let Some(popup) = &mut self.buffer_list_popup {
            match key {
                Key::Escape => {
                    self.buffer_list_popup = None;
                    self.overlay.buffer_list = None;
                    self.dirty.mark_all();
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Up | Key::PageUp => {
                    popup.move_up();
                    self.dirty.buffer_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::PageDown => {
                    popup.move_down();
                    self.dirty.buffer_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Enter => {
                    if let Some(buffer_id) = popup.selected_buffer_id() {
                        let old_rect = self.overlay.buffer_list;
                        self.buffer_list_popup = None;
                        self.overlay.buffer_list = None;
                        if let Some(rect) = old_rect {
                            self.dirty.mark_popup_closed(rect);
                        }

                        // Save outgoing buffer position, switch, restore incoming
                        self.save_current_position();
                        if let Some(window) = self.windows.active_window_mut() {
                            window.set_buffer(buffer_id);
                        }
                        self.restore_cursor_position();

                        // ── Clamp cursor to valid range ──
                        // When switching FROM a special buffer (RG, Build, Git,
                        // LLM) the cursor line can be far past the target
                        // buffer's line count.
                        self.clamp_cursor_to_buffer(&buffer_id);

                        // ── Explicitly rebuild viewport ──
                        // Special buffers may have scroll_line values far past
                        // a normal file's content.  ensure_cursor_visible alone
                        // does not always recover.
                        {
                            let (cursor_line, line_count, edit_height) = {
                                let window = self.windows.active_window().unwrap();
                                let buffer = self.buffers.get(&buffer_id).unwrap();
                                (
                                    window.cursor.position.line,
                                    buffer.line_count(),
                                    window.height.saturating_sub(1) as usize,
                                )
                            };

                            if let Some(window) = self.windows.active_window_mut() {
                                let half = edit_height / 2;
                                let ideal_scroll = cursor_line.saturating_sub(half);

                                if line_count > edit_height {
                                    let max_scroll = line_count.saturating_sub(edit_height);
                                    window.viewport.scroll_line = ideal_scroll.min(max_scroll);
                                } else {
                                    window.viewport.scroll_line = 0;
                                }
                            }
                        }

                        let buf_name = self
                            .buffers
                            .get(&buffer_id)
                            .map(|b| b.display_name())
                            .unwrap_or_else(|| "?".into());
                        self.set_status(format!("Switched to buffer: {}", buf_name));

                        self.dirty.mark_all();
                        return CommandResult::NoOp;
                    }
                    let old_rect = self.overlay.buffer_list;
                    self.buffer_list_popup = None;
                    self.overlay.buffer_list = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                _ => {
                    let old_rect = self.overlay.buffer_list;
                    self.buffer_list_popup = None;
                    self.overlay.buffer_list = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
            }
        }

        // ── Mark pending (waiting for mark name after m) ── MARK
        if self.mark_pending {
            self.mark_pending = false;
            if let Key::Char(c) = key {
                if c.is_ascii_lowercase() {
                    return self.set_mark(c);
                }
            }
            // Invalid mark character — cancel and fall through
            return CommandResult::NoOp;
        }

        // ── Goto mark pending (waiting for mark name after `) ──
        if self.goto_mark_pending {
            self.goto_mark_pending = false;
            if let Key::Char(c) = key {
                if c == '`' {
                    // `` means jump back to last gd position
                    return self.jump_back();
                } else if c.is_ascii_lowercase() {
                    return self.goto_mark(c);
                }
            }
            // Invalid mark character — cancel and fall through
            return CommandResult::NoOp;
        }

        // ── Replace char pending (waiting for char after r) ──
        if self.replace_char_pending {
            self.replace_char_pending = false;
            let count = self.replace_count;
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

        // ── Keymap popup navigation ──
        if let Some(popup) = &mut self.keymap_popup {
            match key {
                Key::Escape => {
                    self.keymap_popup = None;
                    self.overlay.help = None;
                    self.dirty.mark_all();
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Up | Key::PageUp => {
                    popup.move_up();
                    self.dirty.help = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::PageDown => {
                    popup.move_down();
                    self.dirty.help = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Home => {
                    popup.selected = 0;
                    popup.scroll = 0;
                    // Skip to first non-header
                    while popup.selected < popup.entries.len()
                        && popup.entries[popup.selected].is_header
                    {
                        popup.selected += 1;
                    }
                    popup.clamp_scroll();
                    self.dirty.help = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::End => {
                    popup.selected = popup.entries.len().saturating_sub(1);
                    while popup.selected > 0 && popup.entries[popup.selected].is_header {
                        popup.selected -= 1;
                    }
                    popup.clamp_scroll();
                    self.dirty.help = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                _ => {
                    self.keymap_popup = None;
                    self.overlay.help = None;
                    self.dirty.mark_all();
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
            }
        }

        // ── MRU popup navigation ──
        if let Some(popup) = &mut self.mru_popup {
            match key {
                Key::Escape | Key::Ctrl('c') => {
                    self.mru_popup = None;
                    self.overlay.mru = None;
                    self.dirty.mark_all();
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Up | Key::Char('k') | Key::PageUp => {
                    popup.move_up();
                    self.dirty.mru = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::Char('j') | Key::PageDown => {
                    popup.move_down();
                    self.dirty.mru = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Enter => {
                    if let Some(entry) = popup.selected_entry().cloned() {
                        let old_rect = self.overlay.mru;
                        self.mru_popup = None;
                        self.overlay.mru = None;
                        if let Some(rect) = old_rect {
                            self.dirty.mark_popup_closed(rect);
                        }

                        // Open the file at saved position
                        return match self.open_file(&entry.path) {
                            Ok(_) => {
                                // Restore saved cursor position
                                if let Some(window) = self.windows.active_window_mut() {
                                    if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                                        let max_line = buffer.line_count().saturating_sub(1);
                                        window.cursor.position.line = entry.line.min(max_line);
                                        let max_col = buffer.line_len(window.cursor.position.line);
                                        window.cursor.position.col = entry.col.min(max_col);
                                        window.cursor.desired_col = None;
                                        let bid = window.buffer_id;
                                        self.ensure_cursor_visible(&bid);
                                    }
                                }
                                self.dirty.mark_all();
                                CommandResult::ViewChanged
                            }
                            Err(e) => {
                                self.set_infobar_message(format!("Failed to open: {}", e));
                                self.dirty.mark_all();
                                CommandResult::ViewChanged
                            }
                        };
                    }
                    // No selection — close popup
                    let old_rect = self.overlay.mru;
                    self.mru_popup = None;
                    self.overlay.mru = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Delete => {
                    if let Some(entry) = popup.entries.get(popup.selected).cloned() {
                        // Remove from the persistent MRU manager
                        self.mru.remove(&entry.path);

                        // Remove from the popup's local list
                        popup.entries.remove(popup.selected);

                        if popup.entries.is_empty() {
                            // No more entries — close the popup
                            let old_rect = self.overlay.mru;
                            self.mru_popup = None;
                            self.overlay.mru = None;
                            if let Some(rect) = old_rect {
                                self.dirty.mark_popup_closed(rect);
                            }
                        } else {
                            // Clamp selection to valid range
                            if popup.selected >= popup.entries.len() {
                                popup.selected = popup.entries.len() - 1;
                            }
                            popup.clamp_scroll();
                        }
                    }
                    self.dirty.mru = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                _ => {
                    // Any other key dismisses the popup
                    let old_rect = self.overlay.mru;
                    self.mru_popup = None;
                    self.overlay.mru = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
            }
        }

        // ── Function list popup navigation ── FUNC
        if let Some(popup) = &mut self.function_list_popup {
            match key {
                Key::Escape | Key::Ctrl('c') => {
                    self.function_list_popup = None;
                    self.overlay.function_list = None;
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }
                Key::Up | Key::PageUp => {
                    popup.move_up();
                    self.dirty.function_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::PageDown => {
                    popup.move_down();
                    self.dirty.function_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Enter => {
                    if let Some(entry) = popup.selected_entry().cloned() {
                        self.function_list_popup = None;
                        self.overlay.function_list = None;

                        // Jump to the function's line
                        if let Some(window) = self.windows.active_window_mut() {
                            let max_line = self
                                .buffers
                                .get(&window.buffer_id)
                                .map(|b| b.line_count().saturating_sub(1))
                                .unwrap_or(0);
                            window.cursor.position.line = entry.line.min(max_line);
                            window.cursor.position.col = 0;
                            window.cursor.desired_col = None;
                            let bid = window.buffer_id;
                            self.ensure_cursor_visible(&bid);
                        }
                        self.set_status(format!("→ {} (line {})", entry.name, entry.line + 1));
                        self.dirty.mark_all();
                        return CommandResult::ViewChanged;
                    }
                    // No selection — close
                    self.function_list_popup = None;
                    self.overlay.function_list = None;
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }
                Key::Backspace => {
                    popup.filter_pop();
                    self.dirty.function_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Char(c) => {
                    popup.filter_push(c);
                    self.dirty.function_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                _ => {
                    // Any other key dismisses
                    self.function_list_popup = None;
                    self.overlay.function_list = None;
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }
            }
        }

        // ── File picker navigation ── FILE PICKER
        if let Some(picker) = &mut self.file_picker {
            match key {
                Key::Escape
                | Key::Char('\x1b')
                | Key::Ctrl('[')
                | Key::Char('q')
                | Key::Char('Q') => {
                    self.file_picker = None;
                    self.overlay.file_picker = None;
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }
                Key::Up | Key::Char('k') | Key::PageUp => {
                    picker.move_up();
                    self.dirty.file_picker = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::Char('j') | Key::PageDown => {
                    picker.sync_visible_height(self.term_height);
                    picker.move_down();
                    self.dirty.file_picker = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Enter => {
                    if let Some(entry) = picker.selected_entry() {
                        if entry.is_dir {
                            picker.go_into(&entry.path.clone());
                            self.dirty.file_picker = true;
                            return CommandResult::NoOp;
                        } else {
                            let path = entry.path.clone();
                            let old_rect = self.overlay.file_picker;
                            self.file_picker = None;
                            self.overlay.file_picker = None;
                            if let Some(rect) = old_rect {
                                self.dirty.mark_popup_closed(rect);
                            }
                            return match self.open_file(&path) {
                                Ok(_) => {
                                    self.dirty.mark_all();
                                    CommandResult::NoOp
                                }
                                Err(e) => CommandResult::Error(e.to_string()),
                            };
                        }
                    }
                    return CommandResult::NoOp;
                }
                Key::Char('-') => {
                    picker.go_up();
                    self.dirty.file_picker = true;
                    return CommandResult::NoOp;
                }
                Key::Backspace => {
                    picker.filter_pop();
                    self.dirty.file_picker = true;
                    return CommandResult::NoOp;
                }
                Key::Char(c) => {
                    picker.filter_push(c);
                    self.dirty.file_picker = true;
                    return CommandResult::NoOp;
                }
                _ => return CommandResult::NoOp,
            }
        }

        // ── LLM PROMPT MODE ──
        if self.mode == Mode::LlmPrompt {
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
                            self.llm_prompt
                                .buffer
                                .insert_str(self.llm_prompt.cursor, &word);
                            self.llm_prompt.cursor += word.len();
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
                            self.llm_prompt
                                .buffer
                                .insert_str(self.llm_prompt.cursor, &line);
                            self.llm_prompt.cursor += line.len();
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
                            self.llm_prompt
                                .buffer
                                .insert_str(self.llm_prompt.cursor, &path);
                            self.llm_prompt.cursor += path.len();
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
                        self.llm_prompt
                            .buffer
                            .insert_str(self.llm_prompt.cursor, &line);
                        self.llm_prompt.cursor += line.len();
                        self.dirty.mark_all();
                        return CommandResult::ViewChanged;
                    }
                    Key::Escape | Key::Ctrl('c') => {
                        // Cancel register wait, but let Escape fall through to cancel LlmPrompt mode entirely
                    }
                    _ => {
                        // Invalid register key, cancel wait and swallow the key.
                        // We must redraw to clear the hints from the infobar.
                        return CommandResult::ViewChanged;
                    }
                }
                // If we didn't return above (e.g. on Escape), fall through to normal LLM prompt handling
            }

            return match self.llm_prompt.handle_key(&key) {
                PromptAction::Changed => {
                    self.dirty.mark_all();
                    CommandResult::ViewChanged
                }
                PromptAction::Submit => {
                    let input = self.llm_prompt.text().to_string();
                    self.llm_prompt.clear();
                    self.llm_prompt.push_history(input.clone());
                    self.clear_messages();
                    self.mode = Mode::Normal;
                    self.dirty.mark_all();
                    return self.llm_send_from_prompt(input);
                }
                PromptAction::Cancel => {
                    self.llm_prompt.clear();
                    self.llm_active_preset = None;
                    self.llm_active_context = None;
                    self.llm_todo_prefix = false;
                    self.mode = Mode::Normal;
                    self.dirty.mark_all();
                    return CommandResult::ModeChanged(Mode::Normal);
                }
                PromptAction::None => CommandResult::NoOp,
            };
        }

        // ── Interactive help popup navigation ── HELP
        if let Some(popup) = &mut self.help_popup {
            match key {
                Key::Escape => {
                    self.help_popup = None;
                    self.overlay.help = None;
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }
                Key::Up | Key::Char('k') | Key::PageUp => {
                    popup.move_up();
                    self.dirty.help = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::Char('j') | Key::PageDown => {
                    popup.move_down();
                    self.dirty.help = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                _ => {
                    let old_rect = self.overlay.help;
                    self.help_popup = None;
                    self.overlay.help = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
            }
        }
        // ── Escape in Normal mode (ESCAPE) ──
        if self.mode == Mode::Normal && key == Key::Escape {
            self.clear_messages();
            self.pending_count.clear();
            self.which_key_hints.clear();
            self.cancel_which_key_debounce();
            self.keybinds.clear_pending();
            self.mark_pending = false;
            self.goto_mark_pending = false;
            if self.diff_popup.is_some() {
                self.diff_popup = None;
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
            self.dirty.cursor = true;
            self.dirty.status_infobar = true;
            // self.dirty.mark_all();
            return CommandResult::NoOp;
        }

        // ── COUNT PREFIX ACCUMULATION ──
        if matches!(
            self.mode,
            Mode::Normal | Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        ) {
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
                    let result = self.confirm_completion();
                    return result;
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
            self.register_popup = None;
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
                            self.command_prompt
                                .buffer
                                .insert_str(self.command_prompt.cursor, &word);
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
                            self.command_prompt
                                .buffer
                                .insert_str(self.command_prompt.cursor, &line);
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
                            self.command_prompt
                                .buffer
                                .insert_str(self.command_prompt.cursor, &path);
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
                        self.command_prompt
                            .buffer
                            .insert_str(self.command_prompt.cursor, &line);
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
                        return CommandResult::ContentChanged;
                    }
                } else if self.mode == Mode::Replace {
                    if let Key::Char(c) = raw_key {
                        self.ensure_undo_group();
                        self.overwrite_char_at_cursor(c);
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

        if !is_passthrough
            && self.mode != Mode::Command
            && self.mode != Mode::LlmPrompt
            && !self.search_input_active
        {
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
                if matches!(
                    self.mode,
                    Mode::Visual | Mode::VisualLine | Mode::VisualBlock
                ) {
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
                    crate::editor::CommandResult::Error(
                        "Codeium not connected. Use :codeium to start.".into(),
                    )
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
                self.mark_pending = true;
                CommandResult::NoOp
            }
            Action::GotoMark => {
                self.goto_mark_pending = true;
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
                            if let Some(other_id) = self
                                .buffers
                                .iter()
                                .find(|b| b.kind == BufferKind::Normal)
                                .map(|b| b.id)
                            {
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
                // Grab text: visual selection → fallback to current line
                let text = if matches!(
                    self.mode,
                    Mode::Visual | Mode::VisualLine | Mode::VisualBlock
                ) {
                    self.get_selection_text().unwrap_or_default()
                } else {
                    self.current_line_content()
                };

                // Clear visual selection if active
                if let Some(w) = self.windows.active_window_mut() {
                    w.selection_anchor = None;
                }

                if text.trim().is_empty() {
                    self.set_infobar_message("No text to check".to_string());
                    return CommandResult::ViewChanged;
                }

                // Mark this response for infobar + register 'e' instead of LLM buffer
                self.llm_infobar_response = true;
                self.llm_infobar_accumulator.clear();
                self.llm_active_preset = Some(LlmPreset::CheckEnglish);
                self.llm_active_context = Some(text.clone());

                self.set_status("Checking English…".to_string());
                self.dirty.status_infobar = true;

                // Send directly — do NOT enter LlmPrompt mode
                return self.llm_send_from_prompt(text);
            }
            Action::LlmQuickPrompt => {
                let context = if matches!(
                    self.mode,
                    Mode::Visual | Mode::VisualLine | Mode::VisualBlock
                ) {
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
                        self.llm_todo_prefix = true;
                        return self.llm_setup_split_session(selected_text);
                    } else {
                        self.llm_active_context = Some(selected_text);
                        self.llm_todo_prefix = true;
                        self.llm_active_preset = None;
                        self.llm_prompt.clear();
                        self.mode = Mode::LlmPrompt;
                        self.dirty.mark_all();
                        return CommandResult::ModeChanged(Mode::LlmPrompt);
                    }
                } else {
                    // ── No visual selection (Normal mode) ──
                    // Just use the simple 1-line prompt like before
                    self.llm_active_context = None;
                    self.llm_todo_prefix = false;
                    self.llm_active_preset = None;
                    self.llm_prompt.clear();
                    self.mode = Mode::LlmPrompt;
                    self.dirty.mark_all();
                    return CommandResult::ModeChanged(Mode::LlmPrompt);
                }
            }
            Action::LlmEnterPrompt => {
                self.llm_active_preset = None;
                self.llm_active_context = None;
                self.llm_prompt.clear();
                self.mode = Mode::LlmPrompt;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::LlmPrompt)
            }
            Action::LlmClearHistory => {
                self.llm_buffer.clear_history();
                self.dirty.mark_all();
                CommandResult::Message("LLM history cleared".to_string())
            }
            Action::LlmQuickTranslateChinese => {
                self.llm_quick_action(LlmPreset::TranslateToChinese, "")
            }
            Action::LlmQuickTranslateEnglish => {
                self.llm_active_preset = Some(LlmPreset::TranslateToEnglish);
                self.llm_active_context = self.get_selection_text();
                self.llm_prompt.clear();
                self.mode = Mode::LlmPrompt;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::LlmPrompt)
            }
            Action::LlmQuickExplain => {
                self.llm_active_preset = Some(LlmPreset::Explain);
                self.llm_active_context = self.get_selection_text();
                self.llm_prompt.clear();
                self.mode = Mode::LlmPrompt;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::LlmPrompt)
            }
            Action::LlmQuickSummarize => {
                self.llm_active_preset = Some(LlmPreset::Summarize);
                self.llm_active_context = self.get_selection_text();
                self.llm_prompt.clear();
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

            Action::DeleteBuffer => {
                // Save cursor position before closing
                if let Some(window) = self.windows.active_window() {
                    if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                        if let Some(ref path) = buffer.file_path {
                            self.position_map.set(path, window.cursor.position);
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
            Action::DeleteWord => self.with_undo_group(|s| {
                for _ in 0..s.current_count {
                    s.delete_word_before_cursor();
                }
                CommandResult::ContentChanged
            }),
            Action::DeleteWordForward => self.with_undo_group(|s| {
                for _ in 0..s.current_count {
                    s.delete_word_after_cursor();
                }
                CommandResult::ContentChanged
            }),
            Action::DeleteToLineEnd => self.with_undo_group(|s| {
                s.delete_to_line_end();
                CommandResult::ContentChanged
            }),
            Action::DeleteToFileEnd => self.with_undo_group(|s| s.delete_to_file_end()),
            Action::DeleteToLineStart => self.with_undo_group(|s| {
                s.delete_to_line_start();
                CommandResult::ContentChanged
            }),

            Action::Register => {
                self.set_status("TODO.".to_string());
                // TODO impl
                CommandResult::NoOp
            }
            Action::DeleteAroundFunction => {
                self.record_action(RepeatableAction::DeleteAroundFunction, self.current_count);
                let result =
                    self.operate_on_function(TextObjectKind::Around, TextObjectOperator::Delete);
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
                self.replace_count = self.current_count;
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
                if matches!(
                    self.mode,
                    Mode::Visual | Mode::VisualLine | Mode::VisualBlock
                ) {
                    self.with_undo_group(|s| s.indent_selection())
                } else {
                    self.with_undo_group(|s| {
                        s.indent_n_lines(s.current_count);
                        CommandResult::ContentChanged
                    })
                }
            }
            Action::Dedent => {
                if matches!(
                    self.mode,
                    Mode::Visual | Mode::VisualLine | Mode::VisualBlock
                ) {
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

                        s.record_action(
                            RepeatableAction::IndentTs {
                                count: s.current_count,
                            },
                            s.current_count,
                        );

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
                        let line = s
                            .windows
                            .active_window()
                            .map(|w| w.cursor.position.line)
                            .unwrap_or(0);
                        Some((line, line + s.current_count.saturating_sub(1)))
                    };

                    s.record_action(
                        RepeatableAction::IndentTs {
                            count: s.current_count,
                        },
                        s.current_count,
                    );

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
                let line = self
                    .windows
                    .active_window()
                    .map(|w| w.cursor.position.line)
                    .unwrap_or(0);
                let last_line = self
                    .current_buffer()
                    .map(|b| b.line_count().saturating_sub(1))
                    .unwrap_or(0);

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
            Action::SaveFmt => match self.format_current_buffer_async(true) {
                Ok(()) => {
                    self.set_status("Formatting…".into());
                    CommandResult::ViewChanged
                }
                Err(e) => CommandResult::Error(e),
            },
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
                if self.lsp_connected {
                    self.push_jump_position();
                    self.request_lsp_goto_definition();
                } else {
                    self.goto_definition();
                }
                CommandResult::ViewChanged
            }

            Action::GotoDeclaration
            | Action::FindReferences
            | Action::HoverInfo
            | Action::CodeAction
            | Action::GitBlame => {
                self.set_status(format!("{} — not yet implemented", action.label()));
                CommandResult::NoOp
            }
            Action::GitStatus => self.git_status_open(""),
            Action::GitDiff => self.git_diff_open(""),
            Action::GitLog => self.git_log_open("", ""),
            Action::GitNextHunk => self.git_next_hunk(),
            Action::GitPrevHunk => self.git_prev_hunk(),
            Action::GitRevertHunk => self.git_revert_hunk(),
            Action::GitGutterToggle => {
                self.git_gutter_enabled = !self.git_gutter_enabled;
                if !self.git_gutter_enabled {
                    self.invalidate_git_gutter();
                }
                self.dirty.mark_all();
                let state = if self.git_gutter_enabled { "on" } else { "off" };
                CommandResult::Message(format!("Git gutter: {}", state))
            }
            Action::GitStageHunk | Action::GitUnstageHunk => {
                self.set_status(format!("{} — not yet implemented", action.label()));
                CommandResult::NoOp
            }
            Action::RunBuild => self.run_build(),
            Action::RipgrepUnderCursor => self.ripgrep_under_cursor(),
            Action::RipgrepInput => {
                self.command_prompt.clear();
                self.set_status("Ripgrep search: ".to_string());
                self.enter_mode(Mode::Command)
                // Note: We'll need a separate ripgrep input mode or reuse command mode
                // For now, redirect to command mode with :rg prefix
            }
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
                    let move_count = if self.current_count > 1 {
                        self.current_count
                    } else {
                        1
                    };
                    for _ in 0..move_count {
                        self.move_cursor_down();
                    }
                }
                result
            }

            // ── Misc ────────────────────────────────────
            Action::Quit => {
                self.flush_lsp_changes();
                let _ = self.lsp_tx.send(crate::lsp::LspMessage::Shutdown);
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
                let _ = self.lsp_tx.send(crate::lsp::LspMessage::Shutdown);
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
                    self.tag_manager.init(path);
                }
                match self.tag_manager.generate_tags() {
                    Ok(msg) => CommandResult::Message(msg),
                    Err(err) => CommandResult::Error(err),
                }
            }
            Action::ToggleWhitespace => {
                self.config.show_whitespace = !self.config.show_whitespace;
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Action::RepeatLastAction => self.repeat_last_action(),
            Action::None => CommandResult::NoOp,
            _ => {
                if self.mode != Mode::Command
                    && self.mode != Mode::LlmPrompt
                    && !self.search_input_active
                {
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
        if self.lsp_change_pending {
            if let Some(deadline) = self.lsp_change_deadline {
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

        // ── Ghost text: log current state before polling ──
        if !self.ghost_text.is_pending() {
            self.ghost_text.is_visible();
        }

        self.poll_codeium_ghost();
        self.validate_ghost_text();
        self.poll_app_messages();
    }
    /// Poll for Codeium server startup result and show feedback.
    fn tick_codeium_startup(&mut self) {
        if let Some(result) = self.codeium.poll_startup() {
            match result {
                Ok(()) => {
                    self.set_status("Codeium: connected ✓".to_string());
                }
                Err(e) => {
                    self.set_infobar_message(format!("Codeium: {}", e));
                    self.codeium.is_connected = false;
                }
            }
            self.dirty.mark_all();
        }
    }

    /// Return the trimmed text of the current line, for register insertion (Ctrl-R Ctrl-L).
    /// When in LlmPrompt mode, prefer a non-special buffer so we pull the
    /// source code line rather than the LLM conversation.
    fn current_line_content(&self) -> String {
        if let Some(window) = self.windows.active_window() {
            let line_idx = window.cursor.position.line;
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                // In LlmPrompt with an LLM/special buffer active, skip to find a
                // normal buffer in another window instead.
                if self.mode == Mode::LlmPrompt && buffer.kind != BufferKind::Normal {
                    // fall through to search other windows below
                } else if line_idx < buffer.line_count() {
                    return buffer
                        .rope
                        .line(line_idx)
                        .to_string()
                        .trim_end()
                        .to_string();
                }
            }
        }

        // Fallback: scan all windows for a Normal buffer (LlmPrompt edge case)
        if self.mode == Mode::LlmPrompt {
            for w in self.windows.iter() {
                if let Some(buffer) = self.buffers.get(&w.buffer_id) {
                    if buffer.kind == BufferKind::Normal {
                        let line_idx = w.cursor.position.line;
                        if line_idx < buffer.line_count() {
                            return buffer
                                .rope
                                .line(line_idx)
                                .to_string()
                                .trim_end()
                                .to_string();
                        }
                    }
                }
            }
        }

        String::new()
    }
    /// Setup the Top=Input / Bottom=Response split layout
    pub fn llm_setup_split_session(&mut self, initial_text: String) -> CommandResult {
        // Save the buffer we came from
        self.llm_origin_buffer_id = self.windows.active_window().map(|w| w.buffer_id);

        // 1. Split the window (Creates Top=Origin, Bottom=Origin)
        self.split_horizontal();

        // 2. Move to Bottom window & set it to LLM Response buffer
        self.next_window();
        let llm_id = self.ensure_llm_buffer();
        if let Some(w) = self.windows.active_window_mut() {
            w.set_buffer(llm_id);
        }

        // 3. Move back to Top window & set it to LLM Input scratchpad
        self.prev_window();

        let content = format!("## TODO / Instructions:\n\n{}", initial_text);

        // Find existing LlmInput buffer ID FIRST (immutable borrow)
        let existing_input_id = self
            .buffers
            .iter()
            .find(|b| b.kind == BufferKind::LlmInput)
            .map(|b| b.id);

        let input_id = if let Some(id) = existing_input_id {
            // Now we can mutate without the immutable borrow alive
            if let Some(buf) = self.buffers.get_mut(&id) {
                buf.rope = ropey::Rope::from(content);
                buf.dirty = true;
            }
            id
        } else {
            let id = self.buffers.new_buffer();
            if let Some(buf) = self.buffers.get_mut(&id) {
                buf.kind = BufferKind::LlmInput;
                buf.rope = ropey::Rope::from(content);
                buf.dirty = true;
            }
            id
        };

        if let Some(w) = self.windows.active_window_mut() {
            w.set_buffer(input_id);
        }

        // Start in Insert mode so they can immediately start typing instructions
        self.mode = Mode::Insert;
        self.dirty.mark_all();
        CommandResult::ModeChanged(Mode::Insert)
    }
    /// Submit the contents of the Top scratchpad to the LLM
    pub fn llm_send_input_buffer(&mut self) -> CommandResult {
        let prompt_text = self
            .buffers
            .iter()
            .find(|b| b.kind == BufferKind::LlmInput)
            .map(|b| b.rope.to_string())
            .unwrap_or_default();

        if prompt_text.trim().is_empty() {
            self.set_infobar_message("LLM prompt is empty".to_string());
            return CommandResult::ViewChanged;
        }

        // CRITICAL: Clear context and todo_prefix because the context is
        // already embedded inside the prompt_text from the scratchpad buffer!
        // If we don't clear these, llm_send_from_prompt will duplicate the code.
        self.llm_active_context = None;
        self.llm_todo_prefix = false;
        self.llm_active_preset = None;

        // Switch focus to the Response window (Bottom) so the user can watch it stream
        self.next_window();
        self.mode = Mode::Normal;

        self.llm_send_from_prompt(prompt_text)
    }

    /// Close the split and restore the original single-window layout
    pub fn llm_close_split_session(&mut self) -> CommandResult {
        // 1. We are in Top window (Input). Move to Bottom window (Response).
        self.next_window();

        // 2. Close the Bottom window. (Cursor automatically returns to Top window)
        self.close_window();

        // 3. Restore Top window to the original code buffer
        if let Some(origin_id) = self.llm_origin_buffer_id.take() {
            if let Some(w) = self.windows.active_window_mut() {
                if self.buffers.get(&origin_id).is_some() {
                    w.set_buffer(origin_id);
                }
            }
        }

        self.mode = Mode::Normal;
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }
    /// Show the register list popup (used by :reg and Ctrl-R in insert mode)
    /// Resolve the contents of a register by name.
    /// Supports: a-z (named), " + * (default/clipboard), % (current filename).
    pub fn resolve_register(&self, c: char) -> Option<String> {
        match c {
            '"' | '+' | '*' => Some(self.yank_register.clone()),
            '%' => self
                .current_buffer()
                .and_then(|b| b.file_path.as_ref())
                .and_then(|p| p.to_str())
                .map(|s| s.to_string()),
            _ if c.is_ascii_lowercase() => self.get_named_register(c).map(|s| s.to_string()),
            _ => None,
        }
    }
    /// Show a popup listing all functions/methods in the current buffer.
    /// Uses tree-sitter to find function nodes.
    pub fn show_function_list(&mut self) -> CommandResult {
        let entries = {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => return CommandResult::Error("No active window".into()),
            };
            let buffer_id = window.buffer_id;
            let buffer = match self.buffers.get_mut(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::Error("No active buffer".into()),
            };

            if buffer.tree().is_none() {
                buffer.init_tree_sitter();
            } else {
                buffer.reparse_tree();
            }

            crate::ed::text_object::collect_all_functions(buffer)
        };

        if entries.is_empty() {
            return CommandResult::Message("No functions found in this buffer".into());
        }
        // Pre-select the function closest to the current cursor line
        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);

        let mut popup = crate::popup::FunctionListPopup::new(entries);
        // Find nearest function above or at cursor
        let mut best_idx = 0;
        let mut best_dist = usize::MAX;
        for (i, entry) in popup.all_entries.iter().enumerate() {
            let dist = cursor_line as isize - entry.line as isize;
            if dist >= 0 && (dist as usize) < best_dist {
                best_dist = dist as usize;
                best_idx = i;
            }
        }
        popup.selected = best_idx;

        self.function_list_popup = Some(popup);
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }
    // ── LLM quick-action helper ─────────────────────────────────────

    /// Execute a quick LLM action (translate, explain, summarize, check English).
    ///
    /// Grabs text from visual selection or current line, sends it directly
    /// to the LLM with the given preset, and streams the response to the
    /// infobar (and register matching the preset). Does NOT enter LlmPrompt
    /// mode — the user stays in their current mode and sees the result inline.
    pub fn llm_quick_action(&mut self, preset: LlmPreset, status_msg: &str) -> CommandResult {
        // Grab text: visual selection → fallback to current line
        let text = if matches!(
            self.mode,
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        ) {
            self.get_selection_text().unwrap_or_default()
        } else {
            self.current_line_content()
        };

        // Clear visual selection
        if let Some(w) = self.windows.active_window_mut() {
            w.selection_anchor = None;
        }

        if text.trim().is_empty() {
            self.set_infobar_message("No text to process".to_string());
            return CommandResult::ViewChanged;
        }

        // Record for dot-repeat
        self.record_action(RepeatableAction::LlmQuickAction { preset }, 1);

        // ── Single-shot: no session, no conversation history ──
        self.llm_single_shot = true;
        self.llm_infobar_response = true;
        self.llm_infobar_accumulator.clear();
        self.llm_active_preset = Some(preset);
        self.llm_active_context = None;
        self.llm_todo_prefix = false;

        let system_prompt = preset.system_prompt().to_string();
        let messages = vec![
            ("system".to_string(), system_prompt),
            ("user".to_string(), text),
        ];

        self.set_status(format!("{}…", status_msg));
        self.dirty.status_infobar = true;

        self.spawn_llm_request(messages)
    }
    pub fn show_register_popup(&mut self) {
        self.register_popup_title = "Registers".to_string();

        let mut lines = Vec::new();

        if !self.yank_register.is_empty() {
            let preview = self.yank_register.lines().next().unwrap_or("");
            let truncated = if preview.chars().count() > 100 {
                let char_count = preview.chars().take(100).collect::<String>();
                format!("{}…", char_count)
            } else {
                preview.to_string()
            };
            let safe_preview = truncated
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            lines.push(format!("\"\"   {}", safe_preview));
        }

        if let Some(buffer) = self.current_buffer() {
            let path_str = if let Some(path) = buffer.file_path.as_ref() {
                path.to_str().unwrap_or("[Invalid Path]").to_string()
            } else {
                buffer.display_name()
            };

            let truncated = if path_str.chars().count() > 100 {
                let char_count = path_str.chars().take(100).collect::<String>();
                format!("{}…", char_count)
            } else {
                path_str
            };
            lines.push(format!("%    {}", truncated));
        }

        for c in 'a'..='z' {
            if let Some(content) = self.get_named_register(c) {
                if !content.is_empty() {
                    let preview = content.lines().next().unwrap_or("");
                    let truncated = if preview.chars().count() > 100 {
                        let char_count = preview.chars().take(100).collect::<String>();
                        format!("{}…", char_count)
                    } else {
                        preview.to_string()
                    };
                    let safe_preview = truncated
                        .replace('\n', "\\n")
                        .replace('\r', "\\r")
                        .replace('\t', "\\t");
                    lines.push(format!("\"{}   {}", c, safe_preview));
                }
            }
        }

        if lines.is_empty() {
            self.register_popup = None;
            self.set_status("All registers are empty".to_string());
        } else {
            self.register_popup = Some(lines);
        }
        self.dirty.mark_all();
    }

    // ── Substitute confirmation ─────────────────────────────────────

    /// Start interactive substitute confirmation mode.
    pub(crate) fn start_substitute_confirm(
        &mut self,
        regex: regex::Regex,
        replacement: String,
        global: bool,
        start_line: usize,
        end_line: usize,
    ) -> CommandResult {
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        let end_line = end_line.min(line_count.saturating_sub(1));

        self.ensure_undo_group();

        self.substitute_confirm = Some(SubstituteConfirmState {
            regex,
            replacement,
            global,
            buffer_id,
            start_line,
            end_line,
            subs_made: 0,
            next_line: start_line,
            next_byte_offset: 0,
            current_match: None,
        });

        self.substitute_advance()
    }

    /// Find the next match, highlight it, and show the prompt.
    /// If no more matches exist, finish the session and show the summary.
    fn substitute_advance(&mut self) -> CommandResult {
        let state = match self.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        let match_info = self.find_substitute_match(&state);

        match match_info {
            Some((line, start_byte, end_byte, match_text)) => {
                let col = self.byte_offset_to_col(state.buffer_id, line, start_byte);
                if let Some(w) = self.windows.active_window_mut() {
                    w.cursor.position = CursorPosition::new(line, col);
                    w.cursor.desired_col = None;
                }
                self.ensure_cursor_visible(&state.buffer_id);

                // Set up search highlighting for the current match only
                self.search_matches = vec![CursorPosition::new(line, col)];
                self.current_search_match = 0;
                self.search_matches_dirty = false;
                self.search_prompt.buffer = match_text;
                self.search_prompt.cursor = self.search_prompt.buffer.len();

                // Build status message BEFORE moving `state`
                let status_msg = format!("replace with \"{}\"? (y/n/a/q/l)", state.replacement);

                let next_state = SubstituteConfirmState {
                    next_line: if state.global { line } else { line + 1 },
                    next_byte_offset: if state.global { end_byte } else { 0 },
                    current_match: Some((line, start_byte, end_byte)),
                    ..state
                };

                self.substitute_confirm = Some(next_state);
                self.set_status(status_msg);

                // Explicit dirty management
                self.dirty.windows = true;
                self.dirty.cursor = true;
                self.dirty.status_powerline = true;
                self.dirty.status_cmdline = true;
                self.dirty.status_infobar = true;

                // ViewChanged ensures process_event also sets these dirty flags
                CommandResult::ViewChanged
            }
            None => {
                let subs_made = state.subs_made;

                // Clear search highlighting
                self.search_matches.clear();
                self.search_matches_dirty = false;
                self.search_prompt.buffer.clear();
                self.search_prompt.cursor = 0;

                self.close_undo_group();
                self.invalidate_git_gutter();
                self.notify_lsp_change();

                // Full redraw: remove highlight, update content, update status
                self.dirty.mark_all();

                if subs_made > 0 {
                    CommandResult::Message(format!("{} substitutions", subs_made))
                } else {
                    CommandResult::Error("Pattern not found".into())
                }
            }
        }
    }
    /// Search for the next match starting from `state.next_line` / `next_byte_offset`.
    fn find_substitute_match(
        &self,
        state: &SubstituteConfirmState,
    ) -> Option<(usize, usize, usize, String)> {
        let buffer = self.buffers.get(&state.buffer_id)?;

        let mut line = state.next_line.max(state.start_line);
        let mut byte_offset = state.next_byte_offset;

        while line <= state.end_line && line < buffer.line_count() {
            let line_text = buffer.line_text(line)?.trim_end_matches('\n').to_string();

            let mat = state
                .regex
                .find_iter(&line_text)
                .find(|m| m.start() >= byte_offset);

            if let Some(m) = mat {
                return Some((line, m.start(), m.end(), m.as_str().to_string()));
            }

            line += 1;
            byte_offset = 0;
        }

        None
    }

    /// Convert a byte offset within a line to a grapheme column.
    fn byte_offset_to_col(&self, buffer_id: BufferId, line: usize, byte_offset: usize) -> usize {
        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return 0,
        };
        let line_text = match buffer.line_text(line) {
            Some(t) => t.trim_end_matches('\n').to_string(),
            None => return 0,
        };
        let safe = byte_offset.min(line_text.len());
        line_text[..safe].graphemes(true).count()
    }

    /// Replace a single match on a line in‑place.
    fn substitute_perform_one(
        &mut self,
        line: usize,
        start_byte: usize,
        end_byte: usize,
        state: &SubstituteConfirmState,
    ) {
        let buffer = match self.buffers.get_mut(&state.buffer_id) {
            Some(b) => b,
            None => return,
        };

        let line_text = buffer
            .line_text(line)
            .unwrap_or_default()
            .trim_end_matches('\n')
            .to_string();

        // Build the new line by splicing: prefix + replacement + suffix
        let matched_text = &line_text[start_byte..end_byte];
        let replaced = state
            .regex
            .replace(matched_text, state.replacement.as_str())
            .to_string();
        let new_line_text = format!(
            "{}{}{}",
            &line_text[..start_byte],
            replaced,
            &line_text[end_byte..]
        );

        // Swap the entire line content
        let old_len = line_text.graphemes(true).count();
        if old_len > 0 {
            buffer.delete_at(CursorPosition::new(line, 0), old_len);
        }
        if !new_line_text.is_empty() {
            buffer.insert_at(CursorPosition::new(line, 0), &new_line_text);
        }
        buffer.dirty = true;
    }

    /// **y** — replace current match, then advance.
    fn substitute_confirm_yes(&mut self) -> CommandResult {
        let mut state = match self.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        if let Some((line, start_byte, end_byte)) = state.current_match {
            let matched_text = {
                let buffer = match self.buffers.get(&state.buffer_id) {
                    Some(b) => b,
                    None => return CommandResult::NoOp,
                };
                let line_text = buffer
                    .line_text(line)
                    .unwrap_or_default()
                    .trim_end_matches('\n')
                    .to_string();
                line_text[start_byte..end_byte].to_string()
            };
            let replaced_text = state
                .regex
                .replace(&matched_text, state.replacement.as_str())
                .to_string();
            let new_match_end_byte = start_byte + replaced_text.len();

            self.substitute_perform_one(line, start_byte, end_byte, &state);
            state.subs_made += 1;

            self.invalidate_git_gutter();
            self.notify_lsp_change();

            if state.global {
                let new_line_len = self
                    .buffers
                    .get(&state.buffer_id)
                    .and_then(|b| b.line_text(line))
                    .map(|t| t.trim_end_matches('\n').len())
                    .unwrap_or(0);
                state.next_byte_offset = new_match_end_byte.min(new_line_len);
                state.next_line = line;
            } else {
                state.next_line = line + 1;
                state.next_byte_offset = 0;
            }

            state.current_match = None;
            self.substitute_confirm = Some(state);

            // Content changed → mark for redraw
            self.dirty.windows = true;
            self.dirty.cursor = true;

            // substitute_advance will set status and additional dirty flags
            let result = self.substitute_advance();

            // If substitute_advance found another match, it returned ViewChanged.
            // We also changed content, so ensure the insert region is marked dirty.
            if result == CommandResult::ViewChanged {
                self.dirty.mark_insert();
            }

            result
        } else {
            self.substitute_confirm = None;
            self.close_undo_group();
            self.invalidate_git_gutter();
            self.notify_lsp_change();
            self.dirty.mark_all();
            CommandResult::ViewChanged
        }
    }
    /// **n** — skip current match, then advance.
    fn substitute_confirm_no(&mut self) -> CommandResult {
        let mut state = match self.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        if let Some((line, start_byte, end_byte)) = state.current_match {
            if state.global {
                state.next_line = line;
                state.next_byte_offset = end_byte;
            } else {
                state.next_line = line + 1;
                state.next_byte_offset = 0;
            }

            state.current_match = None;
            self.substitute_confirm = Some(state);

            // substitute_advance handles status and dirty flags
            self.substitute_advance()
        } else {
            self.substitute_confirm = None;
            self.close_undo_group();
            self.dirty.mark_all();
            CommandResult::ViewChanged
        }
    }
    /// **a** — replace this and all remaining matches without prompting.
    fn substitute_confirm_all(&mut self) -> CommandResult {
        let mut state = match self.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        // Replace the current match first
        if let Some((line, start_byte, end_byte)) = state.current_match {
            // Compute replacement byte length before mutating
            let matched_text = {
                let buffer = match self.buffers.get(&state.buffer_id) {
                    Some(b) => b,
                    None => return CommandResult::NoOp,
                };
                let line_text = buffer
                    .line_text(line)
                    .unwrap_or_default()
                    .trim_end_matches('\n')
                    .to_string();
                line_text[start_byte..end_byte].to_string()
            };
            let replaced_text = state
                .regex
                .replace(&matched_text, state.replacement.as_str())
                .to_string();
            let new_match_end_byte = start_byte + replaced_text.len();

            self.substitute_perform_one(line, start_byte, end_byte, &state);
            state.subs_made += 1;

            if state.global {
                let new_line_len = self
                    .buffers
                    .get(&state.buffer_id)
                    .and_then(|b| b.line_text(line))
                    .map(|t| t.trim_end_matches('\n').len())
                    .unwrap_or(0);
                state.next_byte_offset = new_match_end_byte.min(new_line_len);
                state.next_line = line;
            } else {
                state.next_line = line + 1;
                state.next_byte_offset = 0;
            }
        }

        // Now replace ALL remaining matches without prompting
        self.substitute_confirm_replace_rest(&mut state);

        let subs_made = state.subs_made;

        // Clear search highlighting
        self.search_matches.clear();
        self.search_matches_dirty = false;
        self.search_prompt.buffer.clear();
        self.search_prompt.cursor = 0;

        self.close_undo_group();
        self.invalidate_git_gutter();
        self.notify_lsp_change();

        // Full redraw to clear highlights and show all changes
        self.dirty.mark_all();

        if subs_made > 0 {
            CommandResult::Message(format!("{} substitutions", subs_made))
        } else {
            CommandResult::Error("Pattern not found".into())
        }
    }

    /// **q** or **Escape** — abort without replacing current match.
    fn substitute_confirm_quit(&mut self) -> CommandResult {
        let state = match self.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        let subs_made = state.subs_made;

        // Clear search highlighting
        self.search_matches.clear();
        self.search_matches_dirty = false;
        self.search_prompt.buffer.clear();
        self.search_prompt.cursor = 0;

        self.close_undo_group();

        // If any substitutions were made earlier (y/l), notify
        if subs_made > 0 {
            self.invalidate_git_gutter();
            self.notify_lsp_change();
        }

        // Full redraw to remove highlight
        self.dirty.mark_all();

        if subs_made > 0 {
            CommandResult::Message(format!(
                "{} substitutions — quit at current match",
                subs_made
            ))
        } else {
            CommandResult::Message("Quit — no substitutions made".into())
        }
    }

    /// **l** — replace this match, then quit (last replacement).
    fn substitute_confirm_last(&mut self) -> CommandResult {
        let mut state = match self.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        if let Some((line, start_byte, end_byte)) = state.current_match {
            self.substitute_perform_one(line, start_byte, end_byte, &state);
            state.subs_made += 1;
        }

        let subs_made = state.subs_made;

        // Clear search highlighting
        self.search_matches.clear();
        self.search_matches_dirty = false;
        self.search_prompt.buffer.clear();
        self.search_prompt.cursor = 0;

        self.close_undo_group();
        self.invalidate_git_gutter();
        self.notify_lsp_change();

        // Full redraw to remove highlight and show content change
        self.dirty.mark_all();

        CommandResult::Message(format!("{} substitutions — last", subs_made))
    }

    /// Replace all remaining matches without prompting (helper for "a" key).
    fn substitute_confirm_replace_rest(&mut self, state: &mut SubstituteConfirmState) {
        let buffer_id = state.buffer_id;

        let mut line = state.next_line.max(state.start_line);
        let mut byte_offset = state.next_byte_offset;

        while line <= state.end_line {
            let line_text = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                match buffer.line_text(line) {
                    Some(t) => t.trim_end_matches('\n').to_string(),
                    None => {
                        line += 1;
                        byte_offset = 0;
                        continue;
                    }
                }
            };

            let matches: Vec<_> = if state.global {
                state
                    .regex
                    .find_iter(&line_text)
                    .filter(|m| m.start() >= byte_offset)
                    .collect()
            } else if byte_offset == 0 {
                match state.regex.find_iter(&line_text).next() {
                    Some(m) => vec![m],
                    None => vec![],
                }
            } else {
                vec![]
            };

            if matches.is_empty() {
                line += 1;
                byte_offset = 0;
                continue;
            }

            // Replace all matches on this line at once
            let new_text = if state.global {
                if byte_offset > 0 {
                    let prefix = &line_text[..byte_offset];
                    let suffix = &line_text[byte_offset..];
                    let replaced = state
                        .regex
                        .replace_all(suffix, state.replacement.as_str())
                        .to_string();
                    format!("{}{}", prefix, replaced)
                } else {
                    state
                        .regex
                        .replace_all(&line_text, state.replacement.as_str())
                        .to_string()
                }
            } else {
                state
                    .regex
                    .replace(&line_text, state.replacement.as_str())
                    .to_string()
            };

            let old_len = line_text.graphemes(true).count();
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if old_len > 0 {
                    buffer.delete_at(CursorPosition::new(line, 0), old_len);
                }
                if !new_text.is_empty() {
                    buffer.insert_at(CursorPosition::new(line, 0), &new_text);
                }
                buffer.dirty = true;
            }

            state.subs_made += matches.len();

            line += 1;
            byte_offset = 0;
        }
    }
    /// Update `next_line` / `next_byte_offset` after a replacement at `(line, start_byte, end_byte)`.
    fn substitute_update_search_pos(
        &self,
        state: &mut SubstituteConfirmState,
        line: usize,
        start_byte: usize,
        end_byte: usize,
    ) {
        if state.global {
            // After the replacement the byte offset shifts.
            // new_offset = end_byte + (new_line_len − old_line_len)
            let old_len = self
                .buffers
                .get(&state.buffer_id)
                .and_then(|b| b.line_text(line))
                .map(|t| t.trim_end_matches('\n').len())
                .unwrap_or(0);
            // We already mutated the buffer, so this is the *new* line.
            // But the caller should have already performed the replacement,
            // so old_len is actually the *new* length. We need the old length
            // passed in separately — instead, compute delta from the match.
            // Simplified: just search from start_byte on the same line.
            state.next_line = line;
            state.next_byte_offset = start_byte;
        } else {
            state.next_line = line + 1;
            state.next_byte_offset = 0;
        }
    }
    /// Whether the substitute confirm prompt is active.
    pub fn is_substitute_confirm_active(&self) -> bool {
        self.substitute_confirm.is_some()
    }

    /// Get the substitute confirm prompt text, if active.
    pub fn substitute_confirm_prompt(&self) -> Option<String> {
        self.substitute_confirm
            .as_ref()
            .map(|state| format!("replace with \"{}\"? (y/n/a/q/l)", state.replacement))
    }
    /// Show the format-info popup with a title and multi-line error text.
    /// Automatically splits the text into lines and marks the view dirty.
    pub fn show_fmt_info_popup(&mut self, title: &str, text: &str) {
        self.fmt_info_popup_title = title.to_string();
        self.fmt_info_popup = Some(text.lines().map(|l| l.to_string()).collect());
        self.dirty.mark_all();
    }
} // end of imp editor
