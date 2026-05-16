// src/ed/file_ops.rs
//! File operations: opening, saving, creating new files, and file picker.

use std::path::Path;

use crate::buffer::BufferError;
use crate::buffer::BufferId;
use crate::buffer::Language;
use crate::config::HistoryData;
use crate::ed::lsp::LspExt;
use crate::ed::GitExt;
use crate::ed::ReplaceExt;
use crate::editor::Editor;
use crate::msgbox::AppMessage;
use crate::popup::FilePicker;
use crate::CommandResult;

/// Extension trait for file operations.
pub trait FileOpsExt {
    /// Open a file, creating a buffer for it and a window if needed.
    /// If the file is already open in a buffer, switch to it instead of creating a clone.
    fn open_file(&mut self, path: &Path) -> Result<crate::buffer::BufferId, BufferError>;

    /// Save the current buffer to its existing path.
    fn save(&mut self) -> Result<(), BufferError>;

    /// Save the current buffer to a new path.
    fn save_as(&mut self, path: &Path) -> Result<(), BufferError>;

    /// Create a new untitled buffer.
    fn new_file(&mut self) -> CommandResult;

    /// Open the file picker (find file) starting from the current file's directory or cwd.
    fn find_file(&mut self) -> CommandResult;

    /// Save command and search history to disk.
    fn save_history(&self);

    /// Save the active window's cursor position for its buffer (if file-backed).
    fn save_current_position(&mut self);

    /// Restore cursor position for the current buffer from the position map.
    /// Call this after opening a file. Centers the viewport on the saved line.
    fn restore_cursor_position(&mut self);

    /// Format the current buffer using an external formatter.
    /// Returns Ok(()) if formatting succeeded, Err(msg) otherwise.
    fn format_current_buffer(&mut self) -> Result<(), String>;

    /// Format the current buffer asynchronously.
    fn format_current_buffer_async(&mut self, save_after: bool) -> Result<(), String>;

    /// Save cursor positions for all open file-backed buffers.
    fn save_all_positions(&mut self);

    // fn open_file_in_current_if_empty(&mut self, path: &Path) -> Result<BufferId, BufferError>;
}

impl FileOpsExt for Editor {
    fn open_file(&mut self, path: &Path) -> Result<crate::buffer::BufferId, BufferError> {
        self.save_current_position();

        let buffer_id = self.buffers.open_file(path)?;

        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(buffer_id);
        } else {
            self.windows.create_window(buffer_id);
        }

        self.restore_cursor_position();
        self.ensure_cursor_visible_all();

