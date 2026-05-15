//! Powerline status bar rendering using ratatui.
//!
//! Provides Catppuccin Mocha-themed powerline status, compact powerline,
//! and tab line rendering. The ratatui-based renderers are available for
//! future use when the full render loop migrates to ratatui. For now, the
//! main render loop in main.rs uses crossterm directly with the same
//! visual design, pulling glyph constants and color helpers from this module.

use crate::buffer::{Buffer, BufferKind};
use crate::editor::{Editor, Mode};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// Simple Arrow Glyph Constants
// ---------------------------------------------------------------------------
pub mod glyphs {
    pub const SEPARATOR_LEFT: &str = "\u{e0b0}";
    pub const SEPARATOR_RIGHT: &str = "\u{e0b2}";
    pub const SEPARATOR_THIN: &str = "\u{e0b1}";
    pub const GIT_BRANCH: &str = "\u{2694}";
    pub const GIT_MODIFIED: &str = "~";
    pub const GIT_ADDED: &str = "+";
    pub const GIT_REMOVED: &str = "-";
    pub const GIT_CONFLICT: &str = "!";
    pub const MODE_NORMAL: &str = "N";
    pub const MODE_INSERT: &str = "I";
    pub const MODE_COMMAND: &str = ":";
    pub const MODE_VISUAL: &str = "V";
    pub const MODE_BLOCK: &str = "B";
    pub const DIRTY: &str = "\u{25cf}";
    pub const CLEAN: &str = "\u{25cb}";
    pub const LOCATION: &str = "\u{2316}";
    pub const BUFFER_ICON: &str = "\u{2261}";
    pub const ENCODING: &str = "\u{2261}";
}

// ---------------------------------------------------------------------------
// Color Schemes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct ModeColors {
    pub bg: Color,
    pub fg: Color,
    pub separator: Color,
}

#[rustfmt::skip]
pub fn get_mode_colors(mode: Mode) -> ModeColors {
    match mode {
        Mode::Normal => ModeColors {
            bg: Color::Rgb(68, 71, 90),
            fg: Color::Rgb(205, 214, 244),
            separator: Color::Rgb(68, 71, 90),
        },
        Mode::Insert => ModeColors {
            bg: Color::Rgb(166, 227, 161),
            fg: Color::Rgb(30, 30, 46),
            separator: Color::Rgb(166, 227, 161),
        },
        Mode::Command => ModeColors {
            bg: Color::Rgb(137, 180, 250),
            fg: Color::Rgb(30, 30, 46),
            separator: Color::Rgb(137, 180, 250),
        },
        Mode::Visual | Mode::VisualLine => ModeColors {
            bg: Color::Rgb(203, 166, 247),
            fg: Color::Rgb(30, 30, 46),
            separator: Color::Rgb(203, 166, 247),
        },
        Mode::VisualBlock => ModeColors {
            bg: Color::Rgb(250, 179, 135),
            fg: Color::Rgb(30, 30, 46),
            separator: Color::Rgb(250, 179, 135),
        },
        Mode::Replace => ModeColors {
            bg: Color::Rgb(243, 139, 168),
            fg: Color::Rgb(30, 30, 46),
            separator: Color::Rgb(243, 139, 168),
        },
        Mode::OperatorPending => ModeColors {
            bg: Color::Rgb(250, 179, 135),
            fg: Color::Rgb(30, 30, 46),
            separator: Color::Rgb(250, 179, 135),
        },
        // ── LLM Prompt mode ─────────────────────────────
        Mode::LlmPrompt => ModeColors {
            bg: Color::Rgb(137, 180, 250), 
            fg: Color::Rgb(30, 30, 46),
            separator: Color::Rgb(137, 180, 250),
        },        
    }
}

