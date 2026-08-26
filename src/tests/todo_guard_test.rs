//! Tests for `src/todo_guard.rs` (LLM output guards for the todo modes).

use super::*;

/// Removes the given workspace files on drop (panic-safe cleanup) for tests
/// that create Goal artifacts under `./artifacts/`.
struct ArtifactFilesGuard(Vec<String>);

impl ArtifactFilesGuard {
    fn new(paths: &[&str]) -> Self {
        std::fs::create_dir_all("./artifacts").unwrap();
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
        Self(paths.iter().map(|p| p.to_string()).collect())
    }
}

impl Drop for ArtifactFilesGuard {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[test]
fn test_one_line_report_truncates() {
    let long = "a".repeat(400);
    let report = one_line_report(&format!("line1\nline2 {}", long));
    assert!(report.ends_with("..."));
    // Truncation happens at the fuzzy limit (360), not the advertised 300.
    assert_eq!(report.chars().count(), 363); // 360 chars + "..."
    assert!(!report.contains('\n'));
}

#[test]
fn test_one_line_report_keeps_report_within_fuzzy_limit() {
    // At the fuzzy limit (over the advertised 300): kept whole.
    let report = one_line_report(&"a".repeat(HANDOVER_REPORT_FUZZY_MAX_CHARS));
    assert_eq!(report.chars().count(), HANDOVER_REPORT_FUZZY_MAX_CHARS);
    assert!(!report.ends_with("..."));
}

#[test]
fn test_one_line_report_short() {
    let report = one_line_report("Status: done\nOutput: a.md");
    assert_eq!(report, "Status: done Output: a.md");
}

// ---------------------------------------------------------------------------
// Improvement 1: Output path preservation in the handover entry.
// ---------------------------------------------------------------------------

#[test]
fn test_extract_output_paths_plain_comma_list() {
    let report =
        "- Status: done\n- Output: artifacts/a.md, artifacts/b.md\n- Findings: ok\n- Next: none";
    assert_eq!(
        extract_output_paths(report),
        vec!["artifacts/a.md", "artifacts/b.md"]
    );
}

#[test]
fn test_extract_output_paths_backticks_and_dashless_prefix() {
    assert_eq!(
        extract_output_paths("- Output: `artifacts/a.md`, `artifacts/b.md`"),
        vec!["artifacts/a.md", "artifacts/b.md"]
    );
    assert_eq!(
        extract_output_paths("Output: artifacts/a.md"),
        vec!["artifacts/a.md"]
    );
}

#[test]
fn test_extract_output_paths_none_and_missing_field() {
    assert!(extract_output_paths("- Status: done\n- Output: none").is_empty());
    assert!(extract_output_paths("- Output: (none)").is_empty());
    assert!(extract_output_paths("- Status: done\n- Findings: ok").is_empty());
}

#[test]
fn test_extract_output_paths_dedups() {
    assert_eq!(
        extract_output_paths("- Output: artifacts/a.md, artifacts/a.md"),
        vec!["artifacts/a.md"]
    );
}

#[test]
fn test_build_handover_entry_keeps_untruncated_outputs_line() {
    let long = "x".repeat(400);
    let report = format!(
        "- Status: done\n- Output: artifacts/findings.md\n- Findings: {}",
        long
    );
    let entry = build_handover_entry("- Task 2", &report);
    assert!(
        entry.starts_with("- Task 2: - Status: done - Output: artifacts/findings.md - Findings: x")
    );
    assert!(entry.ends_with("\noutputs: artifacts/findings.md"));
}

#[test]
fn test_build_handover_entry_no_outputs_line_when_none() {
    let entry = build_handover_entry("- Task 2", "- Status: done\n- Output: none");
    assert_eq!(entry, "- Task 2: - Status: done - Output: none");
}

// ---------------------------------------------------------------------------
// Improvement 2: final answer resolution from the Goal artifact.
// ---------------------------------------------------------------------------

#[test]
fn test_extract_goal_artifact_paths_basic() {
    let md =
        "# Plan\n\n## Goal\nSave the report to artifacts/final-report.md.\n\n## Tasks\n- [ ] a\n";
    assert_eq!(
        extract_goal_artifact_paths(md),
        vec!["artifacts/final-report.md"]
    );
}

#[test]
fn test_extract_goal_artifact_paths_backticks_trailing_punct_dedup() {
    let md = "## Goal\nWrite `artifacts/comparison.md` and artifacts/comparison.md.\n";
    assert_eq!(
        extract_goal_artifact_paths(md),
        vec!["artifacts/comparison.md"]
    );
}

#[test]
fn test_extract_goal_artifact_paths_ignores_other_sections() {
    let md = "# Plan\n\n## Goal\nDo things.\n\n## Tasks\n- [ ] write artifacts/from-task.md\n";
    assert!(extract_goal_artifact_paths(md).is_empty());
}

#[test]
fn test_extract_goal_artifact_paths_rejects_escape_and_non_files() {
    let md =
        "## Goal\nUse ../outside.md, artifacts/../evil.md, artifacts/ (dir) and artifacts/ok.md.\n";
    assert_eq!(extract_goal_artifact_paths(md), vec!["artifacts/ok.md"]);
}

#[test]
fn test_llm_guard_final_answer_reads_goal_artifact() {
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_goal_artifact.md"]);
    std::fs::write("./artifacts/_agt_test_goal_artifact.md", "FINAL CONTENT\n").unwrap();
    let md = "# Plan\n\n## Goal\nSave the report to artifacts/_agt_test_goal_artifact.md.\n";
    assert_eq!(
        llm_guard_final_answer(md, "last report", "fallback"),
        "FINAL CONTENT\n"
    );
}

#[test]
fn test_llm_guard_final_answer_prefers_last_goal_artifact() {
    // A Goal may name intermediate artifacts before the final deliverable;
    // the LAST one is the final result and must win.
    let _guard = ArtifactFilesGuard::new(&[
        "./artifacts/_agt_test_intermediate.md",
        "./artifacts/_agt_test_final.md",
    ]);
    std::fs::write("./artifacts/_agt_test_intermediate.md", "INTERMEDIATE\n").unwrap();
    std::fs::write("./artifacts/_agt_test_final.md", "FINAL\n").unwrap();
    let md = "## Goal\nCreate artifacts/_agt_test_intermediate.md, then artifacts/_agt_test_final.md. Keep artifacts/_agt_test_final.md as the final result.\n";
    assert_eq!(llm_guard_final_answer(md, "", "fallback"), "FINAL\n");
}

#[test]
fn test_llm_guard_final_answer_skips_missing_or_empty_artifact() {
    let _guard = ArtifactFilesGuard::new(&[
        "./artifacts/_agt_test_empty.md",
        "./artifacts/_agt_test_second.md",
    ]);
    std::fs::write("./artifacts/_agt_test_empty.md", "").unwrap();
    std::fs::write("./artifacts/_agt_test_second.md", "SECOND\n").unwrap();
    let md = "## Goal\nSave to artifacts/_agt_test_missing.md then artifacts/_agt_test_empty.md then artifacts/_agt_test_second.md.\n";
    assert_eq!(llm_guard_final_answer(md, "", "fallback"), "SECOND\n");
}

#[test]
fn test_llm_guard_final_answer_caps_long_artifact() {
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_long.md"]);
    std::fs::write("./artifacts/_agt_test_long.md", "x".repeat(5000)).unwrap();
    let md = "## Goal\nSave to artifacts/_agt_test_long.md.\n";
    let answer = llm_guard_final_answer(md, "", "fallback");
    assert!(answer.contains("truncated"));
    assert!(answer.contains("artifacts/_agt_test_long.md"));
    assert!(answer.chars().count() < 5000);
}

#[test]
fn test_llm_guard_final_answer_fallback_chain() {
    let md = "# Plan\n\n## Goal\nDo things.\n";
    assert_eq!(
        llm_guard_final_answer(md, "last report", "fallback"),
        "last report"
    );
    assert_eq!(llm_guard_final_answer(md, "", "fallback"), "fallback");
    assert_eq!(llm_guard_final_answer(md, "   ", "fallback"), "fallback");
}

// ---------------------------------------------------------------------------
// Declared-output verification (job-end sweep + Goal gate).
// ---------------------------------------------------------------------------

#[test]
fn test_unfinished_outputs_lists_missing_task_outputs() {
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_ok.md"]);
    std::fs::write("./artifacts/_agt_test_ok.md", "ok\n").unwrap();
    let md = "# Handover Log\n\n- Task 2: - Status: done - Output: artifacts/_agt_test_ok.md, artifacts/_agt_test_missing.md\noutputs: artifacts/_agt_test_ok.md, artifacts/_agt_test_missing.md\n- Task 5: - Status: done - Output: artifacts/other.md\noutputs: artifacts/other.md\n";
    assert_eq!(
        llm_guard_unfinished_outputs(md),
        vec![
            (
                "Task 2".to_string(),
                "artifacts/_agt_test_missing.md".to_string()
            ),
            ("Task 5".to_string(), "artifacts/other.md".to_string()),
        ]
    );
}

#[test]
fn test_unfinished_outputs_excludes_planner_and_prose() {
    let md = "- Planner: - Status: done - Output: artifacts/plan.md\noutputs: artifacts/plan.md\n- Task 1: - Status: done - Output: none\nA task entry may be followed by an `outputs:` line listing files.\n";
    // Planner outputs and the template prose (not an `outputs:`-synced line
    // under a Task) are ignored; no declared paths remain.
    assert!(llm_guard_unfinished_outputs(md).is_empty());
}

#[test]
fn test_unfinished_outputs_dedups_normalized_paths() {
    let md = "- Task 1: - Status: done - Output: artifacts/a.md\noutputs: ./artifacts/a.md, artifacts/a.md\n";
    // `./`-prefixed and plain spellings of the same missing path collapse
    // into one entry (first task wins).
    assert_eq!(
        llm_guard_unfinished_outputs(md),
        vec![("Task 1".to_string(), "artifacts/a.md".to_string())]
    );
}

#[test]
fn test_unfinished_outputs_skips_unverifiable_paths() {
    let md = "- Task 3: - Status: done\noutputs: artifacts/*.md, https://example.com/x.md, artifacts/real.md\n";
    assert_eq!(
        llm_guard_unfinished_outputs(md),
        vec![("Task 3".to_string(), "artifacts/real.md".to_string())]
    );
}

#[test]
fn test_goal_outputs_missing_lists_only_missing_or_empty() {
    let _guard = ArtifactFilesGuard::new(&[
        "./artifacts/_agt_test_existing.md",
        "./artifacts/_agt_test_empty.md",
    ]);
    std::fs::write("./artifacts/_agt_test_existing.md", "x\n").unwrap();
    std::fs::write("./artifacts/_agt_test_empty.md", "").unwrap();
    let md = "## Goal\nWrite artifacts/_agt_test_existing.md, artifacts/_agt_test_missing.md and artifacts/_agt_test_empty.md.\n";
    assert_eq!(
        llm_guard_goal_outputs_missing(md),
        vec![
            "artifacts/_agt_test_missing.md".to_string(),
            "artifacts/_agt_test_empty.md".to_string(),
        ]
    );
}

#[test]
fn test_goal_outputs_missing_none_declared_is_empty() {
    let md = "## Goal\nDo things.\n";
    assert!(llm_guard_goal_outputs_missing(md).is_empty());
}

#[test]
fn test_final_answer_notes_missing_goal_artifact() {
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_note.md"]);
    let md = "## Goal\nSave to artifacts/_agt_test_note.md.\n";
    let answer = llm_guard_final_answer(md, "last report", "fallback");
    assert!(answer.starts_with("last report"));
    assert!(answer.contains("artifacts/_agt_test_note.md"));
    assert!(answer.contains("missing or empty"));
}

#[test]
fn test_final_answer_has_no_note_when_goal_artifact_exists() {
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_note_ok.md"]);
    std::fs::write("./artifacts/_agt_test_note_ok.md", "GOAL\n").unwrap();
    let md = "## Goal\nSave to artifacts/_agt_test_note_ok.md.\n";
    assert_eq!(llm_guard_final_answer(md, "", "fallback"), "GOAL\n");
}
