// src/popup/mod.rs
//! Popup overlays: trait definitions, utilities, and re-exports.
//!
//! The [`Scrollable`] trait is defined here and implemented by all
//! list-based popup types.  Concrete popup types and their render
//! functions live in sibling submodules.

mod buffer_list;
pub mod completion_popup;
pub mod diff_popup;
mod file_picker;
pub mod function_list;
pub mod guide_popup;
mod help;
mod keymap;
mod mark_list;
mod mru;
pub mod register;
mod taglist;

// ── Re-exports ──────────────────────────────────────────────────────
pub use buffer_list::{render_buffer_list_popup, BufferListEntry, BufferListPopup};
pub use file_picker::{case_insensitive_find, render_file_picker, FilePicker};
pub use function_list::{FunctionEntry, FunctionListPopup};
pub use help::{render_help_popup, HelpPopup};
pub use keymap::{render_keymap_popup, KeymapEntry, KeymapPopup};
pub use mark_list::{render_mark_list_popup, MarkEntry, MarkListPopup};
pub use mru::{render_mru_popup, MruPopup};
pub use taglist::{render_tag_list_popup, TagListPopup};

use unicode_width::UnicodeWidthStr;

// ════════════════════════════════════════════════════════════════════════
// SHARED SCROLLABLE TRAIT
// ════════════════════════════════════════════════════════════════════════

/// Shared scroll/navigation logic for all list-based popups.
///
/// Implement four primitive accessors; `move_up`, `move_down`, and
/// `clamp_scroll` are derived for free.  Types that need to skip header
/// rows (e.g. [`KeymapPopup`]) only need to override `move_up`/`move_down`
/// and can still delegate `clamp_scroll` to this trait.
pub trait Scrollable {
    /// Current selected index.
    fn selected(&self) -> usize;
    /// Mutable reference to the selected index.
    fn selected_mut(&mut self) -> &mut usize;
    /// Mutable reference to the scroll offset.
    fn scroll_mut(&mut self) -> &mut usize;
    /// Total number of navigable items.
    fn len(&self) -> usize;
    /// How many rows are visible at once (may be dynamic).
    fn visible_rows(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Adjust scroll so the selected item stays inside the visible window.
    fn clamp_scroll(&mut self) {
        let sel = self.selected();
        let vis = self.visible_rows();
        let sc = self.scroll_mut();
        if sel < *sc {
            *sc = sel;
        } else if vis > 0 && sel >= *sc + vis {
            *sc = sel - vis + 1;
        }
    }

    /// Move selection up by one row.
    fn move_up(&mut self) {
        if self.selected() > 0 {
            *self.selected_mut() -= 1;
            self.clamp_scroll();
        }
    }

    /// Move selection down by one row.
    fn move_down(&mut self) {
        if !self.is_empty() && self.selected() + 1 < self.len() {
            *self.selected_mut() += 1;
            self.clamp_scroll();
        }
    }
}

// ════════════════════════════════════════════════════════════════════════
// SHARED UTILITIES
// ════════════════════════════════════════════════════════════════════════

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
        if trimmed.starts_with("```") || paragraph.starts_with("  ") || paragraph.starts_with('\t') {
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
            let (sep, sep_width) = if current_line.is_empty() { ("", 0) } else { (" ", 1) };

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
