// src/ed/command.rs
//! Command‑line mode (`:`), command execution, history, `:set`, and
//! **range-aware** command execution for visual‑selection interaction.

use crate::action::Action;
use crate::buffer::CursorPosition;
use crate::ed::editing::EditingExt;
use crate::ed::git::GitExt;
use crate::ed::MovementExt;
use crate::ed::ReplaceExt;
use crate::editor::{CommandResult, Editor};
use crate::prompt::MiniInputPrompt;
use unicode_segmentation::UnicodeSegmentation;

// ═══════════════════════════════════════════════════════════════════
// ── Range types & parsing ─────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

/// A parsed Vim-style command range.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandRange {
    /// `'<,'>` — last visual selection
    VisualSelection,
    /// `%` — entire file
    EntireFile,
    /// `{n},{m}` — explicit line range (0‑based, end inclusive)
    Lines { start: usize, end: usize },
    /// `{n}` — single line (0‑based)
    SingleLine(usize),
}

/// Split a command string into `(name, args)`, handling the `:s/pat/repl/flags`
/// syntax where arguments are attached directly to the command name via a delimiter.
fn split_command(cmd: &str) -> (&str, &str) {
    let cmd = cmd.trim_start();

    // `:s` with an attached delimiter (e.g. `s/pat/repl/flags`)
    if cmd.len() > 1
        && cmd.as_bytes()[0] == b's'
        && matches!(cmd.as_bytes()[1], b'/' | b'|' | b'#' | b'!')
    {
        return (&cmd[..1], &cmd[1..]);
    }

    // `:substitute` with an attached delimiter
    if cmd.starts_with("substitute/")
        || cmd.starts_with("substitute|")
        || cmd.starts_with("substitute#")
        || cmd.starts_with("substitute!")
    {
        return (&cmd[..9], &cmd[9..]);
    }

    // Default: split by first whitespace
    match cmd.split_once(|c: char| c.is_whitespace()) {
        Some((n, a)) => (n, a.trim_start()),
        None => (cmd, ""),
    }
}

/// Parse a command string, extracting any leading range specification.
/// Returns `(optional_range, remaining_command_body)`.
fn parse_command_range(cmd: &str) -> (Option<CommandRange>, &str) {
    let s = cmd.trim_start();

    // '<,'> — visual selection (also handle '>,'< which Vim accepts)
    if let Some(rest) = s.strip_prefix("'<,'>") {
        return (Some(CommandRange::VisualSelection), rest.trim_start());
    }
    if let Some(rest) = s.strip_prefix("'>,'<") {
        return (Some(CommandRange::VisualSelection), rest.trim_start());
    }

    // % — entire file
    if let Some(rest) = s.strip_prefix('%') {
        return (Some(CommandRange::EntireFile), rest.trim_start());
    }

    // Numeric ranges: {n},{m} or bare {n}  (1‑based Vim line numbers)
    let num_end = s
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);

    if num_end > 0 {
        if let Ok(n1) = s[..num_end].parse::<usize>() {
            let after_first = s[num_end..].trim_start();

            // {n},{m}
            if let Some(after_comma) = after_first.strip_prefix(',') {
                let after_comma = after_comma.trim_start();
                let num_end2 = after_comma
                    .char_indices()
                    .take_while(|(_, c)| c.is_ascii_digit())
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);

                if num_end2 > 0 {
                    if let Ok(n2) = after_comma[..num_end2].parse::<usize>() {
                        let start = n1.saturating_sub(1);
                        let end = n2.saturating_sub(1);
                        return (
                            Some(CommandRange::Lines {
                                start,
                                end: end.max(start),
                            }),
                            after_comma[num_end2..].trim_start(),
                        );
                    }
                }
            }

            // Bare {n}
            return (
                Some(CommandRange::SingleLine(n1.saturating_sub(1))),
                after_first,
            );
        }
    }

    (None, s)
}

