// src/ed/git_commit.rs
//! Git commit message generation using LLM.
//!
//! Provides a special buffer that:
//! 1. Gathers `git diff --cached` and `git diff` (staged + unstaged) and the last 2 commit messages
//! 2. Sends them to the LLM with a conventional-commit prompt template
//! 3. Places the LLM-generated message into a GitCommit buffer for editing
//! 4. On `w`, executes `git add -u` (to auto-stage tracked changes) and then
//!    `git commit --cleanup=strip -F -` with the buffer content
//!    (lines starting with `#` are naturally ignored by git's cleanup)
//!
//! # Usage
//!
//! - `:gc` — Generate commit message from staged/unstaged changes via LLM
//! - From the **Git Status** buffer, press `c` to commit staged changes
//!
//! # Keybindings (in GitCommit buffer)
//!
//! - `w`  — Confirm and execute the commit (auto-stages tracked changes)
//! - `q`  — Cancel and close the buffer
//! - Normal editing keys (j/k, i, A, dd, etc.) are available for editing the message
//!
//! # Integration points (changes required in other files)
//!
//! 1. **`buffer.rs`** — Add `GitCommit` variant to `BufferKind` enum.
//! 2. **`editor.rs`** — Add fields `git_commit_buffer_id: Option<BufferId>`,
//!    `git_commit_start_time: Option<Instant>`, and `git_commit_diff_summary: Option<String>`,
//!    initialise all to `None` in `Editor::new()`.
//! 3. **`editor.rs` `process_key()`** — Add GitCommit key interception block
//!    (`w` → commit, `q` → close).
//! 4. **`ed/git_status.rs` `process_key()`** — Add `c` keybinding in GitStatus
//!    interception block to trigger `git_commit_generate()`.
//! 5. **`ed/llm_ext.rs` `poll_llm_responses()`** — Route responses through
//!    `git_commit_on_llm_response()` / `git_commit_on_llm_error()` BEFORE
//!    the infobar / LLM-buffer paths.
//! 6. **`editor.rs` `tick()`** — Call `tick_git_commit()` to animate the loading spinner.
//! 7. **`command_registry.rs`** — Register `:gc` command.

use crate::buffer::BufferKind;
use crate::ed::file_ops::FileOpsExt;
use crate::ed::llm_ext::LlmExt;
use crate::editor::{CommandResult, Editor};
use crate::misc::find_git_root;
use ropey::Rope;
use std::path::PathBuf;

const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
// ── Prompt template ─────────────────────────────────────────────────

/// The prompt sent to the LLM.  `{recent_commits}` and `{diff}` are
/// replaced with actual values at runtime.
const COMMIT_PROMPT_TEMPLATE: &str = r#"Generate a git commit message following this structure no explanation just the core:
1. First line: conventional commit format (type: concise description) (use semantic types like feat, fix, docs, style, refactor, perf, test, chore, etc.).
2. Optional bullet points if necessary:
   - Keep the second line blank
   - Keep them short and direct
   - Focus on what changed
   - Avoid overly formal or fluffy language
Examples:
feat: add user auth system

- Add JWT tokens for API auth
- Handle token refresh for long sessions

fix: resolve memory leak in worker pool

- Clean up idle connections
- Add timeout for stale workers

Simple change example:
fix: typo in README.md

Your message must be based on the provided git diff, with a bit of styling from recent commits.
Recent commits for reference:
{recent_commits}
Git diff:
{diff}"#;

// ── Public trait ─────────────────────────────────────────────────────

/// Extension trait for AI-assisted git commit message generation.
pub trait GitCommitExt {
    /// Open a GitCommit buffer and ask the LLM to generate a message
    /// from the currently staged diff and recent commit history.
    fn git_commit_generate(&mut self) -> CommandResult;

    /// Close the commit buffer without committing.
    fn git_commit_close(&mut self) -> CommandResult;

    /// Route a successful LLM response into the commit buffer.
    /// Returns `true` if the response was consumed (commit buffer is active).
    /// Call this from `poll_llm_responses` BEFORE the normal routing.
    fn git_commit_on_llm_response(&mut self, response: &str) -> bool;

