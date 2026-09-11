//! Knowledge Base (KB) schema definitions and migration.
//!
//! Defines the DDL from `work/spec/knowledge-schema.md` (tables, FTS, views,
//! indexes, triggers, PRAGMA setup, and the metadata map behind
//! `data_kb_schema`) and applies it as migrations. Called from `kb.rs`.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// List of every knowledge table (FTS shadow tables and SQLite system tables
/// are excluded by the schema tool automatically).
pub(crate) const KNOWLEDGE_TABLES: &[&str] = &[
    "analysis_runs",
    "documents",
    "document_units",
    "entities",
    "claims",
    "relations",
    "conditions",
    "events",
    "evidence",
    "canonical_entities",
    "entity_links",
    "annotation_versions",
];

/// Provided views exposed to the LLM (`data_kb_schema` lists them and
/// `data_kb_search` may query them).
pub(crate) const KNOWLEDGE_VIEWS: &[&str] = &["v_documents_current", "v_claims_with_evidence"];

/// FTS virtual table name (external content over `document_units`).
pub(crate) const FTS_TABLE: &str = "units_fts";

/// Allowed values for `data_kb_update.target_type` /
/// `annotation_versions.target_type`.
pub(crate) const UPDATE_TARGET_TYPES: &[&str] = &[
    "documents",
    "document_units",
    "entities",
    "claims",
    "relations",
    "conditions",
    "events",
    "evidence",
    "canonical_entities",
];

/// Static metadata map: table/view name -> one-line purpose. This stands in
/// for SQLite comments (SQLite has no comment feature).
pub(crate) fn table_meta() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "analysis_runs",
            "One analysis-process run record (id / status / started_at / finished_at / config / summary)",
        ),
        (
            "documents",
            "Registered files (version chain superseded_by, analysis_status, file_hash, metadata, annotations)",
        ),
        (
            "document_units",
            "Document units (page / paragraph / section, ...). text is immutable; machine extraction leaves analysis_run_id null",
        ),
        (
            "entities",
            "Entities (people, organizations, standards, things). entity_type is loose; name_norm is NFKC-normalized",
        ),
        (
            "claims",
            "Subject-predicate-object statements. modality / polarity; fingerprint auto-generated",
        ),
        (
            "relations",
            "Relation edges (causes / depends_on / contradicts / supports / precedes / qualifies, ...). document_id NULL = cross-document",
        ),
        (
            "conditions",
            "Conditions / prerequisites / exceptions (prerequisite / exception / threshold / temporal / scope). target is a claim or event",
        ),
        (
            "events",
            "Dated events. Ambiguous times: TEXT for display + sort_key + precision",
        ),
        (
            "evidence",
            "Provenance (most important): which unit and offsets each item came from. No attributes",
        ),
        (
            "canonical_entities",
            "Cross-document identity resolution (derived; entity rows are not modified)",
        ),
        (
            "entity_links",
            "entity -> canonical links (derived; cascade-deleted with entity / canonical)",
        ),
        (
            "annotation_versions",
            "Annotation history (current value lives in each table's annotations column; version / reason / created_by)",
        ),
        (
            "v_documents_current",
            "View: current versions only (superseded_by IS NULL)",
        ),
        (
            "v_claims_with_evidence",
            "View: claims + evidence + source-unit excerpt in one query (prevents JOIN mistakes)",
        ),
        (
            "units_fts",
            "FTS5 trigram full-text search over document_units.text (external content). 3+ chars: MATCH \"phrase\"; 1-2 chars: LIKE fallback",
        ),
    ]
}

/// Static notes per table (obsolete semantics, polymorphic references, etc.).
pub(crate) fn table_notes(name: &str) -> Option<&'static str> {
    match name {
        "documents" => Some(
            "Re-registration skips identical content via file_hash (sha256); changed content bumps the version (new row; old row's superseded_by points at it).",
        ),
        "document_units" => Some(
            "text is immutable; fix extraction errors by adding a new unit and marking the old one obsolete. Machine-extracted units UPSERT on (document_id, unit_type, position, parent_id).",
        ),
        "claims" => Some(
            "subject/object: subject_id/object_id when an extracted entity is referenced, otherwise subject_value/object_value. Duplicates are detected mechanically via fingerprint (resolution is the analysis run's call).",
        ),
        "relations" => Some(
            "source_type / target_type allowed: entity / claim / event / document_unit. No FK (polymorphic references); orphan check runs in /kb sync.",
        ),
        "conditions" => Some(
            "target_type allowed: claim / event. expression: {\"op\", \"field\", \"value\", \"unit\"} basic form, or {\"op\": \"raw\", \"text\": ...} when not mechanically comparable. document_id auto-filled on insert.",
        ),
        "events" => Some(
            "subject_id is an entities.id. precision: year / month / day. sort_key example: \"2026-00-00\".",
        ),
        "evidence" => Some(
            "target_type allowed: entity / claim / relation / event / condition. source_unit_id is a document_units.id (NULL = inferred). start/end_offset are character offsets into unit.text.",
        ),
        "entity_links" => Some(
            "UNIQUE(entity_id, canonical_id). No obsolete flag (derived data; cascade-deleted).",
        ),
        "annotation_versions" => Some(
            "target_type allowed: documents / document_units / entities / claims / relations / conditions / events / evidence / canonical_entities.",
        ),
        _ => None,
    }
}

