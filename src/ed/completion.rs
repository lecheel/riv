//--+ ed/completion.rs
use crate::buffer::{Buffer, CursorPosition};
use crate::ed::lsp::LspExt;
use crate::ed::EditingExt;
use crate::editor::Editor;
use crate::CommandResult;
use unicode_segmentation::UnicodeSegmentation;

/// Extension trait for completion operations.
pub trait CompletionExt {
    fn trigger_completion(&mut self) -> CommandResult;
    fn maybe_update_completion(&mut self);
    fn select_next_completion(&mut self) -> CommandResult;
    fn select_prev_completion(&mut self) -> CommandResult;
    fn confirm_completion(&mut self) -> CommandResult;
    fn trigger_command_completion(&mut self);
}

// ── Internal helper: check if the last typed char is a trigger char ──────────
/// Compute how many graphemes at the start of `remaining` overlap with
/// a suffix of `completion`.
///
/// Used for smart merge: avoids duplicating text that already exists
/// after the cursor when confirming a completion.
///
/// Minimum overlap of 2 graphemes to avoid false single-char matches.
///
/// # Examples
/// ```
/// // "calculate" ends with "culate", remaining starts with "culate()"
/// completion_merge_overlap("calculate", "culate()")  // → 6
///
/// // No significant overlap
/// completion_merge_overlap("calculate", "foo()")     // → 0
/// ```
fn completion_merge_overlap(completion: &str, remaining: &str) -> usize {
    let comp: Vec<&str> = completion.graphemes(true).collect();
    let rem: Vec<&str> = remaining.graphemes(true).collect();

    // Find longest suffix of `completion` that matches a prefix of `remaining`
    let max = comp.len().min(rem.len());
    for len in (1..=max).rev() {
        if comp[comp.len() - len..] == rem[..len] {
            return len;
        }
    }
    0
}

/// Returns the character immediately before the cursor, or None.
fn last_char_before_cursor(buffer: &Buffer, pos: CursorPosition) -> Option<char> {
    if pos.col == 0 {
        return None;
    }
    use unicode_segmentation::UnicodeSegmentation;
    let line = buffer.line_text(pos.line)?;
    let graphemes: Vec<&str> = line.graphemes(true).collect();
    graphemes.get(pos.col - 1).and_then(|g| g.chars().next())
}

/// Returns true if `ch` is a member-access trigger (`.`) or scope
/// trigger (`:`).  We do NOT include `::` as a single unit here — the
/// caller checks the raw character.
fn is_trigger_char(ch: char) -> bool {
    ch == '.' || ch == ':'
}

impl CompletionExt for Editor {
    // ─────────────────────────────────────────────────────────────────────────
    fn trigger_completion(&mut self) -> CommandResult {
        let is_insert_or_replace =
            self.mode == crate::editor::Mode::Insert || self.mode == crate::editor::Mode::Replace;
        if !is_insert_or_replace {
            return CommandResult::NoOp;
        }

        let (buffer_id, cursor_pos) = match self.windows.active_window() {
            Some(w) => (w.buffer_id, w.cursor.position),
            None => {
                return CommandResult::NoOp;
            }
        };

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => {
                return CommandResult::NoOp;
            }
        };

        let triggered = self.completion.try_trigger(buffer, cursor_pos);

