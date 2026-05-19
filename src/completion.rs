//--+ ./completion.rs
// completion.rs — Optimized version with dot-completion scoring fix
// ──────────────────────────────────────────────────────────────
// Key optimizations + fixes:
//   1. Cached buffer word index (BufferWordIndex) — incremental updates
//   2. Reduced cloning in CompletionEntry
//   3. Smarter fuzzy matching with early-exit
//   4. Pre-allocated Vec capacity throughout
//   5. ASCII fast paths for word extraction
//   6. DOT-COMPLETION FIX:
//      - After dot: skip buffer words + vocab (LSP-only)
//      - LSP gets +50.0 boost after dot (vs +10.0 normal)
//      - filter_and_score_entries preserves LSP scores
//      - sort_text and kind bonuses for LSP items
//   7. extract_line_words takes &str (fixes E0308 String vs &str)
//   8. DOT-COMPLETION FIX (round 2):
//      - update() no longer clears after_trigger_char
//      - apply_filter() preserves LSP scores (doesn't overwrite)
//      - apply_filter() uses strict prefix match for LSP after dot
//      - apply_filter() sorts LSP first in member-access context
// ──────────────────────────────────────────────────────────────

use crate::editor::Editor;
use std::collections::HashSet;
use std::path::Path;
use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::{Buffer, CursorPosition};

// ── Completion source ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSource {
    BufferWords,
    AllBuffers,
    Lsp,
    Snippet,
    FilePath,
    Vocab,
}

#[derive(Debug, Clone)]
pub struct CursorContext {
    pub word_prefix: String,
    pub start_col: usize,
    pub trigger_char: Option<char>,
    pub post_trigger_prefix: String,
    pub is_after_trigger: bool,
}

