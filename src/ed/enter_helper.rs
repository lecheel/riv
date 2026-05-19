use crate::buffer::BufferId;
use crate::ed::editing::EditingExt;
use crate::ed::visual::VisualExt;
use crate::misc::render_help_entries;
use crate::popup::HelpPopup;
use crate::CommandResult;
use crate::Editor;
use crate::Mode;
use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;

impl Editor {
    pub(crate) fn enter_mode(&mut self, mode: Mode) -> CommandResult {
        let prev = self.mode;
        self.mode = mode;
        self.keybinds.clear_pending();
        self.cancel_which_key_debounce();
        self.which_key_hints.clear();
        self.pending_count.clear();
        self.dirty.mark_all();

        match (prev, &mode) {
            (Mode::Normal, Mode::Visual)
            | (Mode::Normal, Mode::VisualLine)
            | (Mode::Normal, Mode::VisualBlock)
            | (Mode::Insert, Mode::Visual)
            | (Mode::Insert, Mode::VisualLine)
            | (Mode::Insert, Mode::VisualBlock) => {
                if let Some(w) = self.windows.active_window_mut() {
                    w.selection_anchor = Some(w.cursor.position);
                }
            }
            (Mode::Visual, Mode::Visual)
            | (Mode::Visual, Mode::VisualLine)
            | (Mode::Visual, Mode::VisualBlock)
            | (Mode::VisualLine, Mode::Visual)
            | (Mode::VisualLine, Mode::VisualLine)
            | (Mode::VisualLine, Mode::VisualBlock)
            | (Mode::VisualBlock, Mode::Visual)
            | (Mode::VisualBlock, Mode::VisualLine)
            | (Mode::VisualBlock, Mode::VisualBlock) => {}
            (Mode::Visual, Mode::Normal) | (Mode::VisualLine, Mode::Normal) | (Mode::VisualBlock, Mode::Normal) => {
                if let Some(w) = self.windows.active_window_mut() {
                    w.selection_anchor = None;
                }
            }
            _ => {}
        }

        if (prev == Mode::Insert || prev == Mode::Replace) && mode != Mode::Insert && mode != Mode::Replace {
            self.completion.cancel();
            self.close_undo_group();
            if self.block_insert.is_some() {
                self.replay_block_insert();
            }
        }

        if prev == Mode::Command && mode != Mode::Command {
            self.command_completion.cancel();
            self.command_prompt.clear();
            self.clear_messages();
        }

        CommandResult::ModeChanged(mode)
    }

    pub fn enter_insert_mode_at_cursor(&mut self) -> CommandResult {
        self.enter_mode(Mode::Insert)
    }

    pub fn enter_append_mode_at_cursor(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            let line_len = self.buffers.get(&buffer_id).map(|b| b.line_len(pos.line)).unwrap_or(0);
            if pos.col < line_len {
                pos.col += 1;
            }
            window.cursor.desired_col = None;
        }
        self.enter_mode(Mode::Insert)
    }

    pub fn enter_insert_mode_line_start(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let pos = &mut window.cursor.position;
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                if let Some(line_text) = buffer.line_text(pos.line) {
                    let col = line_text.graphemes(true).position(|g| !g.trim().is_empty()).unwrap_or(0);
                    pos.col = col;
                    window.cursor.desired_col = None;
                }
            }
        }
        self.enter_mode(Mode::Insert)
    }

    pub fn enter_append_mode_line_end(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window_mut() {
            let buffer_id = window.buffer_id;
            let line = window.cursor.position.line;
            let line_len = self.buffers.get(&buffer_id).map(|b| b.line_len(line)).unwrap_or(0);
            window.cursor.position.col = line_len;
            window.cursor.desired_col = None;
        }
        self.enter_mode(Mode::Insert)
    }

    // ── Viewport helpers (called after movement) ─────────────────────

    pub fn ensure_cursor_visible(&mut self, buffer_id: &BufferId) {
        let max_line = self.buffers.get(buffer_id).map(|b| b.line_count()).unwrap_or(0);
        if let Some(window) = self.windows.active_window_mut() {
            window.ensure_cursor_visible(max_line);
        }
    }

    pub(crate) fn clamp_cursor_to_buffer(&mut self, buffer_id: &BufferId) {
        if let Some(window) = self.windows.active_window_mut() {
            if let Some(buffer) = self.buffers.get(buffer_id) {
                let max_line = buffer.line_count().saturating_sub(1);
                window.cursor.position.line = window.cursor.position.line.min(max_line);
                let line_len = buffer.line_len(window.cursor.position.line);
                window.cursor.position.col = window.cursor.position.col.min(line_len);
            }
        }
    }

    // ── Help hints ───────────────────────────────────────────────────

    /// Show full help popup (all modes, auto-generated from keybindings).
    pub(crate) fn show_hints(&mut self) -> CommandResult {
        let entries = self.keybinds.help_entries_for_modes(&["normal", "insert", "visual", "command"]);

        let lines = render_help_entries(&entries, 76);

        self.popup.help = Some(HelpPopup::new(lines, self.term_width, self.term_height));
        self.dirty.mark_all();
        CommandResult::NoOp
    }

    /// Show mini help for the current mode only (compact cheat sheet).
    /// Triggered by pressing `F1` or a dedicated key.
    pub(crate) fn show_mini_help(&mut self) -> CommandResult {
        let mode_name = self.mode.keybind_name();
        let entries = self.keybinds.help_entries(mode_name);
        let lines = render_help_entries(&entries, 60);

        self.popup.help = Some(HelpPopup::new(lines, self.term_width, self.term_height));
        self.dirty.mark_all();
        CommandResult::NoOp
    }

    pub fn start_which_key_debounce(&mut self) {
        self.which_key_debounce_timer = Some(Instant::now());
        // Request a tick (the main loop already polls `tick()` every few ms)
        // We'll check the timer inside `tick()`.
    }

    pub fn cancel_which_key_debounce(&mut self) {
        self.which_key_debounce_timer = None;
    }

    pub fn update_which_key_debounce(&mut self) {
        if let Some(start) = self.which_key_debounce_timer {
            if start.elapsed().as_millis() as u64 >= self.which_key_debounce_timeout {
                // Timer expired – refresh the hints now
                self.refresh_which_key_hints();
                self.dirty.status = true;
                self.which_key_debounce_timer = None;
            }
        }
    }
    /// Refresh which-key hints, prepending the pending count prefix.
    pub fn refresh_which_key_hints(&mut self) {
        let mode_name = self.mode.keybind_name();
        let raw = self.keybinds.which_key(mode_name);
        if self.pending_count.is_empty() {
            self.which_key_hints = raw;
        } else {
            let prefix = self.pending_count.clone();
            self.which_key_hints = raw.into_iter().map(|(k, desc)| (format!("{}{}", prefix, k), desc)).collect();
        }
    }
}
