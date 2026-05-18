//--+ popup_ext/help.rs
//! Help popup overlay for displaying keybindings and documentation.

use crate::popup::Scrollable;
use crate::rounded_box::{
    catppuccin, centered_in_edit, clamp_width, clear_rect, draw_border, draw_empty_row,
    draw_row_text, BoxStyle, RowStyle,
};
use crossterm::execute;
use crossterm::style::ResetColor;

/// Popup for displaying help text with scrollable content.
#[derive(Debug)]
pub struct HelpPopup {
    pub title: String,
    pub lines: Vec<String>,
    pub selected: usize,
    pub scroll: usize,
    pub width: u16,
    /// Total box height (including top and bottom borders).
    pub height: u16,
}

impl HelpPopup {
    pub fn new(lines: Vec<String>, term_width: u16, term_height: u16) -> Self {
        let status_h = 6u16;
        let edit_h = term_height.saturating_sub(status_h);
        let width = clamp_width(80, term_width, 4);
        let max_content = edit_h.saturating_sub(2);
        let content_rows = (lines.len() as u16).min(max_content).max(3);
        let height = content_rows + 2;
        HelpPopup {
            title: "Help".to_string(),
            lines,
            selected: 0,
            scroll: 0,
            width,
            height,
        }
    }

    /// Create a help popup with a custom title and pre-formatted lines.
    pub fn new_with_lines(title: String, lines: Vec<String>) -> Self {
        let width = 80u16;
        let content_rows = (lines.len() as u16).max(3).min(30);
        let height = content_rows + 2;
        HelpPopup {
            title,
            lines,
            selected: 0,
            scroll: 0,
            width,
            height,
        }
    }

    /// Re-calculate dimensions based on terminal size (call before render).
    pub fn resize(&mut self, term_width: u16, term_height: u16) {
        let status_h = 6u16;
        let edit_h = term_height.saturating_sub(status_h);
        self.width = clamp_width(80, term_width, 4);
        let max_content = edit_h.saturating_sub(2);
        let content_rows = (self.lines.len() as u16).min(max_content).max(3);
        self.height = content_rows + 2;
        // Re-clamp after height change via shared trait.
        <Self as Scrollable>::clamp_scroll(self);
    }
}

/// `visible_rows` reads `self.height` so `resize()` keeps it in sync
/// automatically — no separate `clamp_scroll(visible)` overload needed.
impl Scrollable for HelpPopup {
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
        self.lines.len()
    }
    fn visible_rows(&self) -> usize {
        self.height.saturating_sub(2) as usize
    }
}

pub fn render_help_popup(
    popup: &HelpPopup,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_h = 6u16;
    let (x, y) = centered_in_edit(popup.width, popup.height, term_width, term_height, status_h);

    clear_rect(stdout, x, y, popup.width, popup.height, catppuccin::MANTLE)?;

    let border_style = BoxStyle::default()
        .with_title(format!(" {} ", popup.title))
        .with_border(catppuccin::SURFACE0)
        .with_bg(catppuccin::MANTLE);
    draw_border(stdout, x, y, popup.width, popup.height, &border_style)?;

    let visible_content = popup.height.saturating_sub(2) as usize;

    let scroll = popup.scroll;

    for i in 0..visible_content {
        let row_y = y + 1 + i as u16;
        let line_idx = scroll + i;

        if line_idx < popup.lines.len() {
            let line = &popup.lines[line_idx];
            let is_selected = line_idx == popup.selected;
            let row_style = if is_selected {
                RowStyle::selected()
                    .with_border(catppuccin::SURFACE0)
                    .with_text(catppuccin::BLUE)
            } else {
                RowStyle::normal()
                    .with_border(catppuccin::SURFACE0)
                    .with_text(catppuccin::TEXT)
            };
            draw_row_text(stdout, x, row_y, popup.width, line, &row_style)?;
        } else {
            let empty_style = RowStyle::normal().with_border(catppuccin::SURFACE0);
            draw_empty_row(stdout, x, row_y, popup.width, &empty_style)?;
        }
    }

    execute!(stdout, ResetColor)?;
    Ok(())
}
