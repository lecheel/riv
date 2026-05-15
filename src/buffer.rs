//--+ src/buffer.rs
//! Buffer module — the core text storage layer.
//!
//! Uses `ropey::Rope` for efficient rope-based text editing.
//! Each buffer has a unique id, an optional file path, and undo history.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ropey::Rope;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tree_sitter::{Parser, Tree};
use unicode_segmentation::UnicodeSegmentation;

// ── Error ───────────────────────────────────────────────────────────

/// Errors produced by buffer operations.
#[derive(Debug, Error)]
pub enum BufferError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Rope error: {0}")]
    Rope(String),
    #[error("Buffer {0} not found")]
    NotFound(BufferId),
}

// ── IDs ─────────────────────────────────────────────────────────────

/// Opaque identifier for a buffer.
pub type BufferId = u64;

static mut NEXT_BUFFER_ID: BufferId = 1;

/// Generate a new unique buffer id.
pub fn new_buffer_id() -> BufferId {
    // SAFETY: single-threaded main thread usage for buffer id generation.
    unsafe {
        let id = NEXT_BUFFER_ID;
        NEXT_BUFFER_ID += 1;
        id
    }
}

// ── Language detection ──────────────────────────────────────────────

/// Supported programming languages for syntax highlighting / LSP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Python,
    PlainText,
    GitLog,
    GitDiff,
    Build,
    GitCommit,
}

impl Language {
    /// Detect language from a file extension.
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "rs" => Language::Rust,
            "js" | "mjs" | "jsx" => Language::JavaScript,
            "ts" | "tsx" => Language::TypeScript,
            "py" | "pyw" | "pyi" => Language::Python,
            _ => Language::PlainText,
        }
    }

    /// Return the language name as a lowercase string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Python => "python",
            Language::GitLog => "gitlog",
            Language::GitDiff => "gitdiff",
            Language::GitCommit => "gitcommit",
            Language::Build => "build",
            Language::PlainText => "plain",
        }
    }

    /// Return the tree-sitter language name for LSP integration.
    pub fn tree_sitter_name(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Python => "python",
            Language::PlainText => "plain",
            Language::GitLog => "plain",
            Language::Build => "plain",
            Language::GitDiff => "plain",
            Language::GitCommit => "plain",
        }
    }
}

// ── Cursor position ─────────────────────────────────────────────────

/// A position within a buffer, expressed as (line, column) in grapheme offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CursorPosition {
    pub line: usize,
    pub col: usize,
}

impl CursorPosition {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }

    pub fn zero() -> Self {
        Self { line: 0, col: 0 }
    }
}

impl Default for CursorPosition {
    fn default() -> Self {
        Self::zero()
    }
}

// ── Selection ───────────────────────────────────────────────────────

/// A range within a buffer (anchor and head positions).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Selection {
    pub anchor: CursorPosition,
    pub head: CursorPosition,
}

impl Selection {
    pub fn new(anchor: CursorPosition, head: CursorPosition) -> Self {
        Self { anchor, head }
    }

    /// Return the minimum and maximum positions of the selection.
    pub fn normalized(&self) -> (CursorPosition, CursorPosition) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

// ── Buffer kind ────────────────────────────────────────────────────

/// What kind of buffer this is (normal file, ripgrep results, LLM chat, etc.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BufferKind {
    /// Regular file buffer.
    Normal,
    /// Ripgrep search results buffer.
    Ripgrep,
    /// Git diff output buffer.
    GitDiff,
    /// LLM conversation buffer (not backed by a file).
    Llm,
    LlmInput,
    GitStatus,
    GitLog,
    Build,
    GitCommit,
}

impl BufferKind {
    /// Whether this buffer kind is backed by a file on disk.
    pub fn is_file_backed(&self) -> bool {
        matches!(self, BufferKind::Normal)
    }

    /// Whether this buffer kind is read-only (user shouldn't edit directly).
    pub fn is_readonly(&self) -> bool {
        matches!(
            self,
            BufferKind::Ripgrep
                | BufferKind::Build
                | BufferKind::GitDiff
                | BufferKind::GitCommit
                | BufferKind::GitLog
                | BufferKind::Llm
        )
    }

    /// Whether this buffer kind is ephemeral (not saved to disk).
    pub fn is_ephemeral(&self) -> bool {
        matches!(
            self,
            BufferKind::Ripgrep
                | BufferKind::GitDiff
                | BufferKind::GitLog
                | BufferKind::Llm
                | BufferKind::Build
        )
    }
}

