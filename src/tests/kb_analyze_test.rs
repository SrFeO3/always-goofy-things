//! Tests for `src/kb_analyze.rs`: chunking, verdicts, record, finalize.

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

fn add_evidence(
    ctx: &KbContext,
    doc: &str,
    unit: &str,
    run_id: &str,
    start: Option<i64>,
    end: Option<i64>,
    matched: Option<&str>,
) {
    let conn = lock_conn(ctx).unwrap();
    conn.execute(
        "INSERT INTO evidence (id, document_id, source_unit_id, target_type, target_id, \
             start_offset, end_offset, matched_text, analysis_run_id) \
             VALUES (?1, ?2, ?3, 'claim', 'claim-1', ?4, ?5, ?6, ?7)",
        params![
            Uuid::new_v4().to_string(),
            doc,
            unit,
            start,
            end,
            matched,
            run_id
        ],
    )
    .unwrap();
}

fn unit_id(ctx: &KbContext, doc: &str, pos: i64) -> String {
    let conn = lock_conn(ctx).unwrap();
    conn.query_row(
        "SELECT id FROM document_units WHERE document_id = ?1 AND position = ?2",
        params![doc, pos],
        |row| row.get(0),
    )
    .unwrap()
}

fn task_for(ctx: &KbContext, doc: &str) -> Task {
    let chunks = enumerate_extract_chunks(ctx, None, 65536).unwrap();
    let chunk = chunks.iter().find(|c| c.document_id == doc).unwrap();
    ExtractEnumerator::task_for(chunk)
}

fn judge(ctx: &KbContext, task: &Task) -> VerifyResult {
    judge_task(ctx, task).unwrap()
}

#[test]
fn judge_passes_full_valid_coverage() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "t", "data/t.md", "pending");
    add_unit(&ctx, &doc, "paragraph", 0, "first unit text here");
    add_unit(&ctx, &doc, "paragraph", 1, "second unit text here");
    let run = ctx.run_id.to_string();
    add_evidence(
        &ctx,
        &doc,
        &unit_id(&ctx, &doc, 0),
        &run,
        Some(0),
        Some(5),
        Some("first"),
    );
    add_evidence(
        &ctx,
        &doc,
        &unit_id(&ctx, &doc, 1),
        &run,
        Some(0),
        Some(6),
        Some("second"),
    );
    let task = task_for(&ctx, &doc);
    assert!(matches!(judge(&ctx, &task), VerifyResult::Pass));
}

#[test]
fn judge_fails_long_gaps_and_warns_short_only() {
    let ctx = mem_ctx();
    let long_doc = add_doc(&ctx, "l", "data/l.md", "pending");
    add_unit(
        &ctx,
        &long_doc,
        "paragraph",
        0,
        &"long uncovered body text ".repeat(10),
    );
    assert!(matches!(
        judge(&ctx, &task_for(&ctx, &long_doc)),
        VerifyResult::Fail { .. }
    ));
    let short_doc = add_doc(&ctx, "s", "data/s.md", "pending");
    add_unit(&ctx, &short_doc, "paragraph", 0, "Heading");
    assert!(matches!(
        judge(&ctx, &task_for(&ctx, &short_doc)),
        VerifyResult::Warn { .. }
    ));
}

#[test]
fn judge_fails_invented_offsets() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "t", "data/t.md", "pending");
    add_unit(&ctx, &doc, "paragraph", 0, "first unit text here");
    let run = ctx.run_id.to_string();
    // end offset past the unit end: hallucinated excerpt range.
    add_evidence(
        &ctx,
        &doc,
        &unit_id(&ctx, &doc, 0),
        &run,
        Some(0),
        Some(999),
        Some("first"),
    );
    assert!(matches!(
        judge(&ctx, &task_for(&ctx, &doc)),
        VerifyResult::Fail { .. }
    ));
}

#[test]
fn judge_trusts_other_runs_for_coverage_only() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "t", "data/t.md", "pending");
    add_unit(&ctx, &doc, "paragraph", 0, &"covered body text ".repeat(10));
    // Foreign run: counts for coverage, skips offset validation.
    {
        let conn = lock_conn(&ctx).unwrap();
        conn.execute(
                "INSERT INTO analysis_runs (id, label, status) VALUES ('other-run', 'test', 'completed')",
                [],
            )
            .unwrap();
    }
    add_evidence(
        &ctx,
        &doc,
        &unit_id(&ctx, &doc, 0),
        "other-run",
        Some(0),
        Some(999),
        Some("x"),
    );
    assert!(matches!(
        judge(&ctx, &task_for(&ctx, &doc)),
        VerifyResult::Pass
    ));
}

#[test]
fn key_codec_roundtrips_and_rejects() {
    let key = chunk_key("doc-1", "paragraph", 3, 41);
    assert_eq!(
        decode_key(&key).unwrap(),
        ("doc-1".to_string(), "paragraph".to_string(), 3, 41)
    );
    assert!(decode_key("a|b|c").is_err());
    assert!(decode_key("a|b|x|y").is_err());
}

