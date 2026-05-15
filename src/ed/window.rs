// src/ed/window.rs
//! Window management: splits, navigation, and closing.

use crate::editor::Editor;
use crate::window::SplitDirection;
use crate::CommandResult;

/// Extension trait for window operations.
pub trait WindowExt {
    /// Split the active window horizontally (above/below).
    fn split_horizontal(&mut self) -> CommandResult;

    /// Split the active window vertically (left/right).
    fn split_vertical(&mut self) -> CommandResult;

    /// Move focus to the next window (cyclic).
    fn next_window(&mut self) -> CommandResult;

    /// Move focus to the previous window (cyclic).
    fn prev_window(&mut self) -> CommandResult;

    /// Close the active window. If it is the last window, returns an error.
    fn close_window(&mut self) -> CommandResult;
}

impl WindowExt for Editor {
    fn split_horizontal(&mut self) -> CommandResult {
        let min_height = 6u16; // 3 status + 3 edit (minimum for 2 windows + separator)
        if self.term_height < min_height {
            return CommandResult::Error("Terminal too small for horizontal split.".to_string());
        }
        let _ = self.windows.split_active(SplitDirection::Horizontal);
        self.windows.resize_all(self.term_width, self.term_height);
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn split_vertical(&mut self) -> CommandResult {
        let min_width = 20u16; // 2 windows + separator + gutter
        if self.term_width < min_width {
            return CommandResult::Error("Terminal too narrow for vertical split.".to_string());
        }
        let _ = self.windows.split_active(SplitDirection::Vertical);
        self.windows.resize_all(self.term_width, self.term_height);
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn next_window(&mut self) -> CommandResult {
        self.windows.next_window();
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn prev_window(&mut self) -> CommandResult {
        self.windows.prev_window();
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn close_window(&mut self) -> CommandResult {
        if let Some(window) = self.windows.active_window() {
            let window_id = window.id;
            if self.windows.len() <= 1 {
                return CommandResult::Error("Cannot close the last window.".to_string());
            }
            self.windows.close_window(window_id);
            self.windows.resize_all(self.term_width, self.term_height);
            self.dirty.mark_all();
            CommandResult::ViewChanged
        } else {
            CommandResult::NoOp
        }
    }
}