// ── Git gutter ─────────────────────────────────────────────────────

use crate::git::{GitSign, HunkRange};

/// Cached git gutter info per buffer.
#[derive(Debug, Clone, Default)]
pub struct GitGutter {
    branch: Option<String>,
    /// Per-line sign map (0-based line number → sign type).
    signs: std::collections::HashMap<usize, GitSign>,
    /// Simplified hunk ranges for navigation.
    hunks: Vec<HunkRange>,
    /// Whether the gutter data has been computed at least once.
    computed: bool,
}

impl GitGutter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn set_branch(&mut self, branch: String) {
        self.branch = Some(branch);
    }

    /// Update the gutter data from computed hunk ranges (coarse signs).
    pub fn update(&mut self, hunks: Vec<HunkRange>) {
        self.signs = crate::git::GitProvider::line_signs(&hunks);
        self.hunks = hunks;
        self.computed = true;
    }

    /// Update the gutter data with accurate per-line signs computed from
    /// the full parsed diff hunks, plus the simplified hunk ranges for navigation.
    pub fn update_with_diff_signs(
        &mut self,
        hunks: Vec<HunkRange>,
        signs: std::collections::HashMap<usize, crate::git::GitSign>,
    ) {
        self.signs = signs;
        self.hunks = hunks;
        self.computed = true;
    }

    /// Clear all cached gutter data.
    pub fn clear(&mut self) {
        self.signs.clear();
        self.hunks.clear();
        self.computed = false;
    }

    /// Whether gutter data has been computed.
    pub fn is_computed(&self) -> bool {
        self.computed
    }

    /// Get the sign for a specific 0-based line.
    pub fn sign_at(&self, line: usize) -> Option<GitSign> {
        self.signs.get(&line).copied()
    }

    /// Get the cached hunk ranges (immutable).
    pub fn hunks(&self) -> &[HunkRange] {
        &self.hunks
    }
}

// ── Undo group ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UndoGroup {
    pub text_before: String,
    pub text_after: String,
    pub cursor_before: CursorPosition,
    pub cursor_after: CursorPosition,
}

// ── Buffer ──────────────────────────────────────────────────────────

/// A text buffer backed by a ropey Rope.
pub struct Buffer {
    /// Unique buffer identifier.
    pub id: BufferId,
    /// Optional file path this buffer was loaded from / is associated with.
    pub file_path: Option<PathBuf>,
    /// The underlying rope data structure.
    pub rope: Rope,
    /// Whether the buffer has unsaved changes.
    pub dirty: bool,
    /// Detected (or manually set) language.
    pub language: Option<Language>,
    /// What kind of buffer this is.
    pub kind: BufferKind,
    /// Cached git gutter information.
    pub git_gutter: GitGutter,
    /// Edit history for undo/redo (grouped snapshots).
    pub undo_stack: Vec<UndoGroup>,
    pub redo_stack: Vec<UndoGroup>,
    /// Whether we are currently recording an undo group.
    pub undo_group_open: bool,
    /// Cursor position when the current undo group started (None if no group open).
    pub undo_group_cursor_before: Option<CursorPosition>,
    /// Buffer text when the current undo group started.
    pub undo_group_text_before: Option<String>,
    /// For Ripgrep buffers: the parsed search results.
    pub ripgrep_results: Vec<crate::ripgrep::RipgrepResult>,
    /// For Ripgrep buffers: maps buffer line number → index in `ripgrep_results`.
    /// `None` means this line is a header/blank (not a jumpable result).
    pub ripgrep_line_map: Vec<Option<usize>>,
    /// For Ripgrep buffers: the search pattern used.
    pub search_pattern: Option<String>,
    /// Text content at the time of the last successful save.
    /// Used to recalculate `dirty` after undo/redo.
    pub last_saved_text: String,
    /// Last saved revision number for tracking dirty state.
    last_saved_rev: u64,
    /// Monotonic revision counter.
    revision: u64,
    /// Optional display name override (used by LLM, Ripgrep, etc.).
    /// If set, `display_name()` returns this instead of the filename.
    pub display_name_override: Option<String>,
    /// Tree-sitter parser (kept alive for incremental parsing).
    parser: Option<Parser>,
    /// Current syntax tree.
    tree: Option<Tree>,
    pub undo_group_depth: usize,
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("id", &self.id)
            .field("file_path", &self.file_path)
            .field("dirty", &self.dirty)
            .field("language", &self.language)
            .field("kind", &self.kind)
            .field("undo_stack", &self.undo_stack.len())
            .field("redo_stack", &self.redo_stack.len())
            .finish()
    }
}