// Catppuccin Mocha inspired colors (ratatui::style::Color)
const BASE: Color = Color::Rgb(30, 30, 46);
const SURFACE0: Color = Color::Rgb(49, 50, 68);
const SURFACE1: Color = Color::Rgb(69, 71, 90);
const SURFACE2: Color = Color::Rgb(88, 91, 112);
const OVERLAY0: Color = Color::Rgb(108, 112, 134);
const TEXT: Color = Color::Rgb(205, 214, 244);
const SUBTEXT: Color = Color::Rgb(166, 173, 200);
const PEACH: Color = Color::Rgb(250, 179, 135);
const GREEN: Color = Color::Rgb(166, 227, 161);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const RED: Color = Color::Rgb(243, 139, 168);

// ---------------------------------------------------------------------------
// Catppuccin Mocha colors exported as crossterm::style::Color
// (for use in the crossterm-based renderer in main.rs)
// ---------------------------------------------------------------------------
#[rustfmt::skip]
pub mod crossterm_colors {
    use crossterm::style::Color;
    pub const BASE: Color = Color::Rgb { r: 30, g: 30, b: 46 };
    pub const SURFACE0: Color = Color::Rgb { r: 49, g: 50, b: 68 };
    pub const SURFACE1: Color = Color::Rgb { r: 69, g: 71, b: 90 };
    pub const SURFACE2: Color = Color::Rgb { r: 88, g: 91, b: 112 };
    pub const OVERLAY0: Color = Color::Rgb { r: 108, g: 112, b: 134 };
    pub const TEXT: Color = Color::Rgb { r: 205, g: 214, b: 244 };
    pub const SUBTEXT: Color = Color::Rgb { r: 166, g: 173, b: 200 };
    pub const PEACH: Color = Color::Rgb { r: 250, g: 179, b: 135 };
    pub const GREEN: Color = Color::Rgb { r: 166, g: 227, b: 161 };
    pub const YELLOW: Color = Color::Rgb { r: 249, g: 226, b: 175 };
    pub const RED: Color = Color::Rgb { r: 243, g: 139, b: 168 };
    pub const BLUE: Color = Color::Rgb { r: 137, g: 180, b: 250 };
}

// ---------------------------------------------------------------------------
// Mode color helpers — returns crossterm-compatible (fg, bg) tuples
// ---------------------------------------------------------------------------

pub fn get_effective_mode_colors(editor: &Editor) -> ModeColors {
    if editor.is_brief_mode() {
        if editor.pending.brief_ctrl_o {
            ModeColors {
                bg: Color::Rgb(87, 82, 110),
                fg: Color::Rgb(205, 214, 244),
                separator: Color::Rgb(87, 82, 110),
            }
        } else {
            ModeColors {
                bg: Color::Rgb(137, 220, 235),
                fg: Color::Rgb(30, 30, 46),
                separator: Color::Rgb(137, 220, 235),
            }
        }
    } else {
        get_mode_colors(editor.mode)
    }
}

/// Return mode colors as (fg, bg) crossterm Color pair.
pub fn get_mode_colors_crossterm(
    editor: &Editor,
) -> (crossterm::style::Color, crossterm::style::Color) {
    let mc = get_effective_mode_colors(editor);
    let to_crossterm = |c: ratatui::style::Color| -> crossterm::style::Color {
        match c {
            ratatui::style::Color::Rgb(r, g, b) => crossterm::style::Color::Rgb { r, g, b },
            _ => crossterm::style::Color::Rgb { r: 0, g: 0, b: 0 },
        }
    };
    (to_crossterm(mc.fg), to_crossterm(mc.bg))
}

/// Return mode separator color as crossterm Color.
pub fn get_mode_sep_color_crossterm(editor: &Editor) -> crossterm::style::Color {
    let mc = get_effective_mode_colors(editor);
    match mc.separator {
        ratatui::style::Color::Rgb(r, g, b) => crossterm::style::Color::Rgb { r, g, b },
        _ => crossterm::style::Color::Rgb { r: 0, g: 0, b: 0 },
    }
}

// ---------------------------------------------------------------------------
// Git Status Detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub modified: usize,
    pub added: usize,
    pub deleted: usize,
    pub conflicted: usize,
    pub staged: usize,
    pub ahead: usize,
    pub behind: usize,
    pub is_repo: bool,
}

