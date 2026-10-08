//! Tests for `src/todo_guard.rs` (LLM output guards for the todo modes).

use super::*;
use serde_json::json;

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

// ---------------------------------------------------------------------------
// Assistant-scoped report extraction + condense declaration preservation.
// ---------------------------------------------------------------------------

#[test]
fn test_last_assistant_report_skips_tool_and_user_messages() {
    let session = Session {
        id: "t".to_string(),
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
        id: "t".to_string(),
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
fn test_plan_write_next_task_denied_and_missing_content_deferred() {
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
    // str_replace_editor on todo.md is NOT denied at dispatch: its result is
    // validated inside the tool (llm_guard_plan_write_validate) instead.
    assert!(
        llm_guard_plan_file_write(
            "str_replace_editor",
            "./todo.md",
            &json!({ "old_string": "- [ ] c", "new_string": "- [x] c" }),
            &guard,
        )
        .is_none()
    );
    // Missing `content`: left to the tool's own error.
    assert!(llm_guard_plan_file_write("write_file", "./todo.md", &json!({}), &guard).is_none());
}

#[test]
fn test_plan_write_validate_str_replace_result_allowed() {
    // A partial edit that fits the executor's scope (own `[x]` + subtasks)
    // passes the same snapshot check as a write_file rewrite.
    let guard = PlanWriteGuard::capture(PLAN, 0);
    let own_flip = PLAN.replace("- [ ] a", "- [x] a");
    assert_eq!(
        llm_guard_plan_write_validate("./todo.md", &own_flip, &guard),
        None
    );
    let with_sub = PLAN.replace("- [ ] a", "- [ ] a\n- [ ] own-sub");
    assert_eq!(
        llm_guard_plan_write_validate("todo.md", &with_sub, &guard),
        None
    );
}

#[test]
fn test_plan_write_validate_str_replace_result_denied() {
    let guard = PlanWriteGuard::capture(PLAN, 0);
    // Flipping a task the executor was not assigned is rejected...
    let other_flip = PLAN.replace("- [ ] c", "- [x] c");
    let msg = llm_guard_plan_write_validate("./todo.md", &other_flip, &guard)
        .expect("other-task flip via str_replace must be denied");
    assert!(msg.contains("[TOOL_DENIED]"), "{}", msg);
    assert!(msg.contains("checkbox changed"), "{}", msg);
    // ...and so is any change outside the `## Tasks` section.
    let section = PLAN.replace("Do the thing.", "Do the other thing.");
    assert!(llm_guard_plan_write_validate("./todo.md", &section, &guard).is_some());
}

#[test]
fn test_plan_write_validate_ignores_non_plan_files() {
    // Only `./todo.md` is validated; other paths pass through untouched.
    let guard = PlanWriteGuard::capture(PLAN, 0);
    let violated = PLAN.replace("- [ ] c", "- [x] c");
    for path in ["notes.md", "work/todo.md", "artifacts/todo.md"] {
        assert_eq!(
            llm_guard_plan_write_validate(path, &violated, &guard),
            None,
            "{} must not be plan-guarded",
            path
        );
    }
}
