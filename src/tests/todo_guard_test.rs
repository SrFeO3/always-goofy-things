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
fn test_extract_output_paths_trailing_punct_and_links() {
    assert_eq!(
        extract_output_paths("- Output: artifacts/a.md., `artifacts/b.md。`"),
        vec!["artifacts/a.md", "artifacts/b.md"]
    );
    assert_eq!(
        extract_output_paths("- Output: [artifacts/c.md](https://example.com/x), [artifacts/d.md]"),
        vec!["artifacts/c.md", "artifacts/d.md"]
    );
    assert!(extract_output_paths("- Output: none.").is_empty());
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
// Completion report (B2): verified deliverables + harness-composed report.
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
fn test_verified_outputs_goal_first_then_task_outputs() {
    let _guard = ArtifactFilesGuard::new(&[
        "./artifacts/_agt_test_goal.md",
        "./artifacts/_agt_test_task1.md",
    ]);
    std::fs::write("./artifacts/_agt_test_goal.md", "g\n").unwrap();
    std::fs::write("./artifacts/_agt_test_task1.md", "t\n").unwrap();
    let todo = "## Goal\nSave to artifacts/_agt_test_goal.md.\n";
    let handover = "- Task 1: - Status: done\noutputs: artifacts/_agt_test_task1.md, artifacts/_agt_test_missing.md\n- Planner: - Status: done\noutputs: artifacts/plan.md\n";
    // Goal paths come first; missing and Planner outputs are excluded.
    assert_eq!(
        llm_guard_verified_outputs(todo, handover),
        vec![
            "artifacts/_agt_test_goal.md".to_string(),
            "artifacts/_agt_test_task1.md".to_string(),
        ]
    );
}

#[test]
fn test_verified_outputs_normalizes_and_skips_unverifiable() {
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_norm.md"]);
    std::fs::write("./artifacts/_agt_test_norm.md", "x\n").unwrap();
    let todo = "## Goal\nDo things.\n";
    let handover = "- Task 2: - Status: done\noutputs: ./artifacts/_agt_test_norm.md, artifacts/_agt_test_norm.md, artifacts/*.md, https://example.com/r.md\n";
    // `./`-spellings dedup, globs/URLs are unverifiable and skipped.
    assert_eq!(
        llm_guard_verified_outputs(todo, handover),
        vec!["artifacts/_agt_test_norm.md".to_string()]
    );
}

#[test]
fn test_completion_report_zero_outputs() {
    assert_eq!(
        llm_guard_completion_report(&[], 13, 0, 0, false),
        "OK: all 13 tasks completed; deliverables(0; no tasks declared Output paths)"
    );
    assert_eq!(
        llm_guard_completion_report(&[], 13, 0, 0, true),
        "OK: all 13 tasks already completed; deliverables(0; no tasks declared Output paths)"
    );
}

#[test]
fn test_completion_report_zero_annotates_declarations() {
    assert_eq!(
        llm_guard_completion_report(&[], 13, 2, 0, false),
        "OK: all 13 tasks completed; deliverables(0; 2 tasks declared Output paths)"
    );
    assert_eq!(
        llm_guard_completion_report(&[], 13, 1, 0, false),
        "OK: all 13 tasks completed; deliverables(0; 1 task declared Output paths)"
    );
}

#[test]
fn test_completion_report_lists_up_to_cap() {
    let paths: Vec<String> = (1..=7).map(|i| format!("artifacts/f{}.md", i)).collect();
    let report = llm_guard_completion_report(&paths, 7, 7, 0, false);
    assert!(report.starts_with("OK: all 7 tasks completed; deliverables(7): artifacts/f1.md"));
    assert!(report.contains("artifacts/f5.md"));
    assert!(!report.contains("artifacts/f6.md"));
    assert!(report.ends_with("(+2 more)"));
}

#[test]
fn test_completion_report_small_list() {
    let paths = vec!["artifacts/a.md".to_string(), "artifacts/b.md".to_string()];
    assert_eq!(
        llm_guard_completion_report(&paths, 2, 2, 0, false),
        "OK: all 2 tasks completed; deliverables(2): artifacts/a.md, artifacts/b.md"
    );
}

#[test]
fn test_completion_report_already_done_lists_deliverables() {
    let paths = vec!["artifacts/report.md".to_string()];
    assert_eq!(
        llm_guard_completion_report(&paths, 13, 1, 0, true),
        "OK: all 13 tasks already completed; deliverables(1): artifacts/report.md"
    );
}

#[test]
fn test_completion_report_unverifiable_suffix() {
    let paths = vec!["artifacts/r.md".to_string()];
    assert_eq!(
        llm_guard_completion_report(&paths, 7, 1, 2, false),
        "OK: all 7 tasks completed; deliverables(1): artifacts/r.md (+2 unverifiable skipped)"
    );
    assert_eq!(
        llm_guard_completion_report(&[], 7, 0, 3, true),
        "OK: all 7 tasks already completed; deliverables(0; no tasks declared Output paths) (+3 unverifiable skipped)"
    );
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
fn test_unfinished_outputs_skips_home_absolute_escape_paths() {
    let md = "- Task 1: - Status: done\noutputs: ~/notes.md, /tmp/out.md, ../out.md, C:\\out.md\n- Task 2: - Status: done\noutputs: artifacts/a..b.md\n";
    // Home-relative, absolute, parent-escaping, and Windows spellings are
    // unverifiable; a literal `..` inside a file name is not.
    assert_eq!(
        llm_guard_unfinished_outputs(md),
        vec![("Task 2".to_string(), "artifacts/a..b.md".to_string())]
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
fn test_goal_outputs_missing_binary_file_counts_as_existing() {
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_bin.md"]);
    // Non-UTF-8 bytes: unreadable as text, but present and non-empty.
    std::fs::write(
        "./artifacts/_agt_test_bin.md",
        [0x89, 0x50, 0x4e, 0x47, 0x00],
    )
    .unwrap();
    let md = "## Goal\nWrite artifacts/_agt_test_bin.md.\n";
    assert!(llm_guard_goal_outputs_missing(md).is_empty());
}

#[test]
fn test_goal_outputs_missing_none_declared_is_empty() {
    let md = "## Goal\nDo things.\n";
    assert!(llm_guard_goal_outputs_missing(md).is_empty());
}

#[test]
fn test_goal_outputs_missing_rejects_directories() {
    std::fs::create_dir_all("./artifacts/_agt_test_dir.md").unwrap();
    let md = "## Goal\nWrite artifacts/_agt_test_dir.md.\n";
    assert_eq!(
        llm_guard_goal_outputs_missing(md),
        vec!["artifacts/_agt_test_dir.md".to_string()]
    );
    let _ = std::fs::remove_dir_all("./artifacts/_agt_test_dir.md");
}

#[test]
fn test_tasks_declaring_outputs_count() {
    let md = "- Task 1: - Status: done\noutputs: artifacts/a.md\n- Task 2: - Status: done\noutputs: artifacts/b.md, artifacts/c.md\n- Task 3: - Status: done\n- Task 3 [unverified]: not completed\n- Planner: - Status: done\noutputs: artifacts/plan.md\nA task entry may be followed by an `outputs:` line listing files.\n";
    // Task 1 + Task 2 declare paths; Task 3 and the [unverified] note do
    // not; Planner and template prose are excluded.
    assert_eq!(llm_guard_tasks_declaring_outputs(md), 2);
}

#[test]
fn test_tasks_declaring_outputs_zero() {
    let md = "- Task 1: - Status: done - Output: none\n- Task 2: - Status: done\noutputs:\n";
    assert_eq!(llm_guard_tasks_declaring_outputs(md), 0);
}

#[test]
fn test_unverifiable_declared_counts_goal_and_task_outputs() {
    let todo = "## Goal\nWrite artifacts/*.md.\n";
    let handover = "- Task 1: - Status: done\noutputs: https://example.com/x.md, artifacts/real.md, artifacts/?x.md\n";
    // Goal glob (1) + task glob/URL paths (2); the plain path does not count.
    assert_eq!(llm_guard_unverifiable_declared(todo, handover), 3);
}

// ---------------------------------------------------------------------------
// Assistant-scoped report extraction + condense declaration preservation.
// ---------------------------------------------------------------------------

#[test]
fn test_last_assistant_report_skips_tool_and_user_messages() {
    let session = Session {
        label: "t".to_string(),
        turn: 1,
        messages: vec![
            crate::model::Message {
                role: "system".into(),
                content: "sys".into(),
                ..Default::default()
            },
            crate::model::Message {
                role: "user".into(),
                content: "task".into(),
                ..Default::default()
            },
            crate::model::Message {
                role: "assistant".into(),
                content: "Status: done".into(),
                ..Default::default()
            },
            crate::model::Message {
                role: "tool".into(),
                content: "ok".into(),
                ..Default::default()
            },
        ],
    };
    assert_eq!(last_assistant_report(&session), Some("Status: done"));
}

#[test]
fn test_last_assistant_report_none_without_assistant() {
    let session = Session {
        label: "t".to_string(),
        turn: 1,
        messages: vec![crate::model::Message {
            role: "system".into(),
            content: "sys".into(),
            ..Default::default()
        }],
    };
    assert_eq!(last_assistant_report(&session), None);
}

#[test]
fn test_merge_condensed_report_reappends_dropped_outputs() {
    let original = "- Status: done\n- Output: artifacts/a.md, artifacts/b.md\n- Next: none";
    let condensed = "- Status: done\n- Output: artifacts/a.md\n- Next: none";
    let merged = merge_condensed_report(Some(original), condensed);
    assert_eq!(
        extract_output_paths(&merged),
        vec!["artifacts/a.md", "artifacts/b.md"]
    );
    assert!(merged.ends_with("\n- Output: artifacts/b.md"));
}

#[test]
fn test_merge_condensed_report_noop_when_nothing_dropped() {
    let report = "- Status: done\n- Output: artifacts/a.md\n- Next: none";
    assert_eq!(merge_condensed_report(Some(report), report), report);
}

#[test]
fn test_merge_condensed_report_normalizes_dot_slash() {
    // `./`-spellings match plain ones: the rewrite kept the path, so no
    // duplicate line is appended.
    let original = "- Output: ./artifacts/a.md";
    let condensed = "- Output: artifacts/a.md";
    assert_eq!(merge_condensed_report(Some(original), condensed), condensed);
}

#[test]
fn test_merge_condensed_report_noop_without_original() {
    let condensed = "- Status: done\n- Output: artifacts/a.md";
    assert_eq!(merge_condensed_report(None, condensed), condensed);
}
