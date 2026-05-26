//--+ ./src/ed/completion.rs
// src/ed/completion.rs
use crate::buffer::{Buffer, CursorPosition};
use crate::completion::CompletionEntry;
use crate::completion::{collect_file_paths, collect_vocab_words, word_or_path_before_cursor, TriggerMode};
use crate::ed::ghost_text::case_insensitive_suffix;
use crate::ed::lsp::LspExt;
use crate::ed::EditingExt;
use crate::editor::Editor;
use crate::CommandResult;
use unicode_segmentation::UnicodeSegmentation;

pub trait CompletionExt {
    fn trigger_completion(&mut self) -> CommandResult;
    fn maybe_update_completion(&mut self);
    fn select_next_completion(&mut self) -> CommandResult;
    fn select_prev_completion(&mut self) -> CommandResult;
    fn confirm_completion(&mut self) -> CommandResult;
    fn trigger_command_completion(&mut self);
    fn request_completion_resolve(&mut self);
    fn ensure_word_index_fresh(&mut self);
    fn close_completion_popup(&mut self);
    fn update_completion_ghost_text(&mut self);
    fn should_show_completion_popup(&self) -> bool;
}

fn completion_merge_overlap(completion: &str, remaining: &str) -> usize {
    let comp: Vec<&str> = completion.graphemes(true).collect();
    let rem: Vec<&str> = remaining.graphemes(true).collect();

    let max = comp.len().min(rem.len());
    for len in (1..=max).rev() {
        if comp[comp.len() - len..] == rem[..len] {
            return len;
        }
    }
    0
}

#[inline]
fn last_char_before_cursor(buffer: &Buffer, pos: CursorPosition) -> Option<char> {
    if pos.col == 0 {
        return None;
    }
    if let Some(line) = buffer.line_text(pos.line) {
        if line.is_ascii() {
            let bytes = line.as_bytes();
            if pos.col <= bytes.len() {
                let b = bytes[pos.col - 1];
                if b.is_ascii() {
                    return Some(b as char);
                }
            }
        }
        let graphemes: Vec<&str> = line.graphemes(true).collect();
        return graphemes.get(pos.col - 1).and_then(|g| g.chars().next());
    }
    None
}

#[inline]
fn is_trigger_char(ch: char) -> bool {
    ch == '.' || ch == ':'
}

impl CompletionExt for Editor {
    fn should_show_completion_popup(&self) -> bool {
        self.completion.active
            && (self.config.completion_style == crate::config::CompletionStyle::Popup
                || self.config.completion_style == crate::config::CompletionStyle::Both)
    }

    fn update_completion_ghost_text(&mut self) {
        let style = self.config.completion_style;
        if style == crate::config::CompletionStyle::Popup {
            return;
        }

        if !self.completion.active {
            if let Some(ref ghost) = self.ghost_text.current {
                if ghost.source == crate::ghost_text::GhostTextSource::Completion {
                    log::debug!("[ed/completion] update_completion_ghost_text: Session inactive. Clearing ghost preview.");
                    self.ghost_text.clear();
                }
            }
            return;
        }

        if let Some(selected) = self.completion.selected_item() {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => return,
            };
            let pos = window.cursor.position;
            let prefix = &self.completion.prefix;

            // ── Mid-word guard ──────────────────────────────────────────
            // Don't show completion ghost text when the character immediately
            // after the cursor is an identifier char — the cursor is mid-word
            // and the preview would overlap existing content.
            {
                let buffer = match self.buffers.get(&window.buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                if let Some(line_text) = buffer.line_text(pos.line) {
                    let next_is_id = line_text.graphemes(true).nth(pos.col).map_or(false, |g| {
                        g.chars().next().map_or(false, |c| c.is_alphanumeric() || c == '_' || c == '-')
                    });
                    if next_is_id {
                        if self
                            .ghost_text
                            .current
                            .as_ref()
                            .map_or(false, |g| g.source == crate::ghost_text::GhostTextSource::Completion)
                        {
                            log::debug!(
                                "[ed/completion] update_completion_ghost_text: \
                                 Next char is an identifier char. Hiding ghost text."
                            );
                            self.ghost_text.clear();
                        }
                        return;
                    }
                }
            }

            // Parse snippet syntax so ghost text shows clean display text,
            // e.g. "max(other)" instead of "max(${1:other})$0"
            let snippet_res = crate::snippet::parse_snippet_for_insert(&selected.text);
            let display_text = snippet_res.text;

            let ghost_text = if let Some(suffix) = case_insensitive_suffix(&display_text, prefix) {
                suffix
            } else {
                display_text.clone()
            };

            log::debug!(
            "[ed/completion] update_completion_ghost_text: Candidate: '{}', Clean display: '{}', Prefix: '{}', Computed suffix: '{}' (Col: {})",
            selected.text,
            display_text,
            prefix,
            ghost_text,
            pos.col
        );

            if !ghost_text.is_empty() {
                let ghost =
                    crate::ghost_text::GhostText::new(ghost_text, pos.line, pos.col, crate::ghost_text::GhostTextSource::Completion);
                self.ghost_text.set(ghost);
                self.dirty.mark_all();
            } else {
                self.ghost_text.clear();
            }
        } else {
            log::debug!("[ed/completion] update_completion_ghost_text: No selected entry. Clearing ghost preview.");
            self.ghost_text.clear();
        }
    }

