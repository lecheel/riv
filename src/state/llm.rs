//! LLM subsystem state — extracted from the Editor core.
//!
//! Groups all LLM-related fields and provides key-handlers for the
//! LLM Input buffer and LlmPrompt mode bindings.

use tokio::sync::mpsc;

use crate::buffer::{BufferId, BufferKind};
use crate::ed::{BufferOpsExt, LlmExt};
use crate::editor::{CommandResult, Mode};
use crate::llm::LlmBuffer;
use crate::prompt::{MiniInputPrompt, PromptAction};
use crate::terminal::Key;
use crate::Editor;

// ── LLM state ─────────────────────────────────────────────────────

/// LLM subsystem state — extracted from Editor to reduce the core struct size.
pub struct LlmState {
    /// The LLM conversation buffer state (attached to BufferKind::Llm)
    pub buffer: LlmBuffer,
    /// Whether to prefix the user's LLM prompt with "##TODO" (set by `'` in visual mode).
    pub todo_prefix: bool,
    /// Handle to the LLM buffer ID (if created)
    pub buffer_id: Option<BufferId>,
    /// Tokio runtime for async LLM operations.
    pub runtime: tokio::runtime::Runtime,
    /// Channel sender for LLM async tasks to send responses back.
    pub response_tx: mpsc::UnboundedSender<Result<String, String>>,
    /// Channel receiver polled in tick() for completed LLM responses.
    pub response_rx: mpsc::UnboundedReceiver<Result<String, String>>,
    /// Handle to the running LLM async task (for abort on cancel).
    pub task_handle: Option<tokio::task::JoinHandle<()>>,
    /// The preset to use if the user triggers a quick LLM action
    pub active_preset: Option<crate::llm::LlmPreset>,
    /// Context text (e.g., visual selection) for the active LLM prompt
    pub active_context: Option<String>,
    /// Buffer ID the user was editing before opening the LLM split layout.
    pub origin_buffer_id: Option<BufferId>,
    /// Instead of being appended to the LLM conversation buffer, show in info bar.
    pub infobar_response: bool,
    /// Accumulator for infobar-bound LLM responses (streaming chunks).
    pub infobar_accumulator: String,
    pub single_shot: bool,
    /// LLM prompt state.
    pub prompt: MiniInputPrompt,
}

impl LlmState {
    /// Create a new LlmState with a fresh runtime and channels.
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for LLM");

        let (response_tx, response_rx) =
            tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();

        Self {
            buffer: LlmBuffer::new(),
            todo_prefix: false,
            buffer_id: None,
            runtime,
            response_tx,
            response_rx,
            task_handle: None,
            active_preset: None,
            active_context: None,
            origin_buffer_id: None,
            infobar_response: false,
            infobar_accumulator: String::new(),
            single_shot: false,
            prompt: MiniInputPrompt::new(),
        }
    }
}

// ── LLM key dispatch ──────────────────────────────────────────────

impl Editor {
    /// Handle special keys when the active buffer is a LlmInput buffer.
    ///
    /// Returns `Some(CommandResult)` if the key was consumed, `None` to
    /// fall through to normal navigation keybinds.
    pub fn handle_llm_input_buffer_key(&mut self, key: &Key) -> Option<CommandResult> {
        if self.mode != Mode::Normal {
            return None;
        }
        let is_llm_input = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.kind == BufferKind::LlmInput)
            .unwrap_or(false);

        if !is_llm_input {
            return None;
        }

