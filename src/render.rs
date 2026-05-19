//! All rendering logic for riv: windows, status line, popups, and softwrap layout.

use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::dirty::Rect;
use crate::editor::{Editor, FloatPopup, Mode, SearchDirection};
use crate::highlight::Highlighter;
use crate::misc::sanitize_single_line;
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
#[rustfmt::skip]
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
    let git_gutter_width = if editor.git.gutter_enabled && editor.config.enable_git {
        1u16
    } else {
        0u16
    };

    // ── Mark gutter (between line numbers and git gutter) ──
    let mark_at_line: std::collections::HashMap<usize, char> = editor.search.marks
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
                // ── unchanged ──
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
                // ── FIX: set wrap_start_grapheme for non-wrap mode ──
                wrap_start_grapheme = scroll_col;

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

            // Print line content with syntax highlighting + inline indent guides
            let guide_cols = if editor.config.indent_guides && wrap_row == 0 {
                guide_cols_for_line(editor, buffer, line_idx, wrap_start_grapheme, scroll_col)
            } else {
                std::collections::HashSet::new()
            };
            let guide_cols_opt = if guide_cols.is_empty() {
                None
            } else {
                Some(&guide_cols)
            };

            if wrap_row == 0 {
                crate::highlight::render_highlighted_line(
                    stdout,
                    &display,
                    &line_spans,
                    is_cursor_line,
                    guide_cols_opt,
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
                    None, // guides only on wrap_row == 0
                )?;
            }

            // Clear rest of line — account for indent guides rendered
            // beyond the text boundary (e.g. on empty lines).
            let display_w = UnicodeWidthStr::width(display.as_str());
            let max_guide_col = guide_cols.iter().max().map(|&c| c + 1).unwrap_or(0);
            let effective_w = display_w.max(max_guide_col);
            let remaining = content_width.saturating_sub(effective_w);

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
                        Mode::Command => (0, graphemes.len()),
                        _ => continue,
                    };
                    let vis_left = vis_left.min(graphemes.len());
                    let vis_right = vis_right.min(graphemes.len());
                    if vis_left < vis_right {
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

                            let mut sel_start_screen_col = 0usize;
                            for gi in 0..clip_left {
                                sel_start_screen_col += UnicodeWidthStr::width(graphemes[gi]);
                            }
                            if !editor.config.word_wrap {
                                sel_start_screen_col =
                                    sel_start_screen_col.saturating_sub(line_scroll_offset_w);
                            }
                            let sel_start_x = content_start_x as usize + sel_start_screen_col;
                            if sel_start_x < content_start_x as usize + content_width
                                && sel_display_w > 0
                                && sel_start_x >= content_start_x as usize
                            {
                                let max_sel_w =
                                    content_start_x as usize + content_width - sel_start_x;
                                let actual_sel_text = if sel_display_w > max_sel_w {
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
            } // ← selection rect block ENDS here

            // ── Search highlight overlay ─────────────────────────
            // NOTE: This MUST be outside the selection_rect block above,
            // otherwise highlights only appear during visual selection.
            if editor.search.highlight_enabled
                && !editor.search.matches.is_empty()
                && !editor.search.matches_dirty
                && editor.search.buffer_id == Some(buffer_id)
            {
                let pattern_len = editor.search.prompt.buffer.chars().count();
                if pattern_len > 0 {
                    for (m_idx, m_pos) in editor.search.matches.iter().enumerate() {
                        if m_pos.line != line_idx {
                            continue;
                        }

                        let vis_left = m_pos.col;
                        let vis_right = (m_pos.col + pattern_len).min(graphemes.len());
                        if vis_left >= vis_right {
                            continue;
                        }

                        let clip_left = vis_left.max(wrap_start_grapheme);
                        let wrap_end_grapheme =
                            wrap_start_grapheme + display.graphemes(true).count();
                        let clip_right = vis_right.min(wrap_end_grapheme);
                        if clip_left >= clip_right {
                            continue;
                        }

                        let hl_text: String = graphemes[clip_left..clip_right].join("");
                        let hl_display_w = UnicodeWidthStr::width(hl_text.as_str());
                        let content_start_x =
                            x_offset + gutter_width + mark_gutter_width + git_gutter_width;

                        let mut hl_start_screen_col = 0usize;
                        for gi in 0..clip_left {
                            hl_start_screen_col += UnicodeWidthStr::width(graphemes[gi]);
                        }
                        if !editor.config.word_wrap {
                            hl_start_screen_col =
                                hl_start_screen_col.saturating_sub(line_scroll_offset_w);
                        }

                        let hl_start_x = content_start_x as usize + hl_start_screen_col;

                        if hl_start_x >= content_start_x as usize + content_width
                            || hl_display_w == 0
                            || hl_start_x < content_start_x as usize
                        {
                            continue;
                        }

                        let max_hl_w = content_start_x as usize + content_width - hl_start_x;
                        let actual_hl_text = if hl_display_w > max_hl_w {
                            let mut truncated = String::new();
                            let mut w = 0usize;
                            for g in graphemes[clip_left..clip_right].iter() {
                                let gw = UnicodeWidthStr::width(*g);
                                if w + gw > max_hl_w {
                                    break;
                                }
                                truncated.push_str(g);
                                w += gw;
                            }
                            truncated
                        } else {
                            hl_text
                        };

                        if !actual_hl_text.is_empty() {
                            let (hl_bg, hl_fg) = if m_idx == editor.search.current_match {
                                (
                                    Color::Rgb {
                                        r: 249,
                                        g: 226,
                                        b: 175,
                                    }, // Catppuccin Yellow
                                    Color::Rgb {
                                        r: 30,
                                        g: 30,
                                        b: 46,
                                    }, // Catppuccin Mantle
                                )
                            } else {
                                (
                                    Color::Rgb {
                                        r: 55,
                                        g: 50,
                                        b: 35,
                                    }, // Dark warm
                                    Color::Rgb {
                                        r: 249,
                                        g: 226,
                                        b: 175,
                                    }, // Catppuccin Yellow
                                )
                            };
                            execute!(
                                stdout,
                                MoveTo(hl_start_x as u16, y),
                                SetBackgroundColor(hl_bg),
                                SetForegroundColor(hl_fg),
                                Print(&actual_hl_text),
                                ResetColor
                            )?;
                        }
                    }
                }
            }

            // ── Jump mode overlay (EasyMotion) ─────────────────────────
            if editor.jump.active && !editor.jump.targets.is_empty() {
                let is_active_phase = editor.jump.phase == crate::editor::JumpPhase::Active;

                for (t_idx, target) in editor.jump.targets.iter().enumerate() {
                    if target.line != line_idx {
                        continue;
                    }

                    let (vis_left, vis_right, overlay_text, bg_col, fg_col) = if is_active_phase {
                        // Active phase: draw the single-char label (a, b, c...)
                        if let Some(label) = editor
                            .jump
                            .labels
                            .iter()
                            .find(|(idx, _)| *idx == t_idx)
                            .map(|(_, l)| l.clone())
                        {
                            (
                                target.col,
                                target.col + 1,
                                label,
                                catppuccin::SURFACE0,
                                catppuccin::GREEN,
                            )
                        } else {
                            continue;
                        }
                    } else {
                        // PendingChar2 phase: highlight the 2-char pattern (like search highlight)
                        let len = editor.jump.input.chars().count();
                        (
                            target.col,
                            target.col + len,
                            editor.jump.input.clone(),
                            catppuccin::SURFACE0,
                            catppuccin::TEXT,
                        )
                    };

                    let vis_left = vis_left.min(graphemes.len());
                    let vis_right = vis_right.min(graphemes.len());

                    if vis_left < vis_right {
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

                            let mut sel_start_screen_col = 0usize;
                            for gi in 0..clip_left {
                                sel_start_screen_col += UnicodeWidthStr::width(graphemes[gi]);
                            }
                            if !editor.config.word_wrap {
                                sel_start_screen_col =
                                    sel_start_screen_col.saturating_sub(line_scroll_offset_w);
                            }

                            let sel_start_x = content_start_x as usize + sel_start_screen_col;

                            if sel_start_x < content_start_x as usize + content_width
                                && sel_display_w > 0
                                && sel_start_x >= content_start_x as usize
                            {
                                let max_sel_w =
                                    content_start_x as usize + content_width - sel_start_x;

                                // If the label (1 char) is shorter than the match (2 chars), pad it
                                // so the background highlight covers the entire match cleanly.
                                let actual_sel_text = if UnicodeWidthStr::width(
                                    overlay_text.as_str(),
                                ) < sel_display_w
                                {
                                    format!(
                                        "{}{}",
                                        overlay_text,
                                        " ".repeat(
                                            sel_display_w
                                                - UnicodeWidthStr::width(overlay_text.as_str())
                                        )
                                    )
                                } else if UnicodeWidthStr::width(overlay_text.as_str()) > max_sel_w
                                {
                                    let mut truncated = String::new();
                                    let mut w = 0usize;
                                    for g in overlay_text.graphemes(true) {
                                        let gw = UnicodeWidthStr::width(g);
                                        if w + gw > max_sel_w {
                                            break;
                                        }
                                        truncated.push_str(g);
                                        w += gw;
                                    }
                                    truncated
                                } else {
                                    overlay_text.clone()
                                };

                                if !actual_sel_text.is_empty() {
                                    execute!(
                                        stdout,
                                        MoveTo(sel_start_x as u16, y),
                                        SetBackgroundColor(bg_col),
                                        SetForegroundColor(fg_col),
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

#[rustfmt::skip]
fn render_status(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::cursor::MoveTo;
    use crossterm::execute;
    use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};

    use crate::powerline::{self, glyphs};
    use unicode_width::UnicodeWidthStr;

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

    if editor.search.input_active {
        let prefix = match editor.search.direction {
            Some(SearchDirection::Forward) => "/",
            Some(SearchDirection::Backward) => "?",
            None => "/",
        };
        execute!(stdout, SetForegroundColor(yellow))?;
        execute!(stdout, Print(prefix))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(safe!(&editor.search.prompt.buffer)))?;

        let printed = 1 + UnicodeWidthStr::width(editor.search.prompt.buffer.as_str());

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
        let preset_label = editor.llm.active_preset
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
#[rustfmt::skip]
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
#[rustfmt::skip]
fn position_cursor(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    term_width: u16,
    term_height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let edit_height = term_height.saturating_sub(3);

    if editor.search.input_active {
        let cmdline_y = term_height.saturating_sub(2);
        // Calculate display width of text up to cursor
        let before_cursor = &editor.search.prompt.buffer[..editor.search.prompt.cursor];
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
            let preset_label = editor.llm.active_preset
                .map(|p| format!(" [{}]", p))
                .unwrap_or_default();
            1 + UnicodeWidthStr::width(preset_label.as_str()) as u16
        } else {
            1
        };

        let prompt = if editor.mode == Mode::LlmPrompt {
            &editor.llm.prompt
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
        let display_col = if editor.config.word_wrap {
            let mut col = cursor_col;
            if let Some(cursor_line_text) = buffer.and_then(|b| b.line_text(cursor_line)) {
                col = softwrap_display_col(&cursor_line_text, cursor_col, content_width);
            }
            col.saturating_sub(vp.scroll_col as usize)
        } else {
            // No word wrap: compute actual display width of visible portion
            // between the horizontal scroll offset and the cursor.
            if let Some(cursor_line_text) = buffer.and_then(|b| b.line_text(cursor_line)) {
                let line_text = cursor_line_text.trim_end_matches('\n');
                let graphemes: Vec<_> = line_text.graphemes(true).collect();
                let start = (vp.scroll_col as usize).min(graphemes.len());
                let end = cursor_col.min(graphemes.len());
                graphemes[start..end]
                    .iter()
                    .map(|g| UnicodeWidthStr::width(*g))
                    .sum::<usize>()
            } else {
                cursor_col.saturating_sub(vp.scroll_col as usize)
            }
        };
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

/// Render exactly one line of the active window's buffer.
/// Used when the completion popup is active to avoid redrawing the
/// entire screen (which causes flicker under the popup).
#[rustfmt::skip]
fn render_single_buffer_line(
    editor: &Editor,
    stdout: &mut std::io::Stdout,
    abs_line: usize,
    highlighter: &mut Highlighter,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::cursor::MoveTo;
    use crossterm::execute;
    use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
    use unicode_width::UnicodeWidthStr;

    let window = match editor.windows.active_window() {
        Some(w) => w,
        None => return Ok(()),
    };

    let buffer_id = window.buffer_id;
    let buffer = match editor.buffers.get(&buffer_id) {
        Some(b) => b,
        None => return Ok(()),
    };

    let scroll_line = window.viewport.scroll_line;
    let scroll_col = window.viewport.scroll_col as usize;

    // Convert abs_line to screen row
    let screen_row = abs_line.saturating_sub(scroll_line);

    // Account for soft-wrap: count visual rows of lines above
    let content_width = {
        let gutter_w = if editor.config.line_numbers { 5u16 } else { 0 };
        let mark_gutter_w = {
            if editor.search.marks.iter().any(|(_, (bid, _))| *bid == buffer_id) {
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
        (window
            .width
            .saturating_sub(gutter_w + mark_gutter_w + git_gutter_w)) as usize
    };

    let visual_screen_row = if editor.config.word_wrap {
        let mut vrow = 0usize;
        for line_i in scroll_line..abs_line {
            if let Some(txt) = buffer.line_text(line_i) {
                vrow += softwrap_rows(&txt, content_width);
            }
        }
        // For the target line itself, we only need the first visual row
        // (the one at the start of the line — the cursor's wrap row would
        // need softwrap_row_offset but for single-line update we redraw
        // ALL visual rows of this logical line)
        vrow
    } else {
        screen_row
    };

    let is_active = true;
    let cursor_line = window.cursor.position.line;
    let cursor_col = window.cursor.position.col;

    let gutter_width = if editor.config.line_numbers { 5u16 } else { 0 };
    let git_gutter_width = if editor.git.gutter_enabled && editor.config.enable_git {
        1u16
    } else {
        0u16
    };

    let mark_at_line: std::collections::HashMap<usize, char> = editor.search.marks
        .iter()
        .filter(|(_, (bid, _))| *bid == buffer_id)
        .map(|(ch, (_, pos))| (pos.line, *ch))
        .collect();
    let mark_gutter_width = if !mark_at_line.is_empty() { 1u16 } else { 0u16 };

    if abs_line >= buffer.line_count() {
        return Ok(());
    }

    let line_text = buffer.line_text(abs_line).unwrap_or_default();
    let line_text = line_text.trim_end_matches('\n');

    let wrap_rows = if editor.config.word_wrap {
        softwrap_rows(line_text, content_width)
    } else {
        1
    };

    let graphemes: Vec<_> = line_text.graphemes(true).collect();
    let line_spans = highlighter.highlight_line(line_text, buffer.language);

    let line_scroll_offset_w: usize = if scroll_col > 0 && !editor.config.word_wrap {
        graphemes[..scroll_col.min(graphemes.len())]
            .iter()
            .map(|g| UnicodeWidthStr::width(*g))
            .sum()
    } else {
        0
    };

    // ── Check if the completion popup covers any of these rows ──
    let completion_rect = editor.popup.overlay.completion;

    for wrap_row in 0..wrap_rows {
        let row_screen = visual_screen_row + wrap_row;
        let y = window.y_offset + row_screen as u16;

        // Bounds check
        if row_screen >= window.height as usize {
            break;
        }

        // ── Skip rows covered by the completion popup ──
        if let Some(rect) = completion_rect {
            let popup_top = rect.y as usize;
            let popup_bottom = (rect.y + rect.h) as usize;
            let row_abs = row_screen;
            if row_abs >= popup_top && row_abs < popup_bottom {
                continue; // Popup covers this row — don't waste time rendering
            }
        }

        let is_cursor_line = is_active && abs_line == cursor_line;

        execute!(stdout, MoveTo(window.x_offset, y))?;

        if is_cursor_line {
            execute!(stdout, SetBackgroundColor(Color::DarkGrey))?;
        }

        // Line number gutter
        if editor.config.line_numbers && wrap_row == 0 {
            let line_num_abs = abs_line + 1;
            let display_num = if editor.config.relative_line_numbers && is_active {
                if abs_line == cursor_line {
                    line_num_abs
                } else {
                    (cursor_line as isize - abs_line as isize).unsigned_abs()
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
            if let Some(mark_char) = mark_at_line.get(&abs_line) {
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
            let sign = buffer.git_gutter.sign_at(abs_line);
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

        // Calculate display text (same logic as render_window)

        // In render_window, inside the `for wrap_row` loop,
        // replace the non-word-wrap branch of the display computation:

        let mut wrap_start_grapheme = 0usize;
        let display = if editor.config.word_wrap {
            // ── unchanged ──
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
            // ── FIX: set wrap_start_grapheme for non-wrap mode ──
            wrap_start_grapheme = scroll_col;

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

        // Render highlighted text
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

        // ── Search highlight overlay ─────────────────────────
        if !editor.search.matches.is_empty()
            && !editor.search.matches_dirty
            && editor.search.buffer_id == Some(buffer_id)
        {
            let pattern_len = editor.search.prompt.buffer.chars().count();
            if pattern_len > 0 {
                for (m_idx, m_pos) in editor.search.matches.iter().enumerate() {
                    if m_pos.line != abs_line {
                        continue;
                    }

                    let vis_left = m_pos.col;
                    let vis_right = (m_pos.col + pattern_len).min(graphemes.len());
                    if vis_left >= vis_right {
                        continue;
                    }

                    let clip_left = vis_left.max(wrap_start_grapheme);
                    let wrap_end_grapheme = wrap_start_grapheme + display.graphemes(true).count();
                    let clip_right = vis_right.min(wrap_end_grapheme);
                    if clip_left >= clip_right {
                        continue;
                    }

                    let hl_text: String = graphemes[clip_left..clip_right].join("");
                    let hl_display_w = UnicodeWidthStr::width(hl_text.as_str());
                    let content_start_x =
                        window.x_offset + gutter_width + mark_gutter_width + git_gutter_width;

                    let mut hl_start_screen_col = 0usize;
                    for gi in 0..clip_left {
                        hl_start_screen_col += UnicodeWidthStr::width(graphemes[gi]);
                    }
                    if !editor.config.word_wrap {
                        hl_start_screen_col =
                            hl_start_screen_col.saturating_sub(line_scroll_offset_w);
                    }

                    let hl_start_x = content_start_x as usize + hl_start_screen_col;

                    if hl_start_x >= content_start_x as usize + content_width
                        || hl_display_w == 0
                        || hl_start_x < content_start_x as usize
                    {
                        continue;
                    }

                    let max_hl_w = content_start_x as usize + content_width - hl_start_x;
                    let actual_hl_text = if hl_display_w > max_hl_w {
                        let mut truncated = String::new();
                        let mut w = 0usize;
                        for g in graphemes[clip_left..clip_right].iter() {
                            let gw = UnicodeWidthStr::width(*g);
                            if w + gw > max_hl_w {
                                break;
                            }
                            truncated.push_str(g);
                            w += gw;
                        }
                        truncated
                    } else {
                        hl_text
                    };

                    if !actual_hl_text.is_empty() {
                        let (hl_bg, hl_fg) = if m_idx == editor.search.current_match {
                            (
                                Color::Rgb {
                                    r: 249,
                                    g: 226,
                                    b: 175,
                                },
                                Color::Rgb {
                                    r: 30,
                                    g: 30,
                                    b: 46,
                                },
                            )
                        } else {
                            (
                                Color::Rgb {
                                    r: 55,
                                    g: 50,
                                    b: 35,
                                },
                                Color::Rgb {
                                    r: 249,
                                    g: 226,
                                    b: 175,
                                },
                            )
                        };
                        execute!(
                            stdout,
                            MoveTo(hl_start_x as u16, y),
                            SetBackgroundColor(hl_bg),
                            SetForegroundColor(hl_fg),
                            Print(&actual_hl_text),
                            ResetColor
                        )?;
                    }
                }
            }
        }
    } // end wrap_row loop

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

    // ── Hide cursor for the entire frame ──
    let _ = execute!(stdout, crossterm::cursor::Hide);

    let _edit_height = term_height.saturating_sub(3);

    // ── 1. Restore editor content if a popup was closed ──
    if let Some(rect) = editor.dirty.restore_rect {
        restore_region(editor, stdout, rect, highlighter)?;
        if editor.windows.len() > 1 {
            render_separators(editor, stdout, term_width, term_height)?;
        }
    }

    // ── 2. Cursor style ──
    if editor.dirty.full || editor.dirty.cursor || editor.dirty.windows {
        set_cursor_style(editor, stdout)?;
    }

    // ══════════════════════════════════════════════════════════════════
    //  FAST PATH: Single-line update (completion popup is active)
    //
    //  When the completion popup is showing and the user types a character,
    //  only the current line needs redrawing. Redrawing the entire window
    //  would clear the popup's screen area, causing a visible flash.
    // ══════════════════════════════════════════════════════════════════
    if let Some(line) = editor.dirty.single_line {
        render_single_buffer_line(editor, stdout, line, highlighter)?;

        // Always re-render the completion popup on top
        if editor.completion.active && !editor.completion.items.is_empty() {
            crate::popup::completion_popup::render_completion_popup(
                editor,
                stdout,
                term_width,
                term_height,
            )?;
        }

        // Cursor must be repositioned
        position_cursor(editor, stdout, term_width, term_height)?;

        editor.dirty.clear();
        return Ok(());
    }

    // ══════════════════════════════════════════════════════════════════
    //  NORMAL PATH: Full or partial redraw
    // ══════════════════════════════════════════════════════════════════

    // ── 3. Editor windows ──
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
        if let Some(popup) = &editor.popup.float {
            render_float_popup(editor, stdout, popup, term_width, term_height)?;
        }
    }

    // Register popup (bottom-up)
    if must_draw_all_popups || editor.popup.register.is_some() {
        if let Some(ref lines) = editor.popup.register {
            crate::popup::register::render_register_popup(
                &editor.popup.register_title,
                lines,
                stdout,
                term_width,
                term_height,
            )?;
        }
    }

    // Mark list popup (bottom-up)
    if must_draw_all_popups || editor.dirty.mark_list {
        if let Some(popup) = &editor.popup.mark_list {
            crate::popup::render_mark_list_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // Format info popup
    if must_draw_all_popups || editor.popup.fmt_info.is_some() {
        render_fmtinfo(editor, stdout, term_width, term_height)?;
    }

    // Keymap popup
    if must_draw_all_popups || editor.dirty.help {
        if let Some(popup) = &editor.popup.keymap {
            crate::popup::render_keymap_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // Diff popup
    if must_draw_all_popups || editor.dirty.diff {
        if let Some(popup) = &editor.git.diff_popup {
            crate::popup::diff_popup::render_diff_popup(
                editor,
                stdout,
                popup,
                term_width,
                term_height,
            )?;
        }
    }

    // Completion popup (special: high-frequency, never use clear_rect)
    if (must_draw_all_popups || editor.dirty.completion)
        && editor.completion.active
        && !editor.completion.items.is_empty()
    {
        crate::popup::completion_popup::render_completion_popup(
            editor,
            stdout,
            term_width,
            term_height,
        )?;
    }

    // Help popup
    if must_draw_all_popups || editor.dirty.help {
        if let Some(popup) = &editor.popup.help {
            crate::popup::render_help_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // File picker
    if must_draw_all_popups || editor.dirty.file_picker {
        if let Some(picker) = &editor.popup.file_picker {
            crate::popup::render_file_picker(picker, stdout, term_width, term_height)?;
        }
    }

    // Buffer list
    if must_draw_all_popups || editor.dirty.buffer_list {
        if let Some(popup) = &editor.popup.buffer_list {
            crate::popup::render_buffer_list_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // MRU popup
    if must_draw_all_popups || editor.dirty.mru {
        if let Some(popup) = &editor.popup.mru {
            crate::popup::render_mru_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // Tag list popup
    if must_draw_all_popups || editor.popup.tag_list.is_some() {
        if let Some(popup) = &editor.popup.tag_list {
            crate::popup::render_tag_list_popup(popup, stdout, term_width, term_height)?;
        }
    }

    // Guide popup
    if must_draw_all_popups || editor.dirty.guide {
        if let Some(popup) = &editor.popup.guide {
            crate::popup::guide_popup::render_guide_popup(
                editor,
                stdout,
                popup,
                term_width,
                term_height,
            )?;
        }
    }

    // Function list popup
    if must_draw_all_popups || editor.dirty.function_list {
        if let Some(_popup) = &editor.popup.function_list {
            crate::popup::function_list::render_function_list_popup(
                editor,
                stdout,
                term_width,
                term_height,
            )?;
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

// -----------------------------------------------------------------------------
// Status line rendering (split into 3 semantic regions)
// -----------------------------------------------------------------------------
#[rustfmt::skip]
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

#[rustfmt::skip]
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
    } else if editor.search.input_active {
        let prefix = match editor.search.direction {
            Some(SearchDirection::Forward) => "/",
            Some(SearchDirection::Backward) => "?",
            None => "/",
        };
        execute!(stdout, SetForegroundColor(yellow))?;
        execute!(stdout, Print(prefix))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(safe!(&editor.search.prompt.buffer)))?;

        let printed = 1 + UnicodeWidthStr::width(editor.search.prompt.buffer.as_str());

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
        let preset_label = editor.llm.active_preset
            .map(|p| format!(" [{}]", p))
            .unwrap_or_default();
        let prompt_text = format!(">{}", preset_label);
        execute!(stdout, SetForegroundColor(blue))?;
        execute!(stdout, Print(&prompt_text))?;
        execute!(stdout, SetForegroundColor(text))?;
        execute!(stdout, Print(&editor.llm.prompt.buffer))?;

        let printed = UnicodeWidthStr::width(prompt_text.as_str())
            + UnicodeWidthStr::width(editor.llm.prompt.buffer.as_str());
        let remaining = max_width.saturating_sub(printed);
        if remaining > 0 {
            execute!(stdout, Print(&" ".repeat(remaining)))?;
        }
    } else if editor.lsp.formatting_pending {
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
    let lines = match &editor.popup.fmt_info {
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

    let title = format!(" {} ", editor.popup.fmt_info_title);
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
    } else if let Some(ref sig) = editor.lsp.signature_help {
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

/// Compute the set of grapheme-column indices where indent guides should
/// appear for a given line, taking soft-wrap offset into account.
/// Compute the set of grapheme-column indices where indent guides should
/// appear for a given line, taking soft-wrap offset into account.
fn guide_cols_for_line(
    editor: &Editor,
    buffer: &crate::buffer::Buffer,
    line_idx: usize,
    wrap_start_grapheme: usize,
    scroll_col: usize,
) -> std::collections::HashSet<usize> {
    let tab_width = editor.config.tab_width.max(1) as usize;
    let line_text = buffer.line_text(line_idx).unwrap_or_default();
    let line_text = line_text.trim_end_matches('\n');

    let curr_depth = indent_depth(line_text, tab_width);

    // Find the depth of the nearest non-empty line above (skip whitespace-only lines)
    let mut prev_real_depth = 0;
    let scan_limit = 100;
    for i in (line_idx.saturating_sub(scan_limit)..line_idx).rev() {
        if let Some(t) = buffer.line_text(i) {
            let t = t.trim_end_matches('\n');
            if !t.trim().is_empty() {
                prev_real_depth = indent_depth(t, tab_width);
                break;
            }
        }
    }

    // Find the depth of the nearest non-empty line below (skip whitespace-only lines)
    let mut next_real_depth = 0;
    for i in (line_idx + 1)..(line_idx + scan_limit + 1).min(buffer.line_count()) {
        if let Some(t) = buffer.line_text(i) {
            let t = t.trim_end_matches('\n');
            if !t.trim().is_empty() {
                next_real_depth = indent_depth(t, tab_width);
                break;
            }
        }
    }

    let max_depth = curr_depth.max(prev_real_depth).max(next_real_depth);

    (1..=max_depth)
        .filter(|&d| {
            // Draw the guide if the current line has this depth, OR if it's
            // sandwiched between two non-empty lines that both have this depth.
            curr_depth >= d || (curr_depth < d && prev_real_depth >= d && next_real_depth >= d)
        })
        .filter_map(|d| {
            let abs_col = (d - 1) * tab_width; // first space of the indent band
                                               // translate to display-grapheme index within the current wrap slice
            if editor.config.word_wrap {
                if abs_col >= wrap_start_grapheme {
                    Some(abs_col - wrap_start_grapheme)
                } else {
                    None
                }
            } else {
                if abs_col >= scroll_col {
                    Some(abs_col - scroll_col)
                } else {
                    None
                }
            }
        })
        .collect()
}
/// Calculate the indentation depth of a line.
fn indent_depth(line_text: &str, tab_width: usize) -> usize {
    let mut depth = 0;
    for c in line_text.chars() {
        match c {
            ' ' => depth += 1,
            '\t' => depth += tab_width,
            _ => break,
        }
    }
    depth / tab_width.max(1)
}