    fn close_completion_popup(&mut self) {
        log::debug!("[ed/completion] close_completion_popup: Terminating popup session");
        let old_rect = self.popup.overlay.completion;
        self.completion.cancel();
        self.popup.overlay.completion = None;
        if let Some(rect) = old_rect {
            self.dirty.mark_popup_closed(rect);
        }
        self.update_completion_ghost_text();
        self.dirty.windows = true;
        self.dirty.cursor = true;
    }

    fn trigger_completion(&mut self) -> CommandResult {
        // If the word index is stale, rebuild it right now so the popup is accurate
        if self.word_index_dirty {
            if let Some(window) = self.windows.active_window() {
                let buffer_id = window.buffer_id;
                if let Some(buffer) = self.buffers.get(&buffer_id) {
                    self.completion.word_index.build_from_buffer(buffer);
                }
            }
            self.word_index_dirty = false;
            self.word_index_deadline = None;
        }
        let is_insert_or_replace = self.mode == crate::editor::Mode::Insert || self.mode == crate::editor::Mode::Replace;
        if !is_insert_or_replace {
            return CommandResult::NoOp;
        }

        if let Some(timer) = self.completion_debounce_timer {
            if timer.elapsed().as_millis() < self.completion_debounce_ms.into() {
                return CommandResult::NoOp;
            }
        }
        self.completion_debounce_timer = Some(std::time::Instant::now());

        let (buffer_id, cursor_pos) = match self.windows.active_window() {
            Some(w) => (w.buffer_id, w.cursor.position),
            None => return CommandResult::NoOp,
        };

        self.ensure_word_index_fresh();

        let (word, is_path) = {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };
            word_or_path_before_cursor(buffer, cursor_pos)
        };

