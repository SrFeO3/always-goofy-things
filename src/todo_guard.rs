//! LLM deviation guards for the todo modes.
//!
//! Verifies LLM work and fixes deviations the application can handle
//! mechanically: replan feedback, condensing retries, or safer fallbacks.

use crate::model::Session;
use crate::reasoning::{LoopCtx, run_reasoning_loop};

/// Advertised handover-report char limit shown in the system prompt.
pub(crate) const HANDOVER_REPORT_MAX_CHARS: usize = 300;

/// Enforcement limit (chars): 20% above the advertised limit to tolerate the
/// LLM's unreliable character counting.
pub(crate) const HANDOVER_REPORT_FUZZY_MAX_CHARS: usize = HANDOVER_REPORT_MAX_CHARS * 6 / 5; // 300 * 1.2 = 360

/// Session-context budget (in chars) for the condensing retry.
const LLM_GUARD_CONTEXT_CHARS: usize = 120_000;

/// Strip wrapping an LLM may put around a machine-format path: quotes/
/// backticks/edge punctuation, a bracket pair or markdown link, trailing
/// `.`/`。`. No Japanese-prose heuristics; non-conforming text stays as-is.
fn clean_path_token(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    s = s
        .trim_matches(|c| matches!(c, '`' | '"' | '\'' | ',' | ';' | ':' | '*'))
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

/// A bullet marker an LLM may use instead of `-` (`*`, `+`, `・`, `•`);
/// a run of markers incl. whitespace between them is stripped, so bold
/// `- **Output:**` passes. None if the line is not a bullet.
fn strip_bullet_marker(line: &str) -> Option<&str> {
    let mut rest = line;
    loop {
        let next = rest
            .trim_start_matches(['-', '*', '+', '・', '•'])
            .trim_start();
        if next.len() == rest.len() {
            break;
        }
        rest = next;
    }
    if rest.len() == line.len() {
        None
    } else {
        Some(rest)
    }
}

/// Cut an LLM annotation appended after a path (`...md (created)`,
/// `...md; todo.md (...)`) - ASCII `(`/`;` only; fullwidth stays as written
/// (no Japanese fuzz); the `artifacts/` prefix still gates.
fn cut_ascii_annotation(mut s: String) -> String {
    if let Some(idx) = s.find(['(', ';']) {
        s.truncate(idx);
    }
    s.trim().to_string()
}

/// `artifacts/` paths from a report's `Output:` line (comma/semicolon-
/// separated). Prefix sloppiness (bullet markers, bold `**`, `./`) is
/// tolerated; ASCII annotations after a path are cut; non-artifacts
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
        for raw in rest.split([',', ';']) {
            let cleaned = cut_ascii_annotation(clean_path_token(raw));
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

/// Enforce the storage cap: if the final message exceeds it (context
/// budget permitting), ask the LLM to rewrite it within the advertised
/// limit, keeping `fields`. `noun`/`limit` set wording/budget.
pub(crate) async fn llm_guard_condense_final_message<'a>(
    ctx: &mut LoopCtx<'a>,
    session: &mut Session,
    noun: &str,
    fields: &[&str],
    limit: usize,
) {
    let ctx_chars: usize = session
        .messages
        .iter()
        .map(|m| m.content.chars().count())
        .sum();
    let Some(last) = last_assistant_report(session) else {
        return;
    };
    if last.chars().count() <= HANDOVER_REPORT_FUZZY_MAX_CHARS
        || ctx_chars >= LLM_GUARD_CONTEXT_CHARS
    {
        return;
    }
    let feedback = format!(
        "Your {} is too long ({} chars > {}). Rewrite it as ONE concise {} within {} characters, keeping {}.",
        noun,
        last.chars().count(),
        limit,
        noun,
        limit,
        fields.join(" / ")
    );
    // One retry at most; ignore errors (truncation fallback still applies).
    let _ = run_reasoning_loop(ctx, session, "todo:guard:condense", feedback, Vec::new()).await;
}
