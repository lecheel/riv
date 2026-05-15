// src/ed/search.rs
//! Search functionality — / and ? with n/N navigation, * and # word search.
//!
//! Search does NOT wrap — pressing n at the last match shows
//! "Search hit BOTTOM without match", and N at the first match shows
//! "Search hit TOP without match" (like Vim's `set nowrapscan`).

use crate::buffer::CursorPosition;
use crate::ed::buffer_ops::BufferOpsExt;
use crate::ed::command::CommandExt;
use crate::editor::{CommandResult, Editor, SearchDirection};
use crate::CommandResult::*;

pub trait SearchExt {
    fn enter_search_forward(&mut self) -> CommandResult;
    fn enter_search_backward(&mut self) -> CommandResult;
    fn execute_search(&mut self) -> CommandResult;
    fn cancel_search(&mut self) -> CommandResult;
    fn search_next(&mut self) -> CommandResult;
    fn search_prev(&mut self) -> CommandResult;
    fn find_all_matches(&self, pattern: &str) -> Vec<CursorPosition>;
    fn find_all_whole_word_matches(&self, pattern: &str) -> Vec<CursorPosition>;
    fn search_word_forward(&mut self) -> CommandResult;
    fn search_word_backward(&mut self) -> CommandResult;
    fn get_search_matches(&self) -> &[CursorPosition];
    fn is_search_active(&self) -> bool;
    fn clear_search(&mut self);
    fn is_current_search_match(&self, line: usize, col: usize) -> bool;
    fn is_search_match(&self, line: usize, col: usize) -> bool;
}

impl SearchExt for Editor {
    fn enter_search_forward(&mut self) -> CommandResult {
        self.search_direction = Some(SearchDirection::Forward);
        self.search_input_active = true;
        self.search_prompt.clear();
        self.search_matches.clear();
        self.current_search_match = 0;
        self.clear_messages();
        self.dirty.mark_all();
        ViewChanged
    }

    fn enter_search_backward(&mut self) -> CommandResult {
        self.search_direction = Some(SearchDirection::Backward);
        self.search_input_active = true;
        self.search_prompt.clear();
        self.search_matches.clear();
        self.current_search_match = 0;
        self.clear_messages();
        self.dirty.mark_all();
        ViewChanged
    }

    fn execute_search(&mut self) -> CommandResult {
        let pattern = self.search_prompt.buffer.clone();
        self.search_input_active = false;

        if pattern.is_empty() {
            if !self.search_matches.is_empty() {
                return match self.search_direction {
                    Some(SearchDirection::Forward) => self.goto_next_match(),
                    Some(SearchDirection::Backward) => self.goto_prev_match(),
                    None => NoOp,
                };
            }
            self.clear_messages();
            return ViewChanged;
        }

        self.record_search(&pattern);
        self.search_pattern = Some(pattern.clone());

        self.clear_messages();

        let matches = self.find_all_matches(&pattern);

        if matches.is_empty() {
            self.search_matches.clear();
            return Error(format!("Pattern not found: {}", pattern));
        }

        self.search_matches = matches;
        self.search_buffer_id = self.windows.active_window().map(|w| w.buffer_id);

        let result = match self.search_direction {
            Some(SearchDirection::Forward) => self.goto_next_match_from_cursor(),
            Some(SearchDirection::Backward) => self.goto_prev_match_from_cursor(),
            None => self.goto_next_match_from_cursor(),
        };

        if let Error(_) = &result {
            return result;
        }

        Message(format!(
            "[{}/{}] {}",
            self.current_search_match + 1,
            self.search_matches.len(),
            self.search_prompt.buffer
        ))
    }

    fn cancel_search(&mut self) -> CommandResult {
        self.search_input_active = false;
        self.search_prompt.clear();
        self.clear_messages();
        self.dirty.mark_all();
        ViewChanged
    }

