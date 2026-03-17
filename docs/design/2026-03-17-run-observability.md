# Design Document: Run Observability

**Author:** Scott Idler + Claude
**Date:** 2026-03-17
**Status:** Implemented
**Review Passes Completed:** 3/3

## Summary

Add the minimum observability needed to debug a first real end-to-end orchestration run. The system has good event-level telemetry (transitions, tool calls, agent status) but is blind on agent decision-making: LLM prompts are not logged, full responses are not persisted, and action reasoning is invisible after a session ends. Four targeted changes fix this.

## Problem Statement

### Background

Loopr has per-agent log files (`AgentLogger`), session summaries, `loopr diagnose` CLI, and DaemonEvent streaming. Log levels are configurable via CLI > env (`LOG_LEVEL`) > config YAML > default (Info).

Session output is colocated by session ID:
```
~/.local/share/loopr/sessions/{session_id}/
  loopr.log                              # daemon log for this session
  agents/
    coordinator-co-abc12.log             # per-agent event trace
    implementer-im-xyz99.log
```

The `latest` symlink points to the current session. `AgentSession` records in TaskStore tie agent session IDs back to `work_id` / `bundle_id`.

### Problem

When something goes wrong during orchestration, we can see *what* happened (state transitions, tool exit codes, agent status changes) but not *why*:

1. **System prompts not logged** - the Coordinator logs `raw LLM response (N chars)` but never logs the system prompt or assembled user message that produced it. If the Coordinator generates bad Specs, we can't see what context it had.

2. **Full LLM responses not persisted** - `raw LLM response` is logged at INFO but only a snippet. After the session, the full response is gone from memory. For debugging parse failures or bad action selection, we need the complete text.

3. **No post-session replay** - `loopr diagnose` can show agent logs and summaries, but there's no way to see the full prompt/response exchange for a specific iteration. The most valuable debugging artifact (the LLM conversation) is ephemeral.

4. **Important functions lack entry logging** - handler functions, agent lifecycle methods, and orchestration entry points don't log their parameters on entry. When tracing a request through the system, you have to guess what arguments were passed.

### Goals

- G1: Persist full LLM prompt/response exchanges to per-iteration files, separate from the event log
- G2: Instrument key functions with DEBUG-level entry logging showing parameter values
- G3: All changes gated behind DEBUG log level - zero overhead at INFO
- G4: `otto ci` passes with no regressions

### Non-Goals

- Structured tracing (OpenTelemetry, spans) - overkill for now
- LLM token/cost tracking - separate concern
- TUI-visible debug output - logs are sufficient
- New CLI commands or diagnostics UI

## Proposed Solution

### Overview

Two kinds of output, serving different audiences:

1. **Per-iteration conversation files** (`.iter-N.md`) - full LLM prompt + response for replay and debugging. Written alongside the agent log. Self-describing with metadata header.

2. **Function entry debug logging** - DEBUG-level `log::debug!` calls at key entry points showing parameter values. Uses existing `AgentLogger` and `log` crate infrastructure.

The agent event log (`.log`) stays clean and grep-friendly. The conversation files (`.iter-N.md`) are verbose and read sequentially.

### Architecture

**Change 1: Per-iteration conversation files**

After each LLM call in an agent's iteration loop, write a markdown file with the full exchange:

```
~/.local/share/loopr/sessions/{session_id}/agents/
  coordinator-co-abc12.log           # event trace (existing, unchanged)
  coordinator-co-abc12.iter-0.md     # iteration 0 conversation
  coordinator-co-abc12.iter-1.md     # iteration 1 conversation
  implementer-im-xyz99.log
  implementer-im-xyz99.iter-0.md
```

Each `.iter-N.md` file:

```markdown
# Agent: coordinator-co-abc12
# Type: Coordinator
# Work: (none)
# Iteration: 0
# Timestamp: 2026-03-17T14:23:01Z

## System Prompt
{full system prompt text}

## User Message
{full assembled user message / message history}

## Response
{full LLM response, no truncation}
```

The metadata header makes files self-describing - you can understand the context without cross-referencing other logs.

**Implementation:** Add a `write_iter_file` method to `AgentLogger`:

```rust
impl AgentLogger {
    /// Write a per-iteration conversation file alongside the agent log.
    /// Only writes when log level is DEBUG or lower.
    pub fn write_iter_file(
        &self,
        iteration: u32,
        agent_type: &str,
        work_id: Option<&str>,
        system_prompt: &str,
        user_message: &str,
        response: &str,
    ) {
        if !log::log_enabled!(log::Level::Debug) {
            return; // zero cost at INFO
        }
        // Write to {agent_log_dir}/{agent_id}.iter-{N}.md
    }
}
```

Insertion points (call `write_iter_file` after each LLM response):
- `src/agents/coordinator.rs` - after `llm.call_with_history` returns
- `src/agents/implementer.rs` - after `llm.call_with_history` returns
- `src/agents/reviewer.rs` - after its LLM call
- `src/agents/researcher.rs` - after its LLM call

