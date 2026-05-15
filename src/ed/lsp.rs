use std::path::Path;
use std::time::Instant;

use crate::buffer::CursorPosition;
use crate::ed::file_ops::FileOpsExt;
use crate::ed::ghost_text::GhostTextExt;
use crate::ed::git::GitExt;
use crate::ed::goto_def::GotoDefExt;
use crate::ed::movement::MovementExt;
use crate::editor::{Editor, Mode};
use crate::msgbox::AppMessage;
use unicode_segmentation::UnicodeSegmentation;

pub trait LspExt {
    /// Drain all pending AppMessages from async subsystems.
    /// Called every tick — never blocks.
    fn poll_app_messages(&mut self);

    /// Request LSP completions for the current cursor position.
    fn request_lsp_completions(&mut self);

    /// Flush any pending LSP `didChange` notification immediately.
    /// Call before save, completion requests, or quit so the server
    /// always has the latest content.
    fn flush_lsp_changes(&mut self);

    /// Notify LSP that a file was opened.
    fn lsp_did_open(&mut self, path: &Path);

    /// Notify LSP that a file was saved.
    fn lsp_did_save(&mut self, path: &Path);

    /// Notify LSP that a file was closed.
    fn lsp_did_close(&mut self, path: &Path);

    /// Start LSP servers for the current project.
    fn start_lsp_servers(&mut self);

    /// Notify LSP that a file's content changed (full sync).
    fn lsp_did_change(&mut self, path: &Path, text: String, version: i32);

    /// Notify LSP that the current buffer's content changed (debounced).
    /// The actual `didChange` is sent after `lsp_change_debounce_ms` of
    /// inactivity, or immediately when `flush_lsp_changes` is called.
    fn notify_lsp_change(&mut self);

    /// Send a textDocument/definition request to the LSP server.
    fn request_lsp_goto_definition(&mut self);

    /// Handle the LSP goto definition response (called from poll_app_messages).
    fn handle_lsp_goto_definition(&mut self, locations: Vec<crate::lsp::Location>);

    /// Handle LSP format result from the async formatter.
    fn handle_lsp_format_result(
        &mut self,
        result: Result<Option<Vec<crate::lsp::TextEdit>>, String>,
        buffer_idx: usize,
        cursor_state: Option<(usize, usize)>,
        save_after: bool,
    );
}

