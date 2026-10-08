# Local KB Feature

> Extraction is now app-driven: `/kb extract` processes documents chunk by chunk (the old
> ask-the-AI-to-analyze flow is gone). Questions, corrections, and `/kb` management are unchanged.

| | Extract | Analyze | Ask |
|---|---|---|---|
| Command | `/kb extract` | `/kb analyze "<goal>"` | dialogue |
| Writes | knowledge rows | relations / links | nothing |

**Local KB** is an optional feature that builds a knowledge base from documents you add; the AI
can search and analyze it. The application splits each file into units (paragraphs / pages); the
AI extracts structured knowledge - entities, claims, relations, conditions, events, with evidence
back to the source - into the knowledge database. The AI accesses the database with five
`kb_*` tools, following your investigation instructions.

The feature is optional: build with `--features kb`. It creates a KB folder in the app's data
directory - app-managed, like session/cache data, and **it can grow large**. The location can
be changed: `--kb-dir <dir>` (or `KB_DIR`).

## Feature at a Glance

### How it works

- **`<kb>`** - the KB folder: `<app data>/kb/kb-<workdir>-<hash>/` by default (one per
  working directory).
- **`<kb>/data/`** - your source documents.
- **`<kb>/db/library.sqlite`** - the knowledge database the AI builds (one SQLite file).
- **`kb_*` tools** - the AI's interface: `kb_search` / `kb_schema` / `kb_read` (read),
  `kb_insert` / `kb_update` (write). KB-dedicated: they reach only that one
  `library.sqlite`, never workspace files.
- **`/kb` command** - manage documents: add / list / delete / sync / backup / restore.

### What you need to know

- **Build**: `--features kb`. Without it, none of the above exists.
- **Formats**: PDF and UTF-8 text (`.md` / `.txt`, ...). Non-PDF files are read as plain UTF-8
  text (HTML etc. are not stripped); binary files fail extraction (`analysis_status = failed`).
- **Size**: the library grows with use (large corpora can reach several GB); keep the app data
  directory on a disk with room to spare. Corrections never delete rows (they set `obsolete = 1`),
  so the DB also grows over time - compact it with `/kb backup` (see Backup & restore).
- **Cost**: analyzing a document re-reads its text and emits structured rows, so large corpora
  consume a lot of tokens. Watch `--max-reasoning-turns` and your API budget.
- **Moving the project orphans the library**: the default folder is keyed by the working
  directory's absolute path, so moving or renaming the project starts a fresh empty library and
  leaves the old one on disk (point `--kb-dir` at it to reuse).
- **Cloud sync**: don't put `db/library.sqlite` in a cloud-sync folder (Dropbox / OneDrive, ...)
  - WAL files can get corrupted. Only relevant when you relocate the folder with `--kb-dir`.

### Startup options

All optional - the KB works with none of them:

| Option | Env var | What it does |
|---|---|---|
| `--kb-dir <dir>` | `KB_DIR` | Custom KB folder; unset -> `<app data>/kb/kb-<workdir>-<hash>/`. |
| `--kb-auto-confirm <ro\|rw>` | `KB_AUTO_CONFIRM` | Auto-approve the KB tools: `ro` = reads (`kb_search` / `kb_schema` / `kb_read`), `rw` = all five. Independent of `--unsafe-reflex`. |
| `--kb-max-bytes <num>` | `KB_MAX_BYTES` | Maximum bytes of a `kb_search` / `kb_read` result before truncation (default: 65536 = 64KB). |

### `--kb-dir` vs `-w, --working-dir`

- `-w <dir>` (env `WORKING_DIR`, default `.`) = the **workspace**: where the ordinary tools
  (`read_file`, ...) operate.
- `--kb-dir <dir>` = the **library folder**: only `kb_*` / `/kb` touch it, and only
  `<dir>/db/library.sqlite` - never workspace files.
- Independent: the KB may live inside or outside the workspace
  (`--kb-dir /path/to/my-doc-library`).

### Tool confirmation (`kb_*`)

- Reads (`kb_search` / `kb_schema` / `kb_read`) ask `y/N`; auto-approve with `--kb-auto-confirm ro`.
- Writes (`kb_insert` / `kb_update`) too, with `--kb-auto-confirm rw` (needed in
  batch / todo modes, where stdin is unavailable).
