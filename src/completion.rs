//! Auto-completion engine.
//!
//! Provides word-based and LSP-based completion suggestions.
//! Manages the completion popup state (active, selected index, items).

use crate::editor::Editor;
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
    Vocab,
}

// In completion.rs
#[derive(Debug, Clone)]
pub struct CursorContext {
    pub word_prefix: String,
    pub start_col: usize,
    pub trigger_char: Option<char>,
    pub post_trigger_prefix: String,
    pub is_after_trigger: bool,
}

impl CursorContext {
    /// Return the prefix to use for filtering completions.
    /// After a trigger char (`.`, `:`), uses the post-trigger prefix;
    /// otherwise uses the full word prefix.
    pub fn filter_prefix(&self) -> &str {
        if self.is_after_trigger {
            &self.post_trigger_prefix
        } else {
            &self.word_prefix
        }
    }
}

impl Default for CursorContext {
    fn default() -> Self {
        CursorContext {
            word_prefix: String::new(),
            start_col: 0,
            trigger_char: None,
            post_trigger_prefix: String::new(),
            is_after_trigger: false,
        }
    }
}

// ── Completion item ─────────────────────────────────────────────────

/// An item in the completion list.
#[derive(Debug, Clone)]
pub struct CompletionEntry {
    /// Text to insert into the buffer.
    pub text: String,
    /// Text to display in the popup (e.g. LSP label might differ from insert_text).
    pub label: String,
    /// Right-aligned detail (type signature + source tag).
    pub detail: Option<String>,
    /// Optional documentation.
    pub documentation: Option<String>,
    /// The kind of completion (for left badge).
    pub kind: CompletionKind,
    /// Where this came from.
    pub source: CompletionSource,
    /// Relevance score for sorting.
    pub score: f64,
    /// Original LSP CompletionItem (kept for resolve requests).
    pub lsp_item: Option<crate::lsp::CompletionItem>,
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
            CompletionKind::Text => "",
            CompletionKind::Function => "fn",
            CompletionKind::Method => "fn",
            CompletionKind::Variable => "var",
            CompletionKind::Field => "fld",
            CompletionKind::Type => "typ",
            CompletionKind::Module => "mod",
            CompletionKind::Keyword => "kw",
            CompletionKind::Snippet => "snip",
            CompletionKind::File => "file",
            CompletionKind::Folder => "dir",
            CompletionKind::Class => "cls",
            CompletionKind::Interface => "if",
            CompletionKind::Property => "prp",
            CompletionKind::Enum => "enum",
            CompletionKind::Constant => "con",
            CompletionKind::Struct => "st",
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
    /// True when completion was activated by a trigger character ('.' or ':').
    /// Prevents `update()` from cancelling on short prefixes.    
    pub after_trigger_char: bool,
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
    pub base_items: Vec<CompletionEntry>,
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
            base_items: Vec::new(),
            selected_index: 0,
            context: None,
            max_items: 50,
        }
    }

    // In CompletionEngine::try_trigger()
    pub fn try_trigger(
        &mut self,
        buffer: &Buffer,
        position: CursorPosition,
        vocab: &crate::vocab::VocabManager,
    ) -> bool {
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

        if !after_dot && word.len() >= self.trigger_len {
            // vocab passed in via closure or stored ref — see step 3
            let vocab_items = collect_vocab_words(vocab, &word);
            items.extend(vocab_items);
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
            // ── NEW: After a dot, activate in pending state so LSP items
            //    have somewhere to land when they arrive asynchronously.
            if after_dot {
                let context = CompletionContext {
                    trigger: word.clone(),
                    position,
                    line_text,
                    is_path,
                    after_trigger_char: true,
                };
                self.base_items.clear();
                self.items.clear();
                self.selected_index = 0;
                self.context = Some(context);
                self.active = true;
                return true;
            }
            self.cancel();
            return false;
        }

        let context = CompletionContext {
            trigger: word.clone(),
            position,
            line_text,
            is_path,
            after_trigger_char: after_dot,
        };

        self.base_items = items.clone();
        self.items = items;
        self.selected_index = 0;
        self.context = Some(context);
        self.active = true;

        true
    }
    // completion.rs — inside impl CompletionEngine
    fn apply_filter(&mut self) {
        let trigger_lower = self
            .context
            .as_ref()
            .map(|c| c.trigger.to_lowercase())
            .unwrap_or_default();

        self.items = self
            .base_items
            .iter()
            .filter(|item| {
                trigger_lower.is_empty() || fuzzy_match(&item.text.to_lowercase(), &trigger_lower)
            })
            .map(|item| {
                let mut entry = item.clone();
                entry.score = compute_score(&item.text, &trigger_lower);
                // LSP items get a boost so they rank above buffer words
                if item.source == CompletionSource::Lsp {
                    entry.score += 10.0;
                }
                entry
            })
            .collect();

        self.items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.items.truncate(self.max_items);
        // self.selected_index = 0;
    }
    pub fn update(&mut self, new_trigger: &str) {
        if !self.active {
            return;
        }

        let after_trigger_char = self
            .context
            .as_ref()
            .map(|ctx| ctx.after_trigger_char)
            .unwrap_or(false);

        if new_trigger.len() < self.trigger_len && !after_trigger_char {
            self.cancel();
            return;
        }

        if let Some(ctx) = self.context.as_mut() {
            ctx.trigger = new_trigger.to_string();
            if new_trigger.len() >= self.trigger_len {
                ctx.after_trigger_char = false;
            }
        }

        self.apply_filter(); // ← replaces the retain + re-score block

        if self.items.is_empty() && !after_trigger_char {
            self.cancel();
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
        self.base_items.clear();
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
    /// Unified completion update: merges local + LSP candidates,
    /// filters, scores, and updates the completion state.
    pub fn update_unified_completions(
        editor: &mut Editor,
        lsp_items: Option<Vec<crate::lsp::CompletionItem>>,
    ) {
        let ctx = extract_cursor_context(editor);
        let prefix = ctx.filter_prefix().to_string();
        let prefix_lower = prefix.to_lowercase();

        // Collect local completions (buffer words, vocab, file paths)
        let mut all_entries = collect_local_completions(editor, &ctx);

        // Convert and add LSP items
        if let Some(lsp) = lsp_items {
            for item in lsp {
                let raw_label = item.label.clone();
                let label = raw_label
                    .split("(use ")
                    .next()
                    .unwrap_or(&raw_label)
                    .trim()
                    .to_string();
                let text = item
                    .insert_text
                    .as_deref()
                    .unwrap_or(&raw_label)
                    .split("(use ")
                    .next()
                    .unwrap_or(&raw_label)
                    .trim()
                    .to_string();

                let doc = item.documentation.as_ref().and_then(|v| {
                    v.as_str()
                        .map(String::from)
                        .or_else(|| v.get("value")?.as_str().map(String::from))
                });

                let score = compute_score(&text, &prefix_lower) + 10.0; // LSP boost
                let kind = item
                    .kind
                    .map(CompletionKind::from_lsp_kind)
                    .unwrap_or(CompletionKind::Text);

                let lsp_detail = match &item.detail {
                    Some(d) if !d.trim().is_empty() => Some(format!("{} [lsp]", d)),
                    _ => None,
                };
                all_entries.push(CompletionEntry {
                    text,
                    label,
                    detail: lsp_detail,
                    documentation: doc,
                    kind,
                    source: CompletionSource::Lsp,
                    score,
                    lsp_item: Some(item),
                });
            }
        }

        // Filter and score all candidates together
        let filtered = filter_and_score_entries(all_entries, &prefix);

        // Update completion state
        if !filtered.is_empty() {
            editor.completion.base_items = filtered.clone();
            editor.completion.items = filtered;
            editor.completion.selected_index = 0;
            editor.completion.active = true;

            let position = editor
                .windows
                .active_window()
                .map(|w| w.cursor.position)
                .unwrap_or_default();
            let line_text = editor
                .current_buffer()
                .and_then(|b| b.line_text(position.line))
                .unwrap_or_default();

            editor.completion.context = Some(CompletionContext {
                trigger: prefix,
                position,
                line_text,
                is_path: is_path_trigger(&ctx.word_prefix),
                after_trigger_char: ctx.is_after_trigger,
            });
        } else {
            editor.completion.cancel();
        }
    }
    /// Update a resolved LSP item's documentation/detail in base_items
    /// and re-apply the filter so the display refreshes.
    pub fn update_resolved_item(&mut self, resolved: &crate::lsp::CompletionItem) {
        let label = &resolved.label;
        let new_doc = resolved.documentation.as_ref().and_then(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.get("value")?.as_str().map(String::from))
        });
        let new_detail = resolved.detail.clone();

        let mut found = false;
        for item in &mut self.base_items {
            if item.source == CompletionSource::Lsp && item.label == *label {
                if let Some(doc) = &new_doc {
                    if !doc.is_empty() {
                        item.documentation = Some(doc.clone());
                    }
                }
                if let Some(detail) = &new_detail {
                    if !detail.trim().is_empty() {
                        item.detail = Some(format!("{} [lsp]", detail));
                    } else {
                        item.detail = None;
                    }
                }
                item.lsp_item = Some(resolved.clone());
                found = true;
                break;
            }
        }

        if found {
            // Preserve the current selection across the filter refresh
            let saved = self.selected_index;
            self.apply_filter();
            self.selected_index = saved.min(self.items.len().saturating_sub(1));
        }
    }
    /// Return completion items as pre-formatted, right-aligned strings.
    /// Ready to be passed directly to a popup renderer that expects `Vec<String>`.
    pub fn formatted_items(&self) -> Vec<String> {
        if self.items.is_empty() {
            return Vec::new();
        }

        // Calculate the maximum width of the left side (kind + label)
        let max_left_len = self
            .items
            .iter()
            .map(|i| {
                let kind_str = i.kind.as_str();
                let kind_len = if kind_str.is_empty() {
                    0
                } else {
                    kind_str.chars().count() + 1 // +1 for the space after the kind badge
                };
                kind_len + i.label.chars().count()
            })
            .max()
            .unwrap_or(0);

        self.items
            .iter()
            .map(|item| {
                let kind_str = item.kind.as_str();
                let left = if kind_str.is_empty() {
                    item.label.clone()
                } else {
                    format!("{} {}", kind_str, item.label)
                };

                // Pad the left side so the detail aligns perfectly
                let padded_left = format!("{:<width$}", left, width = max_left_len);

                match &item.detail {
                    Some(detail) => format!("{}  {}", padded_left, detail), // 2 spaces gap
                    None => padded_left,
                }
            })
            .collect()
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

    let base = if trigger.starts_with('/') {
        Path::new("/")
    } else {
        base_dir.and_then(|p| p.parent()).unwrap_or(Path::new("."))
    };

    let dir_slash = trigger.ends_with('/');

    let (full_dir, parent_str, file_prefix) = if dir_slash {
        let dir_path = base.join(trigger.trim_end_matches('/'));
        let parent = if trigger == "/" {
            "/".to_string()
        } else {
            trigger.trim_end_matches('/').to_string()
        };
        (dir_path, parent, "")
    } else {
        let trigger_path = Path::new(trigger);
        let parent = trigger_path.parent();
        let prefix = trigger_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        match parent {
            Some(p) if !p.as_os_str().is_empty() => {
                (base.join(p), p.to_string_lossy().to_string(), prefix)
            }
            _ => (base.to_path_buf(), String::new(), prefix),
        }
    };

    list_dir_completion_entries(
        &full_dir,
        &file_prefix,
        &parent_str,
        !trigger.starts_with('.'),
    )
}

