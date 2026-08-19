# Todo Mode (Plan-Execute with Handover)

When a job is too large for a single LLM context, split it into a plan of tasks in `./todo.md`: the application executes the tasks one-by-one with a fresh context each time and carries state forward through the file. `-t` modes run immediately. There are two modes: **Static Plan** (`-t 1`), where the plan is fixed, and **Dynamic Replan** (`-t 2`), where the LLM rewrites the plan as it works. (The default single-loop mode is called ReAct.)

Both modes share the same file conventions:
- `./todo.md` is strictly structured: a title, a `## Goal`, and a `## Tasks` list (`- [ ]` / `- [x]`). Nothing else belongs in it; the format is enforced by the prompts.
- `artifacts/handover.md` is the free-form handover log. Every session reads it together with `./todo.md` first. The application creates it with a short template when it does not exist, appends each task's final report to it (followed by an `outputs:` line listing the task's declared artifact paths, never truncated), and (Mode 2) the replan planner writes its notes there.
- Every task session ends with a structured Handover Report - `Status / Output / Findings / Next` - which the application appends to `artifacts/handover.md`. The application verifies that the paths declared in `Output:` actually exist; missing ones are warned about (Mode 1) or reported to the next replan so the planner can add a fix task (Mode 2).
- Every task saves its outputs to `artifacts/`; the last task also saves the job's final result there. Write that file's name in the Goal: at completion the application reads the Goal-named `artifacts/...` file and returns its content as the job's final answer (falling back to the last task's report, then a completion notice). When the Goal names several `artifacts/...` files, the last one named (the final deliverable) is returned.
- In todo mode, the `-q` query is appended to every replan and task session's user message as additional instructions.

## The Two Modes at a Glance

| | Mode 1: Static Plan | Mode 2: Dynamic Replan |
|---|---|---|
| Startup flag (`-t`) | `1` | `2` |
| Plan author | You - the complete plan is written upfront | The LLM - starting from your goal and initial tasks |
| Who updates `todo.md` | The application - after each task, it marks the task `[x]` | The LLM - it marks `[x]` and adds / removes / reorders / splits tasks |
| Who writes `artifacts/handover.md` | The application (task reports) | The application (task reports) and the replan planner (notes) |
| Use when | Steps are fully known in advance | Steps are unknown or may change (exploration, research) |

## Mode 1: Static Plan (Plan-Exec-Static)

The plan is fixed before execution starts. You write the complete `./todo.md`; the application executes the tasks strictly in order, one per fresh LLM context, and marks each task `[x]` when it finishes, appending the task's final report to `artifacts/handover.md`. The AI never changes the plan itself. If execution is interrupted, rerunning `-t 1` resumes from the first unchecked task.

### Example: Fixed Sequence (all steps known in advance)

A typical Mode 1 case: a simple, fully-specified workflow with no unknowns.

#### 1. Create `./todo.md`

```markdown
# Counter Test

## Goal
Count from 1 to 3, recording each number into artifacts/count.txt.

## Tasks
- [ ] Write "1" to artifacts/count.txt
- [ ] Append "2" to artifacts/count.txt
- [ ] Append "3" to artifacts/count.txt
```

#### 2. Run

```bash
cargo run -- -t 1
```

What happens:

1. The application reads `./todo.md` (and `artifacts/handover.md`, creating it if missing) and picks the first unchecked task.
2. It executes that task with a fresh LLM context, then marks it `[x]` and appends the task's Handover Report to `artifacts/handover.md`.
3. It repeats for the next task, until all three are `[x]`.
4. It returns the content of the Goal artifact `artifacts/count.txt` (`1`, `2`, `3`) as the job's final answer and exits.

---

## Mode 2: Dynamic Replan (Plan-Exec-Dynamic)

The plan is a living document. You write a goal and an initial task list (it may be incomplete or wrong); before each task the LLM reviews `./todo.md` and rewrites it - adding, removing, reordering, or splitting tasks - and after each task it updates the file itself: marking its task `[x]` (it may add subtasks). All notes, status, and reasoning go to `artifacts/handover.md`, never into `./todo.md`.

