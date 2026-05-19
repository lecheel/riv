//--+ ed/motion.rs
// src/ed/motion.rs
//! 2-character EasyMotion/AceJump style jumping.

use crate::editor::Editor;

const LABEL_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789";

/// Enter jump mode — wait for 2 chars before showing any labels
pub fn enter_jump_mode(editor: &mut Editor) {
    editor.jump.targets.clear();
    editor.jump.labels.clear();
    editor.jump.input.clear();
    editor.jump.phase = crate::editor::JumpPhase::PendingChar1;
    editor.jump.active = true;
    editor.set_status("Jump: __".to_string());
}

/// Handle a keypress in jump mode. Returns true to stay in jump mode.
pub fn handle_jump_key(editor: &mut Editor, c: char) -> bool {
    let c_lower = c.to_ascii_lowercase();

    // Any non-label char cancels the jump
    if !LABEL_CHARS.contains(c_lower) {
        cancel_jump(editor);
        return false;
    }

    match editor.jump.phase {
        crate::editor::JumpPhase::PendingChar1 => {
            editor.jump.input.push(c_lower);
            editor.jump.phase = crate::editor::JumpPhase::PendingChar2;
            editor.set_status(format!("Jump: {}_", editor.jump.input));
            true
        }
        crate::editor::JumpPhase::PendingChar2 => {
            editor.jump.input.push(c_lower);

            let targets = find_targets(editor, &editor.jump.input);

            // Store targets so the renderer can highlight the 2-char pattern
            editor.jump.targets = targets.clone();

            if targets.is_empty() {
                cancel_jump(editor);
                editor.set_status("Jump: no match".to_string());
                return false;
            }

            // Exactly 1 match -> jump immediately without showing labels
            if targets.len() == 1 {
                let target = targets[0].clone();
                if let Some(window) = editor.windows.active_window_mut() {
                    let buffer_id = window.buffer_id;
                    window.cursor.position.line = target.line;
                    window.cursor.position.col = target.col;
                    window.cursor.desired_col = None;
                    editor.ensure_cursor_visible(&buffer_id);
                }
                cancel_jump(editor); // Clears targets since we're done
                return false;
            }

            // Multiple matches -> assign single-char labels (a, b, c...)
            let labels: Vec<(usize, String)> = targets
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let ch = LABEL_CHARS.chars().nth(i).unwrap();
                    (i, ch.to_string())
                })
                .collect();

            // Targets are already stored in editor.jump.targets from above!
            editor.jump.labels = labels;
            editor.jump.phase = crate::editor::JumpPhase::Active;
            editor.set_status(format!(
                "Jump: {} [{} targets]",
                editor.jump.input,
                editor.jump.targets.len()
            ));
            true
        }
        crate::editor::JumpPhase::Active => {
            // Find target by single-char label
            let target_idx = editor
                .jump
                .labels
                .iter()
                .find(|(_, label)| label.starts_with(c_lower))
                .map(|(idx, _)| *idx);

            if let Some(idx) = target_idx {
                let target = editor.jump.targets[idx].clone();
                if let Some(window) = editor.windows.active_window_mut() {
                    let buffer_id = window.buffer_id;
                    window.cursor.position.line = target.line;
                    window.cursor.position.col = target.col;
                    window.cursor.desired_col = None;
                    editor.ensure_cursor_visible(&buffer_id);
                }
            }

            cancel_jump(editor);
            false
        }
    }
}

/// Search the visible viewport for the 2-char pattern
fn find_targets(editor: &Editor, pattern: &str) -> Vec<crate::editor::JumpTarget> {
    let window = match editor.windows.active_window() {
        Some(w) => w,
        None => return Vec::new(),
    };

    let buffer_id = window.buffer_id;
    let buffer = match editor.buffers.get(&buffer_id) {
        Some(b) => b,
        None => return Vec::new(),
    };

    let viewport_height = window.height as usize;
    let scroll_y = window.viewport.scroll_line;
    let viewport_end = (scroll_y + viewport_height).min(buffer.line_count());

    let mut targets = Vec::new();
    let pat_lower = pattern.to_lowercase();
    let pat_chars: Vec<char> = pat_lower.chars().collect();

    if pat_chars.len() != 2 {
        return targets;
    }

    for line_idx in scroll_y..viewport_end {
        let line = match buffer.line_text(line_idx) {
            Some(l) => l,
            None => continue,
        };

        let line_lower = line.to_lowercase();
        let line_chars: Vec<char> = line_lower.chars().collect();

        if line_chars.len() < 2 {
            continue;
        }

        for col in 0..=(line_chars.len().saturating_sub(2)) {
            if line_chars[col] == pat_chars[0] && line_chars[col + 1] == pat_chars[1] {
                targets.push(crate::editor::JumpTarget {
                    line: line_idx,
                    col,
                });
            }
        }
    }

    targets
}

pub fn cancel_jump(editor: &mut Editor) {
    editor.jump.active = false;
    editor.jump.targets.clear();
    editor.jump.labels.clear();
    editor.jump.input.clear();
    editor.jump.phase = crate::editor::JumpPhase::PendingChar1;
    editor.clear_messages();
}
