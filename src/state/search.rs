//! Search, Navigation, and Tag subsystem state — extracted from the Editor core.
//!
//! Groups all search, mark, tag, and substitute fields, and provides
//! key-handlers for search input mode and mark pending states.

use std::collections::HashMap;

use crate::buffer::{BufferId, CursorPosition};
use crate::ed::{CommandExt, MarksExt, SearchExt};
use crate::editor::{CommandResult, SearchDirection, SubstituteConfirmState};
use crate::prompt::{MiniInputPrompt, PromptAction};
use crate::session::PositionMap;
use crate::tags::{TagEntry, TagManager};
use crate::terminal::Key;
use crate::Editor;

// ── Search state ──────────────────────────────────────────────────

/// Search, Mark, and Tag subsystem state.
pub struct SearchState {
    /// Current search direction (forward/backward).
    pub direction: Option<SearchDirection>,
    /// Whether search input is active.
    pub input_active: bool,
    /// Current search pattern.
    pub pattern: Option<String>,
    /// Current search match positions.
    pub matches: Vec<CursorPosition>,
    /// Index of current match in search results.
    pub current_match: usize,
    /// Whether search matches need recomputation.
    pub highlight_enabled: bool,
    pub matches_dirty: bool,
    /// Buffer ID that the current search belongs to.
    pub buffer_id: Option<BufferId>,
    /// Command line history prefix for filtering.
    pub command_line_history_prefix: Option<String>,
    /// Search line history prefix for filtering.
    pub search_line_history_prefix: Option<String>,
    /// Search command history.
    pub history: Vec<String>,
    /// Current index in search history.
    pub history_idx: usize,
    /// Search line prompt state.
    pub prompt: MiniInputPrompt,
    /// Active substitute‑confirm state (set by :s/pat/repl/gc).
    pub substitute_confirm: Option<SubstituteConfirmState>,
    pub replace_count: usize,
    /// Named marks (a-z) → (buffer_id, cursor_position).
    pub marks: HashMap<char, (BufferId, CursorPosition)>,
    /// Position before the last gd / cross-buffer jump (for `` jump-back).
    pub last_jump_mark: Option<(BufferId, CursorPosition)>,
    /// Whether we're waiting for a mark name after pressing `m`.
    pub mark_pending: bool,
    /// Whether we're waiting for a mark name after pressing `` ` ``.
    pub goto_mark_pending: bool,
    /// Ctags manager for go-to-definition via tags file.
    pub tag_manager: TagManager,
    /// Current tag search results (for cycling with :tnext/:tprev).
    pub tag_results: Vec<TagEntry>,
    /// Session position persistence map.
    pub position_map: PositionMap,
}

impl SearchState {
    /// Create a new SearchState with history loaded.
    pub fn new(history: Vec<String>) -> Self {
        let history_len = history.len();
        let mut prompt = MiniInputPrompt::new();
        prompt.history = history.clone();
        prompt.history_index = history_len;

        Self {
            direction: None,
            input_active: false,
            pattern: None,
            matches: Vec::new(),
            current_match: 0,
            highlight_enabled: true,
            matches_dirty: false,
            buffer_id: None,
            command_line_history_prefix: None,
            search_line_history_prefix: None,
            history,
            history_idx: history_len,
            prompt,
            substitute_confirm: None,
            replace_count: 1,
            marks: HashMap::new(),
            last_jump_mark: None,
            mark_pending: false,
            goto_mark_pending: false,
            tag_manager: TagManager::new(),
            tag_results: Vec::new(),
            position_map: PositionMap::load(),
        }
    }
}

// ── Search key dispatch ────────────────────────────────────────────

impl Editor {
    /// Process keys in Search Input mode.
    ///
    /// Takes ownership of `key` because Search Input mode fully consumes it.
    pub fn process_search_input_key(&mut self, key: Key) -> CommandResult {
        // ── Prefix-smart history navigation ──
        if key == Key::Up {
            let _ = self.search_history_up();
            self.update_search_matches_from_prompt();
            self.dirty.status_cmdline = true;
            self.dirty.cursor = true;
            self.dirty.windows = true;
            return CommandResult::NoOp;
        }
        if key == Key::Down {
            let _ = self.search_history_down();
            self.update_search_matches_from_prompt();
            self.dirty.status_cmdline = true;
            self.dirty.cursor = true;
            self.dirty.windows = true;
            return CommandResult::NoOp;
        }

        match self.search.prompt.handle_key(&key) {
            PromptAction::Changed => {
                self.update_search_matches_from_prompt();
                self.dirty.status_cmdline = true;
                self.dirty.cursor = true;
                self.dirty.windows = true;
                CommandResult::NoOp
            }
            PromptAction::Submit => {
                let query = self.search.prompt.text().to_string();
                self.search.prompt.push_history(query);
                self.execute_search()
            }
            PromptAction::Cancel => {
                self.search.prompt.clear();
                self.dirty.mark_all();
                self.cancel_search()
            }
            PromptAction::None => {
                // If Backspace is pressed on empty search, cancel search
                if key == Key::Backspace && self.search.prompt.is_empty() {
                    self.search.prompt.clear();
                    self.dirty.mark_all();
                    return self.cancel_search();
                }
                CommandResult::NoOp
            }
        }
    }

    /// Helper to update search matches based on current prompt text.
    fn update_search_matches_from_prompt(&mut self) {
        let query = self.search.prompt.text();
        if query.is_empty() {
            self.clear_messages();
            self.search.matches.clear();
        } else {
            let matches = self.find_all_matches(query);
            self.search.matches = matches;
            if self.search.matches.is_empty() {
                self.set_infobar_message("No match".to_string());
            } else {
                self.set_status(format!("{} matches", self.search.matches.len()));
            }
        }
    }

    /// Handle the mark pending state (waiting for mark name after `m`).
    ///
    /// Returns `Some(CommandResult)` if the key was consumed, `None` to
    /// fall through to normal keybinds.
    pub fn handle_mark_pending_key(&mut self, key: &Key) -> Option<CommandResult> {
        if !self.search.mark_pending {
            return None;
        }

        self.search.mark_pending = false;
        if let Key::Char(c) = key {
            if c.is_ascii_lowercase() {
                return Some(self.set_mark(*c)); // Dereference &char
            }
        }
        // Invalid mark character — cancel and fall through
        None
    }

    /// Handle the goto mark pending state (waiting for mark name after `` ` ``).
    pub fn handle_goto_mark_pending_key(&mut self, key: &Key) -> Option<CommandResult> {
        if !self.search.goto_mark_pending {
            return None;
        }

        self.search.goto_mark_pending = false;
        if let Key::Char(c) = key {
            if *c == '`' {
                return Some(self.jump_back());
            } else if c.is_ascii_lowercase() {
                return Some(self.goto_mark(*c)); // Dereference &char
            }
        }
        // Invalid mark character — cancel and fall through
        None
    }
}
