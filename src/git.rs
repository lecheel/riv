//! Git integration module.
//!
//! Provides git status, diff, blame, and staging functionality
//! by shelling out to the `git` command.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Git sign (per-line change marker) ──────────────────────────────

/// What kind of change a line has relative to the index/HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitSign {
    /// Line was added (green +).
    Added,
    /// Line was modified (yellow ~).
    Modified,
    /// Line was removed above this position (red -).
    RemovedAbove,
}

// ── Hunk range (simplified for navigation) ─────────────────────────

/// A contiguous range of changed lines in the working tree.
/// Used for next-hunk / prev-hunk navigation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkRange {
    /// 0-based start line in the working tree file.
    pub start: usize,
    /// Number of working-tree lines this hunk spans.
    pub count: usize,
    /// The kind of change (used for gutter sign color).
    pub kind: GitSign,
    /// The raw diff hunk header for display.
    pub header: String,
}

impl HunkRange {
    /// Return the 0-based end line (exclusive).
    pub fn end(&self) -> usize {
        self.start + self.count
    }
}

/// A normalized editor-space hunk.
///
/// Unlike raw git diff ranges, this represents the hunk exactly
/// as it appears inside the editor UI.
///
/// All editor systems should use this:
/// - gutter
/// - popup
/// - navigation
/// - revert hit testing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorHunk {
    /// Visual/editor-space start line (0-based inclusive).
    pub start: usize,

    /// Visual/editor-space end line (0-based exclusive).
    pub end: usize,

    /// Primary hunk kind.
    pub kind: GitSign,

    /// Gutter signs belonging to this hunk.
    pub signs: Vec<EditorSign>,

    /// Original parsed git hunk.
    pub diff: DiffHunk,
}

/// A single editor-space gutter sign.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorSign {
    /// 0-based editor line.
    pub line: usize,

    /// Sign type.
    pub kind: GitSign,
}

// ── Error ───────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Git command failed: {0}")]
    CommandFailed(String),
    #[error("Git not found in PATH")]
    NotFound,
    #[error("Not a git repository: {0}")]
    NotRepository(PathBuf),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

// ── File status ─────────────────────────────────────────────────────

/// Status of a file in the git index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileStatus {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Ignored,
}

impl FileStatus {
    /// Parse the two-character git status code.
    pub fn from_status_code(code: &str) -> Self {
        if code.is_empty() {
            return FileStatus::Unmodified;
        }
        let index = code.chars().next().unwrap_or(' ');
        let worktree = code.chars().nth(1).unwrap_or(' ');

        match (index, worktree) {
            (' ', '?') => FileStatus::Untracked,
            ('A', _) | (_, 'A') => FileStatus::Added,
            ('D', _) | (_, 'D') => FileStatus::Deleted,
            ('R', _) | (_, 'R') => FileStatus::Renamed,
            ('C', _) | (_, 'C') => FileStatus::Copied,
            ('M', _) | (_, 'M') => FileStatus::Modified,
            ('!', _) => FileStatus::Ignored,
            _ => FileStatus::Unmodified,
        }
    }
}

// ── Git status entry ────────────────────────────────────────────────

/// A single file's git status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusEntry {
    /// Path relative to the repository root.
    pub path: PathBuf,
    /// Status of the file.
    pub status: FileStatus,
    /// Staged status (index).
    pub staged: FileStatus,
}

// ── Git diff line ───────────────────────────────────────────────────

/// A single line in a git diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffLine {
    /// Line type: addition, deletion, or context.
    pub type_: DiffLineType,
    /// The actual line content (without the leading +/-).
    pub content: String,
    /// Line number in the new file (for additions and context).
    pub new_lineno: Option<usize>,
    /// Line number in the old file (for deletions and context).
    pub old_lineno: Option<usize>,
}

/// Type of diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLineType {
    Add,
    Delete,
    Context,
    HunkHeader,
}

// ── Git diff hunk ───────────────────────────────────────────────────

