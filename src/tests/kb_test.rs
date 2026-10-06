//! Tests for `src/kb.rs`: the five `kb_*` tools and the `/kb` command.
//!
//! Uses an in-memory SQLite connection (`:memory:`) for tool behavior and a
//! temp directory for `/kb` file operations.

use super::*;
use rusqlite::{Connection, params};
use uuid::Uuid;

/// In-memory KB context (migrated) with a scratch `kb_dir` path.
fn mem_ctx() -> KbContext {
    let conn = Connection::open_in_memory().unwrap();
    kb_schema::apply_pragmas(&conn).unwrap();
    kb_schema::migrate(&conn).unwrap();
    let run_id = Uuid::new_v4();
    create_run(&conn, &run_id);
    KbContext {
        kb_dir: std::env::temp_dir()
            .join(format!("kb-mem-test-{}", run_id))
            .to_string_lossy()
            .to_string(),
        max_bytes: KB_DEFAULT_MAX_BYTES,
        conn: Arc::new(Mutex::new(conn)),
        run_id,
    }
}

/// File-based KB context in a temp dir (for /kb restore, which reopens the DB).
fn file_ctx() -> KbContext {
    let dir = std::env::temp_dir().join(format!("kb-file-test-{}", Uuid::new_v4()));
    let db_dir = dir.join("db");
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("library.sqlite");
    let conn = Connection::open(&db_path).unwrap();
    kb_schema::apply_pragmas(&conn).unwrap();
    kb_schema::migrate(&conn).unwrap();
    let run_id = Uuid::new_v4();
    create_run(&conn, &run_id);
    KbContext {
        kb_dir: dir.to_string_lossy().to_string(),
        max_bytes: KB_DEFAULT_MAX_BYTES,
        conn: Arc::new(Mutex::new(conn)),
        run_id,
    }
}

/// Insert an analysis_runs row so FK constraints on analysis_run_id pass.
fn create_run(conn: &Connection, run_id: &Uuid) {
    conn.execute(
        "INSERT INTO analysis_runs (id, label, status) VALUES (?1, 'test-run', 'completed')",
        [run_id.to_string()],
    )
    .unwrap();
}

/// Insert a registered document row; returns its id.
fn add_doc(ctx: &KbContext, title: &str, source: &str) -> String {
    let id = Uuid::new_v4().to_string();
    let conn = ctx.conn.lock().unwrap();
    conn.execute(
        "INSERT INTO documents (id, title, source, document_type, version, analysis_status, file_hash) \
         VALUES (?1, ?2, ?3, 'md', '1', 'pending', ?4)",
        params![id, title, source, "hash"],
    )
    .unwrap();
    id
}

/// 1. Migrate creates all tables, views, FTS and triggers.
#[test]
fn migrate_creates_kb_schema() {
    let conn = Connection::open_in_memory().unwrap();
    kb_schema::apply_pragmas(&conn).unwrap();
    kb_schema::migrate(&conn).unwrap();
    for table in kb_schema::KNOWLEDGE_TABLES {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "table {} must exist", table);
    }
    for view in kb_schema::KNOWLEDGE_VIEWS {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name = ?1",
                [view],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "view {} must exist", view);
    }
    // FTS external content: inserting a unit flows into units_fts via trigger.
    let doc = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO documents (id, title, source, version) VALUES (?1, 't', 'data/t.md', '1')",
        [&doc],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO document_units (id, document_id, unit_type, text, position) \
         VALUES (?1, ?2, 'paragraph', '日本語のテスト文章', 0)",
        params![Uuid::new_v4().to_string(), doc],
    )
    .unwrap();
    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM units_fts WHERE text != ''", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fts_rows, 1, "AFTER INSERT trigger must index the unit text");
}

/// 2. kb_insert: local ref resolution entity -> claim.subject_ref -> evidence.target_ref.
#[test]
fn insert_resolves_local_refs() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");
    let unit = Uuid::new_v4().to_string();
    ctx.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO document_units (id, document_id, unit_type, text, position) \
             VALUES (?1, ?2, 'paragraph', '甲社は製品Aを販売する', 0)",
            params![unit, doc],
        )
        .unwrap();

    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "entities": [
                { "ref": "e1", "name": "甲社", "entity_type": "organization" },
                { "ref": "e2", "name": "製品A", "entity_type": "product" }
            ],
            "claims": [
                { "ref": "c1", "subject_ref": "e1", "predicate": "売却", "object_ref": "e2", "modality": "assertion" }
            ],
            "evidence": [
                { "target_type": "claim", "target_ref": "c1", "source_unit_id": unit, "matched_text": "甲社は製品Aを販売する" }
            ]
        }),
    )
    .unwrap();
    assert_eq!(res["total"].as_u64(), Some(4));

    let e1 = res["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let e2 = res["inserted"]["entities"][1]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let c1 = res["inserted"]["claims"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Verify resolved references (guard dropped before later insert calls).
    {
        let conn = ctx.conn.lock().unwrap();
        let subject_id: String = conn
            .query_row("SELECT subject_id FROM claims WHERE id = ?1", [&c1], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(subject_id, e1);
        let object_id: String = conn
            .query_row("SELECT object_id FROM claims WHERE id = ?1", [&c1], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(object_id, e2);
        let target: String = conn
            .query_row(
                "SELECT target_id FROM evidence WHERE target_type='claim'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(target, c1);
    }

    // Both id and ref -> KB_CONFLICT (and atomic rollback: nothing inserted).
    let err = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [{ "subject_id": e1, "subject_ref": "e1", "predicate": "x" }]
        }),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("[KB_CONFLICT]"), "{}", err);
}

