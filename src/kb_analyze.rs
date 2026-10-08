//! KB extract enumeration (todo-refine Phase 2).
//!
//! Splits pending documents into byte-budget chunks and tracks them in
//! `analysis_chunks`. Execution (fresh sessions) lands in Phase 3.

use anyhow::{Result, anyhow};
use rusqlite::{OptionalExtension, params};
use std::path::Path;

use crate::job::{
    Enumerator, JobOptions, JobOutcome, JobStatus, Store, Task, Verifier, VerifyResult, run_job,
};
use crate::kb::{KbContext, lock_conn, now_iso};
use crate::model::Message;
use crate::reasoning::LoopCtx;
use crate::session::{ReportRule, SessionOutcome, SessionSpec, ToolPolicy, run_session};
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
        let len = text.len(); // bytes (`String::len`); never SQL `length()`
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
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)",
            params![
                id,
                ctx.run_id.to_string(),
                c.document_id,
                c.unit_type,
                c.pos_from,
                c.pos_to,
                c.unit_count as i64,
                c.bytes_est as i64,
                CHUNK_PENDING,
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

/// Tools allowed inside extract sessions (system text and policy share it).
pub(crate) const KB_EXTRACT_TOOL_NAMES: &[&str] = &[
    "kb_schema",
    "kb_search",
    "kb_read",
    "kb_insert",
    "kb_update",
];

/// Extraction Report budget (shared by the rule and the system text).
pub(crate) const EXTRACT_REPORT_MAX_CHARS: usize = 500;

/// Final-report contract for extract sessions.
pub(crate) const EXTRACT_REPORT_RULE: ReportRule = ReportRule {
    title: "Extraction Report",
    fields: &["Status", "Units", "Notes"],
    max_chars: EXTRACT_REPORT_MAX_CHARS,
};

/// Uncovered units shorter than this (chars, trimmed) read as headings:
/// Warn and proceed instead of failing the chunk.
const HEADING_SHORT_CHARS: usize = 64;

/// Handover budget for `--handover auto` between adjacent chunks.
pub(crate) const HANDOVER_AUTO_CHARS: usize = 2000;

/// Which chunks to (re)run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RedoMode {
    SkipDone,
    IncludeDone,
}

/// Chunk-to-chunk handover (`off` = independent chunks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandoverMode {
    Off,
    Auto { max_chars: usize },
}

/// Extract run knobs (CLI flags land here in Phase 5).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtractOptions {
    pub chunk_bytes: usize,
    pub max_retries: u32,
    pub redo: RedoMode,
    pub handover: HandoverMode,
}

/// Stable chunk key: `document_id|unit_type|pos_from|pos_to`.
fn chunk_key(document_id: &str, unit_type: &str, pos_from: i64, pos_to: i64) -> String {
    format!("{}|{}|{}|{}", document_id, unit_type, pos_from, pos_to)
}

/// Split a chunk key back into its range.
fn decode_key(key: &str) -> Result<(String, String, i64, i64)> {
    let mut parts = key.split('|');
    match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some(doc), Some(unit_type), Some(from), Some(to), None) => Ok((
            doc.to_string(),
            unit_type.to_string(),
            from.parse()
                .map_err(|_| anyhow!("[KB_INTERNAL_ERROR] bad chunk key {:?}", key))?,
            to.parse()
                .map_err(|_| anyhow!("[KB_INTERNAL_ERROR] bad chunk key {:?}", key))?,
        )),
        _ => anyhow::bail!("[KB_INTERNAL_ERROR] bad chunk key {:?}", key),
    }
}

