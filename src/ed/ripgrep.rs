//--+ ed/ripgrep.rs
// src/ed/ripgrep.rs
//! Ripgrep integration: searching, result navigation, and result buffer management.

use serde_json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::buffer::{Buffer, BufferId, BufferKind};
use crate::config::Config;
use crate::ed::build::BuildExt;
use crate::ed::file_ops::FileOpsExt;
use crate::ed::movement::MovementExt;
use crate::editor::{CommandResult, Editor, Mode};
use crate::misc::find_git_root;
use crate::ripgrep;
use crate::ripgrep::RipgrepOutput;
use crate::window::Viewport;

/// File name for persisted last rg output.
const LAST_RG_FILE: &str = "last_rg.json";

/// Save the last ripgrep output to disk.
fn save_last_rg_output(output: &RipgrepOutput) -> Result<(), String> {
    let config_dir = Config::config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&config_dir).map_err(|e| format!("Cannot create config dir: {}", e))?;
    let path = config_dir.join(LAST_RG_FILE);
    let data = serde_json::to_vec(output).map_err(|e| format!("Serialize error: {}", e))?;
    fs::write(&path, data).map_err(|e| format!("Write error: {}", e))?;
    Ok(())
}

/// Load the last ripgrep output from disk.
fn load_last_rg_output() -> Option<RipgrepOutput> {
    let config_dir = Config::config_dir().ok()?;
    let path = config_dir.join(LAST_RG_FILE);
    let data = fs::read(&path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Extension trait for ripgrep operations.
pub trait RipgrepExt {
    /// Search for the word under the cursor using ripgrep.
    fn ripgrep_under_cursor(&mut self) -> CommandResult;

    /// Reopen the last ripgrep results buffer (no re-search).
    fn ripgrep_last(&mut self) -> CommandResult;

    /// Re-run the last ripgrep search.
    fn ripgrep_last_rerun(&mut self) -> CommandResult;

    /// Jump to the result under the cursor in a ripgrep buffer.
    fn ripgrep_goto_result(&mut self) -> CommandResult;

    /// Close the ripgrep buffer if the active window is viewing one.
    fn ripgrep_close_buffer(&mut self) -> CommandResult;

    /// Get or create the dedicated ripgrep buffer.
    fn get_or_create_ripgrep_buffer(&mut self) -> BufferId;

    /// Open a file at a specific 1‑based line number.
    fn open_file_at_line(&mut self, path: &Path, line: usize) -> Result<(), String>;
}

impl RipgrepExt for Editor {
    fn ripgrep_under_cursor(&mut self) -> CommandResult {
        let (pattern, root_dir) = {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => return CommandResult::Error("No active window".to_string()),
            };
            let buffer = match self.buffers.get(&window.buffer_id) {
                Some(b) => b,
                None => return CommandResult::Error("No active buffer".to_string()),
            };

            if buffer.kind == BufferKind::Ripgrep {
                self.ripgrep_close_buffer();
                let window = match self.windows.active_window() {
                    Some(w) => w,
                    None => return CommandResult::Error("No active window".to_string()),
                };
                let buffer = match self.buffers.get(&window.buffer_id) {
                    Some(b) => b,
                    None => return CommandResult::Error("No active buffer".to_string()),
                };
                let line_text = buffer.line_text(window.cursor.position.line).unwrap_or_default();
                let pat = ripgrep::word_under_cursor(&line_text, window.cursor.position.col);
                let dir = buffer
                    .file_path
                    .as_ref()
                    .and_then(|p| find_git_root(p))
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
                (pat, dir)
            } else {
                let line_text = buffer.line_text(window.cursor.position.line).unwrap_or_default();
                let pat = ripgrep::word_under_cursor(&line_text, window.cursor.position.col);
                let dir = buffer
                    .file_path
                    .as_ref()
                    .and_then(|p| find_git_root(p))
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
                (pat, dir)
            }
        };

        if pattern.is_empty() {
            return CommandResult::Error(
                "No word under cursor.\n\
             Move to a word and try again, or use :rg <pattern>."
                    .to_string(),
            );
        }

        self.set_status(format!("Searching for '{}'...", pattern));

        let escaped = ripgrep::escape_regex(&pattern);
        let rg_output = match ripgrep::run_ripgrep(&escaped, &root_dir) {
            Ok(output) => output,
            Err(e) => return CommandResult::Error(e),
        };

        // Cache for :lastrg
        self.last_rg_pattern = Some(pattern.clone());
        self.last_rg_root_dir = Some(root_dir.clone());
        self.last_rg_output = Some(rg_output.clone());

        self.populate_ripgrep_buffer(&pattern, rg_output)
    }

    fn ripgrep_last(&mut self) -> CommandResult {
        // ── If an RG buffer already exists, just switch to it (preserves position) ──
        let existing_rg = self.buffers.iter().find(|b| b.kind == BufferKind::Ripgrep).map(|b| b.id);

        if let Some(rg_id) = existing_rg {
            self.save_current_position();
            if let Some(window) = self.windows.active_window_mut() {
                window.set_buffer(rg_id);
            }
            self.restore_cursor_position();
            self.clamp_cursor_to_buffer(&rg_id);
            self.ensure_cursor_visible_all();
            self.dirty.mark_all();
            return CommandResult::ViewChanged;
        }

        // ── No RG buffer exists — load last state and populate ──
        if self.last_rg_output.is_none() {
            self.load_last_rg_state();
        }
        match &self.last_rg_output {
            Some(rg_output) => {
                let pattern = self.last_rg_pattern.clone().unwrap_or_default();
                self.populate_ripgrep_buffer(&pattern, rg_output.clone())
            }
            None => CommandResult::Error(
                "No previous ripgrep search.\n\
             Use K, grg, or :rg <pattern> first."
                    .to_string(),
            ),
        }
    }

    fn ripgrep_last_rerun(&mut self) -> CommandResult {
        if self.last_rg_pattern.is_none() || self.last_rg_root_dir.is_none() {
            self.load_last_rg_state();
        }
        let (pattern, root_dir) = match (&self.last_rg_pattern, &self.last_rg_root_dir) {
            (Some(p), Some(d)) => (p.clone(), d.clone()),
            _ => {
                return CommandResult::Error(
                    "No previous ripgrep search.\n\
                 Use k, grg, or :rg <pattern> first."
                        .to_string(),
                );
            }
        };

        self.set_status(format!("Re-searching for '{}'...", pattern));

        let escaped = ripgrep::escape_regex(&pattern);
        let rg_output = match ripgrep::run_ripgrep(&escaped, &root_dir) {
            Ok(output) => output,
            Err(e) => return CommandResult::Error(e),
        };

        // Update cache
        self.last_rg_output = Some(rg_output.clone());

        self.populate_ripgrep_buffer(&pattern, rg_output)
    }

    fn ripgrep_goto_result(&mut self) -> CommandResult {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return CommandResult::NoOp,
        };

        let buffer_id = window.buffer_id;
        let line_idx = window.cursor.position.line;

        let (file_path, line_number) = {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };

            if buffer.kind != BufferKind::Ripgrep {
                return CommandResult::NoOp;
            }

            let result_idx = match buffer.ripgrep_line_map.get(line_idx) {
                Some(Some(idx)) => *idx,
                _ => {
                    return CommandResult::Message("Cursor is on a header line — move to a match line and press Enter.".to_string());
                }
            };

            let result = &buffer.ripgrep_results[result_idx];
            (result.file_path.clone(), result.line_number)
        };

        match self.open_file_at_line(&file_path, line_number) {
            Ok(()) => {
                self.clear_messages();
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Err(e) => CommandResult::Error(format!("Cannot open {}: {}", file_path.display(), e)),
        }
    }

    fn ripgrep_close_buffer(&mut self) -> CommandResult {
        let (is_rg, _buffer_id) = {
            if let Some(window) = self.windows.active_window() {
                let buf_id = window.buffer_id;
                let is_rg = self.buffers.get(&buf_id).map(|b| b.kind == BufferKind::Ripgrep).unwrap_or(false);
                (is_rg, buf_id)
            } else {
                return CommandResult::NoOp;
            }
        };

        if !is_rg {
            return CommandResult::Message("Not in a ripgrep buffer".to_string());
        }

        let target_id = self
            .buffers
            .iter()
            .find(|b| b.kind == BufferKind::Normal && b.file_path.is_some())
            .or_else(|| self.buffers.iter().find(|b| b.kind == BufferKind::Normal))
            .map(|b| b.id);

        let target_id = match target_id {
            Some(id) => id,
            None => {
                let new_id = self.buffers.new_buffer();
                self.set_status("Created new buffer".to_string());
                new_id
            }
        };

        // ── Switch buffer ──
        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(target_id);
        }

        // ── Restore cursor ──
        self.restore_cursor_position();
        self.clamp_cursor_to_buffer(&target_id);

        // ── Explicitly rebuild viewport: center on cursor, clamp to buffer end ──
        // We do NOT rely on ensure_cursor_visible alone because the viewport
        // inherited from the RG buffer can be far past the original buffer's end,
        // and ensure_cursor_visible only adjusts if the cursor is outside the
        // current viewport — but with a huge scroll_line the math can still leave
        // the last line invisible.
        {
            let (cursor_line, line_count, edit_height) = {
                let window = self.windows.active_window().unwrap();
                let buffer = self.buffers.get(&target_id).unwrap();
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

        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn get_or_create_ripgrep_buffer(&mut self) -> BufferId {
        for buf in self.buffers.iter() {
            if buf.kind == BufferKind::Ripgrep {
                return buf.id;
            }
        }

        let mut buffer = Buffer::new();
        buffer.kind = BufferKind::Ripgrep;
        buffer.file_path = None;
        let id = buffer.id;
        self.buffers.insert(buffer);
        id
    }

    fn open_file_at_line(&mut self, path: &Path, line: usize) -> Result<(), String> {
        // Save the outgoing buffer's position (works for ALL buffer kinds,
        // including Ripgrep, Build, GitLog, etc.)
        self.save_current_position();

        let buffer_id = self.buffers.open_file(path).map_err(|e| e.to_string())?;

        // Cache git root for display_name() path stripping. This is normally
        // done in FileOpsExt::open_file, but we bypass it here to prevent
        // restore_cursor_position from overwriting the match line.
        if let Some(buf) = self.buffers.get_mut(&buffer_id) {
            if buf.git_root.is_none() {
                buf.git_root = crate::misc::find_git_root(path);
            }
        }

        if self.mode != Mode::Normal {
            self.enter_mode(Mode::Normal);
        }

        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(buffer_id);
        }

        let target_line = line.saturating_sub(1);
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position.line = target_line;
            window.cursor.position.col = 0;
            window.cursor.desired_col = None;
        }
        self.scroll_bottom_third();
        self.ensure_cursor_visible(&buffer_id);
        self.clamp_cursor_to_buffer(&buffer_id);
        self.dirty.mark_all();
        Ok(())
    }
}