/// Re-analysis must not create duplicate entity rows: inserting the same
/// (document, name_norm, entity_type) reuses the existing entity id.
#[test]
fn insert_reuses_existing_entity() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "entities": [{ "ref": "e1", "name": "甲社", "entity_type": "organization" }]
        }),
    )
    .unwrap();
    let first_id = res["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        res["inserted"]["entities"][0]["reused"].as_bool(),
        Some(false)
    );

    // Same name + type -> reuse (no new row, reused=true).
    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "entities": [{ "ref": "e1", "name": "甲社", "entity_type": "organization" }]
        }),
    )
    .unwrap();
    let second_id = res["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(second_id, first_id);
    assert_eq!(
        res["inserted"]["entities"][0]["reused"].as_bool(),
        Some(true)
    );

    // Different type -> NOT deduped (a distinct entity row).
    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "entities": [{ "ref": "e2", "name": "甲社", "entity_type": "product" }]
        }),
    )
    .unwrap();
    let third_id = res["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(third_id, first_id);

    let count: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE obsolete = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

/// Re-analysis must not create duplicate claim rows: inserting the same
/// (document, fingerprint) reuses the existing claim id.
#[test]
fn insert_reuses_existing_claim() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    let insert = || {
        execute_kb_insert(
            &ctx,
            &json!({
                "document_id": doc,
                "claims": [{ "ref": "c1", "subject_value": "A", "predicate": "is", "object_value": "B" }],
                "evidence": [{ "target_type": "claim", "target_ref": "c1", "matched_text": "A is B" }]
            }),
        )
        .unwrap()
    };

    let res = insert();
    let first_id = res["inserted"]["claims"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        res["inserted"]["claims"][0]["reused"].as_bool(),
        Some(false)
    );

    let res = insert();
    let second_id = res["inserted"]["claims"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(second_id, first_id);
    assert_eq!(res["inserted"]["claims"][0]["reused"].as_bool(), Some(true));

    let count: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM claims WHERE obsolete = 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

/// Re-analysis must not create duplicate relation / condition / event rows:
/// re-inserting the same dedup key reuses the existing row.
#[test]
fn insert_reuses_existing_relation_condition_event() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    let r = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "entities": [
                { "ref": "e1", "name": "甲社" },
                { "ref": "e2", "name": "乙社" }
            ]
        }),
    )
    .unwrap();
    let e1 = r["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let e2 = r["inserted"]["entities"][1]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A claim to serve as the condition target.
    let rc = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [{ "ref": "c1", "subject_value": "A", "predicate": "is", "object_value": "B" }],
            "evidence": [{ "target_type": "claim", "target_ref": "c1", "matched_text": "A is B" }]
        }),
    )
    .unwrap();
    let c1 = rc["inserted"]["claims"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let insert = || {
        execute_kb_insert(
            &ctx,
            &json!({
                "document_id": doc,
                "relations": [{
                    "ref": "r1", "relation_type": "causes",
                    "source_type": "entity", "source_id": e1,
                    "target_type": "entity", "target_id": e2
                }],
                "conditions": [{
                    "target_type": "claim", "target_id": c1,
                    "condition_type": "exception",
                    "expression": { "op": "raw", "text": "unless X" }
                }],
                "events": [{
                    "ref": "ev1", "event_type": "publication",
                    "subject_id": e1, "sort_key": "2024-00-00"
                }],
                "evidence": [
                    { "target_type": "relation", "target_ref": "r1", "matched_text": "x" },
                    { "target_type": "event", "target_ref": "ev1", "matched_text": "x" }
                ]
            }),
        )
        .unwrap()
    };

    let first = insert();
    let rel_id = first["inserted"]["relations"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ev_id = first["inserted"]["events"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let cond_id = first["inserted"]["conditions"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let second = insert();
    assert_eq!(
        second["inserted"]["relations"][0]["reused"].as_bool(),
        Some(true)
    );
    assert_eq!(
        second["inserted"]["relations"][0]["id"].as_str().unwrap(),
        rel_id
    );
    assert_eq!(
        second["inserted"]["events"][0]["reused"].as_bool(),
        Some(true)
    );
    assert_eq!(
        second["inserted"]["events"][0]["id"].as_str().unwrap(),
        ev_id
    );
    assert_eq!(
        second["inserted"]["conditions"][0]["reused"].as_bool(),
        Some(true)
    );
    assert_eq!(
        second["inserted"]["conditions"][0]["id"].as_str().unwrap(),
        cond_id
    );

    let conn = ctx.conn.lock().unwrap();
    let count = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count("relations"), 1);
    assert_eq!(count("events"), 1);
    assert_eq!(count("conditions"), 1);
}

/// Conditions with the same target+type but different expressions must not be
/// merged into one row (expression is part of the dedup key).
#[test]
fn condition_dedup_distinguishes_expression() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    let rc = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [{ "ref": "c1", "subject_value": "A", "predicate": "is", "object_value": "B" }],
            "evidence": [{ "target_type": "claim", "target_ref": "c1", "matched_text": "A is B" }]
        }),
    )
    .unwrap();
    let c1 = rc["inserted"]["claims"][0]["id"].as_str().unwrap().to_string();

    execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "conditions": [
                { "target_type": "claim", "target_id": c1, "condition_type": "exception", "expression": { "text": "unless X" } },
                { "target_type": "claim", "target_id": c1, "condition_type": "exception", "expression": { "text": "unless Y" } }
            ]
        }),
    )
    .unwrap();

    let n: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM conditions WHERE obsolete = 0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "different expressions must not be merged");
}

/// Claims with the same subject/predicate/object/modality but different
/// polarity must not be deduplicated (polarity is part of the fingerprint).
#[test]
fn claim_dedup_distinguishes_polarity() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [
                { "ref": "c1", "subject_value": "method", "predicate": "is idempotent", "object_value": "GET", "modality": "assertion", "polarity": "positive" },
                { "ref": "c2", "subject_value": "method", "predicate": "is idempotent", "object_value": "GET", "modality": "assertion", "polarity": "negative" }
            ],
            "evidence": [
                { "target_type": "claim", "target_ref": "c1", "matched_text": "GET is idempotent" },
                { "target_type": "claim", "target_ref": "c2", "matched_text": "GET is not idempotent" }
            ]
        }),
    )
    .unwrap();

    let n: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM claims WHERE obsolete = 0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "positive and negative claims must both persist");
}

