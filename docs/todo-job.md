# Todo Job (Plan-Execute with Fresh Sessions)

> Replaces the old `todo.md` mode. Plans are structured JSON, each task runs in a
> fresh LLM context, completion is mechanically verified, and the live plan is a
> single file you can inspect at any time.

When a job is too large for one LLM context, split it into a plan of tasks in
`todo.json`. The application runs the tasks one by one, each in a fresh LLM
context, and verifies completion mechanically.

## The Two Modes

| | Static Plan | Dynamic Replan |
|---|---|---|
| Command | `/job run todo.json --mode static` | `/job run todo.json --mode replan` |
| When to use | Steps are fully known in advance | Steps are unknown or may change (exploration, research) |
| The plan | Fixed, run in order as written | Living, the planner revises it before each task |
| Loop | next pending -> execute -> verify | planner revises -> next pending -> execute -> verify |

The loop is the **same in both modes**; replan mode only inserts a planner step at
the top of each iteration.

## Files

| File | What it is | Written by |
|---|---|---|
| `todo.json` | your input plan (seed, never modified) | you |
| `todo.live.json` | the live plan: current tasks + status + handover + brief | the application |
| `artifacts/...` | task outputs and deliverables | the LLM |

`todo.json` is read once to create the live plan next to it, with the same stem:
`todo.json` -> `todo.live.json`, `jobs/build.json` -> `jobs/build.live.json`. From
then on the live plan is the **single source of truth**: the application updates it
atomically as tasks complete and as the planner revises. Resume reads it, so an
interrupt never desyncs the plan from the progress.

Open `todo.live.json` to see the current plan, each task's `status`, and progress
at a glance. Add `*.live.json` to `.gitignore` if you do not want it tracked.

## The Plan: `todo.json`

```json
{
  "goal": "Write a final report from the research notes.",
  "tasks": [
    { "id": "t1", "description": "Research and save notes to artifacts/notes.md",
      "verify": ["nonempty artifacts/notes.md"] },
    { "id": "t2", "description": "Merge the notes into artifacts/final-report.md",
      "verify": ["nonempty artifacts/final-report.md"] }
  ],
  "deliverables": ["artifacts/final-report.md"]
}
```

- `goal`: the job's objective.
- `tasks[]`: `id` (unique and stable — the resume key), `description`, `verify`
  (the checks that must all pass for the task to count as done; at least one).
- `deliverables[]`: the files the whole job must produce; each must exist and be
  non-empty.

Each `verify` entry is a mechanical check, one per line:
- `nonempty <path>` — the file at `<path>` exists and is not empty.
- `contains <path>:<needle>` — the file's text contains `needle` (an exact
  substring; the path ends at the first `:`).
- `sql <query>` — KB-only; a todo job has no database, so this always fails.

`<path>` values are workspace-relative; task outputs go under `artifacts/`. Checks
and deliverables must stay inside the workspace (no absolute paths, no `..` escape);
an outside path fails verification instead of being read.

## The Live Plan: `todo.live.json`

Generated from `todo.json`; the application owns it. Tasks gain runtime fields:

```json
{
  "goal": "Write a final report from the research notes.",
  "revision": 2,
  "tasks": [
    { "id": "t1", "description": "...", "verify": ["nonempty artifacts/notes.md"],
      "brief": "Read artifacts/notes.md; pull the key claims into the report.",
      "status": "done", "attempts": 1,
      "handover": "Status: done\nOutput: artifacts/notes.md\n..." },
    { "id": "t2", "description": "...", "verify": ["nonempty artifacts/final-report.md"],
      "status": "pending", "attempts": 0 }
  ],
  "deliverables": ["artifacts/final-report.md"]
}
```

- `status`: `pending` / `done` / `cancelled`. A failed check leaves the task `pending`
  (it is retried next loop, with the reason recorded). A pending task the planner
  removes is marked `cancelled` with a reason (subject to `--replan-scope`).