/// Shared helper to populate the RG buffer with results and switch to it.
impl Editor {
    pub(crate) fn populate_ripgrep_buffer(&mut self, pattern: &str, rg_output: ripgrep::RipgrepOutput) -> CommandResult {
        let rg_buffer_id = self.get_or_create_ripgrep_buffer();

        self.save_current_position();

        if let Some(buffer) = self.buffers.get_mut(&rg_buffer_id) {
            let formatted = rg_output.format_for_buffer();
            buffer.rope = ropey::Rope::from_str(&formatted);
            buffer.ripgrep_results = rg_output.results.clone();
            self.quickfix_results = rg_output.results.clone();
            self.quickfix_index = 0; // ← MUST reset — otherwise next/prev panics
            buffer.ripgrep_line_map = rg_output.build_line_map();
            buffer.search_pattern = Some(pattern.to_string());
            buffer.dirty = false;
            buffer.clear_undo_history();
            buffer.file_path = Some(PathBuf::from(format!("*rg* {}", pattern)));
        }

        // Cache in memory and persist to disk
        self.last_rg_pattern = Some(pattern.to_string());
        self.last_rg_root_dir = Some(rg_output.root_dir.clone());
        self.last_rg_output = Some(rg_output.clone());
        if let Err(_e) = save_last_rg_output(&rg_output) {}

        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(rg_buffer_id);
            window.cursor = crate::window::WindowCursor::default();
            window.viewport = Viewport::new();
            window.selection_anchor = None;
        }

