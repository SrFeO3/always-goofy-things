//! Tests for `src/session.rs`: tool policy and handover budget.

use super::*;

#[test]
fn inherit_allows_everything() {
    for name in ["read_file", "write_file", "kb_insert", "anything"] {
        assert!(ToolPolicy::Inherit.allows(name));
    }
}

#[test]
fn allow_list_narrows() {
    let kb_extract: ToolPolicy = ToolPolicy::AllowList(&[
        "kb_schema",
        "kb_search",
        "kb_read",
        "kb_insert",
        "kb_update",
    ]);
    assert!(kb_extract.allows("kb_read"));
    assert!(!kb_extract.allows("write_file"));
    assert!(!kb_extract.allows("read_file"));
    assert!(!kb_extract.allows(""));
}

#[test]
fn handover_empty_reports_is_base_only() {
    assert_eq!(assemble_instruction("goal", &[], 100), "goal");
}

#[test]
fn handover_fits_all_in_order() {
    let out = assemble_instruction("goal", &["r1", "r2"], 100);
    assert_eq!(out, "goal\nr1\nr2");
}

#[test]
fn handover_overflow_keeps_newest() {
    // "r1"(3) + "r2"(3), budget 4: newest r2 kept, r1 dropped.
    let out = assemble_instruction("goal", &["r1", "r2"], 4);
    assert_eq!(out, "goal\nr2");
}

#[test]
fn handover_single_huge_report_never_dropped_silently() {
    let out = assemble_instruction("goal", &["very-long-report"], 4);
    assert_eq!(out, "goal\nvery-long-report");
}

#[test]
fn handover_multibyte_counts_chars_not_bytes() {
    // 3 chars (9 bytes): fits a 4-char budget.
    let out = assemble_instruction("g", &["あいう"], 4);
    assert_eq!(out, "g\nあいう");
}
