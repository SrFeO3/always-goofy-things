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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_space_fuzzy_does_not_cross_newlines() {
        let pattern = build_space_fuzzy_pattern("fn foo()");
        let re = regex::Regex::new(&pattern).unwrap();
        // Should NOT match across newlines
        assert!(
            !re.is_match("fn\nfoo()"),
            "Stage 2 space_fuzzy must NOT match across newlines"
        );
        // Should match space variations
        assert!(
            re.is_match("fn  foo()"),
            "Stage 2 should match space count variations"
        );
        // Should NOT match tabs
        assert!(!re.is_match("fn\tfoo()"), "Stage 2 must NOT match tabs");
    }

    #[test]
    fn test_space_fuzzy_leading_spaces() {
        let pattern = build_space_fuzzy_pattern("    println!(\"hi\");");
        let re = regex::Regex::new(&pattern).unwrap();
        // Match fewer leading spaces
        assert!(
            re.is_match("  println!(\"hi\");"),
            "Stage 2 should match fewer leading spaces"
        );
        // Match more leading spaces
        assert!(
            re.is_match("          println!(\"hi\");"),
            "Stage 2 should match more leading spaces"
        );
        // Should NOT match tabs as indent
        assert!(
            !re.is_match("\t\tprintln!(\"hi\");"),
            "Stage 2 must NOT match tabs as leading indent"
        );
    }

    #[test]
    fn test_tab_fuzzy_does_not_cross_newlines() {
        let pattern = build_tab_fuzzy_pattern("fn foo()");
        let re = regex::Regex::new(&pattern).unwrap();
        // Should NOT match across newlines
        assert!(
            !re.is_match("fn\nfoo()"),
            "Stage 3 tab_fuzzy must NOT match across newlines"
        );
        // Should match horizontal space/tab differences
        assert!(
            re.is_match("fn  foo()"),
            "Stage 3 should match horizontal space differences"
        );
        assert!(
            re.is_match("fn\tfoo()"),
            "Stage 3 should match tab in place of space"
        );
    }

    #[test]
    fn test_full_fuzzy_crosses_newlines() {
        let pattern = build_full_fuzzy_pattern("fn foo()");
        let re = regex::Regex::new(&pattern).unwrap();
        assert!(
            re.is_match("fn\nfoo()"),
            "Stage 4 full_fuzzy must match across newlines"
        );
        assert!(
            re.is_match("fn\r\nfoo()"),
            "Stage 4 full_fuzzy must match across \r\n"
        );
    }

    #[test]
    fn test_tab_fuzzy_tab_vs_spaces_in_indent() {
        // Case where file uses tabs but old_string uses spaces for indent
        let pattern = build_tab_fuzzy_pattern("    println!(\"hi\");");
        let re = regex::Regex::new(&pattern).unwrap();
        assert!(
            re.is_match("\tprintln!(\"hi\");"),
            "Stage 3 should match tab-indent when old_string has spaces"
        );
    }
}