/// Events with the same subject+type+sort_key but different start_time must
/// not be merged (start/end time is part of the dedup key).
#[test]
fn event_dedup_distinguishes_time() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    let re = execute_kb_insert(
        &ctx,
        &json!({ "document_id": doc, "entities": [{ "ref": "e1", "name": "RFC 9110" }] }),
    )
    .unwrap();
    let e1 = re["inserted"]["entities"][0]["id"].as_str().unwrap().to_string();

    execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "events": [
                { "ref": "ev1", "event_type": "publication", "subject_id": e1, "start_time": "2026-06-01", "sort_key": "2026-00-00" },
                { "ref": "ev2", "event_type": "publication", "subject_id": e1, "start_time": "2026-07-01", "sort_key": "2026-00-00" }
            ],
            "evidence": [
                { "target_type": "event", "target_ref": "ev1", "matched_text": "June" },
                { "target_type": "event", "target_ref": "ev2", "matched_text": "July" }
            ]
        }),
    )
    .unwrap();

    let n: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM events WHERE obsolete = 0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "events with different start_time must not be merged");
}

/// Every inserted claim / relation / event must be backed by an evidence row
/// in the same call (new rows) or already exist (reused rows). A missing ref
/// means a new row cannot be evidenced and is rejected.
#[test]
fn insert_requires_evidence() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    // Claim with ref but no evidence.
    let err = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [{ "ref": "c1", "subject_value": "A", "predicate": "is", "object_value": "B" }]
        }),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("[KB_EVIDENCE_REQUIRED]"), "{}", err);

    // Claim without ref and without evidence is rejected too (nothing to evidence).
    let err = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [{ "subject_value": "A", "predicate": "is", "object_value": "B" }]
        }),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("[KB_EVIDENCE_REQUIRED]"), "{}", err);

    // Relation with ref but no evidence.
    let err = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "entities": [{ "ref": "e1", "name": "甲社" }, { "ref": "e2", "name": "乙社" }],
            "relations": [{ "ref": "r1", "relation_type": "causes", "source_type": "entity", "source_ref": "e1", "target_type": "entity", "target_ref": "e2" }]
        }),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("[KB_EVIDENCE_REQUIRED]"), "{}", err);

    // Claim with ref + evidence succeeds.
    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [{ "ref": "c1", "subject_value": "A", "predicate": "is", "object_value": "B" }],
            "evidence": [{ "target_type": "claim", "target_ref": "c1", "matched_text": "A is B" }]
        }),
    )
    .unwrap();
    assert_eq!(res["total"].as_u64(), Some(2));
}

/// Evidence can be attached to an existing row by target_id in a later call
/// (remediation); the row then satisfies the evidence requirement.
#[test]
fn evidence_by_target_id_attaches_to_existing_row() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [{ "ref": "c1", "subject_value": "A", "predicate": "is", "object_value": "B" }],
            "evidence": [{ "target_type": "claim", "target_ref": "c1", "matched_text": "A is B" }]
        }),
    )
    .unwrap();
    let c1 = res["inserted"]["claims"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A second, evidence-only call targeting the existing claim by id.
    execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "evidence": [{ "target_type": "claim", "target_id": c1, "matched_text": "A is B again" }]
        }),
    )
    .unwrap();

    let n: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM evidence WHERE target_type = 'claim' AND target_id = ?1",
            [&c1],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2);
}

/// A cross-document relation's evidence derives its document_id from the
/// source unit, so cross-doc relations can carry evidence too.
#[test]
fn cross_doc_relation_evidence_derives_document_id() {
    let ctx = mem_ctx();
    let a = add_doc(&ctx, "a", "data/a.md");
    let b = add_doc(&ctx, "b", "data/b.md");

    let ra = execute_kb_insert(
        &ctx,
        &json!({ "document_id": a, "entities": [{ "ref": "e1", "name": "甲社" }] }),
    )
    .unwrap();
    let e1 = ra["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let rb = execute_kb_insert(
        &ctx,
        &json!({ "document_id": b, "entities": [{ "ref": "e2", "name": "乙社" }] }),
    )
    .unwrap();
    let e2 = rb["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let unit = Uuid::new_v4().to_string();
    {
        let conn = ctx.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO document_units (id, document_id, unit_type, text, position) \
             VALUES (?1, ?2, 'paragraph', '甲社が乙社を買収', 0)",
            params![unit, a],
        )
        .unwrap();
    }

    execute_kb_insert(
        &ctx,
        &json!({
            "document_id": "null",
            "relations": [{
                "ref": "r1",
                "relation_type": "causes",
                "source_type": "entity", "source_id": e1,
                "target_type": "entity", "target_id": e2
            }],
            "evidence": [{
                "target_type": "relation", "target_ref": "r1",
                "source_unit_id": unit, "matched_text": "甲社が乙社を買収"
            }]
        }),
    )
    .unwrap();

    let conn = ctx.conn.lock().unwrap();
    let (ev_doc, rel_doc): (String, Option<String>) = conn
        .query_row(
            "SELECT e.document_id, r.document_id FROM evidence e \
             JOIN relations r ON r.id = e.target_id \
             WHERE e.target_type = 'relation' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        ev_doc, a,
        "evidence.document_id must derive from the source unit"
    );
    assert_eq!(rel_doc, None, "cross-doc relation document_id is NULL");
}

