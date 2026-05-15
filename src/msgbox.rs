// src/msgbox.rs
use crate::editor::Mode;
use crate::lsp::Diagnostic;
use crate::lsp::{CompletionItem, InlayHint, SignatureHelpState, TextEdit};
use tokio::sync::mpsc;

/// Type alias - all subsystems use this directly
pub type AppSender = mpsc::UnboundedSender<AppMessage>;
pub type AppReceiver = mpsc::UnboundedReceiver<AppMessage>;
pub type SearchResult = Vec<crate::ripgrep::RipgrepResult>;
pub type GitDiffResult = String;

pub fn message_channel() -> (AppSender, AppReceiver) {
    mpsc::unbounded_channel()
}

#[derive(Debug)]
pub enum AppMessage {
    // LSP
    LspDiagnostics {
        uri: String,
        version: Option<i32>,
        diagnostics: Vec<Diagnostic>,
    },
    FormatResult {
        buffer_id: crate::buffer::BufferId,
        result: Result<String, String>,
        cursor_state: crate::buffer::CursorPosition,
        save_after: bool,
    },
    /// LSP server has connected and initialized successfully.
    LspReady,
    LspInlayHints {
        uri: String,
        hints: Vec<InlayHint>,
        version: i32,
    },
    LspSignatureHelp(Option<SignatureHelpState>),
    LspCompletion(Option<Vec<CompletionItem>>),
    LspCompletionResolved(crate::lsp::CompletionItem),
    LspFormatResult {
        result: Result<Option<Vec<TextEdit>>, String>,
        buffer_idx: usize,
        cursor_state: Option<(usize, usize)>,
        save_after: bool,
    },
    LspError(String),
    LspGotoDefinitionResult {
        locations: Vec<crate::lsp::Location>,
    },

    // RG
    RgSearchResult {
        pattern: String,
        results: SearchResult,
    },
    RgSearchError(String),

    // Git
    GitDiffResult(GitDiffResult),

    // General
    StatusMessage(String),
    ModeChange(Mode),
    Redraw,
}

impl AppMessage {
    /// Return a short label for debug logging.
    pub fn label(&self) -> &'static str {
        match self {
            AppMessage::LspDiagnostics { .. } => "LspDiagnostics",
            AppMessage::FormatResult { .. } => "FormatResult",
            AppMessage::LspReady => "LspReady",
            AppMessage::LspInlayHints { .. } => "LspInlayHints",
            AppMessage::LspSignatureHelp(_) => "LspSignatureHelp",
            AppMessage::LspCompletion(_) => "LspCompletion",
            AppMessage::LspGotoDefinitionResult { .. } => "LspGotoDefinitionResult",
            AppMessage::LspCompletionResolved(_) => "LspCompletionResolved",
            AppMessage::LspFormatResult { .. } => "LspFormatResult",
            AppMessage::LspError(_) => "LspError",
            AppMessage::RgSearchResult { .. } => "RgSearchResult",
            AppMessage::RgSearchError(_) => "RgSearchError",
            AppMessage::GitDiffResult(_) => "GitDiffResult",
            AppMessage::StatusMessage(_) => "StatusMessage",
            AppMessage::ModeChange(_) => "ModeChange",
            AppMessage::Redraw => "Redraw",
        }
    }

    /// Messages to suppress during LLM processing (visual noise)
    pub fn suppressed_during_llm(&self) -> bool {
        matches!(
            self,
            AppMessage::LspDiagnostics { .. }
                | AppMessage::LspInlayHints { .. }
                | AppMessage::LspSignatureHelp(_)
                | AppMessage::LspCompletion(_)
                | AppMessage::LspGotoDefinitionResult { .. }
        )
    }
}