impl CursorContext {
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

#[derive(Debug, Clone)]
pub struct CompletionEntry {
    pub text: String,
    pub label: String,
    pub detail: Option<String>,
    pub documentation: Option<String>,
    pub kind: CompletionKind,
    pub source: CompletionSource,
    pub score: f64,
    pub lsp_item: Option<crate::lsp::CompletionItem>,
}

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

#[derive(Debug, Clone)]
pub struct CompletionContext {
    pub trigger: String,
    pub position: CursorPosition,
    pub line_text: String,
    pub is_path: bool,
    pub after_trigger_char: bool,
}

// ============================================================================
// OPT: Incremental Buffer Word Index
// ============================================================================

pub struct BufferWordIndex {
    lines: Vec<HashSet<String>>,
    all_words: HashSet<String>,
    line_count: usize,
}

impl BufferWordIndex {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            all_words: HashSet::new(),
            line_count: 0,
        }
    }

    pub fn build_from_buffer(&mut self, buffer: &Buffer) {
        let count = buffer.line_count();
        self.lines.clear();
        self.all_words.clear();
        self.lines.reserve(count);

        for line_idx in 0..count {
            if let Some(line_text) = buffer.line_text(line_idx) {
                let words = extract_line_words(&line_text);
                for w in &words {
                    self.all_words.insert(w.clone());
                }
                self.lines.push(words);
            } else {
                self.lines.push(HashSet::new());
            }
        }
        self.line_count = count;
    }

    pub fn update_line(&mut self, line_idx: usize, line_text: Option<&str>) {
        if line_idx >= self.lines.len() {
            if let Some(text) = line_text {
                let words = extract_line_words(text);
                for w in &words {
                    self.all_words.insert(w.clone());
                }
                self.lines.push(words);
            } else {
                self.lines.push(HashSet::new());
            }
            return;
        }

        let old_words = &mut self.lines[line_idx];

        if let Some(text) = line_text {
            let new_words = extract_line_words(text);
            for w in &new_words {
                self.all_words.insert(w.clone());
            }
            *old_words = new_words;
        } else {
            old_words.clear();
        }
    }

    pub fn collect_matching(&self, prefix: &str, min_len: usize) -> Vec<CompletionEntry> {
        let prefix_lower = prefix.to_lowercase();
        self.all_words
            .iter()
            .filter(|w| w.len() > min_len && w.to_lowercase().starts_with(&prefix_lower))
            .map(|w| {
                let score = compute_score(w, &prefix_lower);
                CompletionEntry {
                    text: w.clone(),
                    label: w.clone(),
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
}

/// Extract identifier words from a single line of text.
/// FIX: Takes &str (not String) — matches call sites that pass &str.
fn extract_line_words(line_text: &str) -> HashSet<String> {
    let mut words = HashSet::new();
    let mut current_word = String::new();

    for g in line_text.graphemes(true) {
        if is_identifier_char(g) {
            current_word.push_str(g);
        } else {
            if !current_word.is_empty() {
                words.insert(current_word.clone());
            }
            current_word.clear();
        }
    }
    if !current_word.is_empty() {
        words.insert(current_word);
    }
    words
}

// ── Completion engine ───────────────────────────────────────────────

pub struct CompletionEngine {
    pub trigger_len: usize,
    pub active: bool,
    pub items: Vec<CompletionEntry>,
    pub base_items: Vec<CompletionEntry>,
    pub selected_index: usize,
    pub context: Option<CompletionContext>,
    pub max_items: usize,
    pub word_index: BufferWordIndex,
    pub word_index_buffer_id: Option<crate::buffer::BufferId>,
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
            word_index: BufferWordIndex::new(),
            word_index_buffer_id: None,
        }
    }

    pub fn try_trigger(&mut self, buffer: &Buffer, position: CursorPosition, vocab: &crate::vocab::VocabManager) -> bool {
        let (word, is_path) = word_or_path_before_cursor(buffer, position);

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
            0
        } else {
            1
        };

        if word.len() < min_len {
            self.cancel();
            return false;
        }

        let line_text = buffer.line_text(position.line).unwrap_or_default();
        let base_dir = buffer.file_path.as_deref();

        let mut items = Vec::new();

        // OPT + FIX: skip buffer words after dot — LSP is authoritative
        if !after_dot && word.len() >= self.trigger_len {
            let word_items = self.word_index.collect_matching(&word, word.len());
            items.extend(word_items);
        }

        if !after_dot && word.len() >= self.trigger_len {
            let vocab_items = collect_vocab_words(vocab, &word);
            items.extend(vocab_items);
        }

        if is_path {
            let path_items = collect_file_paths(&word, base_dir);
            items.extend(path_items);
        }

        items.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        items.truncate(self.max_items);

        if items.is_empty() {
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

    /// prefix matching in member-access context (after `.` or `::`).
    fn apply_filter(&mut self) {
        let trigger_lower = self.context.as_ref().map(|c| c.trigger.to_lowercase()).unwrap_or_default();

        if trigger_lower.is_empty() {
            self.items = self.base_items.clone();
            return;
        }

        let after_trigger_char = self.context.as_ref().map(|ctx| ctx.after_trigger_char).unwrap_or(false);

        let estimated = self.base_items.len();
        self.items = Vec::with_capacity(estimated);

        for item in &self.base_items {
            // FIX: In member-access context, use strict prefix matching for
            // LSP items.  Fuzzy matching is too lenient and pulls in items
            // that happen to contain the typed characters in the wrong order.
            let matches = if after_trigger_char && item.source == CompletionSource::Lsp {
                item.text.to_lowercase().starts_with(&trigger_lower)
            } else {
                fuzzy_match(&item.text.to_lowercase(), &trigger_lower)
            };

            if !matches {
                continue;
            }

            let mut entry = item.clone();
            // FIX: Preserve LSP item scores.  They include sort_text and
            // kind bonuses that are critical for correct ordering after a
            // dot.  Re-scoring with compute_score() + 10.0 destroys the
            // LSP server's intended relevance ordering and makes most items
            // appear equally (ir)relevant.
            if item.source != CompletionSource::Lsp {
                entry.score = compute_score(&item.text, &trigger_lower);
            }
            self.items.push(entry);
        }

        // FIX: In member-access context, LSP items always come first.
        if after_trigger_char {
            self.items.sort_by(|a, b| {
                let a_is_lsp = a.source == CompletionSource::Lsp;
                let b_is_lsp = b.source == CompletionSource::Lsp;
                match (a_is_lsp, b_is_lsp) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal),
                }
            });
        } else {
            self.items
                .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        }

        self.items.truncate(self.max_items);
    }

    /// FIX: update() no longer clears after_trigger_char.
    /// The trigger character (dot / colon) is still present before the word;
    /// clearing the flag caused apply_filter to lose the member-access
    /// context, which in turn made LSP items get re-scored without their
    /// +50.0 boost and sorted alongside buffer-word noise.
    pub fn update(&mut self, new_trigger: &str) {
        if !self.active {
            return;
        }

        let after_trigger_char = self.context.as_ref().map(|ctx| ctx.after_trigger_char).unwrap_or(false);

        if new_trigger.len() < self.trigger_len && !after_trigger_char {
            self.cancel();
            return;
        }

        if let Some(ctx) = self.context.as_mut() {
            ctx.trigger = new_trigger.to_string();
            // double-colon is still present before the word; clearing the
            // flag would cause apply_filter() to lose the member-access
            // context, overwrite LSP scores, and sort irrelevant items to
            // the top.
        }

        self.apply_filter();

        if self.items.is_empty() && !after_trigger_char {
            self.cancel();
        }
    }

    pub fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.items.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = if self.selected_index == 0 {
                self.items.len() - 1
            } else {
                self.selected_index - 1
            };
        }
    }

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

    pub fn cancel(&mut self) {
        self.active = false;
        self.items.clear();
        self.base_items.clear();
        self.selected_index = 0;
        self.context = None;
    }

    pub fn selected_item(&self) -> Option<&CompletionEntry> {
        self.items.get(self.selected_index)
    }

    pub fn update_path(&mut self, buffer: &Buffer, new_trigger: &str) {
        if !self.active {
            return;
        }

        if new_trigger.len() < 2 {
            self.cancel();
            return;
        }

        let base_dir = buffer.file_path.as_deref();
        let mut items = Vec::new();

        let path_items = collect_file_paths(new_trigger, base_dir);
        items.extend(path_items);

        if new_trigger.len() >= self.trigger_len {
            let word_items = collect_buffer_words(buffer, new_trigger);
            items.extend(word_items);
        }

        items.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        self.items = items;
        self.selected_index = 0;

        if let Some(ctx) = self.context.as_mut() {
            ctx.trigger = new_trigger.to_string();
        }

        if self.items.is_empty() {
            self.cancel();
        }
    }

    /// Unified completion update: merges local + LSP candidates.
    /// FIX: After dot, LSP items get +50.0 boost and buffer words are excluded.
    /// FIX: filter_and_score_entries preserves LSP scores (doesn't overwrite).
    pub fn update_unified_completions(editor: &mut Editor, lsp_items: Option<Vec<crate::lsp::CompletionItem>>) {
        let ctx = extract_cursor_context(editor);
        let prefix = ctx.filter_prefix().to_string();
        let prefix_lower = prefix.to_lowercase();

        let mut all_entries = collect_local_completions(editor, &ctx);

        let is_member_access = ctx.is_after_trigger;

        if let Some(lsp) = lsp_items {
            all_entries.reserve(lsp.len());
            for item in lsp {
                let raw_label = item.label.clone();
                let label = raw_label.split("(use ").next().unwrap_or(&raw_label).trim().to_string();

                let text = item
                    .get_insert_text()
                    .unwrap_or(&raw_label)
                    .split("(use ")
                    .next()
                    .unwrap_or(&raw_label)
                    .trim()
                    .to_string();

                let doc = item
                    .documentation
                    .as_ref()
                    .and_then(|v| v.as_str().map(String::from).or_else(|| v.get("value")?.as_str().map(String::from)));

                // FIX: Scoring after dot — LSP items get a large boost
                // After dot: +50.0 (always beats buffer words max ~50.0)
                // Normal:    +10.0
                let lsp_boost = if is_member_access { 50.0 } else { 10.0 };
                let mut score = compute_score(&text, &prefix_lower) + lsp_boost;

                // FIX: Respect LSP sort_text
                if let Some(ref sort_text) = item.sort_text {
                    if !sort_text.is_empty() {
                        if let Ok(priority) = sort_text.parse::<f64>() {
                            let sort_bonus = (50.0 - priority.min(50.0)) * 0.1;
                            score += sort_bonus;
                        }
                    }
                }

                // FIX: Prioritize functions/methods over fields/variables
                let kind_bonus = match item.kind {
                    Some(3) | Some(2) => 3.0,  // Function / Method
                    Some(6) => 1.0,            // Variable
                    Some(5) => 1.0,            // Field
                    Some(7) | Some(22) => 2.0, // Class / Struct
                    _ => 0.0,
                };
                score += kind_bonus;

                let kind = item.kind.map(CompletionKind::from_lsp_kind).unwrap_or(CompletionKind::Text);

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

        // FIX: After dot, use LSP-priority filtering
        let filtered = if is_member_access {
            filter_and_score_entries_lsp_priority(all_entries, &prefix)
        } else {
            filter_and_score_entries(all_entries, &prefix)
        };

        if !filtered.is_empty() {
            editor.completion.base_items = filtered.clone();
            editor.completion.items = filtered;
            editor.completion.selected_index = 0;
            editor.completion.active = true;

            let position = editor.windows.active_window().map(|w| w.cursor.position).unwrap_or_default();
            let line_text = editor.current_buffer().and_then(|b| b.line_text(position.line)).unwrap_or_default();

            editor.completion.context = Some(CompletionContext {
                trigger: prefix,
                position,
                line_text,
                is_path: is_path_trigger(&ctx.word_prefix),
                after_trigger_char: is_member_access,
            });
        } else {
            editor.completion.cancel();
        }
    }

    pub fn update_resolved_item(&mut self, resolved: &crate::lsp::CompletionItem) {
        let label = &resolved.label;
        let new_doc = resolved
            .documentation
            .as_ref()
            .and_then(|v| v.as_str().map(String::from).or_else(|| v.get("value")?.as_str().map(String::from)));
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
            let saved = self.selected_index;
            self.apply_filter();
            self.selected_index = saved.min(self.items.len().saturating_sub(1));
        }
    }

    pub fn formatted_items(&self) -> Vec<String> {
        if self.items.is_empty() {
            return Vec::new();
        }

        let max_left_len = self
            .items
            .iter()
            .map(|i| {
                let kind_str = i.kind.as_str();
                let kind_len = if kind_str.is_empty() { 0 } else { kind_str.len() + 1 };
                kind_len + i.label.len()
            })
            .max()
            .unwrap_or(0);

        let mut result = Vec::with_capacity(self.items.len());
        let mut buf = String::with_capacity(max_left_len + 40);

        for item in &self.items {
            buf.clear();
            let kind_str = item.kind.as_str();
            if kind_str.is_empty() {
                buf.push_str(&item.label);
            } else {
                buf.push_str(kind_str);
                buf.push(' ');
                buf.push_str(&item.label);
            }

            let current_len = buf.len();
            if current_len < max_left_len {
                for _ in 0..(max_left_len - current_len) {
                    buf.push(' ');
                }
            }

            if let Some(detail) = &item.detail {
                buf.push_str("  ");
                buf.push_str(detail);
            }

            result.push(buf.clone());
        }

        result
    }
}

