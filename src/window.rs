//! Window management — viewports, cursor positioning, and tree-based layout.
//!
//! A window is a view into a buffer. Multiple windows can display the same buffer.
//! The WindowManager uses a binary tree (`SplitNode`) to represent nested splits,
//! allowing arbitrary layouts like vertical-then-horizontal or vice versa.

use crate::buffer::{BufferId, CursorPosition};

// ── Window ID ───────────────────────────────────────────────────────

pub type WindowId = u64;

static mut NEXT_WINDOW_ID: WindowId = 1;

fn new_window_id() -> WindowId {
    unsafe {
        let id = NEXT_WINDOW_ID;
        NEXT_WINDOW_ID += 1;
        id
    }
}

// ── Viewport ────────────────────────────────────────────────────────

/// Scroll state for a window view.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    /// Number of lines scrolled from the top.
    pub scroll_line: usize,
    /// Number of grapheme columns scrolled from the left.
    pub scroll_col: u16,
    /// Pixel-level vertical offset for smooth scrolling (reserved).
    pub scroll_offset_y: f64,
}

impl Viewport {
    pub fn new() -> Self {
        Self {
            scroll_line: 0,
            scroll_col: 0,
            scroll_offset_y: 0.0,
        }
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new()
    }
}

// ── Window cursor ───────────────────────────────────────────────────

/// Tracks the cursor position within a window, including desired column
/// for maintaining vertical motion across varying line lengths.
#[derive(Debug, Clone)]
pub struct WindowCursor {
    /// Current cursor position in buffer coordinates.
    pub position: CursorPosition,
    /// The column the cursor "wants" to be at (for vertical motion).
    pub desired_col: Option<usize>,
}

impl WindowCursor {
    pub fn new(position: CursorPosition) -> Self {
        Self {
            position,
            desired_col: None,
        }
    }
}

impl Default for WindowCursor {
    fn default() -> Self {
        Self::new(CursorPosition::zero())
    }
}

// ── Window ──────────────────────────────────────────────────────────

/// A window is a view into a buffer, with its own cursor and viewport.
#[derive(Debug)]
pub struct Window {
    pub id: WindowId,
    /// The buffer this window is displaying.
    pub buffer_id: BufferId,
    /// Current cursor state.
    pub cursor: WindowCursor,
    /// Scroll / viewport state.
    pub viewport: Viewport,
    /// Width of the window in columns.
    pub width: u16,
    /// Height of the window in rows.
    pub height: u16,
    /// X offset in the terminal for split layouts.
    pub x_offset: u16,
    /// Y offset in the terminal for split layouts.
    pub y_offset: u16,
    /// Selection anchor for visual modes (Visual, VisualLine, VisualBlock).
    /// Set when entering a visual mode; `None` in Normal/Insert/Command.
    /// The selection range is from `selection_anchor` to `cursor.position`.
    pub selection_anchor: Option<crate::buffer::CursorPosition>,
}

impl Window {
    /// Create a new window viewing the given buffer.
    pub fn new(buffer_id: BufferId) -> Self {
        Self {
            id: new_window_id(),
            buffer_id,
            cursor: WindowCursor::default(),
            viewport: Viewport::new(),
            width: 80,
            height: 24,
            x_offset: 0,
            y_offset: 0,
            selection_anchor: None,
        }
    }

    /// Clamp the cursor to the visible area and buffer bounds.
    /// Returns true if the cursor moved.
    pub fn clamp_cursor(&mut self, max_line: usize, line_len: usize) -> bool {
        let old = self.cursor.position;

        // Clamp line.
        if max_line > 0 {
            self.cursor.position.line = self.cursor.position.line.min(max_line - 1);
        }

        // Clamp column.
        if line_len > 0 {
            self.cursor.position.col = self.cursor.position.col.min(line_len);
        } else {
            self.cursor.position.col = 0;
        }

        self.cursor.position != old
    }

