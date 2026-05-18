use crate::popup_ext::word_wrap;
use crate::rounded_box::*;
use crate::Editor;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};

pub fn render_guide_popup(
    _editor: &Editor,
    stdout: &mut std::io::Stdout,
    popup: &crate::guide::Guide,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_height = 3;
    let max_visible = 24usize;
    let visible_count = popup.filtered.len().min(max_visible);

    let mut max_kind = 8usize;
    let mut max_label = 20usize;
    for &idx in popup.filtered.iter() {
        if let Some(entry) = popup.entries.get(idx) {
            max_kind = max_kind.max(entry.kind.len());
            max_label = max_label.max(entry.label.len());
        }
    }

    let calc_width = max_kind + 1 + max_label + 3 + 6 + 4;
    let popup_width = calc_width.max(100).min(term_width as usize) as u16;
    let popup_height = max_visible as u16 + 4;

    let x = (term_width.saturating_sub(popup_width)) / 2;
    let y = (term_height
        .saturating_sub(status_height)
        .saturating_sub(popup_height))
        / 2;

    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    let count_text = if popup.filtered.is_empty() {
        "(no match)".to_string()
    } else {
        format!("({})", popup.filtered.len())
    };
    let title = format!(" Guide {} ", count_text);
    let title_style = BoxStyle::default()
        .with_title(title)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

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

    let scroll = popup.scroll;

    for i in 0..max_visible {
        let row_y = filter_y + 1 + i as u16;
        if row_y >= y + popup_height - 1 {
            break;
        }
        let entry_idx = scroll + i;

        if entry_idx < popup.filtered.len() {
            let real_idx = popup.filtered[entry_idx];
            let entry = match popup.entries.get(real_idx) {
                Some(e) => e,
                None => {
                    let empty_style = RowStyle::normal().with_border(catppuccin::SURFACE2);
                    draw_empty_row(stdout, x, row_y, popup_width, &empty_style)?;
                    continue;
                }
            };
            let is_selected = entry_idx == popup.selected;

            let kind_str = format!("{:>width$} ", entry.kind, width = max_kind);
            let short_file = entry
                .file
                .rsplit_once('/')
                .map(|(_, f)| f)
                .unwrap_or(&entry.file);

            let mut segments = Vec::new();

            let kind_color = match entry.kind.as_str() {
                "struct" | "enum" | "type" => catppuccin::YELLOW,
                "fn" | "impl" => catppuccin::BLUE,
                "trait" => catppuccin::TEAL,
                "section" => catppuccin::MAUVE,
                "match" => catppuccin::PEACH,
                "const" | "macro" | "field" => catppuccin::GREEN,
                _ => catppuccin::SUBTEXT,
            };
            segments.push(Segment::new(&kind_str, kind_color));

            if !popup.filter.is_empty() {
                let lower_label = entry.label.to_lowercase();
                let lower_query = popup.filter.to_lowercase();
                if let Some(match_start) = lower_label.find(&lower_query) {
                    let match_end = match_start + lower_query.len();
                    if match_start > 0 {
                        segments.push(Segment::new(
                            &entry.label[..match_start],
                            if is_selected {
                                catppuccin::TEXT
                            } else {
                                catppuccin::BLUE
                            },
                        ));
                    }
                    segments.push(Segment::new(
                        &entry.label[match_start..match_end.min(entry.label.len())],
                        catppuccin::PEACH,
                    ));
                    if match_end < entry.label.len() {
                        segments.push(Segment::new(
                            &entry.label[match_end..],
                            if is_selected {
                                catppuccin::TEXT
                            } else {
                                catppuccin::BLUE
                            },
                        ));
                    }
                } else {
                    segments.push(Segment::new(
                        &entry.label,
                        if is_selected {
                            catppuccin::TEXT
                        } else {
                            catppuccin::BLUE
                        },
                    ));
                }
            } else {
                segments.push(Segment::new(
                    &entry.label,
                    if is_selected {
                        catppuccin::TEXT
                    } else {
                        catppuccin::BLUE
                    },
                ));
            }

            let file_with_spaces = format!("  {}", short_file);
            segments.push(Segment::new(&file_with_spaces, catppuccin::OVERLAY0));

            segments.push(Segment::new("  ", catppuccin::SURFACE1));
            let remaining = popup_width as usize
                - segments.iter().map(|s| str_width(s.text)).sum::<usize>()
                - 4;
            let desc_display = truncate_to_width(&entry.desc, remaining);
            segments.push(Segment::new(
                desc_display,
                if is_selected {
                    catppuccin::SUBTEXT
                } else {
                    catppuccin::OVERLAY0
                },
            ));

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

    let bottom_y = filter_y + 1 + max_visible as u16;
    let footer = format!(
        "[Enter] jump  [Esc] close  {}/{}",
        if popup.filtered.is_empty() {
            0
        } else {
            popup.selected + 1
        },
        popup.filtered.len(),
    );
    let bottom_style = BoxStyle::default()
        .with_border(catppuccin::SURFACE2)
        .with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    if let Some(entry) = popup.selected_entry() {
        if entry.hint.is_some() || !entry.desc.is_empty() {
            render_guide_doc_popup(
                entry,
                stdout,
                x,
                y,
                popup_width,
                popup_height,
                term_width,
                term_height,
            )?;
        }
    }

    execute!(stdout, ResetColor)?;
    Ok(())
}

fn render_guide_doc_popup(
    entry: &crate::guide::GuideEntry,
    stdout: &mut std::io::Stdout,
    comp_x: u16,
    comp_y: u16,
    comp_width: u16,
    comp_height: u16,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let doc_max_width: u16 = 70;
    let gap: u16 = 1;

    let available_right = term_width.saturating_sub(comp_x + comp_width + gap);
    let available_left = comp_x.saturating_sub(gap);

    let (doc_width, x): (u16, u16) = if available_right >= 40 {
        let w = doc_max_width.min(available_right);
        (w, comp_x + comp_width + gap)
    } else if available_left >= 40 {
        let w = doc_max_width.min(available_left);
        (w, comp_x.saturating_sub(w + gap))
    } else {
        return Ok(());
    };

    let content_width = doc_width.saturating_sub(2) as usize;
    if content_width == 0 {
        return Ok(());
    }

    let mut doc_lines: Vec<String> = Vec::new();
    doc_lines.push(format!("📄 {}", entry.file));
    doc_lines.push(format!("🔍 {}", entry.anchor));
    doc_lines.push(String::new());

    if !entry.desc.is_empty() {
        doc_lines.extend(word_wrap(&entry.desc, content_width));
        doc_lines.push(String::new());
    }

    if !entry.tags.is_empty() {
        doc_lines.push(format!("tags: {}", entry.tags.join(", ")));
        doc_lines.push(String::new());
    }

    if let Some(ref hint) = entry.hint {
        doc_lines.push("💡 Implementation hint:".to_string());
        doc_lines.extend(word_wrap(hint, content_width));
    }

    if doc_lines.is_empty() {
        return Ok(());
    }

    let max_visible_rows = comp_height.saturating_sub(2) as usize;
    let visible_rows = doc_lines.len().min(max_visible_rows).max(1);
    let doc_height = visible_rows as u16 + 2;

    let edit_height = term_height.saturating_sub(3);
    let doc_height = doc_height.min(edit_height.saturating_sub(comp_y));
    if doc_height < 3 {
        return Ok(());
    }

    let visible_rows = (doc_height.saturating_sub(2)) as usize;

    clear_rect(stdout, x, comp_y, doc_width, doc_height, catppuccin::MANTLE)?;

    let title = format!(" {} {} ", entry.kind, entry.label);
    let title_style = BoxStyle::default()
        .with_title(title)
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, comp_y, doc_width, &title_style)?;

    for (i, line) in doc_lines.iter().take(visible_rows).enumerate() {
        let row_y = comp_y + 1 + i as u16;
        let row_style = RowStyle::normal()
            .with_border(catppuccin::SURFACE2)
            .with_bg(catppuccin::MANTLE);

        if line.is_empty() {
            draw_empty_row(stdout, x, row_y, doc_width, &row_style)?;
        } else {
            let color = if line.starts_with("📄") || line.starts_with("🔍") {
                catppuccin::OVERLAY0
            } else if line.starts_with("💡") || line.starts_with("tags:") {
                catppuccin::PEACH
            } else {
                catppuccin::SUBTEXT
            };
            let segments = [Segment::new(line, color)];
            draw_row(stdout, x, row_y, doc_width, &segments, &row_style)?;
        }
    }

    let bottom_y = comp_y + 1 + visible_rows as u16;
    let more = doc_lines.len() > visible_rows;
    let footer = if more {
        format!("↓ {}/{}", visible_rows, doc_lines.len())
    } else {
        String::new()
    };
    let bottom_style = BoxStyle::default()
        .with_border(catppuccin::SURFACE2)
        .with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, doc_width, &bottom_style)?;

    Ok(())
}
