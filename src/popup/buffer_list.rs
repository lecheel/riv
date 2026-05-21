//! Buffer list popup overlay for switching between open buffers.

use crate::popup::{case_insensitive_find, Scrollable};
use crate::rounded_box::{
    catppuccin, centered_in_edit, clamp_height, clamp_width, clear_rect, content_width, draw_bottom_border, draw_empty_row, draw_row,
    draw_top_border, str_width, truncate_to_width, BoxStyle, RowStyle, Segment,
};
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};

#[derive(Debug, Clone)]
pub struct BufferListEntry {
    pub id: u64,
    pub name: String,
    pub dirty: bool,
    pub active: bool,
}

#[derive(Debug)]
pub struct BufferListPopup {
    pub entries: Vec<BufferListEntry>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub scroll: usize,
    pub filter: String,
}

impl BufferListPopup {
    pub fn new(entries: Vec<BufferListEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        let selected = entries.iter().position(|e| e.active).unwrap_or(0);
        BufferListPopup {
            entries,
            filtered,
            selected,
            scroll: 0,
            filter: String::new(),
        }
    }

    pub fn selected_buffer_id(&self) -> Option<u64> {
        self.filtered
            .get(self.selected)
            .and_then(|&idx| self.entries.get(idx))
            .map(|e| e.id)
    }

    pub fn apply_filter(&mut self) {
        self.filtered.clear();
        let query = self.filter.to_lowercase();
        for (i, entry) in self.entries.iter().enumerate() {
            if query.is_empty() || entry.name.to_lowercase().contains(&query) {
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
}

impl Scrollable for BufferListPopup {
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
        12
    }
}

pub fn render_buffer_list_popup(
    popup: &BufferListPopup,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_h = 6u16;
    let edit_h = term_height.saturating_sub(status_h);
    let popup_width = clamp_width(70, term_width, 4);
    let visible_rows = clamp_height(12, edit_h.saturating_sub(4), 3) as usize;
    let popup_height = visible_rows as u16 + 3; // title + filter + footer

    let (x, y) = centered_in_edit(popup_width, popup_height, term_width, term_height, status_h);
    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    // ── Title bar ──────────────────────────────────────────────────────
    let title = format!(
        " Buffers {} ",
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
    let scroll = popup.scroll;
    for i in 0..visible_rows {
        let row_y = filter_y + 1 + i as u16;
        let entry_idx = scroll + i;

        if entry_idx < popup.filtered.len() {
            let real_idx = popup.filtered[entry_idx];
            let entry = &popup.entries[real_idx];
            let is_selected = entry_idx == popup.selected;
            let row_style = if is_selected { RowStyle::selected() } else { RowStyle::normal() };

            let id_str = format!("{:>4}", entry.id);

            let mut segments = Vec::new();
            segments.push(Segment::new(if entry.dirty { "+" } else { " " }, catppuccin::RED));
            segments.push(Segment::new(if entry.active { "%" } else { " " }, catppuccin::GREEN));
            segments.push(Segment::new(" ", catppuccin::SURFACE1));
            segments.push(Segment::new(&id_str, catppuccin::YELLOW));
            segments.push(Segment::new("  ", catppuccin::SURFACE1));

            // Name with match highlighting when filter is active
            if !popup.filter.is_empty() {
                if let Some((match_start, match_end)) = case_insensitive_find(&entry.name, &popup.filter) {
                    if match_start > 0 {
                        segments.push(Segment::new(
                            &entry.name[..match_start],
                            if is_selected { catppuccin::TEXT } else { catppuccin::SUBTEXT },
                        ));
                    }
                    segments.push(Segment::new(&entry.name[match_start..match_end], catppuccin::PEACH));
                    if match_end < entry.name.len() {
                        segments.push(Segment::new(
                            &entry.name[match_end..],
                            if is_selected { catppuccin::TEXT } else { catppuccin::SUBTEXT },
                        ));
                    }
                } else {
                    segments.push(Segment::new(
                        &entry.name,
                        if is_selected { catppuccin::TEXT } else { catppuccin::SUBTEXT },
                    ));
                }
            } else {
                segments.push(Segment::new(
                    &entry.name,
                    if is_selected { catppuccin::TEXT } else { catppuccin::SUBTEXT },
                ));
            }

            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            draw_empty_row(stdout, x, row_y, popup_width, &RowStyle::normal())?;
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + visible_rows as u16;
    let footer = format!(
        "[Enter] switch  [Esc] close  {}/{}",
        if popup.filtered.is_empty() { 0 } else { popup.selected + 1 },
        popup.filtered.len(),
    );
    let bottom_style = BoxStyle::default().with_border(catppuccin::SURFACE0).with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    execute!(stdout, ResetColor)?;
    Ok(())
}
