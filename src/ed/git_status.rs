// src/ed/git_status.rs
//! Git status buffer implementation.

use crate::buffer::BufferKind;
use crate::ed::FileOpsExt;
use crate::editor::{CommandResult, Editor};
use crate::git::{FileStatus, GitProvider, GitStatusEntry};
use crate::misc::find_git_root;
use ropey::Rope;
use std::path::PathBuf;

pub trait GitStatusExt {
    fn git_status_open(&mut self, path_arg: &str) -> CommandResult;
    fn git_status_refresh(&mut self) -> CommandResult;
    fn git_status_toggle_stage(&mut self) -> CommandResult;
    fn git_status_add_file(&mut self) -> CommandResult;
    fn git_status_goto_file(&mut self) -> CommandResult;
    fn git_status_close(&mut self) -> CommandResult;
}

/// Parse git status --porcelain=v1 output
fn parse_git_status_porcelain(output: &str) -> Vec<GitStatusEntry> {
    let mut entries = Vec::new();

    for line in output.lines() {
        if line.len() < 3 {
            continue;
        }

        let index = line.chars().next().unwrap();
        let worktree = line.chars().nth(1).unwrap();
        let path = line[3..].trim().to_string();

        let (status, path) = if index == 'R' || worktree == 'R' {
            if let Some(pos) = path.find(" -> ") {
                (FileStatus::Renamed, path[pos + 4..].to_string())
            } else {
                (FileStatus::Renamed, path)
            }
        } else if index == 'C' || worktree == 'C' {
            if let Some(pos) = path.find(" -> ") {
                (FileStatus::Copied, path[pos + 4..].to_string())
            } else {
                (FileStatus::Copied, path)
            }
        } else {
            let status = match (index, worktree) {
                ('A', _) | (_, 'A') => FileStatus::Added,
                ('D', _) | (_, 'D') => FileStatus::Deleted,
                ('M', _) | (_, 'M') => FileStatus::Modified,
                ('?', '?') => FileStatus::Untracked,
                ('!', '!') => FileStatus::Ignored,
                ('U', _) | (_, 'U') => FileStatus::Modified,
                _ => FileStatus::Modified,
            };
            (status, path)
        };

        let staged = match index {
            'A' | 'M' | 'D' | 'R' | 'C' => FileStatus::Modified,
            _ => FileStatus::Unmodified,
        };

        entries.push(GitStatusEntry {
            path: PathBuf::from(path),
            status,
            staged,
        });
    }

    entries
}

impl GitStatusExt for Editor {
    fn git_status_open(&mut self, path_arg: &str) -> CommandResult {
        let start_dir = if path_arg.is_empty() {
            self.current_buffer()
                .and_then(|b| b.file_path.as_ref())
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        } else {
            let path = PathBuf::from(path_arg);
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        };

        let git_root = match find_git_root(&start_dir) {
            Some(root) => root,
            None => {
                return CommandResult::Error(format!(
                    "Not a git repository (or any parent): {}",
                    start_dir.display()
                ))
            }
        };

        match GitProvider::new(&git_root) {
            Ok(gp) => {
                let needs_update = match &self.git.provider {
                    Some(existing) => existing.repo_path() != git_root,
                    None => true,
                };
                if needs_update {
                    self.git.provider = Some(gp);
                }
            }
            Err(e) => return CommandResult::Error(format!("Failed to init git: {}", e)),
        }

        if let Some(buf_id) = self.buffers.find_by_kind(BufferKind::GitStatus) {
            self.save_current_position();
            if let Some(window) = self.windows.active_window_mut() {
                window.set_buffer(buf_id);
            }
            return self.git_status_refresh();
        }

        let buffer_id = self.buffers.new_buffer();
        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            buffer.kind = BufferKind::GitStatus;
            buffer.set_display_name("Git Status");
        }

