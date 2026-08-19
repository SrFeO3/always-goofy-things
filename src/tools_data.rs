//! Database query tools: data_search, data_schema.
//!
//! Implements capabilities such as read-only data retrieval and schema
//! discovery for remote databases via REST API.
//!
//! # Safety Warning
//!
//! These tools query external databases and may leak sensitive data or corrupt
//! contents if misconfigured. Use only with trusted, isolated databases.
//!
//! A simple prefix check allows only read-only queries
//! (SELECT/SHOW/DESCRIBE/EXPLAIN/WITH).
//!
//! # Available Tools
//!
//! - `data_search`: Execute read-only queries against the connected database
//!   to search, analyze, and retrieve data (metrics, logs, or event data).
//! - `data_schema`: Discover the schema of the connected database. List all
//!   tables or describe a specific table's columns, types, and sample values.
//!
//! # Supported Databases
//!
//! GreptimeDB, ClickHouse, and InfluxDB v3 are currently supported, with
//! Prometheus, Elasticsearch, and Splunk planned for future releases.
//!
//! | db_type      | Default URL                 | Auth            |
//! |-------------|----------------------------|-----------------|
//! | greptimedb  | http://localhost:4000      | Basic           |
//! | clickhouse  | http://localhost:8123      | Basic           |
//! | influxdb    | http://localhost:8086      | Bearer Token    |

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::startup;

// ---------------------------------------------------------------------------
// Constants -- default endpoints for development / testing
// ---------------------------------------------------------------------------

/// GreptimeDB standalone default HTTP endpoint (dev / test).
#[allow(dead_code)]
const GREPTIMEDB_DEFAULT_URL: &str = "http://localhost:4000/v1/sql";

/// ClickHouse default HTTP endpoint (port 8123 is the native HTTP query interface).
#[allow(dead_code)]
const CLICKHOUSE_DEFAULT_URL: &str = "http://localhost:8123";

/// InfluxDB v3 default HTTP endpoint.
#[allow(dead_code)]
const INFLUXDB_DEFAULT_URL: &str = "http://localhost:8086/api/v2/query";

// ---------------------------------------------------------------------------
// DbContext -- connection parameters built once from Config
// ---------------------------------------------------------------------------

/// DB connection context built once from Config, passed to all handlers.
#[derive(Debug, Clone)]
pub(crate) struct DbContext {
    pub db_type: String,
    pub db_url: String,
    pub db_auth_key: Option<String>,
    pub db_timeout: u64,
    pub db_max_bytes: usize,
}

/// Build a `DbContext` from CLI config. Returns `None` when DB is not configured.
pub(crate) fn db_context_from_config(config: &startup::Config) -> Option<DbContext> {
    let db_type = config.db_type.as_deref()?;
    let db_url = config.db_url.as_deref()?;
    Some(DbContext {
        db_type: db_type.to_string(),
        db_url: db_url.to_string(),
        db_auth_key: config.db_auth_key.clone(),
        db_timeout: config.db_timeout,
        db_max_bytes: config.db_max_bytes,
    })
}

// ---------------------------------------------------------------------------
// Placeholder values for tool description generation
// ---------------------------------------------------------------------------

struct DbPlaceholders {
    db_label: &'static str,
    db_hint: &'static str,
    query_lang: &'static str,
    query_allowed: &'static str,
}

fn get_placeholders(db_type: &str) -> Result<DbPlaceholders> {
    match db_type {
        "greptimedb" => Ok(DbPlaceholders {
            db_label: "GreptimeDB",
            db_hint: "Write standard SQL. Use time-bounding WHERE clauses and LIMIT.",
            query_lang: "SQL",
            query_allowed: "Only SELECT/SHOW/DESCRIBE/EXPLAIN are allowed.",
        }),
        "clickhouse" => Ok(DbPlaceholders {
            db_label: "ClickHouse",
            db_hint: "Write standard SQL. Use LIMIT to control result size.",
            query_lang: "SQL",
            query_allowed: "Only SELECT/SHOW/DESCRIBE/EXPLAIN are allowed.",
        }),
        "influxdb" => Ok(DbPlaceholders {
            db_label: "InfluxDB v3",
            db_hint: "Write standard SQL (preferred) or InfluxQL. Use time-range filters and LIMIT.",
            query_lang: "SQL or InfluxQL",
            query_allowed: "Only read-only queries (SELECT/SHOW) are allowed.",
        }),
        _ => bail!(
            "[DB_UNKNOWN_TYPE] Unsupported database type '{}'. Supported types: greptimedb, clickhouse, influxdb.",
            db_type
        ),
    }
}

