// src/ed/text_object.rs
//! Text object operations powered by Tree-sitter.
//!
//! Provides `daf` (delete around function), `dif` (delete inner function),
//! and related yank/change/select operations.
//!
//! Safety: only targets actual function/method nodes — NEVER containers
//! like `impl`, `class`, `mod`, or `struct`.
//!
//! Why we bypass delete_n_lines
//! ─────────────────────────────
//! delete_n_lines re-reads cursor.position.line inside the loop and also
//! calls set_yank_register (clipboard I/O) on every call.  For a text-
//! object operation the range is already known precisely, so we delete the
//! exact rope slice in one shot via delete_line_range(), which:
//!   • converts (start_line, end_line) → char indices once
//!   • removes the whole slice atomically
//!   • clamps the cursor once afterward
//!   • never touches the yank register (the caller may yank separately)
//!
//! Stale-tree invariant
//! ─────────────────────
//! reparse_tree() is called:
//!   (a) TOP — before any tree query
//!   (b) INSIDE the undo-group closure — immediately after rope mutation
//!   (c) BOTTOM — cheap defensive reparse
//!
//! Range computation
//! ─────────────────
//! raw_start / raw_end come from tree-sitter node boundaries (authoritative).
//! compute_range() absorbs at most ONE blank line on each side for Around,
//! or returns the lines strictly inside braces for Inner.

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::BufferKind;
use crate::buffer::{Buffer, CursorPosition, Language};
use crate::ed::git::GitExt;
use crate::ed::EditingExt;
use crate::ed::MovementExt;
use crate::editor::{Editor, Mode};
use crate::popup::FunctionEntry;
use crate::CommandResult;
use log::debug;

// ── Kinds & operators ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObjectKind {
    Around,
    Inner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObjectOperator {
    Delete,
    Change,
    Yank,
    Select,
}

/// Only actual functions and methods.
/// NEVER include containers (impl_item, class_definition, mod_item, struct_item).
pub const FUNCTION_KINDS: &[&str] = &[
    // Rust
    "function_item",
    // Python
    "function_definition",
    // JavaScript / TypeScript
    "function_declaration",
    "method_definition",
    "arrow_function",
    "generator_function_declaration",
    "async_function_declaration",
    "constructor_declaration",
    // Go
    "method_declaration",
    // C / C++
    "function_definition",
];

/// Walk the entire tree-sitter tree and collect every function/method node
/// in source order.  Returns `(kind_prefix, name, signature_snippet, line)`.
pub fn collect_all_functions(buffer: &Buffer) -> Vec<FunctionEntry> {
    use crate::popup::FunctionEntry;

    let tree = match buffer.tree() {
        Some(t) => t,
        None => {
            // Try to parse on the fly
            return Vec::new();
        }
    };

    let root = tree.root_node();
    let rope = &buffer.rope;
    let mut results: Vec<FunctionEntry> = Vec::new();

    // Depth-first walk collecting function nodes
    fn walk(node: tree_sitter::Node, rope: &ropey::Rope, out: &mut Vec<FunctionEntry>) {
        // Check this node
        if FUNCTION_KINDS.contains(&node.kind()) {
            let start_line = node.start_position().row;
            let line_text = if start_line < rope.len_lines() {
                let start_char = rope.line_to_char(start_line);
                let end_char = rope.line_to_char(start_line + 1).min(rope.len_chars());
                rope.slice(start_char..end_char).to_string()
            } else {
                String::new()
            };

            let trimmed = line_text.trim();

            // Extract kind prefix (everything before the name)
            let kind = extract_kind_prefix(trimmed);

            // Extract the function name via tree-sitter field, fallback to text parsing
            let name = if let Some(name_node) = node.child_by_field_name("name") {
                let start = name_node.start_byte();
                let end = name_node.end_byte();
                if start < end && end <= rope.len_bytes() {
                    rope.byte_slice(start..end).to_string()
                } else {
                    extract_function_name_from_text(trimmed, node.kind())
                }
            } else if let Some(id_node) = node.child_by_field_name("identifier") {
                // Some grammars use "identifier" instead of "name"
                let start = id_node.start_byte();
                let end = id_node.end_byte();
                if start < end && end <= rope.len_bytes() {
                    rope.byte_slice(start..end).to_string()
                } else {
                    extract_function_name_from_text(trimmed, node.kind())
                }
            } else {
                extract_function_name_from_text(trimmed, node.kind())
            };

            // Build a brief signature: first line trimmed, or first ~120 chars
            let sig = if trimmed.len() > 120 {
                format!("{}…", &trimmed[..120])
            } else {
                trimmed.to_string()
            };

            out.push(FunctionEntry {
                kind,
                name,
                signature: sig,
                line: start_line,
            });
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, rope, out);
        }
    }

    walk(root, rope, &mut results);
    results
}

