//--+ keybind.rs
//! Keybinding management.
//!
//! Maps `Key` events to editor `Action`s. Supports multiple key maps
//! (one per mode) and nested key sequences for commands like `gg`, `dd`, etc.

use crate::action::{camel_to_snake, Action, ActionCategory};
use crate::terminal::Key;
use log::{info, warn};
use std::collections::HashMap;
use std::fmt::Write;

// ── KeyBindingsConfig (for TOML deserialization) ────────────────────

/// Configuration structure for custom keybindings, loaded from TOML.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct KeyBindingsConfig {
    #[serde(default)]
    pub normal: Option<HashMap<String, String>>,
    #[serde(default)]
    pub insert: Option<HashMap<String, String>>,
    #[serde(default)]
    pub visual: Option<HashMap<String, String>>,
    #[serde(default)]
    pub command: Option<HashMap<String, String>>,
    #[serde(default)]
    pub leader: Option<HashMap<String, String>>,
}

// ── Key map ─────────────────────────────────────────────────────────

/// A mapping from a key sequence to an action.
pub type KeySequence = Vec<Key>;

/// A single keybinding entry.
#[derive(Debug, Clone)]
pub struct KeyBinding {
    /// The key sequence that triggers this binding.
    pub keys: KeySequence,
    /// The action to execute.
    pub action: Action,
    /// Optional description for documentation.
    pub description: Option<String>,
}

// ── Key map ─────────────────────────────────────────────────────────

/// A map of key sequences → actions for a specific editor mode.
#[derive(Debug, Clone)]
pub struct KeyMap {
    bindings: HashMap<KeySequence, Action>,
    descriptions: HashMap<KeySequence, String>,
}

impl KeyMap {
    pub fn new() -> Self {
        Self {
            bindings: HashMap::new(),
            descriptions: HashMap::new(),
        }
    }

    /// Add a keybinding.
    pub fn bind(&mut self, keys: KeySequence, action: Action) {
        self.bindings.insert(keys, action);
    }

    /// Add a keybinding with a description.
    pub fn bind_with_desc(&mut self, keys: KeySequence, action: Action, desc: &str) {
        self.bindings.insert(keys.clone(), action);
        self.descriptions.insert(keys, desc.to_string());
    }

    /// Look up the action for a complete key sequence.
    pub fn get(&self, keys: &[Key]) -> Option<&Action> {
        self.bindings.get(keys)
    }

    /// Check if any binding starts with the given prefix (for multi-key sequences).
    pub fn has_prefix(&self, keys: &[Key]) -> bool {
        if self.bindings.contains_key(keys) {
            return true;
        }
        self.bindings
            .keys()
            .any(|seq| seq.len() > keys.len() && seq[..keys.len()] == *keys)
    }

    /// Return all bindings as an iterator.
    pub fn bindings(&self) -> impl Iterator<Item = (&KeySequence, &Action)> {
        self.bindings.iter()
    }

    /// Return the description for a key sequence, if any.
    pub fn description(&self, keys: &[Key]) -> Option<&str> {
        self.descriptions.get(keys).map(|s| s.as_str())
    }

    /// Clear all bindings.
    pub fn clear(&mut self) {
        self.bindings.clear();
        self.descriptions.clear();
    }

    /// Log all registered bindings (for debugging).
    pub fn log_bindings(&self, _mode: &str) {
        for _action in self.bindings.values() {}
    }
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::new()
    }
}

// ── Key bind manager ────────────────────────────────────────────────

/// Manages keybindings across all editor modes.
///
/// Each mode (Normal, Insert, Visual, etc.) has its own key map.
/// Handles partial key sequence accumulation.
#[derive(Debug)]
pub struct KeyBindManager {
    /// Key maps per mode (identified by mode name as string).
    keymaps: HashMap<String, KeyMap>,
    /// Currently accumulated (partial) key sequence.
    pending_keys: KeySequence,
}

impl KeyBindManager {
    pub fn new() -> Self {
        Self {
            keymaps: HashMap::new(),
            pending_keys: Vec::new(),
        }
    }

    /// Register a key map for a mode.
    pub fn register_keymap(&mut self, mode: &str, keymap: KeyMap) {
        self.keymaps.insert(mode.to_string(), keymap);
    }

    /// Get the key map for a mode (if registered).
    pub fn get_keymap(&self, mode: &str) -> Option<&KeyMap> {
        self.keymaps.get(mode)
    }

    /// Get a mutable reference to the key map for a mode.
    pub fn get_keymap_mut(&mut self, mode: &str) -> Option<&mut KeyMap> {
        self.keymaps.get_mut(mode)
    }

    /// Process a key press in the given mode.
    ///
    /// Returns:
    /// - `Some(KeyBindResult::Action(action))` if a complete binding matched.
    /// - `Some(KeyBindResult::Pending)` if more keys are needed.
    /// - `Some(KeyBindResult::NoMatch)` if the sequence doesn't match anything.
    /// - `None` if the mode has no key map.
    pub fn process_key(&mut self, mode: &str, key: Key) -> Option<KeyBindResult> {
        let keymap = self.keymaps.get(mode)?;

        self.pending_keys.push(key.clone());

        // Check for exact match.
        if let Some(action) = keymap.get(&self.pending_keys) {
            self.pending_keys.clear();
            return Some(KeyBindResult::Action(action.clone()));
        }

        // Check if any binding has this as a prefix.
        if keymap.has_prefix(&self.pending_keys) {
            return Some(KeyBindResult::Pending);
        }

        // No match — clear pending keys.
        self.pending_keys.clear();
        Some(KeyBindResult::NoMatch(key))
    }

    /// Clear any pending key sequence (e.g. on mode change).
    pub fn clear_pending(&mut self) {
        self.pending_keys.clear();
    }

    /// Return the current pending keys.
    pub fn pending_keys(&self) -> &[Key] {
        &self.pending_keys
    }

