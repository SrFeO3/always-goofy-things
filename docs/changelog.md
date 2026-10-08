# Changelog

Notable changes to a lightweight, experimental agentic CLI application.

## v0.1.0 (2026-06-17) - minimal demo of agentic function loop
Minimal demo of an agentic function loop (under 1000 lines): src/main.rs (368) + src/tools.rs (614). ReAct loop + 6 tools (read_file / str_replace / list_directory / grep_search / bash / fetch_web), human approval on every tool call, Ollama / OpenAI-compatible LLM API.

## v0.1.17 (2026-06-18) - day-1 hardening
Multi-line input, workspace path validation, write_file tool, WORKING_DIR, Ctrl+C interruption, command allowlist / private IP block, token usage display.

## v0.2 (2026-06 mid) - config + pretty
clap-based CLI options + env vars (-w, -u, -m, WORKING_DIR, ...), pretty display mode (-p).

## v0.3 (2026-06 end) - rewind conversation
/rewind slash command.

## v0.4 (2026-07 early) - auto-approval + slash commands
Unsafe reflex auto-approval (deterministic, explainability-focused: literal matching = predictable, human-auditable), path/string/grep rules, /history, /switch model, /restore (JSONL session persistence), /config, unsafe filters (head / tail, fetch), RPM limit, session label, max output token.

## v0.5 (2026-07 mid) - add LLM compat (Anthropic-compatible) + document input
Adding an Anthropic-compatible dialect as the 3rd provider (bloated the code), parallel tool calls, verbose display, fuzzy str_replace completion, @ file attachments (image / audio b64, PDF via pdf_oxide, @@ text extraction), batch mode (-q).

## v0.6.0 (2026-07 end) - simple GUI
Experimental GUI via eframe. Also I/O cleanup: loop refactor (run_reasoning_loop + Session/Settings/Metrics).

## v0.6.5 (2026-08 early) - todo mode: beyond one LLM context
A long job is split into tasks; each task runs in a fresh LLM context; planner replans between tasks, with handover & output-verification guards.

## v0.6.30 (2026-08 mid) - tool expansion
data_search / data_schema, --only-tools, child-process isolation (bash / grep), deterministic calc, LLM usage stats, THIRD_PARTY_LICENSES display.

## v0.6.50 (2026-09 very early) - more LLM compat (OpenAI Responses API-compatible)
Adding OpenAI Responses API compatibility and OpenCode Go via --provider-extras; stable session ID; README / docs refresh.

## v0.7.0 (2026-09 early) - Local KB
Local KB (--features kb): builds a searchable knowledge base from your documents; AI extracts structured knowledge (entities / claims / relations, with evidence to source) into SQLite and answers via kb_* tools. Details: docs/kb.md

## v0.8.0 (2026-09 end) - rework GUI
GUI reworked as a CLI process shell; multi-line input via Ctrl+O in CLI and GUI.

## v0.9.0 (2026-10 early) - rework todo into jobs, app-driven Local KB (BREAKING)
Todo mode (`-t`, `todo.md`) replaced by todo job runs (`/job run <todo.json> [--mode static|replan]`, details: docs/todo-job.md) and app-driven KB extraction (`/kb extract`, `/kb analyze`). BREAKING: `-t` is removed; plans are structured JSON with mechanical verification; job state is temporary (`.todo/`, auto-deleted on success).
