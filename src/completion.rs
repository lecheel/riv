//! Auto-completion engine.
//!
//! Provides word-based and LSP-based completion suggestions.
//! Manages the completion popup state (active, selected index, items).

use crate::lsp::CompletionItem;
use std::collections::HashSet;
use std::path::Path;

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::{Buffer, CursorPosition};

// ── Completion source ───────────────────────────────────────────────

/// Where a completion item came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSource {
    /// Suggested from words in the current buffer.
    BufferWords,
    /// Suggested from words in all open buffers.
    AllBuffers,
    /// Suggested from the LSP server.
    Lsp,
    /// Suggested from snippet definitions.
    Snippet,
    /// Suggested from file paths.
    FilePath,
}

// ── Completion item ─────────────────────────────────────────────────

/// An item in the completion list.
#[derive(Debug, Clone)]
pub struct CompletionEntry {
    /// Text to display and insert.
    pub text: String,
    /// Optional detail (e.g., type signature).
    pub detail: Option<String>,
    /// Optional documentation.
    pub documentation: Option<String>,
    /// The kind of completion.
    pub kind: CompletionKind,
    /// Where this came from.
    pub source: CompletionSource,
    /// Relevance score for sorting.
    pub score: f64,
}

/// Kind of completion for display purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Text,
    Function,
    Method,
    Variable,
    Field,
    Type,
    Module,
    Keyword,
    Snippet,
    File,
    Folder,
    Class,
    Interface,
    Property,
    Enum,
    Constant,
    Struct,
}

impl CompletionKind {
    /// Return a short label for display.
    pub fn as_str(&self) -> &'static str {
        match self {
            CompletionKind::Text => "text",
            CompletionKind::Function => "fn",
            CompletionKind::Method => "meth",
            CompletionKind::Variable => "var",
            CompletionKind::Field => "field",
            CompletionKind::Type => "type",
            CompletionKind::Module => "mod",
            CompletionKind::Keyword => "kw",
            CompletionKind::Snippet => "snip",
            CompletionKind::File => "file",
            CompletionKind::Folder => "dir",
            CompletionKind::Class => "class",
            CompletionKind::Interface => "iface",
            CompletionKind::Property => "prop",
            CompletionKind::Enum => "enum",
            CompletionKind::Constant => "const",
            CompletionKind::Struct => "struct",
        }
    }

    pub fn from_lsp_kind(lsp_kind: u32) -> Self {
        crate::lsp::lsp_kind_to_completion_kind(lsp_kind)
    }
}

// ── Completion context ──────────────────────────────────────────────

/// The context in which completion was triggered.
#[derive(Debug, Clone)]
pub struct CompletionContext {
    /// The partial word being completed.
    pub trigger: String,
    /// Cursor position where completion was triggered.
    pub position: CursorPosition,
    /// The line of text at the trigger position.
    pub line_text: String,
    /// Whether the trigger is a file path.
    pub is_path: bool,
}

// ── Completion engine ───────────────────────────────────────────────

/// The completion engine collects and filters completion suggestions.
pub struct CompletionEngine {
    /// The minimum number of characters before triggering completion.
    pub trigger_len: usize,
    /// Whether completion is currently active.
    pub active: bool,
    /// Current completion items.
    pub items: Vec<CompletionEntry>,
    /// Currently selected item index.
    pub selected_index: usize,
    /// The context when completion was triggered.
    pub context: Option<CompletionContext>,
    /// Maximum number of completions to show.
    pub max_items: usize,
}

impl CompletionEngine {
    pub fn new(trigger_len: usize) -> Self {
        Self {
            trigger_len,
            active: false,
            items: Vec::new(),
            selected_index: 0,
            context: None,
            max_items: 50,
        }
    }

