// tags.rs
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct TagEntry {
    pub name: String,
    pub file: PathBuf,
    pub line: usize,
    pub kind: Option<String>,
    pub scope: Option<String>,
}

impl TagEntry {
    pub fn display(&self, project_root: &Path) -> String {
        let kind_str = self.kind.as_deref().unwrap_or("?");
        let file_rel = self.file.strip_prefix(project_root).unwrap_or(&self.file).display();
        if let Some(ref scope) = self.scope {
            format!("[{}] {}::{} ({}:{})", kind_str, scope, self.name, file_rel, self.line)
        } else {
            format!("[{}] {} ({}:{})", kind_str, self.name, file_rel, self.line)
        }
    }
}

#[derive(Debug, Clone)]
pub struct TagStackEntry {
    pub file: PathBuf,
    pub line: usize,
    pub col: usize,
}

pub struct TagManager {
    tags: HashMap<String, Vec<TagEntry>>,
    tag_file: PathBuf,
    project_root: PathBuf,
    tag_stack: Vec<TagStackEntry>,
    current_matches: Vec<TagEntry>,
    current_match_idx: usize,
}

impl TagManager {
    pub fn new() -> Self {
        Self {
            tags: HashMap::new(),
            tag_file: PathBuf::new(),
            project_root: PathBuf::new(),
            tag_stack: Vec::new(),
            current_matches: Vec::new(),
            current_match_idx: 0,
        }
    }

    pub fn init(&mut self, filepath: &Path) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        // Convert to absolute path so we can properly traverse up to find .git
        let abs_path = if filepath.is_absolute() {
            filepath.to_path_buf()
        } else {
            cwd.join(filepath)
        };

        let mut dir = abs_path.parent().unwrap_or(&cwd).to_path_buf();

        loop {
            if dir.join(".git").exists() {
                self.tag_file = dir.join(".tags");
                self.project_root = dir;
                return;
            }
            match dir.parent() {
                Some(p) if p != dir && !p.as_os_str().is_empty() => {
                    dir = p.to_path_buf();
                }
                _ => {
                    // Final fallback: use current working directory
                    self.tag_file = cwd.join(".tags");
                    self.project_root = cwd;
                    return;
                }
            }
        }
    }

    pub fn generate_tags(&mut self) -> Result<String, String> {
        if self.project_root.as_os_str().is_empty() {
            return Err("No project root set".to_string());
        }

        let output = Command::new("ctags")
            .args([
                "-R",
                "--fields=+neS",
                "--extras=+q",
                "--exclude=.git",
                "--exclude=target",
                "--exclude=.venv",
                "--exclude=build",
                "--exclude=node_modules",
                "--exclude=dist",
                "--exclude=__pycache__",
                "--exclude=*.log",
                "-o",
                self.tag_file.to_str().unwrap_or(".tags"),
            ])
            .current_dir(&self.project_root)
            .output()
            .map_err(|e| format!("Failed to run ctags: {}. Install universal-ctags?", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ctags error: {}", stderr.trim()));
        }

        self.load_tags_file()?;

        let count = self.tags.values().map(|v| v.len()).sum::<usize>();
        Ok(format!("Generated {} tags", count))
    }

    pub fn load_tags_file(&mut self) -> Result<(), String> {
        if !self.tag_file.exists() {
            return Err("No .tags file. Run :tags to generate.".to_string());
        }

        let content = std::fs::read_to_string(&self.tag_file).map_err(|e| format!("Failed to read .tags: {}", e))?;

        self.tags.clear();

        for line in content.lines() {
            if line.starts_with('!') || line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 {
                continue;
            }

            let name = parts[0].to_string();
            let file = PathBuf::from(parts[1]);
            let pattern = parts[2].to_string();

            let mut line_num = 0;
            let mut kind = None;
            let mut scope = None;

            if parts.len() > 3 {
                for field in &parts[3..] {
                    if let Some(val) = field.strip_prefix("line:") {
                        line_num = val.parse().unwrap_or(0);
                    } else if let Some(val) = field.strip_prefix("kind:") {
                        kind = Some(val.to_string());
                    } else if let Some(val) = field.strip_prefix("scope:") {
                        scope = Some(val.to_string());
                    }
                }
            }

            if line_num == 0 {
                line_num = Self::parse_line_from_pattern(&pattern);
            }

            if line_num > 0 {
                let entry = TagEntry {
                    name: name.clone(),
                    file,
                    line: line_num,
                    kind,
                    scope,
                };
                self.tags.entry(name.to_lowercase()).or_default().push(entry);
            }
        }

        Ok(())
    }

    fn parse_line_from_pattern(pattern: &str) -> usize {
        if let Some(idx) = pattern.find(';') {
            let prefix = &pattern[..idx];
            if prefix.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = prefix.parse::<usize>() {
                    return n;
                }
            }
        }
        0
    }

    pub fn find_tags(&mut self, name: &str) -> Vec<TagEntry> {
        if self.tags.is_empty() && self.tag_file.exists() {
            let _ = self.load_tags_file();
        }

        let key = name.to_lowercase();

        if let Some(matches) = self.tags.get(&key) {
            let mut results = matches.clone();
            results.sort_by(|a, b| a.line.cmp(&b.line));
            return results;
        }

        Vec::new()
    }

    pub fn push_stack(&mut self, file: PathBuf, line: usize, col: usize) {
        self.tag_stack.push(TagStackEntry { file, line, col });
        if self.tag_stack.len() > 100 {
            self.tag_stack.remove(0);
        }
    }

    pub fn pop_stack(&mut self) -> Option<TagStackEntry> {
        self.tag_stack.pop()
    }

    pub fn stack_size(&self) -> usize {
        self.tag_stack.len()
    }

    pub fn get_stack_display(&self) -> Vec<String> {
        self.tag_stack
            .iter()
            .rev()
            .enumerate()
            .map(|(i, e)| format!("{} {}:{}", i + 1, e.file.display(), e.line + 1))
            .collect()
    }

    pub fn set_current_matches(&mut self, matches: Vec<TagEntry>) {
        self.current_matches = matches;
        self.current_match_idx = 0;
    }

    pub fn current_match(&self) -> Option<&TagEntry> {
        self.current_matches.get(self.current_match_idx)
    }

    pub fn next_match(&mut self) -> Option<&TagEntry> {
        if !self.current_matches.is_empty() {
            self.current_match_idx = (self.current_match_idx + 1) % self.current_matches.len();
            Some(&self.current_matches[self.current_match_idx])
        } else {
            None
        }
    }

    pub fn prev_match(&mut self) -> Option<&TagEntry> {
        if !self.current_matches.is_empty() {
            self.current_match_idx = if self.current_match_idx == 0 {
                self.current_matches.len() - 1
            } else {
                self.current_match_idx - 1
            };
            Some(&self.current_matches[self.current_match_idx])
        } else {
            None
        }
    }

    pub fn match_index(&self) -> usize {
        self.current_match_idx
    }

    pub fn match_count(&self) -> usize {
        self.current_matches.len()
    }

    pub fn get_matches_display(&self, project_root: &Path) -> Vec<String> {
        self.current_matches.iter().map(|tag| tag.display(project_root)).collect()
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Returns true if the tags file exists on disk.
    pub fn tag_file_exists(&self) -> bool {
        self.tag_file.exists()
    }

    /// Returns true if no tags have been loaded into memory.
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }
}