/// kb_insert can create canonical entities and link document entities to them
/// (cross-document identity resolution), with dedup on name/type and link.
#[test]
fn insert_canonical_entities_and_entity_links() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "entities": [{ "ref": "e1", "name": "HTTP", "entity_type": "protocol" }],
            "canonical_entities": [{ "ref": "ce1", "name": "HTTP", "entity_type": "protocol", "aliases": ["HyperText Transfer Protocol"] }],
            "entity_links": [{ "entity_ref": "e1", "canonical_ref": "ce1", "confidence": 0.9 }]
        }),
    )
    .unwrap();

    let e1 = res["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ce1 = res["inserted"]["canonical_entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let link = res["inserted"]["entity_links"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    {
        let conn = ctx.conn.lock().unwrap();
        let (entity_id, canonical_id): (String, String) = conn
            .query_row(
                "SELECT entity_id, canonical_id FROM entity_links WHERE id = ?1",
                [&link],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(entity_id, e1);
        assert_eq!(canonical_id, ce1);
    }

    // Re-inserting the same canonical entity reuses it.
    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "canonical_entities": [{ "ref": "ce2", "name": "HTTP", "entity_type": "protocol" }]
        }),
    )
    .unwrap();
    let ce2 = res["inserted"]["canonical_entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(ce2, ce1);
    assert_eq!(
        res["inserted"]["canonical_entities"][0]["reused"].as_bool(),
        Some(true)
    );

    // Re-linking the same entity->canonical reuses the link.
    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "entity_links": [{ "entity_id": e1, "canonical_id": ce1 }]
        }),
    )
    .unwrap();
    let link2 = res["inserted"]["entity_links"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(link2, link);

    let count: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM entity_links", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

/// /kb restore replaces the live DB with a snapshot and reopens the connection.
#[test]
fn restore_replaces_live_db_with_snapshot() {
    let ctx = file_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    execute_kb_insert(
        &ctx,
        &json!({ "document_id": doc, "entities": [{ "name": "甲社" }] }),
    )
    .unwrap();

    // Snapshot the DB.
    let snapshot = std::env::temp_dir().join(format!("kb-snapshot-{}.sqlite", Uuid::new_v4()));
    {
        let conn = ctx.conn.lock().unwrap();
        conn.execute_batch(&format!(
            "VACUUM INTO '{}'",
            snapshot.to_string_lossy().replace('\'', "''")
        ))
        .unwrap();
    }

    // Diverge from the snapshot.
    execute_kb_insert(
        &ctx,
        &json!({ "document_id": doc, "entities": [{ "name": "乙社" }] }),
    )
    .unwrap();

    let msg = kb_restore(&ctx, snapshot.to_str().unwrap()).unwrap();
    assert!(msg.contains("Restored"), "{}", msg);

    let count: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "restore must revert to the snapshot state");
}

/// Current-version views exclude rows belonging to superseded document
/// versions (and obsolete rows); cross-document relations always stay.
#[test]
fn current_views_exclude_superseded_documents() {
    let ctx = mem_ctx();
    let old = add_doc(&ctx, "old", "data/old.md");
    let cur = add_doc(&ctx, "cur", "data/cur.md");

    {
        let conn = ctx.conn.lock().unwrap();
        // Superseded document: one entity + one claim.
        conn.execute(
            "INSERT INTO entities (id, document_id, name, name_norm) \
             VALUES (?1, ?2, 'old-entity', 'old-entity')",
            params![Uuid::new_v4().to_string(), old],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO claims (id, document_id, subject_value, predicate, object_value) \
             VALUES (?1, ?2, 's', 'old-pred', 'o')",
            params![Uuid::new_v4().to_string(), old],
        )
        .unwrap();
        // Current document: one entity + one claim.
        conn.execute(
            "INSERT INTO entities (id, document_id, name, name_norm) \
             VALUES (?1, ?2, 'cur-entity', 'cur-entity')",
            params![Uuid::new_v4().to_string(), cur],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO claims (id, document_id, subject_value, predicate, object_value) \
             VALUES (?1, ?2, 's', 'cur-pred', 'o')",
            params![Uuid::new_v4().to_string(), cur],
        )
        .unwrap();
        // A cross-document relation (document_id NULL) must always be included.
        conn.execute(
            "INSERT INTO relations (id, document_id, source_type, source_id, relation_type, target_type, target_id) \
             VALUES (?1, NULL, 'claim', 'src', 'depends_on', 'claim', 'dst')",
            params![Uuid::new_v4().to_string()],
        )
        .unwrap();
        // A relation tied to the superseded document must be excluded.
        conn.execute(
            "INSERT INTO relations (id, document_id, source_type, source_id, relation_type, target_type, target_id) \
             VALUES (?1, ?2, 'claim', 'src', 'depends_on', 'claim', 'dst')",
            params![Uuid::new_v4().to_string(), old],
        )
        .unwrap();
        // Supersede the old document.
        conn.execute(
            "UPDATE documents SET superseded_by = ?1 WHERE id = ?2",
            params![cur, old],
        )
        .unwrap();
    }

    let conn = ctx.conn.lock().unwrap();
    let entities: i64 = conn
        .query_row("SELECT COUNT(*) FROM v_entities_current", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        entities, 1,
        "superseded document's entities must be excluded"
    );
    let claims: i64 = conn
        .query_row("SELECT COUNT(*) FROM v_claims_current", [], |r| r.get(0))
        .unwrap();
    assert_eq!(claims, 1, "superseded document's claims must be excluded");
    let relations: i64 = conn
        .query_row("SELECT COUNT(*) FROM v_relations_current", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        relations, 1,
        "cross-document relation must stay; superseded-tied relation must go"
    );
}