    // In CompletionEngine::try_trigger()
    pub fn try_trigger(&mut self, buffer: &Buffer, position: CursorPosition) -> bool {
        let (word, is_path) = word_or_path_before_cursor(buffer, position);

        // ── After a member-access dot, the word fragment can be 0 chars.
        //    We still want to activate (LSP items will fill in).
        let after_dot = !is_path && {
            if let Some(line) = buffer.line_text(position.line) {
                let graphemes: Vec<&str> = line.graphemes(true).collect();
                let word_start = position.col.saturating_sub(word.len());
                graphemes.get(word_start.saturating_sub(1)) == Some(&".")
            } else {
                false
            }
        };

        let min_len = if is_path {
            2
        } else if after_dot {
            0 // ← allow empty trigger right after dot
        } else {
            1
        };

        if word.len() < min_len {
            self.cancel();
            return false;
        }

        let line_text = buffer.line_text(position.line).unwrap_or_default();
        let base_dir = buffer.file_path.as_deref();

        // Collect completions from all sources
        let mut items = Vec::new();

        // Always collect buffer words (if trigger is long enough)
        // ── After a dot, skip buffer words — LSP is authoritative
        if !after_dot && word.len() >= self.trigger_len {
            let word_items = collect_buffer_words(buffer, &word);
            items.extend(word_items);
        }

        // Collect file paths if it looks like a path
        if is_path {
            let path_items = collect_file_paths(&word, base_dir);
            items.extend(path_items);
        }

        // Sort by score and truncate
        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        items.truncate(self.max_items);

        if items.is_empty() {
            self.cancel();
            return false;
        }

        let context = CompletionContext {
            trigger: word.clone(),
            position,
            line_text,
            is_path,
        };

        self.items = items;
        self.selected_index = 0;
        self.context = Some(context);
        self.active = true;

        true
    }

    // In CompletionEngine::add_lsp_items()
    pub fn add_lsp_items(&mut self, lsp_items: Vec<CompletionItem>) {
        if lsp_items.is_empty() {
            return; // nothing to show
        }

        // ── Activate if we have items but completion was in "pending" state.
        //    This handles the case where the trigger char set active=true
        //    but then cancel() was called by a later update, or where
        //    completion was never activated because try_trigger failed
        //    on a short prefix but LSP still has results.
        if !self.active {
            self.active = true;
            if self.context.is_none() {
                // Fabricate a minimal context so confirm() works
                self.context = Some(CompletionContext {
                    trigger: String::new(),
                    position: CursorPosition::zero(),
                    line_text: String::new(),
                    is_path: false,
                });
            }
        }

        let trigger = self
            .context
            .as_ref()
            .map(|c| c.trigger.to_lowercase())
            .unwrap_or_default();

        for item in lsp_items {
            let text = item.insert_text.unwrap_or(item.label.clone());
            let score = compute_score(&text, &trigger);

            let final_score = if trigger.is_empty() {
                50.0
            } else {
                score + 10.0
            };

            let kind = item
                .kind
                .map(CompletionKind::from_lsp_kind)
                .unwrap_or(CompletionKind::Text);

            self.items.push(CompletionEntry {
                text,
                detail: item.detail,
                documentation: item.documentation.and_then(|v| {
                    v.as_str()
                        .map(String::from)
                        .or_else(|| v.get("value")?.as_str().map(String::from))
                }),
                kind,
                source: CompletionSource::Lsp,
                score: final_score,
            });
        }

        self.items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.items.truncate(self.max_items);
    }

    // In CompletionEngine::update()
    pub fn update(&mut self, new_trigger: &str) {
        if !self.active {
            return;
        }

        if new_trigger.len() < self.trigger_len {
            self.cancel();
            return;
        }

        // Re-score and filter existing items.
        let trigger_lower = new_trigger.to_lowercase();
        let _before_count = self.items.len();
        self.items
            .retain(|item| item.text.to_lowercase().contains(&trigger_lower));
        let _after_retain = self.items.len();

        for item in &mut self.items {
            item.score = compute_score(&item.text, &trigger_lower);
        }

        self.items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.selected_index = 0;

        if let Some(ctx) = self.context.as_mut() {
            ctx.trigger = new_trigger.to_string();
        }

        if self.items.is_empty() {
            self.cancel(); // ← add this: cancel when no items remain after filtering
        }
    }

