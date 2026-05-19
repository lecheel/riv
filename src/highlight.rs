//! Syntax highlighting using tree-sitter.
//!
//! Provides a `Highlighter` that can highlight lines of text using tree-sitter
//! grammars. Supports Rust, JavaScript, TypeScript, and Python via the
//! `static-grammars` feature flag. Falls back to regex-based keyword matching
//! when grammars are not compiled in.

use std::collections::HashMap;
use std::sync::LazyLock;

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::Language;

// ── Highlight style ──────────────────────────────────────────────────

/// A single highlight span: (start_byte, end_byte, foreground color index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub style: HighlightStyle,
}

/// Named highlight styles mapped to terminal colors.
/// Color palette: Catppuccin Mocha (aligned with syntax.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightStyle {
    /// Comments.
    Comment,
    /// Doc comments (/// //!).
    DocComment,
    /// String literals.
    String,
    /// Character literals.
    Character,
    /// Escape sequences within strings.
    Escape,
    /// Numeric literals.
    Number,
    /// Boolean literals (true/false).
    Boolean,
    /// Keywords (let, struct, fn, ...).
    Keyword,
    /// Control flow keywords (if, else, match, loop, break, continue, return).
    ControlKeyword,
    /// Type names / constructors.
    Type,
    /// Function / method definitions.
    Function,
    /// Function / method calls.
    FunctionCall,
    /// Variables.
    Variable,
    /// Constants / static values.
    Constant,
    /// Built-in types / attributes.
    Builtin,
    /// Operators and punctuation.
    Operator,
    /// Properties / fields.
    Property,
    /// Tags (HTML/JSX).
    Tag,
    /// Attribute names (HTML/JSX, Rust #[...]).
    Attribute,
    /// Delimiters (brackets, braces, parens).
    Delimiter,
    /// self / Self / super / crate keywords.
    SelfKw,
    /// Lifetimes ('a, 'static).
    Lifetime,
    /// Macro invocations / definitions.
    Macro,
    /// Labels ('label:).
    Label,
    /// Preprocessor / shebang / special.
    Special,
    /// Regular identifiers (no special highlight).
    Plain,
    CommitHash,
    /// Error / invalid syntax.
    Error,
}

/// Color associated with a highlight style.
/// Returns crossterm RGB color values.
/// Palette: Catppuccin Mocha — aligned with syntax.rs.
impl HighlightStyle {
    /// Return the foreground color for this style as crossterm-compatible RGB.
    pub fn fg_rgb(self) -> (u8, u8, u8) {
        match self {
            // Comments — slate blue-gray (readable, subtle)
            HighlightStyle::Comment => (94, 105, 120),
            HighlightStyle::CommitHash => (191, 92, 38),
            // Doc comments — lighter slate (distinct from regular comments)
            HighlightStyle::DocComment => (115, 125, 145),
            // Strings — green
            HighlightStyle::String => (166, 227, 161),
            // Characters — green (same as string)
            HighlightStyle::Character => (166, 227, 161),
            // Escape sequences — light pink
            HighlightStyle::Escape => (235, 160, 172),
            // Numbers — orange-brown (distinct from constants)
            HighlightStyle::Number => (191, 92, 38),
            // Booleans — peach (constant-like)
            HighlightStyle::Boolean => (250, 179, 135),
            // Keywords — purple
            HighlightStyle::Keyword => (203, 166, 247),
            // Control keywords — purple (same as keyword)
            HighlightStyle::ControlKeyword => (203, 166, 247),
            // Types — dark cyan
            HighlightStyle::Type => (52, 155, 235),
            // Function definitions — cyan
            HighlightStyle::Function => (130, 215, 250),
            // Function calls — light cyan
            HighlightStyle::FunctionCall => (116, 199, 236),
            // Variables — muted cyan-gray
            HighlightStyle::Variable => (146, 165, 168),
            // Constants — peach
            HighlightStyle::Constant => (250, 179, 135),
            // Built-in types — dark cyan (same as Type)
            HighlightStyle::Builtin => (52, 155, 235),
            // Operators — light blue
            HighlightStyle::Operator => (137, 180, 250),
            // Properties — off-white
            HighlightStyle::Property => (205, 214, 244),
            // Tags — purple
            HighlightStyle::Tag => (203, 166, 247),
            // Attributes — muted gray
            HighlightStyle::Attribute => (108, 112, 134),
            // Delimiters — grayish blue
            HighlightStyle::Delimiter => (147, 153, 178),
            // self/Self/super/crate — pink
            HighlightStyle::SelfKw => (243, 139, 168),
            // Lifetimes — light magenta
            HighlightStyle::Lifetime => (245, 194, 231),
            // Macros — yellow
            HighlightStyle::Macro => (249, 226, 175),
            // Labels — warm yellow-gold
            HighlightStyle::Label => (250, 220, 150),
            // Special — red
            HighlightStyle::Special => (243, 139, 168),
            // Plain — off-white
            HighlightStyle::Plain => (205, 214, 244),
            // Error — red
            HighlightStyle::Error => (243, 139, 168),
        }
    }

    /// Convert a tree-sitter highlight capture name to our style.
    pub fn from_capture(name: &str) -> Self {
        match name {
            // Comments
            "comment" | "line_comment" | "block_comment" => HighlightStyle::Comment,
            "doc_comment" => HighlightStyle::DocComment,
            // Strings
            "string" | "string_literal" => HighlightStyle::String,
            "character" | "char" => HighlightStyle::Character,
            // Escapes
            "escape" | "escape_sequence" => HighlightStyle::Escape,
            // Numbers
            "number" | "integer" | "float" => HighlightStyle::Number,
            "boolean" => HighlightStyle::Boolean,
            // Self / super / crate
            "self" => HighlightStyle::SelfKw,
            // Lifetimes
            "lifetime" => HighlightStyle::Lifetime,
            // Macros
            "function.macro" | "macro" | "macro_invocation" => HighlightStyle::Macro,
            // Keywords
            "keyword" | "type" | "type_identifier" | "storage.type" | "storage.modifier"
            | "storage.class" => HighlightStyle::Keyword,
            "keyword.control"
            | "keyword.control.conditional"
            | "keyword.control.import"
            | "keyword.control.export"
            | "keyword.control.return"
            | "keyword.control.exception"
            | "conditional"
            | "repeat"
            | "exception" => HighlightStyle::ControlKeyword,
            // Types
            "type.definition" | "type.builtin" => HighlightStyle::Type,
            // Functions
            "function" | "function.definition" | "function.method" | "method" => {
                HighlightStyle::Function
            }
            "function.call" | "function.method.call" | "method.call" | "function.builtin" => {
                HighlightStyle::FunctionCall
            }
            // Variables
            "variable" | "variable.parameter" => HighlightStyle::Variable,
            "variable.builtin" => HighlightStyle::Builtin,
            "constant" | "constant.builtin" | "constant.language" => HighlightStyle::Constant,
            "constant.character" => HighlightStyle::Character,
            "constant.numeric" => HighlightStyle::Number,
            "property" | "variable.member" | "variable.object" => HighlightStyle::Property,
            // Operators
            "operator" | "punctuation.operator" => HighlightStyle::Operator,
            "punctuation"
            | "punctuation.bracket"
            | "punctuation.delimiter"
            | "punctuation.separator"
            | "punctuation.special"
            | "punctuation.accessor"
            | "separator" => HighlightStyle::Delimiter,
            // Tags / attributes
            "tag" => HighlightStyle::Tag,
            "attribute" | "attribute.name" => HighlightStyle::Attribute,
            // Labels
            "label" => HighlightStyle::Label,
            // Special
            "preproc" | "preprocessor" | "shebang" | "include" => HighlightStyle::Special,
            // Error
            "error" => HighlightStyle::Error,
            // Default
            _ => HighlightStyle::Plain,
        }
    }
}

