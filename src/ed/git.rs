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
}

impl GitExt for Editor {
    fn invalidate_git_gutter(&mut self) {
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

        let file_path = match file_path {
            Some(p) => p,
            None => {
                self.clear_git_gutter_state();
                return false;
            }
        };

        let file_path = file_path.canonicalize().unwrap_or(file_path);
        let file_dir = file_path.parent().unwrap_or(file_path.as_path());

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
                    self.clear_git_gutter_state();
                    return false;
                }
            }
        }

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
        self.cached_diff_hunks = editor_hunks;

        if let Some(w) = self.windows.active_window() {
            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                let ranges: Vec<HunkRange> = self
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
        if let Some(dirty_since) = self.git_gutter_dirty_since {
            if dirty_since.elapsed().as_millis() >= self.git_gutter_debounce_ms as u128 {
                self.ensure_git_gutter();
                if self.diff_popup.is_some() {
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

        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // Extract hunk information before borrowing self mutably
        let hunk_info = {
            let hunks = &self.cached_diff_hunks;

            let next_idx = hunks
                .iter()
                .enumerate()
                .find(|(_, h)| h.start > cursor_line)
                .map(|(i, _)| i);

            match next_idx {
                Some(idx) => {
                    let editor_hunk = &hunks[idx];
                    let target_line = self
                        .first_changed_line_in_hunk(buffer_id, editor_hunk)
                        .unwrap_or(editor_hunk.start);

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
                self.diff_popup = None;
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

        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // Extract hunk information before borrowing self mutably
        let hunk_info = {
            let hunks = &self.cached_diff_hunks;

            let current_idx = hunks
                .iter()
                .position(|h| cursor_line >= h.start && cursor_line < h.end);

            let prev_idx = match current_idx {
                Some(0) => None,
                Some(idx) => Some(idx - 1),
                None => hunks
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, h)| h.start < cursor_line)
                    .map(|(i, _)| i),
            };

            match prev_idx {
                Some(idx) => {
                    let editor_hunk = &hunks[idx];
                    let target_line = self
                        .last_changed_line_in_hunk(buffer_id, editor_hunk)
                        .unwrap_or(editor_hunk.start);

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
                self.diff_popup = None;
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

        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);

        let hunk = match self
            .cached_diff_hunks
            .iter()
            .find(|h| cursor_line >= h.start && cursor_line < h.end)
            .map(|h| h.diff.clone())
        {
            Some(h) => h,
            None => return CommandResult::Error("Cursor is not in a hunk.".to_string()),
        };

        let restore_cursor_line = hunk_insert_point(&hunk);

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

        self.diff_popup = None;
        self.invalidate_git_gutter();
        self.lsp_did_open(&file_path);
        self.ensure_cursor_visible_all();
        self.dirty.mark_all();

        CommandResult::Message("Hunk reverted in buffer (not saved to disk).".to_string())
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

        let hunk_idx = self
            .cached_diff_hunks
            .iter()
            .position(|h| cursor_line >= h.start.saturating_sub(1) && cursor_line < h.end);

        match hunk_idx {
            Some(idx) => {
                let hunk = &self.cached_diff_hunks[idx].diff;
                let popup = Self::build_diff_popup(hunk);
                self.diff_popup = Some(popup);
            }
            None => {
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

        let _buffer_id = self.windows.active_window().map(|w| w.buffer_id);

        let hunk_idx = self
            .cached_diff_hunks
            .iter()
            .position(|h| cursor_line >= h.start.saturating_sub(1) && cursor_line < h.end);

        match hunk_idx {
            Some(idx) => {
                let hunk = &self.cached_diff_hunks[idx].diff;
                self.diff_popup = Some(Self::build_diff_popup(hunk));
            }
            None => {
                if self.diff_mode_active {
                    self.diff_popup = None;
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
        (hunk.start..end)
            .rev()
            .find(|&line| buffer.git_gutter.sign_at(line).is_some())
    }

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
                buf.git_gutter
                    .update_with_diff_signs(vec![], std::collections::HashMap::new());
            }
        }
        self.dirty.mark_all();
    }
}

/// Build the original (HEAD) text for a hunk's region.
fn reconstruct_original_lines(hunk: &DiffHunk) -> Vec<String> {
    hunk.lines
        .iter()
        .filter(|dl| dl.type_ != DiffLineType::Add)
        .map(|dl| {
            dl.content
                .strip_prefix(' ')
                .or_else(|| dl.content.strip_prefix('-'))
                .unwrap_or(&dl.content)
                .to_string()
        })
        .collect()
}

fn hunk_buffer_range(hunk: &DiffHunk) -> Option<(usize, usize)> {
    let mut min_line = usize::MAX;
    let mut max_line = 0;

    for dl in &hunk.lines {
        match dl.type_ {
            DiffLineType::Add | DiffLineType::Context => {
                if let Some(nl) = dl.new_lineno {
                    let idx = nl.saturating_sub(1);
                    min_line = min_line.min(idx);
                    max_line = max_line.max(idx);
                }
            }
            DiffLineType::Delete | DiffLineType::HunkHeader => {}
        }
    }

    if min_line == usize::MAX {
        None
    } else {
        Some((min_line, max_line + 1))
    }
}

// ── Hunk revert helpers ─────────────────────────────────────────────

/// Compute the buffer insertion point for deleted lines within a hunk.
///
/// Deleted lines have no `new_lineno` (they don't exist in the working tree).
/// We must insert them at the position of the first surviving line that
/// immediately follows the deletion in the original sequence.
///
/// Falls back to one past the last `new_lineno` in the hunk for
/// trailing-delete hunks (EOF deletions).
fn hunk_insert_point(hunk: &DiffHunk) -> usize {
    let mut after_delete = false;

    for dl in &hunk.lines {
        match dl.type_ {
            DiffLineType::Delete => {
                after_delete = true;
            }
            DiffLineType::Add | DiffLineType::Context => {
                if after_delete {
                    if let Some(nl) = dl.new_lineno {
                        return nl.saturating_sub(1);
                    }
                }
            }
            DiffLineType::HunkHeader => {}
        }
    }

    // Trailing-delete fallback: insert after the last surviving line.
    hunk.lines
        .iter()
        .filter_map(|dl| dl.new_lineno)
        .last()
        .map(|n| n.saturating_sub(1) + 1)
        .unwrap_or(0)
}

/// Collect lines to insert (Delete lines → original content).
fn hunk_lines_to_restore(hunk: &DiffHunk) -> Vec<String> {
    hunk.lines
        .iter()
        .filter(|dl| dl.type_ == DiffLineType::Delete)
        .map(|dl| {
            dl.content
                .strip_prefix('-')
                .unwrap_or(&dl.content)
                .to_string()
        })
        .collect()
}

/// Collect buffer line indices to remove (Add lines → should not exist).
/// Returned in reverse order so deletions don't shift subsequent indices.
fn hunk_lines_to_remove(hunk: &DiffHunk) -> Vec<usize> {
    let mut indices: Vec<usize> = hunk
        .lines
        .iter()
        .filter(|dl| dl.type_ == DiffLineType::Add)
        .filter_map(|dl| dl.new_lineno)
        .map(|nl| nl.saturating_sub(1))
        .collect();

    indices.sort_unstable();
    indices.reverse();
    indices
}

/// Apply a hunk revert directly to a buffer:
/// - Remove Add lines (reverse order to preserve indices).
/// - Insert Delete lines at the correct position.
/// - Never touch Context lines.
fn apply_hunk_revert(buffer: &mut Buffer, hunk: &DiffHunk) {
    let insert_point = hunk_insert_point(hunk);
    let to_restore = hunk_lines_to_restore(hunk);
    let to_remove = hunk_lines_to_remove(hunk);

    for idx in to_remove {
        if idx < buffer.line_count() {
            buffer.delete_line(idx);
        }
    }

    for (offset, content) in to_restore.iter().enumerate() {
        let pos = CursorPosition::new(insert_point + offset, 0);
        buffer.insert_at(pos, &format!("{}\n", content));
    }
}