    /// Ensure the cursor is visible by adjusting the viewport scroll.
    pub fn ensure_cursor_visible(&mut self, max_line: usize) {
        let pos = self.cursor.position;
        let edit_height = self.height.saturating_sub(1) as usize;

        // Vertical scrolling.
        if pos.line < self.viewport.scroll_line {
            self.viewport.scroll_line = pos.line;
        } else if pos.line >= self.viewport.scroll_line + edit_height {
            self.viewport.scroll_line = pos.line - edit_height + 1;
        }

        // Clamp scroll_line to buffer end.
        if max_line > 0 && self.viewport.scroll_line > 0 {
            let max_scroll = max_line.saturating_sub(edit_height);
            self.viewport.scroll_line = self.viewport.scroll_line.min(max_scroll);
        }

        // Horizontal scrolling (simple: just follow cursor).
        let edit_width = self.width as usize;
        if pos.col >= self.viewport.scroll_col as usize + edit_width {
            self.viewport.scroll_col = (pos.col - edit_width + 1) as u16;
        } else if pos.col < self.viewport.scroll_col as usize {
            self.viewport.scroll_col = pos.col as u16;
        }
    }

    /// Resize the window.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Resize and reposition the window.
    pub fn place(&mut self, x: u16, y: u16, width: u16, height: u16) {
        self.x_offset = x;
        self.y_offset = y;
        self.width = width;
        self.height = height;
    }

    /// Switch this window to view a different buffer.
    pub fn set_buffer(&mut self, buffer_id: BufferId) {
        self.buffer_id = buffer_id;
        self.cursor = WindowCursor::default();
        self.viewport = Viewport::new();
    }
}

// ── Split direction ────────────────────────────────────────────────

/// Direction of a window split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Split horizontally — children arranged top and bottom.
    Horizontal,
    /// Split vertically — children arranged left and right.
    Vertical,
}

// ── Separator info (for rendering) ─────────────────────────────────

/// Information about a separator line between two split panes, used by the renderer.
#[derive(Debug, Clone, Copy)]
pub struct Separator {
    /// Direction of the separator.
    pub direction: SplitDirection,
    /// For vertical separators (│): the x column.
    /// For horizontal separators (─): the x start column.
    pub x: u16,
    /// For horizontal separators (─): the y row.
    /// For vertical separators (│): the y start row.
    pub y: u16,
    /// Length of the separator in cells.
    pub length: u16,
}

// ── Split tree node ────────────────────────────────────────────────

/// A node in the binary split tree.
///
/// Leaf nodes are actual windows. Internal nodes represent splits that
/// divide their region between two children.
#[derive(Debug, Clone)]
pub enum SplitNode {
    /// A leaf — an actual window displaying a buffer.
    Leaf(WindowId),
    /// An internal split — divides space between two children.
    Split {
        /// Direction of this split.
        direction: SplitDirection,
        /// First (top or left) child.
        first: Box<SplitNode>,
        /// Second (bottom or right) child.
        second: Box<SplitNode>,
        /// Size ratio for the first child (0.0..1.0). The second gets (1.0 - ratio).
        ratio: f64,
    },
}

impl SplitNode {
    /// Create a new leaf node.
    pub fn leaf(window_id: WindowId) -> Self {
        SplitNode::Leaf(window_id)
    }

    /// Create a new split node with the given direction and equal ratio.
    pub fn split(direction: SplitDirection, first: SplitNode, second: SplitNode) -> Self {
        SplitNode::Split {
            direction,
            first: Box::new(first),
            second: Box::new(second),
            ratio: 0.5,
        }
    }