// ── Regex-based fallback highlighter ─────────────────────────────────

/// Simple regex-free keyword highlighter for when tree-sitter grammars
/// are not available. Uses string matching for common keywords.
pub struct RegexHighlighter {
    keywords: Vec<(String, HighlightStyle)>,
}

impl RegexHighlighter {
    fn new() -> Self {
        Self {
            keywords: vec![
                // ── Rust keywords ────────────────────────────────────
                ("fn".into(), HighlightStyle::Function),
                ("let".into(), HighlightStyle::Keyword),
                ("mut".into(), HighlightStyle::Keyword),
                ("const".into(), HighlightStyle::Keyword),
                ("static".into(), HighlightStyle::Keyword),
                ("struct".into(), HighlightStyle::Keyword),
                ("enum".into(), HighlightStyle::Keyword),
                ("impl".into(), HighlightStyle::Keyword),
                ("trait".into(), HighlightStyle::Keyword),
                ("type".into(), HighlightStyle::Keyword),
                ("use".into(), HighlightStyle::Keyword),
                ("mod".into(), HighlightStyle::Keyword),
                ("pub".into(), HighlightStyle::Keyword),
                ("where".into(), HighlightStyle::Keyword),
                ("async".into(), HighlightStyle::Keyword),
                ("await".into(), HighlightStyle::Keyword),
                ("move".into(), HighlightStyle::Keyword),
                ("ref".into(), HighlightStyle::Keyword),
                ("dyn".into(), HighlightStyle::Keyword),
                ("as".into(), HighlightStyle::Keyword),
                ("in".into(), HighlightStyle::Keyword),
                ("unsafe".into(), HighlightStyle::Keyword),
                ("extern".into(), HighlightStyle::Keyword),
                // self / Self / super / crate — pink
                ("self".into(), HighlightStyle::SelfKw),
                ("Self".into(), HighlightStyle::SelfKw),
                ("super".into(), HighlightStyle::SelfKw),
                ("crate".into(), HighlightStyle::SelfKw),
                // Control flow — purple
                ("for".into(), HighlightStyle::ControlKeyword),
                ("while".into(), HighlightStyle::ControlKeyword),
                ("loop".into(), HighlightStyle::ControlKeyword),
                ("if".into(), HighlightStyle::ControlKeyword),
                ("else".into(), HighlightStyle::ControlKeyword),
                ("match".into(), HighlightStyle::ControlKeyword),
                ("return".into(), HighlightStyle::ControlKeyword),
                ("break".into(), HighlightStyle::ControlKeyword),
                ("continue".into(), HighlightStyle::ControlKeyword),
                // Booleans — peach (constant-like)
                ("true".into(), HighlightStyle::Boolean),
                ("false".into(), HighlightStyle::Boolean),
                // Rust enum variants — peach (constant)
                ("Some".into(), HighlightStyle::Constant),
                ("None".into(), HighlightStyle::Constant),
                ("Ok".into(), HighlightStyle::Constant),
                ("Err".into(), HighlightStyle::Constant),
                // ── Python keywords ─────────────────────────────────
                ("def".into(), HighlightStyle::Function),
                ("class".into(), HighlightStyle::Keyword),
                ("import".into(), HighlightStyle::ControlKeyword),
                ("from".into(), HighlightStyle::ControlKeyword),
                ("print".into(), HighlightStyle::FunctionCall),
                ("None".into(), HighlightStyle::Constant),
                ("True".into(), HighlightStyle::Boolean),
                ("False".into(), HighlightStyle::Boolean),
                ("pass".into(), HighlightStyle::Keyword),
                ("raise".into(), HighlightStyle::ControlKeyword),
                ("try".into(), HighlightStyle::ControlKeyword),
                ("except".into(), HighlightStyle::ControlKeyword),
                ("finally".into(), HighlightStyle::ControlKeyword),
                ("with".into(), HighlightStyle::ControlKeyword),
                ("is".into(), HighlightStyle::Keyword),
                ("and".into(), HighlightStyle::Keyword),
                ("or".into(), HighlightStyle::Keyword),
                ("not".into(), HighlightStyle::Keyword),
                ("lambda".into(), HighlightStyle::Keyword),
                ("yield".into(), HighlightStyle::ControlKeyword),
                ("global".into(), HighlightStyle::Keyword),
                ("nonlocal".into(), HighlightStyle::Keyword),
                ("assert".into(), HighlightStyle::FunctionCall),
                ("del".into(), HighlightStyle::Keyword),
                ("elif".into(), HighlightStyle::ControlKeyword),
                // ── JS/TS keywords ──────────────────────────────────
                ("function".into(), HighlightStyle::Function),
                ("var".into(), HighlightStyle::Keyword),
                ("extends".into(), HighlightStyle::Keyword),
                ("new".into(), HighlightStyle::Keyword),
                ("this".into(), HighlightStyle::SelfKw),
                ("typeof".into(), HighlightStyle::Keyword),
                ("instanceof".into(), HighlightStyle::Keyword),
                ("export".into(), HighlightStyle::ControlKeyword),
                ("default".into(), HighlightStyle::Keyword),
                ("null".into(), HighlightStyle::Constant),
                ("undefined".into(), HighlightStyle::Constant),
                ("console".into(), HighlightStyle::Builtin),
                ("require".into(), HighlightStyle::FunctionCall),
                ("interface".into(), HighlightStyle::Keyword),
                ("implements".into(), HighlightStyle::Keyword),
                ("readonly".into(), HighlightStyle::Keyword),
                ("declare".into(), HighlightStyle::Keyword),
            ],
        }
    }

