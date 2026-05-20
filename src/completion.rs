// completion.rs — Session-based completion engine
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
        }
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
    }

    // ── set_prefix — called every keystroke while active ─────────────
    // Returns false if the prefix is incompatible (caller should cancel).
    pub fn set_prefix(&mut self, new_prefix: &str) -> bool {
        let Some(session) = &self.session else {
            return false;
        };

        match session.mode {
            TriggerMode::MemberAccess => {
                self.prefix = new_prefix.to_string();
                // buffer words are noise in member-access — keep only LSP
                self.base_items.retain(|i| i.source == CompletionSource::Lsp);
                self.items = self.filter_items();
                true
            }
            TriggerMode::Word => {
                if new_prefix.len() < self.trigger_len {
                    return false;
                }
                self.prefix = new_prefix.to_string();
                self.items = self.filter_items();
                if self.items.is_empty() {
                    return false;
                }
                true
            }
            TriggerMode::Path => {
                if new_prefix.len() < 2 {
                    return false;
                }
                self.prefix = new_prefix.to_string();
                self.items = self.filter_items();
                true
            }
        }
    }

    // ── merge_lsp — called when LSP response arrives ─────────────────
    pub fn merge_lsp(&mut self, lsp_items: Vec<crate::lsp::CompletionItem>) {
        if self.session.is_none() {
            return;
        }

        let is_member = self.is_member_access();
        let prefix_lower = self.prefix.to_lowercase();

        // remove stale LSP items from a previous request
        self.base_items.retain(|i| i.source != CompletionSource::Lsp);

        for item in lsp_items {
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

            let lsp_boost = if is_member { 50.0 } else { 10.0 };
            let mut score = compute_score(&text, &prefix_lower) + lsp_boost;

            if let Some(ref sort_text) = item.sort_text {
                if let Ok(priority) = sort_text.parse::<f64>() {
                    score += (50.0 - priority.min(50.0)) * 0.1;
                }
            }

            score += match item.kind {
                Some(3) | Some(2) => 3.0,  // Function / Method
                Some(7) | Some(22) => 2.0, // Class / Struct
                Some(6) | Some(5) => 1.0,  // Variable / Field
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

        // dedup by lowercase text
        let mut seen = HashSet::new();
        filtered.retain(|item| seen.insert(item.text.to_lowercase()));
        filtered.truncate(self.max_items);
        filtered
    }

    // ── cancel — clears session, closes popup ────────────────────────
    pub fn cancel(&mut self) {
        self.session = None;
        self.active = false;
        self.prefix = String::new();
        self.base_items.clear();
        self.items.clear();
        self.selected_index = 0;
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
            let saved = self.selected_index;
            self.items = self.filter_items();
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
    if let Some(line_text) = buffer.line_text(position.line) {
        // ASCII fast path
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

/// Score tiers:
///   100.0 — exact match
///    50.0 — prefix match (scaled by coverage)
///     5.0 — substring match
///     2.0 — fuzzy match
///     0.0 — no match
///
/// LSP items receive an additional boost in the caller:
///   +10.0 normal context, +50.0 member-access context
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

/// Simple fuzzy match: all needle chars appear in haystack in order.
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
