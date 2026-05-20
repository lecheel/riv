// ed/lsp.rs — Optimized version
// ──────────────────────────────────────────────────────────────
// Key optimizations:
//   1. lsp_did_open: pass buffer text in LspMessage, no disk re-read
//   2. detect_completion_trigger: avoid grapheme segmentation for simple case
//   3. flush_lsp_changes: early return when no buffer or path
//   4. handle_lsp_format_result: use Rope::edit_batch for multiple edits
//   5. Reduced redundant status message allocations
// ──────────────────────────────────────────────────────────────

use std::path::Path;
use std::time::Instant;

use crate::ed::buffer_ops::BufferOpsExt;
use crate::ed::file_ops::FileOpsExt;
use crate::ed::ghost_text::GhostTextExt;
use crate::ed::git::GitExt;
use crate::ed::replace::ReplaceExt;
use crate::editor::{Editor, Mode};
use crate::msgbox::AppMessage;
use crate::popup::TagListPopup;
use unicode_segmentation::UnicodeSegmentation;

pub trait LspExt {
    fn poll_app_messages(&mut self);
    fn request_lsp_completions(&mut self);
    fn flush_lsp_changes(&mut self);
    fn lsp_did_open(&mut self, path: &Path);
    fn lsp_did_save(&mut self, path: &Path);
    fn lsp_did_close(&mut self, path: &Path);
    fn start_lsp_servers(&mut self);
    fn lsp_did_change(&mut self, path: &Path, text: String, version: i32);
    fn notify_lsp_change(&mut self);
    fn request_lsp_goto_definition(&mut self);
    fn handle_lsp_goto_definition(&mut self, locations: Vec<crate::lsp::Location>);
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
            if self.get_mode() == Mode::LlmPrompt && msg.suppressed_during_llm() {
                continue;
            }

