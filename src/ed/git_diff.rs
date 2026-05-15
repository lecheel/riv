// src/ed/git_diff.rs
//! Git diff buffer implementation.
//!
//! Provides a special buffer that displays `git diff <ref> -- <file>` output
//! for the current file. Supports navigating to files at hunk lines,
//! jumping between hunks, refreshing, and closing.
//!
//! # Usage
//!
//! - `:gdiff`       — `git diff HEAD -- <file>` (all uncommitted changes)
//! - `:gdiff 3`     — `git diff HEAD~3 HEAD -- <file>` (last 3 commits)
//! - `:gdiff main`  — `git diff main HEAD -- <file>` (changes since main)
//!
//! # Keybindings (in GitDiff buffer)
//!
//! - `Enter` — Open file at hunk line
//! - `n`     — Next diff hunk
//! - `N`     — Previous diff hunk
//! - `r`     — Refresh diff
//! - `q`     — Close

use crate::buffer::{BufferKind, Language};
use crate::ed::FileOpsExt;
use crate::editor::{CommandResult, Editor};
use crate::git::find_git_root;
use crate::git::GitProvider;
use ropey::Rope;
use std::path::PathBuf;

// ── Public trait ─────────────────────────────────────────────────────

/// Extension trait for git diff buffer operations.
pub trait GitDiffExt {
    /// Open a git diff buffer for the current file.
    /// `ref_arg` is an optional git ref (e.g. `"3"` → `HEAD~3`, `"main"`, `""` → `HEAD`).
    fn git_diff_open(&mut self, ref_arg: &str) -> CommandResult;
    fn git_diff_all(&mut self, ref_arg: &str) -> CommandResult;
    /// Refresh the git diff buffer content (re-runs `git diff`).
    fn git_diff_refresh(&mut self) -> CommandResult;

    /// Open the file at the line indicated by the diff hunk under cursor.
    fn git_diff_goto_file(&mut self) -> CommandResult;

    /// Close the git diff buffer and switch back to a Normal buffer.
    fn git_diff_close(&mut self) -> CommandResult;

    /// Jump to the next diff hunk (`@@` line).
    fn git_diff_next_hunk(&mut self) -> CommandResult;

    /// Jump to the previous diff hunk (`@@` line).
    fn git_diff_prev_hunk(&mut self) -> CommandResult;
}

// ── Internal: parse hunk header ─────────────────────────────────────

/// Parse the new-file start line number from a unified-diff hunk header.
///
/// Input:  `@@ -10,7 +12,9 @@ some context`
/// Output: `Some(12)`
fn parse_hunk_new_start(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("@@")?;
    let rest = rest.trim_start();
    let parts: Vec<&str> = rest.splitn(4, ' ').collect();
    if parts.len() < 2 {
        return None;
    }
    let new_range = parts[1].strip_prefix('+')?;
    let num_str: String = new_range
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if num_str.is_empty() {
        return None;
    }
    num_str.parse().ok()
}

/// Compute the path of `file_path` relative to `repo_path`.
///
/// Handles the case where `file_path` is relative (e.g. `buffer.rs`)
/// by canonicalizing both paths before stripping the prefix.
fn compute_rel_path(file_path: &std::path::Path, repo_path: &std::path::Path) -> PathBuf {
    // 1) Try direct strip (works when file_path is already absolute)
    if let Ok(rel) = file_path.strip_prefix(repo_path) {
        return rel.to_path_buf();
    }

    // 2) Canonicalize both, then strip
    let canon_file = match std::fs::canonicalize(file_path) {
        Ok(p) => p,
        Err(_) => return file_path.to_path_buf(), // last resort
    };
    let canon_repo = match std::fs::canonicalize(repo_path) {
        Ok(p) => p,
        Err(_) => return file_path.to_path_buf(),
    };

    if let Ok(rel) = canon_file.strip_prefix(&canon_repo) {
        return rel.to_path_buf();
    }

    // 3) Give up — return as-is
    file_path.to_path_buf()
}

/// Resolve `file_path` to an absolute, canonical path for reliable
/// storage in metadata and later file opening.
fn canonicalize_file_path(file_path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.to_path_buf())
}