impl GitStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_buffer(buffer: &Buffer) -> Self {
        if let Some(ref path) = buffer.file_path {
            if let Some(parent) = path.parent() {
                return Self::detect(parent);
            }
        }
        Self::new()
    }

    pub fn detect(dir: &Path) -> Self {
        let mut status = Self::new();
        status.is_repo = dir.join(".git").exists();

        if !status.is_repo {
            if let Some(git_dir) = find_git_dir(dir) {
                status.is_repo = true;
                status.branch = get_git_branch(git_dir.parent().unwrap_or(dir));
                status = get_git_status_from_dir(git_dir.parent().unwrap_or(dir));
                return status;
            }
            return status;
        }

        status.branch = get_git_branch(dir);
        status = get_git_status_from_dir(dir);
        status
    }

    pub fn has_changes(&self) -> bool {
        self.modified > 0 || self.added > 0 || self.deleted > 0 || self.conflicted > 0
    }

    #[rustfmt::skip]
    pub fn status_string(&self) -> String {
        let mut parts = Vec::new();
        if self.modified > 0 { parts.push(format!("~{}", self.modified)); }
        if self.added > 0 || self.staged > 0 { parts.push(format!("+{}", self.added + self.staged)); }
        if self.deleted > 0 { parts.push(format!("-{}", self.deleted)); }
        if self.conflicted > 0 { parts.push(format!("!{}", self.conflicted)); }
        parts.join(" ")
    }

    #[rustfmt::skip]
    pub fn ahead_behind_string(&self) -> Option<String> {
        if self.ahead == 0 && self.behind == 0 { return None; }
        let mut s = String::new();
        if self.ahead > 0 { s.push_str(&format!("\u{2191}{}", self.ahead)); }
        if self.behind > 0 { s.push_str(&format!("\u{2193}{}", self.behind)); }
        Some(s)
    }
}

fn find_git_dir(start_dir: &Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current.join(".git"));
        }
        if !current.pop() {
            return None;
        }
    }
}

fn get_git_branch(dir: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if branch.is_empty() || branch == "HEAD" {
                    Command::new("git")
                        .args(["rev-parse", "--short", "HEAD"])
                        .current_dir(dir)
                        .output()
                        .ok()
                        .and_then(|o| {
                            if o.status.success() {
                                let hash = String::from_utf8_lossy(&o.stdout).trim().to_string();
                                Some(format!("@{}", hash))
                            } else {
                                None
                            }
                        })
                } else {
                    Some(branch)
                }
            } else {
                None
            }
        })
}

fn get_git_status_from_dir(dir: &Path) -> GitStatus {
    let mut status = GitStatus::new();
    status.is_repo = true;

    if let Ok(output) = Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(dir)
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.len() < 2 {
                    continue;
                }
                let chars: Vec<char> = line.chars().collect();
                let index = chars[0];
                let worktree = chars[1];
                if index == 'U' || worktree == 'U' || (index == 'A' && worktree == 'A') {
                    status.conflicted += 1;
                } else {
                    match index {
                        'M' | 'A' | 'R' | 'C' => status.staged += 1,
                        'D' => status.staged += 1,
                        _ => {}
                    }
                    match worktree {
                        'M' => status.modified += 1,
                        'A' | '?' => status.added += 1,
                        'D' => status.deleted += 1,
                        _ => {}
                    }
                }
            }
        }
    }

    if let Ok(output) = Command::new("git")
        .args(["rev-list", "--count", "--left-right", "@{upstream}...HEAD"])
        .current_dir(dir)
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some((behind_str, ahead_str)) = stdout.split_once('\t') {
                status.behind = behind_str.parse().unwrap_or(0);
                status.ahead = ahead_str.trim_end().parse().unwrap_or(0);
            }
        }
    }
    status
}

