//! LSP subsystem state — extracted from the Editor core.
//!
//! Groups all Language Server Protocol related fields.

use std::collections::HashMap;
use std::time::Instant;

use crate::buffer::BufferId;

// ── LSP state ─────────────────────────────────────────────────────

/// LSP subsystem state — extracted from Editor to reduce the core struct size.
pub struct LspState {
    /// LSP message sender (editor → async LSP task).
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::lsp::LspMessage>,
    /// Whether an LSP completion request is in flight.
    pub completion_pending: bool,
    pub completion_was_trigger: bool,
    /// Whether the LSP server has connected and initialized.
    pub connected: bool,
    /// Cached LSP diagnostics per URI.
    pub diagnostics: HashMap<String, Vec<crate::lsp::Diagnostic>>,
    /// Current signature help state (for info bar display).
    pub signature_help: Option<crate::lsp::SignatureHelpState>,
    /// Inlay hints per URI.
    pub inlay_hints: HashMap<String, Vec<crate::lsp::InlayHint>>,
    /// Whether an LSP didChange notification is pending (debounced).
    pub change_pending: bool,
    /// Deadline after which to send the pending LSP didChange.
    pub change_deadline: Option<Instant>,
    /// LSP change debounce interval in milliseconds.
    pub change_debounce_ms: u64,
    /// LSP document version counter.
    pub doc_version: i32,
    /// Whether formatting is pending.
    pub formatting_pending: bool,
    /// Buffer ID for pending formatting operation.
    pub formatting_buffer_id: Option<BufferId>,
}

impl LspState {
    /// Create a new LspState with the given channel sender.
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<crate::lsp::LspMessage>) -> Self {
        Self {
            tx,
            completion_pending: false,
            completion_was_trigger: false,
            connected: false,
            diagnostics: HashMap::new(),
            signature_help: None,
            inlay_hints: HashMap::new(),
            change_pending: false,
            change_deadline: None,
            change_debounce_ms: 20,
            doc_version: 0,
            formatting_pending: false,
            formatting_buffer_id: None,
        }
    }
}