/// Run `git diff` and return stdout.
fn run_git_diff(repo_path: &std::path::Path, args: &[&str]) -> Result<String, String> {
    match std::process::Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
    {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr);
            // git diff exits with 1 when there ARE differences — that's success.
            if !o.status.success() && o.status.code() != Some(1) {
                if !stderr.is_empty() {
                    return Err(format!("git diff failed: {}", stderr.trim()));
                }
                return Err(format!(
                    "git diff failed with exit code {:?}",
                    o.status.code()
                ));
            }
            Ok(stdout)
        }
        Err(e) => Err(format!("Failed to run git: {}", e)),
    }
}

// ── Trait implementation ────────────────────────────────────────────

impl GitDiffExt for Editor {
    fn git_diff_open(&mut self, ref_arg: &str) -> CommandResult {
        let ref_arg = ref_arg.trim();

        // Resolve the git ref from the argument
        let git_ref = if ref_arg.is_empty() {
            "HEAD".to_string()
        } else if ref_arg.chars().all(|c| c.is_ascii_digit()) {
            // Pure number → HEAD~n
            let n: usize = ref_arg.parse().unwrap_or(0);
            if n == 0 {
                "HEAD".to_string()
            } else {
                format!("HEAD~{}", n)
            }
        } else {
            ref_arg.to_string()
        };

        // Determine the file path: if we're already in a GitDiff buffer,
        // extract the original file from metadata; otherwise use current buffer.
        let file_path = if self
            .current_buffer()
            .map(|b| b.kind == BufferKind::GitDiff)
            .unwrap_or(false)
        {
            match self.extract_git_diff_file_path() {
                Some(p) => p,
                None => {
                    return CommandResult::Error(
                        "Cannot determine original file for diff".to_string(),
                    )
                }
            }
        } else {
            match self.current_buffer().and_then(|b| b.file_path.clone()) {
                Some(p) => p,
                None => {
                    return CommandResult::Error(
                        "No file associated with current buffer".to_string(),
                    )
                }
            }
        };

        // Find git root
        let start_dir = file_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let git_root = match find_git_root(&start_dir) {
            Some(root) => root,
            None => {
                return CommandResult::Error(format!(
                    "Not a git repository (or any parent): {}",
                    start_dir.display()
                ))
            }
        };

        // Update git provider if needed
        let needs_update = match &self.git_provider {
            Some(existing) => existing.repo_path() != git_root,
            None => true,
        };
        if needs_update {
            match GitProvider::new(&git_root) {
                Ok(gp) => self.git_provider = Some(gp),
                Err(e) => return CommandResult::Error(format!("Failed to init git: {}", e)),
            }
        }

        let git_provider = match &self.git_provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        // ── KEY FIX: compute relative path correctly ──
        let canon_file_path = canonicalize_file_path(&file_path);
        let rel_path = compute_rel_path(&canon_file_path, git_provider.repo_path());
        let rel_path_str = rel_path.to_string_lossy();

        // Build git diff command
        let diff_output = if git_ref == "HEAD" {
            // Uncommitted changes: working tree + index vs HEAD
            run_git_diff(
                git_provider.repo_path(),
                &["diff", "HEAD", "--", &rel_path_str],
            )
        } else {
            // Commit range: ref vs HEAD
            run_git_diff(
                git_provider.repo_path(),
                &["diff", &git_ref, "HEAD", "--", &rel_path_str],
            )
        };

        match diff_output {
            Err(e) => CommandResult::Error(e),
            Ok(output) => {
                let file_path_owned = file_path.clone();
                let git_ref_owned = git_ref.clone();

                // Reuse an existing GitDiff buffer if one is already open
                if let Some(buf_id) = self.buffers.find_by_kind(BufferKind::GitDiff) {
                    self.save_current_position();
                    if let Some(window) = self.windows.active_window_mut() {
                        window.set_buffer(buf_id);
                    }
                    if let Some(buffer) = self.buffers.get_mut(&buf_id) {
                        buffer
                            .set_display_name(format!("Git Diff: {} vs {}", rel_path_str, git_ref));
                    }
                    return self.git_diff_populate(
                        buf_id,
                        &output,
                        &canon_file_path,
                        &git_ref_owned,
                        &rel_path_str,
                    );
                }

                // Create a new buffer
                let buffer_id = self.buffers.new_buffer();
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.kind = BufferKind::GitDiff;
                    buffer.set_display_name(format!("Git Diff: {} vs {}", rel_path_str, git_ref));
                    buffer.language = Some(Language::GitDiff);
                }

                self.save_current_position();
                if let Some(window) = self.windows.active_window_mut() {
                    window.set_buffer(buffer_id);
                }

                self.git_diff_populate(
                    buffer_id,
                    &output,
                    &canon_file_path,
                    &git_ref_owned,
                    &rel_path_str,
                )
            }
        }
    }

    /// `:gdiff!` — diff the entire repository (no file filter).
    ///
    /// Runs `git diff <ref>` without `-- <file>`.
    fn git_diff_all(&mut self, ref_arg: &str) -> CommandResult {
        let ref_arg = ref_arg.trim();

        let git_ref = if ref_arg.is_empty() {
            "HEAD".to_string()
        } else if ref_arg.chars().all(|c| c.is_ascii_digit()) {
            let n: usize = ref_arg.parse().unwrap_or(0);
            if n == 0 {
                "HEAD".to_string()
            } else {
                format!("HEAD~{}", n)
            }
        } else {
            ref_arg.to_string()
        };

        // Find git root from current buffer or cwd
        let start_dir = self
            .current_buffer()
            .and_then(|b| b.file_path.as_ref())
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let git_root = match find_git_root(&start_dir) {
            Some(root) => root,
            None => {
                return CommandResult::Error(format!(
                    "Not a git repository (or any parent): {}",
                    start_dir.display()
                ))
            }
        };

        let needs_update = match &self.git_provider {
            Some(existing) => existing.repo_path() != git_root,
            None => true,
        };
        if needs_update {
            match GitProvider::new(&git_root) {
                Ok(gp) => self.git_provider = Some(gp),
                Err(e) => return CommandResult::Error(format!("Failed to init git: {}", e)),
            }
        }

        let git_provider = match &self.git_provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        // No file filter — diff everything
        let diff_output = if git_ref == "HEAD" {
            run_git_diff(git_provider.repo_path(), &["diff", "HEAD"])
        } else {
            run_git_diff(git_provider.repo_path(), &["diff", &git_ref, "HEAD"])
        };

        match diff_output {
            Err(e) => CommandResult::Error(e),
            Ok(output) => {
                let git_ref_owned = git_ref.clone();

                // Reuse an existing GitDiff buffer if one is already open
                if let Some(buf_id) = self.buffers.find_by_kind(BufferKind::GitDiff) {
                    self.save_current_position();
                    if let Some(window) = self.windows.active_window_mut() {
                        window.set_buffer(buf_id);
                    }
                    if let Some(buffer) = self.buffers.get_mut(&buf_id) {
                        buffer.set_display_name(format!("Git Diff: ALL vs {}", git_ref));
                    }
                    return self.git_diff_populate_all(buf_id, &output, &git_ref_owned);
                }

                // Create a new buffer
                let buffer_id = self.buffers.new_buffer();
                if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                    buffer.kind = BufferKind::GitDiff;
                    buffer.set_display_name(format!("Git Diff: ALL vs {}", git_ref));
                    buffer.language = Some(Language::GitDiff);
                }

                self.save_current_position();
                if let Some(window) = self.windows.active_window_mut() {
                    window.set_buffer(buffer_id);
                }

                self.git_diff_populate_all(buffer_id, &output, &git_ref_owned)
            }
        }
    }

    fn git_diff_refresh(&mut self) -> CommandResult {
        let git_ref = match self.extract_git_diff_ref() {
            Some(r) => r,
            None => {
                return CommandResult::Error(
                    "Cannot determine git ref for diff refresh".to_string(),
                )
            }
        };

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".to_string()),
        };

        let git_provider = match &self.git_provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        // Check if this is an "all files" diff (no │file: metadata)
        let file_path = self.extract_git_diff_file_path();

        match file_path {
            Some(fp) => {
                // Single-file diff — refresh with file filter
                let rel_path = compute_rel_path(&fp, git_provider.repo_path());
                let rel_path_str = rel_path.to_string_lossy();

                let diff_output = if git_ref == "HEAD" {
                    run_git_diff(
                        git_provider.repo_path(),
                        &["diff", "HEAD", "--", &rel_path_str],
                    )
                } else {
                    run_git_diff(
                        git_provider.repo_path(),
                        &["diff", &git_ref, "HEAD", "--", &rel_path_str],
                    )
                };

                match diff_output {
                    Err(e) => CommandResult::Error(e),
                    Ok(output) => {
                        self.git_diff_populate(buffer_id, &output, &fp, &git_ref, &rel_path_str)
                    }
                }
            }
            None => {
                // All-files diff — refresh without file filter
                let diff_output = if git_ref == "HEAD" {
                    run_git_diff(git_provider.repo_path(), &["diff", "HEAD"])
                } else {
                    run_git_diff(git_provider.repo_path(), &["diff", &git_ref, "HEAD"])
                };

                match diff_output {
                    Err(e) => CommandResult::Error(e),
                    Ok(output) => self.git_diff_populate_all(buffer_id, &output, &git_ref),
                }
            }
        }
    }

    fn git_diff_goto_file(&mut self) -> CommandResult {
        let (file_path, line_num) = match self.get_git_diff_hunk_line() {
            Some(result) => result,
            None => {
                return CommandResult::Message(
                    "Move cursor to a diff hunk to jump to file".to_string(),
                )
            }
        };

        // Close the diff buffer first
        let _ = self.git_diff_close();

        // Open the file
        match self.open_file(&file_path) {
            Ok(_) => {
                // Move to the target line
                if let Some(window) = self.windows.active_window_mut() {
                    let max_line = self
                        .buffers
                        .get(&window.buffer_id)
                        .map(|b| b.line_count().saturating_sub(1))
                        .unwrap_or(0);
                    window.cursor.position.line = line_num.min(max_line);
                    window.cursor.position.col = 0;
                    window.cursor.desired_col = None;
                    let bid = window.buffer_id;
                    self.ensure_cursor_visible(&bid);
                }
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Err(e) => CommandResult::Error(format!("Failed to open file: {}", e)),
        }
    }

    fn git_diff_close(&mut self) -> CommandResult {
        let (is_gd, buffer_id) = {
            if let Some(window) = self.windows.active_window() {
                let bid = window.buffer_id;
                let is_gd = self
                    .buffers
                    .get(&bid)
                    .map(|b| b.kind == BufferKind::GitDiff)
                    .unwrap_or(false);
                (is_gd, bid)
            } else {
                return CommandResult::NoOp;
            }
        };

        if !is_gd {
            return CommandResult::NoOp;
        }

        // Switch to a Normal buffer
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

        self.buffers.remove(&buffer_id);
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn git_diff_next_hunk(&mut self) -> CommandResult {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return CommandResult::NoOp,
        };
        let buffer_id = window.buffer_id;
        let current_line = window.cursor.position.line;

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return CommandResult::NoOp,
        };

        if buffer.kind != BufferKind::GitDiff {
            return CommandResult::NoOp;
        }

        // Search forward for next @@ line
        for line_idx in (current_line + 1)..buffer.line_count() {
            if let Some(line_text) = buffer.line_text(line_idx) {
                if line_text.trim().starts_with("@@") {
                    if let Some(window) = self.windows.active_window_mut() {
                        window.cursor.position.line = line_idx;
                        window.cursor.position.col = 0;
                        window.cursor.desired_col = None;
                        self.ensure_cursor_visible(&buffer_id);
                    }
                    self.dirty.windows = true;
                    self.dirty.cursor = true;
                    return CommandResult::ViewChanged;
                }
            }
        }

        CommandResult::Message("No next hunk".to_string())
    }

    fn git_diff_prev_hunk(&mut self) -> CommandResult {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return CommandResult::NoOp,
        };
        let buffer_id = window.buffer_id;
        let current_line = window.cursor.position.line;

        let buffer = match self.buffers.get(&buffer_id) {
            Some(b) => b,
            None => return CommandResult::NoOp,
        };

        if buffer.kind != BufferKind::GitDiff {
            return CommandResult::NoOp;
        }

        if current_line == 0 {
            return CommandResult::Message("No previous hunk".to_string());
        }

        // Search backward for previous @@ line
        for line_idx in (0..current_line).rev() {
            if let Some(line_text) = buffer.line_text(line_idx) {
                if line_text.trim().starts_with("@@") {
                    if let Some(window) = self.windows.active_window_mut() {
                        window.cursor.position.line = line_idx;
                        window.cursor.position.col = 0;
                        window.cursor.desired_col = None;
                        self.ensure_cursor_visible(&buffer_id);
                    }
                    self.dirty.windows = true;
                    self.dirty.cursor = true;
                    return CommandResult::ViewChanged;
                }
            }
        }

        CommandResult::Message("No previous hunk".to_string())
    }
}