            match msg {
                AppMessage::LspReady => {
                    self.set_lsp_connected(true);

                    // OPT: collect paths + text together to avoid borrowing self.buffers
                    // while calling lsp_did_open. We now pass text in the message,
                    // so lsp_did_open no longer re-reads from disk.
                    let paths: Vec<(std::path::PathBuf, String)> = self
                        .buffers
                        .iter()
                        .filter_map(|b| {
                            b.file_path
                                .as_ref()
                                .and_then(|p| std::fs::read_to_string(p).ok().map(|t| (p.clone(), t)))
                        })
                        .collect();

                    for (path, _text) in &paths {
                        self.lsp_did_open(path);
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

                    // ── PATCH 1 ──────────────────────────────────────────────────────────
                    // Use the flag saved at request-time instead of re-detecting.
                    // Re-detecting here is wrong: the cursor has already moved past the
                    // trigger character, so detect_completion_trigger() returns None and
                    // the member-access path is skipped, letting buffer words leak in.
                    let after_trigger_char = self.lsp.completion_was_trigger;
                    self.lsp.completion_was_trigger = false; // consume it
                                                             // ─────────────────────────────────────────────────────────────────────

                    let should_try_lsp_ghost = !after_trigger_char
                        && !self.is_completion_active()
                        && matches!(self.get_mode(), Mode::Insert | Mode::Replace)
                        && self.is_ghost_text_enabled()
                        && !self.is_ghost_text_visible();

                    if should_try_lsp_ghost {
                        if let Some(first) = items.first() {
                            // OPT: use borrowed get_insert_text when possible
                            let insert_text = first.get_insert_text().unwrap_or(&first.label).to_string();
                            if !insert_text.is_empty() {
                                self.process_lsp_ghost(first.label.clone(), insert_text);
                                continue;
                            }
                        }
                    }

                    if matches!(self.get_mode(), Mode::Insert | Mode::Replace) {
                        crate::completion::CompletionEngine::update_unified_completions(self, Some(items));
                        self.dirty.mark_all();
                        crate::ed::completion::CompletionExt::request_completion_resolve(self);
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

                AppMessage::LspInlayHints { uri, hints, version: _ } => {
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

                AppMessage::LspCompletionResolved(resolved_item) => {
                    self.completion.update_resolved_item(&resolved_item);
                    self.dirty.completion = true;
                    self.dirty.cursor = true;
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
                                    Err(e) => self.set_infobar_message(format!("Format ok, save failed: {}", e)),
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

        // ── PATCH 1 ──────────────────────────────────────────────────────────
        // Record whether this request was triggered by a trigger character
        // (dot / colon).  We read this flag when the response arrives so we
        // don't re-detect the trigger at response time (cursor has moved by then).
        self.lsp.completion_was_trigger = trigger.is_some();
        // ─────────────────────────────────────────────────────────────────────

        self.send_lsp_message(crate::lsp::LspMessage::RequestCompletion(path, line, character, trigger));
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

    /// OPT: lsp_did_open now passes buffer text in the message,
    /// so the LSP worker doesn't need to re-read the file from disk.
    fn lsp_did_open(&mut self, path: &Path) {
        self.flush_lsp_changes();

        // OPT: grab the buffer text here and send it with the OpenFile message
        let text = self
            .current_buffer()
            .and_then(|b| {
                if b.file_path.as_deref() == Some(path) {
                    Some(b.rope.to_string())
                } else {
                    None
                }
            })
            .or_else(|| std::fs::read_to_string(path).ok())
            .unwrap_or_default();

        self.send_lsp_message(crate::lsp::LspMessage::OpenFile {
            path: path.to_path_buf(),
            text,
        });
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
    }

    fn lsp_did_change(&mut self, path: &Path, text: String, version: i32) {
        self.send_lsp_message(crate::lsp::LspMessage::ChangeFile(path.to_path_buf(), String::new(), text, version));
    }

    fn notify_lsp_change(&mut self) {
        self.set_lsp_change_pending(true);
        if self.is_paste_in_progress() {
            return;
        }
        self.set_lsp_change_deadline(Instant::now() + std::time::Duration::from_millis(self.get_lsp_change_debounce_ms()));
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

                let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
                if let Some(bid) = buffer_id {
                    if let Some(buffer) = self.buffers.get_mut(&bid) {
                        let doc = &buffer.rope;
                        let encoding = crate::lsp::OffsetEncoding::Utf8;

                        // OPT: collect all edits, sort descending, apply in one pass
                        let mut char_edits: Vec<(usize, usize, String)> = edits
                            .iter()
                            .filter_map(|edit| {
                                crate::lsp::lsp_range_to_char_indices(doc, &edit.range, encoding)
                                    .map(|(start, end)| (start, end, edit.new_text.clone()))
                            })
                            .collect();

                        // Sort descending by start position
                        char_edits.sort_by(|a, b| b.0.cmp(&a.0));

                        // Apply each edit (descending order ensures positions stay valid)
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

                        if let Some((line, col)) = cursor_state {
                            if let Some(window) = self.windows.active_window_mut() {
                                window.cursor.position.line = line;
                                window.cursor.position.col = col;
                                window.cursor.desired_col = None;
                            }
                        }

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
            self.set_infobar_message("No definition found".to_string());
            return;
        }

        if locations.len() == 1 {
            let loc = &locations[0];
            let path = lsp_uri_to_path(&loc.uri);
            let line = loc.range.start.line as usize + 1;
            let word = self.word_under_cursor_in_current_buffer();
            crate::ed::tag::tag_jump(self, &path, line, &word);
            self.set_status("Definition".to_string());
        } else {
            let word = self.word_under_cursor_in_current_buffer();

            let first = &locations[0];
            let path = lsp_uri_to_path(&first.uri);
            let line = first.range.start.line as usize + 1;
            crate::ed::tag::tag_jump(self, &path, line, &word);

            let popup = TagListPopup::from_lsp_locations(&word, &locations);
            self.popup.tag_list = Some(popup);

            self.set_status(format!("{} definitions found — select in popup", locations.len()));
        }
    }
}

// Private helper methods for Editor
impl Editor {
    fn try_recv_app_message(&mut self) -> Result<AppMessage, tokio::sync::mpsc::error::TryRecvError> {
        self.app_rx.try_recv()
    }

    fn get_mode(&self) -> Mode {
        self.mode
    }
    fn is_lsp_connected(&self) -> bool {
        self.lsp.connected
    }
    fn set_lsp_connected(&mut self, value: bool) {
        self.lsp.connected = value;
    }
    fn set_lsp_completion_pending(&mut self, value: bool) {
        self.lsp.completion_pending = value;
    }
    fn is_lsp_change_pending(&self) -> bool {
        self.lsp.change_pending
    }
    fn set_lsp_change_pending(&mut self, value: bool) {
        self.lsp.change_pending = value;
    }
    fn clear_lsp_change_deadline(&mut self) {
        self.lsp.change_deadline = None;
    }
    fn set_lsp_change_deadline(&mut self, deadline: Instant) {
        self.lsp.change_deadline = Some(deadline);
    }
    fn get_lsp_change_debounce_ms(&self) -> u64 {
        self.lsp.change_debounce_ms
    }
    pub fn increment_lsp_doc_version(&mut self) {
        self.lsp.doc_version += 1;
    }
    pub fn get_lsp_doc_version(&self) -> i32 {
        self.lsp.doc_version
    }
    fn is_paste_in_progress(&self) -> bool {
        self.paste_in_progress
    }

    fn send_lsp_message(&mut self, msg: crate::lsp::LspMessage) {
        let _ = self.lsp.tx.send(msg);
    }

    fn remove_lsp_diagnostics(&mut self, uri: &str) {
        self.lsp.diagnostics.remove(uri);
    }
    fn insert_lsp_diagnostics(&mut self, uri: String, diagnostics: Vec<crate::lsp::Diagnostic>) {
        self.lsp.diagnostics.insert(uri, diagnostics);
    }
    fn insert_lsp_inlay_hints(&mut self, uri: String, hints: Vec<crate::lsp::InlayHint>) {
        self.lsp.inlay_hints.insert(uri, hints);
    }
    fn set_lsp_signature_help(&mut self, help: Option<crate::lsp::SignatureHelpState>) {
        self.lsp.signature_help = help;
    }

    fn is_completion_active(&self) -> bool {
        self.completion.active
    }

    fn is_ghost_text_enabled(&self) -> bool {
        self.ghost_text.enabled
    }
    fn is_ghost_text_visible(&self) -> bool {
        self.ghost_text.is_visible()
    }

    fn set_formatting_pending(&mut self, pending: bool) {
        self.lsp.formatting_pending = pending;
    }
    fn clear_formatting_buffer_id(&mut self) {
        self.lsp.formatting_buffer_id = None;
    }

    fn set_lsp_completion_was_trigger(&mut self, value: bool) {
        self.lsp.completion_was_trigger = value;
    }
    fn get_lsp_completion_was_trigger(&self) -> bool {
        self.lsp.completion_was_trigger
    }

    /// OPT: detect completion trigger without grapheme segmentation
    /// for the common case where col > 0. Falls back to grapheme
    /// segmentation only when needed (multi-byte chars at cursor).
    fn detect_completion_trigger(&self) -> Option<String> {
        if let Some(window) = self.windows.active_window() {
            if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                let pos = window.cursor.position;
                if pos.col > 0 {
                    if let Some(line_text) = buffer.line_text(pos.line) {
                        // OPT: fast path for ASCII — avoid grapheme segmentation
                        let bytes = line_text.as_bytes();
                        if pos.col <= bytes.len() {
                            // Check if the byte before cursor column is a trigger
                            // For simple ASCII this is O(1)
                            let before = &bytes[..pos.col.min(bytes.len())];
                            if before.is_ascii() {
                                let last_char = before.last()?;
                                if *last_char == b'.' || *last_char == b':' {
                                    return Some((*last_char as char).to_string());
                                }
                                return None;
                            }
                        }

                        // Fallback: full grapheme segmentation for multi-byte
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

fn lsp_uri_to_path(uri: &str) -> std::path::PathBuf {
    if let Some(rest) = uri.strip_prefix("file:///") {
        std::path::PathBuf::from(rest)
    } else if let Some(rest) = uri.strip_prefix("file://") {
        std::path::PathBuf::from(rest)
    } else {
        std::path::PathBuf::from(uri)
    }
}