        let count = rg_output.results.len();
        let file_count = rg_output
            .results
            .iter()
            .map(|r| r.file_path.clone())
            .collect::<std::collections::HashSet<_>>()
            .len();

        if count == 0 {
            self.set_status(format!("No matches for '{}' in {}", pattern, rg_output.root_dir.display()));
        } else {
            self.set_status(format!(
                "[RG] '{}' — {} match{} in {} file{}  (Enter=jump, q=close)",
                pattern,
                count,
                if count == 1 { "" } else { "es" },
                file_count,
                if file_count == 1 { "" } else { "s" },
            ));
        }

        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn load_last_rg_state(&mut self) {
        if let Some(output) = load_last_rg_output() {
            self.last_rg_pattern = Some(output.pattern.clone());
            self.last_rg_root_dir = Some(output.root_dir.clone());
            self.last_rg_output = Some(output);
        }
    }

    // ── Ripgrep result navigation ─────────────────────────
    pub(crate) fn ripgrep_next_result(&mut self) -> CommandResult {
        self.quickfix_next()
    }

    pub(crate) fn ripgrep_prev_result(&mut self) -> CommandResult {
        self.quickfix_prev()
    }

    pub(crate) fn ripgrep_next_result_old(&mut self) -> CommandResult {
        if self.quickfix_results.is_empty() {
            self.set_infobar_message("No ripgrep results. Run :rg first.".to_string());
            self.dirty.mark_all();
            return CommandResult::Error("No results".to_string());
        }

        if self.quickfix_index + 1 >= self.quickfix_results.len() {
            self.set_status("Already at last result".to_string());
            self.dirty.mark_all();
            return CommandResult::ViewChanged;
        }

        self.quickfix_index += 1;
        // Extract owned data before mutable call
        let result = &self.quickfix_results[self.quickfix_index];
        let file_path = result.file_path.clone();
        let line_number = result.line_number;

        match self.open_file_at_line(&file_path, line_number) {
            Ok(()) => {
                // Use display_name() for the status message so it respects git_root stripping
                let buf_name = self
                    .current_buffer()
                    .map(|b| b.display_name())
                    .unwrap_or_else(|| file_path.display().to_string());
                self.set_status(format!(
                    "Result {}/{}: {}:{}",
                    self.quickfix_index + 1,
                    self.quickfix_results.len(),
                    buf_name,
                    line_number
                ));
                CommandResult::ViewChanged
            }
            Err(e) => CommandResult::Error(e),
        }
    }

    pub(crate) fn ripgrep_prev_result_old(&mut self) -> CommandResult {
        if self.quickfix_results.is_empty() {
            self.set_infobar_message("No ripgrep results. Run :rg first.".to_string());
            self.dirty.mark_all();
            return CommandResult::Error("No results".to_string());
        }

        if self.quickfix_index == 0 {
            self.set_status("Already at first result".to_string());
            self.dirty.mark_all();
            return CommandResult::ViewChanged;
        }

        self.quickfix_index -= 1;
        // Extract owned data before mutable call
        let result = &self.quickfix_results[self.quickfix_index];
        let file_path = result.file_path.clone();
        let line_number = result.line_number;

        match self.open_file_at_line(&file_path, line_number) {
            Ok(()) => {
                // Use display_name() for the status message so it respects git_root stripping
                let buf_name = self
                    .current_buffer()
                    .map(|b| b.display_name())
                    .unwrap_or_else(|| file_path.display().to_string());
                self.set_status(format!(
                    "Result {}/{}: {}:{}",
                    self.quickfix_index + 1,
                    self.quickfix_results.len(),
                    buf_name,
                    line_number
                ));
                CommandResult::ViewChanged
            }
            Err(e) => CommandResult::Error(e),
        }
    }
}
