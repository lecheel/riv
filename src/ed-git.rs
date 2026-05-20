// src/ed/git.rs
//! Git integration: gutter signs, hunk navigation, diff popups, and hunk reverts.

use std::time::Instant;

use crate::buffer::Buffer;
use crate::buffer::BufferId;
use crate::buffer::CursorPosition;
use crate::ed::editing::EditingExt;
use crate::ed::lsp::LspExt;
use crate::ed::MovementExt;
use crate::editor::Editor;
use crate::git::{DiffHunk, DiffLineType, EditorHunk, GitProvider, GitSign, HunkRange};
use crate::CommandResult;

// -----------------------------------------------------------------------------
// Diff popup types (non‑interactive overlay)
// -----------------------------------------------------------------------------

/// A non-interactive diff popup shown at the right-top of the screen.
#[derive(Debug, Clone)]
pub struct DiffPopup {
    pub lines: Vec<DiffPopupLine>,
    pub width_fraction: f32,
    pub max_rows: usize,
}

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

pub trait GitExt {
    fn ensure_git_gutter(&mut self) -> bool;
    fn invalidate_git_gutter(&mut self);
    fn tick_git(&mut self);
    fn git_next_hunk(&mut self) -> CommandResult;
    fn git_prev_hunk(&mut self) -> CommandResult;
    fn git_revert_hunk(&mut self) -> CommandResult;
    fn show_hunk_popup(&mut self, hunk: &DiffHunk);
    fn build_diff_popup(hunk: &DiffHunk) -> DiffPopup;
    fn update_diff_popup(&mut self);
    fn refresh_diff_popup_if_shown(&mut self);
    fn first_changed_line_in_hunk(&self, buffer_id: BufferId, hunk: &EditorHunk) -> Option<usize>;
    fn last_changed_line_in_hunk(&self, buffer_id: BufferId, hunk: &EditorHunk) -> Option<usize>;
    fn force_git_gutter_recompute(&mut self);
    fn clear_git_gutter_state(&mut self);
    /// Safety net: after a **manual** file save (user pressed Save, not
    /// save-on-quit), clear all cached git state and force a fresh recompute
    /// so the diff always reflects the actual on-disk content.
    ///
    /// Do NOT call this on save-during-quit — the buffer is about to be
    /// destroyed, so recomputing gutter signs is wasted work.
    fn sync_after_manual_save(&mut self);
}

impl GitExt for Editor {
    fn invalidate_git_gutter(&mut self) {
        if !self.config.enable_git || !self.git.gutter_enabled {
            self.clear_git_gutter_state();
            return;
        }
        if self.git.diff_mode_active {
            self.git.diff_popup = None;
        }
        self.git.gutter_dirty_since = Some(Instant::now());
    }

    fn ensure_git_gutter(&mut self) -> bool {
        if !self.config.enable_git || !self.git.gutter_enabled {
            self.clear_git_gutter_state();
            return false;
        }

        let file_path = match self.windows.active_window() {
            Some(w) => self.buffers.get(&w.buffer_id).and_then(|b| b.file_path.clone()),
            None => {
                self.clear_git_gutter_state();
                return false;
            }
        };

        let file_path = match file_path {
            Some(p) => p,
            None => {
                self.clear_git_gutter_state();
                return false;
            }
        };

        let file_path = file_path.canonicalize().unwrap_or(file_path);
        let file_dir = file_path.parent().unwrap_or(file_path.as_path());

        if self.git.provider.is_none() {
            match GitProvider::new(file_dir) {
                Ok(gp) => {
                    if let Ok(branch) = gp.current_branch() {
                        if let Some(w) = self.windows.active_window() {
                            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                                buf.git_gutter.set_branch(branch);
                            }
                        }
                    }
                    self.git.provider = Some(gp);
                }
                Err(_) => {
                    self.clear_git_gutter_state();
                    return false;
                }
            }
        }

