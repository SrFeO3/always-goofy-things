# Local KB Feature

**Local KB** is an optional feature that builds a knowledge base from documents you add; the AI
can search and analyze it. The application splits each file into units (paragraphs / pages); the
AI extracts structured knowledge - entities, claims, relations, conditions, events, with evidence
back to the source - into the knowledge database. The AI accesses the database with four
`data_kb_*` tools, following your investigation instructions.

The feature is optional: build with `--features kb`. It creates a KB folder in the app's data
directory - app-managed, like session/cache data, and **it can grow large**. The location can
be changed: `--kb-dir <dir>` (or `KB_DIR`).

## Feature at a Glance

### How it works

- **`<kb>`** - the KB folder: `<app data>/kb/kb-<workdir>-<hash>/` by default (one per
  working directory).
- **`<kb>/data/`** - your source documents.
- **`<kb>/db/library.sqlite`** - the knowledge database the AI builds (one SQLite file).
- **`data_kb_*` tools** - the AI's interface: `data_kb_search` / `data_kb_schema` (read),
  `data_kb_insert` / `data_kb_update` (write). KB-dedicated: they reach only that one
  `library.sqlite`, never workspace files.
- **`/kb` command** - manage documents: add / list / delete / sync / backup.

### What you need to know

- **Build**: `--features kb`. Without it, none of the above exists.
- **Formats**: text (`.md` / `.txt`, ...) and PDF. Other files are registered, but machine
  extraction fails (`analysis_status = failed`).
- **Size**: the library grows with use (large corpora can reach several GB); keep the app data
  directory on a disk with room to spare.
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
| `--kb-auto-confirm <ro\|rw>` | `KB_AUTO_CONFIRM` | Auto-approve the KB tools: `ro` = reads (`data_kb_search` / `data_kb_schema`), `rw` = all four. Independent of `--unsafe-reflex`. |
| `--kb-max-bytes <num>` | `KB_MAX_BYTES` | Maximum bytes of a `data_kb_search` result before truncation (default: 65536 = 64KB). |

### `--kb-dir` vs `-w, --working-dir`

- `-w <dir>` (env `WORKING_DIR`, default `.`) = the **workspace**: where the ordinary tools
  (`read_file`, ...) operate.
- `--kb-dir <dir>` = the **library folder**: only `data_kb_*` / `/kb` touch it, and only
  `<dir>/db/library.sqlite` - never workspace files.
- Independent: the KB may live inside or outside the workspace
  (`--kb-dir /path/to/my-doc-library`).

### Tool confirmation (`data_kb_*`)

- Reads (`data_kb_search` / `data_kb_schema`) ask `y/N`; auto-approve with `--kb-auto-confirm ro`.
- Writes (`data_kb_insert` / `data_kb_update`) too, with `--kb-auto-confirm rw` (needed in
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
```

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
`KB_DIR`) overrides it. Without the feature, the `data_kb_*` tools and `/kb` are unavailable.

> Tip: write tools ask `y/N` before running - press `y`.

### How analysis works

`/kb add` and `/kb sync` only do **machine extraction** (files -> units, status `pending`); they
never run the LLM, and questions don't change the status either. Only the AI writes it:

- `pending` -> `analyzing` -> `analyzed`: via `data_kb_update` (`target_type` = `documents`, key
  `analysis_status`) - a write tool, so it asks `y/N`.
- `failed`: by the application, when extraction fails.

So: to analyze, ask the AI to extract the knowledge, then ask it to set `analysis_status` to
`analyzed`. No `/kb` command does this.

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

Ask the AI to extract the document's knowledge:

```text
Look at the knowledge database schema first, then read rfc9110.txt and register its main terms and
concepts as entities and claims, with evidence pointing back to the source text. Distinguish
modality (fact vs assertion).
```

Approve the writes with `y` (`data_kb_insert` is a write tool). When it is done, close the
document:

```text
The analysis of rfc9110.txt is complete. Set analysis_status to analyzed for that document.
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

> Vague answer? Tell the AI: "Call `data_kb_schema` first, then search with `data_kb_search`"
> (it may answer from memory). If the database itself is sparse, the analysis (1-3) was incomplete.

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

As in Pattern 1: ask the AI to register 9111's concepts as entities and claims with evidence,
then set `analysis_status` to `analyzed`.

#### 2-3. Connect the two documents

The AI can add **cross-document relations** (a relation whose `document_id` is `"null"`). Ask
explicitly for the links you want:

```text
Across rfc9110.txt and rfc9111.txt, connect what 9111 depends on from 9110 using relations
(depends_on, document_id="null"), with evidence for each link.
```

```text
If the two documents contradict each other, add relations (contradicts). If not, report
"no contradictions" with your reasoning.
```

```text
Resolve the same things that appear in both documents (e.g. methods and headers) using
canonical_entities / entity_links. If something is ambiguous, don't link it - record it in
annotations instead.
```

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

- Cross-document identity resolution is **lazy**: it runs when first needed, and the result is
  saved and reused.
- Uncertain links are not forced; they are recorded in `annotations` (nothing is lost, history is
  kept).

## Troubleshooting

| Symptom | Fix |
|---|---|
| `/kb` or `data_kb_*` are missing | Build with `--features kb` (the KB then lives in the app data dir by default; `--kb-dir <dir>` / `KB_DIR` to choose a location) |
| `/kb list` still shows `pending` | Expected: ask the AI to extract the knowledge, then to set `analysis_status` to `analyzed` (How analysis works). No `/kb` command does this |
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
| `analysis_runs` | one row per analysis session |
| `v_documents_current` | view: current versions only |
| `v_claims_with_evidence` | view: claims joined to their evidence + source text |
| `units_fts` | full-text index over `document_units.text` (trigram) |

`attributes` / `annotations` / `metadata` are JSON text - read them with `json_extract(...)`.
Rows are never overwritten: corrections mark the old row `obsolete = 1`, so add `WHERE obsolete = 0`
(the two views already do this).

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

This is the app's database: explore it read-only, and manage it with `/kb` (the AI writes through
`data_kb_*` tools). Don't edit rows by hand - that would break the provenance and version history.