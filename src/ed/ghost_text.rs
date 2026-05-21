// ─────────────────────────────────────────────────────────────────────────────
// ed/ghost_text.rs
// ─────────────────────────────────────────────────────────────────────────────
use crate::codeium::CodeiumResult;
use crate::ed::completion::CompletionExt;
use crate::ed::EditingExt;
use crate::editor::{CommandResult, Editor};
use crate::ghost_text::{GhostText, GhostTextSource};
use crate::Mode;

pub trait GhostTextExt {
    fn request_ghost_text(&mut self);
    fn request_codeium(&mut self);
    fn request_codeium_force(&mut self);
    fn process_codeium_ghost(&mut self, result: Option<CodeiumResult>);
    fn process_lsp_ghost(&mut self, label: String, insert_text: String);
    fn accept_ghost_text(&mut self) -> CommandResult;
    fn dismiss_ghost_text(&mut self);
    fn validate_ghost_text(&mut self);
    fn should_dismiss_ghost(&self) -> bool;
}

/// Helper: extract completion params from current buffer state.
struct CompletionParams {
    full_text: String,
    cursor_offset: usize,
    language: String,
    absolute_path: Option<String>,
    current_line: String,
}

fn get_completion_params(editor: &Editor) -> Option<CompletionParams> {
    let window = editor.windows.active_window()?;
    let buffer = editor.buffers.get(&window.buffer_id)?;
    let pos = window.cursor.position;

    let full_text = buffer.text();
    let cursor_offset = crate::codeium::cursor_to_offset(&full_text, pos.line, pos.col);
    let language = buffer.language.map(|l| l.as_str()).unwrap_or("plain").to_string();
    let absolute_path = buffer
        .file_path
        .as_ref()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().to_string());
    let current_line = buffer.line_text(pos.line).unwrap_or_default().to_string();

    Some(CompletionParams {
        full_text,
        cursor_offset,
        language,
        absolute_path,
        current_line,
    })
}

/// Helper to safely strip prefix case-insensitively and return the suffix.
pub fn case_insensitive_suffix(text: &str, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(text.to_string());
    }

    let mut text_chars = text.chars();
    let mut prefix_chars = prefix.chars();

    while let Some(p_char) = prefix_chars.next() {
        if let Some(t_char) = text_chars.next() {
            if !p_char.to_lowercase().eq(t_char.to_lowercase()) {
                return None;
            }
        } else {
            return None;
        }
    }

    Some(text_chars.collect())
}

impl GhostTextExt for Editor {
    /// Auto-trigger: called after typing in insert mode (debounced).
    fn request_ghost_text(&mut self) {
        if !self.ghost_text.enabled {
            return;
        }
        if self.mode != Mode::Insert {
            return;
        }
        if !self.codeium.is_connected {
            return;
        }
        // Guard: Do not request other suggestions if the completion list is already active
        if self.completion.active {
            return;
        }

        if self.ghost_text.is_pending() && !self.ghost_text.is_request_stale() {
            return;
        }

        if self.ghost_text.should_debounce() {
            return;
        }

        let params = match get_completion_params(self) {
            Some(p) => p,
            None => {
                return;
            }
        };

        let sent = self
            .codeium
            .request(params.full_text, params.cursor_offset, &params.language, params.absolute_path);

        if sent {
            self.ghost_text.mark_requested();
        }
    }
    /// Auto-trigger alias kept for compatibility.
    fn request_codeium(&mut self) {
        self.request_ghost_text();
    }

    /// Manual trigger (Alt+/): skips debounce and mode guards.
    fn request_codeium_force(&mut self) {
        if !self.codeium.is_connected {
            return;
        }

        // If the user manually forces Codeium suggestions, close the completion list first
        if self.completion.active {
            self.close_completion_popup();
        }

        let params = match get_completion_params(self) {
            Some(p) => p,
            None => {
                return;
            }
        };

        self.ghost_text.clear();
        self.codeium.cancel();

        let sent = self
            .codeium
            .request_force(params.full_text, params.cursor_offset, &params.language, params.absolute_path);

        if sent {
            self.ghost_text.mark_requested();
        }
    }

