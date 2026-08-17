use super::*;
use std::sync::Mutex;

/// Serializes tests that mutate the real `./artifacts/handover.md`.
/// (append_handover is hardwired to the CWD-relative HANDOVER_MD_PATH, so
/// these tests run in the project directory instead of chdir-ing the whole
/// test process, which raced with every other CWD-relative test.)
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
fn test_one_line_report_truncates() {
    let long = "a".repeat(400);
    let report = one_line_report(&format!("line1\nline2 {}", long));
    assert!(report.ends_with("..."));
    assert_eq!(report.chars().count(), 303); // 300 chars + "..."
    assert!(!report.contains('\n'));
}

#[test]
fn test_one_line_report_short() {
    let report = one_line_report("Status: done\nOutput: a.md");
    assert_eq!(report, "Status: done Output: a.md");
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
