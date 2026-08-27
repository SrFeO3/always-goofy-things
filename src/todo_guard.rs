//! LLM deviation guards for the todo modes.
//!
//! Verifies LLM work and fixes deviations the application can handle
//! mechanically: replan feedback, condensing retries, or safer fallbacks.

use crate::compat_provider::LlmProvider;
use crate::llm_stats::Metrics;
use crate::model::{Session, Settings};
use crate::reasoning::run_reasoning_loop;
use crate::startup;

/// Advertised handover-report char limit shown in the system prompt.
pub(crate) const HANDOVER_REPORT_MAX_CHARS: usize = 300;

/// Enforcement limit (chars): 20% above the advertised limit to tolerate the
/// LLM's unreliable character counting.
pub(crate) const HANDOVER_REPORT_FUZZY_MAX_CHARS: usize = HANDOVER_REPORT_MAX_CHARS * 6 / 5; // 300 * 1.2 = 360

/// Session-context budget (in chars) for the condensing retry.
const LLM_GUARD_CONTEXT_CHARS: usize = 120_000;

/// Collapse a report to a single line (max `HANDOVER_REPORT_FUZZY_MAX_CHARS`)
/// for handover logging.
fn one_line_report(raw: &str) -> String {
    let note: String = raw
        .replace('\n', " ")
        .chars()
        .take(HANDOVER_REPORT_FUZZY_MAX_CHARS)
        .collect();
    if raw.chars().count() > HANDOVER_REPORT_FUZZY_MAX_CHARS {
        format!("{}...", note)
    } else {
        note
    }
}

/// Strip wrapping an LLM may put around a machine-format path: quotes/
/// backticks/edge punctuation, a bracket pair or markdown link, trailing
/// `.`/`。`. No Japanese-prose heuristics; non-conforming text stays as-is.
fn clean_path_token(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    s = s
        .trim_matches(|c| matches!(c, '`' | '"' | '\'' | ',' | ';' | ':'))
        .trim()
        .to_string();
    // Parenthesized wrapper (ASCII chars, so byte indices are boundaries).
    if s.starts_with('(') && s.ends_with(')') && s.len() >= 2 {
        s = s[1..s.len() - 1].trim().to_string();
    }
    // Markdown link `[path](url)` or bracket-wrapped `[path]`.
    if s.starts_with('[') {
        if let Some(close) = s.find("](") {
            s = s[1..close].to_string();
        } else if s.ends_with(']') && s.len() >= 2 {
            s = s[1..s.len() - 1].trim().to_string();
        }
    }
    // Sentence punctuation an LLM may append after a path.
    s.trim_end_matches(['.', '。']).to_string()
}

/// A bullet marker an LLM may use instead of `-` (`*`, `+`, `・`, `•`),
/// plus any following whitespace. None when the line is not a bullet.
fn strip_bullet_marker(line: &str) -> Option<&str> {
    line.strip_prefix(['-', '*', '+', '・', '•'])
        .map(str::trim_start)
}

/// A Tasks-style `[ ]` / `[x]` / `[X]` checkbox decorating a bullet. Its
/// state is ignored (disk existence is the only status); other `[`-lines
/// are left untouched.
fn strip_checkbox(body: &str) -> &str {
    let Some(rest) = body.strip_prefix('[').map(str::trim_start) else {
        return body;
    };
    rest.strip_prefix(']')
        .or_else(|| rest.strip_prefix("x]"))
        .or_else(|| rest.strip_prefix("X]"))
        .map(str::trim_start)
        .unwrap_or(body)
}

/// `artifacts/` paths from a report's `Output:` line (comma-separated).
/// Prefix sloppiness is tolerated - bullet markers, tabs/extra spaces, a
/// glued dash, a space before the colon; `./` is normalized, non-artifacts
/// declarations (`todo.md (updated)`) are prose noise, never returned.
fn extract_output_paths(report: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in report.lines() {
        let trimmed = line.trim();
        let rest = strip_bullet_marker(trimmed).unwrap_or(trimmed).trim_start();
        let Some(rest) = rest.strip_prefix("Output").map(str::trim_start) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start();
        for raw in rest.split(',') {
            let cleaned = clean_path_token(raw);
            if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("none") {
                continue;
            }
            let path = cleaned.strip_prefix("./").unwrap_or(&cleaned);
            if !path.starts_with("artifacts/") {
                continue;
            }
            if !paths.iter().any(|p| p == path) {
                paths.push(path.to_string());
            }
        }
    }
    paths
}

