# Design Document: Unified Log with [component:id] Prefix Notation

**Author:** Scott Idler
**Date:** 2026-04-06
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Loopr currently splits log output across multiple per-agent files and a main `loopr.log`,
creating a split-brain diagnostic problem. This document describes deleting `AgentLogger`
entirely and routing all output through standard `log` macros with structured `[component:id]`
prefixes. One log file. Every line prefixed. Grep for isolation.

## Problem Statement

### Background

`AgentLogger` was introduced to give each agent an isolated log file. It double-writes: every
call writes to a per-agent `.log` file AND calls the standard `log::info!` macro. The result is
that everything from agents appears in two places simultaneously.

Library functions (decomposer, evaluator, validator) were never wired to `AgentLogger` - they
only use plain macros. Their output goes to `loopr.log` only, with no component prefix.

### Problem

Two symptoms from the python-api E2E run that triggered this:

1. An external diagnostic tool checked `decomposer-cg-7kkha.log` and saw two lines - start and
   failure. It concluded the decomposer never ran and suspected an async deadlock. In reality,
   600+ lines of decomposer progress lived in `loopr.log` under `loopr::decomposer`. The per-agent
   log file actively misled the diagnosis.

2. When `loopr.log` was checked, the decomposer progress had no component or goal ID on each
   line - only the Rust module path (`loopr::decomposer`). Correlating lines to a specific run
   required inferring from timestamps.

### Goals

- One log file: `loopr.log`
- Every line carries `[component:id]` so grep gives per-component isolation
- `AgentLogger` deleted - no struct, no file I/O, no module
- All log output uses standard `info!` / `warn!` / `debug!` / `error!` macros
- Library functions (decomposer etc.) receive the relevant ID as a plain `&str` parameter and
  embed it in their macro calls

### Non-Goals

- Log rotation, archiving, or log level changes
- Structured/JSON output
- Changes to `env_logger` initialization
- Any behavior changes - this is observability only
- The coordinator FSM deadlock fix (separate doc)

---

## Proposed Solution

### Overview

Delete `src/agents/agent_logger.rs`. Remove `AgentLogger` from every call site. Replace every
`log.info(...)` / `self.ctx.info(...)` / `agent_log.warn(...)` call with the corresponding
standard macro that includes a `[component:id]` prefix in the format string.

Before:
```rust
self.ctx.info(&format!("iteration {} (FSM: {:?})", n, state));
```

After:
```rust
info!("[{}:{}] iteration {} (FSM: {:?})", self.ctx.session.agent_type, self.ctx.session.id, n, state);
```

`AgentKind` implements `Display` with lowercase names (`coordinator`, `integrator`, etc.), so
`self.ctx.session.agent_type` in a format string produces the right component name automatically.
No hardcoding the component name - it comes from the session.

For library functions that currently use anonymous macros:
```rust
// before
info!("decompose_into: done parent={} -> {} {}(s)", parent.id, n, kind);

// after
info!("[decomposer:{}] decompose_into: done parent={} -> {} {}(s)", goal_id, parent.id, n, kind);
```

The `goal_id` (or equivalent domain ID) is threaded as a plain `&str` parameter into library
functions that need it.

### Complete Prefix Table

These are every prefix that will appear in `loopr.log` after this change. No others.

| Prefix | ID source | ID prefix | Example |
|--------|-----------|-----------|---------|
| `[coordinator:ag-xxxxx]` | `AgentSession.id` | `ag-` | `[coordinator:ag-vh9so]` |
| `[integrator:ag-xxxxx]` | `AgentSession.id` | `ag-` | `[integrator:ag-wqemh]` |
| `[implementer:ag-xxxxx]` | `AgentSession.id` | `ag-` | `[implementer:ag-3kpqr]` |
| `[reviewer:ag-xxxxx]` | `AgentSession.id` | `ag-` | `[reviewer:ag-9mnop]` |
| `[researcher:ag-xxxxx]` | `AgentSession.id` | `ag-` | `[researcher:ag-2xvwz]` |
| `[chat:ag-xxxxx]` | `AgentSession.id` | `ag-` | `[chat:ag-5jklm]` |
| `[worker:N]` | Worker index integer | - | `[worker:0]`, `[worker:1]` |
| `[decomposer:cg-xxxxx]` | `CoordinatorGoal.id` | `cg-` | `[decomposer:cg-7kkha]` |
| `[evaluator:ph-xxxxx]` | `Phase.id` | `ph-` | `[evaluator:ph-x28oa]` |
| `[validator:xx-xxxxx]` | Doc id (any kind) | varies | `[validator:pl-oouxr]` |
| `[ratifier:pl-xxxxx]` | `Plan.id` | `pl-` | `[ratifier:pl-3mnyq]` |
| `[daemon]` | singleton | - | `[daemon]` |
| `[ipc]` | singleton | - | `[ipc]` |
| `[worktree:wt-xxxxx]` | Worktree branch/id | `wt-` | `[worktree:wt-ag-3kpqr]` |

