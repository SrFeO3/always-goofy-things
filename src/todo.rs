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
use crate::compat_provider::LlmProvider;
use crate::llm_stats::Metrics;
use crate::model::{Message, Session, Settings};
use crate::persistence;
use crate::reasoning::{EndReason, run_reasoning_loop};
use crate::startup;
use crate::todo_guard::{
    build_handover_entry, llm_guard_declared_outputs, llm_guard_final_answer,
    llm_guard_handover_report,
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

/// Count unchecked tasks (`- [ ]`) in the given todo.md content.
fn count_unchecked(todo_md: &str) -> usize {
    let mut in_tasks = false;
    let mut count = 0;
    for line in todo_md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## Tasks") {
            in_tasks = true;
            continue;
        }
        if in_tasks && trimmed.starts_with("##") {
            break;
        }
        if in_tasks && trimmed.starts_with("- [ ]") {
            count += 1;
        }
    }
    count
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
         Neither the executor nor the planner edits this file; the application writes it.\n",
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
async fn run_replan_loop(
    config: &startup::Config,
    settings: &mut Settings,
    provider: LlmProvider,
    metrics: &mut Metrics,
    gui_log: &mut Session,
    user_input: &str,
    app_feedback: Option<&str>,
) -> Result<ReplanOutcome> {
    let q = user_input.trim();
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
        config,
        provider,
        &mut replan_session,
        settings,
        metrics,
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

    // The planner hands its plan-update notes over as its final message; the
    // application appends them to artifacts/handover.md exactly like a task
    // report (one line, capped at HANDOVER_REPORT_FUZZY_MAX_CHARS), so the
    // planner never edits the handover log itself.
    let note = replan_session
        .messages
        .last()
        .map(|m| m.content.as_str())
        .unwrap_or_default();
    let note = if note.trim().is_empty() {
        "Status: (no note)".to_string()
    } else {
        note.to_string()
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
async fn final_replan_confirms_completion(
    config: &startup::Config,
    settings: &mut Settings,
    provider: LlmProvider,
    metrics: &mut Metrics,
    gui_log: &mut Session,
    user_input: &str,
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
    let first = run_replan_loop(
        config,
        settings,
        provider,
        metrics,
        gui_log,
        user_input,
        app_feedback,
    )
    .await;
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
            match run_replan_loop(
                config,
                settings,
                provider,
                metrics,
                gui_log,
                user_input,
                app_feedback,
            )
            .await
            {
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
            // Ctrl+C: stop with exit 0 but state that completion was NOT
            // confirmed.
            println!(
                "\n{}[Replan] (final) Interrupted by user. Completion was NOT confirmed; todo.md is all-[x]. Run again to confirm.{}",
                startup::C_YELLOW,
                startup::RESET
            );
            Ok(true)
        }
    }
}

/// Run the Plan-Exec todo loop for Mode 1 and Mode 2.
pub(crate) async fn run_todo_loop(
    config: &startup::Config,
    provider: LlmProvider,
    settings: &mut Settings,
    metrics: &mut Metrics,
    gui_log: &mut Session,
    user_input: String,
    attached_files: Vec<AttachedFile>,
) -> Result<String> {
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

    // Resumed run: Mode 1 finishes with the Goal answer; Mode 2 falls through
    // so the loop's final-replan completion gate still runs.
    if pending.is_empty() && config.todo_mode != 2 {
        let content = std::fs::read_to_string(TODO_MD_PATH).unwrap_or_default();
        return Ok(llm_guard_final_answer(
            &content,
            "",
            "All tasks already completed.",
        ));
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

    if config.todo_mode == 2 {
        run_todo_loop_mode2(
            config,
            provider,
            settings,
            metrics,
            gui_log,
            &user_input,
            attached_files,
        )
        .await
    } else {
        run_todo_loop_mode1(
            config,
            provider,
            settings,
            metrics,
            gui_log,
            &user_input,
            attached_files,
            pending,
        )
        .await
    }
}

/// Build the task user message: the task description plus the `-q` addendum
/// (if any). The system prompt binds this to ./todo.md.
fn task_prompt(description: &str, user_input: &str) -> String {
    let q = user_input.trim();
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
async fn run_todo_loop_mode1(
    config: &startup::Config,
    provider: LlmProvider,
    settings: &mut Settings,
    metrics: &mut Metrics,
    gui_log: &mut Session,
    user_input: &str,
    attached_files: Vec<AttachedFile>,
    pending: Vec<&TaskItem>,
) -> Result<String> {
    let pending_count = pending.len();
    let mut completed = 0usize;
    let mut last_answer = String::new();

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
            config,
            provider,
            &mut task_session,
            settings,
            metrics,
            &format!("todo:task:{}", task.index),
            task_prompt(&task.description, user_input),
            attached_files.clone(),
        )
        .await?;

        if end_reason.is_completed() {
            // llm_guard_: condense an over-long handover report before logging.
            llm_guard_handover_report(config, provider, settings, metrics, &mut task_session).await;
            let raw_note = task_session
                .messages
                .last()
                .map(|m| m.content.as_str())
                .unwrap_or("Task completed.");

            mark_task_done(0)?;
            // Hand the executor's final report over to the free-form log.
            // Uses the stable task index (not the per-run counter) so that
            // resumed runs never collide with earlier entries of the same
            // task number. Declared Output paths are kept on an untruncated
            // `outputs:` line (the one-liner may cut them).
            append_handover(&build_handover_entry(
                &format!("- Task {}", task.index + 1),
                raw_note,
            ))?;

            // Mechanical check: every Output path the executor declared must
            // exist on disk. Mode 1 has no planner to fix it, so warn.
            let missing = llm_guard_declared_outputs(raw_note);
            if !missing.is_empty() {
                println!(
                    "\n{}--- [App] Task {} declared Output paths that do not exist: {} ---{}",
                    startup::C_YELLOW,
                    task_num,
                    missing.join(", "),
                    startup::RESET
                );
            }

            // Capture LLM's final message as the final answer
            if let Some(msg) = task_session.messages.last()
                && !msg.content.trim().is_empty()
            {
                last_answer = msg.content.clone();
            }

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
        llm_guard_final_answer(
            &final_todo,
            &last_answer,
            &format!("All {} tasks completed successfully.", pending_count),
        )
    } else {
        format!(
            "{} of {} tasks completed. Check ./todo.md for pending items.",
            completed, pending_count
        )
    })
}

/// Mode 2: Dynamic replanning with LLM-driven todo.md updates.
async fn run_todo_loop_mode2(
    config: &startup::Config,
    provider: LlmProvider,
    settings: &mut Settings,
    metrics: &mut Metrics,
    gui_log: &mut Session,
    user_input: &str,
    attached_files: Vec<AttachedFile>,
) -> Result<String> {
    let mut completed = 0usize;
    let mut replan_stalls = 0u32;
    // Consecutive failed replans; drives the backoff. Reset on success.
    let mut replan_consecutive_failures: u32 = 0;
    let mut last_answer = String::new();
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
            let feedback = if app_feedback.is_empty() {
                None
            } else {
                Some(app_feedback.join("\n"))
            };
            if final_replan_confirms_completion(
                config,
                settings,
                provider,
                metrics,
                gui_log,
                user_input,
                feedback.as_deref(),
            )
            .await?
            {
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
        let updated = match run_replan_loop(
            config,
            settings,
            provider,
            metrics,
            gui_log,
            user_input,
            feedback.as_deref(),
        )
        .await
        {
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
                match run_replan_loop(
                    config,
                    settings,
                    provider,
                    metrics,
                    gui_log,
                    user_input,
                    feedback.as_deref(),
                )
                .await
                {
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
            let feedback = if app_feedback.is_empty() {
                None
            } else {
                Some(app_feedback.join("\n"))
            };
            if final_replan_confirms_completion(
                config,
                settings,
                provider,
                metrics,
                gui_log,
                user_input,
                feedback.as_deref(),
            )
            .await?
            {
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

        // Mode 2 system message: plan read via read_file, updated via write_file
        let system_msg = startup::system_message_mode2_task_loop(config);

        // Label by the task's todo.md index so resumed runs never collide.
        let task_label = format!("{}_task{}", config.session_label, task.index);
        let mut task_session = Session::new(task_label.clone(), system_msg);
        // Move a leftover session file from an earlier interrupted run aside.
        persistence::init_session(&task_label)?;

        let end_reason = run_reasoning_loop(
            config,
            provider,
            &mut task_session,
            settings,
            metrics,
            &format!("todo:task:{}", task.index),
            task_prompt(&task.description, user_input),
            attached_files.clone(),
        )
        .await?;

        if end_reason.is_completed() {
            // llm_guard_: condense an over-long handover report before logging.
            llm_guard_handover_report(config, provider, settings, metrics, &mut task_session).await;
            // Capture LLM's final message as the final answer
            if let Some(msg) = task_session.messages.last()
                && !msg.content.trim().is_empty()
            {
                last_answer = msg.content.clone();
            }
            // Record the executor's handover report (its final message) so
            // the next replan has a record of what this task reported.
            // Declared Output paths are kept on an untruncated `outputs:` line.
            let raw_note = task_session
                .messages
                .last()
                .map(|m| m.content.as_str())
                .unwrap_or_default();
            let report = if raw_note.trim().is_empty() {
                "Status: (no report)".to_string()
            } else {
                raw_note.to_string()
            };
            // Marker = the task's todo.md number (stable across runs).
            append_handover(&build_handover_entry(
                &format!("- Task {}", task.index + 1),
                &report,
            ))?;

            // Mechanical check: every Output path the executor declared must
            // exist on disk; missing ones are reported to the next replan so
            // the planner can add a fix task.
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
            }

            persistence::archive_todo_session(&task_label, task.index)?;

            // Push task's LLM conversation to GUI log
            for msg in task_session.messages.iter().skip(1) {
                gui_log.messages.push(msg.clone());
            }
            let done_msg = format!("--- Task {} done ---", task.index + 1);
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
        Ok(llm_guard_final_answer(
            &final_todo,
            &last_answer,
            "All tasks completed.",
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
