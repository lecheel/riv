// src/ed/goto_def.rs
//! Goto definition via tree-sitter + ctags fallback.
//!
//! Implements `gd` using a three-tier strategy:
//!   1. LSP goto-definition (if connected) — most accurate, async
//!   2. Tree-sitter semantic search — good for local/open-buffer analysis
//!   3. Ctags search — good for cross-file when LSP is unavailable
//!
//! All successful jumps push to the unified tag/jump stack so that
//! `` `` `` / `:pop` can return the user to the previous position.

use crate::buffer::{Buffer, BufferKind, Language};
use crate::ed::buffer_ops::BufferOpsExt;
use crate::ed::movement::MovementExt;
use crate::ed::text_object::TextObjectExt;
use crate::editor::{CommandResult, Editor};

pub trait GotoDefExt {
    /// Jump to the definition of the identifier under the cursor.
    /// Uses tree-sitter first, then falls back to ctags.
    fn goto_definition(&mut self) -> CommandResult;
}

impl GotoDefExt for Editor {
    fn goto_definition(&mut self) -> CommandResult {
        let identifier = self.word_under_cursor_in_current_buffer();
        if identifier.is_empty() {
            return CommandResult::Error("No identifier under cursor".into());
        }

        let current_buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::NoOp,
        };

        // ── Tier 1: Tree-sitter in current buffer ──
        {
            if let Some(buffer) = self.buffers.get_mut(&current_buffer_id) {
                ensure_tree(buffer);
            }
        }

        {
            let found = self
                .buffers
                .get(&current_buffer_id)
                .and_then(|buffer| buffer.tree().and_then(|tree| find_definition_in_tree(buffer, tree, &identifier)));

            if let Some((line, col)) = found {
                self.push_jump_position();
                self.move_to_position(line, col);
                self.ensure_cursor_visible_all();
                return CommandResult::Message(format!("Goto definition: line {}", line + 1));
            }
        }

        // ── Tier 2: Tree-sitter in other open buffers ──
        let other_ids: Vec<_> = self
            .buffers
            .iter()
            .filter(|b| b.id != current_buffer_id && b.kind == BufferKind::Normal)
            .map(|b| b.id)
            .collect();

        for buf_id in other_ids {
            if let Some(buffer) = self.buffers.get_mut(&buf_id) {
                ensure_tree(buffer);
            }

            let found = self
                .buffers
                .get(&buf_id)
                .and_then(|buffer| buffer.tree().and_then(|tree| find_definition_in_tree(buffer, tree, &identifier)));

            if let Some((line, col)) = found {
                self.push_jump_position();
                if let Some(window) = self.windows.active_window_mut() {
                    window.set_buffer(buf_id);
                }
                self.move_to_position(line, col);
                self.ensure_cursor_visible_all();
                let name = self.buffers.get(&buf_id).map(|b| b.display_name()).unwrap_or_else(|| "?".into());
                return CommandResult::Message(format!("Goto definition in {}: line {}", name, line + 1));
            }
        }

        // ── Tier 2b: Vim-like fallback (first word in scope) ──
        if let Some((line, col)) = self.vim_like_gd(&identifier, current_buffer_id) {
            self.push_jump_position();
            self.move_to_position(line, col);
            self.ensure_cursor_visible_all();
            return CommandResult::Message(format!("Goto first occurrence: line {}", line + 1));
        }

        // ── Tier 3: Ctags fallback ──
        // Tree-sitter found nothing — try ctags which can search the
        // entire project, not just open buffers.
        crate::ed::tag::tag_under_cursor(self);

        // tag_under_cursor sets status/error internally.
        // If it found a match it already navigated; if not it set an error.
        // Return ViewChanged since the jump or error is already handled.
        CommandResult::ViewChanged
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn ensure_tree(buffer: &mut Buffer) {
    if buffer.tree().is_none() {
        buffer.init_tree_sitter();
    } else {
        buffer.reparse_tree();
    }
}

fn definition_kinds(language: Language) -> &'static [&'static str] {
    match language {
        Language::Rust => &[
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "type_item",
            "const_item",
            "static_item",
            "mod_item",
            "macro_definition",
            "let_declaration",
        ],
        Language::Python => &["function_definition", "class_definition"],
        Language::JavaScript | Language::TypeScript => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "lexical_declaration",
            "variable_declarator",
        ],
        _ => &[],
    }
}