### Grep Patterns for Common Diagnostic Queries

```bash
# All output from one coordinator session
grep '\[coordinator:ag-vh9so\]' loopr.log

# Everything the decomposer did for one goal
grep '\[decomposer:cg-7kkha\]' loopr.log

# Coordinator + decomposer interleaved for the same run
grep -E '\[(coordinator|decomposer):' loopr.log | grep 'vh9so\|7kkha'

# All warnings and errors across every component
grep -E ' WARN | ERROR ' loopr.log

# Any component hitting a failure
grep 'failed\|error\|panic' loopr.log

# All worker activity
grep '\[worker:' loopr.log
```

### Decomposer Progress Events

After this change, a full decomposition run will produce lines like:

```
[decomposer:cg-7kkha] starting (brief=false)
[decomposer:cg-7kkha] plan pl-oouxr -> 3 specs
[decomposer:cg-7kkha] spec sp-n79ap "Database Layer": starting 5 phase branches
[decomposer:cg-7kkha] spec sp-0wpvm "API Routes": starting 5 phase branches
[decomposer:cg-7kkha] spec sp-tqzpk "Test Suite": starting 3 phase branches
[decomposer:cg-7kkha] phase ph-6jqvo "Schema Bootstrap" -> 1 work
[decomposer:cg-7kkha] phase ph-x28oa "Create & List" -> 2 works
[decomposer:cg-7kkha] phase ph-ck69m "Test Infrastructure" -> 4 works
... one line per phase ...
[decomposer:cg-7kkha] spec sp-0wpvm "API Routes": complete (12 docs)
[decomposer:cg-7kkha] spec sp-tqzpk "Test Suite": complete (11 docs)
[decomposer:cg-7kkha] spec sp-n79ap "Database Layer": FAILED - Failed to parse LLM output as JSON array
[decomposer:cg-7kkha] decomposition failed
```

This is what was missing from the python-api run. Every line is greppable by goal ID.

### Implementation Plan

**Phase 1 - Delete `AgentLogger`**

- Delete `src/agents/agent_logger.rs`
- Remove `pub mod agent_logger` and `use` imports from `src/agents.rs`
- Remove `pub log: AgentLogger` field from `AgentContext`
- Remove the `info`, `warn`, `debug`, `error`, `trace` wrapper methods from `AgentContext`
  (all 5 delegate to `self.log.*` - they all go)
- Remove `AgentLogger::new(...)` construction in `AgentContext::new` and
  `src/agents/executor/lifecycle.rs`
- Remove `AgentLogger::for_component(...)` call in `src/daemon/handlers/doc.rs`
- Remove the unused import added to `src/decomposer.rs` during an interrupted edit

**Phase 2 - Replace call sites in agents**

For each agent (`coordinator`, `integrator`, `implementer`, `reviewer`, `researcher`, `chat`):
- Replace `self.ctx.info(msg)` with `info!("[{kind}:{id}] {}", msg)` where `kind` is the
  agent's string name and `id` is `self.ctx.session.id`
- Replace `self.ctx.warn(msg)`, `self.ctx.debug(msg)`, `self.ctx.error(msg)` the same way
- Replace `agent_log.info(msg)` / `log.info(msg)` patterns in coordinator helpers and
  executor lifecycle

Files affected:
- `src/agents/coordinator/run.rs`
- `src/agents/coordinator.rs`
- `src/agents/integrator.rs`
- `src/agents/implementer.rs`
- `src/agents/reviewer.rs`
- `src/agents/researcher.rs`
- `src/agents/executor/lifecycle.rs`
- `src/agents/executor/action/file.rs`
- `src/agents/executor/util.rs`
- `src/agents.rs` (AgentContext)

**Phase 3 - Add prefixes to library functions**

Decomposer (`src/decomposer.rs`):
- Add `goal_id: &str` parameter to `decompose_hierarchy`, `decompose_spec_branch`,
  `decompose_phase_branch`, `decompose_into`
- Replace all bare `info!`, `warn!`, `debug!` calls with prefixed versions:
  `info!("[decomposer:{}] ...", goal_id, ...)`
- Thread `goal_id` through from the `doc.rs` call site

`src/daemon/handlers/doc.rs`:
- Pass `goal_id_bg.as_str()` into `decompose_hierarchy`
- Replace the now-deleted `AgentLogger::for_component` block with a single
  `info!("[decomposer:{}] starting (brief={})", goal_id_bg, brief)` call

