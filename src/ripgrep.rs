//! Ripgrep integration — search across project, display results in a special buffer.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use unicode_segmentation::UnicodeSegmentation;

// ── Ripgrep  result ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RipgrepResult {
    /// Full absolute path to the file containing the match.
    pub file_path: PathBuf,
    /// 1-based line number where the match occurs.
    pub line_number: usize,
    /// The full line content.
    pub line_content: String,
}

// ── Ripgrep output ──────────────────────────────────────────────────

/// Parsed ripgrep output with results and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RipgrepOutput {
    /// All matching lines, in rg output order.
    pub results: Vec<RipgrepResult>,
    /// The search pattern used.
    pub pattern: String,
    /// The root directory that was searched (always absolute).
    pub root_dir: PathBuf,
}

impl RipgrepOutput {
    /// Format results for display in a ripgrep navigation buffer.
    ///
    /// Groups results by file, showing the full path as a header,
    /// then each matching line with its line number.
    pub fn format_for_buffer(&self) -> String {
        if self.results.is_empty() {
            return format!(
                "  No results found for: '{}'\n  in: {}\n",
                self.pattern,
                self.root_dir.display()
            );
        }

        let mut output = String::new();
        let mut current_file: Option<&Path> = None;
        let match_count = self.results.len();
        let mut file_count = 0usize;

        for result in &self.results {
            if current_file != Some(result.file_path.as_path()) {
                if current_file.is_some() {
                    output.push('\n');
                }
                output.push_str(&format!("{}:\n", result.file_path.display()));
                current_file = Some(&result.file_path);
                file_count += 1;
            }
            output.push_str(&format!(
                "{}: {}\n",
                result.line_number, result.line_content
            ));
        }

        let mut header = format!(
            "  [RG] '{}' — {} matches in {} files\n",
            self.pattern, match_count, file_count
        );
        header.push_str(&format!("  {}\n\n", "─".repeat(40)));
        header.push_str(&output);

        header
    }

    /// Build a line-indexed map for the formatted buffer.
    pub fn build_line_map(&self) -> Vec<Option<usize>> {
        let mut line_map = Vec::new();
        let mut current_file: Option<&Path> = None;

        line_map.push(None);
        line_map.push(None);
        line_map.push(None);

        for (idx, result) in self.results.iter().enumerate() {
            if current_file != Some(result.file_path.as_path()) {
                line_map.push(None);
                if current_file.is_some() {
                    line_map.push(None);
                }
                current_file = Some(&result.file_path);
            }
            line_map.push(Some(idx));
        }

        line_map
    }
}

// ── Run ripgrep ─────────────────────────────────────────────────────

/// Run ripgrep with the given pattern in the specified directory.
///
/// All file paths in results are converted to absolute paths.
pub fn run_ripgrep(pattern: &str, root_dir: &Path) -> Result<RipgrepOutput, String> {
    if pattern.is_empty() {
        return Err("Empty search pattern".to_string());
    }

    let root_dir = if root_dir.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else if root_dir.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(root_dir))
            .unwrap_or_else(|_| root_dir.to_path_buf())
    } else {
        root_dir.to_path_buf()
    };

    let output = Command::new("rg")
        .args([
            "--line-number",
            "--no-heading",
            "--with-filename",
            "--color=never",
            "--max-count=500",
            "--max-filesize=10M",
            pattern,
            ".",
        ])
        .current_dir(&root_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            format!(
                "Failed to run rg: {}. Is ripgrep installed? (https://github.com/BurntSushi/ripgrep)",
                e
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(1) {
            return Ok(RipgrepOutput {
                results: Vec::new(),
                pattern: pattern.to_string(),
                root_dir,
            });
        }
        return Err(format!("rg error: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();

    for line in stdout.lines() {
        if let Some((path_str, rest)) = line.split_once(':') {
            if let Some((line_num_str, content)) = rest.split_once(':') {
                if let Ok(line_num) = line_num_str.parse::<usize>() {
                    let file_path = PathBuf::from(path_str);
                    let absolute_path = if file_path.is_relative() {
                        root_dir.join(&file_path)
                    } else {
                        file_path
                    };
                    results.push(RipgrepResult {
                        file_path: absolute_path,
                        line_number: line_num,
                        line_content: content.to_string(),
                    });
                }
            }
        }
    }

    Ok(RipgrepOutput {
        results,
        pattern: pattern.to_string(),
        root_dir,
    })
}

// ── Word extraction ─────────────────────────────────────────────────

/// Get the word under the cursor from a line of text at a given column.
pub fn word_under_cursor(line: &str, col: usize) -> String {
    let graphemes: Vec<&str> = line.graphemes(true).collect();
    if col >= graphemes.len() {
        return String::new();
    }

    if !is_word_char(graphemes[col]) {
        return String::new();
    }

    let mut start = col;
    let mut end = col;

    while start > 0 && is_word_char(graphemes[start - 1]) {
        start -= 1;
    }
    while end < graphemes.len() && is_word_char(graphemes[end]) {
        end += 1;
    }

    graphemes[start..end].join("")
}

fn is_word_char(g: &str) -> bool {
    g.chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_')
        .unwrap_or(false)
}

/// Escape special regex characters for a literal search.
pub fn escape_regex(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len() * 2);
    for c in pattern.chars() {
        match c {
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' | '^' | '$'
            | '-' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}
