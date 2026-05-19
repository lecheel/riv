//! Keymap popup overlay for displaying mode-specific keybindings.

use crate::keybind::HelpEntry;
use crate::popup::Scrollable;
use crate::rounded_box::{
    catppuccin, centered_in_edit, clamp_height, clamp_width, clear_rect, draw_border, draw_empty_row, draw_row, draw_row_text, BoxStyle,
    RowStyle, Segment,
};
use crossterm::execute;
use crossterm::style::ResetColor;

#[derive(Debug, Clone)]
pub struct KeymapEntry {
    pub keys: String,
    pub action: String,
    pub is_header: bool,
}

#[derive(Debug)]
pub struct KeymapPopup {
    pub mode_name: String,
    pub entries: Vec<KeymapEntry>,
    pub selected: usize,
    pub scroll: usize,
}

impl KeymapPopup {
    pub fn new(mode_name: String, help_entries: Vec<HelpEntry>) -> Self {
        let mut entries = Vec::new();
        let mut last_category: Option<String> = None;

        for entry in &help_entries {
            let cat_label = format!("{:?}", entry.category);
            if last_category.as_ref() != Some(&cat_label) {
                entries.push(KeymapEntry {
                    keys: String::new(),
                    action: cat_label.clone(),
                    is_header: true,
                });
                last_category = Some(cat_label);
            }
            entries.push(KeymapEntry {
                keys: entry.keys.clone(),
                action: entry.description.clone(),
                is_header: false,
            });
        }

        let selected = entries.iter().position(|e| !e.is_header).unwrap_or(0);
        KeymapPopup {
            mode_name,
            entries,
            selected,
            scroll: 0,
        }
    }

    pub fn total_bindings(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_header).count()
    }

    /// Override: skip header rows while navigating up.
    pub fn move_up(&mut self) {
        if self.selected == 0 {
            return;
        }
        self.selected -= 1;
        while self.selected > 0 && self.entries[self.selected].is_header {
            self.selected -= 1;
        }
        <Self as Scrollable>::clamp_scroll(self);
    }

    /// Override: skip header rows while navigating down.
    pub fn move_down(&mut self) {
        if self.selected + 1 >= self.entries.len() {
            return;
        }
        self.selected += 1;
        while self.selected + 1 < self.entries.len() && self.entries[self.selected].is_header {
            self.selected += 1;
        }
        <Self as Scrollable>::clamp_scroll(self);
    }
}

impl Scrollable for KeymapPopup {
    fn selected(&self) -> usize {
        self.selected
    }
    fn selected_mut(&mut self) -> &mut usize {
        &mut self.selected
    }
    fn scroll_mut(&mut self) -> &mut usize {
        &mut self.scroll
    }
    fn len(&self) -> usize {
        self.entries.len()
    }
    fn visible_rows(&self) -> usize {
        20
    }
}

pub fn render_keymap_popup(
    popup: &KeymapPopup,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_h = 6u16;
    let edit_h = term_height.saturating_sub(status_h);
    let popup_width = clamp_width(78, term_width, 4);
    let visible_rows = clamp_height(20, edit_h.saturating_sub(2), 3) as usize;
    let popup_height = visible_rows as u16 + 2;

    let (x, y) = centered_in_edit(popup_width, popup_height, term_width, term_height, status_h);
    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    let total = popup.total_bindings();
    let title = format!(" {} Keymap ({} bindings) ", popup.mode_name.to_uppercase(), total);
    let border_style = BoxStyle::default()
        .with_title(title)
        .with_border(catppuccin::SURFACE0)
        .with_bg(catppuccin::MANTLE)
        .with_footer("[Esc] close");
    draw_border(stdout, x, y, popup_width, popup_height, &border_style)?;

    let key_col_width: usize = 20;

    let scroll = popup.scroll;
    for i in 0..visible_rows {
        let row_y = y + 1 + i as u16;
        let entry_idx = scroll + i;

        if entry_idx < popup.entries.len() {
            let entry = &popup.entries[entry_idx];
            let is_selected = entry_idx == popup.selected;

            if entry.is_header {
                let row_style = RowStyle::normal()
                    .with_border(catppuccin::SURFACE0)
                    .with_bg(catppuccin::CRUST)
                    .with_text(catppuccin::MAUVE)
                    .no_padding();
                let header_text = format!(
                    "  ── {} ──{}",
                    entry.action,
                    "─".repeat((popup_width as usize).saturating_sub(entry.action.len() + 8))
                );
                draw_row_text(stdout, x, row_y, popup_width, &header_text, &row_style)?;
            } else {
                let row_style = if is_selected {
                    RowStyle::selected()
                        .with_border(catppuccin::SURFACE0)
                        .with_bg(catppuccin::SURFACE0)
                        .with_text(catppuccin::TEXT)
                } else {
                    RowStyle::normal()
                        .with_border(catppuccin::SURFACE0)
                        .with_bg(catppuccin::MANTLE)
                        .with_text(catppuccin::TEXT)
                };

                let key_display = if entry.keys.chars().count() > key_col_width {
                    let trunc: String = entry.keys.chars().take(key_col_width - 1).collect();
                    format!("{:<width$} ", trunc, width = key_col_width - 1)
                } else {
                    format!("{:<width$}", entry.keys, width = key_col_width)
                };

                let segments = [
                    Segment::new(&key_display, if is_selected { catppuccin::GREEN } else { catppuccin::LAVENDER }),
                    Segment::new("  ", catppuccin::SURFACE1),
                    Segment::new(&entry.action, if is_selected { catppuccin::TEXT } else { catppuccin::SUBTEXT }),
                ];
                draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
            }
        } else {
            let empty_style = RowStyle::normal().with_border(catppuccin::SURFACE0);
            draw_empty_row(stdout, x, row_y, popup_width, &empty_style)?;
        }
    }

    execute!(stdout, ResetColor)?;
    Ok(())
}
