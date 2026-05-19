// src/ed/marks.rs
//! Mark (bookmark) system for quick navigation.
//!
//! - `m{a-z}`  → set mark at current position
//! - `` `{a-z} `` → jump to mark (exact column)
//! - `` `` ``   → jump back (toggles with last jump position)
//!
//! All long-range jumps (`gd`, ctags, LSP goto-def) push to the **unified
//! tag/jump stack** via `push_jump_position()`, so both `` `` `` (toggle)
//! and `:pop` / `Ctrl-T` (deep stack unwind) work regardless of which
//! mechanism performed the jump.

use crate::ed::movement::MovementExt;
use crate::editor::{CommandResult, Editor};

pub trait MarksExt {
    /// Set a named mark (a-z) at the current cursor position.
    fn set_mark(&mut self, name: char) -> CommandResult;

    /// Jump to a named mark. Switches buffer if the mark is in a different file.
    fn goto_mark(&mut self, name: char) -> CommandResult;

    /// Jump back to the position before the last `gd` / tag / LSP jump.
    /// Uses `last_jump_mark` for toggle (`` pressed repeatedly alternates
    /// between two positions). Falls back to the unified tag stack if
    /// `last_jump_mark` is empty.
    fn jump_back(&mut self) -> CommandResult;

    /// Push the current position onto the unified jump stack.
    /// Called automatically before `gd`, ctag jumps, and LSP goto-def.
    fn save_jump_mark(&mut self);
}

impl MarksExt for Editor {
    fn set_mark(&mut self, name: char) -> CommandResult {
        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            self.search.marks.insert(name, (buffer_id, pos));
            self.dirty.mark_all();
        }
        CommandResult::Message(format!("Mark '{}' set", name))
    }

    fn goto_mark(&mut self, name: char) -> CommandResult {
        let (target_buf_id, target_pos) = match self.search.marks.get(&name) {
            Some(&(bid, pos)) => (bid, pos),
            None => return CommandResult::Error(format!("Mark '{}' not set", name)),
        };

        // Verify the buffer still exists
        if self.buffers.get(&target_buf_id).is_none() {
            self.search.marks.remove(&name);
            return CommandResult::Error(format!("Mark '{}' buffer closed", name));
        }

        // Save current position before jumping (so `` can return here)
        self.save_jump_mark();

        // Switch buffer if needed
        if let Some(window) = self.windows.active_window() {
            if window.buffer_id != target_buf_id {
                if let Some(w) = self.windows.active_window_mut() {
                    w.set_buffer(target_buf_id);
                }
            }
        }

        // Move to the mark position
        self.move_to_position(target_pos.line, target_pos.col);
        self.ensure_cursor_visible_all();
        self.dirty.mark_all();

        CommandResult::Message(format!("Jumped to mark '{}'", name))
    }

    fn jump_back(&mut self) -> CommandResult {
        // ── Primary: last_jump_mark toggle ──
        // Swapping current ↔ last_jump_mark gives the classic Vim ``
        // toggle: pressing `` repeatedly alternates between two positions.
        if let Some((target_buf_id, target_pos)) = self.search.last_jump_mark.take() {
            // Verify the target buffer still exists
            if self.buffers.get(&target_buf_id).is_none() {
                // Buffer was closed; discard and fall through to tag stack
                self.search.last_jump_mark = None;
            } else {
                // Save CURRENT position as the new last_jump_mark (toggle)
                let current = self.windows.active_window().map(|w| (w.buffer_id, w.cursor.position));
                self.search.last_jump_mark = current;

                // Switch buffer if needed
                if let Some(window) = self.windows.active_window() {
                    if window.buffer_id != target_buf_id {
                        if let Some(w) = self.windows.active_window_mut() {
                            w.set_buffer(target_buf_id);
                        }
                    }
                }

                self.move_to_position(target_pos.line, target_pos.col);
                self.ensure_cursor_visible_all();
                self.dirty.mark_all();

                return CommandResult::Message("Jumped back".into());
            }
        }

        // ── Fallback: unified tag/jump stack ──
        // When last_jump_mark is empty (e.g. first `` after startup, or
        // after the toggle buffer was invalidated), use the tag stack
        // which contains the full history of gd / ctag / LSP jumps.
        if self.search.tag_manager.stack_size() > 0 {
            return crate::ed::tag::tag_pop(self);
        }

        CommandResult::Error("No previous jump position".into())
    }

    fn save_jump_mark(&mut self) {
        self.push_jump_position();
    }
}
