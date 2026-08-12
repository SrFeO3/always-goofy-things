use super::*;

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