impl LspExt for Editor {
    fn poll_app_messages(&mut self) {
        while let Ok(msg) = self.try_recv_app_message() {
            // Suppress visual noise during LLM processing
            if self.get_mode() == Mode::LlmPrompt && msg.suppressed_during_llm() {
                continue;
            }

            match msg {
                AppMessage::LspReady => {
                    self.set_lsp_connected(true);

                    let path_to_open = self.current_buffer().and_then(|b| b.file_path.clone());
                    if let Some(path) = path_to_open {
                        self.lsp_did_open(&path);
                    }

                    self.dirty.mark_all();
                }
                AppMessage::LspGotoDefinitionResult { locations } => {
                    self.handle_lsp_goto_definition(locations);
                }
                AppMessage::LspCompletion(items) => {
                    self.set_lsp_completion_pending(false);

                    let Some(items) = items else {
                        continue;
                    };
                    if items.is_empty() {
                        continue;
                    }

                    // Try to create ghost text from the top LSP item
                    let should_try_lsp_ghost = !self.is_completion_active()
                        && matches!(self.get_mode(), Mode::Insert | Mode::Replace)
                        && self.is_ghost_text_enabled()
                        && !self.is_ghost_text_visible();

                    if should_try_lsp_ghost {
                        if let Some(first) = items.first() {
                            let insert_text = first
                                .get_insert_text()
                                .unwrap_or_else(|| first.label.clone());
                            if !insert_text.is_empty() {
                                self.process_lsp_ghost(first.label.clone(), insert_text);
                                // Don't also show the popup — ghost text is shown instead.
                                // Skip the rest of the completion popup handling.
                                continue;
                            }
                        }
                    }

                    // Existing completion popup handling
                    if self.is_completion_active() {
                        self.add_lsp_items_to_completion(items);
                        self.dirty.mark_all();
                    } else if matches!(self.get_mode(), Mode::Insert | Mode::Replace) {
                        let cursor_pos = self.windows.active_window().map(|w| w.cursor.position);
                        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);

                        if let (Some(pos), Some(bid)) = (cursor_pos, buffer_id) {
                            if let Some(buffer) = self.buffers.get(&bid) {
                                use crate::completion::word_or_path_before_cursor;
                                let (word, is_path) = word_or_path_before_cursor(buffer, pos);

                                let trigger = word.clone();

                                self.set_completion_context(
                                    trigger.clone(),
                                    pos,
                                    buffer.line_text(pos.line).unwrap_or_default(),
                                    is_path,
                                );
                                self.set_completion_active(true);
                                self.clear_completion_items();
                                self.set_completion_selected_index(0);

                                self.add_lsp_items_to_completion(items);

                                if self.get_completion_items_count() == 0 {
                                    self.cancel_completion();
                                } else {
                                    self.dirty.mark_all();
                                }
                            }
                        }
                    }
                }

                AppMessage::LspDiagnostics {
                    uri,
                    version: _,
                    diagnostics,
                } => {
                    if diagnostics.is_empty() {
                        self.remove_lsp_diagnostics(&uri);
                    } else {
                        self.insert_lsp_diagnostics(uri, diagnostics);
                    }
                    self.dirty.mark_all();
                }

                AppMessage::LspSignatureHelp(state) => {
                    self.set_lsp_signature_help(state);
                    self.dirty.mark_all();
                }

                AppMessage::LspInlayHints {
                    uri,
                    hints,
                    version: _,
                } => {
                    self.insert_lsp_inlay_hints(uri, hints);
                    self.dirty.mark_all();
                }

                AppMessage::LspFormatResult {
                    result,
                    buffer_idx,
                    cursor_state,
                    save_after,
                } => {
                    self.handle_lsp_format_result(result, buffer_idx, cursor_state, save_after);
                }

                AppMessage::LspCompletionResolved(_item) => {
                    // Could update the selected item's detail/documentation in the popup
                }

                AppMessage::LspError(e) => {
                    self.set_lsp_connected(false);
                    self.set_infobar_message(format!("LSP: {}", e));
                }

                AppMessage::RgSearchResult { pattern, results } => {
                    let _ = (pattern, results);
                }
                AppMessage::RgSearchError(e) => {
                    self.set_infobar_message(format!("rg: {}", e));
                }

                AppMessage::GitDiffResult(result) => {
                    let _ = result;
                }

                AppMessage::StatusMessage(msg) => {
                    self.set_status(msg);
                }
                AppMessage::ModeChange(mode) => {
                    self.enter_mode(mode);
                }
                AppMessage::Redraw => {
                    self.dirty.mark_all();
                }
                AppMessage::FormatResult {
                    buffer_id,
                    result,
                    cursor_state,
                    save_after,
                } => {
                    self.set_formatting_pending(false);
                    self.clear_formatting_buffer_id();

                    match result {
                        Err(e) => {
                            self.show_fmt_info_popup("Format Error", &e);
                            self.dirty.status_cmdline = true;
                        }
                        Ok(formatted) => {
                            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                                let current = buffer.text();
                                if formatted != current {
                                    buffer.replace_all(&formatted, cursor_state);
                                    buffer.reparse_tree();
                                    self.invalidate_git_gutter();
                                }
                            }
                            // Restore cursor, clamped
                            if let Some(window) = self.windows.active_window_mut() {
                                if let Some(buffer) = self.buffers.get(&buffer_id) {
                                    let max_line = buffer.line_count().saturating_sub(1);
                                    window.cursor.position.line = cursor_state.line.min(max_line);
                                    let max_col = buffer.line_len(window.cursor.position.line);
                                    window.cursor.position.col = cursor_state.col.min(max_col);
                                }
                            }
                            if save_after {
                                match self.save() {
                                    Ok(()) => self.set_status("Formatted and saved.".into()),
                                    Err(e) => {
                                        self.set_infobar_message(format!("Format ok, save failed: {}", e))
                                    }
                                }
                            } else {
                                self.set_status("Formatted.".into());
                            }
                            self.dirty.mark_all();
                        }
                    }
                }
            }
        }
    }

    fn request_lsp_completions(&mut self) {
        self.flush_lsp_changes();
        if !self.is_lsp_connected() {
            return;
        }

        let path = match self.current_buffer().and_then(|b| b.file_path.clone()) {
            Some(p) => p,
            None => return,
        };

        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return,
        };

        let line = window.cursor.position.line as u32;
        let character = window.cursor.position.col as u32;
        let trigger = self.detect_completion_trigger();

        self.send_lsp_message(crate::lsp::LspMessage::RequestCompletion(
            path, line, character, trigger,
        ));
        self.set_lsp_completion_pending(true);
    }

    fn flush_lsp_changes(&mut self) {
        if !self.is_lsp_change_pending() {
            return;
        }
        self.set_lsp_change_pending(false);
        self.clear_lsp_change_deadline();

        let Some(window) = self.windows.active_window() else {
            return;
        };
        let buffer_id = window.buffer_id;

        let (path, text) = {
            let Some(buffer) = self.buffers.get(&buffer_id) else {
                return;
            };
            let Some(ref path) = buffer.file_path else {
                return;
            };
            (path.clone(), buffer.rope.to_string())
        };

        self.increment_lsp_doc_version();
        self.send_lsp_message(crate::lsp::LspMessage::ChangeFile(
            path,
            String::new(),
            text,
            self.get_lsp_doc_version(),
        ));
    }

    fn lsp_did_open(&mut self, path: &Path) {
        self.flush_lsp_changes();
        self.send_lsp_message(crate::lsp::LspMessage::OpenFile(path.to_path_buf()));
    }

    fn lsp_did_save(&mut self, path: &Path) {
        self.flush_lsp_changes();
        self.send_lsp_message(crate::lsp::LspMessage::SaveFile(path.to_path_buf()));
    }

    fn lsp_did_close(&mut self, path: &Path) {
        self.send_lsp_message(crate::lsp::LspMessage::CloseFile(path.to_path_buf()));
    }

    fn start_lsp_servers(&mut self) {
        // LSP servers are auto-started when files are opened
        // via LspMessage::OpenFile in the LspManager async loop.
        // No explicit startup is needed here.
    }

    fn lsp_did_change(&mut self, path: &Path, text: String, version: i32) {
        self.send_lsp_message(crate::lsp::LspMessage::ChangeFile(
            path.to_path_buf(),
            String::new(),
            text,
            version,
        ));
    }

    fn notify_lsp_change(&mut self) {
        self.set_lsp_change_pending(true);
        // During paste we skip the deadline — handle_paste flushes explicitly.
        if self.is_paste_in_progress() {
            return;
        }
        self.set_lsp_change_deadline(
            Instant::now() + std::time::Duration::from_millis(self.get_lsp_change_debounce_ms()),
        );
    }

    fn handle_lsp_format_result(
        &mut self,
        result: Result<Option<Vec<crate::lsp::TextEdit>>, String>,
        _buffer_idx: usize,
        cursor_state: Option<(usize, usize)>,
        save_after: bool,
    ) {
        match result {
            Ok(Some(edits)) => {
                if edits.is_empty() {
                    self.set_status("Already formatted".to_string());
                    return;
                }

                // Apply text edits to the buffer
                let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
                if let Some(bid) = buffer_id {
                    if let Some(buffer) = self.buffers.get_mut(&bid) {
                        // Apply edits in reverse order (so earlier edits don't
                        // invalidate later positions)
                        let doc = &buffer.rope;
                        let encoding = crate::lsp::OffsetEncoding::Utf8; // default

                        let mut char_edits: Vec<(usize, usize, String)> = edits
                            .iter()
                            .filter_map(|edit| {
                                crate::lsp::lsp_range_to_char_indices(doc, &edit.range, encoding)
                                    .map(|(start, end)| (start, end, edit.new_text.clone()))
                            })
                            .collect();

                        // Sort descending by start position
                        char_edits.sort_by(|a, b| b.0.cmp(&a.0));

                        // Apply each edit
                        for (start, end, new_text) in &char_edits {
                            if start == end {
                                buffer.rope.insert(*start, new_text);
                            } else {
                                buffer.rope.remove(*start..*end);
                                buffer.rope.insert(*start, new_text);
                            }
                        }

                        buffer.dirty = true;
                        self.invalidate_git_gutter();

                        // Restore cursor position
                        if let Some((line, col)) = cursor_state {
                            if let Some(window) = self.windows.active_window_mut() {
                                window.cursor.position.line = line;
                                window.cursor.position.col = col;
                                window.cursor.desired_col = None;
                            }
                        }

                        // Save if requested
                        if save_after {
                            let _ = self.save();
                        }

                        self.dirty.mark_all();
                        self.set_status(format!("Formatted ({} edits)", char_edits.len()));
                    }
                }
            }
            Ok(None) => {
                self.set_status("Already formatted".to_string());
            }
            Err(e) => {
                self.set_infobar_message(format!("Format failed: {}", e));
            }
        }
    }

    fn request_lsp_goto_definition(&mut self) {
        self.flush_lsp_changes();
        if !self.is_lsp_connected() {
            return;
        }

        let path = match self.current_buffer().and_then(|b| b.file_path.clone()) {
            Some(p) => p,
            None => return,
        };

        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return,
        };

        let line = window.cursor.position.line as u32;
        let col = window.cursor.position.col as u32;

        self.send_lsp_message(crate::lsp::LspMessage::GotoDefinition { path, line, col });
        self.set_status("Goto definition…".to_string());
    }

    fn handle_lsp_goto_definition(&mut self, locations: Vec<crate::lsp::Location>) {
        if locations.is_empty() {
            // LSP found nothing — fall back to Tree-sitter
            self.set_status("LSP: no definition. Trying local…".to_string());
            let result = self.goto_definition();
            if let crate::editor::CommandResult::Error(e) = result {
                self.set_infobar_message(e);
            }
            return;
        }

        let location = &locations[0];
        let path = crate::lsp::uri_to_path(&location.uri);
        let target_line = location.range.start.line as usize;
        let target_col = location.range.start.character as usize;

        // Check if the file is already open
        let existing_id = self
            .buffers
            .iter()
            .find(|b| b.file_path.as_ref() == Some(&path))
            .map(|b| b.id);

        if let Some(id) = existing_id {
            if let Some(window) = self.windows.active_window_mut() {
                window.set_buffer(id);
            }
        } else {
            match self.open_file(&path) {
                Ok(_) => {}
                Err(e) => {
                    self.set_infobar_message(format!("Failed to open definition: {}", e));
                    return;
                }
            }
        }

        self.move_to_position(target_line, target_col);
        self.ensure_cursor_visible_all();

        let display = self
            .current_buffer()
            .map(|b| b.display_name())
            .unwrap_or_else(|| "?".into());
        self.set_status(format!(
            "Goto definition: {} line {}",
            display,
            target_line + 1
        ));
        self.dirty.mark_all();
    }
}