    /// Count the total number of leaf (window) nodes.
    pub fn leaf_count(&self) -> usize {
        match self {
            SplitNode::Leaf(_) => 1,
            SplitNode::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    /// Collect all leaf window IDs in tree order.
    pub fn leaf_ids(&self) -> Vec<WindowId> {
        match self {
            SplitNode::Leaf(id) => vec![*id],
            SplitNode::Split { first, second, .. } => {
                let mut ids = first.leaf_ids();
                ids.extend(second.leaf_ids());
                ids
            }
        }
    }

    /// Find the leaf node for a given window ID, returning `true` if found.
    pub fn contains(&self, window_id: WindowId) -> bool {
        match self {
            SplitNode::Leaf(id) => *id == window_id,
            SplitNode::Split { first, second, .. } => first.contains(window_id) || second.contains(window_id),
        }
    }

    /// Replace the leaf with the given `window_id` with a new subtree.
    /// Used when splitting a window.
    pub fn replace_leaf(&mut self, window_id: WindowId, new_node: SplitNode) -> bool {
        match self {
            SplitNode::Leaf(id) if *id == window_id => {
                *self = new_node;
                true
            }
            SplitNode::Leaf(_) => false,
            SplitNode::Split { first, second, .. } => {
                first.replace_leaf(window_id, new_node.clone()) || second.replace_leaf(window_id, new_node)
            }
        }
    }

    /// Remove a leaf by window_id. If the leaf is in a Split, replace the
    /// Split with its sibling. Returns the removed WindowId.
    pub fn remove_leaf(&mut self, window_id: WindowId) -> Option<WindowId> {
        match self {
            SplitNode::Leaf(_) => None,
            SplitNode::Split { first, second, .. } => {
                // Try removing from first child (recursive).
                if first.as_ref().contains(window_id) {
                    // If first child is the leaf itself, replace this split with second.
                    if let SplitNode::Leaf(id) = first.as_ref() {
                        if *id == window_id {
                            let sibling = (**second).clone();
                            *self = sibling;
                            return Some(window_id);
                        }
                    }
                    return first.remove_leaf(window_id);
                }
                // Try removing from second child (recursive).
                if second.as_ref().contains(window_id) {
                    // If second child is the leaf itself, replace this split with first.
                    if let SplitNode::Leaf(id) = second.as_ref() {
                        if *id == window_id {
                            let sibling = (**first).clone();
                            *self = sibling;
                            return Some(window_id);
                        }
                    }
                    return second.remove_leaf(window_id);
                }
                None
            }
        }
    }

    /// Recursively compute geometry for all windows and collect separators.
    /// Places each window into the `windows` map and collects separator info.
    pub fn compute_layout(
        &self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        windows: &mut std::collections::HashMap<WindowId, (u16, u16, u16, u16)>,
        separators: &mut Vec<Separator>,
    ) {
        match self {
            SplitNode::Leaf(id) => {
                windows.insert(*id, (x, y, width, height));
            }
            SplitNode::Split {
                direction,
                first,
                second,
                ratio,
            } => {
                let r = *ratio;
                match direction {
                    SplitDirection::Horizontal => {
                        // Top/bottom split. Reserve 1 row for separator.
                        let avail = height.saturating_sub(1);
                        let first_h = ((avail as f64) * r) as u16;
                        let second_h = avail.saturating_sub(first_h);
                        let sep_y = y + first_h;
                        separators.push(Separator {
                            direction: SplitDirection::Horizontal,
                            x,
                            y: sep_y,
                            length: width,
                        });
                        first.compute_layout(x, y, width, first_h, windows, separators);
                        second.compute_layout(x, sep_y + 1, width, second_h, windows, separators);
                    }
                    SplitDirection::Vertical => {
                        // Left/right split. Reserve 1 col for separator.
                        let avail = width.saturating_sub(1);
                        let first_w = ((avail as f64) * r) as u16;
                        let second_w = avail.saturating_sub(first_w);
                        let sep_x = x + first_w;
                        separators.push(Separator {
                            direction: SplitDirection::Vertical,
                            x: sep_x,
                            y,
                            length: height,
                        });
                        first.compute_layout(x, y, first_w, height, windows, separators);
                        second.compute_layout(sep_x + 1, y, second_w, height, windows, separators);
                    }
                }
            }
        }
    }
}

// ── Layout enum (kept for compatibility, derived from tree) ─────────

/// How the top-level windows are arranged. Derived from the split tree root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Single,
    HorizontalSplit,
    VerticalSplit,
}

// ── Window Manager ──────────────────────────────────────────────────

/// Manages all windows using a tree-based layout for nested splits.
///
/// The `root` tree determines the geometry. The `windows` vec stores the
/// actual `Window` structs (cursor, viewport, etc.) by `WindowId`.
#[derive(Debug)]
pub struct WindowManager {
    /// The split tree root. None means no windows.
    root: Option<SplitNode>,
    /// All window instances, keyed by their WindowId.
    windows: Vec<Window>,
    /// The currently active window.
    active_id: Option<WindowId>,
}

impl WindowManager {
    pub fn new() -> Self {
        Self {
            root: None,
            windows: Vec::new(),
            active_id: None,
        }
    }