    /// Try to trigger completion at the given position.
    /// Returns true if completion was activated.
    /// Handles both buffer words and file paths uniformly.
    /// Select the next completion item.
    pub fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.items.len();
        }
    }

    /// Select the previous completion item.
    pub fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = if self.selected_index == 0 {
                self.items.len() - 1
            } else {
                self.selected_index - 1
            };
        }
    }

    /// Confirm the selected completion and return the text to insert.
    /// Also returns the number of characters of the trigger to remove.
    pub fn confirm(&mut self) -> Option<(String, usize)> {
        if !self.active || self.items.is_empty() {
            return None;
        }

        let item = &self.items[self.selected_index];
        let trigger_len = self.context.as_ref()?.trigger.len();
        let text = item.text.clone();

        self.cancel();
        Some((text, trigger_len))
    }

    /// Cancel / close the completion popup.
    pub fn cancel(&mut self) {
        self.active = false;
        self.items.clear();
        self.selected_index = 0;
        self.context = None;
    }

    /// Get the currently selected item.
    pub fn selected_item(&self) -> Option<&CompletionEntry> {
        self.items.get(self.selected_index)
    }

    /// Update items for path mode — re-collects both paths and buffer words.
    pub fn update_path(&mut self, buffer: &Buffer, new_trigger: &str) {
        if !self.active {
            return;
        }

        // Paths need at least 2 chars (e.g., "./")
        if new_trigger.len() < 2 {
            self.cancel();
            return;
        }

        let base_dir = buffer.file_path.as_deref();
        let mut items = Vec::new();

        // Collect file paths
        let path_items = collect_file_paths(new_trigger, base_dir);
        items.extend(path_items);

        // Also collect buffer words if trigger is long enough
        if new_trigger.len() >= self.trigger_len {
            let word_items = collect_buffer_words(buffer, new_trigger);
            items.extend(word_items);
        }

        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        self.items = items;
        self.selected_index = 0;

        if let Some(ctx) = self.context.as_mut() {
            ctx.trigger = new_trigger.to_string();
        }

        if self.items.is_empty() {
            self.cancel();
        }
    }
}

// ── Helper functions ────────────────────────────────────────────────

/// Check if a grapheme is an identifier character.
fn is_identifier_char(g: &str) -> bool {
    g.chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_' || c == '-')
        .unwrap_or(false)
}

/// Check if a character is valid in a file path.
fn is_path_char(g: &str) -> bool {
    g.chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
        .unwrap_or(false)
}