/// Extract the keyword prefix from a function declaration line.
/// E.g. "pub async fn foo(" → "pub async fn"
fn extract_kind_prefix(trimmed: &str) -> String {
    let keywords = [
        "pub(crate) async fn",
        "pub(crate) fn",
        "pub async fn",
        "pub fn",
        "async fn",
        "fn",
        "public static",
        "public",
        "static",
        "private",
        "protected",
        "async function",
        "function",
        "async def",
        "def",
        "func",
        "class",
    ];
    for kw in &keywords {
        if trimmed.starts_with(kw) {
            return kw.to_string();
        }
    }
    // Fallback: first word
    trimmed
        .split_whitespace()
        .next()
        .unwrap_or("fn")
        .to_string()
}

/// Extract the function name from a declaration line (text heuristic fallback).
fn extract_function_name_from_text(trimmed: &str, node_kind: &str) -> String {
    // Strip at the first '(' or '<' to isolate the signature header.
    // e.g., "def extract_trait(bar: int) -> str:" -> "def extract_trait"
    // e.g., "pub async fn foo<T>" -> "pub async fn foo"
    let base = if let Some(idx) = trimmed.find('(') {
        &trimmed[..idx]
    } else if let Some(idx) = trimmed.find('<') {
        &trimmed[..idx]
    } else if node_kind == "arrow_function" {
        if let Some(idx) = trimmed.find("=>") {
            &trimmed[..idx]
        } else {
            trimmed
        }
    } else {
        trimmed
    };

    // The name is the last identifier-like token in the header.
    // e.g., "def extract_trait" -> "extract_trait"
    // e.g., "pub async fn foo" -> "foo"
    // e.g., "int main" -> "main"
    let name = base
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .last()
        .unwrap_or("<anonymous>");

    // Filter out keywords that aren't names
    if [
        "fn",
        "function",
        "def",
        "func",
        "class",
        "pub",
        "async",
        "static",
        "public",
        "private",
        "protected",
        "crate",
    ]
    .iter()
    .any(|&kw| kw == name)
    {
        "<anonymous>".to_string()
    } else {
        name.to_string()
    }
}

/// Extract the function name from a declaration line, falling back to
/// tree-sitter's child named "name" or "identifier".
fn extract_function_name(trimmed: &str, node_kind: &str) -> String {
    // Try to find the name token after the keyword(s)
    let after_kw = if node_kind == "function_definition" || node_kind == "def" {
        // Python: "def foo(" → name is after "def "
        if let Some(rest) = trimmed.strip_prefix("async def ") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("def ") {
            rest
        } else {
            trimmed
        }
    } else if node_kind == "arrow_function" {
        // JS: "const foo = (" or "foo =>"
        if let Some(idx) = trimmed.find("=>") {
            &trimmed[..idx]
        } else {
            trimmed
        }
    } else {
        // Rust/Go/C/Java: find the name before '(' or '<'
        if let Some(idx) = trimmed.find('(') {
            &trimmed[..idx]
        } else if let Some(idx) = trimmed.find('<') {
            &trimmed[..idx]
        } else {
            trimmed
        }
    };

    // The name is the last identifier-like token before '(' or end
    let name = after_kw
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .last()
        .unwrap_or("<anonymous>");

    // Filter out keywords that aren't names
    if [
        "fn",
        "function",
        "def",
        "func",
        "class",
        "pub",
        "async",
        "static",
        "public",
        "private",
        "protected",
        "crate",
    ]
    .iter()
    .any(|&kw| kw == name)
    {
        "<anonymous>".to_string()
    } else {
        name.to_string()
    }
}
// ── Extension trait ─────────────────────────────────────────────────