        self.git_provider = None;
        self.cached_diff_hunks.clear();
        self.git_gutter_dirty_since = None; // bypass debounce
        if let Some(w) = self.windows.active_window() {
            if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                buf.git_gutter.clear();
            }
        }
        self.ensure_git_gutter();
        self.dirty.mark_all();

        let file_path = self
            .buffers
            .get(&buffer_id)
            .and_then(|b| b.file_path.clone());

        if let Some(ref path) = file_path {
            self.lsp_did_open(path);
        }

        // ── MRU: record this file as recently used ──
        if let Some(ref fp) = file_path {
            // Get the restored cursor position (if any) for the MRU entry
            let (line, col) = self
                .windows
                .active_window()
                .map(|w| (w.cursor.position.line, w.cursor.position.col))
                .unwrap_or((0, 0));
            self.mru.touch(fp.clone(), line, col);
        }

        self.dirty.mark_all();
        self.set_status(format!("Opened {:?}", path));
        Ok(buffer_id)
    }

    fn save(&mut self) -> Result<(), BufferError> {
        // Format on save if enabled
        if self.config.format_on_save {
            match self.format_current_buffer() {
                Ok(()) => {}
                Err(e) => {
                    self.set_infobar_message(format!("⚠ fmt: {}", e));
                    self.show_fmt_info_popup("Format Error (save)", &e);
                }
            }
        }

        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            // Perform the save inside a block so the mutable borrow ends before lsp_did_save
            let file_path = {
                let buffer = self
                    .buffers
                    .get_mut(&buffer_id)
                    .ok_or_else(|| BufferError::Io(std::io::Error::other("Buffer not found")))?;
                buffer.save()?;
                buffer.file_path.clone()
            };
            if let Some(ref path) = file_path {
                self.lsp_did_save(path);
            }
            self.invalidate_git_gutter();
            self.set_status("File saved.".to_string());
            self.dirty.mark_all();
            Ok(())
        } else {
            Err(BufferError::Io(std::io::Error::other(
                "No active buffer to save",
            )))
        }
    }
    fn save_as(&mut self, path: &Path) -> Result<(), BufferError> {
        // Format on save if enabled
        if self.config.format_on_save {
            match self.format_current_buffer() {
                Ok(()) => {}
                Err(e) => {
                    self.set_infobar_message(format!("⚠ fmt: {}", e));
                    self.show_fmt_info_popup("Format Error (save)", &e);
                }
            }
        }

        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            // Perform save inside a block to release the buffer borrow
            {
                let buffer = self
                    .buffers
                    .get_mut(&buffer_id)
                    .ok_or_else(|| BufferError::Io(std::io::Error::other("Buffer not found")))?;
                buffer.save_to(path)?;
            }
            self.lsp_did_open(path);

            // ── MRU: record the new save path ──
            self.mru.touch(path.to_path_buf(), 0, 0);

            self.set_status(format!("Saved as {:?}", path));
            self.dirty.mark_all();
            Ok(())
        } else {
            Err(BufferError::Io(std::io::Error::other(
                "No active buffer to save",
            )))
        }
    }

    fn new_file(&mut self) -> CommandResult {
        let buf = crate::buffer::Buffer::new();
        let id = buf.id;
        self.buffers.insert(buf);

        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(id);
        }
        self.dirty.mark_all();
        self.set_status("[No Name]".to_string());
        CommandResult::ViewChanged
    }

    fn find_file(&mut self) -> CommandResult {
        // Determine starting directory: current file's parent, or cwd.
        let start_dir = self
            .current_buffer()
            .and_then(|b| b.file_path.as_ref())
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        self.file_picker = Some(FilePicker::new(&start_dir));
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    /// Save command and search history to disk.
    fn save_history(&self) {
        let data = HistoryData {
            command: self.command_prompt.history.clone(),
            search: self.search_prompt.history.clone(),
        };
        data.save();
    }

    /// Save the cursor position for the active buffer.
    fn save_current_position(&mut self) {
        if let Some(window) = self.windows.active_window() {
            let buffer_id = window.buffer_id;
            let pos = window.cursor.position;
            let scroll = window.viewport.scroll_line;

            // 1. Persist file-backed buffers to disk (existing behavior)
            if let Some(buffer) = self.buffers.get(&buffer_id) {
                if let Some(ref path) = buffer.file_path {
                    self.position_map.set(path, pos);
                }
            }

            // 2. Remember ALL buffers (including special) for in-session :bn/:bp
            self.buffer_positions.insert(buffer_id, (pos, scroll));
        }
    }
    /// Format the current buffer using an external formatter.
    /// Returns Ok(()) if formatting succeeded, Err(msg) otherwise.
    fn format_current_buffer(&mut self) -> Result<(), String> {
        let (buffer_id, _cursor_pos, language, file_path) = {
            let window = match self.windows.active_window() {
                Some(w) => w,
                None => return Err("No active window".into()),
            };
            let buffer = match self.buffers.get(&window.buffer_id) {
                Some(b) => b,
                None => return Err("No buffer".into()),
            };
            (
                window.buffer_id,
                window.cursor.position,
                buffer.language,
                buffer.file_path.clone(),
            )
        };

        let (cmd, args) = match language {
            Some(crate::buffer::Language::Rust) => ("rustfmt", Vec::new()),
            Some(crate::buffer::Language::JavaScript)
            | Some(crate::buffer::Language::TypeScript) => {
                // Use prettier if available, with the file path for parser detection
                let mut args = vec!["--stdin-filepath".to_string()];
                if let Some(ref path) = file_path {
                    args.push(path.to_string_lossy().into_owned());
                } else {
                    // Fallback: assume JS
                    args.push("file.js".to_string());
                }
                ("prettier", args)
            }
            Some(crate::buffer::Language::Python) => ("black", vec!["-".to_string()]),
            _ => return Err("No formatter configured for this language".into()),
        };

        // Get the buffer text
        let text = match self.buffers.get(&buffer_id) {
            Some(b) => b.text(),
            None => return Err("Buffer not found".into()),
        };

        // Run the formatter
        let result = std::process::Command::new(cmd)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(ref mut stdin) = child.stdin {
                    let _ = stdin.write_all(text.as_bytes());
                }
                child.wait_with_output()
            });

        match result {
            Ok(output) => {
                if output.status.success() {
                    let formatted = String::from_utf8_lossy(&output.stdout);
                    let formatted_str = formatted.into_owned();

                    // Only update if something changed
                    if formatted_str != text {
                        if let Some(buffer) = self.buffers.get_mut(&buffer_id) {
                            let cursor_pos = self
                                .windows
                                .active_window()
                                .map(|w| w.cursor.position)
                                .unwrap_or_default();
                            buffer.replace_all(&formatted_str, cursor_pos);

                            buffer.reparse_tree();
                        }
                        // Restore cursor, clamped to new buffer bounds
                        if let Some(window) = self.windows.active_window_mut() {
                            if let Some(buffer) = self.buffers.get(&buffer_id) {
                                let max_line = buffer.line_count().saturating_sub(1);
                                window.cursor.position.line =
                                    window.cursor.position.line.min(max_line);
                                let max_col = buffer.line_len(window.cursor.position.line);
                                window.cursor.position.col =
                                    window.cursor.position.col.min(max_col);
                            }
                        }
                    }
                    Ok(())
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    Err(format!("{} failed: {}", cmd, stderr.trim()))
                }
            }
            Err(e) => Err(format!("Failed to run {}: {}", cmd, e)),
        }
    }

    // ── Format on save ──────────────────────────────────────────

    fn format_current_buffer_async(&mut self, save_after: bool) -> Result<(), String> {
        if self.formatting_pending {
            return Err("Already formatting…".into());
        }

        let (buffer_id, cursor_pos, language, file_path) = {
            let window = self.windows.active_window().ok_or("No active window")?;
            let buffer = self.buffers.get(&window.buffer_id).ok_or("No buffer")?;
            (
                window.buffer_id,
                window.cursor.position,
                buffer.language,
                buffer.file_path.clone(),
            )
        };

        let (cmd, args): (&'static str, Vec<String>) = match language {
            Some(Language::Rust) => (
                "rustfmt",
                vec![
                    "--edition".into(),
                    "2021".into(),
                    "--emit".into(),
                    "stdout".into(),
                ],
            ),
            Some(Language::JavaScript) | Some(Language::TypeScript) => {
                let mut a = vec!["--stdin-filepath".into()];
                if let Some(ref p) = file_path {
                    a.push(p.to_string_lossy().into_owned());
                } else {
                    a.push("file.js".into());
                }
                ("prettier", a)
            }
            Some(Language::Python) => ("black", vec!["-".into()]),
            _ => return Err("No formatter configured for this language".into()),
        };

        let text = self
            .buffers
            .get(&buffer_id)
            .ok_or("Buffer not found")?
            .text();

        let app_tx = self.app_tx.clone();
        self.formatting_pending = true;
        self.formatting_buffer_id = Some(buffer_id);

        self.llm_runtime.spawn(async move {
            let result =
                tokio::task::spawn_blocking(move || Editor::run_formatter(cmd, &args, &text)).await;

            let msg = match result {
                Ok(Ok(formatted)) => AppMessage::FormatResult {
                    buffer_id,
                    result: Ok(formatted),
                    cursor_state: cursor_pos,
                    save_after,
                },
                Ok(Err(e)) => AppMessage::FormatResult {
                    buffer_id,
                    result: Err(e),
                    cursor_state: cursor_pos,
                    save_after,
                },
                Err(e) => AppMessage::FormatResult {
                    buffer_id,
                    result: Err(format!("Formatter task panicked: {}", e)),
                    cursor_state: cursor_pos,
                    save_after,
                },
            };
            let _ = app_tx.send(msg);
        });

        Ok(())
    }
    /// Save cursor positions for all open file-backed buffers.
    fn save_all_positions(&mut self) {
        // Save the active window's position first
        if let Some(window) = self.windows.active_window() {
            if let Some(buffer) = self.buffers.get(&window.buffer_id) {
                if let Some(ref path) = buffer.file_path {
                    self.position_map.set(path, window.cursor.position);
                    self.mru.update_position(
                        path,
                        window.cursor.position.line,
                        window.cursor.position.col,
                    );
                }
            }
        }

        self.save_history();
        self.mru.save();
        // Try to save positions for other windows too
        // (requires WindowManager to support iteration — see note below)
        self.position_map.cleanup();
        self.position_map.save();
    }
    /// Restore the cursor position for the active buffer.
    fn restore_cursor_position(&mut self) {
        let buffer_id = match self.windows.active_window() {
            Some(w) => w.buffer_id,
            None => return,
        };

        // 1. Try the in-session map first (works for ALL buffer kinds)
        if let Some(&(pos, scroll)) = self.buffer_positions.get(&buffer_id) {
            if let Some(window) = self.windows.active_window_mut() {
                window.cursor.position = pos;
                window.viewport.scroll_line = scroll;
            }
            return;
        }

        // 2. Fallback to cross-session map (file-backed only, for newly opened files)
        if let Some(buffer) = self.buffers.get(&buffer_id) {
            if let Some(ref path) = buffer.file_path {
                if let Some(pos) = self.position_map.get(path) {
                    if let Some(window) = self.windows.active_window_mut() {
                        window.cursor.position = pos;
                    }
                }
            }
        }
    }
}