- Replanning runs before each task as a fresh planner session: a full reasoning loop that reads `./todo.md` and `artifacts/handover.md`, may inspect `artifacts/` with tools, writes the updated plan to `./todo.md`, and writes its notes to `artifacts/handover.md`.
- Completion: when all tasks are `[x]`, the application runs one final replan so the planner can add tasks if the Goal is not yet achieved; the loop exits only when the plan is still all-`[x]` after that final replan.
- Safety: if replanning fails to reduce the unchecked-task count for `--max-replan-attempts` consecutive rounds (default 3, `0` = unlimited), the application stops.

### Example 1: Missing Task Discovery (plan is incomplete; the AI fills the gap)

Shows the replan step in action: the Goal requires two files, but the Tasks list only mentions one. The LLM notices the gap and adds the missing task.

#### 1. Create `./todo.md`

```markdown
# Two-File Creation

## Goal
Create artifacts/one.txt with "hello" and artifacts/two.txt with "world".

## Tasks
- [ ] Create artifacts/one.txt with content "hello"
```

#### 2. Run

```bash
cargo run -- -t 2
```

The LLM will:

1. Replan - notice `artifacts/two.txt` is missing, add it to Tasks
2. Execute - create both files and mark its task `[x]` in `./todo.md`
3. Replan - all tasks `[x]`; the final replan confirms the Goal is reached
4. Exit

### Example 2: Offline Open-Ended Research (exploratory; the AI discovers subtasks as it goes, no external access)

Shows an exploratory job: the exact steps are not known upfront. The initial list is only a starting point - the LLM adds subtasks or restructures the plan as it learns. The corpus is downloaded once beforehand and read from disk, so the run itself needs no network access. Every claim in the final report must cite the local file it came from: this prevents the LLM from writing the report from memory and lets you verify the result against the corpus.

#### 1. Download the docs (one-time, with network access)

```bash
mkdir -p research/anyhow research/eyre
curl -fsSL -o research/anyhow/README.md https://raw.githubusercontent.com/dtolnay/anyhow/master/README.md
curl -fsSL -o research/anyhow/lib.rs https://raw.githubusercontent.com/dtolnay/anyhow/master/src/lib.rs
curl -fsSL -o research/eyre/README.md https://raw.githubusercontent.com/yaahc/eyre/master/README.md
curl -fsSL -o research/eyre/lib.rs https://raw.githubusercontent.com/yaahc/eyre/master/eyre/src/lib.rs
```

Any local copy works - docs.rs HTML, `cargo doc` output, or the crate sources - the test only requires the corpus to be on disk. The READMEs are small enough to read in full; the `lib.rs` files are large, so the LLM is expected to grep them for the relevant sections.

#### 2. Create `./todo.md`

```markdown
# Offline Crate Research

## Goal
Compare `anyhow` and `eyre` error-handling crates using the local docs under research/ and save the comparison report to artifacts/comparison.md. Every claim must cite the local file it comes from.

## Tasks
- [ ] Read the anyhow docs under research/anyhow/ and summarize key features
- [ ] Read the eyre docs under research/eyre/ and summarize key features
- [ ] Read both summaries and write a comparison report
```

#### 3. Run

```bash
cargo run -- -t 2 -q "Use the local docs under research/ only; do not fetch anything from the web."
```

The `-q` query is appended to every replan and task session - it keeps the LLM on the local corpus even on systems where web access is allowed.

The LLM will:

1. Replan - keep the plan; decide whether the initial tasks are enough or add subtasks (e.g., comparing feature flags or the `Context` / `Report` APIs)
2. Execute - list research/anyhow/, grep and read the docs, and summarize key features; the summary lands in `artifacts/handover.md` and the task is marked `[x]`
3. Execute - do the same for eyre; mark the task `[x]`
4. Replan - restructure the plan based on what was learned (add, split, or reorder tasks)
5. Execute - read both summaries from the handover and write `artifacts/comparison.md`, citing `research/` files for every claim; mark the task `[x]`
6. Replan - all tasks `[x]`; the final replan confirms the Goal is reached
7. Exit

Steps 2-5 each run in a fresh context, and step 5 has to work from the handover summaries - this is the replan + reset behavior under test, exercised on a real corpus instead of web pages. Verify the result by reading `artifacts/comparison.md` and spot-checking a few citations against the files under `research/`. The LLM may add more subtasks or restructure the plan as it learns.