        if triggered {
            self.request_lsp_completions();
            self.dirty.completion = true;
            self.dirty.cursor = true;
            CommandResult::NoOp
        } else {
            CommandResult::NoOp
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    fn maybe_update_completion(&mut self) {
        if self.block_insert.is_some() {
            return;
        }

        let is_insert_or_replace =
            self.mode == crate::editor::Mode::Insert || self.mode == crate::editor::Mode::Replace;
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

        // ── Case 1: cursor is sitting right after a trigger character ────────
        //
        // When the user types `map.` the cursor lands at col N where
        // grapheme[N-1] == '.'.  At this point word_or_path_before_cursor
        // returns "" (nothing after the dot yet), which previously caused an
        // immediate cancel.  Instead we should:
        //   • dismiss the buffer-word popup (nothing useful to show yet), and
        //   • fire an LSP completion request so results arrive as soon as the
        //     user types the first letter after the dot.
        if let Some(last_ch) = last_char_before_cursor(buffer, cursor_pos) {
            if is_trigger_char(last_ch) {
                // ── Don't cancel! Set up a "pending" state so LSP results have
                //    somewhere to land when they arrive asynchronously.
                self.completion.active = true;
                self.completion.items.clear();
                self.completion.selected_index = 0;
                self.completion.context = Some(crate::completion::CompletionContext {
                    trigger: String::new(), // empty: we're right after the dot
                    position: cursor_pos,
                    line_text: buffer.line_text(cursor_pos.line).unwrap_or_default(),
                    is_path: false,
                });
                // Keep the overlay open (or mark dirty so render can show a loading state)
                self.dirty.completion = true;
                self.request_lsp_completions();
                return;
            }
        }

        use crate::completion::{is_path_trigger, word_or_path_before_cursor};
        let (word, is_path) = word_or_path_before_cursor(buffer, cursor_pos);

        if self.completion.active {
            // ── Case 2: completion popup is already open ─────────────────────

            if word.is_empty() {
                // Nothing left to complete (e.g. user deleted back past the
                // trigger character).
                let old_rect = self.overlay.completion;
                self.completion.cancel();
                self.overlay.completion = None;
                if let Some(rect) = old_rect {
                    self.dirty.mark_popup_closed(rect);
                }
                self.dirty.windows = true;
                self.dirty.cursor = true;
                return;
            }

            // Detect whether the trigger type changed (word ↔ path).
            let was_path = self
                .completion
                .context
                .as_ref()
                .map(|ctx| is_path_trigger(&ctx.trigger))
                .unwrap_or(false);

            if is_path != was_path {
                // Trigger type flipped — start fresh.
                let old_rect = self.overlay.completion;
                self.completion.cancel();
                self.overlay.completion = None;
                if let Some(rect) = old_rect {
                    self.dirty.mark_popup_closed(rect);
                }
                if self.completion.try_trigger(buffer, cursor_pos) {
                    self.request_lsp_completions();
                }
                self.dirty.completion = true;
                self.dirty.cursor = true;
                return;
            }

            // Detect whether the prefix context changed significantly.
            // This happens after member-access dots: the stored trigger was
            // e.g. "map" but after typing "map.in" the new word is "in".
            // In that case the existing items (scored against "map") are stale.
            let prefix_compatible = self
                .completion
                .context
                .as_ref()
                .map(|ctx| {
                    // Compatible if the new word extends or shrinks the old
                    // trigger within the same identifier segment.
                    word.starts_with(&ctx.trigger) || ctx.trigger.starts_with(&word)
                })
                .unwrap_or(false);

            if !prefix_compatible && !is_path {
                // Prefix diverged (e.g. after a dot) — re-trigger fresh.
                let old_rect = self.overlay.completion;
                self.completion.cancel();
                self.overlay.completion = None;
                if let Some(rect) = old_rect {
                    self.dirty.mark_popup_closed(rect);
                }
                if self.completion.try_trigger(buffer, cursor_pos) {
                    self.request_lsp_completions();
                }
                self.dirty.completion = true;
                self.dirty.cursor = true;
                return;
            }

            // Normal case: same identifier segment, just filter/score existing items.
            if is_path {
                self.completion.update_path(buffer, &word);
            } else {
                self.completion.update(&word);
            }

            // Request fresh LSP completions when the trigger is long enough.
            if word.len() >= 1 {
                self.request_lsp_completions();
            }
        } else {
            // Case 3: no popup open — try to auto-trigger
            let (word, is_path) = crate::completion::word_or_path_before_cursor(buffer, cursor_pos);

            // ── Detect member-access context: word is short but follows a dot.
            //    Example: typing `map.i` → word = "i" (1 char), but LSP
            //    should still be asked because this is member access.
            let after_dot = !is_path && {
                if let Some(line) = buffer.line_text(cursor_pos.line) {
                    let graphemes: Vec<&str> = line.graphemes(true).collect();
                    // Look for '.' immediately before the trigger word
                    let word_start = cursor_pos.col.saturating_sub(word.len());
                    graphemes.get(word_start.saturating_sub(1)) == Some(&".")
                } else {
                    false
                }
            };

            let triggered = self.completion.try_trigger(buffer, cursor_pos);

            // ── Always fire LSP for member-access or if buffer words triggered.
            //    try_trigger may return false when word is 1 char (below trigger_len),
            //    but LSP completions are the primary source after a dot.
            if triggered || after_dot {
                self.request_lsp_completions();
            }
        }

        if self.completion.active {
            self.dirty.completion = true;
            self.dirty.cursor = true;
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    fn select_next_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            return CommandResult::NoOp;
        }
        self.completion.select_next();
        self.dirty.mark_completion_scroll();
        CommandResult::NoOp
    }

    fn select_prev_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            return CommandResult::NoOp;
        }
        self.completion.select_prev();
        self.dirty.mark_completion_scroll();
        CommandResult::NoOp
    }

    fn confirm_completion(&mut self) -> CommandResult {
        if !self.completion.active {
            return CommandResult::NoOp;
        }

        if let Some((text, trigger_len)) = self.completion.confirm() {
            // ── Step 1: Delete the trigger text before cursor ──────────────
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

            // ── Step 2: Smart merge — delete overlapping suffix ────────────
            //
            // When the cursor is mid-word, the completion's suffix may already
            // exist after the cursor.  Detect and remove the overlap so we
            // don't duplicate it.
            //
            //   Example:  `cal|culate()`  + completion `calculate`
            //     → overlap = 6  ("culate")
            //     → delete 6 graphemes after cursor  → `|()`
            //     → insert `calculate`               → `calculate()`
            //
            let overlap: usize = if let Some(window) = self.windows.active_window() {
                let pos = window.cursor.position;
                let buffer_id = window.buffer_id;
                if let Some(buffer) = self.buffers.get(&buffer_id) {
                    if let Some(line_text) = buffer.line_text(pos.line) {
                        let remaining: String = line_text.graphemes(true).skip(pos.col).collect();
                        completion_merge_overlap(&text, &remaining)
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

            // ── Step 3: Insert the full completion text ────────────────────
            self.insert_text_at_cursor(&text);

            self.overlay.completion = None;
            self.dirty.mark_insert();

            // If we completed a directory (ends with '/'), immediately trigger
            // path completion for entries inside it.
            if text.ends_with('/') {
                self.maybe_update_completion();
            }
            self.dirty.mark_all();
            CommandResult::ContentChanged
        } else {
            self.overlay.completion = None;
            self.dirty.mark_all();
            CommandResult::NoOp
        }
    }

    fn trigger_command_completion(&mut self) {
        let input = self.command_prompt.text().trim();
        if input.is_empty() {
            self.command_completion.cancel();
            return;
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
                    detail: Some(desc),
                    documentation: None,
                    kind: crate::completion::CompletionKind::Keyword,
                    source: crate::completion::CompletionSource::BufferWords,
                    score,
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
        });
    }
}
