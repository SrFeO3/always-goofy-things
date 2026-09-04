//! Fuzzy matching utilities for LLM tool requests.
//!
//! Provides a multi-stage matching pipeline to improve the robustness of text replacement:
//!   Stage 1 (exact_match): Perfect character-for-character match.
//!   Stage 2 (space_fuzzy_match): Flexible horizontal space runs ([ ]+).
//!   Stage 3 (tab_fuzzy_match): Flexible spaces and tabs ([ \t]*).
//!   Stage 4 (full_fuzzy_match): Flexible all-whitespace, including newlines and CRLF/LF.

/// Stage 2: Space-fuzzy pattern.
///
/// - Continuous space runs are replaced with `[ ]+`.
/// - All other characters are escaped.
///
/// Does not match across newlines or include tabs.
pub fn build_space_fuzzy_pattern(s: &str) -> String {
    let mut pattern = String::with_capacity(s.len() * 2);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if chars[i] == ' ' {
            pattern.push_str(r"[ ]+");
            while i < len && chars[i] == ' ' {
                i += 1;
            }
        } else {
            pattern.push_str(&regex::escape(&chars[i].to_string()));
            i += 1;
        }
    }
    pattern
}

/// Stage 3: Tab-fuzzy pattern.
///
/// - Spaces and tabs are replaced with `[ \t]*`.
/// - All other characters are escaped.
///
/// Does not match across newlines.
pub fn build_tab_fuzzy_pattern(s: &str) -> String {
    let mut pattern = String::with_capacity(s.len() * 2);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if chars[i] == '\t' || chars[i] == ' ' {
            pattern.push_str(r"[ \t]*");
            while i < len && (chars[i] == '\t' || chars[i] == ' ') {
                i += 1;
            }
        } else {
            pattern.push_str(&regex::escape(&chars[i].to_string()));
            i += 1;
        }
    }
    pattern
}

/// Stage 4: Full-fuzzy pattern.
///
/// - Spaces are replaced with `\s*`.
/// - All other characters are escaped.
///
/// This stage absorbs line breaks, mixed CRLF/LF differences, and arbitrary whitespace runs.
pub fn build_full_fuzzy_pattern(s: &str) -> String {
    let mut pattern = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if c == ' ' {
            pattern.push_str(r"\s*");
        } else {
            pattern.push_str(&regex::escape(&c.to_string()));
        }
    }
    pattern
}

/// Helper: join per-line patterns with a blank-line-tolerant connector.
/// The connector `\n(?:[ \t]*\n)*` allows zero or more blank lines
/// (empty or space/tab-only) between non-blank lines.
fn join_non_blank_lines(parts: Vec<String>) -> String {
    if parts.is_empty() {
        return String::new();
    }
    parts.join(r"\n(?:[ \t]*\n)*")
}

/// Stage 3.5: Tab-fuzzy pattern with blank-line tolerance.
///
/// - Blank lines (empty or space/tab-only) in the source string are ignored,
///   allowing the pattern to match files with or without those blank lines.
/// - Non-blank lines are processed by [`build_tab_fuzzy_pattern`].
/// - Does NOT cross non-blank line boundaries; only blank-line flexibility.
pub fn build_tab_skip_blank_pattern(s: &str) -> String {
    let parts: Vec<String> = s
        .lines()
        .filter(|line| !line.chars().all(|c| c == ' ' || c == '\t'))
        .map(build_tab_fuzzy_pattern)
        .collect();
    join_non_blank_lines(parts)
}

/// Stage 4.5: Full-fuzzy pattern with blank-line tolerance.
///
/// - Blank lines (empty or space/tab-only) in the source string are ignored,
///   allowing the pattern to match files with or without those blank lines.
/// - Non-blank lines are processed by [`build_full_fuzzy_pattern`].
/// - Does NOT cross non-blank line boundaries; only blank-line flexibility.
pub fn build_full_skip_blank_pattern(s: &str) -> String {
    let parts: Vec<String> = s
        .lines()
        .filter(|line| !line.chars().all(|c| c == ' ' || c == '\t'))
        .map(build_full_fuzzy_pattern)
        .collect();
    join_non_blank_lines(parts)
}

// ---------------------------------------------------------------------------
// Escape-mismatch feedback (str_replace_editor)
// ---------------------------------------------------------------------------

