//! Todo jobs.
//!
//! Runs `todo.json` plans through `run_job`: static in order, replan with
//! a planner round before every task. State is a temp JSON file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::job::{Check, JobOutcome, JobStatus, Task, VerifyResult};
use crate::model::Message;
use crate::reasoning::LoopCtx;
use crate::session::{ReportRule, SessionOutcome, SessionSpec, ToolPolicy, run_session};
use crate::todo_guard::llm_guard_declared_outputs;
use crate::{persistence, startup};

/// Planner report budget: plans are JSON and must survive intact.
pub(crate) const PLANNER_REPORT_MAX_CHARS: usize = 2000;

/// Handover budget into the planner prompt.
const PLANNER_HANDOVER_CHARS: usize = 4000;

/// Temp state dir inside the workspace.
const TODO_STATE_DIR: &str = ".todo";

/// Fence marker carrying the planner's revised plan.
pub(crate) const PLAN_FENCE: &str = "```todo-plan";

/// One planned task with parsed checks plus raw strings for display.
#[derive(Debug, Clone)]
pub(crate) struct PlanTask {
    pub id: String,
    pub description: String,
    pub verify_raw: Vec<String>,
    pub checks: Vec<Check>,
}

/// A loaded `todo.json` plan.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub goal: String,
    pub tasks: Vec<PlanTask>,
    pub deliverables: Vec<String>,
}

/// Load and validate a `todo.json` plan. Check strings parse now so bad
/// plans fail before any session runs.
pub(crate) fn load_plan(path: &Path) -> Result<Plan> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("cannot read plan {}: {:#}", path.display(), e))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| anyhow!("bad plan {}: {:#}", path.display(), e))?;
    let goal = v
        .get("goal")
        .and_then(|g| g.as_str())
        .filter(|g| !g.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "bad plan {}: `goal` must be a non-empty string",
                path.display()
            )
        })?;
    let tasks_raw = v.get("tasks").and_then(|t| t.as_array()).ok_or_else(|| {
        anyhow!(
            "bad plan {}: `tasks` must be a non-empty array",
            path.display()
        )
    })?;
    if tasks_raw.is_empty() {
        anyhow::bail!(
            "bad plan {}: `tasks` must be a non-empty array",
            path.display()
        );
    }
    let mut ids = HashSet::new();
    let mut tasks = Vec::new();
    for (i, t) in tasks_raw.iter().enumerate() {
        let id = t
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "bad plan {}: tasks[{}] needs a non-empty `id`",
                    path.display(),
                    i
                )
            })?;
        if !ids.insert(id.to_string()) {
            anyhow::bail!("bad plan {}: duplicate task id {:?}", path.display(), id);
        }
        let description = t
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "bad plan {}: task {:?} needs a non-empty `description`",
                    path.display(),
                    id
                )
            })?;
        let verify_raw: Vec<String> = match t.get("verify") {
            None => Vec::new(),
            Some(v) => v
                .as_array()
                .ok_or_else(|| {
                    anyhow!(
                        "bad plan {}: task {:?} `verify` must be an array",
                        path.display(),
                        id
                    )
                })?
                .iter()
                .map(|s| {
                    s.as_str().map(str::to_string).ok_or_else(|| {
                        anyhow!(
                            "bad plan {}: task {:?} `verify` entries must be strings",
                            path.display(),
                            id
                        )
                    })
                })
                .collect::<Result<_>>()?,
        };
        if verify_raw.is_empty() {
            anyhow::bail!(
                "bad plan {}: task {:?} needs at least one `verify` check",
                path.display(),
                id
            );
        }
        let mut checks = Vec::new();
        for s in &verify_raw {
            checks.push(
                Check::parse(s)
                    .map_err(|e| anyhow!("bad plan {}: task {:?}: {:#}", path.display(), id, e))?,
            );
        }
        tasks.push(PlanTask {
            id: id.to_string(),
            description: description.to_string(),
            verify_raw,
            checks,
        });
    }
    let deliverables: Vec<String> = match v.get("deliverables") {
        None => Vec::new(),
        Some(d) => d
            .as_array()
            .ok_or_else(|| {
                anyhow!(
                    "bad plan {}: `deliverables` must be an array",
                    path.display()
                )
            })?
            .iter()
            .map(|s| {
                s.as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        anyhow!(
                            "bad plan {}: `deliverables` entries must be non-empty strings",
                            path.display()
                        )
                    })
            })
            .collect::<Result<_>>()?,
    };
    Ok(Plan {
        goal: goal.to_string(),
        tasks,
        deliverables,
    })
}

/// The live plan path next to the input: `<stem>.live.json`.
fn live_plan_path(plan_path: &Path) -> PathBuf {
    let parent = plan_path.parent().unwrap_or_else(|| Path::new(""));
    let stem = plan_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("todo");
    parent.join(format!("{}.live.json", stem))
}

