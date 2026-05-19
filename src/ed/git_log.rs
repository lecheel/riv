// src/ed/git_log.rs
//! Git log buffer implementation — tig-style view.
//!
//! Displays recent git commits with full descriptions and file stats.
//! Supports navigating to files, viewing commit diffs, saving file
//! versions at a commit with `{6hash}_{filename}` naming, grepping,
//! and refreshing.
//!
//! # Keybindings (inside GitLog buffer)
//!
//! | Key     | Action                                    |
//! |---------|-------------------------------------------|
//! | Enter   | Open file / show commit info              |
//! | d / D   | Show commit diff (popup)                  |
//! | s / S   | Save file(s) at commit `{hash}_{name}`    |
//! | r / R   | Refresh log                               |
//! | q /     | Close                                     |
//!
//! # Command examples
//!
//! | Command          | Effect                                  |
//! |------------------|-----------------------------------------|
//! | `:glog` / `:tig` | Show last 5 commits                     |
//! | `:tig 0`         | Show all commits                        |
//! | `:tig 10`        | Show last 10 commits                    |
//! | `:tig Quick`     | Show commits matching "Quick"           |
//! | `:tig grep Quick`| Same as above                           |
//! | `:tig 5 Quick`   | Show up to 5 commits matching "Quick"   |

use crate::buffer::BufferKind;
use crate::buffer::Language;
use crate::ed::FileOpsExt;
use crate::editor::{CommandResult, Editor, FloatPopup};
use crate::git::GitProvider;
use crate::misc::find_git_root;
use ropey::Rope;
use std::path::{Path, PathBuf};

/// Sentinel value meaning "show all commits" (used when user passes `0`).
const COUNT_ALL: usize = 999;

/// Default number of commits when no count is specified.
const COUNT_DEFAULT: usize = 5;

// ── Public trait ─────────────────────────────────────────────────────

/// Extension trait for git log buffer operations.
pub trait GitLogExt {
    /// Open a git log buffer.
    ///
    /// - `count_arg`: number of commits ("" = 5, "0" = all)
    /// - `grep_arg`: filter commits by pattern (empty = no filter)
    fn git_log_open(&mut self, count_arg: &str, grep_arg: &str) -> CommandResult;

    /// Refresh the git log buffer content.
    fn git_log_refresh(&mut self) -> CommandResult;

    /// Show the full diff for the commit under the cursor.
    fn git_log_show_diff(&mut self) -> CommandResult;

    /// Open the file under the cursor, or show commit info.
    fn git_log_goto_file(&mut self) -> CommandResult;

    /// Close the git log buffer.
    fn git_log_close(&mut self) -> CommandResult;

    /// Save file(s) at the commit version with `{6hash}_{filename}` naming.
    fn git_log_save_file(&mut self) -> CommandResult;
}

// ── Internal types ──────────────────────────────────────────────────

/// A parsed commit with its full message and changed files.
#[derive(Debug, Clone)]
struct LogCommit {
    hash: String,
    short_hash: String,
    author: String,
    date: String,
    subject: String,
    body: String,
    files: Vec<LogFile>,
    stat: Option<String>,
}

/// A file changed in a commit.
#[derive(Debug, Clone)]
struct LogFile {
    stat_line: String,
}

// ── Parsing ─────────────────────────────────────────────────────────