pub fn collect_vocab_words(
    vocab: &crate::vocab::VocabManager,
    prefix: &str,
) -> Vec<CompletionEntry> {
    let prefix_lower = prefix.to_lowercase();
    vocab
        .words()
        .iter()
        .filter(|w| {
            let wl = w.to_lowercase();
            wl.starts_with(&prefix_lower) && w.len() > prefix.len()
        })
        .map(|w| {
            let score = compute_score(w, &prefix_lower) + 5.0;
            CompletionEntry {
                text: w.clone(),
                label: w.clone(),
                detail: Some("[vocab]".into()),
                documentation: None,
                kind: CompletionKind::Text,
                source: CompletionSource::Vocab,
                score,
                lsp_item: None,
            }
        })
        .collect()
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
                text: text.clone(),
                label: text,
                detail: Some("[buffer]".into()),
                documentation: None,
                kind: CompletionKind::Text,
                source: CompletionSource::BufferWords,
                score,
                lsp_item: None,
            }
        })
        .collect()
}

/// Compute a relevance score for a completion item.
/// Higher is better. Uses prefix, substring, and fuzzy matching heuristics.
pub fn compute_score(text: &str, trigger: &str) -> f64 {
    if trigger.is_empty() {
        return 0.0;
    }

    let text_lower = text.to_lowercase();
    let trigger_lower = trigger.to_lowercase();

    // Exact match bonus.
    if text_lower == trigger_lower {
        return 100.0;
    }

    // Prefix match (best for completion)
    if text_lower.starts_with(&trigger_lower) {
        let coverage = trigger.len() as f64 / text.len().max(1) as f64;
        return coverage * 50.0;
    }

    // Substring match
    if text_lower.contains(&trigger_lower) {
        return 5.0;
    }

    // Fuzzy match fallback (e.g., "ins_" matches "insert_str")
    if fuzzy_match(&text_lower, &trigger_lower) {
        return 2.0;
    }

    0.0
}
// ── Add after the existing `collect_file_paths` function ──────────