    /// Return possible completions for the currently pending key sequence.
    /// Returns a list of (remaining_keys_display, description) for each binding
    /// that starts with the current pending prefix.
    pub fn which_key(&self, mode: &str) -> Vec<(String, String)> {
        let keymap = match self.keymaps.get(mode) {
            Some(km) => km,
            None => return Vec::new(),
        };
        if self.pending_keys.is_empty() {
            // Show every first-level key from the keymap so the which-key
            // bar always has content (e.g. while a count prefix "3" is
            // being built in the editor layer).
            let mut seen: HashMap<String, String> = HashMap::new();
            for (seq, action) in keymap.bindings() {
                if let Some(first) = seq.first() {
                    let label = first.to_string();
                    seen.entry(label).or_insert_with(|| {
                        keymap
                            .description(seq)
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| action.label())
                    });
                }
            }
            let mut hints: Vec<(String, String)> = seen.into_iter().collect();
            hints.sort_by(|a, b| a.0.cmp(&b.0));
            return hints;
        }
        let mut hints = Vec::new();
        for (seq, action) in keymap.bindings() {
            if seq.len() > self.pending_keys.len()
                && seq[..self.pending_keys.len()] == *self.pending_keys
            {
                let remaining: String = seq[self.pending_keys.len()..]
                    .iter()
                    .map(|k| k.to_string())
                    .collect();

                let desc = keymap
                    .description(seq)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| action.label());
                hints.push((remaining, desc));
            }
        }
        hints.sort_by(|a, b| a.0.cmp(&b.0));
        hints
    }

    /// Bind a key sequence to an action in the given mode.
    /// Creates the keymap for that mode if it doesn't exist yet.
    pub fn bind(&mut self, mode: &str, keys: Vec<Key>, action: Action) {
        if let Some(keymap) = self.keymaps.get_mut(mode) {
            keymap.bind(keys, action);
        } else {
            let mut km = KeyMap::new();
            km.bind(keys, action);
            self.keymaps.insert(mode.to_string(), km);
        }
    }

    // ── Inspection / logging ────────────────────────────────────────

    /// Return a human-readable dump of all registered keybindings.
    pub fn dump_bindings(&self) -> String {
        let mut out = String::new();

        for mode_name in &["normal", "insert", "visual", "command"] {
            let _ = writeln!(out, "╔══ {} ══╗", mode_name.to_uppercase());

            if let Some(keymap) = self.keymaps.get(*mode_name) {
                // Sort by the formatted key string instead of Vec<Key>
                let mut entries: Vec<_> = keymap.bindings.iter().collect();
                entries.sort_by(|a, b| {
                    let a_str = format_key_sequence(a.0);
                    let b_str = format_key_sequence(b.0);
                    a_str.cmp(&b_str)
                });

                for (seq, action) in entries {
                    let key_str = format_key_sequence(seq);
                    let _ = writeln!(out, "  {:<20} → {}", key_str, action.label());
                }
            } else {
                let _ = writeln!(out, "  (empty)");
            }
            let _ = writeln!(out);
        }

        out
    }
    /// Return a summary: mode name → binding count.
    pub fn binding_counts(&self) -> Vec<(&str, usize)> {
        let mut counts = Vec::new();
        for mode_name in &["normal", "insert", "visual", "command"] {
            let count = self
                .keymaps
                .get(*mode_name)
                .map(|km| km.bindings.len())
                .unwrap_or(0);
            counts.push((*mode_name, count));
        }
        counts
    }

    /// Return bindings for a single mode as (key_str, action_label) pairs.
    pub fn bindings_for_mode(&self, mode_name: &str) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        if let Some(keymap) = self.keymaps.get(mode_name) {
            for (seq, action) in keymap.bindings.iter() {
                let key_str = format_key_sequence(seq);
                entries.push((key_str, action.label()));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Generate help entries for a given mode, grouped by category.
    pub fn help_entries(&self, mode: &str) -> Vec<HelpEntry> {
        let keymap = match self.keymaps.get(mode) {
            Some(km) => km,
            None => return Vec::new(),
        };

        let mut entries: Vec<HelpEntry> = Vec::new();
        let mut seen_actions: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (seq, action) in keymap.bindings() {
            let action_key = action.label();

            let keys_display = format_key_sequence(seq);

            if let Some(existing) = entries.iter_mut().find(|e| e.description == action.label()) {
                existing.keys = format!("{}, {}", existing.keys, keys_display);
                continue;
            }

            if seen_actions.contains(&action_key) {
                continue;
            }
            seen_actions.insert(action_key);

            let description = keymap
                .description(seq)
                .map(|s| s.to_string())
                .unwrap_or_else(|| action.label());

            entries.push(HelpEntry {
                keys: keys_display,
                description,
                category: action.category(),
            });
        }

        entries.sort_by(|a, b| {
            a.category
                .cmp(&b.category)
                .then_with(|| a.keys.cmp(&b.keys))
        });

        entries
    }

    /// Generate help entries for multiple modes, merged and deduplicated.
    pub fn help_entries_for_modes(&self, modes: &[&str]) -> Vec<HelpEntry> {
        let mut entries = Vec::new();
        for mode in modes {
            entries.extend(self.help_entries(mode));
        }

        let mut seen = std::collections::HashSet::new();
        entries.retain(|e| seen.insert((e.category, e.description.clone())));

        entries.sort_by(|a, b| {
            a.category
                .cmp(&b.category)
                .then_with(|| a.keys.cmp(&b.keys))
        });

        entries
    }
}

impl Default for KeyBindManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Key bind result ─────────────────────────────────────────────────

/// The result of processing a key in the key bind manager.
#[derive(Debug, Clone)]
pub enum KeyBindResult {
    /// A complete binding matched → execute this action.
    Action(Action),
    /// More keys needed for a multi-key sequence.
    Pending,
    /// No binding matched; the raw key is returned for default handling.
    NoMatch(Key),
}

// ── Help entry ──────────────────────────────────────────────────────

/// A single entry for help display, auto-generated from keybindings.
#[derive(Debug, Clone)]
pub struct HelpEntry {
    /// Key sequence display string (e.g. "gg", "Ctrl-w s", "g h r").
    pub keys: String,
    /// Human-readable description.
    pub description: String,
    /// Semantic category for grouping.
    pub category: ActionCategory,
}

// ── Key formatting ──────────────────────────────────────────────────

/// Format a `Key` value as a human-readable string (for logging/display).
pub fn format_key(key: &Key) -> String {
    match key {
        Key::Char(c) => format!("{}", c),
        Key::Ctrl(c) => format!("Ctrl-{}", c),
        Key::Alt(c) => format!("Alt-{}", c),
        Key::Enter => "Enter".into(),
        Key::Backspace => "BS".into(),
        Key::Delete => "Del".into(),
        Key::Tab => "Tab".into(),
        Key::Escape => "Esc".into(),
        Key::Up => "Up".into(),
        Key::Down => "Down".into(),
        Key::Left => "Left".into(),
        Key::Right => "Right".into(),
        Key::PageUp => "PgUp".into(),
        Key::PageDown => "PgDn".into(),
        Key::Home => "Home".into(),
        Key::End => "End".into(),
        Key::Paste(s) => format!("Paste({} chars)", s.len()),
        Key::F(n) => format!("F{}", n),
        Key::BackTab => "S-Tab".into(),
        _ => format!("{:?}", key),
    }
}

/// Format a key sequence for display.
///
/// Examples:
///   `[Key::Char('g'), Key::Char('g')]` → `"gg"`
///   `[Key::Ctrl('w'), Key::Char('s')]` → `"Ctrl-w s"`
///   `[Key::Char('d'), Key::Char('d')]` → `"dd"`
///   `[Key::Char('g'), Key::Char('h'), Key::Char('r')]` → `"g h r"`
fn format_key_sequence(seq: &[Key]) -> String {
    seq.iter()
        .map(|k| k.to_string())
        .collect::<Vec<_>>()
        .join(" ")
        .replace("g g", "gg")
        .replace("d d", "dd")
        .replace("y y", "yy")
        .replace("z z", "zz")
}

// ── Default keymaps ─────────────────────────────────────────────────

#[rustfmt::skip]
/// Create the default Normal mode key map.
pub fn default_normal_keymap() -> KeyMap {
    let mut km = KeyMap::new();

    // Movement
    km.bind_with_desc(vec![Key::Up], Action::MoveUp, "Move up");
    km.bind_with_desc(vec![Key::Down], Action::MoveDown, "Move down");
    km.bind_with_desc(vec![Key::Left], Action::MoveLeft, "Move left");
    km.bind_with_desc(vec![Key::Right], Action::MoveRight, "Move right");
    km.bind_with_desc(vec![Key::Home], Action::MoveLineStart, "Move to line start");
    km.bind_with_desc(vec![Key::End], Action::MoveLineEnd, "Move to line end");
    km.bind_with_desc(vec![Key::Char('h')], Action::MoveLeft, "Move left");
    km.bind_with_desc(vec![Key::Char('j')], Action::MoveDown, "Move down");
    // km.bind_with_desc(vec![Key::Char('k')], Action::MoveUp, "Move up");
    km.bind_with_desc(vec![Key::Char('l')], Action::MoveRight, "Move right");
    km.bind_with_desc(vec![Key::Char('w')], Action::MoveWordForward, "Move word forward");
    km.bind_with_desc(vec![Key::Char('b')], Action::MoveWordBack, "Move word back");
    km.bind_with_desc(vec![Key::Char('e')], Action::MoveWordEnd, "Move to end of word");
    km.bind_with_desc(vec![Key::Char('0')], Action::MoveLineStart, "Move to line start");
    km.bind_with_desc(vec![Key::Char('^')], Action::MoveLineStart, "Move to line start");
    km.bind_with_desc(vec![Key::Char('$')], Action::MoveLineEnd, "Move to line end");
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('g')], Action::MoveFileStart, "Go to first line");
    km.bind_with_desc(vec![Key::Char('*')], Action::SearchWordForward, "Search word forward");
    km.bind_with_desc(vec![Key::Char('G')], Action::MoveFileEnd, "Go to last line");
    km.bind_with_desc(vec![Key::Char('%')], Action::MatchBracket, "MatchBracket");
    km.bind_with_desc(vec![Key::Char('.')], Action::RepeatLastAction, "Repeat last change");
    km.bind_with_desc(vec![Key::Ctrl('a')], Action::MoveLineStart, "Move to line start");
    km.bind_with_desc(vec![Key::Ctrl('e')], Action::MoveLineEnd, "Move to line end");

    // Scrolling
    km.bind_with_desc(vec![Key::Ctrl('u')], Action::ScrollUp, "Scroll up half page");
    km.bind_with_desc(vec![Key::Ctrl('d')], Action::ScrollDown, "Scroll down half page");
    km.bind_with_desc(vec![Key::PageUp], Action::PageUp, "Page up");
    km.bind_with_desc(vec![Key::PageDown], Action::PageDown, "Page down");
    km.bind_with_desc(vec![Key::Char('z'), Key::Char('z')], Action::ScrollCenter, "Center cursor on screen");

    // Editing
    km.bind_with_desc(vec![Key::Char('i')], Action::EnterInsertMode, "Enter insert mode");
    km.bind_with_desc(vec![Key::Char('a')], Action::EnterAppendMode, "Enter append mode");
    km.bind_with_desc(vec![Key::Char('I')], Action::EnterInsertLineStart, "Insert at line start");
    km.bind_with_desc(vec![Key::Char('A')], Action::EnterAppendLineEnd, "Append at line end");
    km.bind_with_desc(vec![Key::Char('o')], Action::OpenLineBelow, "Open line below");
    km.bind_with_desc(vec![Key::Char('O')], Action::OpenLineAbove, "Open line above");
    km.bind_with_desc(vec![Key::Char('x')], Action::DeleteChar, "Delete character");
    km.bind_with_desc(vec![Key::Delete], Action::DeleteChar, "Delete character");
    km.bind_with_desc(vec![Key::Char('r')], Action::ReplaceChar, "Replace character");
    km.bind_with_desc(vec![Key::Char('d'), Key::Char('d')], Action::DeleteLine, "Delete line");
    km.bind_with_desc(vec![Key::Char('d'), Key::Char('w')], Action::DeleteWordForward, "Delete word");
    km.bind_with_desc(vec![Key::Char('d'), Key::Char('$')], Action::DeleteToLineEnd, "Del EOL");
    km.bind_with_desc(vec![Key::Char('d'), Key::Char('G')], Action::DeleteToFileEnd, "Del EOF");
    km.bind_with_desc(vec![Key::Char('d'), Key::Char('a'), Key::Char('f')], Action::DeleteAroundFunction, "Delete around function");
    km.bind_with_desc(vec![Key::Char('y'), Key::Char('y')], Action::YankLine, "Yank (copy) line");
    km.bind_with_desc(vec![Key::Char('p')], Action::PasteAfter, "Paste after cursor");
    km.bind_with_desc(vec![Key::Char('P')], Action::PasteBefore, "Paste before cursor");
    km.bind_with_desc(vec![Key::Char('u')], Action::Undo, "Undo");
    km.bind_with_desc(vec![Key::Ctrl('r')], Action::Redo, "Redo");

    // Mode changes
    km.bind_with_desc(vec![Key::Char('v')], Action::EnterVisualMode, "Enter visual mode");
    km.bind_with_desc(vec![Key::Char('V')], Action::EnterVisualLineMode, "Enter visual line mode");
    km.bind_with_desc(vec![Key::Ctrl('v')], Action::EnterVisualBlockMode, "Enter visual block mode");
    km.bind_with_desc(vec![Key::Char('R')], Action::EnterReplaceMode, "Enter replace mode");
    km.bind_with_desc(vec![Key::Char(':')], Action::EnterCommandMode, "Enter command mode");

    // Indent / Dedent
    km.bind_with_desc(vec![Key::Char('>')], Action::Indent, "Indent line (re-indent)");
    km.bind_with_desc(vec![Key::Char('<')], Action::Dedent, "Dedent line (un-indent)");
    km.bind_with_desc(vec![Key::Char('='), Key::Char('=')], Action::IndentTs, "Smart indent (function body or line)");
    km.bind_with_desc(vec![Key::Char('='), Key::Char('G')], Action::IndentTsToFileEnd, "Indent to end of file");

    // Join lines
    km.bind_with_desc(vec![Key::Char('J')], Action::JoinLines, "Join line below to current");

    // Register prefix
    km.bind_with_desc(vec![Key::Char('"')], Action::RegisterPrefix, "Register prefix (\"x)");
    
    // Marks / Bookmarks
    km.bind_with_desc(vec![Key::Char('m')], Action::SetMark, "Set mark (mX)");
    km.bind_with_desc(vec![Key::Char('`')], Action::GotoMark, "Goto mark (`X) / Jump back (``)");

    // Tags (ctags)
    km.bind_with_desc(vec![Key::Ctrl('5')], Action::TagJump, "Jump to tag under cursor");
    km.bind_with_desc(vec![Key::Ctrl('t')], Action::TagPop, "Pop tag stack");

    // Search
    km.bind_with_desc(vec![Key::Char('/')], Action::SearchForward, "Search forward");
    km.bind_with_desc(vec![Key::Char('?')], Action::SearchBackward, "Search backward");
    km.bind_with_desc(vec![Key::Char('n')], Action::SearchNext, "Next search result");
    km.bind_with_desc(vec![Key::Char('N')], Action::SearchPrev, "Previous search result");

    // File operations
    km.bind_with_desc(vec![Key::Ctrl('s')], Action::Save, "Save file");
    km.bind_with_desc(vec![Key::Ctrl('w'), Key::Char('n')], Action::NewFile, "New file");
    km.bind_with_desc(vec![Key::Ctrl('p')], Action::FindFile, "Find file");

    // Window management
    km.bind_with_desc(vec![Key::Ctrl('w'), Key::Char('s')], Action::SplitHorizontal, "Horizontal split");
    km.bind_with_desc(vec![Key::Ctrl('w'), Key::Char('v')], Action::SplitVertical, "Vertical split");
    km.bind_with_desc(vec![Key::Ctrl('w'), Key::Char('w')], Action::NextWindow, "Next window");
    km.bind_with_desc(vec![Key::Tab], Action::NextWindow, "Next window");
    km.bind_with_desc(vec![Key::Ctrl('w'), Key::Char('q')], Action::CloseWindow, "Close window");

    // Quit
    km.bind_with_desc(vec![Key::Char('Z'), Key::Char('Z')], Action::Quit, "Quit");
    km.bind_with_desc(vec![Key::Char('Z'), Key::Char('Q')], Action::ForceQuit, "Force quit");

    // Git hunk navigation
    km.bind_with_desc(vec![Key::Char(']'), Key::Char('h')], Action::GitNextHunk, "Next git hunk");
    km.bind_with_desc(vec![Key::Char('['), Key::Char('h')], Action::GitPrevHunk, "Previous git hunk");
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('d')], Action::GotoDefinition, "Goto definition");
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('h'), Key::Char('r')], Action::GitRevertHunk, "Revert git hunk");
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('h'), Key::Char('g')], Action::GitGutterToggle, "Toggle git gutter");
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('r'), Key::Char('g')], Action::RipgrepUnderCursor, "Ripgrep word under cursor");
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('r'), Key::Char('i')], Action::RipgrepInput, "Ripgrep with input");

    // Tools
    km.bind_with_desc(vec![Key::Char('k')], Action::RipgrepUnderCursor, "Ripgrep word under cursor");
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('a'), Key::Char('i')], Action::LlmOpen, "Open LLM chat");
    km.bind_with_desc(vec![Key::Char('\'')], Action::LlmQuickPrompt, "LLM Prompt");

    // Comments
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('c'), Key::Char('c')], Action::ToggleComment, "Toggle comment on line");
    km.bind_with_desc(vec![Key::Char('g'), Key::Char('c'), Key::Char('j')], Action::ToggleCommentAndMoveDown, "Toggle comment and move down");

    km
}