// ── Editor helper methods ───────────────────────────────────────────

impl Editor {
    /// Populate the GitDiff buffer with diff output and metadata.
    fn git_diff_populate(
        &mut self,
        buffer_id: crate::buffer::BufferId,
        diff_output: &str,
        file_path: &std::path::Path,
        git_ref: &str,
        rel_path: &str,
    ) -> CommandResult {
        let git_provider = match &self.git_provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let branch = git_provider
            .current_branch()
            .unwrap_or_else(|_| "HEAD".to_string());

        let mut content = String::new();

        // Header
        content.push_str(&format!(
            "── Git Diff ── {} vs {} ── branch: {} ──\n",
            rel_path, git_ref, branch
        ));
        // Metadata lines (parsed by refresh/goto — hidden from casual view)
        content.push_str(&format!("│file:{}\n", file_path.display()));
        content.push_str(&format!("│ref:{}\n", git_ref));
        content.push('\n');

        if diff_output.trim().is_empty() {
            content.push_str("  (no differences)\n");
        } else {
            content.push_str(diff_output);
            if !diff_output.ends_with('\n') {
                content.push('\n');
            }
        }

        content.push('\n');
        content.push_str("── Keybindings ──\n");
        content.push_str("  Enter   Open file at hunk line\n");
        content.push_str("  n       Next hunk\n");
        content.push_str("  p       Previous hunk\n");
        content.push_str("  r       Refresh\n");
        content.push_str("  q/Esc   Close\n");

        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            buffer.rope = Rope::from_str(&content);
        }

