// src/guide.rs
//! Code architecture guide — searchable checkpoints for navigation.
//!
//! Reads `.riv/guide.toml` and provides a popup for searching/jumping
//! to structural checkpoints within source files.
//!
//! Supports inline source markers synced via `:guide update`:
//!
//! Structured:
//!   //-- guide: kind=fn label=my_func anchor="fn my_func" -- Description --//
//!
//! Plain (auto-parses label and anchor):
//!   //-- process_action Action::RipgrepLast (anchor Action::RipgrepLast) --//

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::popup::Scrollable;

// ── Data structures ──────────────────────────────────────────────

/// A single checkpoint in the code guide.
#[derive(Debug, Clone)]
pub struct GuideEntry {
    /// Source file path relative to repo root.
    pub file: String,
    /// Substring to search for when navigating (NOT a line number).
    pub anchor: String,
    /// Display kind: struct, enum, fn, impl, trait, section, match, const, type, macro, field.
    pub kind: String,
    /// Short display name for the popup.
    pub label: String,
    /// Brief description of what this section is/does.
    pub desc: String,
    /// Tags for filtering (comma-separated in TOML, split on load).
    pub tags: Vec<String>,
    /// Implementation hint (shown in doc popup on the right).
    pub hint: Option<String>,
}

/// Parsed structured guide marker from source code.
#[derive(Debug, Clone)]
pub struct ParsedGuideMarker {
    pub kind: String,
    pub label: String,
    pub anchor: String,
    pub desc: String,
}

/// Result of a guide sync operation.
#[derive(Debug, Default)]
pub struct SyncResult {
    pub added: usize,
    pub updated: usize,
}

/// The loaded code guide with filter/search state.
#[derive(Debug)]
pub struct Guide {
    /// All entries loaded from .riv/guide.toml.
    pub entries: Vec<GuideEntry>,
    /// Indices into `entries` matching the current filter.
    pub filtered: Vec<usize>,
    /// Current filter string.
    pub filter: String,
    /// Selected index within `filtered`.
    pub selected: usize,
    /// Scroll offset within `filtered`.
    pub scroll: usize,
    /// Root directory of the repo (for resolving file paths).
    pub root: PathBuf,

    // ── Auto-reload state ──
    /// Last known modification time of guide.toml.
    last_modified: Option<SystemTime>,
    /// Last time we checked the file's mtime.
    last_check_time: Option<Instant>,
    /// Debounce interval (milliseconds) between file checks.
    check_interval_ms: u64,
}

// ── TOML helpers ──────────────────────────────────────────────────

/// Escape a string for safe inclusion in a TOML double-quoted value.
fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

// ── Marker parsing ────────────────────────────────────────────────

/// Extract a key=value token from a string.
/// Handles both quoted and unquoted values.
fn extract_token(remaining: &str) -> Option<(String, &str)> {
    let remaining = remaining.trim_start();
    if remaining.is_empty() {
        return None;
    }

    if remaining.starts_with('"') {
        // Quoted value
        let end = remaining[1..].find('"')? + 1;
        let value = remaining[1..end].to_string();
        let rest = &remaining[end + 1..];
        Some((value, rest))
    } else {
        // Unquoted value
        let end = remaining
            .find(|c: char| c.is_whitespace())
            .unwrap_or(remaining.len());
        let value = remaining[..end].to_string();
        let rest = &remaining[end..];
        Some((value, rest))
    }
}

