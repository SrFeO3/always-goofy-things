//! Plan-Execute task loop for multi-task execution with todo.md-based handover.
//!
//! Runs Mode 1 (Plan-Exec-Static) and Mode 2 (Plan-Exec-Dynamic)
//! for multi-task execution with LLM context reset between tasks.
//!
//! # Execution Flow
//!
//! ## Mode 1 (Plan-Exec-Static)
//!
//! 1. [User Input]     : Read the task plan from todo.md.
//! 2. [Todo Loop]      : Recursive cycle through unchecked tasks, one per fresh context.
//!    - Parse State    : Identify the next unchecked task from todo.md.
//!    - Task Loop      : Fresh executor reasoning loop (LLM Call -> Tool Exec -> Feedback); runs the single task.
//!    - Store State    : The application updates todo.md (mark [x]) and artifacts/handover.md and resets the LLM context.
//! 3. [Final Answer]   : Read the Goal artifact (an `artifacts/...` path named in the Goal) and present it; fall back to the last task's report, then a completion notice.
//!
//! ## Mode 2 (Plan-Exec-Dynamic)
//!
//! 1. [User Input]     : Read the task plan from todo.md.
//! 2. [Todo Loop]      : Recursive cycle: replan, then execute one task per fresh context.
//!    - Parse State    : Read todo.md for the plan; the replan reads handover.md (task reports, in) and writes next-task.md (per-task brief, out).
//!    - Replan Loop    : Fresh planner reasoning loop (LLM Call -> Tool Exec -> Feedback); inspects todo.md, handover.md and the latest artifacts/, then updates todo.md and rewrites next-task.md.
//!    - Task Loop      : Fresh executor reasoning loop (LLM Call -> Tool Exec -> Feedback); reads todo.md + next-task.md, runs the single task and updates todo.md.
//!    - Store State    : The application updates artifacts/handover.md and resets the LLM context.
//! 3. [Final Answer]   : Read the Goal artifact (an `artifacts/...` path named in the Goal) and present it; fall back to the last task's report, then a completion notice.

use anyhow::{Context, Result, anyhow};

use crate::attach::AttachedFile;
use crate::model::{Message, Session};
use crate::persistence;
use crate::reasoning::{EndReason, LoopCtx, run_reasoning_loop};
use crate::startup;
use crate::todo_guard::{
    HANDOVER_REPORT_MAX_CHARS, PlanWriteGuard, build_handover_entry, has_deliverables_section,
    last_assistant_report, llm_guard_completion_report, llm_guard_condense_final_message,
    llm_guard_declared_outputs, llm_guard_goal_outputs_missing, llm_guard_tasks_declaring_outputs,
    llm_guard_unfinished_outputs, llm_guard_unverifiable_declared, llm_guard_verified_outputs,
    merge_condensed_report,
};

pub(crate) const TODO_MD_PATH: &str = "./todo.md";

/// Push a system message to both the session log (for persistence / completed
/// display) and the live GUI stream buffer (so it appears during execution).
#[cfg(feature = "gui")]
fn push_system_msg(gui_log: &mut Session, text: &str) {
    push_system_msg_blank(gui_log, text, true);
}

/// Same as `push_system_msg` but with control over the leading blank line.
#[cfg(feature = "gui")]
fn push_system_msg_blank(gui_log: &mut Session, text: &str, leading_blank: bool) {
    gui_log.messages.push(Message {
        role: "system".to_string(),
        content: text.to_string(),
        ..Default::default()
    });
    let mut buf = crate::model::LLM_STREAM_BUF.lock().unwrap();
    if leading_blank {
        buf.2.push('\n');
    }
    buf.2.push_str(text);
    buf.2.push('\n');
}

/// A single task item parsed from todo.md.
#[derive(Debug, Clone)]
struct TaskItem {
    /// 0-based index among the original task list.
    index: usize,
    /// The task description (text after `- [ ]` or `- [x]`).
    description: String,
    /// Whether this task is already completed.
    done: bool,
}

/// Parse `./todo.md` and extract task items from the `## Tasks` section.
fn parse_todo_md() -> Result<Vec<TaskItem>> {
    let content = std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;

    let mut tasks = Vec::new();
    let mut in_tasks_section = false;
    let mut index: usize = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("## Tasks") {
            in_tasks_section = true;
            continue;
        }
        if in_tasks_section && trimmed.starts_with("##") {
            break;
        }

        if in_tasks_section {
            if let Some(desc) = trimmed.strip_prefix("- [x]") {
                tasks.push(TaskItem {
                    index,
                    description: desc.trim().to_string(),
                    done: true,
                });
                index += 1;
            } else if let Some(desc) = trimmed.strip_prefix("- [ ]") {
                tasks.push(TaskItem {
                    index,
                    description: desc.trim().to_string(),
                    done: false,
                });
                index += 1;
            }
        }
    }

    if tasks.is_empty() {
        anyhow::bail!(
            "No tasks found in {} (## Tasks section with - [ ] / - [x] items required)",
            TODO_MD_PATH
        );
    }

    Ok(tasks)
}

/// Count Task-section bullets as `(checked, unchecked)`: walks the
/// `## Tasks` section, stops at the next `##` heading; legacy plans yield
/// zeroes without panicking.
fn count_task_boxes(todo_md: &str) -> (usize, usize) {
    let mut in_tasks = false;
    let mut checked = 0;
    let mut unchecked = 0;
    for line in todo_md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## Tasks") {
            in_tasks = true;
            continue;
        }
        if in_tasks && trimmed.starts_with("##") {
            break;
        }
        if in_tasks {
            if trimmed.starts_with("- [x]") {
                checked += 1;
            } else if trimmed.starts_with("- [ ]") {
                unchecked += 1;
            }
        }
    }
    (checked, unchecked)
}