    /// Highlight a line of text using keyword matching.
    pub fn highlight_line(&self, text: &str, language: Option<Language>) -> Vec<HighlightSpan> {
        let mut spans = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0usize;
        let len = chars.len();

        // ── Special handling for Markdown ──────────────────────────────────
        if language == Some(Language::Markdown) {
            return self.highlight_markdown_line(text);
        }

        // ── Special handling for GitLog buffer ──────────────────────────
        if language == Some(Language::GitLog) {
            // Look for "commit " at the beginning of the line (ignoring leading spaces)
            let trimmed = text.trim_start();
            if let Some(rest) = trimmed.strip_prefix("commit ") {
                // Find the start of the hash in the original string
                let prefix_len = text.find("commit ").unwrap_or(0) + "commit ".len();
                let mut end = prefix_len;
                let bytes = text.as_bytes();
                while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                    end += 1;
                }
                if end > prefix_len {
                    spans.push(HighlightSpan {
                        start: prefix_len,
                        end,
                        style: HighlightStyle::CommitHash, // Or use Constant if you prefer
                    });
                    // No further tokenization needed for this line – return early.
                    return spans;
                }
            }
            // Fall through to normal highlighting for non‑commit lines (e.g. file names)
        }

        // ── Normal highlighting (keywords, strings, numbers, etc.) ──────
        while i < len {
            let ch = chars[i];

            // Skip whitespace
            if ch.is_whitespace() {
                i += 1;
                continue;
            }

            // Single-line comment (Rust / JS / TS)
            if ch == '/' && i + 1 < len && chars[i + 1] == '/' {
                let is_doc = i + 2 < len && (chars[i + 2] == '/' || chars[i + 2] == '!');
                spans.push(HighlightSpan {
                    start: i,
                    end: len,
                    style: if is_doc {
                        HighlightStyle::DocComment
                    } else {
                        HighlightStyle::Comment
                    },
                });
                break;
            }
            // Hash comment (Python)
            if ch == '#' {
                spans.push(HighlightSpan {
                    start: i,
                    end: len,
                    style: HighlightStyle::Comment,
                });
                break;
            }

            // String literals
            if ch == '"' || ch == '\'' {
                let quote = ch;
                let start = i;
                i += 1;
                while i < len && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < len {
                        // Mark escape sequence
                        let esc_start = i;
                        i += 2; // skip escaped char
                        spans.push(HighlightSpan {
                            start: esc_start,
                            end: i,
                            style: HighlightStyle::Escape,
                        });
                    } else {
                        i += 1;
                    }
                }
                if i < len {
                    i += 1; // closing quote
                }
                spans.push(HighlightSpan {
                    start,
                    end: i,
                    style: HighlightStyle::String,
                });
                continue;
            }

