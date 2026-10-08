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

// ---------------------------------------------------------------------------
// Plan-write guard (Mode 2 executor): `./todo.md` rewrites are validated
// against a session-start snapshot at the tool boundary before they land.
// ---------------------------------------------------------------------------

/// Session-start plan snapshot plus the assigned task's absolute index.
pub(crate) struct PlanWriteGuard {
    /// Absolute index (all `## Tasks` bullets) of the assigned task
    /// (== `TaskItem.index`).
    assigned_index: usize,
    /// The plan at session start.
    plan: PlanView,
}

impl PlanWriteGuard {
    /// Snapshot constructor for tests (production no longer builds guards:
    /// new jobs never let the LLM write plans, so `plan_guard` stays None).
    #[cfg(test)]
    pub(crate) fn capture(todo_md: &str, assigned_index: usize) -> Self {
        let plan = PlanView::parse(todo_md)
            .expect("plan-write guard: todo.md must have a ## Tasks section");
        Self {
            assigned_index,
            plan,
        }
    }
}

/// One `## Tasks` bullet (`- [ ]` / `- [x]`), in `parse_todo_md`'s syntax so
/// the bullet index matches `TaskItem.index`.
#[derive(Clone)]
struct TaskBullet {
    checked: bool,
    desc: String,
}

/// A `## Tasks` line: bullet, or other (prose/blank).
#[derive(Clone)]
enum TasksLine {
    Bullet(TaskBullet),
    Other(String),
}

/// One plan file split into its `## Tasks` content and the rest, using the
/// same section rules as `parse_todo_md` (`## Tasks` heading, ends at the
/// next `##` line).
struct PlanView {
    /// Lines outside `## Tasks` (headings and other sections), trailing
    /// whitespace trimmed.
    outside: Vec<String>,
    /// `## Tasks` lines in order.
    tasks: Vec<TasksLine>,
}

impl PlanView {
    /// Parses; `Err` if there is no `## Tasks` section.
    fn parse(content: &str) -> Result<Self, String> {
        let mut outside: Vec<String> = Vec::new();
        let mut tasks: Vec<TasksLine> = Vec::new();
        let mut in_tasks = false;
        let mut saw_tasks = false;
        for line in content.lines() {
            let t = line.trim();
            if t.starts_with("## Tasks") {
                in_tasks = true;
                saw_tasks = true;
                continue;
            }
            if in_tasks && t.starts_with("##") {
                in_tasks = false;
                outside.push(t.to_string());
                continue;
            }
            if in_tasks {
                if let Some(rest) = t.strip_prefix("- [x]") {
                    tasks.push(TasksLine::Bullet(TaskBullet {
                        checked: true,
                        desc: rest.trim().to_string(),
                    }));
                } else if let Some(rest) = t.strip_prefix("- [ ]") {
                    tasks.push(TasksLine::Bullet(TaskBullet {
                        checked: false,
                        desc: rest.trim().to_string(),
                    }));
                } else {
                    tasks.push(TasksLine::Other(t.to_string()));
                }
            } else {
                outside.push(t.to_string());
            }
        }
        if !saw_tasks {
            return Err("the plan has no `## Tasks` section".to_string());
        }
        Ok(Self { outside, tasks })
    }
}

/// The `## Tasks` bullets of a parsed plan, in order.
fn tasks_bullets(view: &PlanView) -> Vec<TaskBullet> {
    view.tasks
        .iter()
        .filter_map(|l| match l {
            TasksLine::Bullet(b) => Some(b.clone()),
            TasksLine::Other(_) => None,
        })
        .collect()
}

/// The non-bullet lines of a parsed plan's `## Tasks`, in order.
fn tasks_other_lines(view: &PlanView) -> Vec<String> {
    view.tasks
        .iter()
        .filter_map(|l| match l {
            TasksLine::Other(s) => Some(s.clone()),
            TasksLine::Bullet(_) => None,
        })
        .collect()
}

/// Equality ignoring blank lines; parse already trimmed line whitespace.
fn same_lines_ignoring_blanks(a: &[String], b: &[String]) -> bool {
    a.iter()
        .filter(|l| !l.is_empty())
        .eq(b.iter().filter(|l| !l.is_empty()))
}

/// Checkbox rule for aligning old[i] onto new[j]: identical, or the assigned
/// task's `[ ]`->`[x]` flip; never anything else.
fn bullet_state_ok(old: &TaskBullet, new: &TaskBullet, i: usize, assigned: usize) -> bool {
    old.checked == new.checked || (i == assigned && !old.checked && new.checked)
}

/// Whether `new` keeps every start bullet in order, unchanged except the
/// assigned task's `[ ]`->`[x]`; unmatched new bullets are added subtasks
/// (any checkbox), requiring a non-empty description.
fn tasks_preserved(old: &[TaskBullet], new: &[TaskBullet], assigned: usize) -> bool {
    let (n, m) = (old.len(), new.len());
    // dp[i][j]: can old[i..] be aligned into new[j..]?
    let mut dp = vec![vec![false; m + 1]; n + 1];
    dp[n][m] = true;
    // Base: all old bullets matched; the remaining new bullets are additions.
    for j in (0..m).rev() {
        dp[n][j] = !new[j].desc.is_empty() && dp[n][j + 1];
    }
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            // new[j] as an added subtask (skip it): requires a description.
            let skip = !new[j].desc.is_empty() && dp[i][j + 1];
            // Or align old[i] onto new[j].
            let align = old[i].desc == new[j].desc
                && bullet_state_ok(&old[i], &new[j], i, assigned)
                && dp[i + 1][j + 1];
            dp[i][j] = skip || align;
        }
    }
    dp[0][0]
}