/// Plan definition hash (goal + tasks + deliverables); P1-3 compares it to the
/// seed so a hand-edited `todo.json` can trigger a reset.
fn plan_hash(plan: &Plan) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    plan.goal.hash(&mut h);
    for t in &plan.tasks {
        t.id.hash(&mut h);
        t.description.hash(&mut h);
        t.verify_raw.hash(&mut h);
    }
    plan.deliverables.hash(&mut h);
    h.finish()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum TaskStatus {
    #[default]
    Pending,
    Done,
    /// Dropped by a replan (scope reduction); terminal, never re-executed.
    Cancelled,
}

#[derive(Debug, Clone, Default)]
struct TaskState {
    status: TaskStatus,
    attempts: u32,
    handover: Option<String>,
    error: Option<String>,
    cancelled_reason: Option<String>,
}

fn to_task(t: &PlanTask) -> Task {
    Task {
        id: t.id.clone(),
        description: t.description.clone(),
        verify: t.checks.clone(),
    }
}

/// First undone task in plan order.
fn next_undone(plan: &Plan, states: &HashMap<String, TaskState>) -> Option<Task> {
    plan.tasks
        .iter()
        .find(|t| {
            !matches!(
                states.get(&t.id).map(|s| s.status),
                Some(TaskStatus::Done) | Some(TaskStatus::Cancelled)
            )
        })
        .map(to_task)
}

/// Number of tasks with `status == done` (the loop's progress metric).
fn count_done(plan: &Plan, states: &HashMap<String, TaskState>) -> usize {
    plan.tasks
        .iter()
        .filter(|t| matches!(states.get(&t.id).map(|s| s.status), Some(TaskStatus::Done)))
        .count()
}

