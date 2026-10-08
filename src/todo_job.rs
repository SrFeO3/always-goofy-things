//! Structured todo jobs (todo-refine Phase 4).
//!
//! Runs `todo.json` plans through `run_job`: static in order, replan with
//! a planner round before every task. State is a temp JSON file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::job::{
    Check, Enumerator, JobOptions, JobOutcome, JobStatus, Store, Task, Verifier, VerifyResult,
    run_job,
};
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
const PLAN_FENCE: &str = "```todo-plan";

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

/// Stable job id from the plan path (canonicalized; falls back to as-given).
pub(crate) fn job_id_for(plan_path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let key = std::fs::canonicalize(plan_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| plan_path.to_string_lossy().into_owned());
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// State file inside the workspace (`.todo/<job-id>.state.json`).
pub(crate) fn state_file_for(workspace: &Path, job_id: &str) -> PathBuf {
    workspace
        .join(TODO_STATE_DIR)
        .join(format!("{}.state.json", job_id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum TaskStatus {
    Pending,
    Done,
    Failed,
}

impl Default for TaskStatus {
    fn default() -> Self {
        TaskStatus::Pending
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TaskState {
    #[serde(default)]
    status: TaskStatus,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    report: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Static enumerator: plan order, skipping done.
struct StaticEnumerator {
    plan: Plan,
    state_path: PathBuf,
}

fn count_undone(plan: &Plan, states: &HashMap<String, TaskState>) -> usize {
    plan.tasks
        .iter()
        .filter(|t| !matches!(states.get(&t.id).map(|s| s.status), Some(TaskStatus::Done)))
        .count()
}

fn to_task(t: &PlanTask) -> Task {
    Task {
        id: t.id.clone(),
        description: t.description.clone(),
        verify: t.checks.clone(),
    }
}

/// First undone task in plan order (pure core of both enumerators).
fn next_undone(plan: &Plan, states: &HashMap<String, TaskState>) -> Option<Task> {
    plan.tasks
        .iter()
        .find(|t| !matches!(states.get(&t.id).map(|s| s.status), Some(TaskStatus::Done)))
        .map(to_task)
}

impl Enumerator for StaticEnumerator {
    async fn next_task(&mut self, _ctx: &mut LoopCtx<'_>) -> Result<Option<Task>> {
        Ok(next_undone(&self.plan, &load_task_states(&self.state_path)))
    }
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
}

/// Take the LAST fenced block (a revision supersedes examples/older drafts).
fn extract_plan_block(report: &str) -> Option<&str> {
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

/// Validate a proposal against done-state immutability and check syntax.
fn apply_replan(
    plan: &mut Plan,
    states: &HashMap<String, TaskState>,
    proposed: ProposedPlan,
) -> Result<String> {
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
                if old.map_or(true, |o| o.description != t.description) {
                    anyhow::bail!("replan rejected: done task {:?} altered or renamed", done);
                }
            }
            None => anyhow::bail!("replan rejected: done task {:?} removed", done),
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
    Ok(format!(
        "plan applied ({} tasks, {} added)",
        plan.tasks.len(),
        added
    ))
}

/// Dynamic enumerator: planner round before every task, plus one final
/// confirmation round when nothing remains.
struct DynamicEnumerator {
    plan: Plan,
    state_path: PathBuf,
    session_label: String,
    system_planner: Message,
    note: String,
    max_stalls: u32,
    stalls: u32,
    last_unchecked: usize,
    rounds: u32,
    feedback: Vec<String>,
}

impl DynamicEnumerator {
    fn planner_instruction(&self, states: &HashMap<String, TaskState>) -> String {
        let mut lines = Vec::new();
        for t in &self.plan.tasks {
            let mark = match states.get(&t.id).map(|s| s.status) {
                Some(TaskStatus::Done) => "[x]",
                _ => "[ ]",
            };
            lines.push(format!("{} {}: {}", mark, t.id, t.description));
        }
        let mut reports: Vec<String> = Vec::new();
        for t in &self.plan.tasks {
            if let Some(r) = states.get(&t.id).and_then(|s| s.report.clone()) {
                reports.push(format!("{}: {}", t.id, r));
            }
        }
        let mut base = format!(
            "Goal: {}\nPlan:\n{}\nReply with a {} fenced block ",
            self.plan.goal,
            lines.join("\n"),
            PLAN_FENCE
        );
        base.push_str(
            "{\"tasks\": [{\"id\", \"description\", \"verify\": [check strings]}], \"deliverables\"?} \
             with the FULL revised list, then notes. Done ([x]) tasks are immutable; new ids unique; \
             checks parse as exists/nonempty/contains/sql. Always include the block.",
        );
        if !self.feedback.is_empty() {
            base.push_str(&format!("\nApp feedback: {}", self.feedback.join(" / ")));
        }
        if !self.note.trim().is_empty() {
            base.push_str(&format!("\nAdditional instructions: {}", self.note));
        }
        crate::session::assemble_instruction(
            &base,
            &reports.iter().map(String::as_str).collect::<Vec<_>>(),
            PLANNER_HANDOVER_CHARS,
        )
    }

    /// One planner round: propose, validate, apply. Returns Ok on applied
    /// plans; failed rounds (LLM fault, bad fence) count as stalls.
    async fn replan_round(&mut self, ctx: &mut LoopCtx<'_>) -> Result<()> {
        let states = load_task_states(&self.state_path);
        self.rounds += 1;
        let spec = SessionSpec {
            label: format!("{}_plan-{}", self.session_label, self.rounds),
            system: self.system_planner.clone(),
            instruction: self.planner_instruction(&states),
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
            return self.note_stall(format!(
                "planner interrupted ({:?}); execute next pending anyway",
                outcome.end_reason
            ));
        }
        let text = outcome
            .raw_report
            .as_deref()
            .or(outcome.report.as_deref())
            .unwrap_or("");
        let Some(block) = extract_plan_block(text) else {
            return self.note_stall("missing ```todo-plan block; repeat the full list".to_string());
        };
        let proposed: ProposedPlan = match serde_json::from_str(block) {
            Ok(p) => p,
            Err(e) => {
                return self.note_stall(format!("bad plan JSON: {:#}", e));
            }
        };
        match apply_replan(&mut self.plan, &states, proposed) {
            Ok(note) => {
                self.feedback.clear();
                self.feedback.push(note);
                let cur = count_undone(&self.plan, &load_task_states(&self.state_path));
                if cur >= self.last_unchecked {
                    return self.note_stall("unchecked count did not shrink".to_string());
                }
                self.stalls = 0;
                self.last_unchecked = cur;
                Ok(())
            }
            Err(e) => self.note_stall(e.to_string()),
        }
    }

    /// Count a non-progress round; breach the stall limit into a job abort.
    fn note_stall(&mut self, reason: String) -> Result<()> {
        self.stalls += 1;
        self.feedback.push(reason);
        if self.max_stalls > 0 && self.stalls > self.max_stalls {
            anyhow::bail!("replan stalled {} rounds without progress", self.stalls);
        }
        Ok(())
    }
}

impl Enumerator for DynamicEnumerator {
    async fn next_task(&mut self, ctx: &mut LoopCtx<'_>) -> Result<Option<Task>> {
        // A failed planner round still executes the next pending task
        // (static fallback); only stall-limit breaches abort via `?`.
        self.replan_round(ctx).await?;
        Ok(next_undone(&self.plan, &load_task_states(&self.state_path)))
    }
}

/// Check runner: file/SQL checks; SQL has no database here and fails shut.
struct TodoVerifier;

impl Verifier for TodoVerifier {
    fn check(&self, _ctx: &LoopCtx<'_>, task: &Task) -> Result<VerifyResult> {
        let no_sql = |_: &str| -> Result<bool> {
            anyhow::bail!("sql checks need a database (todo has none)")
        };
        for check in &task.verify {
            match check.eval(&no_sql) {
                VerifyResult::Pass => {}
                fail => return Ok(fail),
            }
        }
        Ok(VerifyResult::Pass)
    }

    fn finalize(&self, _ctx: &LoopCtx<'_>) -> Result<JobOutcome> {
        // run_todo closes through outcome_with_finalize with the live plan
        // (replan mutates it); this neutral close only satisfies the trait.
        Ok(JobOutcome {
            status: JobStatus::Completed,
            summary: "tasks done; finalize follows".to_string(),
        })
    }
}

/// Two-tier finalize: deliverables fatal, declared task outputs warn.
fn finalize_plan(plan: &Plan, states: &HashMap<String, TaskState>) -> JobOutcome {
    let mut missing: Vec<String> = Vec::new();
    for d in &plan.deliverables {
        let bad = match std::fs::metadata(d) {
            Ok(m) if m.len() > 0 => false,
            _ => true,
        };
        if bad {
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
        if let Some(r) = states.get(&t.id).and_then(|s| s.report.clone()) {
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
    JobOutcome {
        status: JobStatus::Completed,
        summary,
    }
}

/// State recorder plus per-task session archive.
struct TodoStore {
    state_path: PathBuf,
    job_id: String,
    plan_path: PathBuf,
    session_label: String,
    order: Vec<String>,
}

impl TodoStore {
    fn record_state(
        &self,
        task: &Task,
        outcome: &SessionOutcome,
        verdict: &VerifyResult,
    ) -> Result<()> {
        let mut states = load_task_states(&self.state_path);
        let entry = states.entry(task.id.clone()).or_insert(TaskState {
            status: TaskStatus::Pending,
            attempts: 0,
            report: None,
            error: None,
        });
        entry.attempts += 1;
        entry.report = outcome.report.clone();
        match verdict {
            VerifyResult::Pass => {
                entry.status = TaskStatus::Done;
                entry.error = None;
            }
            VerifyResult::Warn { reason } => {
                entry.status = TaskStatus::Done;
                entry.error = Some(reason.clone());
            }
            VerifyResult::Fail { reason } => {
                entry.status = TaskStatus::Failed;
                entry.error = Some(reason.clone());
            }
        }
        save_task_states(&self.state_path, &self.job_id, &self.plan_path, &states)
    }
}

impl Store for TodoStore {
    fn record(
        &self,
        _ctx: &LoopCtx<'_>,
        task: &Task,
        outcome: &SessionOutcome,
        verdict: &VerifyResult,
    ) -> Result<()> {
        self.record_state(task, outcome, verdict)?;
        let pos = self.order.iter().position(|id| id == &task.id).unwrap_or(0);
        let _ = persistence::archive_todo_session(
            &task_session_label(&self.session_label, pos, &task.id),
            pos,
        );
        Ok(())
    }
}

/// One executor session's inputs from a display-ready task.
#[allow(clippy::too_many_arguments)]
fn executor_spec(
    session_label: &str,
    system: &Message,
    goal: &str,
    notes: &str,
    prev: Option<&str>,
    pos: usize,
    task: &PlanTask,
    policy: ToolPolicy,
) -> SessionSpec {
    SessionSpec {
        label: task_session_label(session_label, pos, &task.id),
        system: system.clone(),
        instruction: build_task_instruction(goal, notes, prev, task),
        allow_tools: policy,
        report_rule: TODO_REPORT_RULE,
        max_turns: None,
    }
}

/// Run a `todo.json` plan; returns the completion summary. Completed jobs
/// delete their state file (artifacts stay); anything else keeps it.
/// (`gui_log` push is skipped: task sessions are ephemeral and stdout
/// already carries progress. Revisit in Phase 5 if the GUI needs it.)
pub(crate) async fn run_todo(
    ctx: &mut LoopCtx<'_>,
    plan_path: &Path,
    options: &TodoOptions,
) -> Result<String> {
    let plan = load_plan(plan_path)?;
    let job_id = job_id_for(plan_path);
    let state_path = state_file_for(Path::new("."), &job_id);
    let session_label = ctx.config.session_label.clone();
    let system_exec = startup::system_message_todo_task(
        ctx.config,
        match options.executor_policy {
            ToolPolicy::Inherit => None,
            ToolPolicy::AllowList(list) => Some(list),
        },
    );
    let order: Vec<String> = plan.tasks.iter().map(|t| t.id.clone()).collect();
    let job_options = JobOptions {
        max_retries: options.max_retries,
    };
    let notes = options.note.clone();
    let outcome = match options.mode {
        TodoMode::Static => {
            let mut enumerator = StaticEnumerator {
                plan: plan.clone(),
                state_path: state_path.clone(),
            };
            let goal = plan.goal.clone();
            let store = TodoStore {
                state_path: state_path.clone(),
                job_id: job_id.clone(),
                plan_path: plan_path.to_path_buf(),
                session_label: session_label.clone(),
                order: order.clone(),
            };
            let outcome = run_job(
                ctx,
                &mut enumerator,
                &TodoVerifier,
                &store,
                &job_options,
                |task| {
                    let states = load_task_states(&state_path);
                    let plan_task = plan
                        .tasks
                        .iter()
                        .find(|t| t.id == task.id)
                        .ok_or_else(|| anyhow!("task {:?} vanished from the plan", task.id))?;
                    let pos = order.iter().position(|id| id == &task.id).unwrap_or(0);
                    Ok(executor_spec(
                        &session_label,
                        &system_exec,
                        &goal,
                        &notes,
                        prev_report(&plan, &states, &task.id).as_deref(),
                        pos,
                        plan_task,
                        options.executor_policy,
                    ))
                },
            )
            .await?;
            outcome_with_finalize(outcome, &plan, &state_path)
        }
        TodoMode::Replan => {
            let states = load_task_states(&state_path);
            let unchecked = count_undone(&plan, &states);
            let mut enumerator = DynamicEnumerator {
                plan: plan.clone(),
                state_path: state_path.clone(),
                session_label: session_label.clone(),
                system_planner: startup::system_message_todo_planner(ctx.config),
                note: notes.clone(),
                max_stalls: options.max_stalls,
                stalls: 0,
                last_unchecked: unchecked,
                rounds: 0,
                feedback: Vec::new(),
            };
            let store = TodoStore {
                state_path: state_path.clone(),
                job_id: job_id.clone(),
                plan_path: plan_path.to_path_buf(),
                session_label: session_label.clone(),
                order: order.clone(),
            };
            let goal = plan.goal.clone();
            let outcome = run_job(
                ctx,
                &mut enumerator,
                &TodoVerifier,
                &store,
                &job_options,
                |task| {
                    let states = load_task_states(&state_path);
                    // Replan may have reshaped the plan: display the live
                    // checks (re-rendered) with handover from the base order.
                    let plan_task = PlanTask {
                        id: task.id.clone(),
                        description: task.description.clone(),
                        verify_raw: task.verify.iter().map(|c| c.to_dsl()).collect(),
                        checks: task.verify.clone(),
                    };
                    let pos = order
                        .iter()
                        .position(|id| id == &task.id)
                        .unwrap_or(order.len());
                    Ok(executor_spec(
                        &session_label,
                        &system_exec,
                        &goal,
                        &notes,
                        prev_report(&plan, &states, &task.id).as_deref(),
                        pos,
                        &plan_task,
                        options.executor_policy,
                    ))
                },
            )
            .await?;
            outcome_with_finalize(outcome, &enumerator.plan, &state_path)
        }
    };
    if outcome.status == JobStatus::Completed {
        let _ = std::fs::remove_file(&state_path);
    }
    Ok(outcome.summary)
}

/// Completed runs close through the plan finalize (deliverables gate);
/// failed or interrupted runs keep their own summary for resume.
fn outcome_with_finalize(outcome: JobOutcome, plan: &Plan, state_path: &Path) -> JobOutcome {
    if outcome.status != JobStatus::Completed {
        return outcome;
    }
    finalize_plan(plan, &load_task_states(state_path))
}

/// Scaffold template for `/job init` (Phase 5 writes it; refuses overwrite).
pub(crate) const TODO_TEMPLATE: &str = r#"{
  "goal": "TODO: describe the job goal",
  "tasks": [
    {"id": "t1", "description": "TODO: first task", "verify": ["exists artifacts/t1.done"]},
    {"id": "t2", "description": "TODO: second task", "verify": ["exists artifacts/t2.done"]}
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

/// Delete stale states: `all` removes everything, otherwise only states
/// whose plan file is gone. Returns deleted paths.
pub(crate) fn clean_states(workspace: &Path, all: bool) -> Result<Vec<PathBuf>> {
    let dir = workspace.join(TODO_STATE_DIR);
    let mut deleted = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(deleted);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let orphan = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("plan").and_then(|p| p.as_str()).map(str::to_string))
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

/// Executor instruction: description plus goal, handover, and note.
fn build_task_instruction(
    goal: &str,
    note: &str,
    prev_report: Option<&str>,
    task: &PlanTask,
) -> String {
    let mut s = format!("Task ({}): {}", task.id, task.description);
    s.push_str(&format!("\nGoal: {}", goal));
    if !task.verify_raw.is_empty() {
        s.push_str(&format!("\nChecks: {}", task.verify_raw.join(", ")));
    }
    s.push_str(&format!(
        "\nHandover (previous task report): {}",
        prev_report.unwrap_or("none")
    ));
    if !note.trim().is_empty() {
        s.push_str(&format!("\nAdditional instructions: {}", note));
    }
    s.push_str("\nDeliverable: satisfy the checks; close with a Handover Report.");
    s
}

/// Previous done task's report in plan order (handover into the next task).
fn prev_report(plan: &Plan, states: &HashMap<String, TaskState>, task_id: &str) -> Option<String> {
    let mut prev = None;
    for t in &plan.tasks {
        if t.id == task_id {
            break;
        }
        if let Some(r) = states.get(&t.id).and_then(|s| s.report.clone()) {
            prev = Some(r);
        }
    }
    prev
}

/// Run modes (one implementation; the enumerator differs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoMode {
    Static,
    Replan,
}

/// Todo run knobs (CLI flags land here in Phase 5).
#[derive(Debug, Clone)]
pub(crate) struct TodoOptions {
    pub mode: TodoMode,
    pub max_retries: u32,
    pub max_stalls: u32,
    pub note: String,
    pub executor_policy: ToolPolicy,
}

impl Default for TodoOptions {
    fn default() -> Self {
        Self {
            mode: TodoMode::Static,
            max_retries: 1,
            max_stalls: 3,
            note: String::new(),
            executor_policy: ToolPolicy::Inherit,
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

/// Load states (missing file = all pending). Unknown ids are kept.
fn load_task_states(path: &Path) -> HashMap<String, TaskState> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("tasks").cloned())
        .and_then(|t| serde_json::from_value(t).ok())
        .unwrap_or_default()
}

/// Atomic save (tmp + rename: a crash must not corrupt the reopen key).
fn save_task_states(
    path: &Path,
    job_id: &str,
    plan_path: &Path,
    states: &HashMap<String, TaskState>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let doc = serde_json::json!({
        "job_id": job_id,
        "plan": plan_path.to_string_lossy(),
        "tasks": states,
    });
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
#[path = "tests/todo_job_test.rs"]
mod tests;