/// List directory entries matching a file prefix, returning completion items.
///
/// * `dir`          – directory to scan
/// * `file_prefix`  – filter: only entries whose name starts with this (case-insensitive)
/// * `path_prefix`  – string to prepend to each entry's `text` (e.g. `"src"`)
/// * `skip_hidden`  – skip dotfiles unless `file_prefix` itself starts with a dot
fn list_dir_completion_entries(
    dir: &Path,
    file_prefix: &str,
    path_prefix: &str,
    skip_hidden: bool,
) -> Vec<CompletionEntry> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let prefix_lower = file_prefix.to_lowercase();
    let mut items = Vec::new();

    for entry in entries.flatten() {
        let name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        // Skip hidden files unless the prefix explicitly starts with '.'
        if skip_hidden && name.starts_with('.') && !file_prefix.starts_with('.') {
            continue;
        }

        // Filter by prefix
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
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

        let display_name = if is_dir {
            format!("{}/", name)
        } else {
            name.clone()
        };

        // Build the full text to insert, joining path_prefix / display_name
        let insert_text = if path_prefix.is_empty() {
            display_name
        } else if path_prefix.ends_with('/') {
            // Root-like prefix ("/") — don't double the slash
            format!("{}{}", path_prefix, display_name)
        } else {
            format!("{}/{}", path_prefix, display_name)
        };

        let score = compute_score(&name, &prefix_lower) + 15.0;

        let detail = if is_dir {
            Some("dir".to_string())
        } else {
            let len = metadata.len();
            if len < 1024 {
                Some(format!("{} B", len))
            } else if len < 1024 * 1024 {
                Some(format!("{:.1} KB", len as f64 / 1024.0))
            } else {
                Some(format!("{:.1} MB", len as f64 / (1024.0 * 1024.0)))
            }
        };

        items.push(CompletionEntry {
            text: insert_text.clone(),
            label: insert_text,
            detail,
            documentation: None,
            kind,
            source: CompletionSource::FilePath,
            score,
            lsp_item: None,
        });

        // Early cutoff to avoid collecting too many items
        if items.len() >= 200 {
            break;
        }
    }

    // Sort directories first, then alphabetically
    items.sort_by(|a, b| {
        let a_dir = a.kind == CompletionKind::Folder;
        let b_dir = b.kind == CompletionKind::Folder;
        b_dir.cmp(&a_dir).then_with(|| a.text.cmp(&b.text))
    });

    items
}