/// Units in the range that still lack non-obsolete evidence.
fn uncovered_units(
    kb: &KbContext,
    document_id: &str,
    unit_type: &str,
    pos_from: i64,
    pos_to: i64,
) -> Result<Vec<(String, i64, String)>> {
    let conn = lock_conn(kb)?;
    let mut stmt = conn.prepare(
        "SELECT u.id, u.position, u.text FROM document_units u \
         WHERE u.document_id = ?1 AND u.obsolete = 0 AND u.unit_type = ?2 \
           AND u.position BETWEEN ?3 AND ?4 \
           AND NOT EXISTS (SELECT 1 FROM evidence e \
                            WHERE e.source_unit_id = u.id AND e.obsolete = 0) \
         ORDER BY u.position ASC",
    )?;
    let rows = stmt
        .query_map(params![document_id, unit_type, pos_from, pos_to], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Offsets invented by the LLM fail the chunk: every evidence row of this
/// run must land inside its unit with a verbatim excerpt.
fn run_evidence_problems(
    kb: &KbContext,
    run_id: &str,
    document_id: &str,
    unit_type: &str,
    pos_from: i64,
    pos_to: i64,
) -> Result<Vec<String>> {
    let conn = lock_conn(kb)?;
    let mut stmt = conn.prepare(
        "SELECT e.id, e.start_offset, e.end_offset, e.matched_text, u.text \
         FROM evidence e JOIN document_units u ON u.id = e.source_unit_id \
         WHERE e.analysis_run_id = ?1 AND e.document_id = ?2 AND e.obsolete = 0 \
           AND u.obsolete = 0 AND u.unit_type = ?3 AND u.position BETWEEN ?4 AND ?5",
    )?;
    let rows = stmt
        .query_map(
            params![run_id, document_id, unit_type, pos_from, pos_to],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut problems = Vec::new();
    for (id, start, end, matched, text) in &rows {
        let len = text.chars().count() as i64;
        let bad = match (start, end, matched) {
            (Some(s), Some(e), Some(m)) => {
                !(*s >= 0 && *s < *e && *e <= len && !m.is_empty() && text.contains(m))
            }
            _ => true,
        };
        if bad {
            problems.push(id.clone());
        }
    }
    Ok(problems)
}

/// Next pending chunk as a `run_job` task.
/// Quote a literal for the coverage SQL string (ids/types are app-built,
/// but quoting stays total).
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// Uncovered-unit ids in the range; empty = full coverage.
fn coverage_sql(document_id: &str, unit_type: &str, pos_from: i64, pos_to: i64) -> String {
    format!(
        "SELECT u.id FROM document_units u WHERE u.document_id = '{}' AND u.obsolete = 0 \
         AND u.unit_type = '{}' AND u.position BETWEEN {} AND {} AND NOT EXISTS \
         (SELECT 1 FROM evidence e WHERE e.source_unit_id = u.id AND e.obsolete = 0)",
        esc(document_id),
        esc(unit_type),
        pos_from,
        pos_to
    )
}

/// Next pending chunk as a `run_job` task. The coverage query doubles as
/// the task's mechanical check string.
struct ExtractEnumerator {
    chunks: Vec<ExtractChunk>,
    statuses: std::collections::HashMap<String, String>,
    redo_all: bool,
    pos: usize,
}

impl ExtractEnumerator {
    fn task_for(chunk: &ExtractChunk) -> Task {
        Task {
            id: chunk_key(
                &chunk.document_id,
                &chunk.unit_type,
                chunk.pos_from,
                chunk.pos_to,
            ),
            description: format!(
                "extract {} [{}] pos {}..={} ({} units, {} bytes)",
                chunk.document_label,
                chunk.unit_type,
                chunk.pos_from,
                chunk.pos_to,
                chunk.unit_count,
                chunk.bytes_est
            ),
            verify: vec![crate::job::Check::SqlEmpty(coverage_sql(
                &chunk.document_id,
                &chunk.unit_type,
                chunk.pos_from,
                chunk.pos_to,
            ))],
        }
    }
}

impl Enumerator for ExtractEnumerator {
    async fn next_task(&mut self, _ctx: &mut LoopCtx<'_>) -> Result<Option<Task>> {
        while self.pos < self.chunks.len() {
            let chunk = &self.chunks[self.pos];
            self.pos += 1;
            let key = chunk_key(
                &chunk.document_id,
                &chunk.unit_type,
                chunk.pos_from,
                chunk.pos_to,
            );
            let done = self.statuses.get(&key).is_some_and(|s| s == CHUNK_DONE);
            if done && !self.redo_all {
                continue;
            }
            return Ok(Some(Self::task_for(chunk)));
        }
        Ok(None)
    }
}

/// Coverage judge plus whole-document finalize (app-owned `analyzed`).
struct ExtractVerifier<'a> {
    kb: &'a KbContext,
    docs: Vec<(String, String)>,
}

impl Verifier for ExtractVerifier<'_> {
    fn check(&self, _ctx: &LoopCtx<'_>, task: &Task) -> Result<VerifyResult> {
        judge_task(self.kb, task)
    }

    fn finalize(&self, _ctx: &LoopCtx<'_>) -> Result<JobOutcome> {
        finalize_docs(self.kb, &self.docs)
    }
}

/// Coverage probe over the task's own check strings, then offset
/// integrity (this run) and the heading short-text rule.
fn judge_task(kb: &KbContext, task: &Task) -> Result<VerifyResult> {
    let (doc, unit_type, from, to) = decode_key(&task.id)?;
    {
        let probe = |q: &str| -> Result<bool> {
            let conn = lock_conn(kb)?;
            let mut stmt = conn.prepare(q)?;
            let mut rows = stmt.query([])?;
            Ok(rows.next()?.is_none())
        };
        let mut covered = true;
        for check in &task.verify {
            if !matches!(check.eval(&probe), VerifyResult::Pass) {
                covered = false;
                break;
            }
        }
        if !covered {
            let uncovered = uncovered_units(kb, &doc, &unit_type, from, to)?;
            if uncovered
                .iter()
                .all(|(_, _, t)| t.trim().chars().count() < HEADING_SHORT_CHARS)
            {
                return Ok(VerifyResult::Warn {
                    reason: format!(
                        "{} short unit(s) without evidence (likely headings)",
                        uncovered.len()
                    ),
                });
            }
            return Ok(VerifyResult::Fail {
                reason: format!("{} unit(s) without evidence", uncovered.len()),
            });
        }
    }
    {
        let problems =
            run_evidence_problems(kb, &kb.run_id.to_string(), &doc, &unit_type, from, to)?;
        if !problems.is_empty() {
            return Ok(VerifyResult::Fail {
                reason: format!(
                    "{} evidence row(s) with bad offsets/excerpts (e.g. {})",
                    problems.len(),
                    problems[0]
                ),
            });
        }
    }
    Ok(VerifyResult::Pass)
}

/// Whole-document finalize: app-owned `analyzed` for fully covered docs.
fn finalize_docs(kb: &KbContext, docs: &[(String, String)]) -> Result<JobOutcome> {
    {
        let conn = lock_conn(kb)?;
        let mut analyzed = Vec::new();
        let mut incomplete = Vec::new();
        for (doc_id, label) in docs {
            let uncovered: i64 = conn.query_row(
                "SELECT COUNT(*) FROM document_units u \
                 WHERE u.document_id = ?1 AND u.obsolete = 0 \
                   AND length(trim(u.text)) >= ?2 \
                   AND NOT EXISTS (SELECT 1 FROM evidence e \
                                    WHERE e.source_unit_id = u.id AND e.obsolete = 0)",
                params![doc_id, HEADING_SHORT_CHARS as i64],
                |row| row.get(0),
            )?;
            if uncovered == 0 {
                conn.execute(
                    "UPDATE documents SET analysis_status = 'analyzed', analyzed_at = ?1 \
                     WHERE id = ?2",
                    params![now_iso(), doc_id],
                )?;
                analyzed.push(label.clone());
            } else {
                incomplete.push(label.clone());
            }
        }
        drop(conn);
        if incomplete.is_empty() {
            return Ok(JobOutcome {
                status: JobStatus::Completed,
                summary: format!("analyzed({}): {}", analyzed.len(), analyzed.join(", ")),
            });
        }
        Ok(JobOutcome {
            status: JobStatus::Failed,
            summary: format!(
                "incomplete({}): {}",
                incomplete.len(),
                incomplete.join(", ")
            ),
        })
    }
}

/// Chunk-row recorder: Pass/Warn close the row, Fail keeps it resumable.
struct ExtractStore<'a> {
    kb: &'a KbContext,
}