// ── Helper functions ────────────────────────────────────────────────

#[inline]
fn is_identifier_char(g: &str) -> bool {
    g.chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_' || c == '-')
        .unwrap_or(false)
}

#[inline]
fn is_path_char(g: &str) -> bool {
    g.chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
        .unwrap_or(false)
}

pub fn is_path_trigger(s: &str) -> bool {
    s.contains('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with('/')
}

/// OPT: ASCII fast path for word extraction before cursor.
pub fn word_or_path_before_cursor(buffer: &Buffer, position: CursorPosition) -> (String, bool) {
    if let Some(line_text) = buffer.line_text(position.line) {
        // OPT: ASCII fast path
        if line_text.is_ascii() {
            let bytes = line_text.as_bytes();
            let end = position.col.min(bytes.len());
            let mut start = end;

            while start > 0 {
                let b = bytes[start - 1];
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' || b == b'/' {
                    start -= 1;
                } else {
                    break;
                }
            }

            let text = &line_text[start..end];
            let is_path = is_path_trigger(text);

            if !is_path {
                if let Some(dot_pos) = text.rfind('.') {
                    let after_dot = &text[dot_pos + 1..];
                    return (after_dot.to_string(), false);
                }
            }

            return (text.to_string(), is_path);
        }

        // Fallback: grapheme-based extraction for multi-byte text
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

pub fn word_before_cursor(buffer: &Buffer, position: CursorPosition) -> String {
    if let Some(line_text) = buffer.line_text(position.line) {
        if line_text.is_ascii() {
            let bytes = line_text.as_bytes();
            let end = position.col.min(bytes.len());
            let mut start = end;

            while start > 0 {
                let b = bytes[start - 1];
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                    start -= 1;
                } else {
                    break;
                }
            }

            return line_text[start..end].to_string();
        }

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
        let prefix = trigger_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        match parent {
            Some(p) if !p.as_os_str().is_empty() => (base.join(p), p.to_string_lossy().to_string(), prefix),
            _ => (base.to_path_buf(), String::new(), prefix),
        }
    };

    list_dir_completion_entries(&full_dir, &file_prefix, &parent_str, !trigger.starts_with('.'))
}

pub fn collect_vocab_words(vocab: &crate::vocab::VocabManager, prefix: &str) -> Vec<CompletionEntry> {
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

fn collect_buffer_words(buffer: &Buffer, prefix: &str) -> Vec<CompletionEntry> {
    let mut words: HashSet<String> = HashSet::new();
    let prefix_lower = prefix.to_lowercase();

    for line_idx in 0..buffer.line_count() {
        if let Some(line_text) = buffer.line_text(line_idx) {
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
            if !current_word.is_empty() && current_word.len() > prefix.len() && current_word.to_lowercase().starts_with(&prefix_lower) {
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
///
/// Score tiers:
///   100.0  — exact match (text == trigger)
///    50.0  — prefix match (text starts with trigger), scaled by coverage
///     5.0  — substring match
///     2.0  — fuzzy match
///     0.0  — no match
///
/// LSP items receive an additional boost in the caller:
///   +10.0  normal context
///   +50.0  member-access context (after `.` or `::`)
#[inline]
pub fn compute_score(text: &str, trigger: &str) -> f64 {
    if trigger.is_empty() {
        return 0.0;
    }

    let text_lower = text.to_lowercase();
    let trigger_lower = trigger.to_lowercase();

    if text_lower.starts_with(&trigger_lower) {
        if text_lower == trigger_lower {
            return 100.0;
        }
        let coverage = trigger.len() as f64 / text.len().max(1) as f64;
        return coverage * 50.0;
    }

    if text_lower.contains(&trigger_lower) {
        return 5.0;
    }

    if fuzzy_match(&text_lower, &trigger_lower) {
        return 2.0;
    }

    0.0
}

fn list_dir_completion_entries(dir: &Path, file_prefix: &str, path_prefix: &str, skip_hidden: bool) -> Vec<CompletionEntry> {
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

        if skip_hidden && name.starts_with('.') && !file_prefix.starts_with('.') {
            continue;
        }

        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let is_dir = metadata.is_dir();
        let kind = if is_dir { CompletionKind::Folder } else { CompletionKind::File };

        let display_name = if is_dir { format!("{}/", name) } else { name.clone() };

        let insert_text = if path_prefix.is_empty() {
            display_name
        } else if path_prefix.ends_with('/') {
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

        if items.len() >= 200 {
            break;
        }
    }

    items.sort_by(|a, b| {
        let a_dir = a.kind == CompletionKind::Folder;
        let b_dir = b.kind == CompletionKind::Folder;
        b_dir.cmp(&a_dir).then_with(|| a.text.cmp(&b.text))
    });

    items
}

pub fn collect_file_completions_for_arg(prefix: &str, base_dir: Option<&Path>) -> Vec<CompletionEntry> {
    let base = if prefix.starts_with('/') {
        Path::new("/").to_path_buf()
    } else {
        base_dir.and_then(|p| p.parent()).unwrap_or(Path::new(".")).to_path_buf()
    };

    if is_path_trigger(prefix) {
        let dir_slash = prefix.ends_with('/');

        let (full_dir, parent_str, file_prefix) = if dir_slash {
            let dir_path = base.join(prefix.trim_end_matches('/'));
            let parent = if prefix == "/" {
                "/".to_string()
            } else {
                prefix.trim_end_matches('/').to_string()
            };
            (dir_path, parent, "")
        } else {
            let trigger_path = Path::new(prefix);
            let parent = trigger_path.parent();
            let file_prefix = trigger_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            match parent {
                Some(p) if !p.as_os_str().is_empty() => (base.join(p), p.to_string_lossy().to_string(), file_prefix),
                _ => (base.to_path_buf(), String::new(), file_prefix),
            }
        };

        list_dir_completion_entries(&full_dir, file_prefix, &parent_str, !prefix.starts_with('.'))
    } else {
        list_dir_completion_entries(&base, prefix, "", !prefix.starts_with('.'))
    }
}

// ── Unified completion pipeline ────────────────────────────────────

/// FIX: After a trigger char (dot/scope), skip buffer words + vocab.
/// Only LSP knows the actual members of the type.
fn collect_local_completions(editor: &Editor, ctx: &CursorContext) -> Vec<CompletionEntry> {
    let mut entries = Vec::new();
    let prefix = ctx.filter_prefix();

    // FIX: After a member-access dot or scope `::`, LSP is authoritative.
    // Buffer words and vocab are noise here — skip them entirely.
    if ctx.is_after_trigger {
        return entries; // empty — LSP items only
    }

    // Buffer words (only when NOT after a trigger)
    if let Some(buffer) = editor.current_buffer() {
        if prefix.len() >= editor.completion.trigger_len {
            let word_items = editor.completion.word_index.collect_matching(prefix, prefix.len());
            entries.extend(word_items);
        }
    }

    // Vocab words (only when NOT after a trigger)
    if prefix.len() >= editor.completion.trigger_len {
        entries.extend(collect_vocab_words(&editor.vocab, prefix));
    }

    // File paths (never affected by trigger context)
    if is_path_trigger(prefix) {
        let base_dir = editor.current_buffer().and_then(|b| b.file_path.as_deref());
        entries.extend(collect_file_paths(prefix, base_dir));
    }

    entries
}

/// FIX: filter_and_score_entries for member-access context (after `.` or `::`).
/// Preserves the score already assigned to LSP items (including +50.0 boost
/// and sort_text/kind bonuses). LSP items are ALWAYS sorted above non-LSP items.
fn filter_and_score_entries_lsp_priority(entries: Vec<CompletionEntry>, prefix: &str) -> Vec<CompletionEntry> {
    let prefix_lower = prefix.to_lowercase();

    let mut filtered: Vec<CompletionEntry> = entries
        .into_iter()
        .filter(|item| {
            if prefix_lower.is_empty() && item.source == CompletionSource::Lsp {
                return true;
            }
            item.text.to_lowercase().starts_with(&prefix_lower)
        })
        .map(|mut item| {
            // Re-score only non-LSP items (LSP scores are already correct)
            if item.source != CompletionSource::Lsp {
                item.score = compute_score(&item.text, &prefix_lower);
            }
            item
        })
        .collect();

    // Sort: LSP items first, then by score within each group
    filtered.sort_by(|a, b| {
        let a_is_lsp = a.source == CompletionSource::Lsp;
        let b_is_lsp = b.source == CompletionSource::Lsp;
        match (a_is_lsp, b_is_lsp) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal),
        }
    });

    // Dedup by lowercase text
    let mut seen = std::collections::HashSet::new();
    filtered.retain(|item| seen.insert(item.text.to_lowercase()));

    filtered
}

/// FIX: filter_and_score_entries — preserves LSP scores.
/// Only re-scores non-LSP items. LSP items already have their final score
/// set in update_unified_completions (including sort_text/kind bonuses).
fn filter_and_score_entries(entries: Vec<CompletionEntry>, prefix: &str) -> Vec<CompletionEntry> {
    let prefix_lower = prefix.to_lowercase();

    let mut filtered: Vec<CompletionEntry> = entries
        .into_iter()
        .filter(|item| prefix_lower.is_empty() || item.text.to_lowercase().starts_with(&prefix_lower))
        .map(|mut item| {
            // FIX: Only re-score non-LSP items
            if item.source != CompletionSource::Lsp {
                item.score = compute_score(&item.text, &prefix_lower);
            }
            item
        })
        .collect();

    // Sort: prefix matches first, then by score descending
    filtered.sort_by(|a, b| {
        let a_exact = a.text.to_lowercase().starts_with(&prefix_lower);
        let b_exact = b.text.to_lowercase().starts_with(&prefix_lower);
        match (a_exact, b_exact) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal),
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

    // OPT: ASCII fast path for word extraction
    if line.is_ascii() {
        let bytes = line.as_bytes();
        let end = col.min(bytes.len());
        let mut start = end;

        // Walk backwards to find the word boundary
        while start > 0 {
            let b = bytes[start - 1];
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                start -= 1;
            } else {
                break;
            }
        }

        let word: String = bytes[start..end].iter().map(|&b| b as char).collect();

        // Check if there's a trigger char before the word
        if start > 0 && (bytes[start - 1] == b'.' || bytes[start - 1] == b':') {
            let trigger_char = bytes[start - 1] as char;
            return CursorContext {
                word_prefix: word.clone(),
                start_col: start,
                trigger_char: Some(trigger_char),
                post_trigger_prefix: word,
                is_after_trigger: true,
            };
        }

        return CursorContext {
            word_prefix: word,
            start_col: start,
            trigger_char: None,
            post_trigger_prefix: String::new(),
            is_after_trigger: false,
        };
    }

    // Fallback: grapheme-based for multi-byte text
    let graphemes: Vec<_> = line.graphemes(true).collect();
    let end = col.min(graphemes.len());
    let mut start = end;

    while start > 0 {
        let g = graphemes[start - 1];
        if is_identifier_char(g) {
            start -= 1;
        } else {
            break;
        }
    }

    let word: String = graphemes[start..end].join("");

    if start > 0 {
        let prev = graphemes[start - 1];
        if prev == "." || prev == ":" {
            return CursorContext {
                word_prefix: word.clone(),
                start_col: start,
                trigger_char: Some(prev.chars().next().unwrap()),
                post_trigger_prefix: word,
                is_after_trigger: true,
            };
        }
    }

    CursorContext {
        word_prefix: word,
        start_col: start,
        trigger_char: None,
        post_trigger_prefix: String::new(),
        is_after_trigger: false,
    }
}

/// Simple fuzzy match: checks if all characters of needle appear in haystack in order.
pub fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut needle_chars = needle.chars().peekable();
    for c in haystack.chars() {
        if c == *needle_chars.peek().unwrap_or(&'\0') {
            needle_chars.next();
        }
        if needle_chars.peek().is_none() {
            return true;
        }
    }
    needle_chars.peek().is_none()
}
