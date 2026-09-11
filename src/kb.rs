//! Knowledge Base (KB) feature: the `data_kb_*` tools and the `/kb` command.
//!
//! Executes the four KB tools (read: `data_kb_search` / `data_kb_schema`,
//! write: `data_kb_insert` / `data_kb_update`) and the `/kb` command
//! (add / list / delete / sync / backup) against the single SQLite file
//! `<kb>/db/library.sqlite`. KB tools are KB-dedicated, with an approval
//! gate independent of `reflex` (`tools::confirm_execute_tool`).

use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, anyhow, bail};
use rusqlite::{Connection, Transaction, params, params_from_iter};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::kb_schema;
use crate::startup;

/// Max items per `data_kb_insert` call (spec: quota).
const KB_INSERT_MAX_ITEMS: usize = 500;
/// Max JSON body bytes per `data_kb_insert` call (spec: quota).
const KB_INSERT_MAX_BYTES: usize = 1_000_000;
/// Default `data_kb_search` result truncation (overridden by --kb-max-bytes).
const KB_DEFAULT_MAX_BYTES: usize = 65536;
/// Stale `running` threshold: runs older than this are marked failed.
const STALE_RUN_HOURS: i64 = 24;
/// Machine-extraction label used in unit metadata.
const KB_PARSER_VERSION: &str = "0.1.0";

// ---------------------------------------------------------------------------
// Connection lock (single process-wide connection)
// ---------------------------------------------------------------------------

thread_local! {
    /// Same-thread hold flag of the KB connection lock. The std `Mutex` is
    /// non-reentrant: a KB helper called while the lock is already held on
    /// this thread would deadlock silently. `lock_conn()` checks this flag
    /// and rejects re-entrant acquisition with a clear error instead.
    static CONN_LOCK_HELD: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard over the shared connection that keeps `CONN_LOCK_HELD` in sync.
struct ConnGuard<'a>(MutexGuard<'a, Connection>);

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        CONN_LOCK_HELD.with(|h| h.set(false));
    }
}

impl std::ops::Deref for ConnGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.0
    }
}

impl std::ops::DerefMut for ConnGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.0
    }
}

/// Acquire the shared connection lock. Re-entrant acquisition from the same
/// thread (a KB helper called while the lock is held) fails loudly instead of
/// deadlocking; a poisoned mutex maps to the usual `[KB_INTERNAL_ERROR]`.
fn lock_conn(ctx: &KbContext) -> Result<ConnGuard<'_>> {
    CONN_LOCK_HELD.with(|h| -> Result<()> {
        if h.get() {
            bail!(
                "[KB_INTERNAL_ERROR] Re-entrant acquisition of the KB connection lock would \
                 deadlock (std::sync::Mutex is not reentrant): do not call a KB helper while \
                 holding the lock."
            );
        }
        h.set(true);
        Ok(())
    })?;
    let guard = ctx.conn.lock().map_err(|e| {
        CONN_LOCK_HELD.with(|h| h.set(false));
        anyhow!("[KB_INTERNAL_ERROR] {}", e)
    })?;
    Ok(ConnGuard(guard))
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// KB run context: the KB directory, the shared SQLite connection, the byte
/// cap for `data_kb_search` results, and the `analysis_runs` id for this
/// process (run granularity = one process start, per spec).
pub(crate) struct KbContext {
    pub kb_dir: String,
    pub max_bytes: usize,
    pub conn: Arc<Mutex<Connection>>,
    pub run_id: Uuid,
}

/// Default KB root when `--kb-dir` / `KB_DIR` is unset: the app data
/// directory (`SESSION_DATA_DIR` or platform app-data), under `kb/`, one
/// library per working directory (`kb-<sanitized cwd>-<hash8>`), so the
/// library stays outside the workspace and the name stays unique. Call after
/// the working directory is canonicalized (chdir).
pub(crate) fn default_kb_dir() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let cwd_str = cwd.to_string_lossy();
    let sanitized: String = cwd_str
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(cwd_str.as_bytes());
        let digest = hasher.finalize();
        digest[..8]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    };
    crate::persistence::data_dir().map(|d| d.join("kb").join(format!("kb-{}-{}", sanitized, hash)))
}

/// Build a `KbContext` from CLI config. The KB directory is `--kb-dir` /
/// `KB_DIR` when set; otherwise it defaults to the app data directory
/// (`kb/kb-<workdir>-<hash>`, one library per working directory). If neither
/// a configured directory nor a resolvable app data directory exists, fails
/// with `[KB_CONFIG_ERROR]`; so does directory / DB / migration failure.
pub(crate) fn kb_context_from_config(config: &startup::Config) -> Result<Option<KbContext>> {
    let kb_dir = match config.kb_dir.as_deref() {
        Some(d) => d.to_string(),
        None => match default_kb_dir() {
            Some(d) => d.to_string_lossy().into_owned(),
            None => bail!(
                "[KB_CONFIG_ERROR] No KB directory configured (--kb-dir / KB_DIR) and the app \
                 data directory could not be resolved."
            ),
        },
    };
    let db_dir = Path::new(&kb_dir).join("db");
    fs::create_dir_all(&db_dir).map_err(|e| {
        anyhow!(
            "[KB_CONFIG_ERROR] Knowledge base initialization failed at '{}': {}. \
             Check the directory permissions and that library.sqlite is not corrupted.",
            kb_dir,
            e
        )
    })?;
    let db_path = db_dir.join("library.sqlite");
    let conn = Connection::open(&db_path).map_err(|e| {
        anyhow!(
            "[KB_CONFIG_ERROR] Knowledge base initialization failed at '{}': {}. \
             Check the directory permissions and that library.sqlite is not corrupted.",
            kb_dir,
            e
        )
    })?;
    kb_schema::apply_pragmas(&conn)?;
    kb_schema::migrate(&conn)?;

    // Stale runs from a previous crash: running for > 24h -> failed.
    mark_stale_runs(&conn)?;
    // Run granularity = one KB process start.
    let run_id = Uuid::new_v4();
    conn.execute(
        "INSERT INTO analysis_runs (id, label, status, config) VALUES (?1, ?2, 'running', ?3)",
        params![
            run_id.to_string(),
            format!("auto {} {}", env!("CARGO_PKG_VERSION"), now_iso()),
            "{}"
        ],
    )
    .map_err(|e| anyhow!("[KB_CONFIG_ERROR] Failed to create analysis run: {}", e))?;

    Ok(Some(KbContext {
        kb_dir: kb_dir.to_string(),
        max_bytes: raw_max_bytes(config.kb_max_bytes),
        conn: Arc::new(Mutex::new(conn)),
        run_id,
    }))
}

/// Mark any `running` run older than 24h as `failed` (crash recovery).
fn mark_stale_runs(conn: &Connection) -> Result<()> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(STALE_RUN_HOURS))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    conn.execute(
        "UPDATE analysis_runs SET status = 'failed', finished_at = ?1, summary = ?2 \
         WHERE status = 'running' AND started_at < ?1",
        params![cutoff, "stale run (previous crash)"],
    )
    .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] Failed to mark stale runs: {}", e))?;
    Ok(())
}

/// Mark this process's run as `completed` on normal exit (best effort).
pub(crate) fn kb_finish_run(ctx: &KbContext) {
    if let Ok(conn) = lock_conn(ctx) {
        let _ = conn.execute(
            "UPDATE analysis_runs SET status = 'completed', finished_at = ?1 \
             WHERE id = ?2 AND status = 'running'",
            params![now_iso(), ctx.run_id.to_string()],
        );
    }
}

fn raw_max_bytes(configured: usize) -> usize {
    if configured == 0 {
        KB_DEFAULT_MAX_BYTES
    } else {
        configured
    }
}

fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let digest = h.finalize();
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

fn nfkc(s: &str) -> String {
    s.nfkc().collect()
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_json_obj(s: &str) -> Result<Map<String, Value>> {
    match serde_json::from_str::<Value>(s) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Ok(Map::new()),
    }
}

/// Merge `patch` into `target`; a null value removes the key (spec).
fn merge_json_obj(target: &mut Map<String, Value>, patch: &Value) -> Result<()> {
    let Some(obj) = patch.as_object() else {
        bail!("[KB_EXEC_ERROR] attributes / annotations must be JSON objects.");
    };
    for (k, v) in obj {
        if v.is_null() {
            target.remove(k);
        } else {
            target.insert(k.clone(), v.clone());
        }
    }
    Ok(())
}

fn json_to_text(v: Option<&Value>) -> Option<String> {
    v.map(|x| serde_json::to_string(x).unwrap_or_else(|_| "{}".to_string()))
}

/// Quote an identifier for safe interpolation into read SQL / PRAGMA.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Escape a file-system path for a `VACUUM INTO '...'`.
fn escape_sql_string(s: &str) -> String {
    s.replace('\'', "''")
}

