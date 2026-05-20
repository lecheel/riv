// ─────────────────────────────────────────────────────────────────────────────
// ed/ghost_text.rs
// ─────────────────────────────────────────────────────────────────────────────
use crate::codeium::CodeiumResult;
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

        let params = match get_completion_params(self) {
            Some(p) => p,
            None => {
                return;
            }
        };

        // Cancel any in-flight request so the response from the old position
        // doesn't overwrite the one we're about to send.
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
    ///
    /// The `insert_text` is the full text the LSP would insert. We compute
    /// the suffix after the already-typed `trigger` and show only that as
    /// ghost text, consistent with how Codeium results work (they contain
    /// only the part after the cursor).
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

        let ghost_text = if !trigger.is_empty() && insert_text.starts_with(&trigger) {
            insert_text[trigger.len()..].to_string()
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

    /// Validate whether the current ghost text is still applicable.
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
            let popup_conflict = self.completion.active;

            if invalid_position || popup_conflict {
                self.ghost_text.clear();
                self.codeium.cancel();
            }
        }
    }

    /// Return true if the ghost text should be dismissed because the user
    /// typed something that diverges from the suggestion.
    fn should_dismiss_ghost(&self) -> bool {
        let ghost = match self.ghost_text.current.as_ref() {
            Some(g) => g,
            None => return false,
        };

        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return true,
        };
        let pos = window.cursor.position;

        // Wrong line or cursor before trigger — definitely dismiss.
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

            typed_since_trigger != suggestion_prefix
        } else {
            true
        }
    }
}