/// Apply the operational PRAGMAs (spec: operational settings). Idempotent.
pub(crate) fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .context("[KB_CONFIG_ERROR] Failed to apply operational PRAGMAs")?;
    Ok(())
}

/// Schema version 1 DDL (tables / FTS / triggers / views / indexes).
const DDL_V1: &str = r#"
CREATE TABLE IF NOT EXISTS analysis_runs (
  id          TEXT PRIMARY KEY,
  label       TEXT,
  status      TEXT NOT NULL DEFAULT 'running',
  started_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  finished_at TEXT,
  config      TEXT NOT NULL DEFAULT '{}',
  summary     TEXT
);

CREATE TABLE IF NOT EXISTS documents (
  id              TEXT PRIMARY KEY,
  title           TEXT NOT NULL,
  source          TEXT,
  document_type   TEXT,
  version         TEXT,
  superseded_by   TEXT,
  analysis_status TEXT NOT NULL DEFAULT 'pending',
  analyzed_at     TEXT,
  file_hash       TEXT,
  created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  metadata        TEXT NOT NULL DEFAULT '{}',
  annotations     TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS document_units (
  id              TEXT PRIMARY KEY,
  document_id     TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  parent_id       TEXT REFERENCES document_units(id),
  unit_type       TEXT NOT NULL,
  text            TEXT NOT NULL,
  position        INTEGER NOT NULL DEFAULT 0,
  metadata        TEXT NOT NULL DEFAULT '{}',
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  obsolete        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS entities (
  id              TEXT PRIMARY KEY,
  document_id     TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  entity_type     TEXT,
  name            TEXT NOT NULL,
  name_norm       TEXT,
  attributes      TEXT NOT NULL DEFAULT '{}',
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  obsolete        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS claims (
  id              TEXT PRIMARY KEY,
  document_id     TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  subject_id      TEXT REFERENCES entities(id),
  subject_value   TEXT,
  predicate       TEXT NOT NULL,
  object_id       TEXT REFERENCES entities(id),
  object_value    TEXT,
  modality        TEXT NOT NULL DEFAULT 'assertion',
  polarity        TEXT NOT NULL DEFAULT 'positive',
  fingerprint     TEXT,
  confidence      REAL,
  attributes      TEXT NOT NULL DEFAULT '{}',
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  obsolete        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS relations (
  id              TEXT PRIMARY KEY,
  document_id     TEXT REFERENCES documents(id) ON DELETE CASCADE,
  source_type     TEXT NOT NULL,
  source_id       TEXT NOT NULL,
  relation_type   TEXT NOT NULL,
  target_type     TEXT NOT NULL,
  target_id       TEXT NOT NULL,
  confidence      REAL,
  attributes      TEXT NOT NULL DEFAULT '{}',
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  obsolete        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS conditions (
  id              TEXT PRIMARY KEY,
  document_id     TEXT REFERENCES documents(id) ON DELETE CASCADE,
  target_type     TEXT NOT NULL,
  target_id       TEXT NOT NULL,
  condition_type  TEXT NOT NULL,
  expression      TEXT NOT NULL DEFAULT '{}',
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  obsolete        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS events (
  id              TEXT PRIMARY KEY,
  document_id     TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  event_type      TEXT,
  subject_id      TEXT REFERENCES entities(id),
  start_time      TEXT,
  end_time        TEXT,
  sort_key        TEXT,
  precision       TEXT NOT NULL DEFAULT 'day',
  attributes      TEXT NOT NULL DEFAULT '{}',
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  obsolete        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS evidence (
  id              TEXT PRIMARY KEY,
  document_id     TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  source_unit_id  TEXT REFERENCES document_units(id),
  target_type     TEXT NOT NULL,
  target_id       TEXT NOT NULL,
  start_offset    INTEGER,
  end_offset      INTEGER,
  matched_text    TEXT,
  evidence_type   TEXT,
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  obsolete        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS canonical_entities (
  id              TEXT PRIMARY KEY,
  name            TEXT NOT NULL,
  entity_type     TEXT,
  aliases         TEXT NOT NULL DEFAULT '[]',
  attributes      TEXT NOT NULL DEFAULT '{}',
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  obsolete        INTEGER NOT NULL DEFAULT 0,
  created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE IF NOT EXISTS entity_links (
  id              TEXT PRIMARY KEY,
  entity_id       TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
  canonical_id    TEXT NOT NULL REFERENCES canonical_entities(id) ON DELETE CASCADE,
  confidence      REAL,
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  UNIQUE(entity_id, canonical_id)
);

CREATE TABLE IF NOT EXISTS annotation_versions (
  id              TEXT PRIMARY KEY,
  target_type     TEXT NOT NULL,
  target_id       TEXT NOT NULL,
  version         INTEGER NOT NULL,
  annotations     TEXT NOT NULL DEFAULT '{}',
  analysis_run_id TEXT REFERENCES analysis_runs(id),
  created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  created_by      TEXT,
  reason          TEXT,
  UNIQUE(target_type, target_id, version)
);

CREATE VIRTUAL TABLE IF NOT EXISTS units_fts USING fts5(
  text,
  content='document_units',
  content_rowid='rowid',
  tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS units_fts_ai AFTER INSERT ON document_units BEGIN
  INSERT INTO units_fts(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TRIGGER IF NOT EXISTS units_fts_ad AFTER DELETE ON document_units BEGIN
  INSERT INTO units_fts(units_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
END;

CREATE INDEX IF NOT EXISTS idx_documents_hash    ON documents(file_hash);
CREATE INDEX IF NOT EXISTS idx_units_doc         ON document_units(document_id, position);
CREATE INDEX IF NOT EXISTS idx_entities_doc      ON entities(document_id);
CREATE INDEX IF NOT EXISTS idx_entities_name     ON entities(name);
CREATE INDEX IF NOT EXISTS idx_entities_norm     ON entities(name_norm);
CREATE INDEX IF NOT EXISTS idx_claims_doc        ON claims(document_id);
CREATE INDEX IF NOT EXISTS idx_claims_subj       ON claims(subject_id);
CREATE INDEX IF NOT EXISTS idx_claims_obj        ON claims(object_id);
CREATE INDEX IF NOT EXISTS idx_claims_fingerprint ON claims(fingerprint);
CREATE INDEX IF NOT EXISTS idx_evidence_unit     ON evidence(source_unit_id);
CREATE INDEX IF NOT EXISTS idx_evidence_target   ON evidence(target_type, target_id);
CREATE INDEX IF NOT EXISTS idx_relations_src     ON relations(source_type, source_id);
CREATE INDEX IF NOT EXISTS idx_relations_dst     ON relations(target_type, target_id);
CREATE INDEX IF NOT EXISTS idx_links_entity      ON entity_links(entity_id);
CREATE INDEX IF NOT EXISTS idx_links_canonical   ON entity_links(canonical_id);
CREATE INDEX IF NOT EXISTS idx_ann_versions      ON annotation_versions(target_type, target_id);
CREATE INDEX IF NOT EXISTS idx_conditions_target ON conditions(target_type, target_id);
CREATE INDEX IF NOT EXISTS idx_conditions_doc    ON conditions(document_id);

CREATE VIEW IF NOT EXISTS v_documents_current AS
SELECT * FROM documents WHERE superseded_by IS NULL;

CREATE VIEW IF NOT EXISTS v_claims_with_evidence AS
SELECT c.id            AS claim_id,
       c.document_id,
       c.subject_id,
       c.subject_value,
       c.object_id,
       c.object_value,
       c.predicate,
       c.modality,
       c.polarity,
       c.confidence,
       c.annotations,
       e.id            AS evidence_id,
       e.start_offset,
       e.end_offset,
       e.matched_text,
       u.id            AS source_unit_id,
       u.unit_type,
       u.position,
       u.text          AS unit_text
FROM claims c
LEFT JOIN evidence e  ON e.target_type = 'claim' AND e.target_id = c.id AND e.obsolete = 0
LEFT JOIN document_units u ON u.id = e.source_unit_id AND u.obsolete = 0
WHERE c.obsolete = 0;
"#;

/// Run pending schema migrations (idempotent; starts at user_version = 0).
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("[KB_CONFIG_ERROR] Failed to read PRAGMA user_version")?;
    if version < 1 {
        conn.execute_batch(DDL_V1)
            .context("[KB_CONFIG_ERROR] Failed to apply KB schema v1")?;
        // Note: triggers reference document_units; units_fts must not be
        // rebuilt here (empty DB). Data inserted later flows through the
        // AFTER INSERT trigger automatically.
        conn.execute_batch("PRAGMA user_version = 1;")
            .context("[KB_CONFIG_ERROR] Failed to set PRAGMA user_version = 1")?;
    }
    Ok(())
}