impl Store for ExtractStore<'_> {
    fn record(
        &self,
        _ctx: &LoopCtx<'_>,
        task: &Task,
        outcome: &SessionOutcome,
        verdict: &VerifyResult,
    ) -> Result<()> {
        record_chunk(self.kb, task, outcome, verdict)
    }
}

/// Chunk-row write: Pass/Warn close the row, Fail keeps it resumable.
fn record_chunk(
    kb: &KbContext,
    task: &Task,
    outcome: &SessionOutcome,
    verdict: &VerifyResult,
) -> Result<()> {
    let (doc, unit_type, from, to) = decode_key(&task.id)?;
    let (status, error) = match verdict {
        VerifyResult::Pass => (CHUNK_DONE, None),
        VerifyResult::Warn { reason } => (CHUNK_DONE, Some(reason.clone())),
        VerifyResult::Fail { reason } => (CHUNK_FAILED, Some(reason.clone())),
    };
    let conn = lock_conn(kb)?;
    conn.execute(
        "UPDATE analysis_chunks SET status = ?1, attempts = attempts + 1, \
                report = ?2, error = ?3, finished_at = ?4, \
                started_at = COALESCE(started_at, ?4) \
         WHERE document_id = ?5 AND unit_type = ?6 AND pos_from = ?7 AND pos_to = ?8",
        params![
            status,
            outcome.report,
            error,
            now_iso(),
            doc,
            unit_type,
            from,
            to
        ],
    )?;
    Ok(())
}