/// Resolve `*_ref` / `*_id` pairs (local references resolved in a prior pass).
fn resolve_id_ref(
    item: &Value,
    id_field: &str,
    ref_field: &str,
    refs: &HashMap<String, (String, String)>,
) -> Result<Option<String>> {
    let id = item.get(id_field).and_then(|v| v.as_str());
    let rf = item.get(ref_field).and_then(|v| v.as_str());
    match (id, rf) {
        (Some(_), Some(_)) => bail!(
            "[KB_CONFLICT] Conflicting reference: specify either '{}' or '{}', not both.",
            id_field,
            ref_field
        ),
        (Some(i), None) => Ok(Some(i.to_string())),
        (None, Some(r)) => {
            let found = refs.get(r).ok_or_else(|| {
                anyhow!(
                    "[KB_REF_NOT_FOUND] Reference not found: '{}'. Insert the referenced item \
                     first or fix the id (query data_kb_search to find existing ids).",
                    r
                )
            })?;
            Ok(Some(found.1.clone()))
        }
        (None, None) => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// data_kb_search
// ---------------------------------------------------------------------------

/// Build the `data_kb_search` tool definition (KB-dedicated, read-only).
pub(crate) fn build_kb_search_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "data_kb_search",
            "description": "Execute read-only SQL queries against the Knowledge Base (entities, claims, relations, conditions, events, and evidence with provenance). Use data_kb_schema first if you need to discover table structures. Only the single <kb>/db/library.sqlite file is reachable. Always include LIMIT to control the result size. Results are returned as CSV with a header row.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The SQL query to execute. Only SELECT / WITH / EXPLAIN statements are allowed. Always include LIMIT to prevent excessive data retrieval."
                    }
                },
                "required": ["query"]
            }
        }
    })
}

/// Read-only query sanitizer: strip leading comments/whitespace, then check
/// the first keyword. Combined with `prepare`-only execution (multi-statement
/// queries are structurally rejected).
fn sanitize_query(query: &str) -> Result<()> {
    let mut s = query.to_string();
    loop {
        let trimmed = s.trim_start().to_string();
        if trimmed.is_empty() {
            bail!(
                "[KB_READONLY_VIOLATION] Empty query. Write operations are not allowed. \
                 Only SELECT/WITH/EXPLAIN are permitted."
            );
        }
        if let Some(rest) = trimmed.strip_prefix("--") {
            if let Some(nl) = rest.find('\n') {
                s = rest[nl + 1..].to_string();
                continue;
            }
            bail!(
                "[KB_READONLY_VIOLATION] Query contains only comments. \
                 Only SELECT/WITH/EXPLAIN are permitted."
            );
        }
        if let Some(rest) = trimmed.strip_prefix("/*") {
            if let Some(end) = rest.find("*/") {
                s = rest[end + 2..].to_string();
                continue;
            }
            bail!("[KB_READONLY_VIOLATION] Unclosed block comment.");
        }
        s = trimmed;
        break;
    }
    let first = s.split_whitespace().next().unwrap_or("").to_uppercase();
    match first.as_str() {
        "SELECT" | "WITH" | "EXPLAIN" => Ok(()),
        _ => bail!(
            "[KB_READONLY_VIOLATION] Write operations are not allowed. \
             Only SELECT/WITH/EXPLAIN are permitted. Rewrite your query as a read-only operation."
        ),
    }
}

/// Execute a read-only query against the single KB sqlite file and return CSV.
pub(crate) fn execute_kb_search(ctx: &KbContext, query: &str) -> Result<Value> {
    sanitize_query(query)?;
    let conn = lock_conn(ctx)?;
    let mut stmt = conn.prepare(query).map_err(|e| {
        anyhow!(
            "[KB_SYNTAX_ERROR] Query syntax error: {}. Check your SQL syntax and try again \
             (use data_kb_schema to confirm table/column names).",
            e
        )
    })?;
    let csv = rows_to_csv(&mut stmt).map_err(|e| {
        anyhow!(
            "[KB_EXEC_ERROR] Execution error: {}. Verify table/column names exist \
             (use data_kb_schema to check) and that you are not writing (INSERT/UPDATE/DELETE).",
            e
        )
    })?;
    let truncated = truncate_body(&csv, ctx.max_bytes);
    Ok(json!({ "content": truncated }))
}

/// Render a prepared statement as CSV with a header row.
fn rows_to_csv(stmt: &mut rusqlite::Statement) -> Result<String> {
    let col_count = stmt.column_count();
    let headers: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let mut out = String::new();
    out.push_str(&csv_join(&headers));
    out.push('\n');
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut vals: Vec<String> = Vec::with_capacity(col_count);
        for i in 0..col_count {
            vals.push(match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => String::new(),
                rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                rusqlite::types::ValueRef::Real(f) => f.to_string(),
                rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).to_string(),
                rusqlite::types::ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
            });
        }
        out.push_str(&csv_join(&vals));
        out.push('\n');
    }
    Ok(out)
}

fn csv_join(fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| {
            if f.contains(',') || f.contains('"') || f.contains('\n') || f.contains('\r') {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Truncate the CSV at `max_bytes` on a UTF-8 boundary, appending the
/// `[KB_TRUNCATED]` notice when truncation occurs.
fn truncate_body(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !body.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!(
        "{}\n[KB_TRUNCATED] Result truncated at {} bytes. Use tighter WHERE filters, \
         GROUP BY aggregation, or smaller LIMIT to retrieve complete data.",
        &body[..boundary],
        max_bytes
    )
}

// ---------------------------------------------------------------------------
// data_kb_schema
// ---------------------------------------------------------------------------

/// Build the `data_kb_schema` tool definition.
pub(crate) fn build_kb_schema_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "data_kb_schema",
            "description": "Discover the schema of the Knowledge Base. List all tables, or describe a specific table's columns, types, purpose, sample values, and related tables. Call this BEFORE writing data_kb_search queries to understand the data structure. Also lists the provided views (e.g. v_claims_with_evidence) and the FTS table units_fts with the Japanese search rules (>=3 chars: MATCH phrase, 1-2 chars: LIKE).",
            "parameters": {
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "Optional table or view name to describe. If omitted, lists all tables and views with their purpose and approximate row counts."
                    }
                },
                "required": []
            }
        }
    })
}

/// Approximate row counts from `sqlite_stat1` (never runs COUNT(*) at list
/// time). Falls back to an empty map when unavailable.
fn approx_row_counts(conn: &Connection) -> HashMap<String, i64> {
    let mut counts = HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT tbl, stat FROM sqlite_stat1") else {
        return counts;
    };
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
    if let Ok(iter) = rows {
        for row in iter.flatten() {
            let first = row.1.split_whitespace().next().unwrap_or("0");
            if let Ok(n) = first.parse::<i64>() {
                counts.entry(row.0).or_insert(n);
            }
        }
    }
    counts
}

/// Discover the schema: all tables/views, or one table in detail.
pub(crate) fn execute_kb_schema(ctx: &KbContext, table: Option<&str>) -> Result<Value> {
    let conn = lock_conn(ctx)?;
    let Some(name) = table.map(str::trim).filter(|s| !s.is_empty()) else {
        // List mode: whitelist the known KB tables / views (KB-dedicated:
        // unknown tables such as FTS shadow tables are never advertised).
        let meta = kb_schema::table_meta();
        let mut tables: Vec<(String, String)> = Vec::new();
        for t in kb_schema::KNOWLEDGE_TABLES {
            tables.push((t.to_string(), "table".to_string()));
        }
        for v in kb_schema::KNOWLEDGE_VIEWS {
            tables.push((v.to_string(), "view".to_string()));
        }
        tables.push((
            kb_schema::FTS_TABLE.to_string(),
            "table (FTS5 trigram)".to_string(),
        ));
        tables.sort();
        let counts = approx_row_counts(&conn);
        let items: Vec<Value> = tables
            .into_iter()
            .map(|(name, typ)| {
                let purpose = meta
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, p)| *p)
                    .unwrap_or("");
                json!({
                    "name": name,
                    "type": typ,
                    "purpose": purpose,
                    "approx_rows": counts.get(&name).copied().unwrap_or(0)
                })
            })
            .collect();
        return Ok(json!({ "status": "ok", "tables": items }));
    };

    // Detail mode.
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1 \
             AND type IN ('table','view'))",
            [name],
            |r| r.get(0),
        )
        .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?;
    if !exists {
        bail!(
            "[KB_EXEC_ERROR] Table or view not found: '{}'. Verify the name (use data_kb_schema with no table to list all).",
            name
        );
    }

    let purpose = kb_schema::table_meta()
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, p)| (*p).to_string())
        .unwrap_or_default();
    let notes = kb_schema::table_notes(name).unwrap_or("").to_string();

    let mut columns = Vec::new();
    {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({})", quote_ident(name)))
            .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] PRAGMA table_info failed: {}", e))?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        for row in rows.flatten() {
            columns.push(json!({ "name": row.0, "type": row.1, "pk": row.2 == 1 }));
        }
    }

    let mut samples: Vec<Value> = Vec::new();
    if !columns.is_empty() {
        if let Ok(mut stmt) = conn.prepare(&format!("SELECT * FROM {} LIMIT 3", quote_ident(name)))
        {
            let col_names: Vec<String> = (0..stmt.column_count())
                .map(|i| stmt.column_name(i).unwrap_or("").to_string())
                .collect();
            if let Ok(mut rows) = stmt.query([]) {
                while let Ok(Some(row)) = rows.next() {
                    let mut rec: Map<String, Value> = Map::new();
                    for (i, col) in col_names.iter().enumerate() {
                        rec.insert(
                            col.clone(),
                            match row.get_ref(i) {
                                Ok(rusqlite::types::ValueRef::Null) => Value::Null,
                                Ok(rusqlite::types::ValueRef::Integer(n)) => json!(n),
                                Ok(rusqlite::types::ValueRef::Real(f)) => json!(f),
                                Ok(rusqlite::types::ValueRef::Text(t)) => {
                                    json!(String::from_utf8_lossy(t))
                                }
                                Ok(rusqlite::types::ValueRef::Blob(b)) => {
                                    json!(format!("<blob {} bytes>", b.len()))
                                }
                                Err(_) => Value::Null,
                            },
                        );
                    }
                    samples.push(Value::Object(rec));
                }
            }
        }
    }

    Ok(json!({
        "status": "ok",
        "table": name,
        "purpose": purpose,
        "notes": notes,
        "columns": columns,
        "samples": samples
    }))
}

