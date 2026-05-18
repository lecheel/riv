use std::path::PathBuf;

use crate::action::Action;
use crate::command::CommandRegistry;
use crate::popup::FilePicker;
use crate::window::SplitDirection;

use crate::ed::build::BuildExt;
use crate::ed::git_diff::GitDiffExt;
use crate::ed::git_status::GitStatusExt;
use crate::ed::BufferOpsExt;
use crate::ed::CommandExt;
use crate::ed::EditingExt;
use crate::ed::FileOpsExt;
use crate::ed::GitExt;
use crate::ed::GitLogExt;
use crate::ed::LlmExt;
use crate::ed::RipgrepExt;
use crate::ed::WindowExt;
use crate::editor::{CommandResult, Editor, Mode};

// ---------------------- Handler functions -------------------------
fn shortcuts_handler(e: &mut Editor, _args: &str) -> CommandResult {
    use crate::ed::ShortcutsExt;
    e.show_shortcuts()
}

// ── LLM handlers ───────────────────────────────────────────────────

fn llm_prompt_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmQuickPrompt)
}

fn llm_open_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmOpen)
}

fn llm_close_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmClose)
}

fn llm_clear_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmClearHistory)
}

fn llm_cancel_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.llm_cancel()
}

fn llm_explain_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmQuickExplain)
}

fn llm_summarize_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmQuickSummarize)
}

fn llm_check_english_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmQuickCheckEnglish)
}

fn llm_translate_zh_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmQuickTranslateChinese)
}

fn llm_translate_en_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::LlmQuickTranslateEnglish)
}

fn llm_session_handler(e: &mut Editor, args: &str) -> CommandResult {
    use crate::ed::LlmExt;
    let args = args.trim();
    if args.is_empty() {
        e.llm_session_list()
    } else if args == "new" {
        e.llm_session_new()
    } else if args == "save" {
        e.llm_session_save()
    } else if args == "delete" || args == "del" {
        e.llm_session_delete()
    } else if args.starts_with("switch ") || args.starts_with("load ") {
        let name = args.split_whitespace().nth(1).unwrap_or("").to_string();
        e.llm_session_switch(name)
    } else {
        // Treat bare argument as session name to switch to
        e.llm_session_switch(args.to_string())
    }
}

fn quit_handler(e: &mut Editor, _args: &str) -> CommandResult {
    if e.buffers.iter().any(|b| b.dirty) {
        CommandResult::Error("Unsaved changes! Use :q! to force quit.".into())
    } else {
        e.save_all_positions();
        e.should_quit = true;
        CommandResult::Quit
    }
}

fn force_quit_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.save_all_positions();
    e.should_quit = true;
    CommandResult::Quit
}

fn save_handler(e: &mut Editor, args: &str) -> CommandResult {
    let args = args.trim();
    if args.is_empty() {
        match e.save() {
            Ok(()) => CommandResult::Message("File saved.".into()),
            Err(err) => CommandResult::Error(err.to_string()),
        }
    } else {
        let path = PathBuf::from(args);
        match e.save_as(&path) {
            Ok(()) => CommandResult::Message(format!("Saved as {:?}", path)),
            Err(err) => CommandResult::Error(err.to_string()),
        }
    }
}

fn force_save_handler(e: &mut Editor, _args: &str) -> CommandResult {
    match e.save() {
        Ok(()) => CommandResult::Message("File saved.".into()),
        Err(err) => CommandResult::Error(err.to_string()),
    }
}

fn save_quit_handler(e: &mut Editor, _args: &str) -> CommandResult {
    let save_result = e.save();
    let dirty = e.buffers.iter().any(|b| b.dirty);
    match save_result {
        Ok(()) => {
            e.save_all_positions();
            e.should_quit = true;
            CommandResult::Quit
        }
        Err(err) if dirty => CommandResult::Error(format!("Failed to save: {}", err)),
        Err(_) => {
            e.save_all_positions();
            e.should_quit = true;
            CommandResult::Quit
        }
    }
}

fn force_save_quit_handler(e: &mut Editor, _args: &str) -> CommandResult {
    let _ = e.save();
    e.save_all_positions();
    e.should_quit = true;
    CommandResult::Quit
}

fn edit_handler(e: &mut Editor, args: &str) -> CommandResult {
    let args = args.trim();
    if args.is_empty() {
        let path = e.current_buffer().and_then(|b| b.file_path.clone());
        match path {
            Some(p) => match e.open_file(&p) {
                Ok(_) => CommandResult::Message(format!("Reloaded {:?}", p)),
                Err(err) => CommandResult::Error(err.to_string()),
            },
            None => CommandResult::Error("No file associated with buffer.".into()),
        }
    } else {
        let path = PathBuf::from(args);
        match e.open_file(&path) {
            Ok(_) => CommandResult::Message(format!("Opened {:?}", path)),
            Err(err) => CommandResult::Error(err.to_string()),
        }
    }
}

fn new_file_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.new_file()
}

