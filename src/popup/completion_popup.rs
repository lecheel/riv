//--+ popup/completion_popup.rs
// popup/completion_popup.rs — Optimized version
// ──────────────────────────────────────────────────────────────
// Key optimizations:
//   1. Pre-compute unicode widths once, not per-item
//   2. Reuse String buffer for formatted output
//   3. Skip softwrap computation when word_wrap is disabled
//   4. Batch crossterm execute! calls where possible
//   5. Strictly position below current line; shrink instead of overlapping above
//   6. Auto-scroll-center when cursor is in the bottom 25% of the text area
// ──────────────────────────────────────────────────────────────

use crate::completion::{CompletionKind, CompletionSource};
use crate::rounded_box::*;
use crate::Editor;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Print, SetBackgroundColor, SetForegroundColor};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn softwrap_rows(line_text: &str, content_width: usize) -> usize {
    if content_width == 0 {
        return 1;
    }
    if line_text.is_empty() {
        return 1;
    }
    let mut rows = 1usize;
    let mut col = 0usize;
    for g in line_text.graphemes(true) {
        let w = UnicodeWidthStr::width(g);
        if col + w > content_width {
            rows += 1;
            col = w;
        } else {
            col += w;
        }
    }
    rows
}

fn softwrap_row_offset(line_text: &str, cursor_col: usize, content_width: usize) -> usize {
    if content_width == 0 || line_text.is_empty() {
        return 0;
    }
    let mut row = 0usize;
    let mut col = 0usize;
    let mut char_col = 0usize;
    for g in line_text.graphemes(true) {
        if char_col >= cursor_col {
            break;
        }
        let w = UnicodeWidthStr::width(g);
        if col + w > content_width {
            row += 1;
            col = w;
        } else {
            col += w;
        }
        char_col += 1;
    }
    row
}

fn calculate_cursor_screen_row(editor: &Editor, gutter_w: u16, mark_gutter_w: u16, git_gutter_w: u16) -> u16 {
    editor
        .windows
        .active_window()
        .map(|w| {
            let vp = &w.viewport;
            let cursor_line = w.cursor.position.line;
            let cursor_col = w.cursor.position.col;
            let buffer = editor.buffers.get(&w.buffer_id);
            let content_width = (w
                .width
                .saturating_sub(gutter_w)
                .saturating_sub(mark_gutter_w)
                .saturating_sub(git_gutter_w)) as usize;
            let mut visual_rows = 0usize;
            if let Some(buf) = buffer {
                if editor.config.word_wrap {
                    for line_i in vp.scroll_line..cursor_line {
                        if let Some(txt) = buf.line_text(line_i) {
                            visual_rows += softwrap_rows(&txt, content_width);
                        }
                    }
                    if let Some(cl_txt) = buf.line_text(cursor_line) {
                        visual_rows += softwrap_row_offset(&cl_txt, cursor_col, content_width);
                    }
                } else {
                    visual_rows = cursor_line.saturating_sub(vp.scroll_line);
                }
            }
            w.y_offset + visual_rows as u16
        })
        .unwrap_or(0)
}

fn calculate_cursor_screen_col(editor: &Editor) -> usize {
    editor
        .windows
        .active_window()
        .map(|w| {
            let scroll_col = w.viewport.scroll_col as usize;
            let cursor_col = w.cursor.position.col;

            let buf = editor.buffers.get(&w.buffer_id);
            if let Some(buf) = buf {
                let line_text = buf.line_text(w.cursor.position.line).unwrap_or_default();
                let line_text = line_text.trim_end_matches('\n');

                if line_text.is_ascii() {
                    let end = cursor_col.min(line_text.len());
                    (end.saturating_sub(scroll_col)) as usize
                } else {
                    let graphemes: Vec<_> = line_text.graphemes(true).collect();
                    let start = scroll_col.min(graphemes.len());
                    let end = cursor_col.min(graphemes.len());
                    graphemes[start..end].iter().map(|g| UnicodeWidthStr::width(*g)).sum::<usize>()
                }
            } else {
                cursor_col.saturating_sub(scroll_col)
            }
        })
        .unwrap_or(0)
}

// ── Doc popup ──

