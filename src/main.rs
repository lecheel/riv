// src/main.rs
//! riv-core — Main binary entry point for the riv Vim-like text editor.
//!
//! Initializes the terminal, creates the editor instance, and runs
//! the main event loop until the user quits.

mod action;
mod buffer;
mod clipboard;
mod codeium;
mod command;
mod command_macros;
mod command_registry;
mod completion;
mod config;
mod dirty;
mod dlog;
pub mod ed;
mod editor;
mod ghost_text;
mod git;
mod guide;
mod health;
mod highlight;
mod jsonrpc;
mod keybind;
mod llm;
mod llm_client;
mod llm_session;
mod lsp;
mod misc;
mod mru;
mod msgbox;
mod overlay;
mod popup;
mod powerline;
pub mod prompt;
mod render;
pub mod ripgrep;
mod rounded_box;
mod session;
mod state;
mod status;
mod tags;
mod terminal;
mod terminal_sanitize;
mod undo;
mod vocab;
mod window;

use crate::ed::FileOpsExt;
use crate::ed::GitExt;
use clap::Parser;
use config::Config;
use editor::{CommandResult, Editor, Mode};
use highlight::Highlighter;
use render::render;
use std::path::PathBuf;
use terminal::Terminal;

/// The event poll timeout in milliseconds.
const POLL_TIMEOUT_MS: u64 = 16; // ~60 FPS

/// Number of lines reserved for the status area (powerline + command input + infobar).
const STATUS_HEIGHT: u16 = 3;

/// riv-core — Vim-like text editor
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(next_line_help = true)]
struct Cli {
    /// Files to open
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Initial line number to position the cursor at (1-based +N).
    #[arg(short = 'L', long = "line", value_name = "NUM")]
    line: Option<usize>,

    /// Enable verbose logging (info level)
    #[arg(short, long, default_value_t = false)]
    verbose: bool,

    /// Run health check and exit
    #[arg(long)]
    healthy: bool,
}

/// Preprocess raw CLI arguments: convert Vim‑style `+N` tokens into
/// `--line=N` so that `clap` can parse them uniformly.
fn preprocess_plus_line_args() -> Vec<String> {
    let raw_args: Vec<String> = std::env::args().collect();
    raw_args
        .into_iter()
        .map(|arg| {
            // Only translate +N where N is purely numeric (1-based line number).
            // Leave +/pattern for future implementation.
            if let Some(n) = arg.strip_prefix('+') {
                if n.parse::<usize>().is_ok() && !n.starts_with('/') {
                    return format!("--line={}", n);
                }
            }
            arg
        })
        .collect()
}

fn main() {
    // ── Convert +N → --line=N before clap sees the args ──
    let args = preprocess_plus_line_args();
    let cli = Cli::parse_from(args);

    // Initialize logger with appropriate level
    let log_level = if cli.verbose { "info" } else { "warn" };
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(format!("riv={}", log_level)),
    )
    .init();

    // Run health check if requested
    if cli.healthy {
        let code = health::run_health_check();
        std::process::exit(code);
    }

    // Load configuration.
    let config = Config::load().unwrap_or_else(|_e| Config::default());

    // Create editor instance.
    let mut editor = Editor::new(config);

    // Track the first file opened so we can ensure it's the active buffer
    let mut first_buffer_id: Option<crate::buffer::BufferId> = None;

    for path in &cli.files {
        match editor.open_file_in_current_if_empty(path) {
            Ok(id) => {
                if first_buffer_id.is_none() {
                    first_buffer_id = Some(id);
                }
            }
            Err(_e) => {}
        }
    }

    // If we opened multiple files, the last one will be active.
    // Switch back to the first file so it's the one displayed on startup.
    if let Some(first_id) = first_buffer_id {
        if let Some(window) = editor.windows.active_window_mut() {
            if window.buffer_id != first_id {
                window.set_buffer(first_id);
                editor.restore_cursor_position();
                editor.ensure_cursor_visible_all();
            }
        }
    }

    // ── Apply +N / --line: position cursor at the requested line ──
    // This intentionally overrides the restored position from position_map
    // because the user explicitly asked for a specific line on the CLI.
    if let Some(requested_line) = cli.line {
        if requested_line > 0 {
            if let Some(window) = editor.windows.active_window_mut() {
                let buffer_id = window.buffer_id;
                // Convert 1-based (user-facing) to 0-based (internal)
                let target_line = requested_line.saturating_sub(1);
                if let Some(buffer) = editor.buffers.get(&buffer_id) {
                    let max_line = buffer.line_count().saturating_sub(1);
                    window.cursor.position.line = target_line.min(max_line);
                    window.cursor.position.col = 0;
                    window.cursor.desired_col = None;
                }
            }
            editor.ensure_cursor_visible_all();
        }
    }

    // Enter terminal raw mode.
    let mut terminal = match Terminal::enable() {
        Ok(t) => t,
        Err(_e) => {
            std::process::exit(1);
        }
    };

    // Handle initial terminal size.
    if let Ok((w, h)) = terminal.size() {
        editor.handle_resize(w, h);
    }

    // Run the main event loop.
    let result = run_event_loop(&mut editor, &mut terminal);

    // Cleanup is handled by Terminal::Drop (leaves raw mode, restores screen).
    match result {
        Ok(()) => {}
        Err(_e) => {
            std::process::exit(1);
        }
    }
}

/// The main event loop: poll events → batch → process → render → repeat.
fn run_event_loop(
    editor: &mut Editor,
    terminal: &mut Terminal,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut highlighter = Highlighter::new();
    let mut last_render_time = std::time::Instant::now();

    loop {
        // ── Poll for the first event (blocking with timeout) ──
        let event = terminal.poll_event(POLL_TIMEOUT_MS)?;
        editor.process_event(event);

        // ── Batch: drain all immediately pending events ──
        // This collapses rapid j/j/j/j into a single render frame.
        loop {
            match terminal.poll_event(0) {
                Ok(Some(e)) => {
                    editor.process_event(Some(e));
                    if editor.should_quit {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        // ── Check for quit ──
        if editor.should_quit {
            break;
        }

        // ── Keep cursor visible ──
        editor.ensure_cursor_visible_all();

        // Debounced background updates (git gutter, diff popup refresh)
        editor.tick();
        editor.update_diff_popup();

        // ── Render (throttled to ~60 FPS) ──
        let wants_render = editor.dirty.is_any_dirty();

        if wants_render && last_render_time.elapsed().as_millis() >= 8 {
            if let Err(_e) = render(editor, terminal, &mut highlighter) {}
            editor.dirty.clear();
            last_render_time = std::time::Instant::now();
        }
        // If wants_render but throttled, dirty flags stay set → next frame renders

        // ── Flush output ──
        let _ = terminal.flush();
    }

    Ok(())
}