pub trait TextObjectExt {
    fn operate_on_function(
        &mut self,
        kind: TextObjectKind,
        operator: TextObjectOperator,
    ) -> CommandResult;
    fn find_function_lines(&self) -> Option<(usize, usize)>;
}

impl TextObjectExt for Editor {
    fn operate_on_function(
        &mut self,
        kind: TextObjectKind,
        operator: TextObjectOperator,
    ) -> CommandResult {
        // ── Extract buffer_id at the top level so it's available everywhere ──
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // ── (a) Fresh tree before any query ────────────────────────────
        {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                // Ensure tree-sitter is initialized and parse the current rope
                if buffer.tree().is_none() {
                    buffer.init_tree_sitter();
                } else {
                    buffer.reparse_tree();
                }

                // Verify tree is not stale by checking if its span exceeds the current buffer length
                if let Some(tree) = buffer.tree() {
                    let root = tree.root_node();
                    let tree_end_line = root.end_position().row;
                    let buffer_end_line = buffer.line_count().saturating_sub(1);
                    if tree_end_line > buffer_end_line + 1 {
                        // Force a full reparse from scratch
                        buffer.init_tree_sitter();
                    }
                }
            }
        }

        // ── Find the initial range from tree-sitter ──
        let (raw_start, raw_end) = match self.find_function_lines() {
            Some(r) => r,
            None => return CommandResult::Error("No function found around cursor".into()),
        };

