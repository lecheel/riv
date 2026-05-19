use crate::ed::editing::EditingExt;
use crate::editor::FloatPopup;
use crate::editor::{CommandResult, Editor};

impl Editor {
    /// Poll for Codeium server startup result and show feedback.
    pub fn tick_codeium_startup(&mut self) {
        if let Some(result) = self.codeium.poll_startup() {
            match result {
                Ok(()) => {
                    self.set_status("Codeium: connected ✓".to_string());
                }
                Err(e) => {
                    self.set_infobar_message(format!("Codeium: {}", e));
                    self.codeium.is_connected = false;
                }
            }
            self.dirty.mark_all();
        }
    }

    /// Show the register list popup (used by :reg and Ctrl-R in insert mode)
    /// Resolve the contents of a register by name.
    /// Supports: a-z (named), " + * (default/clipboard), % (current filename).
    pub fn resolve_register(&self, c: char) -> Option<String> {
        match c {
            '"' | '+' | '*' => Some(self.yank_register.clone()),
            '%' => self
                .current_buffer()
                .and_then(|b| b.file_path.as_ref())
                .and_then(|p| p.to_str())
                .map(|s| s.to_string()),
            _ if c.is_ascii_lowercase() => self.get_named_register(c).map(|s| s.to_string()),
            _ => None,
        }
    }
    /// Show a popup listing all functions/methods in the current buffer.
    /// Uses tree-sitter to find function nodes.
    pub fn show_function_list(&mut self) -> CommandResult {
        let entries = {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => return CommandResult::Error("No active window".into()),
            };
            let buffer_id = window.buffer_id;
            let buffer = match self.buffers.get_mut(&buffer_id) {
                Some(b) => b,
                None => return CommandResult::Error("No active buffer".into()),
            };

            if buffer.tree().is_none() {
                buffer.init_tree_sitter();
            } else {
                buffer.reparse_tree();
            }

            crate::ed::text_object::collect_all_functions(buffer)
        };

