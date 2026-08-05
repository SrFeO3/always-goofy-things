# Todo Mode (Plan-Execute with Handover)

For tasks too large for a single LLM context, the agent reads a task plan from `./todo.md`, executes tasks one-by-one with fresh context each time, and carries state forward through the file. `-t` modes run immediately. (The default single-loop mode is called ReAct.)

## Mode 1: Static Plan

User writes the full plan. Agent executes in order, marking `[x]`.

### 1. Create `./todo.md`

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

### 2. Run

```bash
cargo run -- -t 1
```

---

## Mode 2

User writes a goal and initial tasks. Before each task, the LLM replans —
adding missing tasks, reordering, or writing `Conclusion` when done.

### Dynamic Replan (adds missing tasks)

The LLM notices the Goal requires `two.txt` but only `one.txt` is listed.

### 1. Create `./todo.md`

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

### 2. Run

```bash
cargo run -- -t 2
```

The LLM will:
1. Replan — notice `two.txt` is missing, add it to Tasks
2. Execute — create both files, update `./todo.md`
3. Replan — mark both `[x]`, write `Status: Completed`
4. Exit

---

### Research Task Example (open-ended exploration)

For tasks where you don't know the exact steps upfront — the LLM discovers and
adds subtasks as it goes.

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

The LLM may add more subtasks or restructure the plan as it learns.