        if let Some(dirty_since) = self.git.gutter_dirty_since {
            if dirty_since.elapsed().as_millis() < self.git.gutter_debounce_ms as u128 {
                return false;
            }
            self.git.gutter_dirty_since = None;
            if let Some(w) = self.windows.active_window() {
                if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                    buf.git_gutter.clear();
                }
            }
        }

        let needs_update = self
            .windows
            .active_window()
            .map(|w| self.buffers.get(&w.buffer_id).map(|b| !b.git_gutter.is_computed()).unwrap_or(true))
            .unwrap_or(true);

        if !needs_update {
            return true;
        }

        let gp = match &self.git.provider {
            Some(gp) => gp,
            None => return false,
        };

        let buffer_content = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.text());

        // Step 1: raw diff → Vec<DiffHunk>
        let raw_hunks = match buffer_content {
            Some(content) => match gp.diff_buffer(&file_path, &content) {
                Ok(h) => h,
                Err(_) => {
                    self.clear_git_gutter_state();
                    return false;
                }
            },
            None => return false,
        };

        // Step 2: build unified EditorHunks (each carries its own .diff)
        let editor_hunks = GitProvider::build_editor_hunks(&raw_hunks);

        // Step 3: collect per-line signs
        let mut signs = std::collections::HashMap::new();
        for hunk in &editor_hunks {
            for sign in &hunk.signs {
                signs.insert(sign.line, sign.kind);
            }
        }

        // Step 4: single unified cache
        self.git.cached_diff_hunks = editor_hunks;

        if let Some(w) = self.windows.active_window() {
            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                let ranges: Vec<HunkRange> = self
                    .git
                    .cached_diff_hunks
                    .iter()
                    .map(|h| HunkRange {
                        start: h.start,
                        count: h.end.saturating_sub(h.start),
                        kind: h.kind,
                        header: h.diff.header.clone(),
                    })
                    .collect();

                buf.git_gutter.update_with_diff_signs(ranges, signs);
            }
        }

        true
    }

    fn tick_git(&mut self) {
        if let Some(dirty_since) = self.git.gutter_dirty_since {
            if dirty_since.elapsed().as_millis() >= self.git.gutter_debounce_ms as u128 {
                self.ensure_git_gutter();
                if self.git.diff_popup.is_some() {
                    self.refresh_diff_popup_if_shown();
                }
                self.dirty.mark_all();
            }
        }
    }

    fn git_next_hunk(&mut self) -> CommandResult {
        self.force_git_gutter_recompute();

        if !self.ensure_git_gutter() {
            return CommandResult::Message("No git diff available.".to_string());
        }

        let cursor_line = self.windows.active_window().map(|w| w.cursor.position.line).unwrap_or(0);

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // Extract hunk information before borrowing self mutably
        let hunk_info = {
            let hunks = &self.git.cached_diff_hunks;

            let next_idx = hunks.iter().enumerate().find(|(_, h)| h.start > cursor_line).map(|(i, _)| i);

            match next_idx {
                Some(idx) => {
                    let editor_hunk = &hunks[idx];
                    let target_line = self.first_changed_line_in_hunk(buffer_id, editor_hunk).unwrap_or(editor_hunk.start);

                    Some((
                        idx,
                        target_line,
                        editor_hunk.diff.clone(),
                        editor_hunk.kind,
                        editor_hunk.end - editor_hunk.start,
                        hunks.len(),
                    ))
                }
                None => None,
            }
        };

        let (idx, target_line, diff_hunk, kind, hunk_len, total_hunks) = match hunk_info {
            Some(info) => info,
            None => {
                self.git.diff_popup = None;
                self.set_status("No more hunks forward.".to_string());
                return CommandResult::ViewChanged;
            }
        };

        // Now we can mutably borrow self
        self.move_to_position(target_line, 0);
        self.scroll_bottom_third();

        if self.config.display_hunk {
            self.show_hunk_popup(&diff_hunk);
        }

        let msg = format!(
            "Hunk {}/{}: line {} ({}, {} lines)",
            idx + 1,
            total_hunks,
            target_line + 1,
            match kind {
                GitSign::Added => "added",
                GitSign::Modified => "modified",
                GitSign::RemovedAbove => "removed",
            },
            hunk_len,
        );
        self.set_status(msg);

        CommandResult::ViewChanged
    }

    fn git_prev_hunk(&mut self) -> CommandResult {
        self.force_git_gutter_recompute();

        if !self.ensure_git_gutter() {
            return CommandResult::Message("No git diff available.".to_string());
        }

        let cursor_line = self.windows.active_window().map(|w| w.cursor.position.line).unwrap_or(0);

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // Extract hunk information before borrowing self mutably
        let hunk_info = {
            let hunks = &self.git.cached_diff_hunks;

            let current_idx = hunks.iter().position(|h| cursor_line >= h.start && cursor_line < h.end);

            let prev_idx = match current_idx {
                Some(0) => None,
                Some(idx) => Some(idx - 1),
                None => hunks.iter().enumerate().rev().find(|(_, h)| h.start < cursor_line).map(|(i, _)| i),
            };

            match prev_idx {
                Some(idx) => {
                    let editor_hunk = &hunks[idx];
                    let target_line = self.last_changed_line_in_hunk(buffer_id, editor_hunk).unwrap_or(editor_hunk.start);

                    Some((
                        idx,
                        target_line,
                        editor_hunk.diff.clone(),
                        editor_hunk.kind,
                        editor_hunk.end - editor_hunk.start,
                        hunks.len(),
                    ))
                }
                None => None,
            }
        };

        let (idx, target_line, diff_hunk, kind, hunk_len, total_hunks) = match hunk_info {
            Some(info) => info,
            None => {
                self.git.diff_popup = None;
                self.set_status("No more hunks backward.".to_string());
                return CommandResult::ViewChanged;
            }
        };

        // Now we can mutably borrow self
        self.move_to_position(target_line, 0);
        self.scroll_bottom_third();

        if self.config.display_hunk {
            self.show_hunk_popup(&diff_hunk);
        }

        let msg = format!(
            "Hunk {}/{}: line {} ({}, {} lines)",
            idx + 1,
            total_hunks,
            target_line + 1,
            match kind {
                GitSign::Added => "added",
                GitSign::Modified => "modified",
                GitSign::RemovedAbove => "removed",
            },
            hunk_len,
        );
        self.set_status(msg);

        CommandResult::ViewChanged
    }

    fn git_revert_hunk(&mut self) -> CommandResult {
        self.force_git_gutter_recompute();

        if !self.ensure_git_gutter() {
            return CommandResult::Error("No git diff available.".to_string());
        }

        let cursor_line = self.windows.active_window().map(|w| w.cursor.position.line).unwrap_or(0);

        let hunk = match self
            .git
            .cached_diff_hunks
            .iter()
            .find(|h| cursor_line >= h.start && cursor_line < h.end)
            .map(|h| h.diff.clone())
        {
            Some(h) => h,
            None => return CommandResult::Error("Cursor is not in a hunk.".to_string()),
        };

        // Compute cursor position: start of the hunk's region in the original
        // file.  old_start is 1-based, so subtract 1 for 0-based.
        let restore_cursor_line = hunk.old_start.saturating_sub(1);

        let buffer_id = match self.windows.active_window().map(|w| w.buffer_id) {
            Some(id) => id,
            None => return CommandResult::NoOp,
        };

        let file_path = match self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .and_then(|b| b.file_path.clone())
        {
            Some(p) => p,
            None => return CommandResult::Error("No file path.".to_string()),
        };

        self.with_undo_group(|s| {
            if let Some(buffer) = s.buffers.get_mut(&buffer_id) {
                apply_hunk_revert(buffer, &hunk);
                buffer.dirty = true;
                s.invalidate_git_gutter();
                CommandResult::ContentChanged
            } else {
                CommandResult::NoOp
            }
        });

        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position = CursorPosition::new(restore_cursor_line, 0);
            window.cursor.desired_col = None;
        }
        self.clamp_cursor_to_buffer(&buffer_id);

        self.git.diff_popup = None;
        self.invalidate_git_gutter();

        // Use didChange (not didOpen) because the buffer content changed
        // while the file on disk is still the old version.
        // Send the full buffer text and increment the doc version so the
        // LSP server stays in sync with the reverted buffer.
        {
            let text = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.rope.to_string())
                .unwrap_or_default();
            self.increment_lsp_doc_version();
            self.lsp_did_change(&file_path, text, self.get_lsp_doc_version());
        }

        self.ensure_cursor_visible_all();
        self.dirty.mark_all();

        CommandResult::Message("Hunk reverted in buffer (not saved to disk).".to_string())
    }

    fn show_hunk_popup(&mut self, hunk: &DiffHunk) {
        let popup = Self::build_diff_popup(hunk);
        self.git.diff_popup = Some(popup);
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
            // dl.content was already stripped of +/-/ prefix by parse_diff,
            // so use it directly.
            lines.push(DiffPopupLine {
                prefix,
                text: dl.content.clone(),
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
        if !self.git.diff_mode_active {
            return;
        }

        if !self.git.gutter_enabled || !self.config.enable_git || self.popup.float.is_some() {
            self.git.diff_popup = None;
            return;
        }

        self.ensure_git_gutter();

        let cursor_line = self.windows.active_window().map(|w| w.cursor.position.line).unwrap_or(usize::MAX);

        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        if buffer_id.is_none() {
            self.git.diff_popup = None;
            return;
        }

        let hunk_idx = self
            .git
            .cached_diff_hunks
            .iter()
            .position(|h| cursor_line >= h.start.saturating_sub(1) && cursor_line < h.end);

        match hunk_idx {
            Some(idx) => {
                let hunk = &self.git.cached_diff_hunks[idx].diff;
                let popup = Self::build_diff_popup(hunk);
                self.git.diff_popup = Some(popup);
            }
            None => {
                self.git.diff_popup = None;
            }
        }
    }

    fn refresh_diff_popup_if_shown(&mut self) {
        if self.git.diff_popup.is_none() {
            return;
        }

        let cursor_line = self.windows.active_window().map(|w| w.cursor.position.line).unwrap_or(usize::MAX);

        let _buffer_id = self.windows.active_window().map(|w| w.buffer_id);

        let hunk_idx = self
            .git
            .cached_diff_hunks
            .iter()
            .position(|h| cursor_line >= h.start.saturating_sub(1) && cursor_line < h.end);

        match hunk_idx {
            Some(idx) => {
                let hunk = &self.git.cached_diff_hunks[idx].diff;
                self.git.diff_popup = Some(Self::build_diff_popup(hunk));
            }
            None => {
                if self.git.diff_mode_active {
                    self.git.diff_popup = None;
                }
            }
        }
    }

    fn first_changed_line_in_hunk(&self, buffer_id: BufferId, hunk: &EditorHunk) -> Option<usize> {
        let buffer = self.buffers.get(&buffer_id)?;
        let end = hunk.end.min(buffer.line_count());
        (hunk.start..end).find(|&line| buffer.git_gutter.sign_at(line).is_some())
    }

    fn last_changed_line_in_hunk(&self, buffer_id: BufferId, hunk: &EditorHunk) -> Option<usize> {
        let buffer = self.buffers.get(&buffer_id)?;
        let end = hunk.end.min(buffer.line_count());
        (hunk.start..end).rev().find(|&line| buffer.git_gutter.sign_at(line).is_some())
    }

    fn force_git_gutter_recompute(&mut self) {
        self.git.gutter_dirty_since = None;
        if let Some(w) = self.windows.active_window() {
            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                buf.git_gutter.clear();
            }
        }
    }

    fn clear_git_gutter_state(&mut self) {
        self.git.cached_diff_hunks.clear();
        self.git.diff_popup = None;
        self.git.gutter_dirty_since = None;
        if let Some(w) = self.windows.active_window() {
            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                buf.git_gutter.clear();
                buf.git_gutter.update_with_diff_signs(vec![], std::collections::HashMap::new());
            }
        }
        self.dirty.mark_all();
    }

    fn sync_after_manual_save(&mut self) {
        // Called only after a user-initiated save (Ctrl+S / Cmd+S), NOT on
        // save-during-quit.  The user is continuing to edit, so we must
        // bring the git gutter back in sync with the on-disk state.
        //
        // Why only manual save?
        // - On save-on-quit the buffer is about to be destroyed; recomputing
        //   gutter signs is wasted work and may cause panics if the buffer
        //   is partially torn down.
        // - On auto-save (if ever added) the user hasn't explicitly committed
        //   the buffer state to disk, so silently re-syncing could cause
        //   visible gutter flicker for no benefit.
        //
        // This also acts as a safety net: if any earlier operation
        // (e.g. a hunk revert with a subtle offset bug) left the
        // buffer slightly out of sync with what `git diff` would
        // produce from the file on disk, saving + this sync will
        // bring everything back into consistency.
        self.clear_git_gutter_state();
        self.force_git_gutter_recompute();
        self.dirty.mark_all();
    }
}