    /// Create a new window and return its id. Also sets it as active.
    pub fn create_window(&mut self, buffer_id: BufferId) -> WindowId {
        let window = Window::new(buffer_id);
        let id = window.id;
        self.windows.push(window);
        self.root = Some(SplitNode::leaf(id));
        self.active_id = Some(id);
        id
    }

    /// Return a reference to the active window.
    pub fn active_window(&self) -> Option<&Window> {
        self.active_id.and_then(|id| self.windows.iter().find(|w| w.id == id))
    }

    /// Return a mutable reference to the active window.
    pub fn active_window_mut(&mut self) -> Option<&mut Window> {
        self.active_id.and_then(|id| self.windows.iter_mut().find(|w| w.id == id))
    }

    /// Return a reference to a window by id.
    pub fn get(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// Return a mutable reference to a window by id.
    pub fn get_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// Set the active window.
    pub fn set_active(&mut self, id: WindowId) {
        if self.windows.iter().any(|w| w.id == id) {
            self.active_id = Some(id);
        }
    }

    /// Return the number of open windows (leaf nodes).
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Return true if there are no windows.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Iterate over all windows.
    pub fn iter(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter()
    }

    /// Mutably iterate over all windows.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Window> {
        self.windows.iter_mut()
    }

    /// Return the current layout type derived from the tree root.
    pub fn layout(&self) -> Layout {
        match &self.root {
            None | Some(SplitNode::Leaf(_)) => Layout::Single,
            Some(SplitNode::Split {
                direction: SplitDirection::Horizontal,
                ..
            }) => Layout::HorizontalSplit,
            Some(SplitNode::Split {
                direction: SplitDirection::Vertical,
                ..
            }) => Layout::VerticalSplit,
        }
    }

    // ── Split operations ──────────────────────────────────────────

    /// Minimum usable width for a window (content area).
    const MIN_WINDOW_WIDTH: u16 = 12;
    /// Minimum usable height for a window (content area).
    const MIN_WINDOW_HEIGHT: u16 = 4;