    fn search_next(&mut self) -> CommandResult {
        if self.search_prompt.buffer.is_empty() {
            return Error("No previous search pattern".to_string());
        }

        // ── Recompute if the active buffer changed or content was modified ──
        let current_buf = self.windows.active_window().map(|w| w.buffer_id);

        let cursor = self
            .windows
            .active_window()
            .map(|w| w.cursor.position)
            .unwrap_or_default();

        let cursor_still_on_current_match = !self.search_matches.is_empty()
            && self.current_search_match < self.search_matches.len()
            && self.search_matches[self.current_search_match] == cursor;

        let need_recompute = current_buf != self.search_buffer_id
            || self.search_matches_dirty
            || !cursor_still_on_current_match;

        if need_recompute {
            self.search_matches = self.find_all_matches(&self.search_prompt.buffer);
            self.search_buffer_id = current_buf;
            self.search_matches_dirty = false;

            if self.search_matches.is_empty() {
                return Error(format!("Pattern not found: {}", self.search_prompt.buffer));
            }

            let cursor = self
                .windows
                .active_window()
                .map(|w| w.cursor.position)
                .unwrap_or_default();

            // n = "next match in search direction" — must be strictly
            // after / before the cursor so we don't re-find the match
            // we're already sitting on.
            match self.search_direction.unwrap_or(SearchDirection::Forward) {
                SearchDirection::Forward => {
                    // If cursor is already on a match, restore that match index.
                    if let Some(idx) = self.search_matches.iter().position(|pos| *pos == cursor) {
                        self.current_search_match = idx;
                    } else {
                        match self.search_matches.iter().position(|pos| *pos > cursor) {
                            Some(idx) => {
                                self.current_search_match = idx;
                                let pos = self.search_matches[idx];
                                let _ = self.move_to_match(pos);
                            }
                            None => {
                                self.current_search_match = self.search_matches.len() - 1;
                                return Error("Search hit BOTTOM without match".to_string());
                            }
                        }
                    }
                }
                SearchDirection::Backward => {
                    let prev_idx = self
                        .search_matches
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, pos)| **pos <= cursor)
                        .map(|(i, _)| i);
                    match prev_idx {
                        Some(idx) => {
                            self.current_search_match = idx;
                            let pos = self.search_matches[idx];
                            let _ = self.move_to_match(pos);
                        }
                        None => {
                            self.current_search_match = 0;
                            return Error("Search hit TOP without match".to_string());
                        }
                    }
                }
            }
        } else {
            if self.search_matches.is_empty() {
                return Error("No previous search pattern".to_string());
            }

            // Fast path — just advance the index, matches list is unchanged.
            let result = match self.search_direction {
                Some(SearchDirection::Forward) | None => self.goto_next_match(),
                Some(SearchDirection::Backward) => self.goto_prev_match(),
            };

            if let Error(_) = &result {
                return result;
            }
        }

        Message(format!(
            "[{}/{}] {}",
            self.current_search_match + 1,
            self.search_matches.len(),
            self.search_prompt.buffer
        ))
    }

    fn search_prev(&mut self) -> CommandResult {
        if self.search_prompt.buffer.is_empty() {
            return Error("No previous search pattern".to_string());
        }

        // ── Recompute if the active buffer changed or content was modified ──
        let current_buf = self.windows.active_window().map(|w| w.buffer_id);

        let cursor = self
            .windows
            .active_window()
            .map(|w| w.cursor.position)
            .unwrap_or_default();

        let cursor_still_on_current_match = !self.search_matches.is_empty()
            && self.current_search_match < self.search_matches.len()
            && self.search_matches[self.current_search_match] == cursor;

        let need_recompute = current_buf != self.search_buffer_id
            || self.search_matches_dirty
            || !cursor_still_on_current_match;

        if need_recompute {
            self.search_matches = self.find_all_matches(&self.search_prompt.buffer);
            self.search_buffer_id = current_buf;
            self.search_matches_dirty = false;

            if self.search_matches.is_empty() {
                return Error(format!("Pattern not found: {}", self.search_prompt.buffer));
            }

            let cursor = self
                .windows
                .active_window()
                .map(|w| w.cursor.position)
                .unwrap_or_default();

            // N = "previous match in search direction" — the opposite
            // of n, so strictly before (forward) or after (backward).
            match self.search_direction.unwrap_or(SearchDirection::Forward) {
                SearchDirection::Forward => {
                    let prev_idx = self
                        .search_matches
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, pos)| **pos < cursor)
                        .map(|(i, _)| i);
                    match prev_idx {
                        Some(idx) => {
                            self.current_search_match = idx;
                            let pos = self.search_matches[idx];
                            let _ = self.move_to_match(pos);
                        }
                        None => {
                            self.current_search_match = 0;
                            return Error("Search hit TOP without match".to_string());
                        }
                    }
                }
                SearchDirection::Backward => {
                    match self.search_matches.iter().position(|pos| *pos > cursor) {
                        Some(idx) => {
                            self.current_search_match = idx;
                            let pos = self.search_matches[idx];
                            let _ = self.move_to_match(pos);
                        }
                        None => {
                            self.current_search_match = self.search_matches.len() - 1;
                            return Error("Search hit BOTTOM without match".to_string());
                        }
                    }
                }
            }
        } else {
            if self.search_matches.is_empty() {
                return Error("No previous search pattern".to_string());
            }

            let result = match self.search_direction {
                Some(SearchDirection::Forward) | None => self.goto_prev_match(),
                Some(SearchDirection::Backward) => self.goto_next_match(),
            };

            if let Error(_) = &result {
                return result;
            }
        }

        Message(format!(
            "[{}/{}] {}",
            self.current_search_match + 1,
            self.search_matches.len(),
            self.search_prompt.buffer
        ))
    }

    fn find_all_matches(&self, pattern: &str) -> Vec<CursorPosition> {
        let mut matches = Vec::new();

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return matches,
        };

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return matches,
        };

        if pattern.is_empty() {
            return matches;
        }

        let text = buffer.text();

        let (search_text, search_pattern) = if self.config.case_sensitive_search {
            (text.clone(), pattern.to_string())
        } else {
            (text.to_ascii_lowercase(), pattern.to_ascii_lowercase())
        };

        let text_byte_len = search_text.len();
        let pattern_byte_len = search_pattern.len();

        let mut search_start = 0usize;
        while search_start < text_byte_len {
            match search_text[search_start..].find(&search_pattern) {
                Some(byte_offset) => {
                    let match_start_byte = search_start + byte_offset;
                    let match_start_char = text[..match_start_byte].chars().count();

                    let line = buffer.rope.char_to_line(match_start_char);
                    let line_start_char = buffer.rope.line_to_char(line);
                    let col = match_start_char - line_start_char;

                    matches.push(CursorPosition::new(line, col));

                    let match_end_byte = match_start_byte + pattern_byte_len;
                    search_start = match_end_byte.max(match_start_byte + 1);
                }
                None => break,
            }
        }

        matches
    }

    fn find_all_whole_word_matches(&self, pattern: &str) -> Vec<CursorPosition> {
        let mut matches = Vec::new();

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return matches,
        };

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return matches,
        };

        if pattern.is_empty() {
            return matches;
        }

        let text = buffer.text();

        let (search_text, search_pattern) = if self.config.case_sensitive_search {
            (text.clone(), pattern.to_string())
        } else {
            (text.to_ascii_lowercase(), pattern.to_ascii_lowercase())
        };

        let text_byte_len = search_text.len();
        let pattern_byte_len = search_pattern.len();

        let mut search_start = 0usize;
        while search_start < text_byte_len {
            match search_text[search_start..].find(&search_pattern) {
                Some(byte_offset) => {
                    let match_start_byte = search_start + byte_offset;
                    let match_end_byte = match_start_byte + pattern_byte_len;

                    let before_is_word = if match_start_byte > 0 {
                        text[..match_start_byte]
                            .chars()
                            .next_back()
                            .map(|c| c.is_alphanumeric() || c == '_')
                            .unwrap_or(false)
                    } else {
                        false
                    };

                    let after_is_word = if match_end_byte < text.len() {
                        text[match_end_byte..]
                            .chars()
                            .next()
                            .map(|c| c.is_alphanumeric() || c == '_')
                            .unwrap_or(false)
                    } else {
                        false
                    };

                    if !before_is_word && !after_is_word {
                        let match_start_char = text[..match_start_byte].chars().count();
                        let line = buffer.rope.char_to_line(match_start_char);
                        let line_start_char = buffer.rope.line_to_char(line);
                        let col = match_start_char - line_start_char;
                        matches.push(CursorPosition::new(line, col));
                    }

                    search_start = match_end_byte.max(match_start_byte + 1);
                }
                None => break,
            }
        }

        matches
    }

    /// `*` — search forward for the whole word under the cursor.
    fn search_word_forward(&mut self) -> CommandResult {
        let word = self.word_under_cursor_in_current_buffer();
        if word.is_empty() {
            return Error("No word under cursor".to_string());
        }

        self.search_prompt.buffer = word.clone();
        self.search_prompt.cursor = self.search_prompt.buffer.len();
        self.search_direction = Some(SearchDirection::Forward);
        self.search_input_active = false;

        self.record_search(&word);
        self.search_pattern = Some(word.clone());

        let matches = self.find_all_whole_word_matches(&word);

        if matches.is_empty() {
            self.search_matches.clear();
            return Error(format!("Pattern not found: {}", word));
        }

        self.search_matches = matches;
        self.search_buffer_id = self.windows.active_window().map(|w| w.buffer_id);

        let cursor = self
            .windows
            .active_window()
            .map(|w| w.cursor.position)
            .unwrap_or_default();

        // Find the first match strictly after cursor — no wrap.
        let next_idx = match self.search_matches.iter().position(|pos| *pos > cursor) {
            Some(idx) => idx,
            None => return Error("Search hit BOTTOM without match".to_string()),
        };

        self.current_search_match = next_idx;
        let pos = self.search_matches[self.current_search_match];

        let _ = self.move_to_match(pos);

        Message(format!(
            "[{}/{}] {}",
            self.current_search_match + 1,
            self.search_matches.len(),
            self.search_prompt.buffer
        ))
    }

    /// `#` — search backward for the whole word under the cursor.
    fn search_word_backward(&mut self) -> CommandResult {
        let word = self.word_under_cursor_in_current_buffer();
        if word.is_empty() {
            return Error("No word under cursor".to_string());
        }

        self.search_prompt.buffer = word.clone();
        self.search_prompt.cursor = self.search_prompt.buffer.len();
        self.search_direction = Some(SearchDirection::Backward);
        self.search_input_active = false;

        self.record_search(&word);
        self.search_pattern = Some(word.clone());
        let matches = self.find_all_whole_word_matches(&word);

        if matches.is_empty() {
            self.search_matches.clear();
            return Error(format!("Pattern not found: {}", word));
        }

        self.search_matches = matches;
        self.search_buffer_id = self.windows.active_window().map(|w| w.buffer_id);

        let cursor = self
            .windows
            .active_window()
            .map(|w| w.cursor.position)
            .unwrap_or_default();

        let word_char_len = word.chars().count();
        let reference_pos = self
            .search_matches
            .iter()
            .find(|pos| {
                pos.line == cursor.line
                    && cursor.col >= pos.col
                    && cursor.col < pos.col + word_char_len
            })
            .copied()
            .unwrap_or(cursor);

        // Find the last match strictly before the reference position — no wrap.
        let prev_idx = match self
            .search_matches
            .iter()
            .enumerate()
            .rev()
            .find(|(_, pos)| **pos < reference_pos)
            .map(|(i, _)| i)
        {
            Some(idx) => idx,
            None => return Error("Search hit TOP without match".to_string()),
        };

        self.current_search_match = prev_idx;
        let pos = self.search_matches[self.current_search_match];

        let _ = self.move_to_match(pos);

        Message(format!(
            "[{}/{}] {}",
            self.current_search_match + 1,
            self.search_matches.len(),
            self.search_prompt.buffer
        ))
    }

    fn get_search_matches(&self) -> &[CursorPosition] {
        &self.search_matches
    }

    fn is_search_active(&self) -> bool {
        !self.search_matches.is_empty()
    }

    fn clear_search(&mut self) {
        self.search_prompt.clear();
        self.search_matches.clear();
        self.search_direction = None;
        self.search_input_active = false;
        self.current_search_match = 0;
    }

    fn is_current_search_match(&self, line: usize, col: usize) -> bool {
        if self.search_matches_dirty || self.search_matches.is_empty() {
            return false;
        }
        if self.current_search_match >= self.search_matches.len() {
            return false;
        }
        let match_pos = self.search_matches[self.current_search_match];
        if match_pos.line != line {
            return false;
        }
        let pattern_len = self.search_prompt.buffer.chars().count();
        col >= match_pos.col && col < match_pos.col + pattern_len
    }

    fn is_search_match(&self, line: usize, col: usize) -> bool {
        if self.search_matches_dirty {
            return false;
        }
        let pattern_len = self.search_prompt.buffer.chars().count();
        if pattern_len == 0 {
            return false;
        }
        self.search_matches
            .iter()
            .any(|pos| pos.line == line && col >= pos.col && col < pos.col + pattern_len)
    }
}