/// Resolve JSON escapes (`\"`, `\\`, `\n`, `\t`, `\r`, `\/`, `\b`, `\f`, `\uXXXX`).
/// Unknown escapes and a lone trailing `\` stay literal.
/// Returns the resolved string and per-kind escape counts.
fn resolve_json_escapes(s: &str) -> (String, Vec<(char, usize)>) {
    let mut out = String::with_capacity(s.len());
    let mut counts: Vec<(char, usize)> = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' || i + 1 >= chars.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        let resolved: Option<char> = match next {
            '"' => Some('"'),
            '\\' => Some('\\'),
            '/' => Some('/'),
            'n' => Some('\n'),
            't' => Some('\t'),
            'r' => Some('\r'),
            'b' => Some('\u{0008}'),
            'f' => Some('\u{000C}'),
            'u' => {
                // \uXXXX (4 hex digits); otherwise unresolved.
                let hex_end = (i + 6).min(chars.len());
                let hex = &chars[i + 2..hex_end];
                if hex.len() == 4 && hex.iter().all(|h| h.is_ascii_hexdigit()) {
                    let hex_str: String = hex.iter().collect();
                    u32::from_str_radix(&hex_str, 16)
                        .ok()
                        .and_then(char::from_u32)
                } else {
                    None
                }
            }
            _ => None,
        };
        match resolved {
            Some(ch) => {
                out.push(ch);
                if let Some((_, n)) = counts.iter_mut().find(|(k, _)| *k == next) {
                    *n += 1;
                } else {
                    counts.push((next, 1));
                }
                i += if next == 'u' { 6 } else { 2 };
            }
            None => {
                // Unknown/invalid escape: keep the backslash.
                out.push(c);
                i += 1;
            }
        }
    }
    (out, counts)
}

/// Human-readable description of an escape kind for the feedback message.
fn describe_escape_kind(kind: char) -> &'static str {
    match kind {
        '"' => "backslash + quote",
        '\\' => "backslash + backslash",
        'n' => "backslash + n",
        't' => "backslash + t",
        'r' => "backslash + r",
        'b' => "backslash + b",
        'f' => "backslash + f",
        '/' => "backslash + slash",
        'u' => "unicode escape",
        _ => "unknown escape",
    }
}

/// Whether `s` contains a backslash immediately followed by `kind`.
fn contains_escape_kind(s: &str, kind: char) -> bool {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.next() == Some(kind) {
            return true;
        }
    }
    false
}

/// Truncate `s` to at most `max` characters, appending `...` when truncated.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{}...", head)
    }
}

/// Feedback for an over-escaped `old_string` that failed all match stages:
/// if the same text without JSON backslash escapes exists in `content`,
/// return a hint telling the caller to fix its escapes. Feedback only --
/// nothing is resolved or written; `None` when there is nothing to report.
pub fn escape_mismatch_feedback(
    old_string: &str,
    new_string: &str,
    path: &str,
    content: &str,
) -> Option<String> {
    let (resolved, counts) = resolve_json_escapes(old_string);
    if counts.is_empty() {
        return None; // no resolvable escapes
    }
    let occurrences: Vec<usize> = content.match_indices(&resolved).map(|(i, _)| i).collect();
    if occurrences.is_empty() {
        return None; // resolved text not in the file
    }

    let first = occurrences[0];
    let line = content[..first].matches('\n').count() + 1;
    // Snippet: match-start line, first ~120 chars.
    let line_start = content[..first].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = content[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(content.len());
    let snippet = truncate_chars(&content[line_start..line_end], 120);

    let mut msg = String::new();
    msg.push_str(
        "[ESCAPE_HINT] old_string contains backslash escapes that do not exist in the file:\n",
    );
    for (kind, n) in &counts {
        let seq = if *kind == 'u' {
            "\\uXXXX".to_string()
        } else {
            format!("\\{}", kind)
        };
        msg.push_str(&format!(
            "  - `{}` ({}) x {}\n",
            seq,
            describe_escape_kind(*kind),
            n
        ));
    }
    msg.push_str(&format!(
        "Removing those backslashes, the text matches {} at {} place{} (line {}):\n",
        path,
        occurrences.len(),
        if occurrences.len() == 1 { "" } else { "s" },
        line
    ));
    msg.push_str(&format!("  > {}\n", snippet));
    msg.push_str(
        "The tool argument is a JSON string: the JSON decoder resolves standard escapes before \
the tool receives the text (`\\\"` -> `\"`, `\\n` -> newline). The file contains only the \
resolved characters. Send the text exactly as it appears in the file: do NOT add backslashes.\n",
    );
    if counts
        .iter()
        .any(|(kind, _)| contains_escape_kind(new_string, *kind))
    {
        msg.push_str(
            "Note: your new_string contains the same escapes; they will be written to the file \
literally unless you fix them.",
        );
    }
    Some(msg)
}

#[cfg(test)]
#[path = "tests/tools_fuzzy_test.rs"]
mod tests;
