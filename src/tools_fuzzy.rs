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

#[cfg(test)]
#[path = "tests/tools_fuzzy_test.rs"]
mod tests;
