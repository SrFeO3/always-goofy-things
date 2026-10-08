# Todo Job (Plan-Execute with Fresh Sessions)

> This replaces the previous `todo.md`-based mode (`-t 1` / `-t 2`): plans are now structured JSON and completion is mechanically verified.

When a job is too large for a single LLM context, split it into a plan of tasks in `todo.json`: the application executes the tasks one-by-one, each in a fresh LLM context, and verifies completion mechanically. There are two modes: **Static Plan**, where the plan is fixed, and **Dynamic Replan**, where the planner revises the plan before each task. Either way the executor never sees more than its own task plus the previous report.

## The Two Modes at a Glance

| | Static Plan | Dynamic Replan |
|---|---|---|
| Command | `/job run todo.json --mode static` | `/job run todo.json --mode replan` |
| When to use | Steps are fully known in advance | Steps are unknown or may change (exploration, research) |
| The plan | Fixed - tasks run in order as written | Living - the planner revises it before each task (add / remove / reorder / split) |

## The Plan: `todo.json`

```json
{
  "goal": "Write a final report from the research notes.",
  "tasks": [
    { "id": "t1", "description": "Research and save notes to artifacts/notes.md",
      "verify": ["exists artifacts/notes.md", "nonempty artifacts/notes.md"] },
    { "id": "t2", "description": "Merge the notes into artifacts/final-report.md",
      "verify": ["exists artifacts/final-report.md"] }
  ],
  "deliverables": ["artifacts/final-report.md"]
}
```

- `goal`: the job's objective. `tasks[]`: `id` (unique, the resume key), `description`,
  `verify` (completion checks). `deliverables[]`: the goal files the job must produce.
- Checks read as `exists <path>` / `nonempty <path>` / `contains <path>:<needle>` /
  `sql <query>`. Completion is mechanical: every check true, plus every deliverable
  present and non-empty. A task that fails its checks is retried fresh (`--max-retries`);
  declared `Output:` paths that never materialize are reported as warnings, not errors.
- Save every file you produce under `artifacts/`; checks and deliverables are paths there.

## Quick Start

1. **Write the plan**: ask the AI ("write a `todo.json` plan for ...") or scaffold one
   with `/job init todo.json` and fill it in.
2. **Preview (optional)**: `/job run todo.json --dry-run` (tasks, checks, deliverables).
3. **Run**: `/job run todo.json --mode static`.

```text
--- [Job] t1: Research and save notes to artifacts/notes.md ---
[ok] t1
Completed: deliverables(1) artifacts/final-report.md
```

4. **Interrupt and resume**: state survives, so `/job run todo.json --status` shows
   progress and rerunning the same command resumes where it stopped.

## State Is Temporary (Cleaned Up Automatically)

Unlike the knowledge base, job state exists only to resume. It lives in
`.todo/<job-id>.state.json` inside the workspace:

| Situation | Behavior |
|---|---|
| Success | state deleted automatically (artifacts stay in `artifacts/`) |
| Failure / interrupt | state kept; rerun to resume |
| Abandoned | `/job clean [--all]` removes it |

## How It Works

Each task runs in a fresh LLM context carrying only its description, the goal, and the
previous task's report - never the whole log. The planner (replan mode) works the same
way: it reads the goal, the plan, and recent reports, then proposes the revised task
list, which the application validates (finished tasks are immutable) before applying.
The LLM never writes state itself; the application owns the JSON.

## Commands

```text
/job init <path>     Scaffold a todo.json plan
/job run <todo.json> [--mode static|replan] [--dry-run] [--status] [--max-retries N] [--note "..."]
/job clean [--all]   Remove abandoned job states
```

Batch runs (`-q "/job run ..."`) work unattended; writes then need `--unsafe-reflex`,
and extra instructions go in `--note`.
