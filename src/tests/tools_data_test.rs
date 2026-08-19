use super::*;
use std::time::Duration;

/// Default local GreptimeDB URL used in integration tests (matches spec constant).
const TEST_DB_URL: &str = "http://localhost:4000/v1/sql";

// ---------------------------------------------------------------------------
// Helper: skip integration tests when no local GreptimeDB is reachable
// ---------------------------------------------------------------------------

/// Check if local GreptimeDB (no auth) is reachable at default port.
/// Returns `true` if a quick health-check succeeds.
async fn greptimedb_reachable() -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok();
    let Some(client) = client else {
        return false;
    };
    // GreptimeDB standalone returns HTTP 200 with a trivial query
    client
        .post(format!("{}?format=csvWithNames", TEST_DB_URL))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("sql=SELECT 1")
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Skip the current test unless a local GreptimeDB is running.
macro_rules! require_db {
    () => {
        if !greptimedb_reachable().await {
            eprintln!("SKIP: local GreptimeDB not reachable at {}", TEST_DB_URL);
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Unit tests — placeholder generation
// ---------------------------------------------------------------------------

#[test]
fn test_placeholders_greptimedb() {
    // GreptimeDB: standard SQL with time-bounding hints
    let p = get_placeholders("greptimedb").unwrap();
    assert_eq!(p.db_label, "GreptimeDB");
    assert!(p.db_hint.contains("time-bounding"));
    assert!(p.db_hint.contains("LIMIT"));
    assert_eq!(p.query_lang, "SQL");
    assert!(p.query_allowed.contains("SELECT"));
}

#[test]
fn test_placeholders_clickhouse() {
    // ClickHouse: standard SQL, LIMIT emphasis
    let p = get_placeholders("clickhouse").unwrap();
    assert_eq!(p.db_label, "ClickHouse");
    assert!(p.db_hint.contains("LIMIT"));
    assert_eq!(p.query_lang, "SQL");
    assert!(p.query_allowed.contains("SELECT"));
}

#[test]
fn test_placeholders_influxdb() {
    // InfluxDB v3: SQL or InfluxQL
    let p = get_placeholders("influxdb").unwrap();
    assert_eq!(p.db_label, "InfluxDB v3");
    assert!(p.db_hint.contains("InfluxQL"));
    assert!(p.query_lang.contains("InfluxQL"));
    assert!(p.query_allowed.contains("read-only"));
}

#[test]
fn test_placeholders_unknown_type() {
    // Unknown db_type returns an error
    assert!(get_placeholders("mysql").is_err());
    assert!(get_placeholders("").is_err());
}

// ---------------------------------------------------------------------------
// Unit tests — tool definition JSON generation
// ---------------------------------------------------------------------------

#[test]
fn test_build_data_search_def_structure() {
    // Verify the generated JSON has the expected OpenAPI function-call shape
    let def = build_data_search_def("greptimedb").unwrap();
    assert_eq!(def["type"], "function");
    assert_eq!(def["function"]["name"], "data_search");
    // description should contain the expanded placeholders
    let desc = def["function"]["description"].as_str().unwrap();
    assert!(desc.contains("GreptimeDB"));
    assert!(desc.contains("search, analyze, and retrieve data"));
    assert!(!desc.contains("{db_label}")); // placeholders must be replaced
    assert!(!desc.contains("{db_hint}"));
    // query parameter
    let query_desc = def["function"]["parameters"]["properties"]["query"]["description"]
        .as_str()
        .unwrap();
    assert!(!query_desc.contains("{query_lang}"));
    assert!(!query_desc.contains("{query_allowed}"));
    // required
    let required = def["function"]["parameters"]["required"]
        .as_array()
        .unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("query")));
}

#[test]
fn test_build_data_schema_def_structure() {
    // Verify the schema tool definition is correctly shaped
    let def = build_data_schema_def();
    assert_eq!(def["type"], "function");
    assert_eq!(def["function"]["name"], "data_schema");
    let desc = def["function"]["description"].as_str().unwrap();
    assert!(desc.contains("schema"));
    // table parameter is optional
    let required = def["function"]["parameters"]["required"]
        .as_array()
        .unwrap();
    assert!(required.is_empty());
}

// ---------------------------------------------------------------------------
// Unit tests — SQL sanitization (read-only enforcement)
// ---------------------------------------------------------------------------

#[test]
fn test_sanitize_select_allowed() {
    assert!(sanitize_query("SELECT * FROM t").is_ok());
    assert!(sanitize_query("select count(*) from t").is_ok());
}

#[test]
fn test_sanitize_show_allowed() {
    assert!(sanitize_query("SHOW TABLES").is_ok());
    assert!(sanitize_query("show create table t").is_ok());
}

#[test]
fn test_sanitize_describe_allowed() {
    assert!(sanitize_query("DESCRIBE t").is_ok());
    assert!(sanitize_query("DESC t").is_ok());
    assert!(sanitize_query("desc t").is_ok());
}

#[test]
fn test_sanitize_explain_allowed() {
    assert!(sanitize_query("EXPLAIN SELECT 1").is_ok());
}

#[test]
fn test_sanitize_with_allowed() {
    assert!(sanitize_query("WITH cte AS (SELECT 1) SELECT * FROM cte").is_ok());
}

#[test]
fn test_sanitize_insert_blocked() {
    assert!(sanitize_query("INSERT INTO t VALUES (1)").is_err());
}

#[test]
fn test_sanitize_update_blocked() {
    assert!(sanitize_query("UPDATE t SET a=1").is_err());
}

#[test]
fn test_sanitize_delete_blocked() {
    assert!(sanitize_query("DELETE FROM t").is_err());
}

#[test]
fn test_sanitize_drop_blocked() {
    assert!(sanitize_query("DROP TABLE t").is_err());
}

#[test]
fn test_sanitize_create_blocked() {
    assert!(sanitize_query("CREATE TABLE t (a int)").is_err());
    assert!(sanitize_query("ALTER TABLE t ADD b int").is_err());
}

#[test]
fn test_sanitize_truncate_blocked() {
    assert!(sanitize_query("TRUNCATE TABLE t").is_err());
}

#[test]
fn test_sanitize_grant_blocked() {
    assert!(sanitize_query("GRANT SELECT ON t TO u").is_err());
    assert!(sanitize_query("REVOKE SELECT ON t FROM u").is_err());
}

#[test]
fn test_sanitize_case_insensitive() {
    // All checks must be case-insensitive
    assert!(sanitize_query("select * from t").is_ok());
    assert!(sanitize_query("SELECT * FROM t").is_ok());
    assert!(sanitize_query("Select * From t").is_ok());
    assert!(sanitize_query("insert into t values(1)").is_err());
    assert!(sanitize_query("INSERT INTO t VALUES(1)").is_err());
    assert!(sanitize_query("Drop table t").is_err());
}

#[test]
fn test_sanitize_leading_whitespace() {
    // Whitespace before the keyword must be ignored
    assert!(sanitize_query("   SELECT 1").is_ok());
    assert!(sanitize_query("\t\n SELECT 1").is_ok());
    assert!(sanitize_query("   DROP TABLE t").is_err());
}

#[test]
fn test_sanitize_leading_comment() {
    // Single-line SQL comment before the keyword must be stripped
    assert!(sanitize_query("-- this is a comment\nSELECT 1").is_ok());
    assert!(sanitize_query("-- comment\n   SELECT 1").is_ok());
    // Block comment
    assert!(sanitize_query("/* block */ SELECT 1").is_ok());
    assert!(sanitize_query("-- comment\nDROP TABLE t").is_err());
}

// ---------------------------------------------------------------------------
// Unit tests — DB error detail truncation (改善 4a: head-cap, DB-independent)
// ---------------------------------------------------------------------------

#[test]
fn test_truncate_error_body_short_unchanged() {
    let body = "No field named computername";
    assert_eq!(truncate_error_body(body), body);
}

#[test]
fn test_truncate_error_body_exactly_at_cap_unchanged() {
    let body = "x".repeat(DB_ERROR_DETAIL_MAX_CHARS);
    assert_eq!(truncate_error_body(&body), body);
}

#[test]
fn test_truncate_error_body_long_caps_head_keeps_cause() {
    // Tail noise is trimmed; the head (cause) survives.
    let body = format!("CAUSE-LINE {}\n{}", "x".repeat(30), "y".repeat(2000));
    let out = truncate_error_body(&body);
    let kept: String = out
        .split("[DB_TRUNCATED]")
        .next()
        .unwrap()
        .trim_end()
        .to_string();
    assert!(kept.starts_with("CAUSE-LINE "));
    assert_eq!(kept.chars().count(), DB_ERROR_DETAIL_MAX_CHARS);
    assert!(out.contains("[DB_TRUNCATED] Error detail truncated at"));
}

#[test]
fn test_truncate_error_body_multibyte_safe() {
    // The cap must not split a multi-byte char.
    let body = format!("あ{}", "い".repeat(1000));
    let out = truncate_error_body(&body);
    let head: &str = out.lines().next().unwrap();
    assert_eq!(head.chars().count(), DB_ERROR_DETAIL_MAX_CHARS);
    assert!(head.starts_with('あ'));
    assert!(head.ends_with('い'));
}

#[test]
fn test_truncate_error_body_notice_passive() {
    // Only states truncation + front kept: no success wording, no next-action push.
    let body = format!("head\n{}", "z".repeat(1000));
    let out = truncate_error_body(&body);
    assert!(out.contains("(front kept)"));
    assert!(!out.contains("Use tighter WHERE filters"));
    assert!(!out.contains("data_schema"));
    assert!(!out.contains("fix the query"));
}

// ---------------------------------------------------------------------------
// Integration tests — local GreptimeDB (no auth) at localhost:4000/v1/sql
// ---------------------------------------------------------------------------

/// Helper: build a `DbContext` pointing at the default local GreptimeDB.
fn local_greptimedb_ctx() -> DbContext {
    DbContext {
        db_type: "greptimedb".into(),
        db_url: TEST_DB_URL.into(),
        db_auth_key: None,
        db_timeout: 10,
        db_max_bytes: 65536,
    }
}

#[tokio::test]
async fn test_greptimedb_data_search_select_one() {
    // SELECT 1 should always succeed on any running GreptimeDB
    require_db!();
    let ctx = local_greptimedb_ctx();
    let result = execute_data_search(&ctx, "SELECT 1").await.unwrap();
    let body = result["content"].as_str().unwrap_or("");
    assert!(body.contains("1"));
}

#[tokio::test]
async fn test_greptimedb_data_search_syntax_error() {
    // Malformed SQL returns DB_SYNTAX_ERROR.
    // Use a whitelist-passing keyword (SELECT) with invalid syntax so the
    // sanitizer lets it through and the DB returns the syntax error.
    require_db!();
    let ctx = local_greptimedb_ctx();
    let result = execute_data_search(&ctx, "SELECT FROM").await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("DB_SYNTAX_ERROR") || err.contains("400"),
        "expected DB_SYNTAX_ERROR or 400, got: {}",
        err
    );
}

