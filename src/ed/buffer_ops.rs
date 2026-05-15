// src/ed/buffer_ops.rs
//! Buffer switching operations (next/previous buffer).

use crate::buffer::BufferId;
use crate::ed::file_ops::FileOpsExt;
use crate::editor::Editor;
use crate::popup::MruPopup;
use crate::CommandResult;

/// Extension trait for buffer switching operations.
pub trait BufferOpsExt {
    /// Switch to the next buffer (cyclic through all buffers).
    fn next_buffer(&mut self) -> CommandResult;

    /// Switch to the previous buffer (cyclic).
    fn prev_buffer(&mut self) -> CommandResult;

    fn list_buffers(&mut self) -> CommandResult;
    fn word_under_cursor_in_current_buffer(&mut self) -> String;

    /// Delete the current buffer.
    /// If `force` is false and the buffer has unsaved changes, returns an error.
    /// Ephemeral buffers (Ripgrep, GitDiff, Llm) are always deletable.
    fn delete_buffer(&mut self, force: bool) -> CommandResult;
    fn open_mru(&mut self) -> CommandResult;
}

impl BufferOpsExt for Editor {
    fn list_buffers(&mut self) -> CommandResult {
        use crate::popup::{BufferListEntry, BufferListPopup};

        let active_buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        let entries: Vec<BufferListEntry> = self
            .buffers
            .iter()
            .map(|buffer| BufferListEntry {
                id: buffer.id,
                name: buffer.display_name(),
                dirty: buffer.dirty,
                active: active_buffer_id == Some(buffer.id),
            })
            .collect();

        if entries.is_empty() {
            return CommandResult::Message("No buffers open".into());
        }

        self.buffer_list_popup = Some(BufferListPopup::new(entries));
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn next_buffer(&mut self) -> CommandResult {
        let current_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };

        // Save current cursor position before switching
        self.save_current_position();

        let mut ids: Vec<BufferId> = self.buffers.iter().map(|b| b.id).collect();
        if ids.len() <= 1 {
            return CommandResult::Message("Only one buffer open".into());
        }
        ids.sort();

        let pos = ids.iter().position(|&id| id == current_id).unwrap_or(0);
        let next_id = ids[(pos + 1) % ids.len()];

        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(next_id);
        }

        // Restore saved position (no-op if none) then ensure cursor is visible
        self.restore_cursor_position();
        self.clamp_cursor_to_buffer(&next_id);
        self.ensure_cursor_visible_all();

        self.dirty.mark_all();
        let buf_name = self
            .buffers
            .get(&next_id)
            .map(|b| b.display_name())
            .unwrap_or_else(|| "?".into());
        CommandResult::Message(format!("Switched to buffer: {}", buf_name))
    }

    fn prev_buffer(&mut self) -> CommandResult {
        let current_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };

        // Save current cursor position before switching
        self.save_current_position();

        let mut ids: Vec<BufferId> = self.buffers.iter().map(|b| b.id).collect();
        if ids.len() <= 1 {
            return CommandResult::Message("Only one buffer open".into());
        }
        ids.sort();

        let pos = ids.iter().position(|&id| id == current_id).unwrap_or(0);
        let prev_id = if pos == 0 {
            ids[ids.len() - 1]
        } else {
            ids[pos - 1]
        };

        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(prev_id);
        }

        // Restore saved position (no-op if none) then ensure cursor is visible
        self.restore_cursor_position();
        self.clamp_cursor_to_buffer(&prev_id);
        self.ensure_cursor_visible_all();

        self.dirty.mark_all();
        let buf_name = self
            .buffers
            .get(&prev_id)
            .map(|b| b.display_name())
            .unwrap_or_else(|| "?".into());
        CommandResult::Message(format!("Switched to buffer: {}", buf_name))
    }

    /// Get the word under the cursor in the current normal buffer (not ripgrep buffer)
    fn word_under_cursor_in_current_buffer(&mut self) -> String {
        if let Some(buffer) = self.current_buffer() {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => return String::new(),
            };
            let line_idx = window.cursor.position.line;
            let col_idx = window.cursor.position.col;
            if let Some(line_text) = buffer.line_text(line_idx) {
                // Use the existing public function from ripgrep module
                return crate::ripgrep::word_under_cursor(&line_text, col_idx);
            }
        }
        String::new()
    }

    fn delete_buffer(&mut self, force: bool) -> CommandResult {
        let current_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };

        // Gather info about the current buffer before any mutation
        let (is_dirty, is_ephemeral, has_file_path, buf_name) = match self.buffers.get(&current_id)
        {
            Some(b) => (
                b.dirty,
                b.kind.is_ephemeral(),
                b.file_path.is_some(),
                b.display_name(),
            ),
            None => return CommandResult::Error("Buffer not found.".into()),
        };

        // Check for unsaved changes — ephemeral buffers bypass this check
        if !is_ephemeral && is_dirty && !force {
            return CommandResult::Error(
                "Buffer has unsaved changes! Use :bd! to force delete.".into(),
            );
        }

        // Collect sorted buffer IDs to determine the replacement buffer
        let mut ids: Vec<BufferId> = self.buffers.iter().map(|b| b.id).collect();
        ids.sort();

        // Find the replacement buffer (prefer one with a file_path)
        let next_id = if ids.len() <= 1 {
            self.buffers.new_buffer()
        } else {
            // Prefer switching to a Normal buffer with a file_path
            let preferred = self
                .buffers
                .iter()
                .find(|b| {
                    b.id != current_id
                        && b.kind == crate::buffer::BufferKind::Normal
                        && b.file_path.is_some()
                })
                .or_else(|| {
                    self.buffers
                        .iter()
                        .find(|b| b.id != current_id && b.kind == crate::buffer::BufferKind::Normal)
                })
                .or_else(|| self.buffers.iter().find(|b| b.id != current_id))
                .map(|b| b.id);

            preferred.unwrap_or_else(|| self.buffers.new_buffer())
        };

        // ── Save position of outgoing buffer, switch, restore incoming ──
        //
        // This mirrors the save/restore pattern used by next_buffer,
        // prev_buffer, ripgrep_open, git_log_open, and git_status_open.
        self.save_current_position();

        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(next_id);
        }

        self.restore_cursor_position();
        self.clamp_cursor_to_buffer(&next_id);
        self.ensure_cursor_visible_all();

        // Now remove the deleted buffer
        self.buffers.remove(&current_id);

        self.dirty.mark_all();

        let next_name = self
            .buffers
            .get(&next_id)
            .map(|b| b.display_name())
            .unwrap_or_else(|| "[No Name]".into());

        CommandResult::Message(format!(
            "Deleted buffer '{}'. Switched to: {}",
            buf_name, next_name
        ))
    }
    /// Open the MRU (Most Recently Used) popup.
    fn open_mru(&mut self) -> CommandResult {
        let entries = self.mru.get_entries();

        if entries.is_empty() {
            return CommandResult::Message("No recent files".to_string());
        }

        self.mru_popup = Some(MruPopup::new(entries));
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }
}
