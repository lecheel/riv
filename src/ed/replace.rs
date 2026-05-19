use crate::buffer::CursorPosition;
use crate::ed::editing::EditingExt;
use crate::ed::git::GitExt;
use crate::ed::lsp::LspExt;
use crate::editor::{CommandResult, Editor, SubstituteConfirmState};
use unicode_segmentation::UnicodeSegmentation;

pub trait ReplaceExt {
    fn start_substitute_confirm(
        &mut self,
        regex: regex::Regex,
        replacement: String,
        global: bool,
        start_line: usize,
        end_line: usize,
    ) -> CommandResult;

    fn is_substitute_confirm_active(&self) -> bool;
    fn substitute_confirm_prompt(&self) -> Option<String>;
    fn show_fmt_info_popup(&mut self, title: &str, text: &str);

    // Interactive substitution commands
    fn substitute_confirm_yes(&mut self) -> CommandResult;
    fn substitute_confirm_no(&mut self) -> CommandResult;
    fn substitute_confirm_all(&mut self) -> CommandResult;
    fn substitute_confirm_quit(&mut self) -> CommandResult;
    fn substitute_confirm_last(&mut self) -> CommandResult;
}

impl ReplaceExt for Editor {
    // ── Substitute confirmation ─────────────────────────────────────

    /// Start interactive substitute confirmation mode.
    fn start_substitute_confirm(
        &mut self,
        regex: regex::Regex,
        replacement: String,
        global: bool,
        start_line: usize,
        end_line: usize,
    ) -> CommandResult {
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self.buffers.get(&buffer_id).map(|b| b.line_count()).unwrap_or(0);
        let end_line = end_line.min(line_count.saturating_sub(1));

        self.ensure_undo_group();

        self.search.substitute_confirm = Some(SubstituteConfirmState {
            regex,
            replacement,
            global,
            buffer_id,
            start_line,
            end_line,
            subs_made: 0,
            next_line: start_line,
            next_byte_offset: 0,
            current_match: None,
        });

        self.substitute_advance()
    }

    /// **y** — replace current match, then advance.
    fn substitute_confirm_yes(&mut self) -> CommandResult {
        let mut state = match self.search.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        if let Some((line, start_byte, end_byte)) = state.current_match {
            let matched_text = {
                let buffer = match self.buffers.get(&state.buffer_id) {
                    Some(b) => b,
                    None => return CommandResult::NoOp,
                };
                let line_text = buffer.line_text(line).unwrap_or_default().trim_end_matches('\n').to_string();
                line_text[start_byte..end_byte].to_string()
            };
            let replaced_text = state.regex.replace(&matched_text, state.replacement.as_str()).to_string();
            let new_match_end_byte = start_byte + replaced_text.len();

            self.substitute_perform_one(line, start_byte, end_byte, &state);
            state.subs_made += 1;

            self.invalidate_git_gutter();
            self.notify_lsp_change();

            if state.global {
                let new_line_len = self
                    .buffers
                    .get(&state.buffer_id)
                    .and_then(|b| b.line_text(line))
                    .map(|t| t.trim_end_matches('\n').len())
                    .unwrap_or(0);
                state.next_byte_offset = new_match_end_byte.min(new_line_len);
                state.next_line = line;
            } else {
                state.next_line = line + 1;
                state.next_byte_offset = 0;
            }

            state.current_match = None;
            self.search.substitute_confirm = Some(state);

            // Content changed → mark for redraw
            self.dirty.windows = true;
            self.dirty.cursor = true;

            // substitute_advance will set status and additional dirty flags
            let result = self.substitute_advance();

            // If substitute_advance found another match, it returned ViewChanged.
            // We also changed content, so ensure the insert region is marked dirty.
            if result == CommandResult::ViewChanged {
                self.dirty.mark_insert();
            }

            result
        } else {
            self.search.substitute_confirm = None;
            self.close_undo_group();
            self.invalidate_git_gutter();
            self.notify_lsp_change();
            self.dirty.mark_all();
            CommandResult::ViewChanged
        }
    }