#[rustfmt::skip]
/// Create the default Insert mode key map.
pub fn default_insert_keymap() -> KeyMap {
    let mut km = KeyMap::new();

    km.bind_with_desc(vec![Key::Escape], Action::EnterNormalMode, "Return to normal mode");
    km.bind_with_desc(vec![Key::Backspace], Action::Backspace, "Delete previous character");
    km.bind_with_desc(vec![Key::Delete], Action::DeleteCharForward, "Delete next character");
    km.bind_with_desc(vec![Key::Enter], Action::InsertNewline, "Insert newline");
    km.bind_with_desc(vec![Key::Ctrl('w')], Action::DeleteWord, "Delete word before cursor");
    km.bind_with_desc(vec![Key::Ctrl('u')], Action::DeleteToLineStart, "Delete to line start");
    km.bind_with_desc(vec![Key::Ctrl('s')], Action::Save, "Save file");
    // km.bind_with_desc(vec![Key::Ctrl('r')], Action::Register, "Register paste");
    km.bind_with_desc(vec![Key::Ctrl('r')], Action::InsertRegisterPrefix, "Insert register contents");
    km.bind_with_desc(vec![Key::Up], Action::MoveUp, "Move up");
    km.bind_with_desc(vec![Key::Down], Action::MoveDown, "Move down");
    km.bind_with_desc(vec![Key::Left], Action::MoveLeft, "Move left");
    km.bind_with_desc(vec![Key::Right], Action::MoveRight, "Move right");
    km.bind_with_desc(vec![Key::Home], Action::MoveLineStart, "Move to line start");
    km.bind_with_desc(vec![Key::End], Action::MoveLineEnd, "Move to line end");
    km.bind_with_desc(vec![Key::PageUp], Action::PageUp, "Scroll up");
    km.bind_with_desc(vec![Key::PageDown], Action::PageDown, "Scroll down");

    // Completion keybindings
    km.bind_with_desc(vec![Key::Tab], Action::ConfirmCompletion, "Confirm completion or insert tab");
    km.bind_with_desc(vec![Key::BackTab], Action::SelectPrevCompletion, "Previous command completion");
    km.bind_with_desc(vec![Key::Ctrl(' ')], Action::TriggerCompletion, "Trigger completion");
    km.bind_with_desc(vec![Key::Ctrl('e')], Action::CancelCompletion, "Cancel completion");
    km.bind_with_desc(vec![Key::Alt('/')], Action::TriggerCodeiumCompletion, "Codeium AI completion");

    // Undo break: Ctrl-G u in insert mode (Vim compatible).
    km.bind_with_desc(vec![Key::Ctrl('g'), Key::Char('u')], Action::UndoBreak, "Undo break (start new undo sequence)");

    km
}