    fn handle_commit_write(&mut self) -> CommandResult;

    /// Route an LLM error into the commit buffer.
    /// Returns `true` if the error was consumed (commit buffer is active).
    /// Call this from `poll_llm_responses` BEFORE the normal routing.
    fn git_commit_on_llm_error(&mut self, error: &str) -> bool;
}

// ── Trait implementation ────────────────────────────────────────────

impl GitCommitExt for Editor {
    fn git_commit_generate(&mut self) -> CommandResult {
        // ── 1. Locate git root ──────────────────────────────
        let start_dir = self
            .current_buffer()
            .and_then(|b| b.file_path.as_ref())
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let git_root = match find_git_root(&start_dir) {
            Some(root) => root,
            None => return CommandResult::Error("Not a git repository (or any parent)".to_string()),
        };

        // ── 2. Gather staged + unstaged diff ────────────────
        let staged_output = match std::process::Command::new("git")
            .args(["diff", "--cached", "-U3", "--diff-algorithm=minimal"])
            .current_dir(&git_root)
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(e) => return CommandResult::Error(format!("Failed to run git diff --cached: {}", e)),
        };

        let unstaged_output = match std::process::Command::new("git")
            .args(["diff", "-U3", "--diff-algorithm=minimal"])
            .current_dir(&git_root)
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(e) => return CommandResult::Error(format!("Failed to run git diff: {}", e)),
        };

        if staged_output.trim().is_empty() && unstaged_output.trim().is_empty() {
            return CommandResult::Error("No staged or unstaged changes found.".to_string());
        }

        let diff_output = if unstaged_output.trim().is_empty() {
            // Only staged changes
            staged_output
        } else if staged_output.trim().is_empty() {
            // Only unstaged changes
            format!("Unstaged changes:\n{}", unstaged_output)
        } else {
            // Both staged and unstaged
            format!("Staged changes:\n{}\nUnstaged changes:\n{}", staged_output, unstaged_output)
        };

        // ── 3. Gather numstat for the loading animation summary ──
        let staged_numstat = match std::process::Command::new("git")
            .args(["diff", "--cached", "--numstat"])
            .current_dir(&git_root)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => String::new(),
        };

        let unstaged_numstat = match std::process::Command::new("git")
            .args(["diff", "--numstat"])
            .current_dir(&git_root)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => String::new(),
        };

        let mut summary_lines = Vec::new();
        for line in staged_numstat.lines().chain(unstaged_numstat.lines()) {
            if let Some(formatted) = format_numstat_line(line) {
                summary_lines.push(formatted);
            }
        }

        self.git.commit_diff_summary = if summary_lines.is_empty() {
            None
        } else {
            Some(summary_lines.join("\n"))
        };

        // ── 4. Get last 2 commit messages (full body) ───────
        let recent_commits = match std::process::Command::new("git")
            .args(["log", "-2", "--format=%B"]) // %B = full body (subject + body)
            .current_dir(&git_root)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => "(no recent commits)".to_string(),
        };

        // ── 5. Format the prompt ────────────────────────────
        let prompt = COMMIT_PROMPT_TEMPLATE
            .replace("{recent_commits}", &recent_commits)
            .replace("{diff}", &diff_output);

        // ── 6. Create / reuse GitCommit buffer ──────────────
        self.git.commit_start_time = Some(std::time::Instant::now());

        let buffer_id = if let Some(existing_id) = self.buffers.find_by_kind(BufferKind::GitCommit) {
            if let Some(buffer) = self.buffers.get_mut(&existing_id) {
                buffer.rope = Rope::from_str(&format!(
                    "  [COMMIT] Generating commit message… 0.0s ⠋\n  {}\n\n  (querying LLM, please wait...)\n",
                    "─".repeat(40)
                ));
                buffer.dirty = false;
            }
            existing_id
        } else {
            let id = self.buffers.new_buffer();
            if let Some(buffer) = self.buffers.get_mut(&id) {
                buffer.kind = BufferKind::GitCommit;
                buffer.set_display_name("Git Commit");
                buffer.rope = Rope::from_str(&format!(
                    "  [COMMIT] Generating commit message… 0.0s ⠋\n  {}\n\n  (querying LLM, please wait...)\n",
                    "─".repeat(40)
                ));
                buffer.dirty = false;
            }
            id
        };

        // ── Switch to the commit buffer so the animation is visible ──
        self.save_current_position();
        if let Some(w) = self.windows.active_window_mut() {
            w.set_buffer(buffer_id);
            w.cursor.position = Default::default();
        }

        // ── 7. Mark as the active LLM commit target ─────────
        self.git.commit_buffer_id = Some(buffer_id);

        // ── 8. Send to LLM (single-shot, NOT session-based) ─
        let messages = vec![
            (
                "system".to_string(),
                "You are a git commit message generator. Generate concise, conventional-commit-style \
                 messages based on diffs. Output ONLY the commit message text — no explanations, \
                 no markdown code fences, no preamble."
                    .to_string(),
            ),
            ("user".to_string(), prompt),
        ];

        self.llm.single_shot = true;
        self.llm.infobar_response = false; // do NOT route to infobar
        self.spawn_llm_request(messages);

        self.set_status("Generating commit message…".to_string());
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn git_commit_on_llm_response(&mut self, response: &str) -> bool {
        self.git.commit_start_time = None;
        self.git.commit_diff_summary = None; // Clear animation summary

        let commit_buf_id = match self.git.commit_buffer_id {
            Some(id) => id,
            None => return false,
        };

        let cleaned = clean_llm_response(response);

        // Fetch git status to display at the bottom
        let git_status_output = self.get_git_status_for_commit();

        let mut status_lines = String::new();
        for line in git_status_output.lines() {
            status_lines.push_str(&format!("# {}\n", line));
        }

        let content = format!(
            "{}\n\n# ── w to commit  |  q to cancel ──\n# Lines starting with # are ignored\n#\n{}",
            cleaned.trim_end(),
            status_lines.trim_end()
        );

        if let Some(buffer) = self.buffers.get_mut(&commit_buf_id) {
            buffer.rope = Rope::from_str(&content);
            buffer.dirty = true;
        }

        // Move cursor to the top so the user can start editing the subject line
        if let Some(window) = self.windows.active_window_mut() {
            if window.buffer_id == commit_buf_id {
                window.cursor.position.line = 0;
                window.cursor.position.col = 0;
                window.cursor.desired_col = None;
            }
        }

        self.set_status("Commit message generated — edit and 'w' to commit".to_string());
        self.dirty.mark_all();
        true
    }

    fn git_commit_close(&mut self) -> CommandResult {
        self.git.commit_start_time = None;
        self.git.commit_diff_summary = None; // Clear animation summary

        let buffer_id = match self.git.commit_buffer_id.take() {
            Some(id) => id,
            None => {
                // Fallback: check the active window
                if let Some(window) = self.windows.active_window() {
                    let bid = window.buffer_id;
                    if self.buffers.get(&bid).map(|b| b.kind) == Some(BufferKind::GitCommit) {
                        bid
                    } else {
                        return CommandResult::NoOp;
                    }
                } else {
                    return CommandResult::NoOp;
                }
            }
        };

        // Cancel any in-flight LLM request for this commit
        if let Some(handle) = self.llm.task_handle.take() {
            handle.abort();
        }
        self.git.commit_buffer_id = None;

        self.git_commit_switch_back();
        self.buffers.remove(&buffer_id);
        self.dirty.mark_all();
        CommandResult::Message("Commit cancelled".to_string())
    }

    fn git_commit_on_llm_error(&mut self, error: &str) -> bool {
        self.git.commit_start_time = None;
        self.git.commit_diff_summary = None; // Clear animation summary

        if self.git.commit_buffer_id.is_none() {
            return false;
        }

        if error == "[cancelled]" {
            self.git.commit_buffer_id = None;
            let _ = self.git_commit_close();
            return true;
        }

        // Show error in the commit buffer itself so the user can see it
        if let Some(commit_buf_id) = self.git.commit_buffer_id {
            if let Some(buffer) = self.buffers.get_mut(&commit_buf_id) {
                buffer.rope = Rope::from_str(&format!(
                    "# Error generating commit message:\n# {}\n\n# Press q/Esc to cancel",
                    error
                ));
                buffer.dirty = true;
            }
        }

        self.set_error(format!("LLM error: {}", error));
        self.dirty.mark_all();
        true
    }

    /// Called when user presses 'w' in the commit buffer.
    fn handle_commit_write(&mut self) -> CommandResult {
        // ── Don't commit while the LLM is still streaming ──
        if self.llm.task_handle.is_some() {
            return CommandResult::Message("Wait for LLM response to finish…".to_string());
        }

        // ── Identify the commit buffer ──
        let buffer_id = match self.git.commit_buffer_id {
            Some(id) => id,
            None => return CommandResult::Error("No commit buffer active".to_string()),
        };

        let (text, start_dir) = match self.buffers.get(&buffer_id) {
            Some(buffer) => {
                let text = buffer.rope.to_string();
                let dir = buffer
                    .file_path
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
                (text, dir)
            }
            None => return CommandResult::Error("Commit buffer not found".to_string()),
        };

        // ── Pre-flight check: ensure there's at least one non-comment line ──
        // Git would reject it anyway, but this gives instant feedback without spawning a process.
        let has_real_content = text.lines().any(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        });

        if !has_real_content {
            return CommandResult::Error("Aborting commit due to empty commit message.".to_string());
        }

        let git_root = match find_git_root(&start_dir) {
            Some(root) => root,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        // ── Auto-stage tracked changes ──
        let add_output = match std::process::Command::new("git")
            .args(["add", "-u"])
            .current_dir(&git_root)
            .output()
        {
            Ok(o) => o,
            Err(e) => return CommandResult::Error(format!("Failed to run git add: {e}")),
        };

        if !add_output.status.success() {
            let msg = String::from_utf8_lossy(&add_output.stderr);
            return CommandResult::Error(format!("git add failed: {}", msg.trim()));
        }

        // ── Execute git commit -F - ──
        // We pipe the FULL text directly. Git natively strips the #-prefixed lines!
        use std::io::Write;
        let mut child = match std::process::Command::new("git")
            .args(["commit", "--cleanup=strip", "-F", "-"])
            .current_dir(&git_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return CommandResult::Error(format!("Failed to run git commit: {e}")),
        };

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }

        match child.wait_with_output() {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let summary = stdout.lines().next().unwrap_or("Committed successfully").to_string();

                // ── Cleanup ──
                self.git.commit_buffer_id = None;
                self.git.commit_diff_summary = None; // Clear animation summary
                self.git_commit_switch_back();
                self.buffers.remove(&buffer_id);
                self.dirty.mark_all();

                CommandResult::Message(summary)
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let msg = if stderr.trim().is_empty() {
                    stdout.trim().to_string()
                } else {
                    stderr.trim().to_string()
                };
                CommandResult::Error(format!("git commit failed: {}", msg))
            }
            Err(e) => CommandResult::Error(format!("Failed to wait for git commit: {e}")),
        }
    }
}