// ---------------------------------------------------------------------------
// Query sanitization -- read-only enforcement
// ---------------------------------------------------------------------------

/// Validate that `query` starts with a read-only keyword (SELECT, SHOW,
/// DESCRIBE/DESC, EXPLAIN, WITH). Leading whitespace and SQL comments
/// (`-- ...\n`, `/* ... */`) are stripped before the check.
fn sanitize_query(query: &str) -> Result<()> {
    let mut s = query.to_string();

    // Strip leading SQL comments and whitespace iteratively
    loop {
        let trimmed = s.trim_start().to_string();
        if trimmed.is_empty() {
            bail!(
                "[DB_READONLY_VIOLATION] Empty query. Write operations are not allowed. \
                 Only SELECT/SHOW/DESCRIBE/EXPLAIN are permitted."
            );
        }
        if let Some(rest) = trimmed.strip_prefix("--") {
            if let Some(nl) = rest.find('\n') {
                s = rest[nl + 1..].to_string();
                continue;
            }
            // Entire remainder is a single-line comment → empty query
            bail!(
                "[DB_READONLY_VIOLATION] Query contains only comments. Write operations are not allowed. \
                 Only SELECT/SHOW/DESCRIBE/EXPLAIN are permitted."
            );
        }
        if let Some(rest) = trimmed.strip_prefix("/*") {
            if let Some(end) = rest.find("*/") {
                s = rest[end + 2..].to_string();
                continue;
            }
            bail!("[DB_READONLY_VIOLATION] Unclosed block comment.");
        }
        s = trimmed;
        break;
    }

    // Extract the first word (case-insensitive)
    let first_word = s.split_whitespace().next().unwrap_or("").to_uppercase();

    match first_word.as_str() {
        "SELECT" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "WITH" => Ok(()),
        _ => bail!(
            "[DB_READONLY_VIOLATION] Write operations are not allowed. \
             Only SELECT/SHOW/DESCRIBE/EXPLAIN are permitted. \
             Rewrite your query as a read-only operation."
        ),
    }
}

// ---------------------------------------------------------------------------
// Tool definition builders
// ---------------------------------------------------------------------------

/// Build the `data_search` tool definition with db_type-specific descriptions.
pub(crate) fn build_data_search_def(db_type: &str) -> Result<Value> {
    let p = get_placeholders(db_type)?;
    let desc = format!(
        "Execute read-only queries against {} to search, analyze, and retrieve data (metrics, logs, or event data). {} Use data_schema first if you need to discover table structures.",
        p.db_label, p.db_hint
    );
    let query_desc = format!(
        "The {} query to execute. {} Always include LIMIT or equivalent result bounding to prevent excessive data retrieval.",
        p.query_lang, p.query_allowed
    );

    Ok(json!({
        "type": "function",
        "function": {
            "name": "data_search",
            "description": desc,
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": query_desc
                    }
                },
                "required": ["query"]
            }
        }
    }))
}

/// Build the `data_schema` tool definition (fixed description).
pub(crate) fn build_data_schema_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "data_schema",
            "description": "Discover the schema of the connected database. List all tables or describe a specific table's columns, types, and sample values. Call this BEFORE writing queries to understand the data structure.",
            "parameters": {
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "Optional table name to describe. If omitted, lists all available tables."
                    }
                },
                "required": []
            }
        }
    })
}

// ---------------------------------------------------------------------------
// HTTP execution helpers
// ---------------------------------------------------------------------------

/// Build an HTTP client with the configured timeout.
fn build_http_client(timeout_secs: u64) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| anyhow!("[DB_INTERNAL_ERROR] Failed to create HTTP client: {}", e))
}

