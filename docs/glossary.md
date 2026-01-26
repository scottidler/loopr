# Glossary

**Version:** 1.0
**Date:** 2026-01-25
**Status:** Implementation Spec

---

This glossary defines terminology used throughout Loopr documentation. Terms are listed alphabetically.

---

## A

### Artifact
Output file produced by a loop that defines work for child loops. The connective tissue between hierarchy levels.

| Loop Type | Artifact | Spawns |
|-----------|----------|--------|
| PlanLoop | `plan.md` | SpecLoop |
| SpecLoop | `spec.md` | PhaseLoop |
| PhaseLoop | `phase.md` | RalphLoop |
| RalphLoop | code files | (none - leaf node) |

**See:** [artifacts.md](artifacts.md)

---

## B

### Backpressure
Validation mechanisms that cause loop re-iteration on failure. Loopr implements three backpressure layers:

1. **Downstream gates** - Tests, linting, type-checking (objective, automated)
2. **Upstream steering** - Existing code patterns guide LLM behavior (prompt engineering)
3. **LLM-as-judge** - Binary pass/fail review for subjective criteria

**See:** [loop-validation.md](loop-validation.md)

### Branch (Git)
Each loop executes on its own git branch named `loop-{loop-id}`. Successful loops merge to main; failed/invalidated loops leave branches for debugging.

---

## C

### Child Loop
A loop spawned from a parent loop's artifact. The `parent_loop` field links to the parent, and `triggered_by` references the specific artifact.

### Complete (Status)
Loop status indicating validation passed and artifacts were produced. Terminal state.

### Context Window
The maximum tokens an LLM can process in a single conversation. Loopr uses fresh context each iteration to prevent context rot.

### Context Rot
Performance degradation that occurs when LLM conversations grow too long. Prevented by starting fresh context each iteration (the Ralph Wiggum pattern).

### Conversation
A TUI chat session with the user. Referenced by `conversation_id` in loop records. The originating conversation ID propagates from PlanLoop down to all descendants.

---

## D

### Daemon
**Not used in Loopr.** The `loopr` binary runs as a single process with the TUI directly - no background daemon.

### Depth
A loop's distance from the root (PlanLoop). Used in priority calculation to encourage depth-first execution.

| Loop Type | Typical Depth |
|-----------|---------------|
| PlanLoop | 0 |
| SpecLoop | 1 |
| PhaseLoop | 2 |
| RalphLoop | 3 |

---

## F

### Failed (Status)
Loop status indicating max iterations reached without validation passing. Terminal state.

### Fresh Context
Starting each loop iteration with a new LLM conversation, carrying no memory from previous iterations. State is conveyed through files and the progress field, not conversation history.

---

## I

### Invalidated (Status)
Loop status indicating the parent loop re-iterated, making this loop's triggering artifact stale. Terminal state. Invalidated loops are moved to the archive directory.

### Iteration
One attempt within a loop: prompt → LLM → tools → validation. If validation fails, the loop increments its iteration counter and tries again with updated feedback.

---

## J

### JSONL
JSON Lines format used by TaskStore. Each line is a complete JSON record. Enables append-only writes and git-friendly merges.

---

## L

### LLM-as-Judge
Validation layer using an LLM to evaluate subjective criteria (documentation quality, API design, etc.) with binary PASS/FAIL output.

### Loop
A single execution unit in the Loopr hierarchy. Four types exist: PlanLoop, SpecLoop, PhaseLoop, RalphLoop. Each follows the Ralph Wiggum pattern.

### Loop Hierarchy
The four-level structure: Plan → Spec → Phase → Ralph. Each level produces artifacts that spawn the next level.

```
PlanLoop
└── SpecLoop (1+)
    └── PhaseLoop (3-7)
        └── RalphLoop (1)
```

### LoopManager
Component that coordinates loop execution. Polls TaskStore for pending loops, respects concurrency limits, and spawns loops as tokio tasks.

### LoopRecord
The persistent record for a loop stored in TaskStore. Contains id, type, status, parent_loop, triggered_by, iteration count, and timestamps.

**See:** [domain-types.md](domain-types.md)

---

## M

### Max Iterations
The maximum number of attempts a loop can make before being marked as failed. Configurable per loop type (default: 100).

---

## O

### Onion Problem
When an outer loop re-iterates, all inner loops spawned from its previous artifacts become stale. Loopr solves this by invalidating descendants and archiving their work.

