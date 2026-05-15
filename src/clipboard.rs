//! Clipboard integration via the `arboard` crate.
//!
//! Provides simple `get_text` / `set_text` helpers that create a fresh
//! `arboard::Clipboard` on each call (the clipboard handle does not need
//! to live for the lifetime of the editor).

/// Read text from the system clipboard.
/// Returns `None` if the clipboard is empty, not text, or unavailable.
pub fn get_text() -> Option<String> {
    match arboard::Clipboard::new() {
        Ok(mut clip) => clip.get_text().ok(),
        Err(_e) => None,
    }
}

/// Write text to the system clipboard.
/// Returns `Ok(())` on success, or an error string on failure.
pub fn set_text(text: &str) -> Result<(), String> {
    match arboard::Clipboard::new() {
        Ok(mut clip) => match clip.set_text(text) {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = format!("clipboard set_text failed: {}", e);
                Err(msg)
            }
        },
        Err(e) => {
            let msg = format!("clipboard init failed: {}", e);
            Err(msg)
        }
    }
}