// ---------------------------------------------------------------------------
// data_kb_insert
// ---------------------------------------------------------------------------

/// Build the `data_kb_insert` tool definition.
pub(crate) fn build_kb_insert_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "data_kb_insert",
            "description": "Insert extracted knowledge into the Knowledge Base. All items in one call are inserted atomically in a single transaction. Each item needs no id: new UUIDs are generated and returned. Use \"ref\"/\"*_ref\" local references to link items created in the same call. Every item belongs to the document given by document_id (for cross-document relations, pass \"null\" as document_id). Corrections follow the obsolete model: insert the replacement row, then mark the old row obsolete via data_kb_update.",
            "parameters": {
                "type": "object",
                "properties": {
                    "document_id": {
                        "type": "string",
                        "description": "UUID of the registered document (documents.id). Get it from the /kb add result or by querying documents via data_kb_search. Required. For cross-document relations pass \"null\"."
                    },
                    "entities": {
                        "type": "array",
                        "description": "Entities to insert. Item fields: name (required), entity_type (optional, e.g. organization/person/product/regulation), attributes (optional object), ref (optional local reference, e.g. \"e1\").",
                        "items": { "type": "object" }
                    },
                    "claims": {
                        "type": "array",
                        "description": "Claims/facts to insert. Item fields: predicate (required), subject (required: subject_id UUID, subject_ref local ref, or subject_value object for unextracted subjects), object (optional, same three forms), modality (optional, default assertion: fact/assertion/hypothesis/prediction/possibility/requirement/recommendation/opinion), confidence (optional 0..1), attributes (optional object), ref (optional).",
                        "items": { "type": "object" }
                    },
                    "relations": {
                        "type": "array",
                        "description": "Relations between knowledge items. Item fields: relation_type (required, e.g. causes/depends_on/contradicts/supports/precedes/qualifies), source_type + source_id (or source_ref), target_type + target_id (or target_ref), confidence (optional), attributes (optional). source_type/target_type: entity/claim/event/document_unit.",
                        "items": { "type": "object" }
                    },
                    "conditions": {
                        "type": "array",
                        "description": "Conditions attached to a claim (or rarely an event). Item fields: target_type (required, claim/event) + target_id (or target_ref), condition_type (required: prerequisite/exception/threshold/temporal/scope), expression (required object, e.g. {\"op\": \">=\", \"field\": \"amount\", \"value\": 1000000}).",
                        "items": { "type": "object" }
                    },
                    "events": {
                        "type": "array",
                        "description": "Events/timeline items. Item fields: event_type (optional), subject_id (or subject_ref, optional), start_time (optional text, ambiguous allowed e.g. \"around 2026\"), end_time (optional), sort_key (optional normalized key for sorting, e.g. \"2026-00-00\"), precision (optional: year/month/day).",
                        "items": { "type": "object" }
                    },
                    "evidence": {
                        "type": "array",
                        "description": "Provenance: which source passage supports a target item. Item fields: target_type (required: entity/claim/relation/event/condition) + target_id (or target_ref), source_unit_id (or source_unit_ref, optional - the document_units.id backing this item), start_offset/end_offset (optional character offsets into the unit text), matched_text (optional excerpt snapshot), evidence_type (optional).",
                        "items": { "type": "object" }
                    }
                },
                "required": ["document_id"]
            }
        }
    })
}

/// Insert extracted knowledge. All items in one call = one IMMEDIATE
/// transaction. Local `ref`/`*_ref` resolution happens before any INSERT.
pub(crate) fn execute_kb_insert(ctx: &KbContext, args: &Value) -> Result<Value> {
    let doc_id = args
        .get("document_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] Missing required 'document_id'."))?
        .to_string();
    let is_cross_doc = doc_id == "null";

    // Quotas.
    let body_size = serde_json::to_vec(args)
        .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?
        .len();
    if body_size > KB_INSERT_MAX_BYTES {
        bail!(
            "[KB_QUOTA_EXCEEDED] Too many items in one call (limit: {} items / 1MB). Split the insert into smaller calls.",
            KB_INSERT_MAX_ITEMS
        );
    }
    let total_items = [
        "entities",
        "claims",
        "relations",
        "conditions",
        "events",
        "evidence",
    ]
    .iter()
    .map(|k| {
        args.get(*k)
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    })
    .sum::<usize>();
    if total_items > KB_INSERT_MAX_ITEMS {
        bail!(
            "[KB_QUOTA_EXCEEDED] Too many items in one call (limit: {} items / 1MB). Split the insert into smaller calls.",
            KB_INSERT_MAX_ITEMS
        );
    }

    // Pass 1: assign ids and collect local refs (duplicate ref -> conflict).
    let mut refs: HashMap<String, (String, String)> = HashMap::new();
    let mut assigned: HashMap<String, Vec<(String, Value)>> = HashMap::new();
    const KINDS: [&str; 6] = [
        "entities",
        "claims",
        "relations",
        "conditions",
        "events",
        "evidence",
    ];
    for kind in KINDS {
        let Some(items) = args.get(kind).and_then(|v| v.as_array()) else {
            continue;
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let id = Uuid::new_v4().to_string();
            if let Some(r) = item.get("ref").and_then(|v| v.as_str()) {
                if refs.contains_key(r) {
                    bail!(
                        "[KB_CONFLICT] Conflicting reference: duplicate ref '{}'.",
                        r
                    );
                }
                refs.insert(r.to_string(), (kind.to_string(), id.clone()));
            }
            out.push((id.clone(), item.clone()));
        }
        assigned.insert(kind.to_string(), out);
    }

    let mut conn = lock_conn(ctx)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Failed to start transaction: {}", e))?;

    let mut inserted: Map<String, Value> = Map::new();
    let mut total = 0usize;

    for kind in KINDS {
        let items = assigned.remove(kind).unwrap_or_default();
        let mut ids: Vec<Value> = Vec::with_capacity(items.len());
        for (id, item) in items {
            insert_item(
                &tx,
                kind,
                &id,
                &item,
                &doc_id,
                is_cross_doc,
                &ctx.run_id,
                &refs,
            )?;
            let ref_name = item.get("ref").and_then(|v| v.as_str()).unwrap_or("");
            ids.push(json!({
                "ref": if ref_name.is_empty() { Value::Null } else { Value::String(ref_name.to_string()) },
                "id": id
            }));
            total += 1;
        }
        if !ids.is_empty() {
            inserted.insert(kind.to_string(), Value::Array(ids));
        }
    }

    tx.commit()
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Transaction commit failed: {}", e))?;

    Ok(json!({ "status": "ok", "inserted": inserted, "total": total }))
}