// ── Helper functions ────────────────────────────────────────────────

/// Strip markdown code fences and trim the LLM response.
fn clean_llm_response(text: &str) -> String {
    let mut result = text.trim().to_string();

    // Opening fence with optional language tag: ```text\n …
    if result.starts_with("```") {
        if let Some(nl) = result.find('\n') {
            result = result[nl + 1..].to_string();
        }
    }
    // Closing fence
    if result.ends_with("```") {
        result.truncate(result.len() - 3);
    }

    result.trim().to_string()
}

/// Filter a commit-message buffer: remove comment lines (#) and
/// collapse consecutive blank lines, returning a clean message ready
/// for `git commit -F`.
fn filter_commit_message(text: &str) -> String {
    let mut out = String::new();
    let mut prev_blank = false;

    for line in text.lines() {
        // Skip comment lines
        if line.trim_start().starts_with('#') {
            continue;
        }

        let is_blank = line.trim().is_empty();

        // Collapse consecutive blank lines into one
        if is_blank && prev_blank {
            continue;
        }

        out.push_str(line);
        out.push('\n');
        prev_blank = is_blank;
    }

    out.trim().to_string()
}

/// Format a single line of `git diff --numstat` output into a readable summary.
/// Input format: `additions\tdeletions\tfilename`
/// Output format: `filename   +++ add` or `filename   -- del` or `filename   +- add/del`
fn format_numstat_line(line: &str) -> Option<String> {
    let mut parts = line.splitn(3, '\t');
    let add_str = parts.next()?;
    let del_str = parts.next()?;
    let file = parts.next()?;

    if file.is_empty() {
        return None;
    }

    // Binary files show as `- - filename`
    if add_str == "-" || del_str == "-" {
        return Some(format!("{}   (binary)", file));
    }

    let add: usize = add_str.parse().ok()?;
    let del: usize = del_str.parse().ok()?;

    let change = if add > 0 && del == 0 {
        format!("+++ {}", add)
    } else if del > 0 && add == 0 {
        format!("-- {}", del)
    } else {
        format!("+- {}/{}", add, del)
    };

    Some(format!("{}   {}", file, change))
}