        // ── Validate: tree-sitter error recovery can return partial ranges ──
        let (raw_start, raw_end) = {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };
            let validated = ensure_range_includes_braces(buffer, raw_start, raw_end);
            let _ = validated != (raw_start, raw_end);
            validated
        };

        let (start_line, end_line, open_brace_line) = {
            let buffer = match self.buffers.get(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };
            compute_range(buffer, raw_start, raw_end, kind)
        };

        if start_line > end_line {
            return CommandResult::Error("Empty function body".into());
        }

        let line_count = end_line - start_line + 1;

        let result = match operator {
            // ── Delete ─────────────────────────────────────────────────
            TextObjectOperator::Delete => self.with_undo_group(|s| {
                // Yank before deletion so the register is correct.
                s.yank_line_range(start_line, end_line);

                // Delete the exact range in one rope operation.
                delete_line_range(s, buffer_id, start_line, end_line);

                // ── (b) Reparse immediately after rope mutation ─────────
                if let Some(buf) = s.buffers.get_mut(&buffer_id) {
                    buf.reparse_tree();
                }

                CommandResult::ContentChanged
            }),

            // ── Change ─────────────────────────────────────────────────
            TextObjectOperator::Change => self.with_undo_group(|s| {
                s.yank_line_range(start_line, end_line);

                delete_line_range(s, buffer_id, start_line, end_line);

                // ── (b) Reparse after the delete half of change ─────────
                if let Some(buf) = s.buffers.get_mut(&buffer_id) {
                    buf.reparse_tree();
                }

                if kind == TextObjectKind::Inner {
                    // Place cursor at the opening-brace line and go to end of line
                    if let Some(w) = s.windows.active_window_mut() {
                        // After deletion open_brace_line may have shifted if
                        // start_line < open_brace_line.  Compute the new row.
                        let shifted = if start_line <= open_brace_line {
                            open_brace_line.saturating_sub(line_count)
                        } else {
                            open_brace_line
                        };
                        w.cursor.position = CursorPosition::new(shifted, 0);
                        w.cursor.desired_col = None;
                    }
                    s.move_line_end();
                } else {
                    s.open_line_below();
                }
                s.mode = Mode::Insert;
                CommandResult::ModeChanged(Mode::Insert)
            }),

            // ── Yank ───────────────────────────────────────────────────
            TextObjectOperator::Yank => {
                self.yank_line_range(start_line, end_line);
                CommandResult::Message(format!("{} lines yanked", line_count))
            }

            // ── Select ─────────────────────────────────────────────────
            TextObjectOperator::Select => {
                if let Some(w) = self.windows.active_window_mut() {
                    w.selection_anchor = Some(CursorPosition::new(start_line, 0));
                    w.cursor.position = CursorPosition::new(end_line, 0);
                    w.cursor.desired_col = None;
                }
                self.mode = Mode::VisualLine;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::VisualLine)
            }
        };

        // ── (c) Defensive reparse at the operation boundary ────────────
        if matches!(
            result,
            CommandResult::ContentChanged | CommandResult::ModeChanged(_)
        ) {
            if let Some(buf) = self.buffers.get_mut(&buffer_id) {
                buf.reparse_tree();
            }
        }

        result
    }

    // ────────────────────────────────────────────────────────────────
    // find_function_lines  (Tree-sitter powered)
    // ────────────────────────────────────────────────────────────────
    fn find_function_lines(&self) -> Option<(usize, usize)> {
        let window = self.windows.active_window()?;
        let buffer_id = window.buffer_id;
        let buffer = self.buffers.get(&buffer_id)?;
        let cursor_pos = window.cursor.position;
        let tree = buffer.tree()?;
        let root = tree.root_node();

        // ── Strategy 1: Try exact cursor position ──
        if let Some(byte_off) = cursor_pos_to_byte(buffer, cursor_pos) {
            if let Some(r) = try_find_function_at(&root, byte_off, buffer) {
                return Some(r);
            }
        }

        // ── Strategy 2: Cursor on whitespace / blank line ──
        let line_text = buffer.line_text(cursor_pos.line).unwrap_or_default();
        let trimmed = line_text.trim();
        if trimmed.is_empty() {
            // Blank line — try first non-blank below
            let max = buffer.line_count().saturating_sub(1);
            for scan_line in (cursor_pos.line + 1)..=max {
                if let Some(text) = buffer.line_text(scan_line) {
                    if !text.trim().is_empty() {
                        let pos = CursorPosition::new(scan_line, 0);
                        if let Some(byte_off) = cursor_pos_to_byte(buffer, pos) {
                            if let Some(r) = try_find_function_at(&root, byte_off, buffer) {
                                if r.0 <= scan_line {
                                    return Some(r);
                                }
                            }
                        }
                        break;
                    }
                }
            }
            // Then first non-blank above
            for scan_line in (0..cursor_pos.line).rev() {
                if let Some(text) = buffer.line_text(scan_line) {
                    if !text.trim().is_empty() {
                        let pos = CursorPosition::new(scan_line, 0);
                        if let Some(byte_off) = cursor_pos_to_byte(buffer, pos) {
                            if let Some(r) = try_find_function_at(&root, byte_off, buffer) {
                                if cursor_pos.line <= r.1 + 1 {
                                    return Some(r);
                                }
                            }
                        }
                        break;
                    }
                }
            }
        } else {
            // Non-blank line but cursor is in leading whitespace
            let graphemes: Vec<_> = line_text.graphemes(true).collect();
            let mut col = cursor_pos.col;
            while col < graphemes.len() {
                if !graphemes[col].trim().is_empty() {
                    break;
                }
                col += 1;
            }
            if col != cursor_pos.col && col < graphemes.len() {
                let pos = CursorPosition::new(cursor_pos.line, col);
                if let Some(byte_off) = cursor_pos_to_byte(buffer, pos) {
                    if let Some(r) = try_find_function_at(&root, byte_off, buffer) {
                        return Some(r);
                    }
                }
            }
        }

        None
    }
}

// ── Editor helpers ──────────────────────────────────────────────────