/// Insert a single knowledge item (dispatch by kind).
#[allow(clippy::too_many_arguments)]
fn insert_item(
    tx: &Transaction,
    kind: &str,
    id: &str,
    item: &Value,
    doc_id: &str,
    is_cross_doc: bool,
    run_id: &Uuid,
    refs: &HashMap<String, (String, String)>,
) -> Result<()> {
    let run = run_id.to_string();
    match kind {
        "entities" => {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] entity.name is required."))?;
            let entity_type = item.get("entity_type").and_then(|v| v.as_str());
            let attributes = json_to_text(item.get("attributes")).unwrap_or_else(|| "{}".into());
            tx.execute(
                "INSERT INTO entities (id, document_id, entity_type, name, name_norm, attributes, annotations, analysis_run_id, obsolete) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, '{}', ?7, 0)",
                params![id, doc_id, entity_type, name, nfkc(name), attributes, run],
            )
            .map_err(sql_insert_err)?;
        }
        "claims" => {
            let predicate = item
                .get("predicate")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] claim.predicate is required."))?;
            let subject_id = resolve_id_ref(item, "subject_id", "subject_ref", refs)?;
            let subject_value = if subject_id.is_none() {
                json_to_text(item.get("subject_value"))
            } else {
                None
            };
            if subject_id.is_none() && subject_value.is_none() {
                return Err(anyhow!(
                    "[KB_MISSING_FIELDS] claim.subject is required: subject_id, subject_ref, or subject_value."
                ));
            }
            let object_id = resolve_id_ref(item, "object_id", "object_ref", refs)?;
            let object_value = if object_id.is_none() {
                json_to_text(item.get("object_value"))
            } else {
                None
            };
            let modality = item
                .get("modality")
                .and_then(|v| v.as_str())
                .unwrap_or("assertion");
            let polarity = item
                .get("polarity")
                .and_then(|v| v.as_str())
                .unwrap_or("positive");
            let confidence = item.get("confidence").and_then(|v| v.as_f64());
            let attributes = json_to_text(item.get("attributes")).unwrap_or_else(|| "{}".into());
            let fingerprint = build_claim_fingerprint(
                subject_id.as_deref().unwrap_or(""),
                subject_value.as_deref().unwrap_or(""),
                predicate,
                object_id.as_deref().unwrap_or(""),
                object_value.as_deref().unwrap_or(""),
                modality,
            );
            tx.execute(
                "INSERT INTO claims (id, document_id, subject_id, subject_value, predicate, object_id, object_value, modality, polarity, fingerprint, confidence, attributes, annotations, analysis_run_id, obsolete) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, '{}', ?13, 0)",
                params![
                    id, doc_id, subject_id, subject_value, predicate, object_id, object_value,
                    modality, polarity, fingerprint, confidence, attributes, run
                ],
            )
            .map_err(sql_insert_err)?;
        }
        "relations" => {
            let relation_type = item
                .get("relation_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow!("[KB_MISSING_FIELDS] relation.relation_type is required.")
                })?;
            let source_type = item
                .get("source_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] relation.source_type is required."))?;
            let source_id =
                resolve_id_ref(item, "source_id", "source_ref", refs)?.ok_or_else(|| {
                    anyhow!("[KB_MISSING_FIELDS] relation.source_id / source_ref is required.")
                })?;
            let target_type = item
                .get("target_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] relation.target_type is required."))?;
            let target_id =
                resolve_id_ref(item, "target_id", "target_ref", refs)?.ok_or_else(|| {
                    anyhow!("[KB_MISSING_FIELDS] relation.target_id / target_ref is required.")
                })?;
            let confidence = item.get("confidence").and_then(|v| v.as_f64());
            let attributes = json_to_text(item.get("attributes")).unwrap_or_else(|| "{}".into());
            let document_col: Option<&str> = if is_cross_doc { None } else { Some(doc_id) };
            tx.execute(
                "INSERT INTO relations (id, document_id, source_type, source_id, relation_type, target_type, target_id, confidence, attributes, annotations, analysis_run_id, obsolete) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '{}', ?10, 0)",
                params![
                    id, document_col, source_type, source_id, relation_type, target_type,
                    target_id, confidence, attributes, run
                ],
            )
            .map_err(sql_insert_err)?;
        }
        "conditions" => {
            let target_type = item
                .get("target_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow!("[KB_MISSING_FIELDS] condition.target_type is required (claim/event).")
                })?;
            if target_type != "claim" && target_type != "event" {
                bail!("[KB_EXEC_ERROR] condition.target_type must be 'claim' or 'event'.");
            }
            let target_id =
                resolve_id_ref(item, "target_id", "target_ref", refs)?.ok_or_else(|| {
                    anyhow!("[KB_MISSING_FIELDS] condition.target_id / target_ref is required.")
                })?;
            let condition_type = item
                .get("condition_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow!("[KB_MISSING_FIELDS] condition.condition_type is required.")
                })?;
            let expression = json_to_text(item.get("expression"))
                .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] condition.expression is required."))?;
            // document_id auto-completed from the target (spec).
            let doc: Option<String> = tx
                .query_row(
                    "SELECT document_id FROM claims WHERE id = ?1 \
                     UNION SELECT document_id FROM events WHERE id = ?1",
                    [&target_id],
                    |r| r.get(0),
                )
                .ok();
            let doc = doc.filter(|d| !d.is_empty());
            tx.execute(
                "INSERT INTO conditions (id, document_id, target_type, target_id, condition_type, expression, annotations, analysis_run_id, obsolete) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, '{}', ?7, 0)",
                params![id, doc, target_type, target_id, condition_type, expression, run],
            )
            .map_err(sql_insert_err)?;
        }
        "events" => {
            let event_type = item.get("event_type").and_then(|v| v.as_str());
            let subject_id = resolve_id_ref(item, "subject_id", "subject_ref", refs)?;
            let start_time = item.get("start_time").and_then(|v| v.as_str());
            let end_time = item.get("end_time").and_then(|v| v.as_str());
            let sort_key = item.get("sort_key").and_then(|v| v.as_str());
            let precision = item
                .get("precision")
                .and_then(|v| v.as_str())
                .unwrap_or("day");
            let attributes = json_to_text(item.get("attributes")).unwrap_or_else(|| "{}".into());
            tx.execute(
                "INSERT INTO events (id, document_id, event_type, subject_id, start_time, end_time, sort_key, precision, attributes, annotations, analysis_run_id, obsolete) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '{}', ?10, 0)",
                params![
                    id, doc_id, event_type, subject_id, start_time, end_time, sort_key, precision,
                    attributes, run
                ],
            )
            .map_err(sql_insert_err)?;
        }
        "evidence" => {
            let target_type = item
                .get("target_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] evidence.target_type is required."))?;
            let target_id =
                resolve_id_ref(item, "target_id", "target_ref", refs)?.ok_or_else(|| {
                    anyhow!("[KB_MISSING_FIELDS] evidence.target_id / target_ref is required.")
                })?;
            let source_unit_id = resolve_id_ref(item, "source_unit_id", "source_unit_ref", refs)?;
            let start_offset = item.get("start_offset").and_then(|v| v.as_i64());
            let end_offset = item.get("end_offset").and_then(|v| v.as_i64());
            let matched_text = item.get("matched_text").and_then(|v| v.as_str());
            let evidence_type = item.get("evidence_type").and_then(|v| v.as_str());
            tx.execute(
                "INSERT INTO evidence (id, document_id, source_unit_id, target_type, target_id, start_offset, end_offset, matched_text, evidence_type, analysis_run_id, obsolete) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)",
                params![
                    id, doc_id, source_unit_id, target_type, target_id, start_offset, end_offset,
                    matched_text, evidence_type, run
                ],
            )
            .map_err(sql_insert_err)?;
        }
        _ => unreachable!("validated item kind"),
    }
    Ok(())
}

fn sql_insert_err(e: rusqlite::Error) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("FOREIGN KEY constraint failed") || msg.contains("constraint failed") {
        anyhow!(
            "[KB_REF_NOT_FOUND] Insert failed (constraint): {}. Verify referenced ids exist \
             (query data_kb_search to find existing ids).",
            msg
        )
    } else {
        anyhow!("[KB_EXEC_ERROR] Insert failed: {}", msg)
    }
}

/// claims.fingerprint = sha256(normalized subject/predicate/object/modality),
/// first 16 hex chars (spec: derived column, auto-generated, no UNIQUE).
fn build_claim_fingerprint(
    subject_id: &str,
    subject_value: &str,
    predicate: &str,
    object_id: &str,
    object_value: &str,
    modality: &str,
) -> String {
    let subject = if subject_id.is_empty() {
        subject_value
    } else {
        subject_id
    };
    let object = if object_id.is_empty() {
        object_value
    } else {
        object_id
    };
    let norm = format!(
        "{}|{}|{}|{}",
        normalize_ws(&nfkc(subject)),
        normalize_ws(&nfkc(predicate)),
        normalize_ws(&nfkc(object)),
        normalize_ws(&nfkc(modality))
    );
    sha256_hex(&norm)[..16].to_string()
}

// ---------------------------------------------------------------------------
// data_kb_update
// ---------------------------------------------------------------------------