        if entries.is_empty() {
            return CommandResult::Message("No functions found in this buffer".into());
        }
        // Pre-select the function closest to the current cursor line
        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);

        let mut popup = crate::popup::FunctionListPopup::new(entries);
        // Find nearest function above or at cursor
        let mut best_idx = 0;
        let mut best_dist = usize::MAX;
        for (i, entry) in popup.all_entries.iter().enumerate() {
            let dist = cursor_line as isize - entry.line as isize;
            if dist >= 0 && (dist as usize) < best_dist {
                best_dist = dist as usize;
                best_idx = i;
            }
        }
        popup.selected = best_idx;

        self.popup.function_list = Some(popup);
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    pub fn show_register_popup(&mut self) {
        self.popup.register_title = "Registers".to_string();

        let mut lines = Vec::new();

        if !self.yank_register.is_empty() {
            let is_multiline = self.yank_register.contains('\n');
            let first_line = self
                .yank_register
                .lines()
                .next()
                .unwrap_or("")
                .replace('\r', "\\r")
                .replace('\t', "\\t");

            let max_len = if is_multiline { 89 } else { 97 };
            let mut preview = if first_line.chars().count() > max_len {
                format!("{}…", first_line.chars().take(max_len).collect::<String>())
            } else {
                first_line
            };

            if is_multiline {
                preview.push_str(" (...)");
            }

            lines.push(format!("\"\"   {}", preview));
        }

        if let Some(buffer) = self.current_buffer() {
            let path_str = if let Some(path) = buffer.file_path.as_ref() {
                path.to_str().unwrap_or("[Invalid Path]").to_string()
            } else {
                buffer.display_name()
            };

            let truncated = if path_str.chars().count() > 100 {
                let char_count = path_str.chars().take(97).collect::<String>();
                format!("{}…", char_count)
            } else {
                path_str
            };
            lines.push(format!("%    {}", truncated));
        }

        for c in 'a'..='z' {
            if let Some(content) = self.get_named_register(c) {
                if !content.is_empty() {
                    let is_multiline = content.contains('\n');
                    let first_line = content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .replace('\r', "\\r")
                        .replace('\t', "\\t");

                    let max_len = if is_multiline { 89 } else { 97 };
                    let mut preview = if first_line.chars().count() > max_len {
                        format!("{}…", first_line.chars().take(max_len).collect::<String>())
                    } else {
                        first_line
                    };

                    if is_multiline {
                        preview.push_str(" (...)");
                    }

                    lines.push(format!("\"{}   {}", c, preview));
                }
            }
        }

        if lines.is_empty() {
            self.popup.register = None;
            self.set_status("All registers are empty".to_string());
        } else {
            self.popup.register = Some(lines);
        }
        self.dirty.mark_all();
    }

    /// Show a popup listing all named marks (a-z) for quick navigation.
    pub fn show_mark_list(&mut self) -> CommandResult {
        let mut entries = Vec::new();

        for (&name, &(buffer_id, pos)) in &self.search.marks {
            let buffer = self.buffers.get(&buffer_id);
            let file_name = buffer
                .map(|b| b.display_name())
                .unwrap_or_else(|| "[closed]".into());
            let line_preview = buffer
                .and_then(|b| {
                    if pos.line < b.line_count() {
                        Some(b.rope.line(pos.line).to_string().trim_end().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            entries.push(crate::popup::MarkEntry {
                name,
                buffer_id,
                file_name,
                line: pos.line,
                col: pos.col,
                line_preview,
            });
        }

        // Sort by mark name for consistent display
        entries.sort_by_key(|e| e.name);

        if entries.is_empty() {
            return CommandResult::Message("No marks set".into());
        }

        // Pre-select mark nearest to current cursor position
        let cursor_line = self
            .windows
            .active_window()
            .map(|w| w.cursor.position.line)
            .unwrap_or(0);
        let cursor_buf = self.windows.active_window().map(|w| w.buffer_id);

        let mut popup = crate::popup::MarkListPopup::new(entries);
        let mut best_idx = 0;
        let mut best_dist = usize::MAX;
        for (i, entry) in popup.entries.iter().enumerate() {
            if Some(entry.buffer_id) == cursor_buf {
                let dist = (cursor_line as isize - entry.line as isize).unsigned_abs();
                if dist < best_dist {
                    best_dist = dist;
                    best_idx = i;
                }
            }
        }
        popup.selected = best_idx;

        self.popup.mark_list = Some(popup);
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }
    /// Rebuild the shortcut popup showing only entries matching the current prefix.
    pub(crate) fn rebuild_shortcut_popup(&mut self) {
        let prefix = self.shortcut_pending_keys.clone();
        let prefix_len = prefix.len();

        let matching: Vec<_> = self
            .active_shortcuts
            .iter()
            .filter(|(keys, _)| keys.len() >= prefix_len && keys[..prefix_len] == prefix[..])
            .collect();

        let mode_name = self.mode.keybind_name();

        // Measure column widths
        let mut max_key_len = 0;
        let mut max_desc_len = 0;
        for (keys, action) in &matching {
            let key_str = crate::misc::format_shortcut_keys(keys);
            max_key_len = max_key_len.max(key_str.len());
            max_desc_len = max_desc_len.max(action.label().len());
        }

        let mut lines = Vec::new();
        for (keys, action) in &matching {
            let key_str = crate::misc::format_shortcut_keys(keys);
            let desc = action.label();

            let original_keys = self.keybinds.keys_for_action_in_mode(mode_name, action);
            let hint = if original_keys.is_empty() {
                String::new()
            } else {
                format!("[{}]", original_keys.join(", "))
            };

            lines.push(format!(
                "  {:<key_w$}  {:<desc_w$}  {}",
                key_str,
                desc,
                hint,
                key_w = max_key_len,
                desc_w = max_desc_len,
            ));
        }

        let prefix_str = crate::misc::format_shortcut_keys(&prefix);
        let title = if prefix_str.is_empty() {
            " Shortcuts ".to_string()
        } else {
            format!(" Shortcuts [{}] ", prefix_str)
        };

        self.popup.float = Some(FloatPopup::new(title, lines));
        self.dirty.mark_all();
    }
    /// Handle the `:vocab <word>` command.
    pub fn vocab_handle(&mut self, word: &str) -> CommandResult {
        let word = word.trim();
        if word.is_empty() {
            return CommandResult::Error("Usage: :vocab <word>".into());
        }

        if self.vocab.add(word) {
            if let Err(e) = self.vocab.save() {
                return CommandResult::Error(format!("Failed to save vocabulary: {}", e));
            }
            CommandResult::Message(format!("Added '{}' to vocabulary", word))
        } else {
            CommandResult::Message(format!("'{}' already in vocabulary", word))
        }
    }
}
