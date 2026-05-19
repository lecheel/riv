use crate::ed::DiffPopupPrefix;
use crate::rounded_box::*;

pub fn render_diff_popup(
    _editor: &crate::Editor,
    stdout: &mut std::io::Stdout,
    popup: &crate::ed::git::DiffPopup,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_height = 3;
    let edit_height = term_height.saturating_sub(status_height);

    let pw = ((term_width as f32 * popup.width_fraction) as u16).min(term_width.saturating_sub(2));
    let max_content_rows = (popup.max_rows as u16).min(edit_height.saturating_sub(3));
    let content_rows = (popup.lines.len() as u16).min(max_content_rows);
    let total_height = 2 + content_rows + 1;

    if total_height < 4 || pw < 10 {
        return Ok(());
    }

    let x = term_width.saturating_sub(pw).saturating_sub(1);
    let y = 0;

    clear_rect(stdout, x, y, pw, total_height, catppuccin::MANTLE)?;

    let title_style = BoxStyle::default()
        .with_title("Diff")
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::SURFACE0);
    draw_top_border(stdout, x, y, pw, &title_style)?;

    for (i, line) in popup.lines.iter().take(content_rows as usize).enumerate() {
        let row_y = y + 1 + i as u16;
        let sign = match line.prefix {
            DiffPopupPrefix::Add => "+",
            DiffPopupPrefix::Delete => "-",
            DiffPopupPrefix::Header => "@",
            DiffPopupPrefix::Context => " ",
        };
        let sign_color = match line.prefix {
            DiffPopupPrefix::Add => catppuccin::GREEN,
            DiffPopupPrefix::Delete => catppuccin::RED,
            DiffPopupPrefix::Header => catppuccin::BLUE,
            DiffPopupPrefix::Context => catppuccin::SUBTEXT,
        };
        let lineno_str = match line.prefix {
            DiffPopupPrefix::Add => line.new_lineno.map(|n| format!("{:>4} ", n)),
            DiffPopupPrefix::Delete => line.old_lineno.map(|n| format!("{:>4} ", n)),
            _ => None,
        };
        let mut segments = vec![Segment::new(sign, sign_color)];
        if let Some(ref ln) = lineno_str {
            segments.push(Segment::new(ln, catppuccin::SUBTEXT));
        }
        segments.push(Segment::new(&line.text, catppuccin::TEXT));

        let row_style = RowStyle::normal()
            .with_border(catppuccin::SURFACE2)
            .with_bg(catppuccin::MANTLE)
            .no_padding();
        draw_row(stdout, x, row_y, pw, &segments, &row_style)?;
    }

    let bottom_y = y + 1 + content_rows;
    let bottom_style = BoxStyle::default().with_border(catppuccin::SURFACE2);
    draw_bottom_border(stdout, x, bottom_y, pw, &bottom_style)?;

    Ok(())
}