    /// **n** — skip current match, then advance.
    fn substitute_confirm_no(&mut self) -> CommandResult {
        let mut state = match self.search.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        if let Some((line, start_byte, end_byte)) = state.current_match {
            if state.global {
                state.next_line = line;
                state.next_byte_offset = end_byte;
            } else {
                state.next_line = line + 1;
                state.next_byte_offset = 0;
            }

            state.current_match = None;
            self.search.substitute_confirm = Some(state);

            // substitute_advance handles status and dirty flags
            self.substitute_advance()
        } else {
            self.search.substitute_confirm = None;
            self.close_undo_group();
            self.dirty.mark_all();
            CommandResult::ViewChanged
        }
    }

    /// **a** — replace this and all remaining matches without prompting.
    fn substitute_confirm_all(&mut self) -> CommandResult {
        let mut state = match self.search.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        // Replace the current match first
        if let Some((line, start_byte, end_byte)) = state.current_match {
            // Compute replacement byte length before mutating
            let matched_text = {
                let buffer = match self.buffers.get(&state.buffer_id) {
                    Some(b) => b,
                    None => return CommandResult::NoOp,
                };
                let line_text = buffer.line_text(line).unwrap_or_default().trim_end_matches('\n').to_string();
                line_text[start_byte..end_byte].to_string()
            };
            let replaced_text = state.regex.replace(&matched_text, state.replacement.as_str()).to_string();
            let new_match_end_byte = start_byte + replaced_text.len();

            self.substitute_perform_one(line, start_byte, end_byte, &state);
            state.subs_made += 1;

            if state.global {
                let new_line_len = self
                    .buffers
                    .get(&state.buffer_id)
                    .and_then(|b| b.line_text(line))
                    .map(|t| t.trim_end_matches('\n').len())
                    .unwrap_or(0);
                state.next_byte_offset = new_match_end_byte.min(new_line_len);
                state.next_line = line;
            } else {
                state.next_line = line + 1;
                state.next_byte_offset = 0;
            }
        }

        // Now replace ALL remaining matches without prompting
        self.substitute_confirm_replace_rest(&mut state);

        let subs_made = state.subs_made;

        // Clear search highlighting
        self.search.matches.clear();
        self.search.matches_dirty = false;
        self.search.prompt.buffer.clear();
        self.search.prompt.cursor = 0;

        self.close_undo_group();
        self.invalidate_git_gutter();
        self.notify_lsp_change();

        // Full redraw to clear highlights and show all changes
        self.dirty.mark_all();

        if subs_made > 0 {
            CommandResult::Message(format!("{} substitutions", subs_made))
        } else {
            CommandResult::Error("Pattern not found".into())
        }
    }

    /// **q** or **Escape** — abort without replacing current match.
    fn substitute_confirm_quit(&mut self) -> CommandResult {
        let state = match self.search.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        let subs_made = state.subs_made;

        // Clear search highlighting
        self.search.matches.clear();
        self.search.matches_dirty = false;
        self.search.prompt.buffer.clear();
        self.search.prompt.cursor = 0;

        self.close_undo_group();

        // If any substitutions were made earlier (y/l), notify
        if subs_made > 0 {
            self.invalidate_git_gutter();
            self.notify_lsp_change();
        }

        // Full redraw to remove highlight
        self.dirty.mark_all();

        if subs_made > 0 {
            CommandResult::Message(format!("{} substitutions — quit at current match", subs_made))
        } else {
            CommandResult::Message("Quit — no substitutions made".into())
        }
    }

    /// **l** — replace this match, then quit (last replacement).
    fn substitute_confirm_last(&mut self) -> CommandResult {
        let mut state = match self.search.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        if let Some((line, start_byte, end_byte)) = state.current_match {
            self.substitute_perform_one(line, start_byte, end_byte, &state);
            state.subs_made += 1;
        }

        let subs_made = state.subs_made;

        // Clear search highlighting
        self.search.matches.clear();
        self.search.matches_dirty = false;
        self.search.prompt.buffer.clear();
        self.search.prompt.cursor = 0;

        self.close_undo_group();
        self.invalidate_git_gutter();
        self.notify_lsp_change();

        // Full redraw to remove highlight and show content change
        self.dirty.mark_all();

        CommandResult::Message(format!("{} substitutions — last", subs_made))
    }

    /// Whether the substitute confirm prompt is active.
    fn is_substitute_confirm_active(&self) -> bool {
        self.search.substitute_confirm.is_some()
    }