/// A diff hunk (a contiguous block of changes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffHunk {
    /// Header line (e.g., "@@ -1,3 +1,4 @@ ...").
    pub header: String,
    /// Old file start line.
    pub old_start: usize,
    /// Old file line count.
    pub old_count: usize,
    /// New file start line.
    pub new_start: usize,
    /// New file line count.
    pub new_count: usize,
    /// Lines in this hunk.
    pub lines: Vec<DiffLine>,
}

// ── Git blame line ──────────────────────────────────────────────────

/// Blame information for a single line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameLine {
    /// Commit hash.
    pub commit: String,
    /// Author name.
    pub author: String,
    /// Author email.
    pub author_email: String,
    /// Author date (UNIX timestamp).
    pub author_time: i64,
    /// 1-based line number.
    pub lineno: usize,
    /// The line content.
    pub content: String,
}

// ── Git commit ──────────────────────────────────────────────────────

/// A git log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommit {
    /// Full commit hash.
    pub hash: String,
    /// Abbreviated hash.
    pub short_hash: String,
    /// Author name.
    pub author: String,
    /// Commit message (first line).
    pub message: String,
    /// UNIX timestamp.
    pub time: i64,
}

// ── Git provider ────────────────────────────────────────────────────

/// Provides git functionality for a repository.
pub struct GitProvider {
    /// Path to the git repository root.
    repo_path: PathBuf,
}

