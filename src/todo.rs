//! Plan-Execute task loop for multi-step task execution with todo.md-based handover.
//!
//! Runs Mode 1 (Plan-Exec-Static) and Mode 2 (Plan-Exec-Dynamic)
//! for multi-step task execution with LLM context reset between steps.
//!
//! # Execution Flow
//!
//! ## Mode 1 (Plan-Exec-Static)
//!
//! 1. [User Input]     : Read the task plan from ./todo.md (error if missing).
//! 2. [Todo Loop]      : Recursive cycle through unchecked tasks until all are complete.
//!    - Parse State    : Identify the next unchecked task.
//!    - Reasoning Loop : Recursive cycle for this task (LLM Call -> Tool Exec -> Feedback).
//!    - Store State    : Update todo.md (mark [x]) and reset LLM context.
//! 3. [Final Answer]   : Notify the user that all tasks are complete.
//!
//! ## Mode 2 (Plan-Exec-Dynamic)
//!
//! 1. [User Input]     : Read the task plan from ./todo.md (error if missing).
//! 2. [Todo Loop]      : Recursive cycle until the LLM declares completion.
//!    - Parse State    : Read todo.md to get current tasks and progress.
//!    - Replan         : LLM reviews and rewrites todo.md if needed (lightweight, 1 turn).
//!    - Reasoning Loop : Recursive cycle for this task (LLM Call -> Tool Exec -> Feedback).
//!    - Store State    : Update todo.md (LLM rewrites with results) and reset LLM context.
//! 3. [Final Answer]   : Present the final conclusion to the user.

use anyhow::{Context, Result};

use crate::attach::AttachedFile;
use crate::compat_provider::LlmProvider;
use crate::model::{Message, Metrics, Session, Settings};
use crate::persistence;
use crate::reasoning::{call_llm, run_reasoning_loop};
use crate::startup;

const TODO_MD_PATH: &str = "./todo.md";

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

/// Check if the `## Conclusion` section declares completion (Mode 2).
///
/// Returns `true` when both:
/// 1. `Status:` starts with `Completed` (case-insensitive)
/// 2. No unchecked `- [ ]` tasks remain
fn check_conclusion(todo_md: &str) -> bool {
    let mut in_conclusion = false;
    let mut status_completed = false;

    for line in todo_md.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("## Conclusion") {
            in_conclusion = true;
            continue;
        }
        if in_conclusion && trimmed.starts_with("##") {
            break;
        }
        if in_conclusion {
            let lower = trimmed.to_lowercase();
            if lower.starts_with("- status:") && lower.contains("completed") {
                status_completed = true;
            }
        }
    }

    status_completed && count_unchecked(todo_md) == 0
}

/// Lightweight replan step (Mode 2): ask the LLM to review and update todo.md.
///
/// Calls `call_llm` directly (1 turn), executes `write_file` for `./todo.md`
/// without confirmation, and returns the updated todo.md content.
async fn run_replan_loop(
    config: &startup::Config,
    settings: &mut Settings,
    provider: LlmProvider,
    todo_content: &str,
) -> Result<String> {
    let replan_prompt = format!(
        "You are a task planner. Review the current todo.md below.\n\n\
        ## Instructions\n\
        - If any task in `## Tasks` is completed (e.g., its output file exists), mark it as `[x]`.\n\
        - If ALL tasks are `[x]` AND the Goal is achieved, update `## Conclusion`:\n\
          `Status: Completed (No further investigation needed)`.\n\
        - If more work is needed, update `## Tasks`: add, remove, reorder, or split tasks.\n\
        - Use `write_file` to save `./todo.md`. Return ONLY the updated file -- no explanation.\n\n\
        ## Current todo.md\n\n{}",
        todo_content
    );

    let messages = vec![Message {
        role: "user".to_string(),
        content: replan_prompt,
        ..Default::default()
    }];

    let (assistant_msg, _usage) = call_llm(config, settings, provider, &messages).await?;

    // Execute write_file for ./todo.md without confirmation (internal bookkeeping)
    if let Some(tool_calls) = &assistant_msg.tool_calls {
        for call in tool_calls {
            if call.function.name == "write_file" {
                let args = &call.function.arguments;
                // Normalize: OpenAI wraps args in a JSON string
                let args_val: &serde_json::Value = args;
                let path = args_val.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if (path == "./todo.md" || path == "todo.md")
                    && let Some(content) = args_val.get("content").and_then(|v| v.as_str())
                {
                    std::fs::write("./todo.md", content)
                        .context("Failed to write ./todo.md during replan")?;
                    println!(
                        "{}[Replan] ./todo.md updated.{}",
                        startup::C_DIM_GRAY,
                        startup::RESET
                    );
                }
            }
        }
    }

    std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md after replan")
}

