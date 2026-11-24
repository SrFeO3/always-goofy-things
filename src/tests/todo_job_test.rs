//! Tests for `src/todo_job.rs`: plan validation, replan protocol,
//! state roundtrip, finalize tiers, init/clean. No LLM calls.

use super::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn plan_json() -> serde_json::Value {
    serde_json::json!({
        "goal": "write a report",
        "tasks": [
            {"id": "t1", "description": "take notes", "verify": ["nonempty artifacts/notes.md"]},
            {"id": "t2", "description": "draft report", "verify": ["nonempty artifacts/draft.md"]}
        ],
        "deliverables": ["artifacts/report.md"]
    })
}

fn write_plan(dir: &Path, name: &str, v: &serde_json::Value) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_string_pretty(v).unwrap()).unwrap();
    p
}

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "todo-job-test-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst),
        tag
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn done_state(handover: &str) -> TaskState {
    TaskState {
        status: TaskStatus::Done,
        attempts: 1,
        handover: Some(handover.to_string()),
        error: None,
    }
}

#[test]
fn load_plan_accepts_valid_and_rejects_bad() {
    let dir = scratch("plan");
    let good = write_plan(&dir, "todo.json", &plan_json());
    let plan = load_plan(&good).unwrap();
    assert_eq!(plan.goal, "write a report");
    assert_eq!(plan.tasks.len(), 2);
    assert_eq!(plan.deliverables, vec!["artifacts/report.md"]);

    let mut dup = plan_json();
    dup["tasks"][1]["id"] = serde_json::json!("t1");
    assert!(load_plan(&write_plan(&dir, "dup.json", &dup)).is_err());

    let mut bad_check = plan_json();
    bad_check["tasks"][0]["verify"] = serde_json::json!(["bogus here"]);
    assert!(load_plan(&write_plan(&dir, "bad.json", &bad_check)).is_err());

    let mut no_goal = plan_json();
    no_goal.as_object_mut().unwrap().remove("goal");
    assert!(load_plan(&write_plan(&dir, "nogoal.json", &no_goal)).is_err());

    let mut empty = plan_json();
    empty["tasks"] = serde_json::json!([]);
    assert!(load_plan(&write_plan(&dir, "empty.json", &empty)).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn live_plan_path_is_sibling_with_stem() {
    let dir = scratch("livepath");
    let a = write_plan(&dir, "todo.json", &plan_json());
    assert_eq!(live_plan_path(&a), dir.join("todo.live.json"));
    let b = dir.join("sub").join("build.json");
    assert_eq!(live_plan_path(&b), dir.join("sub").join("build.live.json"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn live_plan_roundtrips_atomically() {
    let dir = scratch("live");
    let plan_path = write_plan(&dir, "todo.json", &plan_json());
    let plan = load_plan(&plan_path).unwrap();
    let live_path = live_plan_path(&plan_path);
    let mut states = HashMap::new();
    states.insert("t1".to_string(), done_state("- Status: done"));
    let briefs = HashMap::from([("t2".to_string(), "read t1 output".to_string())]);
    save_live(&live_path, &plan_path, &plan, &states, &briefs, 3, 42).unwrap();
    let (plan2, states2, briefs2, rev, seed_hash) = load_live_only(&live_path).unwrap().unwrap();
    assert_eq!(rev, 3);
    assert_eq!(seed_hash, 42);
    assert_eq!(states2["t1"].attempts, 1);
    assert!(matches!(states2["t1"].status, TaskStatus::Done));
    assert_eq!(briefs2["t2"], "read t1 output");
    assert_eq!(plan2.tasks.len(), 2);
    // The live file is a visible sibling of the input.
    assert_eq!(live_path.parent(), plan_path.parent());
    // Missing live reads as None.
    assert!(load_live_only(&dir.join("nope.live.json")).unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn resume_reads_live_plan_with_replan_added_task() {
    // P0 regression: a replan-added task that is already done must survive a
    // resume (the old mode rejected it as `removed` and stalled).
    let dir = scratch("resume");
    let seed = write_plan(&dir, "todo.json", &plan_json()); // t1, t2
    let seed_plan = load_plan(&seed).unwrap();
    let live_path = live_plan_path(&seed);
    let mut plan = seed_plan.clone();
    plan.tasks.push(PlanTask {
        id: "t3".to_string(),
        description: "added by replan".to_string(),
        verify_raw: Vec::new(),
        checks: Vec::new(),
    });
    let mut states = HashMap::new();
    states.insert("t1".to_string(), done_state("r"));
    states.insert("t3".to_string(), done_state("r"));
    save_live(
        &live_path,
        &seed,
        &plan,
        &states,
        &HashMap::new(),
        5,
        plan_hash(&seed_plan),
    )
    .unwrap();
    // Resume works even if the seed file is gone.
    std::fs::remove_file(&seed).ok();
    let (plan2, states2, _b, rev, _h) = load_live_only(&live_path).unwrap().unwrap();
    assert_eq!(rev, 5);
    assert_eq!(count_done(&plan2, &states2), 2);
    assert!(plan2.tasks.iter().any(|t| t.id == "t3"));
    assert_eq!(next_undone(&plan2, &states2).unwrap().id, "t2");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn next_undone_skips_done_in_order() {
    let dir = scratch("undone");
    let plan = load_plan(&write_plan(&dir, "todo.json", &plan_json())).unwrap();
    let mut states = HashMap::new();
    assert_eq!(next_undone(&plan, &states).unwrap().id, "t1");
    states.insert("t1".to_string(), done_state("r"));
    assert_eq!(next_undone(&plan, &states).unwrap().id, "t2");
    states.insert("t2".to_string(), done_state("r"));
    assert!(next_undone(&plan, &states).is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn plan_block_takes_last_fence() {
    assert!(extract_plan_block("no fence here").is_none());
    let two = "notes ```todo-plan {\"tasks\": []} ``` mid ```todo-plan {\"tasks\": [1]} ``` end";
    assert_eq!(extract_plan_block(two), Some(" {\"tasks\": [1]} "));
}

#[test]
fn replan_validates_and_applies() {
    let dir = scratch("replan");
    let mut plan = load_plan(&write_plan(&dir, "todo.json", &plan_json())).unwrap();
    let mut states = HashMap::new();
    states.insert("t1".to_string(), done_state("r"));
    // Valid: keep done, split pending.
    let ok: ProposedPlan = serde_json::from_str(
        r#"{"tasks": [
            {"id": "t1", "description": "take notes", "verify": ["nonempty artifacts/notes.md"]},
            {"id": "t2a", "description": "draft part one", "verify": ["nonempty artifacts/p1.md"]},
            {"id": "t2b", "description": "draft part two"}
        ]}"#,
    )
    .unwrap();
    let note = apply_replan(&mut plan, &states, ok).unwrap();
    assert!(note.contains("3 tasks"));
    assert_eq!(plan.tasks[1].id, "t2a");
    assert!(plan.tasks[2].checks.is_empty());
    // Reject: drop a done task.
    let drop_done: ProposedPlan =
        serde_json::from_str(r#"{"tasks": [{"id": "t2a", "description": "draft part one"}]}"#)
            .unwrap();
    assert!(apply_replan(&mut plan, &states, drop_done).is_err());
    // Reject: rename a done task.
    let rename_done: ProposedPlan = serde_json::from_str(
        r#"{"tasks": [
            {"id": "t1", "description": "CHANGED", "verify": []},
            {"id": "t2a", "description": "draft part one"}]}"#,
    )
    .unwrap();
    assert!(apply_replan(&mut plan, &states, rename_done).is_err());
    // Reject: bad check string.
    let bad_check: ProposedPlan = serde_json::from_str(
        r#"{"tasks": [
            {"id": "t1", "description": "take notes", "verify": ["nonempty artifacts/notes.md"]},
            {"id": "t9", "description": "new", "verify": ["frobnicate x"]}]}"#,
    )
    .unwrap();
    assert!(apply_replan(&mut plan, &states, bad_check).is_err());
    // Deliverables replaced when present.
    let with_del: ProposedPlan = serde_json::from_str(
        r#"{"tasks": [
            {"id": "t1", "description": "take notes", "verify": ["nonempty artifacts/notes.md"]}],
         "deliverables": ["artifacts/other.md"]}"#,
    )
    .unwrap();
    apply_replan(&mut plan, &states, with_del).unwrap();
    assert_eq!(plan.deliverables, vec!["artifacts/other.md"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn finalize_separates_fatal_and_warn() {
    let dir = scratch("finalize");
    let report = dir.join("artifacts").join("report.md");
    std::fs::create_dir_all(report.parent().unwrap()).unwrap();
    std::fs::write(&report, "").unwrap();
    let plan = Plan {
        goal: "g".to_string(),
        tasks: vec![PlanTask {
            id: "t1".to_string(),
            description: "d".to_string(),
            verify_raw: Vec::new(),
            checks: Vec::new(),
        }],
        deliverables: vec![report.to_string_lossy().into_owned()],
    };
    let mut states = HashMap::new();
    // Relative artifacts path: parsed, then reported missing (warn tier).
    let ghost_rel = format!("artifacts/todo-job-test-ghost-{}.md", std::process::id());
    states.insert(
        "t1".to_string(),
        done_state(&format!("- Status: done\n- Output: {}", ghost_rel)),
    );
    // Empty deliverable is fatal even with outputs present.
    let out = finalize_plan(&plan, &states);
    assert_eq!(out.status, JobStatus::Failed);
    assert!(out.summary.contains("missing deliverables"));
    // Non-empty deliverable completes; ghost output warns.
    std::fs::write(&report, "body").unwrap();
    let out = finalize_plan(&plan, &states);
    assert_eq!(out.status, JobStatus::Completed);
    assert!(out.summary.contains("Completed: deliverables(1)"));
    assert!(out.summary.contains("todo-job-test-ghost"));
    // Quiet when nothing declared missing.
    states.insert(
        "t1".to_string(),
        done_state("- Status: done\n- Output: none"),
    );
    let out = finalize_plan(&plan, &states);
    assert!(!out.summary.contains("Warnings"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn instruction_carries_checks_note_and_brief() {
    let task = PlanTask {
        id: "t2".to_string(),
        description: "draft it".to_string(),
        verify_raw: vec!["nonempty artifacts/d.md".to_string()],
        checks: Vec::new(),
    };
    let s = build_task_instruction("make report", "be quick", Some("prev done"), &task);
    assert!(s.contains("draft it"));
    assert!(s.contains("make report"));
    assert!(s.contains("nonempty artifacts/d.md"));
    assert!(s.contains("be quick"));
    assert!(s.contains("Brief: prev done"));
    let bare = build_task_instruction("g", "", None, &task);
    assert!(!bare.contains("Brief:"));
}

#[test]
fn session_labels_stay_filesafe() {
    assert_eq!(task_session_label("s", 0, "t1"), "s_task0-t1");
    assert_eq!(task_session_label("s", 3, "a/b c:d"), "s_task3-a_b_c_d");
}

#[test]
fn init_refuses_overwrite_and_template_parses() {
    let dir = scratch("init");
    let p = dir.join("sub").join("todo.json");
    init_plan(&p).unwrap();
    assert!(init_plan(&p).is_err());
    load_plan(&p).unwrap(); // template itself is a valid plan
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn clean_removes_orphans_or_all() {
    let dir = scratch("clean");
    let state_dir = dir.join(".todo");
    std::fs::create_dir_all(&state_dir).unwrap();
    let live_plan = write_plan(&dir, "todo.json", &plan_json());
    let plan = load_plan(&live_plan).unwrap();
    let empty = HashMap::new();
    save_live(
        &state_dir.join("live.live.json"),
        &live_plan,
        &plan,
        &empty,
        &HashMap::new(),
        0,
        0,
    )
    .unwrap();
    save_live(
        &state_dir.join("orphan.live.json"),
        Path::new("/nonexistent/todo.json"),
        &plan,
        &empty,
        &HashMap::new(),
        0,
        0,
    )
    .unwrap();
    let deleted = clean_states(&dir, false).unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(state_dir.join("live.live.json").exists());
    let deleted = clean_states(&dir, true).unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(std::fs::read_dir(&state_dir).unwrap().next().is_none());
    // Missing dir reads as nothing to do.
    assert!(clean_states(&dir.join("nodir"), false).unwrap().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn status_reads_live_counts() {
    let dir = scratch("status");
    let p = write_plan(&dir, "todo.json", &plan_json());
    let plan = load_plan(&p).unwrap();
    let live_path = live_plan_path(&p);
    let mut states = HashMap::new();
    states.insert("t1".to_string(), done_state("r"));
    save_live(&live_path, &p, &plan, &states, &HashMap::new(), 1, 0).unwrap();
    let out = todo_status_in(&p).unwrap();
    assert!(out.contains("[job status] 1/2 tasks done"));
    assert!(out.contains("- [x] t1"));
    assert!(out.contains("- [ ] t2 (not started)"));
    // No live file: fall back to the seed, all pending.
    std::fs::remove_file(&live_path).ok();
    let out = todo_status_in(&p).unwrap();
    assert!(out.contains("[job status] 0/2 tasks done"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dry_run_lists_tasks_checks_deliverables() {
    let dir = scratch("dryrun");
    let p = write_plan(&dir, "todo.json", &plan_json());
    let out = dry_run_todo(&p).unwrap();
    assert!(out.contains("[job dry-run] 2 tasks, 1 deliverables"));
    assert!(out.contains("t1: take notes"));
    assert!(out.contains("check: nonempty artifacts/notes.md"));
    assert!(out.contains("deliverable: artifacts/report.md"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn generated_plan_writes_through_validation() {
    let dir = scratch("genplan");
    let path = dir.join(".todo").join("gen.json");
    let plan = write_generated_plan(
        "investigate",
        r#"{"tasks": [{"id": "q1", "description": "look", "verify": ["nonempty artifacts/q.md"]}]}"#,
        &path,
        "artifacts/default.md",
    )
    .unwrap();
    assert_eq!(plan.goal, "investigate");
    assert_eq!(plan.deliverables, vec!["artifacts/default.md"]);
    // Reload validates the written file.
    let reloaded = load_plan(&path).unwrap();
    assert_eq!(reloaded.tasks.len(), 1);
    // Bad fence JSON fails before touching the disk.
    assert!(
        write_generated_plan("g", "not json", &dir.join("bad.json"), "artifacts/d.md").is_err()
    );
    std::fs::remove_dir_all(&dir).ok();
}