            // Numbers
            if ch.is_ascii_digit() {
                let start = i;
                while i < len && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '_') {
                    i += 1;
                }
                spans.push(HighlightSpan {
                    start,
                    end: i,
                    style: HighlightStyle::Number,
                });
                continue;
            }

            // Identifiers / keywords
            if ch.is_alphabetic() || ch == '_' {
                let start = i;
                while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();

                // Check if this is a macro invocation (identifier followed by '!')
                let is_macro = i < len && chars[i] == '!';

                let style = if is_macro {
                    HighlightStyle::Macro
                } else {
                    self.keywords
                        .iter()
                        .find(|(kw, _)| *kw == word)
                        .map(|(_, s)| *s)
                        .unwrap_or_else(|| {
                            // Heuristic: PascalCase → Type, ALL_CAPS → Constant
                            let mut cs = word.chars();
                            if let Some(first) = cs.next() {
                                if first.is_uppercase() {
                                    let has_lower = cs.any(|c| c.is_lowercase());
                                    if !has_lower && word.len() > 1 {
                                        HighlightStyle::Constant // ALL_CAPS
                                    } else {
                                        HighlightStyle::Type // PascalCase
                                    }
                                } else {
                                    HighlightStyle::Plain
                                }
                            } else {
                                HighlightStyle::Plain
                            }
                        })
                };

                spans.push(HighlightSpan {
                    start,
                    end: i,
                    style,
                });
                continue;
            }

            // Operators / punctuation
            spans.push(HighlightSpan {
                start: i,
                end: i + 1,
                style: HighlightStyle::Operator,
            });
            i += 1;
        }

        spans
    }

    // ── Markdown highlighting ─────────────────────────────────────────

    /// Highlight a Markdown line with dedicated MD syntax handling.
    ///
    /// Block-level elements (headings, code fences, blockquotes, horizontal
    /// rules, list markers) are detected first and short-circuit the line.
    /// Remaining inline content is handed off to [`Self::highlight_md_inline`].
    fn highlight_markdown_line(&self, text: &str) -> Vec<HighlightSpan> {
        let mut spans = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();

        if len == 0 {
            return spans;
        }

        // Skip leading whitespace to detect block-level elements.
        let mut ws_end = 0;
        while ws_end < len && chars[ws_end] == ' ' {
            ws_end += 1;
        }
        if ws_end >= len {
            return spans; // whitespace-only line
        }

        // ── Heading (# ## ### …) ──────────────────────────────────────
        if chars[ws_end] == '#' {
            let mut i = ws_end;
            while i < len && chars[i] == '#' {
                i += 1;
            }
            spans.push(HighlightSpan {
                start: ws_end,
                end: i,
                style: HighlightStyle::Keyword,
            });
            // Skip space(s) after markers
            while i < len && chars[i] == ' ' {
                i += 1;
            }
            // Heading content
            if i < len {
                spans.push(HighlightSpan {
                    start: i,
                    end: len,
                    style: HighlightStyle::Type,
                });
            }
            return spans;
        }

        // ── Code fence (``` or ~~~) ───────────────────────────────────
        if chars[ws_end] == '`' || chars[ws_end] == '~' {
            let fence_ch = chars[ws_end];
            let mut i = ws_end;
            while i < len && chars[i] == fence_ch {
                i += 1;
            }
            if i - ws_end >= 3 {
                spans.push(HighlightSpan {
                    start: ws_end,
                    end: i,
                    style: HighlightStyle::Delimiter,
                });
                // Optional language identifier after fence
                while i < len && chars[i] == ' ' {
                    i += 1;
                }
                if i < len {
                    let lang_start = i;
                    while i < len && !chars[i].is_whitespace() {
                        i += 1;
                    }
                    if i > lang_start {
                        spans.push(HighlightSpan {
                            start: lang_start,
                            end: i,
                            style: HighlightStyle::Type,
                        });
                    }
                }
                return spans;
            }
            // Not a fence (fewer than 3 backticks / tildes) — fall through.
        }

        // ── Blockquote (>) ────────────────────────────────────────────
        if chars[ws_end] == '>' {
            spans.push(HighlightSpan {
                start: ws_end,
                end: ws_end + 1,
                style: HighlightStyle::Comment,
            });
            let mut i = ws_end + 1;
            // Skip optional space after '>'
            if i < len && chars[i] == ' ' {
                i += 1;
            }
            if i < len {
                spans.extend(Self::highlight_md_inline(&chars[i..], i));
            }
            return spans;
        }

        // ── Horizontal rule (---, ***, ___) ───────────────────────────
        {
            let ch = chars[ws_end];
            if ch == '-' || ch == '*' || ch == '_' {
                let mut i = ws_end;
                let mut count = 0;
                let mut valid = true;
                while i < len {
                    if chars[i] == ch {
                        count += 1;
                    } else if chars[i] != ' ' {
                        valid = false;
                        break;
                    }
                    i += 1;
                }
                if valid && count >= 3 {
                    spans.push(HighlightSpan {
                        start: 0,
                        end: len,
                        style: HighlightStyle::Comment,
                    });
                    return spans;
                }
            }
        }

        // ── List marker ───────────────────────────────────────────────
        let mut inline_start = 0;

        // Unordered: -, *, + followed by a space
        if (chars[ws_end] == '-' || chars[ws_end] == '*' || chars[ws_end] == '+')
            && ws_end + 1 < len
            && chars[ws_end + 1] == ' '
        {
            spans.push(HighlightSpan {
                start: ws_end,
                end: ws_end + 1,
                style: HighlightStyle::Operator,
            });
            inline_start = ws_end + 2;

            // Checkbox: [ ], [x], [X]
            if inline_start + 2 < len
                && chars[inline_start] == '['
                && (chars[inline_start + 1] == ' '
                    || chars[inline_start + 1] == 'x'
                    || chars[inline_start + 1] == 'X')
                && chars[inline_start + 2] == ']'
            {
                let check_style = if chars[inline_start + 1] == ' ' {
                    HighlightStyle::Delimiter
                } else {
                    HighlightStyle::Constant
                };
                spans.push(HighlightSpan {
                    start: inline_start,
                    end: inline_start + 3,
                    style: check_style,
                });
                inline_start += 3;
                // Skip space after checkbox
                if inline_start < len && chars[inline_start] == ' ' {
                    inline_start += 1;
                }
            }
        }
        // Ordered: 1. or 1) followed by a space
        else if chars[ws_end].is_ascii_digit() {
            let mut j = ws_end;
            while j < len && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j < len && (chars[j] == '.' || chars[j] == ')') && j + 1 < len && chars[j + 1] == ' '
            {
                spans.push(HighlightSpan {
                    start: ws_end,
                    end: j + 1,
                    style: HighlightStyle::Operator,
                });
                inline_start = j + 2;
            }
        }

        // ── Inline elements (code, bold, italic, links, images, …) ────
        spans.extend(Self::highlight_md_inline(
            &chars[inline_start..],
            inline_start,
        ));
        spans
    }

    /// Process inline Markdown elements within a slice of characters.
    ///
    /// `offset` is added to every span position so that the caller can pass
    /// a sub-slice starting partway through the line (e.g. after a list
    /// marker or blockquote `>`).
    ///
    /// Handled constructs:
    /// - Inline code   `` `code` ``, `` ``code`` ``
    /// - Bold          `**text**`, `__text__`
    /// - Bold+Italic   `***text***`, `___text___`
    /// - Italic        `*text*`  (single `_` intentionally skipped to avoid
    ///                            false positives with snake_case identifiers)
    /// - Strikethrough `~~text~~`
    /// - Images        `![alt](url)`
    /// - Links         `[text](url)`, `[text][ref]`, standalone `[text]`
    fn highlight_md_inline(chars: &[char], offset: usize) -> Vec<HighlightSpan> {
        let mut spans = Vec::new();
        let len = chars.len();
        let mut i = 0usize;

        while i < len {
            let ch = chars[i];

            // ── Inline code (`code` or ``code``) ─────────────────
            if ch == '`' {
                let open_start = i;
                let mut open_count = 0;
                while i < len && chars[i] == '`' {
                    open_count += 1;
                    i += 1;
                }
                let code_start = i;
                let mut found_close = false;
                while i < len {
                    if chars[i] == '`' {
                        let close_start = i;
                        let mut close_count = 0;
                        while i < len && chars[i] == '`' {
                            close_count += 1;
                            i += 1;
                        }
                        if close_count == open_count {
                            // Opening backticks
                            spans.push(HighlightSpan {
                                start: offset + open_start,
                                end: offset + code_start,
                                style: HighlightStyle::Delimiter,
                            });
                            // Code content
                            if code_start < close_start {
                                spans.push(HighlightSpan {
                                    start: offset + code_start,
                                    end: offset + close_start,
                                    style: HighlightStyle::String,
                                });
                            }
                            // Closing backticks
                            spans.push(HighlightSpan {
                                start: offset + close_start,
                                end: offset + i,
                                style: HighlightStyle::Delimiter,
                            });
                            found_close = true;
                            break;
                        }
                        // Mismatched close count — keep searching.
                    } else {
                        i += 1;
                    }
                }
                if !found_close {
                    // Unclosed — mark opening backticks as delimiter, rest as plain.
                    spans.push(HighlightSpan {
                        start: offset + open_start,
                        end: offset + code_start,
                        style: HighlightStyle::Delimiter,
                    });
                    if code_start < i {
                        spans.push(HighlightSpan {
                            start: offset + code_start,
                            end: offset + i,
                            style: HighlightStyle::Plain,
                        });
                    }
                }
                continue;
            }

            // ── Strikethrough (~~text~~) ─────────────────────────
            if ch == '~' && i + 1 < len && chars[i + 1] == '~' {
                let open_start = i;
                i += 2;
                let content_start = i;
                let mut found_close = false;
                while i < len {
                    if chars[i] == '~' && i + 1 < len && chars[i + 1] == '~' {
                        spans.push(HighlightSpan {
                            start: offset + open_start,
                            end: offset + content_start,
                            style: HighlightStyle::Operator,
                        });
                        if content_start < i {
                            spans.push(HighlightSpan {
                                start: offset + content_start,
                                end: offset + i,
                                style: HighlightStyle::Plain,
                            });
                        }
                        spans.push(HighlightSpan {
                            start: offset + i,
                            end: offset + i + 2,
                            style: HighlightStyle::Operator,
                        });
                        i += 2;
                        found_close = true;
                        break;
                    } else {
                        i += 1;
                    }
                }
                if !found_close {
                    // Unclosed — opening ~~ consumed, treat rest as plain.
                }
                continue;
            }

            // ── Image ![alt](url) ────────────────────────────────
            if ch == '!' && i + 1 < len && chars[i + 1] == '[' {
                let bang = i;
                let mut j = i + 2; // skip ![
                while j < len && chars[j] != ']' {
                    j += 1;
                }
                if j < len && j + 1 < len && chars[j + 1] == '(' {
                    let mut k = j + 2;
                    while k < len && chars[k] != ')' {
                        k += 1;
                    }
                    if k < len {
                        // ! marker
                        spans.push(HighlightSpan {
                            start: offset + bang,
                            end: offset + bang + 1,
                            style: HighlightStyle::Macro,
                        });
                        // [
                        spans.push(HighlightSpan {
                            start: offset + bang + 1,
                            end: offset + bang + 2,
                            style: HighlightStyle::Delimiter,
                        });
                        // alt text
                        if bang + 2 < j {
                            spans.push(HighlightSpan {
                                start: offset + bang + 2,
                                end: offset + j,
                                style: HighlightStyle::FunctionCall,
                            });
                        }
                        // ](
                        spans.push(HighlightSpan {
                            start: offset + j,
                            end: offset + j + 2,
                            style: HighlightStyle::Delimiter,
                        });
                        // url
                        if j + 2 < k {
                            spans.push(HighlightSpan {
                                start: offset + j + 2,
                                end: offset + k,
                                style: HighlightStyle::String,
                            });
                        }
                        // )
                        spans.push(HighlightSpan {
                            start: offset + k,
                            end: offset + k + 1,
                            style: HighlightStyle::Delimiter,
                        });
                        i = k + 1;
                        continue;
                    }
                }
                // Not a valid image — skip the '!' and continue.
                i += 1;
                continue;
            }

            // ── Link [text](url) or [text][ref] ──────────────────
            if ch == '[' {
                let open_bracket = i;
                let mut j = i + 1;
                while j < len && chars[j] != ']' {
                    j += 1;
                }
                if j < len {
                    let text_end = j;

                    // [text](url)
                    if j + 1 < len && chars[j + 1] == '(' {
                        let mut k = j + 2;
                        while k < len && chars[k] != ')' {
                            k += 1;
                        }
                        if k < len {
                            // [
                            spans.push(HighlightSpan {
                                start: offset + open_bracket,
                                end: offset + open_bracket + 1,
                                style: HighlightStyle::Delimiter,
                            });
                            // link text
                            if open_bracket + 1 < text_end {
                                spans.push(HighlightSpan {
                                    start: offset + open_bracket + 1,
                                    end: offset + text_end,
                                    style: HighlightStyle::FunctionCall,
                                });
                            }
                            // ](
                            spans.push(HighlightSpan {
                                start: offset + text_end,
                                end: offset + text_end + 2,
                                style: HighlightStyle::Delimiter,
                            });
                            // url
                            if text_end + 2 < k {
                                spans.push(HighlightSpan {
                                    start: offset + text_end + 2,
                                    end: offset + k,
                                    style: HighlightStyle::String,
                                });
                            }
                            // )
                            spans.push(HighlightSpan {
                                start: offset + k,
                                end: offset + k + 1,
                                style: HighlightStyle::Delimiter,
                            });
                            i = k + 1;
                            continue;
                        }
                    }

                    // [text][ref]
                    if j + 1 < len && chars[j + 1] == '[' {
                        let mut k = j + 2;
                        while k < len && chars[k] != ']' {
                            k += 1;
                        }
                        if k < len {
                            spans.push(HighlightSpan {
                                start: offset + open_bracket,
                                end: offset + open_bracket + 1,
                                style: HighlightStyle::Delimiter,
                            });
                            if open_bracket + 1 < text_end {
                                spans.push(HighlightSpan {
                                    start: offset + open_bracket + 1,
                                    end: offset + text_end,
                                    style: HighlightStyle::FunctionCall,
                                });
                            }
                            // ][ref]
                            spans.push(HighlightSpan {
                                start: offset + text_end,
                                end: offset + k + 1,
                                style: HighlightStyle::Delimiter,
                            });
                            i = k + 1;
                            continue;
                        }
                    }

                    // Standalone [text] (footnote ref, etc.)
                    spans.push(HighlightSpan {
                        start: offset + open_bracket,
                        end: offset + open_bracket + 1,
                        style: HighlightStyle::Delimiter,
                    });
                    if open_bracket + 1 < text_end {
                        spans.push(HighlightSpan {
                            start: offset + open_bracket + 1,
                            end: offset + text_end,
                            style: HighlightStyle::FunctionCall,
                        });
                    }
                    spans.push(HighlightSpan {
                        start: offset + text_end,
                        end: offset + text_end + 1,
                        style: HighlightStyle::Delimiter,
                    });
                    i = text_end + 1;
                    continue;
                }
                // Unclosed [ — just advance.
                i += 1;
                continue;
            }

            // ── Bold (**text**, __text__) or Bold+Italic (***text***, ___text___)
            if (ch == '*' || ch == '_') && i + 1 < len && chars[i + 1] == ch {
                let marker = ch;
                let open_start = i;
                let mut open_len = 2;
                i += 2;
                // Check for triple (bold + italic)
                if i < len && chars[i] == marker {
                    open_len = 3;
                    i += 1;
                }
                let content_start = i;
                let mut found_close = false;
                while i < len {
                    if chars[i] == marker {
                        let close_start = i;
                        let mut close_count = 0;
                        while i < len && chars[i] == marker {
                            close_count += 1;
                            i += 1;
                        }
                        if close_count >= open_len {
                            // Opening markers
                            spans.push(HighlightSpan {
                                start: offset + open_start,
                                end: offset + content_start,
                                style: HighlightStyle::Operator,
                            });
                            // Content
                            if content_start < close_start {
                                spans.push(HighlightSpan {
                                    start: offset + content_start,
                                    end: offset + close_start,
                                    style: HighlightStyle::Plain,
                                });
                            }
                            // Closing markers
                            let close_end = close_start + open_len;
                            spans.push(HighlightSpan {
                                start: offset + close_start,
                                end: offset + close_end,
                                style: HighlightStyle::Operator,
                            });
                            i = close_end;
                            found_close = true;
                            break;
                        }
                        // Not enough closing markers — keep searching.
                    } else {
                        i += 1;
                    }
                }
                if !found_close {
                    // Unclosed bold — opening markers consumed, rest is plain.
                }
                continue;
            }

            // ── Italic (*text*) ──────────────────────────────────
            // Note: _text_ italic is intentionally NOT handled to avoid false
            // positives with snake_case identifiers common in technical Markdown.
            if ch == '*' {
                let open_start = i;
                i += 1;
                let content_start = i;
                let mut found_close = false;
                while i < len {
                    if chars[i] == '*' {
                        // Opening *
                        spans.push(HighlightSpan {
                            start: offset + open_start,
                            end: offset + content_start,
                            style: HighlightStyle::Operator,
                        });
                        // Italic content
                        if content_start < i {
                            spans.push(HighlightSpan {
                                start: offset + content_start,
                                end: offset + i,
                                style: HighlightStyle::Plain,
                            });
                        }
                        // Closing *
                        spans.push(HighlightSpan {
                            start: offset + i,
                            end: offset + i + 1,
                            style: HighlightStyle::Operator,
                        });
                        i += 1;
                        found_close = true;
                        break;
                    } else {
                        i += 1;
                    }
                }
                // Unclosed italic — opening * already consumed, rest is plain.
                continue;
            }

            // Default: advance past any other character (no span emitted;
            // uncovered regions are implicitly Plain).
            i += 1;
        }

        spans
    }
}

