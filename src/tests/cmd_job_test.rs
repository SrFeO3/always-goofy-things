//! Tests for `parse_job_request` in `src/cmd.rs`: slash surface parsing
//! stays synchronous and unit-testable; execution lives in `main`.

use super::*;
use crate::todo_job::TodoMode;

#[test]
fn non_job_input_falls_through() {
    assert!(parse_job_request("hello world").is_none());
    assert!(parse_job_request("/kb list").is_none());
    assert!(parse_job_request("/kb add doc.md").is_none());
    assert!(parse_job_request("/model foo").is_none());
    assert!(parse_job_request("/kb").is_none());
}

#[test]
fn job_init_needs_path() {
    assert!(matches!(
        parse_job_request("/job init work.json"),
        Some(Ok(JobRequest::TodoInit { path })) if path == "work.json"
    ));
    assert!(matches!(parse_job_request("/job init"), Some(Err(_))));
}

#[test]
fn job_run_parses_all_flags() {
    match parse_job_request(
        "/job run work.json --mode replan --dry-run --max-retries 3 --note \"be quick\"",
    ) {
        Some(Ok(JobRequest::TodoRun {
            plan,
            mode,
            dry_run,
            status,
            max_retries,
            note,
        })) => {
            assert_eq!(plan, "work.json");
            assert_eq!(mode, TodoMode::Replan);
            assert!(dry_run);
            assert!(!status);
            assert_eq!(max_retries, 3);
            assert_eq!(note, "be quick");
        }
        other => panic!("unexpected {:?}", other),
    }
}

#[test]
fn job_run_rejects_bad_flags() {
    assert!(matches!(parse_job_request("/job run"), Some(Err(_))));
    assert!(matches!(
        parse_job_request("/job run w.json --mode wild"),
        Some(Err(_))
    ));
    assert!(matches!(
        parse_job_request("/job run w.json --bogus"),
        Some(Err(_))
    ));
    assert!(matches!(parse_job_request("/job bogus"), Some(Err(_))));
}

#[test]
fn job_clean_all_flag() {
    assert!(matches!(
        parse_job_request("/job clean --all"),
        Some(Ok(JobRequest::TodoClean { all: true }))
    ));
    assert!(matches!(
        parse_job_request("/job clean"),
        Some(Ok(JobRequest::TodoClean { all: false }))
    ));
}

#[test]
fn kb_extract_parses_source_and_flags() {
    match parse_job_request(
        "/kb extract rfc.txt --chunk-bytes 1000 --max-retries 2 --redo --handover auto",
    ) {
        Some(Ok(JobRequest::KbExtract {
            source,
            chunk_bytes,
            dry_run,
            status,
            max_retries,
            redo,
            handover_auto,
        })) => {
            assert_eq!(source, Some("rfc.txt".to_string()));
            assert_eq!(chunk_bytes, Some(1000));
            assert!(!dry_run);
            assert!(!status);
            assert_eq!(max_retries, 2);
            assert!(redo);
            assert!(handover_auto);
        }
        other => panic!("unexpected {:?}", other),
    }
    assert!(matches!(
        parse_job_request("/kb extract --dry-run --status"),
        Some(Ok(JobRequest::KbExtract {
            source: None,
            dry_run: true,
            status: true,
            ..
        }))
    ));
    assert!(matches!(parse_job_request("/kb extract a b"), Some(Err(_))));
    assert!(matches!(
        parse_job_request("/kb extract --handover wild"),
        Some(Err(_))
    ));
}

#[test]
fn kb_analyze_takes_quoted_goal_and_sources() {
    match parse_job_request("/kb analyze \"compare rfc a and b\" --sources a.txt b.txt --note n") {
        Some(Ok(JobRequest::KbAnalyze {
            goal,
            sources,
            max_retries,
            note,
        })) => {
            assert_eq!(goal, "compare rfc a and b");
            assert_eq!(sources, vec!["a.txt".to_string(), "b.txt".to_string()]);
            assert_eq!(max_retries, 1);
            assert_eq!(note, "n");
        }
        other => panic!("unexpected {:?}", other),
    }
    assert!(matches!(parse_job_request("/kb analyze"), Some(Err(_))));
}
