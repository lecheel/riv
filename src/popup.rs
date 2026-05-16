// src/popup.rs
//! Popup overlays: help viewer, buffer list, file picker, floating messages, etc.
//!
//! All list-based popups implement the [`Scrollable`] trait, which provides
//! shared `move_up`, `move_down`, and `clamp_scroll` logic so there is no
//! duplication across popup types.

use crate::rounded_box::*;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use std::path::{Path, PathBuf};

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
    let popup_width = clamp_width(90, term_width, 4);
    let content_rows = clamp_height(20, edit_h.saturating_sub(4), 5) as usize;
    let popup_height = content_rows as u16 + 4;

    let (x, y) = centered_in_edit(popup_width, popup_height, term_width, term_height, status_h);
    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    // ── Title bar ──────────────────────────────────────────────────────
    let title = format!(
        " Recent Files {} ",
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

            let fixed_len = 4 + file_name_width + 2 + pos_str.len() + 1;
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

            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            draw_empty_row(stdout, x, row_y, popup_width, &RowStyle::normal())?;
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + content_rows as u16;
    let footer = format!(
        "[Del] Remove  [Enter] open  [Esc]{}close  {}/{}",
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
// ════════════════════════════════════════════════════════════════════════
// HELP POPUP
// ════════════════════════════════════════════════════════════════════════

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

// ════════════════════════════════════════════════════════════════════════
// BUFFER LIST POPUP
// ════════════════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════════════════
// BUFFER LIST POPUP
// ════════════════════════════════════════════════════════════════════════

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
    let popup_height = visible_rows as u16 + 4; // title + filter + content + footer

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
            let row_style = if is_selected {
                RowStyle::selected()
            } else {
                RowStyle::normal()
            };

            let id_str = format!("{:>4}", entry.id);

            let mut segments = Vec::new();
            segments.push(Segment::new(
                if entry.dirty { "+" } else { " " },
                catppuccin::RED,
            ));
            segments.push(Segment::new(
                if entry.active { "%" } else { " " },
                catppuccin::GREEN,
            ));
            segments.push(Segment::new(" ", catppuccin::SURFACE1));
            segments.push(Segment::new(&id_str, catppuccin::YELLOW));
            segments.push(Segment::new("  ", catppuccin::SURFACE1));

            // Name with match highlighting when filter is active
            if !popup.filter.is_empty() {
                if let Some((match_start, match_end)) =
                    case_insensitive_find(&entry.name, &popup.filter)
                {
                    if match_start > 0 {
                        segments.push(Segment::new(
                            &entry.name[..match_start],
                            if is_selected {
                                catppuccin::TEXT
                            } else {
                                catppuccin::SUBTEXT
                            },
                        ));
                    }
                    segments.push(Segment::new(
                        &entry.name[match_start..match_end],
                        catppuccin::PEACH,
                    ));
                    if match_end < entry.name.len() {
                        segments.push(Segment::new(
                            &entry.name[match_end..],
                            if is_selected {
                                catppuccin::TEXT
                            } else {
                                catppuccin::SUBTEXT
                            },
                        ));
                    }
                } else {
                    segments.push(Segment::new(
                        &entry.name,
                        if is_selected {
                            catppuccin::TEXT
                        } else {
                            catppuccin::SUBTEXT
                        },
                    ));
                }
            } else {
                segments.push(Segment::new(
                    &entry.name,
                    if is_selected {
                        catppuccin::TEXT
                    } else {
                        catppuccin::SUBTEXT
                    },
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
        "[Enter] switch  [Esc] close  {}/{}",
        if popup.filtered.is_empty() {
            0
        } else {
            popup.selected + 1
        },
        popup.filtered.len(),
    );
    let bottom_style = BoxStyle::default()
        .with_border(catppuccin::SURFACE0)
        .with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    execute!(stdout, ResetColor)?;
    Ok(())
}

// ════════════════════════════════════════════════════════════════════════
// KEYMAP POPUP
// ════════════════════════════════════════════════════════════════════════

use crate::keybind::HelpEntry;

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
    let title = format!(
        " {} Keymap ({} bindings) ",
        popup.mode_name.to_uppercase(),
        total
    );
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
                    Segment::new(
                        &key_display,
                        if is_selected {
                            catppuccin::GREEN
                        } else {
                            catppuccin::LAVENDER
                        },
                    ),
                    Segment::new("  ", catppuccin::SURFACE1),
                    Segment::new(
                        &entry.action,
                        if is_selected {
                            catppuccin::TEXT
                        } else {
                            catppuccin::SUBTEXT
                        },
                    ),
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

// ════════════════════════════════════════════════════════════════════════
// MARK LIST POPUP
// ════════════════════════════════════════════════════════════════════════

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
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
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
            let row_style = if is_selected {
                RowStyle::selected()
            } else {
                RowStyle::normal()
            };

            let is_closed = entry.file_name == "[closed]";

            // Mark name: " a "
            let name_str = format!(" {} ", entry.name);
            let name_color = if is_closed {
                catppuccin::OVERLAY0
            } else {
                catppuccin::MAUVE
            };

            // File name (truncated to column width)
            let displayed_name: String = if str_width(&entry.file_name) > file_name_width {
                let truncated =
                    truncate_to_width(&entry.file_name, file_name_width.saturating_sub(1));
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
                if let Some((match_start, match_end)) =
                    case_insensitive_find(&displayed_name, &popup.filter)
                {
                    if match_start > 0 {
                        segments.push(Segment::new(&displayed_name[..match_start], file_color));
                    }
                    segments.push(Segment::new(
                        &displayed_name[match_start..match_end],
                        catppuccin::PEACH,
                    ));
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
            let pad = if padding > 0 {
                " ".repeat(padding)
            } else {
                String::new()
            };
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
        "[Enter] jump  [Del] remove  [Esc]{}close  {}/{}",
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

// ════════════════════════════════════════════════════════════════════════
// FILE PICKER
// ════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_parent: bool,
}

#[derive(Debug)]
pub struct FilePicker {
    pub all_entries: Vec<FileEntry>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub scroll: usize,
    pub filter: String,
    pub cwd: PathBuf,
    pub visible_height: usize,
}

impl FilePicker {
    pub fn new(initial_path: &Path) -> Self {
        let effective_cwd = if initial_path.is_file() {
            initial_path
                .parent()
                .map(|p| {
                    if p.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        p.to_path_buf()
                    }
                })
                .unwrap_or_else(|| PathBuf::from("."))
        } else if initial_path.as_os_str().is_empty() {
            PathBuf::from(".")
        } else if initial_path.is_dir() {
            initial_path.to_path_buf()
        } else {
            initial_path
                .parent()
                .and_then(|p| {
                    if p.as_os_str().is_empty() {
                        Some(PathBuf::from("."))
                    } else if p.is_dir() {
                        Some(p.to_path_buf())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| PathBuf::from("."))
        };

        let mut picker = FilePicker {
            all_entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            scroll: 0,
            filter: String::new(),
            cwd: effective_cwd,
            visible_height: 20,
        };
        picker.refresh_entries();
        picker
    }

    pub fn sync_visible_height(&mut self, term_height: u16) {
        let status_height: u16 = 6;
        let edit_height = term_height.saturating_sub(status_height);
        let max_content_rows = edit_height.saturating_sub(4) as usize;
        self.visible_height = self.filtered.len().min(max_content_rows).max(1);
    }

    pub fn refresh_entries(&mut self) {
        self.all_entries.clear();

        if self.can_go_up() {
            if let Some(parent) = self.cwd.parent() {
                self.all_entries.push(FileEntry {
                    name: "../".to_string(),
                    path: parent.to_path_buf(),
                    is_dir: true,
                    is_parent: true,
                });
            }
        }

        if let Ok(entries) = std::fs::read_dir(&self.cwd) {
            let mut dirs: Vec<FileEntry> = Vec::new();
            let mut files: Vec<FileEntry> = Vec::new();

            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = path.is_dir();
                let fe = FileEntry {
                    name: if is_dir { format!("{}/", name) } else { name },
                    path,
                    is_dir,
                    is_parent: false,
                };
                if is_dir {
                    dirs.push(fe);
                } else {
                    files.push(fe);
                }
            }

            dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            self.all_entries.extend(dirs);
            self.all_entries.extend(files);
        }

        self.apply_filter();
    }

    fn apply_filter(&mut self) {
        self.filtered.clear();
        let query = self.filter.to_lowercase();

        for (i, entry) in self.all_entries.iter().enumerate() {
            if entry.is_parent {
                self.filtered.push(i);
                continue;
            }
            if query.is_empty() || entry.name.to_lowercase().contains(&query) {
                self.filtered.push(i);
            }
        }

        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            let parent_pos = self.filtered.iter().position(|&idx| {
                self.all_entries
                    .get(idx)
                    .map(|e| e.is_parent)
                    .unwrap_or(false)
            });
            self.selected = parent_pos.unwrap_or(self.filtered.len() - 1);
        }

        // Use trait clamp_scroll after filter change.
        <Self as Scrollable>::clamp_scroll(self);
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.selected = 0;
        self.scroll = 0;
        self.apply_filter();
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

    pub fn handle_minus(&mut self) -> bool {
        if self.filter.is_empty() {
            self.go_up();
            true
        } else {
            false
        }
    }

    pub fn go_up(&mut self) {
        if let Some(parent) = self.cwd.parent() {
            let new_cwd = if parent.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                parent.to_path_buf()
            };
            self.cwd = new_cwd;
            self.filter.clear();
            self.selected = 0;
            self.scroll = 0;
            self.refresh_entries();
        }
    }

    pub fn go_into(&mut self, path: &Path) {
        if path.is_dir() {
            self.cwd = path.to_path_buf();
            self.filter.clear();
            self.selected = 0;
            self.scroll = 0;
            self.refresh_entries();
        }
    }

    pub fn selected_entry(&self) -> Option<&FileEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.all_entries.get(i))
    }

    pub fn can_go_up(&self) -> bool {
        self.cwd
            .parent()
            .map(|p| !p.as_os_str().is_empty())
            .unwrap_or(false)
    }

    pub fn cwd_display(&self) -> String {
        self.cwd.display().to_string()
    }
}

/// FilePicker uses `filtered.len()` as item count and `visible_height`
/// (synced by `sync_visible_height`) as the dynamic visible row count.
impl Scrollable for FilePicker {
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
        self.visible_height
    }
}

