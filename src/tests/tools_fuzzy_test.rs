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

// -------------------------------------------------------------------------
// Stage 3.5: build_tab_skip_blank_pattern
// -------------------------------------------------------------------------

#[test]
fn test_tab_skip_blank_old_has_blank_file_lacks() {
    // old has an extra blank line, file doesn't → should match
    let pattern = build_tab_skip_blank_pattern("foo\n\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Stage 3.5: old blank, file no blank"
    );
}

#[test]
fn test_tab_skip_blank_old_lacks_blank_file_has() {
    // old has no blank line, file has one → should match
    let pattern = build_tab_skip_blank_pattern("foo\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\n\nbar"),
        "Stage 3.5: old no blank, file has blank"
    );
}

#[test]
fn test_tab_skip_blank_space_only_line() {
    // old has space-only blank line, file has none → should match
    let pattern = build_tab_skip_blank_pattern("foo\n  \nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Stage 3.5: space-only blank line vs no blank line"
    );
    assert!(
        re.is_match("foo\n\nbar"),
        "Stage 3.5: space-only blank line vs empty blank line"
    );
}

#[test]
fn test_tab_skip_blank_tab_only_line() {
    // old has tab-only blank line, file has none → should match
    let pattern = build_tab_skip_blank_pattern("foo\n\t\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Stage 3.5: tab-only blank line vs no blank line"
    );
    assert!(
        re.is_match("foo\n\nbar"),
        "Stage 3.5: tab-only blank line vs empty blank line"
    );
}

#[test]
fn test_tab_skip_blank_multiple_blank_lines() {
    // old has multiple consecutive blank lines
    let pattern = build_tab_skip_blank_pattern("foo\n\n\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Stage 3.5: multiple blanks vs no blank"
    );
    assert!(
        re.is_match("foo\n\nbar"),
        "Stage 3.5: multiple blanks vs single blank"
    );
    assert!(
        re.is_match("foo\n\n\nbar"),
        "Stage 3.5: multiple blanks vs multiple blanks"
    );
}

#[test]
fn test_tab_skip_blank_leading_trailing_blank_lines() {
    // Leading and trailing blank lines should be ignored
    let pattern = build_tab_skip_blank_pattern("\n\nfoo\nbar\n\n");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Stage 3.5: leading/trailing blank lines ignored"
    );
    assert!(
        re.is_match("foo\n\nbar"),
        "Stage 3.5: leading/trailing blanks + middle blank"
    );
}

#[test]
fn test_tab_skip_blank_single_line_no_effect() {
    // Single non-blank line → should still match normally
    let pattern = build_tab_skip_blank_pattern("foo");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(re.is_match("foo"), "Stage 3.5: single line should match");
    assert!(
        !re.is_match("bar"),
        "Stage 3.5: single line should not match different content"
    );
}

#[test]
fn test_tab_skip_blank_only_blank_lines() {
    // Only blank lines → empty pattern (regex will fail to compile)
    let pattern = build_tab_skip_blank_pattern("\n  \n\t\n");
    assert!(
        pattern.is_empty(),
        "Stage 3.5: all-blank input should produce empty pattern"
    );
}

#[test]
fn test_tab_skip_blank_does_not_cross_non_blank_boundary() {
    // Safety: must NOT match when non-blank content differs
    let pattern = build_tab_skip_blank_pattern("foo\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        !re.is_match("foo\nbaz"),
        "Stage 3.5: must not match differing non-blank line"
    );
    assert!(
        !re.is_match("baz\nbar"),
        "Stage 3.5: must not match differing non-blank line"
    );
    assert!(
        !re.is_match("foox\nbar"),
        "Stage 3.5: must not match partial identifier"
    );
}

#[test]
fn test_tab_skip_blank_tab_fuzzy_behavior_preserved() {
    // Non-blank lines still benefit from tab-fuzzy flexibility
    let pattern = build_tab_skip_blank_pattern("    foo\n\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("\tfoo\nbar"),
        "Stage 3.5: tab-indent in non-blank line still matches"
    );
    assert!(
        re.is_match("  foo\n\nbar"),
        "Stage 3.5: space-indent in non-blank line still matches"
    );
}

// -------------------------------------------------------------------------
// Stage 4.5: build_full_skip_blank_pattern
// -------------------------------------------------------------------------

#[test]
fn test_full_skip_blank_old_has_blank_file_lacks() {
    let pattern = build_full_skip_blank_pattern("foo\n\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Stage 4.5: old blank, file no blank"
    );
}

#[test]
fn test_full_skip_blank_old_lacks_blank_file_has() {
    let pattern = build_full_skip_blank_pattern("foo\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\n\nbar"),
        "Stage 4.5: old no blank, file has blank"
    );
}

#[test]
fn test_full_skip_blank_space_tab_blank_lines() {
    // Various blank line types
    let pattern = build_full_skip_blank_pattern("foo\n  \n\t\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Stage 4.5: mixed blank lines ignored"
    );
    assert!(
        re.is_match("foo\n\nbar"),
        "Stage 4.5: mixed blanks vs empty blank"
    );
}

#[test]
fn test_full_skip_blank_intra_line_flexibility() {
    // Full-fuzzy flexibility within non-blank lines still works
    // (space becomes \s*)
    let pattern = build_full_skip_blank_pattern("fn  foo(  )\n\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("fn foo()\nbar"),
        "Stage 4.5: intra-line spacing differences absorbed"
    );
    assert!(
        re.is_match("fn   foo( )\n\nbar"),
        "Stage 4.5: varied intra-line spacing"
    );
}

#[test]
fn test_full_skip_blank_does_not_cross_non_blank_boundary() {
    let pattern = build_full_skip_blank_pattern("foo\nbar");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        !re.is_match("foo\nbaz"),
        "Stage 4.5: must not match differing non-blank line"
    );
    assert!(
        !re.is_match("foox\nbar"),
        "Stage 4.5: must not match partial identifier"
    );
}

#[test]
fn test_full_skip_blank_leading_trailing_blank_lines() {
    let pattern = build_full_skip_blank_pattern("\n\nfoo\nbar\n\n");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Stage 4.5: leading/trailing blanks ignored"
    );
}

#[test]
fn test_full_skip_blank_only_blank_lines() {
    let pattern = build_full_skip_blank_pattern("\n  \n\t\n");
    assert!(
        pattern.is_empty(),
        "Stage 4.5: all-blank input should produce empty pattern"
    );
}

#[test]
fn test_full_skip_blank_single_line() {
    let pattern = build_full_skip_blank_pattern("foo");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(re.is_match("foo"), "Stage 4.5: single line should match");
    assert!(
        !re.is_match("bar"),
        "Stage 4.5: single line should not match different content"
    );
}
