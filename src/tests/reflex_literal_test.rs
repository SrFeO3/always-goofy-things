//! Tests for `src/reflex_literal.rs`: the literal-matching safety filters used
//! by `--unsafe-reflex` auto-confirmation.
//!
//! Focus: `is_safe_grep_query` - Japanese (hiragana / katakana / kanji) queries
//! must be accepted so `grep_search` works on Japanese documents, while shell
//! metacharacters, regex metacharacters, control characters, quotes, and
//! non-covered scripts must still be rejected. Also locks the safety
//! invariants of `is_safe_subpath`.

use super::*;

// ---------------------------------------------------------------------------
// is_safe_grep_query: allowed
// ---------------------------------------------------------------------------

#[test]
fn ascii_queries_are_allowed() {
    assert!(is_safe_grep_query("abc"));
    assert!(is_safe_grep_query("ABC_123 -"));
    assert!(is_safe_grep_query("some query"));
}

#[test]
fn japanese_hiragana_are_allowed() {
    assert!(is_safe_grep_query("よ"));
    assert!(is_safe_grep_query("ひらがな"));
}

#[test]
fn japanese_katakana_are_allowed() {
    assert!(is_safe_grep_query("モニタリング"));
    assert!(is_safe_grep_query("モニト"));
}

#[test]
fn japanese_kanji_are_allowed() {
    assert!(is_safe_grep_query("必須"));
    assert!(is_safe_grep_query("株式会社"));
}

#[test]
fn mixed_ascii_and_japanese_are_allowed() {
    // These are literal queries from the qwen3.8 test logs that were denied
    // before the Japanese ranges were added.
    assert!(is_safe_grep_query("クラウドService"));
    assert!(is_safe_grep_query("Service受領"));
    assert!(is_safe_grep_query("モodel"));
    assert!(is_safe_grep_query("股 式"));
    assert!(is_safe_grep_query("must-例"));
}

#[test]
fn leading_caret_and_trailing_dollar_are_stripped() {
    assert!(is_safe_grep_query("^foo"));
    assert!(is_safe_grep_query("foo$"));
    assert!(is_safe_grep_query("^foo$"));
}

// ---------------------------------------------------------------------------
// is_safe_grep_query: rejected (malicious / unsafe / not covered)
// ---------------------------------------------------------------------------

#[test]
fn empty_query_is_rejected() {
    assert!(!is_safe_grep_query(""));
    // A lone "^" / "$" strips down to empty.
    assert!(!is_safe_grep_query("^"));
    assert!(!is_safe_grep_query("$"));
}

#[test]
fn shell_metacharacters_are_rejected() {
    for q in [
        "a; rm -rf", // command separator
        "a|b",       // pipe
        "a&b",       // background / AND
        "a>b",       // redirect out
        "a<b",       // redirect in
        "$(ls)",     // command substitution
        "`ls`",      // backticks
        "$HOME",     // environment variable
        "a\nb",      // newline
        "a\tb",      // tab
    ] {
        assert!(!is_safe_grep_query(q), "should reject {:?}", q);
    }
}

#[test]
fn regex_metacharacters_are_rejected() {
    for q in [
        "(a|b)", // group + alternation
        "[a-z]", // character class
        "{a,b}", // repetition
        "a*b",   // star
        "a+b",   // plus
        "a?b",   // question
        "a.b",   // any character
        "a\\b",  // escape
        "a^b",   // interior caret
        "a$b",   // interior dollar
    ] {
        assert!(!is_safe_grep_query(q), "should reject {:?}", q);
    }
}

#[test]
fn quotes_and_punctuation_are_rejected() {
    for q in ["\"", "'", "a\"b", "a'b", "§", "##", ":", ";", "@", "%", "!"] {
        assert!(!is_safe_grep_query(q), "should reject {:?}", q);
    }
}

#[test]
fn path_traversal_like_queries_are_rejected() {
    for q in ["..", "../etc", "./x", "a/../b"] {
        assert!(!is_safe_grep_query(q), "should reject {:?}", q);
    }
}

#[test]
fn non_covered_scripts_are_rejected() {
    // Hangul, Cyrillic, CJK Extension B (U+20BB7), CJK Compatibility
    // Ideograph (U+F902): all outside the allowed Japanese ranges.
    for q in ["한", "привет", "𠮷", "車"] {
        assert!(!is_safe_grep_query(q), "should reject {:?}", q);
    }
}

// ---------------------------------------------------------------------------
// is_safe_subpath: safety invariants
// ---------------------------------------------------------------------------

#[test]
fn subpath_accepts_workspace_relative_paths() {
    assert!(is_safe_subpath("artifacts"));
    assert!(is_safe_subpath("artifacts/checklist.md"));
    assert!(is_safe_subpath("./artifacts"));
    assert!(is_safe_subpath("src/main.rs"));
    assert!(is_safe_subpath("."));
    assert!(is_safe_subpath("./"));
}

#[test]
fn subpath_rejects_traversal_absolute_and_home() {
    for p in [
        "/etc/passwd",   // absolute
        "../etc/passwd", // parent traversal
        "a/../../b",     // nested traversal
        "a//b",          // double slash
        "~",             // home expansion
    ] {
        assert!(!is_safe_subpath(p), "should reject {:?}", p);
    }
}

#[test]
fn subpath_rejects_trailing_slash() {
    // Current behavior: `list_directory artifacts/` is denied in batch mode.
    // (Known friction point, intentionally unchanged here.)
    assert!(!is_safe_subpath("artifacts/"));
}