fn split_horizontal_handler(e: &mut Editor, args: &str) -> CommandResult {
    let args = args.trim();
    if args.is_empty() {
        e.split_horizontal()
    } else {
        let path = PathBuf::from(args);
        match e.buffers.open_file(&path) {
            Ok(bid) => {
                let _ = e
                    .windows
                    .split_active_with_buffer(SplitDirection::Horizontal, bid);
                e.windows.resize_all(e.term_width, e.term_height);
                e.dirty.mark_all();
                CommandResult::Message(format!("Opened {:?}", path))
            }
            Err(err) => CommandResult::Error(err.to_string()),
        }
    }
}

fn force_split_horizontal_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.split_horizontal()
}

fn split_vertical_handler(e: &mut Editor, args: &str) -> CommandResult {
    let args = args.trim();
    if args.is_empty() {
        e.split_vertical()
    } else {
        let path = PathBuf::from(args);
        match e.buffers.open_file(&path) {
            Ok(bid) => {
                let _ = e
                    .windows
                    .split_active_with_buffer(SplitDirection::Vertical, bid);
                e.windows.resize_all(e.term_width, e.term_height);
                e.dirty.mark_all();
                CommandResult::Message(format!("Opened {:?}", path))
            }
            Err(err) => CommandResult::Error(err.to_string()),
        }
    }
}

fn force_split_vertical_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.split_vertical()
}

fn only_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.windows.close_all_others();
    e.windows.resize_all(e.term_width, e.term_height);
    e.dirty.mark_all();
    CommandResult::ViewChanged
}

fn set_handler(e: &mut Editor, args: &str) -> CommandResult {
    e.handle_set_command(args.trim())
}

fn help_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.set_status(
        "Commands: :q :q! :w :wq :e :sp :vs :set :n :rg :<num> :hints | \
         Keys: h/j/k/l i o O dd yy p grg Ctrl-u/d Ctrl-w s/v"
            .into(),
    );
    CommandResult::NoOp
}

fn hints_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.show_hints()
}

fn diff_toggle_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.diff_mode_active = !e.diff_mode_active;
    if e.diff_mode_active {
        e.update_diff_popup();
        if e.diff_popup.is_some() {
            e.dirty.mark_all();
            CommandResult::Message("Diff mode enabled".into())
        } else {
            CommandResult::Message("Diff mode enabled (no hunk at cursor)".into())
        }
    } else {
        e.diff_popup = None;
        e.dirty.mark_all();
        CommandResult::Message("Diff mode disabled".into())
    }
}

// Add the handler function near gdiff_handler:
fn gdiff_all_handler(e: &mut Editor, args: &str) -> CommandResult {
    e.git_diff_all(args.trim())
}

fn gdiff_handler(e: &mut Editor, args: &str) -> CommandResult {
    e.git_diff_open(args.trim())
}

fn daf_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.process_action(Action::DeleteAroundFunction)
}

fn ghunk_next_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.git_next_hunk()
}

fn ghunk_prev_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.git_prev_hunk()
}

fn ghunk_revert_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.git_revert_hunk()
}

fn gc_handler(e: &mut Editor, _args: &str) -> CommandResult {
    use crate::ed::git_commit::GitCommitExt;
    e.git_commit_generate()
}

fn gstatus_handler(e: &mut Editor, args: &str) -> CommandResult {
    e.git_status_open(args.trim())
}

fn gblame_handler(_e: &mut Editor, _args: &str) -> CommandResult {
    CommandResult::Message("Gblame — not yet implemented".into())
}

fn glog_handler(e: &mut Editor, args: &str) -> CommandResult {
    let args = args.trim();
    let mut count_arg = "";
    let mut grep_arg = "";

    if !args.is_empty() {
        // Strip optional "grep " prefix
        let effective_args = if args.starts_with("grep ") {
            &args[5..]
        } else {
            args
        };

        // Split into first token and rest
        let mut parts = effective_args.splitn(2, |c: char| c.is_ascii_whitespace());
        let first = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        if !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) {
            // First token is a number → count, rest is grep pattern
            count_arg = first;
            grep_arg = rest;
        } else {
            // Entire input is the grep pattern
            grep_arg = effective_args;
        }
    }

    e.git_log_open(count_arg, grep_arg)
}

fn shell_handler(_e: &mut Editor, args: &str) -> CommandResult {
    let args = args.trim();
    if args.is_empty() {
        CommandResult::Error("No command specified.".into())
    } else {
        CommandResult::Error(format!("Shell command '{}' not yet implemented", args))
    }
}

fn bm_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.show_mark_list()
}

fn bn_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.next_buffer()
}

fn bp_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.prev_buffer()
}

fn ls_handler(e: &mut Editor, _args: &str) -> CommandResult {
    use crate::popup::{BufferListEntry, BufferListPopup};

    // active_buffer_id is already u64 (BufferId is a type alias)
    let active_buffer_id = e.windows.active_window().map(|w| w.buffer_id);

    let entries: Vec<BufferListEntry> = e
        .buffers
        .iter()
        .map(|buffer| BufferListEntry {
            id: buffer.id, // no .0
            name: buffer.display_name(),
            dirty: buffer.dirty,
            active: active_buffer_id == Some(buffer.id), // no .0
        })
        .collect();

    if entries.is_empty() {
        return CommandResult::Message("No buffers open".into());
    }

    e.buffer_list_popup = Some(BufferListPopup::new(entries));
    e.dirty.mark_all();
    CommandResult::ViewChanged
}