// Private helper methods for Editor (these need to be implemented or called via public methods)
impl Editor {
    /// These methods should be implemented in editor.rs or exposed as public methods:

    // Message receiving
    fn try_recv_app_message(
        &mut self,
    ) -> Result<AppMessage, tokio::sync::mpsc::error::TryRecvError> {
        self.app_rx.try_recv()
    }

    // Mode and state getters
    fn get_mode(&self) -> Mode {
        self.mode
    }
    fn is_lsp_connected(&self) -> bool {
        self.lsp_connected
    }
    fn set_lsp_connected(&mut self, value: bool) {
        self.lsp_connected = value;
    }

    // LSP completion state
    fn set_lsp_completion_pending(&mut self, value: bool) {
        self.lsp_completion_pending = value;
    }
    fn is_lsp_change_pending(&self) -> bool {
        self.lsp_change_pending
    }
    fn set_lsp_change_pending(&mut self, value: bool) {
        self.lsp_change_pending = value;
    }
    fn clear_lsp_change_deadline(&mut self) {
        self.lsp_change_deadline = None;
    }
    fn set_lsp_change_deadline(&mut self, deadline: Instant) {
        self.lsp_change_deadline = Some(deadline);
    }
    fn get_lsp_change_debounce_ms(&self) -> u64 {
        self.lsp_change_debounce_ms
    }
    fn increment_lsp_doc_version(&mut self) {
        self.lsp_doc_version += 1;
    }
    fn get_lsp_doc_version(&self) -> i32 {
        self.lsp_doc_version
    }
    fn is_paste_in_progress(&self) -> bool {
        self.paste_in_progress
    }