/// Add authentication header to a request builder based on db_type.
fn add_auth_header(
    builder: reqwest::RequestBuilder,
    db_type: &str,
    auth_key: Option<&str>,
) -> reqwest::RequestBuilder {
    let Some(key) = auth_key else {
        return builder;
    };
    match db_type {
        "greptimedb" | "clickhouse" => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(key);
            builder.header("Authorization", format!("Basic {}", encoded))
        }
        "influxdb" => builder.header("Authorization", format!("Token {}", key)),
        _ => builder,
    }
}

/// Map an HTTP response to a Result, translating HTTP errors to DB_xxx codes.
async fn check_http_response(response: reqwest::Response, db_url: &str) -> Result<String> {
    let status = response.status();
    if status.is_success() {
        return response
            .text()
            .await
            .map_err(|e| anyhow!("[DB_NETWORK_ERROR] Failed to read response body: {}", e));
    }

    // Read error body for diagnostics
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "(unreadable)".to_string());

    match status.as_u16() {
        401 | 403 => bail!(
            "[DB_AUTH_ERROR] Authentication failed. Check --db-auth-key, or try without authentication for local databases. (HTTP {})",
            status
        ),
        400 => bail!(
            "[DB_SYNTAX_ERROR] Query syntax error from database: {}. Check your SQL syntax and try again.",
            truncate_error_body(body.trim())
        ),
        404 => bail!(
            "[DB_EXEC_ERROR] Resource not found: {}. Verify table/column names exist (use data_schema to check).",
            truncate_error_body(body.trim())
        ),
        500 => bail!(
            "[DB_EXEC_ERROR] Execution error: {}. Verify table/column names exist (use data_schema to check).",
            truncate_error_body(body.trim())
        ),
        _ => bail!(
            "[DB_NETWORK_ERROR] Cannot reach database at {}. HTTP {}: {}. Verify the database is running and --db-url is correct.",
            db_url,
            status,
            truncate_error_body(body.trim())
        ),
    }
}

/// Truncate `body` to `max_bytes` at a valid UTF-8 boundary, appending
/// a `[DB_TRUNCATED]` notice when truncation occurs.
fn truncate_body(body: &str, max_bytes: usize, max_bytes_orig: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }

    // Walk back from max_bytes to find a valid UTF-8 character boundary
    let mut boundary = max_bytes;
    while boundary > 0 && !body.is_char_boundary(boundary) {
        boundary -= 1;
    }

    let truncated = &body[..boundary];
    format!(
        "{}\n[DB_TRUNCATED] Result truncated at {} bytes. Use tighter WHERE filters, GROUP BY aggregation, or smaller LIMIT to retrieve complete data.",
        truncated, max_bytes_orig
    )
}

/// Max chars of a DB error detail kept in an error message. Bodies can be
/// huge (e.g. GreptimeDB lists every valid column); the cause is at the head,
/// so a head cap keeps the useful part for any DB.
const DB_ERROR_DETAIL_MAX_CHARS: usize = 500;

