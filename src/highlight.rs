//--+ highlight.rs
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
    fg_override: Option<HighlightStyle>,
) -> std::io::Result<()> {
    use crossterm::execute;
    use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};

    // Set cursor-line background FIRST — it stays active for the whole line.
    if is_cursor_line {
        execute!(writer, SetBackgroundColor(Color::DarkGrey))?;
    }

    let chars: Vec<_> = text.graphemes(true).collect();
    let total_chars = chars.len();

    if spans.is_empty() {
        // No highlights — just print plain text.
        if let Some(style) = fg_override {
            let (r, g, b) = style.fg_rgb();
            execute!(writer, SetForegroundColor(Color::Rgb { r, g, b }))?;
        }
        execute!(writer, Print(text))?;
        // On cursor line: reset fg only, preserving background.
        if is_cursor_line {
            execute!(writer, SetForegroundColor(Color::Reset))?;
        } else if fg_override.is_some() {
            execute!(writer, ResetColor)?;
        }
        return Ok(());
    }

    let mut char_idx = 0usize;
    for span in spans {
        // Skip spans that end before our current position
        if span.end <= char_idx {
            continue;
        }
        let start = span.start.max(char_idx).min(total_chars);
        let end = span.end.min(total_chars);

        // Print gap (unhighlighted text) before this span — whitespace, plain
        // identifiers, etc. The cursor-line background is still active.
        if start > char_idx {
            execute!(writer, Print(&chars[char_idx..start].join("")))?;
        }

        if start >= end {
            char_idx = end;
            continue;
        }

        // Emit the text for this span with its foreground color.
        let style = fg_override.unwrap_or(span.style);
        let (r, g, b) = style.fg_rgb();

        // Only set foreground if it's not Plain (avoid unnecessary escapes).
        // Plain inherits the terminal default fg, which works on top of
        // the cursor-line DarkGrey background.
        if style != HighlightStyle::Plain {
            execute!(writer, SetForegroundColor(Color::Rgb { r, g, b }))?;
        }

        let segment: String = chars[start..end].join("");
        execute!(writer, Print(&segment))?;

        // After a colored span, reset ONLY the foreground so syntax spans
        // don't leak color. The cursor-line background is preserved.
        if style != HighlightStyle::Plain {
            if is_cursor_line {
                execute!(writer, SetForegroundColor(Color::Reset))?;
            } else {
                execute!(writer, ResetColor)?;
            }
        }

        char_idx = end;
    }

    // Print any remaining characters after the last span.
    if char_idx < total_chars {
        execute!(writer, Print(&chars[char_idx..].join("")))?;
    }

    // Final reset: restore both fg and bg to terminal defaults.
    if is_cursor_line {
        execute!(writer, ResetColor)?;
    }

    Ok(())
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}