// ── Main highlighter ───────────────────────────────────────────────

/// Syntax highlighter that uses tree-sitter when available,
/// falling back to regex-based keyword matching.
pub struct Highlighter {
    /// Per-language regex fallback highlighters.
    fallback: RegexHighlighter,
    /// Cached tree-sitter language + highlight config (only when static-grammars feature is on).
    #[cfg(feature = "static-grammars")]
    ts_configs: HashMap<String, TsConfig>,
}

#[cfg(feature = "static-grammars")]
struct TsConfig {
    language: tree_sitter::Language,
    highlight_config: tree_sitter_highlight::HighlightConfiguration,
}

/// Lazy-loaded list of recognized capture names for tree-sitter highlights.
/// The index in this list corresponds to the `Highlight(index)` value.
static HIGHLIGHT_NAMES: &[&str] = &[
    "comment",
    "line_comment",
    "block_comment",
    "doc_comment",
    "string",
    "string_literal",
    "character",
    "escape",
    "escape_sequence",
    "number",
    "integer",
    "float",
    "boolean",
    "self",
    "lifetime",
    "keyword",
    "keyword.control",
    "keyword.control.conditional",
    "keyword.control.import",
    "keyword.control.export",
    "keyword.control.return",
    "keyword.control.exception",
    "conditional",
    "repeat",
    "exception",
    "storage.type",
    "storage.modifier",
    "storage.class",
    "type",
    "type.identifier",
    "type.definition",
    "type.builtin",
    "function",
    "function.definition",
    "function.method",
    "function.macro",
    "macro",
    "macro_invocation",
    "function.call",
    "function.method.call",
    "function.builtin",
    "method",
    "method.call",
    "variable",
    "variable.builtin",
    "variable.parameter",
    "variable.member",
    "variable.object",
    "constant",
    "constant.builtin",
    "constant.character",
    "constant.numeric",
    "constant.language",
    "property",
    "operator",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.separator",
    "punctuation.special",
    "punctuation.accessor",
    "separator",
    "tag",
    "attribute",
    "attribute.name",
    "label",
    "preproc",
    "preprocessor",
    "shebang",
    "include",
    "error",
];