// ── Hunk revert ─────────────────────────────────────────────────────

/// Apply a hunk revert directly to a buffer.
///
/// Strategy: "build original, replace range".
///
/// Instead of trying to incrementally delete Add lines and insert Delete
/// lines with complex offset tracking (which is error-prone when Add and
/// Delete lines are interleaved in the same hunk, or when multiple
/// disjoint Delete blocks exist), we:
///
/// 1. Build the original (pre-change) content by walking the diff lines
///    and keeping Context + Delete lines (skipping Add lines and HunkHeader).
///
/// 2. Determine the buffer line range occupied by this hunk in the
///    current (new) version from the `new_lineno` values of Add/Context
///    lines.
///
/// 3. Delete that entire range from the buffer (reverse order to preserve
///    indices).
///
/// 4. Insert the original lines at the start of the now-vacated range.
///
/// This is correct because reverting a hunk means replacing the "new"
/// content with the "old" content.  The old content is exactly the
/// Context + Delete lines in diff order.
fn apply_hunk_revert(buffer: &mut Buffer, hunk: &DiffHunk) {
    // ── Phase 1: Build the original (pre-change) line content ──────
    //
    // The original file content for this hunk consists of:
    //   - Context lines (unchanged between old and new)
    //   - Delete lines (present in old, absent in new)
    // Add lines are skipped (they don't exist in the original).
    // HunkHeader lines are metadata, not content.
    let original_lines: Vec<String> = hunk
        .lines
        .iter()
        .filter(|dl| dl.type_ == DiffLineType::Context || dl.type_ == DiffLineType::Delete)
        .map(|dl| dl.content.clone())
        .collect();

    // ── Phase 2: Determine the buffer range to replace ─────────────
    //
    // The current (new) version occupies a contiguous range of buffer
    // lines.  We determine this from the new_lineno values of Add and
    // Context lines (Delete lines have no new_lineno because they
    // don't exist in the new version).
    let new_line_indices: Vec<usize> = hunk
        .lines
        .iter()
        .filter_map(|dl| dl.new_lineno)
        .map(|nl| nl.saturating_sub(1))
        .collect();

    if new_line_indices.is_empty() {
        // Pure-delete hunk with no context lines (possible with `diff -U0`).
        // The deleted lines need to be re-inserted at the old position.
        // old_start is 1-based; convert to 0-based.
        let insert_pos = hunk.old_start.saturating_sub(1);
        for (i, content) in original_lines.iter().enumerate() {
            buffer.insert_at(
                CursorPosition::new(insert_pos + i, 0),
                &format!("{}\n", content),
            );
        }
        return;
    }

    let range_start = *new_line_indices.first().unwrap();
    let range_end = new_line_indices.last().unwrap() + 1;
    let clamped_end = range_end.min(buffer.line_count());

    // ── Phase 3: Delete the current hunk range (reverse order) ─────
    for idx in (range_start..clamped_end).rev() {
        buffer.delete_line(idx);
    }

    // ── Phase 4: Insert the original lines ─────────────────────────
    for (i, content) in original_lines.iter().enumerate() {
        buffer.insert_at(
            CursorPosition::new(range_start + i, 0),
            &format!("{}\n", content),
        );
    }
}
