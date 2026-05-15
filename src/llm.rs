//! LLM buffer state management.
//!
//! Manages conversation history, input state, presets, and streaming
//! for the LLM interaction buffer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;

/// Role of a message in the conversation
#[derive(Debug, Clone, PartialEq)]
pub enum LlmRole {
    User,
    Assistant,
    System,
    Error,
}

impl LlmRole {
    pub fn prefix(&self) -> &'static str {
        match self {
            LlmRole::User => "You",
            LlmRole::Assistant => "AI",
            LlmRole::System => "System",
            LlmRole::Error => "Error",
        }
    }

    pub fn as_api_role(&self) -> &'static str {
        match self {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "system",
            LlmRole::Error => "system",
        }
    }
}

/// A single message in the LLM conversation
#[derive(Debug, Clone)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: String,
    pub timestamp: Instant,
}

impl LlmMessage {
    pub fn new(role: LlmRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            timestamp: Instant::now(),
        }
    }

    /// Format message for display with word wrapping
    pub fn format_lines(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();

        // Header with role
        let header = match self.role {
            LlmRole::User => "▸ You:",
            LlmRole::Assistant => "◇ AI:",
            LlmRole::System => "⚙ System:",
            LlmRole::Error => "✗ Error:",
        };
        lines.push(format!("  {}", header));

        // Separator
        let sep_len = width.saturating_sub(2);
        lines.push(format!("  {}", "─".repeat(sep_len)));

        // Content with wrapping
        let content_width = width.saturating_sub(4);
        for paragraph in self.content.split('\n') {
            if paragraph.trim().is_empty() {
                lines.push("  │".to_string());
                continue;
            }

            // Simple word-wrap
            let mut current_line = String::new();
            for word in paragraph.split_whitespace() {
                if current_line.is_empty() {
                    current_line = word.to_string();
                } else if current_line.len() + word.len() < content_width {
                    current_line.push(' ');
                    current_line.push_str(word);
                } else {
                    lines.push(format!("  │ {}", current_line));
                    current_line = word.to_string();
                }
            }
            if !current_line.is_empty() {
                lines.push(format!("  │ {}", current_line));
            }
        }

        lines
    }
}

/// State of the LLM interaction
#[derive(Debug, Clone, PartialEq)]
pub enum LlmState {
    /// Ready for user input
    Idle,
    /// Currently sending a request to the LLM
    Sending,
    /// Receiving a streamed response
    Streaming,
    /// Request completed successfully
    Done,
    /// An error occurred
    Error(String),
}

impl LlmState {
    pub fn is_active(&self) -> bool {
        matches!(self, LlmState::Sending | LlmState::Streaming)
    }

    pub fn status_indicator(&self) -> &'static str {
        match self {
            LlmState::Idle => "",
            LlmState::Sending => "⏳ Sending",
            LlmState::Streaming => "● Streaming",
            LlmState::Done => "✓",
            LlmState::Error(_) => "✗ Error",
        }
    }
}

/// Preset prompt templates
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LlmPreset {
    /// Free-form chat
    Chat,
    /// Check and correct English grammar
    CheckEnglish,
    /// Translate to Simplified Chinese
    TranslateToChinese,
    /// Translate to English
    TranslateToEnglish,
    /// Explain selected code or text
    Explain,
    /// Summarize selected text
    Summarize,
    /// Custom prompt (user provides full prompt)
    Custom,
}

impl LlmPreset {
    /// Get the system prompt for this preset
    pub fn system_prompt(&self) -> &'static str {
        match self {
            LlmPreset::Chat => {
                "You are a helpful assistant. Be concise and clear in your responses."
            }
            LlmPreset::CheckEnglish => {
                "You are a grammar checker. Return ONLY the corrected text. \
                 Do not add explanations, prefixes, or quotes."
            }
            LlmPreset::TranslateToChinese => {
                "Translate the following text to Simplified Chinese. \
                 Return ONLY the translation without quotes or explanations."
            }
            LlmPreset::TranslateToEnglish => {
                "Translate the following text to English. \
                 Return ONLY the translation without quotes or explanations."
            }
            LlmPreset::Explain => {
                "Explain the following code or text clearly. Use simple language \
                 and provide examples where helpful."
            }
            LlmPreset::Summarize => {
                "Provide a concise summary of the following text. \
                 Keep it brief but capture the key points."
            }
            LlmPreset::Custom => "",
        }
    }

    /// Whether this preset wraps user input in additional context
    pub fn wraps_input(&self) -> bool {
        !matches!(self, LlmPreset::Chat | LlmPreset::Custom)
    }

    /// Get the input prompt placeholder
    pub fn placeholder(&self) -> &'static str {
        match self {
            LlmPreset::Chat => "Ask anything...",
            LlmPreset::CheckEnglish => "Text to check...",
            LlmPreset::TranslateToChinese => "Text to translate...",
            LlmPreset::TranslateToEnglish => "Text to translate...",
            LlmPreset::Explain => "Code or text to explain...",
            LlmPreset::Summarize => "Text to summarize...",
            LlmPreset::Custom => "Enter prompt...",
        }
    }

    /// Get all presets for cycling
    pub fn all() -> &'static [LlmPreset] {
        &[
            LlmPreset::Chat,
            LlmPreset::CheckEnglish,
            LlmPreset::TranslateToChinese,
            LlmPreset::TranslateToEnglish,
            LlmPreset::Explain,
            LlmPreset::Summarize,
        ]
    }
}

