# v1 → v2 Conflicts and Resolutions

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Decision Log

---

## Summary

This document tracks architectural decisions where v1 (loopr/docs) and the new design (chatgpt/docs) conflicted. In all cases, **the chatgpt approach was adopted** and the loopr design was adapted to fit.

---

## Resolved Conflicts

### 1. Process Model

| Aspect | v1 (loopr) | v2 (chatgpt) | Resolution |
|--------|------------|--------------|------------|
| Architecture | Single process | Daemon + TUI + Runners | **Adopt v2** |
| TUI lifecycle | Kill TUI = kill loops | TUI can detach | **Adopt v2** |
| Tool execution | In-process | Sandboxed subprocesses | **Adopt v2** |

**Rationale:** Daemon separation enables TUI detach/reattach, better crash recovery, and tool sandboxing. The complexity is worth it for multi-hour autonomous tasks.

**Migration:** Loop execution code moves from TUI main loop to daemon's LoopManager. TUI becomes a thin display layer.

---

### 2. Coordination Mechanism

| Aspect | v1 (loopr) | v2 (chatgpt) | Resolution |
|--------|------------|--------------|------------|
| Loop coordination | TaskStore polling only | TaskStore + IPC | **Hybrid** |
| Tool routing | N/A (in-process) | IPC to runners | **Adopt v2** |
| TUI updates | Direct state access | IPC events | **Adopt v2** |

**Rationale:** TaskStore polling is still the right choice for loop coordination (survives crashes, audit trail). But TUI↔Daemon and Daemon↔Runner need IPC for real-time interaction.

**Migration:**
- Loop-to-loop coordination: Keep TaskStore polling + signals
- TUI updates: Add IPC event stream
- Tool execution: Add runner IPC protocol

---

### 3. Tool Isolation

| Aspect | v1 (loopr) | v2 (chatgpt) | Resolution |
|--------|------------|--------------|------------|
| Network control | None | runner-no-net lane | **Adopt v2** |
| Sandboxing | Path validation only | Path + network + process groups | **Adopt v2** |
| Concurrency | Global semaphore | Per-lane semaphores | **Adopt v2** |

**Rationale:** Running code analysis tools with network access is a security risk. The runner lane model provides defense in depth.

**Migration:** Tools assigned to lanes in catalog.toml. Runner implementation handles sandboxing.

---

### 4. Tool Catalog

| Aspect | v1 (loopr) | v2 (chatgpt) | Resolution |
|--------|------------|--------------|------------|
| Tool definitions | Rust code | catalog.toml config | **Adopt v2** |
| Lane assignment | N/A | Per-tool in catalog | **Adopt v2** |
| Timeouts | Hardcoded | Config per tool | **Adopt v2** |

**Rationale:** Config-driven tool catalog is more flexible and doesn't require recompilation to adjust timeouts or add tools.

**Migration:** Move tool definitions from tools.rs to catalog.toml. Keep Rust implementations for tool logic.

---

### 5. Terminology: Loop, Not Agent

| Aspect | v1 (loopr) | v2 (chatgpt) | Resolution |
|--------|------------|--------------|------------|
| Execution unit | Loop | Agent | **Keep "Loop"** |
| Hierarchy | Plan→Spec→Phase→Ralph | Flat agents | **Keep hierarchy** |
| Leaf loop name | Ralph | Code | **Use "Code"** |

**Rationale:** The loop hierarchy is the core value of Loopr. "Agent" was just terminology - the underlying model is Ralph Wiggum loops. "Code" is more descriptive than "Ralph" for the leaf-level loop that produces code.

**Final terminology:**
- Plan → Spec → Phase → Code (not Ralph)
- One unified `Loop` struct with `loop_type` field (not separate PlanLoop/SpecLoop structs)
- Behavior via `LoopConfig` (prompt_template, validation_command, max_iterations, child_type)
- `Loop` is self-contained with `impl Loop { fn run() }` - no separate "runner" needed
- There is no `LoopRecord` - `Loop` IS the record (persistence and in-memory are the same struct)

---

### 6. Persistence Schema

| Aspect | v1 (loopr) | v2 (chatgpt) | Resolution |
|--------|------------|--------------|------------|
| Collections | loops.jsonl only | Multiple collections | **Merge** |
| Loop record | Rich schema | Simpler schema | **Keep v1 schema** |
| Event stream | In conversation.jsonl | events.jsonl | **Adopt v2** |

**Rationale:** v1's loop record schema is more complete. But v2's separation of events into its own collection is cleaner.

**Migration:**
- Keep loops.jsonl with unified `Loop` struct (no separate LoopRecord)
- Add tool_jobs.jsonl from v2
- Add events.jsonl from v2
- Keep signals.jsonl for loop coordination

---

### 7. TUI Implementation

| Aspect | v1 (loopr) | v2 (chatgpt) | Resolution |
|--------|------------|--------------|------------|
| State management | Direct TaskStore access | Daemon state + IPC | **Adopt v2** |
| Rendering | Direct ratatui calls | Same | **Keep** |
| Views | Chat + Loops | Same | **Keep** |

**Rationale:** TUI should be a thin client that displays state received from daemon. This enables multiple TUI connections and cleaner separation.

**Migration:** TUI no longer reads TaskStore directly. All state comes from daemon via IPC events.

---

## Design Decisions Carried Forward from v1

These v1 decisions remain unchanged:

| Decision | Rationale |
|----------|-----------|
| Loop hierarchy (Plan→Spec→Phase→Code) | Core value proposition |
| Artifacts as connective tissue | Natural handoff between levels |
| Rule of Five for plans | Quality gate for plan creation |
| 3-layer validation | Backpressure prevents bad code |
| Git worktrees for isolation | Parallel execution without conflicts |
| JSONL + SQLite (TaskStore) | Git-friendly, fast queries |
| Timestamp-based loop IDs | Simple, sortable, unique |

---

## Open Questions

### Q1: Should runners be persistent or spawned per-job?

**Current decision:** Persistent (spawned at daemon start, reused).

**Alternative:** Spawn per-job for better isolation.

**Concern:** Spawn overhead for frequent file operations.

**Status:** Keep persistent, revisit if isolation issues arise.

---

### Q2: How does rate limiting work across daemon restart?

**Current decision:** Rate limit state is in-memory, resets on restart.

**Alternative:** Persist rate limit state in TaskStore.

**Concern:** Could hit rate limits harder after restart.

**Status:** Acceptable for now. Add persistence if it becomes a problem.

---

### Q3: Should TUI be able to operate without daemon (offline viewing)?

**Current decision:** TUI requires daemon for all operations.

**Alternative:** TUI can read TaskStore directly for viewing (but not modifying).

**Status:** Deferred. Implement daemon-required first, add offline later if needed.

---

## Migration Path

### Phase 1: Infrastructure
1. Implement daemon skeleton (socket server, config loading)
2. Implement runner protocol and spawning
3. Implement TUI↔Daemon IPC

### Phase 2: Port Loops
1. Move LoopManager from TUI to daemon
2. Route tool calls through runners instead of direct execution
3. Stream events back to TUI

### Phase 3: Testing
1. Integration tests for IPC protocols
2. Chaos tests for crash recovery
3. Security tests for sandboxing

---

## References

- [README.md](README.md) - v2 overview
- [architecture.md](architecture.md) - v2 architecture
- [../README.md](../README.md) - v1 documentation (for reference)