// ---------------------------------------------------------------------------
// Powerline Renderer (full version — ratatui)
// ---------------------------------------------------------------------------
#[rustfmt::skip]
pub fn draw_powerline_status(f: &mut Frame, editor: &Editor, area: Rect) {
    let buf = editor.current_buffer();
    let mode_colors = get_effective_mode_colors(editor);
    let mode_text = editor.mode_display();

    if let Some(buf) = buf {
        let filename = buf.display_name();
        let is_dirty = buf.is_dirty();
        let window = editor.windows.active_window();
        let line = window.map(|w| w.cursor.position.line + 1).unwrap_or(1);
        let col = window.map(|w| w.cursor.position.col + 1).unwrap_or(1);
        let total_lines = buf.line_count();

        let _git_status = GitStatus::from_buffer(buf);

        let mut left_spans: Vec<Span> = Vec::new();
        let mut right_spans: Vec<Span> = Vec::new();

        // === LEFT SIDE ===
        // Segment 1: Mode
        left_spans.push(Span::styled(
            format!(" {} ", mode_text),
            Style::default()
                .fg(mode_colors.fg)
                .bg(mode_colors.bg)
                .add_modifier(Modifier::BOLD),
        ));

        // Separator
        left_spans.push(Span::styled(
            glyphs::SEPARATOR_LEFT,
            Style::default().fg(mode_colors.separator).bg(SURFACE0),
        ));

        // Segment 2: Filename + dirty indicator
        left_spans.push(Span::styled(
            format!(" {} ", filename),
            Style::default().fg(TEXT).bg(SURFACE0),
        ));
        if is_dirty {
            left_spans.push(Span::styled(
                format!(" {} ", glyphs::DIRTY),
                Style::default().fg(PEACH).bg(SURFACE0),
            ));
        } else {
            left_spans.push(Span::styled(
                format!(" {} ", glyphs::CLEAN),
                Style::default().fg(GREEN).bg(SURFACE0),
            ));
        }

        // Separator
        left_spans.push(Span::styled(
            glyphs::SEPARATOR_LEFT,
            Style::default().fg(SURFACE0).bg(SURFACE1),
        ));

        // Segment 3: Position
        left_spans.push(Span::styled(
            format!(" {}:{} ", line, col),
            Style::default().fg(SUBTEXT).bg(SURFACE1),
        ));

        // Separator
        left_spans.push(Span::styled(
            glyphs::SEPARATOR_LEFT,
            Style::default().fg(SURFACE1).bg(SURFACE2),
        ));

        // Segment 4: Total lines percentage
        let pct = if total_lines > 0 {
            ((line as f64 / total_lines as f64) * 100.0) as usize
        } else {
            100
        };
        left_spans.push(Span::styled(
            format!(" {}% ", pct),
            Style::default().fg(OVERLAY0).bg(SURFACE2),
        ));

        // === RIGHT SIDE ===
        // Segment 1: Buffer position
        let buf_text = format!(
            " {} {} / {} ",
            glyphs::BUFFER_ICON,
            editor.active_buffer_idx + 1,
            editor.buffers.len()
        );
        right_spans.insert(0, Span::styled(buf_text, Style::default().fg(SUBTEXT).bg(SURFACE1)));
        right_spans.insert(0, Span::styled(
            glyphs::SEPARATOR_RIGHT,
            Style::default().fg(SURFACE1).bg(SURFACE0),
        ));

        // Segment 2: Encoding & filetype
        let ft_text = buf.language.map(|l| l.as_str().to_string()).unwrap_or_else(|| "text".to_string());
        right_spans.insert(0, Span::styled(
            format!(" UTF-8 {} ", ft_text),
            Style::default().fg(SUBTEXT).bg(SURFACE0),
        ));
        right_spans.insert(0, Span::styled(
            glyphs::SEPARATOR_RIGHT,
            Style::default().fg(SURFACE0).bg(SURFACE0),
        ));

        // Calculate padding between left and right
        let left_width: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
        let right_width: usize = right_spans.iter().map(|s| s.content.chars().count()).sum();
        let area_width = area.width as usize;
        let padding = area_width.saturating_sub(left_width + right_width);

        left_spans.push(Span::styled(
            " ".repeat(padding),
            Style::default().bg(SURFACE2),
        ));

        let mut all_spans = left_spans;
        all_spans.extend(right_spans);

        f.render_widget(
            Paragraph::new(Line::from(all_spans)).style(Style::default().bg(BASE)),
            area,
        );
    } else {
        // No buffer: just show mode
        let spans = vec![
            Span::styled(
                format!(" {} ", mode_text),
                Style::default()
                    .fg(mode_colors.fg)
                    .bg(mode_colors.bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ".repeat(area.width as usize),
                Style::default().bg(SURFACE0),
            ),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

// ---------------------------------------------------------------------------
// Compact Powerline (ratatui)
// ---------------------------------------------------------------------------
#[rustfmt::skip]
pub fn draw_powerline_status_compact(
    f: &mut Frame,
    editor: &Editor,
    area: Rect,
) {
    let buf = editor.current_buffer();
    let mode_colors = get_effective_mode_colors(editor);
    let mode_text = editor.mode_display();

    let mut spans: Vec<Span> = Vec::new();

    spans.push(Span::styled(
        format!(" {} ", mode_text),
        Style::default()
            .fg(mode_colors.fg)
            .bg(mode_colors.bg)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        glyphs::SEPARATOR_LEFT,
        Style::default().fg(mode_colors.separator).bg(SURFACE0),
    ));

    if let Some(buf) = buf {
        let filename = buf.display_name();
        spans.push(Span::styled(filename, Style::default().fg(TEXT).bg(SURFACE0)));
        if buf.is_dirty() {
            spans.push(Span::styled(" +", Style::default().fg(PEACH).bg(SURFACE0)));
        }
    }

    spans.push(Span::styled(
        glyphs::SEPARATOR_LEFT,
        Style::default().fg(SURFACE0).bg(SURFACE1),
    ));

    let window = editor.windows.active_window();
    let line = window.map(|w| w.cursor.position.line + 1).unwrap_or(1);
    let col = window.map(|w| w.cursor.position.col + 1).unwrap_or(1);
    spans.push(Span::styled(
        format!("{}:{}", line, col),
        Style::default().fg(SUBTEXT).bg(SURFACE1),
    ));

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BASE)),
        area,
    );
}

// ---------------------------------------------------------------------------
// Tab Line (ratatui)
// ---------------------------------------------------------------------------

#[rustfmt::skip]
pub fn draw_tab_line(f: &mut Frame, editor: &Editor, area: Rect) {
    let active_buffer_id = editor
        .windows
        .active_window()
        .map(|w| w.buffer_id);

    let mut spans: Vec<Span> = Vec::new();

    for (i, buf) in editor.buffers.iter().enumerate() {
        let is_active = active_buffer_id == Some(buf.id);

        let filename = match buf.kind {
            BufferKind::Ripgrep => "*rg*".to_string(),
            BufferKind::GitDiff => "*git-diff*".to_string(),
            BufferKind::GitStatus => "*git-st*".to_string(),
            BufferKind::GitLog => "*git-log*".to_string(),
            BufferKind::Llm => "*LLM*".to_string(),  
            BufferKind::LlmInput => "*LLM prompt*".to_string(),  
            BufferKind::Normal => buf.display_name(),
        };

        let (bg, fg) = if is_active { (SURFACE1, TEXT) } else { (BASE, OVERLAY0) };

        if i > 0 {
            spans.push(Span::styled(
                glyphs::SEPARATOR_THIN,
                Style::default().fg(OVERLAY0).bg(bg),
            ));
        }

        let dirty_marker = if buf.is_dirty() { " \u{25cf}" } else { "" };
        let tab_text = format!(" {} {} {}", glyphs::BUFFER_ICON, filename, dirty_marker);

        spans.push(Span::styled(
            tab_text,
            Style::default().fg(fg).bg(bg).add_modifier(if is_active { Modifier::BOLD } else { Modifier::empty() }),
        ));
    }

    let used_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (area.width as usize).saturating_sub(used_width);
    if remaining > 0 {
        spans.push(Span::styled(" ".repeat(remaining), Style::default().bg(BASE)));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BASE)),
        area,
    );
}