/// Collect file/directory completions for command-line arguments (e.g. `:e`).
///
/// Unlike [`collect_file_paths`], this works with **any** prefix — not just
/// path-like triggers (`./`, `/`, `../`). When the prefix is not a path, it
/// lists files in the base directory that match the prefix.
///
/// `base_dir` should be the *current buffer's file path* (its parent will be
/// used as the directory to search).
pub fn collect_file_completions_for_arg(
    prefix: &str,
    base_dir: Option<&Path>,
) -> Vec<CompletionEntry> {
    let base = if prefix.starts_with('/') {
        Path::new("/").to_path_buf()
    } else {
        base_dir
            .and_then(|p| p.parent())
            .unwrap_or(Path::new("."))
            .to_path_buf()
    };

    if is_path_trigger(prefix) {
        // Path-like prefix — determine directory and file-name parts.
        let dir_slash = prefix.ends_with('/');

        let (full_dir, parent_str, file_prefix) = if dir_slash {
            // "src/" → list contents of src/ with empty file prefix
            let dir_path = base.join(prefix.trim_end_matches('/'));
            let parent = if prefix == "/" {
                "/".to_string()
            } else {
                prefix.trim_end_matches('/').to_string()
            };
            (dir_path, parent, "")
        } else {
            // "src/ma" → list contents of src/ matching "ma"
            let trigger_path = Path::new(prefix);
            let parent = trigger_path.parent();
            let file_prefix = trigger_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            match parent {
                Some(p) if !p.as_os_str().is_empty() => {
                    (base.join(p), p.to_string_lossy().to_string(), file_prefix)
                }
                _ => (base.to_path_buf(), String::new(), file_prefix),
            }
        };

        list_dir_completion_entries(
            &full_dir,
            file_prefix,
            &parent_str,
            !prefix.starts_with('.'),
        )
    } else {
        // Non-path prefix — list files in base directory matching the prefix
        list_dir_completion_entries(&base, prefix, "", !prefix.starts_with('.'))
    }
}