#[rustfmt::skip]
/// Create the default Visual mode key map.
pub fn default_visual_keymap() -> KeyMap {
    let mut km = default_normal_keymap();

    // Override some bindings for visual mode.
    km.bind_with_desc(vec![Key::Escape], Action::EnterNormalMode, "Return to normal mode");
    km.bind_with_desc(vec![Key::Char('x')], Action::DeleteSelection, "Delete selection");
    km.bind_with_desc(vec![Key::Char('d')], Action::DeleteSelection, "Delete selection");
    km.bind_with_desc(vec![Key::Char('y')], Action::YankSelection, "Yank (copy) selection");
    km.bind_with_desc(vec![Key::Char('c')], Action::ChangeSelection, "Change selection");

    // Visual block operations
    km.bind_with_desc(vec![Key::Char('I')], Action::BlockInsert, "Block insert at left edge");
    km.bind_with_desc(vec![Key::Char('A')], Action::BlockAppend, "Block append at right edge");
    km.bind_with_desc(vec![Key::Char('o')], Action::SwapSelectionAnchor, "Swap anchor and cursor");

    // v / V / Ctrl-v to switch between visual submodes
    km.bind_with_desc(vec![Key::Char('v')], Action::EnterVisualMode, "Switch to visual mode");
    km.bind_with_desc(vec![Key::Char('V')], Action::EnterVisualLineMode, "Switch to visual line mode");
    km.bind_with_desc(vec![Key::Ctrl('v')], Action::EnterVisualBlockMode, "Switch to visual block mode");

    // LLM quick prompt with selection
    km.bind_with_desc(vec![Key::Char('\'')], Action::LlmQuickPrompt, "LLM Prompt (selection → ##TODO)");

    km
}

