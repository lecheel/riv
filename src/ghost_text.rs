// src/ghost_text.rs
//! Inline ghost text suggestions (Codeium, LSP inline hints).
//!
//! Ghost text appears directly in the editor line, dimmed/faded,
//! and can be accepted with Tab or dismissed by typing.
use std::time::Instant;

/// Source of a ghost text suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhostTextSource {
    /// Codeium AI completion.
    Codeium,
    /// LSP inline hint (future).
    LspInlineHint,
    Completion,
}

impl GhostTextSource {
    pub fn label(&self) -> &'static str {
        match self {
            GhostTextSource::Codeium => "Codeium",
            GhostTextSource::LspInlineHint => "LSP",
            GhostTextSource::Completion => "Comp",
        }
    }
}

/// State for an inline ghost text suggestion.
#[derive(Debug, Clone)]
pub struct GhostText {
    /// The suggested text to display (dimmed).
    pub text: String,
    /// The line where ghost text starts.
    pub line: usize,
    /// The column where ghost text starts (same as cursor when triggered).
    pub start_col: usize,
    /// Source of this suggestion.
    pub source: GhostTextSource,
    /// When this suggestion was received.
    pub received_at: Instant,
    pub pinned_generation: u64,
}

impl GhostText {
    /// Create a new ghost text suggestion.
    pub fn new(text: String, line: usize, start_col: usize, source: GhostTextSource) -> Self {
        Self {
            text,
            line,
            start_col,
            source,
            received_at: Instant::now(),
            pinned_generation: 0,
        }
    }

    /// Check if this ghost text is still valid at the given cursor position.
    pub fn is_valid_at(&self, line: usize, col: usize) -> bool {
        self.line == line && col >= self.start_col
    }
    /// How many characters of the suggestion have already been typed.
    pub fn already_typed(&self, current_col: usize) -> usize {
        current_col.saturating_sub(self.start_col)
    }

    /// The remaining suggestion text that has not yet been typed.
    pub fn remaining_text(&self, current_col: usize) -> &str {
        let skip = self.already_typed(current_col);
        if skip >= self.text.len() {
            ""
        } else {
            &self.text[skip..]
        }
    }
}

/// Manager for ghost text state.
#[derive(Debug, Clone)]
pub struct GhostTextManager {
    /// Current ghost text, if any.
    pub current: Option<GhostText>,
    /// Whether a request is in flight.
    pub pending: bool,
    /// Whether to show ghost text (user can toggle).
    pub enabled: bool,
    /// Last request time for debouncing.
    pub last_request_time: Option<Instant>,
    /// Debounce interval in milliseconds.
    pub debounce_ms: u64,
    /// When the last in-flight request was sent (for stale-request detection).
    pub last_sent_at: Option<Instant>,
    /// Timeout after which a pending request is considered stale.
    pub pending_timeout_ms: u64,
    pub generation: u64,
}

impl Default for GhostTextManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GhostTextManager {
    pub fn new() -> Self {
        Self {
            current: None,
            pending: false,
            enabled: true,
            last_request_time: None,
            debounce_ms: 150,
            last_sent_at: None,
            pending_timeout_ms: 5_000,
            generation: 0,
        }
    }

    pub fn set(&mut self, mut ghost: GhostText) {
        self.generation = self.generation.wrapping_add(1); // ← ADD (before storing)
        ghost.pinned_generation = self.generation; // ← ADD
        self.pending = false;
        self.last_sent_at = None;
        self.current = Some(ghost);
    }

    pub fn clear(&mut self) {
        if self.current.is_some() {
            // ← FIX: was `self.current.is_none()`
            self.generation = self.generation.wrapping_add(1); // ← ADD
        }
        self.current = None;
        self.pending = false;
        self.last_sent_at = None;
    }

    /// Check if we should debounce (return true to skip request).
    pub fn should_debounce(&self) -> bool {
        if let Some(last) = self.last_request_time {
            last.elapsed().as_millis() < self.debounce_ms as u128
        } else {
            false
        }
    }

    /// Whether a pending request is stale and should be superseded.
    pub fn is_request_stale(&self) -> bool {
        self.last_sent_at
            .map(|t| {
                let elapsed = t.elapsed().as_millis();

                elapsed >= self.pending_timeout_ms as u128
            })
            .unwrap_or(true)
    }

    /// Mark that a request was sent.
    pub fn mark_requested(&mut self) {
        self.pending = true;
        let now = Instant::now();
        self.last_request_time = Some(now);
        self.last_sent_at = Some(now);
    }

    /// Whether ghost text is currently visible.
    pub fn is_visible(&self) -> bool {
        self.enabled && self.current.is_some()
    }

    /// Whether we're waiting for a result.
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// Toggle ghost text on/off.
    pub fn toggle(&mut self) -> bool {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.clear();
        }
        self.enabled
    }
}