    /// Split the active window in the given direction.
    /// Creates a new window with the same buffer, places it as the sibling,
    /// and makes it the new active window.
    ///
    /// Returns `Ok(())` on success, or `Err(msg)` if the active window
    /// is too small to split further.
    pub fn split_active(&mut self, direction: SplitDirection) -> Result<(), String> {
        let active_id = match self.active_id {
            Some(id) => id,
            None => {
                return Err("No active window to split.".to_string());
            }
        };

        // Get the active window's current geometry for min-size check.
        let active_geom = self
            .windows
            .iter()
            .find(|w| w.id == active_id)
            .map(|w| (w.width, w.height, w.buffer_id));

        let (win_w, win_h, buffer_id) = match active_geom {
            Some(g) => g,
            None => {
                return Err("Active window not found.".to_string());
            }
        };

        // Check if the active window is large enough to split.
        // After split, each child gets ~(size-1)/2 (minus separator).
        match direction {
            SplitDirection::Horizontal => {
                // Horizontal split: top/bottom. Each child gets (height-1)/2.
                let child_h = win_h.saturating_sub(1) / 2;
                if child_h < Self::MIN_WINDOW_HEIGHT {
                    return Err(format!(
                        "Window too small for horizontal split ({} rows, need {}).",
                        win_h,
                        Self::MIN_WINDOW_HEIGHT * 2 + 1
                    ));
                }
            }
            SplitDirection::Vertical => {
                // Vertical split: left/right. Each child gets (width-1)/2.
                let child_w = win_w.saturating_sub(1) / 2;
                if child_w < Self::MIN_WINDOW_WIDTH {
                    return Err(format!(
                        "Window too small for vertical split ({} cols, need {}).",
                        win_w,
                        Self::MIN_WINDOW_WIDTH * 2 + 1
                    ));
                }
            }
        }

        // Create a new window with the same buffer.
        let new_window = Window::new(buffer_id);
        let new_id = new_window.id;
        self.windows.push(new_window);

        // Build the new subtree: a Split with the old leaf and new leaf.
        let new_node = SplitNode::split(direction, SplitNode::leaf(active_id), SplitNode::leaf(new_id));

        // Replace the active leaf in the tree.
        if let Some(ref mut root) = self.root {
            let replaced = root.replace_leaf(active_id, new_node);
            if !replaced {
                // Rollback: remove the new window we just added.
                self.windows.retain(|w| w.id != new_id);
                return Err("Failed to split: active window not found in layout tree.".to_string());
            }
        } else {
            self.windows.retain(|w| w.id != new_id);
            return Err("Failed to split: no layout tree.".to_string());
        }

        // Activate the new window.
        self.active_id = Some(new_id);

        Ok(())
    }

    /// Split the active window and load a different buffer into the new pane.
    /// Returns `Ok(())` on success or `Err(msg)` on failure.
    pub fn split_active_with_buffer(&mut self, direction: SplitDirection, buffer_id: BufferId) -> Result<(), String> {
        let active_id = match self.active_id {
            Some(id) => id,
            None => {
                return Err("No active window to split.".to_string());
            }
        };

        // Get the active window's current geometry for min-size check.
        let (win_w, win_h) = self
            .windows
            .iter()
            .find(|w| w.id == active_id)
            .map(|w| (w.width, w.height))
            .unwrap_or((0, 0));

        // Check if the active window is large enough to split.
        match direction {
            SplitDirection::Horizontal => {
                let child_h = win_h.saturating_sub(1) / 2;
                if child_h < Self::MIN_WINDOW_HEIGHT {
                    return Err(format!(
                        "Window too small for horizontal split ({} rows, need {}).",
                        win_h,
                        Self::MIN_WINDOW_HEIGHT * 2 + 1
                    ));
                }
            }
            SplitDirection::Vertical => {
                let child_w = win_w.saturating_sub(1) / 2;
                if child_w < Self::MIN_WINDOW_WIDTH {
                    return Err(format!(
                        "Window too small for vertical split ({} cols, need {}).",
                        win_w,
                        Self::MIN_WINDOW_WIDTH * 2 + 1
                    ));
                }
            }
        }

        // Create a new window with the specified buffer.
        let new_window = Window::new(buffer_id);
        let new_id = new_window.id;
        self.windows.push(new_window);

        // Build the new subtree.
        let new_node = SplitNode::split(direction, SplitNode::leaf(active_id), SplitNode::leaf(new_id));

        if let Some(ref mut root) = self.root {
            let replaced = root.replace_leaf(active_id, new_node);
            if !replaced {
                self.windows.retain(|w| w.id != new_id);
                return Err("Failed to split: active window not found in layout tree.".to_string());
            }
        } else {
            self.windows.retain(|w| w.id != new_id);
            return Err("Failed to split: no layout tree.".to_string());
        }

        self.active_id = Some(new_id);

        Ok(())
    }