fn registered(ctx: &KbContext, doc: &str) -> Vec<(i64, i64, String, i64)> {
    let conn = lock_conn(ctx).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT pos_from, pos_to, status, attempts FROM analysis_chunks \
                 WHERE document_id = ?1 ORDER BY pos_from",
        )
        .unwrap();
    stmt.query_map([doc], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn outcome(end: crate::reasoning::EndReason, report: Option<&str>) -> SessionOutcome {
    SessionOutcome {
        end_reason: end,
        report: report.map(str::to_string),
        raw_report: None,
        label: "test-session".to_string(),
    }
}

#[test]
fn record_closes_and_counts_attempts() {
    use crate::reasoning::EndReason;
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "t", "data/t.md", "pending");
    add_unit(&ctx, &doc, "paragraph", 0, "text");
    let chunks = enumerate_extract_chunks(&ctx, None, 65536).unwrap();
    register_chunks(&ctx, &chunks).unwrap();
    let task = ExtractEnumerator::task_for(&chunks[0]);
    record_chunk(
        &ctx,
        &task,
        &outcome(EndReason::Completed, Some("r1")),
        &VerifyResult::Fail {
            reason: "gap".to_string(),
        },
    )
    .unwrap();
    record_chunk(
        &ctx,
        &task,
        &outcome(EndReason::Completed, Some("r2")),
        &VerifyResult::Pass,
    )
    .unwrap();
    let rows = registered(&ctx, &doc);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, CHUNK_DONE);
    assert_eq!(rows[0].3, 2);
    let conn = lock_conn(&ctx).unwrap();
    let report: String = conn
        .query_row(
            "SELECT report FROM analysis_chunks WHERE document_id = ?1",
            [&doc],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(report, "r2");
}

#[test]
fn finalize_marks_only_covered_docs() {
    let ctx = mem_ctx();
    let full = add_doc(&ctx, "f", "data/f.md", "pending");
    add_unit(&ctx, &full, "paragraph", 0, "covered body text here");
    let run = ctx.run_id.to_string();
    add_evidence(
        &ctx,
        &full,
        &unit_id(&ctx, &full, 0),
        &run,
        Some(0),
        Some(7),
        Some("covered"),
    );
    let partial = add_doc(&ctx, "p", "data/p.md", "pending");
    add_unit(
        &ctx,
        &partial,
        "paragraph",
        0,
        &"missing body text ".repeat(10),
    );
    let out = finalize_docs(
        &ctx,
        &[
            (full.clone(), "f".to_string()),
            (partial.clone(), "p".to_string()),
        ],
    )
    .unwrap();
    assert_eq!(out.status, JobStatus::Failed);
    let conn = lock_conn(&ctx).unwrap();
    let status = |id: &str| -> (String, Option<String>) {
        conn.query_row(
            "SELECT analysis_status, analyzed_at FROM documents WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    let (full_status, full_at) = status(&full);
    assert_eq!(full_status, "analyzed");
    assert!(full_at.is_some());
    assert_eq!(status(&partial).0, "pending");
}

#[test]
fn build_embeds_scope_units_and_handover() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "t", "data/t.md", "pending");
    add_unit(&ctx, &doc, "paragraph", 0, "alpha text");
    add_unit(&ctx, &doc, "paragraph", 1, "beta text");
    let chunks = enumerate_extract_chunks(&ctx, None, 65536).unwrap();
    register_chunks(&ctx, &chunks).unwrap();
    let system = crate::startup::system_message_kb_extract(&test_cli_config());
    let task = ExtractEnumerator::task_for(&chunks[0]);
    let off = build_extract_spec(&ctx, "s", &system, HandoverMode::Off, &task).unwrap();
    assert!(off.instruction.contains("pos 0..=1"));
    assert!(off.instruction.contains("<unit id=\""));
    assert!(off.instruction.contains("alpha text"));
    assert!(off.instruction.contains("Handover: none"));
    assert!(off.label.starts_with("s_kb-"));
    // Auto with no predecessor falls back to first-chunk note.
    let auto = build_extract_spec(
        &ctx,
        "s",
        &system,
        HandoverMode::Auto { max_chars: 100 },
        &task,
    )
    .unwrap();
    assert!(auto.instruction.contains("first chunk"));
}

#[test]
fn previous_report_truncates_over_budget() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "t", "data/t.md", "pending");
    add_unit(&ctx, &doc, "paragraph", 0, "a");
    add_unit(&ctx, &doc, "paragraph", 1, "b");
    let chunks = enumerate_extract_chunks(&ctx, None, 1).unwrap();
    assert_eq!(chunks.len(), 2);
    register_chunks(&ctx, &chunks).unwrap();
    let conn = lock_conn(&ctx).unwrap();
    conn.execute(
        "UPDATE analysis_chunks SET status = 'done', report = '0123456789' \
             WHERE document_id = ?1 AND pos_from = 0",
        [&doc],
    )
    .unwrap();
    drop(conn);
    let full = previous_report(&ctx, &doc, "paragraph", 1, 100).unwrap();
    assert_eq!(full, Some("0123456789".to_string()));
    let cut = previous_report(&ctx, &doc, "paragraph", 1, 4)
        .unwrap()
        .unwrap();
    assert!(cut.ends_with("[truncated]"));
    assert!(cut.starts_with("0123"));
}

#[test]
fn extract_system_forbids_self_declared_analyzed() {
    let msg = crate::startup::system_message_kb_extract(&test_cli_config());
    assert_eq!(msg.role, "system");
    assert!(msg.content.contains("NEVER set `analysis_status`"));
    assert!(msg.content.contains("Extraction Report"));
}

#[test]
fn write_approval_requires_rw() {
    let mut config = test_cli_config();
    config.kb_auto_confirm = crate::startup::KbAutoConfirm::Rw;
    assert!(require_write_approval(&config).is_ok());
    config.kb_auto_confirm = crate::startup::KbAutoConfirm::Ro;
    assert!(require_write_approval(&config).is_err());
    config.kb_auto_confirm = crate::startup::KbAutoConfirm::Ask;
    assert!(require_write_approval(&config).is_err());
}

fn test_cli_config() -> crate::startup::Config {
    use clap::Parser;
    crate::startup::Config::try_parse_from(["agt"]).unwrap()
}