/// Replan proposal shape inside the ```todo-plan fence.
#[derive(Debug, serde::Deserialize)]
struct ProposedPlan {
    tasks: Vec<ProposedTask>,
    #[serde(default)]
    deliverables: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
struct ProposedTask {
    id: String,
    description: String,
    #[serde(default)]
    verify: Vec<String>,
    #[serde(default)]
    brief: Option<String>,
}

/// Take the LAST fenced block (a revision supersedes examples/older drafts).
pub(crate) fn extract_plan_block(report: &str) -> Option<&str> {
    let mut last = None;
    let mut rest = report;
    while let Some(i) = rest.find(PLAN_FENCE) {
        let after = &rest[i + PLAN_FENCE.len()..];
        if let Some(end) = after.find("```") {
            last = Some(&after[..end]);
            rest = &after[end + 3..];
        } else {
            break;
        }
    }
    last
}

/// Validate a proposal against terminal-state immutability and check syntax.
/// `scope` decides what happens when the proposal drops unfinished tasks;
/// `confirm` is only called for `Ask` (stdin y/N in production, a stub in tests).
fn apply_replan(
    plan: &mut Plan,
    states: &mut HashMap<String, TaskState>,
    proposed: ProposedPlan,
    scope: ReplanScope,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> Result<String> {
    if proposed.tasks.is_empty() {
        anyhow::bail!("replan rejected: empty task list");
    }
    let done_ids: HashSet<&str> = states
        .iter()
        .filter(|(_, s)| s.status == TaskStatus::Done)
        .map(|(id, _)| id.as_str())
        .collect();
    let mut seen = HashSet::new();
    let mut tasks = Vec::new();
    for p in &proposed.tasks {
        if p.id.trim().is_empty() || p.description.trim().is_empty() {
            anyhow::bail!("replan rejected: task with empty id/description");
        }
        if !seen.insert(p.id.clone()) {
            anyhow::bail!("replan rejected: duplicate task id {:?}", p.id);
        }
        if p.verify.is_empty() {
            anyhow::bail!(
                "replan rejected: task {:?} needs at least one verify check",
                p.id
            );
        }
        let mut checks = Vec::new();
        for s in &p.verify {
            checks.push(
                Check::parse(s)
                    .map_err(|e| anyhow!("replan rejected: task {:?}: {:#}", p.id, e))?,
            );
        }
        tasks.push(PlanTask {
            id: p.id.clone(),
            description: p.description.clone(),
            verify_raw: p.verify.clone(),
            checks,
        });
    }
    for done in &done_ids {
        match tasks.iter().find(|t| &t.id == done) {
            Some(t) => {
                let old = plan.tasks.iter().find(|t| &t.id == done);
                match old {
                    None => anyhow::bail!("replan rejected: done task {:?} not found", done),
                    Some(o) if o.description != t.description => {
                        anyhow::bail!("replan rejected: done task {:?} altered or renamed", done)
                    }
                    Some(o) if o.verify_raw != t.verify_raw => {
                        anyhow::bail!("replan rejected: done task {:?} verify changed", done)
                    }
                    Some(_) => {}
                }
            }
            None => anyhow::bail!("replan rejected: done task {:?} removed", done),
        }
    }
    // Cancelled tasks are terminal too: if the proposal keeps one, it must not
    // alter it. Omitting it is fine (it is carried over below).
    for t in &tasks {
        let Some(st) = states.get(&t.id) else {
            continue;
        };
        if st.status != TaskStatus::Cancelled {
            continue;
        }
        let Some(old) = plan.tasks.iter().find(|o| o.id == t.id) else {
            continue;
        };
        if old.description != t.description {
            anyhow::bail!(
                "replan rejected: cancelled task {:?} altered or renamed",
                t.id
            );
        }
        if old.verify_raw != t.verify_raw {
            anyhow::bail!("replan rejected: cancelled task {:?} verify changed", t.id);
        }
    }

    // Unfinished tasks dropped by the revision are scope reductions: they need
    // approval and are recorded as `cancelled` (never silently disappear).
    let removed: Vec<PlanTask> = {
        let proposed_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        plan.tasks
            .iter()
            .filter(|t| !proposed_ids.contains(t.id.as_str()))
            .filter(|t| {
                !matches!(
                    states.get(&t.id).map(|s| s.status),
                    Some(TaskStatus::Done) | Some(TaskStatus::Cancelled)
                )
            })
            .cloned()
            .collect()
    };
    if !removed.is_empty() {
        let list = removed
            .iter()
            .map(|t| format!("{} ({})", t.id, t.description))
            .collect::<Vec<_>>()
            .join(", ");
        match scope {
            ReplanScope::Deny => {
                anyhow::bail!("replan rejected: would remove unfinished task(s): {}", list)
            }
            ReplanScope::Ask => {
                if !confirm(&format!(
                    "Replan removes unfinished task(s): {}. Cancel them?",
                    list
                )) {
                    anyhow::bail!(
                        "replan rejected: cancellation of unfinished task(s) not approved: {}",
                        list
                    );
                }
            }
            ReplanScope::Auto => {}
        }
        for t in &removed {
            let st = states.entry(t.id.clone()).or_default();
            st.status = TaskStatus::Cancelled;
            st.cancelled_reason = Some("removed by replan".to_string());
        }
    }

    // Keep cancelled tasks in the live plan (full history), appended after the
    // active tasks; the planner may omit them from later revisions.
    for old in &plan.tasks {
        let cancelled = matches!(
            states.get(&old.id).map(|s| s.status),
            Some(TaskStatus::Cancelled)
        );
        if cancelled && !tasks.iter().any(|t| t.id == old.id) {
            tasks.push(old.clone());
        }
    }

    if let Some(deliverables) = proposed.deliverables {
        if deliverables.iter().any(|d| d.trim().is_empty()) {
            anyhow::bail!("replan rejected: empty deliverable path");
        }
        plan.deliverables = deliverables;
    }
    let added = tasks
        .iter()
        .filter(|t| !plan.tasks.iter().any(|o| o.id == t.id))
        .count();
    plan.tasks = tasks;
    let active = plan
        .tasks
        .iter()
        .filter(|t| {
            !matches!(
                states.get(&t.id).map(|s| s.status),
                Some(TaskStatus::Cancelled)
            )
        })
        .count();
    Ok(format!(
        "plan applied ({} tasks, {} added, {} cancelled)",
        active,
        added,
        removed.len()
    ))
}

/// Planner instruction: goal, current plan with status marks, all handovers,
/// app feedback, and the note. It also asks the planner for the next task's brief.
fn planner_instruction(
    plan: &Plan,
    states: &HashMap<String, TaskState>,
    feedback: &[String],
    note: &str,
) -> String {
    let mut lines = Vec::new();
    for t in &plan.tasks {
        let mark = match states.get(&t.id).map(|s| s.status) {
            Some(TaskStatus::Done) => "[x]",
            Some(TaskStatus::Cancelled) => "[~]",
            _ => "[ ]",
        };
        lines.push(format!("{} {}: {}", mark, t.id, t.description));
    }
    let mut reports: Vec<String> = Vec::new();
    for t in &plan.tasks {
        if let Some(r) = states.get(&t.id).and_then(|s| s.handover.clone()) {
            reports.push(format!("{}: {}", t.id, r));
        }
    }
    let mut base = format!(
        "Goal: {}\nPlan:\n{}\nReply with a {} fenced block ",
        plan.goal,
        lines.join("\n"),
        PLAN_FENCE
    );
    base.push_str(
        "{\"tasks\": [{\"id\", \"description\", \"verify\": [check strings], \"brief\"}], \
         \"deliverables\"?} with the FULL revised list, then notes. Done ([x]) and cancelled \
         ([~]) tasks are immutable; removing an unfinished task cancels it (reason recorded). \
         New ids unique; checks parse as nonempty/contains/sql. Give the NEXT task a short \
         `brief` (scope, files to read, warnings): the executor sees only that brief plus its \
         own task. Always include the block.",
    );
    if !feedback.is_empty() {
        base.push_str(&format!("\nApp feedback: {}", feedback.join(" / ")));
    }
    if !note.trim().is_empty() {
        base.push_str(&format!("\nAdditional instructions: {}", note));
    }
    crate::session::assemble_instruction(
        &base,
        &reports.iter().map(String::as_str).collect::<Vec<_>>(),
        PLANNER_HANDOVER_CHARS,
    )
}

/// Evaluate a task's checks mechanically (sql has no database in a todo job).
fn eval_task(task: &Task) -> VerifyResult {
    let no_sql =
        |_: &str| -> Result<bool> { anyhow::bail!("sql checks need a database (todo has none)") };
    for check in &task.verify {
        match check.eval(&no_sql) {
            VerifyResult::Pass => {}
            fail => return fail,
        }
    }
    VerifyResult::Pass
}

/// Two-tier finalize: every task terminal (done/cancelled), deliverables fatal,
/// declared task outputs warn.
fn finalize_plan(plan: &Plan, states: &HashMap<String, TaskState>) -> JobOutcome {
    let incomplete: Vec<&str> = plan
        .tasks
        .iter()
        .filter(|t| {
            !matches!(
                states.get(&t.id).map(|s| s.status),
                Some(TaskStatus::Done) | Some(TaskStatus::Cancelled)
            )
        })
        .map(|t| t.id.as_str())
        .collect();
    if !incomplete.is_empty() {
        return JobOutcome {
            status: JobStatus::Failed,
            summary: format!(
                "incomplete tasks({}): {}",
                incomplete.len(),
                incomplete.join(", ")
            ),
        };
    }
    let mut missing: Vec<String> = Vec::new();
    for d in &plan.deliverables {
        if !matches!(std::fs::metadata(d), Ok(m) if m.len() > 0) {
            missing.push(d.clone());
        }
    }
    if !missing.is_empty() {
        return JobOutcome {
            status: JobStatus::Failed,
            summary: format!(
                "missing deliverables({}): {}",
                missing.len(),
                missing.join(", ")
            ),
        };
    }
    let mut declared_missing: Vec<String> = Vec::new();
    for t in &plan.tasks {
        if let Some(r) = states.get(&t.id).and_then(|s| s.handover.clone()) {
            declared_missing.extend(llm_guard_declared_outputs(&r));
        }
    }
    declared_missing.sort();
    declared_missing.dedup();
    let mut summary = format!(
        "Completed: deliverables({}) {}",
        plan.deliverables.len(),
        plan.deliverables.join(", ")
    );
    if !declared_missing.is_empty() {
        summary.push_str(&format!(
            "\nWarnings: task outputs missing({}): {}",
            declared_missing.len(),
            declared_missing.join(", ")
        ));
    }
    let cancelled: Vec<String> = plan
        .tasks
        .iter()
        .filter(|t| {
            matches!(
                states.get(&t.id).map(|s| s.status),
                Some(TaskStatus::Cancelled)
            )
        })
        .map(|t| {
            let reason = states
                .get(&t.id)
                .and_then(|s| s.cancelled_reason.as_deref())
                .unwrap_or("cancelled");
            format!("{} ({})", t.id, reason)
        })
        .collect();
    if !cancelled.is_empty() {
        summary.push_str(&format!(
            "\nCancelled({}): {}",
            cancelled.len(),
            cancelled.join(", ")
        ));
    }
    JobOutcome {
        status: JobStatus::Completed,
        summary,
    }
}

/// Record one attempt into the in-memory map (`Done` only on a clean pass).
fn record_task(
    states: &mut HashMap<String, TaskState>,
    task_id: &str,
    outcome: &SessionOutcome,
    verdict: &VerifyResult,
) {
    let entry = states.entry(task_id.to_string()).or_default();
    entry.attempts += 1;
    entry.handover = outcome.report.clone();
    match verdict {
        VerifyResult::Pass => {
            entry.status = TaskStatus::Done;
            entry.error = None;
        }
        VerifyResult::Warn { reason } => {
            entry.status = TaskStatus::Done;
            entry.error = Some(reason.clone());
        }
        // A failed check leaves the task pending so the loop retries it.
        VerifyResult::Fail { reason } => {
            entry.status = TaskStatus::Pending;
            entry.error = Some(reason.clone());
        }
    }
}

/// One executor session's inputs from a display-ready task.
#[allow(clippy::too_many_arguments)]
fn executor_spec(
    session_label: &str,
    system: &Message,
    goal: &str,
    notes: &str,
    brief: Option<&str>,
    pos: usize,
    task: &PlanTask,
    policy: ToolPolicy,
) -> SessionSpec {
    SessionSpec {
        label: task_session_label(session_label, pos, &task.id),
        system: system.clone(),
        instruction: build_task_instruction(goal, notes, brief, task),
        allow_tools: policy,
        report_rule: TODO_REPORT_RULE,
        max_turns: None,
    }
}

/// y/N confirmation for a replan scope reduction (stdin; EOF = no).
fn confirm_cancel_removal(prompt: &str) -> bool {
    use std::io::Write as _;
    print!("\x1b[93m{}\x1b[0m (y/N) ", prompt);
    if std::io::stdout().flush().is_err() {
        return false;
    }
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    #[cfg(feature = "gui")]
    let input = crate::gui::decode_child_input(input);
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Run a `todo.json` plan on the status-driven loop. `todo.json` is an
/// immutable seed; `<stem>.live.json` holds the live plan (plan + status +
/// handover + brief) and is the single source of truth across reruns.
pub(crate) async fn run_todo(
    ctx: &mut LoopCtx<'_>,
    plan_path: &Path,
    options: &TodoOptions,
) -> Result<String> {
    let live_path = live_plan_path(plan_path);
    // Per-task retry is gone (retries are loop-driven via `max_stalls`); the
    // option stays only for CLI compatibility.
    let _ = options.max_retries;
    let (mut plan, mut states, mut briefs, mut revision, seed_hash) =
        resolve_initial(&live_path, plan_path, options.reset, options.generated)?;
    let session_label = ctx.config.session_label.clone();
    let system_exec = startup::system_message_todo_task(
        ctx.config,
        match options.executor_policy {
            ToolPolicy::Inherit => None,
            ToolPolicy::AllowList(list) => Some(list),
        },
    );
    let system_planner = matches!(options.mode, TodoMode::Replan)
        .then(|| startup::system_message_todo_planner(ctx.config));
    let mut feedback: Vec<String> = Vec::new();
    let mut stalls: u32 = 0;
    let mut round: u32 = 0;
    let mut confirm_scope = |prompt: &str| confirm_cancel_removal(prompt);

    loop {
        // Replan step (replan mode only): revise the plan + next-task brief.
        if let Some(system) = &system_planner {
            round += 1;
            let instruction = planner_instruction(&plan, &states, &feedback, &options.note);
            let spec = SessionSpec {
                label: format!("{}_plan-{}", session_label, round),
                system: system.clone(),
                instruction,
                allow_tools: ToolPolicy::AllowList(startup::TODO_PLANNER_TOOLS),
                report_rule: ReportRule {
                    title: "Replan Notes",
                    fields: &["Plan", "Notes"],
                    max_chars: PLANNER_REPORT_MAX_CHARS,
                },
                max_turns: None,
            };
            let outcome = run_session(ctx, spec).await?;
            if !outcome.end_reason.is_completed() {
                save_live(
                    &live_path, plan_path, &plan, &states, &briefs, revision, seed_hash,
                )?;
                return Ok(format!(
                    "interrupted during replan ({:?}); rerun to resume",
                    outcome.end_reason
                ));
            }
            let text = outcome
                .raw_report
                .as_deref()
                .or(outcome.report.as_deref())
                .unwrap_or("");
            let proposed =
                extract_plan_block(text).and_then(|b| serde_json::from_str::<ProposedPlan>(b).ok());
            match proposed {
                Some(p) => {
                    let new_briefs = collect_briefs(&p);
                    match apply_replan(
                        &mut plan,
                        &mut states,
                        p,
                        options.replan_scope,
                        &mut confirm_scope,
                    ) {
                        Ok(note) => {
                            briefs = new_briefs;
                            revision += 1;
                            feedback.clear();
                            feedback.push(note);
                            save_live(
                                &live_path, plan_path, &plan, &states, &briefs, revision, seed_hash,
                            )?;
                        }
                        Err(e) => feedback.push(e.to_string()),
                    }
                }
                None => feedback
                    .push("missing/bad ```todo-plan block; repeat the full list".to_string()),
            }
        }

        // Execute the first pending task; none means the job is done.
        let Some(task) = next_undone(&plan, &states) else {
            break;
        };
        let done_before = count_done(&plan, &states);

        let pos = plan.tasks.iter().position(|t| t.id == task.id).unwrap_or(0);
        let plan_task = plan
            .tasks
            .iter()
            .find(|t| t.id == task.id)
            .cloned()
            .ok_or_else(|| anyhow!("task {:?} vanished from the plan", task.id))?;
        let spec = executor_spec(
            &session_label,
            &system_exec,
            &plan.goal,
            &options.note,
            briefs.get(&task.id).map(String::as_str),
            pos,
            &plan_task,
            options.executor_policy,
        );
        let outcome = run_session(ctx, spec).await?;
        if !outcome.end_reason.is_completed() {
            record_task(
                &mut states,
                &task.id,
                &outcome,
                &VerifyResult::Fail {
                    reason: format!("interrupted: {:?}", outcome.end_reason),
                },
            );
            save_live(
                &live_path, plan_path, &plan, &states, &briefs, revision, seed_hash,
            )?;
            return Ok(format!(
                "interrupted at {} ({:?}); rerun to resume",
                task.id, outcome.end_reason
            ));
        }

        let verdict = eval_task(&task);
        record_task(&mut states, &task.id, &outcome, &verdict);
        let _ = persistence::archive_todo_session(&outcome.label, 0);
        save_live(
            &live_path, plan_path, &plan, &states, &briefs, revision, seed_hash,
        )?;

        // Progress = a task became done; otherwise count a bounded stall.
        if count_done(&plan, &states) > done_before {
            stalls = 0;
        } else {
            stalls += 1;
            if options.max_stalls > 0 && stalls > options.max_stalls {
                let out = finalize_plan(&plan, &states);
                return Ok(format!(
                    "{} (stalled {} rounds without progress; rerun to continue)",
                    out.summary, stalls
                ));
            }
        }
    }

    let out = finalize_plan(&plan, &states);
    Ok(out.summary)
}

/// Dry-run text: tasks, checks, deliverables. No execution, no state.
pub(crate) fn dry_run_todo(plan_path: &Path) -> Result<String> {
    let plan = load_plan(plan_path)?;
    let mut out = format!(
        "[job dry-run] {} tasks, {} deliverables\n",
        plan.tasks.len(),
        plan.deliverables.len()
    );
    for t in &plan.tasks {
        out.push_str(&format!(" - {}: {}\n", t.id, t.description));
        for c in &t.verify_raw {
            out.push_str(&format!("     check: {}\n", c));
        }
    }
    for d in &plan.deliverables {
        out.push_str(&format!(" deliverable: {}\n", d));
    }
    Ok(out)
}

/// Progress text from the live plan (missing live = seed, all pending).
pub(crate) fn todo_status(plan_path: &Path) -> Result<String> {
    todo_status_in(plan_path)
}

/// Progress from the live plan (tests point it at scratch dirs).
pub(crate) fn todo_status_in(plan_path: &Path) -> Result<String> {
    let live_path = live_plan_path(plan_path);
    let (plan, states) = match load_live_only(&live_path)? {
        Some((plan, states, ..)) => (plan, states),
        None => (load_plan(plan_path)?, HashMap::new()),
    };
    let done = count_done(&plan, &states);
    let mut out = format!("[job status] {}/{} tasks done\n", done, plan.tasks.len());
    for t in &plan.tasks {
        let line = match states.get(&t.id) {
            Some(s) if s.status == TaskStatus::Cancelled => format!(
                " - [~] {} (cancelled: {})\n",
                t.id,
                s.cancelled_reason.as_deref().unwrap_or("removed")
            ),
            Some(s) if s.status == TaskStatus::Done => {
                format!(" - [x] {} (attempts {})\n", t.id, s.attempts)
            }
            Some(s) if s.attempts == 0 => format!(" - [ ] {} (not started)\n", t.id),
            Some(s) => format!(
                " - [ ] {} (attempts {}, error: {})\n",
                t.id,
                s.attempts,
                s.error.as_deref().unwrap_or("none")
            ),
            None => format!(" - [ ] {} (not started)\n", t.id),
        };
        out.push_str(&line);
    }
    Ok(out)
}

/// Write a planner-fenced plan to disk for generated jobs (analyze).
/// Validates through the same gate as replan, then re-validates the file.
/// Missing deliverables fall back to `default_deliverable` so finalize
/// keeps its teeth. Overwrites: generated plans live in managed space.
/// Called only from the kb analyze path (`--features kb`).
#[cfg_attr(not(feature = "kb"), allow(dead_code))]
pub(crate) fn write_generated_plan(
    goal: &str,
    fence_block: &str,
    path: &Path,
    default_deliverable: &str,
) -> Result<Plan> {
    let proposed: ProposedPlan = serde_json::from_str(fence_block)
        .map_err(|e| anyhow!("bad generated plan JSON: {:#}", e))?;
    let mut plan = Plan {
        goal: goal.to_string(),
        tasks: Vec::new(),
        deliverables: Vec::new(),
    };
    let mut states: HashMap<String, TaskState> = HashMap::new();
    let mut confirm = |_: &str| false;
    apply_replan(
        &mut plan,
        &mut states,
        proposed,
        ReplanScope::Deny,
        &mut confirm,
    )?;
    if plan.deliverables.is_empty() {
        plan.deliverables = vec![default_deliverable.to_string()];
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let doc = serde_json::json!({
        "goal": plan.goal,
        "tasks": plan.tasks.iter().map(|t| serde_json::json!({
            "id": t.id, "description": t.description, "verify": t.verify_raw,
        })).collect::<Vec<_>>(),
        "deliverables": plan.deliverables,
    });
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    load_plan(path)
}

/// Scaffold template for `/job init` (refuses overwrite).
pub(crate) const TODO_TEMPLATE: &str = r#"{
  "goal": "TODO: describe the job goal",
  "tasks": [
    {"id": "t1", "description": "TODO: first task", "verify": ["nonempty artifacts/t1.done"]},
    {"id": "t2", "description": "TODO: second task", "verify": ["nonempty artifacts/t2.done"]}
  ],
  "deliverables": ["artifacts/final.md"]
}
"#;

/// Write the scaffold unless the path already exists.
pub(crate) fn init_plan(path: &Path) -> Result<()> {
    if path.exists() {
        anyhow::bail!("refusing to overwrite {}", path.display());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, TODO_TEMPLATE)?;
    Ok(())
}

/// Delete stale state/live files under `.todo/`: `all` removes everything,
/// otherwise only files whose seed plan is gone. Generated analyze plans
/// (`kb-analyze-*.json`, no `.state.json`/`.live.json` suffix) survive.
pub(crate) fn clean_states(workspace: &Path, all: bool) -> Result<Vec<PathBuf>> {
    let dir = workspace.join(TODO_STATE_DIR);
    let mut deleted = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(deleted);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let field = if name.ends_with(".state.json") {
            "plan"
        } else if name.ends_with(".live.json") {
            "seed"
        } else {
            continue;
        };
        let orphan = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get(field).and_then(|p| p.as_str()).map(str::to_string))
            .map(|p| !Path::new(&p).exists())
            .unwrap_or(true);
        if all || orphan {
            std::fs::remove_file(&path)?;
            deleted.push(path);
        }
    }
    Ok(deleted)
}

/// Filesystem-safe session label per task attempt stream.
fn task_session_label(session_label: &str, pos: usize, task_id: &str) -> String {
    let san: String = task_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(24)
        .collect();
    format!("{}_task{}-{}", session_label, pos, san)
}

/// Executor instruction: own task plus goal and the planner's brief (if any).
fn build_task_instruction(goal: &str, note: &str, brief: Option<&str>, task: &PlanTask) -> String {
    let mut s = format!("Task ({}): {}", task.id, task.description);
    s.push_str(&format!("\nGoal: {}", goal));
    if !task.verify_raw.is_empty() {
        s.push_str(&format!("\nChecks: {}", task.verify_raw.join(", ")));
    }
    if let Some(b) = brief.filter(|b| !b.trim().is_empty()) {
        s.push_str(&format!("\nBrief: {}", b));
    }
    if !note.trim().is_empty() {
        s.push_str(&format!("\nAdditional instructions: {}", note));
    }
    s.push_str("\nDeliverable: satisfy the checks; close with a Handover Report.");
    s
}

/// Run modes (one implementation; the enumerator differs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoMode {
    Static,
    Replan,
}