pub fn render_file_picker(
    picker: &FilePicker,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_h = 6u16;
    let edit_h = term_height.saturating_sub(status_h);
    let popup_width = clamp_width(80, term_width, 4);
    let content_rows = clamp_height(20, edit_h.saturating_sub(4), 5) as usize;
    // +4 = top border + filter row + bottom border + 1 spare
    let popup_height = content_rows as u16 + 4;

    let (x, y) = centered_in_edit(popup_width, popup_height, term_width, term_height, status_h);
    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    // ── Title bar ──────────────────────────────────────────────────────
    let title_style = BoxStyle::default()
        .with_title(format!(" File Picker {} ", picker.cwd_display()))
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    // ── Filter row ─────────────────────────────────────────────────────
    let filter_y = y + 1;
    {
        let filter_style = RowStyle::normal().with_bg(catppuccin::CRUST).no_padding();
        let prompt_w = str_width(">");
        let max_filter_len = content_width(popup_width, &filter_style).saturating_sub(prompt_w + 1);
        let filter_display = truncate_to_width(&picker.filter, max_filter_len);

        let segments = [
            Segment::new(">", catppuccin::PEACH),
            Segment::new(filter_display, catppuccin::TEXT),
        ];
        draw_row(stdout, x, filter_y, popup_width, &segments, &filter_style)?;

        // Block cursor after the filter text.
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
    let mut scroll = picker.scroll;
    if !picker.filtered.is_empty() && picker.selected >= scroll + content_rows {
        scroll = picker.selected - content_rows + 1;
    }
    if picker.selected < scroll {
        scroll = picker.selected;
    }

    for i in 0..content_rows {
        let row_y = filter_y + 1 + i as u16;
        let entry_idx = scroll + i;

        if entry_idx < picker.filtered.len() {
            let real_idx = picker.filtered[entry_idx];
            let entry = &picker.all_entries[real_idx];
            let is_selected = entry_idx == picker.selected;
            let row_style = if is_selected {
                RowStyle::selected()
            } else {
                RowStyle::normal()
            };

            let (icon, icon_color) = if entry.is_parent {
                ("← ", catppuccin::YELLOW)
            } else if entry.is_dir {
                ("+ ", catppuccin::MAUVE)
            } else {
                ("  ", catppuccin::SUBTEXT)
            };

            let name_color = if entry.is_parent {
                if is_selected {
                    catppuccin::YELLOW
                } else {
                    catppuccin::OVERLAY0
                }
            } else if entry.is_dir {
                if is_selected {
                    catppuccin::BLUE
                } else {
                    catppuccin::MAUVE
                }
            } else if is_selected {
                catppuccin::TEXT
            } else {
                catppuccin::SUBTEXT
            };

            let segments = [
                Segment::new(icon, icon_color),
                Segment::new(&entry.name, name_color),
            ];
            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            draw_empty_row(stdout, x, row_y, popup_width, &RowStyle::normal())?;
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + content_rows as u16;
    let footer_text = format!(
        "[-] up  [Enter] open  [Esc]{}close  {}/{}",
        if picker.filter_is_empty() {
            " "
        } else {
            " clear "
        },
        picker.selected + 1,
        picker.filtered.len(),
    );
    let footer_style = BoxStyle::default()
        .with_footer(footer_text)
        .with_bg(catppuccin::MANTLE);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &footer_style)?;

    execute!(stdout, ResetColor)?;
    Ok(())
}

/// Case-insensitive substring search returning safe byte offsets in the original `haystack`.
pub fn case_insensitive_find(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let needle_lower: Vec<char> = needle.to_lowercase().chars().collect();
    let n_len = needle_lower.len();
    if n_len == 0 {
        return None;
    }
    let hay_chars: Vec<char> = haystack.chars().collect();
    if hay_chars.len() < n_len {
        return None;
    }
    for i in 0..=hay_chars.len() - n_len {
        let window: String = hay_chars[i..i + n_len].iter().collect();
        if window
            .to_lowercase()
            .chars()
            .eq(needle_lower.iter().cloned())
        {
            let start_byte = hay_chars[..i].iter().collect::<String>().len();
            let end_byte = hay_chars[..i + n_len].iter().collect::<String>().len();
            return Some((start_byte, end_byte));
        }
    }
    None
}