    /// Get the substitute confirm prompt text, if active.
    fn substitute_confirm_prompt(&self) -> Option<String> {
        self.search
            .substitute_confirm
            .as_ref()
            .map(|state| format!("replace with \"{}\"? (y/n/a/q/l)", state.replacement))
    }

    /// Show the format-info popup with a title and multi-line error text.
    /// Automatically splits the text into lines and marks the view dirty.
    fn show_fmt_info_popup(&mut self, title: &str, text: &str) {
        self.popup.fmt_info_title = title.to_string();
        self.popup.fmt_info = Some(text.lines().map(|l| l.to_string()).collect());
        self.dirty.mark_all();
    }
}

// Internal implementation - helper methods that don't need to be in the trait
impl Editor {
    /// Find the next match, highlight it, and show the prompt.
    /// If no more matches exist, finish the session and show the summary.
    pub(crate) fn substitute_advance(&mut self) -> CommandResult {
        let state = match self.search.substitute_confirm.take() {
            Some(s) => s,
            None => return CommandResult::NoOp,
        };

        let match_info = self.find_substitute_match(&state);

        match match_info {
            Some((line, start_byte, end_byte, match_text)) => {
                let col = self.byte_offset_to_col(state.buffer_id, line, start_byte);
                if let Some(w) = self.windows.active_window_mut() {
                    w.cursor.position = CursorPosition::new(line, col);
                    w.cursor.desired_col = None;
                }
                self.ensure_cursor_visible(&state.buffer_id);

                // Set up search highlighting for the current match only
                self.search.matches = vec![CursorPosition::new(line, col)];
                self.search.current_match = 0;
                self.search.matches_dirty = false;
                self.search.prompt.buffer = match_text;
                self.search.prompt.cursor = self.search.prompt.buffer.len();

                // Build status message BEFORE moving `state`
                let status_msg = format!("replace with \"{}\"? (y/n/a/q/l)", state.replacement);

                let next_state = SubstituteConfirmState {
                    next_line: if state.global { line } else { line + 1 },
                    next_byte_offset: if state.global { end_byte } else { 0 },
                    current_match: Some((line, start_byte, end_byte)),
                    ..state
                };

                self.search.substitute_confirm = Some(next_state);
                self.set_status(status_msg);

                // Explicit dirty management
                self.dirty.windows = true;
                self.dirty.cursor = true;
                self.dirty.status_powerline = true;
                self.dirty.status_cmdline = true;
                self.dirty.status_infobar = true;

                // ViewChanged ensures process_event also sets these dirty flags
                CommandResult::ViewChanged
            }
            None => {
                let subs_made = state.subs_made;

                // Clear search highlighting
                self.search.matches.clear();
                self.search.matches_dirty = false;
                self.search.prompt.buffer.clear();
                self.search.prompt.cursor = 0;

                self.close_undo_group();
                self.invalidate_git_gutter();
                self.notify_lsp_change();

                // Full redraw: remove highlight, update content, update status
                self.dirty.mark_all();

                if subs_made > 0 {
                    CommandResult::Message(format!("{} substitutions", subs_made))
                } else {
                    CommandResult::Error("Pattern not found".into())
                }
            }
        }
    }

    /// Search for the next match starting from `state.next_line` / `next_byte_offset`.
    fn find_substitute_match(&self, state: &SubstituteConfirmState) -> Option<(usize, usize, usize, String)> {
        let buffer = self.buffers.get(&state.buffer_id)?;

        let mut line = state.next_line.max(state.start_line);
        let mut byte_offset = state.next_byte_offset;

        while line <= state.end_line && line < buffer.line_count() {
            let line_text = buffer.line_text(line)?.trim_end_matches('\n').to_string();

            let mat = state.regex.find_iter(&line_text).find(|m| m.start() >= byte_offset);

            if let Some(m) = mat {
                return Some((line, m.start(), m.end(), m.as_str().to_string()));
            }

            line += 1;
            byte_offset = 0;
        }

        None
    }

