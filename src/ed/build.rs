// src/ed/build.rs
//! Build command — run `cargo build --release` and capture errors/warnings
//! into a navigable build buffer (like a quickfix list).

use crate::buffer::{BufferId, BufferKind};
use crate::ed::editing::EditingExt;
use crate::ed::file_ops::FileOpsExt;
use crate::ed::repeat::RepeatExt;
use crate::ed::RepeatableAction;
use crate::editor::{CommandResult, Editor};
use std::path::{Path, PathBuf};
use std::time::Instant;

// ═══════════════════════════════════════════════════════════════════
// ── Data types ────────────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

/// A single parsed diagnostic from `cargo build` output.
#[derive(Debug, Clone)]
pub struct BuildDiagnostic {
    pub file_path: PathBuf,
    /// 1-based line number.
    pub line_number: usize,
    /// 1-based column number.
    pub column: usize,
    pub severity: BuildSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSeverity {
    Error,
    Warning,
    Note,
}
/// Result sent back from the background build thread.
pub struct BuildResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

// Spinner animation frames
const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
// ═══════════════════════════════════════════════════════════════════
// ── Extension trait ───────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

pub trait BuildExt {
    /// Run `cargo build --release` and show results in a build buffer.
    fn run_build(&mut self) -> CommandResult;

    /// Navigate to the build error under the cursor (Enter key).
    fn build_goto_error(&mut self) -> CommandResult;

    /// Close the build buffer and return to the previous code buffer.
    fn build_close(&mut self) -> CommandResult;

    /// Jump to the next build error in the quickfix list.
    fn build_next_error(&mut self) -> CommandResult;

    /// Jump to the previous build error in the quickfix list.
    fn build_prev_error(&mut self) -> CommandResult;

