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
/// Accepts `- Output:` / `Output:` prefixes, backticks, commas, and `none`.
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

/// Max chars of the final answer read from the Goal artifact.
const LLM_GUARD_FINAL_ANSWER_MAX_CHARS: usize = 4000;

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

/// Resolve the job's final answer.
/// Prefers the Goal artifact (the LAST named path first), then the last
/// task's report, then the given notice.
pub(crate) fn llm_guard_final_answer(todo_md: &str, last_answer: &str, fallback: &str) -> String {
    for path in extract_goal_artifact_paths(todo_md).into_iter().rev() {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        let chars: Vec<char> = content.chars().collect();
        if chars.len() > LLM_GUARD_FINAL_ANSWER_MAX_CHARS {
            let head: String = chars[..LLM_GUARD_FINAL_ANSWER_MAX_CHARS].iter().collect();
            return format!(
                "{}\n\n... (final answer truncated at {} chars; full result: {})",
                head.trim_end(),
                LLM_GUARD_FINAL_ANSWER_MAX_CHARS,
                path
            );
        }
        return content;
    }
    if !last_answer.trim().is_empty() {
        return last_answer.to_string();
    }
    fallback.to_string()
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
    let Some(last) = task_session.messages.last() else {
        return;
    };
    if last.content.chars().count() <= HANDOVER_REPORT_FUZZY_MAX_CHARS
        || ctx_chars >= LLM_GUARD_CONTEXT_CHARS
    {
        return;
    }
    let feedback = format!(
        "Your Handover Report is too long ({} chars > {}). Rewrite it as ONE concise report within {} characters, keeping Status / Output / Findings / Next.",
        last.content.chars().count(),
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