/// Split a substitute argument string by `delim`, respecting `\`‑escapes.
///
/// `"foo\/bar/baz/gi"` with delim `/` → `["foo\/bar", "baz", "gi"]`
fn split_substitute_parts(s: &str, delim: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut char_idx = 0;
    let chars: Vec<char> = s.chars().collect();

    while char_idx < chars.len() && parts.len() < 3 {
        if chars[char_idx] == '\\' && char_idx + 1 < chars.len() {
            char_idx += 2; // skip escaped char
            continue;
        }
        if chars[char_idx] == delim {
            let byte_pos = s
                .char_indices()
                .nth(char_idx)
                .map(|(pos, _)| pos)
                .unwrap_or(s.len());
            parts.push(&s[start..byte_pos]);
            start = byte_pos + delim.len_utf8();
            char_idx += 1;
            continue;
        }
        char_idx += 1;
    }
    parts.push(&s[start..]);
    parts
}

/// Convert a Vim‑style replacement string to `regex`‑crate replacement.
///
/// | Vim     | regex crate | Meaning              |
/// |---------|-------------|----------------------|
/// | `&`     | `$0`        | whole match          |
/// | `\1`–`\9`| `$1`–`$9`  | capture groups       |
/// | `\&`    | `&`         | literal ampersand    |
/// | `\\`    | `\`         | literal backslash    |
/// | `\n`,`\r`| `\n`       | newline              |
/// | `\t`    | `\t`        | tab                  |
/// | `$`     | `$$`        | literal dollar       |
fn vim_replacement_to_regex(vim_repl: &str) -> String {
    let mut out = String::with_capacity(vim_repl.len());
    let chars: Vec<char> = vim_repl.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            if next.is_ascii_digit() && next != '0' {
                out.push('$');
                out.push(next);
                i += 2;
            } else if next == '&' {
                out.push('&');
                i += 2;
            } else if next == '\\' {
                out.push('\\');
                i += 2;
            } else if next == 'n' || next == 'r' {
                out.push('\n');
                i += 2;
            } else if next == 't' {
                out.push('\t');
                i += 2;
            } else {
                out.push(next); // strip unknown escape
                i += 2;
            }
        } else if chars[i] == '&' {
            out.push_str("$0");
            i += 1;
        } else if chars[i] == '$' {
            out.push_str("$$"); // escape for regex crate
            i += 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Parse substitute args like `/pattern/replacement/flags` or `|pattern|replacement|flags`.
fn parse_substitute_args(args: &str) -> Result<(String, String, String), String> {
    let args = args.trim();
    if args.is_empty() {
        return Err("E146: Regular expression is empty".to_string());
    }
    let delim = args.chars().next().unwrap();
    if !matches!(delim, '/' | '|' | '#' | '!') {
        return Err(format!(
            "Expected s/pattern/replacement/flags, got delimiter '{}'",
            delim
        ));
    }
    let rest = &args[delim.len_utf8()..];
    let parts = split_substitute_parts(rest, delim);
    if parts.len() < 2 || parts[0].is_empty() {
        return Err("E146: Regular expression is empty".to_string());
    }
    let pattern = parts[0].to_string();
    let replacement = parts.get(1).unwrap_or(&"").to_string();
    let flags = parts.get(2).unwrap_or(&"").to_string();
    Ok((pattern, replacement, flags))
}

// ═══════════════════════════════════════════════════════════════════
// ── Extension trait ───────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

/// Extension trait for command‑line operations.
pub trait CommandExt {
    /// Execute the current command line (triggered by `Enter` in command mode).
    fn execute_command(&mut self) -> CommandResult;

    /// Parse and run a command string (e.g., `"w"`, `"e main.rs"`, `"q!"`).
    fn run_command(&mut self, cmd: &str) -> CommandResult;

    /// Handle the `:set` command (e.g., `:set nu`, `:set ts=4`).
    fn handle_set_command(&mut self, setting: &str) -> CommandResult;

    /// Move up in command history (usually `Up` arrow or `Ctrl-p`).
    fn command_history_up(&mut self) -> CommandResult;

    /// Move down in command history (usually `Down` arrow or `Ctrl-n`).
    fn command_history_down(&mut self) -> CommandResult;

    /// Record a search pattern into search history.
    fn record_search(&mut self, pattern: &str);

    /// Move up in search history (Up arrow in `/` or `?` prompt).
    fn search_history_up(&mut self) -> CommandResult;

    /// Move down in search history (Down arrow in `/` or `?` prompt).
    fn search_history_down(&mut self) -> CommandResult;

    /// Context‑aware Up: dispatches to search or command history based on
    /// whether the current command line starts with `/` or `?`.
    fn history_up(&mut self) -> CommandResult;

    /// Context‑aware Down: dispatches to search or command history.
    fn history_down(&mut self) -> CommandResult;
}

// ── Helper functions for prefix‑aware history navigation ──────────

fn prompt_history_up(prompt: &mut MiniInputPrompt) -> bool {
    if prompt.history.is_empty() {
        return false;
    }

    // Capture the prefix when we first start navigating history
    let prefix = if prompt.history_index == prompt.history.len() {
        prompt.buffer.clone()
    } else {
        // Keep the original prefix we started with
        prompt.draft.clone().unwrap_or_default()
    };
    prompt.draft = Some(prefix.clone());

    let prefix_lower = prefix.to_lowercase();

    // Search backwards from current index for a match
    let start = if prompt.history_index == prompt.history.len() {
        prompt.history.len().saturating_sub(1)
    } else {
        prompt.history_index.saturating_sub(1)
    };

    for i in (0..=start).rev() {
        if prompt.history[i].to_lowercase().starts_with(&prefix_lower) {
            prompt.history_index = i;
            prompt.buffer = prompt.history[i].clone();
            prompt.cursor = prompt.buffer.len();
            return true;
        }
    }

    false // No match found above the current selection
}

fn prompt_history_down(prompt: &mut MiniInputPrompt) -> bool {
    if prompt.history.is_empty() {
        return false;
    }

    if prompt.history_index >= prompt.history.len().saturating_sub(1) {
        // Reached the bottom, restore the original typed prefix
        prompt.history_index = prompt.history.len();
        if let Some(prefix) = prompt.draft.take() {
            prompt.buffer = prefix;
        } else {
            prompt.buffer.clear();
        }
        prompt.cursor = prompt.buffer.len();
        return true;
    }

    let prefix = prompt.draft.clone().unwrap_or_default();
    let prefix_lower = prefix.to_lowercase();

    // Search forwards from current index for a match
    let start = prompt.history_index.saturating_add(1);

    for i in start..prompt.history.len() {
        if prompt.history[i].to_lowercase().starts_with(&prefix_lower) {
            prompt.history_index = i;
            prompt.buffer = prompt.history[i].clone();
            prompt.cursor = prompt.buffer.len();
            return true;
        }
    }

    // If no match below, jump to the bottom and restore prefix
    prompt.history_index = prompt.history.len();
    if let Some(prefix) = prompt.draft.take() {
        prompt.buffer = prefix;
    } else {
        prompt.buffer.clear();
    }
    prompt.cursor = prompt.buffer.len();
    true
}

// ═══════════════════════════════════════════════════════════════════
// ── Implementation ─────────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

impl CommandExt for Editor {
    // ────────────────────────────────────────────────────────────────
    // ── execute_command — now range‑aware ───────────────────────────
    // ────────────────────────────────────────────────────────────────

    fn execute_command(&mut self) -> CommandResult {
        let cmd = self.command_prompt.text().trim().to_string();
        self.command_prompt.clear();
        self.command_completion.cancel();

        if cmd.is_empty() {
            self.enter_mode(crate::editor::Mode::Normal);
            return CommandResult::NoOp;
        }

        // ── Search commands: /pattern or ?pattern (never have ranges) ──
        if let Some(p) = cmd.strip_prefix('/') {
            let pattern: &str = p.trim();
            if !pattern.is_empty() {
                self.record_search(pattern);
                self.search.pattern = Some(pattern.to_string());
            }
            self.enter_mode(crate::editor::Mode::Normal);
            return self.process_action(Action::SearchNext);
        }
        if let Some(p) = cmd.strip_prefix('?') {
            let pattern: &str = p.trim();
            if !pattern.is_empty() {
                self.record_search(pattern);
                self.search.pattern = Some(pattern.to_string());
            }
            self.enter_mode(crate::editor::Mode::Normal);
            return self.process_action(Action::SearchPrev);
        }

        // ── Parse range prefix ──
        let (range, cmd_body) = parse_command_range(&cmd);

        // ── Deduplicate digit-prefixed entries — keep only the latest ──
        // Covers bare line jumps (:888), ranged deletes (:5d), and substitutes (:10,20s/..)
        if cmd.trim_start().starts_with(|c: char| c.is_ascii_digit()) {
            self.command_prompt
                .history
                .retain(|c| !c.trim_start().starts_with(|ch: char| ch.is_ascii_digit()));
        }

        // Record in history
        self.command_prompt.push_history(cmd.clone());

        // ── Dispatch ──
        let result = if let Some(ref rng) = range {
            // If a single line number was provided as the entire command
            // (e.g. `:123`), treat it as a line jump — just like Vim.
            if let CommandRange::SingleLine(n) = rng {
                if cmd_body.is_empty() {
                    // n is 0‑based; move_to_line expects 1‑based
                    self.move_to_line(*n + 1)
                } else {
                    // Range with a command, e.g. `:123d`
                    match self.resolve_command_range(rng) {
                        Ok((start, end)) => self.execute_ranged_command(cmd_body, start, end),
                        Err(e) => CommandResult::Error(e),
                    }
                }
            } else {
                match self.resolve_command_range(rng) {
                    Ok((start, end)) => self.execute_ranged_command(cmd_body, start, end),
                    Err(e) => CommandResult::Error(e),
                }
            }
        } else {
            self.run_command(cmd_body)
        };

        // Clear visual selection after a ranged command
        if range.is_some() {
            if let Some(w) = self.windows.active_window_mut() {
                w.selection_anchor = None;
            }
        }

        self.enter_mode(crate::editor::Mode::Normal);

        // Re-display substitute confirm prompt if active
        // (enter_mode clears messages, but we need the prompt to survive)
        if self.search.substitute_confirm.is_some() {
            if let Some(ref state) = self.search.substitute_confirm {
                self.set_status(format!(
                    "replace with \"{}\"? (y/n/a/q/l)",
                    state.replacement
                ));
            }
            self.dirty.status_cmdline = true;
            self.dirty.status_infobar = true;
        }

        result
    }
    // ────────────────────────────────────────────────────────────────
    // ── run_command — handles non‑ranged commands ───────────────────
    // ────────────────────────────────────────────────────────────────
    fn run_command(&mut self, cmd: &str) -> CommandResult {
        let (name, args) = split_command(cmd);

        // Handle :s without range (operates on current line)
        if name == "s" || name == "substitute" {
            let cur_line = self
                .windows
                .active_window()
                .map(|w| w.cursor.position.line)
                .unwrap_or(0);
            return self.ranged_substitute(args, cur_line, cur_line);
        }

        // Special case: bare line number
        if let Ok(line_num) = cmd.trim().parse::<usize>() {
            return self.move_to_line(line_num);
        }

        // Resolve command name (handles aliases)
        if let Some(canonical) = self.command_registry.resolve(name) {
            if let Some(handler) = self
                .command_registry
                .get(canonical)
                .and_then(|entry| entry.handler)
            {
                return handler(self, args);
            }
        }

        // Fuzzy expansion
        let candidates: Vec<String> = self
            .command_registry
            .prefix_match(name)
            .into_iter()
            .filter(|n| *n != name)
            .filter_map(|n| self.command_registry.resolve(n).map(String::from))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        match candidates.len() {
            0 => CommandResult::Error(format!("Unknown command: {}", cmd)),
            1 => {
                let matched = candidates.into_iter().next().unwrap();
                self.set_status(format!("Expanded '{}' -> '{}'", cmd, matched));
                if let Some(handler) = self
                    .command_registry
                    .get(&matched)
                    .and_then(|entry| entry.handler)
                {
                    handler(self, args)
                } else {
                    CommandResult::Error(format!("Expanded '{}' but no handler", matched))
                }
            }
            _ => CommandResult::Error(format!(
                "Ambiguous command '{}': matches {}",
                cmd,
                candidates
                    .iter()
                    .map(|c| format!(":{}", c))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    // ── :set ────────────────────────────────────────────────────────

    fn handle_set_command(&mut self, setting: &str) -> CommandResult {
        let parts: Vec<&str> = setting.splitn(2, '=').collect();
        let key = parts[0].trim();
        let value = parts.get(1).map(|s| s.trim());

        match key {
            "nu" | "number" | "ruler" => {
                self.config.line_numbers =
                    !value.map(|v| v == "false" || v == "no").unwrap_or(true);
                self.dirty.mark_all();
                CommandResult::Message(format!("line_numbers = {}", self.config.line_numbers))
            }
            "ic" | "ignorecase" => {
                self.config.case_sensitive_search =
                    value.map(|v| v == "false" || v == "no").unwrap_or(false);
                self.dirty.mark_all();
                CommandResult::Message(format!(
                    "case_sensitive_search = {}",
                    self.config.case_sensitive_search
                ))
            }
            "noic" | "noignorecase" => {
                self.config.case_sensitive_search = true;
                self.dirty.mark_all();
                CommandResult::Message(format!(
                    "case_sensitive_search = {}",
                    self.config.case_sensitive_search
                ))
            }
            "tabstop" | "ts" => {
                if let Some(v) = value {
                    if let Ok(n) = v.parse::<u8>() {
                        self.config.tab_width = n.max(1);
                        return CommandResult::Message(format!(
                            "tab_width = {}",
                            self.config.tab_width
                        ));
                    }
                }
                CommandResult::Error("Invalid tab width.".to_string())
            }
            "scrolloff" => {
                if let Some(v) = value {
                    if let Ok(n) = v.parse::<usize>() {
                        self.config.scroll_offset = n;
                        return CommandResult::Message(format!(
                            "scroll_offset = {}",
                            self.config.scroll_offset
                        ));
                    }
                }
                CommandResult::Error("Invalid scroll offset.".to_string())
            }
            _ => CommandResult::Error(format!("Unknown option: {}", key)),
        }
    }

    // ── Command history ─────────────────────────────────────────────

    fn command_history_up(&mut self) -> CommandResult {
        if prompt_history_up(&mut self.command_prompt) {
            self.dirty.mark_all();
            CommandResult::ViewChanged
        } else {
            CommandResult::NoOp
        }
    }

    fn command_history_down(&mut self) -> CommandResult {
        if prompt_history_down(&mut self.command_prompt) {
            self.dirty.mark_all();
            CommandResult::ViewChanged
        } else {
            CommandResult::NoOp
        }
    }

    // ── Search history ──────────────────────────────────────────────

    fn record_search(&mut self, pattern: &str) {
        self.search.prompt.push_history(pattern.to_string());
    }

    fn search_history_up(&mut self) -> CommandResult {
        if prompt_history_up(&mut self.search.prompt) {
            self.dirty.mark_all();
            CommandResult::ViewChanged
        } else {
            CommandResult::NoOp
        }
    }

    fn search_history_down(&mut self) -> CommandResult {
        if prompt_history_down(&mut self.search.prompt) {
            self.dirty.mark_all();
            CommandResult::ViewChanged
        } else {
            CommandResult::NoOp
        }
    }

    // ── Context‑aware dispatch ──────────────────────────────────────

    fn history_up(&mut self) -> CommandResult {
        // With MiniInputPrompt, search mode has its own active block
        // and history is handled there. This method is only called from command mode.
        self.command_history_up()
    }

    fn history_down(&mut self) -> CommandResult {
        self.command_history_down()
    }
}

// ═══════════════════════════════════════════════════════════════════
// ── Range resolution & ranged command execution ───────────────────
// ═══════════════════════════════════════════════════════════════════

impl Editor {
    /// Resolve a `CommandRange` into concrete 0‑based (start, end) line numbers.
    fn resolve_command_range(&self, range: &CommandRange) -> Result<(usize, usize), String> {
        match range {
            CommandRange::VisualSelection => self
                .visual_selection_range
                .ok_or_else(|| "E20: Mark not set".to_string()),
            CommandRange::EntireFile => {
                let lc = self.current_buffer().map(|b| b.line_count()).unwrap_or(0);
                if lc == 0 {
                    Err("Empty buffer".to_string())
                } else {
                    Ok((0, lc - 1))
                }
            }
            CommandRange::Lines { start, end } => Ok((*start, *end)),
            CommandRange::SingleLine(n) => Ok((*n, *n)),
        }
    }

    /// Execute a command on a line range.
    fn execute_ranged_command(&mut self, cmd: &str, start: usize, end: usize) -> CommandResult {
        let (name, args) = split_command(cmd);

        // NEW: Handle bare range like :123 (start == end, no command)
        if name.is_empty() && start == end {
            return self.move_to_line(start + 1);
        }

        match name {
            "d" | "delete" | "dl" | "delete_l" => self.ranged_delete(start, end),
            "y" | "yank" => self.ranged_yank(start, end),
            "s" | "substitute" => self.ranged_substitute(args, start, end),
            ">" | ">>" => self.ranged_indent(start, end),
            "<" | "<<" => self.ranged_dedent(start, end),
            "j" | "join" => self.ranged_join(start, end),
            "w" | "write" => self.ranged_write(args, start, end),
            "normal" | "norm" => self.ranged_normal(args, start, end),
            "t" | "copy" => self.ranged_copy(args, start, end),
            "m" | "move" => self.ranged_move(args, start, end),
            "" => {
                // Bare :'<,'> — just return to normal mode
                CommandResult::NoOp
            }
            _ => {
                // Fall through to registry (ignoring range for unsupported commands)
                if let Some(canonical) = self.command_registry.resolve(name) {
                    if let Some(handler) = self
                        .command_registry
                        .get(canonical)
                        .and_then(|entry| entry.handler)
                    {
                        return handler(self, args);
                    }
                }
                CommandResult::Error(format!("Command :{} does not support a range", name))
            }
        }
    }
    // ════════════════════════════════════════════════════════════════
    // ── Individual ranged commands ──────────────────────────────────
    // ════════════════════════════════════════════════════════════════

    /// `:'<,'>d` — delete lines in range
    fn ranged_delete(&mut self, start: usize, end: usize) -> CommandResult {
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };

        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        let end_clamped = end.min(line_count.saturating_sub(1));

        // Yank first (read‑only pass)
        let mut parts = Vec::new();
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            for line in start..=end_clamped {
                if let Some(text) = buffer.line_text(line) {
                    parts.push(text.trim_end_matches('\n').to_string());
                }
            }
        }
        self.set_yank_register(parts.join("\n"));

        // Delete bottom‑to‑top
        self.ensure_undo_group();
        for line in (start..=end_clamped).rev() {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.delete_line(line);
            }
        }
        self.close_undo_group();
        self.invalidate_git_gutter();

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position.line = start.min(
                self.buffers
                    .get(&buffer_id)
                    .map(|b| b.line_count().saturating_sub(1))
                    .unwrap_or(0),
            );
            w.cursor.position.col = 0;
            w.cursor.desired_col = None;
        }
        CommandResult::ContentChanged
    }

    /// `:'<,'>y` — yank lines in range
    fn ranged_yank(&mut self, start: usize, end: usize) -> CommandResult {
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        let end_clamped = end.min(line_count.saturating_sub(1));

        let mut parts = Vec::new();
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            for line in start..=end_clamped {
                if let Some(text) = buffer.line_text(line) {
                    parts.push(text.trim_end_matches('\n').to_string());
                }
            }
        }
        self.set_yank_register(parts.join("\n"));

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position.line = start;
            w.cursor.position.col = 0;
            w.cursor.desired_col = None;
        }
        CommandResult::Message(format!("Yanked {} lines", end_clamped - start + 1))
    }

    /// `:'<,'>s/pattern/replacement/flags` — substitute within range
    fn ranged_substitute(&mut self, args: &str, start: usize, end: usize) -> CommandResult {
        let (pattern, vim_repl, flags) = match parse_substitute_args(args) {
            Ok(p) => p,
            Err(e) => return CommandResult::Error(e),
        };

        let global = flags.contains('g');
        let icase = flags.contains('i');
        let confirm = flags.contains('c');

        let re = match if icase {
            regex::RegexBuilder::new(&pattern)
                .case_insensitive(true)
                .build()
        } else {
            regex::Regex::new(&pattern)
        } {
            Ok(r) => r,
            Err(e) => return CommandResult::Error(format!("Invalid pattern: {}", e)),
        };

        let regex_repl = vim_replacement_to_regex(&vim_repl);

        // ── Confirm mode: delegate to interactive handler ──
        if confirm {
            return self.start_substitute_confirm(re, regex_repl, global, start, end);
        }

        // ── Non‑confirm mode: original batch behavior ──
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        if line_count == 0 {
            return CommandResult::Error("Empty buffer".into());
        }
        let end_clamped = end.min(line_count - 1);

        self.ensure_undo_group();
        let mut total_subs = 0usize;
        let mut lines_changed = 0usize;

        for line in start..=end_clamped {
            let old_text = if let Some(buffer) = self.buffers.get(&buffer_id) {
                buffer
                    .line_text(line)
                    .unwrap_or_default()
                    .trim_end_matches('\n')
                    .to_string()
            } else {
                break;
            };

            let match_count = if global {
                re.find_iter(&old_text).count()
            } else if re.is_match(&old_text) {
                1
            } else {
                0
            };

            if match_count > 0 {
                let new_text = if global {
                    re.replace_all(&old_text, regex_repl.as_str()).to_string()
                } else {
                    re.replace(&old_text, regex_repl.as_str()).to_string()
                };

                let old_len = old_text.graphemes(true).count();
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    if old_len > 0 {
                        buffer.delete_at(CursorPosition::new(line, 0), old_len);
                    }
                    if !new_text.is_empty() {
                        buffer.insert_at(CursorPosition::new(line, 0), &new_text);
                    }
                    buffer.dirty = true;
                }

                total_subs += match_count;
                lines_changed += 1;
            }
        }

        self.close_undo_group();
        self.invalidate_git_gutter();

        if total_subs > 0 {
            CommandResult::Message(format!(
                "{} substitutions on {} lines",
                total_subs, lines_changed
            ))
        } else {
            CommandResult::Error("Pattern not found".into())
        }
    }
    /// `:'<,'>`  > — indent lines in range
    fn ranged_indent(&mut self, start: usize, end: usize) -> CommandResult {
        let indent_str = if self.config.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.config.tab_width as usize)
        };

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        let end_clamped = end.min(line_count.saturating_sub(1));

        self.ensure_undo_group();
        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            for line in start..=end_clamped {
                buffer.insert_at(CursorPosition::new(line, 0), &indent_str);
            }
            buffer.dirty = true;
        }
        self.close_undo_group();
        self.invalidate_git_gutter();

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position.line = start;
            w.cursor.position.col = 0;
            w.cursor.desired_col = None;
        }
        CommandResult::ContentChanged
    }

    /// `:'<,'>`  < — dedent lines in range
    fn ranged_dedent(&mut self, start: usize, end: usize) -> CommandResult {
        let shiftwidth = self.config.tab_width as usize;
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        let end_clamped = end.min(line_count.saturating_sub(1));

        self.ensure_undo_group();
        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            for line in start..=end_clamped {
                let line_text = buffer.line_text(line).unwrap_or_default();
                let leading: String = line_text
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                if leading.is_empty() {
                    continue;
                }
                let ws_cols: usize = leading
                    .chars()
                    .map(|c| if c == '\t' { shiftwidth } else { 1 })
                    .sum();
                let remove_cols = ws_cols.min(shiftwidth);
                let mut cols_remaining = remove_cols;
                let mut chars_to_remove: usize = 0;
                for c in leading.chars() {
                    let char_cols = if c == '\t' { shiftwidth } else { 1 };
                    if cols_remaining >= char_cols {
                        cols_remaining -= char_cols;
                        chars_to_remove += 1;
                    } else {
                        chars_to_remove += 1;
                        break;
                    }
                    if cols_remaining == 0 {
                        break;
                    }
                }
                if chars_to_remove > 0 {
                    buffer.delete_at(CursorPosition::new(line, 0), chars_to_remove);
                }
            }
            buffer.dirty = true;
        }
        self.close_undo_group();
        self.invalidate_git_gutter();

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position.line = start;
            w.cursor.position.col = 0;
            w.cursor.desired_col = None;
        }
        CommandResult::ContentChanged
    }

    /// `:'<,'>j` — join lines in range
    fn ranged_join(&mut self, start: usize, end: usize) -> CommandResult {
        if start >= end {
            return CommandResult::NoOp;
        }
        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position.line = start;
            w.cursor.position.col = 0;
        }
        self.ensure_undo_group();
        for _ in 0..(end - start) {
            self.join_lines();
        }
        self.close_undo_group();
        CommandResult::ContentChanged
    }

    /// `:'<,'>w {file}` — write range to file
    fn ranged_write(&mut self, args: &str, start: usize, end: usize) -> CommandResult {
        let filename = args.trim();
        if filename.is_empty() {
            return CommandResult::Error("E172: Only one file name allowed".into());
        }
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        let end_clamped = end.min(line_count.saturating_sub(1));

        let mut lines = Vec::new();
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            for line in start..=end_clamped {
                if let Some(text) = buffer.line_text(line) {
                    lines.push(text.trim_end_matches('\n').to_string());
                }
            }
        }
        match std::fs::write(filename, lines.join("\n")) {
            Ok(()) => CommandResult::Message(format!(
                "Wrote {} lines to {}",
                end_clamped - start + 1,
                filename
            )),
            Err(e) => CommandResult::Error(format!("Write failed: {}", e)),
        }
    }

    /// `:'<,'>normal {cmd}` — execute normal‑mode keys on each line (stub)
    fn ranged_normal(&mut self, args: &str, _start: usize, _end: usize) -> CommandResult {
        let keys = args.trim();
        if keys.is_empty() {
            return CommandResult::Error("E471: Argument required".into());
        }
        CommandResult::Error(":'<,'>normal is not yet implemented".into())
    }

    /// `:'<,'>t {n}` — copy lines below line n
    fn ranged_copy(&mut self, args: &str, start: usize, end: usize) -> CommandResult {
        let dest_line = match args.trim().parse::<usize>() {
            Ok(n) => n.saturating_sub(1), // 1‑based → 0‑based
            Err(_) => return CommandResult::Error("Invalid destination line".into()),
        };

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        let end_clamped = end.min(line_count.saturating_sub(1));

        // Collect lines (read‑only pass)
        let mut copied = Vec::new();
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            for line in start..=end_clamped {
                if let Some(text) = buffer.line_text(line) {
                    copied.push(text.trim_end_matches('\n').to_string());
                }
            }
        }

        // Insert at destination top‑down. Each insert_at at col 0 with a
        // trailing newline creates a new line, pushing existing content down.
        // After inserting line i at dest_line + i, the next line goes at
        // dest_line + i + 1, which is correct because the previous insert
        // shifted everything below it by one line.
        self.ensure_undo_group();
        let insert_line = dest_line.min(line_count);
        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            for (i, text) in copied.iter().enumerate() {
                buffer.insert_at(
                    CursorPosition::new(insert_line + i, 0),
                    &format!("{}\n", text),
                );
            }
            buffer.dirty = true;
        }
        self.close_undo_group();
        self.invalidate_git_gutter();

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position.line = insert_line;
            w.cursor.position.col = 0;
            w.cursor.desired_col = None;
        }
        CommandResult::ContentChanged
    }

    /// `:'<,'>m {n}` — move lines below line n
    fn ranged_move(&mut self, args: &str, start: usize, end: usize) -> CommandResult {
        let dest_line = match args.trim().parse::<usize>() {
            Ok(n) => n.saturating_sub(1),
            Err(_) => return CommandResult::Error("Invalid destination line".into()),
        };

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".into()),
        };
        let line_count = self
            .buffers
            .get(&buffer_id)
            .map(|b| b.line_count())
            .unwrap_or(0);
        let end_clamped = end.min(line_count.saturating_sub(1));

        // Collect lines (read‑only pass)
        let mut moved = Vec::new();
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            for line in start..=end_clamped {
                if let Some(text) = buffer.line_text(line) {
                    moved.push(text.trim_end_matches('\n').to_string());
                }
            }
        }

        // Delete original lines (bottom‑up)
        self.ensure_undo_group();
        for line in (start..=end_clamped).rev() {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.delete_line(line);
            }
        }

        // Adjust destination if it was after the deleted range
        let adjusted_dest = if dest_line > end_clamped {
            dest_line - (end_clamped - start + 1)
        } else if dest_line > start {
            start // collapsed range
        } else {
            dest_line
        };

        // Insert at adjusted destination top‑down
        let insert_line = adjusted_dest.min(
            self.buffers
                .get(&buffer_id)
                .map(|b| b.line_count())
                .unwrap_or(0),
        );
        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            for (i, text) in moved.iter().enumerate() {
                buffer.insert_at(
                    CursorPosition::new(insert_line + i, 0),
                    &format!("{}\n", text),
                );
            }
            buffer.dirty = true;
        }
        self.close_undo_group();
        self.invalidate_git_gutter();

        if let Some(w) = self.windows.active_window_mut() {
            w.cursor.position.line = insert_line;
            w.cursor.position.col = 0;
            w.cursor.desired_col = None;
        }
        CommandResult::ContentChanged
    }
}
