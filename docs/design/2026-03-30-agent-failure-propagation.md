# Design Document: Agent Failure Propagation and IPC Client Fail-Fast

**Author:** Scott Idler
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

When the Coordinator agent fails (e.g., missing API key, network error), the daemon emits
`agent.status_changed(Failed)` but IPC clients waiting for `record.created(plan)` have no way
to learn the agent has died - they time out after 120 seconds. The fix has two parts: enrich the
`agent.status_changed` event to carry the error message when status is `Failed`, and update
`run_persona` (and the broader IPC client contract) to treat terminal agent status as a failure
signal rather than a noise event to ignore.

## Problem Statement

### Background

The Coordinator interview flow requires IPC clients to:
1. Send `coordinator.set_goal`
2. Send `agent.start`
3. Respond to `coordinator.interview_question` events
4. Wait for `record.created(collection: plan)`

The `run_persona` driver in `tests/funnel.rs` implements this flow. It ignores all events
except `coordinator.interview_question` and `record.created(plan)`.

### Problem

When the Coordinator agent task fails (for any reason) before producing a plan, the daemon
emits `agent.status_changed` with `status: "failed"`. The executor stores the error message
in `session.error_message` but does not include it in the event payload. The `run_persona`
loop discards all `agent.status_changed` events. The loop then waits the full timeout
(default 120 seconds) for a `record.created(plan)` event that will never arrive.

The error reported to the developer is:

```
persona 'Golden Path': timed out after 120s waiting for record.created (collection: plan)
```

There is no indication of what actually went wrong. The root cause (e.g., `API key not found`)
is only findable in daemon logs.

This is not limited to tests. Any automation client implementing the same pattern (set goal,
start agent, wait for plan) has the same latent timeout bug.

### Goals

- IPC clients waiting for `record.created(plan)` fail fast when the Coordinator agent reaches
  a terminal failure state (`Failed` or `Cancelled`).
- The error message from the executor is surfaced in the `agent.status_changed` event, so
  clients do not need a separate `session.get` call to understand why the agent failed.
- The `run_persona` driver uses this mechanism rather than an env-var pre-flight check.

### Non-Goals

- Restarting the Coordinator on failure - that is handled by the supervisor.
- Changing the supervisor restart logic.
- Improving error handling for non-Coordinator agents.
- Retrying failed persona runs.

## Proposed Solution

### Overview

Add an optional `error` field to `AgentEvent::StatusChange`. Populate it in the executor when
transitioning to `Failed`. Update `run_persona` to treat `agent.status_changed` with a terminal
failure status as a hard error and exit the event loop immediately with a descriptive message.

### Architecture

Two layers change:

**Protocol layer** (`src/agents/mod.rs`, `src/ipc/protocol.rs`):
- `AgentEvent::StatusChange` gains `error: Option<String>`
- A new `DaemonEvent::agent_status_failed(session_id, error: Option<String>)` constructor is
  added alongside the existing `agent_status_changed`. This avoids touching the 18 existing
  `agent_status_changed` call sites.
- Serialized event payload on failure:
  ```json
  { "type": "status_change", "session_id": "...", "status": "failed", "error": "API key not found" }
  ```

**Executor layer** (`src/agents/executor.rs`):
- The two `Failed` emission points use `agent_status_failed(session_id, error)`:
  - Line 179: worktree creation failure (passes worktree error)
  - Line 339: terminal failure after `run_agent_loop` (passes `session.error_message.clone()`)
- The `terminal_status` at line 339 is only `Failed` when `result.is_err()`, so the error
  argument is only populated for `Failed`; for `Completed`/`Cancelled` the existing
  `agent_status_changed` is used unchanged.

**Test driver** (`tests/funnel.rs`):
- `run_persona` matches on `agent.status_changed` in the event loop
- If `status` is `"failed"` or `"cancelled"`, return `Err` immediately with the error text from
  the event data (or `"agent failed (no error message)"` as fallback)
- Remove the env-var pre-flight check from `run_persona` - the fail-fast event handling covers
  all failure causes, not just a missing API key

### Data Model

`AgentEvent::StatusChange` before:
```rust
StatusChange {
    session_id: String,
    status: AgentStatus,
}
```

`AgentEvent::StatusChange` after:
```rust
StatusChange {
    session_id: String,
    status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
```

No schema migrations needed - `error` is optional and skipped when absent.

### API Design

New constructor added to `DaemonEvent` (existing `agent_status_changed` unchanged):

```rust
pub fn agent_status_failed(session_id: &str, error: Option<String>) -> Self {
    let event = AgentEvent::StatusChange {
        session_id: session_id.to_string(),
        status: AgentStatus::Failed,
        error,
    };
    Self::new("agent.status_changed", serde_json::to_value(event).unwrap_or_default())
}
```

The event name stays `"agent.status_changed"` - the constructor is just a typed convenience.
This approach leaves the 18 existing `agent_status_changed` call sites untouched.