/// Build the `data_kb_update` tool definition.
pub(crate) fn build_kb_update_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "data_kb_update",
            "description": "Update existing Knowledge Base rows: merge attributes, or append annotations (versioned history is recorded automatically). Use this for analysis results: evaluations, classifications, uncertainty, missing-information findings. Never erase a row; correct by updating annotations (or adding a new row and marking the old one obsolete).",
            "parameters": {
                "type": "object",
                "properties": {
                    "target_type": {
                        "type": "string",
                        "description": "Row kind to update: documents / document_units / entities / claims / relations / conditions / events / evidence / canonical_entities."
                    },
                    "target_id": {
                        "type": "string",
                        "description": "UUID of the row to update."
                    },
                    "attributes": {
                        "type": "object",
                        "description": "Optional. Merged into the knowledge model attributes (existing keys overwritten; a key with null removes it)."
                    },
                    "annotations": {
                        "type": "object",
                        "description": "Optional. Merged into the analysis annotations; the previous value is kept in annotation_versions and the current value is the latest version. Reserved key on documents: \"analysis_status\" (pending/analyzing/analyzed/failed) transitions the document status; transitioning to analyzed also stamps analyzed_at."
                    },
                    "obsolete": {
                        "type": "boolean",
                        "description": "Optional. Set true to mark the row superseded on any knowledge table (replaced by a newer extraction). All knowledge tables support it: document_units / entities / claims / relations / conditions / events / evidence / canonical_entities."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional short reason for the change; recorded in annotation_versions.reason."
                    }
                },
                "required": ["target_type", "target_id"]
            }
        }
    })
}

fn has_attributes_column(target_type: &str) -> bool {
    matches!(
        target_type,
        "entities" | "claims" | "relations" | "events" | "canonical_entities"
    )
}

fn has_obsolete_column(target_type: &str) -> bool {
    matches!(
        target_type,
        "document_units"
            | "entities"
            | "claims"
            | "relations"
            | "conditions"
            | "events"
            | "evidence"
            | "canonical_entities"
    )
}

/// Update attributes / annotations (versioned) / obsolete on one row.
/// One call = one IMMEDIATE transaction; the current value always matches the
/// latest annotation_versions entry.
pub(crate) fn execute_kb_update(ctx: &KbContext, args: &Value) -> Result<Value> {
    let target_type = args
        .get("target_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] Missing required 'target_type'."))?;
    if !kb_schema::UPDATE_TARGET_TYPES.contains(&target_type) {
        bail!(
            "[KB_EXEC_ERROR] Invalid target_type '{}'. Allowed: {}.",
            target_type,
            kb_schema::UPDATE_TARGET_TYPES.join(" / ")
        );
    }
    let target_id = args
        .get("target_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("[KB_MISSING_FIELDS] Missing required 'target_id'."))?
        .to_string();
    let attributes = args.get("attributes").cloned().filter(|v| !v.is_null());
    let annotations = args.get("annotations").cloned().filter(|v| !v.is_null());
    let obsolete = args
        .get("obsolete")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .map(String::from);
    if attributes.is_none() && annotations.is_none() && !obsolete {
        bail!(
            "[KB_MISSING_FIELDS] No update content: provide attributes, annotations, or obsolete."
        );
    }

    let mut conn = lock_conn(ctx)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Failed to start transaction: {}", e))?;

    // Load the current row generically (columns differ per table).
    let mut cur_attrs = "{}".to_string();
    let mut cur_annos = "{}".to_string();
    let mut doc_id: Option<String> = None;
    {
        let mut stmt = tx
            .prepare(&format!(
                "SELECT * FROM {} WHERE id = ?1",
                quote_ident(target_type)
            ))
            .map_err(|_| {
                anyhow!(
                    "[KB_NOT_FOUND] Target not found: {} '{}'. Verify the id via data_kb_search.",
                    target_type,
                    target_id
                )
            })?;
        let cols: Vec<String> = (0..stmt.column_count())
            .map(|i| stmt.column_name(i).unwrap_or("").to_string())
            .collect();
        let mut rows = stmt
            .query([&target_id])
            .map_err(|_| anyhow!("[KB_INTERNAL_ERROR] query failed"))?;
        let row = rows.next()?.ok_or_else(|| {
            anyhow!(
                "[KB_NOT_FOUND] Target not found: {} '{}'. Verify the id via data_kb_search.",
                target_type,
                target_id
            )
        })?;
        for (i, col) in cols.iter().enumerate() {
            if col == "attributes" {
                if let Ok(v) = row.get::<_, String>(i) {
                    cur_attrs = v;
                }
            } else if col == "annotations" {
                if let Ok(v) = row.get::<_, String>(i) {
                    cur_annos = v;
                }
            } else if col == "document_id" {
                if let Ok(v) = row.get::<_, Option<String>>(i) {
                    doc_id = v;
                }
            }
        }
    }

    let mut attrs_obj = parse_json_obj(&cur_attrs)?;
    if let Some(a) = &attributes {
        merge_json_obj(&mut attrs_obj, a)?;
    }
    let new_attrs = serde_json::to_string(&attrs_obj)?;

    let mut anno_obj = parse_json_obj(&cur_annos)?;
    if let Some(a) = &annotations {
        merge_json_obj(&mut anno_obj, a)?;
    }
    if obsolete {
        anno_obj.insert("obsolete".to_string(), Value::Bool(true));
    }
    let new_annos = serde_json::to_string(&anno_obj)?;

    // Versioned history: max(version) + 1 for this target.
    let version: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM annotation_versions \
             WHERE target_type = ?1 AND target_id = ?2",
            params![target_type, target_id],
            |r| r.get(0),
        )
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Failed to read annotation version: {}", e))?;
    tx.execute(
        "INSERT INTO annotation_versions (id, target_type, target_id, version, annotations, analysis_run_id, created_by, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            Uuid::new_v4().to_string(),
            target_type,
            target_id,
            version,
            new_annos,
            ctx.run_id.to_string(),
            "data_kb_update",
            reason
        ],
    )
    .map_err(|e| anyhow!("[KB_EXEC_ERROR] Failed to append annotation version: {}", e))?;

    // Build the UPDATE (set params first, WHERE id last).
    let mut sets: Vec<String> = Vec::new();
    let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if has_attributes_column(target_type) {
        sets.push("attributes = ?".to_string());
        vals.push(Box::new(new_attrs));
    }
    sets.push("annotations = ?".to_string());
    vals.push(Box::new(new_annos));
    if obsolete && has_obsolete_column(target_type) {
        sets.push("obsolete = 1".to_string());
    }
    if target_type == "documents" {
        if let Some(status) = anno_obj.get("analysis_status").and_then(|v| v.as_str()) {
            if !matches!(status, "pending" | "analyzing" | "analyzed" | "failed") {
                bail!(
                    "[KB_EXEC_ERROR] Invalid analysis_status '{}': allowed values are \
                     pending / analyzing / analyzed / failed.",
                    status
                );
            }
            sets.push("analysis_status = ?".to_string());
            vals.push(Box::new(status.to_string()));
            if status == "analyzed" {
                sets.push("analyzed_at = ?".to_string());
                vals.push(Box::new(now_iso()));
            }
        }
    }
    if sets.is_empty() {
        bail!(
            "[KB_MISSING_FIELDS] No update content: provide attributes, annotations, or obsolete."
        );
    }
    let sql = format!(
        "UPDATE {} SET {} WHERE id = ?{}",
        quote_ident(target_type),
        sets.join(", "),
        vals.len() + 1
    );
    vals.push(Box::new(target_id.clone()));
    tx.execute(&sql, params_from_iter(vals.iter().map(|b| b.as_ref())))
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Update failed: {}", e))?;

    // Parent documents.updated_at refresh (conditions / canonical_entities
    // excluded per spec).
    let doc_updated = if target_type == "documents" {
        let ts = now_iso();
        tx.execute(
            "UPDATE documents SET updated_at = ?1 WHERE id = ?2",
            params![ts, target_id],
        )?;
        ts
    } else if let Some(parent) = doc_id {
        if target_type != "conditions" {
            let ts = now_iso();
            tx.execute(
                "UPDATE documents SET updated_at = ?1 WHERE id = ?2",
                params![ts, parent],
            )?;
            ts
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    tx.commit()
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Transaction commit failed: {}", e))?;

    Ok(json!({
        "status": "ok",
        "version": version,
        "document_updated_at": doc_updated
    }))
}

// ---------------------------------------------------------------------------
// /kb command shared operations
// ---------------------------------------------------------------------------

/// Data file root of the KB directory.
fn data_dir(kb_dir: &str) -> PathBuf {
    Path::new(kb_dir).join("data")
}

/// Build a `data/...`-relative source path for a file under `kb_dir`.
fn relative_source(root: &Path, file: &Path) -> Result<String> {
    let rel = file.strip_prefix(root).map_err(|_| {
        anyhow!(
            "[KB_FILE_ERROR] File is outside the KB root: '{}'",
            file.display()
        )
    })?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

/// `/kb add <path>`: register a file (copy into data/ when outside) and run
/// machine extraction. Returns a human-readable summary.
pub(crate) fn kb_add(ctx: &KbContext, path_arg: &str) -> Result<String> {
    let root = Path::new(&ctx.kb_dir);
    let data = data_dir(&ctx.kb_dir);
    fs::create_dir_all(&data)
        .map_err(|e| anyhow!("[KB_FILE_ERROR] Failed to create data dir: {}", e))?;

    let arg = PathBuf::from(path_arg);
    let file_path = if arg.is_absolute() {
        copy_into_data(&data, &arg, path_arg)?
    } else {
        // Relative: prefer a file already under data/; otherwise resolve
        // against the workspace (CWD) and copy it in (spec: files outside
        // data/ are copied into data/ first).
        let under_data = data.join(&arg);
        if under_data.exists() {
            under_data
        } else if arg.exists() {
            copy_into_data(&data, &arg, path_arg)?
        } else if let Some(name) = arg.file_name() {
            let by_name = data.join(name);
            if by_name.exists() {
                by_name
            } else {
                bail!(
                    "[KB_FILE_ERROR] File not found in KB data/ or workspace: '{}'.",
                    path_arg
                );
            }
        } else {
            bail!("[KB_FILE_ERROR] Invalid path: '{}'", path_arg);
        }
    };

    let rel_source = relative_source(root, &file_path)?;
    register_file(ctx, &file_path, &rel_source)
}

/// Copy `src` into `data/` (name collision -> KB_FILE_ERROR); returns the dest.
fn copy_into_data(data: &Path, src: &Path, path_arg: &str) -> Result<PathBuf> {
    if !src.exists() {
        bail!("[KB_FILE_ERROR] File not found: '{}'.", path_arg);
    }
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("[KB_FILE_ERROR] Invalid path: '{}'", path_arg))?;
    let dest = data.join(name);
    if dest.exists() {
        bail!(
            "[KB_FILE_ERROR] File '{}' already exists in {}. Register it or use a different name.",
            name,
            data.display()
        );
    }
    fs::copy(src, &dest).map_err(|e| {
        anyhow!(
            "[KB_FILE_ERROR] Failed to copy '{}' into {}: {}",
            path_arg,
            data.display(),
            e
        )
    })?;
    Ok(dest)
}

/// `/kb list`: all versions summary.
pub(crate) fn kb_list(ctx: &KbContext) -> Result<String> {
    let conn = lock_conn(ctx)?;
    let mut stmt = conn
        .prepare(
            "SELECT source, version, superseded_by, analysis_status, created_at, updated_at, metadata \
             FROM documents ORDER BY source, version",
        )
        .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })
        .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?;
    let mut lines = vec![format!(
        "{:<32} {:<12} {:<10} {:<12} {:<28} {:<28} missing",
        "source", "version", "status", "analysis", "created_at", "updated_at"
    )];
    let mut pending: Vec<String> = Vec::new();
    for row in rows.flatten() {
        let (source, version, superseded, status, created, updated, metadata) = row;
        let version_label = format!(
            "v{}{}",
            version,
            if superseded.is_some() { "(old)" } else { "" }
        );
        let meta_obj = parse_json_obj(&metadata).unwrap_or_default();
        let missing = meta_obj
            .get("file_missing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let src = source.unwrap_or_default();
        if superseded.is_none() && status == "pending" {
            pending.push(src.clone());
        }
        lines.push(format!(
            "{:<32} {:<12} {:<10} {:<12} {:<28} {:<28} {}",
            src,
            version_label,
            if superseded.is_some() {
                "superseded"
            } else {
                "current"
            },
            status,
            created,
            updated,
            if missing { "[missing]" } else { "" }
        ));
    }
    if lines.len() == 1 {
        lines.push("(no documents registered)".to_string());
    }
    if !pending.is_empty() {
        lines.push(format!(
            "Hint: {} document(s) are pending analysis. Ask the AI to analyze them, then to set \
             analysis_status to analyzed via data_kb_update (a write tool; it asks y/N). /kb add / \
             /kb sync only extract paragraphs - they never run the LLM, so the status stays \
             pending until the AI writes the transition.",
            pending.len()
        ));
    }
    Ok(lines.join("\n"))
}

