// src/ed/editing.rs
//! Editing operations: insert, delete, yank, paste, undo, redo, etc.

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::CursorPosition;
use crate::ed::VisualExt;
use crate::ed::{CompletionExt, GitExt};
use crate::editor::Editor;
use crate::editor::Mode;
use crate::misc::{comment_chars, get_line_indent, is_string_or_comment_node, is_word_char};
use crate::CommandResult;

/// Extension trait for editing commands (insert, delete, yank, paste, undo/redo).
pub trait EditingExt {
    // ── Insert ───────────────────────────────────────────────
    fn insert_char_at_cursor(&mut self, c: char);
    fn insert_newline_at_cursor(&mut self);
    fn insert_tab_at_cursor(&mut self);
    fn insert_text_at_cursor(&mut self, text: &str);

    // ── Delete ───────────────────────────────────────────────
    fn delete_char_before_cursor(&mut self);
    fn delete_char_at_cursor(&mut self);
    fn delete_n_lines(&mut self, count: usize);
    fn delete_current_line(&mut self);
    fn delete_word_before_cursor(&mut self);
    fn delete_word_after_cursor(&mut self);
    fn delete_to_line_end(&mut self);
    fn delete_to_line_start(&mut self);
    fn delete_selection(&mut self); // dispatches to visual-specific
    fn delete_to_file_end(&mut self) -> CommandResult;

    // ── Replace ──────────────────────────────────────────────
    fn replace_chars_at_cursor(&mut self, c: char, count: usize);
    /// Replace characters with a newline (`r<Enter>`).
    fn replace_char_with_newline(&mut self, count: usize);

    /// Overwrite the character under cursor with `c` and advance right.
    /// Used by Replace mode (R) — overtype behavior.
    fn overwrite_char_at_cursor(&mut self, c: char);
    // ── Open lines ───────────────────────────────────────────
    fn open_line_below(&mut self);
    fn open_line_below_raw(&mut self);
    fn open_line_above(&mut self);
    fn open_line_above_raw(&mut self);

    // ── Join & Indent ────────────────────────────────────────
    fn join_lines(&mut self);
    fn indent_n_lines(&mut self, count: usize);
    fn indent_line(&mut self);
    fn dedent_n_lines(&mut self, count: usize);
    fn dedent_line(&mut self);

    // ── Yank ─────────────────────────────────────────────────
    fn yank_n_lines(&mut self, count: usize) -> CommandResult;
    fn yank_line(&mut self) -> CommandResult;
    fn yank_selection(&mut self) -> CommandResult; // visual-specific

    // ── Paste ────────────────────────────────────────────────
    fn paste_after(&mut self);
    fn paste_before(&mut self);

    // ── System clipboard ─────────────────────────────────────
    fn yank_to_clipboard(&mut self) -> CommandResult;
    fn paste_from_clipboard(&mut self) -> CommandResult;
    fn clipboard_paste_line(&mut self) -> CommandResult;
    fn clipboard_replace_buffer(&mut self) -> CommandResult;
    fn toggle_comment_lines(&mut self, count: usize) -> CommandResult;
    fn toggle_comment_line(&mut self, line: usize, prefix: &str) -> CommandResult;

    // ── Indent (line / visual / tree‑sitter) ─────────────────
    /// Indent the current visual selection (works in Visual, VisualLine, VisualBlock).
    // fn indent_selection(&mut self) -> CommandResult;

    /// Dedent the current visual selection.
    // fn dedent_selection(&mut self) -> CommandResult;

    /// Format indentation using tree‑sitter syntax tree.
    /// `range`: optional `(start_line, end_line)` inclusive; if `None`, format whole buffer.
    fn format_ts_indent(&mut self, range: Option<(usize, usize)>) -> Result<(), String>;

    // ── Undo / Redo helpers ──────────────────────────────────
    fn ensure_undo_group(&mut self);
    fn close_undo_group(&mut self);
    fn with_undo_group<F>(&mut self, f: F) -> CommandResult
    where
        F: FnOnce(&mut Self) -> CommandResult;
    fn undo(&mut self) -> CommandResult;
    fn redo(&mut self) -> CommandResult;
    fn handle_paste(&mut self, text: String) -> CommandResult;
    fn set_yank_register(&mut self, text: String);

    fn delete_inline_target(&mut self, target: char, inclusive: bool) -> CommandResult;

    fn store_yank(&mut self, text: String);
    fn paste_named_register(&mut self, name: char) -> CommandResult;
    fn get_named_register(&self, name: char) -> Option<&str>;
    fn set_named_register(&mut self, name: char, content: String);
}

