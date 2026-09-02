use super::*;
use std::sync::Mutex;

/// Serializes tests that mutate CWD-relative files (`./artifacts/handover.md`
/// and `./todo.md`). The mutating helpers are hardwired to CWD paths, so
/// these tests run in the project directory instead of chdir-ing the whole
/// test process, which raced with every other CWD-relative test.
static HANDOVER_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Backup of `./artifacts/handover.md`, restored on drop (panic-safe).
struct HandoverBackup(Option<String>);

impl HandoverBackup {
    fn capture() -> Self {
        let backup = std::fs::read_to_string(HANDOVER_MD_PATH).ok();
        let _ = std::fs::remove_file(HANDOVER_MD_PATH);
        std::fs::create_dir_all("./artifacts").unwrap();
        Self(backup)
    }
}

impl Drop for HandoverBackup {
    fn drop(&mut self) {
        match &self.0 {
            Some(content) => {
                let _ = std::fs::create_dir_all("./artifacts");
                let _ = std::fs::write(HANDOVER_MD_PATH, content);
            }
            None => {
                let _ = std::fs::remove_file(HANDOVER_MD_PATH);
            }
        }
    }
}

/// Backup of `./todo.md`, restored on drop (panic-safe); a missing file is
/// removed again on drop.
struct TodoFileBackup(Option<String>);

impl TodoFileBackup {
    fn capture() -> Self {
        Self(std::fs::read_to_string(TODO_MD_PATH).ok())
    }
}

impl Drop for TodoFileBackup {
    fn drop(&mut self) {
        match &self.0 {
            Some(content) => {
                let _ = std::fs::write(TODO_MD_PATH, content);
            }
            None => {
                let _ = std::fs::remove_file(TODO_MD_PATH);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// count_task_items (completion report task total).
// ---------------------------------------------------------------------------

#[test]
fn test_count_task_items_mixed() {
    let md = "# Plan\n\n## Goal\nDo things.\n\n## Tasks\n- [x] a\n- [ ] b\n- [x] c\n- [ ] d\n";
    assert_eq!(count_task_items(md), 4);
}

#[test]
fn test_count_task_items_ignores_prose_and_other_sections() {
    let md =
        "## Tasks\n- [x] a\n- [ ] b\n- some prose - [ ] not-a-task\n## Status\n- [ ] after-tasks\n";
    // Prose lines and items in later sections do not count.
    assert_eq!(count_task_items(md), 2);
}

#[test]
fn test_count_task_items_legacy_or_empty() {
    // Legacy plans without a Tasks section: no tasks, no panic.
    assert_eq!(count_task_items("## Status\nStatus: Completed\n"), 0);
    assert_eq!(count_task_items("## Tasks\n"), 0);
    assert_eq!(count_task_items(""), 0);
}

// ---------------------------------------------------------------------------
// mark_task_done (Mode 1 [x] marking).
// ---------------------------------------------------------------------------

#[test]
fn test_mark_task_done_marks_nth_unchecked() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    std::fs::write(
        TODO_MD_PATH,
        "# Plan\n\n## Tasks\n- [ ] a\n- [x] b\n- [ ] c\n",
    )
    .unwrap();
    mark_task_done(1).unwrap(); // second unchecked task (`c`)
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(content, "# Plan\n\n## Tasks\n- [ ] a\n- [x] b\n- [x] c\n");
}

#[test]
fn test_mark_task_done_index_out_of_range_is_noop() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    std::fs::write(TODO_MD_PATH, "## Tasks\n- [x] a\n").unwrap();
    mark_task_done(0).unwrap(); // no unchecked task at index 0
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(content, "## Tasks\n- [x] a\n");
}

#[test]
fn test_mark_task_done_does_not_touch_later_sections() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    std::fs::write(
        TODO_MD_PATH,
        "## Tasks\n- [ ] a\n- [ ] b\n\n## Status\nStatus: In Progress\n",
    )
    .unwrap();
    mark_task_done(0).unwrap();
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(
        content,
        "## Tasks\n- [x] a\n- [ ] b\n\n## Status\nStatus: In Progress\n"
    );
}

#[test]
fn test_mark_task_done_missing_file_errors() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    let _ = std::fs::remove_file(TODO_MD_PATH);
    assert!(mark_task_done(0).is_err());
}

// ---------------------------------------------------------------------------
// unmark_task_by_index (Mode 2 false-[x] rejection).
// ---------------------------------------------------------------------------

#[test]
fn test_unmark_task_by_index_flips_checked_to_unchecked() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    std::fs::write(
        TODO_MD_PATH,
        "# Plan\n\n## Tasks\n- [ ] a\n- [x] b\n- [ ] c\n",
    )
    .unwrap();
    // Absolute index 1 is the `- [x] b` bullet (index 0 is the `- [ ] a`).
    unmark_task_by_index(1).unwrap();
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(content, "# Plan\n\n## Tasks\n- [ ] a\n- [ ] b\n- [ ] c\n");
}

