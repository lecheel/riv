// ──────────────────────────────────────────────────────────────
// Architecture:
//   open()        → starts a session (Word / MemberAccess / Path)
//   set_prefix()  → updates prefix each keystroke, re-filters
//   merge_lsp()   → merges LSP response into base_items
//   filter_items()→ internal: filters + sorts base_items by prefix
//   cancel()      → clears session, closes popup
//   confirm()     → returns selected text + prefix length
// ──────────────────────────────────────────────────────────────

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

// ── Trigger mode — set once at session start ────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMode {
    Word,         // normal identifier completion
    MemberAccess, // after '.' or '::'
    Path,         // after '/' or './'
}

// ── Session — stable across the lifetime of one popup ───────────────

#[derive(Debug, Clone)]
pub struct CompletionSession {
    pub mode: TriggerMode,
    pub trigger_line: usize,
    pub trigger_col: usize, // col of the dot / slash / word-start
}

// ── CompletionEngine ────────────────────────────────────────────────

pub struct CompletionEngine {
    pub active: bool,
    pub session: Option<CompletionSession>,

    // frame: recomputed each keystroke
    pub prefix: String,                   // current typed prefix after trigger point
    pub base_items: Vec<CompletionEntry>, // full candidate list (LSP + local)
    pub items: Vec<CompletionEntry>,      // filtered + sorted view
    pub selected_index: usize,

    // config
    pub trigger_len: usize,
    pub max_items: usize,

    // cache
    pub word_index: BufferWordIndex,
    pub word_index_buffer_id: Option<crate::buffer::BufferId>,

    /// Stable selection identity that survives re-filters and LSP merges.
    /// Stores the lowercase text of the currently selected item.
    pub selection_key: Option<String>,
}

impl CompletionEngine {
    pub fn new(trigger_len: usize) -> Self {
        Self {
            active: false,
            session: None,
            prefix: String::new(),
            base_items: Vec::new(),
            items: Vec::new(),
            selected_index: 0,
            trigger_len,
            max_items: 50,
            word_index: BufferWordIndex::new(),
            word_index_buffer_id: None,
            selection_key: None,
        }
    }

    /// Generate a unique key for a completion item that disambiguates
    /// items sharing the same lowercase text (e.g. "Self" the type vs
    /// "self" the keyword, or same-name methods from different traits).
    fn item_key(item: &CompletionEntry) -> String {
        let detail_key = item.detail.as_deref().unwrap_or("");
        format!("{:?}|{}|{}", item.source, item.text.to_lowercase(), detail_key)
    }

    /// Resolve `selected_index` from `selection_key` after re-filtering.
    /// Falls back to clamped index if key is lost.
    fn resolve_selection(&mut self) {
        if let Some(ref key) = self.selection_key {
            if let Some(idx) = self.items.iter().position(|i| Self::item_key(i) == *key) {
                log::debug!(
                    "[completion] resolve_selection: Found key at index {} among {} items",
                    idx,
                    self.items.len()
                );
                self.selected_index = idx;
                return;
            }
            log::debug!("[completion] resolve_selection: Selection key lost. Re-clamping index.");
        }

        let old_index = self.selected_index;
        self.selected_index = self.selected_index.min(self.items.len().saturating_sub(1));
        log::debug!(
            "[completion] resolve_selection: Clamped selection from {} to {} (total items: {})",
            old_index,
            self.selected_index,
            self.items.len()
        );
        self.sync_selection_key();
    }

    /// Sync `selection_key` from the current `selected_index`.
    fn sync_selection_key(&mut self) {
        self.selection_key = self.items.get(self.selected_index).map(|i| Self::item_key(i));
        log::debug!(
            "[completion] sync_selection_key: Selected index: {}, Synchronized key: {:?}",
            self.selected_index,
            self.selection_key
        );
    }

    pub fn filter_items_pub(&self) -> Vec<CompletionEntry> {
        self.filter_items()
    }

    pub fn is_member_access(&self) -> bool {
        matches!(self.session.as_ref().map(|s| s.mode), Some(TriggerMode::MemberAccess))
    }

