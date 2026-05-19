//! Git subsystem state — extracted from the Editor core.
//!
//! Groups all git-related fields and provides key-handlers for the
//! GitStatus, GitDiff, GitCommit, and GitLog special buffer bindings.

use std::time::Instant;

use crate::buffer::BufferId;
use crate::buffer::BufferKind;
use crate::ed::git_commit::GitCommitExt;
use crate::ed::git_diff::GitDiffExt;
use crate::ed::git_log::GitLogExt;
use crate::ed::git_status::GitStatusExt;
use crate::editor::{CommandResult, Mode};
use crate::git::EditorHunk;
use crate::terminal::Key;
use crate::Editor;

// ── Git state ─────────────────────────────────────────────────────

/// Git subsystem state — extracted from Editor to reduce the core struct size.
pub struct GitState {
    /// Automatic diff popup shown when cursor is near a git hunk.
    /// Non‑interactive — does not intercept input.
    pub diff_popup: Option<crate::ed::git::DiffPopup>,
    /// Whether the diff popup is active (diff mode).
    pub diff_mode_active: bool,
    /// Cached git provider for the current file's repository.
    pub provider: Option<crate::git::GitProvider>,
    /// Whether the git gutter sign column is enabled.
    pub gutter_enabled: bool,
    /// Cached diff hunks for the active buffer (for hunk revert).
    pub cached_diff_hunks: Vec<EditorHunk>,
    /// Timestamp of the last content change that invalidated the git gutter.
    pub gutter_dirty_since: Option<Instant>,
    /// Debounce interval (milliseconds) for git gutter recomputation after edits.
    pub gutter_debounce_ms: u64,
    /// Git log commit count (persisted for refresh).
    pub log_count: usize,
    /// Git log grep pattern (persisted for refresh).
    pub log_grep: String,
    /// Buffer ID of the active GitCommit buffer (for LLM response routing).
    pub commit_buffer_id: Option<BufferId>,
    /// Timestamp when the git commit LLM request started (for animation).
    pub commit_start_time: Option<Instant>,
    /// Diff summary for the git commit LLM prompt.
    pub commit_diff_summary: Option<String>,
}

impl GitState {
    /// Create a new GitState with defaults.
    pub fn new() -> Self {
        Self {
            diff_popup: None,
            diff_mode_active: false,
            provider: None,
            gutter_enabled: true,
            cached_diff_hunks: Vec::new(),
            gutter_dirty_since: None,
            gutter_debounce_ms: 500,
            log_count: 0,
            log_grep: String::new(),
            commit_buffer_id: None,
            commit_start_time: None,
            commit_diff_summary: None,
        }
    }
}

// ── Git buffer key dispatch ────────────────────────────────────────

impl Editor {
    /// Handle special keys when the active buffer is a GitStatus buffer.
    ///
    /// Returns `Some(CommandResult)` if the key was consumed, `None` to
    /// fall through to normal navigation keybinds.
    pub fn handle_git_status_buffer_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.mode != Mode::Normal {
            return None;
        }
        let is_git_status = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.kind == BufferKind::GitStatus)
            .unwrap_or(false);

        if !is_git_status {
            return None;
        }

        match key {
            Key::Char('s') | Key::Char('S') => {
                self.dirty.mark_all();
                Some(self.git_status_toggle_stage())
            }
            Key::Char('c') | Key::Char('C') => Some(self.git_commit_generate()),
            Key::Enter => {
                self.dirty.mark_all();
                Some(self.git_status_goto_file())
            }
            Key::Char('r') | Key::Char('R') => {
                self.dirty.mark_all();
                Some(self.git_status_refresh())
            }
            Key::Char('q') | Key::Char('Q') => Some(self.git_status_close()),
            _ => None,
        }
    }

    /// Handle special keys when the active buffer is a GitDiff buffer.
    pub fn handle_git_diff_buffer_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.mode != Mode::Normal {
            return None;
        }
        let is_git_diff = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.kind == BufferKind::GitDiff)
            .unwrap_or(false);

        if !is_git_diff {
            return None;
        }

        match key {
            Key::Enter => {
                self.dirty.mark_all();
                Some(self.git_diff_goto_file())
            }
            Key::Char('n') => {
                self.dirty.mark_all();
                Some(self.git_diff_next_hunk())
            }
            Key::Char('N') => {
                self.dirty.mark_all();
                Some(self.git_diff_prev_hunk())
            }
            Key::Char('r') | Key::Char('R') => {
                self.dirty.mark_all();
                Some(self.git_diff_refresh())
            }
            Key::Char('q') | Key::Char('Q') => Some(self.git_diff_close()),
            _ => None,
        }
    }

    /// Handle special keys when the active buffer is a GitCommit buffer.
    pub fn handle_git_commit_buffer_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.mode != Mode::Normal {
            return None;
        }
        let is_git_commit = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.kind == BufferKind::GitCommit)
            .unwrap_or(false);

        if !is_git_commit {
            return None;
        }

        match key {
            Key::Char('w') => Some(self.handle_commit_write()),
            Key::Char('q') | Key::Char('Q') => Some(self.git_commit_close()),
            _ => None, // Fall through to normal editing keybinds
        }
    }

    /// Handle special keys when the active buffer is a GitLog buffer.
    pub fn handle_git_log_buffer_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.mode != Mode::Normal {
            return None;
        }
        let is_git_log = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.kind == BufferKind::GitLog)
            .unwrap_or(false);

        if !is_git_log {
            return None;
        }

        match key {
            Key::Enter => {
                self.dirty.mark_all();
                Some(self.git_log_goto_file())
            }
            Key::Char('d') | Key::Char('D') => {
                self.dirty.mark_all();
                Some(self.git_log_show_diff())
            }
            Key::Char('s') | Key::Char('S') => {
                self.dirty.mark_all();
                Some(self.git_log_save_file())
            }
            Key::Char('r') | Key::Char('R') => {
                self.dirty.mark_all();
                Some(self.git_log_refresh())
            }
            Key::Char('q') | Key::Char('Q') => Some(self.git_log_close()),
            _ => None,
        }
    }
}