impl std::fmt::Display for LlmPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmPreset::Chat => write!(f, "Chat"),
            LlmPreset::CheckEnglish => write!(f, "Check English"),
            LlmPreset::TranslateToChinese => write!(f, "Translate → 中文"),
            LlmPreset::TranslateToEnglish => write!(f, "Translate → English"),
            LlmPreset::Explain => write!(f, "Explain"),
            LlmPreset::Summarize => write!(f, "Summarize"),
            LlmPreset::Custom => write!(f, "Custom"),
        }
    }
}

/// The LLM buffer managing conversation state and input
#[derive(Debug)]
pub struct LlmBuffer {
    /// Conversation history
    messages: Vec<LlmMessage>,
    /// Current interaction state
    state: LlmState,
    /// The input line being typed
    input_line: String,
    /// Input cursor position (byte offset, guaranteed at char boundary)
    input_cursor: usize,
    /// Current preset/template
    preset: LlmPreset,
    /// Whether to auto-scroll to bottom on new content
    auto_scroll: bool,
    /// Cancel flag for async operations
    cancel_flag: Arc<AtomicBool>,
    /// Current streaming response (partial, not yet committed)
    streaming_content: String,
    /// Input history for up/down navigation
    input_history: Vec<String>,
    /// Current position in input history
    history_index: usize,
    /// Optional context from selection (for presets)
    selection_context: Option<String>,
    session_name: String,
}

