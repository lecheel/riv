use crate::buffer::Language;
use crate::rounded_box::truncate_to_width;
use std::path::Path;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

// ── Helper: word character check ───────────────────────────────────
/// Check if a tree-sitter node kind is a string or comment
/// (braces inside these should not affect indentation).
pub fn is_string_or_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal" | "raw_string_literal" | "char_literal"  // Rust
        | "line_comment" | "block_comment" | "doc_comment"
        | "string" | "template_string" | "regex" | "regex_pattern" // JS/TS/Python
        | "fstring" | "fstring_start" | "fstring_end"
        | "string_start" | "string_end" | "string_content"
        | "comment" | "comment_content"
    )
}

/// Check if a grapheme cluster is a word character.
/// Uses Unicode character properties for better accuracy.
pub fn is_word_char(g: &str) -> bool {
    if g.is_empty() {
        return false;
    }

    // For multi-character graphemes (like flags, emoji sequences),
    // treat the entire grapheme as a single unit
    let chars: Vec<char> = g.chars().collect();

    // Handle common word character cases
    match chars.len() {
        1 => {
            let c = chars[0];
            c.is_alphanumeric() || c == '_' || c == '-' // Add hyphen for hyphenated words
        }
        _ => {
            // For graphemes like "ﬁ" (ligature) or emoji, check if first char is alphanumeric
            // But generally treat complex graphemes as non-word characters
            chars[0].is_alphanumeric()
        }
    }
}

/// Render help entries into display lines, grouped by category.
/// Now properly handles Unicode width for alignment.
pub fn render_help_entries(entries: &[crate::keybind::HelpEntry], max_width: u16) -> Vec<String> {
    use crate::action::ActionCategory;

    let mut lines = Vec::new();
    let max_width_usize = max_width as usize;

    // First pass: find the max display width of keys in each category
    let mut categories: Vec<(ActionCategory, Vec<&crate::keybind::HelpEntry>)> = Vec::new();
    for entry in entries {
        if categories.last().map(|(c, _)| *c) != Some(entry.category) {
            categories.push((entry.category, Vec::new()));
        }
        categories.last_mut().unwrap().1.push(entry);
    }

    for (category, cat_entries) in &categories {
        // Category header
        let header = format!("── {} ──", category.header());
        lines.push(truncate_to_width(&header, max_width_usize).to_string());
        lines.push(String::new());

        // Find max key display width using Unicode width
        let max_keys_width = cat_entries
            .iter()
            .map(|e| UnicodeWidthStr::width(e.keys.as_str()))
            .max()
            .unwrap_or(0)
            .min(30); // cap at 30 columns

        for entry in cat_entries {
            let key_width = UnicodeWidthStr::width(entry.keys.as_str());
            let padding_needed = max_keys_width.saturating_sub(key_width);
            let padding = " ".repeat(padding_needed);

            // Build the line with proper spacing
            let line = format!("  {}{}  {}", entry.keys, padding, entry.description);

            // Truncate using display width, not character count
            let truncated = truncate_to_width(&line, max_width_usize);
            lines.push(truncated.to_string());
        }

        lines.push(String::new());
    }

    // Remove trailing blank line
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }

    lines
}

/// Helper function (outside the trait impl, at bottom of file)
pub fn comment_chars(language: &Option<Language>) -> Option<(&'static str, &'static str)> {
    match language {
        Some(Language::Rust) | Some(Language::JavaScript) | Some(Language::TypeScript) => {
            Some(("// ", ""))
        }
        Some(Language::Python) => Some(("# ", "")),
        Some(Language::PlainText) | None => None,
        _ => Some(("// ", "")), // fallback
    }
}

/// Extract the leading whitespace (indentation) from a line of text.
/// Maintains grapheme cluster boundaries.
pub fn get_line_indent(text: &str) -> String {
    let mut indent = String::new();
    for g in text.graphemes(true) {
        // Check both spaces and tabs, but also other Unicode whitespace?
        if g == " "
            || g == "\t"
            || (g.chars().next().map(|c| c.is_whitespace()).unwrap_or(false)
                && g != "\n"
                && g != "\r")
        {
            indent.push_str(g);
        } else {
            break;
        }
    }
    indent
}

/// Sanitize a string for single-line display in the status/cmd bar.
/// Now uses Unicode display width consistently for all measurements.
pub fn sanitize_single_line(s: &str, max_chars: usize, max_width: usize) -> String {
    if s.is_empty() {
        return String::new();
    }

    let mut result = String::with_capacity(s.len().min(max_chars * 3));
    let mut char_count = 0;
    let mut grapheme_count = 0;

    // Process by grapheme clusters for better Unicode handling
    for g in s.graphemes(true) {
        if grapheme_count >= max_chars {
            result.push('…');
            break;
        }

        // Replace newlines with spaces
        if g == "\n" || g == "\r" {
            result.push(' ');
        } else {
            result.push_str(g);
        }

        grapheme_count += 1;
        char_count += g.chars().count();
    }

    // Finally, ensure it fits within display width
    truncate_to_width(&result, max_width).to_string()
}