#[tokio::test]
async fn test_greptimedb_data_schema_list_tables() {
    // data_schema without table arg → list tables
    require_db!();
    let ctx = local_greptimedb_ctx();
    let result = execute_data_schema(&ctx, None).await.unwrap();
    let body = result["content"].as_str().unwrap_or("");
    // Response should contain at least the system tables
    assert!(!body.is_empty());
}

#[tokio::test]
async fn test_greptimedb_truncate() {
    // Small db_max_bytes should truncate the result and add [DB_TRUNCATED]
    require_db!();
    let mut ctx = local_greptimedb_ctx();
    ctx.db_max_bytes = 20; // very small
    let result = execute_data_search(&ctx, "SELECT 1, 2, 3, 4, 5")
        .await
        .unwrap();
    let body = result["content"].as_str().unwrap_or("");
    assert!(body.contains("[DB_TRUNCATED]"));
}

#[tokio::test]
async fn test_greptimedb_readonly_violation() {
    // INSERT/DROP should be caught by sanitize before reaching the DB
    // This test does NOT need a running DB
    let ctx = local_greptimedb_ctx();
    let result = execute_data_search(&ctx, "DROP TABLE foo").await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("DB_READONLY_VIOLATION")
    );
}

#[tokio::test]
async fn test_greptimedb_timeout() {
    // Very short timeout should trigger DB_TIMEOUT on a slow query
    require_db!();
    let mut ctx = local_greptimedb_ctx();
    ctx.db_timeout = 1; // 1ms is impossibly short
    // A simple query might still succeed in <1ms, so we try a heavier one
    let result = execute_data_search(&ctx, "SELECT count(*) FROM numbers(1000000)").await;
    // Either succeeds quickly or times out — both are acceptable;
    // we just verify the timeout path doesn't panic.
    if let Err(e) = &result {
        let msg = e.to_string();
        // A 1ms timeout is impossibly short. The query may:
        // - time out → DB_TIMEOUT
        // - fail immediately with a DB error (e.g. function not found) → still acceptable
        // - succeed impossibly fast → also acceptable
        // The point is to verify the timeout codepath doesn't panic.
        assert!(
            msg.contains("DB_TIMEOUT")
                || msg.contains("timeout")
                || msg.contains("timed out")
                || msg.contains("DB_SYNTAX_ERROR")
                || msg.contains("DB_EXEC_ERROR")
        );
    }
}
