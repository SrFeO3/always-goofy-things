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

/// Extract the file paths declared in a raw report's `Output:` field.
/// Accepts `- Output:` / `Output:` prefixes, punctuation/backticks/quotes,
/// markdown links, trailing `.`/`。`, comma lists, and `none`.
fn extract_output_paths(report: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in report.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("- Output:")
            .or_else(|| trimmed.strip_prefix("Output:"))
            .map(str::trim)
        else {
            continue;
        };
        for raw in rest.split(',') {
            let mut cleaned = raw
                .trim()
                .trim_matches('`')
                .trim_matches('"')
                .trim()
                .to_string();
            if cleaned.starts_with('(') && cleaned.ends_with(')') && cleaned.len() >= 2 {
                cleaned = cleaned[1..cleaned.len() - 1].trim().to_string();
            }
            // Markdown link `[path](url)` or bracket-wrapped `[path]`.
            if cleaned.starts_with('[') {
                if let Some(close) = cleaned.find("](") {
                    cleaned = cleaned[1..close].to_string();
                } else if cleaned.ends_with(']') && cleaned.len() >= 2 {
                    cleaned = cleaned[1..cleaned.len() - 1].trim().to_string();
                }
            }
            // Sentence punctuation an LLM may append after a path.
            cleaned = cleaned.trim_end_matches(['.', '。']).to_string();
            if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("none") {
                continue;
            }
            if !paths.contains(&cleaned) {
                paths.push(cleaned);
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
    // A `..` component escapes the workspace; a literal one (a..b.md) is not.
    p.split('/').any(|component| component == "..")
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
            let normalized = path.strip_prefix("./").unwrap_or(&path);
            if std::path::Path::new(normalized).exists() {
                continue;
            }
            if !missing.iter().any(|(_, m)| m == normalized) {
                missing.push((task.clone(), normalized.to_string()));
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

/// Declared Goal artifact paths that are not deliverables (missing,
/// zero-sized, or a directory). Metadata-based, so binary artifacts pass
/// on size alone. Empty when the Goal names no artifact (no gate).
pub(crate) fn llm_guard_goal_outputs_missing(todo_md: &str) -> Vec<String> {
    extract_goal_artifact_paths(todo_md)
        .into_iter()
        .filter(|p| {
            if is_unverifiable_path(p) {
                return false;
            }
            !is_deliverable_file(p)
        })
        .collect()
}

/// Build a handover entry: one-line report plus an untruncated `outputs:`
/// line when `Output:` paths are declared.
pub(crate) fn build_handover_entry(prefix: &str, report: &str) -> String {
    let mut entry = format!("{}: {}", prefix, one_line_report(report));
    let outputs = extract_output_paths(report);
    if !outputs.is_empty() {
        entry.push_str(&format!("\noutputs: {}", outputs.join(", ")));
    }
    entry
}

/// Extract safe `artifacts/...` file paths from the `## Goal` section.
/// Keeps only paths under `artifacts/` with no `..` and a file extension.
fn extract_goal_artifact_paths(todo_md: &str) -> Vec<String> {
    let mut in_goal = false;
    let mut paths = Vec::new();
    for line in todo_md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## Goal") {
            in_goal = true;
            continue;
        }
        if in_goal && trimmed.starts_with("##") {
            break;
        }
        if !in_goal {
            continue;
        }
        for token in trimmed.split_whitespace() {
            let cleaned = token.trim_matches(|c| matches!(c, '`' | '"' | '\'' | ',' | ';' | ':'));
            let cleaned = cleaned
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .unwrap_or(cleaned)
                .trim_end_matches(['.', '。']);
            let looks_like_file = cleaned
                .rsplit('/')
                .next()
                .map(|name| name.contains('.') && !name.starts_with('.'))
                .unwrap_or(false);
            if cleaned.starts_with("artifacts/")
                && !cleaned.contains("..")
                && looks_like_file
                && !paths.iter().any(|p| p == cleaned)
            {
                paths.push(cleaned.to_string());
            }
        }
    }
    paths
}

/// Verified deliverables for the completion report: Goal artifact paths
/// (declared order) followed by task-declared `outputs:` paths from the
/// handover log, all existing non-empty regular files on disk, deduped
/// and `./`-normalized. `- Planner` entries are excluded (their outputs
/// are not deliverables).
pub(crate) fn llm_guard_verified_outputs(todo_md: &str, handover_md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: &str| {
        let normalized = p.strip_prefix("./").unwrap_or(p);
        if is_unverifiable_path(normalized) || !is_deliverable_file(normalized) {
            return;
        }
        if !out.iter().any(|m| m == normalized) {
            out.push(normalized.to_string());
        }
    };
    for path in extract_goal_artifact_paths(todo_md) {
        push(&path);
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
                push(&path);
            }
        }
    }
    out
}

/// `- Task` handover entries whose `outputs:` line declares at least one
/// path; feeds the completion report's zero-deliverables annotation.
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

/// Declared paths (Goal + task `outputs:` lines) the checks skip as
/// unverifiable; reported as `(+U unverifiable skipped)` so the skip is
/// never silent. Same sources as `llm_guard_verified_outputs`.
pub(crate) fn llm_guard_unverifiable_declared(todo_md: &str, handover_md: &str) -> usize {
    let mut count = 0usize;
    for path in extract_goal_artifact_paths(todo_md) {
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

/// Max number of deliverable paths listed in a completion report; the
/// remainder is summarized as `(+K more)`.
const COMPLETION_REPORT_MAX_LISTED: usize = 5;

/// One mechanical completion line (verified deliverables only):
/// `OK: all {N} tasks {already }completed; deliverables({M}): p1, ... (+K more)`.
/// Zero deliverables annotate how many tasks declared Output paths;
/// unverifiable declarations are appended as `(+U unverifiable skipped)`.
pub(crate) fn llm_guard_completion_report(
    verified: &[String],
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
    let mut line = if verified.is_empty() {
        let declared_note = if declared_outputs_tasks == 0 {
            "no tasks declared Output paths".to_string()
        } else {
            format!(
                "{} task{} declared Output paths",
                declared_outputs_tasks,
                if declared_outputs_tasks == 1 { "" } else { "s" }
            )
        };
        format!(
            "OK: all {} tasks {}; deliverables(0; {})",
            task_total, verb, declared_note
        )
    } else {
        let extra = verified.len().saturating_sub(COMPLETION_REPORT_MAX_LISTED);
        let n = verified.len() - extra;
        let mut line = format!(
            "OK: all {} tasks {}; deliverables({}): {}",
            task_total,
            verb,
            verified.len(),
            verified[..n].join(", ")
        );
        if extra > 0 {
            line.push_str(&format!(" (+{} more)", extra));
        }
        line
    };
    if unverifiable > 0 {
        line.push_str(&format!(" (+{} unverifiable skipped)", unverifiable));
    }
    line
}

/// The session's final report: the last assistant message, never a tool or
/// harness-injected message. After `Completed` this equals `messages.last()`;
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
