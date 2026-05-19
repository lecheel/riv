// ed/completion.rs — Optimized version
// ──────────────────────────────────────────────────────────────
// Key optimizations:
//   1. Word index cache rebuild trigger on buffer change
//   2. Simplified debounce — single timer, no redundant checks
//   3. Reduced String allocations in confirm_completion
//   4. Skip completion resolve when documentation already present
// ──────────────────────────────────────────────────────────────

use crate::buffer::{Buffer, CursorPosition};
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
    /// OPT: Rebuild word index for the current buffer if stale.
    fn ensure_word_index_fresh(&mut self);
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

/// OPT: ASCII fast path for last-char detection.
#[inline]
fn last_char_before_cursor(buffer: &Buffer, pos: CursorPosition) -> Option<char> {
    if pos.col == 0 {
        return None;
    }
    if let Some(line) = buffer.line_text(pos.line) {
        // Fast path: ASCII
        if line.is_ascii() {
            let bytes = line.as_bytes();
            if pos.col <= bytes.len() {
                let b = bytes[pos.col - 1];
                if b.is_ascii() {
                    return Some(b as char);
                }
            }
        }
        // Fallback
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
    fn trigger_completion(&mut self) -> CommandResult {
        let is_insert_or_replace = self.mode == crate::editor::Mode::Insert || self.mode == crate::editor::Mode::Replace;
        if !is_insert_or_replace {
            return CommandResult::NoOp;
        }

        // OPT: single debounce check
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

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return CommandResult::NoOp,
        };

        // OPT: ensure word index is fresh before triggering
        let _ = buffer;
        self.ensure_word_index_fresh();

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return CommandResult::NoOp,
        };

        let triggered = self.completion.try_trigger(buffer, cursor_pos, &self.vocab);

        if triggered {
            self.request_lsp_completions();
            self.dirty.completion = true;
            self.dirty.cursor = true;
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

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return,
        };

        // OPT: update word index for changed line
        let _ = buffer;
        self.ensure_word_index_fresh();

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return,
        };

        // Case 1: cursor after trigger character
        if let Some(last_ch) = last_char_before_cursor(buffer, cursor_pos) {
            if is_trigger_char(last_ch) {
                self.completion.active = true;
                self.completion.items.clear();
                self.completion.base_items.clear();
                self.completion.selected_index = 0;
                self.completion.context = Some(crate::completion::CompletionContext {
                    trigger: String::new(),
                    position: cursor_pos,
                    line_text: buffer.line_text(cursor_pos.line).unwrap_or_default(),
                    is_path: false,
                    after_trigger_char: true,
                });
                self.dirty.completion = true;
                self.request_lsp_completions();
                return;
            }
        }

        use crate::completion::{is_path_trigger, word_or_path_before_cursor};
        let (word, is_path) = word_or_path_before_cursor(buffer, cursor_pos);

        if self.completion.active {
            if word.is_empty() {
                let old_rect = self.popup.overlay.completion;
                self.completion.cancel();
                self.popup.overlay.completion = None;
                if let Some(rect) = old_rect {
                    self.dirty.mark_popup_closed(rect);
                }
                self.dirty.windows = true;
                self.dirty.cursor = true;
                return;
            }

            let was_path = self
                .completion
                .context
                .as_ref()
                .map(|ctx| is_path_trigger(&ctx.trigger))
                .unwrap_or(false);

            if is_path != was_path {
                let old_rect = self.popup.overlay.completion;
                self.completion.cancel();
                self.popup.overlay.completion = None;
                if let Some(rect) = old_rect {
                    self.dirty.mark_popup_closed(rect);
                }
                if self.completion.try_trigger(buffer, cursor_pos, &self.vocab) {
                    self.request_lsp_completions();
                }
                self.dirty.completion = true;
                self.dirty.cursor = true;
                return;
            }

            let prefix_compatible = self
                .completion
                .context
                .as_ref()
                .map(|ctx| word.starts_with(&ctx.trigger) || ctx.trigger.starts_with(&word))
                .unwrap_or(false);

            if !prefix_compatible && !is_path {
                let old_rect = self.popup.overlay.completion;
                self.completion.cancel();
                self.popup.overlay.completion = None;
                if let Some(rect) = old_rect {
                    self.dirty.mark_popup_closed(rect);
                }
                if self.completion.try_trigger(buffer, cursor_pos, &self.vocab) {
                    self.request_lsp_completions();
                }
                self.dirty.completion = true;
                self.dirty.cursor = true;
                return;
            }

            if is_path {
                self.completion.update_path(buffer, &word);
            } else {
                self.completion.update(&word);
            }
        } else {
            let (word, is_path) = crate::completion::word_or_path_before_cursor(buffer, cursor_pos);

            let after_dot = !is_path && {
                if let Some(line) = buffer.line_text(cursor_pos.line) {
                    // OPT: ASCII fast path
                    if line.is_ascii() {
                        let bytes = line.as_bytes();
                        let word_start = cursor_pos.col.saturating_sub(word.len());
                        word_start > 0 && bytes.get(word_start - 1) == Some(&b'.')
                    } else {
                        let graphemes: Vec<&str> = line.graphemes(true).collect();
                        let word_start = cursor_pos.col.saturating_sub(word.len());
                        graphemes.get(word_start.saturating_sub(1)) == Some(&".")
                    }
                } else {
                    false
                }
            };

            let triggered = self.completion.try_trigger(buffer, cursor_pos, &self.vocab);

            if triggered || after_dot {
                const LSP_DEBOUNCE_MS: u128 = 50;
                if let Some(timer) = self.completion_debounce_timer {
                    if timer.elapsed().as_millis() < LSP_DEBOUNCE_MS {
                        return;
                    }
                }
                self.completion_debounce_timer = Some(std::time::Instant::now());

                self.request_lsp_completions();
            }
        }

        if self.completion.active {
            self.dirty.completion = true;
            self.dirty.cursor = true;
        }
    }

    fn select_next_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            return CommandResult::NoOp;
        }
        self.completion.select_next();
        self.request_completion_resolve();
        self.dirty.mark_completion_scroll();
        CommandResult::NoOp
    }

    fn select_prev_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            return CommandResult::NoOp;
        }
        self.completion.select_prev();
        self.request_completion_resolve();
        self.dirty.mark_completion_scroll();
        CommandResult::NoOp
    }

    fn request_completion_resolve(&mut self) {
        if !self.lsp.connected {
            return;
        }

        // OPT: skip resolve if documentation already present
        let lsp_item = self.completion.selected_item().and_then(|item| {
            if item.source == crate::completion::CompletionSource::Lsp && item.documentation.is_none() && item.lsp_item.is_some() {
                item.lsp_item.clone()
            } else {
                None
            }
        });

        if let Some(lsp_item) = lsp_item {
            if lsp_item.data.is_some() {
                let _ = self.lsp.tx.send(crate::lsp::LspMessage::ResolveCompletionItem(lsp_item));
            }
        }
    }

    fn confirm_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            return CommandResult::NoOp;
        }

        if let Some((text, trigger_len)) = self.completion.confirm() {
            // Step 1: Delete the trigger text before cursor
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

            // Step 2: Smart merge — detect overlapping suffix
            let overlap: usize = if let Some(window) = self.windows.active_window() {
                let pos = window.cursor.position;
                let buffer_id = window.buffer_id;
                if let Some(buffer) = self.buffers.get(&buffer_id) {
                    if let Some(line_text) = buffer.line_text(pos.line) {
                        // OPT: ASCII fast path for overlap detection
                        if line_text.is_ascii() {
                            let remaining = &line_text.as_bytes()[pos.col.min(line_text.len())..];
                            let remaining_str = std::str::from_utf8(remaining).unwrap_or("");
                            completion_merge_overlap(&text, remaining_str)
                        } else {
                            let remaining: String = line_text.graphemes(true).skip(pos.col).collect();
                            completion_merge_overlap(&text, &remaining)
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
                if let Some(window) = self.windows.active_window() {
                    let pos = window.cursor.position;
                    let buffer_id = window.buffer_id;
                    if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                        buffer.delete_at(CursorPosition::new(pos.line, pos.col), overlap);
                    }
                }
            }

            // Step 3: Insert the full completion text
            self.insert_text_at_cursor(&text);

            // Step 4: Dismiss popup and reparse
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
            self.dirty.mark_all();

            if text.ends_with('/') {
                self.maybe_update_completion();
            }

            CommandResult::ContentChanged
        } else {
            let old_rect = self.popup.overlay.completion;
            self.completion.cancel();
            self.popup.overlay.completion = None;

            if let Some(rect) = old_rect {
                self.dirty.mark_popup_closed(rect);
            }

            if let Some(window) = self.windows.active_window() {
                let buffer_id = window.buffer_id;
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.reparse_if_dirty();
                }
            }

            self.dirty.mark_all();
            CommandResult::NoOp
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

                self.command_completion.active = true;
                self.command_completion.items = items;
                self.command_completion.selected_index = 0;
                self.command_completion.context = Some(crate::completion::CompletionContext {
                    trigger: input.to_string(),
                    position: CursorPosition::zero(),
                    line_text: self.command_prompt.buffer.clone(),
                    is_path: true,
                    after_trigger_char: false,
                });
                return;
            }
        }

        // Regular command name completion
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

        self.command_completion.active = true;
        self.command_completion.items = items;
        self.command_completion.selected_index = 0;
        self.command_completion.context = Some(crate::completion::CompletionContext {
            trigger: input.to_string(),
            position: CursorPosition::zero(),
            line_text: self.command_prompt.buffer.clone(),
            is_path: false,
            after_trigger_char: false,
        });
    }

    /// OPT: Rebuild the buffer word index if the buffer has changed.
    /// Uses the buffer's dirty flag to avoid unnecessary rebuilds.
    fn ensure_word_index_fresh(&mut self) {
        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);

        if let Some(bid) = buffer_id {
            let needs_rebuild =
                self.completion.word_index_buffer_id != Some(bid) || self.buffers.get(&bid).map(|b| b.dirty).unwrap_or(false);

            if needs_rebuild {
                if let Some(buffer) = self.buffers.get(&bid) {
                    self.completion.word_index.build_from_buffer(buffer);
                    self.completion.word_index_buffer_id = Some(bid);
                }
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