/// Count unchecked tasks (`- [ ]`) in the given todo.md content.
fn count_unchecked(todo_md: &str) -> usize {
    count_task_boxes(todo_md).1
}

/// Count all task items (`- [x]` and `- [ ]`) in the given todo.md content.
/// Used for the completion report's task total without requiring a full
/// parse (robust against legacy plans without a Tasks section).
fn count_task_items(todo_md: &str) -> usize {
    let (checked, unchecked) = count_task_boxes(todo_md);
    checked + unchecked
}

/// Mark the nth unchecked task as done by replacing `- [ ]` with `- [x]`.
///
/// Only the checkbox is flipped; prose no longer belongs in todo.md (the
/// executor's report goes to `artifacts/handover.md` via `append_handover`).
fn mark_task_done(task_index: usize) -> Result<()> {
    let content = std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;

    let mut new_lines: Vec<String> = Vec::new();
    let mut in_tasks_section = false;
    let mut unchecked_found: usize = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("## Tasks") {
            in_tasks_section = true;
        }
        if in_tasks_section && trimmed.starts_with("##") && !trimmed.starts_with("## Tasks") {
            in_tasks_section = false;
        }

        if in_tasks_section && let Some(desc) = trimmed.strip_prefix("- [ ]") {
            if unchecked_found == task_index {
                new_lines.push(format!("- [x]{}", desc));
                unchecked_found += 1;
                continue;
            }
            unchecked_found += 1;
        }

        new_lines.push(line.to_string());
    }

    let mut new_content = new_lines.join("\n");
    new_content.push('\n');

    std::fs::write(TODO_MD_PATH, &new_content).context("Failed to write ./todo.md")?;
    Ok(())
}

/// Unmark the task at the absolute index by replacing `- [x]` with `- [ ]`.
///
/// Counts every `## Tasks` bullet (both `- [x]` and `- [ ]`), unlike
/// `mark_task_done` (only `- [ ]`); the absolute basis matches `task.index`.
/// No-op (no write) if already `- [ ]`, out of range, or no `## Tasks`.
fn unmark_task_by_index(index: usize) -> Result<()> {
    let content = std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;

    let mut new_lines: Vec<String> = Vec::new();
    let mut in_tasks_section = false;
    let mut task_count: usize = 0;
    let mut changed = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("## Tasks") {
            in_tasks_section = true;
        }
        if in_tasks_section && trimmed.starts_with("##") && !trimmed.starts_with("## Tasks") {
            in_tasks_section = false;
        }

        if in_tasks_section && (trimmed.starts_with("- [x]") || trimmed.starts_with("- [ ]")) {
            if task_count == index && trimmed.starts_with("- [x]") {
                let desc = trimmed.strip_prefix("- [x]").unwrap();
                new_lines.push(format!("- [ ]{}", desc));
                changed = true;
                task_count += 1;
                continue;
            }
            task_count += 1;
        }

        new_lines.push(line.to_string());
    }

    if changed {
        let mut new_content = new_lines.join("\n");
        new_content.push('\n');
        std::fs::write(TODO_MD_PATH, &new_content).context("Failed to write ./todo.md")?;
    }
    Ok(())
}

/// Path of the free-form handover log (inside `artifacts/`, never parsed).
pub(crate) const HANDOVER_MD_PATH: &str = "./artifacts/handover.md";

/// Create `artifacts/handover.md` with a short template when it does not exist,
/// so LLM sessions always have a concrete file to read, and a guide to what
/// belongs there (the executor report format and the planner note order).
fn seed_handover() -> Result<()> {
    if std::path::Path::new(HANDOVER_MD_PATH).exists() {
        return Ok(());
    }
    append_handover(
        "# Handover Log\n\
         \n\
         Notes for the next LLM session(s). The replan planner (Mode 2) and every\n\
         Mode 1 task session read this file together with ./todo.md first; Mode 2\n\
         task sessions read ./next-task.md instead. The application appends every\n\
         session's final report here, in this format:\n\
         - Task N: - Status: done / blocked - Output: <paths> - Findings: <facts> - Next: <pointer>\n\
         - Planner: - Status: <state> - Progress: <progress> - Decisions: <decisions> - Next: <plan>\n\
         A task entry may be followed by an `outputs:` line listing the files the task declared in Output (never truncated).\n\
         Neither the executor nor the planner edits this file; the application writes it.\n\
         \n\
         ---\n\
         Entries are appended below by the application after each session.\n",
    )
}

