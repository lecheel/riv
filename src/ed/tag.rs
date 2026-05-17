// src/ed/tag.rs
//! Ctags integration: jumping to definitions, tag stack navigation, and tag search.

use crate::ed::buffer_ops::BufferOpsExt;
use crate::ed::file_ops::FileOpsExt;
use crate::ed::movement::MovementExt;
use crate::editor::{CommandResult, Editor};
use crate::popup::TagListPopup;
use crate::tags::TagEntry;
use std::path::PathBuf;

/// Jump to the definition of the word under the cursor using ctags.
/// If there are multiple matches, shows a float popup and jumps to the first.
pub fn tag_under_cursor(editor: &mut Editor) {
    // Step 1: Extract the word under cursor, with fallback
    let word = {
        let w = editor.word_under_cursor_in_current_buffer();
        if w.is_empty() {
            extract_word_at_cursor(editor)
        } else {
            w
        }
    };

    if word.is_empty() {
        editor.set_infobar_message("No word under cursor".to_string());
        return;
    }

    // Step 2: Strip qualified identifiers to the last component.
    let tag_name = strip_qualifiers(&word);

    // Step 3: Initialize tag manager for current file
    let file_path = editor.current_buffer().and_then(|b| b.file_path.clone());
    if let Some(ref path) = file_path {
        editor.tag_manager.init(path);
    }

    // Load tags if they exist but aren't in memory
    if editor.tag_manager.is_empty() && editor.tag_manager.tag_file_exists() {
        if let Err(e) = editor.tag_manager.load_tags_file() {
            editor.set_infobar_message(format!("Failed to load tags: {}", e));
            return;
        }
    }

    // Step 4: Try the stripped name first, then the full word
    let matches = editor.tag_manager.find_tags(&tag_name);
    let matches = if matches.is_empty() && tag_name != word {
        editor.tag_manager.find_tags(&word)
    } else {
        matches
    };

    if matches.is_empty() {
        if editor.tag_manager.tag_file_exists() {
            editor.set_infobar_message(format!("Tag '{}' not found", tag_name));
        } else {
            editor.set_infobar_message(format!(
                "Tag '{}' not found (run :tags to generate)",
                tag_name
            ));
        }
        return;
    }

    handle_tag_matches(editor, matches, &tag_name);
}