        // Determine whether the cursor is in a member-access position.
        // Uses the dedicated helper so "..", "./" and bare "." are excluded.
        let after_dot = !is_path && {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };
            if let Some(line) = buffer.line_text(cursor_pos.line) {
                let word_start = cursor_pos.col.saturating_sub(word.len());
                crate::completion::is_member_dot_before(&line, word_start)
            } else {
                false
            }
        };

        let mode = if is_path {
            TriggerMode::Path
        } else if after_dot && self.config.enable_lsp {
            TriggerMode::MemberAccess
        } else {
            TriggerMode::Word
        };

        let min_len = match mode {
            TriggerMode::MemberAccess => 0,
            TriggerMode::Path => 2,
            TriggerMode::Word => self.completion.trigger_len,
        };

        log::debug!(
            "[ed/completion] trigger_completion: Manual request. Word: '{}', Mode: {:?}, Min Len req: {}",
            word,
            mode,
            min_len
        );

        if word.len() >= min_len {
            self.completion
                .open(mode, cursor_pos.col.saturating_sub(word.len()), cursor_pos.line);

            if !matches!(mode, TriggerMode::MemberAccess) {
                let word_items = self.completion.word_index.collect_matching(&word, word.len());
                self.completion.base_items.extend(word_items);
                let vocab_items = collect_vocab_words(&self.vocab, &word);
                self.completion.base_items.extend(vocab_items);
            }

            self.completion.set_prefix(&word);
            self.update_completion_ghost_text();
        }

        CommandResult::NoOp
    }

    fn maybe_update_completion(&mut self) {
        if self.block_insert.is_some() {
            return;
        }
        let is_insert_or_replace = self.mode == crate::editor::Mode::Insert || self.mode == crate::editor::Mode::Replace;
        if !is_insert_or_replace {
            return;
        }

        let (buffer_id, cursor_pos) = match self.windows.active_window() {
            Some(w) => (w.buffer_id, w.cursor.position),
            None => return,
        };

        self.ensure_word_index_fresh();

        // ── Case 1: cursor just landed on a trigger character ─────────────────
        let has_trigger = {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return,
            };
            last_char_before_cursor(buffer, cursor_pos).map(is_trigger_char).unwrap_or(false)
        };

        if has_trigger {
            // Guard: prevent double-init if already active at this exact position
            if self.completion.active && self.completion.is_member_access() {
                if let Some(ref session) = self.completion.session {
                    if session.trigger_line == cursor_pos.line && session.trigger_col == cursor_pos.col {
                        log::debug!(
                            "[ed/completion] maybe_update_completion (Case 1 Guard): \
                 Backspaced to trigger position. Resetting prefix to empty."
                        );
                        self.completion.set_prefix("");
                        self.update_completion_ghost_text();
                        self.dirty.completion = true;
                        return;
                    }
                }
            }

            // Guard: bare "." that is NOT a member-access dot (e.g. a lone dot
            // typed in an expression context with nothing to its left).
            let is_real_member_dot = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                if let Some(line) = buffer.line_text(cursor_pos.line) {
                    // cursor_pos.col is AFTER the dot, so the dot is at col-1;
                    // word_start == cursor_pos.col (nothing typed after dot yet).
                    crate::completion::is_member_dot_before(&line, cursor_pos.col)
                } else {
                    false
                }
            };

            if !is_real_member_dot || !self.config.enable_lsp {
                // Trigger char is ":" (e.g. "::" path separator) or a stray ".".
                // Let the normal word-completion path handle it or do nothing.
                log::debug!(
                    "[ed/completion] maybe_update_completion (Case 1): \
                 Trigger char present but not a valid member dot — skipping MemberAccess."
                );
                // Fall through to Cases 2/3 for the word path.
            } else {
                self.completion.open(TriggerMode::MemberAccess, cursor_pos.col, cursor_pos.line);
                self.flush_lsp_changes();
                self.request_lsp_completions();
                self.update_completion_ghost_text();
                self.dirty.completion = true;
                return;
            }
        }

        let (word, is_path) = {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return,
            };
            word_or_path_before_cursor(buffer, cursor_pos)
        };

        log::debug!(
            "[ed/completion] maybe_update_completion: Word: '{}', Path: {}, Active: {}",
            word,
            is_path,
            self.completion.active
        );

        // ── Case 2: popup already open — check boundaries & update prefix ──────
        if self.completion.active {
            if let Some(ref session) = self.completion.session {
                if cursor_pos.line != session.trigger_line || cursor_pos.col < session.trigger_col {
                    log::debug!(
                        "[ed/completion] maybe_update_completion (Case 2 Boundary): \
                     Cursor out of session bounds. Closing popup."
                    );
                    self.close_completion_popup();
                }
            }
        }

        if self.completion.active {
            // Keep MemberAccess session alive even when the prefix after the dot
            // is empty (e.g. after backspace: "foo.aa" → "foo.").
            if word.is_empty() && !self.completion.is_member_access() {
                log::debug!(
                    "[ed/completion] maybe_update_completion (Case 2): \
                 Empty word, not MemberAccess. Closing popup."
                );
                self.close_completion_popup();
                return;
            }

            let mode_changed = is_path != self.completion.is_path();
            if mode_changed {
                log::debug!(
                    "[ed/completion] maybe_update_completion (Case 2): \
                 Path mode transition. Closing popup."
                );
                self.close_completion_popup();
                return;
            }

            let session_mode = self.completion.session.as_ref().map(|s| s.mode);
            log::debug!(
                "[ed/completion] maybe_update_completion (Case 2): \
             Re-querying candidates for '{}'. Session mode: {:?}",
                word,
                session_mode
            );

            if let Some(mode) = session_mode {
                match mode {
                    TriggerMode::Word => {
                        self.completion
                            .base_items
                            .retain(|i| i.source == crate::completion::CompletionSource::Lsp);
                        let word_items = self.completion.word_index.collect_matching(&word, word.len());
                        let vocab_items = collect_vocab_words(&self.vocab, &word);
                        self.completion.base_items.extend(word_items);
                        self.completion.base_items.extend(vocab_items);
                    }
                    TriggerMode::Path => {
                        self.completion
                            .base_items
                            .retain(|i| i.source == crate::completion::CompletionSource::Lsp);
                        let path_items = {
                            let buffer = self.buffers.get(&buffer_id);
                            let base_dir = buffer.and_then(|b| b.file_path.as_deref());
                            collect_file_paths(&word, base_dir)
                        };
                        self.completion.base_items.extend(path_items);
                    }
                    TriggerMode::MemberAccess => {
                        // LSP items are accumulated; local candidates not added
                    }
                }
            }

            let still_open = self.completion.set_prefix(&word);
            if !still_open {
                log::debug!(
                    "[ed/completion] maybe_update_completion (Case 2): \
                 set_prefix returned false. Closing popup."
                );
                self.close_completion_popup();
                return;
            }

            if self.completion.is_member_access() && self.config.enable_lsp {
                let should = self
                    .completion_debounce_timer
                    .map(|t| t.elapsed().as_millis() >= 150)
                    .unwrap_or(true);
                if should {
                    self.completion_debounce_timer = Some(std::time::Instant::now());
                    self.flush_lsp_changes();
                    self.request_lsp_completions();
                }
            }

            self.update_completion_ghost_text();
            self.dirty.completion = true;
            return;
        }

        // ── Case 3: popup closed — decide whether to open ─────────────────────
        let after_dot = !is_path && {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return,
            };
            if let Some(line) = buffer.line_text(cursor_pos.line) {
                let word_start = cursor_pos.col.saturating_sub(word.len());
                crate::completion::is_member_dot_before(&line, word_start)
            } else {
                false
            }
        };

        // Case 3: opening new session
        let mode = if is_path {
            TriggerMode::Path
        } else if after_dot && self.config.enable_lsp {
            // ← add guard
            TriggerMode::MemberAccess
        } else {
            TriggerMode::Word
        };

        let min_len = match mode {
            TriggerMode::MemberAccess => 0,
            TriggerMode::Path => 2,
            TriggerMode::Word => self.completion.trigger_len,
        };

        log::debug!(
            "[ed/completion] maybe_update_completion (Case 3): \
         Mode: {:?}, word len: {}, min len: {}",
            mode,
            word.len(),
            min_len
        );

        if word.len() < min_len {
            return;
        }

        self.completion
            .open(mode, cursor_pos.col.saturating_sub(word.len()), cursor_pos.line);

        match mode {
            TriggerMode::Word => {
                let word_items = self.completion.word_index.collect_matching(&word, word.len());
                let vocab_items = collect_vocab_words(&self.vocab, &word);
                log::debug!(
                    "[ed/completion] maybe_update_completion (Case 3): \
                 WordIndex: {}, Vocab: {}",
                    word_items.len(),
                    vocab_items.len()
                );
                self.completion.base_items.extend(word_items);
                self.completion.base_items.extend(vocab_items);
                self.completion.set_prefix(&word);
                if self.completion.items.is_empty() {
                    log::debug!(
                        "[ed/completion] maybe_update_completion (Case 3): \
                     No candidates. Cancelling."
                    );
                    self.completion.cancel();
                    return;
                }
            }
            TriggerMode::Path => {
                let path_items = {
                    let buffer = self.buffers.get(&buffer_id);
                    let base_dir = buffer.and_then(|b| b.file_path.as_deref());
                    collect_file_paths(&word, base_dir)
                };
                log::debug!(
                    "[ed/completion] maybe_update_completion (Case 3): \
                 Path candidates: {}",
                    path_items.len()
                );
                self.completion.base_items.extend(path_items);
                self.completion.set_prefix(&word);
            }
            TriggerMode::MemberAccess => {
                self.completion.set_prefix(&word);
            }
        }

        if self.config.enable_lsp {
            self.flush_lsp_changes();
            self.request_lsp_completions();
        }
        self.update_completion_ghost_text();
        self.dirty.completion = true;
    }

    fn select_next_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            return CommandResult::NoOp;
        }
        let old_index = self.completion.selected_index;
        self.completion.select_next();
        log::debug!(
            "[ed/completion] select_next_completion: Selection index cycled from {} to {} (Key: {:?})",
            old_index,
            self.completion.selected_index,
            self.completion.selection_key
        );
        self.request_completion_resolve();
        self.update_completion_ghost_text();
        self.dirty.mark_completion_scroll();
        self.dirty.status_infobar = true;
        CommandResult::NoOp
    }

    fn select_prev_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            return CommandResult::NoOp;
        }
        let old_index = self.completion.selected_index;
        self.completion.select_prev();
        log::debug!(
            "[ed/completion] select_prev_completion: Selection index cycled from {} to {} (Key: {:?})",
            old_index,
            self.completion.selected_index,
            self.completion.selection_key
        );
        self.request_completion_resolve();
        self.update_completion_ghost_text();
        self.dirty.mark_completion_scroll();
        self.dirty.status_infobar = true;
        CommandResult::NoOp
    }

    fn confirm_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            log::debug!("[ed/completion] confirm_completion: Request ignored, no active session");
            return CommandResult::NoOp;
        }

        // TAB Sync Fallback: If active but the item list is empty, close the popup and return NoOp.
        // This allows key routing systems to fall back to inserting standard tabs/spaces.
        if self.completion.items.is_empty() {
            log::debug!("[ed/completion] confirm_completion: Active but items list is empty. Closing popup.");
            self.close_completion_popup();
            return CommandResult::NoOp;
        }

        let selected = self.resolve_selected_completion_item();
        log::debug!(
            "[ed/completion] confirm_completion: Resolved selection to confirm: {:?}",
            selected.as_ref().map(|i| &i.text)
        );

        let selected = match selected {
            Some(item) => item,
            None => {
                log::debug!("[ed/completion] confirm_completion: Resolution failed. Closing popup.");
                self.close_completion_popup();
                return CommandResult::NoOp;
            }
        };

        let text = selected.text;

        // Corrected trigger prefix length calculation to use Unicode graphemes
        // instead of raw byte counts. This aligns with grapheme-based columns.
        let trigger_len = self.completion.prefix.graphemes(true).count();

        log::debug!(
            "[ed/completion] confirm_completion: Initiating insertion. Target: '{}', Deleting prefix length: {}",
            text,
            trigger_len
        );

        // Delete trigger prefix before cursor
        if trigger_len > 0 {
            if let Some(window) = self.windows.active_window_mut() {
                let pos = window.cursor.position;
                let delete_start = pos.col.saturating_sub(trigger_len);
                let buffer_id = window.buffer_id;
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.delete_at(CursorPosition::new(pos.line, delete_start), trigger_len);
                    window.cursor.position.col = delete_start;
                }
            }
        }

        let snippet_res = crate::snippet::parse_snippet_for_insert(&text);
        let clean_text = snippet_res.text;
        let clean_chars_count = clean_text.chars().count();

        let overlap: usize = if let Some(window) = self.windows.active_window() {
            let pos = window.cursor.position;
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                if let Some(line_text) = buffer.line_text(pos.line) {
                    if line_text.is_ascii() {
                        let remaining = &line_text.as_bytes()[pos.col.min(line_text.len())..];
                        let remaining_str = std::str::from_utf8(remaining).unwrap_or("");
                        completion_merge_overlap(&clean_text, remaining_str)
                    } else {
                        let remaining: String = line_text.graphemes(true).skip(pos.col).collect();
                        completion_merge_overlap(&clean_text, &remaining)
                    }
                } else {
                    0
                }
            } else {
                0
            }
        } else {
            0
        };

        if overlap > 0 {
            log::debug!(
                "[ed/completion] confirm_completion: Detected overlap of {} characters. Deleting trailing overlap.",
                overlap
            );
            if let Some(window) = self.windows.active_window() {
                let pos = window.cursor.position;
                let buffer_id = window.buffer_id;
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.delete_at(CursorPosition::new(pos.line, pos.col), overlap);
                }
            }
        }

        self.insert_text_at_cursor(&clean_text);

        let mut target_char_offset = if !snippet_res.stops.is_empty() {
            snippet_res.stops[0].0
        } else {
            snippet_res.final_offset
        };

        if snippet_res.stops.is_empty() && clean_text.ends_with("()") && target_char_offset == clean_chars_count {
            target_char_offset = clean_chars_count.saturating_sub(1);
        }

        let cursor_offset_from_end = clean_chars_count.saturating_sub(target_char_offset);

        if cursor_offset_from_end > 0 {
            if let Some(window) = self.windows.active_window_mut() {
                // Corrected cursor subtraction to compute graphemes instead of byte counts
                // from the trailing characters of the clean_text insertion.
                let trailing_graphemes_count = clean_text.graphemes(true).rev().take(cursor_offset_from_end).count();
                window.cursor.position.col = window.cursor.position.col.saturating_sub(trailing_graphemes_count);
            }
        }

        let old_rect = self.popup.overlay.completion;
        self.completion.cancel();
        self.popup.overlay.completion = None;

        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.reparse_if_dirty();
            }
        }

        if let Some(rect) = old_rect {
            self.dirty.mark_popup_closed(rect);
        }
        self.update_completion_ghost_text();
        self.dirty.mark_all();

        if text.ends_with('/') {
            self.maybe_update_completion();
        }

        CommandResult::ContentChanged
    }

    fn request_completion_resolve(&mut self) {
        if !self.lsp.connected || !self.config.enable_lsp {
            return;
        }
        let lsp_item = self.completion.selected_item().and_then(|item| {
            if item.source == crate::completion::CompletionSource::Lsp && item.documentation.is_none() && item.lsp_item.is_some() {
                item.lsp_item.clone()
            } else {
                None
            }
        });

        if let Some(lsp_item) = lsp_item {
            if lsp_item.data.is_some() {
                log::debug!(
                    "[ed/completion] request_completion_resolve: Requesting resolved metadata for item '{}'",
                    lsp_item.label
                );
                let _ = self.lsp.tx.send(crate::lsp::LspMessage::ResolveCompletionItem(lsp_item));
            }
        }
    }

    fn trigger_command_completion(&mut self) {
        let raw = self.command_prompt.text();
        let input = raw.trim_start();
        if input.is_empty() {
            self.command_completion.cancel();
            return;
        }

        let (range_prefix, after_range) = strip_range_prefix(input);

        const FILE_ARG_COMMANDS: &[&str] = &[
            "e", "edit", "open", "sp", "split", "vs", "vsplit", "find", "tabe", "tabedit", "w", "write",
        ];

        if let Some(space_pos) = after_range.find(|c: char| c.is_whitespace()) {
            let cmd_name = &after_range[..space_pos];

            let is_file_cmd = FILE_ARG_COMMANDS.contains(&cmd_name)
                || self
                    .command_registry
                    .resolve(cmd_name)
                    .map(|canonical| FILE_ARG_COMMANDS.contains(&canonical))
                    .unwrap_or(false);

            if is_file_cmd {
                let arg = after_range[space_pos..].trim_start();
                let cmd_prefix = format!("{}{} ", range_prefix, cmd_name);
                let base_dir = self.current_buffer().and_then(|b| b.file_path.as_deref());

                let mut items: Vec<crate::completion::CompletionEntry> =
                    crate::completion::collect_file_completions_for_arg(if arg.is_empty() { "" } else { arg }, base_dir)
                        .into_iter()
                        .map(|entry| crate::completion::CompletionEntry {
                            text: format!("{}{}", cmd_prefix, entry.text),
                            label: format!("{}{}", cmd_prefix, entry.label),
                            ..entry
                        })
                        .collect();

                items.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.text.cmp(&b.text))
                });
                items.truncate(self.command_completion.max_items);

                if items.is_empty() {
                    self.command_completion.cancel();
                    return;
                }

                self.command_completion.open(crate::completion::TriggerMode::Word, 0, 0);
                self.command_completion.prefix = input.to_string();
                self.command_completion.base_items = items;
                self.command_completion.items = self.command_completion.filter_items_pub();
                self.command_completion.selected_index = 0;

                return;
            }
        }

        let input_lower = input.to_lowercase();

        let mut items: Vec<crate::completion::CompletionEntry> = self
            .command_registry
            .all_names()
            .iter()
            .filter(|(display_name, _)| display_name.to_lowercase().starts_with(&input_lower))
            .map(|(display_name, canonical)| {
                let desc = self
                    .command_registry
                    .get(canonical)
                    .map(|e| e.description.clone())
                    .unwrap_or_default();
                let score = crate::completion::compute_score(display_name, &input_lower);
                crate::completion::CompletionEntry {
                    text: display_name.to_string(),
                    label: display_name.to_string(),
                    detail: Some(desc),
                    documentation: None,
                    kind: crate::completion::CompletionKind::Keyword,
                    source: crate::completion::CompletionSource::BufferWords,
                    score,
                    lsp_item: None,
                }
            })
            .collect();

        if items.is_empty() {
            self.command_completion.cancel();
            return;
        }

        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.text.cmp(&b.text))
        });

        self.command_completion.open(crate::completion::TriggerMode::Word, 0, 0);
        self.command_completion.prefix = input.to_string();
        self.command_completion.base_items = items;
        self.command_completion.items = self.command_completion.filter_items_pub();
        self.command_completion.selected_index = 0;
    }

    fn ensure_word_index_fresh(&mut self) {
        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        if let Some(bid) = buffer_id {
            if self.completion.word_index_buffer_id != Some(bid) {
                // Switched to a different buffer -> full rebuild
                if let Some(buffer) = self.buffers.get(&bid) {
                    self.completion.word_index.build_from_buffer(buffer);
                    self.completion.word_index_buffer_id = Some(bid);
                }
            }
            // Otherwise, already up-to-date via incremental updates.
        }
    }
}