/// Declared `Output:` paths that do not exist on disk.
/// Missing paths are reported to the next replan (Mode 2) or warned about (Mode 1).
pub(crate) fn llm_guard_declared_outputs(report: &str) -> Vec<String> {
    extract_output_paths(report)
        .into_iter()
        .filter(|p| !std::path::Path::new(p).exists())
        .collect()
}

/// Paths an existence check cannot judge (globs, URLs, `~/`, absolute,
/// `..`-escaping, Windows spellings): skipped to avoid false "missing"
/// reports, counted as `(+U unverifiable skipped)`. The declaration gate
/// deliberately treats them as missing so the retry demands concrete paths.
fn is_unverifiable_path(p: &str) -> bool {
    if p.contains('*') || p.contains('?') || p.contains('<') || p.contains('>') || p.contains("://")
    {
        return true;
    }
    if p.starts_with('~') || p.starts_with('/') || p.contains('\\') {
        return true;
    }
    escapes_workspace(p)
}

/// Job-end sweep: declared `outputs:` paths of `- Task` entries in the
/// handover log that still do not exist on disk, as `(task label, path)`
/// pairs (deduped by normalized path, first task wins).
///
/// The `outputs:` line is written by `build_handover_entry` (never
/// truncated), so this is the durable declaration record. `- Planner`
/// entries are excluded (their outputs are plan files, not deliverables).
pub(crate) fn llm_guard_unfinished_outputs(handover_md: &str) -> Vec<(String, String)> {
    let mut current_task: Option<String> = None;
    let mut missing: Vec<(String, String)> = Vec::new();
    for line in handover_md.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- Task ") {
            let num = rest
                .split_whitespace()
                .next()
                .unwrap_or("?")
                .trim_end_matches(':')
                .to_string();
            current_task = Some(format!("Task {}", num));
            continue;
        }
        if trimmed.starts_with("- Planner") {
            current_task = None;
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("outputs:") else {
            continue;
        };
        let Some(task) = &current_task else {
            continue;
        };
        // Reuse the report parser by synthesizing an `Output:` line, so the
        // sweep and build_handover_entry share one syntax.
        for path in extract_output_paths(&format!("Output: {}", rest)) {
            if is_unverifiable_path(&path) {
                continue;
            }
            if std::path::Path::new(&path).exists() {
                continue;
            }
            if !missing.iter().any(|(_, m)| m == &path) {
                missing.push((task.clone(), path));
            }
        }
    }
    missing
}