impl Buffer {
    /// Create a new empty buffer.
    pub fn new() -> Self {
        Self {
            id: new_buffer_id(),
            file_path: None,
            rope: Rope::new(),
            dirty: false,
            language: None,
            kind: BufferKind::Normal,
            git_gutter: GitGutter::new(),
            ripgrep_results: Vec::new(),
            ripgrep_line_map: Vec::new(),
            search_pattern: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            undo_group_open: false,
            undo_group_cursor_before: None,
            undo_group_text_before: None,
            last_saved_text: String::new(),
            last_saved_rev: 0,
            revision: 0,
            display_name_override: None,
            parser: None,
            tree: None,
            undo_group_depth: 0,
        }
    }

    /// Create a buffer from a string.
    pub fn from_str(text: &str) -> Self {
        let mut buf = Self::new();
        buf.rope = Rope::from_str(text);
        // A buffer created from_str starts as "saved" — its content is the
        // canonical state until someone edits it.
        buf.last_saved_text = text.to_string();
        buf.init_tree_sitter();
        buf
    }

    /// Load a file into a new buffer.
    pub fn from_file(path: &Path) -> Result<Self, BufferError> {
        let content = std::fs::read_to_string(path)?;
        let language = path
            .extension()
            .and_then(|e| e.to_str())
            .map(Language::from_extension);

        let mut buf = Self::from_str(&content);
        buf.file_path = Some(path.to_path_buf());
        buf.language = language;
        buf.init_tree_sitter();
        buf.last_saved_rev = buf.revision;
        buf.dirty = false;

        Ok(buf)
    }