fn parse_log_with_files(output: &str) -> Vec<LogCommit> {
    let mut commits = Vec::new();
    let mut current: Option<LogCommit> = None;
    let mut in_body = false;
    let mut body_lines: Vec<String> = Vec::new();

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("COMMIT:") {
            if let Some(mut c) = current.take() {
                c.body = clean_body(&body_lines);
                commits.push(c);
            }
            body_lines.clear();
            in_body = false;

            let parts: Vec<&str> = rest.splitn(5, '|').collect();
            if parts.len() >= 5 {
                current = Some(LogCommit {
                    hash: parts[0].to_string(),
                    short_hash: parts[1].to_string(),
                    author: parts[2].to_string(),
                    date: parts[3].to_string(),
                    subject: parts[4].to_string(),
                    body: String::new(),
                    files: Vec::new(),
                    stat: None,
                });
            }
        } else if line == "GSLOG_MSG_START" {
            in_body = true;
        } else if line == "GSLOG_MSG_END" {
            in_body = false;
        } else if in_body {
            body_lines.push(line.to_string());
        } else if let Some(ref mut c) = current {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.contains("file changed") || trimmed.contains("files changed") {
                c.stat = Some(trimmed.to_string());
            } else if trimmed.contains(" | ") {
                c.files.push(LogFile {
                    stat_line: line.trim_end().to_string(),
                });
            }
        }
    }

    if let Some(mut c) = current.take() {
        c.body = clean_body(&body_lines);
        commits.push(c);
    }

    commits
}

fn clean_body(lines: &[String]) -> String {
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

// ── Path resolution ─────────────────────────────────────────────────

fn resolve_stat_path(trimmed: &str) -> Option<PathBuf> {
    if !trimmed.contains(" | ") {
        return None;
    }
    let path_part = trimmed.split(" | ").next()?.trim();
    if path_part.is_empty() {
        return None;
    }

    let path_str = if path_part.contains(" => ") {
        let before_arrow = path_part.split(" => ").next().unwrap_or("").trim();
        let after_arrow = path_part.split(" => ").nth(1).unwrap_or("").trim();
        let prefix = before_arrow.trim_end_matches('{');
        let after_parts: Vec<&str> = after_arrow.splitn(2, '}').collect();
        let middle = after_parts[0];
        let suffix = if after_parts.len() > 1 {
            after_parts[1]
        } else {
            ""
        };
        format!("{}{}{}", prefix, middle, suffix)
    } else {
        path_part.to_string()
    };

    if path_str.starts_with("──")
        || path_str.starts_with("Enter")
        || path_str.starts_with("Open")
        || path_str.starts_with("Show")
        || path_str.starts_with("Save")
        || path_str.starts_with("Refresh")
        || path_str.starts_with("Close")
        || path_str.starts_with("Keybindings")
        || path_str.contains("file changed")
    {
        return None;
    }

    Some(PathBuf::from(path_str))
}

// ── Display helper ──────────────────────────────────────────────────

/// Format the commit count for display: "all" when `0` was requested,
/// otherwise the numeric string.
fn count_display(count: usize) -> String {
    if count >= COUNT_ALL {
        "all".to_string()
    } else {
        count.to_string()
    }
}

// ── Content generation ──────────────────────────────────────────────

fn build_log_content(
    commits: &[LogCommit],
    branch: &str,
    repo_path: &str,
    count: usize,
    grep: &str,
) -> String {
    let mut content = String::new();

    let grep_display = if grep.is_empty() {
        String::new()
    } else {
        format!(" grep='{}'", grep)
    };

    content.push_str(&format!(
        "── Git Log ── {} @ {} ── {} commits{} ──\n",
        branch,
        repo_path,
        count_display(count),
        grep_display
    ));
    content.push('\n');

    for (i, commit) in commits.iter().enumerate() {
        content.push_str(&format!("commit {}\n", commit.hash));
        content.push_str(&format!("Author: {}  {}\n", commit.author, commit.date));
        content.push_str(&format!("    {}\n", commit.subject));

        if !commit.body.is_empty() {
            content.push('\n');
            for body_line in commit.body.lines() {
                if body_line.trim().is_empty() {
                    content.push('\n');
                } else {
                    content.push_str(&format!("    {}\n", body_line));
                }
            }
        }

        content.push('\n');

        for file in &commit.files {
            content.push_str(&format!("{}\n", file.stat_line));
        }

        if let Some(ref stat) = commit.stat {
            content.push_str(&format!(" {}\n", stat));
        }

        if i < commits.len() - 1 {
            content.push_str("───────────────────────────────────────────────\n");
            content.push('\n');
        }
    }

    content.push('\n');
    content.push_str("── Keybindings ─────────────────────────────────\n");
    content.push_str("  Enter   Open file / show commit info\n");
    content.push_str("  d       Show commit diff\n");
    content.push_str("  s       Save file at commit [hash]_[name]\n");
    content.push_str("  r       Refresh\n");
    content.push_str("  q       Close\n");

    content
}

// ── File save helper ────────────────────────────────────────────────

fn save_file_at_commit(
    repo_path: &Path,
    hash: &str,
    short_hash: &str,
    file_path: &Path,
) -> Result<String, String> {
    let ref_path = format!("{}:{}", hash, file_path.to_string_lossy());

    let output = std::process::Command::new("git")
        .args(["show", &ref_path])
        .current_dir(repo_path)
        .output()
        .map_err(|e| format!("Failed to run git: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git show failed: {}", stderr.trim()));
    }

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let output_name = format!("{}_{}", short_hash, file_name);

    let output_path = repo_path.join(&output_name);

    std::fs::write(&output_path, &output.stdout)
        .map_err(|e| format!("Failed to write {}: {}", output_name, e))?;

    Ok(output_name)
}