/// How a replan may shrink scope (drop unfinished tasks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ReplanScope {
    /// Apply the removal and record it as `cancelled`.
    Auto,
    /// Ask y/N before cancelling unfinished tasks (default).
    #[default]
    Ask,
    /// Reject any revision that drops an unfinished task.
    Deny,
}

impl ReplanScope {
    pub(crate) fn parse(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(Self::Auto),
            "ask" => Ok(Self::Ask),
            "deny" => Ok(Self::Deny),
            other => anyhow::bail!("--replan-scope must be auto|ask|deny, got {:?}", other),
        }
    }
}

/// Todo run knobs.
#[derive(Debug, Clone)]
pub(crate) struct TodoOptions {
    pub mode: TodoMode,
    pub max_retries: u32,
    pub max_stalls: u32,
    pub note: String,
    pub executor_policy: ToolPolicy,
    pub reset: bool,
    pub generated: bool,
    pub replan_scope: ReplanScope,
}

impl Default for TodoOptions {
    fn default() -> Self {
        Self {
            mode: TodoMode::Static,
            max_retries: 1,
            max_stalls: 3,
            note: String::new(),
            executor_policy: ToolPolicy::Inherit,
            reset: false,
            generated: false,
            replan_scope: ReplanScope::Ask,
        }
    }
}