        // Position cursor at the first meaningful diff line
        // (skip header + 2 metadata lines + blank = 4 lines)
        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position.line = 4;
            window.cursor.position.col = 0;
            window.cursor.desired_col = None;
        }

        self.dirty.mark_all();
        CommandResult::Message(format!("Git diff: {} vs {}", rel_path, git_ref))
    }

    /// Extract the original file path from the `│file:` metadata line.
    fn extract_git_diff_file_path(&self) -> Option<PathBuf> {
        let window = self.windows.active_window()?;
        let buffer = self.buffers.get(&window.buffer_id)?;

        if buffer.kind != BufferKind::GitDiff {
            return None;
        }

        for i in 0..buffer.line_count().min(10) {
            if let Some(line_text) = buffer.line_text(i) {
                if let Some(rest) = line_text.strip_prefix("│file:") {
                    let path = rest.trim();
                    if !path.is_empty() {
                        return Some(PathBuf::from(path));
                    }
                }
            }
        }

        None
    }

    /// Extract the git ref from the `│ref:` metadata line.
    fn extract_git_diff_ref(&self) -> Option<String> {
        let window = self.windows.active_window()?;
        let buffer = self.buffers.get(&window.buffer_id)?;

        if buffer.kind != BufferKind::GitDiff {
            return None;
        }

        for i in 0..buffer.line_count().min(10) {
            if let Some(line_text) = buffer.line_text(i) {
                if let Some(rest) = line_text.strip_prefix("│ref:") {
                    let r = rest.trim();
                    if !r.is_empty() {
                        return Some(r.to_string());
                    }
                }
            }
        }

        None
    }

    /// Determine the file path and 0-indexed line number that the cursor
    /// corresponds to in the new file.
    ///
    /// Uses the hunk header above the cursor to find `+new_start`,
    /// then counts context (` `) and added (`+`) lines to calculate
    /// the offset.

    fn get_git_diff_hunk_line(&self) -> Option<(PathBuf, usize)> {
        let window = self.windows.active_window()?;
        let buffer = self.buffers.get(&window.buffer_id)?;

        if buffer.kind != BufferKind::GitDiff {
            return None;
        }

        let cursor_line = window.cursor.position.line;

        // Try single-file path from metadata first
        let single_file = self.extract_git_diff_file_path();

        // For all-files diff, extract from `diff --git a/path b/path` header
        let mut diff_file_path: Option<PathBuf> = None;
        for i in (0..=cursor_line).rev() {
            if let Some(line_text) = buffer.line_text(i) {
                let trimmed = line_text.trim();
                if let Some(rest) = trimmed.strip_prefix("diff --git ") {
                    // Format: "diff --git a/path/to/file b/path/to/file"
                    // The b/ path is the new-file version
                    let parts: Vec<&str> = rest.splitn(2, " b/").collect();
                    if parts.len() == 2 {
                        let new_path = parts[1].trim();
                        if !new_path.is_empty() {
                            // Resolve against git root
                            if let Some(gp) = &self.git_provider {
                                diff_file_path = Some(gp.repo_path().join(new_path));
                            }
                        }
                    }
                    break;
                }
            }
        }

        let file_path = single_file.or(diff_file_path)?;

        // Find the nearest @@ hunk header at or above the cursor
        let mut hunk_new_start: usize = 0;
        let mut hunk_header_line: usize = 0;
        let mut found_hunk = false;

        for i in (0..=cursor_line).rev() {
            if let Some(line_text) = buffer.line_text(i) {
                let trimmed = line_text.trim();
                if trimmed.starts_with("@@") {
                    if let Some(start) = parse_hunk_new_start(trimmed) {
                        hunk_new_start = start;
                        hunk_header_line = i;
                        found_hunk = true;
                    }
                    break;
                }
            }
        }

        if !found_hunk {
            // Cursor is not inside a hunk — just open the file at line 0
            return Some((file_path, 0));
        }

        // Count context (' ') and added ('+') lines between hunk header and cursor
        let mut new_line_offset: usize = 0;
        let limit = cursor_line.min(buffer.line_count().saturating_sub(1));
        for i in (hunk_header_line + 1)..=limit {
            if let Some(line_text) = buffer.line_text(i) {
                if line_text.is_empty() {
                    continue;
                }
                match line_text.chars().next() {
                    Some(' ') | Some('+') => new_line_offset += 1,
                    Some('-') => {}
                    _ => {}
                }
            }
        }

        let target_line = hunk_new_start.saturating_sub(1) + new_line_offset.saturating_sub(1);

        Some((file_path, target_line))
    }
    /// Populate the GitDiff buffer with a full-repo diff (no file filter).
    fn git_diff_populate_all(
        &mut self,
        buffer_id: crate::buffer::BufferId,
        diff_output: &str,
        git_ref: &str,
    ) -> CommandResult {
        let git_provider = match &self.git_provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let branch = git_provider
            .current_branch()
            .unwrap_or_else(|_| "HEAD".to_string());

        let mut content = String::new();

        content.push_str(&format!(
            "── Git Diff ── ALL FILES vs {} ── branch: {} ──\n",
            git_ref, branch
        ));
        // Metadata — no │file: line means "all files"
        content.push_str(&format!("│ref:{}\n", git_ref));
        content.push('\n');

        if diff_output.trim().is_empty() {
            content.push_str("  (no differences)\n");
        } else {
            content.push_str(diff_output);
            if !diff_output.ends_with('\n') {
                content.push('\n');
            }
        }

        content.push('\n');
        content.push_str("── Keybindings ──\n");
        content.push_str("  Enter   Open file at hunk line\n");
        content.push_str("  n       Next hunk\n");
        content.push_str("  N       Previous hunk\n");
        content.push_str("  r       Refresh\n");
        content.push_str("  q/Esc   Close\n");

        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            buffer.rope = Rope::from_str(&content);
        }

        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position.line = 3;
            window.cursor.position.col = 0;
            window.cursor.desired_col = None;
        }

        self.dirty.mark_all();
        CommandResult::Message(format!("Git diff: all files vs {}", git_ref))
    }
} // end of impl Editor