/// Append one entry to `artifacts/handover.md` (the free-form handover log).
/// The `artifacts/` directory is created if missing; the file is appended,
/// never rewritten, so earlier entries survive across sessions.
///
/// Loose dedup: when the file already contains an entry with the same
/// `- Task N:` marker (e.g. the executor wrote its own report during the
/// session), the append is skipped. Exactness is not required; this only
/// prevents obvious double records.
fn append_handover(entry: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(HANDOVER_MD_PATH).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap_or_default();

    let marker = entry
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    if marker.starts_with("- Task ")
        && existing
            .lines()
            .any(|l| l.trim_start().starts_with(&marker))
    {
        return Ok(());
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(entry);
    if !content.ends_with('\n') {
        content.push('\n');
    }
    std::fs::write(HANDOVER_MD_PATH, content).context("Failed to write ./artifacts/handover.md")?;
    Ok(())
}

/// Check whether the plan is complete (Mode 2).
///
/// The strict plan format has no `## Status` section: completion is simply
/// "no unchecked `- [ ]` tasks remain". Legacy plans may still carry a
/// `## Status` section with `Status: Completed`; if such a section exists,
/// it is honored as well.
fn check_completion(todo_md: &str) -> bool {
    if count_unchecked(todo_md) > 0 {
        return false;
    }

    let mut in_status_section = false;
    let mut has_status_section = false;
    let mut status_completed = false;
    for line in todo_md.lines() {
        let trimmed = line.trim();

        if trimmed == "## Status" {
            in_status_section = true;
            has_status_section = true;
            continue;
        }
        if in_status_section && trimmed.starts_with("##") {
            break;
        }
        if in_status_section {
            let lower = trimmed.to_lowercase();
            // Accept both the plain form (`Status: Completed`) and the
            // markdown list form (`- Status: Completed`).
            if lower.contains("status:") && lower.contains("completed") {
                status_completed = true;
            }
        }
    }

    // Strict format (no Status section): complete. Legacy format: require
    // the Status section to declare completion.
    !has_status_section || status_completed
}

/// How one replan (Mode 2) ended.
#[derive(Debug)]
enum ReplanOutcome {
    /// Replan succeeded; carries the todo.md content read back from disk.
    Updated(String),
    /// User pressed Ctrl+C during the replan (no retry, no handover append).
    Interrupted,
}

/// Backoff delays (seconds) between consecutive replan failures, capped.
const REPLAN_BACKOFF_SECS: [u64; 5] = [10, 30, 60, 120, 300];

/// Backoff wait after `consecutive_failures` failed replan attempts.
fn replan_backoff(consecutive_failures: u32) -> std::time::Duration {
    let idx = (consecutive_failures as usize)
        .saturating_sub(1)
        .min(REPLAN_BACKOFF_SECS.len() - 1);
    std::time::Duration::from_secs(REPLAN_BACKOFF_SECS[idx])
}

/// Replan phase (Mode 2): the planner reviews and updates todo.md.
///
/// Runs a full reasoning loop in a fresh planner session: the planner reads
/// ./todo.md with read_file, may inspect artifacts/ with tools, saves the
/// updated plan to ./todo.md via write_file, and rewrites the per-task brief
/// ./next-task.md. Its final message (the plan-update notes) is appended to
/// artifacts/handover.md by the application, like a task report. Returns
/// `ReplanOutcome::Updated` with the todo.md content after the loop.
async fn run_replan_loop<'a>(
    ctx: &mut LoopCtx<'a>,
    gui_log: &mut Session,
    user_query: &str,
    app_feedback: Option<&str>,
) -> Result<ReplanOutcome> {
    // The replanner restructures the plan freely; no plan-write guard here.
    ctx.plan_guard = None;
    let config = ctx.config;
    let q = user_query.trim();
    let mut sections =
        vec!["Review and update `./todo.md` per the system instructions.".to_string()];
    if !q.is_empty() {
        sections.push(format!("Additional user instructions: {}", q));
    }
    if let Some(feedback) = app_feedback {
        sections.push(format!("Application feedback: {}", feedback));
    }
    let replan_prompt = sections.join("\n\n");

    let system_msg = startup::system_message_mode2_replan(config);
    let replan_label = format!("{}_replan", config.session_label);
    let mut replan_session = Session::new(replan_label.clone(), system_msg);
    // Move a leftover session file from an earlier replan/run aside.
    persistence::init_session(&replan_label)?;
    let end_reason = run_reasoning_loop(
        ctx,
        &mut replan_session,
        "todo:replan",
        replan_prompt,
        Vec::new(),
    )
    .await?;
    match end_reason {
        EndReason::Completed => {}
        EndReason::Interrupted => return Ok(ReplanOutcome::Interrupted),
        EndReason::LlmError => {
            return Err(anyhow!("replan failed: LLM connection error"));
        }
        EndReason::EmptyLimit => {
            return Err(anyhow!(
                "replan failed: empty LLM responses reached the limit"
            ));
        }
        EndReason::MaxTurns => {
            return Err(anyhow!("replan failed: max reasoning turns reached"));
        }
    }

    // The planner's final message is its plan-update notes; the application
    // appends them to artifacts/handover.md like a task report (one line;
    // the planner never edits the log itself). Condense over-long notes
    // first like task reports, snapshotting for merge_condensed_report
    // (re-appends a dropped `- Output:` line).
    let pre_condense = last_assistant_report(&replan_session).map(str::to_string);
    llm_guard_condense_final_message(
        ctx,
        &mut replan_session,
        "plan-update note",
        &["Status", "Progress", "Decisions", "Next"],
        HANDOVER_REPORT_MAX_CHARS,
    )
    .await;
    let raw_note = last_assistant_report(&replan_session).unwrap_or_default();
    let note = if raw_note.trim().is_empty() {
        "Status: (no note)".to_string()
    } else {
        merge_condensed_report(pre_condense.as_deref(), raw_note)
    };
    append_handover(&build_handover_entry("- Planner", &note))?;

    // Push the planner's conversation to the GUI log for visibility.
    for msg in replan_session.messages.iter().skip(1) {
        gui_log.messages.push(msg.clone());
    }

    // The planner updates ./todo.md via write_file during the loop; read it back.
    let todo_content =
        std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md after replan")?;
    Ok(ReplanOutcome::Updated(todo_content))
}