#[rustfmt::skip]
/// Create the default Command mode key map.
pub fn default_command_keymap() -> KeyMap {
    let mut km = KeyMap::new();

    km.bind_with_desc(vec![Key::Enter], Action::ExecuteCommand, "Execute command");
    km.bind_with_desc(vec![Key::Escape], Action::EnterNormalMode, "Cancel command");
    km.bind_with_desc(vec![Key::Up], Action::CommandHistoryUp, "Previous command");
    km.bind_with_desc(vec![Key::Down], Action::CommandHistoryDown, "Next command");
    km.bind_with_desc(vec![Key::Backspace], Action::Backspace, "Delete last char");
    km.bind_with_desc(vec![Key::Ctrl('c')], Action::EnterNormalMode, "Cancel command");

    km
}

#[rustfmt::skip]
/// Create the default LLM buffer key map.
pub fn default_llm_keymap() -> KeyMap {
    let mut km = KeyMap::new();

    // Input navigation
    km.bind_with_desc(vec![Key::Left], Action::MoveLeft, "Cursor left");
    km.bind_with_desc(vec![Key::Right], Action::MoveRight, "Cursor right");
    km.bind_with_desc(vec![Key::Home], Action::MoveLineStart, "Cursor to start");
    km.bind_with_desc(vec![Key::End], Action::MoveLineEnd, "Cursor to end");
    km.bind_with_desc(vec![Key::Ctrl('a')], Action::MoveLineStart, "Cursor to start");
    km.bind_with_desc(vec![Key::Ctrl('e')], Action::MoveLineEnd, "Cursor to end");

    // Editing
    km.bind_with_desc(vec![Key::Backspace], Action::Backspace, "Delete before cursor");
    km.bind_with_desc(vec![Key::Delete], Action::DeleteCharForward, "Delete after cursor");
    km.bind_with_desc(vec![Key::Ctrl('w')], Action::DeleteWord, "Delete word before");
    km.bind_with_desc(vec![Key::Ctrl('u')], Action::DeleteToLineStart, "Delete to start");

    // Send message
    km.bind_with_desc(vec![Key::Enter], Action::LlmSend, "Send message");

    // Cancel current request
    km.bind_with_desc(vec![Key::Ctrl('c')], Action::LlmCancel, "Cancel request");

    // History navigation
    km.bind_with_desc(vec![Key::Up], Action::CommandHistoryUp, "Previous input");
    km.bind_with_desc(vec![Key::Down], Action::CommandHistoryDown, "Next input");

    // Exit LLM buffer
    km.bind_with_desc(vec![Key::Escape], Action::LlmClose, "Close LLM buffer");
    km.bind_with_desc(vec![Key::Ctrl(']')], Action::LlmClose, "Close LLM buffer");

    // Preset switching / completion
    km.bind_with_desc(vec![Key::Tab], Action::ConfirmCompletion, "Confirm completion or insert tab");
    km.bind_with_desc(vec![Key::BackTab], Action::LlmPrevPreset, "Previous preset");

    // Clear history
    km.bind_with_desc(vec![Key::Ctrl('l')], Action::LlmClearHistory, "Clear history");

    // Scroll conversation
    km.bind_with_desc(vec![Key::PageUp], Action::PageUp, "Scroll up");
    km.bind_with_desc(vec![Key::PageDown], Action::PageDown, "Scroll down");
    km.bind_with_desc(vec![Key::Ctrl('b')], Action::PageUp, "Scroll up");
    km.bind_with_desc(vec![Key::Ctrl('f')], Action::PageDown, "Scroll down");

    km
}

#[rustfmt::skip]
/// Create the default quick prompt key map.
pub fn default_quick_prompt_keymap() -> KeyMap {
    let mut km = KeyMap::new();

    km.bind_with_desc(vec![Key::Enter], Action::LlmSend, "Send prompt");
    km.bind_with_desc(vec![Key::Escape], Action::EnterNormalMode, "Cancel");
    km.bind_with_desc(vec![Key::Ctrl('c')], Action::EnterNormalMode, "Cancel");
    km.bind_with_desc(vec![Key::Backspace], Action::Backspace, "Delete char");
    km.bind_with_desc(vec![Key::Left], Action::MoveLeft, "Cursor left");
    km.bind_with_desc(vec![Key::Right], Action::MoveRight, "Cursor right");
    km.bind_with_desc(vec![Key::Home], Action::MoveLineStart, "Cursor home");
    km.bind_with_desc(vec![Key::End], Action::MoveLineEnd, "Cursor end");
    km.bind_with_desc(vec![Key::Ctrl('a')], Action::MoveLineStart, "Cursor home");
    km.bind_with_desc(vec![Key::Ctrl('e')], Action::MoveLineEnd, "Cursor end");
    km.bind_with_desc(vec![Key::Ctrl('w')], Action::DeleteWord, "Delete word");
    km.bind_with_desc(vec![Key::Ctrl('u')], Action::DeleteToLineStart, "Delete to start");
    km.bind_with_desc(vec![Key::Up], Action::CommandHistoryUp, "History up");
    km.bind_with_desc(vec![Key::Down], Action::CommandHistoryDown, "History down");

    km
}

// ── Key string parser (for config file) ─────────────────────────────

