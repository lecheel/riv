//! Tag list popup overlay for selecting among multiple tag/definition matches.

use crate::lsp::Location;
use crate::popup::Scrollable;
use crate::rounded_box::{
    catppuccin, centered_in_edit, clamp_height, clamp_width, clear_rect, draw_bottom_border,
    draw_empty_row, draw_row, draw_top_border, str_width, truncate_to_width, BoxStyle, RowStyle,
    Segment,
};
use crate::tags::TagEntry;
use crossterm::execute;
use crossterm::style::ResetColor;

/// Popup for selecting among multiple tag/definition matches.
/// Shown when `gd` (or ctags tag_under_cursor) finds multiple definitions.
#[derive(Debug, Clone)]
pub struct TagListPopup {
    pub entries: Vec<TagListEntry>,
    pub selected: usize,
    pub scroll: usize,
    pub title: String,
    pub word: String,
}

#[derive(Debug, Clone)]
pub struct TagListEntry {
    pub name: String,
    pub file: String,
    pub line: usize,
    /// Excerpt of the line content for context
    pub preview: String,
}

impl TagListPopup {
    pub fn new(word: &str, matches: &[TagEntry], project_root: &std::path::Path) -> Self {
        let entries: Vec<TagListEntry> = matches
            .iter()
            .map(|tag| {
                let relative = tag
                    .file
                    .strip_prefix(project_root)
                    .unwrap_or(&tag.file)
                    .to_string_lossy()
                    .to_string();
                TagListEntry {
                    name: tag.name.clone(),
                    file: relative,
                    line: tag.line,
                    preview: String::new(),
                }
            })
            .collect();

        TagListPopup {
            entries,
            selected: 0,
            scroll: 0,
            title: format!("Tags: {}", word),
            word: word.to_string(),
        }
    }

    /// Create from LSP Location results.
    pub fn from_lsp_locations(word: &str, locations: &[Location]) -> Self {
        let entries: Vec<TagListEntry> = locations
            .iter()
            .map(|loc| {
                // loc.uri is a String — handle file:// prefix or plain path
                let file_path = if loc.uri.starts_with("file:///") {
                    loc.uri[7..].to_string()
                } else if loc.uri.starts_with("file://") {
                    loc.uri[7..].to_string()
                } else {
                    loc.uri.clone()
                };
                TagListEntry {
                    name: word.to_string(),
                    file: file_path,
                    line: loc.range.start.line as usize + 1,
                    preview: String::new(),
                }
            })
            .collect();

        TagListPopup {
            entries,
            selected: 0,
            scroll: 0,
            title: format!("Definitions: {}", word),
            word: word.to_string(),
        }
    }

    pub fn selected_entry(&self) -> Option<&TagListEntry> {
        self.entries.get(self.selected)
    }
}

impl Scrollable for TagListPopup {
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
        15
    }
}

pub fn render_tag_list_popup(
    popup: &TagListPopup,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_h = 6u16;
    let edit_h = term_height.saturating_sub(status_h);
    let popup_width = clamp_width(80, term_width, 4);
    let content_rows = clamp_height(15, edit_h.saturating_sub(4), 5) as usize;
    let popup_height = content_rows as u16 + 2;

    let (x, y) = centered_in_edit(popup_width, popup_height, term_width, term_height, status_h);
    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    // ── Title bar ──
    let title = format!(
        " {} {} ",
        popup.title,
        if popup.entries.is_empty() {
            "(empty)".to_string()
        } else {
            format!("({})", popup.entries.len())
        }
    );
    let title_style = BoxStyle::default()
        .with_title(title)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    // ── Content rows ──
    let file_name_width: usize = 40;
    let scroll = popup.scroll;

    for i in 0..content_rows {
        let row_y = y + 1 + i as u16;
        let entry_idx = scroll + i;

        if entry_idx < popup.entries.len() {
            let entry = &popup.entries[entry_idx];
            let is_selected = entry_idx == popup.selected;
            let row_style = if is_selected {
                RowStyle::selected()
                    .with_border(catppuccin::SURFACE2)
                    .with_bg(catppuccin::SURFACE0)
            } else {
                RowStyle::normal()
                    .with_border(catppuccin::SURFACE2)
                    .with_bg(catppuccin::MANTLE)
            };

            let idx_str = format!("{:>2}", entry_idx + 1);
            let file_line = format!("{}:{}", entry.file, entry.line);
            let file_display = if str_width(&file_line) > file_name_width {
                let truncated = truncate_to_width(&file_line, file_name_width.saturating_sub(1));
                format!("{}…", truncated)
            } else {
                file_line
            };

            let file_color = if is_selected {
                catppuccin::TEXT
            } else {
                catppuccin::BLUE
            };

            let mut segments = Vec::new();
            segments.push(Segment::new(&idx_str, catppuccin::OVERLAY0));
            segments.push(Segment::new("  ", catppuccin::SURFACE1));
            segments.push(Segment::new(&file_display, file_color));

            let displayed_w = str_width(&file_display);
            let padding = file_name_width.saturating_sub(displayed_w);
            let pad_str = if padding > 0 {
                " ".repeat(padding)
            } else {
                String::new()
            };
            if !pad_str.is_empty() {
                segments.push(Segment::new(&pad_str, catppuccin::SURFACE1));
            }

            segments.push(Segment::new("  ", catppuccin::SURFACE1));

            segments.push(Segment::new(
                &entry.name,
                if is_selected {
                    catppuccin::TEXT
                } else {
                    catppuccin::MAUVE
                },
            ));

            if !entry.preview.is_empty() {
                segments.push(Segment::new("  ", catppuccin::SURFACE1));
                segments.push(Segment::new(&entry.preview, catppuccin::OVERLAY0));
            }

            draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
        } else {
            let empty_style = RowStyle::normal().with_border(catppuccin::SURFACE2);
            draw_empty_row(stdout, x, row_y, popup_width, &empty_style)?;
        }
    }

    // ── Bottom border ──
    let bottom_y = y + 1 + content_rows as u16;
    let footer = format!(
        "[Enter] jump  [Esc] close  {}/{}",
        if popup.entries.is_empty() {
            0
        } else {
            popup.selected + 1
        },
        popup.entries.len(),
    );
    let footer_style = BoxStyle::default()
        .with_footer(footer)
        .with_bg(catppuccin::MANTLE);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &footer_style)?;

    execute!(stdout, ResetColor)?;
    Ok(())
}
