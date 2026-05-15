//! Status bar rendering and state.
//!
//! Manages the 3-line status area at the bottom of the editor:
//!   Line 1: Powerline — mode indicator, filename, cursor position (1-based)
//!   Line 2: Command input — `:` prompt in command mode, messages otherwise
//!   Line 3: Infobar — which-key hints, general info (encoding, filetype)

use crate::buffer::Buffer;
use crate::editor::Mode;

// ── Status segment ──────────────────────────────────────────────────

/// A segment of the status bar, with text and styling.
#[derive(Debug, Clone)]
pub struct StatusSegment {
    pub text: String,
    pub style: SegmentStyle,
    pub priority: u8,
}

/// Styling for a status bar segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentStyle {
    /// Mode indicator (e.g., "NORMAL", "INSERT").
    ModeIndicator,
    /// Powerline connector triangle.
    PowerlineConnector,
    /// File information (name, modified flag).
    FileInfo,
    /// Cursor position (line:col), 1-based for display.
    CursorPosition,
    /// Git branch name.
    GitBranch,
    /// File type / language.
    FileType,
    /// Informational message (on the command input line).
    Info,
    /// Error message.
    Error,
    /// Progress indicator.
    Progress,
    /// Which-key hint entry.
    WhichKey,
    Warning,
}

// ── Status bar state ────────────────────────────────────────────────

/// The current state to render in the status bar.
#[derive(Debug, Clone)]
pub struct StatusBarState {
    /// Current editor mode.
    pub mode: Mode,
    /// Current buffer (if any).
    pub buffer: Option<StatusBufferInfo>,
    /// Cursor position (0-indexed internally; display as +1).
    pub cursor_line: usize,
    pub cursor_col: usize,
    /// Total lines in buffer.
    pub total_lines: usize,
    /// Git branch name (if in a git repo).
    pub git_branch: Option<String>,
    /// Whether the buffer is dirty.
    pub dirty: bool,
    /// Status message (shown on Line 2).
    pub message: Option<String>,
    /// Error message (shown on Line 2).
    pub error: Option<String>,
    /// Command line text (shown on Line 2 when in command mode).
    pub command_line: String,
    /// Whether the editor is recording a macro.
    pub recording_macro: bool,
    /// Pending key sequence (for multi-key bindings).
    pub pending_keys: String,
    /// LSP server status indicator.
    pub lsp_status: Option<String>,
    /// Which-key hints (shown on Line 3 infobar).
    pub which_key_hints: Vec<(String, String)>,
    pub infobar_message: Option<String>,
}

/// Buffer information needed for the status bar.
#[derive(Debug, Clone)]
pub struct StatusBufferInfo {
    pub file_name: String,
    pub language: String,
    pub dirty: bool,
}

// ── Status bar renderer ─────────────────────────────────────────────

/// Renders the status bar from state.
pub struct StatusBarRenderer {
    /// Terminal width for truncation.
    pub term_width: u16,
}

impl StatusBarRenderer {
    pub fn new(term_width: u16) -> Self {
        Self { term_width }
    }

    /// Build the list of segments for Line 1 (powerline).
    pub fn build_powerline_segments(&self, state: &StatusBarState) -> Vec<StatusSegment> {
        let mut segments = Vec::new();

        // 1. Mode indicator.
        let mode_text = format!(" {} > ", state.mode.as_str());
        segments.push(StatusSegment {
            text: mode_text,
            style: SegmentStyle::ModeIndicator,
            priority: 0,
        });

        // 2. File info + cursor position.
        if let Some(ref buf) = state.buffer {
            let modified = if buf.dirty { " [+]" } else { "" };
            // 1-based display for line:col
            let pos_text = format!(
                " {}{} {}:{} / {} ",
                buf.file_name,
                modified,
                state.cursor_line + 1,
                state.cursor_col + 1,
                state.total_lines
            );
            segments.push(StatusSegment {
                text: pos_text,
                style: SegmentStyle::FileInfo,
                priority: 1,
            });

            // 3. Language (right-aligned).
            if !buf.language.is_empty() {
                segments.push(StatusSegment {
                    text: format!(" {} ", buf.language),
                    style: SegmentStyle::FileType,
                    priority: 10,
                });
            }
        }

        segments
    }

    /// Build the command/message line content (Line 2).
    pub fn build_cmdline_text(&self, state: &StatusBarState) -> (String, SegmentStyle) {
        if state.mode == Mode::Command {
            (format!(":{}", state.command_line), SegmentStyle::Info)
        } else if let Some(ref error) = state.error {
            (format!(" {}", error), SegmentStyle::Error)
        } else if let Some(ref msg) = state.message {
            (format!(" {}", msg), SegmentStyle::Info)
        } else {
            (" ".to_string(), SegmentStyle::Info)
        }
    }

    /// Build the infobar content (Line 3).
    pub fn build_infobar_text(&self, state: &StatusBarState) -> (String, SegmentStyle) {
        // Priority 1: which-key hints
        if !state.which_key_hints.is_empty() {
            let hints_text: String = state
                .which_key_hints
                .iter()
                .take(8)
                .map(|(keys, desc)| format!(" {}:{} ", keys, desc))
                .collect();
            return (hints_text, SegmentStyle::WhichKey);
        }

        // Priority 2: infobar message (formatter warnings, etc.)
        if let Some(ref msg) = state.infobar_message {
            return (format!(" {}", msg), SegmentStyle::Warning);
        }

        // Priority 3: default info
        let mut info = " utf-8".to_string();
        if let Some(ref buf) = state.buffer {
            if !buf.language.is_empty() {
                info.push_str(&format!(" {}", buf.language));
            }
            if buf.dirty {
                info.push_str(" [modified]");
            }
        }
        (info, SegmentStyle::Info)
    }
}

// ── Status bar builder ──────────────────────────────────────────────

/// Helper to build a `StatusBarState` from the editor's current state.
impl StatusBarState {
    /// Create status bar state from an editor context.
    pub fn from_editor(
        mode: Mode,
        buffer: Option<&Buffer>,
        cursor_line: usize,
        cursor_col: usize,
        total_lines: usize,
        git_branch: Option<String>,
        message: Option<String>,
        error: Option<String>,
        command_line: &str,
        pending_keys: &str,
        which_key_hints: Vec<(String, String)>,
        infobar_message: Option<String>,
    ) -> Self {
        let buffer_info = buffer.map(|b| StatusBufferInfo {
            file_name: b
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "[No Name]".to_string()),
            language: b
                .language
                .map(|l| l.as_str().to_string())
                .unwrap_or_default(),
            dirty: b.dirty,
        });

        Self {
            mode,
            buffer: buffer_info,
            cursor_line,
            cursor_col,
            total_lines,
            git_branch,
            dirty: buffer.map(|b| b.dirty).unwrap_or(false),
            message,
            error,
            command_line: command_line.to_string(),
            recording_macro: false,
            pending_keys: pending_keys.to_string(),
            lsp_status: None,
            which_key_hints,
            infobar_message,
        }
    }
}