/// `/kb delete <path> [--all-versions]`: delete data file + DB rows (cascade),
/// clean up cross-document relations, and promote the previous version.
pub(crate) fn kb_delete(ctx: &KbContext, path_arg: &str, all_versions: bool) -> Result<String> {
    let root = Path::new(&ctx.kb_dir);
    let data = data_dir(&ctx.kb_dir);
    let arg = PathBuf::from(path_arg);
    // Normalize to a data/... source.
    let source = if arg.starts_with(&data) {
        relative_source(root, &arg)?
    } else {
        let name = arg
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("[KB_FILE_ERROR] Invalid path: '{}'", path_arg))?;
        if path_arg.starts_with("data/") {
            path_arg.to_string()
        } else {
            format!("data/{}", name)
        }
    };

    let mut conn = lock_conn(ctx)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] {}", e))?;

    let mut ids: Vec<String> = Vec::new();
    {
        let mut stmt = tx
            .prepare("SELECT id FROM documents WHERE source = ?1")
            .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?;
        let rows = stmt
            .query_map([&source], |r| r.get::<_, String>(0))
            .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?;
        for row in rows.flatten() {
            ids.push(row);
        }
    }
    if ids.is_empty() {
        bail!(
            "[KB_FILE_ERROR] No registered document for '{}'. Use /kb list to see sources.",
            source
        );
    }

    let current_id: Option<String> = tx
        .query_row(
            "SELECT id FROM documents WHERE source = ?1 AND superseded_by IS NULL",
            [&source],
            |r| r.get(0),
        )
        .ok();

    // Collect every knowledge row of the deleted document(s) so cross-document
    // relations whose endpoint disappears can be removed (spec: endpoint scan).
    let mut endpoint_ids: Vec<String> = Vec::new();
    if let Ok(mut stmt) = tx.prepare(
        "SELECT id FROM entities WHERE document_id IN (SELECT id FROM documents WHERE source = ?1) \
         UNION SELECT id FROM claims WHERE document_id IN (SELECT id FROM documents WHERE source = ?1) \
         UNION SELECT id FROM conditions WHERE document_id IN (SELECT id FROM documents WHERE source = ?1) \
         UNION SELECT id FROM events WHERE document_id IN (SELECT id FROM documents WHERE source = ?1) \
         UNION SELECT id FROM document_units WHERE document_id IN (SELECT id FROM documents WHERE source = ?1)",
    ) {
        let rows = stmt
            .query_map([&source], |r| r.get::<_, String>(0))
            .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?;
        for row in rows.flatten() {
            endpoint_ids.push(row);
        }
    }

    let deleted = if all_versions {
        tx.execute("DELETE FROM documents WHERE source = ?1", [&source])
            .map_err(|e| anyhow!("[KB_EXEC_ERROR] Delete failed: {}", e))?
    } else {
        let Some(cur) = current_id.as_deref() else {
            bail!(
                "[KB_FILE_ERROR] No current version for '{}'. Use --all-versions to delete old versions.",
                source
            );
        };
        // Promote the immediate predecessor (the row pointing at this one).
        tx.execute(
            "UPDATE documents SET superseded_by = NULL WHERE superseded_by = ?1",
            [cur],
        )
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Promote failed: {}", e))?;
        tx.execute("DELETE FROM documents WHERE id = ?1", [cur])
            .map_err(|e| anyhow!("[KB_EXEC_ERROR] Delete failed: {}", e))?
    };

    // Cross-document relations referencing a deleted endpoint are removed.
    if !endpoint_ids.is_empty() {
        let placeholders = vec!["?"; endpoint_ids.len()].join(",");
        tx.execute(
            &format!(
                "DELETE FROM relations WHERE document_id IS NULL \
                 AND (source_id IN ({}) OR target_id IN ({}))",
                placeholders, placeholders
            ),
            params_from_iter(
                endpoint_ids
                    .iter()
                    .chain(endpoint_ids.iter())
                    .map(|s| s.as_str()),
            ),
        )
        .map_err(|e| {
            anyhow!(
                "[KB_EXEC_ERROR] Cross-document relation cleanup failed: {}",
                e
            )
        })?;
    }

    tx.commit()
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Commit failed: {}", e))?;

    // Delete the data file (ignore missing file).
    let file = data.join(&source["data/".len()..]);
    if file.exists() {
        fs::remove_file(&file).map_err(|e| {
            anyhow!(
                "[KB_FILE_ERROR] Failed to delete '{}': {}",
                file.display(),
                e
            )
        })?;
    }

    Ok(format!(
        "Deleted {} version(s) of '{}' ({}) and its data file.",
        deleted,
        source,
        if all_versions {
            "all versions"
        } else {
            "current"
        }
    ))
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir)
        .map_err(|e| anyhow!("[KB_FILE_ERROR] Failed to read '{}': {}", dir.display(), e))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }
            collect_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// `/kb sync`: rescan data/, register new files, version-update changed ones,