    /// Convenience alias for file_path (matches naming in powerline).
    pub fn filepath(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    /// Whether the buffer has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Display name for the buffer (filename, override, or "[No Name]").
    pub fn display_name(&self) -> String {
        // Priority: override > filename > fallback
        if let Some(ref name) = self.display_name_override {
            return name.clone();
        }
        self.file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[No Name]")
            .to_string()
    }

    /// Set the display name override.
    pub fn set_display_name(&mut self, name: impl Into<String>) {
        self.display_name_override = Some(name.into());
    }

    /// Clear the display name override (revert to filename-based).
    pub fn clear_display_name(&mut self) {
        self.display_name_override = None;
    }

    /// Access the underlying rope (read-only).
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Return the number of lines in the buffer.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Return the text of a specific line, or `None` if out of range.
    pub fn line_text(&self, line_idx: usize) -> Option<String> {
        if line_idx >= self.line_count() {
            None
        } else {
            Some(self.rope.line(line_idx).to_string())
        }
    }

    /// Return the entire buffer content as a String.
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Return the length of a line in graphemes (excluding the trailing newline).
    /// This matches Vim's `len()` — the newline is a line terminator, not content.
    pub fn line_len(&self, line_idx: usize) -> usize {
        if line_idx >= self.line_count() {
            return 0;
        }
        let line_str = self.rope.line(line_idx).to_string();
        let trimmed = line_str.trim_end_matches('\n');
        trimmed.graphemes(true).count()
    }

    /// Insert text at a cursor position, returning the new cursor position.
    pub fn insert_at(&mut self, pos: CursorPosition, text: &str) -> CursorPosition {
        // Convert line/col to char offset.
        let char_idx = match self.rope.try_line_to_char(pos.line) {
            Ok(start) => {
                let line_str = self.rope.line(pos.line).to_string();
                let col_chars: usize = line_str.grapheme_indices(true).take(pos.col).count();
                start + col_chars
            }
            Err(_) => self.rope.len_chars(),
        };

        self.rope.insert(char_idx, text);
        self.revision += 1;
        self.dirty = true;

        // Calculate new position.
        let new_lines = text.chars().filter(|&c| c == '\n').count();
        let last_line_graphemes = text
            .split('\n')
            .next_back()
            .map(|s| s.graphemes(true).count())
            .unwrap_or(0);

        self.reparse_tree();
        CursorPosition {
            line: pos.line + new_lines,
            col: if new_lines > 0 {
                last_line_graphemes
            } else {
                pos.col + last_line_graphemes
            },
        }
    }

    /// Delete `count` graphemes starting at a cursor position.
    /// Returns the new cursor position.
    pub fn delete_at(&mut self, pos: CursorPosition, count: usize) -> CursorPosition {
        let line = pos.line.min(self.line_count().saturating_sub(1));
        let line_str = self.rope.line(line).to_string();
        let graphemes: Vec<_> = line_str.grapheme_indices(true).collect();
        let end = (pos.col + count).min(graphemes.len());

        if pos.col < graphemes.len() {
            let start_byte = graphemes[pos.col].0;
            let end_byte = if end < graphemes.len() {
                graphemes[end].0
            } else {
                line_str.len()
            };

            let char_start = match self.rope.try_line_to_char(line) {
                Ok(c) => c,
                Err(_) => return pos,
            };

            // Convert byte offsets to char offsets.
            let chars_before = line_str[..start_byte].chars().count();
            let chars_after_end = line_str[..end_byte].chars().count();

            self.rope
                .remove(char_start + chars_before..char_start + chars_after_end);
            self.revision += 1;
            self.dirty = true;
        }

        self.reparse_tree();
        CursorPosition { line, col: pos.col }
    }

    /// Delete the line at `line_idx`.
    pub fn delete_line(&mut self, line_idx: usize) {
        if line_idx >= self.line_count() {
            return;
        }
        let start = self.rope.line_to_char(line_idx);
        let end = if line_idx + 1 < self.line_count() {
            self.rope.line_to_char(line_idx + 1)
        } else {
            self.rope.len_chars()
        };
        self.rope.remove(start..end);
        self.reparse_tree();
        self.revision += 1;
        self.dirty = true;
    }

    /// Save the buffer to its associated file path.
    pub fn save(&mut self) -> Result<(), BufferError> {
        if let Some(ref path) = self.file_path {
            std::fs::write(path, self.text())?;
            self.last_saved_text = self.text();
            self.dirty = false;
            self.last_saved_rev = self.revision;
            Ok(())
        } else {
            Err(BufferError::Io(std::io::Error::other(
                "No file path associated with buffer",
            )))
        }
    }

    /// Save to a specific path.
    pub fn save_to(&mut self, path: &Path) -> Result<(), BufferError> {
        std::fs::write(path, self.text())?;
        self.file_path = Some(path.to_path_buf());
        self.last_saved_text = self.text();
        self.dirty = false;
        self.last_saved_rev = self.revision;

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(Language::from_extension);
        if ext.is_some() {
            self.language = ext;
        }

        Ok(())
    }

    pub fn begin_undo_group(&mut self, cursor: CursorPosition) {
        if self.undo_group_depth == 0 {
            self.undo_group_open = true;
            self.undo_group_cursor_before = Some(cursor);
            self.undo_group_text_before = Some(self.text());
        }
        self.undo_group_depth += 1;
    }

    pub fn end_undo_group(&mut self, cursor: CursorPosition) {
        if self.undo_group_depth == 0 {
            return;
        }
        self.undo_group_depth -= 1;
        if self.undo_group_depth > 0 {
            return; // Inner group — don't snapshot yet
        }
        // Outermost group closing
        self.undo_group_open = false;
        if let (Some(cursor_before), Some(text_before)) = (
            self.undo_group_cursor_before.take(),
            self.undo_group_text_before.take(),
        ) {
            let text_after = self.text();
            if text_before != text_after {
                self.undo_stack.push(UndoGroup {
                    cursor_before,
                    cursor_after: cursor,
                    text_before,
                    text_after,
                });
                self.redo_stack.clear();
                if self.undo_stack.len() > 10_000 {
                    self.undo_stack.remove(0);
                }
            }
        }
    }

    pub fn cancel_undo_group(&mut self) {
        self.undo_group_depth = 0;
        self.undo_group_open = false;
        self.undo_group_cursor_before = None;
        self.undo_group_text_before = None;
    }

    pub fn in_undo_group(&self) -> bool {
        self.undo_group_depth > 0
    }

    pub fn break_undo_group(&mut self, cursor: CursorPosition) {
        self.undo_group_open = false;
        self.undo_group_cursor_before = None;
        self.undo_group_text_before = None;
        self.begin_undo_group(cursor);
    }

    /// Undo: pop the last group and return the snapshot info to restore.
    /// Returns `Some((text, cursor))` to restore, or `None` if nothing to undo.
    pub fn pop_undo(&mut self) -> Option<(String, CursorPosition)> {
        let mut group = self.undo_stack.pop()?;
        let text = std::mem::take(&mut group.text_before);
        let cursor = group.cursor_before;
        self.redo_stack.push(group);
        // Recalculate dirty: if we undid back to the last saved state, clear dirty.
        // Otherwise keep dirty.
        self.dirty = true; // conservative default
        Some((text, cursor))
    }

    /// Redo: pop the last redo group and return the snapshot info to restore.
    pub fn pop_redo(&mut self) -> Option<(String, CursorPosition)> {
        let mut group = self.redo_stack.pop()?;
        let text = std::mem::take(&mut group.text_after);
        let cursor = group.cursor_after;
        self.undo_stack.push(group);
        self.dirty = true; // conservative default
        Some((text, cursor))
    }

    /// Recalculate dirty flag by comparing current text with last saved state.
    /// Call this after undo/redo to update the status bar correctly.
    pub fn recalc_dirty(&mut self) {
        self.dirty = self.text() != self.last_saved_text;
    }

    /// Return the number of undo entries.
    pub fn undo_len(&self) -> usize {
        self.undo_stack.len()
    }

    /// Return the number of redo entries.
    pub fn redo_len(&self) -> usize {
        self.redo_stack.len()
    }

    /// Clear all undo/redo history.
    pub fn clear_undo_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.cancel_undo_group();
    }