impl Editor {
    fn resolve_selected_completion_item(&self) -> Option<CompletionEntry> {
        // `resolve_selection` already keeps `selected_index` pointing at the
        // correct item after every re-filter and LSP merge.  Doing another
        // key-based `.find()` here can match the WRONG item when two
        // completions share the same lowercase text (e.g. "Self" vs "self").
        let item = self.completion.selected_item().cloned();
        log::debug!(
            "[ed/completion] resolve_selected_completion_item: Resolved to {:?}",
            item.as_ref().map(|i| &i.text)
        );
        item
    }
    pub fn update_word_index_after_edit(
        &mut self,
        buffer_id: crate::buffer::BufferId,
        start_line: usize,
        old_line_count: usize,
        new_line_count: usize,
    ) {
        if self.completion.word_index_buffer_id != Some(buffer_id) {
            return;
        }

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return,
        };

        if new_line_count == old_line_count {
            let text = buffer.line_text(start_line);
            // Use `.as_deref()` to convert Option<String> to Option<&str>
            self.completion.word_index.update_line(start_line, text.as_deref());
        } else {
            for line in start_line..new_line_count {
                let text = buffer.line_text(line);
                // Use `.as_deref()` here as well
                self.completion.word_index.update_line(line, text.as_deref());
            }
            for line in new_line_count..old_line_count {
                self.completion.word_index.update_line(line, None);
            }
        }
    }
}

fn strip_range_prefix(input: &str) -> (&str, &str) {
    if let Some(rest) = input.strip_prefix("'<,'>") {
        return ("'<,'>", rest.trim_start());
    }
    if let Some(rest) = input.strip_prefix("'>,'<") {
        return ("'>,'<", rest.trim_start());
    }
    if let Some(rest) = input.strip_prefix('%') {
        return ("%", rest.trim_start());
    }
    ("", input)
}