impl Editor {
    /// Go to the next match in the list. Stops at the last match (no wrap).
    pub(crate) fn goto_next_match(&mut self) -> CommandResult {
        if self.search_matches.is_empty() {
            return NoOp;
        }
        if self.current_search_match + 1 >= self.search_matches.len() {
            return Error("Search hit BOTTOM without match".to_string());
        }
        self.current_search_match += 1;
        self.move_to_match(self.search_matches[self.current_search_match])
    }

    /// Go to the previous match in the list. Stops at the first match (no wrap).
    pub(crate) fn goto_prev_match(&mut self) -> CommandResult {
        if self.search_matches.is_empty() {
            return NoOp;
        }
        if self.current_search_match == 0 {
            return Error("Search hit TOP without match".to_string());
        }
        self.current_search_match -= 1;
        self.move_to_match(self.search_matches[self.current_search_match])
    }

    /// Go to the first match at or after the cursor. Stops at the end (no wrap).
    pub(crate) fn goto_next_match_from_cursor(&mut self) -> CommandResult {
        if self.search_matches.is_empty() {
            return NoOp;
        }
        let cursor = self
            .windows
            .active_window()
            .map(|w| w.cursor.position)
            .unwrap_or_default();

        for (i, pos) in self.search_matches.iter().enumerate() {
            if *pos >= cursor {
                self.current_search_match = i;
                return self.move_to_match(*pos);
            }
        }
        Error("Search hit BOTTOM without match".to_string())
    }

    /// Go to the last match at or before the cursor. Stops at the top (no wrap).
    pub(crate) fn goto_prev_match_from_cursor(&mut self) -> CommandResult {
        if self.search_matches.is_empty() {
            return NoOp;
        }
        let cursor = self
            .windows
            .active_window()
            .map(|w| w.cursor.position)
            .unwrap_or_default();

        for i in (0..self.search_matches.len()).rev() {
            if self.search_matches[i] <= cursor {
                self.current_search_match = i;
                return self.move_to_match(self.search_matches[i]);
            }
        }
        Error("Search hit TOP without match".to_string())
    }

    pub(crate) fn move_to_match(&mut self, pos: CursorPosition) -> CommandResult {
        let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position = pos;
            window.cursor.desired_col = None;
        }
        if let Some(buffer_id) = buffer_id {
            self.ensure_cursor_visible(&buffer_id);
        }
        self.dirty.windows = true;
        ViewChanged
    }
}