/// Lazy-loaded capture name → HighlightStyle mapping for tree-sitter highlights.
static HIGHLIGHT_STYLE_MAP: LazyLock<HashMap<String, HighlightStyle>> = LazyLock::new(|| {
    let captures = [
        // Comments
        ("comment", HighlightStyle::Comment),
        ("line_comment", HighlightStyle::Comment),
        ("block_comment", HighlightStyle::Comment),
        ("doc_comment", HighlightStyle::DocComment),
        // Strings
        ("string", HighlightStyle::String),
        ("string_literal", HighlightStyle::String),
        ("character", HighlightStyle::Character),
        // Escapes
        ("escape", HighlightStyle::Escape),
        ("escape_sequence", HighlightStyle::Escape),
        // Numbers
        ("number", HighlightStyle::Number),
        ("integer", HighlightStyle::Number),
        ("float", HighlightStyle::Number),
        ("boolean", HighlightStyle::Boolean),
        // Self / super / crate
        ("self", HighlightStyle::SelfKw),
        // Lifetimes
        ("lifetime", HighlightStyle::Lifetime),
        // Keywords
        ("keyword", HighlightStyle::Keyword),
        ("keyword.control", HighlightStyle::ControlKeyword),
        (
            "keyword.control.conditional",
            HighlightStyle::ControlKeyword,
        ),
        ("keyword.control.import", HighlightStyle::ControlKeyword),
        ("keyword.control.export", HighlightStyle::ControlKeyword),
        ("keyword.control.return", HighlightStyle::ControlKeyword),
        ("keyword.control.exception", HighlightStyle::ControlKeyword),
        ("conditional", HighlightStyle::ControlKeyword),
        ("repeat", HighlightStyle::ControlKeyword),
        ("exception", HighlightStyle::ControlKeyword),
        ("storage.type", HighlightStyle::Keyword),
        ("storage.modifier", HighlightStyle::Keyword),
        ("storage.class", HighlightStyle::Keyword),
        // Types
        ("type", HighlightStyle::Type),
        ("type.identifier", HighlightStyle::Type),
        ("type.definition", HighlightStyle::Type),
        ("type.builtin", HighlightStyle::Type),
        // Functions
        ("function", HighlightStyle::Function),
        ("function.definition", HighlightStyle::Function),
        ("function.method", HighlightStyle::Function),
        ("function.macro", HighlightStyle::Macro),
        ("macro", HighlightStyle::Macro),
        ("macro_invocation", HighlightStyle::Macro),
        ("function.call", HighlightStyle::FunctionCall),
        ("function.method.call", HighlightStyle::FunctionCall),
        ("function.builtin", HighlightStyle::FunctionCall),
        ("method", HighlightStyle::Function),
        ("method.call", HighlightStyle::FunctionCall),
        // Variables
        ("variable", HighlightStyle::Variable),
        ("variable.builtin", HighlightStyle::Builtin),
        ("variable.parameter", HighlightStyle::Variable),
        ("variable.member", HighlightStyle::Property),
        ("variable.object", HighlightStyle::Variable),
        // Constants
        ("constant", HighlightStyle::Constant),
        ("constant.builtin", HighlightStyle::Constant),
        ("constant.character", HighlightStyle::Character),
        ("constant.numeric", HighlightStyle::Number),
        ("constant.language", HighlightStyle::Constant),
        // Properties
        ("property", HighlightStyle::Property),
        // Operators
        ("operator", HighlightStyle::Operator),
        // Punctuation / delimiters
        ("punctuation", HighlightStyle::Delimiter),
        ("punctuation.bracket", HighlightStyle::Delimiter),
        ("punctuation.delimiter", HighlightStyle::Delimiter),
        ("punctuation.separator", HighlightStyle::Delimiter),
        ("punctuation.special", HighlightStyle::Delimiter),
        ("punctuation.accessor", HighlightStyle::Delimiter),
        ("separator", HighlightStyle::Delimiter),
        // Tags / attributes
        ("tag", HighlightStyle::Tag),
        ("attribute", HighlightStyle::Attribute),
        ("attribute.name", HighlightStyle::Attribute),
        // Labels
        ("label", HighlightStyle::Label),
        // Special
        ("preproc", HighlightStyle::Special),
        ("preprocessor", HighlightStyle::Special),
        ("shebang", HighlightStyle::Special),
        ("include", HighlightStyle::Special),
        // Error
        ("error", HighlightStyle::Error),
    ];
    captures.iter().map(|(k, v)| (k.to_string(), *v)).collect()
});