// ── Editor helper (private) ─────────────────────────────────────────

impl Editor {
    /// Animate the git commit buffer while LLM is generating.
    /// Called from `Editor::tick()`.
    pub fn tick_git_commit(&mut self) {
        if self.git.commit_buffer_id.is_some() && self.llm.task_handle.is_some() {
            if let Some(start) = self.git.commit_start_time {
                // Reuse the build spinner index (they won't run simultaneously)
                self.build.spinner_idx = (self.build.spinner_idx + 1) % SPINNER_CHARS.len();
                let elapsed = start.elapsed().as_secs_f32();
                let spinner = SPINNER_CHARS[self.build.spinner_idx];

                let status_msg = format!("{} Generating commit ({:.1}s)", spinner, elapsed);
                self.set_status(status_msg);
                self.dirty.status_powerline = true;
                self.dirty.status_cmdline = true;

                if let Some(id) = self.git.commit_buffer_id {
                    if let Some(buf) = self.buffers.get_mut(&id) {
                        // Append the diff summary line beneath the header if available
                        let summary_block = if let Some(ref summary) = self.git.commit_diff_summary {
                            format!("{}\n\n", summary)
                        } else {
                            String::new()
                        };

                        let header = format!(
                            "  [COMMIT] Generating commit message… {:.1}s {}\n  {}\n\n{}  querying LLM, please wait ...\n",
                            elapsed,
                            spinner,
                            "─".repeat(40),
                            summary_block
                        );
                        buf.rope = ropey::Rope::from(header);
                        buf.dirty = false;
                    }
                    self.dirty.windows = true;
                }
            }
        }
    }