- The KB gate is independent: the global `--unsafe-reflex` never applies to the KB tools.
- `--only-tools` treats them like any other tool: omitted names are hidden from the LLM and
  refused. `/kb` is a CLI command, not a tool - `--only-tools` does not disable it.

### Slash command: `/kb`

```text
/kb add <path>                     Register a file and extract units (copies it in if it's outside data/)
/kb list                           List documents (version, status, analysis, updated time, missing)
/kb delete <path> [--all-versions] Delete the current version, or all versions
/kb sync                           Rescan data/ for added / changed / missing files (also reports broken references)
/kb backup [path]                  Snapshot the knowledge database (VACUUM INTO)
/kb restore <snapshot>             Replace the knowledge database with a snapshot and reopen
/kb extract [<source>] [--chunk-bytes N] [--dry-run] [--status] [--max-retries N] [--redo] [--handover off|auto]
                                    Extract knowledge app-driven, chunk by chunk (see How analysis works)
/kb analyze "<goal>" [--sources ...] [--max-retries N] [--note "..."]
                                    Investigate across documents (dependencies, contradictions, identity)
```

### Backup & restore

`/kb backup [path]` writes a compact snapshot of the knowledge DB via `VACUUM INTO` (default
`<kb>/db/backup/library-<timestamp>.sqlite`). The snapshot is the DB only - it does **not** include
the `data/` source files.

To restore a snapshot while the app is running:

```text
/kb restore my-doc-library/db/backup/library-20260701-120000.sqlite
```

`/kb restore <snapshot>` replaces `<kb>/db/library.sqlite` with the snapshot (removing any `-wal` /
`-shm` sidecars first), reopens the database, and re-applies migrations; the session keeps working on
the restored data. It replaces the DB only - re-run `/kb sync` if the `data/` files changed since the
snapshot.

Manual restore (e.g. the app is not running):

1. Stop the app.
2. Replace `<kb>/db/library.sqlite` with the snapshot (delete any `-wal` / `-shm` files first).
3. Restart with the same `--kb-dir`; re-run `/kb sync` if the `data/` files changed.

The app never `VACUUM`s the live DB, so `/kb backup` is also how you compact a bloated library (the
snapshot is smaller than the live DB, which still holds all the `obsolete` rows).

## Quick Start

This guide walks the shortest path: analyze one document (RFC 9110), add a second (RFC 9111), and
investigate across both.

### Setup

LLM setup: the app's usual (default Ollama at `http://localhost:11434`). Start with Local KB
enabled:

```bash
cargo run --features kb -- --kb-dir my-doc-library
```

You should see the `kb-feature : enabled (run_id: ...)` row in the configuration block, with
`kb-dir` showing the KB folder - `<app data>/kb/kb-<workdir>-<hash>/` unless `--kb-dir` (or
`KB_DIR`) overrides it. Without the feature, the `kb_*` tools and `/kb` are unavailable.

> Tip: write tools ask `y/N` before running - press `y`.

### How analysis works

`/kb add` and `/kb sync` only do **machine extraction** (files -> units, status `pending`); they
never run the LLM. `/kb extract` does the rest as an app-driven job: it splits each pending
document into byte-budget chunks, runs one fresh LLM session per chunk to register knowledge
with evidence, and marks the document `analyzed` once every unit is covered (verified
mechanically, not self-declared). Re-running skips finished chunks; re-registration is
safe: entities, claims, relations, conditions, events, and links are deduplicated.

```text
/kb extract --dry-run      # preview: chunks, sizes, rough token cost (runs nothing)
/kb extract rfc9110.txt    # extract one document
/kb extract                # extract all pending documents
/kb list                   # analyzed when covered
```

Extraction costs time and tokens in proportion to chunk count - check `--dry-run` first.
To lower per-call load, shrink `--chunk-bytes` (more chunks, more calls); to cap cost,
lower `--max-retries`. `--redo` re-runs finished chunks (otherwise they are skipped), and
`--handover auto` carries the previous chunk's summary forward (default `off`: independent).
A document larger than one LLM context still completes: chunks keep each session small,
and the knowledge stays queryable across the whole document afterwards.