#[rustfmt::skip]
/// Parse a key specification string (from config TOML) into a `Key`.
/// Supports: "a", "A", "ctrl-a", "alt-a", "ctrl-q", "alt-q", "enter",
/// "escape", "tab", "backspace", "delete", "up", "down", "left", "right",
/// "home", "end", "pageup", "pagedown", "f1"–"f12", "space".
/// Case is preserved for character keys: "L" → Key::Char('L'), "l" → Key::Char('l').
pub fn parse_key_str(s: &str) -> Option<Key> {
    let s = s.trim();
    let s_lower = s.to_lowercase();

    // Handle <...> angle bracket notation: <ctrl-s>, <alt-q>, <minus>, etc.
    if s.starts_with('<') && s.ends_with('>') && s.len() >= 3 {
        let inner = &s[1..s.len() - 1];
        return parse_angle_key(inner);
    }

    // Bare special key names (no angle brackets) — case-insensitive.
    match s_lower.as_str() {
        "enter" | "return" => return Some(Key::Enter),
        "escape" | "esc" => return Some(Key::Escape),
        "tab" => return Some(Key::Tab),
        "backspace" | "bs" => return Some(Key::Backspace),
        "delete" | "del" => return Some(Key::Delete),
        "up" => return Some(Key::Up),
        "down" => return Some(Key::Down),
        "left" => return Some(Key::Left),
        "right" => return Some(Key::Right),
        "home" => return Some(Key::Home),
        "end" => return Some(Key::End),
        "pageup" | "pgup" => return Some(Key::PageUp),
        "pagedown" | "pgdn" => return Some(Key::PageDown),
        "space" => return Some(Key::Char(' ')),
        "insert" | "ins" => return Some(Key::Insert),
        _ => {}
    }

    // Bare modifier prefixes (backwards compat): ctrl-x, alt-x, c-x, a-x
    if let Some(rest) = s_lower.strip_prefix("ctrl-") {
        return Some(Key::Ctrl(resolve_special_char(rest)?));
    } else if let Some(rest) = s_lower.strip_prefix("alt-") {
        return Some(Key::Alt(resolve_special_char(rest)?));
    } else if let Some(rest) = s_lower.strip_prefix("a-") {
        return Some(Key::Alt(resolve_special_char(rest)?));
    } else if let Some(rest) = s_lower.strip_prefix("c-") {
        return Some(Key::Ctrl(resolve_special_char(rest)?));
    } else if let Some(rest) = s_lower.strip_prefix('f') {
        let n: u8 = rest.parse().ok()?;
        if (1..=12).contains(&n) {
            return Some(Key::F(n));
        }
        return None;
    }

    // Single character (bare letter, digit, symbol) — preserve ORIGINAL case.
    Some(Key::Char(s.chars().next()?))
}

/// Parse the content inside angle brackets (without `<` and `>`).
fn parse_angle_key(inner: &str) -> Option<Key> {
    let inner_lower = inner.to_lowercase();

    match inner_lower.as_str() {
        "enter" | "return" => return Some(Key::Enter),
        "escape" | "esc" => return Some(Key::Escape),
        "tab" => return Some(Key::Tab),
        "backspace" | "bs" => return Some(Key::Backspace),
        "delete" | "del" => return Some(Key::Delete),
        "up" => return Some(Key::Up),
        "down" => return Some(Key::Down),
        "left" => return Some(Key::Left),
        "right" => return Some(Key::Right),
        "home" => return Some(Key::Home),
        "end" => return Some(Key::End),
        "pageup" | "pgup" => return Some(Key::PageUp),
        "pagedown" | "pgdn" => return Some(Key::PageDown),
        "space" => return Some(Key::Char(' ')),
        "insert" | "ins" => return Some(Key::Insert),
        _ => {}
    }

    // F-keys
    if let Some(rest) = inner_lower.strip_prefix('f') {
        let n: u8 = rest.parse().ok()?;
        if (1..=12).contains(&n) {
            return Some(Key::F(n));
        }
    }

    // Modifier + key
    if let Some(rest) = inner_lower.strip_prefix("ctrl-") {
        return Some(Key::Ctrl(resolve_special_char(rest)?));
    }
    if let Some(rest) = inner_lower.strip_prefix("alt-") {
        return Some(Key::Alt(resolve_special_char(rest)?));
    }
    if let Some(rest) = inner_lower.strip_prefix("shift-") {
        let c = resolve_special_char(rest)?;
        return Some(Key::Char(c.to_ascii_uppercase()));
    }
    if let Some(rest) = inner_lower.strip_prefix("c-") {
        return Some(Key::Ctrl(resolve_special_char(rest)?));
    }
    if let Some(rest) = inner_lower.strip_prefix("a-") {
        return Some(Key::Alt(resolve_special_char(rest)?));
    }
    if let Some(rest) = inner_lower.strip_prefix("s-") {
        let c = resolve_special_char(rest)?;
        return Some(Key::Char(c.to_ascii_uppercase()));
    }

    // Special character names or bare character (preserving case)
    resolve_special_char(inner).map(Key::Char)
}

/// Resolve a key name to a character.
fn resolve_special_char(s: &str) -> Option<char> {
    match s.to_lowercase().as_str() {
        "minus" | "hyphen" => Some('-'),
        "plus" => Some('+'),
        "equal" | "equals" => Some('='),
        "left-bracket" | "lbracket" => Some('['),
        "right-bracket" | "rbracket" => Some(']'),
        "left-curly" | "lbrace" => Some('{'),
        "right-curly" | "rbrace" => Some('}'),
        "left-paren" | "lparen" => Some('('),
        "right-paren" | "rparen" => Some(')'),
        "backslash" => Some('\\'),
        "slash" | "forward-slash" => Some('/'),
        "pipe" => Some('|'),
        "tilde" => Some('~'),
        "grave" | "backtick" => Some('`'),
        "at" => Some('@'),
        "hash" | "pound" => Some('#'),
        "dollar" => Some('$'),
        "percent" => Some('%'),
        "caret" | "circumflex" => Some('^'),
        "ampersand" => Some('&'),
        "asterisk" | "star" => Some('*'),
        "underscore" => Some('_'),
        "comma" => Some(','),
        "period" | "dot" => Some('.'),
        "semicolon" => Some(';'),
        "colon" => Some(':'),
        "quote" | "double-quote" => Some('"'),
        "apostrophe" | "single-quote" => Some('\''),
        "space" => Some(' '),
        "nul" | "null" => Some('\0'),
        "lt" | "less-than" => Some('<'),
        "gt" | "greater-than" => Some('>'),
        "question-mark" => Some('?'),
        "exclamation" | "bang" => Some('!'),
        _ => s.chars().next(),
    }
}

// ── Leader key support ─────────────────────────────────────────────

/// Parse a key sequence string that may contain `<leader>` as a placeholder.
pub fn parse_key_sequence_with_leader(s: &str, leader: char) -> Option<Vec<Key>> {
    let leader_tag = "<leader>";
    let tag_len = leader_tag.len();
    let bytes = s.as_bytes();
    let mut keys: Vec<Key> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        // Check for <leader> (case-insensitive).
        if i + tag_len <= bytes.len()
            && s[i..].as_bytes()[..tag_len].eq_ignore_ascii_case(leader_tag.as_bytes())
        {
            keys.push(Key::Char(leader));
            i += tag_len;
            continue;
        }

        if bytes[i] == b'<' {
            // Extract <...> as an explicit key token.
            let end = bytes[i + 1..].iter().position(|&b| b == b'>')?;
            let token = &s[i..i + end + 2];
            if token.len() < 3 {
                return None;
            }
            keys.push(parse_key_str(token)?);
            i += end + 2;
        } else if bytes[i].is_ascii_whitespace() {
            // If the leader is a whitespace char (space), treat it as a key
            // instead of skipping it. Only skip whitespace that isn't the leader.
            if leader.is_ascii_whitespace() && bytes[i] == leader as u8 {
                keys.push(Key::Char(leader));
            }
            i += 1;
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let bare = &s[start..i];
            if bare.contains('-') {
                return None;
            }
            for ch in bare.chars() {
                keys.push(parse_key_str(&ch.to_string())?);
            }
        }
    }

    if keys.is_empty() {
        None
    } else {
        Some(keys)
    }
}

