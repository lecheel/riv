//--+ prompt.rs
//! src/prompt.rs
//! Reusable mini input prompt with line editing and history.

use crate::terminal::Key;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    None,
    Changed,
    Submit,
    Cancel,
}

const MAX_PROMPT_LEN: usize = 99;

#[derive(Debug, Clone)]
pub struct MiniInputPrompt {
    pub buffer: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_index: usize,
    pub draft: Option<String>,
}

impl MiniInputPrompt {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: 0,
            draft: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = self.history.len();
        self.draft = None;
    }

    pub fn push_history(&mut self, entry: String) {
        if entry.is_empty() {
            return;
        }
        // Remove any previous exact match so it moves to the end (most recent)
        self.history.retain(|c| c != &entry);
        self.history.push(entry);
        self.history_index = self.history.len();
        self.draft = None;
    }

    pub fn handle_key(&mut self, key: &Key) -> PromptAction {
        match key {
            Key::Char(c) => {
                // ── Enforce max length ──
                if self.buffer.chars().count() >= MAX_PROMPT_LEN {
                    return PromptAction::None;
                }
                self.buffer.insert(self.cursor, *c);
                self.cursor += c.len_utf8();
                PromptAction::Changed
            }
            Key::Paste(ref text) => {
                // ── Truncate paste to fit within max length ──
                let current_len = self.buffer.chars().count();
                let available = MAX_PROMPT_LEN.saturating_sub(current_len);
                if available == 0 {
                    return PromptAction::None;
                }

                // Sanitize: replace newlines with spaces to prevent prompt overflow
                // and join pasted lines into a single continuous string.
                let sanitized: String = text
                    .chars()
                    .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                    .take(available)
                    .collect();

                if sanitized.is_empty() {
                    return PromptAction::None;
                }

                let byte_len = sanitized.len();
                self.buffer.insert_str(self.cursor, &sanitized);
                self.cursor += byte_len;
                PromptAction::Changed
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    let prev = self.buffer[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
                    self.buffer.drain(prev..self.cursor);
                    self.cursor = prev;
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Delete => {
                if self.cursor < self.buffer.len() {
                    let next = self.buffer[self.cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.cursor + i)
                        .unwrap_or(self.buffer.len());
                    self.buffer.drain(self.cursor..next);
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Left | Key::Ctrl('b') => {
                if self.cursor > 0 {
                    self.cursor = self.buffer[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Right | Key::Ctrl('f') => {
                if self.cursor < self.buffer.len() {
                    self.cursor = self.buffer[self.cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.cursor + i)
                        .unwrap_or(self.buffer.len());
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Home | Key::Ctrl('a') => {
                if self.cursor != 0 {
                    self.cursor = 0;
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::End | Key::Ctrl('e') => {
                if self.cursor != self.buffer.len() {
                    self.cursor = self.buffer.len();
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Ctrl('k') => {
                if self.cursor < self.buffer.len() {
                    self.buffer.drain(self.cursor..);
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Ctrl('u') => {
                if self.cursor > 0 {
                    self.buffer.drain(..self.cursor);
                    self.cursor = 0;
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Up => {
                if self.history_index > 0 {
                    if self.history_index == self.history.len() {
                        self.draft = Some(self.buffer.clone());
                    }
                    self.history_index -= 1;
                    self.buffer = self.history[self.history_index].clone();
                    self.cursor = self.buffer.len();
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Down => {
                if self.history_index < self.history.len() {
                    self.history_index += 1;
                    if self.history_index == self.history.len() {
                        self.buffer = self.draft.take().unwrap_or_default();
                    } else {
                        self.buffer = self.history[self.history_index].clone();
                    }
                    self.cursor = self.buffer.len();
                    PromptAction::Changed
                } else {
                    PromptAction::None
                }
            }
            Key::Enter => PromptAction::Submit,
            Key::Escape | Key::Ctrl('c') => PromptAction::Cancel,
            _ => PromptAction::None,
        }
    }
}