    /// Convert a byte offset within a line to a grapheme column.
    fn byte_offset_to_col(&self, buffer_id: crate::buffer::BufferId, line: usize, byte_offset: usize) -> usize {
        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return 0,
        };
        let line_text = match buffer.line_text(line) {
            Some(t) => t.trim_end_matches('\n').to_string(),
            None => return 0,
        };
        let safe = byte_offset.min(line_text.len());
        line_text[..safe].graphemes(true).count()
    }

    /// Replace a single match on a line in‑place.
    fn substitute_perform_one(&mut self, line: usize, start_byte: usize, end_byte: usize, state: &SubstituteConfirmState) {
        let buffer = match self.buffers.get_mut(&state.buffer_id) {
            Some(b) => b,
            None => return,
        };

        let line_text = buffer.line_text(line).unwrap_or_default().trim_end_matches('\n').to_string();

        // Build the new line by splicing: prefix + replacement + suffix
        let matched_text = &line_text[start_byte..end_byte];
        let replaced = state.regex.replace(matched_text, state.replacement.as_str()).to_string();
        let new_line_text = format!("{}{}{}", &line_text[..start_byte], replaced, &line_text[end_byte..]);

        // Swap the entire line content
        let old_len = line_text.graphemes(true).count();
        if old_len > 0 {
            buffer.delete_at(CursorPosition::new(line, 0), old_len);
        }
        if !new_line_text.is_empty() {
            buffer.insert_at(CursorPosition::new(line, 0), &new_line_text);
        }
        buffer.dirty = true;
    }

    /// Replace all remaining matches without prompting (helper for "a" key).
    fn substitute_confirm_replace_rest(&mut self, state: &mut SubstituteConfirmState) {
        let buffer_id = state.buffer_id;

        let mut line = state.next_line.max(state.start_line);
        let mut byte_offset = state.next_byte_offset;

        while line <= state.end_line {
            let line_text = {
                let buffer = match self.buffers.get(&buffer_id) {
                    Some(b) => b,
                    None => return,
                };
                match buffer.line_text(line) {
                    Some(t) => t.trim_end_matches('\n').to_string(),
                    None => {
                        line += 1;
                        byte_offset = 0;
                        continue;
                    }
                }
            };

            let matches: Vec<_> = if state.global {
                state.regex.find_iter(&line_text).filter(|m| m.start() >= byte_offset).collect()
            } else if byte_offset == 0 {
                match state.regex.find_iter(&line_text).next() {
                    Some(m) => vec![m],
                    None => vec![],
                }
            } else {
                vec![]
            };

            if matches.is_empty() {
                line += 1;
                byte_offset = 0;
                continue;
            }

            // Replace all matches on this line at once
            let new_text = if state.global {
                if byte_offset > 0 {
                    let prefix = &line_text[..byte_offset];
                    let suffix = &line_text[byte_offset..];
                    let replaced = state.regex.replace_all(suffix, state.replacement.as_str()).to_string();
                    format!("{}{}", prefix, replaced)
                } else {
                    state.regex.replace_all(&line_text, state.replacement.as_str()).to_string()
                }
            } else {
                state.regex.replace(&line_text, state.replacement.as_str()).to_string()
            };

            let old_len = line_text.graphemes(true).count();
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                if old_len > 0 {
                    buffer.delete_at(CursorPosition::new(line, 0), old_len);
                }
                if !new_text.is_empty() {
                    buffer.insert_at(CursorPosition::new(line, 0), &new_text);
                }
                buffer.dirty = true;
            }

            state.subs_made += matches.len();

            line += 1;
            byte_offset = 0;
        }
    }

    /// Update `next_line` / `next_byte_offset` after a replacement at `(line, start_byte, end_byte)`.
    fn substitute_update_search_pos(&self, state: &mut SubstituteConfirmState, line: usize, start_byte: usize, end_byte: usize) {
        if state.global {
            // After the replacement the byte offset shifts.
            // new_offset = end_byte + (new_line_len − old_line_len)
            let old_len = self
                .buffers
                .get(&state.buffer_id)
                .and_then(|b| b.line_text(line))
                .map(|t| t.trim_end_matches('\n').len())
                .unwrap_or(0);
            // We already mutated the buffer, so this is the *new* line.
            // But the caller should have already performed the replacement,
            // so old_len is actually the *new* length. We need the old length
            // passed in separately — instead, compute delta from the match.
            // Simplified: just search from start_byte on the same line.
            state.next_line = line;
            state.next_byte_offset = start_byte;
        } else {
            state.next_line = line + 1;
            state.next_byte_offset = 0;
        }
    }
}