In `run_persona`, the event loop checks status from the raw JSON:
```rust
"agent.status_changed" => {
    let status = event.data["status"].as_str().unwrap_or("");
    if status == "failed" || status == "cancelled" {
        let error = event.data["error"].as_str().unwrap_or("agent failed (no error message)");
        return Err(eyre!("persona '{}': agent reached terminal state '{status}': {error}", fixture.name));
    }
}
```

### Implementation Plan

**Phase 1: Protocol**
- Add `error: Option<String>` (with `#[serde(skip_serializing_if = "Option::is_none")]`) to
  `AgentEvent::StatusChange`
- Add `DaemonEvent::agent_status_failed(session_id, error)` constructor
- Existing `agent_status_changed` is unchanged

**Phase 2: Executor**
- Replace the two `agent_status_changed(session_id, AgentStatus::Failed)` calls in
  `executor.rs` (lines ~179 and ~339) with `agent_status_failed(session_id, error)`
- At line 179 (worktree failure), construct the error from the worktree creation error
- At line 339 (loop failure), pass `session.error_message.clone()`

**Phase 3: Test driver**
- Add `"agent.status_changed"` match arm to `run_persona` event loop
- Remove the env-var pre-flight check from `run_persona`

## Alternatives Considered

### Alternative 1: Keep env-var check as the fix

- **Description:** The already-landed check in `run_persona` catches the API key case.
- **Pros:** Zero protocol change, already done.
- **Cons:** Only catches one specific failure mode. All other agent failures (network error,
  LLM error, bad config) still produce a 120-second timeout with a cryptic message. The check
  also fires after daemon spawn, so it does not eliminate all setup cost.
- **Why not chosen:** Treats a symptom of one root cause rather than the class of problems.

### Alternative 2: Separate `agent.failed` event type

- **Description:** Add a distinct `agent.failed` event instead of extending `agent.status_changed`.
- **Pros:** Cleaner semantics for clients that only care about failures.
- **Cons:** Two events for the same state transition. Clients must subscribe to both to get a
  complete picture. Increases protocol surface.
- **Why not chosen:** `agent.status_changed` is the single source of truth for agent lifecycle.
  Extend it rather than duplicate it.

### Alternative 3: Client queries session on timeout

- **Description:** After timeout, `run_persona` queries `session.get` to find the error.
- **Pros:** No protocol changes.
- **Cons:** Still waits the full timeout. Error message still buried in a separate call.
- **Why not chosen:** Does not provide fail-fast behavior.

## Technical Considerations

### Dependencies

- `AgentEvent`, `AgentStatus` in `src/agents/mod.rs`
- `DaemonEvent` in `src/ipc/protocol.rs`
- `executor.rs` (caller of `agent_status_changed`)
- All other callers of `agent_status_changed` (supervisor, handlers) - compiler will flag them

### Performance

No performance impact. `error` is `Option<String>` and skipped in serialization when `None`.

### Security

Error messages from `session.error_message` are already stored in TaskStore and visible via
`session.get`. Surfacing them in the broadcast event does not expose anything not already
accessible to connected clients.

### Testing Strategy

- Update `test_agent_event_status_change_serde` to cover the `error` field (both `Some` and `None`)
- Add a unit test for the `run_persona` event loop logic: construct a fake
  `agent.status_changed(Failed, "test error")` event and verify `run_persona` returns an error
  with the message (requires extracting the loop logic or using a mock client)
- Existing non-ignored funnel tests continue to pass (structural tests do not touch the event loop)

### Rollout Plan

Protocol change is fully backwards-compatible: `error` is `skip_serializing_if = None`, so
old clients that deserialize `AgentEvent::StatusChange` will simply ignore the new field.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `session.error_message` is empty at failure time | Low | Low | Fallback to `"agent failed (no error message)"` in client |
| A different agent's `Failed` event prematurely terminates the persona test | Low | Medium | Scope the status check to the coordinator session_id returned from `agent.start` |
| Supervisor restart produces a new `Failed` event before the new coordinator succeeds | Low | Medium | Same mitigation - scope to the original session_id; supervisor restarts use new session_ids |
| `terminal_status == Completed` at line 339 accidentally uses `agent_status_failed` | None | High | `agent_status_failed` is a separate constructor; only called explicitly for `Failed` paths |

## Open Questions

- [x] Should `Cancelled` also be treated as a test failure in `run_persona`, or only `Failed`?
  Yes - cancellation during a test run is unexpected and should fail loudly.
- [x] Should `run_persona` filter `agent.status_changed` events by the session_id returned from
  `agent.start`? Yes - scoped to the coordinator's session_id to avoid false positives from
  unrelated agent events (e.g., supervisor restarts).

## References

- `src/ipc/protocol.rs` - `DaemonEvent::agent_status_changed`
- `src/agents/mod.rs` - `AgentEvent::StatusChange`, `AgentStatus`
- `src/agents/executor.rs` - failure handling and event emission
- `src/daemon/supervisor.rs` - coordinator restart on failure
- `tests/funnel.rs` - `run_persona` event loop
- `docs/design/2026-03-29-conversational-funnel-testing.md` - persona test design