impl Editor {
    /// Open a file, reusing the current buffer if it is empty and unnamed.
    pub fn open_file_in_current_if_empty(&mut self, path: &Path) -> Result<BufferId, BufferError> {
        use crate::buffer::Language;
        use ropey::Rope;

        let is_empty_unnamed = self
            .current_buffer()
            .map(|b| {
                b.file_path.is_none()
                    && !b.dirty
                    && b.line_count() <= 1
                    && b.line_text(0).map(|l| l.trim().is_empty()).unwrap_or(true)
            })
            .unwrap_or(false);

        if is_empty_unnamed {
            let window = self.windows.active_window().unwrap();
            let buf_id = window.buffer_id;

            {
                let buffer = self.buffers.get_mut(&buf_id).unwrap();

                // Try to read the file. If it doesn't exist yet, keep the empty
                // buffer but still assign the file path so :w works for new files.
                match std::fs::read_to_string(path) {
                    Ok(content) => {
                        buffer.rope = Rope::from_str(&content);
                        buffer.last_saved_text = content; // keep dirty-tracking accurate
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // New file — keep the empty rope; last_saved_text stays ""
                    }
                    Err(e) => return Err(BufferError::Io(e)),
                }

                buffer.file_path = Some(path.to_path_buf());
                buffer.dirty = false;
                buffer.clear_undo_history();

                buffer.language = Some(
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .map(Language::from_extension)
                        .unwrap_or(Language::PlainText),
                );

                buffer.init_tree_sitter();
            } // ← mutable borrow ends here

            // ── Init git gutter (same as open_file) ──
            self.git_provider = None;
            self.cached_diff_hunks.clear();
            self.git_gutter_dirty_since = None; // bypass debounce
            if let Some(w) = self.windows.active_window() {
                if let Some(buf) = self.buffers.get_mut(&w.buffer_id) {
                    buf.git_gutter.clear();
                }
            }
            self.ensure_git_gutter();

            self.set_status(format!("Opened {:?}", path));
            self.restore_cursor_position();
            self.ensure_cursor_visible_all();
            self.dirty.mark_all();
            Ok(buf_id)
        } else {
            self.open_file(path)
        }
    }
    /// Pure blocking formatter call — runs inside spawn_blocking, no editor refs.
    fn run_formatter(cmd: &str, args: &[String], text: &str) -> Result<String, String> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    format!("`{}` not found — is it installed and on PATH?", cmd)
                }
                _ => format!("Failed to launch `{}`: {}", cmd, e),
            })?;

        if let Some(ref mut stdin) = child.stdin {
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| format!("Failed to write to `{}` stdin: {}", cmd, e))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| format!("`{}` did not exit cleanly: {}", cmd, e))?;

        if output.status.success() {
            String::from_utf8(output.stdout)
                .map_err(|_| format!("`{}` produced non-UTF-8 output", cmd))
        } else {
            // Prefer stderr; fall back to a generic message with exit code
            let stderr = String::from_utf8_lossy(&output.stderr);
            let trimmed = stderr.trim();
            if trimmed.is_empty() {
                Err(format!("`{}` exited with {}", cmd, output.status))
            } else {
                // Trim to first 3 lines so it doesn't flood the status bar
                let short: String = trimmed.lines().take(3).collect::<Vec<_>>().join(" │ ");
                Err(format!("`{}`: {}", cmd, short))
            }
        }
    }
}
