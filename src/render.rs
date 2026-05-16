//! All rendering logic for riv: windows, status line, popups, and softwrap layout.

use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::completion::CompletionKind;
use crate::dirty::Rect;
use crate::ed::DiffPopupPrefix;
use crate::editor::{Editor, FloatPopup, Mode, SearchDirection};
use crate::highlight::Highlighter;
use crate::misc::sanitize_single_line;
use crate::popup::MarkListPopup;
use crate::rounded_box::*;
use crate::terminal::Terminal;
use crate::terminal_sanitize::sanitize_for_display;
// Convenience — wraps the cow into a &str for Print
macro_rules! safe {
    ($s:expr_2021) => {
        sanitize_for_display($s).as_ref()
    };
}

// -----------------------------------------------------------------------------
// Softwrap helpers (moved from main.rs)
// -----------------------------------------------------------------------------

/// Given a line of text and a content width, return the number of visual rows
/// that this line would occupy when soft-wrapped.
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

/// Get the visual row offset for a given cursor column within a soft-wrapped line.
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

/// Get the display column for a given cursor column within a soft-wrapped line
/// (column relative to the visual row the cursor is on).
fn softwrap_display_col(line_text: &str, cursor_col: usize, content_width: usize) -> usize {
    if content_width == 0 || line_text.is_empty() {
        return 0;
    }
    let mut col = 0usize;
    let mut char_col = 0usize;
    for g in line_text.graphemes(true) {
        if char_col >= cursor_col {
            break;
        }
        let w = UnicodeWidthStr::width(g);
        col += w;
        char_col += 1;
    }
    col
}

// -----------------------------------------------------------------------------
// Window rendering
// -----------------------------------------------------------------------------