// ── Unified completion pipeline ────────────────────────────────────

/// Collect local (non-LSP) completion candidates from buffer words,
/// vocabulary, and file paths.
fn collect_local_completions(editor: &Editor, ctx: &CursorContext) -> Vec<CompletionEntry> {
    let mut entries = Vec::new();
    let prefix = ctx.filter_prefix();

    // Buffer words
    if let Some(buffer) = editor.current_buffer() {
        if prefix.len() >= editor.completion.trigger_len {
            entries.extend(collect_buffer_words(buffer, prefix));
        }
    }

    // Vocabulary words
    if prefix.len() >= editor.completion.trigger_len {
        entries.extend(collect_vocab_words(&editor.vocab, prefix));
    }

    // File paths
    if is_path_trigger(prefix) {
        let base_dir = editor.current_buffer().and_then(|b| b.file_path.as_deref());
        entries.extend(collect_file_paths(prefix, base_dir));
    }

    entries
}

/// Filter and score completion entries against the given prefix.
fn filter_and_score_entries(entries: Vec<CompletionEntry>, prefix: &str) -> Vec<CompletionEntry> {
    let prefix_lower = prefix.to_lowercase();

    let mut filtered: Vec<CompletionEntry> = entries
        .into_iter()
        .filter(|item| {
            prefix_lower.is_empty() || item.text.to_lowercase().starts_with(&prefix_lower)
        })
        .map(|mut item| {
            item.score = compute_score(&item.text, &prefix_lower)
                + if item.source == CompletionSource::Lsp {
                    10.0
                } else {
                    0.0
                };
            item
        })
        .collect();

    // Sort by score descending first, then deduplicate keeping highest score
    filtered.sort_by(|a, b| {
        let a_exact = a.text.to_lowercase().starts_with(&prefix_lower);
        let b_exact = b.text.to_lowercase().starts_with(&prefix_lower);
        match (a_exact, b_exact) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b
                .score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    });

    let mut seen = std::collections::HashSet::new();
    filtered.retain(|item| seen.insert(item.text.to_lowercase()));

    filtered
}
/// Extract cursor context from the editor for completion.
pub fn extract_cursor_context(editor: &Editor) -> CursorContext {
    let window = match editor.windows.active_window() {
        Some(w) => w,
        None => return CursorContext::default(),
    };

    let buffer = match editor.current_buffer() {
        Some(b) => b,
        None => return CursorContext::default(),
    };

    let pos = window.cursor.position;
    let line = match buffer.line_text(pos.line) {
        Some(l) => l,
        None => return CursorContext::default(),
    };

    let col = pos.col;
    let graphemes: Vec<&str> = line.graphemes(true).collect();

    // Scan back to find word start
    let mut word_start = col;
    while word_start > 0 {
        if let Some(g) = graphemes.get(word_start - 1) {
            if is_identifier_char(g) {
                word_start -= 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    let word_prefix: String = graphemes[word_start..col].join("");

    let trigger_char = if word_start > 0 {
        graphemes
            .get(word_start.saturating_sub(1))
            .and_then(|g| g.chars().next())
    } else {
        None
    };

    let is_after_trigger = matches!(trigger_char, Some('.') | Some(':'));
    let post_trigger_prefix = word_prefix.clone();

    CursorContext {
        word_prefix,
        start_col: word_start,
        trigger_char,
        post_trigger_prefix,
        is_after_trigger,
    }
}

pub fn fuzzy_match(text: &str, query: &str) -> bool {
    let mut text_chars = text.chars();
    for q_char in query.chars() {
        if text_chars.find(|&c| c == q_char).is_none() {
            return false;
        }
    }
    true
}