fn bd_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.delete_buffer(false)
}

fn force_bd_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.delete_buffer(true)
}

fn cd_handler(_e: &mut Editor, args: &str) -> CommandResult {
    let args = args.trim();
    if args.is_empty() {
        match std::env::current_dir() {
            Ok(p) => CommandResult::Message(format!("Current directory: {:?}", p)),
            Err(_) => CommandResult::Error("Cannot determine current directory.".into()),
        }
    } else {
        match std::env::set_current_dir(args) {
            Ok(()) => CommandResult::Message(format!("Changed directory to {:?}", args)),
            Err(err) => CommandResult::Error(format!("cd: {}", err)),
        }
    }
}

fn pwd_handler(_e: &mut Editor, _args: &str) -> CommandResult {
    match std::env::current_dir() {
        Ok(p) => CommandResult::Message(format!("{}", p.display())),
        Err(_) => CommandResult::Error("Cannot determine current directory.".into()),
    }
}

fn colorscheme_handler(_e: &mut Editor, _args: &str) -> CommandResult {
    CommandResult::Message("colorscheme — not yet implemented".into())
}

fn sort_handler(_e: &mut Editor, _args: &str) -> CommandResult {
    CommandResult::Message("sort — not yet implemented".into())
}

fn fmt_handler(e: &mut Editor, _args: &str) -> CommandResult {
    match e.format_current_buffer() {
        Ok(()) => {
            e.dirty.mark_all();
            CommandResult::Message("Buffer formatted (external).".into())
        }
        Err(e) => CommandResult::Error(e),
    }
}

