//--+ ed/movement.rs
// src/ed/movement.rs
//! Editor movement and scrolling extensions.

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::CursorPosition;
use crate::editor::Editor;
use crate::CommandResult;

/// Extension trait for cursor movement and viewport scrolling.
pub trait MovementExt {
    // ── Basic cursor movement (count-aware in caller) ──
    fn move_cursor_left(&mut self) -> CommandResult;
    fn move_cursor_right(&mut self) -> CommandResult;
    fn move_cursor_up(&mut self) -> CommandResult;
    fn move_cursor_down(&mut self) -> CommandResult;
    fn match_bracket(&mut self) -> CommandResult;

    // ── Word movement ──
    fn move_word_forward(&mut self) -> CommandResult;
    fn move_word_back(&mut self) -> CommandResult;
    fn move_word_end(&mut self) -> CommandResult;

    // ── Line / file movement ──
    fn move_line_start(&mut self) -> CommandResult;
    fn move_line_end(&mut self) -> CommandResult;
    fn move_to_column(&mut self, col: usize) -> CommandResult;
    fn move_file_start(&mut self) -> CommandResult;
    fn move_file_end(&mut self) -> CommandResult;
    fn move_to_line(&mut self, line: usize) -> CommandResult;
    fn move_to_position(&mut self, line: usize, col: usize) -> CommandResult;

    // ── Scrolling ──
    fn scroll_up(&mut self) -> CommandResult;
    fn scroll_down(&mut self) -> CommandResult;
    fn page_up(&mut self) -> CommandResult;
    fn page_down(&mut self) -> CommandResult;
    fn scroll_left(&mut self) -> CommandResult;
    fn scroll_right(&mut self) -> CommandResult;
    fn scroll_center(&mut self) -> CommandResult;
    fn scroll_bottom_third(&mut self) -> CommandResult;

    // ── Viewport helpers (called after movement) ──
    fn ensure_cursor_visible(&mut self, buffer_id: &crate::buffer::BufferId);
    fn clamp_cursor_to_buffer(&mut self, buffer_id: &crate::buffer::BufferId);
    fn move_cursor_to_first_nonblank_of(&mut self, line: usize);
}

