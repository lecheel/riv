// src/ed/git.rs
//! Git integration: gutter signs, hunk navigation, diff popups, and hunk reverts.

use std::time::Instant;

use crate::buffer::BufferId;
use crate::ed::lsp::LspExt;
use crate::ed::MovementExt;
use crate::editor::Editor;
use crate::git::{DiffHunk, DiffLineType, GitProvider, HunkRange};
use crate::CommandResult;

// -----------------------------------------------------------------------------
// Diff popup types (non‑interactive overlay)
// -----------------------------------------------------------------------------

/// A non-interactive diff popup shown at the right-top of the screen.
/// Appears automatically when the cursor is near a git hunk, and hides
/// when the cursor moves away. Does NOT intercept input.
#[derive(Debug, Clone)]
pub struct DiffPopup {
    /// Content lines to display (diff lines with +/- prefixes).
    pub lines: Vec<DiffPopupLine>,
    /// Width fraction of terminal (0.0–1.0).
    pub width_fraction: f32,
    /// Max content rows (not counting title/border).
    pub max_rows: usize,
}

/// A single line in the diff popup.
#[derive(Debug, Clone)]
pub struct DiffPopupLine {
    pub prefix: DiffPopupPrefix,
    pub text: String,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffPopupPrefix {
    Add,
    Delete,
    Context,
    Header,
}

// -----------------------------------------------------------------------------
// Git extension trait
// -----------------------------------------------------------------------------

/// Extension trait for Git operations (gutter, hunks, diff popups).
pub trait GitExt {
    // --- Gutter management -------------------------------------------------
    /// Ensure the git gutter for the current buffer is up to date.
    /// Returns `true` if the gutter is usable (computed or up to date),
    /// `false` if git is disabled or no repository is found.
    fn ensure_git_gutter(&mut self) -> bool;

    /// Mark the git gutter as dirty (call after content changes).
    /// The actual `git diff` recomputation is deferred until the debounce
    /// interval has elapsed.
    fn invalidate_git_gutter(&mut self);

    /// Called every frame from the main loop. Handles debounced background
    /// updates of the git gutter and diff popup.
    fn tick_git(&mut self);

    // --- Hunk navigation ---------------------------------------------------
    /// Jump to the next git hunk (below the cursor) and show its diff popup.
    fn git_next_hunk(&mut self) -> CommandResult;

    /// Jump to the previous git hunk (above the cursor) and show its diff popup.
    fn git_prev_hunk(&mut self) -> CommandResult;

    /// Revert the hunk under the cursor (or the closest hunk) using `git revert`.
    fn git_revert_hunk(&mut self) -> CommandResult;

    // --- Diff popup --------------------------------------------------------
    /// Show a diff popup for the given hunk at the top‑right of the screen.
    fn show_hunk_popup(&mut self, hunk: &DiffHunk);

    /// Build a `DiffPopup` from a `DiffHunk`.
    fn build_diff_popup(hunk: &DiffHunk) -> DiffPopup;

    /// Update the diff popup based on the current cursor position.
    /// Automatically shows or hides the popup depending on whether the cursor
    /// is inside a hunk. Used in auto mode.
    fn update_diff_popup(&mut self);

    /// Refresh the diff popup content if it is currently shown (used after
    /// gutter recomputation in both auto and manual mode).
    fn refresh_diff_popup_if_shown(&mut self);

    // --- Internal helpers --------------------------------------------------
    /// Find the first line inside a hunk that actually has a git sign (added/modified).
    fn first_changed_line_in_hunk(&self, buffer_id: BufferId, hunk: &HunkRange) -> Option<usize>;

    /// Find the last line inside a hunk that actually has a git sign.
    fn last_changed_line_in_hunk(&self, buffer_id: BufferId, hunk: &HunkRange) -> Option<usize>;