fn fmt_ts_handler(e: &mut Editor, _args: &str) -> CommandResult {
    // Visual mode: only selected lines
    let range = if matches!(e.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
        if let Some(window) = e.windows.active_window() {
            if let Some(anchor) = window.selection_anchor {
                let head = window.cursor.position;
                Some((anchor.line.min(head.line), anchor.line.max(head.line)))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        // Normal mode: entire buffer
        let last_line = e
            .current_buffer()
            .map(|b| b.line_count().saturating_sub(1))
            .unwrap_or(0);
        Some((0, last_line))
    };

    match e.format_ts_indent(range) {
        Ok(()) => {
            if matches!(e.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
                if let Some(window) = e.windows.active_window_mut() {
                    window.selection_anchor = None;
                }
                e.mode = Mode::Normal;
            }
            e.dirty.mark_all();
            CommandResult::Message("Indent fixed".into())
        }
        Err(e) => CommandResult::Error(e),
    }
}

fn file_picker_handler(e: &mut Editor, _args: &str) -> CommandResult {
    let start_dir = e
        .current_buffer()
        .and_then(|b| b.file_path.as_ref())
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    e.file_picker = Some(FilePicker::new(&start_dir));
    e.dirty.mark_all();
    CommandResult::ViewChanged
}

fn reg_handler(e: &mut Editor, _args: &str) -> CommandResult {
    let mut lines = Vec::new();

    // Default register ""
    if !e.yank_register.is_empty() {
        let preview = e.yank_register.lines().next().unwrap_or("");
        let truncated = if preview.len() > 60 {
            format!("{}…", &preview[..60])
        } else {
            preview.to_string()
        };
        let safe_preview = truncated
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        lines.push(format!("\"\"   {}", safe_preview));
    }

    // Named registers a-z
    for c in 'a'..='z' {
        if let Some(content) = e.get_named_register(c) {
            if !content.is_empty() {
                let preview = content.lines().next().unwrap_or("");
                let truncated = if preview.len() > 60 {
                    format!("{}…", &preview[..60])
                } else {
                    preview.to_string()
                };
                let safe_preview = truncated
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t");
                lines.push(format!("\"{}   {}", c, safe_preview));
            }
        }
    }

    if lines.is_empty() {
        e.register_popup = None;
        e.set_status("All registers are empty".to_string());
        return CommandResult::Message("All registers are empty".to_string());
    }

    e.register_popup = Some(lines);
    e.dirty.mark_all();
    CommandResult::ViewChanged
}

// ── Tag (ctags) handlers ──────────────────────────────────────────

fn tags_generate_handler(e: &mut Editor, _args: &str) -> CommandResult {
    let file_path = e.current_buffer().and_then(|b| b.file_path.clone());
    if let Some(ref path) = file_path {
        e.tag_manager.init(path);
    }
    match e.tag_manager.generate_tags() {
        Ok(msg) => {
            e.dirty.mark_all();
            CommandResult::Message(msg)
        }
        Err(err) => CommandResult::Error(err),
    }
}

fn tag_handler(e: &mut Editor, args: &str) -> CommandResult {
    let name = args.trim();

    if name.is_empty() {
        // Delegate to tag_under_cursor which handles init + load + word extraction
        crate::ed::tag::tag_under_cursor(e);
        CommandResult::ViewChanged
    } else {
        // Explicit tag name provided — initialize and search
        let file_path = e.current_buffer().and_then(|b| b.file_path.clone());
        if let Some(ref path) = file_path {
            e.tag_manager.init(path);
        }
        if e.tag_manager.is_empty() && e.tag_manager.tag_file_exists() {
            if let Err(err) = e.tag_manager.load_tags_file() {
                return CommandResult::Error(err);
            }
        }

        // Strip qualifiers from the provided name too
        let tag_name = crate::ed::tag::strip_qualifiers(name);
        let matches = e.tag_manager.find_tags(&tag_name);
        // Fallback to full name if stripped name finds nothing
        let matches = if matches.is_empty() && tag_name != name {
            e.tag_manager.find_tags(name)
        } else {
            matches
        };

        if matches.is_empty() {
            if e.tag_manager.tag_file_exists() {
                CommandResult::Error(format!("Tag '{}' not found", name))
            } else {
                CommandResult::Error(format!("Tag '{}' not found (run :tags to generate)", name))
            }
        } else {
            crate::ed::tag::handle_tag_matches(e, matches, name);
            CommandResult::ViewChanged
        }
    }
}
fn tnext_handler(e: &mut Editor, _args: &str) -> CommandResult {
    crate::ed::tag::tag_next(e)
}

fn tprev_handler(e: &mut Editor, _args: &str) -> CommandResult {
    crate::ed::tag::tag_prev(e)
}

fn tpop_handler(e: &mut Editor, _args: &str) -> CommandResult {
    crate::ed::tag::tag_pop(e)
}

fn vocab_handler(e: &mut Editor, args: &str) -> CommandResult {
    e.vocab_handle(args)
}

pub fn guide_handler(e: &mut Editor, args: &str) -> CommandResult {
    let arg = args.trim();

    // ── :guide update ── Scan current buffer for markers ──
    if arg == "update" {
        let mut guide = if let Some(g) = e.guide_popup.take() {
            g
        } else {
            crate::guide::Guide::load() // Loads empty if file missing
        };

        // Get current buffer's file path and in-memory source text
        let (file_path, source) = if let Some(buffer) = e.current_buffer() {
            let path = match buffer.file_path.as_ref() {
                Some(p) => p.clone(),
                None => {
                    e.guide_popup = Some(guide);
                    e.dirty.guide = true;
                    return CommandResult::Error("Current buffer has no file path".into());
                }
            };
            let text = buffer.rope.to_string();
            (path, text)
        } else {
            e.guide_popup = Some(guide);
            e.dirty.guide = true;
            return CommandResult::Error("No active buffer".into());
        };

        match guide.sync_from_buffer(&file_path, &source) {
            Ok(result) => {
                e.guide_popup = Some(guide);
                e.dirty.guide = true;
                if result.added > 0 || result.updated > 0 {
                    CommandResult::Message(format!(
                        "Guide updated: +{} added, {} updated",
                        result.added, result.updated
                    ))
                } else {
                    CommandResult::Message("No guide markers found in current buffer".into())
                }
            }
            Err(err) => {
                e.guide_popup = Some(guide);
                e.dirty.guide = true;
                CommandResult::Error(err)
            }
        }
    } else {
        // ── :guide (no args) ── Open the popup ──
        let mut guide = crate::guide::Guide::load();
        guide.apply_filter();

        // Allow opening even if empty (user might run :guide update next)
        if !guide.entries.is_empty() {
            // Pre-select the entry matching the current file
            let current_file = e
                .current_buffer()
                .and_then(|b| b.file_path.as_ref())
                .and_then(|p| p.canonicalize().ok())
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .unwrap_or_default();

            if let Some(pos) = guide.filtered.iter().position(|&idx| {
                let entry_path = guide.root.join(&guide.entries[idx].file);
                let entry_canonical = entry_path.canonicalize().ok();
                current_file
                    == entry_canonical
                        .and_then(|p| p.to_str().map(|s| s.to_string()))
                        .unwrap_or_default()
                    || current_file.ends_with(&guide.entries[idx].file)
            }) {
                guide.selected = pos;
            }
        }

        e.guide_popup = Some(guide);
        e.dirty.mark_all();
        CommandResult::ViewChanged
    }
}

// ── Ripgrep handlers ───────────────────────────────────────────────

/// `:rg <pattern>` — Search the project with ripgrep.
fn rg_handler(e: &mut Editor, args: &str) -> CommandResult {
    let pattern = args.trim();

    if pattern.is_empty() {
        return CommandResult::Error(
            "Usage: :rg <pattern>\n\
             Or use K or grg in normal mode to search for the word under cursor."
                .into(),
        );
    }

    if let Some(window) = e.windows.active_window() {
        if let Some(buffer) = e.buffers.get(&window.buffer_id) {
            if buffer.kind == crate::buffer::BufferKind::Ripgrep {
                e.ripgrep_close_buffer();
            }
        }
    }

    let root_dir = e
        .current_buffer()
        .and_then(|b| b.file_path.as_ref())
        .and_then(|p| crate::misc::find_git_root(p)) // flattens Option<Option> -> Option<PathBuf>
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    e.set_status(format!("Searching for '{}'...", pattern));

    let escaped = crate::ripgrep::escape_regex(pattern);
    let rg_output = match crate::ripgrep::run_ripgrep(&escaped, &root_dir) {
        Ok(output) => output,
        Err(err) => return CommandResult::Error(err),
    };

    // Cache for :lastrg
    e.last_rg_pattern = Some(pattern.to_string());
    e.last_rg_root_dir = Some(root_dir);
    e.last_rg_output = Some(rg_output.clone());

    e.populate_ripgrep_buffer(pattern, rg_output)
}

/// `:rg! <pattern>` — Search from cwd (ignores git root).
fn rg_force_handler(e: &mut Editor, args: &str) -> CommandResult {
    let pattern = args.trim();

    if pattern.is_empty() {
        return CommandResult::Error(
            "Usage: :rg! <pattern>\n\
             Searches from current working directory (ignores git root)."
                .into(),
        );
    }

    if let Some(window) = e.windows.active_window() {
        if let Some(buffer) = e.buffers.get(&window.buffer_id) {
            if buffer.kind == crate::buffer::BufferKind::Ripgrep {
                e.ripgrep_close_buffer();
            }
        }
    }

    let root_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    e.set_status(format!("Searching for '{}' (from cwd)...", pattern));

    let escaped = crate::ripgrep::escape_regex(pattern);
    let rg_output = match crate::ripgrep::run_ripgrep(&escaped, &root_dir) {
        Ok(output) => output,
        Err(err) => return CommandResult::Error(err),
    };

    // Cache for :lastrg
    e.last_rg_pattern = Some(pattern.to_string());
    e.last_rg_root_dir = Some(root_dir);
    e.last_rg_output = Some(rg_output.clone());

    e.populate_ripgrep_buffer(pattern, rg_output)
}

/// `:lastrg` — Reopen last ripgrep results (instant, no re-search).
fn lastrg_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.ripgrep_last()
}

/// `:lastrg!` — Re-run the last ripgrep search.
fn lastrg_rerun_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.ripgrep_last_rerun()
}