    // ── Close operations ──────────────────────────────────────────

    /// Close the window with the given id.
    /// Returns true if the window was closed.
    pub fn close_window(&mut self, id: WindowId) -> bool {
        if self.windows.len() <= 1 {
            return false; // Don't close the last window.
        }

        // Remove from the tree.
        if let Some(ref mut root) = self.root {
            root.remove_leaf(id);
        }

        // Remove from the vec.
        if let Some(idx) = self.windows.iter().position(|w| w.id == id) {
            self.windows.remove(idx);
        }

        // If we closed the active window, activate the first remaining one.
        if self.active_id == Some(id) {
            self.active_id = self.windows.first().map(|w| w.id);
        }

        // If only one window remains, simplify the root to a Leaf.
        if self.windows.len() == 1 {
            let last_id = self.windows[0].id;
            self.root = Some(SplitNode::leaf(last_id));
        }

        true
    }

    /// Close all windows except the active one.
    pub fn close_all_others(&mut self) {
        let active_id = match self.active_id {
            Some(id) => id,
            None => return,
        };
        let buffer_id = self.windows.iter().find(|w| w.id == active_id).map(|w| w.buffer_id);

        // Simplify to a single leaf.
        self.root = Some(SplitNode::leaf(active_id));

        // Remove all windows except the active one.
        self.windows.retain(|w| w.id == active_id);

        // Ensure the buffer_id is still correct.
        if let Some(win) = self.windows.iter_mut().find(|w| w.id == active_id) {
            if let Some(bid) = buffer_id {
                win.buffer_id = bid;
            }
        }
    }

    // ── Window navigation ─────────────────────────────────────────

    /// Switch to the next window (cycling in tree order).
    pub fn next_window(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let current = self.active_id.unwrap_or(self.windows[0].id);
        if let Some(idx) = self.windows.iter().position(|w| w.id == current) {
            let next = (idx + 1) % self.windows.len();
            self.active_id = Some(self.windows[next].id);
        }
    }

    /// Switch to the previous window (cycling in tree order).
    pub fn prev_window(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let current = self.active_id.unwrap_or(self.windows[0].id);
        if let Some(idx) = self.windows.iter().position(|w| w.id == current) {
            let prev = if idx == 0 { self.windows.len() - 1 } else { idx - 1 };
            self.active_id = Some(self.windows[prev].id);
        }
    }

    // ── Layout computation ────────────────────────────────────────

    /// Resize all windows to fit the given terminal dimensions.
    /// `term_height` should include the 3-line status area; we subtract it internally.
    /// Also returns a list of separators that the renderer should draw.
    pub fn resize_all(&mut self, term_width: u16, term_height: u16) {
        let status_height: u16 = 3;
        let edit_height = term_height.saturating_sub(status_height);

        // Compute geometry from the tree.
        let mut geom = std::collections::HashMap::new();
        let mut seps = Vec::new();

        if let Some(ref root) = self.root {
            root.compute_layout(0, 0, term_width, edit_height, &mut geom, &mut seps);
        }

        // Apply computed positions to each window.
        for window in &mut self.windows {
            if let Some(&(x, y, w, h)) = geom.get(&window.id) {
                window.place(x, y, w, h);
            }
        }
    }

    /// Collect the separators for the current layout (for rendering).
    pub fn compute_separators(&self, term_width: u16, term_height: u16) -> Vec<Separator> {
        let status_height: u16 = 3;
        let edit_height = term_height.saturating_sub(status_height);
        let mut seps = Vec::new();

        if let Some(ref root) = self.root {
            let mut geom = std::collections::HashMap::new();
            root.compute_layout(0, 0, term_width, edit_height, &mut geom, &mut seps);
        }

        seps
    }
}

impl Default for WindowManager {
    fn default() -> Self {
        Self::new()
    }
}