impl GitProvider {
    pub fn new(path: &Path) -> Result<Self, GitError> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::NotRepository(path.to_path_buf()));
        }

        let repo_path = PathBuf::from(String::from_utf8(output.stdout)?.trim());
        // Canonicalize repo_path to ensure it matches the format produced by
        // Path::canonicalize() on file paths (important for symlinks and Windows UNC paths).
        let repo_path = repo_path.canonicalize().unwrap_or(repo_path);

        Ok(Self { repo_path })
    }
    /// Run a git command and return stdout.
    fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GitError::CommandFailed(stderr.to_string()));
        }

        Ok(String::from_utf8(output.stdout)?)
    }

    /// Get the repository root path.
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Get the current branch name.
    pub fn current_branch(&self) -> Result<String, GitError> {
        let output = self.run(&["branch", "--show-current"])?;
        Ok(output.trim().to_string())
    }

    /// Check if the repository has any changes.
    pub fn is_clean(&self) -> Result<bool, GitError> {
        let output = self.run(&["status", "--porcelain"])?;
        Ok(output.trim().is_empty())
    }

    /// Get the full status of all files.
    pub fn status(&self) -> Result<Vec<GitStatusEntry>, GitError> {
        let output = self.run(&["status", "--porcelain=v1"])?;
        let mut entries = Vec::new();

        for line in output.lines() {
            if line.len() < 4 {
                continue;
            }
            let status_code = &line[..2];
            let path = line[3..].trim();

            // Handle renames: "R  old -> new"
            let actual_path = if status_code.starts_with('R') || status_code.starts_with('C') {
                path.split(" -> ").last().unwrap_or(path)
            } else {
                path
            };

            entries.push(GitStatusEntry {
                path: PathBuf::from(actual_path),
                status: FileStatus::from_status_code(status_code),
                staged: FileStatus::from_status_code(&status_code[..1]),
            });
        }

        Ok(entries)
    }

    /// Get the diff for a specific file.
    pub fn diff_file(&self, path: &Path) -> Result<Vec<DiffHunk>, GitError> {
        let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let rel = abs_path.strip_prefix(&self.repo_path).unwrap_or(&abs_path);
        let output = self.run(&["diff", "--", &rel.to_string_lossy()])?;
        parse_diff(&output)
    }

    /// Get the staged diff for a specific file.
    pub fn diff_staged(&self, path: &Path) -> Result<Vec<DiffHunk>, GitError> {
        let output = self.run(&["diff", "--cached", "--", &path.to_string_lossy()])?;
        parse_diff(&output)
    }

    /// Compute simplified hunk ranges from a diff, suitable for gutter display
    /// and hunk navigation.
    ///
    /// IMPORTANT:
    /// Git deletion hunks do not map cleanly to working-tree line ranges.
    /// Especially for EOF deletions, git may report:
    ///
    ///     @@ -151,25 +150,3 @@
    ///
    /// which visually occupies lines 150..153 in the editor even though
    /// `new_count == 3`.
    ///
    /// The gutter renderer already places `RemovedAbove` signs on the
    /// visible editor line (including EOF synthetic positions), so the
    /// hunk range must match the visible editor-space span rather than
    /// the raw new-file span.
    ///
    /// Otherwise:
    /// - gutter sign appears on line 153
    /// - next_hunk jumps to 150
    /// - revert/popup fail at 153
    ///
    pub fn hunk_ranges(hunks: &[DiffHunk]) -> Vec<HunkRange> {
        let mut ranges = Vec::new();

        for hunk in hunks {
            let has_add = hunk.lines.iter().any(|l| l.type_ == DiffLineType::Add);

            let has_del = hunk.lines.iter().any(|l| l.type_ == DiffLineType::Delete);

            let kind = if has_del && has_add {
                GitSign::Modified
            } else if has_add {
                GitSign::Added
            } else {
                GitSign::RemovedAbove
            };

            let start = hunk.new_start.saturating_sub(1);

            // Base visible span from new-file lines.
            let mut count = hunk.new_count.max(1);

            // Pure deletions visually occupy one extra editor line
            // because the deletion marker is rendered "between" lines.
            //
            // Example:
            //
            //     @@ -151,25 +150,3 @@
            //
            // new_count = 3
            //
            // but gutter marker appears on line 153 (EOF marker line),
            // so the visible editor-space range must include it.
            //
            if has_del && !has_add {
                count += 1;
            }

            ranges.push(HunkRange {
                start,
                count,
                kind,
                header: hunk.header.clone(),
            });
        }

        ranges
    }
    /// Build per-line sign map from hunk ranges (coarse: one kind per hunk).
    /// Prefer `line_signs_from_diff` for accurate per-line signs.
    pub fn line_signs(ranges: &[HunkRange]) -> std::collections::HashMap<usize, GitSign> {
        let mut signs = std::collections::HashMap::new();
        for range in ranges {
            // For RemovedAbove, we don't mark any existing lines — the removed
            // lines no longer exist in the working tree. We place a marker on
            // the first context line after deletions.
            if range.kind == GitSign::RemovedAbove {
                // Mark the first line of the hunk (which is context after deletion)
                if range.count > 0 {
                    signs.insert(range.start, GitSign::RemovedAbove);
                }
                continue;
            }
            for i in 0..range.count {
                signs.insert(range.start + i, range.kind);
            }
        }
        signs
    }

    /// Build per-line sign map directly from parsed diff hunks.
    /// This is more accurate than `line_signs` because it analyses each diff line
    /// individually, correctly handling hunks that contain a mix of additions,
    /// deletions, and modifications.
    ///
    /// Algorithm:
    /// - Walk through each diff line in order.
    /// - **Delete** lines are queued as `pending_deletes`.
    /// - **Add** lines consume a pending delete → `Modified`; otherwise → `Added`.
    /// - **Context** lines flush pending deletes → `RemovedAbove` on that context line.
    /// - After processing all lines in a hunk, flush any remaining pending deletes:
    ///   place `RemovedAbove` on the first line of the next hunk, or on the line
    ///   just past the hunk's working-tree range if no next hunk exists.

    pub fn line_signs_from_diff(hunks: &[DiffHunk]) -> std::collections::HashMap<usize, GitSign> {
        let mut signs = std::collections::HashMap::new();

        for (hunk_idx, hunk) in hunks.iter().enumerate() {
            let mut pending_deletes: usize = 0;

            for line in &hunk.lines {
                match line.type_ {
                    DiffLineType::Add => {
                        if let Some(new_ln) = line.new_lineno {
                            let sign = if pending_deletes > 0 {
                                pending_deletes -= 1;
                                GitSign::Modified
                            } else {
                                GitSign::Added
                            };
                            signs.insert(new_ln.saturating_sub(1), sign);
                        }
                    }
                    DiffLineType::Delete => {
                        pending_deletes += 1;
                    }
                    DiffLineType::Context => {
                        if pending_deletes > 0 {
                            if let Some(new_ln) = line.new_lineno {
                                signs
                                    .entry(new_ln.saturating_sub(1))
                                    .or_insert(GitSign::RemovedAbove);
                            }
                            pending_deletes = 0;
                        }
                    }
                    DiffLineType::HunkHeader => {}
                }
            }

            // Flush trailing deletes
            if pending_deletes > 0 {
                let target = if let Some(next_hunk) = hunks.get(hunk_idx + 1) {
                    next_hunk.new_start.saturating_sub(1)
                } else {
                    hunk.new_start.saturating_sub(1) + hunk.new_count
                };
                signs.entry(target).or_insert(GitSign::RemovedAbove);
            }
        }

        signs
    }

    /// Revert a single hunk by applying its reverse patch.
    /// This extracts the hunk text and pipes it through `git apply --reverse`.
    // In GitProvider::revert_hunk — replace the entire method body:
    pub fn revert_hunk(&self, path: &Path, hunk: &DiffHunk) -> Result<(), GitError> {
        // Compute the repo-relative path for the diff header.
        // git apply runs from repo_path, so the path must be relative to the repo root.
        let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let rel = match abs_path.strip_prefix(&self.repo_path) {
            Ok(p) => p.to_path_buf(),
            Err(_) => {
                // Fallback: try with normalized path separators (for Windows)
                let abs_str = abs_path.to_string_lossy().replace('\\', "/");
                let repo_str = self.repo_path.to_string_lossy().replace('\\', "/");
                if abs_str.starts_with(&repo_str) {
                    PathBuf::from(&abs_str[repo_str.len()..])
                } else {
                    abs_path.clone()
                }
            }
        };
        let rel_str = rel.to_string_lossy();

        // Build the patch text: just the hunk header + diff lines.
        let mut patch = String::new();
        patch.push_str(&format!("diff --git a/{} b/{}\n", rel_str, rel_str));
        patch.push_str(&format!("--- a/{}\n", rel_str));
        patch.push_str(&format!("+++ b/{}\n", rel_str));
        patch.push_str(&hunk.header);
        patch.push('\n');
        for line in &hunk.lines {
            match line.type_ {
                DiffLineType::Add => patch.push_str(&format!("+{}\n", line.content)),
                DiffLineType::Delete => patch.push_str(&format!("-{}\n", line.content)),
                DiffLineType::Context => patch.push_str(&format!(" {}\n", line.content)),
                DiffLineType::HunkHeader => {}
            }
        }
        let mut child = std::process::Command::new("git")
            .args(["apply", "--reverse"])
            .current_dir(&self.repo_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| GitError::CommandFailed(format!("failed to spawn git apply: {}", e)))?;

        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            stdin
                .write_all(patch.as_bytes())
                .map_err(|e| GitError::CommandFailed(format!("failed to write patch: {}", e)))?;
        }
        // stdin dropped here → pipe closes → signals EOF to git apply

        let output = child
            .wait_with_output()
            .map_err(|e| GitError::CommandFailed(format!("failed to wait for git apply: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GitError::CommandFailed(format!(
                "git apply --reverse failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Stage a file (git add).
    pub fn stage_file(&self, path: &Path) -> Result<(), GitError> {
        self.run(&["add", &path.to_string_lossy()])?;
        Ok(())
    }

    /// Unstage a file (git reset HEAD).
    pub fn unstage_file(&self, path: &Path) -> Result<(), GitError> {
        self.run(&["reset", "HEAD", &path.to_string_lossy()])?;
        Ok(())
    }

    /// Get blame information for a file.
    pub fn blame(&self, path: &Path) -> Result<Vec<BlameLine>, GitError> {
        let output = self.run(&["blame", "--porcelain", &path.to_string_lossy()])?;
        parse_blame(&output)
    }

    /// Show the content of a file at HEAD (for hunk revert).
    pub fn show_file_at_head(&self, path: &Path) -> Result<String, GitError> {
        let rel = path.to_string_lossy();
        let output = self.run(&["show", &format!("HEAD:{}", rel)])?;
        Ok(output)
    }

    /// Get recent commit log.
    pub fn log(&self, count: usize) -> Result<Vec<GitCommit>, GitError> {
        let count_str = count.to_string();
        let output = self.run(&[
            "log",
            &format!("-{}", count_str),
            "--format=%H %h %an %at %s",
        ])?;
        parse_log(&output)
    }

    /// Get the diff for a file, using `content` as the working-tree version
    /// instead of the on-disk file. This lets us diff unsaved buffer content.
    pub fn diff_buffer(&self, path: &Path, content: &str) -> Result<Vec<DiffHunk>, GitError> {
        use std::io::Write;

        let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        let rel = match abs_path.strip_prefix(&self.repo_path) {
            Ok(p) => p.to_path_buf(),
            Err(_) => {
                let abs_str = abs_path.to_string_lossy().replace('\\', "/");
                let repo_str = self.repo_path.to_string_lossy().replace('\\', "/");
                if abs_str.starts_with(&repo_str) {
                    PathBuf::from(&abs_str[repo_str.len()..])
                } else {
                    abs_path.clone()
                }
            }
        };

        let head_ref = format!("HEAD:{}", rel.display());

        let head_bytes = Command::new("git")
            .args(["show", &head_ref])
            .current_dir(&self.repo_path)
            .output()?;

        let head_content = if head_bytes.status.success() {
            String::from_utf8_lossy(&head_bytes.stdout).into_owned()
        } else {
            let stderr = String::from_utf8_lossy(&head_bytes.stderr);
            String::new()
        };

        // Write HEAD content to first temp file.
        let mut head_tmp = tempfile::NamedTempFile::new().map_err(GitError::Io)?;
        head_tmp
            .write_all(head_content.as_bytes())
            .map_err(GitError::Io)?;
        head_tmp.flush().map_err(GitError::Io)?;

        // Write buffer content to second temp file.
        let mut buf_tmp = tempfile::NamedTempFile::new().map_err(GitError::Io)?;
        buf_tmp
            .write_all(content.as_bytes())
            .map_err(GitError::Io)?;
        buf_tmp.flush().map_err(GitError::Io)?;

        let output = Command::new("git")
            .args([
                "diff",
                "--no-index",
                "-U3",
                &head_tmp.path().to_string_lossy(),
                &buf_tmp.path().to_string_lossy(),
            ])
            .current_dir(&self.repo_path)
            .output()?;

        // git diff --no-index exits 1 when differences exist — not an error.
        let stdout = String::from_utf8(output.stdout)?;
        parse_diff(&stdout)
    }
    /// Normalize raw git diff hunks into editor-space hunks.
    ///
    /// This is the canonical UI representation.
    ///
    /// IMPORTANT:
    /// Git diff coordinates are NOT identical to editor-space coordinates,
    /// especially for deletions and EOF delete hunks.
    ///
    /// This function converts raw diff geometry into:
    /// - visual editor ranges
    /// - gutter signs
    /// - navigation spans
    ///
    /// so every editor subsystem uses the SAME geometry.
    pub fn build_editor_hunks(hunks: &[DiffHunk]) -> Vec<EditorHunk> {
        let mut result = Vec::new();

        for (hunk_idx, hunk) in hunks.iter().enumerate() {
            let mut signs = Vec::new();

            let mut pending_deletes = 0usize;

            let mut min_line = usize::MAX;
            let mut max_line = 0usize;

            let mut has_add = false;
            let mut has_del = false;

            for line in &hunk.lines {
                match line.type_ {
                    DiffLineType::Add => {
                        has_add = true;

                        if let Some(new_ln) = line.new_lineno {
                            let line_idx = new_ln.saturating_sub(1);

                            let kind = if pending_deletes > 0 {
                                pending_deletes -= 1;
                                GitSign::Modified
                            } else {
                                GitSign::Added
                            };

                            signs.push(EditorSign {
                                line: line_idx,
                                kind,
                            });

                            min_line = min_line.min(line_idx);
                            max_line = max_line.max(line_idx);
                        }
                    }

                    DiffLineType::Delete => {
                        has_del = true;
                        pending_deletes += 1;
                    }

                    DiffLineType::Context => {
                        if let Some(new_ln) = line.new_lineno {
                            let line_idx = new_ln.saturating_sub(1);

                            min_line = min_line.min(line_idx);
                            max_line = max_line.max(line_idx);

                            if pending_deletes > 0 {
                                signs.push(EditorSign {
                                    line: line_idx,
                                    kind: GitSign::RemovedAbove,
                                });

                                pending_deletes = 0;
                            }
                        }
                    }

                    DiffLineType::HunkHeader => {}
                }
            }

            // Flush trailing deletes (EOF delete handling)
            if pending_deletes > 0 {
                let target = if let Some(next_hunk) = hunks.get(hunk_idx + 1) {
                    next_hunk.new_start.saturating_sub(1)
                } else {
                    hunk.new_start.saturating_sub(1) + hunk.new_count
                };

                signs.push(EditorSign {
                    line: target,
                    kind: GitSign::RemovedAbove,
                });

                min_line = min_line.min(target);
                max_line = max_line.max(target);
            }

            let kind = if has_add && has_del {
                GitSign::Modified
            } else if has_add {
                GitSign::Added
            } else {
                GitSign::RemovedAbove
            };

            // Fallback safety
            if min_line == usize::MAX {
                min_line = hunk.new_start.saturating_sub(1);
                max_line = min_line;
            }

            result.push(EditorHunk {
                start: min_line,
                end: max_line + 1,
                kind,
                signs,
                diff: hunk.clone(),
            });
        }

        result
    }
}

// ── Parsing helpers ─────────────────────────────────────────────────

/// Parse git diff output into hunks and lines.
/// Parse git diff output into hunks and lines.
fn parse_diff(output: &str) -> Result<Vec<DiffHunk>, GitError> {
    let mut hunks = Vec::new();
    let mut current_hunk: Option<DiffHunk> = None;
    let mut old_lineno: Option<usize> = None;
    let mut new_lineno: Option<usize> = None;

    for line in output.lines() {
        if line.starts_with("@@ ") {
            // Finish the previous hunk.
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }

            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            let header = line.to_string();
            let parts: Vec<&str> = line.split_whitespace().collect();
            let mut old_start = 1usize;
            let mut old_count = 1usize;
            let mut new_start = 1usize;
            let mut new_count = 1usize;

            if parts.len() >= 3 {
                if let Some(old) = parts[1].strip_prefix('-') {
                    let nums: Vec<&str> = old.split(',').collect();
                    old_start = nums[0].parse().unwrap_or(1);
                    old_count = nums.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
                }
                if let Some(new) = parts[2].strip_prefix('+') {
                    let nums: Vec<&str> = new.split(',').collect();
                    new_start = nums[0].parse().unwrap_or(1);
                    new_count = nums.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
                }
            }

            old_lineno = Some(old_start);
            new_lineno = Some(new_start);

            current_hunk = Some(DiffHunk {
                header,
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
        } else if let Some(ref mut hunk) = current_hunk {
            let (type_, content, old_ln, new_ln) = if line.starts_with('+') {
                let ln = new_lineno;
                if let Some(ref mut n) = new_lineno {
                    *n += 1;
                }
                (DiffLineType::Add, line[1..].to_string(), None, ln)
            } else if line.starts_with('-') {
                let ln = old_lineno;
                if let Some(ref mut n) = old_lineno {
                    *n += 1;
                }
                (DiffLineType::Delete, line[1..].to_string(), ln, None)
            } else {
                let old_ln = old_lineno;
                let new_ln = new_lineno;
                if let Some(ref mut n) = old_lineno {
                    *n += 1;
                }
                if let Some(ref mut n) = new_lineno {
                    *n += 1;
                }
                (
                    DiffLineType::Context,
                    if line.starts_with(' ') {
                        line[1..].to_string()
                    } else {
                        line.to_string()
                    },
                    old_ln,
                    new_ln,
                )
            };

            hunk.lines.push(DiffLine {
                type_,
                content,
                new_lineno: new_ln,
                old_lineno: old_ln,
            });
        }
    }

    // Don't forget the last hunk.
    if let Some(hunk) = current_hunk.take() {
        hunks.push(hunk);
    }

    Ok(hunks)
}

/// Parse git blame --porcelain output.
fn parse_blame(output: &str) -> Result<Vec<BlameLine>, GitError> {
    let mut lines = Vec::new();
    let mut current: Option<BlameLine> = None;

    for line in output.lines() {
        if line.is_empty() {
            continue;
        }

        // Header line: <hash> <old_lineno> <new_lineno> <count>
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            if let Some(bl) = current.take() {
                lines.push(bl);
            }
            current = Some(BlameLine {
                commit: parts[0].to_string(),
                author: String::new(),
                author_email: String::new(),
                author_time: 0,
                lineno: parts[2].parse().unwrap_or(0),
                content: String::new(),
            });
        } else if let Some(ref mut bl) = current {
            if line.starts_with("author ") {
                bl.author = line[7..].to_string();
            } else if line.starts_with("author-mail ") {
                bl.author_email = line[12..]
                    .trim_matches(|c: char| c == '<' || c == '>')
                    .to_string();
            } else if line.starts_with("author-time ") {
                bl.author_time = line[12..].parse().unwrap_or(0);
            } else if line.starts_with('\t') {
                bl.content = line[1..].to_string();
            }
        }
    }

    if let Some(bl) = current.take() {
        lines.push(bl);
    }

    Ok(lines)
}

/// Parse git log output.
fn parse_log(output: &str) -> Result<Vec<GitCommit>, GitError> {
    let mut commits = Vec::new();

    for line in output.lines() {
        let parts: Vec<&str> = line.splitn(5, ' ').collect();
        if parts.len() >= 5 {
            commits.push(GitCommit {
                hash: parts[0].to_string(),
                short_hash: parts[1].to_string(),
                author: parts[2].to_string(),
                time: parts[3].parse().unwrap_or(0),
                message: parts[4].to_string(),
            });
        }
    }

    Ok(commits)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a DiffLine quickly.
    fn dl(ty: DiffLineType, content: &str, old: Option<usize>, new: Option<usize>) -> DiffLine {
        DiffLine {
            type_: ty,
            content: content.to_string(),
            old_lineno: old,
            new_lineno: new,
        }
    }

    #[test]
    fn test_line_signs_pure_add() {
        // @@ -1,5 +1,6 @@: 2 context, 1 add, 2 context
        let hunks = vec![DiffHunk {
            header: "@@ -1,5 +1,6 @@".to_string(),
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 6,
            lines: vec![
                dl(DiffLineType::Context, "line1", Some(1), Some(1)),
                dl(DiffLineType::Context, "line2", Some(2), Some(2)),
                dl(DiffLineType::Add, "", None, Some(3)),
                dl(DiffLineType::Context, "line3", Some(3), Some(4)),
                dl(DiffLineType::Context, "line4", Some(4), Some(5)),
            ],
        }];
        let signs = GitProvider::line_signs_from_diff(&hunks);
        // Line 3 (0-based = 2) should be Added
        assert_eq!(signs.get(&2), Some(&GitSign::Added));
        // Context lines should have no sign
        assert_eq!(signs.get(&0), None);
        assert_eq!(signs.get(&1), None);
        assert_eq!(signs.get(&3), None);
        assert_eq!(signs.get(&4), None);
    }

    #[test]
    fn test_line_signs_modify() {
        // -old_line / +new_line → Modified
        let hunks = vec![DiffHunk {
            header: "@@ -1,3 +1,3 @@".to_string(),
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            lines: vec![
                dl(DiffLineType::Delete, "old_line", Some(1), None),
                dl(DiffLineType::Add, "new_line", None, Some(1)),
                dl(DiffLineType::Context, "context", Some(2), Some(2)),
            ],
        }];
        let signs = GitProvider::line_signs_from_diff(&hunks);
        // Line 1 (0-based = 0) should be Modified
        assert_eq!(signs.get(&0), Some(&GitSign::Modified));
        assert_eq!(signs.get(&1), None); // context
    }

    #[test]
    fn test_line_signs_delete_with_context() {
        // Delete a line, context after → RemovedAbove on context line
        let hunks = vec![DiffHunk {
            header: "@@ -2,3 +1,2 @@".to_string(),
            old_start: 2,
            old_count: 3,
            new_start: 1,
            new_count: 2,
            lines: vec![
                dl(DiffLineType::Context, "keep", Some(2), Some(1)),
                dl(DiffLineType::Delete, "removed", Some(3), None),
                dl(DiffLineType::Context, "after", Some(4), Some(2)),
            ],
        }];
        let signs = GitProvider::line_signs_from_diff(&hunks);
        // Line 2 (0-based = 1) should be RemovedAbove
        assert_eq!(signs.get(&1), Some(&GitSign::RemovedAbove));
        assert_eq!(signs.get(&0), None); // context
    }

    #[test]
    fn test_line_signs_trailing_deletes() {
        // Hunk ends with deletes, no context after → RemovedAbove on next line
        let hunks = vec![DiffHunk {
            header: "@@ -3,3 +2,1 @@".to_string(),
            old_start: 3,
            old_count: 3,
            new_start: 2,
            new_count: 1,
            lines: vec![
                dl(DiffLineType::Context, "keep", Some(3), Some(2)),
                dl(DiffLineType::Delete, "removed1", Some(4), None),
                dl(DiffLineType::Delete, "removed2", Some(5), None),
            ],
        }];
        let signs = GitProvider::line_signs_from_diff(&hunks);
        // Should place RemovedAbove on line past the hunk range:
        // new_start - 1 + new_count = 1 + 1 = 2
        assert_eq!(signs.get(&2), Some(&GitSign::RemovedAbove));
    }

    #[test]
    fn test_line_signs_mixed_modify_and_add() {
        // 2 deletes, 4 adds → first 2 adds are Modified, last 2 are Added
        let hunks = vec![DiffHunk {
            header: "@@ -1,6 +1,8 @@".to_string(),
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 8,
            lines: vec![
                dl(DiffLineType::Delete, "old1", Some(1), None),
                dl(DiffLineType::Delete, "old2", Some(2), None),
                dl(DiffLineType::Add, "new1", None, Some(1)),
                dl(DiffLineType::Add, "new2", None, Some(2)),
                dl(DiffLineType::Add, "new3", None, Some(3)),
                dl(DiffLineType::Add, "new4", None, Some(4)),
                dl(DiffLineType::Context, "ctx", Some(3), Some(5)),
            ],
        }];
        let signs = GitProvider::line_signs_from_diff(&hunks);
        assert_eq!(signs.get(&0), Some(&GitSign::Modified)); // new1
        assert_eq!(signs.get(&1), Some(&GitSign::Modified)); // new2
        assert_eq!(signs.get(&2), Some(&GitSign::Added)); // new3
        assert_eq!(signs.get(&3), Some(&GitSign::Added)); // new4
        assert_eq!(signs.get(&4), None); // context
    }

    #[test]
    fn test_parse_diff_real() {
        let output = "diff --git a/test.rs b/test.rs\n\
                      index abc..def 100644\n\
                      --- a/test.rs\n\
                      +++ b/test.rs\n\
                      @@ -1,5 +1,6 @@\n\
                      line1\n\
                      line2\n\
                      +added\n\
                      line3\n\
                      line4\n";
        let hunks = parse_diff(output).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].new_count, 6);
        assert_eq!(hunks[0].lines.len(), 5); // 2 context + 1 add + 2 context

        let signs = GitProvider::line_signs_from_diff(&hunks);
        assert_eq!(signs.get(&2), Some(&GitSign::Added)); // added line at 0-based 2
    }
}