impl Editor {
    pub fn yank_line_range(&mut self, start_line: usize, end_line: usize) {
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return,
        };
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            let mut text = String::new();
            for i in start_line..=end_line {
                if let Some(line) = buffer.line_text(i) {
                    text.push_str(line.trim_end_matches('\n'));
                    text.push('\n');
                }
            }
            self.yank_register = text;
        }
    }
    /// Return the name of the function the cursor is currently inside, if any.
    ///
    /// Walks up the tree-sitter tree from the cursor position looking for an
    /// enclosing function/method node.  Returns `None` if no tree-sitter tree
    /// is available or the cursor is not inside a function.
    pub fn current_function_name(&self) -> Option<String> {
        let window = self.windows.active_window()?;
        let buffer_id = window.buffer_id;
        let buffer = self.buffers.get(&buffer_id)?;
        let cursor_pos = window.cursor.position;

        let tree = buffer.tree()?;
        let root = tree.root_node();

        let byte_off = cursor_pos_to_byte(buffer, cursor_pos)?;

        let mut node = match root.descendant_for_byte_range(byte_off, byte_off) {
            Some(n) => n,
            None => return None,
        };

        loop {
            if FUNCTION_KINDS.contains(&node.kind()) {
                let name = extract_node_name(buffer, &node);
                return Some(name);
            }
            node = match node.parent() {
                Some(p) => p,
                None => return None,
            };
        }
    }

    /// Update the cached current function name. Called from tick().
    pub fn update_function_name_cache(&mut self) {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => {
                debug!("[fn_name] no active window");
                self.current_function_name = None;
                self.fn_name_cache_key = None;
                return;
            }
        };
        let buffer_id = window.buffer_id;
        let cursor_line = window.cursor.position.line;
        let cache_key = (buffer_id, cursor_line);

        // Skip if nothing changed since last computation
        if !self.fn_name_needs_update && self.fn_name_cache_key == Some(cache_key) {
            return;
        }

        debug!(
            "[fn_name] recomputing: needs_update={}, old_key={:?}, new_key={:?}",
            self.fn_name_needs_update, self.fn_name_cache_key, cache_key
        );

        self.fn_name_needs_update = false;
        self.fn_name_cache_key = Some(cache_key);

        // Skip special buffer types and buffers without language support
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            debug!(
                "[fn_name] buffer kind={:?}, language={:?}, has_file={:?}",
                buffer.kind,
                buffer.language,
                buffer.file_path.as_ref().map(|p| p.display().to_string()),
            );
            if buffer.kind != BufferKind::Normal || buffer.language.is_none() {
                debug!("[fn_name] skipping: non-Normal buffer or no language");
                self.current_function_name = None;
                return;
            }
        } else {
            debug!("[fn_name] buffer not found for id={:?}", buffer_id);
            self.current_function_name = None;
            return;
        }

        // Ensure tree-sitter is initialized and up-to-date
        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            let had_tree_before = buffer.tree().is_some();
            if buffer.tree().is_none() {
                debug!("[fn_name] initializing tree-sitter");
                buffer.init_tree_sitter();
            } else {
                buffer.reparse_tree();
            }
            let has_tree_after = buffer.tree().is_some();
            debug!(
                "[fn_name] tree-sitter: had_tree={}, has_tree_now={}",
                had_tree_before, has_tree_after
            );
        }

        // Compute the function name
        let new_name = self.compute_current_function_name();

        debug!("[fn_name] computed={:?}", new_name);

        // Only mark powerline dirty if name actually changed
        if self.current_function_name != new_name {
            debug!(
                "[fn_name] changed: old={:?}, new={:?}",
                self.current_function_name, new_name
            );
            self.current_function_name = new_name;
            self.dirty.status_powerline = true;
        }
    }

    /// Compute the current function name from tree-sitter.
    fn compute_current_function_name(&self) -> Option<String> {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return None,
        };
        let buffer_id = window.buffer_id;
        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return None,
        };
        let cursor_pos = window.cursor.position;

        let tree = match buffer.tree() {
            Some(t) => t,
            None => {
                debug!("[fn_name] compute: no tree for buffer {:?}", buffer_id);
                return None;
            }
        };
        let root = tree.root_node();

        debug!(
            "[fn_name] compute: cursor=({}, {}), root_kind={}, root_bytes=({},{})",
            cursor_pos.line,
            cursor_pos.col,
            root.kind(),
            root.start_byte(),
            root.end_byte()
        );

        let byte_off = match cursor_pos_to_byte(buffer, cursor_pos) {
            Some(b) => b,
            None => {
                debug!(
                    "[fn_name] compute: cursor_pos_to_byte returned None for ({}, {})",
                    cursor_pos.line, cursor_pos.col
                );
                return None;
            }
        };

        debug!("[fn_name] compute: byte_off={}", byte_off);

        let mut node = match root.descendant_for_byte_range(byte_off, byte_off) {
            Some(n) => {
                debug!(
                    "[fn_name] compute: descendant kind={}, bytes=({},{})",
                    n.kind(),
                    n.start_byte(),
                    n.end_byte()
                );
                n
            }
            None => {
                debug!("[fn_name] compute: no descendant at byte {}", byte_off);
                return None;
            }
        };

        let mut depth = 0;
        loop {
            let is_fn = FUNCTION_KINDS.contains(&node.kind());
            debug!(
                "[fn_name] compute: walk depth={} kind={} is_function={}",
                depth,
                node.kind(),
                is_fn
            );

            if is_fn {
                let name = extract_node_name(buffer, &node);
                debug!("[fn_name] compute: found function, name={:?}", name);
                return Some(name);
            }
            match node.parent() {
                Some(p) => {
                    depth += 1;
                    node = p;
                }
                None => {
                    debug!("[fn_name] compute: reached root without finding function");
                    return None;
                }
            }
        }
    }
}