#[test]
fn test_unmark_task_by_index_absolute_index_counts_all_tasks() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    std::fs::write(TODO_MD_PATH, "## Tasks\n- [x] a\n- [x] b\n- [x] c\n").unwrap();
    // Every bullet is `- [x]`; the absolute index 1 is the second bullet (b),
    // not a `- [ ]`-relative index (mark_task_done(1) would be a no-op here).
    unmark_task_by_index(1).unwrap();
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(content, "## Tasks\n- [x] a\n- [ ] b\n- [x] c\n");
}

#[test]
fn test_unmark_task_by_index_already_unchecked_is_noop() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    let original = "## Tasks\n- [x] a\n- [ ] b\n";
    std::fs::write(TODO_MD_PATH, original).unwrap();
    // Index 1 is already `- [ ]`; unmarking it is a no-op (no write).
    unmark_task_by_index(1).unwrap();
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(content, original);
}

#[test]
fn test_unmark_task_by_index_out_of_range_is_noop() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    let original = "## Tasks\n- [x] a\n";
    std::fs::write(TODO_MD_PATH, original).unwrap();
    unmark_task_by_index(5).unwrap(); // no task at absolute index 5
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(content, original);
}

#[test]
fn test_unmark_task_by_index_no_tasks_section_is_noop() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    let original = "## Status\nStatus: In Progress\n";
    std::fs::write(TODO_MD_PATH, original).unwrap();
    unmark_task_by_index(0).unwrap(); // no `## Tasks` section
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(content, original);
}

#[test]
fn test_unmark_task_by_index_does_not_touch_later_sections() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    std::fs::write(
        TODO_MD_PATH,
        "## Tasks\n- [x] a\n- [x] b\n\n## Status\nStatus: In Progress\n",
    )
    .unwrap();
    unmark_task_by_index(0).unwrap();
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(
        content,
        "## Tasks\n- [ ] a\n- [x] b\n\n## Status\nStatus: In Progress\n"
    );
}

#[test]
fn test_unmark_task_by_index_missing_file_errors() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    let _ = std::fs::remove_file(TODO_MD_PATH);
    assert!(unmark_task_by_index(0).is_err());
}

#[test]
fn test_check_completion_plain_line() {
    // The prompt asks for `Status: Completed`; the list dash is optional.
    let md = "## Status\nStatus: Completed\n";
    assert!(check_completion(md));
}

#[test]
fn test_check_completion_list_line() {
    let md = "## Status\n- Status: Completed\n";
    assert!(check_completion(md));
}

#[test]
fn test_check_completion_in_progress() {
    let md = "## Status\nStatus: In Progress\n";
    assert!(!check_completion(md));
}

#[test]
fn test_check_completion_missing_section() {
    assert!(!check_completion("## Tasks\n- [ ] a\n"));
}

#[test]
fn test_check_completion_unchecked_tasks_block() {
    // Completion requires no unchecked tasks left.
    let md = "## Status\nStatus: Completed\n\n## Tasks\n- [ ] pending\n";
    assert!(!check_completion(md));
}

#[test]
fn test_check_completion_status_after_tasks() {
    // A full plan: status at the end, all tasks done.
    let md = "# Plan\n\n## Goal\nDo things.\n\n## Tasks\n- [x] a\n- [x] b\n\n## Status\nStatus: Completed\n";
    assert!(check_completion(md));
}

#[test]
fn test_check_completion_strict_format_no_status() {
    // Strict format (title / Goal / Tasks only, no Status section):
    // completion is simply "no unchecked tasks left".
    let md = "# Plan\n\n## Goal\nDo things.\n\n## Tasks\n- [x] a\n- [x] b\n";
    assert!(check_completion(md));
}

#[test]
fn test_check_completion_strict_format_pending() {
    let md = "# Plan\n\n## Goal\nDo things.\n\n## Tasks\n- [x] a\n- [ ] b\n";
    assert!(!check_completion(md));
}

#[test]
fn test_check_completion_legacy_status_in_progress_blocks() {
    // Legacy plan with a Status section: the section must declare completion.
    let md = "# Plan\n\n## Tasks\n- [x] a\n\n## Status\nStatus: In Progress\n";
    assert!(!check_completion(md));
}

#[test]
fn test_append_handover_creates_and_appends() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = HandoverBackup::capture();

    let r1 = append_handover("- Task 1: hello");
    let r2 = append_handover("- Task 2: world");

    assert!(r1.is_ok() && r2.is_ok());
    let content = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap();
    assert_eq!(content, "- Task 1: hello\n- Task 2: world\n");
}

#[test]
fn test_append_handover_dedup_same_task_marker() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = HandoverBackup::capture();

    let r1 = append_handover("- Task 3: first report text");
    // Same task marker (e.g. the executor already wrote its own report):
    // the app-side append must be skipped even though the text differs.
    let r2 =
        append_handover("- Task 3: Status: done - Output: count.txt - Findings: different wording");
    // Different marker: must still be appended.
    let r3 = append_handover("- Task 4: another report");
    // Non-task entries (e.g. the seed template) are never deduped.
    let r4 = append_handover("# Handover Log");

    assert!(r1.is_ok() && r2.is_ok() && r3.is_ok() && r4.is_ok());
    let content = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap();
    assert_eq!(
        content,
        "- Task 3: first report text\n- Task 4: another report\n# Handover Log\n"
    );
}