/// First violation found, phrased for a denial message (best effort).
fn describe_tasks_violation(old: &[TaskBullet], new: &[TaskBullet], assigned: usize) -> String {
    if !old.iter().any(|b| b.desc.is_empty())
        && let Some(b) = new.iter().find(|b| b.desc.is_empty())
    {
        return format!(
            "a subtask has an empty description (`- {}` followed by nothing)",
            if b.checked { "[x]" } else { "[ ]" }
        );
    }
    for (i, ob) in old.iter().enumerate() {
        if !new.iter().any(|nb| nb.desc == ob.desc) {
            return format!(
                "pre-existing task #{} ('{}') was removed or renamed; every task that existed at session start must stay unchanged",
                i + 1,
                ob.desc
            );
        }
        if !new.iter().any(|nb| {
            nb.desc == ob.desc
                && (nb.checked == ob.checked || (i == assigned && !ob.checked && nb.checked))
        }) {
            return format!(
                "pre-existing task #{} ('{}') had its checkbox changed; only your own task (#{}) may be marked [x]",
                i + 1,
                ob.desc,
                assigned + 1
            );
        }
    }
    "the order of the existing tasks changed; keep the `## Tasks` order unchanged".to_string()
}

/// Validate a `./todo.md` rewrite against the snapshot; `Err(reason)` rejects.
fn validate_plan_write(guard: &PlanWriteGuard, intended: &str) -> Result<(), String> {
    let new_view = PlanView::parse(intended)?;
    let old = &guard.plan;
    if !same_lines_ignoring_blanks(&old.outside, &new_view.outside) {
        return Err(
            "content outside the `## Tasks` section changed (## Goal / ## Deliverables / headings / Status must stay exactly as they were)"
                .to_string(),
        );
    }
    let old_other = tasks_other_lines(old);
    let new_other = tasks_other_lines(&new_view);
    if !same_lines_ignoring_blanks(&old_other, &new_other) {
        return Err(
            "non-task lines inside the `## Tasks` section changed; keep them as they were"
                .to_string(),
        );
    }
    let old_bullets = tasks_bullets(old);
    let new_bullets = tasks_bullets(&new_view);
    if !tasks_preserved(&old_bullets, &new_bullets, guard.assigned_index) {
        return Err(describe_tasks_violation(
            &old_bullets,
            &new_bullets,
            guard.assigned_index,
        ));
    }
    Ok(())
}

/// The plan file a tool `path` names, `.`/`..` resolved (`todo.md` /
/// `next-task.md`); `None` for other files. (`validate_path` already rejects
/// absolute paths and `..` escapes.)
fn plan_file_name(path: &str) -> Option<&'static str> {
    let mut comps: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                comps.pop();
            }
            c => comps.push(c),
        }
    }
    match comps.as_slice() {
        [name] if *name == "todo.md" => Some("todo.md"),
        [name] if *name == "next-task.md" => Some("next-task.md"),
        _ => None,
    }
}

/// Shared `[TOOL_DENIED]` message for plan-write rejections.
fn plan_write_denied_message(reason: &str, assigned: usize) -> String {
    format!(
        "[TOOL_DENIED] './todo.md' write rejected by the todo-mode guard (the plan is frozen except your task): {}. \
         Allowed: mark your task (#{}) `[x]`, and add subtasks - checked or unchecked - for work you discovered. \
         Forbidden: changing, removing, reordering, or marking `[x]` any task that existed at session start, \
         changing other sections, or editing `./next-task.md`. \
         Make the edit so only the allowed changes remain.",
        reason,
        assigned + 1
    )
}

/// Validate a plan-file write before it lands:
/// - `todo.md` + `write_file`: checked against the snapshot at dispatch.
/// - `todo.md` + `str_replace_editor`: checked in the tool before the write
///   (`llm_guard_plan_write_validate`).
/// - `next-task.md`: any write is rejected (planner-owned).
///
/// `None` = allowed. Denials are tool errors; the reasoning loop feeds them
/// back to the LLM, which rewrites and retries.
pub(crate) fn llm_guard_plan_file_write(
    name: &str,
    path: &str,
    args: &serde_json::Value,
    guard: &PlanWriteGuard,
) -> Option<String> {
    let plan_file = plan_file_name(path)?;
    if plan_file == "next-task.md" {
        if matches!(name, "write_file" | "str_replace_editor") {
            return Some("[TOOL_DENIED] './next-task.md' is owned by the replan planner; the executor must not write it (the planner rewrites it before the next task).".to_string());
        }
        return None;
    }
    match name {
        "write_file" => {
            let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                return None; // missing content: the tool reports it
            };
            if let Err(reason) = validate_plan_write(guard, content) {
                return Some(plan_write_denied_message(&reason, guard.assigned_index));
            }
            None
        }
        "str_replace_editor" => None, // result checked in the tool, before the write
        _ => None,
    }
}

/// Validate `./todo.md` content a tool computed (e.g. a `str_replace_editor`
/// result) with the same snapshot check as `llm_guard_plan_file_write`.
/// `None` = allowed.
pub(crate) fn llm_guard_plan_write_validate(
    path: &str,
    content: &str,
    guard: &PlanWriteGuard,
) -> Option<String> {
    if plan_file_name(path) != Some("todo.md") {
        return None;
    }
    match validate_plan_write(guard, content) {
        Ok(()) => None,
        Err(reason) => Some(plan_write_denied_message(&reason, guard.assigned_index)),
    }
}

#[cfg(test)]
#[path = "tests/todo_guard_test.rs"]
mod tests;
