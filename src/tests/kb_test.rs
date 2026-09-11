//! Tests for `src/kb.rs`: the four `data_kb_*` tools and the `/kb` command.
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

/// 2. data_kb_insert: local ref resolution entity -> claim.subject_ref -> evidence.target_ref.
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

/// 3. data_kb_update: annotations versioned; current value = latest version.
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

/// data_kb_update: `analysis_status` transitions are whitelisted, and a
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

/// 5. Deleting a document cascades its rows and removes cross-document
/// relations referencing its endpoints.
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

    // Cross-document relation (document_id = "null").
    execute_kb_insert(
        &ctx,
        &json!({
            "document_id": "null",
            "relations": [{
                "relation_type": "causes",
                "source_type": "entity", "source_id": e1,
                "target_type": "entity", "target_id": e2
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
/// obsolete rows excluded by join.
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

    // fingerprint: identical claims share it; different claims differ.
    let res = execute_kb_insert(
        &ctx,
        &json!({
            "document_id": doc,
            "claims": [
                { "ref": "a", "predicate": "売却", "subject_value": { "text": "甲社" }, "object_value": { "text": "製品A" } },
                { "ref": "b", "predicate": "売却", "subject_value": { "text": "甲社" }, "object_value": { "text": "製品A" } },
                { "ref": "c", "predicate": "売却", "subject_value": { "text": "甲社" }, "object_value": { "text": "製品B" } }
            ]
        }),
    )
    .unwrap();
    let ca = res["inserted"]["claims"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let cb = res["inserted"]["claims"][1]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let cc = res["inserted"]["claims"][2]["id"]
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
    assert_eq!(fp(&ca), fp(&cb), "duplicate claims share fingerprint");
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
