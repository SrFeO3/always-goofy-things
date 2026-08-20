//! Tests for `src/reasoning.rs`: tool-run regression guard.
//!
//! The per-call accounting/formatting logic lives in `llm_stats.rs` and is
//! tested in `llm_stats_test.rs`; these tests only guard that the tool path
//! fed by the reasoning loop keeps working.

// --------------------------------------------------------------------------
// Tool-run regression guard (spec step 0): tool definitions and representative
// executions must keep working; the accounting feature must not disturb them.
// --------------------------------------------------------------------------

#[tokio::test]
async fn test_tool_smoke_read_file_and_grep() {
    // Register the package root as the workspace root (OnceLock, first call
    // wins, matching the other tool tests) so path validation works.
    crate::tools::set_workspace_root(std::env::current_dir().unwrap_or_else(|_| ".".into()));
    // Tool paths are validated against the workspace root; create the probe
    // inside the project with a CWD-relative path and clean up on drop.
    let dirname = format!("._smtool_{}", std::process::id());
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = Cleanup(dirname.clone());
    std::fs::create_dir_all(&dirname).unwrap();
    let probe = format!("{}/probe.txt", dirname);
    std::fs::write(&probe, "unique_probe_token_42\n").unwrap();

    // get_tool_definitions exposes the standard toolset.
    let defs = crate::tools::get_tool_definitions(None, |_| true);
    assert!(!defs.is_empty());
    let names = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"grep_search"));

    // read_file reads a text file.
    let res = crate::tools::execute_tool(
        "read_file",
        &serde_json::json!({ "path": probe }),
        None,
        |_| true,
    )
    .await
    .unwrap();
    assert!(res.to_string().contains("unique_probe_token_42"));

    // grep_search finds the token in the temp dir.
    let res = crate::tools::execute_tool(
        "grep_search",
        &serde_json::json!({ "query": "unique_probe_token_42", "path": dirname }),
        None,
        |_| true,
    )
    .await
    .unwrap();
    assert!(res.to_string().contains("unique_probe_token_42"));
}

#[tokio::test]
async fn test_tool_smoke_disabled_tool_refused() {
    // Defense in depth: disabled tools must be refused even if called.
    let res = crate::tools::execute_tool(
        "read_file",
        &serde_json::json!({ "path": "._smtool_disabled_probe.txt" }),
        None,
        |_| false,
    )
    .await;
    assert!(res.is_err());
}
