// src/ed/repeat.rs
use crate::action::Action;
use crate::llm::LlmPreset;
use crate::CommandResult;
use crate::Editor;
use crate::ed::build::BuildExt;

/// Types of actions that can be repeated with dot command
#[derive(Clone, Debug, PartialEq)]
pub enum RepeatableAction {
    /// Insert text at cursor position
    Insert(String),
    /// Delete characters (count, direction)
    DeleteChars {
        count: usize,
        direction: DeleteDirection,
    },
    RipgrepNextResult,
    RipgrepPrevResult,
    QuickfixNext,
    QuickfixPrev,

    /// Delete line
    DeleteLine,
    DeleteAroundFunction,
    ToggleComment,
    /// Change (delete + insert)
    Change {
        deleted: String,
        inserted: String,
    },
    /// Replace character
    ReplaceChar(char),
    /// Paste from register
    Paste {
        register: char,
        after_cursor: bool,
    },
    /// Indent/outdent
    Indent {
        count: usize,
        outdent: bool,
    },
    IndentTs {
        count: usize,
    },
    /// Join lines
    JoinLines {
        count: usize,
    },
    /// Substitute (e.g., `s/old/new/`)
    Substitute {
        pattern: String,
        replacement: String,
        flags: String,
    },
    /// LLM quick action (translate, explain, summarize, check English)
    /// On repeat, re-grabs the current text under cursor.
    LlmQuickAction {
        preset: LlmPreset,
    },
    /// Custom command
    Custom(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeleteDirection {
    Left,
    Right,
}

/// Stores the last action for dot repeat
#[derive(Clone, Debug)]
pub struct LastAction {
    pub action: RepeatableAction,
    pub count: usize,
}

pub trait RepeatExt {
    /// Repeat the last action (dot command)
    fn repeat_last_action(&mut self) -> CommandResult;

    /// Record an action for potential repetition
    fn record_action(&mut self, action: RepeatableAction, count: usize);

    /// Clear the last action (e.g., after undo)
    fn clear_last_action(&mut self);
}

impl RepeatExt for Editor {
    fn repeat_last_action(&mut self) -> CommandResult {
        let action = self.last_action.action.clone();
        let original_count = self.current_count;
        self.current_count *= self.last_action.count;

        let action_to_process = match action {
            RepeatableAction::IndentTs { count: _ } => {
                self.set_infobar_message("Cannot repeat indent_ts yet".to_string());
                return CommandResult::NoOp;
            }
            RepeatableAction::DeleteLine => Action::DeleteLine,
            RepeatableAction::DeleteAroundFunction => Action::DeleteAroundFunction,
            RepeatableAction::Insert(text) => Action::InsertText(text),
            RepeatableAction::DeleteChars { .. } => Action::DeleteChar,
            RepeatableAction::ReplaceChar(c) => Action::InsertChar(c),
            RepeatableAction::Paste { after_cursor, .. } => {
                if after_cursor {
                    Action::PasteAfter
                } else {
                    Action::PasteBefore
                }
            }
            RepeatableAction::Indent { outdent, .. } => {
                if outdent {
                    Action::Dedent
                } else {
                    Action::Indent
                }
            }
            RepeatableAction::JoinLines { .. } => Action::JoinLines,
            RepeatableAction::RipgrepNextResult | RepeatableAction::QuickfixNext => {
                self.current_count = original_count;
                return self.quickfix_next();
            }
            RepeatableAction::RipgrepPrevResult | RepeatableAction::QuickfixPrev => {
                self.current_count = original_count;
                return self.quickfix_prev();
            }
            RepeatableAction::LlmQuickAction { preset } => {
                self.current_count = original_count;
                let status_msg = match preset {
                    LlmPreset::CheckEnglish => "Checking English",
                    LlmPreset::TranslateToChinese => "Translating → 中文",
                    LlmPreset::TranslateToEnglish => "Translating → English",
                    LlmPreset::Explain => "Explaining",
                    LlmPreset::Summarize => "Summarizing",
                    _ => "Processing",
                };
                return self.llm_quick_action(preset, status_msg);
            }
            _ => {
                self.set_infobar_message("Cannot repeat this action".to_string());
                return CommandResult::NoOp;
            }
        };

        let result = self.process_action(action_to_process);
        self.current_count = original_count;
        result
    }

    fn record_action(&mut self, action: RepeatableAction, count: usize) {
        self.last_action = LastAction { action, count };
        self.repeat_pending = true;
    }

    fn clear_last_action(&mut self) {
        self.last_action = LastAction::default();
        self.repeat_pending = false;
    }
}

impl Default for LastAction {
    fn default() -> Self {
        Self {
            action: RepeatableAction::Insert(String::new()),
            count: 1,
        }
    }
}