- `attempts`: how many executor sessions have run for the task.
- `handover`: the task's final report, kept so the planner can read it.
- `brief`: the planner's distilled brief for the task (replan mode), read by the
  executor.

## Quick Start

1. **Write the plan**: ask the AI ("write a `todo.json` plan for ...") or scaffold
   with `/job init todo.json` and fill it in.
2. **Preview (optional)**: `/job run todo.json --dry-run` (tasks, checks, deliverables).
3. **Run**: `/job run todo.json --mode static` (or `--mode replan`).
4. **Inspect / resume**: open `todo.live.json` to see the live plan and progress;
   `/job run todo.json --status` summarizes it; rerun the same command to resume.

## How It Runs

The loop is the same in both modes:

1. *(replan only)* The planner reads the goal, deliverables, the whole live plan,
   all task handovers, and recent verification failures, then proposes a revised
   plan plus a brief for the next task. The application validates and applies it,
   then writes the live plan atomically.
2. Pick the first task whose `status` is neither `done` nor `cancelled`.
3. Run it in a fresh LLM context with **only** its `description`, `verify`,
   `brief`, the `goal`, any `--note`, and (on a retry) the previous failure reason —
   never the whole log. It writes outputs under `artifacts/` and closes with a
   Handover Report.
4. Verify mechanically. If every check passes, the task becomes `done`; otherwise it
   stays `pending` and is retried on the next loop, and the retry is told why it failed.
   Declared `Output:` paths in the handover that are missing (or point outside the
   workspace) are reported as warnings when the job completes.
5. When no pending task remains, finish (deliverables gate). In replan mode the
   planner runs once more before this exit, so it can still add tasks if the goal is
   not yet met.

Retries: a task that fails verification stays pending and is retried on the next
iteration (in replan mode the planner sees the failure first and may split or fix
it). The loop stops after `--max-replan-attempts` rounds without completing another
task (default 3; `0` = unlimited). In replan mode the planner proposes and the
application applies — the LLM never writes the plan or the status.

Scope reduction: if a revised plan drops an unfinished task, it is not silently
lost. `--replan-scope` decides: `ask` (default) prompts y/N, `deny` rejects the
revision, and `auto` applies it. An approved removal becomes `cancelled` with a
reason; the task stays in the live plan and is reported at the end.

Tools: executors get the workspace tools (`read_file`, `write_file`,
`str_replace_editor`, `grep_search`, `execute_bash`, `calc`, ...). If
`--only-tools` leaves no write path (`write_file` / `str_replace_editor` /
`execute_bash` / `mini_python_interpreter`), the run stops before the first LLM
call with `[TOOL_CONFIG_ERROR]`.

## Progress, Resume, Reset

- `/job run todo.json --status` renders progress from the live plan (`[~]` = cancelled).
  The header counts active tasks only, with `(+N cancelled)` when any were cancelled.
- Interrupt, then rerun the same command: it resumes from the live plan.
- After completion the live plan stays (every task `done` or `cancelled`) so you can
  review what the plan became; rerunning completes immediately.
- To start over (for example after editing `todo.json`), run with `--reset` (or
  delete `<stem>.live.json`); the live plan is regenerated from `todo.json`.
- If `todo.json` changed but `--reset` was not given, the run keeps the existing
  live plan and prints a warning.
- `/job clean [--all]` clears stale files under `.todo/` (e.g. abandoned KB analyze runs).

## Static vs Replan in One Line

- **Static**: no planner; each pending task runs and is verified in order.
- **Replan**: a fresh planner revises the plan and writes the next task's brief
  before each task; the executor only ever sees that brief plus its own task.

## Commands

```text
/job init <path>     Scaffold a todo.json plan
/job run <todo.json> [--mode static|replan] [--dry-run] [--status] [--reset] [--replan-scope auto|ask|deny] [--note "..."]
/job clean [--all]   Remove abandoned live plans
```