    fn build_insert_brace_content(&mut self) -> CommandResult;
    fn build_insert_error_snippet(&mut self) -> CommandResult;
    fn quickfix_next(&mut self) -> CommandResult;
    fn quickfix_prev(&mut self) -> CommandResult;
}

// ═══════════════════════════════════════════════════════════════════
// ── Implementation ────────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

impl BuildExt for Editor {
    fn run_build(&mut self) -> CommandResult {
        // Prevent double-builds
        if self.build_in_progress {
            return CommandResult::Message("Build already in progress...".into());
        }

        let project_root = self.find_cargo_root();
        let tx = self.build_response_tx.clone();
        let root_clone = project_root.clone();

        self.build_in_progress = true;
        self.build_start_time = Some(Instant::now());
        self.build_spinner_idx = 0;

        // Spawn background thread for cargo build
        std::thread::spawn(move || {
            let output = std::process::Command::new("cargo")
                .args([
                    "build",
                    "--release",
                    "--color=never",
                    "--message-format=short",
                ])
                .current_dir(&root_clone)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output();

            let result = match output {
                Ok(o) => BuildResult {
                    stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                    success: o.status.success(),
                },
                Err(e) => BuildResult {
                    stdout: String::new(),
                    stderr: e.to_string(),
                    success: false,
                },
            };
            let _ = tx.send(result);
        });

        // Create empty build buffer immediately with animation header
        let header = format!(
            "  [BUILD] cargo build --release — Building... 0.0s ⠋\n  {}\n\n  (compiling, please wait...)\n",
            "─".repeat(40)
        );
        let build_id = self.ensure_build_buffer(&header);

        // Switch to the build buffer
        self.save_current_position();
        if let Some(w) = self.windows.active_window_mut() {
            w.set_buffer(build_id);
            w.cursor.position = Default::default();
        }

        self.dirty.mark_all();
        CommandResult::ViewChanged
    }
    fn build_goto_error(&mut self) -> CommandResult {
        let (cursor_line, line_text) = match self.current_buffer_line() {
            Some(pair) => pair,
            None => return CommandResult::NoOp,
        };

        let mut diag = if let Some(d) = self.parse_location_from_line(&line_text) {
            d
        } else {
            let buffer_id = self.windows.active_window().map(|w| w.buffer_id);
            let buffer = match buffer_id.and_then(|id| self.buffers.get(&id)) {
                Some(b) => b,
                None => return CommandResult::NoOp,
            };

            let mut found = None;
            for line in (0..cursor_line).rev() {
                if let Some(text) = buffer.line_text(line) {
                    if let Some(d) = self.parse_location_from_line(&text) {
                        found = Some(d);
                        break;
                    }
                }
            }
            match found {
                Some(d) => d,
                None => return CommandResult::NoOp,
            }
        };

        // ── Resolve relative paths against project root ──
        if diag.file_path.is_relative() {
            let project_root = self.find_cargo_root();
            diag.file_path = project_root.join(&diag.file_path);
        }

        // Save build buffer position before jumping
        self.save_current_position();

        match self.open_file(&diag.file_path) {
            Ok(_) => {
                // Restore file's last known position first
                self.restore_cursor_position();

                // Override with the exact error location
                if let Some(w) = self.windows.active_window_mut() {
                    let max_line = self
                        .buffers
                        .get(&w.buffer_id)
                        .map(|b| b.line_count().saturating_sub(1))
                        .unwrap_or(0);
                    w.cursor.position.line = diag.line_number.saturating_sub(1).min(max_line);
                    w.cursor.position.col = diag.column.saturating_sub(1);
                    w.cursor.desired_col = None;
                    let bid = w.buffer_id;
                    self.ensure_cursor_visible(&bid);
                }
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Err(e) => CommandResult::Error(format!(
                "Failed to open {}: {}",
                diag.file_path.display(),
                e
            )),
        }
    }

    fn build_close(&mut self) -> CommandResult {
        let other_id = self
            .buffers
            .iter()
            .find(|b| b.kind == BufferKind::Normal)
            .map(|b| b.id);

        let is_build = self
            .windows
            .active_window()
            .and_then(|w| self.buffers.get(&w.buffer_id))
            .map(|b| b.kind == BufferKind::Build)
            .unwrap_or(false);

        if is_build {
            if let Some(other_id) = other_id {
                if let Some(w) = self.windows.active_window_mut() {
                    w.set_buffer(other_id);
                }
                self.restore_cursor_position();
            }
        }

        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn quickfix_next(&mut self) -> CommandResult {
        self.record_action(RepeatableAction::QuickfixNext, 1);
        if self.quickfix_results.is_empty() {
            return CommandResult::Message("No quickfix results".into());
        }
        if self.quickfix_index < self.quickfix_results.len() - 1 {
            self.quickfix_index += 1;
        } else {
            self.quickfix_index = 0;
        }
        self.quickfix_goto()
    }

    fn quickfix_prev(&mut self) -> CommandResult {
        self.record_action(RepeatableAction::QuickfixPrev, 1);
        if self.quickfix_results.is_empty() {
            return CommandResult::Message("No quickfix results".into());
        }
        if self.quickfix_index > 0 {
            self.quickfix_index -= 1;
        } else {
            self.quickfix_index = self.quickfix_results.len() - 1;
        }
        self.quickfix_goto()
    }

    fn build_next_error(&mut self) -> CommandResult {
        if self.quickfix_results.is_empty() {
            return CommandResult::Message("No build errors".into());
        }
        if self.quickfix_index < self.quickfix_results.len() - 1 {
            self.quickfix_index += 1;
        } else {
            self.quickfix_index = 0; // wrap around
        }
        self.quickfix_goto()
    }

    fn build_prev_error(&mut self) -> CommandResult {
        if self.quickfix_results.is_empty() {
            return CommandResult::Message("No build errors".into());
        }
        if self.quickfix_index > 0 {
            self.quickfix_index -= 1;
        } else {
            self.quickfix_index = self.quickfix_results.len() - 1;
        }
        self.quickfix_goto()
    }

    fn build_insert_error_snippet(&mut self) -> CommandResult {
        // 1. Get the current line text from the build buffer
        let (_, line_text) = match self.current_buffer_line() {
            Some(pair) => pair,
            None => return CommandResult::NoOp,
        };

        // 2. Extract snippet using multiple strategies
        let snippet = match extract_snippet(&line_text) {
            Some(content) => content,
            None => {
                return CommandResult::Message(
                    "No snippet ({ }, ` `, or | code) found on this line".into(),
                )
            }
        };

        if snippet.is_empty() {
            return CommandResult::Message("Snippet is empty".into());
        }

        // 3. Copy to yank register so the user can re-paste with `p`
        self.yank_register = snippet.clone();

        // 4. Jump to the error location (switches to the source buffer)
        let goto_result = self.build_goto_error();

        // 5. If successfully jumped, insert the extracted text at cursor
        if matches!(goto_result, CommandResult::ViewChanged) {
            self.ensure_undo_group();
            self.insert_text_at_cursor(&snippet);
            return CommandResult::ContentChanged;
        }

        goto_result
    }
    fn build_insert_brace_content(&mut self) -> CommandResult {
        // 1. Get the current line text from the build buffer
        let (_, line_text) = match self.current_buffer_line() {
            Some(pair) => pair,
            None => return CommandResult::NoOp,
        };

        // 2. Extract content within the last { } pair
        let brace_content = match extract_brace_content(&line_text) {
            Some(content) => content,
            None => return CommandResult::Message("No { } found on this line".into()),
        };

        if brace_content.is_empty() {
            return CommandResult::Message("{ } is empty on this line".into());
        }

        // 3. Copy to yank register so the user can re-paste with `p`
        self.yank_register = brace_content.clone();

        // 4. Jump to the error location (switches to the source buffer)
        let goto_result = self.build_goto_error();

        // 5. If successfully jumped, insert the extracted text at cursor
        if matches!(goto_result, CommandResult::ViewChanged) {
            self.ensure_undo_group();
            self.insert_text_at_cursor(&brace_content);
            return CommandResult::ContentChanged;
        }

        goto_result
    }
}

// ═══════════════════════════════════════════════════════════════════
// ── Editor helper methods ─────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

impl Editor {
    /// Find the best directory to run `cargo build` from.
    /// Walks upward looking for `Cargo.toml`, then falls back to the git root,
    /// then the current working directory.
    pub fn find_cargo_root(&self) -> PathBuf {
        use std::path::PathBuf;

        let start_path = self
            .current_buffer()
            .and_then(|b| b.file_path.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // Canonicalize to an absolute path so parent() walks up to the filesystem root
        let absolute_path = if start_path.is_relative() {
            std::env::current_dir()
                .map(|cwd| cwd.join(&start_path))
                .ok()
                .and_then(|p| p.canonicalize().ok())
                .unwrap_or(start_path.clone())
        } else {
            start_path.clone()
        };

        let start_dir = if absolute_path.is_file() {
            absolute_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| absolute_path.clone())
        } else {
            absolute_path.clone()
        };

        // 1. Search upward for Cargo.toml
        let mut current = start_dir.clone();
        loop {
            if current.join("Cargo.toml").exists() {
                return current;
            }
            match current.parent() {
                Some(p) => current = p.to_path_buf(),
                None => break,
            }
        }

        // 2. Fallback: check git root (which might be a workspace root)
        let git_root = crate::ripgrep::find_git_root(&start_path);
        if git_root.join("Cargo.toml").exists() {
            return git_root;
        }

        // 3. Ultimate fallback: return the start_dir and let cargo print its own error
        start_dir
    }
    /// Create or reuse a Build buffer with the given content.
    fn ensure_build_buffer(&mut self, content: &str) -> BufferId {
        // Reuse existing build buffer if one exists
        let existing_id = self
            .buffers
            .iter()
            .find(|b| b.kind == BufferKind::Build)
            .map(|b| b.id);

        let id = if let Some(id) = existing_id {
            if let Some(buf) = self.buffers.get_mut(&id) {
                buf.rope = ropey::Rope::from(content);
                buf.dirty = false;
            }
            id
        } else {
            let id = self.buffers.new_buffer();
            if let Some(buf) = self.buffers.get_mut(&id) {
                buf.kind = BufferKind::Build;
                buf.rope = ropey::Rope::from(content);
                buf.dirty = false;
            }
            id
        };

        id
    }

    /// Get (line_index, line_text) of the cursor line in the active buffer.
    pub fn current_buffer_line(&self) -> Option<(usize, String)> {
        let window = self.windows.active_window()?;
        let buffer = self.buffers.get(&window.buffer_id)?;
        let line_idx = window.cursor.position.line;
        let line_text = buffer
            .line_text(line_idx)?
            .trim_end_matches('\n')
            .to_string();
        Some((line_idx, line_text))
    }

    /// Try to parse a location from a build output line.
    /// Supports both `path:line:col: error: msg` (short format) and
    /// `--> path:line:col` (human format).
    pub fn parse_location_from_line(&self, line: &str) -> Option<BuildDiagnostic> {
        let trimmed = line.trim();

        // 1. Try short format: `path:line:col: severity[: code]: message`
        if let Some((idx, len, severity)) = find_severity_marker(trimmed) {
            let location_str = trimmed[..idx].trim();
            let message = trimmed[idx + len..].trim().to_string();
            return parse_path_line_col(location_str).map(|mut diag| {
                diag.severity = severity;
                diag.message = message;
                diag
            });
        }

        // 2. Try human format: `  --> path:line:col`
        if let Some(idx) = trimmed.find("-->") {
            let location_str = trimmed[idx + 3..].trim();
            return parse_path_line_col(location_str);
        }

        None
    }

    /// Jump to the current quickfix result (shared by build & ripgrep).
    pub fn quickfix_goto(&mut self) -> CommandResult {
        let result = match self.quickfix_results.get(self.quickfix_index) {
            Some(r) => r.clone(),
            None => return CommandResult::NoOp,
        };

        // Save current position before jumping
        self.save_current_position();

        match self.open_file(&result.file_path) {
            Ok(_) => {
                // Restore file's last known position first
                self.restore_cursor_position();

                // Override with the specific quickfix result location
                if let Some(w) = self.windows.active_window_mut() {
                    let max_line = self
                        .buffers
                        .get(&w.buffer_id)
                        .map(|b| b.line_count().saturating_sub(1))
                        .unwrap_or(0);
                    w.cursor.position.line = result.line_number.saturating_sub(1).min(max_line);
                    w.cursor.position.col = 0;
                    w.cursor.desired_col = None;
                    let bid = w.buffer_id;
                    self.ensure_cursor_visible(&bid);
                }
                self.set_status(format!(
                    "Quickfix {}/{}: {}:{}",
                    self.quickfix_index + 1,
                    self.quickfix_results.len(),
                    result.file_path.display(),
                    result.line_number,
                ));
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Err(e) => CommandResult::Error(e.to_string()),
        }
    }
    /// Poll the build background thread and update the animation.
    /// Called from `Editor::tick()`.
    pub fn tick_build(&mut self) {
        if self.build_in_progress {
            // 1. Update spinner and elapsed time
            self.build_spinner_idx = (self.build_spinner_idx + 1) % SPINNER_CHARS.len();
            let elapsed = self
                .build_start_time
                .map(|t| t.elapsed().as_secs_f32())
                .unwrap_or(0.0);

            let spinner = SPINNER_CHARS[self.build_spinner_idx];
            let status_msg = format!("{} Building ({:.1}s)", spinner, elapsed);
            self.set_status(status_msg);
            self.dirty.status_powerline = true;
            self.dirty.status_cmdline = true;

            // 2. Update the build buffer header with animation
            let existing_build_id = self
                .buffers
                .iter()
                .find(|b| b.kind == crate::buffer::BufferKind::Build)
                .map(|b| b.id);

            if let Some(id) = existing_build_id {
                if let Some(buf) = self.buffers.get_mut(&id) {
                    let header = format!(
                        "  [BUILD] cargo build --release — Building... {:.1}s {}\n  {}\n\n  (compiling, please wait...)\n",
                        elapsed, spinner, "─".repeat(40)
                    );
                    buf.rope = ropey::Rope::from(header);
                    buf.dirty = false; // Don't mark as needing file save
                }
                self.dirty.windows = true;
            }
        }

        // 3. Check if the background thread finished
        if let Ok(result) = self.build_response_rx.try_recv() {
            self.build_in_progress = false;
            self.build_start_time = None;
            self.build_spinner_idx = 0;

            let full_output = format!("{}{}", result.stdout, result.stderr);
            let project_root = self.find_cargo_root();

            let diagnostics = parse_cargo_output(&full_output, &project_root);

            // Populate quickfix list
            self.quickfix_results = diagnostics
                .iter()
                .filter(|d| d.line_number > 0)
                .map(|d| crate::ripgrep::RipgrepResult {
                    file_path: d.file_path.clone(),
                    line_number: d.line_number,
                    line_content: d.message.clone(),
                })
                .collect();
            self.quickfix_index = 0;
            self.build_diagnostics = diagnostics;

            // Format final buffer text
            let buffer_text = format_build_buffer(&full_output, &self.build_diagnostics);
            let build_id = self.ensure_build_buffer(&buffer_text);

            // Update status based on result
            if result.success {
                self.set_status("Build succeeded ✓".into());
            } else {
                let errors = self
                    .build_diagnostics
                    .iter()
                    .filter(|d| d.severity == BuildSeverity::Error)
                    .count();
                let warns = self
                    .build_diagnostics
                    .iter()
                    .filter(|d| d.severity == BuildSeverity::Warning)
                    .count();
                self.set_status(format!(
                    "Build failed: {} error(s), {} warning(s)",
                    errors, warns
                ));
            }

            self.dirty.mark_all();
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// ── Parsing ──────────────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

/// Find a severity marker in a cargo output line.
///
/// Handles both standard and code-annotated formats:
/// - `: error: `           → standard
/// - `: error[E0046]: `    → with diagnostic code
/// - `: warning: `         → standard
/// - `: warning[unused]: ` → with diagnostic code
///
/// Returns `(position, marker_length, severity)` where:
/// - `position` is the byte index of the colon preceding the severity word
/// - `marker_length` includes the severity word, optional `[code]`, and trailing `: `
/// - `severity` indicates the diagnostic level
///
/// If multiple severity markers appear in the line, the leftmost one is returned
/// (which corresponds to the actual diagnostic location, not a mention in the message).
fn find_severity_marker(line: &str) -> Option<(usize, usize, BuildSeverity)> {
    let mut best: Option<(usize, usize, BuildSeverity)> = None;

    for (label, severity) in [
        ("error", BuildSeverity::Error),
        ("warning", BuildSeverity::Warning),
        ("note", BuildSeverity::Note),
    ] {
        let marker = format!(": {}", label);
        if let Some(pos) = line.find(&marker) {
            let after = pos + marker.len();
            let rest = line.get(after..).unwrap_or("");

            let marker_len = if rest.starts_with(": ") {
                // Standard format: `: error: `
                Some(marker.len() + 2) // +2 for ": "
            } else if rest.starts_with('[') {
                // With diagnostic code: `: error[E0046]: `
                rest.find("]: ").map(|be| marker.len() + be + 3) // +3 for "]: "
            } else {
                None
            };

            if let Some(len) = marker_len {
                if best.as_ref().map_or(true, |(bp, _, _)| pos < *bp) {
                    best = Some((pos, len, severity));
                }
            }
        }
    }

    best
}

/// Extract the content of the last matching `{ }` pair in a line.
/// Handles the common case in compiler output where types or expressions
/// are wrapped in `{…}` for emphasis.
fn extract_brace_content(line: &str) -> Option<String> {
    let close = line.rfind('}')?;
    let open = line[..close].rfind('{')?;
    Some(line[open + 1..close].trim().to_string())
}

/// Parse `"src/main.rs:10:5"` → `BuildDiagnostic`.
fn parse_path_line_col(s: &str) -> Option<BuildDiagnostic> {
    // Trim trailing whitespace or punctuation
    let s = s.trim_end_matches(|c: char| c.is_whitespace() || c == ':');

    // rsplitn from the right: col : line : rest-is-path
    // This automatically handles Windows drive letters (C:\) because we only split 3 times from the right.
    let mut parts = s.rsplitn(3, ':');
    let col: usize = parts.next()?.parse().ok()?;
    let line: usize = parts.next()?.parse().ok()?;
    let path_str = parts.next()?;

    if path_str.is_empty() {
        return None;
    }

    Some(BuildDiagnostic {
        file_path: PathBuf::from(path_str),
        line_number: line,
        column: col,
        severity: BuildSeverity::Error, // default, overridden by caller
        message: String::new(),
    })
}

/// Parse the full `cargo build` output into structured diagnostics.
/// Handles both `--message-format=short` and default human-readable output.
fn parse_cargo_output(output: &str, project_root: &Path) -> Vec<BuildDiagnostic> {
    let mut diagnostics = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();

        // ── Try parsing short format: `path:line:col: severity[: code]: message` ──
        if let Some((idx, len, severity)) = find_severity_marker(trimmed) {
            let location_str = trimmed[..idx].trim();
            let message = trimmed[idx + len..].trim();

            if let Some(mut diag) = parse_path_line_col(location_str) {
                if diag.file_path.is_relative() {
                    diag.file_path = project_root.join(&diag.file_path);
                }
                diag.message = message.to_string();
                diag.severity = severity;
                diagnostics.push(diag);
                continue;
            }
        }

        // ── Try parsing human format: `  --> path:line:col` ──
        // (Fallback for normal `cargo build` output without `--message-format=short`)
        if let Some(idx) = trimmed.find("-->") {
            let location_str = trimmed[idx + 3..].trim();
            if let Some(mut diag) = parse_path_line_col(location_str) {
                if diag.file_path.is_relative() {
                    diag.file_path = project_root.join(&diag.file_path);
                }
                // In human format, the error message is on previous lines,
                // but for the quickfix list we just need the location.
                diag.severity = BuildSeverity::Error;
                diagnostics.push(diag);
            }
        }
    }

    diagnostics
}

// ═══════════════════════════════════════════════════════════════════
// ── Buffer formatting ─────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════

/// Format the build output for display in the build buffer.
fn format_build_buffer(raw_output: &str, diagnostics: &[BuildDiagnostic]) -> String {
    let errors = diagnostics
        .iter()
        .filter(|d| d.severity == BuildSeverity::Error)
        .count();
    let warns = diagnostics
        .iter()
        .filter(|d| d.severity == BuildSeverity::Warning)
        .count();

    let mut buf = format!(
        "  [BUILD] cargo build --release — {} error(s), {} warning(s)\n",
        errors, warns
    );
    buf.push_str(&format!("  {}\n\n", "─".repeat(40)));

    // ── Quickfix list with source context ──
    if !diagnostics.is_empty() {
        for (idx, diag) in diagnostics.iter().enumerate() {
            let severity_str = match diag.severity {
                BuildSeverity::Error => "ERROR",
                BuildSeverity::Warning => "WARN",
                BuildSeverity::Note => "NOTE",
            };
            let severity_marker = match diag.severity {
                BuildSeverity::Error => "✗",
                BuildSeverity::Warning => "⚠",
                BuildSeverity::Note => "ℹ",
            };

            buf.push_str(&format!(
                "  {} [{}] {}:{}:{}: {}\n",
                severity_marker,
                severity_str,
                diag.file_path.display(),
                diag.line_number,
                diag.column,
                diag.message,
            ));

            // ── Source context (like grep -B5 -A3) ──
            if let Some((start_line, context_lines)) =
                read_source_context(&diag.file_path, diag.line_number, 5, 3)
            {
                let last_line = start_line + context_lines.len().saturating_sub(1);
                let line_num_width = format!("{}", last_line).len().max(3);

                for (i, line) in context_lines.iter().enumerate() {
                    let line_num = start_line + i;
                    let is_error_line = line_num == diag.line_number;

                    if is_error_line {
                        buf.push_str(&format!(
                            "  > {:>width$} │ {}\n",
                            line_num,
                            line,
                            width = line_num_width,
                        ));
                        // Show a caret at the error column
                        if diag.column > 0 {
                            let prefix_w =
                                format!("  > {:>width$} │ ", "", width = line_num_width,).len();
                            // Calculate display width up to the column
                            let col_display: usize = line
                                .chars()
                                .take(diag.column.saturating_sub(1))
                                .map(|c| {
                                    unicode_width::UnicodeWidthStr::width(c.to_string().as_str())
                                })
                                .sum();
                            let caret_pad = prefix_w + col_display;
                            buf.push_str(&format!("  {:pad$}^\n", "", pad = caret_pad,));
                        }
                    } else {
                        buf.push_str(&format!(
                            "    {:>width$} │ {}\n",
                            line_num,
                            line,
                            width = line_num_width,
                        ));
                    }
                }
            }

            // Separator between diagnostics
            if idx + 1 < diagnostics.len() {
                buf.push_str(&format!("  {}\n", "─".repeat(40)));
            }
        }
        buf.push('\n');
    }

    // ── Raw compiler output ──
    buf.push_str(&format!(
        "  {} Raw Output {}\n",
        "─".repeat(14),
        "─".repeat(14)
    ));
    if raw_output.trim().is_empty() {
        buf.push_str("  (no output)\n");
    } else {
        buf.push_str(raw_output);
        if !raw_output.ends_with('\n') {
            buf.push('\n');
        }
    }

    // ── Keybindings hint ──
    buf.push('\n');
    buf.push_str(&format!("  {}\n", "─".repeat(40)));
    buf.push_str("  Keybindings: [Enter] open file  [n/N] next/prev  [y] copy  [q] close\n");

    buf
}
/// Extract the most relevant snippet from a compiler error line.
/// Looks for the last `{...}`, the last `` `...` ``, or a code line `| ...`.
fn extract_snippet(line: &str) -> Option<String> {
    // 1. Try to find the last { } pair
    if let Some(close) = line.rfind('}') {
        if let Some(open) = line[..close].rfind('{') {
            let content = line[open + 1..close].trim().to_string();
            if !content.is_empty() {
                return Some(content);
            }
        }
    }

    // 2. Try to find the last ` ` pair (backticks, used by rustc for identifiers)
    if let Some(close) = line.rfind('`') {
        if let Some(open) = line[..close].rfind('`') {
            if close > open + 1 {
                let content = line[open + 1..close].trim().to_string();
                if !content.is_empty() {
                    return Some(content);
                }
            }
        }
    }

    // 3. Try to extract the code from a source line (e.g., "42 |             al_entries: entries,")
    if let Some(pipe_pos) = line.find("| ") {
        let code = line[pipe_pos + 2..].trim();
        // Ignore error underline lines like "   |             ^^^^^^^^^^"
        if !code.is_empty()
            && !code.starts_with('^')
            && !code.starts_with('~')
            && !code.starts_with('-')
            && !code.starts_with('|')
        {
            return Some(code.to_string());
        }
    }

    None
}

/// Read source lines around a given line number from a file.
/// Returns `(start_line_1based, context_lines)` where `start_line_1based`
/// is the 1-based line number of the first returned line.
fn read_source_context(
    path: &Path,
    center_line: usize,
    before: usize,
    after: usize,
) -> Option<(usize, Vec<String>)> {
    if center_line == 0 {
        return None;
    }

    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let center = center_line.saturating_sub(1); // convert to 0-based
    let start = center.saturating_sub(before);
    let end = (center + after + 1).min(lines.len());

    let context: Vec<String> = lines[start..end]
        .iter()
        .map(|l| {
            // Truncate very long lines to keep the buffer readable
            let l = l.trim_end();
            if l.chars().count() > 120 {
                format!("{}…", l.chars().take(117).collect::<String>())
            } else {
                l.to_string()
            }
        })
        .collect();

    Some((start + 1, context)) // back to 1-based
}
