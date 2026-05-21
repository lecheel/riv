//! Mark list popup overlay for quick navigation of set marks (a-z).

use crate::popup::{case_insensitive_find, Scrollable};
use crate::rounded_box::{
    catppuccin, centered_in_edit, clamp_height, clamp_width, clear_rect, content_width, draw_bottom_border, draw_empty_row, draw_row,
    draw_top_border, str_width, truncate_to_width, BoxStyle, RowStyle, Segment,
};
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};

/// A single mark entry for the marks popup.
#[derive(Debug, Clone)]
pub struct MarkEntry {
    /// Mark character (a-z).
    pub name: char,
    /// Buffer ID where the mark is set.
    pub buffer_id: u64,
    /// Display name of the file (or "[closed]" if buffer was dropped).
    pub file_name: String,
    /// 0-indexed line number.
    pub line: usize,
    /// 0-indexed column number.
    pub col: usize,
    /// Preview of the marked line content (trimmed).
    pub line_preview: String,
}

/// Popup that lists all set marks (a-z) for quick navigation.
#[derive(Debug, Clone)]
pub struct MarkListPopup {
    pub entries: Vec<MarkEntry>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub scroll: usize,
    pub filter: String,
}

impl MarkListPopup {
    pub fn new(entries: Vec<MarkEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        Self {
            entries,
            filtered,
            selected: 0,
            scroll: 0,
            filter: String::new(),
        }
    }

    pub fn selected_entry(&self) -> Option<&MarkEntry> {
        self.filtered.get(self.selected).and_then(|&i| self.entries.get(i))
    }

    fn apply_filter(&mut self) {
        self.filtered.clear();
        let query = self.filter.to_lowercase();
        for (i, entry) in self.entries.iter().enumerate() {
            if query.is_empty()
                || entry.name.to_string().contains(&query)
                || entry.file_name.to_lowercase().contains(&query)
                || entry.line_preview.to_lowercase().contains(&query)
            {
                self.filtered.push(i);
            }
        }
        if self.selected >= self.filtered.len() && !self.filtered.is_empty() {
            self.selected = self.filtered.len() - 1;
        }
        <Self as Scrollable>::clamp_scroll(self);
    }

    pub fn filter_push(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
        self.scroll = 0;
        self.apply_filter();
    }

    pub fn filter_pop(&mut self) {
        self.filter.pop();
        self.selected = 0;
        self.scroll = 0;
        self.apply_filter();
    }

    pub fn remove_selected(&mut self) {
        if let Some(&real_idx) = self.filtered.get(self.selected) {
            self.entries.remove(real_idx);
            self.apply_filter();
        }
    }
}

impl Scrollable for MarkListPopup {
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
        self.filtered.len()
    }
    fn visible_rows(&self) -> usize {
        15
    }
}