**See:** [loop-architecture.md](loop-architecture.md)

---

## P

### Paused (Status)
Loop status indicating execution is suspended (user action or rate limit). Can transition back to running.

### Pending (Status)
Loop status indicating the loop is waiting to start. Transitions to running when the scheduler picks it up.

### Phase
A discrete implementation step within a spec. Specs contain 3-7 phases, each spawning one RalphLoop.

### PhaseLoop
Loop type that implements a single phase from a spec. Produces `phase.md` artifact and code files. Spawns one RalphLoop.

### PlanLoop
Top-level loop type that creates high-level plans using the Rule of Five methodology. Produces `plan.md` artifacts that spawn SpecLoops.

### Polling
The coordination mechanism where loops and the LoopManager periodically query TaskStore for state changes, rather than using IPC or message passing.

### Priority
Numeric score determining which pending loop runs next. Calculated from loop type, age, depth, and retry count.

**See:** [scheduler.md](scheduler.md)

### Progress
Accumulated feedback from previous iterations, stored in the loop record and injected into subsequent prompts. Enables learning from failures despite fresh context.

**See:** [progress-strategy.md](progress-strategy.md)

---

## R

### Ralph / RalphLoop
The leaf-level loop type that does actual coding work. Named after the Ralph Wiggum technique. Produces code, tests, and documentation.

### Ralph Wiggum Technique
Geoffrey Huntley's pattern for autonomous LLM coding:
```bash
while :; do cat PROMPT.md | claude ; done
```
Key insight: fresh context each iteration prevents context rot. State lives in files, not conversation history.

**Origin:** [ghuntley.com/ralph](https://ghuntley.com/ralph/)

### Running (Status)
Loop status indicating active execution. The loop is iterating until validation passes or max iterations reached.

### Rule of Five
Structured 5-pass review methodology for creating high-quality Plan documents:

1. **Completeness** - Is anything missing?
2. **Correctness** - Is anything wrong?
3. **Edge Cases** - What could go wrong?
4. **Architecture** - Does this fit the larger system?
5. **Clarity** - Can someone implement this unambiguously?

**See:** [rule-of-five.md](rule-of-five.md)

---

## S

### Scheduler
Component that determines which pending loops to run based on priority, dependencies, and concurrency limits.

**See:** [scheduler.md](scheduler.md)

### Signal
A record in TaskStore used for inter-loop communication (stop, pause, resume, error). Enables coordination without IPC.

**See:** [loop-coordination.md](loop-coordination.md)

### SpecLoop
Loop type that creates detailed specifications from plans. Produces `spec.md` artifacts with 3-7 phases that spawn PhaseLoops.

### Status
Loop lifecycle state. Valid values:
- `pending` - Waiting to start
- `running` - Actively iterating
- `paused` - Suspended
- `complete` - Validation passed (terminal)
- `failed` - Max iterations reached (terminal)
- `invalidated` - Parent re-iterated (terminal)

---

## T

### TaskStore
Persistence layer combining JSONL (append-only log) with SQLite (query cache). Provides durable storage with fast queries.

**Repository:** [github.com/scottidler/taskstore](https://github.com/scottidler/taskstore)

### Terminal State
A loop status from which no further transitions occur: `complete`, `failed`, or `invalidated`.

### Tool
A capability exposed to the LLM during loop execution. Standard tools: read_file, write_file, edit_file, list_directory, glob, grep, run_command, complete_task.

**See:** [tools.md](tools.md)

### ToolContext
Execution context for tools, scoped to a single loop's git worktree. Enforces sandbox (tools cannot escape worktree).

### TUI
Terminal User Interface. The `loopr` binary provides a k9s-style interface with Chat and Loops views.

**See:** [tui.md](tui.md)

---

## V

### Validation
The process of determining if a loop iteration succeeded. May use command execution (tests), LLM-as-judge, or both.

**See:** [loop-validation.md](loop-validation.md)

---

## W

### Worktree
Git worktree providing isolated workspace for each loop. Created when loop starts, cleaned up when loop reaches terminal state.

**See:** [execution-model.md](execution-model.md)

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy overview
- [domain-types.md](domain-types.md) - Data structures
- [execution-model.md](execution-model.md) - Worktree management
- [scheduler.md](scheduler.md) - Priority and scheduling
- [loop-validation.md](loop-validation.md) - Validation strategies
