/// Parsed snippet with placeholders
#[derive(Debug, Clone)]
pub struct ParsedSnippet {
    /// The literal text parts and placeholders
    pub parts: Vec<SnippetPart>,
    /// Tab stop order (indices into parts)
    pub tab_stops: Vec<usize>,
}

#[derive(Debug, Clone)]
pub enum SnippetPart {
    Literal(String),
    TabStop { index: usize, default: Option<String> },
    Placeholder { index: usize, default: String },
    Choice { index: usize, options: Vec<String> },
    Variable { name: String, default: Option<String> },
}

/// Parse a snippet string into structured parts
pub fn parse_snippet(snippet: &str) -> Result<ParsedSnippet, String> {
    let mut parts = Vec::new();
    let mut tab_stops = Vec::new();
    let mut current_literal = String::new();
    let mut chars = snippet.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' {
            if let Some(&next) = chars.peek() {
                if next == '$' {
                    // Escaped $                     current_literal.push('$');
                    chars.next();
                    continue;
                }

                if !current_literal.is_empty() {
                    parts.push(SnippetPart::Literal(std::mem::take(&mut current_literal)));
                }

                chars.next(); // consume the char after $
                if next.is_ascii_digit() {
                    // Simple tab stop: $0, $1, $2...
                    let index = next.to_digit(10).unwrap() as usize;
                    tab_stops.push(parts.len());
                    parts.push(SnippetPart::TabStop { index, default: None });
                } else if next == '{' {
                    // Complex placeholder: ${1:default} or ${1|opt1,opt2|}
                    let mut content = String::new();
                    let mut depth = 1;

                    while let Some(&inner) = chars.peek() {
                        chars.next();
                        match inner {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => content.push(inner),
                        }
                    }

                    if let Some((index, rest)) = parse_placeholder_content(&content) {
                        tab_stops.push(parts.len());
                        parts.push(rest);
                    }
                }
            } else {
                current_literal.push('$');
            }
        } else {
            current_literal.push(c);
        }
    }

    if !current_literal.is_empty() {
        parts.push(SnippetPart::Literal(current_literal));
    }

    Ok(ParsedSnippet { parts, tab_stops })
}

fn parse_placeholder_content(content: &str) -> Option<(usize, SnippetPart)> {
    let mut chars = content.chars();

    // Parse index
    let mut index_str = String::new();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            index_str.push(c);
        } else {
            break;
        }
    }
    let index: usize = index_str.parse().ok()?;

    // Check what follows the index
    let rest: String = chars.collect();

    if rest.is_empty() {
        // Simple tab stop: ${1}
        Some((index, SnippetPart::TabStop { index, default: None }))
    } else if rest.starts_with(':') {
        // Placeholder with default: ${1:default text}
        let default_text = rest[1..].to_string();
        Some((
            index,
            SnippetPart::Placeholder {
                index,
                default: default_text,
            },
        ))
    } else if rest.starts_with('|') && rest.ends_with('|') {
        // Choice: ${1|opt1,opt2,opt3|}
        let options_str = &rest[1..rest.len() - 1];
        let options: Vec<String> = options_str.split(',').map(String::from).collect();
        Some((index, SnippetPart::Choice { index, options }))
    } else {
        None
    }
}

/// Convert a parsed snippet to plain text (without placeholders)
/// Used when snippet support is not available
pub fn snippet_to_plain_text(snippet: &str) -> String {
    let mut result = String::new();
    let mut chars = snippet.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' {
            if let Some(&'$') = chars.peek() {
                result.push('$');
                chars.next();
                continue;
            }

            if let Some(&next) = chars.peek() {
                if next.is_ascii_digit() {
                    chars.next();
                    continue; // Skip tab stop
                } else if next == '{' {
                    chars.next();
                    let mut depth = 1;
                    while let Some(&inner) = chars.peek() {
                        chars.next();
                        match inner {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            ':' | '|' => continue,
                            _ => result.push(inner),
                        }
                    }
                    continue;
                }
            }
        }
        result.push(c);
    }

    result
}

pub fn parse_snippet_for_insert(snippet: &str) -> SnippetInsertResult {
    let mut text = String::new();
    let mut stops: Vec<(usize, usize)> = Vec::new();
    let mut final_offset = 0;
    let mut current_offset = 0;

    let chars: Vec<char> = snippet.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' {
            if i + 1 < chars.len() {
                let next = chars[i + 1];

                if next == '0' {
                    final_offset = current_offset;
                    i += 2;
                    continue;
                } else if next.is_ascii_digit() {
                    let stop_idx = next.to_digit(10).unwrap() as usize;
                    while stops.len() <= stop_idx {
                        stops.push((0, 0));
                    }
                    stops[stop_idx] = (current_offset, current_offset);
                    i += 2;
                    continue;
                } else if next == '{' {
                    let close = snippet[i + 2..].find('}');
                    if let Some(close_idx) = close {
                        let inner = &snippet[i + 2..i + 2 + close_idx];
                        if let Some(colon_idx) = inner.find(':') {
                            let num_str = &inner[..colon_idx];
                            let default_text = &inner[colon_idx + 1..];

                            if num_str == "0" {
                                text.push_str(default_text);
                                current_offset += default_text.chars().count();
                                final_offset = current_offset;
                            } else if let Ok(stop_idx) = num_str.parse::<usize>() {
                                let start = current_offset;
                                text.push_str(default_text);
                                current_offset += default_text.chars().count();
                                let end = current_offset;

                                while stops.len() <= stop_idx {
                                    stops.push((0, 0));
                                }
                                stops[stop_idx] = (start, end);
                            }
                        } else if let Ok(stop_idx) = inner.parse::<usize>() {
                            while stops.len() <= stop_idx {
                                stops.push((0, 0));
                            }
                            stops[stop_idx] = (current_offset, current_offset);
                        }
                        i += 2 + close_idx + 1;
                        continue;
                    }
                }
            }
        }

        text.push(chars[i]);
        current_offset += 1;
        i += 1;
    } // ← END of while loop

    // ← OUTSIDE the loop now
    let tab_stops: Vec<(usize, usize)> = stops
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| *idx > 0)
        .map(|(_, range)| range)
        .collect();

    if final_offset == 0 && !snippet.ends_with("$0") {
        final_offset = current_offset;
    }

    SnippetInsertResult {
        text,
        stops: tab_stops,
        final_offset,
    }
}

pub struct SnippetInsertResult {
    /// The plain text to insert
    pub text: String,
    /// Tab stops: (char_offset_start, char_offset_end) relative to insert position
    /// Sorted by tab stop number (1, 2, 3, ...)
    pub stops: Vec<(usize, usize)>,
    /// Final cursor offset ($0) relative to insert position
    pub final_offset: usize,
}