impl MovementExt for Editor {
    /// Move the cursor to the first non-blank grapheme on the given line.
    /// Falls back to column 0 if the line is blank.
    fn move_cursor_to_first_nonblank_of(&mut self, line: usize) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                let col = buffer
                    .line_text(line)
                    .map(|text| {
                        text.graphemes(true)
                            .position(|g| !g.trim().is_empty())
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                window.cursor.position = CursorPosition::new(line, col);
                window.cursor.desired_col = None;
            }
        }
    }
    fn move_cursor_left(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            if pos.col > 0 {
                pos.col -= 1;
            } else if pos.line > 0 {
                pos.line -= 1;
                pos.col = self
                    .buffers
                    .get(&buffer_id)
                    .map(|b| b.line_len(pos.line))
                    .unwrap_or(0);
            }
            window.cursor.desired_col = None;
        }
        CommandResult::ViewChanged
    }

    fn move_cursor_right(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            let line_len = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_len(pos.line))
                .unwrap_or(0);

            if pos.col < line_len {
                pos.col += 1;
            } else if pos.line + 1
                < self
                    .buffers
                    .get(&buffer_id)
                    .map(|b| b.line_count())
                    .unwrap_or(0)
            {
                pos.line += 1;
                pos.col = 0;
            }
            window.cursor.desired_col = None;
        }
        CommandResult::ViewChanged
    }

    fn move_cursor_down(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            let desired = window.cursor.desired_col.unwrap_or(pos.col);
            let max_line = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_count())
                .unwrap_or(0);

            if pos.line + 1 < max_line {
                pos.line += 1;
            }

            let max_col = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_len(pos.line))
                .unwrap_or(0);
            pos.col = desired.min(max_col);
            window.cursor.desired_col = Some(desired);

            // ── Scroll offset: keep at least `scroll_offset` lines below cursor ──
            let scroll_offset = self.config.scroll_offset;
            let edit_height = (window.height as usize).saturating_sub(1);
            if scroll_offset > 0 && edit_height > 2 * scroll_offset {
                // lower_bound = minimum scroll_line so that
                //   (scroll_line + edit_height - 1) - cursor_line >= scroll_offset
                let lower_bound = (pos.line + scroll_offset + 1).saturating_sub(edit_height);
                let max_scroll = max_line.saturating_sub(edit_height.min(max_line));
                window.viewport.scroll_line =
                    window.viewport.scroll_line.max(lower_bound).min(max_scroll);
            }
        }
        CommandResult::ViewChanged
    }

    fn move_cursor_up(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            let desired = window.cursor.desired_col.unwrap_or(pos.col);

            if pos.line > 0 {
                pos.line -= 1;
            }

            let max_col = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_len(pos.line))
                .unwrap_or(0);
            pos.col = desired.min(max_col);
            window.cursor.desired_col = Some(desired);

            // ── Scroll offset: keep at least `scroll_offset` lines above cursor ──
            let scroll_offset = self.config.scroll_offset;
            let edit_height = (window.height as usize).saturating_sub(1);
            if scroll_offset > 0 && edit_height > 2 * scroll_offset {
                // upper_bound = maximum scroll_line so that
                //   cursor_line - scroll_line >= scroll_offset
                let upper_bound = pos.line.saturating_sub(scroll_offset);
                window.viewport.scroll_line = window.viewport.scroll_line.min(upper_bound);
            }
        }
        CommandResult::ViewChanged
    }

    fn move_word_forward(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                if let Some(line_text) = buffer.line_text(pos.line) {
                    let graphemes: Vec<_> = line_text.graphemes(true).collect();
                    let mut col = pos.col;

                    // Skip current word chars.
                    while col < graphemes.len() && is_word_char(graphemes[col]) {
                        col += 1;
                    }
                    // Skip whitespace/punctuation.
                    while col < graphemes.len() && !is_word_char(graphemes[col]) {
                        col += 1;
                    }
                    pos.col = col;
                    window.cursor.desired_col = None;
                }
            }
        }
        CommandResult::ViewChanged
    }

    fn move_word_back(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                if pos.col == 0 && pos.line > 0 {
                    pos.line -= 1;
                    pos.col = buffer.line_len(pos.line);
                }
                if let Some(line_text) = buffer.line_text(pos.line) {
                    let graphemes: Vec<_> = line_text.graphemes(true).collect();
                    let mut col = pos.col.saturating_sub(1);

                    // Skip whitespace/punctuation.
                    while col > 0 && !is_word_char(graphemes[col]) {
                        col -= 1;
                    }
                    // Skip word chars.
                    while col > 0 && is_word_char(graphemes[col.saturating_sub(1)]) {
                        col -= 1;
                    }
                    pos.col = col;
                    window.cursor.desired_col = None;
                }
            }
        }
        CommandResult::ViewChanged
    }

    fn move_word_end(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                if let Some(line_text) = buffer.line_text(pos.line) {
                    let graphemes: Vec<_> = line_text.graphemes(true).collect();
                    let mut col = pos
                        .col
                        .saturating_sub(1)
                        .min(graphemes.len().saturating_sub(1));

                    // Skip whitespace.
                    while col + 1 < graphemes.len() && !is_word_char(graphemes[col + 1]) {
                        col += 1;
                    }
                    // Skip word chars.
                    while col + 1 < graphemes.len() && is_word_char(graphemes[col + 1]) {
                        col += 1;
                    }
                    pos.col = col;
                    window.cursor.desired_col = None;
                }
            }
        }
        CommandResult::ViewChanged
    }

    fn move_line_start(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position.col = 0;
            window.cursor.desired_col = None;
        }
        CommandResult::ViewChanged
    }

    fn move_line_end(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let line = window.cursor.position.line;
            let line_len = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_len(line)) // no subtract
                .unwrap_or(0);
            window.cursor.position.col = line_len;
            window.cursor.desired_col = None;
        }
        CommandResult::ViewChanged
    }

    fn move_to_column(&mut self, col: usize) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let line_len = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_len(window.cursor.position.line))
                .unwrap_or(0);
            window.cursor.position.col = col.min(line_len);
            window.cursor.desired_col = None;
        }
        CommandResult::ViewChanged
    }

    fn move_file_start(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position = CursorPosition::zero();
            window.cursor.desired_col = None;
            window.viewport.scroll_line = 0;
            window.viewport.scroll_col = 0;
        }
        CommandResult::ViewChanged
    }

    fn move_file_end(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let last_line = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_count().saturating_sub(1))
                .unwrap_or(0);
            window.cursor.position.line = last_line;
            window.cursor.position.col = 0;
            window.cursor.desired_col = None;
        }
        CommandResult::ViewChanged
    }

    fn move_to_line(&mut self, line: usize) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let max_line = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_count().saturating_sub(1))
                .unwrap_or(0);
            window.cursor.position.line = line.saturating_sub(1).min(max_line);
            window.cursor.position.col = 0;
            window.cursor.desired_col = None;
        }
        CommandResult::ViewChanged
    }

    fn move_to_position(&mut self, line: usize, col: usize) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position = CursorPosition::new(line, col);
            window.cursor.desired_col = None;
        }
        CommandResult::ViewChanged
    }

    // ── Scrolling ──

    fn scroll_up(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let half = (window.height as usize) / 2;
            window.viewport.scroll_line = window.viewport.scroll_line.saturating_sub(half);
            let cursor_screen_line = window
                .cursor
                .position
                .line
                .saturating_sub(window.viewport.scroll_line);
            window.cursor.position.line =
                window.viewport.scroll_line + cursor_screen_line.min(half.saturating_sub(1));
            self.clamp_cursor_to_buffer(&buffer_id);
        }
        CommandResult::ViewChanged
    }

    fn scroll_down(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let max_line = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_count())
                .unwrap_or(0);
            let half = (window.height as usize) / 2;
            window.viewport.scroll_line = (window.viewport.scroll_line + half).min(max_line);
            let cursor_screen_line = window
                .cursor
                .position
                .line
                .saturating_sub(window.viewport.scroll_line.saturating_sub(half));
            window.cursor.position.line =
                window.viewport.scroll_line + cursor_screen_line.min(half.saturating_sub(1));
            self.clamp_cursor_to_buffer(&buffer_id);
        }
        CommandResult::ViewChanged
    }

    fn page_up(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let page = (window.height as usize).saturating_sub(1);
            window.viewport.scroll_line = window.viewport.scroll_line.saturating_sub(page);
            // Place cursor at the bottom of the new viewport
            window.cursor.position.line = window.viewport.scroll_line + page;
            self.clamp_cursor_to_buffer(&buffer_id);
        }
        CommandResult::ViewChanged
    }

    fn page_down(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let max_line = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_count())
                .unwrap_or(0);
            let page = (window.height as usize).saturating_sub(1);
            window.viewport.scroll_line = (window.viewport.scroll_line + page).min(max_line);
            window.cursor.position.line = window.viewport.scroll_line;
            self.clamp_cursor_to_buffer(&buffer_id);
        }
        CommandResult::ViewChanged
    }

    fn scroll_left(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let shift = 4u16;
            window.viewport.scroll_col = window.viewport.scroll_col.saturating_sub(shift);
        }
        CommandResult::ViewChanged
    }

    fn scroll_right(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let shift = 4u16;
            window.viewport.scroll_col = window.viewport.scroll_col.saturating_add(shift);
        }
        CommandResult::ViewChanged
    }

    fn scroll_center(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let half = (window.height as usize) / 2;
            let cursor_line = window.cursor.position.line;
            window.viewport.scroll_line = cursor_line.saturating_sub(half);
        }
        CommandResult::ViewChanged
    }

    fn scroll_bottom_third(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let third = window.height as usize / 3;
            let cursor_line = window.cursor.position.line;
            // Place cursor at bottom third => 2/3 down from top
            window.viewport.scroll_line = cursor_line.saturating_sub(2 * third);
        }
        CommandResult::ViewChanged
    }
    // ── Viewport helpers (used inside movement methods) ──

    fn ensure_cursor_visible(&mut self, buffer_id: &crate::buffer::BufferId) {
        let max_line = self
            .buffers
            .get(buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        if let Some(window) = self.windows.active_window_mut() {
            window.ensure_cursor_visible(max_line);
        }
    }

    fn clamp_cursor_to_buffer(&mut self, buffer_id: &crate::buffer::BufferId) {
        if let Some(window) = self.windows.active_window_mut() {
            if let Some(buffer) = self.buffers.get(buffer_id) {
                let max_line = buffer.line_count().saturating_sub(1);
                window.cursor.position.line = window.cursor.position.line.min(max_line);
                let line_len = buffer.line_len(window.cursor.position.line);
                window.cursor.position.col = window.cursor.position.col.min(line_len);
            }
        }
    }

    fn match_bracket(&mut self) -> CommandResult {
        // First, get the buffer_id and cursor position without holding a mutable borrow
        let (buffer_id, cursor_pos) = {
            let window = match self.windows.active_window() {
                // Use immutable borrow
                Some(w) => w,
                None => return CommandResult::NoOp,
            };
            (window.buffer_id, window.cursor.position)
        };

        // Now get the buffer and compute the match
        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return CommandResult::NoOp,
        };

        let line_text = buffer.line_text(cursor_pos.line).unwrap_or_default();
        let chars: Vec<char> = line_text.chars().collect();

        if cursor_pos.col >= chars.len() {
            return CommandResult::NoOp;
        }

        let bracket = chars[cursor_pos.col];
        let (open_char, close_char, is_opening) = match bracket {
            '(' => ('(', ')', true),
            ')' => ('(', ')', false),
            '[' => ('[', ']', true),
            ']' => ('[', ']', false),
            '{' => ('{', '}', true),
            '}' => ('{', '}', false),
            '"' => ('"', '"', true),
            '\'' => ('\'', '\'', true),
            _ => return CommandResult::NoOp,
        };

        // For quotes, only match on the same line
        if bracket == '"' || bracket == '\'' {
            let mut target_col = None;
            let count = 0;
            for (i, &c) in chars.iter().enumerate().skip(cursor_pos.col + 1) {
                if c == bracket {
                    if count == 0 {
                        target_col = Some(i);
                        break;
                    }
                } else if c == '\\' && i + 1 < chars.len() {
                    continue;
                }
            }

            if let Some(col) = target_col {
                // Now get mutable access to update the cursor
                if let Some(window) = self.windows.active_window_mut() {
                    window.cursor.position.col = col;
                    window.cursor.desired_col = None;
                    self.ensure_cursor_visible(&buffer_id);
                }
                return CommandResult::ViewChanged;
            }
            return CommandResult::NoOp;
        }

        // For brackets, scan the whole buffer
        let full_text = buffer.text();
        let char_idx =
            buffer.rope.line_to_char(cursor_pos.line) + line_text[..cursor_pos.col].chars().count();

        let new_pos = if !is_opening {
            // closing bracket: scan backwards
            let mut level = 1;
            let match_char = open_char;
            let mut found_idx = None;
            let mut current_idx = char_idx;

            while current_idx > 0 {
                current_idx -= 1;
                let c = full_text.chars().nth(current_idx).unwrap();
                if c == match_char {
                    level -= 1;
                    if level == 0 {
                        found_idx = Some(current_idx);
                        break;
                    }
                } else if c == bracket {
                    level += 1;
                }
            }

            found_idx.map(|idx| self.char_idx_to_cursor_position(buffer, idx))
        } else {
            // opening bracket: scan forward
            let mut level = 1;
            let match_char = close_char;
            let total_chars = full_text.chars().count();
            let mut found_idx = None;
            let mut current_idx = char_idx;

            while current_idx + 1 < total_chars {
                current_idx += 1;
                let c = full_text.chars().nth(current_idx).unwrap();
                if c == match_char {
                    level -= 1;
                    if level == 0 {
                        found_idx = Some(current_idx);
                        break;
                    }
                } else if c == bracket {
                    level += 1;
                }
            }

            found_idx.map(|idx| self.char_idx_to_cursor_position(buffer, idx))
        };

        match new_pos {
            Some(pos) => {
                // Now get mutable access to update the cursor
                if let Some(window) = self.windows.active_window_mut() {
                    window.cursor.position = pos;
                    window.cursor.desired_col = None;
                    self.ensure_cursor_visible(&buffer_id);
                }
                CommandResult::ViewChanged
            }
            None => {
                self.set_infobar_message("No matching bracket found".to_string());
                CommandResult::NoOp
            }
        }
    }
}

// Add this separate impl block for private helpers
impl Editor {
    pub fn char_idx_to_cursor_position(
        &self,
        buffer: &crate::buffer::Buffer,
        char_idx: usize,
    ) -> CursorPosition {
        let line = buffer.rope.char_to_line(char_idx);
        let line_start = buffer.rope.line_to_char(line);
        let col = buffer.rope.slice(line_start..char_idx).chars().count();
        CursorPosition::new(line, col)
    }
}

// ── Helper: word character check (copied from original editor.rs) ──
fn is_word_char(g: &str) -> bool {
    g.chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_')
        .unwrap_or(false)
}
