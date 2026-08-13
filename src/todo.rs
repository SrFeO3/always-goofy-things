//! Plan-Execute task loop for multi-task execution with todo.md-based handover.
//!
//! Runs Mode 1 (Plan-Exec-Static) and Mode 2 (Plan-Exec-Dynamic)
//! for multi-task execution with LLM context reset between tasks.
//!
//! # Execution Flow
//!
//! ## Mode 1 (Plan-Exec-Static)
//!
//! 1. [User Input]     : Read the task plan from ./todo.md (error if missing).
//! 2. [Todo Loop]      : Recursive cycle through unchecked tasks, one per fresh context.
//!    - Parse State    : Identify the next unchecked task.
//!    - Task Loop      : Fresh executor reasoning loop (LLM Call -> Tool Exec -> Feedback); runs the single task from the user message only.
//!    - Store State    : The application updates todo.md (mark [x]) and resets the LLM context.
//! 3. [Final Answer]   : Notify the user that all tasks are complete.
//!
//! ## Mode 2 (Plan-Exec-Dynamic)
//!
//! 1. [User Input]     : Read the task plan from ./todo.md (error if missing).
//! 2. [Todo Loop]      : Recursive cycle: replan, then execute one task per fresh context.
//!    - Parse State    : Read todo.md to get current tasks and progress.
//!    - Replan Loop    : Fresh planner reasoning loop (LLM Call -> Tool Exec -> Feedback).
//!      Inspects artifacts/, then updates todo.md ([x] marks,
//!      task changes, completion status).
//!    - Task Loop      : Fresh executor reasoning loop (LLM Call -> Tool Exec -> Feedback); runs the single task from the user message only.
//!    - Store State    : The task LLM updates todo.md via write_file; the application
//!      resets the LLM context.
//! 3. [Final Answer]   : Present the final answer to the user.

use anyhow::{Context, Result};

use crate::attach::AttachedFile;
use crate::compat_provider::LlmProvider;
use crate::model::{Message, Metrics, Session, Settings};
use crate::persistence;
use crate::reasoning::run_reasoning_loop;
use crate::startup;

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

/// Mark the nth unchecked task as done by replacing `- [ ]` with `- [x]`
/// and appending notes to the Handover Notes section.
fn mark_task_done(task_index: usize, notes: &str) -> Result<()> {
    let content = std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;

    let mut new_lines: Vec<String> = Vec::new();
    let mut in_tasks_section = false;
    let mut unchecked_found: usize = 0;
    let mut in_handover_section = false;
    let mut notes_appended = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("## Tasks") {
            in_tasks_section = true;
            new_lines.push(line.to_string());
            continue;
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

        if trimmed.starts_with("## Handover Notes") {
            in_handover_section = true;
            new_lines.push(line.to_string());
            continue;
        }
        if in_handover_section
            && trimmed.starts_with("##")
            && !trimmed.starts_with("## Handover Notes")
        {
            if !notes_appended && !notes.trim().is_empty() {
                new_lines.push(format!("- {}", notes));
                notes_appended = true;
            }
            in_handover_section = false;
        }

        new_lines.push(line.to_string());
    }

    if in_handover_section && !notes_appended && !notes.trim().is_empty() {
        new_lines.push(format!("- {}", notes));
        notes_appended = true;
    }
    if !notes_appended && !notes.trim().is_empty() {
        new_lines.push(String::new());
        new_lines.push("## Handover Notes".to_string());
        new_lines.push(format!("- {}", notes));
    }

    let mut new_content = new_lines.join("\n");
    new_content.push('\n');

    std::fs::write(TODO_MD_PATH, &new_content).context("Failed to write ./todo.md")?;

    Ok(())
}

/// Check if the `## Status` section declares completion (Mode 2).
///
/// Returns `true` when both:
/// 1. A `Status:` line contains `Completed` (case-insensitive; the leading
///    `-` of the markdown list form is optional)
/// 2. No unchecked `- [ ]` tasks remain
fn check_completion(todo_md: &str) -> bool {
    let mut in_status_section = false;
    let mut status_completed = false;

    for line in todo_md.lines() {
        let trimmed = line.trim();

        if trimmed == "## Status" {
            in_status_section = true;
            continue;
        }
        if in_status_section && trimmed.starts_with("##") {
            break;
        }
        if in_status_section {
            let lower = trimmed.to_lowercase();
            // Accept both the plain form the prompt asks for (`Status: Completed`)
            // and the markdown list form (`- Status: Completed`).
            if lower.contains("status:") && lower.contains("completed") {
                status_completed = true;
            }
        }
    }

    status_completed && count_unchecked(todo_md) == 0
}