impl Highlighter {
    /// Create a new highlighter.
    pub fn new() -> Self {
        let fallback = RegexHighlighter::new();

        #[cfg(feature = "static-grammars")]
        let ts_configs = HashMap::new();

        Self {
            fallback,
            #[cfg(feature = "static-grammars")]
            ts_configs,
        }
    }

    /// Get or create the tree-sitter config for a language.
    #[cfg(feature = "static-grammars")]
    fn get_ts_config(&mut self, lang: Language) -> Option<&TsConfig> {
        let name = lang.as_str().to_string();
        if self.ts_configs.contains_key(&name) {
            return self.ts_configs.get(&name);
        }

        let (ts_lang, mut hl_config) = match lang {
            Language::Markdown => {
                let ts_lang = tree_sitter_md::LANGUAGE.into();
                let mut config = tree_sitter_highlight::HighlightConfiguration::new(
                    ts_lang,
                    "markdown",
                    tree_sitter_md::HIGHLIGHT_QUERY,
                    "", // no injections query typically
                    "", // no locals query typically
                )
                .ok()?;
                (config.language.clone(), config)
            }
            Language::Rust => {
                let ts_lang = tree_sitter_rust::LANGUAGE.into();
                let mut config = tree_sitter_highlight::HighlightConfiguration::new(
                    ts_lang,
                    "rust",
                    tree_sitter_rust::HIGHLIGHTS_QUERY,
                    tree_sitter_rust::INJECTIONS_QUERY,
                    "",
                )
                .ok()?;
                (config.language.clone(), config)
            }
            Language::JavaScript => {
                let ts_lang = tree_sitter_javascript::LANGUAGE.into();
                let mut config = tree_sitter_highlight::HighlightConfiguration::new(
                    ts_lang,
                    "javascript",
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::INJECTIONS_QUERY,
                    "",
                )
                .ok()?;
                (config.language.clone(), config)
            }
            Language::TypeScript => {
                let ts_lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
                let mut config = tree_sitter_highlight::HighlightConfiguration::new(
                    ts_lang,
                    "typescript",
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    "",
                    tree_sitter_typescript::LOCALS_QUERY,
                )
                .ok()?;
                (config.language.clone(), config)
            }
            Language::Python => {
                let ts_lang = tree_sitter_python::LANGUAGE.into();
                let mut config = tree_sitter_highlight::HighlightConfiguration::new(
                    ts_lang,
                    "python",
                    tree_sitter_python::HIGHLIGHTS_QUERY,
                    "",
                    "",
                )
                .ok()?;
                (config.language.clone(), config)
            }
            Language::PlainText => return None,
        };

        hl_config.configure(HIGHLIGHT_NAMES);
        let config_name = name.clone();
        self.ts_configs.insert(
            config_name,
            TsConfig {
                language: ts_lang,
                highlight_config: hl_config,
            },
        );
        self.ts_configs.get(&name)
    }

    /// Highlight a line of text, returning a list of colored spans.
    /// Uses tree-sitter if the `static-grammars` feature is enabled and the
    /// language has a compiled grammar; otherwise falls back to keyword matching.
    pub fn highlight_line(&mut self, text: &str, language: Option<Language>) -> Vec<HighlightSpan> {
        let lang = language.unwrap_or(Language::PlainText);

        #[cfg(feature = "static-grammars")]
        if let Some(_config) = self.get_ts_config(lang) {
            // For languages with a tree-sitter grammar, we still use the regex
            // fallback for *single lines* because full‑document highlighting
            // is done via `highlight_document`. Return the regex result.
        }

        // Use the regex fallback for per‑line highlighting (includes GitLog special case)
        self.fallback.highlight_line(text, language)
    }

    /// Highlight an entire document using tree-sitter.
    /// Returns a Vec of highlight spans for the entire document.
    #[cfg(feature = "static-grammars")]
    pub fn highlight_document(
        &mut self,
        source: &str,
        language: Option<Language>,
    ) -> Vec<HighlightSpan> {
        let lang = language.unwrap_or(Language::PlainText);
        let config = match self.get_ts_config(lang) {
            Some(c) => c,
            None => return Vec::new(),
        };

        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&config.language).is_err() {
            return Vec::new();
        }
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut ts_highlighter = tree_sitter_highlight::Highlighter::new();
        let events_result = ts_highlighter.highlight(
            &config.highlight_config,
            source.as_bytes(),
            None,
            |_| None, // no injection callback
        );

        let events = match events_result {
            Ok(events) => events,
            Err(_) => return Vec::new(),
        };

        // Convert tree-sitter highlight events to our spans.
        let mut style_stack: Vec<usize> = Vec::new();
        let mut spans = Vec::new();

        for event in events {
            let event = match event {
                Ok(e) => e,
                Err(_) => continue,
            };
            match event {
                tree_sitter_highlight::HighlightEvent::HighlightStart(hl) => {
                    style_stack.push(hl.0);
                }
                tree_sitter_highlight::HighlightEvent::HighlightEnd => {
                    style_stack.pop();
                }
                tree_sitter_highlight::HighlightEvent::Source { start, end } => {
                    let style_idx = *style_stack.last().unwrap_or(&usize::MAX);
                    let style = if style_idx < HIGHLIGHT_NAMES.len() {
                        let name = HIGHLIGHT_NAMES[style_idx];
                        HighlightStyle::from_capture(name)
                    } else {
                        HighlightStyle::Plain
                    };
                    spans.push(HighlightSpan { start, end, style });
                }
            }
        }