/// A deliverable is a regular file with content: existence checks answer
/// "does the path exist", deliverable checks "is there a real deliverable";
/// directories and size-0 files are not deliverables.
fn is_deliverable_file(p: &str) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Declared Deliverables-section paths that are not deliverables (missing,
/// zero-sized, or a directory). Metadata-based, so binary artifacts pass
/// on size alone. Empty when the plan has no `## Deliverables` section
/// (no gate).
pub(crate) fn llm_guard_goal_outputs_missing(todo_md: &str) -> Vec<String> {
    extract_deliverables_paths(todo_md)
        .into_iter()
        .filter(|p| {
            if is_unverifiable_path(p) {
                return false;
            }
            !is_deliverable_file(p)
        })
        .collect()
}

/// Handover entry: the one-line report plus an untruncated `outputs:` line
/// when `Output:` declares artifacts paths (prose declarations are never
/// recorded).
pub(crate) fn build_handover_entry(prefix: &str, report: &str) -> String {
    let mut entry = format!("{}: {}", prefix, one_line_report(report));
    let outputs = extract_output_paths(report);
    if !outputs.is_empty() {
        entry.push_str(&format!("\noutputs: {}", outputs.join(", ")));
    }
    entry
}

/// A `..` path component escapes the workspace; a literal one (`a..b.md`)
/// does not.
fn escapes_workspace(p: &str) -> bool {
    p.split('/').any(|component| component == "..")
}

/// The last path component looks like a file name (has an extension).
fn looks_like_file(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .map(|name| name.contains('.') && !name.starts_with('.'))
        .unwrap_or(false)
}

/// A `## Deliverables` heading; extra spaces and trailing section words
/// are tolerated (same rule as the extractor below).
fn is_deliverables_heading(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("##") && trimmed.split_whitespace().nth(1) == Some("Deliverables")
}

/// Whether the plan has a `## Deliverables` section; a plan without one
/// declares no goal deliverables (the completion report says so instead of
/// listing task outputs as deliverables).
pub(crate) fn has_deliverables_section(todo_md: &str) -> bool {
    todo_md.lines().any(is_deliverables_heading)
}

/// Deliverable paths from the `## Deliverables` section - the only
/// machine-verified Goal source. One path per bullet (`- artifacts/<path>`,
/// first token); bullet markers, extra spaces, and Tasks-style checkboxes
/// are tolerated, prose after the path is ignored. `## Goal` prose is never
/// parsed, so a plan without the section declares nothing (no gate).
pub(crate) fn extract_deliverables_paths(todo_md: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut in_deliverables = false;
    for line in todo_md.lines() {
        let trimmed = line.trim();
        if is_deliverables_heading(trimmed) {
            in_deliverables = true;
            continue;
        }
        if in_deliverables && trimmed.starts_with("##") {
            break;
        }
        if !in_deliverables {
            continue;
        }
        // Bullet line: marker (plus optional checkbox), then the path.
        let Some(body) = strip_bullet_marker(trimmed) else {
            continue;
        };
        let body = strip_checkbox(body);
        // First whitespace token only; anything else on the line is ignored.
        let Some(token) = body.split_whitespace().next() else {
            continue;
        };
        let path = clean_path_token(token);
        if path.starts_with("artifacts/")
            && !escapes_workspace(&path)
            && looks_like_file(&path)
            && !paths.iter().any(|p| p == &path)
        {
            paths.push(path);
        }
    }
    paths
}

/// The guard's state files under `artifacts/` (`handover.md` task reports,
/// `calc_ledger.jsonl` calc results): the guard appends to them, so an LLM
/// write would corrupt the record (observed: an executor overwrote
/// `handover.md`, erasing history). Reads stay allowed.
fn is_guard_state_file(path: &str) -> bool {
    let p = path.strip_prefix("./").unwrap_or(path);
    let mut comps = p.split('/').collect::<Vec<_>>();
    let name = comps.pop().unwrap_or("");
    comps.join("/") == "artifacts" && matches!(name, "handover.md" | "calc_ledger.jsonl")
}

/// Mode-2 tool guard: LLM writes to guard state files are refused with a
/// `[TOOL_DENIED]` message; None elsewhere (other modes, reads, paths).
pub(crate) fn llm_guard_state_file_write(name: &str, path: &str, todo_mode: u8) -> Option<String> {
    if todo_mode == 2
        && matches!(name, "write_file" | "str_replace_editor")
        && is_guard_state_file(path)
    {
        Some(format!(
            "[TOOL_DENIED] '{}' is managed by the todo-mode guard (task reports / calc ledger); LLM writes to it are rejected - your report is appended automatically.",
            path
        ))
    } else {
        None
    }
}

/// Verified completion-report lists, split so gated goal deliverables and
/// soft task outputs never mix:
/// - `goal_deliverables`: `## Deliverables` paths (declared order) that
///   exist as non-empty regular files - the job's goal deliverables.
/// - `task_outputs`: verified `outputs:` paths of `- Task` handover entries
///   minus any goal path, so each artifact is reported once, under the
///   gated category. `- Planner` entries are excluded.
pub(crate) fn llm_guard_verified_outputs(
    todo_md: &str,
    handover_md: &str,
) -> (Vec<String>, Vec<String>) {
    let mut goal: Vec<String> = Vec::new();
    let mut tasks: Vec<String> = Vec::new();
    let push = |out: &mut Vec<String>, p: &str| {
        if is_unverifiable_path(p) || !is_deliverable_file(p) {
            return;
        }
        if !out.iter().any(|m| m == p) {
            out.push(p.to_string());
        }
    };
    for path in extract_deliverables_paths(todo_md) {
        push(&mut goal, &path);
    }
    let mut current_task = false;
    for line in handover_md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("- Task ") {
            current_task = true;
            continue;
        }
        if trimmed.starts_with("- Planner") {
            current_task = false;
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("outputs:") else {
            continue;
        };
        if current_task {
            for path in extract_output_paths(&format!("Output: {}", rest)) {
                push(&mut tasks, &path);
            }
        }
    }
    tasks.retain(|p| !goal.iter().any(|g| g == p));
    (goal, tasks)
}

/// `- Task` handover entries whose `outputs:` line declares at least one
/// path; feeds the completion report's zero task-outputs annotation.
/// `- Planner` entries and the seeded template prose are excluded.
pub(crate) fn llm_guard_tasks_declaring_outputs(handover_md: &str) -> usize {
    let mut in_task = false;
    let mut count = 0;
    for line in handover_md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("- Task ") {
            in_task = true;
            continue;
        }
        if trimmed.starts_with("- Planner") {
            in_task = false;
            continue;
        }
        if in_task && let Some(rest) = trimmed.strip_prefix("outputs:") {
            if !extract_output_paths(&format!("Output: {}", rest)).is_empty() {
                count += 1;
            }
            in_task = false;
        }
    }
    count
}