/// A resumed run appends its task report with the todo.md number (here 6);
/// it must not be deduped away by an earlier `- Task 1:` entry.
#[test]
fn test_append_handover_resume_numbering_does_not_collide() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = HandoverBackup::capture();

    let r1 = append_handover("- Task 1: earlier run report");
    let r2 = append_handover("- Task 6: resumed run report");
    assert!(r1.is_ok() && r2.is_ok());
    let content = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap();
    assert_eq!(
        content,
        "- Task 1: earlier run report\n- Task 6: resumed run report\n"
    );
}

#[test]
fn test_seed_handover_creates_once() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = HandoverBackup::capture();

    let r1 = seed_handover();
    let seeded = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap();
    let r2 = seed_handover(); // second call must be a no-op
    let after = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap();

    assert!(r1.is_ok() && r2.is_ok());
    assert!(seeded.starts_with("# Handover Log"));
    assert_eq!(seeded, after);
}

// ---------------------------------------------------------------------------
// Plan-write guard through execute_tool (str_replace_editor path).
// ---------------------------------------------------------------------------

/// End to end: `str_replace_editor` edits of `./todo.md` are guarded like
/// write_file rewrites (the computed result is checked before the write).
///
/// Lock held across await is safe: the sync tests block on it from plain
/// threads (no same-runtime waiters).
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn test_execute_tool_mode2_str_replace_plan_write_guard() {
    let _guard = HANDOVER_TEST_LOCK.lock().unwrap();
    let _backup = TodoFileBackup::capture();
    crate::tools::set_workspace_root(
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    let plan = "# Plan\n\n## Tasks\n- [ ] a\n- [ ] b\n";
    std::fs::write(TODO_MD_PATH, plan).unwrap();
    let session_plan = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    let guard = crate::todo_guard::PlanWriteGuard::capture(&session_plan, 0);

    // Allowed: the executor marks its own task with a targeted edit.
    let ok = crate::tools::execute_tool(
        "str_replace_editor",
        &serde_json::json!({
            "path": "./todo.md",
            "old_string": "- [ ] a",
            "new_string": "- [x] a"
        }),
        None,
        None,
        Some(&guard),
        2,
        |_| true,
    )
    .await;
    assert!(
        ok.is_ok(),
        "own-task flip via str_replace_editor must be allowed: {:?}",
        ok.err()
    );
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(content, "# Plan\n\n## Tasks\n- [x] a\n- [ ] b\n");

    // Denied: flipping a task the executor was not assigned is rejected
    // before the write lands (the file stays unchanged).
    let denied = crate::tools::execute_tool(
        "str_replace_editor",
        &serde_json::json!({
            "path": "./todo.md",
            "old_string": "- [ ] b",
            "new_string": "- [x] b"
        }),
        None,
        None,
        Some(&guard),
        2,
        |_| true,
    )
    .await;
    let err = denied.unwrap_err().to_string();
    assert!(err.contains("[TOOL_DENIED]"), "{}", err);
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(
        content, "# Plan\n\n## Tasks\n- [x] a\n- [ ] b\n",
        "a denied edit must not land"
    );

    // Fuzzy path: whitespace-sloppy old_string, same guard on the result.
    // Allowed: adding a subtask below the pending task.
    let ok = crate::tools::execute_tool(
        "str_replace_editor",
        &serde_json::json!({
            "path": "./todo.md",
            "old_string": "- [ ]  b",
            "new_string": "- [ ] b\n- [ ] fuzzy-sub"
        }),
        None,
        None,
        Some(&guard),
        2,
        |_| true,
    )
    .await;
    assert!(
        ok.is_ok(),
        "fuzzy subtask add via str_replace_editor must be allowed: {:?}",
        ok.err()
    );
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(
        content,
        "# Plan\n\n## Tasks\n- [x] a\n- [ ] b\n- [ ] fuzzy-sub\n"
    );

    // Denied: fuzzy-matching the other task's flip is rejected before the
    // write (the file stays unchanged).
    let denied = crate::tools::execute_tool(
        "str_replace_editor",
        &serde_json::json!({
            "path": "./todo.md",
            "old_string": "- [ ]  b",
            "new_string": "- [x] b"
        }),
        None,
        None,
        Some(&guard),
        2,
        |_| true,
    )
    .await;
    let err = denied.unwrap_err().to_string();
    assert!(err.contains("[TOOL_DENIED]"), "{}", err);
    let content = std::fs::read_to_string(TODO_MD_PATH).unwrap();
    assert_eq!(
        content, "# Plan\n\n## Tasks\n- [x] a\n- [ ] b\n- [ ] fuzzy-sub\n",
        "a denied fuzzy edit must not land"
    );
}
