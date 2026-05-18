//! Popup overlays: help viewer, buffer list, file picker, floating messages, etc.
//!
//! All list-based popups implement the [`Scrollable`] trait, which provides
//! shared `move_up`, `move_down`, and `clamp_scroll` logic so there is no
//! duplication across popup types.

mod buffer_list;
pub mod completion_popup;
pub mod diff_popup;
pub mod function_list;
pub mod guide_popup;
mod help;
mod keymap;
mod mark_list;
pub mod register;
mod taglist;

// Re-export help popup types
pub use buffer_list::{render_buffer_list_popup, BufferListEntry, BufferListPopup};
pub use function_list::{FunctionEntry, FunctionListPopup};
pub use help::{render_help_popup, HelpPopup};
pub use keymap::{render_keymap_popup, KeymapEntry, KeymapPopup};
pub use mark_list::{render_mark_list_popup, MarkEntry, MarkListPopup};
// pub use register::render_register_popup;
pub use taglist::{render_tag_list_popup, TagListPopup};
// mod file_picker;
// mod mru;

use unicode_width::UnicodeWidthStr;

/// Word-wrap text to fit within `max_width` display columns.
/// Respects paragraph breaks (newlines) and preserves code-block /
/// indented lines as-is (truncated if too long).
pub fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }

        let trimmed = paragraph.trim_start();
        if trimmed.starts_with("```") || paragraph.starts_with("  ") || paragraph.starts_with('\t')
        {
            if UnicodeWidthStr::width(paragraph) <= max_width {
                lines.push(paragraph.to_string());
            } else {
                lines.push(truncate_to_width(paragraph, max_width).to_string());
            }
            continue;
        }

        let mut current_line = String::new();
        let mut current_width = 0usize;

        for word in paragraph.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            let (sep, sep_width) = if current_line.is_empty() {
                ("", 0)
            } else {
                (" ", 1)
            };

            if current_width + sep_width + word_width > max_width && !current_line.is_empty() {
                lines.push(current_line);
                current_line = word.to_string();
                current_width = word_width;
            } else {
                current_line.push_str(sep);
                current_line.push_str(word);
                current_width += sep_width + word_width;
            }
        }

        if !current_line.is_empty() {
            lines.push(current_line);
        }
    }

    lines
}

// Re-export truncate_to_width for submodules
pub use crate::rounded_box::truncate_to_width;