The agent event log gets a one-line pointer: `debug!("LLM exchange -> iter-{n}.md ({} chars prompt, {} chars response)")`

**Change 2: Function entry logging with parameters**

Instrument key orchestration functions with `log::debug!` on entry showing parameter values. Target the functions most relevant to debugging a live run:

**Daemon handlers** (`src/daemon/handlers.rs`):
- `handle_coordinator_accept_plan` - log plan text length, plan_id
- `handle_agent_start` - log agent_type, work_id, bundle_id
- `handle_agent_stop` - log session_id
- `handle_chat_submit` - log session_id, funnel_state, message length

**Agent lifecycle** (`src/agents/executor.rs`):
- `run_agent_task` - log session_id, agent_type, work_id
- `run_agent_loop` - log agent_type, iteration count

**Coordinator decisions** (`src/agents/coordinator.rs`):
- `run_iteration` - log FSM state, goal_id, iteration number
- `check_fsm_transition` - log current state, computed next state
- `sweep_integrated_to_done` - log count of works swept
- `execute_action` calls - log action type and target

**Implementer flow** (`src/agents/implementer.rs`):
- `run_iteration` - log work_id, iteration number, has_previous_summary
- Action execution - log action type, tool name

**Integrator** (`src/agents/integrator.rs`):
- `run_iteration` - log tick_id, bundle count
- `merge_bundle_branches` - log branch names

Pattern: `log::debug!("[{agent_type}:{session_id}] fn_name(param=value, ...)")` matching existing log style.

### Data Model

No new domain types. One new method on `AgentLogger`.

### Implementation Plan

**Phase 1: Conversation file infrastructure + LLM logging**

Files modified:
- `src/agents/agent_logger.rs` - add `write_iter_file` method
- `src/agents/coordinator.rs` - call write_iter_file after LLM call in `run_iteration`
- `src/agents/implementer.rs` - call write_iter_file after LLM call in `run_iteration`
- `src/agents/reviewer.rs` - call write_iter_file after LLM call
- `src/agents/researcher.rs` - call write_iter_file after LLM call

**Phase 2: Function entry instrumentation**

Files modified:
- `src/daemon/handlers.rs` - entry logging on 4 key handlers
- `src/agents/executor.rs` - entry logging on run_agent_task, run_agent_loop
- `src/agents/coordinator.rs` - entry logging on FSM methods
- `src/agents/implementer.rs` - entry logging on iteration and action methods
- `src/agents/integrator.rs` - entry logging on merge methods

Tests:
- Existing tests pass (log calls and file writes are side-effect-free at non-DEBUG levels)
- No new tests needed (logging is infrastructure, not logic)

## Alternatives Considered

### Alternative 1: Persist LLM exchanges to TaskStore

- **Description:** Create a new `LlmExchange` record type with prompt + response + metadata, persisted per-iteration.
- **Pros:** Queryable, structured, survives log rotation.
- **Cons:** New domain type, schema, storage overhead. Overkill for debugging first runs.
- **Why not chosen:** Files are sufficient for now. Can upgrade later if needed.

### Alternative 2: Inline everything in the agent log

- **Description:** Write full prompts and responses as DEBUG log lines in the existing per-agent `.log` file.
- **Pros:** No new files. Single log to read.
- **Cons:** Mixes two audiences. A 50KB LLM response in a log full of one-line status entries makes both harder to use. Can't grep the event trace without drowning in prompt text. Can't read the conversation without picking through status noise.
- **Why not chosen:** Separate files serve separate purposes. Event trace stays scannable. Conversations are readable end-to-end.

## Technical Considerations

### Performance

- Zero overhead at INFO level (`log_enabled!` check short-circuits before any formatting or I/O)
- At DEBUG level: one `fs::write` per agent iteration (~50-200KB per file, buffered)
- Expected overhead: <5ms per iteration (one synchronous write)

### Testing Strategy

- Run `otto ci` - all existing tests pass
- Manual: run with `LOG_LEVEL=debug`, verify `.iter-N.md` files appear in session agents directory with correct content

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Disk usage at DEBUG | Medium | Low | Only active when user opts in. Session-scoped, easy to clean up. |
| Sensitive data in prompts | Low | Low | Prompts contain codebase context, not secrets. Same risk as existing tool result logging. |
| File I/O errors | Low | Low | `write_iter_file` logs warning on error, never panics or fails the agent. |

## Open Questions

- [ ] Should `loopr diagnose` learn to list/cat iter files? (Could be a fast follow.)

## References

- `src/agents/agent_logger.rs` - per-agent logging infrastructure
- `src/lib.rs` - `resolve_log_level` hierarchy (CLI > env > config > default)
- `src/cli/diagnose.rs` - existing diagnostics CLI