    fn process_codeium_ghost(&mut self, result: Option<CodeiumResult>) {
        // Discard incoming Codeium suggestions if completion list is active
        if self.completion.active {
            return;
        }

        match result {
            Some(codeium_result) => {
                let _preview = if codeium_result.text.len() > 80 {
                    format!("{}...", &codeium_result.text[..80])
                } else {
                    codeium_result.text.clone()
                };

                let window = match self.windows.active_window() {
                    Some(w) => w,
                    None => {
                        return;
                    }
                };
                let pos = window.cursor.position;

                let ghost = GhostText::new(codeium_result.text, pos.line, pos.col, GhostTextSource::Codeium);
                self.ghost_text.set(ghost);
                self.dirty.mark_all();
            }
            None => {
                self.ghost_text.clear();
            }
        }
    }
    /// Process an LSP completion item into ghost text.
    fn process_lsp_ghost(&mut self, _label: String, insert_text: String) {
        if insert_text.is_empty() {
            return;
        }

        let window = match self.windows.active_window() {
            Some(w) => w,
            None => {
                return;
            }
        };
        let pos = window.cursor.position;

        // Compute the already-typed trigger so we can show only the suffix.
        let trigger = self.completion.prefix.clone();

        let ghost_text = if let Some(suffix) = case_insensitive_suffix(&insert_text, &trigger) {
            suffix
        } else {
            insert_text
        };

        if ghost_text.is_empty() {
            return;
        }

        let ghost = GhostText::new(ghost_text, pos.line, pos.col, GhostTextSource::LspInlineHint);
        self.ghost_text.set(ghost);
        self.dirty.mark_all();
    }

    /// Accept the current ghost text suggestion.
    fn accept_ghost_text(&mut self) -> CommandResult {
        let ghost = match self.ghost_text.current.take() {
            Some(g) => g,
            None => {
                return CommandResult::NoOp;
            }
        };

        let (pos_line, pos_col) = {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => {
                    return CommandResult::NoOp;
                }
            };
            (window.cursor.position.line, window.cursor.position.col)
        };

        if !ghost.is_valid_at(pos_line, pos_col) {
            return CommandResult::NoOp;
        }

        // Only insert the part of the suggestion not yet typed.
        let to_insert = ghost.remaining_text(pos_col).to_string();
        if to_insert.is_empty() {
            self.ghost_text.clear();
            return CommandResult::NoOp;
        }

        // If the ghost text came from the completion popup, close it so
        // the popup doesn't linger after Right-arrow accepts the inline hint.
        if ghost.source == crate::ghost_text::GhostTextSource::Completion {
            self.close_completion_popup();
        }

        self.ensure_undo_group();
        for ch in to_insert.chars() {
            match ch {
                '\n' => self.insert_newline_at_cursor(),
                _ => self.insert_char_at_cursor(ch),
            }
        }

        self.ghost_text.clear();
        self.dirty.mark_all();
        CommandResult::ContentChanged
    }

    fn dismiss_ghost_text(&mut self) {
        self.ghost_text.clear();
        self.codeium.cancel();
        self.dirty.mark_all();
    }

    fn validate_ghost_text(&mut self) {
        if let Some(ref ghost) = self.ghost_text.current {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => {
                    self.ghost_text.clear();
                    return;
                }
            };
            let pos = window.cursor.position;

            let invalid_position = !ghost.is_valid_at(pos.line, pos.col);

            // Conflict with any ghost text source other than Completion if the list is active
            let popup_conflict = self.completion.active && ghost.source != crate::ghost_text::GhostTextSource::Completion;

            if invalid_position || popup_conflict {
                self.ghost_text.clear();
                self.codeium.cancel();
            }
        }
    }

    fn should_dismiss_ghost(&self) -> bool {
        let ghost = match self.ghost_text.current.as_ref() {
            Some(g) => g,
            None => return false,
        };

        // Handled internally by `update_completion_ghost_text` loops
        if ghost.source == crate::ghost_text::GhostTextSource::Completion {
            return false;
        }

        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return true,
        };
        let pos = window.cursor.position;

        if pos.line != ghost.line || pos.col < ghost.start_col {
            return true;
        }

        let typed_count = pos.col.saturating_sub(ghost.start_col);
        if typed_count == 0 {
            return false;
        }

        let buffer = match self.buffers.get(&window.buffer_id) {
            Some(b) => b,
            None => return true,
        };

        if let Some(line_text) = buffer.line_text(pos.line) {
            let typed_since_trigger: String = line_text.chars().skip(ghost.start_col).take(typed_count).collect();
            let suggestion_prefix: String = ghost.text.chars().take(typed_count).collect();
            !typed_since_trigger.to_lowercase().starts_with(&suggestion_prefix.to_lowercase())
        } else {
            true
        }
    }
}
