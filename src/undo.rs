//! Undo/redo management using grouped snapshots.
//!
//! The actual undo logic lives in `buffer.rs` (see `UndoGroup`, `begin_undo_group`,
//! `end_undo_group`). This module is retained for future tree-based branching
//! support and as a reference for the legacy approach.

// NOTE: The `UndoTree` and `UndoStack` have been replaced by snapshot-based
// `UndoGroup` management directly in `Buffer`.  See `buffer.rs` for the
// current implementation.
