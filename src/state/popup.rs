//! Popup and Overlay subsystem state — extracted from the Editor core.
//!
//! Groups all popup-related fields and their key-handlers, vastly reducing
//! the size of the main process_key loop.

use crate::buffer::BufferKind;
use crate::ed::FileOpsExt;
use crate::ed::MovementExt;
use crate::ed::RipgrepExt;
use crate::ed::{tag, MarksExt};
use crate::editor::{CommandResult, FloatPopup, Mode};
use crate::guide::Guide;
use crate::overlay::OverlayTracker;
use crate::popup::{FilePicker, HelpPopup, MruPopup, Scrollable, TagListPopup};
use crate::terminal::Key;
use crate::Editor;

// ── Popup state ────────────────────────────────────────────────────

/// Popup and Overlay subsystem state.
pub struct PopupState {
    pub float: Option<FloatPopup>,
    pub help: Option<HelpPopup>,
    pub buffer_list: Option<crate::popup::BufferListPopup>,
    pub file_picker: Option<FilePicker>,
    pub keymap: Option<crate::popup::KeymapPopup>,
    pub mru: Option<MruPopup>,
    pub register: Option<Vec<String>>,
    pub register_title: String,
    pub overlay: OverlayTracker,
    pub fmt_info: Option<Vec<String>>,
    pub fmt_info_title: String,
    pub mark_list: Option<crate::popup::MarkListPopup>,
    pub function_list: Option<crate::popup::FunctionListPopup>,
    pub tag_list: Option<TagListPopup>,
    pub guide: Option<Guide>,
}

impl PopupState {
    pub fn new() -> Self {
        Self {
            float: None,
            help: None,
            buffer_list: None,
            file_picker: None,
            keymap: None,
            mru: None,
            register: None,
            register_title: "Registers".to_string(),
            overlay: OverlayTracker::default(),
            fmt_info: None,
            fmt_info_title: "Format Info".to_string(),
            mark_list: None,
            function_list: None,
            tag_list: None,
            guide: None,
        }
    }
}

// ── Popup key dispatch ─────────────────────────────────────────────

impl Editor {
    /// Master dispatcher for all popup key events.
    /// Returns Some(CommandResult) if a popup consumed the key, None if it should fall through.
    #[rustfmt::skip]
    pub fn handle_popup_keys(&mut self, key: &Key) -> Option<CommandResult> {
        if let Some(r) = self.handle_float_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_register_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_fmt_info_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_mark_list_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_buffer_list_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_keymap_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_mru_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_tag_list_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_guide_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_function_list_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_file_picker_key(key) { return Some(r); }
        if let Some(r) = self.handle_help_popup_key(key) { return Some(r); }
        if let Some(r) = self.handle_ripgrep_buffer_key(key) { return Some(r); }
        None
    }

    fn handle_ripgrep_buffer_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.mode != Mode::Normal {
            return None;
        }

        // If a real popup is active, let it handle the keys instead
        let popup_active = self.popup.buffer_list.is_some()
            || self.popup.mru.is_some()
            || self.popup.file_picker.is_some()
            || self.popup.function_list.is_some()
            || self.popup.keymap.is_some()
            || self.popup.help.is_some()
            || self.popup.mark_list.is_some();

        if popup_active {
            return None;
        }