    /// Replace the entire buffer text, tracking the change in the undo stack.
    /// Returns `true` if the text actually changed.
    pub fn replace_all(&mut self, new_text: &str, cursor: CursorPosition) -> bool {
        let old_text = self.text();
        if old_text == new_text {
            return false;
        }
        self.undo_stack.push(UndoGroup {
            text_before: old_text,
            text_after: new_text.to_string(),
            cursor_before: cursor,
            cursor_after: cursor,
        });
        self.redo_stack.clear();
        self.rope = ropey::Rope::from_str(new_text);
        self.dirty = true;
        self.revision += 1;
        self.reparse_tree(); // ← ADD THIS
        true
    }

    /// Create an LLM chat buffer (ephemeral, not file-backed).
    pub fn new_llm() -> Self {
        let mut buf = Self::new();
        buf.kind = BufferKind::Llm;
        buf.display_name_override = Some("LLM Chat".to_string());
        // LLM buffers are never "dirty" in the save-to-disk sense
        buf.dirty = false;
        buf.last_saved_text = String::new();
        buf
    }

    // ── Tree-sitter ────────────────────────────────────────────

    /// Initialize the tree-sitter parser for this buffer's language.
    pub fn init_tree_sitter(&mut self) {
        let language = match self.language {
            Some(Language::Rust) => tree_sitter_rust::LANGUAGE,
            Some(Language::JavaScript) => tree_sitter_javascript::LANGUAGE,
            Some(Language::TypeScript) => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            Some(Language::Python) => tree_sitter_python::LANGUAGE,
            _ => return, // PlainText — no parser
        };

        let mut parser = Parser::new();
        // tree-sitter 0.24+ uses set_language(&Language)
        if parser.set_language(&language.into()).is_err() {
            return;
        }

        let text = self.text();
        let tree = parser.parse(&text, None);
        self.parser = Some(parser);
        self.tree = tree;
    }

    /// Re-parse the tree after an edit. Call this after insert_at / delete_at.
    ///
    /// IMPORTANT: We always pass `None` as the old tree instead of the previous
    /// tree. This disables incremental parsing and forces a full reparse.
    ///
    /// Incremental parsing requires calling `tree.edit()` with the exact byte
    /// delta *before* mutating the rope. Since this codebase mutates the rope
    /// directly (insert_at, delete_at, delete_line, remove, etc.) without
    /// calling `tree.edit()`, the old tree's byte offsets are out of sync with
    /// the new rope. Passing the old tree to tree-sitter causes it to either:
    ///   - Reuse stale subtrees (thinking they haven't changed)
    ///   - Return a corrupt tree with wrong node boundaries
    ///   - Return a tree whose span exceeds the actual buffer length
    ///
    /// Full reparsing is slightly slower but guaranteed correct. To re-enable
    /// incremental parsing, every rope mutation site must call `tree.edit()`
    /// first with the correct InputEdit.
    /// Re-parse the tree after an edit. Call this after insert_at / delete_at.
    pub fn reparse_tree(&mut self) {
        let text = self.text();
        if let Some(parser) = self.parser.as_mut() {
            self.tree = parser.parse(&text, None);
        }
    }