/// mark missing files, and report polymorphic orphan references.
pub(crate) fn kb_sync(ctx: &KbContext) -> Result<String> {
    let data = data_dir(&ctx.kb_dir);
    if !data.exists() {
        fs::create_dir_all(&data)
            .map_err(|e| anyhow!("[KB_FILE_ERROR] Failed to create data dir: {}", e))?;
    }
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut unchanged = 0usize;
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(&data, &mut files)?;
    for f in files {
        let rel = relative_source(Path::new(&ctx.kb_dir), &f)?;
        let conn = lock_conn(ctx)?;
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT id, metadata FROM documents \
                 WHERE source = ?1 AND superseded_by IS NULL",
                [&rel],
                |r| Ok((r.get(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        drop(conn);

        let meta = existing.as_ref().and_then(|(_, m)| parse_json_obj(m).ok());
        let size = fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        let mtime = fs::metadata(&f)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let changed = meta.map(|m| {
            m.get("size").and_then(|v| v.as_u64()).unwrap_or(0) != size
                || m.get("mtime").and_then(|v| v.as_u64()).unwrap_or(0) != mtime
        });
        match (&existing, changed) {
            (None, _) => {
                if register_file(ctx, &f, &rel)?.contains("registered") {
                    added += 1;
                }
            }
            (Some(_), Some(true)) => {
                if register_file(ctx, &f, &rel)?.contains("updated") {
                    updated += 1;
                } else {
                    unchanged += 1;
                    update_doc_metadata(ctx, &rel, size, mtime)?;
                }
            }
            (Some(_), _) => {
                unchanged += 1;
                update_doc_metadata(ctx, &rel, size, mtime)?;
            }
        }
    }

    // Missing detection: mark / unmark every registered source.
    let conn = lock_conn(ctx)?;
    let sources: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT source FROM documents")
            .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };
    drop(conn);
    for src in sources {
        let file = Path::new(&ctx.kb_dir).join(&src);
        mark_missing(ctx, &src, !file.exists())?;
    }

    let report = orphan_report(ctx)?;
    Ok(format!(
        "KB sync done: added {}, updated {}, unchanged {}.{}",
        added, updated, unchanged, report
    ))
}

/// Set / clear the `file_missing` flag in documents.metadata for a source.
fn mark_missing(ctx: &KbContext, source: &str, missing: bool) -> Result<()> {
    let mut conn = lock_conn(ctx)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] {}", e))?;
    let rows: Vec<(String, String)> = {
        let mut stmt = tx
            .prepare("SELECT id, metadata FROM documents WHERE source = ?1")
            .map_err(|e| anyhow!("[KB_INTERNAL_ERROR] {}", e))?;
        let iter = stmt.query_map([source], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        iter.flatten().collect()
    };
    let ts = now_iso();
    for (id, metadata) in rows {
        let mut meta = parse_json_obj(&metadata)?;
        if missing {
            meta.insert("file_missing".to_string(), Value::Bool(true));
            meta.insert("missing_at".to_string(), Value::String(ts.clone()));
        } else {
            meta.remove("file_missing");
            meta.remove("missing_at");
        }
        let new_meta = serde_json::to_string(&meta)?;
        tx.execute(
            "UPDATE documents SET metadata = ?1 WHERE id = ?2",
            params![new_meta, id],
        )
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Failed to update metadata: {}", e))?;
    }
    tx.commit()
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Commit failed: {}", e))?;
    Ok(())
}

/// Refresh size / mtime in documents.metadata of the current version for a source.
fn update_doc_metadata(ctx: &KbContext, rel: &str, size: u64, mtime: u64) -> Result<()> {
    let mut conn = lock_conn(ctx)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] {}", e))?;
    update_current_meta(&tx, rel, size, mtime)?;
    tx.commit()
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Commit failed: {}", e))?;
    Ok(())
}

/// Merge size / mtime into the metadata of the current version of `rel`.
fn update_current_meta(tx: &Transaction, rel: &str, size: u64, mtime: u64) -> Result<()> {
    if let Ok((id, metadata)) = tx.query_row(
        "SELECT id, metadata FROM documents WHERE source = ?1 AND superseded_by IS NULL",
        [rel],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    ) {
        let mut meta = parse_json_obj(&metadata)?;
        meta.insert("size".to_string(), json!(size));
        meta.insert("mtime".to_string(), json!(mtime));
        let new_meta = serde_json::to_string(&meta)?;
        tx.execute(
            "UPDATE documents SET metadata = ?1 WHERE id = ?2",
            params![new_meta, id],
        )
        .map_err(|e| anyhow!("[KB_EXEC_ERROR] Failed to update metadata: {}", e))?;
    }
    Ok(())
}

/// `/kb backup [path]`: `VACUUM INTO` snapshot (never VACUUM the live DB).
pub(crate) fn kb_backup(ctx: &KbContext, path_arg: Option<&str>) -> Result<String> {
    let dest = match path_arg {
        Some(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => {
            let backup_dir = Path::new(&ctx.kb_dir).join("db").join("backup");
            fs::create_dir_all(&backup_dir)
                .map_err(|e| anyhow!("[KB_FILE_ERROR] Failed to create backup dir: {}", e))?;
            backup_dir.join(format!(
                "library-{}.sqlite",
                chrono::Utc::now().format("%Y%m%d-%H%M%S")
            ))
        }
    };
    if dest.exists() {
        bail!(
            "[KB_FILE_ERROR] Backup destination already exists: '{}'.",
            dest.display()
        );
    }
    let conn = lock_conn(ctx)?;
    conn.execute_batch(&format!(
        "VACUUM INTO '{}'",
        escape_sql_string(&dest.to_string_lossy())
    ))
    .map_err(|e| anyhow!("[KB_FILE_ERROR] Backup failed (VACUUM INTO): {}", e))?;
    Ok(format!("Backup written to {}.", dest.display()))
}

/// Register a file: hash check -> insert / version-update -> machine
/// extraction. Returns a summary containing "registered" / "updated" /
/// "skipped".
fn register_file(ctx: &KbContext, file: &Path, rel: &str) -> Result<String> {
    let bytes = fs::read(file)
        .map_err(|e| anyhow!("[KB_FILE_ERROR] Failed to read '{}': {}", file.display(), e))?;
    let hash = sha256_bytes(&bytes);
    let title = file
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("untitled")
        .to_string();
    let document_type = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt")
        .to_lowercase();
    let size = bytes.len() as u64;
    let mtime = fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Tx 1: document row upsert (insert new or bump version + supersede).
    let (doc_id, version, kind) = {
        let mut conn = lock_conn(ctx)?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| anyhow!("[KB_EXEC_ERROR] {}", e))?;
        let existing: Option<(String, String, String)> = tx
            .query_row(
                "SELECT id, version, file_hash FROM documents \
                 WHERE source = ?1 AND superseded_by IS NULL",
                [rel],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();

        let result = match existing {
            Some((old_id, _, fh)) if fh == hash => {
                update_current_meta(&tx, rel, size, mtime)?;
                let v: String = tx
                    .query_row(
                        "SELECT version FROM documents WHERE id = ?1",
                        [&old_id],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| "1".to_string());
                (old_id, v, "skipped".to_string())
            }
            Some((old_id, old_version, _)) => {
                let next: i64 = old_version.parse::<i64>().unwrap_or(0) + 1;
                let new_id = Uuid::new_v4().to_string();
                let new_version = next.to_string();
                tx.execute(
                    "INSERT INTO documents (id, title, source, document_type, version, analysis_status, file_hash, metadata, annotations) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, '{}')",
                    params![
                        new_id,
                        title,
                        rel,
                        document_type,
                        new_version,
                        hash,
                        json!({ "size": size, "mtime": mtime }).to_string()
                    ],
                )
                .map_err(|e| anyhow!("[KB_EXEC_ERROR] Insert document failed: {}", e))?;
                tx.execute(
                    "UPDATE documents SET superseded_by = ?1 WHERE id = ?2",
                    params![new_id, old_id],
                )
                .map_err(|e| anyhow!("[KB_EXEC_ERROR] Supersede failed: {}", e))?;
                (new_id, new_version, "updated".to_string())
            }
            None => {
                let new_id = Uuid::new_v4().to_string();
                let new_version = "1".to_string();
                tx.execute(
                    "INSERT INTO documents (id, title, source, document_type, version, analysis_status, file_hash, metadata, annotations) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, '{}')",
                    params![
                        new_id,
                        title,
                        rel,
                        document_type,
                        new_version,
                        hash,
                        json!({ "size": size, "mtime": mtime }).to_string()
                    ],
                )
                .map_err(|e| anyhow!("[KB_EXEC_ERROR] Insert document failed: {}", e))?;
                (new_id, new_version, "registered".to_string())
            }
        };
        tx.commit()
            .map_err(|e| anyhow!("[KB_EXEC_ERROR] Commit failed: {}", e))?;
        result
    };

    // Tx 2: machine extraction (rollback units on failure; document kept).
    if kind == "skipped" {
        return Ok(format!(
            "skipped: {} (v{}, unchanged content)",
            rel, version
        ));
    }
    let mut conn = lock_conn(ctx)?;
    let status = {
        let tx2 = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| anyhow!("[KB_EXEC_ERROR] {}", e))?;
        match machine_extract(&tx2, &doc_id, file, &document_type) {
            Ok(()) => {
                tx2.commit()
                    .map_err(|e| anyhow!("[KB_EXEC_ERROR] Commit failed: {}", e))?;
                "pending".to_string()
            }
            Err(e) => {
                let msg = e.to_string();
                drop(tx2); // rollback partial units
                let tx3 = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|e| anyhow!("[KB_EXEC_ERROR] {}", e))?;
                tx3.execute(
                    "UPDATE documents SET analysis_status = 'failed' WHERE id = ?1",
                    [&doc_id],
                )
                .map_err(|e| anyhow!("[KB_EXEC_ERROR] {}", e))?;
                if let Ok((_, metadata)) = tx3.query_row(
                    "SELECT id, metadata FROM documents WHERE id = ?1",
                    [&doc_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                ) {
                    let mut meta = parse_json_obj(&metadata)?;
                    meta.insert("extraction_error".to_string(), Value::String(msg));
                    tx3.execute(
                        "UPDATE documents SET metadata = ?1 WHERE id = ?2",
                        params![serde_json::to_string(&meta)?, doc_id],
                    )
                    .map_err(|e| anyhow!("[KB_EXEC_ERROR] {}", e))?;
                }
                tx3.commit()
                    .map_err(|e| anyhow!("[KB_EXEC_ERROR] Commit failed: {}", e))?;
                "failed".to_string()
            }
        }
    };
    Ok(format!(
        "{}: {} (id {}, v{}, status {})",
        kind, rel, doc_id, version, status
    ))
}

/// Split text into trimmed, non-empty paragraphs (blank-line separated).
fn split_paragraphs(text: &str) -> Vec<String> {
    let norm = text.replace("\r\n", "\n");
    norm.split("\n\n")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Machine extraction of document_units (natural-key UPSERT, analysis_run_id
/// stays NULL = machine extraction). PDF: page units + paragraph children;
/// text: paragraph units.
fn machine_extract(tx: &Transaction, doc_id: &str, file: &Path, document_type: &str) -> Result<()> {
    let created = now_iso();
    let unit_meta = |page: Option<usize>| -> String {
        let mut src = serde_json::Map::new();
        src.insert("document_id".to_string(), Value::String(doc_id.to_string()));
        if let Some(p) = page {
            src.insert("page".to_string(), json!(p));
        }
        json!({
            "source": src,
            "extraction": {
                "method": "machine",
                "parser": if page.is_some() { "pdf-text" } else { "text-paragraph" },
                "version": KB_PARSER_VERSION,
                "created_at": created
            }
        })
        .to_string()
    };
    match document_type {
        "pdf" => {
            let path_str = file.to_string_lossy().to_string();
            let total = crate::file_pdf::pdf_page_count(&path_str)
                .map_err(|e| anyhow!("[KB_FILE_ERROR] {}", e))?;
            for p in 1..=total {
                let result = crate::file_pdf::extract_text_from_pdf(&path_str, Some((p, p)))
                    .map_err(|e| anyhow!("[KB_FILE_ERROR] {}", e))?;
                let page_id = upsert_unit(
                    tx,
                    doc_id,
                    "page",
                    p as i64,
                    None,
                    &result.text,
                    &unit_meta(Some(p)),
                )?;
                for (i, para) in split_paragraphs(&result.text).into_iter().enumerate() {
                    upsert_unit(
                        tx,
                        doc_id,
                        "paragraph",
                        i as i64,
                        Some(&page_id),
                        &para,
                        &unit_meta(Some(p)),
                    )?;
                }
            }
        }
        _ => {
            let raw = fs::read_to_string(file).map_err(|e| {
                anyhow!("[KB_FILE_ERROR] Failed to read '{}': {}", file.display(), e)
            })?;
            for (i, para) in split_paragraphs(&raw).into_iter().enumerate() {
                upsert_unit(
                    tx,
                    doc_id,
                    "paragraph",
                    i as i64,
                    None,
                    &para,
                    &unit_meta(None),
                )?;
            }
        }
    }
    Ok(())
}

/// Natural-key UPSERT of a machine-extracted unit; returns its id.
fn upsert_unit(
    tx: &Transaction,
    doc_id: &str,
    unit_type: &str,
    position: i64,
    parent_id: Option<&str>,
    text: &str,
    metadata: &str,
) -> Result<String> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT id FROM document_units WHERE document_id=?1 AND unit_type=?2 AND position=?3 \
             AND ((?4 IS NULL AND parent_id IS NULL) OR parent_id=?4)",
            params![doc_id, unit_type, position, parent_id],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO document_units (id, document_id, parent_id, unit_type, text, position, metadata, annotations, analysis_run_id, obsolete) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '{}', NULL, 0)",
        params![id, doc_id, parent_id, unit_type, text, position, metadata],
    )
    .map_err(|e| anyhow!("[KB_EXEC_ERROR] Unit insert failed: {}", e))?;
    Ok(id)
}

