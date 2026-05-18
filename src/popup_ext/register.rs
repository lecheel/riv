//! Register popup overlay for displaying register contents and LLM responses.

use crate::rounded_box::{
    catppuccin, clear_rect, draw_bottom_border, draw_row_text, draw_top_border, BoxStyle, RowStyle,
};
use unicode_width::UnicodeWidthStr;

/// Render the register / LLM-response popup at the bottom of the edit area.
pub fn render_register_popup(
    title: &str,
    lines: &[String],
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    if lines.is_empty() || term_width == 0 {
        return Ok(());
    }

    let status_height = 3;
    let edit_height = (term_height.saturating_sub(status_height)) as usize;

    // Dynamic max height: up to half the edit area for LLM responses,
    // or 10 lines for register listing
    let is_registers = title == "Registers";
    let max_height = if is_registers {
        10usize
    } else {
        edit_height / 2
    };
    let visible_count = lines.len().min(max_height);
    let content_rows = visible_count as u16;
    let total_height = content_rows + 2; // +2 for border

    let popup_width = term_width;
    let x = 0;

    let y = term_height
        .saturating_sub(status_height)
        .saturating_sub(total_height);

    clear_rect(stdout, x, y, popup_width, total_height, catppuccin::MANTLE)?;

    // Dynamic title
    let display_title = format!(" {} ", title);
    let title_style = BoxStyle::default()
        .with_title(display_title)
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    for (i, line) in lines.iter().take(visible_count).enumerate() {
        let row_y = y + 1 + i as u16;
        let row_style = RowStyle::normal()
            .with_border(catppuccin::SURFACE2)
            .with_bg(catppuccin::MANTLE);

        let max_line_w = popup_width.saturating_sub(2) as usize;
        let display_line = if UnicodeWidthStr::width(line.as_str()) > max_line_w {
            let mut s = String::new();
            let mut w = 0;
            for c in line.chars() {
                w += UnicodeWidthStr::width(c.to_string().as_str());
                if w > max_line_w.saturating_sub(1) {
                    s.push('…');
                    break;
                }
                s.push(c);
            }
            s
        } else {
            line.clone()
        };

        draw_row_text(stdout, x, row_y, popup_width, &display_line, &row_style)?;
    }

    let bottom_y = y + 1 + visible_count as u16;
    let footer = if is_registers {
        String::new()
    } else {
        format!("{}/{} [\"e] paste  [Esc] close", visible_count, lines.len())
    };
    let bottom_style = BoxStyle::default()
        .with_border(catppuccin::SURFACE2)
        .with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    Ok(())
}