/// Executor report contract (same shape as the legacy handover report,
/// so `Output:` declarations keep parsing).
pub(crate) const TODO_REPORT_RULE: ReportRule = ReportRule {
    title: "Handover Report",
    fields: &["Status", "Output", "Findings", "Next"],
    max_chars: crate::todo_guard::HANDOVER_REPORT_MAX_CHARS,
};

/// Persisted live plan: the definition plus per-task runtime fields.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct LiveDoc {
    goal: String,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    seed: String,
    #[serde(default)]
    seed_hash: u64,
    tasks: Vec<LiveTask>,
    #[serde(default)]
    deliverables: Vec<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct LiveTask {
    id: String,
    description: String,
    #[serde(default)]
    verify: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    brief: Option<String>,
    #[serde(default)]
    status: TaskStatus,
    #[serde(default)]
    attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handover: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cancelled_reason: Option<String>,
}

/// In-memory live plan: plan, per-task state, briefs, revision, seed hash.
type LiveState = (
    Plan,
    HashMap<String, TaskState>,
    HashMap<String, String>,
    u64,
    u64,
);

/// Load the live plan if present. Rebuilds the in-memory plan + states + briefs.
fn load_live_only(live_path: &Path) -> Result<Option<LiveState>> {
    let Ok(text) = std::fs::read_to_string(live_path) else {
        return Ok(None);
    };
    let doc: LiveDoc = serde_json::from_str(&text)
        .map_err(|e| anyhow!("bad live plan {}: {:#}", live_path.display(), e))?;
    let mut tasks = Vec::new();
    let mut states = HashMap::new();
    let mut briefs = HashMap::new();
    for t in doc.tasks {
        let LiveTask {
            id,
            description,
            verify,
            brief,
            status,
            attempts,
            handover,
            error,
            cancelled_reason,
        } = t;
        let mut checks = Vec::new();
        for s in &verify {
            checks.push(
                Check::parse(s)
                    .map_err(|e| anyhow!("bad live plan {}: {:#}", live_path.display(), e))?,
            );
        }
        tasks.push(PlanTask {
            id: id.clone(),
            description: description.clone(),
            verify_raw: verify,
            checks,
        });
        states.insert(
            id.clone(),
            TaskState {
                status,
                attempts,
                handover,
                error,
                cancelled_reason,
            },
        );
        if let Some(b) = brief.filter(|b| !b.trim().is_empty()) {
            briefs.insert(id, b);
        }
    }
    let plan = Plan {
        goal: doc.goal,
        tasks,
        deliverables: doc.deliverables,
    };
    Ok(Some((plan, states, briefs, doc.revision, doc.seed_hash)))
}