/// Declared paths (Deliverables + task `outputs:` lines) the checks skip as
/// unverifiable; reported as `(+U unverifiable skipped)` so the skip is
/// never silent. Same sources as `llm_guard_verified_outputs`.
pub(crate) fn llm_guard_unverifiable_declared(todo_md: &str, handover_md: &str) -> usize {
    let mut count = 0usize;
    for path in extract_deliverables_paths(todo_md) {
        if is_unverifiable_path(&path) {
            count += 1;
        }
    }
    let mut in_task = false;
    for line in handover_md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("- Task ") {
            in_task = true;
            continue;
        }
        if trimmed.starts_with("- Planner") {
            in_task = false;
            continue;
        }
        if in_task && let Some(rest) = trimmed.strip_prefix("outputs:") {
            for path in extract_output_paths(&format!("Output: {}", rest)) {
                if is_unverifiable_path(&path) {
                    count += 1;
                }
            }
            in_task = false;
        }
    }
    count
}

/// Max number of paths listed per completion-report segment; the remainder
/// is summarized as `(+K more)`.
const COMPLETION_REPORT_MAX_LISTED: usize = 5;

/// Up to `COMPLETION_REPORT_MAX_LISTED` paths joined with ", ", the
/// remainder summarized as `(+K more)`.
fn join_limited(paths: &[String]) -> String {
    let extra = paths.len().saturating_sub(COMPLETION_REPORT_MAX_LISTED);
    let n = paths.len() - extra;
    let mut s = paths[..n].join(", ");
    if extra > 0 {
        s.push_str(&format!(" (+{} more)", extra));
    }
    s
}