/// Completion gate for Mode 2: when all tasks are `[x]`, run ONE final replan
/// so the planner can add tasks if the Goal is not yet achieved (task-adding
/// continuation pattern), then re-check. Returns `true` when the plan is
/// still complete after that final replan.
///
/// A failed final replan propagates `Err` (completion unconfirmed, exit 1);
/// a Ctrl+C stops with exit 0 but prints "completion NOT confirmed".
async fn final_replan_confirms_completion<'a>(
    ctx: &mut LoopCtx<'a>,
    gui_log: &mut Session,
    user_query: &str,
    app_feedback: Option<&str>,
) -> Result<bool> {
    println!(
        "{}--- [Replan] (final: all tasks done - planner may add tasks) ---{}",
        startup::C_CYAN,
        startup::RESET
    );
    #[cfg(feature = "gui")]
    push_system_msg(
        gui_log,
        "--- [Replan] (final: all tasks done - planner may add tasks) ---",
    );

    // One backoff retry before treating the completion as unconfirmed.
    let first = run_replan_loop(ctx, gui_log, user_query, app_feedback).await;
    let replan_outcome = match first {
        Ok(replan_outcome) => replan_outcome,
        Err(e) => {
            let wait = replan_backoff(1);
            println!(
                "\n{}[Replan] (final) LLM error: {}. Retrying once (backoff {}s).{}",
                startup::C_RED,
                e,
                wait.as_secs(),
                startup::RESET
            );
            tokio::time::sleep(wait).await;
            match run_replan_loop(ctx, gui_log, user_query, app_feedback).await {
                Ok(replan_outcome) => replan_outcome,
                Err(e2) => {
                    println!(
                        "\n{}[Replan] (final) LLM error: {}. Completion could not be confirmed. todo.md is all-[x]; run again to confirm.{}",
                        startup::C_RED,
                        e2,
                        startup::RESET
                    );
                    return Err(e2);
                }
            }
        }
    };

    match replan_outcome {
        ReplanOutcome::Updated(todo) => Ok(check_completion(&todo)),
        ReplanOutcome::Interrupted => {
            // Ctrl+C: the planner did not confirm; the mechanical Goal
            // gate that follows decides the outcome (LLM-free, so it
            // still runs).
            println!(
                "\n{}[Replan] (final) Interrupted by user; running the mechanical completion checks.{}",
                startup::C_YELLOW,
                startup::RESET
            );
            Ok(true)
        }
    }
}