    pub fn is_path(&self) -> bool {
        matches!(self.session.as_ref().map(|s| s.mode), Some(TriggerMode::Path))
    }

    // ── open — called exactly once per popup lifetime ────────────────
    pub fn open(&mut self, mode: TriggerMode, trigger_col: usize, trigger_line: usize) {
        log::debug!(
            "[completion] open: Initializing session. Mode: {:?}, Line: {}, Trigger Col: {}",
            mode,
            trigger_line,
            trigger_col
        );
        self.cancel();
        self.session = Some(CompletionSession {
            mode,
            trigger_line,
            trigger_col,
        });
        self.active = true;
        self.prefix = String::new();
        self.base_items.clear();
        self.items.clear();
        self.selected_index = 0;
        self.selection_key = None;
    }

    // ── set_prefix — preserve selection across prefix change ─────────
    pub fn set_prefix(&mut self, new_prefix: &str) -> bool {
        let Some(session) = &self.session else {
            log::debug!("[completion] set_prefix: Called without an active session");
            return false;
        };

        log::debug!(
            "[completion] set_prefix: Prefix updating from '{}' to '{}' (Mode: {:?})",
            self.prefix,
            new_prefix,
            session.mode
        );

        match session.mode {
            TriggerMode::MemberAccess => {
                self.prefix = new_prefix.to_string();
                self.base_items.retain(|i| i.source == CompletionSource::Lsp);
                self.items = self.filter_items();
                self.resolve_selection();
                true
            }
            TriggerMode::Word => {
                if new_prefix.len() < self.trigger_len {
                    log::debug!(
                        "[completion] set_prefix (Word): Rejected prefix '{}' (too short, min: {})",
                        new_prefix,
                        self.trigger_len
                    );
                    return false;
                }
                self.prefix = new_prefix.to_string();
                self.items = self.filter_items();
                self.resolve_selection();
                if self.items.is_empty() {
                    log::debug!("[completion] set_prefix (Word): Empty items. Discarding selection key.");
                    self.selection_key = None;
                    return false;
                }
                true
            }
            TriggerMode::Path => {
                if new_prefix.len() < 2 {
                    log::debug!("[completion] set_prefix (Path): Rejected prefix '{}' (too short)", new_prefix);
                    return false;
                }
                self.prefix = new_prefix.to_string();
                self.items = self.filter_items();
                self.resolve_selection();
                true
            }
        }
    }