impl EditingExt for Editor {
    fn insert_char_at_cursor(&mut self, c: char) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let new_pos = buffer.insert_at(pos, &c.to_string());
                window.cursor.position = new_pos;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
        // Skip per-character completion + LSP updates during paste;
        // handle_paste triggers one consolidated update after.
        if !self.paste_in_progress {
            self.maybe_update_completion();
        }
    }

    fn insert_newline_at_cursor(&mut self) {
        // Skip completion cancel during paste — we cancel once after.
        if !self.paste_in_progress {
            self.completion.cancel();
        }
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;

            // ── Compute auto-indentation ──
            let (insert_text, cursor_col, after_len) = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                let line_text = buffer.line_text(pos.line).unwrap_or_default();
                let line_str = line_text.trim_end_matches('\n');

                // Leading whitespace of the current line
                let indent = get_line_indent(line_str);

                // Split current line at cursor position
                let graphemes: Vec<&str> = line_str.graphemes(true).collect();
                let col = pos.col.min(graphemes.len());
                let before: String = graphemes[..col].concat();
                let after: String = graphemes[col..].concat();
                let after_trim = after.trim_start(); // Remove old leading whitespace to prevent duplication

                // Determine new indentation level
                let mut new_indent = indent.clone();
                let before_trim = before.trim_end();
                if before_trim.ends_with('{')
                    || before_trim.ends_with('(')
                    || before_trim.ends_with('[')
                {
                    let one_level = if self.config.use_tabs {
                        "\t".to_string()
                    } else {
                        " ".repeat(self.config.tab_width as usize)
                    };
                    new_indent.push_str(&one_level);
                }

                // If a closing delimiter follows the cursor, place it on its own line
                let dedent_after = after_trim.starts_with('}')
                    || after_trim.starts_with(')')
                    || after_trim.starts_with(']');

                let (text, c_col) = if dedent_after {
                    (
                        format!("\n{}\n{}{}", new_indent, indent, after_trim),
                        new_indent.graphemes(true).count(),
                    )
                } else {
                    (
                        format!("\n{}{}", new_indent, after_trim),
                        new_indent.graphemes(true).count(),
                    )
                };

                (text, c_col, after.graphemes(true).count())
            };

            // ── Delete the `after` part and insert new text ──
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if after_len > 0 {
                    buffer.delete_at(pos, after_len);
                }
                buffer.insert_at(pos, &insert_text);
                window.cursor.position = CursorPosition::new(pos.line + 1, cursor_col);
                window.cursor.desired_col = None;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }

    fn insert_tab_at_cursor(&mut self) {
        let indent = if self.config.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.config.tab_width as usize)
        };

        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let new_pos = buffer.insert_at(pos, &indent);
                window.cursor.position = new_pos;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }

    fn insert_text_at_cursor(&mut self, text: &str) {
        self.ensure_undo_group();
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let new_pos = buffer.insert_at(pos, text);
                window.cursor.position = new_pos;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }

    fn delete_char_before_cursor(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            if window.cursor.position.col > 0 || window.cursor.position.line > 0 {
                let pos = window.cursor.position;
                if pos.col > 0 {
                    if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                        buffer.delete_at(CursorPosition::new(pos.line, pos.col - 1), 1);
                        window.cursor.position.col -= 1;
                        buffer.dirty = true;
                        self.invalidate_git_gutter();
                    }
                } else if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    let prev_line_len = buffer.line_len(pos.line - 1);
                    let delete_pos = CursorPosition::new(pos.line - 1, prev_line_len);
                    buffer.delete_at(delete_pos, 1);
                    window.cursor.position.line -= 1;
                    window.cursor.position.col = prev_line_len;
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        }
    }

    fn delete_char_at_cursor(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let line_len = buffer.line_len(pos.line);
                if pos.col < line_len || pos.line + 1 < buffer.line_count() {
                    buffer.delete_at(pos, 1);
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        }
    }

    fn delete_n_lines(&mut self, count: usize) {
        let count = count.max(1);
        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        let start_line = self.windows.active_window().map(|w| w.cursor.position.line);

        // Yank all lines first.
        if let (Some(buffer_id), Some(start_line)) = (buffer_id, start_line) {
            // ── Extract yank text ──
            let yank_text = {
                let mut yank_text = String::new();
                if let Some(buffer) = self.buffers.get(&buffer_id) {
                    let max_line = buffer.line_count();
                    let end = (start_line + count).min(max_line);
                    for i in start_line..end {
                        if let Some(text) = buffer.line_text(i) {
                            yank_text.push_str(text.trim_end_matches('\n'));
                            yank_text.push('\n');
                        }
                    }
                }
                yank_text
            };

            // ── Set register (borrows self mutably) ──
            self.set_yank_register(yank_text);
            // Delete lines.
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                for _ in 0..count {
                    let line = self
                        .windows
                        .active_window()
                        .map(|w| w.cursor.position.line)
                        .unwrap_or(0);
                    if line < buffer.line_count() {
                        buffer.delete_line(line);
                        buffer.dirty = true;
                    }
                }
                // Clamp cursor: if we deleted past EOF, move to the last line.
                if let Some(window) = self.windows.active_window_mut() {
                    if window.cursor.position.line >= buffer.line_count() && buffer.line_count() > 0
                    {
                        window.cursor.position.line = buffer.line_count() - 1;
                    }
                }
                self.invalidate_git_gutter();
            }
        }
    }

    fn delete_current_line(&mut self) {
        self.delete_n_lines(1);
    }

    fn delete_word_before_cursor(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            let delete_info = self
                .buffers
                .get(&buffer_id)
                .and_then(|buffer| {
                    buffer.line_text(pos.line).map(|line_text| {
                        let graphemes: Vec<_> = line_text.graphemes(true).collect();
                        let mut end = pos.col;
                        if end == 0 {
                            return None;
                        }
                        while end > 0 && graphemes[end - 1].trim().is_empty() {
                            end -= 1;
                        }
                        while end > 0 && is_word_char(graphemes[end - 1]) {
                            end -= 1;
                        }
                        let count = pos.col - end;
                        if count > 0 {
                            Some((end, count))
                        } else {
                            None
                        }
                    })
                })
                .flatten();
            if let Some((new_col, count)) = delete_info {
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.delete_at(CursorPosition::new(pos.line, new_col), count);
                    window.cursor.position.col = new_col;
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        }
    }

    fn delete_word_after_cursor(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            let delete_info = self
                .buffers
                .get(&buffer_id)
                .and_then(|buffer| {
                    buffer.line_text(pos.line).map(|line_text| {
                        let graphemes: Vec<_> = line_text.graphemes(true).collect();
                        if pos.col >= graphemes.len() {
                            return None;
                        }
                        let mut end = pos.col;
                        while end < graphemes.len() && graphemes[end].trim().is_empty() {
                            end += 1;
                        }
                        while end < graphemes.len() && is_word_char(graphemes[end]) {
                            end += 1;
                        }
                        Some(end - pos.col)
                    })
                })
                .flatten();
            if let Some(count) = delete_info {
                if count > 0 {
                    if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                        buffer.delete_at(pos, count);
                        buffer.dirty = true;
                        self.invalidate_git_gutter();
                    }
                }
            }
        }
    }

    fn delete_to_line_end(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            let line_len = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_len(pos.line))
                .unwrap_or(0);
            if pos.col < line_len {
                let count = line_len - pos.col;
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.delete_at(pos, count);
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        }
    }

    fn delete_to_file_end(&mut self) -> CommandResult {
        let buffer_id = match self.windows.active_window().map(|w| w.buffer_id) {
            Some(id) => id,
            None => return CommandResult::NoOp,
        };
        let cursor = self.windows.active_window().unwrap().cursor.position;

        // Determine target line (0‑based)
        let target_line = if self.current_count <= 1 {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };
            buffer.line_count().saturating_sub(1)
        } else {
            let line = self.current_count.saturating_sub(1);
            let max_line = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_count().saturating_sub(1))
                .unwrap_or(0);
            line.min(max_line)
        };

        if target_line < cursor.line {
            self.set_infobar_message("Target line is before cursor".to_string());
            return CommandResult::NoOp;
        }

        // Compute start and end character indices (immutable borrow)
        let (start_char, end_char) = {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };
            let start = buffer.rope.line_to_char(cursor.line) + cursor.col;
            let end = if target_line == cursor.line {
                buffer.rope.line_to_char(cursor.line) + buffer.line_len(cursor.line)
            } else {
                buffer.rope.line_to_char(target_line) + buffer.line_len(target_line)
            };
            (start, end)
        };

        if start_char >= end_char {
            return CommandResult::NoOp;
        }

        // Perform deletion (mutable borrow – short lived)
        {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.rope.remove(start_char..end_char);
                buffer.dirty = true;
            } else {
                return CommandResult::NoOp;
            }
        } // mutable borrow of `self.buffers` ends here

        // Now we can safely borrow `self` mutably again
        self.invalidate_git_gutter();

        // Update cursor position after deletion
        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return CommandResult::NoOp,
        };
        let new_line = buffer.rope.char_to_line(start_char);
        let line_start = buffer.rope.line_to_char(new_line);
        let new_col = start_char - line_start;
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position = CursorPosition::new(new_line, new_col);
            window.cursor.desired_col = None;
        }

        CommandResult::ContentChanged
    }

    fn delete_to_line_start(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if pos.col > 0 {
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.delete_at(CursorPosition::new(pos.line, 0), pos.col);
                    window.cursor.position.col = 0;
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        }
    }

    fn delete_selection(&mut self) {
        self.delete_current_line();
    }

    /// Replace `count` characters starting at cursor with `c` ('r' command).
    /// Cursor ends on the last replaced character (Vim behavior).
    fn replace_chars_at_cursor(&mut self, c: char, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let line_len = buffer.line_len(pos.line);
                let actual_count = count.min(line_len.saturating_sub(pos.col));
                if actual_count > 0 {
                    // Delete the range, then insert replacement characters
                    buffer.delete_at(pos, actual_count);
                    let replacement = c.to_string().repeat(actual_count);
                    buffer.insert_at(pos, &replacement);
                    // Cursor ends on the last replaced character
                    window.cursor.position.col = pos.col + actual_count - 1;
                    window.cursor.desired_col = None;
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        }
    }

    /// Replace character(s) with a newline (`r<Enter>` in Vim).
    fn replace_char_with_newline(&mut self, count: usize) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let line_len = buffer.line_len(pos.line);
                let actual_count = count.min(line_len.saturating_sub(pos.col));
                if actual_count > 0 {
                    buffer.delete_at(pos, actual_count);
                    buffer.insert_at(pos, "\n");
                    // Cursor moves to start of next line
                    window.cursor.position.line += 1;
                    window.cursor.position.col = 0;
                    window.cursor.desired_col = None;
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        }
    }

    /// Overwrite the character under cursor with `c` and advance right.
    /// Used by Replace mode (R) — like overtype in typical editors.
    /// At end of line, inserts instead (Vim's Replace mode extends the line).
    fn overwrite_char_at_cursor(&mut self, c: char) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let line_len = buffer.line_len(pos.line);
                if pos.col < line_len {
                    // Overwrite existing character
                    buffer.delete_at(pos, 1);
                    buffer.insert_at(pos, &c.to_string());
                    window.cursor.position.col += 1;
                    window.cursor.desired_col = None;
                } else {
                    // At end of line — insert (Vim's Replace mode extends the line)
                    let new_pos = buffer.insert_at(pos, &c.to_string());
                    window.cursor.position = new_pos;
                }
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }
    fn open_line_below(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let line = window.cursor.position.line;

            // ── Compute auto-indentation ──
            let (insert_text, cursor_col) = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                let line_text = buffer.line_text(line).unwrap_or_default();
                let line_str = line_text.trim_end_matches('\n');
                let indent = get_line_indent(line_str);

                let mut new_indent = indent.clone();
                let trimmed = line_str.trim_end();
                if trimmed.ends_with('{') || trimmed.ends_with('(') || trimmed.ends_with('[') {
                    let one_level = if self.config.use_tabs {
                        "\t".to_string()
                    } else {
                        " ".repeat(self.config.tab_width as usize)
                    };
                    new_indent.push_str(&one_level);
                }

                (
                    format!("\n{}", new_indent),
                    new_indent.graphemes(true).count(),
                )
            };

            let line_len = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_len(line))
                .unwrap_or(0);
            let insert_pos = CursorPosition::new(line, line_len);

            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.insert_at(insert_pos, &insert_text);
                window.cursor.position = CursorPosition::new(line + 1, cursor_col);
                window.cursor.desired_col = None;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
            self.mode = crate::editor::Mode::Insert;
            self.dirty.mark_all();
        }
    }

    fn open_line_below_raw(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let line = window.cursor.position.line;

            // ── Compute auto-indentation ──
            let (insert_text, cursor_col) = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                let line_text = buffer.line_text(line).unwrap_or_default();
                let line_str = line_text.trim_end_matches('\n');
                let indent = get_line_indent(line_str);

                let mut new_indent = indent.clone();
                let trimmed = line_str.trim_end();
                if trimmed.ends_with('{') || trimmed.ends_with('(') || trimmed.ends_with('[') {
                    let one_level = if self.config.use_tabs {
                        "\t".to_string()
                    } else {
                        " ".repeat(self.config.tab_width as usize)
                    };
                    new_indent.push_str(&one_level);
                }

                (
                    format!("\n{}", new_indent),
                    new_indent.graphemes(true).count(),
                )
            };

            let line_len = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_len(line))
                .unwrap_or(0);
            let insert_pos = CursorPosition::new(line, line_len);

            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.insert_at(insert_pos, &insert_text);
                // Move cursor down to the new line for subsequent count iterations
                window.cursor.position = CursorPosition::new(line + 1, cursor_col);
                window.cursor.desired_col = None;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }

    fn open_line_above(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let line = window.cursor.position.line;

            // ── Compute auto-indentation (same as current line) ──
            let (insert_text, cursor_col) = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                let line_text = buffer.line_text(line).unwrap_or_default();
                let line_str = line_text.trim_end_matches('\n');
                let indent = get_line_indent(line_str);
                (format!("{}\n", indent), indent.graphemes(true).count())
            };

            let insert_pos = CursorPosition::new(line, 0);
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.insert_at(insert_pos, &insert_text);
                // Cursor stays at `line` — the new blank line is here now
                window.cursor.position.line = line;
                window.cursor.position.col = cursor_col;
                window.cursor.desired_col = None;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
            self.mode = crate::editor::Mode::Insert;
            self.dirty.mark_all();
        }
    }

    fn open_line_above_raw(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let line = window.cursor.position.line;

            // ── Compute auto-indentation (same as current line) ──
            let (insert_text, cursor_col) = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                let line_text = buffer.line_text(line).unwrap_or_default();
                let line_str = line_text.trim_end_matches('\n');
                let indent = get_line_indent(line_str);
                (format!("{}\n", indent), indent.graphemes(true).count())
            };

            let insert_pos = CursorPosition::new(line, 0);
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.insert_at(insert_pos, &insert_text);
                // Cursor stays at `line` — the new blank line is here now
                window.cursor.position.line = line;
                window.cursor.position.col = cursor_col;
                window.cursor.desired_col = None;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }

    fn join_lines(&mut self) {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if pos.line + 1 < buffer.line_count() {
                    let current_end = CursorPosition::new(pos.line, buffer.line_len(pos.line));
                    buffer.delete_at(current_end, 1);
                    let end_col = buffer.line_len(pos.line);
                    if end_col > 0 {
                        let line = buffer.line_text(pos.line).unwrap_or_default();
                        if !line.ends_with(' ') {
                            buffer.insert_at(CursorPosition::new(pos.line, end_col), " ");
                        }
                    }
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        }
    }

    fn indent_n_lines(&mut self, count: usize) {
        let count = count.max(1);
        let indent = if self.config.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.config.tab_width as usize)
        };
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let start_line = window.cursor.position.line;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let end = (start_line + count).min(buffer.line_count());
                for line in start_line..end {
                    buffer.insert_at(CursorPosition::new(line, 0), &indent);
                }
                // Move cursor to the first non-blank char of the first indented line.
                window.cursor.position.col += indent.graphemes(true).count();
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }

    fn indent_line(&mut self) {
        self.indent_n_lines(1);
    }

    fn dedent_n_lines(&mut self, count: usize) {
        let count = count.max(1);
        let remove_count = if self.config.use_tabs {
            1
        } else {
            self.config.tab_width as usize
        };
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let start_line = window.cursor.position.line;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let end = (start_line + count).min(buffer.line_count());
                for line in start_line..end {
                    let actual = remove_count.min(buffer.line_len(line));
                    buffer.delete_at(CursorPosition::new(line, 0), actual);
                }
                window.cursor.position.col =
                    window.cursor.position.col.saturating_sub(remove_count);
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }

    fn dedent_line(&mut self) {
        self.dedent_n_lines(1);
    }

    fn yank_n_lines(&mut self, count: usize) -> CommandResult {
        let count = count.max(1);
        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            let start_line = window.cursor.position.line;

            // ── Extract yank text ──
            let yank_result = self.buffers.get(&buffer_id).map(|buffer| {
                let max_line = buffer.line_count();
                let end = (start_line + count).min(max_line);
                let mut yank_text = String::new();
                for i in start_line..end {
                    if let Some(text) = buffer.line_text(i) {
                        yank_text.push_str(text.trim_end_matches('\n'));
                        yank_text.push('\n');
                    }
                }
                yank_text
            });

            // ── Set register and return (borrows self mutably) ──
            if let Some(yank_text) = yank_result {
                self.set_yank_register(yank_text);
                let msg = if count == 1 {
                    "1 line yanked.".to_string()
                } else {
                    format!("{} lines yanked.", count)
                };
                self.set_status(msg.clone());
                return CommandResult::Message(msg);
            }
        }
        CommandResult::NoOp
    }

    fn yank_line(&mut self) -> CommandResult {
        self.yank_n_lines(1)
    }

    fn yank_selection(&mut self) -> CommandResult {
        match self.mode {
            Mode::Visual => self.yank_visual_selection(),
            Mode::VisualLine => self.yank_visual_line_selection(),
            Mode::VisualBlock => self.yank_block_selection(),
            _ => self.yank_line(), // fallback
        }
    }
    fn paste_before(&mut self) {
        // ── Named register routing (e.g. "aP, "%P) ──
        if let Some(reg) = self.pending_register.take() {
            if let Some(content) = self.resolve_register(reg) {
                self.yank_register = content;
            } else {
                self.set_infobar_message(format!("Register '{}' is empty", reg));
                return;
            }
        }

        if self.yank_register.is_empty() {
            return;
        }

        // ── Visual mode: same as paste_after (replace selection) ──
        // In Vim, both p and P replace the visual selection.
        if matches!(
            self.mode,
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        ) {
            // pending_register was already consumed above, yank_register is set.
            // Delegate to paste_after which now handles visual mode.
            self.paste_after();
            return;
        }

        // ── Normal mode ──
        let linewise = self.yank_register.ends_with('\n');
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let insert_pos = if linewise {
                    CursorPosition::new(pos.line, 0)
                } else {
                    pos
                };
                let _new_pos = buffer.insert_at(insert_pos, &self.yank_register);
                if linewise {
                    // ★ FIX: cursor on the first pasted line (which is at pos.line
                    // since we inserted above the current line)
                    window.cursor.position = CursorPosition::new(pos.line, 0);
                } else {
                    window.cursor.position = _new_pos;
                }
                window.cursor.desired_col = None;
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }
    }
    fn paste_after(&mut self) {
        // ── Named register routing (e.g. "ap, "%p) ──
        if let Some(reg) = self.pending_register.take() {
            if let Some(content) = self.resolve_register(reg) {
                self.yank_register = content;
            } else {
                self.set_infobar_message(format!("Register '{}' is empty", reg));
                return;
            }
        }

        if self.yank_register.is_empty() {
            return;
        }

        // ── Visual mode: replace selection with register contents ──
        if matches!(
            self.mode,
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        ) {
            let paste_text = self.yank_register.clone();

            match self.mode {
                Mode::Visual => self.delete_visual_selection(),
                Mode::VisualLine => self.delete_visual_line_selection(),
                Mode::VisualBlock => self.delete_block_selection(),
                _ => {}
            }

            let (buffer_id, pos) = match self.windows.active_window() {
                Some(w) => (w.buffer_id, w.cursor.position),
                None => return,
            };

            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let new_pos = buffer.insert_at(pos, &paste_text);
                if let Some(window) = self.windows.active_window_mut() {
                    // ★ For linewise paste text, land on the first pasted line
                    if paste_text.ends_with('\n') {
                        window.cursor.position = pos;
                    } else {
                        window.cursor.position = new_pos;
                    }
                }
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }

            self.clamp_cursor_to_buffer(&buffer_id);
            return;
        }

        // ── Normal mode ──
        let linewise = self.yank_register.ends_with('\n');

        let (buffer_id, pos) = match self.windows.active_window() {
            Some(w) => (w.buffer_id, w.cursor.position),
            None => return,
        };

        if linewise {
            let line_count = self
                .buffers
                .get(&buffer_id)
                .map(|b| b.line_count())
                .unwrap_or(0);
            let next_line = pos.line + 1;

            if next_line < line_count {
                let insert_pos = CursorPosition::new(next_line, 0);
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    let _new_pos = buffer.insert_at(insert_pos, &self.yank_register);
                    if let Some(window) = self.windows.active_window_mut() {
                        // ★ FIX: cursor on the first pasted line, not after it
                        window.cursor.position = CursorPosition::new(next_line, 0);
                        window.cursor.desired_col = None;
                    }
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            } else {
                let last_line = pos.line;
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    let last_len = buffer.line_len(last_line);
                    let insert_pos = CursorPosition::new(last_line, last_len);
                    let text = format!("\n{}", self.yank_register.trim_end_matches('\n'));
                    let _new_pos = buffer.insert_at(insert_pos, &text);
                    if let Some(window) = self.windows.active_window_mut() {
                        // ★ This was already correct — explicitly sets pasted line
                        window.cursor.position = CursorPosition::new(last_line + 1, 0);
                        window.cursor.desired_col = None;
                    }
                    buffer.dirty = true;
                    self.invalidate_git_gutter();
                }
            }
        } else {
            // Characterwise paste after cursor character
            let insert_col = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                let line_len = buffer.line_len(pos.line);
                if line_len > 0 && pos.col < line_len {
                    pos.col + 1
                } else {
                    pos.col
                }
            };

            let insert_pos = CursorPosition::new(pos.line, insert_col);
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let new_pos = buffer.insert_at(insert_pos, &self.yank_register);
                if let Some(window) = self.windows.active_window_mut() {
                    window.cursor.position = new_pos;
                    window.cursor.desired_col = None;
                }
                buffer.dirty = true;
                self.invalidate_git_gutter();
            }
        }

        self.clamp_cursor_to_buffer(&buffer_id);
    }
    fn yank_to_clipboard(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window() {
            if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                let text = buffer.text();
                match crate::clipboard::set_text(&text) {
                    Ok(()) => {
                        // Also update the local yank register so paste_after/paste_before
                        // is consistent with what was just yanked.
                        self.yank_register = text.clone();

                        self.set_status(format!(
                            "Buffer yanked to clipboard ({} bytes)",
                            text.len()
                        ));
                        return CommandResult::Message("Buffer yanked to clipboard".to_string());
                    }
                    Err(e) => {
                        self.set_infobar_message(format!("Clipboard error: {}", e));
                        return CommandResult::Error(e);
                    }
                }
            }
        }
        CommandResult::NoOp
    }

    fn paste_from_clipboard(&mut self) -> CommandResult {
        self.with_undo_group(|s| {
            if let Some(window) = s.windows.active_window_mut() {
                let buffer_id = window.buffer_id;
                let pos = window.cursor.position;
                if let Some(text) = crate::clipboard::get_text() {
                    if let Some(buffer) = s.buffers.get_mut(&buffer_id) {
                        let new_pos = buffer.insert_at(pos, &text);
                        window.cursor.position = new_pos;
                        buffer.dirty = true;
                        s.invalidate_git_gutter();
                        s.set_status("Pasted from clipboard".to_string());
                        return CommandResult::ContentChanged;
                    }
                } else {
                    s.set_status("Clipboard empty or unavailable".to_string());
                }
            }
            CommandResult::NoOp
        })
    }

    fn clipboard_paste_line(&mut self) -> CommandResult {
        self.with_undo_group(|s| {
            if let Some(text) = crate::clipboard::get_text() {
                if let Some(window) = s.windows.active_window_mut() {
                    let buffer_id = window.buffer_id;
                    let line = window.cursor.position.line;
                    if let Some(buffer) = s.buffers.get_mut(&buffer_id) {
                        let pos = CursorPosition::new(line, 0);
                        let new_pos = buffer.insert_at(pos, &text);
                        window.cursor.position = new_pos;
                        buffer.dirty = true;
                        s.invalidate_git_gutter();
                        s.set_status(format!(
                            "Pasted {} bytes from clipboard at line {}",
                            text.len(),
                            line + 1
                        ));
                        return CommandResult::ContentChanged;
                    }
                }
            } else {
                s.set_status("Clipboard empty or unavailable".to_string());
            }
            CommandResult::NoOp
        })
    }

    fn clipboard_replace_buffer(&mut self) -> CommandResult {
        if let Some(text) = crate::clipboard::get_text() {
            self.with_undo_group(|s| {
                let buffer_id = s.windows.active_window().map(|w| w.buffer_id);
                let bytes = buffer_id.and_then(|bid| s.buffers.get(&bid).map(|b| b.text().len()));

                if let Some(bid) = buffer_id {
                    if let Some(buffer) = s.buffers.get_mut(&bid) {
                        buffer.rope = ropey::Rope::from_str(&text);
                        buffer.reparse_tree();
                        buffer.dirty = true;
                        if let Some(window) = s.windows.active_window_mut() {
                            window.cursor.position = CursorPosition::zero();
                            window.cursor.desired_col = None;
                        }
                        s.invalidate_git_gutter();
                        s.dirty.mark_all();
                        let msg = format!(
                            "Buffer replaced from clipboard ({} -> {} bytes)",
                            bytes.unwrap_or(0),
                            text.len()
                        );
                        s.set_status(msg);
                        return CommandResult::ContentChanged;
                    }
                }
                CommandResult::NoOp
            })
        } else {
            self.set_status("Clipboard empty or unavailable".to_string());
            CommandResult::NoOp
        }
    }

    fn ensure_undo_group(&mut self) {
        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if buffer.in_undo_group() {
                    if self.last_edit_time.elapsed().as_millis()
                        > self.undo_break_timeout_ms as u128
                    {
                        buffer.end_undo_group(window.cursor.position);
                        buffer.begin_undo_group(window.cursor.position);
                        self.last_edit_time = std::time::Instant::now();
                    }
                } else {
                    buffer.begin_undo_group(window.cursor.position);
                    self.last_edit_time = std::time::Instant::now();
                }
            }
        }
    }

    fn close_undo_group(&mut self) {
        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.end_undo_group(window.cursor.position);
            }
        }
    }

    // In the `undo` function, add a dirty flag sync after recalc_dirty:

    fn undo(&mut self) -> CommandResult {
        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        if let Some(bid) = buffer_id {
            if let Some(buffer) = self.buffers.get_mut(&bid) {
                buffer.cancel_undo_group();
            }
        }

        let result =
            buffer_id.and_then(|bid| self.buffers.get_mut(&bid).and_then(|b| b.pop_undo()));

        if let Some((text, cursor)) = result {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id.unwrap()) {
                buffer.rope = ropey::Rope::from_str(&text);
                buffer.reparse_tree();
                let undo_count = buffer.undo_len();
                buffer.recalc_dirty();

                // If undo stack is empty, the buffer is back to its saved/loaded state.
                if undo_count == 0 {
                    buffer.dirty = false;
                }

                if let Some(window) = self.windows.active_window_mut() {
                    window.cursor.position = cursor;
                    window.cursor.desired_col = None;
                }
                self.invalidate_git_gutter();
                self.dirty.mark_all();
                self.set_status(format!("{} changes to undo", undo_count));
                return CommandResult::ContentChanged;
            }
        }
        CommandResult::NoOp
    }

    fn redo(&mut self) -> CommandResult {
        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        if let Some(bid) = buffer_id {
            if let Some(buffer) = self.buffers.get_mut(&bid) {
                buffer.cancel_undo_group();
            }
        }

        let result =
            buffer_id.and_then(|bid| self.buffers.get_mut(&bid).and_then(|b| b.pop_redo()));

        if let Some((text, cursor)) = result {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id.unwrap()) {
                buffer.rope = ropey::Rope::from_str(&text);
                buffer.reparse_tree();
                let redo_count = buffer.redo_len();
                let undo_count = buffer.undo_len();
                buffer.recalc_dirty();

                // After redo, the buffer has unsaved changes if the undo stack
                // has entries (i.e. we've diverged from the loaded state).
                // If both stacks are empty we're at the original state.
                if undo_count == 0 {
                    buffer.dirty = false;
                }

                if let Some(window) = self.windows.active_window_mut() {
                    window.cursor.position = cursor;
                    window.cursor.desired_col = None;
                }
                self.invalidate_git_gutter();
                self.dirty.mark_all();
                self.set_status(format!("{} changes to redo", redo_count));
                return CommandResult::ContentChanged;
            }
        }
        CommandResult::NoOp
    }

    fn toggle_comment_lines(&mut self, count: usize) -> CommandResult {
        let count = count.max(1);
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return CommandResult::NoOp,
        };
        let buffer_id = window.buffer_id;
        let start_line = window.cursor.position.line;
        let language = self.buffers.get(&buffer_id).and_then(|b| b.language);
        let (prefix, _suffix) = match comment_chars(&language) {
            Some(p) => p,
            None => {
                self.set_infobar_message(
                    "No comment syntax defined for this file type".to_string(),
                );
                return CommandResult::NoOp;
            }
        };

        let end_line = (start_line + count).min(
            self.buffers
                .get(&buffer_id)
                .map(|b| b.line_count())
                .unwrap_or(0),
        );

        self.with_undo_group(|s| {
            for line in start_line..end_line {
                if let CommandResult::Error(e) = s.toggle_comment_line(line, prefix) {
                    return CommandResult::Error(e);
                }
            }
            // Move cursor to the first toggled line, keep same column
            if let Some(window) = s.windows.active_window_mut() {
                window.cursor.position.line = start_line;
                s.clamp_cursor_to_buffer(&buffer_id);
            }
            CommandResult::ContentChanged
        })
    }

    fn toggle_comment_line(&mut self, line: usize, prefix: &str) -> CommandResult {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return CommandResult::NoOp,
        };
        let buffer_id = window.buffer_id;
        let prefix_len = prefix.chars().count();
        let buffer = match self.buffers.get_mut(&buffer_id) {
            Some(b) => b,
            None => return CommandResult::NoOp,
        };
        let line_text = match buffer.line_text(line) {
            Some(t) => t,
            None => return CommandResult::NoOp,
        };
        let trimmed = line_text.trim_start();
        let indent_len = line_text.len() - trimmed.len();

        if trimmed.starts_with(prefix) {
            // Remove comment prefix
            let start = indent_len;
            let _end = start + prefix_len;
            buffer.delete_at(CursorPosition::new(line, start), prefix_len);
        } else {
            // Insert comment prefix after indentation
            buffer.insert_at(CursorPosition::new(line, indent_len), prefix);
        }
        buffer.dirty = true;
        self.invalidate_git_gutter();
        CommandResult::ContentChanged
    }

    // cater for rapid paste from clipboard
    // In ed/editing.rs, replace the handle_paste implementation:

    fn handle_paste(&mut self, text: String) -> CommandResult {
        match self.mode {
            Mode::Insert | Mode::Replace => {
                // ── Enter paste mode: suppress per-char LSP/completion ──
                self.paste_in_progress = true;
                self.ensure_undo_group();

                // Normalize line endings: \r\n → \n, bare \r → \n
                let normalized: String = text.replace("\r\n", "\n").replace('\r', "\n");

                if let Some(window) = self.windows.active_window_mut() {
                    let buffer_id = window.buffer_id;
                    let pos = window.cursor.position;
                    if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                        let new_pos = buffer.insert_at(pos, &normalized);
                        window.cursor.position = new_pos;
                        window.cursor.desired_col = None;
                        buffer.dirty = true;
                        self.invalidate_git_gutter();
                    }
                }

                self.close_undo_group();

                // ── Exit paste mode ──
                self.paste_in_progress = false;

                self.maybe_update_completion();
                CommandResult::ContentChanged
            }
            Mode::Command | Mode::LlmPrompt => {
                // Paste into command line — strip newlines, take first line only
                let first_line = text.lines().next().unwrap_or("").to_string();
                self.command_prompt
                    .buffer
                    .insert_str(self.command_prompt.cursor, &first_line);
                self.command_prompt.cursor += first_line.len();
                self.trigger_command_completion();
                CommandResult::ViewChanged
            }
            Mode::Normal => {
                // ── Normal mode: paste after cursor (like `p`) ──
                let normalized: String = text.replace("\r\n", "\n").replace('\r', "\n");
                if normalized.is_empty() {
                    return CommandResult::NoOp;
                }

                self.with_undo_group(|s| {
                    let linewise = normalized.ends_with('\n');
                    let (buffer_id, pos) = match s.windows.active_window() {
                        Some(w) => (w.buffer_id, w.cursor.position),
                        None => return CommandResult::NoOp,
                    };

                    if linewise {
                        // ── Linewise paste: insert on the next line ──
                        let line_count = s
                            .buffers
                            .get(&buffer_id)
                            .map(|b| b.line_count())
                            .unwrap_or(0);
                        let next_line = pos.line + 1;

                        if next_line < line_count {
                            let insert_pos = CursorPosition::new(next_line, 0);
                            if let Some(buffer) = s.buffers.get_mut(&buffer_id) {
                                let new_pos = buffer.insert_at(insert_pos, &normalized);
                                if let Some(window) = s.windows.active_window_mut() {
                                    window.cursor.position = new_pos;
                                }
                                buffer.dirty = true;
                                s.invalidate_git_gutter();
                            }
                        } else {
                            // Last line — append after
                            let last_line = pos.line;
                            if let Some(buffer) = s.buffers.get_mut(&buffer_id) {
                                let last_len = buffer.line_len(last_line);
                                let insert_pos = CursorPosition::new(last_line, last_len);
                                let paste_text = format!("\n{}", normalized.trim_end_matches('\n'));
                                let _new_pos = buffer.insert_at(insert_pos, &paste_text);
                                if let Some(window) = s.windows.active_window_mut() {
                                    window.cursor.position = CursorPosition::new(last_line + 1, 0);
                                }
                                buffer.dirty = true;
                                s.invalidate_git_gutter();
                            }
                        }
                    } else {
                        // ── Characterwise paste: insert after cursor character ──
                        let line_len = s
                            .buffers
                            .get(&buffer_id)
                            .map(|b| b.line_len(pos.line))
                            .unwrap_or(0);
                        // In vim, `p` pastes after the character under cursor
                        let insert_col = if line_len > 0 && pos.col < line_len {
                            pos.col + 1
                        } else {
                            pos.col
                        };
                        let insert_pos = CursorPosition::new(pos.line, insert_col);
                        if let Some(buffer) = s.buffers.get_mut(&buffer_id) {
                            let new_pos = buffer.insert_at(insert_pos, &normalized);
                            if let Some(window) = s.windows.active_window_mut() {
                                window.cursor.position = new_pos;
                            }
                            buffer.dirty = true;
                            s.invalidate_git_gutter();
                        }
                    }

                    s.clamp_cursor_to_buffer(&buffer_id);
                    s.set_status("Pasted from clipboard".to_string());
                    CommandResult::ContentChanged
                })
            }
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                // ── Visual mode: replace selection with clipboard content ──
                let normalized: String = text.replace("\r\n", "\n").replace('\r', "\n");
                if normalized.is_empty() {
                    return CommandResult::NoOp;
                }

                self.with_undo_group(|s| {
                    // Delete the visual selection first
                    match s.mode {
                        Mode::Visual => s.delete_visual_selection(),
                        Mode::VisualLine => s.delete_visual_line_selection(),
                        Mode::VisualBlock => s.delete_block_selection(),
                        _ => {}
                    }

                    // Insert clipboard text at cursor (now at the deletion point)
                    let (buffer_id, pos) = match s.windows.active_window() {
                        Some(w) => (w.buffer_id, w.cursor.position),
                        None => return CommandResult::NoOp,
                    };

                    if let Some(buffer) = s.buffers.get_mut(&buffer_id) {
                        let new_pos = buffer.insert_at(pos, &normalized);
                        if let Some(window) = s.windows.active_window_mut() {
                            window.cursor.position = new_pos;
                        }
                        buffer.dirty = true;
                        s.invalidate_git_gutter();
                    }

                    // Return to normal mode
                    s.mode = Mode::Normal;
                    if let Some(window) = s.windows.active_window_mut() {
                        window.selection_anchor = None;
                    }
                    s.clamp_cursor_to_buffer(&buffer_id);
                    s.set_status("Pasted from clipboard".to_string());
                    CommandResult::ContentChanged
                })
            }
            _ => CommandResult::NoOp,
        }
    }

    fn with_undo_group<F>(&mut self, f: F) -> CommandResult
    where
        F: FnOnce(&mut Self) -> CommandResult,
    {
        // Close any already-open undo group so we get a clean boundary.
        let was_open = if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if buffer.in_undo_group() {
                    buffer.end_undo_group(window.cursor.position);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        // Begin a fresh group that will capture THIS operation.
        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.begin_undo_group(window.cursor.position);
            }
        }

        let result = f(self);

        // End the group we just opened.
        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.end_undo_group(window.cursor.position);
            }
        }

        // If a group was previously open, reopen it so subsequent edits
        // continue in the same logical group.
        if was_open {
            if let Some(window) = self.windows.active_window() {
                let buffer_id = window.buffer_id;
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.begin_undo_group(window.cursor.position);
                    self.last_edit_time = std::time::Instant::now();
                }
            }
        }

        result
    }

    /// Fix indentation using the tree‑sitter syntax tree.
    /// If `range` is `Some((start, end))`, only those lines (inclusive) are adjusted.
    /// Otherwise the entire buffer is formatted.
    /// Returns `Ok(())` on success, `Err(String)` if no syntax tree is available.
    fn format_ts_indent(&mut self, range: Option<(usize, usize)>) -> Result<(), String> {
        let (buffer_id, tab_width, use_tabs) = {
            let window = self.windows.active_window().ok_or("No active window")?;
            let buffer = self.buffers.get(&window.buffer_id).ok_or("No buffer")?;
            if buffer.tree().is_none() {
                return Err("No syntax tree — save file first or check language detection".into());
            }
            (
                window.buffer_id,
                self.config.tab_width as usize,
                self.config.use_tabs,
            )
        };

        // ── Collect brace line numbers ──
        let buffer = self.buffers.get(&buffer_id).unwrap();
        let tree = buffer.tree().unwrap();
        let line_count = buffer.line_count();

        let mut open_braces: Vec<usize> = Vec::new();
        let mut close_braces: Vec<usize> = Vec::new();

        fn collect_braces(
            node: &tree_sitter::Node,
            opens: &mut Vec<usize>,
            closes: &mut Vec<usize>,
        ) {
            let kind = node.kind();
            if is_string_or_comment_node(kind) {
                return;
            }
            if kind == "{" {
                opens.push(node.start_position().row);
            } else if kind == "}" {
                closes.push(node.start_position().row);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_braces(&child, opens, closes);
            }
        }

        collect_braces(&tree.root_node(), &mut open_braces, &mut close_braces);
        open_braces.sort();
        close_braces.sort();

        // ── Compute depth at each line boundary ──
        let mut depths = Vec::with_capacity(line_count);
        let mut open_idx = 0;
        let mut close_idx = 0;
        let mut current_depth: i32 = 0;

        for line in 0..line_count {
            // Count closes that happened BEFORE this line (they reduce depth entering this line)
            while close_idx < close_braces.len() && close_braces[close_idx] < line {
                current_depth -= 1;
                close_idx += 1;
            }
            // Count opens that happened BEFORE this line
            while open_idx < open_braces.len() && open_braces[open_idx] < line {
                current_depth += 1;
                open_idx += 1;
            }
            depths.push(current_depth.max(0));
        }
        // ── Determine the range to modify ──
        let (start_line, end_line) = range.unwrap_or((0, line_count.saturating_sub(1)));
        let end_line = end_line.min(line_count.saturating_sub(1));

        // ── Apply indentation only for the specified range ──
        let mut new_lines = Vec::with_capacity(end_line - start_line + 1);
        let mut changed = false;

        for line_idx in start_line..=end_line {
            let line_text = buffer.line_text(line_idx).unwrap_or_default();
            let trimmed = line_text.trim_end_matches('\n').trim_start();

            if trimmed.is_empty() {
                new_lines.push(String::new());
                continue;
            }

            let mut depth = depths[line_idx];
            if trimmed.starts_with('}') || trimmed.starts_with(']') || trimmed.starts_with(')') {
                depth = depth.saturating_sub(1);
            }

            let depth_usize = depth.max(0) as usize;
            let indent_str = if use_tabs {
                "\t".repeat(depth_usize)
            } else {
                " ".repeat(depth_usize * tab_width)
            };

            let new_line = format!("{}{}", indent_str, trimmed);
            let old_line = line_text.trim_end_matches('\n');
            if new_line != old_line {
                changed = true;
            }
            new_lines.push(new_line);
        }

        if !changed {
            return Ok(());
        }

        // Build final text
        let mut final_text = String::new();
        for line_idx in 0..line_count {
            if line_idx >= start_line && line_idx <= end_line {
                let idx = line_idx - start_line;
                if let Some(line) = new_lines.get(idx) {
                    final_text.push_str(line);
                }
            } else {
                let line_orig = buffer.line_text(line_idx).unwrap_or_default();
                final_text.push_str(line_orig.trim_end_matches('\n'));
            }
            final_text.push('\n');
        }

        // ★ Undoable replacement ★
        let cursor_pos = self
            .windows
            .active_window()
            .map(|w| w.cursor.position)
            .unwrap_or_default();

        let buffer = self.buffers.get_mut(&buffer_id).unwrap();
        let replaced = buffer.replace_all(&final_text, cursor_pos);
        if replaced {
            buffer.reparse_tree();
        }

        // Clamp cursor
        if let Some(window) = self.windows.active_window_mut() {
            let buffer = self.buffers.get(&buffer_id).unwrap();
            let max_line = buffer.line_count().saturating_sub(1);
            window.cursor.position.line = window.cursor.position.line.min(max_line);
            let max_col = buffer.line_len(window.cursor.position.line);
            window.cursor.position.col = window.cursor.position.col.min(max_col);
        }

        Ok(())
    }
    fn set_yank_register(&mut self, text: String) {
        self.yank_register = text.clone();

        // ── Named register routing (e.g. "ayy) ──
        if let Some(reg) = self.pending_register.take() {
            self.set_named_register(reg, text);
        }
    }
    /// Store text in a named register (a–z).
    fn set_named_register(&mut self, name: char, content: String) {
        if name.is_ascii_lowercase() {
            self.named_registers.insert(name, content);
        }
    }

    /// Retrieve text from a named register.
    fn get_named_register(&self, name: char) -> Option<&str> {
        if name.is_ascii_lowercase() {
            self.named_registers.get(&name).map(|s| s.as_str())
        } else {
            None
        }
    }
    /// Paste from a named register after the cursor.
    fn paste_named_register(&mut self, name: char) -> CommandResult {
        if let Some(content) = self.get_named_register(name) {
            self.yank_register = content.to_string();
            self.paste_after(); // Call for side-effect (modifies self in-place)
            CommandResult::ContentChanged // Explicitly return the result
        } else {
            self.set_infobar_message(format!("Register '{}' is empty", name));
            CommandResult::ViewChanged
        }
    }
    /// Store text in the yank register AND the pending named register (if any).
    /// Call this from every yank operation.
    fn store_yank(&mut self, text: String) {
        self.yank_register = text.clone();
        if let Some(reg) = self.pending_register.take() {
            self.set_named_register(reg, text);
        }
        self.pending_register = None;
    }

    /// Delete from cursor up to (exclusive) or including (inclusive) a target character on the same line.
    fn delete_inline_target(&mut self, target: char, inclusive: bool) -> CommandResult {
        let (buffer_id, cursor, line_text) = {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => return CommandResult::NoOp,
            };
            let buffer_id = window.buffer_id;
            let cursor = window.cursor.position;
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };
            let line_text = buffer
                .line_text(cursor.line)
                .unwrap_or_default()
                .to_string();
            (buffer_id, cursor, line_text)
        };

        let graphemes: Vec<&str> = line_text.graphemes(true).collect();
        if cursor.col >= graphemes.len() {
            return CommandResult::NoOp;
        }

        // Search forward for the target character
        let mut target_col = None;
        for (i, g) in graphemes.iter().enumerate() {
            if i > cursor.col {
                // Graphemes can be multi-char, but our target is a single Key::Char
                if g.chars().next() == Some(target) {
                    target_col = Some(i);
                    break;
                }
            }
        }

        if let Some(end_col) = target_col {
            let delete_end = if inclusive { end_col + 1 } else { end_col };
            let count = delete_end - cursor.col;

            self.with_undo_group(|s| {
                if let Some(buffer) = s.buffers.get_mut(&buffer_id) {
                    buffer.delete_at(cursor, count);
                    buffer.dirty = true;
                }
                CommandResult::ContentChanged
            })
        } else {
            // Target character not found on this line
            CommandResult::NoOp
        }
    }
}