/// Replan feedback: pending app reports plus the current sweep of declared
/// but still-missing output paths, so the planner can add fix tasks.
fn build_replan_feedback(app_feedback: &[String]) -> Option<String> {
    let mut items: Vec<String> = app_feedback.to_vec();
    let handover = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap_or_default();
    let unfinished = llm_guard_unfinished_outputs(&handover);
    if !unfinished.is_empty() {
        items.push(format!(
            "Declared Output paths still do not exist: {}. Add a fix task if the plan is incomplete.",
            unfinished
                .iter()
                .map(|(t, p)| format!("{}: {}", t, p))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if items.is_empty() {
        None
    } else {
        Some(items.join("\n"))
    }
}

/// Goal gate core: non-deliverable Goal artifacts veto completion. LLM-free,
/// so it also runs after Ctrl+C. Applied to Mode 2's final replan, Mode 1
/// all-complete/early exit, and every post-loop OK path.
fn check_goal_deliverables(todo_md: &str) -> Result<()> {
    let missing = llm_guard_goal_outputs_missing(todo_md);
    if missing.is_empty() {
        return Ok(());
    }
    println!(
        "\n{}--- [App] Completion NOT confirmed: Goal deliverables missing or empty: {} ---{}",
        startup::C_RED,
        missing.join(", "),
        startup::RESET
    );
    anyhow::bail!(
        "completion unconfirmed: Goal deliverables missing or empty: {}",
        missing.join(", ")
    )
}

/// Mode 2 gate: runs `check_goal_deliverables` after the final replan
/// (Ctrl+C included); failure leaves completion unconfirmed (Err, exit != 0).
fn verify_goal_deliverables() -> Result<()> {
    let todo_content = std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;
    check_goal_deliverables(&todo_content)
}

/// Job-end sweep: report declared output paths that still do not
/// exist. Three channels: console warning, summary note, and a handover.md
/// machine note (content-deduped; write failure is warned about, never
/// fatal, so a completed job is not broken by a logging problem).
fn finalize_with_sweep(mut summary: String) -> String {
    let handover = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap_or_default();
    let missing = llm_guard_unfinished_outputs(&handover);
    if missing.is_empty() {
        return summary;
    }
    let paths: Vec<String> = missing
        .iter()
        .map(|(t, p)| format!("{}: {}", t, p))
        .collect();
    println!(
        "\n{}--- [App] Declared Output paths still do not exist at job end: {} ---{}",
        startup::C_YELLOW,
        paths.join(", "),
        startup::RESET
    );
    let note = format!(
        "> [App] Job ended with declared Output paths still missing: {}",
        paths.join(", ")
    );
    if !handover.lines().any(|l| l.trim() == note)
        && let Err(e) = append_handover(&note)
    {
        eprintln!(
            "[App] Warning: failed to record job-end note in handover.md: {}",
            e
        );
    }
    if !summary.is_empty() && !summary.ends_with('\n') {
        summary.push('\n');
    }
    summary.push_str(&format!(
        "(Note: declared Output paths still missing at job end: {})",
        paths.join(", ")
    ));
    summary
}

/// Run the Plan-Exec todo loop for Mode 1 and Mode 2.
pub(crate) async fn run_todo_loop<'a>(
    ctx: &mut LoopCtx<'a>,
    gui_log: &mut Session,
    user_query: String,
    attached_files: Vec<AttachedFile>,
) -> Result<String> {
    let config = ctx.config;
    if !std::path::Path::new(TODO_MD_PATH).exists() {
        anyhow::bail!(
            "./todo.md not found. Create a task plan with ## Tasks section (Mode 1) or let the AI generate one (Mode 2)."
        );
    }

    let tasks = parse_todo_md()?;
    let total = tasks.len();
    let pending: Vec<&TaskItem> = tasks.iter().filter(|t| !t.done).collect();

    // Read todo.md title line for preview
    let todo_title = {
        let content = std::fs::read_to_string(TODO_MD_PATH).unwrap_or_default();
        content
            .lines()
            .find(|l| !l.trim().is_empty() && l.starts_with('#'))
            .unwrap_or("(no title)")
            .trim_start_matches('#')
            .trim()
            .to_string()
    };
    let mode_name = if config.todo_mode == 2 {
        "Plan-Exec-Dynamic"
    } else {
        "Plan-Exec-Static"
    };

    // Resumed run: Mode 1 finishes with the completion report; Mode 2 falls
    // through so the loop's final-replan completion gate still runs.
    if pending.is_empty() && config.todo_mode != 2 {
        let content = std::fs::read_to_string(TODO_MD_PATH).unwrap_or_default();
        let handover = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap_or_default();
        let (goal, task_outputs) = llm_guard_verified_outputs(&content, &handover);
        // Goal-gate early exit too: "already completed" needs its deliverables.
        check_goal_deliverables(&content)?;
        return Ok(finalize_with_sweep(llm_guard_completion_report(
            &goal,
            &task_outputs,
            has_deliverables_section(&content),
            count_task_items(&content),
            llm_guard_tasks_declaring_outputs(&handover),
            llm_guard_unverifiable_declared(&content, &handover),
            true,
        )));
    }

    let pending_count = pending.len();
    let exec_line = format!(
        "Executing todo plan \"{}\" ({} mode) - {} tasks, {}/{} pending.",
        todo_title, mode_name, total, pending_count, total
    );
    println!("\n{}{}{}\n", startup::C_CYAN, exec_line, startup::RESET);
    #[cfg(feature = "gui")]
    push_system_msg_blank(gui_log, &exec_line, false);

    // Ensure the free-form handover log exists (seeded with a template), so
    // every session can read it together with todo.md.
    seed_handover()?;

    let summary = if config.todo_mode == 2 {
        run_todo_loop_mode2(ctx, gui_log, &user_query, attached_files).await?
    } else {
        run_todo_loop_mode1(ctx, gui_log, &user_query, attached_files, pending).await?
    };
    Ok(finalize_with_sweep(summary))
}

/// Build the task user message: the task description plus the `-q` addendum
/// (if any). The system prompt binds this to ./todo.md.
fn task_prompt(description: &str, user_query: &str) -> String {
    let q = user_query.trim();
    if q.is_empty() {
        format!("Task: {}", description)
    } else {
        format!(
            "Task: {}\n\nAdditional user instructions: {}",
            description, q
        )
    }
}

/// Mode 1: Static sequential execution with application-side `[x]` marking.
async fn run_todo_loop_mode1<'a>(
    ctx: &mut LoopCtx<'a>,
    gui_log: &mut Session,
    user_query: &str,
    attached_files: Vec<AttachedFile>,
    pending: Vec<&TaskItem>,
) -> Result<String> {
    let config = ctx.config;
    let pending_count = pending.len();
    let mut completed = 0usize;

    for task in &pending {
        let task_num = completed + 1;
        println!(
            "\n{}--- [Task {}/{}] {} ---{}",
            startup::C_CYAN,
            task_num,
            pending_count,
            task.description,
            startup::RESET
        );
        // Also push to GUI so the task header appears during execution.
        #[cfg(feature = "gui")]
        push_system_msg(
            gui_log,
            &format!(
                "--- [Task {}/{}] {} ---",
                task_num, pending_count, task.description
            ),
        );

        let system_msg = startup::system_message_mode1_task_loop(config);

        let task_label = format!("{}_task{}", config.session_label, task.index);
        let mut task_session = Session::new(task_label.clone(), system_msg);
        // Move a leftover session file from an earlier interrupted run aside.
        persistence::init_session(&task_label)?;

        let end_reason = run_reasoning_loop(
            ctx,
            &mut task_session,
            &format!("todo:task:{}", task.index),
            task_prompt(&task.description, user_query),
            attached_files.clone(),
        )
        .await?;

        if end_reason.is_completed() {
            // llm_guard_: condense an over-long handover report before logging.
            // Snapshot first: the rewrite may drop the `- Output:` declaration.
            let pre_condense = last_assistant_report(&task_session).map(str::to_string);
            llm_guard_condense_final_message(
                ctx,
                &mut task_session,
                "Handover Report",
                &["Status", "Output", "Findings", "Next"],
                HANDOVER_REPORT_MAX_CHARS,
            )
            .await;
            // Effective report: the last assistant message plus any
            // declarations the condense rewrite dropped.
            let mut raw_note = merge_condensed_report(
                pre_condense.as_deref(),
                last_assistant_report(&task_session).unwrap_or("Task completed."),
            );

            // A task is complete only when every declared Output path
            // exists on disk. One feedback retry (same session, so the
            // LLM sees its previous work), then give up: no [x], and a
            // durable reason is left for a resumed run.
            let mut missing = llm_guard_declared_outputs(&raw_note);
            if !missing.is_empty() {
                let feedback = format!(
                    "Your report declared Output paths that do not exist: {}. \
                     The task is NOT complete until every declared Output path exists \
                     on disk. Finish the work and rewrite your Handover Report with \
                     the existing paths.",
                    missing.join(", ")
                );
                let _ = run_reasoning_loop(
                    ctx,
                    &mut task_session,
                    "todo:guard:fix-outputs",
                    feedback,
                    Vec::new(),
                )
                .await;
                // The retry report replaces the declaration: merging the old
                // paths back in would keep the task blocked forever.
                raw_note = last_assistant_report(&task_session)
                    .unwrap_or("Task completed.")
                    .to_string();
                missing = llm_guard_declared_outputs(&raw_note);
            }

            if missing.is_empty() {
                mark_task_done(0)?;
                // Hand the executor's final report over to the free-form log.
                // Uses the stable task index (not the per-run counter) so that
                // resumed runs never collide with earlier entries of the same
                // task number. Declared Output paths are kept on an untruncated
                // `outputs:` line (the one-liner may cut them).
                append_handover(&build_handover_entry(
                    &format!("- Task {}", task.index + 1),
                    &raw_note,
                ))?;

                persistence::archive_todo_session(&task_label, task.index)?;

                // Push task's LLM conversation to GUI log
                for msg in task_session.messages.iter().skip(1) {
                    gui_log.messages.push(msg.clone());
                }
                let done_msg = format!("--- Task {}/{} done ---", task_num, pending_count);
                println!("{}{}{}", startup::C_CYAN, done_msg, startup::RESET);
                gui_log.messages.push(Message {
                    role: "system".to_string(),
                    content: done_msg.clone(),
                    ..Default::default()
                });
                #[cfg(feature = "gui")]
                crate::model::LLM_STREAM_BUF
                    .lock()
                    .unwrap()
                    .2
                    .push_str(&format!("{}\n", done_msg));

                completed += 1;
            } else {
                // Give up after the single retry: the task stays unchecked.
                // The machine note marker differs from `- Task N:` so
                // append_handover's dedup never swallows a later real entry
                // from a resumed run.
                println!(
                    "\n{}--- [App] Task {} NOT complete: declared Output paths do not exist: {} ---{}",
                    startup::C_YELLOW,
                    task_num,
                    missing.join(", "),
                    startup::RESET
                );
                append_handover(&format!(
                    "- Task {} [unverified]: not completed; declared Output paths do not exist: {}. No more retries this run; run again to retry.",
                    task.index + 1,
                    missing.join(", ")
                ))?;
                break;
            }
        } else {
            // Distinguish Ctrl+C from LLM failure / limits.
            match end_reason {
                EndReason::LlmError => {
                    println!(
                        "\n{}--- [LLM Error] Task {} could not run (LLM connection error). Run again after recovery. ---{}",
                        startup::C_YELLOW,
                        task.description,
                        startup::RESET
                    );
                }
                EndReason::Interrupted => {
                    println!(
                        "\n{}--- [Interrupted] Task {} was not completed. Run again to resume. ---{}",
                        startup::C_YELLOW,
                        task.description,
                        startup::RESET
                    );
                }
                EndReason::EmptyLimit | EndReason::MaxTurns => {
                    println!(
                        "\n{}--- [Stopped] Task {} was not completed (limit reached). ---{}",
                        startup::C_YELLOW,
                        task.description,
                        startup::RESET
                    );
                }
                EndReason::Completed => unreachable!("is_completed() was false"),
            }
            break;
        }
    }

    Ok(if completed == pending_count {
        let final_todo = std::fs::read_to_string(TODO_MD_PATH).unwrap_or_default();
        // Mode 1 Goal gate: same rule as Mode 2 - Err when deliverables are
        // missing or empty.
        check_goal_deliverables(&final_todo)?;
        let handover = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap_or_default();
        let (goal, task_outputs) = llm_guard_verified_outputs(&final_todo, &handover);
        llm_guard_completion_report(
            &goal,
            &task_outputs,
            has_deliverables_section(&final_todo),
            count_task_items(&final_todo),
            llm_guard_tasks_declaring_outputs(&handover),
            llm_guard_unverifiable_declared(&final_todo, &handover),
            false,
        )
    } else {
        format!(
            "{} of {} tasks completed. Check ./todo.md for pending items.",
            completed, pending_count
        )
    })
}

/// Mode 2: Dynamic replanning with LLM-driven todo.md updates.
async fn run_todo_loop_mode2<'a>(
    ctx: &mut LoopCtx<'a>,
    gui_log: &mut Session,
    user_query: &str,
    attached_files: Vec<AttachedFile>,
) -> Result<String> {
    let config = ctx.config;
    let mut completed = 0usize;
    let mut replan_stalls = 0u32;
    // Consecutive failed replans; drives the backoff. Reset on success.
    let mut replan_consecutive_failures: u32 = 0;
    // App-side reports (e.g. missing declared outputs) waiting to be shown
    // to the planner; cleared once a replan succeeds.
    let mut app_feedback: Vec<String> = Vec::new();

    loop {
        let todo_content =
            std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;

        // Completion gate: when all tasks are `[x]`, run ONE final replan so
        // the planner can add tasks if the Goal is not yet achieved; exit
        // only when the plan is still complete after that final replan.
        if check_completion(&todo_content) {
            if final_replan_confirms_completion(
                ctx,
                gui_log,
                user_query,
                build_replan_feedback(&app_feedback).as_deref(),
            )
            .await?
            {
                // Completion gate: the Goal deliverables must exist on disk
                // before the job exits as complete.
                verify_goal_deliverables()?;
                println!(
                    "{}--- [TODO LOOP] Completion reached. Exiting. ---{}",
                    startup::C_CYAN,
                    startup::RESET
                );
                break;
            }
            // The final replan added tasks: continue with a fresh iteration.
            app_feedback.clear();
            replan_stalls = 0;
            continue;
        }

        // --- Replan ---
        // Backoff before the next replan after a failed round (avoids a fast
        // spin while the server is unreachable; the first site is the retry).
        if replan_consecutive_failures > 0 {
            replan_consecutive_failures += 1;
            let wait = replan_backoff(replan_consecutive_failures);
            println!(
                "\n{}[Replan] server unreachable ({} consecutive failures); retrying in {}s...{}",
                startup::C_YELLOW,
                replan_consecutive_failures,
                wait.as_secs(),
                startup::RESET
            );
            tokio::time::sleep(wait).await;
        }
        println!("{}--- [Replan] ---{}", startup::C_CYAN, startup::RESET);
        #[cfg(feature = "gui")]
        push_system_msg(gui_log, "--- [Replan] ---");
        let prev_unchecked = count_unchecked(&todo_content);
        let feedback = if app_feedback.is_empty() {
            None
        } else {
            Some(app_feedback.join("\n"))
        };
        let mut replan_failed = false;
        let updated = match run_replan_loop(ctx, gui_log, user_query, feedback.as_deref()).await {
            Ok(ReplanOutcome::Updated(u)) => {
                // The planner saw the pending app reports; drop them.
                app_feedback.clear();
                replan_consecutive_failures = 0;
                u
            }
            Ok(ReplanOutcome::Interrupted) => {
                println!(
                    "\n{}--- [Interrupted] Replan stopped. Run again to resume. ---{}",
                    startup::C_YELLOW,
                    startup::RESET
                );
                break;
            }
            Err(e) => {
                // First backoff site: wait before the in-round retry.
                replan_consecutive_failures += 1;
                let wait = replan_backoff(replan_consecutive_failures);
                println!(
                    "\n{}[Replan] LLM error: {}. Retrying once (backoff {}s).{}",
                    startup::C_RED,
                    e,
                    wait.as_secs(),
                    startup::RESET
                );
                tokio::time::sleep(wait).await;
                match run_replan_loop(ctx, gui_log, user_query, feedback.as_deref()).await {
                    Ok(ReplanOutcome::Updated(u)) => {
                        app_feedback.clear();
                        replan_consecutive_failures = 0;
                        u
                    }
                    Ok(ReplanOutcome::Interrupted) => {
                        println!(
                            "\n{}--- [Interrupted] Replan stopped. Run again to resume. ---{}",
                            startup::C_YELLOW,
                            startup::RESET
                        );
                        break;
                    }
                    Err(e2) => {
                        println!(
                            "\n{}[Replan] LLM error again: {}. Skipping task execution this round (next-task.md may be stale); the next replan will retry.{}",
                            startup::C_RED,
                            e2,
                            startup::RESET
                        );
                        replan_failed = true;
                        // Keep the current todo.md; no task runs with a
                        // possibly-stale plan or missing per-task brief.
                        todo_content.clone()
                    }
                }
            }
        };

        if check_completion(&updated) {
            if final_replan_confirms_completion(
                ctx,
                gui_log,
                user_query,
                build_replan_feedback(&app_feedback).as_deref(),
            )
            .await?
            {
                // Completion gate: the Goal deliverables must exist on disk
                // before the job exits as complete.
                verify_goal_deliverables()?;
                println!(
                    "{}--- [TODO LOOP] Completion reached during replan. Exiting. ---{}",
                    startup::C_CYAN,
                    startup::RESET
                );
                break;
            }
            // The final replan added tasks: continue with a fresh iteration.
            app_feedback.clear();
            replan_stalls = 0;
            continue;
        }

        let new_unchecked = count_unchecked(&updated);
        if new_unchecked >= prev_unchecked {
            replan_stalls += 1;
            // 0 = unlimited (never stop on replan stalls).
            if config.max_replan_attempts > 0 && replan_stalls >= config.max_replan_attempts {
                println!(
                    "\n{}--- [TODO LOOP] Replan stalled ({} attempts without reducing unchecked tasks). Stopping. ---{}",
                    startup::C_YELLOW,
                    replan_stalls,
                    startup::RESET
                );
                break;
            }
        } else {
            replan_stalls = 0;
        }

        if replan_failed {
            // The planner failed twice: skip the task and loop back to the
            // replan. The stall counter above bounds the retries.
            continue;
        }

        // --- Execute next task ---
        let tasks = parse_todo_md()?;
        let pending: Vec<&TaskItem> = tasks.iter().filter(|t| !t.done).collect();
        if pending.is_empty() {
            break;
        }

        let task = pending[0];
        println!(
            "\n{}--- [Task {}] {} ---{}",
            startup::C_CYAN,
            task.index + 1,
            task.description,
            startup::RESET
        );
        // Also push to GUI so the task header appears during execution.
        #[cfg(feature = "gui")]
        push_system_msg(
            gui_log,
            &format!("--- [Task {}] {} ---", task.index + 1, task.description),
        );

        // Snapshot the plan as this session sees it: every `./todo.md`
        // rewrite is validated against it (own `[x]` + added subtasks only;
        // the next replanner session clears it).
        let session_plan =
            std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;
        ctx.plan_guard = Some(PlanWriteGuard::capture(&session_plan, task.index));

        // Mode 2 system message: plan read via read_file, updated via write_file
        let system_msg = startup::system_message_mode2_task_loop(config);

        // Label by the task's todo.md index so resumed runs never collide.
        let task_label = format!("{}_task{}", config.session_label, task.index);
        let mut task_session = Session::new(task_label.clone(), system_msg);
        // Move a leftover session file from an earlier interrupted run aside.
        persistence::init_session(&task_label)?;

        let end_reason = run_reasoning_loop(
            ctx,
            &mut task_session,
            &format!("todo:task:{}", task.index),
            task_prompt(&task.description, user_query),
            attached_files.clone(),
        )
        .await?;

        if end_reason.is_completed() {
            // llm_guard_: condense an over-long handover report before logging.
            // Snapshot first: the rewrite may drop the `- Output:` declaration.
            let pre_condense = last_assistant_report(&task_session).map(str::to_string);
            llm_guard_condense_final_message(
                ctx,
                &mut task_session,
                "Handover Report",
                &["Status", "Output", "Findings", "Next"],
                HANDOVER_REPORT_MAX_CHARS,
            )
            .await;
            // Record the executor's report (last assistant message; never a
            // tool response). Output paths stay on an untruncated `outputs:`
            // line; condense cannot drop them (merge_condensed_report).
            let raw_note = last_assistant_report(&task_session).unwrap_or_default();
            let report = if raw_note.trim().is_empty() {
                "Status: (no report)".to_string()
            } else {
                merge_condensed_report(pre_condense.as_deref(), raw_note)
            };
            // Marker = the task's todo.md number (stable across runs).
            append_handover(&build_handover_entry(
                &format!("- Task {}", task.index + 1),
                &report,
            ))?;

            // Mechanical check: every Output path the executor declared must
            // exist on disk; missing ones are reported to the next replan so
            // the planner can add a fix task. The task does not count as
            // verified-complete until the declared paths exist.
            let missing = llm_guard_declared_outputs(&report);
            if !missing.is_empty() {
                println!(
                    "\n{}--- [App] Task {} declared Output paths that do not exist: {} ---{}",
                    startup::C_YELLOW,
                    task.index + 1,
                    missing.join(", "),
                    startup::RESET
                );
                app_feedback.push(format!(
                    "Task {} declared Output paths that do not exist: {}. Verify whether the task actually finished and add a fix task if needed.",
                    task.index + 1,
                    missing.join(", ")
                ));
                // Reject the false [x]: flip this task back to `- [ ]` so
                // check_completion (which counts only unchecked tasks) sees it
                // as pending and does not report a false completion. A read/
                // write I/O failure is non-fatal: the app_feedback above and
                // the Goal gate remain backstops.
                if let Err(e) = unmark_task_by_index(task.index) {
                    eprintln!("[App] unmark failed: {e}");
                }
            }

            persistence::archive_todo_session(&task_label, task.index)?;

            // Push task's LLM conversation to GUI log
            for msg in task_session.messages.iter().skip(1) {
                gui_log.messages.push(msg.clone());
            }
            // A task whose declared Output paths are missing is not
            // verified-complete (its false [x] was unmarked to pending above);
            // report it as not completed instead of done.
            let (done_msg, done_color) = if missing.is_empty() {
                (
                    format!("--- Task {} done ---", task.index + 1),
                    startup::C_CYAN,
                )
            } else {
                (
                    format!(
                        "--- Task {} not completed (declared Output paths missing: {}) ---",
                        task.index + 1,
                        missing.join(", ")
                    ),
                    startup::C_YELLOW,
                )
            };
            println!("{}{}{}", done_color, done_msg, startup::RESET);
            gui_log.messages.push(Message {
                role: "system".to_string(),
                content: done_msg.clone(),
                ..Default::default()
            });
            #[cfg(feature = "gui")]
            crate::model::LLM_STREAM_BUF
                .lock()
                .unwrap()
                .2
                .push_str(&format!("{}\n", done_msg));

            // A task whose declared Output paths are missing does not count
            // as verified-complete; the replan feedback carries the gap.
            if missing.is_empty() {
                completed += 1;
            }
        } else {
            // Distinguish Ctrl+C from LLM failure / limits.
            match end_reason {
                EndReason::LlmError => {
                    println!(
                        "\n{}--- [LLM Error] Task {} could not run (LLM connection error). Run again after recovery. ---{}",
                        startup::C_YELLOW,
                        task.index + 1,
                        startup::RESET
                    );
                }
                EndReason::Interrupted => {
                    println!(
                        "\n{}--- [Interrupted] Task {} was not completed. Run again to resume. ---{}",
                        startup::C_YELLOW,
                        task.index + 1,
                        startup::RESET
                    );
                }
                EndReason::EmptyLimit | EndReason::MaxTurns => {
                    println!(
                        "\n{}--- [Stopped] Task {} was not completed (limit reached). ---{}",
                        startup::C_YELLOW,
                        task.index + 1,
                        startup::RESET
                    );
                }
                EndReason::Completed => unreachable!("is_completed() was false"),
            }
            break;
        }
    }

    // Final check: did the LLM set the completion status?
    let final_todo = std::fs::read_to_string(TODO_MD_PATH).unwrap_or_default();
    if check_completion(&final_todo) {
        // Goal gate on every completion report, including loop-break paths
        // (e.g. Ctrl+C during a non-final replan) that skip the in-loop gate.
        check_goal_deliverables(&final_todo)?;
        let handover = std::fs::read_to_string(HANDOVER_MD_PATH).unwrap_or_default();
        let (goal, task_outputs) = llm_guard_verified_outputs(&final_todo, &handover);
        Ok(llm_guard_completion_report(
            &goal,
            &task_outputs,
            has_deliverables_section(&final_todo),
            count_task_items(&final_todo),
            llm_guard_tasks_declaring_outputs(&handover),
            llm_guard_unverifiable_declared(&final_todo, &handover),
            false,
        ))
    } else if completed > 0 {
        Ok(format!(
            "{} task(s) completed. Check ./todo.md for progress.",
            completed
        ))
    } else {
        Ok("No tasks completed. Check ./todo.md for pending items.".to_string())
    }
}

#[cfg(test)]
#[path = "tests/todo_test.rs"]
mod tests;