        spans
    }

    #[cfg(not(feature = "static-grammars"))]
    pub fn highlight_document(
        &mut self,
        _source: &str,
        _language: Option<Language>,
    ) -> Vec<HighlightSpan> {
        Vec::new()
    }

    /// Get highlight spans for a specific line from document-level highlight spans.
    /// `line_start_byte` and `line_end_byte` are byte offsets for the line in the document.
    /// `doc_spans` are the highlight spans for the entire document (byte offsets).
    pub fn spans_for_line(
        &self,
        line_start_byte: usize,
        line_end_byte: usize,
        doc_spans: &[HighlightSpan],
        text: &str,
    ) -> Vec<HighlightSpan> {
        let mut spans = Vec::new();

        for span in doc_spans {
            // Only include spans that overlap with this line.
            if span.end <= line_start_byte || span.start >= line_end_byte {
                continue;
            }

            let span_start = span.start.max(line_start_byte);
            let span_end = span.end.min(line_end_byte);
            let style = span.style;

            // Skip if span is empty
            if span_start >= span_end {
                continue;
            }

            // Convert byte offsets to character (grapheme) offsets within the line.
            let char_start = byte_to_char_offset(text, span_start - line_start_byte);
            let char_end = byte_to_char_offset(text, span_end - line_start_byte);

            spans.push(HighlightSpan {
                start: char_start,
                end: char_end,
                style,
            });
        }

        spans
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Convert a byte offset to a character (grapheme) offset in a string.
fn byte_to_char_offset(text: &str, byte_offset: usize) -> usize {
    if byte_offset >= text.len() {
        return text.graphemes(true).count();
    }
    text[..byte_offset].graphemes(true).count()
}

/// Render a highlighted line by emitting colored spans to the terminal.
/// Takes a slice of the line text (already sliced for wrap/scroll) and the
/// corresponding highlight spans (adjusted for the slice offset).
///
/// `guide_cols` is an optional set of grapheme-index columns where an indent
/// guide `|` should be drawn inline, replacing the space that is there.
/// Guides are rendered with a dim foreground while preserving the current
/// background (including the cursor-line DarkGrey).
///
/// ## Cursor-line background design
///
/// When `is_cursor_line` is true, `SetBackgroundColor(DarkGrey)` is set once
/// at the start and kept **persistent** for the entire line. All mid-line color
/// resets use `SetForegroundColor(Color::Reset)` (foreground-only) instead of
/// `ResetColor` (which would destroy the background). The final `ResetColor`
/// at the end restores both fg and bg to terminal defaults.
pub fn render_highlighted_line<W: std::io::Write>(
    writer: &mut W,
    text: &str,
    spans: &[HighlightSpan],
    is_cursor_line: bool,
    guide_cols: Option<&std::collections::HashSet<usize>>,
) -> std::io::Result<()> {
    use crossterm::execute;
    use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};

    const GUIDE_FG: Color = Color::Rgb {
        r: 40,
        g: 40,
        b: 58,
    };
    let cursor_bg = Color::DarkGrey;

    // Set cursor-line background FIRST — it stays active for the whole line.
    if is_cursor_line {
        execute!(writer, SetBackgroundColor(cursor_bg))?;
    }

    let chars: Vec<_> = text.graphemes(true).collect();
    let total_chars = chars.len();

    // Build a flat per-grapheme style map from spans.
    // None means "no span covers this grapheme" (plain/default fg).
    let mut fg_map: Vec<Option<HighlightStyle>> = vec![None; total_chars];
    for span in spans {
        let start = span.start.min(total_chars);
        let end = span.end.min(total_chars);
        for i in start..end {
            fg_map[i] = Some(span.style);
        }
    }

    // Walk every grapheme in order, emitting color escapes only when the
    // active style changes. This is a single pass — no overlay needed.
    let mut current_fg: Option<Color> = None; // None = terminal default

    let set_fg = |writer: &mut W, color: Option<Color>| -> std::io::Result<()> {
        match color {
            Some(c) => execute!(writer, SetForegroundColor(c)),
            None => {
                if is_cursor_line {
                    // Foreground-only reset: preserves the DarkGrey background.
                    execute!(writer, SetForegroundColor(Color::Reset))
                } else {
                    execute!(writer, ResetColor)
                }
            }
        }
    };

    for (i, g) in chars.iter().enumerate() {
        let is_guide = guide_cols.map_or(false, |gc| gc.contains(&i));

        let (wanted_fg, ch): (Option<Color>, &str) = if is_guide && *g == " " {
            (Some(GUIDE_FG), "|")
        } else {
            let wanted = match fg_map[i] {
                Some(style) if style != HighlightStyle::Plain => {
                    let (r, gb, b) = style.fg_rgb();
                    Some(Color::Rgb { r, g: gb, b })
                }
                _ => None,
            };
            (wanted, g)
        };

        if wanted_fg != current_fg {
            set_fg(writer, wanted_fg)?;
            if is_guide && *g == " " && is_cursor_line {
                execute!(writer, SetBackgroundColor(cursor_bg))?;
            }
            current_fg = wanted_fg;
        }

        execute!(writer, Print(ch))?;
    }

    // ── Indent guides beyond text length ─────────────────────────
    // Empty or short lines between indented blocks should still
    // display indent guides.  The main loop above only replaces
    // existing space characters; this section adds virtual spaces
    // + `|` at guide columns that fall past the end of the text.
    if let Some(gc) = guide_cols {
        let mut beyond: Vec<usize> = gc.iter().filter(|&&c| c >= total_chars).copied().collect();
        beyond.sort_unstable();

        let mut pos = total_chars;
        for col in beyond {
            // Pad with spaces up to the guide column
            if col > pos {
                if current_fg.is_some() {
                    set_fg(writer, None)?;
                    current_fg = None;
                }
                execute!(writer, Print(&" ".repeat(col - pos)))?;
            }
            // Draw the guide character
            if current_fg != Some(GUIDE_FG) {
                set_fg(writer, Some(GUIDE_FG))?;
                if is_cursor_line {
                    execute!(writer, SetBackgroundColor(cursor_bg))?;
                }
                current_fg = Some(GUIDE_FG);
            }
            execute!(writer, Print("|"))?;
            pos = col + 1;
        }
    }

    // Final reset: restore both fg and bg to terminal defaults.
    execute!(writer, ResetColor)?;

    // Final reset: restore both fg and bg to terminal defaults.
    execute!(writer, ResetColor)?;

    Ok(())
}
impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}