/// Head-cap a DB error detail (char-boundary safe) with a truncation notice
/// distinct from the success-oriented one in `truncate_body`.
fn truncate_error_body(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    if chars.len() <= DB_ERROR_DETAIL_MAX_CHARS {
        return body.to_string();
    }
    let head: String = chars[..DB_ERROR_DETAIL_MAX_CHARS].iter().collect();
    format!(
        "{}\n[DB_TRUNCATED] Error detail truncated at {} characters (front kept).",
        head, DB_ERROR_DETAIL_MAX_CHARS
    )
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Execute a read-only query against the configured database.
pub(crate) async fn execute_data_search(ctx: &DbContext, query: &str) -> Result<Value> {
    sanitize_query(query)?;

    let client = build_http_client(ctx.db_timeout)?;

    match ctx.db_type.as_str() {
        "greptimedb" => {
            // Append format parameter to URL
            let url = if ctx.db_url.contains('?') {
                format!("{}&format=csvWithNames", ctx.db_url)
            } else {
                format!("{}?format=csvWithNames", ctx.db_url)
            };

            let body = format!("sql={}", urlencoding(query));
            let builder = client
                .post(&url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body);
            let builder = add_auth_header(builder, "greptimedb", ctx.db_auth_key.as_deref());

            let response = builder.send().await.map_err(|e| {
                if e.is_timeout() {
                    anyhow!(
                        "[DB_TIMEOUT] Query timed out after {}s. Add tighter WHERE filters, reduce the time range, or increase --db-timeout.",
                        ctx.db_timeout
                    )
                } else if e.is_connect() || e.is_request() {
                    anyhow!(
                        "[DB_NETWORK_ERROR] Cannot reach database at {}. Verify the database is running and --db-url is correct. ({})",
                        ctx.db_url, e
                    )
                } else {
                    anyhow!("[DB_INTERNAL_ERROR] {}", e)
                }
            })?;

            let text = check_http_response(response, &ctx.db_url).await?;
            let truncated = truncate_body(&text, ctx.db_max_bytes, ctx.db_max_bytes);
            Ok(json!({"content": truncated}))
        }

        "clickhouse" => {
            // Append FORMAT CSVWithNames to the query (after sanitization)
            let full_query = format!("{} FORMAT CSVWithNames", query);

            let builder = client
                .post(&ctx.db_url)
                .header("Content-Type", "text/plain")
                .body(full_query);
            let builder = add_auth_header(builder, "clickhouse", ctx.db_auth_key.as_deref());

            let response = builder.send().await.map_err(|e| {
                if e.is_timeout() {
                    anyhow!(
                        "[DB_TIMEOUT] Query timed out after {}s. Add tighter WHERE filters, reduce the time range, or increase --db-timeout.",
                        ctx.db_timeout
                    )
                } else if e.is_connect() || e.is_request() {
                    anyhow!(
                        "[DB_NETWORK_ERROR] Cannot reach database at {}. Verify the database is running and --db-url is correct. ({})",
                        ctx.db_url, e
                    )
                } else {
                    anyhow!("[DB_INTERNAL_ERROR] {}", e)
                }
            })?;

            let text = check_http_response(response, &ctx.db_url).await?;
            let truncated = truncate_body(&text, ctx.db_max_bytes, ctx.db_max_bytes);
            Ok(json!({"content": truncated}))
        }

        "influxdb" => {
            let payload = json!({"query": query});

            let builder = client
                .post(&ctx.db_url)
                .header("Content-Type", "application/json")
                .header("Accept", "text/csv")
                .json(&payload);
            let builder = add_auth_header(builder, "influxdb", ctx.db_auth_key.as_deref());

            let response = builder.send().await.map_err(|e| {
                if e.is_timeout() {
                    anyhow!(
                        "[DB_TIMEOUT] Query timed out after {}s. Add tighter WHERE filters, reduce the time range, or increase --db-timeout.",
                        ctx.db_timeout
                    )
                } else if e.is_connect() || e.is_request() {
                    anyhow!(
                        "[DB_NETWORK_ERROR] Cannot reach database at {}. Verify the database is running and --db-url is correct. ({})",
                        ctx.db_url, e
                    )
                } else {
                    anyhow!("[DB_INTERNAL_ERROR] {}", e)
                }
            })?;

            let text = check_http_response(response, &ctx.db_url).await?;
            let truncated = truncate_body(&text, ctx.db_max_bytes, ctx.db_max_bytes);
            Ok(json!({"content": truncated}))
        }

        _ => bail!(
            "[DB_UNKNOWN_TYPE] Unsupported database type '{}'. Supported types: greptimedb, clickhouse, influxdb.",
            ctx.db_type
        ),
    }
}

/// Discover the database schema: list all tables, or describe a specific table.
pub(crate) async fn execute_data_schema(ctx: &DbContext, table: Option<&str>) -> Result<Value> {
    let query = if let Some(table_name) = table {
        match ctx.db_type.as_str() {
            "clickhouse" => format!("DESCRIBE TABLE {}", table_name),
            _ => format!("DESCRIBE {}", table_name),
        }
    } else {
        "SHOW TABLES".to_string()
    };

    execute_data_search(ctx, &query).await
}

// ---------------------------------------------------------------------------
// URL encoding
// ---------------------------------------------------------------------------

fn urlencoding(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push('%');
                result.push(hex_char(byte >> 4));
                result.push(hex_char(byte & 0x0F));
            }
        }
    }
    result
}

fn hex_char(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tests/tools_data_test.rs"]
mod tests;