// ── Pure helper functions ───────────────────────────────────────────
/// Validate that a tree-sitter function range includes braces for brace-based
/// languages.  Tree-sitter error recovery can sometimes return a partial range
/// (e.g. just the body) that excludes the `{` and `}` lines.  This function
/// detects that and expands the range so `daf`/`dif` always delete the
/// complete function.
fn ensure_range_includes_braces(
    buffer: &Buffer,
    raw_start: usize,
    raw_end: usize,
) -> (usize, usize) {
    let language = buffer.language.unwrap_or(Language::PlainText);
    if matches!(language, Language::Python) {
        return (raw_start, raw_end);
    }

    let (open, close) = find_brace_lines(buffer, raw_start, raw_end);
    let mut start = raw_start;
    let mut end = raw_end;

    if open.is_none() {
        // Scan backward (up to 20 lines) for the opening brace.
        let limit = raw_start.saturating_sub(20);
        for line in (limit..raw_start).rev() {
            if let Some(text) = buffer.line_text(line) {
                if text.contains('{') {
                    start = line;
                    break;
                }
                // Don't cross into another function declaration
                let trimmed = text.trim();
                if trimmed.starts_with("fn ")
                    || trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("async fn ")
                    || trimmed.starts_with("pub async fn ")
                    || trimmed.starts_with("function ")
                    || trimmed.starts_with("def ")
                    || trimmed.starts_with("func ")
                {
                    break;
                }
            }
        }
        if start == raw_start {}
    }

    if close.is_none() {
        // Scan forward (up to 20 lines) for the closing brace.
        let max = buffer.line_count();
        let limit = (raw_end + 20).min(max);
        for line in (raw_end + 1)..limit {
            if let Some(text) = buffer.line_text(line) {
                if text.contains('}') {
                    end = line;
                    break;
                }
            }
        }
        if end == raw_end {}
    }

    (start, end)
}

// ── Atomic range deletion ────────────────────────────────────────────
//
// Converts (start_line, end_line) → rope char indices once and removes
// the whole slice in a single operation.  Does NOT use delete_n_lines,
// which re-reads the cursor inside a loop and has clipboard side-effects.
//
// After deletion the cursor is clamped to the new buffer end and left at
// (start_line, 0) — or the last line if start_line is now past EOF.

