// src/editor.rs
//! Editor — the central coordinator for the text editor.
//!
//! Holds buffers, windows, the keybind manager, and the main state machine
//! that processes events and executes actions.

use std::time::Instant;

use crate::action::Action;
use crate::buffer::BufferKind;
use crate::buffer::{Buffer, BufferCollection, BufferId, CursorPosition};
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
    BufferOpsExt, CommandExt, CompletionExt, EditingExt, FileOpsExt, GitExt, LlmExt, MovementExt,
    RepeatExt, RipgrepExt, VisualExt, WindowExt,
};
use crate::ed::{GitDiffExt, GitLogExt, GitStatusExt};
use crate::ed::{TextObjectExt, TextObjectKind, TextObjectOperator};
use crate::ghost_text::GhostTextManager;
use crate::git::DiffHunk;
use crate::keybind::{
    apply_custom_keybindings, default_command_keymap, default_insert_keymap, default_normal_keymap,
    default_visual_keymap, KeyBindManager, KeyBindResult,
};
use crate::llm::{LlmBuffer, LlmPreset};
use crate::misc::format_shortcut_keys;
use crate::misc::parse_shortcut_keys;
use crate::mru::MruManager;
use crate::overlay::OverlayTracker;
use crate::popup::Scrollable;
use crate::popup::TagListPopup;
use crate::popup::{FilePicker, HelpPopup, MruPopup};
use crate::prompt::MiniInputPrompt;
use crate::prompt::PromptAction;
use crate::terminal::{Key, TerminalEvent};
use crate::vocab::VocabManager;
use crate::window::WindowManager;
use std::path::PathBuf;
use tokio::sync::mpsc;

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
    /// Code architecture guide popup (if active).
    pub guide_popup: Option<crate::guide::Guide>,

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
    pub tag_list_popup: Option<TagListPopup>,

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
    /// Mark list popup (if active).
    pub mark_list_popup: Option<crate::popup::MarkListPopup>,

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
    /// Buffer ID of the active GitCommit buffer (for LLM response routing).
    pub git_commit_buffer_id: Option<BufferId>,
    /// Timestamp when the git commit LLM request started (for animation).
    pub git_commit_start_time: Option<std::time::Instant>,
    pub git_commit_diff_summary: Option<String>,

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
    pub buffer_positions:
        std::collections::HashMap<crate::buffer::BufferId, (crate::buffer::CursorPosition, usize)>,

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

    /// Function list popup (if active).
    pub function_list_popup: Option<crate::popup::FunctionListPopup>,

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

        let lsp_tx = if enable_lsp {
            let tx_for_lsp = app_tx.clone();
            let mut lsp_manager = crate::lsp::LspManager::new(tx_for_lsp);
            let sender = lsp_manager.get_sender();

            llm_runtime.spawn(async move {
                lsp_manager.run().await;
            });

            sender
        } else {
            // Dummy channel — receiver is dropped immediately, so
            // is_closed() returns true and lsp_send() silently no-ops.
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            tx
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

        let active_shortcuts = {
            let mut list = Vec::new();
            for (key_str, action_str) in &config.shortcuts {
                if let Some(keys) = parse_shortcut_keys(key_str) {
                    if let Some(action) = crate::keybind::parse_action_str(action_str) {
                        list.push((keys, action));
                    } else {
                        log::warn!(
                            "[config] shortcuts: unknown action '{}' for key '{}'",
                            action_str,
                            key_str
                        );
                    }
                } else {
                    log::warn!(
                        "[config] shortcuts: invalid key '{}' (use chars like 'f' or sequences like 'gf'; modifiers not supported)",
                        key_str
                    );
                }
            }
            list.sort_by(
                |a: &(Vec<crate::terminal::Key>, crate::action::Action),
                 b: &(Vec<crate::terminal::Key>, crate::action::Action)| {
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
            jump: JumpState::default(),
            guide_popup: None,

            marks: std::collections::HashMap::new(),
            last_jump_mark: None,
            mark_pending: false,
            goto_mark_pending: false,
            position_map: crate::session::PositionMap::load(),

            tag_manager: crate::tags::TagManager::new(),
            tag_results: Vec::new(),
            tag_list_popup: None,
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
            mark_list_popup: None,
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
            git_commit_buffer_id: None,
            git_log_grep: String::new(),
            git_commit_start_time: None,
            git_commit_diff_summary: None,

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
            completion: CompletionEngine::new(completion_trigger_len),
            command_completion,
            completion_debounce_timer: None,
            completion_debounce_ms: 50,
            last_edit_time: Instant::now(),
            undo_break_timeout_ms: 2000,
            vocab: {
                let mut v = VocabManager::new(
                    Config::config_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join("vocab.json"),
                );
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
            self.invalidate_git_gutter(); // MAYBE CAN REMOVED
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
                self.fn_name_needs_update = true;
                if self.completion.active {
                    // Only mark the current line as dirty — skip full window redraw
                    // This prevents the popup from flashing
                    let cursor_line = self
                        .windows
                        .active_window()
                        .map(|w| w.cursor.position.line)
                        .unwrap_or(0);
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
        // ── Float popup interception ──
        if self.float_popup.is_some() {
            if key == Key::Escape || key == Key::Ctrl('c') {
                self.float_popup = None;
                self.overlay.float = None;
                self.shortcut_active = false;
                self.shortcut_pending_keys.clear();
                self.dirty.mark_all();
                return CommandResult::NoOp;
            }

            // ── Float Shortcut Transient State ──
            if self.shortcut_active {
                // Backspace: undo last pending key
                if key == Key::Backspace {
                    if !self.shortcut_pending_keys.is_empty() {
                        self.shortcut_pending_keys.pop();
                        self.rebuild_shortcut_popup();
                        return CommandResult::NoOp;
                    }
                    // No pending keys — dismiss
                    self.float_popup = None;
                    self.overlay.float = None;
                    self.shortcut_active = false;
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }

                // Build the new prefix by appending this key
                let mut new_prefix = self.shortcut_pending_keys.clone();
                new_prefix.push(key);
                let prefix_len = new_prefix.len();

                // Find all shortcuts that match the new prefix
                let matching: Vec<usize> = self
                    .active_shortcuts
                    .iter()
                    .enumerate()
                    .filter(|(_, (keys, _))| {
                        keys.len() >= prefix_len && keys[..prefix_len] == new_prefix[..]
                    })
                    .map(|(i, _)| i)
                    .collect();

                if matching.is_empty() {
                    // No match — dismiss popup
                    self.float_popup = None;
                    self.overlay.float = None;
                    self.shortcut_active = false;
                    self.shortcut_pending_keys.clear();
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }

                // Commit the new prefix
                self.shortcut_pending_keys = new_prefix;

                // Check for exact match at current prefix length
                let exact_idx = matching
                    .iter()
                    .find(|&&i| self.active_shortcuts[i].0.len() == prefix_len);
                let has_longer = matching
                    .iter()
                    .any(|&i| self.active_shortcuts[i].0.len() > prefix_len);

                if let Some(&idx) = exact_idx {
                    if !has_longer {
                        // Unambiguous exact match — execute immediately
                        let action = self.active_shortcuts[idx].1.clone();
                        self.float_popup = None;
                        self.overlay.float = None;
                        self.shortcut_active = false;
                        self.shortcut_pending_keys.clear();
                        self.dirty.mark_all();
                        return self.process_action(action);
                    }
                    // Exact match exists but longer sequences are possible — wait
                    self.rebuild_shortcut_popup();
                    return CommandResult::NoOp;
                }

                // No exact match yet, but prefix matches — wait for more keys
                self.rebuild_shortcut_popup();
                return CommandResult::NoOp;
            }

            // Default float popup: dismiss and fall through to process key normally
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
            || self.help_popup.is_some()
            || self.mark_list_popup.is_some();

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

        // ── Mark list popup navigation ── MARKS
        if let Some(popup) = &mut self.mark_list_popup {
            match key {
                Key::Escape | Key::Ctrl('c') => {
                    self.mark_list_popup = None;
                    self.dirty.mark_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Up | Key::PageUp => {
                    popup.move_up();
                    self.dirty.mark_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::PageDown => {
                    popup.move_down();
                    self.dirty.mark_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Enter => {
                    if let Some(entry) = popup.selected_entry().cloned() {
                        self.mark_list_popup = None;

                        if self.buffers.get(&entry.buffer_id).is_none() {
                            self.marks.remove(&entry.name);
                            self.set_error(format!("Mark '{}' buffer closed", entry.name));
                            self.dirty.mark_all();
                            return CommandResult::NoOp;
                        }

                        self.save_jump_mark();

                        if let Some(window) = self.windows.active_window() {
                            if window.buffer_id != entry.buffer_id {
                                if let Some(w) = self.windows.active_window_mut() {
                                    w.set_buffer(entry.buffer_id);
                                }
                            }
                        }

                        self.move_to_position(entry.line, entry.col);
                        self.ensure_cursor_visible_all();
                        self.set_status(format!("Jumped to mark '{}'", entry.name));
                        self.dirty.mark_all(); // buffer switch → full redraw
                        return CommandResult::ViewChanged;
                    }
                    self.mark_list_popup = None;
                    self.dirty.mark_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Delete => {
                    if let Some(entry) = popup.selected_entry().cloned() {
                        let name = entry.name;
                        self.marks.remove(&name);
                        popup.remove_selected();
                        if popup.entries.is_empty() {
                            self.mark_list_popup = None;
                        }
                        self.set_status(format!("Mark '{}' removed", name));
                    }
                    self.dirty.mark_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Backspace => {
                    popup.filter_pop();
                    self.dirty.mark_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Char(c) => {
                    popup.filter_push(c);
                    self.dirty.mark_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                _ => {
                    self.mark_list_popup = None;
                    self.dirty.mark_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
            }
        }
        //-- process_key popup_active (anchor dont remove) --//
        // ── Git status buffer special keys ── GIT STATUlS
        if self.mode == Mode::Normal && !popup_active {
            if let Some(window) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                    if buffer.kind == BufferKind::GitStatus {
                        match key {
                            Key::Char('s') | Key::Char('S') => {
                                self.dirty.mark_all();
                                return self.git_status_toggle_stage();
                            }
                            Key::Char('c') | Key::Char('C') => {
                                return self.git_commit_generate();
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
                            Key::Char('l') => {
                                return self.build_insert_brace_content();
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

        // Add after the GitLog key-interception block, before popup handling:
        // ── Git commit buffer special keys ── GIT COMMIT
        if self.mode == Mode::Normal && !popup_active {
            if let Some(window) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                    if buffer.kind == BufferKind::GitCommit {
                        match key {
                            Key::Char('w') => {
                                return self.handle_commit_write();
                            }
                            Key::Char('q') | Key::Char('Q') => {
                                return self.git_commit_close();
                            }
                            _ => {} // Fall through to normal editing keybinds
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

        // ── Buffer list popup navigation ── BUFFER
        if let Some(popup) = &mut self.buffer_list_popup {
            match key {
                Key::Escape | Key::Ctrl('c') => {
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
                        self.clamp_cursor_to_buffer(&buffer_id);

                        // ── Explicitly rebuild viewport ──
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
                Key::Backspace => {
                    popup.filter_pop();
                    self.dirty.buffer_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Char(c) => {
                    popup.filter_push(c);
                    self.dirty.buffer_list = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                _ => {
                    // Other special keys dismiss the popup
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
                Key::Up | Key::PageUp => {
                    popup.move_up();
                    self.dirty.mru = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::PageDown => {
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
                    // Remove selected entry from persistent MRU and popup list
                    if let Some(&real_idx) = popup.filtered.get(popup.selected) {
                        if let Some(entry) = popup.entries.get(real_idx).cloned() {
                            self.mru.remove(&entry.path);
                            popup.entries.remove(real_idx); // Only remove once!

                            // Rebuild filtered indices after removal
                            let query = popup.filter.to_lowercase();
                            popup.filtered.clear();
                            for (i, entry) in popup.entries.iter().enumerate() {
                                let file_name = entry
                                    .path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("")
                                    .to_string();
                                let dir_str = entry
                                    .path
                                    .parent()
                                    .and_then(|p| p.to_str())
                                    .unwrap_or("")
                                    .to_string();

                                if query.is_empty()
                                    || file_name.to_lowercase().contains(&query)
                                    || dir_str.to_lowercase().contains(&query)
                                {
                                    popup.filtered.push(i);
                                }
                            }

                            // Adjust selected index if necessary
                            if popup.selected >= popup.filtered.len() && !popup.filtered.is_empty()
                            {
                                popup.selected = popup.filtered.len() - 1;
                            }
                            // clamp_scroll takes 0 arguments - it uses visible_rows() from the trait
                            <MruPopup as Scrollable>::clamp_scroll(popup);

                            if popup.entries.is_empty() {
                                let old_rect = self.overlay.mru;
                                self.mru_popup = None;
                                self.overlay.mru = None;
                                if let Some(rect) = old_rect {
                                    self.dirty.mark_popup_closed(rect);
                                }
                            }
                        }
                    }
                    self.dirty.mru = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Backspace => {
                    popup.filter_pop();
                    self.dirty.mru = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Char(c) => {
                    popup.filter_push(c);
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

        // Tag list popup — intercept keys when active
        if let Some(ref mut popup) = self.tag_list_popup {
            match key {
                Key::Char('j') | Key::Down => {
                    popup.move_down();
                    // Also jump to the now-selected entry as preview
                    if let Some(entry) = popup.selected_entry().cloned() {
                        let path = std::path::PathBuf::from(&entry.file);
                        crate::ed::tag::tag_jump(self, &path, entry.line, &entry.name);
                    }
                    return CommandResult::ViewChanged;
                }
                Key::Char('k') | Key::Up => {
                    popup.move_up();
                    if let Some(entry) = popup.selected_entry().cloned() {
                        let path = std::path::PathBuf::from(&entry.file);
                        crate::ed::tag::tag_jump(self, &path, entry.line, &entry.name);
                    }
                    return CommandResult::ViewChanged;
                }
                Key::Enter => {
                    if let Some(entry) = popup.selected_entry().cloned() {
                        let path = std::path::PathBuf::from(&entry.file);
                        crate::ed::tag::tag_jump(self, &path, entry.line, &entry.name);
                    }
                    self.tag_list_popup = None;
                    return CommandResult::ViewChanged;
                }
                Key::Escape => {
                    // Use whatever your Key enum calls Escape
                    self.tag_list_popup = None;
                    return CommandResult::ViewChanged;
                }
                _ => {}
            }
        }

        // ── Guide popup navigation ── GUIDE
        if let Some(popup) = &mut self.guide_popup {
            match key {
                Key::Escape | Key::Ctrl('c') => {
                    self.guide_popup = None;
                    self.dirty.mark_all(); // Closing popup — must redraw underlying windows
                    return CommandResult::NoOp;
                }
                Key::Up | Key::PageUp => {
                    popup.move_up();
                    self.dirty.guide = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Down | Key::PageDown => {
                    popup.move_down();
                    self.dirty.guide = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Enter => {
                    if let Some(entry) = popup.selected_entry().cloned() {
                        let file_path = popup.root.join(&entry.file);

                        // Open the file
                        let open_result = self.open_file(&file_path);
                        if let Err(e) = open_result {
                            self.guide_popup = None;
                            self.dirty.mark_all();
                            return CommandResult::Error(format!(
                                "Cannot open {}: {}",
                                entry.file, e
                            ));
                        }

                        // Search for the anchor string in the buffer
                        if let Some(window) = self.windows.active_window() {
                            let buffer_id = window.buffer_id;
                            if let Some(buffer) = self.buffers.get(&buffer_id) {
                                let source: String = buffer.rope.to_string();
                                if let Some(line) =
                                    crate::guide::Guide::find_anchor_line(&source, &entry.anchor)
                                {
                                    let max_line = buffer.line_count().saturating_sub(1);
                                    if let Some(w) = self.windows.active_window_mut() {
                                        w.cursor.position.line = line.min(max_line);
                                        w.cursor.position.col = 0;
                                        w.cursor.desired_col = None;
                                        let bid = w.buffer_id;
                                        self.ensure_cursor_visible(&bid);
                                    }
                                    self.set_status(format!(
                                        "→ {} ({})",
                                        entry.label, entry.anchor
                                    ));
                                } else {
                                    self.set_status(format!(
                                        "Anchor not found: '{}' in {}",
                                        entry.anchor, entry.file
                                    ));
                                }
                            }
                        }
                        self.scroll_center();
                        self.guide_popup = None;
                        self.dirty.mark_all(); // Closing + buffer change — full redraw
                        return CommandResult::ViewChanged;
                    }
                    self.guide_popup = None;
                    self.dirty.mark_all();
                    return CommandResult::NoOp;
                }
                Key::Backspace => {
                    popup.filter_pop();
                    self.dirty.guide = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                Key::Char(c) => {
                    popup.filter_push(c);
                    self.dirty.guide = true;
                    self.dirty.cursor = true;
                    return CommandResult::NoOp;
                }
                _ => {
                    // Any other key dismisses the popup
                    self.guide_popup = None;
                    self.dirty.mark_all(); // Closing — full redraw
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
            self.shortcut_pending_keys.clear();
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
                let was_visual = matches!(
                    self.mode,
                    Mode::Visual | Mode::VisualLine | Mode::VisualBlock
                );

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
                self.llm_infobar_response = true;
                self.llm_infobar_accumulator.clear();
                self.llm_active_preset = Some(LlmPreset::CheckEnglish);
                self.llm_active_context = Some(text.clone());

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
                let ctx = self
                    .shortcut_visual_context
                    .take()
                    .or_else(|| self.get_selection_text()); // fallback for direct visual keybind
                self.llm_quick_action(LlmPreset::TranslateToChinese, &ctx.unwrap_or_default())
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
            Action::GitCommit => self.git_commit_generate(),
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

            //-- process_action Action::RipgrepLast (anchor dont remove) --//
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

    pub fn show_register_popup(&mut self) {
        self.register_popup_title = "Registers".to_string();

        let mut lines = Vec::new();

        if !self.yank_register.is_empty() {
            let is_multiline = self.yank_register.contains('\n');
            let first_line = self
                .yank_register
                .lines()
                .next()
                .unwrap_or("")
                .replace('\r', "\\r")
                .replace('\t', "\\t");

            let max_len = if is_multiline { 89 } else { 97 };
            let mut preview = if first_line.chars().count() > max_len {
                format!("{}…", first_line.chars().take(max_len).collect::<String>())
            } else {
                first_line
            };

            if is_multiline {
                preview.push_str(" (...)");
            }

            lines.push(format!("\"\"   {}", preview));
        }

        if let Some(buffer) = self.current_buffer() {
            let path_str = if let Some(path) = buffer.file_path.as_ref() {
                path.to_str().unwrap_or("[Invalid Path]").to_string()
            } else {
                buffer.display_name()
            };

            let truncated = if path_str.chars().count() > 100 {
                let char_count = path_str.chars().take(97).collect::<String>();
                format!("{}…", char_count)
            } else {
                path_str
            };
            lines.push(format!("%    {}", truncated));
        }

        for c in 'a'..='z' {
            if let Some(content) = self.get_named_register(c) {
                if !content.is_empty() {
                    let is_multiline = content.contains('\n');
                    let first_line = content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .replace('\r', "\\r")
                        .replace('\t', "\\t");

                    let max_len = if is_multiline { 89 } else { 97 };
                    let mut preview = if first_line.chars().count() > max_len {
                        format!("{}…", first_line.chars().take(max_len).collect::<String>())
                    } else {
                        first_line
                    };

                    if is_multiline {
                        preview.push_str(" (...)");
                    }

                    lines.push(format!("\"{}   {}", c, preview));
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

    /// Show a popup listing all named marks (a-z) for quick navigation.
    pub fn show_mark_list(&mut self) -> CommandResult {
        let mut entries = Vec::new();

        for (&name, &(buffer_id, pos)) in &self.marks {
            let buffer = self.buffers.get(&buffer_id);
            let file_name = buffer
                .map(|b| b.display_name())
                .unwrap_or_else(|| "[closed]".into());
            let line_preview = buffer
                .and_then(|b| {
                    if pos.line < b.line_count() {
                        Some(b.rope.line(pos.line).to_string().trim_end().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            entries.push(crate::popup::MarkEntry {
                name,
                buffer_id,
                file_name,
                line: pos.line,
                col: pos.col,
                line_preview,
            });
        }

        // Sort by mark name for consistent display
        entries.sort_by_key(|e| e.name);

        if entries.is_empty() {
            return CommandResult::Message("No marks set".into());
        }

        // Pre-select mark nearest to current cursor position
        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);
        let cursor_buf = self.windows.active_window().map(|w| w.buffer_id);

        let mut popup = crate::popup::MarkListPopup::new(entries);
        let mut best_idx = 0;
        let mut best_dist = usize::MAX;
        for (i, entry) in popup.entries.iter().enumerate() {
            if Some(entry.buffer_id) == cursor_buf {
                let dist = (cursor_line as isize - entry.line as isize).unsigned_abs();
                if dist < best_dist {
                    best_dist = dist;
                    best_idx = i;
                }
            }
        }
        popup.selected = best_idx;

        self.mark_list_popup = Some(popup);
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }
    /// Rebuild the shortcut popup showing only entries matching the current prefix.
    pub(crate) fn rebuild_shortcut_popup(&mut self) {
        let prefix = self.shortcut_pending_keys.clone();
        let prefix_len = prefix.len();

        let matching: Vec<_> = self
            .active_shortcuts
            .iter()
            .filter(|(keys, _)| keys.len() >= prefix_len && keys[..prefix_len] == prefix[..])
            .collect();

        let mode_name = self.mode.keybind_name();

        // Measure column widths
        let mut max_key_len = 0;
        let mut max_desc_len = 0;
        for (keys, action) in &matching {
            let key_str = crate::misc::format_shortcut_keys(keys);
            max_key_len = max_key_len.max(key_str.len());
            max_desc_len = max_desc_len.max(action.label().len());
        }

        let mut lines = Vec::new();
        for (keys, action) in &matching {
            let key_str = crate::misc::format_shortcut_keys(keys);
            let desc = action.label();

            let original_keys = self.keybinds.keys_for_action_in_mode(mode_name, action);
            let hint = if original_keys.is_empty() {
                String::new()
            } else {
                format!("[{}]", original_keys.join(", "))
            };

            lines.push(format!(
                "  {:<key_w$}  {:<desc_w$}  {}",
                key_str,
                desc,
                hint,
                key_w = max_key_len,
                desc_w = max_desc_len,
            ));
        }

        let prefix_str = crate::misc::format_shortcut_keys(&prefix);
        let title = if prefix_str.is_empty() {
            " Shortcuts ".to_string()
        } else {
            format!(" Shortcuts [{}] ", prefix_str)
        };

        self.float_popup = Some(FloatPopup::new(title, lines));
        self.dirty.mark_all();
    }
    /// Handle the `:vocab <word>` command.
    pub fn vocab_handle(&mut self, word: &str) -> CommandResult {
        let word = word.trim();
        if word.is_empty() {
            return CommandResult::Error("Usage: :vocab <word>".into());
        }

        if self.vocab.add(word) {
            if let Err(e) = self.vocab.save() {
                return CommandResult::Error(format!("Failed to save vocabulary: {}", e));
            }
            CommandResult::Message(format!("Added '{}' to vocabulary", word))
        } else {
            CommandResult::Message(format!("'{}' already in vocabulary", word))
        }
    }
} // end of imp editor
