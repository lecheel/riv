//--+ ./ed/visual.rs
// src/ed/visual.rs
//! Visual mode operations: block, line, and character selections.
//!
//! Provides block insert/append, yank, delete, and anchor swapping.
//! All methods are implemented for [`Editor`] via the [`VisualExt`] trait.

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::CursorPosition;
use crate::ed::editing::EditingExt;
use crate::ed::GitExt;
use crate::editor::{Editor, Mode};
use crate::CommandResult;

/// State for a visual-block insert/append operation.
///
/// When the user presses `I` or `A` in VisualBlock mode, we record:
/// - The insert column
/// - Which lines are part of the block
/// - The starting cursor position (to detect typed text)
#[derive(Debug, Clone)]
pub struct BlockInsertState {
    /// Column at which to insert text on each target line.
    insert_col: usize,
    /// The 0‑based line indices that participate in the block.
    target_lines: Vec<usize>,
    /// Cursor position when the block insert was initiated (start of first line).
    start_pos: CursorPosition,
}

/// Extension trait for visual mode actions.
///
/// All methods assume the editor is already in the appropriate visual
/// sub‑mode (`Visual`, `VisualLine`, or `VisualBlock`).
pub trait VisualExt {
    /// Return the rectangle (top, bottom, left, right) of the current visual block selection.
    /// Returns `None` if no selection anchor exists.
    fn visual_block_rect(&self) -> Option<(usize, usize, usize, usize)>;

    /// Delete the current visual block selection (rectangle) and yank the deleted text.
    fn delete_block_selection(&mut self);

    /// Yank (copy) the current visual block selection into the yank register.
    fn yank_block_selection(&mut self) -> CommandResult;

    /// Delete the current visual line selection (full lines).
    fn delete_visual_line_selection(&mut self);

    /// Yank (copy) the current visual line selection.
    fn yank_visual_line_selection(&mut self) -> CommandResult;

    /// Delete the current visual character‑wise selection.
    fn delete_visual_selection(&mut self);

    /// Yank (copy) the current visual character‑wise selection.
    fn yank_visual_selection(&mut self) -> CommandResult;

    /// Swap the selection anchor and cursor position (Vim's `o` in visual modes).
    fn swap_selection_anchor(&mut self) -> CommandResult;

    /// Enter block insert mode (`I` in VisualBlock): moves cursor to the left column
    /// of the top line and enters Insert mode.
    fn block_insert(&mut self) -> CommandResult;

    /// Enter block append mode (`A` in VisualBlock): moves cursor just after the
    /// right column of the top line and enters Insert mode.
    fn block_append(&mut self) -> CommandResult;

    /// Internal: start a block insert/append operation.
    fn start_block_insert(&mut self, is_append: bool) -> CommandResult;

    /// Replay the typed text on all other lines of the block.
    /// Called when leaving Insert mode while `block_insert` is active.
    fn replay_block_insert(&mut self);

    /// Indent every line covered by the current visual selection.
    fn indent_selection(&mut self) -> CommandResult;

    /// Dedent every line covered by the current visual selection.
    fn dedent_selection(&mut self) -> CommandResult;
    fn get_selection_text(&self) -> Option<String>;
    fn yank_selection_to_clipboard(&mut self) -> CommandResult;
}

impl VisualExt for Editor {
    // src/ed/visual.rs