/// Run the Plan-Exec todo loop for Mode 1 and Mode 2.
pub(crate) async fn run_todo_loop(
    config: &startup::Config,
    provider: LlmProvider,
    settings: &mut Settings,
    metrics: &mut Metrics,
    _user_input: String,
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
    println!(
        "\n{0}Executing todo plan \"{2}\" ({1} mode).{3}\n",
        startup::C_CYAN,
        mode_name,
        todo_title,
        startup::RESET
    );

    if pending.is_empty() {
        return Ok("All tasks already completed.".to_string());
    }

    let pending_count = pending.len();
    println!(
        "{}--- [TODO LOOP] {} tasks ({}/{} pending) ---{}",
        startup::C_CYAN,
        total,
        pending_count,
        total,
        startup::RESET
    );

    if config.todo_mode == 2 {
        run_todo_loop_mode2(config, provider, settings, metrics, attached_files).await
    } else {
        run_todo_loop_mode1(config, provider, settings, metrics, attached_files, pending).await
    }
}

/// Mode 1: Static sequential execution with agent-side `[x]` marking.
async fn run_todo_loop_mode1(
    config: &startup::Config,
    provider: LlmProvider,
    settings: &mut Settings,
    metrics: &mut Metrics,
    attached_files: Vec<AttachedFile>,
    pending: Vec<&TaskItem>,
) -> Result<String> {
    let pending_count = pending.len();
    let mut completed = 0usize;

    for task in &pending {
        let task_num = completed + 1;
        println!(
            "\n{}--- [Task {}/{}] {} ---{}",
            startup::C_MAGENTA,
            task_num,
            pending_count,
            task.description,
            startup::RESET
        );

        let todo_content =
            std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;
        let system_msg = startup::system_message_with_todo(&todo_content);

        let task_label = format!("{}_task{}", config.session_label, task.index);
        let mut task_session = Session::new(task_label.clone(), system_msg);

        let done = run_reasoning_loop(
            config,
            provider,
            &mut task_session,
            settings,
            metrics,
            task.description.clone(),
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
            persistence::archive_todo_session(&task_label, task.index)?;
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
        format!("All {} tasks completed successfully.", pending_count)
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
    attached_files: Vec<AttachedFile>,
) -> Result<String> {
    let mut completed = 0usize;
    let mut replan_stalls = 0u32;
    let mut step = 0usize;

    loop {
        step += 1;
        let todo_content =
            std::fs::read_to_string(TODO_MD_PATH).context("Failed to read ./todo.md")?;

        // Check if already concluded
        if check_conclusion(&todo_content) {
            println!(
                "{}--- [TODO LOOP] Conclusion reached. Exiting. ---{}",
                startup::C_CYAN,
                startup::RESET
            );
            break;
        }

        // --- Replan ---
        let prev_unchecked = count_unchecked(&todo_content);
        let updated = match run_replan_loop(config, settings, provider, &todo_content).await {
            Ok(u) => u,
            Err(e) => {
                println!(
                    "\n{}[Replan] LLM error: {}. Continuing with current plan.{} ",
                    startup::C_YELLOW,
                    e,
                    startup::RESET
                );
                todo_content.clone()
            }
        };

        if check_conclusion(&updated) {
            println!(
                "{}--- [TODO LOOP] Conclusion reached during replan. Exiting. ---{}",
                startup::C_CYAN,
                startup::RESET
            );
            break;
        }

        let new_unchecked = count_unchecked(&updated);
        if new_unchecked >= prev_unchecked {
            replan_stalls += 1;
            if replan_stalls >= config.max_replan_attempts {
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
            "\n{}--- [Step {}] {} ---{}",
            startup::C_MAGENTA,
            step,
            task.description,
            startup::RESET
        );

        // Mode 2 system message: full todo.md + instruction to update it via write_file
        let system_msg = startup::system_message_with_todo_mode2(&updated);

        let task_label = format!("{}_step{}", config.session_label, step);
        let mut task_session = Session::new(task_label.clone(), system_msg);

        let done = run_reasoning_loop(
            config,
            provider,
            &mut task_session,
            settings,
            metrics,
            task.description.clone(),
            attached_files.clone(),
        )
        .await?;

        if done {
            persistence::archive_todo_session(&task_label, step)?;
            completed += 1;
        } else {
            println!(
                "\n{}--- [Interrupted] Step {} was not completed. Run again to resume. ---{}",
                startup::C_YELLOW,
                step,
                startup::RESET
            );
            break;
        }
    }

    // Final check: did the LLM write a Conclusion?
    let final_todo = std::fs::read_to_string(TODO_MD_PATH).unwrap_or_default();
    if check_conclusion(&final_todo) {
        Ok("All tasks completed (Conclusion reached).".to_string())
    } else if completed > 0 {
        Ok(format!(
            "{} step(s) completed. Check ./todo.md for progress.",
            completed
        ))
    } else {
        Ok("No steps completed. Check ./todo.md for pending items.".to_string())
    }
}