Done means every unit backs at least one evidence row (short heading-only units are
accepted with a warning); anything else stays pending.

> **Evidence is mandatory**: every claim / relation / event must be backed by an `evidence` row whose
> `matched_text` is a verbatim excerpt from the source unit. The extract job rejects invented
excerpts and re-reads gaps until each unit is backed. A cross-document relation's evidence needs
> `source_unit_id` (its `document_id` is derived from that unit).

### Pattern 1 - One document

We'll use **RFC 9110 (HTTP Semantics)** from the IETF: freely available, well structured, and full
of precise definitions.

#### 1-1. Download it

In another terminal (same working directory as the app):

```bash
curl -L -o rfc9110.txt https://www.rfc-editor.org/rfc/rfc9110.txt
```

#### 1-2. Add it

```text
/kb add rfc9110.txt
```

The file is copied into `my-doc-library/data/`, registered as version 1 with `analysis_status =
pending`, and split into paragraph units. Check:

```text
/kb list
```

#### 1-3. Analyze it

Extract the document with the job. Approve the writes with `y` (`kb_insert` is a write
tool; batch runs need `--kb-auto-confirm rw`):

```text
/kb extract rfc9110.txt
```

`/kb list` now shows `analyzed`.

#### 1-4. Ask questions about the document

The AI answers from the knowledge database; asking for sources makes answers trustworthy.

```text
How does this document define "safe" and "idempotent" methods?
Search the knowledge database and summarize briefly with sources (which unit each fact comes from).
```

```text
List the claims in rfc9110.txt that have conditions attached, and show each condition's expression.
```

> Vague answer? The AI is instructed to base KB answers on `kb_search` / `kb_schema`
> results and cite the source unit(s) - never memory. If it still drifts, tell it: "Call
> `kb_schema` first, then search with `kb_search`, and cite the source units." If the
> database itself is sparse, the analysis (1-3) was incomplete.

### Pattern 2 - A second document (cross-document)

We'll add **RFC 9111 (HTTP Caching)**: it builds on 9110's definitions (methods, headers) and adds
caching-specific rules, so the pair is ideal for cross-document work: dependencies, contradictions,
and resolving "the same thing" across documents.

#### 2-1. Download and add it

```bash
curl -L -o rfc9111.txt https://www.rfc-editor.org/rfc/rfc9111.txt
```

```text
/kb add rfc9111.txt
/kb list
```

#### 2-2. Analyze it

As in Pattern 1:

```text
/kb extract rfc9111.txt
```

#### 2-3. Connect the two documents

Cross-document work (dependencies, contradictions, shared identity) is its own job:

```text
/kb analyze "connect what 9111 depends on from 9110 (depends_on), flag contradictions, and resolve shared methods and headers"
```

The job plans the investigation, then runs it in fresh sessions: **cross-document relations**
(a relation whose `document_id` is `"null"`; in SQL the column is `NULL`, so use
`document_id IS NULL`), `contradicts` links with per-side evidence, and `canonical_entities` /
`entity_links` identity (ambiguous links go to `annotations`, never forced). Small follow-ups
stay in dialogue.

#### 2-4. Ask across both

```text
How does 9111 define cache freshness, and which definitions in 9110 does it depend on?
Show sources (units) from both documents.
```

```text
Across both documents, list places that could conflict about caching. For each, give the evidence
in each document and the kind of conflict (contradiction / different condition / different scope).
```

Notes:

- Cross-document identity is recorded explicitly: create a `canonical_entities` row and link
  document entities to it with `entity_links` (via `kb_insert`). Re-inserting the same canonical
  entity or link reuses the existing row (dedup).
- Uncertain links are not forced; they are recorded in `annotations` (nothing is lost, history is
  kept).

### Correcting mistakes (obsolete model)

The KB never overwrites or deletes a row on correction. To fix a wrong claim:

1. Find the row id with `kb_search`, e.g.
   `SELECT id, predicate, object_value FROM claims WHERE predicate = '...' AND obsolete = 0`.
2. Insert the corrected claim with `kb_insert` (a fresh row, with evidence).
3. Mark the old row obsolete with `kb_update`
   (`target_type = "claims"`, `target_id = <old id>`, `obsolete = true`).