        let is_ripgrep = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.kind == BufferKind::Ripgrep)
            .unwrap_or(false);

        if !is_ripgrep {
            return None;
        }

        match key {
            Key::Enter => {
                self.dirty.mark_all();
                Some(self.ripgrep_goto_result())
            }
            Key::Char('q') | Key::Char('Q') | Key::Escape => Some(self.ripgrep_close_buffer()),
            _ => None, // Fall through to normal navigation keybinds
        }
    }

    fn handle_float_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.popup.float.is_none() {
            return None;
        }

        if *key == Key::Escape || *key == Key::Ctrl('c') {
            self.popup.float = None;
            self.popup.overlay.float = None;
            self.shortcut_active = false;
            self.shortcut_pending_keys.clear();
            self.dirty.mark_all();
            return Some(CommandResult::NoOp);
        }

        if self.shortcut_active {
            if *key == Key::Backspace {
                if !self.shortcut_pending_keys.is_empty() {
                    self.shortcut_pending_keys.pop();
                    self.rebuild_shortcut_popup();
                    return Some(CommandResult::NoOp);
                }
                self.popup.float = None;
                self.popup.overlay.float = None;
                self.shortcut_active = false;
                self.dirty.mark_all();
                return Some(CommandResult::NoOp);
            }

            let mut new_prefix = self.shortcut_pending_keys.clone();
            new_prefix.push(key.clone());
            let prefix_len = new_prefix.len();

            let matching: Vec<usize> = self
                .active_shortcuts
                .iter()
                .enumerate()
                .filter(|(_, (keys, _))| keys.len() >= prefix_len && keys[..prefix_len] == new_prefix[..])
                .map(|(i, _)| i)
                .collect();

            if matching.is_empty() {
                self.popup.float = None;
                self.popup.overlay.float = None;
                self.shortcut_active = false;
                self.shortcut_pending_keys.clear();
                self.dirty.mark_all();
                return Some(CommandResult::NoOp);
            }

            self.shortcut_pending_keys = new_prefix;

            let exact_idx = matching.iter().find(|&&i| self.active_shortcuts[i].0.len() == prefix_len);
            let has_longer = matching.iter().any(|&i| self.active_shortcuts[i].0.len() > prefix_len);

            if let Some(&idx) = exact_idx {
                if !has_longer {
                    let action = self.active_shortcuts[idx].1.clone();
                    self.popup.float = None;
                    self.popup.overlay.float = None;
                    self.shortcut_active = false;
                    self.shortcut_pending_keys.clear();
                    self.dirty.mark_all();
                    return Some(self.process_action(action));
                }
                self.rebuild_shortcut_popup();
                return Some(CommandResult::NoOp);
            }

            self.rebuild_shortcut_popup();
            return Some(CommandResult::NoOp);
        }

        // Default: dismiss and fall through to process key normally
        let old_rect = self.popup.overlay.float;
        self.popup.float = None;
        self.popup.overlay.float = None;
        if let Some(rect) = old_rect {
            self.dirty.mark_popup_closed(rect);
        }
        self.dirty.cursor = true;
        None
    }

    fn handle_register_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.popup.register.is_none() {
            return None;
        }

        if *key == Key::Escape || *key == Key::Char('q') || *key == Key::Enter {
            self.popup.register = None;
            self.popup.register_title = "Registers".to_string();
            self.dirty.mark_all();
            return Some(CommandResult::NoOp);
        }
        // Dismiss and fall through
        self.popup.register = None;
        self.popup.register_title = "Registers".to_string();
        self.dirty.mark_all();
        None
    }

    fn handle_fmt_info_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.popup.fmt_info.is_none() {
            return None;
        }

        match key {
            Key::Escape | Key::Char('q') | Key::Enter => {
                self.popup.fmt_info = None;
                self.popup.fmt_info_title = "Format Info".to_string();
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
            _ => {
                self.popup.fmt_info = None;
                self.popup.fmt_info_title = "Format Info".to_string();
                self.dirty.mark_all();
                None // Fall through
            }
        }
    }

    fn handle_help_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        let popup = match &mut self.popup.help {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Escape => {
                self.popup.help = None;
                self.popup.overlay.help = None;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
            Key::Up | Key::Char('k') | Key::PageUp => {
                popup.move_up();
                self.dirty.help = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Down | Key::Char('j') | Key::PageDown => {
                popup.move_down();
                self.dirty.help = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            _ => {
                let old_rect = self.popup.overlay.help;
                self.popup.help = None;
                self.popup.overlay.help = None;
                if let Some(rect) = old_rect {
                    self.dirty.mark_popup_closed(rect);
                }
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
        }
    }

    fn handle_buffer_list_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        let popup = match &mut self.popup.buffer_list {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.popup.buffer_list = None;
                self.popup.overlay.buffer_list = None;
                self.dirty.mark_all();
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Up | Key::PageUp => {
                popup.move_up();
                self.dirty.buffer_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Down | Key::PageDown => {
                popup.move_down();
                self.dirty.buffer_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Enter => {
                if let Some(buffer_id) = popup.selected_buffer_id() {
                    let old_rect = self.popup.overlay.buffer_list;
                    self.popup.buffer_list = None;
                    self.popup.overlay.buffer_list = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }

                    self.save_current_position();
                    if let Some(window) = self.windows.active_window_mut() {
                        window.set_buffer(buffer_id);
                    }
                    self.restore_cursor_position();
                    self.clamp_cursor_to_buffer(&buffer_id);

                    // Rebuild viewport
                    {
                        let (cursor_line, line_count, edit_height) = {
                            let window = self.windows.active_window().unwrap();
                            let buffer = self.buffers.get(&buffer_id).unwrap();
                            (
                                window.cursor.position.line,
                                buffer.line_count(),
                                window.height.saturating_sub(1) as usize,
                            )
                        };
                        if let Some(window) = self.windows.active_window_mut() {
                            let half = edit_height / 2;
                            let ideal_scroll = cursor_line.saturating_sub(half);
                            if line_count > edit_height {
                                let max_scroll = line_count.saturating_sub(edit_height);
                                window.viewport.scroll_line = ideal_scroll.min(max_scroll);
                            } else {
                                window.viewport.scroll_line = 0;
                            }
                        }
                    }

                    let buf_name = self.buffers.get(&buffer_id).map(|b| b.display_name()).unwrap_or_else(|| "?".into());
                    self.set_status(format!("Switched to buffer: {}", buf_name));
                    self.dirty.mark_all();
                } else {
                    let old_rect = self.popup.overlay.buffer_list;
                    self.popup.buffer_list = None;
                    self.popup.overlay.buffer_list = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }
                    self.dirty.cursor = true;
                }
                Some(CommandResult::NoOp)
            }
            Key::Backspace => {
                popup.filter_pop();
                self.dirty.buffer_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Char(c) => {
                popup.filter_push(*c);
                self.dirty.buffer_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            _ => {
                let old_rect = self.popup.overlay.buffer_list;
                self.popup.buffer_list = None;
                self.popup.overlay.buffer_list = None;
                if let Some(rect) = old_rect {
                    self.dirty.mark_popup_closed(rect);
                }
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
        }
    }

    fn handle_keymap_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        let popup = match &mut self.popup.keymap {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Escape => {
                self.popup.keymap = None;
                self.popup.overlay.help = None;
                self.dirty.mark_all();
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Up | Key::PageUp => {
                popup.move_up();
                self.dirty.help = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Down | Key::PageDown => {
                popup.move_down();
                self.dirty.help = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Home => {
                popup.selected = 0;
                popup.scroll = 0;
                while popup.selected < popup.entries.len() && popup.entries[popup.selected].is_header {
                    popup.selected += 1;
                }
                popup.clamp_scroll();
                self.dirty.help = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::End => {
                popup.selected = popup.entries.len().saturating_sub(1);
                while popup.selected > 0 && popup.entries[popup.selected].is_header {
                    popup.selected -= 1;
                }
                popup.clamp_scroll();
                self.dirty.help = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            _ => {
                self.popup.keymap = None;
                self.popup.overlay.help = None;
                self.dirty.mark_all();
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
        }
    }

    fn handle_mru_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        let popup = match &mut self.popup.mru {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.popup.mru = None;
                self.popup.overlay.mru = None;
                self.dirty.mark_all();
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Up | Key::PageUp => {
                popup.move_up();
                self.dirty.mru = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Down | Key::PageDown => {
                popup.move_down();
                self.dirty.mru = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Enter => {
                if let Some(entry) = popup.selected_entry().cloned() {
                    let old_rect = self.popup.overlay.mru;
                    self.popup.mru = None;
                    self.popup.overlay.mru = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }

                    match self.open_file(&entry.path) {
                        Ok(_) => {
                            if let Some(window) = self.windows.active_window_mut() {
                                if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                                    let max_line = buffer.line_count().saturating_sub(1);
                                    window.cursor.position.line = entry.line.min(max_line);
                                    let max_col = buffer.line_len(window.cursor.position.line);
                                    window.cursor.position.col = entry.col.min(max_col);
                                    window.cursor.desired_col = None;
                                    let bid = window.buffer_id;
                                    self.ensure_cursor_visible(&bid);
                                }
                            }
                            self.dirty.mark_all();
                            Some(CommandResult::ViewChanged)
                        }
                        Err(e) => {
                            self.set_infobar_message(format!("Failed to open: {}", e));
                            self.dirty.mark_all();
                            Some(CommandResult::ViewChanged)
                        }
                    }
                } else {
                    let old_rect = self.popup.overlay.mru;
                    self.popup.mru = None;
                    self.popup.overlay.mru = None;
                    if let Some(rect) = old_rect {
                        self.dirty.mark_popup_closed(rect);
                    }
                    self.dirty.cursor = true;
                    Some(CommandResult::NoOp)
                }
            }
            Key::Delete => {
                if let Some(&real_idx) = popup.filtered.get(popup.selected) {
                    if let Some(entry) = popup.entries.get(real_idx).cloned() {
                        self.mru.remove(&entry.path);
                        popup.entries.remove(real_idx);

                        let query = popup.filter.to_lowercase();
                        popup.filtered.clear();
                        for (i, entry) in popup.entries.iter().enumerate() {
                            let file_name = entry.path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                            let dir_str = entry.path.parent().and_then(|p| p.to_str()).unwrap_or("").to_string();
                            if query.is_empty() || file_name.to_lowercase().contains(&query) || dir_str.to_lowercase().contains(&query) {
                                popup.filtered.push(i);
                            }
                        }
                        if popup.selected >= popup.filtered.len() && !popup.filtered.is_empty() {
                            popup.selected = popup.filtered.len() - 1;
                        }
                        <MruPopup as Scrollable>::clamp_scroll(popup);

                        if popup.entries.is_empty() {
                            let old_rect = self.popup.overlay.mru;
                            self.popup.mru = None;
                            self.popup.overlay.mru = None;
                            if let Some(rect) = old_rect {
                                self.dirty.mark_popup_closed(rect);
                            }
                        }
                    }
                }
                self.dirty.mru = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Home => {
                popup.toggle_sort(&self.mru);
                self.dirty.mru = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Backspace => {
                popup.filter_pop();
                self.dirty.mru = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Char(c) => {
                popup.filter_push(*c);
                self.dirty.mru = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            _ => Some(CommandResult::NoOp),
        }
    }
    fn handle_tag_list_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        let popup = match &mut self.popup.tag_list {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Char('j') | Key::Down => {
                popup.move_down();
                if let Some(entry) = popup.selected_entry().cloned() {
                    let path = std::path::PathBuf::from(&entry.file);
                    tag::tag_jump(self, &path, entry.line, &entry.name);
                }
                Some(CommandResult::ViewChanged)
            }
            Key::Char('k') | Key::Up => {
                popup.move_up();
                if let Some(entry) = popup.selected_entry().cloned() {
                    let path = std::path::PathBuf::from(&entry.file);
                    tag::tag_jump(self, &path, entry.line, &entry.name);
                }
                Some(CommandResult::ViewChanged)
            }
            Key::Enter => {
                if let Some(entry) = popup.selected_entry().cloned() {
                    let path = std::path::PathBuf::from(&entry.file);
                    tag::tag_jump(self, &path, entry.line, &entry.name);
                }
                self.popup.tag_list = None;
                Some(CommandResult::ViewChanged)
            }
            Key::Escape => {
                self.popup.tag_list = None;
                Some(CommandResult::ViewChanged)
            }
            _ => None,
        }
    }

    fn handle_guide_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        let popup = match &mut self.popup.guide {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.popup.guide = None;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
            Key::Up | Key::PageUp => {
                popup.move_up();
                self.dirty.guide = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Down | Key::PageDown => {
                popup.move_down();
                self.dirty.guide = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Enter => {
                if let Some(entry) = popup.selected_entry().cloned() {
                    let file_path = popup.root.join(&entry.file);
                    if let Err(e) = self.open_file(&file_path) {
                        self.popup.guide = None;
                        self.dirty.mark_all();
                        return Some(CommandResult::Error(format!("Cannot open {}: {}", entry.file, e)));
                    }

                    if let Some(window) = self.windows.active_window() {
                        let buffer_id = window.buffer_id;
                        if let Some(buffer) = self.buffers.get(&buffer_id) {
                            let source: String = buffer.rope.to_string();
                            if let Some(line) = Guide::find_anchor_line(&source, &entry.anchor) {
                                let max_line = buffer.line_count().saturating_sub(1);
                                if let Some(w) = self.windows.active_window_mut() {
                                    w.cursor.position.line = line.min(max_line);
                                    w.cursor.position.col = 0;
                                    w.cursor.desired_col = None;
                                    let bid = w.buffer_id;
                                    self.ensure_cursor_visible(&bid);
                                }
                                self.set_status(format!("→ {} ({})", entry.label, entry.anchor));
                            } else {
                                self.set_status(format!("Anchor not found: '{}' in {}", entry.anchor, entry.file));
                            }
                        }
                    }
                    self.scroll_center();
                    self.popup.guide = None;
                    self.dirty.mark_all();
                    return Some(CommandResult::ViewChanged);
                }
                self.popup.guide = None;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
            Key::Backspace => {
                popup.filter_pop();
                self.dirty.guide = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Char(c) => {
                popup.filter_push(*c);
                self.dirty.guide = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            _ => {
                self.popup.guide = None;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
        }
    }

    fn handle_function_list_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        let popup = match &mut self.popup.function_list {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.popup.function_list = None;
                self.popup.overlay.function_list = None;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
            Key::Up | Key::PageUp => {
                popup.move_up();
                self.dirty.function_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Down | Key::PageDown => {
                popup.move_down();
                self.dirty.function_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Enter => {
                if let Some(entry) = popup.selected_entry().cloned() {
                    self.popup.function_list = None;
                    self.popup.overlay.function_list = None;
                    if let Some(window) = self.windows.active_window_mut() {
                        let max_line = self
                            .buffers
                            .get(&window.buffer_id)
                            .map(|b| b.line_count().saturating_sub(1))
                            .unwrap_or(0);
                        window.cursor.position.line = entry.line.min(max_line);
                        window.cursor.position.col = 0;
                        window.cursor.desired_col = None;
                        let bid = window.buffer_id;
                        self.ensure_cursor_visible(&bid);
                    }
                    self.set_status(format!("→ {} (line {})", entry.name, entry.line + 1));
                    self.dirty.mark_all();
                    return Some(CommandResult::ViewChanged);
                }
                self.popup.function_list = None;
                self.popup.overlay.function_list = None;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
            Key::Backspace => {
                popup.filter_pop();
                self.dirty.function_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Char(c) => {
                popup.filter_push(*c);
                self.dirty.function_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            _ => {
                self.popup.function_list = None;
                self.popup.overlay.function_list = None;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
        }
    }

    fn handle_file_picker_key(&mut self, key: &Key) -> Option<CommandResult> {
        let picker = match &mut self.popup.file_picker {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Escape | Key::Char('\x1b') | Key::Ctrl('[') | Key::Char('q') | Key::Char('Q') => {
                self.popup.file_picker = None;
                self.popup.overlay.file_picker = None;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
            Key::Up | Key::Char('k') | Key::PageUp => {
                picker.move_up();
                self.dirty.file_picker = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Down | Key::Char('j') | Key::PageDown => {
                picker.sync_visible_height(self.term_height);
                picker.move_down();
                self.dirty.file_picker = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Enter => {
                // In flat mode all entries are files; in tree mode we still
                // need to check is_dir for directory navigation.
                if let Some(entry) = picker.selected_entry() {
                    if entry.is_dir && !picker.flat {
                        picker.go_into(&entry.path.clone());
                        self.dirty.file_picker = true;
                        return Some(CommandResult::NoOp);
                    } else {
                        let path = entry.path.clone();
                        let old_rect = self.popup.overlay.file_picker;
                        self.popup.file_picker = None;
                        self.popup.overlay.file_picker = None;
                        if let Some(rect) = old_rect {
                            self.dirty.mark_popup_closed(rect);
                        }
                        return Some(match self.open_file(&path) {
                            Ok(_) => {
                                self.dirty.mark_all();
                                CommandResult::NoOp
                            }
                            Err(e) => CommandResult::Error(e.to_string()),
                        });
                    }
                }
                Some(CommandResult::NoOp)
            }
            Key::Char('-') => {
                if picker.flat {
                    // In flat mode, treat '-' as a regular filter character
                    picker.filter_push('-');
                } else {
                    // In tree mode, navigate up
                    picker.go_up();
                }
                self.dirty.file_picker = true;
                Some(CommandResult::NoOp)
            }
            // ── Toggle flat / tree mode ────────────────────────────
            Key::Char('~') => {
                picker.toggle_flat();
                self.dirty.file_picker = true;
                Some(CommandResult::NoOp)
            }
            Key::Backspace => {
                picker.filter_pop();
                self.dirty.file_picker = true;
                Some(CommandResult::NoOp)
            }
            Key::Char(c) => {
                picker.filter_push(*c);
                self.dirty.file_picker = true;
                Some(CommandResult::NoOp)
            }
            _ => Some(CommandResult::NoOp),
        }
    }

    fn handle_mark_list_popup_key(&mut self, key: &Key) -> Option<CommandResult> {
        let popup = match &mut self.popup.mark_list {
            Some(p) => p,
            None => return None,
        };
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.popup.mark_list = None;
                self.dirty.mark_list = true;
                self.dirty.cursor = true;
                self.dirty.mark_all();
                Some(CommandResult::NoOp)
            }
            Key::Up | Key::PageUp => {
                popup.move_up();
                self.dirty.mark_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Down | Key::PageDown => {
                popup.move_down();
                self.dirty.mark_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Enter => {
                if let Some(entry) = popup.selected_entry().cloned() {
                    self.popup.mark_list = None;
                    if self.buffers.get(&entry.buffer_id).is_none() {
                        self.search.marks.remove(&entry.name);
                        self.set_error(format!("Mark '{}' buffer closed", entry.name));
                        self.dirty.mark_all();
                        return Some(CommandResult::NoOp);
                    }
                    self.save_jump_mark();
                    if let Some(window) = self.windows.active_window() {
                        if window.buffer_id != entry.buffer_id {
                            if let Some(w) = self.windows.active_window_mut() {
                                w.set_buffer(entry.buffer_id);
                            }
                        }
                    }
                    self.move_to_position(entry.line, entry.col);
                    self.ensure_cursor_visible_all();
                    self.set_status(format!("Jumped to mark '{}'", entry.name));
                    self.dirty.mark_all();
                    return Some(CommandResult::ViewChanged);
                }
                self.popup.mark_list = None;
                self.dirty.mark_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Delete => {
                if let Some(entry) = popup.selected_entry().cloned() {
                    let name = entry.name;
                    self.search.marks.remove(&name);
                    popup.remove_selected();
                    if popup.entries.is_empty() {
                        self.popup.mark_list = None;
                    }
                    self.set_status(format!("Mark '{}' removed", name));
                }
                self.dirty.mark_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Backspace => {
                popup.filter_pop();
                self.dirty.mark_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            Key::Char(c) => {
                popup.filter_push(*c);
                self.dirty.mark_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
            _ => {
                self.popup.mark_list = None;
                self.dirty.mark_list = true;
                self.dirty.cursor = true;
                Some(CommandResult::NoOp)
            }
        }
    }
}
