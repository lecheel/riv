//! Function list popup overlay for quick navigation of functions/methods in a buffer.
use crate::popup::Scrollable;
use crate::rounded_box::*;
use crate::Editor;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::Print;
use crossterm::style::ResetColor;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetForegroundColor;

/// A single function/method entry found in the buffer.
#[derive(Debug, Clone)]
pub struct FunctionEntry {
    /// Short keyword prefix: "fn", "pub fn", "async fn", "def", "function", etc.
    pub kind: String,
    /// Function/method name.
    pub name: String,
    /// Brief signature snippet (args + return) for the popup detail column.
    pub signature: String,
    /// 0-indexed line where the function begins.
    pub line: usize,
}

/// Popup that lists all functions/methods in the current buffer for quick
/// navigation.  Modeled after `BufferListPopup` / `MruPopup`.
#[derive(Debug, Clone)]
pub struct FunctionListPopup {
    pub all_entries: Vec<FunctionEntry>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub scroll: usize,
    pub filter: String,
}

impl FunctionListPopup {
    pub fn new(entries: Vec<FunctionEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        Self {
            all_entries: entries,
            filtered,
            selected: 0,
            scroll: 0,
            filter: String::new(),
        }
    }

    pub fn selected_entry(&self) -> Option<&FunctionEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.all_entries.get(i))
    }

    fn apply_filter(&mut self) {
        self.filtered.clear();
        let query = self.filter.to_lowercase();
        for (i, entry) in self.all_entries.iter().enumerate() {
            if query.is_empty()
                || entry.name.to_lowercase().contains(&query)
                || entry.kind.to_lowercase().contains(&query)
                || entry.signature.to_lowercase().contains(&query)
            {
                self.filtered.push(i);
            }
        }
        if self.selected >= self.filtered.len() && !self.filtered.is_empty() {
            self.selected = self.filtered.len() - 1;
        }
        self.clamp_scroll(15);
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

    pub fn filter_clear(&mut self) {
        self.filter.clear();
        self.selected = 0;
        self.scroll = 0;
        self.apply_filter();
    }

    pub fn filter_is_empty(&self) -> bool {
        self.filter.is_empty()
    }

    pub fn clamp_scroll(&mut self, visible_height: usize) {
        if self.scroll > self.selected {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll + visible_height {
            self.scroll = self.selected - visible_height + 1;
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.clamp_scroll(15);
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
            self.clamp_scroll(15);
        }
    }
}

impl Scrollable for FunctionListPopup {
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

pub fn render_function_list_popup(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let popup = match &editor.popup.function_list {
        Some(p) => p,
        None => return Ok(()),
    };

    let status_height = 3;
    let max_visible = 24usize;
    let visible_count = popup.filtered.len().min(max_visible);

    // Compute column widths from filtered entries
    let mut max_kind = 12usize;
    let mut max_name = 30usize;
    for &idx in popup.filtered.iter() {
        if let Some(entry) = popup.all_entries.get(idx) {
            max_kind = max_kind.max(entry.kind.len());
            max_name = max_name.max(entry.name.len());
        }
    }
    let calc_width = max_kind + 1 + max_name + 3 + 6 + 4;
    let popup_width = calc_width.max(130).min(term_width as usize) as u16;
    let popup_height = max_visible as u16 + 4;

    let x = (term_width.saturating_sub(popup_width)) / 2;
    let y = (term_height
        .saturating_sub(status_height)
        .saturating_sub(popup_height))
        / 2;

    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    // ── Title bar ──────────────────────────────────────────────────────
    let title_style = BoxStyle::default()
        .with_title(format!(
            " Functions {} ",
            if popup.filtered.is_empty() {
                "(no match)".to_string()
            } else {
                format!("({})", popup.filtered.len())
            }
        ))
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    // ── Filter row ─────────────────────────────────────────────────────
    let filter_y = y + 1;
    {
        let filter_style = RowStyle::normal().with_bg(catppuccin::CRUST).no_padding();
        let prompt_w = str_width(">");
        let max_filter_len = content_width(popup_width, &filter_style).saturating_sub(prompt_w + 1);
        let filter_display = truncate_to_width(&popup.filter, max_filter_len);

        let segments = [
            Segment::new(">", catppuccin::PEACH),
            Segment::new(filter_display, catppuccin::TEXT),
        ];
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
    let scroll_offset = if popup.selected >= max_visible {
        popup.selected - max_visible + 1
    } else {
        0
    };

    for i in 0..max_visible {
        let row_y = filter_y + 1 + i as u16;
        if row_y >= y + popup_height - 1 {
            break;
        }
        let entry_idx = scroll_offset + i;

        if entry_idx < popup.filtered.len() {
            let real_idx = popup.filtered[entry_idx];
            let entry = match popup.all_entries.get(real_idx) {
                Some(e) => e,
                None => {
                    let empty_style = RowStyle::normal().with_border(catppuccin::SURFACE2);
                    draw_empty_row(stdout, x, row_y, popup_width, &empty_style)?;
                    continue;
                }
            };
            let is_selected = entry_idx == popup.selected;

            let line_label = format!("{:>5}", entry.line + 1);
            let kind_str = format!("{:>width$} ", entry.kind, width = max_kind);
            let line_str = format!(" {}", line_label);

            let mut segments = Vec::new();
            // Kind keyword
            segments.push(Segment::new(&kind_str, catppuccin::MAUVE));
            // Function name — highlight matching substring
            if !popup.filter.is_empty() {
                let lower_name = entry.name.to_lowercase();
                let lower_query = popup.filter.to_lowercase();
                if let Some(match_start) = lower_name.find(&lower_query) {
                    let match_end = match_start + lower_query.len();
                    if match_start > 0 {
                        segments.push(Segment::new(
                            &entry.name[..match_start],
                            if is_selected {
                                catppuccin::TEXT
                            } else {
                                catppuccin::BLUE
                            },
                        ));
                    }
                    segments.push(Segment::new(
                        &entry.name[match_start..match_end.min(entry.name.len())],
                        catppuccin::PEACH,
                    ));
                    if match_end < entry.name.len() {
                        segments.push(Segment::new(
                            &entry.name[match_end..],
                            if is_selected {
                                catppuccin::TEXT
                            } else {
                                catppuccin::BLUE
                            },
                        ));
                    }
                } else {
                    segments.push(Segment::new(
                        &entry.name,
                        if is_selected {
                            catppuccin::TEXT
                        } else {
                            catppuccin::BLUE
                        },
                    ));
                }
            } else {
                segments.push(Segment::new(
                    &entry.name,
                    if is_selected {
                        catppuccin::TEXT
                    } else {
                        catppuccin::BLUE
                    },
                ));
            }
            // Signature snippet (dimmed)
            if !entry.signature.is_empty() {
                segments.push(Segment::new("  ", catppuccin::SURFACE1));
                segments.push(Segment::new(&entry.signature, catppuccin::OVERLAY0));
            }
            // Line number
            segments.push(Segment::new(&line_str, catppuccin::OVERLAY0));

            let row_style = if is_selected {
                RowStyle::selected()
                    .with_border(catppuccin::SURFACE2)
                    .with_bg(catppuccin::SURFACE0)
            } else {
                RowStyle::normal()
                    .with_border(catppuccin::SURFACE2)
                    .with_bg(catppuccin::MANTLE)
            };
            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            let empty_style = RowStyle::normal().with_border(catppuccin::SURFACE2);
            draw_empty_row(stdout, x, row_y, popup_width, &empty_style)?;
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + max_visible as u16;
    let footer_text = format!(
        "[Esc] close  [Enter] jump  {}/{}",
        if popup.filtered.is_empty() {
            0
        } else {
            popup.selected + 1
        },
        popup.filtered.len(),
    );
    let bottom_style = BoxStyle::default()
        .with_border(catppuccin::SURFACE2)
        .with_footer(footer_text);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    execute!(stdout, ResetColor)?;
    Ok(())
}