// ── Trait implementation ────────────────────────────────────────────

impl GitLogExt for Editor {
    fn git_log_open(&mut self, count_arg: &str, grep_arg: &str) -> CommandResult {
        let count: usize = if count_arg.is_empty() {
            COUNT_DEFAULT
        } else {
            match count_arg.parse::<usize>() {
                Ok(0) => COUNT_ALL,
                Ok(n) => n.clamp(1, COUNT_ALL),
                Err(_) => COUNT_DEFAULT,
            }
        };

        self.git.log_count = count;
        self.git.log_grep = grep_arg.to_string();

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

        let needs_update = match &self.git.provider {
            Some(existing) => existing.repo_path() != git_root,
            None => true,
        };
        if needs_update {
            match GitProvider::new(&git_root) {
                Ok(gp) => self.git.provider = Some(gp),
                Err(e) => return CommandResult::Error(format!("Failed to init git: {}", e)),
            }
        }

        if self.git.provider.is_none() {
            match GitProvider::new(&start_dir) {
                Ok(gp) => {
                    let needs_update = match &self.git.provider {
                        Some(existing) => existing.repo_path() != gp.repo_path(),
                        None => true,
                    };
                    if needs_update {
                        self.git.provider = Some(gp);
                    }
                }
                Err(_) => {
                    return CommandResult::Error("Not a git repository (or any parent)".to_string())
                }
            }
        }

        // Reuse existing GitLog buffer if one is open.
        if let Some(buf_id) = self.buffers.find_by_kind(BufferKind::GitLog) {
            self.save_current_position();
            if let Some(window) = self.windows.active_window_mut() {
                window.set_buffer(buf_id);
            }
            return self.git_log_refresh_with_count(count, grep_arg);
        }

        // Create a new buffer.
        let buffer_id = self.buffers.new_buffer();
        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            buffer.kind = BufferKind::GitLog;
            let display = if grep_arg.is_empty() {
                format!("Git Log ({})", count_display(count))
            } else {
                format!("Git Log ({}) grep='{}'", count_display(count), grep_arg)
            };
            buffer.set_display_name(display);
            buffer.language = Some(Language::GitLog);
        }