        self.save_current_position();
        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(buffer_id);
        }

        self.git_status_refresh()
    }

    fn git_status_refresh(&mut self) -> CommandResult {
        let git_provider = match &self.git.provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let git_root = git_provider.repo_path();

        // Run git status directly with --untracked-files=all
        let entries = match std::process::Command::new("git")
            .args(["status", "--porcelain=v1", "--untracked-files=normal"])
            .current_dir(git_root)
            .output()
        {
            Ok(output) if output.status.success() => {
                parse_git_status_porcelain(&String::from_utf8_lossy(&output.stdout))
            }
            Ok(output) => {
                return CommandResult::Error(format!(
                    "git status failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            Err(e) => return CommandResult::Error(format!("Failed to run git: {}", e)),
        };

        // Categorize entries
        let mut staged: Vec<&GitStatusEntry> = Vec::new();
        let mut unstaged: Vec<&GitStatusEntry> = Vec::new();
        let mut untracked: Vec<&GitStatusEntry> = Vec::new();

        for entry in &entries {
            if entry.status == FileStatus::Untracked {
                untracked.push(entry);
            } else if entry.staged != FileStatus::Unmodified {
                staged.push(entry);
            } else if entry.status != FileStatus::Unmodified {
                unstaged.push(entry);
            }
        }

        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return CommandResult::Error("No active window".to_string()),
        };

        let branch = git_provider
            .current_branch()
            .unwrap_or_else(|_| "HEAD".to_string());
        let repo_path = git_provider.repo_path().display();

        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
            let mut content = String::new();

            content.push_str(&format!("── Git Status ── {} @ {}\n", branch, repo_path));
            content.push('\n');
            content.push_str(&format!("Staged ({}):\n", staged.len()));
            if staged.is_empty() {
                content.push_str("  (none)\n");
            } else {
                for entry in &staged {
                    content.push_str(&format!(
                        "  {} {}\n",
                        staged_status_str(entry),
                        entry.path.display()
                    ));
                }
            }

            content.push('\n');
            content.push_str(&format!("Changes not staged ({}):\n", unstaged.len()));
            if unstaged.is_empty() {
                content.push_str("  (none)\n");
            } else {
                for entry in &unstaged {
                    content.push_str(&format!(
                        "  {} {}\n",
                        unstaged_status_str(entry),
                        entry.path.display()
                    ));
                }
            }

            content.push('\n');
            content.push_str(&format!("Untracked ({}):\n", untracked.len()));
            if untracked.is_empty() {
                content.push_str("  (none)\n");
            } else {
                for entry in &untracked {
                    content.push_str(&format!("  ?? {}\n", entry.path.display()));
                }
            }

            content.push('\n');
            content.push_str("── Keybindings ──\n");
            content.push_str("  s       Toggle stage/unstage\n");
            content.push_str("  c       Commit (generate message via LLM)\n");
            content.push_str("  Enter   Open file\n");
            content.push_str("  r       Refresh\n");
            content.push_str("  q       Close\n");

            buffer.rope = Rope::from_str(&content);
        }

        if let Some(window) = self.windows.active_window_mut() {
            window.cursor.position.line = 3;
            window.cursor.position.col = 4;
            window.cursor.desired_col = None;
        }

        self.dirty.mark_all();
        CommandResult::Message(format!(
            "Git status: {} staged, {} unstaged, {} untracked",
            staged.len(),
            unstaged.len(),
            untracked.len()
        ))
    }

    /// Toggle staging: stage if unstaged/untracked, unstage if staged
    fn git_status_toggle_stage(&mut self) -> CommandResult {
        let (path_str, is_staged) = match self.get_git_status_file_path() {
            Some(result) => result,
            None => {
                return CommandResult::Message(
                    "Move cursor to a file to toggle staging".to_string(),
                )
            }
        };

        let git_provider = match &self.git.provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let full_path = git_provider.repo_path().join(&path_str);

        let result: Result<String, String> = if is_staged {
            git_provider
                .unstage_file(&full_path)
                .map(|_| format!("Unstaged: {}", path_str))
                .map_err(|e| e.to_string())
        } else {
            git_provider
                .stage_file(&full_path)
                .map(|_| format!("Staged: {}", path_str))
                .map_err(|e| e.to_string())
        };

        self.apply_stage_result(result, &path_str)
    }

    /// Always run `git add` on the file (add all changes, including untracked)
    fn git_status_add_file(&mut self) -> CommandResult {
        let (path_str, _) = match self.get_git_status_file_path() {
            Some(result) => result,
            None => return CommandResult::Message("Move cursor to a file to add".to_string()),
        };

        let git_provider = match &self.git.provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let full_path = git_provider.repo_path().join(&path_str);

        let result: Result<String, String> = git_provider
            .stage_file(&full_path)
            .map(|_| format!("Added: {}", path_str))
            .map_err(|e| e.to_string());

        self.apply_stage_result(result, &path_str)
    }

    fn git_status_goto_file(&mut self) -> CommandResult {
        let (path_str, _) = match self.get_git_status_file_path() {
            Some(result) => result,
            None => return CommandResult::Message("Move cursor to a file to open it".to_string()),
        };

        let git_provider = match &self.git.provider {
            Some(gp) => gp,
            None => return CommandResult::Error("Not a git repository".to_string()),
        };

        let full_path = git_provider.repo_path().join(&path_str);

        match self.open_file(&full_path) {
            Ok(_) => {
                self.dirty.mark_all();
                CommandResult::ViewChanged
            }
            Err(e) => CommandResult::Error(format!("Failed to open file: {}", e)),
        }
    }

    fn git_status_close(&mut self) -> CommandResult {
        // Extract check first (matches ripgrep_close_buffer pattern).
        let (is_gs, buffer_id) = {
            if let Some(window) = self.windows.active_window() {
                let bid = window.buffer_id;
                let is_gs = self
                    .buffers
                    .get(&bid)
                    .map(|b| b.kind == BufferKind::GitStatus)
                    .unwrap_or(false);
                (is_gs, bid)
            } else {
                return CommandResult::NoOp;
            }
        };

        if !is_gs {
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
}

impl Editor {
    // Helper to handle common post-stage logic
    fn apply_stage_result(
        &mut self,
        result: Result<String, String>,
        path_str: &str,
    ) -> CommandResult {
        match result {
            Ok(msg) => {
                if let CommandResult::Error(e) = self.git_status_refresh() {
                    return CommandResult::Error(e);
                }

                // Find the file in refreshed buffer
                if let Some(window) = self.windows.active_window_mut() {
                    if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                        let mut found = false;
                        for line_idx in 0..buffer.line_count() {
                            if let Some(line_text) = buffer.line_text(line_idx) {
                                if line_text.contains(path_str) {
                                    window.cursor.position.line = line_idx;
                                    window.cursor.position.col =
                                        line_text.find(path_str).unwrap_or(4);
                                    window.cursor.desired_col = None;
                                    found = true;
                                    break;
                                }
                            }
                        }
                        if !found {
                            window.cursor.position.line = 3;
                            window.cursor.position.col = 4;
                            window.cursor.desired_col = None;
                        }
                    }
                }
                CommandResult::Message(msg)
            }
            Err(e) => CommandResult::Error(format!("Failed: {}", e)),
        }
    }

    fn get_git_status_file_path(&self) -> Option<(String, bool)> {
        let window = self.windows.active_window()?;
        let buffer = self.buffers.get(&window.buffer_id)?;

        if buffer.kind != BufferKind::GitStatus {
            return None;
        }

        let line_idx = window.cursor.position.line;
        let line_text = buffer.line_text(line_idx)?;
        let trimmed = line_text.trim();

        if trimmed.is_empty()
            || trimmed.starts_with("──")
            || trimmed.starts_with("Staged")
            || trimmed.starts_with("Changes")
            || trimmed.starts_with("Untracked")
            || trimmed.starts_with("Keybindings")
            || trimmed == "(none)"
        {
            return None;
        }

        if trimmed.len() < 3 {
            return None;
        }

        let status_indicator = &trimmed[..2];
        let is_staged = status_indicator.starts_with('S');
        let path_part = trimmed[2..].trim();

        if path_part.is_empty() {
            return None;
        }

        Some((path_part.to_string(), is_staged))
    }
}

fn staged_status_str(entry: &GitStatusEntry) -> &'static str {
    match entry.status {
        FileStatus::Added => "SA",
        FileStatus::Modified => "SM",
        FileStatus::Deleted => "SD",
        FileStatus::Renamed => "SR",
        FileStatus::Copied => "SC",
        _ => "S?",
    }
}

fn unstaged_status_str(entry: &GitStatusEntry) -> &'static str {
    match entry.status {
        FileStatus::Modified => " M",
        FileStatus::Added => " A",
        FileStatus::Deleted => " D",
        FileStatus::Renamed => " R",
        FileStatus::Copied => " C",
        _ => "  ",
    }
}
