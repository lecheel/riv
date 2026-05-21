//! File picker popup overlay for browsing and opening files.

use crate::popup::Scrollable;
use crate::rounded_box::*;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

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
    pub flat: bool,
}

impl FilePicker {
    pub fn new(initial_path: &Path, flat: bool) -> Self {
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
            flat,
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

        if self.flat {
            self.refresh_entries_flat();
        } else {
            self.refresh_entries_tree();
        }

        self.apply_filter();
    }

    /// Tree mode: list current-directory entries only (existing behaviour).
    fn refresh_entries_tree(&mut self) {
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
    }

    /// Flat mode: recursively walk cwd, respecting .gitignore, and list
    /// every file with its relative path.
    fn refresh_entries_flat(&mut self) {
        let mut entries = Vec::new();

        // WalkBuilder natively handles .gitignore, global gitignore
        // (~/.gitignore), repo excludes, hidden files, and safely skips symlinks.
        let walker = WalkBuilder::new(&self.cwd)
            .hidden(true) // Skip hidden files/dirs (starts with .)
            .git_ignore(true) // Respect .gitignore
            .git_global(true) // Respect global gitignore (~/.gitignore)
            .git_exclude(true) // Respect .git/info/exclude
            .build();

        for result in walker {
            if let Ok(entry) = result {
                // Skip the root directory itself
                if entry.path() == self.cwd {
                    continue;
                }

                // Only collect files in flat mode (directories are implicitly navigated)
                let is_dir = entry.file_type().map_or(false, |ft| ft.is_dir());
                if is_dir {
                    continue;
                }

                let path = entry.path().to_path_buf();

                // Calculate relative path for the display name
                // e.g. turns "/home/user/project/src/main.rs" -> "src/main.rs"
                let relative = entry.path().strip_prefix(&self.cwd).unwrap_or(entry.path());
                let name = relative.to_string_lossy().to_string();

                entries.push(FileEntry {
                    name,
                    path,
                    is_dir: false,
                    is_parent: false,
                });
            }
        }

        entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        self.all_entries = entries;
    }

    /// Toggle between flat and tree mode, then refresh.
    pub fn toggle_flat(&mut self) {
        self.flat = !self.flat;
        self.filter.clear();
        self.selected = 0;
        self.scroll = 0;
        self.refresh_entries();
    }

    pub(crate) fn apply_filter(&mut self) {
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
            let parent_pos = self
                .filtered
                .iter()
                .position(|&idx| self.all_entries.get(idx).map(|e| e.is_parent).unwrap_or(false));
            self.selected = parent_pos.unwrap_or(self.filtered.len() - 1);
        }

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
        // Flat mode is rooted at the original directory; going up would
        // trigger a massive recursive walk of the parent filesystem.
        if self.flat {
            return;
        }

        if let Some(parent) = self.cwd.parent() {
            if parent.as_os_str().is_empty() {
                return;
            }
            self.cwd = parent.to_path_buf();
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
        self.filtered.get(self.selected).and_then(|&i| self.all_entries.get(i))
    }

    pub fn can_go_up(&self) -> bool {
        self.cwd.parent().map(|p| !p.as_os_str().is_empty()).unwrap_or(false)
    }

    pub fn cwd_display(&self) -> String {
        self.cwd.display().to_string()
    }
}

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
    let popup_height = content_rows as u16 + 3;

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

        let segments = [Segment::new(">", catppuccin::PEACH), Segment::new(filter_display, catppuccin::TEXT)];
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
            let row_style = if is_selected { RowStyle::selected() } else { RowStyle::normal() };

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

            let segments = [Segment::new(icon, icon_color), Segment::new(&entry.name, name_color)];
            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            draw_empty_row(stdout, x, row_y, popup_width, &RowStyle::normal())?;
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + content_rows as u16;

    let footer_text = if picker.flat {
        format!(
            "[~] tree  [Enter] open  [Esc] close  {}/{}",
            picker.selected + 1,
            picker.filtered.len(),
        )
    } else {
        format!(
            "[-] up  [~] flat  [Enter] open  [Esc] close  {}/{}",
            picker.selected + 1,
            picker.filtered.len(),
        )
    };

    let footer_style = BoxStyle::default().with_footer(footer_text).with_bg(catppuccin::MANTLE);
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
        if window.to_lowercase().chars().eq(needle_lower.iter().cloned()) {
            let start_byte = hay_chars[..i].iter().collect::<String>().len();
            let end_byte = hay_chars[..i + n_len].iter().collect::<String>().len();
            return Some((start_byte, end_byte));
        }
    }
    None
}