/// Strip qualified name separators to get the final identifier.
pub fn strip_qualifiers(word: &str) -> String {
    if let Some(last) = word.rsplit("::").next() {
        let trimmed = last.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(last) = word.rsplit('.').next() {
        let trimmed = last.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    word.trim().to_string()
}

/// Manually extract the word under the cursor from the buffer content.
fn extract_word_at_cursor(editor: &Editor) -> String {
    let window = match editor.windows.active_window() {
        Some(w) => w,
        None => return String::new(),
    };
    let line_idx = window.cursor.position.line;
    let col = window.cursor.position.col;
    let buffer_id = window.buffer_id;

    let buffer = match editor.buffers.get(&buffer_id) {
        Some(b) => b,
        None => return String::new(),
    };

    if line_idx >= buffer.line_count() {
        return String::new();
    }

    let line = buffer.rope.line(line_idx).to_string();
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return String::new();
    }

    let col = col.min(chars.len().saturating_sub(1));

    let effective_col = if !is_tag_word_char(chars[col]) {
        if col > 0 && is_tag_word_char(chars[col - 1]) {
            col - 1
        } else if col + 1 < chars.len() && is_tag_word_char(chars[col + 1]) {
            col + 1
        } else {
            return String::new();
        }
    } else {
        col
    };

    let mut start = effective_col;
    while start > 0 && is_tag_word_char(chars[start - 1]) {
        start -= 1;
    }

    let mut end = effective_col;
    while end + 1 < chars.len() && is_tag_word_char(chars[end + 1]) {
        end += 1;
    }

    chars[start..=end].iter().collect()
}

fn is_tag_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Handle multiple tag matches: show popup and jump to first, or jump directly.
pub(crate) fn handle_tag_matches(editor: &mut Editor, matches: Vec<TagEntry>, word: &str) {
    if matches.len() == 1 {
        let tag = matches[0].clone();
        let root = editor.tag_manager.project_root().to_path_buf();
        let path = root.join(&tag.file);
        tag_jump(editor, &path, tag.line, &tag.name);
        editor.tag_manager.set_current_matches(matches);
        editor.set_status(format!("Tag: {}", word));
    } else {
        editor.tag_results = matches.clone();
        editor.tag_manager.set_current_matches(matches.clone());

        // NEW: Interactive tag list popup instead of FloatPopup
        let popup = TagListPopup::new(word, &matches, editor.tag_manager.project_root());
        editor.tag_list_popup = Some(popup);
        // NO active_popup — just check tag_list_popup.is_some()

        // Jump to first match as preview
        if let Some(tag) = editor.tag_manager.current_match().cloned() {
            let root = editor.tag_manager.project_root().to_path_buf();
            let path = root.join(&tag.file);
            tag_jump(editor, &path, tag.line, &tag.name);
        }

        editor.set_status(format!(
            "{} matches for '{}' — select in popup or :tn/:tp to cycle",
            editor.tag_results.len(),
            word
        ));
    }
}
/// Positions the cursor on the identifier name rather than column 0.
pub fn tag_jump(editor: &mut Editor, filepath: &PathBuf, line: usize, name: &str) {
    let current_path = editor.current_buffer().and_then(|b| b.file_path.clone());
    let cursor = editor
        .windows
        .active_window()
        .map(|w| w.cursor.position)
        .unwrap_or(crate::buffer::CursorPosition { line: 0, col: 0 });
    let target_line = line.saturating_sub(1);

    let same_file = match (current_path.as_ref(), filepath) {
        (Some(cur), target) => {
            let cur_canon = std::fs::canonicalize(cur).ok();
            let target_canon = std::fs::canonicalize(target).ok();
            match (cur_canon, target_canon) {
                (Some(a), Some(b)) => a == b,
                _ => cur == target,
            }
        }
        _ => false,
    };

    if !same_file || cursor.line != target_line {
        if let Some(ref cur_path) = current_path {
            editor
                .tag_manager
                .push_stack(cur_path.clone(), cursor.line, cursor.col);
        }
    }

    if let Err(e) = editor.open_file(filepath) {
        editor.set_infobar_message(format!("Failed to open {}: {}", filepath.display(), e));
        return;
    }

    // Find the exact column of the identifier on the target line
    let col = find_identifier_col(editor, target_line, name);
    editor.move_to_position(target_line, col);
    center_current_line(editor);
    editor.ensure_cursor_visible_all();
}

/// Find the char-offset column of `name` on `line_idx` in the active buffer.
/// Returns 0 if the name cannot be located (fallback to line start).
fn find_identifier_col(editor: &Editor, line_idx: usize, name: &str) -> usize {
    let window = match editor.windows.active_window() {
        Some(w) => w,
        None => return 0,
    };
    let buffer = match editor.buffers.get(&window.buffer_id) {
        Some(b) => b,
        None => return 0,
    };

    if line_idx >= buffer.line_count() {
        return 0;
    }

    let line_text = match buffer.line_text(line_idx) {
        Some(t) => t,
        None => return 0,
    };

    let chars: Vec<char> = line_text.chars().collect();
    let name_chars: Vec<char> = name.chars().collect();
    let name_len = name_chars.len();

    if name_len == 0 || chars.len() < name_len {
        return 0;
    }

    for start in 0..=chars.len().saturating_sub(name_len) {
        let before_ok = start == 0 || !is_tag_word_char(chars[start - 1]);
        let after_ok =
            start + name_len >= chars.len() || !is_tag_word_char(chars[start + name_len]);

        if before_ok && after_ok {
            let matches: bool = chars[start..start + name_len]
                .iter()
                .zip(name_chars.iter())
                .all(|(a, b)| a == b);
            if matches {
                return start;
            }
        }
    }

    0
}

/// Center the current line in the viewport.
pub fn center_current_line(editor: &mut Editor) {
    if let Some(window) = editor.windows.active_window_mut() {
        let cursor_line = window.cursor.position.line;
        let viewport_height = window.height as usize;
        let new_scroll = if cursor_line >= viewport_height / 2 {
            cursor_line - viewport_height / 2
        } else {
            0
        };
        window.viewport.scroll_line = new_scroll;
    }
}

/// Return to the previous location from the unified tag/jump stack.
pub fn tag_pop(editor: &mut Editor) -> CommandResult {
    match editor.tag_manager.pop_stack() {
        Some(entry) => {
            if let Err(e) = editor.open_file(&entry.file) {
                return CommandResult::Error(format!(
                    "Failed to open {}: {}",
                    entry.file.display(),
                    e
                ));
            }
            // Try to position on the identifier if we know it (otherwise col 0)
            editor.move_to_position(entry.line, entry.col);
            center_current_line(editor);
            editor.ensure_cursor_visible_all();
            editor.set_status(format!(
                "Jump back ({} remaining)",
                editor.tag_manager.stack_size()
            ));
            CommandResult::ViewChanged
        }
        None => {
            editor.set_infobar_message("No previous jump position".to_string());
            CommandResult::ViewChanged
        }
    }
}

/// Jump to the next match in the current tag result list.
pub fn tag_next(editor: &mut Editor) -> CommandResult {
    if editor.tag_manager.match_count() == 0 {
        editor.set_infobar_message("No active tag search".to_string());
        return CommandResult::ViewChanged;
    }

    let root = editor.tag_manager.project_root().to_path_buf();
    let tag = match editor.tag_manager.next_match() {
        Some(t) => t.clone(),
        None => return CommandResult::NoOp,
    };
    let idx = editor.tag_manager.match_index();
    let count = editor.tag_manager.match_count();

    let path = root.join(&tag.file);
    tag_jump(editor, &path, tag.line, &tag.name);
    editor.set_status(format!("Tag {}/{}: {}", idx + 1, count, tag.name));
    CommandResult::ViewChanged
}

/// Jump to the previous match in the current tag result list.
pub fn tag_prev(editor: &mut Editor) -> CommandResult {
    if editor.tag_manager.match_count() == 0 {
        editor.set_infobar_message("No active tag search".to_string());
        return CommandResult::ViewChanged;
    }

    let root = editor.tag_manager.project_root().to_path_buf();
    let tag = match editor.tag_manager.prev_match() {
        Some(t) => t.clone(),
        None => return CommandResult::NoOp,
    };
    let idx = editor.tag_manager.match_index();
    let count = editor.tag_manager.match_count();

    let path = root.join(&tag.file);
    tag_jump(editor, &path, tag.line, &tag.name);
    editor.set_status(format!("Tag {}/{}: {}", idx + 1, count, tag.name));
    CommandResult::ViewChanged
}