/// Previous done chunk's report in the same document/type (handover auto).
fn previous_report(
    kb: &KbContext,
    document_id: &str,
    unit_type: &str,
    pos_from: i64,
    max_chars: usize,
) -> Result<Option<String>> {
    let conn = lock_conn(kb)?;
    let report: Option<String> = conn
        .query_row(
            "SELECT report FROM analysis_chunks \
             WHERE document_id = ?1 AND unit_type = ?2 AND pos_to < ?3 AND status = 'done' \
             ORDER BY pos_to DESC LIMIT 1",
            params![document_id, unit_type, pos_from],
            |row| row.get(0),
        )
        .optional()?;
    let Some(report) = report else {
        return Ok(None);
    };
    if report.chars().count() <= max_chars {
        return Ok(Some(report));
    }
    let mut end = max_chars;
    while end > 0 && !report.is_char_boundary(end) {
        end -= 1;
    }
    Ok(Some(format!("{}…[truncated]", &report[..end])))
}

/// Build one chunk's fresh-session inputs (P1 prompt embedding).
fn build_extract_spec(
    kb: &KbContext,
    session_label: &str,
    system: &Message,
    handover: HandoverMode,
    task: &Task,
) -> Result<SessionSpec> {
    let (doc_id, unit_type, from, to) = decode_key(&task.id)?;
    let conn = lock_conn(kb)?;
    let label: String = conn.query_row(
        "SELECT COALESCE(source, title) FROM documents WHERE id = ?1",
        [&doc_id],
        |row| row.get(0),
    )?;
    let mut units_stmt = conn.prepare(
        "SELECT id, position, text FROM document_units \
         WHERE document_id = ?1 AND unit_type = ?2 AND obsolete = 0 \
           AND position BETWEEN ?3 AND ?4 ORDER BY position ASC",
    )?;
    let units = units_stmt
        .query_map(params![doc_id, unit_type, from, to], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(units_stmt);
    drop(conn);
    let bytes: usize = units.iter().map(|(_, _, t)| t.len()).sum();
    let handover_line = match handover {
        HandoverMode::Off => "Handover: none (independent chunk).".to_string(),
        HandoverMode::Auto { max_chars } => {
            match previous_report(kb, &doc_id, &unit_type, from, max_chars)? {
                Some(r) => format!("Handover (previous chunk report): {}", r),
                None => "Handover: none (first chunk).".to_string(),
            }
        }
    };
    let mut instruction = format!(
        "Scope: extract {} [{}] pos {}..={} ({} units, {} bytes). Read every unit below.\n\
         Rules: register knowledge with kb_insert + evidence in the same call (ref/target_ref); \
         matched_text verbatim; offsets are char offsets into unit.text; fix mistakes via kb_update \
         obsolete (never delete); NEVER set analysis_status (the app finalizes it).\n\
         Deliverable: at least one evidence row per unit; close with an Extraction Report.\n{}\nUnits:\n",
        label,
        unit_type,
        from,
        to,
        units.len(),
        bytes,
        handover_line
    );
    for (id, pos, text) in &units {
        instruction.push_str(&format!(
            "<unit id=\"{}\" pos=\"{}\">{}</unit>\n",
            id, pos, text
        ));
    }
    Ok(SessionSpec {
        label: format!(
            "{}_kb-{}-{}-{}",
            session_label,
            &doc_id[..8.min(doc_id.len())],
            unit_type,
            from
        ),
        system: system.clone(),
        instruction,
        allow_tools: ToolPolicy::AllowList(KB_EXTRACT_TOOL_NAMES),
        report_rule: EXTRACT_REPORT_RULE,
        max_turns: None,
    })
}

/// Run extract over pending chunks via `run_job` and return its summary.
/// Non-`Completed` outcomes return early: resume later to finalize.
pub(crate) async fn run_extract(
    ctx: &mut LoopCtx<'_>,
    source: Option<&str>,
    options: &ExtractOptions,
) -> Result<String> {
    require_write_approval(ctx.config)?;
    let kb: &KbContext = ctx.kb_ctx.ok_or_else(|| {
        anyhow::anyhow!("[KB_CONFIG_ERROR] The knowledge base is not initialized.")
    })?;
    let chunks = enumerate_extract_chunks(kb, source, options.chunk_bytes)?;
    if chunks.is_empty() {
        return Ok("[kb extract] no pending documents".to_string());
    }
    register_chunks(kb, &chunks)?;
    let statuses = chunk_statuses(kb, &chunks)?;
    let system = startup::system_message_kb_extract(ctx.config);
    let session_label = ctx.config.session_label.clone();
    let handover = options.handover;
    let mut docs: Vec<(String, String)> = Vec::new();
    for c in &chunks {
        if !docs.iter().any(|(id, _)| id == &c.document_id) {
            docs.push((c.document_id.clone(), c.document_label.clone()));
        }
    }
    let mut enumerator = ExtractEnumerator {
        chunks,
        statuses,
        redo_all: options.redo == RedoMode::IncludeDone,
        pos: 0,
    };
    let verifier = ExtractVerifier { kb, docs };
    let store = ExtractStore { kb };
    let job_options = JobOptions {
        max_retries: options.max_retries,
    };
    let outcome = run_job(
        ctx,
        &mut enumerator,
        &verifier,
        &store,
        &job_options,
        |task| build_extract_spec(kb, &session_label, &system, handover, task),
    )
    .await?;
    Ok(outcome.summary)
}

/// Current row statuses keyed by chunk key.
fn chunk_statuses(
    kb: &KbContext,
    chunks: &[ExtractChunk],
) -> Result<std::collections::HashMap<String, String>> {
    let conn = lock_conn(kb)?;
    let mut map = std::collections::HashMap::new();
    let mut stmt = conn
        .prepare("SELECT document_id, unit_type, pos_from, pos_to, status FROM analysis_chunks")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    drop(conn);
    let wanted: std::collections::HashSet<String> = chunks
        .iter()
        .map(|c| chunk_key(&c.document_id, &c.unit_type, c.pos_from, c.pos_to))
        .collect();
    for (doc, unit_type, from, to, status) in rows {
        let key = chunk_key(&doc, &unit_type, from, to);
        if wanted.contains(&key) {
            map.insert(key, status);
        }
    }
    Ok(map)
}

/// Tools for analyze executors: KB reads/writes plus report workspace.
/// (`write_file` prompts y/N interactively; batch needs `--unsafe-reflex`.)
pub(crate) const ANALYZE_EXEC_TOOLS: &[&str] = &[
    "kb_schema",
    "kb_search",
    "kb_read",
    "kb_insert",
    "kb_update",
    "list_directory",
    "read_file",
    "write_file",
    "grep_search",
    "fetch_web",
    "calc",
];

/// Planning reads a little wider than the todo planner (KB scoping).
const ANALYZE_PLANNER_TOOLS: &[&str] = &[
    "list_directory",
    "read_file",
    "grep_search",
    "fetch_web",
    "calc",
    "kb_schema",
    "kb_search",
    "kb_read",
];

/// Analyze run knobs.
#[derive(Debug, Clone)]
pub(crate) struct AnalyzeOptions {
    pub max_retries: u32,
    pub note: String,
}

/// Stable generated-plan path per goal (reruns resume through it).
fn analyze_plan_path(workspace: &Path, goal: &str) -> std::path::PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    goal.hash(&mut h);
    workspace.join(format!(".todo/kb-analyze-{:016x}.json", h.finish()))
}