/// Extract a structured guide marker from a source line.
///
/// Supported formats:
///
/// 1. Structured:
///    //-- guide: kind=<kind> label=<label> anchor=<anchor> -- <desc> --//
///
/// 2. Plain (auto-extracts label and uses the whole comment as anchor):
///    //-- process_action Action::RipgrepLast (anchor Action::RipgrepLast) --//
fn extract_guide_marker(line: &str) -> Option<ParsedGuideMarker> {
    let line = line.trim();

    // ── 1. Structured format: guide: ──────────────────────────────
    let prefix = "guide:";
    if let Some(start) = line.find(prefix) {
        let mut remaining = &line[start + prefix.len()..];

        let mut kind = String::new();
        let mut label = String::new();
        let mut anchor = String::new();

        // Parse key=value pairs until we hit "--"
        loop {
            remaining = remaining.trim_start();
            if remaining.is_empty() || remaining.starts_with("--") {
                remaining = remaining.strip_prefix("--").unwrap_or(remaining);
                break;
            }

            // Must be a key=value pair
            let eq_idx = remaining.find('=')?;
            let key = &remaining[..eq_idx];
            remaining = &remaining[eq_idx + 1..];

            // Extract value
            let (value, rest) = extract_token(remaining)?;
            remaining = rest;

            match key {
                "kind" => kind = value,
                "label" => label = value,
                "anchor" => anchor = value,
                _ => {} // ignore unknown keys
            }
        }

        if label.is_empty() {
            return None; // label is required
        }

        // The rest is the description, up to the closing --// or */
        let desc = if let Some(end_idx) = remaining.find("--//") {
            remaining[..end_idx].trim().to_string()
        } else {
            remaining.trim().to_string()
        };

        let anchor = if anchor.is_empty() {
            label.clone()
        } else {
            anchor
        };
        // adjust plain comment using empty kind:w
        let kind = if kind.is_empty() {
            "usr".to_string()
        } else {
            kind
        };

        return Some(ParsedGuideMarker {
            kind,
            label,
            anchor,
            desc,
        });
    }

    // ── 2. Plain format: //-- ... --// ──────────────
    // The anchor becomes the exact searchable string (e.g., "-- <text> --")
    // The label/desc become the text with the (anchor ...) directive stripped.
    let (inner, anchor) = if let Some(start) = line.find("//--") {
        let rest = &line[start + 4..];
        if let Some(end) = rest.find("--//") {
            let inner = rest[..end].trim();
            // Use the exact marker text from the source line as the anchor.
            // This guarantees find_anchor_line will match, regardless of
            // whether the user wrote //-- text --// or //-- text--//
            let anchor = line[start..start + 4 + end + 3].to_string();
            (inner.to_string(), anchor)
        } else {
            return None;
        }
    } else {
        return None;
    };

    if inner.is_empty() {
        return None;
    }

    // Strip the (anchor ...) directive from the inner text to form label/desc
    let mut label_text = inner.clone();
    if let Some(anchor_start) = inner.find("(anchor") {
        let directive_substring = &inner[anchor_start..];
        if let Some(close_paren) = directive_substring.find(')') {
            label_text = format!(
                "{}{}",
                &inner[..anchor_start],
                &directive_substring[close_paren + 1..]
            );
        }
    }

    let label = label_text.trim().to_string();
    let desc = "".to_string();

    if label.is_empty() {
        return None;
    }

    Some(ParsedGuideMarker {
        kind: "fn".to_string(),
        label,
        anchor,
        desc,
    })
}
// ── Guide implementation ──────────────────────────────────────────

impl Guide {
    /// Find the project root using git.
    fn find_root() -> Option<PathBuf> {
        crate::misc::find_git_root(Path::new("."))
    }

    /// Load the guide from `.riv/guide.toml`, using git root discovery.
    /// Auto-creates an empty guide if the file doesn't exist yet.
    pub fn load() -> Self {
        let root = Self::find_root().unwrap_or_else(|| {
            // Fallback: walk up from CWD looking for .riv/ or .git/
            let mut dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            loop {
                if dir.join(".riv").exists() || dir.join(".git").exists() {
                    return dir;
                }
                if !dir.pop() {
                    return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                }
            }
        });
        Self::load_from(&root)
    }

    /// Load the guide using a specific root directory.
    pub fn load_from(root: &Path) -> Self {
        let path = root.join(".riv").join("guide.toml");
        let last_modified = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        // parse_file returns empty Vec if file doesn't exist
        let entries = Self::parse_file(&path);
        let filtered: Vec<usize> = (0..entries.len()).collect();
        Self {
            entries,
            filtered,
            filter: String::new(),
            selected: 0,
            scroll: 0,
            root: root.to_path_buf(),
            last_modified,
            last_check_time: Some(Instant::now()),
            check_interval_ms: 1000, // Check every 1 second
        }
    }

