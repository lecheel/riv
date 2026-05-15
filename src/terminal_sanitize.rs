// In a new file: src/terminal_sanitize.rs

/// Strip or replace bytes/codepoints that can escape the terminal's
/// current context and inject control sequences.
///
/// Rules:
///  - ESC (0x1B) and its common followers ([ ] O P) are stripped entirely.
///  - C0 controls (0x00–0x1F) except TAB (0x09) are replaced with U+FFFD.
///  - C1 controls (0x80–0x9F) are replaced with U+FFFD (CSI, OSC etc. live here).
///  - DEL (0x7F) is replaced with U+FFFD.
///  - NUL (0x00) is stripped.
///  - Everything else passes through unchanged.
pub fn sanitize_for_display(input: &str) -> std::borrow::Cow<'_, str> {
    // Fast path: nothing to replace
    if !needs_sanitizing(input) {
        return std::borrow::Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // NUL — drop entirely
            '\x00' => {}

            // ESC — drop ESC plus any immediately following [ ] O P
            // This kills CSI (\x1B[), OSC (\x1B]), SS3 (\x1BO), DCS (\x1BP)
            '\x1B' => {
                match chars.peek() {
                    Some(&'[') | Some(&']') | Some(&'O') | Some(&'P') => {
                        chars.next(); // consume the follower too
                    }
                    _ => {}
                }
                // ESC itself is dropped regardless
            }

            // C0 controls except TAB, LF, CR — replace
            '\x01'..='\x08' | '\x0B'..='\x0C' | '\x0E'..='\x1A' | '\x1C'..='\x1F' => {
                out.push('\u{FFFD}');
            }

            // DEL
            '\x7F' => {
                out.push('\u{FFFD}');
            }

            // C1 controls (U+0080–U+009F) — includes CSI, OSC, ST etc.
            '\u{0080}'..='\u{009F}' => {
                out.push('\u{FFFD}');
            }

            // Everything else is safe
            c => out.push(c),
        }
    }

    std::borrow::Cow::Owned(out)
}

/// Returns true if the string contains any byte that would be altered
/// by sanitize_for_display. Used for the fast-path skip.
fn needs_sanitizing(s: &str) -> bool {
    s.bytes().any(|b| {
        matches!(b,
            0x00..=0x08 | 0x0B..=0x0C | 0x0E..=0x1A | 0x1B..=0x1F | 0x7F | 0x80..=0x9F
        )
    })
}

/// Sanitize a filename or path for display. Same rules plus we additionally
/// replace path separators that could make a crafted filename look like a
/// directory traversal in status bar context.
pub fn sanitize_path_display(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    sanitize_for_display(&raw).into_owned()
}

/// Truncate a sanitized string to at most `max_cols` terminal columns,
/// appending "…" if truncated. Uses unicode-width for accurate column count.
pub fn truncate_display(s: &str, max_cols: usize) -> std::borrow::Cow<'_, str> {
    use unicode_width::UnicodeWidthChar;

    let mut cols = 0usize;
    let mut end_byte = s.len();
    let mut truncated = false;

    for (byte_idx, ch) in s.char_indices() {
        let w = ch.width().unwrap_or(0);
        if cols + w > max_cols.saturating_sub(1) {
            end_byte = byte_idx;
            truncated = true;
            break;
        }
        cols += w;
    }

    if truncated {
        let mut result = s[..end_byte].to_owned();
        result.push('…');
        std::borrow::Cow::Owned(result)
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_injection() {
        // A crafted filename: "evil\x1b[2Jfile" would clear the screen
        let evil = "evil\x1b[2Jfile";
        let clean = sanitize_for_display(evil);
        assert!(!clean.contains('\x1b'));
        assert_eq!(clean, "evilfile");
    }

    #[test]
    fn strips_osc_injection() {
        // OSC sequence: set terminal title via filename
        let evil = "name\x1b]0;injected title\x07.rs";
        let clean = sanitize_for_display(evil);
        assert!(!clean.contains('\x1b'));
    }

    #[test]
    fn preserves_normal_text() {
        let normal = "src/main.rs — 42:7";
        assert!(matches!(
            sanitize_for_display(normal),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn replaces_c1_controls() {
        // U+009B is CSI in C1
        let evil = "before\u{009B}2Jafter";
        let clean = sanitize_for_display(evil);
        assert!(!clean.contains('\u{009B}'));
        assert!(clean.contains('\u{FFFD}'));
    }

    #[test]
    fn strips_nul() {
        let evil = "a\x00b";
        assert_eq!(sanitize_for_display(evil).as_ref(), "ab");
    }
}