/// Starting state: resume the live plan (unless `reset`), or seed from the
/// input plan. A changed seed with an existing live plan warns (user-authored
/// plans only; generated plans change every run, so the check is skipped).
fn resolve_initial(
    live_path: &Path,
    plan_path: &Path,
    reset: bool,
    generated: bool,
) -> Result<LiveState> {
    let live = if reset {
        None
    } else {
        load_live_only(live_path)?
    };
    if let Some(live) = live {
        let seed_changed = !generated
            && load_plan(plan_path)
                .map(|seed| plan_hash(&seed) != live.4)
                .unwrap_or(false);
        if seed_changed {
            eprintln!(
                "[job] the plan seed changed since the live plan was created; \
                 continuing with the live plan. Use --reset to regenerate it."
            );
        }
        return Ok(live);
    }
    let plan = load_plan(plan_path)?;
    let seed_hash = plan_hash(&plan);
    let states = HashMap::new();
    let briefs = HashMap::new();
    save_live(live_path, plan_path, &plan, &states, &briefs, 0, seed_hash)?;
    Ok((plan, states, briefs, 0, seed_hash))
}

/// Persist the live plan atomically (tmp + rename).
fn save_live(
    live_path: &Path,
    seed_path: &Path,
    plan: &Plan,
    states: &HashMap<String, TaskState>,
    briefs: &HashMap<String, String>,
    revision: u64,
    seed_hash: u64,
) -> Result<()> {
    let tasks: Vec<LiveTask> = plan
        .tasks
        .iter()
        .map(|t| {
            let st = states.get(&t.id);
            LiveTask {
                id: t.id.clone(),
                description: t.description.clone(),
                verify: t.verify_raw.clone(),
                brief: briefs.get(&t.id).cloned(),
                status: st.map(|s| s.status).unwrap_or(TaskStatus::Pending),
                attempts: st.map(|s| s.attempts).unwrap_or(0),
                handover: st.and_then(|s| s.handover.clone()),
                error: st.and_then(|s| s.error.clone()),
                cancelled_reason: st.and_then(|s| s.cancelled_reason.clone()),
            }
        })
        .collect();
    let doc = LiveDoc {
        goal: plan.goal.clone(),
        revision,
        seed: seed_path.to_string_lossy().into_owned(),
        seed_hash,
        tasks,
        deliverables: plan.deliverables.clone(),
    };
    if let Some(parent) = live_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = live_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)?)?;
    std::fs::rename(&tmp, live_path)?;
    Ok(())
}

/// Briefs from a proposal (the planner's per-task context for the executor).
fn collect_briefs(proposed: &ProposedPlan) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for p in &proposed.tasks {
        if let Some(b) = p.brief.as_ref().filter(|b| !b.trim().is_empty()) {
            m.insert(p.id.clone(), b.clone());
        }
    }
    m
}

#[cfg(test)]
#[path = "tests/todo_job_test.rs"]
mod tests;
