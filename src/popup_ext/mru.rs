// ════════════════════════════════════════════════════════════════════════
// MRU (Most Recently Used) POPUP
// ════════════════════════════════════════════════════════════════════════

use crate::mru::MruEntry;

#[derive(Debug, Clone)]
pub struct MruPopup {
    pub entries: Vec<MruEntry>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub scroll: usize,
    pub filter: String,
    /// When true, entries are sorted by open_count descending instead of recency.
    pub sort_by_frequency: bool,
}

impl MruPopup {
    pub fn new(entries: Vec<MruEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        MruPopup {
            entries,
            filtered,
            selected: 0,
            scroll: 0,
            filter: String::new(),
            sort_by_frequency: false,
        }
    }

    pub fn selected_entry(&self) -> Option<&MruEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&idx| self.entries.get(idx))
    }

    fn apply_filter(&mut self) {
        self.filtered.clear();
        let query = self.filter.to_lowercase();
        for (i, entry) in self.entries.iter().enumerate() {
            let file_name = entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let dir_str = entry
                .path
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();

            if query.is_empty()
                || file_name.to_lowercase().contains(&query)
                || dir_str.to_lowercase().contains(&query)
            {
                self.filtered.push(i);
            }
        }
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
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

    /// Toggle between recency and frequency sort order.
    /// Returns `true` if now sorting by frequency.
    pub fn toggle_sort(&mut self, mru: &crate::mru::MruManager) -> bool {
        self.sort_by_frequency = !self.sort_by_frequency;
        if self.sort_by_frequency {
            self.entries = mru.entries_by_frequency();
        } else {
            self.entries = mru.get_entries();
        }
        self.selected = 0;
        self.scroll = 0;
        self.apply_filter();
        self.sort_by_frequency
    }
}

impl Scrollable for MruPopup {
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
        23
    }
}

