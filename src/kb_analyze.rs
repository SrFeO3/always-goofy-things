//! KB extract enumeration (todo-refine Phase 2).
//!
//! Splits pending documents into byte-budget chunks and tracks them in
//! `analysis_chunks`. Execution (fresh sessions) lands in Phase 3.

use anyhow::Result;
use rusqlite::params;

use crate::kb::{KbContext, lock_conn};
use crate::startup;

/// Provisional per-chunk byte budget (D2: confirm by `--dry-run` metering).
pub(crate) const DEFAULT_CHUNK_BYTES: usize = 65536;

/// Rough chars-per-token divisor for the dry-run cost hint. Labeled rough.
const BYTES_PER_TOKEN: usize = 4;

/// Chunk row states.
pub(crate) const CHUNK_PENDING: &str = "pending";
pub(crate) const CHUNK_DONE: &str = "done";
pub(crate) const CHUNK_FAILED: &str = "failed";

/// One unit range handed to a single fresh session.
#[derive(Debug, Clone)]
pub(crate) struct ExtractChunk {
    pub document_id: String,
    pub document_label: String,
    pub unit_type: String,
    pub pos_from: i64,
    pub pos_to: i64,
    pub unit_count: usize,
    pub bytes_est: usize,
    pub unit_ids: Vec<String>,
}

/// Refuse to run extract/analyze writes without `--kb-auto-confirm rw`.
pub(crate) fn require_write_approval(config: &startup::Config) -> Result<()> {
    if matches!(config.kb_auto_confirm, startup::KbAutoConfirm::Rw) {
        return Ok(());
    }
    anyhow::bail!(
        "[KB_CONFIG_ERROR] Extract jobs write knowledge rows: rerun with \
         `--kb-auto-confirm rw` (current: {}). Reads stay available under `ro`.",
        config.kb_auto_confirm
    )
}

