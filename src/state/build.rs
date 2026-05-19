//! Build subsystem state — extracted from the Editor core.
//!
//! Groups all build-related fields and provides a key-handler for the
//! build buffer's special key bindings (Enter, y, n, N, q, l).

use std::time::Instant;

use crate::buffer::BufferKind;
use crate::ed::build::BuildExt;
use crate::ed::build::{BuildDiagnostic, BuildResult, BuildSeverity};
use crate::editor::CommandResult;
use crate::terminal::Key;
use crate::Editor;
use crate::Mode;

// ── Build state ─────────────────────────────────────────────────────

/// Build subsystem state — extracted from Editor to reduce the core struct size.
pub struct BuildState {
    /// Parsed diagnostics from the last `:build` run.
    pub diagnostics: Vec<BuildDiagnostic>,
    /// Channel sender for background build thread.
    pub response_tx: std::sync::mpsc::Sender<BuildResult>,
    /// Channel receiver polled in tick() for completed build results.
    pub response_rx: std::sync::mpsc::Receiver<BuildResult>,
    /// Whether a build is currently in progress.
    pub in_progress: bool,
    /// Timestamp when the current build started.
    pub start_time: Option<Instant>,
    /// Current frame index for the build spinner animation.
    pub spinner_idx: usize,
}

impl BuildState {
    /// Create a new BuildState with a fresh channel pair.
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            diagnostics: Vec::new(),
            response_tx: tx,
            response_rx: rx,
            in_progress: false,
            start_time: None,
            spinner_idx: 0,
        }
    }
}

impl Editor {
    // ── Build buffer key dispatch ───────────────────────────────────

    /// Handle special keys when the active buffer is a Build buffer.
    ///
    /// Returns `Some(CommandResult)` if the key was consumed, `None` to
    /// fall through to normal navigation keybinds.

    /// Handle special keys when the active buffer is a Build buffer.
    ///
    /// Returns `Some(CommandResult)` if the key was consumed, `None` to
    /// fall through to normal navigation keybinds.
    pub fn handle_build_buffer_key(&mut self, key: &Key) -> Option<CommandResult> {
        // Guard: only in Normal mode with a Build buffer active
        if self.mode != Mode::Normal {
            return None;
        }
        let is_build = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.kind == BufferKind::Build)
            .unwrap_or(false);

        if !is_build {
            return None;
        }

        match key {
            Key::Enter => {
                self.dirty.mark_all();
                Some(self.build_goto_error())
            }
            Key::Char('l') => Some(self.build_insert_brace_content()),
            Key::Char('y') => {
                if self.build.diagnostics.is_empty() {
                    return Some(CommandResult::Message(
                        "No errors/warnings to yank".to_string(),
                    ));
                }

                let mut yank_text = String::new();
                for diag in &self.build.diagnostics {
                    let severity_str = match diag.severity {
                        BuildSeverity::Error => "error",
                        BuildSeverity::Warning => "warning",
                        BuildSeverity::Note => "note",
                    };
                    yank_text.push_str(&format!(
                        "{}:{}:{}: {}: {}\n",
                        diag.file_path.display(),
                        diag.line_number,
                        diag.column,
                        severity_str,
                        diag.message
                    ));
                }

                self.yank_register = yank_text.clone();

                let result = match crate::clipboard::set_text(&yank_text) {
                    Ok(()) => CommandResult::Message(format!(
                        "Yanked {} diagnostic(s) to system clipboard",
                        self.build.diagnostics.len()
                    )),
                    Err(e) => CommandResult::Error(format!("Clipboard error: {}", e)),
                };
                Some(result)
            }
            Key::Char('n') => Some(self.build_next_error()),
            Key::Char('N') => Some(self.build_prev_error()),
            Key::Char('q') | Key::Char('Q') => Some(self.build_close()),
            _ => None, // fall through to normal navigation keybinds
        }
    }
}