    fn force_git_gutter_recompute(&mut self);
    /// Clear all git gutter state for the active buffer (signs, cached hunks,
    /// diff popup). Used when git is disabled or the current file is not in
    /// a git repository. Marks the gutter as "computed with no changes" to
    /// prevent recompute loops on every frame.
    fn clear_git_gutter_state(&mut self);
}

impl GitExt for Editor {
    fn invalidate_git_gutter(&mut self) {
        // If git is disabled, clear any stale gutter state immediately
        // instead of just setting a dirty flag that will never be processed.
        if !self.config.enable_git || !self.git_gutter_enabled {
            self.clear_git_gutter_state();
            return;
        }
        if self.diff_mode_active {
            self.diff_popup = None;
        }
        self.git_gutter_dirty_since = Some(Instant::now());
    }
    fn ensure_git_gutter(&mut self) -> bool {
        // ── Git disabled or gutter toggled off ──
        if !self.config.enable_git || !self.git_gutter_enabled {
            self.clear_git_gutter_state();
            return false;
        }

        let file_path = match self.windows.active_window() {
            Some(w) => self
                .buffers
                .get(&w.buffer_id)
                .and_then(|b| b.file_path.clone()),
            None => {
                self.clear_git_gutter_state();
                return false;
            }
        };

        // ── Scratch buffer (no file path) ──
        let file_path = match file_path {
            Some(p) => p,
            None => {
                self.clear_git_gutter_state();
                return false;
            }
        };

        let file_path = file_path.canonicalize().unwrap_or(file_path);
        let file_dir = file_path.parent().unwrap_or(file_path.as_path());

        // ── No git provider yet — try to create one ──
        if self.git_provider.is_none() {
            match GitProvider::new(file_dir) {
                Ok(gp) => {
                    if let Ok(branch) = gp.current_branch() {
                        if let Some(w) = self.windows.active_window() {
                            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                                buf.git_gutter.set_branch(branch);
                            }
                        }
                    }
                    self.git_provider = Some(gp);
                }
                Err(_) => {
                    // Not a git repository — clear gutter and stop retrying
                    self.clear_git_gutter_state();
                    return false;
                }
            }
        }

        // ── Debounce / dirty handling ──
        if let Some(dirty_since) = self.git_gutter_dirty_since {
            if dirty_since.elapsed().as_millis() < self.git_gutter_debounce_ms as u128 {
                return false;
            }
            self.git_gutter_dirty_since = None;
            if let Some(w) = self.windows.active_window() {
                if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                    buf.git_gutter.clear();
                }
            }
        }

        let needs_update = self
            .windows
            .active_window()
            .map(|w| {
                self.buffers
                    .get(&w.buffer_id)
                    .map(|b| !b.git_gutter.is_computed())
                    .unwrap_or(true)
            })
            .unwrap_or(true);

        if !needs_update {
            return true;
        }

        let gp = match &self.git_provider {
            Some(gp) => gp,
            None => return false,
        };