    /// Switch the active window back to a Normal buffer after the commit
    /// buffer is closed or the commit is executed.
    fn git_commit_switch_back(&mut self) {
        let target_id = self
            .buffers
            .iter()
            .find(|b| b.kind == BufferKind::Normal && b.file_path.is_some())
            .or_else(|| self.buffers.iter().find(|b| b.kind == BufferKind::Normal))
            .map(|b| b.id);

        if let Some(target_id) = target_id {
            if let Some(window) = self.windows.active_window_mut() {
                window.set_buffer(target_id);
                self.restore_cursor_position();
                self.clamp_cursor_to_buffer(&target_id);
                self.ensure_cursor_visible(&target_id);
            }
        }
    }

    /// Fetch `git status --short` for displaying in the commit buffer.
    /// Excludes untracked files (`??`) and formats with indentation.
    fn get_git_status_for_commit(&self) -> String {
        let start_dir = self
            .current_buffer()
            .and_then(|b| b.file_path.as_ref())
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        if let Some(git_root) = find_git_root(&start_dir) {
            match std::process::Command::new("git")
                .args(["status", "--short", "--untracked-files=no"])
                .current_dir(&git_root)
                .output()
            {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let mut lines = vec!["".to_string()]; // ← hint line

                    for line in stdout.lines() {
                        let trimmed = line.trim_start();
                        if !trimmed.is_empty() {
                            lines.push(format!("     {}", trimmed));
                        }
                    }

                    lines.join("\n")
                }
                _ => "(failed to get status)".to_string(),
            }
        } else {
            "(not a git repo)".to_string()
        }
    }
}