/// Polymorphic orphan reference report (repair is left to a later analysis run).
fn orphan_report(ctx: &KbContext) -> Result<String> {
    let conn = lock_conn(ctx)?;
    let checks: &[(&str, &str)] = &[
        (
            "relations.source_id orphan",
            "SELECT COUNT(*) FROM relations r WHERE NOT EXISTS (SELECT 1 FROM entities e WHERE e.id=r.source_id) AND NOT EXISTS (SELECT 1 FROM claims c WHERE c.id=r.source_id) AND NOT EXISTS (SELECT 1 FROM events ev WHERE ev.id=r.source_id) AND NOT EXISTS (SELECT 1 FROM document_units u WHERE u.id=r.source_id)",
        ),
        (
            "relations.target_id orphan",
            "SELECT COUNT(*) FROM relations r WHERE NOT EXISTS (SELECT 1 FROM entities e WHERE e.id=r.target_id) AND NOT EXISTS (SELECT 1 FROM claims c WHERE c.id=r.target_id) AND NOT EXISTS (SELECT 1 FROM events ev WHERE ev.id=r.target_id) AND NOT EXISTS (SELECT 1 FROM document_units u WHERE u.id=r.target_id)",
        ),
        (
            "conditions.target_id orphan",
            "SELECT COUNT(*) FROM conditions cd WHERE NOT EXISTS (SELECT 1 FROM claims c WHERE c.id=cd.target_id) AND NOT EXISTS (SELECT 1 FROM events ev WHERE ev.id=cd.target_id)",
        ),
        (
            "evidence.target_id orphan",
            "SELECT COUNT(*) FROM evidence ev WHERE NOT EXISTS (SELECT 1 FROM entities e WHERE e.id=ev.target_id) AND NOT EXISTS (SELECT 1 FROM claims c WHERE c.id=ev.target_id) AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.id=ev.target_id) AND NOT EXISTS (SELECT 1 FROM events evt WHERE evt.id=ev.target_id) AND NOT EXISTS (SELECT 1 FROM conditions cd WHERE cd.id=ev.target_id)",
        ),
        (
            "evidence.source_unit_id orphan",
            "SELECT COUNT(*) FROM evidence ev WHERE ev.source_unit_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM document_units u WHERE u.id=ev.source_unit_id)",
        ),
        (
            "claims.subject_id orphan",
            "SELECT COUNT(*) FROM claims c WHERE c.subject_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM entities e WHERE e.id=c.subject_id)",
        ),
        (
            "claims.object_id orphan",
            "SELECT COUNT(*) FROM claims c WHERE c.object_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM entities e WHERE e.id=c.object_id)",
        ),
        (
            "events.subject_id orphan",
            "SELECT COUNT(*) FROM events ev WHERE ev.subject_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM entities e WHERE e.id=ev.subject_id)",
        ),
    ];
    let mut out = String::new();
    for (label, sql) in checks {
        if let Ok(n) = conn.query_row(*sql, [], |r| r.get::<_, i64>(0)) {
            if n > 0 {
                out.push_str(&format!("\n  orphan {}: {}", label, n));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "tests/kb_test.rs"]
mod tests;