/// Render a single window into the terminal at the given offset.
fn render_window(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    window: &crate::window::Window,
    x_offset: u16,
    y_offset: u16,
    width: u16,
    height: u16,
    highlighter: &mut Highlighter,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::cursor::MoveTo;
    use crossterm::execute;
    use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
    use unicode_width::UnicodeWidthStr;

    if width == 0 || height == 0 {
        return Ok(());
    }

    let buffer_id = window.buffer_id;
    let buffer = match editor.buffers.get(&buffer_id) {
        Some(b) => b,
        None => return Ok(()),
    };

    let viewport = &window.viewport;
    let start_line = viewport.scroll_line;
    let scroll_col = viewport.scroll_col as usize;
    let is_active = editor.windows.active_window().map(|w| w.id) == Some(window.id);
    let cursor_line = window.cursor.position.line;
    let cursor_col = window.cursor.position.col;

    // ── Visual selection rectangle ──────────────────────────────────
    // In Visual modes, use selection_anchor directly.
    // In Command mode (when entered from visual via `:`), also show the
    // selection so the user can see the :'<,'> range.
    // Fallback to visual_selection_range when anchor was cleared.
    let selection_rect: Option<(usize, usize, usize, usize)> = window
        .selection_anchor
        .and_then(|anchor| {
            let head = window.cursor.position;
            let (top, bot) = if anchor.line <= head.line {
                (anchor.line, head.line)
            } else {
                (head.line, anchor.line)
            };
            let (left, right) = if anchor.col <= head.col {
                (anchor.col, head.col)
            } else {
                (head.col, anchor.col)
            };
            match editor.mode {
                Mode::Visual | Mode::VisualBlock | Mode::VisualLine => {
                    Some((top, bot, left, right))
                }
                Mode::Command => Some((top, bot, left, right)),
                _ => None,
            }
        })
        .or_else(|| {
            // Fallback: use persisted visual_selection_range for Command mode
            if editor.mode == Mode::Command {
                editor
                    .visual_selection_range
                    .map(|(top, bot)| (top, bot, 0, 0))
            } else {
                None
            }
        });

    // Dimmer selection colours for Command mode (frozen :'<,'> range)
    let (sel_bg, sel_fg) = if editor.mode == Mode::Command {
        (
            Color::Rgb {
                r: 36,
                g: 37,
                b: 52,
            },
            Color::Rgb {
                r: 166,
                g: 173,
                b: 200,
            },
        )
    } else {
        (
            Color::Rgb {
                r: 49,
                g: 50,
                b: 68,
            },
            Color::Rgb {
                r: 205,
                g: 214,
                b: 244,
            },
        )
    };

    let gutter_width = if editor.config.line_numbers { 5u16 } else { 0 };
    let git_gutter_width = if editor.git_gutter_enabled && editor.config.enable_git {
        1u16
    } else {
        0u16
    };

    // ── Mark gutter (between line numbers and git gutter) ──
    let mark_at_line: std::collections::HashMap<usize, char> = editor
        .marks
        .iter()
        .filter(|(_, (bid, _))| *bid == buffer_id)
        .map(|(ch, (_, pos))| (pos.line, *ch))
        .collect();
    let mark_gutter_width = if !mark_at_line.is_empty() { 1u16 } else { 0u16 };
    let content_width =
        width.saturating_sub(gutter_width + mark_gutter_width + git_gutter_width) as usize;

    // Pre‑compute the display‑width of the scroll offset so we can adjust
    // selection overlay positions for horizontal scrolling.
    let scroll_offset_display_w: usize = if scroll_col > 0 && !editor.config.word_wrap {
        let line_content = buffer.line_text(cursor_line).unwrap_or_default();
        let g: Vec<_> = line_content
            .trim_end_matches('\n')
            .graphemes(true)
            .collect();
        g[..scroll_col.min(g.len())]
            .iter()
            .map(|gr| gr.width())
            .sum()
    } else {
        0
    };

    let mut screen_row: usize = 0;
    let mut line_idx = start_line;

    while screen_row < height as usize {
        let y = y_offset + screen_row as u16;

        if line_idx >= buffer.line_count() {
            execute!(stdout, MoveTo(x_offset, y))?;
            execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
            execute!(stdout, Print("~"))?;
            execute!(stdout, ResetColor)?;
            let spaces = " ".repeat((width - 1) as usize);
            execute!(stdout, Print(&spaces))?;
            screen_row += 1;
            continue;
        }

        let line_text = buffer.line_text(line_idx).unwrap_or_default();
        let line_text = line_text.trim_end_matches('\n');

        let wrap_rows = if editor.config.word_wrap {
            softwrap_rows(line_text, content_width)
        } else {
            1
        };

        let graphemes: Vec<_> = line_text.graphemes(true).collect();
        let _cursor_wrap_row = if editor.config.word_wrap {
            softwrap_row_offset(line_text, cursor_col, content_width)
        } else {
            0
        };

        let line_spans = highlighter.highlight_line(line_text, buffer.language);

        // Pre‑compute per‑line scroll offset display width for selection positioning
        let line_scroll_offset_w: usize = if scroll_col > 0 && !editor.config.word_wrap {
            graphemes[..scroll_col.min(graphemes.len())]
                .iter()
                .map(|g| UnicodeWidthStr::width(*g))
                .sum()
        } else {
            0
        };

        for wrap_row in 0..wrap_rows {
            if screen_row >= height as usize {
                break;
            }
            let y = y_offset + screen_row as u16;
            execute!(stdout, MoveTo(x_offset, y))?;

            let is_cursor_line = is_active && line_idx == cursor_line;

            if is_cursor_line {
                execute!(stdout, SetBackgroundColor(Color::DarkGrey))?;
            }

            // Line number gutter
            if editor.config.line_numbers && wrap_row == 0 {
                let line_num_abs = line_idx + 1;
                let display_num = if editor.config.relative_line_numbers && is_active {
                    if line_idx == cursor_line {
                        line_num_abs
                    } else {
                        (cursor_line as isize - line_idx as isize).unsigned_abs()
                    }
                } else {
                    line_num_abs
                };
                let num_str = format!("{:>4} ", display_num);
                if is_cursor_line {
                    execute!(stdout, SetForegroundColor(Color::Cyan))?;
                } else {
                    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
                }
                execute!(stdout, Print(&num_str))?;
                if is_cursor_line {
                    execute!(stdout, SetForegroundColor(Color::Reset))?;
                } else {
                    execute!(stdout, ResetColor)?;
                }
            } else if editor.config.line_numbers {
                execute!(stdout, Print(&" ".repeat(gutter_width as usize)))?;
            }

            // Mark gutter
            if mark_gutter_width > 0 && wrap_row == 0 {
                if let Some(mark_char) = mark_at_line.get(&line_idx) {
                    if is_cursor_line {
                        execute!(
                            stdout,
                            SetForegroundColor(Color::Rgb {
                                r: 203,
                                g: 166,
                                b: 247
                            })
                        )?;
                    } else {
                        execute!(
                            stdout,
                            SetForegroundColor(Color::Rgb {
                                r: 145,
                                g: 125,
                                b: 190
                            })
                        )?;
                    }
                    execute!(stdout, Print(&mark_char.to_string()))?;
                    if is_cursor_line {
                        execute!(stdout, SetForegroundColor(Color::Reset))?;
                    } else {
                        execute!(stdout, ResetColor)?;
                    }
                } else {
                    execute!(stdout, Print(" "))?;
                }
            } else if mark_gutter_width > 0 {
                execute!(stdout, Print(" "))?;
            }

            // Git gutter
            if git_gutter_width > 0 && wrap_row == 0 {
                let sign = buffer.git_gutter.sign_at(line_idx);
                match sign {
                    Some(crate::git::GitSign::Added) => {
                        execute!(stdout, SetForegroundColor(Color::Green))?;
                        execute!(stdout, Print("+"))?;
                        if is_cursor_line {
                            execute!(stdout, SetForegroundColor(Color::Reset))?;
                        } else {
                            execute!(stdout, ResetColor)?;
                        }
                    }
                    Some(crate::git::GitSign::Modified) => {
                        execute!(stdout, SetForegroundColor(Color::Yellow))?;
                        execute!(stdout, Print("~"))?;
                        if is_cursor_line {
                            execute!(stdout, SetForegroundColor(Color::Reset))?;
                        } else {
                            execute!(stdout, ResetColor)?;
                        }
                    }
                    Some(crate::git::GitSign::RemovedAbove) => {
                        execute!(stdout, SetForegroundColor(Color::Red))?;
                        execute!(stdout, Print("_"))?;
                        if is_cursor_line {
                            execute!(stdout, SetForegroundColor(Color::Reset))?;
                        } else {
                            execute!(stdout, ResetColor)?;
                        }
                    }
                    None => {
                        execute!(stdout, Print(" "))?;
                    }
                }
            } else if git_gutter_width > 0 {
                execute!(stdout, Print(" "))?;
            }

            // Calculate which slice of the line to show
            let mut wrap_start_grapheme = 0usize;
            let display = if editor.config.word_wrap {
                let mut rows_so_far = 0;
                let mut row_col = 0usize;
                let mut start_grapheme = 0usize;
                let mut end_grapheme = graphemes.len();

                for (gi, g) in graphemes.iter().enumerate() {
                    if gi >= graphemes.len() {
                        break;
                    }
                    let w = UnicodeWidthStr::width(*g);
                    if row_col + w > content_width && gi > 0 {
                        rows_so_far += 1;
                        if rows_so_far == wrap_row {
                            start_grapheme = gi;
                            row_col = w;
                            for gj in gi..graphemes.len() {
                                let gw = UnicodeWidthStr::width(graphemes[gj]);
                                if row_col + gw > content_width {
                                    end_grapheme = gj;
                                    break;
                                }
                                row_col += gw;
                            }
                            break;
                        }
                        row_col = w;
                    } else {
                        row_col += w;
                    }
                }

                if wrap_row == 0 {
                    start_grapheme = 0;
                    let mut rc = 0usize;
                    for gi in 0..graphemes.len() {
                        let gw = UnicodeWidthStr::width(graphemes[gi]);
                        if rc + gw > content_width {
                            end_grapheme = gi;
                            break;
                        }
                        rc += gw;
                    }
                } else if rows_so_far != wrap_row && start_grapheme == 0 {
                    start_grapheme = graphemes.len();
                    end_grapheme = graphemes.len();
                }

                wrap_start_grapheme = start_grapheme;
                graphemes[start_grapheme..end_grapheme.min(graphemes.len())].join("")
            } else {
                let visible: String = if scroll_col < graphemes.len() {
                    graphemes[scroll_col..].join("")
                } else {
                    String::new()
                };
                if content_width > 0 {
                    let display_width = UnicodeWidthStr::width(visible.as_str());
                    if display_width > content_width {
                        graphemes[scroll_col..]
                            .iter()
                            .scan(0usize, |acc, g| {
                                *acc += UnicodeWidthStr::width(*g);
                                if *acc <= content_width {
                                    Some(g.to_string())
                                } else {
                                    None
                                }
                            })
                            .collect()
                    } else {
                        visible
                    }
                } else {
                    visible
                }
            };

            // Print line content with syntax highlighting
            if wrap_row == 0 {
                crate::highlight::render_highlighted_line(
                    stdout,
                    &display,
                    &line_spans,
                    is_cursor_line,
                    None,
                )?;
            } else {
                let offset_spans: Vec<crate::highlight::HighlightSpan> = line_spans
                    .iter()
                    .filter_map(|s| {
                        let start = s.start.saturating_sub(wrap_start_grapheme);
                        let end = s.end.saturating_sub(wrap_start_grapheme);
                        if end == 0 || s.end <= wrap_start_grapheme {
                            return None;
                        }
                        Some(crate::highlight::HighlightSpan {
                            start,
                            end,
                            style: s.style,
                        })
                    })
                    .collect();
                crate::highlight::render_highlighted_line(
                    stdout,
                    &display,
                    &offset_spans,
                    is_cursor_line,
                    None,
                )?;
            }

            // Clear rest of line
            let display_w = UnicodeWidthStr::width(display.as_str());
            let remaining = content_width.saturating_sub(display_w);
            if is_cursor_line {
                execute!(stdout, SetBackgroundColor(Color::DarkGrey))?;
            }
            if remaining > 0 {
                execute!(stdout, Print(&" ".repeat(remaining)))?;
            }
            if is_cursor_line {
                execute!(stdout, ResetColor)?;
            }

            // ── Visual selection overlay ─────────────────────────────
            if let Some((sel_top, sel_bot, sel_left, sel_right)) = selection_rect {
                // Determine if this line is in the selection.
                // In Command mode we always render line-wise (VisualLine style)
                // since :'<,'> operates on full lines.
                let in_sel_line = match editor.mode {
                    Mode::VisualLine => line_idx >= sel_top && line_idx <= sel_bot,
                    Mode::VisualBlock => line_idx >= sel_top && line_idx <= sel_bot,
                    Mode::Visual => {
                        if sel_top != sel_bot {
                            line_idx >= sel_top && line_idx <= sel_bot
                        } else {
                            line_idx == sel_top
                        }
                    }
                    Mode::Command => line_idx >= sel_top && line_idx <= sel_bot,
                    _ => false,
                };
                if in_sel_line {
                    let (vis_left, vis_right) = match editor.mode {
                        Mode::VisualLine => (0, graphemes.len()),
                        Mode::VisualBlock => (sel_left, sel_right + 1),
                        Mode::Visual => {
                            if sel_top == sel_bot {
                                (sel_left, sel_right + 1)
                            } else if line_idx == sel_top {
                                (sel_left, graphemes.len())
                            } else if line_idx == sel_bot {
                                (0, sel_right + 1)
                            } else {
                                (0, graphemes.len())
                            }
                        }
                        // Command mode: always line-wise for :'<,'>
                        Mode::Command => (0, graphemes.len()),
                        _ => continue,
                    };
                    let vis_left = vis_left.min(graphemes.len());
                    let vis_right = vis_right.min(graphemes.len());
                    if vis_left < vis_right {
                        // Clip selection to the visible grapheme range for this wrap row
                        let clip_left = vis_left.max(wrap_start_grapheme);
                        let wrap_end_grapheme =
                            wrap_start_grapheme + display.graphemes(true).count();
                        let clip_right = vis_right.min(wrap_end_grapheme);

                        if clip_left < clip_right {
                            let sel_graphemes: Vec<_> =
                                graphemes[clip_left..clip_right].iter().collect();
                            let sel_text: String =
                                sel_graphemes.iter().map(|g| g.to_string()).collect();
                            let sel_display_w = UnicodeWidthStr::width(sel_text.as_str());
                            let content_start_x =
                                x_offset + gutter_width + mark_gutter_width + git_gutter_width;

                            // Compute screen column of selection start,
                            // accounting for horizontal scroll offset
                            let mut sel_start_screen_col = 0usize;
                            for gi in 0..clip_left {
                                sel_start_screen_col += UnicodeWidthStr::width(graphemes[gi]);
                            }
                            // Subtract the scroll offset so the position is
                            // relative to the visible content area, not the
                            // start of the full line.
                            if !editor.config.word_wrap {
                                sel_start_screen_col =
                                    sel_start_screen_col.saturating_sub(line_scroll_offset_w);
                            }

                            let sel_start_x = content_start_x as usize + sel_start_screen_col;

                            // Only draw if the selection is within the visible content area
                            if sel_start_x < content_start_x as usize + content_width
                                && sel_display_w > 0
                                && sel_start_x >= content_start_x as usize
                            {
                                // Clip selection width so it doesn't overflow the content area
                                let max_sel_w =
                                    content_start_x as usize + content_width - sel_start_x;
                                let actual_sel_text = if sel_display_w > max_sel_w {
                                    // Truncate graphemes to fit
                                    let mut truncated = String::new();
                                    let mut w = 0usize;
                                    for g in sel_graphemes.iter() {
                                        let gw = g.width();
                                        if w + gw > max_sel_w {
                                            break;
                                        }
                                        truncated.push_str(g);
                                        w += gw;
                                    }
                                    truncated
                                } else {
                                    sel_text
                                };

                                if !actual_sel_text.is_empty() {
                                    execute!(
                                        stdout,
                                        MoveTo(sel_start_x as u16, y),
                                        SetBackgroundColor(sel_bg),
                                        SetForegroundColor(sel_fg),
                                        Print(&actual_sel_text),
                                        ResetColor
                                    )?;
                                }
                            }
                        }
                    }
                }
            }

            screen_row += 1;
        }

        line_idx += 1;
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Status line (3 rows)
// -----------------------------------------------------------------------------

fn render_status(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::cursor::MoveTo;
    use crossterm::execute;
    use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};
    use unicode_width::UnicodeWidthStr;

    use crate::powerline::{self, glyphs};

    let _base = powerline::crossterm_colors::BASE;
    let surface0 = powerline::crossterm_colors::SURFACE0;
    let surface1 = powerline::crossterm_colors::SURFACE1;
    let surface2 = powerline::crossterm_colors::SURFACE2;
    let overlay0 = powerline::crossterm_colors::OVERLAY0;
    let text = powerline::crossterm_colors::TEXT;
    let subtext = powerline::crossterm_colors::SUBTEXT;
    let peach = powerline::crossterm_colors::PEACH;
    let green = powerline::crossterm_colors::GREEN;
    let yellow = powerline::crossterm_colors::YELLOW;
    let red = powerline::crossterm_colors::RED;
    let blue = powerline::crossterm_colors::BLUE;

    let powerline_y = term_height.saturating_sub(3);
    let cmdline_y = term_height.saturating_sub(2);
    let infobar_y = term_height.saturating_sub(1);

    execute!(stdout, MoveTo(0, powerline_y))?;

    let (mode_fg, mode_bg) = powerline::get_mode_colors_crossterm(editor);
    let mode_sep = powerline::get_mode_sep_color_crossterm(editor);
    let mode_text = editor.mode_display();

    // Segment 1: Mode badge
    execute!(
        stdout,
        SetForegroundColor(mode_fg),
        SetBackgroundColor(mode_bg)
    )?;
    execute!(stdout, Print(&format!(" {} ", mode_text)))?;

    execute!(
        stdout,
        SetForegroundColor(mode_sep),
        SetBackgroundColor(surface0)
    )?;
    execute!(stdout, Print(glyphs::SEPARATOR_LEFT))?;

    // Segment 2: Filename + dirty indicator
    execute!(stdout, SetBackgroundColor(surface0))?;
    if let Some(buffer) = editor.current_buffer() {
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(&format!(" {} ", buffer.display_name())))?;

        if buffer.is_dirty() {
            execute!(stdout, SetForegroundColor(peach))?;
            execute!(stdout, Print(&format!(" {} ", glyphs::DIRTY)))?;
        } else {
            execute!(stdout, SetForegroundColor(green))?;
            execute!(stdout, Print(&format!(" {} ", glyphs::CLEAN)))?;
        }

        execute!(
            stdout,
            SetForegroundColor(surface0),
            SetBackgroundColor(surface1)
        )?;
        execute!(stdout, Print(glyphs::SEPARATOR_LEFT))?;

        // Segment 3: Position
        let window = editor.windows.active_window();
        let line = window.map(|w| w.cursor.position.line + 1).unwrap_or(1);
        let col = window.map(|w| w.cursor.position.col + 1).unwrap_or(1);
        execute!(stdout, SetForegroundColor(subtext))?;
        execute!(stdout, Print(&format!(" {}:{} ", line, col)))?;

        execute!(
            stdout,
            SetForegroundColor(surface1),
            SetBackgroundColor(surface2)
        )?;
        execute!(stdout, Print(glyphs::SEPARATOR_LEFT))?;

        // Segment 4: Percentage
        let total_lines = buffer.line_count();
        let pct = if total_lines > 0 {
            ((line as f64 / total_lines as f64) * 100.0) as usize
        } else {
            100
        };
        execute!(stdout, SetForegroundColor(overlay0))?;
        execute!(stdout, Print(&format!(" {}% ", pct)))?;

        // Right side: encoding + filetype + buffer position
        let ft_text = buffer
            .language
            .map(|l| l.as_str().to_string())
            .unwrap_or_else(|| "text".to_string());
        let buf_pos = format!(" {} {} ", glyphs::BUFFER_ICON, editor.buffers.len());

        let left_approx = mode_text.len()
            + 2
            + glyphs::SEPARATOR_LEFT.chars().count()
            + buffer.display_name().len()
            + 3
            + glyphs::SEPARATOR_LEFT.chars().count()
            + format!(" {}:{} ", line, col).len()
            + glyphs::SEPARATOR_LEFT.chars().count()
            + format!(" {}% ", pct).len();
        let right_approx = " UTF-8 ".len()
            + ft_text.len()
            + glyphs::SEPARATOR_RIGHT.chars().count()
            + buf_pos.len()
            + glyphs::SEPARATOR_RIGHT.chars().count();
        let padding = (term_width as usize).saturating_sub(left_approx + right_approx);

        execute!(stdout, SetBackgroundColor(surface2))?;
        execute!(stdout, Print(&" ".repeat(padding)))?;

        execute!(
            stdout,
            SetForegroundColor(surface0),
            SetBackgroundColor(surface2)
        )?;
        execute!(stdout, Print(glyphs::SEPARATOR_RIGHT))?;

        execute!(
            stdout,
            SetForegroundColor(subtext),
            SetBackgroundColor(surface0)
        )?;
        execute!(stdout, Print(&format!(" UTF-8 {} ", ft_text)))?;

        execute!(
            stdout,
            SetForegroundColor(surface1),
            SetBackgroundColor(surface0)
        )?;
        execute!(stdout, Print(glyphs::SEPARATOR_RIGHT))?;

        execute!(
            stdout,
            SetForegroundColor(subtext),
            SetBackgroundColor(surface1)
        )?;
        execute!(stdout, Print(&buf_pos))?;
    } else {
        execute!(stdout, SetBackgroundColor(surface0))?;
        execute!(stdout, Print(&" ".repeat(term_width as usize)))?;
    }

    execute!(stdout, ResetColor)?;

    // ── Line 2: Command input / Search / Messages ──
    execute!(stdout, MoveTo(0, cmdline_y))?;
    execute!(stdout, SetBackgroundColor(surface0))?;

    if editor.search_input_active {
        let prefix = match editor.search_direction {
            Some(SearchDirection::Forward) => "/",
            Some(SearchDirection::Backward) => "?",
            None => "/",
        };
        execute!(stdout, SetForegroundColor(yellow))?;
        execute!(stdout, Print(prefix))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(safe!(&editor.search_prompt.buffer)))?;

        let printed = 1 + UnicodeWidthStr::width(editor.search_prompt.buffer.as_str());

        let feedback = if let Some(ref msg) = editor.error_message {
            Some((msg.clone(), red))
        } else {
            editor
                .status_message
                .as_ref()
                .map(|msg| (msg.clone(), green))
        };

        if let Some((ref text, color)) = feedback {
            let feedback_width = UnicodeWidthStr::width(text.as_str());
            let padding = (term_width as usize).saturating_sub(printed + feedback_width + 2);
            if padding > 0 {
                execute!(stdout, Print(&" ".repeat(padding)))?;
            }
            execute!(stdout, SetForegroundColor(color))?;
            execute!(stdout, Print(text))?;
            execute!(stdout, Print("  "))?;
        } else {
            let remaining = (term_width as usize).saturating_sub(printed);
            if remaining > 0 {
                execute!(stdout, Print(&" ".repeat(remaining)))?;
            }
        }
    } else if editor.mode == Mode::Command {
        execute!(stdout, SetForegroundColor(yellow))?;
        execute!(stdout, Print(":"))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(safe!(&editor.command_prompt.buffer)))?;

        let printed = 1 + UnicodeWidthStr::width(editor.command_prompt.buffer.as_str());
        let remaining = (term_width as usize).saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if editor.mode == Mode::LlmPrompt {
        let preset_label = editor
            .llm_active_preset
            .map(|p| format!(" [{}]", p))
            .unwrap_or_default();
        let prompt_text = format!(">{}", preset_label);
        execute!(stdout, SetForegroundColor(blue))?;
        execute!(stdout, Print(&prompt_text))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(&editor.command_prompt.buffer))?;

        let printed = UnicodeWidthStr::width(prompt_text.as_str())
            + UnicodeWidthStr::width(editor.command_prompt.buffer.as_str());
        let remaining = (term_width as usize).saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if let Some(ref msg) = editor.error_message {
        let msg_text = format!(" {}", msg);
        execute!(stdout, SetForegroundColor(red))?;
        execute!(stdout, Print(safe!(&msg_text)))?;

        let printed = UnicodeWidthStr::width(msg_text.as_str());
        let remaining = (term_width as usize).saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if let Some(ref msg) = editor.status_message {
        let msg_text = format!(" {}", msg);
        execute!(stdout, SetForegroundColor(green))?;
        execute!(stdout, Print(safe!(&msg_text)))?;

        let printed = UnicodeWidthStr::width(msg_text.as_str());
        let remaining = (term_width as usize).saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else {
        let remaining = term_width as usize;
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    }

    execute!(stdout, ResetColor)?;

    // ── Line 3: Clear the bottom bar to prevent leftover ghost characters ──
    execute!(stdout, MoveTo(0, infobar_y))?;
    execute!(stdout, SetBackgroundColor(surface0))?;
    execute!(stdout, Print(&" ".repeat(term_width as usize)))?;
    execute!(stdout, ResetColor)?;

    Ok(())
}

// -----------------------------------------------------------------------------
// Popup renderers using rounded_box
// -----------------------------------------------------------------------------
fn render_float_popup(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    popup: &FloatPopup,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_height = 3;

    let pw = popup.width.min(term_width.saturating_sub(4));
    let max_rows = popup
        .max_height
        .min(term_height.saturating_sub(status_height).saturating_sub(2));
    let display_lines: Vec<&str> = popup
        .lines
        .iter()
        .take(max_rows as usize)
        .map(|s| s.as_str())
        .collect();
    let content_rows = display_lines.len();
    let total_height = (content_rows + 2) as u16;

    // ── Bottom-right: grow upward from status bar, 1-col margin on right ──
    let x = term_width.saturating_sub(pw).saturating_sub(1);
    let y = term_height
        .saturating_sub(status_height)
        .saturating_sub(total_height);

    clear_rect(stdout, x, y, pw, total_height, catppuccin::MANTLE)?;

    let border_style = BoxStyle::default()
        .with_title(popup.title.clone())
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::MANTLE);

    draw_border(stdout, x, y, pw, total_height, &border_style)?;

    // ── Render each line with shortcut hints if in shortcut mode ──

    for (i, line) in display_lines.iter().enumerate() {
        let row_y = y + 1 + i as u16;

        let row_style = RowStyle::normal()
            .with_border(catppuccin::SURFACE2)
            .with_bg(catppuccin::MANTLE);

        if editor.shortcut_active {
            // Line format from shortcuts.rs: "  {key:<width$}  {description}"
            let trimmed = line.trim_start();
            let (key_str, display_text) = match trimmed.split_once(|c: char| c.is_whitespace()) {
                Some((k, rest)) => (k, rest.trim_start()),
                None => (trimmed, ""),
            };

            let mut segments = vec![Segment::new("[", catppuccin::TEXT)];

            // Highlight the already-typed prefix vs remaining keys
            let pending_str = crate::misc::format_shortcut_keys(&editor.shortcut_pending_keys);

            if !pending_str.is_empty() && key_str.starts_with(&pending_str) {
                // Highlight the typed part (e.g. "g")
                segments.push(Segment::new(&pending_str, catppuccin::PEACH));
                // Dim the remaining part (e.g. ",x")
                let remaining = &key_str[pending_str.len()..];
                if !remaining.is_empty() {
                    segments.push(Segment::new(remaining, catppuccin::OVERLAY0));
                }
            } else {
                // No pending prefix, show the full key sequence in green
                segments.push(Segment::new(key_str, catppuccin::GREEN));
            }

            segments.push(Segment::new("] ", catppuccin::TEXT));
            segments.push(Segment::new(display_text, catppuccin::TEXT));

            draw_row(stdout, x, row_y, pw, &segments, &row_style)?;
        } else {
            draw_row_text(stdout, x, row_y, pw, line, &row_style)?;
        }
    }

    Ok(())
}