impl Default for LlmBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmBuffer {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            state: LlmState::Idle,
            input_line: String::new(),
            input_cursor: 0,
            preset: LlmPreset::Chat,
            auto_scroll: true,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            streaming_content: String::new(),
            input_history: Vec::new(),
            history_index: 0,
            session_name: "default".to_string(),
            selection_context: None,
        }
    }

    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    pub fn set_session_name(&mut self, name: impl Into<String>) {
        self.session_name = name.into();
    }

    // ── Getters ──────────────────────────────────────────

    pub fn input(&self) -> &str {
        &self.input_line
    }

    pub fn input_cursor(&self) -> usize {
        self.input_cursor
    }

    pub fn state(&self) -> &LlmState {
        &self.state
    }

    pub fn messages(&self) -> &[LlmMessage] {
        &self.messages
    }

    pub fn streaming_content(&self) -> &str {
        &self.streaming_content
    }

    pub fn preset(&self) -> LlmPreset {
        self.preset
    }

    pub fn selection_context(&self) -> Option<&str> {
        self.selection_context.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.streaming_content.is_empty()
    }

    // ── Input handling ───────────────────────────────────

    pub fn input_char(&mut self, c: char) {
        if self.state.is_active() {
            return;
        }
        self.input_line.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
    }

    pub fn input_backspace(&mut self) {
        if self.state.is_active() {
            return;
        }
        if self.input_cursor > 0 {
            let prev = self.input_line[..self.input_cursor]
                .grapheme_indices(true)
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input_line.drain(prev..self.input_cursor);
            self.input_cursor = prev;
        }
    }

    pub fn input_delete(&mut self) {
        if self.state.is_active() {
            return;
        }
        if self.input_cursor < self.input_line.len() {
            let next_end = self.input_line[self.input_cursor..]
                .grapheme_indices(true)
                .nth(1)
                .map(|(i, _)| self.input_cursor + i)
                .unwrap_or(self.input_line.len());
            self.input_line.drain(self.input_cursor..next_end);
        }
    }

    pub fn cursor_left(&mut self) {
        self.input_cursor = self.input_line[..self.input_cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(self.input_cursor);
    }

    pub fn cursor_right(&mut self) {
        if self.input_cursor < self.input_line.len() {
            self.input_cursor = self.input_line[self.input_cursor..]
                .grapheme_indices(true)
                .nth(1)
                .map(|(i, _)| self.input_cursor + i)
                .unwrap_or(self.input_line.len());
        }
    }

    pub fn cursor_home(&mut self) {
        self.input_cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.input_cursor = self.input_line.len();
    }

    pub fn history_up(&mut self) {
        if self.history_index > 0 {
            self.history_index -= 1;
            if let Some(entry) = self.input_history.get(self.history_index) {
                self.input_line = entry.clone();
                self.input_cursor = self.input_line.len();
            }
        }
    }

    pub fn history_down(&mut self) {
        if self.history_index < self.input_history.len().saturating_sub(1) {
            self.history_index += 1;
            if let Some(entry) = self.input_history.get(self.history_index) {
                self.input_line = entry.clone();
                self.input_cursor = self.input_line.len();
            }
        } else {
            self.history_index = self.input_history.len();
            self.input_line.clear();
            self.input_cursor = 0;
        }
    }

    /// Take the input line (for sending), clears it and adds to history
    pub fn take_input(&mut self) -> String {
        let input = std::mem::take(&mut self.input_line);
        self.input_cursor = 0;
        if !input.trim().is_empty() {
            self.input_history.push(input.clone());
            self.history_index = self.input_history.len();
        }
        input
    }

    pub fn clear_input(&mut self) {
        self.input_line.clear();
        self.input_cursor = 0;
    }

    pub fn set_input(&mut self, s: impl Into<String>) {
        self.input_line = s.into();
        self.input_cursor = self.input_line.len();
    }

    // ── State management ─────────────────────────────────

    pub fn set_preset(&mut self, preset: LlmPreset) {
        self.preset = preset;
    }

    pub fn set_selection_context(&mut self, context: Option<String>) {
        self.selection_context = context;
    }

    pub fn add_message(&mut self, role: LlmRole, content: impl Into<String>) {
        self.messages.push(LlmMessage::new(role, content));
    }

    pub fn start_sending(&mut self) {
        self.state = LlmState::Sending;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.streaming_content.clear();
    }

    pub fn start_streaming(&mut self) {
        self.state = LlmState::Streaming;
        self.streaming_content.clear();
    }

    pub fn append_streaming(&mut self, chunk: &str) {
        self.streaming_content.push_str(chunk);
    }

    pub fn finish_streaming(&mut self) {
        let content = std::mem::take(&mut self.streaming_content);
        if !content.is_empty() {
            self.messages
                .push(LlmMessage::new(LlmRole::Assistant, content));
        }
        self.state = LlmState::Done;
    }

    pub fn set_infobar_message(&mut self, error: impl Into<String>) {
        let err = error.into();
        self.messages
            .push(LlmMessage::new(LlmRole::Error, err.clone()));
        self.state = LlmState::Error(err);
        self.streaming_content.clear();
    }

    pub fn set_idle(&mut self) {
        self.state = LlmState::Idle;
        self.streaming_content.clear();
    }

    pub fn cancel(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        if !self.streaming_content.is_empty() {
            self.messages
                .push(LlmMessage::new(LlmRole::Assistant, "[cancelled]"));
        }
        self.streaming_content.clear();
        self.state = LlmState::Idle;
    }

    pub fn clear_history(&mut self) {
        self.messages.clear();
        self.streaming_content.clear();
        self.state = LlmState::Idle;
        self.input_history.clear();
        self.history_index = 0;
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel_flag.clone()
    }

    /// Build the messages array for the LLM API call from conversation history.
    ///
    /// **Important**: The caller must have already added the user message via
    /// `add_message()` before calling this method. This function only reads
    /// from `self.messages` — it does NOT append anything.
    pub fn build_api_messages(&self) -> Vec<(String, String)> {
        let mut msgs = Vec::new();

        // System prompt from preset
        let system = self.preset.system_prompt();
        if !system.is_empty() {
            msgs.push(("system".to_string(), system.to_string()));
        }

        // Add recent conversation history (keep last N messages for context)
        let history_limit = 20;
        let start = self.messages.len().saturating_sub(history_limit);
        for msg in &self.messages[start..] {
            msgs.push((msg.role.as_api_role().to_string(), msg.content.clone()));
        }

        msgs
    }

    /// Calculate the rendered height for the conversation area
    pub fn content_height(&self, width: usize) -> usize {
        let mut height = 0;
        for msg in &self.messages {
            height += msg.format_lines(width).len();
        }
        // Add streaming content if present
        if !self.streaming_content.is_empty() {
            let stream_msg = LlmMessage::new(LlmRole::Assistant, &self.streaming_content);
            height += stream_msg.format_lines(width).len();
        }
        height
    }
}