/// Cross-document investigation: plan once from the goal, then run the
/// replan job over the generated plan (todo engine, KB-aware tools).
/// Verification is deliverables (fatal) plus evidence discipline reviewed
/// at the final replan; open questions stay human-read (R7).
pub(crate) async fn run_kb_analyze(
    ctx: &mut LoopCtx<'_>,
    goal: &str,
    sources: &[String],
    options: &AnalyzeOptions,
) -> Result<String> {
    use crate::todo_job::{TodoMode, TodoOptions, run_todo};
    require_write_approval(ctx.config)?;
    if goal.trim().is_empty() {
        anyhow::bail!("[KB_CONFIG_ERROR] /kb analyze needs a goal.");
    }
    let focus = if sources.is_empty() {
        String::new()
    } else {
        format!("\nFocus documents: {}.", sources.join(", "))
    };
    let session_label = ctx.config.session_label.clone();
    let spec = SessionSpec {
        label: format!("{}_analyze-plan", session_label),
        system: startup::system_message_todo_planner(ctx.config),
        instruction: format!(
            "Goal (cross-document investigation): {}{}\n\
             Plan it as analyzing tasks over the KB (kb_search/kb_schema/kb_read to scope, \
             kb_insert/kb_update with evidence to record links, write_file for a report under \
             artifacts/). Reply with a {} fenced block holding the FULL task list as JSON \
             ({{\"tasks\": [{{\"id\", \"description\", \"verify\": [check strings]}}], \"deliverables\"?}}), \
             then notes. Checks read as exists/nonempty/contains/sql.",
            goal,
            focus,
            crate::todo_job::PLAN_FENCE,
        ),
        allow_tools: ToolPolicy::AllowList(ANALYZE_PLANNER_TOOLS),
        report_rule: ReportRule {
            title: "Analyze Plan",
            fields: &["Plan", "Notes"],
            max_chars: crate::todo_job::PLANNER_REPORT_MAX_CHARS,
        },
        max_turns: None,
    };
    let outcome = run_session(ctx, spec).await?;
    if !outcome.end_reason.is_completed() {
        anyhow::bail!(
            "[KB_LLM_ERROR] Planning session ended ({:?}); rephrase the goal and retry.",
            outcome.end_reason
        );
    }
    let text = outcome
        .raw_report
        .as_deref()
        .or(outcome.report.as_deref())
        .unwrap_or("");
    let Some(block) = crate::todo_job::extract_plan_block(text) else {
        anyhow::bail!(
            "[KB_LLM_ERROR] Planner produced no plan block; rephrase the goal and retry."
        );
    };
    let path = analyze_plan_path(std::path::Path::new("."), goal);
    let deliverable = format!("artifacts/kb-analyze-{:08x}.md", {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        goal.hash(&mut h);
        h.finish() as u32
    });
    crate::todo_job::write_generated_plan(goal, block, &path, &deliverable)?;
    let mut note = String::new();
    if !sources.is_empty() {
        note.push_str(&format!("Focus documents: {}. ", sources.join(", ")));
    }
    note.push_str(&options.note);
    run_todo(
        ctx,
        &path,
        &TodoOptions {
            mode: TodoMode::Replan,
            max_retries: options.max_retries,
            max_stalls: 3,
            note,
            executor_policy: ToolPolicy::AllowList(ANALYZE_EXEC_TOOLS),
        },
    )
    .await
}

#[cfg(test)]
#[path = "tests/kb_analyze_test.rs"]
mod tests;