    // ── merge_lsp — stable selection via key ─────────────────────────
    pub fn merge_lsp(&mut self, lsp_items: Vec<crate::lsp::CompletionItem>) {
        let session = match &self.session {
            Some(s) => s,
            None => {
                log::debug!("[completion] merge_lsp: Ignored, no active session");
                return;
            }
        };

        log::debug!("[completion] merge_lsp: Merging {} LSP candidates", lsp_items.len());

        let is_member = self.is_member_access();
        let prefix_lower = self.prefix.to_lowercase();

        // ── MemberAccess stale-response guard ──
        if is_member && !prefix_lower.is_empty() {
            let current_has_matches = self
                .base_items
                .iter()
                .any(|i| i.source == CompletionSource::Lsp && i.text.to_lowercase().starts_with(&prefix_lower));

            let new_has_matches = lsp_items.iter().any(|item| {
                let text = item.get_insert_text().unwrap_or(&item.label);
                let text = text.split("(use ").next().unwrap_or(text).trim();
                text.to_lowercase().starts_with(&prefix_lower)
            });

            if current_has_matches && !new_has_matches {
                log::debug!(
                    "[completion] merge_lsp: Discarding stale LSP response in MemberAccess mode \
                 (current has matches for '{}', new response has 0)",
                    prefix_lower
                );
                return;
            }
        }

        // ── Remove or accumulate existing LSP items ──
        //
        // In MemberAccess mode we ACCUMULATE rather than replace.  The initial
        // `.` trigger response contains method completions (e.g. `to_owned()`,
        // `to_uppercase()`) that subsequent LSP requests — triggered by prefix
        // changes — frequently omit, returning only trait-level names (e.g.
        // `ToOwned`, `ToString`).  Accumulating ensures these completions
        // survive across the session's lifetime.  Per-item dedup is handled
        // below: if an item with the same lowercase text already exists its
        // metadata is upgraded instead of pushing a duplicate.
        if is_member {
            // Keep existing LSP items — accumulate
        } else {
            self.base_items.retain(|i| i.source != CompletionSource::Lsp);
        }

        // Build a set of existing LSP item keys for dedup (only needed when accumulating)
        let existing_lsp_keys: std::collections::HashSet<String> = if is_member {
            self.base_items
                .iter()
                .filter(|i| i.source == CompletionSource::Lsp)
                .map(|i| i.text.to_lowercase())
                .collect()
        } else {
            std::collections::HashSet::new()
        };

        for item in lsp_items {
            // ── Per-item: skip global keywords in MemberAccess mode ──
            if is_member {
                if matches!(item.kind, Some(14)) {
                    continue;
                }
                let label = item.label.as_str();
                #[rustfmt::skip]
            let is_rust_keyword = matches!(
                label,
                "fn" | "struct" | "impl" | "enum" | "trait" | "mod"
                    | "use" | "pub" | "const" | "static" | "type" | "let" | "mut"
                    | "ref" | "where" | "async" | "await"| "unsafe"| "extern"| "crate"
                    | "self"| "super"| "if"| "else"| "match"| "loop"| "while"
                    | "for"| "in"| "return"| "break"| "continue"| "as"| "dyn"
                    | "move"| "yield"| "true"| "false"| "become"| "box"| "do"
                    | "final"| "macro_rules"| "priv"| "typeof"| "unsized"| "virtual"| "try"
            );
                if is_rust_keyword {
                    continue;
                }
            }

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

            // ── Per-item dedup for MemberAccess accumulation ──
            let text_lower = text.to_lowercase();
            if is_member && existing_lsp_keys.contains(&text_lower) {
                // Item already exists — upgrade metadata if the new version is richer
                if let Some(existing) = self
                    .base_items
                    .iter_mut()
                    .find(|i| i.source == CompletionSource::Lsp && i.text.to_lowercase() == text_lower)
                {
                    if existing.documentation.is_none() && doc.is_some() {
                        existing.documentation = doc.clone();
                    }
                    let new_detail = item.detail.as_ref().filter(|d| !d.trim().is_empty());
                    if existing.detail.is_none() && new_detail.is_some() {
                        existing.detail = Some(format!("{} [lsp]", new_detail.unwrap()));
                    }
                    if item.data.is_some() {
                        existing.lsp_item = Some(item);
                    }
                }
                continue;
            }

            let lsp_boost = if is_member { 50.0 } else { 10.0 };
            let mut score = compute_score(&text, &prefix_lower) + lsp_boost;

            if let Some(ref sort_text) = item.sort_text {
                if let Ok(priority) = sort_text.parse::<f64>() {
                    score += (50.0 - priority.min(50.0)) * 0.1;
                }
            }
            score += match item.kind {
                Some(3) | Some(2) => 3.0,
                Some(7) | Some(22) => 2.0,
                Some(6) | Some(5) => 1.0,
                _ => 0.0,
            };

            let kind = item.kind.map(CompletionKind::from_lsp_kind).unwrap_or(CompletionKind::Text);
            let detail = match &item.detail {
                Some(d) if !d.trim().is_empty() => Some(format!("{} [lsp]", d)),
                _ => None,
            };

            self.base_items.push(CompletionEntry {
                text,
                label,
                detail,
                documentation: doc,
                kind,
                source: CompletionSource::Lsp,
                score,
                lsp_item: Some(item),
            });
        }

        self.items = self.filter_items();
        self.resolve_selection();
        log::debug!(
            "[completion] merge_lsp: Processing complete. Total base_items: {}, filtered items: {}, active key: {:?}",
            self.base_items.len(),
            self.items.len(),
            self.selection_key
        );
    }
    // ── filter_items — pure, reads self.base_items + self.prefix ─────
    fn filter_items(&self) -> Vec<CompletionEntry> {
        let prefix_lower = self.prefix.to_lowercase();

        let mut filtered: Vec<CompletionEntry> = self
            .base_items
            .iter()
            .filter(|item| {
                if prefix_lower.is_empty() {
                    return item.source == CompletionSource::Lsp
                        || self.session.as_ref().map(|s| s.mode) != Some(TriggerMode::MemberAccess);
                }
                if self.is_member_access() {
                    if item.source == CompletionSource::Lsp {
                        item.text.to_lowercase().starts_with(&prefix_lower)
                    } else {
                        fuzzy_match(&item.text.to_lowercase(), &prefix_lower)
                    }
                } else {
                    item.text.to_lowercase().starts_with(&prefix_lower) || fuzzy_match(&item.text.to_lowercase(), &prefix_lower)
                }
            })
            .cloned()
            .collect();

        // re-score non-LSP items against current prefix
        for item in &mut filtered {
            if item.source != CompletionSource::Lsp {
                item.score = compute_score(&item.text, &prefix_lower);
            }
        }

        // sort: in member-access, LSP always above non-LSP
        if self.is_member_access() {
            filtered.sort_by(|a, b| {
                let al = a.source == CompletionSource::Lsp;
                let bl = b.source == CompletionSource::Lsp;
                match (al, bl) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal),
                }
            });
        } else {
            filtered.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        }

        // ── Smart dedup — prefer LSP > Vocab > Buffer ──────
        let mut best_for_key: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

        for (i, item) in filtered.iter().enumerate() {
            let key = item.text.to_lowercase();
            let priority = match item.source {
                CompletionSource::Lsp => 3,
                CompletionSource::Vocab => 2,
                CompletionSource::BufferWords => 1,
                _ => 0,
            };
            best_for_key
                .entry(key)
                .and_modify(|existing| {
                    let existing_priority = match filtered[*existing].source {
                        CompletionSource::Lsp => 3,
                        CompletionSource::Vocab => 2,
                        CompletionSource::BufferWords => 1,
                        _ => 0,
                    };
                    if priority > existing_priority {
                        *existing = i;
                    }
                })
                .or_insert(i);
        }

        let keep_indices: std::collections::HashSet<usize> = best_for_key.values().copied().collect();

        let mut result: Vec<CompletionEntry> = filtered
            .into_iter()
            .enumerate()
            .filter(|(i, _)| keep_indices.contains(i))
            .map(|(_, item)| item)
            .collect();

        result.truncate(self.max_items);
        result
    }

    // ── cancel — clears session, closes popup ────────────────────────
    pub fn cancel(&mut self) {
        self.session = None;
        self.active = false;
        self.prefix = String::new();
        self.base_items.clear();
        self.items.clear();
        self.selected_index = 0;
        self.selection_key = None;
    }

    // ── confirm ──────────────────────────────────────────────────────
    pub fn confirm(&mut self) -> Option<(String, usize)> {
        if self.session.is_none() || self.items.is_empty() {
            return None;
        }
        let text = self.items[self.selected_index].text.clone();
        let prefix_len = self.prefix.len();
        self.cancel();
        Some((text, prefix_len))
    }

    pub fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.items.len();
            self.sync_selection_key();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = if self.selected_index == 0 {
                self.items.len() - 1
            } else {
                self.selected_index - 1
            };
            self.sync_selection_key();
        }
    }

    pub fn selected_item(&self) -> Option<&CompletionEntry> {
        self.items.get(self.selected_index)
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
            self.items = self.filter_items();
            self.resolve_selection();
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
                if detail != "[lsp]" {
                    buf.push_str("  ");
                    buf.push_str(detail);
                }
            }

            result.push(buf.clone());
        }

        result
    }
}
// ============================================================================
// Incremental Buffer Word Index
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
        // Ensure capacity
        while self.lines.len() <= line_idx {
            self.lines.push(HashSet::new());
        }

        // Replace the line's word set
        let _old_words = std::mem::replace(
            &mut self.lines[line_idx],
            if let Some(text) = line_text {
                extract_line_words(text)
            } else {
                HashSet::new()
            },
        );

        // Rebuild the global set from all lines (clears stale words)
        self.all_words.clear();
        for line_words in &self.lines {
            for w in line_words {
                self.all_words.insert(w.clone());
            }
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

// ── Word / path extraction helpers ──────────────────────────────────

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
    s.starts_with("./") || s.starts_with("../")
}

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