fn mru_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.open_mru()
}

/// `:rgc` — Close the ripgrep results buffer.
fn rg_close_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.ripgrep_close_buffer()
}

/// `:copen` / `:clist` — Open the build output buffer (reuses existing if present).
fn clist_handler(e: &mut Editor, _args: &str) -> CommandResult {
    use crate::buffer::BufferKind;

    // Extract the ID first, dropping the immutable borrow before any mutation
    let existing_build_id = e
        .buffers
        .iter()
        .find(|b| b.kind == BufferKind::Build)
        .map(|b| b.id);

    if let Some(id) = existing_build_id {
        if let Some(w) = e.windows.active_window_mut() {
            w.set_buffer(id);
        }
        e.dirty.mark_all();
        CommandResult::ViewChanged
    } else {
        // No build buffer exists yet — run a fresh build
        e.run_build()
    }
}

/// :cn — next quickfix result (works for both ripgrep and build)
fn cn_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.quickfix_next()
}

/// :cp — previous quickfix result (works for both ripgrep and build)
fn cp_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.quickfix_prev()
}

fn keymap_handler(e: &mut Editor, args: &str) -> CommandResult {
    let args = args.trim();

    if args.is_empty() {
        // Show summary as a popup (not a status message — multi-line won't fit)
        let counts = e.keybinds.binding_counts();

        let mut entries = Vec::new();

        entries.push(crate::popup::KeymapEntry {
            keys: String::new(),
            action: "KEYBIND SUMMARY".to_string(),
            is_header: true,
        });

        let mut total = 0usize;
        for (mode, count) in &counts {
            total += count;
            entries.push(crate::popup::KeymapEntry {
                keys: format!("{:<10}", mode),
                action: format!("{} bindings", count),
                is_header: false,
            });
        }

        entries.push(crate::popup::KeymapEntry {
            keys: String::new(),
            action: format!("TOTAL: {} bindings", total),
            is_header: true,
        });

        entries.push(crate::popup::KeymapEntry {
            keys: String::new(),
            action: "COMMANDS".to_string(),
            is_header: true,
        });

        entries.push(crate::popup::KeymapEntry {
            keys: ":keymap n  ".to_string(),
            action: "Normal mode keymap".to_string(),
            is_header: false,
        });
        entries.push(crate::popup::KeymapEntry {
            keys: ":keymap i  ".to_string(),
            action: "Insert mode keymap".to_string(),
            is_header: false,
        });
        entries.push(crate::popup::KeymapEntry {
            keys: ":keymap v  ".to_string(),
            action: "Visual mode keymap".to_string(),
            is_header: false,
        });
        entries.push(crate::popup::KeymapEntry {
            keys: ":keymap c  ".to_string(),
            action: "Command mode keymap".to_string(),
            is_header: false,
        });

        // Build popup directly (no HelpEntry conversion needed)
        let mut popup = crate::popup::KeymapPopup {
            mode_name: "summary".to_string(),
            entries,
            selected: 0,
            scroll: 0,
        };
        // Skip past first header to first selectable row
        while popup.selected < popup.entries.len() && popup.entries[popup.selected].is_header {
            popup.selected += 1;
        }

        e.keymap_popup = Some(popup);
        e.dirty.help = true;
        e.dirty.cursor = true;
        e.dirty.mark_all();
        CommandResult::ViewChanged
    } else {
        // Show bindings for specific mode
        let mode_name = match args {
            "normal" | "n" => "normal",
            "insert" | "i" => "insert",
            "visual" | "v" => "visual",
            "command" | "c" => "command",
            _ => {
                return CommandResult::Error(format!(
                    "Unknown mode '{}'. Use: normal, insert, visual, command",
                    args
                ))
            }
        };

        let help_entries = e.keybinds.help_entries(mode_name);
        if help_entries.is_empty() {
            return CommandResult::Message(format!("No bindings for mode '{}'", mode_name));
        }

        let popup = crate::popup::KeymapPopup::new(mode_name.to_string(), help_entries);
        e.keymap_popup = Some(popup);
        e.dirty.help = true;
        e.dirty.cursor = true;
        e.dirty.mark_all();
        CommandResult::ViewChanged
    }
}