fn delete_line_range(
    editor: &mut Editor,
    buffer_id: crate::buffer::BufferId,
    start_line: usize,
    end_line: usize,
) {
    let buffer = match editor.buffers.get_mut(&buffer_id) {
        Some(b) => b,
        None => return,
    };

    let total_lines = buffer.rope.len_lines();
    if start_line >= total_lines {
        return;
    }
    let clamped_end = end_line.min(total_lines.saturating_sub(1));

    // Log exactly what is being deleted
    let deleted_text: Vec<String> = (start_line..=clamped_end)
        .filter_map(|l| buffer.line_text(l).map(|s| s.trim_end().to_string()))
        .collect();

    let start_char = buffer.rope.line_to_char(start_line);
    let end_char = if clamped_end + 1 < total_lines {
        buffer.rope.line_to_char(clamped_end + 1)
    } else {
        buffer.rope.len_chars()
    };

    if start_char >= end_char {
        return;
    }

    buffer.rope.remove(start_char..end_char);
    buffer.dirty = true;

    if buffer.rope.len_chars() == 0 {
        buffer.rope.insert(0, "\n");
    }

    let new_total = buffer.rope.len_lines().max(1);
    if let Some(window) = editor.windows.active_window_mut() {
        let new_line = start_line.min(new_total.saturating_sub(1));
        window.cursor.position = CursorPosition::new(new_line, 0);
        window.cursor.desired_col = None;
    }

    editor.invalidate_git_gutter();
}
// ── Range computation ────────────────────────────────────────────────
//
// Around: absorb at most ONE blank line before raw_start and ONE after
//         raw_end.  Never more — prevents cascading deletions on repeated daf.
//
// Inner:  lines strictly inside the braces, or after the colon (Python).
// ── Range computation ────────────────────────────────────────────────
//
// Around: absorb at most ONE blank line before raw_start and ONE after
//         raw_end.  Never more — prevents cascading deletions on repeated daf.
//
// Inner:  lines strictly inside the braces, or after the colon (Python).
fn compute_range(
    buffer: &Buffer,
    raw_start: usize,
    raw_end: usize,
    kind: TextObjectKind,
) -> (usize, usize, usize) {
    let (open, close) = find_brace_lines(buffer, raw_start, raw_end);

    match kind {
        TextObjectKind::Around => {
            // At most one preceding blank line.
            let start = if raw_start > 0 {
                match buffer.line_text(raw_start - 1) {
                    Some(t) if t.trim().is_empty() => raw_start - 1,
                    _ => raw_start,
                }
            } else {
                raw_start
            };

            // At most one trailing blank line.
            let max = buffer.line_count().saturating_sub(1);
            let end = if raw_end < max {
                match buffer.line_text(raw_end + 1) {
                    Some(t) if t.trim().is_empty() => raw_end + 1,
                    _ => raw_end,
                }
            } else {
                raw_end
            };

            let open_brace = open.unwrap_or(raw_start);
            (start, end, open_brace)
        }

        TextObjectKind::Inner => match (open, close) {
            (Some(o), Some(c)) if c > o + 1 => (o + 1, c - 1, o),
            _ => {
                let language = buffer.language.unwrap_or(Language::PlainText);
                if matches!(language, Language::Python) {
                    if let Some(colon_line) = find_char_line(buffer, raw_start, raw_end, ':') {
                        if colon_line < raw_end {
                            return (colon_line + 1, raw_end, colon_line);
                        }
                    }
                }
                let open_brace = open.unwrap_or(raw_start);
                (raw_start, raw_end, open_brace)
            }
        },
    }
}

// ── Pure helper functions ───────────────────────────────────────────