    /// Access the syntax tree.
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

// ── Buffer collection ───────────────────────────────────────────────

/// A thread-safe collection of buffers, keyed by id.
pub struct BufferCollection {
    buffers: HashMap<BufferId, Buffer>,
}

impl BufferCollection {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    /// Insert a buffer, returning its id.
    pub fn insert(&mut self, buffer: Buffer) -> BufferId {
        let id = buffer.id;
        self.buffers.insert(id, buffer);
        id
    }

    /// Get an immutable reference to a buffer by id.
    pub fn get(&self, id: &BufferId) -> Option<&Buffer> {
        self.buffers.get(id)
    }

    /// Get a mutable reference to a buffer by id.
    pub fn get_mut(&mut self, id: &BufferId) -> Option<&mut Buffer> {
        self.buffers.get_mut(id)
    }

    /// Remove a buffer by id.
    pub fn remove(&mut self, id: &BufferId) -> Option<Buffer> {
        self.buffers.remove(id)
    }

    /// Return the number of buffers.
    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    /// Return true if empty.
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    /// Iterate over all buffers.
    pub fn iter(&self) -> impl Iterator<Item = &Buffer> {
        self.buffers.values()
    }

    /// Create a new empty buffer and add it to the collection.
    pub fn new_buffer(&mut self) -> BufferId {
        let buffer = Buffer::new();
        self.insert(buffer)
    }

    /// Create a new LLM chat buffer and add it to the collection.
    pub fn new_llm_buffer(&mut self) -> BufferId {
        let buffer = Buffer::new_llm();
        self.insert(buffer)
    }

    // In src/buffer.rs, inside impl BufferCollection

    /// Open a file into a new buffer, or return the existing buffer id if already open.
    /// If the file does not exist, creates a new empty buffer with that path.
    pub fn open_file(&mut self, path: &Path) -> Result<BufferId, BufferError> {
        // Canonicalize the requested path so comparisons are reliable
        let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        // Check if already open by comparing canonicalized paths.
        for buf in self.buffers.values() {
            if let Some(ref buf_path) = buf.file_path {
                let existing_abs = buf_path.canonicalize().unwrap_or_else(|_| buf_path.clone());
                if existing_abs == abs {
                    return Ok(buf.id);
                }
            }
        }

        // File is not open yet — attempt to open it.
        let mut buffer = match Buffer::from_file(path) {
            Ok(b) => b,
            Err(BufferError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                // File doesn't exist yet — create an empty buffer for it
                let mut b = Buffer::new();
                b.language = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(Language::from_extension);
                b.init_tree_sitter();
                b
            }
            Err(e) => return Err(e),
        };

        // Standardize the stored path to the absolute canonical path.
        // This ensures that future lookups match directly without needing
        // to re-canonicalize every time.
        buffer.file_path = Some(abs);

        let id = buffer.id;
        self.buffers.insert(id, buffer);
        Ok(id)
    }

    /// Find a buffer by its kind (returns the first match).
    pub fn find_by_kind(&self, kind: BufferKind) -> Option<BufferId> {
        self.buffers
            .values()
            .find(|buf| buf.kind == kind)
            .map(|buf| buf.id)
    }

    /// Find a buffer by its display name (case-insensitive partial match).
    pub fn find_by_name(&self, name: &str) -> Option<BufferId> {
        let name_lower = name.to_lowercase();
        self.buffers
            .values()
            .find(|buf| buf.display_name().to_lowercase().contains(&name_lower))
            .map(|buf| buf.id)
    }

    /// Return all buffer IDs of a specific kind.
    pub fn ids_by_kind(&self, kind: BufferKind) -> Vec<BufferId> {
        self.buffers
            .values()
            .filter(|buf| buf.kind == kind)
            .map(|buf| buf.id)
            .collect()
    }
}

impl Default for BufferCollection {
    fn default() -> Self {
        Self::new()
    }
}
