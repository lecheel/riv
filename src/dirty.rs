// src/dirty.rs
//! Dirty-state tracking for incremental rendering.

/// Screen region for overlay cleanup.
#[derive(Default, Clone, Copy, Debug)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Tracks which parts of the UI need redrawing.
#[derive(Default)]
pub struct DirtyState {
    /// Force full redraw (first render, resize, theme change).
    pub full: bool,

    // ── Content layers ──
    pub windows: bool,
    pub status: bool,

    // Split status into 3 sub-regions
    pub status_powerline: bool, // Line 1: Mode, filename, position
    pub status_cmdline: bool,   // Line 2: Search, command input, messages
    pub status_infobar: bool,   // Line 3: Which-key hints, signature help

    // ── Popups (each tracked independently) ──
    pub completion: bool,
    pub help: bool,
    pub file_picker: bool,
    pub buffer_list: bool,
    pub float: bool,
    pub diff: bool,
    pub mru: bool,
    pub mark_list: bool,
    pub guide: bool,

    // ── Cursor ──
    pub cursor: bool,

    // ── Single-line update ──
    /// If set, only this specific buffer line (0-based) needs re-rendering.
    /// Used when the completion popup is active to avoid redrawing the
    /// entire buffer (which causes flicker under the popup).
    /// Takes priority over `windows` for the rendering pass.
    pub single_line: Option<usize>,

    // ── Overlay cleanup ──
    /// Previously occupied popup region that needs editor content restored.
    pub restore_rect: Option<Rect>,
    pub function_list: bool,
}

impl DirtyState {
    pub fn clear(&mut self) {
        self.full = false;
        self.windows = false;
        self.status = false;
        self.cursor = false;

        self.status_powerline = false;
        self.status_cmdline = false;
        self.status_infobar = false;
        self.float = false;
        self.diff = false;
        self.completion = false;
        self.help = false;
        self.file_picker = false;
        self.buffer_list = false;
        self.mru = false;
        self.mark_list = false;
        self.restore_rect = None;
        self.function_list = false;
        self.single_line = None;
    }

    pub fn mark_all(&mut self) {
        self.full = true;
        self.windows = true;
        self.status = true;
        self.cursor = true;
        self.status_powerline = true;
        self.status_cmdline = true;
        self.status_infobar = true;
        self.float = true;
        self.diff = true;
        self.completion = true;
        self.help = true;
        self.file_picker = true;
        self.buffer_list = true;
        self.mru = true;
        self.mark_list = true;
        self.function_list = true;
        // Full redraw supersedes single-line optimization
        self.single_line = None;
    }

    /// Typical insert-mode keystroke: buffer changed, cursor moved.
    pub fn mark_insert(&mut self) {
        self.windows = true;
        self.status = true;
        self.cursor = true;
        // Standard insert supersedes single-line
        self.single_line = None;
    }

    /// Only the completion selection scrolled (Ctrl-N/P).
    pub fn mark_completion_scroll(&mut self) {
        self.completion = true;
        self.cursor = true;
    }

    /// Completion content changed (new trigger / filtered list).
    pub fn mark_completion_update(&mut self) {
        self.completion = true;
    }

    /// Mark only the current line as dirty (used when popup is active).
    /// Prevents full window redraw which would flash under the popup.
    pub fn mark_single_line(&mut self, line: usize) {
        self.single_line = Some(line);
        self.cursor = true;
        // Do NOT set self.windows = true — that causes full redraw
    }

    /// Clear the single-line flag (e.g. after rendering it).
    pub fn clear_single_line(&mut self) {
        self.single_line = None;
    }

    /// A popup was dismissed — restore the underlying editor content.
    pub fn mark_popup_closed(&mut self, rect: Rect) {
        self.restore_rect = Some(match self.restore_rect {
            Some(existing) => union_rect(existing, rect),
            None => rect,
        });
        self.cursor = true;
    }

    pub fn is_any_dirty(&self) -> bool {
        self.full
            || self.windows
            || self.status
            || self.completion
            || self.help
            || self.file_picker
            || self.buffer_list
            || self.mru
            || self.float
            || self.diff
            || self.cursor
            || self.restore_rect.is_some()
            || self.single_line.is_some()
    }
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    if a.w == 0 || a.h == 0 {
        return b;
    }
    if b.w == 0 || b.h == 0 {
        return a;
    }
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let r = (a.x + a.w).max(b.x + b.w);
    let bot = (a.y + a.h).max(b.y + b.h);
    Rect {
        x,
        y,
        w: r.saturating_sub(x),
        h: bot.saturating_sub(y),
    }
}
