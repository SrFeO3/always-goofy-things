//! Tests for `src/todo_guard.rs` (LLM output guards for the todo modes).

use super::*;
use serde_json::json;

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
fn test_extract_output_paths_strict_list_syntax() {
    // The machine format is a comma-separated `Output:` list. Japanese list
    // separators are NOT split: `・` between paths is taken as written (one
    // phantom declaration), so reports must use the documented format.
    assert_eq!(
        extract_output_paths("- Output: artifacts/a.md・b.md"),
        vec!["artifacts/a.md・b.md"]
    );
    assert_eq!(
        extract_output_paths("- Output: artifacts/a.md、artifacts/b.md"),
        vec!["artifacts/a.md、artifacts/b.md"]
    );
    // Only `artifacts/` spellings are returned; other declarations are
    // prose noise and never tracked.
    assert_eq!(
        extract_output_paths("- Output: artifacts/a.md, b.md, `c.md`"),
        vec!["artifacts/a.md"]
    );
}

#[test]
fn test_extract_output_paths_strict_prefix_and_prose() {
    // A fullwidth colon after Output is not the documented prefix, and
    // prose suffixes/annotations are taken as written (no Japanese fuzz);
    // non-artifacts wrappers are dropped by the artifacts-only rule.
    assert!(extract_output_paths("- Output：artifacts/final-report.md").is_empty());
    assert_eq!(
        extract_output_paths("- Output: artifacts/final-report.md（既存確認）"),
        vec!["artifacts/final-report.md（既存確認）"]
    );
    assert!(extract_output_paths("- Output: 「artifacts/a.md」").is_empty());
}

#[test]
fn test_extract_output_paths_cuts_ascii_annotations() {
    // LLM-style annotations after a path resolve to the bare artifact path
    // (the Task 3/4 pattern behind the false-positive job-end warning).
    assert_eq!(
        extract_output_paths("- Output: artifacts/chunk-review-11-20.md; todo.md (Task 3 [x])"),
        vec!["artifacts/chunk-review-11-20.md"]
    );
    assert_eq!(
        extract_output_paths(
            "- Output: artifacts/chunk-review-21-25.md (new, ~62 facts); todo task4 [x]"
        ),
        vec!["artifacts/chunk-review-21-25.md"]
    );
    assert_eq!(
        extract_output_paths(
            "- Output: artifacts/overall_review.md (new), artifacts/checklist.md (refined), \
             artifacts/chunk-review-11-20.md (2 corrected)"
        ),
        vec![
            "artifacts/overall_review.md",
            "artifacts/checklist.md",
            "artifacts/chunk-review-11-20.md",
        ]
    );
    // Semicolon-separated bare paths are two declarations, not one.
    assert_eq!(
        extract_output_paths("- Output: artifacts/a.md; artifacts/b.md"),
        vec!["artifacts/a.md", "artifacts/b.md"]
    );
}

#[test]
fn test_extract_output_paths_bold_marker_and_star_junk() {
    // Markdown-bold `**Output:**` and stray asterisks are decorations, not
    // path text; a mid-token `*` (glob-style) stays untouched and is gated
    // by the artifacts-only rule.
    assert_eq!(
        extract_output_paths("- **Output:** artifacts/a.md, artifacts/b.md"),
        vec!["artifacts/a.md", "artifacts/b.md"]
    );
    assert_eq!(
        extract_output_paths("**Output:** artifacts/a.md **"),
        vec!["artifacts/a.md"]
    );
    assert_eq!(
        extract_output_paths("- Output: artifacts/a*b.md"),
        vec!["artifacts/a*b.md"]
    );
}