        match key {
            Key::Enter => Some(self.llm_send_input_buffer()),
            Key::Char('q') => Some(self.llm_close_split_session()),
            _ => None, // Fall through to normal Vim keybinds
        }
    }

    /// Process keys in LlmPrompt mode.
    ///
    /// Takes ownership of `key` because LlmPrompt mode fully consumes it
    /// (it never falls through to the normal keybind processing).
    pub fn process_llm_prompt_key(&mut self, key: Key) -> CommandResult {
        // Show hints when Ctrl-R is initially pressed
        if key == Key::Ctrl('r') {
            self.cmd_waiting_register = true;
            self.which_key_hints = vec![
                ("Register".to_string(), String::new()),
                ("^W".to_string(), "word".to_string()),
                ("^L".to_string(), "line".to_string()),
                ("^A".to_string(), "whole line".to_string()),
                ("%".to_string(), "file path".to_string()),
                ("^P".to_string(), "abs file path".to_string()),
                ("ESC".to_string(), "cancel".to_string()),
            ];
            self.which_key_debounce_timer = None;
            self.dirty.status_infobar = true;
            self.dirty.cursor = true;
            return CommandResult::ViewChanged;
        }

        if self.cmd_waiting_register {
            // Clear waiting state and infobar hints
            self.cmd_waiting_register = false;
            let had_hints = !self.which_key_hints.is_empty();
            self.which_key_hints.clear();
            if had_hints {
                self.dirty.status_infobar = true;
            }

            match key {
                Key::Ctrl('w') => {
                    let word = self.word_under_cursor_in_current_buffer();
                    if !word.is_empty() {
                        self.llm
                            .prompt
                            .buffer
                            .insert_str(self.llm.prompt.cursor, &word);
                        self.llm.prompt.cursor += word.len();
                        self.dirty.mark_all();
                        return CommandResult::ViewChanged;
                    } else {
                        self.set_infobar_message("No word under cursor".to_string());
                        return CommandResult::ViewChanged;
                    }
                }
                Key::Ctrl('l') => {
                    let line = self.current_line_content();
                    if !line.is_empty() {
                        self.llm
                            .prompt
                            .buffer
                            .insert_str(self.llm.prompt.cursor, &line);
                        self.llm.prompt.cursor += line.len();
                        self.dirty.mark_all();
                        return CommandResult::ViewChanged;
                    } else {
                        self.set_infobar_message("Current line is empty".to_string());
                        return CommandResult::ViewChanged;
                    }
                }
                Key::Char('%') | Key::Ctrl('p') => {
                    let path = self
                        .current_buffer()
                        .and_then(|b| b.file_path.as_ref())
                        .and_then(|p| p.canonicalize().ok())
                        .and_then(|p| p.to_str().map(|s| s.to_string()))
                        .unwrap_or_default();

                    if !path.is_empty() {
                        self.llm
                            .prompt
                            .buffer
                            .insert_str(self.llm.prompt.cursor, &path);
                        self.llm.prompt.cursor += path.len();
                        self.dirty.mark_all();
                        return CommandResult::ViewChanged;
                    } else {
                        self.set_infobar_message("No file path for current buffer".to_string());
                        return CommandResult::ViewChanged;
                    }
                }
                Key::Ctrl('a') => {
                    let line = self.current_line_content();
                    self.llm
                        .prompt
                        .buffer
                        .insert_str(self.llm.prompt.cursor, &line);
                    self.llm.prompt.cursor += line.len();
                    self.dirty.mark_all();
                    return CommandResult::ViewChanged;
                }
                Key::Escape | Key::Ctrl('c') => {
                    // Cancel register wait, but let Escape fall through to cancel LlmPrompt mode entirely
                }
                _ => {
                    // Invalid register key, cancel wait and swallow the key.
                    return CommandResult::ViewChanged;
                }
            }
            // If we didn't return above (e.g. on Escape), fall through to normal LLM prompt handling
        }

        match self.llm.prompt.handle_key(&key) {
            PromptAction::Changed => {
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            PromptAction::Submit => {
                let input = self.llm.prompt.text().to_string();
                self.llm.prompt.clear();
                self.llm.prompt.push_history(input.clone());
                self.clear_messages();
                self.mode = Mode::Normal;
                self.dirty.mark_all();
                self.llm_send_from_prompt(input)
            }
            PromptAction::Cancel => {
                self.llm.prompt.clear();
                self.llm.active_preset = None;
                self.llm.active_context = None;
                self.llm.todo_prefix = false;
                self.mode = Mode::Normal;
                self.dirty.mark_all();
                CommandResult::ModeChanged(Mode::Normal)
            }
            PromptAction::None => CommandResult::NoOp,
        }
    }
}