/// Bind a leader-prefixed key sequence to an action.
pub fn bind_leader(km: &mut KeyMap, seq_str: &str, leader: char, action: Action, desc: &str) {
    let leader_str = if leader == ' ' {
        "<space>".to_string()
    } else {
        leader.to_string()
    };
    let full_str = format!("{}{}", leader_str, seq_str);
    if let Some(keys) = parse_key_sequence_with_leader(&full_str, leader) {
        km.bind_with_desc(keys, action, desc);
    }
}

/// Parse a key sequence from config TOML.
///
/// Rules:
/// - `<...>` tokens are parsed as explicit keys (e.g. `<ctrl-s>`, `<alt-q>`, `<leader>`).
/// - Bare characters between `<...>` tokens are split into individual keys
///   (e.g. `"dgg"` → `[d, g, g]`, `"gcc"` → `[g, c, c]`).
/// - Bare text containing `-` is **rejected** — use angle notation instead.
/// - Single bare characters are always accepted (e.g. `<leader>pp` → leader + p + p).
pub fn parse_key_sequence(s: &str) -> Option<Vec<Key>> {
    let mut tokens: Vec<String> = Vec::new();
    let mut i = 0;
    let bytes = s.as_bytes();

    while i < bytes.len() {
        if bytes[i] == b'<' {
            let end = bytes[i + 1..].iter().position(|&b| b == b'>')?;
            let token = &s[i..i + end + 2];
            if token.len() < 3 {
                return None;
            }
            tokens.push(token.to_string());
            i += end + 2;
        } else if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let bare = &s[start..i];
            if bare.contains('-') {
                return None;
            }
            for ch in bare.chars() {
                tokens.push(ch.to_string());
            }
        }
    }

    if tokens.is_empty() {
        return None;
    }

    let mut keys = Vec::new();
    for token in &tokens {
        match parse_key_str(token) {
            Some(k) => keys.push(k),
            None => return None,
        }
    }
    Some(keys)
}

// ── Action name parser (for config file) ────────────────────────────

#[rustfmt::skip]
/// Parse an action name string (from config) into an `Action`.
pub fn parse_action_str(s: &str) -> Option<Action> {
    let s = s.trim();

    macro_rules! match_action {
        ($($variant:ident $(| $alias:literal)*),* $(,)?) => {
            $(
                if s == camel_to_snake(stringify!($variant)) $( || s == $alias )* {
                    return Some(Action::$variant);
                }
            )*
        };
    }

    match_action! {
        // ── Movement ───────────────────────────────────────
        MoveLeft | "left",
        MoveRight | "right",
        MoveUp | "up",
        MoveDown | "down",
        MoveWordForward | "w",
        MoveWordBack | "b",
        MoveWordEnd | "e",
        MoveLineStart | "0",
        MoveLineEnd | "$",
        MoveFileStart | "gg",
        MoveFileEnd | "G",
        MatchBracket | "%",
        SetMark | "m",
        GotoMark | "backtick",
        JumpBack | "jump_back" | "``",
        // ── Scrolling ──────────────────────────────────────
        ScrollUp | "ctrl-u",
        ScrollDown | "ctrl-d",
        ScrollLeft,
        ScrollRight,
        PageUp,
        PageDown,
        ScrollCenter | "zz",

        // ── Editing ────────────────────────────────────────
        Backspace,
        DeleteChar | "x",
        DeleteCharForward,
        DeleteWord | "ctrl-w",
        DeleteWordForward,
        DeleteLine | "dd",
        DeleteToLineEnd | "D",
        DeleteToLineStart,
        DeleteSelection,
        ChangeSelection,
        ReplaceChar | "r",
        JoinLines | "J",
        Indent,
        Dedent,
        IndentSelection,
        DedentSelection,
        ToggleComment | "gcc",
        ToggleCommentAndMoveDown | "gcj",
        DeleteAroundFunction | "daf",
        IndentTs | "fix_indent" | "==",
        IndentTsToFileEnd | "=G",
        BlockInsert,
        BlockAppend,
        SwapSelectionAnchor | "o",
        Register | "ctrl-r",
        InsertRegisterPrefix | "insert-register",

        // ── Insert ─────────────────────────────────────────
        InsertNewline | "enter",
        InsertTab | "tab",
        OpenLineAbove | "O",
        OpenLineBelow,

        // ── Mode changes ───────────────────────────────────
        EnterNormalMode | "escape",
        EnterInsertMode | "insert" | "i",
        EnterAppendMode | "a",
        EnterInsertLineStart | "I",
        EnterAppendLineEnd | "A",
        EnterReplaceMode | "R",
        EnterVisualMode | "v",
        EnterVisualLineMode | "V",
        EnterVisualBlockMode | "ctrl-v",
        EnterCommandMode | ":",

        // ── Yank / Paste ───────────────────────────────────
        YankLine | "yy",
        YankSelection,
        PasteAfter | "p",
        PasteBefore | "P",
        YankToClipboard,
        PasteFromClipboard,
        ClipboardPasteLine,
        ClipboardReplaceBuffer,

        // ── Undo / Redo ────────────────────────────────────
        Undo | "u",
        Redo | "ctrl-r",
        UndoBreak,

        // ── Search ─────────────────────────────────────────
        SearchForward | "/",
        SearchBackward | "?",
        SearchNext | "n",
        SearchPrev | "N",
        SearchWordForward | "*",
        SearchWordBackward | "#",
        ReplaceMode,
        ReplaceAll,

        // ── File operations ────────────────────────────────
        Save | "w" | "ctrl-s",
        SaveFmt | "x",
        NewFile,
        CloseFile,
        OpenMru,
        FindFile | "filepicker" | "ctrl-p",
        DeleteBuffer | "bd",

        // ── Window management ──────────────────────────────
        SplitHorizontal | "sp",
        SplitVertical | "vs",
        NextWindow,
        PrevWindow,
        CloseWindow,
        ZoomWindow,
        EqualizeWindows,
        SwapWindowLeft,
        SwapWindowRight,

        // ── Command line ───────────────────────────────────
        ExecuteCommand,
        CommandHistoryUp,
        CommandHistoryDown,

        // ── LSP ────────────────────────────────────────────
        GotoDefinition,
        GotoDeclaration,
        GotoImplementation,
        GotoTypeDefinition,
        FindReferences,
        RenameSymbol,
        HoverInfo,
        CodeAction,
        FormatDocument | "format",
        SignatureHelp,
        Diagnostics,

        // ── Git ────────────────────────────────────────────
        GitStatus,
        GitDiff,
        GitStageHunk,
        GitUnstageHunk,
        GitBlame,
        GitLog,
        GitNextHunk | "next_hunk" | "]h",
        GitPrevHunk | "prev_hunk" | "[h",
        GitRevertHunk | "hunk_revert" | "ghr",
        GitGutterToggle | "ghg",

        // ── Completion ─────────────────────────────────────
        TriggerCompletion | "ctrl-space",
        SelectNextCompletion,
        SelectPrevCompletion,
        ConfirmCompletion,
        CancelCompletion | "ctrl-e",
        TriggerCodeiumCompletion | "alt-/", 

        // ── Ripgrep ────────────────────────────────────────
        RipgrepUnderCursor | "grg",
        RipgrepInput | "gri",
        RipgrepGotoResult,
        RipgrepClose,
        RipgrepLast,
        RipgrepNextResult,
        RipgrepPrevResult,
        FunctionList,

        // ── Tags ──────────────────────────────────────────
        TagJump | "ctrl-]",
        TagNext,
        TagPrev,
        TagPop | "ctrl-t" | "pop",
        GenerateTags | "tags",

        // ── Buffers ────────────────────────────────────────
        ListBuffers | "ls",
        NextBuffer | "bn",
        PrevBuffer | "bp",

        // ── LLM ────────────────────────────────────────────
        LlmOpen | "gai",
        LlmClose,
        LlmSend,
        LlmCancel,
        LlmClearHistory,
        LlmNextPreset,
        LlmPrevPreset,
        LlmEnterPrompt,
        LlmQuickCheckEnglish,
        LlmQuickTranslateChinese,
        LlmQuickTranslateEnglish,
        LlmQuickExplain,
        LlmQuickSummarize,
        LlmQuickPrompt | "'",
        LlmSessionNew,

        // ── Misc ───────────────────────────────────────────
        Quit | "q",
        ForceQuit | "q!",
        ShowHelp,
        RunBuild,
        ToggleLineNumbers | "set nu",
        ToggleWhitespace,
        ShowShortcuts,
        ClearMessages,
        RegisterPrefix | "\"",
        RepeatLastAction | "."
    }

    None
}