/// Alternative: More aggressive version that uses display width for all measurements
/// Useful for status bars where visual width is most important.
pub fn sanitize_single_line_by_width(s: &str, max_display_width: usize) -> String {
    if s.is_empty() {
        return String::new();
    }

    let mut result = String::new();
    let mut current_width = 0;

    for g in s.graphemes(true) {
        let g_width = UnicodeWidthStr::width(g);

        // Check if adding this grapheme would exceed max width
        if current_width + g_width > max_display_width.saturating_sub(1) {
            result.push('…');
            break;
        }

        // Replace newlines with spaces
        if g == "\n" || g == "\r" {
            result.push(' ');
            current_width += 1;
        } else {
            result.push_str(g);
            current_width += g_width;
        }
    }

    result
}

/// Parse a shortcut key string (e.g. "gx" or "f") into a sequence of Keys.
/// Currently only supports simple character sequences (no modifiers like Ctrl).
pub fn parse_shortcut_keys(s: &str) -> Option<Vec<crate::terminal::Key>> {
    if s.is_empty() {
        return None;
    }
    let keys: Vec<crate::terminal::Key> = s
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(crate::terminal::Key::Char)
        .collect();
    if keys.is_empty() {
        None
    } else {
        Some(keys)
    }
}

/// Format a sequence of keys for display (e.g. [Char('g'), Char('x')] -> "gx").
pub fn format_shortcut_keys(keys: &[crate::terminal::Key]) -> String {
    keys.iter()
        .map(|k| match k {
            crate::terminal::Key::Char(c) => c.to_string(),
            _ => format!("{:?}", k),
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Convert a grapheme-based column to a char offset within a line.
pub fn grapheme_col_to_char_offset(rope: &ropey::Rope, line: usize, grapheme_col: usize) -> usize {
    // Convert the RopeSlice to a String so we can use the graphemes() method
    let line_text = rope.line(line).to_string();
    let mut offset = 0;
    let mut g_count = 0;
    for g in line_text.graphemes(true) {
        if g_count >= grapheme_col {
            break;
        }
        offset += g.chars().count(); // char count, not byte count
        g_count += 1;
    }
    offset
}

pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let len_a = a_chars.len();
    let len_b = b_chars.len();

    // Distance matrix: (len_a + 1) x (len_b + 1)
    let mut dist = vec![vec![0; len_b + 1]; len_a + 1];

    for i in 0..=len_a {
        dist[i][0] = i;
    }
    for j in 0..=len_b {
        dist[0][j] = j;
    }

    for i in 1..=len_a {
        for j in 1..=len_b {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            dist[i][j] = (dist[i - 1][j] + 1) // deletion
                .min(dist[i][j - 1] + 1) // insertion
                .min(dist[i - 1][j - 1] + cost); // substitution
        }
    }

    dist[len_a][len_b]
}

pub fn find_git_root(start_dir: &Path) -> Option<PathBuf> {
    let effective_dir = if start_dir.is_file() {
        start_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| Path::new(".").to_path_buf())
    } else if start_dir.exists() {
        start_dir.to_path_buf()
    } else {
        // File/dir doesn't exist: try its parent, or fall back to CWD
        start_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| Path::new(".").to_path_buf())
    };

    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&effective_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if path_str.is_empty() {
                None
            } else {
                let p = PathBuf::from(path_str);
                // Canonicalize the git root so symlinks are resolved,
                // matching the absolute paths derived in display_path.
                Some(std::fs::canonicalize(&p).unwrap_or(p))
            }
        })
}

/// Return a display-friendly path: if the file lives inside a git repo,
/// strip the repo-root prefix so `/home/user/project/src/main.rs` becomes
/// `src/main.rs`.  Otherwise return the path unchanged.
pub fn display_path(path: &Path, git_root: Option<&Path>) -> String {
    if let Some(root) = git_root {
        // We need an absolute path to successfully strip the absolute git root prefix.
        // If the path is already absolute, use it; otherwise, resolve it against CWD.
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };

        if let Ok(relative) = abs_path.strip_prefix(root) {
            let rel = relative.to_string_lossy();
            let trimmed = rel.trim_start_matches(std::path::MAIN_SEPARATOR);
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    path.to_string_lossy().to_string()
}

/// Extract the command verb from a command-line buffer string.
///
/// `:e /tmp/foo`   → `"e"`
/// `:vs src/`      → `"vs"`
/// `e /tmp/foo`    → `"e"`
/// `/tmp/foo`      → `""` (no verb, bare path — leave untouched)
pub fn extract_command_prefix(buf: &str) -> &str {
    let s = buf.trim_start_matches(':').trim_start();
    // Split on first whitespace
    match s.split_once(|c: char| c.is_whitespace()) {
        Some((verb, _arg)) => verb,
        None => "", // No space yet → no arg portion exists, completion replaces all
    }
}