/// Check if a string looks like a file path trigger.
pub fn is_path_trigger(s: &str) -> bool {
    s.contains('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with('/')
}

/// Extract the word or file path being typed before the cursor.
/// Returns (text, is_path).
pub fn word_or_path_before_cursor(buffer: &Buffer, position: CursorPosition) -> (String, bool) {
    if let Some(line_text) = buffer.line_text(position.line) {
        let graphemes: Vec<_> = line_text.graphemes(true).collect();
        let end = position.col.min(graphemes.len());

        let mut start = end;
        while start > 0 {
            let g = graphemes[start - 1];
            if is_identifier_char(g) || is_path_char(g) {
                start -= 1;
            } else {
                break;
            }
        }

        let text = graphemes[start..end].join("");
        let is_path = is_path_trigger(&text);

        // ── NEW: For code (non-path), '.' is a member accessor.
        // `map.insert` → complete on `insert`, not `map.insert`.
        if !is_path {
            if let Some(dot_pos) = text.rfind('.') {
                let after_dot = &text[dot_pos + 1..];
                return (after_dot.to_string(), false);
            }
        }

        (text, is_path)
    } else {
        (String::new(), false)
    }
}

/// Extract the word currently being typed before the cursor (identifier only, no paths).
pub fn word_before_cursor(buffer: &Buffer, position: CursorPosition) -> String {
    if let Some(line_text) = buffer.line_text(position.line) {
        let graphemes: Vec<_> = line_text.graphemes(true).collect();
        let end = position.col.min(graphemes.len());

        let mut start = end;
        while start > 0 {
            let g = graphemes[start - 1];
            if is_identifier_char(g) {
                start -= 1;
            } else {
                break;
            }
        }

        graphemes[start..end].join("")
    } else {
        String::new()
    }
}

/// Collect file/directory paths matching the trigger.
/// The trigger should look like a path (contain '/' or start with './', '../', etc.).
pub fn collect_file_paths(trigger: &str, base_dir: Option<&Path>) -> Vec<CompletionEntry> {
    if !is_path_trigger(trigger) {
        return Vec::new();
    }

    // Determine the base directory for resolving relative paths
    let base = if trigger.starts_with('/') {
        Path::new("/")
    } else {
        base_dir.and_then(|p| p.parent()).unwrap_or(Path::new("."))
    };

    // Parse the trigger into directory part and filename prefix
    let trigger_path = Path::new(trigger);
    let parent = trigger_path.parent();
    let prefix = trigger_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let (full_dir, parent_str) = match parent {
        Some(p) if !p.as_os_str().is_empty() => (base.join(p), p.to_string_lossy().to_string()),
        _ => (base.to_path_buf(), String::new()),
    };

    let prefix_lower = prefix.to_lowercase();
    let mut items = Vec::new();

    // List directory entries
    let entries = match std::fs::read_dir(&full_dir) {
        Ok(e) => e,
        Err(_) => return items,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        // Skip hidden files unless trigger explicitly starts with '.'
        if name_str.starts_with('.') && !trigger.starts_with('.') {
            continue;
        }

        // Filter by prefix (if any)
        if !prefix_lower.is_empty() && !name_str.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let is_dir = metadata.is_dir();
        let kind = if is_dir {
            CompletionKind::Folder
        } else {
            CompletionKind::File
        };

        // Build display name (with trailing slash for directories)
        let display_name = if is_dir {
            format!("{}/", name_str)
        } else {
            name_str.clone()
        };

        // Build the full text to insert
        let insert_text = if parent_str.is_empty() {
            display_name
        } else {
            format!("{}/{}", parent_str, display_name)
        };

        // Score: prefix match bonus, file paths get a slight boost
        let score = compute_score(&name_str, &prefix_lower) + 15.0;

        // Build detail string
        let detail = if is_dir {
            Some("dir".to_string())
        } else {
            let len = metadata.len();
            if len < 1024 {
                Some(format!("{} B", len))
            } else if len < 1024 * 1024 {
                Some(format!("{}.1 KB", len as f64 / 1024.0))
            } else {
                Some(format!("{}.1 MB", len as f64 / (1024.0 * 1024.0)))
            }
        };

        items.push(CompletionEntry {
            text: insert_text,
            detail,
            documentation: None,
            kind,
            source: CompletionSource::FilePath,
            score,
        });
    }

    // Sort directories first, then alphabetically
    items.sort_by(|a, b| {
        let a_dir = a.kind == CompletionKind::Folder;
        let b_dir = b.kind == CompletionKind::Folder;
        b_dir.cmp(&a_dir).then_with(|| a.text.cmp(&b.text))
    });

    items
}

/// Collect words from the buffer that start with the given prefix.
fn collect_buffer_words(buffer: &Buffer, prefix: &str) -> Vec<CompletionEntry> {
    let mut words: HashSet<String> = HashSet::new();
    let prefix_lower = prefix.to_lowercase();

    for line_idx in 0..buffer.line_count() {
        if let Some(line_text) = buffer.line_text(line_idx) {
            // Simple word extraction: split on non-identifier characters.
            let mut current_word = String::new();
            for g in line_text.graphemes(true) {
                if is_identifier_char(g) {
                    current_word.push_str(g);
                } else {
                    if !current_word.is_empty()
                        && current_word.len() > prefix.len()
                        && current_word.to_lowercase().starts_with(&prefix_lower)
                    {
                        words.insert(current_word.clone());
                    }
                    current_word.clear();
                }
            }
            // Don't forget the last word.
            if !current_word.is_empty()
                && current_word.len() > prefix.len()
                && current_word.to_lowercase().starts_with(&prefix_lower)
            {
                words.insert(current_word);
            }
        }
    }

    words
        .into_iter()
        .map(|text| {
            let score = compute_score(&text, &prefix_lower);
            CompletionEntry {
                text,
                detail: None,
                documentation: None,
                kind: CompletionKind::Text,
                source: CompletionSource::BufferWords,
                score,
            }
        })
        .collect()
}

/// Compute a relevance score for a completion item.
/// Higher is better. Simple prefix-matching heuristic.
pub fn compute_score(text: &str, trigger: &str) -> f64 {
    if trigger.is_empty() {
        return 0.0;
    }

    let text_lower = text.to_lowercase();
    let trigger_lower = trigger.to_lowercase();

    if !text_lower.starts_with(&trigger_lower) {
        // Contains but doesn't start with — lower score.
        if text_lower.contains(&trigger_lower) {
            return 5.0;
        }
        return 0.0;
    }

    // Exact match bonus.
    if text_lower == trigger_lower {
        return 100.0;
    }

    // Score based on how much of the trigger is covered relative to text length.
    let coverage = trigger.len() as f64 / text.len().max(1) as f64;
    coverage * 50.0
}