/// Replan phase (Mode 2): the planner reviews and updates todo.md.
///
/// Runs a full reasoning loop in a fresh planner session: the planner reads
/// ./todo.md with read_file, may inspect artifacts/ with tools, and saves the
/// updated plan to ./todo.md via write_file. Returns the todo.md content
/// after the loop.
async fn run_replan_loop(
    config: &startup::Config,
    settings: &mut Settings,
    provider: LlmProvider,
    metrics: &mut Metrics,
    gui_log: &mut Session,
    user_input: &str,
) -> Result<String> {
    let q = user_input.trim();
    let replan_prompt = if q.is_empty() {
        "Review and update `./todo.md` per the system instructions.".to_string()
    } else {
        format!(
            "Review and update `./todo.md` per the system instructions.\n\nAdditional user instructions: {}",
            q
        )
    };

    let system_msg = startup::system_message_mode2_replan();
    let mut replan_session = Session::new(format!("{}_replan", config.session_label), system_msg);
    run_reasoning_loop(
        config,
        provider,
        &mut replan_session,
        settings,
        metrics,
        replan_prompt,
        Vec::new(),
    )
    .await?;

    // Push the planner's conversation to the GUI log for visibility.
    for msg in replan_session.messages.iter().skip(1) {
        gui_log.messages.push(msg.clone());
    }

    // The planner updates ./todo.md via write_file during the loop; read it back.
    std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md after replan")
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

    if pending.is_empty() {
        return Ok("All tasks already completed.".to_string());
    }

    let pending_count = pending.len();
    let exec_line = format!(
        "Executing todo plan \"{}\" ({} mode) - {} tasks, {}/{} pending.",
        todo_title, mode_name, total, pending_count, total
    );
    println!("\n{}{}{}\n", startup::C_CYAN, exec_line, startup::RESET);
    #[cfg(feature = "gui")]
    push_system_msg_blank(gui_log, &exec_line, false);

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

        let system_msg = startup::system_message_mode1_task_loop();

        let task_label = format!("{}_task{}", config.session_label, task.index);
        let mut task_session = Session::new(task_label.clone(), system_msg);

        let done = run_reasoning_loop(
            config,
            provider,
            &mut task_session,
            settings,
            metrics,
            task_prompt(&task.description, user_input),
            attached_files.clone(),
        )
        .await?;

        if done {
            let raw_note = task_session
                .messages
                .last()
                .map(|m| m.content.as_str())
                .unwrap_or("Task completed.");
            // Collapse to single line: truncate to 300 chars, replace newlines
            let note: String = raw_note.replace('\n', " ").chars().take(300).collect();
            let note = if raw_note.chars().count() > 300 {
                format!("{}...", note)
            } else {
                note
            };

            mark_task_done(0, &note)?;

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
            println!(
                "\n{}--- [Interrupted] Task {} was not completed. Run again to resume. ---{}",
                startup::C_YELLOW,
                task.description,
                startup::RESET
            );
            break;
        }
    }

    Ok(if completed == pending_count {
        if !last_answer.is_empty() {
            last_answer
        } else {
            format!("All {} tasks completed successfully.", pending_count)
        }
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
    let mut task_index = 0usize;
    let mut last_answer = String::new();

    loop {
        task_index += 1;
        let todo_content =
            std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;

        // Check if the completion status is already set
        if check_completion(&todo_content) {
            println!(
                "{}--- [TODO LOOP] Completion reached. Exiting. ---{}",
                startup::C_CYAN,
                startup::RESET
            );
            break;
        }

        // --- Replan ---
        println!("{}--- [Replan] ---{}", startup::C_CYAN, startup::RESET);
        #[cfg(feature = "gui")]
        push_system_msg(gui_log, "--- [Replan] ---");
        let prev_unchecked = count_unchecked(&todo_content);
        let updated =
            match run_replan_loop(config, settings, provider, metrics, gui_log, user_input).await {
                Ok(u) => u,
                Err(e) => {
                    println!(
                        "\n{}[Replan] LLM error: {}. Continuing with current plan.{}",
                        startup::C_RED,
                        e,
                        startup::RESET
                    );
                    // Continue with current todo.md
                    todo_content.clone()
                }
            };

        if check_completion(&updated) {
            println!(
                "{}--- [TODO LOOP] Completion reached during replan. Exiting. ---{}",
                startup::C_CYAN,
                startup::RESET
            );
            break;
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
            task_index,
            task.description,
            startup::RESET
        );
        // Also push to GUI so the task header appears during execution.
        #[cfg(feature = "gui")]
        push_system_msg(
            gui_log,
            &format!("--- [Task {}] {} ---", task_index, task.description),
        );

        // Mode 2 system message: plan read via read_file, updated via write_file
        let system_msg = startup::system_message_mode2_task_loop();

        let task_label = format!("{}_task{}", config.session_label, task_index);
        let mut task_session = Session::new(task_label.clone(), system_msg);

        let done = run_reasoning_loop(
            config,
            provider,
            &mut task_session,
            settings,
            metrics,
            task_prompt(&task.description, user_input),
            attached_files.clone(),
        )
        .await?;

        if done {
            // Capture LLM's final message as the final answer
            if let Some(msg) = task_session.messages.last()
                && !msg.content.trim().is_empty()
            {
                last_answer = msg.content.clone();
            }
            persistence::archive_todo_session(&task_label, task_index)?;

            // Push task's LLM conversation to GUI log
            for msg in task_session.messages.iter().skip(1) {
                gui_log.messages.push(msg.clone());
            }
            let done_msg = format!("--- Task {} done ---", task_index);
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
            println!(
                "\n{}--- [Interrupted] Task {} was not completed. Run again to resume. ---{}",
                startup::C_YELLOW,
                task_index,
                startup::RESET
            );
            break;
        }
    }

    // Final check: did the LLM set the completion status?
    let final_todo = std::fs::read_to_string(TODO_MD_PATH).unwrap_or_default();
    if check_completion(&final_todo) {
        if last_answer.is_empty() {
            Ok("All tasks completed (Status: Completed).".to_string())
        } else {
            Ok(last_answer)
        }
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