    // LSP message sending
    fn send_lsp_message(&mut self, msg: crate::lsp::LspMessage) {
        let _ = self.lsp_tx.send(msg);
    }

    // LSP diagnostics
    fn remove_lsp_diagnostics(&mut self, uri: &str) {
        self.lsp_diagnostics.remove(uri);
    }
    fn insert_lsp_diagnostics(&mut self, uri: String, diagnostics: Vec<crate::lsp::Diagnostic>) {
        self.lsp_diagnostics.insert(uri, diagnostics);
    }
    fn insert_lsp_inlay_hints(&mut self, uri: String, hints: Vec<crate::lsp::InlayHint>) {
        self.lsp_inlay_hints.insert(uri, hints);
    }
    fn set_lsp_signature_help(&mut self, help: Option<crate::lsp::SignatureHelpState>) {
        self.lsp_signature_help = help;
    }

    // Completion state
    fn is_completion_active(&self) -> bool {
        self.completion.active
    }
    fn get_completion_items_count(&self) -> usize {
        self.completion.items.len()
    }
    fn set_completion_active(&mut self, active: bool) {
        self.completion.active = active;
    }
    fn set_completion_context(
        &mut self,
        trigger: String,
        pos: CursorPosition,
        line_text: String,
        is_path: bool,
    ) {
        self.completion.context = Some(crate::completion::CompletionContext {
            trigger,
            position: pos,
            line_text,
            is_path,
        });
    }
    fn clear_completion_items(&mut self) {
        self.completion.items.clear();
    }
    fn set_completion_selected_index(&mut self, index: usize) {
        self.completion.selected_index = index;
    }
    fn add_lsp_items_to_completion(&mut self, items: Vec<crate::lsp::CompletionItem>) {
        self.completion.add_lsp_items(items);
    }
    fn cancel_completion(&mut self) {
        self.completion.cancel();
    }

    // Ghost text
    fn is_ghost_text_enabled(&self) -> bool {
        self.ghost_text.enabled
    }
    fn is_ghost_text_visible(&self) -> bool {
        self.ghost_text.is_visible()
    }

    // Formatting state
    fn set_formatting_pending(&mut self, pending: bool) {
        self.formatting_pending = pending;
    }
    fn clear_formatting_buffer_id(&mut self) {
        self.formatting_buffer_id = None;
    }

    // LSP completion trigger detection (private helper)
    fn detect_completion_trigger(&self) -> Option<String> {
        if let Some(window) = self.windows.active_window() {
            if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                let pos = window.cursor.position;
                if pos.col > 0 {
                    if let Some(line_text) = buffer.line_text(pos.line) {
                        let graphemes: Vec<_> = line_text.graphemes(true).collect();
                        if let Some(last) = graphemes.get(pos.col - 1) {
                            if *last == "." || *last == ":" {
                                return Some(last.to_string());
                            }
                        }
                    }
                }
            }
        }
        None
    }
}
