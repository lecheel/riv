// src/overlay.rs  (or inline in render.rs)

use crate::dirty::Rect;

/// Tracks the screen region occupied by each active popup,
/// so we can restore editor content when a popup closes or shrinks.
#[derive(Default)]
pub struct OverlayTracker {
    pub completion: Option<Rect>,
    pub help: Option<Rect>,
    pub file_picker: Option<Rect>,
    pub buffer_list: Option<Rect>,
    pub float: Option<Rect>,
    pub diff: Option<Rect>,
    pub mru: Option<Rect>,
    pub function_list: Option<crate::dirty::Rect>,
}