/// The provenance view `v_claims_with_evidence` must also be current-version
/// only: superseded document claims must not leak into it (v3 migration).
#[test]
fn v_claims_with_evidence_excludes_superseded_documents() {
    let ctx = mem_ctx();
    let old = add_doc(&ctx, "old", "data/old.md");
    let cur = add_doc(&ctx, "cur", "data/cur.md");

    {
        let conn = ctx.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO claims (id, document_id, subject_value, predicate, object_value) \
             VALUES (?1, ?2, 's', 'old-pred', 'o')",
            params![Uuid::new_v4().to_string(), old],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO claims (id, document_id, subject_value, predicate, object_value) \
             VALUES (?1, ?2, 's', 'cur-pred', 'o')",
            params![Uuid::new_v4().to_string(), cur],
        )
        .unwrap();
        conn.execute(
            "UPDATE documents SET superseded_by = ?1 WHERE id = ?2",
            params![cur, old],
        )
        .unwrap();
    }

    let conn = ctx.conn.lock().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM v_claims_with_evidence", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1, "superseded document's claims must be excluded");

    let pred: String = conn
        .query_row("SELECT predicate FROM v_claims_with_evidence", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        pred, "cur-pred",
        "only the current document's claim must remain"
    );
}

/// kb_read resolves a source/title key to the CURRENT version (never a
/// superseded one), for exact source paths, bare filenames, and titles.
#[test]
fn kb_read_resolves_current_version_by_source() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "rfc9110", "data/rfc9110.txt");
    {
        let conn = ctx.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO document_units (id, document_id, unit_type, text, position) \
             VALUES (?1, ?2, 'paragraph', 'The current body', 0)",
            params![Uuid::new_v4().to_string(), doc],
        )
        .unwrap();
    }

    // Exact source path.
    let r = execute_kb_read(&ctx, &json!({ "source": "data/rfc9110.txt" })).unwrap();
    assert_eq!(r["units"][0]["text"].as_str(), Some("The current body"));

    // Bare filename (source suffix).
    let r = execute_kb_read(&ctx, &json!({ "source": "rfc9110.txt" })).unwrap();
    assert_eq!(r["units"][0]["text"].as_str(), Some("The current body"));

    // Title (stem without extension).
    let r = execute_kb_read(&ctx, &json!({ "source": "rfc9110" })).unwrap();
    assert_eq!(r["units"][0]["text"].as_str(), Some("The current body"));

    // A superseded version must never be resolved.
    let v2 = add_doc(&ctx, "rfc9110", "data/rfc9110.txt");
    {
        let conn = ctx.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO document_units (id, document_id, unit_type, text, position) \
             VALUES (?1, ?2, 'paragraph', 'The new body', 0)",
            params![Uuid::new_v4().to_string(), v2],
        )
        .unwrap();
        conn.execute(
            "UPDATE documents SET superseded_by = ?1 WHERE id = ?2",
            params![v2, doc],
        )
        .unwrap();
    }
    let r = execute_kb_read(&ctx, &json!({ "source": "data/rfc9110.txt" })).unwrap();
    assert_eq!(
        r["units"][0]["text"].as_str(),
        Some("The new body"),
        "must resolve to the current (non-superseded) version"
    );
}

/// Ambiguous or invalid source keys return a structured note (candidate list)
/// or a clear error (never a silent guess).
#[test]
fn kb_read_source_reports_ambiguity_and_errors() {
    let ctx = mem_ctx();
    let x = add_doc(&ctx, "notes", "data/x/notes.md");
    let _y = add_doc(&ctx, "notes", "data/y/notes.md");

    // Ambiguous bare filename -> candidate list.
    let err = execute_kb_read(&ctx, &json!({ "source": "notes.md" }))
        .unwrap_err()
        .to_string();
    assert!(err.contains("[KB_AMBIGUOUS]"), "{}", err);
    assert!(
        err.contains("data/x/notes.md") && err.contains("data/y/notes.md"),
        "{}",
        err
    );

    // Unknown key -> structured not-found.
    let err = execute_kb_read(&ctx, &json!({ "source": "missing.md" }))
        .unwrap_err()
        .to_string();
    assert!(err.contains("[KB_NOT_FOUND]"), "{}", err);

    // Both document_id and source -> conflict.
    let err = execute_kb_read(
        &ctx,
        &json!({ "document_id": x, "source": "data/x/notes.md" }),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("[KB_CONFLICT]"), "{}", err);

    // Neither -> missing fields.
    let err = execute_kb_read(&ctx, &json!({})).unwrap_err().to_string();
    assert!(err.contains("[KB_MISSING_FIELDS]"), "{}", err);
}

/// v4 migration adds composite indexes for the hot dedup lookups.
#[test]
fn migrate_v4_adds_dedup_indexes() {
    let ctx = mem_ctx();
    let conn = ctx.conn.lock().unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 4);
    for name in [
        "idx_entities_doc_norm_type",
        "idx_claims_doc_fingerprint",
        "idx_canonical_entities_name_type",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?1",
                [name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "index {} must exist", name);
    }
}

/// 3. kb_update: annotations versioned; current value = latest version.
#[test]
fn update_appends_annotation_versions() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");
    let id = Uuid::new_v4().to_string();
    ctx.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO entities (id, document_id, name) VALUES (?1, ?2, '甲社')",
            params![id, doc],
        )
        .unwrap();

    let r1 = execute_kb_update(
        &ctx,
        &json!({ "target_type": "entities", "target_id": id, "annotations": { "note": "v1" } }),
    )
    .unwrap();
    assert_eq!(r1["version"].as_i64(), Some(1));
    let r2 = execute_kb_update(
        &ctx,
        &json!({
            "target_type": "entities",
            "target_id": id,
            "annotations": { "note": "v2", "assessment": "good" },
            "reason": "second pass"
        }),
    )
    .unwrap();
    assert_eq!(r2["version"].as_i64(), Some(2));

    let conn = ctx.conn.lock().unwrap();
    let versions: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT version FROM annotation_versions WHERE target_type='entities' AND target_id=?1 ORDER BY version")
            .unwrap();
        let rows = stmt.query_map([&id], |r| r.get::<_, i64>(0)).unwrap();
        rows.flatten().collect()
    };
    assert_eq!(versions, vec![1, 2]);
    let current: String = conn
        .query_row(
            "SELECT annotations FROM entities WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    let cur: Value = serde_json::from_str(&current).unwrap();
    assert_eq!(cur["note"], "v2");
    assert_eq!(cur["assessment"], "good");
}