Other library functions (`evaluator`, `validator`, `ratifier`):
- Apply same pattern: add relevant ID parameter, prefix all macros

**Phase 4 - Infrastructure prefixes**

- `src/agents/worker.rs`: change `info!("Worker {} ...", id)` to `info!("[worker:{}] ...", id)`
- `src/daemon/`: prefix startup/shutdown messages with `[daemon]`
- `src/ipc/server.rs`: prefix connection messages with `[ipc]`
- `src/worktree/`: prefix with `[worktree:{branch}]`

**Phase 5 - Remove stale test code and verify**

- Delete tests in `agent_logger.rs` (file is gone)
- Update any tests that constructed `AgentLogger` instances
- Run `otto ci` - must pass clean

---

## Alternatives Considered

### Alternative 1: Refactor AgentLogger into a lightweight prefix struct

- **Description:** Strip file I/O from `AgentLogger`, keep the struct as a prefix formatter
- **Pros:** Smaller diff, fewer call sites to touch
- **Cons:** Still a struct to thread through functions; the name "AgentLogger" is wrong for
  non-agents; nothing is actually simpler - you still have to pass it everywhere
- **Why not chosen:** Doesn't solve the problem. Adds complexity for no gain over plain macros.

### Alternative 2: tokio task-local for implicit ID propagation

- **Description:** Store the component prefix in a `tokio::task_local!`, read implicitly
- **Pros:** No parameter threading
- **Cons:** Implicit, fragile across spawn boundaries, hard to reason about
- **Why not chosen:** Explicit is better. The ID is a `&str` already in scope.

---

## Technical Considerations

### Dependencies

- No new crates
- `src/agents/agent_logger.rs` deleted
- All files that import `crate::agents::agent_logger` updated

### Performance

Strictly better. No file open, no `Mutex`, no `BufWriter`, no heap allocation per log call
beyond what `log::info!` already does.

### Testing Strategy

- `otto ci` must pass after each phase
- Verify `loopr.log` output from a fresh E2E run contains `[decomposer:cg-xxxxx]` milestone
  lines (not just start/end)
- Verify `grep '[coordinator:' loopr.log` returns the full coordinator run, not a subset

### Rollout Plan

All five phases ship in one PR. No behavior changes - purely observability. Version bump after
`otto ci` passes.

---

### `write_iter_file` - separate concern

`AgentLogger.write_iter_file` writes per-iteration LLM conversation markdown files to disk
(`{agent_id}.iter-N.md`). This is not logging - it is a debugging artifact. It is not part of
`loopr.log` and is unaffected by this change's core goal.

Decision: delete `write_iter_file` along with `AgentLogger`. The same content is visible in the
LLM response already. If per-iteration conversation capture is needed in the future it can be
reimplemented as a standalone utility with an explicit opt-in flag.

### Functions that accept `&AgentLogger` as a parameter

~15 helper functions across `coordinator.rs`, `implementer.rs`, `reviewer.rs`, `researcher.rs`,
`executor/` accept `&AgentLogger` solely to call `log.info(...)` / `log.warn(...)` inside them.

After deletion, these functions change their signature to accept a prefix string:

```rust
// before
fn parse_actions(response: &str, agent_log: &AgentLogger) -> Result<Vec<AgentAction>>

// after
fn parse_actions(response: &str, prefix: &str) -> Result<Vec<AgentAction>>
// internally: warn!("{} parse_actions: ...", prefix, ...)
```

The `prefix` at each call site is `&format!("[{}:{}]", agent_type, session_id)`, constructed
once at the top of the agent's run loop and passed down. No struct, no heap per call.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Miss a call site | Med | Low | After Phase 2: `grep -r 'AgentLogger\|agent_log\.\|self\.ctx\.log\b'` must return zero non-test hits |
| `goal_id` threading changes decomposer signatures used in tests | Med | Low | Compiler catches every mismatch; test call sites updated in the same phase |
| `write_iter_file` removal loses debugging output | Low | Low | Content already visible in LLM responses; no known workflows depend on iter files |

---

## Open Questions

- [ ] Should `[worker:N]` stay as integer index or become `[worker:ag-xxxxx]` once workers
      acquire a session ID per work item?
- [ ] Should `[daemon]` include a startup timestamp or PID for multi-daemon disambiguation?

---

## References

- `src/agents/agent_logger.rs` - the file being deleted
- `src/agents.rs` - `AgentContext` containing `pub log: AgentLogger`
- `src/daemon/handlers/doc.rs` - decomposer background task, only external `for_component` call
- `src/decomposer.rs` - library function, currently has no prefix on any log line