pub fn render_mark_list_popup(
    popup: &MarkListPopup,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_h = 6u16;
    let edit_h = term_height.saturating_sub(status_h);
    let popup_width = clamp_width(80, term_width, 4);
    let content_rows = clamp_height(15, edit_h.saturating_sub(4), 5) as usize;
    let popup_height = content_rows as u16 + 4; // title + filter + content + footer

    let (x, y) = centered_in_edit(popup_width, popup_height, term_width, term_height, status_h);
    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    // ── Title bar ──────────────────────────────────────────────────────
    let title = format!(
        " Marks {} ",
        if popup.filtered.is_empty() {
            "(no match)".to_string()
        } else {
            format!("({}/{})", popup.filtered.len(), popup.entries.len())
        }
    );
    let title_style = BoxStyle::default().with_title(title).with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    // ── Filter row ─────────────────────────────────────────────────────
    let filter_y = y + 1;
    {
        let filter_style = RowStyle::normal().with_bg(catppuccin::CRUST).no_padding();
        let prompt_w = str_width(">");
        let max_filter_len = content_width(popup_width, &filter_style).saturating_sub(prompt_w + 1);
        let filter_display = truncate_to_width(&popup.filter, max_filter_len);

        let segments = [Segment::new(">", catppuccin::PEACH), Segment::new(filter_display, catppuccin::TEXT)];
        draw_row(stdout, x, filter_y, popup_width, &segments, &filter_style)?;

        // Block cursor after the filter text
        let cursor_x = x as usize + 1 + prompt_w + str_width(filter_display);
        if (cursor_x as u16) < x + popup_width.saturating_sub(1) {
            execute!(stdout, MoveTo(cursor_x as u16, filter_y))?;
            execute!(
                stdout,
                SetBackgroundColor(catppuccin::TEXT),
                SetForegroundColor(catppuccin::CRUST),
                Print(" ")
            )?;
        }
    }

    // ── Content rows ───────────────────────────────────────────────────
    let file_name_width: usize = 24;
    let scroll = popup.scroll;

    for i in 0..content_rows {
        let row_y = filter_y + 1 + i as u16;
        let entry_idx = scroll + i;

        if entry_idx < popup.filtered.len() {
            let real_idx = popup.filtered[entry_idx];
            let entry = &popup.entries[real_idx];
            let is_selected = entry_idx == popup.selected;
            let row_style = if is_selected { RowStyle::selected() } else { RowStyle::normal() };

            let is_closed = entry.file_name == "[closed]";

            // Mark name: " a "
            let name_str = format!(" {} ", entry.name);
            let name_color = if is_closed { catppuccin::OVERLAY0 } else { catppuccin::MAUVE };

            // File name (truncated to column width)
            let displayed_name: String = if str_width(&entry.file_name) > file_name_width {
                let truncated = truncate_to_width(&entry.file_name, file_name_width.saturating_sub(1));
                format!("{}…", truncated)
            } else {
                entry.file_name.clone()
            };

            let file_color = if is_closed {
                catppuccin::OVERLAY0
            } else if is_selected {
                catppuccin::TEXT
            } else {
                catppuccin::BLUE
            };

            // Position
            let pos_str = format!("{}:{}", entry.line + 1, entry.col + 1);

            let mut segments = Vec::new();
            segments.push(Segment::new(&name_str, name_color));

            // File name with optional match highlighting
            if !popup.filter.is_empty() && !is_closed {
                if let Some((match_start, match_end)) = case_insensitive_find(&displayed_name, &popup.filter) {
                    if match_start > 0 {
                        segments.push(Segment::new(&displayed_name[..match_start], file_color));
                    }
                    segments.push(Segment::new(&displayed_name[match_start..match_end], catppuccin::PEACH));
                    if match_end < displayed_name.len() {
                        segments.push(Segment::new(&displayed_name[match_end..], file_color));
                    }
                } else {
                    segments.push(Segment::new(&displayed_name, file_color));
                }
            } else {
                segments.push(Segment::new(&displayed_name, file_color));
            }

            // Padding to fill file_name_width
            let displayed_w = str_width(&displayed_name);
            let padding = file_name_width.saturating_sub(displayed_w);
            let pad = if padding > 0 { " ".repeat(padding) } else { String::new() };
            if !pad.is_empty() {
                segments.push(Segment::new(&pad, catppuccin::SURFACE1));
            }
            segments.push(Segment::new("  ", catppuccin::SURFACE1));
            segments.push(Segment::new(&pos_str, catppuccin::YELLOW));
            segments.push(Segment::new(" ", catppuccin::SURFACE1));

            // Line preview (dimmed)
            if !entry.line_preview.is_empty() {
                let preview_color = if is_closed {
                    catppuccin::OVERLAY0
                } else if is_selected {
                    catppuccin::SUBTEXT
                } else {
                    catppuccin::OVERLAY0
                };
                segments.push(Segment::new(&entry.line_preview, preview_color));
            }

            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            draw_empty_row(stdout, x, row_y, popup_width, &RowStyle::normal())?;
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + content_rows as u16;
    let footer = format!(
        "[Enter] jump  [Del] remove  [Esc]{}close  {}/{}",
        if popup.filter.is_empty() { " " } else { " clear " },
        if popup.filtered.is_empty() { 0 } else { popup.selected + 1 },
        popup.filtered.len(),
    );
    let footer_style = BoxStyle::default().with_footer(footer).with_bg(catppuccin::MANTLE);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &footer_style)?;

    execute!(stdout, ResetColor)?;
    Ok(())
}
