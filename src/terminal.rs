//! Terminal abstraction layer for riv-core.
//!
//! Wraps crossterm to provide raw-mode terminal input/output,
//! key event polling, and terminal size queries.

use std::io::{self, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent, KeyEventKind, MouseEvent,
};
use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

// ── Error type ──────────────────────────────────────────────────────

/// Errors that can occur during terminal operations.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Failed to enable raw mode: {0}")]
    RawMode(String),
    #[error("Failed to get terminal size: {0}")]
    Size(String),
}

// ── Key representation ──────────────────────────────────────────────

/// Represents a key press event, normalised across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    Enter,
    Backspace,
    Tab,
    BackTab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    F(u8),
    Paste(String),
    Unknown,
}

impl Key {
    /// Try to convert a crossterm `KeyEvent` into a `Key`.
    pub fn from_key_event(ke: &KeyEvent) -> Self {
        use crossterm::event::{KeyCode, KeyModifiers};

        match ke.code {
            KeyCode::Char(c) => {
                if ke.modifiers.contains(KeyModifiers::CONTROL) {
                    Key::Ctrl(c)
                } else if ke.modifiers.contains(KeyModifiers::ALT) {
                    Key::Alt(c)
                } else {
                    Key::Char(c)
                }
            }
            KeyCode::Enter => Key::Enter,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Tab => Key::Tab,
            KeyCode::BackTab => Key::BackTab,
            KeyCode::Esc => Key::Escape,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Delete => Key::Delete,
            KeyCode::Insert => Key::Insert,
            KeyCode::F(f) => Key::F(f),
            KeyCode::Null => Key::Unknown,
            _ => Key::Unknown,
        }
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Char(c) => write!(f, "{}", c),
            Key::Ctrl(c) => write!(f, "C-{}", c.to_ascii_uppercase()),
            Key::Alt(c) => write!(f, "A-{}", c),
            Key::Enter => write!(f, "<Enter>"),
            Key::Backspace => write!(f, "<BS>"),
            Key::Tab => write!(f, "<Tab>"),
            Key::BackTab => write!(f, "<S-Tab>"),
            Key::Escape => write!(f, "<Esc>"),
            Key::Up => write!(f, "<Up>"),
            Key::Down => write!(f, "<Down>"),
            Key::Left => write!(f, "<Left>"),
            Key::Right => write!(f, "<Right>"),
            Key::Home => write!(f, "<Home>"),
            Key::End => write!(f, "<End>"),
            Key::PageUp => write!(f, "<PgUp>"),
            Key::PageDown => write!(f, "<PgDn>"),
            Key::Delete => write!(f, "<Del>"),
            Key::Insert => write!(f, "<Ins>"),
            Key::F(n) => write!(f, "<F{}>", n),
            Key::Paste(_) => write!(f, "<Paste>"),
            Key::Unknown => write!(f, "<?>"),
        }
    }
}

// ── Terminal event ──────────────────────────────────────────────────

/// Normalised terminal event.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Key(Key),
    Mouse(MouseEvent),
    Resize(u16, u16),
}

// ── Terminal struct ─────────────────────────────────────────────────

/// Manages the terminal in raw mode.
///
/// On creation, enables raw mode, enters the alternate screen, hides the cursor,
/// and enables bracketed paste. On drop, restores the original terminal state.
pub struct Terminal {
    stdout: io::Stdout,
}

impl Terminal {
    /// Enable raw mode and initialise the terminal.
    pub fn enable() -> Result<Self, TerminalError> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();

        // Enter alternate screen, hide cursor, enable bracketed paste.
        crossterm::execute!(stdout, EnterAlternateScreen, Hide, EnableBracketedPaste,)?;

        Ok(Self { stdout })
    }

    /// Poll for a terminal event, blocking up to `timeout_ms`.
    pub fn poll_event(&self, timeout_ms: u64) -> Result<Option<TerminalEvent>, TerminalError> {
        if event::poll(std::time::Duration::from_millis(timeout_ms))? {
            match event::read()? {
                Event::Paste(text) => {
                    // Sanitize immediately at the boundary — nothing raw ever
                    // enters the editor's key processing pipeline.
                    let safe = crate::terminal_sanitize::sanitize_for_display(&text).into_owned();
                    Ok(Some(TerminalEvent::Key(Key::Paste(safe))))
                }
                Event::Key(key_event) => {
                    // crossterm on some platforms fires Press AND Release.
                    // Only handle Press.
                    if key_event.kind == KeyEventKind::Press {
                        Ok(Some(TerminalEvent::Key(Key::from_key_event(&key_event))))
                    } else {
                        Ok(None)
                    }
                }
                Event::Mouse(mouse_event) => Ok(Some(TerminalEvent::Mouse(mouse_event))),
                Event::Resize(w, h) => Ok(Some(TerminalEvent::Resize(w, h))),
                _ => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    /// Return current terminal size as `(columns, rows)`.
    pub fn size(&self) -> Result<(u16, u16), TerminalError> {
        terminal::size().map_err(|e| TerminalError::Size(e.to_string()))
    }

    /// Get a mutable reference to the underlying stdout writer.
    pub fn stdout_mut(&mut self) -> &mut io::Stdout {
        &mut self.stdout
    }

    /// Flush pending output.
    pub fn flush(&mut self) -> Result<(), TerminalError> {
        self.stdout.flush()?;
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Best-effort restore: show cursor, leave alternate screen,
        // disable bracketed paste, then raw mode.
        let _ = crossterm::execute!(
            self.stdout,
            Show,
            LeaveAlternateScreen,
            DisableBracketedPaste,
        );
        let _ = disable_raw_mode();
    }
}