/// Enumerate chunks for all (or one) pending documents, deterministic order.
/// `chunk_bytes == 0` is rejected: a budget is required to bound sessions.
pub(crate) fn enumerate_extract_chunks(
    ctx: &KbContext,
    source: Option<&str>,
    chunk_bytes: usize,
) -> Result<Vec<ExtractChunk>> {
    if chunk_bytes == 0 {
        anyhow::bail!("[KB_CONFIG_ERROR] --chunk-bytes must be >= 1");
    }
    let conn = lock_conn(ctx)?;
    let mut docs_stmt = conn.prepare(
        "SELECT id, title, source FROM documents \
         WHERE superseded_by IS NULL AND analysis_status IN ('pending', 'analyzing') \
         AND (?1 IS NULL OR id = ?1 OR source = ?1) \
         ORDER BY COALESCE(source, title), id",
    )?;
    let docs = docs_stmt
        .query_map([source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(docs_stmt);
    if source.is_some() && docs.is_empty() {
        anyhow::bail!(
            "[KB_NOT_FOUND] No pending document matches {:?}.",
            source.unwrap_or_default()
        );
    }
    let mut chunks = Vec::new();
    for (doc_id, title, doc_source) in &docs {
        let label = doc_source.clone().unwrap_or_else(|| title.clone());
        let mut types_stmt = conn.prepare(
            "SELECT DISTINCT unit_type FROM document_units \
             WHERE document_id = ?1 AND obsolete = 0 ORDER BY unit_type",
        )?;
        let types = types_stmt
            .query_map([doc_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(types_stmt);
        for unit_type in &types {
            chunks.extend(pack_units(&conn, doc_id, &label, unit_type, chunk_bytes)?);
        }
    }
    Ok(chunks)
}

/// Greedy byte-budget packing in position order; a unit never splits and an
/// oversized unit becomes its own chunk. Bytes measured in Rust (SQLite
/// `length()` counts characters, not bytes).
fn pack_units(
    conn: &rusqlite::Connection,
    doc_id: &str,
    label: &str,
    unit_type: &str,
    chunk_bytes: usize,
) -> Result<Vec<ExtractChunk>> {
    let mut units_stmt = conn.prepare(
        "SELECT id, position, text FROM document_units \
         WHERE document_id = ?1 AND unit_type = ?2 AND obsolete = 0 \
         ORDER BY position ASC, id ASC",
    )?;
    let units = units_stmt
        .query_map(params![doc_id, unit_type], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<(String, i64, String)>, _>>()?;
    drop(units_stmt);

    let mut chunks = Vec::new();
    let mut cur: Vec<(String, i64, usize)> = Vec::new();
    let mut cur_bytes: usize = 0;
    for (id, pos, text) in &units {
        let len = text.as_bytes().len();
        if !cur.is_empty() && cur_bytes + len > chunk_bytes {
            chunks.push(make_chunk(doc_id, label, unit_type, &cur, cur_bytes));
            cur.clear();
            cur_bytes = 0;
        }
        cur.push((id.clone(), *pos, len));
        cur_bytes += len;
    }
    if !cur.is_empty() {
        chunks.push(make_chunk(doc_id, label, unit_type, &cur, cur_bytes));
    }
    Ok(chunks)
}

/// Build one chunk from a non-empty position-ordered run.
fn make_chunk(
    doc_id: &str,
    label: &str,
    unit_type: &str,
    cur: &[(String, i64, usize)],
    bytes: usize,
) -> ExtractChunk {
    ExtractChunk {
        document_id: doc_id.to_string(),
        document_label: label.to_string(),
        unit_type: unit_type.to_string(),
        pos_from: cur.first().map(|c| c.1).unwrap_or(0),
        pos_to: cur.last().map(|c| c.1).unwrap_or(0),
        unit_count: cur.len(),
        bytes_est: bytes,
        unit_ids: cur.iter().map(|c| c.0.clone()).collect(),
    }
}

/// Deterministic dry-run text: chunk table, totals, rough token hint.
/// No ids or timestamps, so two runs on the same DB match exactly.
pub(crate) fn dry_run_report(chunks: &[ExtractChunk]) -> String {
    if chunks.is_empty() {
        return "[kb extract dry-run] no pending documents".to_string();
    }
    let units: usize = chunks.iter().map(|c| c.unit_count).sum();
    let bytes: usize = chunks.iter().map(|c| c.bytes_est).sum();
    let mut out = format!(
        "[kb extract dry-run] {} chunks, {} units, {} bytes (~{} tokens, rough)\n",
        chunks.len(),
        units,
        bytes,
        bytes / BYTES_PER_TOKEN
    );
    for c in chunks {
        out.push_str(&format!(
            "- {} [{}] pos {}..={} ({} units, {} bytes)\n",
            c.document_label, c.unit_type, c.pos_from, c.pos_to, c.unit_count, c.bytes_est
        ));
    }
    out
}

/// Register chunks as `pending` (idempotent). Returns newly inserted rows.
pub(crate) fn register_chunks(ctx: &KbContext, chunks: &[ExtractChunk]) -> Result<usize> {
    let conn = lock_conn(ctx)?;
    let mut inserted = 0usize;
    for c in chunks {
        let id = uuid::Uuid::new_v4().to_string();
        let read_ranges = serde_json::to_string(&c.unit_ids)?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO analysis_chunks \
             (id, run_id, document_id, unit_type, pos_from, pos_to, unit_count, \
              bytes_est, status, attempts, read_ranges) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', 0, ?9)",
            params![
                id,
                ctx.run_id.to_string(),
                c.document_id,
                c.unit_type,
                c.pos_from,
                c.pos_to,
                c.unit_count as i64,
                c.bytes_est as i64,
                read_ranges
            ],
        )?;
        inserted += n;
    }
    Ok(inserted)
}

/// Deterministic progress text: per-document done/total plus chunk lines.
pub(crate) fn extract_status(ctx: &KbContext, source: Option<&str>) -> Result<String> {
    let conn = lock_conn(ctx)?;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(d.source, d.title), c.unit_type, c.pos_from, c.pos_to, \
                c.status, c.attempts \
         FROM analysis_chunks c JOIN documents d ON d.id = c.document_id \
         WHERE (?1 IS NULL OR c.document_id = ?1 OR d.source = ?1) \
         ORDER BY 1, c.unit_type, c.pos_from",
    )?;
    let rows = stmt
        .query_map([source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    if rows.is_empty() {
        return Ok("[kb extract status] no registered chunks".to_string());
    }
    let done = rows.iter().filter(|r| r.4 == CHUNK_DONE).count();
    let mut out = format!("[kb extract status] {}/{} chunks done\n", done, rows.len());
    for (label, unit_type, from, to, status, attempts) in &rows {
        out.push_str(&format!(
            "- {} [{}] pos {}..={} {} (attempts {})\n",
            label, unit_type, from, to, status, attempts
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    fn mem_ctx() -> KbContext {
        let conn = Connection::open_in_memory().unwrap();
        crate::kb_schema::apply_pragmas(&conn).unwrap();
        crate::kb_schema::migrate(&conn).unwrap();
        let run_id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO analysis_runs (id, label, status) VALUES (?1, 'test', 'completed')",
            [run_id.to_string()],
        )
        .unwrap();
        KbContext {
            kb_dir: "mem".to_string(),
            max_bytes: 65536,
            conn: Arc::new(Mutex::new(conn)),
            run_id,
        }
    }

    fn add_doc(ctx: &KbContext, title: &str, source: &str, status: &str) -> String {
        let id = Uuid::new_v4().to_string();
        let conn = lock_conn(ctx).unwrap();
        conn.execute(
            "INSERT INTO documents (id, title, source, document_type, version, analysis_status, file_hash) \
             VALUES (?1, ?2, ?3, 'md', '1', ?4, 'hash')",
            params![id, title, source, status],
        )
        .unwrap();
        id
    }

    fn add_unit(ctx: &KbContext, doc: &str, unit_type: &str, pos: i64, text: &str) {
        let conn = lock_conn(ctx).unwrap();
        conn.execute(
            "INSERT INTO document_units (id, document_id, unit_type, text, position) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![Uuid::new_v4().to_string(), doc, unit_type, text, pos],
        )
        .unwrap();
    }

    fn ten(n: usize) -> String {
        "x".repeat(n)
    }

    #[test]
    fn packs_by_budget_without_splitting_units() {
        let ctx = mem_ctx();
        let doc = add_doc(&ctx, "t", "data/t.md", "pending");
        for pos in 0..3 {
            add_unit(&ctx, &doc, "paragraph", pos, &ten(10));
        }
        // 10+10 fit in 25; the third overflows into its own chunk.
        let chunks = enumerate_extract_chunks(&ctx, None, 25).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].pos_from, chunks[0].pos_to), (0, 1));
        assert_eq!((chunks[1].pos_from, chunks[1].pos_to), (2, 2));
        assert_eq!(chunks[0].unit_count, 2);
        assert_eq!(chunks[0].bytes_est, 20);
    }

    #[test]
    fn oversized_unit_is_its_own_chunk() {
        let ctx = mem_ctx();
        let doc = add_doc(&ctx, "t", "data/t.md", "pending");
        add_unit(&ctx, &doc, "paragraph", 0, &ten(100));
        add_unit(&ctx, &doc, "paragraph", 1, &ten(10));
        let chunks = enumerate_extract_chunks(&ctx, None, 25).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].pos_from, chunks[0].pos_to), (0, 0));
        assert_eq!(chunks[0].bytes_est, 100);
    }

    #[test]
    fn only_pending_current_docs_enumerated() {
        let ctx = mem_ctx();
        let pending = add_doc(&ctx, "p", "data/p.md", "pending");
        let analyzing = add_doc(&ctx, "a", "data/a.md", "analyzing");
        let done = add_doc(&ctx, "d", "data/d.md", "analyzed");
        let old = add_doc(&ctx, "o", "data/o.md", "pending");
        // Superseded version drops out.
        {
            let conn = lock_conn(&ctx).unwrap();
            conn.execute(
                "UPDATE documents SET superseded_by = 'next' WHERE id = ?1",
                [&old],
            )
            .unwrap();
        }
        for d in [&pending, &analyzing, &done, &old] {
            add_unit(&ctx, d, "paragraph", 0, "text");
        }
        let chunks = enumerate_extract_chunks(&ctx, None, 65536).unwrap();
        let ids: Vec<&str> = chunks.iter().map(|c| c.document_id.as_str()).collect();
        assert!(ids.contains(&pending.as_str()));
        assert!(ids.contains(&analyzing.as_str()));
        assert!(!ids.contains(&done.as_str()));
        assert!(!ids.contains(&old.as_str()));
    }

    #[test]
    fn dry_run_is_deterministic() {
        let ctx = mem_ctx();
        let doc = add_doc(&ctx, "t", "data/t.md", "pending");
        for pos in 0..5 {
            add_unit(&ctx, &doc, "paragraph", pos, &ten(10));
        }
        let a = dry_run_report(&enumerate_extract_chunks(&ctx, None, 25).unwrap());
        let b = dry_run_report(&enumerate_extract_chunks(&ctx, None, 25).unwrap());
        assert_eq!(a, b);
        assert!(a.contains("3 chunks"));
    }

    #[test]
    fn register_is_idempotent_and_status_reads_back() {
        let ctx = mem_ctx();
        let doc = add_doc(&ctx, "t", "data/t.md", "pending");
        add_unit(&ctx, &doc, "paragraph", 0, "text");
        let chunks = enumerate_extract_chunks(&ctx, None, 65536).unwrap();
        assert_eq!(register_chunks(&ctx, &chunks).unwrap(), 1);
        assert_eq!(register_chunks(&ctx, &chunks).unwrap(), 0);
        let status = extract_status(&ctx, None).unwrap();
        assert!(status.contains("0/1 chunks done"));
        assert!(status.contains("pending"));
    }

    #[test]
    fn write_approval_requires_rw() {
        use clap::Parser;
        let mut config = crate::startup::Config::try_parse_from(["agt"]).unwrap();
        config.kb_auto_confirm = crate::startup::KbAutoConfirm::Rw;
        assert!(require_write_approval(&config).is_ok());
        config.kb_auto_confirm = crate::startup::KbAutoConfirm::Ro;
        assert!(require_write_approval(&config).is_err());
        config.kb_auto_confirm = crate::startup::KbAutoConfirm::Ask;
        assert!(require_write_approval(&config).is_err());
    }
}