/// kb_update: `analysis_status` transitions are whitelisted, and a
/// transition to `analyzed` stamps `analyzed_at` (schema column). Invalid
/// values are rejected without persisting anything (tx rollback).
#[test]
fn update_analysis_status_validates_and_stamps_analyzed_at() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");
    let upd = |status: &str| {
        execute_kb_update(
            &ctx,
            &json!({
                "target_type": "documents",
                "target_id": doc,
                "annotations": { "analysis_status": status }
            }),
        )
    };

    upd("analyzing").unwrap();
    upd("analyzed").unwrap();
    let (status, analyzed_at): (String, Option<String>) = {
        // Keep the guard scoped: execute_kb_update below re-locks the
        // connection, which is rejected (re-entrant guard).
        let conn = ctx.conn.lock().unwrap();
        conn.query_row(
            "SELECT analysis_status, analyzed_at FROM documents WHERE id = ?1",
            [&doc],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(status, "analyzed");
    assert!(
        analyzed_at.is_some(),
        "transition to analyzed must stamp analyzed_at"
    );

    // Typo / unknown value: rejected with KB_EXEC_ERROR, nothing persisted.
    let err = upd("analysed").unwrap_err();
    assert!(
        err.to_string().contains("Invalid analysis_status"),
        "unexpected error: {}",
        err
    );
    let conn = ctx.conn.lock().unwrap();
    let (status, analyzed_at): (String, Option<String>) = conn
        .query_row(
            "SELECT analysis_status, analyzed_at FROM documents WHERE id = ?1",
            [&doc],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "analyzed");
    assert!(analyzed_at.is_some());
}

/// The lock guard rejects re-entrant acquisition with a clear error instead
/// of deadlocking (std::sync::Mutex is not reentrant; this used to hang
/// forever).
#[test]
fn reentrant_conn_lock_fails_loudly() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");
    let _conn = lock_conn(&ctx).unwrap();
    let err = execute_kb_update(
        &ctx,
        &json!({
            "target_type": "documents",
            "target_id": doc,
            "annotations": { "analysis_status": "analyzed" }
        }),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("Re-entrant acquisition"),
        "unexpected error: {}",
        err
    );
}

/// 4. Read-only sanitize: SELECT ok; DELETE / PRAGMA / multi-statement rejected.
#[test]
fn search_sanitizes_readonly() {
    let ctx = mem_ctx();
    let ok = execute_kb_search(&ctx, "SELECT 1").unwrap();
    assert_eq!(ok["content"].as_str().unwrap().lines().next().unwrap(), "1");

    for bad in [
        "DELETE FROM entities",
        "PRAGMA user_version",
        "VACUUM INTO 'x'",
    ] {
        let err = execute_kb_search(&ctx, bad).unwrap_err().to_string();
        assert!(err.contains("[KB_READONLY_VIOLATION]"), "{}: {}", bad, err);
    }
    // Multi-statement injection is structurally rejected by prepare.
    let err = execute_kb_search(&ctx, "SELECT 1; DROP TABLE documents")
        .unwrap_err()
        .to_string();
    assert!(err.contains("[KB_SYNTAX_ERROR]"), "{}", err);
}

/// Data-modifying statements prefixed with WITH (e.g. `WITH ... DELETE ...
/// RETURNING`) start with the keyword `WITH`, so the keyword sanitizer alone
/// lets them through. `readonly()` must reject them at prepare time.
#[test]
fn search_rejects_data_modifying_cte() {
    let ctx = mem_ctx();

    // Read-only CTE still works.
    let ok = execute_kb_search(&ctx, "WITH c AS (SELECT 1 AS one) SELECT one FROM c").unwrap();
    let csv = ok["content"].as_str().unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "one",
        "CSV header must name the column"
    );
    assert_eq!(
        lines.next().unwrap(),
        "1",
        "CSV data row must hold the value"
    );

    // EXPLAIN remains allowed (it never executes).
    let ok = execute_kb_search(&ctx, "EXPLAIN SELECT 1").unwrap();
    assert!(!ok["content"].as_str().unwrap().is_empty());

    // Data-modifying CTEs must be rejected as write operations.
    for bad in [
        "WITH c AS (SELECT 1 AS one) DELETE FROM entities WHERE 1 = (SELECT one FROM c) RETURNING *",
        "WITH c AS (SELECT 1 AS one) UPDATE documents SET title = 'x' WHERE 1 = (SELECT one FROM c) RETURNING *",
        "WITH c AS (SELECT 1 AS one) INSERT INTO documents (id, title, source, version) SELECT 'a','b','c','1' FROM c RETURNING *",
    ] {
        let err = execute_kb_search(&ctx, bad).unwrap_err().to_string();
        assert!(err.contains("[KB_READONLY_VIOLATION]"), "{}: {}", bad, err);
    }
}

/// Truncation keeps whole CSV rows (line boundary) rather than cutting a
/// field mid-row, and appends the [KB_TRUNCATED] notice.
#[test]
fn truncate_body_keeps_whole_rows() {
    let body = "h1,h2\nrow1a,row1b\nrow2a,row2b\n";
    let out = truncate_body(body, 20);
    assert!(
        out.starts_with("h1,h2\nrow1a,row1b\n[KB_TRUNCATED]"),
        "{}",
        out
    );
    assert!(!out.contains("row2a"), "{}", out);

    // No newline before the cap -> UTF-8 boundary fallback.
    let out = truncate_body("abcdef", 3);
    assert!(out.starts_with("abc\n[KB_TRUNCATED]"), "{}", out);
}