    fn indent_selection(&mut self) -> CommandResult {
        let (start_line, end_line) = match self.visual_line_range() {
            Some(r) => r,
            None => return CommandResult::NoOp,
        };

        let indent_str = if self.config.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.config.tab_width as usize)
        };

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // ── Begin undo group BEFORE any inserts ──
        if let Some(window) = self.windows.active_window() {
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if buffer.in_undo_group() {
                    // Close any open group first so we get a clean boundary
                    buffer.end_undo_group(pos);
                }
                buffer.begin_undo_group(pos);
            }
        }

        // ── Perform all inserts ──
        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            let end = (end_line + 1).min(buffer.line_count());
            for line in start_line..end {
                buffer.insert_at(CursorPosition::new(line, 0), &indent_str);
            }
            buffer.dirty = true;
        }

        self.invalidate_git_gutter();

        // Shift the selection to account for added indentation
        let indent_width = indent_str.graphemes(true).count();
        if let Some(window) = self.windows.active_window_mut() {
            if let Some(anchor) = window.selection_anchor.as_mut() {
                anchor.col += indent_width;
            }
            window.cursor.position.col += indent_width;
        }

        // ── End undo group AFTER all inserts ──
        if let Some(window) = self.windows.active_window() {
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.end_undo_group(pos);
            }
        }

        CommandResult::ContentChanged
    }

    fn dedent_selection(&mut self) -> CommandResult {
        let (start_line, end_line) = match self.visual_line_range() {
            Some(r) => r,
            None => return CommandResult::NoOp,
        };

        let shiftwidth = self.config.tab_width as usize;
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // ── Begin undo group BEFORE any deletes ──
        if let Some(window) = self.windows.active_window() {
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if buffer.in_undo_group() {
                    buffer.end_undo_group(pos);
                }
                buffer.begin_undo_group(pos);
            }
        }

        let mut min_removed_cols = usize::MAX;

        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            let end = (end_line + 1).min(buffer.line_count());
            for line in start_line..end {
                let line_text = buffer.line_text(line).unwrap_or_default();
                let leading: String = line_text
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();

                if leading.is_empty() {
                    min_removed_cols = 0;
                    continue;
                }

                let ws_cols: usize = leading
                    .chars()
                    .map(|c| if c == '\t' { shiftwidth } else { 1 })
                    .sum();

                let remove_cols = ws_cols.min(shiftwidth);

                let mut cols_remaining = remove_cols;
                let mut chars_to_remove: usize = 0;
                for c in leading.chars() {
                    let char_cols = if c == '\t' { shiftwidth } else { 1 };
                    if cols_remaining >= char_cols {
                        cols_remaining -= char_cols;
                        chars_to_remove += 1;
                    } else {
                        chars_to_remove += 1;
                        break;
                    }
                    if cols_remaining == 0 {
                        break;
                    }
                }

                if chars_to_remove > 0 {
                    buffer.delete_at(CursorPosition::new(line, 0), chars_to_remove);
                }

                let actual_removed = remove_cols - cols_remaining;
                min_removed_cols = min_removed_cols.min(actual_removed);
            }
            buffer.dirty = true;
        }

        self.invalidate_git_gutter();

        if min_removed_cols == usize::MAX {
            min_removed_cols = 0;
        }
        if let Some(window) = self.windows.active_window_mut() {
            if let Some(anchor) = window.selection_anchor.as_mut() {
                anchor.col = anchor.col.saturating_sub(min_removed_cols);
            }
            window.cursor.position.col =
                window.cursor.position.col.saturating_sub(min_removed_cols);
        }

        // ── End undo group AFTER all deletes ──
        if let Some(window) = self.windows.active_window() {
            let pos = window.cursor.position;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.end_undo_group(pos);
            }
        }

        CommandResult::ContentChanged
    }

    fn visual_block_rect(&self) -> Option<(usize, usize, usize, usize)> {
        let window = self.windows.active_window()?;
        let anchor = window.selection_anchor?;

        let head = window.cursor.position;
        let (top, bot) = if anchor.line <= head.line {
            (anchor.line, head.line)
        } else {
            (head.line, anchor.line)
        };
        let (left, right) = if anchor.col <= head.col {
            (anchor.col, head.col)
        } else {
            (head.col, anchor.col)
        };

        Some((top, bot, left, right))
    }

    fn delete_block_selection(&mut self) {
        let (top, bot, left, right) = match self.visual_block_rect() {
            Some(r) => r,
            None => return,
        };

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return,
        };

        // Collect deleted text for yank register (optional future use).
        let mut deleted_parts: Vec<String> = Vec::new();

        self.ensure_undo_group();

        // Delete from bottom to top to preserve line indices.
        for line in (top..=bot).rev() {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let line_text = buffer.line_text(line).unwrap_or_default();
                let graphemes: Vec<_> = line_text.trim_end_matches('\n').graphemes(true).collect();
                let end_col = (right + 1).min(graphemes.len());
                let del_count = end_col.saturating_sub(left);

                if left < graphemes.len() && left < end_col && del_count > 0 {
                    let deleted: String = graphemes[left..end_col].join("");
                    deleted_parts.push(deleted);
                    buffer.delete_at(CursorPosition::new(line, left), del_count);
                } else if left <= graphemes.len() {
                    // Line is shorter than the selection — nothing to delete.
                    deleted_parts.push(String::new());
                }
            }
        }

        self.close_undo_group();
        self.invalidate_git_gutter();

        // Store deleted text in yank register (join with newlines) & sync clipboard
        self.set_yank_register(deleted_parts.join("\n"));

        // Position cursor at top-left of the deleted region.
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position = CursorPosition::new(top, left);
            window.cursor.desired_col = None;
            window.selection_anchor = None;
        }

        self.enter_mode(Mode::Normal);
        self.dirty.mark_all();
    }

    fn yank_block_selection(&mut self) -> CommandResult {
        let (top, bot, left, right) = match self.visual_block_rect() {
            Some(r) => r,
            None => return CommandResult::NoOp,
        };

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        let mut parts: Vec<String> = Vec::new();
        for line in top..=bot {
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                let text = buffer.line_text(line).unwrap_or_default();
                let graphemes: Vec<_> = text.trim_end_matches('\n').graphemes(true).collect();
                let end_col = (right + 1).min(graphemes.len());
                if left < graphemes.len() && left < end_col {
                    parts.push(graphemes[left..end_col].join(""));
                } else {
                    parts.push(String::new());
                }
            }
        }

        self.set_yank_register(parts.join("\n"));

        // Position cursor at top-left, return to normal mode.
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position = CursorPosition::new(top, left);
            window.cursor.desired_col = None;
            window.selection_anchor = None;
        }
        self.enter_mode(Mode::Normal);
        CommandResult::Message(format!("Yanked {} lines", bot - top + 1))
    }

    fn delete_visual_line_selection(&mut self) {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return,
        };
        let anchor = match window.selection_anchor {
            Some(a) => a,
            None => return,
        };
        let head = window.cursor.position;
        let (top, bot) = if anchor.line <= head.line {
            (anchor.line, head.line)
        } else {
            (head.line, anchor.line)
        };
        let buffer_id = window.buffer_id;

        // ── Yank the lines before deleting ──
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            let mut parts = Vec::new();
            for line in top..=bot {
                if let Some(text) = buffer.line_text(line) {
                    parts.push(text.trim_end_matches('\n').to_string());
                }
            }
            self.set_yank_register(parts.join("\n"));
        }

        // Delete lines bottom‑to‑top.
        self.ensure_undo_group();
        for line in (top..=bot).rev() {
            if let Some(w) = self.windows.active_window() {
                if let Some(buffer) = self.buffers.get_mut(&w.buffer_id) {
                    buffer.delete_line(line);
                }
            }
        }
        self.close_undo_group();
        self.invalidate_git_gutter();

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position = CursorPosition::new(top, 0);
            w.cursor.desired_col = None;
            w.selection_anchor = None;
        }
        self.enter_mode(Mode::Normal);
    }

    fn yank_visual_line_selection(&mut self) -> CommandResult {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return CommandResult::NoOp,
        };
        let anchor = match window.selection_anchor {
            Some(a) => a,
            None => return CommandResult::NoOp,
        };
        let head = window.cursor.position;
        let (top, bot) = if anchor.line <= head.line {
            (anchor.line, head.line)
        } else {
            (head.line, anchor.line)
        };

        let buffer_id = window.buffer_id;
        let mut parts = Vec::new();
        for line in top..=bot {
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                if let Some(text) = buffer.line_text(line) {
                    parts.push(text.trim_end_matches('\n').to_string());
                }
            }
        }
        self.set_yank_register(parts.join("\n"));

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position = CursorPosition::new(top, 0);
            w.cursor.desired_col = None;
            w.selection_anchor = None;
        }
        self.enter_mode(Mode::Normal);
        CommandResult::Message(format!("Yanked {} lines", bot - top + 1))
    }

    fn delete_visual_selection(&mut self) {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return,
        };
        let anchor = match window.selection_anchor {
            Some(a) => a,
            None => return,
        };
        let head = window.cursor.position;

        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };

        let buffer_id = window.buffer_id;

        // ── Collect text to yank BEFORE any mutation ──
        let yanked_text = if start.line == end.line {
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                let text = buffer.line_text(start.line).unwrap_or_default();
                let g: Vec<_> = text.trim_end_matches('\n').graphemes(true).collect();
                let count = end.col.saturating_sub(start.col);
                if count > 0 {
                    g[start.col..(start.col + count).min(g.len())].join("")
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            let mut parts = Vec::new();
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                // First line: from start.col to end
                let first_text = buffer.line_text(start.line).unwrap_or_default();
                let g: Vec<_> = first_text.trim_end_matches('\n').graphemes(true).collect();
                let start_idx = start.col.min(g.len());
                parts.push(g[start_idx..].join(""));

                // Middle lines: full content
                for line in (start.line + 1)..end.line {
                    if let Some(t) = buffer.line_text(line) {
                        parts.push(t.trim_end_matches('\n').to_string());
                    }
                }

                // Last line: from 0 to end.col
                if let Some(t) = buffer.line_text(end.line) {
                    let g: Vec<_> = t.trim_end_matches('\n').graphemes(true).collect();
                    parts.push(g[..end.col.min(g.len())].join(""));
                }
            }
            parts.join("\n")
        };

        // ── Perform the deletion ──
        if start.line == end.line {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let count = end.col.saturating_sub(start.col);
                if count > 0 {
                    buffer.delete_at(start, count);
                    self.invalidate_git_gutter();
                }
            }
        } else {
            self.ensure_undo_group();

            // Delete partial end line (col 0..end.col)
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let end_col = end.col.min(buffer.line_len(end.line));
                if end_col > 0 {
                    buffer.delete_at(CursorPosition::new(end.line, 0), end_col);
                }
            }

            // Delete full intermediate lines (bottom-up to preserve indices)
            for line in ((start.line + 1)..end.line).rev() {
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.delete_line(line);
                }
            }

            // Delete partial start line (from start.col to end-of-line)
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let line_len = buffer.line_len(start.line);
                let count = line_len.saturating_sub(start.col);
                if count > 0 {
                    buffer.delete_at(start, count);
                }
            }

            self.close_undo_group();
            self.invalidate_git_gutter();
        }

        // ── Yank the deleted text ──
        if !yanked_text.is_empty() {
            self.set_yank_register(yanked_text);
        }

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position = start;
            w.cursor.desired_col = None;
            w.selection_anchor = None;
        }
        self.enter_mode(Mode::Normal);
    }

    fn yank_selection_to_clipboard(&mut self) -> CommandResult {
        // First, perform the local yank using the mode-specific method.
        // This correctly populates self.yank_register and exits visual mode.
        let result = match self.mode {
            Mode::Visual => self.yank_visual_selection(),
            Mode::VisualLine => self.yank_visual_line_selection(),
            Mode::VisualBlock => self.yank_block_selection(),
            _ => return CommandResult::NoOp,
        };

        // Then, push the yanked text to the system clipboard.
        if !self.yank_register.is_empty() {
            match crate::clipboard::set_text(&self.yank_register) {
                Ok(()) => {
                    return CommandResult::Message("Yanked selection to clipboard".to_string());
                }
                Err(e) => {
                    return CommandResult::Error(format!("Clipboard error: {}", e));
                }
            }
        }

        result
    }

    fn yank_visual_selection(&mut self) -> CommandResult {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return CommandResult::NoOp,
        };
        let anchor = match window.selection_anchor {
            Some(a) => a,
            None => return CommandResult::NoOp,
        };
        let head = window.cursor.position;

        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };

        let buffer_id = window.buffer_id;
        let yanked = if start.line == end.line {
            // Same line.
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                let text = buffer.line_text(start.line).unwrap_or_default();
                let g: Vec<_> = text.trim_end_matches('\n').graphemes(true).collect();
                g[start.col..(end.col).min(g.len())].join("")
            } else {
                String::new()
            }
        } else {
            // Multi‑line.
            let mut parts = Vec::new();
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                // First line: from start.col to end.
                let first_text = buffer.line_text(start.line).unwrap_or_default();
                let g: Vec<_> = first_text.trim_end_matches('\n').graphemes(true).collect();
                let start_idx = start.col.min(g.len());
                parts.push(g[start_idx..].join(""));

                // Middle lines: full content.
                for line in (start.line + 1)..end.line {
                    if let Some(t) = buffer.line_text(line) {
                        parts.push(t.trim_end_matches('\n').to_string());
                    }
                }

                // Last line: from 0 to end.col.
                if let Some(t) = buffer.line_text(end.line) {
                    let g: Vec<_> = t.trim_end_matches('\n').graphemes(true).collect();
                    parts.push(g[..end.col.min(g.len())].join(""));
                }
            }
            parts.join("\n")
        };

        self.set_yank_register(yanked);

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position = start;
            w.cursor.desired_col = None;
            w.selection_anchor = None;
        }
        self.enter_mode(Mode::Normal);
        CommandResult::Message("Yanked selection".to_string())
    }

    fn swap_selection_anchor(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            if let Some(anchor) = window.selection_anchor.take() {
                window.selection_anchor = Some(window.cursor.position);
                window.cursor.position = anchor;
                window.cursor.desired_col = None;
                self.dirty.mark_all();
            }
        }
        CommandResult::ViewChanged
    }

    fn block_insert(&mut self) -> CommandResult {
        self.start_block_insert(false)
    }

    fn block_append(&mut self) -> CommandResult {
        self.start_block_insert(true)
    }

    fn start_block_insert(&mut self, is_append: bool) -> CommandResult {
        let (top, bot, left, right) = match self.visual_block_rect() {
            Some(r) => r,
            None => return CommandResult::NoOp,
        };

        // Build the list of target lines.
        let target_lines: Vec<usize> = (top..=bot).collect();

        let insert_col = if is_append { right + 1 } else { left };

        // Record start position for later text extraction.
        let start_pos = CursorPosition::new(top, insert_col);
        // Cancel any active completion and prevent it from re‑activating
        self.completion.cancel();
        self.block_insert = Some(BlockInsertState {
            insert_col,
            target_lines,
            start_pos,
        });

        // Move cursor to the insertion point on the first (top) line.
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position = start_pos;
            window.cursor.desired_col = None;
            // Keep selection_anchor for visual feedback while typing.
        }

        // Enter insert mode. The block insert state will be consumed
        // when the user presses Escape (handled in enter_mode).
        self.enter_mode(Mode::Insert)
    }

    fn replay_block_insert(&mut self) {
        let state = match self.block_insert.take() {
            Some(s) => s,
            None => return,
        };
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return,
        };

        // Extract the text typed on the first line.
        let typed_text = {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return,
            };
            let line_text = match buffer.line_text(state.start_pos.line) {
                Some(t) => t.trim_end_matches('\n').to_string(),
                None => return,
            };
            let graphemes: Vec<_> = line_text.graphemes(true).collect();
            let end_col = self
                .windows
                .active_window()
                .map(|w| {
                    if w.cursor.position.line == state.start_pos.line {
                        w.cursor.position.col
                    } else {
                        graphemes.len()
                    }
                })
                .unwrap_or(state.start_pos.col);
            if end_col <= state.start_pos.col {
                return;
            }
            graphemes[state.start_pos.col..end_col.min(graphemes.len())].join("")
        };

        if typed_text.is_empty() {
            return;
        }

        self.ensure_undo_group();

        let start_line = state.start_pos.line;
        for &line in state.target_lines.iter().rev() {
            if line == start_line {
                continue;
            }
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                let line_text = buffer
                    .line_text(line)
                    .unwrap_or_default()
                    .trim_end_matches('\n')
                    .to_string();
                let graphemes: Vec<_> = line_text.graphemes(true).collect();

                // Vim only replays on lines that are at least as wide as the
                // block's insert column. Lines shorter than insert_col are
                // skipped entirely (not padded, not clamped to end-of-line).
                if graphemes.len() < state.insert_col {
                    continue;
                }

                let pos = CursorPosition::new(line, state.insert_col);
                buffer.insert_at(pos, &typed_text);
                self.invalidate_git_gutter();
            }
        }

        self.close_undo_group();

        if let Some(window) = self.windows.active_window_mut() {
            window.selection_anchor = None;
        }
        self.status_message = Some(format!(
            "Block insert \"{}\" on {} lines",
            typed_text,
            state.target_lines.len()
        ));
    }

    /// Get selected text if in a visual mode, otherwise None.
    fn get_selection_text(&self) -> Option<String> {
        if !matches!(
            self.mode,
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        ) {
            return None;
        }

        let window = self.windows.active_window()?;
        let anchor = window.selection_anchor?;
        let head = window.cursor.position;

        let buffer = self.buffers.get(&window.buffer_id)?;

        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };

        // Convert (line, col) to char indices using ropey
        let start_char = buffer.rope.try_line_to_char(start.line).ok()?;
        let end_line = end.line.min(buffer.rope.len_lines().saturating_sub(1));
        let end_line_len = buffer.rope.line(end_line).len_chars().saturating_sub(1); // exclude newline
        let end_col = end.col.min(end_line_len);
        let end_char = buffer.rope.try_line_to_char(end_line).ok()? + end_col;

        if start_char > end_char {
            return None;
        }

        let slice = buffer.rope.slice(start_char..end_char);
        Some(slice.to_string())
    }
}

// Add this helper to Editor (or to the VisualExt impl block)
impl Editor {
    /// Return the (start_line, end_line) of the current visual selection.
    /// Works for Visual, VisualLine, and VisualBlock modes.
    pub fn visual_line_range(&self) -> Option<(usize, usize)> {
        let window = self.windows.active_window()?;
        let anchor = window.selection_anchor?;
        let head = window.cursor.position;
        let (top, bot) = if anchor.line <= head.line {
            (anchor.line, head.line)
        } else {
            (head.line, anchor.line)
        };
        Some((top, bot))
    }
}
