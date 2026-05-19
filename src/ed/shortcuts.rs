use crate::ed::visual::VisualExt;
use crate::editor::{CommandResult, Editor, FloatPopup, Mode};

/// Extension trait for the float-shortcuts menu.
pub trait ShortcutsExt {
    fn show_shortcuts(&mut self) -> CommandResult;
}

impl ShortcutsExt for Editor {
    fn show_shortcuts(&mut self) -> CommandResult {
        // Toggle off if already active
        if self.shortcut_active {
            self.popup.float = None;
            self.popup.overlay.float = None;
            self.shortcut_active = false;
            self.dirty.mark_all();
            return CommandResult::NoOp;
        }

        // Capture visual selection before anything clears the anchor
        if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
            self.shortcut_visual_context = self.get_selection_text();
        } else {
            self.shortcut_visual_context = None;
        }

        if self.active_shortcuts.is_empty() {
            self.set_status("No shortcuts configured. Add [shortcuts] to config.toml".into());
            return CommandResult::NoOp;
        }

        let mode_name = self.mode.keybind_name();

        let mut entries: Vec<_> = self.active_shortcuts.iter().collect();
        entries.sort_by(|a, b| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)));

        // Measure column widths for alignment
        let mut max_key_len = 0;
        let mut max_desc_len = 0;
        for (keys, action) in &entries {
            let k = crate::misc::format_shortcut_keys(keys);
            max_key_len = max_key_len.max(k.len());
            max_desc_len = max_desc_len.max(action.label().len());
        }

        // Build formatted lines with original keybinding hints
        let mut lines: Vec<String> = Vec::new();
        for (keys, action) in &entries {
            let k = crate::misc::format_shortcut_keys(keys);
            let desc = action.label();

            // Look up the original keybinding(s) for this action
            let original_keys = self.keybinds.keys_for_action_in_mode(mode_name, action);
            let hint = if original_keys.is_empty() {
                String::new()
            } else {
                format!("[{}]", original_keys.join(", "))
            };

            lines.push(format!(
                "  {:<key_w$}  {:<desc_w$}  {}",
                k,
                desc,
                hint,
                key_w = max_key_len,
                desc_w = max_desc_len,
            ));
        }

        let popup = FloatPopup::new(" Shortcuts ", lines);
        self.popup.float = Some(popup);
        self.popup.overlay.float = Some(crate::dirty::Rect { x: 0, y: 0, w: 0, h: 0 });
        self.shortcut_active = true;
        self.dirty.mark_all();

        CommandResult::ViewChanged
    }
}