        self.save_current_position();
        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(buffer_id);
        }

        self.git_log_refresh_with_count(count, grep_arg)
    }

    fn git_log_refresh(&mut self) -> CommandResult {
        let count = if self.git.log_count > 0 {
            self.git.log_count
        } else {
            self.extract_git_log_count().unwrap_or(COUNT_DEFAULT)
        };
        let grep = self.git.log_grep.clone();
        self.git_log_refresh_with_count(count, &grep)
    }

    fn git_log_show_diff(&mut self) -> CommandResult {
        let hash = match self.get_git_log_commit_hash() {
            Some(h) => h,
            None => {
                return CommandResult::Message(
                    "Move cursor to a commit to show its diff".to_string(),
                )
            }
        };

        let repo_path = match &self.git.provider {
            Some(gp) => gp.repo_path().to_path_buf(),
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let output = match std::process::Command::new("git")
            .args(["show", "--patch", &hash])
            .current_dir(&repo_path)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                return CommandResult::Error(format!("git show failed: {}", stderr.trim()));
            }
            Err(e) => return CommandResult::Error(format!("Failed to run git: {}", e)),
        };

        if output.trim().is_empty() {
            let short = &hash[..hash.len().min(8)];
            return CommandResult::Message(format!("No diff for commit {}", short));
        }

        // Remember where we came from so we can return
        let log_buffer_id = self.windows.active_window().map(|w| w.buffer_id);

        // Reuse existing GitDiff buffer if one is open
        let buffer_id = if let Some(existing_id) = self.buffers.find_by_kind(BufferKind::GitDiff) {
            if let Some(buffer) = self.buffers.get_mut(&existing_id) {
                let short = &hash[..hash.len().min(8)];
                buffer.set_display_name(format!("Diff: commit {}", short));
                buffer.rope = Rope::from_str(&output);
                buffer.dirty = true;
            }
            existing_id
        } else {
            let new_id = self.buffers.new_buffer();
            if let Some(buffer) = self.buffers.get_mut(&new_id) {
                buffer.kind = BufferKind::GitDiff;
                let short = &hash[..hash.len().min(8)];
                buffer.set_display_name(format!("Diff: commit {}", short));
                buffer.rope = Rope::from_str(&output);
                buffer.dirty = true;
            }
            new_id
        };

        // Store the log buffer so git_diff_close can return to it
        if let Some(log_id) = log_buffer_id {
            self.llm.origin_buffer_id = Some(log_id);
        }

        // Switch active window to the diff buffer
        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(buffer_id);
            window.cursor.position.line = 0;
            window.cursor.position.col = 0;
            window.cursor.desired_col = None;
        }

        self.ensure_cursor_visible_all();
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn git_log_goto_file(&mut self) -> CommandResult {
        if let Some((path, _status)) = self.get_git_log_file_path() {
            let repo_path = match &self.git.provider {
                Some(gp) => gp.repo_path().to_path_buf(),
                None => return CommandResult::Error("Not a git repository".to_string()),
            };

            let full_path = repo_path.join(&path);

            return match self.open_file(&full_path) {
                Ok(_) => {
                    self.dirty.mark_all();
                    CommandResult::ViewChanged
                }
                Err(e) => CommandResult::Error(format!("Failed to open file: {}", e)),
            };
        }

        let hash = match self.get_git_log_commit_hash() {
            Some(h) => h,
            None => return CommandResult::Message("Move cursor to a commit or file".to_string()),
        };

        let repo_path = match &self.git.provider {
            Some(gp) => gp.repo_path().to_path_buf(),
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let output = match std::process::Command::new("git")
            .args(["show", "--stat", &hash])
            .current_dir(&repo_path)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                return CommandResult::Error(format!("git show failed: {}", stderr.trim()));
            }
            Err(e) => return CommandResult::Error(format!("Failed to run git: {}", e)),
        };

        let all_lines: Vec<&str> = output.lines().collect();
        let max_lines = 60;
        let lines: Vec<String> = if all_lines.len() > max_lines {
            let mut l: Vec<String> = all_lines[..max_lines]
                .iter()
                .map(|s| s.to_string())
                .collect();
            l.push(format!("... ({} more lines)", all_lines.len() - max_lines));
            l
        } else {
            all_lines.iter().map(|s| s.to_string()).collect()
        };

        let short_hash = &hash[..hash.len().min(8)];
        let title = format!("Commit {}", short_hash);

        let mut popup = FloatPopup::new(title, lines);
        popup.max_height = (self.term_height.saturating_sub(4)).min(40);
        popup.width = ((self.term_width as usize) * 85 / 100).min(120) as u16;

        self.popup.float = Some(popup);
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn git_log_close(&mut self) -> CommandResult {
        let (is_gl, buffer_id) = {
            if let Some(window) = self.windows.active_window() {
                let bid = window.buffer_id;
                let is_gl = self
                    .buffers
                    .get(&bid)
                    .map(|b| b.kind == BufferKind::GitLog)
                    .unwrap_or(false);
                (is_gl, bid)
            } else {
                return CommandResult::NoOp;
            }
        };

        if !is_gl {
            return CommandResult::NoOp;
        }

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

    fn git_log_save_file(&mut self) -> CommandResult {
        let hash = match self.get_git_log_commit_hash() {
            Some(h) => h,
            None => {
                return CommandResult::Message(
                    "Move cursor to a commit or file stat line".to_string(),
                )
            }
        };

        let short_hash: String = hash.chars().take(6).collect();

        let repo_path = match &self.git.provider {
            Some(gp) => gp.repo_path().to_path_buf(),
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        if let Some((path, _)) = self.get_git_log_file_path() {
            return match save_file_at_commit(&repo_path, &hash, &short_hash, &path) {
                Ok(name) => {
                    self.set_status(format!("Saved: {}", name));
                    CommandResult::Message(format!("Saved: {}", name))
                }
                Err(e) => CommandResult::Error(e),
            };
        }

        let files = self.get_git_log_commit_files();
        if files.is_empty() {
            return CommandResult::Message("No files found for this commit".to_string());
        }

        let mut saved = Vec::new();
        let mut errors = Vec::new();

        for path in &files {
            match save_file_at_commit(&repo_path, &hash, &short_hash, path) {
                Ok(name) => saved.push(name),
                Err(e) => errors.push(e),
            }
        }

        if !errors.is_empty() {
            CommandResult::Error(format!(
                "Saved {}/{}, errors: {}",
                saved.len(),
                files.len(),
                errors.join("; ")
            ))
        } else {
            CommandResult::Message(format!(
                "Saved {} file(s): {}",
                saved.len(),
                saved.join(", ")
            ))
        }
    }
}

// ── Editor helper methods ───────────────────────────────────────────

impl Editor {
    fn git_log_refresh_with_count(&mut self, count: usize, grep: &str) -> CommandResult {
        let buffer_id = match self.windows.active_window() {
            Some(w) => {
                let bid = w.buffer_id;
                match self.buffers.get(&bid) {
                    Some(b) if b.kind == BufferKind::GitLog => bid,
                    _ => return CommandResult::Error("Not a Git Log buffer".to_string()),
                }
            }
            None => return CommandResult::Error("No active window".to_string()),
        };

        let git_provider = match &self.git.provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let git_root = git_provider.repo_path();
        let branch = git_provider
            .current_branch()
            .unwrap_or_else(|_| "HEAD".to_string());

        let stat_width = (self.term_width as usize).max(80);

        // Build git log arguments
        let grep_flag;
        let mut args_vec = vec![
            "log".to_string(),
            format!("-{}", count),
            "--pretty=format:COMMIT:%H|%h|%an|%ad|%s%nGSLOG_MSG_START%n%bGSLOG_MSG_END".to_string(),
            "--date=short".to_string(),
            format!("--stat={}", stat_width),
        ];

        if !grep.is_empty() {
            grep_flag = format!("--grep={}", grep);
            args_vec.push(grep_flag);
        }

        let output = match std::process::Command::new("git")
            .args(&args_vec)
            .current_dir(git_root)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            Ok(o) => {
                return CommandResult::Error(format!(
                    "git log failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                ));
            }
            Err(e) => return CommandResult::Error(format!("Failed to run git: {}", e)),
        };

        let grep_display = if grep.is_empty() {
            String::new()
        } else {
            format!(" grep='{}'", grep)
        };

        if output.trim().is_empty() {
            if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                buffer.rope = Rope::from_str(&format!(
                    "── Git Log ── {} @ {} ── {} commits{} ──\n\n  (no commits)\n",
                    branch,
                    git_root.display(),
                    count_display(count),
                    grep_display
                ));
            }
            self.dirty.mark_all();
            return CommandResult::Message("Git log: no commits".to_string());
        }

        let commits = parse_log_with_files(&output);

        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            let content = build_log_content(
                &commits,
                &branch,
                &git_root.display().to_string(),
                count,
                grep,
            );
            buffer.rope = Rope::from_str(&content);
        }

        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position.line = 2;
            window.cursor.position.col = 7;
            window.cursor.desired_col = None;
        }

        self.dirty.mark_all();
        CommandResult::Message(format!(
            "Git log: {} commits{}",
            commits.len(),
            if grep.is_empty() {
                String::new()
            } else {
                format!(" matching '{}'", grep)
            }
        ))
    }

    fn get_git_log_commit_hash(&self) -> Option<String> {
        let window = self.windows.active_window()?;
        let buffer = self.buffers.get(&window.buffer_id)?;

        if buffer.kind != BufferKind::GitLog {
            return None;
        }

        let line_idx = window.cursor.position.line;

        for i in (0..=line_idx).rev() {
            if let Some(line_text) = buffer.line_text(i) {
                let trimmed = line_text.trim();
                if let Some(hash) = trimmed.strip_prefix("commit ") {
                    let hash = hash.trim().to_string();
                    if !hash.is_empty() {
                        return Some(hash);
                    }
                }
            }
        }

        None
    }

    fn get_git_log_file_path(&self) -> Option<(PathBuf, String)> {
        let window = self.windows.active_window()?;
        let buffer = self.buffers.get(&window.buffer_id)?;

        if buffer.kind != BufferKind::GitLog {
            return None;
        }

        let line_idx = window.cursor.position.line;
        let line_text = buffer.line_text(line_idx)?;
        let trimmed = line_text.trim();

        resolve_stat_path(trimmed).map(|p| (p, String::new()))
    }

    fn get_git_log_commit_files(&self) -> Vec<PathBuf> {
        let window = match self.windows.active_window() {
            Some(w) => w,
            None => return Vec::new(),
        };
        let buffer = match self.buffers.get(&window.buffer_id) {
            Some(b) => b,
            None => return Vec::new(),
        };

        if buffer.kind != BufferKind::GitLog {
            return Vec::new();
        }

        let current_line = window.cursor.position.line;
        let line_count = buffer.line_count();

        let mut commit_start = 0;
        for i in (0..=current_line).rev() {
            if let Some(line_text) = buffer.line_text(i) {
                if line_text.trim().starts_with("commit ") {
                    commit_start = i;
                    break;
                }
            }
        }

        let mut commit_end = line_count;
        for i in (commit_start + 1)..line_count {
            if let Some(line_text) = buffer.line_text(i) {
                if line_text.trim().starts_with("commit ") {
                    commit_end = i;
                    break;
                }
            }
        }

        let mut files = Vec::new();
        for i in commit_start..commit_end {
            if let Some(line_text) = buffer.line_text(i) {
                if let Some(path) = resolve_stat_path(line_text.trim()) {
                    files.push(path);
                }
            }
        }

        files
    }

    fn extract_git_log_count(&self) -> Option<usize> {
        let window = self.windows.active_window()?;
        let buffer = self.buffers.get(&window.buffer_id)?;

        if buffer.kind != BufferKind::GitLog {
            return None;
        }

        let header = buffer.line_text(0)?;
        let marker = " commits ";
        let pos = header.find(marker)?;

        let before = &header[..pos];

        // Handle "all commits" header
        if let Some(all_pos) = before.rfind("all") {
            if all_pos + 3 == before.len() {
                return Some(COUNT_ALL);
            }
        }

        let num_start = before.rfind(|c: char| !c.is_ascii_digit())? + 1;
        before[num_start..].parse().ok()
    }
}