    /// Parse the TOML guide file.
    fn parse_file(path: &Path) -> Vec<GuideEntry> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Vec::new(), // File missing → empty entries
        };

        let mut entries = Vec::new();

        if let Ok(value) = content.parse::<toml::Value>() {
            if let Some(checkpoints) = value.get("checkpoint").and_then(|v| v.as_array()) {
                for cp in checkpoints {
                    let file = cp
                        .get("file")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let anchor = cp
                        .get("anchor")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let kind = cp
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("fn")
                        .to_string();
                    let label = cp
                        .get("label")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let desc = cp
                        .get("desc")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let tags: Vec<String> = cp
                        .get("tags")
                        .and_then(|v| v.as_str())
                        .map(|s| {
                            s.split(',')
                                .map(|t| t.trim().to_lowercase())
                                .filter(|t| !t.is_empty())
                                .collect()
                        })
                        .unwrap_or_default();
                    let hint = cp
                        .get("hint")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    if !anchor.is_empty() && !label.is_empty() {
                        entries.push(GuideEntry {
                            file,
                            anchor,
                            kind,
                            label,
                            desc,
                            tags,
                            hint,
                        });
                    }
                }
            }
        }

        entries
    }

    /// Reload the guide from disk (manual or auto).
    pub fn reload(&mut self) {
        let path = self.root.join(".riv").join("guide.toml");
        self.last_modified = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        self.last_check_time = Some(Instant::now());
        let new_entries = Self::parse_file(&path);
        self.entries = new_entries;
        self.apply_filter();
    }

    /// Check if the guide file has been modified on disk and reload if needed.
    /// Returns `true` if the guide was reloaded.
    pub fn check_and_reload_if_stale(&mut self) -> bool {
        let now = Instant::now();
        let last_check = self
            .last_check_time
            .unwrap_or(Instant::now() - std::time::Duration::from_secs(10));

        // Debounce: don't check more often than check_interval_ms
        if now.duration_since(last_check) < std::time::Duration::from_millis(self.check_interval_ms)
        {
            return false;
        }

        self.last_check_time = Some(now);

        let path = self.root.join(".riv").join("guide.toml");
        let current_modified = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());

        if current_modified != self.last_modified {
            self.reload();
            return true;
        }

        false
    }

    // ── Sync from current buffer only ─────────────────────────────

    /// Write the current entries back to `.riv/guide.toml`.
    /// Auto-creates the `.riv/` directory and the file if they don't exist.
    pub fn save_to_disk(&self) -> Result<(), String> {
        let path = self.root.join(".riv").join("guide.toml");

        // Ensure the .riv directory exists
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut content = String::new();
        content.push_str("# .riv/guide.toml\n");
        content.push_str("# Auto-updated by :guide update\n\n");

        for entry in &self.entries {
            content.push_str("[[checkpoint]]\n");
            content.push_str(&format!("file = \"{}\"\n", escape_toml_string(&entry.file)));
            content.push_str(&format!(
                "anchor = \"{}\"\n",
                escape_toml_string(&entry.anchor)
            ));
            content.push_str(&format!("kind = \"{}\"\n", escape_toml_string(&entry.kind)));
            content.push_str(&format!(
                "label = \"{}\"\n",
                escape_toml_string(&entry.label)
            ));
            content.push_str(&format!("desc = \"{}\"\n", escape_toml_string(&entry.desc)));
            if !entry.tags.is_empty() {
                content.push_str(&format!(
                    "tags = \"{}\"\n",
                    escape_toml_string(&entry.tags.join(", "))
                ));
            }
            if let Some(hint) = &entry.hint {
                content.push_str(&format!("hint = \"{}\"\n", escape_toml_string(hint)));
            }
            content.push('\n');
        }

        std::fs::write(&path, content).map_err(|e| format!("Failed to write guide.toml: {}", e))
    }

    // ── Filtering ─────────────────────────────────────────────────

    /// Apply the current filter string, updating `filtered`.
    pub fn apply_filter(&mut self) {
        let query = self.filter.to_lowercase();
        self.filtered.clear();

        for (i, entry) in self.entries.iter().enumerate() {
            if query.is_empty()
                || entry.label.to_lowercase().contains(&query)
                || entry.desc.to_lowercase().contains(&query)
                || entry.file.to_lowercase().contains(&query)
                || entry.kind.to_lowercase().contains(&query)
                || entry.tags.iter().any(|t| t.contains(&query))
                || entry.anchor.to_lowercase().contains(&query)
            {
                self.filtered.push(i);
            }
        }

        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.clamp_scroll();
    }

    /// Push a filter character and re-apply.
    pub fn filter_push(&mut self, c: char) {
        self.filter.push(c);
        self.apply_filter();
    }

    /// Pop the last filter character and re-apply.
    pub fn filter_pop(&mut self) {
        self.filter.pop();
        self.apply_filter();
    }

    // ── Selection ─────────────────────────────────────────────────

    /// Return the currently selected entry (if any).
    pub fn selected_entry(&self) -> Option<&GuideEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&idx| self.entries.get(idx))
    }

    // ── Anchor search ─────────────────────────────────────────────

    /// Find the line number (0-based) of the anchor string in the given source text.
    pub fn find_anchor_line(source: &str, anchor: &str) -> Option<usize> {
        let byte_offset = source.find(anchor)?;
        Some(source[..byte_offset].lines().count())
    }

    /// Compute the relative path of `file_path` with respect to `self.root`.
    /// Handles absolute/relative mismatches and symlink differences.
    fn make_relative_path(&self, file_path: &Path) -> String {
        // Strategy 1: Direct strip_prefix (fast path)
        if let Some(rel) = file_path.strip_prefix(&self.root).ok() {
            if let Some(s) = rel.to_str() {
                let trimmed = s.trim_start_matches(std::path::MAIN_SEPARATOR);
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }

        // Strategy 2: Canonicalize both (resolves symlinks, normalizes relative→absolute)
        if let (Ok(can_file), Ok(can_root)) = (file_path.canonicalize(), self.root.canonicalize()) {
            if let Some(rel) = can_file.strip_prefix(&can_root).ok() {
                if let Some(s) = rel.to_str() {
                    let trimmed = s.trim_start_matches(std::path::MAIN_SEPARATOR);
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
            }
        }

        // Strategy 3: file_path is already relative — use as-is
        if file_path.is_relative() {
            if let Some(s) = file_path.to_str() {
                return s.to_string();
            }
        }

        // Fallback: just the filename (last resort)
        file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    }

    pub fn sync_from_buffer(
        &mut self,
        file_path: &Path,
        source: &str,
    ) -> Result<SyncResult, String> {
        let mut result = SyncResult::default();

        let relative_file = self.make_relative_path(file_path);

        // ── Parse all markers from current source ──
        let markers: Vec<ParsedGuideMarker> = source
            .lines()
            .filter_map(|line| extract_guide_marker(line))
            .collect();

        // ── Remove stale entries for this file that no longer exist in source ──
        let current_anchors: Vec<String> = markers.iter().map(|m| m.anchor.clone()).collect();
        let current_labels: Vec<String> = markers.iter().map(|m| m.label.clone()).collect();
        let before_len = self.entries.len();
        self.entries.retain(|e| {
            if e.file != relative_file {
                return true; // Other files — keep
            }
            // Keep if anchor OR label still exists in source
            current_anchors.contains(&e.anchor) || current_labels.contains(&e.label)
        });
        let removed = before_len - self.entries.len();
        if removed > 0 {
            // Count removals as updates for reporting
            result.updated += removed;
        }

        // ── Upsert markers ──
        for marker in &markers {
            // Priority 1: Match by (file, anchor) — exact same anchor
            let by_anchor = self
                .entries
                .iter_mut()
                .find(|e| e.file == relative_file && e.anchor == marker.anchor);

            if let Some(existing) = by_anchor {
                let mut changed = false;
                if existing.desc != marker.desc {
                    existing.desc = marker.desc.clone();
                    changed = true;
                }
                if existing.kind != marker.kind {
                    existing.kind = marker.kind.clone();
                    changed = true;
                }
                if existing.label != marker.label {
                    existing.label = marker.label.clone();
                    changed = true;
                }
                if changed {
                    result.updated += 1;
                }
                continue;
            }

            // Priority 2: Match by (file, label) — anchor changed but same label
            let by_label = self
                .entries
                .iter_mut()
                .find(|e| e.file == relative_file && e.label == marker.label);

            if let Some(existing) = by_label {
                let mut changed = false;
                if existing.anchor != marker.anchor {
                    existing.anchor = marker.anchor.clone();
                    changed = true;
                }
                if existing.desc != marker.desc {
                    existing.desc = marker.desc.clone();
                    changed = true;
                }
                if existing.kind != marker.kind {
                    existing.kind = marker.kind.clone();
                    changed = true;
                }
                if changed {
                    result.updated += 1;
                }
                continue;
            }

            // Priority 3: New entry
            self.entries.push(GuideEntry {
                file: relative_file.clone(),
                anchor: marker.anchor.clone(),
                kind: marker.kind.clone(),
                label: marker.label.clone(),
                desc: marker.desc.clone(),
                tags: Vec::new(),
                hint: None,
            });
            result.added += 1;
        }

        if result.added > 0 || result.updated > 0 {
            self.save_to_disk()?;
            self.apply_filter();
        }

        Ok(result)
    }
}

// ── Scrollable implementation ─────────────────────────────────────

impl Scrollable for Guide {
    fn selected(&self) -> usize {
        self.selected
    }

    fn selected_mut(&mut self) -> &mut usize {
        &mut self.selected
    }

    fn scroll_mut(&mut self) -> &mut usize {
        &mut self.scroll
    }

    fn len(&self) -> usize {
        self.filtered.len()
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
        self.clamp_scroll();
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        let vis = self.visible_rows();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + vis {
            self.scroll = self.selected - vis + 1;
        }
    }

    fn visible_rows(&self) -> usize {
        20
    }
}