/// kb_read returns a document's units in position order, with
/// unit_type / position-range filters and limit/offset paging.
#[test]
fn read_returns_units_in_position_order() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "t", "data/t.md");

    {
        let conn = ctx.conn.lock().unwrap();
        for (i, text) in ["first", "second", "third"].iter().enumerate() {
            conn.execute(
                "INSERT INTO document_units (id, document_id, unit_type, text, position) \
                 VALUES (?1, ?2, 'paragraph', ?3, ?4)",
                params![Uuid::new_v4().to_string(), doc, text, i as i64],
            )
            .unwrap();
        }
    }

    // All units, in position order.
    let out = execute_kb_read(&ctx, &json!({ "document_id": doc })).unwrap();
    let units = out["units"].as_array().unwrap();
    assert_eq!(units.len(), 3);
    assert_eq!(units[0]["text"], "first");
    assert_eq!(units[1]["text"], "second");
    assert_eq!(units[2]["text"], "third");
    assert_eq!(units[0]["position"], 0);
    assert_eq!(units[2]["position"], 2);
    assert_eq!(out["truncated"], false);
    assert_eq!(out["total"], 3, "total must count all matching units");

    // Position range filter (inclusive).
    let out = execute_kb_read(
        &ctx,
        &json!({ "document_id": doc, "start_position": 1, "end_position": 1 }),
    )
    .unwrap();
    let units = out["units"].as_array().unwrap();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0]["text"], "second");

    // unit_type filter matches nothing here (only paragraphs exist).
    let out = execute_kb_read(&ctx, &json!({ "document_id": doc, "unit_type": "page" })).unwrap();
    assert_eq!(out["units"].as_array().unwrap().len(), 0);

    // limit / offset paging.
    let out = execute_kb_read(
        &ctx,
        &json!({ "document_id": doc, "limit": 2, "offset": 1 }),
    )
    .unwrap();
    let units = out["units"].as_array().unwrap();
    assert_eq!(units.len(), 2);
    assert_eq!(units[0]["text"], "second");
    assert_eq!(units[1]["text"], "third");
    assert_eq!(out["total"], 3, "total must ignore limit/offset");
}

/// 5. Deleting a document cascades its rows and removes cross-document
///    relations referencing its endpoints.
#[test]
fn delete_cascades_and_cleans_cross_doc_relations() {
    let ctx = mem_ctx();
    let base = std::path::Path::new(&ctx.kb_dir);
    std::fs::create_dir_all(base.join("data")).unwrap();
    let a = add_doc(&ctx, "a", "data/a.md");
    let b = add_doc(&ctx, "b", "data/b.md");

    let ra = execute_kb_insert(
        &ctx,
        &json!({ "document_id": a, "entities": [{ "ref": "e1", "name": "甲社" }] }),
    )
    .unwrap();
    let e1 = ra["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let rb = execute_kb_insert(
        &ctx,
        &json!({ "document_id": b, "entities": [{ "ref": "e2", "name": "乙社" }] }),
    )
    .unwrap();
    let e2 = rb["inserted"]["entities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A unit in doc a to back the cross-doc relation's evidence.
    let unit = Uuid::new_v4().to_string();
    {
        let conn = ctx.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO document_units (id, document_id, unit_type, text, position) \
             VALUES (?1, ?2, 'paragraph', '甲社が乙社を買収', 0)",
            params![unit, a],
        )
        .unwrap();
    }

    // Cross-document relation (document_id = "null"); evidence derives its
    // document_id from the source unit.
    execute_kb_insert(
        &ctx,
        &json!({
            "document_id": "null",
            "relations": [{
                "ref": "r1",
                "relation_type": "causes",
                "source_type": "entity", "source_id": e1,
                "target_type": "entity", "target_id": e2
            }],
            "evidence": [{
                "target_type": "relation", "target_ref": "r1",
                "source_unit_id": unit, "matched_text": "甲社が乙社を買収"
            }]
        }),
    )
    .unwrap();

    let msg = kb_delete(&ctx, "data/a.md", false).unwrap();
    assert!(msg.contains("Deleted 1 version(s)"));

    let conn = ctx.conn.lock().unwrap();
    let relation_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(relation_count, 0, "cross-doc relation must be removed");
    let ent_a: i64 = conn
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ent_a, 1, "only doc b's entity remains (FK cascade)");
}

/// 6. Run rollback deletes only the run's own rows; other runs' evidence stays.
#[test]
fn run_rollback_keeps_other_runs_references() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");
    let other_run = Uuid::new_v4().to_string();
    ctx.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO analysis_runs (id, status) VALUES (?1, 'completed')",
            [&other_run],
        )
        .unwrap();

    // This run's entity; another run's evidence referencing it.
    let ent = Uuid::new_v4().to_string();
    let ev = Uuid::new_v4().to_string();
    {
        let conn = ctx.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO entities (id, document_id, name, analysis_run_id) VALUES (?1, ?2, '甲社', ?3)",
            params![ent, doc, ctx.run_id.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence (id, document_id, target_type, target_id, analysis_run_id) \
             VALUES (?1, ?2, 'entity', ?3, ?4)",
            params![ev, doc, ent, other_run],
        )
        .unwrap();
    }

    // Rollback = delete this run's rows (spec: delete all rows matching the
    // analysis_run_id).
    ctx.conn
        .lock()
        .unwrap()
        .execute(
            "DELETE FROM entities WHERE analysis_run_id = ?1",
            [ctx.run_id.to_string()],
        )
        .unwrap();

    let conn = ctx.conn.lock().unwrap();
    let ent_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM entities WHERE id = ?1", [&ent], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(ent_count, 0, "run's own row deleted");
    let ev_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence WHERE id = ?1", [&ev], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(ev_count, 1, "other run's evidence referencing it stays");
}