/// One mechanical completion line, goal deliverables and task outputs
/// reported separately, never mixed:
/// `OK: all {N} tasks {already }completed; deliverables({M}): p1, ...;
/// task outputs({T}): q1, ...`.
/// `deliverables` are the gated `## Deliverables` paths; `task outputs`
/// are verified `outputs:` declarations beyond those. Zero lists annotate
/// why (`no ## Deliverables section`, `no tasks declared Output paths`);
/// unverifiable declarations are appended as `(+U unverifiable skipped)`.
pub(crate) fn llm_guard_completion_report(
    goal_deliverables: &[String],
    task_outputs: &[String],
    has_section: bool,
    task_total: usize,
    declared_outputs_tasks: usize,
    unverifiable: usize,
    already_done: bool,
) -> String {
    let verb = if already_done {
        "already completed"
    } else {
        "completed"
    };
    let mut line = format!("OK: all {} tasks {}; ", task_total, verb);
    if goal_deliverables.is_empty() {
        line.push_str(if has_section {
            "deliverables(0)"
        } else {
            "deliverables(0; no ## Deliverables section)"
        });
    } else {
        line.push_str(&format!(
            "deliverables({}): {}",
            goal_deliverables.len(),
            join_limited(goal_deliverables)
        ));
    }
    line.push_str("; ");
    if task_outputs.is_empty() {
        let note = if declared_outputs_tasks == 0 {
            "no tasks declared Output paths".to_string()
        } else {
            format!(
                "{} task{} declared Output paths",
                declared_outputs_tasks,
                if declared_outputs_tasks == 1 { "" } else { "s" }
            )
        };
        line.push_str(&format!("task outputs(0; {})", note));
    } else {
        line.push_str(&format!(
            "task outputs({}): {}",
            task_outputs.len(),
            join_limited(task_outputs)
        ));
    }
    if unverifiable > 0 {
        line.push_str(&format!(" (+{} unverifiable skipped)", unverifiable));
    }
    line
}

/// The session's final report: the last assistant message, never a tool or
/// guard-injected message. After `Completed` this equals `messages.last()`;
/// it differs only when a retry stopped mid-tool-call.
pub(crate) fn last_assistant_report(session: &Session) -> Option<&str> {
    session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| m.content.as_str())
}

/// Re-append Output declarations the 300-char condense rewrite may have
/// dropped; they are the durable record for the gate, `outputs:` line, and
/// sweep. No-op when nothing was dropped (`./`-spellings count as equal).
pub(crate) fn merge_condensed_report(original: Option<&str>, condensed: &str) -> String {
    let Some(original) = original else {
        return condensed.to_string();
    };
    let orig_paths = extract_output_paths(original);
    if orig_paths.is_empty() {
        return condensed.to_string();
    }
    let kept: Vec<String> = extract_output_paths(condensed)
        .into_iter()
        .map(|k| k.strip_prefix("./").unwrap_or(&k).to_string())
        .collect();
    let dropped: Vec<String> = orig_paths
        .into_iter()
        .filter(|p| {
            let normalized = p.strip_prefix("./").unwrap_or(p);
            !kept.iter().any(|k| k == normalized)
        })
        .collect();
    if dropped.is_empty() {
        return condensed.to_string();
    }
    let mut merged = condensed.to_string();
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push_str(&format!("- Output: {}", dropped.join(", ")));
    merged
}

/// Ask the LLM once to rewrite an over-long Handover Report (skipped near
/// the context budget; truncation is the fallback).
pub(crate) async fn llm_guard_handover_report(
    config: &startup::Config,
    provider: LlmProvider,
    settings: &mut Settings,
    metrics: &mut Metrics,
    task_session: &mut Session,
) {
    let ctx_chars: usize = task_session
        .messages
        .iter()
        .map(|m| m.content.chars().count())
        .sum();
    let Some(last) = last_assistant_report(task_session) else {
        return;
    };
    if last.chars().count() <= HANDOVER_REPORT_FUZZY_MAX_CHARS
        || ctx_chars >= LLM_GUARD_CONTEXT_CHARS
    {
        return;
    }
    let feedback = format!(
        "Your Handover Report is too long ({} chars > {}). Rewrite it as ONE concise report within {} characters, keeping Status / Output / Findings / Next.",
        last.chars().count(),
        HANDOVER_REPORT_MAX_CHARS,
        HANDOVER_REPORT_MAX_CHARS
    );
    // One retry at most; ignore errors (truncation fallback still applies).
    let _ = run_reasoning_loop(
        config,
        provider,
        task_session,
        settings,
        metrics,
        "todo:guard:condense",
        feedback,
        Vec::new(),
    )
    .await;
}

#[cfg(test)]
#[path = "tests/todo_guard_test.rs"]
mod tests;
