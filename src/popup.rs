// src/popup.rs
//! Popup overlays: help viewer, buffer list, file picker, floating messages, etc.
//!
//! All list-based popups implement the [`Scrollable`] trait, which provides
//! shared `move_up`, `move_down`, and `clamp_scroll` logic so there is no
//! duplication across popup types.

pub use crate::popup_ext::{
    render_buffer_list_popup, render_help_popup, render_keymap_popup, render_mark_list_popup,
    render_tag_list_popup, BufferListEntry, BufferListPopup, FunctionEntry, FunctionListPopup,
    HelpPopup, KeymapEntry, KeymapPopup, MarkEntry, MarkListPopup, TagListPopup,
};
use crate::rounded_box::*;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use std::path::{Path, PathBuf};

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

    // Fixed column display widths for right-aligned metadata
    let pos_w = 8;
    let count_w = 5;
    let time_w = 9;

    // Fixed-width zones (display-columns):
    //   left:  [2] idx + [2] gap + [file_name_width] name + [2] gap
    //   right: [1] sep + [pos_w] pos + [1] sep + [count_w] cnt + [1] sep + [time_w] time
    let left_w = 2 + 2 + file_name_width + 2;
    let meta_right_w = 1 + pos_w + 1 + count_w + 1 + time_w;
    let dir_field_w = inner_width.saturating_sub(left_w + meta_right_w);

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

            // Truncate filename if too long, adding '…'
            let displayed_name_raw = if str_width(&file_stem_raw) > file_name_width {
                let mut s = truncate_to_width(&file_stem_raw, file_name_width.saturating_sub(1))
                    .to_string();
                s.push('…');
                s
            } else {
                file_stem_raw.clone()
            };

            let idx_str = format!("{:>2}", entry_idx + 1);

            // ── Metadata: all right-aligned within their fixed-width columns ──
            let pos_raw = format!("{}:{}", entry.line + 1, entry.col + 1);
            let pos_pad = " ".repeat(pos_w.saturating_sub(str_width(&pos_raw)));
            let pos_col = format!("{}{}", pos_pad, pos_raw); // right-align: pad then value

            let count_raw = if entry.open_count > 1 {
                format!("×{}", entry.open_count)
            } else {
                String::new()
            };
            let count_pad = " ".repeat(count_w.saturating_sub(str_width(&count_raw)));
            let count_col = format!("{}{}", count_pad, count_raw); // right-align

            let time_raw = entry.relative_time();
            let time_pad = " ".repeat(time_w.saturating_sub(str_width(&time_raw)));
            let time_col = format!("{}{}", time_pad, time_raw); // right-align

            // ── Directory: compress then pad to exactly dir_field_w ──────────
            let dir_str = entry
                .path
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();

            let compressed = if str_width(&dir_str) <= dir_field_w {
                dir_str.clone()
            } else if dir_field_w > 0 {
                // Fish-style compress: /opt/proj/riv/src → /o/p/r/src
                let mut c = String::new();
                let mut first = true;
                for part in dir_str.split('/') {
                    if !first {
                        c.push('/');
                    }
                    first = false;
                    if part.is_empty() {
                        continue;
                    }
                    if let Some(ch) = part.chars().next() {
                        c.push(ch);
                    }
                }
                if str_width(&c) <= dir_field_w {
                    c
                } else {
                    // Still too wide — truncate from the left
                    let trunc = dir_field_w.saturating_sub(1);
                    if trunc == 0 {
                        String::new()
                    } else {
                        let chars: Vec<char> = c.chars().collect();
                        let start = chars.len().saturating_sub(trunc);
                        format!("…{}", chars[start..].iter().collect::<String>())
                    }
                }
            } else {
                String::new()
            };

            // Pad dir to full field width so metadata is always right-anchored
            let dir_w = str_width(&compressed);
            let dir_display = if dir_w < dir_field_w {
                format!("{}{}", compressed, " ".repeat(dir_field_w - dir_w))
            } else {
                compressed
            };

            // ── Match highlighting in filename ────────────────────────────────
            let name_color = if is_selected {
                catppuccin::TEXT
            } else {
                catppuccin::BLUE
            };

            let (prefix, matched, suffix): (String, String, String) = if !popup.filter.is_empty() {
                if let Some((start, end)) =
                    case_insensitive_find(&displayed_name_raw, &popup.filter)
                {
                    (
                        displayed_name_raw[..start].to_string(),
                        displayed_name_raw[start..end].to_string(),
                        displayed_name_raw[end..].to_string(),
                    )
                } else {
                    (displayed_name_raw.clone(), String::new(), String::new())
                }
            } else {
                (displayed_name_raw.clone(), String::new(), String::new())
            };

            // ── Assemble row segments ─────────────────────────────────────────
            let mut segments: Vec<Segment> = Vec::new();
            segments.push(Segment::new(&idx_str, catppuccin::OVERLAY0));
            segments.push(Segment::new("  ", catppuccin::SURFACE1));

            if !prefix.is_empty() {
                segments.push(Segment::new(&prefix, name_color));
            }
            if !matched.is_empty() {
                segments.push(Segment::new(&matched, catppuccin::PEACH));
            }
            if !suffix.is_empty() {
                segments.push(Segment::new(&suffix, name_color));
            }

            // Pad filename to exactly file_name_width
            let displayed_w = str_width(&displayed_name_raw);
            let name_pad = " ".repeat(file_name_width.saturating_sub(displayed_w));
            if !name_pad.is_empty() {
                segments.push(Segment::new(&name_pad, catppuccin::SURFACE1));
            }

            segments.push(Segment::new("  ", catppuccin::SURFACE1)); // gap after name
            segments.push(Segment::new(&dir_display, catppuccin::OVERLAY0)); // already padded to dir_field_w

            // Single separator then right-anchored metadata columns
            segments.push(Segment::new(" ", catppuccin::SURFACE1));
            segments.push(Segment::new(&pos_col, catppuccin::YELLOW));
            segments.push(Segment::new(" ", catppuccin::SURFACE1));
            segments.push(Segment::new(&count_col, catppuccin::PEACH));
            segments.push(Segment::new(" ", catppuccin::SURFACE1));
            segments.push(Segment::new(&time_col, catppuccin::OVERLAY0));

            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            draw_empty_row(stdout, x, row_y, popup_width, &RowStyle::normal())?;
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + content_rows as u16;
    let footer = format!(
        "[Home] {}  [Del] remove  [Enter] open  [Esc]{}close  {}/{}",
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