pub fn render_mru_popup(
    popup: &MruPopup,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_h = 6u16;
    let edit_h = term_height.saturating_sub(status_h);
    let popup_width = clamp_width(100, term_width, 4);
    let content_rows = clamp_height(20, edit_h.saturating_sub(4), 5) as usize;
    let popup_height = content_rows as u16 + 2;

    let (x, y) = centered_in_edit(popup_width, popup_height, term_width, term_height, status_h);
    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    // ── Title bar ──────────────────────────────────────────────────────
    let sort_label = if popup.sort_by_frequency {
        "by freq"
    } else {
        "recent"
    };
    let title = format!(
        " Recent Files ({}) {} ",
        sort_label,
        if popup.filtered.is_empty() {
            "(no match)".to_string()
        } else {
            format!("({}/{})", popup.filtered.len(), popup.entries.len())
        }
    );
    let title_style = BoxStyle::default()
        .with_title(title)
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
    let inner_width = popup_width.saturating_sub(2) as usize;
    let file_name_width: usize = 25;

    let mut scroll = popup.scroll;
    if !popup.filtered.is_empty() && popup.selected >= scroll + content_rows {
        scroll = popup.selected - content_rows + 1;
    }
    if popup.selected < scroll {
        scroll = popup.selected;
    }

    for i in 0..content_rows {
        let row_y = filter_y + 1 + i as u16;
        let entry_idx = scroll + i;

        if entry_idx < popup.filtered.len() {
            let real_idx = popup.filtered[entry_idx];
            let entry = &popup.entries[real_idx];
            let is_selected = entry_idx == popup.selected;
            let row_style = if is_selected {
                RowStyle::selected()
            } else {
                RowStyle::normal()
            };

            let file_stem_raw = entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string();

            let displayed_name = if str_width(&file_stem_raw) > file_name_width {
                let suffix: String = file_stem_raw
                    .chars()
                    .rev()
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let suffix_w = str_width(&suffix);
                let prefix_max_w = file_name_width.saturating_sub(3).saturating_sub(suffix_w);
                let prefix = truncate_to_width(&file_stem_raw, prefix_max_w);
                format!("{}...{}", prefix, suffix)
            } else {
                file_stem_raw.clone()
            };

            let idx_str = format!("{:>2}", entry_idx + 1);
            let pos_str = format!("{}:{}", entry.line + 1, entry.col + 1);

            // ── Right-aligned metadata ──
            let time_str = entry.relative_time();
            let count_str = if entry.open_count > 1 {
                format!("×{}", entry.open_count)
            } else {
                String::new()
            };
            let meta_text = if count_str.is_empty() {
                time_str.clone()
            } else {
                format!("{} {}", count_str, time_str)
            };
            let meta_w = str_width(&meta_text);

            // Calculate directory space after fixed columns + metadata
            let fixed_len = 4 + file_name_width + 2 + pos_str.len() + 1 + meta_w + 2;
            let dir_avail = inner_width.saturating_sub(fixed_len);

            let dir_str = entry
                .path
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();

            let dir_chars: Vec<char> = dir_str.chars().collect();
            let dir_display = if dir_chars.len() > dir_avail {
                let trunc_len = dir_avail.saturating_sub(3);
                if trunc_len > 0 {
                    let start = dir_chars.len() - trunc_len;
                    let truncated: String = dir_chars[start..].iter().collect();
                    format!("…{}", truncated)
                } else {
                    String::new()
                }
            } else {
                dir_str.clone()
            };

            let name_color = if is_selected {
                catppuccin::TEXT
            } else {
                catppuccin::BLUE
            };

            let mut segments = Vec::new();
            segments.push(Segment::new(&idx_str, catppuccin::OVERLAY0));
            segments.push(Segment::new("  ", catppuccin::SURFACE1));

            // File name with match highlighting (Unicode-safe)
            let (prefix, matched, suffix) = if !popup.filter.is_empty() {
                if let Some((start, end)) = case_insensitive_find(&displayed_name, &popup.filter) {
                    let p = displayed_name[..start].to_string();
                    let m = displayed_name[start..end].to_string();
                    let s = displayed_name[end..].to_string();
                    (p, m, s)
                } else {
                    (displayed_name.clone(), String::new(), String::new())
                }
            } else {
                (displayed_name.clone(), String::new(), String::new())
            };

            if !prefix.is_empty() {
                segments.push(Segment::new(&prefix, name_color));
            }
            if !matched.is_empty() {
                segments.push(Segment::new(&matched, catppuccin::PEACH));
            }
            if !suffix.is_empty() {
                segments.push(Segment::new(&suffix, name_color));
            }

            // Padding to fill file_name_width
            let displayed_w = str_width(&displayed_name);
            let padding = if displayed_w < file_name_width {
                " ".repeat(file_name_width.saturating_sub(displayed_w))
            } else {
                String::new()
            };
            if !padding.is_empty() {
                segments.push(Segment::new(&padding, catppuccin::SURFACE1));
            }

            segments.push(Segment::new("  ", catppuccin::SURFACE1));
            segments.push(Segment::new(&dir_display, catppuccin::OVERLAY0));
            segments.push(Segment::new(" ", catppuccin::SURFACE1));
            segments.push(Segment::new(&pos_str, catppuccin::YELLOW));

            // ── Right-aligned: pad + metadata ──
            let used_w: usize = segments.iter().map(|s| str_width(s.text)).sum();
            let remaining = inner_width.saturating_sub(used_w);

            if remaining > meta_w {
                let pad = remaining - meta_w;
                segments.push(Segment::new(&" ".repeat(pad), catppuccin::SURFACE1));
            }

            // Open count badge (only if > 1)
            if !count_str.is_empty() {
                segments.push(Segment::new(&count_str, catppuccin::PEACH));
                segments.push(Segment::new(" ", catppuccin::SURFACE1));
            }

            // Relative time
            segments.push(Segment::new(&time_str, catppuccin::OVERLAY0));

            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            draw_empty_row(stdout, x, row_y, popup_width, &RowStyle::normal())?;
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + content_rows as u16;
    let footer = format!(
        "[f] {}  [Del] remove  [Enter] open  [Esc]{}close  {}/{}",
        if popup.sort_by_frequency {
            "recency"
        } else {
            "freq"
        },
        if popup.filter.is_empty() {
            " "
        } else {
            " clear "
        },
        if popup.filtered.is_empty() {
            0
        } else {
            popup.selected + 1
        },
        popup.filtered.len(),
    );
    let footer_style = BoxStyle::default()
        .with_footer(footer)
        .with_bg(catppuccin::MANTLE);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &footer_style)?;

    execute!(stdout, ResetColor)?;
    Ok(())
}