        let buffer_content = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.text());

        let hunks = match buffer_content {
            Some(content) => match gp.diff_buffer(&file_path, &content) {
                Ok(h) => h,
                Err(_) => {
                    // Diff failed (e.g. file not tracked) — clear and mark done
                    self.clear_git_gutter_state();
                    return false;
                }
            },
            None => return false,
        };

        let ranges = GitProvider::hunk_ranges(&hunks);
        let signs = GitProvider::line_signs_from_diff(&hunks);
        self.cached_diff_hunks = hunks;

        if let Some(w) = self.windows.active_window() {
            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                buf.git_gutter.update_with_diff_signs(ranges, signs);
            }
        }

        true
    }
    fn tick_git(&mut self) {
        if let Some(dirty_since) = self.git_gutter_dirty_since {
            if dirty_since.elapsed().as_millis() >= self.git_gutter_debounce_ms as u128 {
                // Always attempt recompute once debounce expires
                self.ensure_git_gutter();
                // Always redraw after gutter recompute attempt, even if git diff is empty
                if self.diff_popup.is_some() {
                    self.refresh_diff_popup_if_shown();
                }
                self.dirty.mark_all();
            }
        }
    }

    fn git_next_hunk(&mut self) -> CommandResult {
        // Force immediate recomputation — bypass debounce for user-initiated navigation
        self.force_git_gutter_recompute();

        if !self.ensure_git_gutter() {
            return CommandResult::Message("No git diff available.".to_string());
        }

        // Derive hunk ranges from cached_diff_hunks directly — guaranteed
        // consistent with the diff data and the popup (not from git_gutter.hunks()
        // which may be out of sync with the displayed signs).
        let hunk_ranges = GitProvider::hunk_ranges(&self.cached_diff_hunks);
        if hunk_ranges.is_empty() {
            return CommandResult::Message("No hunks found.".to_string());
        }

        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // Find the first hunk that starts strictly AFTER the cursor line.
        let next_idx = hunk_ranges
            .iter()
            .enumerate()
            .find(|(_, h)| h.start > cursor_line)
            .map(|(i, _)| i);
        let idx = match next_idx {
            Some(idx) => idx,
            None => {
                self.diff_popup = None;
                self.set_status("No more hunks forward.".to_string());
                return CommandResult::ViewChanged;
            }
        };

        let hunk_range = &hunk_ranges[idx];
        let target_line = self
            .first_changed_line_in_hunk(buffer_id, hunk_range)
            .unwrap_or(hunk_range.start);

        self.move_to_position(target_line, 0);

        // Show the diff popup for the new hunk
        let diff_hunk_clone = self.cached_diff_hunks.get(idx).cloned();
        if let Some(hunk) = diff_hunk_clone {
            if self.config.display_hunk {
                self.show_hunk_popup(&hunk);
            }
        }

        let msg = format!(
            "Hunk {}/{}: line {} ({}, {} lines)",
            idx + 1,
            hunk_ranges.len(),
            target_line + 1,
            match hunk_range.kind {
                crate::git::GitSign::Added => "added",
                crate::git::GitSign::Modified => "modified",
                crate::git::GitSign::RemovedAbove => "removed",
            },
            hunk_range.count,
        );
        self.set_status(msg);

        CommandResult::ViewChanged
    }

    fn git_prev_hunk(&mut self) -> CommandResult {
        // Force immediate recomputation — bypass debounce for user-initiated navigation
        self.force_git_gutter_recompute();

        if !self.ensure_git_gutter() {
            return CommandResult::Message("No git diff available.".to_string());
        }

        // Derive hunk ranges from cached_diff_hunks directly (consistent with popup)
        let hunk_ranges = GitProvider::hunk_ranges(&self.cached_diff_hunks);
        if hunk_ranges.is_empty() {
            return CommandResult::Message("No hunks found.".to_string());
        }

        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // Check if cursor is inside a hunk
        let current_idx = hunk_ranges
            .iter()
            .position(|h| cursor_line >= h.start && cursor_line < h.end());

        // Find the previous hunk
        let prev_idx = match current_idx {
            Some(0) => None,
            Some(idx) => Some(idx - 1),
            None => {
                // Not inside any hunk — find the last hunk that starts before cursor
                hunk_ranges
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, h)| h.start < cursor_line)
                    .map(|(i, _)| i)
            }
        };

        let idx = match prev_idx {
            Some(idx) => idx,
            None => {
                self.diff_popup = None;
                self.set_status("No more hunks backward.".to_string());
                return CommandResult::ViewChanged;
            }
        };
        let hunk_range = &hunk_ranges[idx];
        let target_line = self
            .last_changed_line_in_hunk(buffer_id, hunk_range)
            .unwrap_or(hunk_range.start);

        self.move_to_position(target_line, 0);

        let diff_hunk_clone = self.cached_diff_hunks.get(idx).cloned();
        if let Some(hunk) = diff_hunk_clone {
            if self.config.display_hunk {
                self.show_hunk_popup(&hunk);
            }
        }

        let msg = format!(
            "Hunk {}/{}: line {} ({}, {} lines)",
            idx + 1,
            hunk_ranges.len(),
            target_line + 1,
            match hunk_range.kind {
                crate::git::GitSign::Added => "added",
                crate::git::GitSign::Modified => "modified",
                crate::git::GitSign::RemovedAbove => "removed",
            },
            hunk_range.count,
        );
        self.set_status(msg);

        CommandResult::ViewChanged
    }

    fn git_revert_hunk(&mut self) -> CommandResult {
        // Force immediate recomputation — bypass debounce for user-initiated action
        self.force_git_gutter_recompute();

        if !self.ensure_git_gutter() {
            return CommandResult::Error("No git diff available.".to_string());
        }

        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);

        // Use cached_diff_hunks-derived ranges for consistency
        let hunk_ranges = GitProvider::hunk_ranges(&self.cached_diff_hunks);
        let hunk_idx = hunk_ranges
            .iter()
            .position(|h| cursor_line >= h.start && cursor_line < h.start + h.count);

        let hunk_idx = match hunk_idx {
            Some(i) => i,
            None => return CommandResult::Error("Cursor is not in a hunk.".to_string()),
        };

        let file_path = match self.windows.active_window() {
            Some(w) => self
                .buffers
                .get(&w.buffer_id)
                .and_then(|b| b.file_path.clone()),
            None => return CommandResult::Error("No file path.".to_string()),
        };
        let file_path = match file_path {
            Some(p) => p,
            None => return CommandResult::Error("No file path.".to_string()),
        };

        let hunk_new_start = self
            .cached_diff_hunks
            .get(hunk_idx)
            .map(|h| h.new_start)
            .unwrap_or(0);

        // Perform the revert — git_provider borrow ends after this expression
        let revert_result = match &self.git_provider {
            Some(gp) => gp.revert_hunk(&file_path, self.cached_diff_hunks.get(hunk_idx).unwrap()),
            None => return CommandResult::Error("No git provider.".to_string()),
        };

        match revert_result {
            Ok(()) => {
                // Dismiss diff popup (the reverted hunk no longer exists)
                self.diff_popup = None;

                // ── Reload buffer content from disk ──
                // Use replace_all() instead of directly assigning buf.rope so
                // the change is recorded in the undo history.  This lets the
                // user press 'u' to undo the hunk revert.
                let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
                if let Some(bid) = buffer_id {
                    // Grab cursor position before mutating the buffer (needed by replace_all)
                    let cursor_pos = self
                        .windows
                        .active_window()
                        .map(|w| w.cursor.position)
                        .unwrap_or_default();

                    if let Some(buf) = self.buffers.get_mut(&bid) {
                        if let Some(ref path) = buf.file_path {
                            if let Ok(content) = std::fs::read_to_string(path) {
                                buf.replace_all(&content, cursor_pos);
                                buf.last_saved_text = content;
                                buf.dirty = false;
                                buf.reparse_tree();
                            }
                        }
                        buf.git_gutter.clear();
                    }

                    // Clamp cursor to new buffer bounds
                    if let Some(window) = self.windows.active_window_mut() {
                        if let Some(buf) = self.buffers.get(&bid) {
                            let max_line = buf.line_count().saturating_sub(1);
                            window.cursor.position.line = window.cursor.position.line.min(max_line);
                            let max_col = buf.line_len(window.cursor.position.line);
                            window.cursor.position.col = window.cursor.position.col.min(max_col);
                            window.cursor.desired_col = None;
                        }
                    }
                }

                // Invalidate git gutter to force recompute on next tick/render
                self.invalidate_git_gutter();

                // Notify LSP that the file content changed
                self.lsp_did_open(&file_path);

                self.ensure_cursor_visible_all();
                self.dirty.mark_all();
                CommandResult::Message(format!("Hunk at line {} reverted.", hunk_new_start))
            }
            Err(e) => CommandResult::Error(format!("Failed to revert hunk: {}", e)),
        }
    }
    fn show_hunk_popup(&mut self, hunk: &DiffHunk) {
        let popup = Self::build_diff_popup(hunk);
        self.diff_popup = Some(popup);
        self.dirty.mark_all();
    }

    fn build_diff_popup(hunk: &DiffHunk) -> DiffPopup {
        let mut lines = Vec::new();

        let header_text = hunk.header.trim_start_matches("@@ ").to_string();
        lines.push(DiffPopupLine {
            prefix: DiffPopupPrefix::Header,
            text: header_text,
            old_lineno: None,
            new_lineno: None,
        });

        let max_rows = 10;
        let mut count = 0;
        for dl in &hunk.lines {
            if count >= max_rows {
                break;
            }
            let prefix = match dl.type_ {
                DiffLineType::Add => DiffPopupPrefix::Add,
                DiffLineType::Delete => DiffPopupPrefix::Delete,
                DiffLineType::Context | DiffLineType::HunkHeader => DiffPopupPrefix::Context,
            };
            let text = dl
                .content
                .strip_prefix('+')
                .or_else(|| dl.content.strip_prefix('-'))
                .or_else(|| dl.content.strip_prefix(' '))
                .unwrap_or(&dl.content)
                .to_string();
            lines.push(DiffPopupLine {
                prefix,
                text,
                old_lineno: dl.old_lineno,
                new_lineno: dl.new_lineno,
            });
            count += 1;
        }

        if hunk.lines.len() > max_rows {
            lines.push(DiffPopupLine {
                prefix: DiffPopupPrefix::Context,
                text: format!("... (+{} more lines)", hunk.lines.len() - max_rows),
                old_lineno: None,
                new_lineno: None,
            });
        }

        DiffPopup {
            lines,
            width_fraction: 0.6,
            max_rows,
        }
    }

    fn update_diff_popup(&mut self) {
        if !self.diff_mode_active {
            return;
        }

        if !self.git_gutter_enabled || !self.config.enable_git || self.float_popup.is_some() {
            self.diff_popup = None;
            return;
        }

        // Ensure gutter is up to date (handles debounce internally)
        self.ensure_git_gutter();

        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(usize::MAX);

        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        if buffer_id.is_none() {
            self.diff_popup = None;
            return;
        }
        let buffer_id = buffer_id.unwrap();

        let hunk_idx = {
            let buf = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => {
                    self.diff_popup = None;
                    return;
                }
            };
            buf.git_gutter.hunks().iter().position(|h| {
                let hunk_end = h.start + h.count;
                cursor_line >= h.start.saturating_sub(1) && cursor_line < hunk_end
            })
        };

        match hunk_idx {
            Some(idx) if idx < self.cached_diff_hunks.len() => {
                let hunk = &self.cached_diff_hunks[idx];
                let popup = Self::build_diff_popup(hunk);
                self.diff_popup = Some(popup);
            }
            _ => {
                // Auto mode: hide popup when cursor leaves hunk
                self.diff_popup = None;
            }
        }
    }

    fn refresh_diff_popup_if_shown(&mut self) {
        if self.diff_popup.is_none() {
            return;
        }

        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(usize::MAX);

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => {
                self.diff_popup = None;
                return;
            }
        };

        let hunk_idx = self.buffers.get(&buffer_id).and_then(|buf| {
            buf.git_gutter.hunks().iter().position(|h| {
                let hunk_end = h.start + h.count;
                cursor_line >= h.start.saturating_sub(1) && cursor_line < hunk_end
            })
        });

        match hunk_idx {
            Some(idx) if idx < self.cached_diff_hunks.len() => {
                let hunk = &self.cached_diff_hunks[idx];
                self.diff_popup = Some(Self::build_diff_popup(hunk));
            }
            _ => {
                if self.diff_mode_active {
                    self.diff_popup = None;
                }
            }
        }
    }

    fn first_changed_line_in_hunk(&self, buffer_id: BufferId, hunk: &HunkRange) -> Option<usize> {
        let buffer = self.buffers.get(&buffer_id)?;
        let end = (hunk.start + hunk.count).min(buffer.line_count());
        (hunk.start..end).find(|&line| buffer.git_gutter.sign_at(line).is_some())
    }

    fn last_changed_line_in_hunk(&self, buffer_id: BufferId, hunk: &HunkRange) -> Option<usize> {
        let buffer = self.buffers.get(&buffer_id)?;
        let end = (hunk.start + hunk.count).min(buffer.line_count());
        (hunk.start..end)
            .rev()
            .find(|&line| buffer.git_gutter.sign_at(line).is_some())
    }
    /// Force git gutter recomputation on the next `ensure_git_gutter()` call.
    /// Bypasses debounce and clears cached gutter state so the data is always fresh
    /// for user-initiated navigation.
    fn force_git_gutter_recompute(&mut self) {
        self.git_gutter_dirty_since = None;
        if let Some(w) = self.windows.active_window() {
            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                buf.git_gutter.clear();
            }
        }
    }
    fn clear_git_gutter_state(&mut self) {
        self.cached_diff_hunks.clear();
        self.diff_popup = None;
        self.git_gutter_dirty_since = None;
        if let Some(w) = self.windows.active_window() {
            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                buf.git_gutter.clear();
                // Mark the gutter as computed with empty data so that
                // `ensure_git_gutter` won't keep retrying on every frame
                // for non-git / disabled buffers.  Without this, `clear()`
                // sets `is_computed() → false`, causing `needs_update → true`
                // and an infinite recompute loop.
                buf.git_gutter
                    .update_with_diff_signs(vec![], std::collections::HashMap::new());
            }
        }
        self.dirty.mark_all();
    }
}