/// 7. units_fts trigram search: >=3 chars MATCH, 1-2 chars LIKE fallback,
///    obsolete rows excluded by join.
#[test]
fn fts_trigram_and_like_fallback() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");
    let u1 = Uuid::new_v4().to_string();
    let u2 = Uuid::new_v4().to_string();
    {
        let conn = ctx.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO document_units (id, document_id, unit_type, text, position, obsolete) \
             VALUES (?1, ?2, 'paragraph', '株式会社テストとアプリ株式会社', 0, 0)",
            params![u1, doc],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO document_units (id, document_id, unit_type, text, position, obsolete) \
             VALUES (?1, ?2, 'paragraph', 'テスト（旧）', 0, 1)",
            params![u2, doc],
        )
        .unwrap();
    }
    // 3+ chars: trigram phrase match, obsolete = 0 only.
    let hits: i64 = execute_kb_search(
        &ctx,
        "SELECT COUNT(*) FROM units_fts f JOIN document_units u ON u.rowid = f.rowid \
         WHERE f.text MATCH '\"テスト\"' AND u.obsolete = 0",
    )
    .unwrap()["content"]
        .as_str()
        .unwrap()
        .lines()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        hits, 1,
        "trigram phrase must hit only the non-obsolete unit"
    );

    // 1-2 chars: LIKE fallback on document_units.text with obsolete = 0.
    let like_hits: i64 = execute_kb_search(
        &ctx,
        "SELECT COUNT(*) FROM document_units WHERE text LIKE '%社%' AND obsolete = 0",
    )
    .unwrap()["content"]
        .as_str()
        .unwrap()
        .lines()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        like_hits, 1,
        "LIKE fallback must hit only the non-obsolete unit"
    );
}

/// 8. Insert quota and fingerprint auto-generation.
#[test]
fn insert_quota_and_fingerprint() {
    let ctx = mem_ctx();
    let doc = add_doc(&ctx, "doc", "data/doc.md");

    // 501 items -> KB_QUOTA_EXCEEDED (atomic: nothing inserted).
    let mut args = json!({ "document_id": doc, "entities": [] });
    let arr = args["entities"].as_array_mut().unwrap();
    for i in 0..501 {
        arr.push(json!({ "name": format!("e{}", i) }));
    }
    let err = execute_kb_insert(&ctx, &args).unwrap_err().to_string();
    assert!(err.contains("[KB_QUOTA_EXCEEDED]"), "{}", err);

    // fingerprint: distinct claims differ; length is sha256's first 16 hex
    // chars. Identical claims now dedup (covered by insert_reuses_existing_claim).
    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [
                { "ref": "a", "predicate": "売却", "subject_value": { "text": "甲社" }, "object_value": { "text": "製品A" } },
                { "ref": "c", "predicate": "売却", "subject_value": { "text": "甲社" }, "object_value": { "text": "製品B" } }
            ],
            "evidence": [
                { "target_type": "claim", "target_ref": "a", "matched_text": "x" },
                { "target_type": "claim", "target_ref": "c", "matched_text": "x" }
            ]
        }),
    )
    .unwrap();
    let ca = res["inserted"]["claims"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let cc = res["inserted"]["claims"][1]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let conn = ctx.conn.lock().unwrap();
    let fp = |id: &str| -> String {
        conn.query_row("SELECT fingerprint FROM claims WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_ne!(fp(&ca), fp(&cc), "different object changes fingerprint");
    assert_eq!(fp(&ca).len(), 16, "fingerprint = sha256 first 16 hex chars");
}

/// 9. /kb add / sync: registration, version update, missing flag, orphan report.
#[test]
fn kb_add_sync_versions_missing_and_orphans() {
    let tmp = std::env::temp_dir().join(format!("kb-sync-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(tmp.join("data")).unwrap();
    std::fs::create_dir_all(tmp.join("db")).unwrap();

    let conn = Connection::open(tmp.join("db/library.sqlite")).unwrap();
    kb_schema::apply_pragmas(&conn).unwrap();
    kb_schema::migrate(&conn).unwrap();
    let run_id = Uuid::new_v4();
    create_run(&conn, &run_id);
    let ctx = KbContext {
        kb_dir: tmp.to_string_lossy().to_string(),
        max_bytes: KB_DEFAULT_MAX_BYTES,
        conn: Arc::new(Mutex::new(conn)),
        run_id,
    };

    // Registration (file outside data/ is copied in).
    let src = tmp.join("..").join("kb-src.md");
    std::fs::write(&src, "第1段落\n\n第2段落").unwrap();
    let msg = kb_add(&ctx, src.to_str().unwrap()).unwrap();
    assert!(msg.contains("registered"), "{}", msg);

    let unit_count: i64 = ctx
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM document_units", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unit_count, 2, "two paragraphs extracted");

    // Version update on content change.
    let in_data = tmp.join("data/kb-src.md");
    std::fs::write(&in_data, "第1段落改訂\n\n第2段落\n\n第3段落").unwrap();
    let sync_msg = kb_sync(&ctx).unwrap();
    assert!(sync_msg.contains("updated"), "{}", sync_msg);
    let current: String = ctx
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT version FROM documents WHERE source='data/kb-src.md' AND superseded_by IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(current, "2", "version bumped to 2");

    // Missing flag after the file disappears.
    std::fs::remove_file(&in_data).unwrap();
    let sync_msg = kb_sync(&ctx).unwrap();
    assert!(sync_msg.contains("added 0"), "{}", sync_msg);
    let metadata: String = ctx
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT metadata FROM documents WHERE source='data/kb-src.md' AND superseded_by IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(metadata.contains("file_missing"), "{}", metadata);

    // Orphan report: a relation with an unknown endpoint.
    ctx.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO relations (id, document_id, source_type, source_id, relation_type, target_type, target_id) \
             VALUES (?1, NULL, 'entity', 'nope', 'depends_on', 'entity', 'nope2')",
            [Uuid::new_v4().to_string()],
        )
        .unwrap();
    let report = orphan_report(&ctx).unwrap();
    assert!(report.contains("relations.source_id orphan"), "{}", report);

    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_file(&src);
}