You can just ask the AI: "The claim that <what it says> is wrong. Insert the corrected claim and
mark the old one obsolete." Re-analysis is also safe: entities are deduplicated (re-inserting the
same name + type reuses the existing entity), relations / conditions / events reuse the existing
row when their dedup key matches, and claims carry a fingerprint you can use to spot
near-duplicates.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `/kb` or `kb_*` are missing | Build with `--features kb` (the KB then lives in the app data dir by default; `--kb-dir <dir>` / `KB_DIR` to choose a location) |
| `/kb list` still shows `pending` | Expected: run `/kb extract` (How analysis works). Questions don't change the status |
| Writes keep pausing | Press `y` in interactive mode. In batch/automation, approve the KB calls with `--kb-auto-confirm ro` (reads) or `rw` (reads + writes); the global `--unsafe-reflex` does not apply to KB tools |
| You changed a document | Replace the file and run `/kb sync` (same content -> skipped; changes -> a new version, old ones kept as history) |
| Remove a document | `/kb delete rfc9110.txt` (current version) or `/kb delete rfc9110.txt --all-versions` |
| Make a backup | `/kb backup` (writes `my-doc-library/db/backup/library-<timestamp>.sqlite` via `VACUUM INTO`) |
| Place files yourself | Put them in `my-doc-library/data/` and run `/kb sync` (or `/kb add <filename>`) |

---

## Appendix - Peek into the knowledge database

You never have to, but you can inspect everything the AI stored. Open the same SQLite file
read-only:

```bash
sqlite3 -readonly my-doc-library/db/library.sqlite
```

(If your `sqlite3` is old, open it normally and run `PRAGMA query_only = ON;` first.)

```sql
.headers on
.mode column
.tables
```

### What's inside

| Table / view | What it holds |
|---|---|
| `documents` | one row per file version (`superseded_by` chains versions; `analysis_status`) |
| `document_units` | the document text, split into paragraphs/pages (the "source" evidence points at) |
| `entities` | people, organizations, products, standards, ... |
| `claims` | subject-predicate-object statements (`modality` = fact / assertion / ..., `polarity`) |
| `relations` | links between items (`causes`, `depends_on`, `contradicts`, ...); `document_id IS NULL` = cross-document |
| `conditions` | conditions attached to a claim (`expression` JSON: threshold / exception / ...) |
| `events` | dated things (with `sort_key` for sorting) |
| `evidence` | provenance: which unit and character offsets back each item |
| `canonical_entities` / `entity_links` | cross-document identity ("the same thing in two documents") |
| `annotation_versions` | history of analysis notes (`annotations`) |
| `analysis_chunks` | extract-job progress (one row per chunk; app-managed, not LLM content) |
| `analysis_runs` | one row per analysis session |
| `v_documents_current` | view: current versions only |
| `v_claims_with_evidence` | view: current-version claims joined to their evidence + source text |
| `v_*_current` | views: current-version knowledge only (`document_units` / `entities` / `claims` / `relations` / `conditions` / `events` / `evidence`); cross-document relations/conditions always included |
| `units_fts` | full-text index over `document_units.text` (trigram) |

`attributes` / `annotations` / `metadata` are JSON text - read them with `json_extract(...)`.
Rows are never overwritten: corrections mark the old row `obsolete = 1`, so add `WHERE obsolete = 0`
(the views already do this). To ignore superseded document versions, query the `v_*_current` views
instead of the base tables.

### A short tour