fn find_definition_in_tree(buffer: &Buffer, tree: &tree_sitter::Tree, target_name: &str) -> Option<(usize, usize)> {
    let language = buffer.language.unwrap_or(Language::PlainText);
    let def_kinds = definition_kinds(language);
    if def_kinds.is_empty() {
        return None;
    }

    let source = buffer.rope.to_string();
    let root = tree.root_node();
    let mut matches: Vec<(usize, usize)> = Vec::new();

    let mut cursor = root.walk();

    loop {
        let node = cursor.node();

        if def_kinds.contains(&node.kind()) {
            if let Some(name) = extract_definition_name(&node, &source, language) {
                if name == target_name {
                    let line = node.start_position().row;
                    let col = find_name_column(&node, &source);
                    matches.push((line, col));
                }
            }
        }

        if cursor.goto_first_child() {
            continue;
        }

        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                drop(cursor);
                if !matches.is_empty() {
                    matches.sort_by_key(|m| m.0);
                    return Some(matches[0]);
                }
                return None;
            }
        }
    }
}

fn find_name_column(node: &tree_sitter::Node, source: &str) -> usize {
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.start_position().column;
    }
    if node.kind() == "impl_item" {
        if let Some(type_node) = node.child_by_field_name("type") {
            return type_node.start_position().column;
        }
    }
    if node.kind() == "lexical_declaration" {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                if child.kind() == "variable_declarator" {
                    return find_name_column(&child, source);
                }
            }
        }
    }
    if node.kind() == "variable_declarator" {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                let kind = child.kind();
                if kind == "identifier" || kind == "property_identifier" {
                    return child.start_position().column;
                }
            }
        }
    }
    if node.kind() == "type_declaration" {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                if child.kind() == "type_spec" {
                    return find_name_column(&child, source);
                }
            }
        }
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            let kind = child.kind();
            if kind == "identifier" || kind == "type_identifier" || kind == "field_identifier" || kind == "property_identifier" {
                return child.start_position().column;
            }
        }
    }
    node.start_position().column
}

fn extract_definition_name(node: &tree_sitter::Node, source: &str, language: Language) -> Option<String> {
    if language == Language::Rust && node.kind() == "let_declaration" {
        return extract_let_binding_name(node, source);
    }
    if node.kind() == "lexical_declaration" {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                if child.kind() == "variable_declarator" {
                    return extract_definition_name(&child, source, language);
                }
            }
        }
        return None;
    }
    if node.kind() == "variable_declarator" {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                let kind = child.kind();
                if kind == "identifier" || kind == "property_identifier" {
                    return child.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
                }
            }
        }
        return None;
    }
    if node.kind() == "type_declaration" {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                if child.kind() == "type_spec" {
                    return extract_definition_name(&child, source, language);
                }
            }
        }
        return None;
    }
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
    }
    if node.kind() == "impl_item" {
        if let Some(type_node) = node.child_by_field_name("type") {
            return type_node.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
        }
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            let kind = child.kind();
            if kind == "identifier" || kind == "type_identifier" || kind == "field_identifier" || kind == "property_identifier" {
                return child.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
            }
        }
    }
    None
}

fn extract_let_binding_name(node: &tree_sitter::Node, source: &str) -> Option<String> {
    if let Some(pattern) = node.child_by_field_name("pattern") {
        if pattern.kind() == "identifier" {
            return pattern.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
        }
        for i in 0..pattern.named_child_count() {
            if let Some(child) = pattern.named_child(i) {
                if child.kind() == "identifier" {
                    return child.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
                }
            }
        }
    }
    None
}

impl Editor {
    fn vim_like_gd(&self, word: &str, buffer_id: crate::buffer::BufferId) -> Option<(usize, usize)> {
        let buffer = self.buffers.get(&buffer_id)?;
        let scope_start = self.find_function_lines().map(|(s, _)| s).unwrap_or(0);

        for line_idx in scope_start..buffer.line_count() {
            if let Some(line_text) = buffer.line_text(line_idx) {
                if let Some(col) = find_word_in_line(&line_text, word) {
                    return Some((line_idx, col));
                }
            }
        }
        None
    }

    pub fn push_jump_position(&mut self) {
        let current_path = self.current_buffer().and_then(|b| b.file_path.clone());
        let cursor = self
            .windows
            .active_window()
            .map(|w| w.cursor.position)
            .unwrap_or(crate::buffer::CursorPosition { line: 0, col: 0 });

        if let Some(path) = current_path {
            self.search.tag_manager.push_stack(path, cursor.line, cursor.col);
        }

        if let Some(window) = self.windows.active_window() {
            self.search.last_jump_mark = Some((window.buffer_id, cursor));
        }
    }
}

fn find_word_in_line(line: &str, word: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let word_bytes = word.as_bytes();
    let word_len = word_bytes.len();

    if word_len == 0 || bytes.len() < word_len {
        return None;
    }

    let mut start = 0;
    while start + word_len <= bytes.len() {
        if &bytes[start..start + word_len] == word_bytes {
            let before_ok = start == 0 || !is_word_char(bytes[start - 1]);
            let after_ok = start + word_len >= bytes.len() || !is_word_char(bytes[start + word_len]);

            if before_ok && after_ok {
                let col = line[..start].chars().count();
                return Some(col);
            }
        }
        start += 1;
    }
    None
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