pub fn word_or_path_before_cursor(buffer: &Buffer, position: CursorPosition) -> (String, bool) {
    let Some(line_text) = buffer.line_text(position.line) else {
        return (String::new(), false);
    };

    // ── ASCII fast path ──────────────────────────────────────────────
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

        // "./" or "../" → path trigger (must check before dot-strip)
        if is_path_trigger(text) {
            return (text.to_string(), true);
        }

        // Bare "." — the dot itself is the trigger char handled by Case 1
        // (has_trigger). No typed prefix exists yet; return empty so callers
        // do not open a spurious MemberAccess session.
        if text == "." {
            return (String::new(), false);
        }

        // Member access: strip everything up to and including the last dot.
        // "foo.bar" → "bar"   "foo." → ""   "foo.bar.baz" → "baz"
        if let Some(dot_pos) = text.rfind('.') {
            let after_dot = &text[dot_pos + 1..];
            return (after_dot.to_string(), false);
        }

        return (text.to_string(), false);
    }

    // ── Unicode grapheme fallback ────────────────────────────────────
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

    if is_path_trigger(&text) {
        return (text, true);
    }
    if text == "." {
        return (String::new(), false);
    }
    if let Some(dot_pos) = text.rfind('.') {
        let after_dot = &text[dot_pos + 1..];
        return (after_dot.to_string(), false);
    }

    (text, false)
}

