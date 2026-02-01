# Glossary

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Reference

---

## Core Concepts

### Loop
The core abstraction in Loopr. A single execution unit that iterates with fresh context until validation passes. Four types: Plan, Spec, Phase, Code. All use the same `Loop` struct with behavior determined by `LoopConfig`.

There is no separate "LoopRecord" - `Loop` IS the record. When we serialize to JSONL, we serialize `Loop`. When we deserialize, we get `Loop` back. This unified design eliminates confusion between in-memory and persisted representations.

**See [loop.md](loop.md) for the complete specification.**

### LoopConfig
Configuration that defines how each loop type behaves. Loaded from YAML at startup, shared across all loops of that type. Contains prompt template, validation command, max iterations, child type, and artifact parser. Used as a template when creating new Loops - config values are copied into the Loop at creation time.

### Ralph Wiggum Pattern
Technique from Geoffrey Huntley: fresh context each iteration prevents "context rot". The LLM doesn't accumulate confusion from failed attempts because each API call starts with a new `messages` array. Feedback is injected into the prompt text, not as conversation history.

### Fresh Context
Each iteration makes a new API call with a fresh `messages` array. No conversation history is carried forward. This prevents context rot where the LLM gets confused by accumulated failed attempts.

### Iteration
One attempt within a loop: build prompt → call LLM → execute tools → validate. If validation fails, accumulate feedback and try again with fresh context.

### Progress
Accumulated feedback from failed iterations. Injected into the prompt text (not conversation messages) so the LLM knows what went wrong without carrying full conversation history.

### Artifact
Output file produced by a loop that spawns child loops:
- Plan produces `plan.md` → spawns Specs
- Spec produces `spec.md` → spawns Phases
- Phase produces `phase.md` → spawns Code
- Code produces code/docs → nothing (leaf)

Artifacts are **first-class outputs**, versioned in git alongside code.

---

## Loop Types

### Plan Loop
Top-level loop that creates high-level plans from user tasks. Produces `plan.md` artifact. **Only loop with user gate** - user must approve before Specs spawn.

### Spec Loop
Creates detailed specifications from plans. Spawned by Plan loop. Produces `spec.md` artifact that spawns Phase loops.

### Phase Loop
Creates implementation phases from specs. Spawned by Spec loop. Produces `phase.md` artifact that spawns Code loops. Typically 3-7 phases per spec.

### Code Loop
Leaf-level loop that does actual coding work. Produces code, documentation, tests in the worktree. Does not spawn children. Uses real validation (`cargo test`, `otto ci`).

---

## Architecture

### Daemon
Long-running orchestrator process. Manages loops via LoopManager, makes LLM API calls, routes tools to runners. Continues running after TUI disconnects.

### TUI
Terminal user interface. Thin client that connects to daemon via Unix socket. Renders Chat and Loops views, sends commands, displays events. Can detach/reattach without stopping loops.

### Runner
Subprocess that executes tools in isolation. Three lanes with different capabilities. Spawned by daemon, communicates via Unix socket.

### Lane
Category of runner:
- **no-net**: No network access (10 slots, sandboxed)
- **net**: Network allowed (5 slots)
- **heavy**: Low concurrency for builds/tests (1 slot, long timeout)

### LoopManager
Component in daemon that spawns and manages loops as tokio tasks. Handles scheduling, dependency resolution, and spawning children when loops complete.

---

## Coordination

### TaskStore
Persistence layer using JSONL files with SQLite index. Stores loops, signals, tool jobs, events. JSONL is source of truth, SQLite is derived cache.

### Signal
Record in TaskStore for loop-to-loop communication. Types: stop, pause, resume, invalidate. Loops poll for signals at iteration boundaries.

### Invalidation Cascade
When a parent loop re-iterates after children have started, it sends an invalidate signal to all descendants. Children mark themselves as `Invalidated` and stop. Parent's new iteration spawns fresh children.

---

## Validation

### Validation (Plan/Spec/Phase)
Format check (required sections present) + optional LLM-as-Judge (separate API call to review artifact quality).

### Validation (Code)
Real validation using test commands: `cargo test`, `cargo clippy`, `otto ci`. Must pass for loop to complete.

### User Gate
Approval checkpoint after Plan loop completes. User reviews `plan.md` and chooses: Approve (spawn Specs), Reject (stop), or Iterate (force another attempt). Only Plan has this gate.

---

## Workspace

### Worktree
Git worktree created for each loop. Enables parallel work without file conflicts. Branch named `loop-{id}`. Nothing merges to main until entire hierarchy completes.

### Archive
Directory where invalidated loops are moved. Preserves history for debugging. Located at `~/.loopr/<project>/archive/`.

---

## IPC

### Unix Socket
Communication channel between processes. Daemon listens on `daemon.sock`. Runners listen on `runner-{lane}.sock`.

### ToolJob
Message sent from daemon to runner to execute a tool. Contains command, working directory, timeout, output limits.

---

## Status Values

### Pending
Loop waiting to start. Will be picked up by LoopManager when capacity available.

### Running
Loop actively executing iterations.

### Paused
Loop suspended by user request. Resumable via `resume` signal. Does not lose progress.

### Rebasing
Loop stopped temporarily to rebase its worktree after a sibling merged to main. Automatically resumes after rebase completes. See [worktree-coordination.md](worktree-coordination.md).

### Complete
Loop validation passed. Artifacts produced. May have spawned children.

### Failed
Loop reached max iterations or encountered unrecoverable error.

### Invalidated
Loop's parent re-iterated, making this loop's work stale. Loop is archived.

---

## References

- **[loop.md](loop.md)** - The essential document
- [domain-types.md](domain-types.md) - Data types
- [architecture.md](architecture.md) - System architecture
- [Ralph Wiggum Technique](https://ghuntley.com/ralph/) - Original concept