// ── Config → Action binding helper ──────────────────────────────────

/// Parse a key-string + action-string pair from config TOML into a
/// `(KeySequence, Action)` tuple.
/// Parse a key-string + action-string pair from config TOML into a
/// `(KeySequence, Action)` tuple. Expands `<leader>` tags.
fn parse_key_action(
    key_str: &str,
    action_name: &str,
    leader: char,
) -> Result<(Vec<Key>, Action), String> {
    let keys = parse_key_sequence_with_leader(key_str, leader)
        .ok_or_else(|| format!("invalid key sequence: '{}'", key_str))?;
    let action = parse_action_str(action_name)
        .ok_or_else(|| format!("unknown action: '{}'", action_name))?;
    Ok((keys, action))
}

// ── Apply custom keybindings from config ────────────────────────────

#[rustfmt::skip]
/// Apply custom keybindings from a TOML table (from config) to the keybind manager.
/// The table format is `[keybindings.normal]` with entries like `"<alt-q>" = "force_quit"`.
/// Supports `<leader>` prefix in key sequences for ALL modes (e.g. `"<leader>pp"`).
pub fn apply_custom_keybindings(
    keybinds: &mut KeyBindManager,
    custom: &KeyBindingsConfig,
    leader: Option<char>,
) {
    let leader_ch = leader.unwrap_or('\\');
    let mut applied = 0usize;
    let mut skipped = 0usize;

    // ── Normal mode ──
    if let Some(bindings) = &custom.normal {
        for (key_str, action_name) in bindings {
            match parse_key_action(key_str, action_name, leader_ch) {
                Ok((keys, action)) => {
                    info!("[keybind] normal: {} → {}", key_str, action_name);
                    keybinds.bind("normal", keys, action);
                    applied += 1;
                }
                Err(e) => {
                    warn!("[keybind] normal: SKIP {} → {} ({})", key_str, action_name, e);
                    skipped += 1;
                }
            }
        }
    }

    // ── Insert mode ──
    if let Some(bindings) = &custom.insert {
        for (key_str, action_name) in bindings {
            match parse_key_action(key_str, action_name, leader_ch) {
                Ok((keys, action)) => {
                    info!("[keybind] insert: {} → {}", key_str, action_name);
                    keybinds.bind("insert", keys, action);
                    applied += 1;
                }
                Err(e) => {
                    warn!("[keybind] insert: SKIP {} → {} ({})", key_str, action_name, e);
                    skipped += 1;
                }
            }
        }
    }

    // ── Visual mode ──
    if let Some(bindings) = &custom.visual {
        for (key_str, action_name) in bindings {
            match parse_key_action(key_str, action_name, leader_ch) {
                Ok((keys, action)) => {
                    info!("[keybind] visual: {} → {}", key_str, action_name);
                    keybinds.bind("visual", keys, action);
                    applied += 1;
                }
                Err(e) => {
                    warn!("[keybind] visual: SKIP {} → {} ({})", key_str, action_name, e);
                    skipped += 1;
                }
            }
        }
    }

    // ── Command mode ──
    if let Some(bindings) = &custom.command {
        for (key_str, action_name) in bindings {
            match parse_key_action(key_str, action_name, leader_ch) {
                Ok((keys, action)) => {
                    info!("[keybind] command: {} → {}", key_str, action_name);
                    keybinds.bind("command", keys, action);
                    applied += 1;
                }
                Err(e) => {
                    warn!("[keybind] command: SKIP {} → {} ({})", key_str, action_name, e);
                    skipped += 1;
                }
            }
        }
    }

    // ── Leader bindings (mapped to normal mode with leader prefix) ──
    if let Some(bindings) = &custom.leader {
        for (key_str, action_name) in bindings {
            // Prepend <leader> so it gets expanded by parse_key_sequence_with_leader
            let full_key = format!("<leader>{}", key_str);
            match parse_key_action(&full_key, action_name, leader_ch) {
                Ok((keys, action)) => {
                    info!("[keybind] leader: {} → {}", key_str, action_name);
                    keybinds.bind("normal", keys, action);
                    applied += 1;
                }
                Err(e) => {
                    warn!("[keybind] leader: SKIP {} → {} ({})", key_str, action_name, e);
                    skipped += 1;
                }
            }
        }
    }

    info!(
        "[keybind] applied {} custom bindings, skipped {} (leader='{}')",
        applied, skipped, leader_ch
    );
}