/// Returns `true` when the character immediately before `word_start` is a
/// member-access dot that is NOT part of `..` (range) or `./` / `../` (path).
///
/// # Examples
/// ```
/// // "foo.bar"  word_start = 4  → true
/// // "foo."     word_start = 4  → true
/// // "foo..bar" word_start = 5  → false  (..)
/// // "./foo"    word_start = 2  → false  (./)
/// // "."        word_start = 1  → false  (bare dot, no lhs)
/// ```
pub fn is_member_dot_before(line: &str, word_start: usize) -> bool {
    if word_start == 0 {
        return false;
    }
    let bytes = line.as_bytes();
    // Must be a dot
    if bytes.get(word_start - 1) != Some(&b'.') {
        return false;
    }
    // Exclude ".." — range operator or parent path component
    if word_start >= 2 && bytes.get(word_start - 2) == Some(&b'.') {
        return false;
    }
    // Exclude "./" — already caught by is_path_trigger but be explicit
    if bytes.get(word_start) == Some(&b'/') {
        return false;
    }
    // There must be something to the left of the dot (not a bare leading dot)
    if word_start == 1 {
        // dot is at col 0 after accounting for word — it's a bare "."
        return false;
    }
    // The char before the dot must be an identifier char (letter, digit, _)
    // so we don't trigger on operators like "=.", "-.", etc.
    matches!(bytes.get(word_start - 2), Some(&b) if b.is_ascii_alphanumeric() || b == b'_' || b == b')')
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

// ── File path collection ────────────────────────────────────────────

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

// ── Vocab collection ────────────────────────────────────────────────

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

// ── Scoring ─────────────────────────────────────────────────────────

#[inline]
pub fn compute_score(text: &str, trigger: &str) -> f64 {
    if trigger.is_empty() {
        return 0.0;
    }

    let text_lower = text.to_lowercase();
    let trigger_lower = trigger.to_lowercase();

    if text_lower.starts_with(&trigger_lower) {
        if text_lower == trigger_lower {
            if text == trigger {
                return 120.0; // Absolute top priority for exact case-sensitive match
            }
            return 90.0; // Penalize exact matches with case mismatch (e.g. Person vs person)
        }
        let coverage = trigger.len() as f64 / text.len().max(1) as f64;
        let mut score = coverage * 50.0;
        if text.starts_with(trigger) {
            score += 15.0; // Must exceed LSP Word-mode boost (+10.0) to guarantee case wins
        }
        return score;
    }

    if text_lower.contains(&trigger_lower) {
        return 5.0;
    }

    if fuzzy_match(&text_lower, &trigger_lower) {
        return 2.0;
    }

    0.0
}

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