```sql
-- Documents (current versions only)
SELECT title, source, version, analysis_status FROM v_documents_current;

-- Row counts, to see what the AI actually filled in
SELECT 'entities' AS t, count(*) AS n FROM entities WHERE obsolete = 0
UNION ALL SELECT 'claims',    count(*) FROM claims    WHERE obsolete = 0
UNION ALL SELECT 'relations', count(*) FROM relations WHERE obsolete = 0;

-- Claims of one kind, with where each came from (provenance)
SELECT predicate, modality, polarity, substr(matched_text, 1, 60) AS source
FROM v_claims_with_evidence ORDER BY claim_id LIMIT 20;

-- Current-version claims only (superseded document versions are excluded)
SELECT predicate, modality, substr(object_value, 1, 60) AS object
FROM v_claims_current ORDER BY predicate LIMIT 20;

-- Cross-document links only
SELECT relation_type, source_type, target_type
FROM relations WHERE document_id IS NULL;

-- Conditions (thresholds, exceptions, ...)
SELECT target_type, condition_type, expression
FROM conditions WHERE obsolete = 0 LIMIT 10;

-- The same entity resolved across documents
SELECT ce.name, count(*) AS linked_entities
FROM canonical_entities ce JOIN entity_links el ON el.canonical_id = ce.id
GROUP BY ce.id ORDER BY linked_entities DESC;

-- Full-text search (3+ chars); use LIKE for 1-2 char terms
SELECT u.unit_type, u.position, substr(u.text, 1, 70)
FROM units_fts f JOIN document_units u ON u.rowid = f.rowid
WHERE f.text MATCH '"cache"' AND u.obsolete = 0 LIMIT 5;
```

### Error codes

The KB tools return structured error tags the AI can use to self-correct:

| Tag | Meaning |
|---|---|
| `[KB_MISSING_FIELDS]` | A required argument / item field is missing. |
| `[KB_REF_NOT_FOUND]` | A referenced id does not exist (insert the referenced item first, or fix the id). |
| `[KB_CONFLICT]` | Conflicting inputs (e.g. both `id` and `ref`, or a duplicate local `ref`). |
| `[KB_READONLY_VIOLATION]` | A write was attempted through a read tool (rejected). |
| `[KB_SYNTAX_ERROR]` | The SQL query is malformed. |
| `[KB_EXEC_ERROR]` | The query / update failed (bad table/column, invalid value, ...). |
| `[KB_NOT_FOUND]` | The update target row, or the document resolved from a `source`/title key, does not exist. |
| `[KB_AMBIGUOUS]` | A `source`/title key matches several current documents; the note lists the candidates. |
| `[KB_QUOTA_EXCEEDED]` | Too many items / bytes in one `kb_insert` call (split it). |
| `[KB_EVIDENCE_REQUIRED]` | A claim / relation / event with a local `ref` has no evidence in the same `kb_insert` call. |
| `[KB_FILE_ERROR]` | A `/kb` file operation failed (missing file, collision, ...). |
| `[KB_CONFIG_ERROR]` | KB init / config failed (missing dir, corrupted DB, ...). |
| `[KB_INTERNAL_ERROR]` | Internal invariant violated. |
| `[KB_TRUNCATED]` | Notice (not an error): a result was capped; narrow the query. |

### Tool reference

| Tool | Role | Key arguments |
|---|---|---|
| `kb_search` | read-only SQL | `query` (SELECT/WITH/EXPLAIN only) |
| `kb_schema` | schema discovery | `table` (optional) |
| `kb_read` | read document units | `document_id` (or `source` to auto-resolve the current version), `unit_type` / `start_position` / `end_position` / `limit` / `offset` |
| `kb_insert` | insert knowledge | `document_id`, `entities` / `claims` / `relations` / `conditions` / `events` / `canonical_entities` / `entity_links` / `evidence` |
| `kb_update` | update / obsolete / annotate | `target_type`, `target_id`, `attributes` / `annotations` / `obsolete` / `reason` |

### Expressive schema

Beyond plain facts, the schema records nuance:

- `claims.modality` - fact / assertion / hypothesis / prediction / possibility / requirement /
  recommendation / opinion; separates hard facts from interpretation.
- `claims.polarity` - `positive` / `negative`, for negated statements.
- `conditions` - thresholds / exceptions / temporal / scope attached to a claim or event, with a
  structured `expression`.
- `events` - dated things with `precision` (year / month / day) and a `sort_key` for ordering.
- `confidence` (claims / relations / entity_links) - 0..1. Use it to mark extraction uncertainty
  (1.0 = verbatim fact, lower for paraphrase or inference); when answering, prefer higher
  confidence and flag low-confidence items.
- `annotations` (versioned) - record uncertainty / ambiguity instead of forcing a link; nothing is
  lost.

This is the app's database: explore it read-only, and manage it with `/kb` (the AI writes through
`kb_*` tools). Don't edit rows by hand - that would break the provenance and version history.
