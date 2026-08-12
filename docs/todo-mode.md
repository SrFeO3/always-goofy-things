# Todo Mode (Plan-Execute with Handover)

When a job is too large for a single LLM context, the agent breaks it into a plan of tasks: it reads the plan from `./todo.md`, executes the tasks one-by-one with a fresh context each time, and carries state forward through the file. `-t` modes run immediately. There are two modes: **Static Plan** (`-t 1`), where the plan is fixed, and **Dynamic Replan** (`-t 2`), where the AI rewrites the plan as it works. (The default single-loop mode is called ReAct.)

## The Two Modes at a Glance

| | Mode 1: Static Plan | Mode 2: Dynamic Replan |
|---|---|---|
| Startup flag (`-t`) | `1` | `2` |
| Plan author | You - the complete plan is written upfront | The LLM - starting from your goal and initial tasks |
| Who updates `todo.md` | The agent program - after each task, it marks the task `[x]` | The LLM - it marks `[x]`, adds / removes / reorders / splits tasks, and writes the Conclusion |
| Use when | Steps are fully known in advance | Steps are unknown or may change (exploration, research) |

## Mode 1: Static Plan (Plan-Exec-Static)

The plan is fixed before execution starts. You write the complete `./todo.md`; the agent executes the tasks strictly in order, one per fresh LLM context, and the agent program marks each task `[x]` (appending a short handover note) when it finishes. The AI never changes the plan itself. If execution is interrupted, rerunning `-t 1` resumes from the first unchecked task.

### Example: Fixed Sequence (all steps known in advance)

A typical Mode 1 case: a simple, fully-specified workflow with no unknowns.

#### 1. Create `./todo.md`

```markdown
# Counter Test

## Goal
Count from 1 to 3, recording each number into count.txt.

## Tasks
- [ ] Write "1" to count.txt
- [ ] Append "2" to count.txt
- [ ] Append "3" to count.txt

## Handover Notes
- Nothing executed yet.
```

#### 2. Run

```bash
cargo run -- -t 1
```

What happens:

1. The agent reads `./todo.md` and picks the first unchecked task.
2. It executes that task with a fresh LLM context, then marks it `[x]` and appends a summary to `## Handover Notes`.
3. It repeats for the next task, until all three are `[x]`.
4. It reports that all tasks are complete and exits.

---

## Mode 2: Dynamic Replan (Plan-Exec-Dynamic)

The plan is a living document. You write a goal and an initial task list (it may be incomplete or wrong); before each task the LLM reviews `./todo.md` and rewrites it - adding, removing, reordering, or splitting tasks - and after each task it updates the file itself: marking tasks `[x]` and writing `## Conclusion` with `Status: Completed` once the goal is reached.

- Replanning runs before each task as a lightweight 1-turn review.
- The loop ends when the Conclusion says `Status: Completed` **and** no `- [ ]` tasks remain.
- Safety: if replanning fails to reduce the unchecked-task count for `--max-replan-attempts` consecutive rounds (default 3, `0` = unlimited), the agent stops.

### Example 1: Missing Task Discovery (plan is incomplete; the AI fills the gap)

Shows the replan step in action: the Goal requires two files, but the Tasks list only mentions one. The LLM notices the gap and adds the missing task.

#### 1. Create `./todo.md`

```markdown
# Two-File Creation

## Goal
Create one.txt with "hello" and two.txt with "world".

## Tasks
- [ ] Create one.txt with content "hello"

## Handover Notes
- Nothing executed yet. Note: two.txt task is missing from the plan.

## Conclusion
- Status: In Progress
- Final Conclusion: 
- Key Findings / Results:
```

#### 2. Run

```bash
cargo run -- -t 2
```

The LLM will:

1. Replan - notice `two.txt` is missing, add it to Tasks
2. Execute - create both files, update `./todo.md`
3. Replan - mark both `[x]`, write `Status: Completed`
4. Exit

### Example 2: Open-Ended Research (exploratory; the AI discovers subtasks as it goes)

Shows an exploratory task: the exact steps are not known upfront. The initial list is only a starting point - the LLM adds subtasks or restructures the plan as it learns.

#### 1. Create `./todo.md`

```markdown
# Rust Crate Analysis

## Goal
Compare `anyhow` and `eyre` error-handling crates.

## Tasks
- [ ] Read anyhow docs and summarize key features
- [ ] Read eyre docs and summarize key features
- [ ] Read both summaries and write a comparison report

## Handover Notes
- Start by researching anyhow first.

## Conclusion
- Status: In Progress
- Final Conclusion: 
- Key Findings / Results:
```

#### 2. Run

```bash
cargo run -- -t 2
```

The LLM may add more subtasks or restructure the plan as it learns.