/// First line with `{`, last line with `}` in start..=end.
fn find_brace_lines(buffer: &Buffer, start: usize, end: usize) -> (Option<usize>, Option<usize>) {
    let mut open = None;
    let mut close = None;
    for line in start..=end {
        if let Some(text) = buffer.line_text(line) {
            // FIX: Removed `open.is_none()` check to correctly identify the brace line
            // even if tree-sitter's raw_start began exactly on the `{`.
            if open.is_none() && text.contains('{') {
                open = Some(line);
            }
            if text.contains('}') {
                close = Some(line);
            }
        }
    }
    (open, close)
}

fn try_find_function_at(
    root: &tree_sitter::Node,
    byte_offset: usize,
    buffer: &Buffer,
) -> Option<(usize, usize)> {
    let mut node = root.descendant_for_byte_range(byte_offset, byte_offset)?;
    loop {
        let kind = node.kind();
        if FUNCTION_KINDS.contains(&kind) {
            let start_line = node.start_position().row;
            let end_line = node.end_position().row;

            // Validate that the buffer text at start_line actually starts with a function keyword.
            // This protects against stale tree-sitter trees returning invalid line ranges.
            let is_valid_function = buffer.line_text(start_line).is_some_and(|text| {
                let trimmed = text.trim();
                trimmed.starts_with("fn ")
                    || trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("pub(crate) fn ")
                    || trimmed.starts_with("async fn ")
                    || trimmed.starts_with("pub async fn ")
                    || trimmed.starts_with("function ")
                    || trimmed.starts_with("def ")
                    || trimmed.starts_with("func ")
                    || trimmed.starts_with("class ")
            });

            if !is_valid_function {
                return None; // Do not use this stale node
            }

            let max_line = buffer.line_count().saturating_sub(1);
            if start_line <= max_line && end_line <= max_line && start_line <= end_line {
                return Some((start_line, end_line));
            }
            return None;
        }
        node = match node.parent() {
            Some(p) => p,
            None => {
                return None;
            }
        };
    }
}

fn find_char_line(buffer: &Buffer, start: usize, end: usize, ch: char) -> Option<usize> {
    for line in start..=end {
        if let Some(text) = buffer.line_text(line) {
            if text.contains(ch) {
                return Some(line);
            }
        }
    }
    None
}

fn cursor_pos_to_byte(buffer: &Buffer, pos: CursorPosition) -> Option<usize> {
    let rope = &buffer.rope;
    if pos.line >= rope.len_lines() {
        return None;
    }
    let line_start_char = rope.line_to_char(pos.line);
    let line_text = buffer.line_text(pos.line)?;
    let mut char_offset = 0;
    for (g_idx, grapheme) in line_text.graphemes(true).enumerate() {
        if g_idx >= pos.col {
            break;
        }
        char_offset += grapheme.chars().count();
    }
    let cursor_char = line_start_char + char_offset;
    if cursor_char >= rope.len_chars() {
        return Some(rope.len_bytes());
    }
    Some(rope.char_to_byte(cursor_char))
}

/// Extract the declared name of a tree-sitter function node.
///
/// Tries tree-sitter's `name` field first, then `identifier`, and
/// finally falls back to text-based heuristics.
fn extract_node_name(buffer: &Buffer, node: &tree_sitter::Node) -> String {
    // Try the "name" field (Rust `fn`, Go `func`, C/C++ `function_definition`)
    if let Some(name_node) = node.child_by_field_name("name") {
        let start = name_node.start_byte();
        let end = name_node.end_byte();
        if start < end && end <= buffer.rope.len_bytes() {
            let name = buffer.rope.byte_slice(start..end).to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }

    // Try the "identifier" field (some grammars use this instead)
    if let Some(id_node) = node.child_by_field_name("identifier") {
        let start = id_node.start_byte();
        let end = id_node.end_byte();
        if start < end && end <= buffer.rope.len_bytes() {
            let name = buffer.rope.byte_slice(start..end).to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }

    // Text-based fallback
    let line_text = buffer
        .line_text(node.start_position().row)
        .unwrap_or_default()
        .trim()
        .to_string();
    extract_function_name_from_text(&line_text, node.kind())
}