fn functions_handler(
    editor: &mut crate::editor::Editor,
    _args: &str,
) -> crate::editor::CommandResult {
    editor.show_function_list()
}

/// `:build` — Run `cargo build --release` and show errors in a build buffer.
fn build_handler(e: &mut Editor, _args: &str) -> CommandResult {
    e.run_build()
}

/// `:make` — Alias for `:build` (Vim muscle memory).
fn make_handler(e: &mut Editor, args: &str) -> CommandResult {
    // If args provided, they could be make/cargo flags in the future.
    // For now, just delegate to build.
    let _ = args;
    e.run_build()
}

// ---------------------- Builder function --------------------------

pub fn build_command_registry() -> CommandRegistry {
    let mut reg = CommandRegistry::new();

    // Quit
    reg.register_handler("q", quit_handler, "Quit (fails if dirty buffers exist)");
    reg.alias("quit", "q");
    reg.alias("qa", "q");
    reg.alias("qall", "q");

    reg.register_handler("q!", force_quit_handler, "Force quit without saving");
    reg.alias("quit!", "q!");
    reg.alias("qa!", "q!");
    reg.alias("qall!", "q!");

    // Save
    reg.register_handler("w", save_handler, "Save current file (:w [<path>])");
    reg.alias("write", "w");
    reg.register_handler("w!", force_save_handler, "Force save current file");
    reg.alias("write!", "w!");

    reg.register_handler("wq", save_quit_handler, "Save and quit");
    reg.alias("x", "wq");
    reg.alias("xit", "wq");
    reg.register_handler("wq!", force_save_quit_handler, "Force save and quit");

    // File operations
    reg.register_handler("e", edit_handler, "Open/reload file (:e [<path>])");
    reg.alias("edit", "e");
    reg.register_handler("n", new_file_handler, "Create a new empty buffer");
    reg.alias("new", "n");

    reg.register_handler("FilePicker", file_picker_handler, "Open file picker");
    reg.alias("FindFile", "FilePicker");

    // Window management
    reg.register_handler(
        "sp",
        split_horizontal_handler,
        "Horizontal split (:sp [<path>?])",
    );
    reg.alias("split", "sp");
    reg.register_handler(
        "sp!",
        force_split_horizontal_handler,
        "Force horizontal split",
    );
    reg.alias("split!", "sp!");

    reg.register_handler(
        "vs",
        split_vertical_handler,
        "Vertical split (:vs [<path>?])",
    );
    reg.alias("vsplit", "vs");
    reg.register_handler("vs!", force_split_vertical_handler, "Force vertical split");
    reg.alias("vsplit!", "vs!");

    reg.register_handler("only", only_handler, "Close all other windows");
    reg.alias("on", "only");

    // Settings
    reg.register_handler("set", set_handler, "Set an option (:set <opt>[=<val>])");

    // Help & info
    reg.register_handler("help", help_handler, "Show quick-reference help");
    reg.register_handler(
        "hints",
        hints_handler,
        "Show interactive keybinding help popup",
    );
    reg.alias("map", "hints");
    reg.alias("keys", "hints");
    reg.alias("bindings", "hints");

    // Git
    reg.register_handler(
        "Thunk",
        diff_toggle_handler,
        "Toggle automatic diff popup near hunks",
    );

    reg.register_handler(
        "gdiff!",
        gdiff_all_handler,
        "Git diff ALL files (:gdiff! [n|ref])",
    );
    reg.alias("Gdiff!", "gdiff!");

    reg.register_handler(
        "gdiff",
        gdiff_handler,
        "Git diff current file (:gdiff [n|ref])",
    );
    reg.alias("Gdiff", "gdiff");
    reg.alias("Gdif", "gdiff");
    reg.alias("diff", "gdiff");

    reg.register_handler("GhunkNext", ghunk_next_handler, "Jump to next git hunk");
    reg.alias("gn", "GhunkNext");
    reg.register_handler("GhunkPrev", ghunk_prev_handler, "Jump to previous git hunk");
    reg.alias("gp", "GhunkPrev");
    reg.register_handler(
        "GhunkRevert",
        ghunk_revert_handler,
        "Revert hunk under cursor",
    );
    reg.alias("gr", "GhunkRevert");

    reg.register_handler("Gstatus", gstatus_handler, "Show git status");
    reg.register_handler(
        "Gblame",
        gblame_handler,
        "Show git blame (not yet implemented)",
    );
    reg.register_handler("Glog", glog_handler, "Show git log");
    reg.alias("tig", "Glog");

    // ── LLM ────────────────────────────────────────────────────────
    reg.register_handler(
        "llm-prompt",
        llm_prompt_handler,
        "Open LLM quick prompt (visual: selection + ##TODO prefix)",
    );
    reg.alias("llm", "llm-prompt");
    reg.alias("LlmPrompt", "llm-prompt");

    reg.register_handler("llm-open", llm_open_handler, "Open LLM conversation buffer");
    reg.alias("LlmOpen", "llm-open");

    reg.register_handler(
        "llm-close",
        llm_close_handler,
        "Close LLM conversation buffer",
    );
    reg.alias("LlmClose", "llm-close");

    reg.register_handler(
        "llm-clear",
        llm_clear_handler,
        "Clear LLM conversation history",
    );
    reg.alias("LlmClear", "llm-clear");

    reg.register_handler(
        "llm-cancel",
        llm_cancel_handler,
        "Cancel in-progress LLM request",
    );
    reg.alias("LlmCancel", "llm-cancel");

    reg.register_handler(
        "llm-explain",
        llm_explain_handler,
        "Ask LLM to explain selection/word under cursor",
    );
    reg.alias("LlmExplain", "llm-explain");

    reg.register_handler(
        "llm-summarize",
        llm_summarize_handler,
        "Ask LLM to summarize selection/word under cursor",
    );
    reg.alias("LlmSummarize", "llm-summarize");

    reg.register_handler(
        "llm-check",
        llm_check_english_handler,
        "Ask LLM to check English grammar of selection",
    );
    reg.alias("LlmCheckEnglish", "llm-check");

    reg.register_handler(
        "llm-zh",
        llm_translate_zh_handler,
        "Ask LLM to translate selection to Chinese",
    );
    reg.alias("LlmTranslateZh", "llm-zh");

    reg.register_handler(
        "llm-en",
        llm_translate_en_handler,
        "Ask LLM to translate selection to English",
    );
    reg.alias("LlmTranslateEn", "llm-en");

    reg.register_handler(
        "llm-session",
        llm_session_handler,
        "LLM sessions: :llm-session [new|save|delete|switch <name>]",
    );
    reg.alias("llm-ss", "llm-session");
    reg.alias("LlmSession", "llm-session");

    // ── Ripgrep ────────────────────────────────────────────────────
    reg.register_handler(
        "rg",
        rg_handler,
        "Search project with ripgrep (:rg <pattern>)",
    );
    reg.alias("Rg", "rg");
    reg.alias("Ripgrep", "rg");
    reg.alias("grep", "rg");
    reg.alias("search", "rg");

    reg.register_handler(
        "rg!",
        rg_force_handler,
        "Search from cwd (ignore current file path)",
    );
    reg.alias("Rg!", "rg!");

    reg.register_handler("rgc", rg_close_handler, "Close ripgrep results buffer");
    reg.alias("Rgc", "rgc");
    reg.alias("rgclose", "rgc");
    reg.alias("rgq", "rgc");

    // Shell
    reg.register_handler("!", shell_handler, "Run shell command (:! <cmd>)");

    // Buffer navigation
    reg.register_handler("bn", bn_handler, "Switch to next buffer");
    reg.register_handler("bp", bp_handler, "Switch to previous buffer");
    // Next/prev ripgrep result
    reg.register_handler("cn", cn_handler, "Next ripgrep result in current buffer");
    reg.alias("cnext", "cn");
    reg.register_handler(
        "cp",
        cp_handler,
        "Previous ripgrep result in current buffer",
    );
    reg.alias("cprev", "cp");
    reg.register_handler("ls", ls_handler, "List open buffers (not yet implemented)");
    reg.alias("buffers", "ls");

    reg.register_handler("bd", bd_handler, "Close current buffer (:bd! to force)");
    reg.alias("bdelete", "bd");
    reg.alias("close", "bd");

    reg.register_handler(
        "bd!",
        force_bd_handler,
        "Force close current buffer (discard unsaved changes)",
    );
    reg.alias("bdelete!", "bd!");
    reg.alias("close!", "bd!");

    // Directory
    reg.register_handler("cd", cd_handler, "Change working directory");
    reg.register_handler("pwd", pwd_handler, "Print working directory");

    // Theme
    reg.register_handler(
        "colorscheme",
        colorscheme_handler,
        "Change colorscheme (not yet implemented)",
    );
    reg.alias("theme", "colorscheme");

    // Sort
    reg.register_handler(
        "sort",
        sort_handler,
        "Sort selected lines (not yet implemented)",
    );

    reg.register_handler(
        "codeium",
        |editor, _args| {
            if editor.codeium.is_connected {
                editor.set_status("Codeium: connected".into());
            } else {
                match editor.start_codeium() {
                    Ok(()) => editor.set_status("Codeium: started".into()),
                    Err(e) => editor.set_infobar_message(e),
                }
            }
            CommandResult::NoOp
        },
        "Start Codeium AI server or show status",
    );

    reg.alias("codium", "codeium");

    // Around line 610
    reg.register_handler(
        "codeium-disable",
        |editor, _args| {
            editor.codeium.stop();
            editor.config.codeium.enabled = false;
            editor.set_status("Codeium: disabled".into());
            CommandResult::NoOp
        },
        "Disable Codeium AI completion",
    );
    reg.alias("codium-disable", "codeium-disable");

    reg.register_handler(
        "keymap",
        keymap_handler,
        "Show keybindings (:keymap [normal|insert|visual|command])",
    );
    reg.alias("keymaps", "keymap");
    reg.alias("bindings", "keymap");
    reg.alias("showkeys", "keymap");

    reg.register_handler(
        "gc",
        gc_handler,
        "Generate git commit message from staged changes via LLM",
    );
    reg.alias("gitcommit", "gc");
    reg.alias("GitCommit", "gc");

    reg.register_handler(
        "Gstatus",
        gstatus_handler,
        "Show interactive git status (s=stage, Enter=open, q=close)",
    );
    reg.alias("gs", "Gstatus");
    reg.alias("status", "Gstatus");
    reg.alias("Git", "Gstatus");

    reg.register_handler(
        "lastrg",
        lastrg_handler,
        "Reopen last ripgrep results (instant)",
    );
    reg.alias("LastRg", "lastrg");
    reg.alias("LRg", "lastrg");
    reg.register_handler(
        "lastrg!",
        lastrg_rerun_handler,
        "Re-run last ripgrep search",
    );
    reg.alias("LastRg!", "lastrg!");
    reg.alias("LRg!", "lastrg!");

    reg.register_handler("mru", mru_handler, "Open Most Recently Used file popup");
    reg.alias("recent", "mru");
    reg.alias("Recent", "mru");
    reg.alias("MRU", "mru");

    reg.register_handler("daf", daf_handler, "Delete around function");
    reg.alias("DeleteAroundFunction", "daf");

    reg.register_handler(
        "functions",
        functions_handler,
        "List all functions in current buffer",
    );
    reg.register_handler(
        "funs",
        functions_handler,
        "List all functions in current buffer",
    );

    // ── Tags (ctags) ──────────────────────────────────────────────
    reg.register_handler(
        "tags",
        tags_generate_handler,
        "Generate ctags for the project",
    );
    reg.register_handler(
        "tag",
        tag_handler,
        "Jump to tag definition (:tag [name], empty = word under cursor)",
    );
    reg.register_handler("tn", tnext_handler, "Jump to next tag match");
    reg.alias("tnext", "tn");
    reg.register_handler("tp", tprev_handler, "Jump to previous tag match");
    reg.alias("tprev", "tp");
    reg.register_handler(
        "pop",
        tpop_handler,
        "Return to previous location from tag stack",
    );
    reg.alias("tpop", "pop");
    reg.alias("tagpop", "pop");

    reg.register_handler("reg", reg_handler, "Show contents of all registers");
    reg.alias("registers", "reg");

    // ── Build ──────────────────────────────────────────────────────
    reg.register_handler(
        "build",
        build_handler,
        "Run cargo build --release and show errors in a buffer",
    );
    reg.alias("Build", "build");
    reg.alias("cargo", "build");
    reg.alias("Cargo", "build");

    reg.register_handler("bm", bm_handler, "Show marks popup for quick navigation");
    reg.alias("bookmark", "bm");
    reg.alias("marks", "bm");
    reg.alias("Bookmarks", "bm");

    reg.register_handler(
        "shortcuts",
        shortcuts_handler,
        "Show keybindings for current mode in a float popup",
    );
    reg.alias("sc", "shortcuts");
    reg.register_handler(
        "vocab",
        vocab_handler,
        "Add word to local vocabulary completion (:vocab <word>)",
    );

    reg.register_handler(
        "guide",
        guide_handler,
        "Open code architecture guide (use 'update' to sync from current buffer)",
    );

    reg.register_handler(
        "clist",
        clist_handler,
        "Open build output buffer (or run :build if none exists)",
    );
    reg.alias("copen", "clist");
    reg.alias("Clist", "clist");
    reg.alias("Copen", "clist");

    // In build_command_registry()
    reg.register_handler(
        "indentguides!",
        |editor, _args| {
            editor.config.indent_guides = !editor.config.indent_guides;
            editor.dirty.mark_all();
            let state = if editor.config.indent_guides {
                "on"
            } else {
                "off"
            };
            CommandResult::Message(format!("Indent guides: {}", state))
        },
        "Toggle indent guides on/off",
    );

    // Formatting
    reg.register_handler("fmt", fmt_handler, "Format buffer with external formatter");
    reg.alias("format", "fmt");
    reg.register_handler("fmt!", fmt_ts_handler, "Fix indentation using tree-sitter");
    reg.alias("FixIndent", "fmt!");
    reg.alias("fixindent", "fmt!");

    reg
}
