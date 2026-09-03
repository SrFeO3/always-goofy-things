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
        None,
        None,
        0,
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
        None,
        None,
        0,
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
        None,
        None,
        0,
        |_| false,
    )
    .await;
    assert!(res.is_err());
}

#[test]
fn test_extract_backend_error_shapes() {
    use serde_json::json;
    let e = |v: serde_json::Value| crate::reasoning::extract_backend_error(&v);
    assert_eq!(e(json!({ "error": "boom" })), Some("boom".to_string()));
    assert_eq!(
        e(json!({ "error": { "message": "model not loaded", "code": 100 } })),
        Some("model not loaded".to_string())
    );
    // Normal streaming chunks must never be misdetected.
    assert_eq!(
        e(json!({ "choices": [{ "delta": { "content": "hi" } }] })),
        None
    );
    assert_eq!(e(json!({ "error": null })), None);
    assert_eq!(e(json!({ "done": true })), None);
}

#[test]
fn test_capture_chunk_diagnostics_first_wins() {
    use serde_json::json;
    let mut fr: Option<String> = None;
    let mut be: Option<String> = None;
    // OpenAI finish_reason (`choices[0].finish_reason`) is captured.
    assert_eq!(
        crate::reasoning::capture_chunk_diagnostics(
            &json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
            &mut fr,
            &mut be,
        ),
        None
    );
    assert_eq!(fr.as_deref(), Some("stop"));
    // The first backend error is returned / stored; `finish_reason` is kept.
    assert_eq!(
        crate::reasoning::capture_chunk_diagnostics(
            &json!({ "error": { "message": "model not loaded" } }),
            &mut fr,
            &mut be,
        ),
        Some("model not loaded".to_string())
    );
    assert_eq!(fr.as_deref(), Some("stop"));
    assert_eq!(be.as_deref(), Some("model not loaded"));
    // Later backend errors must not overwrite the first one.
    assert_eq!(
        crate::reasoning::capture_chunk_diagnostics(
            &json!({ "error": "second" }),
            &mut fr,
            &mut be,
        ),
        None
    );
    assert_eq!(be.as_deref(), Some("model not loaded"));
    // Ollama native `done_reason` fallback.
    let mut fr2: Option<String> = None;
    let mut be2: Option<String> = None;
    assert_eq!(
        crate::reasoning::capture_chunk_diagnostics(
            &json!({ "done": true, "done_reason": "length" }),
            &mut fr2,
            &mut be2,
        ),
        None
    );
    assert_eq!(fr2.as_deref(), Some("length (ollama)"));
}

#[test]
fn test_empty_response_diag_compact() {
    use crate::llm_stats::LlmRequestInfo;
    let ri = |done_seen: bool,
              finish_reason: Option<String>,
              backend_error: Option<String>,
              sse_parse_errors: u32,
              sse_utf8_errors: u32| LlmRequestInfo {
        latency_ms: 1,
        ttft_ms: 1,
        request_bytes: 1,
        response_bytes: 1,
        done_seen,
        finish_reason,
        backend_error,
        sse_parse_errors,
        sse_utf8_errors,
    };
    // A cleanly empty response carries no diagnostics.
    assert_eq!(
        crate::reasoning::empty_response_diag(&ri(true, None, None, 0, 0)),
        ""
    );
    // Truncated stream + finish reason + backend error are all reported.
    let d = crate::reasoning::empty_response_diag(&ri(
        false,
        Some("stop".to_string()),
        Some("model not loaded".to_string()),
        2,
        1,
    ));
    assert!(d.contains("finish_reason=stop"));
    assert!(d.contains("stream ended without [DONE]"));
    assert!(d.contains("2 unparseable line(s)"));
    assert!(d.contains("1 invalid-UTF-8 line(s)"));
    assert!(d.contains("backend error: model not loaded"));
}