#[test]
fn test_extract_output_paths_artifacts_only_and_normalized() {
    // `./` is normalized to the canonical artifacts/ form, subdir paths
    // are not artifacts declarations, and `Outputs:` is not `Output:`.
    assert_eq!(
        extract_output_paths("- Output: ./artifacts/a.md, sub/x.md"),
        vec!["artifacts/a.md"]
    );
    assert!(extract_output_paths("- Outputs: artifacts/a.md").is_empty());
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
// Completion report (B2): verified deliverables + application-composed report.
// ---------------------------------------------------------------------------

#[test]
fn test_extract_goal_artifact_paths_removed() {
    // The Goal-prose extraction is gone: `## Deliverables` is the only
    // machine-verified source, so Goal prose is never parsed (phantom-path
    // bugs are impossible).
    let md =
        "# Plan\n\n## Goal\nSave the report to artifacts/final-report.md.\n\n## Tasks\n- [ ] a\n";
    assert!(
        extract_deliverables_paths(md).is_empty(),
        "Goal prose must not be parsed"
    );
}

// ---------------------------------------------------------------------------
// ## Deliverables section: the machine contract for Goal deliverables.
// ---------------------------------------------------------------------------

#[test]
fn test_deliverables_primary_section() {
    // Bullets: plain and checkbox-decorated are both read. Prose lines,
    // Task bullets below the section, and the Goal/Tasks prose are ignored.
    let md = "# Plan\n\n## Goal\n最終成果物は artifacts/final-report.md（Goal 散文は読まない）\n\n## Tasks\n- [ ] write artifacts/from-task.md\n\n## Deliverables\n- artifacts/final-report.md\n- artifacts/schema-notes.md\n- artifacts/dimension-notes.md\n- [ ] artifacts/checkbox-tolerated.md\n- 注意: 散文の行\n- artifacts/source-inventory.md 時系列の整理\n";
    assert_eq!(
        extract_deliverables_paths(md),
        vec![
            "artifacts/final-report.md",
            "artifacts/schema-notes.md",
            "artifacts/dimension-notes.md",
            "artifacts/checkbox-tolerated.md",
            "artifacts/source-inventory.md",
        ]
    );
}

#[test]
fn test_deliverables_loose_bullet_spellings() {
    // LLM sloppiness is tolerated: tabs, runs of spaces, alternative
    // bullet markers (`*`, `+`, `・`, `•`), and Tasks-style checkboxes all
    // resolve to the same path.
    let md = "## Deliverables\n\
- artifacts/a.md\n\
-\tartifacts/b.md\n\
-  artifacts/c.md\n\
* artifacts/d.md\n\
+ artifacts/e.md\n\
・artifacts/f.md\n\
• artifacts/g.md\n\
- [ ] artifacts/h.md\n\
- [x] artifacts/i.md\n\
- [X] artifacts/j.md\n\
\t- artifacts/k.md\n";
    assert_eq!(
        extract_deliverables_paths(md),
        vec![
            "artifacts/a.md",
            "artifacts/b.md",
            "artifacts/c.md",
            "artifacts/d.md",
            "artifacts/e.md",
            "artifacts/f.md",
            "artifacts/g.md",
            "artifacts/h.md",
            "artifacts/i.md",
            "artifacts/j.md",
            "artifacts/k.md",
        ]
    );
}

#[test]
fn test_deliverables_loose_heading() {
    // Extra whitespace and trailing words on the `## Deliverables` heading
    // are tolerated; unrelated headings still end the section.
    let md = "##  Deliverables (成果物)\n- artifacts/a.md\n\n## Appendices\n- artifacts/b.md\n";
    assert_eq!(extract_deliverables_paths(md), vec!["artifacts/a.md"]);
}

#[test]
fn test_extract_output_paths_loose_prefix_spellings() {
    // `Output:` prefix sloppiness is tolerated: an alternative bullet
    // marker, tabs/extra spaces, a glued dash, and a space before the
    // colon all still parse (fullwidth colon stays rejected).
    assert_eq!(
        extract_output_paths("- Output : artifacts/a.md"),
        vec!["artifacts/a.md"]
    );
    assert_eq!(
        extract_output_paths("-Output: artifacts/a.md"),
        vec!["artifacts/a.md"]
    );
    assert_eq!(
        extract_output_paths("* Output:\tartifacts/a.md"),
        vec!["artifacts/a.md"]
    );
    assert_eq!(
        extract_output_paths("・Output: artifacts/a.md"),
        vec!["artifacts/a.md"]
    );
    assert_eq!(
        extract_output_paths("+ Output : artifacts/a.md, artifacts/b.md"),
        vec!["artifacts/a.md", "artifacts/b.md"]
    );
    assert!(extract_output_paths("* Output：artifacts/a.md").is_empty());
}

#[test]
fn test_deliverables_strict_bullets_only() {
    // Non-conforming lines are ignored as written: multi-path bullets and
    // bare names are not parsed (no Japanese fuzz), escape attempts are
    // rejected, and only file-shaped artifacts/ paths are collected.
    let md =
        "## Deliverables\n- artifacts/a.md・b.md\n- b.md\n- artifacts/../evil.md\n- artifacts\n";
    assert_eq!(extract_deliverables_paths(md), vec!["artifacts/a.md・b.md"]);
}

#[test]
fn test_deliverables_absent_means_no_declarations() {
    // A plan without a Deliverables section declares no deliverables; the
    // Goal prose is not read (no gate).
    let md =
        "# Plan\n\n## Goal\nSave the report to artifacts/final-report.md.\n\n## Tasks\n- [ ] a\n";
    assert!(extract_deliverables_paths(md).is_empty());
}

#[test]
fn test_deliverables_empty_section_no_fallback() {
    let md = "# Plan\n\n## Goal\nSave artifacts/ignored.md.\n\n## Tasks\n- [ ] a\n\n## Deliverables\n\n## Appendices\n";
    assert!(extract_deliverables_paths(md).is_empty());
}

#[test]
fn test_guard_state_file_write_mode2_only() {
    // Mode 2: writes to guard-managed state files are denied; the denial
    // message is actionable.
    for (name, path) in [
        ("write_file", "artifacts/handover.md"),
        ("str_replace_editor", "artifacts/handover.md"),
        ("write_file", "artifacts/calc_ledger.jsonl"),
        ("write_file", "./artifacts/handover.md"),
    ] {
        let msg = llm_guard_state_file_write(name, path, 2).expect("mode 2 must deny");
        assert!(msg.starts_with("[TOOL_DENIED]"), "{}", msg);
    }
    // Mode 2: reads, deliverables, root files, same name elsewhere - all fine.
    for (name, path) in [
        ("read_file", "artifacts/handover.md"),
        ("grep_search", "artifacts/handover.md"),
        ("write_file", "artifacts/final-report.md"),
        ("write_file", "todo.md"),
        ("write_file", "handover.md"),
        ("write_file", "sub/artifacts/handover.md"),
        ("write_file", "artifacts/handover.md.bak"),
    ] {
        assert!(
            llm_guard_state_file_write(name, path, 2).is_none(),
            "{} {} must not be denied",
            name,
            path
        );
    }
    // Modes 0/1: no denial for the same canonical path.
    for mode in [0u8, 1u8] {
        assert!(llm_guard_state_file_write("write_file", "artifacts/handover.md", mode).is_none());
    }
}

#[test]
fn test_goal_outputs_missing_ignores_goal_prose() {
    // The muse-glimmer phantom line would previously fail the gate; Goal
    // prose is now never parsed, so a plan without a Deliverables section
    // has no machine-verified deliverables (no gate, no phantom).
    let _guard = ArtifactFilesGuard::new(&[
        "./artifacts/_agt_test_schema.md",
        "./artifacts/_agt_test_dim.md",
    ]);
    std::fs::write("./artifacts/_agt_test_schema.md", "s\n").unwrap();
    std::fs::write("./artifacts/_agt_test_dim.md", "d\n").unwrap();
    let md = "## Goal\nクエリ対象カラムは必ず artifacts/_agt_test_schema.md・_agt_test_dim.md に記載済みのものだけを使う。\n";
    assert!(llm_guard_goal_outputs_missing(md).is_empty());
}

#[test]
fn test_verified_outputs_separates_goal_and_task_lists() {
    let _guard = ArtifactFilesGuard::new(&[
        "./artifacts/_agt_test_goal.md",
        "./artifacts/_agt_test_task1.md",
    ]);
    std::fs::write("./artifacts/_agt_test_goal.md", "g\n").unwrap();
    std::fs::write("./artifacts/_agt_test_task1.md", "t\n").unwrap();
    let todo = "## Goal\nSave to artifacts/_agt_test_goal.md.\n\n## Deliverables\n- artifacts/_agt_test_goal.md\n";
    let handover = "- Task 1: - Status: done\noutputs: artifacts/_agt_test_task1.md, artifacts/_agt_test_missing.md\n- Planner: - Status: done\noutputs: artifacts/plan.md\n";
    // Goal list = Deliverables section only; task list = verified task
    // outputs (missing and Planner outputs excluded). Never mixed.
    assert_eq!(
        llm_guard_verified_outputs(todo, handover),
        (
            vec!["artifacts/_agt_test_goal.md".to_string()],
            vec!["artifacts/_agt_test_task1.md".to_string()],
        )
    );
}

#[test]
fn test_verified_outputs_task_list_excludes_goal_paths() {
    // A path both in the Deliverables section and in a task's outputs is
    // reported once, under the gated goal list.
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_shared.md"]);
    std::fs::write("./artifacts/_agt_test_shared.md", "s\n").unwrap();
    let todo = "## Deliverables\n- artifacts/_agt_test_shared.md\n";
    let handover = "- Task 1: - Status: done\noutputs: artifacts/_agt_test_shared.md\n";
    assert_eq!(
        llm_guard_verified_outputs(todo, handover),
        (vec!["artifacts/_agt_test_shared.md".to_string()], vec![],)
    );
}

#[test]
fn test_has_deliverables_section_tolerates_heading() {
    assert!(has_deliverables_section(
        "## Goal\nx\n\n##  Deliverables (成果物)\n- artifacts/a.md\n"
    ));
    assert!(has_deliverables_section(
        "## Deliverables\n- artifacts/a.md\n"
    ));
    assert!(!has_deliverables_section("## Goal\nDeliverables?\n"));
    assert!(!has_deliverables_section("## Tasks\n- [ ] a\n"));
}

#[test]
fn test_handover_entry_filters_non_artifact_outputs() {
    // `todo.md (updated)`-style declarations are prose noise; the machine
    // `outputs:` line only records artifacts paths.
    let entry = build_handover_entry(
        "- Task 2",
        "- Status: done\n- Output: artifacts/a.md, todo.md (updated)\n- Findings: ok",
    );
    assert!(entry.contains("Output: artifacts/a.md, todo.md (updated)"));
    assert!(entry.ends_with("outputs: artifacts/a.md"));
}

#[test]
fn test_sweep_ignores_non_artifact_declarations() {
    // Annotated declarations (`todo.md (updated)`) are prose noise and are
    // never tracked; only artifacts/ paths are reported.
    let md = "- Task 1: - Status: done\noutputs: artifacts/real_missing.md, todo.md (updated)\n";
    assert_eq!(
        llm_guard_unfinished_outputs(md),
        vec![(
            "Task 1".to_string(),
            "artifacts/real_missing.md".to_string()
        )]
    );
}

#[test]
fn test_tasks_declaring_outputs_counts_artifacts_only() {
    let md = "- Task 1: - Status: done\noutputs: todo.md (updated)\n- Task 2: - Status: done\noutputs: artifacts/a.md\n";
    assert_eq!(llm_guard_tasks_declaring_outputs(md), 1);
}

#[test]
fn test_verified_outputs_normalizes_and_skips_unverifiable() {
    let _guard = ArtifactFilesGuard::new(&["./artifacts/_agt_test_norm.md"]);
    std::fs::write("./artifacts/_agt_test_norm.md", "x\n").unwrap();
    let todo = "## Goal\nDo things.\n";
    let handover = "- Task 2: - Status: done\noutputs: ./artifacts/_agt_test_norm.md, artifacts/_agt_test_norm.md, artifacts/*.md, https://example.com/r.md\n";
    // `./`-spellings dedup, globs/URLs are unverifiable and skipped; no
    // Deliverables section, so everything lands in the task list.
    assert_eq!(
        llm_guard_verified_outputs(todo, handover),
        (vec![], vec!["artifacts/_agt_test_norm.md".to_string()])
    );
}

#[test]
fn test_completion_report_zero_everything() {
    // No Deliverables section and no task declarations: both zeros are
    // annotated so nothing is silently empty.
    assert_eq!(
        llm_guard_completion_report(&[], &[], false, 13, 0, 0, false),
        "OK: all 13 tasks completed; deliverables(0; no ## Deliverables section); task outputs(0; no tasks declared Output paths)"
    );
    assert_eq!(
        llm_guard_completion_report(&[], &[], false, 13, 0, 0, true),
        "OK: all 13 tasks already completed; deliverables(0; no ## Deliverables section); task outputs(0; no tasks declared Output paths)"
    );
}

#[test]
fn test_completion_report_goal_and_task_segments() {
    // Both segments listed side by side, never mixed.
    let goal = vec!["artifacts/final.md".to_string()];
    let tasks = vec!["artifacts/notes.md".to_string()];
    assert_eq!(
        llm_guard_completion_report(&goal, &tasks, true, 3, 2, 0, false),
        "OK: all 3 tasks completed; deliverables(1): artifacts/final.md; task outputs(1): artifacts/notes.md"
    );
}

#[test]
fn test_completion_report_no_section_with_task_declarations() {
    // The shape of a plan without ## Deliverables (e.g. the Counter Test
    // example): the goal segment says so explicitly, and the declared
    // output is listed under task outputs, not deliverables.
    let tasks = vec!["artifacts/count.txt".to_string()];
    assert_eq!(
        llm_guard_completion_report(&[], &tasks, false, 3, 3, 0, false),
        "OK: all 3 tasks completed; deliverables(0; no ## Deliverables section); task outputs(1): artifacts/count.txt"
    );
}

#[test]
fn test_completion_report_zero_task_outputs_annotate_declarations() {
    // Tasks declared outputs but none verified beyond the goal list.
    assert_eq!(
        llm_guard_completion_report(&[], &[], true, 13, 2, 0, false),
        "OK: all 13 tasks completed; deliverables(0); task outputs(0; 2 tasks declared Output paths)"
    );
    assert_eq!(
        llm_guard_completion_report(&[], &[], true, 13, 1, 0, false),
        "OK: all 13 tasks completed; deliverables(0); task outputs(0; 1 task declared Output paths)"
    );
}

#[test]
fn test_completion_report_lists_up_to_cap() {
    let goal: Vec<String> = (1..=7).map(|i| format!("artifacts/f{}.md", i)).collect();
    let report = llm_guard_completion_report(&goal, &[], true, 7, 0, 0, false);
    assert!(report.starts_with("OK: all 7 tasks completed; deliverables(7): artifacts/f1.md"));
    assert!(report.contains("artifacts/f5.md"));
    assert!(!report.contains("artifacts/f6.md"));
    assert!(report.contains("(+2 more)"));
}

#[test]
fn test_completion_report_small_list() {
    let goal = vec!["artifacts/a.md".to_string(), "artifacts/b.md".to_string()];
    assert_eq!(
        llm_guard_completion_report(&goal, &[], true, 2, 0, 0, false),
        "OK: all 2 tasks completed; deliverables(2): artifacts/a.md, artifacts/b.md; task outputs(0; no tasks declared Output paths)"
    );
}

#[test]
fn test_completion_report_already_done_lists_goal_deliverables() {
    let goal = vec!["artifacts/report.md".to_string()];
    assert_eq!(
        llm_guard_completion_report(&goal, &[], true, 13, 0, 0, true),
        "OK: all 13 tasks already completed; deliverables(1): artifacts/report.md; task outputs(0; no tasks declared Output paths)"
    );
}

#[test]
fn test_completion_report_unverifiable_suffix() {
    let goal = vec!["artifacts/r.md".to_string()];
    assert_eq!(
        llm_guard_completion_report(&goal, &[], true, 7, 0, 2, false),
        "OK: all 7 tasks completed; deliverables(1): artifacts/r.md; task outputs(0; no tasks declared Output paths) (+2 unverifiable skipped)"
    );
    assert_eq!(
        llm_guard_completion_report(&[], &[], false, 7, 0, 3, true),
        "OK: all 7 tasks already completed; deliverables(0; no ## Deliverables section); task outputs(0; no tasks declared Output paths) (+3 unverifiable skipped)"
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
fn test_unfinished_outputs_annotation_tokens_resolve() {
    // Regression for the false-positive job-end warning: annotated
    // `outputs:` tokens must resolve, so existing artifacts are not
    // reported missing.
    let _guard = ArtifactFilesGuard::new(&[
        "./artifacts/_agt_test_r11_20.md",
        "./artifacts/_agt_test_r21_25.md",
    ]);
    std::fs::write("./artifacts/_agt_test_r11_20.md", "x\n").unwrap();
    std::fs::write("./artifacts/_agt_test_r21_25.md", "x\n").unwrap();
    let md = "- Task 3: - Status: done - Output: artifacts/_agt_test_r11_20.md; todo.md (Task 3 [x])\n\
             outputs: artifacts/_agt_test_r11_20.md; todo.md (Task 3 [x]), \
             artifacts/_agt_test_r11_20.md (created); todo.md (Task 3 marked [x])\n\
             - Task 4: - Status: done - Output: artifacts/_agt_test_r21_25.md (new, ~62 facts); todo task4 [x]\n\
             outputs: artifacts/_agt_test_r21_25.md (new\n";
    assert_eq!(
        llm_guard_unfinished_outputs(md),
        Vec::<(String, String)>::new()
    );
    // A genuinely missing artifact among the same annotations is still
    // reported.
    let md = md.replace(
        "artifacts/_agt_test_r11_20.md; todo.md",
        "artifacts/_agt_test_r11_20_missing.md; todo.md",
    );
    assert_eq!(
        llm_guard_unfinished_outputs(&md),
        vec![(
            "Task 3".to_string(),
            "artifacts/_agt_test_r11_20_missing.md".to_string()
        )]
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
    let md = "## Deliverables\n- artifacts/_agt_test_existing.md\n- artifacts/_agt_test_missing.md\n- artifacts/_agt_test_empty.md\n";
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
    let md = "## Deliverables\n- artifacts/_agt_test_bin.md\n";
    assert!(llm_guard_goal_outputs_missing(md).is_empty());
}

#[test]
fn test_goal_outputs_missing_none_declared_is_empty() {
    // No Deliverables section -> no machine-verified deliverables.
    let md = "## Goal\nDo things.\n";
    assert!(llm_guard_goal_outputs_missing(md).is_empty());
}

#[test]
fn test_goal_outputs_missing_rejects_directories() {
    std::fs::create_dir_all("./artifacts/_agt_test_dir.md").unwrap();
    let md = "## Deliverables\n- artifacts/_agt_test_dir.md\n";
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
fn test_unverifiable_declared_counts_deliverables_and_task_outputs() {
    let todo = "## Deliverables\n- artifacts/*.md\n";
    let handover = "- Task 1: - Status: done\noutputs: artifacts/*.md, artifacts/real.md\n";
    // Deliverables glob (1) + task glob (1); the plain path and non-artifacts
    // URLs (never returned by extraction) do not count.
    assert_eq!(llm_guard_unverifiable_declared(todo, handover), 2);
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

// ---------------------------------------------------------------------------
// Plan-write guard: `./todo.md` rewrites vs. the session-start snapshot
// (own `[x]` + added subtasks allowed; existing tasks frozen).
// ---------------------------------------------------------------------------

const PLAN: &str = "# Plan\n\n## Goal\nDo the thing.\n\n## Tasks\n- [ ] a\n- [x] b\n- [ ] c\n\n## Deliverables\n- artifacts/out.md\n";

/// Rewrite `./todo.md` (via write_file) with the given content; returns the
/// denial message when the guard rejects it.
fn plan_write(content: &str, assigned: usize) -> Option<String> {
    let guard = PlanWriteGuard::capture(PLAN, assigned);
    llm_guard_plan_file_write(
        "write_file",
        "./todo.md",
        &json!({ "path": "todo.md", "content": content }),
        &guard,
    )
}

#[test]
fn test_plan_write_own_flip_and_noop_allowed() {
    // Exact copy: allowed.
    assert_eq!(plan_write(PLAN, 0), None);
    // Assigned task `a` ([ ]->[x]): allowed.
    let done = PLAN.replace("- [ ] a", "- [x] a");
    assert_eq!(plan_write(&done, 0), None);
}

#[test]
fn test_plan_write_added_subtasks_any_position_allowed() {
    let after_own = PLAN.replace("- [ ] a\n- [x] b", "- [ ] a\n- [ ] own-sub\n- [x] b");
    assert_eq!(plan_write(&after_own, 0), None);
    let before_own = PLAN.replace("- [ ] a", "- [ ] prep\n- [ ] a");
    assert_eq!(plan_write(&before_own, 0), None);
    let at_end = PLAN.replace(
        "- [ ] c\n\n## Deliverables",
        "- [ ] c\n- [ ] tail-sub\n\n## Deliverables",
    );
    assert_eq!(plan_write(&at_end, 0), None);
}

#[test]
fn test_plan_write_checked_subtask_allowed() {
    // Added subtasks may be `[x]` in the same write.
    let with_done_sub = PLAN.replace("- [ ] c", "- [ ] c\n- [x] done-sub");
    assert_eq!(plan_write(&with_done_sub, 0), None);
}

#[test]
fn test_plan_write_added_subtask_flip_in_later_write() {
    // A subtask added in the first write can be flipped to `[x]` in the
    // second (both validated against the same session snapshot).
    let w1 = PLAN.replace("- [ ] c", "- [ ] c\n- [ ] s1");
    let guard = PlanWriteGuard::capture(PLAN, 0);
    assert!(
        llm_guard_plan_file_write("write_file", "./todo.md", &json!({ "content": w1 }), &guard)
            .is_none()
    );
    let w2 = w1.replace("- [ ] s1", "- [x] s1");
    assert!(
        llm_guard_plan_file_write("write_file", "./todo.md", &json!({ "content": w2 }), &guard)
            .is_none()
    );
}

#[test]
fn test_plan_write_other_task_flip_denied() {
    let msg = plan_write(&PLAN.replace("- [ ] c", "- [x] c"), 0).expect("must deny");
    assert!(msg.contains("[TOOL_DENIED]"), "{}", msg);
    assert!(msg.contains("checkbox changed"), "{}", msg);
}

#[test]
fn test_plan_write_rename_removal_reorder_denied() {
    let rename = plan_write(&PLAN.replace("- [ ] c", "- [ ] c2"), 0).expect("must deny");
    assert!(rename.contains("removed or renamed"), "{}", rename);
    let removal = plan_write(&PLAN.replace("- [ ] c\n", ""), 0).expect("must deny");
    assert!(removal.contains("removed or renamed"), "{}", removal);
    let reorder = "# Plan\n\n## Goal\nDo the thing.\n\n## Tasks\n- [ ] c\n- [x] b\n- [ ] a\n\n## Deliverables\n- artifacts/out.md\n";
    let msg = plan_write(reorder, 0).expect("must deny");
    assert!(msg.contains("order"), "{}", msg);
}

#[test]
fn test_plan_write_unflip_denied() {
    // Unchecking a session-start `[x]` task is never allowed.
    let msg = plan_write(&PLAN.replace("- [x] b", "- [ ] b"), 0).expect("must deny");
    assert!(msg.contains("[TOOL_DENIED]"), "{}", msg);
}

#[test]
fn test_plan_write_mixed_write_denied_atomically() {
    // A mixed write (own flip + other flip) is rejected whole.
    let mixed = PLAN
        .replace("- [ ] a", "- [x] a")
        .replace("- [ ] c", "- [x] c");
    assert!(plan_write(&mixed, 0).is_some());
}

#[test]
fn test_plan_write_section_changes_denied() {
    let goal = PLAN.replace("Do the thing.", "Do the other thing.");
    assert!(plan_write(&goal, 0).is_some());
    let deliverables = PLAN.replace("- artifacts/out.md", "- artifacts/out2.md");
    assert!(plan_write(&deliverables, 0).is_some());
    let prose = PLAN.replace("- [ ] c", "- [ ] c\n- note prose");
    assert!(plan_write(&prose, 0).is_some());
    // Removing the `## Tasks` heading is denied too.
    let no_heading = PLAN.replace("## Tasks\n", "");
    assert!(plan_write(&no_heading, 0).is_some());
}

#[test]
fn test_plan_write_empty_desc_subtask_denied() {
    let empty = PLAN.replace("- [ ] c", "- [ ] c\n- [ ] ");
    let msg = plan_write(&empty, 0).expect("must deny");
    assert!(msg.contains("empty description"), "{}", msg);
}

#[test]
fn test_plan_write_whitespace_and_blank_lines_tolerated() {
    // Line whitespace and blank lines are normalized by the parser.
    let cosmetically_different = "# Plan\n\n## Goal\nDo the thing.\n\n## Tasks\n\n- [ ] a  \n- [x] b\n  - [ ] c\n\n## Deliverables\n- artifacts/out.md\n";
    assert_eq!(plan_write(cosmetically_different, 0), None);
}

#[test]
fn test_plan_write_assigned_out_of_range_denies_flip() {
    // Capture with a stale index: no flip may be allowed anywhere.
    assert!(plan_write(&PLAN.replace("- [ ] a", "- [x] a"), 9).is_some());
}

#[test]
fn test_plan_write_duplicate_descriptions_stay_frozen() {
    let dup_plan = "## Tasks\n- [ ] a\n- [ ] a\n";
    // Checking a second pre-existing bullet is not the assigned flip.
    let guard = PlanWriteGuard::capture(dup_plan, 0);
    let both_checked = "## Tasks\n- [x] a\n- [x] a\n";
    let msg = llm_guard_plan_file_write(
        "write_file",
        "./todo.md",
        &json!({ "content": both_checked }),
        &guard,
    )
    .expect("must deny");
    assert!(msg.contains("[TOOL_DENIED]"), "{}", msg);
    // Assigned flip + unchanged duplicate: allowed.
    let one_flip = "## Tasks\n- [x] a\n- [ ] a\n";
    assert!(
        llm_guard_plan_file_write(
            "write_file",
            "./todo.md",
            &json!({ "content": one_flip }),
            &guard
        )
        .is_none()
    );
}

#[test]
fn test_plan_write_path_normalization() {
    let guard = PlanWriteGuard::capture(PLAN, 0);
    let args = json!({ "content": PLAN });
    // `./`-prefix and `dir/..` spellings name the same plan file.
    for path in ["./todo.md", "todo.md", "artifacts/../todo.md"] {
        assert!(
            llm_guard_plan_file_write("write_file", path, &args, &guard).is_none(),
            "{path} must pass"
        );
    }
    let bad = json!({ "content": PLAN.replace("- [ ] c", "- [x] c") });
    for path in ["todo.md", "artifacts/../todo.md"] {
        assert!(
            llm_guard_plan_file_write("write_file", path, &bad, &guard).is_some(),
            "{path} must be guarded"
        );
    }
    // Other files are never guarded by the plan guard.
    for path in [
        "work/todo.md",
        "artifacts/todo.md",
        "sub/next-task.md",
        "notes.md",
    ] {
        assert!(
            llm_guard_plan_file_write("write_file", path, &bad, &guard).is_none(),
            "{path} must not be guarded"
        );
    }
}

#[test]
fn test_plan_write_next_task_and_str_replace_denied() {
    let guard = PlanWriteGuard::capture(PLAN, 0);
    // next-task.md: any executor write is denied (planner-owned).
    for args in [
        json!({ "content": "brief" }),
        json!({ "old_string": "a", "new_string": "b" }),
    ] {
        let msg = llm_guard_plan_file_write("write_file", "./next-task.md", &args, &guard)
            .or_else(|| {
                llm_guard_plan_file_write("str_replace_editor", "next-task.md", &args, &guard)
            })
            .expect("next-task.md write must be denied");
        assert!(msg.contains("[TOOL_DENIED]"), "{}", msg);
    }
    // str_replace_editor on todo.md: full rewrites only.
    let msg = llm_guard_plan_file_write(
        "str_replace_editor",
        "./todo.md",
        &json!({ "old_string": "[ ] c", "new_string": "[x] c" }),
        &guard,
    )
    .expect("str_replace on todo.md must be denied");
    assert!(msg.contains("write_file"), "{}", msg);
    // Missing `content`: left to the tool's own error.
    assert!(llm_guard_plan_file_write("write_file", "./todo.md", &json!({}), &guard).is_none());
}