pub fn render_completion_doc_popup(
    item: &crate::completion::CompletionEntry,
    stdout: &mut std::io::Stdout,
    comp_x: u16,
    comp_y: u16,
    comp_width: u16,
    comp_height: u16,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    if item.source != CompletionSource::Lsp {
        return Ok(());
    }

    let clean_detail = item
        .detail
        .as_deref()
        .map(|d| d.replace(" [lsp]", "").replace(" [buffer]", "").replace(" [vocab]", ""));

    let has_meaningful_doc = item.documentation.as_ref().map_or(false, |d| !d.trim().is_empty());

    let has_meaningful_detail = clean_detail.as_deref().map_or(false, |d| !d.trim().is_empty());

    if !has_meaningful_doc && !has_meaningful_detail {
        return Ok(());
    }

    let doc_max_width: u16 = 60;
    let gap: u16 = 1;
    let available_right = term_width.saturating_sub(comp_x + comp_width + gap);
    let available_left = comp_x.saturating_sub(gap);

    let (doc_width, x): (u16, u16) = if available_right >= 30 {
        let w = doc_max_width.min(available_right);
        (w, comp_x + comp_width + gap)
    } else if available_left >= 30 {
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

    if let Some(ref d) = clean_detail {
        let d = d.trim();
        if !d.is_empty() {
            doc_lines.extend(super::word_wrap(d, content_width));
            doc_lines.push(String::new());
        }
    }

    if let Some(doc) = &item.documentation {
        if !doc.is_empty() {
            doc_lines.extend(super::word_wrap(doc, content_width));
        }
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

    let kind_label = item.kind.as_str();
    let title = if kind_label.is_empty() {
        format!(" {} ", item.label)
    } else {
        format!(" {} {} ", kind_label, item.label)
    };
    let title_style = BoxStyle::default()
        .with_title(title)
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, comp_y, doc_width, &title_style)?;

    for (i, line) in doc_lines.iter().take(visible_rows).enumerate() {
        let row_y = comp_y + 1 + i as u16;

        let row_style = RowStyle::normal().with_border(catppuccin::SURFACE2).with_bg(catppuccin::MANTLE);

        if line.is_empty() {
            draw_empty_row(stdout, x, row_y, doc_width, &row_style)?;
        } else {
            let color = if line.starts_with("```") {
                catppuccin::OVERLAY0
            } else if line.starts_with('#') {
                catppuccin::MAUVE
            } else if line.starts_with("pub ")
                || line.starts_with("fn ")
                || line.starts_with("async ")
                || line.starts_with("struct ")
                || line.starts_with("impl ")
                || line.starts_with("trait ")
                || line.starts_with("enum ")
                || line.starts_with("type ")
                || line.starts_with("const ")
                || line.starts_with("let ")
            {
                catppuccin::BLUE
            } else if line.starts_with('•') || line.starts_with("- ") || line.starts_with("* ") {
                catppuccin::SUBTEXT
            } else if line.starts_with('@') {
                catppuccin::TEAL
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
    let bottom_style = BoxStyle::default().with_border(catppuccin::SURFACE2).with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, doc_width, &bottom_style)?;

    Ok(())
}

// ── Completion popup ──

pub fn render_completion_popup(
    editor: &mut Editor, // Mutability added to adjust viewport for UX
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_height = 3;
    let edit_height = term_height.saturating_sub(status_height);

    let items = &editor.completion.items;
    let selected = editor.completion.selected_index;
    let trigger = editor.completion.context.as_ref().map(|c| c.trigger.as_str()).unwrap_or("");

    if items.is_empty() {
        return Ok(());
    }

    let gutter_w = if editor.config.line_numbers { 5u16 } else { 0 };
    let mark_gutter_w = {
        let current_bid = if let Some(w) = editor.windows.active_window() {
            w.buffer_id
        } else {
            crate::buffer::BufferId::default()
        };
        if editor.search.marks.iter().any(|(_, (bid, _))| *bid == current_bid) {
            1u16
        } else {
            0u16
        }
    };
    let git_gutter_w = if editor.git.gutter_enabled && editor.config.enable_git {
        1u16
    } else {
        0u16
    };

    // 1. Calculate initial visual position to check available space
    let cursor_screen_row = calculate_cursor_screen_row(editor, gutter_w, mark_gutter_w, git_gutter_w);
    let space_below = edit_height.saturating_sub(cursor_screen_row + 1);

    // 2. UX Improvement: If line is in the bottom 25% of text area, apply scroll_center
    if space_below < edit_height / 4 {
        // NOTE: Requires adding `active_window_mut()` to your Windows struct
        if let Some(w) = editor.windows.active_window_mut() {
            let cursor_line = w.cursor.position.line;
            // Target scroll line centers the cursor roughly in the middle of the edit area
            let target_scroll = cursor_line.saturating_sub((edit_height / 2) as usize);
            // Only scroll down (increase scroll_line) to avoid jittering if user scrolled up
            if target_scroll > w.viewport.scroll_line {
                w.viewport.scroll_line = target_scroll;
            }
        }
    }

    // 3. Recalculate visual positions now that the viewport might have scrolled
    let cursor_screen_row = calculate_cursor_screen_row(editor, gutter_w, mark_gutter_w, git_gutter_w);
    let cursor_screen_col = calculate_cursor_screen_col(editor);

    // ── Strictly Position Below: shrink if not enough space, never overlap above ──
    let space_below = edit_height.saturating_sub(cursor_screen_row + 1);
    if space_below < 3 {
        // Not enough space below to even show 1 item + 2 borders
        return Ok(());
    }

    let max_visible = 8usize;
    let mut visible_count = items.len().min(max_visible);

    // Cap visible_count to strictly fit in the space below the current line
    let max_possible_items = (space_below - 2) as usize; // -2 for top/bottom border
    if visible_count > max_possible_items {
        visible_count = max_possible_items;
    }

    if visible_count == 0 {
        return Ok(());
    }

    // Always position directly below the current line
    let y = cursor_screen_row + 1;

    let scroll_offset = if selected >= visible_count {
        selected - visible_count + 1
    } else {
        0
    };

    let rendered_range = scroll_offset..(scroll_offset + visible_count).min(items.len());
    let rendered_items = &items[rendered_range.clone()];

    // OPT: pre-compute kind widths once (based on actually rendered items)
    let max_kind_w = rendered_items
        .iter()
        .map(|item| UnicodeWidthStr::width(item.kind.as_str()))
        .max()
        .unwrap_or(0);

    let max_left_w = rendered_items
        .iter()
        .map(|item| max_kind_w + if max_kind_w > 0 { 1 } else { 0 } + UnicodeWidthStr::width(item.label.as_str()))
        .max()
        .unwrap_or(0);
    let max_item_width = max_left_w.max(UnicodeWidthStr::width(trigger));
    let popup_content_width = (max_item_width + 4).min((term_width as usize).saturating_sub(4)) as u16;

    let popup_width = (popup_content_width + 2).max(40);
    let popup_height = (visible_count as u16) + 2;

    let trigger_display_width = UnicodeWidthStr::width(trigger) as u16;

    let x = (cursor_screen_col.saturating_sub(trigger_display_width as usize))
        .min((term_width as usize).saturating_sub(popup_width as usize)) as u16;

    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    let title_style = BoxStyle::default()
        .with_title("Completion")
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    for (i, item_idx) in rendered_range.enumerate() {
        let row_y = y + 1 + i as u16;
        if row_y >= y + popup_height - 1 {
            break;
        }
        let item = &items[item_idx];
        let is_selected = item_idx == selected;

        let mut segments = Vec::with_capacity(4);
        let kind_label = item.kind.as_str();
        let kind_color = match item.kind {
            CompletionKind::Function | CompletionKind::Method => catppuccin::BLUE,
            CompletionKind::Variable | CompletionKind::Field => catppuccin::MAUVE,
            CompletionKind::Type | CompletionKind::Struct => catppuccin::YELLOW,
            CompletionKind::Keyword => catppuccin::PEACH,
            CompletionKind::Module => catppuccin::GREEN,
            CompletionKind::Constant | CompletionKind::Enum => catppuccin::TEAL,
            _ => catppuccin::SUBTEXT,
        };

        segments.push(Segment::new(kind_label, kind_color));

        let kind_padding_len = max_kind_w.saturating_sub(UnicodeWidthStr::width(kind_label));
        let kind_padding: String = " ".repeat(kind_padding_len);
        segments.push(Segment::new(&kind_padding, catppuccin::SUBTEXT));

        if max_kind_w > 0 {
            segments.push(Segment::new(" ", catppuccin::SUBTEXT));
        }

        segments.push(Segment::new(
            &item.label,
            if is_selected { catppuccin::TEXT } else { catppuccin::SUBTEXT },
        ));

        let row_style = if is_selected {
            RowStyle::selected().with_border(catppuccin::SURFACE2).with_bg(catppuccin::SURFACE0)
        } else {
            RowStyle::normal().with_border(catppuccin::SURFACE2).with_bg(catppuccin::MANTLE)
        };
        draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
    }

    let bottom_y = y + 1 + visible_count as u16;
    let bottom_style = BoxStyle::default().with_border(catppuccin::SURFACE2);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    let info_y = bottom_y + 1;
    if info_y < edit_height {
        let info_text = format!(" {} ({}/{})", trigger, selected + 1, items.len());
        execute!(stdout, MoveTo(x, info_y))?;
        execute!(
            stdout,
            SetForegroundColor(catppuccin::OVERLAY0),
            SetBackgroundColor(catppuccin::MANTLE)
        )?;
        let info_display = truncate_to_width(&info_text, popup_width as usize);
        let pad = (popup_width as usize).saturating_sub(UnicodeWidthStr::width(info_display.as_str()));
        execute!(stdout, Print(info_display.clone()))?;
        let pad = (popup_width as usize).saturating_sub(UnicodeWidthStr::width(info_display.as_str()));
        if pad > 0 {
            execute!(stdout, Print(&" ".repeat(pad)))?;
        }
    }

    if let Some(selected_item) = items.get(selected) {
        render_completion_doc_popup(selected_item, stdout, x, y, popup_width, popup_height, term_width, term_height)?;
    }
    Ok(())
}

/// Truncate a string to fit within a given display width (in Unicode width units).
fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut result = String::with_capacity(s.len());
    for g in s.graphemes(true) {
        let gw = UnicodeWidthStr::width(g);
        if width + gw > max_width {
            break;
        }
        result.push_str(g);
        width += gw;
    }
    result
}