fn render_diff_popup(
    _editor: &Editor,
    stdout: &mut std::io::Stdout,
    popup: &crate::ed::DiffPopup,
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

fn render_completion_popup(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_height = 3;
    let edit_height = term_height.saturating_sub(status_height);

    let items = &editor.completion.items;
    let selected = editor.completion.selected_index;
    let trigger = editor
        .completion
        .context
        .as_ref()
        .map(|c| c.trigger.as_str())
        .unwrap_or("");

    if items.is_empty() {
        return Ok(());
    }

    let max_visible = 8usize;
    let visible_count = items.len().min(max_visible);

    let mut max_item_width = trigger.len();
    for item in items.iter().take(visible_count) {
        let kind_label = item.kind.as_str();
        let detail_w = item.detail.as_deref().map(|d| d.len() + 3).unwrap_or(0);
        let w = kind_label.len() + 2 + item.text.len() + detail_w;
        max_item_width = max_item_width.max(w);
    }
    let popup_content_width = (max_item_width + 4).min((term_width - 4) as usize) as u16;
    let popup_width = (popup_content_width + 2).max(40);
    let popup_height = (visible_count as u16) + 2;

    // Cursor position for anchoring
    let window = editor.windows.active_window();

    // ── Gutter widths (must be computed BEFORE cursor_screen_row closure) ──
    let gutter_w = if editor.config.line_numbers { 5u16 } else { 0 };
    let mark_gutter_w = {
        let current_bid = if let Some(w) = editor.windows.active_window() {
            w.buffer_id
        } else {
            crate::buffer::BufferId::default()
        };
        if editor.marks.iter().any(|(_, (bid, _))| *bid == current_bid) {
            1u16
        } else {
            0u16
        }
    };
    let git_gutter_w = if editor.git_gutter_enabled && editor.config.enable_git {
        1u16
    } else {
        0u16
    };

    let cursor_screen_row = window
        .map(|w| {
            let vp = &w.viewport;
            let cursor_line = w.cursor.position.line;
            let cursor_col = w.cursor.position.col;
            let buffer = editor.buffers.get(&w.buffer_id);
            let content_width = (w
                .width
                .saturating_sub(if editor.config.line_numbers { 5 } else { 0 })
                .saturating_sub(mark_gutter_w)
                .saturating_sub(if editor.git_gutter_enabled && editor.config.enable_git {
                    1
                } else {
                    0
                })) as usize;
            let mut visual_rows = 0usize;
            if let Some(buf) = buffer {
                for line_i in vp.scroll_line..cursor_line {
                    if let Some(txt) = buf.line_text(line_i) {
                        if editor.config.word_wrap {
                            visual_rows += softwrap_rows(&txt, content_width);
                        } else {
                            visual_rows += 1;
                        }
                    }
                }
                if let Some(cl_txt) = buf.line_text(cursor_line) {
                    if editor.config.word_wrap {
                        visual_rows += softwrap_row_offset(&cl_txt, cursor_col, content_width);
                    }
                }
            }
            w.y_offset + visual_rows as u16
        })
        .unwrap_or(0);

    let gutter_w = if editor.config.line_numbers { 5u16 } else { 0 };
    let mark_gutter_w = {
        let current_bid = if let Some(w) = editor.windows.active_window() {
            w.buffer_id
        } else {
            crate::buffer::BufferId::default()
        };
        if editor.marks.iter().any(|(_, (bid, _))| *bid == current_bid) {
            1u16
        } else {
            0u16
        }
    };
    let git_gutter_w = if editor.git_gutter_enabled && editor.config.enable_git {
        1u16
    } else {
        0u16
    };
    let cursor_screen_col = window
        .map(|w| {
            let scroll_col = w.viewport.scroll_col as usize;
            let col = w.cursor.position.col.saturating_sub(scroll_col);
            w.x_offset + gutter_w + mark_gutter_w + git_gutter_w + col as u16
        })
        .unwrap_or(0);

    let x = (cursor_screen_col as usize)
        .min((term_width as usize).saturating_sub(popup_width as usize)) as u16;
    let y = if cursor_screen_row > popup_height {
        cursor_screen_row.saturating_sub(popup_height) - 1
    } else {
        (cursor_screen_row + 1).min(edit_height.saturating_sub(popup_height))
    };

    let scroll_offset = if selected >= max_visible {
        selected - max_visible + 1
    } else {
        0
    };

    clear_rect(stdout, x, y, popup_width, popup_height, catppuccin::MANTLE)?;

    let title_style = BoxStyle::default()
        .with_title("Completion")
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    for (i, item_idx) in (scroll_offset..(scroll_offset + visible_count)).enumerate() {
        let row_y = y + 1 + i as u16;
        if row_y >= y + popup_height - 1 {
            break;
        }
        let item = match items.get(item_idx) {
            Some(it) => it,
            None => break,
        };
        let is_selected = item_idx == selected;

        let mut segments = Vec::new();
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
        segments.push(Segment::new(" ", catppuccin::SUBTEXT));
        segments.push(Segment::new(
            &item.text,
            if is_selected {
                catppuccin::TEXT
            } else {
                catppuccin::SUBTEXT
            },
        ));
        if let Some(detail) = &item.detail {
            segments.push(Segment::new(" ", catppuccin::SUBTEXT));
            segments.push(Segment::new(detail, catppuccin::OVERLAY0));
        }

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
    }

    let bottom_y = y + 1 + visible_count as u16;
    let bottom_style = BoxStyle::default().with_border(catppuccin::SURFACE2);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    // Info line below popup
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
        execute!(stdout, Print(info_display))?;
        let pad = (popup_width as usize).saturating_sub(UnicodeWidthStr::width(info_display));
        if pad > 0 {
            execute!(stdout, Print(&" ".repeat(pad)))?;
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Internal render helpers
// -----------------------------------------------------------------------------

/// Render all windows unconditionally (used for full redraw).
fn render_all_windows(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    highlighter: &mut Highlighter,
) -> Result<(), Box<dyn std::error::Error>> {
    for window in editor.windows.iter() {
        render_window(
            editor,
            stdout,
            window,
            window.x_offset,
            window.y_offset,
            window.width,
            window.height,
            highlighter,
        )?;
    }
    Ok(())
}

/// Render window separators when multiple windows exist.
fn render_separators(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    if editor.windows.len() <= 1 {
        return Ok(());
    }
    let edit_height = term_height.saturating_sub(3);
    let separators = editor.windows.compute_separators(term_width, term_height);
    for sep in &separators {
        use crate::window::SplitDirection;
        match sep.direction {
            SplitDirection::Horizontal => {
                if sep.y < edit_height {
                    execute!(
                        stdout,
                        MoveTo(sep.x, sep.y),
                        SetForegroundColor(Color::DarkGrey),
                        Print("\u{2500}".repeat(sep.length as usize)),
                        ResetColor
                    )?;
                }
            }
            SplitDirection::Vertical => {
                for row in 0..sep.length {
                    let sy = sep.y + row;
                    if sy < edit_height {
                        execute!(
                            stdout,
                            MoveTo(sep.x, sy),
                            SetForegroundColor(Color::DarkGrey),
                            Print("\u{2502}"),
                            ResetColor
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn set_cursor_style(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
) -> Result<(), Box<dyn std::error::Error>> {
    match editor.mode {
        Mode::Normal => {
            // Change from BlinkingBlock to SteadyBlock – no flash
            let _ = execute!(stdout, crossterm::cursor::SetCursorStyle::SteadyBlock);
        }
        Mode::Insert | Mode::Replace => {
            let _ = execute!(stdout, crossterm::cursor::SetCursorStyle::BlinkingBar);
        }
        Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
            let _ = execute!(stdout, crossterm::cursor::SetCursorStyle::SteadyBlock);
        }
        Mode::LlmPrompt => {
            let _ = execute!(stdout, crossterm::cursor::SetCursorStyle::BlinkingBar);
        }
        _ => {
            let _ = execute!(
                stdout,
                crossterm::cursor::SetCursorStyle::BlinkingUnderScore
            );
        }
    }
    Ok(())
}

/// Position the terminal cursor at the correct screen location.
fn position_cursor(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let edit_height = term_height.saturating_sub(3);

    if editor.search_input_active {
        let cmdline_y = term_height.saturating_sub(2);
        // Calculate display width of text up to cursor
        let before_cursor = &editor.search_prompt.buffer[..editor.search_prompt.cursor];
        let cursor_col = 1 + UnicodeWidthStr::width(before_cursor) as u16;
        let cursor_col = cursor_col.min(term_width.saturating_sub(1));
        execute!(
            stdout,
            MoveTo(cursor_col, cmdline_y),
            crossterm::cursor::Show
        )?;
    } else if editor.mode == Mode::Command || editor.mode == Mode::LlmPrompt {
        let cmdline_y = term_height.saturating_sub(2);
        let prompt_len = if editor.mode == Mode::LlmPrompt {
            let preset_label = editor
                .llm_active_preset
                .map(|p| format!(" [{}]", p))
                .unwrap_or_default();
            1 + UnicodeWidthStr::width(preset_label.as_str()) as u16
        } else {
            1
        };

        let prompt = if editor.mode == Mode::LlmPrompt {
            &editor.llm_prompt
        } else {
            &editor.command_prompt
        };

        let before_cursor = &prompt.buffer[..prompt.cursor];
        let cursor_col = prompt_len + UnicodeWidthStr::width(before_cursor) as u16;
        let cursor_col = cursor_col.min(term_width.saturating_sub(1));
        execute!(
            stdout,
            MoveTo(cursor_col, cmdline_y),
            crossterm::cursor::Show
        )?;
    } else if let Some(window) = editor.windows.active_window() {
        let gutter_w = if editor.config.line_numbers { 5u16 } else { 0 };
        let mark_gutter_w = {
            let current_bid = window.buffer_id;
            if editor.marks.iter().any(|(_, (bid, _))| *bid == current_bid) {
                1u16
            } else {
                0u16
            }
        };
        let git_gutter_w = if editor.git_gutter_enabled && editor.config.enable_git {
            1u16
        } else {
            0u16
        };

        let vp = &window.viewport;
        let cursor_line = window.cursor.position.line;
        let cursor_col = window.cursor.position.col;

        let buffer = editor.buffers.get(&window.buffer_id);
        let content_width = (window
            .width
            .saturating_sub(gutter_w + mark_gutter_w + git_gutter_w))
            as usize;
        let mut visual_rows_before_cursor = 0usize;
        if let Some(buf) = buffer {
            for line_i in vp.scroll_line..cursor_line {
                if let Some(text) = buf.line_text(line_i) {
                    if editor.config.word_wrap {
                        visual_rows_before_cursor += softwrap_rows(&text, content_width);
                    } else {
                        visual_rows_before_cursor += 1;
                    }
                }
            }
            if let Some(cursor_line_text) = buf.line_text(cursor_line) {
                if editor.config.word_wrap {
                    visual_rows_before_cursor +=
                        softwrap_row_offset(&cursor_line_text, cursor_col, content_width);
                }
            }
        }
        let abs_cursor_row = window.y_offset + visual_rows_before_cursor as u16;

        let mut display_col = cursor_col;
        if let Some(cursor_line_text) = buffer.and_then(|b| b.line_text(cursor_line)) {
            if editor.config.word_wrap {
                display_col = softwrap_display_col(&cursor_line_text, cursor_col, content_width);
            }
        }
        let display_col = display_col.saturating_sub(vp.scroll_col as usize);
        let abs_cursor_col =
            window.x_offset + gutter_w + mark_gutter_w + git_gutter_w + display_col as u16;

        if abs_cursor_row < edit_height && abs_cursor_col < term_width {
            execute!(
                stdout,
                MoveTo(abs_cursor_col, abs_cursor_row),
                crossterm::cursor::Show
            )?;
        }
    }

    Ok(())
}
// -----------------------------------------------------------------------------
// Main render entry point
// -----------------------------------------------------------------------------
pub fn render(
    editor: &mut Editor,
    terminal: &mut Terminal,
    highlighter: &mut Highlighter,
) -> Result<(), Box<dyn std::error::Error>> {
    let (term_width, term_height) = terminal.size().unwrap_or((80, 24));
    let stdout = terminal.stdout_mut();
    // ── Hide cursor for the entire frame ──────────────────────────────────
    let _ = execute!(stdout, crossterm::cursor::Hide);

    let _edit_height = term_height.saturating_sub(3);

    // ── 1. Restore editor content if a popup was closed ──
    if let Some(rect) = editor.dirty.restore_rect {
        restore_region(editor, stdout, rect, highlighter)?;
        // Separators might need redrawing too
        if editor.windows.len() > 1 {
            render_separators(editor, stdout, term_width, term_height)?;
        }
    }

    // ── 2. Cursor style ──
    if editor.dirty.full || editor.dirty.cursor || editor.dirty.windows {
        set_cursor_style(editor, stdout)?;
    }

    // ── 3. Editor windows (only if dirty, NOT when only popups changed) ──
    let must_draw_windows = editor.dirty.full || editor.dirty.windows;
    if must_draw_windows {
        let window_count = editor.windows.len();
        if window_count == 0 {
            execute!(stdout, MoveTo(0, 0))?;
            execute!(stdout, SetForegroundColor(Color::Cyan))?;
            execute!(stdout, Print("  riv-core — Vim-like Text Editor"))?;
            execute!(stdout, ResetColor)?;
            execute!(stdout, MoveTo(0, 2))?;
            execute!(stdout, Print("  Welcome to riv! Press :q to quit."))?;
        } else {
            for window in editor.windows.iter() {
                render_window(
                    editor,
                    stdout,
                    window,
                    window.x_offset,
                    window.y_offset,
                    window.width,
                    window.height,
                    highlighter,
                )?;
            }
            if window_count > 1 {
                render_separators(editor, stdout, term_width, term_height)?;
            }
        }
    }

    // ── 4. Status line ──
    if editor.dirty.full || editor.dirty.status_powerline {
        render_powerline(editor, stdout, term_width, term_height)?;
    }
    if editor.dirty.full || editor.dirty.status_cmdline {
        render_cmdline(editor, stdout, term_width, term_height)?;
    }
    if editor.dirty.full || editor.dirty.status_infobar {
        render_infobar(editor, stdout, term_width, term_height)?;
    }

    // ── 5. Popups ──
    // If windows were redrawn, ALL active popups must be redrawn on top.
    let must_draw_all_popups = editor.dirty.full || editor.dirty.windows;

    // Float popup
    if must_draw_all_popups || editor.dirty.float {
        if let Some(popup) = &editor.float_popup {
            render_float_popup(editor, stdout, popup, term_width, term_height)?;
        }
    }

    // Register popup (bottom-up)
    if must_draw_all_popups || editor.register_popup.is_some() {
        render_register_popup(editor, stdout, term_width, term_height)?;
    }

    // Mark list popup (bottom-up)
    if must_draw_all_popups || editor.dirty.mark_list {
        if let Some(popup) = &editor.mark_list_popup {
            render_mark_list_popup(popup, stdout, term_width, term_height)?;
        }
    }
    // Format info popup
    if must_draw_all_popups || editor.fmt_info_popup.is_some() {
        render_fmtinfo(editor, stdout, term_width, term_height)?;
    }

    // Keymap popup
    if must_draw_all_popups || editor.dirty.help {
        if let Some(popup) = &editor.keymap_popup {
            crate::popup::render_keymap_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // Diff popup
    if must_draw_all_popups || editor.dirty.diff {
        if let Some(popup) = &editor.diff_popup {
            render_diff_popup(editor, stdout, popup, term_width, term_height)?;
        }
    }

    // Completion popup (special: high-frequency, never use clear_rect)
    if (must_draw_all_popups || editor.dirty.completion)
        && editor.completion.active
        && !editor.completion.items.is_empty()
    {
        render_completion_popup(editor, stdout, term_width, term_height)?;
    }

    // Help popup
    if must_draw_all_popups || editor.dirty.help {
        if let Some(popup) = &editor.help_popup {
            crate::popup::render_help_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // File picker
    if must_draw_all_popups || editor.dirty.file_picker {
        if let Some(picker) = &editor.file_picker {
            crate::popup::render_file_picker(picker, stdout, term_width, term_height)?;
        }
    }

    // Buffer list
    if must_draw_all_popups || editor.dirty.buffer_list {
        if let Some(popup) = &editor.buffer_list_popup {
            crate::popup::render_buffer_list_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // MRU popup
    if must_draw_all_popups || editor.dirty.mru {
        if let Some(popup) = &editor.mru_popup {
            crate::popup::render_mru_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // Function list popup
    if must_draw_all_popups || editor.dirty.function_list {
        if let Some(_popup) = &editor.function_list_popup {
            render_function_list_popup(editor, stdout, term_width, term_height)?;
        }
    }

    // ── 6. Cursor positioning ──
    if editor.dirty.full || editor.dirty.cursor {
        position_cursor(editor, stdout, term_width, term_height)?;
    }
    editor.dirty.clear();
    Ok(())
}

/// Render editor windows that overlap with the given screen region.
fn restore_region(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    rect: Rect,
    highlighter: &mut Highlighter,
) -> Result<(), Box<dyn std::error::Error>> {
    for window in editor.windows.iter() {
        let wx = window.x_offset;
        let wy = window.y_offset;
        let ww = window.width;
        let wh = window.height;

        // Skip windows that don't overlap
        if rect.x + rect.w <= wx || rect.x >= wx + ww || rect.y + rect.h <= wy || rect.y >= wy + wh
        {
            continue;
        }

        render_window(editor, stdout, window, wx, wy, ww, wh, highlighter)?;
    }
    Ok(())
}

fn render_function_list_popup(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let popup = match &editor.function_list_popup {
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

// -----------------------------------------------------------------------------
// Status line rendering (split into 3 semantic regions)
// -----------------------------------------------------------------------------

fn render_powerline(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::powerline::{self, glyphs};

    let surface0 = powerline::crossterm_colors::SURFACE0;
    let surface1 = powerline::crossterm_colors::SURFACE1;
    let surface2 = powerline::crossterm_colors::SURFACE2;
    let overlay0 = powerline::crossterm_colors::OVERLAY0;
    let text = powerline::crossterm_colors::TEXT;
    let subtext = powerline::crossterm_colors::SUBTEXT;
    let peach = powerline::crossterm_colors::PEACH;
    let green = powerline::crossterm_colors::GREEN;
    let yellow = powerline::crossterm_colors::YELLOW;

    let powerline_y = term_height.saturating_sub(3);

    execute!(stdout, MoveTo(0, powerline_y))?;

    let (mode_fg, mode_bg) = powerline::get_mode_colors_crossterm(editor);
    let mode_sep = powerline::get_mode_sep_color_crossterm(editor);
    let mode_text = editor.mode_display();

    // Segment 1: Mode badge
    execute!(
        stdout,
        SetForegroundColor(mode_fg),
        SetBackgroundColor(mode_bg)
    )?;
    execute!(stdout, Print(&format!(" {} ", mode_text)))?;

    execute!(
        stdout,
        SetForegroundColor(mode_sep),
        SetBackgroundColor(surface0)
    )?;
    execute!(stdout, Print(glyphs::SEPARATOR_LEFT))?;

    // Segment 2: Filename + dirty indicator
    execute!(stdout, SetBackgroundColor(surface0))?;
    if let Some(buffer) = editor.current_buffer() {
        execute!(stdout, SetForegroundColor(text))?;
        execute!(
            stdout,
            Print(safe!(&format!(" {} ", buffer.display_name())))
        )?;

        if buffer.is_dirty() {
            execute!(stdout, SetForegroundColor(peach))?;
            execute!(stdout, Print(&format!(" {} ", glyphs::DIRTY)))?;
        } else {
            execute!(stdout, SetForegroundColor(green))?;
            execute!(stdout, Print(&format!(" {} ", glyphs::CLEAN)))?;
        }

        execute!(
            stdout,
            SetForegroundColor(surface0),
            SetBackgroundColor(surface1)
        )?;
        execute!(stdout, Print(glyphs::SEPARATOR_LEFT))?;

        // Segment 3: Position
        let window = editor.windows.active_window();
        let line = window.map(|w| w.cursor.position.line + 1).unwrap_or(1);
        let col = window.map(|w| w.cursor.position.col + 1).unwrap_or(1);
        execute!(stdout, SetForegroundColor(subtext))?;
        execute!(stdout, Print(&format!(" {}:{} ", line, col)))?;

        execute!(
            stdout,
            SetForegroundColor(surface1),
            SetBackgroundColor(surface2)
        )?;
        execute!(stdout, Print(glyphs::SEPARATOR_LEFT))?;

        // Segment 4: Percentage
        let total_lines = buffer.line_count();
        let pct = if total_lines > 0 {
            ((line as f64 / total_lines as f64) * 100.0) as usize
        } else {
            100
        };
        execute!(stdout, SetForegroundColor(overlay0))?;
        execute!(stdout, Print(&format!(" {}% ", pct)))?;

        // ── Right side ──────────────────────────────────────────────
        let ft_text = buffer
            .language
            .map(|l| l.as_str().to_string())
            .unwrap_or_else(|| "text".to_string());
        let buf_pos = format!(" {} {} ", glyphs::BUFFER_ICON, editor.buffers.len());

        // Function name segment (optional)
        let func_display = editor.current_function_name.as_deref().map(|name| {
            if name.chars().count() > 25 {
                format!("{}…", name.chars().take(24).collect::<String>())
            } else {
                name.to_string()
            }
        });

        // Calculate widths for padding
        let func_width: usize = func_display
            .as_deref()
            .map(|d| {
                glyphs::SEPARATOR_RIGHT.chars().count()
                    + format!(" {} {} ", glyphs::FUNCTION, d).chars().count()
                    + glyphs::SEPARATOR_RIGHT.chars().count()
            })
            .unwrap_or(glyphs::SEPARATOR_RIGHT.chars().count());

        let left_approx = format!(" {} ", mode_text).chars().count()
            + glyphs::SEPARATOR_LEFT.chars().count()
            + format!(" {} ", buffer.display_name()).chars().count()
            + format!(" {} ", glyphs::DIRTY).chars().count()
            + glyphs::SEPARATOR_LEFT.chars().count()
            + format!(" {}:{} ", line, col).chars().count()
            + glyphs::SEPARATOR_LEFT.chars().count()
            + format!(" {}% ", pct).chars().count();
        let right_approx = func_width
            + format!(" UTF-8 {} ", ft_text).chars().count()
            + glyphs::SEPARATOR_RIGHT.chars().count()
            + buf_pos.chars().count();

        let padding = (term_width as usize).saturating_sub(left_approx + right_approx);

        // Padding between left and right
        execute!(stdout, SetBackgroundColor(surface2))?;
        execute!(stdout, Print(&" ".repeat(padding)))?;

        // Function name (or plain separator if absent)
        if let Some(ref display) = func_display {
            // SURFACE2 → SURFACE1
            execute!(
                stdout,
                SetForegroundColor(surface1),
                SetBackgroundColor(surface2)
            )?;
            execute!(stdout, Print(glyphs::SEPARATOR_RIGHT))?;
            execute!(
                stdout,
                SetForegroundColor(yellow),
                SetBackgroundColor(surface1)
            )?;
            execute!(
                stdout,
                Print(&format!(" {} {} ", glyphs::FUNCTION, display))
            )?;
            // SURFACE1 → SURFACE0
            execute!(
                stdout,
                SetForegroundColor(surface0),
                SetBackgroundColor(surface1)
            )?;
            execute!(stdout, Print(glyphs::SEPARATOR_RIGHT))?;
        } else {
            // No function: SURFACE2 → SURFACE0 directly
            execute!(
                stdout,
                SetForegroundColor(surface0),
                SetBackgroundColor(surface2)
            )?;
            execute!(stdout, Print(glyphs::SEPARATOR_RIGHT))?;
        }

        // Filetype
        execute!(
            stdout,
            SetForegroundColor(subtext),
            SetBackgroundColor(surface0)
        )?;
        execute!(stdout, Print(&format!(" UTF-8 {} ", ft_text)))?;

        // SURFACE0 → SURFACE1
        execute!(
            stdout,
            SetForegroundColor(surface1),
            SetBackgroundColor(surface0)
        )?;
        execute!(stdout, Print(glyphs::SEPARATOR_RIGHT))?;

        // Buffer position
        execute!(
            stdout,
            SetForegroundColor(subtext),
            SetBackgroundColor(surface1)
        )?;
        execute!(stdout, Print(&buf_pos))?;
    } else {
        execute!(stdout, SetBackgroundColor(surface0))?;
        execute!(stdout, Print(&" ".repeat(term_width as usize)))?;
    }

    execute!(stdout, ResetColor)?;
    Ok(())
}
fn render_cmdline(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::powerline::crossterm_colors;

    let surface0 = crossterm_colors::SURFACE0;
    let text = crossterm_colors::TEXT;
    let yellow = crossterm_colors::YELLOW;
    let red = crossterm_colors::RED;
    let green = crossterm_colors::GREEN;
    let blue = crossterm_colors::BLUE;

    let cmdline_y = term_height.saturating_sub(2);
    let max_width = term_width as usize;

    execute!(stdout, MoveTo(0, cmdline_y))?;
    execute!(stdout, SetBackgroundColor(surface0))?;

    // ── Register prefix visual feedback ──
    if editor.register_pending {
        execute!(stdout, SetForegroundColor(yellow))?;
        execute!(stdout, Print("\""))?;
        let remaining = max_width.saturating_sub(1);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if editor.pending_register.is_some() {
        execute!(stdout, SetForegroundColor(yellow))?;
        let reg_str = format!("\"{}", editor.pending_register.unwrap());
        execute!(stdout, Print(&reg_str))?;
        let remaining = max_width.saturating_sub(reg_str.len());
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if editor.search_input_active {
        let prefix = match editor.search_direction {
            Some(SearchDirection::Forward) => "/",
            Some(SearchDirection::Backward) => "?",
            None => "/",
        };
        execute!(stdout, SetForegroundColor(yellow))?;
        execute!(stdout, Print(prefix))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(safe!(&editor.search_prompt.buffer)))?;

        let printed = 1 + UnicodeWidthStr::width(editor.search_prompt.buffer.as_str());

        // Sanitize feedback to single line, max 120 chars, fitting term_width
        let feedback = if let Some(ref msg) = editor.error_message {
            Some((sanitize_single_line(msg, 120, max_width), red))
        } else {
            editor
                .status_message
                .as_ref()
                .map(|msg| (sanitize_single_line(msg, 120, max_width), green))
        };

        if let Some((ref ftext, color)) = feedback {
            let feedback_width = UnicodeWidthStr::width(ftext.as_str());
            let padding = max_width.saturating_sub(printed + feedback_width + 2);
            if padding > 0 {
                execute!(stdout, Print(&" ".repeat(padding)))?;
            }
            execute!(stdout, SetForegroundColor(color))?;
            execute!(stdout, Print(ftext))?;
            execute!(stdout, Print("  "))?;
        } else {
            let remaining = max_width.saturating_sub(printed);
            if remaining > 0 {
                execute!(stdout, Print(&" ".repeat(remaining)))?;
            }
        }
    } else if editor.mode == Mode::Command {
        execute!(stdout, SetForegroundColor(yellow))?;
        execute!(stdout, Print(":"))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(&editor.command_prompt.buffer))?;

        let printed = 1 + UnicodeWidthStr::width(editor.command_prompt.buffer.as_str());
        let remaining = max_width.saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if editor.mode == Mode::LlmPrompt {
        let preset_label = editor
            .llm_active_preset
            .map(|p| format!(" [{}]", p))
            .unwrap_or_default();
        let prompt_text = format!(">{}", preset_label);
        execute!(stdout, SetForegroundColor(blue))?;
        execute!(stdout, Print(&prompt_text))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(&editor.llm_prompt.buffer))?;

        let printed = UnicodeWidthStr::width(prompt_text.as_str())
            + UnicodeWidthStr::width(editor.llm_prompt.buffer.as_str());
        let remaining = max_width.saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if editor.formatting_pending {
        execute!(stdout, SetForegroundColor(crossterm_colors::OVERLAY0))?;
        execute!(stdout, Print(" Formatting…"))?;
        let printed = " Formatting…".len();
        let remaining = max_width.saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if let Some(ref msg) = editor.error_message {
        let msg_text = format!(" {}", sanitize_single_line(msg, 120, max_width));
        execute!(stdout, SetForegroundColor(red))?;
        execute!(stdout, Print(safe!(&msg_text)))?;

        let printed = UnicodeWidthStr::width(msg_text.as_str());
        let remaining = max_width.saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if let Some(ref msg) = editor.status_message {
        let msg_text = format!(" {}", sanitize_single_line(msg, 120, max_width));
        execute!(stdout, SetForegroundColor(green))?;
        execute!(stdout, Print(safe!(&msg_text)))?;

        let printed = UnicodeWidthStr::width(msg_text.as_str());
        let remaining = max_width.saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else {
        if max_width > 0 {
            execute!(stdout, Print(&" ".repeat(max_width)))?;
        }
    }

    execute!(stdout, ResetColor)?;
    Ok(())
}

fn render_fmtinfo(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let lines = match &editor.fmt_info_popup {
        Some(l) => l,
        None => return Ok(()),
    };
    if lines.is_empty() || term_width == 0 {
        return Ok(());
    }

    let status_height = 3;
    let edit_height = (term_height.saturating_sub(status_height)) as usize;
    let max_visual_rows = edit_height / 2;
    let popup_width = term_width;
    let x = 0;

    let max_line_w = popup_width.saturating_sub(2) as usize; // content area inside borders

    // ── Pre-compute visual rows with color info ──
    struct VisualRow {
        text: String,
        color: Color,
    }

    let mut visual_rows: Vec<VisualRow> = Vec::new();
    let mut logical_count = 0usize;

    for line in lines {
        if visual_rows.len() >= max_visual_rows {
            break;
        }
        logical_count += 1;

        // Color-code by prefix
        let trimmed = line.trim_start();
        let color = if trimmed.starts_with("error") {
            catppuccin::RED
        } else if trimmed.starts_with("warning") {
            catppuccin::YELLOW
        } else if trimmed.starts_with("help") || trimmed.starts_with("note") {
            catppuccin::GREEN
        } else if trimmed.contains("-->") {
            catppuccin::BLUE
        } else if trimmed.starts_with('|') || trimmed.starts_with('^') {
            catppuccin::SUBTEXT
        } else {
            catppuccin::TEXT
        };

        // Wrap the line into visual rows
        if max_line_w == 0 || line.is_empty() {
            visual_rows.push(VisualRow {
                text: String::new(),
                color,
            });
        } else {
            let mut current_row = String::new();
            let mut current_width = 0usize;

            for g in line.graphemes(true) {
                let gw = UnicodeWidthStr::width(g);
                if current_width + gw > max_line_w && !current_row.is_empty() {
                    visual_rows.push(VisualRow {
                        text: std::mem::take(&mut current_row),
                        color,
                    });
                    if visual_rows.len() >= max_visual_rows {
                        break;
                    }
                    current_width = 0;
                }
                current_row.push_str(g);
                current_width += gw;
            }

            if visual_rows.len() < max_visual_rows && !current_row.is_empty() {
                visual_rows.push(VisualRow {
                    text: current_row,
                    color,
                });
            }
        }
    }

    let visible_count = visual_rows.len().min(max_visual_rows);
    let total_height = visible_count as u16 + 2; // +2 for border top/bottom

    let y = term_height
        .saturating_sub(status_height)
        .saturating_sub(total_height);

    clear_rect(stdout, x, y, popup_width, total_height, catppuccin::MANTLE)?;

    let title = format!(" {} ", editor.fmt_info_popup_title);
    let title_style = BoxStyle::default()
        .with_title(title)
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    for (i, vrow) in visual_rows.iter().take(visible_count).enumerate() {
        let row_y = y + 1 + i as u16;

        let row_style = RowStyle::normal()
            .with_border(catppuccin::SURFACE2)
            .with_bg(catppuccin::MANTLE);
        let segments = vec![Segment::new(&vrow.text, vrow.color)];
        draw_row(stdout, x, row_y, popup_width, &segments, &row_style)?;
    }

    let bottom_y = y + 1 + visible_count as u16;
    let footer = format!(
        "{} lines ({} visible) [Esc/q] close",
        logical_count, visible_count
    );
    let bottom_style = BoxStyle::default()
        .with_border(catppuccin::SURFACE2)
        .with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    Ok(())
}

pub fn render_mark_list_popup(
    popup: &MarkListPopup,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status_height = 3u16;
    let edit_height = term_height.saturating_sub(status_height);

    // Full width, bottom-up like register popup
    let popup_width = term_width;
    let x = 0u16;

    let max_visible = 15usize;
    let visible_count = popup.filtered.len().min(max_visible);
    // +3 for top border, filter row, and bottom border
    let total_height = visible_count as u16 + 3;

    if popup_width == 0 || edit_height == 0 || total_height == 0 {
        return Ok(());
    }

    let y = edit_height.saturating_sub(total_height);

    clear_rect(stdout, x, y, popup_width, total_height, catppuccin::MANTLE)?;

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
        .with_border(catppuccin::SURFACE2)
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
    let file_name_width: usize = 24;

    for i in 0..visible_count {
        let row_y = filter_y + 1 + i as u16;
        let entry_idx = scroll + i;

        if entry_idx < popup.filtered.len() {
            let real_idx = popup.filtered[entry_idx];
            let entry = &popup.entries[real_idx];
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

            let is_closed = entry.file_name == "[closed]";

            let name_str = format!(" {} ", entry.name);
            let name_color = if is_closed {
                catppuccin::OVERLAY0
            } else {
                catppuccin::MAUVE
            };

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

            let pos_str = format!("{}:{}", entry.line + 1, entry.col + 1);

            let mut segments = Vec::new();
            segments.push(Segment::new(&name_str, name_color));

            // File name with match highlighting
            if !popup.filter.is_empty() && !is_closed {
                if let Some((match_start, match_end)) =
                    crate::popup::case_insensitive_find(&displayed_name, &popup.filter)
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

            // Line preview
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
        }
    }

    // ── Bottom border with status ──────────────────────────────────────
    let bottom_y = filter_y + 1 + visible_count as u16;
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
        .with_border(catppuccin::SURFACE2)
        .with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &footer_style)?;

    execute!(stdout, ResetColor)?;
    Ok(())
}

fn render_register_popup(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let lines = match &editor.register_popup {
        Some(l) => l,
        None => return Ok(()),
    };
    if lines.is_empty() || term_width == 0 {
        return Ok(());
    }

    let status_height = 3;
    let edit_height = (term_height.saturating_sub(status_height)) as usize;

    // Dynamic max height: up to half the edit area for LLM responses,
    // or 10 lines for register listing
    let is_registers = editor.register_popup_title == "Registers";
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
    let title = format!(" {} ", editor.register_popup_title);
    let title_style = BoxStyle::default()
        .with_title(title)
        .with_border(catppuccin::SURFACE2)
        .with_bg(catppuccin::MANTLE);
    draw_top_border(stdout, x, y, popup_width, &title_style)?;

    for (i, line) in lines.iter().take(visible_count).enumerate() {
        let row_y = y + 1 + i as u16;
        let row_style = RowStyle::normal()
            .with_border(catppuccin::SURFACE2)
            .with_bg(catppuccin::MANTLE);

        let max_line_w = popup_width.saturating_sub(2) as usize;
        let display_line = if unicode_width::UnicodeWidthStr::width(line.as_str()) > max_line_w {
            let mut s = String::new();
            let mut w = 0;
            for c in line.chars() {
                w += unicode_width::UnicodeWidthStr::width(c.to_string().as_str());
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
        "".to_string()
    } else {
        format!("{}/{} [\"e] paste  [Esc] close", visible_count, lines.len())
    };
    let bottom_style = BoxStyle::default()
        .with_border(catppuccin::SURFACE2)
        .with_footer(footer);
    draw_bottom_border(stdout, x, bottom_y, popup_width, &bottom_style)?;

    Ok(())
}
fn render_infobar(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::powerline::crossterm_colors;

    let surface0 = crossterm_colors::SURFACE0;
    let overlay0 = crossterm_colors::OVERLAY0;
    let subtext = crossterm_colors::SUBTEXT;
    let green = crossterm_colors::GREEN;

    let infobar_y = term_height.saturating_sub(1);
    let max_width = term_width as usize;

    execute!(stdout, MoveTo(0, infobar_y))?;
    execute!(stdout, SetBackgroundColor(surface0))?;

    if !editor.which_key_hints.is_empty() {
        let max_col = max_width;
        let mut col = 0usize;

        for (i, (k, desc)) in editor.which_key_hints.iter().enumerate() {
            if i > 0 {
                if col + 2 > max_col {
                    break;
                }
                execute!(stdout, SetForegroundColor(overlay0))?;
                execute!(stdout, Print("  "))?;
                col += 2;
            }

            let key_text = format!("{}:", k);
            let key_w = UnicodeWidthStr::width(key_text.as_str());
            if col + key_w > max_col {
                break;
            }
            execute!(stdout, SetForegroundColor(green))?;
            execute!(stdout, Print(&key_text))?;
            col += key_w;

            let remaining = max_col.saturating_sub(col);
            if remaining == 0 {
                break;
            }
            let desc_display = truncate_to_width(desc, remaining);
            let desc_w = UnicodeWidthStr::width(desc_display);
            if desc_w > 0 {
                execute!(stdout, SetForegroundColor(overlay0))?;
                execute!(stdout, Print(&desc_display))?;
                col += desc_w;
            }

            if col >= max_col {
                break;
            }
        }

        let pad = max_col.saturating_sub(col);
        if pad > 0 {
            execute!(stdout, Print(&" ".repeat(pad)))?;
        }
    } else if let Some(ref msg) = editor.infobar_message {
        // NEW: show formatter errors and other infobar messages
        let display = sanitize_single_line(msg, 120, max_width);
        let printed = UnicodeWidthStr::width(display.as_str());
        execute!(stdout, SetForegroundColor(crossterm_colors::YELLOW))?;
        execute!(stdout, Print(&display))?;
        let pad = max_width.saturating_sub(printed);
        if pad > 0 {
            execute!(stdout, Print(&" ".repeat(pad)))?;
        }
    } else if let Some(ref sig) = editor.lsp_signature_help {
        execute!(stdout, SetForegroundColor(subtext))?;
        let sig_formatted = sig.format_for_infobar();
        let display = sanitize_single_line(&sig_formatted, 120, max_width);
        let printed = UnicodeWidthStr::width(display.as_str());
        execute!(stdout, Print(&display))?;
        let pad = max_width.saturating_sub(printed);
        if pad > 0 {
            execute!(stdout, Print(&" ".repeat(pad)))?;
        }
    } else {
        if max_width > 0 {
            execute!(stdout, Print(&" ".repeat(max_width)))?;
        }
    }

    execute!(stdout, ResetColor)?;
    Ok(())
}
